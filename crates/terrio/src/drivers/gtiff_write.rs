//! A GeoTIFF and Cloud Optimized GeoTIFF writer.
//!
//! wbraster's writer lays COGs out in the wrong order and skips overview levels, so terrio writes
//! TIFFs itself: strips or tiles, pixel-interleaved, with the compressions GDAL reads by default,
//! GeoKeys for EPSG and user-defined CRSs, `GDAL_NODATA` and `GDAL_METADATA`. COGs follow GDAL's
//! layout: the IFDs first, then the image data from the smallest overview to the full resolution,
//! with GDAL's structural metadata, block leaders and trailers.

use crate::pixel::{Pixel, map_data, with_data};
use crate::{Error, Result};
use std::io::Write;
use wbraster::raster::RasterData;
use wbraster::{DataType, Raster};

/// TIFF compressions the writer supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compression {
    None,
    Lzw,
    Deflate,
    PackBits,
    Zstd,
}

impl Compression {
    /// Parses a GDAL compression name. `None` for names this writer does not handle.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_uppercase().as_str() {
            "NONE" => Self::None,
            "LZW" => Self::Lzw,
            "DEFLATE" | "ZIP" => Self::Deflate,
            "PACKBITS" => Self::PackBits,
            "ZSTD" => Self::Zstd,
            _ => return None,
        })
    }

    fn code(self) -> u16 {
        match self {
            Self::None => 1,
            Self::Lzw => 5,
            Self::Deflate => 8,
            Self::PackBits => 32773,
            Self::Zstd => 50000,
        }
    }

    fn compress(self, data: Vec<u8>) -> Result<Vec<u8>> {
        Ok(match self {
            Self::None => data,
            Self::Lzw => weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
                .encode(&data)
                .map_err(|e| Error::InvalidData(format!("LZW: {e}")))?,
            Self::Deflate => {
                let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
                enc.write_all(&data)?;
                enc.finish()?
            }
            Self::PackBits => packbits(&data),
            Self::Zstd => zstd_compress(&data)?,
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn zstd_compress(data: &[u8]) -> Result<Vec<u8>> {
    Ok(zstd::bulk::compress(data, 9)?)
}

#[cfg(target_arch = "wasm32")]
fn zstd_compress(_data: &[u8]) -> Result<Vec<u8>> {
    Err(Error::Unsupported("ZSTD compression is not available in WebAssembly builds".into()))
}

/// PackBits run-length encoding, as TIFF defines it.
fn packbits(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 128 + 1);
    let mut i = 0;
    while i < data.len() {
        // A run of at least 3 equal bytes is worth encoding as a run.
        let mut run = 1;
        while i + run < data.len() && run < 128 && data[i + run] == data[i] {
            run += 1;
        }
        if run >= 3 {
            out.push((1 - run as i32) as u8);
            out.push(data[i]);
            i += run;
            continue;
        }
        let start = i;
        while i < data.len() && i - start < 128 {
            if i + 2 < data.len() && data[i] == data[i + 1] && data[i] == data[i + 2] {
                break;
            }
            i += 1;
        }
        out.push((i - start - 1) as u8);
        out.extend_from_slice(&data[start..i]);
    }
    out
}

/// How to write a GeoTIFF.
#[derive(Debug, Clone)]
pub(crate) struct TiffOptions {
    /// The compression.
    pub compression: Compression,
    /// Tile size; strips when `None`.
    pub tile_size: Option<u32>,
    /// Force BigTIFF; otherwise used only when the file would exceed 4 GiB.
    pub bigtiff: bool,
    /// Write a Cloud Optimized GeoTIFF: tiled, with overviews and GDAL's layout.
    pub cog: bool,
    /// The nodata value to declare.
    pub nodata: Option<f64>,
    /// Overview resampling: nearest neighbour instead of averaging.
    pub nearest_overviews: bool,
}

impl Default for TiffOptions {
    fn default() -> Self {
        Self { compression: Compression::None, tile_size: None, bigtiff: false, cog: false, nodata: None, nearest_overviews: false }
    }
}

// TIFF field types.
const SHORT: u16 = 3;
const LONG: u16 = 4;
const ASCII: u16 = 2;
const DOUBLE: u16 = 12;
const LONG8: u16 = 16;

/// A tag's values.
#[derive(Clone)]
enum Value {
    Shorts(Vec<u16>),
    /// Offsets or counts: LONG in classic TIFF, LONG8 in BigTIFF.
    Offsets(Vec<u64>),
    Longs(Vec<u32>),
    Doubles(Vec<f64>),
    Ascii(String),
}

impl Value {
    fn encode(&self, big: bool) -> (u16, u64, Vec<u8>) {
        match self {
            Value::Shorts(v) => (SHORT, v.len() as u64, v.iter().flat_map(|x| x.to_le_bytes()).collect()),
            Value::Longs(v) => (LONG, v.len() as u64, v.iter().flat_map(|x| x.to_le_bytes()).collect()),
            Value::Offsets(v) if big => (LONG8, v.len() as u64, v.iter().flat_map(|x| x.to_le_bytes()).collect()),
            Value::Offsets(v) => (LONG, v.len() as u64, v.iter().flat_map(|x| (*x as u32).to_le_bytes()).collect()),
            Value::Doubles(v) => (DOUBLE, v.len() as u64, v.iter().flat_map(|x| x.to_le_bytes()).collect()),
            Value::Ascii(s) => {
                let mut b = s.as_bytes().to_vec();
                b.push(0);
                (ASCII, b.len() as u64, b)
            }
        }
    }
}

/// One image (full resolution or overview) ready to be written: its tags without the chunk
/// offsets, and its compressed chunks.
struct Image {
    tags: Vec<(u16, Value)>,
    chunks: Vec<Vec<u8>>,
    tiled: bool,
}

/// Writes a raster as a GeoTIFF or COG.
pub(crate) fn write(raster: &Raster, path: &str, opts: &TiffOptions) -> Result<()> {
    let images = prepare(raster, opts)?;
    let mut file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    images.write_to(&mut file)?;
    file.flush()?;
    Ok(())
}

/// Encodes a raster as a GeoTIFF or COG in memory.
pub(crate) fn encode(raster: &Raster, opts: &TiffOptions) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    prepare(raster, opts)?.write_to(&mut out)?;
    Ok(out)
}

/// The images of a TIFF file, compressed, ready to be laid out.
struct Prepared {
    images: Vec<Image>,
    big: bool,
    cog: bool,
}

impl Prepared {
    fn write_to(self, out: &mut impl Write) -> std::io::Result<()> {
        assemble(self.images, self.big, self.cog, out)
    }
}

/// Splits a raster (and its overviews, for a COG) into compressed chunks with their tags.
fn prepare(raster: &Raster, opts: &TiffOptions) -> Result<Prepared> {
    if raster.cols == 0 || raster.rows == 0 || raster.bands == 0 {
        return Err(Error::InvalidArgument("cannot write an empty raster".into()));
    }
    let tile = if opts.cog { Some(opts.tile_size.unwrap_or(512)) } else { opts.tile_size };
    if let Some(t) = tile
        && (t == 0 || t % 16 != 0)
    {
        return Err(Error::InvalidArgument(format!("the tile size must be a multiple of 16, not {t}")));
    }
    let geo = geo_tags(raster, opts.nodata)?;

    // The full-resolution image, then overviews (COG) until one fits in a tile.
    let mut levels = vec![Level::from_raster(raster)];
    if let (true, Some(t)) = (opts.cog, tile) {
        let t = t as usize;
        while levels.last().is_some_and(|l| l.cols > t || l.rows > t) {
            let next = levels[levels.len() - 1].downsample(opts.nodata, opts.nearest_overviews);
            levels.push(next);
        }
    }
    let mut images = Vec::with_capacity(levels.len());
    for (i, level) in levels.iter().enumerate() {
        let mut img = level.image(raster.data_type, opts.compression, tile)?;
        if i > 0 {
            img.tags.push((254, Value::Longs(vec![1])));
        }
        // Georeferencing on the full-resolution image; nodata on every image, as GDAL does.
        for (tag, v) in &geo {
            if i == 0 || *tag == 42113 {
                img.tags.push((*tag, v.clone()));
            }
        }
        img.tags.sort_by_key(|(t, _)| *t);
        images.push(img);
    }

    let data_len: u64 = images.iter().flat_map(|i| i.chunks.iter()).map(|c| c.len() as u64 + 8).sum();
    let big = opts.bigtiff || data_len > (u32::MAX as u64) - (1 << 24);
    Ok(Prepared { images, big, cog: opts.cog })
}

/// One resolution level's pixels, band-major: the raster's own, or an overview's.
struct Level<'a> {
    cols: usize,
    rows: usize,
    bands: usize,
    data: std::borrow::Cow<'a, RasterData>,
}

impl<'a> Level<'a> {
    fn from_raster(r: &'a Raster) -> Self {
        Self { cols: r.cols, rows: r.rows, bands: r.bands, data: std::borrow::Cow::Borrowed(&r.data) }
    }

    /// Halves the resolution: the average (or the top-left value) of each 2x2 block of valid
    /// pixels.
    fn downsample(&self, nodata: Option<f64>, nearest: bool) -> Level<'static> {
        let (cols, rows) = (self.cols.div_ceil(2), self.rows.div_ceil(2));
        let data = map_data!(self.data.as_ref(), v => downsample(v, self.cols, self.rows, cols, rows, self.bands, nodata, nearest));
        Level { cols, rows, bands: self.bands, data: std::borrow::Cow::Owned(data) }
    }

    /// Splits the level into compressed strips or tiles, with its structural tags.
    fn image(&self, t: DataType, compression: Compression, tile: Option<u32>) -> Result<Image> {
        let size = t.size_bytes();
        let spp = self.bands;
        let (chunk_w, chunk_h, across, down, tiled) = match tile {
            Some(ts) => {
                let ts = ts as usize;
                (ts, ts, self.cols.div_ceil(ts), self.rows.div_ceil(ts), true)
            }
            None => {
                let row_bytes = self.cols * spp * size;
                let rows_per_strip = ((1usize << 16) / row_bytes.max(1)).clamp(1, self.rows);
                (self.cols, rows_per_strip, 1, self.rows.div_ceil(rows_per_strip), false)
            }
        };
        let chunks = crate::par::map_range(across * down, |i| {
            let (x0, y0) = ((i % across) * chunk_w, (i / across) * chunk_h);
            // Strips stop at the last row; tiles are padded to full size.
            let h = if tiled { chunk_h } else { chunk_h.min(self.rows - y0) };
            let buf = with_data!(self.data.as_ref(), v => chunk_bytes(v, self.cols, self.rows, spp, x0, y0, chunk_w, h));
            compression.compress(buf)
        });
        let chunks = chunks.into_iter().collect::<Result<Vec<_>>>()?;
        let (bits, format) = sample_format(t);
        let rgb = spp == 3 && t == DataType::U8;
        let mut tags = vec![
            (256, Value::Longs(vec![self.cols as u32])),
            (257, Value::Longs(vec![self.rows as u32])),
            (258, Value::Shorts(vec![bits; spp])),
            (259, Value::Shorts(vec![compression.code()])),
            (262, Value::Shorts(vec![if rgb { 2 } else { 1 }])),
            (277, Value::Shorts(vec![spp as u16])),
            (284, Value::Shorts(vec![1])),
            (339, Value::Shorts(vec![format; spp])),
        ];
        let extra = if rgb { 0 } else { spp - 1 };
        if extra > 0 {
            tags.push((338, Value::Shorts(vec![0; extra])));
        }
        if tiled {
            tags.push((322, Value::Longs(vec![chunk_w as u32])));
            tags.push((323, Value::Longs(vec![chunk_h as u32])));
        } else {
            tags.push((278, Value::Longs(vec![chunk_h as u32])));
        }
        Ok(Image { tags, chunks, tiled })
    }
}

/// The pixel-interleaved little-endian bytes of a chunk of `chunk_w` x `h` pixels at `(x0, y0)`,
/// padded with zeros past the image's edges.
#[allow(clippy::too_many_arguments)]
fn chunk_bytes<T: Pixel>(data: &[T], cols: usize, rows: usize, spp: usize, x0: usize, y0: usize, chunk_w: usize, h: usize) -> Vec<u8> {
    let size = T::SIZE;
    let row_len = chunk_w * spp * size;
    let mut buf = vec![0u8; row_len * h];
    let band_len = cols * rows;
    let n = chunk_w.min(cols - x0);
    for row in 0..h.min(rows.saturating_sub(y0)) {
        let dst = &mut buf[row * row_len..][..n * spp * size];
        let src = (y0 + row) * cols + x0;
        if spp == 1 {
            for (d, v) in dst.chunks_exact_mut(size).zip(&data[src..src + n]) {
                v.put_le(d);
            }
        } else {
            for b in 0..spp {
                let band = &data[b * band_len + src..][..n];
                for (c, v) in band.iter().enumerate() {
                    v.put_le(&mut dst[(c * spp + b) * size..][..size]);
                }
            }
        }
    }
    buf
}

/// Halves a band-major image: each output pixel is the average of the valid pixels of its 2x2
/// block (or the block's top-left pixel), computed in `f64` and converted as the writer converts.
#[allow(clippy::too_many_arguments)]
fn downsample<T: Pixel>(
    src: &[T],
    cols: usize,
    rows: usize,
    out_cols: usize,
    out_rows: usize,
    bands: usize,
    nodata: Option<f64>,
    nearest: bool,
) -> Vec<T> {
    let fill = nodata.unwrap_or(0.0);
    let mut out = vec![T::default(); out_cols * out_rows * bands];
    crate::par::chunks_mut(&mut out, out_cols, |i, line| {
        let (b, r) = (i / out_rows, i % out_rows);
        let band = &src[b * cols * rows..(b + 1) * cols * rows];
        let get = |r: usize, c: usize| band[r * cols + c].to_f64();
        for (c, o) in line.iter_mut().enumerate() {
            let v = if nearest {
                get(2 * r, 2 * c)
            } else {
                let (mut sum, mut n) = (0.0, 0u32);
                for (rr, cc) in [(2 * r, 2 * c), (2 * r, 2 * c + 1), (2 * r + 1, 2 * c), (2 * r + 1, 2 * c + 1)] {
                    if rr >= rows || cc >= cols {
                        continue;
                    }
                    let v = get(rr, cc);
                    if v.is_nan() || nodata.is_some_and(|nd| v == nd) {
                        continue;
                    }
                    sum += v;
                    n += 1;
                }
                if n == 0 { fill } else { sum / n as f64 }
            };
            *o = T::from_f64(v);
        }
    });
    out
}

fn sample_format(t: DataType) -> (u16, u16) {
    let bits = (t.size_bytes() * 8) as u16;
    let format = match t {
        DataType::U8 | DataType::U16 | DataType::U32 | DataType::U64 => 1,
        DataType::I8 | DataType::I16 | DataType::I32 | DataType::I64 => 2,
        DataType::F32 | DataType::F64 => 3,
    };
    (bits, format)
}

/// Appends a value as a little-endian sample of type `t`, rounding and clamping integers.
pub(crate) fn encode_sample_le(t: DataType, v: f64, out: &mut Vec<u8>) {
    // `as` saturates float-to-int conversions and maps NaN to 0.
    match t {
        DataType::U8 => out.push(v.round() as u8),
        DataType::I8 => out.push(v.round() as i8 as u8),
        DataType::U16 => out.extend((v.round() as u16).to_le_bytes()),
        DataType::I16 => out.extend((v.round() as i16).to_le_bytes()),
        DataType::U32 => out.extend((v.round() as u32).to_le_bytes()),
        DataType::I32 => out.extend((v.round() as i32).to_le_bytes()),
        DataType::U64 => out.extend((v.round() as u64).to_le_bytes()),
        DataType::I64 => out.extend((v.round() as i64).to_le_bytes()),
        DataType::F32 => out.extend((v as f32).to_le_bytes()),
        DataType::F64 => out.extend(v.to_le_bytes()),
    }
}

/// The georeferencing tags: pixel scale and tie point, GeoKeys, nodata and metadata.
fn geo_tags(r: &Raster, nodata: Option<f64>) -> Result<Vec<(u16, Value)>> {
    let mut tags = vec![
        (33550, Value::Doubles(vec![r.cell_size_x, r.cell_size_y, 0.0])),
        (33922, Value::Doubles(vec![0.0, 0.0, 0.0, r.x_min, r.y_max(), 0.0])),
    ];
    let crs = super::common::spatial_ref(&r.crs);
    let mut v = crate::drivers::geokeys::GeoKeyValues::default();
    v.shorts.push((1025, 1)); // RasterPixelIsArea
    let local = crs.wkt.as_deref().and_then(local_cs);
    if let Some((name, unit)) = &local {
        // A local CRS, as GDAL writes one: a citation and the linear unit, no model type.
        v.ascii.push((1026, name.clone()));
        v.shorts.push((3076, *unit));
    } else if !crs.is_unknown() {
        let geographic = crs.is_geographic();
        match crs.epsg.and_then(|c| u16::try_from(c).ok()) {
            Some(code) => {
                v.shorts.push((1024, if geographic { 2 } else { 1 }));
                v.shorts.push((if geographic { 2048 } else { 3072 }, code));
            }
            None => {
                let name = crs.name().unwrap_or_else(|| "unnamed".into());
                let user = crs.to_wbp().ok().and_then(|c| crate::drivers::geokeys::keys_for_crs(&c, &name));
                match user {
                    Some(keys) => v = merge(v, keys),
                    None => {
                        // GDAL's fallback for methods GeoTIFF cannot describe: the WKT in an ESRI PE
                        // citation, with a user-defined model type.
                        let wkt = crs.to_wkt().ok_or_else(|| Error::Crs("the raster's CRS has no WKT definition".into()))?;
                        v.shorts.push((1024, 32767));
                        v.ascii.push((3073, format!("ESRI PE String = {wkt}")));
                    }
                }
            }
        }
    }
    v.shorts.sort_by_key(|k| k.0);
    let mut keys: Vec<[u16; 4]> = v.shorts.iter().map(|(id, val)| [*id, 0, 1, *val]).collect();
    let doubles: Vec<f64> = v.doubles.iter().map(|d| d.1).collect();
    for (i, (id, _)) in v.doubles.iter().enumerate() {
        keys.push([*id, 34736, 1, i as u16]);
    }
    let mut ascii = String::new();
    for (id, text) in &v.ascii {
        let text = format!("{text}|");
        keys.push([*id, 34737, text.len() as u16, ascii.len() as u16]);
        ascii.push_str(&text);
    }
    keys.sort_by_key(|k| k[0]);
    let mut dir = vec![1u16, 1, 0, keys.len() as u16];
    dir.extend(keys.iter().flatten());
    // Without a CRS, no GeoKeys: GDAL reads a lone raster-type key as a local CRS.
    if !crs.is_unknown() {
        tags.push((34735, Value::Shorts(dir)));
    }
    if !doubles.is_empty() {
        tags.push((34736, Value::Doubles(doubles)));
    }
    if !ascii.is_empty() {
        tags.push((34737, Value::Ascii(ascii)));
    }
    if !r.metadata.is_empty() {
        let mut xml = String::from("<GDALMetadata>\n");
        for (k, v) in &r.metadata {
            xml.push_str(&format!("  <Item name=\"{}\">{}</Item>\n", xml_escape(k), xml_escape(v)));
        }
        xml.push_str("</GDALMetadata>");
        tags.push((42112, Value::Ascii(xml)));
    }
    if let Some(nd) = nodata {
        tags.push((42113, Value::Ascii(format_nodata(nd))));
    }
    Ok(tags)
}

/// The name and EPSG linear unit code of a `LOCAL_CS` WKT (metres and feet).
fn local_cs(wkt: &str) -> Option<(String, u16)> {
    let rest = wkt.trim_start().strip_prefix("LOCAL_CS[\"")?;
    let name = rest.split('"').next()?.to_string();
    let lower = rest.to_ascii_lowercase();
    let unit = if lower.contains("unit[\"metre\"") || lower.contains("unit[\"meter\"") {
        9001
    } else if lower.contains("unit[\"foot\"") {
        9002
    } else if lower.contains("us survey foot") {
        9003
    } else {
        return None;
    };
    Some((name, unit))
}

fn merge(mut a: crate::drivers::geokeys::GeoKeyValues, b: crate::drivers::geokeys::GeoKeyValues) -> crate::drivers::geokeys::GeoKeyValues {
    a.shorts.extend(b.shorts);
    a.doubles.extend(b.doubles);
    a.ascii.extend(b.ascii);
    a
}

/// Formats a nodata value as GDAL writes it.
pub(crate) fn format_nodata(nd: f64) -> String {
    if nd.is_nan() {
        "nan".into()
    } else if nd.is_infinite() {
        if nd > 0.0 { "inf".into() } else { "-inf".into() }
    } else {
        // Shortest representation that round-trips.
        format!("{nd}")
    }
}

/// The dataset-level items of a `GDAL_METADATA` XML document (band items are skipped).
pub(crate) fn parse_gdal_metadata(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for item in xml.split("<Item ").skip(1) {
        let Some((attrs, rest)) = item.split_once('>') else { continue };
        if attrs.contains("sample=") || attrs.contains("role=") || attrs.contains("domain=") {
            continue;
        }
        let Some(name) = attrs.split("name=\"").nth(1).and_then(|s| s.split('"').next()) else { continue };
        let Some(value) = rest.split("</Item>").next() else { continue };
        out.push((xml_unescape(name), xml_unescape(value)));
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// GDAL's structural metadata for COGs, written just after the header.
const COG_GHOST: &str = "LAYOUT=IFDS_BEFORE_DATA\nBLOCK_ORDER=ROW_MAJOR\nBLOCK_LEADER=SIZE_AS_UINT4\nBLOCK_TRAILER=LAST_4_BYTES_REPEATED\nKNOWN_INCOMPATIBLE_EDITION=NO\n ";

/// Lays the file out: header, (COG) structural metadata, every IFD, then the image data.
fn assemble(mut images: Vec<Image>, big: bool, cog: bool, w: &mut impl Write) -> std::io::Result<()> {
    let mut out = Vec::new();
    if big {
        out.extend(b"II");
        out.extend(43u16.to_le_bytes());
        out.extend(8u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u64.to_le_bytes()); // first IFD offset, patched below
    } else {
        out.extend(b"II");
        out.extend(42u16.to_le_bytes());
        out.extend(0u32.to_le_bytes());
    }
    if cog {
        out.extend(format!("GDAL_STRUCTURAL_METADATA_SIZE={:06} bytes\n", COG_GHOST.len()).as_bytes());
        out.extend(COG_GHOST.as_bytes());
    }

    // IFD sizes: the entries (with the chunk offset and byte count tags added), then each tag's
    // out-of-line values.
    let (entry_len, count_len, inline_len) = if big { (20u64, 8u64, 8usize) } else { (12, 2, 4) };
    for img in images.iter_mut() {
        let n = img.chunks.len();
        let (off_tag, cnt_tag) = if img.tiled { (324, 325) } else { (273, 279) };
        img.tags.push((off_tag, Value::Offsets(vec![0; n])));
        img.tags.push((cnt_tag, Value::Offsets(img.chunks.iter().map(|c| c.len() as u64).collect())));
        img.tags.sort_by_key(|(t, _)| *t);
    }
    let next_len = if big { 8 } else { 4 };
    let ifd_size = |img: &Image| -> u64 {
        let extra: u64 = img
            .tags
            .iter()
            .map(|(_, v)| {
                let (_, _, b) = v.encode(big);
                if b.len() > inline_len { (b.len() as u64).next_multiple_of(2) } else { 0 }
            })
            .sum();
        count_len + entry_len * img.tags.len() as u64 + next_len + extra
    };

    // IFD offsets, then data offsets. COGs store the smallest overview first.
    out.resize(out.len().next_multiple_of(2), 0);
    let mut pos = out.len() as u64;
    let mut ifd_offsets = Vec::with_capacity(images.len());
    for img in &images {
        ifd_offsets.push(pos);
        pos += ifd_size(img);
    }
    let order: Vec<usize> = if cog { (0..images.len()).rev().collect() } else { (0..images.len()).collect() };
    let mut chunk_offsets: Vec<Vec<u64>> = images.iter().map(|i| vec![0; i.chunks.len()]).collect();
    for &i in &order {
        for (j, c) in images[i].chunks.iter().enumerate() {
            if cog {
                pos += 4; // leader: the chunk size
            }
            chunk_offsets[i][j] = pos;
            pos += c.len() as u64;
            if cog {
                pos += 4; // trailer: the chunk's last 4 bytes
            }
        }
    }

    // Patch the first IFD offset.
    if big {
        out[8..16].copy_from_slice(&ifd_offsets[0].to_le_bytes());
    } else {
        out[4..8].copy_from_slice(&(ifd_offsets[0] as u32).to_le_bytes());
    }

    for (i, img) in images.iter_mut().enumerate() {
        let off_tag = if img.tiled { 324 } else { 273 };
        if let Some((_, v)) = img.tags.iter_mut().find(|(t, _)| *t == off_tag) {
            *v = Value::Offsets(chunk_offsets[i].clone());
        }
        let start = out.len() as u64;
        debug_assert_eq!(start, ifd_offsets[i]);
        let n = img.tags.len() as u64;
        if big {
            out.extend(n.to_le_bytes());
        } else {
            out.extend((n as u16).to_le_bytes());
        }
        let mut extra_pos = start + count_len + entry_len * n + next_len;
        let mut extra = Vec::new();
        for (tag, v) in &img.tags {
            let (typ, count, bytes) = v.encode(big);
            out.extend(tag.to_le_bytes());
            out.extend(typ.to_le_bytes());
            if big {
                out.extend(count.to_le_bytes());
            } else {
                out.extend((count as u32).to_le_bytes());
            }
            if bytes.len() <= inline_len {
                let mut inline = bytes.clone();
                inline.resize(inline_len, 0);
                out.extend(inline);
            } else {
                if big {
                    out.extend(extra_pos.to_le_bytes());
                } else {
                    out.extend((extra_pos as u32).to_le_bytes());
                }
                extra.extend(&bytes);
                if bytes.len() % 2 == 1 {
                    extra.push(0);
                }
                extra_pos += (bytes.len() as u64).next_multiple_of(2);
            }
        }
        let next = ifd_offsets.get(i + 1).copied().unwrap_or(0);
        if big {
            out.extend(next.to_le_bytes());
        } else {
            out.extend((next as u32).to_le_bytes());
        }
        out.extend(extra);
    }

    w.write_all(&out)?;
    for &i in &order {
        for c in &images[i].chunks {
            if cog {
                w.write_all(&(c.len() as u32).to_le_bytes())?;
            }
            w.write_all(c)?;
            if cog {
                let mut tail = [0u8; 4];
                let n = c.len().min(4);
                tail[4 - n..].copy_from_slice(&c[c.len() - n..]);
                w.write_all(&tail)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packbits_round_trips() {
        let data: Vec<u8> = [vec![7u8; 300], (0..200).map(|i| i as u8).collect(), vec![1, 1, 2, 2, 3]].concat();
        let enc = packbits(&data);
        // Decode.
        let mut out = Vec::new();
        let mut i = 0;
        while i < enc.len() {
            let n = enc[i] as i8;
            i += 1;
            if n >= 0 {
                out.extend_from_slice(&enc[i..i + n as usize + 1]);
                i += n as usize + 1;
            } else if n != -128 {
                out.extend(std::iter::repeat_n(enc[i], (1 - n as i32) as usize));
                i += 1;
            }
        }
        assert_eq!(out, data);
    }

    #[test]
    fn writes_a_cog_that_reads_back() {
        let cfg = wbraster::RasterConfig {
            cols: 70,
            rows: 50,
            bands: 2,
            x_min: 500000.0,
            y_min: 4000000.0,
            cell_size: 10.0,
            nodata: -1.0,
            data_type: DataType::I16,
            crs: wbraster::CrsInfo::from_epsg(32610),
            ..Default::default()
        };
        let data: Vec<i16> = (0..70 * 50 * 2).map(|i| (i % 1000) as i16 - 1).collect();
        let r = Raster::from_data_native(cfg, RasterData::I16(data.clone())).unwrap();
        for (cog, comp, big) in
            [(true, Compression::Deflate, false), (false, Compression::Lzw, true), (false, Compression::PackBits, false)]
        {
            let opts = TiffOptions { compression: comp, cog, tile_size: Some(32), bigtiff: big, nodata: Some(-1.0), ..Default::default() };
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("t.tif");
            write(&r, p.to_str().unwrap(), &opts).unwrap();
            let ds = crate::geotiff::read_geotiff(&p).unwrap();
            assert_eq!(ds.raster.data_i16().unwrap(), &data[..], "{comp:?}");
            assert_eq!(ds.nodata(), Some(-1.0));
            assert_eq!(ds.crs().epsg, Some(32610));
            assert_eq!(ds.geo_transform(), [500000.0, 10.0, 0.0, 4000500.0, 0.0, -10.0]);
        }
    }
}

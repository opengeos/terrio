//! Lazy rasters: open a raster for its metadata only, then read pixel windows, at full resolution
//! or from an overview.
//!
//! GeoTIFFs (Cloud Optimized or not, local or remote) are read in place: opening reads the image
//! headers, and a window read decodes only the tiles or strips that intersect the window. Remote
//! files are read through HTTP range requests (see [`crate::http`]), fetching the tiles of a window
//! in as few requests as possible. Other formats are read whole on open, then windows are cut from
//! memory.
//!
//! ```no_run
//! use terrio::{Window, WindowOptions};
//!
//! let mut r = terrio::open_raster_lazy("https://example.com/cog.tif")?;
//! println!("{} x {}, overviews {:?}", r.width(), r.height(), r.overviews());
//! // A 512 x 512 window of band 1 of the first overview.
//! let opts = WindowOptions { bands: vec![1], overview: Some(0) };
//! let ds = r.read_window(Window::new(0, 0, 512, 512), &opts)?;
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::crs::SpatialRef;
use crate::formats::RasterDriver;
use crate::geotiff::{GeoTags, PatchedReader, read_geo_tags_from};
use crate::vfs::{self, Location};
use crate::{Error, RasterDataset, Result};
use std::io::{BufReader, Read, Seek, SeekFrom};
use tiff::decoder::{ChunkType, Decoder, DecodingResult, Limits};
use tiff::tags::Tag;
use wbraster::DataType;
use wbraster::raster::RasterData;

/// The most images (IFDs) looked at for overviews.
const MAX_IMAGES: usize = 256;

/// A rectangle of pixels: `width` columns and `height` rows from column `x`, row `y`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Window {
    /// The first column.
    pub x: usize,
    /// The first row.
    pub y: usize,
    /// The number of columns.
    pub width: usize,
    /// The number of rows.
    pub height: usize,
}

impl Window {
    /// A window of `width` x `height` pixels from column `x`, row `y`.
    pub fn new(x: usize, y: usize, width: usize, height: usize) -> Self {
        Self { x, y, width, height }
    }

    /// The window of a whole image.
    pub fn full(width: usize, height: usize) -> Self {
        Self::new(0, 0, width, height)
    }

    /// Whether the window is not empty and lies within an image of `width` x `height` pixels.
    pub fn fits(&self, width: usize, height: usize) -> bool {
        self.width > 0
            && self.height > 0
            && self.x.checked_add(self.width).is_some_and(|e| e <= width)
            && self.y.checked_add(self.height).is_some_and(|e| e <= height)
    }
}

/// What [`LazyRaster::read_window`] reads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowOptions {
    /// The bands, numbered from 1 as in GDAL. Empty means all bands.
    pub bands: Vec<usize>,
    /// The overview to read, numbered from 0 (the largest) as in GDAL; `None` for full resolution.
    /// The window is in the overview's pixels.
    pub overview: Option<usize>,
}

/// A raster opened for its metadata, whose pixels are read on demand.
pub struct LazyRaster {
    inner: Inner,
}

impl std::fmt::Debug for LazyRaster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LazyRaster")
            .field("width", &self.width())
            .field("height", &self.height())
            .field("bands", &self.band_count())
            .field("overviews", &self.overviews())
            .finish_non_exhaustive()
    }
}

enum Inner {
    Tiff(Box<TiffRaster>),
    Memory(RasterDataset),
}

/// Opens a raster lazily: see [`LazyRaster::open`].
pub fn open_raster_lazy(path: &str) -> Result<LazyRaster> {
    LazyRaster::open(path)
}

impl LazyRaster {
    /// Opens a raster: a path, URL, `s3://`, `gs://` or `az://` object, or zip archive member.
    ///
    /// GeoTIFFs are read in place: only their headers are read now. Other formats are read whole.
    pub fn open(path: &str) -> Result<Self> {
        match Self::open_tiff(path)? {
            Some(r) => Ok(r),
            None => crate::open_raster(path).map(|ds| Self { inner: Inner::Memory(ds) }),
        }
    }

    /// Opens a GeoTIFF in place, reading only its headers. Returns `None` when the file is not a
    /// TIFF (having read only its first bytes).
    pub fn open_tiff(path: &str) -> Result<Option<Self>> {
        let loc = Location::parse(path);
        let (mut source, local) = match &loc {
            #[cfg(not(target_arch = "wasm32"))]
            Location::Url(_) | Location::S3 { .. } => {
                let remote = loc.remote().ok_or_else(|| Error::InvalidArgument(format!("'{path}' is not remote")))?;
                (Source::Http(crate::http::RangeReader::open(&remote)?), None)
            }
            Location::Local(p) if !p.is_file() => return Ok(None),
            Location::Local(p) => (Source::File(BufReader::new(std::fs::File::open(p)?)), None),
            _ => {
                let local = vfs::resolve(path)?;
                (Source::File(BufReader::new(std::fs::File::open(local.path())?)), Some(local))
            }
        };
        if !is_tiff(&mut source) {
            return Ok(None);
        }
        TiffRaster::new(source, local).map(|t| Some(Self { inner: Inner::Tiff(Box::new(t)) }))
    }

    /// Opens a GeoTIFF held in memory, reading only its headers now. Returns `None` when the bytes
    /// are not a TIFF.
    pub fn from_tiff_bytes(bytes: Vec<u8>) -> Result<Option<Self>> {
        let mut source = Source::Memory(std::io::Cursor::new(bytes));
        if !is_tiff(&mut source) {
            return Ok(None);
        }
        TiffRaster::new(source, None).map(|t| Some(Self { inner: Inner::Tiff(Box::new(t)) }))
    }

    /// The raster in memory, when it is not read on demand.
    pub fn dataset(&self) -> Option<&RasterDataset> {
        match &self.inner {
            Inner::Tiff(_) => None,
            Inner::Memory(ds) => Some(ds),
        }
    }

    /// Whether the pixels are read on demand (a GeoTIFF), rather than already in memory.
    pub fn is_lazy(&self) -> bool {
        matches!(self.inner, Inner::Tiff(_))
    }

    /// The width in pixels.
    pub fn width(&self) -> usize {
        match &self.inner {
            Inner::Tiff(t) => t.levels[0].width,
            Inner::Memory(ds) => ds.raster.cols,
        }
    }

    /// The height in pixels.
    pub fn height(&self) -> usize {
        match &self.inner {
            Inner::Tiff(t) => t.levels[0].height,
            Inner::Memory(ds) => ds.raster.rows,
        }
    }

    /// The number of bands.
    pub fn band_count(&self) -> usize {
        match &self.inner {
            Inner::Tiff(t) => t.bands,
            Inner::Memory(ds) => ds.raster.bands,
        }
    }

    /// The pixel data type.
    pub fn data_type(&self) -> DataType {
        match &self.inner {
            Inner::Tiff(t) => t.data_type,
            Inner::Memory(ds) => ds.raster.data_type,
        }
    }

    /// The driver.
    pub fn driver(&self) -> RasterDriver {
        match &self.inner {
            Inner::Tiff(_) => RasterDriver::GTiff,
            Inner::Memory(ds) => ds.driver,
        }
    }

    /// The GDAL-style geotransform of the full-resolution image.
    pub fn geo_transform(&self) -> [f64; 6] {
        match &self.inner {
            Inner::Tiff(t) => t.geo_transform,
            Inner::Memory(ds) => ds.geo_transform(),
        }
    }

    /// The CRS.
    pub fn crs(&self) -> SpatialRef {
        match &self.inner {
            Inner::Tiff(t) => SpatialRef::from_raster_crs(&t.tags.crs_info()),
            Inner::Memory(ds) => ds.crs(),
        }
    }

    /// The nodata value, when the file declares one.
    pub fn nodata(&self) -> Option<f64> {
        match &self.inner {
            Inner::Tiff(t) => t.tags.nodata,
            Inner::Memory(ds) => ds.nodata(),
        }
    }

    /// The sizes `(width, height)` of the overviews, largest first.
    pub fn overviews(&self) -> Vec<(usize, usize)> {
        match &self.inner {
            Inner::Tiff(t) => t.levels[1..].iter().map(|l| (l.width, l.height)).collect(),
            Inner::Memory(_) => Vec::new(),
        }
    }

    /// The tile (or strip) size `(width, height)` of the full-resolution image, when it is read on
    /// demand.
    pub fn block_size(&self) -> Option<(usize, usize)> {
        match &self.inner {
            Inner::Tiff(t) => Some(t.levels[0].block),
            Inner::Memory(_) => None,
        }
    }

    /// The geotransform of an overview (or of the full-resolution image for `None`).
    pub fn level_geo_transform(&self, overview: Option<usize>) -> Result<[f64; 6]> {
        let (w, h) = self.level_size(overview)?;
        let gt = self.geo_transform();
        let (sx, sy) = (self.width() as f64 / w as f64, self.height() as f64 / h as f64);
        Ok([gt[0], gt[1] * sx, gt[2] * sy, gt[3], gt[4] * sx, gt[5] * sy])
    }

    fn level_size(&self, overview: Option<usize>) -> Result<(usize, usize)> {
        match overview {
            None => Ok((self.width(), self.height())),
            Some(i) => {
                self.overviews().get(i).copied().ok_or_else(|| {
                    Error::InvalidArgument(format!("no overview {i}: the raster has {} overview(s)", self.overviews().len()))
                })
            }
        }
    }

    /// The raster's metadata (size, bands, data type, georeferencing, CRS, nodata) as a dataset
    /// without pixels: its data buffer is empty. For operations that only need the metadata, such
    /// as building a VRT.
    pub fn header(&self) -> RasterDataset {
        if let Inner::Memory(ds) = &self.inner {
            return ds.clone();
        }
        let gt = self.geo_transform();
        let raster = wbraster::Raster {
            cols: self.width(),
            rows: self.height(),
            bands: self.band_count(),
            x_min: gt[0],
            y_min: gt[3] + gt[5] * self.height() as f64,
            cell_size_x: gt[1],
            cell_size_y: -gt[5],
            nodata: self.nodata().unwrap_or(-9999.0),
            data_type: self.data_type(),
            crs: self.crs().to_crs_info(),
            metadata: Vec::new(),
            data: wbraster::raster::RasterData::new_filled(self.data_type(), 0, 0.0),
        };
        RasterDataset { raster, driver: self.driver(), has_nodata: self.nodata().is_some() }
    }

    /// Reads the whole full-resolution image.
    pub fn read(&mut self) -> Result<RasterDataset> {
        let window = Window::full(self.width(), self.height());
        self.read_window(window, &WindowOptions::default())
    }

    /// Reads a window of some bands, at full resolution or from an overview, as a raster
    /// georeferenced to the window.
    pub fn read_window(&mut self, window: Window, options: &WindowOptions) -> Result<RasterDataset> {
        let (lw, lh) = self.level_size(options.overview)?;
        if !window.fits(lw, lh) {
            return Err(Error::InvalidArgument(format!(
                "window {}x{} at ({}, {}) is outside the {lw}x{lh} image",
                window.width, window.height, window.x, window.y
            )));
        }
        let bands: Vec<usize> = if options.bands.is_empty() { (1..=self.band_count()).collect() } else { options.bands.clone() };
        if let Some(b) = bands.iter().find(|b| **b == 0 || **b > self.band_count()) {
            return Err(Error::InvalidArgument(format!("no band {b}: the raster has {} band(s)", self.band_count())));
        }
        let gt = self.level_geo_transform(options.overview)?;
        let (data, nodata, has_nodata, crs) = match &mut self.inner {
            Inner::Tiff(t) => {
                let level = options.overview.map_or(0, |i| i + 1);
                let data = t.read(level, window, &bands)?;
                (data, t.tags.nodata.unwrap_or(-9999.0), t.tags.nodata.is_some(), Some(t.tags.crs_info()))
            }
            Inner::Memory(ds) => (cut(&ds.raster, window, &bands), ds.raster.nodata, ds.has_nodata, Some(ds.raster.crs.clone())),
        };
        let cfg = wbraster::RasterConfig {
            cols: window.width,
            rows: window.height,
            bands: bands.len(),
            nodata,
            data_type: data.data_type(),
            crs: crs.unwrap_or_default(),
            ..Default::default()
        };
        let mut raster = wbraster::Raster::from_data_native(cfg, data)?;
        raster.cell_size_x = gt[1];
        raster.cell_size_y = -gt[5];
        raster.x_min = gt[0] + window.x as f64 * gt[1];
        raster.y_min = gt[3] + (window.y + window.height) as f64 * gt[5];
        Ok(RasterDataset { raster, driver: self.driver(), has_nodata })
    }
}

impl From<RasterDataset> for LazyRaster {
    /// Wraps a raster in memory, so that it is read by window like a lazy one.
    fn from(ds: RasterDataset) -> Self {
        Self { inner: Inner::Memory(ds) }
    }
}

/// Whether a stream starts with a TIFF or BigTIFF signature. Rewinds it.
fn is_tiff<R: Read + Seek>(r: &mut R) -> bool {
    let mut b = [0u8; 4];
    let ok = r.read_exact(&mut b).is_ok();
    let _ = r.seek(SeekFrom::Start(0));
    ok && matches!(&b, b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+")
}

/// Cuts a window of some bands (numbered from 1) out of a raster in memory.
fn cut(r: &wbraster::Raster, w: Window, bands: &[usize]) -> RasterData {
    macro_rules! cut {
        ($($v:ident),*) => {
            match &r.data {
                $(RasterData::$v(src) => {
                    let mut out = Vec::with_capacity(bands.len() * w.width * w.height);
                    for b in bands {
                        for row in w.y..w.y + w.height {
                            let start = (b - 1) * r.cols * r.rows + row * r.cols + w.x;
                            out.extend_from_slice(&src[start..start + w.width]);
                        }
                    }
                    RasterData::$v(out)
                })*
            }
        };
    }
    cut!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64)
}

/// Where a TIFF's bytes come from.
enum Source {
    File(BufReader<std::fs::File>),
    Memory(std::io::Cursor<Vec<u8>>),
    #[cfg(not(target_arch = "wasm32"))]
    Http(crate::http::RangeReader),
}

impl Source {
    /// Fetches byte ranges `(offset, length)` ahead of reading them, when that saves requests.
    #[cfg_attr(target_arch = "wasm32", allow(unused_variables))]
    fn prefetch(&self, ranges: &[(u64, u64)]) -> Result<()> {
        match self {
            Source::File(_) | Source::Memory(_) => Ok(()),
            #[cfg(not(target_arch = "wasm32"))]
            Source::Http(r) => Ok(r.prefetch(ranges)?),
        }
    }
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Source::File(f) => f.read(buf),
            Source::Memory(m) => m.read(buf),
            #[cfg(not(target_arch = "wasm32"))]
            Source::Http(r) => r.read(buf),
        }
    }
}

impl Seek for Source {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self {
            Source::File(f) => f.seek(pos),
            Source::Memory(m) => m.seek(pos),
            #[cfg(not(target_arch = "wasm32"))]
            Source::Http(r) => r.seek(pos),
        }
    }
}

/// One image of a TIFF: the full-resolution image or an overview.
struct Level {
    /// The image's index in the file.
    ifd: usize,
    width: usize,
    height: usize,
    /// The tile or strip size.
    block: (usize, usize),
}

struct TiffRaster {
    decoder: Decoder<PatchedReader<Source>>,
    tags: GeoTags,
    bands: usize,
    data_type: DataType,
    geo_transform: [f64; 6],
    /// The full-resolution image, then the overviews, largest first.
    levels: Vec<Level>,
    /// The index of the image the decoder is on.
    current_ifd: usize,
    /// Keeps a file extracted from an archive.
    _local: Option<vfs::LocalPath>,
}

fn tiff_err(e: tiff::TiffError) -> Error {
    Error::InvalidData(format!("TIFF: {e}"))
}

impl TiffRaster {
    fn new(mut source: Source, local: Option<vfs::LocalPath>) -> Result<Self> {
        let tags = read_geo_tags_from(&mut source).ok_or_else(|| Error::InvalidData("not a TIFF file".into()))?;
        source.seek(SeekFrom::Start(0))?;
        let mut dec = Decoder::new(PatchedReader::new(source, &tags)).map_err(tiff_err)?.with_limits(Limits::unlimited());
        let (w, h) = dec.dimensions().map_err(tiff_err)?;
        let bands = dec.find_tag_unsigned::<u16>(Tag::SamplesPerPixel).map_err(tiff_err)?.unwrap_or(1).max(1) as usize;
        let data_type = sample_type(&mut dec)?;
        let (cw, ch) = dec.chunk_dimensions();
        let mut levels = vec![Level { ifd: 0, width: w as usize, height: h as usize, block: (cw as usize, ch as usize) }];
        let mut ifd = 1;
        while dec.more_images() && ifd < MAX_IMAGES {
            // Images the decoder cannot read (such as unusual masks) are skipped.
            if dec.seek_to_image(ifd).is_ok() {
                let subfile = dec.find_tag_unsigned::<u32>(Tag::NewSubfileType).ok().flatten().unwrap_or(0);
                let samples = dec.find_tag_unsigned::<u16>(Tag::SamplesPerPixel).ok().flatten().unwrap_or(1).max(1) as usize;
                let same_type = sample_type(&mut dec).is_ok_and(|t| t == data_type);
                // A reduced-resolution image that is not a transparency mask.
                if subfile & 1 == 1 && subfile & 4 == 0 && samples == bands && same_type {
                    let (ow, oh) = dec.dimensions().map_err(tiff_err)?;
                    let (cw, ch) = dec.chunk_dimensions();
                    levels.push(Level { ifd, width: ow as usize, height: oh as usize, block: (cw as usize, ch as usize) });
                }
            }
            ifd += 1;
        }
        levels[1..].sort_by_key(|l| std::cmp::Reverse((l.width, l.height)));
        if ifd > 1 {
            dec.seek_to_image(0).map_err(tiff_err)?;
        }
        let geo_transform = match tags.geo_transform {
            Some(gt) if gt[2] == 0.0 && gt[4] == 0.0 && gt[1] > 0.0 && gt[5] < 0.0 => {
                // PixelIsPoint tie points refer to pixel centres; shift to the corner, as GDAL does.
                let (dx, dy) = if tags.pixel_is_point { (gt[1] / 2.0, gt[5] / 2.0) } else { (0.0, 0.0) };
                [gt[0] - dx, gt[1], 0.0, gt[3] - dy, 0.0, gt[5]]
            }
            Some(_) => return Err(Error::Unsupported("rotated or south-up geotransforms are not supported".into())),
            // As the in-memory reader does: unit pixels, with the image's bottom at y = 0.
            None => [0.0, 1.0, 0.0, h as f64, 0.0, -1.0],
        };
        Ok(Self { decoder: dec, tags, bands, data_type, geo_transform, levels, current_ifd: 0, _local: local })
    }

    /// Reads a window of some bands (numbered from 1) of a level, band-major.
    fn read(&mut self, level: usize, w: Window, bands: &[usize]) -> Result<RasterData> {
        let ifd = self.levels[level].ifd;
        let dec = &mut self.decoder;
        if self.current_ifd != ifd {
            dec.seek_to_image(ifd).map_err(tiff_err)?;
            self.current_ifd = ifd;
        }
        let (lw, lh) = (self.levels[level].width, self.levels[level].height);
        let planar = dec.find_tag_unsigned::<u16>(Tag::PlanarConfiguration).map_err(tiff_err)? == Some(2);
        let (cw, ch) = dec.chunk_dimensions();
        let (cw, ch) = (cw as usize, ch as usize);
        let across = lw.div_ceil(cw);
        let per_plane = across * lh.div_ceil(ch);
        let (offsets_tag, counts_tag) = match dec.get_chunk_type() {
            ChunkType::Tile => (Tag::TileOffsets, Tag::TileByteCounts),
            ChunkType::Strip => (Tag::StripOffsets, Tag::StripByteCounts),
        };
        let offsets = dec.get_tag_u64_vec(offsets_tag).map_err(tiff_err)?;
        let counts = dec.get_tag_u64_vec(counts_tag).map_err(tiff_err)?;

        // The chunks to read, with the output bands each one fills: (chunk, [(sample, out band)]).
        let planes: Vec<(usize, Vec<(usize, usize)>)> = if planar {
            bands.iter().enumerate().map(|(k, b)| (b - 1, vec![(0, k)])).collect()
        } else {
            vec![(0, bands.iter().enumerate().map(|(k, b)| (b - 1, k)).collect())]
        };
        let mut chunks = Vec::new();
        for (plane, map) in &planes {
            for ty in w.y / ch..=(w.y + w.height - 1) / ch {
                for tx in w.x / cw..=(w.x + w.width - 1) / cw {
                    chunks.push((plane * per_plane + ty * across + tx, tx * cw, ty * ch, map));
                }
            }
        }
        let ranges: Vec<(u64, u64)> =
            chunks.iter().filter_map(|(i, ..)| Some((*offsets.get(*i)?, *counts.get(*i)?))).filter(|(_, n)| *n > 0).collect();
        dec.inner().get_ref().prefetch(&ranges)?;

        // Missing (sparse) chunks read as nodata, or 0, as in GDAL.
        let len = bands.len() * w.width * w.height;
        let mut out = RasterData::new_filled(self.data_type, len, self.tags.nodata.unwrap_or(0.0));
        let spp = if planar { 1 } else { self.bands };
        for (idx, x0, y0, map) in chunks {
            if counts.get(idx).copied().unwrap_or(0) == 0 {
                continue;
            }
            let chunk = dec.read_chunk(idx as u32).map_err(tiff_err)?;
            let (dw, dh) = dec.chunk_data_dimensions(idx as u32);
            let geom = ChunkGeom { x0, y0, dw: dw as usize, dh: dh as usize, spp };
            macro_rules! place {
                ($($out:ident <- $res:ident),*) => {
                    match (&mut out, chunk) {
                        $((RasterData::$out(o), DecodingResult::$res(v)) => place(o, &v, &geom, w, map),)*
                        (RasterData::F32(o), DecodingResult::F16(v)) => {
                            let v: Vec<f32> = v.iter().map(|x| x.to_f32()).collect();
                            place(o, &v, &geom, w, map)
                        }
                        _ => return Err(Error::InvalidData("TIFF chunks differ in sample type".into())),
                    }
                };
            }
            place!(U8 <- U8, I8 <- I8, U16 <- U16, I16 <- I16, U32 <- U32, I32 <- I32, U64 <- U64, I64 <- I64, F32 <- F32, F64 <- F64);
        }
        Ok(out)
    }
}

/// Where a decoded chunk lies in its image.
struct ChunkGeom {
    /// The image column and row of the chunk's first pixel.
    x0: usize,
    y0: usize,
    /// The chunk's columns and rows that hold image data (edge chunks are cropped).
    dw: usize,
    dh: usize,
    /// Samples per pixel in the chunk.
    spp: usize,
}

/// Copies the part of a chunk that intersects a window into the window's band-major buffer.
/// `map` pairs each sample to copy with its output band.
fn place<T: Copy>(out: &mut [T], vals: &[T], g: &ChunkGeom, w: Window, map: &[(usize, usize)]) {
    if g.dh == 0 {
        return;
    }
    // Rows in a chunk may be padded to the full chunk width.
    let stride = vals.len() / (g.dh * g.spp);
    let band_len = w.width * w.height;
    let (c0, c1) = (g.x0.max(w.x), (g.x0 + g.dw).min(w.x + w.width));
    let (r0, r1) = (g.y0.max(w.y), (g.y0 + g.dh).min(w.y + w.height));
    for row in r0..r1 {
        let src_row = (row - g.y0) * stride;
        let dst_row = (row - w.y) * w.width;
        for col in c0..c1 {
            let src = (src_row + col - g.x0) * g.spp;
            let dst = dst_row + col - w.x;
            for (sample, band) in map {
                if let Some(v) = vals.get(src + sample) {
                    out[band * band_len + dst] = *v;
                }
            }
        }
    }
}

/// The pixel type of the decoder's current image, from `BitsPerSample` and `SampleFormat`.
fn sample_type<R: Read + Seek>(dec: &mut Decoder<R>) -> Result<DataType> {
    let bits = dec.find_tag_unsigned_vec::<u16>(Tag::BitsPerSample).map_err(tiff_err)?.and_then(|v| v.first().copied()).unwrap_or(1);
    let format = dec.find_tag_unsigned_vec::<u16>(Tag::SampleFormat).map_err(tiff_err)?.and_then(|v| v.first().copied()).unwrap_or(1);
    Ok(match (format, bits) {
        (2, 8) => DataType::I8,
        (2, 16) => DataType::I16,
        (2, 32) => DataType::I32,
        (2, 64) => DataType::I64,
        (3, 16 | 32) => DataType::F32,
        (3, 64) => DataType::F64,
        (_, 1..=8) => DataType::U8,
        (_, 16) => DataType::U16,
        (_, 32) => DataType::U32,
        (_, 64) => DataType::U64,
        _ => return Err(Error::Unsupported(format!("TIFF samples of {bits} bits (format {format}) are not supported"))),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_fits() {
        assert!(Window::new(0, 0, 10, 10).fits(10, 10));
        assert!(!Window::new(1, 0, 10, 10).fits(10, 10));
        assert!(!Window::new(0, 0, 0, 10).fits(10, 10));
        assert!(!Window::new(usize::MAX, 0, 2, 1).fits(10, 10));
    }

    #[test]
    fn places_chunk_parts() {
        // A 4x3 chunk (padded to 5 columns) of 2 samples at (4, 2), cropped to 3x2 at the image edge.
        let vals: Vec<u16> = (0..5 * 3 * 2).collect();
        let g = ChunkGeom { x0: 4, y0: 2, dw: 3, dh: 2, spp: 2 };
        let w = Window::new(5, 3, 4, 2);
        let mut out = vec![0u16; 2 * 4 * 2];
        // Sample 1 to band 0, sample 0 to band 1.
        place(&mut out, &vals[..5 * 2 * 2], &g, w, &[(1, 0), (0, 1)]);
        // The window's row 0 is the chunk's row 1; its columns 0-1 are the chunk's columns 1-2.
        let src = |r: usize, c: usize, s: usize| ((r * 5 + c) * 2 + s) as u16;
        assert_eq!(&out[0..4], &[src(1, 1, 1), src(1, 2, 1), 0, 0]);
        assert_eq!(&out[4..8], &[0; 4]);
        assert_eq!(&out[8..12], &[src(1, 1, 0), src(1, 2, 0), 0, 0]);
    }

    #[test]
    fn cuts_windows_in_memory() {
        let cfg = wbraster::RasterConfig { cols: 4, rows: 3, bands: 2, data_type: DataType::I32, ..Default::default() };
        let data = RasterData::I32((0..24).collect());
        let r = wbraster::Raster::from_data_native(cfg, data).unwrap();
        let RasterData::I32(v) = cut(&r, Window::new(1, 1, 2, 2), &[2]) else { panic!() };
        assert_eq!(v, vec![17, 18, 21, 22]);
    }
}

//! Raster tiles as rasters: reading a zoom level (and the lower ones as overviews) by window, and
//! writing rasters as tile pyramids, as GDAL's MBTiles and PMTiles drivers do.

use super::{MAX_GM, TileArchive, TileFormat, TileWriteOptions, lonlat_to_mercator, mercator_to_lonlat};
use crate::crs::SpatialRef;
use crate::{Error, RasterDataset, Result};
use std::collections::HashMap;
use wbraster::DataType;
use wbraster::raster::RasterData;

/// The resolution of zoom level `z` with tiles of `ts` pixels.
fn resolution(z: u8, ts: usize) -> f64 {
    2.0 * MAX_GM / ts as f64 / (1u64 << z) as f64
}

// ─── Tile images ────────────────────────────────────────────────────────────────────────────────

/// A decoded tile: width, height, channels (1 gray, 2 gray and alpha, 3 RGB, 4 RGBA), samples.
pub(crate) struct Image {
    pub w: usize,
    pub h: usize,
    pub channels: usize,
    pub data: Vec<u8>,
    /// Whether it had a palette (GDAL reads those as RGB).
    pub palette: bool,
}

/// Decodes a PNG, JPEG, WebP or AVIF tile to 8-bit samples.
pub(crate) fn decode_image(b: &[u8]) -> Result<Image> {
    let err = |e: &dyn std::fmt::Display| Error::InvalidData(format!("tile image: {e}"));
    if b.starts_with(b"\x89PNG") {
        let mut d = png::Decoder::new(std::io::Cursor::new(b));
        d.set_transformations(png::Transformations::normalize_to_color8());
        let mut reader = d.read_info().map_err(|e| err(&e))?;
        let palette = reader.info().color_type == png::ColorType::Indexed;
        let mut buf = vec![0u8; reader.output_buffer_size().ok_or_else(|| err(&"too large"))?];
        let info = reader.next_frame(&mut buf).map_err(|e| err(&e))?;
        let channels = info.color_type.samples();
        let (w, h) = (info.width as usize, info.height as usize);
        buf.truncate(w * h * channels);
        return Ok(Image { w, h, channels, data: buf, palette });
    }
    if b.starts_with(&[0xff, 0xd8]) {
        let img = crate::jpeg::decode_file(b).map_err(|e| err(&e))?;
        return Ok(Image { w: img.width, h: img.height, channels: img.components, data: img.data, palette: false });
    }
    if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        let mut d = image_webp::WebPDecoder::new(std::io::Cursor::new(b)).map_err(|e| err(&e))?;
        let (w, h) = d.dimensions();
        let channels = if d.has_alpha() { 4 } else { 3 };
        let mut data = vec![0u8; d.output_buffer_size().ok_or_else(|| err(&"too large"))?];
        d.read_image(&mut data).map_err(|e| err(&e))?;
        return Ok(Image { w: w as usize, h: h as usize, channels, data, palette: false });
    }
    if crate::tiles::is_avif(b) {
        #[cfg(feature = "avif")]
        {
            let img = crate::avif::decode(b)?;
            return Ok(Image { w: img.width, h: img.height, channels: img.channels, data: img.data, palette: false });
        }
        #[cfg(not(feature = "avif"))]
        return Err(Error::disabled("avif"));
    }
    Err(err(&"unsupported tile format (PNG, JPEG, WebP and AVIF are supported)"))
}

/// One pixel of a tile as `bands` values: gray and RGB values for the first bands, alpha (opaque
/// when the tile has none) for the last of 2 or 4, as GDAL expands tiles.
fn pixel(img: &Image, i: usize, bands: usize, out: &mut [u8]) {
    let px = &img.data[i * img.channels..(i + 1) * img.channels];
    let rgba = match px.len() {
        1 => [px[0], px[0], px[0], 255],
        2 => [px[0], px[0], px[0], px[1]],
        3 => [px[0], px[1], px[2], 255],
        _ => [px[0], px[1], px[2], px[3]],
    };
    match bands {
        1 => out[0] = rgba[0],
        2 => {
            out[0] = rgba[0];
            out[1] = rgba[3];
        }
        3 => out[..3].copy_from_slice(&rgba[..3]),
        _ => out[..4].copy_from_slice(&rgba),
    }
}

fn encode_png(w: usize, h: usize, channels: usize, data: &[u8]) -> Result<Vec<u8>> {
    let err = |e: png::EncodingError| Error::InvalidData(format!("PNG tile: {e}"));
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
        enc.set_color(match channels {
            1 => png::ColorType::Grayscale,
            2 => png::ColorType::GrayscaleAlpha,
            3 => png::ColorType::Rgb,
            _ => png::ColorType::Rgba,
        });
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().map_err(err)?;
        wr.write_image_data(data).map_err(err)?;
        wr.finish().map_err(err)?;
    }
    Ok(out)
}

fn encode_jpeg(w: usize, h: usize, channels: usize, data: &[u8], quality: u8) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let enc = jpeg_encoder::Encoder::new(&mut out, quality);
    let color = if channels == 1 { jpeg_encoder::ColorType::Luma } else { jpeg_encoder::ColorType::Rgb };
    enc.encode(data, w as u16, h as u16, color).map_err(|e| Error::InvalidData(format!("JPEG tile: {e}")))?;
    Ok(out)
}

fn encode_webp(w: usize, h: usize, channels: usize, data: &[u8]) -> Result<Vec<u8>> {
    // The lossless encoder takes RGB(A): gray tiles are expanded.
    let (rgb, color): (std::borrow::Cow<[u8]>, _) = match channels {
        1 => (data.iter().flat_map(|v| [*v, *v, *v]).collect::<Vec<u8>>().into(), image_webp::ColorType::Rgb8),
        2 => (data.chunks(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect::<Vec<u8>>().into(), image_webp::ColorType::Rgba8),
        3 => (data.into(), image_webp::ColorType::Rgb8),
        _ => (data.into(), image_webp::ColorType::Rgba8),
    };
    let mut out = Vec::new();
    image_webp::WebPEncoder::new(&mut out)
        .encode(&rgb, w as u32, h as u32, color)
        .map_err(|e| Error::InvalidData(format!("WebP tile: {e}")))?;
    Ok(out)
}

// ─── Reading ────────────────────────────────────────────────────────────────────────────────────

/// A zoom level read as a raster: its size and the global pixel position of its first pixel.
#[derive(Debug, Clone, Copy)]
struct Level {
    zoom: u8,
    width: usize,
    height: usize,
    shift_x: i64,
    shift_y: i64,
}

/// A tile archive's image tiles read as a raster in EPSG:3857: a zoom level as full resolution
/// and the lower ones as overviews, read by window.
pub(crate) struct TileRaster {
    archive: TileArchive,
    levels: Vec<Level>,
    pub bands: usize,
    tile_size: usize,
    /// `[min_x, min_y, max_x, max_y]` in EPSG:3857.
    extent: [f64; 4],
    cache: HashMap<(u8, u32, u32), Option<std::sync::Arc<Image>>>,
}

impl std::fmt::Debug for TileRaster {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileRaster").field("levels", &self.levels).field("bands", &self.bands).finish_non_exhaustive()
    }
}

impl TileRaster {
    /// Reads an archive's layout: the full resolution zoom level (the highest by default), its
    /// extent and band count. MBTiles extents come from the `bounds` metadata (or the tiles), as
    /// in GDAL's MBTiles driver; PMTiles extents from the header's bounds aligned on the pixel
    /// grid, as in GDAL's PMTiles driver. MBTiles are read as 4 bands (RGBA), PMTiles with the
    /// bands of their tiles.
    pub(crate) fn new(mut archive: TileArchive, zoom_level: Option<u8>) -> Result<Self> {
        if archive.is_vector() {
            return Err(Error::Unsupported("the archive holds vector tiles, not a raster".into()));
        }
        let (min_z, max_z) = archive.zoom_range();
        let zoom = zoom_level.unwrap_or(max_z);
        if zoom < min_z || zoom > max_z {
            return Err(Error::InvalidArgument(format!("invalid zoom level {zoom}: the archive has zoom levels {min_z} to {max_z}")));
        }
        let present = archive.tiles_at_zoom(zoom)?;
        let &(fx, fy) = present.first().ok_or_else(|| Error::InvalidData(format!("the archive has no tiles at zoom level {zoom}")))?;
        let first = archive.tile(zoom, fx, fy)?.ok_or_else(|| Error::InvalidData("missing tile".into()))?;
        let img = decode_image(&first)?;
        let tile_size = img.w.max(1);
        let res = resolution(zoom, tile_size);
        let kind = archive.kind();
        let tile_extent = || {
            let (x0, x1) = (present.iter().map(|t| t.0).min().unwrap_or(0), present.iter().map(|t| t.0).max().unwrap_or(0));
            let (y0, y1) = (present.iter().map(|t| t.1).min().unwrap_or(0), present.iter().map(|t| t.1).max().unwrap_or(0));
            let dim = 2.0 * MAX_GM / (1u64 << zoom) as f64;
            [-MAX_GM + x0 as f64 * dim, MAX_GM - (y1 + 1) as f64 * dim, -MAX_GM + (x1 + 1) as f64 * dim, MAX_GM - y0 as f64 * dim]
        };
        let to_merc = |b: [f64; 4]| {
            let (x0, y0) = lonlat_to_mercator(b[0], b[1]);
            let (x1, y1) = lonlat_to_mercator(b[2], b[3]);
            [x0, y0.max(-MAX_GM), x1, y1.min(MAX_GM)]
        };
        let (extent, width, height, bands) = match kind {
            super::ArchiveKind::MBTiles => {
                let e = archive.bounds().map(to_merc).unwrap_or_else(tile_extent);
                let w = (0.5 + (e[2] - e[0]) / res) as usize;
                let h = (0.5 + (e[3] - e[1]) / res) as usize;
                (e, w, h, 4)
            }
            super::ArchiveKind::PMTiles => {
                let b = archive.bounds().filter(|b| b[0] < b[2] && b[1] < b[3]);
                let e = b.map(to_merc).unwrap_or_else(tile_extent);
                const EPS: f64 = 1e-2;
                let e = [
                    (e[0] / res + EPS).floor() * res,
                    (e[1] / res + EPS).floor() * res,
                    (e[2] / res - EPS).ceil() * res,
                    (e[3] / res - EPS).ceil() * res,
                ];
                let w = (((e[2] - e[0]) / res + EPS) as usize).max(1);
                let h = (((e[3] - e[1]) / res + EPS) as usize).max(1);
                let bands = if img.palette { 3 } else { img.channels };
                (e, w, h, bands)
            }
        };
        if width == 0 || height == 0 {
            return Err(Error::InvalidData("the archive's extent is empty".into()));
        }
        let mut levels = Vec::new();
        for z in (min_z..=zoom).rev() {
            let r = resolution(z, tile_size);
            let (w, h) = if z == zoom {
                (width, height)
            } else {
                (((extent[2] - extent[0]) / r + 0.5).max(1.0) as usize, ((extent[3] - extent[1]) / r + 0.5).max(1.0) as usize)
            };
            let shift_x = ((extent[0] + MAX_GM) / r + 0.5).floor() as i64;
            let shift_y = ((MAX_GM - extent[3]) / r + 0.5).floor() as i64;
            levels.push(Level { zoom: z, width: w, height: h, shift_x, shift_y });
        }
        Ok(Self { archive, levels, bands, tile_size, extent, cache: HashMap::new() })
    }

    pub(crate) fn width(&self) -> usize {
        self.levels[0].width
    }

    pub(crate) fn height(&self) -> usize {
        self.levels[0].height
    }

    /// The overview sizes, largest first.
    pub(crate) fn overviews(&self) -> Vec<(usize, usize)> {
        self.levels[1..].iter().map(|l| (l.width, l.height)).collect()
    }

    /// The geotransform of the full resolution.
    pub(crate) fn geo_transform(&self) -> [f64; 6] {
        let res = resolution(self.levels[0].zoom, self.tile_size);
        [self.extent[0], res, 0.0, self.extent[3], 0.0, -res]
    }

    /// The archive's metadata, with the zoom level read, as GDAL lists it.
    pub(crate) fn metadata(&self) -> Vec<(String, String)> {
        let mut md = vec![("ZOOM_LEVEL".to_string(), self.levels[0].zoom.to_string())];
        md.extend(self.archive.metadata().into_iter().filter(|(k, _)| k != "json"));
        md
    }

    pub(crate) fn driver(&self) -> crate::formats::RasterDriver {
        match self.archive.kind() {
            super::ArchiveKind::PMTiles => crate::formats::RasterDriver::PMTiles,
            super::ArchiveKind::MBTiles => crate::formats::RasterDriver::MBTiles,
        }
    }

    /// Reads a window of a level (0 is the full resolution) for some bands (from 1), as Byte
    /// band-major samples. Pixels without tiles are 0.
    pub(crate) fn read(&mut self, level: usize, win: crate::Window, bands: &[usize]) -> Result<RasterData> {
        let l = *self.levels.get(level).ok_or_else(|| Error::InvalidArgument(format!("no level {level}")))?;
        let ts = self.tile_size as i64;
        let n = (1i64 << l.zoom) - 1;
        let gx0 = l.shift_x + win.x as i64;
        let gy0 = l.shift_y + win.y as i64;
        let (gx1, gy1) = (gx0 + win.width as i64, gy0 + win.height as i64);
        let (tx0, tx1) = (gx0.div_euclid(ts).max(0), (gx1 - 1).div_euclid(ts).min(n));
        let (ty0, ty1) = (gy0.div_euclid(ts).max(0), (gy1 - 1).div_euclid(ts).min(n));
        let mut wanted = Vec::new();
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let key = (l.zoom, tx as u32, ty as u32);
                if !self.cache.contains_key(&key) {
                    wanted.push(key);
                }
            }
        }
        if !wanted.is_empty() {
            if self.cache.len() > 4096 {
                self.cache.clear();
            }
            for chunk in wanted.chunks(512) {
                let data = self.archive.tiles(chunk)?;
                for (key, bytes) in chunk.iter().zip(data) {
                    let img = match bytes {
                        Some(b) => Some(std::sync::Arc::new(decode_image(&b)?)),
                        None => None,
                    };
                    self.cache.insert(*key, img);
                }
            }
        }
        let nb = self.bands;
        let (w, h) = (win.width, win.height);
        let mut out = vec![0u8; bands.len() * w * h];
        let mut px = [0u8; 4];
        for ty in ty0..=ty1 {
            for tx in tx0..=tx1 {
                let Some(Some(img)) = self.cache.get(&(l.zoom, tx as u32, ty as u32)) else { continue };
                let img = img.clone();
                for y in 0..ts.min(img.h as i64) {
                    let gy = ty * ts + y;
                    if gy < gy0 || gy >= gy1 {
                        continue;
                    }
                    let oy = (gy - gy0) as usize;
                    for x in 0..ts.min(img.w as i64) {
                        let gx = tx * ts + x;
                        if gx < gx0 || gx >= gx1 {
                            continue;
                        }
                        let ox = (gx - gx0) as usize;
                        pixel(&img, y as usize * img.w + x as usize, nb, &mut px);
                        for (i, b) in bands.iter().enumerate() {
                            out[(i * h + oy) * w + ox] = px[b - 1];
                        }
                    }
                }
            }
        }
        Ok(RasterData::U8(out))
    }

    /// The raster's metadata (size, bands, georeferencing, CRS, metadata items) without pixels.
    pub(crate) fn header(&self) -> RasterDataset {
        let gt = self.geo_transform();
        let (w, h) = (self.width(), self.height());
        let crs = SpatialRef::from_epsg(3857).map(|s| s.to_crs_info()).unwrap_or_default();
        let raster = wbraster::Raster {
            cols: w,
            rows: h,
            bands: self.bands,
            x_min: gt[0],
            y_min: gt[3] + gt[5] * h as f64,
            cell_size_x: gt[1],
            cell_size_y: -gt[5],
            nodata: -9999.0,
            data_type: DataType::U8,
            crs,
            metadata: self.metadata(),
            data: RasterData::new_filled(DataType::U8, 0, 0.0),
        };
        RasterDataset::with_driver(raster, self.driver(), false)
    }

    /// Reads the whole full resolution level.
    pub(crate) fn read_all(&mut self) -> Result<RasterDataset> {
        let (w, h) = (self.width(), self.height());
        if (w as u64) * (h as u64) * (self.bands as u64) > 1 << 32 {
            return Err(Error::Unsupported(format!(
                "the zoom level {} raster is {w} x {h} pixels: read it by window (LazyRaster) or open a lower zoom level",
                self.levels[0].zoom
            )));
        }
        let bands: Vec<usize> = (1..=self.bands).collect();
        let data = self.read(0, crate::Window::full(w, h), &bands)?;
        let crs = SpatialRef::from_epsg(3857)?.to_crs_info();
        let mut raster = crate::drivers::common::make_raster(w, h, self.bands, data, self.geo_transform(), crs, None)?;
        raster.metadata = self.metadata();
        Ok(RasterDataset::with_driver(raster, self.driver(), false))
    }
}

impl crate::cog::WindowSource for TileRaster {
    fn overviews(&self) -> Vec<(usize, usize)> {
        TileRaster::overviews(self)
    }

    fn read(&mut self, level: usize, window: crate::Window, bands: &[usize]) -> Result<RasterData> {
        TileRaster::read(self, level, window, bands)
    }
}

/// Opens a PMTiles or MBTiles archive of image tiles for reading by window. `None` when the path
/// does not name a tile archive.
pub(crate) fn open_lazy(path: &str) -> Result<Option<TileRaster>> {
    let loc = crate::vfs::Location::parse(path);
    if matches!(loc, crate::vfs::Location::Zip { .. }) || super::archive_kind(&loc.file_name()).is_none() {
        return Ok(None);
    }
    let archive = TileArchive::open_named(path)?;
    if archive.is_vector() {
        return Err(Error::Unsupported(format!("'{path}' holds vector tiles, not a raster")));
    }
    TileRaster::new(archive, None).map(Some)
}

/// Opens an archive of image tiles held in memory for reading by window.
pub(crate) fn open_lazy_bytes(bytes: Vec<u8>) -> Result<TileRaster> {
    TileRaster::new(TileArchive::from_bytes_judged(bytes, &[])?, None)
}

// ─── Writing ────────────────────────────────────────────────────────────────────────────────────

/// A level of the pyramid being written: data bands (1 or 3) and an alpha band, band-major.
struct Pixels {
    w: usize,
    h: usize,
    bands: usize,
    data: Vec<u8>,
    alpha: Vec<u8>,
}

/// A resampling kernel: its weight at a distance (in source pixels).
#[cfg(test)]
type Kernel = fn(f64) -> f64;

/// Tiles `(z, x, y, bytes)`.
#[cfg(test)]
type Tiles = Vec<(u8, u32, u32, Vec<u8>)>;

/// The in-memory reference the streamed pyramid ([`super::pyramid`]) is checked against.
/// Downsamples a level to `w` x `h` pixels, as GDAL computes overviews (`gdaladdo`): nearest,
/// average, or a separable convolution (bilinear, cubic) whose kernel is stretched by the
/// reduction factor, with the alpha band as the weights of every band (horizontal pass, then
/// vertical pass over the horizontally filtered alpha), as GDAL weights by a dataset's mask.
#[cfg(test)]
fn downsample(src: &Pixels, w: usize, h: usize, method: &str) -> Pixels {
    let (sw, sh) = (src.w, src.h);
    let (rx, ry) = (sw as f64 / w as f64, sh as f64 / h as f64);
    let nb = src.bands + 1;
    let n = sw * sh;
    // Band `b` of the source (the alpha band last).
    let band = |b: usize, i: usize| if b < src.bands { src.data[b * n + i] } else { src.alpha[i] } as f64;
    let mut out = vec![0u8; nb * w * h];
    let kernel: Option<(f64, Kernel)> = match method {
        "bilinear" => Some((1.0, |x: f64| (1.0 - x.abs()).max(0.0))),
        "cubic" => Some((2.0, |x: f64| {
            let x = x.abs();
            let a = -0.5;
            if x <= 1.0 {
                ((a + 2.0) * x - (a + 3.0)) * x * x + 1.0
            } else if x < 2.0 {
                a * (((x - 5.0) * x + 8.0) * x - 4.0)
            } else {
                0.0
            }
        })),
        _ => None,
    };
    if let Some((radius, k)) = kernel {
        // Per-axis taps, as GDAL computes them: for each output index, the first source index and
        // the weights.
        let taps = |dst: usize, src_n: usize, ratio: f64| -> Vec<(usize, Vec<f64>)> {
            let scale = (1.0 / ratio).min(1.0);
            let r = radius / scale;
            (0..dst)
                .map(|i| {
                    let c = (i as f64 + 0.5) * ratio;
                    let lo = ((c - r + 0.5).floor() as i64).max(0) as usize;
                    let hi = ((c + r + 0.5) as i64).min(src_n as i64).max(lo as i64) as usize;
                    (lo, (lo..hi).map(|j| k(scale * (j as f64 - c + 0.5))).collect())
                })
                .collect()
        };
        let (tx, ty) = (taps(w, sw, rx), taps(h, sh, ry));
        // Horizontal pass: values and the filtered mask, per source row.
        let mut hv = vec![0.0f64; nb * sh * w];
        let mut hm = vec![0.0f64; sh * w];
        for y in 0..sh {
            for (x, (lo, wt)) in tx.iter().enumerate() {
                let (mut sa, mut sw_) = (0.0, 0.0);
                let mut sv = [0.0f64; 4];
                for (t, &wj) in wt.iter().enumerate() {
                    let i = y * sw + lo + t;
                    let m = wj * src.alpha[i] as f64;
                    sa += m;
                    sw_ += wj;
                    for (b, v) in sv.iter_mut().enumerate().take(nb) {
                        *v += m * band(b, i);
                    }
                }
                let o = y * w + x;
                if sa > 0.0 {
                    for (b, v) in sv.iter().enumerate().take(nb) {
                        hv[(b * sh + y) * w + x] = (v / sa).clamp(0.0, 255.0);
                    }
                    hm[o] = (sa / sw_ + 0.5).min(255.0).floor();
                }
            }
        }
        // Vertical pass.
        for (y, (lo, wt)) in ty.iter().enumerate() {
            for x in 0..w {
                let mut sa = 0.0;
                let mut sv = [0.0f64; 4];
                for (t, &wj) in wt.iter().enumerate() {
                    let r = lo + t;
                    let m = wj * hm[r * w + x];
                    sa += m;
                    for (b, v) in sv.iter_mut().enumerate().take(nb) {
                        *v += m * hv[(b * sh + r) * w + x];
                    }
                }
                if sa > 0.0 {
                    for (b, v) in sv.iter().enumerate().take(nb) {
                        out[(b * h + y) * w + x] = (v / sa + 0.5).clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
    } else if method == "average" {
        for y in 0..h {
            let y0 = (y as f64 * ry + 1e-8) as usize;
            let y1 = (((y + 1) as f64 * ry - 1e-8).ceil() as usize).clamp(y0 + 1, sh);
            for x in 0..w {
                let x0 = (x as f64 * rx + 1e-8) as usize;
                let x1 = (((x + 1) as f64 * rx - 1e-8).ceil() as usize).clamp(x0 + 1, sw);
                let mut sa = 0.0;
                let mut sv = [0.0f64; 4];
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        let i = sy * sw + sx;
                        let m = src.alpha[i] as f64;
                        sa += m;
                        for (b, v) in sv.iter_mut().enumerate().take(nb) {
                            *v += m * band(b, i);
                        }
                    }
                }
                if sa > 0.0 {
                    for (b, v) in sv.iter().enumerate().take(nb) {
                        out[(b * h + y) * w + x] = (v / sa + 0.5).clamp(0.0, 255.0) as u8;
                    }
                }
            }
        }
    } else {
        for y in 0..h {
            let sy = (((y as f64 + 0.5) * ry) as usize).min(sh - 1);
            for x in 0..w {
                let sx = (((x as f64 + 0.5) * rx) as usize).min(sw - 1);
                for b in 0..nb {
                    out[(b * h + y) * w + x] = band(b, sy * sw + sx) as u8;
                }
            }
        }
    }
    let alpha = out.split_off(src.bands * w * h);
    Pixels { w, h, bands: src.bands, data: out, alpha }
}

/// A raster as a tile pyramid: its tiles `(z, x, y, bytes)`, the bounds (lon/lat), the zoom
/// levels and the tile format name.
pub(crate) struct RasterTiles {
    pub tiles: Vec<(u8, u32, u32, Vec<u8>)>,
    pub bounds: [f64; 4],
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub format: TileFormat,
}

/// Cuts a raster into a tile pyramid as GDAL's MBTiles driver does: reprojected to EPSG:3857
/// (bilinear by default) at the zoom level whose resolution is closest to the source's, with an
/// alpha band when the source is not already in EPSG:3857 (or has one), placed on the tile grid
/// by whole pixels; and the lower zoom levels as overviews computed from it (as `gdaladdo`
/// would), down to `min_zoom` or the level whose single tile covers the raster.
pub(crate) fn make_tiles(ds: &RasterDataset, o: &TileWriteOptions) -> Result<RasterTiles> {
    let format = tile_format(o)?;
    let w = warp_level(ds, o, None)?;
    pyramid(w.level, w.zoom, w.minx, w.maxy, o, format, None)
}

/// The tile format `TILE_FORMAT` asks for (PNG by default).
fn tile_format(o: &TileWriteOptions) -> Result<TileFormat> {
    Ok(match o.tile_format.as_deref().map(str::to_ascii_uppercase).as_deref() {
        None | Some("PNG") | Some("PNG8") => TileFormat::Png,
        Some("JPEG") | Some("JPG") => TileFormat::Jpeg,
        Some("WEBP") => TileFormat::Webp,
        Some(f) => return Err(Error::InvalidArgument(format!("unknown tile format '{f}' (PNG, JPEG, WEBP)"))),
    })
}

/// A raster warped to EPSG:3857 at a zoom level's resolution: its pixels (with an alpha band)
/// and the coordinates of its top left corner.
struct Warped {
    level: Pixels,
    zoom: u8,
    minx: f64,
    maxy: f64,
}

/// The pixel grid of an existing archive's full resolution: its zoom level, and the top left
/// corner of its extent.
#[derive(Clone, Copy)]
struct Grid {
    zoom: u8,
    x0: f64,
    y0: f64,
}

/// Plans the warp of a raster to EPSG:3857 as GDAL's MBTiles driver does (see [`warp_level`]),
/// from its metadata only: the source is read for its pixels only when its latitudes are cut.
fn warp_plan<'a>(ds: &'a RasterDataset, o: &TileWriteOptions, grid: Option<Grid>) -> Result<WarpPlan<'a>> {
    let r = &ds.raster;
    if r.data_type != DataType::U8 || !(1..=4).contains(&r.bands) {
        return Err(Error::Unsupported(format!(
            "tile archives hold 1 to 4 Byte bands (gray, gray and alpha, RGB or RGBA), not {} band(s) of {}: convert the data type first (for example with `terrio translate --ot Byte --scale`)",
            r.bands,
            crate::info::gdal_type_name(r.data_type)
        )));
    }
    let src_srs = ds.crs();
    if src_srs.is_unknown() {
        return Err(Error::InvalidArgument("the raster has no CRS: tiles need one to be reprojected to EPSG:3857".into()));
    }
    let web = SpatialRef::from_epsg(3857)?;

    // Latitudes beyond Web Mercator's are cut off first, as GDAL does.
    const MAX_LAT: f64 = 85.0511287798066;
    let gt = ds.geo_transform();
    let mut src = std::borrow::Cow::Borrowed(ds);
    let (mut clamp_max, mut clamp_min) = (false, false);
    if src_srs.is_geographic() && gt[2] == 0.0 && gt[4] == 0.0 && gt[5] < 0.0 {
        let max_lat = gt[3];
        let min_lat = gt[3] + r.rows as f64 * gt[5];
        clamp_max = max_lat > MAX_LAT;
        clamp_min = min_lat < -MAX_LAT;
        if clamp_max || clamp_min {
            let to = crate::ops::translate::TranslateOptions {
                proj_win: Some((gt[0], max_lat.min(MAX_LAT), gt[0] + r.cols as f64 * gt[1], min_lat.max(-MAX_LAT))),
                ..Default::default()
            };
            src = std::borrow::Cow::Owned(crate::ops::translate::translate(ds, &to)?);
        }
    }
    let (sgt, _, _, mut ext) = crate::ops::warp::suggested_output(&src, &web)?;
    let mut sgt = sgt;
    if clamp_max {
        sgt[3] = MAX_GM;
        ext[3] = MAX_GM;
    }
    if clamp_min {
        ext[1] = -MAX_GM;
    }
    if (clamp_max || clamp_min) && gt[0] + r.cols as f64 * gt[1] == 180.0 {
        ext[2] = MAX_GM;
    }

    // The zoom level, as GDAL picks it.
    let computed = sgt[1];
    let res0 = 2.0 * MAX_GM / 256.0;
    let (mut zoom, mut prev, mut res) = (0u8, 0.0, 0.0);
    while zoom < 25 {
        res = res0 / (1u64 << zoom) as f64;
        if computed > res {
            break;
        }
        prev = res;
        zoom += 1;
    }
    if zoom == 25 {
        return Err(Error::InvalidArgument("could not find an appropriate zoom level".into()));
    }
    if ((computed - res) / res).abs() > 1e-8 {
        match o.zoom_level_strategy.as_str() {
            "LOWER" => zoom = zoom.saturating_sub(1),
            "UPPER" => {}
            _ => {
                if zoom > 0 && prev / computed < computed / res {
                    zoom -= 1;
                }
            }
        }
    }
    if let Some(z) = o.max_zoom {
        zoom = z;
    }
    if let Some(g) = grid {
        zoom = g.zoom;
    }
    let res = res0 / (1u64 << zoom) as f64;
    if let Some(g) = grid {
        // Whole pixels of the grid, covering the raster.
        const EPS: f64 = 1e-6;
        ext[0] = g.x0 + ((ext[0] - g.x0) / res + EPS).floor() * res;
        ext[2] = g.x0 + ((ext[2] - g.x0) / res - EPS).ceil() * res;
        ext[3] = g.y0 - ((g.y0 - ext[3]) / res + EPS).floor() * res;
        ext[1] = g.y0 - ((g.y0 - ext[1]) / res - EPS).ceil() * res;
        sgt[0] = ext[0];
        sgt[3] = ext[3];
    }
    let width = (0.5 + (ext[2] - ext[0]) / res) as usize;
    let height = (0.5 + (ext[3] - ext[1]) / res) as usize;
    if width == 0 || height == 0 {
        return Err(Error::InvalidArgument("the raster is empty at that zoom level".into()));
    }
    // Checked here, so that a level read by window is refused as one warped whole is.
    if (width as u64).checked_mul(height as u64).is_none_or(|pixels| pixels > 1 << 31) {
        return Err(Error::Unsupported(format!(
            "the raster would be {width} x {height} pixels at zoom level {zoom}: choose a lower MAXZOOM"
        )));
    }
    Ok(WarpPlan { src, zoom, sgt, width, height, res })
}

/// Where [`warp_plan`] puts a raster: the source (cut to Web Mercator's latitudes), the zoom
/// level, the output's geotransform and size, and the resolution.
struct WarpPlan<'a> {
    src: std::borrow::Cow<'a, RasterDataset>,
    zoom: u8,
    sgt: [f64; 6],
    width: usize,
    height: usize,
    res: f64,
}

/// Warps a raster to EPSG:3857 as GDAL's MBTiles driver does, at the zoom level it picks (or
/// `MAXZOOM`), or onto an existing archive's pixel grid (as `gdalwarp` into an existing dataset).
fn warp_level(ds: &RasterDataset, o: &TileWriteOptions, grid: Option<Grid>) -> Result<Warped> {
    let WarpPlan { src, zoom, sgt, width, height, res } = warp_plan(ds, o, grid)?;
    let src_srs = ds.crs();
    let web = SpatialRef::from_epsg(3857)?;
    let resampling = crate::ops::Resampling::parse(o.resampling.as_deref().unwrap_or("bilinear"))?;

    // Warp, with an alpha band: the source's, or one added when reprojecting gray or RGB.
    let src_alpha = matches!(src.raster.bands, 2 | 4);
    let data_bands = if src_alpha { src.raster.bands - 1 } else { src.raster.bands };
    let add_alpha = !src_alpha && src_srs.epsg != Some(3857);
    let sn = src.raster.cols * src.raster.rows;
    let values = src.raster.data.to_f64_vec();
    let with_bands = |v: Vec<f64>, bands: usize, t: DataType| {
        let mut d = (*src).clone();
        d.raster.data = RasterData::from_f64_vec(t, v);
        d.raster.data_type = t;
        d.raster.bands = bands;
        d.has_nodata = false;
        d
    };
    let mut wo = crate::ops::warp::WarpOptions {
        dst_srs: Some(web.clone()),
        extent: Some((sgt[0], sgt[3] - height as f64 * res, sgt[0] + width as f64 * res, sgt[3])),
        size: Some((width, height)),
        resampling,
        src_nodata: crate::ops::warp::Nodata::None,
        dst_nodata: crate::ops::warp::Nodata::None,
        output_type: Some(DataType::U8),
        ..Default::default()
    };
    let bytes = |ds: RasterDataset| match ds.raster.data {
        RasterData::U8(v) => v,
        d => d.to_f64_vec().into_iter().map(|v| v.clamp(0.0, 255.0) as u8).collect(),
    };
    // Fully transparent source pixels do not count, as with GDAL's source alpha.
    let color = if src_alpha {
        let a = &values[data_bands * sn..];
        let mut v = values[..data_bands * sn].to_vec();
        for (i, x) in v.iter_mut().enumerate() {
            if a[i % sn] == 0.0 {
                *x = f64::NAN;
            }
        }
        wo.src_nodata = crate::ops::warp::Nodata::Values(vec![f64::NAN]);
        with_bands(v, data_bands, DataType::F32)
    } else {
        with_bands(values[..data_bands * sn].to_vec(), data_bands, DataType::U8)
    };
    let data = bytes(crate::ops::warp::warp(&[&color], &wo)?);
    wo.src_nodata = crate::ops::warp::Nodata::None;
    // On an existing grid, the pixels the warper writes (as `gdalwarp` into a dataset leaves the
    // others alone): those its resampling reaches.
    let written = if grid.is_some() {
        let mut w = wo.clone();
        w.dst_nodata = crate::ops::warp::Nodata::Values(vec![f64::NAN]);
        w.output_type = Some(DataType::F32);
        let cover = crate::ops::warp::warp(&[&with_bands(vec![1.0; sn], 1, DataType::F32)], &w)?;
        Some(cover.raster.data.to_f64_vec().into_iter().map(|v| !v.is_nan()).collect::<Vec<bool>>())
    } else {
        None
    };
    // The alpha band: the source's, or opaque where the source is; GDAL's warper takes it from
    // the nearest source pixel.
    let alpha = if src_alpha || add_alpha {
        wo.resampling = crate::ops::Resampling::Nearest;
        let a = if src_alpha { values[data_bands * sn..].to_vec() } else { vec![255.0; sn] };
        bytes(crate::ops::warp::warp(&[&with_bands(a, 1, DataType::U8)], &wo)?)
    } else {
        vec![255u8; width * height]
    };
    let mut alpha = alpha;
    if let Some(written) = written {
        for (a, w) in alpha.iter_mut().zip(written) {
            if !w {
                *a = 0;
            }
        }
    }
    // Pixels left transparent are not written by the warper.
    let mut data = data;
    if src_alpha || grid.is_some() {
        let n = width * height;
        for (i, x) in data.iter_mut().enumerate() {
            if alpha[i % n] == 0 {
                *x = 0;
            }
        }
    }
    let level = Pixels { w: width, h: height, bands: data_bands, data, alpha };
    Ok(Warped { level, zoom, minx: sgt[0], maxy: sgt[3] })
}

/// The layout of a tile pyramid from a level of `width` x `height` pixels at zoom `zoom` whose
/// top left corner is `(minx, maxy)`: the level, and the lower zoom levels as overviews (as
/// `gdaladdo` would compute them), down to `MINZOOM` or the level whose single tile covers the
/// raster; and the bounds recorded (`bounds` when given: an updated archive keeps its own when
/// its extent does not change).
#[allow(clippy::too_many_arguments)]
fn pyramid_plan(
    width: usize,
    height: usize,
    zoom: u8,
    minx: f64,
    maxy: f64,
    bands: usize,
    o: &TileWriteOptions,
    format: TileFormat,
    bounds: Option<[f64; 4]>,
) -> (super::pyramid::Plan, [f64; 4]) {
    let res0 = 2.0 * MAX_GM / 256.0;
    let res = res0 / (1u64 << zoom) as f64;

    // Overviews: down to the level whose single tile covers the raster (GDAL's PMTiles driver).
    let min_zoom = match o.min_zoom {
        Some(z) => z.min(zoom),
        None => {
            let count = ((width.max(height) as f64 / 256.0).log2().ceil()).max(0.0) as u8;
            zoom.saturating_sub(count)
        }
    };
    let method = super::pyramid::Method::parse(o.resampling.as_deref());
    let (maxx, miny) = (minx + width as f64 * res, maxy - height as f64 * res);

    // The bounds, as GDAL records them.
    let (mut lon0, mut lat0) = mercator_to_lonlat(minx, miny);
    let (mut lon1, mut lat1) = mercator_to_lonlat(maxx, maxy);
    if (lon0 + 180.0).abs() < 1e-7 {
        lon0 = -180.0;
    }
    if (lon1 - 180.0).abs() < 1e-7 {
        lon1 = 180.0;
    }
    let ok_max = mercator_to_lonlat(0.0, MAX_GM).1;
    lat1 = lat1.min(ok_max);
    lat0 = lat0.max(-ok_max);
    if let Some(b) = bounds {
        (lon0, lat0, lon1, lat1) = (b[0], b[1], b[2], b[3]);
    }

    // Overviews are laid out over the extent read back from the bounds, as GDAL builds them on
    // the file it wrote.
    let (bx0, by0) = lonlat_to_mercator(lon0, lat0);
    let (bx1, by1) = lonlat_to_mercator(lon1, lat1);
    let (bx0, by0, bx1, by1) = (bx0, by0.max(-MAX_GM), bx1, by1.min(MAX_GM));
    let mut levels = Vec::new();
    for z in (min_zoom..=zoom).rev() {
        let r = res0 / (1u64 << z) as f64;
        let (ox, oy) = if z == zoom { (minx, maxy) } else { (bx0, by1) };
        let (w, h) = if z == zoom {
            (width, height)
        } else {
            (((bx1 - bx0) / r + 0.5).max(1.0) as usize, ((by1 - by0) / r + 0.5).max(1.0) as usize)
        };
        let sx = ((ox + MAX_GM) / r + 0.5).floor() as i64;
        let sy = ((MAX_GM - oy) / r + 0.5).floor() as i64;
        levels.push(super::pyramid::LevelGeom { z, w, h, sx, sy });
    }
    (super::pyramid::Plan { levels, bands, method, format, quality: o.quality }, [lon0, lat0, lon1, lat1])
}

/// The rows of the full-resolution level a pyramid reads at once.
const BASE_STRIP: usize = 256;

/// A tile pyramid from a level at zoom `zoom` whose top left corner is `(minx, maxy)`: see
/// [`pyramid_plan`]. The level is cut into tiles and the lower levels computed from it a tile row
/// at a time ([`super::pyramid::build`]); the tiles are gathered in memory.
fn pyramid(
    level: Pixels,
    zoom: u8,
    minx: f64,
    maxy: f64,
    o: &TileWriteOptions,
    format: TileFormat,
    bounds: Option<[f64; 4]>,
) -> Result<RasterTiles> {
    let (plan, bounds) = pyramid_plan(level.w, level.h, zoom, minx, maxy, level.bands, o, format, bounds);
    let mut tiles = Vec::new();
    let mut rows =
        super::pyramid::MemoryRows { w: level.w, h: level.h, bands: level.bands, data: &level.data, alpha: &level.alpha, next: 0 };
    super::pyramid::build(&plan, &mut rows, &mut tiles, BASE_STRIP)?;
    let min_zoom = plan.levels.last().map_or(zoom, |l| l.z);
    Ok(RasterTiles { tiles, bounds, min_zoom, max_zoom: zoom, format })
}

// ─── Streamed writing ───────────────────────────────────────────────────────────────────────────

/// The full-resolution level of a raster already on a Web Mercator zoom level's pixel grid,
/// read by window: the pixels the warper would copy, whole pixel for whole pixel. `(dx, dy)` is
/// the source pixel of the level's first pixel; pixels outside the source are those the warper
/// leaves at its initial value (zero), transparent unless the source has no alpha band (as
/// [`warp_level`] adds none to a raster already in EPSG:3857).
#[cfg(not(target_family = "wasm"))]
struct AlignedRows<'a> {
    src: &'a mut crate::LazyRaster,
    w: usize,
    dx: i64,
    dy: i64,
    data_bands: usize,
    src_alpha: bool,
    next: usize,
}

#[cfg(not(target_family = "wasm"))]
impl super::pyramid::BaseRows for AlignedRows<'_> {
    fn next_rows(&mut self, n: usize) -> Result<Vec<Vec<u8>>> {
        let (w, bands) = (self.w, self.data_bands);
        let (sw, sh) = (self.src.width() as i64, self.src.height() as i64);
        let outside_alpha = if self.src_alpha { 0 } else { 255 };
        // Sized from the source's header: allocated fallibly (`super::pyramid::build` checked
        // the strip against the memory the process can use).
        let row_len = crate::dataset::fallible::len(&[bands + 1, w], super::pyramid::ROWS)?;
        let mut rows: Vec<Vec<u8>> = Vec::with_capacity(n);
        for _ in 0..n {
            let mut row = crate::dataset::fallible::filled(row_len, 0u8, super::pyramid::ROWS)?;
            row[bands * w..].fill(outside_alpha);
            rows.push(row);
        }
        // The part of the rows inside the source.
        let (y0, y1) = ((self.next as i64 + self.dy).max(0), (self.next as i64 + n as i64 + self.dy).min(sh));
        let (x0, x1) = (self.dx.max(0), (self.dx + w as i64).min(sw));
        if y0 < y1 && x0 < x1 {
            let (cw, chh) = ((x1 - x0) as usize, (y1 - y0) as usize);
            let window = crate::Window::new(x0 as usize, y0 as usize, cw, chh);
            let read = self.src.read_window(window, &crate::WindowOptions::default())?;
            let RasterData::U8(v) = read.raster.data else {
                return Err(Error::InvalidData("expected Byte pixels".into()));
            };
            let n_src = cw * chh;
            let ox = (x0 - self.dx) as usize;
            for r in 0..chh {
                let row = &mut rows[(y0 - self.dy) as usize - self.next + r];
                for x in 0..cw {
                    let i = r * cw + x;
                    let a = if self.src_alpha { v[bands * n_src + i] } else { 255 };
                    let o = ox + x;
                    row[bands * w + o] = a;
                    // Fully transparent source pixels are not copied, as with GDAL's source alpha.
                    if a != 0 || !self.src_alpha {
                        for b in 0..bands {
                            row[b * w + o] = v[b * n_src + i];
                        }
                    }
                }
            }
        }
        self.next += n;
        Ok(rows)
    }

    /// While `n` rows are read: the source's window (one byte per sample, as `warp_plan` takes
    /// Byte rasters only), its chunks decoded and read on each thread (`decode_chunks` decodes
    /// four per thread at a time), and the compressed window a source that is not a local file
    /// fetches first.
    fn held(&self, n: usize) -> (u64, u64) {
        let (bands, cols) = (self.src.band_count() as u64, self.w.min(self.src.width()) as u64);
        let window = n as u64 * cols * bands;
        let (bw, bh) = self.src.block_size().unwrap_or((self.src.width(), n));
        let (bw, bh) = (bw.max(1), bh.max(1));
        let chunks = (n.div_ceil(bh) as u64 + 1) * (self.w.div_ceil(bw) as u64 + 1);
        let decoding = chunks.min(4 * crate::par::threads() as u64) * (bw * bh) as u64 * bands * 2;
        let remote = if self.src.input_paths().is_ok_and(|p| !p.is_empty()) { 0 } else { window };
        (0, window + decoding + remote)
    }
}

/// When a raster is already on the pixel grid [`warp_level`] would warp it to (EPSG:3857 at the
/// zoom level's resolution, shifted by whole pixels), the level's size, zoom and corner, and the
/// source pixel of its first pixel: the warp copies pixels, so the level can be read by window.
#[cfg(not(target_family = "wasm"))]
fn aligned_level(header: &RasterDataset, o: &TileWriteOptions) -> Result<Option<(WarpPlanned, i64, i64)>> {
    let gt = header.geo_transform();
    if header.crs().epsg != Some(3857) || !header.gcps.is_empty() || gt[2] != 0.0 || gt[4] != 0.0 || gt[1] <= 0.0 || gt[5] >= 0.0 {
        return Ok(None);
    }
    let p = warp_plan(header, o, None)?;
    let dst = [p.sgt[0], p.res, 0.0, p.sgt[3], 0.0, -p.res];
    // Coordinates near 2e7 m are only good to about 4e-9 m, more than 1e-8 of a pixel at zoom 19
    // and above, so the tolerance is a millionth of a pixel.
    let near_int = |v: f64| (v - v.round()).abs() < 1e-6;
    let translation = (gt[1] - dst[1]).abs() < 1e-12 * gt[1].abs()
        && (gt[5] - dst[5]).abs() < 1e-12 * gt[5].abs()
        && near_int((dst[0] - gt[0]) / gt[1])
        && near_int((dst[3] - gt[3]) / gt[5]);
    if !translation {
        return Ok(None);
    }
    let (dx, dy) = (((dst[0] - gt[0]) / gt[1]).round() as i64, ((dst[3] - gt[3]) / gt[5]).round() as i64);
    Ok(Some((WarpPlanned { zoom: p.zoom, minx: p.sgt[0], maxy: p.sgt[3], width: p.width, height: p.height }, dx, dy)))
}

/// The level [`warp_plan`] plans, without its source.
#[cfg(not(target_family = "wasm"))]
struct WarpPlanned {
    zoom: u8,
    minx: f64,
    maxy: f64,
    width: usize,
    height: usize,
}

/// A tile pyramid being written: built a tile row at a time from rows of its full-resolution
/// level, its tiles given to `sink`. Returns the bounds and the zoom levels.
#[cfg(not(target_family = "wasm"))]
pub(crate) struct Streamed {
    pub bounds: [f64; 4],
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub format: TileFormat,
}

/// Builds the tile pyramid of a raster (as [`make_tiles`] does) without holding its levels whole,
/// giving its tiles to `sink`. A raster read by window that is already on the pixel grid of its
/// zoom level (in EPSG:3857, at the level's resolution: a raster `terrio warp` wrote with `-tap`
/// and the zoom level's `-tr`) is read a strip at a time, so that it is never held whole; others
/// are read whole and warped first, as [`make_tiles`] warps them.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn make_tiles_streamed(
    src: &mut crate::LazyRaster,
    o: &TileWriteOptions,
    sink: &mut dyn super::pyramid::TileSink,
) -> Result<Streamed> {
    let format = tile_format(o)?;
    crate::ops::Resampling::parse(o.resampling.as_deref().unwrap_or("bilinear"))?;
    let header = src.header();
    if let Some((p, dx, dy)) = aligned_level(&header, o)? {
        let src_alpha = matches!(header.raster.bands, 2 | 4);
        let data_bands = if src_alpha { header.raster.bands - 1 } else { header.raster.bands };
        let (plan, bounds) = pyramid_plan(p.width, p.height, p.zoom, p.minx, p.maxy, data_bands, o, format, None);
        let mut rows = AlignedRows { src, w: p.width, dx, dy, data_bands, src_alpha, next: 0 };
        super::pyramid::build(&plan, &mut rows, sink, BASE_STRIP)?;
        return Ok(Streamed { bounds, min_zoom: plan.levels.last().map_or(p.zoom, |l| l.z), max_zoom: p.zoom, format });
    }
    let ds = src.read()?;
    make_tiles_from_dataset(&ds, o, sink)
}

/// [`make_tiles_streamed`] for a raster in memory: warped whole, then its pyramid built a tile
/// row at a time.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn make_tiles_from_dataset(
    ds: &RasterDataset,
    o: &TileWriteOptions,
    sink: &mut dyn super::pyramid::TileSink,
) -> Result<Streamed> {
    let format = tile_format(o)?;
    let w = warp_level(ds, o, None)?;
    let l = &w.level;
    let (plan, bounds) = pyramid_plan(l.w, l.h, w.zoom, w.minx, w.maxy, l.bands, o, format, None);
    let mut rows = super::pyramid::MemoryRows { w: l.w, h: l.h, bands: l.bands, data: &l.data, alpha: &l.alpha, next: 0 };
    super::pyramid::build(&plan, &mut rows, sink, BASE_STRIP)?;
    Ok(Streamed { bounds, min_zoom: plan.levels.last().map_or(w.zoom, |l| l.z), max_zoom: w.zoom, format })
}

/// Adds a raster to an archive of image tiles, as `gdalwarp` into an existing MBTiles file then
/// `gdaladdo` do: the raster warped onto the pixel grid of the archive's full resolution and laid
/// over it (where it is not transparent), the archive's extent grown to hold it, and the lower
/// zoom levels computed again from the result, down to the archive's lowest (or `MINZOOM`). The
/// tiles keep the archive's format.
pub(crate) fn update_tiles(archive: TileArchive, ds: &RasterDataset, o: &TileWriteOptions) -> Result<RasterTiles> {
    let format = archive.format();
    if !matches!(format, TileFormat::Png | TileFormat::Jpeg | TileFormat::Webp) {
        return Err(Error::Unsupported(format!(
            "tiles of format '{}' cannot be written: only PNG, JPEG and WebP archives can be updated",
            format.name()
        )));
    }
    // The tiles keep the archive's format: another one asked for is an error, not ignored.
    if o.tile_format.is_some() && tile_format(o)? != format {
        return Err(Error::InvalidArgument(format!(
            "the archive's tiles are {}: an update keeps their format, not {}",
            format.name(),
            o.tile_format.as_deref().unwrap_or_default()
        )));
    }
    let (min_zoom, _) = archive.zoom_range();
    let archive_bounds = archive.bounds();
    let mut tr = TileRaster::new(archive, None)?;
    if tr.tile_size != 256 {
        return Err(Error::Unsupported(format!("archives of {}-pixel tiles cannot be updated (256-pixel tiles only)", tr.tile_size)));
    }
    let zoom = tr.levels[0].zoom;
    let res = resolution(zoom, 256);
    let (x0, y0) = (tr.extent[0], tr.extent[3]);
    let (w, h) = (tr.width(), tr.height());
    let new = warp_level(ds, o, Some(Grid { zoom, x0, y0 }))?;
    // The new raster's place relative to the archive's full resolution, and the union of both.
    let nx = ((new.minx - x0) / res).round() as i64;
    let ny = ((y0 - new.maxy) / res).round() as i64;
    let (nw, nh) = (new.level.w as i64, new.level.h as i64);
    let (cx0, cy0) = (nx.min(0), ny.min(0));
    let (cx1, cy1) = (nx.saturating_add(nw).max(w as i64), ny.saturating_add(nh).max(h as i64));
    // The archive and the raster are held whole at full resolution (bands, alpha and a copy of
    // the archive's pixels): 2^28 pixels at most (2^26 in WebAssembly).
    const MAX_PIXELS: u64 = if cfg!(target_family = "wasm") { 1 << 26 } else { 1 << 28 };
    let (sw, sh) = (cx1.saturating_sub(cx0), cy1.saturating_sub(cy0));
    let fits = u64::try_from(sw).ok().zip(u64::try_from(sh).ok()).and_then(|(a, b)| a.checked_mul(b)).is_some_and(|n| n <= MAX_PIXELS);
    if !fits {
        return Err(Error::Unsupported(format!(
            "the updated archive would be {sw} x {sh} pixels at zoom level {zoom} (at most {MAX_PIXELS} are updated in memory)"
        )));
    }
    // Both fit in `usize`: their product is at most `MAX_PIXELS`.
    let (cw, ch) = (sw as usize, sh as usize);
    let existing_alpha = matches!(tr.bands, 2 | 4);
    let existing_data = if existing_alpha { tr.bands - 1 } else { tr.bands };
    let bands = existing_data.max(new.level.bands);
    let n = cw * ch;
    let mut data = vec![0u8; bands * n];
    let mut alpha = vec![0u8; n];
    let all: Vec<usize> = (1..=tr.bands).collect();
    let old = match tr.read(0, crate::Window::full(w, h), &all)? {
        RasterData::U8(v) => v,
        other => other.to_f64_vec().into_iter().map(|v| v as u8).collect(),
    };
    let (ox, oy) = ((-cx0) as usize, (-cy0) as usize);
    for y in 0..h {
        for x in 0..w {
            let (i, c) = (y * w + x, (y + oy) * cw + x + ox);
            alpha[c] = if existing_alpha { old[existing_data * w * h + i] } else { 255 };
            for b in 0..bands {
                data[b * n + c] = old[b.min(existing_data - 1) * w * h + i];
            }
        }
    }
    let l = &new.level;
    let (px, py) = ((nx - cx0) as usize, (ny - cy0) as usize);
    let ln = l.w * l.h;
    for y in 0..l.h {
        for x in 0..l.w {
            let i = y * l.w + x;
            if l.alpha[i] == 0 {
                continue;
            }
            let c = (y + py) * cw + x + px;
            alpha[c] = l.alpha[i];
            for b in 0..bands {
                data[b * n + c] = l.data[b.min(l.bands - 1) * ln + i];
            }
        }
    }
    let canvas = Pixels { w: cw, h: ch, bands, data, alpha };
    let mut o = o.clone();
    o.min_zoom = Some(o.min_zoom.unwrap_or(min_zoom).min(zoom));
    let same = cx0 == 0 && cy0 == 0 && cw == w && ch == h;
    pyramid(canvas, zoom, x0 + cx0 as f64 * res, y0 - cy0 as f64 * res, &o, format, archive_bounds.filter(|_| same))
}

/// Encodes a 256-pixel tile of `bands` data bands and an alpha band, pixel-interleaved (the alpha
/// band is dropped from JPEG tiles).
pub(crate) fn encode_tile(px: &[u8], bands: usize, format: TileFormat, quality: u8) -> Result<Vec<u8>> {
    let ch = bands + 1;
    let (channels, samples): (usize, std::borrow::Cow<[u8]>) = if format == TileFormat::Jpeg {
        (bands, px.chunks(ch).flat_map(|p| p[..ch - 1].iter().copied()).collect::<Vec<u8>>().into())
    } else {
        (ch, px.into())
    };
    let t = 256;
    match format {
        TileFormat::Jpeg => encode_jpeg(t, t, channels, &samples, quality),
        TileFormat::Webp => encode_webp(t, t, channels, &samples),
        _ => encode_png(t, t, channels, &samples),
    }
}

/// Cuts a level into 256-pixel tiles of zoom `z`, its first pixel at global pixel `(sx, sy)`:
/// the in-memory reference the streamed pyramid ([`super::pyramid`]) is checked against.
/// Fully transparent tiles are left out. Every tile has an alpha band (but JPEG ones), so that
/// readers that take the band count of a tile (GDAL's PMTiles driver) see the same bands.
#[cfg(test)]
fn cut_tiles(l: &Pixels, z: u8, sx: i64, sy: i64, format: TileFormat, quality: u8) -> Result<Tiles> {
    const TS: i64 = 256;
    let nmax = (1i64 << z) - 1;
    let (tx0, tx1) = (sx.div_euclid(TS).max(0), (sx + l.w as i64 - 1).div_euclid(TS).min(nmax));
    let (ty0, ty1) = (sy.div_euclid(TS).max(0), (sy + l.h as i64 - 1).div_euclid(TS).min(nmax));
    let n = l.w * l.h;
    let mut out = Vec::new();
    for ty in ty0..=ty1 {
        for tx in tx0..=tx1 {
            let ch = l.bands + 1;
            let mut px = vec![0u8; (TS * TS) as usize * ch];
            let mut any = false;
            for y in 0..TS {
                let gy = ty * TS + y - sy;
                for x in 0..TS {
                    let gx = tx * TS + x - sx;
                    let o = (y * TS + x) as usize * ch;
                    if gy < 0 || gy >= l.h as i64 || gx < 0 || gx >= l.w as i64 {
                        continue;
                    }
                    let i = gy as usize * l.w + gx as usize;
                    let a = l.alpha[i];
                    any |= a != 0;
                    for b in 0..l.bands {
                        px[o + b] = l.data[b * n + i];
                    }
                    px[o + l.bands] = a;
                }
            }
            if !any {
                continue;
            }
            out.push((z, tx as u32, ty as u32, encode_tile(&px, l.bands, format, quality)?));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pyramid as terrio computed it before it streamed: every level held whole.
    fn pyramid_reference(
        mut level: Pixels,
        zoom: u8,
        minx: f64,
        maxy: f64,
        o: &TileWriteOptions,
        format: TileFormat,
        bounds: Option<[f64; 4]>,
    ) -> Result<RasterTiles> {
        let res0 = 2.0 * MAX_GM / 256.0;
        let (plan, bounds) = pyramid_plan(level.w, level.h, zoom, minx, maxy, level.bands, o, format, bounds);
        let method = o.resampling.as_deref().unwrap_or("bilinear").to_ascii_lowercase();
        let method = match method.as_str() {
            "near" | "nearest" => "nearest",
            "average" => "average",
            "cubic" | "cubicspline" | "lanczos" => "cubic",
            _ => "bilinear",
        };
        let (bx0, by0) = lonlat_to_mercator(bounds[0], bounds[1]);
        let (bx1, by1) = lonlat_to_mercator(bounds[2], bounds[3]);
        let (bx0, by0, bx1, by1) = (bx0, by0.max(-MAX_GM), bx1, by1.min(MAX_GM));
        let min_zoom = plan.levels.last().unwrap().z;
        let mut tiles = Vec::new();
        for z in (min_zoom..=zoom).rev() {
            let r = res0 / (1u64 << z) as f64;
            let (ox, oy) = if z == zoom { (minx, maxy) } else { (bx0, by1) };
            if z != zoom {
                let w = ((bx1 - bx0) / r + 0.5).max(1.0) as usize;
                let h = ((by1 - by0) / r + 0.5).max(1.0) as usize;
                level = downsample(&level, w, h, method);
            }
            let sx = ((ox + MAX_GM) / r + 0.5).floor() as i64;
            let sy = ((MAX_GM - oy) / r + 0.5).floor() as i64;
            tiles.extend(cut_tiles(&level, z, sx, sy, format, o.quality)?);
        }
        Ok(RasterTiles { tiles, bounds, min_zoom, max_zoom: zoom, format })
    }

    #[test]
    fn rasters_round_trip_through_tiles() {
        // A 512 x 256 RGB raster in EPSG:3857 at zoom 10's resolution, on the tile grid.
        let res = resolution(10, 256);
        let (w, h) = (512, 256);
        let mut v = Vec::new();
        for b in 0..3 {
            for y in 0..h {
                for x in 0..w {
                    v.push(((x + y * 2 + b * 50) % 256) as f64);
                }
            }
        }
        let crs = SpatialRef::from_epsg(3857).unwrap().to_crs_info();
        let gt = [-MAX_GM + 300.0 * 256.0 * res, res, 0.0, MAX_GM - 400.0 * 256.0 * res, 0.0, -res];
        let raster =
            crate::drivers::common::make_raster(w, h, 3, RasterData::from_f64_vec(DataType::U8, v.clone()), gt, crs, None).unwrap();
        let ds = RasterDataset::with_driver(raster, crate::formats::RasterDriver::GTiff, false);
        let o = TileWriteOptions { resampling: Some("near".into()), ..Default::default() };
        let rt = make_tiles(&ds, &o).unwrap();
        assert_eq!(rt.max_zoom, 10);
        assert_eq!(rt.min_zoom, 9);
        let bytes = super::super::write_raster_archive(&rt, &o, "t", super::super::ArchiveKind::PMTiles).unwrap();
        let mut tr = TileRaster::new(TileArchive::from_bytes(bytes).unwrap(), None).unwrap();
        assert_eq!((tr.width(), tr.height(), tr.bands), (512, 256, 4));
        assert_eq!(tr.overviews(), vec![(256, 128)]);
        let back = tr.read_all().unwrap();
        assert_eq!(back.raster.data.to_f64_vec()[..v.len()], v[..]);
        let gtb = back.geo_transform();
        assert!((gtb[0] - gt[0]).abs() < 1e-6 && (gtb[3] - gt[3]).abs() < 1e-6);
    }

    /// A level of `bands` data bands with an alpha band: values that change at every pixel, and
    /// transparent and half-transparent areas (an edge, holes), so that overviews mix them.
    fn test_level(w: usize, h: usize, bands: usize, seed: u64) -> Pixels {
        let mut state = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as u32
        };
        let n = w * h;
        let mut data = vec![0u8; bands * n];
        let mut alpha = vec![0u8; n];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                // A transparent corner, a band of partial transparency, random holes.
                let a = if x + y < w / 3 {
                    0
                } else if (y * 7 / h.max(1)) == 3 {
                    (next() % 256) as u8
                } else if next() % 17 == 0 {
                    0
                } else {
                    255
                };
                alpha[i] = a;
                for b in 0..bands {
                    data[b * n + i] = if a == 0 { 0 } else { ((x * 3 + y * 5 + b * 70) as u32 % 200 + next() % 56) as u8 };
                }
            }
        }
        Pixels { w, h, bands, data, alpha }
    }

    fn sorted(mut t: Vec<(u8, u32, u32, Vec<u8>)>) -> Vec<(u8, u32, u32, Vec<u8>)> {
        t.sort_by_key(|t| (t.0, t.1, t.2));
        t
    }

    /// The pyramid built a tile row at a time is the one built from whole levels, tile for tile
    /// and byte for byte: every overview method, tile format, band count, level size and
    /// position on the tile grid (tiles straddling the level's edges, levels whose size is not a
    /// power of two, so that overviews are not exact halves).
    #[test]
    fn streamed_pyramids_are_the_in_memory_ones() {
        let res0 = 2.0 * MAX_GM / 256.0;
        let cases: &[(usize, usize, u8, i64, i64)] = &[
            (700, 530, 10, 300 * 256 + 37, 400 * 256 + 211),
            (513, 1029, 11, 1000 * 256, 900 * 256 + 255),
            (300, 40, 9, 100 * 256 + 250, 200 * 256 + 3),
            (1, 1, 8, 77 * 256 + 5, 66 * 256 + 9),
            (1100, 900, 12, 2000 * 256 + 129, 1500 * 256 + 64),
        ];
        let mut checked = 0;
        for (k, &(w, h, zoom, gx, gy)) in cases.iter().enumerate() {
            let r = res0 / (1u64 << zoom) as f64;
            let (minx, maxy) = (-MAX_GM + gx as f64 * r, MAX_GM - gy as f64 * r);
            for bands in [1, 3] {
                for method in ["near", "average", "bilinear", "cubic"] {
                    for (format, min_zoom) in
                        [(TileFormat::Png, None), (TileFormat::Jpeg, None), (TileFormat::Webp, Some(zoom.saturating_sub(1)))]
                    {
                        // The slower encoders on the smaller levels only.
                        if format != TileFormat::Png && w * h > 400_000 {
                            continue;
                        }
                        let o = TileWriteOptions { resampling: Some(method.into()), min_zoom, ..Default::default() };
                        let level = test_level(w, h, bands, k as u64 * 31 + bands as u64);
                        let copy = Pixels { w, h, bands, data: level.data.clone(), alpha: level.alpha.clone() };
                        let a = pyramid_reference(level, zoom, minx, maxy, &o, format, None).unwrap();
                        let b = pyramid(copy, zoom, minx, maxy, &o, format, None).unwrap();
                        assert_eq!((a.min_zoom, a.max_zoom, a.bounds), (b.min_zoom, b.max_zoom, b.bounds));
                        assert!(!a.tiles.is_empty());
                        let (a, b) = (sorted(a.tiles), sorted(b.tiles));
                        assert_eq!(a.len(), b.len(), "{w}x{h} z{zoom} {bands} {method} {format:?}");
                        for (ta, tb) in a.iter().zip(&b) {
                            assert!(ta == tb, "tile {:?} differs: {w}x{h} z{zoom} {bands} band(s) {method} {format:?}", (ta.0, ta.1, ta.2));
                        }
                        checked += 1;
                    }
                }
            }
        }
        assert!(checked > 60);
    }

    /// A raster in EPSG:3857 on its zoom level's grid, written as a GeoTIFF.
    fn aligned_raster(dir: &std::path::Path, name: &str, bands: usize, zoom: u8) -> (String, RasterDataset) {
        let res = resolution(zoom, 256);
        let (w, h) = (900, 520);
        let level = test_level(w, h, bands.clamp(1, 3), bands as u64);
        let data_bands = if matches!(bands, 2 | 4) { bands - 1 } else { bands };
        let n = w * h;
        let mut v: Vec<f64> = Vec::with_capacity(bands * n);
        for b in 0..data_bands {
            v.extend(level.data[b.min(level.bands - 1) * n..(b.min(level.bands - 1) + 1) * n].iter().map(|x| *x as f64));
        }
        if data_bands < bands {
            v.extend(level.alpha.iter().map(|x| *x as f64));
        }
        let crs = SpatialRef::from_epsg(3857).unwrap().to_crs_info();
        let gt = [-MAX_GM + 1234.0 * res, res, 0.0, MAX_GM - 2345.0 * res, 0.0, -res];
        let raster = crate::drivers::common::make_raster(w, h, bands, RasterData::from_f64_vec(DataType::U8, v), gt, crs, None).unwrap();
        let ds = RasterDataset::with_driver(raster, crate::formats::RasterDriver::GTiff, false);
        let path = dir.join(name).to_str().unwrap().to_string();
        let o = crate::RasterWriteOptions { tile_size: Some(256), compression: Some("DEFLATE".into()), ..Default::default() };
        crate::write_raster_dataset(&ds, &path, &o).unwrap();
        (path.clone(), crate::open_raster(&path).unwrap())
    }

    /// A raster already on its zoom level's pixel grid is read a strip at a time, and gives the
    /// tiles (and the archive, byte for byte) the in-memory writer gives: gray, gray and alpha,
    /// RGB and RGBA, every overview method, PMTiles and MBTiles.
    #[test]
    fn aligned_rasters_stream_as_the_in_memory_writer_writes_them() {
        let dir = tempfile::tempdir().unwrap();
        for bands in 1..=4 {
            let (path, ds) = aligned_raster(dir.path(), &format!("a{bands}.tif"), bands, 13);
            for method in ["near", "average", "bilinear", "cubic"] {
                let o = TileWriteOptions { resampling: Some(method.into()), max_zoom: Some(13), ..Default::default() };
                // The previous writer: the level warped whole, the pyramid of whole levels.
                let w = warp_level(&ds, &o, None).unwrap();
                let reference = pyramid_reference(w.level, w.zoom, w.minx, w.maxy, &o, TileFormat::Png, None).unwrap();
                let mut lazy = crate::open_raster_lazy(&path).unwrap();
                assert!(aligned_level(&lazy.header(), &o).unwrap().is_some(), "{bands} band(s) aligned");
                let mut tiles = Vec::new();
                let s = make_tiles_streamed(&mut lazy, &o, &mut tiles).unwrap();
                assert_eq!((s.min_zoom, s.max_zoom, s.bounds), (reference.min_zoom, reference.max_zoom, reference.bounds));
                assert!(sorted(tiles) == sorted(reference.tiles.clone()), "{bands} band(s) {method}");
                // Archives: written from the raster read by window, and from the raster in memory.
                let options = crate::RasterWriteOptions {
                    creation_options: vec![("RESAMPLING".into(), method.into()), ("MAXZOOM".into(), "13".into())],
                    ..Default::default()
                };
                for (ext, kind) in [("pmtiles", super::super::ArchiveKind::PMTiles), ("mbtiles", super::super::ArchiveKind::MBTiles)] {
                    let out = dir.path().join(format!("o{bands}{method}.{ext}")).to_str().unwrap().to_string();
                    crate::open_raster_lazy(&path).unwrap().write(&out, &options).unwrap();
                    let expected = super::super::write_raster_archive(&reference, &o, &format!("o{bands}{method}"), kind).unwrap();
                    assert!(std::fs::read(&out).unwrap() == expected, "{bands} band(s) {method} {ext}");
                    crate::write_raster_dataset(&ds, &out, &options).unwrap();
                    assert!(std::fs::read(&out).unwrap() == expected, "{bands} band(s) {method} {ext}, in memory");
                }
            }
        }
        // No spill file is left behind.
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(left.is_empty());
    }

    /// Rasters on the grids of zoom 19 and 20 are still read a strip at a time, and give the
    /// in-memory tiles: at these zooms the rounding of coordinates near 2e7 m is about the size
    /// of the alignment tolerance.
    #[test]
    fn aligned_rasters_at_high_zooms_stream_as_the_in_memory_writer_writes_them() {
        let dir = tempfile::tempdir().unwrap();
        for zoom in [19, 20] {
            let (path, ds) = aligned_raster(dir.path(), &format!("z{zoom}.tif"), 3, zoom);
            let o = TileWriteOptions { resampling: Some("near".into()), max_zoom: Some(zoom), ..Default::default() };
            let mut lazy = crate::open_raster_lazy(&path).unwrap();
            assert!(aligned_level(&lazy.header(), &o).unwrap().is_some(), "zoom {zoom} aligned");
            let w = warp_level(&ds, &o, None).unwrap();
            let reference = pyramid_reference(w.level, w.zoom, w.minx, w.maxy, &o, TileFormat::Png, None).unwrap();
            let mut tiles = Vec::new();
            make_tiles_streamed(&mut lazy, &o, &mut tiles).unwrap();
            assert!(sorted(tiles) == sorted(reference.tiles), "zoom {zoom}");
        }
    }

    /// A raster that is not on a zoom level's grid (another resolution, another CRS) is warped as
    /// before, then tiled a tile row at a time: the same archive.
    #[test]
    fn other_rasters_are_warped_then_streamed() {
        let dir = tempfile::tempdir().unwrap();
        let (path, ds) = aligned_raster(dir.path(), "a.tif", 4, 13);
        // Zoom 12 is not the raster's resolution: it is resampled.
        let o = TileWriteOptions { resampling: Some("bilinear".into()), max_zoom: Some(12), ..Default::default() };
        let mut lazy = crate::open_raster_lazy(&path).unwrap();
        assert!(aligned_level(&lazy.header(), &o).unwrap().is_none());
        let w = warp_level(&ds, &o, None).unwrap();
        let reference = pyramid_reference(w.level, w.zoom, w.minx, w.maxy, &o, TileFormat::Png, None).unwrap();
        let mut tiles = Vec::new();
        make_tiles_streamed(&mut lazy, &o, &mut tiles).unwrap();
        assert!(sorted(tiles) == sorted(reference.tiles));
        // In another CRS.
        let mut utm = ds.clone();
        utm.raster.crs = SpatialRef::from_epsg(32618).unwrap().to_crs_info();
        utm.set_geo_transform([500000.0, 10.0, 0.0, 4500000.0, 0.0, -10.0]);
        let o = TileWriteOptions::default();
        assert!(aligned_level(&utm, &o).unwrap().is_none());
        let w = warp_level(&utm, &o, None).unwrap();
        let reference = pyramid_reference(w.level, w.zoom, w.minx, w.maxy, &o, TileFormat::Png, None).unwrap();
        let mut tiles = Vec::new();
        make_tiles_from_dataset(&utm, &o, &mut tiles).unwrap();
        assert!(sorted(tiles) == sorted(reference.tiles));
    }
}

#[cfg(all(test, not(target_family = "wasm")))]
mod memory_tests;

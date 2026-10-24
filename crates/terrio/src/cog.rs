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
    /// GDAL's external mask, the `.msk` sidecar of a local file without an internal mask: a TIFF
    /// whose first band is the per-dataset mask, with overviews of its own.
    sidecar_mask: Option<Box<LazyRaster>>,
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
    /// A raster another driver reads by window: its header (without pixels) and reader.
    Source(RasterDataset, Box<dyn WindowSource>),
}

/// A raster read by window, at full resolution or from an overview, by a driver other than the
/// GeoTIFF one (STAC mosaics, Zarr pyramids).
pub(crate) trait WindowSource: Send + Sync {
    /// The sizes `(width, height)` of the overviews, largest first.
    fn overviews(&self) -> Vec<(usize, usize)>;
    /// Whether the source supplies an explicit per-dataset validity mask.
    fn has_mask(&self) -> bool {
        false
    }
    /// Read the validity mask: 0 is invalid and 255 valid; absent means all valid.
    fn read_mask(&mut self, _level: usize, _window: Window) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    /// The block size `(width, height)` reads are best aligned to, when the format has one.
    fn block_size(&self) -> Option<(usize, usize)> {
        None
    }
    /// The source as [`LazyRaster::write`] reads it a strip of rows at a time, several strips at
    /// once; `None` when it is read whole.
    fn streamed(&self) -> Option<&dyn StreamedSource> {
        None
    }
    /// Reads a window of some bands (numbered from 1) of a level (0 for full resolution, `i + 1`
    /// for overview `i`), band after band.
    fn read(&mut self, level: usize, window: Window, bands: &[usize]) -> Result<RasterData>;
}

/// A [`WindowSource`] whose full-resolution rows can be read from several threads at once.
pub(crate) trait StreamedSource: Sync {
    /// The local files the source reads: an output replacing one of them is written from the
    /// source read whole.
    fn inputs(&self) -> Vec<String>;
    /// Reads a full-resolution window of some bands (numbered from 1), band after band.
    fn read_rows(&self, window: Window, bands: &[usize]) -> Result<RasterData>;
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
        let mut r = Self::open_without_sidecars(path)?;
        r.find_sidecar_mask(path);
        Ok(r)
    }

    /// Attaches the `.msk` sidecar of a local file (`dem.tif.msk`, as GDAL writes it when
    /// `GDAL_TIFF_INTERNAL_MASK=NO` or for formats without internal masks) when the raster has
    /// no internal mask and the sidecar is a per-dataset mask of its size.
    fn find_sidecar_mask(&mut self, path: &str) {
        if self.has_mask() || !Location::parse(path).is_local() {
            return;
        }
        let msk = format!("{path}.msk");
        if !std::path::Path::new(&msk).is_file() {
            return;
        }
        // Per-dataset (INTERNAL_MASK_FLAGS_1=2, GMF_PER_DATASET): one mask for every band.
        let per_dataset = crate::geotiff::read_geo_tags(std::path::Path::new(&msk)).is_none_or(|t| {
            t.metadata
                .iter()
                .find(|(k, _)| k == "INTERNAL_MASK_FLAGS_1")
                .is_none_or(|(_, v)| v.trim().parse::<u32>().is_ok_and(|f| f & 2 != 0))
        });
        if let Ok(Some(m)) = Self::open_tiff(&msk)
            && per_dataset
            && (m.width(), m.height()) == (self.width(), self.height())
        {
            self.sidecar_mask = Some(Box::new(m));
        }
    }

    fn open_without_sidecars(path: &str) -> Result<Self> {
        if crate::formats::extension(path).as_deref() == Some("vrt") {
            return crate::ops::vrt::open_virtual_raster(path);
        }
        if let Some(result) = crate::stac::open_lazy(path) {
            return result;
        }
        if let Some(result) = crate::drivers::open_lazy(path) {
            return result;
        }
        #[cfg(feature = "tiles")]
        if let Some(t) = crate::tiles::raster::open_lazy(path)? {
            return Ok(Self::from_tiles(t));
        }
        match Self::open_tiff(path)? {
            Some(r) => Ok(r),
            None => crate::open_raster(path).map(|ds| Self { inner: Inner::Memory(ds), sidecar_mask: None }),
        }
    }

    /// Opens a GeoTIFF in place, reading only its headers. Returns `None` when the file is not a
    /// TIFF (having read only its first bytes).
    pub fn open_tiff(path: &str) -> Result<Option<Self>> {
        let loc = Location::parse(path);
        let (mut source, local, file) = match &loc {
            #[cfg(remote)]
            Location::Url(_) | Location::S3 { .. } => {
                let remote = loc.remote().ok_or_else(|| Error::InvalidArgument(format!("'{path}' is not remote")))?;
                (Source::Http(crate::http::RangeReader::open(&remote)?), None, None)
            }
            Location::Local(p) if !p.is_file() => return Ok(None),
            Location::Local(p) => (Source::File(BufReader::new(std::fs::File::open(p)?)), None, Some(p.clone())),
            _ => {
                let local = vfs::resolve(path)?;
                let file = local.path().to_path_buf();
                (Source::File(BufReader::new(std::fs::File::open(&file)?)), Some(local), Some(file))
            }
        };
        if !is_tiff(&mut source) {
            return Ok(None);
        }
        let mut t = TiffRaster::new(source, local)?;
        if let Location::Local(p) = &loc {
            t.tags.apply_sidecar_crs(p);
        }
        t.tags.warn_unreadable(path);
        t.file = file;
        Ok(Some(Self { inner: Inner::Tiff(Box::new(t)), sidecar_mask: None }))
    }

    /// Opens a GeoTIFF held in memory, reading only its headers now. Returns `None` when the bytes
    /// are not a TIFF.
    pub fn from_tiff_bytes(bytes: Vec<u8>) -> Result<Option<Self>> {
        let mut source = Source::Memory(std::io::Cursor::new(SharedBytes(std::sync::Arc::new(bytes))));
        if !is_tiff(&mut source) {
            return Ok(None);
        }
        let t = TiffRaster::new(source, None)?;
        t.tags.warn_unreadable("a GeoTIFF in memory");
        Ok(Some(Self { inner: Inner::Tiff(Box::new(t)), sidecar_mask: None }))
    }

    /// Opens a remote GeoTIFF whose bytes the caller fetches (see [`crate::sparse`]), reading
    /// only its headers. Reads fail until the bytes they need are fetched, and are then run
    /// again. Returns `None` when the file is not a TIFF.
    pub fn from_sparse(file: crate::sparse::SparseFile) -> Result<Option<Self>> {
        let mut source = Source::Sparse(file);
        source.seek(SeekFrom::Start(0))?;
        let mut magic = [0u8; 4];
        source.read_exact(&mut magic)?;
        if !matches!(&magic, b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+") {
            return Ok(None);
        }
        source.seek(SeekFrom::Start(0))?;
        let t = TiffRaster::new(source, None)?;
        t.tags.warn_unreadable("a remote GeoTIFF");
        Ok(Some(Self { inner: Inner::Tiff(Box::new(t)), sidecar_mask: None }))
    }

    /// Opens an archive of image tiles (PMTiles or MBTiles) for reading by window, zoom levels
    /// as overviews.
    #[cfg(feature = "tiles")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tiles")))]
    pub fn from_tile_archive(archive: crate::tiles::TileArchive) -> Result<Self> {
        Ok(Self::from_tiles(crate::tiles::raster::TileRaster::new(archive, None)?))
    }

    /// The raster in memory, when it is not read on demand.
    pub fn dataset(&self) -> Option<&RasterDataset> {
        match &self.inner {
            Inner::Tiff(_) | Inner::Source(..) => None,
            Inner::Memory(ds) => Some(ds),
        }
    }

    /// Whether the pixels are read on demand (a GeoTIFF), rather than already in memory.
    pub fn is_lazy(&self) -> bool {
        matches!(self.inner, Inner::Tiff(_) | Inner::Source(..))
    }

    /// The width in pixels.
    pub fn width(&self) -> usize {
        match &self.inner {
            Inner::Tiff(t) => t.levels[0].width,
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.raster.cols,
        }
    }

    /// The height in pixels.
    pub fn height(&self) -> usize {
        match &self.inner {
            Inner::Tiff(t) => t.levels[0].height,
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.raster.rows,
        }
    }

    /// The number of bands.
    pub fn band_count(&self) -> usize {
        match &self.inner {
            Inner::Tiff(t) => t.bands,
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.raster.bands,
        }
    }

    /// The pixel data type.
    pub fn data_type(&self) -> DataType {
        match &self.inner {
            Inner::Tiff(t) => t.data_type,
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.raster.data_type,
        }
    }

    /// The driver.
    pub fn driver(&self) -> RasterDriver {
        match &self.inner {
            Inner::Tiff(_) => RasterDriver::GTiff,
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.driver,
        }
    }

    /// The GDAL-style geotransform of the full-resolution image.
    pub fn geo_transform(&self) -> [f64; 6] {
        match &self.inner {
            Inner::Tiff(t) => t.geo_transform,
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.geo_transform(),
        }
    }

    /// The CRS.
    pub fn crs(&self) -> SpatialRef {
        match &self.inner {
            Inner::Tiff(t) => SpatialRef::from_raster_crs(&t.tags.crs_info()),
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.crs(),
        }
    }

    /// The nodata value, when the file declares one.
    pub fn nodata(&self) -> Option<f64> {
        match &self.inner {
            Inner::Tiff(t) => t.tags.band_nodata.iter().find(|(b, _)| *b == 1).map(|(_, v)| *v).or(t.tags.nodata),
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.nodata(),
        }
    }

    /// A band's nodata value (one-based), including per-band VRT/PAM metadata.
    pub fn band_nodata(&self, band: usize) -> Option<f64> {
        if band == 0 || band > self.band_count() {
            return None;
        }
        match &self.inner {
            Inner::Tiff(t) => t.tags.band_nodata.iter().find(|(b, _)| *b == band).map(|(_, v)| *v).or(t.tags.nodata),
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.band_nodata(band),
        }
    }

    /// The dataset's metadata items (for tile archives: the zoom level read and the archive's
    /// metadata).
    pub fn metadata(&self) -> Vec<(String, String)> {
        match &self.inner {
            Inner::Tiff(_) => Vec::new(),
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.raster.metadata.clone(),
        }
    }

    /// The sizes `(width, height)` of the overviews, largest first.
    pub fn overviews(&self) -> Vec<(usize, usize)> {
        match &self.inner {
            Inner::Tiff(t) => t.levels[1..].iter().map(|l| (l.width, l.height)).collect(),
            Inner::Memory(_) => Vec::new(),
            Inner::Source(_, s) => s.overviews(),
        }
    }

    /// The tile (or strip) size `(width, height)` of the full-resolution image, when it is read on
    /// demand (for JPEG 2000, the block size GDAL gives it).
    pub fn block_size(&self) -> Option<(usize, usize)> {
        match &self.inner {
            Inner::Tiff(t) => Some(t.levels[0].block),
            Inner::Memory(_) => None,
            Inner::Source(_, s) => s.block_size(),
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
        if let Inner::Source(ds, _) = &self.inner {
            return ds.clone();
        }
        let raster = wbraster::Raster {
            cols: self.width(),
            rows: self.height(),
            bands: self.band_count(),
            x_min: 0.0,
            y_min: 0.0,
            cell_size_x: 1.0,
            cell_size_y: 1.0,
            nodata: self.nodata().unwrap_or(-9999.0),
            data_type: self.data_type(),
            crs: self.crs().to_crs_info(),
            metadata: Vec::new(),
            data: wbraster::raster::RasterData::new_filled(self.data_type(), 0, 0.0),
        };
        let mut ds = RasterDataset::with_driver(raster, self.driver(), self.nodata().is_some());
        ds.band_nodata = (1..=self.band_count()).map(|b| self.band_nodata(b)).collect();
        ds.has_nodata = ds.band_nodata[0].is_some();
        ds.raster.nodata = ds.band_nodata[0].unwrap_or(-9999.);
        ds.set_geo_transform(self.geo_transform());
        ds
    }

    /// Reads the whole full-resolution image.
    pub fn read(&mut self) -> Result<RasterDataset> {
        let window = Window::full(self.width(), self.height());
        self.read_window(window, &WindowOptions::default())
    }

    /// Writes the full-resolution image to `path`, as [`crate::write_raster_dataset`] writes the
    /// raster [`LazyRaster::read`] reads. A GeoTIFF or JPEG 2000 file read in place and written as
    /// a GeoTIFF or a COG (in a compression terrio's own writer encodes) is read and written a
    /// strip of rows at a time, so that it is never held whole; other rasters are read whole, then
    /// written.
    ///
    /// ```no_run
    /// use terrio::RasterWriteOptions;
    ///
    /// let mut r = terrio::open_raster_lazy("big.tif")?;
    /// let options = RasterWriteOptions { driver: Some(terrio::formats::RasterDriver::Cog), ..Default::default() };
    /// r.write("big_cog.tif", &options)?;
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn write(&mut self, path: &str, options: &crate::RasterWriteOptions) -> Result<()> {
        let (width, height, bands) = (self.width(), self.height(), self.band_count());
        let block = self.block_size().map(|(_, h)| h);
        let value_size = self.data_type().size_bytes();
        if let Inner::Source(ds, s) = &self.inner
            && let Some(s) = s.streamed()
            && !crate::ops::strips::overwrites(path, &s.inputs())
        {
            // Strips of a row of blocks, several read at once: blocks slow to decode overlap the
            // next strips' blocks rather than keep the other threads waiting.
            let rows = crate::ops::strips::small_strip_rows(width, bands, value_size, block);
            let all: Vec<usize> = (1..=bands).collect();
            let mut out = crate::ops::strips::StripWriter::create(ds, path, options)?;
            crate::ops::strips::read_ahead(
                height.div_ceil(rows),
                |i| {
                    let y = i * rows;
                    let h = rows.min(height - y);
                    Ok((s.read_rows(Window::new(0, y, width, h), &all)?, h))
                },
                |_, (data, h)| out.write(data, h),
            )?;
            return out.finish();
        }
        let Inner::Tiff(t) = &self.inner else { return crate::write_raster_dataset(&self.read()?, path, options) };
        // An output that replaces the input is written from the input read whole.
        let inputs: Vec<String> = t.file.iter().map(|f| f.to_string_lossy().into_owned()).collect();
        if t.file.is_none() || crate::ops::strips::overwrites(path, &inputs) {
            return crate::write_raster_dataset(&self.read()?, path, options);
        }
        // The header the whole raster is read with.
        let mut header = self.header();
        header.raster.crs = t.tags.crs_info();
        header.raster.metadata = t.tags.metadata.clone();
        let rows = crate::ops::strips::strip_rows(width, bands, value_size, block);
        let mut out = crate::ops::strips::StripWriter::create(&header, path, options)?;
        let read = WindowOptions::default();
        crate::ops::strips::pipelined(
            height.div_ceil(rows),
            |i| {
                let y = i * rows;
                let h = rows.min(height - y);
                Ok((self.read_window(Window::new(0, y, width, h), &read)?.raster.data, h))
            },
            |_, (data, h)| out.write(data, h),
        )?;
        out.finish()
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
        let band_nodata: Vec<Option<f64>> = bands.iter().map(|b| self.band_nodata(*b)).collect();
        let gt = self.level_geo_transform(options.overview)?;
        let (data, nodata, has_nodata, crs) = match &mut self.inner {
            Inner::Tiff(t) => {
                let level = options.overview.map_or(0, |i| i + 1);
                let data = t.read(level, window, &bands)?;
                (data, t.tags.nodata.unwrap_or(-9999.0), t.tags.nodata.is_some(), Some(t.tags.crs_info()))
            }
            Inner::Memory(ds) => (cut(&ds.raster, window, &bands), ds.raster.nodata, ds.has_nodata, Some(ds.raster.crs.clone())),
            Inner::Source(ds, s) => {
                let data = s.read(options.overview.map_or(0, |i| i + 1), window, &bands)?;
                (data, ds.raster.nodata, ds.has_nodata, Some(ds.raster.crs.clone()))
            }
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
        let raster = crate::pixel::raster_from_data(cfg, data)?;
        let (x, y) = (window.x as f64, window.y as f64);
        let mut ds = RasterDataset::with_driver(raster, self.driver(), has_nodata);
        ds.has_nodata = band_nodata[0].is_some();
        ds.raster.nodata = band_nodata[0].unwrap_or(-9999.);
        ds.band_nodata = band_nodata;
        ds.set_geo_transform([gt[0] + x * gt[1] + y * gt[2], gt[1], gt[2], gt[3] + x * gt[4] + y * gt[5], gt[4], gt[5]]);
        Ok(ds)
    }
}

impl LazyRaster {
    /// Whether the raster has a per-dataset mask (GDAL's `GMF_PER_DATASET`): a GeoTIFF's
    /// internal mask, or a `.msk` sidecar.
    pub fn has_mask(&self) -> bool {
        self.sidecar_mask.is_some()
            || match &self.inner {
                Inner::Source(_, s) => s.has_mask(),
                Inner::Tiff(t) => t.masks.iter().any(|m| !m.overview && (m.width, m.height) == (t.levels[0].width, t.levels[0].height)),
                _ => false,
            }
    }

    /// Reads a window of the per-dataset mask, at full resolution or of an overview: 255 for
    /// valid pixels, 0 for invalid ones, row by row. `None` when the raster has no mask (for an
    /// overview without a mask of its own, the full-resolution mask is sampled by nearest
    /// neighbour, as GDAL does).
    pub fn read_mask(&mut self, window: Window, overview: Option<usize>) -> Result<Option<Vec<u8>>> {
        let (lw, lh) = self.level_size(overview)?;
        if !window.fits(lw, lh) {
            return Err(Error::InvalidArgument(format!(
                "window {}x{} at ({}, {}) is outside the {lw}x{lh} image",
                window.width, window.height, window.x, window.y
            )));
        }
        let (fw, fh) = (self.width(), self.height());
        if let Some(m) = self.sidecar_mask.as_mut() {
            return read_sidecar_mask(m, window, (lw, lh)).map(Some);
        }
        if let Inner::Source(_, source) = &mut self.inner {
            return source.read_mask(overview.map_or(0, |v| v + 1), window);
        }
        let Inner::Tiff(t) = &mut self.inner else { return Ok(None) };
        let w = (window.x, window.y, window.width, window.height);
        let own = t.masks.iter().find(|m| m.overview == overview.is_some() && (m.width, m.height) == (lw, lh)).cloned();
        let reader = t.decoder.inner();
        if let Some(m) = own {
            return crate::tiff_mask::read_mask(reader, &m, w).map(Some);
        }
        let Some(full) = t.masks.iter().find(|m| !m.overview && (m.width, m.height) == (fw, fh)).cloned() else { return Ok(None) };
        // Nearest neighbour from the full-resolution mask.
        let (sx, sy) = (fw as f64 / lw as f64, fh as f64 / lh as f64);
        let col = |c: usize| (((c as f64 + 0.5) * sx) as usize).min(fw - 1);
        let row = |r: usize| (((r as f64 + 0.5) * sy) as usize).min(fh - 1);
        let (c0, c1) = (col(window.x), col(window.x + window.width - 1));
        let (r0, r1) = (row(window.y), row(window.y + window.height - 1));
        let src = crate::tiff_mask::read_mask(reader, &full, (c0, r0, c1 - c0 + 1, r1 - r0 + 1))?;
        let sw = c1 - c0 + 1;
        let mut out = Vec::with_capacity(window.width * window.height);
        for r in window.y..window.y + window.height {
            for c in window.x..window.x + window.width {
                out.push(src[(row(r) - r0) * sw + col(c) - c0]);
            }
        }
        Ok(Some(out))
    }

    /// A raster read by window by `source`, described by `header` (a dataset without pixels).
    pub(crate) fn from_source(header: RasterDataset, source: Box<dyn WindowSource>) -> Self {
        Self { inner: Inner::Source(header, source), sidecar_mask: None }
    }

    /// Wraps an archive of image tiles (PMTiles, MBTiles), its zoom levels as overviews.
    #[cfg(feature = "tiles")]
    pub(crate) fn from_tiles(t: crate::tiles::raster::TileRaster) -> Self {
        Self::from_source(t.header(), Box::new(t))
    }
}

impl From<RasterDataset> for LazyRaster {
    /// Wraps a raster in memory, so that it is read by window like a lazy one.
    fn from(ds: RasterDataset) -> Self {
        Self { inner: Inner::Memory(ds), sidecar_mask: None }
    }
}

/// Reads a window of a `.msk` sidecar's mask at a level of size `(lw, lh)`: the sidecar's own
/// overview of that size, else its full-resolution mask sampled by nearest neighbour, as GDAL
/// does. 255 for valid pixels, 0 for invalid ones.
fn read_sidecar_mask(m: &mut LazyRaster, window: Window, (lw, lh): (usize, usize)) -> Result<Vec<u8>> {
    if window.width == 0 || window.height == 0 {
        return Ok(Vec::new());
    }
    let opts = |overview| WindowOptions { bands: vec![1], overview };
    // A Byte band (as GDAL writes masks); another type read through doubles.
    let to_mask = |ds: RasterDataset| -> Vec<u8> {
        match ds.raster.data {
            RasterData::U8(v) => v.into_iter().map(|b| if b != 0 { 255 } else { 0 }).collect(),
            other => other.to_f64_vec().into_iter().map(|v| if v != 0.0 { 255 } else { 0 }).collect(),
        }
    };
    let (fw, fh) = (m.width(), m.height());
    if (lw, lh) == (fw, fh) {
        return m.read_window(window, &opts(None)).map(to_mask);
    }
    if let Some(i) = m.overviews().iter().position(|s| *s == (lw, lh)) {
        return m.read_window(window, &opts(Some(i))).map(to_mask);
    }
    let (sx, sy) = (fw as f64 / lw as f64, fh as f64 / lh as f64);
    let col = |c: usize| (((c as f64 + 0.5) * sx) as usize).min(fw - 1);
    let row = |r: usize| (((r as f64 + 0.5) * sy) as usize).min(fh - 1);
    let (c0, c1) = (col(window.x), col(window.x + window.width - 1));
    let (r0, r1) = (row(window.y), row(window.y + window.height - 1));
    let sw = c1 - c0 + 1;
    let src = to_mask(m.read_window(Window::new(c0, r0, sw, r1 - r0 + 1), &opts(None))?);
    let mut out = Vec::with_capacity(window.width * window.height);
    for r in window.y..window.y + window.height {
        for c in window.x..window.x + window.width {
            out.push(src[(row(r) - r0) * sw + col(c) - c0]);
        }
    }
    Ok(out)
}

/// Whether a stream starts with a TIFF or BigTIFF signature. Rewinds it.
fn is_tiff<R: Read + Seek>(r: &mut R) -> bool {
    let mut b = [0u8; 4];
    let ok = r.read_exact(&mut b).is_ok();
    let _ = r.seek(SeekFrom::Start(0));
    ok && matches!(&b, b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+")
}

/// Cuts a window of some bands (numbered from 1) out of a raster in memory.
pub(crate) fn cut(r: &wbraster::Raster, w: Window, bands: &[usize]) -> RasterData {
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

/// A TIFF's bytes in memory, which more decoders can read at once to decode chunks in parallel.
#[derive(Clone)]
struct SharedBytes(std::sync::Arc<Vec<u8>>);

impl AsRef<[u8]> for SharedBytes {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

/// Where a TIFF's bytes come from.
enum Source {
    File(BufReader<std::fs::File>),
    Memory(std::io::Cursor<SharedBytes>),
    #[cfg(remote)]
    Http(crate::http::RangeReader),
    /// A remote file whose bytes the caller fetches (see [`crate::sparse`]).
    Sparse(crate::sparse::SparseFile),
}

impl Source {
    /// Fetches byte ranges `(offset, length)` ahead of reading them, when that saves requests.
    #[cfg_attr(not(remote), allow(unused_variables))]
    fn prefetch(&self, ranges: &[(u64, u64)]) -> Result<()> {
        match self {
            Source::File(_) | Source::Memory(_) => Ok(()),
            #[cfg(remote)]
            Source::Http(r) => Ok(r.prefetch(ranges)?),
            Source::Sparse(s) => Ok(s.prefetch(ranges)?),
        }
    }
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Source::File(f) => f.read(buf),
            Source::Memory(m) => m.read(buf),
            #[cfg(remote)]
            Source::Http(r) => r.read(buf),
            Source::Sparse(s) => s.read(buf),
        }
    }
}

impl Seek for Source {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self {
            Source::File(f) => f.seek(pos),
            Source::Memory(m) => m.seek(pos),
            #[cfg(remote)]
            Source::Http(r) => r.seek(pos),
            Source::Sparse(s) => s.seek(pos),
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
    /// GDAL's internal masks (of the full-resolution image and of overviews).
    masks: Vec<crate::tiff_mask::MaskImage>,
    /// The index of the image the decoder is on.
    current_ifd: usize,
    /// Keeps a file extracted from an archive.
    _local: Option<vfs::LocalPath>,
    /// The local file the TIFF is read from, which more decoders can open to decode chunks in
    /// parallel.
    file: Option<std::path::PathBuf>,
}

/// What more decoders of a TIFF read: its local file, or its bytes in memory.
enum Reopened<'a> {
    File(&'a std::path::Path),
    Memory(SharedBytes),
}

fn tiff_err(e: tiff::TiffError) -> Error {
    Error::InvalidData(format!("TIFF: {e}"))
}

impl TiffRaster {
    fn new(mut source: Source, local: Option<vfs::LocalPath>) -> Result<Self> {
        let tags = read_geo_tags_from(&mut source).ok_or_else(|| Error::InvalidData("not a TIFF file".into()))?;
        let masks = crate::tiff_mask::find_masks(&mut source);
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
        // As the in-memory reader does without georeferencing: unit pixels, with the image's
        // bottom at y = 0.
        let geo_transform = tags.corner_geo_transform().unwrap_or([0.0, 1.0, 0.0, h as f64, 0.0, -1.0]);
        Ok(Self { decoder: dec, tags, bands, data_type, geo_transform, levels, masks, current_ifd: 0, _local: local, file: None })
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
        // Chunks with data, and where they go.
        let mut wanted: Vec<u32> = Vec::with_capacity(chunks.len());
        // A chunk can fill several output bands (a band asked for twice in a planar image).
        let mut targets: std::collections::HashMap<u32, Vec<ChunkTarget>> = std::collections::HashMap::with_capacity(chunks.len());
        for (idx, x0, y0, map) in chunks {
            if counts.get(idx).copied().unwrap_or(0) == 0 {
                continue;
            }
            let t = targets.entry(idx as u32).or_default();
            if t.is_empty() {
                wanted.push(idx as u32);
            }
            t.push((x0, y0, map));
        }
        // More decoders over the local file, or over the bytes in memory.
        let tags = &self.tags;
        let source = match (self.file.as_deref(), dec.inner().get_ref()) {
            (Some(path), _) => Some(Reopened::File(path)),
            (None, Source::Memory(m)) => Some(Reopened::Memory(m.get_ref().clone())),
            _ => None,
        };
        let reopen = source.map(|source| {
            move || -> Result<Decoder<PatchedReader<Source>>> {
                let s = match &source {
                    Reopened::File(path) => Source::File(BufReader::new(std::fs::File::open(path)?)),
                    Reopened::Memory(bytes) => Source::Memory(std::io::Cursor::new(bytes.clone())),
                };
                Ok(Decoder::new(PatchedReader::new(s, tags)).map_err(tiff_err)?.with_limits(Limits::unlimited()))
            }
        });
        let reopen = reopen.as_ref().map(|f| f as crate::geotiff::Reopen<PatchedReader<Source>>);
        crate::geotiff::decode_chunks(dec, reopen, ifd, &wanted, &tiff_err, |(idx, chunk, (dw, dh))| {
            let targets = &targets[&idx];
            macro_rules! place {
                ($($out:ident <- $res:ident),*) => {
                    match (&mut out, chunk) {
                        $((RasterData::$out(o), DecodingResult::$res(v)) => {
                            for &(x0, y0, map) in targets {
                                place(o, &v, &ChunkGeom { x0, y0, dw, dh, spp }, w, map);
                            }
                        })*
                        (RasterData::F32(o), DecodingResult::F16(v)) => {
                            let v: Vec<f32> = v.iter().map(|x| x.to_f32()).collect();
                            for &(x0, y0, map) in targets {
                                place(o, &v, &ChunkGeom { x0, y0, dw, dh, spp }, w, map);
                            }
                        }
                        _ => return Err(Error::InvalidData("TIFF chunks differ in sample type".into())),
                    }
                };
            }
            place!(U8 <- U8, I8 <- I8, U16 <- U16, I16 <- I16, U32 <- U32, I32 <- I32, U64 <- U64, I64 <- I64, F32 <- F32, F64 <- F64);
            Ok(())
        })?;
        Ok(out)
    }
}

/// Where a chunk goes in a window: its first column and row, and the output band of each of its
/// samples to copy.
type ChunkTarget<'a> = (usize, usize, &'a Vec<(usize, usize)>);

/// Where a decoded chunk lies in its image.
pub(crate) struct ChunkGeom {
    /// The image column and row of the chunk's first pixel.
    pub x0: usize,
    pub y0: usize,
    /// The chunk's columns and rows that hold image data (edge chunks are cropped).
    pub dw: usize,
    pub dh: usize,
    /// Samples per pixel in the chunk.
    pub spp: usize,
}

/// Copies the part of a chunk that intersects a window into the window's band-major buffer.
/// `map` pairs each sample to copy with its output band.
pub(crate) fn place<T: Copy>(out: &mut [T], vals: &[T], g: &ChunkGeom, w: Window, map: &[(usize, usize)]) {
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
        if let [(0, band)] = map
            && g.spp == 1
        {
            // One sample per pixel: copy the row's samples the chunk holds.
            let src = (src_row + c0.saturating_sub(g.x0)).min(vals.len());
            let n = c1.saturating_sub(c0).min(vals.len() - src);
            let dst = band * band_len + dst_row + c0 - w.x;
            out[dst..dst + n].copy_from_slice(&vals[src..src + n]);
            continue;
        }
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
        // Packed samples (GDAL's NBITS) are read as the type GDAL gives them.
        (1, 9..=16) => DataType::U16,
        (1, 17..=32) => DataType::U32,
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

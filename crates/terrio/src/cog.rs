// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/gtiff/, gcore/gdaldefaultoverviews.cpp
// No code is translated from these sources.

//! Lazy rasters: open a raster for its metadata only, then read pixel windows, at full resolution
//! or from an overview.
//!
//! GeoTIFFs (Cloud Optimized or not, local or remote) are read in place: opening reads the image
//! headers, and a window read decodes only the tiles or strips that intersect the window. Remote
//! files are read through HTTP range requests (see [`crate::http`]), fetching the tiles of a window
//! in as few requests as possible. Raw-layout rasters (SRTM tiles, EHdr, ENVI, SAGA, ERS, Idrisi and
//! Surfer 6 grids), JPEG 2000, GRIB and Zarr are read in place by their own drivers too. Other
//! formats are read whole on open, then windows are cut from memory.
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
use crate::geotiff::{ChunkTables, GeoTags, PatchedReader, try_read_geo_tags_from};
use crate::vfs::{self, Location};
use crate::{Error, RasterDataset, Result};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use tiff::decoder::{Decoder, DecodingResult, Limits};
use tiff::tags::Tag;
use wbraster::DataType;
use wbraster::raster::RasterData;

/// The most images (IFDs) looked at for overviews.
const MAX_IMAGES: usize = 256;

/// The bytes buffered over a [`crate::sparse::SparseFile`].
///
/// The TIFF decoder issues many small reads (tag values, predictors and codec streams).
/// Buffering avoids a lock and lookup for every small decoder read. The buffer is at most one
/// sparse block (and at most 64 KiB); SparseBlockReader also clips refills at block boundaries,
/// so read-ahead cannot announce a following block that the decoder never requests.
const SPARSE_BUFFER_BYTES: usize = 64 * 1024;

/// The most chunks a single prefetch collects.
///
/// A whole-raster read of a file without overviews would otherwise list every strip at once (a
/// very large file has hundreds of thousands). Above this, the read falls back to the existing
/// progressive window reads and the caller's read-ahead.
pub(crate) const MAX_PREFETCH_CHUNKS: usize = 4096;

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

    /// The error of a read of the window from an image of `width` x `height` pixels, unless it
    /// [`fits`](Self::fits).
    pub(crate) fn check(&self, width: usize, height: usize) -> Result<()> {
        if self.fits(width, height) {
            return Ok(());
        }
        Err(Error::InvalidArgument(format!(
            "window {}x{} at ({}, {}) is outside the {width}x{height} image",
            self.width, self.height, self.x, self.y
        )))
    }
}

/// The bands a read of `bands` (numbered from 1, all for none) reads of a raster of `count` bands,
/// or the error of a band it does not have.
pub(crate) fn read_bands(bands: &[usize], count: usize) -> Result<Vec<usize>> {
    let bands: Vec<usize> = if bands.is_empty() { (1..=count).collect() } else { bands.to_vec() };
    if let Some(b) = bands.iter().find(|b| **b == 0 || **b > count) {
        return Err(Error::InvalidArgument(format!("no band {b}: the raster has {count} band(s)")));
    }
    Ok(bands)
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

/// Bounds on what decoding a GeoTIFF's pixels may allocate, checked before the allocation: a
/// malformed header (a strip of 10^12 pixels in a file of a few bytes) is an error rather than an
/// allocation the process cannot survive.
///
/// The default is the `tiff` crate's: 256 MiB for a decoded chunk (tile or strip). Files with
/// larger chunks, such as a pixel-interleaved GeoTIFF of many bands whose tile holds every band,
/// need a larger limit: the `TERRIO_TIFF_MAX_CHUNK_BYTES` option sets it for the readers opened
/// without explicit limits (see [`TiffReadLimits::from_config`]); see also
/// [`LazyRaster::open_tiff_with_limits`],
/// [`LazyRaster::from_tiff_bytes_with_limits`], [`LazyRaster::from_sparse_with_limits`] and
/// [`crate::open_raster_with_tiff_limits`] for eager reads. The
/// window a read returns is not bounded by it: a window the process cannot hold is refused before
/// it is allocated (see [`crate::usable_memory`]); check the window's size before reading it.
///
/// Nor do the limits bound the chunk tables a reader keeps: after a level's first window read,
/// its chunk offsets and byte counts (about 16 bytes per chunk) are kept for the reader's
/// lifetime, so a file with very many small chunks keeps that memory in use while the reader is
/// open. A reader that decodes on several threads also keeps their decoders between window reads,
/// each with its own copy of those tables: the open readers of the process keep these copies
/// within 256 MiB together (`TERRIO_TIFF_SPARE_CACHEMAX` changes that process-wide budget,
/// or disables spare retention at 0), closing the least recently used decoders of the other readers first,
/// or keep those of one reader when they alone take more. A reader whose decoders were closed
/// opens them again on its next such read, which reads the tables once more for each thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TiffReadLimits {
    /// The most bytes one decoded chunk may take, whichever codec decodes it, and the most a
    /// LERC chunk's Deflate or Zstandard stream may inflate to. The `tiff` crate (0.11) bounds
    /// the size of tag values, such as the chunk offsets, by the same limit.
    pub chunk_bytes: usize,
}

impl Default for TiffReadLimits {
    fn default() -> Self {
        Self { chunk_bytes: Limits::default().decoding_buffer_size }
    }
}

/// The option that sets [`TiffReadLimits::chunk_bytes`] of readers opened without explicit limits.
pub(crate) const TIFF_MAX_CHUNK_BYTES_OPTION: &str = "TERRIO_TIFF_MAX_CHUNK_BYTES";

/// The resource of a GeoTIFF's [`Error::DecodedLimitExceeded`], whose message names
/// [`TIFF_MAX_CHUNK_BYTES_OPTION`].
pub(crate) const TIFF_CHUNK_RESOURCE: &str = "TIFF chunk or tag value";

impl TiffReadLimits {
    /// The limits of a reader opened without explicit ones: the defaults, with
    /// [`TiffReadLimits::chunk_bytes`] set by the `TERRIO_TIFF_MAX_CHUNK_BYTES` option (a positive
    /// number of bytes) when it is set, in the calling thread's scope or the environment
    /// ([`crate::config::get`]). Raise it only for trusted files: it bounds what a malformed
    /// header can make a read allocate.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] when the option is set to anything but a positive integer.
    pub fn from_config() -> Result<Self> {
        match crate::config::get(TIFF_MAX_CHUNK_BYTES_OPTION) {
            None => Ok(Self::default()),
            Some(v) => v.trim().parse::<usize>().ok().filter(|n| *n > 0).map(|chunk_bytes| Self { chunk_bytes }).ok_or_else(|| {
                Error::InvalidArgument(format!("{TIFF_MAX_CHUNK_BYTES_OPTION} must be a positive integer number of bytes, not '{v}'"))
            }),
        }
    }

    /// No configured chunk/tag limit. Decoded chunk buffers must still fit the process's
    /// usable memory, independently of this limit.
    pub fn unlimited() -> Self {
        Self { chunk_bytes: usize::MAX }
    }

    /// The `tiff` crate's limits. Stored chunks are read as far as the file goes, whatever byte
    /// count they declare (as GDAL reads them), so their size is not limited.
    pub(crate) fn tiff(self) -> Limits {
        let mut l = Limits::unlimited();
        // `tiff` 0.11 bounds tag values by `decoding_buffer_size` and reads `ifd_value_size`
        // nowhere; both are set, should a later version read it.
        l.decoding_buffer_size = self.chunk_bytes;
        l.ifd_value_size = self.chunk_bytes;
        l
    }

    /// [`TiffReadLimits::tiff`] within the bounds of a file of `len` bytes
    /// ([`crate::geotiff::file_limits`]): the tag values the crate allocates are read from the
    /// file, so a file holds at most so many of them, whatever these limits allow.
    pub(crate) fn tiff_within(self, len: u64) -> Limits {
        let file = crate::geotiff::file_limits(len);
        let mut l = self.tiff();
        l.decoding_buffer_size = l.decoding_buffer_size.min(file.decoding_buffer_size);
        l.ifd_value_size = l.ifd_value_size.min(file.ifd_value_size);
        l
    }

    /// The error of a TIFF decoder with these limits: [`Error::DecodedLimitExceeded`], with the
    /// limit, for a chunk or tag value over it.
    pub(crate) fn error(self) -> impl Fn(tiff::TiffError) -> Error + Sync + Copy {
        move |e| match e {
            tiff::TiffError::LimitsExceeded => Error::DecodedLimitExceeded { resource: TIFF_CHUNK_RESOURCE, limit: self.chunk_bytes },
            e => tiff_err(e),
        }
    }
}

/// A raster opened for its metadata, whose pixels are read on demand.
pub struct LazyRaster {
    inner: Inner,
    /// GDAL's external mask, the `.msk` sidecar of a local file without an internal mask: a TIFF
    /// whose first band is the per-dataset mask, with overviews of its own.
    sidecar_mask: Option<Box<LazyRaster>>,
    asset_policy: Option<crate::stac::AssetReadPolicy>,
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
    Memory(Arc<RasterDataset>),
    /// A raster another driver reads by window: its header (without pixels) and reader.
    Source(Arc<RasterDataset>, Box<dyn WindowSource>),
}

/// A raster read by window, at full resolution or from an overview, by a driver other than the
/// GeoTIFF one (STAC mosaics, Zarr pyramids).
pub(crate) trait WindowSource: Send + Sync {
    /// Whether the source explicitly supplies an affine transform, when known.
    fn geotransform_present(&self) -> Option<bool> {
        None
    }

    /// The declared type of one band, for sources with heterogeneous band types.
    fn band_data_type(&self, _band: usize) -> Option<DataType> {
        None
    }
    /// A second handle to an already shared source.
    fn shared_clone(&self) -> Option<Box<dyn WindowSource>> {
        None
    }
    /// The explicitly tagged source alpha band.
    fn alpha_band(&self) -> Option<usize> {
        None
    }
    /// Whether [`LazyRaster::read`] gives the dataset's metadata items, as the driver's eager
    /// read does, rather than a window's.
    fn whole_read_keeps_metadata(&self) -> bool {
        false
    }
    /// The sizes `(width, height)` of the overviews, largest first.
    fn overviews(&self) -> Vec<(usize, usize)>;
    /// The rows strips of the full-resolution image are best made of (a multiple of), so that
    /// each block of the files the source reads is read once: by default, its block height.
    fn row_alignment(&mut self) -> Option<usize> {
        self.block_size().map(|(_, h)| h)
    }
    /// The block size `(width, height)` reads are best aligned to, when the format has one.
    fn block_size(&self) -> Option<(usize, usize)> {
        None
    }
    /// Whether the source exposes a dataset-level validity mask.
    fn has_mask(&self) -> bool {
        false
    }
    /// Reads a mask window: 255 for valid pixels, zero for invalid pixels.
    fn read_mask(&mut self, _level: usize, _window: Window) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }
    /// Whether a level (`i + 1` for overview `i`) has a mask of its own, which
    /// [`WindowSource::read_own_mask`] reads, rather than none: by default, every level of a
    /// source with a mask.
    fn level_has_own_mask(&self, _level: usize) -> bool {
        self.has_mask()
    }
    /// Whether GDAL's band of a level without a mask of its own reports the mask flags of full
    /// resolution, as an internal overview does: by default, every level.
    fn level_keeps_mask_flags(&self, _level: usize) -> bool {
        true
    }
    /// The overviews (numbered from 0) that GDAL's band of an overview level (`i + 1` for
    /// overview `i`) has of its own, which `RasterIO` chooses among again (see
    /// [`LazyRaster::level_overviews`]): by default, none.
    fn level_band_overviews(&self, _level: usize) -> Vec<usize> {
        Vec::new()
    }
    /// A candidate overview's GDAL `RESAMPLING` item (GDAL 3.13.3), if stored.
    fn level_resampling(&mut self, _overview: usize) -> Option<String> {
        None
    }
    /// The overviews of GDAL's per-dataset mask band (see [`LazyRaster::mask_band_overviews`]):
    /// by default, every overview of a source with a mask.
    fn mask_band_overviews(&self) -> Vec<(usize, usize)> {
        if self.has_mask() { self.overviews() } else { Vec::new() }
    }
    /// The bits of a sample of the per-dataset mask band as stored (see
    /// [`LazyRaster::mask_band_bits`]): by default, unknown.
    fn mask_band_bits(&self) -> Option<u16> {
        None
    }
    /// Reads a window of a level's own mask (see [`LazyRaster::read_level_mask`]): `None` for a
    /// level without one. By default, [`WindowSource::read_mask`].
    fn read_own_mask(&mut self, level: usize, window: Window) -> Result<Option<Vec<u8>>> {
        self.read_mask(level, window)
    }
    /// Files a non-streamed source reads; unknown inventories must fail before an export.
    fn input_paths(&self) -> Result<Vec<String>> {
        Err(Error::Unsupported("source inventory unavailable".into()))
    }
    /// The source as [`LazyRaster::write`] reads it a strip of rows at a time, several strips at
    /// once; `None` when it is read whole.
    fn streamed(&self) -> Option<&dyn StreamedSource> {
        None
    }
    /// Whether a window is read from the parts of the source it covers alone, so that
    /// [`LazyRaster::write`] reads the source a strip of rows at a time (on one thread) rather
    /// than whole. False for sources that decode the whole raster for any window.
    fn reads_by_window(&self) -> bool {
        false
    }
    /// Reads a window of some bands (numbered from 1) of a level (0 for full resolution, `i + 1`
    /// for overview `i`), band after band.
    fn read(&mut self, level: usize, window: Window, bands: &[usize]) -> Result<RasterData>;
    /// Reads the pixels at columns `cols` of rows `rows` of a level (see
    /// [`LazyRaster::read_sampled`]), when the source reads them from the blocks that hold them
    /// alone; `None` for a source that would read the window spanning them.
    fn read_sampled(&mut self, _level: usize, _cols: &[usize], _rows: &[usize], _bands: &[usize]) -> Result<Option<RasterData>> {
        Ok(None)
    }
}

/// A [`WindowSource`] whose full-resolution rows can be read from several threads at once.
pub(crate) trait StreamedSource: Sync {
    /// The local files the source reads: an output replacing one of them is written from the
    /// source read whole.
    fn inputs(&self) -> Vec<String>;
    /// Source inventory for the overwrite guard. Readers that can fail while
    /// enumerating inputs override this so no destination is opened on failure.
    fn checked_inputs(&self) -> Result<Vec<String>> {
        Ok(self.inputs())
    }
    /// Reads a full-resolution window of some bands (numbered from 1), band after band.
    fn read_rows(&self, window: Window, bands: &[usize]) -> Result<RasterData>;
}

#[derive(Clone)]
struct SharedRaster {
    referenced: bool,
    reader: Arc<Mutex<LazyRaster>>,
    block: Option<(usize, usize)>,
    alpha: Option<usize>,
    mask: bool,
    mask_bits: Option<u16>,
}
impl SharedRaster {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, LazyRaster>> {
        self.reader.lock().map_err(|_| Error::InvalidData("shared raster lock poisoned".into()))
    }
}
impl WindowSource for SharedRaster {
    fn geotransform_present(&self) -> Option<bool> {
        Some(self.referenced)
    }
    fn band_data_type(&self, band: usize) -> Option<DataType> {
        self.lock().ok().map(|reader| reader.band_data_type(band))
    }
    fn shared_clone(&self) -> Option<Box<dyn WindowSource>> {
        Some(Box::new(self.clone()))
    }
    fn alpha_band(&self) -> Option<usize> {
        self.alpha
    }
    fn overviews(&self) -> Vec<(usize, usize)> {
        self.lock().map(|reader| reader.overviews()).unwrap_or_default()
    }
    fn block_size(&self) -> Option<(usize, usize)> {
        self.block
    }
    fn has_mask(&self) -> bool {
        self.mask
    }
    fn read_mask(&mut self, level: usize, window: Window) -> Result<Option<Vec<u8>>> {
        self.lock()?.read_mask(window, level.checked_sub(1))
    }
    fn level_has_own_mask(&self, level: usize) -> bool {
        level.checked_sub(1).map_or(self.mask, |i| self.lock().is_ok_and(|reader| reader.level_has_own_mask(i)))
    }
    fn level_keeps_mask_flags(&self, level: usize) -> bool {
        level.checked_sub(1).is_none_or(|i| self.lock().map_or(true, |reader| reader.level_keeps_mask_flags(i)))
    }
    fn level_band_overviews(&self, level: usize) -> Vec<usize> {
        level.checked_sub(1).and_then(|i| self.lock().ok().map(|reader| reader.level_overviews(Some(i)))).unwrap_or_default()
    }
    fn level_resampling(&mut self, overview: usize) -> Option<String> {
        self.lock().ok()?.level_resampling(overview)
    }
    fn mask_band_overviews(&self) -> Vec<(usize, usize)> {
        self.lock().map(|reader| reader.mask_band_overviews()).unwrap_or_default()
    }
    fn mask_band_bits(&self) -> Option<u16> {
        self.mask_bits
    }
    fn read_own_mask(&mut self, level: usize, window: Window) -> Result<Option<Vec<u8>>> {
        self.lock()?.read_level_mask(window, level.checked_sub(1))
    }
    fn streamed(&self) -> Option<&dyn StreamedSource> {
        Some(self)
    }
    fn reads_by_window(&self) -> bool {
        self.lock().is_ok_and(|reader| reader.reads_by_window())
    }
    fn row_alignment(&mut self) -> Option<usize> {
        self.lock().ok().and_then(|mut reader| reader.row_alignment())
    }
    fn read(&mut self, level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
        Ok(self.lock()?.read_window(window, &WindowOptions { bands: bands.into(), overview: level.checked_sub(1) })?.raster.data)
    }
    fn read_sampled(&mut self, level: usize, cols: &[usize], rows: &[usize], bands: &[usize]) -> Result<Option<RasterData>> {
        self.lock()?.read_sampled(cols, rows, &WindowOptions { bands: bands.into(), overview: level.checked_sub(1) })
    }
}
impl StreamedSource for SharedRaster {
    fn inputs(&self) -> Vec<String> {
        self.checked_inputs().unwrap_or_default()
    }
    fn checked_inputs(&self) -> Result<Vec<String>> {
        self.lock()?.input_paths()
    }
    fn read_rows(&self, window: Window, bands: &[usize]) -> Result<RasterData> {
        Ok(self.lock()?.read_window(window, &WindowOptions { bands: bands.into(), overview: None })?.raster.data)
    }
}

/// Opens a raster lazily: see [`LazyRaster::open`].
pub fn open_raster_lazy(path: &str) -> Result<LazyRaster> {
    LazyRaster::open(path)
}

/// A raster a public constructor opened (callers may hand them untrusted files), unless it
/// declares more than `TERRIO_MAX_DECLARED_BYTES` or `TERRIO_MAX_BANDS` allow (see
/// [`crate::declared`]).
fn declared_allowed(opened: Result<Option<LazyRaster>>) -> Result<Option<LazyRaster>> {
    let opened = opened?;
    if let Some(r) = &opened {
        crate::declared::check_lazy(r)?;
    }
    Ok(opened)
}

impl LazyRaster {
    pub(crate) fn enter_asset_read(&self) -> Result<Option<crate::stac::AssetReadScope>> {
        self.asset_policy.as_ref().map(crate::stac::AssetReadPolicy::enter).transpose()
    }

    /// Shares this raster without decoding pixels or copying loaded pixel buffers.
    /// Lazy handles serialize access to their existing reader and retain window/overview reads.
    /// Handles are read-only aliases: replacing one handle does not replace the others. Loaded
    /// pixels are immutable shared buffers; changes to backing files are not snapshotted.
    pub fn shared(&mut self) -> Self {
        if let Inner::Memory(ds) = &self.inner {
            let inner = Inner::Memory(ds.clone());
            let sidecar_mask = self.sidecar_mask.as_mut().map(|m| Box::new(m.shared()));
            return Self { inner, sidecar_mask, asset_policy: self.asset_policy.clone() };
        }
        if let Inner::Source(header, source) = &self.inner
            && let Some(source) = source.shared_clone()
        {
            let inner = Inner::Source(header.clone(), source);
            let sidecar_mask = self.sidecar_mask.as_mut().map(|m| Box::new(m.shared()));
            return Self { inner, sidecar_mask, asset_policy: self.asset_policy.clone() };
        }
        let mut header = self.header();
        if let Inner::Tiff(t) = &self.inner {
            header.raster.metadata = t.tags.metadata.clone();
            header.band_metadata = t.tags.band_properties.clone();
            header.color_table = t.tags.color_table.clone();
            header.gcps = t.tags.gcps.clone();
            header.raster.crs = t.tags.crs_info();
        }
        let block = self.block_size();
        let alpha = self.alpha_band();
        let mask = self.has_mask();
        let mask_bits = self.mask_band_bits();
        let referenced = self.geotransform_present();
        let policy = self.asset_policy.clone();
        let replacement = Self::from(header.clone());
        let original = std::mem::replace(self, replacement);
        let shared = SharedRaster { referenced, reader: Arc::new(Mutex::new(original)), block, alpha, mask, mask_bits };
        *self = Self::from_source(header.clone(), Box::new(shared.clone()));
        self.asset_policy = policy.clone();
        let mut alias = Self::from_source(header, Box::new(shared));
        alias.asset_policy = policy;
        alias
    }

    pub(crate) fn geotransform_present(&self) -> bool {
        match &self.inner {
            Inner::Tiff(t) => t.tags.geo_transform.is_some(),
            Inner::Source(_, source) => source.geotransform_present().unwrap_or(true),
            Inner::Memory(ds) => ds.geotransform_present.unwrap_or(true),
        }
    }

    pub(crate) fn input_paths(&self) -> Result<Vec<String>> {
        let _scope = self.enter_asset_read()?;
        let mut paths = match &self.inner {
            Inner::Tiff(t) => {
                let mut paths = t.file.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>();
                if let Some(path) = t.zip.as_ref().and_then(|zip| zip.native_archive()) {
                    paths.push(path.to_string_lossy().into_owned());
                }
                paths
            }
            Inner::Memory(_) => Vec::new(),
            Inner::Source(_, source) => match source.streamed() {
                Some(source) => source.checked_inputs()?,
                None => source.input_paths()?,
            },
        };
        if let Some(mask) = &self.sidecar_mask {
            paths.extend(mask.input_paths()?);
        }
        Ok(paths)
    }

    /// The files the dataset is read from, as GDAL's `GetFileList` lists them: for a GeoTIFF, the
    /// file as it was named (a path, URL or archive member), the `.ovr` whose overviews it took,
    /// the `.msk` sidecar mask and the `.aux.xml` sidecar whose CRS or nodata it took; the files
    /// of a VRT or catalog's sources. Empty for rasters in memory or read from a
    /// [`crate::sparse::SparseFile`]. Unlike GDAL's, the list has no RPC sidecar.
    pub fn file_list(&self) -> Result<Vec<String>> {
        let mut files = match &self.inner {
            Inner::Tiff(t) => t.files.clone(),
            Inner::Memory(_) => Vec::new(),
            Inner::Source(..) => return self.input_paths(),
        };
        // As GDAL lists them: the file, its `.ovr`, its `.msk`, then its `.aux.xml`.
        if let Some(mask) = &self.sidecar_mask {
            let at = files.iter().position(|f| f.ends_with(".aux.xml")).unwrap_or(files.len());
            files.splice(at..at, mask.file_list()?);
        }
        Ok(files)
    }

    /// Opens a raster: a path, URL, `s3://`, `gs://` or `az://` object, or zip archive member.
    ///
    /// GeoTIFFs are read in place: only their headers are read now. So are raw-layout rasters (SRTM
    /// tiles, EHdr, ENVI, SAGA, ERS, Idrisi and Surfer 6 grids), JPEG 2000, GRIB and Zarr; other
    /// formats are read whole.
    ///
    /// A raster that declares more than the options `TERRIO_MAX_DECLARED_BYTES` (width x height x
    /// bands x bytes per sample) or `TERRIO_MAX_BANDS` allow, when they are set, is refused with
    /// [`Error::ResourceLimit`] naming the option: from its headers, before any pixel is read,
    /// where its reader reads headers first.
    pub fn open(path: &str) -> Result<Self> {
        let policy = crate::stac::AssetReadPolicy::nested(path)?;
        let _scope = policy.as_ref().map(crate::stac::AssetReadPolicy::enter).transpose()?;
        crate::formats::check_allowed(path, Some(crate::formats::Kind::Raster))?;
        // A missing file, or a directory, is named in the error.
        let mut r = Self::open_without_sidecars(path).map_err(|e| e.naming_local_file(path))?;
        // What it declares, from its header (a format read whole was checked when it was read).
        crate::declared::check_lazy(&r)?;
        r.find_sidecar_mask(path)?;
        if r.asset_policy.is_none() {
            r.asset_policy = policy;
        }
        Ok(r)
    }

    /// Attaches the `.msk` sidecar of a local file (`dem.tif.msk`, as GDAL writes it when
    /// `GDAL_TIFF_INTERNAL_MASK=NO` or for formats without internal masks) when the raster has
    /// no internal mask and the sidecar is a per-dataset mask of its size.
    fn find_sidecar_mask(&mut self, path: &str) -> Result<()> {
        // Not under `GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR`, where GDAL sees an empty directory
        // and reads no `.msk` (GDAL 3.12.1: the mask is then all valid), nor when the allow-list
        // refuses the sidecar, which GDAL cannot read with its GTiff driver de-registered.
        if self.has_mask() || !Location::parse(path).is_local() || crate::config::sidecars_disabled() {
            return Ok(());
        }
        let msk = format!("{path}.msk");
        if !std::path::Path::new(&msk).is_file() || crate::formats::check_allowed(&msk, Some(crate::formats::Kind::Raster)).is_err() {
            return Ok(());
        }
        // Per-dataset (INTERNAL_MASK_FLAGS_1=2, GMF_PER_DATASET): one mask for every band.
        let per_dataset = crate::geotiff::read_geo_tags(std::path::Path::new(&msk)).is_none_or(|t| {
            t.metadata
                .iter()
                .find(|(k, _)| k == "INTERNAL_MASK_FLAGS_1")
                .is_none_or(|(_, v)| v.trim().parse::<u32>().is_ok_and(|f| f & 2 != 0))
        });
        let limits = match &self.inner {
            Inner::Tiff(t) => t.limits,
            _ => TiffReadLimits::default(),
        };
        match Self::open_tiff_with_limits(&msk, limits) {
            Ok(Some(m)) if per_dataset && (m.width(), m.height()) == (self.width(), self.height()) => {
                self.sidecar_mask = Some(Box::new(m));
            }
            Err(error) if error.is_resource_refusal() => return Err(error),
            _ => (),
        }
        Ok(())
    }

    /// Attaches the `.ovr` beside a local GeoTIFF that has no overviews of its own, even under
    /// `TERRIO_DISABLE_READDIR_ON_OPEN=EMPTY_DIR`, which keeps an ordinary open from reading it:
    /// for an external refresh over an extent, which keeps that `.ovr`'s pixels outside it
    /// (`ops::overview`).
    pub(crate) fn attach_managed_ovr(&mut self, path: &str) -> Result<()> {
        if let (Inner::Tiff(t), Location::Local(p)) = (&mut self.inner, Location::parse(path)) {
            let mut ovr = p.into_os_string();
            ovr.push(".ovr");
            t.attach_ovr(std::path::Path::new(&ovr))?;
        }
        Ok(())
    }

    fn open_without_sidecars(path: &str) -> Result<Self> {
        if let Some(result) = crate::catalog::open_lazy(path) {
            return result;
        }
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
        // `LazyRaster::open` checks what it declares before looking for a sidecar mask.
        match Self::open_tiff_unchecked(path)? {
            Some(r) => Ok(r),
            // A local VRT whatever its name, as GDAL's VRT driver identifies it by its content,
            // read as a `.vrt` is. An allow-list without VRT has refused it.
            None if is_local_vrt(path) => crate::ops::vrt::read_vrt(path).map(|ds| Self {
                inner: Inner::Memory(Arc::new(ds)),
                sidecar_mask: None,
                asset_policy: crate::stac::AssetReadPolicy::current(),
            }),
            None => crate::open_raster(path).map(|ds| Self {
                inner: Inner::Memory(Arc::new(ds)),
                sidecar_mask: None,
                asset_policy: crate::stac::AssetReadPolicy::current(),
            }),
        }
    }

    /// Opens a GeoTIFF in place, reading only its headers. Returns `None` when the file is not a
    /// TIFF (having read only its first bytes). Refused when it declares more than
    /// `TERRIO_MAX_DECLARED_BYTES` or `TERRIO_MAX_BANDS` allow, as [`LazyRaster::open`] refuses it.
    pub fn open_tiff(path: &str) -> Result<Option<Self>> {
        declared_allowed(Self::open_tiff_unchecked(path))
    }

    /// [`LazyRaster::open_tiff`] with other decoding limits than the defaults.
    pub fn open_tiff_with_limits(path: &str, limits: TiffReadLimits) -> Result<Option<Self>> {
        declared_allowed(Self::open_tiff_using(path, || Ok(limits)))
    }

    /// [`LazyRaster::open_tiff`] whatever the GeoTIFF declares: for [`LazyRaster::open`], which
    /// checks it before looking for a sidecar mask, and for terrio's own files, the copy of a GeoTIFF
    /// an update wrote, read back to refresh its overviews (`ops::update`).
    pub(crate) fn open_tiff_unchecked(path: &str) -> Result<Option<Self>> {
        Self::open_tiff_using(path, TiffReadLimits::from_config)
    }

    fn open_tiff_using(path: &str, limits: impl FnOnce() -> Result<TiffReadLimits>) -> Result<Option<Self>> {
        let loc = Location::parse(path);
        let resolve_file = || {
            let local = vfs::resolve(path)?;
            let file = local.path().to_path_buf();
            Ok::<_, Error>((
                Source::File(BufReader::new(crate::geotiff::FileCursor::new(std::fs::File::open(&file)?))),
                Some(local),
                Some(file),
            ))
        };
        let (mut source, local, file) = match &loc {
            #[cfg(remote)]
            Location::Url(_) | Location::S3 { .. } => {
                let remote = loc.remote().ok_or_else(|| Error::InvalidArgument(format!("'{path}' is not remote")))?;
                (Source::Http(crate::http::RangeReader::open(&remote)?), None, None)
            }
            Location::Local(p) if !p.is_file() => return Ok(None),
            Location::Local(p) => {
                (Source::File(BufReader::new(crate::geotiff::FileCursor::new(std::fs::File::open(p)?))), None, Some(p.clone()))
            }
            // A member of a zip archive, stored or seek-optimized: read in place, without
            // extracting the archive (remote archives by range requests).
            #[cfg(not(target_family = "wasm"))]
            Location::Zip { archive, inner: Some(inner) } => {
                if let Some(member) = crate::vsi::sozip::open_member(archive, inner.trim_start_matches('/'))? {
                    (Source::Zip(Box::new(member)), None, None)
                } else {
                    resolve_file()?
                }
            }
            _ => resolve_file()?,
        };
        if !is_tiff(&mut source) {
            return Ok(None);
        }
        let limits = limits()?;
        // A TIFF is read by the GeoTIFF reader whatever its name: only if the GTiff driver is
        // allowed. `LazyRaster::open` judges a local file's content before; a remote one's, and a
        // TIFF opened here directly, are judged here, after the first request.
        crate::formats::check_driver_allowed("GTiff", path, crate::formats::Kind::Raster)?;
        let mut t = TiffRaster::new(source, local, limits)?;
        t.files.push(path.to_string());
        if let Location::Local(p) = &loc
            && t.tags.apply_sidecar_crs(p)
        {
            t.files.push(format!("{path}.aux.xml"));
        }
        t.tags.warn_unreadable(path);
        t.file = file;
        // `TERRIO_DISABLE_READDIR_ON_OPEN=EMPTY_DIR` asks that nothing next to the file be read:
        // no `.ovr` either (see `LazyRaster::attach_managed_ovr`).
        if let Location::Local(p) = &loc
            && !crate::config::sidecars_disabled()
        {
            let mut ovr = p.as_os_str().to_owned();
            ovr.push(".ovr");
            t.attach_ovr(std::path::Path::new(&ovr))?;
        }
        Ok(Some(Self { inner: Inner::Tiff(Box::new(t)), sidecar_mask: None, asset_policy: crate::stac::AssetReadPolicy::current() }))
    }

    /// Opens a GeoTIFF held in memory, reading only its headers now. Returns `None` when the bytes
    /// are not a TIFF. Refused when it declares more than `TERRIO_MAX_DECLARED_BYTES` or
    /// `TERRIO_MAX_BANDS` allow, as [`LazyRaster::open`] refuses it.
    pub fn from_tiff_bytes(bytes: Vec<u8>) -> Result<Option<Self>> {
        Self::from_tiff_shared_bytes(std::sync::Arc::new(bytes))
    }

    /// [`LazyRaster::from_tiff_bytes`] with other decoding limits than the defaults.
    pub fn from_tiff_bytes_with_limits(bytes: Vec<u8>, limits: TiffReadLimits) -> Result<Option<Self>> {
        declared_allowed(Self::from_tiff_shared_bytes_with_limits(std::sync::Arc::new(bytes), limits))
    }

    /// Opens GeoTIFF bytes shared by immutable readers without copying the file buffer, refused as
    /// [`LazyRaster::from_tiff_bytes`] is.
    pub fn from_tiff_shared_bytes(bytes: std::sync::Arc<Vec<u8>>) -> Result<Option<Self>> {
        declared_allowed(Self::from_tiff_shared_bytes_using(bytes, TiffReadLimits::from_config))
    }

    /// [`LazyRaster::from_tiff_bytes`] whatever the GeoTIFF declares, for terrio's own bytes: the
    /// GeoTIFF an update of bytes wrote, read back to refresh its overviews (`ops::update`).
    pub(crate) fn from_tiff_bytes_unchecked(bytes: Vec<u8>) -> Result<Option<Self>> {
        Self::from_tiff_shared_bytes_using(std::sync::Arc::new(bytes), TiffReadLimits::from_config)
    }

    fn from_tiff_shared_bytes_with_limits(bytes: std::sync::Arc<Vec<u8>>, limits: TiffReadLimits) -> Result<Option<Self>> {
        Self::from_tiff_shared_bytes_using(bytes, || Ok(limits))
    }

    fn from_tiff_shared_bytes_using(
        bytes: std::sync::Arc<Vec<u8>>,
        limits: impl FnOnce() -> Result<TiffReadLimits>,
    ) -> Result<Option<Self>> {
        let mut source = Source::Memory(std::io::Cursor::new(SharedBytes(bytes)));
        if !is_tiff(&mut source) {
            return Ok(None);
        }
        let limits = limits()?;
        // Opened as a GeoTIFF by its caller: only if the GTiff driver is allowed.
        crate::formats::check_driver_allowed("GTiff", "a GeoTIFF in memory", crate::formats::Kind::Raster)?;
        let t = TiffRaster::new(source, None, limits)?;
        t.tags.warn_unreadable("a GeoTIFF in memory");
        Ok(Some(Self { inner: Inner::Tiff(Box::new(t)), sidecar_mask: None, asset_policy: crate::stac::AssetReadPolicy::current() }))
    }

    /// Opens a remote GeoTIFF whose bytes the caller fetches (see [`crate::sparse`]), reading
    /// only its headers. Reads fail until the bytes they need are fetched, and are then run
    /// again. Returns `None` when the file is not a TIFF. Refused when it declares more than
    /// `TERRIO_MAX_DECLARED_BYTES` or `TERRIO_MAX_BANDS` allow, as [`LazyRaster::open`] refuses it.
    pub fn from_sparse(file: crate::sparse::SparseFile) -> Result<Option<Self>> {
        declared_allowed(Self::from_sparse_using(file, TiffReadLimits::from_config))
    }

    /// Opens the array at `key` (`""` for the store's root: a group's single array) of a Zarr
    /// store whose objects the caller fetches (see [`crate::sparse::SparseObjects`]), named
    /// `name` in messages, read by window as a remote store is. A group requires consolidated
    /// metadata to discover its arrays; otherwise pass the array's key explicitly. Opening and
    /// reads fail until the objects they need are fetched, and are then run again. Refused when
    /// the array declares more than `TERRIO_MAX_DECLARED_BYTES` or `TERRIO_MAX_BANDS` allow.
    #[cfg(feature = "zarr")]
    pub fn from_zarr_objects(objects: std::sync::Arc<crate::sparse::SparseObjects>, name: &str, key: &str) -> Result<Self> {
        crate::formats::check_driver_allowed("Zarr", "a Zarr store", crate::formats::Kind::Raster)?;
        let r = crate::drivers::zarr::open_objects(objects, name, key)?;
        crate::declared::check_lazy(&r)?;
        Ok(r)
    }

    /// [`LazyRaster::from_sparse`] with other decoding limits than the defaults.
    pub fn from_sparse_with_limits(file: crate::sparse::SparseFile, limits: TiffReadLimits) -> Result<Option<Self>> {
        declared_allowed(Self::from_sparse_using(file, || Ok(limits)))
    }

    fn from_sparse_using(file: crate::sparse::SparseFile, limits: impl FnOnce() -> Result<TiffReadLimits>) -> Result<Option<Self>> {
        let mut source = Source::sparse(file);
        source.seek(SeekFrom::Start(0))?;
        let mut magic = [0u8; 4];
        source.read_exact(&mut magic)?;
        if !matches!(&magic, b"II*\0" | b"MM\0*" | b"II+\0" | b"MM\0+") {
            return Ok(None);
        }
        let limits = limits()?;
        // Opened as a GeoTIFF by its caller: only if the GTiff driver is allowed.
        crate::formats::check_driver_allowed("GTiff", "a remote GeoTIFF", crate::formats::Kind::Raster)?;
        source.seek(SeekFrom::Start(0))?;
        let t = TiffRaster::new(source, None, limits)?;
        t.tags.warn_unreadable("a remote GeoTIFF");
        Ok(Some(Self { inner: Inner::Tiff(Box::new(t)), sidecar_mask: None, asset_policy: crate::stac::AssetReadPolicy::current() }))
    }

    /// Opens an archive of image tiles (PMTiles or MBTiles) for reading by window, zoom levels
    /// as overviews. Refused when its full resolution declares more than
    /// `TERRIO_MAX_DECLARED_BYTES` or `TERRIO_MAX_BANDS` allow.
    #[cfg(feature = "tiles")]
    #[cfg_attr(docsrs, doc(cfg(feature = "tiles")))]
    pub fn from_tile_archive(archive: crate::tiles::TileArchive) -> Result<Self> {
        let r = Self::from_tiles(crate::tiles::raster::TileRaster::new(archive, None)?);
        crate::declared::check_lazy(&r)?;
        Ok(r)
    }

    /// The raster in memory, when it is not read on demand.
    pub fn dataset(&self) -> Option<&RasterDataset> {
        match &self.inner {
            Inner::Tiff(_) | Inner::Source(..) => None,
            Inner::Memory(ds) => Some(ds),
        }
    }

    /// The raster in memory, shared rather than copied, when it is not read on demand.
    pub fn shared_dataset(&self) -> Option<Arc<RasterDataset>> {
        if let Inner::Memory(ds) = &self.inner { Some(Arc::clone(ds)) } else { None }
    }

    /// The description and metadata of each band, for drivers that report them per band (GRIB);
    /// empty otherwise.
    pub fn band_metadata(&self) -> &[crate::BandMetadata] {
        match &self.inner {
            Inner::Memory(ds) | Inner::Source(ds, _) => &ds.band_metadata,
            Inner::Tiff(_) => &[],
        }
    }

    /// The nodata value of each band, when the bands declare different ones (GRIB); empty when
    /// every band has [`LazyRaster::nodata`]. [`LazyRaster::band_nodata`] gives one band's.
    pub fn band_nodata_values(&self) -> &[Option<f64>] {
        match &self.inner {
            Inner::Memory(ds) | Inner::Source(ds, _) => &ds.band_nodata,
            Inner::Tiff(_) => &[],
        }
    }

    /// Metadata domains other than the default one (see [`RasterDataset::metadata_domains`]).
    pub fn metadata_domains(&self) -> &[crate::dataset::MetadataDomain] {
        match &self.inner {
            Inner::Memory(ds) | Inner::Source(ds, _) => &ds.metadata_domains,
            Inner::Tiff(_) => &[],
        }
    }

    /// The color table of the first band (see [`RasterDataset::color_table`]); empty for
    /// GeoTIFFs, whose color map `terrio.rio` reads from the file's tags.
    pub fn color_table(&self) -> &[[u8; 4]] {
        match &self.inner {
            Inner::Memory(ds) | Inner::Source(ds, _) => &ds.color_table,
            Inner::Tiff(_) => &[],
        }
    }

    /// The RPCs of a GeoTIFF, as GDAL reads them: its `RPCCoefficientTag`, else (for a local
    /// file) a `_RPC.TXT` or `.RPB` sidecar. Also for GeoTIFFs in memory and sparse ones, which
    /// have the tag alone. `None` without RPCs, and for other formats.
    pub fn rpc(&self) -> Result<Option<crate::ops::rpc::Rpc>> {
        let Inner::Tiff(t) = &self.inner else { return Ok(None) };
        if let Some(tag) = &t.tags.rpc {
            return crate::ops::rpc::Rpc::from_tiff_tag(tag).map(Some);
        }
        match &t.file {
            Some(path) => crate::ops::rpc::rpc_sidecar(path),
            None => Ok(None),
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

    /// The last alpha sample, when a TIFF explicitly tags one (or a PNG has alpha).
    /// Uses the already open reader, including remote and sparse TIFFs.
    pub(crate) fn alpha_band(&mut self) -> Option<usize> {
        match &mut self.inner {
            Inner::Tiff(t) => t.alpha,
            Inner::Memory(ds) | Inner::Source(ds, _) if ds.driver == RasterDriver::PNG && matches!(ds.raster.bands, 2 | 4) => {
                Some(ds.raster.bands)
            }
            Inner::Source(_, s) => s.alpha_band(),
            _ => None,
        }
    }

    /// The pixel data type: for complex bands, the type of their real and imaginary parts (see
    /// [`LazyRaster::is_complex`]).
    pub fn data_type(&self) -> DataType {
        match &self.inner {
            Inner::Tiff(t) => t.data_type,
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.raster.data_type,
        }
    }

    /// Whether the bands hold complex numbers, as GDAL's `CInt16`, `CInt32`, `CFloat32` and
    /// `CFloat64` bands (see [`RasterDataset::complex`]): each is two of the
    /// [`LazyRaster::band_count`] bands, its real parts then its imaginary parts.
    pub fn is_complex(&self) -> bool {
        match &self.inner {
            Inner::Tiff(t) => t.complex,
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.complex,
        }
    }

    /// Exact integer nodata from native TIFF tags. PAM overrides remain f64 metadata.
    pub(crate) fn band_integer_nodata(&self, band: usize, overview: Option<usize>) -> Option<i128> {
        let Inner::Tiff(tiff) = &self.inner else { return None };
        let tags = tiff.tags_at_level(overview.map_or(0, |i| i + 1));
        if tags.band_nodata.iter().any(|(b, _)| *b == band) { None } else { tags.integer_nodata }
    }

    /// The declared band type, falling back to the shared raster type.
    pub(crate) fn band_data_type(&self, band: usize) -> DataType {
        match &self.inner {
            Inner::Source(_, source) => source.band_data_type(band).unwrap_or_else(|| self.data_type()),
            _ => self.data_type(),
        }
    }

    /// The bits of a sample as a TIFF stores them (its `BitsPerSample`); `None` for other
    /// sources. Samples may be read as a wider [`LazyRaster::data_type`], as GDAL reads them:
    /// Float16 (16 bits, read as Float32; GDAL's Float16) and unsigned samples of 1 to 7 or 9 to
    /// 31 bits (GDAL's `NBITS`).
    pub fn sample_bits(&self) -> Option<u16> {
        match &self.inner {
            Inner::Tiff(t) => Some(t.bits),
            Inner::Memory(_) | Inner::Source(..) => None,
        }
    }

    /// The bits of a sample of a level (full resolution, or an overview) as stored: an external
    /// overview's own (its `.ovr` file is a dataset of its own, with its own `NBITS`), else
    /// [`LazyRaster::sample_bits`].
    pub(crate) fn level_sample_bits(&self, overview: Option<usize>) -> Option<u16> {
        if let Inner::Tiff(t) = &self.inner
            && overview.and_then(|i| t.levels.get(i + 1)).is_some_and(|l| l.external.is_some())
            && let Some(ovr) = &t.ovr
        {
            return Some(ovr.bits);
        }
        self.sample_bits()
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

    /// Whether the file gives a geotransform, for a GeoTIFF: one without (no transformation
    /// matrix, or no tie point with a pixel scale) has [`LazyRaster::geo_transform`] `(0, 1, 0,
    /// height, 0, -1)`, which a georeferenced file can also have. `None` for other formats, whose
    /// fallback the caller tells apart (rio's `has_transform`), unlike
    /// [`LazyRaster::has_geo_transform`].
    pub fn geotiff_has_geo_transform(&self) -> Option<bool> {
        match &self.inner {
            Inner::Tiff(t) => Some(t.tags.corner_geo_transform().is_some()),
            Inner::Memory(_) | Inner::Source(..) => None,
        }
    }

    /// Whether the file has a geotransform of its own: false for an unreferenced or GCP-only
    /// GeoTIFF, whose [`geo_transform`](Self::geo_transform) is the unit-pixel fallback. An
    /// in-memory dataset always carries a pixel grid, so only a GCP-only one without an affine
    /// reports false.
    pub fn has_geo_transform(&self) -> bool {
        match &self.inner {
            Inner::Tiff(t) => t.tags.corner_geo_transform().is_some(),
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.gcps.is_empty() || ds.affine.is_some(),
        }
    }

    /// Marks `options` for writing this raster to `path` without a geotransform when it has none
    /// (an unreferenced GeoTIFF or JPEG 2000 file): `GEOREFERENCED=NO` for a GeoTIFF, COG, JPEG
    /// 2000, PNG, JPEG or WebP output, unless the options give a geotransform or a
    /// `GEOREFERENCED` of their own. Written without a CRS either, such an output has no
    /// georeferencing at all: no tie point, GeoJP2 box or world file declaring the unit-pixel grid
    /// [`geo_transform`](Self::geo_transform) gives it.
    pub fn keep_unreferenced(&self, options: &mut crate::RasterWriteOptions, path: &str) {
        crate::drivers::common::keep_unreferenced(options, path, self.geotransform_present());
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
            Inner::Tiff(t) => t.tags.metadata.clone(),
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.raster.metadata.clone(),
        }
    }

    /// The subdatasets declared in this raster's metadata, without opening the source again.
    /// Uses the same listing rules as `info` (reference GDAL 3.13.3). Driver-specific
    /// container listings such as GeoPackage tables still require `info_path`.
    pub fn subdatasets(&self) -> Vec<crate::info::Subdataset> {
        crate::info::subdatasets_from_metadata(&self.metadata())
    }

    /// The sizes `(width, height)` of the overviews, largest first.
    /// Returns an empty list when the retained asset-read policy refuses access.
    pub fn overviews(&self) -> Vec<(usize, usize)> {
        let Ok(_scope) = self.enter_asset_read() else { return Vec::new() };
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

    /// The rows strips of the full-resolution image are best made of (a multiple of), so that each
    /// block of the files it reads is read once: a GeoTIFF's block height, or a VRT's sources'.
    pub(crate) fn row_alignment(&mut self) -> Option<usize> {
        let _scope = self.enter_asset_read().ok()?;
        match &mut self.inner {
            Inner::Tiff(t) => Some(t.levels[0].block.1),
            Inner::Memory(_) => None,
            Inner::Source(_, s) => s.row_alignment(),
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
    /// as building a VRT. Dataset items describe the complete source and remain valid here.
    pub fn header(&self) -> RasterDataset {
        if let Inner::Memory(ds) | Inner::Source(ds, _) = &self.inner {
            let r = &ds.raster;
            let raster = wbraster::Raster {
                cols: r.cols,
                rows: r.rows,
                bands: r.bands,
                x_min: r.x_min,
                y_min: r.y_min,
                cell_size_x: r.cell_size_x,
                cell_size_y: r.cell_size_y,
                nodata: r.nodata,
                data_type: r.data_type,
                crs: r.crs.clone(),
                metadata: r.metadata.clone(),
                data: wbraster::raster::RasterData::new_filled(r.data_type, 0, 0.0),
            };
            // Spell out dataset fields too: additions to Raster or RasterDataset
            // must be handled here, and cannot silently disappear from headers.
            return RasterDataset {
                raster,
                driver: ds.driver,
                has_nodata: ds.has_nodata,
                band_nodata: ds.band_nodata.clone(),
                band_metadata: ds.band_metadata.clone(),
                gcps: ds.gcps.clone(),
                affine: ds.affine,
                geotransform_present: Some(self.geotransform_present()),
                complex: ds.complex,
                geolocation: ds.geolocation.clone(),
                metadata_domains: ds.metadata_domains.clone(),
                color_table: ds.color_table.clone(),
            };
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
            metadata: self.metadata(),
            data: wbraster::raster::RasterData::new_filled(self.data_type(), 0, 0.0),
        };
        let mut ds = RasterDataset::with_driver(raster, self.driver(), self.nodata().is_some());
        ds.complex = self.is_complex();
        ds.band_nodata = (1..=self.band_count()).map(|b| self.band_nodata(b)).collect();
        let first_nodata = ds.band_nodata.first().copied().flatten();
        ds.has_nodata = first_nodata.is_some();
        ds.raster.nodata = first_nodata.unwrap_or(-9999.);
        ds.set_geo_transform(self.geo_transform());
        ds.geotransform_present = Some(self.geotransform_present());
        if let Inner::Tiff(t) = &self.inner {
            ds.band_metadata = t.tags.band_properties.clone();
            ds.color_table = t.tags.color_table.clone();
            ds.gcps = t.tags.gcps.clone();
        }
        ds
    }

    /// Reads the whole full-resolution image. An image that the process cannot hold (more than
    /// [`crate::usable_memory`]) is refused with [`Error::ResourceLimit`] before it is allocated,
    /// as any of its windows is.
    pub fn read(&mut self) -> Result<RasterDataset> {
        let window = Window::full(self.width(), self.height());
        let mut ds = self.read_window(window, &WindowOptions::default())?;
        // The whole image of a source read whole with its dataset's metadata items (which a
        // window leaves out), as its driver reads it eagerly.
        if let Inner::Source(header, source) = &self.inner
            && source.whole_read_keeps_metadata()
        {
            ds.raster.metadata = header.raster.metadata.clone();
        }
        Ok(ds)
    }

    /// Whether [`LazyRaster::write`] can stream `self` to a tile archive (`PMTiles` or `MBTiles`)
    /// at `path` without reading the whole raster into memory first.
    pub fn can_stream_tile_archive(&self, path: &str, options: &crate::RasterWriteOptions) -> bool {
        let driver = options.driver.or_else(|| RasterDriver::for_extension(path));
        self.dataset().is_none()
            && matches!(driver, Some(RasterDriver::PMTiles | RasterDriver::MBTiles))
            && self.input_paths().is_ok_and(|inputs| !crate::ops::strips::overwrites(path, &inputs))
    }

    /// Writes the full-resolution image to `path`, as [`crate::write_raster_dataset`] writes the
    /// raster [`LazyRaster::read`] reads. A GeoTIFF or JPEG 2000 file read in place, or a VRT of
    /// such files copied or picked by nearest neighbour, written as a GeoTIFF or a COG (in a
    /// compression terrio's own writer encodes) is read and written a strip of rows at a time,
    /// so that it is never held whole; a PMTiles or MBTiles archive is built a tile row at a
    /// time (and a raster already on its zoom level's Web Mercator pixel grid read a strip at a
    /// time); other rasters are read whole, then written.
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
        let context = crate::stac::AssetReadContext::capture(self.asset_policy.clone());
        context.check()?;
        let driver = options.driver.or_else(|| RasterDriver::for_extension(path));
        self.refuse_masked_tile_archive(driver, options)?;
        let source_mask = options.mask.is_none() && self.has_mask() && matches!(driver, Some(RasterDriver::GTiff | RasterDriver::Cog));
        // Tile archives are built a tile row at a time (see `crate::tiles`). A source that cannot
        // list the files it reads, or an output replacing one of them, is read whole, as before.
        #[cfg(all(feature = "tiles", not(target_family = "wasm")))]
        if let Some(driver @ (RasterDriver::PMTiles | RasterDriver::MBTiles)) = options.driver.or_else(|| RasterDriver::for_extension(path))
            && self.input_paths().is_ok_and(|inputs| !crate::ops::strips::overwrites(path, &inputs))
        {
            return crate::tiles::write_raster_lazy(self, path, driver, options);
        }
        let mut with_block;
        let options = if options.source_block_size.is_none() && self.block_size().is_some() {
            with_block = options.clone();
            with_block.source_block_size = self.block_size();
            &with_block
        } else {
            options
        };
        let (width, height, bands) = (self.width(), self.height(), self.band_count());
        let value_size = self.data_type().size_bytes();
        let block = crate::ops::strips::bounded_alignment(width, bands, value_size, self.row_alignment());
        if let Inner::Source(ds, s) = &self.inner
            && let Some(s) = s.streamed()
            && !source_mask
            && !crate::ops::strips::overwrites(path, &s.checked_inputs()?)
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
                    let _scope = context.enter()?;
                    Ok((s.read_rows(Window::new(0, y, width, h), &all)?, h))
                },
                |_, (data, h)| out.write(data, h),
            )?;
            return out.finish();
        }
        // An output that replaces the input is written from the input read whole.
        let requires_inventory =
            matches!(&self.inner, Inner::Tiff(_)) || matches!(&self.inner, Inner::Source(_, source) if source.streamed().is_some());
        let can_stream = if requires_inventory {
            !crate::ops::strips::overwrites(path, &self.input_paths()?)
        } else {
            self.input_paths().is_ok_and(|inputs| !crate::ops::strips::overwrites(path, &inputs))
        };
        let can_stream = can_stream
            && match &self.inner {
                Inner::Tiff(t) => t.file.is_some() || t.zip.is_some(),
                Inner::Source(_, s) => source_mask || s.reads_by_window(),
                _ => false,
            };
        if !can_stream {
            let mut options = options.clone();
            self.prepare_output_mask(path, &mut options, Window::full(width, height), None)?;
            return crate::write_raster_dataset(&self.read()?, path, &options);
        }
        // The header the whole raster is read with.
        let mut header = self.header();
        if let Inner::Tiff(t) = &self.inner {
            header.raster.crs = t.tags.crs_info();
            header.raster.metadata = t.tags.metadata.clone();
            header.band_metadata = t.tags.band_properties.clone();
            header.color_table = t.tags.color_table.clone();
            header.gcps = t.tags.gcps.clone();
        }
        // A source read by window keeps its dataset's metadata items as its whole read does
        // (see `LazyRaster::read`): `AREA_OR_POINT` alone, unless its driver reads them whole.
        // A masked source has long been written with all of them, and still is.
        if let Inner::Source(_, source) = &self.inner
            && !source_mask
            && !source.whole_read_keeps_metadata()
        {
            header.raster.metadata.retain(|(k, _)| k == "AREA_OR_POINT");
        }
        let rows = crate::ops::strips::strip_rows(width, bands, value_size, block);
        let mut out = crate::ops::strips::StripWriter::create_masked(&header, path, options, source_mask)?;
        let read = WindowOptions::default();
        crate::ops::strips::pipelined(
            height.div_ceil(rows),
            |i| {
                let y = i * rows;
                let h = rows.min(height - y);
                let window = Window::new(0, y, width, h);
                let mask = if source_mask { self.read_mask(window, None)? } else { None };
                Ok((self.read_window(window, &read)?.raster.data, h, mask))
            },
            |_, (data, h, mask)| out.write_masked(data, h, mask),
        )?;
        out.finish()
    }

    /// Fetches the byte ranges of the chunks a window read needs, for sources whose bytes the
    /// caller fetches (see [`crate::sparse`]). The next read of the window then decodes without
    /// stopping at its first missing chunk. A no-op for sources read locally.
    ///
    /// # Errors
    ///
    /// When the window or band is invalid, or the source is a sparse one that still misses some
    /// of the chunks: fetch them with [`crate::sparse::SparseFile::missing`] and try again.
    pub fn prefetch_window(&mut self, window: Window, options: &WindowOptions) -> Result<()> {
        self.prefetch_windows(&[window], options)
    }

    /// Prefetches one bounded batch of the windows the sampler actually reads.
    pub(crate) fn prefetch_windows(&mut self, windows: &[Window], options: &WindowOptions) -> Result<()> {
        let _scope = self.asset_policy.as_ref().map(crate::stac::AssetReadPolicy::enter).transpose()?;
        let (lw, lh) = self.level_size(options.overview)?;
        for window in windows {
            window.check(lw, lh)?;
        }
        let bands = read_bands(&options.bands, self.band_count())?;
        match &mut self.inner {
            Inner::Tiff(t) => t.prefetch_windows(options.overview.map_or(0, |i| i + 1), windows, &bands),
            _ => Ok(()),
        }
    }

    fn prefetch_mask_windows(&mut self, windows: &[Window], overview: Option<usize>) -> Result<()> {
        let _scope = self.asset_policy.as_ref().map(crate::stac::AssetReadPolicy::enter).transpose()?;
        let (lw, lh) = self.level_size(overview)?;
        for window in windows {
            window.check(lw, lh)?;
        }
        if let Some(m) = self.sidecar_mask.as_mut() {
            return prefetch_sidecar_masks(m, windows, (lw, lh));
        }
        if let Inner::Tiff(t) = &mut self.inner {
            t.prefetch_mask_windows(overview.map_or(0, |i| i + 1), windows)?;
        }
        Ok(())
    }

    pub(crate) fn prefetch_mask_band_window(&mut self, window: Window, overview: Option<usize>) -> Result<()> {
        self.prefetch_mask_band_windows(&[window], overview)
    }

    /// Prefetches the own-mask windows selected by the mask sampler, without filling gaps
    /// between distant sampled blocks.
    pub(crate) fn prefetch_mask_band_windows(&mut self, windows: &[Window], overview: Option<usize>) -> Result<()> {
        let _scope = self.asset_policy.as_ref().map(crate::stac::AssetReadPolicy::enter).transpose()?;
        let Some(j) = overview else {
            return self.prefetch_mask_windows(windows, None);
        };
        let Some(size) = self.mask_band_overviews().get(j).copied() else {
            return Ok(());
        };
        if let Some(m) = self.sidecar_mask.as_mut() {
            return prefetch_sidecar_own_masks(m, windows, size);
        }
        let Some(level) = self.overviews().iter().position(|s| *s == size) else {
            return Ok(());
        };
        if let Inner::Tiff(t) = &mut self.inner {
            t.prefetch_own_mask_windows(level + 1, windows)?;
        }
        Ok(())
    }

    /// Reads a window of some bands, at full resolution or from an overview, as a raster
    /// georeferenced to the window.
    ///
    /// Pixels are band-major, each band row-major from the top, regardless of the file's
    /// interleave: `data[band * (height * width) + row * width + col]` (zero-based indices,
    /// returned window dimensions). Planes follow the requested band order. This also applies
    /// to internal and external overviews; no pixel reordering is needed before a warp.
    /// The result does not retain the dataset mask. Read it separately with [`Self::read_mask`]
    /// using the same window and overview selection when needed. A window the process
    /// cannot hold (more than [`crate::usable_memory`]) is refused with [`Error::ResourceLimit`]
    /// before it is allocated.
    pub fn read_window(&mut self, window: Window, options: &WindowOptions) -> Result<RasterDataset> {
        let _scope = self.asset_policy.as_ref().map(crate::stac::AssetReadPolicy::enter).transpose()?;
        let (lw, lh) = self.level_size(options.overview)?;
        window.check(lw, lh)?;
        let bands = read_bands(&options.bands, self.band_count())?;
        // Repeated bands can make even a small in-memory raster's output exceed usable memory.
        // Check the returned buffer before dispatching to any reader or allocating band metadata.
        crate::dataset::fallible::check_raster_buffer("a window", window.width, window.height, bands.len(), self.data_type())?;
        let band_nodata = self.level_band_nodata(&bands, options.overview);
        let gt = self.level_geo_transform(options.overview)?;
        let data = match &mut self.inner {
            Inner::Tiff(t) => t.read(options.overview.map_or(0, |i| i + 1), window, &bands)?,
            Inner::Memory(ds) => {
                let r = &ds.raster;
                if r.cols.checked_mul(r.rows).and_then(|n| n.checked_mul(r.bands)) != Some(r.data.len()) {
                    return Err(Error::InvalidData(format!(
                        "the raster holds {} values for {} band(s) of {} x {} pixels",
                        r.data.len(),
                        r.bands,
                        r.cols,
                        r.rows
                    )));
                }
                cut(r, window, &bands)
            }
            Inner::Source(_, s) => s.read(options.overview.map_or(0, |i| i + 1), window, &bands)?,
        };
        let (x, y) = (window.x as f64, window.y as f64);
        let geo_transform = [gt[0] + x * gt[1] + y * gt[2], gt[1], gt[2], gt[3] + x * gt[4] + y * gt[5], gt[4], gt[5]];
        let (full_cols, full_rows) = (self.width() as f64, self.height() as f64);
        let gcp = |pixel: f64, line: f64| (pixel * lw as f64 / full_cols - x, line * lh as f64 / full_rows - y);
        let whole = options.overview.is_none() && window == Window::full(lw, lh);
        self.window_dataset(&bands, band_nodata, data, (window.width, window.height), geo_transform, gcp, whole)
    }

    /// The nodata values of some bands (numbered from 1) at full resolution or of an overview,
    /// whose own tags a GeoTIFF's overview may carry.
    pub fn level_band_nodata(&self, bands: &[usize], overview: Option<usize>) -> Vec<Option<f64>> {
        if let Inner::Tiff(t) = &self.inner {
            let tags = t.tags_at_level(overview.map_or(0, |i| i + 1));
            bands.iter().map(|b| tags.band_nodata.iter().find(|(i, _)| i == b).map(|(_, v)| *v).or(tags.nodata)).collect()
        } else {
            bands.iter().map(|b| self.band_nodata(*b)).collect()
        }
    }

    /// The dataset of a read of some bands (numbered from 1): their pixels `data` on a grid of
    /// `cols` × `rows` georeferenced by `geo_transform`, with the raster's description of those
    /// bands, their nodata values `band_nodata`, and its GCPs moved onto the grid by `gcp`
    /// (`(pixel, line)` to `(pixel, line)`). Band statistics are kept only when the read is the
    /// `whole` full-resolution image.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn window_dataset(
        &self,
        bands: &[usize],
        band_nodata: Vec<Option<f64>>,
        data: RasterData,
        (cols, rows): (usize, usize),
        geo_transform: [f64; 6],
        gcp: impl Fn(f64, f64) -> (f64, f64),
        whole: bool,
    ) -> Result<RasterDataset> {
        let first_nodata = band_nodata.first().copied().flatten();
        let crs = match &self.inner {
            Inner::Tiff(t) => t.tags.crs_info(),
            Inner::Memory(ds) | Inner::Source(ds, _) => ds.raster.crs.clone(),
        };
        let cfg = wbraster::RasterConfig {
            cols,
            rows,
            bands: bands.len(),
            nodata: first_nodata.unwrap_or(-9999.),
            data_type: data.data_type(),
            crs,
            // Source-wide statistics and other dataset items do not describe a subset.
            metadata: self.metadata().into_iter().filter(|(k, _)| k == "AREA_OR_POINT").collect(),
            ..Default::default()
        };
        let raster = crate::pixel::raster_from_data(cfg, data)?;
        let mut ds = RasterDataset::with_driver(raster, self.driver(), first_nodata.is_some());
        ds.set_geo_transform(geo_transform);
        ds.geotransform_present = Some(self.geotransform_present());
        let (gcps, color_table) = match &self.inner {
            Inner::Tiff(t) => (&t.tags.gcps, &t.tags.color_table),
            Inner::Memory(source) | Inner::Source(source, _) => (&source.gcps, &source.color_table),
        };
        ds.gcps = gcps
            .iter()
            .cloned()
            .map(|mut g| {
                (g.pixel, g.line) = gcp(g.pixel, g.line);
                g
            })
            .collect();
        ds.band_metadata = self.bands_metadata(bands);
        ds.band_nodata = band_nodata;
        ds.complex = self.reads_complex(bands);
        if bands == [1] {
            ds.color_table = color_table.clone();
        }
        if !whole {
            for band in &mut ds.band_metadata {
                band.items.retain(|(key, _)| !key.starts_with("STATISTICS_"));
            }
        }
        Ok(ds)
    }

    /// Whether a window of the raster reads only the parts of it the window covers and gives the
    /// values a whole read gives them: a GeoTIFF (read in place or from memory), a JPEG 2000 file,
    /// or a VRT of such files copied or picked by nearest neighbour. Decoded rasters in memory,
    /// and sources that decode the whole raster for any window, are not. [`LazyRaster::write`]
    /// writes those that are a strip of rows at a time (a GeoTIFF only when read from a file).
    pub fn reads_by_window(&self) -> bool {
        match &self.inner {
            Inner::Tiff(_) => true,
            Inner::Source(_, s) => s.reads_by_window(),
            Inner::Memory(_) => false,
        }
    }

    /// Whether a read of some bands (numbered from 1) gives complex bands: whole complex bands
    /// (a band's real then imaginary parts), which are read as complex bands.
    fn reads_complex(&self, bands: &[usize]) -> bool {
        self.is_complex() && bands.len().is_multiple_of(2) && bands.as_chunks::<2>().0.iter().all(|p| p[0] % 2 == 1 && p[1] == p[0] + 1)
    }

    /// The description and metadata of some bands (numbered from 1) as a read of the whole image
    /// gives them, band statistics included: per complex band for whole complex bands.
    pub(crate) fn bands_metadata(&self, bands: &[usize]) -> Vec<crate::BandMetadata> {
        let band_metadata = match &self.inner {
            Inner::Tiff(t) => &t.tags.band_properties,
            Inner::Memory(source) | Inner::Source(source, _) => &source.band_metadata,
        };
        if self.reads_complex(bands) {
            return bands.as_chunks::<2>().0.iter().map(|p| band_metadata.get((p[0] - 1) / 2).cloned().unwrap_or_default()).collect();
        }
        bands.iter().map(|&b| band_metadata.get(b - 1).cloned().unwrap_or_default()).collect()
    }

    /// The tile (or strip) size `(width, height)` of the full-resolution image or of an overview,
    /// when the format reads by blocks.
    pub(crate) fn level_block_size(&self, overview: Option<usize>) -> Option<(usize, usize)> {
        match &self.inner {
            Inner::Tiff(t) => t.levels.get(overview.map_or(0, |i| i + 1)).map(|l| l.block),
            Inner::Memory(_) => None,
            Inner::Source(_, s) => s.block_size(),
        }
    }

    /// Whether the first band has a color table (a paletted raster), which GDAL resamples by
    /// nearest neighbour only.
    pub(crate) fn has_color_table(&self) -> bool {
        match &self.inner {
            Inner::Tiff(t) => !t.tags.color_table.is_empty(),
            Inner::Memory(ds) | Inner::Source(ds, _) => !ds.color_table.is_empty(),
        }
    }

    /// Reads the pixels at columns `cols` of rows `rows` (each in any order, repeated or not) of
    /// some bands, at full resolution or from an overview, as nearest-neighbour resampling picks
    /// them: `(bands, rows, cols)`, band-major, as stored. Read, as GDAL reads them, from the
    /// blocks that hold them alone, by the formats whose reader can (Zarr: one chunk at a time,
    /// rather than a window spanning billions of columns); `None` for the others, whose callers
    /// read the window spanning the pixels.
    pub fn read_sampled(&mut self, cols: &[usize], rows: &[usize], options: &WindowOptions) -> Result<Option<RasterData>> {
        let _scope = self.asset_policy.as_ref().map(crate::stac::AssetReadPolicy::enter).transpose()?;
        let (lw, lh) = self.level_size(options.overview)?;
        if let Some(c) = cols.iter().find(|c| **c >= lw) {
            return Err(Error::InvalidArgument(format!("column {c} is outside the {lw}x{lh} image")));
        }
        if let Some(r) = rows.iter().find(|r| **r >= lh) {
            return Err(Error::InvalidArgument(format!("row {r} is outside the {lw}x{lh} image")));
        }
        let bands = read_bands(&options.bands, self.band_count())?;
        match &mut self.inner {
            Inner::Source(_, s) => s.read_sampled(options.overview.map_or(0, |i| i + 1), cols, rows, &bands),
            Inner::Tiff(_) | Inner::Memory(_) => Ok(None),
        }
    }
}

impl LazyRaster {
    /// Carries a materialized operation result and its full-resolution validity mask together.
    pub(crate) fn from_masked_dataset(dataset: RasterDataset, mask: Vec<u8>) -> Result<Self> {
        let mut raster = Self::from(dataset);
        if raster.width().checked_mul(raster.height()) != Some(mask.len()) {
            return Err(Error::InvalidArgument("mask must contain one byte per pixel".into()));
        }
        let dataset = crate::ops::derived(&raster.header(), wbraster::raster::RasterData::U8(mask), 1)?;
        raster.sidecar_mask = Some(Box::new(Self::from(dataset)));
        Ok(raster)
    }

    /// Whether the raster has a per-dataset mask (GDAL's `GMF_PER_DATASET`): a GeoTIFF's
    /// internal mask, or a `.msk` sidecar.
    pub fn has_mask(&self) -> bool {
        self.sidecar_mask.is_some()
            || match &self.inner {
                Inner::Source(_, source) => source.has_mask(),
                Inner::Tiff(t) => mask_for_image(&t.masks, &t.levels[0]).is_some(),
                _ => false,
            }
    }

    /// Tile archive writers cannot retain a dataset mask or an explicit one.
    fn refuse_masked_tile_archive(&self, driver: Option<RasterDriver>, options: &crate::RasterWriteOptions) -> Result<()> {
        if matches!(driver, Some(RasterDriver::PMTiles | RasterDriver::MBTiles)) && (self.has_mask() || options.mask.is_some()) {
            return Err(Error::InvalidArgument("tile archive writes do not preserve dataset masks; use an explicit alpha band".into()));
        }
        Ok(())
    }

    /// Prepares inherited validity for an output window. GeoTIFF/COG retain
    /// per-dataset masks unless an explicit mask is supplied. Tile archives
    /// reject masked sources because their writers cannot retain that validity.
    /// Other formats keep their existing pixel/alpha/nodata serialization.
    pub fn prepare_output_mask(
        &mut self,
        filename: &str,
        options: &mut crate::RasterWriteOptions,
        window: Window,
        overview: Option<usize>,
    ) -> Result<()> {
        use crate::formats::RasterDriver;
        let driver = options.driver.or_else(|| RasterDriver::for_extension(filename));
        self.refuse_masked_tile_archive(driver, options)?;
        if options.mask.is_none() && matches!(driver, Some(RasterDriver::GTiff | RasterDriver::Cog)) {
            options.mask = self.read_mask(window, overview)?;
        }
        Ok(())
    }

    /// Reads a window of the per-dataset mask, at full resolution or of an overview: 255 for
    /// valid pixels, 0 for invalid ones, row by row. `None` when the raster has no mask (for an
    /// overview without a mask of its own, the full-resolution mask is sampled by nearest
    /// neighbour, as GDAL does).
    pub fn read_mask(&mut self, window: Window, overview: Option<usize>) -> Result<Option<Vec<u8>>> {
        let _scope = self.asset_policy.as_ref().map(crate::stac::AssetReadPolicy::enter).transpose()?;
        let (lw, lh) = self.level_size(overview)?;
        window.check(lw, lh)?;
        // An external overview is its own TIFF dataset. GDAL does not inherit
        // the base TIFF's mask when that dataset has no mask of its own.
        if let Inner::Tiff(t) = &mut self.inner
            && t.levels[overview.map_or(0, |i| i + 1)].external.is_some()
        {
            return read_tiff_mask(t, window, overview.map_or(0, |i| i + 1));
        }
        if let Some(m) = self.sidecar_mask.as_mut() {
            return read_sidecar_mask(m, window, (lw, lh)).map(Some);
        }
        if let Inner::Source(_, source) = &mut self.inner {
            return source.read_mask(overview.map_or(0, |i| i + 1), window);
        }
        let Inner::Tiff(t) = &mut self.inner else { return Ok(None) };
        read_tiff_mask(t, window, overview.map_or(0, |i| i + 1))
    }

    /// Whether overview `overview` (from 0) has a mask image of its own (GDAL's internal mask
    /// of an overview), rather than none. External `.ovr` levels are excluded; use
    /// [`Self::level_has_own_mask`] to include their own masks.
    pub fn has_overview_mask(&self, overview: usize) -> bool {
        let Inner::Tiff(t) = &self.inner else { return false };
        let Some(l) = t.levels.get(overview + 1) else { return false };
        l.external.is_none() && mask_for_image(&t.masks, l).is_some()
    }

    /// Reads a window of the per-dataset mask of the full-resolution image or of an overview as
    /// GDAL's band of that level has it (`GetMaskBand()`), which weights its resampled reads: an
    /// overview's own mask (an internal mask overview, the sidecar's overview of its size), else
    /// `None`, all valid, as is the full-resolution image without a mask. Unlike
    /// [`LazyRaster::read_mask`], an overview without a mask of its own does not sample the
    /// full-resolution mask.
    pub fn read_level_mask(&mut self, window: Window, overview: Option<usize>) -> Result<Option<Vec<u8>>> {
        let _scope = self.asset_policy.as_ref().map(crate::stac::AssetReadPolicy::enter).transpose()?;
        let Some(i) = overview else { return self.read_mask(window, None) };
        let (lw, lh) = self.level_size(overview)?;
        window.check(lw, lh)?;
        if let Inner::Tiff(t) = &mut self.inner
            && t.levels[i + 1].external.is_some()
        {
            return read_tiff_own_mask(t, window, i + 1);
        }
        if let Some(m) = self.sidecar_mask.as_mut() {
            return read_sidecar_own_mask(m, window, (lw, lh));
        }
        match &mut self.inner {
            Inner::Source(_, source) if source.level_has_own_mask(i + 1) => source.read_own_mask(i + 1, window),
            Inner::Source(..) => Ok(None),
            Inner::Tiff(t) => read_tiff_own_mask(t, window, i + 1),
            Inner::Memory(_) => Ok(None),
        }
    }

    /// Whether overview `overview` has a per-dataset mask of its own, which
    /// [`LazyRaster::read_level_mask`] reads, rather than none.
    pub fn level_has_own_mask(&self, overview: usize) -> bool {
        let Some(&size) = self.overviews().get(overview) else { return false };
        if let Inner::Tiff(t) = &self.inner
            && let Some(i) = t.levels[overview + 1].external
        {
            return match (&t.ovr_mask, &t.ovr) {
                (Some(m), _) => (m.width(), m.height()) == size || m.overviews().contains(&size),
                (None, Some(ovr)) => mask_for_image(&ovr.masks, &ovr.levels[i]).is_some(),
                (None, None) => false,
            };
        }
        if let Some(m) = &self.sidecar_mask {
            return m.overviews().contains(&size);
        }
        match &self.inner {
            Inner::Tiff(_) => self.has_overview_mask(overview),
            Inner::Source(_, s) => s.level_has_own_mask(overview + 1),
            Inner::Memory(_) => false,
        }
    }

    /// Whether GDAL's band of overview `overview` reports the mask flags of full resolution when
    /// it has no mask of its own (an internal overview), rather than its own (an external
    /// overview, a dataset of its own).
    pub(crate) fn level_keeps_mask_flags(&self, overview: usize) -> bool {
        match &self.inner {
            Inner::Tiff(t) => !t.levels.get(overview + 1).is_some_and(|l| l.external.is_some()),
            Inner::Source(_, s) => s.level_keeps_mask_flags(overview + 1),
            Inner::Memory(_) => true,
        }
    }

    /// The overviews (numbered as [`LazyRaster::overviews`]) of GDAL's band of a level, which
    /// `RasterIO` chooses among when it reads that band into a smaller buffer: every overview for
    /// full resolution. The first level of an external `.ovr` is that file's image, whose band
    /// has the file's other images as overviews, in their order there (GDAL's
    /// `GDALDefaultOverviews::GetOverview` in GDAL 3.13.3); other overviews have none.
    pub(crate) fn level_overviews(&self, overview: Option<usize>) -> Vec<usize> {
        let Some(i) = overview else { return (0..self.overviews().len()).collect() };
        match &self.inner {
            Inner::Tiff(t) if t.levels.get(i + 1).is_some_and(|l| l.external == Some(0)) => {
                let mut own: Vec<(usize, usize)> =
                    t.levels[1..].iter().enumerate().filter_map(|(j, l)| l.external.filter(|&k| k > 0).map(|_| (l.ifd, j))).collect();
                own.sort_unstable();
                own.into_iter().map(|(_, j)| j).collect()
            }
            Inner::Source(_, s) => s.level_band_overviews(i + 1),
            Inner::Tiff(_) | Inner::Memory(_) => Vec::new(),
        }
    }

    /// The overviews of GDAL's per-dataset mask band (a band's `GetMaskBand()` when the dataset
    /// has a mask), largest first: the sidecar's overviews, or the levels of a GeoTIFF with an
    /// internal mask of their own. A mask read into another size chooses among these, and reads
    /// the full-resolution mask when there are none.
    pub(crate) fn mask_band_overviews(&self) -> Vec<(usize, usize)> {
        if let Some(m) = &self.sidecar_mask {
            return m.overviews();
        }
        match &self.inner {
            Inner::Tiff(t) => (0..t.levels.len() - 1)
                .filter(|&i| t.levels[i + 1].external.is_none() && self.has_overview_mask(i))
                .map(|i| (t.levels[i + 1].width, t.levels[i + 1].height))
                .collect(),
            Inner::Source(_, s) => s.mask_band_overviews(),
            _ => Vec::new(),
        }
    }

    /// The bits of a sample of the per-dataset mask band as stored (GDAL's `NBITS`): 1 for a
    /// GeoTIFF's internal mask, the sidecar's bits; `None` for other sources.
    pub(crate) fn mask_band_bits(&self) -> Option<u16> {
        match (&self.sidecar_mask, &self.inner) {
            (Some(m), _) => m.sample_bits(),
            (None, Inner::Tiff(t)) if !t.masks.is_empty() => Some(1),
            (None, Inner::Source(_, s)) => s.mask_band_bits(),
            _ => None,
        }
    }

    /// Reads a window of GDAL's per-dataset mask band at full resolution or of its overview
    /// `overview` (numbered in [`LazyRaster::mask_band_overviews`]): 255 for valid pixels, 0 for
    /// invalid ones.
    pub(crate) fn read_mask_band(&mut self, window: Window, overview: Option<usize>) -> Result<Vec<u8>> {
        let all_valid = || vec![255; window.width * window.height];
        let Some(j) = overview else { return Ok(self.read_mask(window, None)?.unwrap_or_else(all_valid)) };
        let size = *self.mask_band_overviews().get(j).ok_or_else(|| Error::InvalidArgument(format!("no mask overview {j}")))?;
        if let Some(m) = self.sidecar_mask.as_mut() {
            return read_sidecar_own_mask(m, window, size)?.ok_or_else(|| Error::InvalidData("missing sidecar mask overview".into()));
        }
        let level = self
            .overviews()
            .iter()
            .position(|s| *s == size)
            .ok_or_else(|| Error::InvalidData(format!("no overview of the mask overview's size {size:?}")))?;
        Ok(self.read_level_mask(window, Some(level))?.unwrap_or_else(all_valid))
    }

    /// The first overview's `RESAMPLING` item, which GDAL writes with the overviews it builds
    /// (`GTIFFBuildOverviewMetadata`), if any.
    pub fn overview_resampling(&mut self) -> Option<String> {
        self.level_resampling(0)
    }

    /// A candidate overview's `RESAMPLING`, used by GDAL 3.13.3's best-overview selection.
    pub(crate) fn level_resampling(&mut self, overview: usize) -> Option<String> {
        let _scope = self.enter_asset_read().ok()?;
        if let Inner::Source(_, source) = &mut self.inner {
            return source.level_resampling(overview);
        }
        let Inner::Tiff(t) = &mut self.inner else { return None };
        let level = t.levels.get(overview + 1)?;
        if let Some(value) = &level.resampling {
            return value.clone();
        }
        let (external, ifd) = (level.external, level.ifd);
        let value = (|| {
            // An external overview uses its own TIFF dataset's metadata.
            let t = match (external, t.ovr.as_mut()) {
                (Some(i), Some(ovr)) => {
                    let ifd = ovr.levels.get(i)?.ifd;
                    ovr.decoder.seek_to_image(ifd).ok()?;
                    ovr.current_ifd = ifd;
                    ovr
                }
                _ => {
                    t.decoder.seek_to_image(ifd).ok()?;
                    t.current_ifd = ifd;
                    &mut *t
                }
            };
            overview_resampling_metadata(&mut t.decoder).ok()
        })()?;
        t.levels[overview + 1].resampling = Some(value.clone());
        value
    }

    /// A raster read by window by `source`, described by `header` (a dataset without pixels).
    pub(crate) fn from_source(header: RasterDataset, source: Box<dyn WindowSource>) -> Self {
        Self { inner: Inner::Source(Arc::new(header), source), sidecar_mask: None, asset_policy: crate::stac::AssetReadPolicy::current() }
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
        Self { inner: Inner::Memory(Arc::new(ds)), sidecar_mask: None, asset_policy: crate::stac::AssetReadPolicy::current() }
    }
}

/// Prefer matching flags; retain a unique size-matching legacy first-image mask when flags disagree.
fn mask_for_image<'a>(masks: &'a [crate::tiff_mask::MaskImage], image: &Level) -> Option<&'a crate::tiff_mask::MaskImage> {
    let same_size = |m: &&crate::tiff_mask::MaskImage| (m.width, m.height) == (image.width, image.height);
    masks.iter().filter(same_size).find(|m| m.overview == image.reduced).or_else(|| {
        if image.ifd != 0 || !image.reduced {
            return None;
        }
        let mut matches = masks.iter().filter(same_size);
        let first = matches.next()?;
        matches.next().is_none().then_some(first)
    })
}

/// A TIFF level's own mask, then its full-resolution mask as a nearest fallback.
fn read_tiff_mask(t: &mut TiffRaster, window: Window, level: usize) -> Result<Option<Vec<u8>>> {
    if let Some(i) = t.levels[level].external
        && let Some(ovr) = t.ovr.as_mut()
    {
        if let Some(mask) = t.ovr_mask.as_mut() {
            return read_sidecar_mask(mask, window, (ovr.levels[i].width, ovr.levels[i].height)).map(Some);
        }
        return read_tiff_mask(ovr, window, i);
    }
    let (lw, lh) = (t.levels[level].width, t.levels[level].height);
    let (fw, fh) = (t.levels[0].width, t.levels[0].height);
    let w = (window.x, window.y, window.width, window.height);
    let limit = t.limits.chunk_bytes;
    // The masks are borrowed, not copied with their chunk tables.
    let own = mask_for_image(&t.masks, &t.levels[level]);
    let reader = t.decoder.inner();
    if let Some(m) = own {
        return crate::tiff_mask::read_mask(reader, m, w, limit).map(Some);
    }
    let Some(full) = mask_for_image(&t.masks, &t.levels[0]) else { return Ok(None) };
    // Nearest neighbour from the full-resolution mask.
    let (sx, sy) = (fw as f64 / lw as f64, fh as f64 / lh as f64);
    let col = |c: usize| (((c as f64 + 0.5) * sx) as usize).min(fw - 1);
    let row = |r: usize| (((r as f64 + 0.5) * sy) as usize).min(fh - 1);
    let (c0, c1) = (col(window.x), col(window.x + window.width - 1));
    let (r0, r1) = (row(window.y), row(window.y + window.height - 1));
    let src = crate::tiff_mask::read_mask(reader, full, (c0, r0, c1 - c0 + 1, r1 - r0 + 1), limit)?;
    let sw = c1 - c0 + 1;
    let mut out = Vec::new();
    crate::dataset::fallible::reserve(&mut out, crate::dataset::fallible::len(&[window.width, window.height], "the mask")?, "the mask")?;
    for r in window.y..window.y + window.height {
        for c in window.x..window.x + window.width {
            out.push(src[(row(r) - r0) * sw + col(c) - c0]);
        }
    }
    Ok(Some(out))
}

/// A TIFF level's own mask, as GDAL's band of that level has it (see
/// [`LazyRaster::read_level_mask`]): `None` for an overview without one.
fn read_tiff_own_mask(t: &mut TiffRaster, window: Window, level: usize) -> Result<Option<Vec<u8>>> {
    if let Some(i) = t.levels[level].external
        && let Some(ovr) = t.ovr.as_mut()
    {
        if let Some(mask) = t.ovr_mask.as_mut() {
            return read_sidecar_own_mask(mask, window, (ovr.levels[i].width, ovr.levels[i].height));
        }
        return read_tiff_own_mask(ovr, window, i);
    }
    let w = (window.x, window.y, window.width, window.height);
    let limit = t.limits.chunk_bytes;
    let own = mask_for_image(&t.masks, &t.levels[level]);
    let reader = t.decoder.inner();
    own.map(|m| crate::tiff_mask::read_mask(reader, m, w, limit)).transpose()
}

/// Reads a window of a `.msk` sidecar's mask at a level of size `(lw, lh)` when the sidecar has
/// one of that size (its full-resolution mask or an overview); `None` otherwise.
fn read_sidecar_own_mask(m: &mut LazyRaster, window: Window, (lw, lh): (usize, usize)) -> Result<Option<Vec<u8>>> {
    let level = if (lw, lh) == (m.width(), m.height()) {
        None
    } else {
        match m.overviews().iter().position(|s| *s == (lw, lh)) {
            Some(i) => Some(i),
            None => return Ok(None),
        }
    };
    read_sidecar_mask(m, window, m.level_size(level)?).map(Some)
}

/// Maps windows exactly as the nearest fallback in read_tiff_mask/read_sidecar_mask does.
fn full_mask_windows(windows: &[Window], (lw, lh): (usize, usize), (fw, fh): (usize, usize)) -> Vec<Window> {
    let (sx, sy) = (fw as f64 / lw as f64, fh as f64 / lh as f64);
    let col = |c: usize| (((c as f64 + 0.5) * sx) as usize).min(fw - 1);
    let row = |r: usize| (((r as f64 + 0.5) * sy) as usize).min(fh - 1);
    windows
        .iter()
        .map(|w| {
            let (x0, x1) = (col(w.x), col(w.x + w.width - 1));
            let (y0, y1) = (row(w.y), row(w.y + w.height - 1));
            Window::new(x0, y0, x1 - x0 + 1, y1 - y0 + 1)
        })
        .collect()
}

fn prefetch_sidecar_own_masks(m: &mut LazyRaster, windows: &[Window], size: (usize, usize)) -> Result<()> {
    let overview = if (m.width(), m.height()) == size {
        None
    } else if let Some(i) = m.overviews().iter().position(|s| *s == size) {
        Some(i)
    } else {
        return Ok(());
    };
    m.prefetch_windows(windows, &WindowOptions { bands: vec![1], overview })
}

fn prefetch_sidecar_masks(m: &mut LazyRaster, windows: &[Window], size: (usize, usize)) -> Result<()> {
    if (m.width(), m.height()) == size || m.overviews().contains(&size) {
        return prefetch_sidecar_own_masks(m, windows, size);
    }
    let windows = full_mask_windows(windows, size, (m.width(), m.height()));
    m.prefetch_windows(&windows, &WindowOptions { bands: vec![1], overview: None })
}

/// The mask chunks touched by a bounded set of actual decoder windows.
fn mask_prefetch_ranges(mask: &crate::tiff_mask::MaskImage, windows: &[Window]) -> Vec<(u64, u64)> {
    let count = windows.iter().fold(0usize, |n, w| n.saturating_add(mask.chunk_count((w.x, w.y, w.width, w.height))));
    if count > MAX_PREFETCH_CHUNKS {
        return Vec::new();
    }
    windows.iter().flat_map(|w| mask.chunk_ranges((w.x, w.y, w.width, w.height))).collect()
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

/// Whether `path` is a local file whose first bytes show a VRT.
fn is_local_vrt(path: &str) -> bool {
    matches!(Location::parse(path), Location::Local(p) if p.is_file())
        && crate::formats::identify(path).by_content.as_deref() == Some("VRT")
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
    File(BufReader<crate::geotiff::FileCursor>),
    Memory(std::io::Cursor<SharedBytes>),
    #[cfg(remote)]
    Http(crate::http::RangeReader),
    /// A remote file whose bytes the caller fetches (see [`crate::sparse`]). Buffered so the
    /// decoder's many small reads do not each enter the sparse block store.
    Sparse(BufReader<SparseBlockReader>),
    /// A zip archive member read in place: stored, or seek-optimized (SOZip).
    Zip(Box<crate::vsi::sozip::MemberReader>),
}

/// A buffered TIFF read may read ahead within the current held block, never into the next
/// missing block. Direct SparseFile reads retain their multi-block request semantics.
struct SparseBlockReader {
    file: crate::sparse::SparseFile,
    block_size: u64,
}

impl SparseBlockReader {
    fn new(file: crate::sparse::SparseFile) -> Self {
        let block_size = file.block_size();
        Self { file, block_size }
    }
}

impl Read for SparseBlockReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        // BufReader bypasses its buffer for larger explicit reads. Those bytes are genuinely
        // requested, so keep SparseFile's batched missing-range behavior for them.
        if buf.len() as u64 > self.block_size {
            return self.file.read(buf);
        }
        let pos = self.file.stream_position()?;
        let len = (self.block_size - pos % self.block_size).min(buf.len() as u64) as usize;
        self.file.read(&mut buf[..len])
    }
}

impl Seek for SparseBlockReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}

impl Source {
    /// A remote file whose bytes the caller fetches, buffered by at most a block
    /// ([`SPARSE_BUFFER_BYTES`]).
    fn sparse(file: crate::sparse::SparseFile) -> Self {
        let capacity = file.block_size().min(SPARSE_BUFFER_BYTES as u64) as usize;
        Source::Sparse(BufReader::with_capacity(capacity, SparseBlockReader::new(file)))
    }

    /// Whether reading needs [`Source::prefetch`]: a source whose bytes the caller fetches.
    fn prefetches(&self) -> bool {
        match self {
            Source::Sparse(_) => true,
            #[cfg(remote)]
            Source::Http(_) => true,
            _ => false,
        }
    }

    /// Fetches byte ranges `(offset, length)` ahead of reading them, when that saves requests.
    #[cfg_attr(not(remote), allow(unused_variables))]
    fn prefetch(&self, ranges: &[(u64, u64)]) -> Result<()> {
        match self {
            Source::File(_) | Source::Memory(_) | Source::Zip(_) => Ok(()),
            #[cfg(remote)]
            Source::Http(r) => Ok(r.prefetch(ranges)?),
            Source::Sparse(r) => Ok(r.get_ref().file.prefetch(ranges)?),
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
            Source::Zip(z) => z.read(buf),
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
            Source::Zip(z) => z.seek(pos),
        }
    }
}

/// A definitive metadata answer, distinguishing an absent tag from an I/O/decoding error.
fn overview_resampling_metadata<R: Read + Seek>(decoder: &mut Decoder<R>) -> tiff::TiffResult<Option<String>> {
    let xml = decoder.find_tag(Tag::Unknown(42112))?.map(|value| value.into_string()).transpose()?;
    Ok(xml.and_then(|xml| {
        crate::drivers::gtiff_write::parse_gdal_band_metadata(&xml)
            .into_iter()
            .find(|(band, key, _)| *band == 0 && key == "RESAMPLING")
            .map(|(_, _, value)| value)
    }))
}

/// One image of a TIFF: the full-resolution image or an overview.
struct Level {
    /// The image's index in the file.
    ifd: usize,
    width: usize,
    height: usize,
    /// The image's own reduced-resolution bit, independent of its position in the file.
    reduced: bool,
    /// The tile or strip size.
    block: (usize, usize),
    /// The level of the `.ovr` file (see [`TiffRaster::ovr`]) this one is, rather than an image
    /// of this file.
    external: Option<usize>,
    /// The image's chunk tables, read by the level's first window read and reused by the next.
    tables: Option<Arc<ChunkTables>>,
    /// How the image's chunks are decoded, settled by the level's first window read that gets to
    /// decoding: `Some(None)` when the `tiff` crate decodes them itself.
    codec: Option<Option<crate::tiff_codecs::ChunkCodec>>,
    /// How the image's chunks are stored, read by the level's first window read.
    storage: Option<Storage>,
    /// Cached GDAL RESAMPLING metadata, including absence, for repeated selection.
    resampling: Option<Option<String>>,
}

/// How an image's chunks are stored: its PlanarConfiguration and Compression tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Storage {
    /// Band-interleaved (PlanarConfiguration 2) rather than pixel-interleaved.
    planar: bool,
    /// The TIFF compression code (1 when the tag is absent).
    compression: u16,
}

struct TiffRaster {
    decoder: Decoder<PatchedReader<Source>>,
    tags: GeoTags,
    /// The bands reported: the samples per pixel, or 4 for CMYK read as RGBA.
    bands: usize,
    /// The samples per pixel of the file's images.
    samples: usize,
    alpha: Option<usize>,
    /// 8-bit CMYK, read as GDAL reads it: RGBA (see [`cmyk_to_rgba`]).
    cmyk: bool,
    /// The type of the samples, or of their parts when they are complex.
    data_type: DataType,
    /// Complex samples (see [`sample_type`]): `bands` counts their real and imaginary parts.
    complex: bool,
    /// `BitsPerSample` of the full-resolution image.
    bits: u16,
    geo_transform: [f64; 6],
    /// The full-resolution image, then the overviews, largest first.
    levels: Vec<Level>,
    /// GDAL's internal masks (of the full-resolution image and of overviews).
    masks: Vec<crate::tiff_mask::MaskImage>,
    /// The index of the image the decoder is on.
    current_ifd: usize,
    /// Keeps a file extracted from an archive.
    _local: Option<vfs::LocalPath>,
    /// The local file path, used for output alias checks and sidecar discovery.
    file: Option<std::path::PathBuf>,
    /// Reopening and native archive alias information for an in-place ZIP member.
    zip: Option<crate::vsi::sozip::MemberReopen>,
    /// The decoders' limits.
    limits: TiffReadLimits,
    /// The length of the TIFF, in bytes.
    file_len: u64,
    /// The files [`LazyRaster::file_list`] lists: the TIFF as it was named, and the sidecars read.
    files: Vec<String>,
    /// The external overviews (`.ovr` file) of a file without internal ones.
    ovr: Option<Box<TiffRaster>>,
    /// The external overview dataset's per-dataset `.ovr.msk`, if present.
    ovr_mask: Option<Box<LazyRaster>>,
    /// The identity of this open reader in [`crate::block_cache`]: its chunks are keyed by it, not
    /// by the file's path, so that a file replaced under another reader is never read from the
    /// first one's chunks.
    cache_handle: u64,
    /// The decoders that decode chunks in parallel, kept between window reads (see
    /// [`crate::geotiff::DecoderPool`]).
    workers: crate::geotiff::DecoderPool<PatchedReader<Source>>,
}

impl Drop for TiffRaster {
    fn drop(&mut self) {
        crate::block_cache::remove_handle(self.cache_handle);
    }
}

/// What more decoders of a TIFF read: its local file, or its bytes in memory.
enum Reopened {
    File(crate::geotiff::FileCursor),
    Memory(SharedBytes),
    Zip(crate::vsi::sozip::MemberReopen),
    /// A remote file whose bytes the caller fetches: the chunks were prefetched before decoding.
    /// Quiet ([`crate::sparse::SparseFile::quiet`]): a decoder that cannot be opened is done
    /// without.
    Sparse(crate::sparse::SparseFile),
}

fn tiff_err(e: tiff::TiffError) -> Error {
    match e {
        tiff::TiffError::LimitsExceeded => Error::ResourceLimit("TIFF: decoding limits exceeded (see TiffReadLimits)".into()),
        tiff::TiffError::IoError(e)
            if e.kind() == std::io::ErrorKind::OutOfMemory
                || crate::error::is_http_io(&e)
                || e.get_ref().is_some_and(|e| e.is::<crate::sparse::NotFetched>()) =>
        {
            Error::Io(e)
        }
        e => Error::InvalidData(format!("TIFF: {e}")),
    }
}

impl TiffRaster {
    fn new(mut source: Source, local: Option<vfs::LocalPath>, limits: TiffReadLimits) -> Result<Self> {
        let zip = match &source {
            Source::Zip(member) => member.reopen(),
            _ => None,
        };
        let uploaded = matches!(&source, Source::Memory(_));
        let tags = try_read_geo_tags_from(&mut source)
            .map_err(|error| {
                // Invalid metadata is malformed input regardless of its source. An immutable
                // buffer's EOF is malformed too; live EOF errors retain their I/O causes.
                if error.kind() == std::io::ErrorKind::InvalidData || (uploaded && error.kind() == std::io::ErrorKind::UnexpectedEof) {
                    Error::InvalidData(format!("TIFF: {error}"))
                } else {
                    Error::Io(error)
                }
            })?
            .ok_or_else(|| Error::InvalidData("not a TIFF file".into()))?;
        // A directory libtiff cannot read is refused here, before any pixel is decoded.
        tags.check_structure()?;
        let masks = crate::tiff_mask::find_masks(&mut source)?;
        let len = source.seek(SeekFrom::End(0))?;
        source.seek(SeekFrom::Start(0))?;
        let tiff_err = limits.error();
        // The caller's limits, within the file's bounds (see `TiffReadLimits::tiff_within`).
        let mut dec = Decoder::new(PatchedReader::new(source, &tags)).map_err(tiff_err)?.with_limits(limits.tiff_within(len));
        let (w, h) = dec.dimensions().map_err(tiff_err)?;
        crate::geotiff::check_open(&mut dec, tiff_err)?;
        let samples = dec.find_tag_unsigned::<u16>(Tag::SamplesPerPixel).map_err(tiff_err)?.unwrap_or(1).max(1) as usize;
        let (data_type, complex) = sample_type(&mut dec)?;
        let cmyk = reads_cmyk_as_rgba(&mut dec, samples, data_type)?;
        // A complex sample is two bands: its real and its imaginary parts.
        let bands = if cmyk { 4 } else { samples * if complex { 2 } else { 1 } };
        let alpha = dec
            .get_tag_u16_vec(Tag::ExtraSamples)
            .ok()
            .and_then(|samples| samples.last().copied())
            .is_some_and(|sample| matches!(sample, 1 | 2))
            .then_some(bands);
        let bits = dec.find_tag_unsigned_vec::<u16>(Tag::BitsPerSample).map_err(tiff_err)?.and_then(|v| v.first().copied()).unwrap_or(1);
        let reduced = dec.find_tag_unsigned::<u32>(Tag::NewSubfileType).ok().flatten().unwrap_or(0) & 1 != 0;
        let (cw, ch) = dec.chunk_dimensions();
        let mut levels = vec![Level {
            ifd: 0,
            reduced,
            width: w as usize,
            height: h as usize,
            block: (cw as usize, ch as usize),
            external: None,
            tables: None,
            codec: None,
            storage: None,
            resampling: None,
        }];
        let mut ifd = 1;
        while dec.more_images() && ifd < MAX_IMAGES {
            // Images the decoder cannot read (such as unusual masks) are skipped.
            if dec.seek_to_image(ifd).is_ok() {
                let subfile = dec.find_tag_unsigned::<u32>(Tag::NewSubfileType).ok().flatten().unwrap_or(0);
                let level_samples = dec.find_tag_unsigned::<u16>(Tag::SamplesPerPixel).ok().flatten().unwrap_or(1).max(1) as usize;
                let same_type = sample_type(&mut dec).is_ok_and(|t| t == (data_type, complex));
                // A reduced-resolution image that is not a transparency mask.
                if subfile & 1 == 1 && subfile & 4 == 0 && level_samples == samples && same_type {
                    let (ow, oh) = dec.dimensions().map_err(tiff_err)?;
                    let (cw, ch) = dec.chunk_dimensions();
                    let block = (cw as usize, ch as usize);
                    levels.push(Level {
                        ifd,
                        reduced: subfile & 1 != 0,
                        width: ow as usize,
                        height: oh as usize,
                        block,
                        external: None,
                        tables: None,
                        codec: None,
                        storage: None,
                        resampling: None,
                    });
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
        Ok(Self {
            decoder: dec,
            tags,
            bands,
            samples,
            alpha,
            cmyk,
            data_type,
            complex,
            bits,
            geo_transform,
            levels,
            masks,
            current_ifd: 0,
            _local: local,
            file: None,
            zip,
            limits,
            file_len: len,
            files: Vec::new(),
            ovr: None,
            ovr_mask: None,
            cache_handle: crate::block_cache::new_handle(),
            workers: Default::default(),
        })
    }

    /// Takes the images of a GDAL `.ovr` file as overviews, as GDAL does when the TIFF has none
    /// of its own, when they have the TIFF's bands and type.
    fn attach_ovr(&mut self, path: &std::path::Path) -> Result<()> {
        if self.levels.len() > 1 || !path.is_file() {
            return Ok(());
        }
        let Ok(file) = std::fs::File::open(path) else { return Ok(()) };
        let mut ovr = match TiffRaster::new(Source::File(BufReader::new(crate::geotiff::FileCursor::new(file))), None, self.limits) {
            Ok(ovr) => ovr,
            Err(error) if error.is_resource_refusal() => return Err(error),
            Err(_) => return Ok(()),
        };
        if ovr.bands != self.bands || ovr.data_type != self.data_type || ovr.complex != self.complex {
            return Ok(());
        }
        let first = &ovr.levels[0];
        crate::declared::check(first.width, first.height, ovr.bands, ovr.data_type, ovr.complex)?;
        ovr.file = Some(path.to_path_buf());
        for (i, l) in ovr.levels.iter().enumerate() {
            // GDAL accepts all matching-band/type images, including irregular,
            // equal-sized and larger images; there is no reduction-factor gate.
            // Read through the `.ovr`'s own levels, which keep their tables.
            self.levels.push(Level {
                ifd: l.ifd,
                reduced: l.reduced,
                width: l.width,
                height: l.height,
                block: l.block,
                external: Some(i),
                tables: None,
                codec: None,
                storage: None,
                resampling: None,
            });
        }
        self.levels[1..].sort_by_key(|l| std::cmp::Reverse((l.width, l.height)));
        // Use the same sidecar-mask policy as a local base TIFF. This probes
        // `.ovr.msk` only when an external overview was actually opened.
        ovr.files.push(path.to_string_lossy().into_owned());
        let mut view =
            LazyRaster { inner: Inner::Tiff(Box::new(ovr)), sidecar_mask: None, asset_policy: crate::stac::AssetReadPolicy::current() };
        view.find_sidecar_mask(&path.to_string_lossy())?;
        let files = view.file_list().unwrap_or_else(|_| vec![path.to_string_lossy().into_owned()]);
        // Listed after the TIFF, as GDAL lists it.
        let at = self.files.len().min(1);
        self.files.splice(at..at, files);
        self.ovr_mask = view.sidecar_mask;
        let Inner::Tiff(ovr) = view.inner else { unreachable!() };
        self.ovr = Some(ovr);
        Ok(())
    }

    fn tags_at_level(&self, level: usize) -> &GeoTags {
        match (self.levels[level].external, self.ovr.as_ref()) {
            (Some(i), Some(ovr)) => ovr.tags_at_level(i),
            _ => &self.tags,
        }
    }

    /// Reads a window of some bands (numbered from 1) of a level, band-major: CMYK read as GDAL
    /// reads it, as RGBA.
    fn read(&mut self, level: usize, w: Window, bands: &[usize]) -> Result<RasterData> {
        // A level of an external `.ovr` is read from it.
        if let Some(i) = self.levels[level].external
            && let Some(ovr) = self.ovr.as_mut()
        {
            return ovr.read(i, w, bands);
        }
        if !self.cmyk {
            return self.read_samples(level, w, bands);
        }
        let RasterData::U8(cmyk) = self.read_samples(level, w, &[1, 2, 3, 4])? else {
            return Err(Error::InvalidData("CMYK samples that are not bytes".into()));
        };
        cmyk_to_rgba(&cmyk, w.width * w.height, bands)
    }

    /// Fetches the byte ranges of the chunks a window of some bands (numbered from 1) needs at a
    /// level, before [`TiffRaster::read`] decodes them: one sparse fetch round for the whole
    /// window instead of one per chunk.
    fn prefetch_windows(&mut self, level: usize, windows: &[Window], bands: &[usize]) -> Result<()> {
        // A level of an external `.ovr` is fetched from it.
        if let Some(i) = self.levels[level].external
            && let Some(ovr) = self.ovr.as_mut()
        {
            return ovr.prefetch_windows(i, windows, bands);
        }
        // CMYK is read as RGBA from all four samples, whatever bands were asked for.
        let bands = if self.cmyk { &[1, 2, 3, 4][..] } else { bands };
        let tiff_err = self.limits.error();
        let ifd = self.levels[level].ifd;
        let dec = &mut self.decoder;
        if !dec.inner().get_ref().prefetches() {
            return Ok(());
        }
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
        let tables = match &self.levels[level].tables {
            Some(tables) => tables.clone(),
            None => {
                let tables = Arc::new(ChunkTables::read(dec).map_err(tiff_err)?);
                self.levels[level].tables = Some(tables.clone());
                tables
            }
        };
        // The chunks of the planes the requested bands are stored in.
        let parts = if self.complex { 2 } else { 1 };
        let mut planes: Vec<usize> = if planar { bands.iter().map(|b| (b - 1) / parts).collect() } else { vec![0] };
        planes.sort_unstable();
        planes.dedup();
        let what = "the TIFF chunk list";
        let mut ranges = Vec::new();
        let chunks = windows.iter().fold(0usize, |n, w| {
            let rows = (w.y + w.height - 1) / ch - w.y / ch + 1;
            let cols = (w.x + w.width - 1) / cw - w.x / cw + 1;
            n.saturating_add(rows.saturating_mul(cols))
        });
        if planes.len().saturating_mul(chunks) > MAX_PREFETCH_CHUNKS {
            return Ok(());
        }
        crate::dataset::fallible::reserve(&mut ranges, crate::dataset::fallible::len(&[planes.len(), chunks], what)?, what)?;
        for &plane in &planes {
            for w in windows {
                for ty in w.y / ch..=(w.y + w.height - 1) / ch {
                    for tx in w.x / cw..=(w.x + w.width - 1) / cw {
                        let idx = plane * per_plane + ty * across + tx;
                        if crate::block_cache::contains(self.cache_handle, ifd, idx as u32) {
                            continue;
                        }
                        if let Some((offset, count)) = tables.get(idx)
                            && count > 0
                        {
                            ranges.push((offset, count));
                        }
                    }
                }
            }
        }
        let compression = dec.find_tag_unsigned::<u16>(Tag::Compression).map_err(tiff_err)?.unwrap_or(1);
        crate::geotiff::check_stored_chunks(compression, ranges.iter().copied(), self.file_len)?;
        dec.inner().get_ref().prefetch(&ranges)
    }

    /// Fetches the byte ranges of a level's mask chunks a window needs: the level's own mask
    /// image, else the full-resolution mask sampled at the window as [`read_tiff_mask`] does.
    fn prefetch_mask_windows(&mut self, level: usize, windows: &[Window]) -> Result<()> {
        if let Some(i) = self.levels[level].external
            && let Some(ovr) = self.ovr.as_mut()
        {
            if let Some(mask) = self.ovr_mask.as_mut() {
                return prefetch_sidecar_masks(mask, windows, (ovr.levels[i].width, ovr.levels[i].height));
            }
            return ovr.prefetch_mask_windows(i, windows);
        }
        if !self.decoder.inner().get_ref().prefetches() {
            return Ok(());
        }
        let (lw, lh) = (self.levels[level].width, self.levels[level].height);
        let (fw, fh) = (self.levels[0].width, self.levels[0].height);
        let ranges = if let Some(mask) = mask_for_image(&self.masks, &self.levels[level]) {
            mask_prefetch_ranges(mask, windows)
        } else if let Some(mask) = mask_for_image(&self.masks, &self.levels[0]) {
            mask_prefetch_ranges(mask, &full_mask_windows(windows, (lw, lh), (fw, fh)))
        } else {
            Vec::new()
        };
        self.decoder.inner().get_ref().prefetch(&ranges)
    }

    fn prefetch_own_mask_windows(&mut self, level: usize, windows: &[Window]) -> Result<()> {
        if let Some(i) = self.levels[level].external
            && let Some(ovr) = self.ovr.as_mut()
        {
            if let Some(mask) = self.ovr_mask.as_mut() {
                return prefetch_sidecar_own_masks(mask, windows, (ovr.levels[i].width, ovr.levels[i].height));
            }
            return ovr.prefetch_own_mask_windows(i, windows);
        }
        if !self.decoder.inner().get_ref().prefetches() {
            return Ok(());
        }
        let ranges = mask_for_image(&self.masks, &self.levels[level]).map(|mask| mask_prefetch_ranges(mask, windows)).unwrap_or_default();
        self.decoder.inner().get_ref().prefetch(&ranges)
    }

    /// Reads a window of some samples (numbered from 1) of a level, band-major, as stored.
    fn read_samples(&mut self, level: usize, w: Window, bands: &[usize]) -> Result<RasterData> {
        // A window too large for any allocation is refused before its chunks are listed or fetched,
        // and so is one the process cannot hold: allocated beyond the usable memory, it would kill
        // the process as its pages are written.
        let len = bands
            .len()
            .checked_mul(w.width)
            .and_then(|n| n.checked_mul(w.height))
            .filter(|n| n.checked_mul(self.data_type.size_bytes()).is_some_and(|b| b <= isize::MAX as usize))
            .ok_or_else(|| {
                Error::InvalidArgument(format!("a window of {}x{} pixels in {} band(s) is too large", w.width, w.height, bands.len()))
            })?;
        crate::dataset::fallible::check_raster_buffer("a window", w.width, w.height, bands.len(), self.data_type)?;
        let tiff_err = self.limits.error();
        let ifd = self.levels[level].ifd;
        let dec = &mut self.decoder;
        if self.current_ifd != ifd {
            dec.seek_to_image(ifd).map_err(tiff_err)?;
            self.current_ifd = ifd;
        }
        let (lw, lh) = (self.levels[level].width, self.levels[level].height);
        let Storage { planar, compression } = match self.levels[level].storage {
            Some(storage) => storage,
            None => {
                let storage = Storage {
                    planar: dec.find_tag_unsigned::<u16>(Tag::PlanarConfiguration).map_err(tiff_err)? == Some(2),
                    compression: dec.find_tag_unsigned::<u16>(Tag::Compression).map_err(tiff_err)?.unwrap_or(1),
                };
                self.levels[level].storage = Some(storage);
                storage
            }
        };
        let (cw, ch) = dec.chunk_dimensions();
        let (cw, ch) = (cw as usize, ch as usize);
        let across = lw.div_ceil(cw);
        let per_plane = across * lh.div_ceil(ch);
        // The level's chunk tables list every chunk of the image, whatever the window: read by its
        // first window read, then reused.
        let tables = match &self.levels[level].tables {
            Some(tables) => tables.clone(),
            None => {
                let tables = Arc::new(ChunkTables::read(dec).map_err(tiff_err)?);
                self.levels[level].tables = Some(tables.clone());
                tables
            }
        };

        // The chunks to read, with the output bands each one fills: (chunk, [(sample, out band)]).
        // A complex sample's parts are two consecutive samples of the chunk: band `b` (from 1)
        // is part `(b - 1) % 2` of the plane (or sample) `(b - 1) / 2`.
        let parts = if self.complex { 2 } else { 1 };
        let planes: Vec<(usize, Vec<(usize, usize)>)> = if planar {
            bands.iter().enumerate().map(|(k, b)| ((b - 1) / parts, vec![((b - 1) % parts, k)])).collect()
        } else {
            vec![(0, bands.iter().enumerate().map(|(k, b)| (b - 1, k)).collect())]
        };
        let (rows, cols) = ((w.y + w.height - 1) / ch - w.y / ch + 1, (w.x + w.width - 1) / cw - w.x / cw + 1);
        let what = "the TIFF chunk list";
        let mut chunks = Vec::new();
        crate::dataset::fallible::reserve(&mut chunks, crate::dataset::fallible::len(&[planes.len(), rows, cols], what)?, what)?;
        for (plane, map) in &planes {
            for ty in w.y / ch..=(w.y + w.height - 1) / ch {
                for tx in w.x / cw..=(w.x + w.width - 1) / cw {
                    chunks.push((plane * per_plane + ty * across + tx, tx * cw, ty * ch, map));
                }
            }
        }
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        crate::dataset::fallible::reserve(&mut ranges, chunks.len(), what)?;
        ranges.extend(chunks.iter().filter_map(|(i, ..)| tables.get(*i)).filter(|(_, n)| *n > 0));
        // Compressed chunks whose stored bytes run past the end of the file: refused, as GDAL does,
        // before anything is fetched.
        crate::geotiff::check_stored_chunks(compression, ranges.iter().copied(), self.file_len)?;
        drop(ranges);

        // Missing (sparse) chunks read as nodata, or 0, as in GDAL.
        let mut out = try_new_filled(self.data_type, len, self.tags.nodata.unwrap_or(0.0))?;
        if let Some(nodata) = self.tags.integer_nodata {
            match &mut out {
                RasterData::U64(values) => values.fill(nodata as u64),
                RasterData::I64(values) => values.fill(nodata as i64),
                _ => {}
            }
        }
        for (output_band, band) in bands.iter().enumerate() {
            if let Some((_, nodata)) = self.tags.band_nodata.iter().find(|(b, _)| b == band) {
                let plane = w.width * w.height;
                crate::pixel::with_data!(&mut out, values => values[output_band * plane..(output_band + 1) * plane].fill(*nodata as _));
            }
        }
        let spp = if planar { parts } else { self.samples * parts };
        // Chunks with data, and where they go.
        let mut wanted: Vec<u32> = Vec::new();
        crate::dataset::fallible::reserve(&mut wanted, chunks.len(), what)?;
        // A chunk can fill several output bands (a band asked for twice in a planar image).
        let mut targets: std::collections::HashMap<u32, Vec<ChunkTarget>> = std::collections::HashMap::new();
        targets.try_reserve(chunks.len()).map_err(|_| crate::dataset::fallible::out_of_memory(None, what))?;
        for (idx, x0, y0, map) in chunks {
            if tables.counts.get(idx).copied().unwrap_or(0) == 0 {
                continue;
            }
            let t = targets.entry(idx as u32).or_default();
            if t.is_empty() {
                wanted.push(idx as u32);
            }
            t.push((x0, y0, map));
        }
        // Chunks decoded by earlier reads are placed from the cache; only the others are fetched
        // and decoded.
        let (handle, caching) = (self.cache_handle, crate::block_cache::max_bytes() > 0);
        let mut misses: Vec<u32> = Vec::new();
        crate::dataset::fallible::reserve(&mut misses, wanted.len(), what)?;
        for &idx in &wanted {
            match caching.then(|| crate::block_cache::get(handle, ifd, idx)).flatten() {
                Some(cached) => place_decoded(&mut out, &cached.samples, cached.dims, &targets[&idx], w, spp)?,
                None => misses.push(idx),
            }
        }
        if misses.is_empty() {
            return Ok(out);
        }
        let mut ranges: Vec<(u64, u64)> = Vec::new();
        crate::dataset::fallible::reserve(&mut ranges, misses.len(), what)?;
        ranges.extend(misses.iter().filter_map(|i| tables.get(*i as usize)));
        dec.inner().get_ref().prefetch(&ranges)?;
        drop(ranges);
        // More decoders over the local file, or over the bytes in memory.
        let (tags, limits) = (&self.tags, self.limits);
        let source = match dec.inner().get_ref() {
            Source::File(f) => Some(Reopened::File(f.get_ref().clone())),
            Source::Memory(m) => Some(Reopened::Memory(m.get_ref().clone())),
            Source::Zip(member) => member.reopen().map(Reopened::Zip),
            // Without recording what they miss: a header the caller's cache dropped would be
            // fetched again for decoders the read does without.
            Source::Sparse(s) => Some(Reopened::Sparse(s.get_ref().file.quiet())),
            #[cfg(remote)]
            Source::Http(_) => None,
        };
        let reopen = source.map(|source| {
            move || -> Result<Decoder<PatchedReader<Source>>> {
                // The `tiff` crate's limits bounded by the file's size, as for the first decoder.
                let (s, len) = match &source {
                    Reopened::File(file) => {
                        let mut file = file.clone();
                        file.seek(SeekFrom::Start(0))?;
                        let len = file.len()?;
                        (Source::File(BufReader::new(file)), len)
                    }
                    Reopened::Memory(bytes) => (Source::Memory(std::io::Cursor::new(bytes.clone())), bytes.as_ref().len() as u64),
                    Reopened::Zip(member) => {
                        let mut s = Source::Zip(Box::new(member.open()?));
                        let len = s.seek(SeekFrom::End(0))?;
                        s.seek(SeekFrom::Start(0))?;
                        (s, len)
                    }
                    Reopened::Sparse(file) => {
                        let mut s = Source::sparse(file.clone());
                        s.seek(SeekFrom::Start(0))?;
                        (s, file.len())
                    }
                };
                Ok(Decoder::new(PatchedReader::new(s, tags)).map_err(tiff_err)?.with_limits(limits.tiff_within(len)))
            }
        });
        let reopen = reopen.as_ref().map(|f| f as crate::geotiff::Reopen<PatchedReader<Source>>);
        // The compressions the `tiff` crate does not decode (alone), with the same limit: the
        // level's codec is made once, with the level's tables, and kept, as is the lack of one.
        let codec = &mut self.levels[level].codec;
        if codec.is_none() {
            *codec = Some(crate::tiff_codecs::ChunkCodec::new(dec, limits.chunk_bytes, Some(tables.clone())).map_err(tiff_err)?);
        }
        let codec = codec.as_ref().and_then(Option::as_ref);
        let pool = Some(&self.workers);
        crate::geotiff::decode_chunks(dec, reopen, pool, ifd, &misses, codec, limits.chunk_bytes, &tiff_err, |(idx, chunk, dims)| {
            place_decoded(&mut out, &chunk, dims, &targets[&idx], w, spp)?;
            if caching {
                crate::block_cache::insert(handle, ifd, idx, chunk, dims);
            }
            Ok(())
        })?;
        Ok(out)
    }
}

/// Places a decoded chunk, whose image data is `dims` (width, height) pixels, into the window `w`
/// held in `out`, at each of its `targets`. A half-float chunk is converted to `f32` here, as
/// decoded, so that the chunk can be kept as it is.
fn place_decoded(
    out: &mut RasterData,
    chunk: &DecodingResult,
    (dw, dh): (usize, usize),
    targets: &[ChunkTarget],
    w: Window,
    spp: usize,
) -> Result<()> {
    macro_rules! place {
        ($($variant:ident <- $res:ident),*) => {
            match (out, chunk) {
                $((RasterData::$variant(o), DecodingResult::$res(v)) => {
                    for &(x0, y0, map) in targets {
                        place(o, v, &ChunkGeom { x0, y0, dw, dh, spp }, w, map);
                    }
                })*
                (RasterData::F32(o), DecodingResult::F16(v)) => {
                    let mut f = Vec::new();
                    crate::dataset::fallible::reserve(&mut f, v.len(), "a TIFF chunk")?;
                    f.extend(v.iter().map(|x| x.to_f32()));
                    for &(x0, y0, map) in targets {
                        place(o, &f, &ChunkGeom { x0, y0, dw, dh, spp }, w, map);
                    }
                }
                _ => return Err(Error::InvalidData("TIFF chunks differ in sample type".into())),
            }
        };
    }
    place!(U8 <- U8, I8 <- I8, U16 <- U16, I16 <- I16, U32 <- U32, I32 <- I32, U64 <- U64, I64 <- I64, F32 <- F32, F64 <- F64);
    Ok(())
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
        // Each sample copied to its band, a band at a time; a chunk shorter than its rows (a
        // truncated one) leaves the columns past its end as they are.
        if c1 <= c0 {
            continue;
        }
        let first = (src_row + c0 - g.x0) * g.spp;
        let dst = dst_row + c0 - w.x;
        for &(sample, band) in map {
            let src = vals.get(first + sample..).unwrap_or_default().iter().step_by(g.spp);
            for (o, v) in out[band * band_len + dst..][..c1 - c0].iter_mut().zip(src) {
                *o = *v;
            }
        }
    }
}

/// Whether the decoder's image is CMYK that GDAL reads through libtiff's RGBA interface, as RGBA
/// (GDAL 3.12.1 `GTiffDataset::OpenOffset`: `bTreatAsRGBA` for PHOTOMETRIC_SEPARATED of at most 8
/// bits when `TIFFRGBAImageOK`; libtiff 4.7 `TIFFRGBAImageOK` wants InkSet CMYK and 4 samples or
/// more, and its put routines exist for 8-bit samples, of 4 samples when planar). Other CMYK
/// images are left as they are stored.
pub(crate) fn reads_cmyk_as_rgba<R: Read + Seek>(dec: &mut Decoder<R>, samples: usize, data_type: DataType) -> Result<bool> {
    if dec.find_tag_unsigned::<u16>(Tag::PhotometricInterpretation).map_err(tiff_err)? != Some(5) {
        return Ok(false);
    }
    let bits = dec.find_tag_unsigned_vec::<u16>(Tag::BitsPerSample).map_err(tiff_err)?.and_then(|v| v.first().copied()).unwrap_or(1);
    let format = dec.find_tag_unsigned_vec::<u16>(Tag::SampleFormat).map_err(tiff_err)?.and_then(|v| v.first().copied()).unwrap_or(1);
    let inkset = dec.find_tag_unsigned::<u16>(Tag::Unknown(332)).map_err(tiff_err)?.unwrap_or(1);
    let planar = dec.find_tag_unsigned::<u16>(Tag::PlanarConfiguration).map_err(tiff_err)? == Some(2);
    Ok(bits == 8 && format == 1 && data_type == DataType::U8 && inkset == 1 && samples >= 4 && (!planar || samples == 4))
}

/// RGBA from 8-bit CMYK planes (`cmyk`: C, M, Y and K, `n` samples each), as libtiff's
/// `putRGBcontig8bitCMYKtile` and `putCMYKseparate8bittile` compute it: `k = 255 - K`, each color
/// `k * (255 - C) / 255` in integers, alpha 255. Returns the bands asked for (1 red, 2 green, 3
/// blue, 4 alpha), band-major.
pub(crate) fn cmyk_to_rgba(cmyk: &[u8], n: usize, bands: &[usize]) -> Result<RasterData> {
    let what = "the window";
    let mut out = Vec::new();
    crate::dataset::fallible::reserve(&mut out, crate::dataset::fallible::len(&[bands.len(), n], what)?, what)?;
    let plane = |i: usize| &cmyk[i * n..(i + 1) * n];
    for &b in bands {
        match b {
            1..=3 => out.extend(plane(b - 1).iter().zip(plane(3)).map(|(&c, &k)| ((u32::from(255 - k) * u32::from(255 - c)) / 255) as u8)),
            _ => out.extend(std::iter::repeat_n(255u8, n)),
        }
    }
    Ok(RasterData::U8(out))
}

/// [`RasterData::new_filled`], or an error when the samples cannot be allocated: a header can
/// declare an image of terabytes, whose full window would abort the process. Allocated as
/// [`crate::dataset::fallible::raster_data`] allocates: zeros from the allocator zeroed, their
/// pages untouched until written.
fn try_new_filled(data_type: DataType, len: usize, value: f64) -> Result<RasterData> {
    crate::dataset::fallible::raster_data(data_type, len, value, "the window")
        .map_err(|_| Error::InvalidArgument(format!("a window of {len} samples is too large to allocate")))
}

/// The pixel type of the decoder's current image, from `BitsPerSample` and `SampleFormat`, and
/// whether its samples are complex (`SampleFormat` 5 or 6, GDAL's `CInt16`, `CInt32`, `CFloat32`
/// and `CFloat64`): the type is then that of their real and imaginary parts, each sample two
/// values of it (see [`RasterDataset::complex`]).
pub(crate) fn sample_type<R: Read + Seek>(dec: &mut Decoder<R>) -> Result<(DataType, bool)> {
    let bits = dec.find_tag_unsigned_vec::<u16>(Tag::BitsPerSample).map_err(tiff_err)?.and_then(|v| v.first().copied()).unwrap_or(1);
    let format = dec.find_tag_unsigned_vec::<u16>(Tag::SampleFormat).map_err(tiff_err)?.and_then(|v| v.first().copied()).unwrap_or(1);
    Ok(match (format, bits) {
        // GDAL 3.12.1 `GTiffRasterBand::GTiffRasterBand`: complex integers of 32 and 64 bits,
        // complex floats of 64 and 128 bits (its CFloat16, of 32 bits, is not read).
        (5, 32) => (DataType::I16, true),
        (5, 64) => (DataType::I32, true),
        (6, 64) => (DataType::F32, true),
        (6, 128) => (DataType::F64, true),
        (2, 8) => (DataType::I8, false),
        (2, 16) => (DataType::I16, false),
        (2, 32) => (DataType::I32, false),
        (2, 64) => (DataType::I64, false),
        (3, 16 | 32) => (DataType::F32, false),
        (3, 64) => (DataType::F64, false),
        (_, 1..=8) => (DataType::U8, false),
        // Packed samples (GDAL's NBITS) are read as the type GDAL gives them.
        (1, 9..=16) => (DataType::U16, false),
        (1, 17..=32) => (DataType::U32, false),
        (_, 64) => (DataType::U64, false),
        _ => return Err(Error::Unsupported(format!("TIFF samples of {bits} bits (format {format}) are not supported"))),
    })
}

#[cfg(test)]
mod tests {
    /// A transient metadata read must remain retryable, rather than become cached absence.
    #[test]
    fn overview_metadata_read_error_can_be_retried() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        struct Reader {
            inner: std::io::Cursor<Vec<u8>>,
            fail: Arc<AtomicBool>,
        }
        impl std::io::Read for Reader {
            fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                if self.fail.swap(false, Ordering::Relaxed) {
                    return Err(std::io::Error::other("temporary read failure"));
                }
                self.inner.read(bytes)
            }
        }
        impl std::io::Seek for Reader {
            fn seek(&mut self, position: std::io::SeekFrom) -> std::io::Result<u64> {
                self.inner.seek(position)
            }
        }
        let mut bytes = std::io::Cursor::new(Vec::new());
        {
            let mut encoder = tiff::encoder::TiffEncoder::new(&mut bytes).unwrap();
            let mut image = encoder.new_image::<tiff::encoder::colortype::Gray8>(1, 1).unwrap();
            image
                .encoder()
                .write_tag(
                    tiff::tags::Tag::Unknown(42112),
                    "<GDALMetadata><Item name=\"RESAMPLING\" sample=\"0\">AVERAGE_BIT2GRAYSCALE</Item></GDALMetadata>",
                )
                .unwrap();
            image.write_data(&[0u8]).unwrap();
        }
        let fail = Arc::new(AtomicBool::new(false));
        let mut decoder =
            tiff::decoder::Decoder::new(Reader { inner: std::io::Cursor::new(bytes.into_inner()), fail: fail.clone() }).unwrap();
        fail.store(true, Ordering::Relaxed);
        assert!(super::overview_resampling_metadata(&mut decoder).is_err());
        assert_eq!(super::overview_resampling_metadata(&mut decoder).unwrap().as_deref(), Some("AVERAGE_BIT2GRAYSCALE"));
    }

    #[test]
    fn window_allocation_capacity_overflow_returns_an_error() {
        let error = super::try_new_filled(crate::DataType::F64, usize::MAX, 0.0).unwrap_err();
        assert!(matches!(error, crate::Error::InvalidArgument(ref message) if message.contains("too large to allocate")));
    }

    use super::*;

    #[test]
    fn buffered_sparse_source_reads_seeks_and_prefetches_through_the_buffer() {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
        let file = crate::sparse::SparseFile::with_block_size(data.len() as u64, 4096);
        file.insert(0, &data);

        let mut source = Source::Sparse(BufReader::with_capacity(1024, SparseBlockReader::new(file)));
        source.seek(SeekFrom::Start(150_000)).unwrap();
        let mut buf = vec![0u8; 1000];
        source.read_exact(&mut buf).unwrap();
        assert_eq!(buf, data[150_000..151_000]);

        let file = crate::sparse::SparseFile::with_block_size(data.len() as u64, 4096);
        let source = Source::Sparse(BufReader::with_capacity(1024, SparseBlockReader::new(file.clone())));
        // Prefetch reaches the SparseFile under the buffer and records whole blocks.
        assert!(source.prefetch(&[(10_000, 100)]).is_err());
        assert_eq!(file.missing(), [(8192, 4096)]);
        file.insert(8192, &data[8192..12288]);
        assert!(source.prefetch(&[(10_000, 100)]).is_ok());
        assert!(file.missing().is_empty());
    }

    #[test]
    fn sparse_read_ahead_stops_at_the_current_block_boundary() {
        for block in [4096, 65536] {
            let file = crate::sparse::SparseFile::with_block_size(4 * block, block);
            let capacity = (block as usize).min(SPARSE_BUFFER_BYTES);
            let mut source = Source::Sparse(BufReader::with_capacity(capacity, SparseBlockReader::new(file.clone())));
            source.seek(SeekFrom::Start(block - 1)).unwrap();
            assert!(source.read_exact(&mut [0]).is_err());
            assert_eq!(file.missing(), [(0, block)]);
        }
    }

    #[test]
    fn explicit_large_sparse_reads_keep_batched_missing_ranges() {
        let file = crate::sparse::SparseFile::with_block_size(100_000, 4096);
        let mut source = Source::Sparse(BufReader::with_capacity(4096, SparseBlockReader::new(file.clone())));
        source.seek(SeekFrom::Start(100)).unwrap();
        assert!(source.read_exact(&mut [0; 16_384]).is_err());
        assert_eq!(file.missing(), [(0, 5 * 4096)]);
    }

    #[test]
    fn memory_header_preserves_metadata_without_pixels() {
        let cfg = wbraster::RasterConfig { cols: 4, rows: 3, bands: 2, data_type: DataType::I32, ..Default::default() };
        let raster = wbraster::Raster::from_data_native(cfg, RasterData::I32((0..24).collect())).unwrap();
        let mut dataset = RasterDataset::new(raster, true);
        dataset.set_geo_transform([10., 2., 0.1, 20., 0.2, -3.]);
        dataset.complex = true;
        dataset.geolocation = Some(crate::Geolocation {
            srs: "EPSG:4326".into(),
            x_dataset: "x.tif".into(),
            y_dataset: "y.tif".into(),
            x_band: 1,
            y_band: 2,
            pixel_offset: 0.,
            line_offset: 0.,
            pixel_step: 2.,
            line_step: 2.,
            pixel_center: true,
        });
        dataset.band_metadata = vec![crate::BandMetadata { color_interp: Some("Gray".into()), ..Default::default() }];
        dataset.raster.metadata = vec![("example".into(), "value".into())];
        let lazy = LazyRaster::from(dataset.clone());
        let header = lazy.header();
        assert_eq!(header.raster.data.len(), 0);
        assert_eq!(header.geo_transform(), dataset.geo_transform());
        assert_eq!(header.geolocation, dataset.geolocation);
        assert_eq!(header.band_metadata, dataset.band_metadata);
        assert_eq!(header.complex, dataset.complex);
        assert_eq!(header.raster.metadata, dataset.raster.metadata);
        assert_eq!(header.raster.cols, 4);
        assert_eq!(lazy.dataset().unwrap().raster.data.len(), 24);
    }

    #[test]
    fn shares_loaded_buffers_and_lazy_window_readers() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct CountingSource(Arc<AtomicUsize>);
        impl WindowSource for CountingSource {
            fn overviews(&self) -> Vec<(usize, usize)> {
                vec![(512, 512)]
            }
            fn block_size(&self) -> Option<(usize, usize)> {
                Some((16, 16))
            }
            fn alpha_band(&self) -> Option<usize> {
                Some(2)
            }
            fn has_mask(&self) -> bool {
                true
            }
            fn read_mask(&mut self, _: usize, window: Window) -> Result<Option<Vec<u8>>> {
                Ok(Some(vec![255; window.width * window.height]))
            }
            fn read(&mut self, level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
                assert!(window.width <= 4 && window.height <= 4, "a shared source must stay windowed");
                let n = window.width * window.height * bands.len();
                self.0.fetch_add(n, Ordering::Relaxed);
                Ok(RasterData::U8(vec![10 + level as u8; n]))
            }
        }
        let cfg = wbraster::RasterConfig { cols: 1024, rows: 1024, bands: 2, data_type: DataType::U8, ..Default::default() };
        let ds = RasterDataset::new(wbraster::Raster::from_data_native(cfg, RasterData::U8(vec![7; 2 * 1024 * 1024])).unwrap(), false);
        let mut memory = LazyRaster::from(ds);
        let loaded = memory.shared();
        assert!(std::ptr::eq(memory.dataset().unwrap(), loaded.dataset().unwrap()));
        assert!(!memory.is_lazy() && !loaded.is_lazy());
        let pixels = Arc::new(AtomicUsize::new(0));
        let mut source = LazyRaster::from_source(memory.header(), Box::new(CountingSource(pixels.clone())));
        let mut shared = source.shared();
        let mut again = source.shared();
        assert_eq!(pixels.load(Ordering::Relaxed), 0);
        assert!(source.is_lazy() && shared.is_lazy());
        assert_eq!(shared.overviews(), vec![(512, 512)]);
        assert_eq!(shared.block_size(), Some((16, 16)));
        assert_eq!(shared.alpha_band(), Some(2));
        let window = Window::new(2, 3, 2, 2);
        assert_eq!(shared.read_mask(window, Some(0)).unwrap(), Some(vec![255; 4]));
        for reader in [&mut source, &mut shared, &mut again] {
            let read = reader.read_window(window, &WindowOptions { bands: vec![2, 1, 2], overview: Some(0) }).unwrap();
            assert_eq!(read.raster.data.to_f64_vec(), vec![11.; 12]);
        }
        assert_eq!(pixels.load(Ordering::Relaxed), 36);
    }

    /// A masked input larger than one strip must never be requested whole, for pixels or masks.
    #[test]
    fn masked_geotiff_writes_keep_pixel_and_mask_reads_windowed() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct MaskedSource(Arc<AtomicUsize>);
        const WIDTH: usize = 128;
        const HEIGHT: usize = 65537;
        impl WindowSource for MaskedSource {
            fn overviews(&self) -> Vec<(usize, usize)> {
                Vec::new()
            }
            fn block_size(&self) -> Option<(usize, usize)> {
                Some((WIDTH, 16))
            }
            fn has_mask(&self) -> bool {
                true
            }
            fn input_paths(&self) -> Result<Vec<String>> {
                Ok(Vec::new())
            }
            fn read_mask(&mut self, _: usize, window: Window) -> Result<Option<Vec<u8>>> {
                assert!(window.height < HEIGHT, "the source mask must stay windowed");
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(Some(
                    (0..window.width * window.height).map(|i| if (window.x + i % window.width) < WIDTH / 2 { 0 } else { 255 }).collect(),
                ))
            }
            fn read(&mut self, _: usize, window: Window, _: &[usize]) -> Result<RasterData> {
                assert!(window.height < HEIGHT, "the source pixels must stay windowed");
                self.0.fetch_add(1, Ordering::Relaxed);
                Ok(RasterData::F64(vec![100.; window.width * window.height]))
            }
        }
        let grid = crate::ops::Grid { width: WIDTH, height: HEIGHT, geo_transform: [0., 1., 0., HEIGHT as f64, 0., -1.] };
        let header = crate::ops::raster_header(grid, DataType::F64, 1, None, &crate::crs::SpatialRef::parse("EPSG:3857").unwrap()).unwrap();
        let reads = Arc::new(AtomicUsize::new(0));
        let mut source = LazyRaster::from_source(header, Box::new(MaskedSource(reads.clone())));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("masked.tif");
        source
            .write(path.to_str().unwrap(), &crate::RasterWriteOptions { compression: Some("DEFLATE".into()), ..Default::default() })
            .unwrap();
        assert_eq!(reads.load(Ordering::Relaxed), 4, "two pixel strips and two mask strips");
        let mut output = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        let window = Window::new(WIDTH / 2 - 1, HEIGHT - 2, 2, 2);
        assert_eq!(output.read_mask(window, None).unwrap(), Some(vec![0, 255, 0, 255]));
        assert_eq!(output.read_window(window, &Default::default()).unwrap().raster.data.to_f64_vec(), vec![100.; 4]);
    }

    /// A source that cannot list the files it reads is still written as a tile archive (read
    /// whole), the archive the raster in memory gives.
    #[cfg(all(feature = "tiles", not(target_family = "wasm")))]
    #[test]
    fn writes_a_tile_archive_from_a_source_without_an_inventory() {
        struct Uninventoried(RasterDataset);
        impl WindowSource for Uninventoried {
            fn overviews(&self) -> Vec<(usize, usize)> {
                Vec::new()
            }
            fn read(&mut self, level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
                assert_eq!(level, 0);
                let (cols, rows) = (self.0.raster.cols, self.0.raster.rows);
                let all = self.0.raster.data.to_f64_vec();
                let mut v = Vec::with_capacity(window.width * window.height * bands.len());
                for &b in bands {
                    for y in window.y..window.y + window.height {
                        let row = ((b - 1) * rows + y) * cols;
                        v.extend_from_slice(&all[row + window.x..row + window.x + window.width]);
                    }
                }
                Ok(RasterData::from_f64_vec(DataType::U8, v))
            }
        }
        let res = 2.0 * 6378137.0 * std::f64::consts::PI / 256.0 / 1024.0;
        let cfg = wbraster::RasterConfig {
            cols: 300,
            rows: 200,
            bands: 1,
            x_min: 1000.0 * res,
            y_min: 2000.0 * res,
            cell_size: res,
            data_type: DataType::U8,
            crs: crate::crs::SpatialRef::from_epsg(3857).unwrap().to_crs_info(),
            ..Default::default()
        };
        let data = RasterData::from_f64_vec(DataType::U8, (0..300 * 200).map(|i| (i % 251) as f64).collect());
        let ds = RasterDataset::new(wbraster::Raster::from_data_native(cfg, data).unwrap(), false);
        let mut lazy = LazyRaster::from_source(LazyRaster::from(ds.clone()).header(), Box::new(Uninventoried(ds.clone())));
        assert!(lazy.input_paths().is_err());
        let dir = tempfile::tempdir().unwrap();
        let options = crate::RasterWriteOptions { creation_options: vec![("MAXZOOM".into(), "10".into())], ..Default::default() };
        for name in ["tiles.pmtiles", "tiles.mbtiles"] {
            let (lazy_dir, memory_dir) = (dir.path().join("lazy"), dir.path().join("memory"));
            std::fs::create_dir_all(&lazy_dir).unwrap();
            std::fs::create_dir_all(&memory_dir).unwrap();
            let (written, expected) = (lazy_dir.join(name), memory_dir.join(name));
            lazy.write(written.to_str().unwrap(), &options).unwrap();
            crate::write_raster_dataset(&ds, expected.to_str().unwrap(), &options).unwrap();
            assert!(std::fs::read(&written).unwrap() == std::fs::read(&expected).unwrap(), "{name}");
        }
    }

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

    /// A local VRT named `.tif` (or without an extension) opens as a VRT, as GDAL's VRT
    /// driver identifies it by its content.
    #[test]
    fn a_local_vrt_opens_whatever_its_name() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.tif").to_str().unwrap().to_string();
        let cfg = wbraster::RasterConfig { cols: 8, rows: 8, bands: 1, data_type: DataType::U8, ..Default::default() };
        let r = wbraster::Raster::from_data_native(cfg, RasterData::U8((0..64).collect())).unwrap();
        let options = crate::RasterWriteOptions { driver: Some(RasterDriver::GTiff), ..Default::default() };
        crate::write_raster_dataset(&RasterDataset::with_driver(r, RasterDriver::GTiff, false), &src, &options).unwrap();
        let vrt = format!(
            "<VRTDataset rasterXSize=\"8\" rasterYSize=\"8\"><VRTRasterBand dataType=\"Byte\" band=\"1\"><SimpleSource>\
<SourceFilename relativeToVRT=\"0\">{src}</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"
        );
        for name in ["pointer.vrt", "disguised.tif", "no_extension"] {
            let path = dir.path().join(name).to_str().unwrap().to_string();
            std::fs::write(&path, &vrt).unwrap();
            let mut r = LazyRaster::open(&path).unwrap_or_else(|e| panic!("{name}: {e}"));
            let RasterData::U8(v) = r.read().unwrap().raster.data else { panic!("{name}") };
            assert_eq!(v, (0..64).collect::<Vec<u8>>(), "{name}");
        }
    }

    /// 8-bit CMYK reads as GDAL reads it through libtiff's RGBA interface: RGBA, by libtiff's
    /// integer formula, alpha 255; 16-bit CMYK is left as stored.
    #[test]
    fn cmyk_reads_as_rgba() {
        // libtiff: k = 255 - K; R = k * (255 - C) / 255 (integer division).
        let n = 3;
        let cmyk = [121u8, 0, 255, 229, 0, 255, 158, 0, 255, 159, 0, 255];
        let RasterData::U8(rgba) = cmyk_to_rgba(&cmyk, n, &[1, 2, 3, 4]).unwrap() else { panic!() };
        assert_eq!(rgba, [50, 255, 0, 9, 255, 0, 36, 255, 0, 255, 255, 255]);
        let RasterData::U8(b) = cmyk_to_rgba(&cmyk, n, &[3, 1]).unwrap() else { panic!() };
        assert_eq!(b, [36, 255, 0, 50, 255, 0]);
        // A 2 x 1 strip of CMYK, 8 and 16 bits.
        for (bits, as_rgba) in [(8u16, true), (16, false)] {
            let samples: Vec<u16> = vec![121, 229, 158, 159, 0, 0, 0, 0];
            let data: Vec<u8> =
                if bits == 8 { samples.iter().map(|v| *v as u8).collect() } else { samples.iter().flat_map(|v| v.to_le_bytes()).collect() };
            let tags: [(u16, u16, u32); 9] = [
                (256, 3, 2),
                (257, 3, 1),
                (258, 3, u32::from(bits)),
                (259, 3, 1),
                (262, 3, 5),
                (273, 4, 0),
                (277, 3, 4),
                (278, 3, 1),
                (279, 4, data.len() as u32),
            ];
            let data_offset = 8 + 2 + tags.len() as u32 * 12 + 4;
            let mut b = b"II*\0".to_vec();
            b.extend(8u32.to_le_bytes());
            b.extend((tags.len() as u16).to_le_bytes());
            for (tag, typ, value) in tags {
                b.extend(tag.to_le_bytes());
                b.extend(typ.to_le_bytes());
                b.extend(1u32.to_le_bytes());
                b.extend((if tag == 273 { data_offset } else { value }).to_le_bytes());
            }
            b.extend(0u32.to_le_bytes());
            b.extend(&data);
            let mut r = LazyRaster::from_tiff_bytes(b).unwrap().unwrap();
            assert_eq!(r.band_count(), 4);
            let got = r.read().unwrap().raster.data;
            match (as_rgba, got) {
                (true, RasterData::U8(v)) => assert_eq!(v, [50, 255, 9, 255, 36, 255, 255, 255], "{bits} bits"),
                (false, RasterData::U16(v)) => assert_eq!(v, [121, 0, 229, 0, 158, 0, 159, 0], "{bits} bits"),
                (_, other) => panic!("{bits} bits: {:?}", other.data_type()),
            }
        }
    }

    /// A GeoTIFF whose own transform is `(1, 0, 0, 0, -1, H)`, H its height, has a geotransform;
    /// one without georeferencing tags has none, though both read `(0, 1, 0, H, 0, -1)`.
    #[test]
    fn says_whether_a_geotiff_has_a_geotransform() {
        let dir = tempfile::tempdir().unwrap();
        for (name, georeferenced) in [("flipped.tif", true), ("plain.tif", false)] {
            let path = dir.path().join(name).to_str().unwrap().to_string();
            let cfg = wbraster::RasterConfig {
                cols: 8,
                rows: 6,
                bands: 1,
                data_type: DataType::U8,
                x_min: 0.0,
                y_min: 0.0,
                cell_size: 1.0,
                ..Default::default()
            };
            let r = wbraster::Raster::from_data_native(cfg, RasterData::U8(vec![1; 48])).unwrap();
            let options = crate::RasterWriteOptions { driver: Some(RasterDriver::GTiff), ..Default::default() };
            let mut ds = RasterDataset::with_driver(r, RasterDriver::GTiff, false);
            let bytes = if georeferenced {
                ds.affine = Some([0.0, 1.0, 0.0, 6.0, 0.0, -1.0]);
                crate::write_raster_dataset(&ds, &path, &options).unwrap();
                std::fs::read(&path).unwrap()
            } else {
                // The same strip, without its georeferencing tags.
                crate::write_raster_dataset(&ds, &path, &options).unwrap();
                strip_georeferencing(std::fs::read(&path).unwrap())
            };
            let r = LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap();
            assert_eq!(r.geo_transform(), [0.0, 1.0, 0.0, 6.0, 0.0, -1.0], "{name}");
            assert_eq!(r.geotiff_has_geo_transform(), Some(georeferenced), "{name}");
        }
    }

    /// A little-endian classic TIFF with its ModelPixelScale, ModelTiepoint and
    /// ModelTransformation entries renamed to an unknown private tag.
    fn strip_georeferencing(mut b: Vec<u8>) -> Vec<u8> {
        assert_eq!(&b[..4], b"II*\0");
        let ifd = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
        let n = u16::from_le_bytes(b[ifd..ifd + 2].try_into().unwrap()) as usize;
        for i in 0..n {
            let at = ifd + 2 + 12 * i;
            if matches!(u16::from_le_bytes(b[at..at + 2].try_into().unwrap()), 33550 | 33922 | 34264) {
                b[at..at + 2].copy_from_slice(&65000u16.to_le_bytes());
            }
        }
        b
    }

    /// A `.msk` sidecar (#201) is read as the per-dataset mask, except under
    /// `GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR`, where GDAL 3.12.1 reads no sidecar and the mask
    /// is all valid (`test_empty_dir_ignores_msk_sidecars` compares with GDAL).
    #[test]
    fn msk_sidecars_are_not_read_under_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        let tif = dir.path().join("m.tif").to_str().unwrap().to_string();
        let write = |path: &str, data: RasterData| {
            let cfg = wbraster::RasterConfig { cols: 8, rows: 8, bands: 1, data_type: DataType::U8, ..Default::default() };
            let r = wbraster::Raster::from_data_native(cfg, data).unwrap();
            let options = crate::RasterWriteOptions { driver: Some(RasterDriver::GTiff), ..Default::default() };
            crate::write_raster_dataset(&RasterDataset::with_driver(r, RasterDriver::GTiff, false), path, &options).unwrap();
        };
        write(&tif, RasterData::U8(vec![7; 64]));
        write(&format!("{tif}.msk"), RasterData::U8((0..64).map(|i| if i < 32 { 0 } else { 255 }).collect()));
        let mut r = LazyRaster::open(&tif).unwrap();
        assert!(r.has_mask());
        let mask = r.read_mask(Window::full(8, 8), None).unwrap().unwrap();
        assert_eq!(mask.iter().filter(|v| **v == 0).count(), 32);
        crate::config::push_scope(std::collections::HashMap::from([(
            "GDAL_DISABLE_READDIR_ON_OPEN".to_string(),
            Some("EMPTY_DIR".to_string()),
        )]));
        let opened = LazyRaster::open(&tif);
        crate::config::pop_scope();
        assert!(!opened.unwrap().has_mask(), "EMPTY_DIR: no sidecar is read");
    }

    /// Samples of `data_type`, `n` of them, from a deterministic sequence of `seed`.
    fn sequence(data_type: DataType, n: usize, seed: usize) -> RasterData {
        let at = |i: usize| (i * 7 + seed * 13) % 251;
        match data_type {
            DataType::U8 => RasterData::U8((0..n).map(|i| at(i) as u8).collect()),
            DataType::I16 => RasterData::I16((0..n).map(|i| at(i) as i16 * 100 - 9000).collect()),
            DataType::U16 => RasterData::U16((0..n).map(|i| at(i) as u16 * 200).collect()),
            _ => RasterData::F32((0..n).map(|i| at(i) as f32 * 0.25 - 10.0).collect()),
        }
    }

    /// Writes a tiled LZW GeoTIFF of 16-pixel tiles at `path`.
    fn write_tiled_tiff(path: &std::path::Path, (cols, rows, bands): (usize, usize, usize), data: RasterData, options: &[(&str, &str)]) {
        let cfg = wbraster::RasterConfig { cols, rows, bands, data_type: data.data_type(), ..Default::default() };
        let r = wbraster::Raster::from_data_native(cfg, data).unwrap();
        let options = crate::RasterWriteOptions {
            driver: Some(RasterDriver::GTiff),
            compression: Some("LZW".into()),
            tile_size: Some(16),
            creation_options: options.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        };
        crate::write_raster_dataset(&RasterDataset::with_driver(r, RasterDriver::GTiff, false), path.to_str().unwrap(), &options).unwrap();
    }

    /// The windows of a pseudo-random sequence within an image.
    fn some_windows(width: usize, height: usize, n: usize) -> Vec<Window> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = |bound: usize| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((state >> 33) as usize) % bound
        };
        (0..n)
            .map(|_| {
                let (x, y) = (next(width), next(height));
                Window::new(x, y, 1 + next(width - x), 1 + next(height - y))
            })
            .collect()
    }

    /// A window of `bands` read as `f64`, band after band.
    fn read_f64(r: &mut LazyRaster, w: Window, bands: &[usize], overview: Option<usize>) -> Vec<f64> {
        r.read_window(w, &WindowOptions { bands: bands.to_vec(), overview }).unwrap().raster.data.to_f64_vec()
    }

    /// A window read again finds every chunk in the cache: no chunk is decoded, and the values are
    /// the same.
    #[test]
    fn a_repeated_window_read_decodes_no_chunk() {
        let _serial = crate::block_cache::tests::cache_test_guard(64 << 20);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiled.tif");
        write_tiled_tiff(&path, (70, 50, 1), sequence(DataType::U16, 70 * 50, 1), &[]);
        let mut r = LazyRaster::open(path.to_str().unwrap()).unwrap();
        let w = Window::new(5, 3, 50, 40);
        let first = read_f64(&mut r, w, &[1], None);
        let (hits, misses) = crate::block_cache::tests::take_lookups();
        assert_eq!((hits, misses), (0, 4 * 3), "the first read decodes the 12 tiles it covers");
        let second = read_f64(&mut r, w, &[1], None);
        assert_eq!(crate::block_cache::tests::take_lookups(), (12, 0), "the second read decodes no tile");
        assert_eq!(first, second);
        // A window over some of the same tiles and a new one decodes only the new one.
        let edge = Window::new(40, 0, 30, 10);
        read_f64(&mut r, edge, &[1], None);
        assert_eq!(crate::block_cache::tests::take_lookups(), (2, 1));
        read_f64(&mut r, edge, &[1], None);
        assert_eq!(crate::block_cache::tests::take_lookups(), (3, 0));
    }

    /// Multi-chunk window reads reuse the worker decoders that earlier reads opened, at full
    /// resolution and on overviews, rather than open (and read the chunk tables of) one decoder
    /// per worker each time: however many reads, no more decoders are opened than there are
    /// threads. The values are those of a reader opened for each read. With a single thread there
    /// are no workers, and no decoder is opened.
    #[test]
    fn window_reads_reuse_the_worker_decoders_of_earlier_reads() {
        // Off, so that every read decodes its chunks and needs the workers.
        let _serial = crate::block_cache::tests::cache_test_guard(0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiled.tif");
        let (cols, rows, bands) = (160, 144, 3);
        let cfg = wbraster::RasterConfig { cols, rows, bands, data_type: DataType::U8, ..Default::default() };
        let r = wbraster::Raster::from_data_native(cfg, sequence(DataType::U8, cols * rows * bands, 3)).unwrap();
        let options = crate::RasterWriteOptions {
            driver: Some(RasterDriver::GTiff),
            compression: Some("DEFLATE".into()),
            tile_size: Some(16),
            overviews: vec![2],
            ..Default::default()
        };
        crate::write_raster_dataset(&RasterDataset::with_driver(r, RasterDriver::GTiff, false), path.to_str().unwrap(), &options).unwrap();
        let path = path.to_str().unwrap();
        // Windows of 4 x 4 to 9 x 9 tiles, full resolution and overview mixed.
        let reads: Vec<(Window, Option<usize>)> = vec![
            (Window::new(3, 5, 120, 100), None),
            (Window::new(17, 9, 64, 64), None),
            (Window::new(0, 0, 70, 66), Some(0)),
            (Window::new(40, 33, 100, 90), None),
            (Window::new(8, 4, 60, 60), Some(0)),
            (Window::new(3, 5, 120, 100), None),
        ];
        let mut reader = LazyRaster::open(path).unwrap();
        // A budget of its own: the readers of the tests running alongside close none of its decoders.
        match &mut reader.inner {
            Inner::Tiff(t) => {
                t.workers = crate::geotiff::DecoderPool::sharing(Box::leak(Box::new(crate::geotiff::SpareDecoders::new(1 << 20))))
            }
            _ => panic!("a GeoTIFF reader"),
        }
        let opened = |r: &LazyRaster| match &r.inner {
            Inner::Tiff(t) => t.workers.opened.load(std::sync::atomic::Ordering::Relaxed),
            _ => panic!("a GeoTIFF reader"),
        };
        let threads = crate::par::threads();
        let first = read_f64(&mut reader, reads[0].0, &[1, 2, 3], reads[0].1);
        for _ in 0..3 {
            for (w, overview) in &reads {
                let fresh = read_f64(&mut LazyRaster::open(path).unwrap(), *w, &[3, 1], *overview);
                assert_eq!(read_f64(&mut reader, *w, &[3, 1], *overview), fresh, "{w:?} on {overview:?}");
            }
        }
        assert_eq!(read_f64(&mut reader, reads[0].0, &[1, 2, 3], reads[0].1), first);
        // A decoder is opened only when the pool has none to spare, whatever order the workers run
        // in; opening one per worker would open one per thread on every one of these 20 reads.
        let most = if threads > 1 { threads } else { 0 };
        assert!(opened(&reader) <= most, "{} decoders opened on {threads} threads", opened(&reader));
        // No lower bound with several threads: the decode workers are shared by the whole process,
        // so reads of tests running alongside can hold them all and every read here then decodes
        // alone, which opens none.
        if threads == 1 {
            assert_eq!(opened(&reader), 0, "a single thread decodes with the reader's own decoder");
        }
        match &reader.inner {
            Inner::Tiff(t) => assert!(t.workers.len() <= most),
            _ => unreachable!(),
        }
    }

    /// The open rasters keep the spare decoders of their workers within one budget. With a budget
    /// that holds no complete decoder, only the raster read last keeps its own: the others open theirs again on
    /// their next read, and read the same values. A raster that is closed gives up its share.
    #[test]
    fn open_rasters_keep_their_spare_decoders_within_one_budget() {
        // Off, so that every read decodes its chunks and needs the workers.
        let _serial = crate::block_cache::tests::cache_test_guard(0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiled.tif");
        write_tiled_tiff(&path, (96, 96, 1), sequence(DataType::U8, 96 * 96, 5), &[]);
        let path = path.to_str().unwrap();
        // A budget of their own, apart from the rasters of the tests running alongside.
        let spares: &'static crate::geotiff::SpareDecoders = Box::leak(Box::new(crate::geotiff::SpareDecoders::new(1)));
        let mut readers: Vec<LazyRaster> = (0..3)
            .map(|_| {
                let mut r = LazyRaster::open(path).unwrap();
                match &mut r.inner {
                    Inner::Tiff(t) => t.workers = crate::geotiff::DecoderPool::sharing(spares),
                    _ => panic!("a GeoTIFF reader"),
                }
                r
            })
            .collect();
        let kept = |readers: &[LazyRaster]| -> Vec<usize> {
            let of = |r: &LazyRaster| match &r.inner {
                Inner::Tiff(t) => t.workers.len(),
                _ => unreachable!(),
            };
            readers.iter().map(of).collect()
        };
        // What one spare decoder counts for: the fixed part and the tables of 6 x 6 tiles.
        let each = crate::geotiff::SPARE_DECODER_OVERHEAD + 16 * 36;
        let w = Window::new(1, 2, 90, 90);
        let fresh = read_f64(&mut LazyRaster::open(path).unwrap(), w, &[1], None);
        for _ in 0..3 {
            for i in 0..readers.len() {
                assert_eq!(read_f64(&mut readers[i], w, &[1], None), fresh);
                // The decoders this read gave back closed those of the other rasters. No lower
                // bound on how many it gave back: the decode workers are shared by the whole
                // process, and a read that finds none free decodes alone.
                let kept = kept(&readers);
                assert!(kept.iter().filter(|&&n| n > 0).count() <= 1, "{kept:?} after reading raster {i}");
                assert!(kept[i] <= crate::par::threads(), "{kept:?}");
                assert_eq!(spares.bytes(), kept.iter().sum::<usize>() as u64 * each);
            }
        }
        while let Some(reader) = readers.pop() {
            drop(reader);
            assert_eq!(spares.bytes(), kept(&readers).iter().sum::<usize>() as u64 * each);
        }
        assert_eq!(spares.bytes(), 0);
    }

    /// Window reads on several threads at once share one budget of decode workers: never more
    /// worker threads at a time than [`crate::geotiff::decode_worker_limit`] (the cores, or the
    /// thread count when higher), however many reads start them, with the values of a read alone.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn concurrent_window_reads_share_the_decode_workers() {
        // Off, so that every read decodes its chunks and needs the workers.
        let _serial = crate::block_cache::tests::cache_test_guard(0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiled.tif");
        let (cols, rows, bands) = (480, 480, 3);
        let cfg = wbraster::RasterConfig { cols, rows, bands, data_type: DataType::U8, ..Default::default() };
        let r = wbraster::Raster::from_data_native(cfg, sequence(DataType::U8, cols * rows * bands, 3)).unwrap();
        let options = crate::RasterWriteOptions {
            driver: Some(RasterDriver::GTiff),
            compression: Some("DEFLATE".into()),
            tile_size: Some(16),
            ..Default::default()
        };
        crate::write_raster_dataset(&RasterDataset::with_driver(r, RasterDriver::GTiff, false), path.to_str().unwrap(), &options).unwrap();
        let path = path.to_str().unwrap();
        // 30 x 30 tiles: enough that every read keeps its workers busy at once
        let w = Window::new(3, 5, 470, 460);
        let alone = read_f64(&mut LazyRaster::open(path).unwrap(), w, &[1, 2, 3], None);
        crate::geotiff::PEAK_WORKERS.store(0, std::sync::atomic::Ordering::Relaxed);
        std::thread::scope(|s| {
            for _ in 0..8 {
                s.spawn(|| {
                    let mut r = LazyRaster::open(path).unwrap();
                    for _ in 0..5 {
                        assert_eq!(read_f64(&mut r, w, &[1, 2, 3], None), alone);
                    }
                });
            }
        });
        let peak = crate::geotiff::PEAK_WORKERS.load(std::sync::atomic::Ordering::Relaxed);
        let limit = crate::geotiff::decode_worker_limit();
        assert!(peak <= limit, "{peak} decode workers at once, over the {limit} allowed");
    }

    /// A remote file read through a `SparseFile` (terrio-js's `openUrl`) decodes its tiles on the
    /// workers, as a local file does, and reads the same values. With the TIFF's header dropped
    /// from the sparse cache after a read, the next read still returns the same values.
    #[test]
    fn sparse_window_reads_decode_on_the_workers() {
        let _serial = crate::block_cache::tests::cache_test_guard(0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiled.tif");
        write_tiled_tiff(&path, (160, 144, 3), sequence(DataType::U8, 160 * 144 * 3, 5), &[]);
        let bytes = std::fs::read(&path).unwrap();
        // Keep header and window data in different blocks so prefetch cannot restore
        // the header incidentally after trimming.
        let file = crate::sparse::SparseFile::with_block_size(bytes.len() as u64, 256);
        let header_fetches = std::cell::Cell::new(0usize);
        let fetch = |o: u64, n: u64| -> std::io::Result<Vec<u8>> {
            if o == 0 {
                header_fetches.set(header_fetches.get() + 1);
            }
            Ok(bytes[o as usize..(o + n) as usize].to_vec())
        };
        let open = || crate::sparse::read_with(&file, 32, fetch, || Ok(LazyRaster::from_sparse(file.clone())?.unwrap())).unwrap();
        let w = Window::new(20, 20, 120, 100);
        let want = read_f64(&mut LazyRaster::open(path.to_str().unwrap()).unwrap(), w, &[1, 2, 3], None);
        let opts = WindowOptions { bands: vec![1, 2, 3], overview: None };
        let read = |r: &mut LazyRaster| {
            crate::sparse::read_with(&file, 32, fetch, || Ok(r.read_window(w, &opts)?.raster.data.to_f64_vec())).unwrap()
        };
        let opened = |r: &LazyRaster| match &r.inner {
            Inner::Tiff(t) => t.workers.opened.load(std::sync::atomic::Ordering::Relaxed),
            _ => panic!("a GeoTIFF reader"),
        };
        // Tests running alongside may hold the process's decode workers: a read then decodes on
        // its own thread. Fresh readers until one gets workers (20 at most).
        let mut r = open();
        assert_eq!(read(&mut r), want);
        for _ in 0..20 {
            if crate::par::threads() < 2 || opened(&r) >= 1 {
                break;
            }
            r = open();
            assert_eq!(read(&mut r), want);
        }
        if crate::par::threads() > 1 {
            assert!(opened(&r) >= 1, "no read opened decoders for its workers over the sparse file");
        }
        // Force the next read to reopen workers rather than reuse cached decoders.
        static SPARES: crate::geotiff::SpareDecoders = crate::geotiff::SpareDecoders::new(0);
        if let Inner::Tiff(t) = &mut r.inner {
            t.workers = crate::geotiff::DecoderPool::sharing(&SPARES);
        }
        file.trim(0);
        let before = header_fetches.get();
        assert!(before > 0, "the opening read must fetch the TIFF header");
        assert_eq!(read(&mut r), want);
        assert_eq!(header_fetches.get(), before, "worker reopening must not refetch the trimmed header");
    }

    /// `place` matches a pixel-by-pixel copy of each sample of pixel-interleaved chunks, over:
    /// samples sent to any bands, chunks cropped by the window on every side, a chunk not reaching
    /// the window, and a truncated chunk, whose missing samples leave the output as it was.
    #[test]
    fn interleaved_chunks_are_placed_sample_by_sample() {
        fn by_pixel(out: &mut [u16], vals: &[u16], g: &ChunkGeom, w: Window, map: &[(usize, usize)]) {
            let stride = vals.len() / (g.dh * g.spp);
            for row in g.y0.max(w.y)..(g.y0 + g.dh).min(w.y + w.height) {
                for col in g.x0.max(w.x)..(g.x0 + g.dw).min(w.x + w.width) {
                    let src = ((row - g.y0) * stride + col - g.x0) * g.spp;
                    for &(sample, band) in map {
                        if let Some(v) = vals.get(src + sample) {
                            out[band * w.width * w.height + (row - w.y) * w.width + col - w.x] = *v;
                        }
                    }
                }
            }
        }
        let (dw, dh, spp) = (16, 12, 3);
        let full: Vec<u16> = (0..(dw * dh * spp) as u16).collect();
        let maps: [&[(usize, usize)]; 3] = [&[(0, 0), (1, 1), (2, 2)], &[(2, 0), (0, 1)], &[(1, 1)]];
        let windows = [Window::new(0, 0, 16, 12), Window::new(5, 3, 20, 20), Window::new(10, 7, 4, 3), Window::new(18, 0, 4, 4)];
        let g = ChunkGeom { x0: 2, y0: 1, dw, dh, spp };
        for vals in [&full[..], &full[..full.len() - 50]] {
            for map in maps {
                for w in windows {
                    let mut want = vec![u16::MAX; w.width * w.height * 3];
                    let mut got = want.clone();
                    by_pixel(&mut want, vals, &g, w, map);
                    place(&mut got, vals, &g, w, map);
                    assert_eq!(got, want, "{w:?} {map:?} {} samples", vals.len());
                }
            }
        }
    }

    /// With the cache off, reads decode every time and look nothing up.
    #[test]
    fn a_disabled_cache_decodes_every_read() {
        let _serial = crate::block_cache::tests::cache_test_guard(0);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiled.tif");
        write_tiled_tiff(&path, (40, 40, 1), sequence(DataType::U8, 1600, 2), &[]);
        let mut r = LazyRaster::open(path.to_str().unwrap()).unwrap();
        let first = read_f64(&mut r, Window::full(40, 40), &[1], None);
        assert_eq!(first, read_f64(&mut r, Window::full(40, 40), &[1], None));
        assert_eq!(crate::block_cache::tests::take_lookups(), (0, 0));
    }

    /// Random windows read the same values with the cache on (twice, the second time from it) as
    /// with it off, for several sample types, and for pixel-interleaved, band-interleaved and
    /// multi-band images.
    #[test]
    fn random_windows_read_the_same_with_the_cache_on_and_off() {
        let _serial = crate::block_cache::tests::cache_test_guard(64 << 20);
        let dir = tempfile::tempdir().unwrap();
        type Case<'a> = (DataType, usize, &'a [(&'a str, &'a str)]);
        let cases: [Case; 6] = [
            (DataType::U8, 1, &[]),
            (DataType::U16, 3, &[]),
            (DataType::I16, 3, &[("INTERLEAVE", "BAND")]),
            (DataType::F32, 1, &[]),
            (DataType::F32, 2, &[("INTERLEAVE", "BAND")]),
            (DataType::U8, 3, &[("INTERLEAVE", "PIXEL")]),
        ];
        for (k, (data_type, bands, options)) in cases.iter().enumerate() {
            let (cols, rows) = (83, 57);
            let path = dir.path().join(format!("case{k}.tif"));
            write_tiled_tiff(&path, (cols, rows, *bands), sequence(*data_type, cols * rows * bands, k), options);
            let windows = some_windows(cols, rows, 25);
            let band_sets: [Vec<usize>; 2] = [(1..=*bands).collect(), vec![*bands, 1]];
            let read_all = |r: &mut LazyRaster| -> Vec<Vec<f64>> {
                windows.iter().enumerate().map(|(i, w)| read_f64(r, *w, &band_sets[i % 2], None)).collect()
            };
            crate::block_cache::set_max_bytes(0);
            let off = read_all(&mut LazyRaster::open(path.to_str().unwrap()).unwrap());
            crate::block_cache::set_max_bytes(64 << 20);
            let mut r = LazyRaster::open(path.to_str().unwrap()).unwrap();
            assert_eq!(read_all(&mut r), off, "case {k}: the first pass");
            let (_, misses) = crate::block_cache::tests::take_lookups();
            assert!(misses > 0);
            assert_eq!(read_all(&mut r), off, "case {k}: the second pass");
            assert_eq!(crate::block_cache::tests::take_lookups().1, 0, "case {k}: the second pass decodes nothing");
        }
    }

    /// A cache too small for a read's chunks still reads the right values.
    #[test]
    fn a_small_cache_still_reads_the_same_values() {
        let _serial = crate::block_cache::tests::cache_test_guard(3000);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiled.tif");
        write_tiled_tiff(&path, (96, 96, 1), sequence(DataType::U16, 96 * 96, 3), &[]);
        let mut r = LazyRaster::open(path.to_str().unwrap()).unwrap();
        let expected = sequence(DataType::U16, 96 * 96, 3).to_f64_vec();
        for w in some_windows(96, 96, 20) {
            let got = read_f64(&mut r, w, &[1], None);
            let want: Vec<f64> =
                (w.y..w.y + w.height).flat_map(|y| (w.x..w.x + w.width).map(move |x| (y, x))).map(|(y, x)| expected[y * 96 + x]).collect();
            assert_eq!(got, want, "{w:?}");
        }
    }

    /// Half-float chunks are cached as decoded and converted to `f32` when placed.
    #[test]
    fn half_float_chunks_are_read_from_the_cache() {
        let _serial = crate::block_cache::tests::cache_test_guard(64 << 20);
        let values: Vec<half::f16> = (0..8).map(|i| half::f16::from_f32(i as f32 * 0.5 - 1.0)).collect();
        let data: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let tags: [(u16, u16, u32); 10] = [
            (256, 3, 4),
            (257, 3, 2),
            (258, 3, 16),
            (259, 3, 1),
            (262, 3, 1),
            (273, 4, 0),
            (277, 3, 1),
            (278, 3, 2),
            (279, 4, 16),
            (339, 3, 3),
        ];
        let data_offset = 8 + 2 + tags.len() as u32 * 12 + 4;
        let mut b = b"II*\0".to_vec();
        b.extend(8u32.to_le_bytes());
        b.extend((tags.len() as u16).to_le_bytes());
        for (tag, typ, value) in tags {
            b.extend(tag.to_le_bytes());
            b.extend(typ.to_le_bytes());
            b.extend(1u32.to_le_bytes());
            b.extend((if tag == 273 { data_offset } else { value }).to_le_bytes());
        }
        b.extend(0u32.to_le_bytes());
        b.extend(&data);
        let mut r = LazyRaster::from_tiff_bytes(b).unwrap().unwrap();
        assert_eq!(r.data_type(), DataType::F32);
        let want: Vec<f64> = values.iter().map(|v| f64::from(v.to_f32())).collect();
        let w = Window::new(1, 0, 3, 2);
        let sub: Vec<f64> = [1, 2, 3, 5, 6, 7].iter().map(|&i| want[i]).collect();
        assert_eq!(read_f64(&mut r, w, &[1], None), sub);
        assert_eq!(crate::block_cache::tests::take_lookups(), (0, 1));
        assert_eq!(read_f64(&mut r, w, &[1], None), sub);
        assert_eq!(read_f64(&mut r, Window::full(4, 2), &[1], None), want);
        assert_eq!(crate::block_cache::tests::take_lookups(), (2, 0));
    }

    /// The chunks of an external `.ovr` level are cached too, apart from the TIFF's own.
    #[test]
    fn external_overview_windows_read_the_same_from_the_cache() {
        let _serial = crate::block_cache::tests::cache_test_guard(64 << 20);
        let dir = tempfile::tempdir().unwrap();
        let tif = dir.path().join("img.tif");
        write_tiled_tiff(&tif, (80, 64, 1), sequence(DataType::U8, 80 * 64, 4), &[]);
        let ovr = dir.path().join("ovr.tif");
        write_tiled_tiff(&ovr, (40, 32, 1), sequence(DataType::U8, 40 * 32, 5), &[]);
        std::fs::rename(&ovr, dir.path().join("img.tif.ovr")).unwrap();
        let mut r = LazyRaster::open(tif.to_str().unwrap()).unwrap();
        assert_eq!(r.overviews(), [(40, 32)]);
        let w = Window::new(3, 2, 30, 25);
        let first = read_f64(&mut r, w, &[1], Some(0));
        let (hits, misses) = crate::block_cache::tests::take_lookups();
        assert_eq!((hits, misses), (0, 6));
        assert_eq!(read_f64(&mut r, w, &[1], Some(0)), first);
        assert_eq!(crate::block_cache::tests::take_lookups(), (6, 0));
        // The full-resolution level has chunks of its own, not the overview's.
        let full = read_f64(&mut r, Window::new(3, 2, 30, 25), &[1], None);
        assert_eq!(crate::block_cache::tests::take_lookups().0, 0);
        let expected = sequence(DataType::U8, 80 * 64, 4).to_f64_vec();
        assert_eq!(full[0], expected[2 * 80 + 3]);
    }

    /// GDAL 3.13.3 treats an external overview as its own TIFF, even at the base size.
    #[test]
    fn equal_sized_external_overview_does_not_claim_the_base_internal_mask() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image.tif");
        let cfg = wbraster::RasterConfig { cols: 16, rows: 16, bands: 1, data_type: DataType::U8, ..Default::default() };
        let dataset = RasterDataset::with_driver(
            wbraster::Raster::from_data_native(cfg, RasterData::U8(vec![7; 256])).unwrap(),
            RasterDriver::GTiff,
            false,
        );
        crate::write_raster_dataset(
            &dataset,
            path.to_str().unwrap(),
            &crate::RasterWriteOptions { mask: Some(vec![0; 256]), ..Default::default() },
        )
        .unwrap();
        {
            let mut output = std::fs::File::create(dir.path().join("image.tif.ovr")).unwrap();
            let mut encoder = tiff::encoder::TiffEncoder::new(&mut output).unwrap();
            let mut image = encoder.new_image::<tiff::encoder::colortype::Gray8>(16, 16).unwrap();
            image.encoder().write_tag(Tag::NewSubfileType, 1u32).unwrap();
            image.write_data(&[9u8; 256]).unwrap();
        }
        let mut source = LazyRaster::open(path.to_str().unwrap()).unwrap();
        assert_eq!(source.overviews(), [(16, 16)]);
        assert_eq!(source.read_mask(Window::full(16, 16), None).unwrap(), Some(vec![0; 256]));
        assert!(!source.has_overview_mask(0));
        assert!(!source.level_has_own_mask(0));
        assert_eq!(source.read_mask(Window::full(16, 16), Some(0)).unwrap(), None);
    }

    /// Readers of different files never share chunks, though their chunks have the same indexes.
    #[test]
    fn readers_of_different_files_never_share_chunks() {
        let _serial = crate::block_cache::tests::cache_test_guard(64 << 20);
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.tif"), dir.path().join("b.tif"));
        write_tiled_tiff(&a, (64, 64, 1), sequence(DataType::U8, 64 * 64, 1), &[]);
        write_tiled_tiff(&b, (64, 64, 1), sequence(DataType::U8, 64 * 64, 2), &[]);
        let (mut ra, mut rb) = (LazyRaster::open(a.to_str().unwrap()).unwrap(), LazyRaster::open(b.to_str().unwrap()).unwrap());
        let w = Window::full(64, 64);
        let (va, vb) = (read_f64(&mut ra, w, &[1], None), read_f64(&mut rb, w, &[1], None));
        assert_ne!(va, vb);
        assert_eq!(va, sequence(DataType::U8, 64 * 64, 1).to_f64_vec());
        assert_eq!(read_f64(&mut ra, w, &[1], None), va);
        assert_eq!(read_f64(&mut rb, w, &[1], None), vb);
    }

    /// A file replaced by a rename under an open reader (#432): the reader keeps reading the
    /// original, as GDAL does, and a reader opened after the rename reads the new file, never
    /// the old file's cached chunks.
    #[test]
    fn a_file_replaced_under_an_open_reader_keeps_reading_the_original() {
        let _serial = crate::block_cache::tests::cache_test_guard(64 << 20);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("img.tif");
        let (old, new) = (sequence(DataType::U8, 64 * 64, 1), sequence(DataType::U8, 64 * 64, 2));
        write_tiled_tiff(&path, (64, 64, 1), old.clone(), &[]);
        let w = Window::full(64, 64);
        let mut first = LazyRaster::open(path.to_str().unwrap()).unwrap();
        // Part of the file is read, and cached, before the replacement.
        let corner = read_f64(&mut first, Window::new(0, 0, 20, 20), &[1], None);
        assert_eq!(corner[0], old.to_f64_vec()[0]);
        let replacement = dir.path().join("replacement.tif");
        write_tiled_tiff(&replacement, (64, 64, 1), new.clone(), &[]);
        std::fs::rename(&replacement, &path).unwrap();
        let mut second = LazyRaster::open(path.to_str().unwrap()).unwrap();
        assert_eq!(read_f64(&mut second, w, &[1], None), new.to_f64_vec());
        assert_eq!(read_f64(&mut first, w, &[1], None), old.to_f64_vec());
        assert_eq!(read_f64(&mut first, w, &[1], None), old.to_f64_vec());
    }

    /// A reader that is closed gives back its chunks.
    #[test]
    fn a_closed_reader_drops_its_cached_chunks() {
        let _serial = crate::block_cache::tests::cache_test_guard(64 << 20);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiled.tif");
        write_tiled_tiff(&path, (64, 64, 1), sequence(DataType::U8, 64 * 64, 1), &[]);
        let file = std::fs::File::open(&path).unwrap();
        let source = Source::File(BufReader::new(crate::geotiff::FileCursor::new(file)));
        let mut r = TiffRaster::new(source, None, TiffReadLimits::default()).unwrap();
        r.read_samples(0, Window::full(64, 64), &[1]).unwrap();
        let handle = r.cache_handle;
        assert!(crate::block_cache::get(handle, 0, 0).is_some());
        drop(r);
        assert!(crate::block_cache::get(handle, 0, 0).is_none());
    }

    /// Each level keeps the storage (planar or not, compression) of its own image: a full
    /// resolution image of pixel-interleaved LZW tiles and an external overview of
    /// band-interleaved Deflate ones, read in alternation and repeatedly, decode as they are
    /// stored, never as the other level was.
    #[test]
    fn levels_of_different_storage_are_each_decoded_as_stored() {
        // No cached chunk: every read decodes, so a wrong cached storage would show.
        let _serial = crate::block_cache::tests::cache_test_guard(0);
        let dir = tempfile::tempdir().unwrap();
        let tif = dir.path().join("img.tif");
        write_tiled_tiff(&tif, (64, 48, 3), sequence(DataType::U8, 64 * 48 * 3, 6), &[("INTERLEAVE", "PIXEL")]);
        let ovr = dir.path().join("img.tif.ovr");
        let small = sequence(DataType::U8, 32 * 24 * 3, 8);
        let cfg = wbraster::RasterConfig { cols: 32, rows: 24, bands: 3, data_type: DataType::U8, ..Default::default() };
        let options = crate::RasterWriteOptions {
            driver: Some(RasterDriver::GTiff),
            compression: Some("DEFLATE".into()),
            tile_size: Some(16),
            creation_options: vec![("INTERLEAVE".into(), "BAND".into())],
            ..Default::default()
        };
        let dataset =
            RasterDataset::with_driver(wbraster::Raster::from_data_native(cfg, small.clone()).unwrap(), RasterDriver::GTiff, false);
        crate::write_raster_dataset(&dataset, ovr.to_str().unwrap(), &options).unwrap();

        // What each file's tags say, read independently of the reader under test.
        let stored = |path: &std::path::Path| {
            let mut dec = Decoder::new(BufReader::new(std::fs::File::open(path).unwrap())).unwrap();
            let planar = dec.find_tag_unsigned::<u16>(Tag::PlanarConfiguration).unwrap() == Some(2);
            Storage { planar, compression: dec.find_tag_unsigned::<u16>(Tag::Compression).unwrap().unwrap() }
        };
        let (full_storage, small_storage) = (stored(&tif), stored(&ovr));
        assert_eq!(full_storage, Storage { planar: false, compression: 5 }, "LZW tiles, pixel-interleaved");
        assert!(small_storage.planar && small_storage.compression != 5, "Deflate tiles, band-interleaved: {small_storage:?}");

        let mut r = LazyRaster::open(tif.to_str().unwrap()).unwrap();
        assert_eq!(r.overviews(), [(32, 24)]);
        let expected = |data: &RasterData, (width, height): (usize, usize), w: Window| -> Vec<f64> {
            let data = data.to_f64_vec();
            (0..3)
                .flat_map(|b| (w.y..w.y + w.height).flat_map(move |y| (w.x..w.x + w.width).map(move |x| (b, y, x))))
                .map(|(b, y, x)| data[(b * height + y) * width + x])
                .collect()
        };
        let full = sequence(DataType::U8, 64 * 48 * 3, 6);
        let (wf, ws) = (Window::new(5, 3, 40, 30), Window::new(2, 1, 25, 20));
        for pass in 0..2 {
            assert_eq!(read_f64(&mut r, wf, &[1, 2, 3], None), expected(&full, (64, 48), wf), "full resolution, pass {pass}");
            assert_eq!(read_f64(&mut r, ws, &[1, 2, 3], Some(0)), expected(&small, (32, 24), ws), "overview, pass {pass}");
        }
        let Inner::Tiff(tiff) = &r.inner else { panic!("not a TIFF reader") };
        assert_eq!(tiff.levels[0].storage, Some(full_storage));
        let own = tiff.ovr.as_ref().expect("the external overview is attached");
        assert_eq!(own.levels[0].storage, Some(small_storage));
    }

    /// Float16 samples of a pixel-interleaved image under the floating-point predictor, stored in
    /// strips that the windows cross, read lazily as the values written. The strips are built here
    /// with the predictor applied by hand (byte planes of each row, differenced by sample), apart
    /// from terrio's decoding of it, compressed with Deflate (left to the `tiff` crate) and with
    /// Zstandard (decoded by terrio's own chunk codec).
    #[test]
    fn pixel_interleaved_float16_multiband_windows_undo_the_floating_point_predictor() {
        use std::io::Write;
        let _serial = crate::block_cache::tests::cache_test_guard(64 << 20);
        let (width, height, spp, rows_per_strip) = (6usize, 5usize, 3usize, 2usize);
        let values: Vec<half::f16> = (0..width * height * spp)
            .map(|i| half::f16::from_f32((i as f32 * 0.37 - 20.0) * if i % 4 == 0 { -1.0 } else { 1.0 }))
            .collect();
        // Deflate strips are decoded by the `tiff` crate, Zstandard ones by terrio's `ChunkCodec`.
        for compression in [8u16, 50000] {
            let mut strips: Vec<Vec<u8>> = Vec::new();
            for strip in values.chunks(rows_per_strip * width * spp) {
                let mut stored = Vec::new();
                for row in strip.chunks(width * spp) {
                    // Most significant bytes of every sample first, then the next ones; then each byte
                    // less the one a pixel's samples before it.
                    let planes: Vec<u8> = (0..2).flat_map(|b| row.iter().map(move |v| v.to_be_bytes()[b])).collect();
                    stored.extend((0..planes.len()).map(|i| if i < spp { planes[i] } else { planes[i].wrapping_sub(planes[i - spp]) }));
                }
                strips.push(if compression == 8 {
                    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                    z.write_all(&stored).unwrap();
                    z.finish().unwrap()
                } else {
                    crate::zstandard::compress(&stored, 9).unwrap()
                });
            }
            // A classic little-endian TIFF: the directory, the arrays it points to, then the strips.
            let n = strips.len();
            let entries: u32 = 12;
            let arrays = 8 + 2 + entries * 12 + 4;
            let (bits_at, format_at, offsets_at, counts_at) = (arrays, arrays + 6, arrays + 12, arrays + 12 + 4 * n as u32);
            let mut at = counts_at + 4 * n as u32;
            let offsets: Vec<u32> = strips
                .iter()
                .map(|s| {
                    at += s.len() as u32;
                    at - s.len() as u32
                })
                .collect();
            let shorts = |v: u32| (3u16, 1u32, v);
            let tags: [(u16, (u16, u32, u32)); 12] = [
                (256, shorts(width as u32)),
                (257, shorts(height as u32)),
                (258, (3, 3, bits_at)),
                (259, shorts(u32::from(compression))),
                (262, shorts(2)),
                (273, (4, n as u32, offsets_at)),
                (277, shorts(spp as u32)),
                (278, shorts(rows_per_strip as u32)),
                (279, (4, n as u32, counts_at)),
                (284, shorts(1)),
                (317, shorts(3)),
                (339, (3, 3, format_at)),
            ];
            let mut b = b"II*\0".to_vec();
            b.extend(8u32.to_le_bytes());
            b.extend((tags.len() as u16).to_le_bytes());
            for (tag, (typ, count, value)) in tags {
                b.extend(tag.to_le_bytes());
                b.extend(typ.to_le_bytes());
                b.extend(count.to_le_bytes());
                b.extend(value.to_le_bytes());
            }
            b.extend(0u32.to_le_bytes());
            b.extend([16u16, 16, 16, 3, 3, 3].iter().flat_map(|v| v.to_le_bytes()));
            b.extend(offsets.iter().flat_map(|v| v.to_le_bytes()));
            b.extend(strips.iter().flat_map(|s| (s.len() as u32).to_le_bytes()));
            for s in &strips {
                b.extend(s);
            }
            let mut r = LazyRaster::from_tiff_bytes(b).unwrap().unwrap();
            assert_eq!((r.data_type(), r.band_count()), (DataType::F32, 3));
            let expected = |w: Window, bands: &[usize]| -> Vec<f64> {
                bands
                    .iter()
                    .flat_map(|&b| (w.y..w.y + w.height).flat_map(move |y| (w.x..w.x + w.width).map(move |x| (b, y, x))))
                    .map(|(b, y, x)| f64::from(values[(y * width + x) * spp + b - 1].to_f32()))
                    .collect()
            };
            // Windows inside a strip, across strips, and of single bands.
            for (w, bands) in [
                (Window::full(width, height), vec![1, 2, 3]),
                (Window::new(1, 1, 4, 3), vec![1, 2, 3]),
                (Window::new(2, 0, 3, 5), vec![3, 1]),
                (Window::new(0, 4, 6, 1), vec![2]),
            ] {
                assert_eq!(read_f64(&mut r, w, &bands, None), expected(w, &bands), "compression {compression}, {w:?} bands {bands:?}");
            }
            // Whose decoder read them: terrio's for Zstandard, the `tiff` crate's for Deflate.
            let Inner::Tiff(tiff) = &r.inner else { panic!("not a TIFF reader") };
            assert_eq!(tiff.levels[0].codec.as_ref().map(Option::is_some), Some(compression != 8), "compression {compression}");
        }
    }
}

//! Overviews: their selection for warping, following GDAL's `-ovr` choices, and their management
//! in GeoTIFFs as `gdaladdo` and `gdal raster overview add/delete/refresh` manage them
//! ([`add_overviews`], [`delete_overviews`], [`refresh_overviews`]).
//!
//! Overviews are added to a GeoTIFF in place (internal overviews) or written to its `.ovr`
//! sidecar (external overviews, which [`crate::LazyRaster`] reads as the raster's overviews when
//! it has no internal ones). As GDAL does, each overview is computed from the previous (larger)
//! one being built, the valid pixels (not nodata, not masked out by a per-dataset mask) only,
//! internal overviews take the full-resolution image's compression, predictor and tile size (128
//! pixel tiles for a striped image), and a raster with an internal mask gets mask overviews. The
//! full-resolution image is read once, by strips of rows, rather than whole, for whole levels
//! (partial refreshes read the rows over their region). The file is changed through a copy
//! renamed over it, so a failure leaves it as it was.
//!
//! ```no_run
//! use terrio::ops::Resampling;
//! use terrio::ops::overview::{OverviewOptions, RefreshOptions, add_overviews, refresh_overviews};
//!
//! // gdaladdo -r average dem.tif 2 4 8
//! let o = OverviewOptions { levels: vec![2, 4, 8], resampling: Resampling::Average, ..Default::default() };
//! add_overviews("dem.tif", &o)?;
//! // gdal raster overview refresh --bbox 500000,4100000,510000,4110000 dem.tif
//! let r = RefreshOptions { bbox: vec![[500000.0, 4100000.0, 510000.0, 4110000.0]], ..Default::default() };
//! refresh_overviews("dem.tif", &r)?;
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::drivers::gtiff_update::{self, Appender, Directory, Entry, Tiff};
use crate::drivers::gtiff_write::{self as gw, CodecParams, Compression, Layout, OverviewResampling, Value};
use crate::ops::Resampling;
use crate::ops::overview_kernel as kernel;
use crate::{Error, LazyRaster, Result, Window, WindowOptions};
use std::io::{Read, Seek, Write};
use wbraster::DataType;
use wbraster::raster::RasterData;

/// Which stored source resolution a warp reads. Overview levels start at zero.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OverviewSelection {
    /// Choose the finest stored resolution close to the output's sampling ratio.
    #[default]
    Auto,
    /// Choose `n` levels finer than the automatic choice (`AUTO-n`).
    AutoOffset(usize),
    /// Always read full resolution (`NONE`).
    None,
    /// Read this overview, or the last available one if the level is too large.
    Level(usize),
}

impl OverviewSelection {
    /// Parses `AUTO`, `AUTO-n`, `NONE`, or an overview number, case-insensitively.
    pub fn parse(value: &str) -> Result<Self> {
        let word = value.to_ascii_uppercase();
        match word.as_str() {
            "AUTO" => Ok(Self::Auto),
            "NONE" => Ok(Self::None),
            _ => {
                if let Some(offset) = word.strip_prefix("AUTO-") {
                    return offset.parse().map(Self::AutoOffset).map_err(|_| invalid(value));
                }
                word.parse().map(Self::Level).map_err(|_| invalid(value))
            }
        }
    }

    pub(crate) fn select(self, width: usize, overviews: &[(usize, usize)], target_ratio: f64) -> Option<usize> {
        match self {
            Self::None => None,
            Self::Level(index) => {
                if index >= overviews.len() {
                    crate::warnings::warn(format!("cannot get overview level {index}; using the coarsest available source resolution"));
                }
                (!overviews.is_empty()).then(|| index.min(overviews.len() - 1))
            }
            Self::Auto | Self::AutoOffset(_) => {
                if !target_ratio.is_finite() || target_ratio <= 1.0 {
                    return None;
                }
                let mut best = None;
                let mut best_ratio = 0.0;
                // GDAL includes full resolution in the search and uses 0.1 as
                // an absolute ratio tolerance, rather than rounding powers of two.
                for i in 0..=overviews.len() {
                    let ratio = if i == 0 { 1.0 } else { width as f64 / overviews[i - 1].0 as f64 };
                    if ratio >= target_ratio + 0.1 || ratio <= best_ratio {
                        continue;
                    }
                    best = i.checked_sub(1);
                    best_ratio = ratio;
                    if (target_ratio - ratio).abs() < 0.1 {
                        break;
                    }
                }
                let offset = match self {
                    Self::AutoOffset(n) => n,
                    _ => 0,
                };
                best.and_then(|index| index.checked_sub(offset))
            }
        }
    }
}

/// How [`add_overviews`] builds overviews (`gdaladdo`, `gdal raster overview add`).
#[derive(Debug, Clone, PartialEq)]
pub struct OverviewOptions {
    /// The decimation factors (`2 4 8`). Empty for GDAL's automatic levels: factors of 2, 4, 8...
    /// until the overview fits in `min_size` pixels.
    pub levels: Vec<usize>,
    /// For automatic levels, the largest width or height of the smallest overview (256).
    pub min_size: usize,
    /// The resampling: nearest (the default), average, min, max or mode.
    pub resampling: Resampling,
    /// Write the overviews to the `.ovr` sidecar instead of into the file.
    pub external: bool,
    /// The overviews' compression (GDAL's `COMPRESS_OVERVIEW`): by default the full-resolution
    /// image's for internal overviews, `NONE` for external ones, as in GDAL.
    pub compression: Option<String>,
    /// Update a Cloud Optimized GeoTIFF, breaking its layout (GDAL's `IGNORE_COG_LAYOUT_BREAK`
    /// open option); refused otherwise.
    pub ignore_cog_layout_break: bool,
}

impl Default for OverviewOptions {
    fn default() -> Self {
        Self {
            levels: Vec::new(),
            min_size: 256,
            resampling: Resampling::Nearest,
            external: false,
            compression: None,
            ignore_cog_layout_break: false,
        }
    }
}

/// How [`delete_overviews`] deletes overviews (`gdaladdo -clean`, `gdal raster overview delete`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DeleteOptions {
    /// Delete the `.ovr` sidecar instead of internal overviews.
    pub external: bool,
    /// Update a Cloud Optimized GeoTIFF, breaking its layout; refused otherwise.
    pub ignore_cog_layout_break: bool,
}

/// How [`refresh_overviews`] recomputes existing overviews (`gdal raster overview refresh`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RefreshOptions {
    /// The resampling. By default, the method each overview was built with (its `RESAMPLING`
    /// metadata item), else nearest.
    pub resampling: Option<Resampling>,
    /// Refresh only the overviews of these decimation factors; empty for all.
    pub levels: Vec<usize>,
    /// Refresh only the overview blocks over these extents `[min_x, min_y, max_x, max_y]`, in
    /// the raster's CRS (`--bbox`, or the extents of `--like` datasets: see [`dataset_bounds`]);
    /// empty for whole overviews.
    pub bbox: Vec<[f64; 4]>,
    /// Refresh the `.ovr` sidecar's overviews instead of internal ones.
    pub external: bool,
    /// Update a Cloud Optimized GeoTIFF, breaking its layout; refused otherwise.
    pub ignore_cog_layout_break: bool,
}

/// GDAL's automatic overview factors (`gdaladdo` without levels): 2, 4, 8... until an overview
/// fits in `min_size` x `min_size` pixels.
pub fn automatic_levels(width: usize, height: usize, min_size: usize) -> Vec<usize> {
    let min_size = min_size.max(1);
    let mut factor = 1usize;
    let mut out = Vec::new();
    while (width.div_ceil(factor) > min_size || height.div_ceil(factor) > min_size) && factor < width.max(height) {
        factor *= 2;
        out.push(factor);
    }
    out
}

/// GDAL's decimation factor of an overview (`GDALComputeOvFactor`), preferring powers of two.
pub fn overview_factor(overview: (usize, usize), full: (usize, usize)) -> usize {
    let pick = |o: usize, n: usize| {
        let v = (0.5 + n as f64 / o.max(1) as f64) as usize;
        let p2 = if v == 0 { 0 } else { 1usize << (usize::BITS - 1 - v.leading_zeros()) };
        for fact in 1..=2 {
            if p2 > 0 && n.div_ceil(fact * p2) == o {
                return fact * p2;
            }
        }
        v
    };
    if full.0 != 1 && full.0 >= full.1 / 2 { pick(overview.0, full.0) } else { pick(overview.1, full.1) }
}

/// Whether an existing overview is the one of decimation factor `factor`, as GDAL matches them
/// (`GDALComputeOvFactor` and `GDALOvLevelAdjust2`).
fn matches_factor(overview: (usize, usize), full: (usize, usize), factor: usize) -> bool {
    let (w, h) = full;
    let adjusted = if w >= h / 2 && !(w < h && w < factor) {
        (0.5 + w as f64 / w.div_ceil(factor) as f64) as usize
    } else {
        (0.5 + h as f64 / h.div_ceil(factor) as f64) as usize
    };
    let f = overview_factor(overview, full);
    f == factor || f == adjusted
}

/// Adds (or rebuilds) overviews of a local GeoTIFF, in place or in its `.ovr` sidecar, as
/// `gdaladdo` does. Existing overviews of the sizes asked for are recomputed; the others are
/// kept. Returns the sizes of the raster's overviews afterwards, largest first.
pub fn add_overviews(path: &str, options: &OverviewOptions) -> Result<Vec<(usize, usize)>> {
    let file = gtiff_update::writable_local_file(path)?;
    let mut src = open_source(path)?;
    let method = overview_method(options.resampling)?;
    let factors = factors(&src, &options.levels, options.min_size)?;
    if options.external {
        let ovr = ovr_path(std::path::Path::new(path));
        let main = parse_file(&file)?;
        if main.dirs.iter().any(Directory::is_overview) {
            return Err(Error::InvalidArgument(format!("'{path}' has internal overviews: external ones cannot be added (as in GDAL)")));
        }
        let sizes: Vec<(usize, usize)> = factors.iter().map(|f| (src.width().div_ceil(*f), src.height().div_ceil(*f))).collect();
        let existing = if ovr.is_file() { Some(parse_file(&ovr)?) } else { None };
        if sizes.is_empty() {
            return Ok(existing.map_or_else(Vec::new, |t| t.dirs.iter().filter(|d| !d.is_mask()).map(Directory::size).collect()));
        }
        let compression = options.compression.as_deref().unwrap_or("NONE");
        let main_dir = &main.dirs[images(&main).main.unwrap_or(0)];
        let levels = build_new_levels(
            &mut src,
            main_dir,
            &sizes,
            method,
            compression,
            options.compression.is_some(),
            existing.as_ref().map(|t| t.dirs.as_slice()),
        )?;
        drop(src);
        let mut all_sizes = sizes;
        if let Some(existing) = existing {
            all_sizes.extend(existing.dirs.iter().filter(|d| d.is_overview() && !d.is_mask()).map(Directory::size));
            gtiff_update::update_file(&ovr, |f| {
                let mut a = Appender::new(f, existing.big)?;
                let mut dirs = existing.dirs;
                for img in levels {
                    let mut dir = a.image(img)?;
                    shared_tags(main_dir, &mut dir);
                    match dirs.iter().position(|old| !old.is_mask() && old.size() == dir.size()) {
                        Some(i) => dirs[i] = dir,
                        None => dirs.push(dir),
                    }
                }
                dirs.sort_by_key(|d| std::cmp::Reverse(d.size()));
                a.finish(&dirs)
            })?;
        } else {
            gtiff_update::replace_file(&ovr, |f| write_ovr(f, levels, main_dir))?;
        }
        return Ok(sorted_sizes(all_sizes));
    }
    let compression = options.compression.clone();
    let cog = options.ignore_cog_layout_break;
    gtiff_update::update_file(&file, move |f| {
        // The source is closed before the copy replaces the file.
        let out = add_internal(f, &mut src, &factors, method, compression.as_deref(), cog);
        drop(src);
        out
    })
}

/// [`add_overviews`] on a GeoTIFF held in memory (internal overviews only): returns the updated
/// file.
pub fn add_overviews_to_bytes(tiff: &[u8], options: &OverviewOptions) -> Result<Vec<u8>> {
    if options.external {
        return Err(Error::Unsupported("external overviews need a file: a GeoTIFF in memory takes internal overviews".into()));
    }
    let mut src = LazyRaster::from_tiff_bytes(tiff.to_vec())?.ok_or_else(|| Error::InvalidData("not a TIFF file".into()))?;
    let method = overview_method(options.resampling)?;
    let factors = factors(&src, &options.levels, options.min_size)?;
    let mut out = std::io::Cursor::new(tiff.to_vec());
    add_internal(&mut out, &mut src, &factors, method, options.compression.as_deref(), options.ignore_cog_layout_break)?;
    Ok(out.into_inner())
}

/// Deletes the overviews of a local GeoTIFF (`gdaladdo -clean`): its internal ones (and their
/// masks), or its `.ovr` sidecar with `external`. As in GDAL, the space internal overviews took
/// is not reclaimed.
pub fn delete_overviews(path: &str, options: &DeleteOptions) -> Result<()> {
    let file = gtiff_update::writable_local_file(path)?;
    if options.external {
        let ovr = ovr_path(std::path::Path::new(path));
        if ovr.is_file() {
            let tiff = parse_file(&ovr)?;
            if tiff.dirs.is_empty() || !tiff.dirs.iter().all(|d| d.is_overview() || d.is_mask()) {
                return Err(Error::InvalidArgument(format!("refusing to remove '{}': not an overview sidecar", ovr.display())));
            }
            std::fs::remove_file(&ovr)?;
        }
        return Ok(());
    }
    gtiff_update::update_file(&file, |f| delete_internal(f, options.ignore_cog_layout_break))
}

/// [`delete_overviews`] on a GeoTIFF held in memory: returns the updated file.
pub fn delete_overviews_from_bytes(tiff: &[u8], options: &DeleteOptions) -> Result<Vec<u8>> {
    if options.external {
        return Err(Error::Unsupported("a GeoTIFF in memory has no external overviews".into()));
    }
    let mut out = std::io::Cursor::new(tiff.to_vec());
    delete_internal(&mut out, options.ignore_cog_layout_break)?;
    Ok(out.into_inner())
}

/// Recomputes the existing overviews of a local GeoTIFF from its full-resolution image, whole or
/// over some extents only (`gdal raster overview refresh`, `gdaladdo
/// -partial_refresh_from_projwin`). Returns the sizes of the overviews refreshed.
pub fn refresh_overviews(path: &str, options: &RefreshOptions) -> Result<Vec<(usize, usize)>> {
    let file = gtiff_update::writable_local_file(path)?;
    let mut src = open_source(path)?;
    let target = if options.external { ovr_path(std::path::Path::new(path)) } else { file };
    if options.external && !target.is_file() {
        return Err(Error::InvalidArgument(format!("'{}' has no external overviews ({} is missing)", path, target.display())));
    }
    // Over an extent, the sidecar keeps its other pixels from the raster's overviews, which are
    // its internal ones when it has them.
    if options.external && !options.bbox.is_empty() && parse_file(std::path::Path::new(path))?.dirs.iter().any(Directory::is_overview) {
        return Err(Error::InvalidArgument(format!(
            "'{path}' has internal overviews: its external overviews cannot be refreshed over an extent"
        )));
    }
    gtiff_update::update_file(&target, move |f| {
        let out = refresh(f, &mut src, options);
        drop(src);
        out
    })
}

/// [`refresh_overviews`] on a GeoTIFF held in memory (internal overviews): returns the updated
/// file.
pub fn refresh_overviews_in_bytes(tiff: &[u8], options: &RefreshOptions) -> Result<Vec<u8>> {
    if options.external {
        return Err(Error::Unsupported("a GeoTIFF in memory has no external overviews".into()));
    }
    let mut src = LazyRaster::from_tiff_bytes(tiff.to_vec())?.ok_or_else(|| Error::InvalidData("not a TIFF file".into()))?;
    let mut out = std::io::Cursor::new(tiff.to_vec());
    refresh(&mut out, &mut src, options)?;
    Ok(out.into_inner())
}

/// The extent `[min_x, min_y, max_x, max_y]` of a raster, in `crs` (the extent's corners and
/// edges transformed when the raster has another CRS): a `--like` dataset's extent.
pub fn dataset_bounds(path: &str, crs: &crate::crs::SpatialRef) -> Result<[f64; 4]> {
    let r = crate::open_raster_lazy(path)?;
    if !r.has_geo_transform() {
        return Err(Error::InvalidArgument(format!("'{path}' has no geotransform: its extent is unknown")));
    }
    let gt = r.geo_transform();
    let (w, h) = (r.width() as f64, r.height() as f64);
    let corners = [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)].map(|(c, l)| super::apply_gt(&gt, c, l));
    let xs = corners.map(|c| c.0);
    let ys = corners.map(|c| c.1);
    let b = (
        xs.iter().copied().fold(f64::INFINITY, f64::min),
        ys.iter().copied().fold(f64::INFINITY, f64::min),
        xs.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        ys.iter().copied().fold(f64::NEG_INFINITY, f64::max),
    );
    let own = r.crs();
    if own.is_unknown() || crs.is_unknown() || own.same_as(crs) {
        return Ok([b.0, b.1, b.2, b.3]);
    }
    let t = super::Ct::new(&own, crs)?;
    let b = super::transform_bounds(&t, b, 21)?;
    Ok([b.0, b.1, b.2, b.3])
}

/// The extents of the `--like` datasets `likes`, in the CRS of the raster `path` (none when
/// `likes` is empty).
pub fn like_bounds(path: &str, likes: &[String]) -> Result<Vec<[f64; 4]>> {
    if likes.is_empty() {
        return Ok(Vec::new());
    }
    let crs = crate::open_raster_lazy(path)?.crs();
    likes.iter().map(|src| dataset_bounds(src, &crs)).collect()
}

/// The `.ovr` sidecar of a file.
fn ovr_path(file: &std::path::Path) -> std::path::PathBuf {
    let mut p = file.as_os_str().to_owned();
    p.push(".ovr");
    p.into()
}

fn parse_file(path: &std::path::Path) -> Result<Tiff> {
    gtiff_update::parse(&mut std::io::BufReader::new(std::fs::File::open(path)?))
}

/// Opens a GeoTIFF to compute overviews from (with its `.msk` and `.ovr` sidecars).
fn open_source(path: &str) -> Result<LazyRaster> {
    let r = LazyRaster::open(path)?;
    if r.driver() != crate::formats::RasterDriver::GTiff || !r.is_lazy() {
        return Err(Error::Unsupported(format!("overviews are managed for GeoTIFFs only, not '{path}'")));
    }
    Ok(r)
}

/// The overview method of a resampling.
fn overview_method(r: Resampling) -> Result<OverviewResampling> {
    Ok(match r {
        Resampling::Nearest => OverviewResampling::Nearest,
        Resampling::Average => OverviewResampling::Average,
        Resampling::Min => OverviewResampling::Min,
        Resampling::Max => OverviewResampling::Max,
        Resampling::Mode => OverviewResampling::Mode,
        other => {
            return Err(Error::Unsupported(format!("'{}' overviews are not supported (nearest, average, min, max, mode)", other.name())));
        }
    })
}

fn factors(src: &LazyRaster, levels: &[usize], min_size: usize) -> Result<Vec<usize>> {
    if let Some(f) = levels.iter().find(|f| **f < 2) {
        return Err(Error::InvalidArgument(format!("an overview level is a decimation factor of 2 or more, not {f}")));
    }
    let mut f = if levels.is_empty() { automatic_levels(src.width(), src.height(), min_size) } else { levels.to_vec() };
    f.sort_unstable();
    f.dedup();
    Ok(f)
}

fn sorted_sizes(mut sizes: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    sizes.sort_by_key(|s| std::cmp::Reverse(*s));
    sizes.dedup();
    sizes
}

/// What an image of the file is, for the chain of directories.
struct Images {
    /// The images that are not overviews or their masks, in order (the full-resolution image,
    /// its mask), by index in the file.
    base: Vec<usize>,
    /// The index of the full-resolution image, when the file has it (not an `.ovr`).
    main: Option<usize>,
    /// Whether the full-resolution image has an internal mask.
    main_mask: bool,
    /// The overviews: size, index, and the index of its mask.
    overviews: Vec<((usize, usize), usize, Option<usize>)>,
}

fn images(tiff: &Tiff) -> Images {
    let mut base = Vec::new();
    let mut main = None;
    let mut masks = Vec::new();
    let mut overviews = Vec::new();
    for (i, d) in tiff.dirs.iter().enumerate() {
        if d.is_overview() {
            overviews.push((d.size(), i, None));
        } else if d.is_mask() && d.subfile() & gtiff_update::REDUCED != 0 {
            masks.push((d.size(), i));
        } else {
            if main.is_none() && !d.is_mask() {
                main = Some(i);
            }
            base.push(i);
        }
    }
    let main_size = main.map(|m| tiff.dirs[m].size());
    let main_mask = base.iter().any(|&i| tiff.dirs[i].is_mask() && Some(tiff.dirs[i].size()) == main_size);
    for o in &mut overviews {
        o.2 = masks.iter().find(|(s, _)| *s == o.0).map(|(_, i)| *i);
    }
    Images { base, main, main_mask, overviews }
}

/// The layout, compression and codec parameters of new images like `dir` (the full-resolution
/// image or an existing overview), in another compression when one is given.
fn layout_like(
    dir: &Directory,
    compression: Option<&str>,
    new_image: bool,
    data_type: DataType,
) -> Result<(Layout, Compression, CodecParams)> {
    if matches!(dir.uint(339), Some(5 | 6)) {
        return Err(Error::Unsupported("overviews of complex GeoTIFF samples are not supported".into()));
    }
    let bits = dir.uint(258).unwrap_or(1) as usize;
    if bits != data_type.size_bytes() * 8 {
        return Err(Error::Unsupported(format!("overviews of {bits}-bit samples are not supported")));
    }
    let own = Compression::from_tiff(dir.uint(259).unwrap_or(1) as u16, dir.get(50674).and_then(|e| e.uints().get(1).copied()));
    let compression = match compression {
        Some(name) => {
            Compression::parse(name).ok_or_else(|| Error::Unsupported(format!("'{name}' overview compression is not supported")))?
        }
        None => own.unwrap_or_else(|| {
            crate::warnings::warn("the image's compression is not one terrio encodes: overviews are compressed with DEFLATE".into());
            Compression::Deflate
        }),
    };
    let samples = dir.uint(277).unwrap_or(1) as usize;
    let mut photometric = dir.uint(262).unwrap_or(1) as u16;
    if photometric == 6 && compression != Compression::Jpeg {
        photometric = 2;
    }
    let tile = if dir.tiled() {
        Some((dir.uint(322).unwrap_or(256) as u32, dir.uint(323).unwrap_or(256) as u32))
    } else if new_image {
        // GDAL tiles the overviews of striped images (GDAL_TIFF_OVR_BLOCKSIZE).
        Some((128, 128))
    } else {
        None
    };
    let predictor = if compression.takes_predictor() { dir.uint(317).unwrap_or(1) as u16 } else { 1 };
    let layout = Layout {
        tile,
        rows_per_strip: if tile.is_none() { dir.uint(278).map(|r| r as u32) } else { None },
        planar: dir.uint(284) == Some(2) && samples > 1,
        photometric,
        extra: dir.get(338).map(|e| e.uints().into_iter().map(|v| v as u16).collect()).unwrap_or_default(),
        predictor,
        parts: 1,
    };
    let codec = CodecParams { jpeg_ycbcr: photometric == 6, ..Default::default() };
    Ok((layout, compression, codec))
}

/// The chunk width and height of a layout for an image of `cols` columns (strips: the whole
/// width, and the strip rows the writer uses).
fn chunk_size(layout: &Layout, cols: usize) -> (usize, usize) {
    match layout.tile {
        Some((w, h)) => (w as usize, h as usize),
        None => (cols, layout.rows_per_strip.map_or(usize::MAX, |r| r.max(1) as usize)),
    }
}

/// One overview level being computed: its pixels over rows `rows` (all columns), and its mask
/// (255 valid) over the same rows when the raster has a per-dataset mask.
pub(crate) struct Computed {
    pub(crate) size: (usize, usize),
    pub(crate) rows: std::ops::Range<usize>,
    pub(crate) data: RasterData,
    pub(crate) mask: Option<Vec<u8>>,
}

/// How GDAL schedules the computation of a level.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Schedule {
    /// From the previous level computed (when it is larger), rather than from full resolution.
    from_prev: bool,
    /// The source rows of `GDALRegenerateOverviewsEx`'s chunks, which clamp the windows; `None`
    /// for `GDALRegenerateOverviewsMultiBand`'s whole windows.
    chunk_rows: Option<usize>,
}

/// The pixels overviews are computed from: the raster (its full-resolution image read once when
/// a partial refresh spans its height) and its stored overviews.
type SharedPixels = (std::sync::Arc<RasterData>, Option<std::sync::Arc<[u8]>>);

struct Sources<'a> {
    src: &'a mut LazyRaster,
    base: Option<SharedPixels>,
    /// The alpha band (from 0), whose zeros GDAL leaves out of the other methods than nearest.
    alpha: Option<usize>,
    /// About how many bytes of values each full-resolution read of whole levels takes (64 MB,
    /// as [`LazyRaster::write`] reads them): fewer, larger reads, which decode on all threads.
    read_bytes: usize,
    /// About how many values per band the strips of whole-window levels are computed from
    /// ([`STRIP_VALUES`]).
    strip_values: usize,
}

impl Sources<'_> {
    /// Rows `rows` (all columns) of the full-resolution image (`level` `None`) or of a stored
    /// overview, with the level's mask (255 valid) when `want_mask`, and the rows of `computed`
    /// (the level's new pixels) over them.
    fn rows(
        &mut self,
        level: Option<usize>,
        size: (usize, usize),
        rows: std::ops::Range<usize>,
        computed: Option<&Computed>,
        want_mask: bool,
    ) -> Result<SharedPixels> {
        let (w, n) = (size.0, rows.len());
        let bands = self.src.band_count();
        if let Some(c) = computed.filter(|c| c.rows.start <= rows.start && c.rows.end >= rows.end) {
            let data = slice_rows(&c.data, c.rows.len(), w, bands, rows.start - c.rows.start..rows.end - c.rows.start);
            let mask = c.mask.as_ref().map(|m| m[(rows.start - c.rows.start) * w..(rows.end - c.rows.start) * w].to_vec());
            return Ok((data.into(), mask.map(Into::into)));
        }
        if level.is_none() && rows == (0..size.1) {
            if self.base.is_none() {
                let data = self.src.read()?.raster.data;
                let mask = self.src.read_mask(Window::new(0, 0, size.0, size.1), None)?;
                self.base = Some((data.into(), mask.map(Into::into)));
            }
            let (data, mask) = self.base.as_ref().unwrap();
            return Ok((data.clone(), mask.clone().filter(|_| want_mask)));
        }
        let window = Window::new(0, rows.start, w, n);
        let mut data = self.src.read_window(window, &WindowOptions { bands: Vec::new(), overview: level })?.raster.data;
        let mut mask = if want_mask { self.src.read_mask(window, level)? } else { None };
        if let Some(c) = computed {
            let (a, b) = (rows.start.max(c.rows.start), rows.end.min(c.rows.end));
            if a < b {
                overlay_rows(&mut data, n, rows.start, &c.data, c.rows.len(), c.rows.start, a..b, w, bands);
                if let (Some(m), Some(cm)) = (mask.as_mut(), c.mask.as_ref()) {
                    m[(a - rows.start) * w..(b - rows.start) * w].copy_from_slice(&cm[(a - c.rows.start) * w..(b - c.rows.start) * w]);
                }
            }
        }
        Ok((data.into(), mask.map(Into::into)))
    }
}

/// Rows `range` of a band-major image of `rows` rows of `cols` columns.
fn slice_rows(data: &RasterData, rows: usize, cols: usize, bands: usize, range: std::ops::Range<usize>) -> RasterData {
    crate::pixel::map_data!(data, v => {
        let mut out = Vec::with_capacity(range.len() * cols * bands);
        for b in 0..bands {
            out.extend_from_slice(&v[(b * rows + range.start) * cols..(b * rows + range.end) * cols]);
        }
        out
    })
}

/// Columns `range` of a band-major image of `rows` rows of `cols` columns.
fn slice_cols(data: &RasterData, rows: usize, cols: usize, bands: usize, range: std::ops::Range<usize>) -> RasterData {
    crate::pixel::map_data!(data, v => {
        let mut out = Vec::with_capacity(range.len() * rows * bands);
        for b in 0..bands {
            for r in 0..rows {
                let i = (b * rows + r) * cols;
                out.extend_from_slice(&v[i + range.start..i + range.end]);
            }
        }
        out
    })
}

/// Copies rows `range` (image rows) from `src` (rows from `src_start`) into `dst` (rows from
/// `dst_start`), band-major images of the same type.
#[allow(clippy::too_many_arguments)]
fn overlay_rows(
    dst: &mut RasterData,
    dst_rows: usize,
    dst_start: usize,
    src: &RasterData,
    src_rows: usize,
    src_start: usize,
    range: std::ops::Range<usize>,
    cols: usize,
    bands: usize,
) {
    macro_rules! copy {
        ($($t:ident),*) => {
            match (dst, src) {
                $((RasterData::$t(d), RasterData::$t(s)) => {
                    for b in 0..bands {
                        let di = (b * dst_rows + range.start - dst_start) * cols;
                        let si = (b * src_rows + range.start - src_start) * cols;
                        let n = range.len() * cols;
                        d[di..di + n].copy_from_slice(&s[si..si + n]);
                    }
                })*
                _ => {}
            }
        };
    }
    copy!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64);
}

/// The source rows the overview rows `rows` of a level of `out_rows` rows are computed from, in
/// an image of `src_rows` rows (with a row of margin for the windows' rounding).
fn source_rows(rows: &std::ops::Range<usize>, out_rows: usize, src_rows: usize) -> std::ops::Range<usize> {
    let ratio = src_rows as f64 / out_rows as f64;
    let start = ((rows.start as f64 * ratio) as usize).saturating_sub(1);
    let end = (((rows.end as f64 * ratio).ceil() as usize) + 1).min(src_rows);
    start.min(end.saturating_sub(1))..end
}

/// Resamples one band (`values`, rows `src_rows` of a level of `src_size`) to rows `rows` of an
/// overview, as GDAL schedules it. GDAL's chunks for a whole level are used only by
/// [`compute`]'s whole-level reference, which the tests compare [`whole_levels`] with.
#[allow(clippy::too_many_arguments)]
fn resample_band(
    values: &[f64],
    valid: Option<&[bool]>,
    src_size: (usize, usize),
    src_rows: std::ops::Range<usize>,
    t: &kernel::Target,
    rows: std::ops::Range<usize>,
    chunk_rows: Option<usize>,
    group_origin: usize,
) -> Vec<f64> {
    let band = kernel::Band { values, valid, width: src_size.0, height: src_size.1, rows: src_rows.clone() };
    match chunk_rows {
        Some(chunk) if rows == (0..t.height) && src_rows == (0..src_size.1) => {
            let mut out = Vec::with_capacity(t.width * t.height);
            for (s, o) in kernel::ex_chunks(src_size.1, t.height, chunk) {
                out.extend(kernel::resample(&band, t, o, s, 0));
            }
            out
        }
        _ => kernel::resample(&band, t, rows, src_rows, group_origin),
    }
}

/// The values of band `b` of a band-major image of `n` pixels per band, as `f64`.
fn band_values(data: &RasterData, b: usize, n: usize) -> Vec<f64> {
    crate::pixel::with_data!(data, v => v[b * n..(b + 1) * n].iter().map(|x| crate::pixel::Pixel::to_f64(*x)).collect())
}

/// A band-major image of `data_type` from values of that type held as `f64`.
fn from_values(data_type: DataType, values: Vec<f64>) -> RasterData {
    match data_type {
        DataType::U8 => RasterData::U8(values.into_iter().map(|v| v as u8).collect()),
        DataType::I8 => RasterData::I8(values.into_iter().map(|v| v as i8).collect()),
        DataType::U16 => RasterData::U16(values.into_iter().map(|v| v as u16).collect()),
        DataType::I16 => RasterData::I16(values.into_iter().map(|v| v as i16).collect()),
        DataType::U32 => RasterData::U32(values.into_iter().map(|v| v as u32).collect()),
        DataType::I32 => RasterData::I32(values.into_iter().map(|v| v as i32).collect()),
        DataType::U64 => RasterData::U64(values.into_iter().map(|v| v as u64).collect()),
        DataType::I64 => RasterData::I64(values.into_iter().map(|v| v as i64).collect()),
        DataType::F32 => RasterData::F32(values.into_iter().map(|v| v as f32).collect()),
        DataType::F64 => RasterData::F64(values),
    }
}

/// Whether a level of width `width` scheduled with `sched` is computed from the previous level
/// computed (of width `prev_width`), rather than from full resolution.
fn cascades(sched: Schedule, prev_width: usize, width: usize) -> bool {
    sched.from_prev && prev_width > width
}

/// What a level is computed from: the previous level (computed, or stored at `level`) or full
/// resolution.
fn source_of<'c>(
    sched: Schedule,
    prev: Option<&'c Computed>,
    size: (usize, usize),
    full: (usize, usize),
    overviews: &[(usize, usize)],
) -> ((usize, usize), Option<usize>, Option<&'c Computed>) {
    match prev.filter(|p| cascades(sched, p.size.0, size.0)) {
        Some(p) => (p.size, overviews.iter().position(|s| *s == p.size), Some(p)),
        None => (full, None, None),
    }
}

/// Whether GDAL computes the overviews of a TIFF image like `dir` with
/// `GDALRegenerateOverviewsMultiBand` (block by block, every level from the previous one)
/// rather than band by band with `GDALRegenerateOverviewsEx`: for internal overviews of
/// pixel-interleaved (or single-band, or alpha) images, and for compressed pixel-interleaved
/// external ones.
fn gdal_multiband(dir: &Directory, external: bool, compressed: bool, bands: usize, alpha: bool) -> bool {
    let contig = dir.uint(284) != Some(2) || bands == 1;
    if external { (compressed && contig) || alpha } else { contig || alpha }
}

/// The band GDAL takes as an alpha band (its color interpretation, which `GDAL_METADATA` may
/// override), numbered from 0.
fn alpha_band(src: &mut LazyRaster) -> Option<usize> {
    let header = src.header();
    let explicit = header.band_metadata.iter().position(|m| m.color_interp.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("alpha")));
    if explicit.is_some() {
        return explicit;
    }
    let last = src.alpha_band()?.checked_sub(1)?;
    header.band_metadata.get(last).is_none_or(|m| m.color_interp.is_none()).then_some(last)
}

/// The schedules GDAL computes the levels of `sizes` (largest first, with their blocks) with:
/// for the data, and for the mask when the raster has one.
#[allow(clippy::too_many_arguments)]
fn schedules(
    full: (usize, usize),
    base_block: (usize, usize),
    sizes: &[(usize, usize)],
    blocks: &[(usize, usize)],
    method: OverviewResampling,
    data_type: DataType,
    multiband: bool,
    has_mask: bool,
) -> (Vec<Schedule>, Vec<Option<Schedule>>) {
    let factor = |src: (usize, usize), dst: (usize, usize)| {
        ((src.0 as f64 / dst.0 as f64 + 0.5) as usize).max((src.1 as f64 / dst.1 as f64 + 0.5) as usize).max(1)
    };
    // GDALRegenerateOverviewsEx on a band whose values are `value_size` bytes; `cascadable` for
    // averages and modes whose mask is the nodata value (or none).
    let ex = |value_size: usize, cascadable: bool| -> Vec<Schedule> {
        let cascade = method != OverviewResampling::Nearest && sizes.len() > 1 && cascadable;
        if cascade {
            return (0..sizes.len())
                .map(|i| {
                    let (src, block) = if i == 0 { (full, base_block) } else { (sizes[i - 1], blocks[i - 1]) };
                    let rows = kernel::ex_chunk_rows(src.0, src.1, block, factor(src, sizes[i]), value_size);
                    Schedule { from_prev: true, chunk_rows: rows }
                })
                .collect();
        }
        let max = sizes.iter().map(|s| factor(full, *s)).max().unwrap_or(1);
        match kernel::ex_chunk_rows(full.0, full.1, base_block, max, value_size) {
            Some(rows) => vec![Schedule { from_prev: false, chunk_rows: Some(rows) }; sizes.len()],
            None => vec![Schedule { from_prev: true, chunk_rows: None }; sizes.len()],
        }
    };
    let data = if multiband {
        vec![Schedule { from_prev: true, chunk_rows: None }; sizes.len()]
    } else {
        ex(kernel::work_size(method, data_type), !has_mask)
    };
    let mask = if has_mask { ex(1, true).into_iter().map(Some).collect() } else { vec![None; sizes.len()] };
    (data, mask)
}

/// Computes overview rows `rows` of a level of `size`, as GDAL does: the data with `data` and,
/// for a raster with a per-dataset mask, the mask's overview with `mask` (GDAL computes masks band
/// by band, then the data from them). `group_origin` is the first column of the region GDAL
/// computes (for the grouping of its factor-of-two averages). Partial refreshes compute their
/// rows with it; whole levels are computed by [`whole_levels`], which reads the image by strips
/// (this computes them from the image read whole, the reference the tests compare them with).
#[allow(clippy::too_many_arguments)]
fn compute(
    srcs: &mut Sources,
    size: (usize, usize),
    rows: std::ops::Range<usize>,
    prev: Option<&Computed>,
    method: OverviewResampling,
    data: Schedule,
    mask: Option<Schedule>,
    group_origin: usize,
) -> Result<Computed> {
    let full = (srcs.src.width(), srcs.src.height());
    let overviews = srcs.src.overviews();
    let (bands, data_type, nodata) = (srcs.src.band_count(), srcs.src.data_type(), srcs.src.nodata());
    // The mask's overview.
    let new_mask = match mask {
        Some(ms) => {
            let (msize, level, computed) = source_of(ms, prev.filter(|p| p.mask.is_some()), size, full, &overviews);
            let mrows = if rows == (0..size.1) && ms.chunk_rows.is_some() { 0..msize.1 } else { source_rows(&rows, size.1, msize.1) };
            let (_, m) = srcs.rows(level, msize, mrows.clone(), computed, true)?;
            let m = m.ok_or_else(|| Error::InvalidData("the raster's mask could not be read".into()))?;
            let values: Vec<f64> = m.iter().map(|v| *v as f64).collect();
            let valid: Option<Vec<bool>> = (method != OverviewResampling::Nearest).then(|| m.iter().map(|v| *v != 0).collect());
            let t = kernel::Target { width: size.0, height: size.1, data_type: DataType::U8, nodata: None, method };
            let out = resample_band(&values, valid.as_deref(), msize, mrows, &t, rows.clone(), ms.chunk_rows, group_origin);
            Some(out.into_iter().map(|v| v as u8).collect::<Vec<u8>>())
        }
        None => None,
    };
    let (dsize, level, computed) = source_of(data, prev, size, full, &overviews);
    let drows = if rows == (0..size.1) && data.chunk_rows.is_some() { 0..dsize.1 } else { source_rows(&rows, size.1, dsize.1) };
    let masked = method != OverviewResampling::Nearest;
    let want_mask = masked && (mask.is_some() || (srcs.src.has_mask() && computed.is_none()));
    let (src_data, src_mask) = srcs.rows(level, dsize, drows.clone(), computed, want_mask)?;
    let n = dsize.0 * drows.len();
    let t = kernel::Target { width: size.0, height: size.1, data_type, nodata, method };
    let alpha: Option<Vec<bool>> =
        srcs.alpha.filter(|_| masked && src_mask.is_none()).map(|a| band_values(&src_data, a, n).iter().map(|v| *v != 0.0).collect());
    let mut out = Vec::with_capacity(size.0 * rows.len() * bands);
    for b in 0..bands {
        let values = band_values(&src_data, b, n);
        // GDAL's mask band: the per-dataset mask, else the alpha band, else the nodata value.
        let valid: Option<Vec<bool>> = match (&src_mask, &alpha, nodata) {
            _ if !masked => None,
            (Some(m), ..) => Some(m.iter().map(|v| *v != 0).collect()),
            (None, Some(a), _) => Some(a.clone()),
            (None, None, Some(nd)) => Some(values.iter().map(|v| !(*v == nd || (v.is_nan() && nd.is_nan()))).collect()),
            (None, None, None) => None,
        };
        out.extend(resample_band(&values, valid.as_deref(), dsize, drows.clone(), &t, rows.clone(), data.chunk_rows, group_origin));
    }
    Ok(Computed { size, rows, data: from_values(data_type, out), mask: new_mask })
}

/// The bytes of values each full-resolution read of whole levels takes, about.
const READ_BYTES: usize = 64 << 20;

/// The values per band of the full-resolution rows a whole-window level (GDAL's
/// `GDALRegenerateOverviewsMultiBand`) is computed from at a time, before rounding to whole
/// blocks.
const STRIP_VALUES: usize = 1 << 20;

/// A level, and how GDAL schedules its data and its mask's overview, computed whole.
#[derive(Debug, Clone, Copy)]
struct WholeLevel {
    size: (usize, usize),
    method: OverviewResampling,
    data: Schedule,
    mask: Option<Schedule>,
}

/// The data, or the mask's overview, of a level computed whole from one source: the full
/// resolution or the previous level.
#[derive(Debug, Clone, Copy)]
struct Job {
    /// The level's index.
    level: usize,
    /// The mask's overview, rather than the data.
    mask: bool,
}

/// The parts of the levels computed so far: their data and their mask's overview.
type Parts = Vec<(Option<RasterData>, Option<Vec<u8>>)>;

/// Computes whole levels (largest first) as GDAL does, and hands each to `each` as it is
/// computed. The full-resolution image is read once, forward, by chunks of rows: every level
/// computed from full resolution is computed in that pass, chunk by chunk, so that the image is
/// never held whole. The other levels are computed from the previous level, held in memory.
fn whole_levels(
    srcs: &mut Sources,
    levels: &[WholeLevel],
    mut each: impl FnMut(&mut Sources, usize, &Computed) -> Result<()>,
) -> Result<()> {
    let full = (srcs.src.width(), srcs.src.height());
    // Whether a level's part is computed from the previous level, as `source_of` chooses (for the
    // mask, when the previous level has one).
    let from_prev = |k: usize, mask: bool| {
        let l = &levels[k];
        let sched = if mask { l.mask } else { Some(l.data) };
        k > 0 && sched.is_some_and(|s| cascades(s, levels[k - 1].size.0, l.size.0)) && (!mask || levels[k - 1].mask.is_some())
    };
    let parts_of = |k: usize| [Some(false), levels[k].mask.map(|_| true)].into_iter().flatten().map(move |mask| Job { level: k, mask });
    let first: Vec<Job> = (0..levels.len()).flat_map(parts_of).filter(|j| !from_prev(j.level, j.mask)).collect();
    let mut parts: Parts = (0..levels.len()).map(|_| (None, None)).collect();
    run_jobs(srcs, None, full, levels, &first, &mut parts)?;
    let mut prev: Option<Computed> = None;
    for (k, level) in levels.iter().enumerate() {
        let jobs: Vec<Job> = parts_of(k).filter(|j| from_prev(j.level, j.mask)).collect();
        if let Some(p) = prev.as_ref().filter(|_| !jobs.is_empty()) {
            run_jobs(srcs, Some(p), p.size, levels, &jobs, &mut parts)?;
        }
        let (data, mask) = std::mem::take(&mut parts[k]);
        let data = data.ok_or_else(|| Error::InvalidData("an overview level was not computed".into()))?;
        let c = Computed { size: level.size, rows: 0..level.size.1, data, mask };
        each(srcs, k, &c)?;
        prev = Some(c);
    }
    Ok(())
}

/// The source rows jobs read: rows `start..start + rows` (band-major) of the full-resolution
/// image, read forward, and of its mask; or the whole previous level.
struct Held<'c> {
    start: usize,
    rows: usize,
    data: Option<std::borrow::Cow<'c, RasterData>>,
    mask: Option<std::borrow::Cow<'c, [u8]>>,
    /// Which full-resolution sources [`Held::fill`] reads: the image, the mask.
    read_data: bool,
    read_mask: bool,
}

impl Held<'_> {
    /// Reads full-resolution rows up to `end`, keeping the rows from `keep` held: whole blocks of
    /// `block` rows, and about [`Sources::read_bytes`] of values at a time.
    fn fill(&mut self, srcs: &mut Sources, keep: usize, end: usize, block: usize) -> Result<()> {
        let held = self.start + self.rows;
        if end <= held {
            return Ok(());
        }
        let (data, mask) = (self.read_data, self.read_mask);
        let (w, h, bands) = (srcs.src.width(), srcs.src.height(), srcs.src.band_count());
        let from = keep.max(self.start).min(held);
        let read_from = held.max(keep);
        let row = w * if data { bands * srcs.src.data_type().size_bytes() } else { 1 };
        let ahead = (srcs.read_bytes / row.max(1) / block).max(1) * block;
        let end = end.max(read_from + ahead).div_ceil(block).saturating_mul(block).min(h);
        let window = Window::new(0, read_from, w, end - read_from);
        let new_data = if data { Some(srcs.src.read_window(window, &WindowOptions::default())?.raster.data) } else { None };
        let new_mask = if mask { srcs.src.read_mask(window, None)? } else { None };
        let (kept, rows) = (from - self.start..held - self.start, self.rows);
        self.data = match (self.data.take(), new_data) {
            (Some(old), Some(new)) if !kept.is_empty() => {
                Some(std::borrow::Cow::Owned(join_rows(&old, rows, kept.clone(), &new, end - read_from, w, bands)))
            }
            (_, new) => new.map(std::borrow::Cow::Owned),
        };
        self.mask = match (self.mask.take(), new_mask) {
            (Some(old), Some(new)) if !kept.is_empty() => Some([&old[kept.start * w..kept.end * w], &new[..]].concat().into()),
            (_, new) => new.map(Into::into),
        };
        self.start = if kept.is_empty() { read_from } else { from };
        self.rows = end - self.start;
        Ok(())
    }

    /// The values of band `b` over rows `rows`, as `f64`.
    fn band(&self, b: usize, rows: &std::ops::Range<usize>, w: usize) -> Vec<f64> {
        let data = self.data.as_deref().expect("data rows are held");
        let i = (b * self.rows + rows.start - self.start) * w;
        crate::pixel::with_data!(data, v => v[i..i + rows.len() * w].iter().map(|x| crate::pixel::Pixel::to_f64(*x)).collect())
    }

    /// The mask over rows `rows`.
    fn mask(&self, rows: &std::ops::Range<usize>, w: usize) -> Option<&[u8]> {
        self.mask.as_deref().map(|m| &m[(rows.start - self.start) * w..(rows.end - self.start) * w])
    }
}

/// Rows `a_range` of `a` (band-major, `a_rows` rows) followed by the rows of `b` (`b_rows` rows),
/// images of `cols` columns of the same type.
fn join_rows(
    a: &RasterData,
    a_rows: usize,
    a_range: std::ops::Range<usize>,
    b: &RasterData,
    b_rows: usize,
    cols: usize,
    bands: usize,
) -> RasterData {
    macro_rules! join {
        ($($t:ident),*) => {
            match (a, b) {
                $((RasterData::$t(a), RasterData::$t(b)) => {
                    let mut out = Vec::with_capacity((a_range.len() + b_rows) * cols * bands);
                    for band in 0..bands {
                        out.extend_from_slice(&a[(band * a_rows + a_range.start) * cols..(band * a_rows + a_range.end) * cols]);
                        out.extend_from_slice(&b[band * b_rows * cols..(band + 1) * b_rows * cols]);
                    }
                    RasterData::$t(out)
                })*
                _ => unreachable!("the rows of one image have one type"),
            }
        };
    }
    join!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64)
}

/// Stores values of `data`'s type held as `f64` from `offset`, converted as [`from_values`]
/// converts them.
fn put_values(data: &mut RasterData, offset: usize, values: &[f64]) {
    macro_rules! put {
        ($($t:ident => $ty:ty),*) => {
            match data {
                $(RasterData::$t(d) => {
                    for (o, v) in d[offset..offset + values.len()].iter_mut().zip(values) {
                        *o = *v as $ty;
                    }
                })*
            }
        };
    }
    put!(U8 => u8, I8 => i8, U16 => u16, I16 => i16, U32 => u32, I32 => i32, U64 => u64, I64 => i64, F32 => f32, F64 => f64);
}

/// The steps of a job from a level of `src` rows: `(source rows, overview rows)`, GDAL's chunks of
/// `GDALRegenerateOverviewsEx`, or, for the unclamped windows, strips of overview rows whose
/// source rows are about `values` values per band (in whole blocks of `block` rows). Each strip's
/// source rows have a row of margin (as [`source_rows`] gives them), so every window lies in them.
fn steps(
    sched: Schedule,
    out_rows: usize,
    src: (usize, usize),
    block: usize,
    values: usize,
) -> Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    match sched.chunk_rows {
        Some(chunk) => kernel::ex_chunks(src.1, out_rows, chunk),
        None => {
            let rows = (values / src.0.max(1)).max(1).div_ceil(block) * block;
            if rows >= src.1 {
                return vec![(0..src.1, 0..out_rows)];
            }
            // Strips of overview rows of about `rows` source rows.
            let strip = ((rows.saturating_sub(2) as u64 * out_rows as u64 / src.1 as u64) as usize).max(1);
            (0..out_rows)
                .step_by(strip)
                .map(|o| {
                    let o = o..(o + strip).min(out_rows);
                    (source_rows(&o, out_rows, src.1), o)
                })
                .collect()
        }
    }
}

/// Computes `jobs` from `prev` (the previous level, of `src_size`), or from the full-resolution
/// image when `prev` is `None`: their steps run in the order of their last source rows, so the
/// image is read forward once and only the rows the current steps need are held.
fn run_jobs(
    srcs: &mut Sources,
    prev: Option<&Computed>,
    src_size: (usize, usize),
    levels: &[WholeLevel],
    jobs: &[Job],
    parts: &mut Parts,
) -> Result<()> {
    if jobs.is_empty() {
        return Ok(());
    }
    let (bands, data_type, nodata, has_mask) = (srcs.src.band_count(), srcs.src.data_type(), srcs.src.nodata(), srcs.src.has_mask());
    // GDAL's mask band for the data of a method other than nearest: the per-dataset mask (the
    // mask's overview for a previous level), else the alpha band, else the nodata value.
    let use_mask = |l: &WholeLevel| l.method != OverviewResampling::Nearest && (prev.is_some() || l.mask.is_some() || has_mask);
    let block = srcs.src.block_size().map_or(1, |b| b.1.max(1));
    let mut held = match prev {
        Some(p) => Held {
            start: 0,
            rows: p.size.1,
            data: Some(std::borrow::Cow::Borrowed(&p.data)),
            mask: p.mask.as_deref().map(Into::into),
            read_data: false,
            read_mask: false,
        },
        None => Held {
            start: 0,
            rows: 0,
            data: None,
            mask: None,
            read_data: jobs.iter().any(|j| !j.mask),
            read_mask: jobs.iter().any(|j| j.mask || use_mask(&levels[j.level])),
        },
    };
    let job_steps: Vec<_> = jobs
        .iter()
        .map(|j| {
            let l = &levels[j.level];
            let sched = if j.mask { l.mask.unwrap_or(l.data) } else { l.data };
            steps(sched, l.size.1, src_size, if prev.is_some() { 1 } else { block }, srcs.strip_values)
        })
        .collect();
    for (j, s) in jobs.iter().zip(&job_steps) {
        let size = levels[j.level].size;
        let n = size.0 * size.1;
        debug_assert_eq!(s.iter().map(|(_, o)| o.len()).sum::<usize>(), size.1);
        if j.mask {
            parts[j.level].1 = Some(vec![0; n]);
        } else {
            parts[j.level].0 = Some(RasterData::new_filled(data_type, n * bands, 0.0));
        }
    }
    let w = src_size.0;
    let mut at = vec![0usize; jobs.len()];
    let pending = |at: &[usize], j: usize| job_steps[j].get(at[j]);
    while let Some(j) = (0..jobs.len()).filter(|&j| pending(&at, j).is_some()).min_by_key(|&j| pending(&at, j).map(|s| s.0.end)) {
        let keep = (0..jobs.len()).filter_map(|j| pending(&at, j)).map(|s| s.0.start).min().unwrap_or(0);
        let (rows, out_rows) = job_steps[j][at[j]].clone();
        at[j] += 1;
        // The previous level is held whole.
        held.fill(srcs, keep, rows.end, block)?;
        let (job, l) = (jobs[j], &levels[jobs[j].level]);
        let band = |values: &[f64], valid: Option<&[bool]>, t: &kernel::Target| {
            let band = kernel::Band { values, valid, width: w, height: src_size.1, rows: rows.clone() };
            kernel::resample(&band, t, out_rows.clone(), rows.clone(), 0)
        };
        let masked = l.method != OverviewResampling::Nearest;
        if job.mask {
            let m = held.mask(&rows, w).ok_or_else(|| Error::InvalidData("the raster's mask could not be read".into()))?;
            let values: Vec<f64> = m.iter().map(|v| *v as f64).collect();
            let valid: Option<Vec<bool>> = masked.then(|| m.iter().map(|v| *v != 0).collect());
            let t = kernel::Target { width: l.size.0, height: l.size.1, data_type: DataType::U8, nodata: None, method: l.method };
            let out = band(&values, valid.as_deref(), &t);
            let dst = parts[job.level].1.as_mut().expect("the mask is allocated");
            for (d, v) in dst[out_rows.start * l.size.0..].iter_mut().zip(out) {
                *d = v as u8;
            }
            continue;
        }
        let src_mask = held.mask(&rows, w).filter(|_| use_mask(l));
        let t = kernel::Target { width: l.size.0, height: l.size.1, data_type, nodata, method: l.method };
        // The mask's or the alpha band's validity, shared by the bands; else each band's nodata.
        let shared: Option<Vec<bool>> = match src_mask {
            _ if !masked => None,
            Some(m) => Some(m.iter().map(|v| *v != 0).collect()),
            None => srcs.alpha.map(|a| held.band(a, &rows, w).iter().map(|v| *v != 0.0).collect()),
        };
        for b in 0..bands {
            let values = held.band(b, &rows, w);
            let own: Option<Vec<bool>> = match (&shared, nodata) {
                (None, Some(nd)) if masked => Some(values.iter().map(|v| !(*v == nd || (v.is_nan() && nd.is_nan()))).collect()),
                _ => None,
            };
            let out = band(&values, shared.as_deref().or(own.as_deref()), &t);
            let dst = parts[job.level].0.as_mut().expect("the data is allocated");
            put_values(dst, (b * l.size.1 + out_rows.start) * l.size.0, &out);
        }
    }
    Ok(())
}

/// The `GDAL_METADATA` of an overview: the method it was computed with, as GDAL records it.
fn resampling_item(method: OverviewResampling) -> String {
    format!("<GDALMetadata><Item name=\"RESAMPLING\" sample=\"0\">{}</Item></GDALMetadata>", method.gdal_name())
}

/// The method an existing overview was computed with, from its `RESAMPLING` item.
fn stored_method(dir: &Directory) -> Option<Result<OverviewResampling>> {
    let xml = dir.get(42112)?.ascii()?;
    let item = gw::gdal_metadata_items(&xml).into_iter().find(|i| i.attr("name") == Some("RESAMPLING"))?;
    let name = item.text.trim().to_ascii_lowercase();
    Some(OverviewResampling::parse(&name).map_err(|_| {
        Error::Unsupported(format!("the overviews were built with '{name}' resampling, which terrio does not compute: pick another one"))
    }))
}

/// Encodes a computed level as a new overview image like `like` (the full-resolution image).
fn new_level_image(
    c: &Computed,
    like: &Directory,
    compression: Option<&str>,
    method: OverviewResampling,
    data_type: DataType,
    bands: usize,
) -> Result<(gw::Image, Option<gw::Image>)> {
    let (layout, compression, codec) = layout_like(like, compression, true, data_type)?;
    let level = gw::Level { cols: c.size.0, rows: c.size.1, bands, data: std::borrow::Cow::Borrowed(&c.data) };
    let mut img = level.image(data_type, compression, &codec, &layout)?;
    img.tags.push((254, Value::Longs(vec![1])));
    img.tags.push((42112, Value::Ascii(resampling_item(method))));
    let mask = match &c.mask {
        Some(m) => {
            let (cw, ch) = chunk_size(&layout, c.size.0);
            let mut mi = gw::mask_image(m, c.size.0, c.size.1, (cw, ch.min(c.size.1), layout.tile.is_some()))?;
            for (t, v) in &mut mi.tags {
                if *t == 254 {
                    *v = Value::Longs(vec![5]);
                }
            }
            Some(mi)
        }
        None => None,
    };
    Ok((img, mask))
}

/// Copies the tags of the full-resolution image overviews share: nodata, the color table and
/// the JPEG reference values.
fn shared_tags(main: &Directory, dir: &mut Directory) {
    for tag in [320, 532, 42113] {
        if let Some(e) = main.get(tag) {
            dir.set(e.clone());
        }
    }
}

/// Warns that overview arithmetic on 64-bit integers may lose precision above 2^53.
fn warn_integer_precision(data_type: DataType) {
    if matches!(data_type, DataType::I64 | DataType::U64) {
        crate::warnings::warn(
            "overview arithmetic uses Float64; 64-bit integer values above 2^53 may lose precision, including nearest and mode".into(),
        );
    }
}

/// Computes whole new levels of `sizes` (in cascade, largest first) as images like `like`
/// (without mask images: for an `.ovr` sidecar).
fn build_new_levels(
    src: &mut LazyRaster,
    like: &Directory,
    sizes: &[(usize, usize)],
    method: OverviewResampling,
    compression: &str,
    override_compression: bool,
    existing: Option<&[Directory]>,
) -> Result<Vec<gw::Image>> {
    warn_integer_precision(src.data_type());
    let sizes = sorted_sizes(sizes.to_vec());
    let (layout, new_compression, ..) = layout_like(like, Some(compression), true, src.data_type())?;
    let blocks: Vec<(usize, usize)> = sizes
        .iter()
        .map(|size| match existing.and_then(|dirs| dirs.iter().find(|d| !d.is_mask() && d.size() == *size)) {
            Some(dir) => layout_like(dir, None, false, src.data_type()).map(|(l, ..)| chunk_size(&l, size.0)),
            None => Ok(chunk_size(&layout, size.0)),
        })
        .collect::<Result<_>>()?;
    let alpha = alpha_band(src);
    let multiband = gdal_multiband(like, true, new_compression != Compression::None, src.band_count(), alpha.is_some());
    let base_block = base_block(src);
    let (data, _) = schedules((src.width(), src.height()), base_block, &sizes, &blocks, method, src.data_type(), multiband, src.has_mask());
    let levels: Vec<WholeLevel> = sizes.iter().zip(data).map(|(&size, data)| WholeLevel { size, method, data, mask: None }).collect();
    let mut srcs = Sources { src, base: None, alpha, read_bytes: READ_BYTES, strip_values: STRIP_VALUES };
    let mut out = Vec::new();
    whole_levels(&mut srcs, &levels, |srcs, _, c| {
        let old = existing.and_then(|dirs| dirs.iter().find(|d| !d.is_mask() && d.size() == c.size));
        let compression = if old.is_some() && !override_compression { None } else { Some(compression) };
        out.push(new_level_image(c, old.unwrap_or(like), compression, method, srcs.src.data_type(), srcs.src.band_count())?.0);
        Ok(())
    })?;
    Ok(out)
}

/// The block size of the full-resolution image (strips: the whole width).
fn base_block(src: &LazyRaster) -> (usize, usize) {
    src.block_size().unwrap_or((src.width(), src.height()))
}

/// Writes a new `.ovr` file: the overview images, largest first, with the tags of the
/// full-resolution image `main` they share.
fn write_ovr(f: &mut std::fs::File, images: Vec<gw::Image>, main: &Directory) -> Result<()> {
    let bytes: u64 = images.iter().flat_map(|i| i.chunks.iter()).map(|c| c.len() as u64 + 2).sum();
    let big = bytes > u32::MAX as u64 - (1 << 24);
    f.write_all(&gtiff_update::header(big))?;
    let mut a = Appender::new(f, big)?;
    let mut dirs = Vec::new();
    for img in images {
        let mut dir = a.image(img)?;
        shared_tags(main, &mut dir);
        dirs.push(dir);
    }
    a.finish(&dirs)
}

/// Adds overviews of these factors to a TIFF (the file, or a copy of it).
fn add_internal<F: Read + Write + Seek>(
    file: &mut F,
    src: &mut LazyRaster,
    factors: &[usize],
    method: OverviewResampling,
    compression: Option<&str>,
    ignore_cog_layout_break: bool,
) -> Result<Vec<(usize, usize)>> {
    let tiff = gtiff_update::parse(file)?;
    if factors.is_empty() {
        // Nothing to add, so a Cloud Optimized GeoTIFF's layout is not broken for nothing.
        return Ok(sorted_sizes(images(&tiff).overviews.iter().map(|o| o.0).collect()));
    }
    gtiff_update::break_cog_layout(file, tiff.big, ignore_cog_layout_break)?;
    let im = images(&tiff);
    let main = im.main.ok_or_else(|| Error::InvalidData("the TIFF has no full-resolution image".into()))?;
    let (w, h) = (src.width(), src.height());
    // Each factor's existing overview, or the size of a new one.
    let mut used = vec![false; im.overviews.len()];
    let mut sizes = Vec::new();
    for &f in factors {
        match im.overviews.iter().enumerate().find(|(j, o)| !used[*j] && matches_factor(o.0, (w, h), f)) {
            Some((j, o)) => {
                used[j] = true;
                sizes.push(o.0);
            }
            None => sizes.push((w.div_ceil(f), h.div_ceil(f))),
        }
    }
    let sizes = sorted_sizes(sizes);
    let main_dir = &tiff.dirs[main];
    if method == OverviewResampling::Average && main_dir.uint(262) == Some(3) {
        return Err(Error::Unsupported(
            "average overviews of palette images are not supported (GDAL averages their colors): use nearest or mode".into(),
        ));
    }
    // The block of each level: an existing level's own, else the one new images take.
    let (new_layout, ..) = layout_like(main_dir, compression, true, src.data_type())?;
    let blocks: Vec<(usize, usize)> = sizes
        .iter()
        .map(|s| match im.overviews.iter().find(|o| o.0 == *s) {
            Some(&(_, i, _)) => layout_like(&tiff.dirs[i], None, false, src.data_type()).map(|(l, ..)| chunk_size(&l, s.0)),
            None => Ok(chunk_size(&new_layout, s.0)),
        })
        .collect::<Result<_>>()?;
    let alpha = alpha_band(src);
    let multiband = gdal_multiband(main_dir, false, true, src.band_count(), alpha.is_some());
    let (mut data, masks) = schedules((w, h), base_block(src), &sizes, &blocks, method, src.data_type(), multiband, im.main_mask);
    if !im.main_mask && src.has_mask() {
        // A `.msk` sidecar: its pixels count, and GDAL cannot cascade over it.
        data = schedules((w, h), base_block(src), &sizes, &blocks, method, src.data_type(), multiband, true).0;
    }
    let plan: Vec<WholeLevel> =
        sizes.iter().zip(data).zip(masks).map(|((&size, data), mask)| WholeLevel { size, method, data, mask }).collect();
    let mut srcs = Sources { src, base: None, alpha, read_bytes: READ_BYTES, strip_values: STRIP_VALUES };
    let mut a = Appender::new(file, tiff.big)?;
    // Every overview of the result: (size, directory, mask directory).
    let mut levels: Vec<((usize, usize), Directory, Option<Directory>)> = Vec::new();
    whole_levels(&mut srcs, &plan, |srcs, _, c| {
        let size = c.size;
        let existing = im.overviews.iter().find(|o| o.0 == size);
        let src = &mut *srcs.src;
        let (dir, mask) = match existing {
            Some(&(_, i, old_mask)) if compression.is_some() => {
                let (image, mask) = new_level_image(c, &tiff.dirs[i], compression, method, src.data_type(), src.band_count())?;
                let mut dir = a.image(image)?;
                shared_tags(main_dir, &mut dir);
                let mask = if im.main_mask || old_mask.is_some() { mask.map(|m| a.image(m)).transpose()? } else { None };
                (dir, mask)
            }
            // An existing level is recomputed in its own layout.
            Some(&(_, i, mask)) => {
                let mut dir = replace_chunks(&mut a, &tiff.dirs[i], c, 0..c.size.0, src.data_type(), src.band_count(), None)?;
                dir.set(Entry::new(42112, &Value::Ascii(resampling_item(method)), tiff.big));
                let mask = match (mask, &c.mask) {
                    (Some(m), Some(_)) => {
                        Some(replace_chunks(&mut a, &tiff.dirs[m], c, 0..c.size.0, src.data_type(), src.band_count(), Some(&tiff.dirs[i]))?)
                    }
                    (None, Some(_)) if im.main_mask => {
                        new_level_image(c, main_dir, compression, method, src.data_type(), src.band_count())?
                            .1
                            .map(|m| a.image(m))
                            .transpose()?
                    }
                    _ => None,
                };
                (dir, mask)
            }
            None => {
                let (img, mask) = new_level_image(c, main_dir, compression, method, src.data_type(), src.band_count())?;
                let mut dir = a.image(img)?;
                shared_tags(main_dir, &mut dir);
                let mask = if im.main_mask { mask.map(|m| a.image(m)).transpose()? } else { None };
                (dir, mask)
            }
        };
        levels.push((size, dir, mask));
        Ok(())
    })?;
    for &(size, i, mask) in &im.overviews {
        if !sizes.contains(&size) {
            levels.push((size, tiff.dirs[i].clone(), mask.map(|m| tiff.dirs[m].clone())));
        }
    }
    levels.sort_by_key(|l| std::cmp::Reverse(l.0));
    let mut chain: Vec<Directory> = im.base.iter().map(|&i| tiff.dirs[i].clone()).collect();
    for (_, dir, mask) in &levels {
        chain.push(dir.clone());
        chain.extend(mask.clone());
    }
    a.finish(&chain)?;
    Ok(levels.iter().map(|l| l.0).collect())
}

/// Re-encodes the chunks of `dir` (an overview, or with `mask_of` the mask of that overview)
/// that rows `c.rows` and columns `cols` of the computed level cover, and returns the directory
/// pointing at them (and at the old chunks elsewhere).
pub(crate) fn replace_chunks<F: Write + Seek>(
    a: &mut Appender<'_, F>,
    dir: &Directory,
    c: &Computed,
    cols: std::ops::Range<usize>,
    data_type: DataType,
    bands: usize,
    mask_of: Option<&Directory>,
) -> Result<Directory> {
    let (w, h) = dir.size();
    if (w, h) != c.size {
        return Err(Error::InvalidData("an overview changed size".into()));
    }
    let rows_n = c.rows.len();
    // Only the tile columns of `cols` are encoded (strips span the whole width).
    let tile_w = if dir.tiled() { dir.uint(322).unwrap_or(256) as usize } else { w };
    let (tx0, tx1) = (cols.start / tile_w, cols.end.div_ceil(tile_w).min(w.div_ceil(tile_w)));
    let (x0, x1) = (tx0 * tile_w, (tx1 * tile_w).min(w));
    let whole = (x0, x1) == (0, w);
    let (img, chunk) = match mask_of {
        None => {
            let (layout, compression, codec) = layout_like(dir, None, false, data_type)?;
            if dir.uint(277).unwrap_or(1) as usize != bands {
                return Err(Error::InvalidData("an overview has another band count than the raster".into()));
            }
            let data = if whole {
                std::borrow::Cow::Borrowed(&c.data)
            } else {
                std::borrow::Cow::Owned(slice_cols(&c.data, rows_n, w, bands, x0..x1))
            };
            let level = gw::Level { cols: x1 - x0, rows: rows_n, bands, data };
            let chunk = chunk_size(&layout, w);
            (level.image(data_type, compression, &codec, &layout)?, chunk)
        }
        Some(_) => {
            let m = c.mask.as_ref().ok_or_else(|| Error::InvalidData("no mask computed".into()))?;
            let tiled = dir.tiled();
            let chunk = if tiled {
                (dir.uint(322).unwrap_or(256) as usize, dir.uint(323).unwrap_or(256) as usize)
            } else {
                (w, dir.uint(278).unwrap_or(h as u64).max(1) as usize)
            };
            let m = if whole {
                std::borrow::Cow::Borrowed(m.as_slice())
            } else {
                std::borrow::Cow::Owned(m.chunks(w).flat_map(|row| &row[x0..x1]).copied().collect())
            };
            (gw::mask_image(&m, x1 - x0, rows_n, (chunk.0, chunk.1.min(rows_n), tiled))?, chunk)
        }
    };
    let (cw, ch) = (chunk.0, chunk.1.min(h.max(1)));
    if !c.rows.start.is_multiple_of(ch) || (c.rows.end != h && !c.rows.end.is_multiple_of(ch)) {
        return Err(Error::InvalidData("refreshed overview rows are not aligned to its blocks".into()));
    }
    let across = w.div_ceil(cw);
    let down = h.div_ceil(ch);
    let strip_down = rows_n.div_ceil(ch);
    let (mut offsets, mut counts) = dir.chunks();
    let planes = (offsets.len() / (across * down).max(1)).max(1);
    if offsets.len() != planes * across * down || counts.len() != offsets.len() {
        return Err(Error::InvalidData("an overview's chunk offsets do not match its size".into()));
    }
    // The encoded level's chunks: the covered tile columns, from the first.
    let covered = tx1 - tx0;
    let strip_per_plane = covered * strip_down;
    if img.chunks.len() != planes * strip_per_plane {
        return Err(Error::InvalidData("an overview's layout differs from the one recomputed".into()));
    }
    for (k, chunk) in img.chunks.into_iter().enumerate() {
        let (plane, j) = (k / strip_per_plane, k % strip_per_plane);
        let (ty, tx) = (j / covered + c.rows.start / ch, j % covered + tx0);
        let idx = plane * across * down + ty * across + tx;
        offsets[idx] = a.write(&chunk)?;
        counts[idx] = chunk.len() as u64;
    }
    let mut out = dir.clone();
    let big = a.is_big();
    let (o, cn) = if dir.tiled() { (324, 325) } else { (273, 279) };
    out.set(Entry::new(o, &Value::Offsets(offsets), big));
    out.set(Entry::new(cn, &Value::Offsets(counts), big));
    Ok(out)
}

/// Removes the overviews (and their masks) from the chain of a TIFF.
fn delete_internal<F: Read + Write + Seek>(file: &mut F, ignore_cog_layout_break: bool) -> Result<()> {
    let tiff = gtiff_update::parse(file)?;
    let chain: Vec<Directory> = tiff.dirs.iter().filter(|d| d.subfile() & gtiff_update::REDUCED == 0).cloned().collect();
    if chain.len() == tiff.dirs.len() {
        return Ok(());
    }
    gtiff_update::break_cog_layout(file, tiff.big, ignore_cog_layout_break)?;
    Appender::new(file, tiff.big)?.finish(&chain)
}

/// Refreshes the overviews of a TIFF (a GeoTIFF, or the `.ovr` of `src`).
pub(crate) fn refresh<F: Read + Write + Seek>(file: &mut F, src: &mut LazyRaster, o: &RefreshOptions) -> Result<Vec<(usize, usize)>> {
    warn_integer_precision(src.data_type());
    let tiff = gtiff_update::parse(file)?;
    let im = images(&tiff);
    let (w, h) = (src.width(), src.height());
    if let Some(f) = o.levels.iter().find(|f| **f < 2) {
        return Err(Error::InvalidArgument(format!("an overview level is a decimation factor of 2 or more, not {f}")));
    }
    if let Some(b) = o.bbox.iter().find(|b| !(b.iter().all(|v| v.is_finite()) && b[0] < b[2] && b[1] < b[3])) {
        return Err(Error::InvalidArgument(format!("bounding box {b:?} is not [xmin, ymin, xmax, ymax] with xmin < xmax and ymin < ymax")));
    }
    let mut targets: Vec<((usize, usize), usize, Option<usize>)> = im
        .overviews
        .iter()
        .copied()
        .filter(|(s, ..)| o.levels.is_empty() || o.levels.iter().any(|f| matches_factor(*s, (w, h), *f)))
        .collect();
    if targets.is_empty() {
        return Err(Error::InvalidArgument("no overview to refresh".into()));
    }
    targets.sort_by_key(|t| std::cmp::Reverse(t.0));
    let methods: Vec<OverviewResampling> = targets
        .iter()
        .map(|(_, i, _)| match o.resampling {
            Some(r) => overview_method(r),
            None => stored_method(&tiff.dirs[*i]).unwrap_or(Ok(OverviewResampling::Nearest)),
        })
        .collect::<Result<_>>()?;
    // The full-resolution pixels to refresh from, [x0, y0, x1, y1).
    let region = match o.bbox.as_slice() {
        [] => Some([0, 0, w, h]),
        boxes => {
            let inv = super::invert_gt(&src.geo_transform())
                .ok_or_else(|| Error::InvalidData("the raster's geotransform cannot be inverted".into()))?;
            let mut r: Option<[usize; 4]> = None;
            for b in boxes {
                let pts = [(b[0], b[1]), (b[2], b[1]), (b[0], b[3]), (b[2], b[3])].map(|(x, y)| super::apply_gt(&inv, x, y));
                let (c0, c1) =
                    (pts.iter().map(|p| p.0).fold(f64::INFINITY, f64::min), pts.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max));
                let (l0, l1) =
                    (pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min), pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max));
                let x0 = c0.floor().clamp(0.0, w as f64) as usize;
                let x1 = c1.ceil().clamp(0.0, w as f64) as usize;
                let y0 = l0.floor().clamp(0.0, h as f64) as usize;
                let y1 = l1.ceil().clamp(0.0, h as f64) as usize;
                if x0 < x1 && y0 < y1 {
                    r = Some(match r {
                        None => [x0, y0, x1, y1],
                        Some(p) => [p[0].min(x0), p[1].min(y0), p[2].max(x1), p[3].max(y1)],
                    });
                }
            }
            r
        }
    };
    let Some(region) = region else {
        crate::warnings::warn("the extents to refresh do not intersect the raster: no overview changed".into());
        return Ok(Vec::new());
    };
    gtiff_update::break_cog_layout(file, tiff.big, o.ignore_cog_layout_break)?;
    let sizes: Vec<(usize, usize)> = targets.iter().map(|t| t.0).collect();
    let blocks: Vec<(usize, usize)> = targets
        .iter()
        .map(|&(s, i, _)| layout_like(&tiff.dirs[i], None, false, src.data_type()).map(|(l, ..)| chunk_size(&l, s.0)))
        .collect::<Result<_>>()?;
    let alpha = alpha_band(src);
    let multiband = match im.main {
        Some(m) => gdal_multiband(&tiff.dirs[m], false, true, src.band_count(), alpha.is_some()),
        None => {
            let first = &tiff.dirs[targets[0].1];
            gdal_multiband(first, true, first.uint(259).unwrap_or(1) != 1, src.band_count(), alpha.is_some())
        }
    };
    let has_mask = targets.iter().all(|t| t.2.is_some());
    let whole = region == [0, 0, w, h];
    let mut a = Appender::new(file, tiff.big)?;
    let mut dirs = tiff.dirs.clone();
    let mut srcs = Sources { src, base: None, alpha, read_bytes: READ_BYTES, strip_values: STRIP_VALUES };
    // Each target's schedules, for its own method.
    let plan: Vec<WholeLevel> = targets
        .iter()
        .zip(&methods)
        .enumerate()
        .map(|(k, (&(size, _, mask), &method))| {
            let (mut data, mut masks) =
                schedules((w, h), base_block(srcs.src), &sizes, &blocks, method, srcs.src.data_type(), multiband, has_mask);
            if !whole {
                // Partial refreshes compute whole windows (GDALRegenerateOverviewsMultiBand's region).
                for s in data.iter_mut().chain(masks.iter_mut().flatten()) {
                    s.chunk_rows = None;
                }
            }
            WholeLevel { size, method, data: data[k], mask: masks[k].filter(|_| mask.is_some()) }
        })
        .collect();
    // Writes a target's refreshed rows `c.rows` over columns `cols`.
    let mut write = |srcs: &mut Sources, k: usize, c: &Computed, cols: std::ops::Range<usize>| -> Result<()> {
        let (_, i, mask) = targets[k];
        let (data_type, bands) = (srcs.src.data_type(), srcs.src.band_count());
        let mut dir = replace_chunks(&mut a, &tiff.dirs[i], c, cols.clone(), data_type, bands, None)?;
        if whole {
            dir.set(Entry::new(42112, &Value::Ascii(resampling_item(plan[k].method)), tiff.big));
        }
        dirs[i] = dir;
        if let (Some(m), Some(_)) = (mask, &c.mask) {
            dirs[m] = replace_chunks(&mut a, &tiff.dirs[m], c, cols, data_type, bands, Some(&tiff.dirs[i]))?;
        }
        Ok(())
    };
    if whole {
        whole_levels(&mut srcs, &plan, |srcs, k, c| write(srcs, k, c, 0..c.size.0))?;
    } else {
        let mut prev: Option<Computed> = None;
        for (k, l) in plan.iter().enumerate() {
            let size = l.size;
            // GDAL's region of the overview, and the rows of whole blocks over it.
            const EPS: f64 = 1e-8;
            let dx0 = (region[0] as f64 / w as f64 * size.0 as f64 + EPS) as usize;
            let dx1 = ((region[2] as f64 / w as f64 * size.0 as f64 - EPS).ceil() as usize).min(size.0).max(dx0 + 1);
            let dy0 = (region[1] as f64 / h as f64 * size.1 as f64 + EPS) as usize;
            let dy1 = ((region[3] as f64 / h as f64 * size.1 as f64 - EPS).ceil() as usize).min(size.1).max(dy0 + 1);
            let ch = blocks[k].1.min(size.1).max(1);
            let (r0, r1) = (dy0 / ch * ch, (dy1.div_ceil(ch) * ch).min(size.1));
            let mut c = compute(&mut srcs, size, r0..r1, prev.as_ref(), l.method, l.data, l.mask, dx0)?;
            if (dx0, dy0, dx1, dy1) != (0, 0, size.0, size.1) {
                // Outside the region, the overview keeps its pixels.
                let level = srcs.src.overviews().iter().position(|s| *s == size).ok_or_else(|| {
                    Error::InvalidData(format!("the raster has no stored overview of {}x{} to keep its pixels from", size.0, size.1))
                })?;
                let (old, old_mask) = srcs.rows(Some(level), size, r0..r1, None, c.mask.is_some())?;
                keep_outside(&mut c, &old, old_mask.as_deref(), dx0..dx1, dy0..dy1, srcs.src.band_count());
            }
            write(&mut srcs, k, &c, dx0..dx1)?;
            prev = Some(c);
        }
    }
    a.finish(&dirs)?;
    Ok(targets.iter().map(|t| t.0).collect())
}

/// Puts back the pixels (and mask values) of `old` (the level's stored rows `c.rows`) outside
/// columns `cols` and rows `rows` of a computed level.
fn keep_outside(
    c: &mut Computed,
    old: &RasterData,
    old_mask: Option<&[u8]>,
    cols: std::ops::Range<usize>,
    rows: std::ops::Range<usize>,
    bands: usize,
) {
    let (w, n) = (c.size.0, c.rows.len());
    let inside = |y: usize, x: usize| rows.contains(&y) && cols.contains(&x);
    macro_rules! keep {
        ($($t:ident),*) => {
            match (&mut c.data, old) {
                $((RasterData::$t(d), RasterData::$t(o)) => {
                    for b in 0..bands {
                        for (j, y) in c.rows.clone().enumerate() {
                            for x in 0..w {
                                if !inside(y, x) {
                                    let i = (b * n + j) * w + x;
                                    d[i] = o[i];
                                }
                            }
                        }
                    }
                })*
                _ => {}
            }
        };
    }
    keep!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64);
    if let (Some(m), Some(o)) = (c.mask.as_mut(), old_mask) {
        for (j, y) in c.rows.clone().enumerate() {
            for x in 0..w {
                if !inside(y, x) {
                    m[j * w + x] = o[j * w + x];
                }
            }
        }
    }
}

fn invalid(value: &str) -> Error {
    Error::InvalidArgument(format!("invalid overview selection '{value}' (AUTO, AUTO-n, NONE or a level from 0)"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gdal_ratios_and_fallbacks() {
        let levels = [(512, 512), (256, 256), (128, 128)];
        assert_eq!(OverviewSelection::Auto.select(1024, &levels, 1.0), None);
        assert_eq!(OverviewSelection::Auto.select(1024, &levels, 1.95), Some(0));
        assert_eq!(OverviewSelection::Auto.select(1024, &levels, 3.0), Some(0));
        assert_eq!(OverviewSelection::Auto.select(1024, &levels, 4.0), Some(1));
        assert_eq!(OverviewSelection::AutoOffset(1).select(1024, &levels, 4.0), Some(0));
        assert_eq!(OverviewSelection::AutoOffset(3).select(1024, &levels, 4.0), None);
        assert_eq!(OverviewSelection::None.select(1024, &levels, 16.0), None);
        assert_eq!(OverviewSelection::Level(99).select(1024, &levels, 1.0), Some(2));
        assert_eq!(OverviewSelection::Level(0).select(1024, &[], 1.0), None);
        assert_eq!(OverviewSelection::parse("auto-2").unwrap(), OverviewSelection::AutoOffset(2));
        for invalid in ["-1", "AUTO-", "AUTO--1", "bilinear"] {
            assert!(OverviewSelection::parse(invalid).is_err());
        }
    }

    #[test]
    fn like_datasets_without_a_geotransform_are_refused() {
        let config = wbraster::RasterConfig { cols: 2, rows: 2, bands: 1, data_type: DataType::U8, ..Default::default() };
        let raster = wbraster::Raster::from_data(config, vec![1., 2., 3., 4.]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unreferenced.tif");
        std::fs::write(&path, gw::encode(&raster, &gw::TiffOptions { georeferenced: false, ..Default::default() }).unwrap()).unwrap();
        let err = dataset_bounds(path.to_str().unwrap(), &crate::crs::SpatialRef::default()).unwrap_err();
        assert!(err.to_string().contains("has no geotransform"), "{err}");
    }

    #[test]
    fn whole_level_reads_share_the_pixel_buffer() {
        let config = wbraster::RasterConfig { cols: 2, rows: 2, bands: 1, data_type: DataType::U8, ..Default::default() };
        let raster = wbraster::Raster::from_data(config, vec![1., 2., 3., 4.]).unwrap();
        let mut src = LazyRaster::from(crate::RasterDataset::new(raster, false));
        let mut sources = Sources { src: &mut src, base: None, alpha: None, read_bytes: READ_BYTES, strip_values: STRIP_VALUES };
        let (a, _) = sources.rows(None, (2, 2), 0..2, None, false).unwrap();
        let (b, _) = sources.rows(None, (2, 2), 0..2, None, false).unwrap();
        assert!(std::sync::Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn whole_levels_match_whole_image_compute_for_every_type_and_method() {
        // Every method, in types whose work types and conversions differ.
        let (w, h) = (300, 211);
        let sizes = [(150, 106), (75, 53), (19, 14)];
        let blocks = [(64, 64); 3];
        let methods = [
            OverviewResampling::Nearest,
            OverviewResampling::Average,
            OverviewResampling::Mode,
            OverviewResampling::Min,
            OverviewResampling::Max,
        ];
        for (data_type, nodata) in [(DataType::U8, 0.0), (DataType::I16, -7.0), (DataType::U16, 3.0), (DataType::F32, -1.5)] {
            let values: Vec<f64> = (0..w * h)
                .map(|i| match data_type {
                    _ if i % 11 == 4 => nodata,
                    DataType::U8 => (i * 31 % 251) as f64,
                    DataType::I16 => (i * 37 % 4001) as f64 - 2000.0,
                    DataType::U16 => (i * 41 % 60001) as f64,
                    _ => (i * 43 % 1009) as f64 * 0.37 - 100.0,
                })
                .collect();
            let config = wbraster::RasterConfig { cols: w, rows: h, bands: 2, data_type, nodata, ..Default::default() };
            let doubled: Vec<f64> = values.iter().chain(values.iter().rev()).copied().collect();
            let raster = wbraster::Raster::from_data(config, doubled).unwrap();
            let options = gw::TiffOptions { tile_size: Some(64), nodata: Some(nodata), ..Default::default() };
            let tiff = gw::encode(&raster, &options).unwrap();
            for method in methods {
                for multiband in [false, true] {
                    let (data, masks) = schedules((w, h), (64, 64), &sizes, &blocks, method, data_type, multiband, false);
                    let mut whole = LazyRaster::from_tiff_bytes(tiff.clone()).unwrap().unwrap();
                    let mut srcs = Sources { src: &mut whole, base: None, alpha: None, read_bytes: READ_BYTES, strip_values: STRIP_VALUES };
                    let mut expected: Vec<Computed> = Vec::new();
                    for (k, &size) in sizes.iter().enumerate() {
                        let c = compute(&mut srcs, size, 0..size.1, expected.last(), method, data[k], masks[k], 0).unwrap();
                        expected.push(c);
                    }
                    let mut src = LazyRaster::from_tiff_bytes(tiff.clone()).unwrap().unwrap();
                    let plan: Vec<WholeLevel> =
                        sizes.iter().enumerate().map(|(k, &size)| WholeLevel { size, method, data: data[k], mask: masks[k] }).collect();
                    // Reads of 64 rows, and strips of 64 source rows (with their margins).
                    let mut srcs = Sources { src: &mut src, base: None, alpha: None, read_bytes: 1, strip_values: 20 * w };
                    whole_levels(&mut srcs, &plan, |_, k, c| {
                        assert_eq!(c.data.data_type(), data_type);
                        assert_eq!(c.data.to_f64_vec(), expected[k].data.to_f64_vec(), "{data_type:?} {method:?} {multiband} level {k}");
                        Ok(())
                    })
                    .unwrap();
                }
            }
        }
    }

    /// A raster read by window that records the windows of its full-resolution reads: `(mask,
    /// window)`.
    struct Recording {
        inner: std::sync::Mutex<LazyRaster>,
        reads: std::sync::Arc<std::sync::Mutex<Vec<(bool, Window)>>>,
    }

    impl crate::cog::WindowSource for Recording {
        fn overviews(&self) -> Vec<(usize, usize)> {
            self.inner.lock().unwrap().overviews()
        }
        fn block_size(&self) -> Option<(usize, usize)> {
            self.inner.lock().unwrap().block_size()
        }
        fn has_mask(&self) -> bool {
            self.inner.lock().unwrap().has_mask()
        }
        fn read_mask(&mut self, level: usize, window: Window) -> Result<Option<Vec<u8>>> {
            self.reads.lock().unwrap().push((true, window));
            self.inner.get_mut().unwrap().read_mask(window, level.checked_sub(1))
        }
        fn read(&mut self, level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
            self.reads.lock().unwrap().push((false, window));
            let options = WindowOptions { bands: bands.to_vec(), overview: level.checked_sub(1) };
            Ok(self.inner.get_mut().unwrap().read_window(window, &options)?.raster.data)
        }
    }

    #[test]
    fn whole_levels_stream_the_image_once_as_whole_reads_compute_them() {
        // Float64 rows of 2600 pixels: GDAL's chunks are 256 rows, the whole windows' strips 512.
        let (w, h) = (2600, 700);
        let mask: Vec<u8> = (0..w * h).map(|i| if (i / w + i % w) % 17 == 0 { 0 } else { 255 }).collect();
        let sizes = [(1300, 350), (650, 175), (217, 59)];
        let blocks = [(256, 256); 3];
        // One band, with and without a mask, and pixel-interleaved bands with an alpha band.
        for (bands, alpha, has_mask) in [(1, None, false), (1, None, true), (3, Some(2), false)] {
            let values: Vec<f64> = (0..w * h * bands)
                .map(|i| match i / (w * h) {
                    b if Some(b) == alpha => (i % 89 > 9) as u8 as f64 * 255.0,
                    b => {
                        if i % 13 == 5 {
                            -1.0
                        } else {
                            ((i * 7919 + b) % 1013) as f64 * 0.5
                        }
                    }
                })
                .collect();
            let config = wbraster::RasterConfig { cols: w, rows: h, bands, data_type: DataType::F64, nodata: -1.0, ..Default::default() };
            let raster = wbraster::Raster::from_data(config, values).unwrap();
            let options = gw::TiffOptions {
                tile_size: Some(256),
                nodata: Some(-1.0),
                mask: has_mask.then(|| mask.clone()),
                alpha: alpha.is_some(),
                ..Default::default()
            };
            let tiff = gw::encode(&raster, &options).unwrap();
            for method in [OverviewResampling::Nearest, OverviewResampling::Average] {
                for multiband in [false, true] {
                    let (data, masks) = schedules((w, h), (256, 256), &sizes, &blocks, method, DataType::F64, multiband, has_mask);
                    // The levels as computed from the whole image.
                    let mut whole = LazyRaster::from_tiff_bytes(tiff.clone()).unwrap().unwrap();
                    let mut srcs = Sources { src: &mut whole, base: None, alpha, read_bytes: READ_BYTES, strip_values: STRIP_VALUES };
                    let mut expected: Vec<Computed> = Vec::new();
                    for (k, &size) in sizes.iter().enumerate() {
                        let c = compute(&mut srcs, size, 0..size.1, expected.last(), method, data[k], masks[k], 0).unwrap();
                        expected.push(c);
                    }
                    // Streamed, through a source recording its reads.
                    let inner = LazyRaster::from_tiff_bytes(tiff.clone()).unwrap().unwrap();
                    let reads = std::sync::Arc::default();
                    let header = inner.header();
                    let recording = Recording { inner: inner.into(), reads: std::sync::Arc::clone(&reads) };
                    let mut src = LazyRaster::from_source(header, Box::new(recording));
                    let plan: Vec<WholeLevel> =
                        sizes.iter().enumerate().map(|(k, &size)| WholeLevel { size, method, data: data[k], mask: masks[k] }).collect();
                    let mut seen = 0;
                    whole_levels(
                        &mut Sources { src: &mut src, base: None, alpha, read_bytes: 1 << 20, strip_values: STRIP_VALUES },
                        &plan,
                        |_, k, c| {
                            let e = &expected[k];
                            assert_eq!(c.data.to_f64_vec(), e.data.to_f64_vec(), "{method:?} {multiband} {bands} {has_mask} level {k}");
                            assert_eq!(c.mask, e.mask, "{method:?} {multiband} {bands} {has_mask} level {k} mask");
                            seen += 1;
                            Ok(())
                        },
                    )
                    .unwrap();
                    assert_eq!(seen, sizes.len());
                    // Every full-resolution row is read once, forward, in windows of whole blocks.
                    let reads = reads.lock().unwrap();
                    for mask in [false, true] {
                        let windows: Vec<Window> = reads.iter().filter(|r| r.0 == mask).map(|r| r.1).collect();
                        if mask && !(has_mask && (masks[0].is_some() || method != OverviewResampling::Nearest)) {
                            assert!(windows.is_empty(), "{windows:?}");
                            continue;
                        }
                        assert!(windows.len() > 1, "{method:?} {multiband} {has_mask}: {windows:?}");
                        let mut next = 0;
                        for win in &windows {
                            assert_eq!((win.x, win.width, win.y), (0, w, next), "{method:?} {multiband} {has_mask}: {windows:?}");
                            assert!(win.height <= 512 && (win.y + win.height == h || win.height % 256 == 0), "{windows:?}");
                            next += win.height;
                        }
                        assert_eq!(next, h);
                    }
                }
            }
        }
    }
}

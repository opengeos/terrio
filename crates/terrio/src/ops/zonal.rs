//! Per-polygon statistics across raster bands. Zones are evaluated independently, so overlapping
//! zones can include the same pixel. Binary membership uses the GDAL-compatible rasterizer;
//! fractional membership uses polygon/cell areas, agreeing with GDAL 3.13.3 within documented
//! tolerances except for median and empty-zone sums. Zones are computed once per distinct raster
//! grid; rasters are read lazily, strip by strip, and only where zones lie.

#[path = "zonal_fractional.rs"]
mod fractional;
#[path = "zonal_index.rs"]
mod index;
#[path = "zonal_reduce.rs"]
mod reduce;

use super::rasterize::Grid;
use crate::crs::SpatialRef;
use crate::raster::RasterData;
use crate::{Error, FieldDef, FieldType, FieldValue, Geometry, Layer, LazyRaster, Result, Window};
use std::sync::{Arc, Mutex};

/// A statistic for [`zonal_stats`]. Standard deviation is the population standard deviation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Statistic {
    /// Number of finite, non-nodata pixels; fractional mode sums their coverage fractions as a Real.
    /// Zero when no valid pixels contribute.
    Count,
    /// Minimum pixel value.
    Min,
    /// Maximum pixel value.
    Max,
    /// Arithmetic mean: the sum over the count (coverage-weighted in fractional mode), as
    /// exactextract computes it; infinite, like the sum, when the sum overflows.
    Mean,
    /// Sum of pixel values.
    Sum,
    /// Population standard deviation.
    StdDev,
    /// Median; the mean of the two middle values for an even binary count.
    /// Fractional mode uses exactextract's interpolated coverage-weighted median.
    Median,
    /// Mean weighted by coverage and a second raster.
    WeightedMean,
    /// Sum weighted by coverage and a second raster.
    WeightedSum,
    /// Population standard deviation weighted by coverage and a second raster.
    WeightedStdDev,
    /// Population variance (coverage-weighted in fractional mode).
    Variance,
    /// Coefficient of variation: standard deviation divided by the mean; null when the mean is 0.
    Cv,
    /// Quantile at this percentage (0 to 100). Centre/all-touched pixels interpolate linearly
    /// (numpy's default); fractional mode uses exactextract 0.3.0's weighted quantile. The 50th
    /// percentile equals [`Statistic::Median`].
    Quantile(f64),
    /// Value with the largest total coverage (pixel count in binary modes); ties take the larger value.
    Mode,
    /// Value with the smallest total coverage; ties take the smaller value.
    Minority,
    /// Number of distinct values, an Integer; 0 when no valid pixels contribute.
    Variety,
}

impl Statistic {
    /// Parses count/min/max/mean/sum/stddev/median/variance/cv/mode/minority/variety,
    /// `quantile_P` (P a percentage from 0 to 100, such as `quantile_25` or `quantile_2.5`) or
    /// weighted_mean/weighted_sum/weighted_stddev. Aliases: `majority` (mode), `var`,
    /// `coefficient_of_variation`.
    pub fn parse(name: &str) -> Result<Self> {
        let lower = name.to_ascii_lowercase();
        if let Some(p) = lower.strip_prefix("quantile_") {
            return match p.parse::<f64>() {
                Ok(p) if (0.0..=100.0).contains(&p) => Ok(Self::Quantile(p)),
                _ => Err(Error::InvalidArgument(format!("quantile '{name}' must be quantile_P with P from 0 to 100"))),
            };
        }
        Ok(match lower.as_str() {
            "count" => Self::Count,
            "min" => Self::Min,
            "max" => Self::Max,
            "mean" => Self::Mean,
            "sum" => Self::Sum,
            "stddev" | "std" | "stdev" => Self::StdDev,
            "median" => Self::Median,
            "weighted_mean" => Self::WeightedMean,
            "weighted_sum" => Self::WeightedSum,
            "weighted_stddev" | "weighted_stdev" => Self::WeightedStdDev,
            "variance" | "var" => Self::Variance,
            "cv" | "coefficient_of_variation" => Self::Cv,
            "mode" | "majority" => Self::Mode,
            "minority" => Self::Minority,
            "variety" => Self::Variety,
            _ => return Err(Error::InvalidArgument(format!("unknown zonal statistic '{name}'"))),
        })
    }

    /// Whether this statistic uses the optional weighting raster.
    pub fn uses_weights(self) -> bool {
        matches!(self, Self::WeightedMean | Self::WeightedSum | Self::WeightedStdDev)
    }

    /// The suffix used in output field names.
    pub fn name(self) -> String {
        if let Self::Quantile(p) = self {
            return format!("quantile_{p}");
        }
        match self {
            Self::Count => "count",
            Self::Min => "min",
            Self::Max => "max",
            Self::Mean => "mean",
            Self::Sum => "sum",
            Self::StdDev => "stddev",
            Self::Median => "median",
            Self::WeightedMean => "weighted_mean",
            Self::WeightedSum => "weighted_sum",
            Self::WeightedStdDev => "weighted_stddev",
            Self::Variance => "variance",
            Self::Cv => "cv",
            Self::Mode => "mode",
            Self::Minority => "minority",
            Self::Variety => "variety",
            Self::Quantile(_) => unreachable!(),
        }
        .into()
    }

    /// Whether this statistic needs the zone's values sorted (median, quantiles, mode, ...).
    fn needs_values(self) -> bool {
        matches!(self, Self::Median | Self::Quantile(_) | Self::Mode | Self::Minority | Self::Variety)
    }
}

/// A raster read lazily, and the bands to summarize from it.
#[derive(Debug)]
pub struct ZonalSource {
    /// The raster: open, or a path opened while it is read.
    pub raster: ZonalRaster,
    /// Bands, in output order.
    pub bands: Vec<ZonalBand>,
}

/// The raster of a [`ZonalSource`].
#[derive(Debug)]
pub enum ZonalRaster {
    /// Any raster terrio opens: a local or remote COG, a STAC asset, Zarr, or one in memory
    /// (`LazyRaster::from(dataset)`).
    Open(LazyRaster),
    /// A path or URL, opened for its header and again while the source is reduced: a list of
    /// thousands of files keeps open only those being read, within the open-file limit.
    Path(String),
}

impl From<LazyRaster> for ZonalRaster {
    fn from(raster: LazyRaster) -> Self {
        Self::Open(raster)
    }
}

/// A band to summarize and its output names.
#[derive(Debug, Clone)]
pub struct ZonalBand {
    /// Band, numbered from 1.
    pub band: usize,
    /// Output fields are `prefix_STATISTIC`. Name collisions get `_2`, `_3`, etc.
    pub prefix: String,
    /// Optional time label (an ISO 8601 date or free text), kept by outputs with a time dimension.
    pub time: Option<String>,
    /// Optional band label (such as `B04`), kept by outputs with a band dimension.
    pub label: Option<String>,
}

impl ZonalBand {
    /// A band without labels.
    pub fn new(band: usize, prefix: impl Into<String>) -> Self {
        Self { band, prefix: prefix.into(), time: None, label: None }
    }
}

/// The field prefix of a labelled band: `time_label`, or `label` alone, with every character
/// outside `[A-Za-z0-9_]` replaced by `_` (`("2024-07-01", "B04")` gives `2024_07_01_B04`).
pub fn label_prefix(time: Option<&str>, label: &str) -> String {
    let joined = match time {
        Some(time) => format!("{time}_{label}"),
        None => label.to_string(),
    };
    joined.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect()
}

/// The time label of an ISO 8601 date-time from Python or JavaScript, named alike on every
/// surface: a UTC offset `+00:00` is `Z`, a zero fraction of a second is dropped
/// (`06:30:00.000Z` is `06:30:00Z`), and midnight, naive or UTC, is the date alone.
pub fn time_text(iso: &str) -> String {
    let mut t = iso.trim().to_string();
    if let Some(utc) = t.strip_suffix("+00:00") {
        t = format!("{utc}Z");
    }
    if let Some(dot) = t.rfind('.').filter(|&d| t[..d].contains('T')) {
        let end = t[dot + 1..].find(|c: char| !c.is_ascii_digit()).map_or(t.len(), |e| dot + 1 + e);
        if end > dot + 1 && t[dot + 1..end].bytes().all(|b| b == b'0') {
            t.replace_range(dot..end, "");
        }
    }
    for midnight in ["T00:00:00Z", "T00:00:00"] {
        if let Some(day) = t.strip_suffix(midnight) {
            return day.to_string();
        }
    }
    t
}

/// The bands of one labelled input (a CLI `--inputs` row, a Python dict, a JavaScript object):
/// each of `bands`, its fields named by [`label_prefix`] when the input has a time or a label,
/// else `unlabelled(band)`. A label given to several bands is suffixed with each band's number
/// (`B04_b1`, `B04_b2`), so that their fields say which band they hold. Every band gets a
/// `label`, `bN` when the input gives none, so that outputs with a band dimension can name it.
/// Blank times and labels are absent, as an empty `--inputs` cell is.
pub fn labelled_bands(bands: &[usize], time: Option<&str>, label: Option<&str>, unlabelled: impl Fn(usize) -> String) -> Vec<ZonalBand> {
    let (time, label) = (time.filter(|t| !t.trim().is_empty()), label.filter(|l| !l.trim().is_empty()));
    bands
        .iter()
        .map(|&band| {
            let name = match label {
                Some(label) if bands.len() > 1 => format!("{label}_b{band}"),
                Some(label) => label.to_string(),
                None => format!("b{band}"),
            };
            let prefix = if time.is_some() || label.is_some() { label_prefix(time, &name) } else { unlabelled(band) };
            ZonalBand { band, prefix, time: time.map(String::from), label: Some(name) }
        })
        .collect()
}

/// A weighting raster band, shared by every source. Its grid and CRS must match every value raster.
#[derive(Debug, Clone)]
pub struct ZonalWeight {
    /// Nonnegative weights; nodata/nonfinite weights make weighted statistics null. Each reader
    /// takes its own handle (`LazyRaster::shared`).
    pub raster: Arc<Mutex<LazyRaster>>,
    /// Band, numbered from 1.
    pub band: usize,
}

impl ZonalWeight {
    /// A weighting band of `raster`.
    pub fn new(raster: LazyRaster, band: usize) -> Self {
        Self { raster: Arc::new(Mutex::new(raster)), band }
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, LazyRaster>> {
        self.raster.lock().map_err(|_| Error::InvalidArgument("the weighting raster lock is poisoned".into()))
    }
}

/// Pixel inclusion and statistics for [`zonal_stats`].
#[derive(Debug, Clone)]
pub struct ZonalOptions {
    /// Use exact polygon/cell intersection areas in pixel coordinates, rather than a binary mask.
    pub fractional: bool,
    /// Optional second raster for weighted_mean, weighted_sum and weighted_stddev.
    pub weights: Option<ZonalWeight>,
    /// Statistics to calculate, in output order. Default: count, min, max, mean, sum, stddev.
    pub stats: Vec<Statistic>,
    /// Include every pixel touched by the polygon, instead of pixels whose centres are inside.
    pub all_touched: bool,
    /// Reproject output geometries after computing statistics on the original zones.
    pub dst_srs: Option<SpatialRef>,
    /// Round only newly computed floating statistics to 0–15 decimal places, ties away from zero.
    /// Integer counts and source attributes are unchanged. Trailing zero display is format-dependent.
    pub decimal_places: Option<u8>,
}

impl Default for ZonalOptions {
    fn default() -> Self {
        Self {
            stats: vec![Statistic::Count, Statistic::Min, Statistic::Max, Statistic::Mean, Statistic::Sum, Statistic::StdDev],
            all_touched: false,
            fractional: false,
            weights: None,
            dst_srs: None,
            decimal_places: None,
        }
    }
}

/// Holds `bytes` of the values budget that the passes of all sources share, waiting while other
/// passes hold it, until dropped. A pass always runs when no other holds any, so the values held
/// together stay within `VALUES_PER_PASS`, or within one pass's when that is larger.
fn hold_values(budget: &(Mutex<usize>, std::sync::Condvar), bytes: usize) -> Result<impl Drop + '_> {
    struct Held<'a>(&'a (Mutex<usize>, std::sync::Condvar), usize);
    impl Drop for Held<'_> {
        fn drop(&mut self) {
            if let Ok(mut used) = self.0.0.lock() {
                *used -= self.1;
            }
            self.0.1.notify_all();
        }
    }
    let lock = || Error::InvalidArgument("zonal values budget lock poisoned".into());
    let (used, released) = budget;
    let mut held = used.lock().map_err(|_| lock())?;
    while bytes > 0 && *held > 0 && *held + bytes > VALUES_PER_PASS {
        held = released.wait(held).map_err(|_| lock())?;
    }
    *held += bytes;
    Ok(Held(budget, bytes))
}

/// The most bytes of zone values that the passes of all sources hold together, for statistics
/// that keep values (median, quantiles, mode, minority, variety); one pass may exceed it when it
/// runs alone. While a zone finishes, a sorted copy of one band's values adds 16 bytes per pixel
/// of that zone. Small in unit tests, so that they reduce bands in several passes.
const VALUES_PER_PASS: usize = if cfg!(test) { 64 } else { 1 << 30 };

/// Errors on a repeated statistic, a quantile outside 0 to 100, or a zone that is not a polygon or
/// has non-finite coordinates.
fn check_stats_and_zones(stats: &[Statistic], zones: &Layer) -> Result<()> {
    for (i, stat) in stats.iter().enumerate() {
        if stats[..i].contains(stat) {
            return Err(Error::InvalidArgument(format!("duplicate statistic '{}'", stat.name())));
        }
        if let Statistic::Quantile(p) = stat
            && !(0.0..=100.0).contains(p)
        {
            return Err(Error::InvalidArgument(format!("quantile percentage {p} is not between 0 and 100")));
        }
    }
    for f in &zones.features {
        if let Some(g) = &f.geometry {
            if !matches!(g, Geometry::Polygon { .. } | Geometry::MultiPolygon(_)) {
                return Err(Error::InvalidArgument(format!("zone {} is not a polygon or multipolygon", f.fid)));
            }
            if g.all_coords().iter().any(|c| !c.x.is_finite() || !c.y.is_finite()) {
                return Err(Error::InvalidArgument(format!("zone {} has non-finite XY coordinates", f.fid)));
            }
        }
    }
    Ok(())
}

/// Rows read at a time: terrio's strip size for `bands` of this raster (about 8 MB of values, or
/// a row of blocks when larger), as other operations that read by strips use.
fn strip_rows(raster: &LazyRaster, bands: usize) -> usize {
    super::strips::small_strip_rows(raster.width(), bands, raster.data_type().size_bytes(), raster.block_size().map(|(_, h)| h))
}

/// Adds statistics for each zone and raster band, preserving source geometry by default, Z/M, attributes,
/// feature IDs and CRS. Zones must be polygons or multipolygons; null/empty geometries and zones
/// with no valid contributing pixels have count 0 and null values for the other statistics
/// (`variety` is 0). Nodata and non-finite pixels are excluded. Holes are excluded; overlapping
/// zones are evaluated independently. Statistics use f64 values; integers above 2^53 can lose precision.
///
/// Zones are computed once per distinct grid (dimensions, geotransform and CRS) and shared by
/// every source on it. Sources are read lazily in strips of whole block rows, skipping strips
/// without zones, so rasters are never loaded whole. Median, quantiles, mode, minority and variety
/// keep the values of each zone being reduced, for every band of every source read at once (as
/// many as the pool has threads): memory grows with zone size, not with the raster.
///
/// `dst_srs` reprojects output geometries after analysis; `decimal_places` rounds only newly
/// computed floating statistics. Both are optional and leave integer counts and source attributes intact.
/// Raster grids may be rotated or south-up and use different CRSs. Each boundary is reprojected
/// into each raster's CRS before rasterization; a boundary transformation failure is an error.
/// Missing CRSs are accepted only when both zone and raster CRS are missing. Curved edges are
/// not densified during reprojection. Complex rasters and geolocation arrays are unsupported.
///
/// ```no_run
/// use terrio::ops::zonal::{zonal_stats, ZonalBand, ZonalSource};
/// let zones = terrio::open_vector("counties.gpkg", &Default::default())?;
/// let dem = terrio::open_raster_lazy("dem.tif")?;
/// let mut sources = [ZonalSource { raster: dem.into(), bands: vec![ZonalBand::new(1, "elevation")] }];
/// let stats = zonal_stats(&zones.layers[0], &mut sources, &Default::default())?;
/// terrio::write_vector(&[stats], "statistics.gpkg", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn zonal_stats(zones: &Layer, sources: &mut [ZonalSource], options: &ZonalOptions) -> Result<Layer> {
    super::sample::validate_decimal_places(options.decimal_places)?;
    let mut out = zones.clone();
    let mut columns = Vec::new();
    for source in sources.iter() {
        let mut source_columns = Vec::new();
        for band in &source.bands {
            if band.prefix.trim().is_empty() {
                return Err(Error::InvalidArgument("zonal field prefixes cannot be empty".into()));
            }
            let mut indices = Vec::new();
            for &stat in &options.stats {
                let name = super::geometry::unique_field(&out, &format!("{}_{}", band.prefix, stat.name()));
                indices.push(out.schema.len());
                out.schema.add_field(FieldDef::new(
                    name,
                    if (stat == Statistic::Count && !options.fractional) || stat == Statistic::Variety {
                        FieldType::Integer
                    } else {
                        FieldType::Float
                    },
                ));
            }
            source_columns.push(indices);
        }
        columns.push(source_columns);
    }
    for f in &mut out.features {
        f.attributes.resize(out.schema.len(), FieldValue::Null);
    }
    let out = Mutex::new(out);
    zonal_stats_with(zones, sources, options, &|stats| {
        let mut out = out.lock().map_err(|_| Error::InvalidArgument("zonal output lock poisoned".into()))?;
        for (stat_values, &column) in stats.values.into_iter().zip(&columns[stats.source][stats.band]) {
            for (feature, value) in out.features.iter_mut().zip(stat_values) {
                feature.attributes[column] = value;
            }
        }
        Ok(())
    })?;
    let out = out.into_inner().map_err(|_| Error::InvalidArgument("zonal output lock poisoned".into()))?;
    super::sample::finish_output(out, zones.schema.len(), options.dst_srs.as_ref(), options.decimal_places)
}

/// Statistics of one band of one source for every zone feature: `values[s][f]` is statistic `s`
/// (in [`ZonalOptions::stats`] order) of feature `f` (in layer order).
#[derive(Debug, Clone, PartialEq)]
pub struct BandStats {
    /// Index of the source in the slice given to [`zonal_stats_with`].
    pub source: usize,
    /// Index of the band in that source's [`ZonalSource::bands`].
    pub band: usize,
    /// One column of values per statistic.
    pub values: Vec<Vec<FieldValue>>,
}

/// Computes zonal statistics as [`zonal_stats`] does, without building a layer: each finished
/// band goes to `sink`, from any thread and in any order, as soon as its source is done. Sources
/// are read in parallel, as many at once as terrio's pool has threads, and the zones of each
/// strip are reduced in parallel on the pool. Sources without bands are not read. Output naming
/// options ([`ZonalBand::prefix`], `dst_srs`, `decimal_places`) are not used.
///
/// On errors, the one of the first failing source (in slice order) is returned; other sources may
/// already have reached `sink`.
pub fn zonal_stats_with(
    zones: &Layer,
    sources: &mut [ZonalSource],
    options: &ZonalOptions,
    sink: &(dyn Fn(BandStats) -> Result<()> + Sync),
) -> Result<()> {
    if options.fractional && options.all_touched {
        return Err(Error::InvalidArgument("fractional coverage and all_touched are mutually exclusive".into()));
    }
    if options.stats.iter().any(|s| s.uses_weights()) && options.weights.is_none() {
        return Err(Error::InvalidArgument("weighted statistics require a weighting raster".into()));
    }
    if sources.iter().all(|s| s.bands.is_empty()) || options.stats.is_empty() {
        return Err(Error::InvalidArgument("zonal statistics needs at least one raster band and one statistic".into()));
    }
    check_stats_and_zones(&options.stats, zones)?;
    let weights = options.weights.as_ref().filter(|_| options.stats.iter().any(|s| s.uses_weights()));
    let weight_header = match weights {
        Some(w) => {
            let header = w.lock()?.header();
            super::sample::check_raster(&header)?;
            super::sample::check_band(&header, w.band)?;
            Some(header)
        }
        None => None,
    };
    let mut grids: Vec<(index::GridKey, Option<index::ZoneIndex>)> = Vec::new();
    // The grid of each source; none for a source without bands, which is not read.
    let mut source_grid: Vec<Option<usize>> = Vec::new();
    let mut nodata = Vec::new();
    // Paths are opened for their headers on the pool, a few at a time, and closed again (see
    // `ZonalRaster::Path`); the warnings of their opening are recorded on the calling thread.
    let paths: Vec<Option<&str>> = sources
        .iter()
        .map(|s| match &s.raster {
            ZonalRaster::Path(path) if !s.bands.is_empty() => Some(path.as_str()),
            _ => None,
        })
        .collect();
    let mut path_headers = crate::par::map_init(
        &paths,
        || (),
        |_, path| {
            path.map(|path| {
                let header = crate::open_raster_lazy(path).map(|r| (r.header(), index::GridKey::of(&r)));
                (header, crate::warnings::take())
            })
        },
    )
    .into_iter();
    for source in sources.iter() {
        let path_header = path_headers.next().flatten();
        if source.bands.is_empty() {
            nodata.push(Vec::new());
            source_grid.push(None);
            continue;
        }
        let (header, grid) = match (&source.raster, path_header) {
            (ZonalRaster::Open(raster), _) => (raster.header(), index::GridKey::of(raster)),
            (ZonalRaster::Path(_), Some((header, warnings))) => {
                warnings.into_iter().for_each(crate::warnings::warn);
                header?
            }
            (ZonalRaster::Path(_), None) => unreachable!("paths with bands are opened above"),
        };
        super::sample::check_raster(&header)?;
        if let Some(w) = &weight_header {
            let weight_grid = index::GridKey { cols: w.raster.cols, rows: w.raster.rows, geo_transform: w.geo_transform(), crs: w.crs() };
            if !grid.same(&weight_grid) {
                return Err(Error::InvalidArgument(
                    "weighting and value rasters must have identical grids and CRS; align them before analysis".into(),
                ));
            }
        }
        for band in &source.bands {
            super::sample::check_band(&header, band.band)?;
        }
        nodata.push(source.bands.iter().map(|b| header.band_nodata(b.band)).collect::<Vec<_>>());
        source_grid.push(Some(match grids.iter().position(|(g, _)| g.same(&grid)) {
            Some(i) => i,
            None => {
                grids.push((grid, None));
                grids.len() - 1
            }
        }));
    }
    for (grid, zone_index) in &mut grids {
        *zone_index = Some(index::build(zones, grid, options.all_touched, options.fractional)?);
    }
    let fids: Vec<u64> = zones.features.iter().map(|f| f.fid).collect();
    let plan = reduce::Plan::new(&options.stats, options.fractional, &fids);
    // Strips read ahead of each source: with sources reduced on several threads, they read in
    // parallel already, and every strip held ahead is memory. Two still overlap a source's reads
    // with its reduction (one was 11% slower on 8 threads): 100 Sentinel-2 bands on 32 threads
    // peak at 3.9 GB instead of 4.7, as fast.
    #[cfg(not(target_family = "wasm"))]
    let ahead = match crate::par::threads().min(sources.len()) {
        0 | 1 => super::strips::READ_AHEAD,
        drivers => (crate::par::threads() / drivers).clamp(2, super::strips::READ_AHEAD),
    };
    #[cfg(target_family = "wasm")]
    let ahead = 1;
    // Warnings of the rasters opened on other threads, recorded again on the calling thread.
    let warnings = Mutex::new(Vec::new());
    // Bytes of zone values held by the passes running now, and their release.
    let values_held = (Mutex::new(0usize), std::sync::Condvar::new());
    // The error of the first failing source, by index. Sources after it are skipped: those before
    // it were taken first and still run, so the error reported is the same.
    let failure: Mutex<Option<(usize, Error)>> = Mutex::new(None);
    let reduce_one = |s: usize, source: &mut ZonalSource| -> Result<()> {
        let Some(grid) = source_grid[s] else { return Ok(()) };
        if failure.lock().is_ok_and(|f| f.as_ref().is_some_and(|(first, _)| *first < s)) {
            return Ok(());
        }
        let mut opened;
        let raster = match &mut source.raster {
            ZonalRaster::Open(raster) => raster,
            ZonalRaster::Path(path) => {
                opened = crate::open_raster_lazy(path)?;
                if let Ok(mut w) = warnings.lock() {
                    w.extend(crate::warnings::take());
                }
                // The zones were built for the grid its header had: a file replaced since is refused.
                if !index::GridKey::of(&opened).same(&grids[grid].0) {
                    return Err(Error::InvalidArgument(format!("{path} changed grid while zonal statistics were computed")));
                }
                &mut opened
            }
        };
        let bands: Vec<_> = source.bands.iter().zip(&nodata[s]).map(|(b, &nodata)| reduce::BandSpec { band: b.band, nodata }).collect();
        // Each source reads the weight band again (all share one grid); reduce the
        // sources of a grid together, reading each weight strip once, if weighted runs over many
        // sources are slow. That holds every source's open zones at once, so bound the group.
        let mut weight_raster = weights.map(|w| w.lock().map(|mut r| r.shared())).transpose()?;
        let zone_index = grids[grid].1.as_ref().expect("built above");
        // Statistics that keep values hold every open zone's values for each band reduced at once.
        // Bands are reduced in groups whose values take at most `VALUES_PER_PASS` (one band at a
        // time when one band's take more), so that a zone covering a whole multi-band raster holds
        // one band's values, as 0.7.0 did. Each further pass reads the raster again. Passes of all
        // sources share the budget (`hold_values`).
        // Values per band at the strip height of a pass of `n` bands (strips of fewer bands are
        // taller); the largest group that fits, or one band.
        let per_band = |n: usize| match plan.value_bytes_per_pixel() {
            0 => 0,
            bytes => reduce::open_pixels_peak(zone_index, strip_rows(raster, n)) * bytes,
        };
        let group = (1..=bands.len()).rev().find(|&n| n * per_band(n) <= VALUES_PER_PASS).unwrap_or(1);
        let per_band = per_band(group);
        let mut band = 0;
        for group in bands.chunks(group.max(1)) {
            let _held = hold_values(&values_held, per_band * group.len())?;
            let weight = match (&mut weight_raster, weights, &weight_header) {
                (Some(raster), Some(w), Some(h)) => Some(reduce::WeightSpec { raster, band: w.band, nodata: h.band_nodata(w.band) }),
                _ => None,
            };
            let strip = strip_rows(raster, group.len());
            for values in reduce::reduce_source(zone_index, raster, group, weight, &plan, strip, ahead)? {
                sink(BandStats { source: s, band, values })?;
                band += 1;
            }
        }
        Ok(())
    };
    let record = |s: usize, result: Result<()>| {
        if let Err(e) = result
            && let Ok(mut failure) = failure.lock()
            && failure.as_ref().is_none_or(|(first, _)| s < *first)
        {
            *failure = Some((s, e));
        }
    };
    // Natively, sources are reduced on threads of their own, never on the pool: a source waits
    // for the strips its readers read (`strips::read_ahead`), and decoders of those strips may
    // need the pool (JPEG XL, progressive JPEG), which waiting pool threads could leave without a
    // thread to run on. One source runs on the calling thread. In WebAssembly, strips are read on
    // the thread that reduces them, so sources share the pool.
    #[cfg(not(target_family = "wasm"))]
    {
        let drivers = crate::par::threads().min(sources.len());
        if drivers <= 1 {
            for (s, source) in sources.iter_mut().enumerate() {
                record(s, reduce_one(s, source));
                release_freed_memory();
            }
        } else {
            let queue = Mutex::new(sources.iter_mut().enumerate());
            std::thread::scope(|scope| {
                for _ in 0..drivers {
                    scope.spawn(|| {
                        while let Some((s, source)) = queue.lock().ok().and_then(|mut q| q.next()) {
                            record(s, reduce_one(s, source));
                            release_freed_memory();
                        }
                    });
                }
            });
        }
    }
    #[cfg(target_family = "wasm")]
    crate::par::chunks_mut_coarse(sources, 1, |s, chunk| record(s, reduce_one(s, &mut chunk[0])));
    for w in warnings.into_inner().unwrap_or_default() {
        crate::warnings::warn(w);
    }
    match failure.into_inner() {
        Ok(Some((_, e))) => Err(e),
        Ok(None) => Ok(()),
        Err(_) => Err(Error::InvalidArgument("zonal error lock poisoned".into())),
    }
}

/// Hands the memory freed by a reduced source back to the system. glibc keeps freed strips in the
/// arena of the thread that freed them, for that thread to reuse: with sources on many threads,
/// every arena holds its own (100 Sentinel-2 bands on 32 threads peak at 2.7 GB instead of 3.9,
/// as fast).
#[cfg(not(target_family = "wasm"))]
fn release_freed_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: `malloc_trim` only returns free pages of glibc's heaps to the system.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Zonal statistics of arrays the caller supplies strip by strip (after its own per-pixel
/// transforms, say), equal to those [`zonal_stats`] gives for a raster holding the same values:
/// zones are computed once for the grid, then each [`ZonalReducer::stream`] reduces one band.
/// Cloning shares the zones. Statistics that keep values (median, quantiles, mode, minority,
/// variety) hold each open zone's values in each stream, 8 bytes per pixel (16 with fractional
/// coverage), outside `zonal_stats`' shared budget: feed streams one at a time to bound them.
///
/// ```no_run
/// use terrio::ops::rasterize::Grid;
/// use terrio::ops::zonal::{Statistic, ZonalOptions, ZonalReducer};
/// use terrio::{crs::SpatialRef, raster::RasterData};
///
/// let zones = terrio::open_vector("fields.gpkg", &Default::default())?;
/// let grid = Grid {
///     cols: 10980,
///     rows: 10980,
///     geo_transform: [600000.0, 10.0, 0.0, 5000040.0, 0.0, -10.0],
///     crs: SpatialRef::parse("EPSG:32631")?,
/// };
/// let options = ZonalOptions { stats: vec![Statistic::Mean, Statistic::Median], ..Default::default() };
/// let reducer = ZonalReducer::new(&zones.layers[0], grid, &options, None)?;
/// let mut stream = reducer.stream(None);
/// for (i, w) in reducer.windows().iter().enumerate() {
///     let ndvi = vec![0.5f64; w.width * w.height]; // your values for window `w`
///     stream.add(i, &RasterData::F64(ndvi))?;
/// }
/// let columns = stream.finish()?; // [statistic][feature]
/// # Ok::<(), terrio::Error>(())
/// ```
#[derive(Debug, Clone)]
pub struct ZonalReducer {
    inner: Arc<Reducer>,
}

#[derive(Debug)]
struct Reducer {
    index: index::ZoneIndex,
    stats: Vec<Statistic>,
    fractional: bool,
    fids: Vec<u64>,
    windows: Vec<Window>,
}

impl Reducer {
    fn plan(&self) -> reduce::Plan<'_> {
        reduce::Plan::new(&self.stats, self.fractional, &self.fids)
    }
}

impl ZonalReducer {
    /// The zones of `zones` on `grid`, with `options`' statistics and pixel inclusion, and the
    /// [`windows`](Self::windows) to supply: strips of `strip_rows` rows from row 0 (by default
    /// terrio's strip size for one f64 band of this width; give a tiled source's block height to
    /// read each block once) that hold zone pixels, each over the rows and columns of those
    /// pixels. Zones in another CRS than the grid's are reprojected, as `zonal_stats` does.
    /// Weighted statistics are not supported, nor are `dst_srs` and `decimal_places` used.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidArgument`] for no statistics, a duplicate or weighted statistic, a
    ///   quantile outside 0-100, `fractional` with `all_touched`, `strip_rows` of 0, a grid
    ///   without pixels, over `u32::MAX` rows or columns, or with a non-finite or non-invertible
    ///   geotransform, or a zone that is not a polygon, has non-finite coordinates, or cannot be
    ///   reprojected or rasterized (such as an invalid geometry with `fractional`), naming it.
    /// - [`Error::Crs`] when only one of the zones and the grid has a CRS, or the zones' CRS
    ///   cannot be transformed into the grid's.
    pub fn new(zones: &Layer, grid: Grid, options: &ZonalOptions, strip_rows: Option<usize>) -> Result<Self> {
        if options.fractional && options.all_touched {
            return Err(Error::InvalidArgument("fractional coverage and all_touched are mutually exclusive".into()));
        }
        if options.stats.iter().any(|s| s.uses_weights()) {
            return Err(Error::InvalidArgument("weighted statistics need a weighting raster, which ZonalReducer does not take".into()));
        }
        if options.stats.is_empty() {
            return Err(Error::InvalidArgument("zonal statistics needs at least one statistic".into()));
        }
        if strip_rows == Some(0) {
            return Err(Error::InvalidArgument("strip_rows must be at least 1".into()));
        }
        check_stats_and_zones(&options.stats, zones)?;
        let Grid { cols, rows, geo_transform, crs } = grid;
        super::sample::check_grid(&geo_transform, cols, rows)?;
        let index = index::build(zones, &index::GridKey { cols, rows, geo_transform, crs }, options.all_touched, options.fractional)?;
        let fids = zones.features.iter().map(|f| f.fid).collect();
        let strip_rows = strip_rows.unwrap_or_else(|| super::strips::small_strip_rows(cols, 1, std::mem::size_of::<f64>(), None));
        let windows = reduce::strip_windows(&index, strip_rows);
        Ok(Self { inner: Arc::new(Reducer { index, stats: options.stats.clone(), fractional: options.fractional, fids, windows }) })
    }

    /// The windows to supply, in order.
    pub fn windows(&self) -> &[Window] {
        &self.inner.windows
    }

    /// The number of features of the zone layer.
    pub fn feature_count(&self) -> usize {
        self.inner.fids.len()
    }

    /// The statistics computed, in output order.
    pub fn stats(&self) -> &[Statistic] {
        &self.inner.stats
    }

    /// A reduction of one band over [`ZonalReducer::windows`]; pixels equal to `nodata`, or not
    /// finite, are left out.
    pub fn stream(&self, nodata: Option<f64>) -> ZonalStream {
        ZonalStream { next: 0, state: reduce::StreamState::new(vec![nodata], &self.inner.plan()), reducer: self.inner.clone() }
    }
}

/// One band reduced by a [`ZonalReducer`], fed its windows in order.
pub struct ZonalStream {
    reducer: Arc<Reducer>,
    next: usize,
    state: reduce::StreamState,
}

/// Prints the stream's progress, not the values it holds.
impl std::fmt::Debug for ZonalStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ZonalStream").field("next_window", &self.next).field("windows", &self.reducer.windows.len()).finish_non_exhaustive()
    }
}

impl ZonalStream {
    /// The window [`add`](Self::add) takes next: the number of windows added.
    pub fn next_window(&self) -> usize {
        self.next
    }

    /// Adds the pixels of window `window` (row-major, `width * height` of them, of any sample
    /// type). Windows must come in order: 0, 1, 2, ...
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for a window out of order or past the last, or data of the
    /// wrong length. These are checked before any pixel is added, so the stream is unchanged and
    /// can take the right window. After a panic caught by the caller, drop the stream.
    pub fn add(&mut self, window: usize, data: &RasterData) -> Result<()> {
        if window != self.next {
            return Err(Error::InvalidArgument(format!("zonal stream: got window {window}, expected window {}", self.next)));
        }
        let Some(&w) = self.reducer.windows.get(window) else {
            return Err(Error::InvalidArgument(format!(
                "zonal stream: window {window} is past the last of {}",
                self.reducer.windows.len()
            )));
        };
        // Checked: on a 32-bit target, a window of 65,536 x 65,536 pixels wraps to 0.
        if w.width.checked_mul(w.height) != Some(data.len()) {
            return Err(Error::InvalidArgument(format!(
                "zonal stream: window {window} has {} pixels, expected {} x {}",
                data.len(),
                w.width,
                w.height
            )));
        }
        let r = &self.reducer;
        self.state.add_strip(&r.index, &r.plan(), w, &reduce::Strip::of(std::borrow::Cow::Borrowed(data), w), None)?;
        self.next += 1;
        Ok(())
    }

    /// Whether every window was added: [`finish`](Self::finish) checks this first, so a caller
    /// can check it without giving up the stream.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if windows are missing.
    pub fn check_complete(&self) -> Result<()> {
        let windows = self.reducer.windows.len();
        if self.next < windows {
            return Err(Error::InvalidArgument(format!("zonal stream: missing windows, {} of {windows} added", self.next)));
        }
        Ok(())
    }

    /// `[statistic][feature]`, features in layer order.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if windows are missing ([`check_complete`](Self::check_complete)).
    pub fn finish(self) -> Result<Vec<Vec<FieldValue>>> {
        self.check_complete()?;
        Ok(self.state.finish(&self.reducer.index)?.swap_remove(0))
    }
}

#[cfg(test)]
mod tests {
    use super::{Statistic, ZonalBand, ZonalOptions, ZonalSource, hold_values, labelled_bands, zonal_stats};
    use crate::vector::Coord;
    use crate::{DataType, Feature, Geometry, Layer, LazyRaster, Raster, RasterConfig, RasterDataset};

    #[test]
    fn passes_wait_while_others_hold_the_values_budget() {
        // 64 bytes in tests: a pass of 40 waits for another of 40, one of 0 never does.
        let budget = (std::sync::Mutex::new(0usize), std::sync::Condvar::new());
        let first = hold_values(&budget, 40).unwrap();
        drop(hold_values(&budget, 0).unwrap());
        std::thread::scope(|scope| {
            let second = scope.spawn(|| {
                let _held = hold_values(&budget, 40).unwrap();
                *budget.0.lock().unwrap()
            });
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert_eq!(*budget.0.lock().unwrap(), 40);
            drop(first);
            assert_eq!(second.join().unwrap(), 40);
        });
        assert_eq!(*budget.0.lock().unwrap(), 0);
    }

    #[test]
    fn bands_reduced_in_several_passes_match_bands_reduced_alone() {
        // 3 bands of 6 x 6; a zone of 30 pixels holds 240 bytes of values per band, over the
        // 64 bytes a pass may hold in tests, so each band takes a pass of its own.
        let values: Vec<f64> = (0..3 * 36).map(|i| ((i * 37) % 11) as f64).collect();
        let cfg = RasterConfig { cols: 6, rows: 6, bands: 3, data_type: DataType::F64, ..Default::default() };
        let mut r = RasterDataset::new(Raster::from_data(cfg, values).unwrap(), false);
        r.set_geo_transform([0.0, 1.0, 0.0, 6.0, 0.0, -1.0]);
        let ring = vec![Coord::xy(0., 0.), Coord::xy(6., 0.), Coord::xy(6., 5.), Coord::xy(0., 5.), Coord::xy(0., 0.)];
        let mut zones = Layer::new("zones");
        zones.features.push(Feature { fid: 1, geometry: Some(Geometry::polygon(ring, vec![])), attributes: Vec::new() });
        let options = ZonalOptions { stats: vec![Statistic::Median, Statistic::Mode, Statistic::Mean], ..Default::default() };
        let source = |bands: &[usize]| ZonalSource {
            raster: LazyRaster::from(r.clone()).into(),
            bands: bands.iter().map(|&b| ZonalBand::new(b, format!("b{b}"))).collect(),
        };
        let together = zonal_stats(&zones, &mut [source(&[1, 2, 3])], &options).unwrap();
        let alone = zonal_stats(&zones, &mut [source(&[1]), source(&[2]), source(&[3])], &options).unwrap();
        assert_eq!(together.features[0].attributes, alone.features[0].attributes);
        assert_eq!(together.features[0].attributes.len(), 9);
    }

    #[test]
    fn time_text_names_python_and_javascript_times_alike() {
        for (iso, text) in [
            ("2024-07-01T00:00:00", "2024-07-01"),
            ("2024-07-01T00:00:00+00:00", "2024-07-01"),
            ("2024-07-01T00:00:00.000Z", "2024-07-01"),
            ("2024-07-11T06:30:00.000Z", "2024-07-11T06:30:00Z"),
            ("2024-07-11T06:30:00+00:00", "2024-07-11T06:30:00Z"),
            ("2024-07-11T06:30:00.250Z", "2024-07-11T06:30:00.250Z"),
            ("2024-07-11T06:30:00+02:00", "2024-07-11T06:30:00+02:00"),
            ("2024-07-11", "2024-07-11"),
        ] {
            assert_eq!(super::time_text(iso), text, "{iso}");
        }
        assert_eq!(labelled_bands(&[1], Some(" "), Some(""), |b| format!("s_b{b}"))[0].prefix, "s_b1");
    }

    #[test]
    fn labelled_bands_name_each_band_of_a_label_and_keep_unlabelled_names() {
        let names = |bands: &[usize], time, label| -> Vec<String> {
            labelled_bands(bands, time, label, |b| format!("s_b{b}")).into_iter().map(|b| b.prefix).collect()
        };
        assert_eq!(names(&[4], Some("2024-07-01"), Some("B04")), ["2024_07_01_B04"]);
        assert_eq!(names(&[1, 2], Some("2024-07-01"), Some("B04")), ["2024_07_01_B04_b1", "2024_07_01_B04_b2"]);
        assert_eq!(names(&[1, 2], Some("2024-07-01"), None), ["2024_07_01_b1", "2024_07_01_b2"]);
        assert_eq!(names(&[1, 2], None, None), ["s_b1", "s_b2"]);
        let bands = labelled_bands(&[1, 2], None, Some("B04"), |_| unreachable!());
        assert_eq!(bands.iter().map(|b| b.label.as_deref().unwrap()).collect::<Vec<_>>(), ["B04_b1", "B04_b2"]);
    }
}

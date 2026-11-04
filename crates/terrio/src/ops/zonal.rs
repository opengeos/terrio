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

use crate::crs::SpatialRef;
use crate::{Error, FieldDef, FieldType, FieldValue, Geometry, Layer, LazyRaster, Result};
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

/// The bands of one labelled input (a CLI `--inputs` row, a Python dict, a JavaScript object):
/// each of `bands`, its fields named by [`label_prefix`] when the input has a time or a label,
/// else `unlabelled(band)`. A label given to several bands is suffixed with each band's number
/// (`B04_b1`, `B04_b2`), so that their fields say which band they hold.
pub fn labelled_bands(bands: &[usize], time: Option<&str>, label: Option<&str>, unlabelled: impl Fn(usize) -> String) -> Vec<ZonalBand> {
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
    if sources.iter().all(|s| s.bands.is_empty()) || options.stats.is_empty() {
        return Err(Error::InvalidArgument("zonal statistics needs at least one raster band and one statistic".into()));
    }
    for (i, stat) in options.stats.iter().enumerate() {
        if options.stats[..i].contains(stat) {
            return Err(Error::InvalidArgument(format!("duplicate statistic '{}'", stat.name())));
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
    let mut grids: Vec<(index::GridKey, Option<index::ZoneIndex>)> = Vec::new();
    // The grid of each source; none for a source without bands, which is not read.
    let mut source_grid: Vec<Option<usize>> = Vec::new();
    let mut nodata = Vec::new();
    for source in sources.iter() {
        // A path is opened for its header and closed again (see `ZonalRaster::Path`).
        let opened;
        let raster = match &source.raster {
            ZonalRaster::Open(raster) => raster,
            ZonalRaster::Path(path) => {
                opened = crate::open_raster_lazy(path)?;
                &opened
            }
        };
        let header = raster.header();
        super::sample::check_raster(&header)?;
        let grid = index::GridKey::of(raster);
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
        source_grid.push(if source.bands.is_empty() {
            None
        } else {
            Some(match grids.iter().position(|(g, _)| g.same(&grid)) {
                Some(i) => i,
                None => {
                    grids.push((grid, None));
                    grids.len() - 1
                }
            })
        });
    }
    for (grid, zone_index) in &mut grids {
        *zone_index = Some(index::build(zones, grid, options.all_touched, options.fractional)?);
    }
    let fids: Vec<u64> = zones.features.iter().map(|f| f.fid).collect();
    let plan = reduce::Plan::new(&options.stats, options.fractional, &fids);
    // Warnings of the rasters opened on other threads, recorded again on the calling thread.
    let warnings = Mutex::new(Vec::new());
    let reduce_one = |s: usize, source: &mut ZonalSource| -> Result<()> {
        let Some(grid) = source_grid[s] else { return Ok(()) };
        let mut opened;
        let raster = match &mut source.raster {
            ZonalRaster::Open(raster) => raster,
            ZonalRaster::Path(path) => {
                opened = crate::open_raster_lazy(path)?;
                if let Ok(mut w) = warnings.lock() {
                    w.extend(crate::warnings::take());
                }
                &mut opened
            }
        };
        let bands: Vec<_> = source.bands.iter().zip(&nodata[s]).map(|(b, &nodata)| reduce::BandSpec { band: b.band, nodata }).collect();
        // ponytail: each source reads the weight band again (all share one grid); reduce the
        // sources of a grid together, reading each weight strip once, if weighted runs over many
        // sources are slow. That holds every source's open zones at once, so bound the group.
        let mut weight_raster = weights.map(|w| w.lock().map(|mut r| r.shared())).transpose()?;
        let weight = match (&mut weight_raster, weights, &weight_header) {
            (Some(raster), Some(w), Some(h)) => Some(reduce::WeightSpec { raster, band: w.band, nodata: h.band_nodata(w.band) }),
            _ => None,
        };
        let zone_index = grids[grid].1.as_ref().expect("built above");
        let strip = strip_rows(raster, bands.len());
        let values = reduce::reduce_source(zone_index, raster, &bands, weight, &plan, strip)?;
        for (band, values) in values.into_iter().enumerate() {
            sink(BandStats { source: s, band, values })?;
        }
        Ok(())
    };
    // The error of the first failing source, by index.
    let failure: Mutex<Option<(usize, Error)>> = Mutex::new(None);
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
            }
        } else {
            let queue = Mutex::new(sources.iter_mut().enumerate());
            std::thread::scope(|scope| {
                for _ in 0..drivers {
                    scope.spawn(|| {
                        while let Some((s, source)) = queue.lock().ok().and_then(|mut q| q.next()) {
                            record(s, reduce_one(s, source));
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

#[cfg(test)]
mod tests {
    use super::labelled_bands;

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

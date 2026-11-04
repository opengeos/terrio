//! Reduction of raster bands over a zone index, strip by strip. Each zone's pixels are visited in
//! row-major order whatever the strip height, so results do not depend on it.

use super::Statistic;
use super::index::ZoneIndex;
use crate::{DataType, Error, FieldValue, LazyRaster, RasterDataset, Result, Window, WindowOptions};

/// A band to reduce: its number (from 1) and nodata value.
pub(super) struct BandSpec {
    pub band: usize,
    pub nodata: Option<f64>,
}

/// The weighting raster of a source, read alongside it.
pub(super) struct WeightSpec<'a> {
    pub raster: &'a mut LazyRaster,
    pub band: usize,
    pub nodata: Option<f64>,
}

/// What every reduction computes, with the questions the per-pixel loop asks answered once.
pub(super) struct Plan<'a> {
    stats: &'a [Statistic],
    fractional: bool,
    /// Feature IDs in layer order, for error messages.
    fids: &'a [u64],
    /// Whether some statistic needs the zone's values sorted.
    keep_values: bool,
    /// Whether some statistic needs the variance (a running mean and M2 per pixel).
    moments: bool,
    weighted_sum: bool,
    weighted_mean: bool,
    weighted_stddev: bool,
}

impl<'a> Plan<'a> {
    pub fn new(stats: &'a [Statistic], fractional: bool, fids: &'a [u64]) -> Self {
        Self {
            stats,
            fractional,
            fids,
            keep_values: stats.iter().any(|s| s.needs_values()),
            moments: stats.iter().any(|s| matches!(s, Statistic::StdDev | Statistic::Variance | Statistic::Cv)),
            weighted_sum: stats.contains(&Statistic::WeightedSum),
            weighted_mean: stats.contains(&Statistic::WeightedMean),
            weighted_stddev: stats.contains(&Statistic::WeightedStdDev),
        }
    }

    fn empty(&self) -> Vec<FieldValue> {
        let blank = Accumulator::default();
        self.stats.iter().map(|&s| blank.stat(s, self, &[])).collect()
    }
}

/// The windows read for `index` in strips of `strip_rows` rows from row 0: each strip holding
/// zone pixels, over the rows and columns of the pixels in it. Strips without zone pixels, such
/// as those between the parts of a multipolygon, are left out.
pub(super) fn strip_windows(index: &ZoneIndex, rows: usize, strip_rows: usize) -> Vec<Window> {
    // Per strip: (first row, end row, first column, end column) of its zone pixels.
    let mut extents: Vec<Option<(usize, usize, usize, usize)>> = vec![None; rows.div_ceil(strip_rows)];
    for span in index.zones.iter().flat_map(|z| &z.spans) {
        let (row, col, end) = (span.row as usize, span.col as usize, (span.col + span.len) as usize);
        let e = extents[row / strip_rows].get_or_insert((row, row + 1, col, end));
        *e = (e.0.min(row), e.1.max(row + 1), e.2.min(col), e.3.max(end));
    }
    extents.into_iter().flatten().map(|(r0, r1, c0, c1)| Window::new(c0, r0, c1 - c0, r1 - r0)).collect()
}

/// Statistics of each band (in `bands` order) for each statistic (in plan order) and feature
/// (in layer order). Strips are `strip_rows` rows starting at multiples of it; strips that no
/// zone intersects are not read. The next strips are read on other threads while one is reduced
/// (`strips::read_ahead`).
pub(super) fn reduce_source(
    index: &ZoneIndex,
    raster: &mut LazyRaster,
    bands: &[BandSpec],
    weight: Option<WeightSpec<'_>>,
    plan: &Plan<'_>,
    strip_rows: usize,
) -> Result<Vec<Vec<Vec<FieldValue>>>> {
    let features = plan.fids.len();
    let empty = plan.empty();
    let mut out: Vec<Vec<Vec<FieldValue>>> = bands.iter().map(|_| empty.iter().map(|v| vec![v.clone(); features]).collect()).collect();
    let zones = &index.zones;
    let windows = strip_windows(index, raster.height(), strip_rows.max(1));
    let read_bands = WindowOptions { bands: bands.iter().map(|b| b.band).collect(), overview: None };
    // Reads share one handle each: reads of a raster take turns, but overlap the reduction.
    let raster = std::sync::Mutex::new(raster);
    let weight_band = weight.as_ref().map(|w| (w.band, w.nodata));
    let weight_raster = std::sync::Mutex::new(weight.map(|w| w.raster));
    let lock = |m| Error::InvalidArgument(format!("zonal {m} reader lock poisoned"));
    // Open zones: (zone, next span, one accumulator per band).
    let mut open: Vec<(usize, usize, Vec<Accumulator>)> = Vec::new();
    let mut next = 0;
    crate::ops::strips::read_ahead(
        windows.len(),
        |i| {
            let window = windows[i];
            let strip = Strip::read(&mut **raster.lock().map_err(|_| lock("raster"))?, window, &read_bands)?;
            let weights = match (weight_band, &mut *weight_raster.lock().map_err(|_| lock("weight"))?) {
                (Some((band, nodata)), Some(w)) => {
                    Some((Strip::read(w, window, &WindowOptions { bands: vec![band], overview: None })?, nodata))
                }
                _ => None,
            };
            Ok((strip, weights))
        },
        |i, (strip, weights)| {
            let Window { x: c0, y: r0, height, .. } = windows[i];
            let r1 = r0 + height;
            while next < zones.len() && zones[next].row_start < r1 {
                // Value lists hold at most the zone's pixels: reserved once.
                let pixels = if plan.keep_values { zones[next].spans.iter().map(|s| s.len as usize).sum() } else { 0 };
                let accumulator = || Accumulator {
                    values: Vec::with_capacity(pixels),
                    coverages: Vec::with_capacity(if plan.fractional { pixels } else { 0 }),
                    ..Default::default()
                };
                open.push((next, 0, bands.iter().map(|_| accumulator()).collect()));
                next += 1;
            }
            // Zones accumulate independently: in parallel, keeping the error of the first zone.
            let failure = std::sync::Mutex::new(None::<(usize, Error)>);
            let (strip_ref, weights_ref) = (&strip, &weights);
            let chunk = crate::par::chunk_len(open.len(), 8);
            crate::par::chunks_mut(&mut open, chunk, |_, chunk| {
                for (zone, cursor, accumulators) in chunk {
                    let spans = &zones[*zone].spans;
                    let fid = plan.fids[zones[*zone].feature];
                    let mut run = || -> Result<()> {
                        while let Some(span) = spans.get(*cursor).filter(|s| (s.row as usize) < r1) {
                            // Band by band, the span's pixels in a loop of their sample type: a
                            // band's pixels are still added in row-major order.
                            let start = (span.row as usize - r0) * strip_ref.cols + span.col as usize - c0;
                            let (len, coverage, data_type) = (span.len as usize, span.coverage, strip_ref.data_type);
                            for (b, (acc, spec)) in accumulators.iter_mut().zip(bands).enumerate() {
                                match weights_ref {
                                    // Nothing to fail on without weights.
                                    None => strip_ref.each(b, start, len, |_, v| {
                                        acc.add_value(v, spec.nodata, data_type, coverage, plan);
                                        Ok(())
                                    })?,
                                    Some((w, nodata)) => strip_ref.each(b, start, len, |i, v| {
                                        let w = (w.value(0, start + i), *nodata, w.data_type);
                                        acc.add(v, spec.nodata, data_type, coverage, w, plan, fid)
                                    })?,
                                }
                            }
                            *cursor += 1;
                        }
                        Ok(())
                    };
                    if let Err(e) = run() {
                        if let Ok(mut failure) = failure.lock()
                            && failure.as_ref().is_none_or(|(first, _)| *zone < *first)
                        {
                            *failure = Some((*zone, e));
                        }
                        return;
                    }
                }
            });
            if let Ok(Some((_, e))) = failure.into_inner() {
                return Err(e);
            }
            // Finished zones: sorted and summarized in parallel, then written.
            let (finished, still_open): (Vec<_>, Vec<_>) = std::mem::take(&mut open).into_iter().partition(|o| zones[o.0].row_end <= r1);
            open = still_open;
            let mut jobs: Vec<_> = finished.into_iter().map(|o| (o, Vec::new())).collect();
            let chunk = crate::par::chunk_len(jobs.len(), 4);
            crate::par::chunks_mut(&mut jobs, chunk, |_, chunk| {
                for ((_, _, accumulators), values) in chunk {
                    *values = accumulators
                        .iter_mut()
                        .map(|acc| {
                            let sorted = if plan.keep_values { acc.sorted(plan.fractional) } else { Vec::new() };
                            plan.stats.iter().map(|&stat| acc.stat(stat, plan, &sorted)).collect::<Vec<_>>()
                        })
                        .collect();
                }
            });
            for ((zone, _, _), values) in jobs {
                let feature = zones[zone].feature;
                for (b, band_values) in values.into_iter().enumerate() {
                    for (s, value) in band_values.into_iter().enumerate() {
                        out[b][s][feature] = value;
                    }
                }
            }
            Ok(())
        },
    )?;
    Ok(out)
}

/// A window of some bands, band after band.
struct Strip {
    data: RasterDataset,
    cols: usize,
    pixels: usize,
    data_type: DataType,
}

impl Strip {
    fn read(raster: &mut LazyRaster, window: Window, options: &WindowOptions) -> Result<Self> {
        let data = raster.read_window(window, options)?;
        let data_type = data.raster.data_type;
        Ok(Self { data, cols: window.width, pixels: window.width * window.height, data_type })
    }

    fn value(&self, band: usize, at: usize) -> f64 {
        self.data.raster.data.get_f64(band * self.pixels + at)
    }

    /// Calls `f` with the offset from `at` and the value of each of `len` pixels of `band` from
    /// `at`: one loop per sample type, into which `f` inlines. Reading values one by one
    /// (`value`, a call and a type dispatch each) made the reduction about five times slower.
    #[inline]
    fn each(&self, band: usize, at: usize, len: usize, mut f: impl FnMut(usize, f64) -> Result<()>) -> Result<()> {
        use crate::raster::RasterData as D;
        let from = band * self.pixels + at;
        macro_rules! each {
            ($($variant:ident),*) => {
                match &self.data.raster.data {
                    $(D::$variant(v) => {
                        for (i, &x) in v[from..from + len].iter().enumerate() {
                            f(i, x as f64)?;
                        }
                    })*
                }
            };
        }
        each!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64);
        Ok(())
    }
}

/// One zone's running statistics for one band.
#[derive(Default)]
struct Accumulator {
    summary: Summary,
    weighted: Summary,
    invalid_weight: bool,
    /// Values, kept when a statistic needs them sorted.
    values: Vec<f64>,
    /// Their coverages, in fractional mode (binary coverage is 1).
    coverages: Vec<f64>,
}

impl Accumulator {
    #[allow(clippy::too_many_arguments)]
    #[inline]
    fn add(
        &mut self,
        v: f64,
        nodata: Option<f64>,
        data_type: DataType,
        coverage: f64,
        (w, w_nodata, w_type): (f64, Option<f64>, DataType),
        plan: &Plan<'_>,
        fid: u64,
    ) -> Result<()> {
        if !self.add_value(v, nodata, data_type, coverage, plan) {
            return Ok(());
        }
        if !w.is_finite() || !crate::histogram::is_valid(w, w_nodata, w_type) {
            self.invalid_weight = true;
        } else if w < 0.0 {
            return Err(Error::InvalidArgument(format!("zone {fid} contains a negative weight")));
        } else if w > 0.0 {
            let contribution = coverage * w;
            if contribution > 0.0 {
                self.weighted.add(v, contribution, true);
                let m2 = plan.weighted_stddev;
                if (plan.weighted_sum && !self.weighted.sum.is_finite())
                    || ((m2 || plan.weighted_mean) && (!self.weighted.weight.is_finite() || !self.weighted.mean.is_finite()))
                    || (m2 && !self.weighted.m2.is_finite())
                {
                    return Err(Error::InvalidArgument(format!("zone {fid} weighted statistics overflow binary64 arithmetic")));
                }
            }
        }
        Ok(())
    }

    /// Adds a value without weight, unless it is nodata or not finite: whether it counts.
    #[inline]
    fn add_value(&mut self, v: f64, nodata: Option<f64>, data_type: DataType, coverage: f64, plan: &Plan<'_>) -> bool {
        if !(v.is_finite() && crate::histogram::is_valid(v, nodata, data_type)) {
            return false;
        }
        self.summary.add(v, coverage, plan.moments);
        if plan.keep_values {
            self.values.push(v);
            if plan.fractional {
                self.coverages.push(coverage);
            }
        }
        true
    }

    /// The kept values with their coverages, sorted by value; the lists are emptied. Equal values
    /// are interchangeable for every statistic, so an unstable sort suffices.
    fn sorted(&mut self, fractional: bool) -> Vec<(f64, f64)> {
        let values = std::mem::take(&mut self.values);
        if fractional {
            let mut pairs: Vec<(f64, f64)> = values.into_iter().zip(std::mem::take(&mut self.coverages)).collect();
            pairs.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
            pairs
        } else {
            let mut values = values;
            values.sort_unstable_by(f64::total_cmp);
            values.into_iter().map(|v| (v, 1.0)).collect()
        }
    }

    /// `sorted` is [`Accumulator::sorted`] when the statistic needs values.
    fn stat(&self, stat: Statistic, plan: &Plan<'_>, sorted: &[(f64, f64)]) -> FieldValue {
        if stat.uses_weights() {
            return if self.invalid_weight || self.summary.count == 0 {
                FieldValue::Null
            } else if stat == Statistic::WeightedSum {
                FieldValue::Float(self.weighted.sum)
            } else if self.weighted.weight == 0.0 {
                FieldValue::Null
            } else {
                FieldValue::Float(if stat == Statistic::WeightedMean {
                    self.weighted.mean
                } else {
                    (self.weighted.m2 / self.weighted.weight).max(0.0).sqrt()
                })
            };
        }
        self.summary.stat(stat, sorted, plan.fractional)
    }
}

#[derive(Default)]
struct Summary {
    count: usize,
    weight: f64,
    sum: f64,
    mean: f64,
    m2: f64,
    min: f64,
    max: f64,
}

impl Summary {
    /// Adds `value` with `weight`; with `moments`, also to the running mean and M2 (Welford's
    /// algorithm) that the variance needs. Without, no division depends on the previous pixel's:
    /// the mean is the sum over the weight, as exactextract computes it.
    #[inline]
    fn add(&mut self, value: f64, weight: f64, moments: bool) {
        if self.count == 0 {
            self.min = value;
            self.max = value;
        }
        self.count += 1;
        self.weight += weight;
        self.sum += value * weight;
        if moments {
            let delta = value - self.mean;
            // Preserve the existing unweighted delta/count arithmetic when each pixel has weight one.
            self.mean += if weight == 1.0 { delta / self.weight } else { (weight / self.weight) * delta };
            self.m2 += weight * (delta * (value - self.mean));
        }
        self.min = self.min.min(value);
        self.max = self.max.max(value);
    }

    fn stat(&self, stat: Statistic, sorted: &[(f64, f64)], fractional: bool) -> FieldValue {
        match stat {
            Statistic::Count => {
                return if fractional { FieldValue::Float(self.weight) } else { FieldValue::Integer(self.count as i64) };
            }
            Statistic::Variety => return FieldValue::Integer(bins(sorted).count() as i64),
            _ if self.count == 0 => return FieldValue::Null,
            _ => {}
        }
        let variance = (self.m2 / self.weight).max(0.0);
        let mean = self.sum / self.weight;
        FieldValue::Float(match stat {
            Statistic::Min => self.min,
            Statistic::Max => self.max,
            Statistic::Mean => mean,
            Statistic::Sum => self.sum,
            Statistic::StdDev => variance.sqrt(),
            Statistic::Variance => variance,
            Statistic::Cv if mean == 0.0 => return FieldValue::Null,
            Statistic::Cv => variance.sqrt() / mean,
            Statistic::Median => quantile(sorted, 0.5, fractional),
            Statistic::Quantile(p) => quantile(sorted, p / 100.0, fractional),
            // Bins ascend by value: `>=` lets a later (larger) value win a mode tie, `<` keeps the
            // earlier (smaller) value on a minority tie, as exactextract 0.3.0 does.
            Statistic::Mode => bins(sorted).fold((f64::NAN, f64::NEG_INFINITY), |b, (v, w)| if w >= b.1 { (v, w) } else { b }).0,
            Statistic::Minority => bins(sorted).fold((f64::NAN, f64::INFINITY), |b, (v, w)| if w < b.1 { (v, w) } else { b }).0,
            Statistic::Count | Statistic::Variety | Statistic::WeightedMean | Statistic::WeightedSum | Statistic::WeightedStdDev => {
                unreachable!()
            }
        })
    }
}

/// Equal values of a sorted `(value, coverage)` list merged, with their coverage summed.
fn bins(sorted: &[(f64, f64)]) -> impl Iterator<Item = (f64, f64)> + '_ {
    sorted.chunk_by(|a, b| a.0 == b.0).map(|run| (run[0].0, run.iter().map(|p| p.1).sum()))
}

/// Quantile `q` (0 to 1) of a sorted non-empty list: exactextract's weighted quantile in
/// fractional mode, else linear interpolation between the closest ranks (numpy's default), with
/// a halfway point taken by `f64::midpoint` so the 0.5 quantile is the median of two middle values.
fn quantile(sorted: &[(f64, f64)], q: f64, fractional: bool) -> f64 {
    if fractional {
        return super::fractional::quantile(sorted, q);
    }
    let h = (sorted.len() - 1) as f64 * q;
    let lo = h.floor() as usize;
    let fraction = h - lo as f64;
    if fraction == 0.0 {
        sorted[lo].0
    } else if fraction == 0.5 {
        sorted[lo].0.midpoint(sorted[lo + 1].0)
    } else {
        let (a, b) = (sorted[lo].0, sorted[lo + 1].0);
        // Values of opposite signs are weighted apart: their difference can overflow.
        if a.is_sign_negative() == b.is_sign_negative() { a + fraction * (b - a) } else { a * (1.0 - fraction) + b * fraction }
    }
}

#[cfg(test)]
mod tests {
    use super::super::index::{GridKey, build};
    use super::{BandSpec, Plan, reduce_source};
    use crate::ops::zonal::Statistic;
    use crate::vector::Coord;
    use crate::{DataType, Feature, Geometry, Layer, LazyRaster, Raster, RasterConfig, RasterDataset};

    fn ring(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<Coord> {
        vec![Coord::xy(x0, y0), Coord::xy(x1, y0), Coord::xy(x1, y1), Coord::xy(x0, y1), Coord::xy(x0, y0)]
    }

    #[test]
    fn strip_windows_cover_zone_pixels_and_skip_strips_without_them() {
        use super::super::index::{Span, Zone, ZoneIndex};
        use super::strip_windows;
        use crate::Window;
        // `rows` rows of `len` pixels from (`row`, `col`).
        let block = |row: u32, rows: u32, col, len| (row..row + rows).map(move |row| Span { row, col, len, coverage: 1.0 });
        let zone = |spans: Vec<Span>| Zone { feature: 0, row_start: 0, row_end: 0, spans };
        // A multipart zone's parts, far apart: the strips between them are not read.
        let parts = block(300, 10, 1, 2).chain(block(1500, 10, 9, 31)).chain(block(1599, 1, 0, 1)).collect();
        let index = ZoneIndex { zones: vec![zone(block(0, 10, 5, 3).collect()), zone(parts)] };
        assert_eq!(
            strip_windows(&index, 1600, 256),
            [Window::new(5, 0, 3, 10), Window::new(1, 300, 2, 10), Window::new(9, 1500, 31, 10), Window::new(0, 1599, 1, 1)]
        );
        assert!(strip_windows(&ZoneIndex { zones: vec![] }, 1600, 256).is_empty());
    }

    #[test]
    fn binary_quantiles_of_extreme_values_of_opposite_signs_stay_finite() {
        let sorted = [(-f64::MAX, 1.0), (f64::MAX, 1.0)];
        let q25 = super::quantile(&sorted, 0.25, false);
        assert!((q25 / (-f64::MAX / 2.0) - 1.0).abs() < 1e-15, "{q25}");
        assert_eq!(super::quantile(&sorted, 0.5, false), 0.0);
        assert_eq!(super::quantile(&[(1.0, 1.0), (3.0, 1.0)], 0.25, false), 1.5);
    }

    #[test]
    fn zonal_mean_is_the_sum_over_the_count_whatever_else_is_requested() {
        let (cols, rows) = (37, 23);
        let values: Vec<f64> = (0..cols * rows).map(|i| 1e6 + ((i * 7919) % 101) as f64 / 7.).collect();
        let sum: f64 = values.iter().sum();
        let cfg = RasterConfig { cols, rows, data_type: DataType::F64, ..Default::default() };
        let mut ds = RasterDataset::new(Raster::from_data(cfg, values).unwrap(), false);
        ds.set_geo_transform([0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]);
        let mut layer = Layer::new("zones");
        layer.features.push(Feature {
            fid: 0,
            geometry: Some(Geometry::polygon(ring(0., 0., cols as f64, rows as f64), vec![])),
            attributes: vec![],
        });
        let mut raster = LazyRaster::from(ds);
        let index = build(&layer, &GridKey::of(&raster), false, false).unwrap();
        let mut mean = |names: &[&str]| {
            let stats: Vec<Statistic> = names.iter().map(|s| Statistic::parse(s).unwrap()).collect();
            let plan = Plan::new(&stats, false, &[0]);
            reduce_source(&index, &mut raster, &[BandSpec { band: 1, nodata: None }], None, &plan, rows).unwrap()[0][0][0].clone()
        };
        let expected = crate::FieldValue::Float(sum / (cols * rows) as f64);
        assert_eq!(mean(&["mean"]), expected);
        assert_eq!(mean(&["mean", "stddev", "cv"]), expected);
    }

    #[test]
    fn zonal_strip_height_does_not_change_results() {
        let (cols, rows) = (40, 300);
        let values: Vec<f64> = (0..2 * cols * rows).map(|i| ((i * 7919) % 97) as f64 / 3.).collect();
        let cfg = RasterConfig { cols, rows, bands: 2, data_type: DataType::F64, ..Default::default() };
        let mut ds = RasterDataset::new(Raster::from_data(cfg, values).unwrap(), false);
        ds.set_geo_transform([0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]);
        let mut layer = Layer::new("zones");
        let geometries = [
            Geometry::polygon(ring(2.3, 5.1, 9.7, 290.4), vec![]),
            Geometry::polygon(ring(10., 10., 35., 200.), vec![ring(15., 50., 30., 120.)]),
            Geometry::multi_polygon(vec![(ring(0., 0., 4.5, 3.5), vec![]), (ring(20.2, 250., 39.9, 299.9), vec![])]),
        ];
        for (i, g) in geometries.into_iter().enumerate() {
            layer.features.push(Feature { fid: i as u64, geometry: Some(g), attributes: vec![] });
        }
        let stats: Vec<Statistic> =
            ["count", "mean", "stddev", "median", "quantile_90", "mode", "variety"].iter().map(|s| Statistic::parse(s).unwrap()).collect();
        let fids = [0, 1, 2];
        for fractional in [false, true] {
            let mut raster = LazyRaster::from(ds.clone());
            let index = build(&layer, &GridKey::of(&raster), false, fractional).unwrap();
            let plan = Plan::new(&stats, fractional, &fids);
            let bands = || vec![BandSpec { band: 1, nodata: None }, BandSpec { band: 2, nodata: None }];
            let whole = reduce_source(&index, &mut raster, &bands(), None, &plan, rows).unwrap();
            assert!(whole[0][0].iter().all(|v| !v.is_null()));
            for strip in [1, 7, 64] {
                assert_eq!(reduce_source(&index, &mut raster, &bands(), None, &plan, strip).unwrap(), whole, "strip {strip}");
            }
        }
    }
}

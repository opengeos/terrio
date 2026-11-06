//! Reduction of raster bands over a zone index, strip by strip. Each zone's pixels are visited in
//! row-major order whatever the strip height, so results do not depend on it.

use super::Statistic;
use super::index::{Zone, ZoneIndex};
use crate::raster::RasterData;
use crate::{DataType, Error, FieldValue, LazyRaster, Result, Window, WindowOptions};

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
    /// Whether some statistic needs the values' ranks: a median or quantile in binary mode.
    ranks: bool,
    /// Whether some statistic needs the variance (a running mean and M2 per pixel).
    moments: bool,
    weighted_sum: bool,
    weighted_mean: bool,
    weighted_stddev: bool,
}

impl<'a> Plan<'a> {
    /// Bytes kept per pixel and band until a zone finishes: its value, and its coverage in
    /// fractional mode; none when no statistic keeps values.
    pub fn value_bytes_per_pixel(&self) -> usize {
        if self.keep_values { 8 * (1 + usize::from(self.fractional)) } else { 0 }
    }

    pub fn new(stats: &'a [Statistic], fractional: bool, fids: &'a [u64]) -> Self {
        Self {
            stats,
            fractional,
            fids,
            keep_values: stats.iter().any(|s| s.needs_values()),
            ranks: !fractional && stats.iter().any(|s| matches!(s, Statistic::Median | Statistic::Quantile(_))),
            moments: stats.iter().any(|s| matches!(s, Statistic::StdDev | Statistic::Variance | Statistic::Cv)),
            weighted_sum: stats.contains(&Statistic::WeightedSum),
            weighted_mean: stats.contains(&Statistic::WeightedMean),
            weighted_stddev: stats.contains(&Statistic::WeightedStdDev),
        }
    }

    fn empty(&self) -> Vec<FieldValue> {
        let blank = Accumulator::default();
        self.stats.iter().map(|&s| blank.stat(s, self, &[], &[])).collect()
    }
}

/// The strips `(first, last)` that the zones of `index` cross in strips of `strip_rows` rows;
/// `None` without zones. A slot per strip of that range (as `zonal_stats`' strips of many rows
/// usually take) needs no sort; zones far apart for their strip height (a caller's strips of one
/// row, say) take less room as a list of (zone, strip) pairs.
fn zone_strips(index: &ZoneIndex, strip_rows: usize) -> Option<(usize, usize)> {
    let first = index.zones.first()?.row_start / strip_rows;
    let last = index.zones.iter().map(|z| (z.row_end - 1) / strip_rows).max()?;
    Some((first, last))
}

/// The windows read for `index` in strips of `strip_rows` rows from row 0: each strip holding
/// zone pixels, over the rows and columns of the pixels in it. Strips without zone pixels, such
/// as those between the parts of a multipolygon, are left out.
pub(super) fn strip_windows(index: &ZoneIndex, strip_rows: usize) -> Vec<Window> {
    let window = |(r0, r1, c0, c1): (usize, usize, usize, usize)| Window::new(c0, r0, c1 - c0, r1 - r0);
    // A slot per strip, when there are no more strips than (zone, strip) pairs (40 bytes each):
    // the first row, end row, first column and end column of its zone pixels.
    // (zone, strip) pairs holding zone pixels: none between the far parts of a multipart zone.
    let occupied = index
        .zones
        .iter()
        .map(|z| 1 + z.spans.windows(2).filter(|p| p[0].row as usize / strip_rows != p[1].row as usize / strip_rows).count())
        .fold(0, usize::saturating_add);
    if let Some((first, last)) = zone_strips(index, strip_rows)
        && last - first < occupied
    {
        let mut slots: Vec<Option<(usize, usize, usize, usize)>> = vec![None; last - first + 1];
        for span in index.zones.iter().flat_map(|z| &z.spans) {
            let (row, col, end) = (span.row as usize, span.col as usize, (span.col + span.len) as usize);
            let e = slots[row / strip_rows - first].get_or_insert((row, row + 1, col, end));
            *e = (e.0.min(row), e.1.max(row + 1), e.2.min(col), e.3.max(end));
        }
        return slots.into_iter().flatten().map(window).collect();
    }
    // Else (strip, first row, end row, first column, end column) per (zone, strip) pair, merged by
    // strip.
    let mut parts: Vec<(usize, usize, usize, usize, usize)> = Vec::with_capacity(occupied);
    for zone in &index.zones {
        let zone_parts = parts.len();
        for span in &zone.spans {
            let (row, col, end) = (span.row as usize, span.col as usize, (span.col + span.len) as usize);
            match parts[zone_parts..].last_mut() {
                // A zone's spans are in row-major order: its strips come one after another.
                Some(p) if p.0 == row / strip_rows => *p = (p.0, p.1.min(row), p.2.max(row + 1), p.3.min(col), p.4.max(end)),
                _ => parts.push((row / strip_rows, row, row + 1, col, end)),
            }
        }
    }
    parts.sort_unstable_by_key(|p| p.0);
    parts
        .chunk_by(|a, b| a.0 == b.0)
        .map(|strip| {
            window(strip.iter().fold((usize::MAX, 0, usize::MAX, 0), |e, p| (e.0.min(p.1), e.1.max(p.2), e.2.min(p.3), e.3.max(p.4))))
        })
        .collect()
}

/// The most pixels of the zones open at once while strips of `strip_rows` rows are reduced: what
/// each band's value lists hold at their peak, as `reduce_source` opens a zone at the strip of its
/// first row and finishes it after the strip of its last. A zone counts at most `per_zone` pixels
/// ([`values_per_zone`]): a zone of at least `per_zone` pixels counts its values in a histogram of
/// `per_zone` bins.
pub(super) fn open_pixels_peak(index: &ZoneIndex, strip_rows: usize, per_zone: usize) -> usize {
    let strip_rows = strip_rows.max(1);
    let pixels = |zone: &Zone| zone.pixels().min(per_zone as u64) as i64;
    // The most open pixels, from the change in open pixels at each strip where it changes.
    let peak = |changes: &mut dyn Iterator<Item = i64>| {
        let mut open = 0i64;
        changes
            .map(|c| {
                open += c;
                open
            })
            .max()
            .unwrap_or(0)
            .max(0) as usize
    };
    match zone_strips(index, strip_rows) {
        // A slot per strip (8 bytes), when the slots take no more room than two (strip, change)
        // pairs per zone (32 bytes a zone).
        Some((first, last)) if last - first + 2 <= 4 * index.zones.len() => {
            let mut change = vec![0i64; last - first + 2];
            for zone in &index.zones {
                let pixels = pixels(zone);
                change[zone.row_start / strip_rows - first] += pixels;
                change[(zone.row_end - 1) / strip_rows + 1 - first] -= pixels;
            }
            peak(&mut change.into_iter())
        }
        // Else the pairs, sorted and summed by strip.
        _ => {
            let mut pairs: Vec<(usize, i64)> = Vec::with_capacity(2 * index.zones.len());
            for zone in &index.zones {
                let pixels = pixels(zone);
                pairs.extend([(zone.row_start / strip_rows, pixels), ((zone.row_end - 1) / strip_rows + 1, -pixels)]);
            }
            pairs.sort_unstable_by_key(|c| c.0);
            peak(&mut pairs.chunk_by(|a, b| a.0 == b.0).map(|strip| strip.iter().map(|c| c.1).sum()))
        }
    }
}

/// Statistics of each band (in `bands` order) for each statistic (in plan order) and feature
/// (in layer order). Strips are `strip_rows` rows starting at multiples of it; strips that no
/// zone intersects are not read. The next strips are read on other threads while one is reduced
/// (`strips::read_ahead`).
///
/// With `histograms`, zones of an 8- or 16-bit raster count their values in histograms of the
/// type the raster declares, as [`values_per_zone`] charges them. A strip read in another type
/// could hold values those histograms do not count, listed beyond that charge: the reduction then
/// stops and gives `None`, for the caller to reduce the bands again without histograms.
#[allow(clippy::too_many_arguments)]
pub(super) fn reduce_source(
    index: &ZoneIndex,
    raster: &mut LazyRaster,
    bands: &[BandSpec],
    weight: Option<WeightSpec<'_>>,
    plan: &Plan<'_>,
    strip_rows: usize,
    ahead: usize,
    histograms: bool,
) -> Result<Option<Vec<Vec<Vec<FieldValue>>>>> {
    let windows = strip_windows(index, strip_rows.max(1));
    let declared = raster.data_type();
    // Float64 has no histogram range: zones list every value.
    let data_type = if histograms { declared } else { DataType::F64 };
    let check_type = histograms && histogram_range(declared).is_some();
    let mut other_type = false;
    let read_bands = WindowOptions { bands: bands.iter().map(|b| b.band).collect(), overview: None };
    // Reads share one handle each: reads of a raster take turns, but overlap the reduction.
    let raster = std::sync::Mutex::new(raster);
    let weight_band = weight.as_ref().map(|w| (w.band, w.nodata));
    let weight_raster = std::sync::Mutex::new(weight.map(|w| w.raster));
    let lock = |m| Error::InvalidArgument(format!("zonal {m} reader lock poisoned"));
    let mut state = StreamState::new(bands.iter().map(|b| b.nodata).collect(), plan, Some(data_type));
    let result = crate::ops::strips::read_ahead_by(
        ahead,
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
            if check_type && strip.data_type != declared {
                other_type = true;
                return Err(Error::InvalidData(format!("a strip came back as {:?}, not {declared:?}", strip.data_type)));
            }
            state.add_strip(index, plan, windows[i], &strip, weights.as_ref().map(|(w, nodata)| (w, *nodata)))
        },
    );
    match result {
        Err(_) if other_type => Ok(None),
        result => result.and_then(|()| state.finish(index)).map(Some),
    }
}

/// A reduction fed strip by strip in increasing rows: a zone opens at the strip holding its first
/// row and is summarized at the strip holding its last, its pixels added in row-major order.
pub(super) struct StreamState {
    /// Per band.
    nodata: Vec<Option<f64>>,
    /// The sample type whose values zones count in a histogram ([`histogram_range`]): the
    /// raster's, which its band grouping counts on, or else that of the strip opening a zone.
    data_type: Option<DataType>,
    /// Open zones: (zone, next span, one accumulator per band).
    open: Vec<(usize, usize, Vec<Accumulator>)>,
    /// The next zone to open.
    next: usize,
    /// `[band][statistic][feature]`, empty values until a zone is summarized.
    out: Vec<Vec<Vec<FieldValue>>>,
}

impl StreamState {
    pub fn new(nodata: Vec<Option<f64>>, plan: &Plan<'_>, data_type: Option<DataType>) -> Self {
        let features = plan.fids.len();
        let empty = plan.empty();
        let out = nodata.iter().map(|_| empty.iter().map(|v| vec![v.clone(); features]).collect()).collect();
        Self { nodata, data_type, open: Vec::new(), next: 0, out }
    }

    /// Adds `strip`, the pixels of `window` (band after band), with the weights of the same window
    /// and their nodata. Windows must come in increasing rows, as [`strip_windows`] gives them.
    pub fn add_strip(
        &mut self,
        index: &ZoneIndex,
        plan: &Plan<'_>,
        window: Window,
        strip: &Strip<'_>,
        weights: Option<(&Strip<'_>, Option<f64>)>,
    ) -> Result<()> {
        let zones = &index.zones;
        let Window { x: c0, y: r0, height, .. } = window;
        let r1 = r0 + height;
        let bands = self.nodata.len();
        while self.next < zones.len() && zones[self.next].row_start < r1 {
            // Saturated on a 32-bit target: an 8- or 16-bit zone that big still gets a histogram,
            // and `Accumulator::new` reserves no lists past `isize::MAX` bytes.
            let pixels = if plan.keep_values { usize::try_from(zones[self.next].pixels()).unwrap_or(usize::MAX) } else { 0 };
            let data_type = self.data_type.unwrap_or(strip.data_type);
            self.open.push((self.next, 0, (0..bands).map(|_| Accumulator::new(plan, pixels, data_type)).collect()));
            self.next += 1;
        }
        // Zones accumulate independently: in parallel, keeping the error of the first zone.
        let failure = std::sync::Mutex::new(None::<(usize, Error)>);
        let nodata = &self.nodata;
        let chunk = crate::par::chunk_len(self.open.len(), 8);
        crate::par::chunks_mut(&mut self.open, chunk, |_, chunk| {
            for (zone, cursor, accumulators) in chunk {
                let spans = &zones[*zone].spans;
                let fid = plan.fids[zones[*zone].feature];
                let mut run = || -> Result<()> {
                    while let Some(span) = spans.get(*cursor).filter(|s| (s.row as usize) < r1) {
                        // Band by band, the span's pixels in a loop of their sample type: a
                        // band's pixels are still added in row-major order.
                        let start = (span.row as usize - r0) * strip.cols + span.col as usize - c0;
                        let (len, coverage, data_type) = (span.len as usize, span.coverage, strip.data_type);
                        for (b, (acc, &nodata)) in accumulators.iter_mut().zip(nodata).enumerate() {
                            match weights {
                                // Nothing to fail on without weights.
                                None => strip.each(b, start, len, |_, v| {
                                    acc.add_value(v, nodata, data_type, coverage, plan);
                                    Ok(())
                                })?,
                                Some((w, w_nodata)) => strip.each(b, start, len, |i, v| {
                                    let w = (w.value(0, start + i), w_nodata, w.data_type);
                                    acc.add(v, nodata, data_type, coverage, w, plan, fid)
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
        // Finished zones: binned and summarized in parallel, then written.
        let (finished, still_open): (Vec<_>, Vec<_>) = std::mem::take(&mut self.open).into_iter().partition(|o| zones[o.0].row_end <= r1);
        self.open = still_open;
        let mut jobs: Vec<_> = finished.into_iter().map(|o| (o, Vec::new())).collect();
        let chunk = crate::par::chunk_len(jobs.len(), 4);
        crate::par::chunks_mut(&mut jobs, chunk, |_, chunk| {
            for ((_, _, accumulators), values) in chunk {
                *values = accumulators
                    .iter_mut()
                    .map(|acc| {
                        let bins = if plan.keep_values { acc.bins(plan.fractional) } else { Vec::new() };
                        let ranks = if plan.ranks { cumulative(&bins) } else { Vec::new() };
                        plan.stats.iter().map(|&stat| acc.stat(stat, plan, &bins, &ranks)).collect::<Vec<_>>()
                    })
                    .collect();
            }
        });
        for ((zone, _, _), values) in jobs {
            let feature = zones[zone].feature;
            for (b, band_values) in values.into_iter().enumerate() {
                for (s, value) in band_values.into_iter().enumerate() {
                    self.out[b][s][feature] = value;
                }
            }
        }
        Ok(())
    }

    /// `[band][statistic][feature]`; an error unless every zone of `index` was summarized.
    pub fn finish(self, index: &ZoneIndex) -> Result<Vec<Vec<Vec<FieldValue>>>> {
        let unfinished = self.open.len() + index.zones.len() - self.next;
        if unfinished > 0 {
            return Err(Error::InvalidArgument(format!("zonal statistics: missing windows, {unfinished} zones not finished")));
        }
        Ok(self.out)
    }
}

/// A window of some bands, band after band.
pub(super) struct Strip<'a> {
    data: std::borrow::Cow<'a, RasterData>,
    cols: usize,
    pixels: usize,
    data_type: DataType,
}

impl Strip<'static> {
    fn read(raster: &mut LazyRaster, window: Window, options: &WindowOptions) -> Result<Self> {
        Ok(Strip::of(std::borrow::Cow::Owned(raster.read_window(window, options)?.raster.data), window))
    }
}

impl<'a> Strip<'a> {
    /// `data`, the pixels of `window` band after band, row-major.
    pub fn of(data: std::borrow::Cow<'a, RasterData>, window: Window) -> Self {
        let data_type = data.data_type();
        Self { data, cols: window.width, pixels: window.width * window.height, data_type }
    }

    fn value(&self, band: usize, at: usize) -> f64 {
        self.data.get_f64(band * self.pixels + at)
    }

    /// Calls `f` with the offset from `at` and the value of each of `len` pixels of `band` from
    /// `at`: one loop per sample type, into which `f` inlines. Reading values one by one
    /// (`value`, a call and a type dispatch each) made the reduction about five times slower.
    #[inline]
    fn each(&self, band: usize, at: usize, len: usize, mut f: impl FnMut(usize, f64) -> Result<()>) -> Result<()> {
        use RasterData as D;
        let from = band * self.pixels + at;
        macro_rules! each {
            ($($variant:ident),*) => {
                match &*self.data {
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
    /// Values, kept when a statistic needs them binned: all of them, or with a histogram those it
    /// does not count.
    values: Vec<f64>,
    /// Their coverages, in fractional mode (binary coverage is 1).
    coverages: Vec<f64>,
    /// For a zone of at least as many pixels as its 8- or 16-bit type has values, a histogram of
    /// them: it takes the values it counts, the lists only the others (-0, fractions, values out
    /// of range). Boxed, so that accumulators without one, most of them, stay small.
    histogram: Option<Box<Histogram>>,
}

/// The weight (pixel count, or coverage sum in fractional mode) of each integer from `first`.
struct Histogram {
    first: f64,
    weights: Vec<f64>,
}

/// The integers a histogram counts for a band of `data_type`, every value of an 8- or 16-bit
/// integer type: the first and how many.
fn histogram_range(data_type: DataType) -> Option<(f64, usize)> {
    match data_type {
        DataType::U8 => Some((0.0, 256)),
        DataType::I8 => Some((-128.0, 256)),
        DataType::U16 => Some((0.0, 65_536)),
        DataType::I16 => Some((-32_768.0, 65_536)),
        _ => None,
    }
}

/// The most values an accumulator lists for a zone of a band of `data_type`: a zone of at least as
/// many pixels counts them in a histogram ([`histogram_range`]).
pub(super) fn values_per_zone(data_type: DataType) -> usize {
    histogram_range(data_type).map_or(usize::MAX, |r| r.1)
}

/// The bin of `v` among `bins` integers from `first`, if it is one of them. -0 is not: sorted by
/// `total_cmp`, it comes apart from 0.
#[inline]
fn bin(v: f64, first: f64, bins: usize) -> Option<usize> {
    // Saturating: below `first` and NaN give 0, which fails the round trip.
    let index = (v - first) as usize;
    (index < bins && (first + index as f64).to_bits() == v.to_bits()).then_some(index)
}

/// The integers radix-sorted keys stand for, from `RADIX_FIRST`: every value of an 8- or 16-bit
/// integer type.
const RADIX_FIRST: f64 = -32_768.0;
const RADIX_KEYS: usize = 98_304;

/// `values` as `(value, count)` bins of identical values (-0 apart from 0) in `total_cmp` order.
fn counts(mut values: Vec<f64>) -> Vec<(f64, f64)> {
    // Integers from RADIX_FIRST (-32,768 to 65,535, every value of an 8- or 16-bit type), whatever
    // the band's type, as 17-bit keys sorted by radix: faster than comparing, but for a few values,
    // which its fixed passes over 512 digits would outweigh.
    if values.len() >= 256 && values.iter().all(|&v| bin(v, RADIX_FIRST, RADIX_KEYS).is_some()) {
        let mut keys: Vec<u32> = values.iter().map(|&v| (v - RADIX_FIRST) as u32).collect();
        drop(values);
        radix_sort(&mut keys);
        return keys.chunk_by(|a, b| a == b).map(|run| (RADIX_FIRST + f64::from(run[0]), run.len() as f64)).collect();
    }
    values.sort_unstable_by(f64::total_cmp);
    values.chunk_by(|a, b| a.to_bits() == b.to_bits()).map(|run| (run[0], run.len() as f64)).collect()
}

/// Sorts keys below 2^17 (`RADIX_KEYS`): a least-significant-digit radix sort of 9 then 8 bits.
fn radix_sort(keys: &mut Vec<u32>) {
    const _: () = assert!(RADIX_KEYS <= 1 << 17);
    let mut sorted = vec![0u32; keys.len()];
    for (shift, bits) in [(0, 9), (9, 8)] {
        let digit = |k: u32| ((k >> shift) & ((1 << bits) - 1)) as usize;
        let mut next = [0usize; 1 << 9];
        for &k in keys.iter() {
            next[digit(k)] += 1;
        }
        let mut start = 0;
        for n in &mut next {
            (*n, start) = (start, start + *n);
        }
        for &k in keys.iter() {
            let d = digit(k);
            sorted[next[d]] = k;
            next[d] += 1;
        }
        std::mem::swap(keys, &mut sorted);
    }
}

impl Accumulator {
    /// For a zone of `pixels` pixels of a band of `data_type`: a histogram when it has at least as
    /// many pixels as an 8- or 16-bit type has values, else lists reserved for its pixels (none
    /// past `isize::MAX` bytes).
    fn new(plan: &Plan<'_>, pixels: usize, data_type: DataType) -> Self {
        if !plan.keep_values {
            return Self::default();
        }
        if let Some((first, bins)) = histogram_range(data_type).filter(|r| pixels >= r.1) {
            return Self { histogram: Some(Box::new(Histogram { first, weights: vec![0.0; bins] })), ..Self::default() };
        }
        // Reserved exactly, so a large zone's lists don't grow by doubling. A count past
        // `isize::MAX` bytes (`pixels` saturated on a 32-bit target), where `Vec::with_capacity`
        // would panic, is left to the pushes; an allocation the allocator refuses still aborts.
        let reserved = if pixels <= isize::MAX as usize / std::mem::size_of::<f64>() { pixels } else { 0 };
        Self {
            values: Vec::with_capacity(reserved),
            coverages: Vec::with_capacity(if plan.fractional { reserved } else { 0 }),
            ..Self::default()
        }
    }

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

    /// Adds a value without weight, unless it is nodata, not finite or without coverage: whether it
    /// counts.
    #[inline]
    fn add_value(&mut self, v: f64, nodata: Option<f64>, data_type: DataType, coverage: f64, plan: &Plan<'_>) -> bool {
        // A pixel without coverage (which `spans` leaves out) counts nowhere: the histogram drops
        // zero-weight bins, which the lists would keep.
        if !(coverage > 0.0 && v.is_finite() && crate::histogram::is_valid(v, nodata, data_type)) {
            return false;
        }
        self.summary.add(v, coverage, plan.moments);
        if plan.keep_values {
            if let Some(h) = &mut self.histogram
                && let Some(at) = bin(v, h.first, h.weights.len())
            {
                h.weights[at] += if plan.fractional { coverage } else { 1.0 };
                return true;
            }
            self.values.push(v);
            if plan.fractional {
                self.coverages.push(coverage);
            }
        }
        true
    }

    /// The kept values as `(value, weight)` bins in `total_cmp` order; the lists and histogram are
    /// emptied. In binary mode, a bin holds identical values (-0 apart from 0, as their ranks are)
    /// weighted by their count; in fractional mode, equal values weighted by their coverage sum:
    /// summed in sorted order (unstable) in the lists, in the order they were added in the
    /// histogram, so their rounding can differ.
    fn bins(&mut self, fractional: bool) -> Vec<(f64, f64)> {
        let values = std::mem::take(&mut self.values);
        let listed: Vec<(f64, f64)> = if fractional {
            let mut pairs: Vec<(f64, f64)> = values.into_iter().zip(std::mem::take(&mut self.coverages)).collect();
            pairs.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
            // Equal values merged in place, as `merged` does: the first kept, coverages summed in
            // order.
            pairs.dedup_by(|later, kept| {
                let equal = later.0 == kept.0;
                if equal {
                    kept.1 += later.1;
                }
                equal
            });
            pairs
        } else {
            counts(values)
        };
        let Some(histogram) = self.histogram.take() else { return listed };
        let Histogram { first, weights } = *histogram;
        // The listed values are those the histogram does not count: -0 at most is equal to one.
        let mut bins = Vec::with_capacity(listed.len() + weights.iter().filter(|&&w| w > 0.0).count());
        let mut listed = listed.into_iter().peekable();
        for (i, &weight) in weights.iter().enumerate().filter(|b| *b.1 > 0.0) {
            let v = first + i as f64;
            bins.extend(std::iter::from_fn(|| listed.next_if(|l| l.0.total_cmp(&v).is_lt())));
            match bins.last_mut() {
                // A listed -0 and a counted 0 are one bin in fractional mode, as in the lists.
                Some(last) if fractional && last.0 == v => last.1 += weight,
                _ => bins.push((v, weight)),
            }
        }
        bins.extend(listed);
        bins
    }

    /// `bins` is [`Accumulator::bins`] when the statistic needs values, `ranks` their cumulative
    /// weights when the plan needs ranks.
    fn stat(&self, stat: Statistic, plan: &Plan<'_>, bins: &[(f64, f64)], ranks: &[f64]) -> FieldValue {
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
        self.summary.stat(stat, bins, ranks, plan.fractional)
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

    fn stat(&self, stat: Statistic, bins: &[(f64, f64)], ranks: &[f64], fractional: bool) -> FieldValue {
        match stat {
            Statistic::Count => {
                return if fractional { FieldValue::Float(self.weight) } else { FieldValue::Integer(self.count as i64) };
            }
            Statistic::Variety => return FieldValue::Integer(merged(bins).count() as i64),
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
            Statistic::Median => quantile(bins, ranks, 0.5, fractional),
            Statistic::Quantile(p) => quantile(bins, ranks, p / 100.0, fractional),
            // Bins ascend by value: `>=` lets a later (larger) value win a mode tie, `<` keeps the
            // earlier (smaller) value on a minority tie, as exactextract 0.3.0 does.
            Statistic::Mode => merged(bins).fold((f64::NAN, f64::NEG_INFINITY), |b, (v, w)| if w >= b.1 { (v, w) } else { b }).0,
            Statistic::Minority => merged(bins).fold((f64::NAN, f64::INFINITY), |b, (v, w)| if w < b.1 { (v, w) } else { b }).0,
            Statistic::Count | Statistic::Variety | Statistic::WeightedMean | Statistic::WeightedSum | Statistic::WeightedStdDev => {
                unreachable!()
            }
        })
    }
}

/// The cumulative weights of `bins`: in binary mode, the rank after each bin's last value.
fn cumulative(bins: &[(f64, f64)]) -> Vec<f64> {
    bins.iter()
        .scan(0.0, |n, b| {
            *n += b.1;
            Some(*n)
        })
        .collect()
}

/// Bins of equal values (-0 with 0) merged, value of the first, weights summed.
fn merged(bins: &[(f64, f64)]) -> impl Iterator<Item = (f64, f64)> + '_ {
    bins.chunk_by(|a, b| a.0 == b.0).map(|run| (run[0].0, run.iter().map(|b| b.1).sum()))
}

/// Quantile `q` (0 to 1) of the non-empty [`Accumulator::bins`]: exactextract's weighted quantile
/// in fractional mode, else linear interpolation between the closest ranks (numpy's default),
/// with a halfway point taken by `f64::midpoint` so the 0.5 quantile is the median of two middle
/// values. `ranks` are the bins' cumulative counts in binary mode.
fn quantile(bins: &[(f64, f64)], ranks: &[f64], q: f64, fractional: bool) -> f64 {
    if fractional {
        return super::fractional::quantile(bins, q);
    }
    let h = (ranks[ranks.len() - 1] - 1.0) * q;
    let lo = h.floor() as usize;
    let fraction = h - lo as f64;
    // The values of ranks `lo` and `lo + 1` (from 0), the second only when interpolated.
    let at = ranks.partition_point(|&r| r <= lo as f64);
    let a = bins[at].0;
    let b = if ranks[at] > (lo + 1) as f64 { a } else { bins.get(at + 1).map_or(a, |b| b.0) };
    if fraction == 0.0 {
        a
    } else if fraction == 0.5 {
        a.midpoint(b)
    } else {
        // Values of opposite signs are weighted apart: their difference can overflow.
        if a.is_sign_negative() == b.is_sign_negative() { a + fraction * (b - a) } else { a * (1.0 - fraction) + b * fraction }
    }
}

#[cfg(test)]
mod tests {
    use super::super::index::{GridKey, Span, Zone, ZoneIndex, build};
    use super::{BandSpec, Plan, open_pixels_peak, reduce_source};

    #[test]
    fn open_pixels_peak_counts_zones_open_in_the_same_strip() {
        let zone = |row_start: usize, row_end: usize, len| Zone {
            feature: 0,
            row_start,
            row_end,
            spans: (row_start..row_end).map(|row| Span { row: row as u32, col: 0, len, coverage: 1.0 }).collect(),
        };
        // Strips of 10 rows: the zones of rows 0-14 (30 pixels) and 12-29 (18) are open together
        // in strip 1; the zone of row 40 (100) is alone.
        let zones = vec![zone(0, 15, 2), zone(12, 30, 1), zone(40, 41, 100)];
        assert_eq!(open_pixels_peak(&ZoneIndex { zones: zones.clone() }, 10, usize::MAX), 100);
        assert_eq!(open_pixels_peak(&ZoneIndex { zones: zones[..2].to_vec() }, 10, usize::MAX), 48);
        assert_eq!(open_pixels_peak(&ZoneIndex { zones: Vec::new() }, 10, usize::MAX), 0);
        // A zone counts at most `per_zone` pixels (its histogram's bins): 20 + 18 together in
        // strip 1; at 18, 18 + 18.
        assert_eq!(open_pixels_peak(&ZoneIndex { zones: zones.clone() }, 10, 20), 38);
        assert_eq!(open_pixels_peak(&ZoneIndex { zones: zones.clone() }, 10, 18), 36);
        // Counted over the zones, not the rows between them: here rows 0 and 4e9.
        assert_eq!(open_pixels_peak(&ZoneIndex { zones: vec![zone(0, 1, 5), zone(4_000_000_000, 4_000_000_002, 3)] }, 1, usize::MAX), 6);
        // Zones open in the same strip, and one far away: counted from pairs, not slots.
        let far = vec![zone(0, 15, 2), zone(12, 30, 1), zone(4_000_000_000, 4_000_000_001, 7)];
        assert_eq!(open_pixels_peak(&ZoneIndex { zones: far.clone() }, 10, usize::MAX), 48);
        assert_eq!(open_pixels_peak(&ZoneIndex { zones: far }, 10, 20), 38);
    }
    use crate::ops::zonal::Statistic;
    use crate::vector::Coord;
    use crate::{DataType, Feature, FieldValue, Geometry, Layer, LazyRaster, Raster, RasterConfig, RasterDataset};

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
        let zone = |spans: Vec<Span>| Zone {
            feature: 0,
            row_start: spans[0].row as usize,
            row_end: spans[spans.len() - 1].row as usize + 1,
            spans,
        };
        // A multipart zone's parts, far apart: the strips between them are not read.
        let parts = block(300, 10, 1, 2).chain(block(1500, 10, 9, 31)).chain(block(1599, 1, 0, 1)).collect();
        let index = ZoneIndex { zones: vec![zone(block(0, 10, 5, 3).collect()), zone(parts)] };
        assert_eq!(
            strip_windows(&index, 256),
            [Window::new(5, 0, 3, 10), Window::new(1, 300, 2, 10), Window::new(9, 1500, 31, 10), Window::new(0, 1599, 1, 1)]
        );
        assert!(strip_windows(&ZoneIndex { zones: vec![] }, 256).is_empty());
        // Nor between the parts of one zone: a multipart zone at rows 0 and 4e9.
        let index = ZoneIndex { zones: vec![zone(block(0, 1, 1, 1).chain(block(4_000_000_007, 1, 2, 4)).collect())] };
        assert_eq!(strip_windows(&index, 1), [Window::new(1, 0, 1, 1), Window::new(2, 4_000_000_007, 4, 1)]);
        // Memory follows the zones, not the rows between them: zones at rows 0 and 4e9.
        let index = ZoneIndex { zones: vec![zone(block(0, 1, 1, 1).collect()), zone(block(4_000_000_007, 2, 2, 4).collect())] };
        assert_eq!(
            strip_windows(&index, 1),
            [Window::new(1, 0, 1, 1), Window::new(2, 4_000_000_007, 4, 1), Window::new(2, 4_000_000_008, 4, 1)]
        );
        // Zones sharing a strip give one window over both, from slots per strip and, with a zone
        // far away, from (zone, strip) pairs.
        let index = ZoneIndex { zones: vec![zone(block(0, 2, 1, 1).collect()), zone(block(1, 2, 5, 3).collect())] };
        assert_eq!(strip_windows(&index, 256), [Window::new(1, 0, 7, 3)]);
        let mut zones = index.zones;
        zones.push(zone(block(4_000_000_000, 1, 0, 1).collect()));
        assert_eq!(
            strip_windows(&ZoneIndex { zones }, 2),
            [Window::new(1, 0, 7, 2), Window::new(5, 2, 3, 1), Window::new(0, 4_000_000_000, 1, 1)]
        );
    }

    #[test]
    fn binary_quantiles_of_extreme_values_of_opposite_signs_stay_finite() {
        let sorted = [(-f64::MAX, 1.0), (f64::MAX, 1.0)];
        let q25 = super::quantile(&sorted, &[1.0, 2.0], 0.25, false);
        assert!((q25 / (-f64::MAX / 2.0) - 1.0).abs() < 1e-15, "{q25}");
        assert_eq!(super::quantile(&sorted, &[1.0, 2.0], 0.5, false), 0.0);
        assert_eq!(super::quantile(&[(1.0, 1.0), (3.0, 1.0)], &[1.0, 2.0], 0.25, false), 1.5);
    }

    /// The statistics of `acc` as a finished zone computes them.
    fn finish(acc: &mut super::Accumulator, plan: &Plan<'_>, stats: &[Statistic], fractional: bool) -> Vec<FieldValue> {
        let bins = acc.bins(fractional);
        let ranks = super::cumulative(&bins);
        stats.iter().map(|&stat| acc.stat(stat, plan, &bins, &ranks)).collect()
    }

    /// -0 and 0 keep their own ranks, as when the values were sorted one by one, and are one value
    /// for mode, minority and variety.
    #[test]
    fn signed_zeros_keep_their_ranks() {
        let names = "median quantile_25 mode minority variety";
        let stats: Vec<Statistic> = names.split(' ').map(|s| Statistic::parse(s).unwrap()).collect();
        let plan = Plan::new(&stats, false, &[0]);
        let bits = |values: &[f64]| {
            let mut acc = super::Accumulator::default();
            for &v in values {
                acc.add_value(v, None, DataType::F64, 1.0, &plan);
            }
            finish(&mut acc, &plan, &stats, false)
                .into_iter()
                .map(|v| match v {
                    FieldValue::Float(f) => f.to_bits(),
                    FieldValue::Integer(i) => i as u64,
                    v => panic!("{v:?}"),
                })
                .collect::<Vec<_>>()
        };
        // midpoint(-0, 0) is 0.
        assert_eq!(bits(&[0.0, -0.0])[0], 0.0f64.to_bits());
        let got = bits(&[0.0, 5.0, -0.0, 0.0, -0.0]);
        assert_eq!(got, [0.0f64.to_bits(), (-0.0f64).to_bits(), (-0.0f64).to_bits(), 5.0f64.to_bits(), 2]);
        // -0 and 0 (two values) against two 5: a mode tie takes the larger, 5, a minority tie the
        // smaller, -0.
        let tie = bits(&[5.0, -0.0, 0.0, 5.0]);
        assert_eq!((tie[2], tie[3]), (5.0f64.to_bits(), (-0.0f64).to_bits()));
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
            reduce_source(&index, &mut raster, &[BandSpec { band: 1, nodata: None }], None, &plan, rows, 4, true).unwrap().unwrap()[0][0][0]
                .clone()
        };
        let expected = crate::FieldValue::Float(sum / (cols * rows) as f64);
        assert_eq!(mean(&["mean"]), expected);
        assert_eq!(mean(&["mean", "stddev", "cv"]), expected);
    }

    /// A zone of at least as many pixels as its 8- or 16-bit type has values starts with a
    /// histogram; one pixel fewer, or another type, lists its values.
    #[test]
    fn histogram_starts_at_as_many_pixels_as_the_type_has_values() {
        use super::Accumulator;
        let stats = [Statistic::Median];
        let plan = Plan::new(&stats, false, &[0]);
        for (data_type, values) in [(DataType::U8, 256), (DataType::I8, 256), (DataType::U16, 65_536), (DataType::I16, 65_536)] {
            assert!(Accumulator::new(&plan, values, data_type).histogram.is_some(), "{data_type:?}");
            assert!(Accumulator::new(&plan, values - 1, data_type).histogram.is_none(), "{data_type:?}");
        }
        assert!(Accumulator::new(&plan, 1 << 20, DataType::F32).histogram.is_none());
        // Nothing kept without a statistic that needs values.
        assert!(Accumulator::new(&Plan::new(&[Statistic::Mean], false, &[0]), 1 << 20, DataType::U8).histogram.is_none());
    }

    /// A stream of an 8-bit raster opens a zone of at least 256 pixels with a histogram, whether
    /// the type is the raster's (`reduce_source`) or that of the strip opening the zone; a stream
    /// declared Float64 lists the values.
    #[test]
    fn eight_bit_streams_open_zones_with_histograms() {
        use super::{StreamState, Strip};
        let (cols, rows) = (40, 30);
        let mut layer = Layer::new("zones");
        layer.features.push(Feature { fid: 0, geometry: Some(Geometry::polygon(ring(0., 0., 40., 30.), vec![])), attributes: vec![] });
        let cfg = RasterConfig { cols, rows, data_type: DataType::U8, ..Default::default() };
        let pixels: Vec<f64> = (0..cols * rows).map(|i| (i % 251) as f64).collect();
        let mut ds = RasterDataset::new(Raster::from_data(cfg, pixels).unwrap(), false);
        ds.set_geo_transform([0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]);
        let mut raster = LazyRaster::from(ds);
        let index = build(&layer, &GridKey::of(&raster), false, false).unwrap();
        assert!(index.zones[0].pixels() >= 256);
        let plan = Plan::new(&[Statistic::Median], false, &[0]);
        let window = crate::Window { x: 0, y: 0, width: cols, height: 7 };
        let strip = Strip::read(&mut raster, window, &crate::WindowOptions { bands: vec![1], overview: None }).unwrap();
        assert_eq!(strip.data_type, DataType::U8);
        for (data_type, histogram) in [(Some(DataType::U8), true), (None, true), (Some(DataType::F64), false)] {
            let mut state = StreamState::new(vec![None], &plan, data_type);
            state.add_strip(&index, &plan, window, &strip, None).unwrap();
            assert_eq!(state.open.len(), 1, "the zone stays open past its first strip");
            assert_eq!(state.open[0].2[0].histogram.is_some(), histogram, "{data_type:?}");
        }
    }

    /// A pixel without coverage counts in neither the histogram nor the lists, so both agree.
    #[test]
    fn pixels_without_coverage_count_nowhere() {
        use super::Accumulator;
        let stats = [Statistic::Count, Statistic::Median, Statistic::Mode, Statistic::Variety];
        for fractional in [false, true] {
            let plan = Plan::new(&stats, fractional, &[0]);
            let summarize = |data_type| {
                let mut acc = Accumulator::new(&plan, 256, data_type);
                for (v, coverage) in [(3.0, 1.0), (5.0, 0.5), (7.0, 0.0), (9.0, -0.25), (11.0, f64::NAN)] {
                    assert_eq!(acc.add_value(v, None, data_type, coverage, &plan), coverage > 0.0, "{v}");
                }
                let bins = acc.bins(fractional);
                stats.iter().map(|&s| acc.stat(s, &plan, &bins, &super::cumulative(&bins))).collect::<Vec<_>>()
            };
            // U8 counts in a histogram; F64 lists its values.
            assert_eq!(summarize(DataType::U8), summarize(DataType::F64), "fractional {fractional}");
        }
    }

    /// A pixel count saturated to `usize::MAX` (a zone over 2^32 pixels on a 32-bit target) gets
    /// unreserved lists instead of a capacity-overflow panic; an 8-bit band still gets a histogram.
    #[test]
    fn saturated_pixel_counts_leave_lists_unreserved() {
        use super::Accumulator;
        let stats = [Statistic::Median];
        for fractional in [false, true] {
            let plan = Plan::new(&stats, fractional, &[0]);
            let acc = Accumulator::new(&plan, usize::MAX, DataType::F64);
            assert!(acc.histogram.is_none() && acc.values.capacity() == 0 && acc.coverages.capacity() == 0);
            assert!(Accumulator::new(&plan, usize::MAX, DataType::U8).histogram.is_some());
        }
    }

    /// A raster declared 8-bit whose strips come back as floats (half-integer values here, which a
    /// histogram cannot count) is reduced without histograms: `reduce_source` says so before
    /// listing them beyond the charge, and `zonal_stats` gives the float raster's results.
    #[test]
    fn strips_of_another_type_than_declared_are_reduced_without_histograms() {
        let (cols, rows) = (40, 30);
        let mut layer = Layer::new("zones");
        layer.features.push(Feature { fid: 0, geometry: Some(Geometry::polygon(ring(0., 0., 40., 30.), vec![])), attributes: vec![] });
        // Three bands of different values, so that results out of band order would differ.
        let pixels: Vec<f64> = (0..3 * cols * rows).map(|i| (i % 37 + 40 * (i / (cols * rows))) as f64 + 0.5).collect();
        let raster = |declared| {
            let cfg = RasterConfig { cols, rows, bands: 3, data_type: DataType::F64, ..Default::default() };
            let mut ds = RasterDataset::new(Raster::from_data(cfg, pixels.clone()).unwrap(), false);
            ds.set_geo_transform([0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]);
            ds.raster.data_type = declared;
            LazyRaster::from(ds)
        };
        let (mut declared_u8, mut float) = (raster(DataType::U8), raster(DataType::F64));
        assert_eq!(declared_u8.data_type(), DataType::U8);
        let stats = [Statistic::Count, Statistic::Median, Statistic::Mode, Statistic::Variety];
        let plan = Plan::new(&stats, false, &[0]);
        let index = build(&layer, &GridKey::of(&float), false, false).unwrap();
        let bands = [BandSpec { band: 1, nodata: None }];
        assert!(reduce_source(&index, &mut declared_u8, &bands, None, &plan, 7, 1, true).unwrap().is_none());
        let expected = reduce_source(&index, &mut float, &bands, None, &plan, 7, 1, true).unwrap().unwrap();
        assert_eq!(reduce_source(&index, &mut declared_u8, &bands, None, &plan, 7, 1, false).unwrap().unwrap(), expected);
        let zonal = |raster: LazyRaster| {
            let bands = (1..=3).map(|b| crate::ops::zonal::ZonalBand::new(b, format!("b{b}"))).collect();
            let source = crate::ops::zonal::ZonalSource { raster: raster.into(), bands };
            let options = crate::ops::zonal::ZonalOptions { stats: stats.to_vec(), ..Default::default() };
            crate::ops::zonal::zonal_stats(&layer, &mut [source], &options).unwrap().features[0].attributes.clone()
        };
        assert_eq!(zonal(raster(DataType::U8)), zonal(raster(DataType::F64)));
    }

    /// Zones of at least as many pixels as an 8- or 16-bit band has values are counted in a
    /// histogram: the same results as the value lists of the same values in a Float64 band.
    #[test]
    fn counted_integer_bands_match_listed_float_bands() {
        let (cols, rows) = (400, 300);
        assert!(cols * rows > 65_536);
        let mut layer = Layer::new("zones");
        let geometries = [
            Geometry::polygon(ring(0., 0., cols as f64, rows as f64), vec![]),
            Geometry::polygon(ring(3.3, 2.7, 390.6, 290.2), vec![ring(50.5, 50.5, 60.2, 70.9)]),
            Geometry::polygon(ring(10.5, 10.5, 30.5, 40.5), vec![]),
            // 16 x 16 and 15 x 17 pixels: at and below the 256 values of an 8-bit type.
            Geometry::polygon(ring(100., 100., 116., 116.), vec![]),
            Geometry::polygon(ring(200., 100., 215., 117.), vec![]),
        ];
        for (i, g) in geometries.into_iter().enumerate() {
            layer.features.push(Feature { fid: i as u64, geometry: Some(g), attributes: vec![] });
        }
        let names = "count min max mean median quantile_0 quantile_10 quantile_33 quantile_90 quantile_100 mode minority variety";
        let stats: Vec<Statistic> = names.split(' ').map(|s| Statistic::parse(s).unwrap()).collect();
        let fids = [0, 1, 2, 3, 4];
        for (data_type, first, values) in
            [(DataType::U8, 0, 256), (DataType::I8, -128, 256), (DataType::U16, 0, 65536), (DataType::I16, -32768, 65536)]
        {
            // Skewed, with ties, and every value of the type.
            let pixels: Vec<f64> =
                (0..cols * rows).map(|i| (first + ((i as i64 * i as i64 * 7919 + i as i64 / 3) % values)) as f64).collect();
            let raster = |data_type| {
                let cfg = RasterConfig { cols, rows, data_type, ..Default::default() };
                let mut ds = RasterDataset::new(Raster::from_data(cfg, pixels.clone()).unwrap(), false);
                ds.set_geo_transform([0.0, 1.0, 0.0, rows as f64, 0.0, -1.0]);
                LazyRaster::from(ds)
            };
            let (mut counted, mut listed) = (raster(data_type), raster(DataType::F64));
            assert_eq!(counted.data_type(), data_type);
            let nodata = Some((first + 5) as f64);
            for fractional in [false, true] {
                let index = build(&layer, &GridKey::of(&listed), false, fractional).unwrap();
                let plan = Plan::new(&stats, fractional, &fids);
                let expected =
                    reduce_source(&index, &mut listed, &[BandSpec { band: 1, nodata }], None, &plan, rows, 1, true).unwrap().unwrap();
                assert!(expected[0].iter().all(|s| s.iter().all(|v| !v.is_null())));
                for strip in [rows, 7] {
                    let got =
                        reduce_source(&index, &mut counted, &[BandSpec { band: 1, nodata }], None, &plan, strip, 2, true).unwrap().unwrap();
                    if !fractional {
                        assert_eq!(got, expected, "{data_type:?} strip {strip}");
                        continue;
                    }
                    // Coverage sums are rounded in another order (`Accumulator::bins`).
                    for (g, e) in got.iter().flatten().flatten().zip(expected.iter().flatten().flatten()) {
                        match (g, e) {
                            (FieldValue::Float(g), FieldValue::Float(e)) => {
                                assert!((g - e).abs() <= 1e-12 * e.abs().max(1.0), "{data_type:?} strip {strip}: {g} {e}")
                            }
                            _ => assert_eq!(g, e, "{data_type:?} strip {strip}"),
                        }
                    }
                }
            }
        }
    }

    /// A zone of a 16-bit band large enough for a histogram, also fed windows of other sample
    /// types (a `ZonalStream`): values the histogram does not count (-0, fractions, out of range)
    /// stay listed, and the statistics are those of a Float64 zone listing them all.
    #[test]
    fn histogram_keeps_listing_values_it_does_not_count() {
        use super::Accumulator;
        let names = "median quantile_0 quantile_25 quantile_99 quantile_100 mode minority variety";
        let stats: Vec<Statistic> = names.split(' ').map(|s| Statistic::parse(s).unwrap()).collect();
        let others = [2.5, 70_000.0, -40_000.0, -3.0, 1e9, 2.5, 7.0, -0.5, 65_535.0, -32_768.0, 70_000.0, -0.0, -0.0];
        for fractional in [false, true] {
            let plan = Plan::new(&stats, fractional, &[0]);
            let accumulator = |data_type| Accumulator::new(&plan, 70_000, data_type);
            let (mut counted, mut listed) = (accumulator(DataType::U16), accumulator(DataType::F64));
            assert!(counted.histogram.is_some() && listed.histogram.is_none());
            let values = (0..70_000u64).map(|i| ((i * i) % 1000) as f64).map(|v| (v, DataType::U16));
            for (i, (v, data_type)) in values.chain(others.iter().map(|&v| (v, DataType::F64))).enumerate() {
                // Exact binary fractions: their sums do not depend on the order.
                let coverage = if fractional { [1.0, 0.25, 0.5, 0.125][i % 4] } else { 1.0 };
                counted.add_value(v, None, data_type, coverage, &plan);
                listed.add_value(v, None, DataType::F64, coverage, &plan);
            }
            assert_eq!(counted.values.len(), 11);
            assert_eq!(finish(&mut counted, &plan, &stats, fractional), finish(&mut listed, &plan, &stats, fractional));
        }
        // The bins themselves, -0 included.
        let plan = Plan::new(&stats, false, &[0]);
        let (mut counted, mut listed) = (Accumulator::new(&plan, 70_000, DataType::U16), Accumulator::new(&plan, 70_000, DataType::F64));
        for v in (0..70_000u64).map(|i| ((i * i) % 1000) as f64).chain(others) {
            counted.add_value(v, None, DataType::F64, 1.0, &plan);
            listed.add_value(v, None, DataType::F64, 1.0, &plan);
        }
        let bits = |acc: &mut Accumulator| acc.bins(false).iter().map(|b| (b.0.to_bits(), b.1)).collect::<Vec<_>>();
        assert_eq!(bits(&mut counted), bits(&mut listed));
    }

    /// Integers counted by radix sort, other values compared: the same counts in the same order.
    #[test]
    fn counts_sort_integers_by_radix_like_any_values() {
        let reference = |values: &[f64]| {
            let mut sorted = values.to_vec();
            sorted.sort_unstable_by(f64::total_cmp);
            sorted.chunk_by(|a, b| a.to_bits() == b.to_bits()).map(|run| (run[0], run.len() as f64)).collect::<Vec<_>>()
        };
        let integers: Vec<f64> = (0..20_000u64).map(|i| ((i * i * 7919 + i) % 98_304) as f64 - 32_768.0).collect();
        assert!(integers.contains(&-32_768.0) && integers.iter().any(|&v| v > 60_000.0));
        let cases = [
            integers.clone(),
            [integers.clone(), vec![65_535.0, -32_768.0, 0.0, 0.0]].concat(),
            // Not all counted: compared, -0 apart from 0.
            [integers.clone(), vec![0.5]].concat(),
            [integers.clone(), vec![-0.0]].concat(),
            [integers.clone(), vec![70_000.0]].concat(),
            vec![0.0, -0.0, 3.0, -0.0],
            vec![70_000.0, 1.0, 1.0],
            vec![],
        ];
        for values in cases {
            let got = super::counts(values.clone());
            let expected = reference(&values);
            assert_eq!(got.len(), expected.len());
            for (g, e) in got.iter().zip(&expected) {
                assert_eq!((g.0.to_bits(), g.1), (e.0.to_bits(), e.1));
            }
        }
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
            let whole = reduce_source(&index, &mut raster, &bands(), None, &plan, rows, 4, true).unwrap().unwrap();
            assert!(whole[0][0].iter().all(|v| !v.is_null()));
            for strip in [1, 7, 64] {
                assert_eq!(
                    reduce_source(&index, &mut raster, &bands(), None, &plan, strip, 1, true).unwrap().unwrap(),
                    whole,
                    "strip {strip}"
                );
            }
        }
    }
}

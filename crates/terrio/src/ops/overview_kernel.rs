// SPDX-FileCopyrightText: Copyright (c) 2000, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2010, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 1999, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2013, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.12.0:
//     gcore/overview.cpp
//     gcore/gdal_misc.cpp

//! GDAL's overview resampling arithmetic (`gcore/overview.cpp`, GDAL 3.12): the source windows of
//! each overview pixel, nearest neighbour at `0.5 + i * ratio`, the coverage-weighted average in
//! GDAL's work data type (with its factor-of-two paths, the order of their additions included, and
//! its replacement of averages equal to the nodata value), and the most frequent value. Terrio's
//! minimum and maximum (which GDAL's `gdaladdo` does not offer) use the same windows.
//!
//! The windows are clamped to the source chunk GDAL reads them from, so the per-band chunking of
//! `GDALRegenerateOverviewsEx` (full-width chunks of source rows) is reproduced by computing each
//! chunk's overview rows from that chunk only (see [`ex_chunk_rows`]).

use crate::drivers::gtiff_write::AddOverviewsResampling as Method;
use wbraster::DataType;

/// The type GDAL computes averages in (`GDALGetOvrWorkDataType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Work {
    U8,
    U16,
    F32,
    F64,
}

fn work_type(dt: DataType) -> Work {
    match dt {
        DataType::U8 => Work::U8,
        DataType::U16 => Work::U16,
        DataType::I8 | DataType::I16 | DataType::F32 => Work::F32,
        _ => Work::F64,
    }
}

/// The size in bytes of the values GDAL reads a chunk in, for its chunk sizes.
pub(crate) fn work_size(method: Method, dt: DataType) -> usize {
    match method {
        Method::Average => match work_type(dt) {
            Work::U8 => 1,
            Work::U16 => 2,
            Work::F32 => 4,
            Work::F64 => 8,
        },
        _ => dt.size_bytes(),
    }
}

/// The source rows of each chunk `GDALRegenerateOverviewsEx` reads a band of `width` x `height`
/// pixels in blocks of `block_rows` rows by, for overviews up to `max_factor` times smaller:
/// `None` when it hands the band to `GDALRegenerateOverviewsMultiBand` instead (chunks over 10 MB
/// of a tiled band), whose windows are not clamped.
pub(crate) fn ex_chunk_rows(width: usize, height: usize, block: (usize, usize), max_factor: usize, value_size: usize) -> Option<usize> {
    const MAX: usize = 10 * 1024 * 1024;
    let rows = block.1.max(2 * max_factor).max(1);
    let size = |rows: usize| value_size * rows * width;
    if size(rows) > MAX {
        // GDAL hands tiled bands to GDALRegenerateOverviewsMultiBand.
        return if block.0 < width { None } else { Some(rows) };
    }
    let mut rows = rows;
    while size(rows) < MAX / 2 {
        rows *= 2;
    }
    Some(rows.min(height.max(1)).max(1))
}

/// The rows of each chunk of [`ex_chunk_rows`] rows: `(source rows, overview rows)` for an
/// overview of `out_rows` rows of an image of `rows` rows, as `GDALRegenerateOverviewsEx` assigns
/// them.
pub(crate) fn ex_chunks(rows: usize, out_rows: usize, chunk: usize) -> Vec<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    let ratio = rows as f64 / out_rows as f64;
    let mut out = Vec::new();
    let mut y = 0;
    while y < rows {
        let n = chunk.min(rows - y);
        let d0 = (0.5 + y as f64 / ratio) as usize;
        let d1 = if y + n == rows { out_rows } else { ((0.5 + (y + n) as f64 / ratio) as usize).min(out_rows) };
        if d0 < out_rows {
            out.push((y..y + n, d0..d1.max(d0)));
        }
        y += n;
    }
    out
}

/// A source band: its values over rows `rows` (all `width` columns) of an image of `height` rows,
/// and which are valid (for the methods other than nearest: GDAL's nodata or mask band).
pub(crate) struct Band<'a> {
    pub(crate) values: &'a [f64],
    pub(crate) valid: Option<&'a [bool]>,
    pub(crate) width: usize,
    pub(crate) height: usize,
    pub(crate) rows: std::ops::Range<usize>,
}

/// What the overview is computed as.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Target {
    /// The overview's size.
    pub(crate) width: usize,
    pub(crate) height: usize,
    /// The data type of the source and the overview.
    pub(crate) data_type: DataType,
    /// The band's nodata value.
    pub(crate) nodata: Option<f64>,
    pub(crate) method: Method,
}

/// Computes overview rows `out_rows` (all columns) of `band`, its windows clamped to the source
/// rows `chunk`, with the factor-of-two average's groups of pixels counted from column
/// `group_origin`. Returns the overview values (of the overview's data type, as `f64`).
pub(crate) fn resample(
    band: &Band,
    t: &Target,
    out_rows: std::ops::Range<usize>,
    chunk: std::ops::Range<usize>,
    group_origin: usize,
) -> Vec<f64> {
    // Coarse row groups preserve each pixel's visitation/tie order. Each worker reuses its
    // mode counters; small jobs stay sequential to avoid scheduling and buffer overhead.
    if t.method == Method::Mode && out_rows.len() > 16 && out_rows.len() * t.width >= 65536 {
        let mut out = vec![0.0; out_rows.len() * t.width];
        crate::par::chunks_mut_coarse(&mut out, 16 * t.width, |i, rows| {
            let start = out_rows.start + i * 16;
            rows.copy_from_slice(&resample(band, t, start..start + rows.len() / t.width, chunk.clone(), group_origin));
        });
        return out;
    }
    let rx = band.width as f64 / t.width as f64;
    let ry = band.height as f64 / t.height as f64;
    let (cx0, cx1) = (0usize, band.width);
    let (cy0, cy1) = (chunk.start.max(band.rows.start), chunk.end.min(band.rows.end));
    let at = |x: usize, y: usize| band.values[(y - band.rows.start) * band.width + x];
    let ok = |x: usize, y: usize| band.valid.is_none_or(|v| v[(y - band.rows.start) * band.width + x]);
    let mut out = Vec::with_capacity(out_rows.len() * t.width);
    match t.method {
        Method::Nearest => {
            let xs: Vec<usize> = (0..t.width).map(|i| ((0.5 + i as f64 * rx) as usize).max(cx0).min(cx1 - 1)).collect();
            for i in out_rows {
                let y = ((0.5 + i as f64 * ry) as usize).max(cy0).min(cy1 - 1);
                out.extend(xs.iter().map(|&x| at(x, y)));
            }
        }
        _ => {
            let xs: Vec<XWindow> = (0..t.width).map(|i| XWindow::new(i, rx, cx0, cx1)).collect();
            let spacing_two = xs.windows(2).all(|w| w[0].x1 == w[1].x0) && xs.iter().all(|w| w.x1 - w.x0 == 2);
            let average = Average::new(t);
            let mut mode = Mode::new();
            for i in out_rows {
                let (y0, y1, fy0, fy1) = window(i, ry, cy0, cy1);
                match t.method {
                    Method::Average => {
                        average.row(&mut out, &xs, (y0, y1, fy0, fy1), &at, &ok, band.valid.is_some(), spacing_two, group_origin)
                    }
                    Method::Mode => {
                        for w in &xs {
                            out.push(mode.sample(w, (y0, y1), &at, &ok, band.valid.is_some(), t));
                        }
                    }
                    Method::Min | Method::Max => {
                        for w in &xs {
                            let mut best: Option<f64> = None;
                            for y in y0..y1 {
                                for x in w.x0..w.x1 {
                                    let v = at(x, y);
                                    let valid = ok(x, y) && !v.is_nan() && t.nodata.is_none_or(|nd| v != nd);
                                    if valid {
                                        best = Some(match best {
                                            None => v,
                                            Some(b) if t.method == Method::Min => b.min(v),
                                            Some(b) => b.max(v),
                                        });
                                    }
                                }
                            }
                            out.push(best.unwrap_or(t.nodata.unwrap_or(0.0)));
                        }
                    }
                    Method::Nearest => unreachable!(),
                }
            }
        }
    }
    out
}

/// The source window `[start, end)` of overview line or column `i` at `ratio`, clamped to
/// `[lo, hi)`, with the weights GDAL gives its first and last source lines.
fn window(i: usize, ratio: f64, lo: usize, hi: usize) -> (usize, usize, f64, f64) {
    let f0 = i as f64 * ratio;
    let mut a = ((f0 + 1e-8) as usize).max(lo);
    let f1 = (i + 1) as f64 * ratio;
    let mut b = (f1 - 1e-8).ceil() as usize;
    if b == a {
        b += 1;
    }
    b = b.min(hi);
    if a >= b {
        a = b.saturating_sub(1);
    }
    let first = if a + 1 == b { 1.0 } else { 1.0 - (f0 - a as f64) };
    let last = 1.0 - (b as f64 - f1);
    (a, b, first, last)
}

/// A column's source window and weights (GDAL's `PrecomputedXValue`).
struct XWindow {
    x0: usize,
    x1: usize,
    left: f64,
    right: f64,
    full_line: f64,
}

impl XWindow {
    fn new(i: usize, ratio: f64, lo: usize, hi: usize) -> Self {
        let (x0, x1, left, right) = window(i, ratio, lo, hi);
        let mut full_line = left;
        if x0 + 1 < x1 {
            full_line += (x1 - x0 - 2) as f64;
            full_line += right;
        }
        Self { x0, x1, left, right, full_line }
    }
}

/// GDAL's average (`GDALResampleChunk_AverageOrRMS_T`), in its work type.
struct Average {
    work: Work,
    data_type: DataType,
    nodata: Option<f64>,
    /// The nodata value in the work type, and its replacement.
    work_nodata: f64,
    replacement: f64,
}

impl Average {
    fn new(t: &Target) -> Self {
        let work = work_type(t.data_type);
        let to_work = |v: f64| match work {
            Work::U8 => (v as i64).clamp(0, 255) as f64,
            Work::U16 => (v as i64).clamp(0, 65535) as f64,
            Work::F32 => v as f32 as f64,
            Work::F64 => v,
        };
        let work_nodata = t.nodata.map_or(0.0, to_work);
        let replacement = t.nodata.map_or(0.0, |nd| to_work(replacement_value(t.data_type, nd)));
        Self { work, data_type: t.data_type, nodata: t.nodata, work_nodata, replacement }
    }

    /// A value of the work type as the overview stores it (GDALCopyWords to its type).
    fn store(&self, v: f64) -> f64 {
        match (self.work, self.data_type) {
            (Work::U8, _) | (Work::U16, _) | (Work::F64, DataType::F64) | (Work::F32, DataType::F32) => v,
            _ => super::convert_value(v, self.data_type),
        }
    }

    fn finish(&self, v: f64) -> f64 {
        let v = match self.work {
            Work::F32 => v as f32 as f64,
            _ => v,
        };
        let v = if self.nodata.is_some() && v == self.work_nodata { self.replacement } else { v };
        self.store(v)
    }

    #[allow(clippy::too_many_arguments)]
    fn row(
        &self,
        out: &mut Vec<f64>,
        xs: &[XWindow],
        (y0, y1, bottom, top): (usize, usize, f64, f64),
        at: &impl Fn(usize, usize) -> f64,
        ok: &impl Fn(usize, usize) -> bool,
        masked: bool,
        spacing_two: bool,
        group_origin: usize,
    ) {
        let integer = matches!(self.work, Work::U8 | Work::U16);
        if spacing_two && y1 == y0 + 2 && !masked {
            // Factor-of-two windows without a nodata mask: GDAL's optimized paths.
            let group = match self.work {
                Work::F32 => 4,
                Work::F64 => 2,
                _ => 1,
            };
            let n = xs.len();
            let tail = n - group_origin.min(n);
            let grouped_end = group_origin.min(n) + tail / group * group;
            for (i, w) in xs.iter().enumerate() {
                let (a, b, c, d) = (at(w.x0, y0), at(w.x0 + 1, y0), at(w.x0, y0 + 1), at(w.x0 + 1, y0 + 1));
                let v = if integer {
                    ((a + b + c + d) as u64 + 2) as f64 / 4.0
                } else {
                    let in_group = i >= group_origin && i < grouped_end;
                    match (self.work, in_group) {
                        (Work::F32, true) => {
                            let q = 0.25f32;
                            let (a, b, c, d) = (a as f32, b as f32, c as f32, d as f32);
                            ((q * a + q * c) + (q * b + q * d)) as f64
                        }
                        (Work::F32, false) => {
                            let q = 0.25f32;
                            let (a, b, c, d) = (a as f32, b as f32, c as f32, d as f32);
                            (q * a + q * b + q * c + q * d) as f64
                        }
                        (_, true) => (0.25 * a + 0.25 * c) + (0.25 * b + 0.25 * d),
                        (_, false) => 0.25 * a + 0.25 * b + 0.25 * c + 0.25 * d,
                    }
                };
                let v = if integer { v.floor() } else { v };
                out.push(self.store(v));
            }
            return;
        }
        let mut full_column = bottom;
        if y0 + 1 < y1 {
            full_column += (y1 - y0 - 2) as f64;
            full_column += top;
        }
        for w in xs {
            if !masked {
                let (mul, inv) = if integer {
                    (1.0, 1.0)
                } else {
                    let m = ((y1 - y0) * (w.x1 - w.x0)) as f64;
                    (m, 1.0 / m)
                };
                let mut total = 0.0;
                for y in y0..y1 {
                    let mut line = at(w.x0, y) * inv * w.left;
                    if w.x0 + 1 < w.x1 {
                        for x in w.x0 + 1..w.x1 - 1 {
                            line += at(x, y) * inv;
                        }
                        line += at(w.x1 - 1, y) * inv * w.right;
                    }
                    let wy = if y == y0 {
                        bottom
                    } else if y + 1 == y1 {
                        top
                    } else {
                        1.0
                    };
                    total += line * wy;
                }
                let weight = w.full_line * full_column;
                let v = if integer { (total / weight + 0.5).floor() } else { mul * (total / weight) };
                out.push(self.finish(v));
                continue;
            }
            let (mut count, mut total, mut weight) = (0usize, 0.0, 0.0);
            for y in y0..y1 {
                let (mut line, mut line_weight) = (0.0, 0.0);
                if ok(w.x0, y) {
                    count += 1;
                    line_weight = w.left;
                    line = at(w.x0, y) * w.left;
                }
                if w.x0 < w.x1 - 1 {
                    for x in w.x0 + 1..w.x1 - 1 {
                        if ok(x, y) {
                            count += 1;
                            line_weight += 1.0;
                            line += at(x, y);
                        }
                    }
                    let x = w.x1 - 1;
                    if ok(x, y) {
                        count += 1;
                        line_weight += w.right;
                        line += at(x, y) * w.right;
                    }
                }
                let wy = if y == y0 {
                    bottom
                } else if y + 1 == y1 {
                    top
                } else {
                    1.0
                };
                total += line * wy;
                weight += line_weight * wy;
            }
            if count == 0 {
                out.push(self.store(self.work_nodata));
                continue;
            }
            let v = if integer { (total / weight + 0.5).floor() } else { total / weight };
            out.push(self.finish(v));
        }
    }
}

/// GDAL's `GDALGetNoDataReplacementValue`: the value an average equal to the nodata value is
/// replaced with.
fn replacement_value(dt: DataType, nd: f64) -> f64 {
    let (lo, hi) = super::type_range(dt);
    match dt {
        DataType::F32 => {
            if nd.is_nan() || nd.is_infinite() || nd < -(f32::MAX as f64) || nd > f32::MAX as f64 {
                return 0.0;
            }
            let f = nd as f32;
            (if f == f32::MAX { f.next_down() } else { f.next_up() }) as f64
        }
        DataType::F64 => {
            if nd.is_nan() || nd.is_infinite() {
                return 0.0;
            }
            if nd == f64::MAX { nd.next_down() } else { nd.next_up() }
        }
        _ => {
            if nd.is_nan() || nd < lo || nd > hi {
                return 0.0;
            }
            if nd >= hi { hi - 1.0 } else { nd + 1.0 }
        }
    }
}

/// GDAL's mode (`GDALResampleChunk_ModeT`): the first value to reach the highest count. Byte bands
/// leave out the nodata value only (not the mask); others leave out what the mask does.
struct Mode {
    bytes: [u64; 256],
    touched: Vec<usize>,
    seen: Vec<(f64, u64)>,
    counts: std::collections::HashMap<u64, usize>,
}

impl Mode {
    fn new() -> Self {
        Self { bytes: [0; 256], touched: Vec::new(), seen: Vec::new(), counts: Default::default() }
    }

    fn sample(
        &mut self,
        w: &XWindow,
        (y0, y1): (usize, usize),
        at: &impl Fn(usize, usize) -> f64,
        ok: &impl Fn(usize, usize) -> bool,
        masked: bool,
        t: &Target,
    ) -> f64 {
        // Signed integers are compared as unsigned values of their size: an out-of-range nodata
        // value leaves the empty windows at 0.
        let in_range = |nd: f64| match t.data_type {
            DataType::F64 | DataType::F32 => !nd.is_nan(),
            DataType::I8 => (-128.0..=127.0).contains(&nd),
            DataType::U8 => (0.0..=255.0).contains(&nd),
            DataType::U16 | DataType::I16 => (0.0..=65535.0).contains(&nd),
            DataType::U32 | DataType::I32 => (0.0..=4294967295.0).contains(&nd),
            DataType::U64 | DataType::I64 => (0.0..18446744073709551615.0).contains(&nd),
        };
        let empty = t.nodata.filter(|nd| in_range(*nd)).unwrap_or(0.0);
        if t.data_type == DataType::U8 {
            for i in self.touched.drain(..) {
                self.bytes[i] = 0;
            }
            let (mut best, mut high) = (empty, 0);
            for y in y0..y1 {
                for x in w.x0..w.x1 {
                    let v = at(x, y);
                    if t.nodata.is_some() && v == empty {
                        continue;
                    }
                    // Decoded Byte samples (or 0/1 mask samples) are integers in 0..=255.
                    let i = v as usize;
                    if self.bytes[i] == 0 {
                        self.touched.push(i);
                    }
                    self.bytes[i] += 1;
                    if self.bytes[i] > high {
                        best = v;
                        high = self.bytes[i];
                    }
                }
            }
            return best;
        }
        let (seen, counts) = (&mut self.seen, &mut self.counts);
        seen.clear();
        counts.clear();
        let mut best = 0;
        for y in y0..y1 {
            for x in w.x0..w.x1 {
                let v = at(x, y);
                let valid = !masked || ok(x, y);
                if !valid {
                    continue;
                }
                let key = if v == 0.0 {
                    0
                } else if v.is_nan() {
                    f64::NAN.to_bits()
                } else {
                    v.to_bits()
                };
                match counts.get(&key).copied() {
                    Some(i) => {
                        seen[i].1 += 1;
                        if seen[i].1 > seen[best].1 {
                            best = i;
                        }
                    }
                    None => {
                        counts.insert(key, seen.len());
                        seen.push((v, 1));
                    }
                }
            }
        }
        if seen.is_empty() { empty } else { seen[best].0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(width: usize, height: usize, data_type: DataType, nodata: Option<f64>, method: Method) -> Target {
        Target { width, height, data_type, nodata, method }
    }

    #[test]
    fn factor_two_averages_round_as_gdal() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let band = Band { values: &values, valid: None, width: 4, height: 2, rows: 0..2 };
        // (1 + 2 + 5 + 6 + 2) / 4 and (3 + 4 + 7 + 8 + 2) / 4, as integers.
        assert_eq!(resample(&band, &target(2, 1, DataType::U8, None, Method::Average), 0..1, 0..2, 0), vec![4.0, 6.0]);
        let v = resample(&band, &target(2, 1, DataType::F32, None, Method::Average), 0..1, 0..2, 0);
        assert_eq!(v, vec![3.5, 5.5]);
    }

    #[test]
    fn averages_equal_to_nodata_are_replaced() {
        let values = vec![0.0, 2.0, 1.0, 1.0];
        let valid = vec![false, true, true, true];
        let band = Band { values: &values, valid: Some(&valid), width: 2, height: 2, rows: 0..2 };
        // (2 + 1 + 1) / 3 rounds to 1, the nodata value: GDAL stores 2.
        assert_eq!(resample(&band, &target(1, 1, DataType::U8, Some(1.0), Method::Average), 0..1, 0..2, 0), vec![2.0]);
    }

    #[test]
    fn modes_take_the_first_value_to_lead() {
        // 7 is the first to reach a count of 2; 5 only ties it.
        let values = vec![5.0, 7.0, 7.0, 5.0];
        let band = Band { values: &values, valid: None, width: 4, height: 1, rows: 0..1 };
        assert_eq!(resample(&band, &target(1, 1, DataType::I16, None, Method::Mode), 0..1, 0..1, 0), vec![7.0]);
        // A NaN nodata value leaves an empty window at 0.
        let nan = [f64::NAN; 4];
        let valid = [false; 4];
        let band = Band { values: &nan, valid: Some(&valid), width: 4, height: 1, rows: 0..1 };
        assert_eq!(resample(&band, &target(1, 1, DataType::F64, Some(f64::NAN), Method::Mode), 0..1, 0..1, 0), vec![0.0]);
    }

    /// GDAL 3.13.3 mode ignores masks on Byte bands, but excludes numeric nodata.
    #[test]
    fn byte_mode_resets_counts_and_preserves_ties_and_nodata() {
        let values = [5.0, 7.0, 7.0, 5.0, 5.0, 5.0, 7.0, 7.0, 0.0, 0.0, 0.0, 0.0];
        let mask = [false; 12];
        let band = Band { values: &values, valid: Some(&mask), width: 12, height: 1, rows: 0..1 };
        assert_eq!(resample(&band, &target(3, 1, DataType::U8, Some(0.0), Method::Mode), 0..1, 0..1, 0), [7.0, 5.0, 0.0]);
        let mut mode = Mode::new();
        let t = target(1, 1, DataType::F64, None, Method::Mode);
        let w = XWindow::new(0, 4.0, 0, 4);
        assert!(mode.sample(&w, (0, 1), &|_, _| f64::NAN, &|_, _| true, false, &t).is_nan());
        assert_eq!(mode.sample(&w, (0, 1), &|x, _| [5.0, 7.0, 7.0, 5.0][x], &|_, _| true, false, &t), 7.0);
    }

    #[test]
    fn parallel_mode_matches_individual_rows_on_uneven_grids() {
        let values: Vec<_> = (0..1031 * 263).map(|i| ((i * 7 + i / 1031) % 256) as f64).collect();
        let band = Band { values: &values, valid: None, width: 1031, height: 263, rows: 0..263 };
        for dt in [DataType::U8, DataType::I16, DataType::F64] {
            let t = target(517, 131, dt, Some(0.0), Method::Mode);
            let parallel = resample(&band, &t, 0..131, 0..263, 0);
            let serial: Vec<_> = (0..131).flat_map(|y| resample(&band, &t, y..y + 1, 0..263, 0)).collect();
            assert_eq!(parallel, serial);
        }
    }

    #[test]
    fn chunks_assign_every_overview_row_once() {
        let chunks = ex_chunks(1773, 887, 256);
        assert_eq!(chunks.first().unwrap().1.start, 0);
        assert_eq!(chunks.last().unwrap().1.end, 887);
        for w in chunks.windows(2) {
            assert_eq!(w[0].1.end, w[1].1.start);
        }
    }
}

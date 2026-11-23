// SPDX-FileCopyrightText: Copyright (c) 2024 William M. Katzenmeyer, P.E., C.F.M. / CLB Engineering
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   ras2cng v0.15.0:
//     ras2cng/cog.py

//! Overviews that keep the valid area of the full-resolution image (`OVERVIEW_RESAMPLING`
//! `area_matched`, and `area_matched_mode` for categorical rasters).
//!
//! GDAL's `average` (GDAL 3.12.1 `gcore/overview.cpp`) makes a coarse pixel valid when any of its
//! source pixels is, so the valid area of a thin or ragged extent (a flood-depth grid, a river or
//! burn mask) grows at every level; `nearest` keeps it only on average and speckles the margin.
//! An area-matched level instead keeps as many valid cells as its pixels need to cover the valid
//! full-resolution area, those covering the most valid pixels first:
//!
//! - Each source pixel belongs to the coarse cell whose footprint holds its centre, so a level of
//!   GDAL's size (half the previous one, rounded down, for a COG) partitions the pixels of the
//!   level before it: 2 x 2 pixels when the size halves exactly, 2 or 3 per axis otherwise. Each
//!   cell carries the number of valid full-resolution pixels it covers (its weight) and their sum,
//!   level 1 from the image, each later level from the one before.
//! - A level of `w x h` cells over a `cols x rows` image keeps `round(valid * w * h / (cols *
//!   rows))` cells (half to even): the heaviest, those exactly at the threshold weight ranked by
//!   how many of their four neighbours are already kept (most first), then by a deterministic hash
//!   of their index, so that the margin stays connected and the output is reproducible.
//! - A kept cell holds the mean of its valid pixels (rounded half to even for integer types), or
//!   for `area_matched_mode` the most frequent value of the cells below it (of the pixels, at level
//!   1), the lowest on ties. A mean equal to the nodata value is replaced as GDAL replaces one
//!   (`GDALGetNoDataReplacementValue`). Other cells are nodata.
//!
//! NaN is never valid. The method is ras2cng's (`cog.build_area_matched_overviews`, MIT), translated
//! to Rust from its NumPy implementation and adapted to GDAL's level sizes. When every
//! level halves exactly, the levels are those of ras2cng's NumPy implementation, value for value;
//! ras2cng sizes odd levels by rounding up instead, so its pixels differ from these there.

use crate::Result;
use crate::pixel::Pixel;
use wbraster::raster::RasterData;

/// The two area-matched methods.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AreaMatched {
    /// The mean of the valid pixels (`area_matched`).
    Mean,
    /// The most frequent value, for categorical rasters (`area_matched_mode`).
    Mode,
}

impl AreaMatched {
    /// The method named `s` (`area_matched` or `area_matched_mode`, in any case).
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "area_matched" => Some(Self::Mean),
            "area_matched_mode" => Some(Self::Mode),
            _ => None,
        }
    }
}

/// The band metadata item holding, for each overview level, the valid area it keeps over the
/// band's valid full-resolution area (its kept cells times the area of a cell in full-resolution
/// pixels, over the valid pixels), comma-separated: `1` when a level keeps the area exactly.
pub(crate) const AREA_RATIOS_ITEM: &str = "OVERVIEW_AREA_RATIOS";
/// The band metadata item holding the band's valid full-resolution pixels, which the ratios of
/// [`AREA_RATIOS_ITEM`] are relative to.
pub(crate) const BASE_VALID_PIXELS_ITEM: &str = "OVERVIEW_BASE_VALID_PIXELS";

/// What a band's area-matched levels keep.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct BandReport {
    /// Per level, the valid area kept over the valid full-resolution area (1 for an empty band).
    pub(crate) area_ratios: Vec<f64>,
    /// The valid full-resolution pixels.
    pub(crate) base_valid: u64,
}

impl BandReport {
    /// The band metadata items of the report ([`AREA_RATIOS_ITEM`], [`BASE_VALID_PIXELS_ITEM`]),
    /// the ratios as shortest round-trip decimals.
    pub(crate) fn items(&self) -> [(String, String); 2] {
        let ratios: Vec<String> = self.area_ratios.iter().map(|r| format!("{r}")).collect();
        [(AREA_RATIOS_ITEM.to_string(), ratios.join(",")), (BASE_VALID_PIXELS_ITEM.to_string(), self.base_valid.to_string())]
    }
}

/// One level of a band: per cell, the sum and count (weight) of the valid full-resolution pixels it
/// covers, and its modal value (`area_matched_mode`; NaN for none).
struct Cells {
    cols: usize,
    rows: usize,
    /// Per cell, the sum and the weight.
    acc: Vec<(f64, u64)>,
    modes: Vec<f64>,
}

impl Cells {
    fn weight(&self, i: usize) -> u64 {
        self.acc[i].1
    }
}

/// For each of `m` cells along an axis, the range of the `n` cells (or pixels) of the level
/// before whose centres fall in its footprint: index `x` goes to `floor((x + 0.5) * m / n)`.
fn groups(n: usize, m: usize) -> Vec<std::ops::Range<usize>> {
    let (n64, m64) = (n as u128, m as u128);
    let mut start = 0;
    (0..m)
        .map(|c| {
            // The first index whose centre lies beyond the footprint of `c`: the smallest x with
            // (2x + 1) * m >= 2 * (c + 1) * n.
            let end = if c + 1 == m { n } else { ((2 * (c as u128 + 1) * n64).div_ceil(m64).saturating_sub(1)).div_ceil(2) as usize };
            let range = start..end.max(start);
            start = range.end;
            range
        })
        .collect()
}

/// The most frequent of `values` (all valid, none NaN), the lowest on ties; NaN when empty.
fn mode(values: &mut [f64]) -> f64 {
    values.sort_unstable_by(f64::total_cmp);
    let (mut best, mut best_count) = (f64::NAN, 0usize);
    let mut i = 0;
    while i < values.len() {
        let mut j = i + 1;
        // Equal values (-0 and 0 among them) are counted together.
        while j < values.len() && values[j] == values[i] {
            j += 1;
        }
        if j - i > best_count {
            (best, best_count) = (values[i], j - i);
        }
        i = j;
    }
    best
}

/// A level of `cols x rows` cells from the `n_cols x n_rows` cells of the level before (or the
/// image's pixels), `cell(y, x)` giving the sum, weight and modal value of a valid one. Sums add
/// the cells of each row in order, then the rows, which for 2 x 2 cells is NumPy's order for
/// `reshape(h, 2, w, 2).sum(axis=(1, 3))`.
fn reduce(
    (n_cols, n_rows): (usize, usize),
    (cols, rows): (usize, usize),
    mode_of: Option<AreaMatched>,
    cell: impl Fn(usize, usize) -> Option<(f64, u64, f64)> + Sync,
) -> Cells {
    let xs = groups(n_cols, cols);
    let ys = groups(n_rows, rows);
    let categorical = mode_of == Some(AreaMatched::Mode);
    // Each row of cells is filled in place, so that a level is held once (16 bytes a cell, plus
    // 8 for the modes of `area_matched_mode`).
    // -0 is the identity of IEEE addition: adding to it keeps every value, -0 included.
    let mut acc = vec![(-0.0f64, 0u64); cols * rows];
    crate::par::chunks_mut(&mut acc, cols, |oy, line| {
        for (out, xr) in line.iter_mut().zip(&xs) {
            for y in ys[oy].clone() {
                let mut row = -0.0f64;
                for x in xr.clone() {
                    if let Some((s, w, _)) = cell(y, x) {
                        row += s;
                        out.1 += w;
                    }
                }
                out.0 += row;
            }
        }
    });
    let mut modes = Vec::new();
    if categorical {
        modes = vec![f64::NAN; cols * rows];
        crate::par::chunks_mut(&mut modes, cols, |oy, line| {
            let mut values = Vec::new();
            for (out, xr) in line.iter_mut().zip(&xs) {
                values.clear();
                for y in ys[oy].clone() {
                    values.extend(xr.clone().filter_map(|x| cell(y, x).map(|(_, _, m)| m)));
                }
                *out = mode(&mut values);
            }
        });
    }
    Cells { cols, rows, acc, modes }
}

/// A deterministic hash of a cell index (SplitMix64's finalizer), as ras2cng's `_dither`.
#[inline]
fn dither(index: u64, salt: u64) -> u64 {
    let mut x = index ^ salt;
    x = x.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    x ^= x >> 29;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 32;
    x
}

/// `num / den` rounded half to even.
fn round_ratio(num: u128, den: u128) -> u128 {
    let (q, r) = (num / den, num % den);
    match (2 * r).cmp(&den) {
        std::cmp::Ordering::Greater => q + 1,
        std::cmp::Ordering::Equal => q + (q & 1),
        std::cmp::Ordering::Less => q,
    }
}

/// Which cells of a level stay valid: `target` of them, the heaviest first (ras2cng's
/// `_select_area_matched`), ties at the threshold weight broken by kept 4-neighbours, then by
/// the dither salted with the level's factor, then by the index (highest first).
fn select(cells: &Cells, target: u64, factor: u64) -> Vec<bool> {
    let wet: Vec<bool> = cells.acc.iter().map(|c| c.1 > 0).collect();
    if target == 0 {
        return vec![false; wet.len()];
    }
    let mut weights: Vec<u64> = cells.acc.iter().map(|c| c.1).filter(|w| *w > 0).collect();
    if target >= weights.len() as u64 {
        return wet;
    }
    // The target-th heaviest weight: the threshold.
    let k = target as usize - 1;
    let threshold = *weights.select_nth_unstable_by(k, |a, b| b.cmp(a)).1;
    let mut keep: Vec<bool> = cells.acc.iter().map(|c| c.1 > threshold).collect();
    let above = keep.iter().filter(|v| **v).count();
    let remaining = k + 1 - above;
    let ties: Vec<usize> = (0..keep.len()).filter(|&i| cells.weight(i) == threshold).collect();
    if remaining >= ties.len() {
        ties.into_iter().for_each(|i| keep[i] = true);
        return keep;
    }
    let (cols, rows) = (cells.cols, cells.rows);
    let salt = factor.wrapping_mul(0x2545_F491);
    let mut ranked: Vec<(u8, u64, usize)> = ties
        .iter()
        .map(|&i| {
            let (y, x) = (i / cols, i % cols);
            let neighbours = [y > 0 && keep[i - cols], y + 1 < rows && keep[i + cols], x > 0 && keep[i - 1], x + 1 < cols && keep[i + 1]];
            (neighbours.iter().filter(|n| **n).count() as u8, dither(i as u64, salt), i)
        })
        .collect();
    ranked.sort_unstable_by(|a, b| b.cmp(a));
    for &(_, _, i) in &ranked[..remaining] {
        keep[i] = true;
    }
    keep
}

/// The area-matched levels of a band of `cols x rows` samples, of the `sizes` given (each no
/// larger than the one before), with their nominal decimation `factors` (which salt the tie
/// breaking). `nodata` is the band's nodata value, `replacement` the value a mean equal to it
/// takes instead, and `integral` whether the type is an integer type.
///
/// # Errors
/// `InvalidData` when a size is larger than the level before it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn band<T: Pixel>(
    data: &[T],
    (cols, rows): (usize, usize),
    sizes: &[(usize, usize)],
    factors: &[u64],
    method: AreaMatched,
    nodata: f64,
    replacement: f64,
    integral: bool,
) -> Result<(Vec<Vec<T>>, BandReport)> {
    // The nodata value as the type holds it; an integer band has none when it is not one of its
    // values.
    let key = T::from_f64(nodata).to_f64();
    let has_key = !integral || key == nodata;
    let valid = |v: f64| !v.is_nan() && !(has_key && v == key);
    let mut levels = Vec::with_capacity(sizes.len());
    let mut report = BandReport { area_ratios: Vec::with_capacity(sizes.len()), base_valid: 0 };
    let mut previous: Option<Cells> = None;
    for (k, &(w, h)) in sizes.iter().enumerate() {
        let from = previous.as_ref().map_or((cols, rows), |p| (p.cols, p.rows));
        if w == 0 || h == 0 || w > from.0 || h > from.1 {
            return Err(crate::Error::InvalidData(format!("area-matched overview {w}x{h} is larger than the level before it")));
        }
        let cells = match &previous {
            None => reduce(from, (w, h), Some(method), |y, x| {
                let v = data[y * cols + x].to_f64();
                valid(v).then_some((v, 1, v))
            }),
            Some(p) => reduce(from, (w, h), Some(method), |y, x| {
                let i = y * p.cols + x;
                let (s, w) = p.acc[i];
                (w > 0).then(|| (s, w, p.modes.get(i).copied().unwrap_or(f64::NAN)))
            }),
        };
        if k == 0 {
            report.base_valid = cells.acc.iter().map(|c| c.1).sum();
        }
        let base = report.base_valid as u128;
        let (pixels, n_cells) = ((cols * rows) as u128, (w * h) as u128);
        let target = round_ratio(base * n_cells, pixels) as u64;
        let factor = factors.get(k).copied().unwrap_or(1 << (k + 1));
        let keep = select(&cells, target, factor);
        let kept = keep.iter().filter(|v| **v).count() as u128;
        report.area_ratios.push(match base {
            0 => 1.0,
            // Exactly ras2cng's `kept * f * f / valid` when the level divides the image evenly.
            _ if pixels % n_cells == 0 => (kept * (pixels / n_cells)) as f64 / base as f64,
            _ => (kept * pixels) as f64 / (n_cells * base) as f64,
        });
        let level: Vec<T> = (0..keep.len())
            .map(|i| {
                if !keep[i] {
                    return T::from_f64(nodata);
                }
                let v = match method {
                    AreaMatched::Mode => cells.modes[i],
                    AreaMatched::Mean => {
                        let (sum, weight) = cells.acc[i];
                        let mean = sum / weight as f64;
                        if integral { mean.round_ties_even() } else { mean }
                    }
                };
                let v = T::from_f64(v);
                if has_key && v.to_f64() == key { T::from_f64(replacement) } else { v }
            })
            .collect();
        levels.push(level);
        previous = Some(cells);
    }
    Ok((levels, report))
}

/// Wraps typed samples in the `RasterData` of their type.
pub(crate) trait IntoRasterData: Pixel {
    fn into_data(v: Vec<Self>) -> RasterData;
}

macro_rules! into_data {
    ($($t:ty => $variant:ident),*) => {$(
        impl IntoRasterData for $t {
            fn into_data(v: Vec<Self>) -> RasterData {
                RasterData::$variant(v)
            }
        }
    )*};
}
into_data!(u8 => U8, i8 => I8, u16 => U16, i16 => I16, u32 => U32, i32 => I32, u64 => U64, i64 => I64, f32 => F32, f64 => F64);

#[cfg(test)]
mod tests {
    use super::*;

    fn halving(cols: usize, rows: usize, n: usize) -> Vec<(usize, usize)> {
        (1..=n).map(|k| (cols >> k, rows >> k)).collect()
    }

    #[test]
    fn groups_hold_the_centres_of_each_footprint() {
        assert_eq!(groups(8, 4), vec![0..2, 2..4, 4..6, 6..8]);
        // GDAL's footprints of 7 pixels in 3 cells: [0, 2.33), [2.33, 4.67), [4.67, 7).
        assert_eq!(groups(7, 3), vec![0..2, 2..5, 5..7]);
        assert_eq!(groups(5, 2), vec![0..2, 2..5]);
        assert_eq!(groups(5, 3), vec![0..2, 2..3, 3..5]);
        assert_eq!(groups(3, 1), vec![0..3]);
        for (n, m) in [(1, 1), (9, 4), (100, 33), (6705, 3352), (4852, 2426), (1000, 1)] {
            let g = groups(n, m);
            assert_eq!(g.first().map(|r| r.start), Some(0));
            assert_eq!(g.last().map(|r| r.end), Some(n));
            assert!(g.windows(2).all(|p| p[0].end == p[1].start), "{n} {m}");
            for (c, r) in g.iter().enumerate() {
                assert!(!r.is_empty());
                for x in r.clone() {
                    assert_eq!((2 * x + 1) * m / (2 * n), c, "{n} {m} {x}");
                }
            }
        }
    }

    #[test]
    fn mode_takes_the_most_frequent_then_lowest_value() {
        assert_eq!(mode(&mut [3.0, 1.0, 3.0, 1.0]), 1.0);
        assert_eq!(mode(&mut [3.0, 1.0, 3.0, 2.0]), 3.0);
        assert_eq!(mode(&mut [1.0, 2.0]), 1.0);
        assert!(mode(&mut []).is_nan());
    }

    #[test]
    fn dither_matches_ras2cng() {
        // _dither(np.array([0, 1, 12345]), salt=2 * 0x2545F491) in ras2cng.
        let salt = 2u64 * 0x2545_F491;
        let got: Vec<u64> = [0, 1, 12345].iter().map(|i| dither(*i, salt)).collect();
        assert_eq!(got, [12459186666850337160, 6905069863801935470, 9666813718830214867]);
    }

    #[test]
    fn rounding_is_half_to_even() {
        assert_eq!(round_ratio(5, 2), 2);
        assert_eq!(round_ratio(7, 2), 4);
        assert_eq!(round_ratio(9, 4), 2);
        assert_eq!(round_ratio(11, 4), 3);
    }

    /// A diagonal band of valid pixels, which `average` would widen at every level.
    fn diagonal(cols: usize, rows: usize, nd: f32) -> Vec<f32> {
        (0..rows).flat_map(|y| (0..cols).map(move |x| if (x as i64 - y as i64).abs() <= 2 { (x + y) as f32 } else { nd })).collect()
    }

    #[test]
    fn levels_keep_the_valid_area() {
        let nd = -9999.0f32;
        for (cols, rows) in [(64, 48), (67, 45), (33, 70)] {
            let band_data = diagonal(cols, rows, nd);
            let sizes = halving(cols, rows, 3);
            let (levels, report) =
                band(&band_data, (cols, rows), &sizes, &[2, 4, 8], AreaMatched::Mean, nd as f64, -9998.0, false).unwrap();
            let valid = band_data.iter().filter(|v| **v != nd).count() as u64;
            assert_eq!(report.base_valid, valid);
            for (k, (level, &(w, h))) in levels.iter().zip(&sizes).enumerate() {
                assert_eq!(level.len(), w * h);
                let kept = level.iter().filter(|v| **v != nd).count() as u64;
                let target = round_ratio(valid as u128 * (w * h) as u128, (cols * rows) as u128) as u64;
                assert_eq!(kept, target, "{cols}x{rows} level {k}");
                let ratio = kept as f64 * (cols * rows) as f64 / (w * h) as f64 / valid as f64;
                assert!((report.area_ratios[k] - ratio).abs() < 1e-12);
            }
            // Deterministic.
            let again = band(&band_data, (cols, rows), &sizes, &[2, 4, 8], AreaMatched::Mean, nd as f64, -9998.0, false).unwrap();
            assert_eq!(levels, again.0);
        }
    }

    #[test]
    fn means_of_kept_cells_and_integer_rounding() {
        // One 2x2 block of 1, 2, 2, 2 (mean 1.75) beside an empty block.
        let data: Vec<i16> = vec![1, 2, 0, 0, 2, 2, 0, 0];
        let (levels, _) = band(&data, (4, 2), &[(2, 1)], &[2], AreaMatched::Mean, 0.0, 1.0, true).unwrap();
        assert_eq!(levels[0], vec![2, 0]);
        // Half-way means round to even.
        let data: Vec<i16> = vec![1, 2, 3, 4, 3, 2, 5, 6];
        let (levels, _) = band(&data, (4, 2), &[(2, 1)], &[2], AreaMatched::Mean, -1.0, 0.0, true).unwrap();
        assert_eq!(levels[0], vec![2, 4]);
        let (levels, _) = band(&data, (4, 2), &[(2, 1)], &[2], AreaMatched::Mode, -1.0, 0.0, true).unwrap();
        assert_eq!(levels[0], vec![2, 3]);
        // A mean equal to the nodata value takes the replacement value.
        let data: Vec<i16> = vec![-2, 0];
        let (levels, _) = band(&data, (2, 1), &[(1, 1)], &[2], AreaMatched::Mean, -1.0, 0.0, true).unwrap();
        assert_eq!(levels[0], vec![0]);
    }

    #[test]
    fn odd_levels_partition_the_pixels_of_the_level_before() {
        // A 5 x 1 row in 2 cells: pixels 0-1 and 2-4.
        let data = [1.0f32, 3.0, 5.0, 7.0, 9.0];
        let (levels, report) = band(&data, (5, 1), &[(2, 1)], &[2], AreaMatched::Mean, -1.0, -2.0, false).unwrap();
        assert_eq!(levels[0], vec![2.0, 7.0]);
        assert_eq!(report.base_valid, 5);
        assert_eq!(report.area_ratios, vec![1.0]);
    }

    #[test]
    fn nan_is_never_valid() {
        let data = [f32::NAN, 1.0, f32::NAN, f32::NAN];
        let (levels, report) = band(&data, (2, 2), &[(1, 1)], &[2], AreaMatched::Mean, f64::NAN, 0.0, false).unwrap();
        // One valid pixel of four: round(1 / 4) = 0 cells kept.
        assert!(levels[0][0].is_nan());
        assert_eq!(report.base_valid, 1);
        let data = [2.0f32, 1.0, f32::NAN, 3.0];
        let (levels, _) = band(&data, (2, 2), &[(1, 1)], &[2], AreaMatched::Mean, -9999.0, 0.0, false).unwrap();
        assert_eq!(levels[0], vec![2.0]);
    }

    #[test]
    fn an_empty_band_keeps_nothing() {
        let data = [0u8; 16];
        let (levels, report) = band(&data, (4, 4), &[(2, 2), (1, 1)], &[2, 4], AreaMatched::Mean, 0.0, 1.0, true).unwrap();
        assert_eq!(levels, vec![vec![0; 4], vec![0]]);
        assert_eq!(report, BandReport { area_ratios: vec![1.0, 1.0], base_valid: 0 });
        assert_eq!(report.items()[0].1, "1,1");
    }

    #[test]
    fn levels_larger_than_the_one_before_are_refused() {
        let data = [1.0f32; 4];
        assert!(band(&data, (2, 2), &[(3, 1)], &[2], AreaMatched::Mean, 0.0, 1.0, false).is_err());
    }
}

// SPDX-FileCopyrightText: Copyright (c) 2019-2020 ISciences, LLC.
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: Apache-2.0
//
// Adapted from exactextract's src/weighted_quantiles.cpp (ISciences),
// WeightedQuantiles::prepare and ::quantile, revision
// 76a48f00a497a76c77a821a28705026ec051191a.

/// Exactextract's interpolated quantile `q` (0 to 1) of a histogram of coverage-weighted values.
/// The input is sorted by value; equal values are combined before interpolation.
pub(crate) fn quantile(sorted: &[(f64, f64)], q: f64) -> f64 {
    let mut bins: Vec<(f64, f64)> = Vec::new();
    for &(value, weight) in sorted {
        if let Some(last) = bins.last_mut().filter(|last| last.0 == value) {
            last.1 += weight;
        } else {
            bins.push((value, weight));
        }
    }
    let n = bins.len() as f64;
    let total: f64 = bins.iter().map(|b| b.1).sum();
    let target = q * total * (n - 1.0);
    let (mut cumulative, mut previous_s, mut previous_x) = (0.0, 0.0, bins[0].0);
    for (i, &(value, weight)) in bins.iter().enumerate() {
        let s = i as f64 * weight + (n - 1.0) * cumulative;
        if s > target {
            let fraction = (target - previous_s) / (s - previous_s);
            return if previous_x.is_sign_negative() == value.is_sign_negative() {
                previous_x + fraction * (value - previous_x)
            } else {
                previous_x * (1.0 - fraction) + value * fraction
            };
        }
        previous_s = s;
        previous_x = value;
        cumulative += weight;
    }
    previous_x
}

#[cfg(test)]
mod tests {
    use super::quantile;
    fn median(sorted: &[(f64, f64)]) -> f64 {
        quantile(sorted, 0.5)
    }
    #[test]
    fn interpolation_preserves_finite_extremes_and_subnormals() {
        assert_eq!(median(&[(-f64::MAX, 1.), (f64::MAX, 1.)]), 0.);
        let tiny = f64::from_bits(1);
        assert_eq!(median(&[(tiny, 1.), (tiny, 1.)]), tiny);
        assert!(median(&[(f64::MAX / 2., 1.), (f64::MAX, 1.)]).is_finite());
    }
    /// exactextract 0.3.0, Float64 source; unequal weights and duplicate values.
    #[test]
    fn unequal_coverages_and_histogram_bins_control_interpolation() {
        assert!((median(&[(0., 0.25), (10., 1.), (20., 0.75)]) - 12.0).abs() < 1e-12);
        let repeated = median(&[(0., 0.25), (10., 0.5), (10., 0.5), (20., 0.75)]);
        assert!((repeated - 12.0).abs() < 1e-12);
        assert_eq!(median(&[(3., 0.1), (3., 0.9)]), 3.);
    }
    #[test]
    fn quantile_extremes_return_the_smallest_and_largest_values() {
        let values = [(1., 0.5), (4., 1.), (9., 0.25)];
        assert_eq!(quantile(&values, 0.), 1.);
        assert_eq!(quantile(&values, 1.), 9.);
        assert_eq!(quantile(&[(7., 0.3)], 0.25), 7.);
    }
}

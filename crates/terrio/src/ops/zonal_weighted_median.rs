// SPDX-FileCopyrightText: Copyright (c) 2019-2020 ISciences, LLC.
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: Apache-2.0
//
// Adapted from exactextract's src/weighted_quantiles.cpp (ISciences),
// WeightedQuantiles::prepare and ::quantile, revision
// 76a48f00a497a76c77a821a28705026ec051191a.

/// Exactextract's interpolated median of a histogram of coverage-weighted values.
/// The input is sorted by value; equal values are combined before interpolation.
pub(crate) fn median(sorted: &[(f64, f64)]) -> f64 {
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
    let target = 0.5 * total * (n - 1.0);
    let (mut cumulative, mut previous_s, mut previous_x) = (0.0, 0.0, bins[0].0);
    for (i, &(value, weight)) in bins.iter().enumerate() {
        let s = i as f64 * weight + (n - 1.0) * cumulative;
        if s > target {
            return previous_x + (target - previous_s) * (value - previous_x) / (s - previous_s);
        }
        previous_s = s;
        previous_x = value;
        cumulative += weight;
    }
    previous_x
}

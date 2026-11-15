//! The medoid of a zone's pixel vectors: the pixel whose band vector has the smallest sum of
//! distances to the others, under a [`MedoidMetric`].

use super::MedoidMetric;

/// The largest power of two not above `x` (for `x` in the normal range), or 1 for zero.
fn power_of_two_at_most(x: f64) -> f64 {
    if x > 0.0 {
        let exponent = (x.log2().floor() as i64).clamp(-1022, 1023);
        f64::from_bits(((exponent + 1023) as u64) << 52)
    } else {
        1.0
    }
}

/// Index of the medoid among the pixels of `values`, which holds `dim` band values per pixel, in
/// row-major order. `None` when no pixel can be the medoid (none, or only zero vectors under the
/// spectral angle). Zero vectors under the spectral angle are left out of the sums too, since
/// their angle is undefined. Ties take the earlier pixel. Costs O(n² · dim) for n pixels.
pub(crate) fn medoid(values: &[f64], dim: usize, metric: MedoidMetric) -> Option<usize> {
    let n = values.len().checked_div(dim).unwrap_or(0);
    let max_abs = |xs: &[f64]| xs.iter().fold(0.0f64, |m, x| m.max(x.abs()));
    // Rescaled copies, so squares and norms neither overflow nor underflow. Euclidean distances
    // share one factor for the zone, which leaves the medoid unchanged; spectral angles ignore a
    // pixel's length, so each pixel is scaled alone. The factors are powers of two, so scaling
    // rounds nothing and exact ties stay ties.
    let (rows, usable): (Vec<f64>, Vec<bool>) = match metric {
        MedoidMetric::Euclidean => {
            let scale = power_of_two_at_most(max_abs(values));
            (values.iter().map(|x| x / scale).collect(), vec![true; n])
        }
        MedoidMetric::SpectralAngle => {
            let mut rows = Vec::with_capacity(values.len());
            let mut usable = Vec::with_capacity(n);
            for pixel in values.chunks_exact(dim) {
                let m = max_abs(pixel);
                usable.push(m > 0.0);
                let scale = power_of_two_at_most(m);
                rows.extend(pixel.iter().map(|x| x / scale));
            }
            (rows, usable)
        }
    };
    let pixel = |i: usize| &rows[i * dim..(i + 1) * dim];
    let dot = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
    let norms: Vec<f64> = match metric {
        MedoidMetric::Euclidean => Vec::new(),
        MedoidMetric::SpectralAngle => (0..n).map(|i| dot(pixel(i), pixel(i)).sqrt()).collect(),
    };
    let candidates: Vec<usize> = (0..n).filter(|&i| usable[i]).collect();
    let mut sums = vec![0.0f64; n];
    for (k, &i) in candidates.iter().enumerate() {
        for &j in &candidates[k + 1..] {
            let d = match metric {
                MedoidMetric::Euclidean => pixel(i).iter().zip(pixel(j)).map(|(x, y)| (x - y) * (x - y)).sum::<f64>().sqrt(),
                MedoidMetric::SpectralAngle => (dot(pixel(i), pixel(j)) / (norms[i] * norms[j])).clamp(-1.0, 1.0).acos(),
            };
            sums[i] += d;
            sums[j] += d;
        }
    }
    candidates.into_iter().reduce(|best, i| if sums[i] < sums[best] { i } else { best })
}

#[cfg(test)]
mod tests {
    use super::MedoidMetric::*;
    use super::medoid;

    #[test]
    fn euclidean_picks_the_most_central_pixel() {
        // Pixels (0,0), (1,0), (2,0): the middle one is closest to both ends.
        assert_eq!(medoid(&[0.0, 0.0, 1.0, 0.0, 2.0, 0.0], 2, Euclidean), Some(1));
        // Ties (two pixels, equal sums) keep the first.
        assert_eq!(medoid(&[0.0, 0.0, 3.0, 4.0], 2, Euclidean), Some(0));
        assert_eq!(medoid(&[], 2, Euclidean), None);
    }

    #[test]
    fn spectral_angle_ignores_brightness_and_skips_zero_vectors() {
        // (1,1) and (2,2) have the same direction, so each is at angle 0 from the other; (0,5)
        // is far from both. The zero vector has no angle and is never the medoid. The first two
        // tie, so the first wins.
        assert_eq!(medoid(&[1.0, 1.0, 0.0, 0.0, 2.0, 2.0, 0.0, 5.0], 2, SpectralAngle), Some(0));
        assert_eq!(medoid(&[0.0, 0.0], 2, SpectralAngle), None);
    }

    #[test]
    fn euclidean_is_range_safe() {
        // Squaring 1e200 overflows, and 1e-200 underflows: the middle value is the medoid at both.
        assert_eq!(medoid(&[0., 1e200, 2e200], 1, Euclidean), Some(1));
        assert_eq!(medoid(&[0., 1e-200, 2e-200], 1, Euclidean), Some(1));
    }

    #[test]
    fn spectral_angle_and_euclidean_can_pick_different_pixels() {
        // (1,0), (0,1), (4,1). Euclidean sums are 4.58, 5.41, 7.16: the first. Spectral angle sums
        // (radians) are 1.82, 2.90, 1.57: the third, which is closest to the others in direction.
        let values = [1., 0., 0., 1., 4., 1.];
        assert_eq!(medoid(&values, 2, Euclidean), Some(0));
        assert_eq!(medoid(&values, 2, SpectralAngle), Some(2));
    }

    #[test]
    fn spectral_angle_is_range_safe() {
        // (1e200, 0) and (2e200, 0) point the same way, so the first pixel, orthogonal to both,
        // has the largest sum and the medoid is the first of the parallel pair.
        assert_eq!(medoid(&[0., 1e200, 1e200, 0., 2e200, 0.], 2, SpectralAngle), Some(1));
        assert_eq!(medoid(&[0., 1e-200, 1e-200, 0., 2e-200, 0.], 2, SpectralAngle), Some(1));
    }
}

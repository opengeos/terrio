//! The medoid of a zone's pixel vectors: the pixel whose band vector has the smallest sum of
//! distances to the others, under a [`MedoidMetric`].

use super::MedoidMetric;

/// Index of the medoid among the pixels of `values`, which holds `dim` band values per pixel, in
/// row-major order. `None` when no pixel can be the medoid (none, or only zero vectors under the
/// spectral angle). Ties take the earlier pixel. Costs O(n² · dim) for n pixels.
pub(crate) fn medoid(values: &[f64], dim: usize, metric: MedoidMetric) -> Option<usize> {
    let n = values.len().checked_div(dim).unwrap_or(0);
    let pixel = |i: usize| &values[i * dim..(i + 1) * dim];
    let dot = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
    let norms: Vec<f64> = match metric {
        MedoidMetric::Euclidean => Vec::new(),
        MedoidMetric::SpectralAngle => (0..n).map(|i| dot(pixel(i), pixel(i)).sqrt()).collect(),
    };
    let candidates: Vec<usize> = match metric {
        MedoidMetric::Euclidean => (0..n).collect(),
        MedoidMetric::SpectralAngle => (0..n).filter(|&i| norms[i] > 0.0).collect(),
    };
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
        // is far from both. The zero vector has no angle and is never the medoid.
        assert_eq!(medoid(&[1.0, 1.0, 0.0, 0.0, 2.0, 2.0, 0.0, 5.0], 2, SpectralAngle), Some(0));
        assert_eq!(medoid(&[0.0, 0.0], 2, SpectralAngle), None);
    }
}

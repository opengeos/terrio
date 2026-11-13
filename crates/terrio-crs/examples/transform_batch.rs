//! Times transforming 100,000 points one by one and as a slice, for a few pairs of CRSs.
//!
//! ```sh
//! cargo run --release -p terrio-crs --example transform_batch
//! ```

use std::time::Instant;
use terrio_crs::{SpatialRef, Transformer};

fn main() {
    const N: usize = 100_000;
    let pairs = [
        ("EPSG:32632 -> EPSG:4326", 32632, 4326, (300_000.0, 5_000_000.0), (400_000.0, 400_000.0)),
        ("EPSG:3857 -> EPSG:4326", 3857, 4326, (500_000.0, 5_500_000.0), (500_000.0, 500_000.0)),
        ("EPSG:2056 -> EPSG:4258", 2056, 4258, (2_500_000.0, 1_100_000.0), (300_000.0, 200_000.0)),
        ("EPSG:4326 -> EPSG:4326", 4326, 4326, (5.0, 45.0), (5.0, 5.0)),
    ];
    for (name, src, dst, origin, size) in pairs {
        let t = Transformer::new(&SpatialRef::from_epsg(src).unwrap(), &SpatialRef::from_epsg(dst).unwrap()).unwrap();
        let points: Vec<(f64, f64)> = (0..N)
            .map(|i| {
                let f = i as f64 / N as f64;
                (origin.0 + size.0 * f, origin.1 + size.1 * ((i * 7919) % N) as f64 / N as f64)
            })
            .collect();
        // Warm up the caches the first points fill.
        let _ = t.transform(points[0].0, points[0].1);

        // The best of five runs of each, the copies made outside the timed part.
        let (mut per_point, mut batch) = (f64::INFINITY, f64::INFINITY);
        let (mut one, mut slice, mut failed) = (Vec::new(), Vec::new(), 0);
        for _ in 0..5 {
            one = points.clone();
            let start = Instant::now();
            for p in one.iter_mut() {
                *p = t.transform(p.0, p.1).unwrap_or((f64::NAN, f64::NAN));
            }
            per_point = per_point.min(start.elapsed().as_secs_f64());

            slice = points.clone();
            let start = Instant::now();
            failed = t.transform_slice_or_nan(&mut slice).unwrap();
            batch = batch.min(start.elapsed().as_secs_f64());
        }

        let same = one.iter().zip(&slice).all(|(a, b)| a.0.to_bits() == b.0.to_bits() && a.1.to_bits() == b.1.to_bits());
        println!(
            "{name:24} point by point {:7.3} µs/pt   slice {:7.3} µs/pt   failed {failed}   identical {same}",
            per_point * 1e6 / N as f64,
            batch * 1e6 / N as f64,
        );
    }
}

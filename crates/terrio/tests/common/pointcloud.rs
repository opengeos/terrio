// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Fixtures shared by the point cloud tests.
#![allow(dead_code)] // Each test file uses a subset.

use terrio::crs::SpatialRef;
use terrio::pointcloud::{self, Dimension, PointCloud, Values};

#[path = "test_data.rs"]
mod test_data;

/// The path of `name` in `TERRIO_TEST_DATA/pointcloud` (see [`test_data::data`]).
pub fn data(name: &str) -> Option<String> {
    test_data::data(&format!("pointcloud/{name}"))
}

/// A deterministic pseudo-random sequence in [0, 1).
pub fn noise(n: usize, seed: u64) -> Vec<f64> {
    let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (s >> 11) as f64 / (1u64 << 53) as f64
        })
        .collect()
}

/// Points on the 0.01 grid over 100 x 100 m, with intensities, classes, returns and GPS times.
pub fn cloud(n: usize, seed: u64) -> PointCloud {
    let r = noise(n * 4, seed);
    let q = |v: f64| (v * 100.0).round() / 100.0;
    let dims = vec![
        Dimension::new("x", Values::F64((0..n).map(|i| q(500_000.0 + r[i] * 100.0)).collect())),
        Dimension::new("y", Values::F64((0..n).map(|i| q(4_000_000.0 + r[n + i] * 100.0)).collect())),
        Dimension::new("z", Values::F64((0..n).map(|i| q(100.0 + r[2 * n + i] * 10.0)).collect())),
        Dimension::new("intensity", Values::U16((0..n).map(|i| (i % 1000) as u16).collect())),
        Dimension::new("return_number", Values::U8(vec![1; n])),
        Dimension::new("number_of_returns", Values::U8(vec![1; n])),
        Dimension::new("classification", Values::U8((0..n).map(|i| (i % 7) as u8).collect())),
        Dimension::new("gps_time", Values::F64((0..n).map(|i| 1e5 + r[3 * n + i]).collect())),
    ];
    pointcloud::from_dimensions(dims, SpatialRef::from_epsg(32617).unwrap())
}

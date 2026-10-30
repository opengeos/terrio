// SPDX-FileCopyrightText: Copyright (c) 2010, Andrew Bell
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT AND BSD-3-Clause
//
// Derived from (translated to Rust and modified):
//   PDAL 2.10.2:
//     filters/ChipperFilter.cpp

//! Filters that split points into tiles: PDAL's `filters.splitter` (a square grid) and
//! `filters.chipper` (squarish chips of at most a number of points).

use crate::pointcloud::PointCloud;
use crate::{Error, Result};
use std::collections::HashMap;

/// How to split points into squares (PDAL's `filters.splitter`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SplitterOptions {
    /// The side of the squares. 1000 by default.
    pub length: f64,
    /// The squares' origin. By default, the first point's x and y.
    pub origin: Option<[f64; 2]>,
    /// A buffer around each square whose points it takes too (less than half the length).
    pub buffer: f64,
}

impl Default for SplitterOptions {
    fn default() -> Self {
        Self { length: 1000.0, origin: None, buffer: 0.0 }
    }
}

/// Splits points into squares, as PDAL's `filters.splitter`: one cloud per square with points,
/// in the order of their first points, with the square's column and row from the origin.
pub fn splitter(cloud: &PointCloud, o: &SplitterOptions) -> Result<Vec<((i32, i32), PointCloud)>> {
    if o.length.is_nan() || o.length <= 0.0 {
        return Err(Error::InvalidArgument("the length must be positive".into()));
    }
    if o.buffer >= o.length / 2.0 {
        return Err(Error::InvalidArgument(format!("buffer ({}) must be less than half of length ({})", o.buffer, o.length)));
    }
    if cloud.is_empty() {
        return Ok(Vec::new());
    }
    let (xs, ys, _) = super::xyz(cloud)?;
    let [x0, y0] = o.origin.unwrap_or([xs[0], ys[0]]);
    let len = o.length;
    let mut order: Vec<(i32, i32)> = Vec::new();
    let mut members: HashMap<(i32, i32), Vec<usize>> = HashMap::new();
    let mut add = |key: (i32, i32), i: usize| {
        members
            .entry(key)
            .or_insert_with(|| {
                order.push(key);
                Vec::new()
            })
            .push(i);
    };
    let contains = |xp: i32, yp: i32, x: f64, y: f64| {
        let minx = x0 + xp as f64 * len - o.buffer;
        let maxx = minx + len + 2.0 * o.buffer;
        let miny = y0 + yp as f64 * len - o.buffer;
        let maxy = miny + len + 2.0 * o.buffer;
        minx < x && x < maxx && miny < y && y < maxy
    };
    for i in 0..cloud.len() {
        let (x, y) = (xs[i], ys[i]);
        let (dx, dy) = (x - x0, y - y0);
        let mut xp = (dx / len) as i32;
        if dx < 0.0 {
            xp -= 1;
        }
        let mut yp = (dy / len) as i32;
        if dy < 0.0 {
            yp -= 1;
        }
        add((xp, yp), i);
        if o.buffer > 0.0 {
            if contains(xp - 1, yp, x, y) {
                add((xp - 1, yp), i);
            } else if contains(xp + 1, yp, x, y) {
                add((xp + 1, yp), i);
            }
            if contains(xp, yp - 1, x, y) {
                add((xp, yp - 1), i);
            } else if contains(xp, yp + 1, x, y) {
                add((xp, yp + 1), i);
            }
            for (a, b) in [(-1, -1), (-1, 1), (1, -1), (1, 1)] {
                if contains(xp + a, yp + b, x, y) {
                    add((xp + a, yp + b), i);
                    break;
                }
            }
        }
    }
    Ok(order.into_iter().map(|k| (k, cloud.take(&members[&k]))).collect())
}

/// A point's coordinate along one axis, with its index and its position in the other axis'
/// sorted list.
#[derive(Debug, Clone, Copy, Default)]
struct ChipRef {
    pos: f64,
    point: usize,
    other: usize,
}

struct Chipper {
    partitions: Vec<usize>,
    chips: Vec<Vec<usize>>,
}

impl Chipper {
    fn decide(&mut self, v1: &mut [ChipRef], v2: &mut [ChipRef], spare: &mut [ChipRef], pleft: usize, pright: usize) {
        let left = self.partitions[pleft];
        let right = self.partitions[pright] - 1;
        let r1 = v1[right].pos - v1[left].pos;
        let r2 = v2[right].pos - v2[left].pos;
        if r1 > r2 {
            self.split(v1, v2, spare, pleft, pright);
        } else {
            self.split(v2, v1, spare, pleft, pright);
        }
    }

    fn split(&mut self, wide: &mut [ChipRef], narrow: &mut [ChipRef], spare: &mut [ChipRef], pleft: usize, pright: usize) {
        let left = self.partitions[pleft];
        let right = self.partitions[pright] - 1;
        if pright - pleft == 1 {
            self.emit(wide, left, right);
        } else if pright - pleft == 2 {
            let center = self.partitions[pright - 1];
            self.emit(wide, left, center - 1);
            self.emit(wide, center, right);
        } else {
            let pcenter = (pleft + pright) / 2;
            let center = self.partitions[pcenter];
            let (mut lstart, mut rstart) = (left, center);
            for &r in &narrow[left..=right] {
                if r.other < center {
                    spare[lstart] = r;
                    wide[r.other].other = lstart;
                    lstart += 1;
                } else {
                    spare[rstart] = r;
                    wide[r.other].other = rstart;
                    rstart += 1;
                }
            }
            self.decide(wide, spare, narrow, pleft, pcenter);
            self.decide(wide, spare, narrow, pcenter, pright);
        }
    }

    fn emit(&mut self, wide: &[ChipRef], lo: usize, hi: usize) {
        self.chips.push(wide[lo..=hi].iter().map(|r| r.point).collect());
    }
}

/// Splits points into contiguous, squarish chips of at most `capacity` points, as PDAL's
/// `filters.chipper` (5000 by default), in its order.
pub fn chipper(cloud: &PointCloud, capacity: usize) -> Result<Vec<PointCloud>> {
    if capacity == 0 {
        return Err(Error::InvalidArgument("the capacity must be positive".into()));
    }
    let n = cloud.len();
    if n == 0 {
        return Ok(Vec::new());
    }
    let (xs, ys, _) = super::xyz(cloud)?;
    let mut xv: Vec<ChipRef> = (0..n).map(|i| ChipRef { pos: xs[i], point: i, other: 0 }).collect();
    let mut yv: Vec<ChipRef> = (0..n).map(|i| ChipRef { pos: ys[i], point: i, other: 0 }).collect();
    // Stable sorts, as std::stable_sort; NaNs compare as C++'s `<` does (never less).
    let less = |a: &ChipRef, b: &ChipRef| {
        if a.pos < b.pos {
            std::cmp::Ordering::Less
        } else if b.pos < a.pos {
            std::cmp::Ordering::Greater
        } else {
            std::cmp::Ordering::Equal
        }
    };
    xv.sort_by(less);
    for (i, r) in xv.iter().enumerate() {
        yv[r.point].other = i;
    }
    yv.sort_by(less);
    for i in 0..n {
        xv[yv[i].other].other = i;
    }
    // The partitions: sizes of about `capacity`, the extra points spread among them.
    let parts = n.div_ceil(capacity);
    let size = n as f64 / parts as f64;
    let mut partitions = vec![0usize];
    let mut total = 0.0;
    for _ in 0..parts {
        total += size;
        partitions.push((total + 0.5).floor() as usize);
    }
    let mut c = Chipper { partitions, chips: Vec::new() };
    let mut spare = vec![ChipRef::default(); n];
    let last = c.partitions.len() - 1;
    c.decide(&mut xv, &mut yv, &mut spare, 0, last);
    Ok(c.chips.iter().map(|idx| cloud.take(idx)).collect())
}

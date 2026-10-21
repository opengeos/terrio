//! Ground classification by cloth simulation (PDAL's `filters.csf`, Zhang et al. 2016): the
//! points are turned upside down and a cloth of particles, joined by springs, falls onto them
//! under gravity; the points close to where the cloth rests are ground.
//!
//! A port of the CSF code PDAL vendors, step for step and in its order, so that the classes are
//! PDAL's: the particles move only vertically (only their heights are kept), and the springs are
//! relaxed particle after particle, in place.
//!
//! The CSF code this is ported from (and changed, to Rust) is Copyright 2017 State Key Laboratory
//! of Remote Sensing Science, Institute of Remote Sensing Science and Engineering, Beijing Normal
//! University, licensed under the Apache License, Version 2.0
//! (<http://www.apache.org/licenses/LICENSE-2.0>); see `NOTICE`.

use super::{DimRange, set_classes, xyz};
use crate::pointcloud::{PointCloud, ReturnKind};
use crate::{Error, Result};
use std::collections::VecDeque;

/// How to classify ground points with the cloth simulation filter (PDAL's `filters.csf`).
#[derive(Debug, Clone, PartialEq)]
pub struct CsfOptions {
    /// Handle steep slopes after the simulation. True by default.
    pub smooth: bool,
    /// The time step. 0.65 by default.
    pub step: f64,
    /// The largest distance of a ground point from the cloth. 0.5 by default.
    pub threshold: f64,
    /// The height difference between neighbouring cloth particles for slope handling. 0.3 by
    /// default.
    pub hdiff: f64,
    /// The cloth's resolution (the distance between particles). 1 by default.
    pub resolution: f64,
    /// The cloth's rigidness (1 to 3). 3 by default.
    pub rigidness: usize,
    /// The most simulation steps. 500 by default.
    pub iterations: usize,
    /// Points within these ranges are left alone (PDAL's `ignore`).
    pub ignore: Vec<DimRange>,
    /// The returns classified. Last and only by default.
    pub returns: Vec<ReturnKind>,
    /// The classification of ground points. 2 by default.
    pub ground_class: u8,
    /// The classification of the other points classified. 1 by default.
    pub other_class: u8,
    /// Change the classification of ground points only.
    pub only_ground: bool,
}

impl Default for CsfOptions {
    fn default() -> Self {
        Self {
            smooth: true,
            step: 0.65,
            threshold: 0.5,
            hdiff: 0.3,
            resolution: 1.0,
            rigidness: 3,
            iterations: 500,
            ignore: Vec::new(),
            returns: vec![ReturnKind::Last, ReturnKind::Only],
            ground_class: 2,
            other_class: 1,
            only_ground: false,
        }
    }
}

const SINGLE_MOVE: [f64; 15] =
    [0.0, 0.3, 0.51, 0.657, 0.7599, 0.83193, 0.88235, 0.91765, 0.94235, 0.95965, 0.97175, 0.98023, 0.98616, 0.99031, 0.99322];
const DOUBLE_MOVE: [f64; 15] = [0.0, 0.3, 0.42, 0.468, 0.4872, 0.4949, 0.498, 0.4992, 0.4997, 0.4999, 0.4999, 0.5, 0.5, 0.5, 0.5];
const MIN_INF: f64 = -9_999_999_999.0;
const MAX_INF: f64 = 9_999_999_999.0;
const DAMPING: f64 = 0.01;
/// Connected movable particles fewer than this are not slope-handled.
const MAX_PARTICLE_FOR_POSTPROCESSING: usize = 50;

/// The cloth: particles on a grid (index `y × width + x`), their heights and springs.
struct Cloth {
    width: usize,
    height: usize,
    origin: [f64; 3],
    step: f64,
    /// The particles' x and z (fixed) and heights (current and previous).
    px: Vec<f64>,
    pz: Vec<f64>,
    y: Vec<f64>,
    old_y: Vec<f64>,
    movable: Vec<bool>,
    visited: Vec<bool>,
    /// The position of a particle in its connected component (slope handling).
    c_pos: Vec<usize>,
    neighbors: Vec<Vec<u32>>,
    nearest_height: Vec<f64>,
    tmp_dist: Vec<f64>,
    heights: Vec<f64>,
    acceleration: f64,
    time_step2: f64,
    rigidness: usize,
    smooth_threshold: f64,
    height_threshold: f64,
}

impl Cloth {
    #[allow(clippy::too_many_arguments)]
    fn new(
        origin: [f64; 3],
        width: usize,
        height: usize,
        step: f64,
        smooth_threshold: f64,
        height_threshold: f64,
        rigidness: usize,
        time_step: f64,
    ) -> Self {
        let n = width * height;
        let mut px = vec![0.0; n];
        let mut pz = vec![0.0; n];
        for i in 0..width {
            for j in 0..height {
                px[j * width + i] = origin[0] + i as f64 * step;
                pz[j * width + i] = origin[2] + j as f64 * step;
            }
        }
        let mut c = Self {
            width,
            height,
            origin,
            step,
            px,
            pz,
            y: vec![origin[1]; n],
            old_y: vec![origin[1]; n],
            movable: vec![true; n],
            visited: vec![false; n],
            c_pos: vec![0; n],
            neighbors: vec![Vec::new(); n],
            nearest_height: vec![MIN_INF; n],
            tmp_dist: vec![MAX_INF; n],
            heights: Vec::new(),
            acceleration: 0.0,
            time_step2: time_step * time_step,
            rigidness,
            smooth_threshold,
            height_threshold,
        };
        let idx = |x: usize, y: usize| y * width + x;
        let mut link = |a: usize, b: usize| {
            c.neighbors[a].push(b as u32);
            c.neighbors[b].push(a as u32);
        };
        for x in 0..width {
            for y in 0..height {
                if x + 1 < width {
                    link(idx(x, y), idx(x + 1, y));
                }
                if y + 1 < height {
                    link(idx(x, y), idx(x, y + 1));
                }
                if x + 1 < width && y + 1 < height {
                    link(idx(x, y), idx(x + 1, y + 1));
                    link(idx(x + 1, y), idx(x, y + 1));
                }
            }
        }
        for x in 0..width {
            for y in 0..height {
                if x + 2 < width {
                    link(idx(x, y), idx(x + 2, y));
                }
                if y + 2 < height {
                    link(idx(x, y), idx(x, y + 2));
                }
                if x + 2 < width && y + 2 < height {
                    link(idx(x, y), idx(x + 2, y + 2));
                    link(idx(x + 2, y), idx(x, y + 2));
                }
            }
        }
        c
    }

    fn offset(&mut self, i: usize, dy: f64) {
        if self.movable[i] {
            self.y[i] += dy;
        }
    }

    /// One step: Verlet integration of every particle, then the springs relaxed particle after
    /// particle. The largest height change of the movable particles.
    fn time_step(&mut self) -> f64 {
        for i in 0..self.y.len() {
            if self.movable[i] {
                let temp = self.y[i];
                self.y[i] = self.y[i] + (self.y[i] - self.old_y[i]) * (1.0 - DAMPING) + self.acceleration * self.time_step2;
                self.old_y[i] = temp;
            }
        }
        let r = self.rigidness;
        let (double, single) = if r > 14 { (0.5, 1.0) } else { (DOUBLE_MOVE[r], SINGLE_MOVE[r]) };
        for p1 in 0..self.y.len() {
            for k in 0..self.neighbors[p1].len() {
                let p2 = self.neighbors[p1][k] as usize;
                let correction = self.y[p2] - self.y[p1];
                match (self.movable[p1], self.movable[p2]) {
                    (true, true) => {
                        let half = correction * double;
                        self.offset(p1, half);
                        self.offset(p2, -half);
                    }
                    (true, false) => self.offset(p1, correction * single),
                    (false, true) => self.offset(p2, -(correction * single)),
                    (false, false) => {}
                }
            }
        }
        let mut max_diff = 0.0f64;
        for i in 0..self.y.len() {
            if self.movable[i] {
                let d = (self.old_y[i] - self.y[i]).abs();
                if d > max_diff {
                    max_diff = d;
                }
            }
        }
        max_diff
    }

    /// Particles that fell below the surface go back onto it and stop.
    fn terrain_collision(&mut self) {
        for i in 0..self.y.len() {
            let v = self.y[i];
            if v < self.heights[i] {
                self.offset(i, self.heights[i] - v);
                self.movable[i] = false;
            }
        }
    }

    /// The surface's height under a particle without points: the first along its row, then its
    /// column, then the nearest by its springs.
    fn height_by_scanline(&mut self, p: usize) -> f64 {
        let (x, y) = (p % self.width, p / self.width);
        for i in x + 1..self.width {
            let h = self.nearest_height[y * self.width + i];
            if h > MIN_INF {
                return h;
            }
        }
        for i in (0..x).rev() {
            let h = self.nearest_height[y * self.width + i];
            if h > MIN_INF {
                return h;
            }
        }
        for j in (0..y).rev() {
            let h = self.nearest_height[j * self.width + x];
            if h > MIN_INF {
                return h;
            }
        }
        for j in y + 1..self.height {
            let h = self.nearest_height[j * self.width + x];
            if h > MIN_INF {
                return h;
            }
        }
        self.height_by_neighbor(p)
    }

    fn height_by_neighbor(&mut self, p: usize) -> f64 {
        let mut queue: VecDeque<usize> = VecDeque::new();
        let mut back: Vec<usize> = Vec::new();
        for k in 0..self.neighbors[p].len() {
            self.visited[p] = true;
            queue.push_back(self.neighbors[p][k] as usize);
        }
        while let Some(q) = queue.pop_front() {
            back.push(q);
            if self.nearest_height[q] > MIN_INF {
                for &b in &back {
                    self.visited[b] = false;
                }
                while let Some(r) = queue.pop_front() {
                    self.visited[r] = false;
                }
                return self.nearest_height[q];
            }
            for k in 0..self.neighbors[q].len() {
                let t = self.neighbors[q][k] as usize;
                if !self.visited[t] {
                    self.visited[t] = true;
                    queue.push_back(t);
                }
            }
        }
        MIN_INF
    }

    /// The surface under the cloth: the height of the point nearest each particle (points are
    /// assigned to the nearest particle), else found from the neighbouring particles.
    fn rasterize(&mut self, points: &[[f64; 3]]) {
        for p in points {
            let dx = p[0] - self.origin[0];
            let dz = p[2] - self.origin[2];
            let col = (dx / self.step + 0.5) as i64;
            let row = (dz / self.step + 0.5) as i64;
            if col >= 0 && row >= 0 {
                let k = row as usize * self.width + col as usize;
                let d = (p[0] - self.px[k]) * (p[0] - self.px[k]) + (p[2] - self.pz[k]) * (p[2] - self.pz[k]);
                if d < self.tmp_dist[k] {
                    self.tmp_dist[k] = d;
                    self.nearest_height[k] = p[1];
                }
            }
        }
        let n = self.y.len();
        let mut heights = vec![0.0; n];
        for (i, h) in heights.iter_mut().enumerate() {
            let nearest = self.nearest_height[i];
            *h = if nearest > MIN_INF { nearest } else { self.height_by_scanline(i) };
        }
        self.heights = heights;
    }

    /// Slope handling: movable particles of large connected areas, next to stopped ones on a
    /// smooth surface, are put on the surface, spreading through the area.
    fn movable_filter(&mut self) {
        let w = self.width;
        for x in 0..self.width {
            for y in 0..self.height {
                let start = y * w + x;
                if !self.movable[start] || self.visited[start] {
                    continue;
                }
                let mut queue: VecDeque<usize> = VecDeque::new();
                let mut connected: Vec<(usize, usize)> = vec![(x, y)];
                let mut neighbors: Vec<Vec<usize>> = Vec::new();
                let mut sum = 1usize;
                self.visited[start] = true;
                queue.push_back(start);
                while let Some(f) = queue.pop_front() {
                    let (cx, cy) = (f % w, f / w);
                    let mut nb = Vec::new();
                    let mut candidates: Vec<(usize, usize)> = Vec::with_capacity(4);
                    if cx > 0 {
                        candidates.push((cx - 1, cy));
                    }
                    if cx + 1 < self.width {
                        candidates.push((cx + 1, cy));
                    }
                    if cy > 0 {
                        candidates.push((cx, cy - 1));
                    }
                    if cy + 1 < self.height {
                        candidates.push((cx, cy + 1));
                    }
                    for (nx, ny) in candidates {
                        let k = ny * w + nx;
                        if self.movable[k] {
                            if !self.visited[k] {
                                sum += 1;
                                self.visited[k] = true;
                                connected.push((nx, ny));
                                queue.push_back(k);
                                nb.push(sum - 1);
                                self.c_pos[k] = sum - 1;
                            } else {
                                nb.push(self.c_pos[k]);
                            }
                        }
                    }
                    neighbors.push(nb);
                }
                if sum > MAX_PARTICLE_FOR_POSTPROCESSING {
                    let edges = self.find_unmovable(&connected);
                    self.handle_slope_connected(&edges, &connected, &neighbors);
                }
            }
        }
    }

    fn find_unmovable(&mut self, connected: &[(usize, usize)]) -> Vec<usize> {
        let w = self.width;
        let mut edges = Vec::new();
        for (i, &(x, y)) in connected.iter().enumerate() {
            let index = y * w + x;
            let mut refs: Vec<usize> = Vec::with_capacity(4);
            if x > 0 {
                refs.push(y * w + x - 1);
            }
            if x + 1 < self.width {
                refs.push(y * w + x + 1);
            }
            if y > 0 {
                refs.push((y - 1) * w + x);
            }
            if y + 1 < self.height {
                refs.push((y + 1) * w + x);
            }
            for r in refs {
                if !self.movable[r]
                    && (self.heights[index] - self.heights[r]).abs() < self.smooth_threshold
                    && self.y[index] - self.heights[index] < self.height_threshold
                {
                    let dy = self.heights[index] - self.y[index];
                    self.offset(index, dy);
                    self.movable[index] = false;
                    edges.push(i);
                    break;
                }
            }
        }
        edges
    }

    fn handle_slope_connected(&mut self, edges: &[usize], connected: &[(usize, usize)], neighbors: &[Vec<usize>]) {
        let w = self.width;
        let mut visited = vec![false; connected.len()];
        let mut queue: VecDeque<usize> = VecDeque::new();
        for &e in edges {
            queue.push_back(e);
            visited[e] = true;
        }
        while let Some(index) = queue.pop_front() {
            let center = connected[index].1 * w + connected[index].0;
            for &nb in &neighbors[index] {
                let k = connected[nb].1 * w + connected[nb].0;
                if (self.heights[center] - self.heights[k]).abs() < self.smooth_threshold
                    && (self.y[k] - self.heights[k]).abs() < self.height_threshold
                {
                    let dy = self.heights[k] - self.y[k];
                    self.offset(k, dy);
                    self.movable[k] = false;
                    if !visited[nb] {
                        queue.push_back(nb);
                        visited[nb] = true;
                    }
                }
            }
        }
    }
}

/// Classifies ground points with the cloth simulation filter, as PDAL's `filters.csf`.
pub fn csf(cloud: &PointCloud, options: &CsfOptions) -> Result<PointCloud> {
    let classes = csf_classes(cloud, options)?;
    let mut out = cloud.clone();
    set_classes(&mut out, |i| classes[i]);
    Ok(out)
}

/// The classification CSF gives each point (`None`: left alone).
pub fn csf_classes(cloud: &PointCloud, o: &CsfOptions) -> Result<Vec<Option<u8>>> {
    if !(o.resolution > 0.0 && o.resolution.is_finite()) {
        return Err(Error::InvalidArgument(format!("invalid resolution {}", o.resolution)));
    }
    if o.ground_class == o.other_class && !o.only_ground {
        return Err(Error::InvalidArgument("the ground and other classes cannot be equal unless only ground is set".into()));
    }
    let mut out = vec![None; cloud.len()];
    if cloud.is_empty() {
        return Ok(out);
    }
    let (xs, ys, zs) = xyz(cloud)?;
    let points_idx = super::ground::classified_points(cloud, &o.ignore, &o.returns)?;
    // Upside down: (x, -z, y), y up.
    let pts: Vec<[f64; 3]> = points_idx.iter().map(|&i| [xs[i], -zs[i], ys[i]]).collect();
    let (mut lo, mut hi) = (pts[0], pts[0]);
    for p in &pts[1..] {
        for d in 0..3 {
            if p[d] < lo[d] {
                lo[d] = p[d];
            } else if p[d] > hi[d] {
                hi[d] = p[d];
            }
        }
    }
    let res = o.resolution;
    let buffer = 2.0;
    let origin = [lo[0] - buffer * res, hi[1] + 0.05, lo[2] - buffer * res];
    let width = ((hi[0] - lo[0]) / res).floor() as usize + 4;
    let height = ((hi[2] - lo[2]) / res).floor() as usize + 4;
    // About 150 bytes per particle (its heights, flags and neighbour list): 15 GB at the limit.
    if width.saturating_mul(height) > 100_000_000 {
        return Err(Error::InvalidArgument(format!("cloth of {width} x {height} particles: use a coarser resolution")));
    }
    let mut cloth = Cloth::new(origin, width, height, res, o.hdiff, 9999.0, o.rigidness, o.step);
    cloth.rasterize(&pts);
    let gravity = 0.2;
    cloth.acceleration = -gravity * (o.step * o.step);
    for _ in 0..o.iterations {
        let max_diff = cloth.time_step();
        cloth.terrain_collision();
        if max_diff != 0.0 && max_diff < 0.005 {
            break;
        }
    }
    if o.smooth {
        cloth.movable_filter();
    }
    // Distances to the cloth, interpolated bilinearly.
    for (k, p) in pts.iter().enumerate() {
        let dx = p[0] - cloth.origin[0];
        let dz = p[2] - cloth.origin[2];
        let col0 = (dx / cloth.step) as usize;
        let row0 = (dz / cloth.step) as usize;
        let sx = (dx - col0 as f64 * cloth.step) / cloth.step;
        let sz = (dz - row0 as f64 * cloth.step) / cloth.step;
        let at = |c: usize, r: usize| cloth.y[r * cloth.width + c];
        let fxy = at(col0, row0) * (1.0 - sx) * (1.0 - sz)
            + at(col0, row0 + 1) * (1.0 - sx) * sz
            + at(col0 + 1, row0 + 1) * sx * sz
            + at(col0 + 1, row0) * sx * (1.0 - sz);
        let ground = (fxy - p[1]).abs() < o.threshold;
        let i = points_idx[k];
        if ground {
            out[i] = Some(o.ground_class);
        } else if !o.only_ground {
            out[i] = Some(o.other_class);
        }
    }
    Ok(out)
}

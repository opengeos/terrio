// SPDX-FileCopyrightText: Copyright 2008-2009  Marius Muja (mariusm@cs.ubc.ca). All rights reserved.
// SPDX-FileCopyrightText: Copyright 2008-2009  David G. Lowe (lowe@cs.ubc.ca). All rights reserved.
// SPDX-FileCopyrightText: Copyright 2011-2016  Jose Luis Blanco (joseluisblancoc@gmail.com). All rights reserved.
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT AND BSD-2-Clause
//
// Derived from (translated to Rust and modified):
//   nanoflann, as vendored by PDAL 2.10.2:
//     vendor/nanoflann/nanoflann.hpp

//! A static k-d tree over 2D or 3D points, for nearest-neighbour and radius searches.

/// The most points in a leaf.
const LEAF: usize = 16;

/// A node: a leaf (a range of `order`) or a split.
#[derive(Debug, Clone, Copy)]
enum Node {
    Leaf { start: u32, end: u32 },
    Split { dim: u8, value: f64, left: u32, right: u32 },
}

/// A k-d tree of `D`-dimensional points.
#[derive(Debug, Clone)]
pub(crate) struct KdTree<const D: usize> {
    points: Vec<[f64; D]>,
    order: Vec<u32>,
    nodes: Vec<Node>,
}

impl<const D: usize> KdTree<D> {
    /// Builds the tree of points.
    pub fn new(points: Vec<[f64; D]>) -> Self {
        let mut order: Vec<u32> = (0..points.len() as u32).collect();
        let mut nodes = Vec::new();
        if !points.is_empty() {
            let n = order.len();
            build(&points, &mut order, 0, n, &mut nodes);
        }
        Self { points, order, nodes }
    }

    /// The point at an index.
    pub fn point(&self, i: usize) -> &[f64; D] {
        &self.points[i]
    }

    /// The `k` nearest points to `q`: `(squared distance, index)`, nearest first (ties by index).
    pub fn knn(&self, q: &[f64; D], k: usize, out: &mut Vec<(f64, u32)>) {
        out.clear();
        if self.nodes.is_empty() || k == 0 {
            return;
        }
        self.knn_node(0, q, k, out);
    }

    fn knn_node(&self, node: usize, q: &[f64; D], k: usize, out: &mut Vec<(f64, u32)>) {
        match self.nodes[node] {
            Node::Leaf { start, end } => {
                for &i in &self.order[start as usize..end as usize] {
                    let d = dist2(&self.points[i as usize], q);
                    if out.len() == k && !less(d, i, out[k - 1]) {
                        continue;
                    }
                    let at = out.partition_point(|e| less(e.0, e.1, (d, i)));
                    out.insert(at, (d, i));
                    out.truncate(k);
                }
            }
            Node::Split { dim, value, left, right } => {
                let diff = q[dim as usize] - value;
                let (near, far) = if diff < 0.0 { (left, right) } else { (right, left) };
                self.knn_node(near as usize, q, k, out);
                if out.len() < k || diff * diff <= out[k - 1].0 {
                    self.knn_node(far as usize, q, k, out);
                }
            }
        }
    }

    /// The number of points closer than `r` to `q` (squared distance below `r²`).
    pub fn count_within(&self, q: &[f64; D], r: f64) -> usize {
        let mut n = 0;
        self.within(q, r, &mut |_| n += 1);
        n
    }

    /// Calls `f` with the index of each point closer than `r` to `q`.
    pub fn within(&self, q: &[f64; D], r: f64, f: &mut impl FnMut(u32)) {
        if !self.nodes.is_empty() {
            self.within_node(0, q, r * r, f);
        }
    }

    /// Calls `f` with the index of each point in a box (bounds included).
    pub fn within_box(&self, lo: &[f64; D], hi: &[f64; D], f: &mut impl FnMut(u32)) {
        if !self.nodes.is_empty() {
            self.box_node(0, lo, hi, f);
        }
    }

    fn box_node(&self, node: usize, lo: &[f64; D], hi: &[f64; D], f: &mut impl FnMut(u32)) {
        match self.nodes[node] {
            Node::Leaf { start, end } => {
                for &i in &self.order[start as usize..end as usize] {
                    let p = &self.points[i as usize];
                    if (0..D).all(|k| p[k] >= lo[k] && p[k] <= hi[k]) {
                        f(i);
                    }
                }
            }
            Node::Split { dim, value, left, right } => {
                if lo[dim as usize] <= value {
                    self.box_node(left as usize, lo, hi, f);
                }
                if hi[dim as usize] >= value {
                    self.box_node(right as usize, lo, hi, f);
                }
            }
        }
    }

    fn within_node(&self, node: usize, q: &[f64; D], r2: f64, f: &mut impl FnMut(u32)) {
        match self.nodes[node] {
            Node::Leaf { start, end } => {
                for &i in &self.order[start as usize..end as usize] {
                    if dist2(&self.points[i as usize], q) < r2 {
                        f(i);
                    }
                }
            }
            Node::Split { dim, value, left, right } => {
                let diff = q[dim as usize] - value;
                if diff < 0.0 || diff * diff < r2 {
                    self.within_node(left as usize, q, r2, f);
                }
                if diff >= 0.0 || diff * diff < r2 {
                    self.within_node(right as usize, q, r2, f);
                }
            }
        }
    }
}

/// Whether `(d, i)` comes before `e`: nearer, or as near with a smaller index.
fn less(d: f64, i: u32, e: (f64, u32)) -> bool {
    d < e.0 || (d == e.0 && i < e.1)
}

fn dist2<const D: usize>(a: &[f64; D], b: &[f64; D]) -> f64 {
    let mut s = 0.0;
    for k in 0..D {
        let d = a[k] - b[k];
        s += d * d;
    }
    s
}

/// Builds the subtree of `order[start..end]`, split at the median of the dimension of largest
/// spread; returns its node.
fn build<const D: usize>(points: &[[f64; D]], order: &mut [u32], start: usize, end: usize, nodes: &mut Vec<Node>) -> u32 {
    let at = nodes.len() as u32;
    if end - start <= LEAF {
        nodes.push(Node::Leaf { start: start as u32, end: end as u32 });
        return at;
    }
    let mut lo = [f64::INFINITY; D];
    let mut hi = [f64::NEG_INFINITY; D];
    for &i in &order[start..end] {
        for k in 0..D {
            lo[k] = lo[k].min(points[i as usize][k]);
            hi[k] = hi[k].max(points[i as usize][k]);
        }
    }
    let dim = (0..D).max_by(|&a, &b| (hi[a] - lo[a]).total_cmp(&(hi[b] - lo[b]))).unwrap_or(0);
    if hi[dim] - lo[dim] <= 0.0 {
        // All the points are at the same place.
        nodes.push(Node::Leaf { start: start as u32, end: end as u32 });
        return at;
    }
    let mid = (start + end) / 2;
    order[start..end].select_nth_unstable_by(mid - start, |&a, &b| points[a as usize][dim].total_cmp(&points[b as usize][dim]));
    let value = points[order[mid] as usize][dim];
    nodes.push(Node::Leaf { start: 0, end: 0 });
    // The left part has the points at or below the median's value, the right part those at or
    // above it.
    let left = build(points, order, start, mid, nodes);
    let right = build(points, order, mid, end, nodes);
    nodes[at as usize] = Node::Split { dim: dim as u8, value, left, right };
    at
}

/// A k-d tree built and searched as nanoflann 1.3 (which PDAL uses) builds and searches it: split
/// in the middle of the widest dimension, leaves of up to `leaf` points. Neighbours as far as
/// each other come in the order nanoflann finds them, so that results that depend on which of
/// them are kept (PDAL's SMRF fills empty cells with the mean of 8) are PDAL's.
#[derive(Debug, Clone)]
pub(crate) struct NanoKdTree<const D: usize> {
    points: Vec<[f64; D]>,
    vind: Vec<usize>,
    nodes: Vec<NanoNode>,
    root_bbox: [(f64, f64); D],
}

#[derive(Debug, Clone, Copy)]
enum NanoNode {
    Leaf { left: usize, right: usize },
    Split { divfeat: usize, divlow: f64, divhigh: f64, child1: usize, child2: usize },
}

impl<const D: usize> NanoKdTree<D> {
    /// Builds the index of points, with leaves of up to `leaf` points (PDAL: 100).
    pub fn new(points: Vec<[f64; D]>, leaf: usize) -> Self {
        let n = points.len();
        let mut t = Self { points, vind: (0..n).collect(), nodes: Vec::new(), root_bbox: [(0.0, 0.0); D] };
        if n == 0 {
            return t;
        }
        let mut bbox = [(f64::INFINITY, f64::NEG_INFINITY); D];
        for p in &t.points {
            for k in 0..D {
                bbox[k].0 = bbox[k].0.min(p[k]);
                bbox[k].1 = bbox[k].1.max(p[k]);
            }
        }
        t.root_bbox = bbox;
        t.divide(0, n, &mut bbox, leaf);
        t
    }

    /// The number of points.
    pub fn len(&self) -> usize {
        self.points.len()
    }

    fn get(&self, i: usize, k: usize) -> f64 {
        self.points[i][k]
    }

    fn divide(&mut self, left: usize, right: usize, bbox: &mut [(f64, f64); D], leaf: usize) -> usize {
        let at = self.nodes.len();
        self.nodes.push(NanoNode::Leaf { left, right });
        if right - left <= leaf {
            for (k, b) in bbox.iter_mut().enumerate() {
                *b = (self.get(self.vind[left], k), self.get(self.vind[left], k));
            }
            for j in left + 1..right {
                for (k, b) in bbox.iter_mut().enumerate() {
                    let v = self.get(self.vind[j], k);
                    if b.0 > v {
                        b.0 = v;
                    }
                    if b.1 < v {
                        b.1 = v;
                    }
                }
            }
            return at;
        }
        let (idx, cutfeat, cutval) = self.middle_split(left, right - left, bbox);
        let mut left_bbox = *bbox;
        left_bbox[cutfeat].1 = cutval;
        let child1 = self.divide(left, left + idx, &mut left_bbox, leaf);
        let mut right_bbox = *bbox;
        right_bbox[cutfeat].0 = cutval;
        let child2 = self.divide(left + idx, right, &mut right_bbox, leaf);
        self.nodes[at] = NanoNode::Split { divfeat: cutfeat, divlow: left_bbox[cutfeat].1, divhigh: right_bbox[cutfeat].0, child1, child2 };
        for k in 0..D {
            bbox[k] = (left_bbox[k].0.min(right_bbox[k].0), left_bbox[k].1.max(right_bbox[k].1));
        }
        at
    }

    fn min_max(&self, start: usize, count: usize, k: usize) -> (f64, f64) {
        let mut lo = self.get(self.vind[start], k);
        let mut hi = lo;
        for j in 1..count {
            let v = self.get(self.vind[start + j], k);
            if v < lo {
                lo = v;
            }
            if v > hi {
                hi = v;
            }
        }
        (lo, hi)
    }

    fn middle_split(&mut self, start: usize, count: usize, bbox: &[(f64, f64); D]) -> (usize, usize, f64) {
        const EPS: f64 = 0.00001;
        let mut max_span = bbox[0].1 - bbox[0].0;
        for b in bbox.iter().skip(1) {
            if b.1 - b.0 > max_span {
                max_span = b.1 - b.0;
            }
        }
        let mut max_spread = -1.0;
        let mut cutfeat = 0;
        for (k, b) in bbox.iter().enumerate() {
            if b.1 - b.0 > (1.0 - EPS) * max_span {
                let (lo, hi) = self.min_max(start, count, k);
                if hi - lo > max_spread {
                    cutfeat = k;
                    max_spread = hi - lo;
                }
            }
        }
        let split_val = (bbox[cutfeat].0 + bbox[cutfeat].1) / 2.0;
        let (lo, hi) = self.min_max(start, count, cutfeat);
        let cutval = if split_val < lo {
            lo
        } else if split_val > hi {
            hi
        } else {
            split_val
        };
        let (lim1, lim2) = self.plane_split(start, count, cutfeat, cutval);
        let index = if lim1 > count / 2 {
            lim1
        } else if lim2 < count / 2 {
            lim2
        } else {
            count / 2
        };
        (index, cutfeat, cutval)
    }

    fn plane_split(&mut self, start: usize, count: usize, cutfeat: usize, cutval: f64) -> (usize, usize) {
        let v = |t: &Self, j: usize| t.get(t.vind[start + j], cutfeat);
        let mut left = 0usize;
        let mut right = count - 1;
        loop {
            while left <= right && v(self, left) < cutval {
                left += 1;
            }
            while right > 0 && left <= right && v(self, right) >= cutval {
                right -= 1;
            }
            if left > right || right == 0 {
                break;
            }
            self.vind.swap(start + left, start + right);
            left += 1;
            right -= 1;
        }
        let lim1 = left;
        right = count - 1;
        loop {
            while left <= right && v(self, left) <= cutval {
                left += 1;
            }
            while right > 0 && left <= right && v(self, right) > cutval {
                right -= 1;
            }
            if left > right || right == 0 {
                break;
            }
            self.vind.swap(start + left, start + right);
            left += 1;
            right -= 1;
        }
        (lim1, left)
    }

    /// The `k` nearest points (at most the number of points): `(squared distance, index)`, in
    /// nanoflann's order.
    pub fn knn(&self, q: &[f64; D], k: usize, out: &mut Vec<(f64, usize)>) {
        out.clear();
        let k = k.min(self.points.len());
        if k == 0 {
            return;
        }
        let mut dists = vec![0.0; D];
        let mut distsq = 0.0;
        for i in 0..D {
            if q[i] < self.root_bbox[i].0 {
                dists[i] = (q[i] - self.root_bbox[i].0).powi(2);
                distsq += dists[i];
            }
            if q[i] > self.root_bbox[i].1 {
                dists[i] = (q[i] - self.root_bbox[i].1).powi(2);
                distsq += dists[i];
            }
        }
        let mut rs = KnnResult { idx: vec![0; k], dist: vec![0.0; k], count: 0 };
        rs.dist[k - 1] = f64::MAX;
        self.search_level(&mut rs, q, 0, distsq, &mut dists);
        out.extend(rs.dist.iter().zip(&rs.idx).take(rs.count).map(|(d, i)| (*d, *i)));
    }

    fn search_level(&self, rs: &mut KnnResult, q: &[f64; D], node: usize, mut mindistsq: f64, dists: &mut [f64]) {
        match self.nodes[node] {
            NanoNode::Leaf { left, right } => {
                let worst = rs.worst();
                for i in left..right {
                    let index = self.vind[i];
                    let d = dist2(&self.points[index], q);
                    if d < worst {
                        rs.add(d, index);
                    }
                }
            }
            NanoNode::Split { divfeat, divlow, divhigh, child1, child2 } => {
                let val = q[divfeat];
                let diff1 = val - divlow;
                let diff2 = val - divhigh;
                let (best, other, cut_dist) =
                    if diff1 + diff2 < 0.0 { (child1, child2, (val - divhigh).powi(2)) } else { (child2, child1, (val - divlow).powi(2)) };
                self.search_level(rs, q, best, mindistsq, dists);
                let dst = dists[divfeat];
                mindistsq = mindistsq + cut_dist - dst;
                dists[divfeat] = cut_dist;
                if mindistsq <= rs.worst() {
                    self.search_level(rs, q, other, mindistsq, dists);
                }
                dists[divfeat] = dst;
            }
        }
    }
}

impl<const D: usize> NanoKdTree<D> {
    /// The points closer than `r` to `q` (squared distance below `r²`), nearest first, as PDAL's
    /// radius searches return them: in the order nanoflann finds them, then sorted by distance.
    /// The sort is stable, so points as near keep the order nanoflann found them in. PDAL sorts
    /// with `std::sort`; libstdc++'s sorts up to 16 elements stably, larger result sets may order
    /// points at exactly the same distance differently.
    pub fn radius(&self, q: &[f64; D], r: f64, out: &mut Vec<(usize, f64)>) {
        out.clear();
        if self.points.is_empty() {
            return;
        }
        let mut dists = vec![0.0; D];
        let mut distsq = 0.0;
        for i in 0..D {
            if q[i] < self.root_bbox[i].0 {
                dists[i] = (q[i] - self.root_bbox[i].0).powi(2);
                distsq += dists[i];
            }
            if q[i] > self.root_bbox[i].1 {
                dists[i] = (q[i] - self.root_bbox[i].1).powi(2);
                distsq += dists[i];
            }
        }
        self.radius_level(out, q, r * r, 0, distsq, &mut dists);
        out.sort_by(|a, b| a.1.total_cmp(&b.1));
    }

    fn radius_level(&self, out: &mut Vec<(usize, f64)>, q: &[f64; D], r2: f64, node: usize, mut mindistsq: f64, dists: &mut [f64]) {
        match self.nodes[node] {
            NanoNode::Leaf { left, right } => {
                for i in left..right {
                    let index = self.vind[i];
                    let d = dist2(&self.points[index], q);
                    if d < r2 {
                        out.push((index, d));
                    }
                }
            }
            NanoNode::Split { divfeat, divlow, divhigh, child1, child2 } => {
                let val = q[divfeat];
                let diff1 = val - divlow;
                let diff2 = val - divhigh;
                let (best, other, cut_dist) =
                    if diff1 + diff2 < 0.0 { (child1, child2, (val - divhigh).powi(2)) } else { (child2, child1, (val - divlow).powi(2)) };
                self.radius_level(out, q, r2, best, mindistsq, dists);
                let dst = dists[divfeat];
                mindistsq = mindistsq + cut_dist - dst;
                dists[divfeat] = cut_dist;
                if mindistsq <= r2 {
                    self.radius_level(out, q, r2, other, mindistsq, dists);
                }
                dists[divfeat] = dst;
            }
        }
    }
}

/// nanoflann's `KNNResultSet`.
struct KnnResult {
    idx: Vec<usize>,
    dist: Vec<f64>,
    count: usize,
}

impl KnnResult {
    fn worst(&self) -> f64 {
        self.dist[self.dist.len() - 1]
    }

    fn add(&mut self, d: f64, index: usize) {
        let cap = self.dist.len();
        let mut i = self.count;
        while i > 0 {
            if self.dist[i - 1] > d {
                if i < cap {
                    self.dist[i] = self.dist[i - 1];
                    self.idx[i] = self.idx[i - 1];
                }
            } else {
                break;
            }
            i -= 1;
        }
        if i < cap {
            self.dist[i] = d;
            self.idx[i] = index;
        }
        if self.count < cap {
            self.count += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_nearest_points_as_brute_force_does() {
        let mut s = 7u64;
        let mut r = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 33) % 1000) as f64 / 10.0
        };
        let pts: Vec<[f64; 3]> = (0..2000).map(|_| [r(), r(), r()]).collect();
        let t = KdTree::new(pts.clone());
        let mut out = Vec::new();
        for q in pts.iter().take(50) {
            t.knn(q, 9, &mut out);
            let mut all: Vec<(f64, u32)> = pts.iter().enumerate().map(|(i, p)| (dist2(p, q), i as u32)).collect();
            all.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            assert_eq!(out, all[..9].to_vec());
            assert_eq!(t.count_within(q, 5.0), all.iter().filter(|e| e.0 < 25.0).count());
            // nanoflann's tree finds the same distances.
            let nano = NanoKdTree::new(pts.clone(), 10);
            let mut found = Vec::new();
            nano.knn(q, 9, &mut found);
            let d: Vec<f64> = found.iter().map(|e| e.0).collect();
            assert_eq!(d, all[..9].iter().map(|e| e.0).collect::<Vec<_>>());
            // Radius searches find the same points, nearest first.
            let mut near = Vec::new();
            nano.radius(q, 5.0, &mut near);
            let mut idx: Vec<usize> = near.iter().map(|e| e.0).collect();
            assert!(near.windows(2).all(|w| w[0].1 <= w[1].1));
            idx.sort_unstable();
            let mut want: Vec<usize> = all.iter().filter(|e| e.0 < 25.0).map(|e| e.1 as usize).collect();
            want.sort_unstable();
            assert_eq!(idx, want);
        }
    }

    #[test]
    fn radius_keeps_the_found_order_of_points_as_near() {
        // Terrio's order, which is not PDAL's here: one leaf, so nanoflann finds the points in
        // index order, and twenty points at each of three distances, more than the 16 that
        // libstdc++'s std::sort orders stably.
        let pts: Vec<[f64; 3]> = (0..60).map(|i| [[1.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 3.0]][i % 3]).collect();
        let nano = NanoKdTree::new(pts, 100);
        let mut near = Vec::new();
        nano.radius(&[0.0; 3], 10.0, &mut near);
        let want: Vec<usize> = (0..3).flat_map(|k| (0..60).filter(move |i| i % 3 == k)).collect();
        assert_eq!(near.iter().map(|e| e.0).collect::<Vec<_>>(), want);
    }

    #[test]
    fn radius_keeps_the_found_order_across_leaves() {
        // Thirty points at exactly the same distance (5) from the origin, in a scrambled order,
        // with leaves of 4: the tree reorders them, so the order found is not index order.
        let mut pts: Vec<[f64; 3]> = Vec::new();
        for (a, b) in [(3.0, 4.0), (4.0, 3.0)] {
            for (sa, sb) in [(1.0, 1.0), (1.0, -1.0), (-1.0, 1.0), (-1.0, -1.0)] {
                pts.extend([[sa * a, sb * b, 0.0], [sa * a, 0.0, sb * b], [0.0, sa * a, sb * b]]);
            }
        }
        for s in [5.0, -5.0] {
            pts.extend([[s, 0.0, 0.0], [0.0, s, 0.0], [0.0, 0.0, s]]);
        }
        let n = pts.len();
        let pts: Vec<[f64; 3]> = (0..n).map(|i| pts[(i * 7) % n]).collect();
        let nano = NanoKdTree::new(pts, 4);
        let q = [0.0; 3];
        let mut found = Vec::new();
        nano.radius_level(&mut found, &q, 36.0, 0, 0.0, &mut [0.0; 3]);
        let found: Vec<usize> = found.iter().map(|e| e.0).collect();
        assert_eq!(found.len(), n);
        assert!(found.windows(2).any(|w| w[0] > w[1]), "the tree should reorder the points");
        let mut near = Vec::new();
        nano.radius(&q, 6.0, &mut near);
        assert_eq!(near.iter().map(|e| e.0).collect::<Vec<_>>(), found);
    }
}

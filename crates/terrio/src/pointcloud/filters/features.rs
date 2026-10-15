//! Filters that compute features of each point's neighbourhood: PDAL's `filters.normal`
//! (normals and curvature) and `filters.cluster` (Euclidean clusters).

use super::super::kdtree::{KdTree, NanoKdTree};
use super::{par_map, xyz};
use crate::pointcloud::{Dimension, PointCloud, Values};
use crate::{Error, Result};

/// How to compute normals (PDAL's `filters.normal`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NormalOptions {
    /// The neighbours of each point (itself excluded). 8 by default.
    pub knn: usize,
    /// Orient the normals towards this point (rather than up).
    pub viewpoint: Option<[f64; 3]>,
    /// Orient the normals up (positive z), when there is no viewpoint. True by default.
    pub always_up: bool,
}

impl Default for NormalOptions {
    fn default() -> Self {
        Self { knn: 8, viewpoint: None, always_up: true }
    }
}

/// The eigenvalues (ascending) and eigenvectors (columns, in the same order) of a symmetric 3x3
/// matrix, by Jacobi rotations.
fn eigen_symmetric(m: [[f64; 3]; 3]) -> ([f64; 3], [[f64; 3]; 3]) {
    let mut a = m;
    let mut v = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    for _ in 0..64 {
        let off = a[0][1] * a[0][1] + a[0][2] * a[0][2] + a[1][2] * a[1][2];
        if off <= f64::MIN_POSITIVE {
            break;
        }
        for (p, q) in [(0, 1), (0, 2), (1, 2)] {
            if a[p][q] == 0.0 {
                continue;
            }
            let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
            let t = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
            let t = if theta == 0.0 { 1.0 } else { t };
            let c = 1.0 / (t * t + 1.0).sqrt();
            let s = t * c;
            for row in a.iter_mut() {
                let (akp, akq) = (row[p], row[q]);
                row[p] = c * akp - s * akq;
                row[q] = s * akp + c * akq;
            }
            let (rp, rq) = (a[p], a[q]);
            for k in 0..3 {
                a[p][k] = c * rp[k] - s * rq[k];
                a[q][k] = s * rp[k] + c * rq[k];
            }
            for row in v.iter_mut() {
                let (vp, vq) = (row[p], row[q]);
                row[p] = c * vp - s * vq;
                row[q] = s * vp + c * vq;
            }
        }
    }
    let mut order = [0usize, 1, 2];
    order.sort_by(|&i, &j| a[i][i].total_cmp(&a[j][j]));
    let vals = [a[order[0]][order[0]], a[order[1]][order[1]], a[order[2]][order[2]]];
    let mut vecs = [[0.0; 3]; 3];
    for (col, &o) in order.iter().enumerate() {
        for r in 0..3 {
            vecs[r][col] = v[r][o];
        }
    }
    (vals, vecs)
}

/// Computes normals and curvature, as PDAL's `filters.normal`: the eigenvector of the smallest
/// eigenvalue of the covariance of each point's `knn` nearest neighbours (and itself), oriented
/// up or towards the viewpoint, and the smallest eigenvalue over their sum. Adds `normal_x`,
/// `normal_y`, `normal_z` and `curvature` (zeros where the neighbours all coincide).
pub fn normal(cloud: &PointCloud, o: &NormalOptions) -> Result<PointCloud> {
    if o.knn == 0 {
        return Err(Error::InvalidArgument("knn must be positive".into()));
    }
    let (xs, ys, zs) = xyz(cloud)?;
    let n = cloud.len();
    let tree = NanoKdTree::new((0..n).map(|i| [xs[i], ys[i], zs[i]]).collect(), 100);
    let all: Vec<usize> = (0..n).collect();
    let up = o.always_up && o.viewpoint.is_none();
    let results: Vec<[f64; 4]> = par_map(&all, Vec::new, |found, &i| {
        tree.knn(&[xs[i], ys[i], zs[i]], o.knn + 1, found);
        // PDAL's covariance: a running mean, the offsets from it rounded to floats.
        let (mut mx, mut my, mut mz) = (0.0, 0.0, 0.0);
        for (k, (_, j)) in found.iter().enumerate() {
            let c = (k + 1) as f64;
            mx += (xs[*j] - mx) / c;
            my += (ys[*j] - my) / c;
            mz += (zs[*j] - mz) / c;
        }
        let mut cov = [[0.0f64; 3]; 3];
        for (_, j) in found.iter() {
            let d = [(xs[*j] - mx) as f32 as f64, (ys[*j] - my) as f32 as f64, (zs[*j] - mz) as f32 as f64];
            for r in 0..3 {
                for c in 0..3 {
                    cov[r][c] += d[r] * d[c];
                }
            }
        }
        let m = (found.len() as f64 - 1.0).max(1.0);
        for row in cov.iter_mut() {
            for v in row.iter_mut() {
                *v /= m;
            }
        }
        if cov.iter().flatten().all(|v| *v == 0.0) {
            return [0.0; 4];
        }
        let (vals, vecs) = eigen_symmetric(cov);
        let sum = vals[0] + vals[1] + vals[2];
        let curvature = if sum != 0.0 { (vals[0] / sum).abs() } else { 0.0 };
        let mut nrm = [vecs[0][0], vecs[1][0], vecs[2][0]];
        if let Some(vp) = o.viewpoint {
            let d = [vp[0] - xs[i], vp[1] - ys[i], vp[2] - zs[i]];
            if d[0] * nrm[0] + d[1] * nrm[1] + d[2] * nrm[2] < 0.0 {
                nrm = [-nrm[0], -nrm[1], -nrm[2]];
            }
        } else if up && nrm[2] < 0.0 {
            nrm = [-nrm[0], -nrm[1], -nrm[2]];
        }
        [nrm[0], nrm[1], nrm[2], curvature]
    });
    let mut out = cloud.clone();
    for (k, name) in ["normal_x", "normal_y", "normal_z", "curvature"].iter().enumerate() {
        let v = Values::F64(results.iter().map(|r| r[k]).collect());
        match out.dim_mut(name) {
            Some(d) => d.values = v,
            None => out.dimensions.push(Dimension::new(name, v)),
        }
    }
    Ok(out)
}

/// How to cluster points (PDAL's `filters.cluster`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClusterOptions {
    /// The fewest points of a cluster. 1 by default.
    pub min_points: u64,
    /// The most points of a cluster. Unlimited by default.
    pub max_points: u64,
    /// The distance within which points are neighbours. 1 by default.
    pub tolerance: f64,
    /// Measure distances in 3D (else in x and y). True by default.
    pub is_3d: bool,
}

impl Default for ClusterOptions {
    fn default() -> Self {
        Self { min_points: 1, max_points: u64::MAX, tolerance: 1.0, is_3d: true }
    }
}

/// Labels Euclidean clusters, as PDAL's `filters.cluster`: the points connected by neighbours
/// closer than the tolerance, numbered from 1 in the order of their first points, in
/// `cluster_id` (0 for the points of clusters too small or too large).
pub fn cluster(cloud: &PointCloud, o: &ClusterOptions) -> Result<PointCloud> {
    if o.tolerance.is_nan() || o.tolerance <= 0.0 {
        return Err(Error::InvalidArgument("the tolerance must be positive".into()));
    }
    let (xs, ys, zs) = xyz(cloud)?;
    let n = cloud.len();
    let mut ids = vec![0u64; n];
    let mut processed = vec![false; n];
    let tree3 = o.is_3d.then(|| KdTree::new((0..n).map(|i| [xs[i], ys[i], zs[i]]).collect()));
    let tree2 = (!o.is_3d).then(|| KdTree::new((0..n).map(|i| [xs[i], ys[i]]).collect()));
    let mut next = 1u64;
    let mut queue: Vec<usize> = Vec::new();
    let mut near: Vec<usize> = Vec::new();
    for i in 0..n {
        if processed[i] {
            continue;
        }
        queue.clear();
        queue.push(i);
        processed[i] = true;
        let mut q = 0;
        while q < queue.len() {
            let j = queue[q];
            near.clear();
            match (&tree3, &tree2) {
                (Some(t), _) => t.within(&[xs[j], ys[j], zs[j]], o.tolerance, &mut |k| near.push(k as usize)),
                (_, Some(t)) => t.within(&[xs[j], ys[j]], o.tolerance, &mut |k| near.push(k as usize)),
                _ => {}
            }
            near.sort_unstable();
            for &k in &near {
                if !processed[k] {
                    processed[k] = true;
                    queue.push(k);
                }
            }
            q += 1;
        }
        let size = queue.len() as u64;
        if size >= o.min_points && size <= o.max_points {
            for &k in &queue {
                ids[k] = next;
            }
            next += 1;
        }
    }
    let mut out = cloud.clone();
    let v = Values::U64(ids);
    match out.dim_mut("cluster_id") {
        Some(d) => d.values = v,
        None => out.dimensions.push(Dimension::new("cluster_id", v)),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_eigenvectors() {
        let (vals, vecs) = eigen_symmetric([[2.0, 0.0, 0.0], [0.0, 3.0, 0.0], [0.0, 0.0, 1.0]]);
        assert_eq!(vals, [1.0, 2.0, 3.0]);
        assert_eq!(vecs[2][0].abs(), 1.0);
        let (vals, _) = eigen_symmetric([[4.0, 1.0, 0.5], [1.0, 3.0, 0.2], [0.5, 0.2, 1.0]]);
        let tr: f64 = vals.iter().sum();
        assert!((tr - 8.0).abs() < 1e-12);
    }
}

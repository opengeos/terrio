//! Filters that measure each point's nearest neighbours: PDAL's `filters.dbscan` (density-based
//! clusters), `filters.lof` (local outlier factors), `filters.nndistance`,
//! `filters.radialdensity`, `filters.reciprocity`, `filters.miniball` and `filters.zsmooth`.

use super::super::kdtree::{KdTree, NanoKdTree};
use super::features::{radius_search, set_column};
use super::{find_dim, par_map, xyz};
use crate::pointcloud::{PointCloud, Values};
use crate::{Error, Result};

/// How to cluster points by density (PDAL's `filters.dbscan`).
#[derive(Debug, Clone, PartialEq)]
pub struct DbscanOptions {
    /// The fewest points (itself included) within `eps` of a core point. 6 by default.
    pub min_points: u64,
    /// The distance within which points are neighbours. 1 by default.
    pub eps: f64,
    /// The dimensions distances are measured in, as PDAL or terrio names them. `X`, `Y` and `Z`
    /// by default.
    pub dimensions: Vec<String>,
}

impl Default for DbscanOptions {
    fn default() -> Self {
        Self { min_points: 6, eps: 1.0, dimensions: vec!["X".into(), "Y".into(), "Z".into()] }
    }
}

/// The points of a cloud in `D` dimensions.
fn points_of<const D: usize>(cols: &[Vec<f64>]) -> Vec<[f64; D]> {
    (0..cols[0].len()).map(|i| std::array::from_fn(|k| cols[k][i])).collect()
}

/// DBSCAN in `D` dimensions: the cluster of each point, from 0, -1 for noise.
fn dbscan_in<const D: usize>(cols: &[Vec<f64>], eps: f64, min_points: u64) -> Vec<i64> {
    let pts = points_of::<D>(cols);
    let n = pts.len();
    let tree = KdTree::new(pts);
    let all: Vec<usize> = (0..n).collect();
    // Core points have at least `min_points` neighbours, themselves included.
    let core: Vec<bool> = par_map(&all, || (), |_, &i| tree.count_within(tree.point(i), eps) as u64 >= min_points);
    // PDAL labels the points in order, a cluster spreading through core points to every point
    // not labeled yet: noise found before its cluster joins it, points already in a cluster stay.
    let mut ids = vec![-2i64; n];
    let mut next = 0i64;
    let mut queue: Vec<usize> = Vec::new();
    for i in 0..n {
        if ids[i] != -2 {
            continue;
        }
        if !core[i] {
            ids[i] = -1;
            continue;
        }
        ids[i] = next;
        queue.clear();
        queue.push(i);
        while let Some(p) = queue.pop() {
            tree.within(tree.point(p), eps, &mut |q| {
                let q = q as usize;
                match ids[q] {
                    -1 => ids[q] = next,
                    -2 => {
                        ids[q] = next;
                        if core[q] {
                            queue.push(q);
                        }
                    }
                    _ => {}
                }
            });
        }
        next += 1;
    }
    ids
}

/// Labels density-based clusters, as PDAL's `filters.dbscan`: core points (with at least
/// `min_points` points within `eps`, themselves included) and the points within `eps` of them
/// form clusters, numbered from 0 in the order of their first points, in `cluster_id` (-1 for
/// noise).
pub fn dbscan(cloud: &PointCloud, o: &DbscanOptions) -> Result<PointCloud> {
    if o.eps.is_nan() || o.eps <= 0.0 {
        return Err(Error::InvalidArgument("eps must be positive".into()));
    }
    let cols: Vec<Vec<f64>> = o.dimensions.iter().map(|d| Ok(find_dim(cloud, d)?.to_f64())).collect::<Result<_>>()?;
    let ids = match cols.len() {
        1 => dbscan_in::<1>(&cols, o.eps, o.min_points),
        2 => dbscan_in::<2>(&cols, o.eps, o.min_points),
        3 => dbscan_in::<3>(&cols, o.eps, o.min_points),
        4 => dbscan_in::<4>(&cols, o.eps, o.min_points),
        5 => dbscan_in::<5>(&cols, o.eps, o.min_points),
        6 => dbscan_in::<6>(&cols, o.eps, o.min_points),
        _ => return Err(Error::InvalidArgument("dbscan: give 1 to 6 dimensions".into())),
    };
    let mut out = cloud.clone();
    set_column(&mut out, "cluster_id", Values::I64(ids));
    Ok(out)
}

/// The `k` nearest points to each point (itself among them, at most all the points), as PDAL's
/// `KD3Index::knnSearch` finds them: `(squared distance, index)`, padded with `(0, 0)` as PDAL's
/// lists are when there are fewer points.
fn knn_all(cloud: &PointCloud, k: usize) -> Result<Vec<Vec<(f64, usize)>>> {
    let (xs, ys, zs) = xyz(cloud)?;
    let n = cloud.len();
    let tree = NanoKdTree::new((0..n).map(|i| [xs[i], ys[i], zs[i]]).collect(), 100);
    let all: Vec<usize> = (0..n).collect();
    Ok(par_map(
        &all,
        || (),
        |_, &i| {
            let mut found = Vec::new();
            tree.knn(&[xs[i], ys[i], zs[i]], k, &mut found);
            found.resize(k, (0.0, 0));
            found
        },
    ))
}

/// Computes local outlier factors, as PDAL's `filters.lof`: `nndistance` (the distance to the
/// `minpts`-th nearest neighbour), `local_reachability_distance` (the inverse of the mean
/// reachability distance to the `minpts` nearest neighbours, each the larger of the distance
/// and the neighbour's `nndistance`) and `local_outlier_factor` (the mean ratio of the
/// neighbours' reachability to the point's). PDAL's `minpts` is 10 by default.
pub fn lof(cloud: &PointCloud, minpts: usize) -> Result<PointCloud> {
    if minpts == 0 {
        return Err(Error::InvalidArgument("minpts must be positive".into()));
    }
    // The nearest points include the point itself.
    let k = minpts + 1;
    let near = knn_all(cloud, k)?;
    let kdist: Vec<f64> = near.iter().map(|nb| nb[k - 1].0.sqrt()).collect();
    let lrd: Vec<f64> = near
        .iter()
        .map(|nb| {
            let mut m = 0.0;
            for (j, (d2, p)) in nb.iter().enumerate() {
                let reach = kdist[*p].max(d2.sqrt());
                m += (reach - m) / (j + 1) as f64;
            }
            1.0 / m
        })
        .collect();
    let factor: Vec<f64> = near
        .iter()
        .enumerate()
        .map(|(i, nb)| {
            let mut m = 0.0;
            for (j, (_, p)) in nb.iter().enumerate() {
                m += (lrd[*p] / lrd[i] - m) / (j + 1) as f64;
            }
            m
        })
        .collect();
    let mut out = cloud.clone();
    set_column(&mut out, "nndistance", Values::F64(kdist));
    set_column(&mut out, "local_reachability_distance", Values::F64(lrd));
    set_column(&mut out, "local_outlier_factor", Values::F64(factor));
    Ok(out)
}

/// How PDAL's `filters.nndistance` measures the distance to the nearest neighbours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NnDistanceMode {
    /// The distance to the `k`-th nearest neighbour (the default).
    #[default]
    Kth,
    /// The mean distance to the `k` nearest neighbours.
    Avg,
}

impl NnDistanceMode {
    /// Parses `kth` or `avg`, in any case.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "kth" => Ok(Self::Kth),
            "avg" => Ok(Self::Avg),
            _ => Err(Error::InvalidArgument(format!("invalid mode '{s}': use kth or avg"))),
        }
    }
}

/// The distance to each point's nearest neighbours, as PDAL's `filters.nndistance`, in
/// `nndistance`: to the `k`-th (PDAL's `k` is 10 by default), or their mean distance.
pub fn nndistance(cloud: &PointCloud, k: usize, mode: NnDistanceMode) -> Result<PointCloud> {
    if k == 0 {
        return Err(Error::InvalidArgument("k must be positive".into()));
    }
    let near = knn_all(cloud, k + 1)?;
    let d: Vec<f64> = near
        .iter()
        .map(|nb| match mode {
            NnDistanceMode::Kth => nb[k].0.sqrt(),
            // The first is the point itself.
            NnDistanceMode::Avg => nb[1..].iter().map(|e| e.0.sqrt()).sum::<f64>() / k as f64,
        })
        .collect();
    let mut out = cloud.clone();
    set_column(&mut out, "nndistance", Values::F64(d));
    Ok(out)
}

/// The π of PDAL's `filters.radialdensity`.
#[allow(clippy::approx_constant)]
const PDAL_RADIAL_PI: f64 = 3.14159;

/// The density of the points around each point, as PDAL's `filters.radialdensity`: the points
/// closer than `radius` (itself included) over the volume of the sphere (with PDAL's π of
/// 3.14159), in `radial_density`. PDAL's `radius` is 1 by default.
pub fn radial_density(cloud: &PointCloud, radius: f64) -> Result<PointCloud> {
    if radius.is_nan() || radius <= 0.0 {
        return Err(Error::InvalidArgument("the radius must be positive".into()));
    }
    let (xs, ys, zs) = xyz(cloud)?;
    let n = cloud.len();
    let tree = KdTree::new((0..n).map(|i| [xs[i], ys[i], zs[i]]).collect());
    let factor = 1.0 / ((4.0 / 3.0) * PDAL_RADIAL_PI * (radius * radius * radius));
    let all: Vec<usize> = (0..n).collect();
    let d = par_map(&all, || (), |_, &i| tree.count_within(tree.point(i), radius) as f64 * factor);
    let mut out = cloud.clone();
    set_column(&mut out, "radial_density", Values::F64(d));
    Ok(out)
}

/// The share of each point's `knn` nearest neighbours that do not have it among theirs, in
/// percent, as PDAL's `filters.reciprocity`, in `reciprocity`. PDAL's `knn` is 8 by default.
pub fn reciprocity(cloud: &PointCloud, knn: usize) -> Result<PointCloud> {
    if knn == 0 {
        return Err(Error::InvalidArgument("knn must be positive".into()));
    }
    let near = knn_all(cloud, knn + 1)?;
    let n = cloud.len();
    let lists: Vec<&[(f64, usize)]> = near.iter().map(|nb| &nb[..(knn + 1).min(n)]).collect();
    let all: Vec<usize> = (0..n).collect();
    let r = par_map(
        &all,
        || (),
        |_, &i| {
            let uni = lists[i].iter().filter(|(_, j)| *j != i && !lists[*j].iter().any(|(_, p)| *p == i)).count();
            100.0 * uni as f64 / knn as f64
        },
    );
    let mut out = cloud.clone();
    set_column(&mut out, "reciprocity", Values::F64(r));
    Ok(out)
}

/// The smallest ball enclosing points: its centre and radius, by Welzl's algorithm.
fn miniball_of(pts: &[[f64; 3]]) -> ([f64; 3], f64) {
    fn ball(support: &[[f64; 3]]) -> ([f64; 3], f64) {
        let sub = |a: &[f64; 3], b: &[f64; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        let dot = |a: &[f64; 3], b: &[f64; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        match support.len() {
            0 => ([0.0; 3], -1.0),
            1 => (support[0], 0.0),
            _ => {
                // The centre is p0 + Σ λ_k (p_k − p0) with 2 (p_j − p0)·(c − p0) = |p_j − p0|²:
                // solve the Gram system by Gaussian elimination.
                let p0 = support[0];
                let v: Vec<[f64; 3]> = support[1..].iter().map(|p| sub(p, &p0)).collect();
                let m = v.len();
                let mut a = vec![vec![0.0; m + 1]; m];
                for r in 0..m {
                    for c in 0..m {
                        a[r][c] = 2.0 * dot(&v[r], &v[c]);
                    }
                    a[r][m] = dot(&v[r], &v[r]);
                }
                for c in 0..m {
                    let piv = (c..m).max_by(|&x, &y| a[x][c].abs().total_cmp(&a[y][c].abs())).unwrap_or(c);
                    a.swap(c, piv);
                    if a[c][c].abs() < 1e-300 {
                        continue;
                    }
                    let pivot = a[c].clone();
                    for (r, row) in a.iter_mut().enumerate() {
                        if r != c {
                            let f = row[c] / pivot[c];
                            for (x, p) in row.iter_mut().zip(&pivot).skip(c) {
                                *x -= f * p;
                            }
                        }
                    }
                }
                let mut centre = p0;
                for (r, vr) in v.iter().enumerate() {
                    let l = if a[r][r].abs() < 1e-300 { 0.0 } else { a[r][m] / a[r][r] };
                    for k in 0..3 {
                        centre[k] += l * vr[k];
                    }
                }
                let d = sub(&centre, &p0);
                (centre, dot(&d, &d).sqrt())
            }
        }
    }
    fn inside(b: &([f64; 3], f64), p: &[f64; 3]) -> bool {
        let d = ((p[0] - b.0[0]).powi(2) + (p[1] - b.0[1]).powi(2) + (p[2] - b.0[2]).powi(2)).sqrt();
        b.1 >= 0.0 && d <= b.1 * (1.0 + 1e-12) + 1e-12
    }
    fn welzl(pts: &[[f64; 3]], support: &mut Vec<[f64; 3]>) -> ([f64; 3], f64) {
        if pts.is_empty() || support.len() == 4 {
            return ball(support);
        }
        let (p, rest) = (pts[pts.len() - 1], &pts[..pts.len() - 1]);
        let b = welzl(rest, support);
        if inside(&b, &p) {
            return b;
        }
        support.push(p);
        let b = welzl(rest, support);
        support.pop();
        b
    }
    welzl(pts, &mut Vec::new())
}

/// How far each point is from the centre of the smallest ball enclosing its `knn` nearest
/// neighbours, as PDAL's `filters.miniball`: `d / (d + 2 r / √3)` for a distance `d` to the
/// centre of a ball of radius `r`, in `miniball`. PDAL's `knn` is 8 by default.
pub fn miniball(cloud: &PointCloud, knn: usize) -> Result<PointCloud> {
    if knn == 0 {
        return Err(Error::InvalidArgument("knn must be positive".into()));
    }
    let (xs, ys, zs) = xyz(cloud)?;
    let near = knn_all(cloud, knn + 1)?;
    let n = cloud.len();
    let all: Vec<usize> = (0..n).collect();
    let m = par_map(
        &all,
        || (),
        |_, &i| {
            let pts: Vec<[f64; 3]> =
                near[i][..(knn + 1).min(n)].iter().filter(|(_, j)| *j != i).map(|(_, j)| [xs[*j], ys[*j], zs[*j]]).collect();
            let (c, r) = miniball_of(&pts);
            let d = ((xs[i] - c[0]).powi(2) + (ys[i] - c[1]).powi(2) + (zs[i] - c[2]).powi(2)).sqrt();
            d / (d + 2.0 * r.max(0.0) / 3f64.sqrt())
        },
    );
    let mut out = cloud.clone();
    set_column(&mut out, "miniball", Values::F64(m));
    Ok(out)
}

/// Smooths elevations, as PDAL's `filters.zsmooth`: the value at `percent` (0 the lowest, 50
/// the median, 100 the highest; linearly interpolated) of the `z` of the points closer than
/// `radius` in x and y (the nearest left out, as PDAL leaves it out: the point itself, unless
/// others share its x and y; its own `z` when there are no others), in the dimension `dim`
/// (doubles). PDAL's `radius` is 1 and `medianpercent` 50 by default.
pub fn zsmooth(cloud: &PointCloud, dim: &str, radius: f64, percent: f64) -> Result<PointCloud> {
    if !(0.0..=100.0).contains(&percent) {
        return Err(Error::InvalidArgument("the percent must be in [0, 100]".into()));
    }
    if radius.is_nan() || radius <= 0.0 {
        return Err(Error::InvalidArgument("the radius must be positive".into()));
    }
    let name = super::output_name(cloud, dim);
    if name == "z" {
        return Err(Error::InvalidArgument("zsmooth cannot write its statistic to Z".into()));
    }
    let (xs, ys, zs) = xyz(cloud)?;
    let n = cloud.len();
    let tree = NanoKdTree::new((0..n).map(|i| [xs[i], ys[i]]).collect(), 100);
    let pos = percent / 100.0;
    let all: Vec<usize> = (0..n).collect();
    let v = par_map(
        &all,
        || (Vec::new(), Vec::new()),
        |(found, near), &i| {
            radius_search(&tree, &[xs[i], ys[i]], radius, found, near);
            // PDAL skips the first point found, the point itself unless others share its x and y.
            let mut vals: Vec<f64> = near.iter().skip(1).map(|j| zs[*j]).collect();
            vals.sort_by(f64::total_cmp);
            match vals.len() {
                0 => zs[i],
                1 => vals[0],
                _ if pos == 0.0 => vals[0],
                m if pos == 1.0 => vals[m - 1],
                m => {
                    let p = pos * (m - 1) as f64;
                    let low = p.floor() as usize;
                    let high_frac = p - low as f64;
                    vals[low] * (1.0 - high_frac) + vals[low + 1] * high_frac
                }
            }
        },
    );
    let mut out = cloud.clone();
    set_column(&mut out, &name, Values::F64(v));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_smallest_enclosing_ball() {
        let (c, r) = miniball_of(&[[0.0, 0.0, 0.0], [2.0, 0.0, 0.0], [1.0, 0.1, 0.0]]);
        assert!((r - 1.0).abs() < 1e-12 && (c[0] - 1.0).abs() < 1e-12);
        let tetra = [[1.0, 1.0, 1.0], [1.0, -1.0, -1.0], [-1.0, 1.0, -1.0], [-1.0, -1.0, 1.0], [0.1, 0.2, 0.3]];
        let (c, r) = miniball_of(&tetra);
        assert!((r - 3f64.sqrt()).abs() < 1e-12 && c.iter().all(|v| v.abs() < 1e-12));
    }
}

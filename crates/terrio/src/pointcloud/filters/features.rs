// SPDX-FileCopyrightText: Copyright (c) 2016, Bradley J Chambers (brad.chambers@gmail.com)
// SPDX-FileCopyrightText: Copyright (c) 2019, Helix Re Inc. nicolas@helix.re
// SPDX-FileCopyrightText: Copyright (c) 2016, 2017, 2019, 2020 Bradley J Chambers (brad.chambers@gmail.com)
// SPDX-FileCopyrightText: Copyright (c) 2016-2017, Bradley J. Chambers (brad.chambers@gmail.com)
// SPDX-FileCopyrightText: Copyright (c) 2020, Hobu Inc.
// SPDX-FileCopyrightText: Terrio contributors
// SPDX-License-Identifier: MIT AND BSD-3-Clause
//
// Derived from (translated to Rust and modified):
//   PDAL 2.10.2:
//     pdal/private/MathUtils.cpp
//     filters/CovarianceFeaturesFilter.cpp
//     filters/NormalFilter.cpp
//     filters/EigenvaluesFilter.cpp
//     filters/ApproximateCoplanarFilter.cpp
//     filters/EstimateRankFilter.cpp
//     filters/ClusterFilter.cpp
//     filters/private/Segmentation.hpp
//     pdal/private/KDImpl.hpp

//! Filters that compute features of each point's neighbourhood: PDAL's `filters.normal`
//! (normals and curvature), `filters.cluster` (Euclidean clusters), and those of the covariance
//! of the neighbours: `filters.covariancefeatures`, `filters.eigenvalues`,
//! `filters.approximatecoplanar` and `filters.estimaterank`.

use super::super::kdtree::{KdTree, NanoKdTree};
use super::selfadjoint::eigen_selfadjoint;
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
    if o.min_points > o.max_points {
        return Err(Error::InvalidArgument(format!(
            "cluster: the least points of a cluster ({}) exceed the most ({})",
            o.min_points, o.max_points
        )));
    }
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

/// Sets a dimension's values, adding it when the points lack it.
pub(super) fn set_column(out: &mut PointCloud, name: &str, v: Values) {
    match out.dim_mut(name) {
        Some(d) => {
            d.values = v;
            d.scale = None;
            d.offset = None;
        }
        None => out.dimensions.push(Dimension::new(name, v)),
    }
}

/// A dimension's values as floats, or zeros when the points lack it: what PDAL leaves in the
/// points a filter skips.
pub(super) fn f64_or_zeros(cloud: &PointCloud, name: &str) -> Vec<f64> {
    cloud.dim(name).map_or_else(|| vec![0.0; cloud.len()], |d| d.to_f64())
}

/// PDAL's `KD3Index::neighbors(point, k, stride)`: every `stride`-th of the `k × stride` nearest
/// points (`k` at most the number of points), in nanoflann's order. The places nanoflann leaves
/// unfilled, when there are fewer points, stay 0 as in PDAL's list.
pub(super) fn strided_knn(
    tree: &NanoKdTree<3>,
    q: &[f64; 3],
    k: usize,
    stride: usize,
    found: &mut Vec<(f64, usize)>,
    out: &mut Vec<usize>,
) {
    let k = k.min(tree.len());
    let stride = stride.max(1);
    tree.knn(q, k * stride, found);
    out.clear();
    out.extend(found.iter().map(|e| e.1));
    out.resize(k * stride, 0);
    if stride > 1 {
        for i in 1..k {
            out[i] = out[i * stride];
        }
        out.truncate(k);
    }
}

/// The points closer than `r` to `q`, nearest first, as PDAL's sorted radius searches
/// (`KD2Index::radius`, `KD3Index::radius`) give them. Points at exactly the same distance keep
/// the order nanoflann finds them in, which is PDAL's order for up to 16 results; beyond that,
/// PDAL's `std::sort` may order them differently.
pub(super) fn radius_search<const D: usize>(
    tree: &NanoKdTree<D>,
    q: &[f64; D],
    r: f64,
    found: &mut Vec<(usize, f64)>,
    out: &mut Vec<usize>,
) {
    tree.radius(q, r, found);
    out.clear();
    out.extend(found.iter().map(|e| e.0));
}

/// PDAL's `math::computeCovariance`: the covariance of points about their running mean, the
/// offsets from it rounded to floats, over `n − 1`.
fn covariance(xs: &[f64], ys: &[f64], zs: &[f64], ids: &[usize]) -> [[f64; 3]; 3] {
    let (mut mx, mut my, mut mz) = (0.0, 0.0, 0.0);
    for (k, &j) in ids.iter().enumerate() {
        let c = (k + 1) as f64;
        mx += (xs[j] - mx) / c;
        my += (ys[j] - my) / c;
        mz += (zs[j] - mz) / c;
    }
    let mut cov = [[0.0f64; 3]; 3];
    for &j in ids {
        let d = [(xs[j] - mx) as f32 as f64, (ys[j] - my) as f32 as f64, (zs[j] - mz) as f32 as f64];
        for r in 0..3 {
            for c in 0..3 {
                cov[r][c] += d[r] * d[c];
            }
        }
    }
    let m = ids.len() as f64 - 1.0;
    for row in cov.iter_mut() {
        for v in row.iter_mut() {
            *v /= m;
        }
    }
    cov
}

/// Whether a covariance is zero as Eigen's `isZero` has it: every entry within 1e-12 of 0.
fn is_zero(cov: &[[f64; 3]; 3]) -> bool {
    cov.iter().flatten().all(|v| v.abs() <= 1e-12)
}

/// How PDAL's `filters.eigenvalues` and `filters.covariancefeatures` find each point's
/// neighbours: its `knn` nearest (and itself), every `stride`-th, or, with a radius, those
/// closer than it (the points with fewer than `min_k` are skipped).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Neighbourhood {
    /// The neighbours of each point (itself excluded).
    pub knn: usize,
    /// Take every `stride`-th of the `(knn + 1) × stride` nearest. 1 by default.
    pub stride: usize,
    /// Take the points closer than this instead (`knn` and `stride` are then ignored).
    pub radius: Option<f64>,
    /// With a radius, the fewest points (itself included) of a neighbourhood. 3 by default.
    pub min_k: usize,
}

/// The covariance of each point's neighbourhood (`None` for the points skipped: too few
/// neighbours in the radius). `knn_of` gives each point's number of nearest points (itself
/// included), when not the neighbourhood's.
fn covariances(cloud: &PointCloud, nb: &Neighbourhood, knn_of: Option<&[f64]>) -> Result<Vec<Option<[[f64; 3]; 3]>>> {
    if let Some(r) = nb.radius
        && (r.is_nan() || r <= 0.0)
    {
        return Err(Error::InvalidArgument("the radius must be positive".into()));
    }
    let (xs, ys, zs) = xyz(cloud)?;
    let n = cloud.len();
    let pts: Vec<[f64; 3]> = (0..n).map(|i| [xs[i], ys[i], zs[i]]).collect();
    let all: Vec<usize> = (0..n).collect();
    let tree = NanoKdTree::new(pts, 100);
    if let Some(r) = nb.radius {
        return Ok(par_map(
            &all,
            || (Vec::new(), Vec::new()),
            |(found, ids), &i| {
                radius_search(&tree, &[xs[i], ys[i], zs[i]], r, found, ids);
                (ids.len() >= nb.min_k && ids.len() >= 2).then(|| covariance(xs, ys, zs, ids))
            },
        ));
    }
    Ok(par_map(
        &all,
        || (Vec::new(), Vec::new()),
        |(found, ids), &i| {
            let (k, stride) = match knn_of {
                Some(k) => (k[i] as usize, 1),
                None => (nb.knn + 1, nb.stride),
            };
            strided_knn(&tree, &[xs[i], ys[i], zs[i]], k, stride, found, ids);
            (ids.len() >= 2).then(|| covariance(xs, ys, zs, ids))
        },
    ))
}

/// The ascending eigenvalues of covariances that are not zero.
fn eigenvalues_of(covs: &[Option<[[f64; 3]; 3]>]) -> Vec<Option<[f64; 3]>> {
    par_map(covs, || (), |_, c| c.filter(|c| !is_zero(c)).map(|c| eigen_selfadjoint(c).0))
}

/// How to compute eigenvalues (PDAL's `filters.eigenvalues`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EigenvaluesOptions {
    /// The neighbourhoods: the 8 nearest neighbours by default.
    pub neighbours: Neighbourhood,
    /// Divide the eigenvalues by their sum.
    pub normalize: bool,
}

impl Default for EigenvaluesOptions {
    fn default() -> Self {
        Self { neighbours: Neighbourhood { knn: 8, stride: 1, radius: None, min_k: 3 }, normalize: false }
    }
}

/// The eigenvalues of the covariance of each point's neighbours, ascending, as PDAL's
/// `filters.eigenvalues`: `eigenvalue0`, `eigenvalue1` and `eigenvalue2` (left as they were, 0
/// for new dimensions, where the neighbours coincide or are too few).
pub fn eigenvalues(cloud: &PointCloud, o: &EigenvaluesOptions) -> Result<PointCloud> {
    let vals = eigenvalues_of(&covariances(cloud, &o.neighbours, None)?);
    let names = ["eigenvalue0", "eigenvalue1", "eigenvalue2"];
    let mut cols: Vec<Vec<f64>> = names.iter().map(|n| f64_or_zeros(cloud, n)).collect();
    for (i, v) in vals.iter().enumerate() {
        if let Some(mut ev) = *v {
            if o.normalize {
                let sum = ev[0] + ev[1] + ev[2];
                ev = ev.map(|e| e / sum);
            }
            for k in 0..3 {
                cols[k][i] = ev[k];
            }
        }
    }
    let mut out = cloud.clone();
    for (name, col) in names.iter().zip(cols) {
        set_column(&mut out, name, Values::F64(col));
    }
    Ok(out)
}

/// How PDAL's `filters.covariancefeatures` scales the eigenvalues before computing features.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FeatureMode {
    /// The eigenvalues.
    Raw,
    /// Their square roots (the default).
    #[default]
    Sqrt,
    /// The eigenvalues over their sum.
    Normalized,
}

impl FeatureMode {
    /// Parses `raw`, `sqrt` or `normalized`, in any case.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "raw" => Ok(Self::Raw),
            "sqrt" => Ok(Self::Sqrt),
            "normalized" => Ok(Self::Normalized),
            _ => Err(Error::InvalidArgument(format!("invalid mode '{s}': use raw, sqrt or normalized"))),
        }
    }
}

/// How to compute covariance features (PDAL's `filters.covariancefeatures`).
#[derive(Debug, Clone, PartialEq)]
pub struct CovarianceFeaturesOptions {
    /// The neighbourhoods: the 10 nearest neighbours by default.
    pub neighbours: Neighbourhood,
    /// The features, as PDAL names them, in any case: `Dimensionality` (the default: linearity,
    /// planarity, scattering and verticality), `All`, or `Linearity`, `Planarity`, `Scattering`,
    /// `Verticality`, `Omnivariance`, `Anisotropy`, `Eigenentropy`, `EigenvalueSum`,
    /// `SurfaceVariation`, `DemantkeVerticality` and `Density`. Others are ignored, as PDAL does.
    pub feature_set: Vec<String>,
    /// How the eigenvalues are scaled.
    pub mode: FeatureMode,
    /// Take each point's `optimal_knn` nearest points (as PDAL's `filters.optimalneighborhood`
    /// computes them) instead.
    pub optimized: bool,
}

impl Default for CovarianceFeaturesOptions {
    fn default() -> Self {
        Self {
            neighbours: Neighbourhood { knn: 10, stride: 1, radius: None, min_k: 3 },
            feature_set: vec!["Dimensionality".into()],
            mode: FeatureMode::Sqrt,
            optimized: false,
        }
    }
}

/// Eigenvalues (ascending) and eigenvectors (columns, in the same order).
type Decomposition = ([f64; 3], [[f64; 3]; 3]);

/// The π of PDAL's `filters.covariancefeatures` density.
#[allow(clippy::approx_constant)]
const PDAL_DENSITY_PI: f64 = 3.14159265;

/// The features of `filters.covariancefeatures`, in PDAL's order, with terrio's names.
const FEATURES: [&str; 11] = [
    "linearity",
    "planarity",
    "scattering",
    "verticality",
    "omnivariance",
    "anisotropy",
    "eigenentropy",
    "eigenvalue_sum",
    "surface_variation",
    "demantke_verticality",
    "density",
];

/// The features a feature set asks for, in the order PDAL adds them, each once.
fn feature_names(set: &[String]) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for f in set {
        let f = f.trim().to_ascii_lowercase();
        let add: &[&'static str] = match f.as_str() {
            "dimensionality" => &FEATURES[..4],
            "all" => &FEATURES,
            _ => match FEATURES.iter().position(|n| n.replace('_', "") == f) {
                Some(k) => std::slice::from_ref(&FEATURES[k]),
                None => &[],
            },
        };
        for n in add {
            if !out.contains(n) {
                out.push(n);
            }
        }
    }
    out
}

/// Computes features of the eigenvalues and eigenvectors of the covariance of each point's
/// neighbours, as PDAL's `filters.covariancefeatures`: `linearity`, `planarity`, `scattering`,
/// `verticality`, `omnivariance`, `anisotropy`, `eigenentropy`, `eigenvalue_sum`,
/// `surface_variation`, `demantke_verticality` and `density` (which needs `optimal_knn` and
/// `optimal_radius`), as the feature set asks. Points whose neighbours coincide or are too few
/// are left as they were (0 for new dimensions).
pub fn covariance_features(cloud: &PointCloud, o: &CovarianceFeaturesOptions) -> Result<PointCloud> {
    let names = feature_names(&o.feature_set);
    let density = names.contains(&"density");
    if density && (cloud.dim("optimal_knn").is_none() || cloud.dim("optimal_radius").is_none()) {
        return Err(Error::InvalidArgument(
            "the Density feature needs the OptimalKNN and OptimalRadius dimensions, which the points lack".into(),
        ));
    }
    let optimal_knn = match o.optimized {
        true => {
            Some(cloud.dim("optimal_knn").ok_or_else(|| Error::InvalidArgument("the points have no OptimalKNN dimension".into()))?.to_f64())
        }
        false => None,
    };
    let covs = covariances(cloud, &o.neighbours, optimal_knn.as_deref())?;
    let decomposed: Vec<Option<Decomposition>> = par_map(&covs, || (), |_, c| c.filter(|c| !is_zero(c)).map(eigen_selfadjoint));
    let (kopt, ropt) = match density {
        true => (f64_or_zeros(cloud, "optimal_knn"), f64_or_zeros(cloud, "optimal_radius")),
        false => (Vec::new(), Vec::new()),
    };
    let mut cols: Vec<Vec<f64>> = names.iter().map(|n| f64_or_zeros(cloud, n)).collect();
    for (i, d) in decomposed.iter().enumerate() {
        let Some((ev, vecs)) = d else { continue };
        let mut lambda = [ev[2].max(0.0), ev[1].max(0.0), ev[0].max(0.0)];
        let sum = lambda[0] + lambda[1] + lambda[2];
        if lambda[0] == 0.0 {
            return Err(Error::InvalidArgument("the eigenvalues are all 0: cannot compute local features".into()));
        }
        match o.mode {
            FeatureMode::Sqrt => lambda = lambda.map(f64::sqrt),
            FeatureMode::Normalized => lambda = lambda.map(|v| v / sum),
            FeatureMode::Raw => {}
        }
        // The eigenvectors of the largest, middle and smallest eigenvalues.
        let v1 = [vecs[0][2], vecs[1][2], vecs[2][2]];
        let v2 = [vecs[0][1], vecs[1][1], vecs[2][1]];
        let v3 = [vecs[0][0], vecs[1][0], vecs[2][0]];
        for (name, col) in names.iter().zip(cols.iter_mut()) {
            col[i] = match *name {
                "linearity" => (lambda[0] - lambda[1]) / lambda[0],
                "planarity" => (lambda[1] - lambda[2]) / lambda[0],
                "scattering" => lambda[2] / lambda[0],
                "verticality" => {
                    let unary: [f64; 3] =
                        std::array::from_fn(|k| lambda[0] * v1[k].abs() + lambda[1] * v2[k].abs() + lambda[2] * v3[k].abs());
                    let norm = unary.iter().map(|u| u * u).sum::<f64>().sqrt();
                    unary[2] / norm
                }
                "omnivariance" => (lambda[2] * lambda[1] * lambda[0]).cbrt(),
                "eigenvalue_sum" => sum,
                "eigenentropy" => -(lambda[2] * lambda[2].ln() + lambda[1] * lambda[1].ln() + lambda[0] * lambda[0].ln()),
                "anisotropy" => (lambda[0] - lambda[2]) / lambda[0],
                "surface_variation" => lambda[2] / sum,
                "demantke_verticality" => 1.0 - v3[2].abs(),
                _ => (kopt[i] + 1.0) / ((4.0 / 3.0) * PDAL_DENSITY_PI * ropt[i].powi(3)),
            };
        }
    }
    let mut out = cloud.clone();
    for (name, col) in names.iter().zip(cols) {
        set_column(&mut out, name, Values::F64(col));
    }
    Ok(out)
}

/// Labels approximately coplanar points, as PDAL's `filters.approximatecoplanar`: those whose
/// `knn` nearest points (themselves included) have covariance eigenvalues `λ0 ≤ λ1 ≤ λ2` with
/// `λ1 > thresh1 × λ0` and `thresh2 × λ1 > λ2` get 1 in `coplanar`, others 0 (points whose
/// neighbours coincide are left as they were). PDAL's defaults: `knn` 8, `thresh1` 25 and
/// `thresh2` 6.
pub fn approximate_coplanar(cloud: &PointCloud, knn: usize, thresh1: f64, thresh2: f64) -> Result<PointCloud> {
    if knn < 2 {
        return Err(Error::InvalidArgument("knn must be at least 2".into()));
    }
    let nb = Neighbourhood { knn: knn - 1, stride: 1, radius: None, min_k: 0 };
    let vals = eigenvalues_of(&covariances(cloud, &nb, None)?);
    let mut flags: Vec<u8> = f64_or_zeros(cloud, "coplanar").iter().map(|v| *v as u8).collect();
    for (f, v) in flags.iter_mut().zip(&vals) {
        if let Some(ev) = v {
            *f = u8::from(ev[1] > thresh1 * ev[0] && thresh2 * ev[1] > ev[2]);
        }
    }
    let mut out = cloud.clone();
    set_column(&mut out, "coplanar", Values::U8(flags));
    Ok(out)
}

/// Estimates the rank of each point's neighbourhood, as PDAL's `filters.estimaterank`: the
/// number of singular values of the covariance of its `knn` nearest points (itself included)
/// at least `thresh` (rounded to a float, as PDAL does) times the largest, in `rank`. PDAL's
/// defaults: `knn` 8 and `thresh` 0.01.
pub fn estimate_rank(cloud: &PointCloud, knn: usize, thresh: f64) -> Result<PointCloud> {
    if knn < 2 {
        return Err(Error::InvalidArgument("knn must be at least 2".into()));
    }
    let nb = Neighbourhood { knn: knn - 1, stride: 1, radius: None, min_k: 0 };
    let covs = covariances(cloud, &nb, None)?;
    let threshold = thresh as f32 as f64;
    let ranks: Vec<u8> = par_map(
        &covs,
        || (),
        |_, c| {
            let Some(c) = c else { return 0 };
            // The singular values of a symmetric matrix are its eigenvalues' magnitudes; Eigen's
            // JacobiSVD counts those at least the threshold times the largest.
            let mut sv = eigen_symmetric(*c).0.map(f64::abs);
            sv.sort_by(|a, b| b.total_cmp(a));
            let cut = (sv[0] * threshold).max(f64::MIN_POSITIVE);
            sv.iter().filter(|v| **v != 0.0 && **v >= cut).count() as u8
        },
    );
    let mut out = cloud.clone();
    set_column(&mut out, "rank", Values::U8(ranks));
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

    #[test]
    fn decomposes_as_eigen_does() {
        // Eigen's solver and the Jacobi rotations agree, diagonal and tridiagonal matrices
        // included; the eigenvectors are orthonormal and diagonalize the matrix.
        let mut s = 11u64;
        let mut r = || {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((s >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
        };
        let mut cases = vec![
            [[2.0, 0.0, 0.0], [0.0, 3.0, 0.0], [0.0, 0.0, 1.0]],
            [[2.0, 1.0, 0.0], [1.0, 3.0, 0.5], [0.0, 0.5, 1.0]],
            [[1.0, 0.0, 0.3], [0.0, 1.0, 0.0], [0.3, 0.0, 1.0]],
            [[0.0; 3]; 3],
        ];
        for _ in 0..200 {
            let (a, b, c, d, e, f) = (r(), r(), r(), r(), r(), r());
            cases.push([[a, b, c], [b, d, e], [c, e, f]]);
        }
        for m in cases {
            let (vals, vecs) = eigen_selfadjoint(m);
            let (jv, _) = eigen_symmetric(m);
            for k in 0..3 {
                assert!((vals[k] - jv[k]).abs() < 1e-12, "{m:?}: {vals:?} vs {jv:?}");
                // M v = λ v.
                for row in 0..3 {
                    let mv: f64 = (0..3).map(|c| m[row][c] * vecs[c][k]).sum();
                    assert!((mv - vals[k] * vecs[row][k]).abs() < 1e-12, "{m:?}");
                }
            }
        }
    }
}

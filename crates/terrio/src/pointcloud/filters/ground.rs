//! Ground and terrain filters: PDAL's `filters.pmf` (progressive morphological filter),
//! `filters.elm` (extended local minimum), `filters.hag_nn` and `filters.hag_delaunay` (height
//! above ground).

use super::super::kdtree::NanoKdTree;
use super::{DimRange, dilate_diamond, erode_diamond, par_map, range_mask, set_classes, xyz};
use crate::pointcloud::{Dimension, PointCloud, ReturnKind, Values};
use crate::{Error, Result};
use std::collections::BTreeMap;

/// The points PDAL's ground filters classify: those not ignored, of the returns asked for (all
/// when the points have no return numbers, or all of them are 0).
pub(super) fn classified_points(cloud: &PointCloud, ignore: &[DimRange], returns: &[ReturnKind]) -> Result<Vec<usize>> {
    let n = cloud.len();
    let ignored = if ignore.is_empty() { vec![false; n] } else { range_mask(cloud, ignore)? };
    let mut kept: Vec<usize> = (0..n).filter(|&i| !ignored[i]).collect();
    if let (Some(rn), Some(nr)) = (cloud.dim("return_number"), cloud.dim("number_of_returns")) {
        let get = |d: &Dimension, i: usize| d.values.get_f64(i) as u8;
        let (mut nr_one_zero, mut rn_one_zero, mut nr_all_zero, mut rn_all_zero) = (false, false, true, true);
        for &i in &kept {
            let (r, m) = (get(rn, i), get(nr, i));
            nr_one_zero |= m == 0;
            rn_one_zero |= r == 0;
            nr_all_zero &= m == 0;
            rn_all_zero &= r == 0;
        }
        if (nr_one_zero || rn_one_zero) && !(nr_all_zero && rn_all_zero) {
            return Err(Error::InvalidData(
                "some number_of_returns or return_number values are 0, but not all: they should all be 1 or more".into(),
            ));
        }
        if !(nr_all_zero && rn_all_zero) && !returns.is_empty() {
            kept.retain(|&i| {
                let (r, m) = (get(rn, i), get(nr, i));
                returns.iter().any(|k| match k {
                    ReturnKind::First => r == 1 && m > 1,
                    ReturnKind::Intermediate => r > 1 && r < m,
                    ReturnKind::Last => r == m && m > 1,
                    ReturnKind::Only => m == 1,
                })
            });
        }
    }
    if kept.is_empty() {
        return Err(Error::InvalidData("no returns to process".into()));
    }
    Ok(kept)
}

/// How to classify ground points with the progressive morphological filter (PDAL's
/// `filters.pmf`, Zhang et al. 2003).
#[derive(Debug, Clone, PartialEq)]
pub struct PmfOptions {
    /// The cell size. 1 by default.
    pub cell: f64,
    /// Whether windows grow exponentially (1, 3, 5, 9, 17...) rather than linearly. True by
    /// default.
    pub exponential: bool,
    /// The initial height threshold. 0.15 by default.
    pub initial_distance: f64,
    /// The largest height threshold. 2.5 by default.
    pub max_distance: f64,
    /// The largest window size. 33 by default.
    pub max_window: f64,
    /// The slope. 1 by default.
    pub slope: f64,
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

impl Default for PmfOptions {
    fn default() -> Self {
        Self {
            cell: 1.0,
            exponential: true,
            initial_distance: 0.15,
            max_distance: 2.5,
            max_window: 33.0,
            slope: 1.0,
            ignore: Vec::new(),
            returns: vec![ReturnKind::Last, ReturnKind::Only],
            ground_class: 2,
            other_class: 1,
            only_ground: false,
        }
    }
}

/// Classifies ground points with the progressive morphological filter, as PDAL's `filters.pmf`.
pub fn pmf(cloud: &PointCloud, o: &PmfOptions) -> Result<PointCloud> {
    let classes = pmf_classes(cloud, o)?;
    let mut out = cloud.clone();
    set_classes(&mut out, |i| classes[i]);
    Ok(out)
}

/// The classification PMF gives each point (`None`: left alone).
pub fn pmf_classes(cloud: &PointCloud, o: &PmfOptions) -> Result<Vec<Option<u8>>> {
    let cell = o.cell;
    if !(cell > 0.0 && cell.is_finite()) {
        return Err(Error::InvalidArgument(format!("invalid cell {cell}")));
    }
    if o.ground_class == o.other_class && !o.only_ground {
        return Err(Error::InvalidArgument("the ground and other classes cannot be equal unless only ground is set".into()));
    }
    let (xs, ys, zs) = xyz(cloud)?;
    let inliers = classified_points(cloud, &o.ignore, &o.returns)?;
    let mut out = vec![None; cloud.len()];
    if !o.only_ground {
        for &i in &inliers {
            out[i] = Some(o.other_class);
        }
    }
    let (mut minx, mut miny, mut maxx, mut maxy) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for &i in &inliers {
        minx = minx.min(xs[i]);
        maxx = maxx.max(xs[i]);
        miny = miny.min(ys[i]);
        maxy = maxy.max(ys[i]);
    }
    let cols = ((maxx - minx) / cell + 1.0) as usize;
    let rows = ((maxy - miny) / cell + 1.0) as usize;
    if cols.saturating_mul(rows) > 2_000_000_000 {
        return Err(Error::InvalidArgument(format!("PMF grid of {cols} x {rows} cells: use a larger cell")));
    }
    // The minimum surface (PDAL's cells: the floor of the offset, then divided by the cell).
    let mut zimin = vec![f64::NAN; rows * cols];
    for &i in &inliers {
        let c = ((xs[i] - minx).floor() / cell) as usize;
        let r = ((ys[i] - miny).floor() / cell) as usize;
        let k = c * rows + r;
        if k < zimin.len() && (zs[i] < zimin[k] || zimin[k].is_nan()) {
            zimin[k] = zs[i];
        }
    }
    // Empty cells take the value of the nearest cell with one (by the distance between centres).
    let center = |c: usize, r: usize| [minx + (c as f64 + 0.5) * cell, miny + (r as f64 + 0.5) * cell];
    let mut pts = Vec::new();
    let mut vals = Vec::new();
    for c in 0..cols {
        for r in 0..rows {
            let v = zimin[c * rows + r];
            if !v.is_nan() {
                pts.push(center(c, r));
                vals.push(v);
            }
        }
    }
    if !pts.is_empty() && pts.len() < zimin.len() {
        let tree = NanoKdTree::new(pts, 100);
        let empty: Vec<usize> = (0..rows * cols).filter(|k| zimin[*k].is_nan()).collect();
        let filled = par_map(&empty, Vec::new, |found, &k| {
            tree.knn(&center(k / rows, k % rows), 1, found);
            vals[found[0].1]
        });
        for (k, v) in empty.into_iter().zip(filled) {
            zimin[k] = v;
        }
    }
    // The windows and height thresholds.
    let mut ws_vec: Vec<f64> = Vec::new();
    let mut ht_vec: Vec<f64> = Vec::new();
    let mut ws = 0.0;
    let mut iter = 0i32;
    while ws < o.max_window {
        ws = if o.exponential { cell * (2.0 * 2f64.powi(iter) + 1.0) } else { cell * (2.0 * (iter + 1) as f64 * 2.0 + 1.0) };
        let mut ht = if iter == 0 { o.initial_distance } else { o.slope * (ws - ws_vec[iter as usize - 1]) * cell + o.initial_distance };
        if ht > o.max_distance {
            ht = o.max_distance;
        }
        ws_vec.push(ws);
        ht_vec.push(ht);
        iter += 1;
    }
    let mut ground = inliers.clone();
    for (ws, ht) in ws_vec.iter().zip(&ht_vec) {
        let iters = (0.5 * (ws - 1.0)) as i32;
        if iters > 0 {
            erode_diamond(&mut zimin, rows, cols, iters as usize);
            dilate_diamond(&mut zimin, rows, cols, iters as usize);
        }
        ground.retain(|&i| {
            let c = ((xs[i] - minx) / cell).floor() as usize;
            let r = ((ys[i] - miny) / cell).floor() as usize;
            zs[i] - zimin[c * rows + r] < *ht
        });
    }
    for i in ground {
        out[i] = Some(o.ground_class);
    }
    Ok(out)
}

/// Classifies low noise with the extended local minimum, as PDAL's `filters.elm`: in each cell
/// (of `cell`, 10 by default), from the lowest point up, points more than `threshold` (1 by
/// default) below the next become `class` (7, low point, by default).
pub fn elm(cloud: &PointCloud, cell: f64, threshold: f64, class: u8) -> Result<PointCloud> {
    if !(cell > 0.0 && cell.is_finite()) {
        return Err(Error::InvalidArgument(format!("invalid cell {cell}")));
    }
    let (xs, ys, zs) = xyz(cloud)?;
    let mut out = cloud.clone();
    if cloud.is_empty() {
        return Ok(out);
    }
    let (mut minx, mut miny, mut maxy) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY);
    for i in 0..cloud.len() {
        minx = minx.min(xs[i]);
        miny = miny.min(ys[i]);
        maxy = maxy.max(ys[i]);
    }
    let rows = ((maxy - miny) / cell + 1.0) as usize;
    // The points of each cell, by elevation (as a multimap: equal elevations in point order).
    let mut cells: BTreeMap<u32, Vec<(f64, usize)>> = BTreeMap::new();
    for i in 0..cloud.len() {
        // PDAL's cells: the floor of the offset, then divided by the cell.
        let c = ((xs[i] - minx).floor() / cell) as usize;
        let r = ((ys[i] - miny).floor() / cell) as usize;
        cells.entry((c * rows + r) as u32).or_default().push((zs[i], i));
    }
    let mut noise = vec![false; cloud.len()];
    for ids in cells.values_mut() {
        if ids.len() <= 1 {
            continue;
        }
        ids.sort_by(|a, b| a.0.total_cmp(&b.0));
        for k in 0..ids.len() - 1 {
            if (ids[k].0 - ids[k + 1].0).abs() < threshold {
                break;
            }
            noise[ids[k].1] = true;
        }
    }
    set_classes(&mut out, |i| noise[i].then_some(class));
    Ok(out)
}

/// How to compute heights above ground (PDAL's `filters.hag_nn` and `filters.hag_delaunay`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HagOptions {
    /// The ground points looked at for each point: 1 for `hag_nn` by default, 10 for
    /// `hag_delaunay`.
    pub count: usize,
    /// `hag_nn`: ground points farther than this are not used (0: no limit).
    pub max_distance: f64,
    /// Interpolate for points outside the ground points' bounds (else their height is 0). True by
    /// default.
    pub allow_extrapolation: bool,
    /// The class of ground points. 2 by default.
    pub class: u8,
}

impl HagOptions {
    /// `hag_nn`'s defaults.
    pub fn nn() -> Self {
        Self { count: 1, max_distance: 0.0, allow_extrapolation: true, class: 2 }
    }

    /// `hag_delaunay`'s defaults.
    pub fn delaunay() -> Self {
        Self { count: 10, ..Self::nn() }
    }
}

/// The ground points and their 2D index, for the height above ground.
struct Ground {
    tree: NanoKdTree<2>,
    idx: Vec<usize>,
    bounds: [f64; 4],
}

fn ground(cloud: &PointCloud, class: u8) -> Result<(Vec<bool>, Option<Ground>)> {
    let classes = cloud.dim("classification").ok_or_else(|| Error::InvalidArgument("the points have no classification".into()))?;
    let (xs, ys, _) = xyz(cloud)?;
    let is_ground: Vec<bool> = (0..cloud.len()).map(|i| classes.values.get_f64(i) as u8 == class).collect();
    let idx: Vec<usize> = (0..cloud.len()).filter(|&i| is_ground[i]).collect();
    if idx.is_empty() {
        return Ok((is_ground, None));
    }
    let mut b = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    for &i in &idx {
        b = [b[0].min(xs[i]), b[1].min(ys[i]), b[2].max(xs[i]), b[3].max(ys[i])];
    }
    let tree = NanoKdTree::new(idx.iter().map(|&i| [xs[i], ys[i]]).collect(), 100);
    Ok((is_ground, Some(Ground { tree, idx, bounds: b })))
}

fn with_hag(cloud: &PointCloud, hag: Vec<f64>) -> PointCloud {
    let mut out = cloud.clone();
    match out.dim_mut("height_above_ground") {
        Some(d) => d.values = Values::F64(hag),
        None => out.dimensions.push(Dimension::new("height_above_ground", Values::F64(hag))),
    }
    out
}

/// Heights above ground from the nearest ground points (inverse-distance weighted), as PDAL's
/// `filters.hag_nn`: the `height_above_ground` dimension.
pub fn hag_nn(cloud: &PointCloud, o: &HagOptions) -> Result<PointCloud> {
    if o.count == 0 {
        return Err(Error::InvalidArgument("the count must be a positive integer".into()));
    }
    let (is_ground, g) = ground(cloud, o.class)?;
    let Some(g) = g else { return Ok(with_hag(cloud, vec![0.0; cloud.len()])) };
    let (xs, ys, zs) = xyz(cloud)?;
    let max2 = o.max_distance * o.max_distance;
    let all: Vec<usize> = (0..cloud.len()).collect();
    let hag = par_map(&all, Vec::new, |found, &i| {
        if is_ground[i] {
            return 0.0;
        }
        let (x0, y0, z0) = (xs[i], ys[i], zs[i]);
        g.tree.knn(&[x0, y0], o.count, found);
        let j = g.idx[found[0].1];
        let z1 = if (x0 == xs[j] && y0 == ys[j]) || found.len() == 1 {
            zs[j]
        } else if !inside(&g.bounds, x0, y0) && !o.allow_extrapolation {
            z0
        } else {
            let (mut weights, mut acc) = (0.0, 0.0);
            for (d2, k) in found.iter() {
                if max2 > 0.0 && *d2 > max2 {
                    break;
                }
                let w = 1.0 / d2;
                weights += w;
                acc += w * zs[g.idx[*k]];
            }
            if weights != 0.0 { acc / weights } else { z0 }
        };
        z0 - z1
    });
    Ok(with_hag(cloud, hag))
}

fn inside(b: &[f64; 4], x: f64, y: f64) -> bool {
    x >= b[0] && x <= b[2] && y >= b[1] && y <= b[3]
}

/// PDAL's barycentric interpolation: the z of a point in a triangle, or `None` outside it (a
/// point on an edge, within a tolerance, is inside).
#[allow(clippy::too_many_arguments)]
fn barycentric(x1: f64, y1: f64, z1: f64, x2: f64, y2: f64, z2: f64, x3: f64, y3: f64, z3: f64, x: f64, y: f64) -> Option<f64> {
    let total = (x2 - x1) * (y3 - y2) - (y2 - y1) * (x3 - x2);
    if total == 0.0 {
        return None;
    }
    let sign = total.is_sign_negative();
    let mag2 = |ax: f64, ay: f64, bx: f64, by: f64| (bx - ax) * (bx - ax) + (by - ay) * (by - ay);
    let side = |area: f64, m: f64| -> Option<f64> {
        if area != 0.0 && area.is_sign_negative() != sign {
            if (area / m).abs() > 1e-14 {
                return None;
            }
            return Some(0.0);
        }
        Some(area)
    };
    let a12 = side((x2 - x1) * (y - y1) - (y2 - y1) * (x - x1), mag2(x1, y1, x2, y2) + mag2(x1, y1, x, y))?;
    let a23 = side((x3 - x2) * (y - y2) - (y3 - y2) * (x - x2), mag2(x3, y3, x2, y2) + mag2(x3, y3, x, y))?;
    let a31 = side((x1 - x3) * (y - y3) - (y1 - y3) * (x - x3), mag2(x3, y3, x1, y1) + mag2(x3, y3, x, y))?;
    Some((a12 * z3 + a23 * z1 + a31 * z2) / total)
}

/// Heights above ground from a Delaunay triangulation of the nearest ground points, as PDAL's
/// `filters.hag_delaunay`: the `height_above_ground` dimension.
pub fn hag_delaunay(cloud: &PointCloud, o: &HagOptions) -> Result<PointCloud> {
    if o.count < 3 {
        return Err(Error::InvalidArgument("the count must be at least 3".into()));
    }
    let (is_ground, g) = ground(cloud, o.class)?;
    let Some(g) = g else { return Ok(with_hag(cloud, vec![0.0; cloud.len()])) };
    let (xs, ys, zs) = xyz(cloud)?;
    let all: Vec<usize> = (0..cloud.len()).collect();
    let hag = par_map(&all, Vec::new, |found, &i| {
        if is_ground[i] {
            return 0.0;
        }
        let (x0, y0, z0) = (xs[i], ys[i], zs[i]);
        g.tree.knn(&[x0, y0], o.count, found);
        let ids: Vec<usize> = found.iter().map(|(_, k)| g.idx[*k]).collect();
        let j = ids[0];
        let z1 = if (x0 == xs[j] && y0 == ys[j]) || ids.len() == 1 {
            zs[j]
        } else if !inside(&g.bounds, x0, y0) && !o.allow_extrapolation {
            z0
        } else {
            let pts: Vec<delaunator::Point> = ids.iter().map(|&k| delaunator::Point { x: xs[k], y: ys[k] }).collect();
            let t = delaunator::triangulate(&pts);
            if t.triangles.is_empty() {
                // No triangulation (collinear points): as PDAL, the point's own z.
                z0
            } else {
                t.triangles
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .find_map(|tri| {
                        let (a, b, c) = (ids[tri[0]], ids[tri[1]], ids[tri[2]]);
                        barycentric(xs[a], ys[a], zs[a], xs[b], ys[b], zs[b], xs[c], ys[c], zs[c], x0, y0)
                    })
                    .unwrap_or(zs[j])
            }
        };
        z0 - z1
    });
    Ok(with_hag(cloud, hag))
}

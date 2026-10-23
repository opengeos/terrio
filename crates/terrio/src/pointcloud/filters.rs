//! Point cloud filters, as PDAL's: `filters.range`, `filters.sample`,
//! `filters.voxelcenternearestneighbor`, `filters.voxeldownsize`, `filters.outlier`,
//! `filters.smrf`, `filters.pmf`, `filters.csf` and `filters.skewnessbalancing` (ground
//! classification), `filters.elm` (low noise), `filters.hag_nn` and `filters.hag_delaunay`
//! (height above ground), `filters.crop`, `filters.decimation`, `filters.expression`,
//! `filters.assign`, `filters.merge`, `filters.head`, `filters.tail`, `filters.sort`,
//! `filters.locate`, `filters.iqr`, `filters.mad`, `filters.splitter`, `filters.chipper`,
//! `filters.normal`, `filters.cluster`, `filters.dbscan`, `filters.covariancefeatures`,
//! `filters.eigenvalues`, `filters.approximatecoplanar`, `filters.estimaterank`,
//! `filters.lof`, `filters.nndistance`, `filters.radialdensity`, `filters.reciprocity`,
//! `filters.miniball`, `filters.zsmooth`, `filters.dem`, `filters.colorization` and
//! `filters.overlay`, with PDAL's options, defaults and results.

mod csf;
pub mod expr;
mod features;
mod ground;
mod neighbourhood;
mod overlay;
mod select;
mod tiles;

pub use csf::{CsfOptions, csf, csf_classes};
pub use features::{
    ClusterOptions, CovarianceFeaturesOptions, EigenvaluesOptions, FeatureMode, Neighbourhood, NormalOptions, approximate_coplanar,
    cluster, covariance_features, eigenvalues, estimate_rank, normal,
};
pub use ground::{HagOptions, PmfOptions, elm, hag_delaunay, hag_nn, pmf, pmf_classes, skewness_balancing};
pub use neighbourhood::{DbscanOptions, NnDistanceMode, dbscan, lof, miniball, nndistance, radial_density, reciprocity, zsmooth};
pub use overlay::{BandDimension, OverlayOptions, colorization, dem, overlay};
pub use select::{
    AssignOptions, CropBox, CropOptions, assign, crop, crop_indices, decimation, expression, head, iqr, locate, mad, merge, sort, tail,
};
pub use tiles::{SplitterOptions, chipper, splitter};

use super::kdtree::KdTree;
use super::{PointCloud, ReturnKind, Values};
use crate::{Error, Result};
use std::collections::{BTreeMap, HashMap, HashSet};

/// A dimension's range, as PDAL writes them: `Name[lower:upper]`, with `(` or `)` for exclusive
/// bounds, either bound left out for none, and `!` after the name to negate the range
/// (`Classification[2:2]`, `Z[:100)`, `Classification![7:7]`). Names are PDAL's or terrio's, in
/// any case.
#[derive(Debug, Clone, PartialEq)]
pub struct DimRange {
    /// The dimension, as terrio names it.
    pub name: String,
    /// The lower bound (`f64::MIN` when there is none).
    pub lower: f64,
    /// The upper bound (`f64::MAX` when there is none).
    pub upper: f64,
    /// Whether the lower bound is included.
    pub lower_inclusive: bool,
    /// Whether the upper bound is included.
    pub upper_inclusive: bool,
    /// Whether the range is negated: the values outside it pass.
    pub negate: bool,
}

impl DimRange {
    /// Parses a range.
    pub fn parse(s: &str) -> Result<Self> {
        let err = |m: &str| Error::InvalidArgument(format!("invalid range '{s}': {m}"));
        let t = s.trim();
        let name_end = t.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).ok_or_else(|| err("missing '(' or '['"))?;
        if name_end == 0 {
            return Err(err("no dimension name"));
        }
        let name = super::names::from_any(&t[..name_end]);
        let mut rest = &t[name_end..];
        let negate = rest.starts_with('!');
        if negate {
            rest = &rest[1..];
        }
        let lower_inclusive = match rest.chars().next() {
            Some('[') => true,
            Some('(') => false,
            _ => return Err(err("missing '(' or '['")),
        };
        rest = &rest[1..];
        let (lower, r) = number_prefix(rest);
        rest = r.trim_start();
        let lower = lower.unwrap_or(f64::MIN);
        rest = rest.strip_prefix(':').ok_or_else(|| err("missing ':' limit separator"))?;
        let (upper, r) = number_prefix(rest);
        rest = r.trim_start();
        let upper = upper.unwrap_or(f64::MAX);
        let upper_inclusive = match rest.chars().next() {
            Some(']') => true,
            Some(')') => false,
            _ => return Err(err("missing ')' or ']'")),
        };
        if !rest[1..].trim().is_empty() {
            return Err(err("invalid characters following valid range"));
        }
        Ok(Self { name, lower, upper, lower_inclusive, upper_inclusive, negate })
    }

    /// Parses ranges separated by commas.
    pub fn parse_list(s: &str) -> Result<Vec<Self>> {
        let mut out = Vec::new();
        let mut depth = 0;
        let mut start = 0;
        for (i, c) in s.char_indices() {
            match c {
                '[' | '(' => depth += 1,
                ']' | ')' => depth -= 1,
                ',' if depth == 0 => {
                    out.push(Self::parse(&s[start..i])?);
                    start = i + 1;
                }
                _ => {}
            }
        }
        if !s[start..].trim().is_empty() {
            out.push(Self::parse(&s[start..])?);
        }
        Ok(out)
    }

    /// Whether a value passes (NaN never does).
    pub fn passes(&self, v: f64) -> bool {
        let fail = v.is_nan()
            || (self.lower_inclusive && v < self.lower)
            || (!self.lower_inclusive && v <= self.lower)
            || (self.upper_inclusive && v > self.upper)
            || (!self.upper_inclusive && v >= self.upper);
        if self.negate { fail } else { !fail }
    }
}

/// The longest prefix of `s` (after spaces) that is a number, as C++'s `>>` reads it.
fn number_prefix(s: &str) -> (Option<f64>, &str) {
    let t = s.trim_start();
    let mut best = None;
    for end in (1..=t.len().min(64)).rev() {
        if !t.is_char_boundary(end) {
            continue;
        }
        let p = &t[..end];
        if p.ends_with(|c: char| c.is_ascii_alphabetic() && c != 'e' && c != 'E') {
            continue;
        }
        if let Ok(v) = p.parse::<f64>() {
            best = Some((v, end));
            break;
        }
    }
    match best {
        Some((v, end)) => (Some(v), &t[end..]),
        None => (None, t),
    }
}

/// Which points pass ranges: a range of each dimension named (any of a dimension's ranges, all
/// the dimensions), as PDAL's `filters.range`.
pub fn range_mask(cloud: &PointCloud, ranges: &[DimRange]) -> Result<Vec<bool>> {
    let mut by_dim: BTreeMap<&str, Vec<&DimRange>> = BTreeMap::new();
    for r in ranges {
        by_dim.entry(r.name.as_str()).or_default().push(r);
    }
    let mut keep = vec![true; cloud.len()];
    for (name, rs) in by_dim {
        let d = cloud.dim(name).ok_or_else(|| {
            Error::InvalidArgument(format!("invalid dimension name '{name}' (dimensions: {})", cloud.dimension_names().join(", ")))
        })?;
        for (i, k) in keep.iter_mut().enumerate() {
            if *k {
                let v = d.get(i);
                *k = rs.iter().any(|r| r.passes(v));
            }
        }
    }
    Ok(keep)
}

/// The points that pass ranges (PDAL's `filters.range` `limits`).
pub fn range(cloud: &PointCloud, ranges: &[DimRange]) -> Result<PointCloud> {
    Ok(cloud.select(&range_mask(cloud, ranges)?))
}

/// A dimension of the points, named as PDAL or terrio names it, in any case.
fn find_dim<'a>(cloud: &'a PointCloud, name: &str) -> Result<&'a super::Dimension> {
    cloud.dim(&super::names::from_any(name)).or_else(|| cloud.dim(name)).ok_or_else(|| {
        Error::InvalidArgument(format!("invalid dimension name '{name}' (dimensions: {})", cloud.dimension_names().join(", ")))
    })
}

/// The name of the dimension a filter writes: the points' dimension of that name (PDAL's or
/// terrio's, in any case), or PDAL's name in terrio's convention (snake case) for a new one.
fn output_name(cloud: &PointCloud, name: &str) -> String {
    find_dim(cloud, name).map_or_else(|_| super::names::from_pdal(name.trim()), |d| d.name.clone())
}

fn xyz(cloud: &PointCloud) -> Result<(&[f64], &[f64], &[f64])> {
    let get = |n: &str| cloud.coords(n).ok_or_else(|| Error::InvalidArgument(format!("the points have no {n}")));
    Ok((get("x")?, get("y")?, get("z")?))
}

/// How to sample points (PDAL's `filters.sample`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SampleOptions {
    /// The voxel size; the radius is then `cell / 2 × √3`. One of `cell` and `radius` is needed.
    pub cell: Option<f64>,
    /// The smallest distance between points kept; the voxels are then `2 × radius / √3`.
    pub radius: Option<f64>,
    /// The voxels' origin. By default, the first point's `x`, `y`, and (as PDAL 2.9 has it) `y`.
    pub origin: Option<[f64; 3]>,
}

/// Poisson sampling: keeps each point, in order, that is at least `radius` from every point kept
/// before it, as PDAL's `filters.sample`.
pub fn sample(cloud: &PointCloud, options: &SampleOptions) -> Result<PointCloud> {
    Ok(cloud.select(&sample_mask(cloud, options)?))
}

/// Which points [`sample`] keeps.
pub fn sample_mask(cloud: &PointCloud, options: &SampleOptions) -> Result<Vec<bool>> {
    let (cell, radius) = match (options.cell, options.radius) {
        (Some(_), Some(_)) => return Err(Error::InvalidArgument("set only one of cell and radius".into())),
        (None, None) => return Err(Error::InvalidArgument("set cell or radius".into())),
        (Some(c), None) => (c, c / 2.0 * 3f64.sqrt()),
        (None, Some(r)) => (2.0 * r / 3f64.sqrt(), r),
    };
    if !(cell > 0.0 && cell.is_finite()) {
        return Err(Error::InvalidArgument(format!("invalid cell {cell}")));
    }
    let (xs, ys, zs) = xyz(cloud)?;
    let r2 = radius * radius;
    let mut voxels: HashMap<(i64, i64, i64), Vec<[f64; 3]>> = HashMap::new();
    let mut origin = options.origin.unwrap_or([0.0; 3]);
    let mut keep = vec![false; cloud.len()];
    for i in 0..cloud.len() {
        let (x, y, z) = (xs[i], ys[i], zs[i]);
        if voxels.is_empty() && options.origin.is_none() {
            origin = [x, y, y];
        }
        let v = (((x - origin[0]) / cell).floor() as i64, ((y - origin[1]) / cell).floor() as i64, ((z - origin[2]) / cell).floor() as i64);
        let near = |p: &[f64; 3]| (p[0] - x) * (p[0] - x) + (p[1] - y) * (p[1] - y) + (p[2] - z) * (p[2] - z) < r2;
        let mut ok = true;
        'search: for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    if let Some(list) = voxels.get(&(v.0 + dx, v.1 + dy, v.2 + dz))
                        && list.iter().any(near)
                    {
                        ok = false;
                        break 'search;
                    }
                }
            }
        }
        if ok {
            voxels.entry(v).or_default().push([x, y, z]);
            keep[i] = true;
        }
    }
    Ok(keep)
}

/// Keeps, of the points in each voxel of a grid starting at the points' minimum, the one
/// nearest the voxel's center, as PDAL's `filters.voxelcenternearestneighbor` (in its order: by
/// voxel row, column and layer).
pub fn voxel_center_nearest_neighbor(cloud: &PointCloud, cell: f64) -> Result<PointCloud> {
    if !(cell > 0.0 && cell.is_finite()) {
        return Err(Error::InvalidArgument(format!("invalid cell {cell}")));
    }
    let Some(b) = cloud.bounds() else { return Ok(cloud.clone()) };
    let (xs, ys, zs) = xyz(cloud)?;
    let mut voxels: BTreeMap<(usize, usize, usize), (usize, f64)> = BTreeMap::new();
    for i in 0..cloud.len() {
        let (x, y, z) = (xs[i], ys[i], zs[i]);
        let c = ((x - b[0]) / cell) as usize;
        let r = ((y - b[1]) / cell) as usize;
        let d = ((z - b[2]) / cell) as usize;
        let xv = b[0] + (c as f64 + 0.5) * cell;
        let yv = b[1] + (r as f64 + 0.5) * cell;
        let zv = b[2] + (d as f64 + 0.5) * cell;
        let dist = (xv - x).powi(2) + (yv - y).powi(2) + (zv - z).powi(2);
        let e = voxels.entry((r, c, d)).or_insert((i, dist));
        if dist < e.1 {
            *e = (i, dist);
        }
    }
    let idx: Vec<usize> = voxels.values().map(|(i, _)| *i).collect();
    Ok(cloud.take(&idx))
}

/// How [`voxel_downsize`] keeps a voxel's point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VoxelMode {
    /// The first point, moved to the voxel's center (PDAL's default).
    #[default]
    Center,
    /// The first point, where it is.
    First,
}

impl VoxelMode {
    /// Parses `center` or `first`.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "center" => Ok(VoxelMode::Center),
            "first" => Ok(VoxelMode::First),
            o => Err(Error::InvalidArgument(format!("invalid voxel mode '{o}' (center, first)"))),
        }
    }
}

/// Keeps the first point of each voxel of a grid centred on the first point, as PDAL's
/// `filters.voxeldownsize`; in `center` mode, moved to the voxel's center.
pub fn voxel_downsize(cloud: &PointCloud, cell: f64, mode: VoxelMode) -> Result<PointCloud> {
    if !(cell > 0.0 && cell.is_finite()) {
        return Err(Error::InvalidArgument(format!("invalid cell {cell}")));
    }
    let (xs, ys, zs) = xyz(cloud)?;
    let mut seen: HashSet<(i64, i64, i64)> = HashSet::new();
    let mut origin = [0.0; 3];
    let mut idx = Vec::new();
    let mut centers = Vec::new();
    for i in 0..cloud.len() {
        if seen.is_empty() {
            origin = [xs[i] - cell / 2.0, ys[i] - cell / 2.0, zs[i] - cell / 2.0];
        }
        let v = (
            ((xs[i] - origin[0]) / cell).floor() as i64,
            ((ys[i] - origin[1]) / cell).floor() as i64,
            ((zs[i] - origin[2]) / cell).floor() as i64,
        );
        if seen.insert(v) {
            idx.push(i);
            centers.push([
                (v.0 as f64 + 0.5) * cell + origin[0],
                (v.1 as f64 + 0.5) * cell + origin[1],
                (v.2 as f64 + 0.5) * cell + origin[2],
            ]);
        }
    }
    let mut out = cloud.take(&idx);
    if mode == VoxelMode::Center {
        for (k, name) in ["x", "y", "z"].iter().enumerate() {
            if let Some(d) = out.dim_mut(name) {
                d.values = Values::F64(centers.iter().map(|c| c[k]).collect());
            }
        }
    }
    Ok(out)
}

/// How [`outlier`] finds outliers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutlierMethod {
    /// Points whose mean distance to their `mean_k` nearest neighbours is `multiplier` standard
    /// deviations above the mean of those distances.
    #[default]
    Statistical,
    /// Points with at most `min_k` neighbours within `radius`.
    Radius,
}

impl OutlierMethod {
    /// Parses `statistical` or `radius`.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "statistical" => Ok(OutlierMethod::Statistical),
            "radius" => Ok(OutlierMethod::Radius),
            o => Err(Error::InvalidArgument(format!("invalid outlier method '{o}' (statistical, radius)"))),
        }
    }
}

/// How to find outliers (PDAL's `filters.outlier`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutlierOptions {
    /// The method. Statistical by default.
    pub method: OutlierMethod,
    /// Radius: the fewest neighbours within the radius for a point to be kept. 2 by default.
    pub min_k: usize,
    /// Radius: the radius. 1 by default.
    pub radius: f64,
    /// Statistical: the number of neighbours. 8 by default.
    pub mean_k: usize,
    /// Statistical: the number of standard deviations. 2 by default.
    pub multiplier: f64,
    /// The classification given to outliers. 7 (low point, noise) by default.
    pub class: u8,
}

impl Default for OutlierOptions {
    fn default() -> Self {
        Self { method: OutlierMethod::Statistical, min_k: 2, radius: 1.0, mean_k: 8, multiplier: 2.0, class: 7 }
    }
}

/// Which points are outliers, as PDAL's `filters.outlier` finds them.
pub fn outlier_mask(cloud: &PointCloud, options: &OutlierOptions) -> Result<Vec<bool>> {
    let (xs, ys, zs) = xyz(cloud)?;
    let n = cloud.len();
    let tree = KdTree::new((0..n).map(|i| [xs[i], ys[i], zs[i]]).collect());
    let all: Vec<usize> = (0..n).collect();
    match options.method {
        OutlierMethod::Radius => Ok(par_map(&all, || (), |_, &i| tree.count_within(tree.point(i), options.radius) <= options.min_k)),
        OutlierMethod::Statistical => {
            // The mean distance to the nearest neighbours, the point itself (at distance 0)
            // aside.
            let count = options.mean_k + 1;
            let distances: Vec<f64> = par_map(&all, Vec::new, |found, &i| {
                tree.knn(tree.point(i), count, found);
                let mut mean = 0.0;
                for (j, (d2, _)) in found.iter().enumerate().skip(1) {
                    mean += (d2.sqrt() - mean) / j as f64;
                }
                mean
            });
            let (mut k, mut m1, mut m2) = (0usize, 0.0f64, 0.0f64);
            for d in &distances {
                let k1 = k as f64;
                k += 1;
                let delta = d - m1;
                let delta_n = delta / k as f64;
                m1 += delta_n;
                m2 += delta * delta_n * k1;
            }
            let stdev = (m2 / (k as f64 - 1.0)).sqrt();
            let threshold = m1 + options.multiplier * stdev;
            // NaN distances are outliers too, as in PDAL (`d < threshold` fails).
            Ok(distances.iter().map(|d| d.partial_cmp(&threshold) != Some(std::cmp::Ordering::Less)).collect())
        }
    }
}

/// Classifies outliers (PDAL's `filters.outlier`): their classification becomes
/// `options.class`. All the points are kept (as PDAL keeps them); when every point would be an
/// outlier, none is classified.
pub fn outlier(cloud: &PointCloud, options: &OutlierOptions) -> Result<PointCloud> {
    let mask = outlier_mask(cloud, options)?;
    let mut out = cloud.clone();
    if mask.iter().all(|o| *o) {
        return Ok(out);
    }
    set_classes(&mut out, |i| mask[i].then_some(options.class));
    Ok(out)
}

/// Sets the classification of points (adding the dimension when there is none).
fn set_classes(cloud: &mut PointCloud, class: impl Fn(usize) -> Option<u8>) {
    let n = cloud.len();
    if cloud.dim("classification").is_none() {
        cloud.dimensions.push(super::Dimension::new("classification", Values::U8(vec![0; n])));
    }
    if let Some(d) = cloud.dim_mut("classification") {
        for i in 0..n {
            if let Some(c) = class(i) {
                d.values.set_f64(i, c as f64);
            }
        }
    }
}

/// How to classify ground points (PDAL's `filters.smrf`, Pingel et al. 2013).
#[derive(Debug, Clone, PartialEq)]
pub struct SmrfOptions {
    /// The cell size. 1 by default.
    pub cell: f64,
    /// The slope (rise over run). 0.15 by default.
    pub slope: f64,
    /// The largest window, in CRS units. 18 cells by default.
    pub window: Option<f64>,
    /// The elevation scalar. 1.25 by default.
    pub scalar: f64,
    /// The elevation threshold. 0.5 by default.
    pub threshold: f64,
    /// The cut net size (0: no net cutting). 0 by default.
    pub cut: f64,
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

impl Default for SmrfOptions {
    fn default() -> Self {
        Self {
            cell: 1.0,
            slope: 0.15,
            window: None,
            scalar: 1.25,
            threshold: 0.5,
            cut: 0.0,
            ignore: Vec::new(),
            returns: vec![ReturnKind::Last, ReturnKind::Only],
            ground_class: 2,
            other_class: 1,
            only_ground: false,
        }
    }
}

/// Classifies ground points with the Simple Morphological Filter, as PDAL's `filters.smrf` does:
/// the returns selected (last and only, by default) become ground (2) or not (1); other points
/// keep their classification.
pub fn smrf(cloud: &PointCloud, options: &SmrfOptions) -> Result<PointCloud> {
    let classes = smrf_classes(cloud, options)?;
    let mut out = cloud.clone();
    set_classes(&mut out, |i| classes[i]);
    Ok(out)
}

/// The classification SMRF gives each point (`None`: left alone).
pub fn smrf_classes(cloud: &PointCloud, o: &SmrfOptions) -> Result<Vec<Option<u8>>> {
    let cell = o.cell;
    if !(cell > 0.0 && cell.is_finite()) {
        return Err(Error::InvalidArgument(format!("invalid cell {cell}")));
    }
    if o.ground_class == o.other_class && !o.only_ground {
        return Err(Error::InvalidArgument("the ground and other classes cannot be equal unless only ground is set".into()));
    }
    let window = o.window.unwrap_or(18.0 * cell);
    let (xs, ys, zs) = xyz(cloud)?;
    let n = cloud.len();
    // The points classified: those not ignored, of the returns asked for.
    let ignored = if o.ignore.is_empty() { vec![false; n] } else { range_mask(cloud, &o.ignore)? };
    let rn = cloud.dim("return_number");
    let nr = cloud.dim("number_of_returns");
    let mut inliers: Vec<usize> = (0..n).filter(|&i| !ignored[i]).collect();
    if let (Some(rn), Some(nr)) = (rn, nr) {
        let get = |d: &super::Dimension, i: usize| d.values.get_f64(i) as u8;
        let zero = inliers
            .iter()
            .map(|&i| (get(rn, i) == 0, get(nr, i) == 0))
            .fold((false, false, true, true), |a, (r0, n0)| (a.0 || n0, a.1 || r0, a.2 && n0, a.3 && r0));
        let (nr_one_zero, rn_one_zero, nr_all_zero, rn_all_zero) = zero;
        if (nr_one_zero || rn_one_zero) && !(nr_all_zero && rn_all_zero) {
            return Err(Error::InvalidData(
                "some number_of_returns or return_number values are 0, but not all: they should all be 1 or more".into(),
            ));
        }
        if !(nr_all_zero && rn_all_zero) && !o.returns.is_empty() {
            inliers.retain(|&i| {
                let (r, m) = (get(rn, i), get(nr, i));
                o.returns.iter().any(|k| match k {
                    ReturnKind::First => r == 1 && m > 1,
                    ReturnKind::Intermediate => r > 1 && r < m,
                    ReturnKind::Last => r == m && m > 1,
                    ReturnKind::Only => m == 1,
                })
            });
        }
    }
    if inliers.is_empty() {
        return Err(Error::InvalidData("no returns to classify".into()));
    }
    let mut out = vec![None; n];
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
        return Err(Error::InvalidArgument(format!("SMRF grid of {cols} x {rows} cells: use a larger cell")));
    }
    let g = Grid { rows, cols, minx, miny, cell };
    let cell_of = |i: usize| (((xs[i] - minx) / cell).floor() as usize) * rows + ((ys[i] - miny) / cell).floor() as usize;
    // The minimum surface, filled.
    let mut zimin = vec![f64::NAN; rows * cols];
    for &i in &inliers {
        let c = cell_of(i);
        if zs[i] < zimin[c] || zimin[c].is_nan() {
            zimin[c] = zs[i];
        }
    }
    g.knnfill(&mut zimin);
    // Low outliers: the inverted surface, opened once, with a 500% slope.
    let neg: Vec<f64> = zimin.iter().map(|v| -v).collect();
    let low = g.progressive_filter(&neg, 5.0, cell);
    // The net.
    let mut net = vec![false; rows * cols];
    let mut zinet = zimin.clone();
    if o.cut > 0.0 {
        let v = (o.cut / cell).ceil() as usize;
        for c in (0..cols).step_by(v.max(1)) {
            for r in 0..rows {
                net[c * rows + r] = true;
            }
        }
        for c in 0..cols {
            for r in (0..rows).step_by(v.max(1)) {
                net[c * rows + r] = true;
            }
        }
        let mut opened = zimin.clone();
        erode_diamond(&mut opened, rows, cols, 2 * v);
        dilate_diamond(&mut opened, rows, cols, 2 * v);
        for k in 0..rows * cols {
            if net[k] {
                zinet[k] = opened[k];
            }
        }
    }
    let obj = g.progressive_filter(&zinet, o.slope, window);
    // The provisional DEM: the minimum surface without the object, low and net cells, filled.
    let mut zipro = zimin.clone();
    for k in 0..rows * cols {
        if obj[k] || low[k] || net[k] {
            zipro[k] = f64::NAN;
        }
    }
    g.knnfill(&mut zipro);
    // The threshold: the elevation threshold plus the scalar times the DEM's slope.
    let scaled: Vec<f64> = zipro.iter().map(|v| v / cell).collect();
    let gx = grad(&scaled, rows, cols, true);
    let gy = grad(&scaled, rows, cols, false);
    let mut gsurfs: Vec<f64> = gx.iter().zip(&gy).map(|(a, b)| (a * a + b * b).sqrt()).collect();
    g.knnfill(&mut gsurfs);
    for &i in &inliers {
        let c = cell_of(i);
        if zipro[c].is_nan() || gsurfs[c].is_nan() {
            continue;
        }
        let thresh = o.threshold + o.scalar * gsurfs[c];
        if (zipro[c] - zs[i]).abs() > thresh {
            if !o.only_ground {
                out[i] = Some(o.other_class);
            }
        } else {
            out[i] = Some(o.ground_class);
        }
    }
    Ok(out)
}

/// A raster stored by columns, as PDAL's SMRF stores it: cell `(row, col)` at `col × rows + row`,
/// row 0 at the minimum y.
struct Grid {
    rows: usize,
    cols: usize,
    minx: f64,
    miny: f64,
    cell: f64,
}

impl Grid {
    /// Fills empty (NaN) cells with the mean of the 8 nearest non-empty cells' values (by the
    /// distance between cell centres).
    fn knnfill(&self, v: &mut [f64]) {
        let (rows, cols) = (self.rows, self.cols);
        let center = |c: usize, r: usize| [self.minx + (c as f64 + 0.5) * self.cell, self.miny + (r as f64 + 0.5) * self.cell];
        let mut pts = Vec::new();
        let mut vals = Vec::new();
        for c in 0..cols {
            for r in 0..rows {
                let x = v[c * rows + r];
                if !x.is_nan() {
                    pts.push(center(c, r));
                    vals.push(x);
                }
            }
        }
        if pts.is_empty() || pts.len() == v.len() {
            return;
        }
        // nanoflann's tree, as PDAL's: of cells as far as the 8th nearest, the same are kept.
        let tree = super::kdtree::NanoKdTree::new(pts, 100);
        let empty: Vec<usize> = (0..rows * cols).filter(|k| v[*k].is_nan()).collect();
        let filled: Vec<f64> = par_map(&empty, Vec::new, |found, &k| {
            tree.knn(&center(k / rows, k % rows), 8, found);
            let mut m = 0.0;
            for (j, (_, i)) in found.iter().enumerate() {
                m += (vals[*i] - m) / (j + 1) as f64;
            }
            m
        });
        for (k, m) in empty.into_iter().zip(filled) {
            v[k] = m;
        }
    }

    /// The cells that a progressive morphological filter flags as objects: the surface opened
    /// with growing windows (radius 1 to `max_window / cell` cells), flagging the cells that
    /// drop by more than `slope × cell × radius`.
    fn progressive_filter(&self, z: &[f64], slope: f64, max_window: f64) -> Vec<bool> {
        let (rows, cols) = (self.rows, self.cols);
        let max_radius = (max_window / self.cell).ceil() as usize;
        let mut prev = z.to_vec();
        let mut erosion = z.to_vec();
        let mut obj = vec![false; z.len()];
        for radius in 1..=max_radius {
            erode_diamond(&mut erosion, rows, cols, 1);
            let mut cur = erosion.clone();
            dilate_diamond(&mut cur, rows, cols, radius);
            let threshold = slope * self.cell * radius as f64;
            for k in 0..z.len() {
                if (prev[k] - cur[k]).abs() > threshold {
                    obj[k] = true;
                }
            }
            prev = cur;
        }
        obj
    }
}

/// Maps `f` over items, with a scratch value per worker: in parallel on native builds.
fn par_map<T: Sync, R: Send, S: Send>(
    items: &[T],
    init: impl Fn() -> S + Sync + Send,
    f: impl Fn(&mut S, &T) -> R + Sync + Send,
) -> Vec<R> {
    #[cfg(not(target_family = "wasm"))]
    {
        use rayon::prelude::*;
        items.par_iter().map_init(init, f).collect()
    }
    #[cfg(target_family = "wasm")]
    {
        let mut s = init();
        items.iter().map(|t| f(&mut s, t)).collect()
    }
}

/// Morphological dilation (maximum) or erosion with a diamond (cross) element, `iterations`
/// times, as PDAL's `dilateDiamond` and `erodeDiamond` (which do not reset their output between
/// iterations).
fn morph_diamond(data: &mut Vec<f64>, rows: usize, cols: usize, iterations: usize, dilate: bool) {
    let mut out = vec![if dilate { f64::MIN } else { f64::MAX }; data.len()];
    for _ in 0..iterations {
        for col in 0..cols {
            let index = col * rows;
            for row in 0..rows {
                let at = index + row;
                let mut idx = [at; 5];
                let mut j = 1;
                if row > 0 {
                    idx[j] = at - 1;
                    j += 1;
                }
                if row + 1 < rows {
                    idx[j] = at + 1;
                    j += 1;
                }
                if col > 0 {
                    idx[j] = at - rows;
                    j += 1;
                }
                if col + 1 < cols {
                    idx[j] = at + rows;
                    j += 1;
                }
                for &k in &idx[..j] {
                    if (dilate && data[k] > out[at]) || (!dilate && data[k] < out[at]) {
                        out[at] = data[k];
                    }
                }
            }
        }
        std::mem::swap(data, &mut out);
    }
}

fn dilate_diamond(data: &mut Vec<f64>, rows: usize, cols: usize, iterations: usize) {
    morph_diamond(data, rows, cols, iterations, true);
}

fn erode_diamond(data: &mut Vec<f64>, rows: usize, cols: usize, iterations: usize) {
    morph_diamond(data, rows, cols, iterations, false);
}

/// The gradient across columns (`x`) or rows, as MATLAB's `gradient`: central differences
/// inside, one-sided at the edges.
fn grad(a: &[f64], rows: usize, cols: usize, x: bool) -> Vec<f64> {
    let mut out = vec![0.0; a.len()];
    let at = |r: usize, c: usize| a[c * rows + r];
    let (n, other) = if x { (cols, rows) } else { (rows, cols) };
    if n < 2 {
        return out;
    }
    for o in 0..other {
        let get = |i: usize| if x { at(o, i) } else { at(i, o) };
        let k = |i: usize| if x { i * rows + o } else { o * rows + i };
        for i in 1..n - 1 {
            out[k(i)] = 0.5 * (get(i + 1) - get(i - 1));
        }
        out[k(0)] = get(1) - get(0);
        out[k(n - 1)] = get(n - 1) - get(n - 2);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges_as_pdal() {
        let r = DimRange::parse("Classification[2:2]").unwrap();
        assert_eq!((r.name.as_str(), r.lower, r.upper, r.negate), ("classification", 2.0, 2.0, false));
        let r = DimRange::parse("Z(:100]").unwrap();
        assert_eq!((r.lower, r.upper, r.lower_inclusive), (f64::MIN, 100.0, false));
        let r = DimRange::parse("Classification![7:7]").unwrap();
        assert!(r.negate && !r.passes(7.0) && r.passes(2.0));
        let l = DimRange::parse_list("Classification[2:2], Z[0:1e3)").unwrap();
        assert_eq!(l.len(), 2);
        assert!(l[1].passes(999.0) && !l[1].passes(1000.0));
        assert!(DimRange::parse("Z[1 2]").is_err());
    }
}

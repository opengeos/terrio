//! Filters that select, order, combine or change points: PDAL's `filters.crop`,
//! `filters.decimation`, `filters.expression`, `filters.assign`, `filters.merge`,
//! `filters.head`, `filters.tail`, `filters.sort`, `filters.locate`, `filters.iqr` and
//! `filters.mad`.

use super::DimRange;
use super::expr::{Assignment, Condition};
use crate::crs::{SpatialRef, Transformer};
use crate::pointcloud::{Dimension, PointCloud, Values};
use crate::{Error, Result};
use wbvector::Geometry;

/// A box to crop to, as PDAL writes them: `([xmin, xmax], [ymin, ymax])`, with
/// `[zmin, zmax]` for a 3D box. Bounds are included.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CropBox {
    /// The minimum x, y and z (z is ignored for a 2D box).
    pub min: [f64; 3],
    /// The maximum x, y and z.
    pub max: [f64; 3],
    /// Whether z is bounded.
    pub is_3d: bool,
}

impl CropBox {
    /// Parses PDAL's bounds: `([xmin, xmax], [ymin, ymax])` or
    /// `([xmin, xmax], [ymin, ymax], [zmin, zmax])`.
    pub fn parse(s: &str) -> Result<Self> {
        let err = || {
            Error::InvalidArgument(format!(
                "invalid bounds '{s}': use ([xmin, xmax], [ymin, ymax]) or ([xmin, xmax], [ymin, ymax], [zmin, zmax])"
            ))
        };
        let t = s.trim();
        let t = t.strip_prefix('(').and_then(|t| t.strip_suffix(')')).ok_or_else(err)?;
        let mut pairs = Vec::new();
        for part in t.split(']') {
            let part = part.trim().trim_start_matches(',').trim();
            if part.is_empty() {
                continue;
            }
            let inner = part.strip_prefix('[').ok_or_else(err)?;
            let v: Vec<f64> =
                inner.split(',').map(|x| x.trim().parse::<f64>()).collect::<std::result::Result<_, _>>().map_err(|_| err())?;
            if v.len() != 2 {
                return Err(err());
            }
            pairs.push((v[0], v[1]));
        }
        match pairs.as_slice() {
            [x, y] => Ok(Self { min: [x.0, y.0, f64::MIN], max: [x.1, y.1, f64::MAX], is_3d: false }),
            [x, y, z] => Ok(Self { min: [x.0, y.0, z.0], max: [x.1, y.1, z.1], is_3d: true }),
            _ => Err(err()),
        }
    }

    fn contains(&self, x: f64, y: f64, z: f64) -> bool {
        x >= self.min[0]
            && x <= self.max[0]
            && y >= self.min[1]
            && y <= self.max[1]
            && (!self.is_3d || (z >= self.min[2] && z <= self.max[2]))
    }
}

/// How to crop points (PDAL's `filters.crop`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CropOptions {
    /// Boxes to keep the points of.
    pub bounds: Vec<CropBox>,
    /// Polygons (or multipolygons) to keep the points of, as WKT or GeoJSON geometries.
    pub polygons: Vec<String>,
    /// Centres of circles (2D, `[x, y]`) or spheres (3D, `[x, y, z]`) of radius `distance`.
    pub points: Vec<Vec<f64>>,
    /// The radius of the circles and spheres.
    pub distance: f64,
    /// Keep the points outside the geometries instead.
    pub outside: bool,
    /// The CRS of the geometries, when it is not the points'.
    pub a_srs: Option<SpatialRef>,
}

/// A polygon: its exterior ring and holes.
pub(super) struct Poly {
    pub(super) exterior: Vec<[f64; 2]>,
    pub(super) holes: Vec<Vec<[f64; 2]>>,
}

/// Where a point is relative to a ring: on an edge, inside (by the even-odd rule) or outside.
/// The side of an edge is the sign of the cross product PDAL's `GridPnp` computes, in its order
/// of operations, so that points within rounding of an edge fall on the side PDAL puts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    On,
    Inside,
    Outside,
}

fn place(ring: &[[f64; 2]], x: f64, y: f64) -> Place {
    let n = ring.len();
    if n < 3 {
        return Place::Outside;
    }
    let mut inside = false;
    for i in 0..n {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        if a == b {
            continue;
        }
        let (s0, s1) = (b[0] - a[0], b[1] - a[1]);
        let (q0, q1) = (a[0] - x, a[1] - y);
        let cross = q0 * s1 - q1 * s0;
        if cross == 0.0 && x >= a[0].min(b[0]) && x <= a[0].max(b[0]) && y >= a[1].min(b[1]) && y <= a[1].max(b[1]) {
            return Place::On;
        }
        // The edge crosses the horizontal through the point, to its right.
        if (a[1] > y) != (b[1] > y) && (cross > 0.0) == (s1 > 0.0) {
            inside = !inside;
        }
    }
    if inside { Place::Inside } else { Place::Outside }
}

impl Poly {
    /// Whether a point is inside the polygon, or on its boundary (as PDAL counts them).
    pub(super) fn contains(&self, x: f64, y: f64) -> bool {
        match place(&self.exterior, x, y) {
            Place::On => true,
            Place::Outside => false,
            Place::Inside => self.holes.iter().all(|h| place(h, x, y) != Place::Inside),
        }
    }
}

/// The polygons of a WKT or GeoJSON geometry.
fn polygons(text: &str) -> Result<Vec<Poly>> {
    let t = text.trim();
    let geometry = if t.starts_with('{') {
        let v: serde_json::Value = serde_json::from_str(t).map_err(|e| Error::InvalidArgument(format!("invalid GeoJSON polygon: {e}")))?;
        let g = if v.get("geometry").is_some() { &v["geometry"] } else { &v };
        geojson_geometry(g)?
    } else {
        crate::drivers::vector::wkt::parse(t).ok_or_else(|| Error::InvalidArgument(format!("invalid WKT polygon '{t}'")))?
    };
    polys_of(&geometry).ok_or_else(|| Error::InvalidArgument(format!("'{t}' is not a polygon")))
}

/// The polygons of a polygon or multipolygon, `None` for other geometries.
pub(super) fn polys_of(geometry: &Geometry) -> Option<Vec<Poly>> {
    let ring = |r: &wbvector::Ring| r.coords().iter().map(|c| [c.x, c.y]).collect::<Vec<_>>();
    match geometry {
        Geometry::Polygon { exterior, interiors } => {
            Some(vec![Poly { exterior: ring(exterior), holes: interiors.iter().map(ring).collect() }])
        }
        Geometry::MultiPolygon(ps) => {
            Some(ps.iter().map(|(e, h)| Poly { exterior: ring(e), holes: h.iter().map(ring).collect() }).collect())
        }
        _ => None,
    }
}

/// A Polygon or MultiPolygon GeoJSON geometry.
fn geojson_geometry(g: &serde_json::Value) -> Result<Geometry> {
    let err = || Error::InvalidArgument("invalid GeoJSON polygon".into());
    let ring = |r: &serde_json::Value| -> Result<wbvector::Ring> {
        let coords = r
            .as_array()
            .ok_or_else(err)?
            .iter()
            .map(|p| {
                let p = p.as_array().filter(|p| p.len() >= 2).ok_or_else(err)?;
                Ok(wbvector::Coord::xy(p[0].as_f64().ok_or_else(err)?, p[1].as_f64().ok_or_else(err)?))
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(wbvector::Ring::new(coords))
    };
    let poly = |p: &serde_json::Value| -> Result<(wbvector::Ring, Vec<wbvector::Ring>)> {
        let rings = p.as_array().filter(|r| !r.is_empty()).ok_or_else(err)?;
        Ok((ring(&rings[0])?, rings[1..].iter().map(ring).collect::<Result<_>>()?))
    };
    match g["type"].as_str() {
        Some("Polygon") => {
            let (exterior, interiors) = poly(&g["coordinates"])?;
            Ok(Geometry::Polygon { exterior, interiors })
        }
        Some("MultiPolygon") => {
            Ok(Geometry::MultiPolygon(g["coordinates"].as_array().ok_or_else(err)?.iter().map(poly).collect::<Result<_>>()?))
        }
        _ => Err(err()),
    }
}

/// The indices of the points PDAL's `filters.crop` keeps, in the order it writes them: for each
/// polygon (each part of a multipolygon), then each box, then each circle or sphere, the points
/// inside it (or outside, with `outside`), so that a point in two geometries is kept twice.
pub fn crop_indices(cloud: &PointCloud, o: &CropOptions) -> Result<Vec<usize>> {
    let (xs, ys, zs) = super::xyz(cloud)?;
    let mut polys = Vec::new();
    for p in &o.polygons {
        polys.push(polygons(p)?);
    }
    let mut boxes = o.bounds.clone();
    let mut points = o.points.clone();
    for p in &points {
        if p.len() != 2 && p.len() != 3 {
            return Err(Error::InvalidArgument("crop points are [x, y] or [x, y, z]".into()));
        }
    }
    // The geometries in the points' CRS.
    if let Some(src) = &o.a_srs
        && !cloud.crs.is_unknown()
        && !src.same_as(&cloud.crs)
    {
        let t = Transformer::new(src, &cloud.crs)?;
        for poly in polys.iter_mut().flatten() {
            for ring in std::iter::once(&mut poly.exterior).chain(poly.holes.iter_mut()) {
                for c in ring.iter_mut() {
                    let (x, y) = t.transform(c[0], c[1])?;
                    *c = [x, y];
                }
            }
        }
        for b in boxes.iter_mut() {
            let (x0, y0, x1, y1) = t.transform_bounds((b.min[0], b.min[1], b.max[0], b.max[1]))?;
            b.min[0] = x0;
            b.min[1] = y0;
            b.max[0] = x1;
            b.max[1] = y1;
        }
        for p in points.iter_mut() {
            let (x, y) = t.transform(p[0], p[1])?;
            p[0] = x;
            p[1] = y;
        }
    }
    let n = cloud.len();
    let mut out = Vec::new();
    for poly in polys.iter().flatten() {
        out.extend((0..n).filter(|&i| o.outside != poly.contains(xs[i], ys[i])));
    }
    for b in &boxes {
        out.extend((0..n).filter(|&i| o.outside != b.contains(xs[i], ys[i], zs[i])));
    }
    let d2 = o.distance * o.distance;
    for c in &points {
        let keep = |i: usize| {
            let (dx, dy) = ((xs[i] - c[0]).abs(), (ys[i] - c[1]).abs());
            if dx > o.distance || dy > o.distance {
                return o.outside;
            }
            let inside = if c.len() == 3 {
                let dz = (zs[i] - c[2]).abs();
                if dz > o.distance {
                    return o.outside;
                }
                dx * dx + dy * dy + dz * dz < d2
            } else {
                dx * dx + dy * dy < d2
            };
            o.outside != inside
        };
        out.extend((0..n).filter(|&i| keep(i)));
    }
    Ok(out)
}

/// Crops points to boxes, polygons, circles or spheres, as PDAL's `filters.crop`.
pub fn crop(cloud: &PointCloud, options: &CropOptions) -> Result<PointCloud> {
    Ok(cloud.take(&crop_indices(cloud, options)?))
}

/// Rounds half away from zero, as C's `llround`.
fn llround(x: f64) -> u64 {
    if x < 0.0 { 0 } else { (x + 0.5).floor() as u64 }
}

/// Keeps every `step`-th point (a fraction is allowed) from `offset`, before `limit`, as PDAL's
/// `filters.decimation` keeps them when it streams points (as `pdal translate` and most pipelines
/// run it): the points at `offset + round(k × step)` for k = 0, 1, 2...
pub fn decimation(cloud: &PointCloud, step: f64, offset: usize, limit: Option<usize>) -> Result<PointCloud> {
    if !(step >= 1.0 && step.is_finite()) {
        return Err(Error::InvalidArgument("the step must be at least 1".into()));
    }
    let last = limit.unwrap_or(usize::MAX).min(cloud.len());
    let idx: Vec<usize> = (0u64..).map(|k| offset + llround(k as f64 * step) as usize).take_while(|&i| i < last).collect();
    Ok(cloud.take(&idx))
}

/// Keeps the points for which a condition holds, as PDAL's `filters.expression` (several
/// expressions: the points each keeps, one after the other).
pub fn expression(cloud: &PointCloud, expressions: &[String]) -> Result<PointCloud> {
    let mut idx = Vec::new();
    for e in expressions {
        let mask = Condition::parse(e)?.mask(cloud)?;
        idx.extend(mask.iter().enumerate().filter(|(_, k)| **k).map(|(i, _)| i));
    }
    Ok(cloud.take(&idx))
}

/// What PDAL's `filters.assign` assigns.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AssignOptions {
    /// Ranges with a value, `Classification[1:1]=2`: the dimension's values in the range take
    /// the value.
    pub assignments: Vec<String>,
    /// Only the points in this range are changed.
    pub condition: Option<DimRange>,
    /// Expressions, `Classification = 2 WHERE HeightAboveGround < 0.5`; a dimension assigned
    /// that the points do not have is added (as doubles).
    pub values: Vec<String>,
}

/// Sets a value, rounded (half away from zero) for integers, as PDAL converts doubles; fails when
/// it does not fit the dimension's type. Extra-bytes dimensions with a scale and offset store
/// `(v - offset) / scale`; coordinates hold the scaled value.
pub(super) fn set_value(d: &mut Dimension, i: usize, v: f64) -> Result<()> {
    let stored = (v - d.offset.unwrap_or(0.0)) / d.scale.unwrap_or(1.0);
    let (lo, hi) = match &d.values {
        Values::U8(_) => (0.0, u8::MAX as f64),
        Values::I8(_) => (i8::MIN as f64, i8::MAX as f64),
        Values::U16(_) => (0.0, u16::MAX as f64),
        Values::I16(_) => (i16::MIN as f64, i16::MAX as f64),
        Values::U32(_) => (0.0, u32::MAX as f64),
        Values::I32(_) => (i32::MIN as f64, i32::MAX as f64),
        Values::U64(_) => (0.0, u64::MAX as f64),
        Values::I64(_) => (i64::MIN as f64, i64::MAX as f64),
        Values::F32(_) | Values::F64(_) => {
            d.values.set_f64(i, stored);
            return Ok(());
        }
    };
    let r = if stored > 0.0 { (stored + 0.5).floor() } else { (stored - 0.5).ceil() };
    if !(r >= lo && r <= hi) {
        return Err(Error::InvalidArgument(format!("cannot set {} to {v}: out of range", super::super::names::to_pdal(&d.name))));
    }
    d.values.set_f64(i, r);
    Ok(())
}

/// Assigns values to dimensions, as PDAL's `filters.assign`.
pub fn assign(cloud: &PointCloud, o: &AssignOptions) -> Result<PointCloud> {
    let mut out = cloud.clone();
    let n = out.len();
    let cond: Vec<bool> = match &o.condition {
        Some(r) => super::range_mask(cloud, std::slice::from_ref(r))?,
        None => vec![true; n],
    };
    // Range assignments.
    for a in &o.assignments {
        let (range, value) = a
            .rsplit_once('=')
            .filter(|(r, _)| r.trim_end().ends_with([']', ')']))
            .ok_or_else(|| Error::InvalidArgument(format!("invalid assignment '{a}': use Dimension[lower:upper]=value")))?;
        let range = DimRange::parse(range)?;
        let value: f64 = value
            .trim()
            .parse()
            .map_err(|_| Error::InvalidArgument(format!("invalid assignment '{a}': missing value to assign following '='")))?;
        let d = out
            .dim_mut(&range.name)
            .ok_or_else(|| Error::InvalidArgument(format!("invalid dimension name in 'assignment' option: '{}'", range.name)))?;
        for (i, c) in cond.iter().enumerate() {
            if *c && range.passes(d.get(i)) {
                set_value(d, i, value)?;
            }
        }
    }
    // Expressions, evaluated point by point after the ranges.
    for v in &o.values {
        let a = Assignment::parse(v)?;
        let target = if out.dim(&a.target).is_some() {
            a.target.clone()
        } else {
            let name = a.new_dimension_name();
            if out.dim(&name).is_none() {
                out.dimensions.push(Dimension::new(&name, Values::F64(vec![0.0; n])));
            }
            name
        };
        let values = a.values(&out)?;
        let d = out.dim_mut(&target).expect("added above");
        for (i, v) in values.into_iter().enumerate() {
            if cond[i]
                && let Some(v) = v
            {
                set_value(d, i, v)?;
            }
        }
    }
    Ok(out)
}

/// Merges point clouds into one, as PDAL's `filters.merge`: the points of each in turn, with
/// the dimensions of all (zeros where a cloud has none; values of a dimension whose type differs
/// between clouds kept as doubles). The header and CRS are the first cloud's.
pub fn merge(clouds: &[PointCloud]) -> Result<PointCloud> {
    let Some(first) = clouds.first() else { return Err(Error::InvalidArgument("nothing to merge".into())) };
    let mut names: Vec<(String, crate::pointcloud::ValueType)> = Vec::new();
    for c in clouds {
        for d in &c.dimensions {
            match names.iter_mut().find(|(n, _)| *n == d.name) {
                Some((_, t)) if *t != d.values.value_type() => *t = crate::pointcloud::ValueType::F64,
                Some(_) => {}
                None => names.push((d.name.clone(), d.values.value_type())),
            }
        }
    }
    let total: usize = clouds.iter().map(PointCloud::len).sum();
    let mut dims = Vec::with_capacity(names.len());
    for (name, t) in &names {
        let mut v = Values::with_capacity(*t, total);
        let mut template: Option<&Dimension> = None;
        for c in clouds {
            match c.dim(name) {
                Some(d) => {
                    template.get_or_insert(d);
                    v.extend_from(&d.values.cast(*t));
                }
                None => v.extend_from(&Values::zeros(*t, c.len())),
            }
        }
        let d = template.expect("a cloud has the dimension");
        dims.push(d.with_values(v));
    }
    Ok(PointCloud { header: first.header.clone(), crs: first.crs.clone(), dimensions: dims })
}

/// The first `count` points, or all but them with `invert`, as PDAL's `filters.head` (whose
/// `count` is 10 by default).
pub fn head(cloud: &PointCloud, count: usize, invert: bool) -> PointCloud {
    let k = count.min(cloud.len());
    if invert { cloud.slice(k, cloud.len()) } else { cloud.slice(0, k) }
}

/// The last `count` points, or all but them with `invert`, as PDAL's `filters.tail` (whose
/// `count` is 10 by default).
pub fn tail(cloud: &PointCloud, count: usize, invert: bool) -> PointCloud {
    let k = cloud.len() - count.min(cloud.len());
    if invert { cloud.slice(0, k) } else { cloud.slice(k, cloud.len()) }
}

/// Sorts the points by dimensions, as PDAL's `filters.sort`: by the first, then (stably) by
/// each of the others in turn, so that the last dimension orders the points and the earlier
/// ones break its ties; ascending, or descending with `descending`. Points of equal values keep
/// their order (PDAL's default `NORMAL` algorithm leaves the order of ties to `std::sort`).
pub fn sort(cloud: &PointCloud, dimensions: &[String], descending: bool) -> Result<PointCloud> {
    if dimensions.is_empty() {
        return Err(Error::InvalidArgument("sort: give at least one dimension".into()));
    }
    let mut order: Vec<usize> = (0..cloud.len()).collect();
    for name in dimensions {
        let v = super::find_dim(cloud, name)?.to_f64();
        if descending {
            order.sort_by(|&a, &b| v[b].total_cmp(&v[a]));
        } else {
            order.sort_by(|&a, &b| v[a].total_cmp(&v[b]));
        }
    }
    Ok(cloud.take(&order))
}

/// The point of the largest value of a dimension (the first, when several are as large), or
/// of the smallest with `min`, as PDAL's `filters.locate`.
pub fn locate(cloud: &PointCloud, dimension: &str, min: bool) -> Result<PointCloud> {
    let d = super::find_dim(cloud, dimension)?;
    if cloud.is_empty() {
        return Ok(cloud.clone());
    }
    let (mut lo, mut hi) = (f64::MAX, f64::MIN);
    let (mut lo_at, mut hi_at) = (0, 0);
    for i in 0..cloud.len() {
        let v = d.get(i);
        if v > hi {
            hi = v;
            hi_at = i;
        }
        if v < lo {
            lo = v;
            lo_at = i;
        }
    }
    Ok(cloud.take(&[if min { lo_at } else { hi_at }]))
}

/// The value at a fraction of the sorted values, as PDAL takes it: the value at
/// `int(n × fraction)`.
fn order_statistic(mut v: Vec<f64>, fraction: f64) -> f64 {
    let at = ((v.len() as f64 * fraction) as usize).min(v.len().saturating_sub(1));
    v.select_nth_unstable_by(at, f64::total_cmp);
    v[at]
}

/// The points whose values of a dimension are inside the interquartile fences, as PDAL's
/// `filters.iqr`: strictly between `Q1 − k × IQR` and `Q3 + k × IQR` (PDAL's `k` is 1.5 by
/// default), the quartiles the values at `int(n / 4)` and `int(3n / 4)` in order.
pub fn iqr(cloud: &PointCloud, dimension: &str, k: f64) -> Result<PointCloud> {
    let v = super::find_dim(cloud, dimension)?.to_f64();
    if v.is_empty() {
        return Ok(cloud.clone());
    }
    let q1 = order_statistic(v.clone(), 0.25);
    let q3 = order_statistic(v.clone(), 0.75);
    let (low, high) = (q1 - k * (q3 - q1), q3 + k * (q3 - q1));
    Ok(cloud.select(&v.iter().map(|x| *x > low && *x < high).collect::<Vec<_>>()))
}

/// The points whose values of a dimension are within `k` median absolute deviations of the
/// median, as PDAL's `filters.mad`: `|v − median| / (MAD × mad_multiplier) < k` (PDAL's `k` is
/// 2 and `mad_multiplier` 1.4862 by default), the medians the values at `n / 2` in order.
pub fn mad(cloud: &PointCloud, dimension: &str, k: f64, mad_multiplier: f64) -> Result<PointCloud> {
    let v = super::find_dim(cloud, dimension)?.to_f64();
    if v.is_empty() {
        return Ok(cloud.clone());
    }
    let median = order_statistic(v.clone(), 0.5);
    let dev: Vec<f64> = v.iter().map(|x| (x - median).abs()).collect();
    let mad = order_statistic(dev.clone(), 0.5) * mad_multiplier;
    Ok(cloud.select(&dev.iter().map(|d| d / mad < k).collect::<Vec<_>>()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pdal_bounds() {
        let b = CropBox::parse("([0, 10], [5, 15])").unwrap();
        assert!(!b.is_3d && b.contains(10.0, 5.0, 1e9) && !b.contains(10.1, 5.0, 0.0));
        let b = CropBox::parse("([0,10],[5,15],[-1, 1])").unwrap();
        assert!(b.is_3d && !b.contains(1.0, 6.0, 2.0));
        assert!(CropBox::parse("[0,10],[5,15]").is_err());
    }

    #[test]
    fn rounds_as_llround() {
        assert_eq!(llround(2.5), 3);
        assert_eq!(llround(2.4999), 2);
    }
}

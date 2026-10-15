//! How a transformation moves points between two datums, as PROJ (and so GDAL) does: from the
//! transformations GDAL lists for the pair, or by chaining two transformations through an
//! intermediate datum, picking for each point the most accurate whose area holds it.

use super::datum::{self, Op, Point};
use super::def::Def;
use super::epsg;
use std::sync::{Arc, OnceLock};

/// A transformation and the direction it runs in.
#[derive(Debug, Clone)]
pub(crate) struct Leg {
    pub op: Arc<Op>,
    pub reverse: bool,
}

/// Two transformations through an intermediate datum, with the area, accuracy and flags PROJ
/// ranks the chain by.
#[derive(Debug)]
struct Chain {
    legs: [Leg; 2],
    rank: Op,
}

#[derive(Debug)]
enum Kind {
    /// Same datum: no shift.
    Same,
    /// GDAL's list of transformations for the pair: the best for a point, else the first.
    List(Vec<Leg>),
    /// Chains through intermediate datums: the best for a point, else no shift (PROJ's
    /// ballpark).
    Chains(Vec<Chain>),
    /// CRSs without EPSG geographic CRS: through WGS 84 with each side's transformations.
    Hub,
}

/// Areas in source coordinates, per transformation (two boxes across the antimeridian).
type Boxes = Vec<Vec<[f64; 4]>>;

/// The datum part of a transformation between two CRSs.
#[derive(Debug)]
pub(crate) struct Route {
    kind: Kind,
    /// The areas of the transformations in source coordinates, computed when first needed: per
    /// leg of `List`, or the two legs of each `Chain` in turn.
    boxes: OnceLock<Boxes>,
    /// The same, for the source's and target's transformations of `Hub`.
    hub_boxes: OnceLock<(Boxes, Boxes)>,
    /// The target's transformations to WGS 84, for `Hub`.
    dst_ops: Vec<Arc<Op>>,
}

fn legs(ops: &[Arc<Op>], reverse: bool) -> Vec<Leg> {
    ops.iter().map(|op| Leg { op: op.clone(), reverse }).collect()
}

/// Whether two areas intersect, as PROJ's `GeographicBoundingBox::intersects`.
fn intersects(a: &[f64; 4], b: &[f64; 4]) -> bool {
    let [w, s, e, n] = *a;
    let [ow, os, oe, on] = *b;
    if n < os || s > on {
        return false;
    }
    if w == -180.0 && e == 180.0 && ow > oe {
        return true;
    }
    if ow == -180.0 && oe == 180.0 && w > e {
        return true;
    }
    if w <= e {
        if ow <= oe {
            return w.max(ow) < e.min(oe);
        }
        if ow > 180.0 || oe < -180.0 {
            return false;
        }
        return intersects(a, &[ow, os, 180.0, on]) || intersects(a, &[-180.0, os, oe, on]);
    }
    if ow <= oe {
        return intersects(b, a);
    }
    true
}

/// PROJ's measure of the size of an area.
fn pseudo_area(a: &[f64; 4]) -> f64 {
    let [w, s, mut e, n] = *a;
    if w > e {
        e += 360.0;
    }
    (e - w) * (n.to_radians().sin() - s.to_radians().sin())
}

/// The area the transformations between two CRSs must intersect: the smaller of their areas of
/// use, as PROJ's `SourceTargetCRSExtentUse::SMALLEST`.
fn area_of_interest(src: &Def, dst: &Def) -> Option<[f64; 4]> {
    match (src.area(), dst.area()) {
        (Some(a), Some(b)) => Some(if pseudo_area(&a) < pseudo_area(&b) { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// The intersection of two areas, `None` when they do not overlap or one crosses the
/// antimeridian (then the chain is ranked by the other's area).
fn intersect(a: &[f64; 4], b: &[f64; 4]) -> Option<[f64; 4]> {
    if a[0] > a[2] || b[0] > b[2] {
        return Some(if a[0] > a[2] { *b } else { *a });
    }
    let r = [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])];
    (r[0] <= r[2] && r[1] <= r[3]).then_some(r)
}

impl Route {
    /// The route from `src` to `dst`.
    pub fn new(src: &Def, dst: &Def) -> Self {
        let aoi = area_of_interest(src, dst);
        let keep = |b: &[f64; 4]| aoi.is_none_or(|a| intersects(b, &a));
        let list = |ops: &[Arc<Op>], reverse: bool| -> Kind {
            Kind::List(legs(ops, reverse).into_iter().filter(|l| keep(&l.op.bounds)).collect())
        };
        let kind = if src.same_datum(dst) {
            Kind::Same
        } else if dst.is_wgs84() {
            list(src.ops(), false)
        } else if src.is_wgs84() {
            list(dst.ops(), true)
        } else {
            match (src.geog(), dst.geog()) {
                (Some(a), Some(b)) => match epsg::pair(a, b) {
                    Some((ops, reverse)) => list(&ops, reverse),
                    None => Kind::Chains(chains(a, b).into_iter().filter(|c| keep(&c.rank.bounds)).collect()),
                },
                _ => Kind::Hub,
            }
        };
        Self { kind, boxes: OnceLock::new(), hub_boxes: OnceLock::new(), dst_ops: dst.ops().to_vec() }
    }

    /// Moves a geodetic point (radians) from the source datum to the target's. `(x, y)` are its
    /// source coordinates, which pick the transformation for a projected source as in PROJ.
    /// Heights go through the transformation when `three_d` (between 3D CRSs). `None` when no
    /// transformation succeeds.
    pub fn apply(&self, src: &Def, x: f64, y: f64, p: Point, three_d: bool) -> Option<Point> {
        // PROJ compares the source coordinates with the areas: for a geographic source, its
        // longitude and latitude as given, in degrees.
        let (dlon, dlat) = src.source_degrees(x, y).unwrap_or((p.lon.to_degrees(), p.lat.to_degrees()));
        // Longitudes out of [-180, 180] are also tried wrapped into it, as PROJ does.
        let wrapped = if (-180.0..=180.0).contains(&dlon) { dlon } else { (dlon + 180.0).rem_euclid(360.0) - 180.0 };
        let inside = |op: &Op, boxes: Option<&Vec<[f64; 4]>>| match boxes {
            Some(b) if !b.is_empty() => b.iter().any(|b| x >= b[0] && x <= b[2] && y >= b[1] && y <= b[3]),
            _ => op.contains(dlon, dlat) || op.contains(wrapped, dlat),
        };
        match &self.kind {
            Kind::Same => Some(p),
            Kind::List(list) => {
                let boxes = self.boxes.get_or_init(|| list.iter().map(|l| src.areas(&l.op)).collect());
                let ops: Vec<&Op> = list.iter().map(|l| l.op.as_ref()).collect();
                datum::transform_with(&ops, |op, i| inside(op, boxes.get(i)), |i| datum::run_op(&list[i].op, p, list[i].reverse, three_d))
            }
            Kind::Chains(chains) => {
                let boxes = self.boxes.get_or_init(|| chains.iter().flat_map(|c| c.legs.iter().map(|l| src.areas(&l.op))).collect());
                let ranks: Vec<&Op> = chains.iter().map(|c| &c.rank).collect();
                let contains = |_: &Op, i: usize| {
                    let c = &chains[i];
                    inside(&c.legs[0].op, boxes.get(2 * i)) && inside(&c.legs[1].op, boxes.get(2 * i + 1))
                };
                let mut excluded = Vec::new();
                for _ in 0..3 {
                    let Some(i) = datum::pick_index(&ranks, &excluded, contains) else { break };
                    let c = &chains[i];
                    let r = datum::run_op(&c.legs[0].op, p, c.legs[0].reverse, three_d)
                        .and_then(|q| datum::run_op(&c.legs[1].op, q, c.legs[1].reverse, three_d));
                    if r.is_some() {
                        return r;
                    }
                    excluded.push(i);
                }
                // No transformation for the point: no shift, as PROJ's ballpark.
                Some(p)
            }
            Kind::Hub => {
                let (sb, db) = self.hub_boxes.get_or_init(|| {
                    let areas = |ops: &[Arc<Op>]| ops.iter().map(|o| src.areas(o)).collect();
                    (areas(src_ops(src)), areas(&self.dst_ops))
                });
                // Single transformations only, as PROJ chains them.
                let single = |op: &Op| !op.ballpark && !op.concatenated;
                let s = datum::pick_in(src_ops(src), |op, i| single(op) && inside(op, sb.get(i)));
                let d = datum::pick_in(&self.dst_ops, |op, i| single(op) && inside(op, db.get(i)));
                match (s, d) {
                    (Some(s), Some(d)) => {
                        let q = datum::run_op(s, p, false, three_d)?;
                        datum::run_op(d, q, true, three_d)
                    }
                    // No transformation (or an unknown datum): no shift, as PROJ's ballpark.
                    _ => Some(p),
                }
            }
        }
    }
}

fn src_ops(src: &Def) -> &[Arc<Op>] {
    src.ops()
}

/// The chains of two single transformations from geographic CRS `a` to `b` through the
/// datums both have transformations to.
fn chains(a: u32, b: u32) -> Vec<Chain> {
    let adj = epsg::adjacency();
    let (Some(from_a), Some(from_b)) = (adj.get(&a), adj.get(&b)) else { return Vec::new() };
    let mut out = Vec::new();
    for (pivot, first) in from_a {
        let Some((_, second)) = from_b.iter().find(|(p, _)| p == pivot) else { continue };
        for l1 in first {
            for l2 in second {
                let (o1, o2) = (&l1.op, &l2.op);
                let Some(bounds) = intersect(&o1.bounds, &o2.bounds) else { continue };
                let accuracy = if o1.accuracy >= 0.0 && o2.accuracy >= 0.0 { o1.accuracy + o2.accuracy } else { -1.0 };
                let rank = Op {
                    bounds,
                    accuracy,
                    offshore: o1.offshore || o2.offshore,
                    unknown_area: o1.unknown_area || o2.unknown_area,
                    ballpark: false,
                    concatenated: true,
                    priority: o1.priority || o2.priority,
                    keeps_height: o1.keeps_height && o2.keeps_height,
                    steps: o1.steps.iter().chain(&o2.steps).cloned().collect(),
                };
                // The second leg runs from the pivot to `b`: the reverse of `b` to the pivot.
                let back = Leg { op: o2.clone(), reverse: !l2.reverse };
                out.push(Chain { legs: [l1.clone(), back], rank });
            }
        }
    }
    // PROJ lists concatenations by accuracy, those of unknown accuracy last and by decreasing
    // area.
    out.sort_by(|x, y| {
        let key = |c: &Chain| if c.rank.accuracy < 0.0 { (f64::INFINITY, -c.rank.pseudo_area()) } else { (c.rank.accuracy, 0.0) };
        let (kx, ky) = (key(x), key(y));
        kx.0.total_cmp(&ky.0).then(kx.1.total_cmp(&ky.1))
    });
    out
}

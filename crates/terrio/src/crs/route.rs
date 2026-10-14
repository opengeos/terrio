//! How a transformation moves points between two datums, as PROJ (and so GDAL) does: from the
//! transformations GDAL lists for the pair, or by chaining two transformations through an
//! intermediate datum, picking for each point the most accurate whose area holds it.

use super::datum::{self, Op};
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
}

fn legs(ops: &[Arc<Op>], reverse: bool) -> Vec<Leg> {
    ops.iter().map(|op| Leg { op: op.clone(), reverse }).collect()
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
        let kind = if src.same_datum(dst) {
            Kind::Same
        } else if dst.is_wgs84() {
            Kind::List(legs(src.ops(), false))
        } else if src.is_wgs84() {
            Kind::List(legs(dst.ops(), true))
        } else {
            match (src.geog(), dst.geog()) {
                (Some(a), Some(b)) => match epsg::pair(a, b) {
                    Some((ops, reverse)) => Kind::List(legs(ops, reverse)),
                    None => Kind::Chains(chains(a, b)),
                },
                _ => Kind::Hub,
            }
        };
        Self { kind, boxes: OnceLock::new(), hub_boxes: OnceLock::new() }
    }

    /// Moves a geodetic point (radians) from the source datum to the target's. `(x, y)` are its
    /// source coordinates, which pick the transformation for a projected source as in PROJ.
    pub fn apply(&self, src: &Def, dst: &Def, x: f64, y: f64, lon: f64, lat: f64) -> (f64, f64) {
        let (dlon, dlat) = (lon.to_degrees(), lat.to_degrees());
        let inside = |op: &Op, boxes: Option<&Vec<[f64; 4]>>| match boxes {
            Some(b) if !b.is_empty() => b.iter().any(|b| x >= b[0] && x <= b[2] && y >= b[1] && y <= b[3]),
            _ => op.contains(dlon, dlat),
        };
        match &self.kind {
            Kind::Same => (lon, lat),
            Kind::List(list) => {
                let boxes = self.boxes.get_or_init(|| list.iter().map(|l| src.areas(&l.op)).collect());
                let ops: Vec<&Op> = list.iter().map(|l| l.op.as_ref()).collect();
                let chosen = datum::pick_by(&ops, |op, i| inside(op, boxes.get(i)));
                match chosen.and_then(|op| list.iter().find(|l| std::ptr::eq(l.op.as_ref(), op))) {
                    Some(l) => datum::run(&l.op.steps, lon, lat, l.reverse),
                    None => (lon, lat),
                }
            }
            Kind::Chains(chains) => {
                let boxes = self.boxes.get_or_init(|| chains.iter().flat_map(|c| c.legs.iter().map(|l| src.areas(&l.op))).collect());
                let ranks: Vec<&Op> = chains.iter().map(|c| &c.rank).collect();
                let chosen = datum::pick_in(&ranks, |_, i| {
                    let c = &chains[i];
                    inside(&c.legs[0].op, boxes.get(2 * i)) && inside(&c.legs[1].op, boxes.get(2 * i + 1))
                });
                let Some(rank) = chosen else { return (lon, lat) };
                let c = chains.iter().find(|c| std::ptr::eq(&c.rank, rank)).expect("chain of its rank");
                let (lon, lat) = datum::run(&c.legs[0].op.steps, lon, lat, c.legs[0].reverse);
                datum::run(&c.legs[1].op.steps, lon, lat, c.legs[1].reverse)
            }
            Kind::Hub => {
                let (sb, db) = self.hub_boxes.get_or_init(|| {
                    let areas = |ops: &[Arc<Op>]| ops.iter().map(|o| src.areas(o)).collect();
                    (areas(src.ops()), areas(dst.ops()))
                });
                // Single transformations only, as PROJ chains them.
                let single = |op: &Op| !op.ballpark && !op.concatenated;
                let s = datum::pick_in(src.ops(), |op, i| single(op) && inside(op, sb.get(i)));
                let d = datum::pick_in(dst.ops(), |op, i| single(op) && inside(op, db.get(i)));
                match (s, d) {
                    (Some(s), Some(d)) => {
                        let (lon, lat) = datum::run(&s.steps, lon, lat, false);
                        datum::run(&d.steps, lon, lat, true)
                    }
                    // No transformation (or an unknown datum): no shift, as PROJ's ballpark.
                    _ => (lon, lat),
                }
            }
        }
    }
}

/// The chains of two single transformations from geographic CRS `a` to `b` through the
/// datums both have transformations to.
fn chains(a: u32, b: u32) -> Vec<Chain> {
    let (Some(from_a), Some(from_b)) = (epsg::neighbours(a), epsg::neighbours(b)) else { return Vec::new() };
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
                    steps: Vec::new(),
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

//! How a transformation moves points between two datums, as PROJ (and so GDAL) does: from the
//! transformations GDAL lists for the pair, or by chaining two transformations through an
//! intermediate datum as PROJ lists them, picking for each point the most accurate whose area
//! holds it, else the first that needs no grid (PROJ's `proj_trans`); a list of one is used for
//! every point.

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
    /// Chains through intermediate datums, in PROJ's order: the best for a point, else no shift
    /// when `ballpark` (PROJ's list then ends with its ballpark, which holds every point), else
    /// the first chain that needs no grid.
    Chains { chains: Vec<Chain>, ballpark: bool },
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
    /// leg of `List`, or per `Chain`.
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

/// Whether area `a` holds area `b`, as PROJ's `GeographicBoundingBox::contains`.
fn contains(a: &[f64; 4], b: &[f64; 4]) -> bool {
    let [w, s, e, n] = *a;
    let [ow, os, oe, on] = *b;
    if !(s <= os && n >= on) {
        return false;
    }
    if w == -180.0 && e == 180.0 {
        return ow != oe;
    }
    if ow == -180.0 && oe == 180.0 {
        return false;
    }
    if w < e {
        return ow < oe && w <= ow && e >= oe;
    }
    if ow < oe { ow >= w || oe <= e } else { w <= ow && e >= oe }
}

/// The intersection of two areas, as PROJ's `GeographicBoundingBox::intersection`: `None` when
/// they do not overlap; the larger part when it has two across the antimeridian.
fn intersection(a: &[f64; 4], b: &[f64; 4]) -> Option<[f64; 4]> {
    let [w, s, e, n] = *a;
    let [ow, os, oe, on] = *b;
    if n < os || s > on {
        return None;
    }
    let (s2, n2) = (s.max(os), n.min(on));
    if w == -180.0 && e == 180.0 && ow > oe {
        return Some([ow, s2, oe, n2]);
    }
    if ow == -180.0 && oe == 180.0 && w > e {
        return Some([w, s2, e, n2]);
    }
    if w <= e {
        if ow <= oe {
            let (rw, re) = (w.max(ow), e.min(oe));
            return (rw < re).then_some([rw, s2, re, n2]);
        }
        if ow > 180.0 || oe < -180.0 {
            return None;
        }
        return match (intersection(a, &[ow, os, 180.0, on]), intersection(a, &[-180.0, os, oe, on])) {
            (Some(x), Some(y)) => Some(if x[2] - x[0] > y[2] - y[0] { x } else { y }),
            (x, y) => x.or(y),
        };
    }
    if ow <= oe {
        return intersection(b, a);
    }
    Some([w.max(ow), s2, e.min(oe), n2])
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

/// Whether PROJ considers a transformation between two CRSs: its area must intersect both CRSs'
/// areas (as PROJ queries its database) and the area of interest.
fn relevant(op: &Op, src: &Def, dst: &Def, aoi: Option<&[f64; 4]>) -> bool {
    op.ballpark || [src.area().as_ref(), dst.area().as_ref(), aoi].into_iter().flatten().all(|a| intersects(&op.bounds, a))
}

impl Route {
    /// The route from `src` to `dst`.
    pub fn new(src: &Def, dst: &Def) -> Self {
        let aoi = area_of_interest(src, dst);
        let keep = |op: &Op| relevant(op, src, dst, aoi.as_ref());
        let list =
            |ops: &[Arc<Op>], reverse: bool| -> Kind { Kind::List(legs(ops, reverse).into_iter().filter(|l| keep(&l.op)).collect()) };
        let kind = if src.same_datum(dst) {
            Kind::Same
        } else if dst.is_wgs84() {
            list(src.ops(), false)
        } else if src.is_wgs84() {
            list(dst.ops(), true)
        } else {
            match (src.geog(), dst.geog()) {
                (Some(a), Some(b)) => match epsg::pair(a, b) {
                    // PROJ looks for transformations through other datums when its database has
                    // none it can use between the two for the CRSs' areas (NAD83(CSRS) to NAD83
                    // between Arctic CRSs, outside the offshore area of their only one, or with
                    // the grids missing).
                    Some((ops, reverse)) if ops.iter().any(|o| !o.ballpark && keep(o)) => list(&ops, reverse),
                    _ => chains(a, b, src, dst, aoi.as_ref()),
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
            // No transformation at all (a CRS of an unknown datum, defined by its ellipsoid alone):
            // no shift, PROJ's ballpark transformation.
            Kind::List(list) if list.is_empty() => Some(p),
            Kind::List(list) => {
                let boxes = self.boxes.get_or_init(|| list.iter().map(|l| src.areas(&l.op)).collect());
                let ops: Vec<&Op> = list.iter().map(|l| l.op.as_ref()).collect();
                datum::transform_with(&ops, |op, i| inside(op, boxes.get(i)), |i| datum::run_op(&list[i].op, p, list[i].reverse, three_d))
            }
            // PROJ's ballpark alone: no shift.
            Kind::Chains { chains, .. } if chains.is_empty() => Some(p),
            Kind::Chains { chains, ballpark } => {
                let run = |c: &Chain| {
                    datum::run_op(&c.legs[0].op, p, c.legs[0].reverse, three_d)
                        .and_then(|q| datum::run_op(&c.legs[1].op, q, c.legs[1].reverse, three_d))
                };
                // A single operation is used for every point, whatever its area, as PROJ does.
                if chains.len() == 1 && !ballpark {
                    return run(&chains[0]);
                }
                let boxes = self.boxes.get_or_init(|| chains.iter().map(|c| src.areas(&c.rank)).collect());
                let ranks: Vec<&Op> = chains.iter().map(|c| &c.rank).collect();
                let mut excluded = Vec::new();
                for _ in 0..3 {
                    let Some(i) = datum::pick_index(&ranks, &excluded, |op, i| inside(op, boxes.get(i))) else { break };
                    if let Some(r) = run(&chains[i]) {
                        return Some(r);
                    }
                    excluded.push(i);
                }
                if *ballpark {
                    // The ballpark holds every point: no shift.
                    return Some(p);
                }
                // No transformation for the point: the first that needs no grid, as PROJ's
                // `proj_trans`.
                let first = chains.iter().find(|c| !c.rank.needs_grids())?;
                run(first)
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
/// datums both have transformations to, as PROJ lists them for CRSs `src` and `dst`: those
/// whose transformations' areas intersect both CRSs' and each other, and whose common area
/// intersects the area of interest, sorted as PROJ sorts them. PROJ ends the list with its
/// ballpark (no shift) unless a chain that needs no grid holds the area of interest.
fn chains(a: u32, b: u32, src: &Def, dst: &Def, aoi: Option<&[f64; 4]>) -> Kind {
    let adj = epsg::adjacency();
    let (Some(from_a), Some(from_b)) = (adj.get(&a), adj.get(&b)) else { return Kind::Chains { chains: Vec::new(), ballpark: true } };
    // Between versions of NAD83(CSRS), PROJ does not chain transformations its database
    // registers between geographic CRSs through NAD83 or WGS 84.
    let name = |code: u32| epsg::get(code).map_or("", |e| e.name.as_str());
    let csrs = name(a).starts_with("NAD83(CSRS)") && name(b).starts_with("NAD83(CSRS)");
    let mut out: Vec<Chain> = Vec::new();
    for (pivot, first) in from_a {
        let Some((_, second)) = from_b.iter().find(|(p, _)| p == pivot) else { continue };
        let excluded = csrs && matches!(name(*pivot), "NAD83" | "WGS 84");
        for l1 in first {
            for l2 in second {
                let (o1, o2) = (&l1.op, &l2.op);
                // PROJ chains transformations whose areas intersect both CRSs', and not one
                // registered between geographic 3D CRSs with one between 2D ones.
                if o1.between_3d != o2.between_3d
                    || (excluded && !o1.geocentric && !o2.geocentric)
                    || !relevant(o1, src, dst, None)
                    || !relevant(o2, src, dst, None)
                {
                    continue;
                }
                // The common area, with the name (and so the flags) of the leg whose area it is.
                let Some(bounds) = intersection(&o1.bounds, &o2.bounds) else { continue };
                if aoi.is_some_and(|a| !intersects(&bounds, a)) {
                    continue;
                }
                let named = if contains(&o1.bounds, &o2.bounds) {
                    Some(o2)
                } else if contains(&o2.bounds, &o1.bounds) {
                    Some(o1)
                } else {
                    None
                };
                let accuracy = if o1.accuracy >= 0.0 && o2.accuracy >= 0.0 { o1.accuracy + o2.accuracy } else { -1.0 };
                let rank = Op {
                    bounds,
                    accuracy,
                    offshore: named.is_some_and(|o| o.offshore),
                    unknown_area: named.is_some_and(|o| o.unknown_area),
                    ballpark: false,
                    concatenated: true,
                    between_3d: o1.between_3d,
                    geocentric: o1.geocentric || o2.geocentric,
                    priority: o1.priority || o2.priority,
                    keeps_height: o1.keeps_height && o2.keeps_height,
                    steps: o1.steps.iter().chain(&o2.steps).cloned().collect(),
                };
                // The second leg runs from the pivot to `b`: the reverse of `b` to the pivot.
                let back = Leg { op: o2.clone(), reverse: !l2.reverse };
                // PROJ keeps one of the chains with the same steps and area.
                let same = |c: &Chain| {
                    c.rank.bounds == rank.bounds
                        && c.legs[0].reverse == l1.reverse
                        && c.legs[1].reverse == back.reverse
                        && c.legs[0].op.steps == o1.steps
                        && c.legs[1].op.steps == o2.steps
                };
                if !out.iter().any(same) {
                    out.push(Chain { legs: [l1.clone(), back], rank });
                }
            }
        }
    }
    // PROJ's order: known accuracy first (unknown ones with grids first), then by decreasing
    // area within the area of interest, then by accuracy, those without grids first.
    // A key per chain, compared in turn: a total order.
    let key = |c: &Chain| {
        let (accuracy, grids) = (c.rank.accuracy, c.rank.needs_grids());
        let area = aoi.map_or(Some(c.rank.bounds), |a| intersection(&c.rank.bounds, a)).map_or(0.0, |b| pseudo_area(&b));
        let known = accuracy >= 0.0;
        (!known, !known && !grids, -area, if known { accuracy } else { 0.0 }, grids)
    };
    out.sort_by(|x, y| {
        let (kx, ky) = (key(x), key(y));
        kx.0.cmp(&ky.0).then(kx.1.cmp(&ky.1)).then(kx.2.total_cmp(&ky.2)).then(kx.3.total_cmp(&ky.3)).then(kx.4.cmp(&ky.4))
    });
    let ballpark = !aoi.is_some_and(|a| out.iter().any(|c| !c.rank.needs_grids() && contains(&c.rank.bounds, a)));
    Kind::Chains { chains: out, ballpark }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crs::{SpatialRef, Transformer};

    /// A point moved from one EPSG CRS to another.
    fn moved(src: u32, dst: u32, x: f64, y: f64) -> (f64, f64) {
        let (s, d) = (SpatialRef::from_epsg(src).unwrap(), SpatialRef::from_epsg(dst).unwrap());
        Transformer::new(&s, &d).unwrap().transform(x, y).unwrap()
    }

    #[test]
    fn areas_across_the_antimeridian() {
        let russia = [18.92, 39.87, -168.97, 85.19];
        let azores = [-31.34, 39.3, -31.02, 39.77];
        assert_eq!(intersection(&russia, &azores), None);
        assert!(!intersects(&russia, &azores));
        assert_eq!(intersection(&russia, &[170.0, 50.0, -170.0, 60.0]), Some([170.0, 50.0, -170.0, 60.0]));
        assert_eq!(intersection(&[-180.0, -90.0, 180.0, 90.0], &russia), Some(russia));
        assert_eq!(intersection(&[100.0, 40.0, 120.0, 50.0], &russia), Some([100.0, 40.0, 120.0, 50.0]));
        assert!(contains(&russia, &[170.0, 50.0, -170.0, 60.0]) && contains(&russia, &[30.0, 50.0, 40.0, 60.0]));
        assert!(!contains(&russia, &azores) && !contains(&azores, &russia));
        assert!(contains(&[-180.0, -90.0, 180.0, 90.0], &azores));
    }

    #[test]
    fn chains_outside_their_areas_as_proj() {
        // Just east of the Azores Occidental 1939 area: no chain holds the point, and one holds
        // the area of interest (so PROJ drops its ballpark): the first chain, as GDAL.
        let (x, y) = moved(4182, 4740, -31.016816380974838, 39.56501297818613);
        assert!((x + 31.021097205869058).abs() < 1e-7 && (y - 39.566344404860935).abs() < 1e-7, "{x} {y}");
        // A single chain is used whatever the point, as GDAL does.
        let (x, y) = moved(2999, 9053, 298971.18861002335, 8694000.007133322);
        assert!((x - 43.16443339530448).abs() < 1e-7 && (y + 11.811498334670878).abs() < 1e-7, "{x} {y}");
    }
}

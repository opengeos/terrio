//! How a transformation moves points between two datums, as PROJ (and so GDAL) does: from the
//! transformations GDAL lists for the pair (sorted again for the CRSs' area of interest), or,
//! when PROJ's database has none between the two datums, by chaining two of the transformations
//! it registers (concatenations of several among them) through intermediate CRSs and datums as
//! PROJ's `proj_create_operations` finds and sorts them; then picking for each point the most
//! accurate whose area holds it, else the first that needs no grid (PROJ's `proj_trans`); a list
//! of one is used for every point.

use super::datum::{self, Op, Point, Step};
use super::def::Def;
use super::epsg;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
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
    /// Chains through intermediate datums, in PROJ's order, then PROJ's ballpark (no shift, which
    /// holds every point) when it lists one: the best for a point, else the ballpark, else the
    /// first chain that needs no grid.
    Chains { chains: Vec<Chain>, ballpark: Option<Op> },
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
    src_ops: Vec<Arc<Op>>,
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
    smaller(src.area(), dst.area())
}

/// The smaller of two areas, as PROJ measures them.
fn smaller(a: Option<[f64; 4]>, b: Option<[f64; 4]>) -> Option<[f64; 4]> {
    match (a, b) {
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
        Self::build(src, dst, false)
    }

    /// The route assuming all grids are available.
    pub fn all(src: &Def, dst: &Def) -> Self {
        Self::build(&src.with_all_operations(), &dst.with_all_operations(), true)
    }

    fn build(src: &Def, dst: &Def, all: bool) -> Self {
        let aoi = area_of_interest(src, dst);
        let keep = |op: &Op| relevant(op, src, dst, aoi.as_ref());
        // Between projected CRSs, PROJ concatenates the transformations with the projections,
        // which gives a concatenation its database registers the accuracy and area of its
        // steps (NAD83 to NAD83(NSRS2007) (NADCON5, Alaska) between British Columbia Albers
        // and UTM).
        let concatenated = !src.is_geographic() || !dst.is_geographic();
        let list = |ops: &[Arc<Op>], reverse: bool| -> Kind {
            Kind::List(
                legs(ops, reverse)
                    .into_iter()
                    .filter(|l| keep(&l.op))
                    .map(|l| {
                        let flat = concatenated && (l.op.chain_accuracy != l.op.accuracy || l.op.chain_bounds != l.op.bounds);
                        if flat { Leg { op: Arc::new(l.op.as_concatenated()), reverse: l.reverse } } else { l }
                    })
                    .collect(),
            )
        };
        let kind = if src.same_datum(dst) {
            Kind::Same
        } else if dst.is_wgs84() {
            list(src.ops(), false)
        } else if src.is_wgs84() {
            list(dst.ops(), true)
        } else {
            match (src.geog(), dst.geog()) {
                (Some(a), Some(b)) => match if all { epsg::pair_all(a, b) } else { epsg::pair(a, b) } {
                    // PROJ looks for transformations through other datums when its database has
                    // none it can use between the two for the CRSs' areas (NAD83(CSRS) to NAD83
                    // between Arctic CRSs, outside the offshore area of their only one, or with
                    // the grids missing).
                    Some((ops, reverse)) if ops.iter().any(|o| !o.ballpark && keep(o)) => {
                        let mut kind = list(&ops, reverse);
                        // GDAL's order is for the two geographic CRSs: between CRSs of another
                        // area of interest, PROJ sorts the transformations for theirs.
                        let area = |code: u32| epsg::get(code).and_then(|e| e.area);
                        if let Kind::List(l) = &mut kind
                            && aoi != smaller(area(a), area(b))
                        {
                            // Run in reverse, a transformation has the inverse's name.
                            let key = |leg: &Leg| {
                                let name = step_names(&leg.op, leg.reverse).join(" + ");
                                SortKey::new(&leg.op, name, pipeline_steps(&[(&leg.op, leg.reverse)]), aoi.as_ref())
                            };
                            let mut keyed: Vec<(SortKey, Leg)> = l.drain(..).map(|leg| (key(&leg), leg)).collect();
                            keyed.sort_by(|x, y| x.0.cmp(&y.0));
                            l.extend(keyed.into_iter().map(|(_, leg)| leg));
                        }
                        kind
                    }
                    _ => chains(a, b, src, dst, aoi.as_ref(), all),
                },
                _ => Kind::Hub,
            }
        };
        Self { kind, boxes: OnceLock::new(), hub_boxes: OnceLock::new(), dst_ops: dst.ops().to_vec(), src_ops: src.ops().to_vec() }
    }

    /// Refuse ballpark alternatives while retaining usable accurate operations.
    pub fn disallow_ballpark(&mut self) -> bool {
        match &mut self.kind {
            Kind::Same => true,
            Kind::List(list) => {
                list.retain(|l| !l.op.ballpark);
                !list.is_empty()
            }
            Kind::Chains { chains, ballpark } => {
                *ballpark = None;
                !chains.is_empty()
            }
            Kind::Hub => {
                self.src_ops.retain(|op| !op.ballpark);
                self.dst_ops.retain(|op| !op.ballpark);
                !self.src_ops.is_empty() && !self.dst_ops.is_empty()
            }
        }
    }

    /// The operation selected before evaluating its pipeline, and whether its area excludes the point.
    pub fn selected<'a>(&'a self, src: &Def, x: f64, y: f64, p: Point) -> (Option<&'a Op>, bool) {
        let (lon, lat) = src.source_degrees(x, y).unwrap_or((p.lon.to_degrees(), p.lat.to_degrees()));
        let inside = |op: &Op, boxes: Option<&Vec<[f64; 4]>>| match boxes {
            Some(b) if !b.is_empty() => b.iter().any(|b| x >= b[0] && x <= b[2] && y >= b[1] && y <= b[3]),
            _ => op.contains(lon, lat) || op.contains((lon + 180.0).rem_euclid(360.0) - 180.0, lat),
        };
        let choose = |ops: Vec<&'a Op>, boxes: &Boxes| {
            let i = datum::pick_index(&ops, &[], |op, i| op.ballpark || inside(op, boxes.get(i)))
                .or_else(|| ops.iter().position(|op| !op.needs_grids()));
            let op = i.map(|i| ops[i]);
            (op, i.is_some_and(|i| !ops[i].ballpark && !inside(ops[i], boxes.get(i))))
        };
        match &self.kind {
            Kind::Same => (None, false),
            Kind::List(list) => {
                let boxes = self.boxes.get_or_init(|| list.iter().map(|l| src.areas(&l.op)).collect());
                choose(list.iter().map(|l| l.op.as_ref()).collect(), boxes)
            }
            Kind::Chains { chains, ballpark } => {
                let boxes = self.boxes.get_or_init(|| chains.iter().map(|c| src.areas(&c.rank)).collect());
                choose(chains.iter().map(|c| &c.rank).chain(ballpark.as_ref()).collect(), boxes)
            }
            Kind::Hub => {
                let (sb, db) = self.hub_boxes.get_or_init(|| {
                    let areas = |ops: &[Arc<Op>]| ops.iter().map(|o| src.areas(o)).collect();
                    (areas(&self.src_ops), areas(&self.dst_ops))
                });
                let single = |op: &Op| !op.ballpark && !op.concatenated;
                let select = |ops: &'a [Arc<Op>], boxes: &Boxes| datum::pick_in(ops, |op, i| single(op) && inside(op, boxes.get(i)));
                match (select(&self.src_ops, sb), select(&self.dst_ops, db)) {
                    (Some(s), Some(d)) => (Some(if !s.available() { s } else { d }), false),
                    _ => (None, false),
                }
            }
        }
    }

    /// Evaluate exactly the chosen operation, without a fallback after a pipeline failure.
    pub fn apply_selected(&self, src: &Def, x: f64, y: f64, p: Point, three_d: bool, op: Option<&Op>) -> Option<Point> {
        let Some(op) = op else { return self.apply(src, x, y, p, three_d) };
        match &self.kind {
            Kind::List(list) => {
                let leg = list.iter().find(|l| std::ptr::eq(l.op.as_ref(), op))?;
                datum::run_op(op, p, leg.reverse, three_d)
            }
            Kind::Chains { chains, .. } => {
                if op.ballpark {
                    return Some(p);
                }
                let c = chains.iter().find(|c| std::ptr::eq(&c.rank, op))?;
                let [a, b] = &c.legs;
                let q = datum::run(&a.op.steps, p, a.reverse, true).and_then(|q| datum::run(&b.op.steps, q, b.reverse, true))?;
                Some(if three_d || !c.rank.keeps_height { q } else { Point { h: p.h, ..q } })
            }
            _ => self.apply(src, x, y, p, three_d),
        }
    }

    /// Whether this route represents an unchanged datum.
    pub fn same_datum(&self) -> bool {
        matches!(self.kind, Kind::Same)
    }

    /// The accuracy (metres) of the transformation [`Route::apply`] picks for a point, as PROJ
    /// adds it to the accuracy of a vertical transformation it chains with: 0 between CRSs of one
    /// datum, negative when unknown (a ballpark, or no transformation for the point).
    pub fn accuracy_at(&self, src: &Def, x: f64, y: f64, p: Point) -> f64 {
        let (dlon, dlat) = src.source_degrees(x, y).unwrap_or((p.lon.to_degrees(), p.lat.to_degrees()));
        let wrapped = if (-180.0..=180.0).contains(&dlon) { dlon } else { (dlon + 180.0).rem_euclid(360.0) - 180.0 };
        let inside = |op: &Op, boxes: Option<&Vec<[f64; 4]>>| match boxes {
            Some(b) if !b.is_empty() => b.iter().any(|b| x >= b[0] && x <= b[2] && y >= b[1] && y <= b[3]),
            _ => op.contains(dlon, dlat) || op.contains(wrapped, dlat),
        };
        let first_without_grids = |ops: &[&Op]| ops.iter().find(|o| !o.needs_grids()).map_or(-1.0, |o| o.accuracy);
        match &self.kind {
            Kind::Same => 0.0,
            Kind::List(list) if list.is_empty() => -1.0,
            Kind::List(list) => {
                let boxes = self.boxes.get_or_init(|| list.iter().map(|l| src.areas(&l.op)).collect());
                let ops: Vec<&Op> = list.iter().map(|l| l.op.as_ref()).collect();
                datum::pick_index(&ops, &[], |op, i| inside(op, boxes.get(i)))
                    .map_or_else(|| first_without_grids(&ops), |i| ops[i].accuracy)
            }
            Kind::Chains { chains, .. } if chains.is_empty() => -1.0,
            Kind::Chains { chains, ballpark } => {
                if chains.len() == 1 && ballpark.is_none() {
                    return chains[0].rank.accuracy;
                }
                let boxes = self.boxes.get_or_init(|| chains.iter().map(|c| src.areas(&c.rank)).collect());
                let ranks: Vec<&Op> = chains.iter().map(|c| &c.rank).chain(ballpark.as_ref()).collect();
                match datum::pick_index(&ranks, &[], |op, i| op.ballpark || inside(op, boxes.get(i))) {
                    Some(i) => ranks[i].accuracy,
                    None => first_without_grids(&ranks),
                }
            }
            Kind::Hub => -1.0,
        }
    }

    /// Moves a geodetic point (radians) from the source datum to the target's. `(x, y)` are its
    /// source coordinates, which pick the transformation for a projected source as in PROJ.
    /// Heights go through the transformation when `three_d` (between 3D CRSs). `None` when no
    /// transformation succeeds.
    pub fn apply(&self, src: &Def, x: f64, y: f64, p: Point, three_d: bool) -> Option<Point> {
        self.apply_reporting(src, x, y, p, three_d, &|_, _| {})
    }

    /// Apply a route and report the operation that actually succeeded, including retries.
    pub fn apply_reporting(
        &self,
        src: &Def,
        x: f64,
        y: f64,
        p: Point,
        three_d: bool,
        record: &impl Fn(Option<&Op>, bool),
    ) -> Option<Point> {
        let report = |result: Option<Point>, op: Option<&Op>, outside: bool| result.inspect(|_| record(op, outside));
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
            Kind::List(list) if list.is_empty() => report(Some(p), None, false),
            Kind::List(list) => {
                let boxes = self.boxes.get_or_init(|| list.iter().map(|l| src.areas(&l.op)).collect());
                let ops: Vec<&Op> = list.iter().map(|l| l.op.as_ref()).collect();
                datum::transform_with(
                    &ops,
                    |op, i| inside(op, boxes.get(i)),
                    |i| {
                        report(
                            datum::run_op(&list[i].op, p, list[i].reverse, three_d),
                            Some(&list[i].op),
                            !inside(&list[i].op, boxes.get(i)),
                        )
                    },
                )
            }
            // PROJ's ballpark alone: no shift.
            Kind::Chains { chains, .. } if chains.is_empty() => report(Some(p), None, false),
            Kind::Chains { chains, ballpark } => {
                // Heights go through both transformations, as in PROJ's pipeline for the chain (the
                // conversion to geodetic coordinates and back between them cancelling out), and
                // are restored after them when the chain keeps them.
                let run = |c: &Chain| {
                    let [l1, l2] = &c.legs;
                    let r = datum::run(&l1.op.steps, p, l1.reverse, true).and_then(|q| datum::run(&l2.op.steps, q, l2.reverse, true))?;
                    Some(if three_d || !c.rank.keeps_height { r } else { Point { h: p.h, ..r } })
                };
                // A single operation is used for every point, whatever its area, as PROJ does.
                if chains.len() == 1 && ballpark.is_none() {
                    let chain = &chains[0];
                    let boxes = self.boxes.get_or_init(|| chains.iter().map(|c| src.areas(&c.rank)).collect());
                    return report(run(chain), Some(&chain.rank), !inside(&chain.rank, boxes.first()));
                }
                let boxes = self.boxes.get_or_init(|| chains.iter().map(|c| src.areas(&c.rank)).collect());
                let ranks: Vec<&Op> = chains.iter().map(|c| &c.rank).chain(ballpark.as_ref()).collect();
                let mut excluded = Vec::new();
                for _ in 0..3 {
                    let Some(i) = datum::pick_index(&ranks, &excluded, |op, i| op.ballpark || inside(op, boxes.get(i))) else { break };
                    // The ballpark (more accurate than the chains between CRSs that differ by
                    // their prime meridians alone): no shift.
                    let Some(chain) = chains.get(i) else { return report(Some(p), None, false) };
                    if let Some(r) = run(chain) {
                        return report(Some(r), Some(&chain.rank), !inside(&chain.rank, boxes.get(i)));
                    }
                    excluded.push(i);
                }
                if ballpark.is_some() {
                    // The ballpark holds every point: no shift.
                    return report(Some(p), None, false);
                }
                // No transformation for the point: the first that needs no grid, as PROJ's
                // `proj_trans`.
                let (i, first) = chains.iter().enumerate().find(|(_, c)| !c.rank.needs_grids())?;
                report(run(first), Some(&first.rank), !inside(&first.rank, boxes.get(i)))
            }
            Kind::Hub => {
                let (sb, db) = self.hub_boxes.get_or_init(|| {
                    let areas = |ops: &[Arc<Op>]| ops.iter().map(|o| src.areas(o)).collect();
                    (areas(&self.src_ops), areas(&self.dst_ops))
                });
                // Single transformations only, as PROJ chains them.
                let single = |op: &Op| !op.ballpark && !op.concatenated;
                let s = datum::pick_in(&self.src_ops, |op, i| single(op) && inside(op, sb.get(i)));
                let d = datum::pick_in(&self.dst_ops, |op, i| single(op) && inside(op, db.get(i)));
                match (s, d) {
                    (Some(s), Some(d)) => {
                        // Heights go through both, as in PROJ's pipeline (see the chains).
                        let r = datum::run(&s.steps, p, false, true).and_then(|q| datum::run(&d.steps, q, true, true))?;
                        let result = Some(if three_d || !(s.keeps_height && d.keeps_height) { r } else { Point { h: p.h, ..r } });
                        record(Some(s), false);
                        report(result, Some(d), false)
                    }
                    // No transformation (or an unknown datum): no shift, as PROJ's ballpark.
                    _ => report(Some(p), None, false),
                }
            }
        }
    }
}

/// What a chain is made of, before PROJ sorts the chains: its two registered transformations in
/// the directions they run, and the names of the conversions PROJ frames them with.
struct Found {
    legs: [(usize, bool); 2],
    /// Conversions before the first transformation, between them, and after the second.
    conversions: [Option<String>; 3],
}

impl Found {
    /// The chain run in reverse.
    fn reversed(self) -> Self {
        let [(i1, r1), (i2, r2)] = self.legs;
        // A conversion's inverse, but an axis order change, which is its own.
        let back = |c: Option<String>| c.map(|n| if n.starts_with("axis order change") { n } else { inverse_name(&n) });
        let [c0, c1, c2] = self.conversions;
        Found { legs: [(i2, !r2), (i1, !r1)], conversions: [back(c2), back(c1), back(c0)] }
    }
}

/// PROJ's name for the operation between two geodetic CRSs of one datum, as its `buildOpName`
/// makes it (with the kind of each CRS when their names are the same): a conversion to or from
/// geocentric coordinates, an axis order change between CRSs of one kind, else a null offset
/// (which PROJ leaves out of the chains of its datum pivot).
fn conversion_name(from: &epsg::Geodetic, to: &epsg::Geodetic) -> String {
    use epsg::GeodeticKind::*;
    let qualifier = |g: &epsg::Geodetic| match g.kind {
        Geographic2D => " (geog2D)",
        Geographic3D => " (geog3D)",
        Geocentric => " (geocentric)",
    };
    match (from.kind, to.kind) {
        (Geographic2D, Geographic2D) => return "axis order change (2D)".to_string(),
        (Geographic3D, Geographic3D) => return "axis order change (geographic3D horizontal)".to_string(),
        _ => {}
    }
    let (q1, q2) = if from.name == to.name { (qualifier(from), qualifier(to)) } else { ("", "") };
    let kind = if from.kind == Geocentric || to.kind == Geocentric { "Conversion" } else { NULL_OFFSET };
    format!("{kind} from {}{q1} to {}{q2}", from.name, to.name)
}

/// The name of PROJ's null transformations between geographic CRSs of one datum.
const NULL_OFFSET: &str = "Null geographic offset";

/// The chains of two transformations from geographic CRS `a` to `b` through intermediate
/// datums, as PROJ's `proj_create_operations` lists them for CRSs `src` and `dst` when its
/// database has no transformation between the two datums, sorted as PROJ sorts them:
///
/// - first those through an intermediate CRS PROJ's database registers transformations to
///   from CRSs of both datums (`createFromCRSCodesWithIntermediates`), trying the geographic
///   2D, 3D and geocentric CRSs of the two datums in order of code (its datum pivot), and
///   keeping those of the first pair of CRSs that has some;
/// - then, when that finds none, or only chains through PROJ's null transformations between
///   the WGS 84 ensemble and its realizations, or only chains over less than half the CRSs'
///   common area, or only time-dependent ones between static CRSs, those through an
///   intermediate datum, whatever its CRSs (`createBetweenGeodeticCRSWithDatumBasedIntermediates`).
///
/// Transformations PROJ's database replaces by another it finds too are left out, as are those
/// whose grids are missing. PROJ ends the list with its ballpark (no shift) unless a chain that
/// needs no grid holds the area of interest.
fn chains(a: u32, b: u32, src: &Def, dst: &Def, aoi: Option<&[f64; 4]>, all: bool) -> Kind {
    let reg = epsg::registry();
    let (Some(ga), Some(gb)) = (reg.geodetic.get(&a), reg.geodetic.get(&b)) else {
        return Kind::Chains { chains: Vec::new(), ballpark: Some(ballpark_op(src, dst)) };
    };
    let extents: Vec<[f64; 4]> = [src.area(), dst.area()].into_iter().flatten().filter(|e| *e != [-180.0, -90.0, 180.0, 90.0]).collect();
    // PROJ looks for the transformations to a projected CRS from it, and runs them in reverse:
    // those it finds are the chains from `b` to `a` (it sorts them once reversed).
    let reverse = !dst.is_geographic();
    let (ga, gb) = if reverse { (gb, ga) } else { (ga, gb) };
    let mut found = crs_code_chains(reg, ga, gb, &extents, all);
    let ensemble_only = !found.is_empty() && found.iter().all(|f| f.legs.iter().any(|&(i, _)| reg.ops[i].ensemble));
    let time_only = !found.is_empty()
        && !src.is_dynamic()
        && !dst.is_dynamic()
        && found.iter().all(|f| f.legs.iter().any(|&(i, _)| reg.ops[i].time_dependent));
    let small = too_small(reg, &found, src.area(), dst.area());
    if found.is_empty() || ensemble_only || time_only || small {
        // After chains over too small an area, those of other names only.
        let known: Vec<String> = if small { found.iter().map(|f| found_name(reg, f)).collect() } else { Vec::new() };
        for f in datum_chains(reg, ga, gb, &extents, all) {
            if !small || !known.contains(&found_name(reg, &f)) {
                found.push(f);
            }
        }
    }
    if reverse {
        found = found.into_iter().map(Found::reversed).collect();
    }
    // The chains, each with what PROJ sorts it by.
    let mut out: Vec<(SortKey, Chain)> = Vec::new();
    for f in found {
        let [(i1, r1), (i2, r2)] = f.legs;
        let (o1, o2) = (&reg.ops[i1].op, &reg.ops[i2].op);
        let Some(bounds) = intersection(&o1.chain_bounds, &o2.chain_bounds) else { continue };
        if aoi.is_some_and(|a| !intersects(&bounds, a)) {
            continue;
        }
        // The common area has the name (and so the flags) of the leg whose area it is.
        let named = if contains(&o1.chain_bounds, &o2.chain_bounds) {
            Some(o2)
        } else if contains(&o2.chain_bounds, &o1.chain_bounds) {
            Some(o1)
        } else {
            None
        };
        let (a1, a2) = (o1.chain_accuracy, o2.chain_accuracy);
        let accuracy = if a1 >= 0.0 && a2 >= 0.0 { a1 + a2 } else { -1.0 };
        let name = found_name(reg, &f);
        let rank = Op {
            bounds,
            accuracy,
            chain_accuracy: accuracy,
            chain_bounds: bounds,
            offshore: named.is_some_and(|o| o.offshore),
            // An intersection of areas that is neither leg's has no name, which PROJ takes for
            // an unknown area.
            unknown_area: named.is_none_or(|o| o.unknown_area),
            ballpark: false,
            concatenated: true,
            between_3d: o1.between_3d,
            geocentric: o1.geocentric || o2.geocentric,
            priority: o1.priority || o2.priority || datum::is_priority(&name),
            keeps_height: o1.keeps_height && o2.keeps_height,
            steps: o1.steps.iter().chain(&o2.steps).cloned().collect(),
            name,
        };
        let legs = [Leg { op: o1.clone(), reverse: r1 }, Leg { op: o2.clone(), reverse: r2 }];
        let key = SortKey::new(&rank, rank.name.clone(), pipeline_steps(&[(o1, r1), (o2, r2)]), aoi);
        out.push((key, Chain { legs, rank }));
    }
    out.sort_by(|x, y| x.0.cmp(&y.0));
    // PROJ drops its ballpark when a chain that needs no grid holds the area of interest, even
    // one it then drops itself.
    let ballpark = !aoi.is_some_and(|a| out.iter().any(|(_, c)| !c.rank.needs_grids() && contains(&c.rank.bounds, a)));
    let chains = remove_uninteresting(out.into_iter().map(|(_, c)| c).collect());
    Kind::Chains { chains, ballpark: ballpark.then(|| ballpark_op(src, dst)) }
}

/// The chains PROJ keeps once sorted: without one that brings nothing over the previous one kept
/// (of the same area, as or less accurate, with more transformations; PROJ's
/// `removeUninterestingOps`), nor one that runs the same steps over the same area as another
/// before it (`removeDuplicateOps`).
fn remove_uninteresting(chains: Vec<Chain>) -> Vec<Chain> {
    // The transformations of a chain, as PROJ counts them: its steps but conversions.
    let count =
        |c: &Chain| c.rank.name.split(" + ").filter(|n| !n.starts_with("Conversion from ") && !n.starts_with("axis order change")).count();
    let mut kept: Vec<Chain> = Vec::new();
    for c in chains {
        if let Some(last) = kept.last() {
            let (accuracy, last_accuracy) = (c.rank.accuracy, last.rank.accuracy);
            if ((accuracy >= last_accuracy && last_accuracy >= 0.0) || (accuracy < 0.0 && last_accuracy >= 0.0))
                && c.rank.bounds == last.rank.bounds
                && count(&c) > count(last)
            {
                continue;
            }
        }
        kept.push(c);
    }
    let mut out: Vec<Chain> = Vec::new();
    for c in kept {
        let same = |o: &Chain| {
            o.rank.bounds == c.rank.bounds && o.legs.iter().zip(&c.legs).all(|(x, y)| x.reverse == y.reverse && x.op.steps == y.op.steps)
        };
        if !out.iter().any(same) {
            out.push(c);
        }
    }
    out
}

/// PROJ's name for a chain: the names of its conversions and transformations (those of a
/// registered concatenation's steps), without the conversions to an intermediate CRS and back.
fn found_name(reg: &epsg::Registry, f: &Found) -> String {
    let [(i1, r1), (i2, r2)] = f.legs;
    let mut names: Vec<String> = Vec::new();
    let mut push = |more: Vec<String>| {
        for n in more {
            if names.last().is_some_and(|last| inverse_name(last) == n) {
                names.pop();
            } else {
                names.push(n);
            }
        }
    };
    push(f.conversions[0].iter().cloned().collect());
    push(step_names(&reg.ops[i1].op, r1));
    push(f.conversions[1].iter().cloned().collect());
    push(step_names(&reg.ops[i2].op, r2));
    push(f.conversions[2].iter().cloned().collect());
    names.join(" + ")
}

/// Whether every chain covers at most half the CRSs' common area, as PROJ's
/// `hasTooSmallAreas` measures it (for fewer than 10 chains).
fn too_small(reg: &epsg::Registry, found: &[Found], e1: Option<[f64; 4]>, e2: Option<[f64; 4]>) -> bool {
    if found.is_empty() || found.len() >= 10 {
        return false;
    }
    let area = |b: Option<[f64; 4]>| b.map_or(0.0, |b| pseudo_area(&b));
    let target = match (e1, e2) {
        (Some(x), Some(y)) => area(extent_intersection(&x, &y)),
        (x, y) => area(x.or(y)),
    };
    if target <= 0.0 {
        return false;
    }
    !found.iter().any(|f| {
        let [(i1, _), (i2, _)] = f.legs;
        let Some(op) = intersection(&reg.ops[i1].op.chain_bounds, &reg.ops[i2].op.chain_bounds) else { return false };
        let covered = match (e1, e2) {
            (Some(x), Some(y)) => {
                let (p, q) = (extent_intersection(&op, &x), extent_intersection(&op, &y));
                area(p) + area(q) - p.zip(q).map_or(0.0, |(p, q)| area(extent_intersection(&p, &q)))
            }
            (x, y) => x.or(y).map_or(pseudo_area(&op), |e| area(extent_intersection(&op, &e))),
        };
        covered > 0.5 * target
    })
}

/// The intersection of two extents as PROJ's `Extent::intersection` computes it: the one
/// when it holds the other, else the intersection of their areas.
fn extent_intersection(a: &[f64; 4], b: &[f64; 4]) -> Option<[f64; 4]> {
    if contains(a, b) {
        return Some(*b);
    }
    if contains(b, a) {
        return Some(*a);
    }
    intersection(a, b)
}

/// Whether PROJ rejects an intermediate CRS between two CRSs: an ITRF realization out of the
/// years of two ETRF realizations, NAD83 or WGS 84 between two NAD83(CSRS) realizations.
fn rejected_pivot(from: &str, to: &str, pivot: &str) -> bool {
    let year = |name: &str, prefix: &str| {
        let digits: String = name.strip_prefix(prefix)?.chars().take_while(char::is_ascii_digit).collect();
        let v: i32 = digits.parse().ok()?;
        (v > 0).then_some(if (80..=99).contains(&v) { v + 1900 } else { v })
    };
    if from.starts_with("ETRF")
        && to.starts_with("ETRF")
        && let (Some(s), Some(t), Some(p)) = (year(from, "ETRF"), year(to, "ETRF"), year(pivot, "ITRF"))
        && (p < s.min(t) || p > s.max(t))
    {
        return true;
    }
    from.starts_with("NAD83(CSRS)") && to.starts_with("NAD83(CSRS)") && matches!(pivot, "NAD83" | "WGS 84")
}

/// Whether a registered concatenation goes through one of two CRSs (PROJ's
/// `useIrrelevantPivot`).
fn irrelevant(reg: &epsg::Registry, i: usize, a: u32, b: u32) -> bool {
    reg.ops[i].via.iter().any(|&c| c == a || c == b)
}

/// The transformations of a list that others of the list replace: those of which one
/// replacement is in the list when `any`, else those of which every replacement is (PROJ
/// queries a transformation once per replacement, and drops the rows whose replacement it
/// finds).
fn superseded(reg: &epsg::Registry, ids: impl Iterator<Item = usize> + Clone, any: bool) -> HashSet<usize> {
    let keys: HashSet<&str> = ids.clone().map(|i| reg.ops[i].key.as_str()).collect();
    let found = |r: &String| keys.contains(r.as_str());
    ids.filter(|&i| {
        let r = &reg.ops[i].replaced_by;
        !r.is_empty() && if any { r.iter().any(found) } else { r.iter().all(found) }
    })
    .collect()
}

/// PROJ's datum pivot: the chains through an intermediate CRS between the geodetic CRSs of the
/// datums of `a` and `b`, in order of code: first those of their names and of one kind, then
/// those of other names, then of their names and of other kinds; those of the first pair of CRSs
/// that has some (then also of the next pairs while every chain found uses one of PROJ's own
/// transformations).
fn crs_code_chains(reg: &epsg::Registry, a: &epsg::Geodetic, b: &epsg::Geodetic, extents: &[[f64; 4]], all: bool) -> Vec<Found> {
    let candidates = |g: &epsg::Geodetic| -> Vec<&epsg::Geodetic> {
        reg.by_datum.get(&g.datum).into_iter().flatten().filter_map(|c| reg.geodetic.get(c)).collect()
    };
    let (ca, cb) = (candidates(a), candidates(b));
    let mut found: Vec<Found> = Vec::new();
    let proj_only = |found: &[Found]| found.iter().all(|f| f.legs.iter().any(|&(i, _)| reg.ops[i].key.starts_with("PROJ:")));
    for same_kind in [true, false] {
        // Of the names of `a` and `b`, of one kind (then of other kinds); then of other names.
        let pairs = ca
            .iter()
            .flat_map(|x| cb.iter().map(move |y| (*x, *y)))
            .filter(|(x, y)| x.name == a.name && y.name == b.name && (x.kind == y.kind) == same_kind)
            .chain(ca.iter().flat_map(|x| cb.iter().map(move |y| (*x, *y))).filter(|(x, y)| x.name != a.name || y.name != b.name));
        for (x, y) in pairs {
            let more = crs_code_pair(reg, x, y, extents, all);
            if more.is_empty() {
                continue;
            }
            // The operations from `a` to `x` and from `y` to `b`, without null ones.
            let framing = |from: &epsg::Geodetic, to: &epsg::Geodetic| {
                (from.code != to.code).then(|| conversion_name(from, to)).filter(|n| !n.starts_with(NULL_OFFSET))
            };
            let (first, last) = (framing(a, x), framing(y, b));
            found.extend(more.into_iter().map(|legs| Found { legs, conversions: [first.clone(), None, last.clone()] }));
            if !proj_only(&found) {
                return found;
            }
        }
    }
    found
}

/// The chains of two transformations PROJ's database registers from CRS `x` to an intermediate
/// CRS and from it to CRS `y` (PROJ's `createFromCRSCodesWithIntermediates`): through a
/// geographic CRS of a datum published since the older of theirs (or of none, as WGS 84)
/// between geographic CRSs, through any CRS otherwise; with areas that intersect each other and
/// the CRSs' areas; without those replaced by another of the same direction found.
fn crs_code_pair(reg: &epsg::Registry, x: &epsg::Geodetic, y: &epsg::Geodetic, extents: &[[f64; 4]], all: bool) -> Vec<[(usize, bool); 2]> {
    use epsg::GeodeticKind::*;
    let geographic = x.kind != Geocentric && y.kind != Geocentric;
    let oldest = if geographic { epsg::datum_date(x.datum).zip(epsg::datum_date(y.datum)).map(|(p, q)| p.min(q)) } else { None };
    let fits = |i: usize| extents.iter().all(|e| intersects(&reg.ops[i].op.bounds, e));
    let (Some(from_x), Some(from_y)) = (reg.at.get(&x.code), reg.at.get(&y.code)) else { return Vec::new() };
    // The other end of a transformation from or to a CRS, and whether it runs in reverse to
    // leave the CRS.
    let other = |i: usize, code: u32| {
        let r = &reg.ops[i];
        if r.source == code { (r.target, false) } else { (r.source, true) }
    };
    let mut out = Vec::new();
    // PROJ's four queries, by the directions of the two transformations, each without those
    // replaced by another it finds.
    for (rev1, rev2) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut rows: Vec<(usize, usize, u32)> = Vec::new();
        for &i in from_x {
            let (pivot, r1) = other(i, x.code);
            if r1 != rev1 || !fits(i) {
                continue;
            }
            let Some(p) = reg.geodetic.get(&pivot) else { continue };
            if geographic && (p.kind == Geocentric || oldest.is_some_and(|oldest| epsg::datum_date(p.datum).is_some_and(|d| d < oldest))) {
                continue;
            }
            for &j in from_y {
                // The second runs from the pivot to `y`: in reverse when registered from `y`.
                let (back, r2) = other(j, y.code);
                if back != pivot || r2 == rev2 || i == j || !fits(j) || !intersects(&reg.ops[i].op.bounds, &reg.ops[j].op.bounds) {
                    continue;
                }
                rows.push((i, j, pivot));
            }
        }
        let gone1 = superseded(reg, rows.iter().map(|r| r.0), false);
        let gone2 = superseded(reg, rows.iter().map(|r| r.1), false);
        for (i, j, pivot) in rows {
            let name = reg.geodetic.get(&pivot).map_or("", |p| p.name.as_str());
            // Then those whose grids are missing.
            if gone1.contains(&i)
                || gone2.contains(&j)
                || (!all && !reg.ops[i].op.available())
                || (!all && !reg.ops[j].op.available())
                || irrelevant(reg, i, x.code, y.code)
                || irrelevant(reg, j, x.code, y.code)
                || rejected_pivot(&x.name, &y.name, name)
            {
                continue;
            }
            out.push([(i, rev1), (j, rev2)]);
        }
    }
    out
}

/// PROJ's chains through an intermediate datum (`createBetweenGeodeticCRSWithDatumBasedIntermediates`):
/// transformations PROJ's database registers between any CRSs of the datum of `a` and of a
/// third datum, and of that datum and the datum of `b`, whose areas intersect each other and the
/// CRSs' areas, without those replaced by another found or that use a datum published before
/// 1900 (unless both datums are), with the conversions between the CRSs they go between.
fn datum_chains(reg: &epsg::Registry, a: &epsg::Geodetic, b: &epsg::Geodetic, extents: &[[f64; 4]], all: bool) -> Vec<Found> {
    let date = epsg::datum_date;
    let csrs = a.name.starts_with("NAD83(CSRS)") && b.name.starts_with("NAD83(CSRS)");
    let fits = |i: usize| extents.iter().all(|e| intersects(&reg.ops[i].op.bounds, e));
    // The transformations from or to the CRSs of a datum, by the datum at their other end, with
    // whether they leave the datum in reverse.
    let side = |datum: u32| -> HashMap<u32, Vec<(usize, bool)>> {
        let mut by: HashMap<u32, Vec<(usize, bool)>> = HashMap::new();
        for code in reg.by_datum.get(&datum).into_iter().flatten() {
            for &i in reg.at.get(code).into_iter().flatten() {
                let r = &reg.ops[i];
                let (other, reverse) = if r.source == *code { (r.target, false) } else { (r.source, true) };
                let Some(o) = reg.geodetic.get(&other) else { continue };
                if o.datum == datum || (csrs && matches!(o.name.as_str(), "NAD83" | "WGS 84")) || !fits(i) {
                    continue;
                }
                let list = by.entry(o.datum).or_default();
                if !list.contains(&(i, reverse)) {
                    list.push((i, reverse));
                }
            }
        }
        by
    };
    let (from_a, from_b) = (side(a.datum), side(b.datum));
    let mut pairs: Vec<((usize, bool), (usize, bool))> = Vec::new();
    let mut pivots: Vec<&u32> = from_a.keys().collect();
    pivots.sort_unstable();
    for pivot in pivots {
        let Some(second) = from_b.get(pivot) else { continue };
        for &(i, r1) in &from_a[pivot] {
            if irrelevant(reg, i, a.code, b.code) {
                continue;
            }
            for &(j, r2) in second {
                if intersects(&reg.ops[i].op.bounds, &reg.ops[j].op.bounds) && !irrelevant(reg, j, a.code, b.code) {
                    pairs.push(((i, r1), (j, !r2)));
                }
            }
        }
    }
    let gone = superseded(reg, pairs.iter().flat_map(|(x, y)| [x.0, y.0]), true);
    let oldest = match (date(a.datum), date(b.datum)) {
        (s, Some(t)) if s.is_none_or(|s| t < s) => Some(t),
        (s, _) => s,
    };
    let ancient = |code: u32| {
        reg.geodetic.get(&code).and_then(|g| date(g.datum)).is_some_and(|d| d < "1900-01-01") && oldest.is_some_and(|o| o > "1900-01-01")
    };
    let mut out = Vec::new();
    for ((i, r1), (j, r2)) in pairs {
        let (o1, o2) = (&reg.ops[i], &reg.ops[j]);
        if gone.contains(&i)
            || gone.contains(&j)
            || [o1.source, o1.target, o2.source, o2.target].into_iter().any(ancient)
            || (!all && !o1.op.available())
            || (!all && !o2.op.available())
        {
            continue;
        }
        // The CRSs the transformations go from and to, in the directions they run.
        let ends = |r: &epsg::Registered, reverse: bool| if reverse { (r.target, r.source) } else { (r.source, r.target) };
        let ((s1, t1), (s2, t2)) = (ends(o1, r1), ends(o2, r2));
        let g = |code: u32| reg.geodetic.get(&code);
        let conversion = |from: u32, to: u32| match (g(from), g(to)) {
            (Some(f), Some(t)) if from != to => Some(conversion_name(f, t)),
            _ => None,
        };
        out.push(Found { legs: [(i, r1), (j, r2)], conversions: [conversion(a.code, s1), conversion(t1, s2), conversion(t2, b.code)] });
    }
    out
}

/// PROJ's ballpark transformation between two CRSs, which holds every point: of unknown
/// accuracy, but of 0 m when it is a longitude rotation alone (CRSs on one ellipsoid with
/// different prime meridians, such as MGI 1901 and Greek (Athens)), which then wins over every
/// chain PROJ lists before it.
fn ballpark_op(src: &Def, dst: &Def) -> Op {
    let accuracy = if src.rotation_only_to(dst) { 0.0 } else { -1.0 };
    Op { accuracy, chain_accuracy: accuracy, ballpark: true, ..Op::world(Vec::new()) }
}

/// What PROJ sorts a transformation by, in its order: known accuracy first (unknown ones with
/// grids first), then decreasing area within the area of interest, then accuracy, those without
/// grids first, then those of fewer steps, then of fewer pipeline steps, then of shorter name,
/// then of greater name.
struct SortKey {
    unknown: bool,
    unknown_without_grids: bool,
    area: f64,
    accuracy: f64,
    grids: bool,
    steps: usize,
    pipeline_steps: usize,
    name: String,
}

impl SortKey {
    /// The key of a transformation with PROJ's name for it and about how many steps its
    /// pipeline has.
    fn new(op: &Op, name: String, pipeline_steps: usize, aoi: Option<&[f64; 4]>) -> Self {
        let area = aoi.map_or(Some(op.bounds), |a| intersection(&op.bounds, a)).map_or(0.0, |b| pseudo_area(&b));
        let (unknown, grids) = (op.accuracy < 0.0, op.needs_grids());
        Self {
            unknown,
            unknown_without_grids: unknown && !grids,
            area,
            accuracy: if unknown { 0.0 } else { op.accuracy },
            grids,
            steps: name.split(" + ").count(),
            pipeline_steps,
            name,
        }
    }

    /// The order of two keys: a total order.
    fn cmp(&self, other: &Self) -> Ordering {
        self.unknown
            .cmp(&other.unknown)
            .then(self.unknown_without_grids.cmp(&other.unknown_without_grids))
            .then(other.area.total_cmp(&self.area))
            .then(self.accuracy.total_cmp(&other.accuracy))
            .then(self.grids.cmp(&other.grids))
            .then(self.steps.cmp(&other.steps))
            .then(self.pipeline_steps.cmp(&other.pipeline_steps))
            .then(self.name.len().cmp(&other.name.len()))
            .then_with(|| other.name.cmp(&self.name))
    }
}

/// About how many steps PROJ's pipeline for transformations run one after the other (each in
/// its direction) has, which PROJ prefers fewer of: their datum steps, without a conversion to
/// geocentric coordinates right after its inverse on the same ellipsoid (which PROJ's pipeline
/// optimization takes out), with consecutive geocentric translations merged into one (or taken
/// out when they cancel), as PROJ's optimization does, and the `push v_3` and `pop v_3` around
/// geocentric steps.
fn pipeline_steps(legs: &[(&Op, bool)]) -> usize {
    /// A step of the pipeline: a datum step and whether it runs inverted, or translations PROJ
    /// merged.
    enum Item<'a> {
        Step(&'a Step, bool),
        Translation([f64; 3]),
    }
    // The ellipsoid of a conversion and whether it runs from geocentric coordinates.
    let cart = |item: &Item| match item {
        Item::Step(Step::Cart { ell, inv }, inverted) => Some((*ell, *inv != *inverted)),
        _ => None,
    };
    // The translation of a Helmert step with translations alone, as it runs.
    let translation = |item: &Item| match item {
        Item::Translation(t) => Some(*t),
        Item::Step(Step::Helmert { t, r, scale, pivot, inv, rates: None }, inverted)
            if *scale == 1.0 && *pivot == [0.0; 3] && *r == [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] =>
        {
            let sign = if inv != inverted { -1.0 } else { 1.0 };
            Some(t.map(|v| v * sign))
        }
        _ => None,
    };
    let mut run: Vec<Item> = Vec::new();
    for &(op, reverse) in legs {
        let steps: Vec<(&Step, bool)> =
            if reverse { op.steps.iter().rev().map(|s| (s, true)).collect() } else { op.steps.iter().map(|s| (s, false)).collect() };
        for (step, inverted) in steps {
            let item = Item::Step(step, inverted);
            if let (Some(last), Some((ell, false))) = (run.last(), cart(&item))
                && cart(last) == Some((ell, true))
            {
                run.pop();
                continue;
            }
            if let (Some(a), Some(b)) = (run.last().and_then(translation), translation(&item)) {
                run.pop();
                let sum = [a[0] + b[0], a[1] + b[1], a[2] + b[2]];
                if sum != [0.0; 3] {
                    run.push(Item::Translation(sum));
                }
                continue;
            }
            run.push(item);
        }
    }
    let geocentric = run.iter().any(|item| cart(item).is_some());
    run.len() + if geocentric { 2 } else { 0 }
}

/// PROJ's name for the inverse of a single operation: the names PROJ makes (`Conversion from A
/// to B`) with their CRSs swapped, others with `Inverse of ` added or taken off.
fn inverse_name(name: &str) -> String {
    if let Some(forward) = name.strip_prefix("Inverse of ") {
        return forward.to_string();
    }
    for kind in ["Conversion", "Null geographic offset", "Null geocentric translation"] {
        if let Some((from, to)) = name.strip_prefix(kind).and_then(|n| n.strip_prefix(" from ")).and_then(|n| n.split_once(" to ")) {
            return format!("{kind} from {to} to {from}");
        }
    }
    format!("Inverse of {name}")
}

/// The names of a transformation's steps as PROJ names them when it runs `reverse` or not: those
/// of the inverse are the inverses of the steps in reverse order.
fn step_names(op: &Op, reverse: bool) -> Vec<String> {
    let names: Vec<&str> = op.name.split(" + ").filter(|n| !n.is_empty()).collect();
    if reverse { names.into_iter().rev().map(inverse_name).collect() } else { names.into_iter().map(str::to_string).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SpatialRef, Transformer};

    #[test]
    fn reports_the_successful_fallback_after_a_grid_pipeline_fails() {
        let src = Def::new(&SpatialRef::parse("EPSG:4610").unwrap()).unwrap();
        let preferred = Op { accuracy: 1.0, steps: datum::nadgrids_steps("/nonexistent/terrio-policy-grid.tif"), ..Op::world(Vec::new()) };
        let fallback = Op { ballpark: true, accuracy: -1.0, ..Op::world(Vec::new()) };
        let route = Route {
            kind: Kind::List(vec![Leg { op: Arc::new(preferred), reverse: false }, Leg { op: Arc::new(fallback), reverse: false }]),
            boxes: OnceLock::new(),
            hub_boxes: OnceLock::new(),
            src_ops: Vec::new(),
            dst_ops: Vec::new(),
        };
        let p = Point { lon: 110_f64.to_radians(), lat: 35_f64.to_radians(), h: 0.0, t: None };
        let used_ballpark = std::cell::Cell::new(false);
        assert!(route.apply_reporting(&src, 110.0, 35.0, p, false, &|op, _| used_ballpark.set(op.is_none_or(|op| op.ballpark))).is_some());
        assert!(used_ballpark.get());
    }

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

    #[test]
    fn chain_names_as_proj() {
        assert_eq!(inverse_name("NTF to WGS 84 (1)"), "Inverse of NTF to WGS 84 (1)");
        assert_eq!(inverse_name("Inverse of WGS 72BE to WGS 84 (1)"), "WGS 72BE to WGS 84 (1)");
        assert_eq!(inverse_name("Conversion from A (geog2D) to A (geocentric)"), "Conversion from A (geocentric) to A (geog2D)");
        // The conversions between the CRSs of a datum, as PROJ names them.
        let reg = epsg::registry();
        let name = |a: u32, b: u32| conversion_name(&reg.geodetic[&a], &reg.geodetic[&b]);
        assert_eq!(name(7844, 7842), "Conversion from GDA2020 (geog2D) to GDA2020 (geocentric)");
        assert_eq!(name(4258, 4937), "Null geographic offset from ETRS89 (geog2D) to ETRS89 (geog3D)");
        assert_eq!(name(10310, 10312), "axis order change (2D)");
    }

    #[test]
    fn chains_ordered_as_proj() {
        let close = |(x, y): (f64, f64), (gx, gy): (f64, f64), tol: f64| (x - gx).abs() < tol && (y - gy).abs() < tol;
        // GDA94 through WGS 84 (G1762) and PROJ's own concatenation of the WGS 84 realizations.
        let p = moved(10459, 10606, 19600.97347104833, 592323.3389500268);
        assert!(close(p, (113.95807615479848, -27.180067590263132), 1e-8), "{p:?}");
        // Through WGS 84 rather than WGS 72, of equal accuracy and area: the greater name first.
        let p = moved(27573, 32431, 412711.90358792024, 3155755.5255349036);
        assert!(close(p, (259245.25512248135, 4840436.820252412), 1e-3), "{p:?}");
        // GDA94 to WGS 84 (1), a null transformation, makes fewer pipeline steps than (2).
        let p = moved(28351, 4324, 625263.796562132, 8842164.96869988);
        assert!(close(p, (124.144357767899, -10.472091951341795), 1e-8), "{p:?}");
        // Through WGS 84 rather than ETRS89, of equal accuracy and area: the shorter name first.
        let p = moved(2392, 4124, 2500642.641281675, 7632767.385064217);
        assert!(close(p, (24.016658156077746, 68.77924926097009), 1e-7), "{p:?}");
    }
}

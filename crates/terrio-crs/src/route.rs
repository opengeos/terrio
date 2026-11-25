// SPDX-FileCopyrightText: Copyright (c) 2018, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2018-2024, Even Rouault, <even.rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 2016, 2017 Thomas Knudsen/SDFE
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   PROJ 9.8.1:
//     src/iso19111/metadata.cpp
//     src/iso19111/operation/coordinateoperationfactory.cpp
//     src/iso19111/factory.cpp
//     src/iso19111/operation/oputils.cpp
//     src/coord_operation.cpp
//     src/trans.cpp

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

/// An operation used for a point, as [`crate::Transformer::operation_at`] reports it.
#[derive(Debug, Clone)]
pub(crate) struct Used {
    pub name: String,
    pub accuracy: f64,
    pub grids: Vec<String>,
    pub ballpark: bool,
}

thread_local! {
    /// The operations used on this thread while [`recording`] runs.
    static USED: std::cell::RefCell<Option<Vec<Used>>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f`, returning its result and the operations routes and geoid models used meanwhile.
pub(crate) fn recording<T>(f: impl FnOnce() -> T) -> (T, Vec<Used>) {
    struct Restore(Option<Vec<Used>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            USED.with(|used| *used.borrow_mut() = self.0.take());
        }
    }
    // The previous capture is restored on return and on a caught unwind.
    let _restore = Restore(USED.with(|used| used.replace(Some(Vec::new()))));
    let out = f();
    let used = USED.with(|used| used.take()).unwrap_or_default();
    (out, used)
}

/// Whether an operation changes the datum or heights, for its name when it has none.
#[derive(Clone, Copy)]
pub(crate) enum OpKind {
    Datum,
    Vertical,
}

/// Records an operation, including registered no-step datum transformations and geoid models.
/// `None` or an unregistered no-step fallback records PROJ's ballpark.
pub(crate) fn note(op: Option<&Op>, reverse: bool, kind: OpKind) {
    USED.with(|u| {
        let mut u = u.borrow_mut();
        let Some(used) = u.as_mut() else { return };
        used.push(match op {
            // A named transformation without steps is EPSG's null transformation (SWEREF99 to
            // WGS 84 (1), ETRS89 to WGS 84 (1)), or a concatenation of them (Inverse of ETRS89 to
            // ETRF89 + ETRS89 to WGS 84 (1)), which PROJ 9.8.1 names: not a ballpark. So is a
            // WKT 2 `BOUNDCRS`'s zero Helmert transformation, which PROJ names too.
            // Registered no-step geoid models have a declared accuracy; the missing-grid
            // placeholder does not. Use the same distinction as the model runner.
            Some(op)
                if !op.ballpark
                    && match kind {
                        OpKind::Datum => !op.steps.is_empty() || !op.name.is_empty(),
                        OpKind::Vertical => crate::is_geoid_model(op),
                    } =>
            {
                let names = step_names(op, reverse);
                let name = if names.is_empty() {
                    let unnamed = match kind {
                        OpKind::Datum => "transformation to WGS 84",
                        OpKind::Vertical => "geoid model",
                    };
                    if reverse { format!("Inverse of {unnamed}") } else { format!("{}{}", unnamed[..1].to_uppercase(), &unnamed[1..]) }
                } else {
                    names.join(" + ")
                };
                // One PROJ frames with its ballpark has a ballpark step.
                Used { name, accuracy: op.accuracy, grids: op.grids().into_iter().map(|(g, _)| g).collect(), ballpark: op.framed }
            }
            _ => {
                let name = match kind {
                    OpKind::Datum => "Ballpark geographic offset",
                    OpKind::Vertical => "Ballpark vertical transformation",
                };
                Used { name: name.into(), accuracy: -1.0, grids: Vec::new(), ballpark: true }
            }
        });
    });
}

/// A transformation and the direction it runs in.
#[derive(Debug, Clone)]
pub(crate) struct Leg {
    pub op: Arc<Op>,
    pub reverse: bool,
}

/// Transformations run one after the other (two through an intermediate datum, or one alone
/// among them for a CRS PROJ does not resolve), with the area, accuracy and flags PROJ ranks the
/// chain by.
#[derive(Debug)]
struct Chain {
    legs: Vec<Leg>,
    rank: Op,
}

impl Chain {
    /// Runs a chain, keeping height across its WGS 84 leg and an adjoining custom shift.
    fn run(&self, p: Point, three_d: bool) -> Option<Point> {
        run_chain(self.legs.iter().map(|l| (l.op.as_ref(), l.reverse)), p, three_d)
    }
}

/// Runs raw legs, preserving height across the WGS 84 leg and the adjoining custom operations.
fn run_chain<'a>(legs: impl Clone + Iterator<Item = (&'a Op, bool)>, p: Point, three_d: bool) -> Option<Point> {
    fn run<'a>(mut legs: impl Iterator<Item = (&'a Op, bool)>, p: Point) -> Option<Point> {
        legs.try_fold(p, |q, (op, reverse)| datum::run(&op.steps, q, reverse, true))
    }
    let Some((i, (marked, reverse))) = legs.clone().enumerate().find(|(_, (op, _))| datum::keeps_wgs84_leg_height(op, three_d)) else {
        return run(legs, p);
    };
    let (rest, wgs84) = marked.steps.split_at(datum::wgs84_leg_boundary(marked));
    let q = run(legs.clone().take(i), p)?;
    // Between 3D CRSs only the WGS 84 leg keeps the height: the legs before and after it move it
    // (PZ-90 to WGS 84 (1) in PZ-90.11's chain). Beside a CRS without heights, the height the
    // chain ends with is the one it started with (reverse) or had before the leg (forward).
    // In 3D, the legs after it by the same rule (another leg kept across).
    let tail = |q: Point| -> Option<Point> {
        if three_d {
            let tail: Vec<(&Op, bool)> = legs.clone().skip(i + 1).collect();
            run_chain(tail.into_iter(), q, true)
        } else {
            run(legs.clone().skip(i + 1), q)
        }
    };
    if reverse {
        let height = if three_d { q.h } else { p.h };
        let q = datum::run(wgs84, q, true, true)?;
        let q = datum::run(rest, Point { h: height, ..q }, true, true)?;
        tail(q)
    } else {
        let q = datum::run(rest, q, false, true)?;
        let height = q.h;
        let q = datum::run(wgs84, q, false, true)?;
        if three_d {
            return tail(Point { h: height, ..q });
        }
        tail(q).map(|q| Point { h: height, ..q })
    }
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
    /// Candidate-area constraints for the hub availability query; point execution keeps its
    /// existing per-point operation selection.
    hub_areas: [Option<[f64; 4]>; 3],
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
    smaller(extent(src), extent(dst))
}

/// A CRS's area as PROJ takes it for its search: none for a WKT 2 `BOUNDCRS` to another CRS,
/// which PROJ neither looks up by name nor gives an area beyond its base CRS's own `USAGE`
/// (`getExtent`), and terrio reads no `USAGE`: the area of a code terrio identifies it as is not
/// PROJ's (a bound CRS on Saba to ED50, placed by ED50's ballpark on the island as in PROJ).
fn extent(d: &Def) -> Option<[f64; 4]> {
    if d.bound_elsewhere() { None } else { d.area() }
}

/// The smaller of two areas, as PROJ measures them.
fn smaller(a: Option<[f64; 4]>, b: Option<[f64; 4]>) -> Option<[f64; 4]> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if pseudo_area(&a) < pseudo_area(&b) { a } else { b }),
        (a, b) => a.or(b),
    }
}

/// The transformations of a WKT 2 `BOUNDCRS` to another CRS (see [`Def::bound_elsewhere`]) as
/// PROJ's nested search from that CRS gives them: when only its ballpark is left for the area of
/// interest, PROJ excludes it ("artificial transformations from the hub", in
/// `createOperationsBoundToGeog`) and applies no shift: between a bound CRS to CH1903 and UTM zone
/// 33N, whose area CH1903's transformation does not reach.
fn nested(def: &Def, kind: Kind) -> Kind {
    match kind {
        Kind::List(legs) if def.bound_elsewhere() && legs.iter().all(|l| l.op.ballpark) => Kind::List(Vec::new()),
        kind => kind,
    }
}

/// Whether PROJ considers a transformation between two CRSs: its area must intersect both CRSs'
/// areas (as PROJ queries its database) and the area of interest.
fn relevant(op: &Op, src: &Def, dst: &Def, aoi: Option<&[f64; 4]>) -> bool {
    op.ballpark || [extent(src).as_ref(), extent(dst).as_ref(), aoi].into_iter().flatten().all(|a| intersects(&op.bounds, a))
}

/// What a route between two CRSs is searched in (#1058 step 7c): the CRSs, PROJ's area of
/// interest between them ([`area_of_interest`]), and whether every grid counts as present, as in
/// PROJ's candidate list, which keeps the transformations whose grids are missing
/// ([`Route::all`]), or only the grids found ([`Route::new`]).
struct OperationContext<'a> {
    src: &'a Def,
    dst: &'a Def,
    aoi: Option<[f64; 4]>,
    all: bool,
    /// Whether the transformations PROJ makes between the CRSs are framed by a ballpark, of
    /// unknown accuracy: into the CRSs of a renamed datum, from either CRS (see
    /// [`Def::unresolved`]), or chained with PROJ's ballpark vertical transformation
    /// (`ballpark_heights`).
    framed: bool,
    /// Whether every transformation is chained with PROJ's ballpark vertical transformation,
    /// which makes it of unknown accuracy (see [`Route::with_ballpark_heights`]).
    ballpark_heights: bool,
}

impl<'a> OperationContext<'a> {
    fn new(src: &'a Def, dst: &'a Def, all: bool) -> Self {
        let framed = [src, dst].iter().any(|d| d.unresolved().is_some_and(|u| u.ballpark));
        Self { src, dst, aoi: area_of_interest(src, dst), all, framed, ballpark_heights: false }
    }

    /// A transformation as PROJ frames it between the CRSs: of unknown accuracy, when framed by a
    /// ballpark.
    fn framed(&self, op: Op) -> Op {
        if self.framed { Op { accuracy: -1.0, chain_accuracy: -1.0, framed: true, ..op } } else { op }
    }

    fn aoi(&self) -> Option<&[f64; 4]> {
        self.aoi.as_ref()
    }

    /// Whether PROJ considers a transformation between the CRSs ([`relevant`]).
    fn keep(&self, op: &Op) -> bool {
        relevant(op, self.src, self.dst, self.aoi())
    }

    /// The direct transformations between two EPSG geographic CRSs from GDAL's table, with those
    /// whose grids are missing when every grid counts, and whether they run in reverse.
    fn pair(&self, a: u32, b: u32) -> Option<(Vec<Arc<Op>>, bool)> {
        if self.all { epsg::pair_all(a, b) } else { epsg::pair(a, b) }
    }
}

impl Route {
    /// The route from `src` to `dst`.
    pub fn new(src: &Def, dst: &Def) -> Self {
        Self::build(&OperationContext::new(src, dst, false))
    }

    /// The route from `src` to `dst` when heights go through PROJ's ballpark vertical
    /// transformation (no geoid model or transformation between the vertical CRSs can be used):
    /// PROJ chains it with each horizontal transformation, which makes every one of unknown
    /// accuracy, so that the one of the smallest area holding a point is used for it. From
    /// WGS 84 (3D) into OSGB36 / British National Grid + ODN height (EPSG:7405) without OSGM15's
    /// grid, PROJ 9.9.0 places (-2°, 54°) by OSGB36 to WGS 84 (3), not (6) (#1012 item 47).
    pub fn with_ballpark_heights(src: &Def, dst: &Def) -> Self {
        let ctx = OperationContext { framed: true, ballpark_heights: true, ..OperationContext::new(src, dst, false) };
        Self::build(&ctx)
    }

    /// The route assuming all grids are available.
    pub fn all(src: &Def, dst: &Def) -> Self {
        let (src, dst) = (src.with_all_operations(), dst.with_all_operations());
        Self::build(&OperationContext::new(&src, &dst, true))
    }

    /// Every candidate, including missing grids, with the same ballpark-height framing as
    /// [`Self::with_ballpark_heights`]. Strict selection must retain that ordering.
    pub fn all_with_ballpark_heights(src: &Def, dst: &Def) -> Self {
        let (src, dst) = (src.with_all_operations(), dst.with_all_operations());
        let ctx = OperationContext { framed: true, ballpark_heights: true, ..OperationContext::new(&src, &dst, true) };
        Self::build(&ctx)
    }

    fn build(ctx: &OperationContext) -> Self {
        let (src, dst, aoi) = (ctx.src, ctx.dst, ctx.aoi());
        let keep = |op: &Op| ctx.keep(op);
        // Unless both CRSs are 3D (geographic 3D or geocentric), PROJ makes a concatenation
        // through a transformation its database registers between geocentric CRSs only where no
        // transformation of the area links the datums directly (PROJ filters the direct ones by
        // area before it tries the pivot): from 2D WGS 84, PROJ 9.9.0 places the deprecated
        // GDA94 (3D) (EPSG:4347) by GDA94 to WGS 84 (1), from 3D WGS 84 through WGS 84 (G1762);
        // ITRF90 (geocentric) (EPSG:4332), which none links directly, through WGS 84 (Transit)
        // from both.
        let dimensions = src.geodetic_3d() && dst.geodetic_3d();
        let pivoted = |op: &Op| op.geocentric && op.concatenated;
        // Between projected CRSs, PROJ concatenates the transformations with the projections,
        // which gives a concatenation its database registers the accuracy and area of its
        // steps (NAD83 to NAD83(NSRS2007) (NADCON5, Alaska) between British Columbia Albers
        // and UTM).
        let concatenated = !src.is_geographic() || !dst.is_geographic();
        let list = |ops: &[Arc<Op>], reverse: bool| -> Kind {
            let direct = ops.iter().any(|op| !op.concatenated && !op.ballpark && keep(op));
            Kind::List(
                legs(ops, reverse)
                    .into_iter()
                    .filter(|l| keep(&l.op) && (dimensions || !direct || !pivoted(&l.op)))
                    .map(|l| {
                        let flat = concatenated && (l.op.chain_accuracy != l.op.accuracy || l.op.chain_bounds != l.op.bounds);
                        if flat { Leg { op: Arc::new(l.op.as_concatenated()), reverse: l.reverse } } else { l }
                    })
                    .collect(),
            )
        };
        // A CRS PROJ does not resolve to its EPSG CRS: its search beyond the CRSs' own.
        let searching = src.unresolved().is_some() || dst.unresolved().is_some();
        let searched = |kind: Kind| match (kind, src.geog(), dst.geog()) {
            (Kind::List(l), Some(a), Some(b)) if searching => unresolved_search(l, a, b, ctx),
            (kind, ..) => kind,
        };
        let mut kind = if src.on_bound_source(dst) || dst.on_bound_source(src) {
            // A WKT 2 `BOUNDCRS` into a CRS on its source CRS's datum: PROJ's null
            // transformation.
            Kind::Same
        } else if let Some(op) = src.bound_to(dst) {
            // A WKT 2 `BOUNDCRS` to a CRS on the other's datum: its transformation alone.
            Kind::List(vec![Leg { op: op.clone(), reverse: false }])
        } else if let Some(op) = dst.bound_to(src) {
            Kind::List(vec![Leg { op: op.clone(), reverse: true }])
        } else if src.same_datum(dst) {
            Kind::Same
        } else if dst.is_wgs84() {
            searched(nested(src, list(src.ops(), false)))
        } else if src.is_wgs84() {
            searched(nested(dst, list(dst.ops(), true)))
        } else {
            match (src.geog(), dst.geog()) {
                (Some(a), Some(b)) => match ctx.pair(a, b) {
                    // PROJ looks for transformations through other datums when its database has
                    // none it can use between the two for the CRSs' areas (NAD83(CSRS) to NAD83
                    // between Arctic CRSs, outside the offshore area of their only one, or with
                    // the grids missing).
                    Some((ops, reverse)) if ops.iter().any(|o| !o.ballpark && keep(o)) => {
                        let mut kind = list(&ops, reverse);
                        // GDAL's order is for the two geographic CRSs: between CRSs of another
                        // area of interest, PROJ sorts the transformations for theirs.
                        // The search sorts the list itself, for a CRS PROJ does not resolve.
                        if !searching {
                            resort(&mut kind, a, b, aoi);
                        }
                        searched(kind)
                    }
                    _ => chains(a, b, ctx),
                },
                _ => shifted(ctx).unwrap_or(Kind::Hub),
            }
        };
        if ctx.ballpark_heights
            && let Kind::List(list) = &mut kind
        {
            for leg in list.iter_mut() {
                leg.op = Arc::new(ctx.framed((*leg.op).clone()));
            }
            sort_legs(&mut kind, aoi);
        }
        Self {
            kind,
            boxes: OnceLock::new(),
            hub_boxes: OnceLock::new(),
            dst_ops: dst.ops().to_vec(),
            src_ops: src.ops().to_vec(),
            hub_areas: [extent(src), extent(dst), ctx.aoi],
        }
    }

    /// Refuse ballpark alternatives while retaining usable accurate operations.
    pub fn disallow_ballpark(&mut self) -> bool {
        match &mut self.kind {
            Kind::Same => true,
            Kind::List(list) => {
                list.retain(|l| !l.op.has_ballpark());
                !list.is_empty()
            }
            Kind::Chains { chains, ballpark } => {
                *ballpark = None;
                chains.retain(|c| !c.rank.has_ballpark());
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
        // Longitudes are tried wrapped into [-180, 180[ here, 180 also as -180.
        let at = At::new(src, x, y, p, true);
        let choose = |c: Candidates<'a>| {
            let i = c.best(&at, &[]).or_else(|| c.first_without_grids());
            let op = i.map(|i| c.ops[i]);
            (op, i.is_some_and(|i| !c.ops[i].ballpark && !c.inside(&at, i)))
        };
        match &self.kind {
            Kind::Same => (None, false),
            Kind::Hub => match self.hub_pick(src, &at) {
                (Some(s), Some(d)) => (Some(if !s.available() { s } else { d }), false),
                _ => (None, false),
            },
            // A list's ballpark holds every point here, as a chain's does.
            _ => choose(self.candidates(src, true).expect("a list or chains")),
        }
    }

    /// Evaluate exactly the chosen operation, without a fallback after a pipeline failure.
    pub fn apply_selected(&self, src: &Def, x: f64, y: f64, p: Point, three_d: bool, op: Option<&Op>) -> Option<Point> {
        let Some(op) = op else { return self.apply(src, x, y, p, three_d) };
        match &self.kind {
            Kind::List(list) => {
                let leg = list.iter().find(|l| std::ptr::eq(l.op.as_ref(), op))?;
                datum::run_op(op, p, leg.reverse, three_d).inspect(|_| note(Some(op), leg.reverse, OpKind::Datum))
            }
            Kind::Chains { chains, .. } => {
                // A shift and a ballpark is a chain (see `shifted`); the ballpark alone, no shift.
                let Some(c) = chains.iter().find(|c| std::ptr::eq(&c.rank, op)) else {
                    if !op.ballpark {
                        return None;
                    }
                    note(None, false, OpKind::Datum);
                    return Some(p);
                };
                let q = c.run(p, three_d)?;
                note(Some(&c.rank), false, OpKind::Datum);
                Some(if three_d || !c.rank.keeps_height { q } else { Point { h: p.h, ..q } })
            }
            _ => self.apply(src, x, y, p, three_d),
        }
    }

    /// Whether any retained candidate is not a ballpark: a transformation between the
    /// datums whose grids can be had (see [`Op::obtainable`]), or none needed (one datum). False
    /// when only PROJ's ballpark links the CRSs (or the shift into a hub CRS with it, which PROJ
    /// counts as one). Candidates have already been filtered by the CRSs' extents and area of
    /// interest; this does not select a particular point within those areas.
    pub fn has_transformation(&self) -> bool {
        let usable = |op: &Op| !op.has_ballpark() && op.obtainable();
        match &self.kind {
            Kind::Same => true,
            Kind::List(list) => list.iter().any(|l| usable(&l.op)),
            Kind::Chains { chains, .. } => chains.iter().any(|c| !c.rank.framed && c.legs.iter().all(|l| usable(&l.op))),
            // Through WGS 84: a transformation on each side.
            Kind::Hub => [&self.src_ops, &self.dst_ops]
                .iter()
                .all(|ops| ops.iter().any(|op| usable(op) && self.hub_areas.iter().flatten().all(|area| intersects(&op.bounds, area)))),
        }
    }

    /// Whether this route represents an unchanged datum.
    pub fn same_datum(&self) -> bool {
        matches!(self.kind, Kind::Same)
    }

    /// The candidates of a list, or the chains then their ballpark, with their areas in source
    /// coordinates; `ballpark_holds`: whether a ballpark holds every point. `None` for the other
    /// kinds.
    fn candidates<'a>(&'a self, src: &Def, ballpark_holds: bool) -> Option<Candidates<'a>> {
        let (ops, boxes) = match &self.kind {
            Kind::List(list) => {
                (list.iter().map(|l| l.op.as_ref()).collect(), self.boxes.get_or_init(|| list.iter().map(|l| src.areas(&l.op)).collect()))
            }
            Kind::Chains { chains, ballpark } => (
                chains.iter().map(|c| &c.rank).chain(ballpark.as_ref()).collect(),
                self.boxes.get_or_init(|| chains.iter().map(|c| src.areas(&c.rank)).collect()),
            ),
            _ => return None,
        };
        Some(Candidates { ops, boxes, ballpark_holds })
    }

    /// The source's and target's transformations to WGS 84 a `Hub` route picks for a point: the
    /// single ones (no ballpark, no concatenation), as PROJ chains them, whose areas hold it.
    fn hub_pick<'a>(&'a self, src: &Def, at: &At) -> (Option<&'a Op>, Option<&'a Op>) {
        let (sb, db) = self.hub_boxes.get_or_init(|| {
            let areas = |ops: &[Arc<Op>]| ops.iter().map(|o| src.areas(o)).collect();
            (areas(&self.src_ops), areas(&self.dst_ops))
        });
        let single = |op: &Op| !op.ballpark && !op.concatenated;
        let pick = |ops: &'a [Arc<Op>], boxes: &Boxes| datum::pick_in(ops, |op, i| single(op) && at.inside(op, boxes.get(i)));
        (pick(&self.src_ops, sb), pick(&self.dst_ops, db))
    }

    /// The accuracy (metres) of the transformation [`Route::apply`] picks for a point, as PROJ
    /// adds it to the accuracy of a vertical transformation it chains with: 0 between CRSs of one
    /// datum, negative when unknown (a ballpark, or no transformation for the point).
    pub fn accuracy_at(&self, src: &Def, x: f64, y: f64, p: Point) -> f64 {
        let at = At::new(src, x, y, p, false);
        match &self.kind {
            Kind::Same => 0.0,
            Kind::List(list) if list.is_empty() => -1.0,
            Kind::Chains { chains, .. } if chains.is_empty() => -1.0,
            Kind::Chains { chains, ballpark: None } if chains.len() == 1 => chains[0].rank.accuracy,
            Kind::Hub => -1.0,
            _ => {
                let c = self.candidates(src, matches!(self.kind, Kind::Chains { .. })).expect("a list or chains");
                c.best(&at, &[]).or_else(|| c.first_without_grids()).map_or(-1.0, |i| c.ops[i].accuracy)
            }
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
        let report = |result: Option<Point>, op: Option<&Op>, reverse: bool, outside: bool| {
            result.inspect(|_| {
                note(op, reverse, OpKind::Datum);
                record(op, outside)
            })
        };
        let at = At::new(src, x, y, p, false);
        match &self.kind {
            Kind::Same => Some(p),
            // No transformation at all (a CRS of an unknown datum, defined by its ellipsoid alone):
            // no shift, PROJ's ballpark transformation.
            Kind::List(list) if list.is_empty() => report(Some(p), None, false, false),
            Kind::List(list) => {
                let c = self.candidates(src, false).expect("a list");
                datum::transform_with(
                    &c.ops,
                    |_, i| c.inside(&at, i),
                    |i| {
                        report(
                            datum::run_op(&list[i].op, p, list[i].reverse, three_d),
                            Some(&list[i].op),
                            list[i].reverse,
                            !c.inside(&at, i),
                        )
                    },
                )
            }
            // PROJ's ballpark alone: no shift.
            Kind::Chains { chains, .. } if chains.is_empty() => report(Some(p), None, false, false),
            Kind::Chains { chains, ballpark } => {
                // Heights go through both transformations, as in PROJ's pipeline for the chain (the
                // conversion to geodetic coordinates and back between them cancelling out), and
                // are restored after them when the chain keeps them.
                let run = |c: &Chain| {
                    let r = c.run(p, three_d)?;
                    Some(if three_d || !c.rank.keeps_height { r } else { Point { h: p.h, ..r } })
                };
                let c = self.candidates(src, true).expect("chains");
                // A single operation is used for every point, whatever its area, as PROJ does.
                if chains.len() == 1 && ballpark.is_none() {
                    return report(run(&chains[0]), Some(&chains[0].rank), false, !c.inside(&at, 0));
                }
                for i in c.tries(&at) {
                    // The ballpark (more accurate than the chains between CRSs that differ by
                    // their prime meridians alone): no shift.
                    let Some(chain) = chains.get(i) else { return report(Some(p), None, false, false) };
                    if let Some(r) = run(chain) {
                        return report(Some(r), Some(&chain.rank), false, !c.inside(&at, i));
                    }
                    // A grid the network did not give: PROJ stops on the network error.
                    if chain.legs.iter().any(|l| l.op.network_failed()) {
                        return None;
                    }
                }
                if ballpark.is_some() {
                    // The ballpark holds every point: no shift.
                    return report(Some(p), None, false, false);
                }
                // No transformation for the point: the first that needs no grid, as PROJ's
                // `proj_trans`.
                let i = c.first_without_grids()?;
                report(run(&chains[i]), Some(&chains[i].rank), false, !c.inside(&at, i))
            }
            Kind::Hub => match self.hub_pick(src, &at) {
                (Some(s), Some(d)) => {
                    // Heights go through both, as in PROJ's pipeline (see the chains).
                    let r = run_chain([(s, false), (d, true)].into_iter(), p, three_d)?;
                    let result = Some(if three_d || !(s.keeps_height && d.keeps_height) { r } else { Point { h: p.h, ..r } });
                    if result.is_some() {
                        note(Some(s), false, OpKind::Datum);
                    }
                    record(Some(s), false);
                    report(result, Some(d), true, false)
                }
                // No transformation (or an unknown datum): no shift, as PROJ's ballpark.
                _ => report(Some(p), None, false, false),
            },
        }
    }
}

impl Route {
    /// The transformations whose grids a point needs, as [`Route::apply_reporting`] tries them:
    /// the best one for the point, and while it does not apply (the point is outside its grid),
    /// the next best, up to three. The first whose grid is missing, else the one that applies.
    /// With the grids found that way present, the point goes through the transformation PROJ
    /// picks with every grid present (asking again after each grid is added). Empty when the
    /// transformation the point goes through needs no grid.
    pub fn grid_operations_at<'a>(&'a self, src: &Def, x: f64, y: f64, p: Point) -> Vec<&'a Op> {
        let at = At::new(src, x, y, p, false);
        match &self.kind {
            Kind::Same => Vec::new(),
            Kind::List(list) => {
                let c = self.candidates(src, false).expect("a list");
                for i in c.tries(&at) {
                    let op = c.ops[i];
                    // A grid the network did not give, before or by this run, stops the point there, as
                    // in `apply_reporting`.
                    if !op.available() || datum::run_op(op, p, list[i].reverse, false).is_some() || op.network_failed() {
                        return vec![op];
                    }
                }
                // Then the point goes through the first transformation that needs no grid, so no
                // grid is returned.
                Vec::new()
            }
            Kind::Chains { chains, ballpark } => {
                let legs = |c: &'a Chain| c.legs.iter().map(|l| l.op.as_ref()).collect::<Vec<_>>();
                if chains.len() == 1 && ballpark.is_none() {
                    return legs(&chains[0]);
                }
                let c = self.candidates(src, true).expect("chains");
                for i in c.tries(&at) {
                    let Some(chain) = chains.get(i) else { return Vec::new() };
                    if chain.legs.iter().any(|l| !l.op.available())
                        || chain.run(p, false).is_some()
                        || chain.legs.iter().any(|l| l.op.network_failed())
                    {
                        return legs(chain);
                    }
                }
                Vec::new()
            }
            // Both, or the ballpark (no shift).
            Kind::Hub => match self.hub_pick(src, &at) {
                (Some(s), Some(d)) => vec![s, d],
                _ => Vec::new(),
            },
        }
    }
}

/// A point as PROJ compares it with the areas of the operations: its source coordinates, against
/// an operation's areas in source coordinates when it has them, else its longitude and latitude
/// in degrees (for a geographic source as given), the longitude also wrapped into [-180, 180].
struct At {
    x: f64,
    y: f64,
    lon: f64,
    lat: f64,
    wrapped: f64,
}

impl At {
    /// `wrap_all`: wrap every longitude (180 is also tried as -180), else only those out of
    /// [-180, 180].
    fn new(src: &Def, x: f64, y: f64, p: Point, wrap_all: bool) -> Self {
        let (lon, lat) = src.source_degrees(x, y).unwrap_or((p.lon.to_degrees(), p.lat.to_degrees()));
        let wrap = |lon: f64| (lon + 180.0).rem_euclid(360.0) - 180.0;
        let wrapped = if wrap_all || !(-180.0..=180.0).contains(&lon) { wrap(lon) } else { lon };
        Self { x, y, lon, lat, wrapped }
    }

    /// Whether an operation's area holds the point.
    fn inside(&self, op: &Op, boxes: Option<&Vec<[f64; 4]>>) -> bool {
        match boxes {
            Some(b) if !b.is_empty() => b.iter().any(|b| self.x >= b[0] && self.x <= b[2] && self.y >= b[1] && self.y <= b[3]),
            _ => op.contains(self.lon, self.lat) || op.contains(self.wrapped, self.lat),
        }
    }
}

/// The operations a route chooses from for a point, in PROJ's order, with their areas in source
/// coordinates: a list's, or the chains' then their ballpark.
struct Candidates<'a> {
    ops: Vec<&'a Op>,
    boxes: &'a Boxes,
    /// Whether a ballpark holds every point, whatever its area.
    ballpark_holds: bool,
}

impl Candidates<'_> {
    /// Whether the `i`th operation's area holds the point (not counting a ballpark's).
    fn inside(&self, at: &At, i: usize) -> bool {
        at.inside(self.ops[i], self.boxes.get(i))
    }

    /// Whether the `i`th operation holds the point.
    fn holds(&self, at: &At, i: usize) -> bool {
        (self.ballpark_holds && self.ops[i].ballpark) || self.inside(at, i)
    }

    /// The operation PROJ picks for the point among those that hold it, but `excluded`: the most
    /// accurate, then the one of the smallest area.
    fn best(&self, at: &At, excluded: &[usize]) -> Option<usize> {
        datum::pick_index(&self.ops, excluded, |_, i| self.holds(at, i))
    }

    /// The first operation that needs no grid, which PROJ's `proj_trans` uses when none holds
    /// the point.
    fn first_without_grids(&self) -> Option<usize> {
        self.ops.iter().position(|op| !op.needs_grids())
    }

    /// The operations PROJ's `proj_trans` tries for the point, in turn while each fails: the
    /// best, then the next best, up to three.
    fn tries<'b>(&'b self, at: &'b At) -> impl Iterator<Item = usize> + 'b {
        let mut excluded = Vec::new();
        std::iter::from_fn(move || {
            if excluded.len() == 3 {
                return None;
            }
            let i = self.best(at, &excluded)?;
            excluded.push(i);
            Some(i)
        })
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
fn chains(a: u32, b: u32, ctx: &OperationContext) -> Kind {
    let (src, dst, all) = (ctx.src, ctx.dst, ctx.all);
    let reg = epsg::registry();
    let (Some(ga), Some(gb)) = (reg.geodetic.get(&a), reg.geodetic.get(&b)) else {
        return Kind::Chains { chains: Vec::new(), ballpark: Some(ballpark_op(src, dst)) };
    };
    let extents = crs_extents(src, dst);
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
    finish(ranked(reg, found, ctx), ctx)
}

/// The areas of two CRSs PROJ queries its database's transformations for: those it knows, but
/// the whole world.
fn crs_extents(src: &Def, dst: &Def) -> Vec<[f64; 4]> {
    [src.area(), dst.area()].into_iter().flatten().filter(|e| *e != [-180.0, -90.0, 180.0, 90.0]).collect()
}

/// The transformations PROJ lists between two CRSs when it does not resolve one of them to its
/// EPSG CRS (see [`Def::unresolved`]): `list`, the CRSs' own (which its datum pivot finds through
/// the CRSs of the datum), then, as their direct lookup in its database found nothing, also the
/// chains through an intermediate datum when those cover at most half the CRSs' common area (with
/// no area of a CRS PROJ finds by its name, that is the other CRS's), or go only through PROJ's
/// null transformations between the WGS 84 ensemble and its realizations, or are only
/// time-dependent between static CRSs; all sorted together as PROJ sorts them, for its area of
/// interest. Without those chains too, PROJ sorts the CRSs' own for its area of interest, which
/// is not the codes' when it has no area for the CRS (or another's): GDAL's list for the codes
/// is sorted again ([`resort`]). With none of the CRSs' own, PROJ's search through intermediate
/// CRSs and datums ([`chains`]).
///
/// PROJ's datum pivot tries the CRSs of the datum in order of code, the geocentric and 3D ones
/// too, and frames each transformation with the operation from the CRS into that CRS: so, for an
/// unresolved source CRS, and into an unresolved target CRS PROJ cannot find by its name, the
/// static transformations registered from the datum's other CRSs to the other datum join the
/// list, filtered and sorted as the CRSs' own. The framing is PROJ's ballpark, of unknown
/// accuracy, when the datum's name is not its ID's ([`crate::def::Unresolved::ballpark`]): then every
/// transformation PROJ makes of several (the chains, and those framed with conversions into
/// geocentric CRSs) is of unknown accuracy, and the registered ones of the CRS itself keep theirs
/// (PROJ leaves a ballpark alone out of the chain). A WKT with an ID of an authority PROJ does
/// not know (`AUTHORITY["LOCAL","1"]`) is taken for resolved, as PROJ's search through
/// intermediate datums finds nothing for it; from it, PROJ's datum pivot also takes only the
/// first CRS with transformations, which terrio does not follow yet (opengeos/terrio#1012).
fn unresolved_search(list: Vec<Leg>, a: u32, b: u32, ctx: &OperationContext) -> Kind {
    let (src, dst, aoi, all) = (ctx.src, ctx.dst, ctx.aoi(), ctx.all);
    let reg = epsg::registry();
    if !list.iter().any(|l| !l.op.ballpark) {
        return chains(a, b, ctx);
    }
    // Those PROJ makes of several (a chain, or one framed with conversions into geocentric CRSs),
    // framed by a ballpark: of unknown accuracy. The registered ones keep theirs.
    let list: Vec<Leg> = list
        .into_iter()
        .map(|l| if l.op.concatenated || l.op.geocentric { Leg { op: Arc::new(ctx.framed((*l.op).clone())), ..l } } else { l })
        .collect();
    // CRSs PROJ's registry of chainable CRSs lacks: their own list, sorted as when no search
    // follows (every path sorts once; the CRS-pair branch leaves the sorting to this function).
    let (Some(ga), Some(gb)) = (reg.geodetic.get(&a), reg.geodetic.get(&b)) else {
        let mut kind = Kind::List(list);
        // Framed, the accuracies are not GDAL's: sorted again.
        if ctx.framed {
            sort_legs(&mut kind, aoi)
        } else {
            resort(&mut kind, a, b, aoi)
        }
        return kind;
    };
    // PROJ's datum pivot also tries the other CRSs of an unresolved CRS's datum (its geocentric
    // and 3D ones), with the conversion into each: their transformations to the other datum join
    // the list (ONGD14's Esri WKT, without ID, takes "ONGD14 to WGS 84 (1)", registered between
    // the geocentric CRSs, at 0.1 m, over the geographic 2D CRS's null transformation).
    let mut list = list;
    let mut pivot: Vec<(usize, bool)> = Vec::new();
    // The registered transformations from a CRS `code` of `side`'s datum to the other's datum,
    // run from the source CRS to the target: as the CRSs' own, static, between the datums,
    // available, and relevant to the CRSs' areas and the area of interest.
    let from = |code: u32, other: u32, source_side: bool| -> Vec<(usize, bool)> {
        let mut found = Vec::new();
        for &i in reg.at.get(&code).into_iter().flatten() {
            let r = &reg.ops[i];
            let (end, from_code) = if r.source == code { (r.target, true) } else { (r.source, false) };
            if r.time_dependent || r.ensemble || !reg.geodetic.get(&end).is_some_and(|g| g.datum == other) {
                continue;
            }
            if (!all && !r.op.available()) || !ctx.keep(&r.op) {
                continue;
            }
            // Forward when the registered operation leaves the source CRS's datum.
            found.push((i, if source_side { !from_code } else { from_code }));
        }
        found
    };
    // The datum pivot from one side: through the other CRSs of its datum, in order of code. One
    // named as the WKT's code is tried first, alone: PROJ's datum pivot takes those named as the
    // WKT first and stops at the first with transformations, its own CRS's too (so ESRI's
    // `ONGD14` takes only "ONGD14 to WGS 84 (1)", between the geocentric CRSs, and not "(2)",
    // between the geographic 2D ones). Its own CRS's are the list's.
    let mut exclusive = false;
    let mut side = |g: &epsg::Geodetic, other: u32, same_name: bool, source_side: bool| {
        let codes = reg.by_datum.get(&g.datum).into_iter().flatten().copied();
        if same_name {
            let named = |c: &u32| reg.geodetic.get(c).is_some_and(|n| n.name == g.name);
            for code in codes.clone().filter(named) {
                // The CRS itself first in code order, with registered transformations of its own:
                // those alone, the list as it is.
                if code == g.code {
                    if list.iter().any(|l: &Leg| !l.op.ballpark && !l.op.concatenated && !l.op.geocentric) {
                        return;
                    }
                    continue;
                }
                // Another CRS of the name with transformations: its alone, without the list's.
                let found = from(code, other, source_side);
                if !found.is_empty() {
                    pivot.extend(found);
                    exclusive = true;
                    return;
                }
            }
            // None of the name has any: PROJ goes on through all the datum's CRSs.
        }
        // Under another name, every other CRS of the datum, with the list.
        for code in codes.filter(|c| *c != g.code) {
            pivot.extend(from(code, other, source_side));
        }
    };
    // From an unresolved source CRS; into an unresolved target CRS PROJ cannot find by its name
    // (into one it finds, it goes by the code's own transformations).
    if let Some(u) = src.unresolved() {
        side(ga, gb.datum, u.same_name, true);
    }
    if let Some(u) = dst.unresolved().filter(|u| !u.named) {
        side(gb, ga.datum, u.same_name, false);
    }
    // Another CRS's transformations alone: the list holds the CRS's own only so far (the pivot's
    // join it below), so all of them go but the ballpark.
    if exclusive {
        list.retain(|l| l.op.ballpark);
    }
    // Without those another of them replaces, and those the list has (by name: a list between
    // projected CRSs holds them flattened).
    let gone = superseded(reg, pivot.iter().map(|p| p.0), false);
    let concatenated = !src.is_geographic() || !dst.is_geographic();
    let mut pivoted = false;
    for (i, reverse) in pivot {
        let op = &reg.ops[i].op;
        if gone.contains(&i) || list.iter().any(|l| Arc::ptr_eq(&l.op, op) || (l.op.name == op.name && !op.name.is_empty())) {
            continue;
        }
        let flat = concatenated && (op.chain_accuracy != op.accuracy || op.chain_bounds != op.bounds);
        let op = if flat { op.as_concatenated() } else { (**op).clone() };
        // A pivot uses another CRS's operation, so even a registered geographic 2D operation
        // has a framing step from the unresolved CRS. The code's own registered operations
        // above have no such step and keep their accuracy.
        list.push(Leg { op: Arc::new(ctx.framed(op)), reverse });
        pivoted = true;
    }
    let direct: Vec<&Leg> = list.iter().filter(|l| !l.op.ballpark).collect();
    // Whether every transformation has a step among `names`.
    let all_with = |names: &HashSet<&str>| direct.iter().all(|l| step_names(&l.op, false).iter().any(|n| names.contains(n.as_str())));
    let (ensemble, time_dependent) = flagged_names();
    let areas: Vec<Option<[f64; 4]>> = direct.iter().map(|l| Some(l.op.chain_bounds)).collect();
    let search = too_small_areas(&areas, src.area(), dst.area())
        || all_with(ensemble)
        || (!src.is_dynamic() && !dst.is_dynamic() && all_with(time_dependent));
    if !search {
        let mut kind = Kind::List(list);
        // The transformations the datum pivot adds are sorted in with the CRSs' own.
        if pivoted || ctx.framed {
            sort_legs(&mut kind, aoi);
        } else {
            resort(&mut kind, a, b, aoi);
        }
        return kind;
    }
    let extents = crs_extents(src, dst);
    let reverse = !dst.is_geographic();
    let (ga, gb) = if reverse { (gb, ga) } else { (ga, gb) };
    let mut found = datum_chains(reg, ga, gb, &extents, all);
    if reverse {
        found = found.into_iter().map(Found::reversed).collect();
    }
    let mut out = ranked(reg, found, ctx);
    out.extend(direct.into_iter().map(|l| direct_chain(l, aoi)));
    finish(out, ctx)
}

/// The names of PROJ's null transformations between the WGS 84 ensemble and its realizations, and
/// of its time-dependent transformations, among those of its database.
fn flagged_names() -> &'static (HashSet<&'static str>, HashSet<&'static str>) {
    static NAMES: OnceLock<(HashSet<&'static str>, HashSet<&'static str>)> = OnceLock::new();
    NAMES.get_or_init(|| {
        let reg = epsg::registry();
        let named = |flag: fn(&epsg::Registered) -> bool| reg.ops.iter().filter(|r| flag(r)).map(|r| r.op.name.as_str()).collect();
        (named(|r| r.ensemble), named(|r| r.time_dependent))
    })
}

/// One of the CRSs' own transformations as a chain of one leg, with what PROJ sorts it by: as
/// concatenated with the conversion into the CRS of the datum, as PROJ ranks it then. Run as a
/// chain, it moves heights as [`datum::run_op`] does (kept between 2D CRSs when it keeps them).
fn direct_chain(l: &Leg, aoi: Option<&[f64; 4]>) -> (SortKey, Chain) {
    // Ranked by the op as registered even when run in reverse: its accuracy and area, and its
    // flags, are the same both ways (as in PROJ, whose inverse keeps them); only the name and the
    // pipeline differ.
    let name = step_names(&l.op, l.reverse).join(" + ");
    let rank = Op { name: name.clone(), ..l.op.as_concatenated() };
    let key = SortKey::new(&rank, name, pipeline_steps(&[(&l.op, l.reverse)]), aoi);
    (key, Chain { legs: vec![l.clone()], rank })
}

/// Sorts GDAL's list of transformations between two geographic CRSs again when the area of
/// interest is not theirs, as PROJ sorts them for its own.
fn resort(kind: &mut Kind, a: u32, b: u32, aoi: Option<&[f64; 4]>) {
    let area = |code: u32| epsg::get(code).and_then(|e| e.area);
    if aoi.copied() != smaller(area(a), area(b)) {
        sort_legs(kind, aoi);
    }
}

/// Sorts a list of transformations as PROJ sorts them for an area of interest.
fn sort_legs(kind: &mut Kind, aoi: Option<&[f64; 4]>) {
    if let Kind::List(l) = kind {
        // Run in reverse, a transformation has the inverse's name.
        let key = |leg: &Leg| {
            let name = step_names(&leg.op, leg.reverse).join(" + ");
            SortKey::new(&leg.op, name, pipeline_steps(&[(&leg.op, leg.reverse)]), aoi)
        };
        let mut keyed: Vec<(SortKey, Leg)> = l.drain(..).map(|leg| (key(&leg), leg)).collect();
        keyed.sort_by(|x, y| x.0.cmp(&y.0));
        l.extend(keyed.into_iter().map(|(_, leg)| leg));
    }
}

/// The chains found, each with what PROJ sorts it by: without those whose legs' areas do not
/// intersect, or not the area of interest.
fn ranked(reg: &epsg::Registry, found: Vec<Found>, ctx: &OperationContext) -> Vec<(SortKey, Chain)> {
    let aoi = ctx.aoi();
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
            framed: false,
            concatenated: true,
            between_3d: o1.between_3d,
            geocentric: o1.geocentric || o2.geocentric,
            priority: o1.priority || o2.priority || datum::is_priority(&name),
            keeps_height: o1.keeps_height && o2.keeps_height,
            keeps_wgs84_leg_height: false,
            keeps_height_3d: false,
            steps: o1.steps.iter().chain(&o2.steps).cloned().collect(),
            name,
        };
        let rank = ctx.framed(rank);
        let legs = vec![Leg { op: o1.clone(), reverse: r1 }, Leg { op: o2.clone(), reverse: r2 }];
        let key = SortKey::new(&rank, rank.name.clone(), pipeline_steps(&[(o1, r1), (o2, r2)]), aoi);
        out.push((key, Chain { legs, rank }));
    }
    out
}

/// PROJ's list from the chains it found, sorted: then its ballpark, unless a chain that needs no
/// grid holds the area of interest (even one it then drops itself).
fn finish(mut out: Vec<(SortKey, Chain)>, ctx: &OperationContext) -> Kind {
    let aoi = ctx.aoi();
    out.sort_by(|x, y| x.0.cmp(&y.0));
    // PROJ drops its ballpark when a chain that needs no grid holds the area of interest, even
    // one it then drops itself.
    let ballpark = !aoi.is_some_and(|a| out.iter().any(|(_, c)| !c.rank.needs_grids() && contains(&c.rank.bounds, a)));
    let chains = remove_uninteresting(out.into_iter().map(|(_, c)| c).collect());
    Kind::Chains { chains, ballpark: ballpark.then(|| ballpark_op(ctx.src, ctx.dst)) }
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
            o.rank.bounds == c.rank.bounds
                && o.legs.len() == c.legs.len()
                && o.legs.iter().zip(&c.legs).all(|(x, y)| x.reverse == y.reverse && x.op.steps == y.op.steps)
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
    let areas: Vec<Option<[f64; 4]>> = found
        .iter()
        .map(|f| {
            let [(i1, _), (i2, _)] = f.legs;
            intersection(&reg.ops[i1].op.chain_bounds, &reg.ops[i2].op.chain_bounds)
        })
        .collect();
    too_small_areas(&areas, e1, e2)
}

/// Whether every one of fewer than 10 transformations, of these areas (`None` for an empty
/// intersection), covers at most half the CRSs' common area, as PROJ's `hasTooSmallAreas`
/// measures it.
fn too_small_areas(areas: &[Option<[f64; 4]>], e1: Option<[f64; 4]>, e2: Option<[f64; 4]>) -> bool {
    if areas.is_empty() || areas.len() >= 10 {
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
    !areas.iter().any(|op| {
        let Some(op) = *op else { return false };
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

/// The route between a CRS bound to a hub CRS (see [`Def::hub`]) and a CRS with its own
/// transformations, as PROJ makes it (`createOperationsBoundToGeog`): the shift into the hub,
/// then each of PROJ's transformations from the hub to the other CRS (the reverse the other way),
/// each of unknown accuracy, sorted again for the other CRS's area; then the shift alone (with
/// PROJ's ballpark from the hub), which this search drops only when a transformation that needs
/// no grid holds the area of interest, as PROJ does (a bound CRS to DHDN into ETRS89 keeps it,
/// which the codes' own list drops). With no transformation from the hub but that ballpark, PROJ
/// excludes it ("artificial transformations from the hub"): no shift, which ALLOW_BALLPARK=NO
/// refuses as a ballpark. `None` when neither CRS alone is bound to a hub, or
/// the search from the hub is not one of a list or chains.
fn shifted(ctx: &OperationContext) -> Option<Kind> {
    let (src, dst) = (ctx.src, ctx.dst);
    let (hub, shift, forward) = match (src.hub(), dst.hub()) {
        (Some((hub, shift)), None) if dst.geog().is_some() => (hub, shift, true),
        (None, Some((hub, shift))) if src.geog().is_some() => (hub, shift, false),
        _ => return None,
    };
    let hubbed = if forward { OperationContext::new(&hub, dst, ctx.all) } else { OperationContext::new(src, &hub, ctx.all) };
    let (inner, aoi) = (Route::build(&hubbed), hubbed.aoi);
    let shift = Leg { op: shift, reverse: !forward };
    let mut found: Vec<(Vec<Leg>, Op)> = Vec::new();
    match inner.kind {
        Kind::List(list) => {
            for l in list.iter().filter(|l| !l.op.ballpark) {
                let (_, c) = direct_chain(l, aoi.as_ref());
                found.push((c.legs, c.rank));
            }
        }
        Kind::Chains { chains, .. } => found.extend(chains.into_iter().map(|c| (c.legs, c.rank))),
        _ => return None,
    }
    if found.is_empty() {
        return Some(Kind::List(Vec::new()));
    }
    let ballpark = !aoi.is_some_and(|a| found.iter().any(|(_, rank)| !rank.needs_grids() && contains(&rank.bounds, &a)));
    let around = |legs: Vec<Leg>| -> Vec<Leg> {
        let s = std::iter::once(shift.clone());
        if forward { s.chain(legs).collect() } else { legs.into_iter().chain(s).collect() }
    };
    let unknown = |rank: Op| Op { accuracy: -1.0, chain_accuracy: -1.0, ..rank };
    let mut out: Vec<(SortKey, Chain)> = found
        .into_iter()
        .map(|(legs, rank)| {
            let legs = around(legs);
            let rank = unknown(rank);
            let steps = pipeline_steps(&legs.iter().map(|l| (l.op.as_ref(), l.reverse)).collect::<Vec<_>>());
            (SortKey::new(&rank, rank.name.clone(), steps, aoi.as_ref()), Chain { legs, rank })
        })
        .collect();
    out.sort_by(|x, y| x.0.cmp(&y.0));
    let mut chains: Vec<Chain> = out.into_iter().map(|(_, c)| c).collect();
    if ballpark {
        chains.push(Chain { legs: vec![shift], rank: Op { ballpark: true, ..unknown(Op::world(Vec::new())) } });
    }
    Some(Kind::Chains { chains, ballpark: None })
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

/// Sorts transformations to WGS 84 in PROJ's order, with no area of interest.
pub(crate) fn sort_as_proj(ops: &mut Vec<Arc<Op>>) {
    let mut keyed: Vec<(SortKey, Arc<Op>)> =
        ops.drain(..).map(|op| (SortKey::new(&op, op.name.clone(), pipeline_steps(&[(&op, false)]), None), op)).collect();
    keyed.sort_by(|x, y| x.0.cmp(&y.0));
    ops.extend(keyed.into_iter().map(|(_, op)| op));
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
        Item::Step(Step::Helmert { t, r, scale, pivot, inv, rates: None, .. }, inverted)
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
    fn a_3d_chain_keeps_the_height_only_across_its_wgs84_leg() {
        // A transformation PROJ keeps the height across between 3D CRSs, chained before and
        // after transformations that move it: their changes stay.
        let ell = crate::math::Ell::wgs84();
        let kept = Op { keeps_height_3d: true, ..Op::world(datum::towgs84_steps(ell, &[0.0, 0.0, 10.0])) };
        let moving = Op { keeps_height: false, ..Op::world(datum::towgs84_steps(ell, &[0.0, 0.0, 5.0])) };
        let p = Point { lon: 10_f64.to_radians(), lat: 50_f64.to_radians(), h: 100.0, t: None };
        for reverse in [false, true] {
            let legs = [(&moving, reverse), (&kept, reverse), (&moving, reverse), (&kept, reverse), (&moving, reverse)];
            let expected = legs.iter().try_fold(p, |q, (op, r)| datum::run_op(op, q, *r, true)).unwrap();
            let chained = run_chain(legs.into_iter(), p, true).unwrap();
            assert!((chained.h - expected.h).abs() < 1e-9 && (chained.h - p.h).abs() > 1.0, "reverse {reverse}: {chained:?} {expected:?}");
        }
    }

    #[test]
    fn hub_availability_excludes_operations_outside_the_crs_extents() {
        let src = Def::new(&SpatialRef::from_epsg(27700).unwrap()).unwrap();
        let dst = Def::new(&SpatialRef::from_epsg(2154).unwrap()).unwrap();
        let mut route = Route::new(&src, &dst);
        route.kind = Kind::Hub;
        let world = Arc::new(Op::world(Vec::new()));
        route.src_ops = vec![world.clone()];
        route.dst_ops = vec![world.clone()];
        assert!(route.has_transformation());
        let outside = Arc::new(Op { bounds: [120., 20., 121., 21.], ..Op::world(Vec::new()) });
        route.src_ops = vec![outside.clone()];
        assert!(!route.has_transformation());
        route.src_ops = vec![world];
        route.dst_ops = vec![outside];
        assert!(!route.has_transformation());
    }

    #[test]
    fn ballpark_legs_are_excluded_from_list_and_chain_availability() {
        for ballpark in [false, true] {
            let leg = Leg { op: Arc::new(Op { ballpark, ..Op::world(Vec::new()) }), reverse: false };
            let list = route_of(Kind::List(vec![leg.clone()]));
            let chain = route_of(Kind::Chains { chains: vec![direct_chain(&leg, None).1], ballpark: None });
            assert_eq!(list.has_transformation(), !ballpark);
            assert_eq!(chain.has_transformation(), !ballpark);
        }
    }

    #[test]
    fn recording_cleans_up_after_unwind() {
        assert!(
            std::panic::catch_unwind(|| recording(|| {
                note(None, false, OpKind::Datum);
                panic!("recording interrupted");
            }))
            .is_err()
        );
        USED.with(|used| assert!(used.borrow().is_none()));
        // An unrecorded operation must stay a no-op after the caught panic.
        note(None, false, OpKind::Datum);
        USED.with(|used| assert!(used.borrow().is_none()));
    }

    #[test]
    fn nested_recording_restores_the_outer_capture() {
        let (_, outer) = recording(|| {
            note(None, false, OpKind::Datum);
            let (_, inner) = recording(|| note(None, false, OpKind::Vertical));
            assert_eq!(inner.len(), 1);
            assert_eq!(inner[0].name, "Ballpark vertical transformation");
            assert!(
                std::panic::catch_unwind(|| recording(|| {
                    note(None, false, OpKind::Vertical);
                    panic!("inner recording interrupted");
                }))
                .is_err()
            );
            note(None, false, OpKind::Datum);
        });
        assert_eq!(outer.len(), 2);
        assert!(outer.iter().all(|operation| operation.name == "Ballpark geographic offset"));
        USED.with(|used| assert!(used.borrow().is_none()));
    }

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
            hub_areas: [None; 3],
        };
        let p = Point { lon: 110_f64.to_radians(), lat: 35_f64.to_radians(), h: 0.0, t: None };
        let used_ballpark = std::cell::Cell::new(false);
        assert!(route.apply_reporting(&src, 110.0, 35.0, p, false, &|op, _| used_ballpark.set(op.is_none_or(|op| op.ballpark))).is_some());
        assert!(used_ballpark.get());
    }

    /// A route of one kind, for tests.
    fn route_of(kind: Kind) -> Route {
        Route { kind, boxes: OnceLock::new(), hub_boxes: OnceLock::new(), src_ops: Vec::new(), dst_ops: Vec::new(), hub_areas: [None; 3] }
    }

    #[test]
    fn own_transformations_run_as_chains_move_points_and_heights_as_listed() {
        let src = Def::new(&SpatialRef::parse("EPSG:4230").unwrap()).unwrap();
        let p = Point { lon: 10_f64.to_radians(), lat: 50_f64.to_radians(), h: 100.0, t: None };
        let mut moved = 0;
        for leg in src.ops().iter().filter(|o| !o.ballpark).flat_map(|o| [false, true].map(|reverse| Leg { op: o.clone(), reverse })) {
            let list = route_of(Kind::List(vec![leg.clone()]));
            let chain = route_of(Kind::Chains { chains: vec![direct_chain(&leg, None).1], ballpark: None });
            for three_d in [false, true] {
                let (a, b) = (list.apply(&src, 10.0, 50.0, p, three_d), chain.apply(&src, 10.0, 50.0, p, three_d));
                assert_eq!(a, b, "{} reversed {} in 3D {three_d}", leg.op.name, leg.reverse);
                if a.is_some_and(|q| q.lon != p.lon || q.lat != p.lat) {
                    moved += 1;
                }
            }
        }
        // Some of them shift the point (those needing no grid).
        assert!(moved >= 4, "{moved} legs moved the point");
        // A missing grid: the list's next transformation that needs none, the chains' ballpark,
        // both no shift here.
        let grid = Leg {
            op: Arc::new(Op { accuracy: 1.0, steps: datum::nadgrids_steps("/nonexistent/terrio-grid.tif"), ..Op::world(Vec::new()) }),
            reverse: false,
        };
        let ballpark = Op { ballpark: true, accuracy: -1.0, ..Op::world(Vec::new()) };
        let list = route_of(Kind::List(vec![grid.clone(), Leg { op: Arc::new(ballpark.clone()), reverse: false }]));
        let chain = route_of(Kind::Chains { chains: vec![direct_chain(&grid, None).1], ballpark: Some(ballpark) });
        assert_eq!(list.apply(&src, 10.0, 50.0, p, false), Some(p));
        assert_eq!(chain.apply(&src, 10.0, 50.0, p, false), Some(p));
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

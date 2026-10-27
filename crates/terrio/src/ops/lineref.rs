//! Linear referencing, the equivalent of GDAL's `ogrlineref`: dividing a route into parts of
//! equal reference length from reference points (repers, milestones) with known positions, and
//! converting between positions and coordinates with those parts.
//!
//! The steps follow `apps/ogrlineref.cpp` (MIT licensed), with OGR's `getSubLine`, `Value` and
//! `get_Length` ported, so that the same parts, positions and coordinates come out. The
//! projection of a point on a line and its distance are independently implemented with
//! textbook dot/cross-product formulas, nearest segment first in line order. GEOS is a
//! behavior reference through GDAL, not the source of these implementations.
//! Lengths of the first and last parts of a geographic route are measured in the UTM zone of
//! their middle, as `ogrlineref` measures them.

use crate::crs::{SpatialRef, Transformer};
use crate::ops::sql::field_index;
use crate::{Error, Result};
use std::collections::BTreeMap;
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

/// Distances closer than this are the same.
const DELTA: f64 = 0.00000001;
/// The largest distance of a reference point from a geographic route, in degrees.
const TOLERANCE_DEGREE: f64 = 0.00008983153;
/// The largest distance of a reference point from a projected route, in its units.
const TOLERANCE_METER: f64 = 10.0;
/// The fields of a part: its first and last positions, and its length per position unit.
const FIELD_START: &str = "beg";
const FIELD_FINISH: &str = "end";
const FIELD_SCALE_FACTOR: &str = "scale";

/// A line's vertices, with Z when the line has it.
#[derive(Debug, Clone, PartialEq)]
struct Line {
    pts: Vec<[f64; 3]>,
    has_z: bool,
}

/// A point, with Z when it has it (2D points have Z 0, as OGR's `getZ`).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Point {
    x: f64,
    y: f64,
    z: f64,
    has_z: bool,
}

impl Point {
    fn from_coord(c: &Coord) -> Self {
        Self { x: c.x, y: c.y, z: c.z.unwrap_or(0.0), has_z: c.z.is_some() }
    }
}

impl Line {
    fn from_coords(cs: &[Coord]) -> Self {
        let has_z = cs.iter().any(|c| c.z.is_some());
        Self { pts: cs.iter().map(|c| [c.x, c.y, c.z.unwrap_or(0.0)]).collect(), has_z }
    }

    fn to_geometry(&self) -> Geometry {
        Geometry::LineString(
            self.pts.iter().map(|p| if self.has_z { Coord::xyz(p[0], p[1], p[2]) } else { Coord::xy(p[0], p[1]) }).collect(),
        )
    }

    fn point(&self, i: usize) -> Point {
        let p = self.pts[i];
        Point { x: p[0], y: p[1], z: if self.has_z { p[2] } else { 0.0 }, has_z: self.has_z }
    }

    fn seg_len(&self, i: usize) -> f64 {
        let (dx, dy) = (self.pts[i + 1][0] - self.pts[i][0], self.pts[i + 1][1] - self.pts[i][1]);
        (dx * dx + dy * dy).sqrt()
    }

    /// OGR's `get_Length`.
    fn length(&self) -> f64 {
        let mut len = 0.0;
        for i in 0..self.pts.len().saturating_sub(1) {
            len += self.seg_len(i);
        }
        len
    }

    fn lerp(&self, i: usize, ratio: f64) -> [f64; 3] {
        let (a, b) = (self.pts[i], self.pts[i + 1]);
        [
            a[0] * (1.0 - ratio) + b[0] * ratio,
            a[1] * (1.0 - ratio) + b[1] * ratio,
            if self.has_z { a[2] * (1.0 - ratio) + b[2] * ratio } else { 0.0 },
        ]
    }

    /// OGR's `Value`: the point at a distance along the line (its ends beyond them).
    fn value(&self, distance: f64) -> Result<Point> {
        let n = self.pts.len();
        if n == 0 {
            return Err(Error::InvalidArgument("cannot interpolate an empty line".into()));
        }
        if distance < 0.0 {
            return Ok(self.point(0));
        }
        let mut len = 0.0;
        for i in 0..n.saturating_sub(1) {
            let seg = self.seg_len(i);
            if seg > 0.0 {
                if len <= distance && len + seg >= distance {
                    let p = self.lerp(i, (distance - len) / seg);
                    return Ok(Point { x: p[0], y: p[1], z: p[2], has_z: self.has_z });
                }
                len += seg;
            }
        }
        Ok(self.point(n - 1))
    }

    /// OGR's `getSubLine` (distances, not ratios): None for invalid distances.
    fn sub_line(&self, mut from: f64, mut to: f64) -> Option<Line> {
        let n = self.pts.len();
        let total = self.length();
        if from < 0.0 {
            from = 0.0;
        }
        if to > total {
            to = total;
        }
        if from > to || from >= total {
            return None;
        }
        let mut out = Line { pts: Vec::new(), has_z: self.has_z };
        let mut len = 0.0;
        let mut i = 0;
        if from == 0.0 {
            // OGR getSubLine includes the first vertex here and again in the loop.
            out.pts.push(self.pts[0]);
        } else {
            while i < n - 1 {
                let seg = self.seg_len(i);
                if seg > 0.0 {
                    if len <= from && len + seg >= from {
                        out.pts.push(self.lerp(i, (from - len) / seg));
                        if len <= to && len + seg >= to {
                            out.pts.push(self.lerp(i, (to - len) / seg));
                            return (out.pts.len() >= 2).then_some(out);
                        }
                        i += 1;
                        len += seg;
                        break;
                    }
                    len += seg;
                }
                i += 1;
            }
        }
        while i < n - 1 {
            out.pts.push(self.pts[i]);
            let seg = self.seg_len(i);
            if seg > 0.0 {
                if len <= to && len + seg >= to {
                    out.pts.push(self.lerp(i, (to - len) / seg));
                    return Some(out);
                }
                len += seg;
            }
            i += 1;
        }
        out.pts.push(self.pts[n - 1]);
        (out.pts.len() >= 2).then_some(out)
    }

    /// Independent nearest-segment projection: the length along the line nearest to `p`.
    fn project_segments(&self, p: &Point) -> f64 {
        let mut min_distance = f64::INFINITY;
        let mut measure = -1.0;
        let mut start = 0.0;
        for i in 0..self.pts.len().saturating_sub(1) {
            let (a, b) = ([self.pts[i][0], self.pts[i][1]], [self.pts[i + 1][0], self.pts[i + 1][1]]);
            let d = point_to_segment([p.x, p.y], a, b);
            let seg = dist(a, b);
            let factor = projection_factor([p.x, p.y], a, b);
            let m = if factor <= 0.0 {
                start
            } else if factor <= 1.0 {
                start + factor * seg
            } else {
                start + seg
            };
            if d < min_distance && m > -1.0 {
                measure = m;
                min_distance = d;
            }
            start += seg;
        }
        measure
    }

    /// `ogrlineref`'s `Project`: 0 and the length at the ends, else nearest-segment projection.
    fn project(&self, p: &Point) -> f64 {
        if self.pts.is_empty() {
            return -1.0;
        }
        if self.point(0) == *p {
            return 0.0;
        }
        if self.point(self.pts.len() - 1) == *p {
            return self.length();
        }
        self.project_segments(p)
    }

    /// Euclidean distance from the line to a point.
    fn distance(&self, p: &Point) -> f64 {
        if self.pts.len() == 1 {
            return dist([p.x, p.y], [self.pts[0][0], self.pts[0][1]]);
        }
        let mut min = f64::INFINITY;
        for i in 0..self.pts.len().saturating_sub(1) {
            let d = point_to_segment([p.x, p.y], [self.pts[i][0], self.pts[i][1]], [self.pts[i + 1][0], self.pts[i + 1][1]]);
            if d < min {
                min = d;
            }
        }
        min
    }

    fn reverse(&mut self) {
        self.pts.reverse();
    }
}

fn dist(a: [f64; 2], b: [f64; 2]) -> f64 {
    let (dx, dy) = (a[0] - b[0], a[1] - b[1]);
    (dx * dx + dy * dy).sqrt()
}

/// Dot-product projection parameter along a segment.
fn projection_factor(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    if p == a {
        return 0.0;
    }
    if p == b {
        return 1.0;
    }
    if a == b {
        return 0.0;
    }
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let len2 = dx * dx + dy * dy;
    ((p[0] - a[0]) * dx + (p[1] - a[1]) * dy) / len2
}

/// Point-to-segment distance from dot/cross products.
fn point_to_segment(p: [f64; 2], a: [f64; 2], b: [f64; 2]) -> f64 {
    if a == b {
        return dist(p, a);
    }
    let len2 = (b[0] - a[0]) * (b[0] - a[0]) + (b[1] - a[1]) * (b[1] - a[1]);
    let r = ((p[0] - a[0]) * (b[0] - a[0]) + (p[1] - a[1]) * (b[1] - a[1])) / len2;
    if r <= 0.0 {
        return dist(p, a);
    }
    if r >= 1.0 {
        return dist(p, b);
    }
    let s = ((a[1] - p[1]) * (b[0] - a[0]) - (a[0] - p[0]) * (b[1] - a[1])) / len2;
    s.abs() * len2.sqrt()
}

/// Options of [`create_reference`], named after `ogrlineref -create`'s.
#[derive(Debug, Clone)]
pub struct LineRefOptions {
    /// `-pm`: the field of the reference points holding their positions.
    pub position_field: String,
    /// `-s`: the length of the parts, in position units.
    pub step: f64,
    /// `-lf`: the field identifying routes in the lines layer (with `point_id_field`): each route
    /// is referenced with the points of the same identifier.
    pub line_id_field: Option<String>,
    /// `-pf`: the field identifying the route of each reference point.
    pub point_id_field: Option<String>,
    /// `-of`: the output field receiving the route identifiers. `uniq_uid` by default.
    pub output_id_field: String,
    /// `-on`: the output layer name. `parts` by default.
    pub layer_name: Option<String>,
}

impl Default for LineRefOptions {
    fn default() -> Self {
        Self {
            position_field: String::new(),
            step: f64::NAN,
            line_id_field: None,
            point_id_field: None,
            output_id_field: "uniq_uid".into(),
            layer_name: None,
        }
    }
}

/// The output layer of parts: `beg`, `end` and `scale` (and the route identifier) fields.
fn parts_layer(name: &str, like: &Layer, id_field: Option<&str>) -> Layer {
    let mut out = Layer::new(name).with_geom_type(GeometryType::LineString);
    out.crs = like.crs.clone();
    for f in [FIELD_START, FIELD_FINISH, FIELD_SCALE_FACTOR] {
        out.schema.add_field(FieldDef::new(f, FieldType::Float));
    }
    if let Some(f) = id_field {
        out.schema.add_field(FieldDef::new(f, FieldType::Text));
    }
    out
}

fn add_part(out: &mut Layer, part: &Line, from: f64, to: f64, scale: f64, id: Option<&str>) {
    let mut attributes = vec![FieldValue::Float(from), FieldValue::Float(to), FieldValue::Float(scale)];
    if let Some(id) = id {
        attributes.push(FieldValue::Text(id.to_string()));
    }
    let fid = out.features.len() as u64;
    out.features.push(Feature { fid, geometry: Some(part.to_geometry()), attributes });
}

/// A field as OGR's `GetFieldAsDouble` reads it (NaN when null).
fn as_double(v: Option<&FieldValue>) -> f64 {
    match v {
        Some(FieldValue::Integer(i)) => *i as f64,
        Some(FieldValue::Float(f)) => *f,
        Some(FieldValue::Boolean(b)) => *b as i64 as f64,
        Some(FieldValue::Text(s)) => super::vector_grid::leading_number(s),
        Some(FieldValue::Null) | None => f64::NAN,
        Some(_) => 0.0,
    }
}

/// Unfiltered numeric field lookups use OGR's NULL-as-zero convention.
fn part_scale(value: Option<&FieldValue>) -> f64 {
    match value {
        None | Some(FieldValue::Null) => 0.0,
        value => as_double(value),
    }
}

/// A field as OGR's `GetFieldAsString` writes it.
fn as_string(v: Option<&FieldValue>) -> String {
    match v {
        Some(FieldValue::Integer(i)) => i.to_string(),
        Some(FieldValue::Float(f)) => format!("{f}"),
        Some(FieldValue::Boolean(b)) => (*b as i64).to_string(),
        Some(FieldValue::Text(s)) | Some(FieldValue::Date(s)) | Some(FieldValue::DateTime(s)) => s.clone(),
        _ => String::new(),
    }
}

fn is_geographic(layer: &Layer) -> bool {
    let s = SpatialRef::from_layer(layer);
    !s.is_unknown() && s.is_geographic()
}

/// The length of a part of a geographic route in the UTM zone of its middle, as `ogrlineref`
/// computes it; its length in degrees if it cannot be transformed.
fn utm_length(part: &Line, src: &SpatialRef) -> f64 {
    let len = part.length();
    let Ok(mid) = part.value(len / 2.0) else { return 0.0 };
    let zone = (30.0 + (mid.x + 3.0) / 6.0 + 0.5) as i32;
    let epsg = if mid.y > 0.0 { 32600 + zone } else { 32700 + zone };
    let transformed = || -> Result<f64> {
        let utm = SpatialRef::from_epsg(epsg as u32)?;
        let t = Transformer::new(src, &utm)?;
        let pts = part.pts.iter().map(|p| t.transform(p[0], p[1]).map(|(x, y)| [x, y, p[2]])).collect::<Result<Vec<_>>>()?;
        Ok(Line { pts, has_z: part.has_z }.length())
    };
    transformed().unwrap_or_else(|e| {
        crate::warnings::warn(format!("the part could not be measured in UTM (EPSG:{epsg}): {e}; its length in degrees is used"));
        len
    })
}

/// A part of the route between two positions, `factor` route units per position unit.
struct CurveData {
    part: Line,
    beg: f64,
    end: f64,
    factor: f64,
}

impl CurveData {
    fn is_inside(&self, d: f64) -> bool {
        d + DELTA >= self.beg && d - DELTA <= self.end
    }
}

fn metric_factor(length: f64, metric_length: f64) -> Result<f64> {
    if !(length.is_finite() && length > 0.0 && metric_length.is_finite() && metric_length > 0.0) {
        return Err(Error::InvalidArgument("cannot measure a degenerate route segment".into()));
    }
    let factor = length / metric_length;
    if !factor.is_finite() || factor <= 0.0 {
        return Err(Error::InvalidArgument("route segment scale must be finite and positive".into()));
    }
    Ok(factor)
}

/// `CreatePartsFromLineString`: the parts of one route.
#[allow(clippy::too_many_arguments)]
fn parts_of_line(
    mut path: Line,
    geographic: bool,
    srs: &SpatialRef,
    reference_points: &[&Feature],
    position: usize,
    step: f64,
    out: &mut Layer,
    id: Option<&str>,
) -> Result<()> {
    let tolerance = if geographic { TOLERANCE_DEGREE } else { TOLERANCE_METER };
    // The reference points near the route, by position (a later one replaces an earlier one).
    let mut repers: BTreeMap<OrderedF64, Point> = BTreeMap::new();
    for &f in reference_points {
        let pos = as_double(f.attributes.get(position));
        let Some(Geometry::Point(c)) = &f.geometry else { continue };
        if !pos.is_finite() {
            if pos.is_infinite() {
                crate::warnings::warn("a milestone has a non-finite position and was skipped".into());
            }
            continue;
        }
        let pt = Point::from_coord(c);
        if repers.contains_key(&OrderedF64(pos)) {
            crate::warnings::warn(format!("the distance {pos:.6} is already present in repers file"));
        }
        if path.project(&pt) < 0.0 {
            crate::warnings::warn(format!("the distance {pos:.6} is out of path"));
        } else if path.distance(&pt) < tolerance {
            repers.insert(OrderedF64(pos), pt);
        }
    }
    if repers.len() < 2 {
        return Err(Error::InvalidArgument("not enough reference points near the route to proceed".into()));
    }
    let entries: Vec<(f64, Point)> = repers.iter().map(|(k, v)| (k.0, *v)).collect();
    let begin_position = entries[0].0;
    let mut position_now = entries[0].0;
    let (pt1, pt2) = (entries[0].1, entries[1].1);
    let mut d1 = path.project(&pt1);
    let mut d2 = path.project(&pt2);
    if d1 > d2 {
        crate::warnings::warn("the path is opposite the reference points' direction: reversed".into());
        path.reverse();
        d1 = path.project(&pt1);
        d2 = path.project(&pt2);
    }
    let mut sub_lines: Vec<CurveData> = Vec::new();
    let (mut pt_beg, mut pt_end): (Option<Point>, Option<Point>) = (None, None);
    let (mut pt_beg_position, mut pt_end_position) = (0.0, 0.0);
    // From the route's start to the first point.
    if d1 > DELTA
        && let Some(part) = path.sub_line(0.0, d1)
    {
        let len = if geographic { utm_length(&part, srs) } else { part.length() };
        let factor = if geographic { metric_factor(part.length(), len)? } else { 1.0 };
        pt_beg = Some(part.point(0));
        pt_beg_position = position_now - len;
        sub_lines.push(CurveData { beg: position_now - len, end: position_now, factor, part });
    }
    if d2 - d1 > DELTA
        && let Some(part) = path.sub_line(d1, d2)
    {
        // BTreeMap position keys are distinct, so this denominator is nonzero.
        let factor = part.length() / (entries[1].0 - position_now);
        sub_lines.push(CurveData { part, beg: position_now, end: entries[1].0, factor });
    }
    d1 = d2;
    position_now = entries[1].0;
    let mut end_position = 0.0;
    for &(pos, pt) in &entries[2..] {
        end_position = pos;
        let d = path.project(&pt);
        if d - d1 > DELTA
            && let Some(part) = path.sub_line(d1, d)
        {
            let factor = part.length() / (pos - position_now);
            sub_lines.push(CurveData { part, beg: position_now, end: pos, factor });
            d1 = d;
            position_now = pos;
        }
    }
    // From the last point to the route's end.
    let total = path.length();
    if total - d1 > DELTA
        && let Some(part) = path.sub_line(d1, total)
    {
        let len = if geographic { utm_length(&part, srs) } else { part.length() };
        let factor = if geographic { metric_factor(part.length(), len)? } else { 1.0 };
        pt_end = Some(part.point(part.pts.len() - 1));
        pt_end_position = position_now + len;
        sub_lines.push(CurveData { beg: position_now, end: position_now + len, factor, part });
    }
    // The pickets: a point every `step` positions.
    let round_beg = if pt_beg.is_some() { (pt_beg_position / step).ceil() * step } else { (begin_position / step).ceil() * step };
    if pt_end.is_some() {
        end_position = pt_end_position;
    }
    let mut pickets: BTreeMap<OrderedF64, Point> = BTreeMap::new();
    if let Some(p) = pt_beg {
        pickets.insert(OrderedF64(pt_beg_position), p);
    }
    if let Some(p) = pt_end {
        pickets.insert(OrderedF64(pt_end_position), p);
    }
    // ogrlineref has no limit; a step far smaller than the route would never finish.
    const MAX_PICKETS: f64 = 100_000.0;
    if (end_position - round_beg) / step > MAX_PICKETS {
        return Err(Error::InvalidArgument(format!("the step {step} makes more than {MAX_PICKETS} parts")));
    }
    let mut dist_now = round_beg;
    while dist_now <= end_position {
        if let Some(s) = sub_lines.iter().find(|s| s.is_inside(dist_now)) {
            pickets.insert(OrderedF64(dist_now), s.part.value((dist_now - s.beg) * s.factor)?);
        }
        if dist_now + step == dist_now {
            break;
        }
        dist_now += step;
    }
    // The parts between consecutive pickets.
    let mut d1 = 0.0;
    let mut position_now = pickets.keys().next().map_or(0.0, |k| k.0);
    for (k, pt) in &pickets {
        let d = path.project(pt);
        if d - d1 > DELTA
            && let Some(part) = path.sub_line(d1, d)
        {
            let scale = part.length() / (k.0 - position_now);
            if !scale.is_finite() {
                crate::warnings::warn("a part has a non-finite scale; adjust milestone positions or step before lookup".into());
            }
            add_part(out, &part, position_now, k.0, scale, id);
            d1 = d;
            position_now = k.0;
        }
    }
    Ok(())
}

/// An `f64` ordered as `std::map<double, ...>` orders keys (NaN never stored).
#[derive(Debug, Clone, Copy, PartialEq)]
struct OrderedF64(f64);

impl Eq for OrderedF64 {}

impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.partial_cmp(&other.0).unwrap_or(std::cmp::Ordering::Equal)
    }
}

/// The route lines of a feature: a line string, or each line of a multi-line string.
fn route_lines(g: Option<&Geometry>) -> Vec<Line> {
    match g {
        Some(Geometry::LineString(cs)) => vec![Line::from_coords(cs)],
        Some(Geometry::MultiLineString(ls)) => ls.iter().map(|l| Line::from_coords(l)).collect(),
        _ => Vec::new(),
    }
}

fn check_types(lines: &Layer, repers: &Layer) -> Result<()> {
    let is = |l: &Layer, ok: &dyn Fn(&Geometry) -> bool| l.features.iter().filter_map(|f| f.geometry.as_ref()).all(ok);
    if !is(lines, &|g| matches!(g, Geometry::LineString(_) | Geometry::MultiLineString(_))) {
        return Err(Error::InvalidArgument("unsupported geometry type for path: lines needed".into()));
    }
    if !is(repers, &|g| matches!(g, Geometry::Point(_))) {
        return Err(Error::InvalidArgument("unsupported geometry type for the reference points: points needed".into()));
    }
    Ok(())
}

/// Creates the linear reference of a route, as `ogrlineref -create`: the route (the first
/// feature of `lines`, or with `line_id_field` and `point_id_field` each route with its points)
/// cut into parts `step` positions long, from reference points carrying their positions in
/// `position_field`. Each part has its first and last positions (`beg`, `end`) and its length
/// per position unit (`scale`).
pub fn create_reference(lines: &Layer, repers: &Layer, options: &LineRefOptions) -> Result<Layer> {
    if !options.step.is_finite() || options.step <= 0.0 {
        return Err(Error::InvalidArgument("the step (part length) must be positive".into()));
    }
    check_types(lines, repers)?;
    let position = field_index(&repers.schema, &options.position_field)
        .ok_or_else(|| Error::InvalidArgument(format!("failed to find field {} on layer {}", options.position_field, repers.name)))?;
    let geographic = is_geographic(lines);
    let srs = SpatialRef::from_layer(lines);
    let name = options.layer_name.clone().unwrap_or_else(|| "parts".into());
    match (&options.line_id_field, &options.point_id_field) {
        (Some(lf), Some(pf)) => {
            let li = field_index(&lines.schema, lf).ok_or_else(|| Error::InvalidArgument(format!("the field {lf} was not found")))?;
            let pi = field_index(&repers.schema, pf).ok_or_else(|| Error::InvalidArgument(format!("the field {pf} was not found")))?;
            let mut out = parts_layer(&name, lines, Some(&options.output_id_field));
            let mut routes = BTreeMap::new();
            for feature in &lines.features {
                routes.entry(as_string(feature.attributes.get(li))).or_insert(feature);
            }
            let mut points: std::collections::HashMap<String, Vec<&Feature>> = Default::default();
            for feature in &repers.features {
                points.entry(as_string(feature.attributes.get(pi))).or_default().push(feature);
            }
            for (id, feature) in &routes {
                // As ogrlineref: the first line per route and lexicographic route order;
                // points retain input order so later duplicate positions still win.
                let reference_points = points.get(id).map_or(&[][..], Vec::as_slice);
                for line in route_lines(feature.geometry.as_ref()) {
                    if let Err(e) = parts_of_line(line, geographic, &srs, reference_points, position, options.step, &mut out, Some(id)) {
                        crate::warnings::warn(format!("route {id}: {e}"));
                        break;
                    }
                }
            }
            Ok(out)
        }
        _ => {
            let mut out = parts_layer(&name, lines, None);
            let Some(f) = lines.features.first() else {
                return Err(Error::InvalidArgument("the lines layer has no route".into()));
            };
            let routes = route_lines(f.geometry.as_ref());
            if routes.is_empty() {
                return Err(Error::InvalidArgument("the first feature of the lines layer has no line".into()));
            }
            let reference_points: Vec<&Feature> = repers.features.iter().collect();
            for line in routes {
                parts_of_line(line, geographic, &srs, &reference_points, position, options.step, &mut out, None)?;
            }
            Ok(out)
        }
    }
}

/// A part's line: a line string, or a multi-line string of one line.
fn part_line(g: Option<&Geometry>) -> Option<Line> {
    match g {
        Some(Geometry::LineString(cs)) => Some(Line::from_coords(cs)),
        Some(Geometry::MultiLineString(ls)) if ls.len() == 1 => Some(Line::from_coords(&ls[0])),
        _ => None,
    }
}

fn part_fields(parts: &Layer) -> Result<(usize, usize, usize)> {
    let get = |n: &str| field_index(&parts.schema, n).ok_or_else(|| Error::InvalidArgument(format!("the parts layer has no {n} field")));
    Ok((get(FIELD_START)?, get(FIELD_FINISH)?, get(FIELD_SCALE_FACTOR)?))
}

/// The position of the point `(x, y)`, as `ogrlineref -get_pos`: on the nearest part, its
/// start position plus the length to the point's projection over the part's scale.
pub fn position(parts: &Layer, x: f64, y: f64) -> Result<f64> {
    let (b, _, s) = part_fields(parts)?;
    let pt = Point { x, y, z: 0.0, has_z: false };
    let mut closest: Option<(Line, f64, f64)> = None;
    let mut min = f64::MAX;
    for f in &parts.features {
        let Some(g) = &f.geometry else { continue };
        let Some(line) = part_line(Some(g)) else { continue };
        if line.pts.is_empty() {
            continue;
        }
        let d = line.distance(&pt);
        if d < min {
            min = d;
            closest = Some((line, part_scale(f.attributes.get(b)), part_scale(f.attributes.get(s))));
        }
    }
    let (line, beg, scale) = closest.ok_or_else(|| Error::InvalidArgument("failed to find the closest part".into()))?;
    if scale == 0.0 {
        return Err(Error::InvalidArgument("the closest part has a scale of 0".into()));
    }
    if scale.is_nan() {
        return Err(Error::InvalidArgument("the closest part has a NaN scale".into()));
    }
    Ok(beg + line.project(&pt) / scale)
}

/// The value of `v` as `ogrlineref` puts it in an attribute filter (`%f`).
fn six_decimals(v: f64) -> f64 {
    format!("{v:.6}").parse().unwrap_or(v)
}

/// The coordinates at a position, as `ogrlineref -get_coord`: on every part strictly spanning it
/// (`beg < position < end`, with the position rounded to 6 decimals as `ogrlineref` filters), the
/// point its scale puts there. `[x, y, z]`, z 0 for 2D parts.
pub fn coordinates(parts: &Layer, position: f64) -> Result<Vec<[f64; 3]>> {
    let (b, e, s) = part_fields(parts)?;
    let p = six_decimals(position);
    let mut out = Vec::new();
    for f in &parts.features {
        let (beg, end) = (as_double(f.attributes.get(b)), as_double(f.attributes.get(e)));
        if !(beg < p && end > p) {
            continue;
        }
        let line = part_line(f.geometry.as_ref())
            .ok_or_else(|| Error::InvalidArgument("a part spanning the position has no single LineString geometry".into()))?;
        let scale = part_scale(f.attributes.get(s));
        let pt = line.value((position - beg) * scale)?;
        out.push([pt.x, pt.y, pt.z]);
    }
    if out.is_empty() {
        return Err(Error::InvalidArgument(format!("get coordinates for position {position:.6} failed")));
    }
    Ok(out)
}

/// The route between two positions, as `ogrlineref -get_subline`: the parts covering them (by
/// the step of the second part), the first and last cut at the positions, joined into a line,
/// in a layer named `layer_name` with `beg`, `end` and `scale` (1) fields.
pub fn subline(parts: &Layer, begin: f64, end: f64, layer_name: &str) -> Result<Layer> {
    let (b, e, s) = part_fields(parts)?;
    let failed = || Error::InvalidArgument(format!("get step for positions {begin:.6} - {end:.6} failed"));
    let second = parts.features.get(1).ok_or_else(failed)?;
    let step = as_double(second.attributes.get(e)) - as_double(second.attributes.get(b));
    if !(step.is_finite() && step > 0.0) {
        return Err(Error::InvalidArgument(format!("the second part's length ({step}) is not a positive step")));
    }
    let low = six_decimals((begin / step).floor() * step);
    let high = six_decimals((end / step).ceil() * step);
    let mut chosen: BTreeMap<OrderedF64, (Line, f64)> = BTreeMap::new();
    for f in &parts.features {
        let (fb, fe) = (as_double(f.attributes.get(b)), as_double(f.attributes.get(e)));
        if !(fb >= low && fe <= high) || fb.is_nan() {
            continue;
        }
        if let Some(line) = part_line(f.geometry.as_ref())
            && chosen.insert(OrderedF64(fb), (line, part_scale(f.attributes.get(s)))).is_some()
        {
            return Err(Error::InvalidArgument(
                "several parts have the same start position: select a single route before extracting a subline".into(),
            ));
        }
    }
    if chosen.is_empty() {
        return Err(Error::InvalidArgument(format!("get parts for positions {begin:.6} - {end:.6} failed")));
    }
    let parts_vec: Vec<(f64, Line, f64)> = chosen.into_iter().map(|(k, (l, sf))| (k.0, l, sf)).collect();
    let invalid = || Error::InvalidArgument("the positions are outside the parts".into());
    let line = if parts_vec.len() == 1 {
        let (start, line, sf) = &parts_vec[0];
        line.sub_line((begin - start) * sf, (end - start) * sf).ok_or_else(invalid)?
    } else {
        let mut out = Line { pts: Vec::new(), has_z: parts_vec.iter().any(|p| p.1.has_z) };
        let (start, first, sf) = &parts_vec[0];
        out.pts.extend(first.sub_line((begin - start) * sf, first.length()).ok_or_else(invalid)?.pts);
        for (_, l, _) in &parts_vec[1..parts_vec.len() - 1] {
            out.pts.extend(&l.pts);
        }
        let (start, last, sf) = &parts_vec[parts_vec.len() - 1];
        out.pts.extend(last.sub_line(0.0, (end - start) * sf).ok_or_else(invalid)?.pts);
        out
    };
    let mut out = parts_layer(layer_name, parts, None);
    add_part(&mut out, &line, begin, end, 1.0, None);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(pts: &[(f64, f64)]) -> Line {
        Line { pts: pts.iter().map(|p| [p.0, p.1, 0.0]).collect(), has_z: false }
    }

    #[test]
    fn sub_lines_and_values_follow_ogr() {
        let l = line(&[(0.0, 0.0), (10.0, 0.0), (10.0, 10.0)]);
        assert_eq!(l.length(), 20.0);
        assert_eq!(l.sub_line(5.0, 15.0).unwrap().pts, vec![[5.0, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 5.0, 0.0]]);
        assert_eq!(l.sub_line(2.0, 3.0).unwrap().pts, vec![[2.0, 0.0, 0.0], [3.0, 0.0, 0.0]]);
        assert_eq!(l.sub_line(0.0, 5.0).unwrap().pts, vec![[0.0, 0.0, 0.0], [0.0, 0.0, 0.0], [5.0, 0.0, 0.0]]);
        assert!(l.sub_line(20.0, 25.0).is_none());
        let v = l.value(12.0).unwrap();
        assert_eq!((v.x, v.y), (10.0, 2.0));
        assert_eq!(l.project_segments(&Point { x: 12.0, y: 4.0, z: 0.0, has_z: false }), 14.0);
        assert_eq!(l.distance(&Point { x: 12.0, y: 4.0, z: 0.0, has_z: false }), 2.0);
    }

    fn layers() -> (Layer, Layer) {
        let mut lines = Layer::new("path");
        lines.features.push(Feature {
            fid: 0,
            geometry: Some(Geometry::LineString(vec![Coord::xy(0.0, 0.0), Coord::xy(1000.0, 0.0), Coord::xy(1000.0, 1000.0)])),
            attributes: vec![],
        });
        let mut repers = Layer::new("stones");
        repers.schema.add_field(FieldDef::new("pos", FieldType::Float));
        for (i, (x, y, pos)) in [(200.0, 1.0, 1200.0), (1000.0, 200.0, 2000.0), (999.0, 700.0, 2500.0)].into_iter().enumerate() {
            repers.features.push(Feature {
                fid: i as u64,
                geometry: Some(Geometry::point(x, y)),
                attributes: vec![FieldValue::Float(pos)],
            });
        }
        (lines, repers)
    }

    #[test]
    fn creates_parts_and_converts_positions() {
        let (lines, repers) = layers();
        let o = LineRefOptions { position_field: "pos".into(), step: 500.0, ..Default::default() };
        let parts = create_reference(&lines, &repers, &o).unwrap();
        let begs: Vec<f64> = parts.features.iter().map(|f| as_double(f.attributes.first())).collect();
        assert_eq!(begs, vec![1000.0, 1500.0, 2000.0, 2500.0]);
        let ends: Vec<f64> = parts.features.iter().map(|f| as_double(f.attributes.get(1))).collect();
        assert_eq!(ends, vec![1500.0, 2000.0, 2500.0, 2800.0]);
        // The route starts 200 before the first stone: position 1000 at (0, 0). Between the first
        // two stones, 800 positions span 1000 units: the picket 1500 is at 200 + 300 × 1.25.
        let scales: Vec<f64> = parts.features.iter().map(|f| as_double(f.attributes.get(2))).collect();
        assert_eq!(scales, vec![1.15, 1.25, 1.0, 1.0]);
        let p = position(&parts, 100.0, 0.0).unwrap();
        assert!((p - (1000.0 + 100.0 / 1.15)).abs() < 1e-9, "{p}");
        let c = coordinates(&parts, 2250.0).unwrap();
        assert_eq!(c.len(), 1);
        assert!((c[0][0] - 1000.0).abs() < 1e-9 && (c[0][1] - 450.0).abs() < 1e-9, "{c:?}");
        let s = subline(&parts, 1100.0, 2100.0, "sub").unwrap();
        assert_eq!(s.features.len(), 1);
        let Some(Geometry::LineString(cs)) = &s.features[0].geometry else { panic!() };
        assert!((cs[0].x - 115.0).abs() < 1e-9 && cs[0].y == 0.0);
        let last = cs.last().unwrap();
        assert!((last.x - 1000.0).abs() < 1e-9 && (last.y - 300.0).abs() < 1e-9);
    }

    #[test]
    fn coordinates_rejects_malformed_spanning_parts() {
        let (lines, repers) = layers();
        let options = LineRefOptions { position_field: "pos".into(), step: 500., ..Default::default() };
        let mut parts = create_reference(&lines, &repers, &options).unwrap();
        // Keep a valid match too: returning a partial result would hide the bad part.
        parts.features.push(parts.features[0].clone());
        parts.features[0].geometry = None;
        let error = coordinates(&parts, 1250.).unwrap_err();
        assert!(error.to_string().contains("no single LineString"));
    }

    #[test]
    fn coordinate_scale_zero_null_and_negative_match_ogr() {
        let (lines, repers) = layers();
        let options = LineRefOptions { position_field: "pos".into(), step: 500., ..Default::default() };
        let mut parts = create_reference(&lines, &repers, &options).unwrap();
        for scale in [FieldValue::Float(0.0), FieldValue::Null, FieldValue::Float(-1.0)] {
            parts.features[0].attributes[2] = scale;
            assert_eq!(coordinates(&parts, 1250.).unwrap(), vec![[0.0, 0.0, 0.0]]);
        }
    }

    #[test]
    fn position_rejects_null_and_nan_scales() {
        let (lines, repers) = layers();
        let options = LineRefOptions { position_field: "pos".into(), step: 500., ..Default::default() };
        let mut parts = create_reference(&lines, &repers, &options).unwrap();
        for scale in [FieldValue::Null, FieldValue::Float(f64::NAN)] {
            parts.features[0].attributes[2] = scale;
            assert!(position(&parts, 100., 0.).is_err());
        }
        parts.features[0].attributes[2] = FieldValue::Float(f64::INFINITY);
        assert_eq!(position(&parts, 100., 0.).unwrap(), 1000.0);
        assert_eq!(coordinates(&parts, 1250.).unwrap(), vec![[575.0, 0.0, 0.0]]);
        assert!(subline(&parts, 1100., 1200., "infinite-scale").is_err());
        parts.features[0].attributes[2] = FieldValue::Null;
        let output = subline(&parts, 1100., 1200., "null-scale").unwrap();
        let Some(Geometry::LineString(points)) = &output.features[0].geometry else { panic!() };
        // create_reference retains OGR's duplicate initial vertex.
        assert_eq!(points.len(), 4);
        assert!(points.iter().all(|point| point.x == 0.0 && point.y == 0.0));
    }

    #[test]
    fn null_begin_uses_ogr_zero_position_origin() {
        let (lines, repers) = layers();
        let options = LineRefOptions { position_field: "pos".into(), step: 500., ..Default::default() };
        let mut parts = create_reference(&lines, &repers, &options).unwrap();
        parts.features[0].attributes[0] = FieldValue::Null;
        let expected = 100. / as_double(parts.features[0].attributes.get(2));
        assert!((position(&parts, 100., 0.).unwrap() - expected).abs() < 1e-12);
        assert!(coordinates(&parts, 1250.).is_err());
    }

    #[test]
    fn infinite_milestone_positions_do_not_enter_part_generation() {
        let (lines, original) = layers();
        let options = LineRefOptions { position_field: "pos".into(), step: 500., ..Default::default() };
        for value in [f64::INFINITY, f64::NEG_INFINITY] {
            let mut points = original.clone();
            points.features[0].attributes[0] = FieldValue::Float(value);
            let parts = create_reference(&lines, &points, &options).unwrap();
            assert!(!parts.features.is_empty());
            for feature in parts.features {
                assert!(feature.attributes.iter().take(3).all(|v| as_double(Some(v)).is_finite()));
            }
        }
    }

    #[test]
    fn nan_scale_coordinate_and_final_subline_part_match_ogr() {
        let (lines, points) = layers();
        let options = LineRefOptions { position_field: "pos".into(), step: 500., ..Default::default() };
        let mut parts = create_reference(&lines, &points, &options).unwrap();
        parts.features[0].attributes[2] = FieldValue::Float(f64::NAN);
        assert_eq!(coordinates(&parts, 1250.).unwrap(), vec![[575., 0., 0.]]);
        let mut parts = create_reference(&lines, &points, &options).unwrap();
        parts.features.last_mut().unwrap().attributes[2] = FieldValue::Float(f64::NAN);
        let output = subline(&parts, 1100., 2700., "subline").unwrap();
        let Some(Geometry::LineString(coordinates)) = &output.features[0].geometry else { panic!() };
        let last = coordinates.last().unwrap();
        assert_eq!((last.x, last.y), (1000., 1000.));
    }

    #[test]
    fn grouped_routes_keep_id_order_and_first_line() {
        let (mut lines, mut points) = layers();
        lines.schema.add_field(FieldDef::new("route", FieldType::Text));
        points.schema.add_field(FieldDef::new("route", FieldType::Text));
        let original_line = lines.features[0].clone();
        let original_points = points.features.clone();
        lines.features.clear();
        points.features.clear();
        for id in ["b", "a"] {
            let mut line = original_line.clone();
            line.attributes.push(FieldValue::Text(id.into()));
            lines.features.push(line);
            for point in &original_points {
                let mut point = point.clone();
                point.attributes.push(FieldValue::Text(id.into()));
                points.features.push(point);
            }
        }
        let mut duplicate = lines.features[0].clone();
        duplicate.geometry = None;
        lines.features.push(duplicate);
        let options = LineRefOptions {
            position_field: "pos".into(),
            step: 500.,
            line_id_field: Some("route".into()),
            point_id_field: Some("route".into()),
            ..Default::default()
        };
        let parts = create_reference(&lines, &points, &options).unwrap();
        let field = field_index(&parts.schema, &options.output_id_field).unwrap();
        let ids: Vec<String> = parts.features.iter().map(|f| as_string(f.attributes.get(field))).collect();
        assert_eq!(ids, ["a", "a", "a", "a", "b", "b", "b", "b"]);
        // GDAL keeps the first feature even when NULL; later geometry does not replace it.
        lines.features[0].geometry = None;
        let parts = create_reference(&lines, &points, &options).unwrap();
        assert!(parts.features.iter().all(|feature| as_string(feature.attributes.get(field)) == "a"));
        assert_eq!(parts.features.len(), 4);
    }

    #[test]
    fn reverses_routes_against_the_points() {
        let (mut lines, repers) = layers();
        if let Some(Geometry::LineString(cs)) = &mut lines.features[0].geometry {
            cs.reverse();
        }
        let o = LineRefOptions { position_field: "pos".into(), step: 500.0, ..Default::default() };
        let parts = create_reference(&lines, &repers, &o).unwrap();
        let Some(Geometry::LineString(cs)) = &parts.features[0].geometry else { panic!() };
        assert_eq!((cs[0].x, cs[0].y), (0.0, 0.0));
        assert!(
            create_reference(&lines, &repers, &LineRefOptions { position_field: "nope".into(), step: 1.0, ..Default::default() }).is_err()
        );
    }
    #[test]
    fn two_endpoint_milestones_match_gdal_empty_parts() {
        let (mut lines, mut stones) = layers();
        lines.features[0].geometry = Some(Geometry::LineString(vec![Coord::xy(0., 0.), Coord::xy(100., 0.)]));
        stones.features.truncate(2);
        for (i, f) in stones.features.iter_mut().enumerate() {
            let v = i as f64 * 100.;
            f.geometry = Some(Geometry::point(v, 0.));
            f.attributes = vec![FieldValue::Float(v)];
        }
        let options = LineRefOptions { position_field: "pos".into(), step: 10., ..Default::default() };
        assert!(create_reference(&lines, &stones, &options).unwrap().features.is_empty());
    }

    #[test]
    fn empty_part_returns_error_instead_of_panicking() {
        let (lines, stones) = layers();
        let options = LineRefOptions { position_field: "pos".into(), step: 500., ..Default::default() };
        let mut parts = create_reference(&lines, &stones, &options).unwrap();
        parts.features.truncate(1);
        parts.features[0].geometry = Some(Geometry::LineString(vec![]));
        assert!(coordinates(&parts, 1250.).is_err());
        assert!(position(&parts, 0., 0.).is_err());
    }

    #[test]
    fn rejects_infinite_steps_and_colliding_parts() {
        let (lines, stones) = layers();
        let o = LineRefOptions { position_field: "pos".into(), step: f64::INFINITY, ..Default::default() };
        assert!(create_reference(&lines, &stones, &o).is_err());
        let mut parts = create_reference(&lines, &stones, &LineRefOptions { step: 500., ..o }).unwrap();
        parts.features.push(parts.features[1].clone());
        assert!(subline(&parts, 1100., 2100., "sub").unwrap_err().to_string().contains("single route"));
    }

    #[test]
    fn metric_factor_refuses_degenerate_lengths() {
        assert_eq!(as_double(Some(&FieldValue::Text("1200m".into()))), 1200.);
        assert!(metric_factor(0., 0.).is_err());
        assert!(metric_factor(1., 0.).is_err());
        assert!(metric_factor(f64::INFINITY, 1.).is_err());
        assert_eq!(metric_factor(2., 4.).unwrap(), 0.5);
    }
}

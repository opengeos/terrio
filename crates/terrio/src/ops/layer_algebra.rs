//! Planar XY layer algebra, including lower-dimensional intersections.
//! Reference semantics: GDAL 3.13.3 OGR layer algebra.
//!
//! Inputs must have matching CRS and valid geometry. Output topology is XY;
//! null and empty features contribute no geometry. Layers are materialized.
use crate::{Error, Feature, FieldValue, Geometry, Layer, Result};
use geo::algorithm::line_intersection::{LineIntersection, line_intersection};
use geo::{Area, BooleanOps, BoundingRect, Intersects};
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};
use std::sync::{Arc, OnceLock};
use wbvector::Coord;

use super::topology::{from_polygons, require_valid, to_geo, union};
use super::vector_clip::Mask;

type C = geo::Coord<f64>;
type Line = geo::Line<f64>;

/// The seven OGR layer-algebra operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayerAlgebraOperation {
    /// Emit one common geometry per intersecting pair, with both attributes.
    Intersection,
    /// Emit pair intersections and the remainder of both inputs.
    Union,
    /// Emit only the remainder of both inputs.
    SymmetricDifference,
    /// Emit pair intersections and the input remainder.
    Identity,
    /// Emit the input remainder followed by every method feature.
    Update,
    /// Intersect each input feature with the union of method geometry.
    Clip,
    /// Subtract method geometry from each input feature.
    Erase,
}
impl std::str::FromStr for LayerAlgebraOperation {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        match s {
            "intersection" => Ok(Self::Intersection),
            "union" => Ok(Self::Union),
            "sym-difference" | "symmetric-difference" => Ok(Self::SymmetricDifference),
            "identity" => Ok(Self::Identity),
            "update" => Ok(Self::Update),
            "clip" => Ok(Self::Clip),
            "erase" => Ok(Self::Erase),
            _ => {
                Err(Error::InvalidArgument("operation must be intersection, union, sym-difference, identity, update, clip or erase".into()))
            }
        }
    }
}
/// Attribute prefixes and treatment of touching geometry.
#[derive(Debug, Clone)]
pub struct LayerAlgebraOptions {
    /// Include intersections below the smaller input geometry dimension.
    /// Clip always includes such intersections; erase/update subtract complete geometry.
    pub keep_lower_dimension_geometries: bool,
    /// Prefix for input attributes. Defaults to `input_` when both schemas are used, otherwise empty.
    pub input_prefix: Option<String>,
    /// Prefix for method attributes. Defaults to `method_` when both schemas are used, otherwise empty.
    pub method_prefix: Option<String>,
}
impl Default for LayerAlgebraOptions {
    fn default() -> Self {
        Self { keep_lower_dimension_geometries: true, input_prefix: None, method_prefix: None }
    }
}

// Boundary contact alone does not change an untouched collection's type.
// Use an indexed positive-area intersection test instead of comparing total
// areas before/after union, whose rounding can differ for disjoint components.
fn polygon_components_overlap(polygons: &geo::MultiPolygon<f64>) -> bool {
    let bounds =
        |polygon: &geo::Polygon<f64>| polygon.bounding_rect().map(|r| AABB::from_corners([r.min().x, r.min().y], [r.max().x, r.max().y]));
    let index = RTree::bulk_load(
        polygons.0.iter().enumerate().filter_map(|(i, p)| bounds(p).map(|b| GeomWithData::new(Rectangle::from_aabb(b), i))).collect(),
    );
    polygons.0.iter().enumerate().any(|(i, polygon)| {
        bounds(polygon).is_some_and(|b| {
            index
                .locate_in_envelope_intersecting(b)
                .any(|candidate| candidate.data < i && polygon.intersection(&polygons.0[candidate.data]).unsigned_area() > 0.)
        })
    })
}

/// Combines two layers without modifying either. All seven operations support
/// polygon, line, point, multipart and collection geometry. Matching CRS is required.
/// Intersection/union/symmetric-difference/identity include both schemas; clip,
/// erase and update use input fields only. Update maps method values by output
/// field name. Prefix collisions are made unique. IDs are sequential from one.
pub fn layer_algebra(a: &Layer, b: &Layer, operation: LayerAlgebraOperation, options: &LayerAlgebraOptions) -> Result<Layer> {
    use LayerAlgebraOperation as Op;
    let ca = crate::crs::SpatialRef::from_layer(a);
    let cb = crate::crs::SpatialRef::from_layer(b);
    if ca.is_unknown() != cb.is_unknown() || (!ca.is_unknown() && !ca.same_as(&cb)) {
        return Err(Error::Crs("layer algebra requires matching CRS; reproject first".into()));
    }
    let prepare = |layer: &Layer| -> Result<Vec<Set>> {
        layer
            .features
            .iter()
            .map(|f| {
                if let Some(g) = &f.geometry {
                    require_valid(g, f.fid)?;
                }
                let mut set = f.geometry.as_ref().map_or_else(Set::default, Set::from_geometry);
                // A collection is a union of its components. Flattening overlapping
                // polygons into one even-odd mask would subtract their shared interior.
                if f.geometry.as_ref().is_some_and(|g| matches!(g, Geometry::GeometryCollection(_)))
                    && set.area.0.len() > 1
                    && polygon_components_overlap(&set.area)
                {
                    set.area = super::vector_clip::union_regions(std::slice::from_ref(&set.area))?;
                    set.collection = false;
                }
                Ok(set)
            })
            .collect()
    };
    let left = prepare(a)?;
    let right = prepare(b)?;
    let index = RTree::bulk_load(
        right
            .iter()
            .enumerate()
            .filter_map(|(i, g)| {
                g.bounds()
                    .map(|r| GeomWithData::new(Rectangle::from_aabb(AABB::from_corners([r.min().x, r.min().y], [r.max().x, r.max().y])), i))
            })
            .collect(),
    );
    let both = matches!(operation, Op::Intersection | Op::Union | Op::SymmetricDifference | Op::Identity);
    let mut out = Layer::new(a.name.clone());
    out.crs = a.crs.clone();
    let input_prefix = options.input_prefix.as_deref().unwrap_or(if both { "input_" } else { "" });
    let method_prefix = options.method_prefix.as_deref().unwrap_or(if both { "method_" } else { "" });
    for (prefix, layer) in [(input_prefix, a), (method_prefix, b)].into_iter().take(if both { 2 } else { 1 }) {
        for field in layer.schema.fields() {
            let mut field = field.clone();
            field.name = super::geometry::unique_field(&out, &format!("{prefix}{}", field.name));
            out.schema.add_field(field);
        }
    }
    let attrs = |layer: &Layer, f: &Feature| {
        (0..layer.schema.len()).map(|i| f.attributes.get(i).cloned().unwrap_or(FieldValue::Null)).collect::<Vec<_>>()
    };
    let input_types: Vec<_> = (0..a.schema.len()).map(|i| super::vector_schema::ogr_type(a, i)).collect();
    let method_types: Vec<_> = (0..b.schema.len()).map(|i| super::vector_schema::ogr_type(b, i)).collect();
    let mut emit = |g: Set, ia: Option<usize>, ib: Option<usize>| -> Result<()> {
        let geometry = if operation == Op::Update && ia.is_none() {
            ib.and_then(|j| b.features[j].geometry.clone()).map(|g| super::vector_geom::set_dimensions(g, false, false))
        } else {
            g.geometry()?
        };
        let Some(geometry) = geometry else { return Ok(()) };
        let mut values = if let Some(i) = ia {
            attrs(a, &a.features[i])
        } else if !both && operation == Op::Update {
            out.schema
                .fields()
                .iter()
                .enumerate()
                .map(|(i, field)| {
                    ib.and_then(|feature| b.schema.field_index(&field.name).map(|j| (feature, j)))
                        .and_then(|(feature, j)| {
                            b.features[feature]
                                .attributes
                                .get(j)
                                .map(|value| super::vector_schema::convert_value(value, field, method_types[j], input_types[i]))
                        })
                        .unwrap_or(FieldValue::Null)
                })
                .collect()
        } else {
            vec![FieldValue::Null; a.schema.len()]
        };
        if both {
            values.extend(ib.map_or_else(|| vec![FieldValue::Null; b.schema.len()], |i| attrs(b, &b.features[i])));
        }
        out.features.push(Feature { fid: out.features.len() as u64 + 1, geometry: Some(geometry), attributes: values });
        Ok(())
    };
    let mut right_masks = vec![Set::default(); right.len()];
    for (i, ga) in left.iter().enumerate() {
        let mut candidates: Vec<_> = ga.bounds().map_or_else(Vec::new, |r| {
            index
                .locate_in_envelope_intersecting(AABB::from_corners([r.min().x, r.min().y], [r.max().x, r.max().y]))
                .map(|e| e.data)
                .collect()
        });
        candidates.sort_unstable();
        let mut mask = Set::default();
        for j in candidates {
            let gb = &right[j];
            if matches!(operation, Op::Update | Op::Erase | Op::SymmetricDifference) {
                mask.extend(gb.clone());
                if operation == Op::SymmetricDifference {
                    right_masks[j].extend(ga.clone());
                }
                continue;
            }
            let mut intersection = ga.intersection(gb)?;
            if !options.keep_lower_dimension_geometries
                && operation != Op::Clip
                && intersection.dimension() < ga.dimension().min(gb.dimension())
            {
                intersection = Set::default();
            }
            if operation == Op::Clip {
                mask.extend(intersection.clone());
            } else if intersection.dimension() >= 0 {
                mask.extend(gb.clone());
            }
            if operation == Op::Union {
                right_masks[j].extend(ga.clone());
            }
            if matches!(operation, Op::Intersection | Op::Union | Op::Identity) {
                emit(intersection, Some(i), Some(j))?;
            }
        }
        if operation == Op::Clip {
            emit(mask, Some(i), None)?;
        } else if operation != Op::Intersection {
            emit(ga.difference(&mask)?, Some(i), None)?;
        }
    }
    if operation == Op::Update {
        for (j, g) in right.into_iter().enumerate() {
            emit(g, None, Some(j))?;
        }
    } else if matches!(operation, Op::Union | Op::SymmetricDifference) {
        for (j, g) in right.iter().enumerate() {
            emit(g.difference(&right_masks[j])?, None, Some(j))?;
        }
    }
    Ok(out)
}

#[derive(Clone)]
struct Set {
    area_mask: OnceLock<Arc<Mask>>,
    area: geo::MultiPolygon<f64>,
    lines: Vec<Vec<C>>,
    points: Vec<C>,
    multi: bool,
    collection: bool,
}
impl Default for Set {
    fn default() -> Self {
        Self { area_mask: OnceLock::new(), area: geo::MultiPolygon(vec![]), lines: vec![], points: vec![], multi: false, collection: false }
    }
}
impl Set {
    fn from_geometry(g: &Geometry) -> Self {
        let mut out = Self {
            multi: matches!(g, Geometry::MultiPoint(_) | Geometry::MultiLineString(_) | Geometry::MultiPolygon(_)),
            collection: matches!(g, Geometry::GeometryCollection(_)),
            ..Self::default()
        };
        match g {
            Geometry::Point(c) => out.points.push(xy(c)),
            Geometry::MultiPoint(cs) => out.points.extend(cs.iter().map(xy)),
            Geometry::LineString(cs) => out.lines.push(cs.iter().map(xy).collect()),
            Geometry::MultiLineString(ls) => out.lines.extend(ls.iter().map(|l| l.iter().map(xy).collect())),
            Geometry::Polygon { .. } | Geometry::MultiPolygon(_) => {
                if let geo::Geometry::MultiPolygon(mp) = to_geo(g) {
                    out.area = mp
                } else if let geo::Geometry::Polygon(p) = to_geo(g) {
                    out.area.0.push(p)
                }
            }
            Geometry::GeometryCollection(gs) => {
                for g in gs {
                    out.extend(Self::from_geometry(g));
                }
            }
        }
        out
    }
    fn extend(&mut self, other: Self) {
        if !other.area.0.is_empty() {
            self.area_mask.take();
        }
        self.area.0.extend(other.area.0);
        self.lines.extend(other.lines);
        self.points.extend(other.points);
    }
    fn dimension(&self) -> i8 {
        if !self.area.0.is_empty() {
            2
        } else if !self.lines.is_empty() {
            1
        } else if !self.points.is_empty() {
            0
        } else {
            -1
        }
    }
    fn bounds(&self) -> Option<geo::Rect<f64>> {
        geo::GeometryCollection(self.parts().into_iter().map(|g| to_geo(&g)).collect()).bounding_rect()
    }
    fn segments(&self) -> Vec<Line> {
        self.area
            .iter()
            .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
            .flat_map(|l| l.lines())
            .chain(self.lines.iter().flat_map(|l| l.windows(2).map(|p| Line::new(p[0], p[1]))))
            .collect()
    }
    fn area_mask(&self) -> &Mask {
        self.area_mask.get_or_init(|| Arc::new(Mask::new(self.area.clone()))).as_ref()
    }
    fn covers(&self, c: C) -> bool {
        self.area_mask().covers(c)
            || self.lines.iter().flat_map(|l| l.windows(2)).any(|p| Line::new(p[0], p[1]).intersects(&geo::Point(c)))
            || self.points.contains(&c)
    }
    fn intersection(&self, other: &Self) -> Result<Self> {
        let mut out = Self::default();
        if !other.area.0.is_empty() {
            let mask = Mask::new(other.area.clone());
            for part in self.parts() {
                let mut parts = vec![];
                mask.intersection(&part, true, &mut parts)?;
                for p in parts {
                    out.extend(Self::from_geometry(&p));
                }
            }
        }
        if !self.area.0.is_empty() {
            let mask = Mask::new(self.area.clone());
            for l in &other.lines {
                let mut parts = vec![];
                mask.intersection(&Geometry::LineString(l.iter().copied().map(coord).collect()), true, &mut parts)?;
                for p in parts {
                    out.extend(Self::from_geometry(&p));
                }
            }
        }
        let linear = Self { area: geo::MultiPolygon(vec![]), lines: other.lines.clone(), ..Self::default() };
        for l in &self.lines {
            let (lines, points) = cut_line(l, &linear, true);
            out.lines.extend(lines);
            out.points.extend(points);
        }
        for &p in &self.points {
            if other.covers(p) {
                out.points.push(p);
            }
        }
        for &p in &other.points {
            if self.covers(p) {
                out.points.push(p);
            }
        }
        Ok(out)
    }
    fn difference(&self, other: &Self) -> Result<Self> {
        // Subtract original polygons separately. Unioning a larger mask first
        // changes its quantization grid and can leave artificial boundary slivers.
        let mut area = self.area.clone();
        for p in &other.area.0 {
            area.0.retain(|source| source != p);
            if area.0.is_empty() {
                break;
            }
            let mask = geo::MultiPolygon(vec![p.clone()]);
            if !area.intersection(&mask).0.is_empty() {
                area = area.difference(&mask);
            }
        }
        let mask = other.normalized()?;
        let mut out = Self { area, ..Self::default() };
        for l in &self.lines {
            out.lines.extend(cut_line(l, &mask, false).0);
        }
        out.points.extend(self.points.iter().copied().filter(|p| !mask.covers(*p)));
        out.multi = self.multi && other.dimension() < 0;
        out.collection = self.collection
            && (other.dimension() < 0
                || (self.area.0.len() + self.lines.len() + self.points.len() == 1
                    && out.area == self.area
                    && out.lines == self.lines
                    && out.points == self.points));
        Ok(out)
    }
    fn normalized(&self) -> Result<Self> {
        let area = if self.area.0.len() > 1 { union(std::iter::once(&self.area)) } else { self.area.clone() };
        let mut out = Self { area, multi: self.multi, collection: self.collection, ..Self::default() };
        for l in &self.lines {
            let pieces = cut_line(l, &out, false).0;
            out.lines.extend(pieces);
        }
        for &p in &self.points {
            if !out.covers(p) {
                out.points.push(p);
            }
        }
        Ok(out)
    }
    fn parts(&self) -> Vec<Geometry> {
        let mut parts = super::vector_filter::explode(&from_polygons(self.area.clone()));
        parts.retain(|g| !g.all_coords().is_empty());
        parts.extend(self.lines.iter().filter(|l| l.len() > 1).map(|l| Geometry::LineString(l.iter().copied().map(coord).collect())));
        parts.extend(self.points.iter().copied().map(|p| Geometry::Point(coord(p))));
        parts
    }
    fn geometry(&self) -> Result<Option<Geometry>> {
        let normalized = self.normalized()?;
        let mut parts = normalized.parts();
        if normalized.collection && !parts.is_empty() {
            let geometry = Geometry::GeometryCollection(parts);
            require_valid(&geometry, 0)?;
            return Ok(Some(geometry));
        }
        let geometry = match parts.len() {
            0 => return Ok(None),
            1 => match parts.pop().unwrap() {
                Geometry::Point(c) if normalized.multi => Geometry::MultiPoint(vec![c]),
                Geometry::LineString(l) if normalized.multi => Geometry::MultiLineString(vec![l]),
                Geometry::Polygon { exterior, interiors } if normalized.multi => Geometry::MultiPolygon(vec![(exterior, interiors)]),
                g => g,
            },
            _ => {
                if normalized.lines.is_empty() && normalized.points.is_empty() {
                    from_polygons(normalized.area)
                } else if normalized.area.0.is_empty() && normalized.points.is_empty() {
                    Geometry::MultiLineString(normalized.lines.into_iter().map(|l| l.into_iter().map(coord).collect()).collect())
                } else if normalized.area.0.is_empty() && normalized.lines.is_empty() {
                    Geometry::MultiPoint(normalized.points.into_iter().map(coord).collect())
                } else {
                    Geometry::GeometryCollection(parts)
                }
            }
        };
        require_valid(&geometry, 0)?;
        Ok(Some(geometry))
    }
}
/// Unary dissolve within each feature, preserving rows and attributes. Unlike
/// layer grouping, this unions multipart components and nodes/merges linework.
pub(super) fn dissolve_features(layer: &Layer) -> Result<Layer> {
    let mut out = layer.clone();
    out.geom_type = None;
    for feature in &mut out.features {
        let Some(geometry) = feature.geometry.take() else { continue };
        for part in super::vector_filter::explode(&geometry) {
            require_valid(&part, feature.fid)?;
        }
        let mut set = Set::from_geometry(&geometry);
        if !set.area.0.is_empty() {
            set.area = super::vector_clip::union_regions(&[set.area])?;
        }
        let area = Set { area: set.area.clone(), ..Default::default() };
        set.lines =
            if area.area.0.is_empty() { set.lines } else { set.lines.iter().flat_map(|line| cut_line(line, &area, false).0).collect() };
        set.lines = merge_noded_lines(&set.lines);
        let points = std::mem::take(&mut set.points);
        for point in points {
            if !set.covers(point) {
                set.points.push(point);
            }
        }
        set.multi = false;
        set.collection = false;
        feature.geometry = Some(set.geometry()?.unwrap_or_else(|| Geometry::GeometryCollection(vec![])));
    }
    Ok(out)
}

fn merge_noded_lines(lines: &[Vec<C>]) -> Vec<Vec<C>> {
    use std::collections::BTreeMap;
    type PointKey = (u64, u64);
    let key = |p: C| -> PointKey { (if p.x == 0. { 0 } else { p.x.to_bits() }, if p.y == 0. { 0 } else { p.y.to_bits() }) };
    let segments: Vec<_> = lines.iter().flat_map(|line| line.windows(2).map(|pair| [pair[0], pair[1]])).collect();
    let bounds = |edge: &[C; 2]| {
        AABB::from_corners([edge[0].x.min(edge[1].x), edge[0].y.min(edge[1].y)], [edge[0].x.max(edge[1].x), edge[0].y.max(edge[1].y)])
    };
    let index =
        RTree::bulk_load(segments.iter().enumerate().map(|(i, edge)| GeomWithData::new(Rectangle::from_aabb(bounds(edge)), i)).collect());
    let mut unique = BTreeMap::new();
    for segment in &segments {
        let mask = Set {
            lines: index.locate_in_envelope_intersecting(bounds(segment)).map(|entry| segments[entry.data].to_vec()).collect(),
            ..Default::default()
        };
        for piece in cut_line(segment, &mask, true).0 {
            for pair in piece.windows(2) {
                let a = key(pair[0]);
                let b = key(pair[1]);
                if a != b {
                    unique.entry(if a < b { (a, b) } else { (b, a) }).or_insert([pair[0], pair[1]]);
                }
            }
        }
    }
    let edges: Vec<_> = unique.into_values().collect();
    let mut adjacency: BTreeMap<PointKey, Vec<usize>> = BTreeMap::new();
    for (i, edge) in edges.iter().enumerate() {
        for point in edge {
            adjacency.entry(key(*point)).or_default().push(i);
        }
    }
    let mut used = vec![false; edges.len()];
    let walk = |start: C, first: usize, used: &mut [bool]| {
        let mut result = vec![start];
        let mut current = start;
        let mut edge = first;
        loop {
            used[edge] = true;
            let ends = edges[edge];
            current = if key(ends[0]) == key(current) { ends[1] } else { ends[0] };
            result.push(current);
            let incident = &adjacency[&key(current)];
            if incident.len() != 2 {
                break;
            }
            let Some(next) = incident.iter().copied().find(|i| !used[*i]) else { break };
            edge = next;
        }
        result
    };
    let mut output = vec![];
    for edge in &edges {
        for start in edge {
            let incident = &adjacency[&key(*start)];
            if incident.len() != 2 {
                for &i in incident {
                    if !used[i] {
                        output.push(walk(*start, i, &mut used));
                    }
                }
            }
        }
    }
    for (i, edge) in edges.iter().enumerate() {
        if !used[i] {
            output.push(walk(edge[0], i, &mut used));
        }
    }
    output
}

fn xy(c: &Coord) -> C {
    geo::coord! {x:c.x,y:c.y}
}
fn coord(c: C) -> Coord {
    Coord::xy(c.x, c.y)
}
fn fraction(l: Line, p: C) -> f64 {
    let d = l.end - l.start;
    if d.x.abs() >= d.y.abs() { (p.x - l.start.x) / d.x } else { (p.y - l.start.y) / d.y }
}
fn cut_line(line: &[C], mask: &Set, inside: bool) -> (Vec<Vec<C>>, Vec<C>) {
    let edges = mask.segments();
    let area = mask.area_mask();
    let envelope = |line: Line| {
        AABB::from_corners(
            [line.start.x.min(line.end.x), line.start.y.min(line.end.y)],
            [line.start.x.max(line.end.x), line.start.y.max(line.end.y)],
        )
    };
    let index =
        RTree::bulk_load(edges.iter().enumerate().map(|(i, line)| GeomWithData::new(Rectangle::from_aabb(envelope(*line)), i)).collect());
    let mut lines: Vec<Vec<C>> = vec![];
    let mut contacts = vec![];
    for pair in line.windows(2) {
        if pair[0] == pair[1] {
            continue;
        }
        let source = Line::new(pair[0], pair[1]);
        let mut candidates: Vec<_> = index.locate_in_envelope_intersecting(envelope(source)).map(|entry| entry.data).collect();
        candidates.sort_unstable();
        let covered_endpoint = |p: C| area.covers(p) || candidates.iter().any(|&i| edges[i].intersects(&geo::Point(p)));
        let mut cuts = vec![(0., pair[0], covered_endpoint(pair[0])), (1., pair[1], covered_endpoint(pair[1]))];
        let mut intervals = vec![];
        for &i in &candidates {
            let edge = edges[i];
            if let Some(hit) = line_intersection(source, edge) {
                match hit {
                    LineIntersection::SinglePoint { intersection: p, .. } => cuts.push((fraction(source, p), p, true)),
                    LineIntersection::Collinear { intersection: l } => {
                        let a = fraction(source, l.start);
                        let b = fraction(source, l.end);
                        intervals.push((a.min(b), a.max(b)));
                        cuts.push((a, l.start, true));
                        cuts.push((b, l.end, true));
                    }
                }
            }
        }
        cuts.sort_by(|a, b| a.0.total_cmp(&b.0));
        // Preserve contact provenance: re-testing a computed intersection with a
        // robust orientation predicate can reject its rounded coordinates.
        cuts.dedup_by(|a, b| {
            if a.0 == b.0 {
                b.2 |= a.2;
                true
            } else {
                false
            }
        });
        for w in cuts.windows(2) {
            let a = w[0].1;
            let b = w[1].1;
            let t = w[0].0 * 0.5 + w[1].0 * 0.5;
            let mid = C { x: a.x * 0.5 + b.x * 0.5, y: a.y * 0.5 + b.y * 0.5 };
            let covered = area.covers(mid) || intervals.iter().any(|&(a, b)| a <= t && t <= b);
            if covered == inside {
                if let Some(last) = lines.last_mut().filter(|l| l.last() == Some(&a) && !w[0].2) {
                    last.push(b)
                } else {
                    lines.push(vec![a, b]);
                }
            }
        }
        if inside {
            contacts.extend(cuts.into_iter().filter(|c| c.2).map(|c| c.1));
        }
    }
    // Interval endpoints already belong to the emitted line pieces. Comparing
    // against their segment endpoints avoids another rounded point-on-line test.
    contacts.retain(|p| !lines.iter().any(|l| l.contains(p)));
    (lines, contacts)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn layer(name: &str, geometry: Geometry, value: i64) -> Layer {
        let mut out = Layer::new(name).with_epsg(5070);
        out.schema.add_field(crate::FieldDef::new("id", crate::FieldType::Integer));
        out.features.push(Feature { fid: 42, geometry: Some(geometry), attributes: vec![FieldValue::Integer(value)] });
        out
    }
    fn line(a: (f64, f64), b: (f64, f64)) -> Geometry {
        Geometry::LineString(vec![Coord::xy(a.0, a.1), Coord::xy(b.0, b.1)])
    }
    #[test]
    fn expanded_regions_cover_new_points_without_changing_existing_snapshots() {
        let polygon = |x: f64| Geometry::Polygon {
            exterior: wbvector::Ring(vec![
                Coord::xy(x, 0.),
                Coord::xy(x + 2., 0.),
                Coord::xy(x + 2., 2.),
                Coord::xy(x, 2.),
                Coord::xy(x, 0.),
            ]),
            interiors: vec![],
        };
        let mut region = Set::from_geometry(&polygon(0.));
        assert!(region.covers(geo::coord! {x:1.,y:1.}));
        assert!(!region.covers(geo::coord! {x:11.,y:1.}));
        let snapshot = region.clone();
        region.extend(Set::from_geometry(&polygon(10.)));
        assert!(region.covers(geo::coord! {x:1.,y:1.}));
        assert!(region.covers(geo::coord! {x:11.,y:1.}));
        assert!(!snapshot.covers(geo::coord! {x:11.,y:1.}));
    }
    #[test]
    fn crossing_lines_respect_lower_dimension_and_preserve_schema() {
        let a = layer("a", line((0., 0.), (3., 3.)), 1);
        let b = layer("b", line((0., 3.), (3., 0.)), 2);
        let out = layer_algebra(&a, &b, LayerAlgebraOperation::Identity, &Default::default()).unwrap();
        assert_eq!(out.features.len(), 2);
        assert!(matches!(out.features[0].geometry, Some(Geometry::Point(_))));
        assert!(matches!(out.features[1].geometry, Some(Geometry::MultiLineString(_))));
        assert_eq!(out.features[0].attributes, vec![FieldValue::Integer(1), FieldValue::Integer(2)]);
        let options = LayerAlgebraOptions { keep_lower_dimension_geometries: false, ..Default::default() };
        let out = layer_algebra(&a, &b, LayerAlgebraOperation::Identity, &options).unwrap();
        assert_eq!(out.features.len(), 1);
        assert!(matches!(out.features[0].geometry, Some(Geometry::LineString(_))));
    }
    #[test]
    fn update_maps_method_values_by_name_and_erases_line_intersections() {
        let a = layer("a", line((0., 0.), (3., 3.)), 1);
        let b = layer("b", line((0., 3.), (3., 0.)), 2);
        let out = layer_algebra(&a, &b, LayerAlgebraOperation::Update, &Default::default()).unwrap();
        assert_eq!(out.schema.fields()[0].name, "id");
        assert_eq!(out.features.len(), 2);
        assert!(matches!(out.features[0].geometry, Some(Geometry::MultiLineString(_))));
        assert_eq!(out.features[1].attributes, vec![FieldValue::Integer(2)]);
        assert_eq!(out.features.iter().map(|f| f.fid).collect::<Vec<_>>(), vec![1, 2]);
    }
    #[test]
    fn update_converts_method_values_to_the_input_schema() {
        let a = layer("a", Geometry::Point(Coord::xy(0., 0.)), 1);
        let mut b = layer("b", Geometry::Point(Coord::xy(2., 2.)), 2);
        b.schema = Default::default();
        b.schema.add_field(crate::FieldDef::new("id", crate::FieldType::Text));
        b.features[0].attributes[0] = FieldValue::Text("42".into());
        let out = layer_algebra(&a, &b, LayerAlgebraOperation::Update, &Default::default()).unwrap();
        assert_eq!(out.features[1].attributes, vec![FieldValue::Integer(42)]);
    }
    #[test]
    fn empty_and_null_inputs_emit_nothing() {
        let mut a = layer("a", Geometry::MultiPoint(vec![]), 1);
        a.features.push(Feature { fid: 43, geometry: None, attributes: vec![] });
        let b = layer("b", Geometry::Point(Coord::xy(1., 2.)), 2);
        let out = layer_algebra(&a, &b, LayerAlgebraOperation::Clip, &Default::default()).unwrap();
        assert!(out.features.is_empty());
        assert_eq!(out.schema.len(), 1);
    }
    #[test]
    fn invalid_geometry_and_mismatched_crs_are_rejected_without_mutation() {
        let a = layer("a", Geometry::Point(Coord::xy(0., 0.)), 1);
        let mut b = layer("b", Geometry::Point(Coord::xy(0., 0.)), 2);
        b.crs = Layer::new("other").with_epsg(4326).crs;
        assert!(layer_algebra(&a, &b, LayerAlgebraOperation::Union, &Default::default()).is_err());
        b.crs = a.crs.clone();
        b.features[0].geometry = Some(Geometry::Point(Coord::xy(f64::NAN, 0.)));
        assert!(layer_algebra(&a, &b, LayerAlgebraOperation::Union, &Default::default()).is_err());
        assert_eq!(a.features[0].fid, 42);
    }
    #[test]
    fn rounded_diagonal_intersections_keep_provenance_and_node_vertices() {
        let g = Geometry::LineString(vec![Coord::xy(0.1, 0.2), Coord::xy(1.3, 2.7), Coord::xy(2.9, 4.1)]);
        let s = Set::from_geometry(&g);
        let result = s.intersection(&s).unwrap().geometry().unwrap().unwrap();
        assert!(matches!(result,Geometry::MultiLineString(ref ls) if ls.len()==2));
        assert!(s.difference(&s).unwrap().geometry().unwrap().is_none());
    }
    #[test]
    fn update_prefixes_follow_output_names_and_preserve_typed_empty_method() {
        let a = layer("a", Geometry::Point(Coord::xy(1., 1.)), 1);
        let b = layer("b", Geometry::Polygon { exterior: wbvector::Ring(vec![]), interiors: vec![] }, 2);
        let options = LayerAlgebraOptions { input_prefix: Some("a_".into()), ..Default::default() };
        let out = layer_algebra(&a, &b, LayerAlgebraOperation::Update, &options).unwrap();
        assert_eq!(out.features.len(), 2);
        assert_eq!(out.features[1].attributes, vec![FieldValue::Null]);
        assert!(matches!(out.features[1].geometry, Some(Geometry::Polygon { .. })));
    }
    #[test]
    fn overlapping_collection_components_are_unioned_for_point_queries() {
        let square = |x: f64| Geometry::Polygon {
            exterior: wbvector::Ring(vec![
                Coord::xy(x, 0.),
                Coord::xy(x + 2., 0.),
                Coord::xy(x + 2., 2.),
                Coord::xy(x, 2.),
                Coord::xy(x, 0.),
            ]),
            interiors: vec![],
        };
        let a = layer("a", Geometry::Point(Coord::xy(1.5, 1.)), 1);
        let b = layer("b", Geometry::GeometryCollection(vec![square(0.), square(1.)]), 2);
        for (a, b) in [(&a, &b), (&b, &a)] {
            let out = layer_algebra(a, b, LayerAlgebraOperation::Intersection, &Default::default()).unwrap();
            assert_eq!(out.features.len(), 1);
            assert!(matches!(out.features[0].geometry, Some(Geometry::Point(_))));
        }
    }
    #[test]
    fn untouched_disjoint_polygon_collections_keep_their_outer_type() {
        let polygon = |x: f64| Geometry::Polygon {
            exterior: wbvector::Ring(vec![
                Coord::xy(x, 0.),
                Coord::xy(x + 1., 0.),
                Coord::xy(x + 1., 1.),
                Coord::xy(x, 1.),
                Coord::xy(x, 0.),
            ]),
            interiors: vec![],
        };
        let a = layer("a", Geometry::GeometryCollection(vec![polygon(0.), polygon(3.)]), 1);
        let b = layer("b", polygon(10.), 2);
        for op in [LayerAlgebraOperation::Erase, LayerAlgebraOperation::Identity] {
            let result = layer_algebra(&a, &b, op, &Default::default()).unwrap();
            let Some(Geometry::GeometryCollection(parts)) = &result.features[0].geometry else {
                panic!("collection must remain a collection")
            };
            assert_eq!(parts.len(), 2);
            let actual = Set::from_geometry(result.features[0].geometry.as_ref().unwrap());
            let expected = Set::from_geometry(a.features[0].geometry.as_ref().unwrap());
            assert_eq!(actual.area.xor(&expected.area).unsigned_area(), 0.);
        }
    }
    #[test]
    fn touching_polygon_collections_do_not_become_multipolygons() {
        for x in [0., 0.1, 100_000_000.1] {
            let polygon = |left: f64| Geometry::Polygon {
                exterior: wbvector::Ring(vec![
                    Coord::xy(left, 0.1),
                    Coord::xy(left + 1., 0.1),
                    Coord::xy(left + 1., 1.1),
                    Coord::xy(left, 1.1),
                    Coord::xy(left, 0.1),
                ]),
                interiors: vec![],
            };
            let a = layer("a", Geometry::GeometryCollection(vec![polygon(x), polygon(x + 1.)]), 1);
            let b = layer("b", polygon(-10.), 2);
            assert!(!polygon_components_overlap(&Set::from_geometry(a.features[0].geometry.as_ref().unwrap()).area));
            for op in [LayerAlgebraOperation::Erase, LayerAlgebraOperation::Identity] {
                let result = layer_algebra(&a, &b, op, &Default::default()).unwrap();
                assert!(matches!(result.features[0].geometry, Some(Geometry::GeometryCollection(_))));
            }
        }
    }
    #[test]
    fn untouched_single_part_collections_keep_their_outer_type() {
        let a = layer("a", Geometry::GeometryCollection(vec![Geometry::Point(Coord::xy(1., 1.))]), 1);
        let b = layer("b", Geometry::Point(Coord::xy(3., 3.)), 2);
        for op in
            [LayerAlgebraOperation::Identity, LayerAlgebraOperation::Erase, LayerAlgebraOperation::Update, LayerAlgebraOperation::Union]
        {
            let out = layer_algebra(&a, &b, op, &Default::default()).unwrap();
            assert!(matches!(out.features[0].geometry, Some(Geometry::GeometryCollection(_))));
        }
        let b = layer(
            "b",
            Geometry::Polygon {
                exterior: wbvector::Ring(vec![
                    Coord::xy(0., 0.),
                    Coord::xy(4., 0.),
                    Coord::xy(4., 4.),
                    Coord::xy(0., 4.),
                    Coord::xy(0., 0.),
                ]),
                interiors: vec![wbvector::Ring(vec![
                    Coord::xy(0.5, 0.5),
                    Coord::xy(0.5, 1.5),
                    Coord::xy(1.5, 1.5),
                    Coord::xy(1.5, 0.5),
                    Coord::xy(0.5, 0.5),
                ])],
            },
            2,
        );
        let out = layer_algebra(&a, &b, LayerAlgebraOperation::Erase, &Default::default()).unwrap();
        assert!(matches!(out.features[0].geometry, Some(Geometry::GeometryCollection(_))));
    }
}

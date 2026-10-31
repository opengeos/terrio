//! Polygon clipping for vector features, with boundary-inclusive XY predicates.
//!
//! Clipping never reprojects the input. Only an explicitly referenced clip region is
//! transformed into the input layer's CRS. Points retain their coordinates; line cuts
//! interpolate source Z/M. Polygon topology is constructed in XY.
use crate::crs::SpatialRef;
use crate::{Error, Feature, Geometry, GeometryType, Layer, RasterDataset, Result};
use geo::algorithm::kernels::{Kernel, Orientation, RobustKernel};
use geo::algorithm::line_intersection::{LineIntersection, line_intersection};
use geo::{Area, BooleanOps, BoundingRect, CoordsIter, Intersects};
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};
use std::collections::HashSet;
use wbvector::{Coord, Ring};

use super::topology::{from_polygons, polygons, require_valid, union};

/// A polygonal clipping region. An absent CRS means the input layer's coordinates.
#[derive(Debug, Clone)]
pub struct ClipRegion {
    /// Polygon or multipolygon; holes are honored. An empty multipolygon clips everything out.
    pub geometry: Geometry,
    /// Coordinate reference of the region, when different from the input's.
    pub crs: Option<SpatialRef>,
}

/// Selection and attribute/SQL filtering of a vector clipping template.
#[derive(Debug, Clone, Default)]
pub struct ClipTemplateOptions {
    /// Select a template layer; otherwise exactly one layer is required after SQL.
    pub layer: Option<String>,
    /// OGR SQL WHERE subset supported by vector conversion; exclusive with SQL.
    pub where_clause: Option<String>,
    /// SELECT subset supported by vector conversion; exclusive with WHERE.
    pub sql: Option<String>,
}

impl ClipRegion {
    /// Constructs a finite, nonempty axis-aligned rectangle, in GIS XY axis order.
    pub fn bbox(bounds: [f64; 4], crs: Option<SpatialRef>) -> Result<Self> {
        let [x0, y0, x1, y1] = bounds;
        if bounds.iter().any(|v| !v.is_finite()) || x0 >= x1 || y0 >= y1 {
            return Err(Error::InvalidArgument("clip bbox requires finite xmin < xmax and ymin < ymax".into()));
        }
        Ok(Self {
            geometry: Geometry::Polygon {
                exterior: Ring(vec![Coord::xy(x0, y0), Coord::xy(x1, y0), Coord::xy(x1, y1), Coord::xy(x0, y1), Coord::xy(x0, y0)]),
                interiors: vec![],
            },
            crs,
        })
    }

    /// Reads a polygon/multipolygon as WKT or GeoJSON. GeoJSON defaults to EPSG:4326,
    /// including a legacy embedded CRS when present; WKT defaults to the input layer's CRS.
    pub fn parse(text: &str, crs: Option<SpatialRef>) -> Result<Self> {
        let json = text.trim_start().starts_with('{');
        let (bytes, name) = if json {
            (text.as_bytes().to_vec(), "region.geojson")
        } else {
            (format!("WKT\n\"{}\"\n", text.replace('"', "\"\"")).into_bytes(), "region.csv")
        };
        let ds = crate::memory::open_vector_bytes(bytes, name, &Default::default())?;
        if ds.layers.len() != 1 || ds.layers[0].features.len() != 1 {
            return Err(Error::InvalidArgument("clip geometry must contain exactly one polygonal feature".into()));
        }
        let layer = &ds.layers[0];
        let geometry = layer.features[0].geometry.clone().ok_or_else(|| Error::InvalidArgument("null clip geometry".into()))?;
        require_valid(&geometry, 0)?;
        polygons(&geometry, 0)?;
        let crs = crs.or_else(|| json.then(|| SpatialRef::from_layer(layer)));
        Ok(Self { geometry, crs })
    }

    /// Unions all polygonal features of a template layer. Nulls are ignored; invalid
    /// or non-polygonal features are rejected. Overlapping template polygons do not
    /// duplicate clipped features.
    pub fn from_layer(layer: &Layer) -> Result<Self> {
        let mut shapes = Vec::new();
        for f in &layer.features {
            if let Some(g) = &f.geometry {
                require_valid(g, f.fid)?;
                shapes.push(polygons(g, f.fid)?);
            }
        }
        let geometry = match shapes.as_slice() {
            [] => Geometry::MultiPolygon(vec![]),
            [shape] => from_polygons(shape.clone()),
            _ if shapes.iter().all(|s| s == &shapes[0]) => from_polygons(shapes[0].clone()),
            _ => from_polygons(union_regions(&shapes)?),
        };
        let crs = SpatialRef::from_layer(layer);
        Ok(Self { geometry, crs: (!crs.is_unknown()).then_some(crs) })
    }

    /// Selects/filters a vector template and unions its polygonal features.
    pub fn from_layers(layers: &[Layer], options: &ClipTemplateOptions) -> Result<Self> {
        if options.where_clause.is_some() && options.sql.is_some() {
            return Err(Error::InvalidArgument("like_where and like_sql are mutually exclusive".into()));
        }
        let selected = if let Some(name) = &options.layer {
            vec![
                layers
                    .iter()
                    .find(|l| &l.name == name)
                    .ok_or_else(|| Error::InvalidArgument(format!("no template layer named '{name}'")))?
                    .clone(),
            ]
        } else {
            layers.to_vec()
        };
        let selected = super::vector_filter::translate(
            &selected,
            &super::vector_filter::VectorTranslateOptions {
                where_clause: options.where_clause.clone(),
                sql: options.sql.clone(),
                ..Default::default()
            },
        )?;
        if selected.len() != 1 {
            return Err(Error::InvalidArgument("clip template needs one vector layer; select it with like_layer or like_sql".into()));
        }
        Self::from_layer(&selected[0])
    }

    /// Opens a native/remote template, using metadata-only raster reads where the driver
    /// supports lazy access. Vector selection uses the same options as in-memory templates.
    #[cfg(not(target_family = "wasm"))]
    pub fn from_path(path: &str, options: &ClipTemplateOptions) -> Result<Self> {
        let kind = crate::inspection::identify(path, false)?;
        let may_raster = kind.as_ref().is_some_and(|i| i.kinds.contains(&crate::formats::Kind::Raster));
        let raster_only = may_raster && kind.as_ref().is_some_and(|i| !i.kinds.contains(&crate::formats::Kind::Vector));
        let vector_options = options.layer.is_some() || options.where_clause.is_some() || options.sql.is_some();
        if raster_only {
            if vector_options {
                return Err(Error::InvalidArgument("like_layer/where/sql require a vector template".into()));
            }
            return Self::from_raster(&crate::open_raster_lazy(path)?.header());
        }
        match super::vector_filter::open_layers(path, &Default::default()) {
            Ok(layers) if layers.is_empty() && may_raster && !vector_options => Self::from_raster(&crate::open_raster_lazy(path)?.header()),
            Ok(layers) => Self::from_layers(&layers, options),
            Err(vector_error) if may_raster && !vector_options => match crate::open_raster_lazy(path) {
                Ok(raster) => Self::from_raster(&raster.header()),
                Err(_) => Err(vector_error),
            },
            Err(error) => Err(error),
        }
    }

    /// The rectangular bounds of a raster template, including rotated affine grids.
    /// No raster pixels are needed.
    pub fn from_raster(source: &RasterDataset) -> Result<Self> {
        let gt = source.geo_transform();
        let mut bounds = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
        for (x, y) in [
            (0., 0.),
            (source.raster.cols as f64, 0.),
            (0., source.raster.rows as f64),
            (source.raster.cols as f64, source.raster.rows as f64),
        ] {
            let (x, y) = (gt[0] + gt[1] * x + gt[2] * y, gt[3] + gt[4] * x + gt[5] * y);
            bounds[0] = bounds[0].min(x);
            bounds[1] = bounds[1].min(y);
            bounds[2] = bounds[2].max(x);
            bounds[3] = bounds[3].max(y);
        }
        Self::bbox(bounds, (!source.crs().is_unknown()).then(|| source.crs()))
    }
}

/// Clips features to a region, preserving attributes, schema, layer name and CRS.
/// Null/empty results are omitted. Single-type layers split multipart results and
/// omit lower-dimensional contacts; multi-type layers retain their declared type.
/// Unspecified/collection layers retain boundary-only line/point intersections.
/// Output feature IDs are sequential from 1, including after multipart splitting.
/// Invalid geometries and explicit known/unknown CRS mismatches are errors.
/// Region vertices are reprojected without densification; edges are straight in
/// the input CRS after transformation, matching GDAL vector clipping.
pub fn clip(layer: &Layer, region: &ClipRegion) -> Result<Layer> {
    require_valid(&region.geometry, 0)?;
    let target = SpatialRef::from_layer(layer);
    let transformed = if let Some(crs) = &region.crs {
        if crs.is_unknown() != target.is_unknown() {
            return Err(Error::Crs("cannot transform a clipping region between known and unknown CRSs".into()));
        }
        if !crs.is_unknown() && !crs.same_as(&target) {
            let mut template = Layer::new("clip");
            crs.apply_to_layer(&mut template);
            template.features.push(Feature { fid: 1, geometry: Some(region.geometry.clone()), attributes: vec![] });
            Some(crate::vector::reproject_layer(&template, &target)?.features.remove(0).geometry.unwrap())
        } else {
            None
        }
    } else {
        None
    };
    let geometry = transformed.as_ref().unwrap_or(&region.geometry);
    // The original region was already checked above. Reprojection can change
    // topology, so only transformed geometry needs a second validation.
    if transformed.is_some() {
        require_valid(geometry, 0)?;
    }
    let mask = Mask::new(polygons(geometry, 0)?);
    let keep_lower = matches!(layer.geom_type, None | Some(GeometryType::GeometryCollection));
    let mut out = super::geometry::header(layer, layer.geom_type);
    for f in &layer.features {
        let Some(g) = &f.geometry else { continue };
        require_valid(g, f.fid)?;
        let mut parts = Vec::new();
        mask.intersection(g, keep_lower, &mut parts)?;
        for geometry in fit_type(parts, layer.geom_type) {
            let mut attributes = f.attributes.clone();
            attributes.resize(layer.schema.len(), crate::FieldValue::Null);
            out.features.push(Feature { fid: out.features.len() as u64 + 1, geometry: Some(geometry), attributes });
        }
    }
    Ok(out)
}

struct Mask {
    area: geo::MultiPolygon<f64>,
    bounds: Option<geo::Rect<f64>>,
    edges: Vec<geo::Line<f64>>,
    index: RTree<GeomWithData<Rectangle<[f64; 2]>, usize>>,
    vertices: RTree<[f64; 2]>,
}

fn envelope(line: geo::Line<f64>) -> AABB<[f64; 2]> {
    AABB::from_corners(
        [line.start.x.min(line.end.x), line.start.y.min(line.end.y)],
        [line.start.x.max(line.end.x), line.start.y.max(line.end.y)],
    )
}
fn xy(c: &Coord) -> geo::Coord<f64> {
    geo::coord! { x: c.x, y: c.y }
}
fn same(a: &Coord, b: &Coord) -> bool {
    a.x == b.x && a.y == b.y
}
fn point(c: &Coord) -> geo::Point<f64> {
    geo::Point(xy(c))
}
fn interpolate(a: &Coord, b: &Coord, t: f64, c: geo::Coord<f64>) -> Coord {
    if t == 0.0 {
        return a.clone();
    }
    if t == 1.0 {
        return b.clone();
    }
    Coord { x: c.x, y: c.y, z: a.z.zip(b.z).map(|(a, b)| a + t * (b - a)), m: a.m.zip(b.m).map(|(a, b)| a + t * (b - a)) }
}
fn fraction(line: geo::Line<f64>, c: geo::Coord<f64>) -> f64 {
    let dx = line.end.x - line.start.x;
    let dy = line.end.y - line.start.y;
    (if dx.abs() >= dy.abs() { (c.x - line.start.x) / dx } else { (c.y - line.start.y) / dy }).clamp(0.0, 1.0)
}

impl Mask {
    fn new(area: geo::MultiPolygon<f64>) -> Self {
        let edges: Vec<_> = area.iter().flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors())).flat_map(|r| r.lines()).collect();
        let index =
            RTree::bulk_load(edges.iter().enumerate().map(|(i, l)| GeomWithData::new(Rectangle::from_aabb(envelope(*l)), i)).collect());
        let bounds = area.bounding_rect();
        let vertices = RTree::bulk_load(area.coords_iter().map(|c| [c.x, c.y]).collect());
        Self { area, bounds, edges, index, vertices }
    }
    fn covers(&self, p: geo::Coord<f64>) -> bool {
        let Some(bounds) = self.bounds else {
            return false;
        };
        if p.x < bounds.min().x || p.x > bounds.max().x || p.y < bounds.min().y || p.y > bounds.max().y {
            return false;
        }
        let ray = AABB::from_corners([p.x, p.y], [bounds.max().x, p.y]);
        let mut inside = false;
        for entry in self.index.locate_in_envelope_intersecting(ray) {
            let edge = self.edges[entry.data];
            let orientation = RobustKernel::orient2d(edge.start, edge.end, p);
            if orientation == Orientation::Collinear
                && p.x >= edge.start.x.min(edge.end.x)
                && p.x <= edge.start.x.max(edge.end.x)
                && p.y >= edge.start.y.min(edge.end.y)
                && p.y <= edge.start.y.max(edge.end.y)
            {
                return true;
            }
            // Half-open vertical intervals count shared vertices only once.
            if (edge.start.y > p.y) != (edge.end.y > p.y)
                && ((edge.start.y < edge.end.y && orientation == Orientation::CounterClockwise)
                    || (edge.start.y > edge.end.y && orientation == Orientation::Clockwise))
            {
                inside = !inside;
            }
        }
        inside
    }
    fn edges_at(&self, line: geo::Line<f64>) -> impl Iterator<Item = geo::Line<f64>> + '_ {
        self.index.locate_in_envelope_intersecting(envelope(line)).map(|e| self.edges[e.data])
    }
    fn line(&self, coords: &[Coord], keep_lower: bool, out: &mut Vec<Geometry>) {
        let mut lines: Vec<Vec<Coord>> = Vec::new();
        let mut contacts = Vec::new();
        for pair in coords.windows(2) {
            let a = &pair[0];
            let b = &pair[1];
            if same(a, b) {
                continue;
            }
            let segment = geo::Line::new(xy(a), xy(b));
            let mut cuts = vec![(0.0, a.clone()), (1.0, b.clone())];
            for edge in self.edges_at(segment) {
                if let Some(hit) = line_intersection(segment, edge) {
                    let points = match hit {
                        LineIntersection::SinglePoint { intersection, .. } => vec![intersection],
                        LineIntersection::Collinear { intersection } => vec![intersection.start, intersection.end],
                    };
                    for c in points {
                        let t = fraction(segment, c);
                        cuts.push((t, interpolate(a, b, t, c)));
                    }
                }
            }
            cuts.sort_by(|a, b| a.0.total_cmp(&b.0));
            cuts.dedup_by(|a, b| a.0 == b.0);
            for cut in cuts.windows(2) {
                let mid = geo::Point::new(cut[0].1.x * 0.5 + cut[1].1.x * 0.5, cut[0].1.y * 0.5 + cut[1].1.y * 0.5);
                if self.covers(mid.0) {
                    let start = &cut[0].1;
                    let end = &cut[1].1;
                    if let Some(last) = lines.last_mut().filter(|l| same(l.last().unwrap(), start)) {
                        last.push(end.clone());
                    } else {
                        lines.push(vec![start.clone(), end.clone()]);
                    }
                }
            }
            if keep_lower {
                contacts.extend(cuts.into_iter().map(|(_, c)| c).filter(|c| self.covers(xy(c))));
            }
        }
        if !contacts.is_empty() {
            let segments: Vec<_> = lines.iter().flat_map(|l| l.windows(2)).map(|p| geo::Line::new(xy(&p[0]), xy(&p[1]))).collect();
            let segment_index = RTree::bulk_load(
                segments.iter().enumerate().map(|(i, l)| GeomWithData::new(Rectangle::from_aabb(envelope(*l)), i)).collect(),
            );
            let point_key = |c: &Coord| [if c.x == 0.0 { 0 } else { c.x.to_bits() }, if c.y == 0.0 { 0 } else { c.y.to_bits() }];
            let mut points: HashSet<_> =
                out.iter().filter_map(|g| if let Geometry::Point(c) = g { Some(point_key(c)) } else { None }).collect();
            for c in contacts {
                if !segment_index
                    .locate_in_envelope_intersecting(AABB::from_point([c.x, c.y]))
                    .any(|entry| segments[entry.data].intersects(&point(&c)))
                    && points.insert(point_key(&c))
                {
                    out.push(Geometry::Point(c));
                }
            }
        }
        out.extend(lines.into_iter().map(Geometry::LineString));
    }
    fn intersection(&self, g: &Geometry, keep_lower: bool, out: &mut Vec<Geometry>) -> Result<()> {
        match g {
            Geometry::Point(c) => {
                if self.covers(xy(c)) {
                    out.push(g.clone());
                }
            }
            Geometry::MultiPoint(cs) => {
                for c in cs {
                    self.intersection(&Geometry::Point(c.clone()), keep_lower, out)?;
                }
            }
            Geometry::LineString(cs) => self.line(cs, keep_lower, out),
            Geometry::MultiLineString(ls) => {
                for l in ls {
                    self.line(l, keep_lower, out);
                }
            }
            Geometry::Polygon { .. } | Geometry::MultiPolygon(_) => {
                let shape = polygons(g, 0).expect("validated polygon");
                let result = self.polygon_intersection(&shape)?;
                if keep_lower {
                    self.polygon_contacts(&shape, &result, out);
                }
                out.extend(super::vector_filter::explode(&from_polygons(result)).into_iter().filter(|g| !g.all_coords().is_empty()));
            }
            Geometry::GeometryCollection(gs) => {
                for g in gs {
                    self.intersection(g, keep_lower, out)?;
                }
            }
        }
        Ok(())
    }
    fn polygon_intersection(&self, shape: &geo::MultiPolygon<f64>) -> Result<geo::MultiPolygon<f64>> {
        let result = shape.intersection(&self.area);
        if result.0.is_empty() {
            return Ok(result);
        }
        // The polygon engine computes topology on an integer grid. Restore its
        // vertices from the original floating coordinates and robust segment
        // intersections, so shallow-angle cuts do not amplify grid rounding.
        // Every vertex of a linear polygon intersection belongs to this set.
        let source_index = Mask::new(shape.clone());
        let mut exact: Vec<[f64; 2]> = shape.coords_iter().filter(|c| self.covers(*c)).map(|c| [c.x, c.y]).collect();
        if let Some(bounds) = source_index.bounds {
            exact.extend(
                self.vertices
                    .locate_in_envelope_intersecting(AABB::from_corners([bounds.min().x, bounds.min().y], [bounds.max().x, bounds.max().y]))
                    .filter(|c| source_index.covers(geo::coord! {x:c[0],y:c[1]}))
                    .copied(),
            );
        }
        for ring in shape.iter().flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors())) {
            for line in ring.lines() {
                for edge in self.edges_at(line) {
                    match line_intersection(line, edge) {
                        Some(LineIntersection::SinglePoint { intersection: c, .. }) => exact.push([c.x, c.y]),
                        Some(LineIntersection::Collinear { intersection: l }) => {
                            exact.push([l.start.x, l.start.y]);
                            exact.push([l.end.x, l.end.y]);
                        }
                        None => {}
                    }
                }
            }
        }
        restore_polygon_vertices(result, exact)
    }

    fn polygon_contacts(&self, shape: &geo::MultiPolygon<f64>, area: &geo::MultiPolygon<f64>, out: &mut Vec<Geometry>) {
        let result_index = Mask::new(area.clone());
        let mut contacts = Vec::new();
        for ring in shape.iter().flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors())) {
            for line in ring.lines() {
                for edge in self.edges_at(line) {
                    if let Some(hit) = line_intersection(line, edge) {
                        match hit {
                            LineIntersection::SinglePoint { intersection: c, .. } => {
                                if !result_index.covers(c) {
                                    contacts.push(Geometry::Point(Coord::xy(c.x, c.y)));
                                }
                            }
                            LineIntersection::Collinear { intersection: l } => {
                                if !result_index.covers(geo::coord! {x:l.start.x * 0.5 + l.end.x * 0.5,y:l.start.y * 0.5 + l.end.y * 0.5}) {
                                    contacts.push(Geometry::LineString(vec![Coord::xy(l.start.x, l.start.y), Coord::xy(l.end.x, l.end.y)]));
                                }
                            }
                        }
                    }
                }
            }
        }
        let all_lines: Vec<_> = contacts.iter().filter_map(|g| if let Geometry::LineString(l) = g { Some(l) } else { None }).collect();
        let mut points = Vec::new();
        for g in &contacts {
            if let Geometry::Point(c) = g
                && !all_lines.iter().flat_map(|l| l.windows(2)).any(|p| geo::Line::new(xy(&p[0]), xy(&p[1])).intersects(&point(c)))
                && !points.iter().any(|p| same(p, c))
            {
                points.push(c.clone());
            }
        }
        out.extend(points.into_iter().map(Geometry::Point));
        out.extend(contacts.into_iter().filter(|g| matches!(g, Geometry::LineString(_))));
    }
}

fn union_regions(shapes: &[geo::MultiPolygon<f64>]) -> Result<geo::MultiPolygon<f64>> {
    let all = Mask::new(geo::MultiPolygon(shapes.iter().flat_map(|s| s.0.iter().cloned()).collect()));
    let mut exact: Vec<_> = all.area.coords_iter().map(|c| [c.x, c.y]).collect();
    for (i, line) in all.edges.iter().copied().enumerate() {
        for candidate in all.index.locate_in_envelope_intersecting(envelope(line)) {
            if candidate.data <= i {
                continue;
            }
            match line_intersection(line, all.edges[candidate.data]) {
                Some(LineIntersection::SinglePoint { intersection: c, .. }) => exact.push([c.x, c.y]),
                Some(LineIntersection::Collinear { intersection: l }) => {
                    exact.push([l.start.x, l.start.y]);
                    exact.push([l.end.x, l.end.y]);
                }
                None => {}
            }
        }
    }
    restore_polygon_vertices(union(shapes.iter()), exact)
}

fn restore_polygon_vertices(mut result: geo::MultiPolygon<f64>, exact: Vec<[f64; 2]>) -> Result<geo::MultiPolygon<f64>> {
    if result.0.is_empty() {
        return Ok(result);
    }
    if exact.is_empty() {
        return Err(Error::InvalidData("cannot reconstruct polygon clip vertices".into()));
    }
    let vertices = RTree::bulk_load(exact);
    let restore = |mut ring: geo::LineString<f64>| {
        for c in &mut ring.0 {
            let p = vertices.nearest_neighbor([c.x, c.y]).expect("nonempty vertex index");
            c.x = p[0];
            c.y = p[1];
        }
        ring.0.dedup();
        ring
    };
    result.0 = result
        .0
        .into_iter()
        .filter_map(|p| {
            let (outer, holes) = p.into_inner();
            let outer = restore(outer);
            if outer.0.len() < 4 || geo::Polygon::new(outer.clone(), vec![]).unsigned_area() == 0.0 {
                return None;
            }
            let holes = holes
                .into_iter()
                .map(restore)
                .filter(|r| r.0.len() >= 4 && geo::Polygon::new(r.clone(), vec![]).unsigned_area() > 0.0)
                .collect();
            Some(geo::Polygon::new(outer, holes))
        })
        .collect();
    require_valid(&from_polygons(result.clone()), 0)
        .map_err(|e| Error::InvalidData(format!("polygon clip topology could not be reconstructed at floating precision: {e}")))?;
    Ok(result)
}

fn fit_type(parts: Vec<Geometry>, ty: Option<GeometryType>) -> Vec<Geometry> {
    use GeometryType as T;
    if parts.is_empty() {
        return vec![];
    }
    match ty {
        Some(T::Point | T::LineString | T::Polygon) => parts.into_iter().filter(|g| Some(g.geom_type()) == ty).collect(),
        Some(T::MultiPoint) => {
            let cs = parts.into_iter().filter_map(|g| if let Geometry::Point(c) = g { Some(c) } else { None }).collect::<Vec<_>>();
            if cs.is_empty() { vec![] } else { vec![Geometry::MultiPoint(cs)] }
        }
        Some(T::MultiLineString) => {
            let ls = parts.into_iter().filter_map(|g| if let Geometry::LineString(l) = g { Some(l) } else { None }).collect::<Vec<_>>();
            if ls.is_empty() { vec![] } else { vec![Geometry::MultiLineString(ls)] }
        }
        Some(T::MultiPolygon) => {
            let ps = parts
                .into_iter()
                .filter_map(|g| if let Geometry::Polygon { exterior, interiors } = g { Some((exterior, interiors)) } else { None })
                .collect::<Vec<_>>();
            if ps.is_empty() { vec![] } else { vec![Geometry::MultiPolygon(ps)] }
        }
        Some(T::GeometryCollection) => vec![Geometry::GeometryCollection(parts)],
        None => {
            let ty = parts[0].geom_type();
            if parts.iter().all(|g| g.geom_type() == ty) {
                if parts.len() == 1 {
                    parts
                } else {
                    let multi = match ty {
                        T::Point => T::MultiPoint,
                        T::LineString => T::MultiLineString,
                        T::Polygon => T::MultiPolygon,
                        _ => T::GeometryCollection,
                    };
                    fit_type(parts, Some(multi))
                }
            } else {
                vec![Geometry::GeometryCollection(parts)]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FieldDef, FieldType, FieldValue};
    fn geometry(wkt: &str) -> Geometry {
        let bytes = format!("WKT\n\"{wkt}\"\n").into_bytes();
        crate::memory::open_vector_bytes(bytes, "g.csv", &Default::default()).unwrap().layers[0].features[0].geometry.clone().unwrap()
    }
    fn layer(wkt: &str, ty: Option<GeometryType>) -> Layer {
        let mut layer = Layer::new("source");
        layer.geom_type = ty;
        layer.schema.add_field(FieldDef::new("id", FieldType::Integer));
        layer.features.push(Feature { fid: 77, geometry: Some(geometry(wkt)), attributes: vec![FieldValue::Integer(5)] });
        layer
    }
    fn bbox() -> ClipRegion {
        ClipRegion::bbox([0., 0., 2., 2.], None).unwrap()
    }
    #[test]
    fn points_include_outer_and_hole_boundaries_but_not_hole_interiors() {
        let region = ClipRegion::parse("POLYGON ((0 0,4 0,4 4,0 4,0 0),(1 1,1 3,3 3,3 1,1 1))", None).unwrap();
        let input = layer("MULTIPOINT ((0 0),(1 2),(2 2),(4 4),(5 5))", Some(GeometryType::MultiPoint));
        let out = clip(&input, &region).unwrap();
        assert_eq!(out.features.len(), 1);
        assert_eq!(out.features[0].geometry.as_ref().unwrap().all_coords().len(), 3);
        assert_eq!(out.features[0].attributes, input.features[0].attributes);
        assert_eq!(out.schema.fields()[0].name, input.schema.fields()[0].name);
        assert_eq!(out.name, input.name);
    }
    #[test]
    fn lines_split_at_holes_and_honor_single_or_multi_layer_types() {
        let region = ClipRegion::parse("POLYGON ((0 0,4 0,4 4,0 4,0 0),(1 1,1 3,3 3,3 1,1 1))", None).unwrap();
        let mut input = layer("LINESTRING (-1 2,5 2)", Some(GeometryType::LineString));
        let out = clip(&input, &region).unwrap();
        assert_eq!(out.features.len(), 2);
        assert_eq!(out.features[0].geometry, Some(geometry("LINESTRING (0 2,1 2)")));
        assert_eq!(out.features[1].geometry, Some(geometry("LINESTRING (3 2,4 2)")));
        assert_eq!(out.features.iter().map(|f| f.fid).collect::<Vec<_>>(), vec![1, 2]);
        input.geom_type = Some(GeometryType::MultiLineString);
        let out = clip(&input, &region).unwrap();
        assert_eq!(out.features.len(), 1);
        assert!(matches!(out.features[0].geometry,Some(Geometry::MultiLineString(ref ls)) if ls.len()==2));
    }
    #[test]
    fn line_boundary_overlap_and_isolated_tangent_contacts() {
        let mut input = layer("LINESTRING (-1 0,3 0)", Some(GeometryType::LineString));
        let out = clip(&input, &bbox()).unwrap();
        assert_eq!(out.features[0].geometry, Some(geometry("LINESTRING (0 0,2 0)")));
        input.features[0].geometry = Some(geometry("LINESTRING (-1 1,1 -1)"));
        assert!(clip(&input, &bbox()).unwrap().features.is_empty());
        input.geom_type = None;
        assert_eq!(clip(&input, &bbox()).unwrap().features[0].geometry, Some(geometry("POINT (0 0)")));
    }
    #[test]
    fn line_cuts_interpolate_z_and_m_without_changing_retained_points() {
        let mut input = layer("LINESTRING (-1 1,3 1)", Some(GeometryType::LineString));
        input.features[0].geometry = Some(Geometry::LineString(vec![
            Coord { x: -1., y: 1., z: Some(0.), m: Some(100.) },
            Coord { x: 3., y: 1., z: Some(40.), m: Some(140.) },
        ]));
        let out = clip(&input, &bbox()).unwrap();
        let cs = out.features[0].geometry.as_ref().unwrap().all_coords();
        assert_eq!((cs[0].z, cs[0].m, cs[1].z, cs[1].m), (Some(10.), Some(110.), Some(30.), Some(130.)));
    }
    #[test]
    fn polygon_contacts_are_retained_only_for_unspecified_or_collection_layers() {
        let mut input = layer("POLYGON ((2 0,3 0,3 2,2 2,2 0))", Some(GeometryType::Polygon));
        assert!(clip(&input, &bbox()).unwrap().features.is_empty());
        input.geom_type = None;
        let out = clip(&input, &bbox()).unwrap();
        assert_eq!(out.features.len(), 1);
        assert!(matches!(out.features[0].geometry, Some(Geometry::LineString(_))));
        input.features[0].geometry = Some(geometry("POLYGON ((2 2,3 2,3 3,2 3,2 2))"));
        assert_eq!(clip(&input, &bbox()).unwrap().features[0].geometry, Some(geometry("POINT (2 2)")));
    }
    #[test]
    fn polygon_intersection_area_and_mixed_collection() {
        use geo::Area;
        let input = layer("GEOMETRYCOLLECTION (POLYGON ((1 1,3 1,3 3,1 3,1 1)),LINESTRING (-1 0,3 0),POINT (0 2),POINT (5 5))", None);
        let out = clip(&input, &bbox()).unwrap();
        let g = out.features[0].geometry.as_ref().unwrap();
        assert!((super::super::topology::to_geo(g).unsigned_area() - 1.0).abs() < 1e-12);
        assert!(matches!(g,Geometry::GeometryCollection(gs) if gs.len()==3));
    }
    #[test]
    fn template_overlaps_are_unioned_and_empty_templates_clip_everything() {
        let mut template = layer("POLYGON ((0 0,2 0,2 2,0 2,0 0))", Some(GeometryType::Polygon));
        template.features.push(Feature { fid: 2, geometry: Some(geometry("POLYGON ((1 0,3 0,3 2,1 2,1 0))")), attributes: vec![] });
        let input = layer("LINESTRING (-1 1,4 1)", None);
        let out = clip(&input, &ClipRegion::from_layer(&template).unwrap()).unwrap();
        assert_eq!(out.features.len(), 1);
        assert_eq!(out.features[0].geometry, Some(geometry("LINESTRING (0 1,3 1)")));
        template.features.clear();
        assert!(clip(&input, &ClipRegion::from_layer(&template).unwrap()).unwrap().features.is_empty());
    }
    #[test]
    fn bbox_and_geometry_validation_and_known_unknown_crs_mismatches() {
        for bounds in [[0., 0., 0., 1.], [1., 0., 0., 1.], [0., 0., 1., f64::NAN]] {
            assert!(ClipRegion::bbox(bounds, None).is_err());
        }
        assert!(ClipRegion::parse("POINT (1 1)", None).is_err());
        assert!(ClipRegion::parse("POLYGON ((0 0,2 2,0 2,2 0,0 0))", None).is_err());
        let input = layer("POINT (1 1)", None);
        let region = ClipRegion::bbox([0., 0., 2., 2.], Some(SpatialRef::from_epsg(4326).unwrap())).unwrap();
        assert!(clip(&input, &region).unwrap_err().to_string().contains("known and unknown"));
    }
    #[test]
    fn indexed_point_predicate_matches_independent_geometry_predicate() {
        let region =
            ClipRegion::parse("MULTIPOLYGON (((0 0,4 0,4 4,0 4,0 0),(1 1,1 3,3 3,3 1,1 1)),((5 0,6 0,6 4,5 4,5 0)))", None).unwrap();
        let mask = Mask::new(polygons(&region.geometry, 0).unwrap());
        for x in -4..=28 {
            for y in -4..=20 {
                let p = geo::coord! {x:x as f64/4.,y:y as f64/4.};
                assert_eq!(mask.covers(p), mask.area.intersects(&geo::Point(p)), "{p:?}");
            }
        }
    }

    #[test]
    fn dense_contained_untyped_lines_do_not_emit_spurious_contacts() {
        let mut input = layer("LINESTRING (0 1,2 1)", None);
        let coords: Vec<_> = (0..=10000).map(|i| Coord::xy(i as f64 / 5000., 1.)).collect();
        input.features[0].geometry = Some(Geometry::LineString(coords.clone()));
        let out = clip(&input, &bbox()).unwrap();
        assert_eq!(out.features.len(), 1);
        assert_eq!(out.features[0].geometry, Some(Geometry::LineString(coords)));
    }

    #[test]
    fn shallow_polygon_cuts_restore_original_floating_vertices() {
        use geo::Area;
        let x0 = 1_000_000.05;
        let x1 = 1_000_000.1;
        let input = layer("POLYGON ((1000000 0,1000000.1 100000,999999 100000,1000000 0))", Some(GeometryType::Polygon));
        let region = ClipRegion::bbox([x0, 0., 2_000_000., 200_000.], None).unwrap();
        let out = clip(&input, &region).unwrap();
        let y0 = 100_000. * ((x0 - 1_000_000.) / (x1 - 1_000_000.));
        let expected = 0.5 * (x1 - x0) * (100_000. - y0);
        let actual = super::super::topology::to_geo(out.features[0].geometry.as_ref().unwrap()).unsigned_area();
        assert!((actual - expected).abs() / expected < 1e-9, "{actual} vs {expected}");
    }

    #[test]
    fn only_the_region_is_reprojected() {
        let mut input = layer("POINT (1 1)", Some(GeometryType::Point));
        SpatialRef::from_epsg(4326).unwrap().apply_to_layer(&mut input);
        let region = ClipRegion::bbox([0., 0., 200_000., 200_000.], Some(SpatialRef::from_epsg(3857).unwrap())).unwrap();
        let out = clip(&input, &region).unwrap();
        assert_eq!(out.features[0].geometry, input.features[0].geometry);
        assert!(SpatialRef::from_layer(&out).same_as(&SpatialRef::from_layer(&input)));
    }
}

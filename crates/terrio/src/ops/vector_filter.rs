//! Vector conversion options, the equivalent of `ogr2ogr`'s: attribute and spatial filters, field
//! selection, a feature limit, geometry type conversion, exploding collections, CRS assignment and
//! reprojection, simple SQL, and appending to or overwriting existing outputs.

use crate::crs::{SpatialRef, Transformer};
use crate::ops::sql::{Expression, Select, field_index};
use crate::{Error, Result, VectorWriteOptions};
use std::borrow::Cow;
use wbvector::{Coord, Feature, FieldValue, Geometry, GeometryType, Layer, Ring};

/// Select named layers without silently dropping requests for missing names.
/// Retains source order and moves the layers instead of cloning their features.
pub fn select_layers(layers: Vec<Layer>, names: &[String]) -> Result<Vec<Layer>> {
    for name in names {
        if !layers.iter().any(|layer| layer.name == *name) {
            return Err(Error::InvalidArgument(format!("no layer named '{name}'")));
        }
    }
    Ok(layers.into_iter().filter(|layer| names.contains(&layer.name)).collect())
}

/// Opens selected operation inputs and rejects every missing requested layer name.
/// Keeps the file-stem alias supported by single-layer file drivers.
pub fn open_layers(path: &str, options: &crate::VectorOpenOptions) -> Result<Vec<Layer>> {
    let ds = crate::open_vector(path, options)?;
    let alias = if ds.layers.len() == 1 && ds.driver != crate::formats::VectorDriver::GPKG {
        std::path::Path::new(path).file_stem().and_then(|stem| stem.to_str())
    } else {
        None
    };
    for name in &options.layers {
        if !ds.layers.iter().any(|layer| {
            if ds.driver == crate::formats::VectorDriver::GPKG { layer.name.eq_ignore_ascii_case(name) } else { layer.name == *name }
        }) {
            // A selected subset is not proof that the source has only one layer.
            // Inspect all names only when validating an otherwise missing stem alias.
            if alias == Some(name.as_str()) && crate::list_layers(path)?.len() == 1 {
                continue;
            }
            return Err(Error::InvalidArgument(format!("no layer named '{name}'")));
        }
    }
    Ok(ds.layers)
}

/// What to do with a layer's geometries, as `ogr2ogr -nlt` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GeometryConversion {
    /// Turn single-part geometries into their multi-part form (`PROMOTE_TO_MULTI`).
    PromoteToMulti,
    /// Leave the geometries as they are (`CONVERT_TO_LINEAR`, `CONVERT_TO_CURVE`: terrio has no
    /// curves, so there is nothing to convert).
    Keep,
    /// Leave the geometries, and declare the layer as holding any type (`GEOMETRY`).
    Any,
    /// Force every geometry to this type where it can be (`POINT`, `MULTIPOLYGON`...).
    To(GeometryType),
}

impl GeometryConversion {
    /// Parses an `-nlt` value such as `PROMOTE_TO_MULTI`, `MULTIPOLYGON` or `POINT25D` (heights
    /// and measures suffixes are accepted and ignored).
    pub fn parse(s: &str) -> Result<Self> {
        let up = s.trim().to_ascii_uppercase();
        let base =
            up.strip_suffix("25D").or_else(|| up.strip_suffix("ZM")).or_else(|| up.strip_suffix('Z')).or_else(|| up.strip_suffix('M'));
        let base = base.map(str::trim_end).unwrap_or(&up);
        Ok(match base {
            "PROMOTE_TO_MULTI" => Self::PromoteToMulti,
            "CONVERT_TO_LINEAR" | "CONVERT_TO_CURVE" => Self::Keep,
            "GEOMETRY" => Self::Any,
            "POINT" => Self::To(GeometryType::Point),
            "LINESTRING" => Self::To(GeometryType::LineString),
            "POLYGON" => Self::To(GeometryType::Polygon),
            "MULTIPOINT" => Self::To(GeometryType::MultiPoint),
            "MULTILINESTRING" => Self::To(GeometryType::MultiLineString),
            "MULTIPOLYGON" => Self::To(GeometryType::MultiPolygon),
            "GEOMETRYCOLLECTION" => Self::To(GeometryType::GeometryCollection),
            _ => return Err(Error::InvalidArgument(format!("unknown geometry type '{s}'"))),
        })
    }
}

/// Options for [`translate`], named after `ogr2ogr`'s.
#[derive(Debug, Clone, Default)]
pub struct VectorTranslateOptions {
    /// `-where`: keep the features matching this OGR SQL expression.
    pub where_clause: Option<String>,
    /// `-sql`: a `SELECT ... FROM layer [WHERE ...] [ORDER BY ...] [LIMIT n]` statement. Its
    /// result replaces the input layers.
    pub sql: Option<String>,
    /// `-spat`: keep the features intersecting this rectangle `(min_x, min_y, max_x, max_y)`.
    pub spatial_filter: Option<(f64, f64, f64, f64)>,
    /// `-spat_srs`: the CRS of the spatial filter. By default, that of the layer.
    pub spatial_filter_srs: Option<SpatialRef>,
    /// `-select`: keep only these fields, in this order (names match case-insensitively).
    pub select: Option<Vec<String>>,
    /// `-limit`: keep at most this many features per layer.
    pub limit: Option<usize>,
    /// `-nln`: the output layer name.
    pub layer_name: Option<String>,
    /// `-nlt`: geometry type conversion.
    pub geometry_type: Option<GeometryConversion>,
    /// `-explodecollections`: write each part of multi-part geometries as its own feature.
    pub explode_collections: bool,
    /// A per-feature geometry operation, applied after exploding collections and before the
    /// geometry type conversion and reprojection: `-simplify` and `-segmentize`, or any
    /// [`crate::ops::vector_geom::GeometryOp`]. A simplification that fails leaves the geometry
    /// unchanged, as `ogr2ogr -simplify` does; other operations' errors fail the translation.
    pub geometry_op: Option<crate::ops::vector_geom::GeometryOp>,
    /// Propagate simplification failures instead of preserving the original geometry.
    /// Standalone geometry commands set this; ogr2ogr-compatible conversion leaves it false.
    pub strict_geometry_op: bool,
    /// `-s_srs`: the CRS of the input, overriding what the layer declares.
    pub src_srs: Option<SpatialRef>,
    /// `-t_srs`: reproject into this CRS.
    pub dst_srs: Option<SpatialRef>,
    /// `-a_srs`: assign this CRS to the output, without reprojecting.
    pub assign_srs: Option<SpatialRef>,
    /// Coordinate operation selection, including ALLOW_BALLPARK and ONLY_BEST.
    pub transform_options: crate::crs::TransformOptions,
}

impl VectorTranslateOptions {
    /// The `-spat` rectangle for readers of `input` that use it to skip features (GeoPackage's
    /// R-tree, GeoParquet's bounding boxes): when it is in the layer's CRS (no `-spat_srs` or
    /// `-s_srs`) and applies to the layers as read (no `-sql`). [`translate`] still filters the
    /// features by their geometries.
    pub fn read_filter(&self, input: &str) -> Option<(f64, f64, f64, f64)> {
        use crate::formats::VectorDriver;
        let indexed = matches!(VectorDriver::for_extension(input), Some(VectorDriver::Parquet | VectorDriver::GPKG));
        let plain = self.spatial_filter_srs.is_none() && self.src_srs.is_none() && self.sql.is_none();
        self.spatial_filter.filter(|_| indexed && plain)
    }
}

/// Applies the options to the layers, in `ogr2ogr`'s order: filters, limit, field selection,
/// collection explosion, geometry type conversion, reprojection, CRS assignment and renaming.
pub fn translate(layers: &[Layer], options: &VectorTranslateOptions) -> Result<Vec<Layer>> {
    let selected: Vec<Layer>;
    let (inputs, sql_limit): (&[Layer], Option<usize>) = match &options.sql {
        Some(sql) => {
            let (layer, limit) = run_select(layers, &Select::parse(sql)?, None)?;
            selected = vec![layer];
            (&selected, limit)
        }
        None => (layers, None),
    };
    if options.layer_name.is_some() && inputs.len() > 1 {
        return Err(Error::InvalidArgument("a new layer name needs a single input layer: choose one".into()));
    }
    inputs.iter().map(|l| translate_layer(l, l.features.iter().map(Cow::Borrowed), options, sql_limit)).collect()
}

/// [`translate`], consuming the layers: features are moved to the output rather than copied.
pub fn translate_owned(layers: Vec<Layer>, options: &VectorTranslateOptions) -> Result<Vec<Layer>> {
    if options.sql.is_some() {
        return translate(&layers, options);
    }
    if options.layer_name.is_some() && layers.len() > 1 {
        return Err(Error::InvalidArgument("a new layer name needs a single input layer: choose one".into()));
    }
    layers
        .into_iter()
        .map(|mut l| {
            let features = std::mem::take(&mut l.features);
            translate_layer(&l, features.into_iter().map(Cow::Owned), options, None)
        })
        .collect()
}

/// Translates a layer whose features are `features` (the layer's own, borrowed or taken out of
/// it).
fn translate_layer<'a>(
    layer: &Layer,
    features: impl Iterator<Item = Cow<'a, Feature>>,
    o: &VectorTranslateOptions,
    sql_limit: Option<usize>,
) -> Result<Layer> {
    let mut t = Translator::with_limit(layer, o, sql_limit)?;
    t.apply_iter(features)
}

/// [`translate`] applied to a layer chunk by chunk, for layers read and written as streams: the
/// filters, the field selection, geometry conversions and reprojection apply to each chunk, and
/// the limit counts the features of all the chunks. `--sql` needs the whole layer.
pub struct Translator {
    src: Layer,
    options: VectorTranslateOptions,
    filter: Option<crate::ops::sql::BoundExpression>,
    spatial: Option<SpatialFilter>,
    limit: Option<usize>,
    taken: usize,
    mapping: Option<Vec<usize>>,
    schema: Layer,
}

impl Translator {
    /// A translator for the features of `layer` (its fields and CRS; its features are not read).
    pub fn new(layer: &Layer, options: &VectorTranslateOptions) -> Result<Self> {
        if options.sql.is_some() {
            return Err(Error::Unsupported("an SQL statement needs the whole layer: it cannot be applied to a stream".into()));
        }
        Self::with_limit(layer, options, None)
    }

    fn with_limit(layer: &Layer, o: &VectorTranslateOptions, sql_limit: Option<usize>) -> Result<Self> {
        let mut src_srs = SpatialRef::from_layer(layer);
        if let Some(s) = &o.src_srs {
            src_srs = s.clone();
        }
        let filter = o.where_clause.as_deref().map(|w| Expression::parse(w)?.bind(&layer.schema)).transpose()?;
        let spatial = match o.spatial_filter {
            Some(rect) => Some(SpatialFilter::new(rect, o.spatial_filter_srs.as_ref(), &src_srs)?),
            None => None,
        };
        let limit = match (o.limit, sql_limit) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let mapping: Option<Vec<usize>> = match &o.select {
            Some(names) => {
                let mut idx = Vec::new();
                for n in names.iter().map(|n| n.trim()).filter(|n| !n.is_empty()) {
                    idx.push(
                        field_index(&layer.schema, n)
                            .ok_or_else(|| Error::InvalidArgument(format!("field '{n}' not found in layer '{}'", layer.name)))?,
                    );
                }
                Some(idx)
            }
            None => None,
        };
        let mut src = Layer::new(layer.name.clone());
        src.geom_type = layer.geom_type;
        src.crs = layer.crs.clone();
        match &mapping {
            Some(idx) => idx.iter().for_each(|&i| src.schema.add_field(layer.schema.fields()[i].clone())),
            None => src.schema = layer.schema.clone(),
        }
        if let Some(op) = &o.geometry_op {
            op.validate()?;
        }
        let mut t = Self { src, options: o.clone(), filter, spatial, limit, taken: 0, mapping, schema: Layer::new(String::new()) };
        t.schema = t.finish(t.empty())?;
        Ok(t)
    }

    fn empty(&self) -> Layer {
        let mut out = Layer::new(self.src.name.clone());
        out.geom_type = self.src.geom_type;
        out.crs = self.src.crs.clone();
        out.schema = self.src.schema.clone();
        out
    }

    /// The output layer, without features: its name, fields, geometry type and CRS.
    pub fn schema(&self) -> &Layer {
        &self.schema
    }

    /// Whether the limit has been reached: no further feature will be kept.
    pub fn is_done(&self) -> bool {
        self.limit.is_some_and(|n| self.taken >= n)
    }

    /// Translates a chunk of features (a layer with the input's fields).
    pub fn apply(&mut self, chunk: Layer) -> Result<Layer> {
        self.apply_iter(chunk.features.into_iter().map(Cow::Owned))
    }

    fn apply_iter<'a>(&mut self, features: impl Iterator<Item = Cow<'a, Feature>>) -> Result<Layer> {
        let mut out = self.empty();
        // The limit counts source features, as ogr2ogr does, even when exploding collections.
        for f in features {
            if self.is_done() {
                break;
            }
            if let Some(sf) = &self.spatial
                && !f.geometry.as_ref().is_some_and(|g| sf.intersects(g))
            {
                continue;
            }
            if let Some(e) = &self.filter
                && !e.matches(&f)
            {
                continue;
            }
            self.taken += 1;
            let f = f.into_owned();
            let attributes: Vec<FieldValue> = match &self.mapping {
                Some(idx) => idx.iter().map(|&i| f.attributes.get(i).cloned().unwrap_or(FieldValue::Null)).collect(),
                None => f.attributes,
            };
            match (f.geometry, self.options.explode_collections) {
                (Some(g), true) => {
                    for g in explode(&g) {
                        out.features.push(Feature { fid: f.fid, geometry: Some(g), attributes: attributes.clone() });
                    }
                }
                (g, _) => out.features.push(Feature { fid: f.fid, geometry: g, attributes }),
            }
        }
        self.finish(out)
    }

    /// The geometry conversion, reprojection, CRS assignment and renaming of a chunk.
    fn finish(&self, mut out: Layer) -> Result<Layer> {
        let o = &self.options;
        if let Some(op) = &o.geometry_op {
            use crate::ops::vector_geom::GeometryOp;
            let layer_type = out.geom_type;
            let features = std::mem::take(&mut out.features);
            out.features.reserve(features.len());
            for mut f in features {
                if let GeometryOp::Simplify { tolerance } = op
                    && !o.strict_geometry_op
                {
                    if let Some(g) = &f.geometry
                        && let Ok(simple) = crate::ops::simplify::simplify_preserve_topology(g, *tolerance)
                    {
                        f.geometry = Some(simple);
                    }
                    out.features.push(f);
                } else {
                    out.features.extend(op.apply_feature(f, layer_type)?);
                }
            }
            out.geom_type = op.layer_type(layer_type);
        }
        if let Some(conv) = o.geometry_type {
            for f in &mut out.features {
                if let Some(g) = f.geometry.take() {
                    f.geometry = Some(convert_geometry(g, conv));
                }
            }
            out.geom_type = match conv {
                GeometryConversion::To(t) => Some(t),
                GeometryConversion::Any => None,
                GeometryConversion::Keep => out.geom_type,
                GeometryConversion::PromoteToMulti => out.geom_type.map(promote_type),
            };
        } else if o.explode_collections {
            out.geom_type = out.geom_type.map(single_type);
        }
        if let Some(s) = &o.src_srs {
            s.apply_to_layer(&mut out);
        }
        if let Some(dst) = &o.dst_srs {
            out = crate::vector::reproject_layer_with_options(out, dst, &o.transform_options)?;
        }
        if let Some(a) = &o.assign_srs {
            a.apply_to_layer(&mut out);
        }
        if let Some(name) = &o.layer_name {
            out.name = name.clone();
        }
        out.extent = None;
        Ok(out)
    }
}

/// Runs a `SELECT` on the layer it names. Returns the result and its `LIMIT`.
/// `copy_limit` caps ordered feature references before cloning; translate leaves this unset
/// so its filters run before the returned SQL limit is applied.
pub(crate) fn run_select(layers: &[Layer], s: &Select, copy_limit: Option<usize>) -> Result<(Layer, Option<usize>)> {
    crate::ops::sql::select_query(layers, s, copy_limit)
}

/// A spatial filter: a polygon (a rectangle, or a rectangle transformed into the layer's CRS).
/// Features match when their geometry intersects it, boundary included, like GDAL's filter.
pub struct SpatialFilter {
    ring: Vec<(f64, f64)>,
    bbox: (f64, f64, f64, f64),
}

impl SpatialFilter {
    /// A filter on the rectangle `(min_x, min_y, max_x, max_y)` given in `rect_srs` (or, when
    /// `None`, the layer's CRS), for a layer in `layer_srs`. As `ogr2ogr` does, a rectangle in
    /// another CRS is densified every 10 km before being transformed.
    pub fn new(rect: (f64, f64, f64, f64), rect_srs: Option<&SpatialRef>, layer_srs: &SpatialRef) -> Result<Self> {
        let (x0, y0, x1, y1) = rect;
        let mut ring = vec![(x0, y0), (x0, y1), (x1, y1), (x1, y0), (x0, y0)];
        if let Some(srs) = rect_srs
            && !layer_srs.is_unknown()
            && !srs.same_as(layer_srs)
        {
            // GDAL densifies to 10 km segments, in degrees on geographic CRSs (6378137 m axis).
            let step = if srs.is_angular() { 10_000.0 / (6_378_137.0 * std::f64::consts::PI / 180.0) } else { 10_000.0 };
            let t = Transformer::new(srs, layer_srs)?;
            let mut dense = Vec::new();
            for w in ring.windows(2) {
                let (a, b) = (w[0], w[1]);
                let n = ((b.0 - a.0).hypot(b.1 - a.1) / step).ceil().max(1.0) as usize;
                for k in 0..n {
                    let f = k as f64 / n as f64;
                    dense.push((a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f));
                }
            }
            dense.push(ring[0]);
            ring = dense.into_iter().map(|(x, y)| t.transform(x, y)).collect::<Result<_>>()?;
        }
        let bbox = ring.iter().fold((f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY), |b, &(x, y)| {
            (b.0.min(x), b.1.min(y), b.2.max(x), b.3.max(y))
        });
        Ok(Self { ring, bbox })
    }

    /// Whether the geometry intersects the filter.
    pub fn intersects(&self, g: &Geometry) -> bool {
        let Some(b) = g.bbox() else { return false };
        if b.max_x < self.bbox.0 || b.min_x > self.bbox.2 || b.max_y < self.bbox.1 || b.min_y > self.bbox.3 {
            return false;
        }
        match g {
            Geometry::Point(c) => self.contains(c.x, c.y),
            Geometry::MultiPoint(cs) => cs.iter().any(|c| self.contains(c.x, c.y)),
            Geometry::LineString(cs) => self.path_intersects(cs),
            Geometry::MultiLineString(ls) => ls.iter().any(|l| self.path_intersects(l)),
            Geometry::Polygon { exterior, interiors } => self.polygon_intersects(exterior, interiors),
            Geometry::MultiPolygon(ps) => ps.iter().any(|(e, h)| self.polygon_intersects(e, h)),
            Geometry::GeometryCollection(gs) => gs.iter().any(|g| self.intersects(g)),
        }
    }

    fn contains(&self, x: f64, y: f64) -> bool {
        point_in_ring(x, y, &self.ring) != Inside::Out
    }

    fn path_intersects(&self, cs: &[Coord]) -> bool {
        if cs.iter().any(|c| self.contains(c.x, c.y)) {
            return true;
        }
        cs.windows(2).any(|s| self.ring.windows(2).any(|e| segments_intersect((s[0].x, s[0].y), (s[1].x, s[1].y), e[0], e[1])))
    }

    fn ring_intersects(&self, r: &Ring) -> bool {
        // Drivers may drop the closing point of rings.
        match (r.0.first(), r.0.last()) {
            (Some(a), Some(b)) if a.x != b.x || a.y != b.y => self.path_intersects(&[b.clone(), a.clone()]) || self.path_intersects(&r.0),
            _ => self.path_intersects(&r.0),
        }
    }

    fn polygon_intersects(&self, exterior: &Ring, interiors: &[Ring]) -> bool {
        if self.ring_intersects(exterior) || interiors.iter().any(|r| self.ring_intersects(r)) {
            return true;
        }
        // No boundary crossing: the filter is inside the polygon, or outside it (or in a hole).
        let (x, y) = self.ring[0];
        let in_ring = |r: &Ring| point_in_ring(x, y, &r.0.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>()) != Inside::Out;
        in_ring(exterior) && !interiors.iter().any(in_ring)
    }
}

#[derive(PartialEq)]
enum Inside {
    In,
    Boundary,
    Out,
}

/// Point in a closed ring, by crossing count, with points on the boundary reported as such.
fn point_in_ring(x: f64, y: f64, ring: &[(f64, f64)]) -> Inside {
    let n = ring.len();
    if n < 3 {
        return Inside::Out;
    }
    let mut inside = false;
    for i in 0..n {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        if orientation(a, b, (x, y)) == 0.0 && x >= a.0.min(b.0) && x <= a.0.max(b.0) && y >= a.1.min(b.1) && y <= a.1.max(b.1) {
            return Inside::Boundary;
        }
        if (a.1 > y) != (b.1 > y) && x < (b.0 - a.0) * (y - a.1) / (b.1 - a.1) + a.0 {
            inside = !inside;
        }
    }
    if inside { Inside::In } else { Inside::Out }
}

fn orientation(a: (f64, f64), b: (f64, f64), c: (f64, f64)) -> f64 {
    (b.0 - a.0) * (c.1 - a.1) - (b.1 - a.1) * (c.0 - a.0)
}

/// Whether two closed segments share a point.
fn segments_intersect(p1: (f64, f64), p2: (f64, f64), q1: (f64, f64), q2: (f64, f64)) -> bool {
    let on = |a: (f64, f64), b: (f64, f64), c: (f64, f64)| {
        c.0 >= a.0.min(b.0) && c.0 <= a.0.max(b.0) && c.1 >= a.1.min(b.1) && c.1 <= a.1.max(b.1)
    };
    let (d1, d2) = (orientation(q1, q2, p1), orientation(q1, q2, p2));
    let (d3, d4) = (orientation(p1, p2, q1), orientation(p1, p2, q2));
    if ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0)) && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0)) {
        return true;
    }
    (d1 == 0.0 && on(q1, q2, p1)) || (d2 == 0.0 && on(q1, q2, p2)) || (d3 == 0.0 && on(p1, p2, q1)) || (d4 == 0.0 && on(p1, p2, q2))
}

/// The parts of a multi-part geometry or collection; other geometries are returned as they are.
pub fn explode(g: &Geometry) -> Vec<Geometry> {
    match g {
        Geometry::MultiPoint(cs) => cs.iter().map(|c| Geometry::Point(c.clone())).collect(),
        Geometry::MultiLineString(ls) => ls.iter().map(|l| Geometry::LineString(l.clone())).collect(),
        Geometry::MultiPolygon(ps) => ps.iter().map(|(e, h)| Geometry::Polygon { exterior: e.clone(), interiors: h.clone() }).collect(),
        Geometry::GeometryCollection(gs) => gs.iter().flat_map(explode).collect(),
        g => vec![g.clone()],
    }
}

fn promote_type(t: GeometryType) -> GeometryType {
    match t {
        GeometryType::Point => GeometryType::MultiPoint,
        GeometryType::LineString => GeometryType::MultiLineString,
        GeometryType::Polygon => GeometryType::MultiPolygon,
        t => t,
    }
}

fn single_type(t: GeometryType) -> GeometryType {
    match t {
        GeometryType::MultiPoint => GeometryType::Point,
        GeometryType::MultiLineString => GeometryType::LineString,
        GeometryType::MultiPolygon => GeometryType::Polygon,
        t => t,
    }
}

/// Converts a geometry as `ogr2ogr -nlt` does (OGR's `forceTo`): conversions that cannot be made
/// leave the geometry unchanged.
pub fn convert_geometry(g: Geometry, conv: GeometryConversion) -> Geometry {
    use GeometryType as T;
    let target = match conv {
        GeometryConversion::Keep | GeometryConversion::Any => return g,
        GeometryConversion::PromoteToMulti => match g.geom_type() {
            T::Point | T::LineString | T::Polygon => promote_type(g.geom_type()),
            _ => return g,
        },
        GeometryConversion::To(t) => t,
    };
    if g.geom_type() == target {
        return g;
    }
    match (target, g) {
        (T::MultiPoint, Geometry::Point(c)) => Geometry::MultiPoint(vec![c]),
        (T::Point, Geometry::MultiPoint(mut cs)) if cs.len() == 1 => Geometry::Point(cs.remove(0)),
        (T::MultiLineString, Geometry::LineString(cs)) => Geometry::MultiLineString(vec![cs]),
        (T::MultiLineString, Geometry::Polygon { exterior, interiors }) => {
            Geometry::MultiLineString(std::iter::once(exterior).chain(interiors).map(closed).collect())
        }
        (T::MultiLineString, Geometry::MultiPolygon(ps)) => {
            Geometry::MultiLineString(ps.into_iter().flat_map(|(e, h)| std::iter::once(e).chain(h)).map(closed).collect())
        }
        (T::LineString, Geometry::MultiLineString(ls)) => match join_lines(&ls) {
            Some(l) => Geometry::LineString(l),
            None => Geometry::MultiLineString(ls),
        },
        (T::LineString, Geometry::Polygon { exterior, interiors }) if interiors.is_empty() => Geometry::LineString(closed(exterior)),
        (T::MultiPolygon, Geometry::Polygon { exterior, interiors }) => Geometry::MultiPolygon(vec![(exterior, interiors)]),
        (T::Polygon, Geometry::MultiPolygon(ps)) if !ps.is_empty() => {
            // OGR keeps every ring: the first one as the exterior, the others as holes.
            let mut rings = ps.into_iter().flat_map(|(e, h)| std::iter::once(e).chain(h));
            let exterior = rings.next().unwrap_or_default();
            Geometry::Polygon { exterior, interiors: rings.collect() }
        }
        (T::GeometryCollection, g) => Geometry::GeometryCollection(explode(&g)),
        (T::MultiPolygon | T::MultiLineString | T::MultiPoint, Geometry::GeometryCollection(gs)) => {
            let parts: Vec<Geometry> = gs.iter().flat_map(explode).collect();
            match target {
                T::MultiPolygon if parts.iter().all(|p| matches!(p, Geometry::Polygon { .. })) => Geometry::MultiPolygon(
                    parts
                        .into_iter()
                        .filter_map(|p| match p {
                            Geometry::Polygon { exterior, interiors } => Some((exterior, interiors)),
                            _ => None,
                        })
                        .collect(),
                ),
                T::MultiLineString if parts.iter().all(|p| matches!(p, Geometry::LineString(_))) => Geometry::MultiLineString(
                    parts
                        .into_iter()
                        .filter_map(|p| match p {
                            Geometry::LineString(l) => Some(l),
                            _ => None,
                        })
                        .collect(),
                ),
                T::MultiPoint if parts.iter().all(|p| matches!(p, Geometry::Point(_))) => Geometry::MultiPoint(
                    parts
                        .into_iter()
                        .filter_map(|p| match p {
                            Geometry::Point(c) => Some(c),
                            _ => None,
                        })
                        .collect(),
                ),
                _ => Geometry::GeometryCollection(gs),
            }
        }
        (_, g) => g,
    }
}

/// A ring's coordinates, ending with the first one (drivers may drop the closing point).
fn closed(r: Ring) -> Vec<Coord> {
    let mut cs = r.0;
    if let (Some(a), Some(b)) = (cs.first(), cs.last())
        && (a.x != b.x || a.y != b.y)
    {
        cs.push(a.clone());
    }
    cs
}

/// Joins lines whose ends meet into one line, when they all chain in order.
fn join_lines(ls: &[Vec<Coord>]) -> Option<Vec<Coord>> {
    let mut out: Vec<Coord> = ls.first()?.clone();
    for l in &ls[1..] {
        let (last, first) = (out.last()?, l.first()?);
        if last.x != first.x || last.y != first.y {
            return None;
        }
        out.extend(l.iter().skip(1).cloned());
    }
    Some(out)
}

/// How to treat an existing output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteMode {
    /// Replace the output file.
    #[default]
    Create,
    /// `-overwrite`: replace the layers of the same name, keeping a GeoPackage's other layers
    /// and tables.
    Overwrite,
    /// `-append`: add the features to the layers of the same name (fields are matched by name;
    /// fields the existing layer lacks are dropped), creating the layers that do not exist. In a
    /// GeoPackage, the features get new ids and the layer's R-tree, extent and feature count
    /// are updated, as GDAL does. In a PMTiles or MBTiles archive of vector tiles, the features
    /// are added to the tiles (see [`crate::tiles::update_vector`]).
    Append,
}

/// Writes layers, appending to or overwriting the layers of an existing output as `mode` says.
pub fn write_vector_with_mode(layers: &[Layer], path: &str, options: &VectorWriteOptions, mode: WriteMode) -> Result<()> {
    if mode == WriteMode::Create || !std::path::Path::new(path).exists() {
        return crate::write_vector(layers, path, options);
    }
    // GeoPackages are updated in place, keeping their other tables.
    let driver = options.driver.or_else(|| crate::formats::VectorDriver::for_extension(path));
    if let Some(d) = driver {
        d.ensure_built()?;
    }
    #[cfg(feature = "sqlite")]
    if driver == Some(crate::formats::VectorDriver::GPKG) {
        use crate::drivers::vector::gpkg_update::{UpdateMode, update};
        let mode = if mode == WriteMode::Append { UpdateMode::Append } else { UpdateMode::Overwrite };
        return update(path, layers, mode);
    }
    // Tile archives get the layers' features added to their tiles.
    #[cfg(all(feature = "tiles", not(target_family = "wasm")))]
    if matches!(driver, Some(crate::formats::VectorDriver::MBTiles | crate::formats::VectorDriver::PMTiles)) {
        return crate::tiles::update_vector(path, layers, &options.tiles, mode == WriteMode::Overwrite);
    }
    let mut existing = crate::open_vector(path, &Default::default())?.layers;
    for layer in layers {
        match existing.iter_mut().find(|l| l.name == layer.name) {
            Some(target) if mode == WriteMode::Append => append_features(target, layer),
            Some(target) => *target = layer.clone(),
            None => existing.push(layer.clone()),
        }
    }
    let is_gpkg = options.driver.or_else(|| crate::formats::VectorDriver::for_extension(path)) == Some(crate::formats::VectorDriver::GPKG);
    if !is_gpkg && existing.len() > 1 {
        // Single-layer formats: the layer written replaces or extends the one in the file.
        let first = existing.remove(0);
        let merged = match mode {
            WriteMode::Append => {
                let mut t = first;
                existing.iter().for_each(|l| append_features(&mut t, l));
                t
            }
            _ => existing.pop().unwrap_or(first),
        };
        existing = vec![merged];
    }
    crate::write_vector(&existing, path, options)
}

fn append_features(target: &mut Layer, src: &Layer) {
    let map: Vec<Option<usize>> = target.schema.fields().iter().map(|f| field_index(&src.schema, &f.name)).collect();
    let first_fid = target.features.iter().map(|f| f.fid).max().map_or(0, |m| m + 1);
    for (fid, f) in (first_fid..).zip(&src.features) {
        let attributes = map.iter().map(|i| i.and_then(|i| f.attributes.get(i).cloned()).unwrap_or(FieldValue::Null)).collect();
        target.features.push(Feature { fid, geometry: f.geometry.clone(), attributes });
    }
    target.extent = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{FieldDef, FieldType};

    #[test]
    fn named_selection_rejects_partial_matches() {
        let layers = vec![Layer::new("a"), Layer::new("b")];
        assert_eq!(select_layers(layers.clone(), &["b".into()]).unwrap()[0].name, "b");
        assert!(select_layers(layers, &["a".into(), "missing".into()]).unwrap_err().to_string().contains("missing"));
    }

    #[cfg(feature = "sqlite")]
    #[test]
    fn file_selection_rejects_partial_matches_and_keeps_stem_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("source.gpkg");
        let layers = vec![Layer::new("a"), Layer::new("b")];
        crate::write_vector(&layers, path.to_str().unwrap(), &Default::default()).unwrap();
        let case_options = crate::VectorOpenOptions { layers: vec!["A".into()], ..Default::default() };
        assert_eq!(open_layers(path.to_str().unwrap(), &case_options).unwrap()[0].name, "a");
        let options = crate::VectorOpenOptions { layers: vec!["a".into(), "missing".into()], ..Default::default() };
        assert!(open_layers(path.to_str().unwrap(), &options).unwrap_err().to_string().contains("missing"));
        let path = tmp.path().join("single.geojson");
        crate::write_vector(&layers[..1], path.to_str().unwrap(), &Default::default()).unwrap();
        let options = crate::VectorOpenOptions { layers: vec!["single".into()], ..Default::default() };
        assert_eq!(open_layers(path.to_str().unwrap(), &options).unwrap().len(), 1);
        let options = crate::VectorOpenOptions { layers: vec!["single".into(), "missing".into()], ..Default::default() };
        assert!(open_layers(path.to_str().unwrap(), &options).unwrap_err().to_string().contains("missing"));
    }

    #[test]
    fn filtered_multilayer_file_does_not_gain_a_stem_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("source.kml");
        std::fs::write(&path, r#"<kml xmlns="http://www.opengis.net/kml/2.2"><Document><Folder><name>a</name><Placemark><Point><coordinates>0,0</coordinates></Point></Placemark></Folder><Folder><name>b</name><Placemark><Point><coordinates>1,1</coordinates></Point></Placemark></Folder></Document></kml>"#).unwrap();
        let options = crate::VectorOpenOptions { layers: vec!["a".into(), "source".into()], ..Default::default() };
        assert!(open_layers(path.to_str().unwrap(), &options).unwrap_err().to_string().contains("source"));
        let options = crate::VectorOpenOptions { layers: vec!["a".into()], ..Default::default() };
        assert_eq!(open_layers(path.to_str().unwrap(), &options).unwrap()[0].name, "a");
    }

    fn square(x: f64, y: f64, s: f64) -> Geometry {
        Geometry::polygon(vec![Coord::xy(x, y), Coord::xy(x + s, y), Coord::xy(x + s, y + s), Coord::xy(x, y + s), Coord::xy(x, y)], vec![])
    }

    fn layer() -> Layer {
        let mut l = Layer::new("t");
        l.schema.add_field(FieldDef::new("name", FieldType::Text));
        l.schema.add_field(FieldDef::new("v", FieldType::Integer));
        let geoms = [
            Geometry::point(0.5, 0.5),
            square(2.0, 2.0, 1.0),
            Geometry::line_string(vec![Coord::xy(-1.0, 5.0), Coord::xy(5.0, -1.0)]),
            Geometry::multi_point(vec![Coord::xy(10.0, 10.0), Coord::xy(0.0, 0.0)]),
            square(-10.0, -10.0, 30.0),
        ];
        for (i, g) in geoms.into_iter().enumerate() {
            l.features.push(Feature {
                fid: i as u64,
                geometry: Some(g),
                attributes: vec![FieldValue::Text(format!("f{i}")), FieldValue::Integer(i as i64)],
            });
        }
        l
    }

    #[test]
    fn spatial_filter() {
        let sf = SpatialFilter::new((0.0, 0.0, 1.0, 1.0), None, &SpatialRef::default()).unwrap();
        let l = layer();
        let hits: Vec<bool> = l.features.iter().map(|f| sf.intersects(f.geometry.as_ref().unwrap())).collect();
        // The line passes through (2, 2)... no: it crosses x + y = 4, which misses the unit square.
        assert_eq!(hits, vec![true, false, false, true, true]);
        // Touching counts.
        let sf = SpatialFilter::new((3.0, 3.0, 4.0, 4.0), None, &SpatialRef::default()).unwrap();
        assert!(sf.intersects(&square(2.0, 2.0, 1.0)));
        // A filter inside a hole does not intersect.
        let holed = Geometry::polygon(
            vec![Coord::xy(0.0, 0.0), Coord::xy(10.0, 0.0), Coord::xy(10.0, 10.0), Coord::xy(0.0, 10.0), Coord::xy(0.0, 0.0)],
            vec![vec![Coord::xy(2.0, 2.0), Coord::xy(8.0, 2.0), Coord::xy(8.0, 8.0), Coord::xy(2.0, 8.0), Coord::xy(2.0, 2.0)]],
        );
        assert!(!SpatialFilter::new((4.0, 4.0, 5.0, 5.0), None, &SpatialRef::default()).unwrap().intersects(&holed));
    }

    #[test]
    fn translate_options() {
        let l = layer();
        let o = VectorTranslateOptions {
            where_clause: Some("v >= 1".into()),
            select: Some(vec!["V".into()]),
            limit: Some(2),
            layer_name: Some("out".into()),
            geometry_type: Some(GeometryConversion::PromoteToMulti),
            ..Default::default()
        };
        let out = translate(std::slice::from_ref(&l), &o).unwrap();
        assert_eq!(out[0].name, "out");
        assert_eq!(out[0].len(), 2);
        assert_eq!(out[0].schema.len(), 1);
        assert_eq!(out[0].features[0].attributes, vec![FieldValue::Integer(1)]);
        assert_eq!(out[0].features[0].geometry.as_ref().unwrap().geom_type(), GeometryType::MultiPolygon);
        assert_eq!(out[0].features[1].geometry.as_ref().unwrap().geom_type(), GeometryType::MultiLineString);

        let o = VectorTranslateOptions { explode_collections: true, limit: Some(4), ..Default::default() };
        let out = translate(std::slice::from_ref(&l), &o).unwrap();
        // Four source features, the fourth a two-point multipoint.
        assert_eq!(out[0].len(), 5);
        assert_eq!(out[0].features[4].fid, 3);

        let o = VectorTranslateOptions {
            sql: Some("SELECT name AS n FROM t WHERE v < 3 ORDER BY v DESC LIMIT 2".into()),
            ..Default::default()
        };
        let out = translate(std::slice::from_ref(&l), &o).unwrap();
        assert_eq!(out[0].schema.fields()[0].name, "n");
        let names: Vec<_> = out[0].features.iter().map(|f| f.attributes[0].as_str().unwrap().to_string()).collect();
        assert_eq!(names, vec!["f2", "f1"]);

        assert!(translate(&[l], &VectorTranslateOptions { select: Some(vec!["nope".into()]), ..Default::default() }).is_err());
    }

    #[test]
    fn geometry_conversions() {
        assert!(matches!(GeometryConversion::parse("multipolygon25d").unwrap(), GeometryConversion::To(GeometryType::MultiPolygon)));
        let mp = convert_geometry(square(0.0, 0.0, 1.0), GeometryConversion::To(GeometryType::MultiLineString));
        assert_eq!(mp.geom_type(), GeometryType::MultiLineString);
        let p = convert_geometry(Geometry::multi_point(vec![Coord::xy(1.0, 2.0)]), GeometryConversion::To(GeometryType::Point));
        assert_eq!(p, Geometry::point(1.0, 2.0));
        let l = convert_geometry(
            Geometry::multi_line_string(vec![
                vec![Coord::xy(0.0, 0.0), Coord::xy(1.0, 0.0)],
                vec![Coord::xy(1.0, 0.0), Coord::xy(1.0, 1.0)],
            ]),
            GeometryConversion::To(GeometryType::LineString),
        );
        assert_eq!(l, Geometry::line_string(vec![Coord::xy(0.0, 0.0), Coord::xy(1.0, 0.0), Coord::xy(1.0, 1.0)]));
    }
}

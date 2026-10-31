//! Planar XY buffering, polygon overlay/dissolve, validity, repair and cleanup.
//!
//! Distances and grid spacings use the input CRS's coordinate units, including degrees for
//! geographic CRSs. Reproject first for linear-distance operations. Overlay requires matching
//! CRSs and polygon geometries. Newly constructed topology is two-dimensional; validity reports
//! and already-valid geometries returned by repair retain their original Z/M coordinates.

use crate::{Error, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Result};
use geo::algorithm::buffer::{BufferStyle, LineCap, LineJoin};
use geo::algorithm::kernels::{Kernel, Orientation, RobustKernel};
use geo::{BooleanOps, BoundingRect, Buffer, MakeValid, Orient, Validation};
use rstar::primitives::{GeomWithData, Rectangle};
use rstar::{AABB, RTree};
use std::collections::HashMap;
use wbvector::{Coord, Ring};

use super::geometry::{header, unique_field};

/// Buffer distance and round-arc resolution. Distances are planar CRS units, and may be
/// negative for polygon erosion. Null features remain null; empty buffers retain attributes.
#[derive(Debug, Clone, Copy)]
pub struct BufferOptions {
    /// Offset in coordinate units: metres in a metre-based projected CRS, degrees in lon/lat.
    pub distance: f64,
    /// Segments per quadrant of a circle, from 1 to 4096 (default 30, as OGR).
    pub quadrant_segments: usize,
}

impl Default for BufferOptions {
    fn default() -> Self {
        Self { distance: 0.0, quadrant_segments: 30 }
    }
}

/// Buffers each valid geometry with round caps and joins. Attributes, feature IDs and CRS
/// remain unchanged; non-null output geometries are XY MultiPolygons. Invalid geometries are
/// rejected with their feature ID; repair them with [`make_valid`] first.
///
/// ```no_run
/// let ds = terrio::open_vector("roads.gpkg", &Default::default())?;
/// let buffered = terrio::ops::topology::buffer(&ds.layers[0], &terrio::ops::topology::BufferOptions {
///     distance: 100.0, ..Default::default()
/// })?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn buffer(layer: &Layer, options: &BufferOptions) -> Result<Layer> {
    if !options.distance.is_finite() || !(1..=4096).contains(&options.quadrant_segments) {
        return Err(Error::InvalidArgument("buffer needs a finite distance and 1..=4096 quadrant segments".into()));
    }
    let mut out = header(layer, Some(GeometryType::MultiPolygon));
    let angle = std::f64::consts::FRAC_PI_2 / options.quadrant_segments as f64;
    let style = BufferStyle::new(options.distance).line_cap(LineCap::Round(angle)).line_join(LineJoin::Round(angle));
    for f in &layer.features {
        let geometry = f
            .geometry
            .as_ref()
            .map(|g| {
                require_valid(g, f.fid)?;
                if g.all_coords().is_empty() {
                    return Ok(Geometry::MultiPolygon(vec![]));
                }
                let result = to_geo(g).buffer_with_style(style.clone());
                checked_output(from_polygons(result))
            })
            .transpose()?;
        out.features.push(Feature { fid: f.fid, geometry, attributes: attributes(layer, f) });
    }
    Ok(out)
}

/// A polygon overlay operation. Only positive-area results are emitted: touching lines and
/// points are omitted, as OGR with `KEEP_LOWER_DIMENSION_GEOMETRIES=NO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayOperation {
    /// One result per intersecting pair, with attributes from both input features.
    Intersection,
    /// Intersections plus portions belonging to only one input layer.
    Union,
    /// Portions of each left feature not covered by any right feature.
    Difference,
    /// Portions belonging to only one input layer.
    SymmetricDifference,
}

/// Overlays valid polygon layers in the same CRS (or both with no CRS). Output fields are
/// prefixed `a_`/`b_`, followed by text `a_source_fid`/`b_source_fid` fields. Collisions receive
/// numeric suffixes; fields of an absent side are null. Feature IDs are sequential from 1.
/// Empty/null input geometries contribute no area. Overlaps within a source layer preserve
/// that layer's feature multiplicity; dissolve first when disjoint source coverage is needed.
///
/// ```no_run
/// let a = terrio::open_vector("parcels.gpkg", &Default::default())?;
/// let b = terrio::open_vector("zones.gpkg", &Default::default())?;
/// let result = terrio::ops::topology::overlay(&a.layers[0], &b.layers[0],
///     terrio::ops::topology::OverlayOperation::Intersection)?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn overlay(a: &Layer, b: &Layer, operation: OverlayOperation) -> Result<Layer> {
    let ca = crate::crs::SpatialRef::from_layer(a);
    let cb = crate::crs::SpatialRef::from_layer(b);
    if ca.is_unknown() != cb.is_unknown() || (!ca.is_unknown() && !ca.same_as(&cb)) {
        return Err(Error::Crs("overlay layers must use the same CRS; reproject with convert --t-srs first".into()));
    }
    let left = polygon_features(a)?;
    let right = polygon_features(b)?;
    let left_bounds: Vec<_> = left.iter().map(|(_, g)| g.bounding_rect()).collect();
    let right_bounds: Vec<_> = right.iter().map(|(_, g)| g.bounding_rect()).collect();
    let right_index = bounds_index(&right_bounds);
    let mut out = Layer::new(a.name.clone());
    out.crs = a.crs.clone();
    out.geom_type = Some(GeometryType::MultiPolygon);
    for (prefix, layer) in [("a", a), ("b", b)] {
        for field in layer.schema.fields() {
            let mut field = field.clone();
            field.name = unique_field(&out, &format!("{prefix}_{}", field.name));
            out.schema.add_field(field);
        }
    }
    for name in ["a_source_fid", "b_source_fid"] {
        let name = unique_field(&out, name);
        out.schema.add_field(FieldDef::new(name, FieldType::Text));
    }
    let mut emit = |geometry: geo::MultiPolygon<f64>, fa: Option<&Feature>, fb: Option<&Feature>| -> Result<()> {
        if geometry.0.iter().all(|p| p.exterior().0.is_empty()) {
            return Ok(());
        }
        let geometry = checked_output(from_polygons(geometry))?;
        let mut values = fa.map_or_else(|| vec![FieldValue::Null; a.schema.len()], |f| attributes(a, f));
        values.extend(fb.map_or_else(|| vec![FieldValue::Null; b.schema.len()], |f| attributes(b, f)));
        values.push(fa.map_or(FieldValue::Null, |f| FieldValue::Text(f.fid.to_string())));
        values.push(fb.map_or(FieldValue::Null, |f| FieldValue::Text(f.fid.to_string())));
        out.features.push(Feature { fid: out.features.len() as u64 + 1, geometry: Some(geometry), attributes: values });
        Ok(())
    };
    if matches!(operation, OverlayOperation::Intersection | OverlayOperation::Union) {
        for ((fa, ga), bounds) in left.iter().zip(&left_bounds) {
            for i in candidates(&right_index, *bounds) {
                let (fb, gb) = &right[i];
                emit(ga.intersection(gb), Some(fa), Some(fb))?;
            }
        }
    }
    if operation != OverlayOperation::Intersection {
        for ((f, g), bounds) in left.iter().zip(&left_bounds) {
            let matches = candidates(&right_index, *bounds);
            let result = match matches.as_slice() {
                [] => g.clone(),
                [i] => g.difference(&right[*i].1),
                _ => g.difference(&union(matches.iter().map(|&i| &right[i].1))),
            };
            emit(result, Some(f), None)?;
        }
        if matches!(operation, OverlayOperation::Union | OverlayOperation::SymmetricDifference) {
            let left_index = bounds_index(&left_bounds);
            for ((f, g), bounds) in right.iter().zip(&right_bounds) {
                let matches = candidates(&left_index, *bounds);
                let result = match matches.as_slice() {
                    [] => g.clone(),
                    [i] => g.difference(&left[*i].1),
                    _ => g.difference(&union(matches.iter().map(|&i| &left[i].1))),
                };
                emit(result, None, Some(f))?;
            }
        }
    }
    Ok(out)
}

/// Dissolves valid polygon features, across a whole layer when `fields` is empty, or by typed
/// attribute values (nulls form a group). Only group fields and `feature_count` are retained.
/// Output IDs start at 1 in first-seen group order. Null geometries count toward a group but
/// add no area; an all-null group has null geometry. Empty input produces an empty layer.
///
/// ```no_run
/// let ds = terrio::open_vector("counties.gpkg", &Default::default())?;
/// let states = terrio::ops::topology::dissolve(&ds.layers[0], &["STATEFP".into()])?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn dissolve(layer: &Layer, fields: &[String]) -> Result<Layer> {
    let mut out = Layer::new(layer.name.clone());
    out.crs = layer.crs.clone();
    out.geom_type = Some(GeometryType::MultiPolygon);
    let mut indices = Vec::new();
    for name in fields {
        let i = layer.schema.field_index(name).ok_or_else(|| Error::InvalidArgument(format!("unknown dissolve field '{name}'")))?;
        if indices.contains(&i) {
            return Err(Error::InvalidArgument(format!("duplicate dissolve field '{name}'")));
        }
        indices.push(i);
        out.schema.add_field(layer.schema.fields()[i].clone());
    }
    let count_name = unique_field(&out, "feature_count");
    out.schema.add_field(FieldDef::new(count_name, FieldType::Integer));
    let mut lookup = HashMap::<Vec<Key>, usize>::new();
    let mut groups: Vec<Group> = Vec::new();
    for f in &layer.features {
        let values: Vec<_> = indices.iter().map(|i| f.attributes.get(*i).cloned().unwrap_or(FieldValue::Null)).collect();
        let key = values.iter().map(key).collect::<Result<Vec<_>>>()?;
        let index = *lookup.entry(key).or_insert_with(|| {
            groups.push(Group { values, shapes: Vec::new(), count: 0, has_geometry: false });
            groups.len() - 1
        });
        let group = &mut groups[index];
        group.count += 1;
        if let Some(g) = &f.geometry {
            require_valid(g, f.fid)?;
            group.shapes.push(polygons(g, f.fid)?);
            group.has_geometry = true;
        }
    }
    for Group { mut values, shapes, count, has_geometry } in groups {
        values.push(FieldValue::Integer(count as i64));
        let geometry = has_geometry.then(|| checked_output(from_polygons(union(shapes.iter())))).transpose()?;
        out.features.push(Feature { fid: out.features.len() as u64 + 1, geometry, attributes: values });
    }
    Ok(out)
}

/// Appends collision-safe `is_valid` (Boolean) and `valid_reason` (Text) fields. Geometry,
/// attributes, feature IDs, Z/M and CRS are preserved. Null geometry has null status/reason;
/// empty geometry is valid. Rings are logically closed even if a driver omits the repeated
/// closing coordinate. Reasons describe the first detected problem, and are not GEOS's text.
///
/// ```no_run
/// let ds = terrio::open_vector("polygons.gpkg", &Default::default())?;
/// let report = terrio::ops::topology::validity(&ds.layers[0]);
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn validity(layer: &Layer) -> Layer {
    let mut out = header(layer, layer.geom_type);
    for (name, ty) in [("is_valid", FieldType::Boolean), ("valid_reason", FieldType::Text)] {
        let name = unique_field(&out, name);
        out.schema.add_field(FieldDef::new(name, ty));
    }
    for f in &layer.features {
        let mut values = attributes(layer, f);
        match &f.geometry {
            None => values.extend([FieldValue::Null, FieldValue::Null]),
            Some(g) => {
                let reason = invalid_reason(g);
                values.extend([FieldValue::Boolean(reason.is_none()), FieldValue::Text(reason.unwrap_or_else(|| "Valid Geometry".into()))]);
            }
        }
        out.features.push(Feature { fid: f.fid, geometry: f.geometry.clone(), attributes: values });
    }
    out
}

/// Repairs invalid polygons with an odd-even triangulated fill, possibly returning a
/// MultiPolygon. Collapsed area parts are dropped; fully collapsed polygons become empty
/// MultiPolygons. Degenerate lines become empty lines. Collections are repaired recursively.
/// Already-valid geometries retain Z/M exactly; repaired geometry is XY-only. Null geometries,
/// attributes and IDs are preserved. Non-finite XY or a failed repair returns an error.
///
/// ```no_run
/// let ds = terrio::open_vector("polygons.gpkg", &Default::default())?;
/// let fixed = terrio::ops::topology::make_valid(&ds.layers[0])?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn make_valid(layer: &Layer) -> Result<Layer> {
    map_layer(layer, repair)
}

/// Coordinate cleanup options. Defaults remove consecutive XY duplicates, close polygon
/// rings, drop collapsed/empty parts, and repair invalid polygons without snapping.
#[derive(Debug, Clone, Copy, Default)]
pub struct CleanOptions {
    /// Snap XY to multiples of this positive spacing in CRS units. Zero disables snapping.
    /// Ties round away from zero. Retained vertices keep Z/M; the first XY duplicate wins.
    pub snap_grid: f64,
}

/// Cleans each feature according to [`CleanOptions`], retaining attributes, IDs and CRS.
/// A fully collapsed feature has null geometry. Polygon repair may change its type and
/// discards Z/M only when topology must be rebuilt. Non-finite XY or snapping overflow fails.
///
/// ```no_run
/// let ds = terrio::open_vector("polygons.gpkg", &Default::default())?;
/// let cleaned = terrio::ops::topology::clean(&ds.layers[0], &terrio::ops::topology::CleanOptions { snap_grid: 0.01 })?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn clean(layer: &Layer, options: &CleanOptions) -> Result<Layer> {
    if !options.snap_grid.is_finite() || options.snap_grid < 0.0 {
        return Err(Error::InvalidArgument("snap grid spacing must be finite and nonnegative".into()));
    }
    map_layer(layer, |g| {
        finite(g)?;
        let g = clean_geometry(g, options.snap_grid)?;
        repair(&g)
    })
    .map(|mut out| {
        for f in &mut out.features {
            if f.geometry.as_ref().is_some_and(|g| g.all_coords().is_empty()) {
                f.geometry = None;
            }
        }
        out
    })
}

fn attributes(layer: &Layer, f: &Feature) -> Vec<FieldValue> {
    let mut values = f.attributes.clone();
    values.resize(layer.schema.len(), FieldValue::Null);
    values
}

fn map_layer(layer: &Layer, transform: impl Fn(&Geometry) -> Result<Geometry>) -> Result<Layer> {
    let mut out = header(layer, None);
    for f in &layer.features {
        let geometry = f
            .geometry
            .as_ref()
            .map(|g| {
                transform(g).map_err(|e| match e {
                    Error::InvalidArgument(message) => Error::InvalidArgument(format!("feature {}: {message}", f.fid)),
                    Error::InvalidData(message) => Error::InvalidData(format!("feature {}: {message}", f.fid)),
                    other => other,
                })
            })
            .transpose()?;
        out.features.push(Feature { fid: f.fid, geometry, attributes: attributes(layer, f) });
    }
    Ok(out)
}

fn finite(g: &Geometry) -> Result<()> {
    if g.all_coords().iter().any(|c| !c.x.is_finite() || !c.y.is_finite()) {
        Err(Error::InvalidArgument("non-finite XY coordinates".into()))
    } else {
        Ok(())
    }
}

fn invalid_reason(g: &Geometry) -> Option<String> {
    if let Err(e) = finite(g) {
        return Some(e.to_string());
    }
    collapsed_reason(g).or_else(|| to_geo(g).check_validation().err().map(|e| e.to_string()))
}

fn collapsed_reason(g: &Geometry) -> Option<String> {
    let ring = |r: &Ring| {
        let first = r.0.first()?;
        let second = r.0.iter().find(|c| c.x != first.x || c.y != first.y);
        let has_area = second.is_some_and(|second| {
            r.0.iter().any(|c| {
                RobustKernel::orient2d(geo::coord! {x:first.x,y:first.y}, geo::coord! {x:second.x,y:second.y}, geo::coord! {x:c.x,y:c.y})
                    != Orientation::Collinear
            })
        });
        (!has_area).then(|| "polygon ring has collapsed to a line or point".to_string())
    };
    match g {
        Geometry::Polygon { exterior, interiors } => ring(exterior).or_else(|| interiors.iter().find_map(ring)),
        Geometry::MultiPolygon(ps) => ps.iter().find_map(|(e, i)| ring(e).or_else(|| i.iter().find_map(ring))),
        Geometry::GeometryCollection(gs) => gs.iter().find_map(collapsed_reason),
        _ => None,
    }
}

pub(super) fn require_valid(g: &Geometry, fid: u64) -> Result<()> {
    if let Some(reason) = invalid_reason(g) {
        Err(Error::InvalidArgument(format!("feature {fid} is invalid: {reason}; run make-valid first")))
    } else {
        Ok(())
    }
}

fn checked_output(g: Geometry) -> Result<Geometry> {
    finite(&g)?;
    Ok(g)
}

fn polygons(g: &Geometry, fid: u64) -> Result<geo::MultiPolygon<f64>> {
    match to_geo(g) {
        geo::Geometry::Polygon(p) => Ok(geo::MultiPolygon(if p.exterior().0.is_empty() { vec![] } else { vec![p] })),
        geo::Geometry::MultiPolygon(p) => Ok(p),
        _ => Err(Error::InvalidArgument(format!("feature {fid} is not polygonal"))),
    }
}

fn polygon_features(layer: &Layer) -> Result<Vec<(&Feature, geo::MultiPolygon<f64>)>> {
    layer
        .features
        .iter()
        .filter_map(|f| f.geometry.as_ref().map(|g| (f, g)))
        .map(|(f, g)| {
            require_valid(g, f.fid)?;
            Ok((f, polygons(g, f.fid)?))
        })
        .collect()
}

type BoundsIndex = RTree<GeomWithData<Rectangle<[f64; 2]>, usize>>;

fn envelope(r: geo::Rect<f64>) -> AABB<[f64; 2]> {
    AABB::from_corners([r.min().x, r.min().y], [r.max().x, r.max().y])
}

fn bounds_index(bounds: &[Option<geo::Rect<f64>>]) -> BoundsIndex {
    RTree::bulk_load(
        bounds.iter().enumerate().filter_map(|(i, b)| b.map(|b| GeomWithData::new(Rectangle::from_aabb(envelope(b)), i))).collect(),
    )
}

fn candidates(index: &BoundsIndex, bounds: Option<geo::Rect<f64>>) -> Vec<usize> {
    let Some(bounds) = bounds else { return Vec::new() };
    let mut indices: Vec<_> = index.locate_in_envelope_intersecting(envelope(bounds)).map(|entry| entry.data).collect();
    // R-tree traversal order must not change source-pair order or output feature IDs.
    indices.sort_unstable();
    indices
}

fn union<'a>(shapes: impl Iterator<Item = &'a geo::MultiPolygon<f64>>) -> geo::MultiPolygon<f64> {
    let oriented: Vec<_> = shapes.map(|g| g.orient(geo::algorithm::orient::Direction::Default)).collect();
    geo::unary_union(&oriented)
}

fn repair(g: &Geometry) -> Result<Geometry> {
    finite(g)?;
    if invalid_reason(g).is_none() {
        return Ok(g.clone());
    }
    let result = match g {
        Geometry::Polygon { .. } | Geometry::MultiPolygon(_) => {
            let p = polygons(g, 0)?.make_valid().map_err(|e| Error::InvalidData(format!("polygon repair failed: {e}")))?;
            from_polygons(p)
        }
        Geometry::LineString(cs) => Geometry::LineString(clean_line(cs, 0.0, false)?),
        Geometry::MultiLineString(lines) => {
            Geometry::MultiLineString(lines.iter().map(|l| clean_line(l, 0.0, false)).collect::<Result<_>>()?)
        }
        Geometry::GeometryCollection(gs) => Geometry::GeometryCollection(
            gs.iter().map(repair).collect::<Result<Vec<_>>>()?.into_iter().filter(|g| !g.all_coords().is_empty()).collect(),
        ),
        _ => g.clone(),
    };
    if let Some(reason) = invalid_reason(&result) {
        return Err(Error::InvalidData(format!("geometry repair failed: {reason}")));
    }
    Ok(result)
}

fn clean_coord(c: &Coord, grid: f64) -> Result<Coord> {
    let mut c = c.clone();
    if grid > 0.0 {
        c.x = (c.x / grid).round() * grid;
        c.y = (c.y / grid).round() * grid;
    }
    if !c.x.is_finite() || !c.y.is_finite() {
        return Err(Error::InvalidArgument("coordinate overflow while snapping".into()));
    }
    Ok(c)
}

fn clean_line(cs: &[Coord], grid: f64, ring: bool) -> Result<Vec<Coord>> {
    let mut out: Vec<Coord> = Vec::new();
    for c in cs {
        let c = clean_coord(c, grid)?;
        if out.last().is_none_or(|last| last.x != c.x || last.y != c.y) {
            out.push(c);
        }
    }
    if ring && out.first().zip(out.last()).is_some_and(|(a, b)| a.x != b.x || a.y != b.y) {
        out.push(out[0].clone());
    }
    let min = if ring { 4 } else { 2 };
    if out.len() < min {
        out.clear();
    }
    Ok(out)
}

fn clean_geometry(g: &Geometry, grid: f64) -> Result<Geometry> {
    Ok(match g {
        Geometry::Point(c) => Geometry::Point(clean_coord(c, grid)?),
        Geometry::LineString(cs) => Geometry::LineString(clean_line(cs, grid, false)?),
        Geometry::Polygon { exterior, interiors } => {
            let exterior = Ring(clean_line(&exterior.0, grid, true)?);
            let interiors = if exterior.0.is_empty() {
                Vec::new()
            } else {
                interiors
                    .iter()
                    .map(|r| clean_line(&r.0, grid, true).map(Ring))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .filter(|r| !r.0.is_empty())
                    .collect()
            };
            Geometry::Polygon { exterior, interiors }
        }
        Geometry::MultiPoint(cs) => {
            let mut points = Vec::new();
            for c in cs {
                if let Geometry::Point(c) = clean_geometry(&Geometry::Point(c.clone()), grid)?
                    && points.last().is_none_or(|p: &Coord| p.x != c.x || p.y != c.y)
                {
                    points.push(c);
                }
            }
            Geometry::MultiPoint(points)
        }
        Geometry::MultiLineString(ls) => Geometry::MultiLineString(
            ls.iter().map(|l| clean_line(l, grid, false)).collect::<Result<Vec<_>>>()?.into_iter().filter(|l| !l.is_empty()).collect(),
        ),
        Geometry::MultiPolygon(ps) => {
            let mut out = Vec::new();
            for (e, i) in ps {
                if let Geometry::Polygon { exterior, interiors } =
                    clean_geometry(&Geometry::Polygon { exterior: e.clone(), interiors: i.clone() }, grid)?
                    && !exterior.0.is_empty()
                {
                    out.push((exterior, interiors));
                }
            }
            Geometry::MultiPolygon(out)
        }
        Geometry::GeometryCollection(gs) => Geometry::GeometryCollection(
            gs.iter()
                .map(|g| clean_geometry(g, grid))
                .collect::<Result<Vec<_>>>()?
                .into_iter()
                .filter(|g| !g.all_coords().is_empty())
                .collect(),
        ),
    })
}

fn to_geo(g: &Geometry) -> geo::Geometry<f64> {
    let coord = |c: &Coord| geo::Coord { x: c.x, y: c.y };
    let line = |cs: &[Coord]| geo::LineString(cs.iter().map(coord).collect());
    let poly = |e: &Ring, i: &[Ring]| geo::Polygon::new(line(&e.0), i.iter().map(|r| line(&r.0)).collect());
    match g {
        Geometry::Point(c) => geo::Point(coord(c)).into(),
        Geometry::LineString(cs) => line(cs).into(),
        Geometry::Polygon { exterior, interiors } => poly(exterior, interiors).into(),
        Geometry::MultiPoint(cs) => geo::MultiPoint(cs.iter().map(|c| geo::Point(coord(c))).collect()).into(),
        Geometry::MultiLineString(ls) => geo::MultiLineString(ls.iter().map(|cs| line(cs)).collect()).into(),
        Geometry::MultiPolygon(ps) => geo::MultiPolygon(ps.iter().map(|(e, i)| poly(e, i)).collect()).into(),
        Geometry::GeometryCollection(gs) => geo::Geometry::GeometryCollection(geo::GeometryCollection(gs.iter().map(to_geo).collect())),
    }
}

fn from_polygons(mp: geo::MultiPolygon<f64>) -> Geometry {
    let ring = |r: geo::LineString<f64>| Ring(r.0.into_iter().map(|c| Coord::xy(c.x, c.y)).collect());
    Geometry::MultiPolygon(
        mp.0.into_iter()
            .map(|p| {
                let (e, i) = p.into_inner();
                (ring(e), i.into_iter().map(ring).collect())
            })
            .collect(),
    )
}

struct Group {
    values: Vec<FieldValue>,
    shapes: Vec<geo::MultiPolygon<f64>>,
    count: usize,
    has_geometry: bool,
}

#[derive(Debug, PartialEq, Eq, Hash)]
enum Key {
    Integer(i64),
    Float(u64),
    Text(String),
    Boolean(bool),
    Blob(Vec<u8>),
    Date(String),
    DateTime(String),
    Null,
}

fn key(value: &FieldValue) -> Result<Key> {
    Ok(match value {
        FieldValue::Integer(v) => Key::Integer(*v),
        FieldValue::Float(v) if v.is_finite() => Key::Float(if *v == 0.0 { 0 } else { v.to_bits() }),
        FieldValue::Float(_) => return Err(Error::InvalidArgument("dissolve group fields must not contain non-finite numbers".into())),
        FieldValue::Text(v) => Key::Text(v.clone()),
        FieldValue::Boolean(v) => Key::Boolean(*v),
        FieldValue::Blob(v) => Key::Blob(v.clone()),
        FieldValue::Date(v) => Key::Date(v.clone()),
        FieldValue::DateTime(v) => Key::DateTime(v.clone()),
        FieldValue::Null => Key::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feature_context_preserves_error_categories() {
        let mut layer = Layer::new("input");
        layer.features.push(Feature { fid: 42, geometry: Some(Geometry::point(0.0, 0.0)), attributes: vec![] });
        let argument = map_layer(&layer, |_| Err(Error::InvalidArgument("non-finite XY coordinates".into()))).unwrap_err();
        assert!(matches!(argument, Error::InvalidArgument(_)));
        assert_eq!(argument.to_string(), "feature 42: non-finite XY coordinates");
        let data = map_layer(&layer, |_| Err(Error::InvalidData("polygon repair failed".into()))).unwrap_err();
        assert!(matches!(data, Error::InvalidData(_)));
        assert_eq!(data.to_string(), "feature 42: polygon repair failed");
    }
}

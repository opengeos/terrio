//! DWG and DXF vector interchange using acadrust's pure-Rust codecs.
//! CAD entities become one `entities` layer, with drawing layer names as attributes.
//! Curves are linearized at at most four degrees per segment. Drawing coordinates
//! are retained without assigning a geographic CRS. This is geometry interchange,
//! not a lossless CAD document editor.

use std::f64::consts::{PI, TAU};
use std::io::Cursor;

use acadrust::entities::hatch::{BoundaryEdge, BoundaryPath, PolylineEdge};
use acadrust::entities::{EntityType, Hatch, Line, Point, Polyline3D, Text};
use acadrust::types::{Matrix3, Vector2, Vector3};
use acadrust::{CadDocument, DwgReader, DwgWriter, DxfReader, DxfWriter};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer, Ring};

use crate::formats::VectorDriver;
use crate::{Error, Result};

fn codec_error(e: acadrust::DxfError) -> Error {
    Error::InvalidData(format!("CAD: {e}"))
}

fn unsupported(entity: &EntityType) -> Error {
    Error::Unsupported(format!("CAD: unsupported {} entity (handle {})", entity.as_entity().entity_type(), entity.common().handle))
}

const MAX_CAD_WORK: usize = 1_000_000;

pub(crate) fn parse(bytes: Vec<u8>, driver: VectorDriver) -> Result<Layer> {
    let stream = Cursor::new(bytes);
    let doc = if driver == VectorDriver::DWG {
        DwgReader::from_stream(stream).read().map_err(codec_error)?
    } else {
        DxfReader::from_reader(stream).map_err(codec_error)?.read().map_err(codec_error)?
    };
    layer_from_document(doc, MAX_CAD_WORK)
}

fn layer_from_document(mut doc: CadDocument, max_work: usize) -> Result<Layer> {
    let mut budget = WorkBudget { remaining: max_work };
    let mut layer = Layer::new("entities");
    for name in ["Layer", "EntityHandle", "EntityType", "Linetype", "Text"] {
        layer.add_field(FieldDef::new(name, FieldType::Text));
    }
    layer.add_field(FieldDef::new("PaperSpace", FieldType::Integer));
    // Only drawable spaces: unused block definitions must not create duplicate features.
    let roots: Vec<_> = doc
        .block_records
        .iter()
        .filter(|b| b.name.eq_ignore_ascii_case("*Model_Space") || b.name.to_ascii_lowercase().starts_with("*paper_space"))
        .flat_map(|b| b.entity_handles.iter().map(move |h| (*h, !b.name.eq_ignore_ascii_case("*Model_Space"))))
        .collect();
    for (handle, paper) in roots {
        // Arc shares the document's entity; expansion can mutate the document
        // without deep-cloning all root geometries up front.
        let entity = doc.get_entity_arc(handle).ok_or_else(|| Error::InvalidData(format!("CAD: missing entity {handle}")))?;
        push_entity(&mut doc, &entity, paper, 0, &mut budget, &mut layer)?;
    }
    Ok(layer)
}

struct WorkBudget {
    remaining: usize,
}

impl WorkBudget {
    fn spend(&mut self, count: usize) -> Result<()> {
        self.remaining =
            self.remaining.checked_sub(count).ok_or_else(|| Error::InvalidData("CAD: entity expansion work limit exceeded".into()))?;
        Ok(())
    }
}

fn push_entity(
    doc: &mut CadDocument,
    entity: &EntityType,
    paper: bool,
    depth: usize,
    budget: &mut WorkBudget,
    layer: &mut Layer,
) -> Result<()> {
    // Count skipped leaves and INSERTs too, not only emitted features.
    budget.spend(1)?;
    if depth > 32 || layer.features.len() >= 1_000_000 {
        return Err(Error::InvalidData("CAD: block expansion exceeds the depth (32) or feature (1000000) limit".into()));
    }
    if let EntityType::Insert(insert) = entity {
        // acadrust refuses MINSERT in its document-aware explosion API. Reject
        // arrays before inspecting/allocating any of their potential instances.
        if insert.is_minsert() {
            return Err(unsupported(entity));
        }
        let record = doc
            .block_records
            .get(&insert.block_name)
            .ok_or_else(|| Error::InvalidData(format!("CAD: missing block '{}'", insert.block_name)))?;
        if !record.explodable
            || record.flags.is_xref
            || record.flags.is_xref_overlay
            || record.flags.is_external
            || !record.xref_path.is_empty()
        {
            return Err(unsupported(entity));
        }
        if record.entity_handles.is_empty() && insert.attributes.is_empty() {
            return Ok(());
        }
        // acadrust 0.6.7 keeps nested INSERTs as references. Its shear fallback
        // can clone one child block, but does not recursively expand that child.
        // The upstream_insert_expansion_keeps_nested_references test locks this
        // assumption down for both uniform and non-uniform parent transforms.
        // Reserve cloning work before calling the eager upstream expander.
        // Stretched bulged polylines can produce one entity per segment, and
        // nested INSERT shear fallbacks can clone the referenced child block.
        budget.spend(record.entity_handles.len())?;
        for handle in &record.entity_handles {
            let member = doc.get_entity(*handle).ok_or_else(|| Error::InvalidData(format!("CAD: missing block member {handle}")))?;
            budget.spend(clone_extra_work(member)?)?;
            if let EntityType::Insert(nested) = member
                && let Some(child) = doc.block_records.get(&nested.block_name)
            {
                budget.spend(child.entity_handles.len())?;
                for child_handle in &child.entity_handles {
                    let child_member = doc
                        .get_entity(*child_handle)
                        .ok_or_else(|| Error::InvalidData(format!("CAD: missing block member {child_handle}")))?;
                    budget.spend(clone_extra_work(child_member)?)?;
                }
            }
        }
        let parts = doc.explode_entity(entity);
        budget.spend(parts.len())?;
        if parts.is_empty() {
            return Err(unsupported(entity));
        }
        for part in parts {
            push_entity(doc, &part, paper, depth + 1, budget, layer)?;
        }
        return Ok(());
    }
    // VIEWPORT describes a view into the drawing, rather than vector geometry.
    if matches!(entity, EntityType::Viewport(_) | EntityType::Seqend(_)) {
        return Ok(());
    }
    let mut geometry = geometry(entity)?;
    crate::vector::map_coords(&mut geometry, &mut |c| vector(c).map(|_| ()).map_err(|error| Error::InvalidData(error.to_string())))?;
    let text = match entity {
        EntityType::Text(t) => FieldValue::Text(t.value.clone()),
        EntityType::MText(t) => FieldValue::Text(t.value.clone()),
        _ => FieldValue::Null,
    };
    let common = entity.common();
    let attributes = vec![
        FieldValue::Text(common.layer.clone()),
        FieldValue::Text(format!("{:X}", common.handle.value())),
        FieldValue::Text(entity.as_entity().entity_type().into()),
        if common.linetype.is_empty() { FieldValue::Null } else { FieldValue::Text(common.linetype.clone()) },
        text,
        if paper { FieldValue::Integer(1) } else { FieldValue::Null },
    ];
    layer.features.push(Feature { fid: layer.features.len() as u64, geometry: Some(geometry), attributes });
    Ok(())
}

fn clone_extra_work(entity: &EntityType) -> Result<usize> {
    ensure_native_properties(entity)?;
    Ok(match entity {
        EntityType::LwPolyline(p) => p.vertices.len(),
        EntityType::Polyline2D(p) => p.vertices.len(),
        EntityType::Insert(i) => i.attributes.len(),
        // Avoid an upstream modeler decomposition outside our supported scope.
        EntityType::Solid3D(_) | EntityType::Region(_) | EntityType::Body(_) => return Err(unsupported(entity)),
        _ => 0,
    })
}

fn coord(v: Vector3) -> Coord {
    Coord::xyz(v.x, v.y, v.z)
}

fn ocs(v: Vector3, normal: Vector3) -> Vector3 {
    Matrix3::arbitrary_axis(normal).transform_point(v)
}

fn line(mut cs: Vec<Coord>, closed: bool) -> Result<Geometry> {
    if cs.is_empty() {
        return Err(Error::InvalidData("CAD: polyline has no vertices".into()));
    }
    if cs.len() == 1 {
        return Ok(Geometry::Point(cs.remove(0)));
    }
    if closed && cs.first() != cs.last() {
        cs.push(cs[0].clone());
    }
    Ok(Geometry::LineString(cs))
}

/// Samples in the direction of `sweep`, preserving both ends. Full curves close exactly.
fn curve(start: f64, sweep: f64, at: impl Fn(f64) -> Vector3) -> Result<Vec<Coord>> {
    if !start.is_finite() || !sweep.is_finite() || sweep.abs() > TAU + 1e-8 {
        return Err(Error::InvalidData("CAD: invalid curve angles".into()));
    }
    let count = (sweep.abs() / (PI / 45.0)).ceil().max(1.0) as usize;
    let mut cs: Vec<_> = (0..=count).map(|i| coord(at(start + sweep * i as f64 / count as f64))).collect();
    if (sweep.abs() - TAU).abs() < 1e-12 {
        cs[count] = cs[0].clone();
    }
    if cs.iter().any(|c| !c.x.is_finite() || !c.y.is_finite() || !c.z.unwrap_or(0.0).is_finite()) {
        return Err(Error::InvalidData("CAD: curve has non-finite coordinates".into()));
    }
    Ok(cs)
}

fn ccw_sweep(start: f64, end: f64) -> f64 {
    let sweep = (end - start).rem_euclid(TAU);
    if sweep.abs() < 1e-12 { TAU } else { sweep }
}

/// LWPOLYLINE and hatch polyline vertices store bulge, not a third coordinate.
fn polyline(vertices: &[(Vector2, f64)], closed: bool, elevation: f64, normal: Vector3) -> Result<Vec<Coord>> {
    let mut cs = Vec::new();
    let count = vertices.len();
    for (i, &(a, bulge)) in vertices.iter().enumerate() {
        let at = |x, y| ocs(Vector3::new(x, y, elevation), normal);
        cs.push(coord(at(a.x, a.y)));
        if i + 1 == count && !closed {
            break;
        }
        let b = vertices[(i + 1) % count].0;
        if !bulge.is_finite() {
            return Err(Error::InvalidData("CAD: non-finite polyline bulge".into()));
        }
        if bulge.abs() > 1e-12 && a != b {
            let (dx, dy) = (b.x - a.x, b.y - a.y);
            let offset = (1.0 - bulge * bulge) / (4.0 * bulge);
            let center = Vector2::new((a.x + b.x) / 2.0 - dy * offset, (a.y + b.y) / 2.0 + dx * offset);
            let radius = a.distance(&center);
            let start = (a.y - center.y).atan2(a.x - center.x);
            let arc = curve(start, 4.0 * bulge.atan(), |t| at(center.x + radius * t.cos(), center.y + radius * t.sin()))?;
            // The next segment adds its own exact endpoint.
            let interior = arc.len().saturating_sub(2);
            cs.extend(arc.into_iter().skip(1).take(interior));
        }
    }
    cs.dedup();
    if closed && cs.len() > 1 {
        cs.push(cs[0].clone());
    }
    Ok(cs)
}

/// Check before transforming too: an upstream non-uniform transform may turn
/// a thick circle/arc into an ELLIPSE, which has no thickness field to validate.
fn ensure_native_properties(entity: &EntityType) -> Result<()> {
    let valid = match entity {
        EntityType::Point(p) => p.thickness == 0.0,
        EntityType::Line(l) => l.thickness == 0.0,
        EntityType::LwPolyline(p) => p.thickness == 0.0,
        EntityType::Polyline2D(p) => p.thickness == 0.0 && p.flags.bits() & 0x56 == 0,
        EntityType::Polyline(p) => p.flags.bits() & 0x56 == 0,
        EntityType::Polyline3D(p) => !p.flags.spline_fit && !p.flags.is_3d_mesh && !p.flags.is_polyface_mesh,
        EntityType::Circle(c) => c.thickness == 0.0,
        EntityType::Arc(a) => a.thickness == 0.0,
        EntityType::Text(t) => t.thickness == 0.0,
        EntityType::Solid(s) => s.thickness == 0.0,
        _ => true,
    };
    if valid { Ok(()) } else { Err(unsupported(entity)) }
}

fn geometry(entity: &EntityType) -> Result<Geometry> {
    ensure_native_properties(entity)?;
    Ok(match entity {
        EntityType::Point(p) => Geometry::Point(coord(p.location)),
        EntityType::Line(l) => Geometry::LineString(vec![coord(l.start), coord(l.end)]),
        EntityType::LwPolyline(p) => line(
            polyline(&p.vertices.iter().map(|v| (v.location, v.bulge)).collect::<Vec<_>>(), p.is_closed, p.elevation, p.normal)?,
            false,
        )?,
        EntityType::Polyline2D(p) => {
            let vertices: Vec<_> = p.vertices.iter().map(|v| (Vector2::new(v.location.x, v.location.y), v.bulge)).collect();
            line(polyline(&vertices, p.is_closed(), p.elevation, p.normal)?, false)?
        }
        EntityType::Polyline(p) => line(p.vertices.iter().map(|v| coord(v.location)).collect(), p.flags.is_closed())?,
        EntityType::Polyline3D(p) => line(p.vertices.iter().map(|v| coord(v.position)).collect(), p.is_closed())?,
        EntityType::Circle(c) => Geometry::LineString(curve(0.0, TAU, |t| c.point_at_angle_wcs(t))?),
        EntityType::Arc(a) => {
            Geometry::LineString(curve(a.start_angle, ccw_sweep(a.start_angle, a.end_angle), |t| a.point_at_angle_wcs(t))?)
        }
        EntityType::Ellipse(e) => {
            let minor = e.normal.normalize().cross(&e.major_axis) * e.minor_axis_ratio;
            Geometry::LineString(curve(e.start_parameter, ccw_sweep(e.start_parameter, e.end_parameter), |t| {
                e.center + e.major_axis * t.cos() + minor * t.sin()
            })?)
        }
        EntityType::Text(t) => Geometry::Point(coord(ocs(t.insertion_point, t.normal))),
        EntityType::MText(t) => Geometry::Point(coord(t.insertion_point)),
        EntityType::Face3D(f) => polygon(f.corners().into_iter().map(coord).collect())?,
        EntityType::Solid(s) => polygon(s.boundary_corners().into_iter().map(|v| coord(ocs(v, s.normal))).collect())?,
        EntityType::Hatch(h) => hatch_geometry(h)?,
        _ => return Err(unsupported(entity)),
    })
}

fn polygon(cs: Vec<Coord>) -> Result<Geometry> {
    let mut distinct = Vec::new();
    for c in &cs {
        if !distinct.contains(c) {
            distinct.push(c.clone());
        }
    }
    if distinct.len() < 3 {
        return line(distinct, false);
    }
    // Faces have at most four corners; remove repeated corners before closing.
    distinct.push(distinct[0].clone());
    Ok(Geometry::Polygon { exterior: Ring::new(distinct), interiors: Vec::new() })
}

fn hatch_geometry(h: &Hatch) -> Result<Geometry> {
    let mut rings = Vec::new();
    let mut external = Vec::new();
    for path in &h.paths {
        let mut cs: Vec<Coord> = Vec::new();
        for (edge_index, edge) in path.edges.iter().enumerate() {
            let at = |x, y| Vector3::new(x, y, h.elevation);
            let mut part = match edge {
                BoundaryEdge::Polyline(p) => polyline(
                    &p.vertices.iter().map(|v| (Vector2::new(v.x, v.y), v.z)).collect::<Vec<_>>(),
                    p.is_closed,
                    h.elevation,
                    Vector3::UNIT_Z,
                )?,
                BoundaryEdge::Line(l) => vec![coord(at(l.start.x, l.start.y)), coord(at(l.end.x, l.end.y))],
                BoundaryEdge::CircularArc(a) => {
                    let sweep =
                        if a.counter_clockwise { ccw_sweep(a.start_angle, a.end_angle) } else { -ccw_sweep(a.end_angle, a.start_angle) };
                    curve(a.start_angle, sweep, |t| at(a.center.x + a.radius * t.cos(), a.center.y + a.radius * t.sin()))?
                }
                _ => {
                    return Err(Error::Unsupported(
                        "CAD: unsupported hatch boundary (only polylines, lines and circular arcs are supported)".into(),
                    ));
                }
            };
            if edge_index == 1
                && let (Some(first), Some(last), Some(start), Some(end)) = (cs.first(), cs.last(), part.first(), part.last())
                && (last.x - start.x).hypot(last.y - start.y) > 1e-7
                && (last.x - end.x).hypot(last.y - end.y) > 1e-7
                && ((first.x - start.x).hypot(first.y - start.y) <= 1e-7 || (first.x - end.x).hypot(first.y - end.y) <= 1e-7)
            {
                cs.reverse();
            }
            if let (Some(last), Some(first)) = (cs.last(), part.first()) {
                if (last.x - first.x).hypot(last.y - first.y) > 1e-7 {
                    part.reverse();
                    if (last.x - part[0].x).hypot(last.y - part[0].y) > 1e-7 {
                        return Err(Error::Unsupported("CAD: unordered hatch boundary edges".into()));
                    }
                }
                cs.extend(part.into_iter().skip(1));
            } else {
                cs.extend(part);
            }
        }
        if let (Some(first), Some(last)) = (cs.first(), cs.last())
            && (first.x - last.x).hypot(first.y - last.y) > 1e-7
        {
            return Err(Error::Unsupported("CAD: unclosed hatch boundary".into()));
        }
        rings.push(cs);
        external.push(path.flags.is_external());
    }
    let mut geometry = hatch_polygons(rings, &external)?;
    crate::vector::map_coords(&mut geometry, &mut |c| {
        *c = coord(ocs(Vector3::new(c.x, c.y, c.z.unwrap_or(0.0)), h.normal));
        Ok(())
    })?;
    Ok(geometry)
}

/// Explicit external paths are independent shells, even if they overlap. Other
/// paths go to the smallest containing shell, then use parity for holes/islands.
fn hatch_polygons(rings: Vec<Vec<Coord>>, external: &[bool]) -> Result<Geometry> {
    let rings: Vec<_> = rings
        .into_iter()
        .map(|mut cs| {
            if cs.len() > 1 && cs.first() == cs.last() {
                cs.pop();
            }
            cs
        })
        .collect();
    if rings.iter().any(|cs| cs.len() < 3) {
        return Err(Error::InvalidData("CAD: hatch boundary has fewer than three vertices".into()));
    }
    let mut groups: Vec<Vec<Vec<Coord>>> = Vec::new();
    let shells: Vec<_> = rings.iter().enumerate().filter(|(i, _)| external[*i]).map(|(i, cs)| (i, Ring::new(cs.clone()))).collect();
    if shells.is_empty() {
        groups.push(rings);
    } else {
        groups.extend(shells.iter().map(|(_, r)| vec![r.0.clone()]));
        let mut unassigned = Vec::new();
        for (i, cs) in rings.into_iter().enumerate() {
            if external[i] {
                continue;
            }
            let owner = shells
                .iter()
                .enumerate()
                .filter(|(_, (_, r))| cs.iter().all(|c| super::shapefile::ring_contains(r, c.x, c.y)))
                .min_by(|(_, (_, a)), (_, (_, b))| a.signed_area().abs().total_cmp(&b.signed_area().abs()))
                .map(|(j, _)| j);
            if let Some(j) = owner {
                groups[j].push(cs);
            } else {
                unassigned.push(cs);
            }
        }
        if !unassigned.is_empty() {
            groups.push(unassigned);
        }
    }
    let mut polygons = Vec::new();
    for group in groups {
        match super::polygons_from_rings(group) {
            Some(Geometry::Polygon { exterior, interiors }) => polygons.push((exterior, interiors)),
            Some(Geometry::MultiPolygon(ps)) => polygons.extend(ps),
            _ => return Err(Error::InvalidData("CAD: hatch has no polygon boundaries".into())),
        }
    }
    match polygons.len() {
        0 => Err(Error::InvalidData("CAD: hatch has no polygon boundaries".into())),
        1 => {
            let (exterior, interiors) = polygons.remove(0);
            Ok(Geometry::Polygon { exterior, interiors })
        }
        _ => Ok(Geometry::MultiPolygon(polygons)),
    }
}

/// Encodes an R2018 document. Arbitrary GIS attributes and CRS have no native CAD
/// equivalent; only Layer, Linetype, Text and PaperSpace are mapped back.
pub(crate) fn to_bytes(layers: &[Layer], driver: VectorDriver) -> Result<Vec<u8>> {
    let mut doc = CadDocument::new();
    for layer in layers {
        for f in &layer.features {
            let Some(g) = &f.geometry else {
                return Err(Error::Unsupported("CAD: cannot write a feature without geometry".into()));
            };
            let text = |name| {
                layer.schema.field_index(name).and_then(|i| f.attributes.get(i)).and_then(|v| match v {
                    FieldValue::Text(s) => Some(s.as_str()),
                    _ => None,
                })
            };
            let name = text("Layer").filter(|s| !s.is_empty()).unwrap_or(&layer.name);
            let name = if name.is_empty() { "0" } else { name };
            validate_symbol_name(name, "Layer")?;
            if !doc.layers.contains(name) {
                doc.layers.add(acadrust::tables::Layer::new(name)).map_err(codec_error)?;
            }
            let linetype = text("Linetype").unwrap_or("");
            validate_symbol_name(linetype, "Linetype")?;
            if !linetype.is_empty() && !doc.line_types.contains(linetype) {
                // The name is useful for filtering; GIS fields do not carry the
                // CAD dash pattern, so no pattern is invented here.
                doc.line_types.add(acadrust::tables::LineType::new(linetype)).map_err(codec_error)?;
            }
            let paper = layer.schema.field_index("PaperSpace").and_then(|i| f.attributes.get(i)) == Some(&FieldValue::Integer(1));
            let mut entities = Vec::new();
            encode_geometry(g, text("Text").filter(|s| !s.is_empty()), text("EntityType"), &mut entities)?;
            for mut entity in entities {
                entity.common_mut().layer = name.into();
                entity.common_mut().linetype = linetype.into();
                if paper {
                    doc.add_paper_space_entity(entity).map_err(codec_error)?;
                } else {
                    doc.add_entity(entity).map_err(codec_error)?;
                }
            }
        }
    }
    if driver == VectorDriver::DWG {
        DwgWriter::write_to_vec(&doc).map_err(codec_error)
    } else {
        DxfWriter::new(&doc).write_to_vec().map_err(codec_error)
    }
}

fn vector(c: &Coord) -> Result<Vector3> {
    if !c.x.is_finite() || !c.y.is_finite() || !c.z.unwrap_or(0.0).is_finite() {
        return Err(Error::InvalidArgument("CAD: coordinates must be finite".into()));
    }
    if c.m.is_some() {
        return Err(Error::Unsupported("CAD: measured coordinates have no CAD equivalent".into()));
    }
    Ok(Vector3::new(c.x, c.y, c.z.unwrap_or(0.0)))
}

// Extended symbol-name rules: https://help.autodesk.com/cloudhelp/2023/ENU/AutoCAD-AutoLISP-Reference/files/GUID-2EFBE198-9860-456B-A090-206C743ACB90.htm
fn validate_symbol_name(name: &str, field: &str) -> Result<()> {
    if name.chars().any(|c| c.is_control() || matches!(c, '<' | '>' | '/' | '\\' | ':' | ';' | '?' | '*' | '|' | ',' | '=' | '`' | '"')) {
        return Err(Error::InvalidArgument(format!("CAD: invalid {field} name {name:?}: contains a forbidden character")));
    }
    Ok(())
}

fn encode_geometry(g: &Geometry, text: Option<&str>, entity_type: Option<&str>, out: &mut Vec<EntityType>) -> Result<()> {
    match g {
        Geometry::Point(c) => {
            let location = vector(c)?;
            let entity = match entity_type.map(str::to_ascii_uppercase).as_deref() {
                Some("POINT") => EntityType::Point(Point::at(location)),
                Some("TEXT") => EntityType::Text(Text::with_value(text.unwrap_or(""), location)),
                Some("MTEXT") => EntityType::MText(Box::new(acadrust::entities::MText::with_value(text.unwrap_or(""), location))),
                _ => match text {
                    Some(text) => EntityType::Text(Text::with_value(text, location)),
                    None => EntityType::Point(Point::at(location)),
                },
            };
            out.push(entity);
        }
        Geometry::LineString(cs) => {
            if cs.len() < 2 {
                return Err(Error::InvalidArgument("CAD: a line needs at least two vertices".into()));
            }
            let points = cs.iter().map(vector).collect::<Result<Vec<_>>>()?;
            if points.len() == 2 {
                out.push(EntityType::Line(Line::from_points(points[0], points[1])));
            } else {
                out.push(EntityType::Polyline3D(Polyline3D::from_points(points)));
            }
        }
        Geometry::Polygon { exterior, interiors } => {
            let z = exterior.0.first().and_then(|c| c.z).unwrap_or(0.0);
            let mut h = Hatch::solid().with_elevation(z);
            for (i, ring) in std::iter::once(exterior).chain(interiors).enumerate() {
                if ring.0.len() < 3 {
                    return Err(Error::InvalidArgument("CAD: a polygon ring needs at least three vertices".into()));
                }
                let mut vs = Vec::new();
                for c in &ring.0 {
                    let v = vector(c)?;
                    if v.z != z {
                        return Err(Error::Unsupported("CAD: polygon rings must share one Z elevation".into()));
                    }
                    vs.push(Vector2::new(v.x, v.y));
                }
                if vs.first() == vs.last() {
                    vs.pop();
                }
                let mut path = if i == 0 { BoundaryPath::external() } else { BoundaryPath::new() };
                path.flags.set_polyline(true);
                path.add_edge(BoundaryEdge::Polyline(PolylineEdge::new(vs, true)));
                h.add_path(path);
            }
            out.push(EntityType::Hatch(Box::new(h)));
        }
        Geometry::MultiPoint(cs) => {
            for c in cs {
                encode_geometry(&Geometry::Point(c.clone()), text, entity_type, out)?;
            }
        }
        Geometry::MultiLineString(ls) => {
            for cs in ls {
                encode_geometry(&Geometry::LineString(cs.clone()), None, None, out)?;
            }
        }
        Geometry::MultiPolygon(ps) => {
            for (e, hs) in ps {
                encode_geometry(&Geometry::Polygon { exterior: e.clone(), interiors: hs.clone() }, None, None, out)?;
            }
        }
        Geometry::GeometryCollection(gs) => {
            for g in gs {
                encode_geometry(g, text, entity_type, out)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Check native rejection directly, even when the public API has GDAL fallback.
    #[test]
    fn closed_single_vertex_polyline_is_a_point() {
        for count in [1, 3] {
            let mut p = acadrust::entities::LwPolyline::from_points(vec![Vector2::new(3.0, 7.0); count]);
            p.close();
            p.elevation = 12.0;
            let mut doc = CadDocument::new();
            doc.add_entity(EntityType::LwPolyline(p)).unwrap();
            for (bytes, driver) in [
                (DxfWriter::new(&doc).write_to_vec().unwrap(), VectorDriver::DXF),
                (DwgWriter::write_to_vec(&doc).unwrap(), VectorDriver::DWG),
            ] {
                let layer = parse(bytes, driver).unwrap();
                assert_eq!(layer.features[0].geometry, Some(Geometry::Point(Coord::xyz(3.0, 7.0, 12.0))));
            }
        }
    }

    #[test]
    fn missing_or_empty_linetype_uses_upstream_by_layer_default() {
        assert!(Point::at(Vector3::ZERO).common.linetype.is_empty());
        for with_field in [false, true] {
            let mut layer = Layer::new("survey");
            if with_field {
                layer.add_field(FieldDef::new("Linetype", FieldType::Text));
            }
            layer.features.push(Feature {
                fid: 0,
                geometry: Some(Geometry::Point(Coord::xyz(1.0, 2.0, 3.0))),
                attributes: if with_field { vec![FieldValue::Text(String::new())] } else { Vec::new() },
            });
            for driver in [VectorDriver::DXF, VectorDriver::DWG] {
                let stream = Cursor::new(to_bytes(std::slice::from_ref(&layer), driver).unwrap());
                let doc = if driver == VectorDriver::DWG {
                    DwgReader::from_stream(stream).read().unwrap()
                } else {
                    DxfReader::from_reader(stream).unwrap().read().unwrap()
                };
                let block = doc.block_records.iter().find(|b| b.name.eq_ignore_ascii_case("*Model_Space")).unwrap();
                assert_eq!(block.entity_handles.len(), 1);
                let entity = doc.get_entity_arc(block.entity_handles[0]).unwrap();
                assert!(!entity.common().has_linetype(), "expected ByLayer: {:?}", entity.common().linetype);
            }
        }
    }

    #[test]
    fn mixed_case_layer_names_share_one_symbol() {
        let mut layer = Layer::new("survey");
        layer.add_field(FieldDef::new("Layer", FieldType::Text));
        for (fid, name) in ["Roads", "ROADS"].into_iter().enumerate() {
            layer.features.push(Feature {
                fid: fid as u64,
                geometry: Some(Geometry::Point(Coord::xyz(fid as f64, 2.0, 3.0))),
                attributes: vec![FieldValue::Text(name.into())],
            });
        }
        for driver in [VectorDriver::DXF, VectorDriver::DWG] {
            let stream = Cursor::new(to_bytes(std::slice::from_ref(&layer), driver).unwrap());
            let doc = if driver == VectorDriver::DWG {
                DwgReader::from_stream(stream).read().unwrap()
            } else {
                DxfReader::from_reader(stream).unwrap().read().unwrap()
            };
            assert_eq!(doc.layers.iter().filter(|l| l.name.eq_ignore_ascii_case("roads")).count(), 1);
            assert_eq!(layer_from_document(doc, 8).unwrap().features.len(), 2);
        }
    }

    #[test]
    fn native_reader_rejects_unsupported_splines() {
        let mut doc = CadDocument::new();
        let spline = acadrust::entities::Spline::from_control_points(
            2,
            vec![Vector3::ZERO, Vector3::new(1.0, 1.0, 0.0), Vector3::new(2.0, 0.0, 0.0)],
        );
        doc.add_entity(EntityType::Spline(spline)).unwrap();
        for (bytes, driver) in
            [(DxfWriter::new(&doc).write_to_vec().unwrap(), VectorDriver::DXF), (DwgWriter::write_to_vec(&doc).unwrap(), VectorDriver::DWG)]
        {
            let error = parse(bytes, driver).unwrap_err();
            assert!(matches!(error, Error::Unsupported(ref message) if message.contains("unsupported SPLINE")), "{error}");
        }
    }

    fn add_block(doc: &mut CadDocument, name: &str, entities: Vec<EntityType>) {
        let mut block = acadrust::tables::BlockRecord::new(name);
        block.handle = doc.allocate_handle();
        let owner = block.handle;
        doc.block_records.add(block).unwrap();
        for mut e in entities {
            e.common_mut().owner_handle = owner;
            doc.add_entity(e).unwrap();
        }
    }

    #[test]
    fn expansion_budget_counts_skipped_leaves_and_fanout() {
        let mut doc = CadDocument::new();
        add_block(&mut doc, "leaf", vec![EntityType::Viewport(Box::default())]);
        let mut child = "leaf".to_string();
        for level in 0..8 {
            let name = format!("level{level}");
            let children = (0..2).map(|_| EntityType::Insert(acadrust::entities::Insert::new(&child, Vector3::ZERO))).collect();
            add_block(&mut doc, &name, children);
            child = name;
        }
        doc.add_entity(EntityType::Insert(acadrust::entities::Insert::new(child, Vector3::ZERO))).unwrap();
        // Below the depth cap and no emitted features: the old limits miss this.
        let err = layer_from_document(doc, 64).unwrap_err();
        assert!(err.to_string().contains("work limit"), "{err}");
    }

    #[test]
    fn eager_expansion_is_budgeted_before_cloning() {
        let mut doc = CadDocument::new();
        let p = acadrust::entities::LwPolyline::from_points((0..100).map(|i| Vector2::new(i as f64, 0.0)).collect());
        add_block(&mut doc, "large", vec![EntityType::LwPolyline(p)]);
        let insert = EntityType::Insert(acadrust::entities::Insert::new("large", Vector3::ZERO));
        let next_handle = doc.next_handle();
        let mut layer = Layer::new("entities");
        let error = push_entity(&mut doc, &insert, false, 0, &mut WorkBudget { remaining: 16 }, &mut layer).unwrap_err();
        assert!(error.to_string().contains("work limit"));
        assert_eq!(doc.next_handle(), next_handle, "upstream explosion must not run");
    }

    #[test]
    fn reject_mesh_flags_and_thickness_consistently() {
        for bits in [16, 64] {
            let mut p = acadrust::entities::Polyline::from_points(vec![Vector3::ZERO, Vector3::UNIT_X]);
            p.flags = acadrust::entities::PolylineFlags::from_bits(bits);
            assert!(matches!(geometry(&EntityType::Polyline(p)), Err(Error::Unsupported(_))));
        }
        let mut p = Point::at(Vector3::ZERO);
        p.thickness = 2.0;
        let mut t = Text::with_value("label", Vector3::ZERO);
        t.thickness = 2.0;
        for entity in [EntityType::Point(p), EntityType::Text(t)] {
            assert!(matches!(geometry(&entity), Err(Error::Unsupported(_))));
        }
    }

    #[test]
    fn degenerate_faces_use_their_distinct_vertices() {
        assert_eq!(
            polygon(vec![coord(Vector3::ZERO), coord(Vector3::UNIT_X), coord(Vector3::ZERO)]).unwrap(),
            Geometry::LineString(vec![coord(Vector3::ZERO), coord(Vector3::UNIT_X)])
        );
        assert_eq!(polygon(vec![coord(Vector3::ZERO); 4]).unwrap(), Geometry::Point(coord(Vector3::ZERO)));
    }

    #[test]
    fn repeated_face_corner_produces_a_triangle() {
        let a = coord(Vector3::ZERO);
        let b = coord(Vector3::UNIT_X);
        let c = coord(Vector3::UNIT_Y);
        assert_eq!(
            polygon(vec![a.clone(), b.clone(), a.clone(), c.clone()]).unwrap(),
            Geometry::Polygon { exterior: Ring::new(vec![a.clone(), b, c, a]), interiors: Vec::new() }
        );
    }

    #[test]
    fn hatch_arc_units_and_reversed_first_edge_roundtrip() {
        use acadrust::entities::hatch::{CircularArcEdge, LineEdge};
        for reversed in [false, true] {
            let mut path = BoundaryPath::external();
            let a = Vector2::new(-1.0, 0.0);
            let b = Vector2::new(1.0, 0.0);
            path.add_edge(BoundaryEdge::Line(LineEdge { start: if reversed { b } else { a }, end: if reversed { a } else { b } }));
            path.add_edge(BoundaryEdge::CircularArc(CircularArcEdge {
                center: Vector2::ZERO,
                radius: 1.0,
                start_angle: 0.0,
                end_angle: PI,
                counter_clockwise: true,
            }));
            let mut hatch = Hatch::new();
            hatch.add_path(path);
            let mut doc = CadDocument::new();
            doc.add_entity(EntityType::Hatch(Box::new(hatch))).unwrap();
            for (bytes, driver) in [
                (DxfWriter::new(&doc).write_to_vec().unwrap(), VectorDriver::DXF),
                (DwgWriter::write_to_vec(&doc).unwrap(), VectorDriver::DWG),
            ] {
                let layer = parse(bytes, driver).unwrap();
                let Geometry::Polygon { exterior, .. } = layer.features[0].geometry.as_ref().unwrap() else { panic!("expected polygon") };
                let expected = PI / 2.0;
                assert!((exterior.signed_area().abs() - expected).abs() < 0.002);
            }
        }
    }

    #[test]
    fn arrays_are_rejected_before_eager_expansion() {
        let mut doc = CadDocument::new();
        add_block(&mut doc, "array", vec![EntityType::Point(Point::at(Vector3::ZERO))]);
        let mut insert = acadrust::entities::Insert::new("array", Vector3::ZERO);
        insert.row_count = u16::MAX;
        insert.column_count = u16::MAX;
        let next_handle = doc.next_handle();
        let error =
            push_entity(&mut doc, &EntityType::Insert(insert), false, 0, &mut WorkBudget { remaining: 8 }, &mut Layer::new("entities"))
                .unwrap_err();
        assert!(matches!(error, Error::Unsupported(_)));
        assert_eq!(doc.next_handle(), next_handle);
    }

    #[test]
    fn empty_external_references_still_report_unsupported_data() {
        let mut doc = CadDocument::new();
        let mut block = acadrust::tables::BlockRecord::new("external");
        block.handle = doc.allocate_handle();
        block.flags.is_xref = true;
        doc.block_records.add(block).unwrap();
        doc.add_entity(EntityType::Insert(acadrust::entities::Insert::new("external", Vector3::ZERO))).unwrap();
        assert!(matches!(layer_from_document(doc, 8), Err(Error::Unsupported(_))));
    }

    #[test]
    fn thick_curves_in_stretched_blocks_are_rejected() {
        let mut doc = CadDocument::new();
        let mut circle = acadrust::entities::Circle::from_coords(0.0, 0.0, 0.0, 1.0);
        circle.thickness = 2.0;
        add_block(&mut doc, "thick", vec![EntityType::Circle(circle)]);
        let insert = acadrust::entities::Insert::new("thick", Vector3::ZERO).with_scale(2.0, 1.0, 1.0);
        doc.add_entity(EntityType::Insert(insert)).unwrap();
        assert!(matches!(layer_from_document(doc, 64), Err(Error::Unsupported(_))));
    }

    #[test]
    fn invalid_drawing_coordinates_are_invalid_data() {
        let mut doc = CadDocument::new();
        doc.add_entity(EntityType::Point(Point::from_coords(f64::NAN, 0.0, 0.0))).unwrap();
        assert!(matches!(layer_from_document(doc, 8), Err(Error::InvalidData(_))));
    }

    #[test]
    fn upstream_insert_expansion_keeps_nested_references() {
        for stretch in [false, true] {
            let mut doc = CadDocument::new();
            add_block(&mut doc, "leaf", vec![EntityType::Point(Point::at(Vector3::ZERO))]);
            let mut child = "leaf".to_string();
            for level in 0..8 {
                let name = format!("nested{level}");
                let members =
                    (0..2).map(|_| EntityType::Insert(acadrust::entities::Insert::new(&child, Vector3::ZERO).with_rotation(0.4))).collect();
                add_block(&mut doc, &name, members);
                child = name;
            }
            let mut insert = acadrust::entities::Insert::new(child, Vector3::ZERO);
            if stretch {
                insert = insert.with_scale(2.0, 1.0, 1.0);
            }
            let parts = doc.explode_entity(&EntityType::Insert(insert));
            assert_eq!(parts.len(), 2, "only the immediate block members should be expanded");
            for part in &parts {
                let EntityType::Insert(nested) = part else { panic!("nested INSERT must remain a reference") };
                let members: Vec<_> = doc.entities_in_block(&nested.block_name).collect();
                assert_eq!(members.len(), 2);
                assert!(
                    members.iter().all(|member| matches!(member, EntityType::Insert(_))),
                    "shear fallbacks must not recursively expand the referenced descendants"
                );
            }
        }
    }
}

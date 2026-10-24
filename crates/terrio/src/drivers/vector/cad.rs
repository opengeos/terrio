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

pub(crate) fn parse(bytes: Vec<u8>, driver: VectorDriver) -> Result<Layer> {
    let stream = Cursor::new(bytes);
    let mut doc = if driver == VectorDriver::DWG {
        DwgReader::from_stream(stream).read().map_err(codec_error)?
    } else {
        DxfReader::from_reader(stream).map_err(codec_error)?.read().map_err(codec_error)?
    };
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
        .flat_map(|b| doc.entities_in_block(&b.name).map(move |e| (e.clone(), !b.name.eq_ignore_ascii_case("*Model_Space"))))
        .collect();
    for (entity, paper) in roots {
        push_entity(&mut doc, &entity, paper, 0, &mut layer)?;
    }
    Ok(layer)
}

fn push_entity(doc: &mut CadDocument, entity: &EntityType, paper: bool, depth: usize, layer: &mut Layer) -> Result<()> {
    if depth > 32 || layer.features.len() >= 1_000_000 {
        return Err(Error::InvalidData("CAD: block expansion exceeds the depth (32) or feature (1000000) limit".into()));
    }
    if matches!(entity, EntityType::Insert(_)) {
        let parts = doc.explode_entity(entity);
        if parts.is_empty() {
            return Err(unsupported(entity));
        }
        for part in parts {
            push_entity(doc, &part, paper, depth + 1, layer)?;
        }
        return Ok(());
    }
    // VIEWPORT describes a view into the drawing, rather than vector geometry.
    if matches!(entity, EntityType::Viewport(_) | EntityType::Seqend(_)) {
        return Ok(());
    }
    let mut geometry = geometry(entity)?;
    crate::vector::map_coords(&mut geometry, &mut |c| vector(c).map(|_| ()))?;
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
    if closed && !cs.is_empty() {
        cs.push(cs[0].clone());
    }
    Ok(cs)
}

fn geometry(entity: &EntityType) -> Result<Geometry> {
    Ok(match entity {
        EntityType::Point(p) => Geometry::Point(coord(p.location)),
        EntityType::Line(l) if l.thickness == 0.0 => Geometry::LineString(vec![coord(l.start), coord(l.end)]),
        EntityType::LwPolyline(p) if p.thickness == 0.0 => line(
            polyline(&p.vertices.iter().map(|v| (v.location, v.bulge)).collect::<Vec<_>>(), p.is_closed, p.elevation, p.normal)?,
            false,
        )?,
        EntityType::Polyline2D(p) if p.thickness == 0.0 && p.flags.bits() & 6 == 0 => {
            let vertices: Vec<_> = p.vertices.iter().map(|v| (Vector2::new(v.location.x, v.location.y), v.bulge)).collect();
            line(polyline(&vertices, p.is_closed(), p.elevation, p.normal)?, false)?
        }
        EntityType::Polyline(p) if p.flags.bits() & 6 == 0 => {
            line(p.vertices.iter().map(|v| coord(v.location)).collect(), p.flags.is_closed())?
        }
        EntityType::Polyline3D(p) if !p.flags.spline_fit && !p.flags.is_3d_mesh && !p.flags.is_polyface_mesh => {
            line(p.vertices.iter().map(|v| coord(v.position)).collect(), p.is_closed())?
        }
        EntityType::Circle(c) if c.thickness == 0.0 => Geometry::LineString(curve(0.0, TAU, |t| c.point_at_angle_wcs(t))?),
        EntityType::Arc(a) if a.thickness == 0.0 => {
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
        EntityType::Solid(s) if s.thickness == 0.0 => polygon(s.boundary_corners().into_iter().map(|v| coord(ocs(v, s.normal))).collect())?,
        EntityType::Hatch(h) => hatch_geometry(h)?,
        _ => return Err(unsupported(entity)),
    })
}

fn polygon(mut cs: Vec<Coord>) -> Result<Geometry> {
    cs.dedup();
    if cs.len() < 3 {
        return line(cs, false);
    }
    if cs.first() != cs.last() {
        cs.push(cs[0].clone());
    }
    Ok(Geometry::Polygon { exterior: Ring::new(cs), interiors: Vec::new() })
}

fn hatch_geometry(h: &Hatch) -> Result<Geometry> {
    let mut rings = Vec::new();
    let mut external = Vec::new();
    for path in &h.paths {
        let mut cs: Vec<Coord> = Vec::new();
        for edge in &path.edges {
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
            if !doc.layers.contains(name) {
                doc.layers.add(acadrust::tables::Layer::new(name)).map_err(codec_error)?;
            }
            let linetype = text("Linetype").unwrap_or("");
            if !linetype.is_empty() && !doc.line_types.contains(linetype) {
                // The name is useful for filtering; GIS fields do not carry the
                // CAD dash pattern, so no pattern is invented here.
                doc.line_types.add(acadrust::tables::LineType::new(linetype)).map_err(codec_error)?;
            }
            let paper = layer.schema.field_index("PaperSpace").and_then(|i| f.attributes.get(i)) == Some(&FieldValue::Integer(1));
            let mut entities = Vec::new();
            encode_geometry(g, text("Text"), &mut entities)?;
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

fn encode_geometry(g: &Geometry, text: Option<&str>, out: &mut Vec<EntityType>) -> Result<()> {
    match g {
        Geometry::Point(c) => out.push(match text {
            Some(text) => EntityType::Text(Text::with_value(text, vector(c)?)),
            None => EntityType::Point(Point::from_coords(c.x, c.y, vector(c)?.z)),
        }),
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
                encode_geometry(&Geometry::Point(c.clone()), text, out)?;
            }
        }
        Geometry::MultiLineString(ls) => {
            for cs in ls {
                encode_geometry(&Geometry::LineString(cs.clone()), None, out)?;
            }
        }
        Geometry::MultiPolygon(ps) => {
            for (e, hs) in ps {
                encode_geometry(&Geometry::Polygon { exterior: e.clone(), interiors: hs.clone() }, None, out)?;
            }
        }
        Geometry::GeometryCollection(gs) => {
            for g in gs {
                encode_geometry(g, text, out)?;
            }
        }
    }
    Ok(())
}

// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! CityJSON 1.1 and 2.0, interpreted according to the published CityJSON specifications.
//!
//! The ordinary vector view is deliberately flat: one feature per surface (including solid
//! interior shells), line or point, retaining XYZ, holes, object identity, type, LoD, parents
//! and attributes. Solids are not polygons in XY: vertical faces must retain their vertices.
//! Semantics are retained as JSON text for inspection, not as a second city data model.
//! GDAL 3.13.3 has no CityJSON driver and is not the reference for this format.

use crate::crs::SpatialRef;
use crate::{Error, Result};
use serde_json::Value;
use wbvector::{Coord, FieldDef, FieldType, FieldValue, Geometry, Layer, Ring};

// Mutable attribute slots are named to keep LoD, source and face semantics distinct.
use super::city::{GEOMETRY_TYPE, LOD, SEMANTICS, SURFACE_SEMANTICS};
const SKIPPED_RINGS: usize = 8;
const SKIPPED_FACES: usize = 9;

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("CityJSON: {message}"))
}

fn array(value: &Value) -> Result<&[Value]> {
    value.as_array().map(Vec::as_slice).ok_or_else(|| invalid("expected an array"))
}

fn triple(value: &Value) -> Result<[f64; 3]> {
    let a = array(value)?;
    if a.len() != 3 {
        return Err(invalid("expected three coordinates"));
    }
    let number = |v: &Value| v.as_f64().filter(|v| v.is_finite()).ok_or_else(|| invalid("nonfinite or nonnumeric coordinate"));
    Ok([number(&a[0])?, number(&a[1])?, number(&a[2])?])
}

fn vertices(value: &Value, transform: &Value) -> Result<Vec<Coord>> {
    let (scale, translate) =
        if transform.is_null() { ([1.0; 3], [0.0; 3]) } else { (triple(&transform["scale"])?, triple(&transform["translate"])?) };
    array(value)?
        .iter()
        .map(|v| {
            let p = triple(v)?;
            let p: [f64; 3] = std::array::from_fn(|i| p[i] * scale[i] + translate[i]);
            if p.iter().any(|v| !v.is_finite()) {
                return Err(invalid("transformed coordinate is not finite"));
            }
            Ok(Coord { x: p[0], y: p[1], z: Some(p[2]), m: None })
        })
        .collect()
}

fn index(value: &Value) -> Result<usize> {
    value.as_u64().and_then(|i| usize::try_from(i).ok()).ok_or_else(|| invalid("vertex or template index must be a nonnegative integer"))
}

struct Positions<'a> {
    points: &'a [Coord],
    transform: Option<([f64; 16], [f64; 3])>,
}

fn position(value: &Value, vertices: &Positions<'_>) -> Result<Coord> {
    let c = vertices.points.get(index(value)?).ok_or_else(|| invalid("vertex index out of bounds"))?;
    let Some((matrix, offset)) = &vertices.transform else { return Ok(c.clone()) };
    let p = [c.x, c.y, c.z.unwrap_or(0.0), 1.0];
    let p: [f64; 3] = std::array::from_fn(|i| (0..4).map(|j| matrix[i * 4 + j] * p[j]).sum::<f64>() + offset[i]);
    if p.iter().any(|v| !v.is_finite()) {
        return Err(invalid("instance coordinate is not finite"));
    }
    Ok(Coord { x: p[0], y: p[1], z: Some(p[2]), m: None })
}

fn line(value: &Value, vertices: &Positions<'_>) -> Result<Vec<Coord>> {
    array(value)?.iter().map(|i| position(i, vertices)).collect()
}

#[derive(Default)]
struct Diagnostics {
    skipped_rings: usize,
    skipped_faces: usize,
}

fn ring(value: &Value, vertices: &Positions<'_>, diagnostics: &mut Diagnostics) -> Result<Option<Ring>> {
    let mut points = line(value, vertices)?;
    if points.len() > 1 && points.first() == points.last() {
        points.pop();
    }
    // Count XYZ positions rather than XY area: vertical faces are valid surfaces.
    // Stop after three distinct positions, keeping this linear even for long rings.
    let mut distinct = Vec::with_capacity(3);
    for point in &points {
        if !distinct.contains(&point) {
            distinct.push(point);
        }
        if distinct.len() == 3 {
            break;
        }
    }
    if distinct.len() < 3 {
        diagnostics.skipped_rings += 1;
        return Ok(None);
    }
    Ok(Some(Ring::new(points)))
}

fn surface(value: &Value, vertices: &Positions<'_>, diagnostics: &mut Diagnostics) -> Result<Option<Geometry>> {
    let mut rings = array(value)?.iter();
    let exterior = rings.next().map(|r| ring(r, vertices, diagnostics)).transpose()?.flatten();
    let mut interiors = Vec::new();
    for value in rings {
        if let Some(ring) = ring(value, vertices, diagnostics)? {
            interiors.push(ring);
        }
    }
    let Some(exterior) = exterior else {
        diagnostics.skipped_faces += 1;
        return Ok(None);
    };
    Ok(Some(Geometry::Polygon { exterior, interiors }))
}

// Keep a placeholder for every source face so semantic values retain their indices.
fn surfaces(
    value: &Value,
    depth: usize,
    vertices: &Positions<'_>,
    diagnostics: &mut Diagnostics,
    out: &mut Vec<Option<Geometry>>,
) -> Result<()> {
    if depth == 0 {
        out.push(surface(value, vertices, diagnostics)?);
    } else {
        for child in array(value)? {
            surfaces(child, depth - 1, vertices, diagnostics, out)?;
        }
    }
    Ok(())
}

fn primitives(geometry: &Value, vertices: &Positions<'_>, diagnostics: &mut Diagnostics) -> Result<Vec<Option<Geometry>>> {
    let boundaries = &geometry["boundaries"];
    let mut out = Vec::new();
    match geometry["type"].as_str() {
        Some("MultiSurface" | "CompositeSurface") => surfaces(boundaries, 1, vertices, diagnostics, &mut out)?,
        Some("Solid") => surfaces(boundaries, 2, vertices, diagnostics, &mut out)?,
        Some("MultiSolid" | "CompositeSolid") => surfaces(boundaries, 3, vertices, diagnostics, &mut out)?,
        Some("MultiPoint") => {
            for i in array(boundaries)? {
                out.push(Some(Geometry::Point(position(i, vertices)?)));
            }
        }
        Some("MultiLineString") => {
            for l in array(boundaries)? {
                let points = line(l, vertices)?;
                if points.len() < 2 {
                    return Err(invalid("line has fewer than two vertices"));
                }
                out.push(Some(Geometry::LineString(points)));
            }
        }
        other => return Err(Error::Unsupported(format!("CityJSON geometry type {other:?}"))),
    }
    Ok(out)
}

fn semantic_values<'a>(
    boundaries: &Value,
    values: &Value,
    surfaces: &'a [Value],
    depth: usize,
    out: &mut Vec<Option<&'a Value>>,
) -> Result<()> {
    if depth == 0 {
        out.push(if values.is_null() {
            None
        } else {
            let surface = surfaces.get(index(values)?).ok_or_else(|| invalid("semantic surface index out of bounds"))?;
            if !surface.is_object() {
                return Err(invalid("semantic surface must be an object"));
            }
            Some(surface)
        });
    } else {
        let boundaries = array(boundaries)?;
        let values = array(values)?;
        if boundaries.len() != values.len() {
            return Err(invalid("semantic values do not match boundary nesting"));
        }
        for (boundary, value) in boundaries.iter().zip(values) {
            semantic_values(boundary, value, surfaces, depth - 1, out)?;
        }
    }
    Ok(())
}

fn primitive_semantics(geometry: &Value, count: usize) -> Result<Vec<Option<&Value>>> {
    if geometry["semantics"].is_null() {
        return Ok(vec![None; count]);
    }
    let depth = match geometry["type"].as_str() {
        Some("MultiSurface" | "CompositeSurface") => 1,
        Some("Solid") => 2,
        Some("MultiSolid" | "CompositeSolid") => 3,
        _ => return Err(invalid("semantics require a surface or solid geometry")),
    };
    let semantics = &geometry["semantics"];
    let mut out = Vec::new();
    semantic_values(&geometry["boundaries"], &semantics["values"], array(&semantics["surfaces"])?, depth, &mut out)?;
    Ok(out)
}

fn instance<'a>(geometry: &Value, doc: &'a Value, world: &Positions<'_>, points: &'a [Coord]) -> Result<(&'a Value, Positions<'a>)> {
    let templates = &doc["geometry-templates"];
    let template =
        array(&templates["templates"])?.get(index(&geometry["template"])?).ok_or_else(|| invalid("template index out of bounds"))?;
    let values = array(&geometry["transformationMatrix"])?;
    if values.len() != 16 {
        return Err(invalid("instance matrix must have 16 entries"));
    }
    let mut matrix = [0.0; 16];
    for (target, value) in matrix.iter_mut().zip(values) {
        *target = value.as_f64().filter(|v| v.is_finite()).ok_or_else(|| invalid("nonfinite or nonnumeric instance matrix"))?;
    }
    if matrix[12..] != [0.0, 0.0, 0.0, 1.0] {
        return Err(invalid("instance matrix must be affine"));
    }
    let anchors = array(&geometry["boundaries"])?;
    if anchors.len() != 1 {
        return Err(invalid("instance must have exactly one anchor vertex"));
    }
    let anchor = position(&anchors[0], world)?;
    let offset = [anchor.x, anchor.y, anchor.z.unwrap_or(0.0)];
    // Templates and their vertex pool are borrowed. Only referenced coordinates are
    // transformed, so unrelated templates do not multiply the work per instance.
    Ok((template, Positions { points, transform: Some((matrix, offset)) }))
}

fn text(value: &Value) -> FieldValue {
    if value.is_null() { FieldValue::Null } else { FieldValue::Text(value.as_str().map_or_else(|| value.to_string(), str::to_owned)) }
}

/// Read a CityJSON document into its ordinary, flat vector view.
pub fn parse(bytes: &[u8]) -> Result<Vec<Layer>> {
    parse_with_limit(bytes, super::city::MAX_FEATURE_BYTES)
}

fn parse_with_limit(bytes: &[u8], limit: usize) -> Result<Vec<Layer>> {
    let mut used = 0usize;
    super::geojson::reserve_parse_memory(bytes.len(), "CityJSON")?;
    let doc: Value = serde_json::from_slice(bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes)).map_err(invalid)?;
    if doc["type"] != "CityJSON" {
        return Err(invalid("expected a CityJSON document"));
    }
    if !matches!(doc["version"].as_str(), Some("1.1" | "2.0")) {
        return Err(Error::Unsupported("CityJSON: supported versions are 1.1 and 2.0".into()));
    }
    let transform = doc.get("transform").filter(|v| v.is_object()).ok_or_else(|| invalid("top-level transform object is required"))?;
    let world = vertices(&doc["vertices"], transform)?;
    let points = Positions { points: &world, transform: None };
    let template_points = doc
        .get("geometry-templates")
        .map(|templates| vertices(&templates["vertices-templates"], &Value::Null))
        .transpose()?
        .unwrap_or_default();
    let objects = doc["CityObjects"].as_object().ok_or_else(|| invalid("missing CityObjects object"))?;
    let mut layer = super::city::layer("CityJSON");
    for name in ["skipped_rings", "skipped_faces"] {
        layer.add_field(FieldDef::new(name, FieldType::Integer));
    }
    if let Some(value) = doc.get("metadata").and_then(|m| m.get("referenceSystem")) {
        let crs = value.as_str().ok_or_else(|| invalid("referenceSystem must be a CRS string"))?;
        SpatialRef::parse(crs).map_err(|e| invalid(format!("referenceSystem: {e}")))?.apply_to_layer(&mut layer);
    }
    for (id, object) in objects {
        let kind = object["type"].as_str().ok_or_else(|| invalid(format!("object {id}: missing type")))?;
        let mut attrs = vec![
            FieldValue::Text(id.clone()),
            FieldValue::Text(kind.into()),
            FieldValue::Null,
            text(&object["parents"]),
            text(&object["attributes"]),
            FieldValue::Null,
            FieldValue::Null,
            FieldValue::Null,
            FieldValue::Integer(0),
            FieldValue::Integer(0),
        ];
        let geometries = match object.get("geometry") {
            Some(value) => array(value)?,
            None => &[],
        };
        if geometries.is_empty() {
            super::city::push_with_limit(&mut layer, None, &attrs, &mut used, limit)?;
            continue;
        }
        for geometry in geometries {
            let mut diagnostics = Diagnostics::default();
            attrs[SURFACE_SEMANTICS] = FieldValue::Null;
            attrs[LOD] = text(&geometry["lod"]);
            attrs[GEOMETRY_TYPE] = text(&geometry["type"]);
            attrs[SEMANTICS] = text(&geometry["semantics"]);
            let (primitives, surface_semantics) = if geometry["type"] == "GeometryInstance" {
                let (template, vertices) = instance(geometry, &doc, &points, &template_points)?;
                if attrs[LOD] == FieldValue::Null {
                    attrs[LOD] = text(&template["lod"]);
                }
                attrs[SEMANTICS] = text(&template["semantics"]);
                let primitives = primitives(template, &vertices, &mut diagnostics)?;
                let semantics = primitive_semantics(template, primitives.len())?;
                (primitives, semantics)
            } else {
                let primitives = primitives(geometry, &points, &mut diagnostics)?;
                let semantics = primitive_semantics(geometry, primitives.len())?;
                (primitives, semantics)
            };
            attrs[SKIPPED_RINGS] = FieldValue::Integer(i64::try_from(diagnostics.skipped_rings).map_err(invalid)?);
            attrs[SKIPPED_FACES] = FieldValue::Integer(i64::try_from(diagnostics.skipped_faces).map_err(invalid)?);
            let mut emitted = false;
            for (primitive, semantic) in primitives.into_iter().zip(surface_semantics) {
                if let Some(primitive) = primitive {
                    attrs[SURFACE_SEMANTICS] = semantic.map_or(FieldValue::Null, text);
                    super::city::push_with_limit(&mut layer, Some(primitive), &attrs, &mut used, limit)?;
                    emitted = true;
                }
            }
            if !emitted {
                super::city::push_with_limit(&mut layer, None, &attrs, &mut used, limit)?;
            }
        }
    }
    Ok(vec![layer])
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_large_single_geometry_reports_the_documented_semantics_expansion_limit() {
        let faces = 100;
        let doc = serde_json::json!({"type":"CityJSON","version":"2.0",
            "transform":{"scale":[1,1,1],"translate":[0,0,0]},"vertices":[[0,0,0],[0,1,0],[0,1,1]],
            "CityObjects":{"house":{"type":"Building","geometry":[{"type":"CompositeSurface","lod":"2",
                "boundaries":vec![serde_json::json!([[0,1,2]]);faces],
                "semantics":{"surfaces":vec![serde_json::json!({"type":"WallSurface"});faces],"values":(0..faces).collect::<Vec<_>>()}}]}}});
        let bytes = serde_json::to_vec(&doc).unwrap();
        assert_eq!(super::parse_with_limit(&bytes, 1024 * 1024).unwrap()[0].features.len(), faces);
        assert!(super::parse_with_limit(&bytes, 64 * 1024).unwrap_err().to_string().contains("expansion limit"));
    }

    #[test]
    fn repeated_attributes_stop_at_the_flat_payload_budget() {
        let mut doc = serde_json::json!({"type":"CityJSON","version":"2.0","vertices":[[1,2,3]],"transform":{"scale":[1,1,1],"translate":[0,0,0]},
            "CityObjects":{"tree":{"type":"SolitaryVegetationObject","attributes":{"text":"x".repeat(400)},
                "geometry":[{"type":"MultiPoint","lod":"1","boundaries":[0,0,0,0]}]}}});
        let bytes = serde_json::to_vec(&doc).unwrap();
        assert_eq!(super::parse_with_limit(&bytes, 16384).unwrap()[0].features.len(), 4);
        assert!(super::parse_with_limit(&bytes, 1024).unwrap_err().to_string().contains("expansion limit"));
        doc["CityObjects"]["tree"]["geometry"][0]["boundaries"] = serde_json::json!([0]);
        assert_eq!(super::parse_with_limit(&serde_json::to_vec(&doc).unwrap(), 1024).unwrap()[0].features.len(), 1);
    }
}

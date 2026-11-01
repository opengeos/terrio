// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Borrowed flat primitives and metadata conventions shared by city-model encoders.
use crate::{Error, Result};
use serde_json::{Value, json};
use wbvector::{Coord, Feature, FieldType, FieldValue, Geometry, Layer, Ring};

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::InvalidArgument(format!("City-model writer: {message}"))
}

#[derive(Clone, Copy)]
pub(super) enum Primitive<'a> {
    Point(&'a Coord),
    Line(&'a [Coord]),
    Surface(&'a Ring, &'a [Ring]),
}
impl Primitive<'_> {
    pub(super) fn positions(&self, mut f: impl FnMut(&Coord) -> Result<()>) -> Result<()> {
        match self {
            Self::Point(p) => f(p)?,
            Self::Line(line) => {
                for p in *line {
                    f(p)?;
                }
            }
            Self::Surface(exterior, holes) => {
                for ring in std::iter::once(*exterior).chain(holes.iter()) {
                    for p in &ring.0 {
                        f(p)?;
                    }
                }
            }
        }
        Ok(())
    }
    pub(super) fn kind(self) -> &'static str {
        match self {
            Self::Point(_) => "MultiPoint",
            Self::Line(_) => "MultiLineString",
            Self::Surface(_, _) => "MultiSurface",
        }
    }
}
pub(super) fn flatten<'a>(geometry: &'a Geometry, depth: usize, out: &mut Vec<Primitive<'a>>) -> Result<()> {
    if depth > 256 {
        return Err(invalid("geometry nesting exceeds 256"));
    }
    match geometry {
        Geometry::Point(p) => out.push(Primitive::Point(p)),
        Geometry::MultiPoint(ps) => out.extend(ps.iter().map(Primitive::Point)),
        Geometry::LineString(ps) => out.push(Primitive::Line(ps)),
        Geometry::MultiLineString(lines) => out.extend(lines.iter().map(|p| Primitive::Line(p))),
        Geometry::Polygon { exterior, interiors } => out.push(Primitive::Surface(exterior, interiors)),
        Geometry::MultiPolygon(polygons) => out.extend(polygons.iter().map(|(e, h)| Primitive::Surface(e, h))),
        Geometry::GeometryCollection(geometries) => {
            for g in geometries {
                flatten(g, depth + 1, out)?;
            }
        }
    }
    Ok(())
}
pub(super) fn xyz(p: &Coord) -> Result<[f64; 3]> {
    let values = [p.x, p.y, p.z.unwrap_or(0.0)];
    if values.iter().any(|v| !v.is_finite()) {
        return Err(invalid("coordinates must be finite"));
    }
    if p.m.is_some() {
        return Err(invalid("City-model output has XYZ coordinates; remove measure coordinates before writing"));
    }
    Ok(values)
}
pub(super) fn field<'a>(layer: &Layer, feature: &'a Feature, name: &str) -> &'a FieldValue {
    layer.schema.field_index(name).and_then(|i| feature.attributes.get(i)).unwrap_or(&FieldValue::Null)
}
pub(super) fn json_field(value: &FieldValue) -> Result<Value> {
    match value {
        FieldValue::Null => Ok(Value::Null),
        FieldValue::Text(s) => serde_json::from_str(s).map_err(invalid),
        _ => Err(invalid("flat JSON metadata fields must contain JSON text")),
    }
}
pub(super) fn property(value: &FieldValue, kind: FieldType) -> Result<Value> {
    Ok(match value {
        FieldValue::Null => Value::Null,
        FieldValue::Integer(v) => json!(v),
        FieldValue::Float(v) if v.is_finite() => json!(v),
        FieldValue::Float(_) => return Err(invalid("attribute numbers must be finite")),
        FieldValue::Boolean(v) => json!(v),
        FieldValue::Text(v) if kind == FieldType::Json => serde_json::from_str(v).map_err(invalid)?,
        FieldValue::Text(v) | FieldValue::Date(v) | FieldValue::DateTime(v) => json!(v),
        FieldValue::Blob(v) => json!(v.iter().map(|x| format!("{x:02X}")).collect::<String>()),
    })
}
pub(super) const COMMON_FIELDS: &[&str] =
    &["city_id", "city_type", "lod", "parents", "attributes", "geometry_type", "semantics", "surface_semantics"];
pub(super) fn has_city_fields(layer: &Layer) -> bool {
    layer.schema.field_index("city_id").is_some() && layer.schema.field_index("city_type").is_some()
}

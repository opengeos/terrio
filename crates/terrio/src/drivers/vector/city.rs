// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Shared flat fields and expansion accounting for city-model readers.
use crate::Result;
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer, Ring};

pub(super) fn layer(name: &str) -> Layer {
    let mut layer = Layer::new(name);
    for name in ["city_id", "city_type", "lod", "parents", "attributes", "geometry_type", "semantics", "surface_semantics"] {
        layer.add_field(FieldDef::new(name, FieldType::Text));
    }
    layer
}

pub(super) fn push(layer: &mut Layer, geometry: Option<Geometry>, attrs: &[FieldValue], used: &mut usize) -> Result<()> {
    push_with_limit(layer, geometry, attrs, used, 256 * 1024 * 1024)
}

// Account for owned field text, coordinates and feature containers before cloning
// a primitive. wbvector owns String values; it cannot share repeated attributes.
pub(super) fn push_with_limit(
    layer: &mut Layer,
    geometry: Option<Geometry>,
    attrs: &[FieldValue],
    used: &mut usize,
    limit: usize,
) -> Result<()> {
    let coordinates = match &geometry {
        Some(Geometry::Point(_)) => 1,
        Some(Geometry::LineString(points)) => points.len(),
        Some(Geometry::Polygon { exterior, interiors }) => exterior.len() + interiors.iter().map(Ring::len).sum::<usize>(),
        _ => 0,
    };
    let text = attrs
        .iter()
        .map(|a| match a {
            FieldValue::Text(s) => s.len(),
            _ => 0,
        })
        .sum::<usize>();
    let cost = text
        .saturating_add(coordinates.saturating_mul(std::mem::size_of::<Coord>()))
        .saturating_add(std::mem::size_of_val(attrs))
        .saturating_add(std::mem::size_of::<Feature>());
    *used = used.saturating_add(cost);
    if *used > limit {
        return Err(crate::Error::InvalidData(
            "city model flat feature payload exceeds the 256 MiB expansion limit; split the model into smaller files".into(),
        ));
    }
    layer.push(Feature { fid: layer.features.len() as u64, geometry, attributes: attrs.to_vec() });
    Ok(())
}

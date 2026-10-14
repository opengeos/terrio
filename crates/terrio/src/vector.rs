//! Vector layer helpers.

use wbvector::{Geometry, GeometryType, Layer};

/// The layer's geometry type: the declared one, or the one its features share. Mixes of a type and
/// its multi-part form count as the multi-part form; other mixes are `Unknown`. A ` Z` suffix
/// marks layers with heights.
pub fn geometry_type_name(layer: &Layer) -> String {
    let base = match layer.geom_type {
        Some(t) => Some(t),
        None => {
            let mut found: Option<GeometryType> = None;
            let mut mixed = false;
            for g in layer.features.iter().filter_map(|f| f.geometry.as_ref()) {
                let t = g.geom_type();
                found = match found {
                    None => Some(t),
                    Some(prev) if prev == t => Some(prev),
                    Some(prev) => match promote(prev, t) {
                        Some(m) => Some(m),
                        None => {
                            mixed = true;
                            break;
                        }
                    },
                };
            }
            if mixed { None } else { found }
        }
    };
    let Some(t) = base else {
        return if layer.features.iter().all(|f| f.geometry.is_none()) { "None".into() } else { "Unknown".into() };
    };
    let has_z = layer.features.iter().filter_map(|f| f.geometry.as_ref()).take(100).any(geometry_has_z);
    format!("{}{}", t.as_str(), if has_z { " Z" } else { "" })
}

/// The multi-part type covering two types, when one is the multi-part form of the other.
fn promote(a: GeometryType, b: GeometryType) -> Option<GeometryType> {
    use GeometryType as T;
    match (a, b) {
        (T::Point, T::MultiPoint) | (T::MultiPoint, T::Point) => Some(T::MultiPoint),
        (T::LineString, T::MultiLineString) | (T::MultiLineString, T::LineString) => Some(T::MultiLineString),
        (T::Polygon, T::MultiPolygon) | (T::MultiPolygon, T::Polygon) => Some(T::MultiPolygon),
        _ => None,
    }
}

/// Whether a geometry has heights.
pub fn geometry_has_z(g: &Geometry) -> bool {
    match g {
        Geometry::Point(c) => c.z.is_some(),
        Geometry::LineString(cs) | Geometry::MultiPoint(cs) => cs.first().is_some_and(|c| c.z.is_some()),
        Geometry::Polygon { exterior, .. } => exterior.0.first().is_some_and(|c| c.z.is_some()),
        Geometry::MultiLineString(ls) => ls.first().and_then(|l| l.first()).is_some_and(|c| c.z.is_some()),
        Geometry::MultiPolygon(ps) => ps.first().and_then(|p| p.0.0.first()).is_some_and(|c| c.z.is_some()),
        Geometry::GeometryCollection(gs) => gs.iter().any(geometry_has_z),
    }
}

/// The bounds `(min_x, min_y, max_x, max_y)` of a layer's geometries.
pub fn layer_extent(layer: &Layer) -> Option<(f64, f64, f64, f64)> {
    let mut b: Option<(f64, f64, f64, f64)> = None;
    for g in layer.features.iter().filter_map(|f| f.geometry.as_ref()) {
        if let Some(gb) = g.bbox() {
            b = Some(match b {
                None => (gb.min_x, gb.min_y, gb.max_x, gb.max_y),
                Some(a) => (a.0.min(gb.min_x), a.1.min(gb.min_y), a.2.max(gb.max_x), a.3.max(gb.max_y)),
            });
        }
    }
    b
}

/// Reprojects a layer into `dst`. The layer's CRS must be known.
pub fn reproject_layer(layer: &Layer, dst: &crate::crs::SpatialRef) -> crate::Result<Layer> {
    let src = crate::crs::SpatialRef::from_layer(layer);
    if src.is_unknown() {
        return Err(crate::Error::Crs(format!("layer '{}' has no CRS to reproject from", layer.name)));
    }
    if src.same_as(dst) {
        return Ok(layer.clone());
    }
    let mut out = wbvector::reproject::layer_with_crs(layer, &src.to_wbp()?, &dst.to_wbp()?, dst.epsg)?;
    dst.apply_to_layer(&mut out);
    Ok(out)
}

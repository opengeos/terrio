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

/// Reprojects a layer into `dst`, with terrio's transformations (units, axis order and datum
/// transformations as GDAL applies them). Heights and measures are kept. The layer's CRS must
/// be known.
pub fn reproject_layer(layer: &Layer, dst: &crate::crs::SpatialRef) -> crate::Result<Layer> {
    let src = crate::crs::SpatialRef::from_layer(layer);
    if src.is_unknown() {
        return Err(crate::Error::Crs(format!("layer '{}' has no CRS to reproject from", layer.name)));
    }
    if src.same_as(dst) {
        return Ok(layer.clone());
    }
    let t = crate::crs::Transformer::new(&src, dst)?;
    let mut out = layer.clone();
    for (i, f) in out.features.iter_mut().enumerate() {
        if let Some(g) = f.geometry.as_mut() {
            map_coords(g, &mut |c| {
                let (x, y) = t.transform(c.x, c.y)?;
                c.x = x;
                c.y = y;
                Ok(())
            })
            .map_err(|e| crate::Error::Crs(format!("failed to reproject feature {i} of layer '{}': {e}", layer.name)))?;
        }
    }
    out.extent = None;
    dst.apply_to_layer(&mut out);
    Ok(out)
}

/// Applies `f` to every coordinate of a geometry.
pub fn map_coords(g: &mut Geometry, f: &mut dyn FnMut(&mut wbvector::Coord) -> crate::Result<()>) -> crate::Result<()> {
    let mut all = |cs: &mut Vec<wbvector::Coord>| cs.iter_mut().try_for_each(&mut *f);
    match g {
        Geometry::Point(c) => f(c),
        Geometry::LineString(cs) | Geometry::MultiPoint(cs) => all(cs),
        Geometry::Polygon { exterior, interiors } => {
            all(&mut exterior.0)?;
            interiors.iter_mut().try_for_each(|r| all(&mut r.0))
        }
        Geometry::MultiLineString(ls) => ls.iter_mut().try_for_each(all),
        Geometry::MultiPolygon(ps) => ps.iter_mut().try_for_each(|(e, holes)| {
            all(&mut e.0)?;
            holes.iter_mut().try_for_each(|r| all(&mut r.0))
        }),
        Geometry::GeometryCollection(gs) => gs.iter_mut().try_for_each(|g| map_coords(g, f)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reprojects_nad83_like_ogr2ogr() {
        // ogr2ogr -s_srs EPSG:4269 -t_srs EPSG:32613 of POINT (-106.001666667 40.001666667) gives
        // POINT (414499.347197613 4428422.65095327): NAD83 to WGS 84 is a null transformation.
        let mut layer = Layer::new("p");
        layer.set_crs_epsg(Some(4269));
        layer.add_feature(Some(Geometry::point(-106.001666667, 40.001666667)), &[]).unwrap();
        let out = reproject_layer(&layer, &crate::crs::SpatialRef::from_epsg(32613).unwrap()).unwrap();
        let Some(Geometry::Point(c)) = &out.features[0].geometry else { panic!("not a point") };
        assert!((c.x - 414499.347197613).abs() < 1e-3 && (c.y - 4428422.65095327).abs() < 1e-3, "{} {}", c.x, c.y);
    }

    #[test]
    fn reprojects_with_units_and_keeps_heights() {
        // ogr2ogr -s_srs EPSG:2263 -t_srs EPSG:4326: US survey feet in, degrees out.
        let mut layer = Layer::new("p");
        layer.set_crs_epsg(Some(2263));
        let c = wbvector::Coord { x: 988000.0, y: 200000.0, z: Some(12.5), m: None };
        layer.add_feature(Some(Geometry::LineString(vec![c.clone(), c])), &[]).unwrap();
        let out = reproject_layer(&layer, &crate::crs::SpatialRef::from_epsg(4326).unwrap()).unwrap();
        let Some(Geometry::LineString(cs)) = &out.features[0].geometry else { panic!("not a line") };
        assert!((cs[0].x - -73.98647263351127).abs() < 1e-9 && (cs[0].y - 40.71562970330491).abs() < 1e-9, "{:?}", cs[0]);
        assert_eq!(cs[1].z, Some(12.5));
        assert_eq!(out.crs_epsg(), Some(4326));
    }
}

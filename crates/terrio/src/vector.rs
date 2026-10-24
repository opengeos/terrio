//! Vector layer helpers, and the parts of terrio's vector data model (from `wbvector`) that the
//! crate root does not re-export: coordinates, rings, schemas, bounding boxes and layer CRSs.

pub use wbvector::{BBox, Coord, Crs, GeoError, Ring, Schema};

use wbvector::{Geometry, GeometryType, Layer};

/// The layer's geometry type: the declared one, or the one its features share. Mixes of a type and
/// its multi-part form count as the multi-part form; other mixes are `Unknown`. A ` Z` suffix
/// marks layers with heights.
pub fn geometry_type_name(layer: &Layer) -> String {
    let geometries = || layer.features.iter().filter_map(|f| f.geometry.as_ref());
    geometry_type_name_of(layer.geom_type, geometries().map(|g| g.geom_type()), geometries().take(100).any(geometry_has_z))
}

/// The geometry type name of a layer declaring the type `declared`, whose (non-null) geometries
/// have the types `types`, and whose first 100 geometries have Z coordinates (`has_z`).
pub(crate) fn geometry_type_name_of(declared: Option<GeometryType>, types: impl Iterator<Item = GeometryType>, has_z: bool) -> String {
    let mut types = types.peekable();
    let any = types.peek().is_some();
    let base = match declared {
        Some(t) => Some(t),
        None => {
            let mut found: Option<GeometryType> = None;
            let mut mixed = false;
            for t in types {
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
        return if any { "Unknown".into() } else { "None".into() };
    };
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

/// Whether some coordinate of a geometry has a Z value, and whether some has an M value.
pub(crate) fn geometry_dims(g: &Geometry) -> (bool, bool) {
    let (mut z, mut m) = (false, false);
    visit_coords(g, &mut |c| {
        z |= c.z.is_some();
        m |= c.m.is_some();
    });
    (z, m)
}

fn visit_coords(g: &Geometry, f: &mut dyn FnMut(&Coord)) {
    match g {
        Geometry::Point(c) => f(c),
        Geometry::LineString(cs) | Geometry::MultiPoint(cs) => cs.iter().for_each(f),
        Geometry::Polygon { exterior, interiors } => exterior.0.iter().chain(interiors.iter().flat_map(|r| r.0.iter())).for_each(f),
        Geometry::MultiLineString(ls) => ls.iter().flatten().for_each(f),
        Geometry::MultiPolygon(ps) => ps.iter().flat_map(|(e, h)| e.0.iter().chain(h.iter().flat_map(|r| r.0.iter()))).for_each(f),
        Geometry::GeometryCollection(gs) => gs.iter().for_each(|g| visit_coords(g, f)),
    }
}

/// The number of coordinates of a geometry.
pub(crate) fn coord_count(g: &Geometry) -> usize {
    match g {
        Geometry::Point(_) => 1,
        Geometry::LineString(cs) | Geometry::MultiPoint(cs) => cs.len(),
        Geometry::Polygon { exterior, interiors } => exterior.0.len() + interiors.iter().map(|r| r.0.len()).sum::<usize>(),
        Geometry::MultiLineString(ls) => ls.iter().map(Vec::len).sum(),
        Geometry::MultiPolygon(ps) => ps.iter().map(|(e, h)| e.0.len() + h.iter().map(|r| r.0.len()).sum::<usize>()).sum(),
        Geometry::GeometryCollection(gs) => gs.iter().map(coord_count).sum(),
    }
}

/// The bounds `(min_x, min_y, max_x, max_y)` of a layer's geometries.
pub fn layer_extent(layer: &Layer) -> Option<(f64, f64, f64, f64)> {
    extent_of(layer.features.iter().filter_map(|f| f.geometry.as_ref()?.bbox()).map(|b| (b.min_x, b.min_y, b.max_x, b.max_y)))
}

/// The bounds covering bounding boxes `(min_x, min_y, max_x, max_y)`, combined in order.
pub(crate) fn extent_of(boxes: impl Iterator<Item = (f64, f64, f64, f64)>) -> Option<(f64, f64, f64, f64)> {
    let mut b: Option<(f64, f64, f64, f64)> = None;
    for gb in boxes {
        b = Some(match b {
            None => gb,
            Some(a) => (a.0.min(gb.0), a.1.min(gb.1), a.2.max(gb.2), a.3.max(gb.3)),
        });
    }
    b
}

/// Reprojects a layer into `dst`, with terrio's transformations (units, axis order and datum
/// transformations as GDAL applies them). Heights and measures are kept. The layer's CRS must
/// be known.
pub fn reproject_layer(layer: &Layer, dst: &crate::crs::SpatialRef) -> crate::Result<Layer> {
    reproject_layer_owned(layer.clone(), dst)
}

/// [`reproject_layer`], consuming the layer. Features are reprojected in parallel.
pub(crate) fn reproject_layer_owned(layer: Layer, dst: &crate::crs::SpatialRef) -> crate::Result<Layer> {
    reproject_layer_with_options(layer, dst, &Default::default())
}

/// Reproject a layer with explicit coordinate operation selection controls.
pub fn reproject_layer_with_options(
    mut layer: Layer,
    dst: &crate::crs::SpatialRef,
    options: &crate::crs::TransformOptions,
) -> crate::Result<Layer> {
    let src = crate::crs::SpatialRef::from_layer(&layer);
    if src.is_unknown() {
        return Err(crate::Error::Crs(format!("layer '{}' has no CRS to reproject from", layer.name)));
    }
    if src.same_as(dst) {
        return Ok(layer);
    }
    let t = crate::crs::Transformer::with_options(&src, dst, options)?;
    let len = crate::par::chunk_len(layer.features.len(), 16);
    let errors = std::sync::Mutex::new(Vec::new());
    crate::par::chunks_mut(&mut layer.features, len, |k, features| {
        for (j, f) in features.iter_mut().enumerate() {
            if let Some(g) = f.geometry.as_mut()
                && let Err(e) = map_coords(g, &mut |c| {
                    let (x, y) = t.transform(c.x, c.y)?;
                    c.x = x;
                    c.y = y;
                    Ok(())
                })
            {
                if let Ok(mut errors) = errors.lock() {
                    errors.push((k * len + j, e));
                }
                // Features after a failure are not reprojected, as in a sequential pass.
                break;
            }
        }
    });
    t.flush_warnings();
    // The first feature that fails, as a sequential pass would report it.
    if let Some((i, e)) = errors.into_inner().unwrap_or_default().into_iter().min_by_key(|(i, _)| *i) {
        return Err(crate::Error::Crs(format!("failed to reproject feature {i} of layer '{}': {e}", layer.name)));
    }
    layer.extent = None;
    dst.apply_to_layer(&mut layer);
    Ok(layer)
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

#[cfg(feature = "flatgeobuf")]
#[cfg_attr(docsrs, doc(cfg(feature = "flatgeobuf")))]
pub use crate::drivers::vector::flatgeobuf::FlatGeobufInfo;
#[cfg(feature = "parquet")]
#[cfg_attr(docsrs, doc(cfg(feature = "parquet")))]
pub use crate::drivers::vector::geoparquet::RowGroupInfo;

#[cfg(feature = "flatgeobuf")]
#[cfg_attr(docsrs, doc(cfg(feature = "flatgeobuf")))]
/// Reads a remote FlatGeobuf file whose bytes the caller fetches (see [`crate::sparse`]): its
/// header, and the features whose bounding box intersects `bbox` (in the layer's CRS), found with
/// the file's spatial index so that only the index nodes on the way and those features are read;
/// every feature without `bbox` or without an index. `name` is the file name, whose stem names
/// the layer when the header does not. Callers filter the features by their geometries, as
/// `ogr2ogr -spat` does (see [`crate::ops::vector_filter`]).
///
/// Reads fail until the bytes they need are fetched, and are then run again.
pub fn read_flatgeobuf_sparse(
    file: &crate::sparse::SparseFile,
    name: &str,
    bbox: Option<(f64, f64, f64, f64)>,
) -> crate::Result<(FlatGeobufInfo, Layer)> {
    let mut r = file.clone();
    let prefetch = |ranges: &[(u64, u64)]| -> crate::Result<()> { Ok(file.prefetch(ranges)?) };
    let (info, layer) = crate::drivers::vector::flatgeobuf::read_filtered(&mut r, file.len(), bbox, &prefetch)?;
    Ok((info, crate::dataset::fix_layer(layer, name, crate::formats::VectorDriver::FlatGeobuf)))
}

#[cfg(feature = "flatgeobuf")]
#[cfg_attr(docsrs, doc(cfg(feature = "flatgeobuf")))]
/// The header of a remote FlatGeobuf file whose bytes the caller fetches (see
/// [`crate::sparse`]), and its layer without features.
pub fn read_flatgeobuf_header_sparse(file: &crate::sparse::SparseFile, name: &str) -> crate::Result<(FlatGeobufInfo, Layer)> {
    let mut r = file.clone();
    let (info, layer) = crate::drivers::vector::flatgeobuf::read_header(&mut r, file.len())?;
    Ok((info, crate::dataset::fix_layer(layer, name, crate::formats::VectorDriver::FlatGeobuf)))
}

#[cfg(feature = "parquet")]
#[cfg_attr(docsrs, doc(cfg(feature = "parquet")))]
/// The row groups of a remote GeoParquet file whose bytes the caller fetches (see
/// [`crate::sparse`]): their row counts and bounding boxes (from the statistics of the
/// bounding-box covering column).
pub fn geoparquet_row_groups_sparse(file: &crate::sparse::SparseFile) -> crate::Result<Vec<RowGroupInfo>> {
    crate::drivers::vector::geoparquet::row_groups(file.clone())
}

#[cfg(feature = "parquet")]
#[cfg_attr(docsrs, doc(cfg(feature = "parquet")))]
/// Reads a remote GeoParquet file whose bytes the caller fetches (see [`crate::sparse`]): the
/// rows of `row_groups` (all by default) whose bounding box intersects `bbox`, reading only the
/// column chunks of those row groups. Feature ids are the row numbers in the file, as in GDAL.
/// `name` is the file name, whose stem names the layer.
///
/// Reads fail until the bytes they need are fetched, and are then run again.
pub fn read_geoparquet_sparse(
    file: &crate::sparse::SparseFile,
    name: &str,
    row_groups: Option<&[usize]>,
    bbox: Option<(f64, f64, f64, f64)>,
) -> crate::Result<Layer> {
    let stem = std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    let layer = crate::drivers::vector::geoparquet::read_sparse(file, stem, row_groups, bbox)?;
    Ok(crate::dataset::fix_layer(layer, name, crate::formats::VectorDriver::Parquet))
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

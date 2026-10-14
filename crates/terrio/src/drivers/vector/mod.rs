//! The vector drivers terrio supplies itself, where the format libraries are wrong or missing, and
//! fixes applied to what the format libraries read.

pub mod csv;
pub mod flatgeobuf;
pub mod geojson;
pub mod geopackage;
pub mod geoparquet;
pub mod gml;
pub mod gpx;
pub mod kml;
pub mod mapinfo;
pub mod osm;
pub mod shapefile;
pub mod sqlite;
pub mod topojson;
pub mod wkt;
pub mod xml;

use crate::Result;
use crate::dataset::{VectorOpenOptions, VectorWriteOptions};
use crate::formats::VectorDriver;
use wbvector::{GeometryType, Layer};

/// Reads a dataset with terrio's own driver for the format. `None` when the format library reads it.
pub(crate) fn read(path: &str, driver: VectorDriver, options: &VectorOpenOptions) -> Result<Option<Vec<Layer>>> {
    Ok(Some(match driver {
        VectorDriver::FlatGeobuf => vec![flatgeobuf::read(path)?],
        VectorDriver::GeoJSON => vec![geojson::read(path)?],
        VectorDriver::CSV => vec![csv::read(path, &options.csv)?],
        VectorDriver::Parquet => vec![geoparquet::read(path)?],
        VectorDriver::Shapefile => vec![shapefile::read(path)?],
        VectorDriver::GML => gml::read(path)?,
        VectorDriver::GPX => gpx::read(path)?,
        VectorDriver::KML => kml::read(path)?,
        VectorDriver::KMZ => kml::read_kmz(path)?,
        VectorDriver::TopoJSON => topojson::read(path)?,
        VectorDriver::MapInfo => vec![mapinfo::read(path)?],
        VectorDriver::OSM => osm::read(path)?,
        _ => return Ok(None),
    }))
}

/// Writes layers with terrio's own driver for the format. `false` when the format library writes it.
pub(crate) fn write(layer: &Layer, path: &str, driver: VectorDriver, options: &VectorWriteOptions) -> Result<bool> {
    match driver {
        VectorDriver::FlatGeobuf => flatgeobuf::write(layer, path)?,
        VectorDriver::GeoJSON => geojson::write(layer, path, &options.geojson)?,
        VectorDriver::CSV => csv::write(layer, path, &options.csv)?,
        VectorDriver::Parquet => geoparquet::write(layer, path)?,
        VectorDriver::Shapefile => shapefile::write(layer, path)?,
        VectorDriver::GML => gml::write(layer, path)?,
        VectorDriver::GPX => gpx::write(layer, path)?,
        VectorDriver::KML => kml::write(std::slice::from_ref(layer), path)?,
        VectorDriver::KMZ => kml::write_kmz(std::slice::from_ref(layer), path)?,
        VectorDriver::TopoJSON => topojson::write(layer, path)?,
        VectorDriver::MapInfo => mapinfo::write(layer, path)?,
        _ => return Ok(false),
    }
    Ok(true)
}

/// Fixes what the format library got wrong in a layer it read: wbvector takes a GeoPackage's
/// `srs_id` for an EPSG code even when the organization is not EPSG (GDAL's 99999 undefined SRS,
/// custom definitions from 100000 up), and returns a regenerated WKT without the definition's
/// authority (which identifies NAD83 as NAD83(2011)) instead of the stored one.
pub(crate) fn fix(layer: &mut Layer, driver: VectorDriver) {
    if driver != VectorDriver::GPKG {
        return;
    }
    let Some(code) = layer.crs_epsg() else { return };
    if wbprojection::to_ogc_wkt(code).is_ok() {
        layer.set_crs_wkt(None);
        return;
    }
    match layer.crs_wkt().filter(|w| !w.trim().eq_ignore_ascii_case("undefined")).map(crate::crs::SpatialRef::from_wkt) {
        Some(s) => layer.set_crs_epsg(s.epsg),
        None => {
            layer.set_crs_epsg(None);
            layer.set_crs_wkt(None);
        }
    }
}

/// The EPSG code to write for a layer's CRS: the layer's code when its WKT is absent or is the
/// registry's definition of that code, otherwise only a code the WKT itself declares. Codes
/// identified by matching parameters can be wrong (a US survey foot State Plane CRS matched to
/// its metre twin), so a writer keeps the source WKT instead.
pub(crate) fn trusted_epsg(layer: &Layer) -> Option<u32> {
    match (layer.crs_epsg(), layer.crs_wkt()) {
        (code, None) => code,
        (Some(code), Some(wkt)) if wbprojection::to_ogc_wkt(code).is_ok_and(|w| w == wkt) => Some(code),
        (_, Some(wkt)) => wbprojection::epsg::epsg_from_wkt(wkt),
    }
}

/// The EPSG code naming a layer's CRS in formats that can only name it by code (GeoJSON, GML):
/// the trusted code, or else the identified one.
pub(crate) fn naming_epsg(layer: &Layer) -> Option<u32> {
    trusted_epsg(layer).or_else(|| crate::crs::SpatialRef::from_layer(layer).epsg)
}

/// The geometry type every feature has, as GDAL declares a layer it writes; `None` when they
/// differ (GDAL then declares `GEOMETRY`, keeping each feature's type) or there are none.
pub(crate) fn uniform_type(layer: &Layer) -> Option<GeometryType> {
    let mut types = layer.features.iter().filter_map(|f| f.geometry.as_ref()).map(|g| g.geom_type());
    let first = types.next()?;
    types.all(|t| t == first).then_some(first)
}

/// The type the features' geometries share, counting a mix of single and multi-part forms as the
/// multi-part type. `None` when there are no geometries or they mix other types.
pub(crate) fn features_type(layer: &Layer) -> Option<GeometryType> {
    use GeometryType as T;
    let mut found: Option<GeometryType> = None;
    for t in layer.features.iter().filter_map(|f| f.geometry.as_ref()).map(|g| g.geom_type()) {
        found = Some(match (found, t) {
            (None, t) => t,
            (Some(a), b) if a == b => a,
            (Some(T::Point | T::MultiPoint), T::Point | T::MultiPoint) => T::MultiPoint,
            (Some(T::LineString | T::MultiLineString), T::LineString | T::MultiLineString) => T::MultiLineString,
            (Some(T::Polygon | T::MultiPolygon), T::Polygon | T::MultiPolygon) => T::MultiPolygon,
            _ => return None,
        });
    }
    found
}

/// Builds polygons from rings by containment, whatever their orientation, as GDAL's
/// `organizePolygons` does: a ring inside an odd number of others is a hole of the smallest ring
/// containing it. Rings of fewer than three points are dropped; several polygons make a
/// multipolygon.
pub(crate) fn polygons_from_rings(rings: Vec<Vec<wbvector::Coord>>) -> Option<wbvector::Geometry> {
    use wbvector::{Geometry, Ring};
    let rings: Vec<Ring> = rings
        .into_iter()
        .map(|mut cs| {
            if cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
                cs.pop();
            }
            Ring::new(cs)
        })
        .filter(|r| r.len() >= 3)
        .collect();
    // Like GDAL, decide with the first vertex not on the other ring's boundary.
    let inside = |a: &Ring, b: &Ring| a.0.iter().find(|p| !on_boundary(b, p.x, p.y)).is_some_and(|p| shapefile::ring_contains(b, p.x, p.y));
    let depth: Vec<usize> =
        rings.iter().enumerate().map(|(i, r)| rings.iter().enumerate().filter(|(j, o)| *j != i && inside(r, o)).count()).collect();
    let mut polygons: Vec<(usize, Ring, Vec<Ring>)> = Vec::new();
    for (i, r) in rings.iter().enumerate() {
        if depth[i].is_multiple_of(2) {
            polygons.push((i, r.clone(), Vec::new()));
        }
    }
    for (i, r) in rings.iter().enumerate() {
        if depth[i] % 2 == 1 {
            let owner = polygons
                .iter_mut()
                .filter(|(_, e, _)| inside(r, e))
                .min_by(|a, b| a.1.signed_area().abs().total_cmp(&b.1.signed_area().abs()));
            if let Some((_, _, holes)) = owner {
                holes.push(r.clone());
            }
        }
    }
    let mut polygons: Vec<(Ring, Vec<Ring>)> = polygons.into_iter().map(|(_, e, h)| (e, h)).collect();
    match polygons.len() {
        0 => None,
        1 => {
            let (exterior, interiors) = polygons.pop()?;
            Some(Geometry::Polygon { exterior, interiors })
        }
        _ => Some(Geometry::MultiPolygon(polygons)),
    }
}

/// Whether a point lies on a ring's boundary.
fn on_boundary(r: &wbvector::Ring, x: f64, y: f64) -> bool {
    let cs = &r.0;
    (0..cs.len()).any(|i| {
        let (a, b) = (&cs[i], &cs[(i + 1) % cs.len()]);
        let cross = (b.x - a.x) * (y - a.y) - (b.y - a.y) * (x - a.x);
        let scale = (b.x - a.x).abs().max((b.y - a.y).abs()).max(1e-300);
        cross.abs() <= 1e-12 * scale && x >= a.x.min(b.x) && x <= a.x.max(b.x) && y >= a.y.min(b.y) && y <= a.y.max(b.y)
    })
}

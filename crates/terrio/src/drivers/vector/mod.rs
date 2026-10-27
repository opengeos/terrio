//! The vector drivers terrio supplies itself, where the format libraries are wrong or missing, and
//! fixes applied to what the format libraries read.

#[cfg(feature = "cad")]
pub(crate) mod cad;
pub mod csv;
pub(crate) mod datetime;
#[cfg(feature = "flatgeobuf")]
pub mod flatgeobuf;
pub mod geojson;
#[cfg(feature = "sqlite")]
pub mod geopackage;
#[cfg(feature = "parquet")]
pub mod geoparquet;
pub mod gml;
#[cfg(feature = "sqlite")]
pub(crate) mod gpkg_edit;
#[cfg(feature = "sqlite")]
pub(crate) mod gpkg_inplace;
#[cfg(feature = "sqlite")]
pub(crate) mod gpkg_read;
#[cfg(feature = "sqlite")]
pub(crate) mod gpkg_update;
pub mod gpx;
pub mod kml;
pub mod kml_style;
pub mod mapinfo;
#[cfg(feature = "osm")]
pub mod osm;
pub(crate) mod profile;
pub(crate) mod projjson;
pub mod shapefile;
#[cfg(feature = "sqlite")]
pub mod sqlite;
pub mod tab;
pub mod topojson;
pub mod wkt;
pub mod xml;

use crate::Result;
use crate::dataset::{VectorOpenOptions, VectorWriteOptions};
use crate::formats::VectorDriver;
use wbvector::{GeometryType, Layer};

/// A temporary file to write and read back (in memory in WebAssembly).
pub(crate) trait Spill: std::io::Read + std::io::Write + std::io::Seek + Send {}
impl<T: std::io::Read + std::io::Write + std::io::Seek + Send> Spill for T {}

/// A new, empty [`Spill`], deleted when dropped.
pub(crate) fn spill() -> Result<Box<dyn Spill>> {
    #[cfg(not(target_family = "wasm"))]
    {
        Ok(Box::new(tempfile::tempfile()?))
    }
    #[cfg(target_family = "wasm")]
    {
        Ok(Box::new(std::io::Cursor::new(Vec::new())))
    }
}

/// Reads a dataset with terrio's own driver for the format. `None` when the format library reads it.
pub(crate) fn read(path: &str, driver: VectorDriver, options: &VectorOpenOptions) -> Result<Option<Vec<Layer>>> {
    driver.ensure_built()?;
    Ok(Some(match driver {
        #[cfg(feature = "flatgeobuf")]
        VectorDriver::FlatGeobuf => match options.spatial_filter {
            // With a filter, only the index nodes on the way and the features found are read.
            Some(rect) => {
                let mut f = std::io::BufReader::new(std::fs::File::open(path)?);
                let len = std::fs::metadata(path)?.len();
                vec![flatgeobuf::read_filtered(&mut f, len, Some(rect), &|_| Ok(()))?.1]
            }
            None => vec![flatgeobuf::read(path)?],
        },
        #[cfg(feature = "cad")]
        VectorDriver::DXF | VectorDriver::DWG => vec![cad::parse(std::fs::read(path)?, driver)?],
        VectorDriver::GeoJSON => vec![geojson::read(path)?],
        VectorDriver::GeoJSONSeq => {
            let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
            vec![geojson::parse_seq(&std::fs::read(path)?, stem)?]
        }
        VectorDriver::CSV => vec![csv::read(path, &options.csv)?],
        #[cfg(feature = "parquet")]
        VectorDriver::Parquet => vec![geoparquet::read_filtered(path, options.spatial_filter)?],
        VectorDriver::Shapefile => vec![shapefile::read(path)?],
        VectorDriver::GML => gml::read(path)?,
        VectorDriver::GPX => gpx::read(path)?,
        VectorDriver::KML => kml::read(path)?,
        VectorDriver::KMZ => kml::read_kmz(path)?,
        VectorDriver::TopoJSON => topojson::read(path)?,
        VectorDriver::MapInfo if crate::formats::extension(path).as_deref() == Some("tab") => vec![tab::read(path)?],
        VectorDriver::MapInfo => vec![mapinfo::read(path)?],
        #[cfg(feature = "osm")]
        VectorDriver::OSM => osm::read(path)?,
        _ => return Ok(None),
    }))
}

/// Writes layers with terrio's own driver for the format. `false` when the format library writes it.
pub(crate) fn write(layer: &Layer, path: &str, driver: VectorDriver, options: &VectorWriteOptions) -> Result<bool> {
    driver.ensure_built()?;
    match driver {
        #[cfg(feature = "flatgeobuf")]
        VectorDriver::FlatGeobuf => flatgeobuf::write(layer, path)?,
        #[cfg(feature = "cad")]
        VectorDriver::DXF | VectorDriver::DWG => std::fs::write(path, cad::to_bytes(std::slice::from_ref(layer), driver)?)?,
        VectorDriver::GeoJSON => geojson::write(layer, path, &options.geojson)?,
        VectorDriver::GeoJSONSeq => std::fs::write(path, geojson::to_seq_string(layer)?)?,
        VectorDriver::CSV => csv::write(layer, path, &options.csv)?,
        #[cfg(feature = "parquet")]
        VectorDriver::Parquet => geoparquet::write(layer, path)?,
        VectorDriver::Shapefile => shapefile::write(layer, path)?,
        VectorDriver::GML => gml::write(layer, path)?,
        VectorDriver::GPX => gpx::write(layer, path)?,
        VectorDriver::KML => kml::write(std::slice::from_ref(layer), path)?,
        VectorDriver::KMZ => kml::write_kmz(std::slice::from_ref(layer), path)?,
        VectorDriver::TopoJSON => topojson::write(layer, path)?,
        VectorDriver::MapInfo if crate::formats::extension(path).as_deref() == Some("tab") => tab::write(layer, path)?,
        VectorDriver::MapInfo => mapinfo::write(layer, path)?,
        _ => return Ok(false),
    }
    Ok(true)
}

/// Reads a dataset held in memory with terrio's own driver for the format. `name` is the file
/// name the bytes came from (its stem names single layers). `None` for formats terrio cannot read
/// from memory itself. Formats made of several files are read from their main file alone (a
/// shapefile without attributes or CRS): see [`read_parts`].
pub(crate) fn read_bytes(bytes: Vec<u8>, name: &str, driver: VectorDriver, options: &VectorOpenOptions) -> Result<Option<Vec<Layer>>> {
    driver.ensure_built()?;
    if matches!(driver, VectorDriver::Shapefile | VectorDriver::MapInfo) {
        return read_parts(&bytes, name, driver, &|_: &str| None).map(Some);
    }
    let stem = std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    Ok(Some(match driver {
        #[cfg(feature = "flatgeobuf")]
        VectorDriver::FlatGeobuf => vec![flatgeobuf::parse(&bytes)?],
        #[cfg(feature = "cad")]
        VectorDriver::DXF | VectorDriver::DWG => vec![cad::parse(bytes, driver)?],
        VectorDriver::GeoJSON => vec![geojson::parse(&bytes, stem)?],
        VectorDriver::GeoJSONSeq => vec![geojson::parse_seq(&bytes, stem)?],
        VectorDriver::CSV => {
            let tsv = crate::formats::extension(name).as_deref() == Some("tsv");
            let options = csv::CsvOptions { separator: options.csv.separator.or(tsv.then_some(b'\t')), ..options.csv.clone() };
            vec![csv::parse(&xml::decode(&bytes), stem, &options)?]
        }
        #[cfg(feature = "parquet")]
        VectorDriver::Parquet => vec![geoparquet::parse(bytes, stem)?],
        VectorDriver::GML => gml::parse(&xml::parse(&xml::decode(&bytes))?, None)?,
        VectorDriver::GPX => gpx::parse(&xml::decode(&bytes))?,
        VectorDriver::KML => kml::parse(&xml::decode(&bytes), stem)?,
        VectorDriver::KMZ => kml::parse_kmz(bytes, stem)?,
        VectorDriver::TopoJSON => topojson::parse(&bytes)?,
        #[cfg(feature = "osm")]
        VectorDriver::OSM => osm::parse(bytes)?,
        #[cfg(feature = "sqlite")]
        VectorDriver::GPKG => gpkg_read::parse(bytes, &[])?,
        _ => return Ok(None),
    }))
}

/// Reads a format made of several files from memory: the main file (`.shp`, `.mif`) and its
/// sidecars, which `sidecar` returns by extension (`dbf`, `prj`, `cpg`, `mid`; lowercase).
pub(crate) fn read_parts(main: &[u8], name: &str, driver: VectorDriver, sidecar: &dyn Fn(&str) -> Option<Vec<u8>>) -> Result<Vec<Layer>> {
    let stem = std::path::Path::new(name).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    let text = |ext: &str| sidecar(ext).map(|b| String::from_utf8_lossy(&b).into_owned());
    match driver {
        VectorDriver::Shapefile => {
            let dbf = sidecar("dbf");
            Ok(vec![shapefile::parse(main, dbf.as_deref(), text("prj").as_deref(), text("cpg").as_deref(), stem)?])
        }
        VectorDriver::MapInfo if crate::formats::extension(name).as_deref() == Some("tab") => {
            let dat = sidecar("dat").ok_or_else(|| crate::Error::InvalidData(format!("'{name}': no .dat file")))?;
            Ok(vec![tab::parse(main, &dat, sidecar("map").as_deref(), sidecar("id").as_deref(), stem)?])
        }
        VectorDriver::MapInfo => Ok(vec![mapinfo::parse(main, sidecar("mid").as_deref(), stem)?]),
        d => Err(crate::Error::Unsupported(format!("the {} format is not made of several files", d.name()))),
    }
}

/// Encodes layers with terrio's own driver for the format. `None` for formats made of several
/// files (Shapefile, MapInfo). GML goes without its `.xsd`. Formats holding one layer take the
/// first.
pub(crate) fn write_bytes(layers: &[Layer], driver: VectorDriver, options: &VectorWriteOptions) -> Result<Option<Vec<u8>>> {
    let Some(first) = layers.first() else { return Err(crate::Error::InvalidArgument("no layers to write".into())) };
    driver.ensure_built()?;
    Ok(Some(match driver {
        #[cfg(feature = "sqlite")]
        VectorDriver::GPKG => geopackage::to_bytes(layers)?,
        #[cfg(feature = "cad")]
        VectorDriver::DXF | VectorDriver::DWG => cad::to_bytes(layers, driver)?,
        VectorDriver::GPX => gpx::layers_to_string(layers)?.into_bytes(),
        VectorDriver::KML => kml::to_string(layers)?.into_bytes(),
        VectorDriver::KMZ => kml::kmz_bytes(layers)?,
        VectorDriver::TopoJSON => topojson::to_string(layers)?.into_bytes(),
        #[cfg(feature = "flatgeobuf")]
        VectorDriver::FlatGeobuf => flatgeobuf::to_bytes(first)?,
        VectorDriver::GeoJSON => geojson::to_string(first, &options.geojson)?.into_bytes(),
        VectorDriver::GeoJSONSeq => geojson::to_seq_string(first)?.into_bytes(),
        VectorDriver::CSV => csv::to_string(first, &options.csv)?.into_bytes(),
        #[cfg(feature = "parquet")]
        VectorDriver::Parquet => geoparquet::to_bytes(first)?,
        // Without its schema file: readers infer the field types.
        VectorDriver::GML => gml::to_string(first, "").into_bytes(),
        _ => return Ok(None),
    }))
}

/// The EPSG code to write for a layer's CRS: the layer's code when its WKT is absent or is the
/// registry's definition of that code, otherwise only a code the WKT itself declares. Codes
/// identified by matching parameters can be wrong (a US survey foot State Plane CRS matched to
/// its metre twin), so a writer keeps the source WKT instead. ESRI codes the registry holds as
/// EPSG codes (54009) are not EPSG codes.
pub(crate) fn trusted_epsg(layer: &Layer) -> Option<u32> {
    let code = match (layer.crs_epsg(), layer.crs_wkt()) {
        (code, None) => code,
        (Some(code), Some(wkt)) if wbprojection::to_ogc_wkt(code).is_ok_and(|w| w == wkt) => Some(code),
        // Only CRS codes: the registry can return a datum's or prime meridian's.
        (_, Some(wkt)) => wbprojection::epsg::epsg_from_wkt(wkt).filter(|c| wbprojection::to_ogc_wkt(*c).is_ok()),
    };
    code.filter(|c| !crate::crs::is_esri_code(*c))
}

/// The WKT to write for a layer's CRS: GDAL-style WKT with the EPSG authority when the code is
/// trusted (so that GDAL and PROJ recognize it), and the stored definition otherwise.
pub(crate) fn output_wkt(layer: &Layer) -> Option<String> {
    match trusted_epsg(layer).and_then(|c| crate::crs::SpatialRef::from_epsg(c).ok()) {
        Some(s) => super::wkt::gdal_wkt(&s),
        None => crate::crs::SpatialRef::from_layer(layer).to_wkt(),
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
    // The rings are moved into the polygons: exteriors first, then each hole to its owner.
    let mut rings: Vec<Option<Ring>> = rings.into_iter().map(Some).collect();
    let mut polygons: Vec<(usize, Ring, Vec<Ring>)> = Vec::new();
    for (i, r) in rings.iter_mut().enumerate() {
        if depth[i].is_multiple_of(2)
            && let Some(r) = r.take()
        {
            polygons.push((i, r, Vec::new()));
        }
    }
    let areas: Vec<f64> = polygons.iter().map(|(_, e, _)| e.signed_area().abs()).collect();
    for r in rings.into_iter().flatten() {
        let owner = polygons
            .iter()
            .enumerate()
            .filter(|(_, (_, e, _))| inside(&r, e))
            .min_by(|a, b| areas[a.0].total_cmp(&areas[b.0]))
            .map(|(k, _)| k);
        if let Some(k) = owner {
            polygons[k].2.push(r);
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

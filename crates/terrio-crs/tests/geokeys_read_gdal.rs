//! GeoKeys of user-defined projected CRSs, as writers other than GDAL write them, are read as GDAL
//! 3.12.2 (libgeotiff's `GTIFFetchProjParms`) reads them: ProjCenterEasting/NorthingGeoKey as the
//! false easting and northing, a Hotine oblique Mercator without ProjRectifiedGridAngleGeoKey at
//! 90°, and a polar stereographic CRS of ProjNatOriginLatGeoKey 90° without its
//! ProjStdParallel1GeoKey (opengeos/terrio#1012, item 35).
//!
//! `data/geokeys_read_gdal.json` (`scripts/gen_geokeys_read_gdal.py`, GDAL 3.12.2,
//! `PROJ_NETWORK=OFF`) holds the keys and where GDAL's CRS places a point from WGS 84
//! (`always_xy`, by pyproj). Tolerance: 1 mm.

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

#[test]
fn user_defined_geokeys_are_read_as_gdal_reads_them() {
    let data: Value = serde_json::from_str(include_str!("data/geokeys_read_gdal.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 5);
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let directory: Vec<u16> = case["directory"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u16).collect();
        let doubles: Vec<f64> = case["doubles"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
        let crs = SpatialRef::from_geokeys(&directory, &doubles, b"").unwrap_or_else(|e| panic!("{name}: {e}"));
        let (lon, lat) = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
        let (x, y) = Transformer::new(&wgs84, &crs).and_then(|t| t.transform(lon, lat)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let (gx, gy) = (case["gdal"][0].as_f64().unwrap(), case["gdal"][1].as_f64().unwrap());
        assert!((x - gx).abs() < 1e-3 && (y - gy).abs() < 1e-3, "{name}: ({x}, {y}), GDAL ({gx}, {gy})");
    }
}

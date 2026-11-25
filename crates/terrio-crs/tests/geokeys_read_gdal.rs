//! GeoKeys of user-defined projected CRSs, as writers other than GDAL write them, are read as GDAL
//! 3.12.2 (libgeotiff's `GTIFFetchProjParms`) reads them: ProjCenterEasting/NorthingGeoKey as the
//! false easting and northing, a Hotine oblique Mercator without ProjRectifiedGridAngleGeoKey at
//! 90°, and a polar stereographic CRS of ProjNatOriginLatGeoKey 90° without its
//! ProjStdParallel1GeoKey (opengeos/terrio#1012, item 35). GeoKeys with a projected code alone
//! read as the registry's CRS (a deprecated code's replacement) in their linear unit, an
//! ellipsoid, datum or prime meridian by its code alone, Mercator's scale and latitude keys, and
//! Laborde oblique Mercator (items 32 to 35). Not as GDAL: a Stereographic CRS's
//! ProjScaleAtCenterGeoKey, which terrio wrote before and still reads where there is no
//! ProjScaleAtNatOriginGeoKey (GDAL reads a scale of 1).
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
    assert_eq!(cases.len(), 24);
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let directory: Vec<u16> = case["directory"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u16).collect();
        let doubles: Vec<f64> = case["doubles"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
        let crs = SpatialRef::from_geokeys(&directory, &doubles, b"").unwrap_or_else(|e| panic!("{name}: {e}"));
        let (lon, lat) = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
        // From WGS 84, or from the CRS's own geographic CRS on a sphere, where PROJ has no
        // transformation from WGS 84.
        let source = match case["from"].as_str() {
            Some("geodetic") => SpatialRef::from_wkt(&crs.geographic_wkt().unwrap()),
            _ => wgs84.clone(),
        };
        let (x, y) = Transformer::new(&source, &crs).and_then(|t| t.transform(lon, lat)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let (gx, gy) = (case["gdal"][0].as_f64().unwrap(), case["gdal"][1].as_f64().unwrap());
        assert!((x - gx).abs() < 1e-3 && (y - gy).abs() < 1e-3, "{name}: ({x}, {y}), GDAL ({gx}, {gy})");
    }
}

/// A polar stereographic CRS whose origin latitude or standard parallel is not finite is not
/// read at the north pole: no CRS, or one that places no point.
#[test]
fn a_polar_stereographic_latitude_that_is_not_finite_is_refused() {
    let data: Value = serde_json::from_str(include_str!("data/geokeys_read_gdal.json")).unwrap();
    let case = data["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "polar stereographic at 90° with a standard parallel of -71°")
        .unwrap();
    let directory: Vec<u16> = case["directory"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u16).collect();
    let doubles: Vec<f64> = case["doubles"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let place = |doubles: &[f64]| {
        SpatialRef::from_geokeys(&directory, doubles, b"").and_then(|crs| Transformer::new(&wgs84, &crs)?.transform(10.0, 80.0))
    };
    assert!(place(&doubles).is_ok());
    // The origin latitude; the standard parallel is read only without one.
    let changed: Vec<f64> = doubles.iter().map(|v| if *v == 90.0 { f64::NAN } else { *v }).collect();
    assert_ne!(changed, doubles);
    assert!(place(&changed).is_err());
}

/// A standard parallel that is not finite does not refuse a polar stereographic CRS whose origin
/// latitude is given, as the standard parallel is then not read (GDAL 3.12.2 reads 90 with any
/// standard parallel as variant A).
#[test]
fn an_unread_standard_parallel_that_is_not_finite_is_ignored() {
    let data: Value = serde_json::from_str(include_str!("data/geokeys_read_gdal.json")).unwrap();
    let case = data["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "polar stereographic at 90° with a standard parallel of -71°")
        .unwrap();
    let directory: Vec<u16> = case["directory"].as_array().unwrap().iter().map(|v| v.as_u64().unwrap() as u16).collect();
    let doubles: Vec<f64> =
        case["doubles"].as_array().unwrap().iter().map(|v| if *v == -71.0 { f64::NAN } else { v.as_f64().unwrap() }).collect();
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let crs = SpatialRef::from_geokeys(&directory, &doubles, b"").unwrap();
    let (x, y) = Transformer::new(&wgs84, &crs).and_then(|t| t.transform(10.0, 80.0)).unwrap();
    let (gx, gy) = (case["gdal"][0].as_f64().unwrap(), case["gdal"][1].as_f64().unwrap());
    assert!((x - gx).abs() < 1e-3 && (y - gy).abs() < 1e-3, "({x}, {y}), GDAL ({gx}, {gy})");
}

/// With wbprojection, a citation's CRS name gives its EPSG code only when the keys' own conversion
/// is that code's, as GDAL takes a citation's CRS (#1012 item 49): EPSG:2048 with a false easting
/// of 1000 written to GeoKeys reads back without the code, 1000 m from EPSG:2048; with its own false
/// easting, as EPSG:2048.
#[cfg(feature = "wbprojection-fallback")]
#[test]
fn a_citation_names_its_crs_only_for_the_same_conversion() {
    let wkt = SpatialRef::from_epsg(2048).unwrap().to_wkt().unwrap();
    let start = wkt.find("\"false_easting\",").unwrap() + "\"false_easting\",".len();
    let end = start + wkt[start..].find(']').unwrap();
    for (false_easting, code) in [("1000", None), ("0", Some(2048))] {
        let crs = SpatialRef::from_wkt(&format!("{}{false_easting}{}", &wkt[..start], &wkt[end..]));
        let tags = crs.to_geokeys().unwrap().encode().unwrap();
        let back = terrio_crs::geokeys::read(&tags.directory, &tags.doubles, tags.ascii.as_bytes()).crs.unwrap();
        assert_eq!(back.epsg, code, "false easting {false_easting}");
    }
}

/// A Stereographic CRS's scale in ProjScaleAtCenterGeoKey alone, as terrio wrote it before, keeps
/// that scale (GDAL 3.12.2 reads 1): a file terrio wrote reads back as it was. The writer puts it
/// in ProjScaleAtNatOriginGeoKey, as GDAL does.
#[test]
fn a_stereographic_scale_terrio_wrote_at_the_centre_is_kept() {
    let crs = |key: u16| {
        let directory = [
            1u16, 1, 0, 10, 1024, 0, 1, 1, 1025, 0, 1, 1, 2048, 0, 1, 4326, 3072, 0, 1, 32767, 3074, 0, 1, 32767, 3075, 0, 1, 14, 3076, 0,
            1, 9001, 3088, 34736, 1, 0, 3089, 34736, 1, 1, key, 34736, 1, 2,
        ];
        SpatialRef::from_geokeys(&directory, &[10.0, 40.0, 0.9], b"").unwrap()
    };
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let place = |c: &SpatialRef| Transformer::new(&wgs84, c).unwrap().transform(12.0, 41.0).unwrap();
    let (at_origin, at_centre) = (place(&crs(3092)), place(&crs(3093)));
    assert!((at_origin.0 - at_centre.0).abs() < 1e-6 && (at_origin.1 - at_centre.1).abs() < 1e-6, "{at_origin:?} {at_centre:?}");
    // GDAL's scale of 1 would place it 10% farther.
    assert!((at_centre.0 - 151_459.646_927_064_8).abs() < 1e-3, "{at_centre:?}");
    let written = SpatialRef::from_proj4("+proj=stere +lat_0=40 +lon_0=10 +k=0.9 +datum=WGS84").unwrap().to_geokeys().unwrap();
    assert!(written.doubles.iter().any(|(id, v)| *id == 3092 && *v == 0.9), "{written:?}");
}

/// A projected code alone in another linear unit, with VerticalCSTypeGeoKey, keeps its vertical
/// CRS: UTM zone 33N in feet with EGM96 heights.
#[test]
fn a_code_in_another_unit_keeps_its_vertical_crs() {
    let directory = [1u16, 1, 0, 4, 1024, 0, 1, 1, 3072, 0, 1, 32633, 3076, 0, 1, 9002, 4096, 0, 1, 5773];
    let crs = SpatialRef::from_geokeys(&directory, &[], b"").unwrap();
    let wkt = crs.to_wkt().unwrap();
    assert!(wkt.starts_with("COMPD_CS[") && wkt.contains("EGM96"), "{wkt}");
    let (x, _) = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).unwrap().transform(15.0, 50.0).unwrap();
    assert!((x - 1_640_419.947_506_561_4).abs() < 1e-3, "{x}");
}

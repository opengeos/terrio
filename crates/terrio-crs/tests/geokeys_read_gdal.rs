//! GeoKeys of user-defined projected CRSs, as writers other than GDAL write them, are read as GDAL
//! 3.12.2 (libgeotiff's `GTIFFetchProjParms`) reads them: ProjCenterEasting/NorthingGeoKey as the
//! false easting and northing, a Hotine oblique Mercator without ProjRectifiedGridAngleGeoKey at
//! 90°, and a polar stereographic CRS of ProjNatOriginLatGeoKey 90° without its
//! ProjStdParallel1GeoKey (opengeos/terrio#1012, item 35). GeoKeys with a projected code alone
//! read as the registry's CRS (a deprecated code's replacement) in their linear unit, an
//! ellipsoid, datum or prime meridian by its code alone, Mercator's scale and latitude keys, and
//! Laborde oblique Mercator (items 32 to 35). Not as GDAL: a Stereographic CRS's
//! ProjScaleAtCenterGeoKey, which terrio wrote before and still reads where there is no
//! ProjScaleAtNatOriginGeoKey (GDAL reads a scale of 1), and a polar stereographic CRS's
//! ProjStdParallel1GeoKey without an origin latitude, read as its standard parallel for the
//! writers that put it there (GDAL reads a latitude of true scale of 0 at the north pole).
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
    assert_eq!(cases.len(), 28);
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
        // A code stated in ProjectedCSTypeGeoKey gives the CRS the code GDAL gives it, or none:
        // EPSG:4326 whatever the linear unit, none for a projected code in another unit.
        let stated = directory[4..].chunks(4).any(|e| e[0] == 3072 && e[3] != 32767);
        if stated {
            let code = case
                .get("gdal_code")
                .expect("gdal_code (run scripts/gen_geokeys_read_gdal.py)")
                .as_str()
                .map(|c| c.parse::<u32>().unwrap());
            assert_eq!(crs.epsg, code, "{name}");
        }
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

/// With wbprojection, a citation does not name a CRS of other standard parallels: EPSG:5070
/// (Albers, 29.5° and 45.5°) with a first parallel of 20°, or a second of 50°, written to GeoKeys
/// reads back without the code.
#[cfg(feature = "wbprojection-fallback")]
#[test]
fn a_citation_names_its_crs_only_for_the_same_standard_parallels() {
    let wkt = SpatialRef::from_epsg(5070).unwrap().to_wkt().unwrap();
    for (key, parallel, code) in [("1", "20", None), ("2", "50", None), ("1", "29.5", Some(5070))] {
        let tag = format!("\"standard_parallel_{key}\",");
        let start = wkt.find(&tag).unwrap() + tag.len();
        let end = start + wkt[start..].find(']').unwrap();
        let crs = SpatialRef::from_wkt(&format!("{}{parallel}{}", &wkt[..start], &wkt[end..]));
        let tags = crs.to_geokeys().unwrap().encode().unwrap();
        let back = terrio_crs::geokeys::read(&tags.directory, &tags.doubles, tags.ascii.as_bytes()).crs.unwrap();
        assert_eq!(back.epsg, code, "standard parallel {key} of {parallel}");
    }
}

/// A variant of EPSG:5070 that PROJ 9.9.0 refuses (parallels 29.5° and -29.5°, its code removed)
/// is no EPSG:5070, with wbprojection too, which matches it by name: PROJ identifies no code and
/// places no point, and its GeoKeys are user-defined, not ProjectedCSTypeGeoKey 5070.
#[test]
fn a_refused_variant_of_a_registered_crs_is_not_identified_by_its_name() {
    let wkt = SpatialRef::from_epsg(5070).unwrap().to_wkt().unwrap();
    let refused = wkt.replace("\"standard_parallel_2\",45.5", "\"standard_parallel_2\",-29.5");
    assert_ne!(refused, wkt);
    let wkt = refused;
    let wkt = format!("{}]", &wkt[..wkt.rfind(",AUTHORITY").unwrap()]);
    let crs = SpatialRef::from_wkt(&wkt);
    assert_eq!(crs.epsg, None, "{wkt}");
    assert!(Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).and_then(|t| t.transform(-96.0, 40.0)).is_err());
    let tags = crs.to_geokeys().unwrap().encode().unwrap();
    assert!(!tags.directory[4..].chunks(4).any(|e| e[0] == 3072 && e[3] != 32767), "{:?}", tags.directory);
}

/// With wbprojection, a citation does not name a CRS for keys of a conversion terrio does not read:
/// an off-equator Mercator 1SP at a scale of 0.9 (GDAL 3.12.2 builds a CRS PROJ cannot run) citing
/// WGS 84 / World Mercator (EPSG:3395) gives no CRS, not EPSG:3395's scale of 1 at the equator.
#[cfg(feature = "wbprojection-fallback")]
#[test]
fn a_citation_names_no_crs_for_keys_terrio_does_not_read() {
    let citation = b"WGS 84 / World Mercator|\0";
    let directory = [
        1u16,
        1,
        0,
        8,
        1024,
        0,
        1,
        1,
        1025,
        0,
        1,
        1,
        1026,
        34737,
        citation.len() as u16 - 1,
        0,
        2048,
        0,
        1,
        4326,
        3072,
        0,
        1,
        32767,
        3075,
        0,
        1,
        7,
        3081,
        34736,
        1,
        0,
        3092,
        34736,
        1,
        1,
    ];
    let read = terrio_crs::geokeys::read(&directory, &[10.0, 0.9], citation);
    assert!(read.crs.is_none(), "{:?}", read.crs);
    // At a scale of 1, the citation's CRS.
    let read = terrio_crs::geokeys::read(&directory, &[0.0, 1.0], citation);
    assert_eq!(read.crs.and_then(|c| c.epsg), Some(3395));
}

/// With wbprojection, a citation does not name a CRS for keys whose conversion terrio cannot read:
/// the GeoKeys of EPSG:5070 with a false easting of 1000, then given a false easting that is not a
/// number, read back without the code.
#[cfg(feature = "wbprojection-fallback")]
#[test]
fn a_citation_names_no_crs_for_an_unreadable_conversion() {
    let wkt = SpatialRef::from_epsg(5070).unwrap().to_wkt().unwrap();
    let tag = "\"false_easting\",";
    let start = wkt.find(tag).unwrap() + tag.len();
    let end = start + wkt[start..].find(']').unwrap();
    let tags = SpatialRef::from_wkt(&format!("{}1000{}", &wkt[..start], &wkt[end..])).to_geokeys().unwrap().encode().unwrap();
    let mut doubles = tags.doubles.clone();
    let at = doubles.iter().position(|v| *v == 1000.0).unwrap();
    doubles[at] = f64::NAN;
    let back = terrio_crs::geokeys::read(&tags.directory, &doubles, tags.ascii.as_bytes()).crs;
    assert!(back.as_ref().is_none_or(|c| c.epsg.is_none()), "{tags:?} {back:?}");
}

/// With wbprojection, a citation does not name a Mercator CRS of another latitude of true scale:
/// Mercator 2SP keys at 10° citing WGS 84 / World Mercator (EPSG:3395, true scale at the equator,
/// whose PROJ string gives no `lat_ts`) give no EPSG:3395; at 0°, EPSG:3395.
#[cfg(feature = "wbprojection-fallback")]
#[test]
fn a_citation_names_its_mercator_only_for_the_same_latitude_of_true_scale() {
    let citation = b"WGS 84 / World Mercator|\0";
    let directory = [
        1u16,
        1,
        0,
        8,
        1024,
        0,
        1,
        1,
        1025,
        0,
        1,
        1,
        1026,
        34737,
        citation.len() as u16 - 1,
        0,
        2048,
        0,
        1,
        4326,
        3072,
        0,
        1,
        32767,
        3075,
        0,
        1,
        7,
        3078,
        34736,
        1,
        0,
        3080,
        34736,
        1,
        1,
    ];
    let read = |lat_ts: f64| terrio_crs::geokeys::read(&directory, &[lat_ts, 0.0], citation).crs.and_then(|c| c.epsg);
    assert_eq!(read(10.0), None);
    assert_eq!(read(0.0), Some(3395));
}

/// With wbprojection, a citation does not name a polar stereographic CRS of another latitude of
/// true scale: keys at the north pole with a scale of 1 there (variant A) citing WGS 84 / NSIDC Sea
/// Ice Polar Stereographic North (EPSG:3413, true scale at 70°) give no EPSG:3413; at a natural
/// origin latitude of 70°, which GDAL 3.12.2 reads as that latitude of true scale, EPSG:3413.
#[cfg(feature = "wbprojection-fallback")]
#[test]
fn a_citation_names_its_polar_stereographic_only_for_the_same_latitude_of_true_scale() {
    let citation = b"WGS 84 / NSIDC Sea Ice Polar Stereographic North|\0";
    let directory = [
        1u16,
        1,
        0,
        9,
        1024,
        0,
        1,
        1,
        1025,
        0,
        1,
        1,
        1026,
        34737,
        citation.len() as u16 - 1,
        0,
        2048,
        0,
        1,
        4326,
        3072,
        0,
        1,
        32767,
        3075,
        0,
        1,
        15,
        3081,
        34736,
        1,
        0,
        3092,
        34736,
        1,
        1,
        3095,
        34736,
        1,
        2,
    ];
    let read = |lat: f64| terrio_crs::geokeys::read(&directory, &[lat, 1.0, -45.0], citation).crs.and_then(|c| c.epsg);
    assert_eq!(read(90.0), None);
    assert_eq!(read(70.0), Some(3413));
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

/// A projected code alone in another linear unit with a zero GeogTOWGS84GeoKey on a datum other
/// than WGS 84 keeps that zero shift, as beside the code in its own unit: EPSG:27700 in feet with
/// `TOWGS84[0,0,0]` is OSGB36 shifted by nothing, not by OSGB36's registered transformations
/// (GDAL 3.12.2 reads such keys without the shift; terrio keeps it, as for the code in its own
/// unit). On WGS 84 (EPSG:32633 in feet) a zero shift is none.
#[test]
fn a_code_in_another_unit_keeps_a_zero_shift_off_wgs84() {
    let crs = |code: u16| {
        let directory = [1u16, 1, 0, 4, 1024, 0, 1, 1, 2062, 34736, 3, 0, 3072, 0, 1, code, 3076, 0, 1, 9002];
        SpatialRef::from_geokeys(&directory, &[0.0, 0.0, 0.0], b"").unwrap()
    };
    let wkt = crs(27700).to_wkt().unwrap();
    assert!(wkt.contains("OSGB") && wkt.contains("TOWGS84[0,0,0"), "{wkt}");
    let wkt = crs(32633).to_wkt().unwrap();
    assert!(!wkt.contains("TOWGS84"), "{wkt}");
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

/// Mercator 1SP off the equator (ProjNatOriginLatGeoKey 10°) is `+lat_ts` only at a scale of 1, as
/// GDAL 3.12 reads it. At another ProjScaleAtNatOriginGeoKey, GDAL's CRS is one PROJ 9 cannot run
/// ("Unsupported value for Latitude of natural origin"), so terrio places no point either rather
/// than drop the scale.
#[test]
fn an_off_equator_mercator_1sp_at_another_scale_is_not_placed() {
    let place = |scale: f64| {
        let directory = [
            1u16, 1, 0, 10, 1024, 0, 1, 1, 1025, 0, 1, 1, 2048, 0, 1, 4326, 3072, 0, 1, 32767, 3074, 0, 1, 32767, 3075, 0, 1, 7, 3076, 0,
            1, 9001, 3080, 34736, 1, 0, 3081, 34736, 1, 1, 3092, 34736, 1, 2,
        ];
        SpatialRef::from_geokeys(&directory, &[10.0, 10.0, scale], b"")
            .and_then(|crs| Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs)?.transform(12.0, 40.0))
    };
    // GDAL 3.12.1 with PROJ 9.7.1: (219278.728, 4765445.147).
    let (x, y) = place(1.0).unwrap();
    assert!((x - 219_278.728_136_306_08).abs() < 1e-3 && (y - 4_765_445.146_802_85).abs() < 1e-3, "({x}, {y})");
    assert!(place(0.9).is_err());
}

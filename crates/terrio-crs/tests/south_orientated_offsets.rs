//! South-orientated Transverse Mercator (EPSG method 9808) subtracts its false easting and
//! northing: W = FE - x, S = FN - y (EPSG Guidance Note 7-2), where x and y are Transverse
//! Mercator's easting and northing without offsets. PROJ 9.9.0 reads such a CRS but refuses to
//! transform it or write it as a PROJ string when the offsets are not zero (opengeos/terrio#1012,
//! item 18); terrio applies the method, a documented difference, and writes no PROJ string.
//!
//! Reference: the same CRS without offsets, which terrio places as PROJ does
//! (`+proj=tmerc +axis=wsu`), with the offsets subtracted. Tolerance: 1 mm.

use terrio_crs::{SpatialRef, Transformer};

fn esri(fe: f64, fn_: f64) -> String {
    format!(
        r#"PROJCS["Lo19",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator_South_Orientated"],PARAMETER["False_Easting",{fe}],PARAMETER["False_Northing",{fn_}],PARAMETER["Central_Meridian",19.0],PARAMETER["Scale_Factor",1.0],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#
    )
}

fn wkt2(fe: f64, fn_: f64) -> String {
    let deg = r#"ANGLEUNIT["degree",0.0174532925199433]"#;
    format!(
        r#"PROJCRS["Lo19",BASEGEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,{deg}]],CONVERSION["c",METHOD["Transverse Mercator (South Orientated)",ID["EPSG",9808]],PARAMETER["Latitude of natural origin",0,{deg},ID["EPSG",8801]],PARAMETER["Longitude of natural origin",19,{deg},ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",1,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",{fe},LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",{fn_},LENGTHUNIT["metre",1],ID["EPSG",8807]]],CS[Cartesian,2],AXIS["westing (Y)",west,ORDER[1],LENGTHUNIT["metre",1]],AXIS["southing (X)",south,ORDER[2],LENGTHUNIT["metre",1]]]"#
    )
}

#[test]
fn south_orientated_transverse_mercator_subtracts_its_offsets() {
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let (fe, fn_) = (1000.0, 2000.0);
    let points = [(19.0, 0.0), (18.5, -33.9), (20.2, -30.1)];
    for (form, build) in [("WKT 1", esri as fn(f64, f64) -> String), ("WKT 2", wkt2)] {
        let plain = Transformer::new(&wgs84, &SpatialRef::from_wkt(&build(0.0, 0.0))).unwrap();
        let crs = SpatialRef::from_wkt(&build(fe, fn_));
        let t = Transformer::new(&wgs84, &crs).unwrap();
        for (lon, lat) in points {
            let (x, y) = plain.transform(lon, lat).unwrap();
            let got = t.transform(lon, lat).unwrap();
            let want = (fe + x, fn_ + y);
            assert!((got.0 - want.0).abs() < 1e-3 && (got.1 - want.1).abs() < 1e-3, "{form} ({lon}, {lat}): {got:?}, {want:?}");
        }
        // The natural origin is (FE, FN).
        let origin = t.transform(19.0, 0.0).unwrap();
        assert!((origin.0 - fe).abs() < 1e-6 && (origin.1 - fn_).abs() < 1e-6, "{form}: {origin:?}");
        assert_eq!(crs.to_proj4(), None, "{form}");
        // Written as PROJJSON with EPSG's method and its offsets as given, as PROJ 9.9.0 writes
        // EPSG:2046's.
        let json: serde_json::Value = serde_json::from_str(&crs.to_projjson().unwrap()).unwrap();
        let conversion = &json["conversion"];
        assert_eq!(conversion["method"]["id"]["code"], 9808, "{form}: {conversion}");
        let param =
            |code: u64| conversion["parameters"].as_array().unwrap().iter().find(|v| v["id"]["code"] == code).unwrap()["value"].as_f64();
        assert_eq!((param(8806), param(8807)), (Some(fe), Some(fn_)), "{form}: {conversion}");
        assert!(SpatialRef::from_wkt(&build(0.0, 0.0)).to_proj4().is_some_and(|p| p.contains("+axis=wsu")), "{form}");
        // Written as WKT 1 and as GeoKeys, it reads back in place. A WKT 2 is written as WKT 1
        // through its PROJ string, which it has none of: no strict WKT 1 rather than a wrong one.
        let back = |c: &SpatialRef| Transformer::new(&wgs84, c).unwrap().transform(18.5, -33.9).unwrap();
        let want = back(&crs);
        match crs.to_wkt1_strict() {
            Some(w) => {
                let got = back(&SpatialRef::from_wkt(&w));
                assert!((got.0 - want.0).abs() < 1e-3 && (got.1 - want.1).abs() < 1e-3, "{form}, WKT 1: {got:?}, {want:?}");
            }
            None => assert_eq!(form, "WKT 2"),
        }
        let keys = crs.to_geokeys().unwrap().encode().unwrap();
        let read = SpatialRef::from_geokeys(&keys.directory, &keys.doubles, keys.ascii.as_bytes()).unwrap();
        let got = back(&read);
        assert!((got.0 - want.0).abs() < 1e-3 && (got.1 - want.1).abs() < 1e-3, "{form}, GeoKeys: {got:?}, {want:?}");
        let read = SpatialRef::from_projjson(&crs.to_projjson().unwrap()).unwrap();
        let got = back(&read);
        assert!((got.0 - want.0).abs() < 1e-3 && (got.1 - want.1).abs() < 1e-3, "{form}, PROJJSON: {got:?}, {want:?}");
    }
}

/// A WKT 1 with a north-east CS, read south then west (`+axis=swu`), subtracts the offsets alike:
/// (S, W) = (FN - y, FE - x).
#[test]
fn south_orientated_offsets_with_a_north_east_wkt1() {
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let (fe, fn_) = (1000.0, 2000.0);
    let wkt1 = |fe: f64, fn_: f64| {
        format!(
            r#"PROJCS["Lo19",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator_South_Orientated"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",19],PARAMETER["scale_factor",1],PARAMETER["false_easting",{fe}],PARAMETER["false_northing",{fn_}],UNIT["metre",1],AXIS["Northing",NORTH],AXIS["Easting",EAST]]"#
        )
    };
    let plain = Transformer::new(&wgs84, &SpatialRef::from_wkt(&wkt1(0.0, 0.0))).unwrap();
    // Southing first: about 3753 km, then a westing of about 46 km.
    let (s, w) = plain.transform(18.5, -33.9).unwrap();
    assert!((s - 3_752_681.8).abs() < 1.0 && (w - 46_246.7).abs() < 1.0, "({s}, {w})");
    let t = Transformer::new(&wgs84, &SpatialRef::from_wkt(&wkt1(fe, fn_))).unwrap();
    for (lon, lat) in [(19.0, 0.0), (18.5, -33.9), (20.2, -30.1)] {
        let (s, w) = plain.transform(lon, lat).unwrap();
        let got = t.transform(lon, lat).unwrap();
        let want = (fn_ + s, fe + w);
        assert!((got.0 - want.0).abs() < 1e-3 && (got.1 - want.1).abs() < 1e-3, "({lon}, {lat}): {got:?}, {want:?}");
    }
    // Written as WKT 1 (strict or not) and as PROJJSON, from the WKT and from its PROJ string as
    // terrio stores it, it reads back in place: no WKT 1 that places it elsewhere.
    let crs = SpatialRef::from_wkt(&wkt1(fe, fn_));
    let typed =
        SpatialRef::from_proj4(&format!("+proj=tmerc +axis=swu +lat_0=0 +lon_0=19 +k=1 +x_0={} +y_0={} +datum=WGS84 +units=m", -fe, -fn_))
            .unwrap();
    let want = t.transform(18.5, -33.9).unwrap();
    for (source, c) in [("WKT", &crs), ("PROJ string", &typed)] {
        let placed = |c: &SpatialRef| Transformer::new(&wgs84, c).unwrap().transform(18.5, -33.9).unwrap();
        let got = placed(c);
        assert!((got.0 - want.0).abs() < 1e-3 && (got.1 - want.1).abs() < 1e-3, "{source}: {got:?}, {want:?}");
        let written = [("WKT 1", c.to_wkt()), ("strict WKT 1", c.to_wkt1_strict()), ("PROJJSON", c.to_projjson().ok())];
        for (form, w) in written {
            let Some(w) = w else { continue };
            let read = if form == "PROJJSON" { SpatialRef::from_projjson(&w).unwrap() } else { SpatialRef::from_wkt(&w) };
            let got = placed(&read);
            assert!((got.0 - want.0).abs() < 1e-3 && (got.1 - want.1).abs() < 1e-3, "{source}, {form}: {got:?}, {want:?}\n{w}");
        }
    }
}

/// ESRI's South African Lo zones (ESRI:102470 to 102489) are Transverse Mercator at a scale of -1
/// without offsets, which PROJ 9.9.0 reads as south-orientated at a scale of 1 (`+proj=tmerc
/// +axis=wsu +k=1`), in ESRI and GDAL WKT 1: placed as PROJ places them, from their geographic
/// CRS. Another negative scale, or one with offsets, stays refused, as PROJ refuses it.
#[test]
fn a_transverse_mercator_at_a_scale_of_minus_one_is_south_orientated() {
    // PROJ 9.9.0's ESRI WKT 1 of each.
    let cape = r#"PROJCS["Cape_Lo15",GEOGCS["GCS_Cape",DATUM["D_Cape",SPHEROID["Clarke_1880_Arc",6378249.145,293.4663077]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",15.0],PARAMETER["Scale_Factor",-1.0],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;
    let hartebeesthoek = r#"PROJCS["Hartebeesthoek94_Lo33",GEOGCS["GCS_Hartebeesthoek_1994",DATUM["D_Hartebeesthoek_1994",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",33.0],PARAMETER["Scale_Factor",-1.0],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;
    for (code, esri, lon, want) in [
        (102470, cape, 15.5, (-48244.95594939949, 3319959.4317436223)),
        (102489, hartebeesthoek, 33.5, (-48243.44860596966, 3320218.6505056345)),
    ] {
        let gdal = SpatialRef::from_esri(code).unwrap().to_wkt().unwrap();
        let esri = esri.to_string();
        for wkt in [&gdal, &esri] {
            let crs = SpatialRef::from_wkt(wkt);
            let geographic = SpatialRef::from_wkt(&crs.geographic_wkt().unwrap());
            let (x, y) = Transformer::new(&geographic, &crs).unwrap().transform(lon, -30.0).unwrap();
            assert!((x - want.0).abs() < 1e-3 && (y - want.1).abs() < 1e-3, "ESRI:{code}: ({x}, {y}) {wkt}");
        }
        for (scale, fe) in [("-0.9996", "0"), ("-1", "1000")] {
            let other = gdal
                .replace("\"scale_factor\",-1", &format!("\"scale_factor\",{scale}"))
                .replace("\"false_easting\",0", &format!("\"false_easting\",{fe}"))
                .replace(&format!(",AUTHORITY[\"ESRI\",\"{code}\"]"), "");
            assert_ne!(other, gdal);
            let crs = SpatialRef::from_wkt(&other);
            assert!(
                Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).and_then(|t| t.transform(lon, -30.0)).is_err(),
                "{other}"
            );
        }
    }
}

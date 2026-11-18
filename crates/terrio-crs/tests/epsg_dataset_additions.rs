// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! CRSs of the EPSG dataset v13.102 (PROJ 9.9.0's database), which terrio-crs's tables come from.

use terrio_crs::{CrsQuery, SpatialRef, Transformer, list_crs};

/// SIRGAS-Chile 2025 / UTM zone 19S, new in EPSG v13 (opengeos/RustGIS#2335), is read and projects
/// a point as PROJ 9.9.0 does: from its geographic CRS (EPSG:11473), UTM zone 19S
/// (`Transformer.from_crs(g, CRS.from_epsg(11476), always_xy=True)` in pyproj 3.8.0, network
/// off), within 1 mm.
#[test]
fn sirgas_chile_2025_is_read_and_projected_as_proj_does() {
    let utm = SpatialRef::from_epsg(11476).unwrap();
    let t = Transformer::new(&SpatialRef::from_epsg(11473).unwrap(), &utm).unwrap();
    let (x, y) = t.transform(-70.6, -33.45).unwrap();
    assert!((x - 351290.3712547213).abs() < 1e-3 && (y - 6297679.510189325).abs() < 1e-3, "({x}, {y})");
    assert!(list_crs(&CrsQuery { text: Some("EPSG:11476".into()), ..Default::default() }).iter().any(|e| e.code == 11476));
}

/// PROJ 9.9.0's ESRI WKT 1 of EPSG:11476 (`CRS.from_epsg(11476).to_wkt("WKT1_ESRI")`).
const SIRGAS_CHILE_2025_UTM_19S_ESRI: &str = r#"PROJCS["SIRGAS-Chile_2025_UTM_zone_19S",GEOGCS["GCS_SIRGAS-Chile_2025",DATUM["D_SIRGAS-Chile_2025",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",10000000.0],PARAMETER["Central_Meridian",-69.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;

/// EPSG:11476 given as WKT (its ESRI WKT 1, and the WKT terrio writes for it) is placed in WGS 84
/// as PROJ 9.9.0 places it: SIRGAS-Chile 2025's only transformation, "SIRGAS-Chile 2025 to IGS20
/// (1)" (EPSG:11477), is time-specific (epoch 2025.0) and between geocentric CRSs, so without a
/// coordinate epoch PROJ goes to WGS 84 by its ballpark and keeps the point:
/// (351290.3712547213, 6297679.510189325) is (-70.6°, -33.45°) (`Transformer.from_crs(
/// CRS.from_wkt(wkt), "EPSG:4326", always_xy=True)`, pyproj 3.8.0, network off). Within 1e-9°.
/// terrio's own WKT reads back as EPSG:11476.
#[test]
fn sirgas_chile_2025_given_as_wkt_is_placed_as_proj_does() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let own = SpatialRef::from_epsg(11476).unwrap().to_wkt().unwrap();
    assert_eq!(SpatialRef::from_wkt(&own).epsg, Some(11476));
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for wkt in [SIRGAS_CHILE_2025_UTM_19S_ESRI, own.as_str()] {
        let t = Transformer::new(&SpatialRef::from_wkt(wkt), &wgs84).unwrap();
        let (lon, lat) = t.transform(351_290.371_254_721_3, 6_297_679.510_189_325).unwrap();
        assert!((lon - -70.6).abs() < 1e-9 && (lat - -33.45).abs() < 1e-9, "{wkt}: ({lon}, {lat})");
        assert!(t.operation_at(351_290.371_254_721_3, 6_297_679.510_189_325, 0.0).unwrap().unwrap().ballpark, "{wkt}");
    }
}

/// WGS 84 / UTM grid system (EPSG:32600 and 32700), on the Transverse Mercator Zoned Grid System
/// (EPSG method 9824, PROJ 9.9.0's `tmerczoned`), projects as PROJ 9.9.0 does: the zone of each
/// point, from the initial longitude (180°W) in 6° zones, with its number in the millions of the
/// easting. `cs2cs -d 9 EPSG:4326 EPSG:32600` (and EPSG:32700), PROJ 9.9.0, network off; within
/// 1 mm. The zone's western meridian is in it (6°E is in zone 32), 180°E is 180°W, in zone 1; the
/// eastings of zone 61 (`cs2cs -d 12 EPSG:32600 EPSG:4326` of (61500000, 0)) come back in zone 1.
#[test]
fn the_utm_grid_systems_project_as_proj_does() {
    // (lon, lat) and the northern grid system's (E, N); the southern's is N + 10000 km.
    const POINTS: [((f64, f64), (f64, f64)); 9] = [
        ((7.5, 47.0), (32385962.527261548, 5206255.957226677)),
        ((-180.0, 10.0), (1171071.263941312, 1106908.854243142)),
        ((179.999, 60.0), (60667239.081671305, 6655202.95423713)),
        ((6.0, 0.0), (32166021.44308054, 0.0)),
        ((-1e-06, 51.5), (30708216.80278462, 5709696.980727239)),
        ((180.0, 0.0), (1166021.44308054, 0.0)),
        ((-100.0, 84.0), (14488330.479239818, 9328195.111100344)),
        ((-70.6, -33.45), (19351290.37125547, -3702320.489913396)),
        ((120.0, -80.0), (51441867.784867205, -8883084.955948303)),
    ];
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for (code, northing) in [(32600, 0.0), (32700, 10_000_000.0)] {
        let grid = SpatialRef::from_epsg(code).unwrap();
        let fwd = Transformer::new(&wgs84, &grid).unwrap();
        for ((lon, lat), (e, n)) in POINTS {
            let (x, y) = fwd.transform(lon, lat).unwrap();
            assert!((x - e).abs() < 1e-3 && (y - (n + northing)).abs() < 1e-3, "EPSG:{code} ({lon}, {lat}): ({x}, {y})");
        }
        let listed = list_crs(&CrsQuery { text: Some(format!("EPSG:{code}")), ..Default::default() });
        assert!(listed.iter().any(|e| e.authority == "EPSG" && e.code == code), "EPSG:{code}");
    }
    let inv = Transformer::new(&SpatialRef::from_epsg(32600).unwrap(), &wgs84).unwrap();
    for ((e, n), (lon, lat)) in [
        ((32500000.0, 5207247.6), (9.0, 47.01874880248)),
        ((61500000.0, 0.0), (-177.0, 0.0)),
        ((500000.0, 1107000.0), (177.0, 10.014358334347)),
    ] {
        let (x, y) = inv.transform(e, n).unwrap();
        assert!((x - lon).abs() < 1e-9 && (y - lat).abs() < 1e-9, "({e}, {n}): ({x}, {y})");
    }
}

/// PROJ 9.9.0's WKT 2 of a grid system of 3° zones from Greenwich, in US survey feet
/// (`projinfo -o WKT2:2019` of the PROJ string below).
const ZONED_FEET_WKT2: &str = r#"PROJCRS["unknown",BASEGEOGCRS["unknown",DATUM["Unknown based on GRS 1980 ellipsoid",ELLIPSOID["GRS 1980",6378137,298.257222101,LENGTHUNIT["metre",1],ID["EPSG",7019]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8901]]],CONVERSION["unknown",METHOD["Transverse Mercator Zoned Grid System",ID["EPSG",9824]],PARAMETER["Latitude of natural origin",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Initial longitude",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8830]],PARAMETER["Zone width",3,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8831]],PARAMETER["Scale factor at natural origin",1,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",328083.333333333,LENGTHUNIT["US survey foot",0.304800609601219],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["US survey foot",0.304800609601219],ID["EPSG",8807]]],CS[Cartesian,2],AXIS["(E)",east,ORDER[1],LENGTHUNIT["US survey foot",0.304800609601219,ID["EPSG",9003]]],AXIS["(N)",north,ORDER[2],LENGTHUNIT["US survey foot",0.304800609601219,ID["EPSG",9003]]]]"#;

/// Another Transverse Mercator Zoned Grid System, given by its PROJ string and by PROJ's WKT 2 of
/// it: its zones' easting offsets are in metres whatever the unit, as PROJ 9.9.0 computes them
/// (`cs2cs -d 9 +proj=longlat +ellps=GRS80 +to <string>`, and `-I -d 12` for the inverse, network
/// off). Within 1 mm and 1e-9°.
#[test]
fn a_zoned_grid_system_in_feet_projects_as_proj_does() {
    let string = "+proj=tmerczoned +lon_0=0 +width=3 +k_0=1 +x_0=100000 +y_0=0 +ellps=GRS80 +units=us-ft +type=crs";
    let geographic = SpatialRef::from_proj4("+proj=longlat +ellps=GRS80").unwrap();
    for grid in [SpatialRef::from_proj4(string).unwrap(), SpatialRef::from_wkt(ZONED_FEET_WKT2)] {
        let fwd = Transformer::new(&geographic, &grid).unwrap();
        for ((lon, lat), (e, n)) in [
            ((7.5, 47.0), (10170583.333333334, 17084109.561484117)),
            ((-1.5, -20.0), (394028083.33333343, -7258404.952002865)),
            ((0.0, 0.0), (3061022.610337052, 0.0)),
        ] {
            let (x, y) = fwd.transform(lon, lat).unwrap();
            assert!((x - e).abs() < 1e-3 && (y - n).abs() < 1e-3, "({lon}, {lat}): ({x}, {y})");
        }
        let (lon, lat) = Transformer::new(&grid, &geographic).unwrap().transform(3_100_000.0, 5_000_000.0).unwrap();
        assert!((lon - 0.065686575724).abs() < 1e-9 && (lat - 13.775800902269).abs() < 1e-9, "({lon}, {lat})");
    }
}

/// `+width` is an angle (`pj_param`'s `d`), read whole in a PROJ string, by its readable start in a
/// WKT's PROJ4 extension, as PROJ 9.9.0 reads it (pyproj 3.8.0, network off: `+width=6x` refused
/// in a PROJ string, (2000000.0000, 4984944.3780) for 9°E 45°N in the extension, as with 6).
#[test]
fn a_zone_width_is_read_as_proj_reads_an_angle() {
    assert!(SpatialRef::from_proj4("+proj=tmerczoned +ellps=WGS84 +width=6x").is_err());
    let wkt = r#"PROJCS["x",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],UNIT["metre",1],EXTENSION["PROJ4","+proj=tmerczoned +ellps=WGS84 +width=6x +units=m +no_defs"]]"#;
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let (x, y) = Transformer::new(&wgs84, &SpatialRef::from_wkt(wkt)).unwrap().transform(9.0, 45.0).unwrap();
    assert!((x - 2_000_000.0).abs() < 1e-3 && (y - 4_984_944.377_977_743).abs() < 1e-3, "({x}, {y})");
    let dms = SpatialRef::from_proj4("+proj=tmerczoned +ellps=WGS84 +width=6d00'00\"").unwrap();
    let (x, _) = Transformer::new(&wgs84, &dms).unwrap().transform(9.0, 45.0).unwrap();
    assert!((x - 2_000_000.0).abs() < 1e-3, "{x}");
}

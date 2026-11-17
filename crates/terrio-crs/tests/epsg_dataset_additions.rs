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

/// WGS 84 / UTM grid system (EPSG:32600 and 32700), on the Transverse Mercator Zoned Grid System,
/// which terrio does not compute yet (PROJ 9.9.0's `tmerczoned`), is refused by its code and not
/// listed.
#[test]
fn the_utm_grid_systems_are_refused_until_computed() {
    for code in [32600, 32700] {
        let err = SpatialRef::from_epsg(code).unwrap_err().to_string();
        assert!(err.contains("Transverse Mercator Zoned Grid System"), "{err}");
        let listed = list_crs(&CrsQuery { include_deprecated: true, ..Default::default() });
        assert!(!listed.iter().any(|e| e.authority == "EPSG" && e.code == code), "EPSG:{code}");
    }
}

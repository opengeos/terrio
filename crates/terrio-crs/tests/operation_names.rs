// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! `Transformer::operation_at` names a concatenation PROJ's database registers under a name of its
//! own by that name, as PROJ 9.9.0 reports it (`get_last_used_operation().description`, network
//! off, no grids), not by its steps' (#1012 item 54): NTF (Paris) into WGS 84 is "NTF (Paris) to
//! WGS 84 (1)", not "NTF (Paris) to NTF (1) + NTF to WGS 84 (1)", at the centre of its area.

use terrio_crs::{SpatialRef, Transformer};

#[test]
fn registered_concatenations_are_named_as_proj_names_them() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    // In each CRS's own prime meridian and unit.
    for (code, x, y, name) in [
        (4807, 0.047523144444447246, 51.361111111111164, "NTF (Paris) to WGS 84 (1)"),
        (4120, 24.630000000000003, 38.25, "Greek to WGS 84 (1)"),
        (4197, 13.545, 9.395, "Garoua to WGS 84 (1)"),
        (4805, 33.95166666666667, 44.935, "MGI (Ferro) to WGS 84 (1)"),
        (4803, 1.2569061111111122, 39.555, "Lisbon (Lisbon) to WGS 84 (2)"),
        (4818, 34.99166666666667, 49.394999999999996, "S-JTSK (Ferro) to WGS 84 (2)"),
    ] {
        let crs = SpatialRef::from_epsg(code).unwrap();
        let op = Transformer::new(&crs, &wgs84).unwrap().operation_at(x, y, 0.0).unwrap().unwrap();
        assert_eq!(op.name, name, "EPSG:{code}");
        // The other way, its inverse.
        let (lon, lat) = Transformer::new(&crs, &wgs84).unwrap().transform(x, y).unwrap();
        let back = Transformer::new(&wgs84, &crs).unwrap().operation_at(lon, lat, 0.0).unwrap().unwrap();
        assert_eq!(back.name, format!("Inverse of {name}"), "EPSG:{code}");
    }
}

/// A ballpark that is a transformation's only datum operation is named as PROJ 9.9.0 names it,
/// from the geographic CRSs' names, both ways; between CRSs of different prime meridians, with
/// the transformation that alters one of them to the other's (#1012 item 54).
#[test]
fn ballparks_are_named_as_proj_names_them() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    for (src, dst, x, y, name) in [
        (4214, 4326, 116.0, 40.0, "Ballpark geographic offset from Beijing 1954 to WGS 84"),
        (4326, 4214, 116.0, 40.0, "Ballpark geographic offset from WGS 84 to Beijing 1954"),
        (4979, 4214, 116.0, 40.0, "Ballpark geographic offset from WGS 84 to Beijing 1954"),
        (
            4806,
            4326,
            0.0077,
            40.93,
            "Transformation from Monte Mario (Rome) to Monte Mario (Rome) altered to use prime meridian of WGS 84 + Ballpark geographic offset from Monte Mario (Rome) altered to use prime meridian of WGS 84 to WGS 84",
        ),
        (
            4326,
            4806,
            12.46,
            40.93,
            "Ballpark geographic offset from WGS 84 to Monte Mario (Rome) altered to use prime meridian of WGS 84 + Transformation from WGS 84 to Monte Mario (Rome) altered to use prime meridian of WGS 84",
        ),
        (
            4806,
            4807,
            0.0077,
            40.93,
            "Transformation from Monte Mario (Rome) to Monte Mario (Rome) altered to use prime meridian of NTF (Paris) + Ballpark geographic offset from Monte Mario (Rome) altered to use prime meridian of NTF (Paris) to NTF (Paris)",
        ),
        (
            4214,
            4807,
            116.0,
            40.0,
            "Ballpark geographic offset from Beijing 1954 to NTF (Paris) altered to use prime meridian of Beijing 1954 + Transformation from Beijing 1954 to NTF (Paris) altered to use prime meridian of Beijing 1954",
        ),
    ] {
        let t = Transformer::new(&SpatialRef::from_epsg(src).unwrap(), &SpatialRef::from_epsg(dst).unwrap()).unwrap();
        let op = t.operation_at(x, y, 0.0).unwrap().unwrap();
        assert_eq!((op.name.as_str(), op.ballpark, op.accuracy), (name, true, None), "EPSG:{src} to EPSG:{dst}");
    }
    // An unregistered WKT 2 geographic CRS (EPSG:4610's, in grads, without its ID), named from
    // its own `GEOGCRS`, as PROJ 9.9.0 names it.
    let grad = r#"ANGLEUNIT["grad",0.015707963267949]"#;
    let xian = SpatialRef::from_wkt(&format!(
        r#"GEOGCRS["Xian 1980",DATUM["Xian 1980",ELLIPSOID["IAG 1975",6378140,298.257,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,2],AXIS["geodetic latitude (Lat)",north,ORDER[1],{grad}],AXIS["geodetic longitude (Lon)",east,ORDER[2],{grad}]]"#
    ));
    assert_eq!(xian.epsg, None);
    let op = Transformer::new(&xian, &SpatialRef::from_epsg(4326).unwrap()).unwrap().operation_at(135.0, 42.0, 0.0).unwrap().unwrap();
    assert_eq!(op.name, "Ballpark geographic offset from Xian 1980 to WGS 84");
}

/// PROJ's ballpark vertical transformation is named as PROJ 9.9.0 names it, from the vertical
/// CRSs, or to or from a geographic 3D CRS's geographic CRS, after the horizontal operations;
/// into a CRS with a vertical CRS from one without, the operation the other way, inverted
/// (#1012 item 54). Codes are EPSG's, compound CRSs given as horizontal+vertical.
#[test]
fn vertical_ballparks_are_named_as_proj_names_them() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let crs = |s: &str| {
        let codes: Vec<u32> = s.split('+').map(|c| c.parse().unwrap()).collect();
        match codes[..] {
            [h, v] => SpatialRef::compound(h, v).unwrap(),
            _ => SpatialRef::from_epsg(codes[0]).unwrap(),
        }
    };
    for (src, dst, x, y, name) in [
        ("4326+5773", "4326+5703", 116.0, 40.0, "Transformation from EGM96 height to NAVD88 height (ballpark vertical transformation)"),
        ("4326+5703", "4326+5773", 116.0, 40.0, "Transformation from NAVD88 height to EGM96 height (ballpark vertical transformation)"),
        (
            "4214+5773",
            "4326+5703",
            116.0,
            40.0,
            "Transformation from EGM96 height to NAVD88 height (ballpark vertical transformation) + Ballpark geographic offset from Beijing 1954 to WGS 84",
        ),
        (
            "4214+5773",
            "4979",
            116.0,
            40.0,
            "Ballpark geographic offset from Beijing 1954 to WGS 84 + Transformation from EGM96 height to WGS 84 (ballpark vertical transformation, without ellipsoid height to vertical height correction)",
        ),
        (
            "4979",
            "4214+5773",
            116.0,
            40.0,
            "Inverse of Transformation from EGM96 height to WGS 84 (ballpark vertical transformation, without ellipsoid height to vertical height correction) + Inverse of Ballpark geographic offset from Beijing 1954 to WGS 84",
        ),
        (
            "4326+5773",
            "4979",
            116.0,
            40.0,
            "Transformation from EGM96 height to WGS 84 (ballpark vertical transformation, without ellipsoid height to vertical height correction)",
        ),
        (
            "4979",
            "4326+5703",
            116.0,
            40.0,
            "Inverse of Transformation from NAVD88 height to WGS 84 (ballpark vertical transformation, without ellipsoid height to vertical height correction)",
        ),
        (
            "4214+5773",
            "4610+5703",
            116.0,
            40.0,
            "Transformation from EGM96 height to NAVD88 height (ballpark vertical transformation) + Ballpark geographic offset from Beijing 1954 to Xian 1980",
        ),
        ("4326+3855", "4326+5773", 116.0, 40.0, "Transformation from EGM2008 height to EGM96 height (ballpark vertical transformation)"),
        ("4214+5703", "4214+5773", 116.0, 40.0, "Transformation from NAVD88 height to EGM96 height (ballpark vertical transformation)"),
        ("4979", "4214", 116.0, 40.0, "Ballpark geographic offset from WGS 84 to Beijing 1954"),
        ("4326", "4214+5773", 116.0, 40.0, "Inverse of Ballpark geographic offset from Beijing 1954 to WGS 84"),
        ("4214", "4326+5773", 116.0, 40.0, "Inverse of Ballpark geographic offset from WGS 84 to Beijing 1954"),
        ("4326+5773", "4214", 116.0, 40.0, "Ballpark geographic offset from WGS 84 to Beijing 1954"),
        ("4214+5773", "4326", 116.0, 40.0, "Ballpark geographic offset from Beijing 1954 to WGS 84"),
        (
            "4979",
            "4326+5773",
            116.0,
            40.0,
            "Inverse of Transformation from EGM96 height to WGS 84 (ballpark vertical transformation, without ellipsoid height to vertical height correction)",
        ),
        (
            "4937",
            "4214+5773",
            116.0,
            40.0,
            "Inverse of Transformation from EGM96 height to ETRS89 (ballpark vertical transformation, without ellipsoid height to vertical height correction) + Inverse of Ballpark geographic offset from Beijing 1954 to ETRS89",
        ),
        (
            "4214+5773",
            "4937",
            116.0,
            40.0,
            "Ballpark geographic offset from Beijing 1954 to ETRS89 + Transformation from EGM96 height to ETRS89 (ballpark vertical transformation, without ellipsoid height to vertical height correction)",
        ),
        (
            "4979",
            "4230+5773",
            10.0,
            50.0,
            "Inverse of Transformation from EGM96 height to WGS 84 (ballpark vertical transformation, without ellipsoid height to vertical height correction) + Inverse of ED50 to WGS 84 (17)",
        ),
        (
            "4230+5773",
            "4979",
            10.0,
            50.0,
            "ED50 to WGS 84 (17) + Transformation from EGM96 height to WGS 84 (ballpark vertical transformation, without ellipsoid height to vertical height correction)",
        ),
        (
            "4230+5773",
            "4326+5703",
            10.0,
            50.0,
            "Transformation from EGM96 height to NAVD88 height (ballpark vertical transformation) + ED50 to WGS 84 (17)",
        ),
        ("4326", "4230+5773", 10.0, 50.0, "Inverse of ED50 to WGS 84 (17)"),
    ] {
        let t = Transformer::new(&crs(src), &crs(dst)).unwrap_or_else(|e| panic!("{src} to {dst}: {e}"));
        let op = t.operation_at(x, y, 0.0).unwrap().unwrap();
        assert_eq!(op.name, name, "{src} to {dst}");
    }
}

/// A ballpark between CRSs whose names hold ` + ` is named whole: into a compound CRS of an
/// unregistered geographic CRS named `Alpha + Beta`, inverted and quoted as PROJ 9.9.0 quotes it.
#[test]
fn ballparks_keep_crs_names_that_hold_a_plus_whole() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let custom = SpatialRef::from_wkt(
        r#"COMPD_CS["Alpha + Beta + EGM96 height",GEOGCS["Alpha + Beta",DATUM["Alpha",SPHEROID["Alpha",6378300,298.3]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],AXIS["Latitude",NORTH],AXIS["Longitude",EAST]],VERT_CS["EGM96 height",VERT_DATUM["EGM96 geoid",2005,AUTHORITY["EPSG","5171"]],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Gravity-related height",UP],AUTHORITY["EPSG","5773"]]]"#,
    );
    assert_eq!(custom.epsg, None);
    let into = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &custom).unwrap();
    let op = into.operation_at(116.0, 40.0, 0.0).unwrap().unwrap();
    assert_eq!(op.name, "Inverse of 'Ballpark geographic offset from Alpha + Beta to WGS 84'");
    let from = Transformer::new(&custom, &SpatialRef::from_epsg(4979).unwrap()).unwrap();
    let op = from.operation_at(116.0, 40.0, 0.0).unwrap().unwrap();
    let vertical = "Transformation from EGM96 height to WGS 84 (ballpark vertical transformation, without ellipsoid height to vertical height correction)";
    assert_eq!(op.name, format!("Ballpark geographic offset from Alpha + Beta to WGS 84 + {vertical}"));
}

/// A vertical CRS of an empty name is named so, after the horizontal ballpark, as PROJ 9.9.0
/// names it: `Transformation from  to WGS 84 (...)`.
#[test]
fn a_vertical_ballpark_of_an_empty_name_follows_the_horizontal_step() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let compound = SpatialRef::from_wkt(
        r#"COMPD_CS["x",GEOGCS["Beijing 1954",DATUM["Beijing_1954",SPHEROID["Krassowsky 1940",6378245,298.3,AUTHORITY["EPSG","7024"]],AUTHORITY["EPSG","6214"]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],AXIS["Latitude",NORTH],AXIS["Longitude",EAST],AUTHORITY["EPSG","4214"]],VERT_CS["",VERT_DATUM["",2005],UNIT["metre",1],AXIS["Gravity-related height",UP]]]"#,
    );
    let t = Transformer::new(&compound, &SpatialRef::from_epsg(4979).unwrap()).unwrap();
    let op = t.operation_at(116.0, 40.0, 0.0).unwrap().unwrap();
    assert_eq!(
        op.name,
        "Ballpark geographic offset from Beijing 1954 to WGS 84 + Transformation from  to WGS 84 (ballpark vertical transformation, without ellipsoid height to vertical height correction)"
    );
}

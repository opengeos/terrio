// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! A Web Mercator WKT with a `TOWGS84` of its own is placed as EPSG:3857, whose
//! `+nadgrids=@null` wins over the shift, as PROJ 9.9.0 places it (opengeos/terrio#1012, item 63).

use terrio_crs::{SpatialRef, Transformer};

/// ESRI's WGS_1984_Web_Mercator_Auxiliary_Sphere with `TOWGS84[0,0,0,0,0,0,0]` in its datum.
const ESRI: &str = r#"PROJCS["WGS_1984_Web_Mercator_Auxiliary_Sphere",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563],TOWGS84[0,0,0,0,0,0,0]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Mercator_Auxiliary_Sphere"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",0.0],PARAMETER["Standard_Parallel_1",0.0],PARAMETER["Auxiliary_Sphere_Type",0.0],UNIT["Meter",1.0]]"#;

/// PROJ 9.9.0 (pyproj 3.8.0, network off) identifies ESRI's Web Mercator with a zero or a 10 m
/// `TOWGS84`, and GDAL's WKT 1 of EPSG:3857 with one added, as EPSG:3857 and places (0°, 45°) and
/// (10°, 60°) from WGS 84 at (0, 5621521.486192066) and (1113194.9079327357, 8399737.889818357)
/// (`Transformer.from_crs("EPSG:4326", CRS.from_wkt(wkt), always_xy=True)`), as EPSG:3857 itself.
/// terrio applied the shift through geocentric coordinates on the projection's sphere: 30.2 km and
/// 37.1 km north. Within 1 mm, and back within 1e-9°, both with EPSG:4326, whose Web Mercator
/// shortcut skips datum routing, and with `+proj=longlat +datum=WGS84`, which goes through it.
#[test]
fn a_web_mercator_shift_is_not_applied() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let geographic = [SpatialRef::from_epsg(4326).unwrap(), SpatialRef::from_proj4("+proj=longlat +datum=WGS84").unwrap()];
    let gdal = terrio_crs::epsg_wkt(3857).unwrap().replacen(
        r#"AUTHORITY["EPSG","7030"]]"#,
        r#"AUTHORITY["EPSG","7030"]],TOWGS84[10,0,0,0,0,0,0]"#,
        1,
    );
    assert!(gdal.contains("TOWGS84"));
    for wkt in [ESRI.to_string(), ESRI.replace("TOWGS84[0,0,0", "TOWGS84[10,0,0"), gdal] {
        let crs = SpatialRef::from_wkt(&wkt);
        assert_eq!(crs.epsg, Some(3857), "{wkt}");
        for wgs84 in &geographic {
            let forward = Transformer::new(wgs84, &crs).unwrap();
            let inverse = Transformer::new(&crs, wgs84).unwrap();
            for ((lon, lat), (x, y)) in
                [((0.0, 45.0), (0.0, 5_621_521.486_192_066)), ((10.0, 60.0), (1_113_194.907_932_735_7, 8_399_737.889_818_357))]
            {
                let (a, b) = forward.transform(lon, lat).unwrap();
                assert!((a - x).abs() < 1e-3 && (b - y).abs() < 1e-3, "{wkt}: ({a}, {b}), PROJ ({x}, {y})");
                let (a, b) = inverse.transform(x, y).unwrap();
                assert!((a - lon).abs() < 1e-9 && (b - lat).abs() < 1e-9, "{wkt}: ({a}, {b}), ({lon}, {lat})");
            }
        }
    }
}

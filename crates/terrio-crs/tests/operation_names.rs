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

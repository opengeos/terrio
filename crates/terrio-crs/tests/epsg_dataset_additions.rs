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

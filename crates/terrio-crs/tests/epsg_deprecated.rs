// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! A deprecated EPSG code keeps its own definition, as PROJ 9.8.1 gives it (`CRS.from_epsg`), not
//! its replacement's, which GDAL substitutes by default (`OSR_USE_NON_DEPRECATED=YES`): EPSG:4855,
//! the deprecated ETRS89 / NTM zone 5, has a false northing of 1,000,000 m and a latitude of origin
//! of 0, where its replacement EPSG:5105 has 0 and 58°N, 6,431 km apart.
//!
//! `data/epsg_deprecated.json` (`scripts/gen_epsg_deprecated.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`,
//! no grids) holds every deprecated EPSG geographic, geocentric and projected CRS, the codes PROJ
//! lists as its replacements (`CRS.get_non_deprecated()`), the centre of its area of use and PROJ's
//! coordinates of it in the CRS from WGS 84 (`Transformer.from_crs("EPSG:4326", code,
//! always_xy=True)`), `null` where PROJ cannot transform it. Tolerance: 1 mm (1e-8 degrees for
//! geographic longitudes and latitudes).

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(false));
}

fn cases() -> Vec<Value> {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/epsg_deprecated.json")).unwrap();
    let data: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(data["proj_version"], "9.8.1");
    data["cases"].as_array().unwrap().clone()
}

fn codes(v: &Value) -> Vec<u32> {
    v.as_array().unwrap().iter().map(|c| c.as_u64().unwrap() as u32).collect()
}

/// Deprecated geographic 3D and geocentric CRSs PROJ transforms by other operations than terrio
/// (NAD83(CSRS) 3D and geocentric, RGNC 1991 geocentric): left for the geocentric CRSs of
/// opengeos/terrio#1012, item 8.
const PENDING: &[u32] = &[4359, 4360, 4382, 4968];

/// Every deprecated code PROJ transforms is placed as PROJ places it, by its own definition.
#[test]
fn deprecated_codes_are_placed_by_their_own_definition_as_proj_does() {
    offline();
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let cases = cases();
    assert_eq!(cases.len(), 498);
    let mut bad = Vec::new();
    let mut compared = 0;
    for case in &cases {
        let code = case["code"].as_u64().unwrap() as u32;
        let Some(proj) = case["proj"].as_array() else { continue };
        compared += 1;
        let (lon, lat) = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
        let proj: Vec<f64> = proj.iter().map(|v| v.as_f64().unwrap()).collect();
        let crs = match SpatialRef::from_epsg(code) {
            Ok(crs) => crs,
            Err(e) => {
                bad.push(format!("EPSG:{code}: refused ({e}), PROJ {proj:?}"));
                continue;
            }
        };
        assert!(crs.is_deprecated(), "EPSG:{code}");
        let got = Transformer::new(&wgs84, &crs).and_then(|t| t.transform_3d(lon, lat, 0.0));
        let Ok((x, y, z)) = got else {
            bad.push(format!("EPSG:{code}: {got:?}, PROJ {proj:?}"));
            continue;
        };
        // Degrees for a geographic CRS's horizontal coordinates, metres otherwise.
        let horizontal = if crs.is_geographic() { 1e-8 } else { 1e-3 };
        let off = [(x - proj[0], horizontal), (y - proj[1], horizontal), (z - proj[2], 1e-3)];
        if off.iter().any(|(d, tolerance)| !d.is_finite() || d.abs() >= *tolerance) {
            bad.push(format!("EPSG:{code}: ({x}, {y}, {z}), PROJ {proj:?}"));
        }
    }
    assert_eq!(compared, 490);
    // The pending codes still differ: remove one from `PENDING` once it matches.
    let matching: Vec<u32> = PENDING.iter().copied().filter(|c| !bad.iter().any(|b| b.starts_with(&format!("EPSG:{c}:")))).collect();
    assert!(matching.is_empty(), "pending codes that now match PROJ: {matching:?}");
    bad.retain(|b| !PENDING.iter().any(|c| b.starts_with(&format!("EPSG:{c}:"))));
    assert!(bad.is_empty(), "{} of {compared} differ:\n{}", bad.len(), bad.join("\n"));
}

/// The deprecated codes PROJ cannot transform: terrio refuses those PROJ cannot write as a PROJ
/// string (the four Qornoq 1927 Greenland zones, EPSG:3752 and EPSG:7082), and transforms the RRAF 1991
/// geocentric CRSs (4384, 4972) by their datum's ballpark, where PROJ finds no operation.
#[test]
fn codes_proj_cannot_transform() {
    offline();
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let (mut refused, mut transformed) = (Vec::new(), Vec::new());
    for case in cases().iter().filter(|c| c["proj"].is_null()) {
        let code = case["code"].as_u64().unwrap() as u32;
        let (lon, lat) = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
        match SpatialRef::from_epsg(code) {
            Err(_) => refused.push(code),
            Ok(crs) => {
                let (x, y, z) = Transformer::new(&wgs84, &crs).and_then(|t| t.transform_3d(lon, lat, 0.0)).unwrap();
                assert!(crs.is_geocentric() && x.is_finite() && y.is_finite() && z.is_finite(), "EPSG:{code}");
                transformed.push(code);
            }
        }
    }
    assert_eq!(refused, [2297, 2298, 2300, 2302, 3752, 7082]);
    assert_eq!(transformed, [4384, 4972]);
}

/// The replacements of each deprecated code are PROJ's, and the CRS GDAL gives by default is
/// that of a single replacement.
#[test]
fn replacements_are_those_proj_lists() {
    for case in cases() {
        let code = case["code"].as_u64().unwrap() as u32;
        let replaced_by = codes(&case["replaced_by"]);
        // A code terrio does not carry, as PROJ can neither write it as a PROJ string nor
        // transform it (EPSG:3752, Mercator (variant A) with a latitude of origin), still has
        // its replacements.
        if SpatialRef::from_epsg(code).is_err() {
            assert!(case["proj"].is_null(), "EPSG:{code}");
        }
        assert_eq!(terrio_crs::epsg_replacements(code), &replaced_by[..], "EPSG:{code}");
        // Where the single replacement resolves, GDAL's default gives it.
        if let [single] = replaced_by[..]
            && SpatialRef::from_epsg(single).is_ok()
        {
            assert_eq!(SpatialRef::from_epsg_non_deprecated(code).unwrap().epsg, Some(single), "EPSG:{code}");
        }
    }
    // GDAL substitutes only a single replacement: a code with several (EPSG:2191, replaced by
    // 2942 and 2943) or none (EPSG:2008) keeps its own definition.
    assert!(SpatialRef::from_epsg(3752).is_err());
    assert_eq!(SpatialRef::from_epsg_non_deprecated(3752).unwrap().epsg, Some(3994));
    assert_eq!(terrio_crs::epsg_replacements(2191), &[2942, 2943]);
    assert_eq!(SpatialRef::from_epsg_non_deprecated(2191).unwrap().epsg, Some(2191));
    assert!(terrio_crs::epsg_replacements(2008).is_empty());
    assert!(SpatialRef::from_epsg(2008).unwrap().is_deprecated());
    assert_eq!(SpatialRef::from_epsg_non_deprecated(2008).unwrap().epsg, Some(2008));
    assert!(!SpatialRef::from_epsg(5105).unwrap().is_deprecated());
    assert!(terrio_crs::epsg_replacements(5105).is_empty());
    assert_eq!(SpatialRef::from_epsg_non_deprecated(5105).unwrap().epsg, Some(5105));
}

/// RustGIS's examples (opengeos/terrio#1012): the deprecated NTM zone 5 keeps its own false
/// origin, and the GDAL default gives its replacement's.
#[test]
fn ntm_zone_5_keeps_its_own_false_origin() {
    offline();
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let own = SpatialRef::from_epsg(4855).unwrap();
    let (x, y) = Transformer::new(&wgs84, &own).unwrap().transform(5.2, 60.48).unwrap();
    // PROJ 9.8.1: (83503.1262, 7707590.2512).
    assert!((x - 83_503.126_2).abs() < 1e-3 && (y - 7_707_590.251_2).abs() < 1e-3, "({x}, {y})");
    let gdal = SpatialRef::from_epsg_non_deprecated(4855).unwrap();
    assert_eq!(gdal.epsg, Some(5105));
    let (_, y) = Transformer::new(&wgs84, &gdal).unwrap().transform(5.2, 60.48).unwrap();
    assert!((y - 1_276_307.579_3).abs() < 1e-3, "{y}");
}

/// A deprecated code GDAL 3.12 substitutes by default (a single replacement) is written to
/// GeoKeys as a user-defined CRS, not by its code, which GDAL would read as the replacement
/// (EPSG:4855 as 5105, 6,431 km away); its keys read back as its own definition. A code GDAL
/// keeps (EPSG:2191, two replacements) is written by its code.
#[test]
fn geokeys_of_deprecated_codes_read_as_their_own_definition() {
    offline();
    let coded = |keys: &terrio_crs::geokeys::GeoKeyValues, code: u32| {
        keys.shorts.iter().any(|(k, v)| matches!(k, 2048 | 3072) && u32::from(*v) == code)
    };
    // No geographic or projected code GDAL would substitute, the CRS's own or its base's.
    let substituted = |keys: &terrio_crs::geokeys::GeoKeyValues| {
        keys.shorts.iter().find(|(k, v)| matches!(k, 2048 | 3072) && terrio_crs::epsg_replacements(u32::from(*v)).len() == 1).copied()
    };
    let mut written = 0;
    for case in cases() {
        let code = case["code"].as_u64().unwrap() as u32;
        let Ok(crs) = SpatialRef::from_epsg(code) else { continue };
        let Ok(keys) = crs.to_geokeys() else { continue };
        written += 1;
        assert_eq!(substituted(&keys), None, "EPSG:{code}: {keys:?}");
    }
    assert!(written > 400, "{written}");
    assert!(coded(&SpatialRef::from_epsg(2191).unwrap().to_geokeys().unwrap(), 2191));
    let own = SpatialRef::from_epsg(4855).unwrap();
    let keys = own.to_geokeys().unwrap();
    assert!(keys.shorts.contains(&(3072, 32767)), "{keys:?}");
    let tags = keys.encode().unwrap();
    let back = SpatialRef::from_geokeys(&tags.directory, &tags.doubles, tags.ascii.as_bytes()).unwrap();
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let (x, y) = Transformer::new(&wgs84, &back).unwrap().transform(5.2, 60.48).unwrap();
    // PROJ 9.8.1: (83503.1262, 7707590.2512), as in `ntm_zone_5_keeps_its_own_false_origin`.
    assert!((x - 83_503.126_2).abs() < 1e-3 && (y - 7_707_590.251_2).abs() < 1e-3, "({x}, {y})");
}

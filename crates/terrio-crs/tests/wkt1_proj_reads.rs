//! WKT that PROJ 9.8.1 reads is read, and places points where PROJ does: ESRI's
//! `Mercator_Variant_A` (EPSG Guidance Note 7-2's Makassar / NEIEZ example, EPSG:3002, as an
//! ESRI `.prj` names its method) and EPSG's `Mercator (variant A)` in WKT 1, as PROJ's Mercator
//! (1SP), and WKT 1 and WKT 2 with a node of an unknown keyword (`rustgis:unapplied`, `a-b`,
//! `a"b c"`, an empty one), which PROJ skips whatever its keyword. A keyword with a space, a
//! vertical tab, a NUL or an unclosed quote in it is refused, as PROJ refuses it, and so is
//! EPSG's `Mercator (variant A)` with a latitude of origin other than 0, which PROJ reads but
//! neither transforms nor writes as a PROJ string. A definition ends at its first NUL, as PROJ
//! takes it as a C string, so a NUL in a keyword is refused; text after a complete WKT, NUL or
//! not, was already ignored. A NUL a PROJJSON name gives (`\u0000`) is part of the name, as in
//! PROJ: EPSG:32631's PROJJSON with one is still read.
//!
//! `data/wkt1_proj_reads.json` (`scripts/gen_wkt1_proj_reads.py`, pyproj 3.8.0, no grids,
//! `PROJ_NETWORK=OFF`) holds, for each WKT, a point in its geographic CRS, the point in the
//! WKT's CRS by `Transformer.from_crs(crs.geodetic_crs, crs, always_xy=True)`, and
//! `CRS.from_wkt(wkt).to_proj4()` and `CRS.to_epsg()`; Makassar / NEIEZ in WGS 84 by
//! `Transformer.from_crs(crs, "EPSG:4326", always_xy=True)` and its `CRS.to_epsg()`; the WKT
//! `CRS.from_wkt` refuses; and the WKT `Transformer.from_crs(crs.geodetic_crs, crs)` and
//! `CRS.to_proj4()` refuse; and EPSG:32631's PROJJSON with a `\u0000` in a name, with its
//! `CRS.to_epsg()` and a point by `Transformer.from_crs("EPSG:4326", crs, always_xy=True)`.
//! Tolerance: 1 mm (in the CRS's unit for projected coordinates, 1e-9 degrees for geographic ones,
//! 1e-8 degrees in WGS 84); PROJ strings have PROJ's parameters, their values within a relative
//! 1e-12.

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

fn data() -> Value {
    serde_json::from_str(include_str!("data/wkt1_proj_reads.json")).unwrap()
}

#[test]
fn wkt_proj_reads_is_read_and_places_points_as_proj_does() {
    offline();
    let data = data();
    let cases = data["read"].as_array().unwrap();
    assert_eq!(cases.len(), 24);
    let mut bad = vec![];
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let wkt = case["wkt"].as_str().unwrap();
        let num = |v: &Value, i: usize| v[i].as_f64().unwrap();
        if terrio_crs::wkt_proj(wkt).is_none() {
            bad.push(format!("{name}: wkt_proj reads nothing"));
            continue;
        }
        let crs = SpatialRef::from_wkt(wkt);
        let Some(geog) = crs.geographic_wkt().map(|w| SpatialRef::from_wkt(&w)) else {
            bad.push(format!("{name}: no geographic CRS"));
            continue;
        };
        let got = Transformer::new(&geog, &crs).and_then(|t| t.transform(num(&case["point"], 0), num(&case["point"], 1)));
        let proj = (num(&case["proj"], 0), num(&case["proj"], 1));
        let tolerance = if crs.is_geographic() { 1e-9 } else { 1e-3 };
        match got {
            Ok(got) if (got.0 - proj.0).abs() < tolerance && (got.1 - proj.1).abs() < tolerance => {}
            got => bad.push(format!("{name}: {got:?}, PROJ {proj:?}")),
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Whether two PROJ strings have the same parameters, their values within a relative 1e-12:
/// PROJ writes the false easting of the US survey feet case as 1188722.37744476, terrio as
/// 1188722.37744475 (3,900,000 m is 1,188,722.377444755 US survey feet).
fn same_proj(a: &str, b: &str) -> bool {
    let (a, b): (Vec<&str>, Vec<&str>) = (a.split_whitespace().collect(), b.split_whitespace().collect());
    a.len() == b.len()
        && a.iter().zip(&b).all(|(a, b)| {
            let (Some((ka, va)), Some((kb, vb))) = (a.split_once('='), b.split_once('=')) else { return a == b };
            ka == kb
                && match (va.parse::<f64>(), vb.parse::<f64>()) {
                    (Ok(x), Ok(y)) => (x - y).abs() <= 1e-12 * x.abs().max(y.abs()),
                    _ => va == vb,
                }
        })
}

/// The PROJ string is PROJ's.
#[test]
fn wkt_proj_reads_exports_proj_strings_as_proj_does() {
    offline();
    let data = data();
    let mut bad = vec![];
    for case in data["read"].as_array().unwrap() {
        let got = SpatialRef::from_wkt(case["wkt"].as_str().unwrap()).to_proj4();
        if !got.as_deref().is_some_and(|got| same_proj(got, case["proj4"].as_str().unwrap())) {
            bad.push(format!("{}: {got:?}, PROJ {}", case["name"], case["proj4"]));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

#[test]
fn a_keyword_proj_refuses_is_refused() {
    offline();
    let data = data();
    let cases = data["refused"].as_array().unwrap();
    assert_eq!(cases.len(), 4);
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for case in cases {
        let wkt = case["wkt"].as_str().unwrap();
        assert_eq!(terrio_crs::wkt_proj(wkt), None, "{}", case["name"]);
        assert!(SpatialRef::parse(wkt).is_err(), "{}", case["name"]);
        assert!(Transformer::new(&SpatialRef::from_wkt(wkt), &wgs84).is_err(), "{}", case["name"]);
    }
}

/// Makassar / NEIEZ given by ESRI's `.prj` is EPSG:3002, and goes to WGS 84 by Makassar's
/// transformation, as in PROJ.
#[test]
fn makassar_neiez_goes_to_wgs84_as_in_proj() {
    offline();
    let data = data();
    let case = &data["to_wgs84"];
    let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
    assert_eq!(crs.epsg.map(u64::from), case["epsg"].as_u64());
    let num = |v: &Value, i: usize| v[i].as_f64().unwrap();
    let t = Transformer::new(&crs, &SpatialRef::from_epsg(4326).unwrap()).unwrap();
    let got = t.transform(num(&case["point"], 0), num(&case["point"], 1)).unwrap();
    let proj = (num(&case["proj"], 0), num(&case["proj"], 1));
    // 1e-8 degrees is about a millimetre.
    assert!((got.0 - proj.0).abs() < 1e-8 && (got.1 - proj.1).abs() < 1e-8, "{got:?}, PROJ {proj:?}");
}

/// The EPSG code PROJ identifies is terrio's (NAD27 / UTM zone 17N with a node of a hyphenated
/// keyword is EPSG:26717). A CRS PROJ identifies as none may still be identified by terrio.
#[test]
fn wkt_proj_reads_is_identified_as_proj_identifies_it() {
    offline();
    let data = data();
    let mut bad = vec![];
    for case in data["read"].as_array().unwrap().iter().filter(|c| !c["epsg"].is_null()) {
        let got = SpatialRef::from_wkt(case["wkt"].as_str().unwrap()).epsg;
        if got.map(u64::from) != case["epsg"].as_u64() {
            bad.push(format!("{}: {got:?}, PROJ {}", case["name"], case["epsg"]));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// EPSG's `Mercator (variant A)` with a latitude of origin other than 0 is refused: PROJ reads
/// it, but neither transforms it ("Input is not a transformation") nor writes it as a PROJ
/// string, as `+proj=merc` has no latitude of origin.
#[test]
fn mercator_variant_a_with_a_latitude_of_origin_is_refused_as_in_proj() {
    offline();
    let data = data();
    let cases = data["untransformed"].as_array().unwrap();
    assert_eq!(cases.len(), 3);
    for case in cases {
        let wkt = case["wkt"].as_str().unwrap();
        let crs = SpatialRef::from_wkt(wkt);
        assert_eq!(terrio_crs::wkt_proj(wkt), None, "{}", case["name"]);
        assert_eq!(crs.to_proj4(), None, "{}", case["name"]);
        let geog = SpatialRef::from_wkt(&crs.geographic_wkt().unwrap());
        assert!(Transformer::new(&geog, &crs).is_err(), "{}", case["name"]);
    }
}

/// A NUL (`\u0000`) in a PROJJSON name is part of the name: EPSG:32631's PROJJSON with one is
/// read as EPSG:32631 and places a point as PROJ does. A raw NUL ends the text, as PROJ 9.8.1's
/// `proj_create` reads it (checked through its C API: text after the NUL is ignored, and a NUL
/// inside the JSON leaves it unclosed and refused).
#[test]
fn a_nul_in_a_projjson_name_is_read_as_in_proj() {
    offline();
    let data = data();
    let case = &data["projjson"];
    let text = case["projjson"].as_str().unwrap();
    assert!(text.contains(r"Latitude of\u0000 natural origin"));
    let num = |v: &Value, i: usize| v[i].as_f64().unwrap();
    let proj = (num(&case["proj"], 0), num(&case["proj"], 1));
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for crs in [SpatialRef::from_projjson(text).unwrap(), SpatialRef::parse_literal(text).unwrap()] {
        assert_eq!(crs.epsg.map(u64::from), case["epsg"].as_u64());
        let got = Transformer::new(&wgs84, &crs).unwrap().transform(num(&case["point"], 0), num(&case["point"], 1)).unwrap();
        assert!((got.0 - proj.0).abs() < 1e-3 && (got.1 - proj.1).abs() < 1e-3, "{got:?}, PROJ {proj:?}");
    }
    let trailing = format!("{text}\0garbage");
    assert_eq!(SpatialRef::from_projjson(&trailing).unwrap().epsg, SpatialRef::from_projjson(text).unwrap().epsg);
    let inside = format!("{}\0{}", &text[..50], &text[50..]);
    assert!(SpatialRef::from_projjson(&inside).is_err());
}

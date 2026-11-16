//! A WKT whose root `AUTHORITY` or `ID` is not the CRS its nodes describe is read by its nodes, as
//! PROJ 9.8.1 reads it: a geographic code on a projected CRS, a code that does not exist, a
//! compound code's units and axes on a 2D CRS in degrees, a geographic code on a geocentric CRS, a
//! code of another datum on the same ellipsoid. The declared code is kept only when the content is
//! that CRS; an unknown method stays unusable rather than read as the code. An Esri CRS's name
//! identifies its code only on its datum (ESRI's `D_` datum names included).
//!
//! `data/wkt_root_code.json` (`scripts/gen_wkt_root_code.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`, no
//! grids installed) holds PROJ's point in each CRS from WGS 84 (`always_xy`), `null` where PROJ
//! refuses, the code PROJ identifies, the code a case declares that is not its CRS, and
//! `"terrio": "refused"` for a case terrio refuses where PROJ reads it. Tolerance: 1 mm (1e-8° for
//! a geographic CRS).

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

fn cases() -> Vec<Value> {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let data: Value = serde_json::from_str(include_str!("data/wkt_root_code.json")).unwrap();
    data["cases"].as_array().unwrap().clone()
}

/// The CRS of a case, and the point PROJ places from WGS 84 in it, by terrio.
fn read(case: &Value) -> (SpatialRef, terrio_crs::Result<(f64, f64)>) {
    let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
    let (lon, lat) = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
    let got = Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).and_then(|t| t.transform(lon, lat));
    (crs, got)
}

#[test]
fn a_wkt_is_read_by_its_nodes_not_its_root_code() {
    let cases = cases();
    assert_eq!(cases.len(), 31);
    for case in cases.iter().filter(|c| c["terrio"].is_null()) {
        let name = case["name"].as_str().unwrap();
        let (crs, got) = read(case);
        match case["proj"].as_array() {
            Some(p) => {
                let (x, y) = got.unwrap_or_else(|e| panic!("{name}: {e}"));
                let tolerance = if crs.is_geographic() { 1e-8 } else { 1e-3 };
                let (px, py) = (p[0].as_f64().unwrap(), p[1].as_f64().unwrap());
                assert!((x - px).abs() < tolerance && (y - py).abs() < tolerance, "{name}: ({x}, {y}), PROJ ({px}, {py})");
            }
            None => {
                assert!(got.is_err(), "{name}: {got:?}");
                assert_eq!(crs.to_proj4(), None, "{name}");
            }
        }
        // The code PROJ identifies, none for an Esri name on another datum, and never a declared
        // code that is not the CRS, nor in the PROJJSON written.
        // Where PROJ identifies none, the code terrio identifies by the content (`terrio_epsg`).
        let want = case["terrio_epsg"].as_u64().or(case["epsg"].as_u64());
        assert_eq!(crs.epsg.map(u64::from), want, "{name}");
        if let Some(code) = case["esri"].as_u64() {
            assert_eq!(crs.esri, Some(code as u32), "{name}");
        }
        if name.contains("renamed") {
            assert_eq!((crs.epsg, crs.esri), (None, None), "{name}");
        }
        if let Some(declared) = case["declared"].as_u64() {
            assert_ne!(crs.epsg, Some(declared as u32), "{name}");
            // The root's `id` (a bound CRS's source's), not an inner CRS's.
            if let Ok(json) = crs.to_projjson() {
                let v: Value = serde_json::from_str(&json).unwrap();
                let root = if v["type"] == "BoundCRS" { &v["source_crs"] } else { &v };
                assert_ne!(root["id"]["code"].as_u64(), Some(declared), "{name}: {json}");
            }
        }
    }
}

/// A method terrio's WKT 2 reader does not know (Belgian Lambert 72, opengeos/terrio#1012 item
/// 43) under a code that is not its CRS is refused, not read as that code; PROJ reads its content.
#[test]
fn an_unread_method_under_another_code_is_refused() {
    let cases = cases();
    let refused: Vec<&Value> = cases.iter().filter(|c| c["terrio"] == "refused").collect();
    assert_eq!(refused.len(), 1);
    for case in refused {
        let name = case["name"].as_str().unwrap();
        let (crs, got) = read(case);
        assert!(got.is_err(), "{name}: {got:?}");
        assert_eq!(crs.to_proj4(), None, "{name}");
        assert_ne!(crs.epsg, case["declared"].as_u64().map(|c| c as u32), "{name}");
    }
}

/// A compound CRS whose vertical CRS declares a code of another unit or direction (EGM96 height,
/// EPSG:5773, in US survey feet or as depths) is not written as that code: PROJJSON refuses it
/// rather than give its heights in metres up.
#[test]
fn a_vertical_crs_of_another_unit_is_not_written_as_its_code() {
    let cases = cases();
    for name in ["WGS 84 + EGM96 height in US survey feet declaring EPSG:9707", "WGS 84 + EGM96 depth declaring EPSG:9707"] {
        let case = cases.iter().find(|c| c["name"] == name).unwrap();
        let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
        assert!(crs.to_projjson().is_err(), "{name}: {:?}", crs.to_projjson());
    }
    let own = cases.iter().find(|c| c["name"] == "WGS 84 + EGM96 height with its own code").unwrap();
    let json = SpatialRef::from_wkt(own["wkt"].as_str().unwrap()).to_projjson().unwrap();
    assert!(json.contains("\"code\":5773"), "{json}");
}

//! A WKT 2 ellipsoid in another unit than the metre (`LENGTHUNIT["Clarke's foot",0.3047972654]`)
//! is read in metres, as PROJ 9.9.0 reads it (opengeos/terrio#1012, item 58): its semi-major
//! axis, the length of a degree of a geographic CRS (`metres_per_unit`), a projected CRS's
//! ellipsoid (`projection_params`), the code identified, with or without the root `ID`, and the
//! point placed from WGS 84. EPSG:3139 without its `ID` is read by its method, Hyperbolic
//! Cassini-Soldner (#1012 item 43).
//!
//! `data/ellipsoid_units.json` (`scripts/gen_ellipsoid_units.py`, pyproj 3.8.0,
//! `PROJ_NETWORK=OFF`, no grids installed) holds every EPSG CRS whose WKT 2 PROJ writes with such
//! an ellipsoid: PROJ's ellipsoid in metres (`semi_major_metre`, `semi_minor_metre`), its
//! `to_epsg()` and its point. Tolerances: 1e-6 m for the axes, 1 mm for a point (1e-8°).

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

fn cases() -> Vec<Value> {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let data: Value = serde_json::from_str(include_str!("data/ellipsoid_units.json")).unwrap();
    data["cases"].as_array().unwrap().clone()
}

fn f(v: &Value) -> f64 {
    v.as_f64().unwrap()
}

#[test]
fn an_ellipsoid_in_feet_is_read_in_metres() {
    let cases = cases();
    assert_eq!(cases.len(), 32);
    let mut failures = Vec::new();
    for case in &cases {
        let code = case["code"].as_u64().unwrap();
        let (a, b) = (f(&case["a"]), f(&case["b"]));
        for key in ["wkt", "bare"] {
            let wkt = case[key].as_str().unwrap();
            let name = format!("EPSG:{code} ({key})");
            let crs = SpatialRef::from_wkt(wkt);
            let want = if key == "wkt" { &case["epsg"] } else { &case["bare_epsg"] };
            if crs.epsg.map(u64::from) != want.as_u64() {
                failures.push(format!("{name}: code {:?}, PROJ {want}", crs.epsg));
            }
            match terrio_crs::semi_major_axis(wkt) {
                Some(got) if (got - a).abs() < 1e-6 => {}
                got => failures.push(format!("{name}: semi-major axis {got:?}, PROJ {a}")),
            }
            if crs.is_geographic() {
                let degree = a * std::f64::consts::PI / 180.0;
                if (crs.metres_per_unit() - degree).abs() > 1e-6 {
                    failures.push(format!("{name}: metres per unit {}, PROJ {degree}", crs.metres_per_unit()));
                }
            } else {
                match crs.projection_params() {
                    Some(p) if (p.a - a).abs() < 1e-6 && (p.b - b).abs() < 1e-6 => {}
                    p => failures.push(format!("{name}: ellipsoid {:?}, PROJ ({a}, {b})", p.map(|p| (p.a, p.b)))),
                }
            }
            let (lon, lat) = (f(&case["point"][0]), f(&case["point"][1]));
            let (px, py) = (f(&case["proj"][0]), f(&case["proj"][1]));
            let tolerance = if crs.is_geographic() { 1e-8 } else { 1e-3 };
            match Transformer::new(&SpatialRef::from_epsg(4326).unwrap(), &crs).and_then(|t| t.transform(lon, lat)) {
                Ok((x, y)) if (x - px).abs() < tolerance && (y - py).abs() < tolerance => {}
                got => failures.push(format!("{name}: point {got:?}, PROJ ({px}, {py})")),
            }
        }
    }
    assert!(failures.is_empty(), "{} failures:\n{}", failures.len(), failures.join("\n"));
}

/// The PROJJSON written of such a WKT has its ellipsoid in metres: PROJ's WKT 2 of EPSG:4157
/// without its root `ID`, on its datum by a `TRF` node too.
#[test]
fn the_projjson_ellipsoid_is_in_metres() {
    let cases = cases();
    let case = cases.iter().find(|c| c["code"] == 4157).unwrap();
    let a = f(&case["a"]);
    let bare = case["bare"].as_str().unwrap().to_string();
    for wkt in [bare.clone(), bare.replacen("DATUM[", "TRF[", 1)] {
        let json: Value = serde_json::from_str(&SpatialRef::from_wkt(&wkt).to_projjson().unwrap()).unwrap();
        let e = &json["datum"]["ellipsoid"];
        assert!((e["semi_major_axis"].as_f64().unwrap() - a).abs() < 1e-6, "{e}");
    }
}

//! Of two datum nodes (`TRF`, `GEODETICDATUM`, `DATUM`), two ellipsoid nodes (`SPHEROID`,
//! `ELLIPSOID`) or two base CRSs (`BASEGEODCRS`, `BASEGEOGCRS`), a WKT 2 CRS is read by the first
//! in its own order, whatever their keywords, as PROJ 9.9.0 reads it (opengeos/terrio#1058 step
//! 1b).
//!
//! `data/keyword_order.json` (`scripts/gen_keyword_order.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`,
//! no grids installed) holds British National Grid on such base CRSs, one node on WGS 84 and one on
//! Airy 1830: the semi-major axis PROJ reads and its point from WGS 84 (`always_xy`). Tolerance:
//! 1 mm.

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

#[test]
fn the_first_node_counts_whatever_its_keyword() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let data: Value = serde_json::from_str(include_str!("data/keyword_order.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 7);
    let (lon, lat) = (data["point"][0].as_f64().unwrap(), data["point"][1].as_f64().unwrap());
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let mut failures = Vec::new();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
        let a = case["a"].as_f64().unwrap();
        let got = crs.projection_params().map(|p| p.a);
        if got.is_none_or(|got| (got - a).abs() > 1e-6) {
            failures.push(format!("{name}: semi-major axis {got:?}, PROJ {a}"));
        }
        let (px, py) = (case["proj"][0].as_f64().unwrap(), case["proj"][1].as_f64().unwrap());
        match Transformer::new(&wgs84, &crs).and_then(|t| t.transform(lon, lat)) {
            Ok((x, y)) if (x - px).abs() < 1e-3 && (y - py).abs() < 1e-3 => {}
            got => failures.push(format!("{name}: {got:?}, PROJ ({px}, {py})")),
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

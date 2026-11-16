//! A WKT whose root `AUTHORITY` or `ID` is not the CRS its nodes describe is read by its nodes, as
//! PROJ 9.8.1 reads it: a geographic code on a projected CRS, a code that does not exist, a
//! compound code's units and axes on a 2D CRS in degrees. The declared code is kept only when the
//! content is that CRS; an unknown method stays unusable rather than read as the code. An Esri
//! CRS's name identifies its code only on its datum (ESRI's `D_` datum names included).
//!
//! `data/wkt_root_code.json` (`scripts/gen_wkt_root_code.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`, no
//! grids installed) holds PROJ's point in each CRS from WGS 84 (`always_xy`), `null` where PROJ
//! refuses, and the code PROJ identifies. Tolerance: 1 mm (1e-8° for a geographic CRS).

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

#[test]
fn a_wkt_is_read_by_its_nodes_not_its_root_code() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let data: Value = serde_json::from_str(include_str!("data/wkt_root_code.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 12);
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
        let (lon, lat) = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
        let got = Transformer::new(&wgs84, &crs).and_then(|t| t.transform(lon, lat));
        // A method terrio's WKT 2 reader does not know (Belgian Lambert 72, opengeos/terrio#1012
        // item 43) under a code that is not its CRS: refused, not read as that code.
        let unread = name.starts_with("Belge 1972") && name.contains("declaring");
        match case["proj"].as_array().filter(|_| !unread) {
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
        // The code PROJ identifies, where it identifies one; a declared code that is not the CRS
        // is not kept.
        if let Some(code) = case["epsg"].as_u64().filter(|_| !unread) {
            assert_eq!(crs.epsg, Some(code as u32), "{name}");
        }
        if let Some(code) = case["esri"].as_u64() {
            assert_eq!(crs.esri, Some(code as u32), "{name}");
        }
        // An Esri CRS's name on another datum identifies no code.
        if name.contains("renamed") {
            assert_eq!((crs.epsg, crs.esri), (None, None), "{name}");
        }
        if name.contains("declaring") {
            let declared: u32 = name.rsplit("EPSG:").next().unwrap().parse().unwrap();
            assert_ne!(crs.epsg, Some(declared), "{name}");
        }
    }
}

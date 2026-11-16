//! A WKT 1 projected CRS whose parameters have EPSG's names (`Latitude of false origin`,
//! `Longitude of projection centre`, `Angle from Rectified to Skew Grid`, `Latitude of standard
//! parallel`) is read as PROJ 9.8.1 reads it, not without them; one PROJ refuses is refused. A
//! `PROJCS` with two `UNIT` nodes is in the first, as PROJ takes it (opengeos/terrio#1012, items 25
//! and 31).
//!
//! `data/wkt1_epsg_names.json` (`scripts/gen_wkt1_epsg_names.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`)
//! holds PROJ's point from WGS 84 (`always_xy`), `null` where PROJ refuses. Tolerance: 1 mm.

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

#[test]
fn epsg_parameter_names_in_wkt1_are_read_as_proj_reads_them() {
    let data: Value = serde_json::from_str(include_str!("data/wkt1_epsg_names.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 9);
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
        let (lon, lat) = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
        let got = Transformer::new(&wgs84, &crs).and_then(|t| t.transform(lon, lat));
        match case["proj"].as_array() {
            Some(p) => {
                let (x, y) = got.unwrap_or_else(|e| panic!("{name}: {e}"));
                let (px, py) = (p[0].as_f64().unwrap(), p[1].as_f64().unwrap());
                assert!((x - px).abs() < 1e-3 && (y - py).abs() < 1e-3, "{name}: ({x}, {y}), PROJ ({px}, {py})");
            }
            None => assert!(got.is_err(), "{name}: {got:?}"),
        }
    }
}

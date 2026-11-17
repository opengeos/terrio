//! A WKT 2 world projection (Robinson, Sinusoidal, Mollweide, Eckert IV, Natural Earth) given a
//! latitude of natural origin, which its method has no use for, is read without it, as PROJ 9.9.0
//! ignores it, rather than refused.
//!
//! `data/wkt2_world_latitude.json` (`scripts/gen_wkt2_world_latitude.py`, pyproj 3.8.0,
//! `PROJ_NETWORK=OFF`) holds PROJ's PROJ string and point from WGS 84 (`always_xy`). Tolerance:
//! 1 mm.

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

#[test]
fn an_unused_latitude_of_a_world_projection_is_ignored() {
    let data: Value = serde_json::from_str(include_str!("data/wkt2_world_latitude.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 10);
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
        assert_eq!(crs.to_proj4().as_deref(), case["proj4"].as_str(), "{name}");
        let (lon, lat) = (case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap());
        let (x, y) = Transformer::new(&wgs84, &crs).and_then(|t| t.transform(lon, lat)).unwrap_or_else(|e| panic!("{name}: {e}"));
        let (px, py) = (case["proj"][0].as_f64().unwrap(), case["proj"][1].as_f64().unwrap());
        assert!((x - px).abs() < 1e-3 && (y - py).abs() < 1e-3, "{name}: ({x}, {y}), PROJ ({px}, {py})");
    }
}

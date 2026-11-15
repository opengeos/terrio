//! WKT 1 CRSs whose angular unit is the grad place points as PROJ 9.8.1 does: NTF (Paris)
//! (EPSG:4807) and its projected CRSs (EPSG:27561-27564, 27571-27574, 27581-27584, 27591-27594), as
//! GDAL and ESRI write them, with their codes (identified) and without (read); geographic CRSs in
//! grads on Greenwich and with prime meridians in grads; Ferro and Lisbon, which PROJ knows by name,
//! in degrees beside grads and in EPSG's sexagesimal degrees; ATF (Paris) (EPSG:27500), whose
//! "Paris RGS" GDAL and ESRI also write in degrees; and Krovak East North (EPSG:5514) with its
//! angles in grads. Coordinates in a geographic CRS in grads are in grads, both ways. Paris is
//! PROJ's `+pm=paris` (2.337229166667°), as PROJ transforms with it, not the WKT's 2.33722917°.
//!
//! `data/wkt1_grads.json` (`scripts/gen_wkt1_grads.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`, no grids
//! installed) holds the WKTs (`CRS.from_epsg(code).to_wkt("WKT1_GDAL")` and `"WKT1_ESRI"`, those
//! with their names replaced and `AUTHORITY` removed, and WKTs written by hand) and PROJ's
//! coordinates by `Transformer.from_crs(source, wkt, always_xy=True)` and back by
//! `Transformer.from_crs(wkt, source, always_xy=True)`. Tolerance: 1 mm in metres, 1e-9 in grads
//! and degrees (0.1 mm).

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

fn cases() -> Vec<Value> {
    let data: Value = serde_json::from_str(include_str!("data/wkt1_grads.json")).unwrap();
    assert_eq!(data["proj_version"], "9.8.1");
    data["cases"].as_array().unwrap().clone()
}

fn pair(v: &Value) -> (f64, f64) {
    (v[0].as_f64().unwrap(), v[1].as_f64().unwrap())
}

/// How far `got` is from `want`, against the tolerance for coordinates of `crs`: 1 mm, or 1e-9 of
/// an angular unit. Above 1 is out of tolerance.
fn off(crs: &SpatialRef, got: (f64, f64), want: (f64, f64)) -> f64 {
    let d = (got.0 - want.0).hypot(got.1 - want.1);
    if crs.is_geographic() { d / 1e-9 } else { d / 1e-3 }
}

#[test]
fn crss_in_grads_place_points_as_proj_does() {
    offline();
    let cases = cases();
    assert_eq!(cases.len(), 85);
    let mut bad = vec![];
    for case in &cases {
        let name = case["name"].as_str().unwrap();
        let text = case["wkt"].as_str().unwrap();
        let wkt = SpatialRef::from_wkt(text);
        let src = SpatialRef::parse(case["src"].as_str().unwrap()).unwrap();
        // A WKT without its codes and names is read, not identified.
        if text.contains(r#"["custom""#) {
            assert_eq!((wkt.epsg, wkt.esri), (None, None), "{name}: identified");
        }
        let (proj, back) = (pair(&case["proj"]), pair(&case["proj_back"]));
        match Transformer::new(&src, &wkt).and_then(|t| t.transform(case["point"][0].as_f64().unwrap(), case["point"][1].as_f64().unwrap()))
        {
            Ok(got) if off(&wkt, got, proj) <= 1.0 => {}
            Ok(got) => bad.push(format!("{name}: {got:?}, PROJ {proj:?}")),
            Err(e) => bad.push(format!("{name}: {e}")),
        }
        match Transformer::new(&wkt, &src).and_then(|t| t.transform(proj.0, proj.1)) {
            Ok(got) if off(&src, got, back) <= 1.0 => {}
            Ok(got) => bad.push(format!("{name}, back: {got:?}, PROJ {back:?}")),
            Err(e) => bad.push(format!("{name}, back: {e}")),
        }
    }
    assert!(bad.is_empty(), "{} of {}:\n{}", bad.len(), 2 * cases.len(), bad.join("\n"));
}

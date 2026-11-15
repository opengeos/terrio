//! CRSs whose angular unit is the grad, and prime meridians, place points as PROJ 9.8.1 does.
//!
//! WKT 1: NTF (Paris) (EPSG:4807) and its projected CRSs (EPSG:27561-27564, 27571-27574,
//! 27581-27584, 27591-27594), as GDAL and ESRI write them, with their codes (identified) and without
//! (read); geographic CRSs in grads on Greenwich and with prime meridians in grads; Ferro and
//! Lisbon, which PROJ knows by name, in degrees beside grads and in EPSG's sexagesimal degrees; ATF
//! (Paris) (EPSG:27500), whose "Paris RGS" GDAL and ESRI also write in degrees; and Krovak East
//! North (EPSG:5514) with its angles in grads. WKT 2: PROJ's WKT 2 of EPSG:4807, 27572 and 27500,
//! and prime meridians in their own unit (named meridians in sexagesimal degrees or in degrees
//! beside grads), in their CRS's or base CRS's (an `ANGLEUNIT`, or a `UNIT` only when named grad)
//! and in none (degrees, whatever the axes' unit), under geographic, geodetic, projected and
//! geocentric CRSs. Coordinates in a
//! geographic CRS in grads are in grads, both ways.
//!
//! Paris is PROJ's `+pm=paris` (2.337229166667°), as PROJ transforms with it, not the WKT's
//! 2.33722917°: so is any prime meridian within 1e-10 radians of it, from EPSG:4807 or a PROJ
//! string, and one 1.1e-10 radians off is not.
//!
//! `data/grads_and_prime_meridians.json` (`scripts/gen_grads_and_prime_meridians.py`, pyproj
//! 3.8.0, `PROJ_NETWORK=OFF`, no grids installed) holds the WKTs (`CRS.from_epsg(code).to_wkt(...)`
//! with `"WKT1_GDAL"`, `"WKT1_ESRI"` and `"WKT2_2019"`, those with their names replaced and
//! `AUTHORITY` or `ID` removed, and WKTs written by hand) and PROJ's coordinates by
//! `Transformer.from_crs(source, wkt, always_xy=True)` and back by
//! `Transformer.from_crs(wkt, source, always_xy=True)` (in radians for axes in radians, which
//! pyproj gives in degrees), and the longitude of (0, 0) of the prime
//! meridian cases in EPSG:4275 by `Transformer.from_crs(crs, "EPSG:4275", always_xy=True)`.
//! Tolerance: 1 mm in metres, 1e-9 of an angular unit (0.1 mm in grads and degrees); 1e-12 degrees for the prime
//! meridians.

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

fn data() -> Value {
    let data: Value = serde_json::from_str(include_str!("data/grads_and_prime_meridians.json")).unwrap();
    assert_eq!(data["proj_version"], "9.8.1");
    data
}

fn cases() -> Vec<Value> {
    data()["cases"].as_array().unwrap().clone()
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
    assert_eq!(cases.len(), 110);
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

/// A prime meridian within 1e-10 radians of PROJ's Paris is `+pm=paris`, from an EPSG code or a
/// PROJ string, on both sides of it; one 1.1e-10 radians off is kept.
#[test]
fn prime_meridians_within_1e_10_radians_of_paris_are_paris() {
    offline();
    let ntf = SpatialRef::from_epsg(4275).unwrap();
    let cases = data()["pm_cases"].as_array().unwrap().clone();
    assert_eq!(cases.len(), 6);
    for case in cases {
        let crs = case["crs"].as_str().unwrap();
        let (lon, _) = Transformer::new(&SpatialRef::parse(crs).unwrap(), &ntf).unwrap().transform(0.0, 0.0).unwrap();
        let proj = case["lon"].as_f64().unwrap();
        assert!((lon - proj).abs() < 1e-12, "{crs}: {lon}, PROJ {proj}");
    }
}

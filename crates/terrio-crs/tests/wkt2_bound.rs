//! A WKT 2 `BOUNDCRS` with a Helmert transformation places points as PROJ 9.8.1 does: to WGS 84
//! by that shift, and to another CRS (ED50, OSGB36) by that shift followed by the other CRS's own
//! transformations to WGS 84, as PROJ concatenates them; into a CRS on the other CRS's datum, by
//! the shift alone; into a CRS on the source CRS's own datum, with none. A shift a `BOUNDCRS`
//! gives over a CRS terrio identifies as an EPSG code wins over the code's.
//!
//! `data/wkt2_bound.json` (`scripts/gen_wkt2_bound.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`, no
//! grids installed) holds PROJ's coordinates (`always_xy`) in WGS 84, or in another CRS and back.
//! Tolerance: 1 mm.

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

/// Metres between two longitudes/latitudes, near enough at a millimetre.
fn metres(a: (f64, f64), b: (f64, f64)) -> f64 {
    ((a.0 - b.0) * b.1.to_radians().cos()).hypot(a.1 - b.1) * 111_195.0
}

#[test]
fn bound_crss_place_points_as_proj_does() {
    offline();
    let data: Value = serde_json::from_str(include_str!("data/wkt2_bound.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 32);
    let mut bad = vec![];
    for case in cases {
        let num = |v: &Value, i: usize| v[i].as_f64().unwrap();
        let dst = SpatialRef::parse(case["dst"].as_str().unwrap()).unwrap();
        let t = Transformer::new(&SpatialRef::from_wkt(case["wkt"].as_str().unwrap()), &dst).unwrap();
        let got = t.transform(num(&case["point"], 0), num(&case["point"], 1)).unwrap();
        let proj = (num(&case["proj"], 0), num(&case["proj"], 1));
        let off = if dst.is_geographic() { metres(got, proj) } else { (got.0 - proj.0).hypot(got.1 - proj.1) };
        if off >= 1e-3 {
            bad.push(format!("{} -> {}: {got:?}, PROJ {proj:?}, {off:.3} m", case["name"], case["dst"]));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Back from a CRS on the target's datum, the bound transformation is undone, as PROJ undoes it.
#[test]
fn the_bound_transformation_is_undone_the_other_way() {
    offline();
    let data: Value = serde_json::from_str(include_str!("data/wkt2_bound.json")).unwrap();
    for case in data["cases"].as_array().unwrap().iter().filter(|c| c["dst"] != "EPSG:4326") {
        let num = |v: &Value, i: usize| v[i].as_f64().unwrap();
        let bound = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
        let t = Transformer::new(&SpatialRef::parse(case["dst"].as_str().unwrap()).unwrap(), &bound).unwrap();
        let got = t.transform(num(&case["proj"], 0), num(&case["proj"], 1)).unwrap();
        let proj = (num(&case["proj_back"], 0), num(&case["proj_back"], 1));
        let off = if bound.is_geographic() { metres(got, proj) } else { (got.0 - proj.0).hypot(got.1 - proj.1) };
        assert!(off < 1e-3, "{}: {got:?}, PROJ {proj:?}", case["name"]);
    }
}

/// A scale difference in parts per million is read in that unit, as the same shift written as
/// PROJ's ratio: PROJ 9.8.1 reads an abridged transformation's scale as a ratio whatever its unit
/// (6.7 ppm as 6.7, `+towgs84=...,5700000`), which terrio does not follow.
#[test]
fn a_scale_difference_in_parts_per_million_is_read_in_that_unit() {
    offline();
    let data: Value = serde_json::from_str(include_str!("data/wkt2_bound.json")).unwrap();
    let case = data["cases"].as_array().unwrap().iter().find(|c| c["name"] == "to WGS 84").unwrap();
    let ratio = case["wkt"].as_str().unwrap();
    let ppm = ratio.replace(r#"1.0000067,SCALEUNIT["unity",1]"#, r#"6.7,SCALEUNIT["parts per million",1E-06]"#);
    assert_ne!(ppm, ratio);
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let at = |w: &str| Transformer::new(&SpatialRef::from_wkt(w), &wgs84).unwrap().transform(10.0, 50.0).unwrap();
    assert!(metres(at(&ppm), at(ratio)) < 1e-6);
    assert!(SpatialRef::from_wkt(&ppm).to_proj4().unwrap().contains("+towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7 "));
}

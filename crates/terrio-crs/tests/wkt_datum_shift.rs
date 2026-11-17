//! A WKT whose geographic CRS has no EPSG code of its own gets the datum transformations of the
//! EPSG datum it names, as PROJ 9.9.0 transforms it to WGS 84: a datum known by its ID alone
//! (WKT 2 `DATUM[...,ID["EPSG",6277]]`, WKT 1 `AUTHORITY["EPSG","6277"]` on the `DATUM`), under
//! another CRS and datum name too, by its name in a WKT 2 without IDs, or by ESRI's name
//! (`D_Monte_Mario`, `D_OSGB_1936`), with or without the datum's or the CRS's code. The two cases
//! of opengeos/terrio#1012 (EPSG:3003 as ESRI WKT 1, EPSG:4277 as WKT 2 without its CRS and
//! datum IDs) are among them.
//!
//! `data/wkt_datum_shift.json` (`scripts/gen_wkt_datum_shift.py`, pyproj 3.8.0, no grids,
//! `PROJ_NETWORK=OFF`) holds each WKT, a point in it and PROJ's coordinates of the point in WGS 84
//! by `Transformer.from_crs(CRS(wkt), "EPSG:4326", always_xy=True)`, the transformation PROJ used
//! (`get_last_used_operation().description`), and where the CRS's EPSG code places the point.
//! Tolerance: 1 mm.
//!
//! Without grids, PROJ uses its best transformation that needs none, a datum transformation in
//! every case (none is its no-shift ballpark). For OSGB36, DHDN and some CH1903 cases a better
//! one needs a grid (OSTN15, BETA2007, CHENyx06; `best_grid_missing`):
//! with those grids installed, PROJ may place these points otherwise, which this test doesn't
//! cover.
//!
//! PROJ places a few WKTs otherwise than their EPSG code (`differences`): it does not resolve
//! their geographic CRS to the code (an ESRI WKT 1 has no `AXIS`, so longitude first, unlike the
//! EPSG CRS; a renamed CRS), and then also searches transformations through other datums and other
//! CRSs of the datum, and may pick one of those (Monte Mario (Rome) through Monte Mario, 72 m from
//! EPSG:4806's own). terrio follows that search (see `wkt_unresolved.rs`), but PROJ's datum pivot
//! through the datum's geocentric CRS: ONGD14 (EPSG:7373, `pending`) is still placed by its code.

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

fn data() -> Value {
    serde_json::from_str(include_str!("data/wkt_datum_shift.json")).unwrap()
}

fn pair(v: &Value) -> (f64, f64) {
    (v[0].as_f64().unwrap(), v[1].as_f64().unwrap())
}

/// Where terrio places a case's point in WGS 84, and the transformation it uses. Both come from
/// `transform_3d`'s path, which `operation_at` reports on (no API returns the point with its
/// operation); `transform`'s 2D path must place the point there too.
fn place(case: &Value) -> ((f64, f64), String) {
    let src = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
    let t = Transformer::new(&src, &SpatialRef::from_epsg(4326).unwrap()).unwrap();
    let (x, y) = pair(&case["point"]);
    let (lon, lat, _) = t.transform_3d(x, y, 0.0).unwrap();
    let operation = t.operation_at(x, y, 0.0).unwrap().map(|o| o.name).unwrap_or_default();
    let flat = t.transform(x, y).unwrap();
    let name = case["name"].as_str().unwrap();
    assert!(metres(flat, (lon, lat)) < 1e-6, "{name}: transform {flat:?}, transform_3d {:?}", (lon, lat));
    ((lon, lat), operation)
}

#[test]
fn datums_known_by_id_or_esri_name_are_shifted_as_proj_does() {
    offline();
    let data = data();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 178);
    let mut bad = vec![];
    for case in cases {
        let (got, operation) = place(case);
        let proj = pair(&case["proj"]);
        let off = metres(got, proj);
        // PROJ's name for the transformation holds terrio's (PROJ adds the conversions).
        let named = !operation.is_empty() && case["operation"].as_str().unwrap().contains(&operation);
        // A NaN distance fails too.
        if !off.is_finite() || off >= 1e-3 || !named {
            let (name, proj_operation) = (case["name"].as_str().unwrap(), case["operation"].as_str().unwrap());
            bad.push(format!("{name}: {got:?} by {operation:?}, PROJ {proj:?} by {proj_operation:?}, {off:.3} m"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// Each of the WKTs PROJ places otherwise than their EPSG code, through other datums, with
/// `pending` set or not.
fn differences(pending: bool) -> Vec<Value> {
    let data = data();
    let all = data["differences"].as_array().unwrap();
    assert_eq!(all.len(), 7);
    all.iter().filter(|case| case["pending"].as_bool().unwrap() == pending).cloned().collect()
}

/// The WKTs PROJ places otherwise than their EPSG code, through other datums, are placed as PROJ
/// places them.
#[test]
fn wkts_proj_does_not_resolve_are_placed_through_other_datums_as_proj_does() {
    offline();
    let cases = differences(false);
    assert_eq!(cases.len(), 6);
    for case in &cases {
        let (got, _) = place(case);
        let (proj, by_code) = (pair(&case["proj"]), pair(&case["by_code"]));
        let name = case["name"].as_str().unwrap();
        assert!(metres(got, proj) < 1e-3, "{name}: {got:?}, PROJ {proj:?}");
        // The fixture's own consistency: the generator recorded PROJ placing it elsewhere (a
        // regenerated fixture where PROJ now places it by its code moves it to `cases`).
        assert!(metres(proj, by_code) >= 1e-3, "{name}: PROJ {proj:?} placed by its code");
    }
}

/// The one PROJ places by its datum pivot (EPSG:7373's ESRI WKT 1, ONGD14) is placed as PROJ
/// places it: through "ONGD14 to WGS 84 (1)", registered between the datum's geocentric CRSs
/// (0.1 m), not its code's null transformation (opengeos/terrio#1012, item 7 step 2).
#[test]
fn a_wkt_proj_places_by_its_datum_pivot_is_placed_as_proj_does() {
    offline();
    let cases = differences(true);
    assert_eq!(cases.len(), 1);
    for case in &cases {
        let (got, _) = place(case);
        let (proj, by_code) = (pair(&case["proj"]), pair(&case["by_code"]));
        let name = case["name"].as_str().unwrap();
        assert!(metres(got, proj) < 1e-3, "{name}: {got:?}, PROJ {proj:?}");
        assert!(metres(proj, by_code) >= 1e-3, "{name}: PROJ {proj:?} placed by its code");
    }
}

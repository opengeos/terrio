//! A WKT PROJ 9.9.0 does not resolve to the EPSG CRS terrio reads it as is placed as PROJ places
//! it. Such WKTs include an ESRI WKT 1 (no `AXIS`, so longitude first, unlike the EPSG CRS), a
//! renamed CRS, and a projected CRS whose axis order differs. PROJ then also searches chains
//! through an intermediate datum, and may rank one of them first: ESRI's `GCS_Monte_Mario_Rome`
//! (EPSG:4806) goes through Monte Mario, 72 m from EPSG:4806's own transformation. WKTs PROJ does
//! resolve keep their code's placement.
//!
//! `data/wkt_unresolved.json` (`scripts/gen_wkt_unresolved.py`, pyproj 3.8.0, no grids,
//! `PROJ_NETWORK=OFF`) holds variants of EPSG geographic 2D CRSs and of some projected CRSs: ESRI
//! WKT 1, renamed, WKT 2 without IDs, a WKT 2 `GEODCRS` with an ellipsoidal CS, an ESRI compound
//! CRS, renamed CRSs on the WGS 84 and ETRS89 ensembles with their IDs, CRSs named by an alias, and
//! others. Each case is a point and where PROJ places it, with `Transformer.from_crs(src, dst,
//! always_xy=True).transform`: to WGS 84, from WGS 84, and, for a sample, to ETRS89, NAD83 and
//! ETRS89 / UTM zone 32N. `by_code` is the same with the EPSG code in place of the WKT. Tolerance:
//! 1 mm.
//!
//! PROJ's datum pivot through the other CRSs of a datum (ESRI's ONGD14, EPSG:7373, by "ONGD14 to
//! WGS 84 (1)" between the geocentric CRSs) is followed from an unresolved CRS, and into one PROJ
//! cannot find by its name (ONGD14 renamed). So is its ballpark into the CRSs of a renamed datum
//! (`DATUM["Local datum",..., ID["EPSG",6806]]`), of unknown accuracy: from Monte Mario (Rome),
//! PROJ then goes through Monte Mario (72 m from the code's placement), from the Georgia Geodetic
//! Datum through Pulkovo 1942 (7 m). From ONGD14 renamed with an ID of an authority PROJ does not
//! know (`AUTHORITY["LOCAL","1"]`), PROJ's datum pivot takes only the geocentric CRS's
//! transformation, the first CRS of the datum with one, and so does terrio (`pending` cases, of a
//! step terrio does not follow, are placed by the code: none now). ESRI's ATF (Paris) (EPSG:4901) is left out: PROJ applies the Paris prime
//! meridian twice to it, 178 km from its own placement of EPSG:4901, and terrio does not follow
//! that (`grads_and_prime_meridians.rs` checks it is placed as EPSG:4901).
use serde_json::Value;
use terrio_crs::{SpatialRef, TransformOptions, Transformer};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

fn data() -> Value {
    serde_json::from_str(include_str!("data/wkt_unresolved.json")).unwrap()
}

fn pair(v: &Value) -> (f64, f64) {
    (v[0].as_f64().unwrap(), v[1].as_f64().unwrap())
}

/// The distance between two points, in metres: near enough at a millimetre for longitudes and
/// latitudes.
fn metres(a: (f64, f64), b: (f64, f64), geographic: bool) -> f64 {
    if geographic { ((a.0 - b.0) * b.1.to_radians().cos()).hypot(a.1 - b.1) * 111_195.0 } else { (a.0 - b.0).hypot(a.1 - b.1) }
}

/// Where terrio places a case's point.
fn place(data: &Value, case: &Value) -> Option<(f64, f64)> {
    let wkt = SpatialRef::from_wkt(data["wkts"][case["wkt"].as_str().unwrap()].as_str().unwrap());
    let other = SpatialRef::from_epsg(case["other"].as_u64().unwrap() as u32).unwrap();
    let (src, dst) = if case["reverse"].as_bool().unwrap() { (other, wkt) } else { (wkt, other) };
    let (x, y) = pair(&case["point"]);
    Transformer::new(&src, &dst).ok()?.transform(x, y).ok()
}

/// The cases, with how far terrio places each from PROJ and from the code's placement, computed
/// once for both tests.
fn offsets() -> &'static [(Value, f64, f64)] {
    static OFFSETS: std::sync::OnceLock<Vec<(Value, f64, f64)>> = std::sync::OnceLock::new();
    OFFSETS.get_or_init(compute_offsets)
}

fn compute_offsets() -> Vec<(Value, f64, f64)> {
    offline();
    let data = data();
    let cases = data["cases"].as_array().unwrap();
    cases
        .iter()
        .map(|case| {
            let geographic = case["geographic"].as_bool().unwrap();
            let got = place(&data, case).unwrap_or((f64::NAN, f64::NAN));
            let off = |key: &str| {
                let d = metres(got, pair(&case[key]), geographic);
                if d.is_nan() { f64::INFINITY } else { d }
            };
            (case.clone(), off("proj"), off("by_code"))
        })
        .collect()
}

#[test]
fn wkts_proj_does_not_resolve_are_placed_as_proj_does() {
    let all = offsets();
    assert!(all.len() > 1000, "{} cases", all.len());
    let mut bad = vec![];
    let mut searched = 0;
    for (case, proj, by_code) in all {
        if case["pending"].as_bool().unwrap() {
            continue;
        }
        let code = metres(pair(&case["proj"]), pair(&case["by_code"]), case["geographic"].as_bool().unwrap());
        if code > 0.01 {
            searched += 1;
        }
        if *proj >= 1e-3 {
            bad.push(format!("{}: {proj:.3} m from PROJ, {by_code:.3} m from the code's placement", case["name"].as_str().unwrap()));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    // Those PROJ places otherwise than the code, through its search: both ways.
    assert!(searched >= 130, "{searched} cases placed otherwise than by the code");
}

/// A CRS with an ID of an authority PROJ does not know (`AUTHORITY["LOCAL","1"]`) is placed as PROJ
/// places it, by the first CRS of its datum with transformations: ONGD14's geocentric CRS, so
/// "ONGD14 to WGS 84 (1)", 1.2 m from the code's placement (opengeos/terrio#1012 item 7).
#[test]
fn a_crs_with_an_unknown_authority_is_placed_by_its_datum_pivot() {
    let local: Vec<_> = offsets().iter().filter(|(case, ..)| case["name"] == "7373 esri gcs renamed, local id -> 4326").collect();
    assert_eq!(local.len(), 1);
    for (case, proj, by_code) in local {
        let name = case["name"].as_str().unwrap();
        assert!(!case["pending"].as_bool().unwrap(), "{name}");
        assert!(*proj < 1e-3, "{name}: {proj:.3} m from PROJ");
        assert!(*by_code > 1.0, "{name}: {by_code:.3} m from the code's placement");
    }
}

/// With the ballpark refused (`ALLOW_BALLPARK=NO`), PROJ also refuses the transformations it
/// frames with its ballpark from a renamed datum, and keeps the CRS's own registered ones. PROJ
/// 9.9.0 places the renamed Monte Mario (Rome) and Tokyo 1892 by their codes' own transformations
/// then, and has none for the renamed Georgia Geodetic Datum.
#[test]
fn transformations_framed_by_the_ballpark_are_refused_with_it() {
    offline();
    let data = data();
    let strict = TransformOptions { allow_ballpark: Some(false), ..Default::default() };
    let wgs84 = SpatialRef::from_epsg(4326).unwrap();
    let wkt = |key: &str| SpatialRef::from_wkt(data["wkts"][key].as_str().unwrap());
    for (key, point, want) in [
        ("4806 wkt2 datum id, renamed", (0.007666666666664175, 40.93), (12.45982281152141, 40.93062597830525)),
        ("5132 wkt2 datum id, renamed", (138.44, 32.955), (138.43988601577175, 32.95840880187001)),
    ] {
        let t = Transformer::with_options(&wkt(key), &wgs84, &strict).unwrap();
        let got = t.transform(point.0, point.1).unwrap();
        assert!(metres(got, want, true) < 1e-3, "{key}: {got:?}");
        let op = Transformer::new(&wkt(key), &wgs84).unwrap().operation_at(point.0, point.1, 0.0).unwrap().unwrap();
        assert!(op.ballpark && op.accuracy.is_none(), "{key}: {op:?}");
    }
    let georgia = wkt("10831 wkt2 datum id, renamed");
    assert!(Transformer::with_options(&georgia, &wgs84, &strict).and_then(|t| t.transform(42.845, 42.315)).is_err());
    assert!(!Transformer::new(&georgia, &wgs84).unwrap().has_non_ballpark_operation());
}

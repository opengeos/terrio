//! Regression inputs from rust-proj-capability (#266), pinned in the fixture's source and
//! environment fields. Forward tolerance is 1 mm; inverse tolerance is 1e-7 degrees,
//! with longitude scaled by cos(latitude) at poles, as in the upstream benchmark. At an exact
//! input pole, allow 1e-6 degrees: one last bit before asin can move PROJ's inverse by 8.5e-7°.

use terrio::crs::{SpatialRef, Transformer};

fn check(id: &str) {
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/crs_benchmark.json")).unwrap();
    let case = reference["projections"].as_array().unwrap().iter().find(|p| p["id"] == id).unwrap();
    let geographic = SpatialRef::from_proj4(case["geographic"].as_str().unwrap()).unwrap();
    // The upstream projection suite invokes bare PROJ operations. Terrio implements CRS
    // semantics: wintri's omitted lat_1 is 0 in a CRS, but acos(2/pi) in an operation.
    let mut definition = case["proj"].as_str().unwrap().to_owned();
    if id == "wintri" {
        definition.push_str(&format!(" +lat_1={}", (2.0 / std::f64::consts::PI).acos().to_degrees()));
    }
    let projected = SpatialRef::from_proj4(&definition).unwrap();
    let forward = Transformer::new(&geographic, &projected).unwrap();
    let inverse = Transformer::new(&projected, &geographic).unwrap();
    let mut failures = Vec::new();
    for (i, point) in case["points"].as_array().unwrap().iter().enumerate() {
        let lon = point[0].as_f64().unwrap();
        let lat = point[1].as_f64().unwrap();
        let inverse_tolerance = if lat.abs() == 90.0 { 1e-6 } else { 1e-7 };
        let xy = &case["fwd"][i];
        let actual = forward.transform(lon, lat);
        if xy.is_null() {
            if actual.is_ok_and(|(x, y)| x.is_finite() && y.is_finite()) {
                failures.push(format!("forward ({lon}, {lat}) accepted outside domain"));
            }
            continue;
        }
        let x = xy[0].as_f64().unwrap();
        let y = xy[1].as_f64().unwrap();
        match actual {
            Ok((tx, ty)) if (tx - x).hypot(ty - y) < 1e-3 => {}
            other => failures.push(format!("forward ({lon}, {lat}): {other:?}, expected ({x}, {y})")),
        }
        let ll = &case["inv"][i];
        let actual = inverse.transform(x, y);
        if ll.is_null() {
            // PROJ's iterative inverses can refuse valid poles/map boundaries. A recovered
            // input is acceptable, but returning an arbitrary finite point is not.
            if let Ok((lo, la)) = actual {
                let dl = ((lo - lon + 180.0).rem_euclid(360.0) - 180.0) * lat.to_radians().cos();
                if dl.hypot(la - lat) >= inverse_tolerance {
                    failures.push(format!("inverse ({x}, {y}): ({lo}, {la}), expected input ({lon}, {lat})"));
                }
            }
            continue;
        }
        let ilon = ll[0].as_f64().unwrap();
        let ilat = ll[1].as_f64().unwrap();
        match actual {
            Ok((lo, la)) => {
                let dlon = ((lo - ilon + 180.0).rem_euclid(360.0) - 180.0) * ilat.to_radians().cos();
                if dlon.hypot(la - ilat) >= inverse_tolerance {
                    failures.push(format!("inverse of ({lon}, {lat}): ({lo}, {la}), expected ({ilon}, {ilat})"));
                }
            }
            other => failures.push(format!("inverse of ({lon}, {lat}): {other:?}")),
        }
    }
    assert!(failures.is_empty(), "{id}: {} discrepancies:\n{}", failures.len(), failures.join("\n"));
}

#[test]
fn mollweide() {
    check("moll");
}

#[test]
fn wagner_iv() {
    check("wag4");
}

#[test]
fn wagner_v() {
    check("wag5");
}

#[test]
fn orthographic() {
    check("ortho");
}

#[test]
fn winkel_tripel_explicit_parallel_matches_operation_oracle() {
    check("wintri");
}

#[test]
fn winkel_tripel_omitted_parallel_preserves_crs_default() {
    let geo = SpatialRef::from_proj4("+proj=longlat +R=6371000").unwrap();
    let default = SpatialRef::from_proj4("+proj=wintri +R=6371000").unwrap();
    let zero = SpatialRef::from_proj4("+proj=wintri +R=6371000 +lat_1=0").unwrap();
    let default = Transformer::new(&geo, &default).unwrap();
    let zero = Transformer::new(&geo, &zero).unwrap();
    assert_eq!(default.transform(147.0, -42.0).unwrap(), zero.transform(147.0, -42.0).unwrap());
}

#[test]
fn orthographic_roundtrips_across_origins_scales_and_rotations() {
    for figure in ["+R=6371000", "+ellps=WGS84", "+ellps=GRS80"] {
        let geo = SpatialRef::from_proj4(&format!("+proj=longlat {figure}")).unwrap();
        for origin in [-90.0_f64, -80.0, -42.0, 0.0, 35.0, 80.0, 90.0] {
            for local in ["", "+k_0=0.9 +alpha=30", "+k_0=1.1 +alpha=-75 +x_0=123 +y_0=-456"] {
                let definition = format!("+proj=ortho {figure} +lat_0={origin} +lon_0=147 {local}");
                let projected = SpatialRef::from_proj4(&definition).unwrap();
                let fwd = Transformer::new(&geo, &projected).unwrap();
                let inv = Transformer::new(&projected, &geo).unwrap();
                for lon in (-180..=180).step_by(15) {
                    for lat in (-90..=90).step_by(15) {
                        let (lon, lat) = (f64::from(lon), f64::from(lat));
                        let visibility = origin.to_radians().sin() * lat.to_radians().sin()
                            + origin.to_radians().cos() * lat.to_radians().cos() * (lon - 147.0).to_radians().cos();
                        let xy = fwd.transform(lon, lat);
                        if visibility < -1e-10 {
                            assert!(xy.is_err(), "hidden point accepted: {definition}, ({lon}, {lat})");
                            continue;
                        }
                        let (x, y) = xy.unwrap();
                        let (lo, la) = inv.transform(x, y).unwrap();
                        let dl = ((lo - lon + 180.0).rem_euclid(360.0) - 180.0) * lat.to_radians().cos();
                        // At the limb the inverse is ill-conditioned: a last-bit projected
                        // difference can move latitude by several millionths of a degree.
                        assert!(dl.hypot(la - lat) < 1e-5, "{definition}, ({lon}, {lat}) -> ({lo}, {la})");
                    }
                }
                assert!(inv.transform(1e9, -1e9).is_err(), "outside limb accepted: {definition}");
            }
        }
    }
}

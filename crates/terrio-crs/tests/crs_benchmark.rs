//! Regression inputs from rust-proj-capability (#266), pinned in the fixture's source and
//! environment fields. Forward tolerance is 1 mm; inverse tolerance is 1e-7 degrees,
//! with longitude scaled by cos(latitude) at poles, as in the upstream benchmark. At an exact
//! input pole, allow 1e-6 degrees: one last bit before asin can move PROJ's inverse by 8.5e-7°.

use terrio_crs::{SpatialRef, Transformer};

fn check(id: &str) {
    check_encoding(id, None);
}

fn check_encoding(id: &str, wkt: Option<&str>) {
    let reference: serde_json::Value = serde_json::from_str(include_str!("data/crs_benchmark.json")).unwrap();
    let case = reference["projections"].as_array().unwrap().iter().find(|p| p["id"] == id).unwrap();
    let geographic = SpatialRef::from_proj4(case["geographic"].as_str().unwrap()).unwrap();
    // The upstream projection suite invokes bare PROJ operations. Terrio implements CRS
    // semantics: wintri's omitted lat_1 is 0 in a CRS, but acos(2/pi) in an operation.
    let mut definition = case["proj"].as_str().unwrap().to_owned();
    if id == "wintri" {
        definition.push_str(&format!(" +lat_1={}", (2.0 / std::f64::consts::PI).acos().to_degrees()));
    }
    let projected = wkt.map_or_else(|| SpatialRef::from_proj4(&definition), |wkt| Ok(SpatialRef::from_wkt(wkt))).unwrap();
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
fn lcc_omitted_second_parallel_is_zero_in_a_crs() {
    // GDAL 3.13.3 / PROJ 9.9.0 read `+proj=lcc +lat_1=33` as Lambert Conic Conformal (2SP) with
    // the latitude of false origin and the second parallel at 0 (a bare `lcc` operation puts both
    // on lat_1), and WKT's 2SP method without its second parallel with that parallel at 0.
    let geo = SpatialRef::from_proj4("+proj=longlat +datum=WGS84").unwrap();
    let wkt1 = r#"PROJCS["local",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic_2SP"],PARAMETER["latitude_of_origin",10],PARAMETER["central_meridian",20],PARAMETER["standard_parallel_1",33],PARAMETER["false_easting",5000],PARAMETER["false_northing",6000],UNIT["metre",1]]"#;
    let wkt2 = r#"PROJCRS["local",BASEGEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563]],UNIT["degree",0.0174532925199433]],CONVERSION["c",METHOD["Lambert Conic Conformal (2SP)",ID["EPSG",9802]],PARAMETER["Latitude of false origin",10,ANGLEUNIT["degree",0.0174532925199433]],PARAMETER["Longitude of false origin",20,ANGLEUNIT["degree",0.0174532925199433]],PARAMETER["Latitude of 1st standard parallel",33,ANGLEUNIT["degree",0.0174532925199433]],PARAMETER["Easting at false origin",5000,LENGTHUNIT["metre",1]],PARAMETER["Northing at false origin",6000,LENGTHUNIT["metre",1]]],CS[Cartesian,2],AXIS["(E)",east],AXIS["(N)",north],UNIT["metre",1]]"#;
    let cases = [
        (SpatialRef::from_proj4("+proj=lcc +lat_1=33 +datum=WGS84").unwrap(), (100_000.0, 2_000_000.0), (0.987544983668, 18.614169188061)),
        (SpatialRef::from_wkt(wkt1), (300_000.0, 800_000.0), (22.895533776974, 17.447486644559)),
        (SpatialRef::from_wkt(wkt2), (300_000.0, 800_000.0), (22.895533776974, 17.447486644559)),
    ];
    for (projected, (x, y), (lon, lat)) in cases {
        let (lo, la) = Transformer::new(&projected, &geo).unwrap().transform(x, y).unwrap();
        assert!((lo - lon).abs() < 1e-9 && (la - lat).abs() < 1e-9, "{projected:?}: ({lo}, {la})");
        // The WKT terrio writes reads back as the same CRS.
        let back = SpatialRef::from_wkt(&projected.to_wkt().unwrap());
        let (lo2, la2) = Transformer::new(&back, &geo).unwrap().transform(x, y).unwrap();
        assert!((lo2 - lon).abs() < 1e-9 && (la2 - lat).abs() < 1e-9, "{projected:?} written: ({lo2}, {la2})");
    }
    // With a latitude of origin, one parallel stays one parallel (1SP and its variant B).
    let one = SpatialRef::from_proj4("+proj=lcc +lat_1=33 +lat_0=33 +datum=WGS84").unwrap();
    let (lo, la) = Transformer::new(&one, &geo).unwrap().transform(100_000.0, 2_000_000.0).unwrap();
    assert!((lo - 1.343259630).abs() < 1e-6 && (la - 50.696077540).abs() < 1e-6, "({lo}, {la})");
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
fn polar_stereographic_hemisphere_follows_lat_ts_in_a_crs() {
    // GDAL 3.13.3 / PROJ 9.9.0 read a polar `stere` with `lat_ts` as Polar Stereographic
    // (variant B): the pole is on `lat_ts`'s side (north for 0), whatever `lat_0`'s sign (a bare
    // `stere` operation takes `lat_0`'s).
    let geo = SpatialRef::from_proj4("+proj=longlat +datum=WGS84").unwrap();
    for (definition, (lon, lat)) in [
        ("+proj=stere +lat_0=-90 +lat_ts=90 +datum=WGS84", (153.434948822922, 87.998236137629)),
        ("+proj=stere +lat_0=90 +lat_ts=-70 +datum=WGS84", (26.565051177078, -87.936036886833)),
        ("+proj=stere +lat_0=-90 +lat_ts=0 +datum=WGS84", (153.434948822922, 86.011022737183)),
        ("+proj=stere +lat_0=-90 +lat_ts=-70 +datum=WGS84", (26.565051177078, -87.936036886833)),
    ] {
        let projected = SpatialRef::from_proj4(definition).unwrap();
        let written = SpatialRef::from_wkt(&projected.to_wkt().unwrap());
        for crs in [projected, written] {
            let (lo, la) = Transformer::new(&crs, &geo).unwrap().transform(100_000.0, 200_000.0).unwrap();
            assert!((lo - lon).abs() < 1e-9 && (la - lat).abs() < 1e-9, "{definition}: ({lo}, {la})");
        }
    }
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

#[test]
fn stereographic_origins_near_a_pole_stay_accurate() {
    // PROJ 9.9.0's `sterea` rounds the conformal latitude of an origin within about 1e-7° of a pole
    // to the pole while the origin is not, and puts points in the other hemisphere, 12,714 km off
    // (`+lat_0=89.9999999`). GDAL 3.13.3 answers for `+lat_0=89.99999999` only because PROJ writes
    // that latitude as 90. The answers must approach the pole's as the origin does.
    let geo = SpatialRef::from_proj4("+proj=longlat +datum=WGS84").unwrap();
    let at = |lat_0: f64| {
        let crs = SpatialRef::from_proj4(&format!("+proj=sterea +lat_0={lat_0} +datum=WGS84")).unwrap();
        Transformer::new(&crs, &geo).unwrap().transform(1000.0, 2000.0).unwrap()
    };
    // Metres between two points near a pole.
    let apart = |(lon1, lat1): (f64, f64), (lon2, lat2): (f64, f64)| {
        let (r1, r2) = ((90.0 - lat1.abs()) * 111_195.0, (90.0 - lat2.abs()) * 111_195.0);
        let (a1, a2) = (lon1.to_radians(), lon2.to_radians());
        (r1 * a1.cos() - r2 * a2.cos()).hypot(r1 * a1.sin() - r2 * a2.sin())
    };
    for pole in [90.0_f64, -90.0] {
        let exact = at(pole);
        assert_eq!(exact.1.signum(), pole.signum());
        for off in [1e-6, 1e-7, 1e-8, 1e-9, 1e-12] {
            let near = at(pole - off * pole.signum());
            // The origin moves by `off` degrees, about 0.11 m per 1e-6°.
            assert!(near.1.signum() == pole.signum() && apart(near, exact) < 1.5 * off * 111_195.0, "{pole} - {off}: {near:?}");
        }
    }
    // GDAL 3.13.3 / PROJ 9.9.0, for lat_0 = 89.99999999 (computed at 90): within the 1e-8° shift.
    assert!(apart(at(89.99999999), (153.4349488229, 89.9799804075)) < 2e-3);

    // `stere` with an origin near a pole: within the origin's shift of the polar projection's
    // answer, 4,200 km away (it was metres off, as PROJ's `stere` still is at 89.9999999).
    let ecef = SpatialRef::parse("EPSG:4978").unwrap();
    let to_ecef = |definition: String, (x, y): (f64, f64)| {
        let crs = SpatialRef::from_proj4(&definition).unwrap();
        Transformer::new(&crs, &ecef).unwrap().transform_3d(x, y, 0.0).unwrap()
    };
    for pole in [90.0_f64, -90.0] {
        for point in [(200_000.0, 567_890.0), (3_000_000.0, -3_000_000.0)] {
            let exact = to_ecef(format!("+proj=stere +lat_0={pole} +datum=WGS84"), point);
            for off in [1e-6, 1e-7, 1e-8, 1e-9, 1e-12] {
                let near = to_ecef(format!("+proj=stere +lat_0={} +datum=WGS84", pole - off * pole.signum()), point);
                let d = ((near.0 - exact.0).powi(2) + (near.1 - exact.1).powi(2) + (near.2 - exact.2).powi(2)).sqrt();
                // A shift of `off` degrees moves the origin by `off` × 111 km and turns the plane
                // by `off` (in radians) about it.
                let shift = off * 111_195.0 + point.0.hypot(point.1) * off.to_radians();
                assert!(d < 1.5 * shift + 1e-6, "stere {pole} - {off}, {point:?}: {d} m");
            }
        }
    }
}

#[test]
fn native_wagner_wkt1_includes_pole_graticules() {
    let definitions = include_str!("data/proj_strings.tsv");
    // Git can check out this text fixture with CRLF on Windows. Exercise both
    // forms on every platform, including the full forward/inverse graticules.
    let lf = definitions.lines().collect::<Vec<_>>().join("\n");
    let crlf = lf.replace('\n', "\r\n");
    for definitions in [&lf, &crlf] {
        for id in ["wag4", "wag5"] {
            let marker = format!("# case {id} sphere");
            let wkt = definitions
                .lines()
                .skip_while(|line| *line != marker)
                .skip(1)
                .take_while(|line| !line.starts_with("# case "))
                .find_map(|line| line.strip_prefix("# wkt1 "))
                .expect("Wagner sphere case must contain WKT1");
            check_encoding(id, Some(wkt));
        }
    }
}

#[test]
fn web_mercator_keeps_its_origin_and_false_origin() {
    // GDAL 3.13.3 / PROJ 9.9.0. wbprojection, which `webmerc` went to with the fallback, dropped
    // `+x_0` and `+y_0` (7.4 km here); `k_0` is ignored, as PROJ ignores it.
    for (definition, geographic, (lon, lat)) in [
        ("+proj=webmerc +x_0=5000 +y_0=-6000 +datum=WGS84 +units=m", "+proj=longlat +datum=WGS84", (8.938237076989, 17.730260346253)),
        ("+proj=webmerc +lon_0=10 +datum=WGS84", "+proj=longlat +datum=WGS84", (18.983152841195, 17.678914238336)),
        ("+proj=webmerc +k_0=0.5 +ellps=GRS80", "+proj=longlat +ellps=GRS80", (8.983152841195, 17.678914238336)),
        // GDAL 3.12.2 / PROJ 9.8.1 ignores lat_ts even beyond the poles for webmerc.
        ("+proj=webmerc +lat_ts=95 +ellps=WGS84", "+proj=longlat +ellps=WGS84", (8.983152841195, 17.678914238336)),
    ] {
        let projected = SpatialRef::from_proj4(definition).unwrap();
        let geo = SpatialRef::from_proj4(geographic).unwrap();
        let (lo, la) = Transformer::new(&projected, &geo).unwrap().transform(1_000_000.0, 2_000_000.0).unwrap();
        assert!((lo - lon).abs() < 1e-9 && (la - lat).abs() < 1e-9, "{definition}: ({lo}, {la})");
        let (x, y) = Transformer::new(&geo, &projected).unwrap().transform(lon, lat).unwrap();
        assert!((x - 1_000_000.0).abs() < 1e-6 && (y - 2_000_000.0).abs() < 1e-6, "{definition}: ({x}, {y})");
    }
}

/// PROJ 9.9.0 / pyproj 3.8.0 resolve the versioned references to EPSG:4326.
#[test]
fn versioned_authority_references_match_proj() {
    for reference in [
        "urn:ogc:def:crs:EPSG:6.6:4326",
        "urn:ogc:def:crs:EPSG:9.9:4326",
        "http://www.opengis.net/def/crs/EPSG/0/4326",
        "https://opengis.net/def/crs/EPSG/6.6/4326",
    ] {
        let source = SpatialRef::parse(reference).unwrap();
        assert_eq!(source.epsg, Some(4326));
        let transform = Transformer::new(&source, &SpatialRef::from_epsg(3857).unwrap()).unwrap();
        let (x, y) = transform.transform(10.0, 40.0).unwrap();
        assert!((x - 1_113_194.907_932_735_7).abs() < 1e-7, "{reference}: {x}");
        assert!((y - 4_865_942.279_503_175).abs() < 1e-7, "{reference}: {y}");
    }
}

#[test]
fn quoted_proj_string_values_read_as_in_proj() {
    // GDAL 3.13.3 / PROJ 9.9.0 read values in double quotes, with `""` for a quote inside.
    let geo = SpatialRef::from_proj4("+proj=longlat +datum=WGS84").unwrap();
    for (definition, (lon, lat)) in [
        (r#"+proj="aea" +lat_1="30" +lat_2=40 +datum=WGS84"#, (0.794410772738, 2.057853143328)),
        (r#"+proj=tmerc +k="0.9996" +datum=WGS84"#, (0.899082904747, 1.809232575375)),
        (r#"+proj=tmerc +ellps=WGS84 +towgs84="1,2,3""#, (0.898740777200, 1.808535889369)),
        (r#"+proj=tmerc +title="UTM ""zone"" 32" +k=0.9996 +lon_0=9 +datum=WGS84"#, (9.899082904747, 1.809232575375)),
        // DMS seconds are not quotes, before a quoted value and after it.
        (r#"+proj=tmerc +lat_0=52d30'10"N +lon_0=5d10'20"E +k=0.9996 +x_0=500000 +datum=WGS84"#, (-0.954933652114, 54.144493283792)),
        (
            r#"+proj=tmerc +title="x y" +lat_0=52d30'10"N +lon_0=5d10'20"E +k=0.9996 +x_0=500000 +datum=WGS84"#,
            (-0.954933652114, 54.144493283792),
        ),
    ] {
        let projected = SpatialRef::from_proj4(definition).unwrap();
        let (lo, la) = Transformer::new(&projected, &geo).unwrap().transform(100_000.0, 200_000.0).unwrap();
        assert!((lo - lon).abs() < 1e-9 && (la - lat).abs() < 1e-9, "{definition}: ({lo}, {la})");
    }
}

#[test]
fn unterminated_quoted_proj_values_are_refused() {
    // GDAL 3.12.2 / PROJ 9.8.1 refuses these definitions as CRSs.
    // In particular, a malformed title must not swallow a following scale.
    for definition in [
        "+proj=tmerc +title=\"abc +k=0.9996 +datum=WGS84",
        "+proj=tmerc +datum=WGS84 +title=\"abc +k=0.9996",
        "+init=epsg:4326 +title=\"abc",
    ] {
        assert!(SpatialRef::from_proj4(definition).is_err(), "{definition}");
    }
}

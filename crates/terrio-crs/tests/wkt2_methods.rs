//! WKT 2 of five EPSG methods PROJ 9.9.0 applies is read, without its EPSG `ID`, and places points
//! where PROJ does: Hyperbolic Cassini-Soldner (9833) as `+proj=cass +hyperbolic`, Guam Projection
//! (9831) as `+proj=aeqd +guam`, Krovak Modified (1042) and its north-orientated form (1043) as
//! `+proj=mod_krovak`, Lambert Conic Conformal (2SP Michigan) (1051) as `+proj=lcc` with its
//! ellipsoid scaling factor as `+k_0`, and Colombia Urban (1052) as `+proj=col_urban` with its
//! projection plane origin height as `+h_0`. Krovak Modified's evaluation point and coefficients,
//! which PROJ's `mod_krovak` fixes, are read and ignored, as PROJ ignores them.
//!
//! `data/wkt2_methods.json` (`scripts/gen_wkt2_methods.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`)
//! holds, for each WKT, the PROJ string of its geographic CRS, a point in it, the point in the
//! WKT's CRS by `Transformer.from_crs(crs.geodetic_crs, crs, always_xy=True)`, and
//! `CRS.to_proj4()`, and its method's EPSG code. Each WKT is also read with its method given by that
//! code alone (`METHOD["custom",ID["EPSG",9833]]`).

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

#[test]
fn wkt2_methods_are_read_and_place_points_as_proj_does() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let data: Value = serde_json::from_str(include_str!("data/wkt2_methods.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 9);
    assert_eq!(data["proj_version"], "9.9.0");
    let num = |v: &Value, i: usize| v[i].as_f64().unwrap();
    let mut bad = vec![];
    // Each WKT by its method's name, and by its method's code alone.
    let readings = cases.iter().flat_map(|case| {
        let wkt = case["wkt"].as_str().unwrap();
        let start = wkt.find("METHOD[\"").unwrap();
        let end = start + wkt[start..].find(']').unwrap();
        let by_code = format!("{}METHOD[\"custom\",ID[\"EPSG\",{}]]{}", &wkt[..start], case["method_code"], &wkt[end + 1..]);
        let name = case["name"].as_str().unwrap();
        [(name.to_string(), wkt.to_string(), case), (format!("{name} (by its method's code)"), by_code, case)]
    });
    for (name, wkt, case) in readings {
        let crs = SpatialRef::from_wkt(&wkt);
        let Some(proj) = crs.to_proj4() else {
            bad.push(format!("{name}: not read"));
            continue;
        };
        // The method and its flag, as PROJ writes them.
        let head =
            |p: &str| p.split_whitespace().filter(|t| t.starts_with("+proj=") || !t.contains('=')).map(String::from).collect::<Vec<_>>();
        let expected = case["proj4"].as_str().unwrap();
        if head(&proj).iter().filter(|t| *t != "+no_defs").ne(head(expected).iter().filter(|t| *t != "+no_defs")) {
            bad.push(format!("{name}: {proj}, PROJ {expected}"));
        }
        let src = SpatialRef::from_proj4(case["geographic"].as_str().unwrap()).unwrap();
        let (x, y) = Transformer::new(&src, &crs).unwrap().transform(num(&case["point"], 0), num(&case["point"], 1)).unwrap();
        let (ex, ey) = (num(&case["proj"], 0), num(&case["proj"], 1));
        if (x - ex).abs() > 1e-3 || (y - ey).abs() > 1e-3 {
            bad.push(format!("{name}: ({x:.3}, {y:.3}), PROJ ({ex:.3}, {ey:.3})"));
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

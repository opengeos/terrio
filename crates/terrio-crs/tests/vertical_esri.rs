//! ESRI's vertical CRS (`VERTCS`) is read as a vertical CRS, as PROJ 9.9.0 reads it
//! (opengeos/terrio#1012, item 59): a code it declares is kept when its unit and direction are the
//! code's, its axis points down by `PARAMETER["Direction",-1]`, and it is the vertical CRS of a
//! `COMPD_CS` and of ESRI's compound CRS, a `GEOGCS` or `PROJCS` followed by a `VERTCS` (ESRI's
//! WKT of EPSG:9707). A `VERT_CS` after a `GEOGCS`, or a `VERTCS` after a WKT 2 root, is ignored,
//! as by PROJ.
//!
//! `data/vertical_esri.json` (`scripts/gen_vertical_esri.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`, no
//! grids installed) holds the type, code and axes PROJ reads, and for a compound CRS the point PROJ
//! places from EPSG:9707 (`always_xy`). A compound CRS has no code in terrio unless it declares
//! one, where PROJ identifies EPSG:9707 by its parts (`SpatialRef::from_wkt`). Tolerance: 1 mm
//! (1e-8° for a geographic CRS).

use serde_json::Value;
use terrio_crs::{AxisOrder, SpatialRef, Transformer};

#[test]
fn an_esri_vertcs_is_a_vertical_crs() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let data: Value = serde_json::from_str(include_str!("data/vertical_esri.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 14);
    let point: Vec<f64> = data["point"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
    let from = SpatialRef::from_epsg(9707).unwrap();
    for case in cases {
        let name = case["name"].as_str().unwrap();
        let crs = SpatialRef::from_wkt(case["wkt"].as_str().unwrap());
        let axes: Vec<&str> = case["axes"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        match case["type"].as_str().unwrap() {
            "Vertical CRS" => {
                assert_eq!(crs.epsg.map(u64::from), case["epsg"].as_u64(), "{name}");
                assert_eq!(crs.axis_order(), AxisOrder::Other, "{name}");
                assert!(!crs.has_height(), "{name}");
            }
            "Compound CRS" => {
                assert_eq!(crs.epsg, None, "{name}");
                assert!(crs.has_height(), "{name}");
                assert_eq!(crs.axis_order(), AxisOrder::EastNorth, "{name}: {axes:?}");
                let p = case["proj"].as_array().unwrap();
                let (x, y, z) = Transformer::new(&from, &crs).and_then(|t| t.transform_3d(point[0], point[1], point[2])).unwrap();
                let (px, py, pz) = (p[0].as_f64().unwrap(), p[1].as_f64().unwrap(), p[2].as_f64().unwrap());
                let tolerance = if crs.is_geographic() { 1e-8 } else { 1e-3 };
                assert!((x - px).abs() < tolerance && (y - py).abs() < tolerance && (z - pz).abs() < 1e-3, "{name}: ({x}, {y}, {z}), PROJ ({px}, {py}, {pz})");
            }
            _ => {
                assert_eq!(crs.epsg.map(u64::from), case["epsg"].as_u64(), "{name}");
                assert!(!crs.has_height(), "{name}");
                assert_eq!(axes, ["north", "east"], "{name}");
                assert_eq!(crs.axis_order(), AxisOrder::NorthEast, "{name}");
            }
        }
    }
    // A declared vertical code makes the compound CRS's parts known.
    let declared = cases.iter().find(|c| c["name"] == "GEOGCS and VERTCS declaring EPSG:5773").unwrap();
    assert_eq!(SpatialRef::from_wkt(declared["wkt"].as_str().unwrap()).compound_parts(), Some((4326, 5773)));
}

/// ESRI's compound CRS is named after its parts, and written as a `COMPD_CS`.
#[test]
fn an_esri_compound_crs_is_a_compd_cs() {
    let g = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
    let v = r#"VERTCS["EGM96_height",VDATUM["EGM96_Geoid"],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",1.0],UNIT["Meter",1.0]]"#;
    let crs = SpatialRef::from_wkt(&format!("{g},{v}"));
    assert_eq!(crs.name().as_deref(), Some("GCS_WGS_1984 + EGM96_height"));
    let written = crs.to_wkt().unwrap();
    assert!(written.starts_with(r#"COMPD_CS["GCS_WGS_1984 + EGM96_height",GEOGCS["GCS_WGS_1984""#), "{written}");
    assert!(written.contains(r#"VERTCS["EGM96_height""#), "{written}");
}

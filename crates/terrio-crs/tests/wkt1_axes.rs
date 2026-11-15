//! A projected CRS given by WKT has the axes of its `AXIS` nodes, as PROJ 9.8.1 and GDAL 3.10
//! read them, rather than east and north: `AXIS["Y",WEST],AXIS["X",SOUTH]` on Transverse
//! Mercator puts 18.5°E 33.9°S at (46246.675, 3752681.842), not (-46246.675, -3752681.842).
//!
//! `data/wkt1_axes.json` (`scripts/gen_wkt1_axes.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`, no grids)
//! holds, for Transverse Mercator, south-orientated Transverse Mercator, Lambert Conformal Conic
//! and Krovak with each order of axes, as WKT 1 and as PROJ's WKT 2 of it:
//! - PROJ's coordinates of a point (`always_xy`, GDAL's traditional order, which reads northing
//!   then easting as easting then northing), within 1 mm;
//! - the `+axis` of PROJ's `to_proj4()`: none for a coordinate system's axes, those of
//!   south-orientated Transverse Mercator (`wsu`) and of Krovak's south and west (`swu`).

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

#[test]
fn projected_axes_are_read_and_written_as_proj_does() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
    let data: Value = serde_json::from_str(include_str!("data/wkt1_axes.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 36);
    let num = |v: &Value, i: usize| v[i].as_f64().unwrap();
    for case in cases {
        let src = SpatialRef::parse(case["geographic"].as_str().unwrap()).unwrap();
        let (lon, lat) = (num(&case["point"], 0), num(&case["point"], 1));
        for (key, expected) in [("wkt", "proj"), ("wkt2", "proj_wkt2")] {
            let wkt = case[key].as_str().unwrap();
            let (x, y) = Transformer::new(&src, &SpatialRef::from_wkt(wkt)).unwrap().transform(lon, lat).unwrap();
            let (ex, ey) = (num(&case[expected], 0), num(&case[expected], 1));
            assert!((x - ex).abs() < 1e-3 && (y - ey).abs() < 1e-3, "{wkt}: ({x}, {y}), PROJ ({ex}, {ey})");
        }
        let wkt = case["wkt"].as_str().unwrap();
        let proj = SpatialRef::from_wkt(wkt).to_proj4().unwrap();
        let axis = proj.split_whitespace().find(|t| t.starts_with("+axis="));
        assert_eq!(axis, case["proj4_axis"].as_str(), "{wkt}: {proj}");
    }
}

/// A WKT 2 CRS keeps the axes of its PROJ string: those of a geographic CRS with west and north
/// axes, and those of south-orientated Transverse Mercator given by its EPSG method code under
/// the name `custom`.
#[test]
fn wkt2_axes_survive_export_by_geographic_axes_and_method_code() {
    let geographic = r#"GEOGCRS["Mars West",DATUM["Mars",ELLIPSOID["Mars",3396190,169.8944472236118,LENGTHUNIT["metre",1]]],PRIMEM["Reference_Meridian",0,ANGLEUNIT["degree",0.0174532925199433]],CS[ellipsoidal,2],AXIS["geodetic longitude (Lon)",west,ORDER[1]],AXIS["geodetic latitude (Lat)",north,ORDER[2]],ANGLEUNIT["degree",0.0174532925199433]]"#;
    let proj = SpatialRef::from_wkt(geographic).to_proj4().unwrap();
    assert!(proj.split_whitespace().any(|t| t == "+axis=wnu"), "{geographic}: {proj}");

    let data: Value = serde_json::from_str(include_str!("data/wkt1_axes.json")).unwrap();
    let case = data["cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["wkt"].as_str().unwrap().starts_with(r#"PROJCS["Transverse_Mercator_South_Orientated none""#))
        .unwrap();
    let custom = case["wkt2"].as_str().unwrap().replace(r#"METHOD["Transverse Mercator (South Orientated)""#, r#"METHOD["custom""#);
    assert!(custom.contains(r#"METHOD["custom",ID["EPSG",9808]]"#), "{custom}");
    let proj = SpatialRef::from_wkt(&custom).to_proj4().unwrap();
    assert!(proj.split_whitespace().any(|t| t == "+axis=wsu"), "{custom}: {proj}");
}

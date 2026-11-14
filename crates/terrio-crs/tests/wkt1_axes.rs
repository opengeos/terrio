//! A projected CRS given by WKT 1 has the axes of its `AXIS` nodes, as PROJ 9.8.1 and GDAL 3.10
//! read them, rather than east and north: `AXIS["Y",WEST],AXIS["X",SOUTH]` on Transverse
//! Mercator puts 18.5°E 33.9°S at (46246.675, 3752681.842), not (-46246.675, -3752681.842).
//!
//! `data/wkt1_axes.json` (`scripts/gen_wkt1_axes.py`, pyproj 3.8.0, `PROJ_NETWORK=OFF`, no grids)
//! holds PROJ's coordinates (`always_xy`, GDAL's traditional order, which reads northing then
//! easting as easting then northing) for Transverse Mercator, south-orientated Transverse
//! Mercator, Lambert Conformal Conic and Krovak with each order of axes. Tolerance: 1 mm.

use serde_json::Value;
use terrio_crs::{SpatialRef, Transformer};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

#[test]
fn projected_axes_are_read_as_proj_reads_them() {
    offline();
    let data: Value = serde_json::from_str(include_str!("data/wkt1_axes.json")).unwrap();
    let cases = data["cases"].as_array().unwrap();
    assert_eq!(cases.len(), 32);
    for case in cases {
        let wkt = case["wkt"].as_str().unwrap();
        let src = SpatialRef::parse(case["geographic"].as_str().unwrap()).unwrap();
        let t = Transformer::new(&src, &SpatialRef::from_wkt(wkt)).unwrap();
        let p = |v: &Value, i: usize| v[i].as_f64().unwrap();
        let (x, y) = t.transform(p(&case["point"], 0), p(&case["point"], 1)).unwrap();
        let (ex, ey) = (p(&case["proj"], 0), p(&case["proj"], 1));
        assert!((x - ex).abs() < 1e-3 && (y - ey).abs() < 1e-3, "{wkt}: ({x}, {y}), PROJ ({ex}, {ey})");
    }
}

/// The same in WKT 2, which terrio read so already, and PROJ's PROJ strings: the axes of a
/// coordinate system are not written (`+proj=tmerc` for west and south axes), those of
/// south-orientated Transverse Mercator are, as `+axis=wsu` whatever their order (PROJ 9.8.1's
/// `to_proj4()`).
#[test]
fn proj_strings_carry_the_axes_proj_writes() {
    offline();
    let data: Value = serde_json::from_str(include_str!("data/wkt1_axes.json")).unwrap();
    let wkt = |name: &str| {
        let case = data["cases"].as_array().unwrap().iter().find(|c| c["wkt"].as_str().unwrap().contains(name)).unwrap();
        SpatialRef::from_wkt(case["wkt"].as_str().unwrap())
    };
    let proj = |s: SpatialRef| s.to_proj4().unwrap();
    assert!(!proj(wkt("\"Transverse_Mercator west south\"")).contains("+axis"));
    assert!(!proj(wkt("\"Lambert_Conformal_Conic_2SP south west\"")).contains("+axis"));
    assert!(proj(wkt("\"Transverse_Mercator_South_Orientated north east\"")).starts_with("+proj=tmerc +axis=wsu "));

    let lo19 = r#"PROJCRS["Lo19 west south",BASEGEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433]]],CONVERSION["Lo19",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Latitude of natural origin",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8801]],PARAMETER["Longitude of natural origin",19,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",1,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",0,LENGTHUNIT["metre",1],ID["EPSG",8806]],PARAMETER["False northing",0,LENGTHUNIT["metre",1],ID["EPSG",8807]]],CS[Cartesian,2],AXIS["westing (Y)",west,ORDER[1],LENGTHUNIT["metre",1]],AXIS["southing (X)",south,ORDER[2],LENGTHUNIT["metre",1]]]"#;
    let lo19 = SpatialRef::from_wkt(lo19);
    let t = Transformer::new(&SpatialRef::parse("+proj=longlat +datum=WGS84").unwrap(), &lo19).unwrap();
    let (x, y) = t.transform(18.5, -33.9).unwrap();
    // PROJ 9.8.1: (46246.675, 3752681.842) as for the WKT 1, written `+proj=tmerc` without axes.
    assert!((x - 46246.675).abs() < 1e-3 && (y - 3752681.842).abs() < 1e-3, "{x} {y}");
    assert!(!proj(lo19).contains("+axis"));
}

// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! WKT axes and parameter units read as PROJ 9.9.0 reads them (`PROJ_NETWORK=OFF`, no grids,
//! `always_xy`): a WKT 1 `GEOGCS`'s `AXIS` directions (#1012 item 26), a WKT 2 parameter without
//! a unit in degrees or metres (item 27), and South-orientated Transverse Mercator in WKT 1
//! westing first whatever its `AXIS` nodes (item 46).

use terrio_crs::{SpatialRef, Transformer};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

fn transform(src: &str, dst: &str, x: f64, y: f64) -> (f64, f64) {
    let t = Transformer::new(&SpatialRef::parse(src).unwrap(), &SpatialRef::parse(dst).unwrap()).unwrap();
    t.transform(x, y).unwrap()
}

const WGS84_DATUM: &str =
    r#"DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]"#;

#[test]
fn a_wkt1_geographic_crs_follows_its_axis_directions() {
    offline();
    // (12, 50) in the CRS into WGS 84, PROJ 9.9.0: west and south negate, in the axes' order.
    for (axes, expected) in [
        (r#"AXIS["Lon",WEST],AXIS["Lat",NORTH]"#, (-12.0, 50.0)),
        (r#"AXIS["Lon",EAST],AXIS["Lat",NORTH]"#, (12.0, 50.0)),
        (r#"AXIS["Lat",NORTH],AXIS["Lon",EAST]"#, (12.0, 50.0)),
        (r#"AXIS["Lat",SOUTH],AXIS["Lon",EAST]"#, (50.0, -12.0)),
        (r#"AXIS["Lon",EAST],AXIS["Lat",SOUTH]"#, (12.0, -50.0)),
    ] {
        let crs = format!(r#"GEOGCS["x",{WGS84_DATUM},{axes}]"#);
        let (x, y) = transform(&crs, "EPSG:4326", 12.0, 50.0);
        assert!((x - expected.0).abs() < 1e-9 && (y - expected.1).abs() < 1e-9, "{axes}: {x} {y}");
    }
    // West-positive planetary longitudes (Mars), and back from WGS 84.
    let mars =
        r#"GEOGCS["Mars",DATUM["Mars",SPHEROID["Mars",3396190,169.894447223612]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]"#;
    let (x, _) = transform(&format!(r#"{mars},AXIS["Lon",WEST],AXIS["Lat",NORTH]]"#), &format!("{mars}]"), 12.0, 50.0);
    assert!((x + 12.0).abs() < 1e-9, "{x}");
    let (x, _) = transform("EPSG:4326", &format!(r#"GEOGCS["x",{WGS84_DATUM},AXIS["Lon",WEST],AXIS["Lat",NORTH]]"#), 12.0, 50.0);
    assert!((x + 12.0).abs() < 1e-9, "{x}");
}

#[test]
fn wkt2_parameters_without_a_unit_are_in_degrees_and_metres() {
    offline();
    let base = |unit: &str| {
        format!(
            r#"BASEGEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]],PRIMEM["Greenwich",0,ANGLEUNIT[{unit}]],ANGLEUNIT[{unit}]]"#
        )
    };
    let cs = |unit: &str| {
        format!(r#"CS[Cartesian,2],AXIS["easting",east,ORDER[1],LENGTHUNIT[{unit}]],AXIS["northing",north,ORDER[2],LENGTHUNIT[{unit}]]"#)
    };
    let tm = r#"CONVERSION["tm",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Latitude of natural origin",0],PARAMETER["Longitude of natural origin",10],PARAMETER["Scale factor at natural origin",1],PARAMETER["False easting",100000],PARAMETER["False northing",0]]"#;
    let (degree, grad) = (r#""degree",0.0174532925199433"#, r#""grad",0.015707963267949"#);
    let (metre, foot) = (r#""metre",1"#, r#""US survey foot",0.304800609601219"#);
    // PROJ 9.9.0: a false easting of 100000 m in a US survey foot CS, and a longitude of origin of
    // 10° under a grad base CRS (where terrio read 30480 m and 9°).
    for (base_unit, cs_unit, (x, y), expected) in [(degree, foot, (36000.0, 0.0), 9.200281395407739), (grad, metre, (100000.0, 0.0), 10.0)]
    {
        let crs = format!(r#"PROJCRS["p",{},{tm},{}]"#, base(base_unit), cs(cs_unit));
        let (lon, lat) = transform(&crs, "EPSG:4326", x, y);
        assert!((lon - expected).abs() < 1e-9 && lat.abs() < 1e-9, "{crs}: {lon} {lat}");
    }
}

#[test]
fn south_orientated_transverse_mercator_in_wkt1_is_westing_first() {
    offline();
    // PROJ 9.9.0, WGS 84 (18.5°E, 33.9°S) at `central_meridian` 19: westing then southing whatever
    // the WKT 1 axes, but northing then easting read as GDAL's traditional GIS order (as PROJ 9.9.0
    // with `always_xy`).
    let projcs = |axes: &str| {
        format!(
            r#"PROJCS["x",GEOGCS["WGS 84",{WGS84_DATUM}],PROJECTION["Transverse_Mercator_South_Orientated"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",19],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1]{axes}]"#
        )
    };
    let (w, s) = (46246.67499191615, 3752681.842487887);
    for (axes, expected) in [
        (r#",AXIS["Northing",NORTH],AXIS["Easting",EAST]"#, (s, w)),
        (r#",AXIS["Westing",WEST],AXIS["Southing",SOUTH]"#, (w, s)),
        (r#",AXIS["Easting",EAST],AXIS["Northing",NORTH]"#, (w, s)),
        ("", (w, s)),
    ] {
        let (x, y) = transform("EPSG:4326", &projcs(axes), 18.5, -33.9);
        assert!((x - expected.0).abs() < 1e-3 && (y - expected.1).abs() < 1e-3, "{axes}: {x} {y}");
    }
}

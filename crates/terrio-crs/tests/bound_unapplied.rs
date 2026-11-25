// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! A WKT 2 `BOUNDCRS` whose transformation terrio does not apply warns through each transformer
//! using it (its warning handler, by default `take_warnings`'s; #1012 item 12). The points are
//! then placed without it: by the source CRS's own transformations when it has a code, else by
//! the ballpark. Where PROJ 9.9.0 does not apply the transformation either (a target bound
//! itself), terrio places them where PROJ does, with the network off and no grids: from an
//! unknown Bessel datum, (10°, 50°) stays (10°, 50°).

use terrio_crs::{SpatialRef, Transformer, take_warnings};

const SOURCE: &str = r#"GEOGCRS["Bessel",DATUM["Unknown based on Bessel",ELLIPSOID["Bessel 1841",6377397.155,299.1528128,LENGTHUNIT["metre",1]]],CS[ellipsoidal,2],AXIS["latitude",north,ANGLEUNIT["degree",0.0174532925199433]],AXIS["longitude",east,ANGLEUNIT["degree",0.0174532925199433]]]"#;
const ED50: &str = r#"GEOGCRS["ED50",DATUM["European Datum 1950",ELLIPSOID["International 1924",6378388,297,LENGTHUNIT["metre",1]]],CS[ellipsoidal,2],AXIS["latitude",north,ANGLEUNIT["degree",0.0174532925199433]],AXIS["longitude",east,ANGLEUNIT["degree",0.0174532925199433]],ID["EPSG",4230]]"#;
const WGS84: &str = r#"GEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]],CS[ellipsoidal,2],AXIS["latitude",north,ANGLEUNIT["degree",0.0174532925199433]],AXIS["longitude",east,ANGLEUNIT["degree",0.0174532925199433]],ID["EPSG",4326]]"#;
const ED50_WKT1: &str = r#"GEOGCS["ED50",DATUM["European_Datum_1950",SPHEROID["International 1924",6378388,297]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],AUTHORITY["EPSG","4230"]]"#;
const ED50_WKT1_TOWGS84: &str = r#"GEOGCS["ED50",DATUM["European_Datum_1950",SPHEROID["International 1924",6378388,297],TOWGS84[-80,-90,-110,0,0,0,0]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;

fn translations(name: &str, x: f64, y: f64, z: f64) -> String {
    format!(
        r#"ABRIDGEDTRANSFORMATION["{name}",METHOD["Geocentric translations (geog2D domain)",ID["EPSG",9603]],PARAMETER["X-axis translation",{x},ID["EPSG",8605]],PARAMETER["Y-axis translation",{y},ID["EPSG",8606]],PARAMETER["Z-axis translation",{z},ID["EPSG",8607]]]"#
    )
}

fn bound(source: &str, target: &str, transformation: &str) -> SpatialRef {
    SpatialRef::from_wkt(&format!("BOUNDCRS[SOURCECRS[{source}],TARGETCRS[{target}],{transformation}]"))
}

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

/// The point (10°, 50°) into `code`, and the warnings raised, by a new transformer.
fn at(src: &SpatialRef, code: u32) -> ((f64, f64), Vec<String>) {
    let _ = take_warnings();
    let t = Transformer::new(src, &SpatialRef::from_epsg(code).unwrap()).unwrap();
    let p = t.transform(10.0, 50.0).unwrap();
    t.flush_warnings();
    (p, take_warnings().into_iter().filter(|w| w.starts_with("the BOUNDCRS")).collect())
}

/// A target bound itself: PROJ 9.9.0 links the CRSs by its ballpark ("Ballpark geographic offset
/// from Bessel to WGS 84"), into ED50 too, where terrio chained the shift through the target.
/// A WKT 1 target with its own `TOWGS84` is such a target to PROJ, and terrio does not read it.
#[test]
fn a_target_bound_itself_is_not_chained_through_and_warns() {
    offline();
    let nested = format!("BOUNDCRS[SOURCECRS[{ED50}],TARGETCRS[{WGS84}],{}]", translations("ED50 shift", -87.0, -98.0, -121.0));
    let shift = translations("shift", 100.0, 50.0, -20.0);
    for (target, why) in [
        (nested.as_str(), "its target CRS \"ED50\" is bound to another CRS itself, which PROJ does not chain through either"),
        (ED50_WKT1_TOWGS84, "its target CRS \"ED50\" is not read"),
    ] {
        let src = bound(SOURCE, target, &shift);
        for code in [4326, 4230, 4258] {
            let (p, warnings) = at(&src, code);
            assert_eq!(p, (10.0, 50.0), "into EPSG:{code}");
            assert_eq!(warnings, [format!("the BOUNDCRS transformation \"shift\" is not applied: {why}")], "into EPSG:{code}");
        }
    }
}

/// A WKT 1 target without `TOWGS84` is read, as in WKT 2: PROJ 9.9.0 applies the shift and the
/// target's transformation (9.999316°, 49.999745° into WGS 84) (#1012 item 53).
#[test]
fn a_wkt1_target_is_read_as_in_wkt2() {
    offline();
    let shift = translations("shift", 100.0, 50.0, -20.0);
    for target in [ED50_WKT1, ED50] {
        let ((x, y), warnings) = at(&bound(SOURCE, target, &shift), 4326);
        assert!((x - 9.999315576959662).abs() < 1e-9 && (y - 49.9997449986732).abs() < 1e-9, "{x} {y}");
        assert!(warnings.is_empty(), "{warnings:?}");
    }
}

/// A transformation that is not a Helmert one (NTv2) is not applied, and warns. PROJ 9.9.0 applies
/// the grid, and refuses the transformation when the grid is missing.
#[test]
fn a_grid_transformation_is_not_applied_and_warns() {
    offline();
    let grid = r#"ABRIDGEDTRANSFORMATION["grid",METHOD["NTv2",ID["EPSG",9615]],PARAMETERFILE["Latitude and longitude difference file","BETA2007.gsb"]]"#;
    let src = bound(SOURCE, WGS84, grid);
    // Each transformer warns: the definition is built once and shared.
    for _ in 0..2 {
        let (p, warnings) = at(&src, 4326);
        assert_eq!(p, (10.0, 50.0));
        assert_eq!(
            warnings,
            ["the BOUNDCRS transformation \"grid\" is not applied: terrio applies Helmert transformations only, not \"NTv2\""]
        );
    }
}

fn record(message: String) {
    RECORDED.with(|r| r.borrow_mut().push(message));
}

thread_local! {
    static RECORDED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The destination CRS warns too, through the transformer's own warning handler when it has one.
#[test]
fn a_destination_warns_too_through_the_handler() {
    offline();
    let grid = r#"ABRIDGEDTRANSFORMATION["grid",METHOD["NTv2",ID["EPSG",9615]],PARAMETERFILE["Latitude and longitude difference file","BETA2007.gsb"]]"#;
    let (wgs84, src) = (SpatialRef::from_epsg(4326).unwrap(), bound(SOURCE, WGS84, grid));
    let _ = take_warnings();
    // Dropped without a point: the warning is flushed then.
    drop(Transformer::new(&wgs84, &src).unwrap());
    let warned = |w: &Vec<String>| w.iter().any(|w| w.starts_with("the BOUNDCRS transformation \"grid\" is not applied"));
    assert!(warned(&take_warnings()));
    drop(Transformer::new(&wgs84, &src).unwrap().with_warning_handler(record));
    assert!(!warned(&take_warnings()));
    assert!(RECORDED.with(|r| warned(&r.borrow())));
}

/// A source CRS with an EPSG code (ED50, EPSG:4230) is read from the registry, with the bound
/// CRS's reason kept: it warns as a source without a code does. Its points go by the code's own
/// transformations, as PROJ 9.9.0 places them: (9.998871°, 49.999200°) into WGS 84 by "ED50 to
/// WGS 84 (17)".
#[test]
fn a_source_with_a_code_warns_too() {
    offline();
    let nested = format!("BOUNDCRS[SOURCECRS[{ED50}],TARGETCRS[{WGS84}],{}]", translations("ED50 shift", -87.0, -98.0, -121.0));
    let src = bound(ED50, &nested, &translations("shift", 100.0, 50.0, -20.0));
    let ((x, y), warnings) = at(&src, 4326);
    assert!((x - 9.998871090641538).abs() < 1e-9 && (y - 49.999200164761284).abs() < 1e-9, "{x} {y}");
    assert_eq!(
        warnings,
        [
            "the BOUNDCRS transformation \"shift\" is not applied: its target CRS \"ED50\" is bound to another CRS itself, which PROJ does not chain through either"
        ]
    );
}

/// A source CRS bound itself whose transformation is left out, under a transformation left out
/// too: the warning gives both reasons, the source's first.
#[test]
fn a_bound_source_keeps_its_own_reason() {
    offline();
    let grid = r#"ABRIDGEDTRANSFORMATION["grid",METHOD["NTv2",ID["EPSG",9615]],PARAMETERFILE["Latitude and longitude difference file","BETA2007.gsb"]]"#;
    let inner = format!("BOUNDCRS[SOURCECRS[{SOURCE}],TARGETCRS[{WGS84}],{grid}]");
    let src = bound(&inner, ED50_WKT1_TOWGS84, &translations("shift", 100.0, 50.0, -20.0));
    let (_, warnings) = at(&src, 4326);
    assert_eq!(
        warnings,
        [concat!(
            "the BOUNDCRS transformation \"grid\" is not applied: terrio applies Helmert transformations only, not \"NTv2\"; ",
            "the BOUNDCRS transformation \"shift\" is not applied: its target CRS \"ED50\" is not read"
        )]
    );
}

/// Names from the WKT are shown escaped and bounded, as grid names are: an escape sequence in a
/// transformation's name does not reach the terminal.
#[test]
fn names_in_the_warning_are_escaped() {
    offline();
    let grid = r#"ABRIDGEDTRANSFORMATION["a\u{1b}[31mb",METHOD["NTv2",ID["EPSG",9615]],PARAMETERFILE["Latitude and longitude difference file","BETA2007.gsb"]]"#
        .replace("\\u{1b}", "\u{1b}");
    let (_, warnings) = at(&bound(SOURCE, WGS84, &grid), 4326);
    assert_eq!(
        warnings,
        ["the BOUNDCRS transformation \"a\\u{1b}[31mb\" is not applied: terrio applies Helmert transformations only, not \"NTv2\""]
    );
    let long = translations(&"x".repeat(300), 100.0, 50.0, -20.0);
    let (_, warnings) = at(&bound(SOURCE, ED50_WKT1_TOWGS84, &long), 4326);
    assert_eq!(
        warnings,
        [format!("the BOUNDCRS transformation \"{}\" is not applied: its target CRS \"ED50\" is not read", "x".repeat(120))]
    );
}

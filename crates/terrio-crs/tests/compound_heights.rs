// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Points between EPSG compound CRSs and ellipsoidal heights, without grids, are placed as PROJ
//! 9.9.0 places them (#1012 item 47).
//!
//! Without a geoid model's grid, PROJ goes between ellipsoidal heights and a vertical CRS by its
//! ballpark vertical transformation, which keeps the height. Chained with it, every horizontal
//! transformation is of unknown accuracy, so PROJ uses the one of the smallest area that holds the
//! point: from WGS 84 (3D) into OSGB36 / British National Grid + ODN height (EPSG:7405), it places
//! (-2°, 54°) by OSGB36 to WGS 84 (3), 5.2 m from (6), the most accurate. From WGS 84 (2D), which
//! has no heights, there is no vertical transformation, and (6) is used.
//!
//! `data/compound_heights.tsv` (`scripts/gen_compound_heights.py`, PROJ 9.9.0, no grids,
//! `PROJ_NETWORK=OFF`): every EPSG compound CRS that is not deprecated and has an area of use
//! (and that PROJ places), from WGS 84 (3D) at the
//! centre of its area at 100 m and back; for a sample, from WGS 84 (geocentric) and WGS 84 (2D),
//! and in 2D; and between compound CRSs of one horizontal CRS. Each case is a point and where
//! `Transformer.from_crs(src, dst, always_xy=True).transform` places it. Tolerance: 1 mm.
//!
//! PROJ runs the only transformation between two vertical CRSs outside its area too, where it holds
//! the compound CRSs' area of interest and so is listed alone, as `proj_trans` checks no area
//! when there is one operation: Belfast height to Malin Head height (1) (a 0.037 m offset, for
//! Northern Ireland) at the centre of Ireland (#1012 item 69).
use terrio_crs::{SpatialRef, TransformOptions, Transformer};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

fn transformer(src: &str, dst: &str, opts: &TransformOptions) -> Result<Transformer, terrio_crs::Error> {
    Transformer::with_options(&SpatialRef::parse(src).unwrap(), &SpatialRef::parse(dst).unwrap(), opts)
}

/// The distance between two horizontal positions, in metres: near enough at a millimetre for
/// longitudes and latitudes.
fn metres(a: (f64, f64), b: (f64, f64), geographic: bool) -> f64 {
    if geographic { ((a.0 - b.0) * b.1.to_radians().cos()).hypot(a.1 - b.1) * 111_195.0 } else { (a.0 - b.0).hypot(a.1 - b.1) }
}

#[test]
fn compound_crss_are_placed_as_proj_places_them() {
    offline();
    let mut checked = 0;
    let mut wrong = Vec::new();
    for line in include_str!("data/compound_heights.tsv").lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let num = |s: &str| (!s.is_empty()).then(|| s.parse::<f64>().unwrap());
        let (src, dst) = (f[0], f[1]);
        let (x, y, z) = (num(f[2]).unwrap(), num(f[3]).unwrap(), num(f[4]));
        let (ex, ey, ez) = (num(f[5]).unwrap(), num(f[6]).unwrap(), f.get(7).and_then(|s| num(s)));
        let t = transformer(src, dst, &TransformOptions::default()).unwrap();
        let geographic = SpatialRef::parse(dst).unwrap().is_geographic();
        let (got, height) = match z {
            Some(z) => t.transform_3d(x, y, z).map(|(a, b, c)| ((a, b), Some(c))),
            None => t.transform(x, y).map(|p| (p, None)),
        }
        .unwrap_or_else(|e| panic!("{src} -> {dst} ({x}, {y}): {e}"));
        assert_eq!(height.is_some(), ez.is_some(), "{src} -> {dst} returned a different height dimension");
        let d = metres(got, (ex, ey), geographic);
        let dz = height.zip(ez).map_or(0.0, |(a, b)| (a - b).abs());
        assert!(d.is_finite() && dz.is_finite(), "{src} -> {dst} returned non-finite coordinates: {got:?}, {height:?}");
        if d > 1e-3 || dz > 1e-3 {
            wrong.push(format!("{src} -> {dst} ({x}, {y}, {z:?}): {d:.4} m, height {dz:.4} m"));
        }
        checked += 1;
    }
    assert!(checked > 1_200, "{checked}");
    assert!(wrong.is_empty(), "{} of {checked} placed otherwise than PROJ:\n{}", wrong.len(), wrong.join("\n"));
}

#[test]
fn heights_through_the_ballpark_make_every_transformation_of_unknown_accuracy() {
    offline();
    // PROJ 9.9.0 from WGS 84 (3D): OSGB36 to WGS 84 (3), whose area is the smallest that holds the
    // point; in 2D too.
    let t = transformer("EPSG:4979", "EPSG:7405", &TransformOptions::default()).unwrap();
    let (x, y, z) = t.transform_3d(-2.0, 54.0, 100.0).unwrap();
    assert!((x - 400097.94481535204).abs() < 1e-3 && (y - 455991.2596765667).abs() < 1e-3 && z == 100.0, "{x} {y} {z}");
    let (x, y) = t.transform(-2.0, 54.0).unwrap();
    assert!((x - 400097.94634777925).abs() < 1e-3 && (y - 455991.25926194433).abs() < 1e-3, "{x} {y}");
    let op = t.operation_at(-2.0, 54.0, 100.0).unwrap().unwrap();
    assert!(op.ballpark && op.accuracy.is_none(), "{op:?}");
    // From WGS 84 (2D), which has no heights: OSGB36 to WGS 84 (6), the most accurate.
    let t = transformer("EPSG:4326", "EPSG:7405", &TransformOptions::default()).unwrap();
    let (x, y, _) = t.transform_3d(-2.0, 54.0, 100.0).unwrap();
    assert!((x - 400097.64467714215).abs() < 1e-3 && (y - 455996.5036129216).abs() < 1e-3, "{x} {y}");
    let op = t.operation_at(-2.0, 54.0, 100.0).unwrap().unwrap();
    assert_eq!(op.accuracy, Some(2.0), "{op:?}");
}

#[test]
fn heights_through_the_ballpark_are_refused_with_it() {
    offline();
    // PROJ 9.9.0 finds no operation with ALLOW_BALLPARK=NO, and places none with ONLY_BEST=YES.
    for (dst, lon, lat) in [("EPSG:7405", -2.0, 54.0), ("EPSG:5707", 1.68, 49.64)] {
        let refused = TransformOptions { allow_ballpark: Some(false), ..Default::default() };
        assert!(transformer("EPSG:4979", dst, &refused).is_err(), "{dst}");
        let best = TransformOptions { only_best: true, ..Default::default() };
        let t = transformer("EPSG:4979", dst, &best).unwrap();
        assert!(t.transform_3d(lon, lat, 100.0).is_err(), "{dst}");
    }
}

#[test]
fn registered_no_step_geoid_models_report_their_accuracy() {
    offline();
    // EPSG operation 4679, Saba to Saba height (1), is a registered no-step model with 0.2 m
    // accuracy, not a fallback for missing grids (PROJ 9.9.0).
    for options in [TransformOptions::default(), TransformOptions { allow_ballpark: Some(false), ..Default::default() }] {
        let t = transformer("EPSG:10635", "EPSG:10643", &options).unwrap();
        let op = t.operation_at(-63.235, 17.635, 100.0).unwrap().unwrap();
        assert!(!op.ballpark, "{op:?}");
        assert_eq!(op.accuracy, Some(0.2), "{op:?}");
    }
}

#[test]
fn only_best_keeps_ballpark_height_operation_order() {
    offline();
    // Independently checked with PROJ 9.7.1, without grids: both settings use OSGB36 to WGS 84
    // (3), framed by the ballpark vertical transformation for Fao height, not (6).
    let src = SpatialRef::from_epsg(4979).unwrap();
    let dst = SpatialRef::compound(27700, 3886).unwrap();
    for only_best in [false, true] {
        let options = TransformOptions { only_best, ..Default::default() };
        let t = Transformer::with_options(&src, &dst, &options).unwrap();
        let (x, y, z) = t.transform_3d(-2.0, 54.0, 100.0).unwrap();
        assert!((x - 400097.94481535204).abs() < 1e-3 && (y - 455991.2596765667).abs() < 1e-3 && z == 100.0, "{only_best}: {x} {y} {z}");
        let op = t.operation_at(-2.0, 54.0, 100.0).unwrap().unwrap();
        assert!(op.ballpark && op.accuracy.is_none(), "{only_best}: {op:?}");
    }
}

#[test]
fn a_compound_crs_on_a_datum_without_code_is_placed_by_the_ballpark() {
    offline();
    // British National Grid's projection on a datum PROJ 9.9.0 does not know, with ODN heights:
    // no transformation, so PROJ's ballparks, from WGS 84 (3D) and (2D) alike.
    let wkt = concat!(
        r#"COMPD_CS["Local + ODN",PROJCS["Local grid",GEOGCS["Local",DATUM["Local_datum","#,
        r#"SPHEROID["Airy 1830",6377563.396,299.3249646]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],"#,
        r#"PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",49],PARAMETER["central_meridian",-2],"#,
        r#"PARAMETER["scale_factor",0.9996012717],PARAMETER["false_easting",400000],PARAMETER["false_northing",-100000],"#,
        r#"UNIT["metre",1],AXIS["Easting",EAST],AXIS["Northing",NORTH]],VERT_CS["ODN height","#,
        r#"VERT_DATUM["Ordnance Datum Newlyn",2005,AUTHORITY["EPSG","5101"]],UNIT["metre",1,AUTHORITY["EPSG","9001"]],"#,
        r#"AXIS["Gravity-related height",UP],AUTHORITY["EPSG","5701"]]]"#
    );
    let dst = SpatialRef::from_wkt(wkt);
    for src in [4979, 4326] {
        let t = Transformer::new(&SpatialRef::from_epsg(src).unwrap(), &dst).unwrap();
        let (x, y, z) = t.transform_3d(-1.0, 51.5, 100.0).unwrap();
        assert!((x - 469405.28816918284).abs() < 1e-3 && (y - 178423.54064973217).abs() < 1e-3 && z == 100.0, "{src}: {x} {y} {z}");
        let op = t.operation_at(-1.0, 51.5, 100.0).unwrap().unwrap();
        assert!(op.ballpark && op.accuracy.is_none(), "{src}: {op:?}");
    }
}

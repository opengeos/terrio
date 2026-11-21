// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! The model gives a WKT's PROJ string what the WKT reader reads from its tree (#1058 step 6):
//! [`super::wkt1_input`] against [`wkt1::Wkt1Input::read`] for every WKT 1 text of the #1066
//! corpus (each EPSG and ESRI CRS's GDAL WKT 1 and ESRI WKT 1, as PROJ writes them), and
//! [`super::wkt2_input`] against [`crate::wkt2::Wkt2Input::read`] for its WKT 2 geographic,
//! geocentric and projected CRSs; and both for malformed and unusual input the corpus does not
//! have. Until the readers take their input from the model (#1058 step 6d), this keeps the two the
//! same.
use super::oracle_tests::projjson_compare;
use super::*;

/// The tree's and the model's input of a text, compared by their `Debug` form (a `NaN` value is
/// then the same).
fn inputs(text: &str) -> Option<(String, String)> {
    let root = crate::parsed::tree(text)?;
    if crate::wkt2::handles(&root) {
        return None;
    }
    let tree = wkt1::Wkt1Input::read(&root);
    let model = read(text).as_ref().and_then(|m| wkt1_input(m, &root));
    Some((format!("{tree:?}"), format!("{model:?}")))
}

#[test]
fn every_wkt1_corpus_text_gives_its_proj_string_the_tree_s_input() {
    let corpus = projjson_compare::corpus(&[
        &include_bytes!("../../tests/data/projjson_corpus_epsg.jsonl.zst")[..],
        &include_bytes!("../../tests/data/projjson_corpus_esri.jsonl.zst")[..],
    ]);
    let (mut compared, mut read_by_both, mut differ) = (0, 0, Vec::new());
    // GDAL's and ESRI's WKT 1 (`wkt1` and `esri` rows), each of which parses.
    for (code, kind, text, _) in corpus.iter().filter(|(_, kind, _, _)| kind != "wkt2") {
        let (tree, model) = inputs(text).unwrap_or_else(|| panic!("{code} {kind} does not parse as WKT 1"));
        compared += 1;
        if tree != "None" && model != "None" {
            read_by_both += 1;
        }
        if tree != model {
            differ.push(format!("{code} {kind}\n  tree  {tree}\n  model {model}"));
        }
    }
    assert!(compared > 15_000 && read_by_both > 13_000, "{compared} WKT 1 texts, {read_by_both} read");
    assert!(differ.is_empty(), "{} of {compared} differ:\n{}", differ.len(), differ[..differ.len().min(10)].join("\n"));
}

/// What the corpus does not have: refusals, other units, prime meridians and axes, ESRI's
/// compound CRS, a geocentric CRS, parameters without a name or a number.
#[test]
fn unusual_wkt1_gives_its_proj_string_the_tree_s_input() {
    const G: &str = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563],TOWGS84[1,2,3]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;
    let projcs = |projection: &str, params: &str, tail: &str| {
        format!(r#"PROJCS["x",{G},PROJECTION["{projection}"]{params},UNIT["US survey foot",0.304800609601219]{tail}]"#)
    };
    let geogcs = |inner: &str| {
        format!(
            r#"GEOGCS["g",DATUM["d",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]{inner}]"#
        )
    };
    let esri_3d = r#",VERTCS["WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",1.0],UNIT["Meter",1.0]]"#;
    // Each text, and whether it is read.
    let cases: Vec<(String, bool)> = vec![
        // Read, with a number that is not one dropped.
        (projcs("Transverse_Mercator", r#",PARAMETER["central_meridian",9],PARAMETER["scale_factor","abc"],PARAMETER[5]"#, ""), true),
        (projcs("Transverse_Mercator", r#",PARAMETER["central_meridian",9]"#, r#",AXIS["Northing",NORTH],AXIS["Easting",EAST]"#), true),
        (projcs("Transverse_Mercator", r#",PARAMETER["central_meridian",9]"#, r#",AXIS["Easting",EAST],AXIS["Up",UP]"#), true),
        (projcs("Krovak", r#",PARAMETER["X_Scale",-1],PARAMETER["XY_Plane_Rotation",90]"#, ""), true),
        // Refused: a Hotine two-point parameter that is not a finite number, has no value or no
        // name; a polar stereographic without its latitude of origin; EPSG's variant B with a
        // parallel that is not a number; a projection without a name.
        (projcs("Hotine_Oblique_Mercator_Two_Point_Natural_Origin", r#",PARAMETER["latitude_of_point_1","abc"]"#, ""), false),
        (projcs("Hotine_Oblique_Mercator_Two_Point_Natural_Origin", r#",PARAMETER["latitude_of_point_1",NaN]"#, ""), false),
        (projcs("Hotine_Oblique_Mercator_Two_Point_Natural_Origin", r#",PARAMETER["latitude_of_point_1"]"#, ""), false),
        (projcs("Hotine_Oblique_Mercator_Two_Point_Natural_Origin", r#",PARAMETER[5]"#, ""), false),
        (projcs("Polar_Stereographic", r#",PARAMETER["central_meridian",0]"#, ""), false),
        (projcs("Polar_Stereographic_(variant_B)", r#",PARAMETER["Latitude of standard parallel","abc"]"#, ""), false),
        (projcs("", r#",PARAMETER["central_meridian",9]"#, ""), false),
        // Variant C by its code, whatever its name, and with a code in spaces.
        (r#"PROJCS["x",GEOGCS["g",DATUM["d",SPHEROID["International 1924",6378388,297]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Polar_Stereographic",AUTHORITY["EPSG","9830"]],PARAMETER["latitude_of_standard_parallel",-67],UNIT["metre",1]]"#.into(), true),
        (r#"PROJCS["x",GEOGCS["g",DATUM["d",SPHEROID["International 1924",6378388,297]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Polar_Stereographic",AUTHORITY["EPSG"," 9830 "]],PARAMETER["latitude_of_standard_parallel",-67],UNIT["metre",1]]"#.into(), true),
        // Grads and Paris; ESRI's names; a `GEOGCS` with a Cartesian CS and with an axis in
        // grads, both read as the `GEOGCS` in its own unit.
        (r#"GEOGCS["NTF (Paris)",DATUM["Nouvelle_Triangulation_Francaise_Paris",SPHEROID["Clarke 1880 (IGN)",6378249.2,293.4660212936269]],PRIMEM["Paris",2.5969213],UNIT["grad",0.01570796326794897]]"#.into(), true),
        (r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#.into(), true),
        (geogcs(",CS[Cartesian,2]"), true),
        (geogcs(r#",AXIS["Lon",EAST,UNIT["grad",0.01570796326794897]],AXIS["Lat",NORTH]"#), true),
        // A geocentric CRS in feet; ESRI's compound CRS, and its 3D form with west-north axes.
        (r#"GEOCCS["g",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563],AUTHORITY["EPSG","6326"]],PRIMEM["Greenwich",0],UNIT["foot",0.3048]]"#.into(), true),
        (format!(r#"{},VERTCS["NAVD88",VDATUM["North_American_Vertical_Datum_1988"],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",1.0],UNIT["Meter",1.0]]"#, projcs("Transverse_Mercator", "", "")), true),
        (format!("{}{esri_3d}", projcs("Transverse_Mercator", "", r#",AXIS["W",WEST],AXIS["N",NORTH]"#)), true),
        (format!("{}{}", projcs("Transverse_Mercator", "", r#",AXIS["W",WEST],AXIS["N",NORTH]"#), esri_3d.replace(r#""Direction",1.0"#, r#""Direction",-1.0"#)), true),
        // A third axis the WKT declares, unnamed: no `+axis` pair in either.
        (projcs("Transverse_Mercator", "", r#",AXIS["N",NORTH],AXIS["E",EAST],AXIS["",UP]"#), true),
    ];
    for (text, read) in &cases {
        let (tree, model) = inputs(text).unwrap();
        assert_eq!(tree, model, "{text}");
        assert_eq!(tree != "None", *read, "{text}");
    }
}

/// Where the model reads unusual WKT 1 otherwise than terrio's tree reader. They are settled
/// when the reader takes its input from the model (#1058 step 6d), each as PROJ 9.9.0 reads it
/// (pyproj 3.8.0, `CRS(wkt).to_proj4()`).
#[test]
fn where_the_model_reads_unusual_wkt1_otherwise() {
    let geogcs = |datum: &str, inner: &str| {
        format!(r#"GEOGCS["g",DATUM["d",{datum}],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]{inner}]"#)
    };
    let wgs84 = r#"SPHEROID["WGS 84",6378137,298.257223563]"#;
    let projcs = |unit: &str, axes: &str| {
        format!(r#"PROJCS["x",{},PROJECTION["Transverse_Mercator"],PARAMETER["central_meridian",9],{unit}{axes}]"#, geogcs(wgs84, ""))
    };
    let input = |text: &str| {
        let root = crate::parsed::tree(text).unwrap();
        (wkt1::Wkt1Input::read(&root).unwrap(), read(text).as_ref().and_then(|m| wkt1_input(m, &root)).unwrap())
    };
    // An ellipsoid in its own unit: PROJ `+a=6378137.001384` (the model's, in metres), the tree
    // the value as written.
    let (tree, model) = input(&geogcs(r#"SPHEROID["local",20925646.33,298.257223563,UNIT["foot",0.3048]]"#, ""));
    assert_eq!((tree.a, (model.a - 6_378_137.001_384).abs() < 1e-3), (20_925_646.33, true));
    // A prime meridian in its own unit: PROJ `+pm=9` (the model's), the tree 10.
    let (tree, model) = input(
        r#"GEOGCS["g",DATUM["d",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Local",10,UNIT["grad",0.01570796326794897]],UNIT["degree",0.0174532925199433]]"#,
    );
    assert_eq!((tree.pm, model.pm), (10.0, 9.0));
    // A length unit of size 0: PROJ `+to_meter=0` (the tree's), the model 1.
    let (tree, model) = input(&projcs(r#"UNIT["metre",0]"#, ""));
    assert_eq!((tree.unit, model.unit), (0.0, 1.0));
    // A quoted direction and WKT 2's `ORDER`, which PROJ refuses in WKT 1: the tree reads the
    // first as no direction and ignores the second, the model reads both.
    let (tree, model) = input(&projcs(r#"UNIT["metre",1]"#, r#",AXIS["x","WEST"],AXIS["y",NORTH]"#));
    assert_eq!((tree.axes, model.axes), (None, Some(*b"wn")));
    let (tree, model) = input(&projcs(r#"UNIT["metre",1]"#, r#",AXIS["x",WEST,ORDER[2]],AXIS["y",NORTH,ORDER[1]]"#));
    assert_eq!((tree.axes, model.axes), (Some(*b"wn"), Some(*b"nw")));
    // A projection's identifiers when malformed: a first `AUTHORITY` without its code, and an
    // `ID` before the `AUTHORITY`. The tree reads no EPSG code for the first and the `AUTHORITY`'s
    // for the second (it reads the first `AUTHORITY`, before any `ID`), the model its first
    // well-formed identifier. The code matters only where it decides the method, Polar
    // Stereographic (variant C), where PROJ refuses both forms.
    let tm = |ids: &str| {
        format!(
            r#"PROJCS["x",{},PROJECTION["Transverse_Mercator",{ids}],PARAMETER["central_meridian",9],UNIT["metre",1]]"#,
            geogcs(wgs84, "")
        )
    };
    let (tree, model) = input(&tm(r#"AUTHORITY["vendor"],AUTHORITY["EPSG","9807"]"#));
    assert_eq!((tree.method_epsg, model.method_epsg), (None, Some(9807)));
    let (tree, model) = input(&tm(r#"ID["x","1"],AUTHORITY["EPSG","9807"]"#));
    assert_eq!((tree.method_epsg, model.method_epsg), (Some(9807), None));
    // WKT 2's `CONVERSION` inside a `PROJCS` with its own `PROJECTION` and `PARAMETER`s: PROJ
    // `+lon_0=30 +k=1` (the model's, the `CONVERSION`'s), the tree the `PARAMETER`s, 9 and 0.9996.
    let both = format!(
        r#"PROJCS["x",{},PROJECTION["Transverse_Mercator"],PARAMETER["central_meridian",9],PARAMETER["scale_factor",0.9996],CONVERSION["c",METHOD["Transverse Mercator"],PARAMETER["central_meridian",30],PARAMETER["scale_factor",1]],UNIT["metre",1]]"#,
        geogcs(wgs84, "")
    );
    let (tree, model) = input(&both);
    let param = |i: &wkt1::Wkt1Input, name: &str| i.params.iter().find(|(k, _)| k == name).map(|(_, v)| *v);
    assert_eq!(
        (param(&tree, "centralmeridian"), param(&tree, "scalefactor"), param(&model, "centralmeridian"), param(&model, "scalefactor")),
        (Some(9.0), Some(0.9996), Some(30.0), Some(1.0))
    );
}

/// The tree's and the model's input of a WKT 2 geographic, geocentric or projected CRS (not a
/// derived or bound one), compared by their `Debug` form; `None` for another text.
fn wkt2_inputs(text: &str) -> Option<(String, String)> {
    let root = crate::parsed::tree(text)?;
    if !crate::wkt2::handles(&root) || root.key.eq_ignore_ascii_case("BOUNDCRS") || root.child("DERIVINGCONVERSION").is_some() {
        return None;
    }
    let tree = crate::wkt2::Wkt2Input::read(&root).ok();
    let model = read(text).as_ref().and_then(|m| wkt2_input(m, &root));
    Some((format!("{tree:?}"), format!("{model:?}")))
}

#[test]
fn every_wkt2_corpus_text_gives_its_proj_string_the_tree_s_input() {
    let corpus = projjson_compare::corpus(&[
        &include_bytes!("../../tests/data/projjson_corpus_epsg.jsonl.zst")[..],
        &include_bytes!("../../tests/data/projjson_corpus_esri.jsonl.zst")[..],
    ]);
    let (mut compared, mut read_by_both, mut differ) = (0, 0, Vec::new());
    // PROJ's WKT 2 of every EPSG and ESRI CRS (`wkt2` rows), but for those another step reads
    // (derived and bound CRSs, #1058 step 6c-2) and those the reader does not convert (vertical,
    // compound).
    for (code, kind, text, _) in corpus.iter().filter(|(_, kind, _, _)| kind == "wkt2") {
        let Some((tree, model)) = wkt2_inputs(text) else { continue };
        compared += 1;
        if tree != "None" && model != "None" {
            read_by_both += 1;
        }
        if tree != model {
            differ.push(format!("{code} {kind}\n  tree  {tree}\n  model {model}"));
        }
    }
    assert!(compared > 9_500 && read_by_both > 9_500, "{compared} WKT 2 texts, {read_by_both} read");
    assert!(differ.is_empty(), "{} of {compared} differ:\n{}", differ.len(), differ[..differ.len().min(10)].join("\n"));
}

/// What the WKT 2 corpus does not have: refusals, planetary and polar axes, other units, PROJ's
/// own methods, a geocentric CRS's CS.
#[test]
fn unusual_wkt2_gives_its_proj_string_the_tree_s_input() {
    const DEG: &str = r#"ANGLEUNIT["degree",0.0174532925199433]"#;
    let datum = r#"DATUM["d",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]]"#;
    let geog = |pm: &str, cs: &str| format!(r#"GEOGCRS["g",{datum},{pm},{cs}]"#);
    let pm = format!(r#"PRIMEM["Greenwich",0,{DEG}]"#);
    let lat_lon = format!(r#"CS[ellipsoidal,2],AXIS["lat",north,ORDER[1],{DEG}],AXIS["lon",east,ORDER[2],{DEG}]"#);
    let geod = |cs: &str| format!(r#"GEODCRS["c",{datum},{pm},{cs}]"#);
    let xyz = |z: &str| {
        format!(
            r#"CS[Cartesian,3],AXIS["(X)",geocentricX,ORDER[1],LENGTHUNIT["metre",1]],AXIS["(Y)",geocentricY,ORDER[2],LENGTHUNIT["metre",1]],{z}"#
        )
    };
    let z = r#"AXIS["(Z)",geocentricZ,ORDER[3],LENGTHUNIT["metre",1]]"#;
    let base = format!(r#"BASEGEOGCRS["g",{datum},{pm}]"#);
    let projected = |method: &str, params: &str, cs: &str| format!(r#"PROJCRS["p",{base},CONVERSION["c",METHOD[{method}]{params}],{cs}]"#);
    let en = r#"CS[Cartesian,2],AXIS["(E)",east,ORDER[1],LENGTHUNIT["metre",1]],AXIS["(N)",north,ORDER[2],LENGTHUNIT["metre",1]]"#;
    let tm = r#""Transverse Mercator",ID["EPSG",9807]"#;
    let lon = format!(r#",PARAMETER["Longitude of natural origin",9,{DEG},ID["EPSG",8802]]"#);
    // Each text, and whether it is read.
    let cases: Vec<(String, bool)> = vec![
        (geog(&pm, &lat_lon), true),
        // A planetary CRS, west-positive, in grads with its prime meridian in its own unit.
        (
            geog(
                r#"PRIMEM["Paris",2.5969213,ANGLEUNIT["grad",0.015707963267949]]"#,
                r#"CS[ellipsoidal,2],AXIS["lat",north,ORDER[1],ANGLEUNIT["grad",0.015707963267949]],AXIS["lon",west,ORDER[2],ANGLEUNIT["grad",0.015707963267949]]"#,
            ),
            true,
        ),
        // Planetocentric, two axes only; a CS the reader does not convert.
        (geog(&pm, &format!(r#"CS[spherical,2],AXIS["lat",north,ORDER[1],{DEG}],AXIS["lon",east,ORDER[2],{DEG}]"#)), true),
        (
            geog(
                &pm,
                &format!(
                    r#"CS[spherical,3],AXIS["lat",north,ORDER[1],{DEG}],AXIS["lon",east,ORDER[2],{DEG}],AXIS["r",up,ORDER[3],LENGTHUNIT["metre",1]]"#
                ),
            ),
            false,
        ),
        (geog(&pm, &format!(r#"CS[spherical,3],AXIS["lat",north,ORDER[1],{DEG}],AXIS["lon",east,ORDER[2],{DEG}]"#)), false),
        (geog(&pm, &format!(r#"CS[spherical,3],{DEG}"#)), false),
        // A spherical CS of three axes, or an ellipsoidal one of another dimension than its axes:
        // PROJ 9.9.0 refuses them, the reader reads their first two axes.
        (
            geog(
                &pm,
                &format!(
                    r#"CS[spherical,2],AXIS["lat",north,ORDER[1],{DEG}],AXIS["lon",east,ORDER[2],{DEG}],AXIS["r",up,ORDER[3],LENGTHUNIT["metre",1]]"#
                ),
            ),
            true,
        ),
        (geog(&pm, &format!(r#"CS[ellipsoidal,3],AXIS["lat",north,ORDER[1],{DEG}],AXIS["lon",west,ORDER[2],{DEG}]"#)), true),
        (
            geog(
                &pm,
                &format!(
                    r#"CS[ellipsoidal,2],AXIS["lat",north,ORDER[1],{DEG}],AXIS["lon",east,ORDER[2],{DEG}],AXIS["h",up,ORDER[3],LENGTHUNIT["metre",1]]"#
                ),
            ),
            true,
        ),
        (geog(&pm, &format!(r#"CS[affine,2],AXIS["lat",north,ORDER[1],{DEG}],AXIS["lon",east,ORDER[2],{DEG}]"#)), false),
        // A prime meridian's unit without its size.
        (geog(r#"PRIMEM["p",2,ANGLEUNIT["grad"]]"#, &lat_lon), false),
        // Geocentric: X, Y and Z in one unit, else refused.
        (geod(&xyz(z)), true),
        (geod(&xyz(z).replace("Cartesian,3", "Cartesian,2")), false),
        (geod(&xyz(z).replace("ORDER[2]", "ORDER[3]")), false),
        (geod(&xyz(r#"AXIS["(Z)",geocentricZ,ORDER[3]]"#)), false),
        (geod(&xyz(r#"AXIS["(Z)",geocentricZ,ORDER[3],LENGTHUNIT["foot",0.3048]]"#)), false),
        (geod(&xyz(r#"AXIS["(Z)",geocentricX,ORDER[3],LENGTHUNIT["metre",1]]"#)), false),
        // The CS's unit after its axes.
        (
            geod(r#"CS[Cartesian,3],AXIS["(X)",geocentricX],AXIS["(Y)",geocentricY],AXIS["(Z)",geocentricZ],LENGTHUNIT["foot",0.3048]"#),
            true,
        ),
        // Projected: east-north, north-east in feet, a polar CS, south-orientated axes.
        (projected(tm, &lon, en), true),
        (projected(tm, &lon, r#"CS[Cartesian,2],AXIS["(N)",north],AXIS["(E)",east],LENGTHUNIT["US survey foot",0.304800609601219]"#), true),
        (
            projected(
                r#""Polar Stereographic (variant A)",ID["EPSG",9810]"#,
                &format!(r#",PARAMETER["Latitude of natural origin",90,{DEG}]"#),
                r#"CS[Cartesian,2],AXIS["(E)",south,MERIDIAN[90,ANGLEUNIT["degree",0.0174532925199433]]],AXIS["(N)",south,MERIDIAN[180,ANGLEUNIT["degree",0.0174532925199433]]],LENGTHUNIT["metre",1]"#,
            ),
            true,
        ),
        (
            projected(
                r#""Transverse Mercator (South Orientated)",ID["EPSG",9808]"#,
                &lon,
                r#"CS[Cartesian,2],AXIS["(W)",west],AXIS["(S)",south],LENGTHUNIT["metre",1]"#,
            ),
            true,
        ),
        // A parameter by its code alone, in a scale unit; PROJ's own method, its parameters
        // without units.
        (projected(tm, r#",PARAMETER["k",0.9996,SCALEUNIT["unity",1],ID["EPSG",8805]]"#, en), true),
        (projected(r#""PROJ ob_tran o_proj=moll""#, r#",PARAMETER["o_lat_p",45],PARAMETER["lon_0",10]"#, en), true),
        // An unknown method or parameter (refused when converted).
        (projected(r#""Unknown""#, &lon, en), true),
        (projected(tm, r#",PARAMETER["Unknown",1]"#, en), true),
        // A projected CRS without its conversion's method, or a CS unit without its size.
        (format!(r#"PROJCRS["p",{base},CONVERSION["c"],{en}]"#), false),
        (projected(tm, &lon, r#"CS[Cartesian,2],AXIS["(E)",east],AXIS["(N)",north],LENGTHUNIT["metre"]"#), false),
        // A parameter without its value, of an EPSG method or PROJ's.
        (projected(tm, r#",PARAMETER["Longitude of natural origin"]"#, en), false),
        (projected(r#""PROJ ob_tran o_proj=moll""#, r#",PARAMETER["o_lat_p",ANGLEUNIT["degree",0.0174532925199433]]"#, en), false),
    ];
    let mut differ = Vec::new();
    for (text, read) in &cases {
        let (tree, model) = wkt2_inputs(text).unwrap_or_else(|| panic!("not a WKT 2 CRS the reader converts: {text}"));
        if tree != model || (tree != "None") != *read {
            differ.push(format!("{text}\n  tree  {tree}\n  model {model}"));
        }
    }
    assert!(differ.is_empty(), "{}", differ.join("\n"));
}

/// Where the model reads unusual WKT 2 otherwise than the tree reader, each with PROJ 9.9.0's
/// reading, to settle in #1058 step 6d as PROJ reads it.
#[test]
fn where_the_model_reads_unusual_wkt2_otherwise() {
    let input = |text: &str| {
        let root = crate::parsed::tree(text).unwrap();
        (crate::wkt2::Wkt2Input::read(&root).unwrap(), read(text).as_ref().and_then(|m| wkt2_input(m, &root)).unwrap())
    };
    // An `ID` and then an `AUTHORITY`, of other codes: the tree reads the `AUTHORITY` (it reads an
    // `AUTHORITY` before any `ID`), the model the first. PROJ 9.9.0 reads the CRS, and identifies
    // it as neither.
    let (tree, model) = input(
        r#"GEOGCRS["x",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],CS[ellipsoidal,2],AXIS["latitude",north,ANGLEUNIT["degree",0.0174532925199433]],AXIS["longitude",east,ANGLEUNIT["degree",0.0174532925199433]],ID["EPSG",4258],AUTHORITY["EPSG","4326"]]"#,
    );
    assert_eq!((tree.geodetic.epsg, model.geodetic.epsg), (Some(4326), Some(4258)));

    const DEG: &str = r#"ANGLEUNIT["degree",0.0174532925199433]"#;
    let datum = r#"DATUM["d",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]]"#;
    let geog = |pm: &str, axes: &str| format!(r#"GEOGCRS["g",{datum},{pm},CS[ellipsoidal,2],{axes}]"#);
    let pm = format!(r#"PRIMEM["Greenwich",0,{DEG}]"#);
    let lat_lon = format!(r#"AXIS["lat",north,{DEG}],AXIS["lon",east,{DEG}]"#);
    let projected = |param: &str, axes: &str| {
        format!(
            r#"PROJCRS["p",BASEGEOGCRS["g",{datum},{pm}],CONVERSION["c",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Longitude of natural origin",9,{param},ID["EPSG",8802]]],CS[Cartesian,2],{axes}]"#
        )
    };
    let en = r#"AXIS["(E)",east,LENGTHUNIT["metre",1]],AXIS["(N)",north,LENGTHUNIT["metre",1]]"#;
    let geographic = |i: &crate::wkt2::Wkt2Input| match &i.kind {
        crate::wkt2::Wkt2Kind::Geographic { axes, .. } => axes.clone(),
        _ => panic!("not geographic"),
    };
    let projection = |i: &crate::wkt2::Wkt2Input| match &i.kind {
        crate::wkt2::Wkt2Kind::Projected { directions, method, .. } => (directions.clone(), method.params[0].units.clone()),
        _ => panic!("not projected"),
    };
    // Axes out of their `ORDER`: the tree reads them as written, the model in their order. PROJ
    // 9.9.0 refuses the CRS.
    let (tree, model) = input(&geog(&pm, &format!(r#"AXIS["lon",west,ORDER[2],{DEG}],AXIS["lat",north,ORDER[1],{DEG}]"#)));
    assert_eq!((geographic(&tree), geographic(&model)), (Some("wn".into()), Some("nw".into())));
    let (tree, model) =
        input(&projected(DEG, r#"AXIS["(N)",north,ORDER[2],LENGTHUNIT["metre",1]],AXIS["(E)",west,ORDER[1],LENGTHUNIT["metre",1]]"#));
    assert_eq!(
        (projection(&tree).0, projection(&model).0),
        (vec!["north".to_string(), "west".into()], vec!["west".into(), "north".into()])
    );
    // An axis direction in quotes: the tree skips it, the model reads it. PROJ 9.9.0 refuses the
    // CRS.
    let (tree, model) = input(&geog(&pm, &format!(r#"AXIS["lon","west",{DEG}],AXIS["lat",north,{DEG}]"#)));
    assert_eq!((geographic(&tree), geographic(&model)), (None, Some("wn".into())));
    let (tree, model) = input(&projected(DEG, r#"AXIS["(E)","west",LENGTHUNIT["metre",1]],AXIS["(N)",north,LENGTHUNIT["metre",1]]"#));
    assert_eq!((projection(&tree).0, projection(&model).0), (vec!["north".to_string()], vec!["west".into(), "north".into()]));
    // WKT 2's long `PRIMEMERIDIAN` keyword: the tree reads no prime meridian, the model reads it.
    // PROJ 9.9.0 reads it too (`+pm=2`).
    let (tree, model) = input(&geog(&format!(r#"PRIMEMERIDIAN["Local",2,{DEG}]"#), &lat_lon));
    assert_eq!((tree.geodetic.pm, model.geodetic.pm), (0.0, 2.0));
    // A parameter's unit without its size: the tree keeps the unit, the model drops it; both
    // give the parameter the CRS's unit. PROJ 9.9.0 refuses the CRS.
    let (tree, model) = input(&projected(r#"ANGLEUNIT["degree"]"#, en));
    assert_eq!((projection(&tree).1, projection(&model).1), (vec![("ANGLEUNIT", None)], vec![]));
    // A parameter in a time unit: the tree skips the unit, the model reads it, as PROJ 9.9.0 does
    // (it reads 9 seconds of longitude as 9 radians, `+lon_0=515.662015617741`).
    let (tree, model) = input(&projected(r#"TIMEUNIT["second",1]"#, en));
    assert_eq!((projection(&tree).1, projection(&model).1), (vec![], vec![("UNIT", Some(1.0))]));
}

// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! The PROJ string terrio gives WKT the #1066 corpus does not have (#1058 step 6): unusual,
//! malformed, derived and bound CRSs, and refusals. Each text's PROJ string, or its refusal, is
//! recorded in `tests/data/wkt_proj_strings.tsv`, as the tree readers gave it when the model took
//! their place (#1058 step 6d-4b); re-record a deliberate change with `TERRIO_RECORD=1`. The
//! corpus's own texts are checked through the PROJJSON they give (`projjson_corpus_diffs.tsv`).
//! Where the model reads a text as PROJ 9.9.0 does and the tree readers did not, PROJ's value is
//! checked here too.
use super::*;

/// The PROJ string terrio gives a WKT definition (`model::wkt1_to_proj`, `model::wkt2_to_proj`),
/// or its refusal: `refused: ` and the message of a WKT 2 CRS the WKT 2 reader converts
/// (`wkt2::handles`), `none` for another.
fn proj_string(text: &str) -> String {
    match crate::parsed::tree(text) {
        Some(root) if crate::wkt2::handles(&root) => wkt2_to_proj(text).map_or_else(|e| format!("refused: {e}"), |c| c.proj),
        Some(_) => wkt1_to_proj(text).map_or_else(|| "none".to_string(), |c| c.proj),
        None => "not WKT".to_string(),
    }
}

/// Whether the model reads a text's input to its PROJ string (a refusal of its method or
/// parameters comes after, when converted).
fn input_read(text: &str) -> bool {
    let root = crate::parsed::tree(text).unwrap_or_else(|| panic!("not WKT: {text}"));
    if crate::wkt2::handles(&root) {
        try_read(text).and_then(|m| wkt2_root_input(&m)).is_ok()
    } else {
        read(text).as_ref().and_then(|m| wkt1_input(m, &root)).is_some()
    }
}

/// Each group's texts give the PROJ string, or refusal, recorded in `tests/data/wkt_proj_strings.tsv`
/// (a line per text: its group, its index, its PROJ string and the text), and their input is read
/// or refused as each group says.
#[test]
fn proj_strings_are_as_recorded() {
    let groups = [
        ("unusual_wkt1", unusual_wkt1()),
        ("malformed_wkt1", malformed_wkt1()),
        ("unusual_wkt2", unusual_wkt2()),
        ("derived_wkt2", derived_wkt2()),
        ("bound_wkt2", bound_wkt2()),
        ("malformed_wkt2", malformed_wkt2()),
    ];
    let mut found = String::new();
    let mut misread = Vec::new();
    for (group, cases) in &groups {
        for (i, (text, read)) in cases.iter().enumerate() {
            if input_read(text) != *read {
                misread.push(format!("{group} {i}: read {}, expected {read}: {text}", !read));
            }
            found += &format!("{group}\t{i}\t{}\t{text}\n", proj_string(text));
        }
    }
    assert!(misread.is_empty(), "{}", misread.join("\n"));
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/wkt_proj_strings.tsv");
    if std::env::var("TERRIO_RECORD").as_deref() == Ok("1") {
        std::fs::write(path, &found).unwrap();
        return;
    }
    // Line by line, as a checkout may end lines with CRLF.
    let recorded = std::fs::read_to_string(path).unwrap_or_default().replace("\r\n", "\n");
    let changed: Vec<String> = found
        .lines()
        .zip(recorded.lines().chain(std::iter::repeat("")))
        .filter(|(new, old)| new != old)
        .map(|(new, old)| format!("recorded {old}\n   found {new}"))
        .collect();
    assert!(
        changed.is_empty() && found.lines().count() == recorded.lines().count(),
        "PROJ strings changed, {} texts recorded, {} found (record a deliberate change with TERRIO_RECORD=1):\n{}",
        recorded.lines().count(),
        found.lines().count(),
        changed.join("\n")
    );
}

/// What the corpus does not have: refusals, other units, prime meridians and axes, ESRI's
/// compound CRS, a geocentric CRS, parameters without a name or a number.
fn unusual_wkt1() -> Vec<(String, bool)> {
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
    cases
}

/// Where the model reads unusual WKT 1 otherwise than terrio's tree reader did, as PROJ 9.9.0 reads
/// it (pyproj 3.8.0, `CRS(wkt).to_proj4()`; #1058 step 6d-2).
#[test]
fn where_the_model_reads_unusual_wkt1_otherwise() {
    let geogcs = |datum: &str, inner: &str| {
        format!(r#"GEOGCS["g",DATUM["d",{datum}],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]{inner}]"#)
    };
    let input = |text: &str| {
        let root = crate::parsed::tree(text).unwrap();
        read(text).as_ref().and_then(|m| wkt1_input(m, &root)).unwrap()
    };
    // An ellipsoid in its own unit: PROJ `+a=6378137.001384`, in metres (the tree read the value
    // as written).
    let model = input(&geogcs(r#"SPHEROID["local",20925646.33,298.257223563,UNIT["foot",0.3048]]"#, ""));
    assert!((model.a - 6_378_137.001_384).abs() < 1e-3, "{}", model.a);
    // A prime meridian in its own unit: PROJ `+pm=9` (the tree read 10).
    let model = input(
        r#"GEOGCS["g",DATUM["d",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Local",10,UNIT["grad",0.01570796326794897]],UNIT["degree",0.0174532925199433]]"#,
    );
    assert_eq!(model.pm, 9.0);
}

/// Malformed WKT 1 that PROJ 9.9.0 refuses or reads as the tree did, which the model reads as the
/// tree did, so that the PROJ string does not change (#1058 step 6d-1, backward compatibility).
fn malformed_wkt1() -> Vec<(String, bool)> {
    let geogcs =
        r#"GEOGCS["g",DATUM["d",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;
    let projcs = |ids: &str, rest: &str| {
        format!(r#"PROJCS["x",{geogcs},PROJECTION["Transverse_Mercator"{ids}],PARAMETER["central_meridian",9],{rest}]"#)
    };
    let cases = [
        // A length unit of size 0: PROJ `+to_meter=0`, as the tree reads it.
        projcs("", r#"UNIT["metre",0]"#),
        // A quoted direction, read as none, and WKT 2's `ORDER`, ignored: PROJ refuses both in
        // WKT 1.
        projcs("", r#"UNIT["metre",1],AXIS["x","WEST"],AXIS["y",NORTH]"#),
        projcs("", r#"UNIT["metre",1],AXIS["x",WEST,ORDER[2]],AXIS["y",NORTH,ORDER[1]]"#),
        // A projection's malformed identifiers: a first `AUTHORITY` without its code (no EPSG
        // code), and an `ID` before the `AUTHORITY` (the `AUTHORITY`'s). The code decides only
        // Polar Stereographic (variant C), where PROJ refuses both forms.
        projcs(r#",AUTHORITY["vendor"],AUTHORITY["EPSG","9807"]"#, r#"UNIT["metre",1]"#),
        projcs(r#",ID["x","1"],AUTHORITY["EPSG","9807"]"#, r#"UNIT["metre",1]"#),
        // WKT 2's `CONVERSION` inside a `PROJCS` with its own `PROJECTION` and `PARAMETER`s: those,
        // whatever the `CONVERSION`'s units and method. PROJ reads the `CONVERSION` (`+lon_0=30
        // +k=1`; `+lon_0=27` for 30 grads), which the WKT 1 converter does not (#1012).
        projcs(
            "",
            r#"PARAMETER["scale_factor",0.9996],CONVERSION["c",METHOD["Transverse Mercator"],PARAMETER["central_meridian",30],PARAMETER["scale_factor",1]],UNIT["metre",1]"#,
        ),
        projcs(
            "",
            r#"CONVERSION["c",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Longitude of natural origin",30,ANGLEUNIT["grad",0.015707963267949],ID["EPSG",8802]]],UNIT["metre",1]"#,
        ),
        format!(
            r#"PROJCS["x",{geogcs},PROJECTION["Lambert_Conformal_Conic_2SP"],PARAMETER["standard_parallel_1",45],PARAMETER["standard_parallel_2",50],PARAMETER["central_meridian",9],CONVERSION["c",METHOD["Lambert Conic Conformal (2SP)",ID["EPSG",9802]],PARAMETER["Latitude of 1st standard parallel",45,ANGLEUNIT["degree",0.0174532925199433]],PARAMETER["Latitude of 2nd standard parallel",50,ANGLEUNIT["degree",0.0174532925199433]]],UNIT["metre",1]]"#
        ),
        // A spheroid of axis 0 (PROJ refuses it: "Invalid ellipsoid parameters").
        r#"GEOGCS["x",DATUM["d",SPHEROID["s",0,0]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#.to_string(),
        // ESRI's compound CRS with a `VERTCS` without its datum: its horizontal CRS (PROJ refuses
        // it: "Missing VDATUM or ENSEMBLE node").
        format!(r#"{geogcs},VERTCS["x",UNIT["Meter",1.0]]"#),
    ];
    // Each read; then a `PROJCS` with a `CONVERSION` and no `PROJECTION` of its own, refused, as
    // the tree refused it (PROJ reads it, `+lon_0=30 +k=1`).
    let mut cases: Vec<(String, bool)> = cases.into_iter().map(|text| (text, true)).collect();
    cases.push((
        format!(
            r#"PROJCS["x",{geogcs},CONVERSION["c",METHOD["Transverse Mercator"],PARAMETER["central_meridian",30],PARAMETER["scale_factor",1]],UNIT["metre",1]]"#
        ),
        false,
    ));
    cases
}

/// What the WKT 2 corpus does not have: refusals, planetary and polar axes, other units, PROJ's
/// own methods, a geocentric CRS's CS.
fn unusual_wkt2() -> Vec<(String, bool)> {
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
        // Planetocentric, two-dimensional only; then a CS the reader does not convert.
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
        // A spherical CS of two dimensions with three axes, or an ellipsoidal CS of another
        // dimension than its axes: PROJ 9.9.0 refuses them, the reader reads their first two axes.
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
        // A base CRS's CS in grads, its parameters without units: PROJ 9.9.0 and the reader read
        // them in degrees, not in the base CS's unit.
        (
            format!(
                r#"PROJCRS["p",BASEGEOGCRS["g",{datum},PRIMEM["Greenwich",0],CS[ellipsoidal,2],AXIS["lat",north,ANGLEUNIT["grad",0.015707963267949]],AXIS["lon",east,ANGLEUNIT["grad",0.015707963267949]]],CONVERSION["c",METHOD[{tm}],PARAMETER["Longitude of natural origin",9,ID["EPSG",8802]]],{en}]"#
            ),
            true,
        ),
        // A base CRS's Cartesian CS: PROJ 9.9.0 refuses it, the reader reads the base CRS
        // without its CS.
        (
            format!(
                r#"PROJCRS["p",BASEGEOGCRS["g",{datum},{pm},CS[Cartesian,2],AXIS["x",east,LENGTHUNIT["metre",1]],AXIS["y",north,LENGTHUNIT["metre",1]]],CONVERSION["c",METHOD[{tm}]{lon}],{en}]"#
            ),
            true,
        ),
        // A base CRS's spherical CS of three dimensions: PROJ 9.9.0 refuses it, the reader reads
        // the base CRS without its CS.
        (format!(r#"PROJCRS["p",BASEGEOGCRS["g",{datum},{pm},CS[spherical,3],{DEG}],CONVERSION["c",METHOD[{tm}]{lon}],{en}]"#), true),
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
        // WKT 1's `PROJECTION` and `PARAMETER`s in a WKT 2 `PROJCRS`, refused (PROJ 9.9.0 reads
        // them, `+lon_0=9 +k=0.9996`).
        (
            format!(
                r#"PROJCRS["p",{base},PROJECTION["Transverse_Mercator"],PARAMETER["central_meridian",9],PARAMETER["scale_factor",0.9996],{en}]"#
            ),
            false,
        ),
        // A projected CRS without its conversion's method, or a CS unit without its size.
        (format!(r#"PROJCRS["p",{base},CONVERSION["c"],{en}]"#), false),
        (projected(tm, &lon, r#"CS[Cartesian,2],AXIS["(E)",east],AXIS["(N)",north],LENGTHUNIT["metre"]"#), false),
        // A parameter without its value, of an EPSG method or PROJ's.
        (projected(tm, r#",PARAMETER["Longitude of natural origin"]"#, en), false),
        (projected(r#""PROJ ob_tran o_proj=moll""#, r#",PARAMETER["o_lat_p",ANGLEUNIT["degree",0.0174532925199433]]"#, en), false),
    ];
    cases
}

/// Derived geographic CRSs (rotated poles) as PROJ 9.9.0 writes them, from PROJ strings, CF grid
/// mappings and GRIB's convention (`CRS.to_wkt`), which the corpus does not have.
const DERIVED: [&str; 9] = [
    // `PROJ ob_tran`, a sphere, WKT 2:2019.
    r#"GEOGCRS["unknown",BASEGEOGCRS["unknown",DATUM["unknown",ELLIPSOID["unknown",6371229,0,LENGTHUNIT["metre",1,ID["EPSG",9001]]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8901]]],DERIVINGCONVERSION["unknown",METHOD["PROJ ob_tran o_proj=longlat"],PARAMETER["o_lon_p",0,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],PARAMETER["o_lat_p",39.25,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],PARAMETER["lon_0",18,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]]],CS[ellipsoidal,2],AXIS["longitude",east,ORDER[1],ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],AXIS["latitude",north,ORDER[2],ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]]]"#,
    // the same in WKT 2:2015, simplified.
    r#"GEODCRS["unknown",BASEGEODCRS["unknown",DATUM["unknown",ELLIPSOID["unknown",6371229,0]]],DERIVINGCONVERSION["unknown",METHOD["PROJ ob_tran o_proj=longlat"],PARAMETER["o_lon_p",0],PARAMETER["o_lat_p",39.25],PARAMETER["lon_0",18]],CS[ellipsoidal,2],AXIS["longitude",east],AXIS["latitude",north],UNIT["degree",0.0174532925199433,ID["EPSG",9122]]]"#,
    // `PROJ ob_tran` on the WGS 84 ensemble.
    r#"GEOGCRS["unknown",BASEGEOGCRS["unknown",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]],ID["EPSG",6326]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8901]]],DERIVINGCONVERSION["unknown",METHOD["PROJ ob_tran o_proj=longlat"],PARAMETER["o_lon_p",0,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],PARAMETER["o_lat_p",39.25,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],PARAMETER["lon_0",18,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]]],CS[ellipsoidal,2],AXIS["longitude",east,ORDER[1],ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],AXIS["latitude",north,ORDER[2],ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]]]"#,
    // `PROJ ob_tran` with a Paris prime meridian in its method's name (refused when converted).
    r#"GEOGCRS["unknown",BASEGEOGCRS["unknown",DATUM["Unknown based on International 1924 (Hayford 1909, 1910) ellipsoid",ELLIPSOID["International 1924 (Hayford 1909, 1910)",6378388,297,LENGTHUNIT["metre",1,ID["EPSG",9001]]]],PRIMEM["Paris",2.5969213,ANGLEUNIT["grad",0.015707963267949],ID["EPSG",8903]]],DERIVINGCONVERSION["unknown",METHOD["PROJ ob_tran o_proj=longlat pm=paris"],PARAMETER["o_lon_p",10,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],PARAMETER["o_lat_p",-30,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],PARAMETER["lon_0",0,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]]],CS[ellipsoidal,2],AXIS["longitude",east,ORDER[1],ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],AXIS["latitude",north,ORDER[2],ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]]]"#,
    // `PROJ ob_tran` of `o_proj=latlon` (refused when converted).
    r#"GEOGCRS["unknown",BASEGEOGCRS["unknown",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]],ID["EPSG",6326]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8901]]],DERIVINGCONVERSION["unknown",METHOD["PROJ ob_tran o_proj=latlon"],PARAMETER["o_lat_p",45,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],PARAMETER["lon_0",10,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]]],CS[ellipsoidal,2],AXIS["longitude",east,ORDER[1],ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],AXIS["latitude",north,ORDER[2],ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]]]"#,
    // netCDF CF convention, WGS 84 ensemble, WKT 2:2019.
    r#"GEOGCRS["undefined",BASEGEOGCRS["undefined",ENSEMBLE["World Geodetic System 1984 ensemble",MEMBER["World Geodetic System 1984 (Transit)",ID["EPSG",1166]],MEMBER["World Geodetic System 1984 (G730)",ID["EPSG",1152]],MEMBER["World Geodetic System 1984 (G873)",ID["EPSG",1153]],MEMBER["World Geodetic System 1984 (G1150)",ID["EPSG",1154]],MEMBER["World Geodetic System 1984 (G1674)",ID["EPSG",1155]],MEMBER["World Geodetic System 1984 (G1762)",ID["EPSG",1156]],MEMBER["World Geodetic System 1984 (G2139)",ID["EPSG",1309]],MEMBER["World Geodetic System 1984 (G2296)",ID["EPSG",1383]],ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1],ID["EPSG",7030]],ENSEMBLEACCURACY[2.0],ID["EPSG",6326]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8901]]],DERIVINGCONVERSION["Pole rotation (netCDF CF convention)",METHOD["Pole rotation (netCDF CF convention)"],PARAMETER["Grid north pole latitude (netCDF CF convention)",39.25,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],PARAMETER["Grid north pole longitude (netCDF CF convention)",-162,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],PARAMETER["North pole grid longitude (netCDF CF convention)",0,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]]],CS[ellipsoidal,2],AXIS["longitude",east,ORDER[1],ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],AXIS["latitude",north,ORDER[2],ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]]]"#,
    // the same in WKT 2:2015, simplified.
    r#"GEODCRS["undefined",BASEGEODCRS["undefined",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563],ID["EPSG",6326]]],DERIVINGCONVERSION["Pole rotation (netCDF CF convention)",METHOD["Pole rotation (netCDF CF convention)"],PARAMETER["Grid north pole latitude (netCDF CF convention)",39.25],PARAMETER["Grid north pole longitude (netCDF CF convention)",-162],PARAMETER["North pole grid longitude (netCDF CF convention)",0]],CS[ellipsoidal,2],AXIS["longitude",east],AXIS["latitude",north],UNIT["degree",0.0174532925199433,ID["EPSG",9122]]]"#,
    // GRIB convention, with an axis rotation, WKT 2:2019.
    r#"GEOGCRS["Coordinate System imported from GRIB file",BASEGEOGCRS["Coordinate System imported from GRIB file",DATUM["unnamed",ELLIPSOID["Sphere",6367470,0,LENGTHUNIT["metre",1,ID["EPSG",9001]]]],PRIMEM["Greenwich",0,ANGLEUNIT["degree",0.0174532925199433],ID["EPSG",8901]]],DERIVINGCONVERSION["Pole rotation (GRIB convention)",METHOD["Pole rotation (GRIB convention)"],PARAMETER["Latitude of the southern pole (GRIB convention)",-30,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],PARAMETER["Longitude of the southern pole (GRIB convention)",-15,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],PARAMETER["Axis rotation (GRIB convention)",12.5,ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]]],CS[ellipsoidal,2],AXIS["latitude",north,ORDER[1],ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]],AXIS["longitude",east,ORDER[2],ANGLEUNIT["degree",0.0174532925199433,ID["EPSG",9122]]]]"#,
    // the same in WKT 2:2015, simplified.
    r#"GEODCRS["Coordinate System imported from GRIB file",BASEGEODCRS["Coordinate System imported from GRIB file",DATUM["unnamed",ELLIPSOID["Sphere",6367470,0]]],DERIVINGCONVERSION["Pole rotation (GRIB convention)",METHOD["Pole rotation (GRIB convention)"],PARAMETER["Latitude of the southern pole (GRIB convention)",-30],PARAMETER["Longitude of the southern pole (GRIB convention)",-15],PARAMETER["Axis rotation (GRIB convention)",12.5]],CS[ellipsoidal,2],AXIS["latitude",north],AXIS["longitude",east],UNIT["degree",0.0174532925199433,ID["EPSG",9122]]]"#,
];

/// Derived geographic CRSs: PROJ 9.9.0's texts, and what they lack (refusals, other units and
/// CSs).
fn derived_wkt2() -> Vec<(String, bool)> {
    const DEG: &str = r#"ANGLEUNIT["degree",0.0174532925199433]"#;
    const GRAD: &str = r#"ANGLEUNIT["grad",0.015707963267949]"#;
    let base = r#"BASEGEOGCRS["b",DATUM["d",ELLIPSOID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0]]"#;
    let conv = format!(
        r#"DERIVINGCONVERSION["c",METHOD["PROJ ob_tran o_proj=longlat"],PARAMETER["o_lat_p",39.25,{DEG}],PARAMETER["lon_0",18,{DEG}]]"#
    );
    let derived = |base: &str, conv: &str, cs: &str| format!(r#"GEOGCRS["r",{base},{conv},{cs}]"#);
    let lon_lat = |unit: &str| format!(r#"CS[ellipsoidal,2],AXIS["lon",east,{unit}],AXIS["lat",north,{unit}]"#);
    // Each text, and whether it is read.
    let mut cases: Vec<(String, bool)> = DERIVED.iter().map(|t| (t.to_string(), true)).collect();
    cases.extend([
        // A grid in grads; no axes; a base geodetic CRS.
        (derived(base, &conv, &lon_lat(GRAD)), true),
        (derived(base, &conv, "CS[ellipsoidal,2]"), true),
        (derived(&base.replace("BASEGEOGCRS", "BASEGEODCRS"), &conv, &lon_lat(DEG)), true),
        // Parameters in grads, or without a unit in the base CRS's grads.
        (derived(base, &format!(r#"DERIVINGCONVERSION["c",METHOD["PROJ ob_tran o_proj=longlat"],PARAMETER["o_lat_p",40,{GRAD}]]"#), &lon_lat(DEG)), true),
        (
            derived(
                &base.replace(r#"PRIMEM["Greenwich",0]"#, &format!(r#"PRIMEM["Greenwich",0],{GRAD}"#)),
                r#"DERIVINGCONVERSION["c",METHOD["PROJ ob_tran o_proj=longlat"],PARAMETER["o_lat_p",40]]"#,
                &lon_lat(DEG),
            ),
            true,
        ),
        // A Cartesian CS, which PROJ 9.9.0 refuses, or a first axis in a length unit: the grid unit
        // is the first angular unit of its axes.
        (derived(base, &conv, r#"CS[Cartesian,2],AXIS["x",east,LENGTHUNIT["metre",1]],AXIS["y",north,LENGTHUNIT["metre",1]]"#), true),
        (derived(base, &conv, &format!(r#"CS[ellipsoidal,2],AXIS["lon",east,LENGTHUNIT["metre",1]],AXIS["lat",north,{GRAD}]"#)), true),
        // An unknown method (refused when converted).
        (derived(base, &format!(r#"DERIVINGCONVERSION["c",METHOD["Some conversion"],PARAMETER["o_lat_p",39.25,{DEG}]]"#), &lon_lat(DEG)), true),
        // Refused, as PROJ 9.9.0 refuses them: without its base CRS (or a WKT 1 one), its method,
        // or a parameter's value.
        (format!(r#"GEOGCRS["r",{conv},{}]"#, lon_lat(DEG)), false),
        (
            derived(
                r#"GEOGCS["b",DATUM["d",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#,
                &conv,
                &lon_lat(DEG),
            ),
            false,
        ),
        (derived(base, r#"DERIVINGCONVERSION["c",PARAMETER["o_lat_p",39.25]]"#, &lon_lat(DEG)), false),
        (derived(base, r#"DERIVINGCONVERSION["c",METHOD["PROJ ob_tran o_proj=longlat"],PARAMETER["o_lat_p"]]"#, &lon_lat(DEG)), false),
    ]);
    cases
}

/// Bound CRSs, which the corpus does not have: each kind of Helmert transformation, a grid's, WGS
/// 84 or another target, WKT 1 and derived source CRSs, and refusals.
fn bound_wkt2() -> Vec<(String, bool)> {
    const DEG: &str = r#"ANGLEUNIT["degree",0.0174532925199433]"#;
    const M: &str = r#"LENGTHUNIT["metre",1]"#;
    let geog = |name: &str, datum: &str, ellipsoid: &str, id: &str| {
        format!(
            r#"GEOGCRS["{name}",DATUM["{datum}",ELLIPSOID[{ellipsoid},{M}]],PRIMEM["Greenwich",0,{DEG}],CS[ellipsoidal,2],AXIS["lat",north,ORDER[1],{DEG}],AXIS["lon",east,ORDER[2],{DEG}]{id}]"#
        )
    };
    let osgb = geog("OSGB36", "Ordnance Survey of Great Britain 1936", r#""Airy 1830",6377563.396,299.3249646"#, r#",ID["EPSG",4277]"#);
    let wgs84 = geog("WGS 84", "World Geodetic System 1984", r#""WGS 84",6378137,298.257223563"#, r#",ID["EPSG",4326]"#);
    let etrs89 = geog("ETRS89", "European Terrestrial Reference System 1989", r#""GRS 1980",6378137,298.257222101"#, r#",ID["EPSG",4258]"#);
    let bng = format!(
        r#"PROJCRS["OSGB36 / British National Grid",BASEGEOGCRS["OSGB36",DATUM["Ordnance Survey of Great Britain 1936",ELLIPSOID["Airy 1830",6377563.396,299.3249646,{M}]],PRIMEM["Greenwich",0,{DEG}]],CONVERSION["British National Grid",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Latitude of natural origin",49,{DEG},ID["EPSG",8801]],PARAMETER["Longitude of natural origin",-2,{DEG},ID["EPSG",8802]],PARAMETER["Scale factor at natural origin",0.9996012717,SCALEUNIT["unity",1],ID["EPSG",8805]],PARAMETER["False easting",400000,{M},ID["EPSG",8806]],PARAMETER["False northing",-100000,{M},ID["EPSG",8807]]],CS[Cartesian,2],AXIS["(E)",east,ORDER[1],{M}],AXIS["(N)",north,ORDER[2],{M}],ID["EPSG",27700]]"#
    );
    let p = |name: &str, value: f64, unit: &str, code: u32| format!(r#"PARAMETER["{name}",{value},{unit},ID["EPSG",{code}]]"#);
    let arc_second = r#"ANGLEUNIT["arc-second",4.84813681109536E-06]"#;
    let three =
        [p("X-axis translation", -87.0, M, 8605), p("Y-axis translation", -98.0, M, 8606), p("Z-axis translation", -121.0, M, 8607)]
            .join(",");
    let seven = [
        p("X-axis translation", 446.448, M, 8605),
        p("Y-axis translation", -125.157, M, 8606),
        p("Z-axis translation", 542.06, M, 8607),
        p("X-axis rotation", 0.15, arc_second, 8608),
        p("Y-axis rotation", 0.247, arc_second, 8609),
        p("Z-axis rotation", 0.842, arc_second, 8610),
        p("Scale difference", -20.489, r#"SCALEUNIT["parts per million",1E-06]"#, 8611),
    ]
    .join(",");
    let transformation = |method: &str, params: &str| format!(r#"ABRIDGEDTRANSFORMATION["t",METHOD[{method}],{params}]"#);
    let bound = |source: &str, target: &str, t: &str| format!("BOUNDCRS[SOURCECRS[{source}],TARGETCRS[{target}],{t}]");
    let pv = transformation(r#""Position Vector transformation (geog2D domain)",ID["EPSG",9606]"#, &seven);
    let ntv2 = r#"ABRIDGEDTRANSFORMATION["t",METHOD["NTv2",ID["EPSG",9615]],PARAMETERFILE["Latitude and longitude difference file","OSTN15_NTv2_OSGBtoETRS.gsb"]]"#;
    let towgs84 = "TOWGS84[446.448,-125.157,542.06,0.15,0.247,0.842,-20.489]";
    let rotated = format!(
        r#"GEOGCRS["r",BASEGEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0]],DERIVINGCONVERSION["p",METHOD["PROJ ob_tran o_proj=longlat"],PARAMETER["o_lat_p",39.25,{DEG}],PARAMETER["lon_0",18,{DEG}]],CS[ellipsoidal,2],AXIS["lon",east,{DEG}],AXIS["lat",north,{DEG}]]"#
    );
    let evaluation = [
        p("Ordinate 1 of evaluation point", 3800000.0, M, 8617),
        p("Ordinate 2 of evaluation point", -100000.0, M, 8618),
        p("Ordinate 3 of evaluation point", 5100000.0, M, 8667),
    ]
    .join(",");
    // Each text, and whether it is read.
    let cases: Vec<(String, bool)> = vec![
        // To WGS 84: position vector, of a geographic and a projected CRS; coordinate frame by its
        // name alone; geocentric translations; coordinate frame's full matrix; Molodensky-Badekas.
        (bound(&osgb, &wgs84, &pv), true),
        (bound(&bng, &wgs84, &pv), true),
        (bound(&osgb, &wgs84, &transformation(r#""Coordinate Frame rotation (geog2D domain)""#, &seven)), true),
        (bound(&osgb, &wgs84, &transformation(r#""Geocentric translations (geog2D domain)",ID["EPSG",9603]"#, &three)), true),
        (bound(&osgb, &wgs84, &transformation(r#""Coordinate Frame rotation full matrix (geog2D)",ID["EPSG",1133]"#, &seven)), true),
        (
            bound(
                &osgb,
                &wgs84,
                &transformation(r#""Molodensky-Badekas (PV geog2D domain)",ID["EPSG",1063]"#, &format!("{seven},{evaluation}")),
            ),
            true,
        ),
        // WGS 84 by its datum's name; a WGS 84 ensemble.
        (bound(&osgb, &wgs84.replace(r#",ID["EPSG",4326]"#, ""), &pv), true),
        (
            bound(
                &osgb,
                &format!(
                    r#"GEOGCRS["WGS 84",ENSEMBLE["World Geodetic System 1984 ensemble",MEMBER["World Geodetic System 1984 (G2296)"],ELLIPSOID["WGS 84",6378137,298.257223563,{M}],ENSEMBLEACCURACY[2.0]],PRIMEM["Greenwich",0,{DEG}],CS[ellipsoidal,2],AXIS["lat",north,{DEG}],AXIS["lon",east,{DEG}]]"#
                ),
                &pv,
            ),
            true,
        ),
        // A grid's transformation; to ETRS89 or a projected CRS, which the reader converts too; to a
        // vertical CRS, which it leaves out, as an unreadable target (PROJ 9.9.0 refuses that one).
        (bound(&osgb, &wgs84, ntv2), true),
        (bound(&osgb, &etrs89, &pv), true),
        (bound(&osgb, &bng.replace("OSGB36 / British National Grid", "x"), &pv), true),
        (
            bound(
                &osgb,
                &format!(
                    r#"VERTCRS["ODN height",VDATUM["Ordnance Datum Newlyn"],CS[vertical,1],AXIS["gravity-related height (H)",up,{M}]]"#
                ),
                &pv,
            ),
            true,
        ),
        (bound(&osgb, &format!(r#"GEOGCRS["x",CS[ellipsoidal,2],AXIS["lat",north,{DEG}],AXIS["lon",east,{DEG}]]"#), &pv), true),
        // A WKT 1 `GEOGCS` source with a WKT 2 Cartesian `CS`: read as a `GEOGCS`, by its keyword.
        (
            bound(
                r#"GEOGCS["g",DATUM["d",SPHEROID["Airy 1830",6377563.396,299.3249646]],PRIMEM["Local",2],UNIT["degree",0.0174532925199433],CS[Cartesian,2]]"#,
                &wgs84,
                &pv,
            ),
            true,
        ),
        // A base geographic CRS as the target, which the reader does not convert; a bound CRS as
        // the target whose source is a WKT 1 `GEOCCS`, which it does not read.
        (
            bound(
                &osgb,
                &format!(
                    r#"BASEGEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563,{M}]],PRIMEM["Greenwich",0,{DEG}],ID["EPSG",4326]]"#
                ),
                &pv,
            ),
            true,
        ),
        (
            bound(
                &osgb,
                &bound(
                    r#"GEOCCS["g",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["metre",1]]"#,
                    &wgs84,
                    &pv,
                ),
                &pv,
            ),
            true,
        ),
        // An unreadable target that is WGS 84 by its code or its datum's name: the shift is to
        // WGS 84 (`+towgs84`) all the same.
        (
            bound(
                &osgb,
                &format!(r#"GEOGCRS["WGS 84",CS[ellipsoidal,2],AXIS["lat",north,{DEG}],AXIS["lon",east,{DEG}],ID["EPSG",4326]]"#),
                &transformation(r#""Geocentric translations (geog2D domain)",ID["EPSG",9603]"#, &three),
            ),
            true,
        ),
        (
            bound(
                &osgb,
                &format!(
                    r#"GEOGCRS["WGS 84",DATUM["World Geodetic System 1984"],CS[ellipsoidal,2],AXIS["lat",north,{DEG}],AXIS["lon",east,{DEG}]]"#
                ),
                &pv,
            ),
            true,
        ),
        // GDAL's WKT 1 source CRSs, read without their `TOWGS84`; a rotated pole.
        (
            bound(
                &format!(
                    r#"GEOGCS["OSGB 1936",DATUM["OSGB_1936",SPHEROID["Airy 1830",6377563.396,299.3249646],{towgs84},AUTHORITY["EPSG","6277"]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],AUTHORITY["EPSG","4277"]]"#
                ),
                &wgs84,
                &pv,
            ),
            true,
        ),
        (
            bound(
                &format!(
                    r#"PROJCS["OSGB 1936 / British National Grid",GEOGCS["OSGB 1936",DATUM["OSGB_1936",SPHEROID["Airy 1830",6377563.396,299.3249646],{towgs84}],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",49],PARAMETER["central_meridian",-2],PARAMETER["scale_factor",0.9996012717],PARAMETER["false_easting",400000],PARAMETER["false_northing",-100000],UNIT["metre",1],AXIS["Easting",EAST],AXIS["Northing",NORTH]]"#
                ),
                &wgs84,
                &pv,
            ),
            true,
        ),
        (bound(&rotated, &wgs84, &transformation(r#""Geocentric translations (geog2D domain)",ID["EPSG",9603]"#, &three)), true),
        // An unknown method or parameter (refused when converted).
        (bound(&osgb, &wgs84, &transformation(r#""Something""#, &three)), true),
        (
            bound(
                &osgb,
                &wgs84,
                &transformation(
                    r#""Position Vector transformation (geog2D domain)",ID["EPSG",9606]"#,
                    &format!(r#"{seven},PARAMETER["Other",1]"#),
                ),
            ),
            true,
        ),
        // Refused, as PROJ 9.9.0 refuses them: without its transformation, or a WKT 1 source CRS
        // the WKT 1 reader refuses.
        (format!("BOUNDCRS[SOURCECRS[{osgb}],TARGETCRS[{wgs84}]]"), false),
        (bound(r#"GEOGCS["x",DATUM["d"],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#, &wgs84, &pv), false),
    ];
    cases
}

/// Where the model reads unusual WKT 2 otherwise than terrio's tree reader did, as PROJ 9.9.0 reads
/// it (#1058 step 6d-2).
#[test]
fn where_the_model_reads_unusual_wkt2_otherwise() {
    let input = |text: &str| read(text).as_ref().and_then(wkt2_input).unwrap();
    const DEG: &str = r#"ANGLEUNIT["degree",0.0174532925199433]"#;
    let datum = r#"DATUM["d",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]]"#;
    let geog = |pm: &str, axes: &str| format!(r#"GEOGCRS["g",{datum},{pm},CS[ellipsoidal,2],{axes}]"#);
    let pm = format!(r#"PRIMEM["Greenwich",0,{DEG}]"#);
    let lat_lon = format!(r#"AXIS["lat",north,{DEG}],AXIS["lon",east,{DEG}]"#);
    let projected = |param: &str| {
        format!(
            r#"PROJCRS["p",BASEGEOGCRS["g",{datum},{pm}],CONVERSION["c",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Longitude of natural origin",9,{param},ID["EPSG",8802]]],CS[Cartesian,2],AXIS["(E)",east,LENGTHUNIT["metre",1]],AXIS["(N)",north,LENGTHUNIT["metre",1]]]"#
        )
    };
    let units = |i: &crate::wkt2::Wkt2Input| match &i.kind {
        crate::wkt2::Wkt2Kind::Projected { method, .. } => method.params[0].units.clone(),
        _ => panic!("not projected"),
    };
    // WKT 2's long `PRIMEMERIDIAN` keyword, read as PROJ 9.9.0 reads it (`+pm=2`; the tree read
    // no prime meridian).
    assert_eq!(input(&geog(&format!(r#"PRIMEMERIDIAN["Local",2,{DEG}]"#), &lat_lon)).geodetic.pm, 2.0);
    // A parameter in a time unit, read as PROJ 9.9.0 reads it (9 seconds of longitude as 9
    // radians, `+lon_0=515.662015617741`; the tree skipped the unit).
    assert_eq!(units(&input(&projected(r#"TIMEUNIT["second",1]"#))), vec![("UNIT", Some(1.0))]);
    // A parameter in a length unit and then an angle unit: one unit, the first in the text, the
    // length unit, as PROJ 9.9.0 reads it (0.3048 taken for radians, `+lon_0=157.173782360287`;
    // the tree kept both and read the angle unit, `+lon_0=8.1`).
    assert_eq!(
        units(&input(&projected(r#"LENGTHUNIT["foot",0.3048],ANGLEUNIT["grad",0.015707963267949]"#))),
        vec![("LENGTHUNIT", Some(0.3048))]
    );
    // A derived geographic CRS's CS unit after its axes: the CS's grads, as PROJ 9.9.0 gives its
    // axes (the tree read degrees, its axes having none).
    let model = input(&format!(
        r#"GEOGCRS["r",BASEGEOGCRS["b",{datum},{pm}],DERIVINGCONVERSION["c",METHOD["PROJ ob_tran o_proj=longlat"],PARAMETER["o_lat_p",39.25,{DEG}]],CS[ellipsoidal,2],AXIS["lon",east],AXIS["lat",north],ANGLEUNIT["grad",0.015707963267949]]"#
    ));
    let grid = |i: &crate::wkt2::Wkt2Input| match &i.kind {
        crate::wkt2::Wkt2Kind::Derived { grid_ang, .. } => *grid_ang,
        _ => panic!("not derived"),
    };
    assert_eq!(grid(&model), 0.9);
    // A time-dependent transformation's reference epoch in a time unit, read as a conversion's
    // parameter's (the tree skipped the unit). The epoch is not applied (PROJ 9.9.0 writes no PROJ
    // string for the CRS, nor does terrio).
    let text = format!(
        r#"BOUNDCRS[SOURCECRS[{}],TARGETCRS[GEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563]],{pm},CS[ellipsoidal,2],{lat_lon},ID["EPSG",4326]]],ABRIDGEDTRANSFORMATION["t",METHOD["Time-dependent Coordinate Frame rotation (geog2D)",ID["EPSG",1057]],PARAMETER["X-axis translation",-87,LENGTHUNIT["metre",1],ID["EPSG",8605]],PARAMETER["Parameter reference epoch",2010,TIMEUNIT["year",31556925.445],ID["EPSG",1049]]]]"#,
        geog(&pm, &lat_lon)
    );
    match try_read(&text).and_then(|m| wkt2_root_input(&m)) {
        Ok(crate::wkt2::Wkt2Root::Bound(b)) => assert_eq!(b.transformation.params[1].units, vec![("UNIT", Some(31556925.445))]),
        other => panic!("not a bound CRS: {other:?}"),
    }
}

/// Malformed WKT 2 that PROJ 9.9.0 refuses or reads as the tree did, which the model reads as the
/// tree did, so that the PROJ string does not change (#1058 step 6d-1, backward compatibility).
fn malformed_wkt2() -> Vec<(String, bool)> {
    const DEG: &str = r#"ANGLEUNIT["degree",0.0174532925199433]"#;
    const GRAD: &str = r#"ANGLEUNIT["grad",0.015707963267949]"#;
    const M: &str = r#"LENGTHUNIT["metre",1]"#;
    let datum = r#"DATUM["d",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]]"#;
    let pm = format!(r#"PRIMEM["Greenwich",0,{DEG}]"#);
    let geog = |axes: &str| format!(r#"GEOGCRS["g",{datum},{pm},CS[ellipsoidal,2],{axes}]"#);
    let lat_lon = format!(r#"AXIS["lat",north,{DEG}],AXIS["lon",east,{DEG}]"#);
    let projected = |param: &str, axes: &str| {
        format!(
            r#"PROJCRS["p",BASEGEOGCRS["g",{datum},{pm}],CONVERSION["c",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Longitude of natural origin",9,{param},ID["EPSG",8802]]],CS[Cartesian,2],{axes}]"#
        )
    };
    let rotated = |param: &str, axes: &str| {
        format!(
            r#"GEOGCRS["r",BASEGEOGCRS["b",{datum},{pm}],DERIVINGCONVERSION["c",METHOD["PROJ ob_tran o_proj=longlat"],PARAMETER["o_lat_p",39.25,{param}]],CS[ellipsoidal,2],{axes}]"#
        )
    };
    let bound = |t: &str| {
        format!(
            r#"BOUNDCRS[SOURCECRS[{}],TARGETCRS[GEOGCRS["WGS 84",DATUM["World Geodetic System 1984",ELLIPSOID["WGS 84",6378137,298.257223563]],{pm},CS[ellipsoidal,2],{lat_lon},ID["EPSG",4326]]],ABRIDGEDTRANSFORMATION["t",{t}]]"#,
            geog(&lat_lon)
        )
    };
    let ellipsoid = |e: &str| format!(r#"GEOGCRS["g",DATUM["d",ELLIPSOID["e",{e}]],{pm},CS[ellipsoidal,2],{lat_lon}]"#);
    let translation = |axis: &str, code: u32| format!(r#"PARAMETER["{axis}-axis translation",-87,{M},ID["EPSG",{code}]]"#);
    let polar = |m: &str| format!(r#"AXIS["(E)",south,MERIDIAN[{m},{DEG}]],AXIS["(N)",south,MERIDIAN[{m},{DEG}]],{M}"#);
    // Each text, with PROJ 9.9.0's reading.
    let cases = [
        // An ellipsoid of axis 0 or negative, of a negative inverse flattening, or in a unit of
        // size 0, as written (PROJ refuses them: "Invalid ellipsoid parameters"); in a unit
        // without its size, in metres (PROJ refuses it: "not enough children").
        ellipsoid("0,0"),
        ellipsoid("-6378137,298.257"),
        ellipsoid("6378137,-1"),
        ellipsoid(r#"6378137,298.257,LENGTHUNIT["metre",0]"#),
        ellipsoid(r#"6378137,298.257,LENGTHUNIT["metre"]"#),
        // An `ID` and then an `AUTHORITY` of other codes, or an ESRI `ID` and then an EPSG
        // `AUTHORITY`: the `AUTHORITY`'s code. PROJ identifies the first as neither, the second as
        // EPSG:4326.
        format!(r#"{},ID["EPSG",4258],AUTHORITY["EPSG","4326"]]"#, geog(&lat_lon).strip_suffix(']').unwrap()),
        format!(r#"{},ID["ESRI",104000],AUTHORITY["EPSG","4326"]]"#, geog(&lat_lon).strip_suffix(']').unwrap()),
        // Axes out of their `ORDER`, read as written; an axis direction in quotes, skipped; polar
        // axes whose `MERIDIAN` has no number. PROJ refuses them all.
        geog(&format!(r#"AXIS["lon",west,ORDER[2],{DEG}],AXIS["lat",north,ORDER[1],{DEG}]"#)),
        projected(DEG, &format!(r#"AXIS["(N)",north,ORDER[2],{M}],AXIS["(E)",west,ORDER[1],{M}]"#)),
        geog(&format!(r#"AXIS["lon","west",{DEG}],AXIS["lat",north,{DEG}]"#)),
        projected(DEG, &format!(r#"AXIS["(E)","west",{M}],AXIS["(N)",north,{M}]"#)),
        projected(DEG, &polar(r#""x""#)),
        // A derived CRS's grid unit on the second axis only, or out of `ORDER` in two units (PROJ
        // refuses them), or of size 0 (PROJ reads the CRS); its parameter's unit of size 0 (PROJ
        // `+o_lat_p=0`).
        rotated(DEG, &format!(r#"AXIS["lon",east],AXIS["lat",north,{GRAD}]"#)),
        rotated(DEG, &format!(r#"AXIS["lon",east,ORDER[2],{GRAD}],AXIS["lat",north,ORDER[1],{DEG}]"#)),
        rotated(DEG, r#"AXIS["lon",east,ANGLEUNIT["degree",0]],AXIS["lat",north,ANGLEUNIT["degree",0]]"#),
        rotated(r#"ANGLEUNIT["degree",0]"#, &format!(r#"AXIS["lon",east,{DEG}],AXIS["lat",north,{DEG}]"#)),
        // A derived CRS's first axis unit without its size (degrees; PROJ refuses it), an axis in
        // a length unit and then an angle unit (the angle unit; PROJ reads the CRS), no axes beside
        // a CS unit (degrees; PROJ refuses it).
        rotated(DEG, &format!(r#"AXIS["lon",east,ANGLEUNIT["degree"]],AXIS["lat",north,{GRAD}]"#)),
        rotated(DEG, &format!(r#"AXIS["lon",east,{M},{GRAD}],AXIS["lat",north,{GRAD}]"#)),
        rotated(DEG, GRAD),
        // A negative length unit, kept (PROJ `+to_meter=-1`); one written as `NaN`, kept too
        // (PROJ refuses it).
        projected(DEG, r#"AXIS["(E)",east],AXIS["(N)",north],LENGTHUNIT["metre",-1]"#),
        projected(DEG, r#"AXIS["(E)",east],AXIS["(N)",north],LENGTHUNIT["metre",NaN]"#),
        // A transformation parameter without its value: refused in a Helmert transformation when
        // converted, ignored in a grid's. PROJ refuses both.
        bound(&format!(
            r#"METHOD["Geocentric translations (geog2D domain)",ID["EPSG",9603]],{},PARAMETER["Y-axis translation",{M}],{}"#,
            translation("X", 8605),
            translation("Z", 8607)
        )),
        bound(r#"METHOD["NTv2",ID["EPSG",9615]],PARAMETER["x"],PARAMETERFILE["Latitude and longitude difference file","g.gsb"]"#),
        // A grid's `PARAMETERFILE` without its file name, counted as a file (PROJ refuses it).
        bound(r#"METHOD["NTv2",ID["EPSG",9615]],PARAMETERFILE["Latitude and longitude difference file"]"#),
        // A transformation parameter written as `NaN`, a number to the readers (PROJ refuses it).
        bound(&format!(
            r#"METHOD["Geocentric translations (geog2D domain)",ID["EPSG",9603]],PARAMETER["X-axis translation",NaN,{M},ID["EPSG",8605]],{},{}"#,
            translation("Y", 8606),
            translation("Z", 8607)
        )),
    ];
    // Each read; and a parameter's unit without its size, which PROJ refuses, read in the CRS's
    // unit, as the tree read it.
    let mut cases: Vec<(String, bool)> = cases.into_iter().map(|text| (text, true)).collect();
    cases.push((projected(r#"ANGLEUNIT["degree"]"#, &format!(r#"AXIS["(E)",east,{M}],AXIS["(N)",north,{M}]"#)), true));
    cases
}

/// The model refuses a WKT 2 CRS the WKT 2 reader converts (`wkt2::handles`) with the message
/// terrio's tree reader refused it with (#1058 step 6d-4), for each refusal of its input, and for
/// a text with two of them, the first the reader met.
#[test]
fn the_model_refuses_wkt2_with_the_reader_s_message() {
    const DEG: &str = r#"ANGLEUNIT["degree",0.0174532925199433]"#;
    const M: &str = r#"LENGTHUNIT["metre",1]"#;
    let datum = r#"DATUM["d",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]]"#;
    let pm = format!(r#"PRIMEM["Greenwich",0,{DEG}]"#);
    let lat_lon = format!(r#"AXIS["lat",north,{DEG}],AXIS["lon",east,{DEG}]"#);
    let geog = |datum: &str, pm: &str, cs: &str| format!(r#"GEOGCRS["g",{datum}{pm},{cs}]"#);
    let geog_ok = geog(&format!("{datum},"), &pm, &format!("CS[ellipsoidal,2],{lat_lon}"));
    let geod = |axes: &str| format!(r#"GEODCRS["g",{datum},{pm},{axes}]"#);
    let xyz = |units: [&str; 3]| {
        format!(r#"CS[Cartesian,3],AXIS["X",geocentricX{}],AXIS["Y",geocentricY{}],AXIS["Z",geocentricZ{}]"#, units[0], units[1], units[2])
    };
    let tm = |unit: &str, conversion: &str| {
        format!(r#"PROJCRS["p",BASEGEOGCRS["g",{datum},{pm}],{conversion}CS[Cartesian,2],AXIS["(E)",east],AXIS["(N)",north]{unit}]"#)
    };
    let conversion = |method: &str, value: &str| {
        format!(r#"CONVERSION["c",{method}PARAMETER["Longitude of natural origin"{value},{DEG},ID["EPSG",8802]]],"#)
    };
    let tm_method = r#"METHOD["Transverse Mercator",ID["EPSG",9807]],"#;
    let rotated = |base: bool, method: &str, value: &str| {
        let base = if base { format!(r#"BASEGEOGCRS["b",{datum},{pm}],"#) } else { String::new() };
        format!(r#"GEOGCRS["r",{base}DERIVINGCONVERSION["c",{method}PARAMETER["o_lat_p"{value},{DEG}]],CS[ellipsoidal,2],{lat_lon}]"#)
    };
    let ob_tran = r#"METHOD["PROJ ob_tran o_proj=longlat"],"#;
    let wgs84 = format!(r#"GEOGCRS["WGS 84",{datum},{pm},CS[ellipsoidal,2],{lat_lon},ID["EPSG",4326]]"#);
    let three = format!(
        r#"METHOD["Geocentric translations (geog2D domain)",ID["EPSG",9603]],PARAMETER["X-axis translation",-87,{M},ID["EPSG",8605]],PARAMETER["Y-axis translation",-98,{M},ID["EPSG",8606]],PARAMETER["Z-axis translation",-121,{M},ID["EPSG",8607]]"#
    );
    let bound = |source: &str, t: bool| {
        let t = if t { format!(r#",ABRIDGEDTRANSFORMATION["t",{three}]"#) } else { String::new() };
        format!(r#"BOUNDCRS[SOURCECRS[{source}],TARGETCRS[{wgs84}]{t}]"#)
    };
    let cases = [
        // A geodetic CRS without its datum, ellipsoid, semi-major axis or inverse flattening, or
        // its prime meridian's unit without its size.
        (geog("", &pm, &format!("CS[ellipsoidal,2],{lat_lon}")), "a geodetic CRS without its datum"),
        (geog(r#"DATUM["d"],"#, &pm, &format!("CS[ellipsoidal,2],{lat_lon}")), "a datum without its ellipsoid"),
        (geog(r#"DATUM["d",ELLIPSOID["e"]],"#, &pm, &format!("CS[ellipsoidal,2],{lat_lon}")), "an ellipsoid without its semi-major axis"),
        (
            geog(r#"DATUM["d",ELLIPSOID["e",6378137]],"#, &pm, &format!("CS[ellipsoidal,2],{lat_lon}")),
            "an ellipsoid without its inverse flattening",
        ),
        (
            geog(&format!("{datum},"), r#"PRIMEM["Greenwich",0,ANGLEUNIT["degree"]]"#, &format!("CS[ellipsoidal,2],{lat_lon}")),
            "a prime meridian's unit without its size",
        ),
        // Its CS or axes.
        (geog(&format!("{datum},"), &pm, r#"CS[vertical,1],AXIS["h",up]"#), "unsupported geodetic coordinate system"),
        (geog(&format!("{datum},"), &pm, &format!("CS[spherical,3],{lat_lon}")), "planetocentric spherical CS must be two-dimensional"),
        (
            geog(&format!("{datum},"), &pm, &format!(r#"CS[ellipsoidal,2],AXIS["lon",west,{DEG}],AXIS["h",up,{DEG}]"#)),
            "invalid geographic axes",
        ),
        (
            geod(&format!(r#"CS[Cartesian,2],AXIS["X",geocentricX,{M}],AXIS["Y",geocentricY,{M}]"#)),
            "geocentric Cartesian CS must declare three axes",
        ),
        (
            geod(&format!(r#"CS[Cartesian,3],AXIS["Y",geocentricY,{M}],AXIS["X",geocentricX,{M}],AXIS["Z",geocentricZ,{M}]"#)),
            "geocentric WKT axes must be canonical X/Y/Z",
        ),
        (geod(&xyz(["", "", ""])), "geocentric axis needs a linear unit"),
        (geod(&xyz([&format!(",{M}"), &format!(",{M}"), r#",LENGTHUNIT["foot",0.3048]"#])), "geocentric axes must use one linear unit"),
        // A projected CRS without its base CRS, conversion or method, its unit without its size,
        // a parameter without its value; and without its method and a parameter without its
        // value, the method's refusal.
        (
            format!(r#"PROJCRS["p",{}CS[Cartesian,2],AXIS["(E)",east],AXIS["(N)",north],{M}]"#, conversion(tm_method, ",9")),
            "a PROJCRS without its base CRS",
        ),
        (tm(&format!(",{M}"), ""), "a PROJCRS without its CONVERSION"),
        (tm(&format!(",{M}"), &conversion("", ",9")), "a CONVERSION without its METHOD"),
        (tm(r#",LENGTHUNIT["metre"]"#, &conversion(tm_method, ",9")), "a unit without its size"),
        (tm(&format!(",{M}"), &conversion(tm_method, "")), "parameter 'Longitude of natural origin' without a value"),
        (tm(&format!(",{M}"), &conversion("", "")), "a CONVERSION without its METHOD"),
        // A derived geographic CRS without its base CRS or method, a parameter without its value;
        // without its method and a parameter without its value, the method's refusal.
        (rotated(false, ob_tran, ",39.25"), "a derived geographic CRS without its base CRS"),
        (rotated(true, "", ",39.25"), "a DERIVINGCONVERSION without its METHOD"),
        (rotated(true, ob_tran, ""), "parameter 'o_lat_p' without a value"),
        (rotated(true, "", ""), "a DERIVINGCONVERSION without its METHOD"),
        // A bound CRS without its transformation, of a source CRS refused, of a WKT 1 source CRS
        // refused.
        (bound(&geog_ok, false), "a BOUNDCRS without its ABRIDGEDTRANSFORMATION"),
        (bound(&geog("", &pm, &format!("CS[ellipsoidal,2],{lat_lon}")), true), "a geodetic CRS without its datum"),
        (
            bound(r#"GEOGCS["g",DATUM["d"],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#, true),
            "a BOUNDCRS whose WKT 1 source CRS terrio cannot read",
        ),
    ];
    let mut differ = Vec::new();
    for (text, message) in &cases {
        assert!(crate::wkt2::handles(&crate::parsed::tree(text).unwrap()), "{text}");
        let expected = format!("coordinate reference system: unsupported WKT 2: {message}");
        let refusal = wkt2_to_proj(text).err().map(|e| e.to_string());
        if refusal.as_deref() != Some(&expected) {
            differ.push(format!("{text}\n  {refusal:?}"));
        }
    }
    assert!(differ.is_empty(), "{}", differ.join("\n"));
}

/// Where the model reads unusual WKT otherwise than terrio's tree readers did, the PROJ string is
/// as PROJ 9.9.0 reads the text (pyproj 3.8.0, `CRS(wkt).to_proj4()`; #1058 step 6d-2).
#[test]
fn the_proj_string_is_read_as_proj_reads_it() {
    const DEG: &str = r#"ANGLEUNIT["degree",0.0174532925199433]"#;
    let datum = r#"DATUM["d",ELLIPSOID["WGS 84",6378137,298.257223563,LENGTHUNIT["metre",1]]]"#;
    let pm = format!(r#"PRIMEM["Greenwich",0,{DEG}]"#);
    let geogcs = |datum: &str, pm: &str| format!(r#"GEOGCS["g",DATUM["d",{datum}],{pm},UNIT["degree",0.0174532925199433]]"#);
    let wgs84 = r#"SPHEROID["WGS 84",6378137,298.257223563]"#;
    let projected = |param: &str| {
        format!(
            r#"PROJCRS["p",BASEGEOGCRS["g",{datum},{pm}],CONVERSION["c",METHOD["Transverse Mercator",ID["EPSG",9807]],PARAMETER["Longitude of natural origin",9,{param},ID["EPSG",8802]]],CS[Cartesian,2],AXIS["(E)",east,LENGTHUNIT["metre",1]],AXIS["(N)",north,LENGTHUNIT["metre",1]]]"#
        )
    };
    // Each text and its PROJ string, with PROJ 9.9.0's value and the tree readers' in the comment.
    let ll = "+proj=longlat +a=6378137 +rf=298.257223563";
    let tm = |lon: &str| format!("+proj=tmerc +lat_0=0 +lon_0={lon} +k=1 +x_0=0 +y_0=0 +to_meter=1 +a=6378137 +rf=298.257223563");
    let ob_tran =
        |to_deg: &str| format!("+proj=ob_tran +o_proj=longlat +o_lat_p=39.25 +o_lon_p=0 +lon_0=0{to_deg} +a=6378137 +rf=298.257223563");
    let cases = [
        // An ellipsoid in its own unit (PROJ `+a=6378137.001384`, rounded; the tree `+a=20925646.33`),
        // or a prime meridian (PROJ `+pm=9`; the tree `+pm=10`).
        (
            geogcs(r#"SPHEROID["local",20925646.33,298.257223563,UNIT["foot",0.3048]]"#, r#"PRIMEM["Greenwich",0]"#),
            "+proj=longlat +a=6378137.001383999 +rf=298.257223563".to_string(),
        ),
        (geogcs(wgs84, r#"PRIMEM["Local",10,UNIT["grad",0.01570796326794897]]"#), format!("{ll} +pm=9")),
        // WKT 2's long `PRIMEMERIDIAN` keyword (`+pm=2`; the tree none).
        (
            format!(
                r#"GEOGCRS["g",{datum},PRIMEMERIDIAN["Local",2,{DEG}],CS[ellipsoidal,2],AXIS["lat",north,{DEG}],AXIS["lon",east,{DEG}]]"#
            ),
            format!("{ll} +pm=2"),
        ),
        // A parameter in a time unit (PROJ reads 9 seconds of longitude as 9 radians,
        // `+lon_0=515.662015617741`; the tree `+lon_0=9`), or in a length and then an angle unit
        // (the first, its size read as radians: `+lon_0=157.173782360287`; the tree `+lon_0=8.1`).
        (projected(r#"TIMEUNIT["second",1]"#), tm("515.662015617741")),
        (projected(r#"LENGTHUNIT["foot",0.3048],ANGLEUNIT["grad",0.015707963267949]"#), tm("157.17378236028745")),
        // A derived CRS's CS unit after its axes: its grid in grads, as PROJ gives its axes (the
        // tree in degrees).
        (
            format!(
                r#"GEOGCRS["r",BASEGEOGCRS["b",{datum},{pm}],DERIVINGCONVERSION["c",METHOD["PROJ ob_tran o_proj=longlat"],PARAMETER["o_lat_p",39.25,{DEG}]],CS[ellipsoidal,2],AXIS["lon",east],AXIS["lat",north],ANGLEUNIT["grad",0.015707963267949]]"#
            ),
            ob_tran(" +to_deg=0.9"),
        ),
        // A projected CRS's `GEOGCS` before its `BASEGEOGCRS`: the first, as PROJ takes it (its
        // International 1924, `+ellps=intl`; the tree took the `BASEGEOGCRS`).
        (
            format!(
                r#"PROJCRS["p",GEOGCS["i",DATUM["i",SPHEROID["International 1924",6378388,297]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],{}"#,
                projected(DEG).strip_prefix(r#"PROJCRS["p","#).unwrap()
            ),
            tm("9").replace("+a=6378137 +rf=298.257223563", "+a=6378388 +rf=297"),
        ),
        // WKT 1's `PROJECTION` for a `CONVERSION`'s method: read, as PROJ reads it (`+lon_0=9`; the
        // tree refused it, "a CONVERSION without its METHOD").
        (projected(DEG).replace(r#"METHOD["Transverse Mercator",ID["EPSG",9807]]"#, r#"PROJECTION["Transverse Mercator"]"#), tm("9")),
    ];
    for (text, expected) in &cases {
        assert_eq!(&proj_string(text), expected, "{text}");
    }
}

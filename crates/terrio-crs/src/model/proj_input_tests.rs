// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! The model gives a WKT's PROJ string what the WKT reader reads from its tree (#1058 step 6):
//! [`super::wkt1_input`] against [`wkt1::Wkt1Input::read`], for every WKT 1 text of the #1066
//! corpus (each EPSG and ESRI CRS's GDAL WKT 1 and ESRI WKT 1, as PROJ writes them) and for
//! malformed and unusual input the corpus does not have. Until the reader takes its input from the
//! model (#1058 step 6d), this keeps the two the same.
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
    let (mut compared, mut differ) = (0, Vec::new());
    for (code, kind, text, _) in &corpus {
        let Some((tree, model)) = inputs(text) else { continue };
        compared += 1;
        if tree != model {
            differ.push(format!("{code} {kind}\n  tree  {tree}\n  model {model}"));
        }
    }
    assert!(compared > 20_000, "{compared} WKT 1 texts");
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
    let cases = [
        // Read, with a number that is not one dropped.
        projcs("Transverse_Mercator", r#",PARAMETER["central_meridian",9],PARAMETER["scale_factor","abc"],PARAMETER[5]"#, ""),
        projcs("Transverse_Mercator", r#",PARAMETER["central_meridian",9]"#, r#",AXIS["Northing",NORTH],AXIS["Easting",EAST]"#),
        projcs("Transverse_Mercator", r#",PARAMETER["central_meridian",9]"#, r#",AXIS["Easting",EAST],AXIS["Up",UP]"#),
        projcs("Krovak", r#",PARAMETER["X_Scale",-1],PARAMETER["XY_Plane_Rotation",90]"#, ""),
        // Refused: a Hotine two-point parameter that is not a finite number, a polar
        // stereographic without its latitude of origin, EPSG's variant B with a parallel that is
        // not a number.
        projcs("Hotine_Oblique_Mercator_Two_Point_Natural_Origin", r#",PARAMETER["latitude_of_point_1","abc"]"#, ""),
        projcs("Hotine_Oblique_Mercator_Two_Point_Natural_Origin", r#",PARAMETER["latitude_of_point_1",NaN]"#, ""),
        projcs("Polar_Stereographic", r#",PARAMETER["central_meridian",0]"#, ""),
        projcs("Polar_Stereographic_(variant_B)", r#",PARAMETER["Latitude of standard parallel","abc"]"#, ""),
        // Variant C by its code, whatever its name.
        r#"PROJCS["x",GEOGCS["g",DATUM["d",SPHEROID["International 1924",6378388,297]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Polar_Stereographic",AUTHORITY["EPSG","9830"]],PARAMETER["latitude_of_standard_parallel",-67],UNIT["metre",1]]"#.into(),
        // Grads and Paris; ESRI's names.
        r#"GEOGCS["NTF (Paris)",DATUM["Nouvelle_Triangulation_Francaise_Paris",SPHEROID["Clarke 1880 (IGN)",6378249.2,293.4660212936269]],PRIMEM["Paris",2.5969213],UNIT["grad",0.01570796326794897]]"#.into(),
        r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#.into(),
        // A geocentric CRS in feet; ESRI's compound CRS.
        r#"GEOCCS["g",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563],AUTHORITY["EPSG","6326"]],PRIMEM["Greenwich",0],UNIT["foot",0.3048]]"#.into(),
        format!(r#"{},VERTCS["NAVD88",VDATUM["North_American_Vertical_Datum_1988"],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",1.0],UNIT["Meter",1.0]]"#, projcs("Transverse_Mercator", "", "")),
    ];
    // The refused ones are the fifth to the eighth.
    for (i, text) in cases.iter().enumerate() {
        let (tree, model) = inputs(text).unwrap();
        assert_eq!(tree, model, "{text}");
        assert_eq!(tree == "None", (4..8).contains(&i), "{text}");
    }
}

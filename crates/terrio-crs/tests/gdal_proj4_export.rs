// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! `SpatialRef::to_proj4_gdal` writes the PROJ string GDAL 3.13.3 (PROJ 9.9.0) exports
//! (`osr.SpatialReference().SetFromUserInput(code).ExportToProj4()`, `OSR_USE_NON_DEPRECATED=NO`),
//! with its `+towgs84` and a compound CRS's geoid model: `data/gdal_proj4_export.tsv`
//! (`scripts/gen_gdal_towgs84.py`) holds GDAL's string for every fifth CRS GDAL adds either to,
//! every twentieth other one and the ones RustGIS asked about.

use terrio_crs::SpatialRef;

fn registry(code: &str) -> Option<SpatialRef> {
    let (authority, number) = code.split_once(':')?;
    let number = number.parse().ok()?;
    match authority {
        "EPSG" => SpatialRef::from_epsg(number).ok(),
        "ESRI" => SpatialRef::from_esri(number).ok(),
        _ => None,
    }
}

#[test]
fn registry_crss_are_written_as_gdal_exports_them() {
    let (mut compared, mut unread) = (0, Vec::new());
    for line in include_str!("data/gdal_proj4_export.tsv").lines() {
        let (code, gdal) = line.split_once('\t').unwrap();
        let Some(written) = registry(code).and_then(|s| s.to_proj4_gdal()) else {
            unread.push(code);
            continue;
        };
        assert_eq!(written, gdal, "{code}");
        compared += 1;
    }
    assert!(compared > 1250, "only {compared} compared");
    // The ESRI CRSs terrio does not carry: geographic CRSs of other bodies, and without
    // `wbprojection-fallback` two whose methods it has no formulas for (natearth2, qua_aut).
    let mut expected = vec!["ESRI:104916", "ESRI:104936", "ESRI:104956", "ESRI:104976", "ESRI:104996"];
    if cfg!(not(feature = "wbprojection-fallback")) {
        expected.splice(0..0, ["ESRI:53078", "ESRI:54022"]);
    }
    assert_eq!(unread, expected);
}

#[test]
fn what_gdal_adds_only_to_a_codes_own_definition() {
    // The registry's own CRS has GDAL's `+towgs84`; the same CRS given by its PROJ string, or by a
    // WKT with another datum transformation, keeps its own.
    let own = SpatialRef::from_epsg(2056).unwrap();
    assert!(own.to_proj4_gdal().unwrap().contains("+towgs84=674.374,15.056,405.346,0,0,0,0 "));
    let string = SpatialRef::from_proj4(&own.to_proj4().unwrap()).unwrap();
    assert_eq!(string.to_proj4_gdal(), Some(own.to_proj4().unwrap().replace(" +type=crs", "")));
    let bound = SpatialRef::from_proj4("+proj=longlat +ellps=bessel +towgs84=1,2,3,0,0,0,0 +no_defs").unwrap();
    assert_eq!(bound.to_proj4_gdal().as_deref(), Some("+proj=longlat +ellps=bessel +towgs84=1,2,3,0,0,0,0 +no_defs"));
    // WGS 84 and the datums a PROJ string names have none.
    for code in [4326, 4269, 4267, 27700] {
        assert!(!SpatialRef::from_epsg(code).unwrap().to_proj4_gdal().unwrap().contains("+towgs84"), "EPSG:{code}");
    }
}

#[test]
fn a_dms_prime_meridian_keeps_its_seconds_mark() {
    // A WKT's PROJ4 extension is written as given: a quote that does not follow `=` is DMS
    // seconds, not a quoted value, and `+type=crs` is still dropped.
    let wkt = r#"GEOGCS["unknown",DATUM["unknown",SPHEROID["clrk80ign",6378249.2,293.466021293627]],PRIMEM["Paris",2.33722917],UNIT["degree",0.0174532925199433],EXTENSION["PROJ4","+proj=longlat +ellps=clrk80ign +pm=2d20'14.025""E +no_defs +type=crs"]]"#;
    let written = SpatialRef::from_wkt(wkt).to_proj4_gdal();
    assert_eq!(written.as_deref(), Some(r#"+proj=longlat +ellps=clrk80ign +pm=2d20'14.025"E +no_defs"#));
}

#[test]
fn other_definitions_of_a_code_are_written_as_gdal_exports_them() {
    // GDAL binds a CRS to WGS 84 by its root identifier: its WKT 1 and WKT 2 get the code's
    // additions, the WKT without it, ESRI's WKT and the PROJ string none
    // (`data/gdal_proj4_export_inputs.tsv`, `scripts/gen_gdal_towgs84.py`).
    // Compound CRSs given by a definition, where GDAL adds a geoid model only to the code,
    // binds a WKT 1 by its horizontal part's identifier and writes `+vunits` from ESRI's WKT; and
    // EPSG:6871's PROJ string, which terrio writes on its sphere (`+R`), GDAL with `+wktext`.
    let known = [
        "EPSG:6871 wkt2",
        "EPSG:6871 wkt1-rootless",
        "EPSG:6871 esri",
        "EPSG:6871 proj",
        "EPSG:3901 wkt1",
        "EPSG:3901 wkt1-rootless",
        "EPSG:3901 esri",
    ];
    let mut differ = Vec::new();
    for line in include_str!("data/gdal_proj4_export_inputs.tsv").lines() {
        let [code, kind, input, gdal] = line.split('\t').collect::<Vec<_>>()[..] else { panic!("{line}") };
        let crs = if kind == "proj" { SpatialRef::from_proj4(input).unwrap() } else { SpatialRef::from_wkt(input) };
        if crs.to_proj4_gdal().as_deref() != Some(gdal) {
            differ.push(format!("{code} {kind}"));
        }
    }
    assert_eq!(differ, known);
}

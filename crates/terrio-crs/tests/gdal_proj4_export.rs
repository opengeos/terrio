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
        // The CRSs terrio does not carry or write: ESRI's CRSs of other bodies, and those PROJ has
        // no PROJ string for.
        let Some(written) = registry(code).and_then(|s| s.to_proj4_gdal()) else {
            unread.push(code);
            continue;
        };
        assert_eq!(written, gdal, "{code}");
        compared += 1;
    }
    assert!(compared > 1250, "only {compared} compared");
    assert!(unread.len() < 20, "not written: {unread:?}");
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

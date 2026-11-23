// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! `Transformer::has_non_ballpark_operation` says, as PROJ 9.9.0's candidate operations do, whether
//! any transformation but PROJ's ballpark links two CRSs within their extents (#1012 item 15).
//! `data/has_non_ballpark_operation.tsv` (`scripts/gen_has_non_ballpark_operation.py`, pyproj's
//! `TransformerGroup` with the network off and no grids, counting operations whose grids the PROJ
//! catalog lists for direct download) holds PROJ's answer for every EPSG geographic 2D CRS into
//! WGS 84, and some into ETRS89 and OSGB36.

use terrio_crs::{SpatialRef, Transformer};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

fn has(src: &SpatialRef, dst: &SpatialRef) -> bool {
    Transformer::new(src, dst).unwrap().has_non_ballpark_operation()
}

/// PROJ's grid catalog distinguishes names operations open from producer-only names. Missing
/// grids count by the former's direct-download URL, including non-CDN GTX and GSB files. Keep
/// grid policy changes in this isolated, uniformly offline integration binary, not unit tests.
#[test]
fn obtainable_grids_follow_the_direct_download_catalog() {
    offline();
    let dst = SpatialRef::from_epsg(4326).unwrap();
    for (name, expected) in [
        ("null", true),
        ("us_noaa_conus.tif", true),
        ("conus", true),
        ("naptrans2008.gtx", true),
        ("rdtrans2008.gsb", true),
        ("conus.las", false),
        ("unlisted_nonexistent_terrio_grid.tif", false),
        ("SIRGAS2000-to-SIRGAS-CONSIR17P01.gsb", false),
    ] {
        // A named datum keeps an explicit null-grid operation, rather than the datumless
        // ellipsoid-only case, which is a separate ballpark-policy test below.
        let src = SpatialRef::parse(&format!("+proj=longlat +datum=NAD27 +nadgrids={name}")).unwrap();
        assert_eq!(has(&src, &dst), expected, "{name}");
    }
}

#[test]
fn every_geographic_crs_answers_as_proj() {
    offline();
    let (mut checked, mut wrong) = (0, Vec::new());
    for line in include_str!("data/has_non_ballpark_operation.tsv").lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let (src, dst) = (SpatialRef::from_epsg(f[0].parse().unwrap()).unwrap(), SpatialRef::from_epsg(f[1].parse().unwrap()).unwrap());
        if has(&src, &dst) != (f[2] == "1") {
            wrong.push(line);
        }
        checked += 1;
    }
    assert_eq!(checked, 819);
    assert!(wrong.is_empty(), "{} differ from PROJ:\n{}", wrong.len(), wrong.join("\n"));
}

/// PROJ 9.9.0's answers: CRSs of one datum need none (the conversion is not a ballpark); two
/// datums known by their ellipsoids alone are one on one ellipsoid (PROJ's null transformation,
/// or a prime meridian's rotation), and have only the ballpark on two, or to a known datum; a
/// `+towgs84` shift chains with OSGB36's transformations; one whose only transformation needs a
/// grid the CDN does not serve (SIRGAS-CON SIR17P01, `SIRGAS2000-to-SIRGAS-CONSIR17P01.gsb`) has
/// none. Geocentric, 3D and compound CRSs too.
#[test]
fn proj_strings_and_other_crs_kinds_answer_as_proj() {
    offline();
    let parse = |s: &str| SpatialRef::parse(s).unwrap();
    let bessel = "+proj=longlat +ellps=bessel";
    for (src, dst, proj) in [
        ("EPSG:4326", "EPSG:3857", true),
        (bessel, "EPSG:4326", false),
        ("+proj=longlat +ellps=bessel +towgs84=598.1,73.7,418.2", "EPSG:4277", true),
        (bessel, "+proj=longlat +ellps=clrk66", false),
        (bessel, bessel, true),
        (bessel, "+proj=longlat +ellps=bessel +pm=paris", true),
        ("+proj=longlat +ellps=WGS84", "EPSG:4326", false),
        ("EPSG:27700", "EPSG:3857", true),
        ("EPSG:8987", "EPSG:4326", false),
        ("EPSG:4978", "EPSG:4326", true),
        ("EPSG:4936", "EPSG:4258", true),
        ("EPSG:7415", "EPSG:4326", true),
    ] {
        assert_eq!(has(&parse(src), &parse(dst)), proj, "{src} -> {dst}");
    }
}

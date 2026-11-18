// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! `terrio_crs::helmert_to_wgs84` gives the Helmert transformation PROJ 9.9.0 picks first without
//! grids from each EPSG geographic 2D CRS to WGS 84: `data/helmert_to_wgs84.tsv`
//! (`scripts/gen_helmert_to_wgs84.py`, pyproj's `TransformerGroup` with the network off and no
//! grids) holds its parameters as `+towgs84` carries them, or `-` and the kind of the first
//! operation when it is not one.

use terrio_crs::helmert_to_wgs84;

/// MGI (Ferro): PROJ's MGI (Ferro) to WGS 84 (1) turns longitudes by Ferro's meridian as MGI
/// 1901 has it (-17.6627833333333°, the transformation's), not the CRS's (-17.6666666666667°), so
/// that a `+towgs84` beside `+pm=ferro` would leave out 13.98" of longitude. terrio's EPSG table
/// (scripts/gen_epsg_table.py) keeps that difference as a `geogoffset dlon=13.98...` step before
/// the Helmert step, two shifts, for which terrio gives none.
const KNOWN_DIFFERENCES: [u32; 1] = [4805];

/// The parameters within a relative 1e-12: PROJ converts microradians to arc-seconds.
fn same(a: &[f64; 7], b: &[f64]) -> bool {
    b.len() == 7 && a.iter().zip(b).all(|(x, y)| (x - y).abs() <= 1e-12 * x.abs().max(1.0))
}

#[test]
fn every_geographic_crs_has_proj_s_helmert_transformation() {
    let (mut helmert, mut none, mut wrong) = (0, 0, Vec::new());
    for line in include_str!("data/helmert_to_wgs84.tsv").lines().filter(|l| !l.starts_with('#')) {
        let (code, proj) = line.split_once('\t').unwrap();
        let code: u32 = code.parse().unwrap();
        let ours = helmert_to_wgs84(code);
        if KNOWN_DIFFERENCES.contains(&code) {
            assert_eq!(ours, None, "{code}");
            continue;
        }
        if proj.starts_with("- ") {
            none += 1;
            if ours.is_some() {
                wrong.push(format!("{code}: {ours:?} vs {proj}"));
            }
            continue;
        }
        helmert += 1;
        let expected: Vec<f64> = proj.split(',').map(|v| v.parse().unwrap()).collect();
        if !ours.is_some_and(|p| same(&p, &expected)) {
            wrong.push(format!("{code}: {ours:?} vs {proj}"));
        }
    }
    // The known differences are checked whether or not the fixture has them, and it has each once.
    for code in KNOWN_DIFFERENCES {
        assert_eq!(helmert_to_wgs84(code), None, "{code}");
        let rows = include_str!("data/helmert_to_wgs84.tsv").lines().filter(|l| l.starts_with(&format!("{code}\t"))).count();
        assert_eq!(rows, 1, "{code}");
    }
    assert!(wrong.is_empty(), "{} of {} differ:\n{}", wrong.len(), helmert + none, wrong.join("\n"));
    // 717 CRSs other than WGS 84, MGI (Ferro) apart.
    assert_eq!((helmert, none), (474, 242));
}

#[test]
fn a_projected_or_compound_crs_has_its_geographic_crs_s() {
    // ED50 / UTM zone 32N, OSGB 1936 / British National Grid, Monte Mario (Rome) / Italy zone 1,
    // and the compound KKJ / Finland Uniform Coordinate System + N60 height and RT90 + RH70 height
    // (on a geographic CRS).
    for (projected, geographic) in [(23032, 4230), (27700, 4277), (26591, 4806), (3901, 4123), (7404, 4124)] {
        assert!(helmert_to_wgs84(geographic).is_some(), "{geographic}");
        assert_eq!(helmert_to_wgs84(projected), helmert_to_wgs84(geographic), "{projected}");
    }
}

#[test]
fn a_coordinate_frame_rotation_of_zero_has_no_negative_zero() {
    // PZ-90 to WGS 84 (2): coordinate frame, rx = ry = 0.
    let p = helmert_to_wgs84(4740).unwrap();
    assert!(p.iter().all(|v| *v != 0.0 || v.is_sign_positive()), "{p:?}");
}

#[test]
fn wgs_84_and_unknown_codes_have_none() {
    assert_eq!(helmert_to_wgs84(4326), None);
    assert_eq!(helmert_to_wgs84(32632), None);
    assert_eq!(helmert_to_wgs84(1), None);
}

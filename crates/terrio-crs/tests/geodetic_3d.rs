// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Points between EPSG geocentric and geographic 3D CRSs and WGS 84 (2D) and (3D), without
//! grids, are placed as PROJ 9.9.0 places them (#1012 item 8).
//!
//! From or into WGS 84 (2D), which has no heights, PROJ keeps the height through the datum shift
//! (`push v_3` before it, `pop v_3` after) and converts it to or from geocentric coordinates on the
//! geocentric CRS's own ellipsoid; the legs between geocentric CRSs before it (PZ-90 to PZ-90.11
//! (1)) run on geocentric coordinates. From WGS 84 (3D) the whole shift runs on geocentric
//! coordinates. Into PZ-90 (geocentric) (EPSG:4922) at (0°, 0°, 0 m), X was 1 m off.
//!
//! `data/geodetic_3d.tsv` (`scripts/gen_geodetic_3d.py`, PROJ 9.9.0, no grids,
//! `PROJ_NETWORK=OFF`): every EPSG geocentric and geographic 3D CRS that is not deprecated, from
//! WGS 84 (2D) and (3D) at the centre of its area at 100 m, and back into both. Tolerance: 1 mm.
//!
//! [`PENDING`] cases differ for other reasons, on main too: PROJ places GDA94 (geocentric) and
//! ONGD14 (geographic 3D) by other transformations, PZ-90.11 from and into WGS 84 (3D) is 1 m off,
//! and for LUREF terrio uses LUREF to WGS 84 (3) where PROJ uses (4), of the same accuracy (2 mm).
use terrio_crs::{SpatialRef, Transformer};

fn offline() {
    terrio_crs::set_grid_search_paths(Some(vec![]));
    terrio_crs::set_network_enabled(Some(false));
}

fn transformer(src: u32, dst: u32) -> Transformer {
    Transformer::new(&SpatialRef::from_epsg(src).unwrap(), &SpatialRef::from_epsg(dst).unwrap()).unwrap()
}

/// What terrio does not follow yet (see the module's doc comment).
const PENDING: [(&str, &str); 12] = [
    ("EPSG:4979", "EPSG:4938"),
    ("EPSG:4938", "EPSG:4979"),
    ("EPSG:4979", "EPSG:7372"),
    ("EPSG:4326", "EPSG:7372"),
    ("EPSG:4979", "EPSG:7679"),
    ("EPSG:7679", "EPSG:4979"),
    ("EPSG:4979", "EPSG:7680"),
    ("EPSG:7680", "EPSG:4979"),
    ("EPSG:4326", "EPSG:9892"),
    ("EPSG:9892", "EPSG:4326"),
    ("EPSG:4326", "EPSG:9893"),
    ("EPSG:9893", "EPSG:4326"),
];

#[test]
fn geodetic_3d_crss_are_placed_as_proj_places_them() {
    offline();
    let (mut checked, mut wrong) = (0, Vec::new());
    for line in include_str!("data/geodetic_3d.tsv").lines() {
        let f: Vec<&str> = line.split('\t').collect();
        let n: Vec<f64> = f[2..].iter().map(|s| s.parse().unwrap()).collect();
        let (src, dst) = (SpatialRef::parse(f[0]).unwrap(), SpatialRef::parse(f[1]).unwrap());
        let t = Transformer::new(&src, &dst).unwrap_or_else(|e| panic!("{} -> {}: {e}", f[0], f[1]));
        let (x, y, z) = t.transform_3d(n[0], n[1], n[2]).unwrap_or_else(|e| panic!("{} -> {}: {e}", f[0], f[1]));
        // Metres, near enough at a millimetre for longitudes and latitudes.
        let d = if dst.is_geographic() {
            ((x - n[3]) * n[4].to_radians().cos()).hypot(y - n[4]) * 111_195.0
        } else {
            (x - n[3]).hypot(y - n[4])
        };
        let dz = (z - n[5]).abs();
        let off = !(d <= 1e-3 && dz <= 1e-3);
        if PENDING.contains(&(f[0], f[1])) {
            assert!(off, "{} -> {} is now placed as PROJ places it: remove it from PENDING", f[0], f[1]);
        } else if off {
            wrong.push(format!("{} -> {} ({}, {}, {}): {d:.4} m, Z or height {dz:.4} m", f[0], f[1], n[0], n[1], n[2]));
        }
        checked += 1;
    }
    assert!(checked > 1_000, "{checked}");
    assert!(wrong.is_empty(), "{} of {checked} placed otherwise than PROJ:\n{}", wrong.len(), wrong.join("\n"));
}

#[test]
fn heights_beside_wgs_84_2d_are_kept_through_the_shift() {
    offline();
    // PROJ 9.9.0: into PZ-90 (geocentric) at (0°, 0°, 0 m), the height kept through PZ-90 to
    // WGS 84 (1) on PZ-90's ellipsoid (a = 6378136), against 6378137 from WGS 84 (3D).
    let (x, y, z) = transformer(4326, 4922).transform_3d(0.0, 0.0, 0.0).unwrap();
    assert!(
        (x - 6378135.99999939).abs() < 1e-3 && (y + 2.3500777705104867).abs() < 1e-3 && (z + 1.4999997632364837).abs() < 1e-3,
        "{x} {y} {z}"
    );
    let (x, ..) = transformer(4979, 4922).transform_3d(0.0, 0.0, 0.0).unwrap();
    assert!((x - 6378137.0).abs() < 1e-3, "{x}");
    // Back into WGS 84 (2D), the height on WGS 72BE's ellipsoid: 102.843 m, against 100 m into
    // WGS 84 (3D).
    let (x, y, z) = transformer(4979, 4986).transform_3d(10.0, 50.0, 100.0).unwrap();
    let (_, _, h) = transformer(4986, 4326).transform_3d(x, y, z).unwrap();
    assert!((h - 102.8429150832817).abs() < 1e-3, "{h}");
    let (_, _, h) = transformer(4986, 4979).transform_3d(x, y, z).unwrap();
    assert!((h - 100.00004112254828).abs() < 1e-3, "{h}");
    // PZ-90.11: kept across PZ-90 to WGS 84 (1) only; PZ-90 to PZ-90.11 (1) runs after on
    // geocentric coordinates.
    let (x, y, z) = transformer(4326, 7679).transform_3d(0.0, 0.0, 100.0).unwrap();
    assert!(
        (x - 6378233.102763138).abs() < 1e-3 && (y - 1.9560018670853931).abs() < 1e-3 && (z + 1.1685336073792076).abs() < 1e-3,
        "{x} {y} {z}"
    );
    // Through two transformations, kept across both: OSGB36 to WGS 84 (6), then PZ-90 to WGS 84
    // (1) or WGS 72BE to WGS 84 (1), from OSGB36 (2D) and British National Grid.
    for (src, dst, p, want) in [
        (4277, 4922, (-2.0, 53.0, 100.0), (3844365.9128612163, -134346.96452041809, 5070642.678425124)),
        (4277, 4986, (-2.0, 53.0, 100.0), (3844365.0897396584, -134360.70740497022, 5070641.8107247595)),
        (27700, 4922, (400000.0, 300000.0, 100.0), (3880023.130958016, -135591.99087613926, 5043559.944930366)),
    ] {
        let (x, y, z) = transformer(src, dst).transform_3d(p.0, p.1, p.2).unwrap();
        assert!((x - want.0).abs() < 1e-3 && (y - want.1).abs() < 1e-3 && (z - want.2).abs() < 1e-3, "{src} -> {dst}: {x} {y} {z}");
    }
}

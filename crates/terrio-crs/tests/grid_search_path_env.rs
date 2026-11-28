// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Empty environment variable segments must never inject an empty path (`""`) or relative `"proj"`
//! into grid search paths, which would read grids from the current working directory even after
//! confinement. Its own test binary because environment variables and CWD are process-wide.
#![cfg(not(target_family = "wasm"))]

use std::path::{Path, PathBuf};
use terrio_crs::{SpatialRef, Transformer};

/// A 2 x 2 GTX geoid grid from (0 deg, 0 deg), 1 deg apart.
fn write_tiny_gtx(path: &Path) {
    let mut b = Vec::new();
    for v in [0.0f64, 0.0, 1.0, 1.0] {
        b.extend(v.to_be_bytes());
    }
    for v in [2i32, 2] {
        b.extend(v.to_be_bytes());
    }
    for v in [10.0f32, 20.0, 30.0, 40.0] {
        b.extend(v.to_be_bytes());
    }
    std::fs::write(path, b).unwrap();
}

#[test]
fn empty_env_segments_do_not_add_cwd_to_grid_search_paths() {
    let sep = if cfg!(windows) { ';' } else { ':' };
    let empty_cache = tempfile::tempdir().unwrap();
    let grid_dir = tempfile::tempdir().unwrap();
    write_tiny_gtx(&grid_dir.path().join("cwd_only.gtx"));

    // SAFETY: single test in its own binary, before any threads are spawned.
    unsafe {
        std::env::set_var("TERRIO_GRID_DIR", format!("{sep}/opt/proj_a{sep}{sep}/opt/proj_b{sep}"));
        std::env::set_var("PROJ_DATA", "");
        std::env::set_var("PROJ_LIB", "");
        std::env::set_var("XDG_DATA_HOME", "");
        std::env::set_var("LOCALAPPDATA", "");
        std::env::set_var("TERRIO_GRID_CACHE", empty_cache.path());
    }

    let defaults = terrio_crs::default_grid_search_paths();
    assert!(defaults.iter().all(|p| !p.as_os_str().is_empty()), "empty path leaked into defaults: {defaults:?}");
    assert!(defaults.iter().all(|p| p != Path::new("proj")), "relative 'proj' leaked into defaults: {defaults:?}");
    assert!(defaults.contains(&PathBuf::from("/opt/proj_a")), "valid segment missing: {defaults:?}");
    assert!(defaults.contains(&PathBuf::from("/opt/proj_b")), "valid segment missing: {defaults:?}");

    let src = SpatialRef::parse("+proj=longlat +datum=WGS84 +geoidgrids=cwd_only.gtx +vunits=m").unwrap();
    let dst = SpatialRef::from_epsg(4979).unwrap();

    // Positive control: the fixture loads and shifts height when its directory is explicitly listed.
    terrio_crs::set_grid_search_paths(Some(vec![grid_dir.path().to_path_buf()]));
    terrio_crs::set_network_enabled(Some(false));
    let t_pos = Transformer::new(&src, &dst).unwrap();
    let (_, _, z_pos) = t_pos.transform_3d(0.25, 0.5, 100.0).expect("explicitly configured grid directory must load cwd_only.gtx");
    assert!((z_pos - 100.0).abs() > 1.0, "expected geoid shift from cwd_only.gtx, got {z_pos}");

    // Now set only empty search env vars, chdir to a directory containing the grid, confine
    // search paths, and verify the grid in CWD is NOT loaded.
    // SAFETY: single test in its own binary, no other threads.
    unsafe {
        std::env::set_var("TERRIO_GRID_DIR", "");
        std::env::set_var("PROJ_DATA", "");
        std::env::set_var("PROJ_LIB", "");
        std::env::set_var("XDG_DATA_HOME", "");
        std::env::set_var("LOCALAPPDATA", "");
        std::env::set_var("HOME", "");
        std::env::set_var("USERPROFILE", "");
        std::env::set_var("XDG_CACHE_HOME", "");
        std::env::set_var("TERRIO_GRID_CACHE", "");
    }
    let defaults = terrio_crs::default_grid_search_paths();
    for relative in [".local/share/proj", "terrio/grids", ".cache/terrio/grids"] {
        assert!(!defaults.contains(&PathBuf::from(relative)), "relative home/cache path leaked: {defaults:?}");
    }
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(grid_dir.path()).unwrap();

    terrio_crs::set_grid_search_paths(None);
    terrio_crs::set_network_enabled(Some(false));
    terrio_crs::confine_grid_search_paths();

    let t = Transformer::new(&src, &dst).unwrap();
    let result = t.transform_3d(0.25, 0.5, 100.0);

    std::env::set_current_dir(prev_cwd).unwrap();
    let err = result.expect_err("empty env vars allowed loading a grid from CWD");
    assert!(matches!(err, terrio_crs::Error::Crs(ref msg) if msg.contains("cannot transform")), "unexpected error: {err:?}");
}

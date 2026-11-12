// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Locates real-world files under `TERRIO_TEST_DATA` (see `scripts/fetch_test_data.sh`).

/// Hides the datum shift grids of the machine from the transformations of real-data tests, as CI
/// sees them: the expected values were computed by GDAL 3.12.2 without grids, and terrio would otherwise
/// search PROJ's user directory (`~/.local/share/proj`, where `projsync` writes) and `PROJ_DATA`.
/// Network access is off as well.
fn without_grids() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        terrio::crs::set_grid_search_paths(Some(Vec::new()));
        terrio::crs::set_network_enabled(Some(false));
    });
}

/// The path of `name` in `TERRIO_TEST_DATA`, or `None` when the variable is unset so that
/// real-data tests pass without checking anything. Panics when the file is missing.
/// The first lookup also disables process-wide grid search and networking. Binaries using
/// this helper keep grid-policy tests in separate binaries so parallel tests do not reset it.
pub fn data(name: &str) -> Option<String> {
    without_grids();
    let p = std::path::PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?).join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

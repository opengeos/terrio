// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Locates real-world files under `TERRIO_TEST_DATA` (see `scripts/fetch_test_data.sh`).

/// The path of `name` in `TERRIO_TEST_DATA`, or `None` when the variable is unset so that
/// real-data tests pass without checking anything. Panics when the file is missing.
pub fn data(name: &str) -> Option<String> {
    let p = std::path::PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?).join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

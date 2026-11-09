// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! `TERRIO_ALLOWED_DRIVERS` in the process environment: set but empty or blank, it allows no
//! driver, and a thread's scope only narrows it. A test binary of its own, as it sets the
//! environment variable.

use std::collections::HashMap;
use terrio::formats::allowed_drivers;

const OPTION: &str = "TERRIO_ALLOWED_DRIVERS";

/// A list of drivers, as [`allowed_drivers`] returns one.
fn names(names: &[&str]) -> Option<Vec<String>> {
    Some(names.iter().map(|n| n.to_string()).collect())
}

/// The drivers allowed, and whether `path` opens, with the option `value` (unset for `None`) in
/// a scope of the calling thread.
fn in_scope(value: Option<&str>, path: &str) -> (Option<Vec<String>>, bool) {
    terrio::config::push_scope(HashMap::from([(OPTION.to_string(), value.map(str::to_string))]));
    let seen = (allowed_drivers(None), terrio::open(path).is_ok());
    terrio::config::pop_scope();
    seen
}

#[test]
fn an_empty_list_allows_nothing_and_a_scope_cannot_widen_the_environments() {
    let dir = tempfile::tempdir().unwrap();
    let grid = dir.path().join("grid.asc");
    std::fs::write(&grid, "ncols 2\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2\n3 4\n").unwrap();
    let grid = grid.to_str().unwrap();
    // SAFETY: this binary runs only this test, so no other test thread reads the environment while
    // it changes. Terrio's rayon pool threads may outlive a parallel job, but they are idle between
    // jobs and read no environment variable here.
    unsafe { std::env::remove_var(OPTION) };
    assert_eq!(in_scope(None, grid), (None, true), "no list");
    for value in ["", " ", ","] {
        // SAFETY: as above.
        unsafe { std::env::set_var(OPTION, value) };
        assert_eq!(in_scope(None, grid), (names(&[]), false), "the environment's {value:?}");
    }
    // SAFETY: as above.
    unsafe { std::env::set_var(OPTION, "GTiff") };
    for value in [Some(""), Some(" "), Some("AAIGrid")] {
        assert_eq!(in_scope(value, grid), (names(&[]), false), "a scope's {value:?}");
    }
    assert_eq!(in_scope(None, grid), (names(&["GTIFF"]), false), "a scope that unsets it");
    assert_eq!(in_scope(Some("AAIGrid, GTiff"), grid), (names(&["GTIFF"]), false));
    // The environment's list narrows a scope set before it.
    // SAFETY: as above.
    unsafe { std::env::remove_var(OPTION) };
    terrio::config::push_scope(HashMap::from([(OPTION.to_string(), Some("GTiff,AAIGrid".to_string()))]));
    let before = (allowed_drivers(None), terrio::open(grid).is_ok());
    // SAFETY: as above.
    unsafe { std::env::set_var(OPTION, "GTiff") };
    let after = (allowed_drivers(None), terrio::open(grid).is_ok());
    terrio::config::pop_scope();
    // SAFETY: as above.
    unsafe { std::env::remove_var(OPTION) };
    assert_eq!(before, (names(&["GTIFF", "AAIGRID"]), true));
    assert_eq!(after, (names(&["GTIFF"]), false));
    // A value that is not UTF-8 is set, and read lossily it names no driver: it allows none, alone,
    // in the value a scope set after it holds, and under a scope set before it.
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let invalid = std::ffi::OsStr::from_bytes(b"AAIGrid\xff");
        let scope = || terrio::config::push_scope(HashMap::from([(OPTION.to_string(), Some("AAIGrid".to_string()))]));
        // SAFETY: as above.
        unsafe { std::env::set_var(OPTION, invalid) };
        assert_eq!(in_scope(None, grid), (names(&["AAIGRID\u{fffd}"]), false), "not UTF-8");
        scope();
        let held = terrio::config::thread_options().get(OPTION).cloned();
        let after = (allowed_drivers(None), terrio::open(grid).is_ok());
        terrio::config::pop_scope();
        assert_eq!((held, after), (Some(Some(String::new())), (names(&[]), false)), "a scope set after it");
        // SAFETY: as above.
        unsafe { std::env::remove_var(OPTION) };
        scope();
        // SAFETY: as above.
        unsafe { std::env::set_var(OPTION, invalid) };
        let before = (allowed_drivers(None), terrio::open(grid).is_ok());
        terrio::config::pop_scope();
        // SAFETY: as above.
        unsafe { std::env::remove_var(OPTION) };
        assert_eq!(before, (names(&[]), false), "a scope set before it");
    }
}

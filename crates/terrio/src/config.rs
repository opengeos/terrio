// Written to match the behavior of:
//   GDAL v3.12.1:
//     port/cpl_conv.cpp
//   rasterio 1.5.2:
//     rasterio/env.py
// No code is translated from these sources.

//! Configuration options: environment variables, overridden for the calling thread.
//!
//! terrio reads its settings from environment variables named as GDAL's configuration options.
//! GDAL also lets an option be set for one thread only (`CPLSetThreadLocalConfigOption`), and
//! that is how rasterio's `Env` sets options: two threads in one process can read with different
//! credentials without seeing each other's. [`get`] gives the same precedence here — the innermost
//! scope pushed on the calling thread with [`push_scope`], then the process environment — so the
//! bindings' `Env` can scope options to a thread rather than write them into the process
//! environment, where every thread would see them.
//!
//! Scopes do not follow work handed to other threads (a thread pool decoding tiles in parallel):
//! those threads see the process environment. Components that fetch from other threads capture
//! what they need from the calling thread when they are created.

use std::cell::RefCell;
use std::collections::HashMap;

thread_local! {
    /// The calling thread's scopes, innermost last. `None` unsets an option within a scope.
    static SCOPES: RefCell<Vec<HashMap<String, Option<String>>>> = const { RefCell::new(Vec::new()) };
}

/// The value of the option `key`: the innermost scope of the calling thread that sets (or unsets)
/// it, else the environment variable. Empty values count as unset.
pub fn get(key: &str) -> Option<String> {
    let scoped = SCOPES.with(|s| s.borrow().iter().rev().find_map(|scope| scope.get(key).cloned()));
    let value = match scoped {
        Some(v) => v,
        None => std::env::var(key).ok(),
    };
    value.filter(|v| !v.trim().is_empty())
}

/// Whether `GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR` asks that nothing next to a dataset be read:
/// GDAL then treats the dataset's directory as empty, so no `.aux.xml`, world file or other
/// sidecar is consulted (a tile server opens untrusted assets so).
pub fn sidecars_disabled() -> bool {
    get_any(&["TERRIO_DISABLE_READDIR_ON_OPEN", "GDAL_DISABLE_READDIR_ON_OPEN"]).is_some_and(|v| v.trim().eq_ignore_ascii_case("EMPTY_DIR"))
}

/// The first of `keys` that is set (see [`get`]).
pub fn get_any(keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|k| get(k))
}

/// Pushes a scope of options for the calling thread; `None` unsets an option in it.
pub fn push_scope(options: HashMap<String, Option<String>>) {
    SCOPES.with(|s| s.borrow_mut().push(options));
}

/// Pops the calling thread's innermost scope. Returns whether there was one.
pub fn pop_scope() -> bool {
    SCOPES.with(|s| s.borrow_mut().pop().is_some())
}

/// The options set by the calling thread's scopes, innermost winning.
pub fn thread_options() -> HashMap<String, Option<String>> {
    SCOPES.with(|s| {
        let mut out = HashMap::new();
        for scope in s.borrow().iter() {
            out.extend(scope.iter().map(|(k, v)| (k.clone(), v.clone())));
        }
        out
    })
}

/// Runs `f` with the captured thread options, replacing any worker scopes for its duration.
/// Nested pool work can suspend another task on this worker; its options must not leak into
/// this snapshot. The previous stack is restored even when `f` panics.
#[cfg(any(test, feature = "zarr"))]
pub(crate) fn with_thread_options<T>(options: HashMap<String, Option<String>>, f: impl FnOnce() -> T) -> T {
    struct Restore(Vec<HashMap<String, Option<String>>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SCOPES.with(|s| *s.borrow_mut() = std::mem::take(&mut self.0));
        }
    }
    let previous = SCOPES.with(|s| std::mem::replace(&mut *s.borrow_mut(), vec![options]));
    let _restore = Restore(previous);
    f()
}

/// Runs `f` with `options` set on this thread, removing them afterwards even when `f` panics, so a
/// failing test cannot leave them to the next on the thread.
#[cfg(test)]
pub(crate) fn with_options<T>(options: &[(&str, &str)], f: impl FnOnce() -> T) -> T {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            pop_scope();
        }
    }
    push_scope(options.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect());
    let _pop = Pop;
    f()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_handed_to_another_thread_apply_there() {
        let key = "TERRIO_CONFIG_TEST_HANDED";
        let seen = with_options(&[(key, "set")], || {
            let options = thread_options();
            std::thread::spawn(move || (get(key), with_thread_options(options, || get(key)), get(key))).join().unwrap()
        });
        assert_eq!(seen, (None, Some("set".to_string()), None));
    }

    #[test]
    fn captured_options_isolate_worker_scopes_and_restore_after_nested_work_and_panics() {
        let key = "TERRIO_CONFIG_TEST_WORKER";
        with_options(&[(key, "worker")], || {
            with_thread_options(HashMap::new(), || {
                assert_eq!(get(key), None);
                with_thread_options(HashMap::from([(key.into(), Some("nested".into()))]), || {
                    assert_eq!(get(key).as_deref(), Some("nested"));
                });
                assert_eq!(get(key), None);
            });
            assert_eq!(get(key).as_deref(), Some("worker"));
            let result = std::panic::catch_unwind(|| {
                with_thread_options(HashMap::from([(key.into(), None)]), || {
                    assert_eq!(get(key), None);
                    assert_eq!(thread_options().get(key), Some(&None));
                    panic!("restore the worker snapshot");
                });
            });
            assert!(result.is_err());
            assert_eq!(get(key).as_deref(), Some("worker"));
        });
        assert_eq!(get(key), None);
    }

    #[test]
    fn scopes_override_the_environment_on_their_thread_only() {
        let key = "TERRIO_CONFIG_TEST_OPTION";
        assert_eq!(get(key), None);
        push_scope(HashMap::from([(key.to_string(), Some("outer".to_string()))]));
        push_scope(HashMap::from([(key.to_string(), Some("inner".to_string()))]));
        assert_eq!(get(key).as_deref(), Some("inner"));
        // Another thread does not see this thread's scopes.
        assert_eq!(std::thread::spawn(move || get(key)).join().unwrap(), None);
        assert!(pop_scope());
        assert_eq!(get(key).as_deref(), Some("outer"));
        push_scope(HashMap::from([(key.to_string(), None)]));
        assert_eq!(get(key), None, "a scope can unset an option");
        assert!(pop_scope());
        assert!(pop_scope());
        assert!(!pop_scope());
        assert_eq!(get(key), None);
    }
}

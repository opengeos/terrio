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
    scoped(key).unwrap_or_else(|| std::env::var(key).ok()).filter(|v| !v.trim().is_empty())
}

/// The value of the option `key`, found as [`get`] finds it, but with an empty or blank value
/// counting as set: for an option whose empty value says something (an allow-list that allows
/// nothing). An environment value that is not UTF-8 is set too, and read as empty.
pub(crate) fn get_including_empty(key: &str) -> Option<String> {
    scoped(key).unwrap_or_else(|| std::env::var_os(key).map(|v| v.into_string().unwrap_or_default()))
}

/// The value the innermost scope of the calling thread that sets or unsets `key` gives it:
/// `Some(None)` when that scope unsets it, `None` when no scope does.
fn scoped(key: &str) -> Option<Option<String>> {
    SCOPES.with(|s| s.borrow().iter().rev().find_map(|scope| scope.get(key).cloned()))
}

/// Whether `GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR` asks that nothing next to a dataset be read:
/// GDAL then treats the dataset's directory as empty, so no `.aux.xml`, world file or other
/// sidecar is consulted (a tile server opens untrusted assets so).
pub fn sidecars_disabled() -> bool {
    get_any(&["TERRIO_DISABLE_READDIR_ON_OPEN", "GDAL_DISABLE_READDIR_ON_OPEN"]).is_some_and(|v| v.trim().eq_ignore_ascii_case("EMPTY_DIR"))
}

/// Resolves aliases by scope first, then key order within that scope, then the environment.
/// The innermost scope mentioning any alias wins. An explicit `None` or blank value unsets
/// the logical option; it does not fall through to an outer alias or the environment.
pub fn get_any(keys: &[&str]) -> Option<String> {
    let scoped = SCOPES.with(|scopes| scopes.borrow().iter().rev().find_map(|scope| keys.iter().find_map(|key| scope.get(*key).cloned())));
    scoped
        .unwrap_or_else(|| keys.iter().find_map(|key| std::env::var(key).ok().filter(|value| !value.trim().is_empty())))
        .filter(|value| !value.trim().is_empty())
}

/// Pushes a scope of options for the calling thread; `None` unsets an option in it. The driver
/// allow-list (`TERRIO_ALLOWED_DRIVERS`) only narrows: the scope keeps the drivers its value names
/// that the value outside it allows too, and unsetting it keeps the value outside.
pub fn push_scope(mut options: HashMap<String, Option<String>>) {
    if let Some(value) = options.get_mut(crate::formats::ALLOWED_DRIVERS_OPTION) {
        *value = crate::formats::scoped_allowed_drivers(
            get_including_empty(crate::formats::ALLOWED_DRIVERS_OPTION).as_deref(),
            value.as_deref(),
        );
    }
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

/// An ordered snapshot of thread scopes, retaining alias precedence and explicit unsets.
#[derive(Clone, Default)]
pub struct ThreadOptions(Vec<HashMap<String, Option<String>>>);

impl ThreadOptions {
    /// Captures the calling thread's scopes, excluding process environment variables.
    pub fn capture() -> Self {
        SCOPES.with(|s| Self(s.borrow().clone()))
    }

    /// Adds an innermost override to the snapshot.
    pub fn insert(&mut self, key: String, value: Option<String>) {
        self.0.push(HashMap::from([(key, value)]));
    }

    /// Resolves aliases in the captured scopes, then in the current process environment.
    pub fn get_any(&self, keys: &[&str]) -> Option<String> {
        let scoped = self.0.iter().rev().find_map(|scope| keys.iter().find_map(|key| scope.get(*key).cloned()));
        scoped
            .unwrap_or_else(|| keys.iter().find_map(|key| std::env::var(key).ok().filter(|v| !v.trim().is_empty())))
            .filter(|v| !v.trim().is_empty())
    }

    /// Pushes captured scopes onto this thread. The returned guard pops them on drop.
    pub fn enter(&self) -> ThreadOptionsScope {
        for scope in &self.0 {
            push_scope(scope.clone());
        }
        ThreadOptionsScope { depth: self.0.len(), _thread: std::marker::PhantomData }
    }
}

/// A lexical guard for scopes installed from a snapshot.
pub struct ThreadOptionsScope {
    depth: usize,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}

impl Drop for ThreadOptionsScope {
    fn drop(&mut self) {
        for _ in 0..self.depth {
            pop_scope();
        }
    }
}

/// Runs `f` with the captured thread options, replacing any worker scopes for its duration.
/// Nested pool work can suspend another task on this worker; its options must not leak into
/// this snapshot. The previous stack is restored even when `f` panics.
/// Use only around blocking leaf operations: `f` must not yield into nested pool work,
/// which could run unrelated tasks before the temporary thread-local scope is restored.
#[cfg(any(test, remote, feature = "zarr", all(feature = "pointcloud", not(target_family = "wasm"))))]
pub(crate) fn with_thread_options<T>(options: ThreadOptions, f: impl FnOnce() -> T) -> T {
    struct Restore(Vec<HashMap<String, Option<String>>>);
    impl Drop for Restore {
        fn drop(&mut self) {
            SCOPES.with(|s| *s.borrow_mut() = std::mem::take(&mut self.0));
        }
    }
    let previous = SCOPES.with(|s| std::mem::replace(&mut *s.borrow_mut(), options.0));
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
    fn aliases_resolve_the_innermost_scope_before_key_priority() {
        let primary = "TERRIO_CONFIG_ALIAS_TEST";
        let alternate = "GDAL_CONFIG_ALIAS_TEST";
        with_options(&[(primary, "outer")], || {
            with_options(&[(alternate, "inner")], || {
                assert_eq!(get_any(&[primary, alternate]).as_deref(), Some("inner"));
                assert_eq!(get(primary).as_deref(), Some("outer"));
            });
        });
        with_options(&[(alternate, "outer")], || {
            with_options(&[(primary, "inner")], || {
                assert_eq!(get_any(&[primary, alternate]).as_deref(), Some("inner"));
            });
        });
        with_options(&[(primary, "first"), (alternate, "second")], || {
            assert_eq!(get_any(&[primary, alternate]).as_deref(), Some("first"));
        });
    }

    #[test]
    fn an_inner_alias_can_explicitly_unset_the_option() {
        let keys = ["TERRIO_CONFIG_ALIAS_UNSET", "GDAL_CONFIG_ALIAS_UNSET"];
        with_options(&[(keys[0], "outer")], || {
            with_options(&[(keys[1], "   ")], || assert_eq!(get_any(&keys), None));
            push_scope(HashMap::from([(keys[1].to_string(), None)]));
            let value = get_any(&keys);
            assert!(pop_scope());
            assert_eq!(value, None);
        });
    }

    #[test]
    fn captured_alias_scopes_keep_precedence_and_unsets_on_workers() {
        let keys = ["TERRIO_CAPTURE_ALIAS", "GDAL_CAPTURE_ALIAS"];
        with_options(&[(keys[0], "outer")], || {
            for value in [Some("inner".to_string()), None, Some("  ".to_string())] {
                push_scope(HashMap::from([(keys[1].into(), value.clone())]));
                let snapshot = ThreadOptions::capture();
                assert!(pop_scope());
                let expected = value.filter(|v| !v.trim().is_empty());
                let actual = std::thread::spawn(move || with_thread_options(snapshot, || get_any(&keys))).join().unwrap();
                assert_eq!(actual, expected);
            }
        });
    }

    #[test]
    fn options_handed_to_another_thread_apply_there() {
        let key = "TERRIO_CONFIG_TEST_HANDED";
        let seen = with_options(&[(key, "set")], || {
            let options = ThreadOptions::capture();
            std::thread::spawn(move || (get(key), with_thread_options(options, || get(key)), get(key))).join().unwrap()
        });
        assert_eq!(seen, (None, Some("set".to_string()), None));
    }

    #[test]
    fn captured_options_isolate_worker_scopes_and_restore_after_nested_work_and_panics() {
        let key = "TERRIO_CONFIG_TEST_WORKER";
        with_options(&[(key, "worker")], || {
            with_thread_options(ThreadOptions::default(), || {
                assert_eq!(get(key), None);
                with_thread_options(ThreadOptions(vec![HashMap::from([(key.into(), Some("nested".into()))])]), || {
                    assert_eq!(get(key).as_deref(), Some("nested"));
                });
                assert_eq!(get(key), None);
            });
            assert_eq!(get(key).as_deref(), Some("worker"));
            let result = std::panic::catch_unwind(|| {
                with_thread_options(ThreadOptions(vec![HashMap::from([(key.into(), None)])]), || {
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

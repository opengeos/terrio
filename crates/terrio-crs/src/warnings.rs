//! Warnings: problems that do not stop an operation but change its result, such as a CRS that
//! terrio reads but cannot represent. They are kept for the thread that raised them until taken
//! with [`take`], so that callers (the command line, the Python bindings) can report them.
//! Warnings are recorded on the caller's thread, during I/O and processing: those raised on other
//! threads (a worker of a parallel operation) are kept for those threads and not reported.

use std::cell::RefCell;

/// The most warnings kept: older ones are dropped.
const MAX: usize = 100;

thread_local! {
    static WARNINGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    static CAPTURES: RefCell<Vec<Vec<String>>> = const { RefCell::new(Vec::new()) };
}

/// Records a warning on this thread (once: a repeated one is not recorded again). A warning
/// raised while the thread's locals are destroyed (by a `Transformer` dropped from a caller's
/// `thread_local!`) is dropped: there is no list left to keep it, nor anyone to take it.
pub(crate) fn warn(message: String) {
    let _ = CAPTURES.try_with(|captures| {
        for captured in captures.borrow_mut().iter_mut() {
            if !captured.contains(&message) {
                if captured.len() == MAX {
                    captured.remove(0);
                }
                captured.push(message.clone());
            }
        }
    });
    let _ = WARNINGS.try_with(|w| {
        let mut w = w.borrow_mut();
        if !w.contains(&message) {
            if w.len() == MAX {
                w.remove(0);
            }
            w.push(message);
        }
    });
}

/// Captures diagnostics for a cached result without consuming the caller's pending warnings.
pub(crate) fn capture<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    struct Capture;
    impl Drop for Capture {
        fn drop(&mut self) {
            CAPTURES.with(|captures| {
                captures.borrow_mut().pop();
            });
        }
    }
    CAPTURES.with(|captures| captures.borrow_mut().push(Vec::new()));
    let guard = Capture;
    let result = f();
    let captured = CAPTURES.with(|captures| std::mem::take(captures.borrow_mut().last_mut().unwrap()));
    drop(guard);
    (result, captured)
}

/// Takes the warnings recorded on this thread since the last call, oldest first.
///
/// ```
/// let _ = terrio_crs::take_warnings(); // Nothing to report yet.
/// assert!(terrio_crs::take_warnings().is_empty());
/// ```
pub fn take() -> Vec<String> {
    WARNINGS.try_with(|w| std::mem::take(&mut *w.borrow_mut())).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #[test]
    fn capture_keeps_pending_diagnostics_and_cleans_up_after_a_panic() {
        super::take();
        super::warn("pending".into());
        let (_, captured) = super::capture(|| super::warn("pending".into()));
        assert_eq!(captured, vec!["pending"]);
        assert_eq!(super::take(), vec!["pending"]);
        assert!(std::panic::catch_unwind(|| super::capture(|| panic!("interrupted lookup"))).is_err());
        assert!(super::CAPTURES.with(|captures| captures.borrow().is_empty()));
        let (_, captured) = super::capture(|| super::warn("later".into()));
        assert_eq!(captured, vec!["later"]);
        assert_eq!(super::take(), vec!["later"]);
    }
}

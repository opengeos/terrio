//! Warnings: problems that do not stop an operation but change its result, such as a CRS that
//! terrio reads but cannot represent. They are kept for the thread that raised them until taken
//! with [`take`], so that callers (the command line, the Python bindings) can report them.
//! Warnings are raised where datasets are opened, on the caller's thread: those raised on other
//! threads (a worker of a parallel operation) are kept for those threads and not reported.

use std::cell::RefCell;

/// The most warnings kept: older ones are dropped.
const MAX: usize = 100;

thread_local! {
    static WARNINGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Records a warning on this thread (once: a repeated one is not recorded again).
pub(crate) fn warn(message: String) {
    WARNINGS.with(|w| {
        let mut w = w.borrow_mut();
        if !w.contains(&message) {
            if w.len() == MAX {
                w.remove(0);
            }
            w.push(message);
        }
    });
}

/// Takes the warnings recorded on this thread since the last call, oldest first.
///
/// ```
/// let _ = terrio::warnings::take(); // Nothing to report yet.
/// assert!(terrio::warnings::take().is_empty());
/// ```
pub fn take() -> Vec<String> {
    WARNINGS.with(|w| std::mem::take(&mut *w.borrow_mut()))
}

//! Warnings: problems that do not stop an operation but change its result, such as a CRS that
//! terrio reads but cannot represent. They are kept for the thread that raised them until taken
//! with [`take`], so that callers (the command line, the Python bindings) can report them.
//! Warnings are recorded on the thread that raises them. Workers must explicitly return their
//! warnings to the caller for reporting, as the streamed TIFF writer does when joined.

use std::cell::RefCell;

/// The most warnings kept: older ones are dropped.
const MAX: usize = 100;

thread_local! {
    static WARNINGS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// Records a warning on this thread (once: a repeated one is not recorded again). A warning
/// raised while the thread's locals are destroyed (by a `Transformer` dropped from a caller's
/// `thread_local!`) is dropped: there is no list left to keep it, nor anyone to take it.
pub(crate) fn warn(message: String) {
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

/// Takes the warnings recorded on this thread since the last call, including standalone CRS
/// grid diagnostics. Each queue retains its recording order; duplicate messages are combined.
/// Credential-bearing locations are redacted before delivering standalone CRS diagnostics.
///
/// ```
/// let _ = terrio::warnings::take(); // Nothing to report yet.
/// assert!(terrio::warnings::take().is_empty());
/// ```
pub fn take() -> Vec<String> {
    // Grid discovery records diagnostics directly in the standalone CRS engine.
    // Drain both queues so every public surface reports those refusals too.
    for message in terrio_crs::take_warnings() {
        warn(crate::vsi::redact_diagnostic_location(&message).into_owned());
    }
    WARNINGS.try_with(|w| std::mem::take(&mut *w.borrow_mut())).unwrap_or_default()
}

# Interface decision from the custodian

The production caller can and will propagate a repair Result through the tile writer.
Use the fallible `try_make_valid` API for production. An internal invariant violation
must become an explicit error; no panic, silent omission or successful empty output.
The original infallible package adapter is a harness limitation, not a requirement
to keep a panicking production wrapper. You may adapt candidate transport/tests
to invoke the fallible API and preserve the protocol error status. The custodian
will remove an unused compatibility wrapper during integration if necessary.

This message conveys only an interface/error-policy decision. It supplies no
replacement algorithm or prohibited implementation details.

## Final validation priority

The custodian stopped the optional long stress-oracle Python process after
several minutes of CPU time. Keep this explicitly incomplete, not a pass. The
required sparse/dense stress runs and seeded differential checks already provide
useful evidence. Finish fmt, strict Clippy, WASM, native tests, the immutable
51-case replay and a clear validation record. Do not spend another extended run
on this optional stress oracle. The fallible production API decision above is
approved; production integration has removed the infallible panic wrapper.

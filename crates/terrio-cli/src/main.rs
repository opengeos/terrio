//! The `terrio` command-line tool.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Exit quietly when the reader of our output goes away (`terrio formats | head`).
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    // glibc raises its mmap threshold as large blocks are freed, so that later ones come from the
    // per-thread arenas, which keep the memory once it is freed: a fixed threshold returns blocks
    // of 4 MiB and more to the system when they are freed, which lowered peak memory by 12-60% on
    // large conversions for a few percent of time. A threshold set in the environment
    // (`MALLOC_MMAP_THRESHOLD_`, or `glibc.malloc.mmap_threshold` in `GLIBC_TUNABLES`) is kept.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if std::env::var_os("MALLOC_MMAP_THRESHOLD_").is_none()
        && !std::env::var_os("GLIBC_TUNABLES").is_some_and(|t| t.to_string_lossy().contains("glibc.malloc.mmap_threshold"))
    {
        // SAFETY: called once, before any other thread is started. A refused value (0 returned)
        // leaves glibc's default in place, so the result is not checked.
        unsafe {
            libc::mallopt(libc::M_MMAP_THRESHOLD, 4 << 20);
        }
    }
    let code = terrio_cli::run(std::env::args_os().collect());
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

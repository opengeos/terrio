//! The `terrio` command-line tool.

use std::process::ExitCode;

fn main() -> ExitCode {
    // Exit quietly when the reader of our output goes away (`terrio formats | head`).
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let code = terrio_cli::run(std::env::args_os().collect());
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

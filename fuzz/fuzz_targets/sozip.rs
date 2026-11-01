//! Zip archives, as the SOZip functions read them: validated (every member decompressed, every
//! SOZip index checked) and listed. Their first members are then opened through `/vsizip/` as a
//! lazy raster read opens them (stored and seek-optimized members in place, others extracted),
//! with a window read when a member is a TIFF.
#![no_main]

use libfuzzer_sys::fuzz_target;

/// The most members opened: each open reads the archive's central directory again.
const MAX_MEMBERS: usize = 4;

fuzz_target!(|data: &[u8]| {
    // The SOZip functions take paths: the archive is written to a file named after this process.
    let path = std::env::temp_dir().join(format!("terrio-fuzz-sozip-{}.zip", std::process::id()));
    // A temporary directory whose path is not UTF-8 cannot be named in a `&str` path: skip.
    let Some(archive) = path.to_str().map(str::to_owned) else { return };
    if std::fs::write(&path, data).is_err() {
        return;
    }
    let _ = terrio::vsi::sozip::validate(&archive);
    for member in terrio::vsi::sozip::list(&archive).unwrap_or_default().iter().take(MAX_MEMBERS) {
        let Ok(Some(mut r)) = terrio::LazyRaster::open_tiff(&format!("/vsizip/{{{archive}}}/{}", member.name)) else { continue };
        let window = terrio::Window::new(0, 0, r.width().min(64), r.height().min(64));
        let _ = r.read_window(window, &terrio::WindowOptions::default());
    }
    let _ = std::fs::remove_file(&path);
});

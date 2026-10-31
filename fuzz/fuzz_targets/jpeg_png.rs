//! JPEG and PNG files, decoded whole as the JPEG and PNG drivers read them.
#![no_main]

use libfuzzer_sys::fuzz_target;

/// The most pixels decoded: larger images are the allocation tests' concern, not the decoders'.
const MAX_PIXELS: u64 = 1 << 22;

/// The size a PNG's `IHDR` or a JPEG's frame header declares, when the first bytes show it.
fn declared_size(data: &[u8]) -> Option<(u64, u64)> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        let be = |i: usize| Some(u32::from_be_bytes(data.get(i..i + 4)?.try_into().ok()?) as u64);
        return Some((be(16)?, be(20)?));
    }
    // The first start-of-frame marker of any kind.
    data.windows(9).find(|m| m[0] == 0xFF && (0xC0..=0xCF).contains(&m[1]) && ![0xC4, 0xC8, 0xCC].contains(&m[1])).map(|m| {
        (u16::from_be_bytes([m[7], m[8]]) as u64, u16::from_be_bytes([m[5], m[6]]) as u64)
    })
}

fuzz_target!(|data: &[u8]| {
    if declared_size(data).is_some_and(|(w, h)| w.saturating_mul(h) > MAX_PIXELS) {
        return;
    }
    let name = if data.starts_with(b"\x89PNG") { "fuzz.png" } else { "fuzz.jpg" };
    let _ = terrio::memory::open_raster_bytes(data.to_vec(), name);
});

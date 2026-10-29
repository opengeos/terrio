//! GeoTIFF and COG, read as a tiler reads them: headers, a window, the mask and an overview in
//! place, and the whole image when it is small.
#![no_main]

use libfuzzer_sys::fuzz_target;

/// The most samples read whole: a header can declare any size, and reading a raster whole is
/// the caller's choice (the allocation tests cover sizes no allocator can satisfy).
const MAX_SAMPLES: usize = 1 << 22;

fuzz_target!(|data: &[u8]| {
    let Ok(Some(mut r)) = terrio::LazyRaster::from_tiff_bytes(data.to_vec()) else { return };
    let (w, h, n) = (r.width(), r.height(), r.band_count());
    let window = terrio::Window::new(0, 0, w.min(256), h.min(256));
    let _ = r.read_window(window, &terrio::WindowOptions::default());
    let _ = r.read_mask(window, None);
    if let Some(&(ow, oh)) = r.overviews().first() {
        let options = terrio::WindowOptions { bands: Vec::new(), overview: Some(0) };
        let _ = r.read_window(terrio::Window::new(0, 0, ow.min(64), oh.min(64)), &options);
    }
    if w.saturating_mul(h).saturating_mul(n) <= MAX_SAMPLES {
        let _ = terrio::memory::open_raster_bytes(data.to_vec(), "fuzz.tif");
    }
});

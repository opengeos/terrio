//! Header-declared raster formats opened from bytes: ASCII grids, XYZ, Erdas Imagine and HDF5.
//! The first byte picks the driver by extension, so one corpus reaches each reader's header
//! parsing and size checks. The reader bounds header-declared sizes; this target looks for
//! panics and unbounded allocations, not for a particular decoded value.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&choice, body)) = data.split_first() else { return };
    let name = match choice % 4 {
        0 => "fuzz.asc",
        1 => "fuzz.xyz",
        2 => "fuzz.img",
        _ => "fuzz.h5",
    };
    let _ = terrio::memory::open_raster_bytes(body.to_vec(), name);
});

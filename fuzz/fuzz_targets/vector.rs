//! GeoJSON, a shapefile's `.shp` alone, and zip archives (a shapefile with its sidecars, or any
//! dataset), as the vector reader opens uploads. The first byte picks the format.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&kind, rest)) = data.split_first() else { return };
    let name = match kind % 3 {
        0 => "fuzz.geojson",
        1 => "fuzz.shp",
        _ => "fuzz.zip",
    };
    let _ = terrio::memory::open_bytes(rest.to_vec(), name);
});

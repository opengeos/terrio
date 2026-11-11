//! OSM PBF uploads, through the same reader as native and WebAssembly byte opens.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = terrio::memory::open_vector_bytes(data.to_vec(), "fuzz.osm.pbf", &Default::default());
});

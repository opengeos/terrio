//! GeoPackages validated from bytes with the default options: SQLite records, indexes and foreign
//! keys, core and extension metadata, geometry blobs, tile payloads and supported read-only views.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = terrio::validate_gpkg_bytes(data, &Default::default());
});

//! HEC-RAS geometry and plan HDF files opened from bytes: their root attributes, their CRS and
//! every layer they list, read with the default options.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(hdf) = terrio::hecras::HecRasHdf::from_bytes(data.to_vec(), "fuzz.p01.hdf") else { return };
    let _ = hdf.file_attributes();
    let _ = hdf.crs();
    for name in hdf.layer_names().unwrap_or_default() {
        let _ = hdf.read_layer(&name);
    }
});

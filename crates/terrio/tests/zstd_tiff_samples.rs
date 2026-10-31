//! ZSTD GeoTIFFs of Float16, WhiteIsZero (8-bit) and complex samples, as GDAL 3.12.1 writes them
//! (`tests/data/zstd_samples`: strips and tiles, without and with predictors, little- and
//! big-endian), read as GDAL reads them (rasterio 1.5.0, `expected.json`): Float16 as Float32 of
//! the same values, bit for bit, and WhiteIsZero samples as stored. terrio decodes every ZSTD
//! chunk itself, the `tiff` crate being built without its Zstandard.
use std::path::{Path, PathBuf};
use terrio::{DataType, RasterDataset, Window, WindowOptions};
use wbraster::raster::RasterData;

/// The fixtures' size.
const W: usize = 18;
const H: usize = 17;

fn path(name: &str) -> String {
    let dir: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/zstd_samples");
    dir.join(format!("{name}.tif")).to_str().unwrap().to_string()
}

/// The bytes of each band GDAL reads from the fixture `name`.
fn gdal(name: &str) -> Vec<Vec<u8>> {
    let text = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/zstd_samples/expected.json")).unwrap();
    let expected: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(expected["producer"], "rasterio 1.5.0, GDAL 3.12.1");
    let file = &expected["files"][name];
    assert_eq!(file["compression"], "ZSTD", "{name}");
    let hex = |s: &serde_json::Value| {
        let s = s.as_str().unwrap();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    };
    file["bands"].as_array().unwrap().iter().map(hex).collect()
}

/// The fixture `name` read whole (eagerly), lazily whole, and lazily by a window crossing its
/// strips or tiles (columns 3 to 16, rows 2 to 14): each with the window it covers.
fn reads(name: &str) -> Vec<(RasterDataset, Window)> {
    let whole = terrio::open_raster(&path(name)).unwrap_or_else(|e| panic!("{name}: {e}"));
    let bands: Vec<usize> = (1..=whole.raster.bands).collect();
    let mut lazy = terrio::open_raster_lazy(&path(name)).unwrap();
    let mut out = vec![(whole, Window::full(W, H))];
    for window in [Window::full(W, H), Window::new(3, 2, 14, 13)] {
        let options = WindowOptions { bands: bands.clone(), overview: None };
        out.push((lazy.read_window(window, &options).unwrap_or_else(|e| panic!("{name}: {e}")), window));
    }
    out
}

/// The samples of `bands` (each `W` x `H`, of `size` bytes) in `window`, band after band.
fn crop(bands: &[Vec<u8>], size: usize, window: Window) -> Vec<u8> {
    let mut out = Vec::new();
    for band in bands {
        for row in window.y..window.y + window.height {
            out.extend_from_slice(&band[(row * W + window.x) * size..(row * W + window.x + window.width) * size]);
        }
    }
    out
}

/// Float16 samples (`SampleFormat` 3, 16 bits): a ramp, zeros of both signs, subnormals, the
/// largest and smallest values, infinities and quiet NaNs (one with a payload). Read as Float32 of
/// GDAL's Float16 values, bit for bit.
#[test]
fn zstd_float16_samples_read_as_gdal_reads_them() {
    for name in [
        "float16_strips",
        "float16_strips_predictor2",
        "float16_tiles_predictor3",
        "float16_be_strips_predictor2",
        "float16_be_tiles_predictor3",
        "float16_2band_pixel_tiles_predictor2",
    ] {
        let gdal = gdal(name);
        for (ds, window) in reads(name) {
            assert_eq!((ds.raster.data_type, ds.raster.bands), (DataType::F32, gdal.len()), "{name}");
            let RasterData::F32(got) = &ds.raster.data else { panic!("{name}: not Float32") };
            let want: Vec<u32> = crop(&gdal, 2, window)
                .chunks(2)
                .map(|h| half::f16::from_bits(u16::from_le_bytes([h[0], h[1]])).to_f32().to_bits())
                .collect();
            let got: Vec<u32> = got.iter().map(|v| v.to_bits()).collect();
            assert_eq!(got, want, "{name} {window:?}");
        }
    }
}

/// WhiteIsZero bytes (`PhotometricInterpretation` 0): as stored, as libtiff gives them to GDAL.
#[test]
fn zstd_white_is_zero_samples_read_as_stored() {
    for name in ["miniswhite_strips", "miniswhite_tiles_predictor2"] {
        let gdal = gdal(name);
        for (ds, window) in reads(name) {
            let RasterData::U8(got) = &ds.raster.data else { panic!("{name}: not Byte") };
            assert_eq!(got, &crop(&gdal, 1, window), "{name} {window:?}");
        }
    }
}

/// CInt16 and CFloat32 samples under ZSTD, read as GDAL reads them: each complex band as two
/// bands, its real parts then its imaginary parts. With the horizontal predictor, which libtiff
/// applies to a complex sample as one word, refused as under the other compressions.
#[test]
fn zstd_complex_samples_read_as_gdal_reads_them() {
    for (name, type_name, part) in [("cint16_strips", "CInt16", 2), ("cfloat32_tiles", "CFloat32", 4)] {
        let gdal = gdal(name);
        for (ds, window) in reads(name) {
            assert!(ds.complex, "{name}");
            assert_eq!((ds.data_type_name(), ds.raster.bands), (type_name.to_string(), 2), "{name}");
            // GDAL's samples are real and imaginary parts in turn; terrio's bands hold each.
            let samples = crop(&gdal, 2 * part, window);
            let parts: Vec<Vec<u8>> =
                (0..2).map(|p| samples.chunks(2 * part).flat_map(|s| s[p * part..(p + 1) * part].to_vec()).collect()).collect();
            let got: Vec<u8> = match &ds.raster.data {
                RasterData::I16(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
                RasterData::F32(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
                _ => panic!("{name}: not Int16 or Float32 parts"),
            };
            assert_eq!(got, parts.concat(), "{name} {window:?}");
        }
    }
    // Not with the GDAL backend: with it, what terrio refuses goes on to GDAL (gdal_fallback),
    // which reads them.
    #[cfg(not(feature = "gdal"))]
    for name in ["cint16_strips_predictor2", "cfloat32_tiles_predictor2"] {
        let e = terrio::open_raster(&path(name)).unwrap_err().to_string();
        assert!(e.contains("predictor 2"), "{name}: {e}");
    }
}

//! WhiteIsZero GeoTIFFs (`PhotometricInterpretation` 0) as GDAL 3.12.1 writes them
//! (`PHOTOMETRIC=MINISWHITE`, `tests/data/white_is_zero`): Byte, UInt16, Float32 and bilevel
//! samples, uncompressed, LZW, Deflate, PackBits and ZSTD, without and with the predictors GDAL
//! writes with them, in strips and tiles; and big-endian, two-sample, Int16 and Float16 ones. Read
//! as GDAL reads them (rasterio 1.5.0, `expected.json`): the samples as stored, bit for bit, by
//! the eager reader and the lazy one, whole and by a window. The `tiff` crate inverts such samples
//! (or refuses them): terrio decodes them itself, under every compression.
use std::path::{Path, PathBuf};
use terrio::{DataType, RasterDataset, Window, WindowOptions};
use tiff::tags::Tag;
use wbraster::raster::RasterData;

/// The fixtures' size.
const W: usize = 18;
const H: usize = 17;

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/white_is_zero")
}

fn hex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// The fixture at `path` read whole (eagerly), lazily whole, and lazily by a window crossing its
/// strips or tiles (columns 3 to 16, rows 2 to 14): each with the window it covers.
fn reads(path: &str) -> terrio::Result<Vec<(RasterDataset, Window)>> {
    let whole = terrio::open_raster(path)?;
    let bands: Vec<usize> = (1..=whole.raster.bands).collect();
    let mut lazy = terrio::open_raster_lazy(path)?;
    let mut out = vec![(whole, Window::full(W, H))];
    for window in [Window::full(W, H), Window::new(3, 2, 14, 13)] {
        let options = WindowOptions { bands: bands.clone(), overview: None };
        out.push((lazy.read_window(window, &options)?, window));
    }
    Ok(out)
}

/// The samples of `bands` bands (each `W` x `H`, of `size` bytes, band after band) in `window`.
fn crop(samples: &[u8], bands: usize, size: usize, window: Window) -> Vec<u8> {
    let mut out = Vec::new();
    for band in samples.chunks(W * H * size).take(bands) {
        for row in window.y..window.y + window.height {
            out.extend_from_slice(&band[(row * W + window.x) * size..(row * W + window.x + window.width) * size]);
        }
    }
    out
}

/// A read's samples, little-endian.
fn bytes(data: &RasterData) -> Vec<u8> {
    match data {
        RasterData::U8(v) => v.clone(),
        RasterData::U16(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        RasterData::I16(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        RasterData::F32(v) => v.iter().flat_map(|x| x.to_le_bytes()).collect(),
        other => panic!("unexpected samples {:?}", std::mem::discriminant(other)),
    }
}

/// The tags that make the fixture what its entry says: WhiteIsZero, its compression, predictor,
/// layout and byte order.
fn tags(path: &str) -> (u16, u16, u16, bool, bool) {
    let file = std::fs::File::open(path).unwrap();
    let mut dec = tiff::decoder::Decoder::new(file).unwrap();
    let mut short = |tag| dec.find_tag_unsigned::<u16>(tag).unwrap();
    let (photometric, compression, predictor) = (short(Tag::PhotometricInterpretation), short(Tag::Compression), short(Tag::Predictor));
    let tiled = dec.find_tag(Tag::TileWidth).unwrap().is_some();
    let big_endian = dec.byte_order() == tiff::tags::ByteOrder::BigEndian;
    (photometric.unwrap(), compression.unwrap(), predictor.unwrap_or(1), tiled, big_endian)
}

/// Every fixture, by every reader: the samples GDAL reads, bit for bit. Float16 samples are read
/// as Float32 of the same values (as under ZSTD, `zstd_tiff_samples`).
#[test]
fn white_is_zero_samples_read_as_stored_under_every_compression() {
    let text = std::fs::read_to_string(dir().join("expected.json")).unwrap();
    let expected: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(expected["producer"], "rasterio 1.5.0, GDAL 3.12.1");
    let files = expected["files"].as_object().unwrap();
    assert_eq!(files.len(), 84);
    let mut wrong = Vec::new();
    for (name, file) in files {
        let path = dir().join(format!("{name}.tif")).to_str().unwrap().to_string();
        let as_u16 = |key: &str| file[key].as_u64().unwrap() as u16;
        let entry = (0, as_u16("compression"), as_u16("predictor"), file["tiled"] == true, file["big_endian"] == true);
        assert_eq!(tags(&path), entry, "{name}");
        let kind = &expected["kinds"][file["kind"].as_str().unwrap()];
        let bands = kind["bands"].as_u64().unwrap() as usize;
        let gdal = hex(kind["values"].as_str().unwrap());
        let (data_type, size) = match kind["dtype"].as_str().unwrap() {
            "uint8" => (DataType::U8, 1),
            "uint16" => (DataType::U16, 2),
            "int16" => (DataType::I16, 2),
            "float16" => (DataType::F32, 2),
            "float32" => (DataType::F32, 4),
            other => panic!("{name}: {other}"),
        };
        let reads = match reads(&path) {
            Ok(reads) => reads,
            Err(e) => {
                wrong.push(format!("{name}: {e}"));
                continue;
            }
        };
        for (ds, window) in reads {
            assert_eq!((ds.raster.data_type, ds.raster.bands), (data_type, bands), "{name}");
            let mut want = crop(&gdal, bands, size, window);
            if kind["dtype"] == "float16" {
                want = want.chunks(2).flat_map(|h| half::f16::from_bits(u16::from_le_bytes([h[0], h[1]])).to_f32().to_le_bytes()).collect();
            }
            if bytes(&ds.raster.data) != want {
                wrong.push(format!("{name} {window:?}"));
            }
        }
    }
    assert!(wrong.is_empty(), "{} reads differ from GDAL's: {wrong:#?}", wrong.len());
}

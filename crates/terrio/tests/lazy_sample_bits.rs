//! The bits a GeoTIFF stores per sample, beside the type the lazy reader reads them as.

use terrio::{DataType, LazyRaster, Window, WindowOptions};

/// A little-endian TIFF of `w` x `h` one-band samples of `bits` bits and `format`
/// (`SampleFormat`), in one uncompressed strip of `data`.
fn tiff(w: u32, h: u32, bits: u16, format: u16, data: &[u8]) -> Vec<u8> {
    // (tag, type, value): SHORT 3, LONG 4.
    let entries: [(u16, u16, u32); 10] = [
        (256, 4, w),
        (257, 4, h),
        (258, 3, u32::from(bits)),
        (259, 3, 1),
        (262, 3, 1),
        (273, 4, 0), // StripOffsets, set below
        (277, 3, 1),
        (278, 4, h),
        (279, 4, data.len() as u32),
        (339, 3, u32::from(format)),
    ];
    let data_offset = 8 + 2 + entries.len() * 12 + 4;
    let mut out = b"II*\0".to_vec();
    out.extend_from_slice(&8u32.to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    for (tag, typ, value) in entries {
        let value = if tag == 273 { data_offset as u32 } else { value };
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&typ.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&if typ == 3 { u32::from(value as u16) } else { value }.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(data);
    out
}

fn read(bytes: Vec<u8>) -> (LazyRaster, Vec<f64>) {
    let mut r = LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap();
    let all = Window::full(r.width(), r.height());
    let values = r.read_window(all, &WindowOptions { bands: vec![1], overview: None }).unwrap().raster.data.to_f64_vec();
    (r, values)
}

#[test]
fn float16_is_told_apart_from_float32() {
    // Float16 samples are read as Float32, as GDAL before 3.11 read them; `sample_bits` tells
    // them apart (GDAL 3.11 and later: Float16).
    let values = [1.5f32, -2.0, 0.25, 65504.0];
    let half: Vec<u8> = values.iter().flat_map(|v| half::f16::from_f32(*v).to_le_bytes()).collect();
    let (r, got) = read(tiff(2, 2, 16, 3, &half));
    assert_eq!((r.data_type(), r.sample_bits()), (DataType::F32, Some(16)));
    assert_eq!(got, values.map(f64::from));

    let single: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
    let (r, got) = read(tiff(2, 2, 32, 3, &single));
    assert_eq!((r.data_type(), r.sample_bits()), (DataType::F32, Some(32)));
    assert_eq!(got, values.map(f64::from));
}

#[test]
fn packed_samples_report_their_bits() {
    // 12-bit unsigned samples (GDAL's NBITS=12), read as UInt16: 0xABC, 0x123, MSB first.
    let (r, got) = read(tiff(2, 1, 12, 1, &[0xAB, 0xC1, 0x23]));
    assert_eq!((r.data_type(), r.sample_bits()), (DataType::U16, Some(12)));
    assert_eq!(got, [2748.0, 291.0]);
    // Eager TIFF metadata retains a homogeneous packed sample width in memory.
    let ds = terrio::memory::open_raster_bytes(tiff(2, 1, 12, 1, &[0xAB, 0xC1, 0x23]), "a.tif").unwrap();
    assert_eq!(LazyRaster::from(ds).sample_bits(), Some(12));
}

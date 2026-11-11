// Written to match the behavior of:
//   GDAL v3.12.1:
//     frmts/webp/webpdataset.cpp
// No code is translated from these sources.

//! WebP rasters, read and written as GDAL's WEBP driver reads and writes them (`WEBPDataset` in
//! `frmts/webp/webpdataset.cpp`, GDAL 3.13.3), with lossless output by default.
//!
//! Writing takes 3 (RGB) or 4 (RGBA, the fourth band as alpha) Byte bands of at most 16383 x 16383
//! pixels. Without lossy options, `image-webp` writes exact samples including transparent RGB.
//! `LOSSLESS=NO`, or `QUALITY` without `LOSSLESS=YES`, selects the workspace's pure Rust VP8
//! encoder. QUALITY ranges from 0 to 100 (default 75); alpha is stored exactly in an ALPH chunk.
//! Its rate/distortion tradeoff differs from GDAL's libwebp: equal quality settings do not
//! imply equal bytes, size or PSNR.
//! `LOSSLESS` accepts YES/NO, TRUE/FALSE, ON/OFF and 1/0 (case-insensitive); other values
//! are argument errors, like GeoTIFF's WEBP_LOSSLESS. This is stricter than GDAL's boolean parser.
//!
//! - `EXACT` is checked as GDAL checks it (0 or 1) and has no effect: terrio always keeps the RGB
//!   values under fully transparent pixels in lossless output, as GDAL does with `EXACT=1`. With GDAL's default,
//!   `EXACT=0`, libwebp replaces them with values of its own choosing (0 in some files, the
//!   neighbours' prediction in others), so only the visible pixels and the alpha band are GDAL's
//!   `LOSSLESS=YES` pixels there.
//! - An alpha band that is opaque everywhere is not written, as libwebp leaves it out: the file
//!   reads back as 3 bands, in GDAL as in terrio.
//! - The other creation options (`PRESET`, `METHOD`, `TARGETSIZE`...) tune the lossy encoder and
//!   are ignored.
//!
//! The georeferencing goes to a world file (`.wld`) and the CRS to a `.aux.xml` sidecar, as terrio
//! writes PNG and JPEG files. Reading gives 3 bands, or 4 when the image has an alpha channel
//! (Red, Green, Blue, Alpha), lossless and lossy files alike, with GDAL's
//! `COMPRESSION_REVERSIBILITY` and a world file (`.wld`, `.wpw` or `.webpw`) for the georeferencing.

use super::common;
use crate::dataset::fallible;
use crate::formats::RasterDriver;
use crate::{BandMetadata, Error, RasterDataset, Result};
use std::path::Path;
use wbraster::Raster;
use wbraster::raster::RasterData;

/// The largest width and height GDAL's WEBP driver writes.
const MAX_SIZE: usize = 16383;

fn webp_err(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("{}: {e}", path.display()))
}

/// The first value of a creation option (case-insensitive key), as GDAL's `CSLFetchNameValue`.
fn option<'a>(options: &'a [(String, String)], key: &str) -> Option<&'a str> {
    options.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
}

/// Encodes a raster as a lossless or lossy WebP file, as GDAL's `WEBPDataset::CreateCopy` checks and
/// imports it (see the module documentation for the options).
pub(crate) fn encode(r: &Raster, options: &[(String, String)]) -> Result<Vec<u8>> {
    if !matches!(r.bands, 3 | 4) {
        return Err(Error::Unsupported(format!("WEBP driver doesn't support {} bands. Must be 3 (RGB) or 4 (RGBA) bands.", r.bands)));
    }
    let RasterData::U8(v) = &r.data else {
        // GDAL 3.13 renamed the type in this message (`GDT_Byte` to `GDT_UInt8` in its source).
        let only = match crate::ops::warp::KernelVersion::configured() {
            crate::ops::warp::KernelVersion::Gdal312 => "Only eight bit byte bands supported.",
            crate::ops::warp::KernelVersion::Gdal313 => "Only UInt8 bands supported.",
        };
        return Err(Error::Unsupported(format!(
            "WEBP driver doesn't support data type {}. {only}",
            crate::info::gdal_type_name(r.data_type)
        )));
    };
    if r.cols > MAX_SIZE || r.rows > MAX_SIZE {
        return Err(Error::Unsupported("WEBP maximum image dimensions are 16383 x 16383.".into()));
    }
    let mut quality = 75.0;
    if let Some(q) = option(options, "QUALITY") {
        // `CPLAtof`; NaN passes GDAL's check too.
        quality = crate::ops::dem::atof(q);
        #[allow(clippy::manual_range_contains, reason = "`!(0.0..=100.0).contains` would refuse a NaN QUALITY, which GDAL accepts")]
        if quality < 0.0 || quality > 100.0 {
            return Err(Error::InvalidArgument(format!("QUALITY={q} is not a legal value.")));
        }
    }
    if let Some(e) = option(options, "EXACT")
        && !matches!(crate::ops::dem::atoi(e), 0 | 1)
    {
        return Err(Error::InvalidArgument(format!("EXACT={e} is not a legal value.")));
    }
    let lossless = match option(options, "LOSSLESS") {
        Some(value) if ["YES", "TRUE", "ON", "1"].iter().any(|v| value.eq_ignore_ascii_case(v)) => true,
        Some(value) if ["NO", "FALSE", "OFF", "0"].iter().any(|v| value.eq_ignore_ascii_case(v)) => false,
        Some(value) => return Err(Error::InvalidArgument(format!("LOSSLESS={value} is not a legal value."))),
        None => option(options, "QUALITY").is_none(),
    };
    let n = r.cols * r.rows;
    let alpha = (r.bands == 4).then(|| &v[3 * n..4 * n]);
    // libwebp writes an alpha channel only when a pixel is not opaque (`WebPPictureHasTransparency`).
    let alpha = alpha.filter(|a| a.iter().any(|&x| x != 255));
    let channels = if alpha.is_some() || !lossless { 4 } else { 3 };
    let mut pixels = fallible::filled(fallible::len(&[n, channels], "the WEBP image")?, 0u8, "the WEBP image")?;
    for (i, px) in pixels.chunks_exact_mut(channels).enumerate() {
        px[..3].copy_from_slice(&[v[i], v[n + i], v[2 * n + i]]);
        if channels == 4 {
            px[3] = alpha.map_or(255, |a| a[i]);
        }
    }
    if !lossless {
        // Preserve the historical acceptance of NaN QUALITY with the default quality.
        return encode_lossy(pixels, r.cols, r.rows, if quality.is_nan() { 75.0 } else { quality as f32 });
    }
    let color = if channels == 4 { image_webp::ColorType::Rgba8 } else { image_webp::ColorType::Rgb8 };
    let mut out = Vec::new();
    image_webp::WebPEncoder::new(&mut out)
        .encode(&pixels, r.cols as u32, r.rows as u32, color)
        .map_err(|e| Error::InvalidData(format!("WebP: {e}")))?;
    Ok(out)
}

/// Writes a WebP file, its world file and its CRS sidecar (with the nodata value, which
/// GDAL keeps in its `.aux.xml` too).
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path, options: &[(String, String)]) -> Result<()> {
    let bytes = encode(r, options)?;
    let georeferenced = common::georeferenced(options);
    common::write_replacing(path, bytes)?;
    super::png::remove_stale_world_file(r, path, georeferenced)?;
    super::png::write_world_file(r, path, georeferenced)?;
    common::write_pam_with(path, Some(&r.crs), nodata, r.bands)
}

/// Encodes opaque RGB with VP8 and keeps any transparency in a separate ALPH chunk.
/// Shared by standalone WebP files and GeoTIFF WebP tiles; GDAL 3.13.3 can read both.
pub(crate) fn encode_lossy(mut rgba: Vec<u8>, width: usize, height: usize, quality: f32) -> Result<Vec<u8>> {
    if width == 0 || height == 0 || width > MAX_SIZE || height > MAX_SIZE {
        return Err(Error::InvalidArgument("WebP dimensions must be between 1 and 16383".into()));
    }
    if rgba.len() != width * height * 4 {
        return Err(Error::InvalidData("WebP RGBA buffer length differs from image dimensions".into()));
    }
    let alpha = rgba.as_chunks::<4>().0.iter().any(|p| p[3] != 255).then(|| {
        rgba.as_chunks_mut::<4>()
            .0
            .iter_mut()
            .map(|p| {
                let a = p[3];
                p[3] = 255;
                a
            })
            .collect::<Vec<_>>()
    });
    let image = webp_rust::ImageBuffer { width, height, rgba };
    let encoded =
        webp_rust::encode_lossy_with_config(&image, &webp_rust::LossyEncodingConfig { quality, method: 0, ..Default::default() }, None)
            .map_err(|error| Error::InvalidData(format!("WebP: {error}")))?;

    let Some(alpha) = alpha else { return Ok(encoded) };
    if encoded.get(..4) != Some(b"RIFF") || encoded.get(8..16) != Some(b"WEBPVP8 ") {
        return Err(Error::InvalidData("WebP encoder did not return a simple VP8 container".into()));
    }
    let mut body = b"WEBP".to_vec();
    let mut append = |tag: &[u8; 4], payload: &[u8]| {
        body.extend_from_slice(tag);
        body.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        body.extend_from_slice(payload);
        if !payload.len().is_multiple_of(2) {
            body.push(0);
        }
    };
    let mut extended = vec![0x10, 0, 0, 0];
    extended.extend_from_slice(&((width - 1) as u32).to_le_bytes()[..3]);
    extended.extend_from_slice(&((height - 1) as u32).to_le_bytes()[..3]);
    append(b"VP8X", &extended);
    let mut alpha_chunk = vec![0]; // uncompressed, unfiltered alpha
    alpha_chunk.extend_from_slice(&alpha);
    append(b"ALPH", &alpha_chunk);
    body.extend_from_slice(&encoded[12..]);
    let mut output = b"RIFF".to_vec();
    output.extend_from_slice(&(body.len() as u32).to_le_bytes());
    output.extend_from_slice(&body);
    Ok(output)
}

/// Reads a WebP file, lossless or lossy, as GDAL's WEBP driver reads it.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let bytes = std::fs::read(path)?;
    let mut decoder = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes.as_slice())).map_err(|e| webp_err(path, e))?;
    let (w, h) = decoder.dimensions();
    let (cols, rows) = (w as usize, h as usize);
    let bands = if decoder.has_alpha() { 4 } else { 3 };
    let lossy = decoder.is_lossy();
    // The sizes come from the header: allocated fallibly.
    let size = decoder.output_buffer_size().ok_or_else(|| webp_err(path, "image too large"))?;
    let mut buf = fallible::filled(size, 0u8, "the WEBP image")?;
    decoder.read_image(&mut buf).map_err(|e| webp_err(path, e))?;
    let n = cols * rows;
    let mut data = fallible::filled(fallible::len(&[n, bands], "the WEBP raster")?, 0u8, "the WEBP raster")?;
    for (i, px) in buf.chunks_exact(bands).enumerate() {
        for (b, value) in px.iter().enumerate() {
            data[b * n + i] = *value;
        }
    }
    let gt = super::png::read_world_file(path);
    let raster =
        common::make_raster(cols, rows, bands, RasterData::U8(data), gt.unwrap_or(common::NO_GEOTRANSFORM), Default::default(), None)?;
    let mut ds = RasterDataset::with_driver(raster, RasterDriver::WEBP, false);
    ds.geotransform_present = Some(gt.is_some());
    // `WEBPRasterBand::GetColorInterpretation`.
    ds.band_metadata = ["Red", "Green", "Blue", "Alpha"][..bands]
        .iter()
        .map(|c| BandMetadata { color_interp: Some(c.to_string()), ..Default::default() })
        .collect();
    let reversibility = if lossy { "LOSSY" } else { "LOSSLESS" };
    ds.metadata_domains = vec![(None, "IMAGE_STRUCTURE".into(), vec![("COMPRESSION_REVERSIBILITY".into(), reversibility.into())])];
    Ok(ds)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A raster of `bands` Byte bands, `cols` x `rows`, from band-major values.
    fn raster(cols: usize, rows: usize, bands: usize, v: Vec<u8>) -> Raster {
        common::make_raster(cols, rows, bands, RasterData::U8(v), common::NO_GEOTRANSFORM, Default::default(), None).unwrap()
    }

    /// Decodes WebP bytes to interleaved samples and whether they have alpha.
    fn decode(bytes: &[u8]) -> (Vec<u8>, bool) {
        let mut d = image_webp::WebPDecoder::new(std::io::Cursor::new(bytes)).unwrap();
        let mut buf = vec![0u8; d.output_buffer_size().unwrap()];
        d.read_image(&mut buf).unwrap();
        (buf, d.has_alpha())
    }

    fn co(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn lossy_quality_changes_pixels_but_keeps_alpha() {
        let n = 32 * 32;
        let values: Vec<u8> = (0..4 * n).map(|i| ((i * 31 + i / 32) % 256) as u8).collect();
        let r = raster(32, 32, 4, values.clone());
        let low = encode(&r, &co(&[("quality", "10")])).unwrap();
        let high = encode(&r, &co(&[("QUALITY", "90"), ("LOSSLESS", "NO")])).unwrap();
        assert_ne!(low, high);
        for bytes in [low, high] {
            assert!(bytes.windows(4).any(|w| w == b"VP8 "));
            let (pixels, alpha) = decode(&bytes);
            assert!(alpha);
            assert_eq!(pixels.as_chunks::<4>().0.iter().map(|p| p[3]).collect::<Vec<_>>(), values[3 * n..]);
            assert!(pixels.as_chunks::<4>().0.iter().enumerate().any(|(i, p)| p[..3] != [values[i], values[n + i], values[2 * n + i]]));
        }
        assert!(encode(&r, &co(&[("LOSSLESS", "maybe")])).unwrap_err().to_string().contains("LOSSLESS"));
        assert_eq!(encode(&r, &co(&[("QUALITY", "nan")])).unwrap(), encode(&r, &co(&[("QUALITY", "75")])).unwrap());
        assert!(encode_lossy(Vec::new(), 0, 1, 75.0).is_err());
        assert!(encode_lossy(Vec::new(), 1, 0, 75.0).is_err());
        assert!(encode_lossy(Vec::new(), 1, 1, 75.0).is_err());
    }

    /// A WebP file, not another format's bytes, holding the raster's exact RGB samples: the
    /// default lossless guarantee and the explicit override of QUALITY by LOSSLESS=YES.
    #[test]
    fn rgb_is_written_losslessly_by_default_or_when_requested() {
        let (cols, rows) = (37, 23);
        let v: Vec<u8> = (0..3 * cols * rows).map(|i| ((i * 7919) % 251) as u8).collect();
        let r = raster(cols, rows, 3, v.clone());
        for options in [vec![], co(&[("QUALITY", "5"), ("LOSSLESS", "YES")])] {
            let bytes = encode(&r, &options).unwrap();
            assert_eq!(&bytes[..4], b"RIFF", "{options:?}");
            assert_eq!(&bytes[8..16], b"WEBPVP8L", "{options:?}: a lossless bitstream");
            let (px, alpha) = decode(&bytes);
            assert!(!alpha);
            let n = cols * rows;
            let expected: Vec<u8> = (0..n).flat_map(|i| [v[i], v[n + i], v[2 * n + i]]).collect();
            assert_eq!(px, expected, "{options:?}");
        }
    }

    /// RGBA: every sample exact, the RGB under fully transparent pixels included, whatever `EXACT`
    /// says (GDAL keeps them with `EXACT=1`).
    #[test]
    fn rgba_is_written_exactly_transparent_pixels_included() {
        let (cols, rows) = (16, 16);
        let n = cols * rows;
        let mut v: Vec<u8> = (0..4 * n).map(|i| ((i * 31 + 7) % 256) as u8).collect();
        for i in 0..n {
            v[3 * n + i] = match i % 3 {
                0 => 0,
                1 => 128,
                _ => 255,
            };
        }
        let r = raster(cols, rows, 4, v.clone());
        for options in [vec![], co(&[("EXACT", "0")]), co(&[("exact", "1")])] {
            let (px, alpha) = decode(&encode(&r, &options).unwrap());
            assert!(alpha);
            for i in 0..n {
                assert_eq!(px[4 * i..4 * i + 4], [v[i], v[n + i], v[2 * n + i], v[3 * n + i]], "pixel {i} {options:?}");
            }
        }
    }

    /// An opaque alpha band is left out, as libwebp leaves it out: GDAL reads such a file as 3
    /// bands.
    #[test]
    fn an_opaque_alpha_band_is_not_written() {
        let n = 8 * 8;
        let mut v: Vec<u8> = (0..4 * n).map(|i| (i % 256) as u8).collect();
        v[3 * n..].fill(255);
        let (px, alpha) = decode(&encode(&raster(8, 8, 4, v.clone()), &[]).unwrap());
        assert!(!alpha);
        assert_eq!(px, (0..n).flat_map(|i| [v[i], v[n + i], v[2 * n + i]]).collect::<Vec<u8>>());
    }

    /// GDAL's checks and messages (`WEBPDataset::CreateCopy`).
    #[test]
    fn refuses_what_gdal_refuses() {
        for bands in [1, 2, 5] {
            let err = encode(&raster(4, 4, bands, vec![0; 16 * bands]), &[]).unwrap_err().to_string();
            assert!(err.contains(&format!("WEBP driver doesn't support {bands} bands. Must be 3 (RGB) or 4 (RGBA) bands.")), "{err}");
        }
        let u16 = common::make_raster(4, 4, 3, RasterData::U16(vec![0; 48]), common::NO_GEOTRANSFORM, Default::default(), None).unwrap();
        let err = encode(&u16, &[]).unwrap_err().to_string();
        let only = match crate::ops::warp::KernelVersion::configured() {
            crate::ops::warp::KernelVersion::Gdal312 => "Only eight bit byte bands supported.",
            crate::ops::warp::KernelVersion::Gdal313 => "Only UInt8 bands supported.",
        };
        assert!(err.contains(&format!("WEBP driver doesn't support data type UInt16. {only}")), "{err}");
        let err = encode(&raster(16384, 1, 3, vec![0; 3 * 16384]), &[]).unwrap_err().to_string();
        assert!(err.contains("WEBP maximum image dimensions are 16383 x 16383."), "{err}");
        assert!(encode(&raster(16383, 1, 3, vec![0; 3 * 16383]), &[]).is_ok());
        for (k, v) in [("QUALITY", "101"), ("QUALITY", "-1"), ("QUALITY", "inf"), ("EXACT", "2")] {
            let err = encode(&raster(2, 2, 3, vec![0; 12]), &co(&[(k, v)])).unwrap_err().to_string();
            assert!(err.contains(&format!("{k}={v} is not a legal value.")), "{err}");
        }
        // NaN fails neither of GDAL's two comparisons: GDAL writes the file.
        for q in ["0", "100", "75.5", "nan"] {
            assert!(encode(&raster(2, 2, 3, vec![0; 12]), &co(&[("QUALITY", q)])).is_ok(), "QUALITY={q}");
        }
    }

    /// A file written is read back with its bands, colour interpretations, reversibility and world
    /// file.
    #[test]
    fn reads_back_what_it_writes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.webp");
        let n = 5 * 3;
        let mut v: Vec<u8> = (0..4 * n).map(|i| (i * 13 % 256) as u8).collect();
        v[3 * n..].iter_mut().enumerate().for_each(|(i, a)| *a = if i % 2 == 0 { 255 } else { 77 });
        let gt = [500000.0, 10.0, 0.0, 4100000.0, 0.0, -10.0];
        let r = common::make_raster(5, 3, 4, RasterData::U8(v.clone()), gt, Default::default(), None).unwrap();
        write(&r, None, &p, &[]).unwrap();
        let ds = read(&p).unwrap();
        assert_eq!((ds.raster.cols, ds.raster.rows, ds.raster.bands), (5, 3, 4));
        let RasterData::U8(got) = &ds.raster.data else { panic!("Byte bands") };
        assert_eq!(got, &v);
        let interps: Vec<&str> = ds.band_metadata.iter().filter_map(|m| m.color_interp.as_deref()).collect();
        assert_eq!(interps, ["Red", "Green", "Blue", "Alpha"]);
        assert_eq!(ds.metadata_domains[0].2, vec![("COMPRESSION_REVERSIBILITY".to_string(), "LOSSLESS".to_string())]);
        assert_eq!((ds.raster.x_min, ds.raster.cell_size_x, ds.raster.y_max()), (500000.0, 10.0, 4100000.0));
    }

    /// Through the public write path, the driver chosen by name as `terrio.rio` chooses it (into
    /// a file named `.tif`, as its memory files are): the file is a WebP with the raster's pixels,
    /// and the public open path reads it back by its `.webp` name.
    #[test]
    fn the_write_path_writes_webp_and_reads_it_back() {
        let dir = tempfile::tempdir().unwrap();
        let (cols, rows) = (9, 7);
        let n = cols * rows;
        let v: Vec<u8> = (0..3 * n).map(|i| (i * 29 % 256) as u8).collect();
        let ds = RasterDataset::with_driver(raster(cols, rows, 3, v.clone()), RasterDriver::GTiff, false);
        let options = crate::RasterWriteOptions {
            driver: Some(RasterDriver::WEBP),
            creation_options: co(&[("QUALITY", "75"), ("LOSSLESS", "YES")]),
            ..Default::default()
        };
        let p = dir.path().join("tile.tif");
        crate::write_raster_dataset(&ds, p.to_str().unwrap(), &options).unwrap();
        let bytes = std::fs::read(&p).unwrap();
        assert_eq!((&bytes[..4], &bytes[8..16]), (&b"RIFF"[..], &b"WEBPVP8L"[..]));
        assert_eq!(decode(&bytes).0, (0..n).flat_map(|i| [v[i], v[n + i], v[2 * n + i]]).collect::<Vec<u8>>());
        let q = dir.path().join("tile.webp");
        std::fs::write(&q, &bytes).unwrap();
        let back = crate::open_raster(q.to_str().unwrap()).unwrap();
        assert_eq!(back.driver, RasterDriver::WEBP);
        let RasterData::U8(got) = &back.raster.data else { panic!("Byte bands") };
        assert_eq!(got, &v);
    }
}

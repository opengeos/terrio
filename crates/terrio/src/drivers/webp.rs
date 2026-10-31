//! WebP rasters, read and written as GDAL's WEBP driver reads and writes them (`WEBPDataset` in
//! `frmts/webp/webpdataset.cpp`, GDAL 3.12.1), except that terrio's writer is lossless only.
//!
//! Writing takes 3 (RGB) or 4 (RGBA, the fourth band as alpha) Byte bands of at most 16383 x 16383
//! pixels, and refuses anything else with GDAL's errors. Every file is lossless (a `VP8L`
//! bitstream, encoded by `image-webp` without native code): its pixels are the raster's, the RGB
//! values under fully transparent pixels included.
//!
//! - `LOSSLESS` is accepted with any value. GDAL's default, `LOSSLESS=NO`, writes a lossy (`VP8`)
//!   file; terrio writes a lossless one for it too, whose size is not that of GDAL's file. The
//!   pure-Rust lossy encoder of the workspace (`webp-rust` 0.2.1) was measured for that default and
//!   not adopted: at the same `QUALITY` its files fall more than 1 dB below the PSNR of GDAL's on
//!   some tiles, and a higher `QUALITY` can give it a larger file of lower PSNR.
//! - `QUALITY` is checked as GDAL checks it (0 to 100) and has no effect: it sets the lossy
//!   encoder's quality, and the lossless encoder has no quality.
//! - `EXACT` is checked as GDAL checks it (0 or 1) and has no effect: terrio always keeps the RGB
//!   values under fully transparent pixels, as GDAL does with `EXACT=1`. With GDAL's default,
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

/// Encodes a raster as a lossless WebP file, as GDAL's `WEBPDataset::CreateCopy` checks and
/// imports it (see the module documentation for the options).
pub(crate) fn encode(r: &Raster, options: &[(String, String)]) -> Result<Vec<u8>> {
    if !matches!(r.bands, 3 | 4) {
        return Err(Error::Unsupported(format!("WEBP driver doesn't support {} bands. Must be 3 (RGB) or 4 (RGBA) bands.", r.bands)));
    }
    let RasterData::U8(v) = &r.data else {
        return Err(Error::Unsupported(format!(
            "WEBP driver doesn't support data type {}. Only eight bit byte bands supported.",
            crate::info::gdal_type_name(r.data_type)
        )));
    };
    if r.cols > MAX_SIZE || r.rows > MAX_SIZE {
        return Err(Error::Unsupported("WEBP maximum image dimensions are 16383 x 16383.".into()));
    }
    if let Some(q) = option(options, "QUALITY") {
        // `CPLAtof`; NaN passes GDAL's check too.
        let quality = crate::ops::dem::atof(q);
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
    let n = r.cols * r.rows;
    let alpha = (r.bands == 4).then(|| &v[3 * n..4 * n]);
    // libwebp writes an alpha channel only when a pixel is not opaque (`WebPPictureHasTransparency`).
    let alpha = alpha.filter(|a| a.iter().any(|&x| x != 255));
    let channels = if alpha.is_some() { 4 } else { 3 };
    let mut pixels = fallible::filled(fallible::len(&[n, channels], "the WEBP image")?, 0u8, "the WEBP image")?;
    for (i, px) in pixels.chunks_exact_mut(channels).enumerate() {
        px[..3].copy_from_slice(&[v[i], v[n + i], v[2 * n + i]]);
        if let Some(a) = alpha {
            px[3] = a[i];
        }
    }
    let color = if channels == 4 { image_webp::ColorType::Rgba8 } else { image_webp::ColorType::Rgb8 };
    let mut out = Vec::new();
    image_webp::WebPEncoder::new(&mut out)
        .encode(&pixels, r.cols as u32, r.rows as u32, color)
        .map_err(|e| Error::InvalidData(format!("WebP: {e}")))?;
    Ok(out)
}

/// Writes a lossless WebP file, its world file and its CRS sidecar (with the nodata value, which
/// GDAL keeps in its `.aux.xml` too).
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path, options: &[(String, String)]) -> Result<()> {
    let bytes = encode(r, options)?;
    std::fs::write(path, bytes)?;
    super::png::write_world_file(r, path)?;
    common::write_pam_with(path, Some(&r.crs), nodata, r.bands)
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
    let gt = super::png::read_world_file(path).unwrap_or(common::NO_GEOTRANSFORM);
    let raster = common::make_raster(cols, rows, bands, RasterData::U8(data), gt, Default::default(), None)?;
    let mut ds = RasterDataset::with_driver(raster, RasterDriver::WEBP, false);
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

    /// A WebP file, not another format's bytes, holding the raster's exact RGB samples: the
    /// lossless guarantee, whatever `LOSSLESS` and `QUALITY` say.
    #[test]
    fn rgb_is_written_losslessly_whatever_the_quality() {
        let (cols, rows) = (37, 23);
        let v: Vec<u8> = (0..3 * cols * rows).map(|i| ((i * 7919) % 251) as u8).collect();
        let r = raster(cols, rows, 3, v.clone());
        for options in [vec![], co(&[("QUALITY", "75"), ("LOSSLESS", "NO")]), co(&[("QUALITY", "5"), ("LOSSLESS", "YES")])] {
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
        assert!(err.contains("WEBP driver doesn't support data type UInt16. Only eight bit byte bands supported."), "{err}");
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
            creation_options: co(&[("QUALITY", "75"), ("LOSSLESS", "NO")]),
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

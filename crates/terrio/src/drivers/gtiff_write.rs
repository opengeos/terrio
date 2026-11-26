// SPDX-FileCopyrightText: Copyright (c) 2000, Frank Warmerdam
// SPDX-FileCopyrightText: Copyright (c) 2007-2010, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Copyright (c) 1998, 2002, Frank Warmerdam <warmerdam@pobox.com>
// SPDX-FileCopyrightText: Copyright (c) 2007-2015, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     gcore/overview.cpp
//   GDAL v3.12.1:
//     frmts/gtiff/gtiffdataset_write.cpp

//! A GeoTIFF and Cloud Optimized GeoTIFF writer.
//!
//! wbraster's writer lays COGs out in the wrong order and skips overview levels, so terrio writes
//! TIFFs itself: strips or tiles, pixel-interleaved, with the compressions GDAL reads by default,
//! GeoKeys for EPSG and user-defined CRSs, `GDAL_NODATA` and `GDAL_METADATA`. COGs follow GDAL's
//! layout: the IFDs first, then the image data from the smallest overview to the full resolution,
//! with GDAL's structural metadata, block leaders and trailers.

use super::area_matched::{self, AreaMatched, IntoRasterData};
use super::common::{Rewrite, create_replacing};
use super::gdal_overview::{self, Method};
use crate::pixel::{Pixel, map_data, with_data};
use crate::{Error, Result};
use std::io::Write;
use wbraster::raster::RasterData;
use wbraster::{DataType, Raster};

/// TIFF compressions the writer supports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compression {
    None,
    Lzw,
    Deflate,
    PackBits,
    Zstd,
    /// LERC, alone or with Deflate or Zstandard over its blobs, as GDAL writes it.
    Lerc,
    LercDeflate,
    LercZstd,
    /// Lossless JPEG XL (GDAL's code 50002), for Byte and UInt16 data of 1 to 4 bands.
    Jxl,
    /// Baseline JPEG, for Byte data of 1 band, or 3 bands in YCbCr.
    Jpeg,
    /// Pure Rust lossy/lossless WebP.
    WebP,
}

/// The parameters of the lossy and variable compressions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CodecParams {
    /// LERC: the largest error allowed (0 for lossless).
    pub max_z_error: f64,
    /// JPEG: the quality, 1 to 100.
    pub jpeg_quality: u8,
    /// JPEG: three bands in YCbCr, subsampled; [`Level::sparse_image`] sets it from the layout's
    /// photometric.
    pub jpeg_ycbcr: bool,
    pub webp_quality: u8,
    pub webp_lossless: bool,
    /// Deflate and LERC_DEFLATE: the level (`ZLEVEL`, 1 to 9, default 6).
    pub deflate_level: crate::deflate_compress::Level,
    /// ZSTD and LERC_ZSTD: the level (`ZSTD_LEVEL`, 1 to 22, default 9). Without the `libzstd`
    /// feature, `zrip` writes levels above 4 at level 4.
    pub zstd_level: i32,
}

impl Default for CodecParams {
    fn default() -> Self {
        Self {
            max_z_error: 0.0,
            jpeg_quality: 75,
            jpeg_ycbcr: false,
            webp_quality: 75,
            webp_lossless: false,
            deflate_level: crate::deflate_compress::Level::DEFAULT,
            zstd_level: 9,
        }
    }
}

/// The geometry of a chunk to compress: its size and samples.
struct ChunkInfo {
    w: usize,
    h: usize,
    spp: usize,
    t: DataType,
    /// The width and height of the part of the chunk inside the image: less than `w` x `h` in a
    /// partial tile at the right or bottom edge, whose other pixels are padding.
    #[cfg_attr(not(feature = "lerc"), allow(dead_code))]
    valid: (usize, usize),
}

impl Compression {
    /// Parses a GDAL compression name. `None` for names this writer does not handle.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_uppercase().as_str() {
            "NONE" => Self::None,
            "LZW" => Self::Lzw,
            "DEFLATE" | "ZIP" => Self::Deflate,
            "PACKBITS" => Self::PackBits,
            "ZSTD" => Self::Zstd,
            "LERC" => Self::Lerc,
            "LERC_DEFLATE" => Self::LercDeflate,
            "LERC_ZSTD" => Self::LercZstd,
            "JXL" => Self::Jxl,
            "JPEG" => Self::Jpeg,
            "WEBP" => Self::WebP,
            _ => return None,
        })
    }

    /// The compression of a TIFF `Compression` tag value, with the second `LercParameters`
    /// value for LERC. `None` for compressions this writer does not encode.
    pub(crate) fn from_tiff(code: u16, lerc: Option<u64>) -> Option<Self> {
        Some(match code {
            1 => Self::None,
            5 => Self::Lzw,
            8 | 32946 => Self::Deflate,
            32773 => Self::PackBits,
            50000 => Self::Zstd,
            34887 => match lerc.unwrap_or(0) {
                0 => Self::Lerc,
                1 => Self::LercDeflate,
                2 => Self::LercZstd,
                _ => return None,
            },
            50002 => Self::Jxl,
            7 => Self::Jpeg,
            50001 => Self::WebP,
            _ => return None,
        })
    }

    pub(crate) fn code(self) -> u16 {
        match self {
            Self::None => 1,
            Self::Lzw => 5,
            Self::Deflate => 8,
            Self::PackBits => 32773,
            Self::Zstd => 50000,
            Self::Lerc | Self::LercDeflate | Self::LercZstd => 34887,
            Self::Jxl => 50002,
            Self::Jpeg => 7,
            Self::WebP => 50001,
        }
    }

    /// Whether the compression takes the TIFF predictor.
    pub(crate) fn takes_predictor(self) -> bool {
        matches!(self, Self::Lzw | Self::Deflate | Self::Zstd)
    }

    /// Each chunk remains an independent TIFF stream, with fresh owned output.
    /// Scratch retains dictionary capacity only, never codes from a previous chunk.
    fn compress_reusing(
        self,
        data: Vec<u8>,
        c: &ChunkInfo,
        p: &CodecParams,
        scratch: &mut Option<super::tiff_lzw::Encoder>,
    ) -> Result<Vec<u8>> {
        if !matches!(self, Self::Lzw) {
            return self.compress(data, c, p);
        }
        let encoder = scratch.get_or_insert_with(super::tiff_lzw::Encoder::new);
        encoder.encode(&data)
    }

    fn compress(self, data: Vec<u8>, c: &ChunkInfo, p: &CodecParams) -> Result<Vec<u8>> {
        Ok(match self {
            Self::None => data,
            Self::Lzw => super::tiff_lzw::Encoder::new().encode(&data)?,
            // The chunk's `h` rows, each of the same length: each search also examines the position
            // one row back.
            Self::Deflate => {
                debug_assert_eq!(data.len() % c.h.max(1), 0, "a Deflate chunk of {} bytes is not {} whole rows", data.len(), c.h);
                crate::deflate_compress::zlib_compress_rows(&data, data.len() / c.h.max(1), p.deflate_level)
            }
            Self::PackBits => packbits(&data),
            Self::Zstd => zstd_compress(&data, p.zstd_level)?,
            Self::Lerc => lerc_encode(&data, c, p.max_z_error)?,
            Self::LercDeflate => deflate(&lerc_encode(&data, c, p.max_z_error)?, p.deflate_level)?,
            Self::LercZstd => zstd_compress(&lerc_encode(&data, c, p.max_z_error)?, p.zstd_level)?,
            Self::Jxl => jxl_encode(data, c)?,
            Self::Jpeg => jpeg_encode(&data, c, p.jpeg_quality, p.jpeg_ycbcr)?,
            Self::WebP => webp_encode(data, c, p)?,
        })
    }
}

#[cfg(not(feature = "lerc"))]
fn lerc_encode(_data: &[u8], _c: &ChunkInfo, _max_z_error: f64) -> Result<Vec<u8>> {
    Err(Error::disabled("lerc"))
}

/// The Deflate stage of a `LERC_DEFLATE` chunk: the LERC blob, zlib-wrapped at `level` (`ZLEVEL`,
/// 6 by default) by `zlib_compress`, whose compressor is derived from libdeflate's and also probes
/// periodic-run boundaries at levels 6 to 9. A blob has no rows, so no search looks one row back:
/// `DEFLATE` chunks are compressed in their rows by `zlib_compress_rows` instead.
fn deflate(data: &[u8], level: crate::deflate_compress::Level) -> Result<Vec<u8>> {
    Ok(crate::deflate_compress::zlib_compress(data, level))
}

/// Encodes a chunk (little-endian samples, interleaved) as a LERC blob of `spp` values per pixel,
/// as GDAL's LERC codec does. Floating-point chunks of one sample with NaNs get a mask of the
/// valid pixels. As in GDAL, a floating-point chunk of one sample (one band, or `INTERLEAVE=BAND`)
/// also masks the padding of a partial edge tile, so the padding costs no LERC blocks; integer
/// and pixel-interleaved chunks keep it as zeros.
#[cfg(feature = "lerc")]
fn lerc_encode(data: &[u8], c: &ChunkInfo, max_z_error: f64) -> Result<Vec<u8>> {
    let lerc = |e: lerc_core::Error| Error::InvalidData(format!("LERC: {e}"));
    let options = lerc_writer::EncodeOptions::new().with_max_z_error(max_z_error);
    let (w, h, d) = (c.w as u32, c.h as u32, c.spp as u32);
    macro_rules! encode {
        ($t:ty, $n:expr) => {{
            let v: Vec<$t> = data.chunks_exact($n).map(|b| <$t>::from_le_bytes(b.try_into().unwrap_or_default())).collect();
            let raster = lerc_writer::RasterView::new(w, h, d, &v).map_err(lerc)?;
            lerc_writer::encode(raster, None, options).map_err(lerc)?
        }};
    }
    macro_rules! encode_float {
        ($t:ty, $n:expr) => {{
            let v: Vec<$t> = data.chunks_exact($n).map(|b| <$t>::from_le_bytes(b.try_into().unwrap_or_default())).collect();
            // LERC's mask is per pixel: a pixel is masked when all its samples are NaN. A pixel
            // with some NaN samples only cannot be stored with its other samples.
            if v.chunks_exact(c.spp).any(|px| px.iter().any(|x| x.is_nan()) && !px.iter().all(|x| x.is_nan())) {
                return Err(Error::Unsupported("LERC: NaN in some bands of a pixel only (write with INTERLEAVE=BAND)".into()));
            }
            let (vw, vh) = c.valid;
            let pad = c.spp == 1 && (vw < c.w || vh < c.h);
            let valid: Vec<u8> = v
                .chunks_exact(c.spp)
                .enumerate()
                .map(|(i, px)| u8::from((!pad || (i % c.w < vw && i / c.w < vh)) && !px.iter().all(|x| x.is_nan())))
                .collect();
            let raster = lerc_writer::RasterView::new(w, h, d, &v).map_err(lerc)?;
            if valid.contains(&0) {
                // Masked pixels: their values are not stored.
                let filled: Vec<$t> = v.iter().map(|x| if x.is_nan() { 0.0 } else { *x }).collect();
                let raster = lerc_writer::RasterView::new(w, h, d, &filled).map_err(lerc)?;
                let mask = lerc_writer::MaskView::new(w, h, &valid).map_err(lerc)?;
                lerc_writer::encode(raster, Some(mask), options).map_err(lerc)?
            } else {
                lerc_writer::encode(raster, None, options).map_err(lerc)?
            }
        }};
    }
    Ok(match c.t {
        DataType::U8 => lerc_writer::encode(lerc_writer::RasterView::new(w, h, d, data).map_err(lerc)?, None, options).map_err(lerc)?,
        DataType::I8 => encode!(i8, 1),
        DataType::U16 => encode!(u16, 2),
        DataType::I16 => encode!(i16, 2),
        DataType::U32 => encode!(u32, 4),
        DataType::I32 => encode!(i32, 4),
        DataType::F32 => encode_float!(f32, 4),
        DataType::F64 => encode_float!(f64, 8),
        t => return Err(Error::Unsupported(format!("LERC does not encode {t:?} data"))),
    })
}

/// The rows of each strip: those asked for, or about 64 KB of rows. JPEG strips hold whole MCUs
/// (16 rows with 4:2:0 subsampling), the last one aside.
fn strip_rows(asked: Option<u32>, rows: usize, row_bytes: usize, compression: Compression) -> usize {
    let per = match asked {
        Some(r) => (r as usize).clamp(1, rows),
        None => ((1usize << 16) / row_bytes.max(1)).clamp(1, rows),
    };
    if compression == Compression::Jpeg && per < rows { (per / 16 * 16).max(16).min(rows) } else { per }
}

fn webp_encode(data: Vec<u8>, chunk: &ChunkInfo, options: &CodecParams) -> Result<Vec<u8>> {
    let maximum = if options.webp_lossless { 16384 } else { 16383 };
    if chunk.w == 0 || chunk.h == 0 || chunk.w > maximum || chunk.h > maximum {
        return Err(Error::InvalidArgument(format!("WebP chunk dimensions must be between 1 and {maximum}")));
    }
    if chunk.t != DataType::U8 || !matches!(chunk.spp, 3 | 4) {
        return Err(Error::Unsupported("WebP requires three or four interleaved Byte bands".into()));
    }
    // Dimensions and sample count above bound this product to at most 1 GiB.
    if data.len() != chunk.w * chunk.h * chunk.spp {
        return Err(Error::InvalidData("WebP sample buffer length differs from chunk dimensions".into()));
    }
    let rgba = if chunk.spp == 4 { data } else { data.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect() };
    if !options.webp_lossless {
        return super::webp::encode_lossy(rgba, chunk.w, chunk.h, options.webp_quality as f32);
    }
    let image = webp_rust::ImageBuffer { width: chunk.w, height: chunk.h, rgba };
    webp_rust::encode_lossless_with_config(&image, &webp_rust::LosslessEncodingConfig { z_level: 2 }, None)
        .map_err(|error| Error::InvalidData(format!("WebP: {error}")))
}

/// Encodes a chunk as a lossless JPEG XL codestream: 1 to 4 channels (gray, gray and alpha, RGB,
/// RGBA) of 8 or 16 bits.
fn jxl_encode(mut data: Vec<u8>, c: &ChunkInfo) -> Result<Vec<u8>> {
    use zune_core::bit_depth::BitDepth;
    use zune_core::colorspace::ColorSpace;
    let depth = match c.t {
        DataType::U8 => BitDepth::Eight,
        DataType::U16 => {
            // The encoder reads 16-bit samples in native byte order.
            if cfg!(target_endian = "big") {
                data.as_chunks_mut::<2>().0.iter_mut().for_each(|b| b.swap(0, 1));
            }
            BitDepth::Sixteen
        }
        t => return Err(Error::Unsupported(format!("JPEG XL is written for Byte and UInt16 data, not {t:?}"))),
    };
    let colorspace = match c.spp {
        1 => ColorSpace::Luma,
        2 => ColorSpace::LumaA,
        3 => ColorSpace::RGB,
        4 => ColorSpace::RGBA,
        n => return Err(Error::Unsupported(format!("JPEG XL is written for 1 to 4 bands per chunk, not {n}"))),
    };
    let options = zune_core::options::EncoderOptions::new(c.w, c.h, colorspace, depth).set_num_threads(1);
    let mut out = Vec::new();
    super::jxl_encoder::JxlSimpleEncoder::new(&data, options).encode(&mut out).map_err(|e| Error::InvalidData(format!("JPEG XL: {e}")))?;
    Ok(out)
}

/// Encodes a chunk of Byte samples as a baseline JPEG: gray; RGB components as they are, all
/// with the luminance quantization table, as libjpeg encodes `JCS_RGB` for GDAL's default
/// `PHOTOMETRIC=RGB`; or YCbCr with 4:2:0 subsampling (`PHOTOMETRIC=YCBCR`).
fn jpeg_encode(data: &[u8], c: &ChunkInfo, quality: u8, ycbcr: bool) -> Result<Vec<u8>> {
    let color = match (c.t, c.spp) {
        (DataType::U8, 1) => jpeg_encoder::ColorType::Luma,
        // RGB input, converted to YCbCr by the encoder.
        (DataType::U8, 3) if ycbcr => jpeg_encoder::ColorType::Rgb,
        // Components written as given: no color transform.
        (DataType::U8, 3) => jpeg_encoder::ColorType::Ycbcr,
        (t, n) => return Err(Error::Unsupported(format!("JPEG is written for 1 or 3 Byte bands, not {n} {t:?} bands"))),
    };
    let (w, h) = (u16::try_from(c.w), u16::try_from(c.h));
    let (Ok(w), Ok(h)) = (w, h) else { return Err(Error::Unsupported("JPEG chunks are at most 65535 pixels wide and high".into())) };
    let mut out = Vec::new();
    let mut enc = jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100));
    if c.spp == 3 && ycbcr {
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_2_0);
    } else if c.spp == 3 {
        enc.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_4_4);
        const ANNEX_K_LUMA: [u16; 64] = [
            16, 11, 10, 16, 24, 40, 51, 61, 12, 12, 14, 19, 26, 58, 60, 55, 14, 13, 16, 24, 40, 57, 69, 56, 14, 17, 22, 29, 51, 87, 80, 62,
            18, 22, 37, 56, 68, 109, 103, 77, 24, 35, 55, 64, 81, 104, 113, 92, 49, 64, 78, 87, 103, 121, 120, 101, 72, 92, 95, 98, 112,
            100, 103, 99,
        ];
        // The luminance table for every component, scaled by the quality as libjpeg scales it
        // (`jpeg_quality_scaling`, baseline values).
        let q = quality.clamp(1, 100) as u32;
        let scale = if q < 50 { 5000 / q } else { 200 - 2 * q };
        let table = ANNEX_K_LUMA.map(|v| ((v as u32 * scale + 50) / 100).clamp(1, 255) as u16);
        enc.set_quantization_tables(
            jpeg_encoder::QuantizationTableType::Custom(Box::new(table)),
            jpeg_encoder::QuantizationTableType::Custom(Box::new(table)),
        );
    }
    enc.encode(data, w, h, color).map_err(|e| Error::InvalidData(format!("JPEG: {e}")))?;
    Ok(out)
}

/// A Zstandard chunk at `level` (with the `libzstd` feature; without it, `zrip`, at most level 4).
fn zstd_compress(data: &[u8], level: i32) -> Result<Vec<u8>> {
    Ok(crate::zstandard::compress(data, level)?)
}

/// PackBits run-length encoding, as TIFF defines it.
fn packbits(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 128 + 1);
    let mut i = 0;
    while i < data.len() {
        // A run of at least 3 equal bytes is worth encoding as a run.
        let mut run = 1;
        while i + run < data.len() && run < 128 && data[i + run] == data[i] {
            run += 1;
        }
        if run >= 3 {
            out.push((1 - run as i32) as u8);
            out.push(data[i]);
            i += run;
            continue;
        }
        let start = i;
        while i < data.len() && i - start < 128 {
            if i + 2 < data.len() && data[i] == data[i + 1] && data[i] == data[i + 2] {
                break;
            }
            i += 1;
        }
        out.push((i - start - 1) as u8);
        out.extend_from_slice(&data[start..i]);
    }
    out
}

/// What the encoder of `compression` allocates for a chunk of `raw` bytes of samples: the buffer
/// the chunk is written in, at its largest, and what it holds besides while it writes it.
fn encoder_bytes(compression: Compression, raw: usize) -> (usize, usize) {
    // The others write into a buffer that doubles as it fills (less than twice what it holds, and
    // 8 KiB for the buffer it starts as), and that frees the one it outgrew once it is copied: less
    // than 1/64 more than their samples, but JPEG and JPEG XL (below).
    let doubling = |written: usize| written.saturating_mul(2).saturating_add(8 << 10);
    let most = raw.saturating_add(raw / 64);
    let doubled = doubling(most);
    // As measured on tiles of 256 to 1024 pixels of samples no codec compresses: WebP's encoder
    // holds up to 4.9 times a tile's bytes besides its output (LERC, JPEG and JPEG XL less).
    let measured = raw.saturating_mul(5).saturating_add(2 << 20);
    match compression {
        Compression::None => (raw, 0),
        // `packbits` allocates the most it can write.
        Compression::PackBits => (raw.saturating_add(raw / 128 + 1), 0),
        // LZW writes up to half again as many bytes, 4 KiB at a time, into a vector that doubles
        // from 4 KiB; include the code table and retained long-match witnesses.
        Compression::Lzw => {
            let written = raw.saturating_add(raw / 2 + raw / 1024 + (4 << 10));
            let buffer = written.div_ceil(4 << 10).checked_next_power_of_two().map_or(usize::MAX, |n| n.saturating_mul(4 << 10));
            (buffer, buffer / 2 + super::tiff_lzw::Encoder::SCRATCH_BYTES)
        }
        // Deflate's match finder and sequences are each thread's ([`crate::deflate_compress::SCRATCH_BYTES`]).
        Compression::Deflate => (doubled, most),
        // As measured: Zstandard's encoder holds up to 2.3 MiB besides its output (`zrip`'s, at its
        // level 4; with the `libzstd` feature, the C library's context grows with `ZSTD_LEVEL` and
        // the tile, and is not counted).
        Compression::Zstd => (doubled, most.saturating_add(raw).saturating_add(2 << 20)),
        // As measured on chunks of samples none compresses: JPEG writes up to 1.6 times its samples
        // (at quality 100; less at lower ones), and JPEG XL up to 1.13 times.
        Compression::Jpeg => (doubling(raw.saturating_mul(2)), measured),
        Compression::Jxl => (doubling(raw.saturating_add(raw / 4)), measured),
        _ => (doubled, measured),
    }
}

/// How to write a GeoTIFF.
#[derive(Debug, Clone)]
pub(crate) struct TiffOptions {
    /// The compression.
    pub compression: Compression,
    /// Tile size; strips when `None`.
    pub tile_size: Option<u32>,
    /// Force BigTIFF; otherwise used only when the file would exceed 4 GiB.
    pub bigtiff: bool,
    /// Write a Cloud Optimized GeoTIFF: tiled, with overviews and GDAL's layout.
    pub cog: bool,
    /// The nodata value to declare.
    pub nodata: Option<f64>,
    /// The resampling of a COG's overviews (`OVERVIEW_RESAMPLING`, `RESAMPLING`); by default
    /// GDAL's: cubic, or nearest neighbour for paletted and complex rasters.
    pub cog_resampling: Option<Method>,
    /// The resampling option as given, which GDAL's COG driver records.
    pub cog_resampling_name: Option<String>,
    /// A COG's overviews keep the valid area of the full-resolution image (`OVERVIEW_RESAMPLING`
    /// `area_matched` or `area_matched_mode`, terrio's; see [`area_matched`]) instead of being
    /// resampled with [`TiffOptions::cog_resampling`].
    pub cog_area_matched: Option<AreaMatched>,
    /// A GeoTIFF whose overviews (and their masks) a COG copies rather than computing its own,
    /// as GDAL's COG driver does with a source that has some (`OVERVIEWS=AUTO`).
    pub source_overviews: Option<String>,
    /// Source overview opened as the copy's base image; only its later levels are copied.
    pub source_overview_level: Option<usize>,
    /// The number of a COG's overviews (`OVERVIEW_COUNT`); by default, until one fits in a tile.
    pub overview_count: Option<usize>,
    /// The block size of the raster written, which sets the blocks GDAL computes overviews in.
    pub source_block: Option<(usize, usize)>,
    /// Tiles of `(width, height)`, overriding `tile_size` (GeoTIFF only).
    pub block: Option<(u32, u32)>,
    /// Rows per strip, for striped GeoTIFFs. By default, strips of 64 KiB.
    pub rows_per_strip: Option<u32>,
    /// One plane per band (`INTERLEAVE=BAND`) instead of interleaved pixels.
    pub planar: bool,
    /// The `PhotometricInterpretation`: 1 (min-is-black), 2 (RGB) or 3 (palette). By default, RGB
    /// for three Byte bands, else min-is-black.
    pub photometric: Option<u16>,
    /// The last band is an (unassociated) alpha band.
    pub alpha: bool,
    /// Band descriptions, scales, offsets, units, color interpretations and metadata items.
    pub band_metadata: Vec<crate::BandMetadata>,
    /// The color table of a palette image.
    pub color_table: Vec<[u8; 4]>,
    /// Overview decimation factors (GeoTIFF only; COGs build their own).
    pub overviews: Vec<usize>,
    /// How the overviews of `overviews` are computed.
    pub overview_resampling: OverviewResampling,
    /// Overviews supplied by the caller, from the largest, written as they are
    /// ([`crate::RasterWriteOptions::overview_data`]).
    pub supplied_overviews: Vec<std::sync::Arc<Raster>>,
    /// Dataset metadata items set over the raster's own ([`crate::RasterWriteOptions::metadata`]).
    pub metadata: Vec<(String, String)>,
    /// The TIFF predictor: 1 (none), 2 (horizontal differencing) or 3 (floating point), or
    /// [`PREDICTOR_BY_TYPE`] for a COG's `PREDICTOR=YES`.
    pub predictor: u16,
    /// Write the pixel scale and tie point (`GEOREFERENCED=NO` leaves the file without a
    /// geotransform, as GDAL writes rasters whose geotransform is the identity).
    pub georeferenced: bool,
    /// The geotransform to write instead of the raster's: rotated or south-up ones go to a
    /// `ModelTransformationTag`.
    pub geo_transform: Option<[f64; 6]>,
    /// Ground control points, written as tie points instead of a geotransform.
    pub gcps: Vec<crate::Gcp>,
    /// A validity mask (`cols * rows`, 0 for invalid), written as GDAL's internal mask.
    pub mask: Option<Vec<u8>>,
    /// The mask is a band without a dataset, as a MEM dataset holds the mask GDAL's
    /// `MEMRasterBand::CreateMaskBand` creates (rasterio's buffered COG writes, private option
    /// `@MASK_WITHOUT_DATASET`): see [`mask_levels`].
    pub mask_without_dataset: bool,
    /// The 92 values of the `RPCCoefficientTag`, or none.
    pub rpc: Vec<f64>,
    /// The parameters of LERC and JPEG.
    pub codec: CodecParams,
    /// The raster's bands are pairs of real and imaginary parts (a complex dataset): written as
    /// complex samples (`SampleFormat` 5 or 6, GDAL's `CInt16`... `CFloat64`).
    pub complex: bool,
    /// Stored unsigned sample width (`NBITS`), rather than the container type width.
    pub nbits: Option<u16>,
    /// Leave out the blocks of the full-resolution image whose samples are all nodata, or 0
    /// without nodata (`SPARSE_OK`, GeoTIFF only): their offset and byte count are 0, and they
    /// read as nodata (or 0). Turned off where the `.aux.xml` would read them otherwise
    /// (`TiffOptions::sparse_unless_sidecar_differs`).
    /// GDAL 3.13.3 is the reference for block omission. Terrio warns and disables this option
    /// for lossy codecs, conflicting sidecars and COGs rather than promising parity there.
    pub sparse: bool,
    /// Metadata domains other than the default one (`@METADATA_DOMAINS`), those GDAL writes.
    pub domains: Vec<MetadataDomain>,
    /// Leave out the dataset's and the bands' default-domain items, as GDAL's `CreateCopy` does
    /// for some `COPY_SRC_MDD` and `SRC_MDD` options.
    pub drop_default_items: (bool, bool),
}

/// A metadata domain of the dataset (`band` 0) or of a band (from 1), its items in order.
#[derive(Debug, Clone)]
pub(crate) struct MetadataDomain {
    pub band: usize,
    pub name: String,
    pub items: Vec<(String, String)>,
}

impl MetadataDomain {
    /// The domains of `@METADATA_DOMAINS` (JSON `[[band, domain, {name: value...}]...]`, terrio.rio's
    /// `update_tags`) that GDAL writes into a GeoTIFF, following `GTiffDataset::WriteMetadata`
    /// (gtiffdataset_write.cpp) and `WriteMDMetadata`, which leaves out IMAGE_STRUCTURE,
    /// DERIVED_SUBDATASETS, COLOR_PROFILE, RPC and xml:XMP. A GeoTIFF writes the domains of the
    /// dataset it created (`bSrcIsGeoTIFF`). The COG driver copies a dataset (`CreateCopy`), which
    /// writes, with `COPY_SRC_MDD=AUTO`, only json:ISIS3 and json:VICAR (dataset) and IMAGERY
    /// (bands); with `COPY_SRC_MDD=YES` or `SRC_MDD`, the domains those name. Also returns whether
    /// the dataset's and the bands' default-domain items are left out.
    fn parse(json: Option<&str>, cog: bool, copy_src_mdd: Option<&str>, src_mdd: &[&str]) -> Result<(Vec<Self>, (bool, bool))> {
        let invalid = |e: String| Error::InvalidArgument(format!("invalid @METADATA_DOMAINS: {e}"));
        let mut domains = Vec::new();
        if let Some(json) = json {
            let v: serde_json::Value = serde_json::from_str(json).map_err(|e| invalid(e.to_string()))?;
            for d in v.as_array().ok_or_else(|| invalid("not a list".into()))? {
                let (Some(band), Some(name), Some(items)) =
                    (d.get(0).and_then(serde_json::Value::as_u64), d.get(1).and_then(|n| n.as_str()), d.get(2).and_then(|i| i.as_object()))
                else {
                    return Err(invalid(d.to_string()));
                };
                let items = items.iter().map(|(k, v)| (k.clone(), v.as_str().map_or_else(|| v.to_string(), str::to_string))).collect();
                domains.push(Self { band: band as usize, name: name.to_string(), items });
            }
        }
        let skipped = |n: &str| {
            ["", "IMAGE_STRUCTURE", "DERIVED_SUBDATASETS", "COLOR_PROFILE", "RPC", "xml:XMP"].iter().any(|s| n.eq_ignore_ascii_case(s))
        };
        domains.retain(|d| !skipped(&d.name) && !d.items.is_empty());
        if !cog {
            return Ok((domains, (false, false)));
        }
        let copy = copy_src_mdd.unwrap_or("AUTO");
        let auto = copy.eq_ignore_ascii_case("AUTO");
        // CPLTestBool: anything but NO, FALSE, OFF and 0.
        let test_bool = !["NO", "FALSE", "OFF", "0"].iter().any(|s| copy.eq_ignore_ascii_case(s));
        let listed = |n: &str| src_mdd.iter().any(|s| s.eq_ignore_ascii_case(n));
        let any = auto || test_bool || !src_mdd.is_empty();
        let named = (!auto && test_bool) || !src_mdd.is_empty();
        domains.retain(|d| {
            let wanted = src_mdd.is_empty() || listed(&d.name);
            if d.band == 0 {
                any && ((auto && src_mdd.is_empty() && ["json:ISIS3", "json:VICAR"].iter().any(|s| d.name.eq_ignore_ascii_case(s)))
                    || (named && wanted))
            } else if auto && src_mdd.is_empty() {
                d.name.eq_ignore_ascii_case("IMAGERY")
            } else {
                (test_bool || !src_mdd.is_empty()) && wanted
            }
        });
        let dataset_items = any && (src_mdd.is_empty() || listed("") || listed("_DEFAULT_"));
        Ok((domains, (!dataset_items, !(auto && src_mdd.is_empty()))))
    }
}

impl Default for TiffOptions {
    fn default() -> Self {
        Self {
            compression: Compression::None,
            tile_size: None,
            bigtiff: false,
            cog: false,
            nodata: None,
            cog_resampling: None,
            cog_resampling_name: None,
            cog_area_matched: None,
            source_overviews: None,
            source_overview_level: None,
            overview_count: None,
            source_block: None,
            block: None,
            rows_per_strip: None,
            planar: false,
            photometric: None,
            alpha: false,
            band_metadata: Vec::new(),
            color_table: Vec::new(),
            overviews: Vec::new(),
            overview_resampling: OverviewResampling::Gdal(Method::Nearest),
            supplied_overviews: Vec::new(),
            metadata: Vec::new(),
            georeferenced: true,
            predictor: 1,
            geo_transform: None,
            gcps: Vec::new(),
            mask: None,
            mask_without_dataset: false,
            rpc: Vec::new(),
            codec: CodecParams::default(),
            complex: false,
            nbits: None,
            sparse: false,
            domains: Vec::new(),
            drop_default_items: (false, false),
        }
    }
}

/// How the overviews given as factors are computed: as GDAL does, or terrio's minimum or
/// maximum of the valid pixels, or terrio's area-matched levels (see [`area_matched`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverviewResampling {
    Gdal(Method),
    Min,
    Max,
    AreaMatched(AreaMatched),
}

impl OverviewResampling {
    fn parse(s: &str) -> Result<Self> {
        if let Some(m) = AreaMatched::parse(s) {
            return Ok(Self::AreaMatched(m));
        }
        Ok(match s.trim().to_ascii_lowercase().as_str() {
            "min" => Self::Min,
            "max" => Self::Max,
            other => Self::Gdal(Method::parse(other).ok_or_else(|| unsupported_resampling(other))?),
        })
    }
}

/// The resampling of the overviews that `add_overviews` computes (GDAL's `gdaladdo`), with its
/// own kernels (`ops::overview_kernel`); the overviews of a GeoTIFF or COG being written take
/// [`OverviewResampling`] and GDAL's kernels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AddOverviewsResampling {
    Nearest,
    Average,
    Min,
    Max,
    Mode,
}

impl AddOverviewsResampling {
    /// GDAL's name for the method, as its overviews' `RESAMPLING` metadata item holds it.
    pub(crate) fn gdal_name(self) -> &'static str {
        match self {
            Self::Nearest => "NEAREST",
            Self::Average => "AVERAGE",
            Self::Min => "MIN",
            Self::Max => "MAX",
            Self::Mode => "MODE",
        }
    }

    pub(crate) fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "near" | "nearest" => Self::Nearest,
            "average" => Self::Average,
            "min" => Self::Min,
            "max" => Self::Max,
            "mode" => Self::Mode,
            other => {
                return Err(Error::Unsupported(format!(
                    "'{other}' overview resampling is not supported (nearest, average, min, max, mode)"
                )));
            }
        })
    }
}

fn unsupported_resampling(name: &str) -> Error {
    Error::Unsupported(format!(
        "'{name}' overview resampling is not supported (nearest, average, rms, bilinear, cubic, cubicspline, lanczos, mode, area_matched, area_matched_mode)"
    ))
}

/// The warning for a requested sparse COG, whose tiles are always written.
pub(crate) const SPARSE_COG_WARNING: &str = "SPARSE_OK is not supported for COGs: every tile is written";

/// Whether a creation option value means yes.
pub(crate) fn yes(v: &str) -> bool {
    matches!(v.to_ascii_uppercase().as_str(), "YES" | "TRUE" | "ON" | "1")
}

/// The [`TiffOptions::predictor`] of a COG's `PREDICTOR=YES`, which [`plan`] resolves from the
/// data type.
const PREDICTOR_BY_TYPE: u16 = 0;

/// A `PREDICTOR` creation option. A COG reads it as GDAL 3.13's COG driver does (`GetPredictor`
/// in `frmts/gtiff/cogdriver.cpp`): `YES`, `ON` or `TRUE` is 3 for floating-point data and 2
/// otherwise ([`PREDICTOR_BY_TYPE`]), `STANDARD` is 2, `FLOATING_POINT` is 3, and `NO`, `FALSE`,
/// `OFF` or `NONE` leave the predictor out. A GeoTIFF keeps its numbers and `YES`, `NO` and
/// `NONE` (GDAL's GTiff driver takes only the numbers). Other values are refused, where GDAL's COG
/// driver warns and writes no predictor.
fn parse_predictor(value: &str, cog: bool) -> Result<u16> {
    Ok(match value.trim().to_ascii_uppercase().as_str() {
        "1" | "NO" | "NONE" => 1,
        "FALSE" | "OFF" if cog => 1,
        "YES" | "ON" | "TRUE" if cog => PREDICTOR_BY_TYPE,
        "2" | "YES" => 2,
        "STANDARD" if cog => 2,
        "3" => 3,
        "FLOATING_POINT" if cog => 3,
        other if cog => {
            return Err(Error::InvalidArgument(format!("invalid PREDICTOR '{other}' (NO, YES, STANDARD or 2, FLOATING_POINT or 3)")));
        }
        other => return Err(Error::InvalidArgument(format!("invalid PREDICTOR '{other}' (1, 2, 3)"))),
    })
}

impl TiffOptions {
    /// The options for writing a GeoTIFF (or a COG) with `options`, its creation options read as
    /// GDAL's. `None` when the compression is one this writer does not handle (a lossy one).
    pub(crate) fn from_write_options(options: &crate::RasterWriteOptions, cog: bool, nodata: Option<f64>) -> Result<Option<Self>> {
        let co = |key: &str| options.creation_options.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());
        let name = options.compression.as_deref().or_else(|| co("COMPRESS")).unwrap_or(if cog { "LZW" } else { "NONE" });
        let Some(compression) = Compression::parse(name) else { return Ok(None) };
        #[cfg(not(feature = "lerc"))]
        if matches!(compression, Compression::Lerc | Compression::LercDeflate | Compression::LercZstd) {
            return Err(Error::disabled("lerc"));
        }
        let size = |key: &str| -> Result<Option<u32>> {
            co(key).map(|v| v.trim().parse::<u32>().map_err(|_| Error::InvalidArgument(format!("invalid {key} '{v}'")))).transpose()
        };
        let mut o = Self {
            compression,
            tile_size: options.tile_size,
            bigtiff: options.bigtiff || co("BIGTIFF").is_some_and(yes),
            cog,
            nodata,
            band_metadata: options.band_metadata.clone(),
            color_table: options.color_table.clone(),
            geo_transform: options.geo_transform,
            gcps: options.gcps.clone(),
            mask: options.mask.clone(),
            rpc: options.rpc.clone(),
            ..Default::default()
        };
        if let Some(bits) = size("NBITS")? {
            o.nbits = Some(u16::try_from(bits).map_err(|_| Error::InvalidArgument(format!("invalid NBITS '{bits}'")))?);
        }
        if cog {
            if let Some(b) = size("BLOCKSIZE")? {
                o.tile_size = Some(b);
            }
            // GDAL's COG driver reads OVERVIEW_RESAMPLING, then RESAMPLING; rasterio's
            // `overview_resampling` keyword is the first.
            if let Some(r) = co("OVERVIEW_RESAMPLING").or_else(|| co("RESAMPLING")).or(options.overview_resampling.as_deref()) {
                o.cog_area_matched = AreaMatched::parse(r);
                if o.cog_area_matched.is_none() {
                    o.cog_resampling = Some(Method::parse(r).ok_or_else(|| unsupported_resampling(r))?);
                }
                o.cog_resampling_name = Some(r.to_ascii_uppercase());
            }
            o.source_overviews = co("@COPY_SRC_OVERVIEWS").map(str::to_string);
            o.source_overview_level = co("@COPY_SRC_OVERVIEW_LEVEL")
                .map(|value| value.parse().map_err(|_| Error::InvalidArgument(format!("invalid source overview level '{value}'"))))
                .transpose()?;
            if o.cog_area_matched.is_some() && o.source_overviews.is_some() {
                return Err(Error::InvalidArgument(
                    "area-matched overviews are computed, not copied from the source: remove @COPY_SRC_OVERVIEWS".into(),
                ));
            }
            o.mask_without_dataset = co("@MASK_WITHOUT_DATASET").is_some_and(yes);
            if let Some(n) = co("OVERVIEW_COUNT") {
                let n: i64 = n.trim().parse().map_err(|_| Error::InvalidArgument(format!("invalid OVERVIEW_COUNT '{n}'")))?;
                o.overview_count = usize::try_from(n).ok();
            }
            // GDAL 3.13.3's COG driver (`GDALCOGCreator::Create`): AUTO writes the existing
            // overviews, else generates them; FORCE_USE_EXISTING and NONE never generate any;
            // IGNORE_EXISTING always does. Supplied overviews are the existing ones, so the two
            // that would discard them are contradictions.
            let supplied = !options.overview_data.is_empty();
            match co("OVERVIEWS").map(|v| v.trim().to_ascii_uppercase()).as_deref() {
                Some(v @ ("NONE" | "IGNORE_EXISTING")) if supplied => {
                    return Err(Error::InvalidArgument(format!(
                        "OVERVIEWS={v} contradicts the overviews supplied, which are written as they are"
                    )));
                }
                Some("NONE") => o.overview_count = Some(0),
                Some("FORCE_USE_EXISTING") if !supplied && o.source_overviews.is_none() => {
                    crate::warnings::warn(
                        "OVERVIEWS=FORCE_USE_EXISTING with no existing overviews (none supplied, none copied from a source): the COG has \
                         none, as GDAL's COG driver writes none"
                            .into(),
                    );
                    o.overview_count = Some(0);
                }
                _ => {}
            }
            o.source_block = options.source_block_size;
            if co("SPARSE_OK").is_some_and(yes) {
                crate::warnings::warn(SPARSE_COG_WARNING.into());
            }
        } else {
            let (bx, by) = (size("BLOCKXSIZE")?, size("BLOCKYSIZE")?);
            if co("TILED").is_some_and(yes) {
                let t = options.tile_size.unwrap_or(256);
                o.block = Some((bx.unwrap_or(t), by.unwrap_or(t)));
            } else if options.tile_size.is_none() {
                o.rows_per_strip = by;
            }
            o.overviews = options.overviews.clone();
            if let Some(r) = &options.overview_resampling {
                o.overview_resampling = OverviewResampling::parse(r)?;
            }
            o.sparse = co("SPARSE_OK").is_some_and(yes);
        }
        o.metadata = options.metadata.clone();
        if !options.overview_data.is_empty() {
            if !options.overviews.is_empty() {
                return Err(Error::InvalidArgument("overviews are either supplied or given as factors, not both".into()));
            }
            if o.source_overviews.is_some() {
                return Err(Error::InvalidArgument("overviews are either supplied or copied from a source, not both".into()));
            }
            // GDAL 3.13.3 ignores OVERVIEW_RESAMPLING and RESAMPLING alike when it writes existing
            // overviews; `cog_resampling_name` holds whichever of the three spellings was given.
            if options.overview_resampling.is_some() || o.cog_resampling_name.is_some() {
                return Err(Error::InvalidArgument("supplied overviews are written as they are: no overview resampling applies".into()));
            }
            o.supplied_overviews = options.overview_data.clone();
        }
        if let Some(i) = co("INTERLEAVE") {
            o.planar = match i.to_ascii_uppercase().as_str() {
                "BAND" => true,
                "PIXEL" => false,
                other => return Err(Error::InvalidArgument(format!("unsupported INTERLEAVE '{other}' (PIXEL, BAND)"))),
            };
        }
        if let Some(p) = co("PHOTOMETRIC") {
            o.photometric = Some(match p.to_ascii_uppercase().as_str() {
                "MINISBLACK" => 1,
                "RGB" => 2,
                "PALETTE" => 3,
                "YCBCR" if o.compression == Compression::Jpeg => 6,
                other => {
                    return Err(Error::Unsupported(format!(
                        "PHOTOMETRIC={other} is not supported (MINISBLACK, RGB, PALETTE, YCBCR with JPEG)"
                    )));
                }
            });
        }
        o.georeferenced = super::common::georeferenced(&options.creation_options);
        let src_mdd: Vec<&str> =
            options.creation_options.iter().filter(|(k, _)| k.eq_ignore_ascii_case("SRC_MDD")).map(|(_, v)| v.as_str()).collect();
        (o.domains, o.drop_default_items) = MetadataDomain::parse(co("@METADATA_DOMAINS"), cog, co("COPY_SRC_MDD"), &src_mdd)?;
        if let Some(p) = co("PREDICTOR") {
            o.predictor = parse_predictor(p, cog)?;
        }
        // The levels are checked whatever the compression, as the qualities are. GDAL's COG
        // driver takes LEVEL as ZLEVEL for Deflate and as ZSTD_LEVEL for Zstandard, in their
        // place (GDAL 3.13.3 `GDALCOGCreator::Create`); other codecs ignore it. LEVEL, read last,
        // wins.
        let compression = o.compression;
        let levels = |key: &'static str, codecs: [Compression; 2]| {
            let level = co("LEVEL").filter(|_| cog && codecs.contains(&compression));
            co(key).map(|v| (key, v)).into_iter().chain(level.map(|v| ("LEVEL", v)))
        };
        for (key, v) in levels("ZLEVEL", [Compression::Deflate, Compression::LercDeflate]) {
            o.codec.deflate_level = v
                .trim()
                .parse::<u32>()
                .ok()
                .and_then(crate::deflate_compress::Level::new)
                .ok_or_else(|| Error::InvalidArgument(format!("invalid {key} '{v}' (1 to 9)")))?;
        }
        for (key, v) in levels("ZSTD_LEVEL", [Compression::Zstd, Compression::LercZstd]) {
            o.codec.zstd_level = v
                .trim()
                .parse::<i32>()
                .ok()
                .filter(|l| (1..=22).contains(l))
                .ok_or_else(|| Error::InvalidArgument(format!("invalid {key} '{v}' (1 to 22)")))?;
        }
        if let Some(v) = co("MAX_Z_ERROR") {
            o.codec.max_z_error = v
                .trim()
                .parse::<f64>()
                .ok()
                .filter(|e| *e >= 0.0)
                .ok_or_else(|| Error::InvalidArgument(format!("invalid MAX_Z_ERROR '{v}'")))?;
        }
        if let Some((key, v)) = co("JPEG_QUALITY")
            .map(|v| ("JPEG_QUALITY", v))
            .or_else(|| if o.compression == Compression::WebP { None } else { co("QUALITY").map(|v| ("QUALITY", v)) })
        {
            let quality = v
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|q| (1..=100).contains(q))
                .ok_or_else(|| Error::InvalidArgument(format!("invalid {key} '{v}' (1 to 100)")))?;
            if o.compression == Compression::Jpeg {
                o.codec.jpeg_quality = quality;
            }
        }
        if o.compression == Compression::WebP {
            o.codec.webp_lossless = match co("WEBP_LOSSLESS") {
                None => false,
                Some(value) => match value.trim().to_ascii_uppercase().as_str() {
                    "YES" | "TRUE" | "ON" | "1" => true,
                    "NO" | "FALSE" | "OFF" | "0" => false,
                    _ => return Err(Error::InvalidArgument(format!("invalid WEBP_LOSSLESS '{value}' (YES or NO)"))),
                },
            };
            if let Some((key, value)) = co("WEBP_LEVEL").map(|v| ("WEBP_LEVEL", v)).or_else(|| co("QUALITY").map(|v| ("QUALITY", v))) {
                o.codec.webp_quality = value
                    .trim()
                    .parse::<u8>()
                    .ok()
                    .filter(|q| *q <= 100)
                    .ok_or_else(|| Error::InvalidArgument(format!("invalid {key} '{value}' (0 to 100)")))?;
            }
        }
        if o.compression == Compression::Jxl && co("JXL_LOSSLESS").is_some_and(|v| !yes(v)) {
            return Err(Error::Unsupported("lossy JPEG XL is not written: JXL_LOSSLESS=YES only".into()));
        }
        o.alpha = co("ALPHA").is_some_and(|v| yes(v) || v.eq_ignore_ascii_case("NON-PREMULTIPLIED"));
        if !o.color_table.is_empty() && o.photometric.is_none() {
            o.photometric = Some(3);
        }
        // A lossy codec stores a block of nodata only approximately, where a block left out reads
        // as nodata exactly: left out, it would read otherwise than stored.
        let lossy = match o.compression {
            Compression::Jpeg => true,
            Compression::WebP => !o.codec.webp_lossless,
            Compression::Lerc | Compression::LercDeflate | Compression::LercZstd => o.codec.max_z_error > 0.0,
            _ => false,
        };
        if o.sparse && lossy {
            crate::warnings::warn("SPARSE_OK is ignored with a lossy compression: every block is written".into());
            o.sparse = false;
        }
        Ok(Some(o))
    }

    /// The nodata tag written for samples of type `t` as readers parse it, which the blocks left
    /// out (`SPARSE_OK`) read as: a NaN of any sign or payload is written `nan`, read as
    /// `f64::NAN`; any other value reads back as written.
    pub(crate) fn nodata_as_read(&self, t: DataType) -> Option<f64> {
        self.nodata.and_then(|nd| crate::geotiff::parse_nodata(&format_nodata_for(nd, t)))
    }

    /// Turns `SPARSE_OK` off, with a warning, when a block left out of the GeoTIFF at `path`
    /// could read otherwise with the `.aux.xml` sidecar beside it than without. Without it, every
    /// band reads as the nodata tag, or 0. With it, a band reads as its nodata there, else as the
    /// tag (or the sidecar's dataset nodata), else 0 (`GeoTags::apply_sidecar_crs`). `written`:
    /// the band nodata the writer puts in the sidecar, taken as readers read it back (a NaN of
    /// any payload is written `nan`); `None` for the sidecar already there, which the writer
    /// keeps. `bands`: the file's bands, of samples of type `t`.
    pub(crate) fn sparse_unless_sidecar_differs(&mut self, path: &str, written: Option<&[Option<f64>]>, bands: usize, t: DataType) {
        if !self.sparse {
            return;
        }
        let (band_nodata, dataset_nodata) = match written {
            Some(values) => {
                let as_read = |v: f64| super::common::parse_f64(&format_nodata(v));
                (values.iter().enumerate().filter_map(|(i, v)| v.and_then(as_read).map(|v| (i + 1, v))).collect(), None)
            }
            None => {
                let mut aux = std::ffi::OsString::from(path);
                aux.push(".aux.xml");
                let Some(xml) = crate::memfs::read(std::path::Path::new(&aux)).ok().and_then(|b| String::from_utf8(b).ok()) else {
                    return;
                };
                let pam = super::common::parse_pam(&xml);
                (pam.band_nodata, pam.dataset_nodata)
            }
        };
        let tag = self.nodata_as_read(t);
        let fill = tag.unwrap_or(0.0);
        let same = (1..=bands).all(|band| {
            let with = band_nodata.iter().find(|(b, _)| *b == band).map(|(_, v)| *v).or(tag).or(dataset_nodata);
            with.unwrap_or(0.0).to_bits() == fill.to_bits()
        });
        if !same {
            crate::warnings::warn(
                "SPARSE_OK is ignored: the .aux.xml sidecar gives a band a nodata other than the file's, so every block is written".into(),
            );
            self.sparse = false;
        }
    }
}

// TIFF field types.
const SHORT: u16 = 3;
const LONG: u16 = 4;
const ASCII: u16 = 2;
const DOUBLE: u16 = 12;
const LONG8: u16 = 16;

/// A tag's values.
#[derive(Clone)]
pub(crate) enum Value {
    Shorts(Vec<u16>),
    /// Offsets or counts: LONG in classic TIFF, LONG8 in BigTIFF.
    Offsets(Vec<u64>),
    Longs(Vec<u32>),
    Doubles(Vec<f64>),
    Ascii(String),
}

impl Value {
    pub(crate) fn encode(&self, big: bool) -> (u16, u64, Vec<u8>) {
        match self {
            Value::Shorts(v) => (SHORT, v.len() as u64, v.iter().flat_map(|x| x.to_le_bytes()).collect()),
            Value::Longs(v) => (LONG, v.len() as u64, v.iter().flat_map(|x| x.to_le_bytes()).collect()),
            Value::Offsets(v) if big => (LONG8, v.len() as u64, v.iter().flat_map(|x| x.to_le_bytes()).collect()),
            Value::Offsets(v) => (LONG, v.len() as u64, v.iter().flat_map(|x| (*x as u32).to_le_bytes()).collect()),
            Value::Doubles(v) => (DOUBLE, v.len() as u64, v.iter().flat_map(|x| x.to_le_bytes()).collect()),
            Value::Ascii(s) => {
                let mut b = s.as_bytes().to_vec();
                b.push(0);
                (ASCII, b.len() as u64, b)
            }
        }
    }
}

/// One image (full resolution or overview) ready to be written: its tags without the chunk
/// offsets, and its compressed chunks.
pub(crate) struct Image {
    pub(crate) tags: Vec<(u16, Value)>,
    pub(crate) chunks: Vec<Vec<u8>>,
    pub(crate) tiled: bool,
    clipped_bits: Option<u16>,
}

impl Image {
    fn report_clipping(&self) {
        if let Some(bits) = self.clipped_bits {
            crate::warnings::warn(format!("One or more pixels clipped to fit {bits} bit domain."));
        }
    }
}

/// Writes a raster as a GeoTIFF or COG.
pub(crate) fn write(raster: &Raster, path: &str, opts: &TiffOptions) -> Result<()> {
    let images = prepare(raster, opts)?;
    let mut file = std::io::BufWriter::with_capacity(1 << 20, create_replacing(path)?);
    images.write_to(&mut file)?;
    file.flush()?;
    file.get_mut().finish()?;
    Ok(())
}

/// Encodes a raster as a GeoTIFF or COG in memory.
pub(crate) fn encode(raster: &Raster, opts: &TiffOptions) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    prepare(raster, opts)?.write_to(&mut out)?;
    Ok(out)
}

/// The images of a TIFF file, compressed, ready to be laid out.
struct Prepared {
    images: Vec<Image>,
    big: bool,
    cog: bool,
}

impl Prepared {
    fn write_to(self, out: &mut impl Write) -> std::io::Result<()> {
        assemble(self.images, self.big, self.cog, out)
    }
}

/// Splits a raster (and its overviews) into compressed chunks with their tags.
/// What every image of a file shares: the tiling, the layout of the samples, and the tags of the
/// full-resolution image (georeferencing, metadata, color map).
struct Plan {
    tile: Option<(u32, u32)>,
    layout: Layout,
    geo: Vec<(u16, Value)>,
    color_map: Option<Value>,
}

/// Checks the options against the raster (only its header is read) and plans the file.
fn plan(raster: &Raster, opts: &TiffOptions) -> Result<Plan> {
    if raster.cols == 0 || raster.rows == 0 || raster.bands == 0 {
        return Err(Error::InvalidArgument("cannot write an empty raster".into()));
    }
    let native_bits = (raster.data_type.size_bytes() * 8) as u16;
    let minimum_bits = match raster.data_type {
        DataType::U16 => 9,
        DataType::U32 => 17,
        _ => 1,
    };
    let packed = opts.nbits.is_some_and(|bits| bits != native_bits);
    if let Some(bits) = opts.nbits {
        if !matches!(raster.data_type, DataType::U8 | DataType::U16 | DataType::U32)
            || opts.complex
            || bits < minimum_bits
            || bits > native_bits
        {
            return Err(Error::InvalidArgument(format!(
                "NBITS={bits} requires unsigned Byte, UInt16 or UInt32 samples and {minimum_bits}..={native_bits} bits"
            )));
        }
        if packed
            && !matches!(
                opts.compression,
                Compression::None | Compression::Lzw | Compression::Deflate | Compression::PackBits | Compression::Zstd
            )
        {
            return Err(Error::Unsupported("packed NBITS samples support NONE, LZW, DEFLATE, PACKBITS or ZSTD compression".into()));
        }
        if packed && opts.compression.takes_predictor() && opts.predictor != 1 {
            return Err(Error::Unsupported("packed NBITS samples require PREDICTOR=1".into()));
        }
        if packed && opts.nodata.is_some_and(|v| !v.is_finite() || v < 0.0 || v > ((1u64 << bits) - 1) as f64 || v.fract() != 0.0) {
            return Err(Error::InvalidArgument(format!("nodata {:?} cannot be represented by NBITS={bits}", opts.nodata.unwrap())));
        }
    }
    let tile = if opts.cog {
        let t = opts.tile_size.unwrap_or(512);
        Some((t, t))
    } else {
        opts.block.or(opts.tile_size.map(|t| (t, t)))
    };
    if let Some((tw, th)) = tile {
        for t in [tw, th] {
            if t == 0 || t % 16 != 0 {
                return Err(Error::InvalidArgument(format!("the tile size must be a multiple of 16, not {t}")));
            }
        }
    }
    let parts = if opts.complex { 2 } else { 1 };
    if opts.complex
        && (!raster.bands.is_multiple_of(2) || !matches!(raster.data_type, DataType::I16 | DataType::I32 | DataType::F32 | DataType::F64))
    {
        return Err(Error::InvalidArgument("complex GeoTIFF samples need pairs of Int16, Int32, Float32 or Float64 bands".into()));
    }
    if opts.complex
        && !matches!(
            opts.compression,
            Compression::None | Compression::Lzw | Compression::Deflate | Compression::PackBits | Compression::Zstd
        )
    {
        return Err(Error::Unsupported("complex GeoTIFF samples are written uncompressed or with LZW, DEFLATE, PACKBITS or ZSTD".into()));
    }
    // TIFF samples per pixel: a complex sample holds two bands.
    let samples = raster.bands / parts;
    let jpeg = opts.compression == Compression::Jpeg;
    // One band is laid out the same, band- or pixel-interleaved.
    if jpeg
        && (raster.data_type != DataType::U8 || !matches!(samples, 1 | 3) || (opts.planar && samples > 1) || !opts.color_table.is_empty())
    {
        return Err(Error::Unsupported("JPEG is written for 1 or 3 Byte bands, pixel-interleaved".into()));
    }
    if opts.compression == Compression::WebP
        && (raster.data_type != DataType::U8 || !matches!(samples, 3 | 4) || opts.planar || !opts.color_table.is_empty())
    {
        return Err(Error::Unsupported("WebP requires three or four pixel-interleaved Byte bands".into()));
    }
    // Two or four interleaved bands would be gray and alpha or RGBA in JPEG XL, which GDAL reads
    // as extra channels.
    if opts.compression == Compression::Jxl && !opts.planar && !matches!(samples, 1 | 3) {
        return Err(Error::Unsupported("JPEG XL is written for 1 or 3 interleaved bands (others with INTERLEAVE=BAND)".into()));
    }
    let photometric = match opts.photometric {
        Some(6) if jpeg && samples == 3 => 6,
        Some(6) => return Err(Error::Unsupported("PHOTOMETRIC=YCBCR needs JPEG compression of 3 bands".into())),
        Some(p) => p,
        // GDAL 3.13.3's COG driver writes three JPEG bands as YCbCr (`GDALCOGCreator::Create`);
        // `dataset::color_options` keeps PHOTOMETRIC=RGB off such a copy to match.
        None if opts.cog && jpeg && samples == 3 => 6,
        None if (samples == 3 || (samples == 4 && opts.compression == Compression::WebP)) && raster.data_type == DataType::U8 => 2,
        None => 1,
    };
    let base_samples = if matches!(photometric, 2 | 6) { 3 } else { 1 };
    if samples < base_samples {
        return Err(Error::InvalidArgument(format!("PHOTOMETRIC=RGB needs 3 bands or more, not {samples}")));
    }
    if photometric == 3 {
        if samples != 1 || !matches!(raster.data_type, DataType::U8 | DataType::U16) {
            return Err(Error::InvalidArgument("a color table needs a single Byte or UInt16 band".into()));
        }
        if opts.color_table.is_empty() {
            return Err(Error::InvalidArgument("PHOTOMETRIC=PALETTE needs a color table".into()));
        }
    }
    let mut extra = vec![0u16; samples - base_samples];
    if opts.alpha {
        match extra.last_mut() {
            Some(last) => *last = 2,
            None => return Err(Error::InvalidArgument("ALPHA=YES needs a band besides the color bands".into())),
        }
    }
    // As GDAL, the predictor only applies to compressed files; 3 to floating-point samples.
    let wanted = match opts.predictor {
        PREDICTOR_BY_TYPE if matches!(raster.data_type, DataType::F32 | DataType::F64) => 3,
        PREDICTOR_BY_TYPE => 2,
        p => p,
    };
    let predictor = if opts.compression.takes_predictor() && !opts.complex { wanted } else { 1 };
    if predictor == 3 && !matches!(raster.data_type, DataType::F32 | DataType::F64) {
        return Err(Error::InvalidArgument("PREDICTOR=3 is only supported for floating-point data".into()));
    }
    // The colour interpretations GDAL reads from the photometric and extra samples written
    // (`GTiffRasterBand`): GDAL_METADATA holds only the bands' others (see [`geo_tags`]).
    let implied: Vec<&str> = (0..samples).map(|i| implied_color_interp(photometric, &extra, base_samples, i)).collect();
    // GDAL 3.13.3's `GTiffDataset::CreateCopy` ignores the first three bands' colour
    // interpretations under PHOTOMETRIC=RGB given, and their Undefined ones in YCbCr: those read
    // as red, green and blue. Others, such as Gray or Blue first, are kept in YCbCr.
    let ignored = |i: usize, c: &str| {
        implied.get(i).is_some_and(|m| m.eq_ignore_ascii_case(c))
            || (i < 3 && (opts.photometric == Some(2) || (photometric == 6 && c.eq_ignore_ascii_case("Undefined"))))
    };
    let layout = Layout {
        tile,
        rows_per_strip: opts.rows_per_strip,
        planar: opts.planar && samples > 1,
        photometric,
        extra,
        predictor,
        parts,
        nbits: opts.nbits,
    };
    // GDAL's COG driver records the resampling of the overviews it makes (`@OVERVIEW_RESAMPLING`,
    // an IMAGE_STRUCTURE item): the option as given, else `GetResampling`'s; for overviews it
    // copies, the source's `RESAMPLING` item; none for overviews supplied as they are.
    let image_structure: Vec<(String, String)> = match (tile, &opts.source_overviews) {
        _ if !opts.supplied_overviews.is_empty() => Vec::new(),
        (Some(_), Some(path)) if opts.cog => crate::LazyRaster::open_tiff(path)
            .ok()
            .flatten()
            .and_then(|mut s| s.overview_resampling())
            .map(|r| vec![("OVERVIEW_RESAMPLING".to_string(), r)])
            .unwrap_or_default(),
        (Some((t, _)), None)
            if opts.cog && !gdal_overview::cog_sizes(raster.cols, raster.rows, t as usize, opts.overview_count).is_empty() =>
        {
            let default = if !opts.color_table.is_empty() || opts.complex { "NEAREST" } else { "CUBIC" };
            vec![("OVERVIEW_RESAMPLING".to_string(), opts.cog_resampling_name.clone().unwrap_or_else(|| default.to_string()))]
        }
        _ => Vec::new(),
    };
    let mut geo = geo_tags(raster, opts, &image_structure, ignored)?;
    if !opts.georeferenced || !opts.gcps.is_empty() || opts.geo_transform.is_some_and(|gt| !crate::is_north_up(&gt)) {
        geo.retain(|(tag, _)| !matches!(tag, 33550 | 33922));
    }
    if !opts.gcps.is_empty() {
        // GCPs as tie points, as GDAL writes them (with a CRS, GeoKeys as for a geotransform).
        let ties = opts.gcps.iter().flat_map(|g| [g.pixel, g.line, 0.0, g.x, g.y, g.z]).collect();
        geo.push((33922, Value::Doubles(ties)));
    } else if let Some(gt) = opts.geo_transform.filter(|_| opts.georeferenced) {
        if crate::is_north_up(&gt) {
            geo.retain(|(tag, _)| !matches!(tag, 33550 | 33922));
            geo.push((33550, Value::Doubles(vec![gt[1], -gt[5], 0.0])));
            geo.push((33922, Value::Doubles(vec![0.0, 0.0, 0.0, gt[0], gt[3], 0.0])));
        } else {
            let m = vec![gt[1], gt[2], 0.0, gt[0], gt[4], gt[5], 0.0, gt[3], 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0];
            geo.push((34264, Value::Doubles(m)));
        }
    }
    if pixel_is_point(raster, opts) {
        // Public geotransforms refer to corners. GeoTIFF point tags refer to centres.
        for (tag, value) in &mut geo {
            if let Value::Doubles(v) = value {
                match *tag {
                    33922 if !opts.gcps.is_empty() => {
                        for g in v.as_chunks_mut::<6>().0 {
                            g[0] -= 0.5;
                            g[1] -= 0.5;
                        }
                    }
                    33922 => {
                        let gt =
                            opts.geo_transform.unwrap_or([raster.x_min, raster.cell_size_x, 0.0, raster.y_max(), 0.0, -raster.cell_size_y]);
                        v[3] += (gt[1] + gt[2]) * 0.5;
                        v[4] += (gt[4] + gt[5]) * 0.5;
                    }
                    34264 => {
                        v[3] += (v[0] + v[1]) * 0.5;
                        v[7] += (v[4] + v[5]) * 0.5;
                    }
                    _ => {}
                }
            }
        }
    }
    if !opts.rpc.is_empty() {
        if opts.rpc.len() != 92 {
            return Err(Error::InvalidArgument(format!("RPCs are 92 coefficients, not {}", opts.rpc.len())));
        }
        geo.push((50844, Value::Doubles(opts.rpc.clone())));
    }
    let color_map = (photometric == 3).then(|| color_map_tag(&opts.color_table, opts.nbits.unwrap_or(native_bits)));
    Ok(Plan { tile, layout, geo, color_map })
}

/// The georeferencing, nodata and metadata tags [`write`] gives the full-resolution image of
/// `raster` (only its header is read) with these options.
pub(crate) fn georeferencing_tags(raster: &Raster, opts: &TiffOptions) -> Result<Vec<(u16, Value)>> {
    Ok(plan(raster, opts)?.geo)
}

fn prepare(raster: &Raster, opts: &TiffOptions) -> Result<Prepared> {
    let Plan { tile, layout, mut geo, color_map } = plan(raster, opts)?;
    if let Some(m) = &opts.mask
        && m.len() != raster.cols * raster.rows
    {
        return Err(Error::InvalidArgument(format!("the mask has {} values for {} pixels", m.len(), raster.cols * raster.rows)));
    }

    // The full-resolution image, then overviews: a COG's until one fits in a tile, or the
    // factors asked for.
    let mut levels = vec![Level::from_raster(raster)];
    // The per-dataset mask of each level, as GDAL reads it (0 or 255).
    // In a copied pyramid an empty entry denotes an absent physical mask, preserving
    // implicit nodata/alpha validity. Generated pyramids contain only present masks.
    let mut masks: Vec<Vec<u8>> = opts.mask.iter().map(|m| m.iter().map(|v| if *v != 0 { 255 } else { 0 }).collect()).collect();
    // The overviews of the mask as they are computed, before they are written as bits (level `i`
    // at `i - 1`): GDAL keeps them in a temporary Byte file, which the next level is computed
    // from (see [`mask_levels`]).
    let mut raw_masks: Vec<Vec<u8>> = Vec::new();
    // Area-matched levels, and what each band's keep: written to the bands' metadata.
    let mut reports = Vec::new();
    let (sizes, method) = match (opts.cog, tile) {
        _ if !opts.supplied_overviews.is_empty() => (Vec::new(), None),
        (true, Some(_)) if opts.source_overviews.is_some() => (Vec::new(), None),
        (true, Some((t, _))) if opts.cog_area_matched.is_some() => {
            let sizes = gdal_overview::cog_sizes(raster.cols, raster.rows, t as usize, opts.overview_count);
            let factors: Vec<u64> = (1..=sizes.len() as u32).map(|k| 1u64 << k.min(63)).collect();
            if let Some(m) = opts.cog_area_matched {
                reports = area_matched_levels(raster, opts, m, &sizes, &factors, &mut levels)?;
            }
            (Vec::new(), None)
        }
        (true, Some((t, _))) => {
            (gdal_overview::cog_sizes(raster.cols, raster.rows, t as usize, opts.overview_count), Some(cog_method(opts)))
        }
        (true, None) => (Vec::new(), None),
        (false, _) => {
            let mut factors: Vec<usize> = opts.overviews.iter().copied().filter(|f| *f > 1).collect();
            factors.sort_unstable();
            factors.dedup();
            let sizes: Vec<(usize, usize)> = factors.iter().map(|f| (raster.cols.div_ceil(*f), raster.rows.div_ceil(*f))).collect();
            match opts.overview_resampling {
                OverviewResampling::Gdal(m) => (sizes, Some(m)),
                OverviewResampling::AreaMatched(m) => {
                    let factors: Vec<u64> = factors.iter().map(|f| *f as u64).collect();
                    reports = area_matched_levels(raster, opts, m, &sizes, &factors, &mut levels)?;
                    (Vec::new(), None)
                }
                other => {
                    for (w, h) in sizes {
                        // Each overview is computed from the previous (larger) one.
                        let src = levels.last().filter(|l| l.cols > w).unwrap_or(&levels[0]);
                        let next = src.resample(w, h, other, opts.nodata);
                        levels.push(next);
                    }
                    (Vec::new(), None)
                }
            }
        }
    };
    if !opts.supplied_overviews.is_empty() {
        supplied_overviews(raster, opts, &mut levels, &mut masks)?;
    } else if let (true, Some(_), Some(path)) = (opts.cog, tile, &opts.source_overviews) {
        copy_source_overviews(path, raster, opts, &mut levels, &mut masks)?;
    }
    if !reports.is_empty() {
        // The full-resolution tags again, with each band's report in its metadata (replacing
        // the items of an earlier report the source carried).
        let mut with_reports = opts.clone();
        if with_reports.drop_default_items.1 {
            // Filter inherited items before adding reports produced by this write.
            with_reports.band_metadata.iter_mut().for_each(|b| b.items.clear());
            with_reports.drop_default_items.1 = false;
        }
        with_reports.band_metadata.resize(raster.bands.max(with_reports.band_metadata.len()), crate::BandMetadata::default());
        for (b, report) in with_reports.band_metadata.iter_mut().zip(&reports) {
            let items = report.items();
            b.items.retain(|(k, _)| !items.iter().any(|(name, _)| k == name));
            b.items.extend(items);
        }
        geo = plan(raster, &with_reports)?.geo;
    }
    if let Some(method) = method {
        let block = gdal_overview::ovr_block(opts.source_block);
        // GDAL weights each level by the masks of the level it is computed from
        // (`GDALRegenerateOverviewsMultiBand`): the alpha band's, and the per-dataset mask's,
        // whose own overviews are made first (`GDALCOGCreator::Create`, which cascades them;
        // `GTiffDataset::IBuildOverviews`, which cascades them unless with nearest neighbour).
        let (alpha, alpha_masks) = alpha_weights(opts, raster.bands, raster.data_type);
        // `GTiffDataset::IBuildOverviews` cascades through `GDALRegenerateOverviewsMultiBand` for
        // pixel-interleaved files or those with an alpha band; band-interleaved ones go band by
        // band (`GDALRegenerateOverviewsEx`), cascading only with methods other than nearest and
        // bands whose masks are nodata or all valid, else each level from the full resolution.
        let per_band = !opts.cog && opts.planar && alpha.is_none();
        let cascade = !per_band || (method != Method::Nearest && opts.mask.is_none());
        for (w, h) in sizes {
            // As GDAL does, each overview is computed from the previous one when it is larger.
            let si = if cascade && levels.last().is_some_and(|l| l.cols > w) { levels.len() - 1 } else { 0 };
            let src = &levels[si];
            let plan = gdal_overview::Plan::new(method, (src.cols, src.rows), (w, h), raster.bands, block, raster.data_type);
            // The data are weighted by the mask overview as GDAL keeps it (Byte values, which
            // weight the kernel), the bits written when the mask was regenerated as a mask band.
            let source_mask = match (opts.mask_without_dataset, si) {
                (true, 1..) => raw_masks.get(si - 1),
                _ => masks.get(si),
            };
            let weights = gdal_overview::Weights { alpha, alpha_masks, mask: source_mask.map(Vec::as_slice) };
            let data = gdal_overview::level(&plan, src.data.as_ref(), raster.bands, opts.nodata, weights);
            if !masks.is_empty() {
                // The mask band's overviews cascade on their own (with nearest neighbour, GTiff's
                // single-band regeneration decimates each from the full resolution).
                let cascade_mask = (opts.cog || method != Method::Nearest) && levels.last().is_some_and(|l| l.cols > w);
                let mi = if cascade_mask { levels.len() - 1 } else { 0 };
                let next = mask_levels(opts, method, &masks, &raw_masks, mi, (levels[mi].cols, levels[mi].rows), (w, h), block);
                masks.push(next.iter().map(|v| if *v != 0 { 255 } else { 0 }).collect());
                raw_masks.push(next);
            }
            levels.push(Level { cols: w, rows: h, bands: raster.bands, data: std::borrow::Cow::Owned(data) });
        }
    }
    let mut images = Vec::with_capacity(levels.len());
    for (i, level) in levels.iter().enumerate() {
        // `SPARSE_OK` leaves out blocks of the full-resolution image only.
        let empty = (opts.sparse && i == 0).then(|| opts.nodata_as_read(raster.data_type).unwrap_or(0.0));
        let mut img = level.sparse_image(raster.data_type, opts.compression, &opts.codec, &layout, empty)?;
        img.report_clipping();
        if i > 0 {
            img.tags.push((254, Value::Longs(vec![1])));
        }
        // Georeferencing on the full-resolution image; nodata on every image, as GDAL does, but
        // not on GDAL-method overviews of 64-bit integer rasters (see [`overview_nodata`]).
        // Area-matched levels must declare their invalid cells, including for integer nodata.
        for (tag, v) in &geo {
            if i == 0 || (*tag == 42113 && (!reports.is_empty() || overview_nodata(raster.data_type))) {
                img.tags.push((*tag, v.clone()));
            }
        }
        if let Some(cm) = &color_map {
            img.tags.push((320, cm.clone()));
        }
        img.tags.sort_by_key(|(t, _)| *t);
        images.push(img);
        // GDAL's internal mask follows each image it masks (`NewSubfileType` 4, and 5 for an
        // overview's).
        if let Some(m) = masks.get(i).filter(|m| !m.is_empty()) {
            let main = &images[images.len() - 1];
            let tag = |t: u16| {
                main.tags.iter().find_map(|(k, v)| match v {
                    Value::Longs(v) if *k == t => v.first().map(|x| *x as usize),
                    _ => None,
                })
            };
            let block = match (tag(322), tag(323), tag(278)) {
                (Some(w), Some(h), _) => (w, h, true),
                (_, _, Some(rows)) => (level.cols, rows, false),
                _ => (level.cols, level.rows, false),
            };
            let mut mi = mask_image(m, level.cols, level.rows, block)?;
            if i > 0 {
                for (k, v) in mi.tags.iter_mut() {
                    if *k == 254 {
                        *v = Value::Longs(vec![5]);
                    }
                }
            }
            images.push(mi);
        }
    }

    let data_len: u64 = images.iter().flat_map(|i| i.chunks.iter()).map(|c| c.len() as u64 + 8).sum();
    let big = opts.bigtiff || data_len > (u32::MAX as u64) - (1 << 24);
    Ok(Prepared { images, big, cog: opts.cog })
}

/// Adds the area-matched overviews of `sizes` (with nominal decimation `factors`) to `levels`, each
/// band on its own, and returns what each band's levels keep (see [`area_matched`]).
fn area_matched_levels(
    raster: &Raster,
    opts: &TiffOptions,
    method: AreaMatched,
    sizes: &[(usize, usize)],
    factors: &[u64],
    levels: &mut Vec<Level<'_>>,
) -> Result<Vec<area_matched::BandReport>> {
    let Some(nodata) = opts.nodata else {
        return Err(Error::InvalidArgument("area-matched overviews need a nodata value".into()));
    };
    let has_alpha = opts.alpha
        || opts.band_metadata.iter().take(raster.bands).any(|b| b.color_interp.as_deref().is_some_and(|c| c.eq_ignore_ascii_case("alpha")));
    if opts.mask.is_some() || opts.complex || has_alpha {
        return Err(Error::Unsupported("area-matched overviews are written for nodata rasters without a mask or alpha band".into()));
    }
    if sizes.is_empty() {
        return Ok(Vec::new());
    }
    let integral = !matches!(raster.data_type, DataType::F32 | DataType::F64);
    let replacement = gdal_overview::replacement_value(raster.data_type, nodata);
    let (cols, rows, bands) = (raster.cols, raster.rows, raster.bands);
    fn build<T: IntoRasterData>(
        data: &[T],
        (cols, rows, bands): (usize, usize, usize),
        sizes: &[(usize, usize)],
        factors: &[u64],
        method: AreaMatched,
        (nodata, replacement, integral): (f64, f64, bool),
    ) -> Result<(Vec<RasterData>, Vec<area_matched::BandReport>)> {
        let mut out: Vec<Vec<T>> = sizes.iter().map(|(w, h)| Vec::with_capacity(w * h * bands)).collect();
        let mut reports = Vec::with_capacity(bands);
        for b in 0..bands {
            let band =
                data.get(b * cols * rows..(b + 1) * cols * rows).ok_or_else(|| Error::InvalidData("a band is missing samples".into()))?;
            let (band_levels, report) = area_matched::band(band, (cols, rows), sizes, factors, method, nodata, replacement, integral)?;
            for (level, values) in out.iter_mut().zip(band_levels) {
                level.extend(values);
            }
            reports.push(report);
        }
        Ok((out.into_iter().map(T::into_data).collect(), reports))
    }
    let (data, reports) =
        with_data!(&raster.data, v => build(v, (cols, rows, bands), sizes, factors, method, (nodata, replacement, integral)))?;
    for ((w, h), data) in sizes.iter().zip(data) {
        levels.push(Level { cols: *w, rows: *h, bands, data: std::borrow::Cow::Owned(data) });
    }
    Ok(reports)
}

/// The resampling of a COG's overviews: the one asked for, else GDAL's default.
fn cog_method(opts: &TiffOptions) -> Method {
    opts.cog_resampling.unwrap_or_else(|| Method::cog_default(!opts.color_table.is_empty() || opts.complex))
}

/// The overviews of the GeoTIFF a COG is converted from, copied as they are, as GDAL's COG driver
/// copies a source's (`OVERVIEWS=AUTO`, `COPY_SRC_OVERVIEWS`), with their masks: the source's
/// per-level physical masks, leaving implicit nodata/alpha masks implicit. A mask supplied only to the
/// destination is generated from its full-resolution values (GDAL 3.13.3).
fn copy_source_overviews(path: &str, raster: &Raster, opts: &TiffOptions, levels: &mut Vec<Level>, masks: &mut Vec<Vec<u8>>) -> Result<()> {
    let mut src = crate::LazyRaster::open_tiff(path)?.ok_or_else(|| Error::InvalidArgument(format!("{path}: not a GeoTIFF")))?;
    let sizes = src.overviews();
    let (width, height) = match opts.source_overview_level {
        Some(i) => sizes.get(i).copied().ok_or_else(|| Error::InvalidArgument(format!("{path}: no source overview {i}")))?,
        None => (src.width(), src.height()),
    };
    if (width, height, src.band_count(), src.data_type()) != (raster.cols, raster.rows, raster.bands, raster.data_type) {
        return Err(Error::InvalidArgument(format!("{path}: not the raster written")));
    }
    let first = opts.source_overview_level.map_or(0, |i| i + 1);
    let has_base_mask = opts.source_overview_level.map_or_else(|| src.has_mask(), |i| src.level_has_own_mask(i));
    let tile = opts.tile_size.unwrap_or(512) as usize;
    // GDAL 3.13.3 generates a missing mask pyramid only beyond the output tile.
    // Smaller sources retain all-valid missing masks even when image overviews exist.
    let has_overview_mask = (first..sizes.len()).any(|i| src.level_has_own_mask(i));
    let destination_only_mask = opts.mask.is_some() && !has_base_mask;
    let preserve_level_masks = !destination_only_mask && (has_overview_mask || (has_base_mask && width <= tile && height <= tile));
    if masks.is_empty() && has_overview_mask {
        // Keep the level index aligned without introducing a base mask that would
        // override nodata or alpha validity.
        masks.push(Vec::new());
    }
    let block = gdal_overview::ovr_block(opts.source_block);
    for (i, (w, h)) in sizes.into_iter().enumerate().skip(first) {
        let level = src.read_window(crate::Window::full(w, h), &crate::WindowOptions { bands: Vec::new(), overview: Some(i) })?;
        if let Some(bits) = opts.nbits {
            let max = (1u64 << bits) - 1;
            let fits = match &level.raster.data {
                RasterData::U8(v) => v.iter().all(|&v| u64::from(v) <= max),
                RasterData::U16(v) => v.iter().all(|&v| u64::from(v) <= max),
                RasterData::U32(v) => v.iter().all(|&v| u64::from(v) <= max),
                _ => true,
            };
            if !fits {
                return Err(Error::InvalidArgument(format!(
                    "source overview {i} has values that exceed NBITS={bits}; choose a wider output NBITS"
                )));
            }
        }
        if !masks.is_empty() {
            let m = if preserve_level_masks {
                src.read_level_mask(crate::Window::full(w, h), Some(i))?.unwrap_or_default()
            } else {
                let mi = if levels.last().is_some_and(|l| l.cols > w) { levels.len() - 1 } else { 0 };
                gdal_overview::mask_level(cog_method(opts), &masks[mi], (levels[mi].cols, levels[mi].rows), (w, h), block)
            };
            masks.push(m.into_iter().map(|v| if v != 0 { 255 } else { 0 }).collect());
        }
        levels.push(Level { cols: w, rows: h, bands: raster.bands, data: std::borrow::Cow::Owned(level.raster.data) });
    }
    Ok(())
}

/// The overviews the caller supplied ([`TiffOptions::supplied_overviews`]), written as they are,
/// as [`copy_source_overviews`] writes a source's: each with the raster's bands and data type, and
/// smaller than the level before it (no larger in either dimension). GDAL's COG driver takes a
/// source's overviews at whatever sizes they have, so `gdaladdo`'s `ceil(n / factor)` sizes and
/// the COG driver's halved, floored ones both qualify. A COG's per-dataset mask gets overviews
/// generated from its full-resolution values, as GDAL 3.13.3 generates those of a mask given
/// only to the destination; a GeoTIFF with supplied overviews and a mask is refused.
fn supplied_overviews<'a>(raster: &Raster, opts: &'a TiffOptions, levels: &mut Vec<Level<'a>>, masks: &mut Vec<Vec<u8>>) -> Result<()> {
    if !masks.is_empty() && !opts.cog {
        return Err(Error::Unsupported("a GeoTIFF mask with supplied overviews is not written: write a COG, or an alpha band".into()));
    }
    let block = gdal_overview::ovr_block(opts.source_block);
    let (mut cols, mut rows) = (raster.cols, raster.rows);
    for (i, ov) in opts.supplied_overviews.iter().enumerate() {
        if ov.bands != raster.bands || ov.data_type != raster.data_type {
            return Err(Error::InvalidArgument(format!(
                "overview {i} has {} bands of {:?} and the raster {} bands of {:?}: each overview has the raster's bands and data type",
                ov.bands, ov.data_type, raster.bands, raster.data_type
            )));
        }
        if ov.cols == 0 || ov.rows == 0 || ov.cols > cols || ov.rows > rows || (ov.cols, ov.rows) == (cols, rows) {
            return Err(Error::InvalidArgument(format!(
                "overview {i} is {} x {} pixels: each overview is smaller than the level before it ({cols} x {rows})",
                ov.cols, ov.rows
            )));
        }
        if ov.data.len() != ov.cols * ov.rows * ov.bands {
            return Err(Error::InvalidArgument(format!("overview {i} has {} values for {} x {} pixels", ov.data.len(), ov.cols, ov.rows)));
        }
        if let Some(bits) = opts.nbits {
            let max = (1u64 << bits) - 1;
            let fits = match &ov.data {
                RasterData::U8(v) => v.iter().all(|&v| u64::from(v) <= max),
                RasterData::U16(v) => v.iter().all(|&v| u64::from(v) <= max),
                RasterData::U32(v) => v.iter().all(|&v| u64::from(v) <= max),
                _ => true,
            };
            if !fits {
                return Err(Error::InvalidArgument(format!("overview {i} has values that exceed NBITS={bits}")));
            }
        }
        if !masks.is_empty() {
            // GDAL 3.13.3's GDALRegenerateOverviewsMultiBand cascades only when the previous
            // width is larger. A height-only reduction regenerates the mask from the base.
            let mi = if levels.last().is_some_and(|l| l.cols > ov.cols) { levels.len() - 1 } else { 0 };
            let m = gdal_overview::mask_level(cog_method(opts), &masks[mi], (levels[mi].cols, levels[mi].rows), (ov.cols, ov.rows), block);
            masks.push(m.into_iter().map(|v| if v != 0 { 255 } else { 0 }).collect());
        }
        levels.push(Level::from_raster(ov));
        (cols, rows) = (ov.cols, ov.rows);
    }
    Ok(())
}

/// The next overview of the per-dataset mask, computed from level `mi` (its bits `masks[mi]`, or
/// the values it was computed with, `raw_masks[mi - 1]`) of size `from`, as GDAL's COG and GTiff
/// drivers regenerate a mask band's overviews (`GDALRegenerateOverviewsMultiBand`, which weights
/// each level by the source band's mask band, or by the band itself when it is a mask band).
///
/// A mask with a dataset (a file's internal mask, or `.msk` sidecar) is a mask band at every
/// level: the temporary overviews carry `INTERNAL_MASK_FLAGS_1` (`GTIFFBuildOverviewMetadata`),
/// so each level is weighted by its own values. A mask without a dataset, as GDAL 3.12.1's
/// `MEMRasterBand::CreateMaskBand` creates it (`opts.mask_without_dataset`: rasterio holds a COG
/// it writes in a MEM dataset), gets no such metadata (`GTIFFBuildOverviewsEx` writes it only
/// for a band with a dataset): its first overview is weighted by the mask's own values, and the
/// later ones, computed from the temporary Byte overviews, by their all-valid mask.
#[allow(clippy::too_many_arguments)]
fn mask_levels(
    opts: &TiffOptions,
    method: Method,
    masks: &[Vec<u8>],
    raw_masks: &[Vec<u8>],
    mi: usize,
    from: (usize, usize),
    to: (usize, usize),
    block: usize,
) -> Vec<u8> {
    if !opts.mask_without_dataset || mi == 0 {
        return gdal_overview::mask_level(method, &masks[mi], from, to, block);
    }
    let src = &raw_masks[mi - 1];
    let all_valid = vec![255u8; src.len()];
    let plan = gdal_overview::Plan::new(method, from, to, 1, block, DataType::U8);
    let weights = gdal_overview::Weights { alpha: None, alpha_masks: false, mask: Some(&all_valid) };
    match gdal_overview::level(&plan, &RasterData::U8(src.clone()), 1, None, weights) {
        RasterData::U8(v) => v,
        _ => unreachable!("a Byte level"),
    }
}

/// The alpha band (the last, with `ALPHA` or an `Alpha` color interpretation), which GDAL's
/// overviews weight by itself (`IsMaskBand`), and whether it is the other bands' mask band too
/// (two or four bands, Byte or UInt16: `GDALRasterBand::GetMaskBand`).
fn alpha_weights(opts: &TiffOptions, bands: usize, t: DataType) -> (Option<usize>, bool) {
    // PHOTOMETRIC=RGB given makes the first three bands red, green and blue whatever their source
    // interpretations (see `plan`), so a third band read as Alpha in the source is blue here.
    let rgb_band = bands <= 3 && opts.photometric == Some(2);
    let last_is_alpha = !rgb_band
        && opts
            .band_metadata
            .get(bands.wrapping_sub(1))
            .and_then(|b| b.color_interp.as_deref())
            .is_some_and(|c| c.eq_ignore_ascii_case("alpha"));
    let alpha = (opts.alpha || last_is_alpha).then(|| bands - 1);
    (alpha, alpha.is_some() && matches!(bands, 2 | 4) && matches!(t, DataType::U8 | DataType::U16))
}

/// GDAL's internal mask (`GDAL_TIFF_INTERNAL_MASK`): a 1-bit transparency mask image
/// (`NewSubfileType` 4, `PhotometricInterpretation` 4) in the blocks of the image it masks
/// (`block`: width, height, tiled), Deflate compressed, bits set for valid pixels.
pub(crate) fn mask_image(mask: &[u8], cols: usize, rows: usize, block: (usize, usize, bool)) -> Result<Image> {
    let compression = Compression::Deflate;
    let (chunk_w, chunk_h, tiled) = block;
    let (across, down) = (cols.div_ceil(chunk_w), rows.div_ceil(chunk_h));
    let row_bytes = chunk_w.div_ceil(8);
    let chunks = crate::par::map_range_coarse(across * down, |j| {
        let (x0, y0) = ((j % across) * chunk_w, (j / across) * chunk_h);
        let h = if tiled { chunk_h } else { chunk_h.min(rows - y0) };
        let mut buf = vec![0u8; row_bytes * h];
        for r in 0..h {
            let y = y0 + r;
            if y >= rows {
                break;
            }
            for c in 0..chunk_w {
                let x = x0 + c;
                if x < cols && mask[y * cols + x] != 0 {
                    buf[r * row_bytes + c / 8] |= 0x80 >> (c % 8);
                }
            }
        }
        let valid = (chunk_w.min(cols - x0), h.min(rows - y0));
        compression.compress(buf, &ChunkInfo { w: chunk_w, h, spp: 1, t: DataType::U8, valid }, &CodecParams::default())
    });
    let chunks = chunks.into_iter().collect::<Result<Vec<_>>>()?;
    let mut tags = vec![
        (254, Value::Longs(vec![4])),
        (256, Value::Longs(vec![cols as u32])),
        (257, Value::Longs(vec![rows as u32])),
        (258, Value::Shorts(vec![1])),
        (259, Value::Shorts(vec![compression.code()])),
        (262, Value::Shorts(vec![4])),
        (277, Value::Shorts(vec![1])),
        (284, Value::Shorts(vec![1])),
    ];
    if tiled {
        tags.push((322, Value::Longs(vec![chunk_w as u32])));
        tags.push((323, Value::Longs(vec![chunk_h as u32])));
    } else {
        tags.push((278, Value::Longs(vec![chunk_h as u32])));
    }
    tags.sort_by_key(|(t, _)| *t);
    Ok(Image { tags, chunks, tiled, clipped_bits: None })
}

/// The `ColorMap` tag of a color table: every red value, then green, then blue, as 16 bits.
fn color_map_tag(table: &[[u8; 4]], bits: u16) -> Value {
    let n = 1usize << bits;
    let mut v = vec![0u16; 3 * n];
    for (i, e) in table.iter().take(n).enumerate() {
        for c in 0..3 {
            v[c * n + i] = e[c] as u16 * 257;
        }
    }
    Value::Shorts(v)
}

/// How the pixels are laid out in the file.
#[derive(Clone)]
pub(crate) struct Layout {
    /// Tile width and height; strips when `None`.
    pub(crate) tile: Option<(u32, u32)>,
    pub(crate) rows_per_strip: Option<u32>,
    /// One plane per band.
    pub(crate) planar: bool,
    pub(crate) photometric: u16,
    /// The `ExtraSamples` values of the samples besides the color ones.
    pub(crate) extra: Vec<u16>,
    /// The TIFF predictor.
    pub(crate) predictor: u16,
    /// The bands per TIFF sample: 2 for complex samples (real and imaginary parts), else 1.
    pub(crate) parts: usize,
    pub(crate) nbits: Option<u16>,
}

/// One resolution level's pixels, band-major: the raster's own, or an overview's.
pub(crate) struct Level<'a> {
    pub(crate) cols: usize,
    pub(crate) rows: usize,
    pub(crate) bands: usize,
    pub(crate) data: std::borrow::Cow<'a, RasterData>,
}

impl<'a> Level<'a> {
    pub(crate) fn from_raster(r: &'a Raster) -> Self {
        Self { cols: r.cols, rows: r.rows, bands: r.bands, data: std::borrow::Cow::Borrowed(&r.data) }
    }

    /// Resamples to `cols` x `rows` pixels with terrio's `min` or `max`.
    fn resample(&self, cols: usize, rows: usize, method: OverviewResampling, nodata: Option<f64>) -> Level<'static> {
        let src = (self.cols, self.rows, self.bands);
        let data = map_data!(self.data.as_ref(), v => resample(v, src, cols, rows, method, nodata, nodata.unwrap_or(0.0)));
        Level { cols, rows, bands: self.bands, data: std::borrow::Cow::Owned(data) }
    }

    /// Splits the level into compressed strips or tiles, with its structural tags.
    pub(crate) fn image(&self, t: DataType, compression: Compression, codec: &CodecParams, layout: &Layout) -> Result<Image> {
        self.sparse_image(t, compression, codec, layout, None)
    }

    /// As [`Level::image`], leaving out the chunks whose samples within the level are all
    /// `empty` (nodata, or 0), bit for bit, as empty chunks: a chunk left out of a file reads as
    /// that value. `empty` is ignored when the samples cannot hold it exactly (an integer type
    /// and a fraction or a value out of its range).
    pub(crate) fn sparse_image(
        &self,
        t: DataType,
        compression: Compression,
        codec: &CodecParams,
        layout: &Layout,
        empty: Option<f64>,
    ) -> Result<Image> {
        let codec = &CodecParams { jpeg_ycbcr: layout.photometric == 6, ..*codec };
        let parts = layout.parts;
        // The size of a sample's part, and the TIFF samples per pixel.
        let size = t.size_bytes();
        let spp = self.bands / parts;
        // Samples per chunk (each of `parts` values).
        let chunk_spp = if layout.planar { 1 } else { spp };
        let planes = if layout.planar { spp } else { 1 };
        let (chunk_w, chunk_h, across, down, tiled) = match layout.tile {
            Some((tw, th)) => {
                let (tw, th) = (tw as usize, th as usize);
                (tw, th, self.cols.div_ceil(tw), self.rows.div_ceil(th), true)
            }
            None => {
                let rows_per_strip = strip_rows(layout.rows_per_strip, self.rows, self.cols * chunk_spp * parts * size, compression);
                (self.cols, rows_per_strip, 1, self.rows.div_ceil(rows_per_strip), false)
            }
        };
        let per_plane = across * down;
        let band_len = self.cols * self.rows;
        let clipped = std::sync::atomic::AtomicBool::new(false);
        let packed_bits = layout.nbits.filter(|&bits| bits != (size * 8) as u16);
        let empty = empty.filter(|&v| representable(t, v));
        let chunks = crate::par::map_range_init_coarse(
            planes * per_plane,
            || None,
            |scratch, i| {
                let (plane, j) = (i / per_plane, i % per_plane);
                let (x0, y0) = ((j % across) * chunk_w, (j / across) * chunk_h);
                // Strips stop at the last row; tiles are padded to full size.
                let h = if tiled { chunk_h } else { chunk_h.min(self.rows - y0) };
                if let Some(v) = empty {
                    let (first, samples) = if layout.planar { (plane * parts * band_len, parts) } else { (0, spp * parts) };
                    let data = self.data.as_ref();
                    if with_data!(data, d => all_samples_equal(&d[first..][..samples * band_len], self.cols, self.rows, x0, y0, chunk_w, h, v))
                    {
                        return Ok(Vec::new());
                    }
                }
                // A complex sample's parts are interleaved like the bands of a pixel.
                let mut buf = if layout.planar {
                    let plane_len = parts * band_len;
                    with_data!(self.data.as_ref(), v => chunk_bytes(&v[plane * plane_len..(plane + 1) * plane_len], self.cols, self.rows, parts, x0, y0, chunk_w, h))
                } else {
                    with_data!(self.data.as_ref(), v => chunk_bytes(v, self.cols, self.rows, spp * parts, x0, y0, chunk_w, h))
                };
                if let Some(bits) = packed_bits {
                    let (packed, did_clip) = pack_unsigned_rows(&buf, chunk_w * chunk_spp, size, bits);
                    buf = packed;
                    // GDAL 3.13.3 silently clamps bilevel samples; wider packed domains warn.
                    if did_clip && bits > 1 {
                        clipped.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                } else {
                    apply_predictor(&mut buf, layout.predictor, chunk_w * chunk_spp, chunk_spp, size);
                }
                let valid = (chunk_w.min(self.cols - x0), h.min(self.rows - y0));
                compression.compress_reusing(buf, &ChunkInfo { w: chunk_w, h, spp: chunk_spp, t, valid }, codec, scratch)
            },
        );
        let chunks = chunks.into_iter().collect::<Result<Vec<_>>>()?;
        let clipped_bits = packed_bits.filter(|_| clipped.load(std::sync::atomic::Ordering::Relaxed));
        let (bits, format) = sample_format(t);
        let bits = layout.nbits.unwrap_or(bits);
        // Complex samples: twice the bits, `SampleFormat` 5 (integers) or 6 (floats).
        let (bits, format) = if parts == 2 { (bits * 2, format + 3) } else { (bits, format) };
        let mut tags = vec![
            (256, Value::Longs(vec![self.cols as u32])),
            (257, Value::Longs(vec![self.rows as u32])),
            (258, Value::Shorts(vec![bits; spp])),
            (259, Value::Shorts(vec![compression.code()])),
            (262, Value::Shorts(vec![layout.photometric])),
            (277, Value::Shorts(vec![spp as u16])),
            (284, Value::Shorts(vec![if layout.planar { 2 } else { 1 }])),
            (339, Value::Shorts(vec![format; spp])),
        ];
        if !layout.extra.is_empty() {
            tags.push((338, Value::Shorts(layout.extra.clone())));
        }
        if layout.predictor > 1 {
            tags.push((317, Value::Shorts(vec![layout.predictor])));
        }
        if layout.photometric == 6 {
            // YCbCrSubSampling: the JPEG streams' 4:2:0.
            tags.push((530, Value::Shorts(vec![2, 2])));
        }
        // LercParameters: LERC 2.4, and the compression over its blobs (none, Deflate, Zstandard).
        match compression {
            Compression::Lerc => tags.push((50674, Value::Longs(vec![4, 0]))),
            Compression::LercDeflate => tags.push((50674, Value::Longs(vec![4, 1]))),
            Compression::LercZstd => tags.push((50674, Value::Longs(vec![4, 2]))),
            _ => {}
        }
        if tiled {
            tags.push((322, Value::Longs(vec![chunk_w as u32])));
            tags.push((323, Value::Longs(vec![chunk_h as u32])));
        } else {
            tags.push((278, Value::Longs(vec![chunk_h as u32])));
        }
        Ok(Image { tags, chunks, tiled, clipped_bits })
    }
}

/// Applies a TIFF predictor to the little-endian rows of a chunk, as libtiff does: 2 differences
/// each sample with the previous pixel's (as unsigned integers of the sample size, wrapping); 3
/// splits each row's samples into byte planes, most significant first, then differences each
/// byte with the one `stride` bytes before.
fn apply_predictor(buf: &mut [u8], predictor: u16, row_samples: usize, stride: usize, size: usize) {
    let row_len = row_samples * size;
    if predictor < 2 || row_len == 0 {
        return;
    }
    for row in buf.chunks_exact_mut(row_len) {
        if predictor == 2 {
            macro_rules! diff {
                ($t:ty) => {{
                    const N: usize = std::mem::size_of::<$t>();
                    for i in (stride..row_samples).rev() {
                        let cur = <$t>::from_le_bytes(row[i * N..(i + 1) * N].try_into().unwrap_or_default());
                        let prev = <$t>::from_le_bytes(row[(i - stride) * N..(i - stride + 1) * N].try_into().unwrap_or_default());
                        row[i * N..(i + 1) * N].copy_from_slice(&cur.wrapping_sub(prev).to_le_bytes());
                    }
                }};
            }
            match size {
                1 => diff!(u8),
                2 => diff!(u16),
                4 => diff!(u32),
                _ => diff!(u64),
            }
        } else {
            let tmp = row.to_vec();
            for i in 0..row_samples {
                for b in 0..size {
                    // Little-endian bytes: byte `b` is the (size - 1 - b)th most significant.
                    row[(size - b - 1) * row_samples + i] = tmp[size * i + b];
                }
            }
            for i in (stride..row_len).rev() {
                row[i] = row[i].wrapping_sub(row[i - stride]);
            }
        }
    }
}

/// Resamples a band-major image to `out_cols` x `out_rows` with terrio's `min` or `max`: the
/// extreme of the valid pixels of each output pixel's source window (GDAL's methods are
/// [`gdal_overview`]'s).
fn resample<T: Pixel>(
    src: &[T],
    (cols, rows, bands): (usize, usize, usize),
    out_cols: usize,
    out_rows: usize,
    method: OverviewResampling,
    nodata: Option<f64>,
    fill: f64,
) -> Vec<T> {
    let (rx, ry) = (cols as f64 / out_cols as f64, rows as f64 / out_rows as f64);
    // Source windows: (first, end, first weight, last weight).
    let window = |i: usize, ratio: f64, n: usize| -> (usize, usize, f64, f64) {
        let (f0, f1) = (i as f64 * ratio, (i + 1) as f64 * ratio);
        let a = ((f0 + 1e-8) as usize).min(n - 1);
        let mut b = (f1 - 1e-8).ceil() as usize;
        if b <= a {
            b = a + 1;
        }
        let b = b.min(n);
        let wa = if b == a + 1 { 1.0 } else { 1.0 - (f0 - a as f64) };
        let wb = 1.0 - (b as f64 - f1);
        (a, b, wa, wb)
    };
    let xs: Vec<_> = (0..out_cols).map(|i| window(i, rx, cols)).collect();
    let mut out = vec![T::default(); out_cols * out_rows * bands];
    crate::par::chunks_mut_coarse(&mut out, out_cols, |k, line| {
        let (b, r) = (k / out_rows, k % out_rows);
        let band = &src[b * cols * rows..(b + 1) * cols * rows];
        let (y0, y1, _, _) = window(r, ry, rows);
        for (c, o) in line.iter_mut().enumerate() {
            let (x0, x1, _, _) = xs[c];
            let mut extreme: Option<f64> = None;
            for y in y0..y1 {
                for x in x0..x1 {
                    let v = band[y * cols + x].to_f64();
                    if v.is_nan() || nodata.is_some_and(|nd| v == nd) {
                        continue;
                    }
                    extreme = Some(match (extreme, method) {
                        (None, _) => v,
                        (Some(e), OverviewResampling::Min) => e.min(v),
                        (Some(e), _) => e.max(v),
                    });
                }
            }
            *o = T::from_f64(extreme.unwrap_or(fill));
        }
    });
    out
}

/// Whether samples of type `t` hold `v` exactly: any value for floating-point samples (`F32`
/// rounds it to single precision), an integer within the type's range otherwise.
fn representable(t: DataType, v: f64) -> bool {
    let (lo, hi): (i128, i128) = match t {
        DataType::F32 | DataType::F64 => return true,
        DataType::U8 => (0, u8::MAX.into()),
        DataType::I8 => (i8::MIN.into(), i8::MAX.into()),
        DataType::U16 => (0, u16::MAX.into()),
        DataType::I16 => (i16::MIN.into(), i16::MAX.into()),
        DataType::U32 => (0, u32::MAX.into()),
        DataType::I32 => (i32::MIN.into(), i32::MAX.into()),
        DataType::U64 => (0, u64::MAX.into()),
        DataType::I64 => (i64::MIN.into(), i64::MAX.into()),
    };
    v.fract() == 0.0 && (lo..=hi).contains(&(v as i128))
}

/// Whether the samples of every band of band-major `data` (`cols` x `rows` pixels a band) in the
/// chunk of `chunk_w` x `h` pixels at `(x0, y0)`, within the image, are all `v`, bit for bit.
#[allow(clippy::too_many_arguments)]
fn all_samples_equal<T: Pixel>(data: &[T], cols: usize, rows: usize, x0: usize, y0: usize, chunk_w: usize, h: usize, v: f64) -> bool {
    let (mut want, mut got) = ([0u8; 8], [0u8; 8]);
    T::from_f64(v).put_le(&mut want[..T::SIZE]);
    let n = chunk_w.min(cols - x0);
    data.chunks_exact(cols * rows).all(|band| {
        (y0..rows.min(y0 + h)).all(|y| {
            band[y * cols + x0..][..n].iter().all(|s| {
                s.put_le(&mut got[..T::SIZE]);
                got == want
            })
        })
    })
}

/// The pixel-interleaved little-endian bytes of a chunk of `chunk_w` x `h` pixels at `(x0, y0)`,
/// padded with zeros past the image's edges.
#[allow(clippy::too_many_arguments)]
fn chunk_bytes<T: Pixel>(data: &[T], cols: usize, rows: usize, spp: usize, x0: usize, y0: usize, chunk_w: usize, h: usize) -> Vec<u8> {
    let size = T::SIZE;
    let row_len = chunk_w * spp * size;
    let mut buf = vec![0u8; row_len * h];
    let band_len = cols * rows;
    let n = chunk_w.min(cols - x0);
    for row in 0..h.min(rows.saturating_sub(y0)) {
        let dst = &mut buf[row * row_len..][..n * spp * size];
        let src = (y0 + row) * cols + x0;
        if spp == 1 {
            for (d, v) in dst.chunks_exact_mut(size).zip(&data[src..src + n]) {
                v.put_le(d);
            }
        } else {
            for b in 0..spp {
                let band = &data[b * band_len + src..][..n];
                for (c, v) in band.iter().enumerate() {
                    v.put_le(&mut dst[(c * spp + b) * size..][..size]);
                }
            }
        }
    }
    buf
}

/// Pack unsigned little-endian samples MSB first, padding each row to a byte boundary.
/// GDAL 3.13.3 clips values to the stored unsigned domain, rather than masking high bits.
fn pack_unsigned_rows(input: &[u8], row_samples: usize, size: usize, bits: u16) -> (Vec<u8>, bool) {
    let bits = usize::from(bits);
    let maximum = (1u64 << bits) - 1;
    // Packed rows fit in the existing native row; avoid overflowing a bit-count intermediate.
    let row_bytes = row_samples / 8 * bits + (row_samples % 8 * bits).div_ceil(8);
    let mut output = Vec::with_capacity(row_bytes * (input.len() / (row_samples * size)));
    let mut clipped = false;
    for row in input.chunks_exact(row_samples * size) {
        let (mut accumulator, mut pending) = (0u64, 0usize);
        for sample in row.chunks_exact(size) {
            let value = match size {
                1 => u64::from(sample[0]),
                2 => u64::from(u16::from_le_bytes(sample.try_into().unwrap())),
                4 => u64::from(u32::from_le_bytes(sample.try_into().unwrap())),
                _ => unreachable!("packed samples have 1, 2 or 4 byte containers"),
            };
            clipped |= value > maximum;
            accumulator = (accumulator << bits) | value.min(maximum);
            pending += bits;
            while pending >= 8 {
                pending -= 8;
                output.push((accumulator >> pending) as u8);
            }
            accumulator &= (1u64 << pending) - 1;
        }
        if pending != 0 {
            output.push((accumulator << (8 - pending)) as u8);
        }
    }
    (output, clipped)
}

fn sample_format(t: DataType) -> (u16, u16) {
    let bits = (t.size_bytes() * 8) as u16;
    let format = match t {
        DataType::U8 | DataType::U16 | DataType::U32 | DataType::U64 => 1,
        DataType::I8 | DataType::I16 | DataType::I32 | DataType::I64 => 2,
        DataType::F32 | DataType::F64 => 3,
    };
    (bits, format)
}

/// Appends a value as a little-endian sample of type `t`, rounding and clamping integers.
pub(crate) fn encode_sample_le(t: DataType, v: f64, out: &mut Vec<u8>) {
    // `as` saturates float-to-int conversions and maps NaN to 0.
    match t {
        DataType::U8 => out.push(v.round() as u8),
        DataType::I8 => out.push(v.round() as i8 as u8),
        DataType::U16 => out.extend((v.round() as u16).to_le_bytes()),
        DataType::I16 => out.extend((v.round() as i16).to_le_bytes()),
        DataType::U32 => out.extend((v.round() as u32).to_le_bytes()),
        DataType::I32 => out.extend((v.round() as i32).to_le_bytes()),
        DataType::U64 => out.extend((v.round() as u64).to_le_bytes()),
        DataType::I64 => out.extend((v.round() as i64).to_le_bytes()),
        DataType::F32 => out.extend((v as f32).to_le_bytes()),
        DataType::F64 => out.extend(v.to_le_bytes()),
    }
}

/// Whether the raster declares point samples (in its metadata items as written).
fn pixel_is_point(r: &Raster, opts: &TiffOptions) -> bool {
    dataset_items(r, opts).iter().any(|(k, v)| k == "AREA_OR_POINT" && v.eq_ignore_ascii_case("Point"))
}

/// The dataset's default-domain metadata items as written: the raster's own, with those of
/// `opts.metadata` set over them in their place, or after them when new.
fn dataset_items<'a>(r: &'a Raster, opts: &TiffOptions) -> std::borrow::Cow<'a, [(String, String)]> {
    if opts.metadata.is_empty() {
        return std::borrow::Cow::Borrowed(&r.metadata);
    }
    let mut items = r.metadata.clone();
    for (k, v) in &opts.metadata {
        match items.iter_mut().find(|(key, _)| key == k) {
            Some(item) => item.1.clone_from(v),
            None => items.push((k.clone(), v.clone())),
        }
    }
    std::borrow::Cow::Owned(items)
}

/// The georeferencing tags: pixel scale and tie point, GeoKeys, nodata and metadata.
/// The colour interpretation GDAL gives band `i` (from 0) of a TIFF of the photometric
/// interpretation `photometric`, whose samples past the first `base` are the extra samples
/// `extra` (`GTiffRasterBand`'s, as [`crate::geotiff`] reads them).
fn implied_color_interp(photometric: u16, extra: &[u16], base: usize, i: usize) -> &'static str {
    match (photometric, i) {
        (0 | 1, 0) => "Gray",
        (2 | 6, 0) => "Red",
        (2 | 6, 1) => "Green",
        (2 | 6, 2) => "Blue",
        (3, 0) => "Palette",
        _ if i >= base && extra.get(i - base).is_some_and(|e| matches!(e, 1 | 2)) => "Alpha",
        _ => "Undefined",
    }
}

/// The GeoTIFF tags of a raster written with `opts`. `ignored(band, interpretation)` says
/// whether a band's colour interpretation is left out: one the written photometric and extra
/// samples give the band, as GDAL's `GTiffDataset::WriteMetadata` writes only the bands' other
/// colour interpretations to GDAL_METADATA (GDAL 3.12.1 copies a gray or RGB file without it, and
/// three gray Byte bands, written as RGB, with theirs), or one the photometric given overrides.
fn geo_tags(
    r: &Raster,
    opts: &TiffOptions,
    image_structure: &[(String, String)],
    ignored: impl Fn(usize, &str) -> bool,
) -> Result<Vec<(u16, Value)>> {
    let mut tags = vec![
        (33550, Value::Doubles(vec![r.cell_size_x, r.cell_size_y, 0.0])),
        (33922, Value::Doubles(vec![0.0, 0.0, 0.0, r.x_min, r.y_max(), 0.0])),
    ];
    let crs = super::common::spatial_ref(&r.crs);
    let mut v = crate::crs::geokeys::GeoKeyValues::default();
    let point = pixel_is_point(r, opts);
    v.shorts.push((1025, if point { 2 } else { 1 }));
    if crs.is_unknown() && point {
        return Err(Error::Unsupported(
            "PixelIsPoint GeoTIFF writing requires a CRS; a lone raster-type GeoKey would imply a local CRS".into(),
        ));
    }
    if !crs.is_unknown() {
        // As GDAL 3.12.1 writes them (`GTIFSetFromOGISDefnEx`): see `SpatialRef::to_geokeys`.
        let keys = crs.to_geokeys()?;
        v.shorts.extend(keys.shorts);
        v.doubles.extend(keys.doubles);
        v.ascii.extend(keys.ascii);
    }
    let crate::crs::geokeys::GeoKeyTags { directory: dir, doubles, ascii, .. } = v.encode()?;
    // Without a CRS, no GeoKeys: GDAL reads a lone raster-type key as a local CRS.
    if !crs.is_unknown() {
        tags.push((34735, Value::Shorts(dir)));
    }
    if !doubles.is_empty() {
        tags.push((34736, Value::Doubles(doubles)));
    }
    if !ascii.is_empty() {
        tags.push((34737, Value::Ascii(ascii)));
    }
    let bands: Vec<crate::BandMetadata> = opts
        .band_metadata
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let mut b = b.clone();
            if b.color_interp.as_deref().is_some_and(|c| ignored(i, c)) {
                b.color_interp = None;
            }
            b
        })
        .collect();
    // COPY_SRC_MDD filters source items, not metadata explicitly supplied by the caller.
    let items = if opts.drop_default_items.0 {
        // Explicit overrides have the same last-value-wins policy as dataset_items.
        let mut items: Vec<(String, String)> = Vec::new();
        for (key, value) in &opts.metadata {
            if let Some(item) = items.iter_mut().find(|(name, _)| name == key) {
                item.1.clone_from(value);
            } else {
                items.push((key.clone(), value.clone()));
            }
        }
        std::borrow::Cow::Owned(items)
    } else {
        dataset_items(r, opts)
    };
    if let Some(xml) = gdal_metadata_xml(&items, &bands, &opts.domains, (false, opts.drop_default_items.1), image_structure) {
        tags.push((42112, Value::Ascii(xml)));
    }
    if let Some(nd) = opts.nodata {
        tags.push((42113, Value::Ascii(format_nodata_for(nd, r.data_type))));
    }
    Ok(tags)
}

/// The `GDAL_METADATA` document as GDAL writes it (`GTiffDataset::WriteMetadata`): the dataset's
/// items (default domain, then the other domains), each band's (default domain, other domains,
/// then OFFSET, SCALE, UNITTYPE, DESCRIPTION and COLORINTERP) and the IMAGE_STRUCTURE items, values
/// escaped twice. `None` when there is nothing to write.
fn gdal_metadata_xml(
    dataset: &[(String, String)],
    bands: &[crate::BandMetadata],
    domains: &[MetadataDomain],
    drop_default_items: (bool, bool),
    image_structure: &[(String, String)],
) -> Option<String> {
    let mut xml = String::from("<GDALMetadata>\n");
    let mut any = false;
    let mut item = |name: &str, value: &str, sample: Option<usize>, role: Option<&str>, domain: Option<&str>| {
        let mut attrs = format!("name=\"{}\"", xml_escape(name));
        if let Some(s) = sample {
            attrs.push_str(&format!(" sample=\"{s}\""));
        }
        if let Some(r) = role {
            attrs.push_str(&format!(" role=\"{r}\""));
        }
        if let Some(d) = domain {
            attrs.push_str(&format!(" domain=\"{}\"", xml_escape(d)));
        }
        xml.push_str(&format!("  <Item {attrs}>{}</Item>\n", xml_escape(&xml_escape(value))));
        any = true;
    };
    // WriteMDMetadata: the items of an xml: or json: domain are documents, written as "doc".
    fn domain_items(domains: &[MetadataDomain], band: usize, item: &mut impl FnMut(&str, &str, Option<usize>, Option<&str>, Option<&str>)) {
        for d in domains.iter().filter(|d| d.band == band) {
            let doc = ["xml:", "json:"].iter().any(|p| d.name.get(..p.len()).is_some_and(|s| s.eq_ignore_ascii_case(p)));
            for (k, v) in &d.items {
                if doc {
                    // A document read back (terrio names it `doc`, rasterio after its domain), or
                    // an item `update_tags` set, a `name=value` string to GDAL.
                    let value = if k == "doc" || *k == d.name { v.clone() } else { format!("{k}={v}") };
                    item("doc", &value, band.checked_sub(1), None, Some(&d.name));
                } else {
                    item(k, v, band.checked_sub(1), None, Some(&d.name));
                }
            }
        }
    }
    if !drop_default_items.0 {
        for (k, v) in dataset {
            // Written as the GeoKey RasterPixelIsPoint, as GDAL writes it.
            if k == "AREA_OR_POINT" {
                continue;
            }
            item(k, v, None, None, None);
        }
    }
    domain_items(domains, 0, &mut item);
    let count = bands.len().max(domains.iter().map(|d| d.band).max().unwrap_or(0));
    for i in 0..count {
        let b = bands.get(i);
        if !drop_default_items.1 {
            for (k, v) in b.map(|b| b.items.as_slice()).unwrap_or_default() {
                item(k, v, Some(i), None, None);
            }
        }
        domain_items(domains, i + 1, &mut item);
        let Some(b) = b else { continue };
        if let Some(v) = b.offset {
            item("OFFSET", &format!("{v}"), Some(i), Some("offset"), None);
        }
        if let Some(v) = b.scale {
            item("SCALE", &format!("{v}"), Some(i), Some("scale"), None);
        }
        if let Some(v) = &b.unit {
            item("UNITTYPE", v, Some(i), Some("unittype"), None);
        }
        if let Some(v) = &b.description {
            item("DESCRIPTION", v, Some(i), Some("description"), None);
        }
        if let Some(v) = &b.color_interp {
            item("COLORINTERP", v, Some(i), Some("colorinterp"), None);
        }
    }
    for (k, v) in image_structure {
        item(k, v, None, None, Some("IMAGE_STRUCTURE"));
    }
    xml.push_str("</GDALMetadata>");
    any.then_some(xml)
}

/// Whether overview images carry the `GDAL_NODATA` tag: GDAL 3.12.1 writes it on the overviews it
/// builds only from a nodata held as a double (`GTiffDataset::GetOverviewParameters` reads
/// `m_bNoDataSet`), not from the Int64 and UInt64 nodata it holds apart
/// (`m_bNoDataSetAsInt64`, `m_bNoDataSetAsUInt64`). GDAL then masks no overview pixel of such a
/// raster by value.
fn overview_nodata(t: DataType) -> bool {
    !matches!(t, DataType::I64 | DataType::U64)
}

/// Formats a nodata value as GDAL writes it.
pub(crate) fn format_nodata(nd: f64) -> String {
    if nd.is_nan() {
        "nan".into()
    } else if nd.is_infinite() {
        if nd > 0.0 { "inf".into() } else { "-inf".into() }
    } else {
        // Shortest representation that round-trips.
        format!("{nd}")
    }
}

/// Formats the `GDAL_NODATA` of samples of type `t` as GDAL writes it: the nodata of Int64 and
/// UInt64 samples that the type holds exactly as an integer (`GTiffDataset::WriteNoDataValue` for
/// `int64_t` and `uint64_t`, GDAL 3.12.1), which GDAL reads back with `strtoll` or `strtoull`;
/// other values as [`format_nodata`]. A large value printed as a double (`9223372036854776000`
/// for 2^63) reads back as another integer.
pub(crate) fn format_nodata_for(nd: f64, t: DataType) -> String {
    // `GDALIsValueExactAs`: within [-2^63, 2^63) or [0, 2^64), and integral.
    match t {
        DataType::I64 if (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&nd) && nd.fract() == 0.0 => {
            (nd as i64).to_string()
        }
        DataType::U64 if (0.0..18_446_744_073_709_551_616.0).contains(&nd) && nd.fract() == 0.0 => (nd as u64).to_string(),
        _ => format_nodata(nd),
    }
}

/// Shared XML parsing for dataset metadata, band metadata and descriptive roles.
pub(crate) fn gdal_metadata_items(xml: &str) -> Vec<crate::drivers::vector::xml::Element> {
    crate::drivers::vector::xml::parse(xml)
        .map(|root| root.children.into_iter().filter(|item| item.name == "Item").collect())
        .unwrap_or_default()
}

/// The dataset-level items of a `GDAL_METADATA` XML document (band items are skipped).
pub(crate) fn parse_gdal_metadata(xml: &str) -> Vec<(String, String)> {
    gdal_metadata_items(xml)
        .into_iter()
        .filter_map(|item| {
            if item.attr("sample").is_some() || item.attr("role").is_some() || item.attr("domain").is_some() {
                return None;
            }
            Some((item.attr("name")?.to_string(), xml_unescape(&item.text)))
        })
        .collect()
}

/// The band items of a `GDAL_METADATA` tag (default domain, no role): `(band from 0, name, value)`.
pub(crate) fn parse_gdal_band_metadata(xml: &str) -> Vec<(usize, String, String)> {
    gdal_metadata_items(xml)
        .into_iter()
        .filter_map(|item| {
            if item.attr("role").is_some() || item.attr("domain").is_some() {
                return None;
            }
            Some((item.attr("sample")?.parse().ok()?, item.attr("name")?.to_string(), xml_unescape(&item.text)))
        })
        .collect()
}

// GDAL TIFF Item values carry a second CPL-escaped layer after XML decoding.
// The writer mirrors GDAL; decoding only XML would change values and literal entities.
pub(crate) use super::common::{xml_escape, xml_unescape};

/// A GeoTIFF written a few rows at a time, so that rasters larger than memory can be written: the
/// image data first, as rows of blocks are complete, then the image's directory at the end of the
/// file (a layout GDAL writes too, and reads as any GeoTIFF). The lossless compressions,
/// predictors, tiles or strips, pixel or band interleaving, georeferencing (rotated or by GCPs),
/// metadata, color tables, RPCs and GDAL's internal mask are written as by
/// [`crate::write_raster_dataset`]; overviews and COGs need the whole raster and are not.
///
/// ```no_run
/// use terrio::{GeoTiffWriter, RasterDataset, RasterWriteOptions};
///
/// # fn header() -> RasterDataset { unimplemented!() }
/// let header: RasterDataset = header(); // size, bands, data type, georeferencing, no pixels
/// let options = RasterWriteOptions { compression: Some("DEFLATE".into()), tile_size: Some(256), ..Default::default() };
/// let mut w = GeoTiffWriter::create("big.tif", &header, &options)?;
/// # let (rows, data) = (0, wbraster::raster::RasterData::U8(vec![]));
/// w.write_rows(&data, rows, None)?; // band after band, `rows` full rows, from the top
/// w.finish()?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub struct GeoTiffWriter {
    inner: RowWriter,
}

/// How a [`GeoTiffWriter`] writes: a GeoTIFF's blocks as their rows come, or a COG's levels as
/// their rows come ([`CogWriter`], in a mutex, which makes the writer `Sync` as Python's classes
/// must be: its compression results come through channels).
enum RowWriter {
    Tiff(Box<TiffRows>),
    Cog(Box<std::sync::Mutex<CogWriter>>),
}

/// A GeoTIFF written block row by block row (see [`GeoTiffWriter`]).
struct TiffRows {
    file: std::io::BufWriter<Rewrite>,
    pos: u64,
    big: bool,
    cols: usize,
    rows: usize,
    bands: usize,
    data_type: DataType,
    compression: Compression,
    codec: CodecParams,
    layout: Layout,
    /// The block height: the tile height, or the rows per strip.
    block_rows: usize,
    chunk_w: usize,
    across: usize,
    offsets: Vec<u64>,
    counts: Vec<u64>,
    /// Rows received but not yet written (less than a block row), band after band.
    pending: RasterData,
    pending_mask: Vec<u8>,
    pending_rows: usize,
    rows_done: usize,
    /// Whether a block row failed to be written: the file is incomplete, and nothing more is
    /// written to it.
    failed: bool,
    tags: Vec<(u16, Value)>,
    /// Offsets and lengths of mask chunks streamed alongside the image's block rows.
    mask: Option<(Vec<u64>, Vec<u64>)>,
    /// The samples of the blocks to leave out (`SPARSE_OK`): nodata, or 0.
    empty: Option<f64>,
}

impl std::fmt::Debug for GeoTiffWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("GeoTiffWriter");
        match &self.inner {
            RowWriter::Tiff(t) => d.field("cols", &t.cols).field("rows", &t.rows).field("bands", &t.bands),
            RowWriter::Cog(_) => d.field("cog", &true),
        };
        d.finish_non_exhaustive()
    }
}

impl GeoTiffWriter {
    /// Creates the file. `header` describes the raster (its size, bands, data type,
    /// georeferencing, CRS, nodata and metadata); its pixels are not read. `options` are those of
    /// [`crate::write_raster_dataset`] for GeoTIFFs and COGs; for GeoTIFFs, `options.mask` asks
    /// for an internal mask, whose rows are given with the pixels'. A COG's overviews are
    /// computed as their rows come, the file the same, byte for byte, as `write_raster_dataset`'s.
    ///
    /// # Errors
    /// For explicit overview factors, a source's overviews to copy, and compressions
    /// terrio's own writer does not encode (WEBP...), which need the whole raster; when the
    /// file cannot be created; and, before it is created, [`Error::ResourceLimit`] when the rows
    /// the writer holds, a row of its blocks of every band (twice while rows are appended to
    /// it), cannot fit in the memory the process can use ([`crate::usable_memory`]).
    pub fn create(path: &str, header: &crate::RasterDataset, options: &crate::RasterWriteOptions) -> Result<Self> {
        Self::create_with_sidecar(path, header, options, None)
    }

    /// [`GeoTiffWriter::create`], for a caller that writes the GeoTIFF's `.aux.xml` once the
    /// file is finished: `sidecar` is the band nodata it then writes there, which `SPARSE_OK`
    /// is checked against instead of the sidecar beside the path (`None`: that one, kept).
    pub(crate) fn create_with_sidecar(
        path: &str,
        header: &crate::RasterDataset,
        options: &crate::RasterWriteOptions,
        sidecar: Option<&[Option<f64>]>,
    ) -> Result<Self> {
        let inner = if options.driver == Some(crate::formats::RasterDriver::Cog) {
            RowWriter::Cog(Box::new(std::sync::Mutex::new(cog_rows(path, header, options)?)))
        } else {
            RowWriter::Tiff(Box::new(TiffRows::create(path, header, options, sidecar)?))
        };
        Ok(Self { inner })
    }

    /// The number of rows given so far.
    pub fn rows_written(&self) -> usize {
        match &self.inner {
            RowWriter::Tiff(t) => t.rows_written(),
            RowWriter::Cog(c) => c.lock().unwrap_or_else(std::sync::PoisonError::into_inner).rows_written(),
        }
    }

    /// Appends `rows` full rows of every band (`data`: band after band, of the raster's data
    /// type), and of the mask (0 for invalid pixels) when the file has one.
    ///
    /// # Errors
    /// When the data is not of the raster's data type or does not hold `rows` rows of every band,
    /// rows go past the raster's height, the file has a mask and `mask` does not hold its rows, a
    /// mask is given for a file without one, or the file cannot be written. Once a block row failed
    /// to be written, or a COG's row of tiles failed to be compressed or written or its overview
    /// rows failed to be computed, the writer refuses every later call with
    /// [`Error::InvalidData`] (the call that found the failure returns its error). A GeoTIFF other
    /// than a COG otherwise checks only the data type when `rows` is zero. A COG refuses a call,
    /// before it starts, with [`Error::ResourceLimit`] when the bytes it counts for it (the most the
    /// call can have allocated at once, with or without a mask) are more than the memory the process
    /// can use ([`crate::usable_memory`]): that call takes no rows, and can be made again.
    pub fn write_rows(&mut self, data: &RasterData, rows: usize, mask: Option<&[u8]>) -> Result<()> {
        match &mut self.inner {
            RowWriter::Tiff(t) => t.write_rows(data, rows, mask),
            RowWriter::Cog(c) => {
                let writer = c.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner);
                // Checked before the writer copies the caller's rows.
                writer.admit(data, rows, mask, true)?;
                writer.take_admitted(data.clone(), rows, mask.map(<[u8]>::to_vec))
            }
        }
    }

    /// Writes the directories (and the COG's levels) and closes the file.
    ///
    /// # Errors
    /// When rows are missing, a block row of a GeoTIFF or a COG's row of tiles or overview rows
    /// failed to be written (see [`Self::write_rows`]), or the file cannot be written. A regular
    /// file is then left empty.
    pub fn finish(self) -> Result<()> {
        match self.inner {
            RowWriter::Tiff(t) => (*t).finish(),
            RowWriter::Cog(c) => c.into_inner().unwrap_or_else(std::sync::PoisonError::into_inner).finish(),
        }
    }
}

/// The COG `create` writes in rows: as `crate::write_raster_dataset` writes it, without explicit
/// overview factors, a source's overviews to copy, or a compression only the whole raster is encoded
/// with.
fn cog_rows(path: &str, header: &crate::RasterDataset, options: &crate::RasterWriteOptions) -> Result<CogWriter> {
    if !options.overviews.is_empty() {
        return Err(Error::Unsupported("COGs with explicit overview factors need the whole raster".into()));
    }
    if !options.overview_data.is_empty() {
        return Err(Error::Unsupported("COGs with supplied overviews are written whole, not in rows".into()));
    }
    let mut opts = TiffOptions::from_write_options(options, true, header.nodata())?.ok_or_else(|| {
        let name = options.compression.as_deref().unwrap_or_default();
        Error::Unsupported(format!("{name} compression needs the whole raster"))
    })?;
    if opts.source_overviews.is_some() {
        return Err(Error::Unsupported("a COG copying a source's overviews needs the whole raster".into()));
    }
    if opts.geo_transform.is_none() {
        opts.geo_transform = header.affine;
    }
    opts.complex = header.complex;
    CogWriter::create(path, &header.raster, &opts)
}

impl std::fmt::Debug for TiffRows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TiffRows").field("cols", &self.cols).field("rows", &self.rows).field("bands", &self.bands).finish_non_exhaustive()
    }
}

impl TiffRows {
    /// Creates the file. `header` describes the raster (its size, bands, data type,
    /// georeferencing, CRS, nodata and metadata); its pixels are not read. `options` are those of
    /// [`crate::write_raster_dataset`] for GeoTIFFs; `options.mask` is ignored: masks are given
    /// with the rows. `sidecar`: the band nodata the caller writes to the `.aux.xml` once the
    /// file is finished; `None` when it keeps the sidecar beside the path.
    ///
    /// # Errors
    /// For COGs, overviews and compressions terrio's own writer does not encode (WEBP...), which
    /// need the whole raster, and when the file cannot be created.
    pub fn create(
        path: &str,
        header: &crate::RasterDataset,
        options: &crate::RasterWriteOptions,
        sidecar: Option<&[Option<f64>]>,
    ) -> Result<Self> {
        if matches!(options.driver, Some(d) if d != crate::formats::RasterDriver::GTiff) {
            return Err(Error::Unsupported("rasters are written in rows as GeoTIFFs only".into()));
        }
        if !options.overviews.is_empty() || !options.overview_data.is_empty() {
            return Err(Error::Unsupported("overviews need the whole raster: build them after writing".into()));
        }
        let mut options = options.inherited_from(header, Some(crate::formats::RasterDriver::GTiff));
        let with_mask = options.mask.take().is_some();
        let mut opts = TiffOptions::from_write_options(&options, false, crate::dataset::tiff_nodata(header))?.ok_or_else(|| {
            let name = options.compression.as_deref().unwrap_or_default();
            Error::Unsupported(format!("{name} compression needs the whole raster"))
        })?;
        opts.complex = header.complex;
        // `terrio.rio` writes beside a temporary name, then moves the file to `@FINAL_PATH`.
        let final_path =
            options.creation_options.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case("@FINAL_PATH")).map_or(path, |(_, v)| v.as_str());
        opts.sparse_unless_sidecar_differs(
            final_path,
            sidecar,
            header.raster.bands / if header.complex { 2 } else { 1 },
            header.raster.data_type,
        );
        let r = &header.raster;
        let plan = plan(r, &opts)?;
        let (cols, rows, bands) = (r.cols, r.rows, r.bands);
        let size = r.data_type.size_bytes();
        let planar = plan.layout.planar;
        let chunk_spp = if planar { plan.layout.parts } else { bands };
        let (chunk_w, block_rows, across, down) = match plan.tile {
            Some((tw, th)) => (tw as usize, th as usize, cols.div_ceil(tw as usize), rows.div_ceil(th as usize)),
            None => {
                let per = strip_rows(opts.rows_per_strip, rows, cols * chunk_spp * size, opts.compression);
                (cols, per, 1, rows.div_ceil(per))
            }
        };
        let planes = if planar { bands / plan.layout.parts } else { 1 };
        check_rows_held(r, block_rows)?;
        if plan.tile.is_some() {
            crate::dataset::fallible::check_raster_buffer("a GeoTIFF tile", chunk_w, block_rows, chunk_spp, r.data_type)?;
        }
        let mut layout = plan.layout;
        layout.rows_per_strip = Some(block_rows as u32);
        let raw = cols
            .checked_mul(rows)
            .and_then(|n| n.checked_mul(bands))
            .and_then(|n| n.checked_mul(size))
            .ok_or_else(|| Error::ResourceLimit("a GeoTIFF output is too large to address".into()))? as u64;
        let big = opts.bigtiff || raw.checked_add(raw / 4).is_none_or(|n| n > (u32::MAX as u64) - (1 << 24));
        let mut tags = plan.geo;
        if let Some(cm) = plan.color_map {
            tags.push((320, cm));
        }
        let n = planes
            .checked_mul(across)
            .and_then(|n| n.checked_mul(down))
            .ok_or_else(|| Error::ResourceLimit("a GeoTIFF chunk index is too large to allocate".into()))?;
        let mask_chunks = if with_mask { across.checked_mul(down) } else { Some(0) };
        // Index arrays and serialized directories can coexist when finish writes the IFDs.
        let index_bytes = mask_chunks
            .and_then(|m| n.checked_add(m))
            .and_then(|n| n.checked_mul(2 * std::mem::size_of::<u64>() * 4))
            .and_then(|n| n.checked_add(1 << 20));
        crate::dataset::fallible::check_usable(index_bytes, || "a GeoTIFF chunk index and directory are too large to allocate".into())
            .map_err(Error::ResourceLimit)?;
        let offsets = crate::dataset::fallible::filled(n, 0u64, "GeoTIFF chunk offsets")?;
        let counts = crate::dataset::fallible::filled(n, 0u64, "GeoTIFF chunk sizes")?;
        let mut file = std::io::BufWriter::with_capacity(1 << 20, create_replacing(path)?);
        let head: Vec<u8> = if big {
            [b"II".as_slice(), &43u16.to_le_bytes(), &8u16.to_le_bytes(), &0u16.to_le_bytes(), &0u64.to_le_bytes()].concat()
        } else {
            [b"II".as_slice(), &42u16.to_le_bytes(), &0u32.to_le_bytes()].concat()
        };
        file.write_all(&head)?;
        Ok(Self {
            file,
            pos: head.len() as u64,
            big,
            cols,
            rows,
            bands,
            data_type: r.data_type,
            compression: opts.compression,
            codec: opts.codec,
            layout,
            block_rows,
            chunk_w,
            across,
            offsets,
            counts,
            pending: RasterData::new_filled(r.data_type, 0, 0.0),
            pending_mask: Vec::new(),
            pending_rows: 0,
            rows_done: 0,
            failed: false,
            tags,
            mask: with_mask.then(|| (Vec::new(), Vec::new())),
            empty: opts.sparse.then(|| opts.nodata_as_read(r.data_type).unwrap_or(0.0)),
        })
    }

    /// The number of rows given so far.
    pub fn rows_written(&self) -> usize {
        self.rows_done + self.pending_rows
    }

    /// Appends `rows` full rows of every band (`data`: band after band, of the raster's data
    /// type), and of the mask (0 for invalid pixels) when the file has one.
    ///
    /// # Errors
    /// When a block row failed to be written, or the data is not of the raster's data type; with
    /// `rows` not zero, when it does not hold `rows` rows of every band, rows go past the raster's
    /// height, the file has a mask and `mask` does not hold its rows, a mask is given for a file
    /// without one, or the file cannot be written.
    pub fn write_rows(&mut self, data: &RasterData, rows: usize, mask: Option<&[u8]>) -> Result<()> {
        if self.failed {
            return Err(Error::InvalidData("a block row of the GeoTIFF failed to be written".into()));
        }
        if data.data_type() != self.data_type {
            return Err(Error::InvalidArgument("the rows are not of the raster's data type".into()));
        }
        if rows == 0 {
            return Ok(());
        }
        if self.rows_written() + rows > self.rows {
            return Err(Error::InvalidArgument(format!("{} rows past the raster's {}", self.rows_written() + rows, self.rows)));
        }
        let n = self.cols * rows;
        if data.len() != n * self.bands {
            return Err(Error::InvalidArgument(format!(
                "{} values for {rows} rows of {} bands of {} columns",
                data.len(),
                self.bands,
                self.cols
            )));
        }
        let mask = match (&self.mask, mask) {
            (None, None) => None,
            (None, Some(_)) => return Err(Error::InvalidArgument("the file has no mask: give its rows without one".into())),
            (Some(_), None) => return Err(Error::InvalidArgument("the file has a mask: give its rows".into())),
            (Some(_), Some(m)) if m.len() != n => {
                return Err(Error::InvalidArgument(format!("{} mask values for {rows} rows of {} columns", m.len(), self.cols)));
            }
            (Some(_), m) => m,
        };
        self.check_row_working_set(data, rows, mask)?;
        let mut y = 0;
        while y < rows {
            let count = (self.block_rows - self.pending_rows).min(rows - y);
            let part = if y == 0 && count == rows {
                std::borrow::Cow::Borrowed(data)
            } else {
                std::borrow::Cow::Owned(slice_rows(data, rows, y..y + count, self.cols, self.bands))
            };
            self.append_block_rows(&part, count, mask.map(|m| &m[y * self.cols..(y + count) * self.cols]))?;
            y += count;
        }
        Ok(())
    }

    /// Admit the retained input, one block row's scratch, concurrent LZW encoders and the index arrays (GDAL 3.13.3).
    fn check_row_working_set(&self, data: &RasterData, rows: usize, mask: Option<&[u8]>) -> Result<()> {
        let size = self.data_type.size_bytes();
        let pixels = self.cols.checked_mul(self.block_rows.min(self.rows));
        let row = pixels.and_then(|n| n.checked_mul(self.bands)).and_then(|n| n.checked_mul(size));
        let padded = self.chunk_w.checked_mul(self.across).and_then(|n| n.checked_mul(self.block_rows));
        let image = padded.and_then(|n| n.checked_mul(self.bands)).and_then(|n| n.checked_mul(size));
        let image_indices =
            self.offsets.capacity().checked_add(self.counts.capacity()).and_then(|n| n.checked_mul(std::mem::size_of::<u64>()));
        let mask_work = if let Some((offsets, counts)) = &self.mask {
            let new_chunks =
                rows.checked_add(self.pending_rows).map(|n| n.div_ceil(self.block_rows)).and_then(|n| n.checked_mul(self.across));
            let indices = offsets
                .capacity()
                .checked_add(counts.capacity())
                .and_then(|n| n.checked_mul(3))
                .and_then(|n| new_chunks.and_then(|m| m.checked_mul(2)).and_then(|m| n.checked_add(m)))
                .and_then(|n| n.checked_mul(std::mem::size_of::<u64>()));
            padded
                .and_then(|n| n.checked_mul(3))
                .and_then(|n| n.checked_add(self.pending_mask.capacity()))
                .and_then(|n| n.checked_add(mask.map_or(0, <[u8]>::len)))
                .and_then(|n| indices.and_then(|m| n.checked_add(m)))
        } else {
            Some(0)
        };
        let lzw_scratch = if self.compression == Compression::Lzw {
            let planes = if self.layout.planar { self.bands.checked_div(self.layout.parts) } else { Some(1) };
            planes
                .and_then(|planes| self.across.checked_mul(planes))
                .and_then(|chunks| chunks.min(crate::par::threads()).checked_mul(super::tiff_lzw::Encoder::SCRATCH_BYTES))
        } else {
            Some(0)
        };
        let bytes = [
            data.len().checked_mul(size),
            self.pending.len().checked_mul(size),
            row.and_then(|n| n.checked_mul(2)),
            image.and_then(|n| n.checked_mul(2)),
            image_indices,
            mask_work,
            lzw_scratch,
            Some(1 << 20),
        ]
        .into_iter()
        .try_fold(0usize, |total, bytes| total.checked_add(bytes?));
        crate::dataset::fallible::check_usable(bytes, || "a GeoTIFF input slab and block-row working set are too large to allocate".into())
            .map(|_| ())
            .map_err(Error::ResourceLimit)
    }

    /// Appends at most the remainder of one block row, retaining band-major order.
    fn append_block_rows(&mut self, data: &RasterData, rows: usize, mask: Option<&[u8]>) -> Result<()> {
        // Pending rows are kept band after band, in the raster's type; these rows follow them, or
        // are read where they are when none is pending.
        let old = self.pending_rows;
        let (cols, bands) = (self.cols, self.bands);
        let data_type = self.data_type;
        let empty = || RasterData::new_filled(data_type, 0, 0.0);
        let all = if old > 0 {
            let joined = append_rows(&self.pending, old, 0, data, rows, cols, bands)?;
            // Freed once copied.
            self.pending = empty();
            std::borrow::Cow::Owned(joined)
        } else {
            std::borrow::Cow::Borrowed(data)
        };
        if let Some(m) = mask {
            self.pending_mask.extend_from_slice(m);
        }
        // The complete block rows (and the last rows of the raster), compressed together, then
        // the rows left over, kept for the next call.
        let total = old + rows;
        let start = if self.rows_done + total == self.rows { total } else { total / self.block_rows * self.block_rows };
        let written = if start == 0 {
            Ok(())
        } else if start == total {
            self.flush_block_rows(&all, start)
        } else {
            // Freed before the rows left over are copied.
            let slab = slice_rows(&all, total, 0..start, cols, bands);
            self.flush_block_rows(&slab, start)
        };
        if let Err(e) = written {
            // Every row given so far stays pending, as before the write, and nothing more is
            // written: the file may hold part of the block row.
            self.pending = all.into_owned();
            self.pending_rows = total;
            self.failed = true;
            return Err(e);
        }
        self.pending = match start {
            0 => all.into_owned(),
            _ if start == total => empty(),
            _ => slice_rows(&all, total, start..total, cols, bands),
        };
        if self.mask.is_some() {
            self.pending_mask.drain(..start * cols);
        }
        self.pending_rows = total - start;
        Ok(())
    }

    /// Compresses (in parallel) and writes `h` rows `data` (band after band): whole block rows, or
    /// the raster's last rows.
    fn flush_block_rows(&mut self, data: &RasterData, h: usize) -> Result<()> {
        let (cols, bands) = (self.cols, self.bands);
        let level = Level { cols, rows: h, bands, data: std::borrow::Cow::Borrowed(data) };
        // Tiles are padded to full height.
        let layout = Layout { rows_per_strip: Some(self.block_rows as u32), extra: self.layout.extra.clone(), ..self.layout };
        let img = level.sparse_image(self.data_type, self.compression, &self.codec, &layout, self.empty)?;
        img.report_clipping();
        let block_row = self.rows_done / self.block_rows;
        let per_plane = self.across * self.rows.div_ceil(self.block_rows);
        // The chunks of each plane, block row after block row.
        let slab_per_plane = self.across * h.div_ceil(self.block_rows);
        for (k, chunk) in img.chunks.iter().enumerate() {
            // A chunk left out keeps its offset and byte count of 0.
            if chunk.is_empty() {
                continue;
            }
            let (plane, j) = (k / slab_per_plane, k % slab_per_plane);
            let idx = plane * per_plane + block_row * self.across + j;
            self.offsets[idx] = self.pos;
            self.counts[idx] = chunk.len() as u64;
            self.file.write_all(chunk)?;
            self.pos += chunk.len() as u64;
            self.check_size()?;
        }
        if self.tags.iter().all(|(t, _)| *t != 256) {
            self.tags.extend(img.tags);
        }
        if self.mask.is_some() {
            let m = &self.pending_mask[..h * cols];
            let mi = mask_image(m, cols, h, (self.chunk_w, self.block_rows, self.layout.tile.is_some()))?;
            for chunk in mi.chunks {
                let offset = self.pos;
                self.file.write_all(&chunk)?;
                self.pos += chunk.len() as u64;
                self.check_size()?;
                if let Some((offsets, counts)) = &mut self.mask {
                    offsets.push(offset);
                    counts.push(chunk.len() as u64);
                }
            }
        }
        self.rows_done += h;
        Ok(())
    }

    /// Fails when a classic TIFF grows past what its 32-bit offsets address (the choice of
    /// BigTIFF is made from the uncompressed size, which compression may exceed).
    fn check_size(&self) -> Result<()> {
        if !self.big && self.pos > (u32::MAX as u64) - (1 << 24) {
            return Err(Error::InvalidData("the GeoTIFF exceeds 4 GiB: write it as a BigTIFF (BIGTIFF=YES)".into()));
        }
        Ok(())
    }

    /// Writes the image's directory (and the mask's) and closes the file.
    ///
    /// # Errors
    /// When rows are missing, a block row failed to be written, or the file cannot be written.
    pub fn finish(mut self) -> Result<()> {
        if self.failed {
            return Err(Error::InvalidData("a block row of the GeoTIFF failed to be written".into()));
        }
        if self.rows_written() != self.rows {
            return Err(Error::InvalidArgument(format!("{} rows written of {}", self.rows_written(), self.rows)));
        }
        let tiled = self.layout.tile.is_some();
        let mut tags = std::mem::take(&mut self.tags);
        for (t, v) in tags.iter_mut() {
            if *t == 257 {
                *v = Value::Longs(vec![self.rows as u32]);
            }
            if *t == 278 {
                *v = Value::Longs(vec![self.block_rows as u32]);
            }
        }
        let (off_tag, cnt_tag) = if tiled { (324, 325) } else { (273, 279) };
        tags.push((off_tag, Value::Offsets(std::mem::take(&mut self.offsets))));
        tags.push((cnt_tag, Value::Offsets(std::mem::take(&mut self.counts))));
        tags.sort_by_key(|(t, _)| *t);
        // Mask chunks were streamed with the image; only their directory remains.
        let mut mask_ifd: Option<Vec<(u16, Value)>> = None;
        if let Some((offsets, counts)) = self.mask.take() {
            let full = mask_image(&[], self.cols, 0, (self.chunk_w, self.block_rows, tiled))?;
            let mut mt: Vec<(u16, Value)> = full.tags.into_iter().filter(|(t, _)| *t != 257 && *t != 278).collect();
            mt.push((257, Value::Longs(vec![self.rows as u32])));
            if !tiled {
                mt.push((278, Value::Longs(vec![self.block_rows as u32])));
            }
            mt.push((off_tag, Value::Offsets(offsets)));
            mt.push((cnt_tag, Value::Offsets(counts)));
            mt.sort_by_key(|(t, _)| *t);
            mask_ifd = Some(mt);
        }
        // Word-aligned directories.
        if self.pos % 2 == 1 {
            self.file.write_all(&[0])?;
            self.pos += 1;
        }
        let first = self.pos;
        let main = encode_ifd(&tags, first, 0, self.big);
        let second = first + main.len() as u64 + (main.len() as u64 % 2);
        let main = match &mask_ifd {
            Some(_) => encode_ifd(&tags, first, second, self.big),
            None => main,
        };
        self.file.write_all(&main)?;
        if let Some(mt) = mask_ifd {
            if main.len() % 2 == 1 {
                self.file.write_all(&[0])?;
            }
            self.file.write_all(&encode_ifd(&mt, second, 0, self.big))?;
        }
        // The first directory's offset, in the header, which `finish` writes last: until then
        // the file is no TIFF.
        let mut file = self.file.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        use std::io::{Seek, SeekFrom};
        let end = file.stream_position()?;
        if self.big {
            file.seek(SeekFrom::Start(8))?;
            file.write_all(&first.to_le_bytes())?;
        } else {
            file.seek(SeekFrom::Start(4))?;
            file.write_all(&(first as u32).to_le_bytes())?;
        }
        file.seek(SeekFrom::Start(end))?;
        file.finish()?;
        Ok(())
    }
}

/// Rows `range` of band-major `data`, which holds `total` rows of `bands` bands of `cols` columns.
fn slice_rows(data: &RasterData, total: usize, range: std::ops::Range<usize>, cols: usize, bands: usize) -> RasterData {
    map_data!(data, v => {
        let mut out = Vec::with_capacity(range.len() * cols * bands);
        for b in 0..bands {
            out.extend_from_slice(&v[(b * total + range.start) * cols..(b * total + range.end) * cols]);
        }
        out
    })
}

/// Refuses, before its file is created, a row writer ([`TiffRows`], [`CogWriter`]) of a raster
/// whose rows it would hold cannot fit in the memory the process can use
/// ([`crate::dataset::fallible::check_usable`]): it keeps the rows of a row of its blocks
/// (`block_rows` rows, or the raster's rows when it has fewer) of every band until the row is
/// complete, and holds them twice while a strip is appended to them ([`append_rows`] copies them
/// with the strip into a new buffer). A raster of a few rows and thousands of bands written as a
/// COG, whose 512-row tiles hold every band, is held whole.
fn check_rows_held(r: &Raster, block_rows: usize) -> Result<()> {
    let (rows, size) = (block_rows.min(r.rows), r.data_type.size_bytes());
    let bytes = [r.cols, rows, r.bands, size, 2].iter().try_fold(1usize, |n, &d| n.checked_mul(d));
    crate::dataset::fallible::check_usable(bytes, || {
        format!(
            "the rows a GeoTIFF writer holds ({rows} rows of {} pixels in {} band(s) of {size}-byte samples, a row of its blocks, \
             twice while a strip is appended) are too large to allocate",
            r.cols, r.bands
        )
    })
    .map(|_| ())
    .map_err(Error::ResourceLimit)
}

/// The rows of `a` (`a_rows` rows) from `a_from` on, followed by the `b_rows` rows of `b`, band
/// after band.
fn append_rows(
    a: &RasterData,
    a_rows: usize,
    a_from: usize,
    b: &RasterData,
    b_rows: usize,
    cols: usize,
    bands: usize,
) -> Result<RasterData> {
    macro_rules! join {
        ($($v:ident),*) => {
            match (a, b) {
                $((RasterData::$v(a), RasterData::$v(b)) => {
                    let mut out = Vec::with_capacity(cols * (a_rows - a_from + b_rows) * bands);
                    for band in 0..bands {
                        out.extend_from_slice(&a[(band * a_rows + a_from) * cols..(band + 1) * cols * a_rows]);
                        out.extend_from_slice(&b[band * cols * b_rows..(band + 1) * cols * b_rows]);
                    }
                    Ok(RasterData::$v(out))
                })*
                _ => Err(Error::InvalidArgument("the rows are not of the raster's data type".into())),
            }
        };
    }
    join!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64)
}

/// Where a COG level's compressed chunks (with their leaders and trailers) wait until the file is
/// laid out: an unnamed temporary file, or memory on WebAssembly.
#[cfg(not(target_family = "wasm"))]
type ChunkSpool = std::fs::File;
#[cfg(target_family = "wasm")]
type ChunkSpool = std::io::Cursor<Vec<u8>>;

/// The directory of a COG's chunk spools, which together hold about as many bytes as the COG.
/// As GDAL's COG driver places its temporary files (`GetTmpFilename`), this is the directory of
/// the file the output names (a symbolic link or a redirected `/dev/stdout` resolved), so the
/// spools stay on the output's drive; `TERRIO_TMPDIR` or `CPL_TMPDIR` names another. An output
/// that is not a regular file (a pipe or a device) has no directory to spool beside, so the
/// system's temporary directory is used.
///
/// Reference: GDAL v3.12.1 `frmts/gtiff/cogdriver.cpp` (`GetTmpFilename`).
fn spool_dir(path: &str) -> std::path::PathBuf {
    if let Some(dir) = crate::config::get_any(&["TERRIO_TMPDIR", "CPL_TMPDIR"]) {
        return dir.into();
    }
    if std::fs::metadata(path).is_ok_and(|m| !m.is_file()) {
        return std::env::temp_dir();
    }
    // `/dev/stdout` names a regular file when stdout is redirected to one: spool beside that file.
    let resolved = std::fs::canonicalize(path).unwrap_or_else(|_| {
        // canonicalize requires the final file to exist. Follow a dangling link too, since
        // opening the output will create its target. Bound traversal just as file opening does.
        let mut target = std::path::PathBuf::from(path);
        for _ in 0..40 {
            let Ok(next) = std::fs::read_link(&target) else { break };
            target = if next.is_absolute() { next } else { target.parent().unwrap_or(std::path::Path::new(".")).join(next) };
        }
        target
    });
    match resolved.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => ".".into(),
    }
}

fn chunk_spool(dir: &std::path::Path) -> Result<ChunkSpool> {
    #[cfg(not(target_family = "wasm"))]
    return tempfile::tempfile_in(dir).map_err(Error::Io);
    #[cfg(target_family = "wasm")]
    {
        let _ = dir;
        Ok(Default::default())
    }
}

/// One resolution level of a [`CogWriter`].
struct CogLevel {
    cols: usize,
    rows: usize,
    /// The rows received so far.
    received: usize,
    /// Rows received but not yet compressed (less than a row of tiles), band after band.
    pending: RasterData,
    pending_rows: usize,
    /// The compressed chunks of each plane, and their sizes.
    spools: Vec<ChunkSpool>,
    sizes: Vec<Vec<u64>>,
    /// The structural tags, from the first row of tiles.
    tags: Vec<(u16, Value)>,
    mask: Option<CogMask>,
}

/// Mask rows and compressed tiles follow the same bounded pipeline as the image.
struct CogMask {
    pending: Vec<u8>,
    spool: ChunkSpool,
    sizes: Vec<u64>,
    tags: Vec<(u16, Value)>,
}

type CogImages = (Image, Option<Image>);

/// An overview level of a [`CogWriter`] being computed from the level it is resampled from: the
/// rows of that level its next blocks need (`buf_rows` rows from `buf_row0`, band after band, and
/// their mask when the COG has one), and its next row. With a mask, the rows and the mask are
/// shared with the block rows being computed from them, and the first `spent` rows, which no later
/// block reads, are left out when rows are next appended.
struct CogBuild {
    level: usize,
    src: usize,
    plan: gdal_overview::Plan,
    buf: std::sync::Arc<RasterData>,
    buf_row0: usize,
    buf_rows: usize,
    /// With a mask, the first rows held that no later block reads: left out when rows are next
    /// appended (a block row being computed may still read them until then).
    spent: usize,
    next_y: usize,
    /// The mask of the rows (`RasterData::U8`, empty without a mask).
    mask: std::sync::Arc<RasterData>,
    mask_plan: gdal_overview::Plan,
    /// Block rows being computed on the thread pool (a COG with a mask's), in order.
    #[cfg(not(target_family = "wasm"))]
    in_flight: std::collections::VecDeque<BlockRowJob>,
}

/// An overview block row: its rows of every band, and of its mask when the COG has one.
type OverviewRows = (RasterData, Option<Vec<u8>>);

/// A block row being computed on the thread pool: its rows, the block row (and its mask) to come,
/// and the rows and mask it is resampled from, which it holds until it is done, with their bytes.
#[cfg(not(target_family = "wasm"))]
struct BlockRowJob {
    rows: usize,
    result: std::sync::mpsc::Receiver<OverviewRows>,
    holds: [(std::sync::Weak<RasterData>, usize); 2],
}

impl CogBuild {
    /// The bytes the block rows being computed keep alive besides the build's own rows and mask:
    /// rows and masks the build has replaced since (appending to them), each counted once.
    fn kept(&self) -> usize {
        #[cfg(not(target_family = "wasm"))]
        {
            let mut counted = vec![std::sync::Arc::as_ptr(&self.buf), std::sync::Arc::as_ptr(&self.mask)];
            let mut bytes = 0usize;
            for (held, size) in self.in_flight.iter().flat_map(|job| &job.holds) {
                if held.strong_count() > 0 && !counted.contains(&held.as_ptr()) {
                    counted.push(held.as_ptr());
                    bytes = bytes.saturating_add(*size);
                }
            }
            bytes
        }
        #[cfg(target_family = "wasm")]
        0
    }
}

/// A COG written a few rows at a time, from the top, so that the raster is never held whole. Each
/// level (the full resolution, then the overviews down to one that fits in a tile) compresses its
/// rows of tiles as they are complete, into a temporary file, and keeps the rows the next level's
/// next blocks are resampled from, computing them as GDAL does once their rows are in (as
/// [`write`] computes them from the whole raster). Once the last row is in, the file is laid out as
/// [`write`] lays it out: the IFDs, then the levels' tiles from the smallest overview, copied from
/// the temporary files. The file is byte for byte the same.
pub(crate) struct CogWriter {
    file: std::io::BufWriter<Rewrite>,
    bands: usize,
    data_type: DataType,
    encoding: std::sync::Arc<(Compression, CodecParams, Layout)>,
    tile: usize,
    nodata: Option<f64>,
    /// The alpha band, and whether it masks the other bands ([`alpha_weights`]).
    alpha: (Option<usize>, bool),
    mask_without_dataset: bool,
    builds: Vec<CogBuild>,
    bigtiff: bool,
    /// The full-resolution image's georeferencing and metadata tags (nodata on every level).
    geo: Vec<(u16, Value)>,
    color_map: Option<Value>,
    levels: Vec<CogLevel>,
    /// Rows of tiles being compressed, in the order they were given: their level, number of
    /// rows, and the compressed image to come.
    #[cfg(not(target_family = "wasm"))]
    in_flight: std::collections::VecDeque<(usize, usize, std::sync::mpsc::Receiver<Result<CogImages>>)>,
    /// Whether rows it took failed to be written: a row of tiles failed to be compressed or added
    /// to its level's spools, or rows failed to be resampled or added to a level. The file would
    /// miss them: every later call is refused ([`Self::refuse_failed`]).
    failed: bool,
    /// While a test write-locks it, the block rows started wait for it before they are computed.
    #[cfg(all(test, not(target_family = "wasm")))]
    hold: std::sync::Arc<std::sync::RwLock<()>>,
}

/// The most rows of tiles a [`CogWriter`] compresses at once, so that the tiles of the next rows
/// keep the threads busy while the last tiles of the first are compressed.
#[cfg(not(target_family = "wasm"))]
const COG_IN_FLIGHT: usize = 3;

/// The most block rows of one overview level a [`CogWriter`] with a mask computes at once on the
/// thread pool, while the calling thread gives the next rows and the other levels compute theirs.
/// Each holds the rows it is resampled from and its working buffers, which more at once would
/// multiply for little time saved.
#[cfg(not(target_family = "wasm"))]
const COG_BLOCK_ROWS_IN_FLIGHT: usize = 1;

/// The bytes a row of tiles being compressed takes besides its rows and its chunks' bytes, as
/// [`CogWriter::call_bytes`] counts it: the job compressing it, the channel its tiles come back
/// in, and their tags and list of chunks, per band (measured at about 1 KiB; 4 KiB reserved for margin).
const COG_JOB_BYTES: usize = 4 << 10;

/// A row of tiles a COG call may be compressing, as [`CogWriter::call_bytes`] counts it:
/// the bytes it holds (its rows, unless counted elsewhere, and its compressed chunks), and the
/// image's chunks and the mask's tiles compressed for it.
#[derive(Clone, Copy, Default)]
struct Compressing {
    bytes: usize,
    image_chunks: usize,
    mask_tiles: usize,
}

impl CogWriter {
    /// Creates the file, for a raster described by `raster` (its pixels are not read). `opts` are
    /// a COG's. `opts.mask` enables a mask supplied with each row slab.
    pub(crate) fn create(path: &str, raster: &Raster, opts: &TiffOptions) -> Result<Self> {
        if !opts.cog {
            return Err(Error::Unsupported("a COG row writer requires COG options".into()));
        }
        if opts.cog_area_matched.is_some() {
            return Err(Error::Unsupported("COGs with area-matched overviews need the whole raster".into()));
        }
        let Plan { tile, layout, geo, color_map } = plan(raster, opts)?;
        let tile = tile.map_or(512, |(t, _)| t as usize);
        check_rows_held(raster, tile)?;
        let chunk_spp = if layout.planar { layout.parts } else { raster.bands };
        crate::dataset::fallible::check_raster_buffer("a GeoTIFF tile", tile, tile, chunk_spp, raster.data_type)?;
        let planes = if layout.planar { raster.bands / layout.parts } else { 1 };
        let mut sizes = vec![(raster.cols, raster.rows)];
        sizes.extend(gdal_overview::cog_sizes(raster.cols, raster.rows, tile, opts.overview_count));
        let (method, block) = (cog_method(opts), gdal_overview::ovr_block(opts.source_block));
        let builds = (1..sizes.len())
            .map(|level| {
                // As GDAL does, each overview is computed from the previous one when it is larger.
                let src = if sizes[level - 1].0 > sizes[level].0 { level - 1 } else { 0 };
                let plan = gdal_overview::Plan::new(method, sizes[src], sizes[level], raster.bands, block, raster.data_type);
                let buf = std::sync::Arc::new(RasterData::new_filled(raster.data_type, 0, 0.0));
                let mask_plan = gdal_overview::Plan::new(method, sizes[src], sizes[level], 1, block, DataType::U8);
                CogBuild {
                    level,
                    src,
                    plan,
                    buf,
                    buf_row0: 0,
                    buf_rows: 0,
                    spent: 0,
                    next_y: 0,
                    mask: std::sync::Arc::new(RasterData::U8(Vec::new())),
                    mask_plan,
                    #[cfg(not(target_family = "wasm"))]
                    in_flight: Default::default(),
                }
            })
            .collect();
        let dir = spool_dir(path);
        let levels = sizes
            .into_iter()
            .map(|(cols, rows)| {
                Ok(CogLevel {
                    cols,
                    rows,
                    received: 0,
                    pending: RasterData::new_filled(raster.data_type, 0, 0.0),
                    pending_rows: 0,
                    spools: (0..planes).map(|_| chunk_spool(&dir)).collect::<Result<_>>()?,
                    sizes: vec![Vec::new(); planes],
                    tags: Vec::new(),
                    mask: if opts.mask.is_some() {
                        Some(CogMask { pending: Vec::new(), spool: chunk_spool(&dir)?, sizes: Vec::new(), tags: Vec::new() })
                    } else {
                        None
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let file = std::io::BufWriter::with_capacity(1 << 20, create_replacing(path)?);
        Ok(Self {
            file,
            bands: raster.bands,
            data_type: raster.data_type,
            encoding: std::sync::Arc::new((opts.compression, opts.codec, layout)),
            tile,
            nodata: opts.nodata,
            alpha: alpha_weights(opts, raster.bands, raster.data_type),
            mask_without_dataset: opts.mask_without_dataset,
            builds,
            bigtiff: opts.bigtiff,
            geo,
            color_map,
            levels,
            #[cfg(not(target_family = "wasm"))]
            in_flight: Default::default(),
            failed: false,
            #[cfg(all(test, not(target_family = "wasm")))]
            hold: Default::default(),
        })
    }

    /// The number of full-resolution rows given so far.
    pub(crate) fn rows_written(&self) -> usize {
        self.levels[0].received
    }

    /// Appends the next `rows` full rows of every band (`data`: band after band, of the raster's
    /// data type).
    ///
    /// # Errors
    /// Rejects a sample-type or shape mismatch, or rows past the raster's height, before consuming
    /// rows or changing overview state, and refuses a call whose bytes are more than the memory the
    /// process can use ([`Self::admit`]). Once rows failed to be written, refuses every call
    /// ([`Self::refuse_failed`]).
    pub(crate) fn write_rows(&mut self, data: RasterData, rows: usize) -> Result<()> {
        self.write_rows_masked(data, rows, None)
    }

    /// [`Self::write_rows`], with the rows' mask when the COG has one. The rows and mask are the
    /// writer's: checked as they are, not copied.
    pub(crate) fn write_rows_masked(&mut self, data: RasterData, rows: usize, mask: Option<Vec<u8>>) -> Result<()> {
        self.admit(&data, rows, mask.as_deref(), false)?;
        self.take_admitted(data, rows, mask)
    }

    /// Checks a call giving `rows` rows (`data`, and `mask` when the COG has a mask) before it
    /// takes any: refused once rows failed to be written ([`Self::refuse_failed`]); with
    /// [`Error::InvalidArgument`] when the rows are not of the raster's data type and shape or go
    /// past its height; and with [`Error::ResourceLimit`] when the bytes [`Self::call_bytes`] counts
    /// for it are more than the memory the process can use (`borrowed`: the writer copies the
    /// caller's rows first), once the rows of tiles compressed since the last call are added to
    /// their spools ([`Self::land_compressed`]): they no longer count. A refused call takes no rows,
    /// and can be made again.
    fn admit(&mut self, data: &RasterData, rows: usize, mask: Option<&[u8]>, borrowed: bool) -> Result<()> {
        self.refuse_failed()?;
        if data.data_type() != self.data_type {
            return Err(Error::InvalidArgument("the rows are not of the raster's data type".into()));
        }
        let l = &self.levels[0];
        match (&l.mask, mask) {
            (None, Some(_)) => return Err(Error::InvalidArgument("the COG has no mask: give its rows without one".into())),
            (Some(_), None) => return Err(Error::InvalidArgument("the COG has a mask: give its rows".into())),
            (Some(_), Some(m)) if l.cols.checked_mul(rows) != Some(m.len()) => {
                return Err(Error::InvalidArgument(format!("{} mask values for {rows} rows of {} columns", m.len(), l.cols)));
            }
            _ => {}
        }
        if rows > l.rows - l.received {
            return Err(Error::InvalidArgument(format!("{} rows past the raster's {}", l.received.saturating_add(rows), l.rows)));
        }
        if l.cols.checked_mul(rows).and_then(|n| n.checked_mul(self.bands)) != Some(data.len()) {
            return Err(Error::InvalidArgument(format!(
                "{} values for {rows} rows of {} bands of {} columns",
                data.len(),
                self.bands,
                l.cols
            )));
        }
        #[cfg(not(target_family = "wasm"))]
        self.land_compressed()?;
        let bytes = data.len().checked_mul(self.data_type.size_bytes());
        let bytes = bytes.and_then(|bytes| self.call_bytes(bytes, mask.map_or(0, <[u8]>::len), rows, borrowed));
        crate::dataset::fallible::check_usable(bytes, || "a COG input slab and overview working copies are too large to allocate".into())
            .map_err(Error::ResourceLimit)?;
        Ok(())
    }

    /// Takes the rows of a call [`Self::admit`] admitted; a failure is the writer's
    /// ([`Self::failed`]).
    fn take_admitted(&mut self, data: RasterData, rows: usize, mask: Option<Vec<u8>>) -> Result<()> {
        // As the whole writer reads a mask: any nonzero value is valid, weighted as 255.
        let mask: Option<Vec<u8>> = mask.map(|m| m.into_iter().map(|v| if v != 0 { 255 } else { 0 }).collect());
        let taken = self.take_rows(data, rows, mask);
        self.fail_on(taken)
    }

    /// The rows of a call, once admitted: added to the full resolution, then the overview block
    /// rows computed meanwhile added to their levels.
    fn take_rows(&mut self, data: RasterData, rows: usize, mask: Option<Vec<u8>>) -> Result<()> {
        if rows > 0 {
            self.push(0, data, rows, mask)?;
        }
        #[cfg(not(target_family = "wasm"))]
        for b in 0..self.builds.len() {
            while self.land_block_row(b, false)? {}
        }
        Ok(())
    }

    /// Refuses a call once rows the writer took failed to be written ([`Self::failed`]).
    fn refuse_failed(&self) -> Result<()> {
        if self.failed {
            return Err(Error::InvalidData("a row of the COG's tiles or overviews failed to be written".into()));
        }
        Ok(())
    }

    /// `result`, recorded as the writer's failure when it is one ([`Self::failed`]).
    fn fail_on<T>(&mut self, result: Result<T>) -> Result<T> {
        self.failed |= result.is_err();
        result
    }

    /// Adds `rows` rows to level `i`: resampled into the levels computed from it as soon as their
    /// next blocks' rows are in (with a mask, on the thread pool, alongside the next rows and the
    /// other levels' block rows, each added to its level in turn), and compressed by rows of tiles.
    fn push(&mut self, i: usize, data: RasterData, rows: usize, mask: Option<Vec<u8>>) -> Result<()> {
        let bands = self.bands;
        let (cols, done) = {
            let l = &mut self.levels[i];
            l.received += rows;
            (l.cols, l.received == l.rows)
        };
        let received = self.levels[i].received;
        for b in 0..self.builds.len() {
            if self.builds[b].src != i {
                continue;
            }
            let bd = &mut self.builds[b];
            // The rows held that later blocks read, then the new ones (without a mask, every row held).
            let live = bd.buf_rows - bd.spent;
            let appended = if live == 0 { data.clone() } else { append_rows(&bd.buf, bd.buf_rows, bd.spent, &data, rows, cols, bands)? };
            bd.buf = std::sync::Arc::new(appended);
            if let Some(mask) = &mask {
                let RasterData::U8(held) = &*bd.mask else { unreachable!("a Byte mask") };
                let mut appended = Vec::with_capacity((live + rows) * cols);
                appended.extend_from_slice(&held[bd.spent * cols..]);
                appended.extend_from_slice(mask);
                bd.mask = std::sync::Arc::new(RasterData::U8(appended));
            }
            bd.buf_row0 += bd.spent;
            bd.buf_rows = live + rows;
            bd.spent = 0;
            loop {
                let bd = &self.builds[b];
                if bd.next_y >= bd.plan.dst.1 {
                    break;
                }
                let (n, need) = bd.plan.rows(bd.next_y);
                if need.end > received && !done {
                    break;
                }
                let compute = self.block_row_job(b, mask.is_some());
                // With a mask, a block row is about three times the work (the image's weighted by the
                // mask, and the mask's own): on the thread pool, while the next rows are given. Without
                // one, the calling thread keeps up with it here, before its rows are dropped, as before.
                #[cfg(not(target_family = "wasm"))]
                let here = if mask.is_some() {
                    let bd = &self.builds[b];
                    let holds = [
                        (std::sync::Arc::downgrade(&bd.buf), bd.buf.len() * self.data_type.size_bytes()),
                        (std::sync::Arc::downgrade(&bd.mask), bd.mask.len()),
                    ];
                    let (tx, rx) = std::sync::mpsc::sync_channel(1);
                    #[cfg(test)]
                    let hold = self.hold.clone();
                    crate::par::spawn(move || {
                        #[cfg(test)]
                        drop(hold.read());
                        let _ = tx.send(compute());
                    });
                    self.builds[b].in_flight.push_back(BlockRowJob { rows: n, result: rx, holds });
                    None
                } else {
                    Some(compute())
                };
                #[cfg(target_family = "wasm")]
                let here = Some(compute());
                let bd = &mut self.builds[b];
                bd.next_y += n;
                // The rows no later block reads: with a mask, left out when rows are next appended (a
                // block row being computed may still read the rows held); without, dropped now.
                let keep = bd.plan.first_row(bd.next_y).clamp(bd.buf_row0, bd.buf_row0 + bd.buf_rows);
                if mask.is_some() {
                    bd.spent = keep - bd.buf_row0;
                } else if keep > bd.buf_row0 {
                    let drop = keep - bd.buf_row0;
                    bd.buf = std::sync::Arc::new(slice_rows(&bd.buf, bd.buf_rows, drop..bd.buf_rows, cols, bands));
                    bd.buf_rows -= drop;
                    bd.buf_row0 = keep;
                }
                if let Some((out, next_mask)) = here {
                    let level = self.builds[b].level;
                    self.push(level, out, n, next_mask)?;
                }
                #[cfg(not(target_family = "wasm"))]
                while self.builds[b].in_flight.len() > COG_BLOCK_ROWS_IN_FLIGHT {
                    self.land_block_row(b, true)?;
                }
            }
        }
        // Whole rows of tiles, and the level's last rows.
        let l = &mut self.levels[i];
        if l.pending_rows == 0 {
            l.pending = data;
        } else {
            l.pending = append_rows(&l.pending, l.pending_rows, 0, &data, rows, cols, bands)?;
        }
        l.pending_rows += rows;
        if let (Some(pending), Some(mask)) = (&mut l.mask, mask) {
            if pending.pending.is_empty() {
                pending.pending = mask;
            } else {
                pending.pending.extend(mask);
            }
        }
        let total = l.pending_rows;
        let ready = if done { total } else { total / self.tile * self.tile };
        if ready > 0 {
            let (slab, rest) = if ready == total {
                (std::mem::replace(&mut l.pending, RasterData::new_filled(self.data_type, 0, 0.0)), None)
            } else {
                (slice_rows(&l.pending, total, 0..ready, cols, bands), Some(slice_rows(&l.pending, total, ready..total, cols, bands)))
            };
            if let Some(rest) = rest {
                l.pending = rest;
            }
            l.pending_rows = total - ready;
            let mask = l
                .mask
                .as_mut()
                .map(|m| if ready == total { std::mem::take(&mut m.pending) } else { m.pending.drain(..ready * cols).collect() });
            self.compress(i, slab, ready, mask)?;
        }
        Ok(())
    }

    /// The next block row of build `b` as a job, from the rows the build holds and, when `masked`,
    /// their mask (both shared): it computes the block row and the mask's, side by side, as `push`
    /// computed them.
    fn block_row_job(&self, b: usize, masked: bool) -> impl FnOnce() -> OverviewRows + Send + 'static {
        let bd = &self.builds[b];
        let (plan, mask_plan) = (bd.plan.clone(), bd.mask_plan.clone());
        let (buf, row0, rows, y) = (bd.buf.clone(), bd.buf_row0, bd.buf_rows, bd.next_y);
        let mask = masked.then(|| bd.mask.clone());
        let (bands, nodata, alpha, mask_without_dataset) = (self.bands, self.nodata, self.alpha, self.mask_without_dataset);
        let own = !mask_without_dataset || bd.src == 0;
        move || {
            let values = match mask.as_deref() {
                Some(RasterData::U8(values)) => Some(values.as_slice()),
                _ => None,
            };
            let all_valid = if own { Vec::new() } else { vec![255; values.map_or(0, <[u8]>::len)] };
            let mut parts = crate::par::map_range(1 + usize::from(values.is_some()), |k| match (k, &mask) {
                (0, _) => {
                    let weights = gdal_overview::Weights { alpha: alpha.0, alpha_masks: alpha.1, mask: values };
                    gdal_overview::block_row(&plan, &buf, row0, rows, bands, nodata, weights, y)
                }
                (_, Some(m)) => {
                    let weights = gdal_overview::Weights {
                        alpha: own.then_some(0),
                        alpha_masks: false,
                        mask: (!own).then_some(all_valid.as_slice()),
                    };
                    gdal_overview::block_row(&mask_plan, m, row0, rows, 1, None, weights, y)
                }
                (_, None) => unreachable!("a mask's block row without a mask"),
            });
            let next_mask = (parts.len() == 2).then(|| {
                let Some(RasterData::U8(mut values)) = parts.pop() else { unreachable!("a Byte mask") };
                if !mask_without_dataset {
                    values.iter_mut().for_each(|v| *v = if *v != 0 { 255 } else { 0 });
                }
                values
            });
            let Some(out) = parts.pop() else { unreachable!("a block row") };
            (out, next_mask)
        }
    }

    /// Adds the oldest block row of build `b` being computed to its level, waiting for it when
    /// `wait`; whether one was added.
    #[cfg(not(target_family = "wasm"))]
    fn land_block_row(&mut self, b: usize, wait: bool) -> Result<bool> {
        let Some(job) = self.builds[b].in_flight.front() else { return Ok(false) };
        let stopped = || Error::InvalidData("a COG overview computation stopped".into());
        let (out, next_mask) = if wait {
            job.result.recv().map_err(|_| stopped())?
        } else {
            match job.result.try_recv() {
                Ok(rows) => rows,
                Err(std::sync::mpsc::TryRecvError::Empty) => return Ok(false),
                Err(std::sync::mpsc::TryRecvError::Disconnected) => return Err(stopped()),
            }
        };
        let Some(job) = self.builds[b].in_flight.pop_front() else { return Ok(false) };
        let level = self.builds[b].level;
        self.push(level, out, job.rows, next_mask)?;
        Ok(true)
    }

    /// Compresses rows of tiles of level `i` (in parallel, on the thread pool, while the next rows
    /// are given), to be added to its spools in turn.
    fn compress(&mut self, i: usize, data: RasterData, rows: usize, mask: Option<Vec<u8>>) -> Result<()> {
        let level = Level { cols: self.levels[i].cols, rows, bands: self.bands, data: std::borrow::Cow::Owned(data) };
        let (data_type, encoding) = (self.data_type, self.encoding.clone());
        let tile = self.tile;
        let encode = move || -> Result<CogImages> {
            let (compression, codec, layout) = &*encoding;
            let mask = mask.map(|mask| mask_image(&mask, level.cols, rows, (tile, tile, true))).transpose()?;
            Ok((level.image(data_type, *compression, codec, layout)?, mask))
        };
        #[cfg(target_family = "wasm")]
        return self.add(i, rows, encode()?);
        #[cfg(not(target_family = "wasm"))]
        {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            crate::par::spawn(move || {
                let _ = tx.send(encode());
            });
            self.in_flight.push_back((i, rows, rx));
            while self.in_flight.len() > COG_IN_FLIGHT {
                self.land()?;
            }
            Ok(())
        }
    }

    /// Adds the oldest rows of tiles being compressed to their level's spools; a failure is the
    /// writer's ([`Self::failed`]).
    #[cfg(not(target_family = "wasm"))]
    fn land(&mut self) -> Result<()> {
        let Some((i, rows, rx)) = self.in_flight.pop_front() else { return Ok(()) };
        let landed = match rx.recv() {
            Ok(img) => img.and_then(|img| self.add(i, rows, img)),
            Err(_) => Err(Error::InvalidData("a COG tile compression stopped".into())),
        };
        self.fail_on(landed)
    }

    /// Adds the rows of tiles already compressed, oldest first, to their level's spools; a failure
    /// is the writer's ([`Self::failed`]), which refuses the calls after it.
    #[cfg(not(target_family = "wasm"))]
    fn land_compressed(&mut self) -> Result<()> {
        self.refuse_failed()?;
        while let Some((_, _, rx)) = self.in_flight.front() {
            let img = match rx.try_recv() {
                Ok(img) => img,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(Error::InvalidData("a COG tile compression stopped".into())),
            };
            // Taken from the queue before its error is returned, as `land` takes it.
            let Some((i, rows, _)) = self.in_flight.pop_front() else { break };
            let landed = img.and_then(|img| self.add(i, rows, img));
            self.fail_on(landed)?;
        }
        Ok(())
    }

    /// The most bytes the compressed tiles of `rows` rows of level `i` take: the rows padded to
    /// whole tiles, each chunk in the buffer its encoder wrote it in ([`encoder_bytes`]), and their
    /// mask's tiles (a bit a pixel, deflated), with each chunk's headers and the job compressing
    /// them ([`COG_JOB_BYTES`]).
    fn encoded_bytes(&self, i: usize, rows: usize) -> usize {
        let t = self.tile;
        let tiles = self.levels[i].cols.div_ceil(t).saturating_mul(rows.div_ceil(t));
        let (planes, raw) = self.chunk_raw();
        let image = tiles.saturating_mul(planes).saturating_mul(encoder_bytes(self.encoding.0, raw).0);
        let mask =
            if self.levels[i].mask.is_some() { tiles.saturating_mul(encoder_bytes(Compression::Deflate, t.div_ceil(8) * t).0) } else { 0 };
        // Each chunk's headers.
        let headers = tiles.saturating_mul(self.bands + 1).saturating_mul(64);
        let job = if tiles > 0 { COG_JOB_BYTES.saturating_mul(self.bands) } else { 0 };
        image.saturating_add(mask).saturating_add(headers).saturating_add(job)
    }

    /// The chunks of a tile (one, or one a plane), and the bytes of each chunk's samples.
    fn chunk_raw(&self) -> (usize, usize) {
        let layout = &self.encoding.2;
        let (planes, samples) = if layout.planar { (self.bands / layout.parts, layout.parts) } else { (1, self.bands) };
        (planes, (self.tile * self.tile).saturating_mul(samples * self.data_type.size_bytes()))
    }

    /// The image's chunks, and the mask's tiles, of `rows` rows of level `i`.
    fn chunks(&self, i: usize, rows: usize) -> (usize, usize) {
        let tiles = self.levels[i].cols.div_ceil(self.tile).saturating_mul(rows.div_ceil(self.tile));
        (tiles.saturating_mul(self.chunk_raw().0), if self.levels[i].mask.is_some() { tiles } else { 0 })
    }

    /// The most bytes a thread of the pool holds while it compresses one of the image's chunks, and
    /// one of the mask's tiles, besides the buffer it writes it in: the chunk's samples, copied out
    /// of its rows (and packed to fewer bits, when they are), or the tile's bits, and what the
    /// encoder holds meanwhile.
    fn chunk_work(&self) -> (usize, usize) {
        let (compression, _, layout) = &*self.encoding;
        let size = self.data_type.size_bytes();
        let raw = self.chunk_raw().1;
        let packed = if layout.nbits.is_some_and(|bits| usize::from(bits) != size * 8) { raw } else { 0 };
        let bits = self.tile.div_ceil(8) * self.tile;
        (raw.saturating_add(packed).saturating_add(encoder_bytes(*compression, raw).1), bits + encoder_bytes(Compression::Deflate, bits).1)
    }

    /// The bytes [`Self::admit`] checks against the memory the process can use before a call giving
    /// `rows` rows (`data` bytes, and `mask` bytes of their mask), `None` for more than the address
    /// space holds: the most the call can have allocated at once. That is the rows given, the
    /// writer's copy of them when the caller only lends them (`borrowed`), and each overview build's,
    /// while the rows each build and level holds are replaced by copies (twice); the rows cut into
    /// rows of tiles; the overview block rows computed meanwhile, with their working buffers, and
    /// added to the next levels; and the rows of tiles being compressed: their compressed chunks, the
    /// chunks being compressed with their encoders' memory, and the scratch each thread that deflates
    /// keeps. With a mask, the block rows are computed on the thread pool while the next rows are
    /// given, and the rows they keep that the builds no longer do count too. Without one, the calling
    /// thread computes them one at a time, and each build copies the rows later blocks read after
    /// each block row.
    fn call_bytes(&self, data: usize, mask: usize, rows: usize, borrowed: bool) -> Option<usize> {
        // Saturating: a term at `usize::MAX` is more than the address space holds.
        let sum = |parts: &[usize]| parts.iter().fold(0usize, |total, n| total.saturating_add(*n));
        let size = self.data_type.size_bytes();
        let new = data.saturating_add(mask);
        let held = |b: &CogBuild| b.buf.len().saturating_mul(size).saturating_add(b.mask.len());
        let pending =
            |l: &CogLevel| l.pending.len().saturating_mul(size).saturating_add(l.mask.as_ref().map_or(0, |m| m.pending.capacity()));
        let (built, waiting): (Vec<_>, Vec<_>) = (self.builds.iter().map(held).collect(), self.levels.iter().map(pending).collect());
        // The rows given, the writer's copy of them when they are lent, and a copy for each overview
        // build that reads the full resolution (each appends them to the rows it holds).
        let copies = 1 + usize::from(borrowed) + self.builds.iter().filter(|b| b.src == 0).count();
        let mut terms = vec![new.saturating_mul(copies), sum(&built).saturating_mul(2), sum(&waiting).saturating_mul(2)];
        let l0 = &self.levels[0];
        let masked = l0.mask.is_some();
        // Rows (with those pending) that are not whole rows of tiles are joined, then cut in two:
        // the rows pending are held a third time.
        let cut = l0.pending_rows > 0 || (!rows.is_multiple_of(self.tile) && l0.received.saturating_add(rows) < l0.rows);
        terms.extend([new.saturating_mul(2 * usize::from(cut)), sum(&waiting)]);
        // A block row of a level: its rows of every band, and of the mask when the COG has one.
        let output = |b: &CogBuild| b.plan.rows(0).0.saturating_mul(b.plan.dst.0).saturating_mul(self.bands * size + usize::from(masked));
        let mut work = Vec::new();
        // Without a mask, the most rows a build appends in the call: copied again after a block row,
        // with the rows later blocks read (the build's own rows count above).
        let mut appended = 0;
        for b in &self.builds {
            terms.push(b.kept());
            // The block rows of the level the build reads, added to it: copied, joined and cut.
            let from = self.builds.iter().find(|f| f.level == b.src);
            if let Some(from) = from {
                terms.push(output(from).saturating_mul(3));
            }
            appended = appended.max(from.map_or(new, output));
            let image = b.plan.block_row_bytes(self.bands, self.data_type);
            work.push(if masked {
                let own = !self.mask_without_dataset || b.src == 0;
                sum(&[image, b.mask_plan.block_row_bytes(1, DataType::U8), if own { 0 } else { b.mask.len() }])
            } else {
                image
            });
        }
        let largest_work = work.iter().copied().max().unwrap_or(0);
        // With a mask, a block row of each level being computed, and a second of the level waiting for
        // one; without, the block row the calling thread computes, and the rows a build copies after it.
        terms.extend(if masked { [sum(&work), largest_work] } else { [largest_work, appended] });
        // The rows of tiles being compressed (`COG_IN_FLIGHT` + 1 at most at once; on WebAssembly,
        // one), each with its rows and its tiles: those still compressed at the call, those of the
        // rows given that `push` compresses (whole rows of tiles, or all of them with the last rows;
        // the rows are counted above), and the overviews' (a row of tiles, or a block row and a row
        // of tiles when block rows do not make whole rows of tiles).
        #[cfg(not(target_family = "wasm"))]
        let at_once = COG_IN_FLIGHT + 1;
        #[cfg(target_family = "wasm")]
        let at_once = 1;
        let tiles = |i: usize, r: usize, bytes: usize| {
            let (image_chunks, mask_tiles) = self.chunks(i, r);
            Compressing { bytes: bytes.saturating_add(self.encoded_bytes(i, r)), image_chunks, mask_tiles }
        };
        let compressed =
            |i: usize, r: usize| tiles(i, r, r.saturating_mul(self.levels[i].cols).saturating_mul(self.bands * size + usize::from(masked)));
        let given = l0.pending_rows.saturating_add(rows);
        let ready = if l0.received.saturating_add(rows) >= l0.rows { given } else { given / self.tile * self.tile };
        let mut entries = vec![tiles(0, ready, 0)];
        #[cfg(not(target_family = "wasm"))]
        entries.extend(self.in_flight.iter().map(|(i, r, _)| compressed(*i, *r)));
        let overview = self.builds.iter().map(|b| {
            let n = b.plan.rows(0).0;
            compressed(b.level, if self.tile.is_multiple_of(n) { self.tile } else { n + self.tile })
        });
        let largest = overview.fold(Compressing::default(), |a, b| Compressing {
            bytes: a.bytes.max(b.bytes),
            image_chunks: a.image_chunks.max(b.image_chunks),
            mask_tiles: a.mask_tiles.max(b.mask_tiles),
        });
        entries.extend(std::iter::repeat_n(largest, at_once));
        // The most of one measure the rows of tiles being compressed at once hold together: the
        // sum of the `at_once` largest, each measure bounded on its own.
        let most_at_once = |measure: fn(&Compressing) -> usize| {
            let mut v: Vec<usize> = entries.iter().map(measure).collect();
            v.sort_unstable_by(|a, b| b.cmp(a));
            sum(&v[..at_once])
        };
        // Their chunks and their mask's tiles are compressed on the thread pool, each on one
        // thread, which holds its samples and its encoder's memory meanwhile: as many at once as
        // the pool has threads. Each thread that deflates (the mask's tiles, or the chunks of a
        // DEFLATE or LERC_DEFLATE COG) keeps Deflate's scratch for the next.
        let (threads, (image, mask)) = (crate::par::threads(), self.chunk_work());
        let working = [
            image.saturating_mul(threads.min(most_at_once(|c| c.image_chunks))),
            mask.saturating_mul(threads.min(most_at_once(|c| c.mask_tiles))),
            if masked || matches!(self.encoding.0, Compression::Deflate | Compression::LercDeflate) {
                threads.saturating_mul(crate::deflate_compress::SCRATCH_BYTES)
            } else {
                0
            },
        ];
        terms.extend([most_at_once(|c| c.bytes), sum(&working)]);
        Some(sum(&terms)).filter(|&bytes| bytes < usize::MAX)
    }

    /// Adds compressed rows of tiles of level `i` to its spools.
    fn add(&mut self, i: usize, rows: usize, (img, mask): CogImages) -> Result<()> {
        img.report_clipping();
        let l = &mut self.levels[i];
        let per_plane = l.cols.div_ceil(self.tile) * rows.div_ceil(self.tile);
        // Each plane's chunks (in plane order), framed by their leaders and trailers as they are
        // written, not copied whole first.
        let mut chunks = img.chunks.iter();
        for (plane, spool) in l.spools.iter_mut().enumerate() {
            let mut out = std::io::BufWriter::with_capacity(1 << 16, spool);
            for c in chunks.by_ref().take(per_plane) {
                out.write_all(&chunk_leader(c))?;
                out.write_all(c)?;
                out.write_all(&chunk_trailer(c))?;
                l.sizes[plane].push(c.len() as u64);
            }
            out.into_inner().map_err(std::io::IntoInnerError::into_error)?;
        }
        if l.tags.is_empty() {
            l.tags = img.tags;
        }
        if let (Some(out), Some(mask)) = (&mut l.mask, mask) {
            for chunk in mask.chunks {
                out.spool.write_all(&chunk_leader(&chunk))?;
                out.spool.write_all(&chunk)?;
                out.spool.write_all(&chunk_trailer(&chunk))?;
                out.sizes.push(chunk.len() as u64);
            }
            if out.tags.is_empty() {
                out.tags = mask.tags;
            }
        }
        Ok(())
    }

    /// Lays the file out and closes it.
    ///
    /// # Errors
    /// When rows are missing, rows failed to be written ([`Self::failed`]), or the file cannot be
    /// written.
    pub(crate) fn finish(mut self) -> Result<()> {
        self.refuse_failed()?;
        let (got, rows) = (self.levels[0].received, self.levels[0].rows);
        if got != rows {
            return Err(Error::InvalidArgument(format!("{got} rows written of {rows}")));
        }
        // The last block rows of each level, which give the next level its last rows.
        #[cfg(not(target_family = "wasm"))]
        for b in 0..self.builds.len() {
            while self.land_block_row(b, true)? {}
        }
        #[cfg(not(target_family = "wasm"))]
        while !self.in_flight.is_empty() {
            self.land()?;
        }
        let mut dirs = Vec::with_capacity(self.levels.len());
        for (i, l) in self.levels.iter_mut().enumerate() {
            let mut tags = std::mem::take(&mut l.tags);
            for (t, v) in tags.iter_mut() {
                if *t == 257 {
                    *v = Value::Longs(vec![l.rows as u32]);
                }
            }
            if i > 0 {
                tags.push((254, Value::Longs(vec![1])));
            }
            // Georeferencing on the full-resolution image; nodata on every image, as GDAL does,
            // but not on the overviews of 64-bit integer rasters (see [`overview_nodata`]).
            let ovr_nodata = overview_nodata(self.data_type);
            tags.extend(self.geo.iter().filter(|(t, _)| i == 0 || (*t == 42113 && ovr_nodata)).cloned());
            if let Some(cm) = &self.color_map {
                tags.push((320, cm.clone()));
            }
            tags.sort_by_key(|(t, _)| *t);
            dirs.push(Directory { tags, sizes: l.sizes.concat(), tiled: true });
            if let Some(mask) = &mut l.mask {
                let mut tags = std::mem::take(&mut mask.tags);
                for (tag, value) in &mut tags {
                    match *tag {
                        254 => *value = Value::Longs(vec![if i == 0 { 4 } else { 5 }]),
                        257 => *value = Value::Longs(vec![l.rows as u32]),
                        _ => {}
                    }
                }
                dirs.push(Directory { tags, sizes: std::mem::take(&mut mask.sizes), tiled: true });
            }
        }
        let data_len: u64 = dirs.iter().flat_map(|d| &d.sizes).map(|s| s + 8).sum();
        let big = self.bigtiff || data_len > (u32::MAX as u64) - (1 << 24);
        self.file.write_all(&directories(dirs, big, true))?;
        use std::io::{Seek, SeekFrom};
        for i in data_order(self.levels.len(), true) {
            if let Some(mask) = &mut self.levels[i].mask {
                mask.spool.seek(SeekFrom::Start(0))?;
                std::io::copy(&mut mask.spool, &mut self.file)?;
            }
            for spool in &mut self.levels[i].spools {
                spool.seek(SeekFrom::Start(0))?;
                std::io::copy(spool, &mut self.file)?;
            }
            // The level's temporary files are closed as soon as they are copied.
            self.levels[i].spools.clear();
        }
        self.file.flush()?;
        self.file.get_mut().finish()?;
        Ok(())
    }
}

/// GDAL's structural metadata for COGs, written just after the header.
const COG_GHOST: &str = "LAYOUT=IFDS_BEFORE_DATA\nBLOCK_ORDER=ROW_MAJOR\nBLOCK_LEADER=SIZE_AS_UINT4\nBLOCK_TRAILER=LAST_4_BYTES_REPEATED\nKNOWN_INCOMPATIBLE_EDITION=NO\n ";

/// Encodes an IFD starting at file offset `start`: its entries, the offset of the next IFD, then
/// the values that do not fit in the entries.
fn encode_ifd(tags: &[(u16, Value)], start: u64, next: u64, big: bool) -> Vec<u8> {
    let (entry_len, count_len, inline_len) = if big { (20u64, 8u64, 8usize) } else { (12, 2, 4) };
    let next_len = if big { 8 } else { 4 };
    let mut out = Vec::new();
    let n = tags.len() as u64;
    if big {
        out.extend(n.to_le_bytes());
    } else {
        out.extend((n as u16).to_le_bytes());
    }
    let mut extra_pos = start + count_len + entry_len * n + next_len;
    let mut extra = Vec::new();
    for (tag, v) in tags {
        let (typ, count, bytes) = v.encode(big);
        out.extend(tag.to_le_bytes());
        out.extend(typ.to_le_bytes());
        if big {
            out.extend(count.to_le_bytes());
        } else {
            out.extend((count as u32).to_le_bytes());
        }
        if bytes.len() <= inline_len {
            let mut inline = bytes.clone();
            inline.resize(inline_len, 0);
            out.extend(inline);
        } else {
            if big {
                out.extend(extra_pos.to_le_bytes());
            } else {
                out.extend((extra_pos as u32).to_le_bytes());
            }
            extra.extend(&bytes);
            if bytes.len() % 2 == 1 {
                extra.push(0);
            }
            extra_pos += (bytes.len() as u64).next_multiple_of(2);
        }
    }
    if big {
        out.extend(next.to_le_bytes());
    } else {
        out.extend((next as u32).to_le_bytes());
    }
    out.extend(extra);
    out
}

/// One image's directory before it is laid out: its tags without the chunk offsets and byte
/// counts, the sizes of its chunks, and whether they are tiles.
struct Directory {
    tags: Vec<(u16, Value)>,
    sizes: Vec<u64>,
    tiled: bool,
}

/// Lays the file out: header, (COG) structural metadata, every IFD, then the image data.
fn assemble(images: Vec<Image>, big: bool, cog: bool, w: &mut impl Write) -> std::io::Result<()> {
    let mut chunks = Vec::with_capacity(images.len());
    let dirs = images
        .into_iter()
        .map(|img| {
            let sizes = img.chunks.iter().map(|c| c.len() as u64).collect();
            chunks.push(img.chunks);
            Directory { tags: img.tags, sizes, tiled: img.tiled }
        })
        .collect();
    w.write_all(&directories(dirs, big, cog))?;
    for i in data_order(chunks.len(), cog) {
        for c in &chunks[i] {
            if cog {
                w.write_all(&chunk_leader(c))?;
                w.write_all(c)?;
                w.write_all(&chunk_trailer(c))?;
            } else {
                w.write_all(c)?;
            }
        }
    }
    Ok(())
}

/// The order the images' data is written in: COGs store the smallest overview first.
fn data_order(images: usize, cog: bool) -> Vec<usize> {
    if cog { (0..images).rev().collect() } else { (0..images).collect() }
}

/// A COG chunk's leader: its size.
fn chunk_leader(c: &[u8]) -> [u8; 4] {
    (c.len() as u32).to_le_bytes()
}

/// A COG chunk's trailer: its last 4 bytes, zero-padded at the front.
fn chunk_trailer(c: &[u8]) -> [u8; 4] {
    let mut tail = [0u8; 4];
    let n = c.len().min(4);
    tail[4 - n..].copy_from_slice(&c[c.len() - n..]);
    tail
}

/// The start of a TIFF file: the header, (COG) structural metadata and every image's IFD, with
/// the offsets of the image data that follows, laid out in [`data_order`] (with each COG chunk's
/// leader and trailer).
fn directories(mut images: Vec<Directory>, big: bool, cog: bool) -> Vec<u8> {
    let mut out = Vec::new();
    if big {
        out.extend(b"II");
        out.extend(43u16.to_le_bytes());
        out.extend(8u16.to_le_bytes());
        out.extend(0u16.to_le_bytes());
        out.extend(0u64.to_le_bytes()); // first IFD offset, patched below
    } else {
        out.extend(b"II");
        out.extend(42u16.to_le_bytes());
        out.extend(0u32.to_le_bytes());
    }
    if cog {
        out.extend(format!("GDAL_STRUCTURAL_METADATA_SIZE={:06} bytes\n", COG_GHOST.len()).as_bytes());
        out.extend(COG_GHOST.as_bytes());
    }

    // IFD sizes: the entries (with the chunk offset and byte count tags added), then each tag's
    // out-of-line values.
    let (entry_len, count_len, inline_len) = if big { (20u64, 8u64, 8usize) } else { (12, 2, 4) };
    for img in images.iter_mut() {
        let n = img.sizes.len();
        let (off_tag, cnt_tag) = if img.tiled { (324, 325) } else { (273, 279) };
        img.tags.push((off_tag, Value::Offsets(vec![0; n])));
        img.tags.push((cnt_tag, Value::Offsets(img.sizes.clone())));
        img.tags.sort_by_key(|(t, _)| *t);
    }
    let next_len = if big { 8 } else { 4 };
    let ifd_size = |img: &Directory| -> u64 {
        let extra: u64 = img
            .tags
            .iter()
            .map(|(_, v)| {
                let (_, _, b) = v.encode(big);
                if b.len() > inline_len { (b.len() as u64).next_multiple_of(2) } else { 0 }
            })
            .sum();
        count_len + entry_len * img.tags.len() as u64 + next_len + extra
    };

    // IFD offsets, then data offsets.
    out.resize(out.len().next_multiple_of(2), 0);
    let mut pos = out.len() as u64;
    let mut ifd_offsets = Vec::with_capacity(images.len());
    for img in &images {
        ifd_offsets.push(pos);
        pos += ifd_size(img);
    }
    let mut chunk_offsets: Vec<Vec<u64>> = images.iter().map(|i| vec![0; i.sizes.len()]).collect();
    for i in data_order(images.len(), cog) {
        for (j, size) in images[i].sizes.iter().enumerate() {
            // A chunk left out (`SPARSE_OK`) has no bytes, and its offset is 0.
            if *size == 0 && !cog {
                continue;
            }
            if cog {
                pos += 4; // leader: the chunk size
            }
            chunk_offsets[i][j] = pos;
            pos += size;
            if cog {
                pos += 4; // trailer: the chunk's last 4 bytes
            }
        }
    }

    // Patch the first IFD offset.
    if big {
        out[8..16].copy_from_slice(&ifd_offsets[0].to_le_bytes());
    } else {
        out[4..8].copy_from_slice(&(ifd_offsets[0] as u32).to_le_bytes());
    }

    for (i, img) in images.iter_mut().enumerate() {
        let off_tag = if img.tiled { 324 } else { 273 };
        if let Some((_, v)) = img.tags.iter_mut().find(|(t, _)| *t == off_tag) {
            *v = Value::Offsets(chunk_offsets[i].clone());
        }
        let start = out.len() as u64;
        debug_assert_eq!(start, ifd_offsets[i]);
        out.extend(encode_ifd(&img.tags, start, ifd_offsets.get(i + 1).copied().unwrap_or(0), big));
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn lzw_scratch_estimate_covers_retained_witness_allocations() {
        let mut encoder = super::super::tiff_lzw::Encoder::new();
        for raw in [64, 256, 4096, 16_384] {
            encoder.encode(&vec![0; raw]).unwrap();
            assert!(encoder.allocated_scratch_bytes() > 64 << 10);
            let peak = encoder.allocated_scratch_bytes() + super::super::tiff_lzw::Encoder::FALLBACK_SCRATCH_BYTES;
            assert!(super::super::tiff_lzw::Encoder::SCRATCH_BYTES >= peak);
            let (_, scratch) = encoder_bytes(Compression::Lzw, raw);
            assert!(scratch >= peak, "{raw}: {scratch} < {peak}");
        }
    }

    #[test]
    fn lzw_matches_for_predicted_samples_and_byte_orders() {
        let mut enc = super::super::tiff_lzw::Encoder::new();
        for size in [1, 2, 4] {
            for big in [false, true] {
                for predictor in [1, 2, 3] {
                    if predictor == 3 && size != 4 {
                        continue;
                    }
                    for rows in [1, 2, 15, 16, 17, 255, 256, 257] {
                        let mut data: Vec<u8> = (0..37 * rows)
                            .flat_map(|i| {
                                let bits = [0x7fc01234u32, 0x7fa00001, 0x80000000, 0x7f800000, i as u32][i % 5];
                                let b = if big { bits.to_be_bytes() } else { bits.to_le_bytes() };
                                b[..size].to_vec()
                            })
                            .collect();
                        // Encoder sees bytes only; both sample byte orders are
                        // exercised. The writer's predictor itself is LE-only.
                        apply_predictor(&mut data, predictor, 37, 1, size);
                        let actual = enc.encode(&data).unwrap();
                        let expected = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(&data).unwrap();
                        assert_eq!(actual, expected);
                        assert_eq!(weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).decode(&actual).unwrap(), data);
                    }
                }
            }
        }
    }

    #[test]
    fn lzw_reused_streams_match_fresh_tiff_size_switch() {
        let mut scratch = None;
        let c = ChunkInfo { w: 1, h: 1, spp: 1, t: DataType::U8, valid: (1, 1) };
        let p = CodecParams::default();
        let mut rng = 0x12345678u32;
        let random: Vec<u8> = (0..262144)
            .map(|_| {
                rng ^= rng << 13;
                rng ^= rng >> 17;
                rng ^= rng << 5;
                rng as u8
            })
            .collect();
        let float_bytes: Vec<u8> =
            [0x7fc01234u32, 0x7fa00001, 0x80000000, 0, 0x7f800000, 0xff800000].into_iter().flat_map(u32::to_le_bytes).collect();
        let cases = [vec![], vec![0], vec![255; 217724], random.clone(), float_bytes, (0..65536).map(|i| (i % 251) as u8).collect()];
        // Alternate lengths, dictionary saturation/clears, empty and repeated data.
        for _ in 0..3 {
            for data in &cases {
                let expected = weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(data).unwrap();
                let actual = Compression::Lzw.compress_reusing(data.clone(), &c, &p, &mut scratch).unwrap();
                assert_eq!(actual, expected);
                assert_eq!(weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).decode(&actual).unwrap(), *data);
            }
        }
        let early = Compression::Lzw.compress(random.clone(), &c, &p).unwrap();
        let late = weezl::encode::Encoder::new(weezl::BitOrder::Msb, 8).encode(&random).unwrap();
        assert_ne!(early, late, "fixture must exercise TIFF's early code-width switch");
        for compression in [Compression::None, Compression::Deflate, Compression::PackBits, Compression::Zstd] {
            let mut unused = None;
            assert_eq!(
                compression.compress(random.clone(), &c, &p).unwrap(),
                compression.compress_reusing(random.clone(), &c, &p, &mut unused).unwrap()
            );
            assert!(unused.is_none());
        }
    }

    use super::*;

    #[test]
    fn engineering_geo_tags_retain_wkt_even_with_a_projected_identifier() {
        // Metadata preservation is distinct from GDAL's WKT normalization (reference: 3.12.1).
        let wkt = r#"local_cs ("site grid",UNIT["metre",1],AUTHORITY["EPSG","32615"])"#;
        let crs = crate::crs::SpatialRef::from_wkt(wkt).to_crs_info();
        let r = Raster::from_data(wbraster::RasterConfig { cols: 2, rows: 2, crs, ..Default::default() }, vec![1.0; 4]).unwrap();
        let tags = geo_tags(&r, &Default::default(), &[], |_, _| false).unwrap();
        let Some(Value::Shorts(keys)) = tags.iter().find(|(tag, _)| *tag == 34735).map(|(_, value)| value) else { panic!() };
        let Some(Value::Ascii(ascii)) = tags.iter().find(|(tag, _)| *tag == 34737).map(|(_, value)| value) else { panic!() };
        let (entries, remainder) = keys[4..].as_chunks::<4>();
        assert!(remainder.is_empty());
        assert!(entries.contains(&[1024, 0, 1, 32767]));
        assert!(entries.iter().any(|key| key[0] == 1026));
        assert!(!entries.iter().any(|key| matches!(key[0], 2048 | 3072)));
        let crs = crate::geotiff::crs_from_geokeys(keys, None, Some(ascii.as_bytes()));
        assert_eq!(crs.wkt.as_deref(), Some(wkt));
    }

    /// Writing over a larger file gives the file a fresh write gives, through every writer, and
    /// keeps the file itself (a hard link to it sees the new content).
    #[test]
    fn rewriting_a_larger_file_leaves_only_the_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let raster = |n: usize| {
            let cfg = wbraster::RasterConfig { cols: n, rows: n, bands: 1, data_type: wbraster::DataType::F32, ..Default::default() };
            let data = wbraster::raster::RasterData::F32((0..n * n).map(|v| v as f32).collect());
            crate::RasterDataset::with_driver(Raster::from_data_native(cfg, data).unwrap(), crate::formats::RasterDriver::GTiff, false)
        };
        let small = dir.path().join("small.tif");
        crate::write_raster_dataset(&raster(20), small.to_str().unwrap(), &Default::default()).unwrap();
        for driver in [crate::formats::RasterDriver::GTiff, crate::formats::RasterDriver::Cog] {
            let options = crate::RasterWriteOptions { driver: Some(driver), ..Default::default() };
            for streamed in [false, true] {
                let write = |path: &std::path::Path| {
                    if streamed {
                        crate::open_raster_lazy(small.to_str().unwrap()).unwrap().write(path.to_str().unwrap(), &options).unwrap();
                    } else {
                        crate::write_raster_dataset(&raster(20), path.to_str().unwrap(), &options).unwrap();
                    }
                };
                let (path, link, fresh) = (dir.path().join("out.tif"), dir.path().join("link.tif"), dir.path().join("fresh.tif"));
                for p in [&path, &link, &fresh] {
                    let _ = std::fs::remove_file(p);
                }
                crate::write_raster_dataset(&raster(300), path.to_str().unwrap(), &options).unwrap();
                std::fs::hard_link(&path, &link).unwrap();
                write(&path);
                write(&fresh);
                let expected = std::fs::read(&fresh).unwrap();
                assert_eq!(std::fs::read(&path).unwrap(), expected, "{driver:?}, streamed {streamed}");
                assert_eq!(std::fs::read(&link).unwrap(), expected, "{driver:?}, streamed {streamed}");
            }
        }
    }

    /// TIFF row padding and MSB order, independently cross-read with GDAL 3.13.3.
    #[test]
    fn packed_rows_use_msb_order_and_restart_at_byte_boundaries() {
        assert_eq!(pack_unsigned_rows(&[0, 1, 1, 0, 1, 0, 0, 1, 1], 9, 1, 1), (vec![0x69, 0x80], false));
        let input: Vec<u8> = [0xabcu16, 0x123, 0xdef, 0x456, 0x789, 0xabc].into_iter().flat_map(u16::to_le_bytes).collect();
        assert_eq!(pack_unsigned_rows(&input, 3, 2, 12), (vec![0xab, 0xc1, 0x23, 0xde, 0xf0, 0x45, 0x67, 0x89, 0xab, 0xc0], false));
        assert_eq!(pack_unsigned_rows(&u32::MAX.to_le_bytes(), 1, 4, 31), (vec![255, 255, 255, 254], true));
    }

    /// Packed unsigned sample widths round-trip through the independent TIFF decoder.
    /// GDAL 3.13.3 checks these layouts through the binding conformance comparisons.
    #[test]
    fn packed_unsigned_tiffs_roundtrip_tiles_strips_and_band_planes() {
        for bits in 1u16..=32 {
            let data_type = if bits <= 8 {
                DataType::U8
            } else if bits <= 16 {
                DataType::U16
            } else {
                DataType::U32
            };
            let maximum = (1u64 << bits) - 1;
            let values: Vec<f64> = (0..37 * 19 * 3).map(|i| (i as u64 * 2654435761 % (maximum + 1)) as f64).collect();
            let cfg = wbraster::RasterConfig { cols: 37, rows: 19, bands: 3, data_type, ..Default::default() };
            let raster = Raster::from_data_native(cfg, RasterData::from_f64_vec(data_type, values.clone())).unwrap();
            for tile_size in [None, Some(16)] {
                for planar in [false, true] {
                    let options =
                        TiffOptions { nbits: Some(bits), tile_size, planar, compression: Compression::Deflate, ..Default::default() };
                    let bytes = encode(&raster, &options).unwrap();
                    let mut read = crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap();
                    assert_eq!(read.sample_bits(), Some(bits));
                    assert_eq!(
                        read.read_window(crate::Window::full(37, 19), &Default::default()).unwrap().raster.data.to_f64_vec(),
                        values
                    );
                }
            }
        }
    }

    /// GDAL 3.13.3's bilevel clipping is silent; other packed domains report on the caller.
    #[test]
    fn packed_clipping_warnings_stay_on_the_callers_thread() {
        let dir = tempfile::tempdir().unwrap();
        for bits in [1, 4] {
            let _ = crate::warnings::take();
            let cfg = wbraster::RasterConfig { cols: 32, rows: 32, bands: 1, data_type: DataType::U8, ..Default::default() };
            let raster = Raster::from_data_native(cfg, RasterData::U8(vec![255; 32 * 32])).unwrap();
            let opts = TiffOptions { cog: true, nbits: Some(bits), tile_size: Some(32), ..Default::default() };
            let target = dir.path().join(format!("clipped-{bits}.tif"));
            let mut writer = CogWriter::create(target.to_str().unwrap(), &raster, &opts).unwrap();
            writer.write_rows(raster.data, 32).unwrap();
            writer.finish().unwrap();
            let warnings = crate::warnings::take();
            assert_eq!(warnings.len(), usize::from(bits > 1), "{warnings:?}");
            let mut result = crate::LazyRaster::open(target.to_str().unwrap()).unwrap();
            assert!(result.read().unwrap().raster.data.to_f64_vec().iter().all(|&v| v == ((1u32 << bits) - 1) as f64));
        }
    }

    /// A shared TIFF handle keeps each stored level's width (GDAL 3.13.3 NBITS).
    #[test]
    fn shared_overview_copies_keep_the_selected_sample_width() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("base.tif");
        let ovr = dir.path().join("base.tif.ovr");
        for (path, size, bits, value) in [(&source, 4, 9, 10), (&ovr, 2, 12, 2000)] {
            let cfg = wbraster::RasterConfig { cols: size, rows: size, bands: 1, data_type: DataType::U16, ..Default::default() };
            let raster = Raster::from_data_native(cfg, RasterData::U16(vec![value; size * size])).unwrap();
            std::fs::write(path, encode(&raster, &TiffOptions { nbits: Some(bits), ..Default::default() }).unwrap()).unwrap();
            if path == &source {
                let mut reader = crate::LazyRaster::open(source.to_str().unwrap()).unwrap();
                assert!(reader.overviews().is_empty());
                for overview in [crate::ops::overview::OverviewSelection::Auto, crate::ops::overview::OverviewSelection::AutoOffset(1)] {
                    let dataset =
                        reader.read_resampled(&crate::ResampledReadOptions { size: (2, 2), overview, ..Default::default() }).unwrap();
                    let options = crate::RasterWriteOptions::default().inherited_from(&dataset, Some(crate::formats::RasterDriver::GTiff));
                    assert!(options.creation_options.iter().any(|(key, value)| key == "NBITS" && value == "9"));
                }
            }
        }
        let mut original = crate::LazyRaster::open(source.to_str().unwrap()).unwrap();
        let mut shared = original.shared();
        for reader in [&mut original, &mut shared] {
            assert_eq!(reader.sample_bits(), Some(9));
            assert_eq!(reader.level_sample_bits(Some(0)), Some(12));
            assert_eq!(reader.widest_sample_bits(), Some(12));
            let dataset =
                reader.read_window(crate::Window::full(2, 2), &crate::WindowOptions { overview: Some(0), ..Default::default() }).unwrap();
            let options = crate::RasterWriteOptions::default().inherited_from(&dataset, Some(crate::formats::RasterDriver::GTiff));
            let options = TiffOptions::from_write_options(&options, false, None).unwrap().unwrap();
            let mut result = crate::LazyRaster::from_tiff_bytes(encode(&dataset.raster, &options).unwrap()).unwrap().unwrap();
            assert_eq!(result.sample_bits(), Some(12));
            assert_eq!(result.read().unwrap().raster.data.to_f64_vec(), vec![2000.0; 4]);
            for overview in [crate::ops::overview::OverviewSelection::Level(0), crate::ops::overview::OverviewSelection::Auto] {
                let dataset = reader.read_resampled(&crate::ResampledReadOptions { size: (2, 2), overview, ..Default::default() }).unwrap();
                let options = crate::RasterWriteOptions::default().inherited_from(&dataset, Some(crate::formats::RasterDriver::GTiff));
                assert!(options.creation_options.iter().any(|(key, value)| key == "NBITS" && value == "12"));
                assert_eq!(dataset.raster.data.to_f64_vec(), vec![2000.0; 4]);
            }
        }
    }

    /// A resampled base image without overviews retains its own width (GDAL 3.13.3 NBITS).
    #[test]
    fn resampled_packed_base_without_overviews_keeps_its_width() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("base.tif");
        let config = wbraster::RasterConfig { cols: 8, rows: 4, bands: 1, data_type: DataType::U16, ..Default::default() };
        let raster = Raster::from_data_native(config, RasterData::U16(vec![300; 32])).unwrap();
        std::fs::write(&path, encode(&raster, &TiffOptions { nbits: Some(9), ..Default::default() }).unwrap()).unwrap();
        let mut native = crate::LazyRaster::open(path.to_str().unwrap()).unwrap();
        let mut sharable = crate::LazyRaster::open(path.to_str().unwrap()).unwrap();
        let mut shared = sharable.shared();
        for reader in [&mut native, &mut shared] {
            assert!(reader.overviews().is_empty());
            for overview in [crate::ops::overview::OverviewSelection::Auto, crate::ops::overview::OverviewSelection::Level(0)] {
                let dataset =
                    reader.read_resampled(&crate::ResampledReadOptions { size: (16, 8), overview, ..Default::default() }).unwrap();
                let options = crate::RasterWriteOptions::default().inherited_from(&dataset, Some(crate::formats::RasterDriver::GTiff));
                let options = TiffOptions::from_write_options(&options, false, None).unwrap().unwrap();
                let mut result = crate::LazyRaster::from_tiff_bytes(encode(&dataset.raster, &options).unwrap()).unwrap().unwrap();
                assert_eq!(result.sample_bits(), Some(9));
                assert_eq!(result.read().unwrap().raster.data.to_f64_vec(), vec![300.0; 128]);
            }
        }
    }

    /// GDAL 3.13.3 ignores an ineffective predictor for uncompressed packed output.
    #[test]
    fn inherited_packed_width_uses_the_effective_predictor() {
        use crate::RasterWriteOptions;
        for (compression, predictor, expected) in
            [("NONE", "2", Some(4)), ("PACKBITS", "2", Some(4)), ("DEFLATE", "2", None), ("LZW", "NO", Some(4))]
        {
            let options = RasterWriteOptions {
                compression: Some(compression.into()),
                creation_options: vec![("PREDICTOR".into(), predictor.into())],
                ..Default::default()
            };
            assert_eq!(options.inherited_sample_bits(crate::formats::RasterDriver::GTiff, DataType::U8, &[Some(4)], &[None]), expected);
            assert_eq!(options.inherited_sample_bits(crate::formats::RasterDriver::GTiff, DataType::U8, &[Some(4)], &[Some(255.0)]), None);
            assert_eq!(
                options.inherited_sample_bits(crate::formats::RasterDriver::GTiff, DataType::U8, &[Some(4), Some(8)], &[None, None]),
                None
            );
        }
    }

    #[test]
    fn packed_inheritance_respects_each_drivers_default_compression() {
        let options = crate::RasterWriteOptions { creation_options: vec![("PREDICTOR".into(), "2".into())], ..Default::default() };
        assert_eq!(options.inherited_sample_bits(crate::formats::RasterDriver::GTiff, DataType::U8, &[Some(4)], &[None]), Some(4));
        assert_eq!(options.inherited_sample_bits(crate::formats::RasterDriver::Cog, DataType::U8, &[Some(4)], &[None]), None);
    }

    /// GDAL 3.13.3 retains the packed width when copying an unchanged unsigned band.
    #[test]
    fn packed_tiff_translates_keep_the_source_width() {
        use crate::ops::translate::{TranslateOptions, translate, translate_lazy, translate_path_to_file};
        let cfg = wbraster::RasterConfig { cols: 37, rows: 19, bands: 1, data_type: DataType::U8, ..Default::default() };
        let raster = Raster::from_data_native(cfg, RasterData::U8((0..37 * 19).map(|i| (i % 16) as u8).collect())).unwrap();
        let bytes = encode(&raster, &TiffOptions { nbits: Some(4), ..Default::default() }).unwrap();
        let mut source = crate::LazyRaster::from_tiff_bytes(bytes.clone()).unwrap().unwrap();
        let whole = source.read_window(crate::Window::full(37, 19), &Default::default()).unwrap();
        let translated = translate(&whole, &TranslateOptions::default()).unwrap();
        let lazy = translate_lazy(&mut source, &TranslateOptions::default(), crate::ops::overview::OverviewSelection::None).unwrap();
        for result in [whole, translated, lazy] {
            let options = crate::RasterWriteOptions::default().inherited_from(&result, Some(crate::formats::RasterDriver::GTiff));
            let options = TiffOptions::from_write_options(&options, false, None).unwrap().unwrap();
            let read = crate::LazyRaster::from_tiff_bytes(encode(&result.raster, &options).unwrap()).unwrap().unwrap();
            assert_eq!(read.sample_bits(), Some(4));
        }
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("source.tif");
        let output = directory.path().join("copy.tif");
        std::fs::write(&input, bytes).unwrap();
        translate_path_to_file(
            input.to_str().unwrap(),
            &Default::default(),
            crate::ops::overview::OverviewSelection::Auto,
            output.to_str().unwrap(),
            &Default::default(),
        )
        .unwrap();
        assert_eq!(crate::LazyRaster::open(output.to_str().unwrap()).unwrap().sample_bits(), Some(4));
    }

    #[test]
    fn pixel_is_point_roundtrip_preserves_corners() {
        let r = Raster::from_data(
            wbraster::RasterConfig {
                cols: 4,
                rows: 3,
                bands: 1,
                x_min: 100.0,
                y_min: 200.0,
                cell_size: 10.0,
                crs: wbraster::CrsInfo::from_epsg(32610),
                metadata: vec![("AREA_OR_POINT".into(), "Point".into())],
                ..Default::default()
            },
            vec![7.0; 12],
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        assert!(gdal_metadata_xml(&r.metadata, &[], &[], (false, false), &[]).is_none());
        let mut unknown = r.clone();
        unknown.crs = Default::default();
        let path = dir.path().join("unknown.tif");
        let error = write(&unknown, path.to_str().unwrap(), &Default::default()).unwrap_err();
        assert!(error.to_string().contains("requires a CRS"));
        assert!(!path.exists());
        unknown.metadata.clear();
        write(&unknown, path.to_str().unwrap(), &Default::default()).unwrap();
        let plain = crate::geotiff::read_geotiff(&path).unwrap();
        assert!(!plain.raster.metadata.iter().any(|(k, _)| k == "AREA_OR_POINT"));
        for gt in [None, Some([100.0, 10.0, 2.0, 230.0, 1.0, -10.0]), Some([100.0, 10.0, 0.0, 200.0, 0.0, 10.0])] {
            for cog in [false, true] {
                let path = dir.path().join("point.tif");
                let options = TiffOptions { cog, geo_transform: gt, ..Default::default() };
                write(&r, path.to_str().unwrap(), &options).unwrap();
                let tags = crate::geotiff::read_geo_tags(&path).unwrap();
                assert!(tags.pixel_is_point);
                assert_eq!(tags.corner_geo_transform().unwrap(), gt.unwrap_or([100.0, 10.0, 0.0, 230.0, 0.0, -10.0]));
                let read = crate::geotiff::read_geotiff(&path).unwrap();
                assert!(read.raster.metadata.iter().any(|(k, v)| k == "AREA_OR_POINT" && v == "Point"));
            }
        }
    }

    /// Each encoder writes a chunk in no larger a buffer than `encoder_bytes` counts for it (the
    /// check of a COG's row calls): samples none compresses, and zeros, in chunks of 256 columns and
    /// a power of two of rows or three times one, in each band count and sample type the encoder
    /// writes, at its default settings and, for JPEG and WebP, at quality 100.
    #[test]
    fn encoders_write_chunks_in_the_buffers_counted_for_them() {
        let mix = |i: u64| {
            let z = (i ^ (i >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            let z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            ((z ^ (z >> 31)) >> 56) as u8
        };
        let lossless_webp = CodecParams { webp_lossless: true, ..Default::default() };
        let ycbcr = CodecParams { jpeg_ycbcr: true, ..Default::default() };
        let finest = CodecParams { jpeg_quality: 100, webp_quality: 100, ..Default::default() };
        let plain = CodecParams::default();
        let (large, small): (&[usize], &[usize]) = (&[256, 768, 2048, 3072], &[64, 192, 512, 768]);
        let mut cases = vec![
            (Compression::None, 1, DataType::U8, plain, large),
            (Compression::PackBits, 1, DataType::U8, plain, large),
            (Compression::Lzw, 1, DataType::U8, plain, large),
            (Compression::Deflate, 1, DataType::U8, plain, large),
            (Compression::Zstd, 1, DataType::U8, plain, large),
            (Compression::Jpeg, 1, DataType::U8, plain, small),
            (Compression::Jpeg, 3, DataType::U8, plain, small),
            (Compression::Jpeg, 3, DataType::U8, ycbcr, small),
            (Compression::Jpeg, 1, DataType::U8, finest, small),
            (Compression::Jpeg, 3, DataType::U8, finest, small),
            (Compression::WebP, 3, DataType::U8, plain, small),
            (Compression::WebP, 4, DataType::U8, plain, small),
            (Compression::WebP, 4, DataType::U8, finest, small),
            (Compression::WebP, 3, DataType::U8, lossless_webp, small),
            (Compression::WebP, 4, DataType::U8, lossless_webp, small),
            (Compression::Jxl, 1, DataType::U8, plain, small),
            (Compression::Jxl, 3, DataType::U8, plain, small),
            (Compression::Jxl, 1, DataType::U16, plain, small),
        ];
        if cfg!(feature = "lerc") {
            cases.extend([(Compression::Lerc, 1, DataType::U8, plain, small), (Compression::Lerc, 1, DataType::U16, plain, small)]);
        }
        for (compression, spp, t, params, heights) in cases {
            for &h in heights {
                let raw = 256 * h * spp * t.size_bytes();
                let noise: Vec<u8> = (0..raw as u64).map(|i| mix(i.wrapping_add(0x9E37_79B9_7F4A_7C15))).collect();
                // JPEG XL's encoder panics on UInt16 samples none compresses: zeros only.
                let skip = usize::from(compression == Compression::Jxl && t == DataType::U16);
                for data in [noise, vec![0u8; raw]].into_iter().skip(skip) {
                    let chunk = ChunkInfo { w: 256, h, spp, t, valid: (256, h) };
                    let out = compression.compress(data, &chunk, &params).unwrap();
                    let counted = encoder_bytes(compression, raw).0;
                    assert!(
                        out.capacity() <= counted,
                        "{compression:?}, {spp} x {t:?}, {params:?}, {raw} bytes: a buffer of {} bytes, {counted} counted",
                        out.capacity()
                    );
                }
            }
        }
    }

    #[test]
    fn packbits_round_trips() {
        let data: Vec<u8> = [vec![7u8; 300], (0..200).map(|i| i as u8).collect(), vec![1, 1, 2, 2, 3]].concat();
        let enc = packbits(&data);
        // Decode.
        let mut out = Vec::new();
        let mut i = 0;
        while i < enc.len() {
            let n = enc[i] as i8;
            i += 1;
            if n >= 0 {
                out.extend_from_slice(&enc[i..i + n as usize + 1]);
                i += n as usize + 1;
            } else if n != -128 {
                out.extend(std::iter::repeat_n(enc[i], (1 - n as i32) as usize));
                i += 1;
            }
        }
        assert_eq!(out, data);
    }

    #[test]
    fn writes_a_cog_that_reads_back() {
        let cfg = wbraster::RasterConfig {
            cols: 70,
            rows: 50,
            bands: 2,
            x_min: 500000.0,
            y_min: 4000000.0,
            cell_size: 10.0,
            nodata: -1.0,
            data_type: DataType::I16,
            crs: wbraster::CrsInfo::from_epsg(32610),
            ..Default::default()
        };
        let data: Vec<i16> = (0..70 * 50 * 2).map(|i| (i % 1000) as i16 - 1).collect();
        let r = Raster::from_data_native(cfg, RasterData::I16(data.clone())).unwrap();
        for (cog, comp, big) in
            [(true, Compression::Deflate, false), (false, Compression::Lzw, true), (false, Compression::PackBits, false)]
        {
            let opts = TiffOptions { compression: comp, cog, tile_size: Some(32), bigtiff: big, nodata: Some(-1.0), ..Default::default() };
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("t.tif");
            write(&r, p.to_str().unwrap(), &opts).unwrap();
            let ds = crate::geotiff::read_geotiff(&p).unwrap();
            assert_eq!(ds.raster.data_i16().unwrap(), &data[..], "{comp:?}");
            assert_eq!(ds.nodata(), Some(-1.0));
            assert_eq!(ds.crs().epsg, Some(32610));
            assert_eq!(ds.geo_transform(), [500000.0, 10.0, 0.0, 4000500.0, 0.0, -10.0]);
        }
    }

    /// A failed GeoTIFF row write retains only the failed block row and refuses further rows
    /// and finish; the unconsumed tail of a large caller slab is never copied.
    #[test]
    fn failed_row_writes_stop_the_writer() {
        let raster = overview_fixture(1, DataType::U8, RasterData::U8((0..65 * 35).map(|i| (i % 251) as u8).collect()));
        let mut header = crate::RasterDataset::new(raster.clone(), false);
        header.raster.data = RasterData::U8(Vec::new());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failed.tif");
        let options = crate::RasterWriteOptions { creation_options: vec![("BLOCKYSIZE".into(), "16".into())], ..Default::default() };
        let mut w = TiffRows::create(path.to_str().unwrap(), &header, &options, None).unwrap();
        w.write_rows(&slice_rows(&raster.data, 35, 0..10, 65, 1), 10, None).unwrap();
        // Past what a classic TIFF addresses: the block row is refused once written.
        let pos = std::mem::replace(&mut w.pos, u64::from(u32::MAX));
        assert!(w.write_rows(&slice_rows(&raster.data, 35, 10..20, 65, 1), 10, None).is_err());
        assert_eq!((w.rows_written(), w.pending_rows, w.pending.len()), (16, 16, 65 * 16));
        // Within the limit again, the writer still refuses: the file may hold part of the row.
        w.pos = pos;
        assert!(w.write_rows(&slice_rows(&raster.data, 35, 20..35, 65, 1), 15, None).is_err());
        assert_eq!(w.rows_written(), 16);
        drop(w);
        // A later block row fails before consuming the last three rows of the caller slab.
        let mut w = TiffRows::create(path.to_str().unwrap(), &header, &options, None).unwrap();
        w.write_rows(&slice_rows(&raster.data, 35, 0..16, 65, 1), 16, None).unwrap();
        w.pos = u64::from(u32::MAX);
        assert!(w.write_rows(&slice_rows(&raster.data, 35, 16..35, 65, 1), 19, None).is_err());
        assert_eq!(w.rows_written(), 32);
        assert!(w.finish().is_err());
    }

    /// Invalid slabs must fail before consuming rows or changing the native overview state.
    #[test]
    fn cog_row_type_errors_leave_the_writer_usable() {
        let raster = overview_fixture(1, DataType::U8, RasterData::U8((0..65 * 35).map(|i| (i % 251) as u8).collect()));
        let dir = tempfile::tempdir().unwrap();
        for method in [Method::Nearest, Method::Average, Method::Mode] {
            let opts = TiffOptions { cog: true, tile_size: Some(16), cog_resampling: Some(method), ..Default::default() };
            let path = dir.path().join("typed.tif");
            let mut writer = CogWriter::create(path.to_str().unwrap(), &raster, &opts).unwrap();
            assert!(matches!(writer.write_rows(RasterData::F32(vec![1.0; 65]), 1), Err(Error::InvalidArgument(_))));
            writer.write_rows(slice_rows(&raster.data, 35, 0..16, 65, 1), 16).unwrap();
            assert!(matches!(writer.write_rows(RasterData::F32(vec![1.0; 65]), 1), Err(Error::InvalidArgument(_))));
            assert!(matches!(writer.write_rows(RasterData::F32(Vec::new()), 0), Err(Error::InvalidArgument(_))));
            // More rows than the raster has left, however many.
            assert!(matches!(writer.write_rows(RasterData::U8(Vec::new()), usize::MAX), Err(Error::InvalidArgument(_))));
            writer.write_rows(slice_rows(&raster.data, 35, 16..35, 65, 1), 19).unwrap();
            writer.finish().unwrap();
            assert_eq!(std::fs::read(path).unwrap(), encode(&raster, &opts).unwrap(), "{method:?}");
        }
    }

    /// What the overview block rows of a COG with a mask being computed on the thread pool keep
    /// counts once in the memory its next rows are checked against: the rows and mask the build
    /// still holds count as its own, not again for the block rows reading them (a call that fits is
    /// not refused for them); rows it has replaced since count while a block row keeps them.
    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn cog_block_rows_being_computed_count_what_they_keep_once() {
        let raster = overview_fixture(1, DataType::U8, RasterData::U8((0..65 * 35).map(|i| (i % 251) as u8).collect()));
        let mut header = crate::RasterDataset::new(raster.clone(), false);
        header.raster.data = RasterData::U8(Vec::new());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held.tif");
        let options = crate::RasterWriteOptions {
            driver: Some(crate::formats::RasterDriver::Cog),
            tile_size: Some(16),
            mask: Some(Vec::new()),
            ..Default::default()
        };
        let valid = vec![255u8; 65 * 35];
        let mut w = GeoTiffWriter::create(path.to_str().unwrap(), &header, &options).unwrap();
        w.write_rows(&slice_rows(&raster.data, 35, 0..16, 65, 1), 16, Some(&valid[..65 * 16])).unwrap();
        let rest = slice_rows(&raster.data, 35, 16..35, 65, 1);
        let job = |holds| BlockRowJob { rows: 0, result: std::sync::mpsc::sync_channel(1).1, holds };
        let huge = std::sync::Arc::new(RasterData::U8(Vec::new()));
        {
            let RowWriter::Cog(c) = &mut w.inner else { unreachable!("a COG written in rows") };
            let cog = c.get_mut().unwrap();
            for b in 0..cog.builds.len() {
                while cog.land_block_row(b, true).unwrap() {}
            }
            let before = cog.call_bytes(65 * 19, 65 * 19, 19, true);
            let bd = &cog.builds[0];
            let shared = [(std::sync::Arc::downgrade(&bd.buf), bd.buf.len()), (std::sync::Arc::downgrade(&bd.mask), bd.mask.len())];
            cog.builds[0].in_flight.push_back(job(shared));
            assert_eq!(cog.call_bytes(65 * 19, 65 * 19, 19, true), before, "the build's own rows and mask counted again");
            // Rows the build has replaced, kept by two block rows: counted once, until they are done.
            let old = std::sync::Arc::new(RasterData::U8(vec![0; 1000]));
            let kept = || (std::sync::Arc::downgrade(&old), 1000);
            cog.builds[0].in_flight.push_back(job([kept(), kept()]));
            cog.builds[0].in_flight.push_back(job([kept(), kept()]));
            assert_eq!(cog.call_bytes(65 * 19, 65 * 19, 19, true), before.map(|b| b + 1000));
            drop(old);
            assert_eq!(cog.call_bytes(65 * 19, 65 * 19, 19, true), before);
            // Kept rows more than the address space holds refuse the call.
            let weak = std::sync::Arc::downgrade(&huge);
            cog.builds[0].in_flight.push_back(job([(weak.clone(), usize::MAX), (weak, 0)]));
        }
        assert!(matches!(w.write_rows(&rest, 19, Some(&valid[65 * 16..])), Err(Error::ResourceLimit(_))));
        drop(huge);
        {
            let RowWriter::Cog(c) = &mut w.inner else { unreachable!("a COG written in rows") };
            c.get_mut().unwrap().builds[0].in_flight.clear();
        }
        w.write_rows(&rest, 19, Some(&valid[65 * 16..])).unwrap();
        w.finish().unwrap();
    }

    /// What `push` records for a block row it starts on the thread pool (the rows and mask it reads,
    /// with their bytes) is what the next call's check counts once the level has replaced them by
    /// appending rows, while the block row is still being computed (held here until the check is
    /// read). The test above gives the check block rows it makes itself. A thread holds the block
    /// rows, and lets them go after 60 s at most: a call that waits for the one held fails the test
    /// rather than hanging it.
    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn a_block_row_being_computed_counts_the_rows_it_keeps_in_the_next_call() {
        let (cols, rows) = (50, 600);
        let data = RasterData::U16((0..cols * rows).map(|i| (i % 1000) as u16).collect());
        let config = wbraster::RasterConfig { cols, rows, bands: 1, data_type: DataType::U16, ..Default::default() };
        let raster = Raster::from_data_native(config, data.clone()).unwrap();
        let mask: Vec<u8> = (0..cols * rows).map(|i| if i % 7 == 0 { 0 } else { 255 }).collect();
        let opts = TiffOptions { cog: true, tile_size: Some(64), mask: Some(mask.clone()), ..Default::default() };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("held.tif");
        let mut cog = CogWriter::create(path.to_str().unwrap(), &raster, &opts).unwrap();
        let give = |cog: &mut CogWriter, r: std::ops::Range<usize>| {
            let values = slice_rows(&data, rows, r.clone(), cols, 1);
            cog.write_rows_masked(values, r.len(), Some(mask[r.start * cols..r.end * cols].to_vec())).unwrap();
        };
        // The source rows of the first overview's first two block rows.
        let plan = cog.builds[0].plan.clone();
        let (n, first) = plan.rows(0);
        let second = plan.rows(n).1;
        let hold = cog.hold.clone();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (locked, is_locked) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _held = hold.write().unwrap();
            locked.send(()).unwrap();
            // Whether the block rows were let go because the test did not release them in time.
            matches!(released.recv_timeout(std::time::Duration::from_secs(60)), Err(std::sync::mpsc::RecvTimeoutError::Timeout))
        });
        is_locked.recv().unwrap();
        // The first block row starts, reading the level's rows, then the level appends the next ones
        // to the rows later blocks read, in a new buffer: the block row keeps the first rows alone.
        give(&mut cog, 0..first.end);
        give(&mut cog, first.end..second.end - 1);
        assert_eq!(cog.builds[0].in_flight.len(), 1);
        let kept = first.end * cols * (2 + 1);
        let next = rows - (second.end - 1);
        let check = |cog: &CogWriter| cog.call_bytes(next * cols * 2, next * cols, next, false);
        let with = check(&cog);
        let jobs = std::mem::take(&mut cog.builds[0].in_flight);
        let without = check(&cog);
        cog.builds[0].in_flight = jobs;
        // Released, unless the thread holding the block rows already let them go.
        let _ = release.send(());
        assert!(!holder.join().unwrap(), "a call waited for the block row held, let go after 60 s");
        assert_eq!(with, without.map(|bytes| bytes + kept), "{} rows of values and mask kept", first.end);
        give(&mut cog, second.end - 1..rows);
        cog.finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), encode(&raster, &opts).unwrap());
    }

    /// A row of tiles that fails to compress fails the writer, as a block row that fails to be
    /// written fails a GeoTIFF's: the call that finds it returns its error (JPEG XL's, its row of
    /// tiles taken from the queue, not "a COG tile compression stopped"), then every later call,
    /// with rows or none, and `finish` are refused, before any is checked for memory, and no file
    /// missing those tiles is laid out: the output is left empty. A call refused for memory before,
    /// which takes no rows, is no failure: made again, it is taken. The failure is found by a call
    /// polling the rows of tiles compressed before it is checked, or by the rows taken once one is
    /// admitted waiting for the oldest when a fourth would be compressed. (JPEG XL is written for
    /// Byte and UInt16 samples: the writer of a Float32 COG takes it, and each of its tiles refuses
    /// it.)
    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn a_tile_compression_that_fails_fails_the_writer() {
        let (cols, rows) = (32, 64);
        let config = wbraster::RasterConfig { cols, rows, bands: 1, data_type: DataType::F32, ..Default::default() };
        let mut header =
            crate::RasterDataset::new(Raster::from_data_native(config, RasterData::F32(vec![0.5; cols * rows])).unwrap(), false);
        header.raster.data = RasterData::F32(Vec::new());
        let options = crate::RasterWriteOptions {
            driver: Some(crate::formats::RasterDriver::Cog),
            compression: Some("JXL".into()),
            tile_size: Some(16),
            mask: Some(Vec::new()),
            ..Default::default()
        };
        fn cog(w: &mut GeoTiffWriter) -> &mut CogWriter {
            let RowWriter::Cog(c) = &mut w.inner else { unreachable!("a COG written in rows") };
            c.get_mut().unwrap()
        }
        let slab = || RasterData::F32(vec![0.5; cols * 16]);
        let (valid, none) = (vec![255u8; cols * 16], RasterData::F32(Vec::new()));
        let refused = |r: Result<()>| matches!(r, Err(Error::InvalidData(m)) if m.contains("failed to be written"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("failed.tif");
        let mut w = GeoTiffWriter::create(path.to_str().unwrap(), &header, &options).unwrap();
        // Refused for kept rows of more than the address space holds: no row taken.
        let huge = std::sync::Arc::new(RasterData::F32(Vec::new()));
        let weak = std::sync::Arc::downgrade(&huge);
        let job = BlockRowJob { rows: 0, result: std::sync::mpsc::sync_channel(1).1, holds: [(weak.clone(), usize::MAX), (weak, 0)] };
        cog(&mut w).builds[0].in_flight.push_back(job);
        assert!(matches!(w.write_rows(&slab(), 16, Some(&valid)), Err(Error::ResourceLimit(_))));
        assert_eq!(w.rows_written(), 0);
        cog(&mut w).builds[0].in_flight.clear();
        w.write_rows(&slab(), 16, Some(&valid)).unwrap();
        // Calls of no rows, until one finds the row of tiles compressed.
        let failed = (0..10_000)
            .find_map(|_| {
                let result = w.write_rows(&none, 0, Some(&[])).err();
                if result.is_none() {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
                result
            })
            .expect("the row of tiles is compressed within 10 s");
        assert!(failed.to_string().contains("JPEG XL is written for Byte and UInt16 data"), "{failed}");
        assert!(cog(&mut w).in_flight.is_empty(), "the failed row of tiles is left in the queue");
        assert!(refused(w.write_rows(&none, 0, Some(&[]))));
        assert!(refused(w.write_rows(&slab(), 16, Some(&valid))));
        assert_eq!(w.rows_written(), 16);
        assert!(refused(w.finish()));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0, "the output is not left empty");
        // Rows taken without polling (as a call takes them once admitted, while the rows of tiles
        // before are still being compressed): the call that waits for the oldest row of tiles finds it.
        let mut w = GeoTiffWriter::create(path.to_str().unwrap(), &header, &options).unwrap();
        let failed = (0..rows / 16).find_map(|_| cog(&mut w).take_admitted(slab(), 16, Some(valid.clone())).err());
        let failed = failed.expect("a fourth row of tiles waits for the first");
        assert!(failed.to_string().contains("JPEG XL is written for Byte and UInt16 data"), "{failed}");
        assert!(refused(cog(&mut w).write_rows_masked(none.clone(), 0, Some(Vec::new()))));
        // Refused as failed before it is checked for memory, on every target: not for the kept rows
        // of more than the address space holds.
        let weak = std::sync::Arc::downgrade(&huge);
        let job = BlockRowJob { rows: 0, result: std::sync::mpsc::sync_channel(1).1, holds: [(weak.clone(), usize::MAX), (weak, 0)] };
        cog(&mut w).builds[0].in_flight.push_back(job);
        assert!(refused(cog(&mut w).write_rows_masked(slab(), 16, Some(valid.clone()))));
        assert!(refused(w.finish()));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0, "the output is not left empty");
    }

    /// A masked COG's call is checked for the rows of tiles it compresses, as `push` makes them
    /// ready: whole rows of tiles, or all the rows with the last ones. A row given to a 1 x 4096
    /// COG in 4096-pixel tiles is only held: it is not checked for the padded 4096 x 4096 tile and
    /// its compression, which the last row is.
    #[test]
    fn a_masked_cog_call_is_checked_for_the_tiles_it_compresses() {
        let rows = 4096;
        let config = wbraster::RasterConfig { cols: 1, rows, bands: 1, data_type: DataType::U8, ..Default::default() };
        let raster = Raster::from_data_native(config, RasterData::U8(vec![7; rows])).unwrap();
        let opts = TiffOptions { cog: true, tile_size: Some(4096), mask: Some(vec![255; rows]), ..Default::default() };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("column.tif");
        let mut cog = CogWriter::create(path.to_str().unwrap(), &raster, &opts).unwrap();
        let tile = 4096 * 4096;
        let first = cog.call_bytes(1, 1, 1, false).unwrap();
        cog.write_rows_masked(RasterData::U8(vec![7; rows - 1]), rows - 1, Some(vec![255; rows - 1])).unwrap();
        let last = cog.call_bytes(1, 1, 1, false).unwrap();
        assert!(last >= first + 2 * tile, "the first row checked for {first} bytes, the last for {last}");
        cog.write_rows_masked(RasterData::U8(vec![7]), 1, Some(vec![255])).unwrap();
        cog.finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), encode(&raster, &opts).unwrap());
    }

    /// A call giving the writer its rows, as the operations writing a COG strip by strip give
    /// theirs, is checked before it takes them, as a call lending them is: refused for memory
    /// (`ResourceLimit`), it takes no rows, and made again, it is taken. It is checked for the bytes
    /// a call lending the same rows is, less the copy of them that call makes, which the writer of
    /// given rows does not (the peak test of `declared_size.rs` measures the lending calls): each byte
    /// given counts for the rows and for the overview build's copy of them. (Measured, giving calls
    /// whose rows were not counted exceeded their check.)
    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn a_cog_row_call_giving_its_rows_is_checked_before_it_takes_them() {
        let raster = overview_fixture(1, DataType::U8, RasterData::U8((0..65 * 35).map(|i| (i % 251) as u8).collect()));
        let mask: Vec<u8> = (0..65 * 35).map(|i| if i % 7 == 0 { 0 } else { 255 }).collect();
        let opts = TiffOptions { cog: true, tile_size: Some(16), mask: Some(mask.clone()), ..Default::default() };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("given.tif");
        let mut cog = CogWriter::create(path.to_str().unwrap(), &raster, &opts).unwrap();
        let give = |r: std::ops::Range<usize>| (slice_rows(&raster.data, 35, r.clone(), 65, 1), mask[r.start * 65..r.end * 65].to_vec());
        let (rows, rows_mask) = give(0..16);
        cog.write_rows_masked(rows, 16, Some(rows_mask)).unwrap();
        let (values, masks) = (65 * 19, 65 * 19);
        let given = cog.call_bytes(values, masks, 19, false).unwrap();
        assert_eq!(cog.call_bytes(values, masks, 19, true), Some(given + values + masks));
        // Each byte of the rows given counts for the rows and for the copy the overview build reading
        // them makes.
        assert_eq!(cog.call_bytes(2 * values, 2 * masks, 19, false), Some(given + 2 * (values + masks)));
        // Kept rows of more than the address space holds refuse the next call.
        let huge = std::sync::Arc::new(RasterData::U8(Vec::new()));
        let weak = std::sync::Arc::downgrade(&huge);
        let job = BlockRowJob { rows: 0, result: std::sync::mpsc::sync_channel(1).1, holds: [(weak.clone(), usize::MAX), (weak, 0)] };
        cog.builds[0].in_flight.push_back(job);
        let (rest, rest_mask) = give(16..35);
        assert!(matches!(cog.write_rows_masked(rest.clone(), 19, Some(rest_mask.clone())), Err(Error::ResourceLimit(_))));
        assert_eq!(cog.rows_written(), 16);
        cog.builds[0].in_flight.pop_back();
        cog.write_rows_masked(rest, 19, Some(rest_mask)).unwrap();
        cog.finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), encode(&raster, &opts).unwrap());
    }

    /// A call giving the writer its rows is checked once the rows of tiles compressed since the last
    /// call are spooled, as a call lending them is: they no longer count. Counted, they refused a call
    /// that fits once they are, and each time it was made again, as nothing spooled them: an 8192 x
    /// 2048 masked Byte COG in 512-pixel tiles, uncompressed and without overviews, given four slabs
    /// of 512 rows, the first three compressed by the time the fourth is given, under memory between
    /// the bytes counted with and without them. The file is the one written whole.
    #[test]
    #[cfg(all(target_os = "linux", target_pointer_width = "64"))]
    fn a_cog_row_call_giving_its_rows_is_checked_once_the_tiles_compressed_are_spooled() {
        crate::par::alone(120, || {
            let (cols, rows, slab) = (8192, 2048, 512);
            let values: Vec<u8> = (0..cols * rows).map(|i| (i % 251) as u8).collect();
            let mask: Vec<u8> = (0..cols * rows).map(|i| if i % 7 == 0 { 0 } else { 255 }).collect();
            let config = wbraster::RasterConfig { cols, rows, bands: 1, data_type: DataType::U8, ..Default::default() };
            let raster = Raster::from_data_native(config, RasterData::U8(values.clone())).unwrap();
            let opts =
                TiffOptions { cog: true, tile_size: Some(512), overview_count: Some(0), mask: Some(mask.clone()), ..Default::default() };
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("spooled.tif");
            let mut cog = CogWriter::create(path.to_str().unwrap(), &raster, &opts).unwrap();
            let n = slab * cols;
            let give = |k: usize| (RasterData::U8(values[k * n..(k + 1) * n].to_vec()), Some(mask[k * n..(k + 1) * n].to_vec()));
            // The first three slabs, taken as calls take them once admitted; their rows of tiles are
            // compressed, not yet spooled.
            for k in 0..3 {
                let (data, m) = give(k);
                cog.take_admitted(data, slab, m).unwrap();
            }
            assert_eq!(cog.in_flight.len(), 3);
            for (_, _, result) in &mut cog.in_flight {
                let compressed = result.recv().unwrap();
                let (tx, rx) = std::sync::mpsc::sync_channel(1);
                tx.send(compressed).unwrap();
                *result = rx;
            }
            let counted = cog.call_bytes(n, n, slab, false).unwrap();
            let queued = std::mem::take(&mut cog.in_flight);
            let spooled = cog.call_bytes(n, n, slab, false).unwrap();
            cog.in_flight = queued;
            let limit = spooled + (counted - spooled) / 2;
            assert!(counted > limit && limit > 16 << 20, "{counted} bytes counted with the rows of tiles compressed, {spooled} without");
            crate::vsi::tests::set_limit(libc::RLIMIT_RSS, limit as u64);
            let (data, m) = give(3);
            cog.write_rows_masked(data, slab, m).unwrap();
            crate::vsi::tests::set_limit(libc::RLIMIT_RSS, libc::RLIM_INFINITY);
            assert_eq!(cog.in_flight.len(), 1, "the three rows of tiles compressed are spooled, the fourth's is being compressed");
            cog.finish().unwrap();
            assert_eq!(std::fs::read(&path).unwrap(), encode(&raster, &opts).unwrap());
            true
        });
    }

    /// A call of more rows than the raster has left, however many, is refused as invalid
    /// (`InvalidArgument`) before it is checked for memory, and the count of the bytes such a call
    /// would take does not overflow: more than the address space holds. The writer takes the next
    /// rows.
    #[test]
    fn a_cog_row_call_past_the_raster_is_refused_as_invalid() {
        let raster = overview_fixture(1, DataType::U8, RasterData::U8((0..65 * 35).map(|i| (i % 251) as u8).collect()));
        let mut header = crate::RasterDataset::new(raster.clone(), false);
        header.raster.data = RasterData::U8(Vec::new());
        let options = crate::RasterWriteOptions {
            driver: Some(crate::formats::RasterDriver::Cog),
            tile_size: Some(16),
            mask: Some(Vec::new()),
            ..Default::default()
        };
        let valid = vec![255u8; 65 * 35];
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("past.tif");
        let mut w = GeoTiffWriter::create(path.to_str().unwrap(), &header, &options).unwrap();
        w.write_rows(&slice_rows(&raster.data, 35, 0..16, 65, 1), 16, Some(&valid[..65 * 16])).unwrap();
        let invalid = |r: Result<()>| matches!(r, Err(Error::InvalidArgument(_)));
        assert!(invalid(w.write_rows(&RasterData::U8(vec![7; 65 * 20]), 20, Some(&valid[..65 * 20]))));
        assert!(invalid(w.write_rows(&RasterData::U8(Vec::new()), usize::MAX, Some(&[]))));
        {
            let RowWriter::Cog(c) = &mut w.inner else { unreachable!("a COG written in rows") };
            assert_eq!(c.get_mut().unwrap().call_bytes(0, 0, usize::MAX, true), None);
        }
        assert_eq!(w.rows_written(), 16);
        w.write_rows(&slice_rows(&raster.data, 35, 16..35, 65, 1), 19, Some(&valid[65 * 16..])).unwrap();
        w.finish().unwrap();
    }

    /// A COG's chunks wait next to the output, on its drive, not in the system's temporary
    /// directory (#767); `TERRIO_TMPDIR` or `CPL_TMPDIR` moves them, as `CPL_TMPDIR` does GDAL's.
    #[test]
    fn cog_chunks_spool_beside_the_output() {
        use std::collections::HashMap;
        let unset = || HashMap::from([("TERRIO_TMPDIR".to_string(), None), ("CPL_TMPDIR".to_string(), None)]);
        crate::config::push_scope(unset());
        let sep = std::path::MAIN_SEPARATOR;
        assert_eq!(spool_dir(&format!("out{sep}mosaic.tif")), std::path::Path::new("out"));
        assert_eq!(spool_dir("mosaic.tif"), std::path::Path::new("."));
        crate::config::pop_scope();

        let raster = overview_fixture(1, DataType::U8, RasterData::U8((0..65 * 35).map(|i| (i % 251) as u8).collect()));
        let opts = TiffOptions { cog: true, tile_size: Some(16), ..Default::default() };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("spooled.tif");
        let missing = dir.path().join("missing").to_str().unwrap().to_string();
        for key in ["TERRIO_TMPDIR", "CPL_TMPDIR"] {
            // The option is read: a directory that does not exist fails the write before it starts.
            let mut scope = HashMap::new();
            scope.insert(key.into(), Some(missing.clone()));
            crate::config::push_scope(scope);
            let refused = CogWriter::create(path.to_str().unwrap(), &raster, &opts);
            crate::config::pop_scope();
            match refused {
                Err(Error::Io(e)) => {
                    assert_eq!(e.kind(), std::io::ErrorKind::NotFound, "{key}: {e}");
                    assert!(e.raw_os_error().is_some(), "{key}: lost the OS error code");
                }
                other => panic!("{key}: {:?}", other.err()),
            }
        }
        // A spool opened in a configured directory is there while the COG is written.
        let elsewhere = tempfile::tempdir().unwrap();
        let mut scope = unset();
        scope.insert("TERRIO_TMPDIR".into(), Some(elsewhere.path().to_str().unwrap().into()));
        crate::config::push_scope(scope);
        let writer = CogWriter::create(dir.path().join("elsewhere.tif").to_str().unwrap(), &raster, &opts).unwrap();
        crate::config::pop_scope();
        assert_spooled_in(&writer, elsewhere.path());
        drop(writer);
        std::fs::remove_file(dir.path().join("elsewhere.tif")).unwrap();

        crate::config::push_scope(unset());
        let mut writer = CogWriter::create(path.to_str().unwrap(), &raster, &opts).unwrap();
        crate::config::pop_scope();
        assert_spooled_in(&writer, dir.path());
        writer.write_rows(raster.data.clone(), 35).unwrap();
        writer.finish().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), encode(&raster, &opts).unwrap());
        // The spools were unnamed: nothing but the COG is left beside it.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    /// Where the writer's spools are open, read from the file descriptors on Linux (other systems
    /// have no portable way to name an unnamed file).
    fn assert_spooled_in(writer: &CogWriter, dir: &std::path::Path) {
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::AsRawFd;
            let dir = std::fs::canonicalize(dir).unwrap();
            for spool in writer.levels.iter().flat_map(|l| &l.spools) {
                let target = std::fs::read_link(format!("/proc/self/fd/{}", spool.as_raw_fd())).unwrap();
                assert!(target.starts_with(&dir), "{} is not in {}", target.display(), dir.display());
            }
        }
        #[cfg(not(target_os = "linux"))]
        let _ = (writer, dir);
    }

    /// An output that is a symbolic link (as `/dev/stdout` redirected to a file is) spools beside
    /// the file it names, not in the link's directory.
    #[cfg(unix)]
    #[test]
    fn cog_through_a_link_spools_beside_its_target() {
        crate::config::push_scope(std::collections::HashMap::from([("TERRIO_TMPDIR".to_string(), None), ("CPL_TMPDIR".to_string(), None)]));
        let (links, files) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let target = files.path().join("out.tif");
        std::fs::write(&target, b"").unwrap();
        let link = links.path().join("out.tif");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let spooled = spool_dir(link.to_str().unwrap());
        crate::config::pop_scope();
        assert_eq!(spooled, std::fs::canonicalize(files.path()).unwrap());
    }

    /// A dangling output link still names the directory where opening will create the COG.
    #[cfg(unix)]
    #[test]
    fn cog_through_dangling_links_spools_beside_its_target() {
        crate::config::push_scope(std::collections::HashMap::from([("TERRIO_TMPDIR".to_string(), None), ("CPL_TMPDIR".to_string(), None)]));
        let dir = tempfile::tempdir().unwrap();
        let files = dir.path().join("files");
        let links = dir.path().join("links");
        std::fs::create_dir(&files).unwrap();
        std::fs::create_dir(&links).unwrap();
        // The temporary directory itself may be a link (macOS: /var -> /private/var), so
        // compare against the resolved directory.
        let files = std::fs::canonicalize(&files).unwrap();
        let target = files.join("out.tif");
        let absolute = links.join("absolute.tif");
        let relative = links.join("relative.tif");
        std::os::unix::fs::symlink(&target, &absolute).unwrap();
        std::os::unix::fs::symlink("absolute.tif", &relative).unwrap();
        for link in [&absolute, &relative] {
            assert_eq!(std::fs::canonicalize(spool_dir(link.to_str().unwrap())).unwrap(), files);
        }
        let direct = links.join("direct.tif");
        std::os::unix::fs::symlink("../files/out.tif", &direct).unwrap();
        assert_eq!(std::fs::canonicalize(spool_dir(direct.to_str().unwrap())).unwrap(), files);
        assert!(!target.exists());
        crate::config::pop_scope();
    }

    /// A COG written to a device spools in the system's temporary directory, not `/dev`.
    #[cfg(unix)]
    #[test]
    fn cog_to_a_device_spools_in_the_temporary_directory() {
        crate::config::push_scope(std::collections::HashMap::from([("TERRIO_TMPDIR".to_string(), None), ("CPL_TMPDIR".to_string(), None)]));
        assert_eq!(spool_dir("/dev/null"), std::env::temp_dir());
        let raster = overview_fixture(1, DataType::U8, RasterData::U8((0..65 * 35).map(|i| (i % 251) as u8).collect()));
        let opts = TiffOptions { cog: true, tile_size: Some(16), ..Default::default() };
        let mut writer = CogWriter::create("/dev/null", &raster, &opts).unwrap();
        crate::config::pop_scope();
        writer.write_rows(raster.data.clone(), 35).unwrap();
        writer.finish().unwrap();
    }

    /// COGs written a few rows at a time are those written from the whole raster, byte for
    /// byte: odd sizes, several overview levels, all resampling methods, alpha and nodata,
    /// interleaved and planar bands, and rows given one at a time or in uneven runs.
    #[test]
    fn cogs_written_in_rows_are_those_written_whole() {
        use Method::{Average, Bilinear, Cubic, Mode, Nearest};
        let dir = tempfile::tempdir().unwrap();
        // The matrix of #165, with GDAL's methods: COG overviews are GDAL's, which has no
        // min or max, so the rows of #165 for those are bilinear and cubic here.
        for (cols, rows, bands, t, planar, method, alpha, compression) in [
            (70, 50, 1, DataType::F32, false, Some(Average), false, Compression::Deflate),
            (157, 133, 3, DataType::U8, false, Some(Average), false, Compression::Lzw),
            (157, 133, 2, DataType::I16, true, Some(Average), false, Compression::Zstd),
            (33, 129, 1, DataType::U8, false, Some(Nearest), false, Compression::None),
            (16, 16, 1, DataType::F32, false, Some(Average), false, Compression::Deflate),
            (73, 127, 2, DataType::F32, true, Some(Average), true, Compression::Deflate),
            (67, 131, 4, DataType::U8, false, Some(Average), true, Compression::Lzw),
            (33, 129, 1, DataType::I16, false, Some(Bilinear), false, Compression::Deflate),
            (67, 93, 2, DataType::U8, true, Some(Cubic), true, Compression::Deflate),
            (35, 131, 1, DataType::I16, false, Some(Mode), false, Compression::Deflate),
            (33, 129, 2, DataType::U8, false, Some(Nearest), true, Compression::None),
            // LERC masks the padding of partial edge tiles of floating-point bands.
            #[cfg(feature = "lerc")]
            (70, 50, 1, DataType::F32, false, Some(Average), false, Compression::Lerc),
            #[cfg(feature = "lerc")]
            (73, 127, 2, DataType::F32, true, Some(Nearest), false, Compression::LercDeflate),
        ] {
            let cfg = wbraster::RasterConfig {
                cols,
                rows,
                bands,
                x_min: 500000.0,
                y_min: 4000000.0,
                cell_size: 10.0,
                nodata: -1.0,
                data_type: t,
                crs: wbraster::CrsInfo::from_epsg(32610),
                ..Default::default()
            };
            let n = cols * rows * bands;
            let mut data = match t {
                DataType::F32 => RasterData::F32((0..n).map(|i| if i % 37 == 0 { -1.0 } else { (i % 977) as f32 * 0.25 }).collect()),
                DataType::U8 => RasterData::U8((0..n).map(|i| (i * 7 % 251) as u8).collect()),
                _ => RasterData::I16((0..n).map(|i| (i % 1000) as i16 - 1).collect()),
            };
            if alpha {
                match &mut data {
                    RasterData::U8(values) => {
                        for (i, value) in values[(bands - 1) * cols * rows..].iter_mut().enumerate() {
                            *value = [0, 0, 32, 128, 255][i % 5];
                        }
                    }
                    RasterData::F32(values) => {
                        for (i, value) in values[(bands - 1) * cols * rows..].iter_mut().enumerate() {
                            *value = [0.0, 0.0, 0.25, 0.5, 1.0][i % 5];
                        }
                    }
                    _ => unreachable!(),
                }
            }
            let r = Raster::from_data_native(cfg, data.clone()).unwrap();
            let opts = TiffOptions {
                compression,
                cog: true,
                tile_size: Some(16),
                nodata: Some(-1.0),
                planar,
                cog_resampling: method,
                alpha,
                photometric: (bands == 4).then_some(2),
                ..Default::default()
            };
            let whole = encode(&r, &opts).unwrap();
            for step in [1, 2, 3, 7, 16, 40, rows] {
                let p = dir.path().join("rows.tif");
                let mut w = CogWriter::create(p.to_str().unwrap(), &r, &opts).unwrap();
                let mut y = 0;
                while y < rows {
                    let h = step.min(rows - y);
                    w.write_rows(slice_rows(&data, rows, y..y + h, cols, bands), h).unwrap();
                    y += h;
                }
                w.finish().unwrap();
                assert!(
                    std::fs::read(&p).unwrap() == whole,
                    "{cols}x{rows}x{bands} {t:?} planar {planar}, {method:?}, alpha {alpha}, rows by {step}"
                );
            }
        }
    }

    /// GDAL 3.13.2 mask semantics: mask-weighted data overviews and a mask at every level.
    /// Uneven slabs must produce the whole writer's bytes, including planar/alpha and MEM masks.
    #[test]
    fn masked_cogs_written_in_rows_are_those_written_whole() {
        let dir = tempfile::tempdir().unwrap();
        let (cols, rows, bands) = (131, 259, 2);
        let data = RasterData::U8((0..cols * rows * bands).map(|i| (i * 7 % 251) as u8).collect());
        let mask: Vec<u8> = (0..cols * rows).map(|i| if i % cols < 31 || i % 13 < 5 { 0 } else { 255 }).collect();
        let raster = Raster::from_data_native(
            wbraster::RasterConfig { cols, rows, bands, data_type: DataType::U8, ..Default::default() },
            data.clone(),
        )
        .unwrap();
        for method in [Method::Nearest, Method::Average, Method::Bilinear, Method::Cubic, Method::Mode] {
            for mask_without_dataset in [false, true] {
                for planar in [false, true] {
                    let opts = TiffOptions {
                        cog: true,
                        tile_size: Some(16),
                        cog_resampling: Some(method),
                        mask: Some(mask.clone()),
                        mask_without_dataset,
                        planar,
                        alpha: true,
                        nodata: Some(0.0),
                        ..Default::default()
                    };
                    let whole = encode(&raster, &opts).unwrap();
                    for step in [1, 37, rows] {
                        let path = dir.path().join("masked.tif");
                        let mut writer = CogWriter::create(path.to_str().unwrap(), &raster, &opts).unwrap();
                        assert!(writer.write_rows_masked(slice_rows(&data, rows, 0..1, cols, bands), 1, None).is_err());
                        assert!(writer.write_rows_masked(slice_rows(&data, rows, 0..1, cols, bands), 1, Some(vec![255])).is_err());
                        assert_eq!(writer.rows_written(), 0);
                        for y in (0..rows).step_by(step) {
                            let end = (y + step).min(rows);
                            writer
                                .write_rows_masked(
                                    slice_rows(&data, rows, y..end, cols, bands),
                                    end - y,
                                    Some(mask[y * cols..end * cols].to_vec()),
                                )
                                .unwrap();
                        }
                        writer.finish().unwrap();
                        assert_eq!(
                            std::fs::read(path).unwrap(),
                            whole,
                            "{method:?}, MEM {mask_without_dataset}, planar {planar}, step {step}"
                        );
                    }
                }
            }
        }
    }

    fn layouts_raster(bands: usize, t: DataType) -> Raster {
        let cfg = wbraster::RasterConfig {
            cols: 70,
            rows: 50,
            bands,
            x_min: 500000.0,
            y_min: 4000000.0,
            cell_size: 10.0,
            nodata: -1.0,
            data_type: t,
            crs: wbraster::CrsInfo::from_epsg(32610),
            ..Default::default()
        };
        let n = 70 * 50 * bands;
        let data = match t {
            DataType::F32 => RasterData::F32((0..n).map(|i| (i % 977) as f32 * 0.25 - 3.0).collect()),
            DataType::U8 => RasterData::U8((0..n).map(|i| (i % 251) as u8).collect()),
            _ => RasterData::I16((0..n).map(|i| (i % 1000) as i16 - 1).collect()),
        };
        Raster::from_data_native(cfg, data).unwrap()
    }

    #[test]
    fn jpeg_strips_hold_whole_mcus() {
        // About 64 KB of rows (21 of 3000 bytes), rounded down to 16; the last strip may be short.
        assert_eq!(strip_rows(None, 1000, 3000, Compression::Jpeg), 16);
        assert_eq!(strip_rows(Some(7), 100, 300, Compression::Jpeg), 16);
        assert_eq!(strip_rows(Some(7), 10, 300, Compression::Jpeg), 10);
        assert_eq!(strip_rows(Some(7), 100, 300, Compression::Deflate), 7);
        assert_eq!(strip_rows(None, 1000, 3000, Compression::Deflate), 21);
    }

    #[test]
    fn lerc_masks_pixels_whose_samples_are_all_nan() {
        let mut r = layouts_raster(2, DataType::F32);
        // The raster's bands are band-sequential in memory.
        let band = 70 * 50;
        let set = |r: &mut Raster, i: usize, x: f32| {
            if let RasterData::F32(v) = &mut r.data {
                v[i] = x;
            }
        };
        set(&mut r, 0, f32::NAN);
        set(&mut r, band, f32::NAN);
        set(&mut r, 5, f32::NAN);
        let second = r.data_f32().unwrap()[band + 5];
        let lerc = |r: &Raster, interleave: &str| {
            let options = crate::RasterWriteOptions {
                compression: Some("LERC".into()),
                creation_options: vec![("INTERLEAVE".into(), interleave.into())],
                ..Default::default()
            };
            let opts = TiffOptions::from_write_options(&options, false, None).unwrap().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("t.tif");
            write(r, p.to_str().unwrap(), &opts).map(|()| crate::geotiff::read_geotiff(&p).unwrap())
        };
        // Pixel 5 has a NaN in its first sample only: its second cannot be stored under a mask.
        let err = lerc(&r, "PIXEL").unwrap_err().to_string();
        assert!(err.contains("INTERLEAVE=BAND"), "{err}");
        let back = lerc(&r, "BAND").unwrap();
        let RasterData::F32(b) = &back.raster.data else { unreachable!() };
        assert!(b[0].is_nan() && b[band].is_nan() && b[5].is_nan());
        assert_eq!(b[band + 5], second);
        // Whole pixels of NaN are masked when interleaved by pixel.
        set(&mut r, 5, 1.0);
        let back = lerc(&r, "PIXEL").unwrap();
        let RasterData::F32(b) = &back.raster.data else { unreachable!() };
        assert!(b[0].is_nan() && b[band].is_nan());
        assert_eq!(b[5], 1.0);
    }

    /// GDAL (3.12) masks the padding of a partial edge tile in a LERC blob of one floating-point
    /// sample per pixel, whatever the nodata; integer and pixel-interleaved blobs keep the padding
    /// as unmasked zeros. Every codec stores zeros there. Lossless, the pixels inside are unchanged.
    #[cfg(feature = "lerc")]
    #[test]
    fn partial_tiles_are_padded_as_gdal() {
        let (cols, rows, tile) = (21, 19, 16);
        for (t, bands, planar, compression, nan) in [
            (DataType::F32, 1, false, Compression::Lerc, false),
            (DataType::F32, 1, false, Compression::Lerc, true),
            (DataType::F64, 1, false, Compression::LercDeflate, false),
            (DataType::F32, 2, true, Compression::Lerc, true),
            (DataType::F32, 2, false, Compression::Lerc, false),
            (DataType::I16, 1, false, Compression::Lerc, false),
            (DataType::U8, 1, false, Compression::Lerc, false),
            (DataType::F32, 1, false, Compression::Deflate, true),
        ] {
            let n = cols * rows * bands;
            let value = |i: usize| if nan && i % cols < 3 && (i / cols) % rows < 2 { f64::NAN } else { (i % 89) as f64 + 1.5 };
            let data = match t {
                DataType::F32 => RasterData::F32((0..n).map(|i| value(i) as f32).collect()),
                DataType::F64 => RasterData::F64((0..n).map(value).collect()),
                DataType::I16 => RasterData::I16((0..n).map(|i| (i % 89) as i16 - 7).collect()),
                _ => RasterData::U8((0..n).map(|i| (i % 89) as u8 + 1).collect()),
            };
            let level = Level { cols, rows, bands, data: std::borrow::Cow::Owned(data.clone()) };
            let layout = Layout {
                tile: Some((tile as u32, tile as u32)),
                rows_per_strip: None,
                planar,
                photometric: 1,
                extra: if bands > 1 && !planar { vec![0; bands - 1] } else { Vec::new() },
                predictor: 1,
                parts: 1,
                nbits: None,
            };
            let img = level.image(t, compression, &CodecParams::default(), &layout).unwrap();
            let (across, down) = (cols.div_ceil(tile), rows.div_ceil(tile));
            let spp = if planar { 1 } else { bands };
            assert_eq!(img.chunks.len(), across * down * bands / spp);
            let case = format!("{t:?} x{bands} planar {planar} {compression:?} nan {nan}");
            for (k, chunk) in img.chunks.iter().enumerate() {
                let (plane, j) = (k / (across * down), k % (across * down));
                let (x0, y0) = ((j % across) * tile, (j / across) * tile);
                let (vw, vh) = ((cols - x0).min(tile), (rows - y0).min(tile));
                let blob = match compression {
                    Compression::LercDeflate => {
                        let mut d = Vec::new();
                        std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&chunk[..]), &mut d).unwrap();
                        d
                    }
                    Compression::Deflate => {
                        let mut d = Vec::new();
                        std::io::Read::read_to_end(&mut flate2::read::ZlibDecoder::new(&chunk[..]), &mut d).unwrap();
                        // Zeros in the padding; the pixels inside are the raster's.
                        for (p, b) in d.as_chunks::<4>().0.iter().enumerate() {
                            let (x, y, v) = (p % tile, p / tile, f32::from_le_bytes(*b));
                            if x < vw && y < vh {
                                let s = data.get_f64((y0 + y) * cols + x0 + x) as f32;
                                assert!(v.to_bits() == s.to_bits(), "{case}: pixel {x},{y} of tile {k}");
                            } else {
                                assert!(v.to_bits() == 0, "{case}: padding {x},{y} of tile {k}");
                            }
                        }
                        continue;
                    }
                    _ => chunk.clone(),
                };
                let d = lerc_reader::decode(&blob).unwrap();
                let masks_padding = matches!(t, DataType::F32 | DataType::F64) && spp == 1;
                let partial = vw < tile || vh < tile;
                let values = d.pixels.to_f64();
                for p in 0..tile * tile {
                    let (x, y) = (p % tile, p / tile);
                    let inside = x < vw && y < vh;
                    let valid = d.mask.as_ref().is_none_or(|m| m[p] != 0);
                    if !inside {
                        assert_eq!(valid, !masks_padding, "{case}: padding {x},{y} of tile {k}");
                        if valid {
                            assert!((0..spp).all(|s| values[p * spp + s] == 0.0), "{case}: padding {x},{y} of tile {k}");
                        }
                        continue;
                    }
                    for s in 0..spp {
                        let band = plane + s;
                        let src = data.get_f64((band * rows + y0 + y) * cols + x0 + x);
                        if src.is_nan() {
                            assert!(!valid, "{case}: NaN at {x},{y} of tile {k}");
                        } else {
                            assert!(valid && values[p * spp + s] == src, "{case}: pixel {x},{y} of tile {k}");
                        }
                    }
                }
                // Blobs of whole tiles without NaN have no mask.
                if !partial && !nan {
                    assert!(d.mask.is_none(), "{case}: tile {k}");
                }
            }
        }
    }

    #[test]
    fn writes_gdal_layouts_and_predictors() {
        for (bands, t, co) in [
            (3, DataType::I16, vec![("TILED", "YES"), ("BLOCKXSIZE", "32"), ("BLOCKYSIZE", "16"), ("INTERLEAVE", "BAND")]),
            (2, DataType::I16, vec![("BLOCKYSIZE", "7"), ("COMPRESS", "DEFLATE"), ("PREDICTOR", "2")]),
            (2, DataType::F32, vec![("TILED", "YES"), ("COMPRESS", "LZW"), ("PREDICTOR", "3")]),
            (1, DataType::F32, vec![("COMPRESS", "ZSTD"), ("PREDICTOR", "3"), ("INTERLEAVE", "BAND")]),
            (4, DataType::U8, vec![("PHOTOMETRIC", "RGB"), ("ALPHA", "YES"), ("COMPRESS", "PACKBITS"), ("PREDICTOR", "2")]),
        ] {
            let r = layouts_raster(bands, t);
            let options = crate::RasterWriteOptions {
                creation_options: co.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                ..Default::default()
            };
            let opts = TiffOptions::from_write_options(&options, false, Some(-1.0)).unwrap().unwrap();
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("t.tif");
            write(&r, p.to_str().unwrap(), &opts).unwrap();
            let ds = crate::geotiff::read_geotiff(&p).unwrap();
            let same = match (&ds.raster.data, &r.data) {
                (RasterData::I16(a), RasterData::I16(b)) => a == b,
                (RasterData::F32(a), RasterData::F32(b)) => a == b,
                (RasterData::U8(a), RasterData::U8(b)) => a == b,
                _ => false,
            };
            assert!(same, "{co:?}");
        }
    }

    /// A raster of `cols` x `rows` Float32 pixels whose values mark the level they belong to, so
    /// that no resampling of another level gives them.
    fn level_raster(cols: usize, rows: usize, level: usize) -> std::sync::Arc<Raster> {
        let cfg = wbraster::RasterConfig { cols, rows, bands: 1, data_type: DataType::F32, ..Default::default() };
        let data = (0..cols * rows).map(|i| level as f32 * 1000.0 + (i % 97) as f32 * 0.5).collect();
        std::sync::Arc::new(Raster::from_data_native(cfg, RasterData::F32(data)).unwrap())
    }

    /// Every level of a TIFF file: its size and Float32 samples, the full resolution first.
    fn tiff_levels(path: &std::path::Path) -> Vec<(usize, usize, Vec<f32>)> {
        let mut src = crate::LazyRaster::open_tiff(path.to_str().unwrap()).unwrap().unwrap();
        let mut sizes = vec![(src.width(), src.height())];
        sizes.extend(src.overviews());
        sizes
            .into_iter()
            .enumerate()
            .map(|(i, (w, h))| {
                let o = crate::WindowOptions { bands: Vec::new(), overview: i.checked_sub(1) };
                let RasterData::F32(v) = src.read_window(crate::Window::full(w, h), &o).unwrap().raster.data else { unreachable!() };
                (w, h, v)
            })
            .collect()
    }

    /// Supplied overviews are written as they are, as GDAL's COG driver writes a source's existing
    /// overviews (`OVERVIEWS=AUTO` or `FORCE_USE_EXISTING`) and its GTiff driver with
    /// `COPY_SRC_OVERVIEWS=YES`: `gdaladdo`'s ceil sizes and the COG driver's floored ones alike,
    /// with no `OVERVIEW_RESAMPLING` recorded for them.
    #[test]
    fn supplied_overviews_are_written_verbatim() {
        let r = layouts_raster(1, DataType::F32);
        let ceil = [(35, 25), (18, 13), (9, 7), (5, 4)];
        let floor = [(35, 25), (17, 12), (8, 6)];
        for (driver, co, sizes) in [
            (crate::formats::RasterDriver::Cog, vec![("BLOCKSIZE", "16")], &ceil[..]),
            (crate::formats::RasterDriver::Cog, vec![("BLOCKSIZE", "16"), ("OVERVIEWS", "FORCE_USE_EXISTING")], &floor[..]),
            (crate::formats::RasterDriver::GTiff, vec![("TILED", "YES"), ("BLOCKXSIZE", "16"), ("BLOCKYSIZE", "16")], &ceil[..]),
        ] {
            let supplied: Vec<_> = sizes.iter().enumerate().map(|(i, (w, h))| level_raster(*w, *h, i + 1)).collect();
            let mut creation_options: Vec<(String, String)> = co.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            creation_options.push(("COMPRESS".into(), "DEFLATE".into()));
            let options =
                crate::RasterWriteOptions { driver: Some(driver), creation_options, overview_data: supplied.clone(), ..Default::default() };
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("s.tif");
            crate::write_raster(&r, p.to_str().unwrap(), &options).unwrap();
            let levels = tiff_levels(&p);
            assert_eq!(levels.len(), sizes.len() + 1, "{driver:?} {co:?}");
            let RasterData::F32(base) = &r.data else { unreachable!() };
            assert_eq!(&levels[0].2, base);
            for ((w, h, v), ov) in levels[1..].iter().zip(&supplied) {
                let RasterData::F32(expected) = &ov.data else { unreachable!() };
                assert_eq!((*w, *h), (ov.cols, ov.rows), "{driver:?} {co:?}");
                assert_eq!(v, expected, "{driver:?} {co:?} {w} x {h}");
            }
            let gdal_metadata = {
                let mut d = tiff::decoder::Decoder::new(std::fs::File::open(&p).unwrap()).unwrap();
                d.get_tag_ascii_string(tiff::tags::Tag::Unknown(42112)).unwrap_or_default()
            };
            assert!(!gdal_metadata.contains("OVERVIEW_RESAMPLING"), "{gdal_metadata}");
        }
        // A COG's per-dataset mask gets overviews of its own, made from its full resolution; a
        // GeoTIFF's is refused.
        let mask: Vec<u8> = (0..70 * 50).map(|i| u8::from(i % 70 >= 20)).collect();
        let supplied: Vec<_> = ceil.iter().enumerate().map(|(i, (w, h))| level_raster(*w, *h, i + 1)).collect();
        let options = |driver| crate::RasterWriteOptions {
            driver: Some(driver),
            creation_options: vec![("BLOCKSIZE".into(), "16".into())],
            overview_data: supplied.clone(),
            mask: Some(mask.clone()),
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.tif");
        crate::write_raster(&r, p.to_str().unwrap(), &options(crate::formats::RasterDriver::Cog)).unwrap();
        let mut src = crate::LazyRaster::open_tiff(p.to_str().unwrap()).unwrap().unwrap();
        for (i, (w, h)) in ceil.iter().enumerate() {
            let m = src.read_level_mask(crate::Window::full(*w, *h), Some(i)).unwrap().expect("an overview mask");
            assert_eq!(m.len(), w * h);
            // The left 20 of 70 columns are masked: the first pixel of each level until the
            // (cubic) kernel of a level reaches valid columns, the last pixel never.
            assert_eq!(m[0] == 0, i < 3, "level {i}");
            assert_ne!(m[w - 1], 0);
        }
        let gtiff = crate::write_raster(&r, dir.path().join("g.tif").to_str().unwrap(), &options(crate::formats::RasterDriver::GTiff));
        assert!(gtiff.is_err());
    }

    /// Supplied overviews are refused where they cannot be written as given: another band count,
    /// type or a level no smaller than the one before, with factors or a resampling, with
    /// `OVERVIEWS=NONE` or `IGNORE_EXISTING` (which would discard them), in rows, and in formats
    /// other than GeoTIFF and COG.
    #[test]
    fn supplied_overviews_refuse_contradictions() {
        let r = layouts_raster(1, DataType::F32);
        let write = |driver: crate::formats::RasterDriver,
                     co: &[(&str, &str)],
                     overview_data: Vec<std::sync::Arc<Raster>>,
                     f: fn(&mut crate::RasterWriteOptions)| {
            let mut options = crate::RasterWriteOptions {
                driver: Some(driver),
                creation_options: co.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                overview_data,
                ..Default::default()
            };
            f(&mut options);
            let dir = tempfile::tempdir().unwrap();
            crate::write_raster(&r, dir.path().join("s.tif").to_str().unwrap(), &options).map_err(|e| e.to_string())
        };
        use crate::formats::RasterDriver::{AAIGrid, Cog, GTiff};
        let good = || vec![level_raster(35, 25, 1)];
        assert!(write(Cog, &[], good(), |_| {}).is_ok());
        let i16 = {
            let cfg = wbraster::RasterConfig { cols: 35, rows: 25, bands: 1, data_type: DataType::I16, ..Default::default() };
            std::sync::Arc::new(Raster::from_data_native(cfg, RasterData::I16(vec![0; 35 * 25])).unwrap())
        };
        for (what, result) in [
            ("type", write(Cog, &[], vec![i16], |_| {})),
            ("same size", write(Cog, &[], vec![level_raster(70, 50, 1)], |_| {})),
            ("larger", write(Cog, &[], vec![level_raster(35, 25, 1), level_raster(36, 12, 2)], |_| {})),
            ("factors", write(GTiff, &[], good(), |o| o.overviews = vec![2])),
            ("resampling", write(Cog, &[], good(), |o| o.overview_resampling = Some("average".into()))),
            ("OVERVIEW_RESAMPLING", write(Cog, &[("OVERVIEW_RESAMPLING", "NEAREST")], good(), |_| {})),
            ("RESAMPLING", write(Cog, &[("RESAMPLING", "AVERAGE")], good(), |_| {})),
            (
                "RESAMPLING with FORCE_USE_EXISTING",
                write(Cog, &[("OVERVIEWS", "FORCE_USE_EXISTING"), ("RESAMPLING", "AVERAGE")], good(), |_| {}),
            ),
            // Area-matched overviews are computed too: they and supplied ones are not combined.
            ("OVERVIEW_RESAMPLING=AREA_MATCHED", write(Cog, &[("OVERVIEW_RESAMPLING", "AREA_MATCHED")], good(), |_| {})),
            ("GTiff RESAMPLING keyword area_matched", write(GTiff, &[], good(), |o| o.overview_resampling = Some("area_matched".into()))),
            ("NONE", write(Cog, &[("OVERVIEWS", "NONE")], good(), |_| {})),
            ("IGNORE_EXISTING", write(Cog, &[("OVERVIEWS", "ignore_existing")], good(), |_| {})),
            ("AAIGrid", write(AAIGrid, &[], good(), |_| {})),
        ] {
            assert!(result.is_err(), "{what}");
            if what.contains("RESAMPLING") || what == "resampling" {
                assert!(result.unwrap_err().contains("no overview resampling applies"), "{what}");
            }
        }
        let header = crate::RasterDataset::new(r.clone(), true);
        let dir = tempfile::tempdir().unwrap();
        let options = crate::RasterWriteOptions { driver: Some(Cog), overview_data: good(), ..Default::default() };
        assert!(GeoTiffWriter::create(dir.path().join("rows.tif").to_str().unwrap(), &header, &options).is_err());
    }

    /// As GDAL 3.13.3's COG driver, `OVERVIEWS=FORCE_USE_EXISTING` generates no overviews: without
    /// existing ones (supplied or a source's), the COG has none, with a warning; AUTO generates
    /// them.
    #[test]
    fn overviews_force_use_existing_without_existing_writes_none() {
        let r = layouts_raster(1, DataType::F32);
        let count = |overviews: &str| {
            let options = crate::RasterWriteOptions {
                driver: Some(crate::formats::RasterDriver::Cog),
                creation_options: vec![("BLOCKSIZE".into(), "16".into()), ("OVERVIEWS".into(), overviews.into())],
                ..Default::default()
            };
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("c.tif");
            let _ = crate::warnings::take();
            crate::write_raster(&r, p.to_str().unwrap(), &options).unwrap();
            (tiff_levels(&p).len() - 1, crate::warnings::take())
        };
        let (n, warnings) = count("FORCE_USE_EXISTING");
        assert_eq!(n, 0);
        assert!(warnings.iter().any(|w| w.contains("FORCE_USE_EXISTING")), "{warnings:?}");
        assert_eq!(count("AUTO").0, 3);
        assert_eq!(count("NONE").0, 0);
    }

    /// `metadata` items are set over the raster's own (in their place) or added after them, as
    /// `gdal_translate -mo` sets them, in whole and row-written files alike.
    #[test]
    fn metadata_items_are_set_over_the_raster_own() {
        let mut r = layouts_raster(1, DataType::F32);
        r.metadata = vec![("A".into(), "1".into()), ("B".into(), "2".into())];
        let options = crate::RasterWriteOptions {
            driver: Some(crate::formats::RasterDriver::GTiff),
            metadata: vec![("B".into(), "2".into()), ("B".into(), "3".into()), ("C".into(), "4".into())],
            ..Default::default()
        };
        let expected: Vec<(String, String)> =
            [("A", "1"), ("B", "3"), ("C", "4")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let dir = tempfile::tempdir().unwrap();
        let whole = dir.path().join("whole.tif");
        crate::write_raster(&r, whole.to_str().unwrap(), &options).unwrap();
        let items = |p: &std::path::Path| {
            let m = crate::geotiff::read_geotiff(p).unwrap().raster.metadata;
            m.into_iter().filter(|(k, _)| k != "AREA_OR_POINT").collect::<Vec<_>>()
        };
        assert_eq!(items(&whole), expected);
        let rows = dir.path().join("rows.tif");
        let header = crate::RasterDataset::new(r.clone(), true);
        let mut w = GeoTiffWriter::create(rows.to_str().unwrap(), &header, &options).unwrap();
        w.write_rows(&r.data, r.rows, None).unwrap();
        w.finish().unwrap();
        assert_eq!(items(&rows), expected);
        for creation_options in [vec![("COPY_SRC_MDD".into(), "NO".into())], vec![("SRC_MDD".into(), "OTHER".into())]] {
            let filtered =
                crate::RasterWriteOptions { driver: Some(crate::formats::RasterDriver::Cog), creation_options, ..options.clone() };
            let path = dir.path().join("filtered.tif");
            crate::write_raster(&r, path.to_str().unwrap(), &filtered).unwrap();
            assert_eq!(items(&path), expected[1..].to_vec());
            let path = dir.path().join("filtered-rows.tif");
            let mut w = GeoTiffWriter::create(path.to_str().unwrap(), &header, &filtered).unwrap();
            w.write_rows(&r.data, r.rows, None).unwrap();
            w.finish().unwrap();
            assert_eq!(items(&path), expected[1..].to_vec());
        }
        let o = crate::RasterWriteOptions { driver: Some(crate::formats::RasterDriver::AAIGrid), ..options };
        assert!(crate::write_raster(&r, dir.path().join("m.asc").to_str().unwrap(), &o).is_err());
    }

    /// A COG reads `PREDICTOR` as GDAL 3.13.3's COG driver does: `YES`, `ON` and `TRUE` are 3 for
    /// floating-point data and 2 for integers, `STANDARD` is 2, `FLOATING_POINT` is 3, and `FALSE`
    /// and `OFF` are none; every overview takes the image's predictor, as GDAL's do. A GeoTIFF
    /// keeps `YES` as 2 and refuses the COG's words, and 3 stays refused for integers.
    #[test]
    fn cog_predictor_yes_follows_the_data_type_as_gdal_cog() {
        let predictors = |bytes: Vec<u8>| {
            let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(bytes)).unwrap();
            let mut found = Vec::new();
            for i in 0.. {
                if dec.seek_to_image(i).is_err() {
                    break;
                }
                found.push(dec.find_tag_unsigned::<u16>(tiff::tags::Tag::Predictor).unwrap().unwrap_or(1));
            }
            found
        };
        let options = |driver_cog: bool, word: &str| {
            let mut co = vec![("COMPRESS".to_string(), "DEFLATE".to_string()), ("PREDICTOR".to_string(), word.to_string())];
            if driver_cog {
                co.push(("BLOCKSIZE".into(), "16".into()));
            }
            let options = crate::RasterWriteOptions { creation_options: co, ..Default::default() };
            TiffOptions::from_write_options(&options, driver_cog, Some(-1.0))
        };
        for (t, word, expected) in [
            (DataType::F32, "YES", 3),
            (DataType::F32, "on", 3),
            (DataType::F32, "TRUE", 3),
            (DataType::I16, "YES", 2),
            (DataType::U8, "YES", 2),
            (DataType::F32, "STANDARD", 2),
            (DataType::I16, "STANDARD", 2),
            (DataType::F32, "FLOATING_POINT", 3),
            (DataType::F32, "2", 2),
            (DataType::F32, "3", 3),
            (DataType::F32, "FALSE", 1),
            (DataType::I16, "OFF", 1),
            (DataType::I16, "NO", 1),
        ] {
            let r = layouts_raster(1, t);
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path().join("c.tif");
            write(&r, p.to_str().unwrap(), &options(true, word).unwrap().unwrap()).unwrap();
            let found = predictors(std::fs::read(&p).unwrap());
            assert!(found.len() > 1, "the COG has overviews");
            assert!(found.iter().all(|&p| p == expected), "{t:?} PREDICTOR={word}: {found:?}");
            let back = crate::geotiff::read_geotiff(&p).unwrap();
            let same = match (&back.raster.data, &r.data) {
                (RasterData::I16(a), RasterData::I16(b)) => a == b,
                (RasterData::F32(a), RasterData::F32(b)) => a == b,
                (RasterData::U8(a), RasterData::U8(b)) => a == b,
                _ => false,
            };
            assert!(same, "{t:?} PREDICTOR={word}");
        }
        for word in ["FLOATING_POINT", "3"] {
            let err = encode(&layouts_raster(1, DataType::I16), &options(true, word).unwrap().unwrap()).unwrap_err();
            assert!(err.to_string().contains("PREDICTOR=3"), "{word}: {err}");
        }
        assert!(options(true, "MAYBE").is_err());
        // A GeoTIFF's YES stays 2; the words only GDAL's COG driver takes are refused.
        let bytes = encode(&layouts_raster(1, DataType::F32), &options(false, "YES").unwrap().unwrap()).unwrap();
        assert_eq!(predictors(bytes), [2]);
        for word in ["STANDARD", "FLOATING_POINT", "TRUE", "OFF"] {
            assert!(options(false, word).is_err(), "GTiff PREDICTOR={word}");
        }
    }

    /// `ZLEVEL` (1 to 9) sets the Deflate level of DEFLATE and LERC_DEFLATE chunks, in GeoTIFFs and
    /// COGs, written whole or streamed, 6 when it is not given, and `ZSTD_LEVEL` (1 to 22) the
    /// level of ZSTD and LERC_ZSTD chunks (9 when omitted). The pure encoder honors levels
    /// through 4 and clamps higher values to 4; `libzstd` honors the full range.
    /// Every level reads back to the raster. Both are checked whatever the
    /// compression, as the qualities are: a level out of range is refused.
    #[test]
    fn compression_levels_change_the_chunks_and_are_checked() {
        // A gradient with noise, 256 x 256.
        let cfg = wbraster::RasterConfig { cols: 256, rows: 256, bands: 1, data_type: DataType::U8, ..Default::default() };
        let mut x = 7u64;
        let data = (0..256 * 256)
            .map(|i| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((i % 256) / 4 + (i / 256) / 4 + (x >> 61) as usize) as u8
            })
            .collect();
        let r = Raster::from_data_native(cfg, RasterData::U8(data)).unwrap();
        // LERC's blob of that gradient is about as large compressed as not, at any level: the
        // LERC checks take 64 x 64 Float32 values drawn from 16, whose blob the second stage
        // compresses.
        let lerc_cfg = wbraster::RasterConfig { cols: 64, rows: 64, bands: 1, data_type: DataType::F32, ..Default::default() };
        let lerc_data = (0..64 * 64)
            .map(|_| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                (x >> 60) as f32 / 8.0 + 0.1
            })
            .collect();
        let lerc_raster = Raster::from_data_native(lerc_cfg, RasterData::F32(lerc_data)).unwrap();
        let encode_raster = |r: &Raster, compression: &str, cog: bool, co: &[(&str, &str)]| {
            let options = crate::RasterWriteOptions {
                compression: Some(compression.into()),
                creation_options: co.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                ..Default::default()
            };
            TiffOptions::from_write_options(&options, cog, None).map(|o| encode(r, &o.unwrap()).unwrap())
        };
        let encode_cog = |compression: &str, cog: bool, co: &[(&str, &str)]| encode_raster(&r, compression, cog, co);
        let encode_with = |compression: &str, co: &[(&str, &str)]| encode_cog(compression, false, co);
        let read_back = |bytes: &[u8]| {
            crate::cog::LazyRaster::from_tiff_bytes(bytes.to_vec()).unwrap().unwrap().read().unwrap().raster.data.to_f64_vec()
        };
        let deflate: Vec<Vec<u8>> = (1..=9).map(|l| encode_with("DEFLATE", &[("ZLEVEL", &l.to_string())]).unwrap()).collect();
        assert_eq!(encode_with("DEFLATE", &[]).unwrap(), deflate[5]);
        // On this raster, levels 1, 6 and 9 give other bytes, each no larger than the one before.
        let sizes: Vec<usize> = deflate.iter().map(Vec::len).collect();
        assert!(deflate[0] != deflate[5] && deflate[5] != deflate[8], "{sizes:?}");
        assert!(sizes[8] <= sizes[5] && sizes[5] <= sizes[0], "{sizes:?}");
        for (l, bytes) in deflate.iter().enumerate() {
            assert_eq!(read_back(bytes), r.data.to_f64_vec(), "ZLEVEL {}", l + 1);
        }
        // A COG's Deflate chunks, and LERC_DEFLATE's Deflate stage, take the level too.
        let (one, nine) =
            (encode_cog("DEFLATE", true, &[("ZLEVEL", "1")]).unwrap(), encode_cog("DEFLATE", true, &[("ZLEVEL", "9")]).unwrap());
        assert_ne!(one, nine, "COG");
        assert_eq!(read_back(&nine), r.data.to_f64_vec(), "COG");
        let lerc = |compression: &str, co: &[(&str, &str)]| encode_raster(&lerc_raster, compression, false, co).unwrap();
        if cfg!(feature = "lerc") {
            let (one, nine) = (lerc("LERC_DEFLATE", &[("ZLEVEL", "1")]), lerc("LERC_DEFLATE", &[("ZLEVEL", "9")]));
            // Not only the zlib header's FLEVEL: the Deflate stage's output differs.
            assert!(nine.len() < one.len(), "LERC_DEFLATE: {} >= {}", nine.len(), one.len());
            assert_eq!(read_back(&nine), lerc_raster.data.to_f64_vec(), "LERC_DEFLATE");
        }
        // The streaming writer, for a GeoTIFF and for a COG, reads ZLEVEL as well.
        let header = crate::RasterDataset::new(r.clone(), false);
        let dir = tempfile::tempdir().unwrap();
        let streamed = |driver: crate::formats::RasterDriver, zlevel: &str| {
            let p = dir.path().join(format!("{driver:?}{zlevel}.tif"));
            let options = crate::RasterWriteOptions {
                driver: Some(driver),
                compression: Some("DEFLATE".into()),
                creation_options: vec![("ZLEVEL".into(), zlevel.into())],
                ..Default::default()
            };
            let mut w = GeoTiffWriter::create(p.to_str().unwrap(), &header, &options).unwrap();
            w.write_rows(&r.data, r.rows, None).unwrap();
            w.finish().unwrap();
            std::fs::read(&p).unwrap()
        };
        for driver in [crate::formats::RasterDriver::GTiff, crate::formats::RasterDriver::Cog] {
            assert_ne!(streamed(driver, "1"), streamed(driver, "9"), "streamed {driver:?}");
        }
        let libzstd = cfg!(all(feature = "libzstd", not(target_family = "wasm")));
        let zstd = |level: &str| encode_with("ZSTD", &[("ZSTD_LEVEL", level)]).unwrap();
        assert_eq!(zstd("9"), encode_with("ZSTD", &[]).unwrap());
        // `zrip` finds the same matches in this noisy gradient at every level (it writes levels
        // up to 4); the C library at level 22 finds more. LERC_ZSTD's blob below shows the level.
        assert_eq!(zstd("1") != zstd("22"), libzstd);
        assert_eq!(read_back(&zstd("22")), r.data.to_f64_vec());
        // Natural-language repetitions distinguish zrip's level-1 and level-4 match searches.
        // Keep the MIT license text inline so this fixture does not depend on repository files.
        let text = br#"MIT License

Copyright (c) <year> <copyright holders>

Permission is hereby granted, free of charge, to any person obtaining a copy of this software and
associated documentation files (the "Software"), to deal in the Software without restriction, including
without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is furnished to do so, subject to the
following conditions:
"#;
        let cfg = wbraster::RasterConfig { cols: text.len(), rows: 1, bands: 1, data_type: DataType::U8, ..Default::default() };
        let text_raster = Raster::from_data_native(cfg, RasterData::U8(text.to_vec())).unwrap();
        let encoded = |level: &str| encode_raster(&text_raster, "ZSTD", false, &[("ZSTD_LEVEL", level)]).unwrap();
        let (one, four) = (encoded("1"), encoded("4"));
        if !libzstd {
            assert_ne!(one, four, "ZSTD_LEVEL must reach the pure encoder");
            assert_eq!(four, encoded("22"), "the pure encoder clamps higher levels to 4");
        }
        for bytes in [&one, &four] {
            assert_eq!(read_back(bytes), text_raster.data.to_f64_vec());
        }
        if cfg!(feature = "lerc") {
            let lerc_zstd = |level: &str| lerc("LERC_ZSTD", &[("ZSTD_LEVEL", level)]);
            assert_eq!(lerc_zstd("9"), lerc("LERC_ZSTD", &[]));
            assert_ne!(lerc_zstd("1"), lerc_zstd("22"));
            assert_eq!(read_back(&lerc_zstd("22")), lerc_raster.data.to_f64_vec(), "LERC_ZSTD");
        }
        for (compression, key, value) in [
            ("DEFLATE", "ZLEVEL", "0"),
            ("DEFLATE", "ZLEVEL", "10"),
            ("LZW", "ZLEVEL", "fast"),
            ("ZSTD", "ZSTD_LEVEL", "0"),
            ("ZSTD", "ZSTD_LEVEL", "23"),
            ("NONE", "ZSTD_LEVEL", "-1"),
        ] {
            let error = encode_with(compression, &[(key, value)]).unwrap_err().to_string();
            assert!(error.contains(&format!("invalid {key} '{value}'")), "{error}");
        }
    }

    /// A COG's `LEVEL` is its Deflate (DEFLATE, LERC_DEFLATE) or Zstandard (ZSTD, LERC_ZSTD)
    /// level, as GDAL 3.13.3's COG driver takes it, ahead of `ZLEVEL` and `ZSTD_LEVEL`; other
    /// codecs and GeoTIFFs ignore it.
    #[test]
    fn cog_level_is_the_deflate_or_zstd_level() {
        // Text repeats differently enough for Deflate levels 1 and 9 and Zstandard levels 1 and 4
        // to give other bytes.
        let text = b"Permission is hereby granted, free of charge, to any person obtaining a copy of this software \
            and associated documentation files, to deal in the Software without restriction, including without \
            limitation the rights to use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies.";
        let cfg = wbraster::RasterConfig { cols: text.len(), rows: 4, bands: 1, data_type: DataType::U8, ..Default::default() };
        let r = Raster::from_data_native(cfg, RasterData::U8(text.repeat(4))).unwrap();
        let write = |compression: &str, cog: bool, co: &[(&str, &str)]| {
            let options = crate::RasterWriteOptions {
                compression: Some(compression.into()),
                creation_options: co.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
                ..Default::default()
            };
            TiffOptions::from_write_options(&options, cog, None).map(|o| encode(&r, &o.unwrap()).unwrap())
        };
        let cog = |compression: &str, co: &[(&str, &str)]| write(compression, true, co).unwrap();
        assert_ne!(cog("DEFLATE", &[("ZLEVEL", "1")]), cog("DEFLATE", &[("ZLEVEL", "9")]));
        for level in ["1", "9"] {
            assert_eq!(cog("DEFLATE", &[("LEVEL", level)]), cog("DEFLATE", &[("ZLEVEL", level)]), "LEVEL={level}");
        }
        assert_eq!(cog("DEFLATE", &[("LEVEL", "1"), ("ZLEVEL", "9")]), cog("DEFLATE", &[("ZLEVEL", "1")]));
        assert_ne!(cog("ZSTD", &[("ZSTD_LEVEL", "1")]), cog("ZSTD", &[("ZSTD_LEVEL", "4")]));
        assert_eq!(cog("ZSTD", &[("LEVEL", "1"), ("ZSTD_LEVEL", "4")]), cog("ZSTD", &[("ZSTD_LEVEL", "1")]));
        // A GeoTIFF has no LEVEL; LZW none, whatever its value.
        assert_eq!(write("DEFLATE", false, &[("LEVEL", "1")]).unwrap(), write("DEFLATE", false, &[]).unwrap());
        assert_eq!(cog("LZW", &[("LEVEL", "99")]), cog("LZW", &[]));
        for (compression, value, range) in [("DEFLATE", "10", "1 to 9"), ("ZSTD", "23", "1 to 22")] {
            let error = write(compression, true, &[("LEVEL", value)]).unwrap_err().to_string();
            assert!(error.contains(&format!("invalid LEVEL '{value}' ({range})")), "{error}");
        }
        // LEVEL replaces ZLEVEL and ZSTD_LEVEL, which are still checked.
        assert!(write("DEFLATE", true, &[("LEVEL", "1"), ("ZLEVEL", "99")]).is_err());
        assert!(write("ZSTD", true, &[("LEVEL", "1"), ("ZSTD_LEVEL", "99")]).is_err());
    }

    /// `ZLEVEL` reaches the Deflate encoder through each write of a GeoTIFF or COG: whole
    /// (`write_raster`, `write_raster_dataset`), to bytes (`memory::write_raster_bytes`, and the
    /// in-memory codec of WebAssembly builds) and in strips (an operation's `StripWriter`).
    #[test]
    fn zlevel_reaches_the_encoder_through_each_write() {
        let cfg = wbraster::RasterConfig { cols: 64, rows: 64, bands: 1, data_type: DataType::U8, nodata: 0.0, ..Default::default() };
        let mut x = 7u64;
        let data = (0..64 * 64)
            .map(|i| {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                ((i % 64) / 4 + (i / 64) / 4 + (x >> 61) as usize) as u8
            })
            .collect();
        let r = Raster::from_data_native(cfg, RasterData::U8(data)).unwrap();
        let ds = crate::RasterDataset::new(r.clone(), false);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tif");
        let p = path.to_str().unwrap();
        let read = || std::fs::read(&path).unwrap();
        type Write<'a> = Box<dyn Fn(&crate::RasterWriteOptions) -> Vec<u8> + 'a>;
        let writes: [(&str, Write); 5] = [
            (
                "write_raster",
                Box::new(|o| {
                    crate::write_raster(&r, p, o).unwrap();
                    read()
                }),
            ),
            (
                "write_raster_dataset",
                Box::new(|o| {
                    crate::write_raster_dataset(&ds, p, o).unwrap();
                    read()
                }),
            ),
            ("memory::write_raster_bytes", Box::new(|o| crate::memory::write_raster_bytes(&ds, "a.tif", o).unwrap())),
            ("the in-memory codec", Box::new(|o| crate::memory::codec::write_raster(&ds, o.driver.unwrap(), o, "a").unwrap())),
            (
                "StripWriter",
                Box::new(|o| {
                    let mut w = crate::ops::strips::StripWriter::create(&ds, p, o).unwrap();
                    w.write(r.data.clone(), r.rows).unwrap();
                    w.finish().unwrap();
                    read()
                }),
            ),
        ];
        for driver in [crate::formats::RasterDriver::GTiff, crate::formats::RasterDriver::Cog] {
            let options = |zlevel: &str| crate::RasterWriteOptions {
                driver: Some(driver),
                compression: Some("DEFLATE".into()),
                creation_options: vec![("ZLEVEL".into(), zlevel.into())],
                ..Default::default()
            };
            for (name, write) in &writes {
                assert_ne!(write(&options("1")), write(&options("9")), "{name}, {driver:?}");
            }
        }
    }

    /// A Deflate chunk is compressed in its rows, as long as its width's samples of its data type:
    /// each search examines the position that many bytes back, in a tile or a strip (whatever its
    /// height), of one sample or of interleaved ones, and in a 1-bit mask's rows of packed bits.
    /// Rows that repeat the one above, each a long run of three values in turn (a period that the
    /// periodic-run probe does not take) and then bytes of its own, change the chunk.
    #[test]
    fn a_deflate_chunk_is_compressed_in_its_rows() {
        let codec = CodecParams::default();
        // (width, height, samples per pixel, data type, bytes per row)
        for (w, h, spp, t, row) in [
            (512, 512, 1, DataType::U8, 512),
            (256, 64, 3, DataType::U8, 768),
            (128, 40, 1, DataType::U16, 256),
            (100, 7, 2, DataType::F32, 800),
            // An internal mask's chunk, as `mask_image` passes it: 512 one-bit pixels a row, packed
            // in 64 bytes.
            (512, 16, 1, DataType::U8, 64),
        ] {
            // Groups of 4 equal rows: three quarters of a row of three values in turn, then bytes of
            // its own.
            let data: Vec<u8> = (0..h * row)
                .map(|i| {
                    let (group, x) = (i / row / 4, i % row);
                    let own = ((group * row + x) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 56;
                    if x < row * 3 / 4 { (group % 7) as u8 + 1 + (x % 3) as u8 * 10 } else { own as u8 }
                })
                .collect();
            let chunk = Compression::Deflate.compress(data.clone(), &ChunkInfo { w, h, spp, t, valid: (w, h) }, &codec).unwrap();
            assert_eq!(chunk, crate::deflate_compress::zlib_compress_rows(&data, row, codec.deflate_level), "{w} x {h} x {spp} {t:?}");
            assert_ne!(chunk, crate::deflate_compress::zlib_compress(&data, codec.deflate_level), "{w} x {h} x {spp} {t:?}");
        }
    }

    /// The Deflate chunks of an internal mask are compressed in their rows of bits, `ceil(width / 8)`
    /// bytes each, in tiles and in strips (the last one shorter): each chunk of a disc-shaped mask is
    /// the one its rows give, and some differ from the ones without rows.
    #[test]
    fn a_mask_is_compressed_in_its_rows_of_bits() {
        let (cols, rows) = (1000, 550);
        let mask: Vec<u8> = (0..cols * rows)
            .map(|i| {
                let (x, y) = ((i % cols) as i64, (i / cols) as i64);
                u8::from((x - 500).pow(2) + (y - 275).pow(2) < 260 * 260)
            })
            .collect();
        let level = crate::deflate_compress::Level::DEFAULT;
        for (block, row) in [((512, 512, true), 64), ((1000, 200, false), 125)] {
            let image = mask_image(&mask, cols, rows, block).unwrap();
            let mut differ = 0;
            for chunk in &image.chunks {
                let mut out = vec![0u8; 512 * 64 + 1];
                let (bits, rc) = zlib_rs::decompress_slice(&mut out, chunk, zlib_rs::InflateConfig::default());
                assert_eq!(rc, zlib_rs::ReturnCode::Ok, "{block:?}");
                assert_eq!(bits.len() % row, 0, "{block:?}");
                assert_eq!(*chunk, crate::deflate_compress::zlib_compress_rows(bits, row, level), "{block:?}");
                differ += usize::from(*chunk != crate::deflate_compress::zlib_compress(bits, level));
            }
            assert!(differ > 0, "{block:?}");
        }
    }

    /// An image's Deflate chunks are compressed in their rows through `Level::image`, which the
    /// writers of TIFF chunks call: in tiles, and in strips whose last one is shorter, of pixel- or
    /// band-interleaved bands. Each chunk is the one its rows give, and in each short last strip it
    /// differs from the one without rows: its rows repeat the row above, each three quarters of a
    /// run of three values in turn, a period that the periodic-run probe does not take.
    #[test]
    fn an_image_is_compressed_in_its_rows_in_tiles_and_strips() {
        let (cols, rows, bands) = (300, 70, 2);
        // Groups of 4 equal rows: three quarters of a row of three values in turn, then values of
        // its own.
        let data: Vec<u16> = (0..cols * rows * bands)
            .map(|i| {
                let (band, group, x) = (i / (cols * rows), i / cols % rows / 4, i % cols);
                let own = (((band * rows + group) * cols + x) as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 48;
                if x < cols * 3 / 4 { (group % 7 + 1) as u16 * 257 + (x % 3) as u16 } else { own as u16 }
            })
            .collect();
        let level = Level { cols, rows, bands, data: std::borrow::Cow::Owned(RasterData::U16(data)) };
        let codec = CodecParams::default();
        // (tile, one plane per band, bytes per row, rows of each chunk)
        for (tile, planar, row, heights) in [
            (None, false, 1200, &[30, 30, 10][..]),
            (None, true, 600, &[30, 30, 10, 30, 30, 10][..]),
            (Some((64, 64)), false, 256, &[64; 10][..]),
        ] {
            let layout =
                Layout { tile, rows_per_strip: Some(30), planar, photometric: 1, extra: vec![0], predictor: 1, parts: 1, nbits: None };
            let image = level.image(DataType::U16, Compression::Deflate, &codec, &layout).unwrap();
            assert_eq!(image.chunks.len(), heights.len(), "{tile:?}, {planar}");
            for (i, (chunk, h)) in image.chunks.iter().zip(heights).enumerate() {
                let mut out = vec![0u8; 30 * 1200 + 1];
                let (bytes, rc) = zlib_rs::decompress_slice(&mut out, chunk, zlib_rs::InflateConfig::default());
                assert_eq!((rc, bytes.len()), (zlib_rs::ReturnCode::Ok, h * row), "{tile:?}, {planar}, {i}");
                assert_eq!(*chunk, crate::deflate_compress::zlib_compress_rows(bytes, row, codec.deflate_level), "{tile:?}, {planar}, {i}");
                if *h == 10 {
                    assert_ne!(*chunk, crate::deflate_compress::zlib_compress(bytes, codec.deflate_level), "{tile:?}, {planar}, {i}");
                }
            }
        }
    }

    #[test]
    fn quality_validation_and_webp_limits_fail_before_encoding() {
        for (compression, key, value, valid) in [
            ("DEFLATE", "QUALITY", "abc", false),
            ("WEBP", "JPEG_QUALITY", "0", false),
            ("WEBP", "QUALITY", "0", true),
            ("WEBP", "QUALITY", "abc", false),
            ("WEBP", "WEBP_LOSSLESS", "maybe", false),
            ("WEBP", "WEBP_LOSSLESS", "yes", true),
            ("WEBP", "WEBP_LOSSLESS", "OFF", true),
            ("JPEG", "QUALITY", "0", false),
        ] {
            let options = crate::RasterWriteOptions {
                compression: Some(compression.into()),
                creation_options: vec![(key.into(), value.into())],
                ..Default::default()
            };
            let result = TiffOptions::from_write_options(&options, false, None);
            assert_eq!(result.is_ok(), valid);
            if let Err(error) = result {
                assert!(error.to_string().contains(key));
            }
        }
        for spp in [3, 4] {
            for length in [0, spp - 1, spp + 1, spp + 2] {
                assert!(matches!(
                    webp_encode(vec![0; length], &ChunkInfo { w: 1, h: 1, spp, t: DataType::U8, valid: (1, 1) }, &CodecParams::default()),
                    Err(Error::InvalidData(_))
                ));
            }
        }
        for (w, h) in [(0, 1), (1, 0), (16384, 1), (1, 16384)] {
            assert!(webp_encode(Vec::new(), &ChunkInfo { w, h, spp: 3, t: DataType::U8, valid: (w, h) }, &CodecParams::default()).is_err());
        }
    }

    #[test]
    fn webp_lossless_preserves_rgb_and_alpha_and_quality_changes_output() {
        for bands in [3, 4] {
            let raster = layouts_raster(bands, DataType::U8);
            let options = TiffOptions {
                compression: Compression::WebP,
                codec: CodecParams { webp_lossless: true, ..Default::default() },
                ..Default::default()
            };
            let bytes = encode(&raster, &options).unwrap();
            let back = crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap().read().unwrap();
            assert_eq!(back.raster.data.to_f64_vec(), raster.data.to_f64_vec(), "{bands} bands, including colors under transparent alpha");
        }
        let rgba = layouts_raster(4, DataType::U8);
        let encoded = encode(&rgba, &TiffOptions { compression: Compression::WebP, ..Default::default() }).unwrap();
        let decoded = crate::LazyRaster::from_tiff_bytes(encoded).unwrap().unwrap().read().unwrap();
        assert!(
            decoded.raster.data.to_f64_vec()[3 * 70 * 50..] == rgba.data.to_f64_vec()[3 * 70 * 50..],
            "lossy RGB preserves alpha exactly"
        );
        let raster = layouts_raster(3, DataType::U8);
        let write = |quality| {
            encode(
                &raster,
                &TiffOptions {
                    compression: Compression::WebP,
                    codec: CodecParams { webp_quality: quality, ..Default::default() },
                    ..Default::default()
                },
            )
            .unwrap()
        };
        assert_ne!(write(10), write(95));
    }

    #[test]
    fn jpeg_xl_roundtrips_native_sample_depths_and_layouts() {
        for data_type in [DataType::U8, DataType::U16] {
            for bands in [1, 3] {
                let cfg = wbraster::RasterConfig { cols: 35, rows: 19, bands, data_type, ..Default::default() };
                let data = match data_type {
                    DataType::U8 => RasterData::U8((0..35 * 19 * bands).map(|i| (i * 37 % 256) as u8).collect()),
                    _ => RasterData::U16((0..35 * 19 * bands).map(|i| (i * 113 % 65536) as u16).collect()),
                };
                let raster = Raster::from_data_native(cfg, data).unwrap();
                for tiled in [false, true] {
                    let bytes = encode(
                        &raster,
                        &TiffOptions { compression: Compression::Jxl, tile_size: tiled.then_some(16), ..Default::default() },
                    )
                    .unwrap();
                    let back = crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap().read().unwrap();
                    assert_eq!(back.raster.data.to_f64_vec(), raster.data.to_f64_vec(), "{data_type:?} x{bands} tiled={tiled}");
                }
            }
        }
    }

    /// 512 x 512 JPEG XL tiles of noise above a long uniform run (#1355): images smaller than the
    /// tile, whose zero padding is the run, and a full tile whose lower rows are constant. The
    /// noise makes the encoder's sampled statistics give a run's code many bits, so a run's code,
    /// extra bits and symbol together exceed 32 bits.
    fn jxl_noise_over_uniform_runs() -> Vec<(String, Raster, Vec<u8>)> {
        let mut cases = Vec::new();
        for (cols, rows, noise_rows) in [(480, 300, 300), (512, 300, 300), (480, 480, 480), (512, 512, 300)] {
            for bands in [1, 3] {
                for data_type in [DataType::U8, DataType::U16] {
                    let mut state = 0x9E37_79B9_7F4A_7C15u64;
                    // 16 bits of noise in the first `noise_rows` rows, then the uniform value 7.
                    let mut noise = |i: usize| {
                        state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                        (i / cols % rows < noise_rows).then_some(state >> 48)
                    };
                    let n = cols * rows * bands;
                    let data = match data_type {
                        DataType::U8 => RasterData::U8((0..n).map(|i| noise(i).map_or(7, |v| (v >> 8) as u8)).collect()),
                        _ => RasterData::U16((0..n).map(|i| noise(i).map_or(7, |v| v as u16)).collect()),
                    };
                    let cfg = wbraster::RasterConfig { cols, rows, bands, data_type, ..Default::default() };
                    let raster = Raster::from_data_native(cfg, data).unwrap();
                    let options = TiffOptions { compression: Compression::Jxl, tile_size: Some(512), ..Default::default() };
                    let bytes = encode(&raster, &options).unwrap();
                    cases.push((format!("{cols}x{rows}x{bands}_{data_type:?}"), raster, bytes));
                }
            }
        }
        cases
    }

    #[test]
    fn jpeg_xl_encoder_errors_read_as_one_line() {
        let cfg = wbraster::RasterConfig { cols: 1, rows: 5, bands: 1, data_type: DataType::U8, ..Default::default() };
        let raster = Raster::from_data_native(cfg, RasterData::U8(vec![3; 5])).unwrap();
        let error = encode(&raster, &TiffOptions { compression: Compression::Jxl, ..Default::default() }).unwrap_err().to_string();
        assert!(error.ends_with("JPEG XL: The width is less than 2"), "{error:?}");
    }

    #[test]
    fn jpeg_xl_tiles_of_noise_over_uniform_runs_read_back() {
        let failed: Vec<_> = jxl_noise_over_uniform_runs()
            .into_iter()
            .filter_map(|(name, raster, bytes)| match crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap().read() {
                Ok(back) if back.raster.data.to_f64_vec() == raster.data.to_f64_vec() => None,
                Ok(_) => Some(format!("{name}: values differ")),
                Err(e) => Some(format!("{name}: {e}")),
            })
            .collect();
        assert!(failed.is_empty(), "{failed:#?}");
    }

    /// UInt16 JPEG XL tiles whose residuals take the widest codes: a 512 x 512 RGB tile alternating
    /// (65535, 0, 0) and (0, 0, 65535), whose chroma residuals need 17 extra bits after an 8-bit
    /// prefix (25 bits a sample), and a 256 x 256 gray tile alternating 0 and 65535, a single
    /// group whose first channel shares the header's writer. Each overran its writer and panicked.
    fn jxl_uint16_widest_codes() -> Vec<(String, Raster, Vec<u8>)> {
        [(512, 3), (256, 1)]
            .into_iter()
            .map(|(size, bands)| {
                let data: Vec<u16> = (0..bands)
                    .flat_map(|band| {
                        (0..size * size).map(move |i| {
                            let odd = (i / size + i % size) % 2 == 1;
                            match bands {
                                1 => u16::from(odd) * 65535,
                                _ => u16::from((band == 0) != odd && band != 1) * 65535,
                            }
                        })
                    })
                    .collect();
                let cfg = wbraster::RasterConfig { cols: size, rows: size, bands, data_type: DataType::U16, ..Default::default() };
                let raster = Raster::from_data_native(cfg, RasterData::U16(data)).unwrap();
                let options = TiffOptions { compression: Compression::Jxl, tile_size: Some(size as u32), ..Default::default() };
                let bytes = encode(&raster, &options).unwrap();
                (format!("checkerboard_{size}x{size}x{bands}_U16"), raster, bytes)
            })
            .collect()
    }

    #[test]
    fn jpeg_xl_uint16_tiles_of_the_widest_codes_read_back() {
        for (name, raster, bytes) in jxl_uint16_widest_codes() {
            let back = crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap().read().unwrap();
            assert!(back.raster.data.to_f64_vec() == raster.data.to_f64_vec(), "{name}");
        }
    }

    /// GDAL (reference: 3.13.3, libjxl through libtiff) decodes the tiles of
    /// `jxl_noise_over_uniform_runs` and `jxl_uint16_widest_codes` to the written samples.
    #[cfg(not(target_family = "wasm"))]
    #[test]
    #[ignore = "requires GDAL 3.13.3's Python bindings as python3"]
    fn gdal_reads_jpeg_xl_tiles_of_noise_over_uniform_runs() {
        let dir = tempfile::tempdir().unwrap();
        let cases: Vec<_> = jxl_noise_over_uniform_runs().into_iter().chain(jxl_uint16_widest_codes()).collect();
        let paths: Vec<_> = cases
            .iter()
            .map(|(name, _, bytes)| {
                let path = dir.path().join(format!("{name}.tif"));
                std::fs::write(&path, bytes).unwrap();
                path
            })
            .collect();
        // Each file's samples as GDAL reads them, band after band, in native byte order.
        let script = "import sys\nfrom osgeo import gdal\ngdal.UseExceptions()\nfor p in sys.argv[1:]:\n open(p + '.raw', 'wb').write(gdal.Open(p).ReadRaster())\n";
        let output = std::process::Command::new("python3").arg("-c").arg(script).args(&paths).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        for ((name, raster, _), path) in cases.iter().zip(&paths) {
            let raw = std::fs::read(path.with_extension("tif.raw")).unwrap();
            let values: Vec<f64> = match raster.data_type {
                DataType::U8 => raw.iter().map(|&v| f64::from(v)).collect(),
                _ => raw.as_chunks::<2>().0.iter().map(|b| f64::from(u16::from_ne_bytes(*b))).collect(),
            };
            assert!(values == raster.data.to_f64_vec(), "{name}");
        }
    }

    #[test]
    fn gcp_only_copies_preserve_all_points_without_an_affine() {
        let r = layouts_raster(1, DataType::U8);
        let gcps = (0..1599)
            .map(|i| crate::Gcp {
                pixel: (i % 41) as f64 + 0.5,
                line: (i / 41) as f64 + 0.5,
                x: 500000.0 + (i % 41) as f64 * 10.0,
                y: 4000000.0 - (i / 41) as f64 * 10.0,
                ..Default::default()
            })
            .collect();
        let bytes = encode(&r, &TiffOptions { gcps, cog: true, tile_size: Some(16), ..Default::default() }).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("input.tif");
        std::fs::write(&input, &bytes).unwrap();
        let mut lazy = crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap();
        let header = lazy.header();
        assert_eq!(header.gcps.len(), 1599);
        assert!(header.raster.data.is_empty());
        let cropped = lazy.read_window(crate::Window::new(1, 2, 2, 2), &Default::default()).unwrap();
        assert_eq!((cropped.gcps[0].pixel, cropped.gcps[0].line), (-0.5, -1.5));
        let (width, height) = lazy.overviews()[0];
        let overview =
            lazy.read_window(crate::Window::new(1, 2, 2, 2), &crate::WindowOptions { overview: Some(0), ..Default::default() }).unwrap();
        assert_eq!(overview.gcps[0].pixel, header.gcps[0].pixel * width as f64 / r.cols as f64 - 1.0);
        assert_eq!(overview.gcps[0].line, header.gcps[0].line * height as f64 / r.rows as f64 - 2.0);
        let whole = lazy.read().unwrap();
        assert_eq!(whole.gcps, header.gcps);
        let mut memory = crate::LazyRaster::from(whole.clone());
        let mut shared = lazy.shared();
        for reader in [&mut memory, &mut shared] {
            let window = reader.read_window(crate::Window::new(1, 2, 2, 2), &Default::default()).unwrap();
            assert_eq!(window.gcps, cropped.gcps);
            assert_eq!(window.band_metadata, cropped.band_metadata);
            assert_eq!(window.raster.data.to_f64_vec(), cropped.raster.data.to_f64_vec());
        }

        let dir = tempfile::tempdir().unwrap();
        let options = crate::RasterWriteOptions { driver: Some(crate::formats::RasterDriver::Cog), ..Default::default() };
        for (name, streamed) in [("streamed.tif", true), ("whole.tif", false)] {
            let path = dir.path().join(name);
            if streamed {
                let mut file = crate::open_raster_lazy(input.to_str().unwrap()).unwrap();
                file.write(path.to_str().unwrap(), &options).unwrap();
            } else {
                crate::write_raster_dataset(&whole, path.to_str().unwrap(), &options).unwrap();
            }
            let tags = crate::geotiff::read_geo_tags(&path).unwrap();
            assert_eq!(tags.gcps, header.gcps);
            assert!(tags.geo_transform.is_none());
        }
        // Inherited TIFF-only georeferencing must not make other output formats fail.
        for streamed in [false, true] {
            let path = dir.path().join("copy.asc");
            let options = crate::RasterWriteOptions::default();
            if streamed {
                crate::open_raster_lazy(input.to_str().unwrap()).unwrap().write(path.to_str().unwrap(), &options).unwrap();
            } else {
                crate::write_raster_dataset(&whole, path.to_str().unwrap(), &options).unwrap();
            }
            let actual = crate::open_raster(path.to_str().unwrap()).unwrap();
            assert_eq!(actual.raster.data.to_f64_vec(), whole.raster.data.to_f64_vec());
        }
        let explicit = crate::RasterWriteOptions { gcps: whole.gcps.clone(), ..Default::default() };
        assert!(crate::write_raster_dataset(&whole, dir.path().join("explicit.asc").to_str().unwrap(), &explicit).is_err());
        let assigned = dir.path().join("assigned.tif");
        let gt = [10., 2., 0., 30., 0., -2.];
        crate::write_raster_dataset(&whole, assigned.to_str().unwrap(), &crate::RasterWriteOptions { geo_transform: Some(gt), ..options })
            .unwrap();
        let tags = crate::geotiff::read_geo_tags(&assigned).unwrap();
        assert!(tags.gcps.is_empty());
        assert_eq!(tags.geo_transform, Some(gt));
        for gt in [gt, [10., 2., 0.1, 30., 0.2, -2.]] {
            let mut assigned_ds = whole.clone();
            assigned_ds.set_geo_transform(gt);
            assert!(assigned_ds.gcps.is_empty());
            let options = crate::RasterWriteOptions::default();
            crate::write_raster_dataset(&assigned_ds, assigned.to_str().unwrap(), &options).unwrap();
            let tags = crate::geotiff::read_geo_tags(&assigned).unwrap();
            assert!(tags.gcps.is_empty());
            assert_eq!(tags.geo_transform, Some(gt));
            let bytes = crate::memory::write_raster_bytes(&assigned_ds, "assigned.tif", &options).unwrap();
            let tags = crate::geotiff::read_geo_tags_from(std::io::Cursor::new(bytes)).unwrap();
            assert!(tags.gcps.is_empty());
            assert_eq!(tags.geo_transform, Some(gt));
        }
    }

    #[test]
    fn color_interpretations_survive_lazy_reads_and_band_selection() {
        let rgb = layouts_raster(3, DataType::U8);
        let bytes = encode(&rgb, &TiffOptions { compression: Compression::Jpeg, photometric: Some(6), ..Default::default() }).unwrap();
        let jpeg = crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap().header();
        assert_eq!((1..=3).map(|b| jpeg.band_color_interp(b)).collect::<Vec<_>>(), ["Red", "Green", "Blue"]);

        // Under PHOTOMETRIC=RGB, band 1's Undefined is ignored, as GDAL ignores it; band 4's
        // Alpha, which no extra sample implies, is kept in GDAL_METADATA.
        let r = layouts_raster(4, DataType::U8);
        let undefined = crate::BandMetadata { color_interp: Some("Undefined".into()), ..Default::default() };
        let alpha = crate::BandMetadata { color_interp: Some("Alpha".into()), ..Default::default() };
        let opts = TiffOptions {
            photometric: Some(2),
            band_metadata: vec![undefined.clone(), undefined.clone(), undefined, alpha],
            ..Default::default()
        };
        let mut lazy = crate::LazyRaster::from_tiff_bytes(encode(&r, &opts).unwrap()).unwrap().unwrap();
        let header = lazy.header();
        assert!(header.raster.data.is_empty());
        assert_eq!((1..=4).map(|b| header.band_color_interp(b)).collect::<Vec<_>>(), ["Red", "Green", "Blue", "Alpha"]);
        let subset =
            lazy.read_window(crate::Window::new(1, 1, 2, 2), &crate::WindowOptions { bands: vec![4, 2, 1], ..Default::default() }).unwrap();
        assert_eq!((1..=3).map(|b| subset.band_color_interp(b)).collect::<Vec<_>>(), ["Alpha", "Green", "Red"]);
    }

    #[test]
    fn writes_band_metadata_color_tables_and_overviews() {
        let r = layouts_raster(1, DataType::U8);
        let options = crate::RasterWriteOptions {
            band_metadata: vec![crate::BandMetadata {
                description: Some("a<b".into()),
                scale: Some(2.0),
                items: vec![("k".into(), "c&d &amp;literal".into())],
                ..Default::default()
            }],
            color_table: vec![[255, 0, 0, 255], [0, 255, 0, 255]],
            overviews: vec![2, 4],
            overview_resampling: Some("average".into()),
            creation_options: vec![("GEOREFERENCED".into(), "NO".into())],
            ..Default::default()
        };
        let opts = TiffOptions::from_write_options(&options, false, None).unwrap().unwrap();
        assert_eq!(opts.photometric, Some(3));
        let bytes = encode(&r, &opts).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains(r#"<Item name="DESCRIPTION" sample="0" role="description">a&amp;lt;b</Item>"#));
        assert!(text.contains(r#"<Item name="k" sample="0">c&amp;amp;d &amp;amp;amp;literal</Item>"#));
        let tags = crate::geotiff::read_geo_tags_from(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(tags.geo_transform, None);
        assert_eq!(tags.band_metadata, vec![(0, "k".to_string(), "c&d &amp;literal".to_string())]);
        // The scale is read back as the band's (GDAL's role="scale"); no offset was written.
        assert_eq!((tags.band_properties[0].scale, tags.band_properties[0].offset), (Some(2.0), None));
        let mut lazy = crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap();
        assert_eq!(lazy.overviews(), vec![(35, 25), (18, 13)]);
        let o = lazy.read_window(crate::Window::new(0, 0, 35, 25), &crate::WindowOptions { bands: vec![], overview: Some(0) }).unwrap();
        let wbraster::raster::RasterData::U8(v) = &o.raster.data else { panic!() };
        let wbraster::raster::RasterData::U8(src) = &r.data else { panic!() };
        // The average of each 2x2 block, rounded half up.
        let avg = |r0: usize, c0: usize| {
            let s: u32 = [(0, 0), (0, 1), (1, 0), (1, 1)].iter().map(|(a, b)| src[(r0 + a) * 70 + c0 + b] as u32).sum();
            ((s as f64 / 4.0) + 0.5).floor() as u8
        };
        assert_eq!(v[0], avg(0, 0));
        assert_eq!(v[3 * 35 + 7], avg(6, 14));
    }

    // The tests of the COG overview model of #165: each level reduced from the source,
    // 33x18, 17x9 and 9x5 for 65x35 with 16-pixel tiles. terrio's COG overviews are GDAL's
    // COG driver's (gdal_overview.rs): 32x17 and 16x8 there, each level from the previous one.
    // GDAL 3.12.1 agrees with terrio on 22 of the 23 scenarios below and with the model of #165
    // on none, so the tests whose oracle is that model are ignored here, and their scenarios run
    // against GDAL itself in python/tests/test_cog_overview_scenarios.py.
    // An odd-sized source large enough for factors 2, 4 and 8 with 16-pixel COG tiles.
    fn overview_fixture(bands: usize, data_type: DataType, data: RasterData) -> Raster {
        Raster::from_data_native(wbraster::RasterConfig { cols: 65, rows: 35, bands, data_type, ..Default::default() }, data).unwrap()
    }

    /// GDAL 3.13.3 own-mask association is per level, including an incomplete mask pyramid.
    #[test]
    fn copied_overviews_keep_own_masks_beside_unmasked_levels() {
        for base_mask in [true, false] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("partial-masks.tif");
            let raster = Raster::from_data_native(
                wbraster::RasterConfig { cols: 32, rows: 32, bands: 1, data_type: DataType::U8, ..Default::default() },
                RasterData::U8(vec![7; 32 * 32]),
            )
            .unwrap();
            let options = TiffOptions::default();
            let layout = plan(&raster, &options).unwrap().layout;
            let mut images = Vec::new();
            for (i, size) in [32, 16, 8].into_iter().enumerate() {
                let level = Level { cols: size, rows: size, bands: 1, data: std::borrow::Cow::Owned(RasterData::U8(vec![7; size * size])) };
                let mut image = level.image(DataType::U8, options.compression, &options.codec, &layout).unwrap();
                if i > 0 {
                    image.tags.push((254, Value::Longs(vec![1])));
                    image.tags.sort_by_key(|(tag, _)| *tag);
                }
                images.push(image);
                if i == 1 || (i == 0 && base_mask) {
                    let mut mask = mask_image(&vec![if i == 0 { 255 } else { 0 }; size * size], size, size, (size, size, false)).unwrap();
                    if i > 0 {
                        for (tag, value) in &mut mask.tags {
                            if *tag == 254 {
                                *value = Value::Longs(vec![5]);
                            }
                        }
                    }
                    images.push(mask);
                }
            }
            let mut bytes = Vec::new();
            assemble(images, false, false, &mut bytes).unwrap();
            std::fs::write(&path, bytes).unwrap();
            let source = crate::LazyRaster::open_tiff(path.to_str().unwrap()).unwrap().unwrap();
            assert_eq!(source.has_mask(), base_mask);
            assert_eq!(source.overviews(), vec![(16, 16), (8, 8)]);
            assert!(source.level_has_own_mask(0));
            assert!(!source.level_has_own_mask(1));
            let mut levels = vec![Level::from_raster(&raster)];
            let mut masks = if base_mask { vec![vec![255; 32 * 32]] } else { Vec::new() };
            copy_source_overviews(path.to_str().unwrap(), &raster, &options, &mut levels, &mut masks).unwrap();
            assert_eq!(levels.len(), 3);
            assert_eq!(masks.iter().map(Vec::len).collect::<Vec<_>>(), vec![if base_mask { 32 * 32 } else { 0 }, 16 * 16, 0]);
            assert!(masks[1].iter().all(|&value| value == 0));
            assert!(masks[2].is_empty());
            if !base_mask {
                // A caller-supplied mask replaces source validity at every level.
                let supplied = vec![255; 32 * 32];
                let options = TiffOptions { mask: Some(supplied.clone()), ..options };
                let mut levels = vec![Level::from_raster(&raster)];
                let mut masks = vec![supplied];
                copy_source_overviews(path.to_str().unwrap(), &raster, &options, &mut levels, &mut masks).unwrap();
                assert_eq!(masks.iter().map(Vec::len).collect::<Vec<_>>(), [32 * 32, 16 * 16, 8 * 8]);
                assert!(masks.iter().flatten().all(|&value| value == 255));
            }
        }
    }

    /// GDAL 3.13.3 generates missing mask levels when the base exceeds an output tile.
    #[test]
    fn copied_overviews_generate_masks_for_sources_larger_than_a_tile() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("generated-masks.tif");
        let raster = Raster::from_data_native(
            wbraster::RasterConfig { cols: 64, rows: 64, bands: 1, data_type: DataType::U8, ..Default::default() },
            RasterData::U8(vec![7; 64 * 64]),
        )
        .unwrap();
        let options = TiffOptions { tile_size: Some(16), ..Default::default() };
        let layout = plan(&raster, &options).unwrap().layout;
        let base_mask: Vec<u8> = (0..64 * 64).map(|index| if index % 64 < 32 { 255 } else { 0 }).collect();
        let mut images = Vec::new();
        for (index, size) in [64, 32, 16].into_iter().enumerate() {
            let level = Level { cols: size, rows: size, bands: 1, data: std::borrow::Cow::Owned(RasterData::U8(vec![7; size * size])) };
            let mut image = level.image(DataType::U8, options.compression, &options.codec, &layout).unwrap();
            if index > 0 {
                image.tags.push((254, Value::Longs(vec![1])));
                image.tags.sort_by_key(|(tag, _)| *tag);
            }
            images.push(image);
            if index == 0 {
                images.push(mask_image(&base_mask, size, size, (size, size, false)).unwrap());
            }
        }
        let mut bytes = Vec::new();
        assemble(images, false, false, &mut bytes).unwrap();
        std::fs::write(&path, bytes).unwrap();
        let mut levels = vec![Level::from_raster(&raster)];
        let mut masks = vec![base_mask];
        copy_source_overviews(path.to_str().unwrap(), &raster, &options, &mut levels, &mut masks).unwrap();
        assert_eq!(masks.len(), 3);
        for (mask, width) in masks[1..].iter().zip([32, 16]) {
            assert_eq!(mask.len(), width * width);
            assert!(
                mask.chunks_exact(width)
                    .all(|row| row[..width / 2].iter().all(|&value| value == 255) && row[width / 2..].iter().all(|&value| value == 0))
            );
        }
    }

    /// Streamed mask values are read as the whole writer reads them: any nonzero value is valid
    /// and weighted as 255, so mixed 1 and 255 valid values give the whole writer's overviews.
    #[test]
    fn streamed_mask_values_are_normalized_as_the_whole_writer_normalizes_them() {
        let dir = tempfile::tempdir().unwrap();
        let (cols, rows) = (131, 259);
        let data = RasterData::U8((0..cols * rows).map(|i| (i * 7 % 251) as u8).collect());
        let mask: Vec<u8> = (0..cols * rows)
            .map(|i| {
                if i % cols < 31 || i % 13 < 5 {
                    0
                } else if i % 3 == 0 {
                    255
                } else {
                    1
                }
            })
            .collect();
        let normalized: Vec<u8> = mask.iter().map(|v| if *v != 0 { 255 } else { 0 }).collect();
        let raster = Raster::from_data_native(
            wbraster::RasterConfig { cols, rows, bands: 1, data_type: DataType::U8, ..Default::default() },
            data.clone(),
        )
        .unwrap();
        let opts = TiffOptions {
            cog: true,
            tile_size: Some(16),
            cog_resampling: Some(Method::Cubic),
            mask: Some(normalized.clone()),
            ..Default::default()
        };
        let whole = encode(&raster, &opts).unwrap();
        let path = dir.path().join("masked.tif");
        let mut writer = CogWriter::create(path.to_str().unwrap(), &raster, &opts).unwrap();
        writer.write_rows_masked(slice_rows(&data, rows, 0..rows, cols, 1), rows, Some(mask)).unwrap();
        writer.finish().unwrap();
        assert_eq!(std::fs::read(path).unwrap(), whole);
    }

    /// A mask without a dataset (rasterio's buffered COG writes hold it in a MEM dataset) has its
    /// first overview weighted by its own values, as a file's mask, and its later ones computed
    /// from the Byte values of the level before with all-valid weights, where a file's mask keeps
    /// weighting each level by its own values.
    #[test]
    fn a_mask_without_a_dataset_cascades_as_gdal_from_a_mem_dataset() {
        let (w, h) = (65usize, 35usize);
        let mask: Vec<u8> = (0..w * h).map(|i| if (i * 7919 + i / w * 31) % 13 < 5 { 0 } else { 255 }).collect();
        let file = TiffOptions { cog: true, ..Default::default() };
        let mem = TiffOptions { mask_without_dataset: true, ..file.clone() };
        let (s1, s2, block) = ((33, 18), (17, 9), gdal_overview::ovr_block(None));
        let first = |o: &TiffOptions| mask_levels(o, Method::Cubic, std::slice::from_ref(&mask), &[], 0, (w, h), s1, block);
        let raw1 = first(&mem);
        assert_eq!(raw1, first(&file), "the first overview, from the full resolution");
        let bits1: Vec<u8> = raw1.iter().map(|v| if *v != 0 { 255 } else { 0 }).collect();
        let masks = [mask.clone(), bits1];
        let raws = [raw1.clone()];
        let second = |o: &TiffOptions| mask_levels(o, Method::Cubic, &masks, &raws, 1, s1, s2, block);
        let all_valid = vec![255u8; raw1.len()];
        let plan = gdal_overview::Plan::new(Method::Cubic, s1, s2, 1, block, DataType::U8);
        let weights = gdal_overview::Weights { alpha: None, alpha_masks: false, mask: Some(&all_valid) };
        let RasterData::U8(unweighted) = gdal_overview::level(&plan, &RasterData::U8(raw1.clone()), 1, None, weights) else {
            unreachable!()
        };
        assert_eq!(second(&mem), unweighted);
        assert_eq!(second(&file), gdal_overview::mask_level(Method::Cubic, &masks[1], s1, s2, block));
        assert_ne!(second(&mem), second(&file), "the fixture tells the two apart");
    }

    /// The data of a COG whose mask has no dataset are weighted by the Byte values of the mask
    /// overview they are computed from, as GDAL keeps it in its temporary file, not by the bits
    /// written. The first mask overview, weighted by itself, is all 0 or 255; the
    /// second is not, so the third data overview tells the two apart.
    #[test]
    fn data_overviews_are_weighted_by_a_mask_without_a_dataset_as_gdal_keeps_it() {
        let (w, h) = (130usize, 70usize);
        let data: Vec<u8> = (0..w * h).map(|i| ((i % w) * 3 + (i / w) * 5) as u8).collect();
        let mask: Vec<u8> = (0..w * h).map(|i| if (i * 7919 + i / w * 31) % 13 < 5 { 0 } else { 255 }).collect();
        let cfg = wbraster::RasterConfig { cols: w, rows: h, bands: 1, data_type: DataType::U8, ..Default::default() };
        let raster = Raster::from_data_native(cfg, RasterData::U8(data)).unwrap();
        let options = crate::RasterWriteOptions {
            tile_size: Some(16),
            compression: Some("NONE".into()),
            overview_resampling: Some("cubic".into()),
            ..Default::default()
        };
        let mut opts = TiffOptions::from_write_options(&options, true, None).unwrap().unwrap();
        opts.mask = Some(mask.clone());
        opts.mask_without_dataset = true;
        let mut lazy = crate::LazyRaster::from_tiff_bytes(encode(&raster, &opts).unwrap()).unwrap().unwrap();
        let sizes = lazy.overviews();
        assert_eq!(sizes.len(), 3);
        let mut stored = |level: usize| {
            let (cols, rows) = sizes[level];
            let options = crate::WindowOptions { bands: vec![], overview: Some(level) };
            let RasterData::U8(v) = lazy.read_window(crate::Window::new(0, 0, cols, rows), &options).unwrap().raster.data else {
                unreachable!()
            };
            v
        };
        let block = gdal_overview::ovr_block(None);
        let bits = |raw: &[u8]| -> Vec<u8> { raw.iter().map(|v| if *v != 0 { 255 } else { 0 }).collect() };
        let raw1 = mask_levels(&opts, Method::Cubic, std::slice::from_ref(&mask), &[], 0, (w, h), sizes[0], block);
        assert_eq!(raw1, bits(&raw1), "the first mask overview is all 0 or 255");
        let masks = [mask.clone(), bits(&raw1)];
        let raw2 = mask_levels(&opts, Method::Cubic, &masks, std::slice::from_ref(&raw1), 1, sizes[0], sizes[1], block);
        let second = RasterData::U8(stored(1));
        let plan = gdal_overview::Plan::new(Method::Cubic, sizes[1], sizes[2], 1, block, DataType::U8);
        let weighted = |m: &[u8]| {
            let weights = gdal_overview::Weights { alpha: None, alpha_masks: false, mask: Some(m) };
            let RasterData::U8(v) = gdal_overview::level(&plan, &second, 1, None, weights) else { unreachable!() };
            v
        };
        assert_ne!(weighted(&raw2), weighted(&bits(&raw2)), "the fixture tells the two apart");
        assert_eq!(stored(2), weighted(&raw2));
    }

    /// Whether each image of a TIFF has the `GDAL_NODATA` tag.
    fn nodata_tags(bytes: Vec<u8>) -> Vec<bool> {
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(bytes)).unwrap();
        let mut found = Vec::new();
        for i in 0.. {
            if dec.seek_to_image(i).is_err() {
                break;
            }
            found.push(dec.find_tag(tiff::tags::Tag::Unknown(42113)).unwrap().is_some());
        }
        found
    }

    /// GDAL 3.12.1 writes `GDAL_NODATA` on the overview images it builds from a nodata held as a
    /// double, not on those of Int64 and UInt64 rasters (`GetOverviewParameters`): a COG written
    /// whole or in rows has the tag on its full-resolution image, and on its overviews but for
    /// those types.
    #[test]
    fn overviews_carry_nodata_as_gdal_writes_it() {
        let n = 65 * 35;
        for (data_type, data, on_overviews) in [
            (DataType::U16, RasterData::U16((0..n).map(|i| (i % 9) as u16).collect()), true),
            (DataType::F32, RasterData::F32((0..n).map(|i| (i % 9) as f32).collect()), true),
            (DataType::U64, RasterData::U64((0..n).map(|i| (i % 9) as u64).collect()), false),
            (DataType::I64, RasterData::I64((0..n).map(|i| (i % 9) as i64).collect()), false),
        ] {
            let raster = overview_fixture(1, data_type, data);
            let options = crate::RasterWriteOptions { tile_size: Some(16), compression: Some("NONE".into()), ..Default::default() };
            let opts = TiffOptions::from_write_options(&options, true, Some(7.0)).unwrap().unwrap();
            let whole = nodata_tags(encode(&raster, &opts).unwrap());
            assert!(whole.len() > 1, "{data_type:?}: overviews");
            assert!(whole[0], "{data_type:?}: the full-resolution image");
            assert!(whole[1..].iter().all(|t| *t == on_overviews), "{data_type:?}, written whole: {whole:?}");
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("rows.tif");
            let mut writer = CogWriter::create(path.to_str().unwrap(), &raster, &opts).unwrap();
            writer.write_rows(raster.data.clone(), 35).unwrap();
            writer.finish().unwrap();
            assert_eq!(nodata_tags(std::fs::read(path).unwrap()), whole, "{data_type:?}, written in rows");
        }
    }

    fn stored_cog_overviews(raster: &Raster, mut options: crate::RasterWriteOptions, nodata: Option<f64>) -> Vec<(usize, RasterData)> {
        options.tile_size = Some(16);
        options.compression = Some("NONE".into());
        let opts = TiffOptions::from_write_options(&options, true, nodata).unwrap().unwrap();
        let mut lazy = crate::LazyRaster::from_tiff_bytes(encode(raster, &opts).unwrap()).unwrap().unwrap();
        let dimensions = lazy.overviews();
        assert_eq!(dimensions, vec![(33, 18), (17, 9), (9, 5)]);
        dimensions
            .into_iter()
            .enumerate()
            .map(|(level, (cols, rows))| {
                let overview = lazy
                    .read_window(crate::Window::new(0, 0, cols, rows), &crate::WindowOptions { bands: vec![], overview: Some(level) })
                    .unwrap();
                (1usize << (level + 1), overview.raster.data)
            })
            .collect()
    }

    fn overview_source_block(x: usize, y: usize, factor: usize) -> impl Iterator<Item = usize> {
        (y * factor..((y + 1) * factor).min(35)).flat_map(move |sy| (x * factor..((x + 1) * factor).min(65)).map(move |sx| sy * 65 + sx))
    }

    #[test]
    #[ignore = "#165's model, not GDAL's: run against GDAL by python/tests/test_cog_overview_scenarios.py"]
    fn cog_nearest_overviews_sample_original_block_centers_and_palette_classes() {
        let classes = [0, 3, 9, 17];
        let source: Vec<u8> = (0..35).flat_map(|y| (0..65).map(move |x| classes[(x + 2 * y) % classes.len()])).collect();
        let raster = overview_fixture(1, DataType::U8, RasterData::U8(source.clone()));
        let options = crate::RasterWriteOptions {
            overview_resampling: Some("nearest".into()),
            color_table: (0..18).map(|i| [i * 13, 255 - i * 13, i * 7, 255]).collect(),
            ..Default::default()
        };
        for (factor, data) in stored_cog_overviews(&raster, options, None) {
            let RasterData::U8(actual) = data else { panic!("expected Byte overview") };
            let cols = 65usize.div_ceil(factor);
            let rows = 35usize.div_ceil(factor);
            let expected: Vec<u8> = (0..rows)
                .flat_map(|y| {
                    let source = &source;
                    (0..cols).map(move |x| source[(y * factor + factor / 2).min(34) * 65 + (x * factor + factor / 2).min(64)])
                })
                .collect();
            assert_eq!(actual, expected, "nearest factor {factor}");
            assert!(actual.iter().all(|v| classes.contains(v)));
        }
    }

    #[test]
    #[ignore = "#165's model, not GDAL's: run against GDAL by python/tests/test_cog_overview_scenarios.py"]
    fn cog_average_overviews_weight_rgba_and_gray_alpha_colors() {
        for bands in [2, 4] {
            let pixels = 65 * 35;
            let mut source = vec![0u8; pixels * bands];
            for y in 0..35 {
                for x in 0..65 {
                    let i = y * 65 + x;
                    let pattern = (y % 2) * 2 + x % 2;
                    for band in 0..bands - 1 {
                        source[band * pixels + i] = [150, 20, 60, 110][pattern] + band as u8 * 17;
                    }
                    source[(bands - 1) * pixels + i] = if (8..16).contains(&x) && y < 8 { 0 } else { [0, 64, 128, 255][pattern] };
                }
            }
            let raster = overview_fixture(bands, DataType::U8, RasterData::U8(source.clone()));
            let options = crate::RasterWriteOptions {
                overview_resampling: Some("average".into()),
                creation_options: vec![
                    ("ALPHA".into(), "YES".into()),
                    ("PHOTOMETRIC".into(), if bands == 4 { "RGB".into() } else { "MINISBLACK".into() }),
                ],
                ..Default::default()
            };
            for (factor, data) in stored_cog_overviews(&raster, options, None) {
                let RasterData::U8(actual) = data else { panic!("expected Byte overview") };
                let cols = 65usize.div_ceil(factor);
                let rows = 35usize.div_ceil(factor);
                let mut expected = vec![0u8; cols * rows * bands];
                for y in 0..rows {
                    for x in 0..cols {
                        let block: Vec<usize> = overview_source_block(x, y, factor).collect();
                        let alpha_sum: u64 = block.iter().map(|&i| source[(bands - 1) * pixels + i] as u64).sum();
                        for band in 0..bands - 1 {
                            let weighted: u64 =
                                block.iter().map(|&i| source[band * pixels + i] as u64 * source[(bands - 1) * pixels + i] as u64).sum();
                            expected[band * cols * rows + y * cols + x] =
                                if alpha_sum == 0 { 0 } else { (weighted as f64 / alpha_sum as f64).round() as u8 };
                        }
                        expected[(bands - 1) * cols * rows + y * cols + x] = (alpha_sum as f64 / block.len() as f64).round() as u8;
                    }
                }
                assert_eq!(actual, expected, "{bands} bands, average factor {factor}");
            }
        }
    }

    fn assert_cog_float_average_source_order(data_type: DataType, bands: usize) {
        let (cols, rows, pixels) = (65, 35, 65 * 35);
        let nodata = -9999.0;
        let alpha = bands > 1;
        let mut source = vec![0.0; pixels * bands];
        for band in 0..bands {
            for y in 0..rows {
                for x in 0..cols {
                    let i = y * cols + x;
                    let pattern = (y % 2) * 2 + x % 2;
                    let value = if alpha && band + 1 == bands {
                        if (24..32).contains(&x) && y < 8 { 0.0 } else { [0.0, 0.125, 0.5, 1.0][(x + 3 * y) % 4] }
                    } else if (16..24).contains(&x) && y < 8 {
                        // Values near a Float32 rounding boundary, without intermediate rounding.
                        1.0 + [0.0, 2.0f64.powi(-23), 0.0, 0.0][pattern]
                    } else {
                        // Cancellation makes reassociation, column-major traversal and combining
                        // separately summed rows observably different even after a Float32 cast.
                        let large = 2.0f64.powi(60) * (band + 1) as f64;
                        [large, (i % 29 + 1) as f64 * 0.125, -large, (i % 31 + 1) as f64 * 0.25][pattern]
                    };
                    source[band * pixels + i] = if (8..16).contains(&x) && y < 8 || (i + band * 13) % 47 == 0 {
                        nodata
                    } else if (i + band * 7) % 53 == 0 {
                        f64::NAN
                    } else {
                        value
                    };
                }
            }
        }
        let data = match data_type {
            DataType::F32 => {
                let values: Vec<f32> = source.iter().map(|&v| v as f32).collect();
                source = values.iter().map(|&v| v as f64).collect();
                RasterData::F32(values)
            }
            DataType::F64 => RasterData::F64(source.clone()),
            _ => unreachable!(),
        };
        let raster = overview_fixture(bands, data_type, data);
        let opts = TiffOptions {
            cog: true,
            tile_size: Some(16),
            compression: Compression::None,
            cog_resampling: Some(Method::Average),
            nodata: Some(nodata),
            alpha,
            photometric: (bands == 4).then_some(2),
            ..Default::default()
        };
        let whole = encode(&raster, &opts).unwrap();
        let mut lazy = crate::LazyRaster::from_tiff_bytes(whole.clone()).unwrap().unwrap();
        let dimensions = lazy.overviews();
        assert_eq!(dimensions, vec![(33, 18), (17, 9), (9, 5)]);
        for (level, (out_cols, out_rows)) in dimensions.into_iter().enumerate() {
            let factor = 1usize << (level + 1);
            let overview = lazy
                .read_window(crate::Window::new(0, 0, out_cols, out_rows), &crate::WindowOptions { bands: vec![], overview: Some(level) })
                .unwrap();
            let actual: Vec<u64> = match overview.raster.data {
                RasterData::F32(values) if data_type == DataType::F32 => values.into_iter().map(|v| v.to_bits() as u64).collect(),
                RasterData::F64(values) if data_type == DataType::F64 => values.into_iter().map(f64::to_bits).collect(),
                _ => panic!("expected {data_type:?} overview"),
            };
            for band in 0..bands {
                for y in 0..out_rows {
                    for x in 0..out_cols {
                        // Independent direct-source reference: each block is accumulated in source
                        // row-major order, retaining f64 sums and weights until the final cast.
                        let (mut sum, mut weight) = (0.0, 0.0);
                        for i in overview_source_block(x, y, factor) {
                            let value = source[band * pixels + i];
                            if value.is_nan() || value == nodata {
                                continue;
                            }
                            let w = if alpha && band + 1 < bands {
                                let a = source[(bands - 1) * pixels + i];
                                if !a.is_finite() || a <= 0.0 || a == nodata {
                                    continue;
                                }
                                a
                            } else {
                                1.0
                            };
                            sum += value * w;
                            weight += w;
                        }
                        let expected = if weight == 0.0 { nodata } else { sum / weight };
                        let expected = if data_type == DataType::F32 { (expected as f32).to_bits() as u64 } else { expected.to_bits() };
                        assert_eq!(
                            actual[(band * out_rows + y) * out_cols + x],
                            expected,
                            "{data_type:?}, {bands} bands, factor {factor}, band {band}, ({x}, {y})"
                        );
                    }
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        for step in [1, 3, 7, 8, 16, 31, rows] {
            let path = dir.path().join("average.tif");
            let mut writer = CogWriter::create(path.to_str().unwrap(), &raster, &opts).unwrap();
            for y in (0..rows).step_by(step) {
                let h = step.min(rows - y);
                writer.write_rows(slice_rows(&raster.data, rows, y..y + h, cols, bands), h).unwrap();
            }
            writer.finish().unwrap();
            assert!(std::fs::read(path).unwrap() == whole, "{data_type:?}, {bands} bands, rows by {step}");
        }
    }

    #[test]
    #[ignore = "#165's model, not GDAL's: run against GDAL by python/tests/test_cog_overview_scenarios.py"]
    fn cog_float32_average_pixels_preserve_source_order_across_slabs() {
        for bands in [1, 2, 4] {
            assert_cog_float_average_source_order(DataType::F32, bands);
        }
    }

    #[test]
    #[ignore = "#165's model, not GDAL's: run against GDAL by python/tests/test_cog_overview_scenarios.py"]
    fn cog_float64_average_pixels_preserve_source_order_across_slabs() {
        for bands in [1, 2, 4] {
            assert_cog_float_average_source_order(DataType::F64, bands);
        }
    }

    #[test]
    #[ignore = "#165's model, not GDAL's: run against GDAL by python/tests/test_cog_overview_scenarios.py"]
    fn cog_float_average_overviews_preserve_original_valid_sample_counts() {
        let nodata = -9999.0f32;
        let mut source: Vec<f32> = (0..35).flat_map(|y| (0..65).map(move |x| ((x * 17 + y * 7) % 101) as f32 * 0.25)).collect();
        for y in 0..8 {
            for x in 8..16 {
                source[y * 65 + x] = nodata;
            }
        }
        // One valid zero in the first 2x2 block versus four valid twelves in each
        // other 2x2 block: an average of averages loses their unequal counts.
        for y in 0..4 {
            for x in 0..4 {
                source[y * 65 + x] = if x < 2 && y < 2 { nodata } else { 12.0 };
            }
        }
        source[0] = 0.0;
        let raster = overview_fixture(1, DataType::F32, RasterData::F32(source.clone()));
        for method in [None, Some("average".into())] {
            let options = crate::RasterWriteOptions { overview_resampling: method, ..Default::default() };
            for (factor, data) in stored_cog_overviews(&raster, options, Some(nodata as f64)) {
                let RasterData::F32(actual) = data else { panic!("expected Float32 overview") };
                let cols = 65usize.div_ceil(factor);
                let rows = 35usize.div_ceil(factor);
                for y in 0..rows {
                    for x in 0..cols {
                        let valid: Vec<f32> = overview_source_block(x, y, factor).map(|i| source[i]).filter(|&v| v != nodata).collect();
                        let expected = if valid.is_empty() {
                            nodata
                        } else {
                            (valid.iter().map(|&v| v as f64).sum::<f64>() / valid.len() as f64) as f32
                        };
                        assert_eq!(actual[y * cols + x], expected, "Float32 factor {factor}, ({x}, {y})");
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "#165's model, not GDAL's: run against GDAL by python/tests/test_cog_overview_scenarios.py"]
    fn cog_uint16_average_overviews_round_only_after_full_source_reduction() {
        let nodata = u16::MAX;
        let mut source: Vec<u16> = (0..35).flat_map(|y| (0..65).map(move |x| ((x * 17 + y * 7) % 101) as u16)).collect();
        for y in 0..8 {
            for x in 8..16 {
                source[y * 65 + x] = nodata;
            }
        }
        for y in 0..4 {
            for x in 0..4 {
                source[y * 65 + x] = if x < 2 && y < 2 { nodata } else { 12 };
            }
            for x in 16..20 {
                source[y * 65 + x] = 0;
            }
        }
        source[0] = 0;
        // Three 2x2 blocks round 0.5 to 1, but the original 4x4 mean is 6/16,
        // which rounds to 0 rather than the rounded intermediate mean of 3/4.
        for (x, y) in [(16, 0), (17, 0), (18, 0), (19, 0), (16, 2), (17, 2)] {
            source[y * 65 + x] = 1;
        }
        let raster = overview_fixture(1, DataType::U16, RasterData::U16(source.clone()));
        let options = crate::RasterWriteOptions { overview_resampling: Some("average".into()), ..Default::default() };
        for (factor, data) in stored_cog_overviews(&raster, options, Some(nodata as f64)) {
            let RasterData::U16(actual) = data else { panic!("expected UInt16 overview") };
            let cols = 65usize.div_ceil(factor);
            let rows = 35usize.div_ceil(factor);
            for y in 0..rows {
                for x in 0..cols {
                    let valid: Vec<u16> = overview_source_block(x, y, factor).map(|i| source[i]).filter(|&v| v != nodata).collect();
                    let expected = if valid.is_empty() {
                        nodata
                    } else {
                        (valid.iter().map(|&v| v as f64).sum::<f64>() / valid.len() as f64).round() as u16
                    };
                    assert_eq!(actual[y * cols + x], expected, "UInt16 factor {factor}, ({x}, {y})");
                }
            }
        }
    }

    #[test]
    #[ignore = "#165's model, not GDAL's: run against GDAL by python/tests/test_cog_overview_scenarios.py"]
    fn cog_overviews_honor_min_max_and_mode() {
        let source: Vec<u8> = (0..35).flat_map(|y| (0..65).map(move |x| [1, 5, 5, 9][(y % 2) * 2 + x % 2])).collect();
        let raster = overview_fixture(1, DataType::U8, RasterData::U8(source.clone()));
        for method in ["min", "max", "mode"] {
            let options = crate::RasterWriteOptions { overview_resampling: Some(method.into()), ..Default::default() };
            for (factor, data) in stored_cog_overviews(&raster, options, None) {
                let RasterData::U8(actual) = data else { panic!("expected Byte overview") };
                let cols = 65usize.div_ceil(factor);
                let rows = 35usize.div_ceil(factor);
                for y in 0..rows {
                    for x in 0..cols {
                        let values: Vec<u8> = overview_source_block(x, y, factor).map(|i| source[i]).collect();
                        let expected = match method {
                            "min" => *values.iter().min().unwrap(),
                            "max" => *values.iter().max().unwrap(),
                            "mode" => {
                                let mut counts = [0usize; 256];
                                let mut best = values[0];
                                for value in values {
                                    counts[value as usize] += 1;
                                    if counts[value as usize] > counts[best as usize] {
                                        best = value;
                                    }
                                }
                                best
                            }
                            _ => unreachable!(),
                        };
                        assert_eq!(actual[y * cols + x], expected, "{method} factor {factor}, ({x}, {y})");
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "#165's model, not GDAL's: run against GDAL by python/tests/test_cog_overview_scenarios.py"]
    fn cog_categorical_overviews_preserve_uint64_samples() {
        let base = 1u64 << 53;
        let pattern = [base, base + 1, base + 3, base + 3];
        let source: Vec<u64> = (0..35).flat_map(|y| (0..65).map(move |x| pattern[(y % 2) * 2 + x % 2])).collect();
        let raster = overview_fixture(1, DataType::U64, RasterData::U64(source.clone()));
        let dir = tempfile::tempdir().unwrap();
        for method in ["nearest", "min", "max", "mode"] {
            let options = crate::RasterWriteOptions { overview_resampling: Some(method.into()), ..Default::default() };
            for (factor, data) in stored_cog_overviews(&raster, options.clone(), Some(base as f64)) {
                let RasterData::U64(actual) = data else { panic!() };
                let cols = 65usize.div_ceil(factor);
                let rows = 35usize.div_ceil(factor);
                for y in 0..rows {
                    for x in 0..cols {
                        let values: Vec<u64> = overview_source_block(x, y, factor).map(|i| source[i]).filter(|&v| v != base).collect();
                        let expected = if method == "nearest" {
                            source[(y * factor + factor / 2).min(34) * 65 + (x * factor + factor / 2).min(64)]
                        } else if values.is_empty() {
                            base
                        } else if method == "min" {
                            *values.iter().min().unwrap()
                        } else {
                            // The larger class is both the maximum and the unique mode.
                            *values.iter().max().unwrap()
                        };
                        assert_eq!(actual[y * cols + x], expected, "{method}, factor {factor}, ({x}, {y})");
                    }
                }
            }
            let opts =
                TiffOptions::from_write_options(&crate::RasterWriteOptions { tile_size: Some(16), ..options }, true, Some(base as f64))
                    .unwrap()
                    .unwrap();
            let whole = encode(&raster, &opts).unwrap();
            let path = dir.path().join("uint64.tif");
            let mut writer = CogWriter::create(path.to_str().unwrap(), &raster, &opts).unwrap();
            for y in 0..35 {
                writer.write_rows(slice_rows(&raster.data, 35, y..y + 1, 65, 1), 1).unwrap();
            }
            writer.finish().unwrap();
            assert_eq!(std::fs::read(path).unwrap(), whole, "{method}");
        }
    }

    #[test]
    #[ignore = "#165's model, not GDAL's: run against GDAL by python/tests/test_cog_overview_scenarios.py"]
    fn cog_mode_counts_high_cardinality_uint64_classes_across_slabs() {
        let (cols, rows) = (257, 65);
        let base = 1u64 << 63;
        let (a, b) = (base + 1, base + 3);
        let mut source: Vec<u64> = (0..cols * rows).map(|i| base + 1000 + i as u64).collect();
        // Almost every sample is distinct. B reaches two first, then A reaches three first;
        // both transitions straddle input slabs, and the final 3:3 tie must leave A winning.
        for (x, y, value) in [(0, 0, a), (1, 0, b), (0, 15, b), (0, 16, a), (30, 31, a), (31, 31, b)] {
            source[y * cols + x] = value;
        }
        let raster = Raster::from_data_native(
            wbraster::RasterConfig { cols, rows, bands: 1, data_type: DataType::U64, ..Default::default() },
            RasterData::U64(source.clone()),
        )
        .unwrap();
        let opts = TiffOptions {
            cog: true,
            tile_size: Some(16),
            compression: Compression::None,
            cog_resampling: Some(Method::Mode),
            ..Default::default()
        };
        let dir = tempfile::tempdir().unwrap();
        for step in [1, 16, 17, rows] {
            let path = dir.path().join("mode.tif");
            let mut writer = CogWriter::create(path.to_str().unwrap(), &raster, &opts).unwrap();
            for y in (0..rows).step_by(step) {
                let h = step.min(rows - y);
                writer.write_rows(slice_rows(&raster.data, rows, y..y + h, cols, 1), h).unwrap();
            }
            writer.finish().unwrap();
            let mut lazy = crate::LazyRaster::from_tiff_bytes(std::fs::read(path).unwrap()).unwrap().unwrap();
            let dimensions = lazy.overviews();
            assert_eq!(dimensions.last(), Some(&(9, 3)));
            for (level, (out_cols, out_rows)) in dimensions.into_iter().enumerate() {
                let factor = 1usize << (level + 1);
                let overview = lazy
                    .read_window(
                        crate::Window::new(0, 0, out_cols, out_rows),
                        &crate::WindowOptions { bands: vec![], overview: Some(level) },
                    )
                    .unwrap();
                let RasterData::U64(actual) = overview.raster.data else { panic!("expected UInt64 overview") };
                for y in 0..out_rows {
                    for x in 0..out_cols {
                        let mut counts = std::collections::BTreeMap::new();
                        let (mut expected, mut highest) = (0, 0usize);
                        for sy in y * factor..((y + 1) * factor).min(rows) {
                            for sx in x * factor..((x + 1) * factor).min(cols) {
                                let value = source[sy * cols + sx];
                                let count = counts.entry(value).or_insert(0usize);
                                *count += 1;
                                if *count > highest {
                                    expected = value;
                                    highest = *count;
                                }
                            }
                        }
                        assert_eq!(actual[y * out_cols + x], expected, "step {step}, factor {factor}, ({x}, {y})");
                    }
                }
                if factor == 32 {
                    assert_eq!(actual[0], a, "the first class to reach three wins the tie");
                }
            }
        }
    }

    #[test]
    #[ignore = "#165's model, not GDAL's: run against GDAL by python/tests/test_cog_overview_scenarios.py"]
    fn cog_float_mode_groups_signed_zeros_and_excludes_nans() {
        let nodata = -9999.0;
        let patterns = [
            [1.0, -0.0, 0.0, 1.0],
            [f64::NAN, f64::NAN, 7.0, f64::NAN],
            [f64::NAN; 4],
            [2.0, 2.0, -0.0, 0.0],
            [3.0, -0.0, 0.0, f64::NAN],
            [3.0, 0.0, -0.0, f64::NAN],
        ];
        let mut source = vec![f64::NAN; 65 * 35];
        for y in 0..8 {
            for x in 0..48 {
                source[y * 65 + x] = patterns[x / 8][(y % 2) * 2 + x % 2];
            }
        }
        for data in [RasterData::F32(source.iter().map(|&v| v as f32).collect()), RasterData::F64(source)] {
            let data_type = data.data_type();
            let raster = overview_fixture(1, data_type, data);
            let options = crate::RasterWriteOptions { overview_resampling: Some("mode".into()), ..Default::default() };
            for (factor, data) in stored_cog_overviews(&raster, options, Some(nodata)) {
                let actual: Vec<f64> = match data {
                    RasterData::F32(v) => v.into_iter().map(f64::from).collect(),
                    RasterData::F64(v) => v,
                    _ => panic!("expected floating-point overview"),
                };
                let out_cols = 65usize.div_ceil(factor);
                for y in 0..35usize.div_ceil(factor) {
                    for x in 0..out_cols {
                        let expected: f64 =
                            if y * factor < 8 && x * factor < 48 { [0.0, 7.0, nodata, 2.0, 0.0, -0.0][x * factor / 8] } else { nodata };
                        assert_eq!(actual[y * out_cols + x].to_bits(), expected.to_bits(), "{data_type:?}, factor {factor}, ({x}, {y})");
                    }
                }
            }
        }
    }

    #[test]
    fn cog_rejects_unsupported_overview_resampling() {
        let options = crate::RasterWriteOptions { overview_resampling: Some("not-a-resampler".into()), ..Default::default() };
        assert!(TiffOptions::from_write_options(&options, true, None).is_err());
    }

    /// A thin, ragged Float32 band of 301 x 213 (odd sizes at every level), valid along a
    /// meander about 9 pixels wide, nodata -9999 elsewhere; with `bands` bands, each shifted.
    fn meander(bands: usize) -> Raster {
        let (cols, rows) = (301usize, 213usize);
        let data: Vec<f32> = (0..bands)
            .flat_map(|b| {
                (0..rows).flat_map(move |y| {
                    (0..cols).map(move |x| {
                        let centre = 100.0 + 60.0 * ((x as f64) / 37.0).sin() + 7.0 * ((x as f64) / 5.0).sin() + 3.0 * b as f64;
                        let d = (y as f64 - centre).abs();
                        if d < 4.5 { (4.5 - d) as f32 + b as f32 } else { -9999.0 }
                    })
                })
            })
            .collect();
        let cfg = wbraster::RasterConfig { cols, rows, bands, nodata: -9999.0, data_type: DataType::F32, ..Default::default() };
        Raster::from_data_native(cfg, RasterData::F32(data)).unwrap()
    }

    /// An overview read back: its size and samples.
    type StoredLevel = ((usize, usize), Vec<f64>);

    /// A GeoTIFF or COG read back: the full-resolution dataset, and the overviews' sizes and
    /// samples.
    fn stored_levels(bytes: Vec<u8>) -> (crate::RasterDataset, Vec<StoredLevel>) {
        let mut lazy = crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap();
        let levels = lazy
            .overviews()
            .into_iter()
            .enumerate()
            .map(|(i, (w, h))| {
                let read = lazy.read_window(crate::Window::full(w, h), &crate::WindowOptions { bands: vec![], overview: Some(i) }).unwrap();
                ((w, h), read.raster.data.to_f64_vec())
            })
            .collect();
        (lazy.read().unwrap(), levels)
    }

    fn band_item(ds: &crate::RasterDataset, band: usize, name: &str) -> Option<String> {
        ds.band_metadata.get(band)?.items.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
    }

    /// Area-matched COG overviews have GDAL's sizes (those of `average`) and keep the valid area
    /// at every level, where `average` grows it; the base image is the source's, the method is
    /// recorded as GDAL's COG driver records one (`IMAGE_STRUCTURE`), and each band's metadata
    /// holds the area each level keeps, recomputed here from the levels read back.
    #[test]
    fn cog_area_matched_overviews_keep_the_valid_area() {
        let raster = meander(2);
        let (cols, rows, nd) = (raster.cols, raster.rows, -9999.0);
        let cog = |method: &str| {
            let options = crate::RasterWriteOptions {
                tile_size: Some(32),
                compression: Some("DEFLATE".into()),
                creation_options: vec![("OVERVIEW_RESAMPLING".into(), method.into())],
                ..Default::default()
            };
            encode(&raster, &TiffOptions::from_write_options(&options, true, Some(nd)).unwrap().unwrap()).unwrap()
        };
        let am_bytes = cog("AREA_MATCHED");
        assert!(
            String::from_utf8_lossy(&am_bytes).contains(r#"<Item name="OVERVIEW_RESAMPLING" domain="IMAGE_STRUCTURE">AREA_MATCHED</Item>"#)
        );
        let (am, am_levels) = stored_levels(am_bytes);
        let (_, avg_levels) = stored_levels(cog("AVERAGE"));
        let sizes: Vec<(usize, usize)> = am_levels.iter().map(|l| l.0).collect();
        assert_eq!(sizes, gdal_overview::cog_sizes(cols, rows, 32, None));
        assert_eq!(sizes, avg_levels.iter().map(|l| l.0).collect::<Vec<_>>());
        assert_eq!(sizes, [(150, 106), (75, 53), (37, 26), (18, 13)]);
        let base = am.raster.data.to_f64_vec();
        assert_eq!(base, raster.data.to_f64_vec());
        for b in 0..2 {
            let band = &base[b * cols * rows..(b + 1) * cols * rows];
            let valid = band.iter().filter(|v| **v != nd).count();
            let mut ratios = Vec::new();
            for (((w, h), am), (_, avg)) in am_levels.iter().zip(&avg_levels) {
                let n = w * h;
                let kept = am[b * n..(b + 1) * n].iter().filter(|v| **v != nd).count();
                let cell = (cols * rows) as f64 / n as f64;
                let target = (valid as f64 * n as f64 / (cols * rows) as f64).round_ties_even() as usize;
                assert_eq!(kept, target, "band {b}, {w}x{h}");
                let grown = avg[b * n..(b + 1) * n].iter().filter(|v| **v != nd).count();
                assert!(grown as f64 * cell > 1.2 * valid as f64, "average grows the area: {grown} cells of {cell} px over {valid} px");
                ratios.push(kept as f64 * cell / valid as f64);
            }
            let stored = band_item(&am, b, area_matched::AREA_RATIOS_ITEM).unwrap();
            let stored: Vec<f64> = stored.split(',').map(|r| r.parse().unwrap()).collect();
            assert_eq!(stored.len(), ratios.len());
            for (s, r) in stored.iter().zip(&ratios) {
                assert!((s - r).abs() < 1e-12, "{stored:?} {ratios:?}");
            }
            assert_eq!(band_item(&am, b, area_matched::BASE_VALID_PIXELS_ITEM), Some(valid.to_string()));
        }
        // Means of the valid pixels: within the range of the source values.
        assert!(am_levels.iter().all(|(_, v)| v.iter().all(|v| *v == nd || (0.0..=6.0).contains(v))));
    }

    /// `area_matched_mode` keeps class values: every kept cell holds a class of the source.
    #[test]
    fn cog_area_matched_mode_keeps_classes() {
        let (cols, rows) = (130usize, 70usize);
        let data: Vec<u8> =
            (0..rows).flat_map(|y| (0..cols).map(move |x| if (x + 2 * y) % 23 < 6 { [3, 7, 11][(x / 9) % 3] } else { 0 })).collect();
        let cfg = wbraster::RasterConfig { cols, rows, bands: 1, nodata: 0.0, data_type: DataType::U8, ..Default::default() };
        let raster = Raster::from_data_native(cfg, RasterData::U8(data)).unwrap();
        let options =
            crate::RasterWriteOptions { tile_size: Some(16), overview_resampling: Some("area_matched_mode".into()), ..Default::default() };
        let bytes = encode(&raster, &TiffOptions::from_write_options(&options, true, Some(0.0)).unwrap().unwrap()).unwrap();
        assert!(String::from_utf8_lossy(&bytes).contains(">AREA_MATCHED_MODE</Item>"));
        let (lazy, levels) = stored_levels(bytes);
        assert_eq!(levels.len(), 3);
        for ((w, h), v) in &levels {
            assert!(v.iter().all(|v| [0.0, 3.0, 7.0, 11.0].contains(v)), "{w}x{h}");
        }
        assert!(band_item(&lazy, 0, area_matched::AREA_RATIOS_ITEM).is_some());
    }

    /// Native categorical values above f64 precision survive writing and reading an overview.
    #[test]
    fn geotiff_area_matched_mode_preserves_u64_classes() {
        let class = (1_u64 << 53) + 1;
        let cfg = wbraster::RasterConfig { cols: 2, rows: 2, bands: 1, nodata: 0.0, data_type: DataType::U64, ..Default::default() };
        let raster = Raster::from_data_native(cfg, RasterData::U64(vec![class; 4])).unwrap();
        let options =
            crate::RasterWriteOptions { overviews: vec![2], overview_resampling: Some("area_matched_mode".into()), ..Default::default() };
        let opts = TiffOptions::from_write_options(&options, false, Some(0.0)).unwrap().unwrap();
        let mut lazy = crate::LazyRaster::from_tiff_bytes(encode(&raster, &opts).unwrap()).unwrap().unwrap();
        let read = lazy.read_window(crate::Window::full(1, 1), &crate::WindowOptions { bands: vec![], overview: Some(0) }).unwrap();
        let RasterData::U64(values) = read.raster.data else { panic!("the overview must retain its native type") };
        assert_eq!(values, vec![class]);
    }

    /// Area-matched Int64/UInt64 levels declare nodata, unlike GDAL 3.12.1's ordinary integer
    /// overviews: their invalid footprints must remain invalid and agree with the area report.
    #[test]
    fn area_matched_integer_overviews_declare_invalid_footprints() {
        for data_type in [DataType::I64, DataType::U64] {
            let values: Vec<i32> = (0..64 * 32).map(|i| if i % 64 < 32 { 13 } else { 0 }).collect();
            let data = match data_type {
                DataType::I64 => RasterData::I64(values.iter().map(|&v| i64::from(v)).collect()),
                DataType::U64 => RasterData::U64(values.iter().map(|&v| v as u64).collect()),
                _ => unreachable!(),
            };
            let cfg = wbraster::RasterConfig { cols: 64, rows: 32, bands: 1, nodata: 0.0, data_type, ..Default::default() };
            let raster = Raster::from_data_native(cfg, data).unwrap();
            for cog in [false, true] {
                for method in ["area_matched", "area_matched_mode"] {
                    let options = crate::RasterWriteOptions {
                        tile_size: Some(16),
                        overviews: if cog { vec![] } else { vec![2, 4] },
                        overview_resampling: Some(method.into()),
                        ..Default::default()
                    };
                    let opts = TiffOptions::from_write_options(&options, cog, Some(0.0)).unwrap().unwrap();
                    let mut lazy = crate::LazyRaster::from_tiff_bytes(encode(&raster, &opts).unwrap()).unwrap().unwrap();
                    let header = lazy.header();
                    assert_eq!(band_item(&header, 0, area_matched::BASE_VALID_PIXELS_ITEM), Some("1024".into()));
                    let ratios = band_item(&header, 0, area_matched::AREA_RATIOS_ITEM).unwrap();
                    let ratios: Vec<f64> = ratios.split(',').map(|v| v.parse().unwrap()).collect();
                    let sizes = lazy.overviews();
                    assert_eq!(sizes.len(), 2);
                    for (i, (w, h)) in sizes.into_iter().enumerate() {
                        assert_eq!(lazy.level_band_nodata(&[1], Some(i)), vec![Some(0.0)]);
                        let read = lazy
                            .read_window(crate::Window::full(w, h), &crate::WindowOptions { bands: vec![], overview: Some(i) })
                            .unwrap();
                        assert_eq!(read.nodata(), Some(0.0));
                        let valid = match &read.raster.data {
                            RasterData::I64(v) => v.iter().filter(|&&v| v != 0).count(),
                            RasterData::U64(v) => v.iter().filter(|&&v| v != 0).count(),
                            _ => unreachable!(),
                        };
                        assert_eq!(valid, w * h / 2);
                        assert_eq!(ratios[i], valid as f64 * (64 * 32) as f64 / (w * h * 1024) as f64);
                    }
                }
            }
        }
    }

    /// GeoTIFF overviews at factors take the method too, with `ceil(size / factor)` sizes.
    #[test]
    fn geotiff_area_matched_overviews_at_factors() {
        let raster = meander(1);
        let options =
            crate::RasterWriteOptions { overviews: vec![4, 2, 8], overview_resampling: Some("area_matched".into()), ..Default::default() };
        let opts = TiffOptions::from_write_options(&options, false, Some(-9999.0)).unwrap().unwrap();
        let (lazy, levels) = stored_levels(encode(&raster, &opts).unwrap());
        assert_eq!(levels.iter().map(|l| l.0).collect::<Vec<_>>(), [(151, 107), (76, 54), (38, 27)]);
        let valid = raster.data.to_f64_vec().iter().filter(|v| **v != -9999.0).count();
        let ratios = band_item(&lazy, 0, area_matched::AREA_RATIOS_ITEM).unwrap();
        for (((w, h), v), r) in levels.iter().zip(ratios.split(',')) {
            let kept = v.iter().filter(|v| **v != -9999.0).count();
            let target = (valid as f64 * (w * h) as f64 / (301 * 213) as f64).round_ties_even() as usize;
            assert_eq!(kept, target);
            let expected = kept as f64 * (301 * 213) as f64 / (w * h) as f64 / valid as f64;
            assert!((r.parse::<f64>().unwrap() - expected).abs() < 1e-12);
        }
    }

    /// A report the source carried is replaced, the other band items kept; no levels (the raster
    /// fits in a tile, `OVERVIEWS=NONE`) means no report; and what the method cannot honour is
    /// refused rather than written otherwise.
    #[test]
    fn area_matched_dataset_writes_require_shared_band_nodata() {
        let dir = tempfile::tempdir().unwrap();
        for driver in [crate::formats::RasterDriver::GTiff, crate::formats::RasterDriver::Cog] {
            for method in ["area_matched", "area_matched_mode"] {
                let path = dir.path().join(format!("{driver:?}-{method}.tif"));
                let options = crate::RasterWriteOptions {
                    driver: Some(driver),
                    tile_size: Some(32),
                    overviews: vec![2],
                    overview_resampling: Some(method.into()),
                    ..Default::default()
                };
                for declarations in [vec![Some(-9999.0), Some(-9998.0)], vec![Some(-9999.0), None], vec![None, Some(-9998.0)]] {
                    let mut dataset = crate::RasterDataset::new(meander(2), true);
                    dataset.band_nodata = declarations;
                    let error = crate::write_raster_dataset(&dataset, path.to_str().unwrap(), &options).unwrap_err();
                    assert!(error.to_string().contains("one shared nodata value"), "{error}");
                }
                for nodata in [-9999.0, f64::NAN] {
                    let mut dataset = crate::RasterDataset::new(meander(2), true);
                    dataset.band_nodata = vec![Some(nodata); 2];
                    crate::write_raster_dataset(&dataset, path.to_str().unwrap(), &options).unwrap();
                }
            }
        }
    }

    #[test]
    fn cog_area_matched_report_and_refusals() {
        let raster = meander(1);
        let stale = crate::BandMetadata {
            items: vec![("KEEP".into(), "me".into()), (area_matched::AREA_RATIOS_ITEM.into(), "9".into())],
            ..Default::default()
        };
        let options = |extra: Vec<(&str, &str)>| crate::RasterWriteOptions {
            tile_size: Some(32),
            band_metadata: vec![stale.clone()],
            creation_options: [("OVERVIEW_RESAMPLING", "AREA_MATCHED")]
                .into_iter()
                .chain(extra)
                .map(|(k, v)| (k.into(), v.into()))
                .collect(),
            ..Default::default()
        };
        let write = |o: &crate::RasterWriteOptions, nodata: Option<f64>| {
            TiffOptions::from_write_options(o, true, nodata).and_then(|t| encode(&raster, &t.unwrap()))
        };
        let (lazy, _) = stored_levels(write(&options(vec![]), Some(-9999.0)).unwrap());
        let items = &lazy.band_metadata[0].items;
        assert_eq!(items.iter().filter(|(k, _)| k == area_matched::AREA_RATIOS_ITEM).count(), 1);
        assert_ne!(band_item(&lazy, 0, area_matched::AREA_RATIOS_ITEM).as_deref(), Some("9"));
        assert_eq!(band_item(&lazy, 0, "KEEP").as_deref(), Some("me"));
        let (filtered, _) = stored_levels(write(&options(vec![("COPY_SRC_MDD", "YES")]), Some(-9999.0)).unwrap());
        assert!(band_item(&filtered, 0, "KEEP").is_none());
        assert_eq!(band_item(&filtered, 0, area_matched::AREA_RATIOS_ITEM), band_item(&lazy, 0, area_matched::AREA_RATIOS_ITEM));
        assert_eq!(
            band_item(&filtered, 0, area_matched::BASE_VALID_PIXELS_ITEM),
            band_item(&lazy, 0, area_matched::BASE_VALID_PIXELS_ITEM)
        );
        for extra in [vec![("OVERVIEWS", "NONE")], vec![("BLOCKSIZE", "512")]] {
            let without_levels = options(extra);
            let (lazy, levels) = stored_levels(write(&without_levels, Some(-9999.0)).unwrap());
            assert!(levels.is_empty());
            assert_eq!(band_item(&lazy, 0, area_matched::AREA_RATIOS_ITEM).as_deref(), Some("9"), "left as given");
            assert!(write(&without_levels, None).unwrap_err().to_string().contains("nodata"));
            let masked = crate::RasterWriteOptions { mask: Some(vec![1; 301 * 213]), ..without_levels.clone() };
            assert!(write(&masked, Some(-9999.0)).unwrap_err().to_string().contains("without a mask or alpha band"));
            let alpha = crate::RasterWriteOptions {
                band_metadata: vec![crate::BandMetadata { color_interp: Some("Alpha".into()), ..Default::default() }],
                ..without_levels
            };
            assert!(write(&alpha, Some(-9999.0)).unwrap_err().to_string().contains("without a mask or alpha band"));
        }
        // No nodata; a mask; an alpha band; a source's overviews to copy.
        assert!(write(&options(vec![]), None).is_err());
        let masked = crate::RasterWriteOptions { mask: Some(vec![1; 301 * 213]), ..options(vec![]) };
        assert!(write(&masked, Some(-9999.0)).is_err());
        let alpha = crate::RasterWriteOptions {
            band_metadata: vec![crate::BandMetadata { color_interp: Some("Alpha".into()), ..Default::default() }],
            ..options(vec![])
        };
        assert!(write(&alpha, Some(-9999.0)).is_err());
        for (bands, index) in [(2, 0), (3, 1)] {
            let mut alpha = options(vec![]);
            alpha.band_metadata = vec![crate::BandMetadata::default(); bands];
            alpha.band_metadata[index].color_interp = Some("Alpha".into());
            let opts = TiffOptions::from_write_options(&alpha, true, Some(-9999.0)).unwrap().unwrap();
            let error = encode(&meander(bands), &opts).unwrap_err();
            assert!(error.to_string().contains("without a mask or alpha band"), "{error}");
        }
        assert!(write(&options(vec![("@COPY_SRC_OVERVIEWS", "src.tif")]), Some(-9999.0)).is_err());
        // The row-streaming COG writer needs the whole raster for them.
        let dir = tempfile::tempdir().unwrap();
        let opts = TiffOptions::from_write_options(&options(vec![]), true, Some(-9999.0)).unwrap().unwrap();
        assert!(CogWriter::create(dir.path().join("rows.tif").to_str().unwrap(), &raster, &opts).is_err());
    }

    #[test]
    fn predictors_match_libtiff() {
        // Horizontal differencing of 16-bit samples, two per pixel.
        let mut b: Vec<u8> = [10u16, 20, 13, 25, 7, 30].iter().flat_map(|v| v.to_le_bytes()).collect();
        apply_predictor(&mut b, 2, 6, 2, 2);
        let v: Vec<u16> = b.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        assert_eq!(v, vec![10, 20, 3, 5, 65530, 5]);
        // Floating point: byte planes, most significant first, then byte differences.
        let mut f: Vec<u8> = [1.0f32, 2.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        apply_predictor(&mut f, 3, 2, 1, 4);
        assert_eq!(f, vec![0x3f, 0x01, 0x40, 0x80, 0x00, 0x00, 0x00, 0x00]);
    }

    /// `SPARSE_OK` leaves out of a GeoTIFF, written whole or in rows, tiled or striped, by pixel
    /// or by band, the blocks of its full-resolution image whose samples are all nodata (or 0
    /// without nodata), bit for bit, and only those: their offset and byte count are 0, and the
    /// file reads back, eagerly and lazily, as the same raster written without the option does,
    /// which stores every block. Overviews are written whole, and so is every block when the
    /// samples cannot hold nodata. A COG writes every tile, and warns.
    #[test]
    fn sparse_ok_leaves_out_the_blocks_of_nodata() {
        let (cols, rows) = (100, 70);
        let dir = tempfile::tempdir().unwrap();
        let (path, dense) = (dir.path().join("sparse.tif"), dir.path().join("dense.tif"));
        let bits = |d: &RasterData| d.to_f64_vec().iter().map(|v| v.to_bits()).collect::<Vec<_>>();
        let eager = |p: &std::path::Path| bits(&crate::geotiff::read_geotiff(p).unwrap().raster.data);
        let lazy = |p: &std::path::Path| bits(&crate::LazyRaster::open(p.to_str().unwrap()).unwrap().read().unwrap().raster.data);
        // The type and nodata, and whether blocks of nodata are left out: not when the samples
        // cannot hold nodata (a fraction, a value out of range).
        for (t, nodata, sparse) in [
            (DataType::U16, Some(7.0), true),
            (DataType::F32, None, true),
            (DataType::F32, Some(f64::NAN), true),
            (DataType::U16, Some(7.5), false),
            (DataType::U16, Some(70000.0), false),
        ] {
            // Nodata (or 0) but for pixels of the first band, each alone in its block: in a
            // block's last row and column, its first row and column, the image's last row and
            // column (an edge block), and a strip's first column; and, without nodata, one of the
            // second band that is -0.0. Their blocks are kept.
            let fill = nodata.unwrap_or(0.0);
            let mut values = vec![fill; 2 * cols * rows];
            let pixels = [(0, 31, 47), (0, 32, 48), (0, 69, 99), (0, 40, 0), (1, 50, 60)];
            let pixels = &pixels[..if nodata.is_none() { 5 } else { 4 }];
            for &(b, y, x) in pixels {
                values[b * cols * rows + y * cols + x] = if b == 0 { 1.0 } else { -0.0 };
            }
            // Integer samples are rounded in the first 32 rows and truncated below, so that whole
            // blocks of every layout hold a fractional nodata as either conversion gives it: 7.5
            // as 8 above and as 7 below. None of them is left out.
            let data = match t {
                DataType::U16 => RasterData::U16(
                    values
                        .iter()
                        .enumerate()
                        .map(|(i, v)| (if i % (cols * rows) < 32 * cols { v.round() } else { v.trunc() }) as u16)
                        .collect(),
                ),
                _ => RasterData::F32(values.iter().map(|v| *v as f32).collect()),
            };
            let cfg = wbraster::RasterConfig { cols, rows, bands: 2, data_type: t, nodata: fill, ..Default::default() };
            let raster = Raster::from_data_native(cfg, data).unwrap();
            let header = crate::RasterDataset::new(raster.clone(), nodata.is_some());
            let tiles = [("TILED", "YES"), ("BLOCKXSIZE", "16"), ("BLOCKYSIZE", "16")];
            let planes = [("TILED", "YES"), ("BLOCKXSIZE", "16"), ("BLOCKYSIZE", "16"), ("INTERLEAVE", "BAND")];
            for (layout, (bh, bw), planar) in
                [(&tiles[..], (16, 16), false), (&planes[..], (16, 16), true), (&[("BLOCKYSIZE", "8")][..], (8, cols), false)]
            {
                let options = |sparse_ok: &str, overviews: Vec<usize>| {
                    let mut co: Vec<(String, String)> = layout.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                    co.push(("SPARSE_OK".into(), sparse_ok.into()));
                    crate::RasterWriteOptions { creation_options: co, overviews, ..Default::default() }
                };
                let opts = TiffOptions::from_write_options(&options("NO", Vec::new()), false, nodata).unwrap().unwrap();
                write(&raster, dense.to_str().unwrap(), &opts).unwrap();
                let expected = lazy(&dense);
                let (across, per_plane) = (cols.div_ceil(bw), rows.div_ceil(bh) * cols.div_ceil(bw));
                let every: Vec<usize> = (0..if planar { 2 * per_plane } else { per_plane }).collect();
                let kept: Vec<usize> = if sparse {
                    let mut kept: Vec<usize> =
                        pixels.iter().map(|(b, y, x)| if planar { b * per_plane } else { 0 } + y / bh * across + x / bw).collect();
                    kept.sort_unstable();
                    kept.dedup();
                    kept
                } else {
                    every.clone()
                };
                for (whole, sparse_ok) in [(true, "NO"), (false, "NO"), (true, "YES"), (false, "YES")] {
                    let options = options(sparse_ok, if whole { vec![2] } else { Vec::new() });
                    if whole {
                        let opts = TiffOptions::from_write_options(&options, false, nodata).unwrap().unwrap();
                        write(&raster, path.to_str().unwrap(), &opts).unwrap();
                    } else {
                        let mut w = GeoTiffWriter::create(path.to_str().unwrap(), &header, &options).unwrap();
                        w.write_rows(&raster.data, rows, None).unwrap();
                        w.finish().unwrap();
                    }
                    let case = format!("{t:?}, nodata {nodata:?}, {layout:?}, whole {whole}, SPARSE_OK {sparse_ok}");
                    let mut dec = tiff::decoder::Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
                    let tables = crate::geotiff::ChunkTables::read(&mut dec).unwrap();
                    let written: Vec<usize> = (0..tables.counts.len()).filter(|&i| tables.counts[i] > 0).collect();
                    assert_eq!(&written, if sparse_ok == "YES" { &kept } else { &every }, "{case}");
                    assert!(tables.offsets.iter().zip(&tables.counts).all(|(o, n)| (*o == 0) == (*n == 0)), "{case}");
                    if whole {
                        dec.next_image().unwrap();
                        assert!(crate::geotiff::ChunkTables::read(&mut dec).unwrap().counts.iter().all(|n| *n > 0), "{case}");
                    }
                    assert_eq!(eager(&path), expected, "{case}");
                    assert_eq!(lazy(&path), expected, "{case}");
                }
            }
        }
        // Blocks left out read as the file's nodata, held exactly for 64-bit integers (2^53 + 1,
        // which a double does not hold, written over 2^53), or as a band's own, from the file's
        // `.aux.xml`, whole as by window.
        let nodata = 9_007_199_254_740_992u64;
        let cfg =
            wbraster::RasterConfig { cols: 32, rows: 32, bands: 2, data_type: DataType::U64, nodata: nodata as f64, ..Default::default() };
        let mut values = vec![nodata; 2 * 32 * 32];
        values[0] = 1;
        let raster = Raster::from_data_native(cfg, RasterData::U64(values)).unwrap();
        let co = [("SPARSE_OK", "YES"), ("TILED", "YES"), ("BLOCKXSIZE", "16"), ("BLOCKYSIZE", "16")];
        let options = crate::RasterWriteOptions {
            creation_options: co.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        };
        let opts = TiffOptions::from_write_options(&options, false, Some(nodata as f64)).unwrap().unwrap();
        let mut bytes = encode(&raster, &opts).unwrap();
        let at: Vec<usize> = (0..bytes.len() - 16).filter(|&i| bytes[i..].starts_with(b"9007199254740992\0")).collect();
        assert_eq!(at.len(), 1);
        bytes[at[0] + 15] = b'3';
        std::fs::write(&path, bytes).unwrap();
        let pam = "<PAMDataset><PAMRasterBand band=\"2\"><NoDataValue>9</NoDataValue></PAMRasterBand></PAMDataset>";
        std::fs::write(dir.path().join("sparse.tif.aux.xml"), pam).unwrap();
        let u64s = |d: RasterData| match d {
            RasterData::U64(v) => v,
            d => panic!("{d:?}"),
        };
        let read = u64s(crate::geotiff::read_geotiff(&path).unwrap().raster.data);
        assert_eq!((read[17 * 32], read[32 * 32 + 17 * 32]), (nodata + 1, 9));
        assert_eq!(read, u64s(crate::LazyRaster::open(path.to_str().unwrap()).unwrap().read().unwrap().raster.data));
        // A 64-bit nodata reaches terrio's writer as a double, which its tag and the blocks left
        // out both take: 2^53 + 1 asked for is 2^53 in both, so a block holding 2^53 + 1 is kept,
        // and those of 2^53 left out read back as 2^53.
        let mut values = vec![nodata; 32 * 32];
        values[0] = nodata + 1;
        let cfg = wbraster::RasterConfig { cols: 32, rows: 32, bands: 1, data_type: DataType::U64, ..Default::default() };
        let raster = Raster::from_data_native(cfg, RasterData::U64(values.clone())).unwrap();
        let opts = TiffOptions::from_write_options(&options, false, Some(9_007_199_254_740_993u64 as f64)).unwrap().unwrap();
        let bytes = encode(&raster, &opts).unwrap();
        assert!(bytes.windows(17).any(|w| w == b"9007199254740992\0"));
        let tables = crate::geotiff::ChunkTables::read(&mut tiff::decoder::Decoder::new(std::io::Cursor::new(&bytes)).unwrap()).unwrap();
        assert_eq!(tables.counts.iter().map(|n| *n > 0).collect::<Vec<_>>(), [true, false, false, false]);
        let path = dir.path().join("u64.tif");
        std::fs::write(&path, bytes).unwrap();
        assert_eq!(u64s(crate::geotiff::read_geotiff(&path).unwrap().raster.data), values);
        assert_eq!(u64s(crate::LazyRaster::open(path.to_str().unwrap()).unwrap().read().unwrap().raster.data), values);
        let _ = crate::warnings::take();
        let options = crate::RasterWriteOptions { creation_options: vec![("SPARSE_OK".into(), "YES".into())], ..Default::default() };
        assert!(!TiffOptions::from_write_options(&options, true, None).unwrap().unwrap().sparse);
        assert_eq!(crate::warnings::take(), ["SPARSE_OK is not supported for COGs: every tile is written"]);
    }

    /// Catches blocks left out that the file's `.aux.xml` would read as another nodata, where the
    /// writer keeps that sidecar (`write_raster`, and `GeoTiffWriter` unlike
    /// `write_raster_dataset`): a band's nodata there (band 2's alone), compared bit for bit
    /// (-0 against the file's 0), or the sidecar's dataset nodata for a file without one. Every
    /// block is then written, with a warning; under a sidecar that agrees, blocks are left out.
    #[test]
    fn sparse_ok_writes_every_block_that_a_sidecar_would_read_otherwise() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.tif");
        let co = [("SPARSE_OK", "YES"), ("TILED", "YES"), ("BLOCKXSIZE", "16"), ("BLOCKYSIZE", "16")];
        let options = crate::RasterWriteOptions {
            creation_options: co.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        };
        let band = |b: usize, v: &str| format!("<PAMRasterBand band=\"{b}\"><NoDataValue>{v}</NoDataValue></PAMRasterBand>");
        // The samples, the file's nodata, the sidecar, whether written in rows, the blocks stored.
        for (data, nodata, sidecar, rows, stored) in [
            (RasterData::U16(vec![7; 2 * 32 * 32]), Some(7.0), band(2, "9"), false, 4),
            (RasterData::U16(vec![7; 2 * 32 * 32]), Some(7.0), band(1, "7") + &band(2, "7"), false, 0),
            (RasterData::F32(vec![0.0; 2 * 32 * 32]), Some(0.0), band(1, "-0"), false, 4),
            (RasterData::U16(vec![0; 2 * 32 * 32]), None, "<NoDataValue>9</NoDataValue>".to_string(), true, 4),
        ] {
            let data_type = if matches!(data, RasterData::F32(_)) { DataType::F32 } else { DataType::U16 };
            let cfg =
                wbraster::RasterConfig { cols: 32, rows: 32, bands: 2, data_type, nodata: nodata.unwrap_or(0.0), ..Default::default() };
            let raster = Raster::from_data_native(cfg, data).unwrap();
            std::fs::write(dir.path().join("out.tif.aux.xml"), format!("<PAMDataset>{sidecar}</PAMDataset>")).unwrap();
            let _ = crate::warnings::take();
            if rows {
                let header = crate::RasterDataset::new(raster.clone(), nodata.is_some());
                let mut w = GeoTiffWriter::create(path.to_str().unwrap(), &header, &options).unwrap();
                w.write_rows(&raster.data, 32, None).unwrap();
                w.finish().unwrap();
            } else {
                crate::write_raster(&raster, path.to_str().unwrap(), &options).unwrap();
            }
            let warned = crate::warnings::take();
            let mut dec = tiff::decoder::Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
            let tables = crate::geotiff::ChunkTables::read(&mut dec).unwrap();
            assert_eq!(tables.counts.iter().filter(|n| **n > 0).count(), stored, "{sidecar}");
            assert_eq!(warned.len(), usize::from(stored > 0), "{sidecar}: {warned:?}");
        }
    }

    /// Catches a block left out that reads otherwise than stored: a NaN nodata is written `nan`,
    /// which readers read as `f64::NAN`, so a block of a NaN with another payload is kept, and one
    /// of `f64::NAN` is left out, whole, in rows, and by `write_raster_dataset`, whose `.aux.xml`
    /// gives the band that nodata, also written `nan` (no warning); all read back bit for bit.
    #[test]
    fn sparse_ok_compares_blocks_with_the_nodata_as_read() {
        let dir = tempfile::tempdir().unwrap();
        let co = [("SPARSE_OK", "YES"), ("TILED", "YES"), ("BLOCKXSIZE", "16"), ("BLOCKYSIZE", "16")];
        let options = crate::RasterWriteOptions {
            creation_options: co.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        };
        let bits = |d: &RasterData| d.to_f64_vec().iter().map(|v| v.to_bits()).collect::<Vec<_>>();
        // Tile 0 holds the nodata, a NaN with a payload; tile 1 `f64::NAN`; tiles 2 and 3 data.
        for (t, nodata) in [(DataType::F64, 0x7ff8_0000_0000_0001u64), (DataType::F32, 0x7ff8_0000_2000_0000)] {
            let nodata = f64::from_bits(nodata);
            let mut values = vec![1.0; 32 * 32];
            for y in 0..16 {
                for x in 0..16 {
                    (values[y * 32 + x], values[y * 32 + 16 + x]) = (nodata, f64::NAN);
                }
            }
            let data = match t {
                DataType::F64 => RasterData::F64(values),
                _ => RasterData::F32(values.iter().map(|v| *v as f32).collect()),
            };
            let cfg = wbraster::RasterConfig { cols: 32, rows: 32, bands: 1, data_type: t, nodata, ..Default::default() };
            let raster = Raster::from_data_native(cfg, data).unwrap();
            let header = crate::RasterDataset::new(raster.clone(), true);
            for writer in ["whole", "rows", "dataset"] {
                let path = dir.path().join(format!("{writer}.tif"));
                let p = path.to_str().unwrap();
                let _ = crate::warnings::take();
                match writer {
                    "whole" => {
                        write(&raster, p, &TiffOptions::from_write_options(&options, false, Some(nodata)).unwrap().unwrap()).unwrap()
                    }
                    "rows" => {
                        let mut w = GeoTiffWriter::create(p, &header, &options).unwrap();
                        w.write_rows(&raster.data, 32, None).unwrap();
                        w.finish().unwrap();
                    }
                    _ => {
                        // A sidecar beside the path, which the writer rewrites with the band nodata.
                        std::fs::write(format!("{p}.aux.xml"), "<PAMDataset/>").unwrap();
                        crate::write_raster_dataset(&header, p, &options).unwrap();
                        assert!(std::fs::read_to_string(format!("{p}.aux.xml")).unwrap().contains("<NoDataValue>nan</NoDataValue>"));
                    }
                }
                let case = format!("{t:?}, {writer}");
                assert_eq!(crate::warnings::take(), Vec::<String>::new(), "{case}");
                let mut dec = tiff::decoder::Decoder::new(std::fs::File::open(&path).unwrap()).unwrap();
                let tables = crate::geotiff::ChunkTables::read(&mut dec).unwrap();
                assert_eq!(tables.counts.iter().map(|n| *n > 0).collect::<Vec<_>>(), [true, false, true, true], "{case}");
                assert_eq!(bits(&crate::geotiff::read_geotiff(&path).unwrap().raster.data), bits(&raster.data), "{case}");
                let lazy = crate::LazyRaster::open(path.to_str().unwrap()).unwrap().read().unwrap();
                assert_eq!(bits(&lazy.raster.data), bits(&raster.data), "{case}");
            }
        }
    }

    /// Catches blocks left out under a lossy compression, which stores a block of nodata only
    /// approximately, so that a block left out would read otherwise than stored: `SPARSE_OK` is
    /// then ignored, with a warning. Lossless WebP, and LERC without an error, leave blocks out.
    #[test]
    fn sparse_ok_is_ignored_with_a_lossy_compression() {
        let co = |extra: &[(&str, &str)]| crate::RasterWriteOptions {
            creation_options: [("SPARSE_OK", "YES")].iter().chain(extra).map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        };
        let mut cases = vec![(vec![("COMPRESS", "JPEG")], true), (vec![("COMPRESS", "WEBP")], true)];
        cases.push((vec![("COMPRESS", "WEBP"), ("WEBP_LOSSLESS", "YES")], false));
        if cfg!(feature = "lerc") {
            cases.extend([(vec![("COMPRESS", "LERC"), ("MAX_Z_ERROR", "0.5")], true), (vec![("COMPRESS", "LERC")], false)]);
        }
        for (extra, lossy) in cases {
            let _ = crate::warnings::take();
            let opts = TiffOptions::from_write_options(&co(&extra), false, Some(7.0)).unwrap().unwrap();
            assert_eq!(opts.sparse, !lossy, "{extra:?}");
            let warned: &[&str] = if lossy { &["SPARSE_OK is ignored with a lossy compression: every block is written"] } else { &[] };
            assert_eq!(crate::warnings::take(), warned, "{extra:?}");
        }
    }
}

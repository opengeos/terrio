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
    /// JPEG: three bands in YCbCr (`PHOTOMETRIC=YCBCR`), subsampled.
    pub jpeg_ycbcr: bool,
    pub webp_quality: u8,
    pub webp_lossless: bool,
}

impl Default for CodecParams {
    fn default() -> Self {
        Self { max_z_error: 0.0, jpeg_quality: 75, jpeg_ycbcr: false, webp_quality: 75, webp_lossless: false }
    }
}

/// The geometry of a chunk to compress: its size and samples.
struct ChunkInfo {
    w: usize,
    h: usize,
    spp: usize,
    t: DataType,
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

    fn compress(self, data: Vec<u8>, c: &ChunkInfo, p: &CodecParams) -> Result<Vec<u8>> {
        Ok(match self {
            Self::None => data,
            Self::Lzw => weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
                .encode(&data)
                .map_err(|e| Error::InvalidData(format!("LZW: {e}")))?,
            Self::Deflate => deflate(&data)?,
            Self::PackBits => packbits(&data),
            Self::Zstd => zstd_compress(&data)?,
            Self::Lerc => lerc_encode(&data, c, p.max_z_error)?,
            Self::LercDeflate => deflate(&lerc_encode(&data, c, p.max_z_error)?)?,
            Self::LercZstd => zstd_compress(&lerc_encode(&data, c, p.max_z_error)?)?,
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

fn deflate(data: &[u8]) -> Result<Vec<u8>> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(6));
    enc.write_all(data)?;
    Ok(enc.finish()?)
}

/// Encodes a chunk (little-endian samples, interleaved) as a LERC blob of `spp` values per pixel,
/// as GDAL's LERC codec does. Floating-point chunks of one sample with NaNs get a mask of the
/// valid pixels.
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
            let valid: Vec<u8> = v.chunks_exact(c.spp).map(|px| u8::from(!px.iter().all(|x| x.is_nan()))).collect();
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
    let mut rgba = if chunk.spp == 4 { data } else { data.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]).collect() };
    // The pure Rust VP8 encoder accepts opaque RGB. Preserve alpha in the WebP
    // container's independent, uncompressed ALPH chunk instead of ignoring quality.
    let alpha = (!options.webp_lossless && chunk.spp == 4 && rgba.as_chunks::<4>().0.iter().any(|p| p[3] != 255)).then(|| {
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
    let image = webp_rust::ImageBuffer { width: chunk.w, height: chunk.h, rgba };
    let encoded = if options.webp_lossless {
        webp_rust::encode_lossless_with_config(&image, &webp_rust::LosslessEncodingConfig { z_level: 2 }, None)
    } else {
        webp_rust::encode_lossy_with_config(
            &image,
            &webp_rust::LossyEncodingConfig { quality: options.webp_quality as f32, method: 0, ..Default::default() },
            None,
        )
    }
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
    extended.extend_from_slice(&((chunk.w - 1) as u32).to_le_bytes()[..3]);
    extended.extend_from_slice(&((chunk.h - 1) as u32).to_le_bytes()[..3]);
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
    zune_jpegxl::JxlSimpleEncoder::new(&data, options).encode(&mut out).map_err(|e| Error::InvalidData(format!("JPEG XL: {e:?}")))?;
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

fn zstd_compress(data: &[u8]) -> Result<Vec<u8>> {
    Ok(crate::zstandard::compress(data, 9)?)
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
    /// A GeoTIFF whose overviews (and their masks) a COG copies rather than computing its own,
    /// as GDAL's COG driver does with a source that has some (`OVERVIEWS=AUTO`).
    pub source_overviews: Option<String>,
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
    /// The TIFF predictor: 1 (none), 2 (horizontal differencing) or 3 (floating point).
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
            source_overviews: None,
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
            georeferenced: true,
            predictor: 1,
            geo_transform: None,
            gcps: Vec::new(),
            mask: None,
            mask_without_dataset: false,
            rpc: Vec::new(),
            codec: CodecParams::default(),
            complex: false,
            domains: Vec::new(),
            drop_default_items: (false, false),
        }
    }
}

/// How the overviews given as factors are computed: as GDAL does, or terrio's minimum or
/// maximum of the valid pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverviewResampling {
    Gdal(Method),
    Min,
    Max,
}

impl OverviewResampling {
    fn parse(s: &str) -> Result<Self> {
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
        "'{name}' overview resampling is not supported (nearest, average, rms, bilinear, cubic, cubicspline, lanczos, mode)"
    ))
}

/// Whether a creation option value means yes.
fn yes(v: &str) -> bool {
    matches!(v.to_ascii_uppercase().as_str(), "YES" | "TRUE" | "ON" | "1")
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
        if cog {
            if let Some(b) = size("BLOCKSIZE")? {
                o.tile_size = Some(b);
            }
            // GDAL's COG driver reads OVERVIEW_RESAMPLING, then RESAMPLING; rasterio's
            // `overview_resampling` keyword is the first.
            if let Some(r) = co("OVERVIEW_RESAMPLING").or_else(|| co("RESAMPLING")).or(options.overview_resampling.as_deref()) {
                o.cog_resampling = Some(Method::parse(r).ok_or_else(|| unsupported_resampling(r))?);
                o.cog_resampling_name = Some(r.to_ascii_uppercase());
            }
            o.source_overviews = co("@COPY_SRC_OVERVIEWS").map(str::to_string);
            o.mask_without_dataset = co("@MASK_WITHOUT_DATASET").is_some_and(yes);
            if let Some(n) = co("OVERVIEW_COUNT") {
                let n: i64 = n.trim().parse().map_err(|_| Error::InvalidArgument(format!("invalid OVERVIEW_COUNT '{n}'")))?;
                o.overview_count = usize::try_from(n).ok();
            }
            if co("OVERVIEWS").is_some_and(|v| v.eq_ignore_ascii_case("NONE")) {
                o.overview_count = Some(0);
            }
            o.source_block = options.source_block_size;
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
        o.georeferenced = !co("GEOREFERENCED").is_some_and(|v| !yes(v));
        let src_mdd: Vec<&str> =
            options.creation_options.iter().filter(|(k, _)| k.eq_ignore_ascii_case("SRC_MDD")).map(|(_, v)| v.as_str()).collect();
        (o.domains, o.drop_default_items) = MetadataDomain::parse(co("@METADATA_DOMAINS"), cog, co("COPY_SRC_MDD"), &src_mdd)?;
        if let Some(p) = co("PREDICTOR") {
            o.predictor = match p.trim().to_ascii_uppercase().as_str() {
                "1" | "NO" | "NONE" => 1,
                "2" | "YES" => 2,
                "3" => 3,
                other => return Err(Error::InvalidArgument(format!("invalid PREDICTOR '{other}' (1, 2, 3)"))),
            };
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
        o.codec.jpeg_ycbcr = o.photometric == Some(6);
        o.alpha = co("ALPHA").is_some_and(|v| yes(v) || v.eq_ignore_ascii_case("NON-PREMULTIPLIED"));
        if !o.color_table.is_empty() && o.photometric.is_none() {
            o.photometric = Some(3);
        }
        Ok(Some(o))
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
}

/// Writes a raster as a GeoTIFF or COG.
pub(crate) fn write(raster: &Raster, path: &str, opts: &TiffOptions) -> Result<()> {
    let images = prepare(raster, opts)?;
    let mut file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
    images.write_to(&mut file)?;
    file.flush()?;
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
    let predictor = if opts.compression.takes_predictor() && !opts.complex { opts.predictor } else { 1 };
    if predictor == 3 && !matches!(raster.data_type, DataType::F32 | DataType::F64) {
        return Err(Error::InvalidArgument("PREDICTOR=3 is only supported for floating-point data".into()));
    }
    // The colour interpretations GDAL reads from the photometric and extra samples written
    // (`GTiffRasterBand`): GDAL_METADATA holds only the bands' others (see [`geo_tags`]).
    let implied: Vec<&str> = (0..samples).map(|i| implied_color_interp(photometric, &extra, base_samples, i)).collect();
    let layout =
        Layout { tile, rows_per_strip: opts.rows_per_strip, planar: opts.planar && samples > 1, photometric, extra, predictor, parts };
    // GDAL's COG driver records the resampling of the overviews it makes (`@OVERVIEW_RESAMPLING`,
    // an IMAGE_STRUCTURE item): the option as given, else `GetResampling`'s; for overviews it
    // copies, the source's `RESAMPLING` item.
    let image_structure: Vec<(String, String)> = match (tile, &opts.source_overviews) {
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
    let mut geo = geo_tags(raster, opts, &image_structure, &implied)?;
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
    if pixel_is_point(raster) {
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
    let color_map = (photometric == 3).then(|| color_map_tag(&opts.color_table, raster.data_type));
    Ok(Plan { tile, layout, geo, color_map })
}

/// The georeferencing, nodata and metadata tags [`write`] gives the full-resolution image of
/// `raster` (only its header is read) with these options.
pub(crate) fn georeferencing_tags(raster: &Raster, opts: &TiffOptions) -> Result<Vec<(u16, Value)>> {
    Ok(plan(raster, opts)?.geo)
}

fn prepare(raster: &Raster, opts: &TiffOptions) -> Result<Prepared> {
    let Plan { tile, layout, geo, color_map } = plan(raster, opts)?;
    if let Some(m) = &opts.mask
        && m.len() != raster.cols * raster.rows
    {
        return Err(Error::InvalidArgument(format!("the mask has {} values for {} pixels", m.len(), raster.cols * raster.rows)));
    }

    // The full-resolution image, then overviews: a COG's until one fits in a tile, or the
    // factors asked for.
    let mut levels = vec![Level::from_raster(raster)];
    // The per-dataset mask of each level, as GDAL reads it (0 or 255).
    let mut masks: Vec<Vec<u8>> = opts.mask.iter().map(|m| m.iter().map(|v| if *v != 0 { 255 } else { 0 }).collect()).collect();
    // The overviews of the mask as they are computed, before they are written as bits (level `i`
    // at `i - 1`): GDAL keeps them in a temporary Byte file, which the next level is computed
    // from (see [`mask_levels`]).
    let mut raw_masks: Vec<Vec<u8>> = Vec::new();
    let (sizes, method) = match (opts.cog, tile) {
        (true, Some(_)) if opts.source_overviews.is_some() => (Vec::new(), None),
        (true, Some((t, _))) => {
            (gdal_overview::cog_sizes(raster.cols, raster.rows, t as usize, opts.overview_count), Some(cog_method(opts)))
        }
        (true, None) => (Vec::new(), None),
        (false, _) => {
            let mut factors: Vec<usize> = opts.overviews.iter().copied().filter(|f| *f > 1).collect();
            factors.sort_unstable();
            factors.dedup();
            let sizes = factors.iter().map(|f| (raster.cols.div_ceil(*f), raster.rows.div_ceil(*f))).collect();
            match opts.overview_resampling {
                OverviewResampling::Gdal(m) => (sizes, Some(m)),
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
    if let (true, Some(_), Some(path)) = (opts.cog, tile, &opts.source_overviews) {
        copy_source_overviews(path, raster, opts, &mut levels, &mut masks)?;
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
        let mut img = level.image(raster.data_type, opts.compression, &opts.codec, &layout)?;
        if i > 0 {
            img.tags.push((254, Value::Longs(vec![1])));
        }
        // Georeferencing on the full-resolution image; nodata on every image, as GDAL does, but
        // not on the overviews of 64-bit integer rasters (see [`overview_nodata`]).
        for (tag, v) in &geo {
            if i == 0 || (*tag == 42113 && overview_nodata(raster.data_type)) {
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
        if let Some(m) = masks.get(i) {
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

/// The resampling of a COG's overviews: the one asked for, else GDAL's default.
fn cog_method(opts: &TiffOptions) -> Method {
    opts.cog_resampling.unwrap_or_else(|| Method::cog_default(!opts.color_table.is_empty() || opts.complex))
}

/// The overviews of the GeoTIFF a COG is converted from, copied as they are, as GDAL's COG driver
/// copies a source's (`OVERVIEWS=AUTO`, `COPY_SRC_OVERVIEWS`), with their masks: the source's
/// when its mask has overviews, else made at the overviews' sizes as `GDALCOGCreator::Create`
/// makes them (`bGenerateMskOvr`), cascading from the full-resolution mask.
fn copy_source_overviews(path: &str, raster: &Raster, opts: &TiffOptions, levels: &mut Vec<Level>, masks: &mut Vec<Vec<u8>>) -> Result<()> {
    let mut src = crate::LazyRaster::open_tiff(path)?.ok_or_else(|| Error::InvalidArgument(format!("{path}: not a GeoTIFF")))?;
    if (src.width(), src.height(), src.band_count(), src.data_type()) != (raster.cols, raster.rows, raster.bands, raster.data_type) {
        return Err(Error::InvalidArgument(format!("{path}: not the raster written")));
    }
    let sizes = src.overviews();
    let own_masks = (0..sizes.len()).all(|i| src.has_overview_mask(i));
    let block = gdal_overview::ovr_block(opts.source_block);
    for (i, (w, h)) in sizes.into_iter().enumerate() {
        let level = src.read_window(crate::Window::full(w, h), &crate::WindowOptions { bands: Vec::new(), overview: Some(i) })?;
        if !masks.is_empty() {
            let m = if own_masks {
                src.read_mask(crate::Window::full(w, h), Some(i))?.unwrap_or_else(|| vec![255; w * h])
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
    let last_is_alpha = opts
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
        compression.compress(buf, &ChunkInfo { w: chunk_w, h, spp: 1, t: DataType::U8 }, &CodecParams::default())
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
    Ok(Image { tags, chunks, tiled })
}

/// The `ColorMap` tag of a color table: every red value, then green, then blue, as 16 bits.
fn color_map_tag(table: &[[u8; 4]], t: DataType) -> Value {
    let n = if t == DataType::U16 { 65536 } else { 256 };
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
        let chunks = crate::par::map_range_coarse(planes * per_plane, |i| {
            let (plane, j) = (i / per_plane, i % per_plane);
            let (x0, y0) = ((j % across) * chunk_w, (j / across) * chunk_h);
            // Strips stop at the last row; tiles are padded to full size.
            let h = if tiled { chunk_h } else { chunk_h.min(self.rows - y0) };
            // A complex sample's parts are interleaved like the bands of a pixel.
            let mut buf = if layout.planar {
                let plane_len = parts * band_len;
                with_data!(self.data.as_ref(), v => chunk_bytes(&v[plane * plane_len..(plane + 1) * plane_len], self.cols, self.rows, parts, x0, y0, chunk_w, h))
            } else {
                with_data!(self.data.as_ref(), v => chunk_bytes(v, self.cols, self.rows, spp * parts, x0, y0, chunk_w, h))
            };
            apply_predictor(&mut buf, layout.predictor, chunk_w * chunk_spp, chunk_spp, size);
            compression.compress(buf, &ChunkInfo { w: chunk_w, h, spp: chunk_spp, t }, codec)
        });
        let chunks = chunks.into_iter().collect::<Result<Vec<_>>>()?;
        let (bits, format) = sample_format(t);
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
        Ok(Image { tags, chunks, tiled })
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

/// Whether the raster declares point samples.
fn pixel_is_point(r: &Raster) -> bool {
    r.metadata.iter().any(|(k, v)| k == "AREA_OR_POINT" && v.eq_ignore_ascii_case("Point"))
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

/// The GeoTIFF tags of a raster written with `opts`. `implied` holds the colour interpretations
/// the written photometric and extra samples give the bands: as GDAL's
/// `GTiffDataset::WriteMetadata`, only the bands' other colour interpretations go to
/// GDAL_METADATA (GDAL 3.12.1 copies a gray or RGB file without it, and three gray Byte bands,
/// written as RGB, with theirs).
fn geo_tags(r: &Raster, opts: &TiffOptions, image_structure: &[(String, String)], implied: &[&str]) -> Result<Vec<(u16, Value)>> {
    let mut tags = vec![
        (33550, Value::Doubles(vec![r.cell_size_x, r.cell_size_y, 0.0])),
        (33922, Value::Doubles(vec![0.0, 0.0, 0.0, r.x_min, r.y_max(), 0.0])),
    ];
    let crs = super::common::spatial_ref(&r.crs);
    let mut v = crate::drivers::geokeys::GeoKeyValues::default();
    v.shorts.push((1025, if pixel_is_point(r) { 2 } else { 1 }));
    if crs.is_unknown() && pixel_is_point(r) {
        return Err(Error::Unsupported(
            "PixelIsPoint GeoTIFF writing requires a CRS; a lone raster-type GeoKey would imply a local CRS".into(),
        ));
    }
    let local = crs.wkt.as_deref().and_then(local_cs);
    if let Some((name, unit)) = &local {
        // A local CRS, as GDAL writes one: a citation and the linear unit, no model type.
        v.ascii.push((1026, name.clone()));
        v.shorts.push((3076, *unit));
    } else if !crs.is_unknown() {
        let geographic = crs.is_geographic();
        // A compound CRS of EPSG CRSs: its horizontal CRS's keys, its vertical CRS's and its name,
        // in GeoTIFF 1.1 as GDAL 3.12.1 writes them (`GTIFSetFromOGISDefnEx`); a compound code in
        // a horizontal key reads as another CRS.
        let compound = crs.compound_parts().and_then(|(h, v)| Some((u16::try_from(h).ok()?, u16::try_from(v).ok()?)));
        match (compound, crs.epsg.and_then(|c| u16::try_from(c).ok())) {
            (Some((h, vertical)), _) => {
                let geographic = crate::crs::SpatialRef::from_epsg(u32::from(h)).is_ok_and(|s| s.is_geographic());
                v.shorts.push((1024, if geographic { 2 } else { 1 }));
                v.shorts.push((if geographic { 2048 } else { 3072 }, h));
                v.shorts.push((4096, vertical));
                v.ascii.push((1026, crs.name().unwrap_or_else(|| "unnamed".into())));
            }
            (None, Some(code)) => {
                v.shorts.push((1024, if geographic { 2 } else { 1 }));
                v.shorts.push((if geographic { 2048 } else { 3072 }, code));
            }
            (None, None) => {
                let name = crs.name().unwrap_or_else(|| "unnamed".into());
                let user = crate::drivers::geokeys::user_keys(&crs, &name);
                match user {
                    Some(keys) => v = merge(v, keys),
                    None => {
                        // GDAL's fallback for methods GeoTIFF cannot describe: the WKT in an ESRI PE
                        // citation, with a user-defined model type.
                        let wkt = crs.to_wkt().ok_or_else(|| Error::Crs("the raster's CRS has no WKT definition".into()))?;
                        v.shorts.push((1024, 32767));
                        v.ascii.push((3073, format!("ESRI PE String = {wkt}")));
                    }
                }
            }
        }
    }
    v.shorts.sort_by_key(|k| k.0);
    let mut keys: Vec<[u16; 4]> = v.shorts.iter().map(|(id, val)| [*id, 0, 1, *val]).collect();
    let doubles: Vec<f64> = v.doubles.iter().map(|d| d.1).collect();
    keys.extend(v.double_keys());
    let mut ascii = String::new();
    for (id, text) in &v.ascii {
        let text = format!("{text}|");
        keys.push([*id, 34737, text.len() as u16, ascii.len() as u16]);
        ascii.push_str(&text);
    }
    keys.sort_by_key(|k| k[0]);
    // GeoTIFF 1.1 (minor revision 1) for a vertical CRS, which GDAL strips from 1.0 files.
    let minor = u16::from(keys.iter().any(|k| k[0] == 4096));
    let mut dir = vec![1u16, 1, minor, keys.len() as u16];
    dir.extend(keys.iter().flatten());
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
            if b.color_interp.as_deref().is_some_and(|c| implied.get(i).is_some_and(|m| m.eq_ignore_ascii_case(c))) {
                b.color_interp = None;
            }
            b
        })
        .collect();
    if let Some(xml) = gdal_metadata_xml(&r.metadata, &bands, &opts.domains, opts.drop_default_items, image_structure) {
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

/// The name and EPSG linear unit code of a `LOCAL_CS` WKT (metres and feet).
fn local_cs(wkt: &str) -> Option<(String, u16)> {
    let rest = wkt.trim_start().strip_prefix("LOCAL_CS[\"")?;
    let name = rest.split('"').next()?.to_string();
    let lower = rest.to_ascii_lowercase();
    let unit = if lower.contains("unit[\"metre\"") || lower.contains("unit[\"meter\"") {
        9001
    } else if lower.contains("unit[\"foot\"") {
        9002
    } else if lower.contains("us survey foot") {
        9003
    } else {
        return None;
    };
    Some((name, unit))
}

fn merge(mut a: crate::drivers::geokeys::GeoKeyValues, b: crate::drivers::geokeys::GeoKeyValues) -> crate::drivers::geokeys::GeoKeyValues {
    a.shorts.extend(b.shorts);
    a.doubles.extend(b.doubles);
    a.ascii.extend(b.ascii);
    a
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
pub(crate) fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

pub(crate) fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

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
    file: std::io::BufWriter<std::fs::File>,
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
    tags: Vec<(u16, Value)>,
    /// The internal mask's compressed chunks, written after the image data.
    mask: Option<Vec<Vec<u8>>>,
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
    /// terrio's own writer does not encode (WEBP...), which need the whole raster; and when the
    /// file cannot be created.
    pub fn create(path: &str, header: &crate::RasterDataset, options: &crate::RasterWriteOptions) -> Result<Self> {
        let inner = if options.driver == Some(crate::formats::RasterDriver::Cog) {
            RowWriter::Cog(Box::new(std::sync::Mutex::new(cog_rows(path, header, options)?)))
        } else {
            RowWriter::Tiff(Box::new(TiffRows::create(path, header, options)?))
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
    /// When the data does not hold `rows` rows of every band, rows go past the raster's height,
    /// a mask is given for a file without one, or the file cannot be written.
    pub fn write_rows(&mut self, data: &RasterData, rows: usize, mask: Option<&[u8]>) -> Result<()> {
        match &mut self.inner {
            RowWriter::Tiff(t) => t.write_rows(data, rows, mask),
            RowWriter::Cog(c) => c.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner).write_rows_masked(
                data.clone(),
                rows,
                mask.map(<[u8]>::to_vec),
            ),
        }
    }

    /// Writes the directories (and the COG's levels) and closes the file.
    ///
    /// # Errors
    /// When rows are missing, or the file cannot be written.
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
    /// with the rows.
    ///
    /// # Errors
    /// For COGs, overviews and compressions terrio's own writer does not encode (WEBP...), which
    /// need the whole raster, and when the file cannot be created.
    pub fn create(path: &str, header: &crate::RasterDataset, options: &crate::RasterWriteOptions) -> Result<Self> {
        if matches!(options.driver, Some(d) if d != crate::formats::RasterDriver::GTiff) {
            return Err(Error::Unsupported("rasters are written in rows as GeoTIFFs only".into()));
        }
        if !options.overviews.is_empty() {
            return Err(Error::Unsupported("overviews need the whole raster: build them after writing".into()));
        }
        let mut options = options.inherited_from(header, Some(crate::formats::RasterDriver::GTiff));
        let with_mask = options.mask.take().is_some();
        let opts = TiffOptions::from_write_options(&options, false, crate::dataset::tiff_nodata(header))?.ok_or_else(|| {
            let name = options.compression.as_deref().unwrap_or_default();
            Error::Unsupported(format!("{name} compression needs the whole raster"))
        })?;
        let r = &header.raster;
        let plan = plan(r, &opts)?;
        let (cols, rows, bands) = (r.cols, r.rows, r.bands);
        let size = r.data_type.size_bytes();
        let planar = plan.layout.planar;
        let chunk_spp = if planar { 1 } else { bands };
        let (chunk_w, block_rows, across, down) = match plan.tile {
            Some((tw, th)) => (tw as usize, th as usize, cols.div_ceil(tw as usize), rows.div_ceil(th as usize)),
            None => {
                let per = strip_rows(opts.rows_per_strip, rows, cols * chunk_spp * size, opts.compression);
                (cols, per, 1, rows.div_ceil(per))
            }
        };
        let planes = if planar { bands } else { 1 };
        let mut layout = plan.layout;
        layout.rows_per_strip = Some(block_rows as u32);
        let raw = (cols * rows * bands * size) as u64;
        let big = opts.bigtiff || raw + raw / 4 > (u32::MAX as u64) - (1 << 24);
        let mut tags = plan.geo;
        if let Some(cm) = plan.color_map {
            tags.push((320, cm));
        }
        let mut file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
        let head: Vec<u8> = if big {
            [b"II".as_slice(), &43u16.to_le_bytes(), &8u16.to_le_bytes(), &0u16.to_le_bytes(), &0u64.to_le_bytes()].concat()
        } else {
            [b"II".as_slice(), &42u16.to_le_bytes(), &0u32.to_le_bytes()].concat()
        };
        file.write_all(&head)?;
        let n = planes * across * down;
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
            offsets: vec![0; n],
            counts: vec![0; n],
            pending: RasterData::new_filled(r.data_type, 0, 0.0),
            pending_mask: Vec::new(),
            pending_rows: 0,
            rows_done: 0,
            tags,
            mask: with_mask.then(Vec::new),
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
    /// When the data does not hold `rows` rows of every band, rows go past the raster's height,
    /// or the file cannot be written.
    pub fn write_rows(&mut self, data: &RasterData, rows: usize, mask: Option<&[u8]>) -> Result<()> {
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
        if self.mask.is_some() {
            let m = mask.ok_or_else(|| Error::InvalidArgument("the file has a mask: give its rows".into()))?;
            if m.len() != n {
                return Err(Error::InvalidArgument(format!("{} mask values for {rows} rows of {} columns", m.len(), self.cols)));
            }
            self.pending_mask.extend_from_slice(m);
        }
        // Pending rows are kept band after band, in the raster's type.
        let old = self.pending_rows;
        let (cols, bands) = (self.cols, self.bands);
        macro_rules! merge {
            ($($v:ident),*) => {
                match (&mut self.pending, data) {
                    $((RasterData::$v(p), RasterData::$v(d)) => {
                        let mut merged = Vec::with_capacity(cols * (old + rows) * bands);
                        for b in 0..bands {
                            merged.extend_from_slice(&p[b * cols * old..(b + 1) * cols * old]);
                            merged.extend_from_slice(&d[b * n..(b + 1) * n]);
                        }
                        *p = merged;
                    })*
                    _ => return Err(Error::InvalidArgument("the rows are not of the raster's data type".into())),
                }
            };
        }
        merge!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64);
        self.pending_rows = old + rows;
        // The complete block rows (and the last rows of the raster), compressed together, then
        // the rows left over, kept for the next call.
        let total = self.pending_rows;
        let start = if self.rows_done + total == self.rows { total } else { total / self.block_rows * self.block_rows };
        if start > 0 {
            self.flush_block_rows(start, total)?;
            macro_rules! keep {
                ($($v:ident),*) => {
                    match &mut self.pending {
                        $(RasterData::$v(p) => {
                            let mut rest = Vec::with_capacity(cols * (total - start) * bands);
                            for b in 0..bands {
                                rest.extend_from_slice(&p[(b * total + start) * cols..(b + 1) * total * cols]);
                            }
                            *p = rest;
                        })*
                    }
                };
            }
            keep!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64);
            if self.mask.is_some() {
                self.pending_mask.drain(..start * cols);
            }
            self.pending_rows = total - start;
        }
        Ok(())
    }

    /// Compresses (in parallel) and writes the first `h` pending rows (of `total`): whole block
    /// rows, or the raster's last rows.
    fn flush_block_rows(&mut self, h: usize, total: usize) -> Result<()> {
        let (cols, bands) = (self.cols, self.bands);
        macro_rules! slab {
            ($($v:ident),*) => {
                match &self.pending {
                    $(RasterData::$v(p) => {
                        let mut slab = Vec::with_capacity(cols * h * bands);
                        for b in 0..bands {
                            let src = b * total * cols;
                            slab.extend_from_slice(&p[src..src + cols * h]);
                        }
                        RasterData::$v(slab)
                    })*
                }
            };
        }
        let data = slab!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64);
        let level = Level { cols, rows: h, bands, data: std::borrow::Cow::Owned(data) };
        // Tiles are padded to full height.
        let layout = Layout { rows_per_strip: Some(self.block_rows as u32), extra: self.layout.extra.clone(), ..self.layout };
        let img = level.image(self.data_type, self.compression, &self.codec, &layout)?;
        let block_row = self.rows_done / self.block_rows;
        let per_plane = self.across * self.rows.div_ceil(self.block_rows);
        // The chunks of each plane, block row after block row.
        let slab_per_plane = self.across * h.div_ceil(self.block_rows);
        for (k, chunk) in img.chunks.iter().enumerate() {
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
        if let Some(chunks) = &mut self.mask {
            let m = &self.pending_mask[..h * cols];
            let mi = mask_image(m, cols, h, (self.chunk_w, self.block_rows, self.layout.tile.is_some()))?;
            chunks.extend(mi.chunks);
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
    /// When rows are missing, or the file cannot be written.
    pub fn finish(mut self) -> Result<()> {
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
        tags.push((off_tag, Value::Offsets(self.offsets.clone())));
        tags.push((cnt_tag, Value::Offsets(self.counts.clone())));
        tags.sort_by_key(|(t, _)| *t);
        // The mask's data, then its directory.
        let mut mask_ifd: Option<Vec<(u16, Value)>> = None;
        if let Some(chunks) = self.mask.take() {
            let mut offsets = Vec::with_capacity(chunks.len());
            let mut counts = Vec::with_capacity(chunks.len());
            for c in &chunks {
                offsets.push(self.pos);
                counts.push(c.len() as u64);
                self.file.write_all(c)?;
                self.pos += c.len() as u64;
                self.check_size()?;
            }
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
        // The first directory's offset, in the header.
        let mut file = self.file.into_inner().map_err(|e| Error::Io(e.into_error()))?;
        use std::io::{Seek, SeekFrom};
        if self.big {
            file.seek(SeekFrom::Start(8))?;
            file.write_all(&first.to_le_bytes())?;
        } else {
            file.seek(SeekFrom::Start(4))?;
            file.write_all(&(first as u32).to_le_bytes())?;
        }
        file.flush()?;
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

/// The `a_rows` rows of `a` followed by the `b_rows` rows of `b`, band after band.
fn append_rows(a: &RasterData, a_rows: usize, b: &RasterData, b_rows: usize, cols: usize, bands: usize) -> Result<RasterData> {
    macro_rules! join {
        ($($v:ident),*) => {
            match (a, b) {
                $((RasterData::$v(a), RasterData::$v(b)) => {
                    let mut out = Vec::with_capacity(cols * (a_rows + b_rows) * bands);
                    for band in 0..bands {
                        out.extend_from_slice(&a[band * cols * a_rows..(band + 1) * cols * a_rows]);
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

fn chunk_spool() -> Result<ChunkSpool> {
    #[cfg(not(target_family = "wasm"))]
    return Ok(tempfile::tempfile()?);
    #[cfg(target_family = "wasm")]
    Ok(Default::default())
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
/// rows of that level its next blocks need (`buf_rows` rows from `buf_row0`, band after band),
/// and its next row.
struct CogBuild {
    level: usize,
    src: usize,
    plan: gdal_overview::Plan,
    buf: RasterData,
    buf_row0: usize,
    buf_rows: usize,
    next_y: usize,
    mask: Vec<u8>,
    mask_plan: gdal_overview::Plan,
}

/// A COG written a few rows at a time, from the top, so that the raster is never held whole. Each
/// level (the full resolution, then the overviews down to one that fits in a tile) compresses its
/// rows of tiles as they are complete, into a temporary file, and keeps the rows the next level's
/// next blocks are resampled from, computing them as GDAL does once their rows are in (as
/// [`write`] computes them from the whole raster). Once the last row is in, the file is laid out as
/// [`write`] lays it out: the IFDs, then the levels' tiles from the smallest overview, copied from
/// the temporary files. The file is byte for byte the same.
pub(crate) struct CogWriter {
    file: std::io::BufWriter<std::fs::File>,
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
}

/// The most rows of tiles a [`CogWriter`] compresses at once, so that the tiles of the next rows
/// keep the threads busy while the last tiles of the first are compressed.
#[cfg(not(target_family = "wasm"))]
const COG_IN_FLIGHT: usize = 3;

impl CogWriter {
    /// Creates the file, for a raster described by `raster` (its pixels are not read). `opts` are
    /// a COG's. `opts.mask` enables a mask supplied with each row slab.
    pub(crate) fn create(path: &str, raster: &Raster, opts: &TiffOptions) -> Result<Self> {
        if !opts.cog {
            return Err(Error::Unsupported("a COG row writer requires COG options".into()));
        }
        let Plan { tile, layout, geo, color_map } = plan(raster, opts)?;
        let tile = tile.map_or(512, |(t, _)| t as usize);
        let planes = if layout.planar { raster.bands / layout.parts } else { 1 };
        let mut sizes = vec![(raster.cols, raster.rows)];
        sizes.extend(gdal_overview::cog_sizes(raster.cols, raster.rows, tile, opts.overview_count));
        let (method, block) = (cog_method(opts), gdal_overview::ovr_block(opts.source_block));
        let builds = (1..sizes.len())
            .map(|level| {
                // As GDAL does, each overview is computed from the previous one when it is larger.
                let src = if sizes[level - 1].0 > sizes[level].0 { level - 1 } else { 0 };
                let plan = gdal_overview::Plan::new(method, sizes[src], sizes[level], raster.bands, block, raster.data_type);
                let buf = RasterData::new_filled(raster.data_type, 0, 0.0);
                let mask_plan = gdal_overview::Plan::new(method, sizes[src], sizes[level], 1, block, DataType::U8);
                CogBuild { level, src, plan, buf, buf_row0: 0, buf_rows: 0, next_y: 0, mask: Vec::new(), mask_plan }
            })
            .collect();
        let levels = sizes
            .into_iter()
            .map(|(cols, rows)| {
                Ok(CogLevel {
                    cols,
                    rows,
                    received: 0,
                    pending: RasterData::new_filled(raster.data_type, 0, 0.0),
                    pending_rows: 0,
                    spools: (0..planes).map(|_| chunk_spool()).collect::<Result<_>>()?,
                    sizes: vec![Vec::new(); planes],
                    tags: Vec::new(),
                    mask: if opts.mask.is_some() {
                        Some(CogMask { pending: Vec::new(), spool: chunk_spool()?, sizes: Vec::new(), tags: Vec::new() })
                    } else {
                        None
                    },
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let file = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
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
    /// Rejects a sample-type or shape mismatch before consuming rows or changing overview state.
    pub(crate) fn write_rows(&mut self, data: RasterData, rows: usize) -> Result<()> {
        self.write_rows_masked(data, rows, None)
    }

    pub(crate) fn write_rows_masked(&mut self, data: RasterData, rows: usize, mask: Option<Vec<u8>>) -> Result<()> {
        if data.data_type() != self.data_type {
            return Err(Error::InvalidArgument("the rows are not of the raster's data type".into()));
        }
        let l = &self.levels[0];
        if l.mask.is_some() != mask.is_some() || mask.as_ref().is_some_and(|m| m.len() != l.cols * rows) {
            return Err(Error::InvalidArgument("give one mask value per pixel when the COG has a mask".into()));
        }
        if l.received + rows > l.rows {
            return Err(Error::InvalidArgument(format!("{} rows past the raster's {}", l.received + rows, l.rows)));
        }
        if data.len() != l.cols * rows * self.bands {
            return Err(Error::InvalidArgument(format!(
                "{} values for {rows} rows of {} bands of {} columns",
                data.len(),
                self.bands,
                l.cols
            )));
        }
        // As the whole writer reads a mask: any nonzero value is valid, weighted as 255.
        let mask: Option<Vec<u8>> = mask.map(|m| m.into_iter().map(|v| if v != 0 { 255 } else { 0 }).collect());
        if rows > 0 { self.push(0, data, rows, mask) } else { Ok(()) }
    }

    /// Adds `rows` rows to level `i`: resampled into the levels computed from it as soon as their
    /// next blocks' rows are in, and compressed by rows of tiles.
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
            bd.buf = if bd.buf_rows == 0 { data.clone() } else { append_rows(&bd.buf, bd.buf_rows, &data, rows, cols, bands)? };
            if let Some(mask) = &mask {
                bd.mask.extend_from_slice(mask);
            }
            bd.buf_rows += rows;
            loop {
                let bd = &self.builds[b];
                if bd.next_y >= bd.plan.dst.1 {
                    break;
                }
                let (n, need) = bd.plan.rows(bd.next_y);
                if need.end > received && !done {
                    break;
                }
                let weights = gdal_overview::Weights {
                    alpha: self.alpha.0,
                    alpha_masks: self.alpha.1,
                    mask: mask.as_ref().map(|_| bd.mask.as_slice()),
                };
                let out = gdal_overview::block_row(&bd.plan, &bd.buf, bd.buf_row0, bd.buf_rows, bands, self.nodata, weights, bd.next_y);
                let next_mask = mask.as_ref().map(|_| {
                    let own = !self.mask_without_dataset || bd.src == 0;
                    let all_valid = if own { Vec::new() } else { vec![255; bd.mask.len()] };
                    let weights = gdal_overview::Weights {
                        alpha: own.then_some(0),
                        alpha_masks: false,
                        mask: (!own).then_some(all_valid.as_slice()),
                    };
                    let RasterData::U8(mut values) = gdal_overview::block_row(
                        &bd.mask_plan,
                        &RasterData::U8(bd.mask.clone()),
                        bd.buf_row0,
                        bd.buf_rows,
                        1,
                        None,
                        weights,
                        bd.next_y,
                    ) else {
                        unreachable!("a Byte mask")
                    };
                    if !self.mask_without_dataset {
                        values.iter_mut().for_each(|v| *v = if *v != 0 { 255 } else { 0 });
                    }
                    values
                });
                let bd = &mut self.builds[b];
                bd.next_y += n;
                // The rows no later block reads.
                let keep = bd.plan.first_row(bd.next_y).clamp(bd.buf_row0, bd.buf_row0 + bd.buf_rows);
                if keep > bd.buf_row0 {
                    let drop = keep - bd.buf_row0;
                    bd.buf = slice_rows(&bd.buf, bd.buf_rows, drop..bd.buf_rows, cols, bands);
                    if mask.is_some() {
                        bd.mask.drain(..drop * cols);
                    }
                    bd.buf_rows -= drop;
                    bd.buf_row0 = keep;
                }
                let level = bd.level;
                self.push(level, out, n, next_mask)?;
            }
        }
        // Whole rows of tiles, and the level's last rows.
        let l = &mut self.levels[i];
        if l.pending_rows == 0 {
            l.pending = data;
        } else {
            l.pending = append_rows(&l.pending, l.pending_rows, &data, rows, cols, bands)?;
        }
        l.pending_rows += rows;
        if let (Some(pending), Some(mask)) = (&mut l.mask, mask) {
            pending.pending.extend(mask);
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
            let mask = l.mask.as_mut().map(|m| m.pending.drain(..ready * cols).collect());
            self.compress(i, slab, ready, mask)?;
        }
        Ok(())
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

    /// Adds the oldest rows of tiles being compressed to their level's spools.
    #[cfg(not(target_family = "wasm"))]
    fn land(&mut self) -> Result<()> {
        let Some((i, rows, rx)) = self.in_flight.pop_front() else { return Ok(()) };
        let img = rx.recv().map_err(|_| Error::InvalidData("a COG tile compression stopped".into()))??;
        self.add(i, rows, img)
    }

    /// Adds compressed rows of tiles of level `i` to its spools.
    fn add(&mut self, i: usize, rows: usize, (img, mask): CogImages) -> Result<()> {
        let l = &mut self.levels[i];
        let per_plane = l.cols.div_ceil(self.tile) * rows.div_ceil(self.tile);
        let mut framed = vec![Vec::new(); l.spools.len()];
        for (k, c) in img.chunks.iter().enumerate() {
            let plane = k / per_plane;
            framed[plane].extend_from_slice(&chunk_leader(c));
            framed[plane].extend_from_slice(c);
            framed[plane].extend_from_slice(&chunk_trailer(c));
            l.sizes[plane].push(c.len() as u64);
        }
        for (spool, bytes) in l.spools.iter_mut().zip(framed) {
            spool.write_all(&bytes)?;
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
    /// When rows are missing, or the file cannot be written.
    pub(crate) fn finish(mut self) -> Result<()> {
        let (got, rows) = (self.levels[0].received, self.levels[0].rows);
        if got != rows {
            return Err(Error::InvalidArgument(format!("{got} rows written of {rows}")));
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
    use super::*;

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
            writer.write_rows(slice_rows(&raster.data, 35, 16..35, 65, 1), 19).unwrap();
            writer.finish().unwrap();
            assert_eq!(std::fs::read(path).unwrap(), encode(&raster, &opts).unwrap(), "{method:?}");
        }
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
                    webp_encode(vec![0; length], &ChunkInfo { w: 1, h: 1, spp, t: DataType::U8 }, &CodecParams::default()),
                    Err(Error::InvalidData(_))
                ));
            }
        }
        for (w, h) in [(0, 1), (1, 0), (16384, 1), (1, 16384)] {
            assert!(webp_encode(Vec::new(), &ChunkInfo { w, h, spp: 3, t: DataType::U8 }, &CodecParams::default()).is_err());
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
        let bytes = encode(
            &rgb,
            &TiffOptions {
                compression: Compression::Jpeg,
                photometric: Some(6),
                codec: CodecParams { jpeg_ycbcr: true, ..Default::default() },
                ..Default::default()
            },
        )
        .unwrap();
        let jpeg = crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap().header();
        assert_eq!((1..=3).map(|b| jpeg.band_color_interp(b)).collect::<Vec<_>>(), ["Red", "Green", "Blue"]);

        let r = layouts_raster(4, DataType::U8);
        let opts = TiffOptions {
            photometric: Some(2),
            alpha: true,
            band_metadata: vec![crate::BandMetadata { color_interp: Some("Undefined".into()), ..Default::default() }],
            ..Default::default()
        };
        let mut lazy = crate::LazyRaster::from_tiff_bytes(encode(&r, &opts).unwrap()).unwrap().unwrap();
        let header = lazy.header();
        assert!(header.raster.data.is_empty());
        assert_eq!((1..=4).map(|b| header.band_color_interp(b)).collect::<Vec<_>>(), ["Undefined", "Green", "Blue", "Alpha"]);
        let subset =
            lazy.read_window(crate::Window::new(1, 1, 2, 2), &crate::WindowOptions { bands: vec![4, 2, 1], ..Default::default() }).unwrap();
        assert_eq!((1..=3).map(|b| subset.band_color_interp(b)).collect::<Vec<_>>(), ["Alpha", "Green", "Undefined"]);
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

    /// Streamed mask values are read as the whole writer reads them: any nonzero value is valid
    /// and weighted as 255, so mixed 1 and 255 valid values give the whole writer's overviews.
    #[test]
    fn streamed_mask_values_are_normalized_as_the_whole_writer_normalizes_them() {
        let dir = tempfile::tempdir().unwrap();
        let (cols, rows) = (131, 259);
        let data = RasterData::U8((0..cols * rows).map(|i| (i * 7 % 251) as u8).collect());
        let mask: Vec<u8> = (0..cols * rows).map(|i| if i % cols < 31 || i % 13 < 5 { 0 } else if i % 3 == 0 { 255 } else { 1 }).collect();
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
}

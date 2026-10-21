//! A GeoTIFF and Cloud Optimized GeoTIFF writer.
//!
//! wbraster's writer lays COGs out in the wrong order and skips overview levels, so terrio writes
//! TIFFs itself: strips or tiles, pixel-interleaved, with the compressions GDAL reads by default,
//! GeoKeys for EPSG and user-defined CRSs, `GDAL_NODATA` and `GDAL_METADATA`. COGs follow GDAL's
//! layout: the IFDs first, then the image data from the smallest overview to the full resolution,
//! with GDAL's structural metadata, block leaders and trailers.

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
}

impl Default for CodecParams {
    fn default() -> Self {
        Self { max_z_error: 0.0, jpeg_quality: 75, jpeg_ycbcr: false }
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
            _ => return None,
        })
    }

    fn code(self) -> u16 {
        match self {
            Self::None => 1,
            Self::Lzw => 5,
            Self::Deflate => 8,
            Self::PackBits => 32773,
            Self::Zstd => 50000,
            Self::Lerc | Self::LercDeflate | Self::LercZstd => 34887,
            Self::Jxl => 50002,
            Self::Jpeg => 7,
        }
    }

    /// Whether the compression takes the TIFF predictor.
    fn takes_predictor(self) -> bool {
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

#[cfg(not(target_arch = "wasm32"))]
fn zstd_compress(data: &[u8]) -> Result<Vec<u8>> {
    Ok(zstd::bulk::compress(data, 9)?)
}

/// WebAssembly builds compress with the pure-Rust encoder (its fastest level).
#[cfg(target_arch = "wasm32")]
fn zstd_compress(data: &[u8]) -> Result<Vec<u8>> {
    Ok(ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest))
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
    /// Overview resampling: nearest neighbour instead of averaging.
    pub nearest_overviews: bool,
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
    /// The 92 values of the `RPCCoefficientTag`, or none.
    pub rpc: Vec<f64>,
    /// The parameters of LERC and JPEG.
    pub codec: CodecParams,
    /// The raster's bands are pairs of real and imaginary parts (a complex dataset): written as
    /// complex samples (`SampleFormat` 5 or 6, GDAL's `CInt16`... `CFloat64`).
    pub complex: bool,
}

impl Default for TiffOptions {
    fn default() -> Self {
        Self {
            compression: Compression::None,
            tile_size: None,
            bigtiff: false,
            cog: false,
            nodata: None,
            nearest_overviews: false,
            block: None,
            rows_per_strip: None,
            planar: false,
            photometric: None,
            alpha: false,
            band_metadata: Vec::new(),
            color_table: Vec::new(),
            overviews: Vec::new(),
            overview_resampling: OverviewResampling::Nearest,
            georeferenced: true,
            predictor: 1,
            geo_transform: None,
            gcps: Vec::new(),
            mask: None,
            rpc: Vec::new(),
            codec: CodecParams::default(),
            complex: false,
        }
    }
}

/// How the overviews given as factors are computed, as GDAL does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverviewResampling {
    Nearest,
    Average,
    Min,
    Max,
    Mode,
}

impl OverviewResampling {
    fn parse(s: &str) -> Result<Self> {
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
        if let Some(v) = co("JPEG_QUALITY").or_else(|| co("QUALITY")) {
            o.codec.jpeg_quality = v
                .trim()
                .parse::<u8>()
                .ok()
                .filter(|q| (1..=100).contains(q))
                .ok_or_else(|| Error::InvalidArgument(format!("invalid JPEG_QUALITY '{v}' (1 to 100)")))?;
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
enum Value {
    Shorts(Vec<u16>),
    /// Offsets or counts: LONG in classic TIFF, LONG8 in BigTIFF.
    Offsets(Vec<u64>),
    Longs(Vec<u32>),
    Doubles(Vec<f64>),
    Ascii(String),
}

impl Value {
    fn encode(&self, big: bool) -> (u16, u64, Vec<u8>) {
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
struct Image {
    tags: Vec<(u16, Value)>,
    chunks: Vec<Vec<u8>>,
    tiled: bool,
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
    if jpeg && (raster.data_type != DataType::U8 || !matches!(samples, 1 | 3) || opts.planar || !opts.color_table.is_empty()) {
        return Err(Error::Unsupported("JPEG is written for 1 or 3 Byte bands, pixel-interleaved".into()));
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
        None if samples == 3 && raster.data_type == DataType::U8 => 2,
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
    let layout =
        Layout { tile, rows_per_strip: opts.rows_per_strip, planar: opts.planar && samples > 1, photometric, extra, predictor, parts };
    let mut geo = geo_tags(raster, opts.nodata, &opts.band_metadata)?;
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
    if !opts.rpc.is_empty() {
        if opts.rpc.len() != 92 {
            return Err(Error::InvalidArgument(format!("RPCs are 92 coefficients, not {}", opts.rpc.len())));
        }
        geo.push((50844, Value::Doubles(opts.rpc.clone())));
    }
    let color_map = (photometric == 3).then(|| color_map_tag(&opts.color_table, raster.data_type));
    Ok(Plan { tile, layout, geo, color_map })
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
    if let (true, Some((t, _))) = (opts.cog, tile) {
        let t = t as usize;
        while levels.last().is_some_and(|l| l.cols > t || l.rows > t) {
            let next = levels[levels.len() - 1].downsample(opts.nodata, opts.nearest_overviews);
            levels.push(next);
        }
    } else if !opts.cog {
        let mut factors: Vec<usize> = opts.overviews.iter().copied().filter(|f| *f > 1).collect();
        factors.sort_unstable();
        factors.dedup();
        for f in factors {
            let (w, h) = (raster.cols.div_ceil(f), raster.rows.div_ceil(f));
            // As GDAL does, each overview is computed from the previous (larger) one.
            let src = levels.last().filter(|l| l.cols > w).unwrap_or(&levels[0]);
            let next = src.resample(w, h, opts.overview_resampling, opts.nodata);
            levels.push(next);
        }
    }
    let mut images = Vec::with_capacity(levels.len());
    for (i, level) in levels.iter().enumerate() {
        let mut img = level.image(raster.data_type, opts.compression, &opts.codec, &layout)?;
        if i > 0 {
            img.tags.push((254, Value::Longs(vec![1])));
        }
        // Georeferencing on the full-resolution image; nodata on every image, as GDAL does.
        for (tag, v) in &geo {
            if i == 0 || *tag == 42113 {
                img.tags.push((*tag, v.clone()));
            }
        }
        if let Some(cm) = &color_map {
            img.tags.push((320, cm.clone()));
        }
        img.tags.sort_by_key(|(t, _)| *t);
        images.push(img);
        // GDAL's internal mask follows the full-resolution image.
        if let (0, Some(m)) = (i, &opts.mask) {
            let main = &images[0];
            let tag = |t: u16| {
                main.tags.iter().find_map(|(k, v)| match v {
                    Value::Longs(v) if *k == t => v.first().map(|x| *x as usize),
                    _ => None,
                })
            };
            let block = match (tag(322), tag(323), tag(278)) {
                (Some(w), Some(h), _) => (w, h, true),
                (_, _, Some(rows)) => (raster.cols, rows, false),
                _ => (raster.cols, raster.rows, false),
            };
            images.push(mask_image(m, raster.cols, raster.rows, block)?);
        }
    }

    let data_len: u64 = images.iter().flat_map(|i| i.chunks.iter()).map(|c| c.len() as u64 + 8).sum();
    let big = opts.bigtiff || data_len > (u32::MAX as u64) - (1 << 24);
    Ok(Prepared { images, big, cog: opts.cog })
}

/// GDAL's internal mask (`GDAL_TIFF_INTERNAL_MASK`): a 1-bit transparency mask image
/// (`NewSubfileType` 4, `PhotometricInterpretation` 4) in the blocks of the image it masks
/// (`block`: width, height, tiled), Deflate compressed, bits set for valid pixels.
fn mask_image(mask: &[u8], cols: usize, rows: usize, block: (usize, usize, bool)) -> Result<Image> {
    let compression = Compression::Deflate;
    let (chunk_w, chunk_h, tiled) = block;
    let (across, down) = (cols.div_ceil(chunk_w), rows.div_ceil(chunk_h));
    let row_bytes = chunk_w.div_ceil(8);
    let chunks = crate::par::map_range(across * down, |j| {
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
struct Layout {
    /// Tile width and height; strips when `None`.
    tile: Option<(u32, u32)>,
    rows_per_strip: Option<u32>,
    /// One plane per band.
    planar: bool,
    photometric: u16,
    /// The `ExtraSamples` values of the samples besides the color ones.
    extra: Vec<u16>,
    /// The TIFF predictor.
    predictor: u16,
    /// The bands per TIFF sample: 2 for complex samples (real and imaginary parts), else 1.
    parts: usize,
}

/// One resolution level's pixels, band-major: the raster's own, or an overview's.
struct Level<'a> {
    cols: usize,
    rows: usize,
    bands: usize,
    data: std::borrow::Cow<'a, RasterData>,
}

impl<'a> Level<'a> {
    fn from_raster(r: &'a Raster) -> Self {
        Self { cols: r.cols, rows: r.rows, bands: r.bands, data: std::borrow::Cow::Borrowed(&r.data) }
    }

    /// Halves the resolution: the average (or the top-left value) of each 2x2 block of valid
    /// pixels.
    fn downsample(&self, nodata: Option<f64>, nearest: bool) -> Level<'static> {
        let (cols, rows) = (self.cols.div_ceil(2), self.rows.div_ceil(2));
        let data = map_data!(self.data.as_ref(), v => downsample(v, self.cols, self.rows, cols, rows, self.bands, nodata, nearest));
        Level { cols, rows, bands: self.bands, data: std::borrow::Cow::Owned(data) }
    }

    /// Resamples to `cols` x `rows` pixels, as GDAL computes overviews.
    fn resample(&self, cols: usize, rows: usize, method: OverviewResampling, nodata: Option<f64>) -> Level<'static> {
        let src = (self.cols, self.rows, self.bands);
        // GDAL's mode kernel works on 16, 32 and 64-bit signed data as unsigned: a negative nodata
        // value is out of its range, and empty blocks get 0.
        let signed = matches!(self.data.data_type(), DataType::I16 | DataType::I32 | DataType::I64);
        let fill =
            if method == OverviewResampling::Mode && signed && nodata.is_some_and(|n| n < 0.0) { 0.0 } else { nodata.unwrap_or(0.0) };
        let data = map_data!(self.data.as_ref(), v => resample(v, src, cols, rows, method, nodata, fill));
        Level { cols, rows, bands: self.bands, data: std::borrow::Cow::Owned(data) }
    }

    /// Splits the level into compressed strips or tiles, with its structural tags.
    fn image(&self, t: DataType, compression: Compression, codec: &CodecParams, layout: &Layout) -> Result<Image> {
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
        let chunks = crate::par::map_range(planes * per_plane, |i| {
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

/// Resamples a band-major image to `out_cols` x `out_rows`, as GDAL computes overviews: nearest
/// neighbour picks the pixel at `0.5 + i * ratio`; the other methods combine the valid pixels of
/// the source window of each output pixel (an average weighted by coverage).
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
    crate::par::chunks_mut(&mut out, out_cols, |k, line| {
        let (b, r) = (k / out_rows, k % out_rows);
        let band = &src[b * cols * rows..(b + 1) * cols * rows];
        if method == OverviewResampling::Nearest {
            let sy = ((0.5 + r as f64 * ry) as usize).min(rows - 1);
            for (c, o) in line.iter_mut().enumerate() {
                let sx = ((0.5 + c as f64 * rx) as usize).min(cols - 1);
                *o = band[sy * cols + sx];
            }
            return;
        }
        let (y0, y1, wy0, wy1) = window(r, ry, rows);
        let mut values: Vec<f64> = Vec::new();
        for (c, o) in line.iter_mut().enumerate() {
            let (x0, x1, wx0, wx1) = xs[c];
            let (mut sum, mut weight) = (0.0, 0.0);
            values.clear();
            for y in y0..y1 {
                let wy = if y == y0 {
                    wy0
                } else if y + 1 == y1 {
                    wy1
                } else {
                    1.0
                };
                for x in x0..x1 {
                    let v = band[y * cols + x].to_f64();
                    if v.is_nan() || nodata.is_some_and(|nd| v == nd) {
                        continue;
                    }
                    let wx = if x == x0 {
                        wx0
                    } else if x + 1 == x1 {
                        wx1
                    } else {
                        1.0
                    };
                    sum += v * wx * wy;
                    weight += wx * wy;
                    values.push(v);
                }
            }
            let v = if values.is_empty() {
                fill
            } else {
                match method {
                    OverviewResampling::Average => sum / weight,
                    OverviewResampling::Min => values.iter().copied().fold(f64::INFINITY, f64::min),
                    OverviewResampling::Max => values.iter().copied().fold(f64::NEG_INFINITY, f64::max),
                    _ => mode(&values),
                }
            };
            *o = T::from_f64(v);
        }
    });
    out
}

/// The most frequent value, as GDAL counts: the first value to reach the highest count.
fn mode(values: &[f64]) -> f64 {
    let mut seen: Vec<(f64, usize)> = Vec::new();
    let mut best = 0;
    for v in values {
        match seen.iter().position(|(w, _)| w == v) {
            Some(i) => {
                seen[i].1 += 1;
                if seen[i].1 > seen[best].1 {
                    best = i;
                }
            }
            None => seen.push((*v, 1)),
        }
    }
    seen[best].0
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

/// Halves a band-major image: each output pixel is the average of the valid pixels of its 2x2
/// block (or the block's top-left pixel), computed in `f64` and converted as the writer converts.
#[allow(clippy::too_many_arguments)]
fn downsample<T: Pixel>(
    src: &[T],
    cols: usize,
    rows: usize,
    out_cols: usize,
    out_rows: usize,
    bands: usize,
    nodata: Option<f64>,
    nearest: bool,
) -> Vec<T> {
    let fill = nodata.unwrap_or(0.0);
    let mut out = vec![T::default(); out_cols * out_rows * bands];
    crate::par::chunks_mut(&mut out, out_cols, |i, line| {
        let (b, r) = (i / out_rows, i % out_rows);
        let band = &src[b * cols * rows..(b + 1) * cols * rows];
        let get = |r: usize, c: usize| band[r * cols + c].to_f64();
        for (c, o) in line.iter_mut().enumerate() {
            let v = if nearest {
                get(2 * r, 2 * c)
            } else {
                let (mut sum, mut n) = (0.0, 0u32);
                for (rr, cc) in [(2 * r, 2 * c), (2 * r, 2 * c + 1), (2 * r + 1, 2 * c), (2 * r + 1, 2 * c + 1)] {
                    if rr >= rows || cc >= cols {
                        continue;
                    }
                    let v = get(rr, cc);
                    if v.is_nan() || nodata.is_some_and(|nd| v == nd) {
                        continue;
                    }
                    sum += v;
                    n += 1;
                }
                if n == 0 { fill } else { sum / n as f64 }
            };
            *o = T::from_f64(v);
        }
    });
    out
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

/// The georeferencing tags: pixel scale and tie point, GeoKeys, nodata and metadata.
fn geo_tags(r: &Raster, nodata: Option<f64>, bands: &[crate::BandMetadata]) -> Result<Vec<(u16, Value)>> {
    let mut tags = vec![
        (33550, Value::Doubles(vec![r.cell_size_x, r.cell_size_y, 0.0])),
        (33922, Value::Doubles(vec![0.0, 0.0, 0.0, r.x_min, r.y_max(), 0.0])),
    ];
    let crs = super::common::spatial_ref(&r.crs);
    let mut v = crate::drivers::geokeys::GeoKeyValues::default();
    v.shorts.push((1025, 1)); // RasterPixelIsArea
    let local = crs.wkt.as_deref().and_then(local_cs);
    if let Some((name, unit)) = &local {
        // A local CRS, as GDAL writes one: a citation and the linear unit, no model type.
        v.ascii.push((1026, name.clone()));
        v.shorts.push((3076, *unit));
    } else if !crs.is_unknown() {
        let geographic = crs.is_geographic();
        match crs.epsg.and_then(|c| u16::try_from(c).ok()) {
            Some(code) => {
                v.shorts.push((1024, if geographic { 2 } else { 1 }));
                v.shorts.push((if geographic { 2048 } else { 3072 }, code));
            }
            None => {
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
    let mut dir = vec![1u16, 1, 0, keys.len() as u16];
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
    if let Some(xml) = gdal_metadata_xml(&r.metadata, bands) {
        tags.push((42112, Value::Ascii(xml)));
    }
    if let Some(nd) = nodata {
        tags.push((42113, Value::Ascii(format_nodata(nd))));
    }
    Ok(tags)
}

/// The `GDAL_METADATA` document of dataset items and band metadata, as GDAL writes it (values
/// escaped twice). `None` when there is nothing to write.
fn gdal_metadata_xml(dataset: &[(String, String)], bands: &[crate::BandMetadata]) -> Option<String> {
    let mut xml = String::from("<GDALMetadata>\n");
    let mut any = false;
    let mut item = |name: &str, value: &str, sample: Option<usize>, role: Option<&str>| {
        let mut attrs = format!("name=\"{}\"", xml_escape(name));
        if let Some(s) = sample {
            attrs.push_str(&format!(" sample=\"{s}\""));
        }
        if let Some(r) = role {
            attrs.push_str(&format!(" role=\"{r}\""));
        }
        xml.push_str(&format!("  <Item {attrs}>{}</Item>\n", xml_escape(&xml_escape(value))));
        any = true;
    };
    for (k, v) in dataset {
        item(k, v, None, None);
    }
    for (i, b) in bands.iter().enumerate() {
        if let Some(v) = b.offset {
            item("OFFSET", &format!("{v}"), Some(i), Some("offset"));
        }
        if let Some(v) = b.scale {
            item("SCALE", &format!("{v}"), Some(i), Some("scale"));
        }
        if let Some(v) = &b.unit {
            item("UNITTYPE", v, Some(i), Some("unittype"));
        }
        if let Some(v) = &b.description {
            item("DESCRIPTION", v, Some(i), Some("description"));
        }
        for (k, v) in &b.items {
            item(k, v, Some(i), None);
        }
        if let Some(v) = &b.color_interp {
            item("COLORINTERP", v, Some(i), Some("colorinterp"));
        }
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

/// The dataset-level items of a `GDAL_METADATA` XML document (band items are skipped).
pub(crate) fn parse_gdal_metadata(xml: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for item in xml.split("<Item ").skip(1) {
        let Some((attrs, rest)) = item.split_once('>') else { continue };
        if attrs.contains("sample=") || attrs.contains("role=") || attrs.contains("domain=") {
            continue;
        }
        let Some(name) = attrs.split("name=\"").nth(1).and_then(|s| s.split('"').next()) else { continue };
        let Some(value) = rest.split("</Item>").next() else { continue };
        out.push((xml_unescape(name), xml_unescape(&xml_unescape(value))));
    }
    out
}

/// The band items of a `GDAL_METADATA` tag (default domain, no role): `(band from 0, name, value)`.
pub(crate) fn parse_gdal_band_metadata(xml: &str) -> Vec<(usize, String, String)> {
    let mut out = Vec::new();
    for item in xml.split("<Item ").skip(1) {
        let Some((attrs, rest)) = item.split_once('>') else { continue };
        if attrs.contains("role=") || attrs.contains("domain=") {
            continue;
        }
        let attr = |key: &str| attrs.split(&format!("{key}=\"")).nth(1).and_then(|s| s.split('"').next());
        let (Some(name), Some(sample)) = (attr("name"), attr("sample").and_then(|s| s.parse().ok())) else { continue };
        let Some(value) = rest.split("</Item>").next() else { continue };
        out.push((sample, xml_unescape(name), xml_unescape(&xml_unescape(value))));
    }
    out
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

fn xml_escape(s: &str) -> String {
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
        f.debug_struct("GeoTiffWriter")
            .field("cols", &self.cols)
            .field("rows", &self.rows)
            .field("bands", &self.bands)
            .finish_non_exhaustive()
    }
}

impl GeoTiffWriter {
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
        let mut options = options.clone();
        if options.geo_transform.is_none() {
            options.geo_transform = header.affine;
        }
        let with_mask = options.mask.take().is_some();
        let opts = TiffOptions::from_write_options(&options, false, header.nodata())?.ok_or_else(|| {
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
#[cfg(not(target_arch = "wasm32"))]
type ChunkSpool = std::fs::File;
#[cfg(target_arch = "wasm32")]
type ChunkSpool = std::io::Cursor<Vec<u8>>;

fn chunk_spool() -> Result<ChunkSpool> {
    #[cfg(not(target_arch = "wasm32"))]
    return Ok(tempfile::tempfile()?);
    #[cfg(target_arch = "wasm32")]
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
    /// The last row received, when the next level waits for the row it is averaged with.
    carry: Option<RasterData>,
    /// The compressed chunks of each plane, and their sizes.
    spools: Vec<ChunkSpool>,
    sizes: Vec<Vec<u64>>,
    /// The structural tags, from the first row of tiles.
    tags: Vec<(u16, Value)>,
}

/// A COG written a few rows at a time, from the top, so that the raster is never held whole. Each
/// level (the full resolution, then the overviews down to one that fits in a tile) compresses its
/// rows of tiles as they are complete, into a temporary file, and halves its rows a pair at a time
/// into the next level, as [`write`] computes the overviews from the whole raster. Once the last
/// row is in, the file is laid out as [`write`] lays it out: the IFDs, then the levels' tiles from
/// the smallest overview, copied from the temporary files. The file is byte for byte the same.
pub(crate) struct CogWriter {
    file: std::io::BufWriter<std::fs::File>,
    bands: usize,
    data_type: DataType,
    encoding: std::sync::Arc<(Compression, CodecParams, Layout)>,
    tile: usize,
    nodata: Option<f64>,
    nearest: bool,
    bigtiff: bool,
    /// The full-resolution image's georeferencing and metadata tags (nodata on every level).
    geo: Vec<(u16, Value)>,
    color_map: Option<Value>,
    levels: Vec<CogLevel>,
    /// Rows of tiles being compressed, in the order they were given: their level, number of
    /// rows, and the compressed image to come.
    #[cfg(not(target_arch = "wasm32"))]
    in_flight: std::collections::VecDeque<(usize, usize, std::sync::mpsc::Receiver<Result<Image>>)>,
}

/// The most rows of tiles a [`CogWriter`] compresses at once, so that the tiles of the next rows
/// keep the threads busy while the last tiles of the first are compressed.
#[cfg(not(target_arch = "wasm32"))]
const COG_IN_FLIGHT: usize = 3;

impl CogWriter {
    /// Creates the file, for a raster described by `raster` (its pixels are not read). `opts` are
    /// a COG's, without a mask.
    pub(crate) fn create(path: &str, raster: &Raster, opts: &TiffOptions) -> Result<Self> {
        if !opts.cog || opts.mask.is_some() {
            return Err(Error::Unsupported("COGs are written in rows without a mask only".into()));
        }
        let Plan { tile, layout, geo, color_map } = plan(raster, opts)?;
        let tile = tile.map_or(512, |(t, _)| t as usize);
        let planes = if layout.planar { raster.bands / layout.parts } else { 1 };
        let mut sizes = vec![(raster.cols, raster.rows)];
        while let Some(&(c, r)) = sizes.last().filter(|(c, r)| *c > tile || *r > tile) {
            sizes.push((c.div_ceil(2), r.div_ceil(2)));
        }
        let levels = sizes
            .into_iter()
            .map(|(cols, rows)| {
                Ok(CogLevel {
                    cols,
                    rows,
                    received: 0,
                    pending: RasterData::new_filled(raster.data_type, 0, 0.0),
                    pending_rows: 0,
                    carry: None,
                    spools: (0..planes).map(|_| chunk_spool()).collect::<Result<_>>()?,
                    sizes: vec![Vec::new(); planes],
                    tags: Vec::new(),
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
            nearest: opts.nearest_overviews,
            bigtiff: opts.bigtiff,
            geo,
            color_map,
            levels,
            #[cfg(not(target_arch = "wasm32"))]
            in_flight: Default::default(),
        })
    }

    /// Appends the next `rows` full rows of every band (`data`: band after band, of the raster's
    /// data type).
    pub(crate) fn write_rows(&mut self, data: RasterData, rows: usize) -> Result<()> {
        let l = &self.levels[0];
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
        if rows > 0 { self.push(0, data, rows) } else { Ok(()) }
    }

    /// Adds `rows` rows to level `i`: halved into the next level, and compressed by rows of tiles.
    fn push(&mut self, i: usize, data: RasterData, rows: usize) -> Result<()> {
        let bands = self.bands;
        let (cols, done) = {
            let l = &mut self.levels[i];
            l.received += rows;
            (l.cols, l.received == l.rows)
        };
        if i + 1 < self.levels.len() {
            // Pairs of rows, and the level's last row alone.
            let carry = self.levels[i].carry.take();
            let joined = carry.as_ref().map(|c| append_rows(c, 1, &data, rows, cols, bands)).transpose()?;
            let (src, n) = match &joined {
                Some(j) => (j, rows + 1),
                None => (&data, rows),
            };
            let even = if done { n } else { n / 2 * 2 };
            if even < n {
                self.levels[i].carry = Some(slice_rows(src, n, even..n, cols, bands));
            }
            if even > 0 {
                let cut;
                let src = if even < n {
                    cut = slice_rows(src, n, 0..even, cols, bands);
                    &cut
                } else {
                    src
                };
                let (half_cols, half_rows) = (cols.div_ceil(2), even.div_ceil(2));
                let (nodata, nearest) = (self.nodata, self.nearest);
                let half = map_data!(src, v => downsample(v, cols, even, half_cols, half_rows, bands, nodata, nearest));
                self.push(i + 1, half, half_rows)?;
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
            self.compress(i, slab, ready)?;
        }
        Ok(())
    }

    /// Compresses rows of tiles of level `i` (in parallel, on the thread pool, while the next rows
    /// are given), to be added to its spools in turn.
    fn compress(&mut self, i: usize, data: RasterData, rows: usize) -> Result<()> {
        let level = Level { cols: self.levels[i].cols, rows, bands: self.bands, data: std::borrow::Cow::Owned(data) };
        let (data_type, encoding) = (self.data_type, self.encoding.clone());
        let encode = move || {
            let (compression, codec, layout) = &*encoding;
            level.image(data_type, *compression, codec, layout)
        };
        #[cfg(target_arch = "wasm32")]
        return self.add(i, rows, encode()?);
        #[cfg(not(target_arch = "wasm32"))]
        {
            let (tx, rx) = std::sync::mpsc::sync_channel(1);
            rayon::spawn(move || {
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
    #[cfg(not(target_arch = "wasm32"))]
    fn land(&mut self) -> Result<()> {
        let Some((i, rows, rx)) = self.in_flight.pop_front() else { return Ok(()) };
        let img = rx.recv().map_err(|_| Error::InvalidData("a COG tile compression stopped".into()))??;
        self.add(i, rows, img)
    }

    /// Adds compressed rows of tiles of level `i` to its spools.
    fn add(&mut self, i: usize, rows: usize, img: Image) -> Result<()> {
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
        #[cfg(not(target_arch = "wasm32"))]
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
            // Georeferencing on the full-resolution image; nodata on every image, as GDAL does.
            tags.extend(self.geo.iter().filter(|(t, _)| i == 0 || *t == 42113).cloned());
            if let Some(cm) = &self.color_map {
                tags.push((320, cm.clone()));
            }
            tags.sort_by_key(|(t, _)| *t);
            dirs.push(Directory { tags, sizes: l.sizes.concat(), tiled: true });
        }
        let data_len: u64 = dirs.iter().flat_map(|d| &d.sizes).map(|s| s + 8).sum();
        let big = self.bigtiff || data_len > (u32::MAX as u64) - (1 << 24);
        self.file.write_all(&directories(dirs, big, true))?;
        use std::io::{Seek, SeekFrom};
        for i in data_order(self.levels.len(), true) {
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

    /// COGs written a few rows at a time are those written from the whole raster, byte for
    /// byte: odd sizes, several overview levels, interleaved and planar bands, rows given one at
    /// a time or in uneven runs.
    #[test]
    fn cogs_written_in_rows_are_those_written_whole() {
        let dir = tempfile::tempdir().unwrap();
        for (cols, rows, bands, t, planar, nearest, compression) in [
            (70, 50, 1, DataType::F32, false, false, Compression::Deflate),
            (157, 133, 3, DataType::U8, false, false, Compression::Lzw),
            (157, 133, 2, DataType::I16, true, false, Compression::Zstd),
            (33, 129, 1, DataType::U8, false, true, Compression::None),
            (16, 16, 1, DataType::F32, false, false, Compression::Deflate),
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
            let data = match t {
                DataType::F32 => RasterData::F32((0..n).map(|i| if i % 37 == 0 { -1.0 } else { (i % 977) as f32 * 0.25 }).collect()),
                DataType::U8 => RasterData::U8((0..n).map(|i| (i * 7 % 251) as u8).collect()),
                _ => RasterData::I16((0..n).map(|i| (i % 1000) as i16 - 1).collect()),
            };
            let r = Raster::from_data_native(cfg, data.clone()).unwrap();
            let opts = TiffOptions {
                compression,
                cog: true,
                tile_size: Some(16),
                nodata: Some(-1.0),
                planar,
                nearest_overviews: nearest,
                ..Default::default()
            };
            let whole = encode(&r, &opts).unwrap();
            for step in [1, 7, 16, 40, rows] {
                let p = dir.path().join("rows.tif");
                let mut w = CogWriter::create(p.to_str().unwrap(), &r, &opts).unwrap();
                let mut y = 0;
                while y < rows {
                    let h = step.min(rows - y);
                    w.write_rows(slice_rows(&data, rows, y..y + h, cols, bands), h).unwrap();
                    y += h;
                }
                w.finish().unwrap();
                assert!(std::fs::read(&p).unwrap() == whole, "{cols}x{rows}x{bands} {t:?} planar {planar}, rows by {step}");
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
    fn writes_band_metadata_color_tables_and_overviews() {
        let r = layouts_raster(1, DataType::U8);
        let options = crate::RasterWriteOptions {
            band_metadata: vec![crate::BandMetadata {
                description: Some("a<b".into()),
                scale: Some(2.0),
                items: vec![("k".into(), "v".into())],
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
        assert!(text.contains(r#"<Item name="k" sample="0">v</Item>"#));
        let tags = crate::geotiff::read_geo_tags_from(std::io::Cursor::new(&bytes)).unwrap();
        assert_eq!(tags.geo_transform, None);
        assert_eq!(tags.band_metadata, vec![(0, "k".to_string(), "v".to_string())]);
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

    #[test]
    fn mode_counts_as_gdal() {
        // The first value to reach the highest count wins.
        assert_eq!(mode(&[1.0, 2.0, 2.0, 1.0]), 2.0);
        assert_eq!(mode(&[3.0, 1.0, 2.0]), 3.0);
        assert_eq!(mode(&[1.0, 2.0, 1.0, 2.0, 2.0]), 2.0);
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

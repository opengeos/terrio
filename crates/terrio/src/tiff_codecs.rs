// SPDX-FileCopyrightText: lerc-rust contributors
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   lerc-rust (lerc-reader) 0.5.0:
//     lerc-reader/src/lerc1.rs, lerc2.rs (header parsing)

//! TIFF compressions the `tiff` crate does not decode, read as GDAL (libtiff) reads them: LERC
//! (alone, or wrapped in Deflate or Zstandard), JPEG XL and LZMA, and Zstandard in every build
//! (the crate is built without its Zstandard, zstd 0.13), Float16 samples (read as Float32, as GDAL's
//! Float16) and WhiteIsZero ones (as stored) included. And JPEG, decoded as
//! libjpeg decodes it for GDAL ([`crate::jpeg`]), YCbCr converted to RGB. And unsigned samples
//! packed in 9 to 31 bits (GDAL's `NBITS`, as written for Sentinel-2's 15-bit bands), in any of the
//! lossless compressions, read as UInt16 or UInt32 as GDAL reads them; and samples of 1 to 7 bits
//! (bilevel images, GDAL's `NBITS=1`, `2` or `4`), a byte per sample as GDAL's Byte bands hold
//! them, their values as stored (WhiteIsZero ones not inverted, as libtiff leaves them). And
//! pixel-interleaved RGB or CMYK with samples besides the color ones and an alpha (NAIP's
//! near-infrared band, as `gdalwarp` and public COG archives store it), which the crate
//! reads with too few samples per pixel, in any lossless compression or WebP. And WhiteIsZero
//! samples of 8 bits or more uncompressed, LZW, Deflate or PackBits, which the crate inverts (or
//! refuses): read as stored, as under every other compression, as libtiff gives them to GDAL.

use crate::geotiff::ChunkTables;
use std::io::{Read, Seek, SeekFrom};
use std::sync::Arc;
use tiff::decoder::{ChunkType, Decoder, DecodingResult};
use tiff::tags::Tag;
use tiff::{TiffError, TiffResult};

use crate::dataset::fallible;

/// The TIFF compression codes handled here.
const LERC: u16 = 34887;
const LZMA: u16 = 34925;
const ZSTD: u16 = 50000;
/// JPEG XL, as GDAL first wrote it, then as DNG 1.7 registered it.
const JXL: u16 = 50002;
const JXL_DNG: u16 = 52546;
const WEBP: u16 = 50001;
/// The tag of LERC's parameters: its version and the compression wrapping it.
const LERC_PARAMETERS: u16 = 50674;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Lerc,
    Jxl,
    Lzma,
    /// Zstandard, with the zstd C library natively with the `libzstd` feature, with `ruzstd`
    /// otherwise ([`crate::zstandard::frames`]).
    Zstd,
    /// LZW of 8 to 64-bit samples, decoded whole: the `tiff` crate's reader fails on some chunks
    /// it reads row by row (edge tiles narrower than the tile).
    Lzw,
    Jpeg,
    /// WebP decoded with the TIFF sample count, including unspecified fourth bands.
    WebP,
    /// Unsigned samples of 9 to 31 bits (not 16), or of 1 to 7 bits, packed MSB first with each
    /// row starting on a byte, under compression `.0`.
    Packed(u16),
    /// Samples of 1 to 7 bits under a compression only the `tiff` crate decodes (CCITT): the
    /// crate's rows of packed samples, unpacked here.
    CratePacked,
    /// Pixel-interleaved RGB or CMYK with more samples than the `tiff` crate's color type holds
    /// (unspecified extra samples, as `gdalwarp` writes NAIP's near-infrared band), under the
    /// lossless compression `.0`: the crate reads such chunks with the color type's samples, which
    /// scrambles every band.
    Chunky(u16),
    /// Complex samples (`SampleFormat` 5 or 6), which the `tiff` crate does not read, under the
    /// lossless compression `.0`: each sample is read as its real and imaginary parts, two
    /// samples of their type, as libtiff byte-swaps them.
    Complex(u16),
    /// WhiteIsZero samples (`PhotometricInterpretation` 0) of 8 bits or more under the lossless
    /// compression `.0`, which the `tiff` crate inverts (unsigned and floating-point ones, one
    /// sample per pixel) or refuses (the others): read as stored, as libtiff gives them to GDAL.
    WhiteIsZero(u16),
}

/// A sample type of the TIFF image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sample {
    U8,
    U16,
    U32,
    U64,
    I8,
    I16,
    I32,
    I64,
    /// IEEE half-precision floats, under Zstandard only (`Sample::new` leaves them to the `tiff`
    /// crate).
    F16,
    F32,
    F64,
}

impl Sample {
    /// The sample type of a `SampleFormat` (1 unsigned, 2 signed, 3 float) and bit depth.
    fn new(format: u16, bits: u16) -> Option<Self> {
        Some(match (format, bits) {
            (1 | 4, 8) => Self::U8,
            (1 | 4, 16) => Self::U16,
            (1 | 4, 32) => Self::U32,
            (1 | 4, 64) => Self::U64,
            (2, 8) => Self::I8,
            (2, 16) => Self::I16,
            (2, 32) => Self::I32,
            (2, 64) => Self::I64,
            (3, 32) => Self::F32,
            (3, 64) => Self::F64,
            _ => return None,
        })
    }

    /// The type of the real and imaginary parts of complex samples (`SampleFormat` 5 for
    /// integers, 6 for floats) of a bit depth: GDAL's `CInt16`, `CInt32`, `CFloat32` and
    /// `CFloat64`.
    fn complex_part(format: u16, bits: u16) -> Option<Self> {
        Some(match (format, bits) {
            (5, 32) => Self::I16,
            (5, 64) => Self::I32,
            (6, 64) => Self::F32,
            (6, 128) => Self::F64,
            _ => return None,
        })
    }

    fn size(self) -> usize {
        match self {
            Self::U8 | Self::I8 => 1,
            Self::U16 | Self::I16 | Self::F16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::U64 | Self::I64 | Self::F64 => 8,
        }
    }

    fn is_float(self) -> bool {
        matches!(self, Self::F16 | Self::F32 | Self::F64)
    }
}

/// Decompresses a chunk coded with one of TIFF's lossless compressions, into at most `limit`
/// bytes: a stream that would expand further is cut there (it cannot be a valid chunk), and one
/// that ends short is an error rather than missing pixels.
fn decompress(compression: u16, raw: Vec<u8>, limit: usize) -> TiffResult<Vec<u8>> {
    let mut out = Vec::new();
    // Reads at most `limit` bytes of a decompressing reader.
    let bounded = |r: &mut dyn Read, out: &mut Vec<u8>| r.take(limit as u64).read_to_end(out).map(|_| ());
    match compression {
        1 => out = raw,
        5 => {
            let mut dec = weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8);
            let (mut consumed, mut written) = (0, 0);
            while written < limit {
                // The output grows as it is decoded: a chunk size from the header is not
                // allocated before the stream backs it.
                if written == out.len() {
                    let grown = limit.min(written.max(1 << 16) * 2);
                    fallible::reserve(&mut out, grown - written, "a TIFF chunk")?;
                    out.resize(grown, 0);
                }
                let r = dec.decode_bytes(&raw[consumed..], &mut out[written..]);
                consumed += r.consumed_in;
                written += r.consumed_out;
                match r.status.map_err(|e| invalid(format!("LZW: {e}")))? {
                    weezl::LzwStatus::Done => break,
                    weezl::LzwStatus::NoProgress if r.consumed_in == 0 && r.consumed_out == 0 => break,
                    _ => {}
                }
            }
            out.truncate(written);
        }
        8 | 32946 => bounded(&mut flate2::read::ZlibDecoder::new(&raw[..]), &mut out)?,
        #[cfg(feature = "lzma")]
        LZMA => bounded(&mut lzma_rust2::XzReader::new(&raw[..], true), &mut out).map_err(|e| invalid(format!("LZMA: {e}")))?,
        #[cfg(not(feature = "lzma"))]
        LZMA => return Err(disabled("lzma")),
        ZSTD => out = crate::zstandard::frames(&raw, limit)?,
        32773 => {
            // PackBits: a count byte n, then n + 1 literal bytes (n >= 0) or one byte repeated
            // 1 - n times (n < 0, -128 skipped): at most 64 bytes out per byte in.
            fallible::reserve(&mut out, limit.min(raw.len().saturating_mul(64)), "a TIFF chunk")?;
            let mut i = 0;
            while i < raw.len() && out.len() < limit {
                let n = raw[i] as i8;
                i += 1;
                if n >= 0 {
                    let end = (i + n as usize + 1).min(raw.len());
                    out.extend_from_slice(&raw[i..end]);
                    i = end;
                } else if n != -128 {
                    if let Some(&b) = raw.get(i) {
                        out.extend(std::iter::repeat_n(b, (1 - isize::from(n)) as usize));
                    }
                    i += 1;
                }
            }
        }
        other => return Err(invalid(format!("TIFF compression {other} with packed samples is not supported"))),
    }
    if out.len() < limit {
        return Err(invalid(format!("TIFF chunk decompressed to {} bytes, {limit} expected", out.len())));
    }
    out.truncate(limit);
    Ok(out)
}

/// Whether the `tiff` crate reads the pixel-interleaved chunks of an image of `samples` samples
/// with fewer (or more) samples per pixel than the file holds: RGB and CMYK images whose
/// `ExtraSamples` leave samples beyond the color ones and an alpha, or that have more samples than
/// `ExtraSamples` accounts for. The crate's color type counts the color samples and one alpha
/// only, and its reader lays the chunk out with that count (#208).
fn crate_misreads_samples(photometric: Option<u16>, samples: usize, extra: &[u16]) -> bool {
    let colors = match photometric {
        Some(2) => 3,
        Some(5) => 4,
        _ => return false,
    };
    let alpha = matches!(extra.first(), Some(1 | 2));
    let read = match samples.checked_sub(extra.len()) {
        // The crate rejects these images when it opens them.
        None => return false,
        Some(n) if n == colors => colors + usize::from(alpha),
        Some(n) if n == colors + 1 => colors + 1,
        // The crate reports an unsupported color type.
        Some(_) => 0,
    };
    read != samples
}

/// An error with a message, as the `tiff` crate reports I/O errors.
fn invalid(msg: impl Into<String>) -> TiffError {
    TiffError::IoError(std::io::Error::new(std::io::ErrorKind::InvalidData, msg.into()))
}

/// The error of a compression this build leaves out: terrio was built without `feature`.
#[cfg(not(all(feature = "lerc", feature = "lzma")))]
fn disabled(feature: &str) -> TiffError {
    TiffError::IoError(std::io::Error::new(std::io::ErrorKind::Unsupported, format!("built without the `{feature}` feature")))
}

/// How the chunks of one image are decoded when the `tiff` crate cannot do it (alone).
#[derive(Debug, Clone)]
pub(crate) struct ChunkCodec {
    codec: Codec,
    /// The image's chunk offsets and byte counts, shared with the reader of its windows.
    tables: Arc<ChunkTables>,
    tiled: bool,
    /// The tile size, or the image width and the rows per strip.
    chunk: (usize, usize),
    /// The samples of a pixel in one chunk (one when planar).
    spp: usize,
    sample: Sample,
    big_endian: bool,
    predictor: u16,
    /// The bits of a sample in the file.
    bits: u16,
    /// `FillOrder` 2: the bits of each byte are stored least significant first.
    lsb_first: bool,
    /// LERC: the compression wrapping the blobs (0 none, 1 Deflate, 2 Zstandard).
    #[cfg_attr(not(feature = "lerc"), allow(dead_code))]
    lerc_wrapper: u32,
    /// LERC: the last sample is an alpha channel, coded as the blob's mask.
    #[cfg_attr(not(feature = "lerc"), allow(dead_code))]
    lerc_alpha: bool,
    /// JPEG: the tables the chunks share (a JPEG stream without an image).
    jpeg_tables: Vec<u8>,
    /// JPEG: the image is coded in YCbCr, to be converted to RGB.
    ycbcr: bool,
    /// `PhotometricInterpretation` 0 (WhiteIsZero), whose samples the `tiff` crate inverts.
    white_is_zero: bool,
    /// The most bytes a decoded chunk, or a stream decompressed on the way, may take
    /// ([`crate::TiffReadLimits::chunk_bytes`]).
    limit: usize,
}

impl ChunkCodec {
    /// The codec for the image the decoder is positioned on, `None` when the `tiff` crate decodes
    /// its chunks itself. A chunk decoding to more than `limit` bytes is refused. `tables` are the
    /// image's chunk tables when the caller has them; without them, they are read here.
    pub(crate) fn new<R: Read + Seek>(dec: &mut Decoder<R>, limit: usize, tables: Option<Arc<ChunkTables>>) -> TiffResult<Option<Self>> {
        let compression = dec.find_tag_unsigned::<u16>(Tag::Compression)?.unwrap_or(1);
        let first = |v: Option<Vec<u16>>, default: u16| v.and_then(|v| v.first().copied()).unwrap_or(default);
        let packed_bits = first(dec.find_tag_unsigned_vec::<u16>(Tag::BitsPerSample)?, 1);
        let samples = dec.find_tag_unsigned::<u16>(Tag::SamplesPerPixel)?.unwrap_or(1).max(1) as usize;
        let planar = dec.find_tag_unsigned::<u16>(Tag::PlanarConfiguration)? == Some(2);
        let extra = dec.find_tag_unsigned_vec::<u16>(Tag::ExtraSamples)?.unwrap_or_default();
        let photometric = dec.find_tag_unsigned::<u16>(Tag::PhotometricInterpretation)?;
        let white_is_zero = photometric == Some(0);
        let misread = !planar && crate_misreads_samples(photometric, samples, &extra);
        // Samples of fewer than 8 bits: GDAL reads them as Byte (`GTiffBitmapBand`,
        // `GTiffOddBitsBand`), a sample per byte. The `tiff` crate gives them packed.
        let sub_byte = (1..8).contains(&packed_bits);
        let format = first(dec.find_tag_unsigned_vec::<u16>(Tag::SampleFormat)?, 1);
        // Complex samples, which the crate does not read: in the lossless compressions only.
        let complex = Sample::complex_part(format, packed_bits);
        let codec = match compression {
            1 | 5 | 8 | 32946 | 32773 | LZMA | ZSTD if complex.is_some() => Codec::Complex(compression),
            _ if complex.is_some() => {
                return Err(invalid(format!("TIFF compression {compression} with complex samples is not supported")));
            }
            _ if (9..32).contains(&packed_bits) && packed_bits != 16 => Codec::Packed(compression),
            1 | 5 | 8 | 32946 | 32773 | LZMA | ZSTD if sub_byte => Codec::Packed(compression),
            _ if sub_byte => Codec::CratePacked,
            // WhiteIsZero samples, whatever their type, as stored: the crate inverts or refuses them.
            1 | 5 | 8 | 32946 | 32773 if white_is_zero => Codec::WhiteIsZero(compression),
            LERC => Codec::Lerc,
            JXL | JXL_DNG => Codec::Jxl,
            LZMA => Codec::Lzma,
            ZSTD => Codec::Zstd,
            5 => Codec::Lzw,
            7 => Codec::Jpeg,
            WEBP => Codec::WebP,
            1 | 8 | 32946 | 32773 if misread => Codec::Chunky(compression),
            _ => return Ok(None),
        };
        let tiled = dec.get_chunk_type() == ChunkType::Tile;
        let tables = match tables {
            Some(tables) => tables,
            None => Arc::new(ChunkTables::read(dec)?),
        };
        let (cw, ch) = dec.chunk_dimensions();
        let bits = packed_bits;
        // A complex sample's real and imaginary parts are two samples of the chunk.
        let spp = if planar { 1 } else { samples } * if complex.is_some() { 2 } else { 1 };
        // JPEG of 8-bit samples: 12-bit JPEG is left to the `tiff` crate.
        if codec == Codec::Jpeg && bits != 8 {
            return Ok(None);
        }
        // LZW of other sample types is left to the `tiff` crate. WhiteIsZero images are decoded
        // here under every compression the crate would decode (`Codec::WhiteIsZero`), and under
        // Zstandard (the crate is built without it): as stored, as libtiff leaves them.
        if matches!(codec, Codec::Lzw | Codec::Chunky(_)) && Sample::new(format, bits).is_none() {
            return Ok(None);
        }
        let sample = match (codec, complex) {
            (Codec::Complex(_), Some(part)) => part,
            // Float16, given as the `tiff` crate gives it (read as Float32, as GDAL's Float16).
            (Codec::Zstd | Codec::WhiteIsZero(_), _) if (format, bits) == (3, 16) => Sample::F16,
            // Whatever the sample format, as GDAL (Byte, the bits as stored).
            (Codec::Packed(_) | Codec::CratePacked, _) if sub_byte => Sample::U8,
            (Codec::Packed(_), _) if format != 1 => {
                return Err(invalid(format!("TIFF samples of {bits} bits (format {format}) are not supported")));
            }
            (Codec::Packed(_), _) => {
                if bits <= 16 {
                    Sample::U16
                } else {
                    Sample::U32
                }
            }
            _ => Sample::new(format, bits).ok_or_else(|| {
                invalid(format!("TIFF compression {compression} with {bits}-bit samples (format {format}) is not supported"))
            })?,
        };
        let predictor = dec.find_tag_unsigned::<u16>(Tag::Predictor)?.unwrap_or(1);
        // libtiff's predictors are those of its codecs that take one: uncompressed and PackBits
        // complex and WhiteIsZero chunks are read as stored, whatever the tag says.
        let predictor = if matches!(codec, Codec::Complex(1 | 32773) | Codec::WhiteIsZero(1 | 32773)) { 1 } else { predictor };
        // libtiff's predictors need 8, 16, 32 or 64-bit samples.
        if matches!(codec, Codec::Packed(_) | Codec::CratePacked) && predictor != 1 {
            return Err(invalid(format!("TIFF predictor {predictor} with {bits}-bit samples is not supported")));
        }
        // Its horizontal predictor adds a complex sample as one word of its whole width (carrying
        // from one part to the other): not read here.
        if matches!(codec, Codec::Complex(_)) && predictor != 1 {
            return Err(invalid(format!("TIFF predictor {predictor} with complex samples is not supported")));
        }
        let lerc_wrapper = match codec {
            Codec::Lerc => dec.find_tag_unsigned_vec::<u32>(Tag::Unknown(LERC_PARAMETERS))?.and_then(|p| p.get(1).copied()).unwrap_or(0),
            _ => 0,
        };
        Ok(Some(Self {
            codec,
            tables,
            tiled,
            chunk: (cw as usize, ch as usize),
            spp,
            sample,
            big_endian: dec.byte_order() == tiff::tags::ByteOrder::BigEndian,
            predictor,
            bits,
            lsb_first: dec.find_tag_unsigned::<u16>(Tag::FillOrder)? == Some(2),
            lerc_wrapper,
            lerc_alpha: !planar && samples > 1 && extra.last() == Some(&2) && sample == Sample::U8,
            jpeg_tables: match codec {
                Codec::Jpeg => dec.find_tag(Tag::JPEGTables)?.map(|v| v.into_u8_vec()).transpose()?.unwrap_or_default(),
                _ => Vec::new(),
            },
            ycbcr: codec == Codec::Jpeg && photometric == Some(6),
            white_is_zero,
            limit,
        }))
    }

    /// Decodes chunk `idx`, as `Decoder::read_chunk` does: the samples of the chunk's image data,
    /// without padding.
    pub(crate) fn read<R: Read + Seek>(&self, dec: &mut Decoder<R>, idx: u32) -> TiffResult<DecodingResult> {
        if self.codec == Codec::CratePacked {
            return self.read_crate_packed(dec, idx);
        }
        let Some((offset, count)) = self.tables.get(idx as usize) else {
            return Err(invalid(format!("no TIFF chunk {idx}")));
        };
        let (dw, dh) = dec.chunk_data_dimensions(idx);
        let (dw, dh) = (dw as usize, dh as usize);
        // Tiles are coded whole, strips with their own rows.
        let (w, h) = if self.tiled { self.chunk } else { (dw, dh) };
        // Over the limit, the chunk is refused before any of it is read.
        if self.decoded_size(w, h)? > self.limit {
            return Err(TiffError::LimitsExceeded);
        }
        let r = dec.inner();
        r.seek(SeekFrom::Start(offset))?;
        // The buffer grows as bytes arrive: a corrupt byte count allocates no more than the file holds.
        let mut raw = Vec::new();
        r.take(count).read_to_end(&mut raw)?;
        if raw.len() as u64 != count {
            return Err(invalid(format!("TIFF chunk {idx}: {} bytes, {count} expected", raw.len())));
        }
        let bytes = match self.codec {
            Codec::Lzma | Codec::Lzw | Codec::Zstd | Codec::Chunky(_) | Codec::Complex(_) | Codec::WhiteIsZero(_) => {
                self.lossless(raw, w, h)?
            }
            Codec::Lerc => self.lerc(raw, w, h)?,
            Codec::Jxl => self.jxl(&raw, w, h)?,
            Codec::Jpeg => self.jpeg(&raw, w, h)?,
            Codec::WebP => self.webp(&raw, w, h)?,
            Codec::Packed(compression) => {
                let row_bytes = (w * self.spp).checked_mul(usize::from(self.bits)).map(|b| b.div_ceil(8));
                let size = row_bytes.and_then(|r| r.checked_mul(h)).ok_or_else(|| invalid("TIFF chunk too large"))?;
                self.unpack(&decompress(compression, raw, size)?, w, h)?
            }
            Codec::CratePacked => unreachable!("decoded by `read_crate_packed`"),
        };
        self.result(&bytes, w, dw, dh)
    }

    /// Decodes chunk `idx` of samples of 1 to 7 bits under a compression only the `tiff` crate
    /// decodes: the crate gives the chunk's image data, each row starting on a byte, which is
    /// unpacked to a byte per sample. The crate inverts WhiteIsZero samples, which libtiff (and
    /// GDAL) return as stored: they are inverted back.
    fn read_crate_packed<R: Read + Seek>(&self, dec: &mut Decoder<R>, idx: u32) -> TiffResult<DecodingResult> {
        let (dw, dh) = dec.chunk_data_dimensions(idx);
        let (dw, dh) = (dw as usize, dh as usize);
        let layout = dec.image_chunk_buffer_layout(idx)?;
        // Over the limit, packed or unpacked, the chunk is refused before its buffer is allocated.
        if layout.len > self.limit || self.decoded_size(dw, dh)? > self.limit {
            return Err(TiffError::LimitsExceeded);
        }
        let mut packed = fallible::filled_now(layout.len, 0u8, "a TIFF chunk")?;
        dec.read_chunk_bytes(idx, &mut packed)?;
        if self.white_is_zero {
            for b in &mut packed {
                *b = !*b;
            }
        }
        let bytes = self.unpack(&packed, dw, dh)?;
        self.result(&bytes, dw, dw, dh)
    }

    /// The bytes of `h` rows of `w` pixels of decoded samples.
    fn decoded_size(&self, w: usize, h: usize) -> TiffResult<usize> {
        w.checked_mul(h)
            .and_then(|n| n.checked_mul(self.spp))
            .and_then(|n| n.checked_mul(self.sample.size()))
            .ok_or_else(|| invalid("TIFF chunk too large"))
    }

    /// Decodes an LZMA, LZW or other lossless chunk of `h` rows of `w` pixels into native-endian
    /// samples, its predictor undone.
    fn lossless(&self, raw: Vec<u8>, w: usize, h: usize) -> TiffResult<Vec<u8>> {
        let size = self.sample.size();
        let n = w
            .checked_mul(h)
            .and_then(|n| n.checked_mul(self.spp))
            .and_then(|n| n.checked_mul(size))
            .ok_or_else(|| invalid("TIFF chunk too large"))?;
        let mut out = if self.codec == Codec::Lzw {
            let out = decompress(5, raw, n)?;
            // A truncated stream is an error, as in libtiff, rather than missing pixels.
            if out.len() < n {
                return Err(invalid(format!("LZW: chunk of {} bytes, {n} expected", out.len())));
            }
            out
        } else {
            // At most the chunk's size, however far the stream would expand.
            let compression = match self.codec {
                Codec::Zstd => ZSTD,
                Codec::Chunky(c) | Codec::Complex(c) | Codec::WhiteIsZero(c) => c,
                _ => LZMA,
            };
            decompress(compression, raw, n)?
        };
        out.resize(n, 0);
        // The floating-point predictor's byte planes give native-endian samples whatever the
        // file's byte order, as libtiff's.
        if self.big_endian && size > 1 && self.predictor != 3 {
            for v in out.chunks_exact_mut(size) {
                v.reverse();
            }
        }
        self.unpredict(&mut out, w);
        Ok(out)
    }

    /// Decodes a WebP chunk of `h` rows of `w` 8-bit pixels of 3 or 4 samples, as the `tiff`
    /// crate's `WebPReader` does (libtiff's `TWebPDecode()`): four samples are RGBA, opaque when
    /// the WebP image has no alpha.
    fn webp(&self, raw: &[u8], w: usize, h: usize) -> TiffResult<Vec<u8>> {
        let mut d = image_webp::WebPDecoder::new(std::io::Cursor::new(raw)).map_err(|e| invalid(format!("WebP: {e}")))?;
        let alpha = d.has_alpha();
        if !(self.spp == 4 || (self.spp == 3 && !alpha)) || self.sample != Sample::U8 {
            return Err(invalid("bad sample count for WebP compressed data"));
        }
        let (dw, dh) = d.dimensions();
        if (dw as usize, dh as usize) != (w, h) {
            return Err(invalid(format!("WebP image of {dw}x{dh}, {w}x{h} expected")));
        }
        // The sizes come from the WebP header (checked against the chunk's): allocated fallibly.
        let size = d.output_buffer_size().ok_or_else(|| invalid("WebP image too large"))?;
        let mut data = fallible::filled_now(size, 0u8, "a TIFF chunk")?;
        d.read_image(&mut data).map_err(|e| invalid(format!("WebP: {e}")))?;
        if self.spp == 4 && !alpha {
            let mut rgba = Vec::new();
            fallible::reserve(&mut rgba, fallible::len(&[data.len() / 3, 4], "a TIFF chunk")?, "a TIFF chunk")?;
            rgba.extend(data.as_chunks::<3>().0.iter().flat_map(|p| [p[0], p[1], p[2], 255]));
            data = rgba;
        }
        Ok(data)
    }

    /// Unpacks `h` rows of `w` pixels of samples of `self.bits` bits, MSB first with each row
    /// starting on a byte (as libtiff packs them; with `FillOrder` 2 each byte's bits are
    /// reversed first), into native-endian samples of the output type.
    fn unpack(&self, packed: &[u8], w: usize, h: usize) -> TiffResult<Vec<u8>> {
        let bits = usize::from(self.bits);
        let per_row = w * self.spp;
        let row_bytes = (per_row * bits).div_ceil(8);
        let size = self.sample.size();
        let mut out = fallible::filled_now(fallible::len(&[h, per_row, size], "a TIFF chunk")?, 0u8, "a TIFF chunk")?;
        for r in 0..h {
            let row = packed.get(r * row_bytes..).unwrap_or(&[]);
            for i in 0..per_row {
                let mut v: u32 = 0;
                let first = i * bits;
                for b in first..first + bits {
                    // FillOrder 2 stores each byte's bits least significant first.
                    let byte = row.get(b / 8).map_or(0, |&x| if self.lsb_first { x.reverse_bits() } else { x });
                    let bit = (byte >> (7 - b % 8)) & 1;
                    v = (v << 1) | u32::from(bit);
                }
                let at = (r * per_row + i) * size;
                match size {
                    1 => out[at] = v as u8,
                    2 => out[at..at + 2].copy_from_slice(&(v as u16).to_ne_bytes()),
                    _ => out[at..at + 4].copy_from_slice(&v.to_ne_bytes()),
                }
            }
        }
        Ok(out)
    }

    /// Undoes a predictor on native-endian samples, row by row.
    fn unpredict(&self, data: &mut [u8], w: usize) {
        let (size, spp) = (self.sample.size(), self.spp);
        let row = w * spp * size;
        if row == 0 {
            return;
        }
        match self.predictor {
            2 => {
                for r in data.chunks_exact_mut(row) {
                    macro_rules! acc {
                        ($t:ty) => {{
                            for i in spp..w * spp {
                                let (a, b) = (i * size, (i - spp) * size);
                                let prev = <$t>::from_ne_bytes(r[b..b + size].try_into().unwrap());
                                let cur = <$t>::from_ne_bytes(r[a..a + size].try_into().unwrap());
                                r[a..a + size].copy_from_slice(&cur.wrapping_add(prev).to_ne_bytes());
                            }
                        }};
                    }
                    match size {
                        1 => acc!(u8),
                        2 => acc!(u16),
                        4 => acc!(u32),
                        _ => acc!(u64),
                    }
                }
            }
            3 => {
                // Byte planes, most significant first, differenced along the row.
                let wc = w * spp;
                let mut tmp = vec![0u8; row];
                for r in data.chunks_exact_mut(row) {
                    for i in spp..row {
                        r[i] = r[i].wrapping_add(r[i - spp]);
                    }
                    tmp.copy_from_slice(r);
                    for v in 0..wc {
                        let mut be = [0u8; 8];
                        for (b, byte) in be[..size].iter_mut().enumerate() {
                            *byte = tmp[b * wc + v];
                        }
                        let out = &mut r[v * size..(v + 1) * size];
                        out.copy_from_slice(&be[..size]);
                        if cfg!(target_endian = "little") {
                            out.reverse();
                        }
                    }
                }
            }
            _ => {}
        }
    }

    /// Bound each LERC header before allowing mask or pixel inspection.
    /// Layouts follow lerc-reader 0.5's Lerc1/Lerc2 header readers. Their public
    /// inspection APIs can allocate masks, so bounds must precede those calls.
    #[cfg(feature = "lerc")]
    fn lerc_headers(&self, blob: &[u8], w: usize, h: usize) -> TiffResult<()> {
        let read = |bytes: &[u8], offset: usize| -> TiffResult<u32> {
            let value = bytes
                .get(offset..offset.checked_add(4).ok_or(TiffError::LimitsExceeded)?)
                .ok_or_else(|| invalid("LERC: truncated header"))?;
            let bytes: [u8; 4] = value.try_into().map_err(|_| invalid("LERC: truncated header"))?;
            Ok(u32::from_le_bytes(bytes))
        };
        let expected = w.checked_mul(h).ok_or(TiffError::LimitsExceeded)?;
        let (mut offset, mut channels, mut bytes_total) = (0usize, 0usize, 0usize);
        let mut lerc1_mask: Option<Vec<u8>> = None;
        while offset < blob.len() {
            let data = &blob[offset..];
            let (width, height, depth, size, length) = if data.starts_with(b"Lerc2 ") {
                let version = read(data, 6)?;
                if !(1..=6).contains(&version) {
                    return Err(invalid("LERC: unsupported Lerc2 header version"));
                }
                let shift = usize::from(version >= 3) * 4;
                let height = read(data, 10 + shift)? as usize;
                let width = read(data, 14 + shift)? as usize;
                let depth = if version >= 4 { read(data, 18 + shift)? as usize } else { 1 };
                let extra = usize::from(version >= 4) * 4;
                let length = read(data, 26 + shift + extra)? as usize;
                let kind = read(data, 30 + shift + extra)? as i32;
                let size = lerc_core::DataType::from_code(kind).map_err(|e| invalid(format!("LERC: {e}")))?.byte_len();
                lerc1_mask = None;
                (width, height, depth, size, length)
            } else if data.starts_with(b"CntZImage") {
                let height = read(data, 18)? as usize;
                let width = read(data, 22)? as usize;
                // Lerc1 can either include its mask or inherit the preceding
                // band's mask. Let the decoder select the actual boundary below,
                // only after checking this header's dimensions.
                (width, height, 1, 4, 0)
            } else {
                return Err(invalid("LERC: invalid blob header"));
            };
            let pixels = width.checked_mul(height).ok_or(TiffError::LimitsExceeded)?;
            let bytes = pixels.checked_mul(depth).and_then(|n| n.checked_mul(size)).ok_or(TiffError::LimitsExceeded)?;
            bytes_total = bytes_total.checked_add(bytes).ok_or(TiffError::LimitsExceeded)?;
            if bytes_total > self.limit {
                return Err(TiffError::LimitsExceeded);
            }
            channels = channels.checked_add(depth).ok_or(TiffError::LimitsExceeded)?;
            if pixels > expected {
                return Err(invalid(format!("LERC: a {width} x {height} blob for a {w} x {h} chunk")));
            }
            if channels > self.spp || pixels == 0 || depth == 0 {
                return Err(invalid("LERC: blob dimensions or channels exceed the TIFF chunk"));
            }
            let length = if data.starts_with(b"CntZImage") {
                // Legacy inspection can decode pixels to determine the range;
                // use its bounded first-blob decoder to retain the actual mask
                // and follow inline as well as inherited masks without guessing.
                let decoded = match lerc1_mask.as_deref() {
                    Some(mask) => lerc_reader::decode_first_with_mask(data, mask),
                    None => lerc_reader::decode_first(data),
                }
                .map_err(|e| invalid(format!("LERC: {e}")))?;
                lerc1_mask = decoded.mask;
                decoded.info.blob_size
            } else {
                length
            };
            if length == 0 || length > data.len() {
                return Err(invalid("LERC: invalid blob length"));
            }
            offset = offset.checked_add(length).ok_or(TiffError::LimitsExceeded)?;
        }
        if channels != self.spp && !(self.lerc_alpha && channels.checked_add(1) == Some(self.spp)) {
            return Err(invalid("LERC: decoded channel count differs from TIFF"));
        }

        Ok(())
    }

    /// Decodes LERC blobs into native-endian samples, as libtiff's LERC codec does: masked pixels
    /// of floating-point images are NaN, a mask coding the alpha channel becomes 0 or 255.
    #[cfg(feature = "lerc")]
    fn lerc(&self, raw: Vec<u8>, w: usize, h: usize) -> TiffResult<Vec<u8>> {
        // A blob holds the chunk's samples, at most, with a mask and headers: one inflating past
        // twice that, past every sample as a double with a mask and headers, or past the limit,
        // is not inflated further.
        let decoded = self.decoded_size(w, h)?;
        let most = fallible::len(&[w, h, self.spp.max(1), 9], "a LERC blob")?.saturating_add(1 << 20);
        let cap = decoded.saturating_mul(2).saturating_add(4096 * (self.spp + 1)).min(most);
        let inflate = |r: &mut dyn Read| -> TiffResult<Vec<u8>> {
            let mut out = Vec::new();
            r.take(cap.min(self.limit).saturating_add(1) as u64).read_to_end(&mut out)?;
            if out.len() > self.limit {
                Err(TiffError::LimitsExceeded)
            } else if out.len() > cap {
                Err(invalid(format!("LERC: blobs of over {cap} bytes for a {w} x {h} chunk")))
            } else {
                Ok(out)
            }
        };
        let blob = match self.lerc_wrapper {
            0 => raw,
            1 => inflate(&mut flate2::read::ZlibDecoder::new(&raw[..]))?,
            2 => {
                let mut src = &raw[..];
                inflate(&mut ruzstd::decoding::StreamingDecoder::new(&mut src).map_err(|e| invalid(format!("Zstandard: {e}")))?)?
            }
            other => return Err(invalid(format!("unknown LERC compression wrapper {other}"))),
        };
        self.lerc_headers(&blob, w, h)?;
        let lerc_err = |e: lerc_core::Error| invalid(format!("LERC: {e}"));
        let n = w * h;
        // The blob's own size, checked before it is decoded.
        let info = lerc_reader::inspect_first(&blob).map_err(lerc_err)?;
        if (info.width as usize).saturating_mul(info.height as usize) > n {
            return Err(invalid(format!("LERC: a {} x {} blob for a {w} x {h} chunk", info.width, info.height)));
        }
        let bands = lerc_reader::get_band_count(&blob).map_err(lerc_err)?;
        // Samples interleaved by pixel, with a validity mask per sample.
        let (mut samples, masks): (Vec<f64>, Vec<Option<Vec<u8>>>) = if bands > 1 {
            // A floating-point image with one blob per sample.
            let count = n.checked_mul(bands).ok_or(TiffError::LimitsExceeded)?;
            count.checked_mul(std::mem::size_of::<f64>()).ok_or(TiffError::LimitsExceeded)?;
            let set = lerc_reader::decode_band_set(&blob).map_err(lerc_err)?;
            let (_, planes, masks) = set.into_parts();
            let planes: Vec<Vec<f64>> = planes.iter().map(pixels_f64).collect();
            let mut out = fallible::filled_now(count, 0.0, "a TIFF chunk")?;
            for (b, p) in planes.iter().enumerate() {
                for (i, v) in p.iter().take(n).enumerate() {
                    out[i * bands + b] = *v;
                }
            }
            (out, masks)
        } else {
            let d = lerc_reader::decode(&blob).map_err(lerc_err)?;
            (pixels_f64(&d.pixels), vec![d.mask])
        };
        let depth = samples.len() / n.max(1);
        if self.lerc_alpha && depth + 1 == self.spp {
            let mask = masks.first().cloned().flatten();
            let mut out = Vec::new();
            fallible::reserve(&mut out, fallible::len(&[n, self.spp], "a TIFF chunk")?, "a TIFF chunk")?;
            for i in 0..n {
                out.extend_from_slice(&samples[i * depth..(i + 1) * depth]);
                out.push(if mask.as_ref().is_none_or(|m| m[i] != 0) { 255.0 } else { 0.0 });
            }
            samples = out;
        } else if self.sample.is_float() {
            for (b, mask) in masks.iter().enumerate() {
                let Some(mask) = mask else { continue };
                for (i, valid) in mask.iter().enumerate().take(n) {
                    if *valid == 0 {
                        if masks.len() > 1 {
                            samples[i * depth + b] = f64::NAN;
                        } else {
                            samples[i * depth..(i + 1) * depth].fill(f64::NAN);
                        }
                    }
                }
            }
        }
        if samples.len() != n * self.spp {
            return Err(invalid(format!("LERC: {} samples for a {w} x {h} chunk of {} samples per pixel", samples.len(), self.spp)));
        }
        self.to_bytes(&samples)
    }

    #[cfg(not(feature = "lerc"))]
    fn lerc(&self, _raw: Vec<u8>, _w: usize, _h: usize) -> TiffResult<Vec<u8>> {
        Err(disabled("lerc"))
    }

    /// Decodes a JPEG XL codestream into native-endian samples: the colour channels, then the extra
    /// channels, interleaved by pixel, as libtiff's JPEG XL codec does.
    fn jxl(&self, raw: &[u8], w: usize, h: usize) -> TiffResult<Vec<u8>> {
        use jxl_oxide::image::BitDepth;
        // On terrio's thread pool: `jxl-oxide` decodes with rayon (see `par`).
        let (image, render) = crate::par::install(|| {
            let image = jxl_oxide::JxlImage::builder().read(raw).map_err(|e| invalid(format!("JPEG XL: {e}")))?;
            // The codestream's size, checked before it is rendered: at most the tile, or the strip.
            let (iw, ih) = (image.width() as usize, image.height() as usize);
            if iw > self.chunk.0.max(w) || ih > self.chunk.1.max(h) {
                return Err(invalid(format!("JPEG XL: a {iw} x {ih} image for a {w} x {h} chunk")));
            }
            if self.decoded_size(iw, ih)? > self.limit {
                return Err(TiffError::LimitsExceeded);
            }
            let render = image.render_frame(0).map_err(|e| invalid(format!("JPEG XL: {e}")))?;
            Ok::<_, tiff::TiffError>((image, render))
        })?;
        let meta = &image.image_header().metadata;
        // Each channel's bit depth: the colour channels', then each extra channel's.
        let planes = render.image_planar();
        let depths: Vec<BitDepth> = std::iter::repeat_n(meta.bit_depth, planes.len().saturating_sub(meta.ec_info.len()))
            .chain(meta.ec_info.iter().map(|e| e.bit_depth))
            .collect();
        if planes.len() != self.spp {
            return Err(invalid(format!("JPEG XL: {} channels, {} expected", planes.len(), self.spp)));
        }
        let n = w * h;
        let mut samples = fallible::filled_now(fallible::len(&[n, self.spp], "a TIFF chunk")?, 0.0f64, "a TIFF chunk")?;
        for (c, (plane, depth)) in planes.iter().zip(depths).enumerate() {
            if plane.width() < w || plane.height() < h {
                return Err(invalid("JPEG XL: channel smaller than the chunk"));
            }
            // Rendered samples: integers nominally from 0 to 1, floating-point values as they are.
            let max = match depth {
                BitDepth::IntegerSample { bits_per_sample } => Some(((1u64 << bits_per_sample.min(63)) - 1) as f64),
                BitDepth::FloatSample { .. } => None,
            };
            let buf = plane.buf();
            for i in 0..n {
                let v = buf[(i / w) * plane.width() + i % w] as f64;
                samples[i * self.spp + c] = match max {
                    Some(max) => (v * max).round().clamp(0.0, max),
                    None => v,
                };
            }
        }
        self.to_bytes(&samples)
    }

    /// Decodes a JPEG chunk, its stream completed with the shared tables.
    fn jpeg(&self, raw: &[u8], w: usize, h: usize) -> TiffResult<Vec<u8>> {
        // The tables without their end marker, then the chunk without its start marker.
        let stream = match (self.jpeg_tables.len() >= 4, raw.len() >= 2) {
            (true, true) => [&self.jpeg_tables[..self.jpeg_tables.len() - 2], &raw[2..]].concat(),
            _ => raw.to_vec(),
        };
        // Read SOF metadata before the decoder allocates its component planes.
        let (jw, jh, components) = crate::jpeg::frame_dimensions(&stream).map_err(|e| invalid(format!("JPEG: {e}")))?;
        if self.sample != Sample::U8 || components != self.spp {
            return Err(invalid("JPEG sample layout differs from TIFF"));
        }
        let len = jw.checked_mul(jh).and_then(|n| n.checked_mul(components)).ok_or(TiffError::LimitsExceeded)?;
        if len > self.limit {
            return Err(TiffError::LimitsExceeded);
        }
        let color = if self.ycbcr { crate::jpeg::Color::Rgb } else { crate::jpeg::Color::Raw };
        let image = crate::jpeg::decode(&stream, color).map_err(|e| invalid(format!("JPEG: {e}")))?;
        let (jw, jh, spp, pixels) = (image.width, image.height, image.components, image.data);
        if spp != self.spp || jw < w || jh < h {
            return Err(invalid(format!("JPEG: a {jw} x {jh} image of {spp} components for a {w} x {h} chunk of {} samples", self.spp)));
        }
        let mut out = Vec::new();
        fallible::reserve(&mut out, fallible::len(&[h, w, spp], "a TIFF chunk")?, "a TIFF chunk")?;
        out.extend(pixels.chunks_exact(jw * spp).take(h).flat_map(|row| &row[..w * spp]));
        Ok(out)
    }

    /// Native-endian bytes of samples of the image's type.
    fn to_bytes(&self, samples: &[f64]) -> TiffResult<Vec<u8>> {
        let mut out = Vec::new();
        fallible::reserve(&mut out, fallible::len(&[samples.len(), self.sample.size()], "a TIFF chunk")?, "a TIFF chunk")?;
        for &v in samples {
            match self.sample {
                Sample::U8 => out.push(v as u8),
                Sample::I8 => out.extend((v as i8).to_ne_bytes()),
                Sample::U16 => out.extend((v as u16).to_ne_bytes()),
                Sample::I16 => out.extend((v as i16).to_ne_bytes()),
                Sample::U32 => out.extend((v as u32).to_ne_bytes()),
                Sample::I32 => out.extend((v as i32).to_ne_bytes()),
                Sample::U64 => out.extend((v as u64).to_ne_bytes()),
                Sample::I64 => out.extend((v as i64).to_ne_bytes()),
                Sample::F16 => out.extend(half::f16::from_f64(v).to_ne_bytes()),
                Sample::F32 => out.extend((v as f32).to_ne_bytes()),
                Sample::F64 => out.extend(v.to_ne_bytes()),
            }
        }
        Ok(out)
    }

    /// The `dw` x `dh` image data of a chunk decoded `w` samples wide, typed.
    fn result(&self, bytes: &[u8], w: usize, dw: usize, dh: usize) -> TiffResult<DecodingResult> {
        let size = self.sample.size();
        let (row, keep) = (w * self.spp * size, dw * self.spp * size);
        let what = "a TIFF chunk";
        let total = fallible::len(&[dh, keep], what)?;
        let mut data = Vec::new();
        fallible::reserve(&mut data, total, what)?;
        for r in 0..dh {
            let start = r * row;
            data.extend_from_slice(bytes.get(start..start + keep).unwrap_or(&[]));
        }
        data.resize(total, 0);
        macro_rules! typed {
            ($variant:ident, $t:ty) => {{
                let mut typed = Vec::new();
                fallible::reserve(&mut typed, total / size, what)?;
                typed.extend(data.chunks_exact(size).map(|c| <$t>::from_ne_bytes(c.try_into().unwrap())));
                DecodingResult::$variant(typed)
            }};
        }
        Ok(match self.sample {
            Sample::U8 => DecodingResult::U8(data),
            Sample::I8 => typed!(I8, i8),
            Sample::U16 => typed!(U16, u16),
            Sample::I16 => typed!(I16, i16),
            Sample::U32 => typed!(U32, u32),
            Sample::I32 => typed!(I32, i32),
            Sample::U64 => typed!(U64, u64),
            Sample::I64 => typed!(I64, i64),
            Sample::F16 => typed!(F16, half::f16),
            Sample::F32 => typed!(F32, f32),
            Sample::F64 => typed!(F64, f64),
        })
    }
}

/// LERC samples as `f64`, which holds every type LERC codes exactly.
#[cfg(feature = "lerc")]
fn pixels_f64(p: &lerc_core::PixelData) -> Vec<f64> {
    use lerc_core::PixelData as P;
    match p {
        P::I8(v) => v.iter().map(|x| *x as f64).collect(),
        P::U8(v) => v.iter().map(|x| *x as f64).collect(),
        P::I16(v) => v.iter().map(|x| *x as f64).collect(),
        P::U16(v) => v.iter().map(|x| *x as f64).collect(),
        P::I32(v) => v.iter().map(|x| *x as f64).collect(),
        P::U32(v) => v.iter().map(|x| *x as f64).collect(),
        P::F32(v) => v.iter().map(|x| *x as f64).collect(),
        P::F64(v) => v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lzw(sample: Sample, spp: usize, predictor: u16, big_endian: bool) -> ChunkCodec {
        ChunkCodec { codec: Codec::Lzw, sample, spp, predictor, big_endian, ..packed(8, spp) }
    }

    /// The images whose chunks the `tiff` crate lays out with the wrong number of samples (#208).
    #[test]
    fn rgb_and_cmyk_with_samples_besides_color_and_alpha() {
        // RGB, RGBA (with and without ExtraSamples) and CMYK the crate reads as they are.
        assert!(!crate_misreads_samples(Some(2), 3, &[]));
        assert!(!crate_misreads_samples(Some(2), 4, &[2]));
        assert!(!crate_misreads_samples(Some(2), 4, &[1]));
        assert!(!crate_misreads_samples(Some(2), 4, &[]));
        assert!(!crate_misreads_samples(Some(5), 4, &[]));
        assert!(!crate_misreads_samples(Some(5), 5, &[2]));
        // Gray and multiband images are read with all their samples.
        assert!(!crate_misreads_samples(Some(1), 4, &[0, 0, 0]));
        assert!(!crate_misreads_samples(None, 6, &[]));
        // NAIP as gdalwarp writes it, RGB with several extra samples, alpha after other samples.
        assert!(crate_misreads_samples(Some(2), 4, &[0]));
        assert!(crate_misreads_samples(Some(2), 5, &[0, 0]));
        assert!(crate_misreads_samples(Some(2), 5, &[0, 2]));
        assert!(crate_misreads_samples(Some(2), 5, &[2, 0]));
        assert!(crate_misreads_samples(Some(2), 6, &[0]));
        assert!(crate_misreads_samples(Some(2), 5, &[]));
        assert!(crate_misreads_samples(Some(5), 5, &[0]));
    }

    fn lzw_encode(bytes: &[u8]) -> Vec<u8> {
        weezl::encode::Encoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8).encode(bytes).unwrap()
    }

    /// Big- and little-endian LZW chunks with the horizontal predictor, coded as libtiff codes
    /// them: samples differenced along the row, then stored in the file's byte order.
    #[test]
    fn lzw_with_the_horizontal_predictor() {
        let (w, h, spp) = (5, 3, 2);
        let values: Vec<u16> = (0..w * h * spp).map(|i| (i * 7919 % 65521) as u16).collect();
        for big_endian in [false, true] {
            let mut coded = Vec::new();
            for row in values.chunks(w * spp) {
                for (i, v) in row.iter().enumerate() {
                    let d = if i < spp { *v } else { v.wrapping_sub(row[i - spp]) };
                    coded.extend(if big_endian { d.to_be_bytes() } else { d.to_le_bytes() });
                }
            }
            let out = lzw(Sample::U16, spp, 2, big_endian).lossless(lzw_encode(&coded), w, h).unwrap();
            let back: Vec<u16> = out.as_chunks::<2>().0.iter().map(|c| u16::from_ne_bytes(*c)).collect();
            assert_eq!(back, values, "big endian {big_endian}");
        }
    }

    /// LZW chunks with the floating-point predictor, coded as libtiff codes them: each row's
    /// samples split into byte planes, most significant first, then differenced byte by byte. The
    /// planes are the same in big- and little-endian files.
    #[test]
    fn lzw_with_the_floating_point_predictor() {
        let (w, h) = (6, 4);
        let values: Vec<f32> = (0..w * h).map(|i| (i as f32 * 1.37).sin() * 1000.0).collect();
        let mut coded = Vec::new();
        for row in values.chunks(w) {
            let mut planes = vec![0u8; w * 4];
            for (i, v) in row.iter().enumerate() {
                for (b, byte) in v.to_be_bytes().iter().enumerate() {
                    planes[b * w + i] = *byte;
                }
            }
            for i in (1..planes.len()).rev() {
                planes[i] = planes[i].wrapping_sub(planes[i - 1]);
            }
            coded.extend(planes);
        }
        for big_endian in [false, true] {
            let out = lzw(Sample::F32, 1, 3, big_endian).lossless(lzw_encode(&coded), w, h).unwrap();
            let back: Vec<f32> = out.as_chunks::<4>().0.iter().map(|c| f32::from_ne_bytes(*c)).collect();
            assert_eq!(back, values, "big endian {big_endian}");
        }
    }

    /// Zstandard chunks decode (with the zstd C library natively with the `libzstd` feature, with
    /// `ruzstd` otherwise), whichever encoder wrote them.
    #[test]
    fn zstandard_chunks_decode_in_pure_rust() {
        let c = ChunkCodec { codec: Codec::Zstd, ..lzw(Sample::U16, 1, 2, false) };
        // A 4 x 4 UInt16 ramp, differenced by row (predictor 2).
        let rows: Vec<u8> =
            (0..4u16).flat_map(|r| (0..4u16).map(move |i| if i == 0 { r * 100 } else { 3 })).flat_map(u16::to_ne_bytes).collect();
        let want: Vec<u8> = (0..4u16).flat_map(|r| (0..4u16).map(move |i| r * 100 + 3 * i)).flat_map(u16::to_ne_bytes).collect();
        assert_eq!(c.lossless(crate::zstandard::compress(&rows, 9).unwrap(), 4, 4).unwrap(), want);
        assert_eq!(
            c.lossless(ruzstd::encoding::compress_to_vec(&rows[..], ruzstd::encoding::CompressionLevel::Fastest), 4, 4).unwrap(),
            want
        );
        // A short stream is an error.
        assert!(c.lossless(crate::zstandard::compress(&rows[..8], 9).unwrap(), 4, 4).is_err());
    }

    /// A truncated LZW chunk is an error, and a header's chunk size overflowing is refused.
    #[test]
    fn short_and_oversized_lzw_chunks_are_errors() {
        let c = lzw(Sample::U8, 1, 1, false);
        assert!(c.lossless(lzw_encode(&[1, 2, 3]), 4, 4).is_err());
        assert!(c.lossless(lzw_encode(&[1; 16]), 4, 4).is_ok());
        assert!(c.lossless(lzw_encode(&[1]), usize::MAX, 2).is_err());
    }

    /// A little-endian BigTIFF of one 4 x 4 byte strip of `data`, LZW-coded, whose
    /// StripByteCounts says `count`.
    fn bigtiff_strip(data: &[u8], count: u64) -> Vec<u8> {
        let tags: [(u16, u16, u64); 9] =
            [(256, 3, 4), (257, 3, 4), (258, 3, 8), (259, 3, 5), (262, 3, 1), (273, 16, 0), (277, 3, 1), (278, 3, 4), (279, 16, count)];
        let data_offset = 16 + 8 + tags.len() as u64 * 20 + 8;
        let mut b = b"II\x2b\0\x08\0\0\0".to_vec();
        b.extend(16u64.to_le_bytes());
        b.extend((tags.len() as u64).to_le_bytes());
        for (tag, typ, value) in tags {
            b.extend(tag.to_le_bytes());
            b.extend(typ.to_le_bytes());
            b.extend(1u64.to_le_bytes());
            b.extend((if tag == 273 { data_offset } else { value }).to_le_bytes());
        }
        b.extend(0u64.to_le_bytes());
        b.extend(data);
        b
    }

    /// A chunk's byte count from the header is not allocated before the file backs it: a corrupt
    /// count is an error, where allocating it would abort the process.
    #[test]
    fn a_corrupt_chunk_byte_count_is_an_error() {
        let data = lzw_encode(&[7; 16]);
        for (count, ok) in [(data.len() as u64, true), (1 << 50, false)] {
            let mut dec = Decoder::new(std::io::Cursor::new(bigtiff_strip(&data, count))).unwrap();
            let codec = ChunkCodec::new(&mut dec, usize::MAX, None).unwrap().unwrap();
            assert_eq!(codec.read(&mut dec, 0).is_ok(), ok, "byte count {count}");
        }
    }

    /// A LERC chunk's Deflate or Zstandard stream inflates no further than twice its chunk, or
    /// than the limit; a blob larger than its chunk is not decoded.
    #[cfg(feature = "lerc")]
    #[test]
    fn lerc_streams_inflate_no_further_than_the_chunk_or_the_limit() {
        use std::io::Write;
        let codec = |lerc_wrapper, limit| ChunkCodec { codec: Codec::Lerc, sample: Sample::U8, lerc_wrapper, limit, ..packed(8, 1) };
        let encode = |w: u32, h: u32| {
            let pixels: Vec<u8> = (0..w * h).map(|i| (i % 251) as u8).collect();
            lerc_writer::encode(lerc_writer::RasterView::new(w, h, 1, &pixels[..]).unwrap(), None, lerc_writer::EncodeOptions::new())
                .unwrap()
        };
        let deflate = |b: &[u8]| {
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
            z.write_all(b).unwrap();
            z.finish().unwrap()
        };
        let zstd = |b: &[u8]| ruzstd::encoding::compress_to_vec(b, ruzstd::encoding::CompressionLevel::Fastest);
        // 64 MiB of zeros in a few kilobytes.
        let bomb = vec![0u8; 64 << 20];
        for (wrapper, wrap) in [(1, &deflate as &dyn Fn(&[u8]) -> Vec<u8>), (2, &zstd)] {
            let raw = wrap(&bomb);
            assert!(raw.len() < 1 << 20);
            // A 4 x 4 chunk: corrupt past 2 x 16 + 8192 bytes.
            let e = codec(wrapper, usize::MAX).lerc(raw.clone(), 4, 4).unwrap_err();
            assert!(e.to_string().contains("blobs of over 8224 bytes"), "{e}");
            // A 1024 x 1024 chunk under a limit of 1 MiB: the limit stops the stream.
            let e = codec(wrapper, 1 << 20).lerc(raw, 1024, 1024).unwrap_err();
            assert!(matches!(e, TiffError::LimitsExceeded), "{e}");
            // A valid chunk decodes with the limit at its blob's size.
            let blob = encode(4, 4);
            let pixels = codec(wrapper, blob.len()).lerc(wrap(&blob), 4, 4).unwrap();
            assert_eq!(pixels, (0..16).collect::<Vec<u8>>());
        }
        // A blob larger than its chunk.
        let e = codec(0, usize::MAX).lerc(encode(64, 64), 4, 4).unwrap_err();
        assert!(e.to_string().contains("a 64 x 64 blob for a 4 x 4 chunk"), "{e}");
    }

    #[cfg(feature = "lerc")]
    #[test]
    fn codec_header_limits_lerc_before_mask_inspection() {
        // Lerc2 v2 has no checksum. A partial mask's header must be checked
        // before the library allocates its bitmap, including in appended bands.
        let header = |width: u32, masked: bool| {
            let mut blob = b"Lerc2 ".to_vec();
            for value in [2, 1, width, if masked { 1 } else { width }, 8, if masked { 64 } else { 62 }, 1] {
                blob.extend_from_slice(&value.to_le_bytes());
            }
            for _ in 0..3 {
                blob.extend_from_slice(&0.0_f64.to_le_bytes());
            }
            blob.extend_from_slice(&(if masked { 2_u32 } else { 0 }).to_le_bytes());
            if masked {
                blob.extend_from_slice(&i16::MIN.to_le_bytes());
            }
            blob
        };
        let large = header(1 << 26, true);
        for blob in [large.clone(), [header(16, false), large].concat()] {
            let codec = ChunkCodec { codec: Codec::Lerc, limit: 1024, ..packed(8, 2) };
            let error = codec.lerc(blob, 4, 4).unwrap_err();
            assert!(matches!(error, TiffError::LimitsExceeded), "{error}");
        }
    }

    #[cfg(feature = "lerc")]
    #[test]
    fn lerc_header_counts_allow_full_channels_and_mask_alpha_only() {
        for alpha in [false, true] {
            let codec = ChunkCodec { codec: Codec::Lerc, sample: Sample::U8, limit: 1024, lerc_alpha: alpha, ..packed(8, 3) };
            for bands in 1..=4 {
                let pixels = vec![1_u8; 4 * bands];
                let view = lerc_writer::BandSetView::new(2, 2, 1, bands, lerc_core::BandLayout::Bsq, &pixels).unwrap();
                let raw = lerc_writer::encode_band_set(view, None, lerc_writer::EncodeOptions::new()).unwrap();
                let allowed = bands == 3 || (alpha && bands == 2);
                assert_eq!(codec.lerc_headers(&raw, 2, 2).is_ok(), allowed, "alpha={alpha}, bands={bands}");
                if allowed {
                    let pixels = codec.lerc(raw, 2, 2).unwrap();
                    let expected = if bands == 2 { [1, 1, 255] } else { [1, 1, 1] };
                    assert_eq!(pixels, expected.repeat(4));
                }
            }
        }
    }

    #[cfg(feature = "lerc")]
    #[test]
    fn legacy_lerc_headers_follow_inline_and_inherited_masks() {
        let blob = |inline_mask: bool, width: u32| {
            let mut raw = b"CntZImage ".to_vec();
            for value in [11_u32, 8, 2, width] {
                raw.extend_from_slice(&value.to_le_bytes());
            }
            raw.extend_from_slice(&0.0_f64.to_le_bytes());
            if inline_mask {
                // Implicit all-invalid mask; the next band may inherit it.
                raw.extend_from_slice(&[0; 16]);
            }
            for value in [1_u32, 1, 1, 0] {
                raw.extend_from_slice(&value.to_le_bytes());
            }
            raw.push(2); // Constant zero pixel block.
            raw
        };
        let codec = ChunkCodec { codec: Codec::Lerc, sample: Sample::F32, limit: 1024, ..packed(32, 2) };
        for inline_mask in [false, true] {
            let raw = [blob(true, 2), blob(inline_mask, 2)].concat();
            codec.lerc_headers(&raw, 2, 2).unwrap();
            assert_eq!(lerc_reader::get_band_count(&raw).unwrap(), 2);
            let raw = [blob(true, 2), blob(inline_mask, 1 << 26)].concat();
            assert!(matches!(codec.lerc_headers(&raw, 2, 2), Err(TiffError::LimitsExceeded)));
        }
    }

    #[test]
    fn codec_header_limits_jpeg_before_decoding() {
        let mut raw = Vec::new();
        jpeg_encoder::Encoder::new(&mut raw, 90).encode(&[0; 64 * 64], 64, 64, jpeg_encoder::ColorType::Luma).unwrap();
        let codec = ChunkCodec { codec: Codec::Jpeg, sample: Sample::U8, limit: 1024, ..packed(8, 1) };
        let error = codec.jpeg(&raw, 4, 4).unwrap_err();
        assert!(matches!(error, TiffError::LimitsExceeded), "{error}");
        let codec = ChunkCodec { limit: 4096, ..codec };
        assert_eq!(codec.jpeg(&raw, 4, 4).unwrap(), vec![0; 16]);
        let codec = ChunkCodec { sample: Sample::U16, ..codec };
        assert!(codec.jpeg(&raw, 4, 4).unwrap_err().to_string().contains("sample layout"));
    }

    fn packed(bits: u16, spp: usize) -> ChunkCodec {
        ChunkCodec {
            codec: Codec::Packed(1),
            tables: Arc::default(),
            tiled: false,
            chunk: (0, 0),
            spp,
            sample: if bits < 8 {
                Sample::U8
            } else if bits <= 16 {
                Sample::U16
            } else {
                Sample::U32
            },
            big_endian: false,
            predictor: 1,
            bits,
            lsb_first: false,
            lerc_wrapper: 0,
            lerc_alpha: false,
            jpeg_tables: vec![],
            ycbcr: false,
            white_is_zero: false,
            limit: usize::MAX,
        }
    }

    /// Packs samples MSB first, each row starting on a byte, as libtiff does.
    fn pack(rows: &[Vec<u32>], bits: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for row in rows {
            let mut bitbuf = vec![0u8; (row.len() * bits).div_ceil(8)];
            for (i, v) in row.iter().enumerate() {
                for b in 0..bits {
                    if v >> (bits - 1 - b) & 1 == 1 {
                        let at = i * bits + b;
                        bitbuf[at / 8] |= 0x80 >> (at % 8);
                    }
                }
            }
            out.extend(bitbuf);
        }
        out
    }

    #[test]
    fn unpacks_packed_samples_by_row() {
        // 15 and 12 bits (UInt16) and 24 bits (UInt32), with rows that end mid-byte.
        for (bits, max) in [(15u16, (1u32 << 15) - 1), (12, 4095), (24, (1 << 24) - 1)] {
            let rows: Vec<Vec<u32>> = (0..3).map(|r| (0..5).map(|c| (r * 7919 + c * 104_729) % (max + 1)).collect()).collect();
            let codec = packed(bits, 1);
            let bytes = codec.unpack(&pack(&rows, bits as usize), 5, 3).unwrap();
            let got: Vec<u32> = match codec.result(&bytes, 5, 5, 3).unwrap() {
                DecodingResult::U16(v) => v.into_iter().map(u32::from).collect(),
                DecodingResult::U32(v) => v,
                _ => unreachable!(),
            };
            assert_eq!(got, rows.concat(), "{bits} bits");
        }
    }

    #[test]
    fn reads_lsb_first_bytes() {
        // FillOrder 2: the same packed samples with each byte's bits reversed.
        let rows = vec![vec![1u32, 32_767, 12_345, 7], vec![0, 1 << 14, 3, 32_000]];
        let mut codec = packed(15, 1);
        codec.lsb_first = true;
        let lsb: Vec<u8> = pack(&rows, 15).iter().map(|b| b.reverse_bits()).collect();
        let DecodingResult::U16(got) = codec.result(&codec.unpack(&lsb, 4, 2).unwrap(), 4, 4, 2).unwrap() else { unreachable!() };
        assert_eq!(got.iter().map(|v| u32::from(*v)).collect::<Vec<_>>(), rows.concat());
    }

    /// Samples of 1 to 7 bits, one, two or three per pixel, unpacked to a byte each with rows that
    /// end mid-byte.
    #[test]
    fn unpacks_sub_byte_samples_to_bytes() {
        for bits in 1u16..8 {
            for spp in [1usize, 3] {
                let (w, h) = (5, 3);
                let max = (1u32 << bits) - 1;
                let rows: Vec<Vec<u32>> =
                    (0..h).map(|r| (0..w * spp).map(|c| (r as u32 * 7 + c as u32 * 5 + 1) % (max + 1)).collect()).collect();
                let codec = packed(bits, spp);
                let bytes = codec.unpack(&pack(&rows, bits as usize), w, h).unwrap();
                let DecodingResult::U8(got) = codec.result(&bytes, w, w, h).unwrap() else { panic!("{bits} bits: not Byte") };
                let want: Vec<u8> = rows.concat().into_iter().map(|v| v as u8).collect();
                assert_eq!(got, want, "{bits} bits, {spp} samples");
            }
        }
    }

    /// A little-endian classic TIFF of one strip: `width` x `height` samples of `bits` bits,
    /// `photometric`, its strip `data` as stored under `compression`.
    fn tiff_strip(width: u16, height: u16, bits: u16, compression: u16, photometric: u16, data: &[u8]) -> Vec<u8> {
        tiff_strip_with(width, height, bits, compression, photometric, &[], data)
    }

    /// As [`tiff_strip`], with the tags `more` (numbered above StripByteCounts, in order).
    fn tiff_strip_with(
        width: u16,
        height: u16,
        bits: u16,
        compression: u16,
        photometric: u16,
        more: &[(u16, u16, u32)],
        data: &[u8],
    ) -> Vec<u8> {
        let mut tags: Vec<(u16, u16, u32)> = vec![
            (256, 3, u32::from(width)),
            (257, 3, u32::from(height)),
            (258, 3, u32::from(bits)),
            (259, 3, u32::from(compression)),
            (262, 3, u32::from(photometric)),
            (273, 4, 0),
            (277, 3, 1),
            (278, 3, u32::from(height)),
            (279, 4, data.len() as u32),
        ];
        tags.extend_from_slice(more);
        let data_offset = 8 + 2 + tags.len() as u32 * 12 + 4;
        let mut b = b"II*\0".to_vec();
        b.extend(8u32.to_le_bytes());
        b.extend((tags.len() as u16).to_le_bytes());
        for (tag, typ, value) in tags {
            b.extend(tag.to_le_bytes());
            b.extend(typ.to_le_bytes());
            b.extend(1u32.to_le_bytes());
            b.extend((if tag == 273 { data_offset } else { value }).to_le_bytes());
        }
        b.extend(0u32.to_le_bytes());
        b.extend(data);
        b
    }

    /// Bilevel and 2- and 4-bit images are decoded by terrio, a byte per sample, their values as
    /// stored (WhiteIsZero ones too, as libtiff and GDAL read them), uncompressed or compressed.
    /// The `tiff` crate gives such chunks packed; read as they come, each byte would be a pixel.
    #[test]
    fn sub_byte_images_are_read_a_byte_per_sample() {
        let (w, h) = (13u16, 3u16);
        for bits in [1u16, 2, 4] {
            let max = (1u32 << bits) - 1;
            let rows: Vec<Vec<u32>> = (0..h).map(|r| (0..w).map(|c| (u32::from(r) * 5 + u32::from(c) * 3) % (max + 1)).collect()).collect();
            let want: Vec<u8> = rows.concat().into_iter().map(|v| v as u8).collect();
            let raw = pack(&rows, bits as usize);
            for (compression, data) in [(1u16, raw.clone()), (5, lzw_encode(&raw))] {
                for photometric in [1u16, 0] {
                    let file = tiff_strip(w, h, bits, compression, photometric, &data);
                    let mut dec = Decoder::new(std::io::Cursor::new(file)).unwrap();
                    let codec = ChunkCodec::new(&mut dec, usize::MAX, None).unwrap().expect("terrio decodes sub-byte samples");
                    let DecodingResult::U8(got) = codec.read(&mut dec, 0).unwrap() else { panic!("not Byte") };
                    assert_eq!(got, want, "{bits} bits, compression {compression}, photometric {photometric}");
                }
            }
        }
    }

    /// WhiteIsZero UInt16 samples, which the `tiff` crate inverts, are decoded by terrio as stored
    /// (as libtiff gives them to GDAL); uncompressed and PackBits ones whatever their `Predictor`
    /// tag says, libtiff's predictor being a part of the codecs that take one (LZW, Deflate).
    #[test]
    fn white_is_zero_samples_are_read_as_stored_whatever_the_predictor_tag() {
        let (w, h) = (6u16, 2u16);
        let values: Vec<u16> = (0..w * h).map(|i| i * 4099 % 65521).collect();
        let raw: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        // One PackBits literal run: its length less one, then the bytes.
        let packbits: Vec<u8> = [vec![raw.len() as u8 - 1], raw.clone()].concat();
        for (compression, data) in [(1u16, raw), (32773, packbits)] {
            for predictor in [None, Some(2u32)] {
                let more: Vec<(u16, u16, u32)> = predictor.map(|p| (317, 3, p)).into_iter().collect();
                let file = tiff_strip_with(w, h, 16, compression, 0, &more, &data);
                let mut dec = Decoder::new(std::io::Cursor::new(file)).unwrap();
                let codec = ChunkCodec::new(&mut dec, usize::MAX, None).unwrap().expect("terrio decodes WhiteIsZero samples");
                let DecodingResult::U16(got) = codec.read(&mut dec, 0).unwrap() else { panic!("not UInt16") };
                assert_eq!(got, values, "compression {compression}, predictor {predictor:?}");
            }
        }
    }

    #[test]
    fn decompresses_packbits() {
        // A literal run of 3, a repeat of 4, the -128 no-op, a literal of 1.
        let raw = vec![2, 1, 2, 3, 0xFD, 9, 0x80, 0, 7];
        assert_eq!(decompress(32773, raw.clone(), 8).unwrap(), vec![1, 2, 3, 9, 9, 9, 9, 7]);
        // A chunk that ends short is an error; one that would expand further is cut.
        assert!(decompress(32773, raw.clone(), 9).is_err());
        assert_eq!(decompress(32773, raw, 4).unwrap(), vec![1, 2, 3, 9]);
    }

    #[test]
    fn floating_point_predictor_is_undone() {
        let codec = ChunkCodec {
            codec: Codec::Lzma,
            tables: Arc::default(),
            tiled: false,
            chunk: (2, 1),
            spp: 1,
            sample: Sample::F32,
            big_endian: false,
            predictor: 3,
            bits: 32,
            lsb_first: false,
            lerc_wrapper: 0,
            lerc_alpha: false,
            jpeg_tables: Vec::new(),
            ycbcr: false,
            white_is_zero: false,
            limit: usize::MAX,
        };
        // 1.5 and -2.0, as byte planes (most significant first), then differenced.
        let (a, b) = (1.5f32.to_be_bytes(), (-2.0f32).to_be_bytes());
        let planes = [a[0], b[0], a[1], b[1], a[2], b[2], a[3], b[3]];
        let mut row: Vec<u8> = (0..8).map(|i| if i == 0 { planes[0] } else { planes[i].wrapping_sub(planes[i - 1]) }).collect();
        codec.unpredict(&mut row, 2);
        let values: Vec<f32> = row.as_chunks::<4>().0.iter().map(|c| f32::from_ne_bytes(*c)).collect();
        assert_eq!(values, [1.5, -2.0]);
    }
}

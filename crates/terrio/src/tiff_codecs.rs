//! TIFF compressions the `tiff` crate does not decode, read as GDAL (libtiff) reads them: LERC
//! (alone, or wrapped in Deflate or Zstandard), JPEG XL and LZMA. And JPEG, decoded as libjpeg
//! decodes it for GDAL ([`crate::jpeg`]), YCbCr converted to RGB. And unsigned samples packed in
//! 9 to 31 bits (GDAL's `NBITS`, as written for Sentinel-2's 15-bit bands), in any of the
//! lossless compressions, read as UInt16 or UInt32 as GDAL reads them.

use std::io::{Read, Seek, SeekFrom};
use tiff::decoder::{ChunkType, Decoder, DecodingResult};
use tiff::tags::Tag;
use tiff::{TiffError, TiffResult};

/// The TIFF compression codes handled here.
const LERC: u16 = 34887;
const LZMA: u16 = 34925;
/// JPEG XL, as GDAL first wrote it, then as DNG 1.7 registered it.
const JXL: u16 = 50002;
const JXL_DNG: u16 = 52546;
/// The tag of LERC's parameters: its version and the compression wrapping it.
const LERC_PARAMETERS: u16 = 50674;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Lerc,
    Jxl,
    Lzma,
    /// LZW of 8 to 64-bit samples, decoded whole: the `tiff` crate's reader fails on some chunks
    /// it reads row by row (edge tiles narrower than the tile).
    Lzw,
    Jpeg,
    /// Unsigned samples of 9 to 31 bits (not 16), packed MSB first with each row starting on a
    /// byte, under compression `.0`.
    Packed(u16),
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

    fn size(self) -> usize {
        match self {
            Self::U8 | Self::I8 => 1,
            Self::U16 | Self::I16 => 2,
            Self::U32 | Self::I32 | Self::F32 => 4,
            Self::U64 | Self::I64 | Self::F64 => 8,
        }
    }

    fn is_float(self) -> bool {
        matches!(self, Self::F32 | Self::F64)
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
                    out.resize(limit.min(written.max(1 << 16) * 2), 0);
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
        50000 => {
            let mut src = &raw[..];
            let mut z = ruzstd::decoding::StreamingDecoder::new(&mut src).map_err(|e| invalid(format!("Zstandard: {e}")))?;
            bounded(&mut z, &mut out)?;
        }
        32773 => {
            // PackBits: a count byte n, then n + 1 literal bytes (n >= 0) or one byte repeated
            // 1 - n times (n < 0, -128 skipped).
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
    offsets: Vec<u64>,
    counts: Vec<u64>,
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
}

impl ChunkCodec {
    /// The codec for the image the decoder is positioned on, `None` when the `tiff` crate decodes
    /// its chunks itself.
    pub(crate) fn new<R: Read + Seek>(dec: &mut Decoder<R>) -> TiffResult<Option<Self>> {
        let compression = dec.find_tag_unsigned::<u16>(Tag::Compression)?.unwrap_or(1);
        let first = |v: Option<Vec<u16>>, default: u16| v.and_then(|v| v.first().copied()).unwrap_or(default);
        let packed_bits = first(dec.find_tag_unsigned_vec::<u16>(Tag::BitsPerSample)?, 1);
        let codec = match compression {
            _ if (9..32).contains(&packed_bits) && packed_bits != 16 => Codec::Packed(compression),
            LERC => Codec::Lerc,
            JXL | JXL_DNG => Codec::Jxl,
            LZMA => Codec::Lzma,
            5 => Codec::Lzw,
            7 => Codec::Jpeg,
            _ => return Ok(None),
        };
        let tiled = dec.get_chunk_type() == ChunkType::Tile;
        let (offsets, counts) = if tiled {
            (dec.get_tag_u64_vec(Tag::TileOffsets)?, dec.get_tag_u64_vec(Tag::TileByteCounts)?)
        } else {
            (dec.get_tag_u64_vec(Tag::StripOffsets)?, dec.get_tag_u64_vec(Tag::StripByteCounts)?)
        };
        let (cw, ch) = dec.chunk_dimensions();
        let samples = dec.find_tag_unsigned::<u16>(Tag::SamplesPerPixel)?.unwrap_or(1).max(1) as usize;
        let planar = dec.find_tag_unsigned::<u16>(Tag::PlanarConfiguration)? == Some(2);
        let bits = packed_bits;
        let format = first(dec.find_tag_unsigned_vec::<u16>(Tag::SampleFormat)?, 1);
        let spp = if planar { 1 } else { samples };
        // JPEG of 8-bit samples: 12-bit JPEG is left to the `tiff` crate.
        if codec == Codec::Jpeg && bits != 8 {
            return Ok(None);
        }
        // LZW: other sample types, and WhiteIsZero images (which the crate inverts), are left to
        // the `tiff` crate.
        if codec == Codec::Lzw
            && (Sample::new(format, bits).is_none() || dec.find_tag_unsigned::<u16>(Tag::PhotometricInterpretation)? == Some(0))
        {
            return Ok(None);
        }
        let sample = match codec {
            Codec::Packed(_) if format != 1 => {
                return Err(invalid(format!("TIFF samples of {bits} bits (format {format}) are not supported")));
            }
            Codec::Packed(_) => {
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
        // libtiff's predictors need 8, 16, 32 or 64-bit samples.
        if matches!(codec, Codec::Packed(_)) && predictor != 1 {
            return Err(invalid(format!("TIFF predictor {predictor} with {bits}-bit samples is not supported")));
        }
        let extra = dec.find_tag_unsigned_vec::<u16>(Tag::ExtraSamples)?.unwrap_or_default();
        let lerc_wrapper = match codec {
            Codec::Lerc => dec.find_tag_unsigned_vec::<u32>(Tag::Unknown(LERC_PARAMETERS))?.and_then(|p| p.get(1).copied()).unwrap_or(0),
            _ => 0,
        };
        Ok(Some(Self {
            codec,
            offsets,
            counts,
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
            ycbcr: codec == Codec::Jpeg && dec.find_tag_unsigned::<u16>(Tag::PhotometricInterpretation)? == Some(6),
        }))
    }

    /// Decodes chunk `idx`, as `Decoder::read_chunk` does: the samples of the chunk's image data,
    /// without padding.
    pub(crate) fn read<R: Read + Seek>(&self, dec: &mut Decoder<R>, idx: u32) -> TiffResult<DecodingResult> {
        let i = idx as usize;
        let (Some(&offset), Some(&count)) = (self.offsets.get(i), self.counts.get(i)) else {
            return Err(invalid(format!("no TIFF chunk {idx}")));
        };
        let mut raw = vec![0u8; usize::try_from(count).map_err(|_| invalid("TIFF chunk too large"))?];
        let r = dec.inner();
        r.seek(SeekFrom::Start(offset))?;
        r.read_exact(&mut raw)?;
        let (dw, dh) = dec.chunk_data_dimensions(idx);
        let (dw, dh) = (dw as usize, dh as usize);
        // Tiles are coded whole, strips with their own rows.
        let (w, h) = if self.tiled { self.chunk } else { (dw, dh) };
        let bytes = match self.codec {
            Codec::Lzma | Codec::Lzw => self.lossless(raw, w, h)?,
            Codec::Lerc => self.lerc(raw, w, h)?,
            Codec::Jxl => self.jxl(&raw, w, h)?,
            Codec::Jpeg => self.jpeg(&raw, w, h)?,
            Codec::Packed(compression) => {
                let row_bytes = (w * self.spp).checked_mul(usize::from(self.bits)).map(|b| b.div_ceil(8));
                let size = row_bytes.and_then(|r| r.checked_mul(h)).ok_or_else(|| invalid("TIFF chunk too large"))?;
                self.unpack(&decompress(compression, raw, size)?, w, h)
            }
        };
        Ok(self.result(&bytes, w, dw, dh))
    }

    /// Decodes an LZMA or LZW chunk of `h` rows of `w` pixels into native-endian samples, its
    /// predictor undone.
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
            decompress(LZMA, raw, n)?
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

    /// Unpacks `h` rows of `w` pixels of samples of `self.bits` bits, MSB first with each row
    /// starting on a byte (as libtiff packs them; with `FillOrder` 2 each byte's bits are
    /// reversed first), into native-endian samples of the output type.
    fn unpack(&self, packed: &[u8], w: usize, h: usize) -> Vec<u8> {
        let bits = usize::from(self.bits);
        let per_row = w * self.spp;
        let row_bytes = (per_row * bits).div_ceil(8);
        let size = self.sample.size();
        let mut out = vec![0u8; h * per_row * size];
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
                if size == 2 {
                    out[at..at + 2].copy_from_slice(&(v as u16).to_ne_bytes());
                } else {
                    out[at..at + 4].copy_from_slice(&v.to_ne_bytes());
                }
            }
        }
        out
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

    /// Decodes LERC blobs into native-endian samples, as libtiff's LERC codec does: masked pixels
    /// of floating-point images are NaN, a mask coding the alpha channel becomes 0 or 255.
    #[cfg(feature = "lerc")]
    fn lerc(&self, raw: Vec<u8>, w: usize, h: usize) -> TiffResult<Vec<u8>> {
        let blob = match self.lerc_wrapper {
            0 => raw,
            1 => {
                let mut out = Vec::new();
                flate2::read::ZlibDecoder::new(&raw[..]).read_to_end(&mut out)?;
                out
            }
            2 => {
                let mut out = Vec::new();
                let mut src = &raw[..];
                ruzstd::decoding::StreamingDecoder::new(&mut src).map_err(|e| invalid(format!("Zstandard: {e}")))?.read_to_end(&mut out)?;
                out
            }
            other => return Err(invalid(format!("unknown LERC compression wrapper {other}"))),
        };
        let lerc_err = |e: lerc_core::Error| invalid(format!("LERC: {e}"));
        let n = w * h;
        let bands = lerc_reader::get_band_count(&blob).map_err(lerc_err)?;
        // Samples interleaved by pixel, with a validity mask per sample.
        let (mut samples, masks): (Vec<f64>, Vec<Option<Vec<u8>>>) = if bands > 1 {
            // A floating-point image with one blob per sample.
            let set = lerc_reader::decode_band_set(&blob).map_err(lerc_err)?;
            let (_, planes, masks) = set.into_parts();
            let planes: Vec<Vec<f64>> = planes.iter().map(pixels_f64).collect();
            let mut out = vec![0.0; n * bands];
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
            let mut out = Vec::with_capacity(n * self.spp);
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
        Ok(self.to_bytes(&samples))
    }

    #[cfg(not(feature = "lerc"))]
    fn lerc(&self, _raw: Vec<u8>, _w: usize, _h: usize) -> TiffResult<Vec<u8>> {
        Err(disabled("lerc"))
    }

    /// Decodes a JPEG XL codestream into native-endian samples: the colour channels, then the extra
    /// channels, interleaved by pixel, as libtiff's JPEG XL codec does.
    fn jxl(&self, raw: &[u8], w: usize, h: usize) -> TiffResult<Vec<u8>> {
        use jxl_oxide::image::BitDepth;
        let image = jxl_oxide::JxlImage::builder().read(raw).map_err(|e| invalid(format!("JPEG XL: {e}")))?;
        let render = image.render_frame(0).map_err(|e| invalid(format!("JPEG XL: {e}")))?;
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
        let mut samples = vec![0.0f64; n * self.spp];
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
        Ok(self.to_bytes(&samples))
    }

    /// Decodes a JPEG chunk, its stream completed with the shared tables.
    fn jpeg(&self, raw: &[u8], w: usize, h: usize) -> TiffResult<Vec<u8>> {
        // The tables without their end marker, then the chunk without its start marker.
        let stream = match (self.jpeg_tables.len() >= 4, raw.len() >= 2) {
            (true, true) => [&self.jpeg_tables[..self.jpeg_tables.len() - 2], &raw[2..]].concat(),
            _ => raw.to_vec(),
        };
        let color = if self.ycbcr { crate::jpeg::Color::Rgb } else { crate::jpeg::Color::Raw };
        let image = crate::jpeg::decode(&stream, color).map_err(|e| invalid(format!("JPEG: {e}")))?;
        let (jw, jh, spp, pixels) = (image.width, image.height, image.components, image.data);
        if spp != self.spp || jw < w || jh < h {
            return Err(invalid(format!("JPEG: a {jw} x {jh} image of {spp} components for a {w} x {h} chunk of {} samples", self.spp)));
        }
        Ok(pixels.chunks_exact(jw * spp).take(h).flat_map(|row| &row[..w * spp]).copied().collect())
    }

    /// Native-endian bytes of samples of the image's type.
    fn to_bytes(&self, samples: &[f64]) -> Vec<u8> {
        let mut out = Vec::with_capacity(samples.len() * self.sample.size());
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
                Sample::F32 => out.extend((v as f32).to_ne_bytes()),
                Sample::F64 => out.extend(v.to_ne_bytes()),
            }
        }
        out
    }

    /// The `dw` x `dh` image data of a chunk decoded `w` samples wide, typed.
    fn result(&self, bytes: &[u8], w: usize, dw: usize, dh: usize) -> DecodingResult {
        let size = self.sample.size();
        let (row, keep) = (w * self.spp * size, dw * self.spp * size);
        let mut data = Vec::with_capacity(dh * keep);
        for r in 0..dh {
            let start = r * row;
            data.extend_from_slice(bytes.get(start..start + keep).unwrap_or(&[]));
        }
        data.resize(dh * keep, 0);
        macro_rules! typed {
            ($variant:ident, $t:ty) => {
                DecodingResult::$variant(data.chunks_exact(size).map(|c| <$t>::from_ne_bytes(c.try_into().unwrap())).collect())
            };
        }
        match self.sample {
            Sample::U8 => DecodingResult::U8(data),
            Sample::I8 => typed!(I8, i8),
            Sample::U16 => typed!(U16, u16),
            Sample::I16 => typed!(I16, i16),
            Sample::U32 => typed!(U32, u32),
            Sample::I32 => typed!(I32, i32),
            Sample::U64 => typed!(U64, u64),
            Sample::I64 => typed!(I64, i64),
            Sample::F32 => typed!(F32, f32),
            Sample::F64 => typed!(F64, f64),
        }
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

    /// A truncated LZW chunk is an error, and a header's chunk size overflowing is refused.
    #[test]
    fn short_and_oversized_lzw_chunks_are_errors() {
        let c = lzw(Sample::U8, 1, 1, false);
        assert!(c.lossless(lzw_encode(&[1, 2, 3]), 4, 4).is_err());
        assert!(c.lossless(lzw_encode(&[1; 16]), 4, 4).is_ok());
        assert!(c.lossless(lzw_encode(&[1]), usize::MAX, 2).is_err());
    }

    fn packed(bits: u16, spp: usize) -> ChunkCodec {
        ChunkCodec {
            codec: Codec::Packed(1),
            offsets: vec![],
            counts: vec![],
            tiled: false,
            chunk: (0, 0),
            spp,
            sample: if bits <= 16 { Sample::U16 } else { Sample::U32 },
            big_endian: false,
            predictor: 1,
            bits,
            lsb_first: false,
            lerc_wrapper: 0,
            lerc_alpha: false,
            jpeg_tables: vec![],
            ycbcr: false,
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
            let bytes = codec.unpack(&pack(&rows, bits as usize), 5, 3);
            let got: Vec<u32> = match codec.result(&bytes, 5, 5, 3) {
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
        let DecodingResult::U16(got) = codec.result(&codec.unpack(&lsb, 4, 2), 4, 4, 2) else { unreachable!() };
        assert_eq!(got.iter().map(|v| u32::from(*v)).collect::<Vec<_>>(), rows.concat());
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
            offsets: vec![],
            counts: vec![],
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

//! The rows of a TIFF strip, read a few at a time. An image stored as one strip (TIFF 6.0 allows
//! it, and an image without `RowsPerStrip` is one) can decode to more than the chunk limit
//! ([`crate::TiffReadLimits::chunk_bytes`]), while a window needs only some of its rows.
//!
//! Uncompressed rows are read at their offsets in the file. A compressed strip is decoded as a
//! stream from its start: the rows before the ones wanted are decoded and dropped as they come,
//! and decoding stops after the last row wanted. A read then holds the rows it reads and a bounded
//! working buffer, not the strip: the stored bytes, read 64 KiB first and twice as many each time
//! after, up to 8 MiB at a time; and the decoder's own state, a 32 KiB window and 1 MiB of
//! decoded bytes for Deflate, its code table and 64 KiB for LZW, the frame's window (at most
//! 128 MiB: larger ones are refused) and 64 KiB for Zstandard, none for PackBits.
//!
//! Deflate (compression 8 and 32946), LZW (5), Zstandard (50000) and PackBits (32773) are decoded
//! so, with horizontal differencing (predictor 2) on integer samples and the floating-point
//! predictor (3) on floating-point ones: both apply to a row at a time. A strip of another
//! compression is decoded whole: JPEG, WebP, LERC and JPEG XL code an image rather than a stream
//! of rows, and an LZMA stream needs a dictionary as large as the stream declares.

use std::io::{Read, Seek};
use tiff::decoder::{Decoder, DecodingResult};
use tiff::tags::Tag;
use wbraster::DataType;

use crate::dataset::fallible;
use crate::{Error, Result};

/// The stored bytes of a compressed strip read first, then twice as many each time, up to
/// [`LAST_PART`]: a window at the top of the strip reads few of them.
const FIRST_PART: usize = 64 << 10;
const LAST_PART: usize = 8 << 20;

/// What a Deflate stream may refer back to, and the bytes inflated past it at a time.
const DEFLATE_WINDOW: usize = 32 << 10;
const INFLATE_SPAN: usize = 1 << 20;

/// The bytes of an LZW or Zstandard stream decoded at a time to be dropped (rows before the ones
/// read).
const DROP_BYTES: usize = 64 << 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Codec {
    Stored,
    Deflate,
    Lzw,
    Zstd,
    PackBits,
}

impl Codec {
    fn name(self) -> &'static str {
        match self {
            Codec::Stored => "uncompressed",
            Codec::Deflate => "Deflate",
            Codec::Lzw => "LZW",
            Codec::Zstd => "Zstandard",
            Codec::PackBits => "PackBits",
        }
    }

    /// The most bytes a stored byte decodes to, from the formats' definitions: Deflate's longest
    /// match (258 bytes) takes two bits at the least (a length and a distance code of one bit);
    /// an LZW code takes 9 bits at the least and gives at most 4,096 bytes (its table's); a
    /// PackBits run gives 128 bytes from two; a Zstandard RLE block gives at most 128 KiB from four
    /// (its 3-byte header and the byte).
    fn most_per_byte(self) -> u64 {
        match self {
            Codec::Stored => 1,
            Codec::Deflate => 1032,
            Codec::Lzw => (4096 * 8u64).div_ceil(9),
            Codec::Zstd => 32 << 10,
            Codec::PackBits => 64,
        }
    }
}

/// How the strips of a TIFF image are read by rows: their compression, and how their samples are
/// stored.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rows {
    codec: Codec,
    data_type: DataType,
    /// The samples are stored in the byte order other than the host's.
    swap: bool,
    predictor: u16,
}

impl Rows {
    /// How the strips of the decoder's image, of samples of `data_type` under `compression`, are
    /// read by rows; `None` when they are not. They are when each row's bytes decode to its
    /// samples as a whole decode gives them: whole-byte samples of `data_type`, most significant
    /// bit first, of a photometric interpretation read as stored (BlackIsZero, RGB or palette;
    /// WhiteIsZero is not), uncompressed or under one of the compressions above, with no predictor,
    /// or one of those above (compressed by Deflate, LZW or Zstandard).
    pub(crate) fn of<R: Read + Seek>(dec: &mut Decoder<R>, data_type: DataType, compression: u16) -> tiff::TiffResult<Option<Self>> {
        let codec = match compression {
            1 => Codec::Stored,
            8 | 32946 => Codec::Deflate,
            5 => Codec::Lzw,
            50000 => Codec::Zstd,
            32773 => Codec::PackBits,
            _ => return Ok(None),
        };
        let format = match data_type {
            DataType::U8 | DataType::U16 | DataType::U32 | DataType::U64 => 1,
            DataType::I8 | DataType::I16 | DataType::I32 | DataType::I64 => 2,
            DataType::F32 | DataType::F64 => 3,
        };
        let bits = (data_type.size_bytes() * 8) as u16;
        let predictor = dec.find_tag_unsigned::<u16>(Tag::Predictor)?.unwrap_or(1);
        let predicts = matches!(codec, Codec::Deflate | Codec::Lzw | Codec::Zstd);
        let predicted = match predictor {
            1 => true,
            2 => predicts && format != 3,
            3 => predicts && format == 3,
            _ => false,
        };
        let read = predicted
            && dec.find_tag_unsigned::<u16>(Tag::FillOrder)?.unwrap_or(1) == 1
            && matches!(dec.find_tag_unsigned::<u16>(Tag::PhotometricInterpretation)?, Some(1..=3))
            && dec.find_tag_unsigned_vec::<u16>(Tag::BitsPerSample)?.is_some_and(|v| !v.is_empty() && v.iter().all(|b| *b == bits))
            && dec.find_tag_unsigned_vec::<u16>(Tag::SampleFormat)?.unwrap_or_default().iter().all(|f| *f == format);
        let big_endian = dec.byte_order() == tiff::tags::ByteOrder::BigEndian;
        Ok(read.then_some(Self { codec, data_type, swap: big_endian != cfg!(target_endian = "big"), predictor }))
    }

    /// Whether a strip of `strip` bytes of rows, stored as `(offset, byte count)` in a file of
    /// `len` bytes, is read by rows: the file holds its stored bytes, and they hold its rows
    /// (uncompressed) or can decode to them (compressed: at most so many bytes per stored byte,
    /// [`Codec::most_per_byte`]). A header declaring more is decoded whole, and so refused over
    /// the limit, as before.
    pub(crate) fn holds(&self, (offset, count): (u64, u64), strip: u64, len: u64) -> bool {
        offset.checked_add(count).is_some_and(|end| end <= len) && count.saturating_mul(self.codec.most_per_byte()) >= strip
    }

    /// The rows of the strip stored as `(offset, byte count)`, of `height` rows of `width` pixels
    /// of `spp` samples.
    pub(crate) fn strip(&self, (offset, count): (u64, u64), width: usize, spp: usize, height: usize) -> StripRows {
        let row_bytes = width * spp * self.data_type.size_bytes();
        StripRows {
            rows: *self,
            offset,
            width,
            spp,
            row_bytes,
            height,
            next: 0,
            parts: Parts { next: offset, end: offset.saturating_add(count), size: FIRST_PART, buf: Vec::new(), pos: 0 },
            stream: None,
        }
    }
}

/// A buffer of `len` samples of `data_type`, allocated fallibly.
pub(crate) fn samples(data_type: DataType, len: usize) -> Result<DecodingResult> {
    let what = "TIFF strip rows";
    Ok(match data_type {
        DataType::U8 => DecodingResult::U8(fallible::filled_now(len, 0, what)?),
        DataType::I8 => DecodingResult::I8(fallible::filled_now(len, 0, what)?),
        DataType::U16 => DecodingResult::U16(fallible::filled_now(len, 0, what)?),
        DataType::I16 => DecodingResult::I16(fallible::filled_now(len, 0, what)?),
        DataType::U32 => DecodingResult::U32(fallible::filled_now(len, 0, what)?),
        DataType::I32 => DecodingResult::I32(fallible::filled_now(len, 0, what)?),
        DataType::U64 => DecodingResult::U64(fallible::filled_now(len, 0, what)?),
        DataType::I64 => DecodingResult::I64(fallible::filled_now(len, 0, what)?),
        DataType::F32 => DecodingResult::F32(fallible::filled_now(len, 0.0, what)?),
        DataType::F64 => DecodingResult::F64(fallible::filled_now(len, 0.0, what)?),
    })
}

/// Shortens `buf` to `len` samples.
pub(crate) fn truncate(buf: &mut DecodingResult, len: usize) {
    match buf {
        DecodingResult::U8(v) => v.truncate(len),
        DecodingResult::I8(v) => v.truncate(len),
        DecodingResult::U16(v) => v.truncate(len),
        DecodingResult::I16(v) => v.truncate(len),
        DecodingResult::U32(v) => v.truncate(len),
        DecodingResult::I32(v) => v.truncate(len),
        DecodingResult::U64(v) => v.truncate(len),
        DecodingResult::I64(v) => v.truncate(len),
        DecodingResult::F16(v) => v.truncate(len),
        DecodingResult::F32(v) => v.truncate(len),
        DecodingResult::F64(v) => v.truncate(len),
    }
}

/// The rows of one strip ([`Rows::strip`]).
pub(crate) struct StripRows {
    rows: Rows,
    offset: u64,
    width: usize,
    spp: usize,
    row_bytes: usize,
    height: usize,
    /// The next row a compressed strip's stream decodes.
    next: usize,
    parts: Parts,
    stream: Option<Stream>,
}

impl StripRows {
    /// Reads the rows from `first` into `out`, as many as it holds, as samples in the host's byte
    /// order, their predictor undone. `read_at(offset, buf)` reads the file's bytes at `offset`
    /// into `buf`, whole. A compressed strip is decoded in order: the rows between those read so
    /// far and `first` are decoded and dropped, and a row before them is not read again.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidData`] for a stream that is not valid or ends before the rows, as a whole
    /// decode refuses it; `read_at`'s errors as they are.
    pub(crate) fn read<F>(&mut self, read_at: &mut F, first: usize, out: &mut DecodingResult) -> Result<()>
    where
        F: FnMut(u64, &mut [u8]) -> Result<()>,
    {
        let mut buffer = out.as_buffer(0);
        let bytes = buffer.as_bytes_mut();
        if self.rows.codec == Codec::Stored {
            // Saturating: a strip `Rows::holds` passes fits in 64 bits, and no size here wraps.
            read_at(self.offset.saturating_add((first as u64).saturating_mul(self.row_bytes as u64)), bytes)?;
        } else {
            self.decode(read_at, first, bytes)?;
        }
        let Rows { data_type, swap, predictor, .. } = self.rows;
        let size = data_type.size_bytes();
        // The floating-point predictor's byte planes give samples in the host's byte order.
        if swap && size > 1 && predictor != 3 {
            for sample in bytes.chunks_exact_mut(size) {
                sample.reverse();
            }
        }
        crate::tiff_codecs::unpredict(bytes, self.width, self.spp, size, predictor);
        Ok(())
    }

    /// Decodes the rest of a compressed strip, dropping it, as a whole decode decodes all of it: a
    /// stream that ends before the strip's last row is refused, and a Deflate stream's Adler-32 is
    /// checked.
    ///
    /// # Errors
    ///
    /// As [`StripRows::read`].
    pub(crate) fn finish<F>(&mut self, read_at: &mut F) -> Result<()>
    where
        F: FnMut(u64, &mut [u8]) -> Result<()>,
    {
        if self.rows.codec == Codec::Stored || self.next == self.height {
            return Ok(());
        }
        self.decode(read_at, self.height, &mut [])
    }

    /// Decodes a compressed strip's rows from `first` into `bytes`, dropping the rows before them.
    fn decode<F>(&mut self, read_at: &mut F, first: usize, bytes: &mut [u8]) -> Result<()>
    where
        F: FnMut(u64, &mut [u8]) -> Result<()>,
    {
        if first < self.next {
            return Err(Error::InvalidArgument(format!("TIFF strip row {first} read after row {}", self.next)));
        }
        // Saturating: a strip `Rows::holds` passes fits in 64 bits, and no size here wraps.
        let row_bytes = self.row_bytes as u64;
        let stream = match &mut self.stream {
            Some(stream) => stream,
            None => self.stream.insert(Stream::new(self.rows.codec, (self.height as u64).saturating_mul(row_bytes))?),
        };
        let skip = ((first - self.next) as u64).saturating_mul(row_bytes);
        let dropped = stream.drop_bytes(&mut self.parts, read_at, skip)?;
        let read = if dropped == skip { stream.read(&mut self.parts, read_at, bytes)? } else { 0 };
        if dropped < skip || read < bytes.len() {
            let decoded = (self.next as u64).saturating_mul(row_bytes).saturating_add(dropped).saturating_add(read as u64);
            return Err(Error::InvalidData(format!(
                "TIFF: {}: the strip ends after {} of its {} rows",
                self.rows.codec.name(),
                decoded / row_bytes,
                self.height
            )));
        }
        self.next = first + bytes.len() / self.row_bytes;
        if self.next == self.height {
            stream.end(&mut self.parts, read_at)?;
        }
        Ok(())
    }
}

/// The stored bytes of a compressed strip, read a part at a time.
struct Parts {
    /// The offset of the stored bytes not read yet, and the end of the strip's.
    next: u64,
    end: u64,
    /// The size of the next part.
    size: usize,
    buf: Vec<u8>,
    /// `buf[pos..]` is not decoded yet.
    pos: usize,
}

impl Parts {
    /// Reads the next part when all the bytes read are decoded.
    fn load<F>(&mut self, read_at: &mut F) -> Result<()>
    where
        F: FnMut(u64, &mut [u8]) -> Result<()>,
    {
        if self.pos == self.buf.len() && self.next < self.end {
            let n = (self.end - self.next).min(self.size as u64) as usize;
            self.buf.clear();
            fallible::reserve(&mut self.buf, n, "a TIFF strip's stored bytes")?;
            self.buf.resize(n, 0);
            read_at(self.next, &mut self.buf)?;
            (self.next, self.pos, self.size) = (self.next + n as u64, 0, (self.size * 2).min(LAST_PART));
        }
        Ok(())
    }

    /// The bytes read and not decoded yet: after [`Parts::load`], empty only once the strip's
    /// stored bytes are all decoded.
    fn held(&self) -> &[u8] {
        &self.buf[self.pos..]
    }

    fn consume(&mut self, n: usize) {
        self.pos += n;
    }

    /// Whether the bytes held are the strip's last.
    fn last(&self) -> bool {
        self.next >= self.end
    }

    /// Skips `n` stored bytes (a skippable Zstandard frame).
    fn skip(&mut self, n: u64) -> Result<()> {
        let held = (self.buf.len() - self.pos) as u64;
        if n <= held {
            self.pos += n as usize;
            return Ok(());
        }
        self.pos = self.buf.len();
        let rest = n - held;
        if rest > self.end - self.next {
            return Err(Error::InvalidData("TIFF: Zstandard: truncated skippable frame".into()));
        }
        self.next += rest;
        Ok(())
    }
}

/// [`Parts`] as a reader, for a decoder that reads its input itself: an error reading the file is
/// kept, and returned rather than the decoder's own.
struct PartsReader<'a, F> {
    parts: &'a mut Parts,
    read_at: &'a mut F,
    failed: Option<Error>,
}

impl<F: FnMut(u64, &mut [u8]) -> Result<()>> Read for PartsReader<'_, F> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if let Err(e) = self.parts.load(self.read_at) {
            self.failed = Some(e);
            return Err(std::io::Error::other("the TIFF strip's stored bytes could not be read"));
        }
        let input = self.parts.held();
        let n = input.len().min(out.len());
        out[..n].copy_from_slice(&input[..n]);
        self.parts.consume(n);
        Ok(n)
    }
}

/// A compressed strip's stream, decoded in order.
enum Stream {
    Deflate(Box<Inflate>),
    /// The decoders, and room for the bytes they drop.
    Lzw(Box<weezl::decode::Decoder>, Vec<u8>),
    Zstd(Box<Zstd>, Vec<u8>),
    PackBits(PackBits),
}

impl Stream {
    /// The stream of a strip of `strip` decoded bytes.
    fn new(codec: Codec, strip: u64) -> Result<Self> {
        let drop = || fallible::filled_now(DROP_BYTES, 0u8, "a TIFF strip's dropped rows");
        Ok(match codec {
            Codec::Deflate => Stream::Deflate(Box::new(Inflate::new(strip)?)),
            Codec::Lzw => Stream::Lzw(Box::new(weezl::decode::Decoder::with_tiff_size_switch(weezl::BitOrder::Msb, 8)), drop()?),
            Codec::Zstd => Stream::Zstd(Box::new(Zstd { frame: ruzstd::decoding::FrameDecoder::new(), in_frame: false }), drop()?),
            Codec::PackBits => Stream::PackBits(PackBits::Count),
            Codec::Stored => unreachable!("uncompressed rows are read at their offsets"),
        })
    }

    /// Decodes the next bytes into `out`, as many as it holds: fewer when the stream ends.
    fn read<F>(&mut self, parts: &mut Parts, read_at: &mut F, out: &mut [u8]) -> Result<usize>
    where
        F: FnMut(u64, &mut [u8]) -> Result<()>,
    {
        let n = out.len() as u64;
        Ok(match self {
            Stream::Deflate(z) => z.take(parts, read_at, Some(out), n)? as usize,
            Stream::Lzw(z, _) => lzw(z, parts, read_at, out)?,
            Stream::Zstd(z, _) => z.read(parts, read_at, out)?,
            Stream::PackBits(p) => p.take(parts, read_at, Some(out), n)? as usize,
        })
    }

    /// Decodes the next `n` bytes and drops them: fewer when the stream ends.
    fn drop_bytes<F>(&mut self, parts: &mut Parts, read_at: &mut F, n: u64) -> Result<u64>
    where
        F: FnMut(u64, &mut [u8]) -> Result<()>,
    {
        match self {
            Stream::Deflate(z) => z.take(parts, read_at, None, n),
            Stream::PackBits(p) => p.take(parts, read_at, None, n),
            Stream::Lzw(z, drop) => drop_into(drop, n, |buf| lzw(z, parts, read_at, buf)),
            Stream::Zstd(z, drop) => drop_into(drop, n, |buf| z.read(parts, read_at, buf)),
        }
    }

    /// Reads on after the strip's last row: a Deflate stream's Adler-32 right after it is checked,
    /// as a whole decode checks it.
    fn end<F>(&mut self, parts: &mut Parts, read_at: &mut F) -> Result<()>
    where
        F: FnMut(u64, &mut [u8]) -> Result<()>,
    {
        match self {
            Stream::Deflate(z) => z.end(parts, read_at),
            _ => Ok(()),
        }
    }
}

/// Decodes `n` bytes with `decode` into `drop`, a part at a time, and drops them: fewer when the
/// stream ends.
fn drop_into(drop: &mut [u8], n: u64, mut decode: impl FnMut(&mut [u8]) -> Result<usize>) -> Result<u64> {
    let mut done = 0u64;
    while done < n {
        let k = (n - done).min(drop.len() as u64) as usize;
        let got = decode(&mut drop[..k])?;
        done += got as u64;
        if got < k {
            break;
        }
    }
    Ok(done)
}

/// The error of a Deflate stream, as a whole decode reports it.
fn deflate_error(e: fdeflate::DecompressionError) -> Error {
    Error::InvalidData(format!("TIFF: Deflate: {e:?}"))
}

/// A Deflate (zlib) stream inflated with `fdeflate`, as whole strips are, into a window that keeps
/// the last 32 KiB the stream may refer back to.
struct Inflate {
    z: fdeflate::Decompressor,
    window: Vec<u8>,
    /// `window[..end]` is inflated; `window[given..end]` is not handed out yet.
    end: usize,
    given: usize,
    /// The strip's bytes not inflated yet: inflating stops at the strip's end, as a whole decode
    /// stops at the end of its buffer.
    left: u64,
}

impl Inflate {
    fn new(strip: u64) -> Result<Self> {
        let window = fallible::filled_now(DEFLATE_WINDOW + INFLATE_SPAN, 0u8, "a TIFF strip's Deflate window")?;
        Ok(Self { z: fdeflate::Decompressor::new(), window, end: 0, given: 0, left: strip })
    }

    /// Inflates more bytes past `given`, at most `want`: `false` when the stream ends first.
    fn inflate<F>(&mut self, parts: &mut Parts, read_at: &mut F, want: u64) -> Result<bool>
    where
        F: FnMut(u64, &mut [u8]) -> Result<()>,
    {
        if self.left == 0 {
            return Ok(false);
        }
        if self.end == self.window.len() {
            let keep = self.end.min(DEFLATE_WINDOW);
            self.window.copy_within(self.end - keep..self.end, 0);
            (self.end, self.given) = (keep, keep);
        }
        // No further than the read needs: inflating on to the end of the stream would check its
        // Adler-32 for a read that stops before the strip's last row (`end` checks it).
        let stop = self.window.len().min(self.end.saturating_add(usize::try_from(self.left.min(want)).unwrap_or(usize::MAX)));
        loop {
            parts.load(read_at)?;
            // At the end of the stored bytes, a stream that has not ended is cut short: fdeflate
            // reports it (`InsufficientInput`), as for a whole strip.
            let last = parts.last();
            let (used, made) = self.z.read(parts.held(), &mut self.window[..stop], self.end, last).map_err(deflate_error)?;
            parts.consume(used);
            self.end += made;
            self.left -= made as u64;
            if made > 0 {
                return Ok(true);
            }
            // Ended, or no progress with the bytes held: the stream ends here.
            if self.z.is_done() || used == 0 {
                return Ok(false);
            }
        }
    }

    /// Hands out the next `n` inflated bytes into `out`, or drops them: fewer when the stream ends.
    fn take<F>(&mut self, parts: &mut Parts, read_at: &mut F, mut out: Option<&mut [u8]>, n: u64) -> Result<u64>
    where
        F: FnMut(u64, &mut [u8]) -> Result<()>,
    {
        let mut done = 0u64;
        while done < n {
            if self.given == self.end && !self.inflate(parts, read_at, n - done)? {
                break;
            }
            let k = ((self.end - self.given) as u64).min(n - done) as usize;
            if let Some(out) = out.as_deref_mut() {
                out[done as usize..done as usize + k].copy_from_slice(&self.window[self.given..self.given + k]);
            }
            self.given += k;
            done += k as u64;
        }
        Ok(done)
    }

    /// Reads on after the strip's last byte, as a whole decode reads on once its buffer is full:
    /// an Adler-32 right after the samples is checked; a stream cut in it, or going on past the
    /// samples, is accepted.
    fn end<F>(&mut self, parts: &mut Parts, read_at: &mut F) -> Result<()>
    where
        F: FnMut(u64, &mut [u8]) -> Result<()>,
    {
        while !self.z.is_done() {
            parts.load(read_at)?;
            let last = parts.last();
            let (used, made) = self.z.read(parts.held(), &mut self.window[..self.end], self.end, last).map_err(deflate_error)?;
            parts.consume(used);
            if used == 0 && made == 0 {
                break;
            }
        }
        Ok(())
    }
}

/// Decodes the next bytes of an LZW stream into `out`, as many as it holds: fewer when the stream
/// ends (its end code, or the end of its stored bytes).
fn lzw<F>(z: &mut weezl::decode::Decoder, parts: &mut Parts, read_at: &mut F, out: &mut [u8]) -> Result<usize>
where
    F: FnMut(u64, &mut [u8]) -> Result<()>,
{
    let mut made = 0;
    while made < out.len() {
        parts.load(read_at)?;
        let r = z.decode_bytes(parts.held(), &mut out[made..]);
        parts.consume(r.consumed_in);
        made += r.consumed_out;
        match r.status.map_err(|e| Error::InvalidData(format!("TIFF: LZW: {e}")))? {
            weezl::LzwStatus::Done => break,
            weezl::LzwStatus::NoProgress if r.consumed_in == 0 && r.consumed_out == 0 => break,
            _ => {}
        }
    }
    Ok(made)
}

/// A Zstandard stream decoded with `ruzstd`: its frames one after another, skippable frames
/// skipped, each decoded a block at a time.
struct Zstd {
    frame: ruzstd::decoding::FrameDecoder,
    /// A frame is being decoded.
    in_frame: bool,
}

impl Zstd {
    /// Decodes the next bytes into `out`, as many as it holds: fewer when the stream ends.
    fn read<F>(&mut self, parts: &mut Parts, read_at: &mut F, out: &mut [u8]) -> Result<usize>
    where
        F: FnMut(u64, &mut [u8]) -> Result<()>,
    {
        use ruzstd::decoding::BlockDecodingStrategy;
        use ruzstd::decoding::errors::{FrameDecoderError, ReadFrameHeaderError};
        let mut src = PartsReader { parts, read_at, failed: None };
        let mut made = 0;
        while made < out.len() {
            if !self.in_frame {
                src.parts.load(src.read_at)?;
                if src.parts.held().is_empty() {
                    break;
                }
                match self.frame.init(&mut src) {
                    Ok(()) => self.in_frame = true,
                    Err(FrameDecoderError::ReadFrameHeaderError(ReadFrameHeaderError::SkipFrame { length, .. })) => {
                        src.parts.skip(u64::from(length))?;
                        continue;
                    }
                    Err(e) => return Err(src.failed.take().unwrap_or_else(|| zstd_error(e))),
                }
            }
            while self.frame.can_collect() == 0 && !self.frame.is_finished() {
                if let Err(e) = self.frame.decode_blocks(&mut src, BlockDecodingStrategy::UptoBlocks(1)) {
                    return Err(src.failed.take().unwrap_or_else(|| zstd_error(e)));
                }
            }
            let n = self.frame.read(&mut out[made..]).map_err(|e| Error::InvalidData(format!("TIFF: Zstandard: {e}")))?;
            made += n;
            if n == 0 && self.frame.is_finished() {
                self.in_frame = false;
            }
        }
        Ok(made)
    }
}

fn zstd_error(e: ruzstd::decoding::errors::FrameDecoderError) -> Error {
    Error::InvalidData(format!("TIFF: Zstandard: {e}"))
}

/// Where a PackBits stream is: before a count byte, in a literal run of so many bytes, before the
/// byte a run repeats so many times, or in such a run.
#[derive(Debug, Clone, Copy)]
enum PackBits {
    Count,
    Literal(usize),
    RunByte(usize),
    Run(u8, usize),
}

impl PackBits {
    /// Hands out the next `n` bytes into `out`, or drops them: fewer when the stream ends. A count
    /// byte n is followed by n + 1 bytes as they are (n >= 0) or by a byte repeated 1 - n times
    /// (n < 0); -128 is skipped.
    fn take<F>(&mut self, parts: &mut Parts, read_at: &mut F, mut out: Option<&mut [u8]>, n: u64) -> Result<u64>
    where
        F: FnMut(u64, &mut [u8]) -> Result<()>,
    {
        let mut done = 0u64;
        while done < n {
            if let PackBits::Run(byte, left) = *self {
                let k = (left as u64).min(n - done) as usize;
                if let Some(out) = out.as_deref_mut() {
                    out[done as usize..done as usize + k].fill(byte);
                }
                done += k as u64;
                *self = if k == left { PackBits::Count } else { PackBits::Run(byte, left - k) };
                continue;
            }
            parts.load(read_at)?;
            let input = parts.held();
            let Some(&first) = input.first() else { break };
            match *self {
                PackBits::Count => {
                    parts.consume(1);
                    *self = match first as i8 {
                        c @ 0.. => PackBits::Literal(c as usize + 1),
                        -128 => PackBits::Count,
                        c => PackBits::RunByte((1 - isize::from(c)) as usize),
                    };
                }
                PackBits::RunByte(count) => {
                    parts.consume(1);
                    *self = PackBits::Run(first, count);
                }
                PackBits::Literal(left) => {
                    let k = (left as u64).min(n - done).min(input.len() as u64) as usize;
                    if let Some(out) = out.as_deref_mut() {
                        out[done as usize..done as usize + k].copy_from_slice(&input[..k]);
                    }
                    parts.consume(k);
                    done += k as u64;
                    *self = if k == left { PackBits::Count } else { PackBits::Literal(left - k) };
                }
                PackBits::Run(..) => unreachable!("handled above"),
            }
        }
        Ok(done)
    }
}

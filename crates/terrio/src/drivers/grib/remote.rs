// Written to match the behavior of:
//   GDAL v3.12.2:
//     frmts/grib/gribdataset.cpp
//     frmts/grib/degrib/degrib/inventory.cpp
// No code is translated from these sources.

//! Remote GRIB files read by range, so that reading a band costs its message rather than the
//! file: at open, the header of each message (its sections up to the first octets of its data)
//! is read to find the next message and to describe its bands; a band's message is fetched whole
//! when the band is read. A GRIB1 bitmap is not needed to describe a band, so it is skipped: its
//! message's header costs two small requests (the sections before the bitmap, then the data
//! section's first octets) rather than a block holding the bitmap. A GRIB1 message whose nodata
//! depends on its data (degrib's `resetPrim`) is read whole at open, as GDAL reads it. Bytes
//! received: the headers, those messages, and the messages of the bands read. A server that
//! ignores ranges is read only if the file came whole with the first request (a small file):
//! the rest of the reads are then served from it.

use super::{Band, Source, Store, be, grib1, header, header_needs, message_bands, normalize_units, try_vec};
use crate::http::{ByteRange, HttpOptions, ObjectFetcher, RangeReader, Remote};
use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom};

/// The first block, read when the file is opened.
pub(crate) const BLOCK: usize = 16 * 1024;

/// The bytes first requested for the header of each message after the first: the previous
/// message's header (most files repeat their headers), at least this many.
const HEAD_MIN: usize = 512;

/// The requests of a remote GRIB file: by range, or from the reader that opened it when the
/// server ignored the range (it then holds the whole file).
struct Fetch {
    remote: Remote,
    fetcher: ObjectFetcher,
    reader: std::sync::Mutex<RangeReader>,
}

impl Fetch {
    /// `n` bytes at `at`.
    fn get(&self, at: usize, n: usize) -> Result<Vec<u8>> {
        match get(&self.fetcher, &self.remote, at, n) {
            Ok(data) => Ok(data),
            Err(e) => {
                // A server that ignores ranges: the reader has the file whole from its first
                // request (it refuses a larger one), so this reads from memory.
                let mut reader = self.reader.lock().map_err(|_| e)?;
                let mut out = try_vec(n)?;
                out.resize(n, 0);
                reader.seek(SeekFrom::Start(at as u64))?;
                reader.read_exact(&mut out)?;
                Ok(out)
            }
        }
    }
}

/// The header of a message: its offset and length, the bytes read from its start, and how many
/// of them are the message's own bytes in a row (`prefix`); past it, the skipped body of a
/// bitmap is zeros.
struct Head {
    start: usize,
    len: usize,
    bytes: Vec<u8>,
    prefix: usize,
}

/// The messages of a remote GRIB file.
pub(crate) struct Messages {
    fetch: Fetch,
    file_len: usize,
    messages: Vec<Head>,
    /// Each band: its message, and its index among the message's bands.
    bands: Vec<(usize, usize)>,
}

impl Messages {
    pub(crate) fn file_len(&self) -> usize {
        self.file_len
    }

    /// The message of band `i` (from 0) and the band's index in it.
    pub(crate) fn band(&self, i: usize) -> Option<(usize, usize)> {
        self.bands.get(i).copied()
    }

    /// Message `m`, whole: the start read at open, and the rest.
    pub(crate) fn fetch(&self, m: usize) -> Result<Vec<u8>> {
        let h = &self.messages[m];
        let mut out = try_vec(h.len)?;
        out.extend_from_slice(&h.bytes[..h.prefix.min(h.len)]);
        if out.len() < h.len {
            out.extend_from_slice(&self.fetch.get(h.start + out.len(), h.len - out.len())?);
        }
        Ok(out)
    }
}

/// `n` bytes at `at`.
fn get(fetcher: &ObjectFetcher, remote: &Remote, at: usize, n: usize) -> Result<Vec<u8>> {
    let data =
        fetcher.get(remote, ByteRange::At(at as u64, n as u64))?.ok_or_else(|| Error::Http(format!("bytes {at}+{n}: the file is gone")))?;
    if data.len() != n {
        return Err(Error::Http(format!("bytes {at}+{n}: {} bytes received", data.len())));
    }
    Ok(data)
}

/// The next part of a message's header to read, given the `head` at hand: `(offset, length)`
/// from the message's start, the offset past the head when a GRIB1 bitmap's body is skipped.
/// `None` when the head holds the header. GRIB2 headers are read whole ([`header_needs`]).
fn next_part(head: &[u8], len: usize) -> Option<(usize, usize)> {
    if head.len() >= 16 && head[7] == 1 {
        return grib1_next_part(head, len);
    }
    header_needs(head, len).filter(|&n| n > head.len()).map(|n| (head.len(), n - head.len()))
}

/// [`next_part`] for GRIB1: the sections up to the bitmap's first 6 octets, then the data
/// section's first 11 octets, past the bitmap's body. A header [`grib1::Message::parse`] would
/// refuse ends here: parsing reports it.
fn grib1_next_part(head: &[u8], len: usize) -> Option<(usize, usize)> {
    // Contiguous bytes up to `n`.
    let to = |n: usize| {
        let n = n.min(len);
        (n > head.len()).then(|| (head.len(), n - head.len()))
    };
    let u = |o: usize| be(&head[o..o + 3]) as usize;
    if head.len() < 11 {
        return to(11);
    }
    let pds_len = u(8);
    if pds_len < 28 {
        return None;
    }
    // Past the product definition section: the grid definition section's length.
    let mut cur = 8 + pds_len;
    if head.len() < cur + 6 {
        return to(cur + 6);
    }
    let gds_len = u(cur);
    if gds_len < 6 {
        return None;
    }
    cur += gds_len;
    if head[15] & 0x40 != 0 {
        // The bitmap section's length, then the data section's first octets past its body.
        if head.len() < cur + 6 {
            return to(cur + 6);
        }
        let bds = cur.saturating_add(u(cur).max(6));
        if bds.saturating_add(11) > len || head.len() >= bds + 11 {
            return None;
        }
        if head.len() > bds {
            return to(bds + 11);
        }
        return Some((bds, 11));
    }
    to(cur + 11)
}

/// Opens a remote GRIB file to be read band by band.
pub(crate) fn open(remote: &Remote) -> Result<crate::LazyRaster> {
    let normalize = normalize_units();
    let options = HttpOptions { block_size: BLOCK as u64, ..HttpOptions::from_env() };
    let mut reader = RangeReader::open_with(remote, options)?;
    let file_len = reader.len() as usize;
    // The first block, which opening the reader fetched.
    let mut first = vec![0u8; BLOCK.min(file_len)];
    reader.read_exact(&mut first)?;
    let fetch = Fetch { remote: remote.clone(), fetcher: ObjectFetcher::new(), reader: std::sync::Mutex::new(reader) };
    let find = |b: &[u8]| b.windows(8).position(|w| &w[..4] == b"GRIB" && (w[7] == 1 || w[7] == 2));
    let mut messages: Vec<Head> = Vec::new();
    let mut pos = 0;
    // Bytes read past the previous message: the start of the next.
    let mut carry: Vec<u8> = first;
    let mut guess = HEAD_MIN;
    while pos + 16 <= file_len {
        let mut head = if !carry.is_empty() { std::mem::take(&mut carry) } else { fetch.get(pos, guess.min(file_len - pos))? };
        // A message starts with "GRIB" and its edition, as GDAL's inventory finds them; looked
        // for in a block when the bytes at hand are fewer.
        let mut at = find(&head);
        if at.is_none() && head.len() < BLOCK.min(file_len - pos) {
            head = fetch.get(pos, BLOCK.min(file_len - pos))?;
            at = find(&head);
        }
        let Some(off) = at else {
            if messages.is_empty() {
                return Err(Error::InvalidData("no GRIB message in the first block".into()));
            }
            break;
        };
        let start = pos + off;
        head.drain(..off);
        if head.len() < 16 && start + 16 <= file_len {
            head.extend_from_slice(&fetch.get(start + head.len(), 16 - head.len())?);
        }
        let len = match head.get(7) {
            Some(2) if head.len() >= 16 => be(&head[8..16]) as usize,
            Some(1) => grib1::message_len(&head)?,
            _ => break,
        };
        if len < 16 || start.checked_add(len).is_none_or(|end| end > file_len) {
            return Err(Error::InvalidData(format!("GRIB message at offset {start} is truncated")));
        }
        if head.len() > len {
            carry = head.split_off(len);
        }
        // The rest of the header; a skipped GRIB1 bitmap body stays zeros, past the prefix.
        let mut prefix = None;
        while let Some((at, n)) = next_part(&head, len).filter(|&(at, n)| n > 0 && at >= head.len() && at + n <= len) {
            if at > head.len() {
                prefix.get_or_insert(head.len());
                head.resize(at, 0);
            }
            head.extend_from_slice(&fetch.get(start + at, n)?);
        }
        let prefix = prefix.unwrap_or(head.len());
        // The next message's header is likely the size of this one's (GRIB2), or short (GRIB1,
        // whose bitmap is skipped).
        guess = if head[7] == 2 { prefix.clamp(HEAD_MIN, BLOCK) } else { HEAD_MIN };
        messages.push(Head { start, len, bytes: head, prefix });
        pos = start + len;
    }
    // A GRIB1 message's nodata may depend on its data's maximum: then it is read whole.
    for h in messages.iter_mut() {
        if h.bytes.get(7) != Some(&1) || h.prefix >= h.len {
            continue;
        }
        let mut m = grib1::Message::parse(&h.bytes, file_len)?;
        m.normalize = normalize;
        if m.nodata_needs_data()? {
            let rest = fetch.get(h.start + h.prefix, h.len - h.prefix)?;
            h.bytes.truncate(h.prefix);
            h.bytes.extend_from_slice(&rest);
            h.prefix = h.len;
        }
    }
    let mut all: Vec<Band> = Vec::new();
    let mut bands = Vec::new();
    for (m, h) in messages.iter().enumerate() {
        for (k, b) in message_bands(&h.bytes, h.len, file_len, normalize)?.into_iter().enumerate() {
            bands.push((m, k));
            all.push(b);
        }
    }
    let (ds, split) = header(&all, file_len)?;
    drop(all);
    let (cols, rows) = (ds.raster.cols, ds.raster.rows);
    let store = Store::Remote(Box::new(Messages { fetch, file_len, messages, bands }));
    Ok(crate::LazyRaster::from_source(ds, Box::new(Source { store, cols, rows, split, normalize, cache: Vec::new() })))
}

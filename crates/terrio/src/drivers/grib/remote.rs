// Written to match the behavior of:
//   GDAL v3.12.2:
//     frmts/grib/gribdataset.cpp
//     frmts/grib/degrib/degrib/inventory.cpp
// No code is translated from these sources.

//! Remote GRIB files read by range, so that reading a band costs its message rather than the
//! file: at open, the header of each message (the first octets of each of its sections and its
//! definitions, [`super::read_head`]) is read to find the next message and to describe its bands;
//! a band's message is fetched whole when the band is read. The bodies of bitmap and data sections
//! are not needed to describe a band, so they are skipped: a GRIB1 message's header costs two small
//! requests (the sections before the bitmap, then the data section's first octets) rather than a
//! block holding the bitmap, and each field of a GRIB2 message of several fields after the first a
//! request of its sections rather than the data of the field before it. A GRIB1 message whose
//! nodata depends on its data (degrib's `resetPrim`) is read whole at open. The open dataset keeps
//! of each message the bytes read in a row from its start (those messages whole), which a read of
//! its bands does not fetch again. Bytes received: the headers, those messages, and the messages of
//! the bands read. A server that ignores ranges is read only if the file came whole with the first
//! request (a small file): the rest of the reads are then served from it.

use super::{Band, Head, Source, Store, be, grib1, header_with, normalize_units, read_head, try_vec};
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

/// A message of a remote GRIB file: its offset and length, and the bytes from its start that the
/// open read in a row, which a read of its bands does not fetch again.
struct Message {
    start: usize,
    len: usize,
    prefix: Vec<u8>,
}

/// The messages of a remote GRIB file.
pub(crate) struct Messages {
    fetch: Fetch,
    file_len: usize,
    messages: Vec<Message>,
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
        out.extend_from_slice(&h.prefix[..h.prefix.len().min(h.len)]);
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
    // Each message's offset, length and header, held until the dataset's header is built.
    let mut heads: Vec<(usize, usize, Head)> = Vec::new();
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
            if heads.is_empty() {
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
        // The rest of the header, each part of a GRIB2 one fetched with a few hundred bytes more,
        // so that a field's definitions come in one request.
        let head = read_head(head, len, HEAD_MIN, |at, n| fetch.get(start + at, n))?;
        // The next message's header is likely the size of this one's (GRIB2), or short (GRIB1,
        // whose bitmap is skipped).
        guess = if let Head::Grib2(_, needed) = &head { (*needed).clamp(HEAD_MIN, BLOCK) } else { HEAD_MIN };
        heads.push((start, len, head));
        pos = start + len;
    }
    // A GRIB1 message's nodata may depend on its data's maximum: then it is read whole.
    for (start, len, head) in heads.iter_mut() {
        let Head::Grib1(bytes, prefix) = head else { continue };
        if *prefix >= *len {
            continue;
        }
        let mut m = grib1::Message::parse(bytes, file_len)?;
        m.normalize = normalize;
        if m.nodata_needs_data()? {
            let rest = fetch.get(*start + *prefix, *len - *prefix)?;
            bytes.truncate(*prefix);
            bytes.extend_from_slice(&rest);
            *prefix = *len;
        }
    }
    // What the dataset's header needs of each band.
    let (mut first_grid, mut band_nodata, mut band_metadata, mut bands) = (None, Vec::new(), Vec::new(), Vec::new());
    for (m, (_, len, head)) in heads.iter().enumerate() {
        let (found, _) = head.bands(*len, file_len, normalize)?;
        if first_grid.is_none() {
            first_grid = found.first().map(Band::grid);
        }
        for (k, band) in found.iter().enumerate() {
            bands.push((m, k));
            band_nodata.push(band.nodata());
            band_metadata.push(band.metadata());
        }
    }
    let (ds, split) = header_with(first_grid, bands.len(), band_nodata.into_iter(), band_metadata.into_iter(), file_len)?;
    // Of each header, the bytes read in a row from the message's start.
    let messages = heads.into_iter().map(|(start, len, head)| Message { start, len, prefix: head.into_first() }).collect();
    let store = Store::Remote(Box::new(Messages { fetch, file_len, messages, bands }));
    Ok(Source::lazy(Source::new(ds, split, normalize, store, crate::block_cache::Handle::new())))
}

// SPDX-FileCopyrightText: Copyright (c) 2007, ITC
// SPDX-FileCopyrightText: Copyright (c) 2008-2017, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.12.2:
//     frmts/grib/gribdataset.cpp
//   GDAL (bundled NOAA MDL degrib) v3.12.2 (public domain):
//     frmts/grib/degrib/degrib/degrib2.cpp
//     frmts/grib/degrib/degrib/inventory.cpp

//! GRIB (editions 1 and 2), read as GDAL's GRIB driver reads it: a band per message (per field of
//! GRIB2 messages that repeat their sections), Float64, with GDAL's `GRIB_*` band metadata and
//! descriptions, georeferencing and nodata values, units normalized to metric by default
//! (`GRIB_NORMALIZE_UNITS`), rasters north-up and global grids from -180 to 180 degrees.
//!
//! The files are read in place: opening reads the messages' headers (with range requests for
//! remote files, in parallel when a NOAA `.idx` or ECMWF `.index` sidecar lists the messages), and
//! a band's message is decoded when the band is read. GRIB2 packings are decoded as GDAL's g2clib
//! decodes them (in single precision, to its bit): simple, complex and spatial differencing, IEEE,
//! JPEG 2000 (terrio's OpenJPEG port), PNG and CCSDS (gribberish's pure-Rust decoder).

mod grib1;
mod grib2;
mod grid;
mod pds;
mod tables;
mod unpack;

use crate::cog::WindowSource;
use crate::formats::RasterDriver;
use crate::vfs::Location;
use crate::{Error, RasterDataset, Result, Window};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use wbraster::raster::RasterData;

/// The most bytes searched for the first message (degrib's `GRIB_LIMIT`).
const GRIB_LIMIT: usize = 300;

/// The decoded bands kept in memory, in bytes (GDAL's `GRIB_CACHEMAX` default, 100 MB).
const CACHE_BYTES: usize = 100 * 1024 * 1024;

/// The default maximum points in a field: 128 MiB of Float64 values, before decoder buffers.
const DEFAULT_MAX_POINTS: usize = 16 * 1024 * 1024;

/// Bytes read at once while walking a remote message's sections (sections 0 to 6 of most
/// messages, without large bitmaps).
const CHUNK: usize = 4 * 1024;

/// Where a GRIB file's bytes come from.
pub(crate) trait Source: Send + Sync {
    /// Whether the file is remote: its messages' headers are then read in parallel, with small
    /// range requests.
    fn is_remote(&self) -> bool {
        false
    }
    /// The file's length.
    fn len(&self) -> u64;
    /// The bytes from `offset`, at most `len` of them (fewer at the end of the file).
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>>;
}

/// A local file.
struct FileSource {
    file: Mutex<std::fs::File>,
    len: u64,
}

impl Source for FileSource {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = self.file.lock().map_err(|_| Error::InvalidData("GRIB file lock poisoned".into()))?;
        f.seek(SeekFrom::Start(offset))?;
        let n = (len as u64).min(self.len.saturating_sub(offset)) as usize;
        let mut buf = vec![0; n];
        f.read_exact(&mut buf)?;
        Ok(buf)
    }
}

/// Bytes in memory.
struct BytesSource(Arc<[u8]>);

impl Source for BytesSource {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let start = (offset as usize).min(self.0.len());
        let end = start.saturating_add(len).min(self.0.len());
        Ok(self.0[start..end].to_vec())
    }
}

/// A remote file, read by range requests from any number of threads.
#[cfg(remote)]
struct RemoteSource {
    remote: crate::http::Remote,
    fetcher: crate::http::ObjectFetcher,
    len: u64,
}

#[cfg(remote)]
impl Source for RemoteSource {
    fn is_remote(&self) -> bool {
        true
    }

    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        let n = (len as u64).min(self.len.saturating_sub(offset));
        if n == 0 {
            return Ok(Vec::new());
        }
        self.fetcher
            .get(&self.remote, crate::http::ByteRange::At(offset, n))?
            .ok_or_else(|| Error::Http("the GRIB file disappeared".into()))
    }
}

/// Reads parts of a source, keeping the last chunk read.
struct MessageReader<'a> {
    src: &'a dyn Source,
    base: u64,
    buf: Vec<u8>,
    chunk: usize,
}

impl<'a> MessageReader<'a> {
    fn new(src: &'a dyn Source, chunk: usize) -> Self {
        Self { src, base: 0, buf: Vec::new(), chunk }
    }

    /// `len` bytes from `offset`; fails past the end of the file.
    fn bytes(&mut self, offset: u64, len: usize) -> Result<&[u8]> {
        let end = offset + len as u64;
        if offset < self.base || end > self.base + self.buf.len() as u64 {
            self.buf = self.src.read_at(offset, len.max(self.chunk))?;
            self.base = offset;
            if (self.buf.len() as u64) < len as u64 {
                return Err(Error::InvalidData(format!("GRIB file truncated at offset {}", offset + self.buf.len() as u64)));
            }
        }
        let i = (offset - self.base) as usize;
        Ok(&self.buf[i..i + len])
    }

    /// Seeds the reader with bytes already fetched from `offset`.
    #[cfg_attr(not(remote), allow(dead_code))]
    fn seed(&mut self, offset: u64, bytes: Vec<u8>) {
        self.base = offset;
        self.buf = bytes;
    }
}

/// A band: a field of a message, with GDAL's metadata.
struct Band {
    /// The message's offset and length in the file.
    offset: u64,
    len: u64,
    /// GDAL's band metadata (`GRIB_*`), in GDAL's order.
    items: Vec<(String, String)>,
    /// GDAL's band description (the level, `2[m] HTGL="Specified height level above ground"`).
    description: String,
    nodata: Option<f64>,
    /// The grid's size.
    size: (usize, usize),
    /// A GRIB2 field's grid, sections and unit conversion.
    gds2: Option<(grid::Gds, grib2::FieldSections, tables::UnitConvert)>,
    /// A GRIB1 message's description.
    grib1: Option<grib1::Field>,
}

/// A decoded field: degrib's grid, from the south-west corner (rows first) for the grids it
/// reorders.
struct Decoded {
    nx: usize,
    ny: usize,
    values: Vec<f64>,
}

/// Validates a grid before allocating its decoded Float64 values.
fn point_count(nx: usize, ny: usize, message_len: u64) -> Result<usize> {
    let limit = match std::env::var("TERRIO_GRIB_MAX_POINTS") {
        Ok(v) => v.trim().parse::<usize>().ok().filter(|n| *n > 0),
        Err(std::env::VarError::NotPresent) => Some(DEFAULT_MAX_POINTS),
        Err(_) => None,
    }
    .ok_or_else(|| Error::InvalidArgument("TERRIO_GRIB_MAX_POINTS must be a positive integer".into()))?;
    checked_point_count(nx, ny, message_len, limit)
}

/// Validates the grid against the configured limit as well as platform and message sizes.
fn checked_point_count(nx: usize, ny: usize, message_len: u64, limit: usize) -> Result<usize> {
    let n = nx
        .checked_mul(ny)
        .filter(|n| n.checked_mul(8).is_some_and(|bytes| bytes <= isize::MAX as usize))
        .ok_or_else(|| Error::InvalidData("GRIB grid larger than this platform's memory".into()))?;
    if n > limit {
        // A configured resource limit must not trigger the optional GDAL fallback.
        return Err(Error::InvalidArgument(format!("GRIB grid of {n} points exceeds TERRIO_GRIB_MAX_POINTS={limit}")));
    }
    // degrib's check on large grids: at most 1000 points per byte of the message.
    if n > 100 * 1024 * 1024 && n as u64 / 1000 > message_len {
        return Err(Error::InvalidData("GRIB grid too large for its message".into()));
    }
    Ok(n)
}

/// Whether bytes start a GRIB file: `GRIB` and an edition of 1 or 2 within the first bytes (as
/// degrib looks for the first message).
pub(crate) fn is_grib(head: &[u8]) -> bool {
    find_grib(head, 0).is_some_and(|i| i <= GRIB_LIMIT)
}

/// The offset of the first `GRIB` followed by an edition of 1 or 2 at or after `from`.
fn find_grib(b: &[u8], from: usize) -> Option<usize> {
    (from..b.len().saturating_sub(7)).find(|&i| &b[i..i + 4] == b"GRIB" && matches!(b[i + 7], 1 | 2))
}

/// A message: its offset, length and edition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Message {
    offset: u64,
    len: u64,
    edition: u8,
}

/// The length and edition of a message from its first 16 bytes.
fn message_at(head: &[u8], offset: u64) -> Option<Message> {
    if head.len() < 8 || &head[..4] != b"GRIB" {
        return None;
    }
    match head[7] {
        1 => {
            let len = u64::from(u32::from_be_bytes([0, head[4], head[5], head[6]]));
            (len >= 52).then_some(Message { offset, len, edition: 1 })
        }
        2 if head.len() >= 16 => {
            let len = u64::from_be_bytes(head[8..16].try_into().ok()?);
            (len >= 16).then_some(Message { offset, len, edition: 2 })
        }
        _ => None,
    }
}

/// The messages of a file, found by their section 0 lengths: the first within the first bytes,
/// the next ones from the end of the previous (skipping bytes between them), up to trailing bytes
/// that hold no message.
fn scan(src: &dyn Source) -> Result<Vec<Message>> {
    let total = src.len();
    let mut out = Vec::new();
    let mut pos = 0u64;
    let window = 64 * 1024;
    'messages: while pos + 16 <= total {
        // A message where the previous one ends, as is usual; else "GRIB" further on.
        let head = src.read_at(pos, 16)?;
        if let Some(m) = message_at(&head, pos).filter(|m| m.len <= total - m.offset) {
            out.push(m);
            pos = m.offset + m.len;
            continue;
        }
        let mut at = pos;
        let start = loop {
            let buf = src.read_at(at, window)?;
            if let Some(i) = find_grib(&buf, 0) {
                break at + i as u64;
            }
            if buf.len() < window || (out.is_empty() && at + buf.len() as u64 > GRIB_LIMIT as u64) {
                break 'messages;
            }
            at += (buf.len() - 7) as u64;
        };
        if out.is_empty() && start > GRIB_LIMIT as u64 {
            break;
        }
        let head = src.read_at(start, 16)?;
        let Some(m) = message_at(&head, start) else { break };
        if m.len > total - m.offset {
            if out.is_empty() {
                return Err(Error::InvalidData("truncated GRIB message".into()));
            }
            break;
        }
        out.push(m);
        pos = m.offset + m.len;
    }
    if out.is_empty() {
        return Err(Error::InvalidData("no GRIB message found".into()));
    }
    Ok(out)
}

/// The bands of a message, read through `r`.
fn message_bands(r: &mut MessageReader, m: Message) -> Result<Vec<Band>> {
    match m.edition {
        1 => grib1::bands(r, m.offset, m.len),
        _ => grib2::bands(r, m.offset, m.len),
    }
}

/// The bands of all messages, one after another.
fn all_bands(src: &dyn Source, messages: &[Message]) -> Result<Vec<Band>> {
    let mut r = MessageReader::new(src, 64 * 1024);
    let mut out = Vec::new();
    for m in messages {
        out.extend(message_bands(&mut r, *m)?);
    }
    Ok(out)
}

/// The bands of the messages at offsets an index lists (sorted), their heads fetched in parallel
/// (a request each, and one more for messages whose bitmap goes past it). `None` when the index
/// does not describe the file.
#[cfg(remote)]
fn indexed_bands(src: &dyn Source, offsets: &[u64]) -> Result<Option<Vec<Band>>> {
    let total = src.len();
    let per_message: Vec<Result<Option<Vec<Band>>>> = fetch_parallel(offsets.len(), |i| {
        let off = offsets[i];
        let end = offsets.get(i + 1).copied().unwrap_or(total);
        let head = src.read_at(off, ((end - off) as usize).min(CHUNK))?;
        let Some(m) = message_at(&head, off).filter(|m| m.len <= end - off) else { return Ok(None) };
        let mut r = MessageReader::new(src, CHUNK);
        r.seed(off, head);
        message_bands(&mut r, m).map(Some)
    });
    let mut out = Vec::new();
    for b in per_message {
        match b? {
            Some(b) => out.extend(b),
            None => return Ok(None),
        }
    }
    Ok(Some(out))
}

/// Runs `f` for `0..n` on up to 16 threads (requests waiting on the network rather than the
/// processor), the results in order.
#[cfg(remote)]
fn fetch_parallel<R: Send>(n: usize, f: impl Fn(usize) -> R + Sync) -> Vec<R> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<Option<R>>> = Mutex::new((0..n).map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..n.min(16) {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= n {
                        break;
                    }
                    let r = f(i);
                    results.lock().expect("GRIB parallel results lock poisoned")[i] = Some(r);
                }
            });
        }
    });
    results
        .into_inner()
        .expect("GRIB parallel results lock poisoned")
        .into_iter()
        .map(|r| r.expect("GRIB parallel result missing"))
        .collect()
}

/// A GRIB file read band by band: its bands, and the decoded ones kept.
struct GribSource {
    src: Arc<dyn Source>,
    bands: Vec<Band>,
    width: usize,
    height: usize,
    split: usize,
    normalize: bool,
    cache: Mutex<VecDeque<(usize, Arc<Vec<f64>>)>>,
}

impl GribSource {
    /// Decodes a band (from 0) as GDAL gives it: north-up rows of the dataset's width.
    fn band(&self, b: usize) -> Result<Arc<Vec<f64>>> {
        if let Ok(c) = self.cache.lock()
            && let Some((_, v)) = c.iter().find(|(i, _)| *i == b)
        {
            return Ok(v.clone());
        }
        let band = &self.bands[b];
        let len = usize::try_from(band.len).map_err(|_| Error::Unsupported("a GRIB message larger than this platform's memory".into()))?;
        let msg = self.src.read_at(band.offset, len)?;
        let d = match (&band.gds2, &band.grib1) {
            (Some((gds, s, convert)), _) => grib2::decode(&msg, gds, s, *convert, self.normalize)?,
            (None, Some(f)) => grib1::decode(&msg, f, self.normalize)?,
            _ => return Err(Error::InvalidData("GRIB band without a grid".into())),
        };
        let v = Arc::new(arrange(&d, self.width, self.height, self.split));
        if let Ok(mut c) = self.cache.lock() {
            let size = self.width * self.height * 8;
            let keep = (CACHE_BYTES / size.max(1)).max(1);
            while c.len() >= keep {
                c.pop_front();
            }
            c.push_back((b, v.clone()));
        }
        Ok(v)
    }
}

/// GDAL's `GRIBRasterBand::IReadBlock`: degrib's grid (south-west corner first) as north-up rows
/// of the dataset's size, rewrapped at column `split`; a band of another size is copied into the
/// top-left corner, the rest zeros.
fn arrange(d: &Decoded, width: usize, height: usize, split: usize) -> Vec<f64> {
    let mut out = vec![0f64; width * height];
    let split = if d.nx != width { 0 } else { split.min(width) };
    let copy = width.min(d.nx);
    for y in 0..height.min(d.ny) {
        let src = &d.values[(d.ny - y - 1) * d.nx..(d.ny - y) * d.nx];
        let dst = &mut out[y * width..y * width + width];
        dst[..copy - split].copy_from_slice(&src[split..copy]);
        if split > 0 {
            dst[copy - split..copy].copy_from_slice(&src[..split]);
        }
    }
    out
}

impl WindowSource for GribSource {
    fn overviews(&self) -> Vec<(usize, usize)> {
        Vec::new()
    }

    fn block_size(&self) -> Option<(usize, usize)> {
        Some((self.width, 1))
    }

    fn read(&mut self, _level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
        let this = &*self;
        let decoded: Vec<Result<Arc<Vec<f64>>>> = crate::par::map_range_coarse(bands.len(), |i| this.band(bands[i] - 1));
        let mut out = Vec::with_capacity(window.width * window.height * bands.len());
        for d in decoded {
            let d = d?;
            for row in window.y..window.y + window.height {
                let start = row * self.width + window.x;
                out.extend_from_slice(&d[start..start + window.width]);
            }
        }
        Ok(RasterData::F64(out))
    }
}

/// The dataset header (without pixels) of a GRIB file's bands.
fn header(bands: &[Band]) -> Result<(RasterDataset, usize)> {
    let first = bands.first().ok_or_else(|| Error::InvalidData("no GRIB band".into()))?;
    let (width, height) = first.size;
    if width == 0 || height == 0 {
        return Err(Error::InvalidData("GRIB grid of no points".into()));
    }
    let geo = match (&first.gds2, &first.grib1) {
        (Some((gds, _, _)), _) => grid::georef(gds),
        (None, Some(f)) => grib1::georef(f),
        _ => return Err(Error::InvalidData("GRIB band without a grid".into())),
    };
    let crs = match geo.crs.as_deref() {
        Some(c) if c.starts_with('+') => crate::crs::SpatialRef::from_proj4(c)?.to_crs_info(),
        Some(c) if c.starts_with("EPSG:") => crate::crs::SpatialRef::parse(c)?.to_crs_info(),
        Some(c) => crate::crs::SpatialRef::from_wkt(c).to_crs_info(),
        None => wbraster::CrsInfo::default(),
    };
    // One nodata value for the dataset: that of the first band with one (the others' are in
    // `TERRIO#BAND_<n>_NODATA`).
    let nodata = bands.iter().find_map(|b| b.nodata);
    let mut metadata = Vec::new();
    for (i, b) in bands.iter().enumerate() {
        let n = i + 1;
        metadata.push((format!("{}BAND_{n}_DESCRIPTION", crate::info::INTERNAL), b.description.clone()));
        if let Some(v) = b.nodata {
            metadata.push((format!("{}BAND_{n}_NODATA", crate::info::INTERNAL), format!("{v:?}")));
        }
        metadata.extend(b.items.iter().map(|(k, v)| (format!("BAND_{n}#{k}"), v.clone())));
    }
    let raster = wbraster::Raster {
        cols: width,
        rows: height,
        bands: bands.len(),
        x_min: 0.0,
        y_min: 0.0,
        cell_size_x: 1.0,
        cell_size_y: 1.0,
        nodata: nodata.unwrap_or(-9999.0),
        data_type: wbraster::DataType::F64,
        crs,
        metadata,
        data: RasterData::new_filled(wbraster::DataType::F64, 0, 0.0),
    };
    let mut ds = RasterDataset::with_driver(raster, RasterDriver::GRIB, nodata.is_some());
    ds.set_geo_transform(geo.geo_transform);
    Ok((ds, geo.split_column))
}

/// Opens a GRIB source: its messages (at the offsets an index lists, when there is one) and bands.
fn open_source(src: Arc<dyn Source>, offsets: Option<Vec<u64>>) -> Result<(RasterDataset, GribSource)> {
    // Remote files: the messages of the index, or found one after another by their lengths,
    // then their headers in parallel.
    #[cfg(remote)]
    let indexed = match offsets {
        Some(o) => indexed_bands(src.as_ref(), &o)?,
        None if src.is_remote() => {
            let offsets: Vec<u64> = scan(src.as_ref())?.iter().map(|m| m.offset).collect();
            indexed_bands(src.as_ref(), &offsets)?
        }
        None => None,
    };
    #[cfg(not(remote))]
    let indexed: Option<Vec<Band>> = offsets.and(None);
    let mut bands = match indexed {
        Some(b) => b,
        None => all_bands(src.as_ref(), &scan(src.as_ref())?)?,
    };
    // GDAL's limit.
    bands.truncate(65536);

    let (ds, split) = header(&bands)?;
    let (width, height) = (ds.raster.cols, ds.raster.rows);
    let normalize = grib2::normalize_units();
    Ok((ds, GribSource { src, bands, width, height, split, normalize, cache: Mutex::new(VecDeque::new()) }))
}

fn local_source(path: &Path) -> Result<Arc<dyn Source>> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    Ok(Arc::new(FileSource { file: Mutex::new(file), len }))
}

/// Whether a local file is GRIB.
pub(crate) fn is_grib_file(path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut head = vec![0u8; GRIB_LIMIT + 16];
    let n = f.read(&mut head).unwrap_or(0);
    is_grib(&head[..n])
}

/// Reads a whole GRIB file, its bands decoded in parallel.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let (ds, src) = open_source(local_source(path)?, None).map_err(|e| with_path(path, e))?;
    read_all(ds, src)
}

/// Reads a GRIB file held in memory.
pub(crate) fn read_bytes(bytes: Vec<u8>) -> Result<RasterDataset> {
    let (ds, src) = open_source(Arc::new(BytesSource(bytes.into())), None)?;
    read_all(ds, src)
}

/// Opens a GRIB file held in memory to be read band by band.
pub(crate) fn open_lazy_bytes(bytes: Vec<u8>) -> Result<crate::LazyRaster> {
    let (ds, src) = open_source(Arc::new(BytesSource(bytes.into())), None)?;
    Ok(crate::LazyRaster::from_source(ds, Box::new(src)))
}

fn read_all(mut ds: RasterDataset, mut src: GribSource) -> Result<RasterDataset> {
    let all: Vec<usize> = (1..=src.bands.len()).collect();
    let (w, h) = (src.width, src.height);
    ds.raster.data = src.read(0, Window::full(w, h), &all)?;
    Ok(ds)
}

fn with_path(path: &Path, e: Error) -> Error {
    match e {
        Error::InvalidData(m) => Error::InvalidData(format!("{}: {m}", path.display())),
        e => e,
    }
}

/// Opens a GRIB file (a local path, URL or object store path) to be read band by band. `None`
/// when the file is not GRIB.
pub(crate) fn open_lazy(path: &str) -> Option<Result<crate::LazyRaster>> {
    let (src, offsets): Opened = match Location::parse(path) {
        Location::Local(p) => {
            if !is_grib_file(&p) {
                return None;
            }
            match local_source(&p) {
                Ok(s) => (s, None),
                Err(e) => return Some(Err(e)),
            }
        }
        #[cfg(remote)]
        loc @ (Location::Url(_) | Location::S3 { .. }) => {
            if !has_grib_name(path) {
                return None;
            }
            let remote = loc.remote()?;
            match open_remote(remote) {
                Ok(v) => v,
                Err(e) => return Some(Err(e)),
            }
        }
        _ => return None,
    };
    let opened = open_source(src, offsets).map(|(mut ds, s)| {
        super::nodata::apply_pam(path, &mut ds);
        crate::LazyRaster::from_source(ds, Box::new(s))
    });
    Some(opened.map_err(|e| match e {
        Error::InvalidData(m) => Error::InvalidData(format!("{path}: {m}")),
        e => e,
    }))
}

/// A source opened, with the offsets of its messages when an index lists them.
type Opened = (Arc<dyn Source>, Option<Vec<u64>>);

/// Whether a remote file's name is a GRIB file's: a GRIB extension, or `grb` or `grib` in a name
/// without one of another format, as NOAA names its products (`gfs.t00z.pgrb2.0p25.f000`,
/// `gep01.t00z.pgrb2a.0p50.f006`).
fn has_grib_name(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let lower = lower.split(['?', '#']).next().unwrap_or_default();
    let name = lower.rsplit('/').next().unwrap_or_default();
    if [".grb", ".grb2", ".grib", ".grib2", ".grb1", ".grib1"].iter().any(|e| name.ends_with(e)) {
        return true;
    }
    (name.contains("grb") || name.contains("grib"))
        && crate::formats::RasterDriver::for_extension(name).is_none()
        && !name.ends_with(".idx")
}

/// Opens a remote GRIB file: its messages from a sidecar index when there is one (NOAA's `.idx`
/// next to the file, ECMWF's `.index` in its place), read in parallel; otherwise found one after
/// another.
#[cfg(remote)]
fn open_remote(remote: crate::http::Remote) -> Result<Opened> {
    let fetcher = crate::http::ObjectFetcher::new();
    let reader = crate::http::RangeReader::open_with(
        &remote,
        crate::http::HttpOptions { block_size: CHUNK as u64, ..crate::http::HttpOptions::from_env() },
    )?;
    let len = reader.len();
    let offsets = index_offsets(&fetcher, &remote, len);
    let src: Arc<dyn Source> = Arc::new(RemoteSource { remote, fetcher, len });
    Ok((src, offsets))
}

/// The message offsets a sidecar index lists, sorted and without duplicates.
#[cfg(remote)]
fn index_offsets(fetcher: &crate::http::ObjectFetcher, remote: &crate::http::Remote, len: u64) -> Option<Vec<u64>> {
    use crate::http::{ByteRange, Remote};
    let with_url = |f: &dyn Fn(&str) -> String| -> Remote {
        match remote {
            Remote::Url(u) => Remote::Url(f(u)),
            Remote::S3(o) => {
                let mut o = o.clone();
                o.key = f(&o.key);
                Remote::S3(o)
            }
        }
    };
    let split_query = |u: &str| -> (String, String) {
        match u.split_once('?') {
            Some((a, q)) => (a.to_string(), format!("?{q}")),
            None => (u.to_string(), String::new()),
        }
    };
    // NOAA's (wgrib2's) `.idx`: `msg[.field]:offset:...`.
    let idx = with_url(&|u| {
        let (a, q) = split_query(u);
        format!("{a}.idx{q}")
    });
    // Sidecars of at most 4 MB, as GDAL reads them; one with a line not of its form is not used.
    let parse = |text: &[u8], offset_of: &dyn Fn(&str) -> Option<u64>| -> Option<Vec<u64>> {
        let text = String::from_utf8_lossy(text);
        let mut offsets: Vec<u64> = text.lines().filter(|l| !l.trim().is_empty()).map(offset_of).collect::<Option<_>>()?;
        offsets.sort_unstable();
        offsets.dedup();
        (!offsets.is_empty() && offsets.iter().all(|o| *o < len)).then_some(offsets)
    };
    let range = ByteRange::At(0, 4 * 1024 * 1024);
    if let Ok(Some(text)) = fetcher.get(&idx, range)
        && let Some(offsets) = parse(&text, &|line| line.split(':').nth(1)?.trim().parse::<u64>().ok())
    {
        return Some(offsets);
    }
    // ECMWF's `.index`: JSON lines with `_offset` and `_length`.
    let index = with_url(&|u| {
        let (a, q) = split_query(u);
        let base = a.rsplit_once('.').map_or(a.as_str(), |(b, _)| b).to_string();
        format!("{base}.index{q}")
    });
    if let Ok(Some(text)) = fetcher.get(&index, range)
        && let Some(offsets) = parse(&text, &|line| serde_json::from_str::<serde_json::Value>(line).ok()?.get("_offset")?.as_u64())
    {
        return Some(offsets);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(remote)]
    #[test]
    fn parallel_fetches_keep_every_result_in_order() {
        assert!(fetch_parallel(0, |_| 0).is_empty());
        assert_eq!(
            fetch_parallel(64, |i| {
                std::thread::yield_now();
                i * i
            }),
            (0..64).map(|i| i * i).collect::<Vec<_>>()
        );
    }

    #[cfg(remote)]
    #[test]
    fn parallel_fetches_propagate_worker_panics() {
        assert!(
            std::panic::catch_unwind(|| fetch_parallel(32, |i| {
                assert_ne!(i, 7, "failed fetch");
                i
            }))
            .is_err()
        );
    }

    #[test]
    fn caps_wrap_at_the_row_width() {
        let d = Decoded { nx: 3, ny: 2, values: vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0] };
        assert_eq!(arrange(&d, 3, 2, 1), vec![5.0, 6.0, 4.0, 2.0, 3.0, 1.0]);
        assert_eq!(arrange(&d, 3, 2, 10), vec![4.0, 5.0, 6.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn rejects_grids_that_cannot_be_allocated() {
        assert!(checked_point_count(usize::MAX, 2, u64::MAX, usize::MAX).is_err());
        assert!(checked_point_count(isize::MAX as usize / 8 + 1, 1, u64::MAX, usize::MAX).is_err());
        assert!(checked_point_count(200_000_000, 1, 100, usize::MAX).is_err());
        assert_eq!(checked_point_count(360, 181, 100_000, DEFAULT_MAX_POINTS).unwrap(), 65_160);
    }

    #[test]
    fn caps_even_constant_fields_and_allows_an_explicit_larger_limit() {
        assert!(matches!(checked_point_count(10_000, 10_000, 200, DEFAULT_MAX_POINTS),
            Err(Error::InvalidArgument(e)) if e.contains("TERRIO_GRIB_MAX_POINTS")));
        assert_eq!(checked_point_count(DEFAULT_MAX_POINTS, 1, 200, DEFAULT_MAX_POINTS).unwrap(), DEFAULT_MAX_POINTS);
        assert!(checked_point_count(DEFAULT_MAX_POINTS + 1, 1, 200, DEFAULT_MAX_POINTS).is_err());
        assert_eq!(checked_point_count(10_000, 10_000, 200, 100_000_000).unwrap(), 100_000_000);
    }

    #[test]
    fn rejects_oversized_constant_fields_while_opening_both_editions() {
        let valid = include_bytes!("../../../tests/data/grib/Sample_QuikSCAT.grb");
        let mut grib1 = valid[..68].to_vec();
        grib1[4..7].copy_from_slice(&[0, 0, 83]);
        grib1[15] = 0x80; // Grid definition, without a bitmap.
        grib1[42..44].copy_from_slice(&10_000u16.to_be_bytes());
        grib1[44..46].copy_from_slice(&10_000u16.to_be_bytes());
        grib1.extend_from_slice(&[0, 0, 11, 0, 0, 0, 0, 0, 0, 0, 0]);
        grib1.extend_from_slice(b"7777");
        let mut grib2 = include_bytes!("../../../tests/data/grib/simple_packing_nbits_zero_decimal_scaled.grb2").to_vec();
        let mut pos = 16;
        while pos + 5 < grib2.len() - 4 {
            let len = u32::from_be_bytes(grib2[pos..pos + 4].try_into().unwrap()) as usize;
            match grib2[pos + 4] {
                3 => {
                    grib2[pos + 6..pos + 10].copy_from_slice(&100_000_000u32.to_be_bytes());
                    grib2[pos + 30..pos + 34].copy_from_slice(&10_000u32.to_be_bytes());
                    grib2[pos + 34..pos + 38].copy_from_slice(&10_000u32.to_be_bytes());
                }
                5 => grib2[pos + 5..pos + 9].copy_from_slice(&100_000_000u32.to_be_bytes()),
                _ => {}
            }
            pos += len;
        }
        for (bytes, name) in [(grib1, "large.grb"), (grib2, "large.grb2")] {
            assert!(matches!(crate::memory::open_raster_lazy_bytes(bytes, name),
                Err(Error::InvalidArgument(e)) if e.contains("TERRIO_GRIB_MAX_POINTS")));
        }
    }

    #[cfg(feature = "gdal")]
    #[test]
    fn field_limit_errors_do_not_trigger_gdal_fallback() {
        let error = checked_point_count(10_000, 10_000, 200, DEFAULT_MAX_POINTS).unwrap_err();
        let result: Result<()> =
            crate::dataset::gdal_fallback("large.grb2", Err(error), |_| panic!("a field-limit error must not be retried with GDAL"));
        assert!(matches!(result, Err(Error::InvalidArgument(e)) if e.contains("TERRIO_GRIB_MAX_POINTS")));
    }

    #[test]
    fn reads_headers_from_a_file_larger_than_four_gib() {
        use std::io::{Seek, SeekFrom, Write};
        let mut file = tempfile::tempfile().unwrap();
        let len = (1u64 << 32) + 8;
        file.set_len(len).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        file.write_all(b"GRIB012345678901").unwrap();
        let src = FileSource { file: Mutex::new(file), len };
        assert_eq!(src.read_at(0, 16).unwrap(), b"GRIB012345678901");
        assert!(src.read_at(len + 1, 16).unwrap().is_empty());
    }
}

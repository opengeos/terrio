//! Local GRIB files read by position, so that an open dataset holds where its bands are rather than
//! the file: at open, the first octets of each message, to find the next one, and its header (its
//! sections up to the first octets of its data, [`super::next_part`]); a band's message is read
//! when the band is read. The bytes between messages are read only where a message does not start
//! where the previous one ends, to look for the next one. A GRIB1 message whose nodata depends on
//! its data is read whole at open, when its nodata is worked out once the headers are read, and
//! dropped then: the open holds one such message at a time. Each message's header is dropped once
//! what the dataset's header needs of its bands is taken from it, before the next one is read: the
//! header of a GRIB2 message of several fields holds the data of the fields before the last, whose
//! sections come after it. The last 4 bytes of each message, its end marker, which the walk of a
//! GRIB2 message's sections checks, are read at open too. The reads of the headers go in file order
//! through one buffer that a seek inside it keeps ([`crate::vfs::NearSeek`]): a message's end
//! marker and the next message's first octets and header come from one read of the file, as a rule.
//!
//! The messages are found, and their headers read, as in the bytes of a whole file in memory
//! ([`super::open_lazy_bytes`]): a file gives the same bands, metadata and errors read either way.

use super::{
    Band, BandAt, Source, Store, bad_section, complete_head, find_messages, grib1, header_with, message_bands, message_fields, try_filled,
};
use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom};

/// The bytes first read of a message's header: GRIB2 sections 0 to 5 fit in it, usually.
const HEAD_MIN: usize = 512;

/// The bytes read at a time when looking for a message past bytes that are not one.
const SEARCH: usize = 64 << 10;

/// The buffer of the reads at open: a page, which holds a message's first octets and its header,
/// as a rule.
const SCAN_BUFFER: usize = 4 << 10;

#[cfg(test)]
thread_local! {
    /// The bytes of local GRIB files read by this thread, for tests.
    pub(crate) static BYTES_READ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A cursor over a local GRIB file whose reads are positioned ([`crate::geotiff::FileCursor`]): its
/// clones read the file independently. Tests count the bytes it reads.
#[derive(Clone)]
struct Cursor(crate::geotiff::FileCursor);

impl Read for Cursor {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.0.read(buf)?;
        #[cfg(test)]
        BYTES_READ.with(|c| c.set(c.get() + n as u64));
        Ok(n)
    }
}

impl Seek for Cursor {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.seek(pos)
    }
}

/// The `n` bytes at `at` of `r`.
fn read_at(r: &mut (impl Read + Seek), at: usize, n: usize) -> Result<Vec<u8>> {
    let mut out = try_filled(n, 0u8)?;
    r.seek(SeekFrom::Start(at as u64))?;
    r.read_exact(&mut out)?;
    Ok(out)
}

/// A local GRIB file, read by position.
pub(crate) struct LocalFile {
    cursor: Cursor,
    len: usize,
    /// The extracted copy of an archive's member is kept as long as it is read.
    _local: crate::vfs::LocalPath,
}

impl LocalFile {
    /// The length of the file when it was opened.
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// The `n` bytes at `at`.
    pub(crate) fn read(&self, at: usize, n: usize) -> Result<Vec<u8>> {
        read_at(&mut self.cursor.clone(), at, n)
    }
}

/// The reads of a file of `len` bytes at open, through one buffer.
struct Scan {
    reader: crate::vfs::NearSeek<Cursor>,
    len: usize,
}

impl Scan {
    /// The `n` bytes at `at`.
    fn read(&mut self, at: usize, n: usize) -> Result<Vec<u8>> {
        read_at(&mut self.reader, at, n)
    }

    /// The offset of the first "GRIB" at or after `pos`: at `pos` itself, where the previous
    /// message ends, as a rule.
    fn find(&mut self, mut pos: usize) -> Result<Option<usize>> {
        let mut n = 16;
        while self.len.saturating_sub(pos) >= 4 {
            let bytes = self.read(pos, n.min(self.len - pos))?;
            if let Some(off) = bytes.windows(4).position(|w| w == b"GRIB") {
                return Ok(Some(pos + off));
            }
            // The last three bytes again: "GRIB" may straddle two reads.
            pos += bytes.len() - 3;
            n = SEARCH;
        }
        Ok(None)
    }

    /// The start of the message at `start` of `len` bytes that its header needs, a GRIB1 bitmap's
    /// body left as zeros ([`complete_head`]).
    fn head(&mut self, start: usize, len: usize) -> Result<Vec<u8>> {
        let mut head = self.read(start, HEAD_MIN.min(len))?;
        complete_head(&mut head, len, |at, n| self.read(start + at, n))?;
        Ok(head)
    }
}

/// Whether the nodata of the GRIB1 message whose header is `head` depends on its data. A header
/// that cannot tell is left to the message's parsing, which reports why, as the whole message's.
fn grib1_nodata_needs_data(head: &[u8], file_len: usize, normalize: bool) -> bool {
    grib1::Message::parse(head, file_len).is_ok_and(|mut m| {
        m.normalize = normalize;
        m.nodata_needs_data().unwrap_or(false)
    })
}

/// A band's nodata as its message's header gives it, or where its message is, to be read whole for
/// it: a GRIB1 message whose nodata depends on its values.
enum Nodata {
    Known(Result<Option<f64>>),
    InMessage(usize, usize),
}

/// The nodata of the GRIB1 message `msg`, read whole, as its band gives it.
fn grib1_nodata(msg: &[u8], file_len: usize, normalize: bool) -> Result<Option<f64>> {
    let mut m = grib1::Message::parse(msg, file_len)?;
    m.normalize = normalize;
    m.nodata()
}

/// The bands of a message of `len` bytes of a file of `file_len`, from its header `head` and its
/// last 4 bytes `end_marker`, as the whole message gives them: a GRIB2 message whose sections the
/// header holds until its end marker's place has that marker checked, as the whole message's
/// sections are.
fn bands_of<'a>(head: &'a [u8], end_marker: &[u8], len: usize, file_len: usize, normalize: bool) -> Result<Vec<Band<'a>>> {
    if head.get(7) == Some(&1) {
        return message_bands(head, len, file_len, normalize);
    }
    let mut fields = Vec::new();
    let end = message_fields(head, len, normalize, &mut fields)?;
    if end + 4 > head.len() && end + 4 == len && end_marker != b"7777" {
        return Err(bad_section(end));
    }
    Ok(fields.into_iter().map(Band::Grib2).collect())
}

/// Opens a local GRIB file, or an archive member's extracted copy, to be read band by band. A path
/// that is no regular file (a FIFO, a device) is read whole, into memory.
pub(super) fn open(local: crate::vfs::LocalPath) -> Result<crate::LazyRaster> {
    source(local).map(Source::lazy)
}

/// The dataset of a local GRIB file, and the source that reads its bands: see [`open`].
fn source(local: crate::vfs::LocalPath) -> Result<(crate::RasterDataset, Source)> {
    // Only Unix file versions can share cached bands; other targets need no clock.
    #[cfg(unix)]
    let opened = std::time::SystemTime::now();
    #[cfg(not(unix))]
    let opened = std::time::UNIX_EPOCH;
    let mut file = std::fs::File::open(local.path())?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 {
        // A FIFO, a device, or a file whose size its file system does not report: there is no
        // length to read it by position with, so it is read whole, into memory.
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        return super::bytes_source(bytes);
    }
    let len = usize::try_from(metadata.len()).map_err(|_| Error::ResourceLimit("GRIB file larger than this platform's memory".into()))?;
    let f = LocalFile { cursor: Cursor(crate::geotiff::FileCursor::new(file)), len, _local: local };
    let normalize = super::normalize_units();
    let scan = std::cell::RefCell::new(Scan { reader: crate::vfs::NearSeek::with_capacity(SCAN_BUFFER, f.cursor.clone()), len });
    let spans = find_messages(len, |pos| scan.borrow_mut().find(pos), |at| Ok(scan.borrow_mut().read(at, 16)?.into()))?;
    let mut scan = scan.into_inner();
    // What the dataset's header needs of each band, from its message's header, which is dropped
    // before the next message's is read: the first band's grid, each band's metadata, and its
    // nodata or, when that depends on its values, where its message is, read whole for it when the
    // dataset's header is built, and dropped then. The open holds one message's header at a time.
    let (mut first_grid, mut index, mut band_metadata, mut band_nodata) = (None, Vec::new(), Vec::new(), Vec::new());
    for &(start, n) in &spans {
        let head = scan.head(start, n)?;
        let read_whole = head.get(7) == Some(&1) && head.len() < n && grib1_nodata_needs_data(&head, len, normalize);
        // Messages are at least 16 bytes long (`find_messages`).
        let end_marker = scan.read(start + n - 4, 4)?;
        let bands = bands_of(&head, &end_marker, n, len, normalize)?;
        if first_grid.is_none() {
            first_grid = bands.first().map(Band::grid);
        }
        for (k, band) in bands.iter().enumerate() {
            index.push(BandAt::of(start, n, k, band));
            band_metadata.push(band.metadata());
            band_nodata.push(if read_whole { Nodata::InMessage(start, n) } else { Nodata::Known(band.nodata()) });
        }
    }
    let nodata = band_nodata.into_iter().map(|nodata| match nodata {
        Nodata::Known(nodata) => nodata,
        Nodata::InMessage(start, n) => grib1_nodata(&f.read(start, n)?, len, normalize),
    });
    let (ds, split) = header_with(first_grid, index.len(), nodata, band_metadata.into_iter(), len)?;
    let handle = crate::block_cache::Handle::of_file(&metadata, normalize as u64, opened);
    Ok(Source::new(ds, split, normalize, Store::File(f, index), handle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block_cache::tests::{cache_test_guard, take_lookups};
    use crate::{Window, WindowOptions};

    /// The bytes local GRIB files read so far on this thread.
    fn bytes_read() -> u64 {
        BYTES_READ.with(|c| c.get())
    }

    /// End-of-file offsets near the platform limit stop without overflowing or reading.
    #[test]
    fn signature_scan_stops_with_fewer_than_four_bytes_remaining() {
        let file = tempfile::tempfile().unwrap();
        let cursor = Cursor(crate::geotiff::FileCursor::new(file));
        let mut scan = Scan { reader: crate::vfs::NearSeek::with_capacity(SCAN_BUFFER, cursor), len: usize::MAX };
        for remaining in 0..4 {
            assert_eq!(scan.find(usize::MAX - remaining).unwrap(), None);
        }
    }

    /// A GRIB2 file of `bands` messages of `cols` x `rows` Float32 values (IEEE packing) on a
    /// latitude/longitude grid, written by terrio: its path and bytes.
    fn grib_file(dir: &std::path::Path, cols: usize, rows: usize, bands: usize) -> (String, Vec<u8>) {
        let cfg = wbraster::RasterConfig {
            cols,
            rows,
            bands,
            x_min: 0.0,
            y_min: 0.0,
            cell_size: 0.01,
            data_type: wbraster::DataType::F32,
            crs: crate::crs::SpatialRef::from_epsg(4326).unwrap().to_crs_info(),
            ..Default::default()
        };
        let values = (0..cols * rows * bands).map(|i| (i % 997) as f64 * 0.25).collect();
        let data = wbraster::raster::RasterData::from_f64_vec(wbraster::DataType::F32, values);
        let raster = wbraster::Raster::from_data_native(cfg, data).unwrap();
        let path = dir.join("f.grib2");
        crate::write_raster(&raster, path.to_str().unwrap(), &Default::default()).unwrap();
        (path.to_str().unwrap().to_string(), std::fs::read(&path).unwrap())
    }

    fn window(r: &mut crate::LazyRaster, band: usize, w: Window) -> Vec<f64> {
        r.read_window(w, &WindowOptions { bands: vec![band], overview: None }).unwrap().raster.data.to_f64_vec()
    }

    /// Catches a local GRIB file read whole, at open or when a band is read: opening reads two
    /// pages a message (its first octets, then its header with the previous message's end
    /// marker) and one more, a read of a band its message and nothing else, and a band in the
    /// block cache nothing. Another reader, opened as the file was written less than 2 seconds
    /// before, decodes the band itself (the next test shares it). The values of each window,
    /// decoded or from the cache, are those of the file read whole.
    #[test]
    fn a_local_file_is_read_by_message() {
        let _serial = cache_test_guard(64 << 20);
        let dir = tempfile::tempdir().unwrap();
        let (path, bytes) = grib_file(dir.path(), 1024, 300, 3);
        let spans = super::super::message_spans(&bytes).unwrap();
        assert_eq!(spans.len(), 3);
        let whole = super::super::read_bytes(&bytes).unwrap();
        let expected = |w: Window| -> Vec<f64> {
            let data = &whole.raster.data;
            let at = |r: usize, c: usize| data.get_f64(1024 * 300 + r * 1024 + c);
            (w.y..w.y + w.height).flat_map(|r| (w.x..w.x + w.width).map(move |c| at(r, c))).collect()
        };
        let before = bytes_read();
        let mut r = crate::LazyRaster::open(&path).unwrap();
        assert!(bytes_read() - before <= (2 * 3 + 1) * SCAN_BUFFER as u64, "{} bytes read at open", bytes_read() - before);
        let pixel = Window::new(100, 50, 1, 1);
        let before = bytes_read();
        assert_eq!(window(&mut r, 2, pixel), expected(pixel));
        assert_eq!(bytes_read() - before, spans[1].1 as u64, "band 2's message alone");
        let rows = Window::new(7, 100, 1000, 180);
        let before = bytes_read();
        assert_eq!(window(&mut r, 2, rows), expected(rows));
        assert_eq!(bytes_read() - before, 0, "band 2 from the block cache");
        #[cfg(unix)]
        {
            let mut other = crate::LazyRaster::open(&path).unwrap();
            take_lookups();
            let before = bytes_read();
            assert_eq!(window(&mut other, 2, rows), expected(rows));
            let decoded = (spans[1].1 as u64, (0, 1));
            assert_eq!((bytes_read() - before, take_lookups()), decoded, "a reader of a file changed just now keeps its own bands");
        }
    }

    /// Catches the readers of a local file that has gone unchanged for 2 seconds kept apart: of two
    /// readers opened then, the second finds band 2 in the block cache as the first decoded it,
    /// with no miss and nothing read.
    #[cfg(unix)]
    #[test]
    fn the_readers_of_a_file_unchanged_for_two_seconds_share_its_bands() {
        let dir = tempfile::tempdir().unwrap();
        let (path, bytes) = grib_file(dir.path(), 1024, 300, 3);
        std::thread::sleep(std::time::Duration::from_millis(2100));
        let _serial = cache_test_guard(64 << 20);
        let whole = super::super::read_bytes(&bytes).unwrap();
        let rows = Window::new(7, 100, 1000, 180);
        let at = |r: usize, c: usize| whole.raster.data.get_f64(1024 * 300 + r * 1024 + c);
        let expected: Vec<f64> =
            (rows.y..rows.y + rows.height).flat_map(|r| (rows.x..rows.x + rows.width).map(move |c| at(r, c))).collect();
        let (mut first, mut second) = (crate::LazyRaster::open(&path).unwrap(), crate::LazyRaster::open(&path).unwrap());
        assert_eq!(window(&mut first, 2, rows), expected);
        take_lookups();
        let before = bytes_read();
        assert_eq!(window(&mut second, 2, rows), expected);
        assert_eq!((bytes_read() - before, take_lookups()), (0, (1, 0)), "another reader of the file shares band 2");
    }

    /// Catches the header of a GRIB1 message read with its bitmap's body, which it does not need, or
    /// with its data section's first octets elsewhere than at their offset: the loop completing a
    /// header reads them once, past the body, which it leaves as zeros; and two messages of 1000 x
    /// 500 points, each with a bitmap of 62,500 bytes (constant fields, whose nodata their headers
    /// give), are opened reading two pages each and one more. The reads by position and in memory
    /// give the same bands either way, so the header and the reads are checked themselves.
    #[test]
    fn a_grib1_header_skips_the_bitmap() {
        let message = grib1_with_bitmap(1000, 500);
        let (body_end, len) = (68 + 6 + 62_500, message.len());
        let mut reads = Vec::new();
        let mut head = message[..512].to_vec();
        let prefix = super::super::complete_head(&mut head, len, |at, n| {
            reads.push((at, n));
            Ok(message[at..at + n].to_vec())
        })
        .unwrap();
        let mut expected = message[..body_end + 11].to_vec();
        expected[512..body_end].fill(0);
        assert_eq!((reads, prefix), (vec![(body_end, 11)], 512));
        assert!(head == expected, "the header of a message with a bitmap");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bitmaps.grb");
        std::fs::write(&path, message.repeat(2)).unwrap();
        let before = bytes_read();
        let r = crate::LazyRaster::open(path.to_str().unwrap()).unwrap();
        let read = bytes_read() - before;
        assert_eq!((r.band_count(), r.width(), r.height()), (2, 1000, 500));
        assert!(read <= (2 * 2 + 1) * SCAN_BUFFER as u64, "{read} bytes read at open");
    }

    /// A GRIB1 message of a constant field on an `nx` x `ny` grid, with a bitmap of every point:
    /// `Sample_QuikSCAT.grb`'s first sections, its grid's size set.
    fn grib1_with_bitmap(nx: u16, ny: u16) -> Vec<u8> {
        let valid = include_bytes!("../../../tests/data/grib/Sample_QuikSCAT.grb");
        let mut m = valid[..68].to_vec();
        m[15] = 0xc0; // A grid definition and a bitmap.
        m[42..44].copy_from_slice(&nx.to_be_bytes());
        m[44..46].copy_from_slice(&ny.to_be_bytes());
        let points = nx as usize * ny as usize;
        let bitmap = points.div_ceil(8);
        m.extend_from_slice(&((6 + bitmap) as u32).to_be_bytes()[1..]);
        m.extend_from_slice(&[(bitmap * 8 - points) as u8, 0, 0]);
        m.extend(std::iter::repeat_n(0xff, bitmap));
        m.extend_from_slice(&[0, 0, 11, 0, 0, 0, 0, 0, 0, 0, 0]);
        m.extend_from_slice(b"7777");
        let len = m.len() as u32;
        m[4..7].copy_from_slice(&len.to_be_bytes()[1..]);
        m
    }

    /// Catches a window below a band's shorter grid decoding the band again at every such read: the
    /// first keeps the grid's size, and no row of it (`kept_rows`), which serves the next from the
    /// block cache, reading nothing.
    #[test]
    fn a_window_below_a_shorter_grid_is_read_from_the_cache_next_time() {
        let _serial = cache_test_guard(64 << 20);
        let dir = tempfile::tempdir().unwrap();
        let (_, tall) = grib_file(dir.path(), 20, 10, 1);
        let (_, short) = grib_file(dir.path(), 20, 4, 1);
        let path = dir.path().join("shorter.grib2");
        std::fs::write(&path, [tall, short].concat()).unwrap();
        let mut r = crate::LazyRaster::open(path.to_str().unwrap()).unwrap();
        let reads: Vec<u64> = [6, 5]
            .into_iter()
            .map(|y| {
                let before = bytes_read();
                assert_eq!(window(&mut r, 2, Window::new(0, y, 20, 4)), vec![0.0; 80]);
                bytes_read() - before
            })
            .collect();
        assert!(reads[0] > 0 && reads[1] == 0, "bytes read by two windows below the grid: {reads:?}");
    }

    /// Empty grid entries must fit their band's share, including the cache entry overhead.
    #[test]
    fn empty_grid_entries_respect_the_band_cache_share() {
        let _serial = cache_test_guard(64);
        let dir = tempfile::tempdir().unwrap();
        let (_, bytes) = grib_file(dir.path(), 20, 4, 1);
        let (_, source) = super::super::bytes_source(bytes.clone()).unwrap();
        let window = Window::new(0, 6, 20, 1);
        for share in [32, 64] {
            let values = source.decode_and_keep(&bytes, 0, 1, window, share).unwrap();
            assert_eq!(values, vec![0.0; 20]);
            assert_eq!(crate::block_cache::contains(source.handle.id(), 1, super::super::BAND_CHUNK), share == 64);
        }
    }

    /// Catches the bands a read decodes at once pushing one another out of the block cache, or a
    /// band kept in part read from other rows than those kept: each band is kept in an equal share
    /// of the cache, its first 287 rows of 300 here, where the cache would hold one band whole but
    /// not the four. The next window of those rows reads nothing; a window of the last rows decodes
    /// the bands again. The values are those of the file read whole.
    #[test]
    fn the_bands_read_at_once_share_the_block_cache() {
        let _serial = cache_test_guard(9 << 20);
        let dir = tempfile::tempdir().unwrap();
        let (path, bytes) = grib_file(dir.path(), 1024, 300, 4);
        let spans = super::super::message_spans(&bytes).unwrap();
        let whole = super::super::read_bytes(&bytes).unwrap();
        let expected = |w: Window| -> Vec<f64> {
            let at = |b: usize, r: usize, c: usize| whole.raster.data.get_f64((b * 300 + r) * 1024 + c);
            (0..4).flat_map(|b| (w.y..w.y + w.height).flat_map(move |r| (w.x..w.x + w.width).map(move |c| at(b, r, c)))).collect()
        };
        let mut r = crate::LazyRaster::open(&path).unwrap();
        let mut read = |w: Window| {
            let before = bytes_read();
            let values = r.read_window(w, &WindowOptions { bands: vec![1, 2, 3, 4], overview: None }).unwrap().raster.data.to_f64_vec();
            assert_eq!(values, expected(w), "{w:?}");
            bytes_read() - before
        };
        let messages: u64 = spans.iter().map(|&(_, len)| len as u64).sum();
        assert_eq!(read(Window::new(0, 0, 100, 100)), messages);
        assert_eq!(read(Window::new(500, 0, 100, 100)), 0, "the four bands from the block cache");
        assert_eq!(read(Window::new(500, 290, 100, 10)), messages, "rows the cache does not hold");
    }

    /// Catches a message read once for each of its fields that a read decodes: the two fields of
    /// the one message of `subgrids_reuse_bitmap.grib2`, read together with nothing cached, read the
    /// message once and give the values of the file read whole.
    #[test]
    fn the_fields_of_one_message_read_together_read_it_once() {
        let _serial = cache_test_guard(0);
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/grib/subgrids_reuse_bitmap.grib2");
        let bytes = std::fs::read(path).unwrap();
        let spans = super::super::message_spans(&bytes).unwrap();
        assert_eq!(spans.len(), 1);
        let whole = super::super::read_bytes(&bytes).unwrap().raster.data.to_f64_vec();
        let mut r = crate::LazyRaster::open(path).unwrap();
        assert_eq!(r.band_count(), 2);
        let all = Window::full(r.width(), r.height());
        let before = bytes_read();
        let values = r.read_window(all, &WindowOptions { bands: vec![1, 2], overview: None }).unwrap().raster.data.to_f64_vec();
        assert_eq!(bytes_read() - before, spans[0].1 as u64, "the message read once");
        assert!(values.len() == whole.len() && values.iter().zip(&whole).all(|(a, b)| a.to_bits() == b.to_bits()));
    }

    /// Catches a band's message that cannot be read reported otherwise than by the read's error, or
    /// the errors of a read of several bands reported out of band order: the file changed after
    /// open, band 1's message damaged and band 2's cut off, a read of band 2 fails with the end of
    /// the file, and a read of both with band 1's error.
    #[test]
    fn a_read_fails_with_the_error_of_its_first_band_that_fails() {
        let _serial = cache_test_guard(0);
        let dir = tempfile::tempdir().unwrap();
        let (path, bytes) = grib_file(dir.path(), 40, 30, 2);
        let spans = super::super::message_spans(&bytes).unwrap();
        let mut r = crate::LazyRaster::open(&path).unwrap();
        let mut changed = bytes[..spans[1].0 + 20].to_vec();
        changed[spans[0].0 + 16..spans[0].0 + 20].copy_from_slice(&[0, 0, 0, 1]);
        std::fs::write(&path, &changed).unwrap();
        let mut read = |bands: Vec<usize>| r.read_window(Window::new(0, 0, 40, 30), &WindowOptions { bands, overview: None }).map(|_| ());
        let first = read(vec![1]).unwrap_err().to_string();
        assert!(first.contains("invalid GRIB2 section at offset 16"), "{first}");
        let second = read(vec![2]);
        assert!(matches!(&second, Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof), "{second:?}");
        assert_eq!(read(vec![1, 2]).unwrap_err().to_string(), first);
    }

    /// Catches an open storing a band's points otherwise than its own grid's, from a path or from
    /// bytes, or a read's batch charging its bands otherwise: a batch charges each band the Float64
    /// values of its grid, and a local file's messages too. A file of a 16 x 16 field then three
    /// constant fields of 4096 x 4096 points (128 MiB each as Float64, a few bytes each in the file),
    /// opened both ways: a batch of the four takes the first two; one of the last two takes both
    /// from the bytes (256 MiB exactly), and one from the path, which reads their messages too.
    #[test]
    fn both_opens_charge_each_band_at_its_own_grid() {
        let dir = tempfile::tempdir().unwrap();
        let (_, first) = grib_file(dir.path(), 16, 16, 1);
        let large = constant_field(dir.path(), 4096, 4096);
        let bytes = [first, large.clone(), large.clone(), large].concat();
        let path = dir.path().join("mixed.grib2");
        std::fs::write(&path, &bytes).unwrap();
        let (_, local) = source(crate::vfs::resolve(path.to_str().unwrap()).unwrap()).unwrap();
        let (_, memory) = super::super::bytes_source(bytes).unwrap();
        assert_eq!((local.at_once(&[1, 2, 3, 4]), memory.at_once(&[1, 2, 3, 4])), (2, 2));
        assert_eq!((local.at_once(&[3, 4]), memory.at_once(&[3, 4])), (1, 2));
    }

    /// A GRIB2 message of one constant field of `nx` x `ny` points: one of 16 x 16 points that
    /// terrio writes (simple packing of 0 bits, no data), its grid's sizes and points set.
    fn constant_field(dir: &std::path::Path, nx: u32, ny: u32) -> Vec<u8> {
        let cfg = wbraster::RasterConfig {
            cols: 16,
            rows: 16,
            bands: 1,
            cell_size: 0.01,
            data_type: wbraster::DataType::U8,
            crs: crate::crs::SpatialRef::from_epsg(4326).unwrap().to_crs_info(),
            ..Default::default()
        };
        let data = wbraster::raster::RasterData::from_f64_vec(wbraster::DataType::U8, vec![7.0; 256]);
        let path = dir.join("constant.grib2");
        crate::write_raster(&wbraster::Raster::from_data_native(cfg, data).unwrap(), path.to_str().unwrap(), &Default::default()).unwrap();
        let mut m = std::fs::read(&path).unwrap();
        // The number of points of sections 3 and 5, and the grid's Ni and Nj (template 3.0).
        let mut at = 16;
        while &m[at..at + 4] != b"7777" {
            let len = u32::from_be_bytes(m[at..at + 4].try_into().unwrap()) as usize;
            match m[at + 4] {
                3 => {
                    m[at + 6..at + 10].copy_from_slice(&(nx * ny).to_be_bytes());
                    m[at + 30..at + 34].copy_from_slice(&nx.to_be_bytes());
                    m[at + 34..at + 38].copy_from_slice(&ny.to_be_bytes());
                }
                5 => m[at + 5..at + 9].copy_from_slice(&(nx * ny).to_be_bytes()),
                _ => {}
            }
            at += len;
        }
        m
    }

    /// Catches a band in the block cache served beyond `TERRIO_GRIB_MAX_POINTS`, which is read from
    /// the process environment each time it is checked: once the limit is lowered, a band decoded
    /// before is refused, as its decode is, to the reader that decoded it and to the other readers
    /// of the file, opened before or after. The file's second field has a larger grid than its
    /// first, which is the one its open checks. In a process of its own, where no other test reads
    /// the variable while this one changes it.
    #[cfg(unix)]
    #[test]
    fn a_band_in_the_block_cache_is_refused_beyond_the_point_limit() {
        crate::par::alone(60, || {
            // SAFETY: in the test's own process, on its one test thread, while no other thread
            // reads the environment.
            unsafe { std::env::set_var("TERRIO_GRIB_MAX_POINTS", "1200") };
            let _serial = cache_test_guard(64 << 20);
            let dir = tempfile::tempdir().unwrap();
            let (_, small) = grib_file(dir.path(), 20, 10, 1);
            let (_, large) = grib_file(dir.path(), 40, 30, 1);
            let path = dir.path().join("two.grib2");
            std::fs::write(&path, [small, large].concat()).unwrap();
            let path = path.to_str().unwrap();
            let all = Window::full(20, 10);
            let early = crate::LazyRaster::open(path).unwrap();
            let mut first = crate::LazyRaster::open(path).unwrap();
            window(&mut first, 2, all);
            // SAFETY: as above.
            unsafe { std::env::set_var("TERRIO_GRIB_MAX_POINTS", "1000") };
            let late = crate::LazyRaster::open(path).unwrap();
            for (name, mut r) in [("early", early), ("first", first), ("late", late)] {
                let read = r.read_window(all, &WindowOptions { bands: vec![2], overview: None }).map(|_| ());
                let refused =
                    |e: &Error| matches!(e, Error::ResourceLimit(m) if m == "GRIB grid of 1200 points exceeds TERRIO_GRIB_MAX_POINTS=1000");
                assert!(read.as_ref().is_err_and(refused), "{name}: {read:?}");
                assert_eq!(window(&mut r, 1, all).len(), 200, "{name}: band 1, within the limit");
            }
            true
        });
    }

    /// Catches the readers of one file sharing its bands when they decode it otherwise: one opened
    /// with `GRIB_NORMALIZE_UNITS=NO` reads Kelvin while another, open at once, reads Celsius.
    #[test]
    fn readers_that_decode_a_file_otherwise_keep_their_own_bands() {
        let _serial = cache_test_guard(64 << 20);
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/grib/temperature.grib2");
        let mut celsius = crate::LazyRaster::open(path).unwrap();
        crate::config::push_scope([("GRIB_NORMALIZE_UNITS".to_string(), Some("NO".to_string()))].into());
        let kelvin = crate::LazyRaster::open(path);
        crate::config::pop_scope();
        let mut kelvin = kelvin.unwrap();
        let all = Window::full(celsius.width(), celsius.height());
        let (c, k) = (window(&mut celsius, 1, all), window(&mut kelvin, 1, all));
        assert!(c.iter().zip(&k).all(|(c, k)| (k - c - 273.15).abs() < 1e-6), "{c:?} {k:?}");
    }

    /// Catches a GRIB path that is no regular file read by position: a FIFO has no length to read
    /// it by, so it is read whole, and gives what the bytes it is fed give in memory. Opened through
    /// the GRIB driver: a public open tries other drivers first, which read the first bytes of a
    /// FIFO fed once, before the GRIB driver can. An open that fails or hangs fails the test: the
    /// writer, which waits for a reader, is let go by one that opens the FIFO without waiting.
    #[cfg(unix)]
    #[test]
    fn a_fifo_is_read_whole() {
        crate::par::alone(30, || {
            use std::os::unix::fs::OpenOptionsExt;
            let dir = tempfile::tempdir().unwrap();
            let (_, bytes) = grib_file(dir.path(), 40, 30, 2);
            let fifo = dir.path().join("fifo.grib2");
            let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
            // SAFETY: `c` is a path without NUL, valid for the call.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0, "{}", std::io::Error::last_os_error());
            let writer = {
                let (fifo, bytes) = (fifo.clone(), bytes.clone());
                std::thread::spawn(move || std::fs::write(fifo, bytes))
            };
            let (done, opened) = std::sync::mpsc::channel();
            let path = fifo.to_str().unwrap().to_string();
            std::thread::spawn(move || done.send(outcome(super::super::open_lazy(&path).expect("a GRIB name"))));
            let read = opened.recv_timeout(std::time::Duration::from_secs(10)).expect("the open did not end");
            // A writer the open did not read, as when it fails first, still waits for a reader.
            drop(std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(&fifo));
            let _ = writer.join();
            assert_eq!(read, outcome(super::super::open_lazy_bytes(bytes)));
            true
        });
    }

    /// What reading a GRIB file gives, band by band: the dataset's description, then each band's
    /// values (a digest of their bits) or its error; or the error of the open.
    fn outcome(open: Result<crate::LazyRaster>) -> String {
        use std::hash::{Hash, Hasher};
        let mut r = match open {
            Ok(r) => r,
            Err(e) => return format!("open: {e}"),
        };
        let nodata: Vec<Option<u64>> = (1..=r.band_count()).map(|b| r.band_nodata(b).map(f64::to_bits)).collect();
        let mut out = format!("{}x{}x{} {:?} {nodata:?} {:?}", r.width(), r.height(), r.band_count(), r.geo_transform(), r.band_metadata());
        let all = Window::full(r.width(), r.height());
        for b in 1..=r.band_count() {
            match r.read_window(all, &WindowOptions { bands: vec![b], overview: None }) {
                Ok(ds) => {
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    ds.raster.data.to_f64_vec().iter().for_each(|v| v.to_bits().hash(&mut h));
                    out += &format!(" {:x}", h.finish());
                }
                Err(e) => out += &format!(" band {b}: {e}"),
            }
        }
        out
    }

    /// Catches a file read by position that a damaged message, or bytes around its messages, make
    /// read otherwise than in memory: fixtures and a file terrio writes, cut short, with a
    /// section's length or the end marker damaged, a GRIB1 file whose nodata depends on its data,
    /// and messages behind other bytes (more than a block of them).
    #[test]
    fn a_file_is_read_by_position_as_in_memory() {
        let _serial = cache_test_guard(64 << 20);
        let data = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/grib");
        let dir = tempfile::tempdir().unwrap();
        let mut files: Vec<Vec<u8>> = ["subgrids_reuse_bitmap.grib2", "Sample_QuikSCAT.grb", "complex_sd2_nodata.grib2", "bitmap254.grib2"]
            .iter()
            .map(|name| std::fs::read(data.join(name)).unwrap())
            .collect();
        files.push(grib_file(dir.path(), 40, 30, 3).1);
        let mut cases = Vec::new();
        for bytes in files {
            let spans = super::super::message_spans(&bytes).unwrap();
            let (start, len) = spans[spans.len() - 1];
            let edition = bytes[start + 7];
            cases.push(bytes.clone());
            // Cut short in the last message, in its header and in its data.
            cases.push(bytes[..start + 40].to_vec());
            cases.push(bytes[..start + len - 5].to_vec());
            // The last message's first section length, and its end marker (GRIB2).
            let mut b = bytes.clone();
            b[start + 16 + 3] ^= 0x10;
            cases.push(b);
            if edition == 2 {
                let mut b = bytes.clone();
                b[start + len - 1] = b'8';
                cases.push(b);
            }
            // Other bytes before the first message, more than a block of them, its "GRIB" across
            // two of the blocks read to find it, or where it would be if they did not overlap;
            // between the messages and after the last.
            for junk in [65_547, 65_550] {
                let mut b = vec![7u8; junk];
                for &(start, len) in &spans {
                    b.extend_from_slice(&bytes[start..start + len]);
                    b.extend_from_slice(b"GRIB\x00\x00\x00\x03 not a message ");
                }
                cases.push(b);
            }
        }
        for (i, bytes) in cases.into_iter().enumerate() {
            let path = dir.path().join(format!("case{i}.grib2"));
            std::fs::write(&path, &bytes).unwrap();
            let file = outcome(super::super::open_lazy(path.to_str().unwrap()).unwrap());
            assert_eq!(file, outcome(super::super::open_lazy_bytes(bytes)), "case {i}");
        }
    }
}

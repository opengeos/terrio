//! Outputs written a strip of rows at a time, so that operations reading their input by windows
//! never hold the output whole when it is a GeoTIFF or a COG: [`StripWriter`] writes GeoTIFF
//! strips as they come (with [`GeoTiffWriter`], on another thread), compresses those of COGs as
//! they come (with [`CogWriter`]), and gathers those of other formats to write them at the end.

use crate::drivers::gtiff_write::{CogWriter, TiffOptions};
use crate::formats::RasterDriver;
use crate::{Error, GeoTiffWriter, RasterDataset, RasterWriteOptions, Result};
use wbraster::raster::RasterData;

/// About this many bytes of values per strip: a few strips are in flight at once (read ahead,
/// computed, compressed).
const STRIP_BYTES: usize = 64 << 20;

/// The rows of the strips a raster of `width` pixels and `bands` bands of `value_size` bytes is
/// processed in: about 64 MB of values, a multiple of `block` rows (the input's blocks, read
/// whole), at least one block.
pub(crate) fn strip_rows(width: usize, bands: usize, value_size: usize, block: Option<usize>) -> usize {
    rows_of(STRIP_BYTES, width, bands, value_size, block)
}

/// About this many bytes of values per strip read by [`read_ahead`], which holds several.
const SMALL_STRIP_BYTES: usize = 8 << 20;

/// The rows of the strips [`read_ahead`] reads: as [`strip_rows`], for about 8 MB of values (a
/// row of blocks when a row of blocks is larger).
pub(crate) fn small_strip_rows(width: usize, bands: usize, value_size: usize, block: Option<usize>) -> usize {
    rows_of(SMALL_STRIP_BYTES, width, bands, value_size, block)
}

fn rows_of(bytes: usize, width: usize, bands: usize, value_size: usize, block: Option<usize>) -> usize {
    let row = (width * bands.max(1) * value_size).max(1);
    let block = block.unwrap_or(1).max(1);
    (bytes / row / block).max(1) * block
}

/// Whether writing `output` would overwrite one of `inputs` (the same file, under any name):
/// those inputs are read whole before the output is written.
pub(crate) fn overwrites(output: &str, inputs: &[String]) -> bool {
    let Ok(out) = std::fs::canonicalize(output) else { return false };
    inputs.iter().any(|p| std::fs::canonicalize(p).is_ok_and(|p| p == out))
}

/// An output raster written a strip of rows at a time, from the top. A GeoTIFF dropped before
/// [`StripWriter::finish`] (an operation failing midway) is removed.
pub(crate) struct StripWriter {
    /// `None` once finished.
    inner: Option<Inner>,
    path: String,
    rows: usize,
    bands: usize,
    width: usize,
    nodata_pam: Option<Vec<Option<f64>>>,
}

enum Inner {
    /// Strips compressed and written on another thread, while the next are computed.
    #[cfg(not(target_family = "wasm"))]
    Thread { tx: std::sync::mpsc::SyncSender<(RasterData, usize)>, handle: Option<std::thread::JoinHandle<Result<()>>> },
    #[cfg(target_family = "wasm")]
    Tiff(Tiff),
    /// The output gathered whole, written at the end.
    Whole { ds: Box<RasterDataset>, options: Box<RasterWriteOptions>, done: usize },
}

impl StripWriter {
    /// Prepares to write a raster described by `header` (its data is not read) to `path`.
    pub fn create(header: &RasterDataset, path: &str, options: &RasterWriteOptions) -> Result<Self> {
        let r = &header.raster;
        let (rows, bands, width) = (r.rows, r.bands, r.cols);
        if let Some(tiff) = Tiff::create(header, path, options)? {
            let mut aux = std::ffi::OsString::from(path);
            aux.push(".aux.xml");
            let nodata_pam = (crate::dataset::needs_nodata_pam(header) || crate::memfs::exists(std::path::Path::new(&aux)))
                .then(|| (1..=bands).map(|b| header.band_nodata(b)).collect());
            let inner = {
                #[cfg(not(target_family = "wasm"))]
                {
                    let mut w = tiff;
                    let (tx, rx) = std::sync::mpsc::sync_channel::<(RasterData, usize)>(1);
                    let handle = std::thread::spawn(move || {
                        for (data, n) in rx {
                            w.write_rows(data, n)?;
                        }
                        w.finish()
                    });
                    Inner::Thread { tx, handle: Some(handle) }
                }
                #[cfg(target_family = "wasm")]
                Inner::Tiff(tiff)
            };
            return Ok(Self { inner: Some(inner), path: path.to_string(), rows, bands, width, nodata_pam });
        }
        let mut ds = header.clone();
        ds.raster.data = RasterData::new_filled(r.data_type, width * rows * bands, 0.0);
        Ok(Self {
            inner: Some(Inner::Whole { ds: Box::new(ds), options: Box::new(options.clone()), done: 0 }),
            path: path.to_string(),
            rows,
            bands,
            width,
            nodata_pam: None,
        })
    }

    /// Writes the next `rows` rows of every band (`data`: band after band, of the output's type).
    pub fn write(&mut self, data: RasterData, rows: usize) -> Result<()> {
        let Some(inner) = &mut self.inner else { return Err(Error::InvalidArgument("the raster is written".into())) };
        match inner {
            #[cfg(not(target_family = "wasm"))]
            Inner::Thread { tx, handle } => {
                if tx.send((data, rows)).is_ok() {
                    return Ok(());
                }
                // The writer stopped on an error.
                match handle.take().map(|h| h.join()) {
                    Some(Ok(Err(e))) => Err(e),
                    _ => Err(Error::InvalidData("the GeoTIFF writer stopped".into())),
                }
            }
            #[cfg(target_family = "wasm")]
            Inner::Tiff(w) => w.write_rows(data, rows),
            Inner::Whole { ds, done, .. } => {
                let (w, n) = (self.width, self.width * self.rows);
                if *done + rows > self.rows || data.len() != w * rows * self.bands {
                    return Err(Error::InvalidArgument(format!("{rows} rows past the raster's {}", self.rows)));
                }
                macro_rules! copy {
                    ($($v:ident),*) => {
                        match (&mut ds.raster.data, &data) {
                            $((RasterData::$v(out), RasterData::$v(d)) => {
                                for b in 0..self.bands {
                                    out[b * n + *done * w..b * n + (*done + rows) * w].copy_from_slice(&d[b * w * rows..(b + 1) * w * rows]);
                                }
                            })*
                            _ => return Err(Error::InvalidArgument("the rows are not of the raster's data type".into())),
                        }
                    };
                }
                copy!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64);
                *done += rows;
                Ok(())
            }
        }
    }

    /// Finishes the file.
    pub fn finish(mut self) -> Result<()> {
        let Some(inner) = self.inner.take() else { return Ok(()) };
        let result = match inner {
            #[cfg(not(target_family = "wasm"))]
            Inner::Thread { tx, handle } => {
                drop(tx);
                match handle.map(|h| h.join()) {
                    Some(Ok(result)) => result,
                    _ => Err(Error::InvalidData("the GeoTIFF writer stopped".into())),
                }
            }
            #[cfg(target_family = "wasm")]
            Inner::Tiff(w) => w.finish(),
            Inner::Whole { ds, options, done } => {
                if done != self.rows {
                    return Err(Error::InvalidArgument(format!("{done} rows written of {}", self.rows)));
                }
                return crate::write_raster_dataset(&ds, &self.path, &options);
            }
        }
        .and_then(|()| {
            if let Some(values) = &self.nodata_pam {
                crate::drivers::common::write_band_nodata_pam(std::path::Path::new(&self.path), values)?;
            }
            Ok(())
        });
        if result.is_err() {
            remove_incomplete_output(&self.path);
        }
        result
    }
}

impl Drop for StripWriter {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else { return };
        match inner {
            // Nothing written yet.
            Inner::Whole { .. } => return,
            // The writer thread stops (its channel closed) before the file is removed.
            #[cfg(not(target_family = "wasm"))]
            Inner::Thread { tx, handle } => {
                drop(tx);
                if let Some(h) = handle {
                    let _ = h.join();
                }
            }
            #[cfg(target_family = "wasm")]
            Inner::Tiff(w) => drop(w),
        }
        remove_incomplete_output(&self.path);
    }
}

fn remove_incomplete_output(path: &str) {
    let _ = std::fs::remove_file(path);
    let mut aux = std::ffi::OsString::from(path);
    aux.push(".aux.xml");
    let _ = std::fs::remove_file(aux);
}

/// A GeoTIFF or COG written as its rows come, by terrio's own writer.
enum Tiff {
    GeoTiff(GeoTiffWriter),
    Cog(CogWriter),
}

impl Tiff {
    /// The writer of `path`, when it is a GeoTIFF or COG terrio's writer streams (without
    /// overviews or a mask, in a compression it encodes); `None` for other outputs. Invalid
    /// options fail here, before any work.
    fn create(header: &RasterDataset, path: &str, options: &RasterWriteOptions) -> Result<Option<Self>> {
        let driver = options.driver.or_else(|| RasterDriver::for_extension(path));
        let cog = driver == Some(RasterDriver::Cog);
        if !(cog || driver == Some(RasterDriver::GTiff)) || options.mask.is_some() || !options.overviews.is_empty() {
            return Ok(None);
        }
        // Compressions only wbraster encodes are written whole.
        let Some(mut opts) = TiffOptions::from_write_options(options, cog, crate::dataset::tiff_nodata(header))? else { return Ok(None) };
        crate::dataset::ensure_writable_location(path)?;
        if !cog {
            return Ok(Some(Self::GeoTiff(GeoTiffWriter::create(path, header, options)?)));
        }
        // As `crate::write_raster_dataset` writes it.
        if opts.geo_transform.is_none() {
            opts.geo_transform = header.affine;
        }
        opts.complex = header.complex;
        Ok(Some(Self::Cog(CogWriter::create(path, &header.raster, &opts)?)))
    }

    fn write_rows(&mut self, data: RasterData, rows: usize) -> Result<()> {
        match self {
            Self::GeoTiff(w) => w.write_rows(&data, rows, None),
            Self::Cog(w) => w.write_rows(data, rows),
        }
    }

    fn finish(self) -> Result<()> {
        match self {
            Self::GeoTiff(w) => w.finish(),
            Self::Cog(w) => w.finish(),
        }
    }
}

/// Runs `work` on `n` strips in order, `read` reading each: on another thread, a strip ahead of
/// the one worked on (in turn on WebAssembly).
pub(crate) fn pipelined<R: Send>(
    n: usize,
    mut read: impl FnMut(usize) -> Result<R> + Send,
    mut work: impl FnMut(usize, R) -> Result<()>,
) -> Result<()> {
    #[cfg(target_family = "wasm")]
    {
        for i in 0..n {
            work(i, read(i)?)?;
        }
        Ok(())
    }
    #[cfg(not(target_family = "wasm"))]
    std::thread::scope(|s| {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        s.spawn(move || {
            for i in 0..n {
                let r = read(i);
                let failed = r.is_err();
                // The receiver is gone when work failed.
                if tx.send(r).is_err() || failed {
                    return;
                }
            }
        });
        for i in 0..n {
            let r = rx.recv().map_err(|_| Error::InvalidData("the strip reader stopped".into()))??;
            work(i, r)?;
        }
        Ok(())
    })
}

/// How many strips [`read_ahead`] reads at once, ahead of the one worked on.
#[cfg(not(target_family = "wasm"))]
const READ_AHEAD: usize = 4;

/// Runs `work` on `n` strips in order, `read` reading each: up to [`READ_AHEAD`] strips are read
/// at once on other threads, ahead of the one worked on, so that a strip slow to read does not
/// hold up the next ones (in turn on WebAssembly).
pub(crate) fn read_ahead<R: Send>(
    n: usize,
    read: impl Fn(usize) -> Result<R> + Sync,
    mut work: impl FnMut(usize, R) -> Result<()>,
) -> Result<()> {
    #[cfg(target_family = "wasm")]
    {
        for i in 0..n {
            work(i, read(i)?)?;
        }
        Ok(())
    }
    #[cfg(not(target_family = "wasm"))]
    {
        use std::collections::BTreeMap;
        use std::sync::{Condvar, Mutex};
        let ahead = READ_AHEAD;
        /// The next strip to read, the strips worked on, and whether to stop.
        struct State {
            next: usize,
            done: usize,
            stop: bool,
        }
        let state = Mutex::new(State { next: 0, done: 0, stop: false });
        let turn = Condvar::new();
        /// Stops the readers when dropped: on every exit, a panic in `read` or `work` included,
        /// so that no reader waits for a strip to be worked on that never will be.
        struct Stop<'a>(&'a Mutex<State>, &'a Condvar);
        impl Drop for Stop<'_> {
            fn drop(&mut self) {
                self.0.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
                self.1.notify_all();
            }
        }
        std::thread::scope(|s| {
            let (tx, rx) = std::sync::mpsc::channel();
            for _ in 0..ahead.min(n) {
                let tx = tx.clone();
                let (state, turn, read) = (&state, &turn, &read);
                s.spawn(move || {
                    let _stop = Stop(state, turn);
                    loop {
                        let i = {
                            let mut st = state.lock().unwrap_or_else(|e| e.into_inner());
                            // A strip is read only while fewer than `ahead` wait to be worked on.
                            while !st.stop && st.next < n && st.next >= st.done + ahead {
                                st = turn.wait(st).unwrap_or_else(|e| e.into_inner());
                            }
                            if st.stop || st.next >= n {
                                return;
                            }
                            st.next += 1;
                            st.next - 1
                        };
                        let r = read(i);
                        let failed = r.is_err();
                        // The receiver is gone when work failed.
                        if tx.send((i, r)).is_err() || failed {
                            return;
                        }
                    }
                });
            }
            drop(tx);
            let _stop = Stop(&state, &turn);
            let mut pending = BTreeMap::new();
            (|| {
                for i in 0..n {
                    let r = loop {
                        if let Some(r) = pending.remove(&i) {
                            break r;
                        }
                        let (j, r) = rx.recv().map_err(|_| Error::InvalidData("the strip readers stopped".into()))?;
                        pending.insert(j, r);
                    };
                    work(i, r?)?;
                    state.lock().unwrap_or_else(|e| e.into_inner()).done = i + 1;
                    turn.notify_all();
                }
                Ok(())
            })()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crs::SpatialRef;
    use crate::ops::{Grid, raster_header};
    use wbraster::DataType;

    fn header(data_type: DataType) -> RasterDataset {
        let grid = Grid { width: 4, height: 6, geo_transform: [0.0, 1.0, 0.0, 6.0, 0.0, -1.0] };
        raster_header(grid, data_type, 1, None, &SpatialRef::parse("EPSG:4326").unwrap()).unwrap()
    }

    /// A GeoTIFF dropped before it is finished (its operation failing) is removed; a finished one
    /// is kept.
    #[test]
    fn unfinished_outputs_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.tif");
        let path = path.to_str().unwrap();
        let rows = || RasterData::new_filled(DataType::U8, 4 * 3, 7.0);
        let mut w = StripWriter::create(&header(DataType::U8), path, &Default::default()).unwrap();
        w.write(rows(), 3).unwrap();
        let aux = format!("{path}.aux.xml");
        std::fs::write(&aux, "<PAMDataset/>").unwrap();
        drop(w);
        assert!(!std::path::Path::new(path).exists());
        assert!(!std::path::Path::new(&aux).exists());
        let mut failed = StripWriter::create(&header(DataType::U8), path, &Default::default()).unwrap();
        failed.write(rows(), 3).unwrap();
        std::fs::write(&aux, "<PAMDataset/>").unwrap();
        assert!(failed.finish().is_err());
        assert!(!std::path::Path::new(path).exists());
        assert!(!std::path::Path::new(&aux).exists());
        let mut w = StripWriter::create(&header(DataType::U8), path, &Default::default()).unwrap();
        w.write(rows(), 3).unwrap();
        w.write(rows(), 3).unwrap();
        w.finish().unwrap();
        assert!(std::path::Path::new(path).exists());
    }

    #[test]
    fn failed_pam_finish_removes_sidecar_and_allows_retry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.tif");
        let aux = dir.path().join("out.tif.aux.xml");
        std::fs::write(&aux, "<PAMDataset><Metadata>").unwrap();
        let source = header(DataType::U8);
        let mut writer = StripWriter::create(&source, path.to_str().unwrap(), &Default::default()).unwrap();
        writer.write(RasterData::new_filled(DataType::U8, 4 * 6, 7.), 6).unwrap();
        assert!(writer.finish().is_err());
        assert!(!path.exists());
        assert!(!aux.exists());
        let mut writer = StripWriter::create(&source, path.to_str().unwrap(), &Default::default()).unwrap();
        writer.write(RasterData::new_filled(DataType::U8, 4 * 6, 7.), 6).unwrap();
        writer.finish().unwrap();
        assert_eq!(crate::open_raster_lazy(path.to_str().unwrap()).unwrap().read().unwrap().raster.data.to_f64_vec(), vec![7.; 24]);
        std::fs::write(&aux, "<PAMDataset/>").unwrap();
        drop(StripWriter::create(&source, path.to_str().unwrap(), &Default::default()).unwrap());
        assert!(!path.exists());
        assert!(!aux.exists());
    }

    /// A raster of 300 x 200 Float32 values with nodata and metadata, in UTM.
    fn utm_raster() -> RasterDataset {
        let (w, h) = (300, 200);
        let values: Vec<f32> = (0..w * h).map(|i| if i % 41 == 0 { -9999.0 } else { (i % 1013) as f32 * 0.5 }).collect();
        let cfg = wbraster::RasterConfig {
            cols: w,
            rows: h,
            bands: 1,
            nodata: -9999.0,
            data_type: DataType::F32,
            crs: wbraster::CrsInfo::from_epsg(32613),
            metadata: vec![("SOURCE".into(), "test".into())],
            ..Default::default()
        };
        let raster = crate::pixel::raster_from_data(cfg, RasterData::F32(values)).unwrap();
        let mut ds = RasterDataset::with_driver(raster, RasterDriver::GTiff, true);
        ds.set_geo_transform([450000.0, 30.0, 0.0, 4400000.0, 0.0, -30.0]);
        ds
    }

    /// COGs written a strip at a time, and GeoTIFFs written as COGs from a lazy read, are those
    /// written from the whole raster.
    #[test]
    fn cogs_are_written_in_strips() {
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
        let ds = utm_raster();
        let cog = RasterWriteOptions {
            driver: Some(RasterDriver::Cog),
            compression: Some("DEFLATE".into()),
            tile_size: Some(64),
            ..Default::default()
        };
        crate::write_raster_dataset(&ds, &path("whole.tif"), &cog).unwrap();
        let whole = std::fs::read(path("whole.tif")).unwrap();

        let mut header = ds.clone();
        header.raster.data = RasterData::new_filled(DataType::F32, 0, 0.0);
        let mut w = StripWriter::create(&header, &path("strips.tif"), &cog).unwrap();
        for y in (0..200).step_by(37) {
            let h = 37.min(200 - y);
            let rows = crate::cog::cut(&ds.raster, crate::Window::new(0, y, 300, h), &[1]);
            w.write(rows, h).unwrap();
        }
        w.finish().unwrap();
        assert!(std::fs::read(path("strips.tif")).unwrap() == whole);

        // From a tiled GeoTIFF read in place.
        let tiled = RasterWriteOptions { tile_size: Some(32), compression: Some("LZW".into()), ..Default::default() };
        crate::write_raster_dataset(&ds, &path("src.tif"), &tiled).unwrap();
        let mut lazy = crate::open_raster_lazy(&path("src.tif")).unwrap();
        lazy.write(&path("lazy.tif"), &cog).unwrap();
        let crate::Dataset::Raster(read) = crate::open(&path("src.tif")).unwrap() else { panic!("not a raster") };
        crate::write_raster_dataset(&read, &path("read.tif"), &cog).unwrap();
        assert!(std::fs::read(path("lazy.tif")).unwrap() == std::fs::read(path("read.tif")).unwrap());
    }

    /// Strips read several at once, finishing in any order, are worked on in order, each once,
    /// with at most `READ_AHEAD` read and not yet worked on; a failing read or work stops them.
    #[test]
    fn strips_read_ahead_are_worked_on_in_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let reads = AtomicUsize::new(0);
        let (mut seen, mut most) = (Vec::new(), 0);
        read_ahead(
            40,
            |i| {
                // Every third strip is slow to read.
                std::thread::sleep(std::time::Duration::from_millis(if i % 3 == 0 { 8 } else { 1 }));
                reads.fetch_add(1, Ordering::SeqCst);
                Ok(i * 10)
            },
            |i, v| {
                most = most.max(reads.load(Ordering::SeqCst) - i);
                seen.push((i, v));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, (0..40).map(|i| (i, i * 10)).collect::<Vec<_>>());
        assert!(most <= READ_AHEAD, "{most} strips read ahead");

        let failed = |i| if i == 17 { Err(Error::InvalidData("read".into())) } else { Ok(i) };
        let mut done = Vec::new();
        let push = |i, _| {
            done.push(i);
            Ok(())
        };
        assert!(read_ahead(40, failed, push).is_err());
        assert_eq!(done, (0..17).collect::<Vec<_>>());
        let work = |i, _| if i == 5 { Err(Error::InvalidData("work".into())) } else { Ok(()) };
        assert!(read_ahead(1000, Ok, work).is_err());
        assert!(read_ahead(0, Ok, |_, _: usize| Ok(())).is_ok());
    }

    /// A panic in a read or in the work is propagated rather than leaving the readers waiting.
    #[test]
    fn strips_read_ahead_propagate_panics() {
        let read = |i| if i == 17 { panic!("read") } else { Ok(i) };
        let run = std::panic::catch_unwind(|| read_ahead(40, read, |_, _| Ok(())));
        assert!(run.is_err());
        let work = |i, _| if i == 5 { panic!("work") } else { Ok(()) };
        let run = std::panic::catch_unwind(|| read_ahead(40, Ok, work));
        assert!(run.is_err());
    }

    /// Options the GeoTIFF writer refuses fail before any strip is computed.
    #[test]
    fn invalid_options_fail_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.tif");
        let options = RasterWriteOptions { compression: Some("JPEG".into()), ..Default::default() };
        assert!(StripWriter::create(&header(DataType::F32), path.to_str().unwrap(), &options).is_err());
    }

    #[test]
    fn streamed_tiff_keeps_per_band_and_raw_float_nodata() {
        let dir = tempfile::tempdir().unwrap();
        for (label, nodata) in [
            ("different", vec![Some(-101.), Some(-202.)]),
            ("decimal", vec![Some(0.1), Some(0.1)]),
            ("first-only", vec![Some(-101.), None]),
            ("second-only", vec![None, Some(-202.)]),
        ] {
            let mut source = crate::ops::build_raster(
                crate::ops::Grid { width: 2, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
                vec![vec![1., 2.], vec![3., 4.]],
                DataType::F32,
                nodata[0],
                &Default::default(),
                crate::ops::convert_value,
            )
            .unwrap();
            source.band_nodata = nodata.clone();
            let path = dir.path().join(format!("{label}.tif"));
            let mut writer = StripWriter::create(&source, path.to_str().unwrap(), &Default::default()).unwrap();
            writer.write(source.raster.data.clone(), 1).unwrap();
            writer.finish().unwrap();
            let mut opened = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
            assert_eq!(opened.read().unwrap().band_nodata, nodata);
            assert_eq!(opened.read().unwrap().raster.data.to_f64_vec(), source.raster.data.to_f64_vec());
            let whole = dir.path().join(format!("{label}-whole.tif"));
            crate::write_raster_dataset(&source, whole.to_str().unwrap(), &Default::default()).unwrap();
            let mut opened = crate::open_raster_lazy(whole.to_str().unwrap()).unwrap();
            assert_eq!((1..=2).map(|band| opened.band_nodata(band)).collect::<Vec<_>>(), nodata);
            assert_eq!(opened.read().unwrap().band_nodata, nodata);
        }
    }
}

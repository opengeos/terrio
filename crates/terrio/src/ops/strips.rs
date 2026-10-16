//! Outputs written a strip of rows at a time, so that operations reading their input by windows
//! never hold the output whole when it is a GeoTIFF: [`StripWriter`] writes GeoTIFF strips as
//! they come (with [`GeoTiffWriter`], on another thread), and gathers those of other formats (and
//! of COGs, whose overviews need the whole raster) to write them at the end.

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
    let row = (width * bands.max(1) * value_size).max(1);
    let block = block.unwrap_or(1).max(1);
    (STRIP_BYTES / row / block).max(1) * block
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
}

enum Inner {
    /// Strips compressed and written on another thread, while the next are computed.
    #[cfg(not(target_arch = "wasm32"))]
    Thread { tx: std::sync::mpsc::SyncSender<(RasterData, usize)>, handle: Option<std::thread::JoinHandle<Result<()>>> },
    #[cfg(target_arch = "wasm32")]
    Tiff(GeoTiffWriter),
    /// The output gathered whole, written at the end.
    Whole { ds: Box<RasterDataset>, options: Box<RasterWriteOptions>, done: usize },
}

impl StripWriter {
    /// Prepares to write a raster described by `header` (its data is not read) to `path`.
    pub fn create(header: &RasterDataset, path: &str, options: &RasterWriteOptions) -> Result<Self> {
        let r = &header.raster;
        let (rows, bands, width) = (r.rows, r.bands, r.cols);
        let tiff = options.driver.or_else(|| RasterDriver::for_extension(path)) == Some(RasterDriver::GTiff)
            && options.mask.is_none()
            && options.overviews.is_empty();
        // Compressions only wbraster encodes are written whole; invalid options fail here, before
        // any work.
        if tiff && crate::drivers::gtiff_write::TiffOptions::from_write_options(options, false, header.nodata())?.is_some() {
            crate::dataset::ensure_writable_location(path)?;
            let inner = match GeoTiffWriter::create(path, header, options)? {
                #[cfg(not(target_arch = "wasm32"))]
                mut w => {
                    let (tx, rx) = std::sync::mpsc::sync_channel::<(RasterData, usize)>(1);
                    let handle = std::thread::spawn(move || {
                        for (data, n) in rx {
                            w.write_rows(&data, n, None)?;
                        }
                        w.finish()
                    });
                    Inner::Thread { tx, handle: Some(handle) }
                }
                #[cfg(target_arch = "wasm32")]
                w => Inner::Tiff(w),
            };
            return Ok(Self { inner: Some(inner), path: path.to_string(), rows, bands, width });
        }
        let mut ds = header.clone();
        ds.raster.data = RasterData::new_filled(r.data_type, width * rows * bands, 0.0);
        Ok(Self {
            inner: Some(Inner::Whole { ds: Box::new(ds), options: Box::new(options.clone()), done: 0 }),
            path: path.to_string(),
            rows,
            bands,
            width,
        })
    }

    /// Writes the next `rows` rows of every band (`data`: band after band, of the output's type).
    pub fn write(&mut self, data: RasterData, rows: usize) -> Result<()> {
        let Some(inner) = &mut self.inner else { return Err(Error::InvalidArgument("the raster is written".into())) };
        match inner {
            #[cfg(not(target_arch = "wasm32"))]
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
            #[cfg(target_arch = "wasm32")]
            Inner::Tiff(w) => w.write_rows(&data, rows, None),
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
            #[cfg(not(target_arch = "wasm32"))]
            Inner::Thread { tx, handle } => {
                drop(tx);
                match handle.map(|h| h.join()) {
                    Some(Ok(result)) => result,
                    _ => Err(Error::InvalidData("the GeoTIFF writer stopped".into())),
                }
            }
            #[cfg(target_arch = "wasm32")]
            Inner::Tiff(w) => w.finish(),
            Inner::Whole { ds, options, done } => {
                if done != self.rows {
                    return Err(Error::InvalidArgument(format!("{done} rows written of {}", self.rows)));
                }
                return crate::write_raster_dataset(&ds, &self.path, &options);
            }
        };
        if result.is_err() {
            let _ = std::fs::remove_file(&self.path);
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
            #[cfg(not(target_arch = "wasm32"))]
            Inner::Thread { tx, handle } => {
                drop(tx);
                if let Some(h) = handle {
                    let _ = h.join();
                }
            }
            #[cfg(target_arch = "wasm32")]
            Inner::Tiff(w) => drop(w),
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Runs `work` on `n` strips in order, `read` reading each: on another thread, a strip ahead of
/// the one worked on (in turn on WebAssembly).
pub(crate) fn pipelined<R: Send>(
    n: usize,
    mut read: impl FnMut(usize) -> Result<R> + Send,
    mut work: impl FnMut(usize, R) -> Result<()>,
) -> Result<()> {
    #[cfg(target_arch = "wasm32")]
    {
        for i in 0..n {
            work(i, read(i)?)?;
        }
        Ok(())
    }
    #[cfg(not(target_arch = "wasm32"))]
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
        drop(w);
        assert!(!std::path::Path::new(path).exists());
        let mut w = StripWriter::create(&header(DataType::U8), path, &Default::default()).unwrap();
        w.write(rows(), 3).unwrap();
        w.write(rows(), 3).unwrap();
        w.finish().unwrap();
        assert!(std::path::Path::new(path).exists());
    }

    /// Options the GeoTIFF writer refuses fail before any strip is computed.
    #[test]
    fn invalid_options_fail_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.tif");
        let options = RasterWriteOptions { compression: Some("JPEG".into()), ..Default::default() };
        assert!(StripWriter::create(&header(DataType::F32), path.to_str().unwrap(), &options).is_err());
    }
}

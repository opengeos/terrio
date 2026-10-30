// SPDX-FileCopyrightText: Copyright (c) 2007, ITC
// SPDX-FileCopyrightText: Copyright (c) 2008-2017, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL (bundled NOAA MDL degrib) v3.12.2 (public domain):
//     frmts/grib/degrib/degrib/grib2api.c
//     frmts/grib/degrib/degrib/metaparse.cpp
//   GDAL (bundled NCEP g2clib) v3.12.2 (public domain):
//     frmts/grib/degrib/g2clib/simunpack.c
//     frmts/grib/degrib/g2clib/comunpack.c
//     frmts/grib/degrib/g2clib/jpcunpack.c
//     frmts/grib/degrib/g2clib/pngunpack.c
//     frmts/grib/degrib/g2clib/g2_unpack7.c
//     frmts/grib/degrib/g2clib/g2_getfld.c
//   GDAL v3.12.2:
//     frmts/grib/gribdataset.cpp

//! GRIB (read only), decoded as GDAL's GRIB driver (GDAL 3.12, `frmts/grib`: degrib and NCEP's
//! g2clib) decodes it, so that pixels, nodata and georeferencing are GDAL's. GRIB1 messages are
//! read by [`grib1`], GRIB2 fields here.
//!
//! Each field of each GRIB2 message (each product), and each GRIB1 message, is a band, in file
//! order. GRIB2 values are unpacked in single precision in g2clib's order of operations
//! (`simunpack.c`, `comunpack.c`, `jpcunpack.c`, `pngunpack.c`, `g2_unpack7.c`), a bitmap expands
//! them to the grid with zeros at missing points (`g2_getfld.c`), and degrib reorders them to
//! scanning mode 0100, converts units (Kelvin to Celsius, as `GRIB_NORMALIZE_UNITS=YES`, GDAL's
//! default) and embeds missing values (`grib2api.c` `TransferFloat`, `metaparse.cpp`
//! `ParseGrid`): 9999 where a bitmap masks points, the primary missing value of complex packing.
//! Bands are Float64, flipped north up and split at the antimeridian as `gribdataset.cpp` reads
//! them. Georeferencing and the CRS follow `GRIBDataset::SetGribMetaData`, for regular, rotated
//! and Gaussian latitude/longitude grids (templates 3.0, 3.1 and 3.40), Mercator (3.10),
//! transverse Mercator (3.12), polar stereographic (3.20) and Lambert conformal (3.30) grids.
//!
//! Supported GRIB2 data representations: simple packing (5.0), complex packing with and without
//! spatial differencing (5.2, 5.3), IEEE floating point (5.4), JPEG 2000 (5.40, with the `jp2`
//! feature), PNG (5.41) and CCSDS (5.42, with the pure-Rust `rust-aec` decoder). Remote files are
//! read by range ([`remote`]). Every allocation sized by a header is fallible: a damaged or
//! hostile file is an error, never an abort.

use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use std::path::Path;
use wbraster::raster::RasterData;
use wbraster::{CrsInfo, DataType};

mod grib1;
mod names;
#[cfg(remote)]
mod remote;
mod tables;
pub(crate) mod write;

/// GDAL's missing value for points a bitmap masks (`metaparse.cpp` `ParseGrid`).
const BITMAP_MISSING: f64 = 9999.0;

/// The bits of the scanning mode flags (`GRIB2BIT_1`.. in degrib).
const SCAN_I_NEGATIVE: u8 = 0x80;
const SCAN_J_POSITIVE: u8 = 0x40;
const SCAN_J_CONSECUTIVE: u8 = 0x20;
const SCAN_BOUSTROPHEDON: u8 = 0x10;

/// Whether the file holds a GRIB message: "GRIB" in its first kilobyte, followed by an edition
/// number (1 or 2), as GDAL's identification looks for it.
pub(crate) fn is_grib(path: &Path) -> bool {
    let mut head = [0u8; 1024];
    let Ok(mut f) = std::fs::File::open(path) else { return false };
    let mut n = 0;
    while n < head.len() {
        match std::io::Read::read(&mut f, &mut head[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(_) => return false,
        }
    }
    is_grib_bytes(&head[..n])
}

/// Whether the bytes start (within their first kilobyte) a GRIB message.
pub(crate) fn is_grib_bytes(b: &[u8]) -> bool {
    let b = &b[..b.len().min(1024)];
    b.windows(8).any(|w| &w[..4] == b"GRIB" && (w[7] == 1 || w[7] == 2))
}

/// Reads a GRIB file: every field (GRIB2) or message (GRIB1) a Float64 band.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let bytes = std::fs::read(path)?;
    read_bytes(&bytes).map_err(|e| match e {
        Error::InvalidData(m) => Error::InvalidData(format!("{}: {m}", path.display())),
        e => e,
    })
}

/// Reads GRIB messages held in memory, every band decoded (in parallel, a group of bands at a
/// time, on the crate's pool).
pub(crate) fn read_bytes(bytes: &[u8]) -> Result<RasterDataset> {
    let bands = bands(bytes, normalize_units())?;
    let (mut ds, split) = header(&bands)?;
    let (cols, rows) = (ds.raster.cols, ds.raster.rows);
    let size =
        cols.checked_mul(rows).and_then(|n| n.checked_mul(bands.len())).ok_or_else(|| Error::InvalidData("grid too large".into()))?;
    let mut data = try_vec(size)?;
    let group = decode_group(cols, rows);
    for (g, chunk) in bands.chunks(group).enumerate() {
        let decoded = crate::par::map_range_coarse(chunk.len(), |i| decode_band(&chunk[i], g * group + i, bytes.len()));
        for d in decoded {
            place_band(&d?, cols, rows, split, &mut data);
        }
    }
    ds.raster.data = RasterData::F64(data);
    Ok(ds)
}

/// How many bands are decoded at once: two per thread of the pool, as many as the band cache of
/// a [`Source`] keeps (at least one).
fn decode_group(cols: usize, rows: usize) -> usize {
    let band_bytes = cols.saturating_mul(rows).saturating_mul(8).max(1);
    (crate::par::threads().max(1) * 2).min(CACHE_BYTES / band_bytes).max(1)
}

/// An empty vector with room for `n` values, or an error when the memory cannot be had: sizes
/// come from a file's headers, and a damaged or hostile file must not abort the process.
pub(crate) fn try_vec<T>(n: usize) -> Result<Vec<T>> {
    let mut out = Vec::new();
    out.try_reserve_exact(n)
        .map_err(|_| Error::Io(std::io::Error::new(std::io::ErrorKind::OutOfMemory, format!("GRIB: cannot allocate {n} values"))))?;
    Ok(out)
}

/// `n` copies of `v`, allocated as [`try_vec`].
pub(crate) fn try_filled<T: Clone>(n: usize, v: T) -> Result<Vec<T>> {
    let mut out = try_vec(n)?;
    out.resize(n, v);
    Ok(out)
}

/// A band of a GRIB file: a field of a GRIB2 message, or a GRIB1 message.
pub(crate) enum Band<'a> {
    Grib2(Field<'a>),
    Grib1(grib1::Message<'a>),
}

impl Band<'_> {
    fn grid(&self) -> Result<Grid> {
        match self {
            Band::Grib2(f) => Grid::parse(f.sect3),
            Band::Grib1(m) => m.grid(),
        }
    }

    /// The band's nodata, as GDAL reports it.
    fn nodata(&self) -> Result<Option<f64>> {
        match self {
            Band::Grib2(f) => Ok(nodata_of(f)),
            Band::Grib1(m) => m.nodata(),
        }
    }

    fn metadata(&self) -> Result<crate::BandMetadata> {
        match self {
            Band::Grib2(f) => Ok(tables::band_metadata(f)),
            Band::Grib1(m) => m.metadata(),
        }
    }

    fn decode(&self, file_len: usize) -> Result<Decoded> {
        match self {
            Band::Grib2(f) => decode_field(f, file_len),
            Band::Grib1(m) => Ok(m.decode()?.0),
        }
    }
}

/// The dataset of GRIB bands without their pixels, as GDAL opens it: the size, georeferencing
/// and CRS of the first band's grid, each band's description, metadata and nodata (GRIB2's from
/// the sections, without decoding the data, as GDAL's `FindNoDataGrib2`), and the antimeridian
/// split column.
fn header(bands: &[Band]) -> Result<(RasterDataset, usize)> {
    let first = bands.first().ok_or_else(|| Error::InvalidData("no GRIB message found".into()))?;
    let grid = first.grid()?;
    let georef = georeference(&grid);
    let (cols, rows) = (grid.nx as usize, grid.ny as usize);
    let band_nodata: Vec<Option<f64>> = bands.iter().map(Band::nodata).collect::<Result<_>>()?;
    let nodata = band_nodata[0];
    let raster = wbraster::Raster {
        cols,
        rows,
        bands: bands.len(),
        x_min: 0.0,
        y_min: 0.0,
        cell_size_x: 1.0,
        cell_size_y: 1.0,
        nodata: nodata.unwrap_or(-9999.0),
        data_type: DataType::F64,
        crs: georef.crs,
        metadata: Vec::new(),
        data: RasterData::F64(Vec::new()),
    };
    let mut ds = RasterDataset::with_driver(raster, RasterDriver::GRIB, nodata.is_some());
    ds.set_geo_transform(georef.gt);
    ds.band_metadata = bands.iter().map(Band::metadata).collect::<Result<_>>()?;
    // Each band's items in the dataset's metadata too, as `BAND_<n>#<key>`, as the GRIB driver of
    // #232 gave them (and as the netCDF driver gives its bands').
    ds.raster.metadata = ds
        .band_metadata
        .iter()
        .enumerate()
        .flat_map(|(i, b)| b.items.iter().map(move |(k, v)| (format!("BAND_{}#{k}", i + 1), v.clone())))
        .collect();
    if band_nodata.iter().any(|n| n.map(f64::to_bits) != nodata.map(f64::to_bits)) {
        ds.band_nodata = band_nodata;
    }
    Ok((ds, georef.split_column))
}

/// Decodes band `i` (from 0), its errors naming the band.
fn decode_band(b: &Band, i: usize, file_len: usize) -> Result<Decoded> {
    b.decode(file_len).map_err(|e| match e {
        Error::InvalidData(m) => Error::InvalidData(format!("band {}: {m}", i + 1)),
        e => e,
    })
}

/// Opens a GRIB file to be read band by band, as GDAL reads it: only the bands read are
/// decoded. Local files are found by extension or content; remote ones by extension, and read
/// by range (each message's header, then the messages of the bands read). `None` when `path` is
/// not a GRIB file.
pub(crate) fn open_lazy(path: &str) -> Option<Result<crate::LazyRaster>> {
    use crate::vfs::Location;
    let lower = path.to_ascii_lowercase();
    let lower = lower.split(['?', '#']).next().unwrap_or_default();
    let by_extension = RasterDriver::for_extension(lower);
    let location = Location::parse(path);
    let grib = match &location {
        Location::Local(p) => by_extension == Some(RasterDriver::GRIB) || (by_extension.is_none() && p.is_file() && is_grib(p)),
        // A remote file without a raster extension (NOAA's `gfs.t00z.pgrb2.1p00.f000`) by its
        // first bytes, so that it is read by range as a named one is, not downloaded whole.
        Location::Url(_) | Location::S3 { .. } => {
            by_extension == Some(RasterDriver::GRIB) || (by_extension.is_none() && crate::formats::remote_is_grib(&location))
        }
        _ => by_extension == Some(RasterDriver::GRIB),
    };
    if !grib {
        return None;
    }
    // Found by its content, not by a GRIB extension: read only if the GRIB driver is allowed.
    if by_extension != Some(RasterDriver::GRIB)
        && let Err(e) = super::check_recognised(RasterDriver::GRIB, path)
    {
        return Some(Err(e));
    }
    #[cfg(remote)]
    if let Some(remote) = location.remote().filter(|_| matches!(location, Location::Url(_) | Location::S3 { .. })) {
        return Some(remote::open(&remote).map_err(|e| match e {
            Error::InvalidData(m) => Error::InvalidData(format!("{path}: {m}")),
            e => e,
        }));
    }
    Some(crate::vfs::resolve(path).and_then(|local| open_lazy_bytes(std::fs::read(local.path())?.into())))
}

/// Opens GRIB messages held in memory to be read band by band.
pub(crate) fn open_lazy_bytes(bytes: std::sync::Arc<[u8]>) -> Result<crate::LazyRaster> {
    let normalize = normalize_units();
    let (header, split) = header(&bands(&bytes, normalize)?)?;
    let index = band_index(&bytes)?;
    let (cols, rows) = (header.raster.cols, header.raster.rows);
    let source = Source { store: Store::Bytes(bytes, index), cols, rows, split, normalize, cache: Vec::new() };
    Ok(crate::LazyRaster::from_source(header, Box::new(source)))
}

/// The bytes of decoded bands kept by a [`Source`], at least one band.
const CACHE_BYTES: usize = 256 << 20;

/// Where a [`Source`]'s messages are.
enum Store {
    /// The whole file, in memory, and where each band is: its message's offset and length, and
    /// its index among the message's bands.
    Bytes(std::sync::Arc<[u8]>, Vec<(usize, usize, usize)>),
    /// A remote file, each band's message fetched when the band is read.
    #[cfg(remote)]
    Remote(Box<remote::Messages>),
}

/// A GRIB file read band by band: the bands read are decoded whole, as GDAL decodes them, and
/// the last ones kept.
struct Source {
    store: Store,
    cols: usize,
    rows: usize,
    split: usize,
    /// `GRIB_NORMALIZE_UNITS` when the file was opened.
    normalize: bool,
    /// Decoded bands (from 1), north up, the most recently read last.
    cache: Vec<(usize, std::sync::Arc<Vec<f64>>)>,
}

/// Where each band of the messages in `b` is: `(message offset, message length, index of the
/// band among the message's bands)`, in band order, as [`bands`] finds them.
fn band_index(b: &[u8]) -> Result<Vec<(usize, usize, usize)>> {
    let mut out = Vec::new();
    for (start, len) in message_spans(b)? {
        let n = message_bands(&b[start..start + len], len, b.len(), true)?.len();
        out.extend((0..n).map(|k| (start, len, k)));
    }
    Ok(out)
}

/// Band `b` (from 1) of a file held in memory, decoded and placed as the dataset's band.
fn decode_placed(bytes: &[u8], at: (usize, usize, usize), b: usize, shape: (usize, usize, usize), normalize: bool) -> Result<Vec<f64>> {
    let (start, len, k) = at;
    let (cols, rows, split) = shape;
    let bands = message_bands(&bytes[start..start + len], len, bytes.len(), normalize)?;
    let band = bands.get(k).ok_or_else(|| Error::InvalidArgument(format!("band {b} out of range")))?;
    let decoded = decode_band(band, b - 1, bytes.len())?;
    let mut values = try_vec(cols * rows)?;
    place_band(&decoded, cols, rows, split, &mut values);
    Ok(values)
}

impl Source {
    fn cached(&mut self, b: usize) -> Option<std::sync::Arc<Vec<f64>>> {
        let i = self.cache.iter().position(|(k, _)| *k == b)?;
        let entry = self.cache.remove(i);
        self.cache.push(entry.clone());
        Some(entry.1)
    }

    fn keep(&mut self, b: usize, band: std::sync::Arc<Vec<f64>>) {
        let band_bytes = self.cols * self.rows * 8;
        while !self.cache.is_empty() && (self.cache.len() + 1) * band_bytes > CACHE_BYTES {
            self.cache.remove(0);
        }
        self.cache.push((b, band));
    }

    fn band(&mut self, b: usize) -> Result<std::sync::Arc<Vec<f64>>> {
        if let Some(band) = self.cached(b) {
            return Ok(band);
        }
        let out_of_range = || Error::InvalidArgument(format!("band {b} out of range"));
        let values = match &self.store {
            Store::Bytes(bytes, index) => {
                let at = *index.get(b.wrapping_sub(1)).ok_or_else(out_of_range)?;
                decode_placed(bytes, at, b, (self.cols, self.rows, self.split), self.normalize)?
            }
            #[cfg(remote)]
            Store::Remote(messages) => {
                let (message, k) = messages.band(b.wrapping_sub(1)).ok_or_else(out_of_range)?;
                let bytes = messages.fetch(message)?;
                let bands = message_bands(&bytes, bytes.len(), messages.file_len(), self.normalize)?;
                let decoded = decode_band(bands.get(k).ok_or_else(out_of_range)?, b - 1, messages.file_len())?;
                let mut values = try_vec(self.cols * self.rows)?;
                place_band(&decoded, self.cols, self.rows, self.split, &mut values);
                values
            }
        };
        let band = std::sync::Arc::new(values);
        self.keep(b, band.clone());
        Ok(band)
    }

    /// Decodes the bands of `bands` not in the cache, in parallel (a file held in memory; a
    /// remote file's bands are fetched one at a time by [`Source::band`]), into the cache.
    fn decode_missing(&mut self, bands: &[usize]) -> Result<()> {
        // Irrefutable in builds without `remote` (WebAssembly's), which hold files in memory only.
        #[allow(irrefutable_let_patterns)]
        let Store::Bytes(bytes, index) = &self.store else { return Ok(()) };
        let mut missing: Vec<usize> = bands.iter().copied().filter(|b| !self.cache.iter().any(|(k, _)| k == b)).collect();
        missing.sort_unstable();
        missing.dedup();
        if missing.len() < 2 {
            return Ok(());
        }
        let ats: Vec<(usize, usize, usize)> = missing
            .iter()
            .map(|&b| index.get(b.wrapping_sub(1)).copied().ok_or_else(|| Error::InvalidArgument(format!("band {b} out of range"))))
            .collect::<Result<_>>()?;
        let (bytes, shape, normalize) = (bytes.clone(), (self.cols, self.rows, self.split), self.normalize);
        let decoded = crate::par::map_range_coarse(missing.len(), |i| decode_placed(&bytes, ats[i], missing[i], shape, normalize));
        for (b, d) in missing.into_iter().zip(decoded) {
            self.keep(b, std::sync::Arc::new(d?));
        }
        Ok(())
    }
}

impl crate::cog::WindowSource for Source {
    fn overviews(&self) -> Vec<(usize, usize)> {
        Vec::new()
    }

    fn read(&mut self, level: usize, window: crate::Window, bands: &[usize]) -> Result<RasterData> {
        if level != 0 {
            return Err(Error::InvalidArgument("GRIB files have no overviews".into()));
        }
        if window.x + window.width > self.cols || window.y + window.height > self.rows {
            return Err(Error::InvalidArgument("window outside the raster".into()));
        }
        let mut out = try_vec(window.width * window.height * bands.len())?;
        for group in bands.chunks(decode_group(self.cols, self.rows)) {
            self.decode_missing(group)?;
            for &b in group {
                let band = self.band(b)?;
                for r in window.y..window.y + window.height {
                    out.extend_from_slice(&band[r * self.cols + window.x..r * self.cols + window.x + window.width]);
                }
            }
        }
        Ok(RasterData::F64(out))
    }
}

/// Copies a decoded grid (degrib's order: rows south to north) into a band of the dataset, as
/// `GRIBRasterBand::IReadBlock` does: flipped north up, rotated by the antimeridian split column,
/// and cropped or padded with zeros when the band's grid differs from the dataset's.
fn place_band(d: &Decoded, cols: usize, rows: usize, split: usize, out: &mut Vec<f64>) {
    let (gx, gy) = (d.nx, d.ny);
    let split = if gx != cols { 0 } else { split };
    let copy = cols.min(gx);
    for r in 0..rows {
        let start = out.len();
        out.resize(start + cols, 0.0);
        if r >= gy {
            continue;
        }
        let src = &d.values[(gy - r - 1) * gx..(gy - r) * gx];
        let row = &mut out[start..start + cols];
        row[..copy - split].copy_from_slice(&src[split..copy]);
        row[copy - split..copy].copy_from_slice(&src[..split]);
    }
}

/// The sections of one field: the latest of each kind before its data section.
pub(crate) struct Field<'a> {
    /// The discipline (section 0).
    pub discipline: u8,
    pub sect1: &'a [u8],
    pub sect3: &'a [u8],
    pub sect4: &'a [u8],
    pub sect5: &'a [u8],
    pub sect6: &'a [u8],
    /// The last bitmap (section 6 with indicator 0) of the message before this field, which an
    /// indicator of 254 refers to.
    pub previous_bitmap: Option<&'a [u8]>,
    pub sect7: &'a [u8],
    /// Whether units are normalised (`GRIB_NORMALIZE_UNITS`, see [`normalize_units`]).
    pub normalize: bool,
}

/// `GRIB_NORMALIZE_UNITS` (GDAL's `CPLTestBool`, YES by default): whether temperatures are
/// converted from Kelvin to Celsius and log10 concentrations to their values, with `[K]` read
/// `[C]` in the unit and comment. GDAL reads it per call; terrio when a file is opened, on the
/// opening thread (options are scoped to a thread, and bands decode on the pool's).
pub(crate) fn normalize_units() -> bool {
    crate::config::get("GRIB_NORMALIZE_UNITS")
        .is_none_or(|v| !matches!(v.trim().to_ascii_uppercase().as_str(), "NO" | "FALSE" | "OFF" | "0"))
}

fn be(b: &[u8]) -> u64 {
    b.iter().fold(0u64, |v, &x| (v << 8) | x as u64)
}

/// The bands of every message, in file order: each field of a GRIB2 message, each GRIB1
/// message. Bytes before and between messages are skipped, as GDAL's inventory skips them.
fn bands(b: &[u8], normalize: bool) -> Result<Vec<Band<'_>>> {
    let mut out = Vec::new();
    for (start, len) in message_spans(b)? {
        out.extend(message_bands(&b[start..start + len], len, b.len(), normalize)?);
    }
    Ok(out)
}

/// The offset and length of every message, in file order.
fn message_spans(b: &[u8]) -> Result<Vec<(usize, usize)>> {
    let mut out = Vec::new();
    let mut pos = 0;
    while let Some(off) = b[pos..].windows(4).position(|w| w == b"GRIB") {
        let start = pos + off;
        if start + 16 > b.len() {
            break;
        }
        let len = match b[start + 7] {
            2 => be(&b[start + 8..start + 16]) as usize,
            1 => grib1::message_len(&b[start..])?,
            _ => {
                pos = start + 4;
                continue;
            }
        };
        if len < 16 || b.get(start..start.saturating_add(len)).is_none() {
            return Err(Error::InvalidData(format!("GRIB message at offset {start} is truncated")));
        }
        out.push((start, len));
        pos = start + len;
    }
    Ok(out)
}

/// The bands of one message of a file of `file_len` bytes. `msg` is the message, or the start
/// of a message of `full_len` bytes holding the headers of all its sections (a remote file's).
fn message_bands(msg: &[u8], full_len: usize, file_len: usize, normalize: bool) -> Result<Vec<Band<'_>>> {
    if msg.get(7) == Some(&1) {
        let mut m = grib1::Message::parse(msg, file_len)?;
        m.normalize = normalize;
        return Ok(vec![Band::Grib1(m)]);
    }
    let mut fields = Vec::new();
    message_fields(msg, full_len, normalize, &mut fields)?;
    Ok(fields.into_iter().map(Band::Grib2).collect())
}

/// The fields of a GRIB2 message. When only the start of the message is at hand (`msg` shorter
/// than `full_len`), its last section may be cut short: a bitmap or data section, not read.
fn message_fields<'a>(msg: &'a [u8], full_len: usize, normalize: bool, out: &mut Vec<Field<'a>>) -> Result<()> {
    let discipline = msg[6];
    let mut pos = 16;
    let empty: &[u8] = &[];
    let (mut s1, mut s3, mut s4, mut s5, mut s6) = (empty, empty, empty, empty, empty);
    let mut last_bitmap: Option<&[u8]> = None;
    while pos + 4 <= msg.len() && &msg[pos..pos + 4] != b"7777" {
        let bad = || Error::InvalidData(format!("invalid GRIB2 section at offset {pos} of a message"));
        if pos + 5 > msg.len() {
            return Err(bad());
        }
        let len = be(&msg[pos..pos + 4]) as usize;
        let sect = match msg.get(pos..pos + len) {
            Some(s) if len >= 5 => s,
            // The start of a bitmap or data section past what was read of a message.
            None if full_len > msg.len()
                && pos + len <= full_len
                && matches!(msg[pos + 4], 2 | 6 | 7)
                && msg.len() >= pos + if msg[pos + 4] == 6 { 6 } else { 5 } =>
            {
                &msg[pos..]
            }
            _ => return Err(bad()),
        };
        match sect[4] {
            1 => s1 = sect,
            2 => {}
            3 => s3 = sect,
            4 => s4 = sect,
            5 => s5 = sect,
            6 => s6 = sect,
            7 => {
                if s1.is_empty() || s3.is_empty() || s4.is_empty() || s5.is_empty() || s6.is_empty() {
                    return Err(Error::InvalidData("GRIB2 data section without its definitions".into()));
                }
                out.push(Field {
                    discipline,
                    sect1: s1,
                    sect3: s3,
                    sect4: s4,
                    sect5: s5,
                    sect6: s6,
                    previous_bitmap: last_bitmap,
                    sect7: sect,
                    normalize,
                });
                if s6.get(5) == Some(&0) {
                    last_bitmap = Some(s6);
                }
            }
            _ => return Err(bad()),
        }
        pos += len;
    }
    Ok(())
}

/// How much of the start of a GRIB message of `full_len` bytes its header needs (every section's
/// header, the definitions whole, a bitmap's indicator), given the `head` at hand: `None` when
/// the head holds it, else at least how many bytes.
fn header_needs(head: &[u8], full_len: usize) -> Option<usize> {
    if head.len() < 16 {
        return Some(16.min(full_len));
    }
    if head[7] == 1 {
        return grib1::Message::header_needs(head).filter(|&n| n > head.len()).map(|n| n.min(full_len));
    }
    let mut pos = 16;
    while pos + 4 < full_len {
        if pos + 5 > head.len() {
            return Some(pos + 5);
        }
        let len = be(&head[pos..pos + 4]) as usize;
        let need = match head[pos + 4] {
            2 | 7 => pos + 5,
            6 => pos + 6,
            _ => pos.saturating_add(len),
        };
        if need > head.len() {
            return Some(need.min(full_len));
        }
        if len < 5 {
            return None;
        }
        pos += len;
    }
    None
}

/// Reads octets `o..o + n` (1-based, as the GRIB2 tables number them) of a section, unsigned.
/// g2clib keeps them in 32-bit signed integers: larger 4-octet values wrap.
fn uoct(s: &[u8], o: usize, n: usize) -> i32 {
    s.get(o - 1..o - 1 + n).map_or(0, |b| be(b) as u32 as i32)
}

/// Reads octets `o..o + n` of a section as a sign-and-magnitude integer, as g2clib reads the
/// template entries its maps mark negative.
fn soct(s: &[u8], o: usize, n: usize) -> i32 {
    let v = be(s.get(o - 1..o - 1 + n).unwrap_or_default());
    let sign = 1u64 << (8 * n - 1);
    let mag = (v & (sign - 1)) as i64;
    (if v & sign != 0 { -mag } else { mag }) as i32
}

/// A grid definition (section 3), as degrib's `ParseSect3` reads it.
#[derive(Debug, Clone)]
pub(crate) struct Grid {
    pub template: u16,
    pub nx: u32,
    pub ny: u32,
    pub lat1: f64,
    pub lon1: f64,
    pub lat2: f64,
    pub lon2: f64,
    pub dx: f64,
    pub dy: f64,
    /// The scanning mode as read, before degrib reorders the data to mode 0100.
    pub scan: u8,
    pub sphere: bool,
    /// The semi-axes in km; -1 when the file's are invalid.
    pub maj_earth: f64,
    pub min_earth: f64,
    pub mesh_lat: f64,
    pub orient_lon: f64,
    pub scale_lat1: f64,
    pub scale_lat2: f64,
    /// The octet of the scanning mode in the section (degrib's `scanIndex + 1`), for the templates
    /// degrib reorders.
    scan_octet: Option<usize>,
    /// The scanning mode `SetGribMetaData` compares: GRIB2's after degrib reordered the points,
    /// GRIB1's as read.
    pub gdal_scan: u8,
    /// Rotated latitude/longitude grids: the southern pole and the rotation (degrees).
    pub south_lat: f64,
    pub south_lon: f64,
    pub angle_rotate: f64,
    /// Transverse Mercator grids (template 3.12).
    pub tm: Option<TransverseMercator>,
}

/// The projection and extent of a transverse Mercator grid, as degrib's `ParseSect3` reads them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TransverseMercator {
    pub lat0: f64,
    pub lon0: f64,
    pub k: f64,
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y2: f64,
}

impl Grid {
    pub(crate) fn new(nx: u32, ny: u32, sphere: bool, maj_earth: f64, min_earth: f64) -> Grid {
        Grid {
            template: 0,
            nx,
            ny,
            lat1: 0.0,
            lon1: 0.0,
            lat2: 0.0,
            lon2: 0.0,
            dx: 0.0,
            dy: 0.0,
            scan: 0,
            sphere,
            maj_earth,
            min_earth,
            mesh_lat: 0.0,
            orient_lon: 0.0,
            scale_lat1: 0.0,
            scale_lat2: 0.0,
            scan_octet: None,
            gdal_scan: 0,
            south_lat: 0.0,
            south_lon: 0.0,
            angle_rotate: 0.0,
            tm: None,
        }
    }

    pub(crate) fn parse(s: &[u8]) -> Result<Grid> {
        let template = uoct(s, 13, 2) as u16;
        let unsupported = || Error::Unsupported(format!("GRIB2 grid definition template 3.{template} is not supported"));
        let short = || Error::InvalidData(format!("GRIB2 grid definition template 3.{template} is truncated"));
        let need = match template {
            0 | 40 | 10 => 72,
            1 | 12 => 84,
            20 => 65,
            30 | 31 => 81,
            140 => 64,
            _ => return Err(unsupported()),
        };
        if s.len() < need {
            return Err(short());
        }
        let num_pts = uoct(s, 7, 4) as u32;
        let (sphere, mut maj, mut min) = earth(s)?;
        if !(6300.0..=6400.0).contains(&maj) || !(6300.0..=6400.0).contains(&min) {
            maj = -1.0;
            min = -1.0;
        }
        let nx = uoct(s, 31, 4) as u32;
        let ny = uoct(s, 35, 4) as u32;
        if (nx != 0 && ny > u32::MAX / nx) || nx.wrapping_mul(ny) != num_pts {
            return Err(Error::InvalidData("Nx * Ny != number of points".into()));
        }
        if nx == 0 || ny == 0 || nx > i32::MAX as u32 || ny > i32::MAX as u32 {
            return Err(Error::InvalidData("invalid grid dimension".into()));
        }
        let mut g = Grid::new(nx, ny, sphere, maj, min);
        g.template = template;
        let mut unit = 1e-6;
        // Whether degrib's `ParseSect3` stopped early (see below).
        let mut stopped = true;
        // The signs of the longitudes follow g2clib's template maps: sign and magnitude in 3.0,
        // 3.1, 3.10 and 3.20, unsigned in 3.30 and 3.40.
        let lon = |o: usize| if matches!(template, 0 | 1 | 10 | 20) { soct(s, o, 4) } else { uoct(s, o, 4) };
        match template {
            0 | 1 | 40 => 'grid: {
                // The data are reordered by the scanning mode whatever degrib makes of the rest.
                g.scan = uoct(s, 72, 1) as u8;
                g.scan_octet = Some(72);
                // degrib's `ParseSect3` stops at a subdivision of 0, at a missing first or last point
                // or increment (GRIB2MISSING_s4: all ones in an octet group g2clib reads signed),
                // and at a Gaussian grid without parallels. GDAL ignores the error and reads the
                // grid with what was parsed until then, the rest zeros (a geotransform of zeros).
                let (angle, subdivision) = (uoct(s, 39, 4), uoct(s, 43, 4));
                if angle != 0 {
                    if subdivision == 0 {
                        break 'grid;
                    }
                    unit = angle as f64 / subdivision as f64;
                }
                if [soct(s, 47, 4), lon(51), soct(s, 56, 4), lon(60), uoct(s, 64, 4), uoct(s, 68, 4)].contains(&-i32::MAX) {
                    break 'grid;
                }
                g.lat1 = soct(s, 47, 4) as f64 * unit;
                g.lon1 = lon(51) as f64 * unit;
                let res = uoct(s, 55, 1) as u8;
                g.lat2 = soct(s, 56, 4) as f64 * unit;
                g.lon2 = lon(60) as f64 * unit;
                g.dx = uoct(s, 64, 4) as f64 * unit;
                g.dy = if template == 40 {
                    let np = uoct(s, 68, 4);
                    if np == 0 {
                        break 'grid;
                    }
                    90.0 / np as f64
                } else {
                    uoct(s, 68, 4) as f64 * unit
                };
                if template == 1 {
                    g.south_lat = soct(s, 73, 4) as f64 * unit;
                    g.south_lon = uoct(s, 77, 4) as f64 * unit;
                    g.angle_rotate = uoct(s, 81, 4) as f64 * unit;
                }
                if res & 0x20 != 0 && res & 0x10 == 0 {
                    g.dy = g.dx;
                } else if res & 0x20 == 0 && res & 0x10 != 0 {
                    g.dx = g.dy;
                }
                stopped = false;
            }
            10 => {
                g.lat1 = soct(s, 39, 4) as f64 * unit;
                g.lon1 = lon(43) as f64 * unit;
                let res = uoct(s, 47, 1) as u8;
                g.mesh_lat = soct(s, 48, 4) as f64 * unit;
                g.lat2 = soct(s, 52, 4) as f64 * unit;
                g.lon2 = lon(56) as f64 * unit;
                g.scan = uoct(s, 60, 1) as u8;
                g.scan_octet = Some(60);
                g.orient_lon = uoct(s, 61, 4) as f64 * unit;
                g.dx = uoct(s, 65, 4) as f64 / 1000.0;
                g.dy = uoct(s, 69, 4) as f64 / 1000.0;
                if res & 0x20 != 0 && res & 0x10 == 0 {
                    g.dy = g.dx;
                } else if res & 0x20 == 0 && res & 0x10 != 0 {
                    g.dx = g.dy;
                }
            }
            20 => {
                g.lat1 = soct(s, 39, 4) as f64 * unit;
                g.lon1 = lon(43) as f64 * unit;
                g.mesh_lat = soct(s, 48, 4) as f64 * unit;
                g.orient_lon = soct(s, 52, 4) as f64 * unit;
                g.dx = uoct(s, 56, 4) as f64 / 1000.0;
                g.dy = uoct(s, 60, 4) as f64 / 1000.0;
                let center = uoct(s, 64, 1) as u8;
                g.scale_lat1 = if center & 0x80 != 0 { -90.0 } else { 90.0 };
                g.scale_lat2 = g.scale_lat1;
                if center & 0x40 != 0 {
                    return Err(Error::Unsupported("bi-polar stereographic GRIB2 grids are not supported".into()));
                }
                g.scan = uoct(s, 65, 1) as u8;
                g.scan_octet = Some(65);
            }
            // Lambert conformal (3.30) and Albers equal area (3.31) conics: the same octets.
            30 | 31 => {
                g.lat1 = soct(s, 39, 4) as f64 * unit;
                g.lon1 = uoct(s, 43, 4) as f64 * unit;
                g.mesh_lat = soct(s, 48, 4) as f64 * unit;
                g.orient_lon = uoct(s, 52, 4) as f64 * unit;
                g.dx = uoct(s, 56, 4) as f64 / 1000.0;
                g.dy = uoct(s, 60, 4) as f64 / 1000.0;
                g.scan = uoct(s, 65, 1) as u8;
                g.scan_octet = Some(65);
                g.scale_lat1 = soct(s, 66, 4) as f64 * unit;
                g.scale_lat2 = soct(s, 70, 4) as f64 * unit;
            }
            // Lambert azimuthal equal area: latitude and longitude of the first point and of the
            // projection's centre, signed (g2clib's map for 3.140).
            140 => {
                g.lat1 = soct(s, 39, 4) as f64 * unit;
                g.lon1 = soct(s, 43, 4) as f64 * unit;
                g.mesh_lat = soct(s, 47, 4) as f64 * unit;
                g.orient_lon = soct(s, 51, 4) as f64 * unit;
                g.dx = uoct(s, 56, 4) as f64 / 1000.0;
                g.dy = uoct(s, 60, 4) as f64 / 1000.0;
                g.scan = uoct(s, 64, 1) as u8;
                g.scan_octet = Some(64);
            }
            12 => {
                let k = f32::from_bits(be(&s[47..51]) as u32) as f64;
                g.tm = Some(TransverseMercator {
                    lat0: soct(s, 39, 4) as f64 * unit,
                    lon0: uoct(s, 43, 4) as f64 * unit,
                    k: if (k - 0.9996).abs() < 1e-8 { 0.9996 } else { k },
                    x0: soct(s, 52, 4) as f64 / 100.0,
                    y0: soct(s, 56, 4) as f64 / 100.0,
                    x1: soct(s, 69, 4) as f64 / 100.0,
                    y2: soct(s, 81, 4) as f64 / 100.0,
                });
                g.scan = uoct(s, 60, 1) as u8;
                g.scan_octet = Some(60);
                g.dx = uoct(s, 61, 4) as f64 / 100.0;
                g.dy = uoct(s, 65, 4) as f64 / 100.0;
            }
            _ => return Err(unsupported()),
        }
        // The scanning mode degrib keeps (`meta->gds.scan`): mode 0100 once the points are
        // reordered, or 0 when `ParseSect3` stopped before reading it.
        g.gdal_scan = if stopped && matches!(template, 0 | 1 | 40) { 0 } else { g.reordered_scan() };
        Ok(g)
    }

    /// The scanning mode after degrib reordered the points to mode 0100 (`TransferFloat`).
    fn reordered_scan(&self) -> u8 {
        SCAN_J_POSITIVE + (self.scan & 0x0f)
    }
}

/// The shape of the earth (octets 15-30 of section 3), as degrib's `ParseSect3` reads it:
/// whether a sphere, and the semi-major and semi-minor axes in km.
fn earth(s: &[u8]) -> Result<(bool, f64, f64)> {
    let scaled = |sf: i32, sv: i32| -> Option<(f64, i32, i32)> {
        // Missing: all ones, which g2clib reads as -1 (4 octets) or 255 (1 octet).
        (sv != -1 && sf != 255).then(|| (10f64.powi(sf), sf, sv))
    };
    Ok(match uoct(s, 15, 1) {
        0 => (true, 6367.47, 6367.47),
        6 => (true, 6371.229, 6371.229),
        1 => {
            let (sf, sv) = (uoct(s, 16, 1), uoct(s, 17, 4));
            if !(sv > 0 && sf != 255) {
                // degrib reports the missing radius and parses on; the shape is then invalid,
                // and GDAL reads the grid without georeferencing.
                return Ok((true, -1.0, -1.0));
            }
            let mut r = sv as f64 / (10f64.powi(sf) * 1000.0);
            if r < 6.4 {
                r *= 1000.0;
            }
            (true, r, r)
        }
        2 => (false, 6378.160, 6356.775),
        4 => (false, 6378.137, 6378.137 * (1.0 - 1.0 / 298.257222101)),
        5 => (false, 6378.137, 6378.137 * (1.0 - 1.0 / 298.257223563)),
        c @ (3 | 7) => {
            let (Some((dmaj, ..)), Some((dmin, ..))) = (scaled(uoct(s, 21, 1), uoct(s, 22, 4)), scaled(uoct(s, 26, 1), uoct(s, 27, 4)))
            else {
                // As a missing radius: no georeferencing.
                return Ok((false, -1.0, -1.0));
            };
            let k = if c == 7 { 1000.0 } else { 1.0 };
            let mut maj = uoct(s, 22, 4) as f64 / (dmaj * k);
            let mut min = uoct(s, 27, 4) as f64 / (dmin * k);
            if c == 3 {
                if maj > 6400.0 {
                    maj /= 1000.0;
                }
                if min > 6400.0 {
                    min /= 1000.0;
                }
            } else {
                if maj < 6.4 {
                    maj *= 1000.0;
                }
                if min < 6.4 {
                    min *= 1000.0;
                }
            }
            (false, maj, min)
        }
        8 => (true, 6371.2, 6371.2),
        c => return Err(Error::Unsupported(format!("GRIB2 shape of the earth {c} is not supported"))),
    })
}

/// The georeferencing of a dataset, from its first band's grid.
struct Georef {
    gt: [f64; 6],
    crs: CrsInfo,
    /// The first column east of the antimeridian, which GDAL moves to the left edge of 0-360
    /// global grids; 0 for none.
    split_column: usize,
}

fn lon360to180(lon: f64) -> f64 {
    if lon == 180.0 { 180.0 } else { (lon + 180.0) % 360.0 - 180.0 }
}

/// The geotransform and CRS GDAL's `GRIBDataset::SetGribMetaData` computes.
fn georeference(g: &Grid) -> Georef {
    let mut out = Georef { gt: [0.0, 1.0, 0.0, 0.0, 0.0, 1.0], crs: CrsInfo::default(), split_column: 0 };
    let Some(ellipsoid) = Ellipsoid::of(g) else { return out };
    // GDAL does not georeference Mercator grids at an angle to the equator.
    if g.template == 10 && g.orient_lon != 0.0 {
        return out;
    }
    let projected = matches!(g.template, 10 | 20 | 30 | 31 | 140);
    let (mut min_x, mut max_y, px, py);
    if let Some(tm) = g.tm {
        min_x = tm.x1;
        max_y = tm.y2;
        px = g.dx;
        py = g.dy;
        let wkt = Projection::Transverse { lat0: tm.lat0, lon0: lon360to180(tm.lon0), k: tm.k, x0: tm.x0, y0: tm.y0 }.wkt(&ellipsoid);
        out.crs = CrsInfo { epsg: None, wkt: Some(wkt), proj4: None };
    } else if projected {
        let proj = Projection::of(g, &ellipsoid);
        let (x, y) = proj.forward(&ellipsoid, lon360to180(g.lon1), g.lat1);
        if !x.is_finite() || !y.is_finite() {
            // GDAL keeps no georeferencing when the first point does not project.
            return out;
        }
        min_x = x;
        // degrib reordered the points to scanning mode 0100: the first is the southern-most,
        // unless GDAL's heuristic finds a grid registered by its northern-most point.
        max_y = if g.gdal_scan == SCAN_J_POSITIVE && first_point_is_southern_most(g, &proj, &ellipsoid, x, y) {
            y + (g.ny as f64 - 1.0) * g.dy
        } else {
            y
        };
        px = g.dx;
        py = g.dy;
        out.crs = CrsInfo { epsg: None, wkt: Some(proj.wkt(&ellipsoid)), proj4: None };
    } else {
        min_x = g.lon1;
        max_y = g.lat1;
        let mut min_y = g.lat2;
        let max_x = g.lon2;
        if g.lat2 > max_y {
            max_y = g.lat2;
            min_y = g.lat1;
        }
        let nx = g.nx as f64;
        let mut sx = if g.nx == 1 {
            g.dx
        } else if g.lon1 > g.lon2 {
            (360.0 - (g.lon1 - g.lon2)) / (nx - 1.0)
        } else {
            (g.lon2 - g.lon1) / (nx - 1.0)
        };
        let mut sy = if g.ny == 1 { g.dy } else { (max_y - min_y) / (g.ny as f64 - 1.0) };
        if sx < 0.0 || (sx - g.dx).abs() > 0.002 {
            sx = g.dx;
        }
        if sy < 0.0 || (sy - g.dy).abs() > 0.002 {
            sy = g.dy;
        }
        // GDAL's GRIB_ADJUST_LONGITUDE_RANGE=YES (the default): 0-360 longitudes to -180-180.
        if min_x + sx >= 180.0 || max_x - sx >= 180.0 {
            if sx * nx > 360.0 + sx / 4.0 {
            } else if min_x == 180.0 {
                min_x = -180.0;
            } else if (360.0 - sx * nx).abs() < sx / 4.0 && min_x <= 180.0 && g.template == 0 {
                let candidate = ((180.0 - min_x) / sx).ceil() as i32;
                if candidate < g.nx as i32 {
                    out.split_column = candidate.max(0) as usize;
                    min_x = -180.0;
                }
            } else {
                min_x = lon360to180(min_x);
            }
        }
        px = sx;
        py = sy;
        out.crs = if g.template == 1 {
            ellipsoid.rotated_crs(g.south_lat, lon360to180(g.south_lon), g.angle_rotate)
        } else {
            ellipsoid.geographic_crs(g.template == 0)
        };
    }
    min_x -= px / 2.0;
    max_y += py / 2.0;
    out.gt = [min_x, px, 0.0, max_y, 0.0, -py];
    out
}

/// Whether the first point of a projected grid in scanning mode 0100 is its southern-most, as
/// GDAL's `SetGribMetaData` decides: `GRIB_LATITUDE_OF_FIRST_GRID_POINT_IS_SOUTHERN_MOST` when set,
/// else yes, but for the Lambert grids of JMA's MANAL products (latitude of origin 30, central
/// meridian 140, standard parallels 60 and 30) whose centre is ten times nearer the projection's
/// centre when the first point is taken as the northern-most.
fn first_point_is_southern_most(g: &Grid, proj: &Projection, e: &Ellipsoid, x: f64, y: f64) -> bool {
    if let Some(v) = crate::config::get("GRIB_LATITUDE_OF_FIRST_GRID_POINT_IS_SOUTHERN_MOST") {
        // CPLTestBool.
        return !matches!(v.trim().to_ascii_uppercase().as_str(), "NO" | "FALSE" | "OFF" | "0");
    }
    let near = |a: f64, b: f64| (a - b).abs() <= 1e-8;
    if g.template != 30
        || !near(g.scale_lat1, 60.0)
        || !near(g.scale_lat2, 30.0)
        || !near(g.mesh_lat, 30.0)
        || !near(lon360to180(g.orient_lon), 140.0)
    {
        return true;
    }
    let (cx, cy) = proj.forward(e, lon360to180(g.orient_lon), g.mesh_lat);
    let nominal = (x + g.nx as f64 * g.dx / 2.0, y + g.ny as f64 * g.dy / 2.0);
    let buggy = (nominal.0, y - g.ny as f64 * g.dy / 2.0);
    let sq = |v: f64| v * v;
    sq(buggy.0 - cx) + sq(buggy.1 - cy) >= sq(10.0) * (sq(nominal.0 - cx) + sq(nominal.1 - cy))
}

/// The ellipsoid of a grid, as `SetGribMetaData` derives it (in metres).
struct Ellipsoid {
    a: f64,
    b: f64,
    sphere: bool,
}

impl Ellipsoid {
    /// `None` when the file's shape of the earth is invalid, where GDAL reports no georeferencing.
    fn of(g: &Grid) -> Option<Ellipsoid> {
        if g.maj_earth < 0.0 || g.min_earth < 0.0 {
            return None;
        }
        let have = g.maj_earth > 0.0 && g.min_earth > 0.0;
        let a = if have { g.maj_earth * 1.0e3 } else { 6377563.396 };
        let b = if have {
            g.min_earth * 1.0e3
        } else if g.sphere {
            a
        } else {
            6356256.910
        };
        Some(Ellipsoid { a, b, sphere: g.sphere || a == b })
    }

    fn inv_flattening(&self) -> f64 {
        self.a / (self.a - self.b)
    }

    fn is_wgs84(&self) -> bool {
        !self.sphere && (self.a - 6378137.0).abs() < 0.01 && (self.inv_flattening() - 298.257223563).abs() < 1e-9
    }

    fn is_grs80(&self) -> bool {
        !self.sphere && (self.a - 6378137.0).abs() < 0.01 && (self.inv_flattening() - 298.257222101).abs() < 1e-9
    }

    /// The `GEOGCS` GDAL builds (`SetGeogCS`), without axes.
    fn geogcs(&self) -> String {
        let (datum, spheroid, a, rf) = if self.sphere {
            ("unnamed", "Sphere", self.a, 0.0)
        } else if self.is_wgs84() {
            ("WGS_1984", "WGS 84", 6378137.0, 298.257223563)
        } else if self.is_grs80() {
            ("unnamed", "GRS80", 6378137.0, 298.257222101)
        } else {
            ("unnamed", "Spheroid imported from GRIB file", self.a, self.inv_flattening())
        };
        format!(
            "GEOGCS[\"Coordinate System imported from GRIB file\",DATUM[\"{datum}\",SPHEROID[\"{spheroid}\",{},{}]],\
             PRIMEM[\"Greenwich\",0],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]]",
            num(a),
            num(rf)
        )
    }

    /// The CRS of a rotated latitude/longitude grid, as GDAL's
    /// `SetDerivedGeogCRSWithPoleRotationGRIBConvention` exports it (WKT2).
    fn rotated_crs(&self, south_lat: f64, south_lon: f64, angle: f64) -> CrsInfo {
        const DEG: &str = "ANGLEUNIT[\"degree\",0.0174532925199433,ID[\"EPSG\",9122]]";
        let (datum, spheroid, a, rf) = if self.sphere {
            ("unnamed", "Sphere", self.a, 0.0)
        } else if self.is_wgs84() {
            ("World Geodetic System 1984", "WGS 84", 6378137.0, 298.257223563)
        } else if self.is_grs80() {
            ("unnamed", "GRS80", 6378137.0, 298.257222101)
        } else {
            ("unnamed", "Spheroid imported from GRIB file", self.a, self.inv_flattening())
        };
        let name = "Coordinate System imported from GRIB file";
        let p = |n: &str, v: f64| format!("PARAMETER[\"{n} (GRIB convention)\",{},{DEG}]", num(v));
        let wkt = format!(
            "GEOGCRS[\"{name}\",BASEGEOGCRS[\"{name}\",DATUM[\"{datum}\",ELLIPSOID[\"{spheroid}\",{},{},LENGTHUNIT[\"metre\",1,ID[\"EPSG\",9001]]]],\
             PRIMEM[\"Greenwich\",0,{DEG}]],DERIVINGCONVERSION[\"Pole rotation (GRIB convention)\",\
             METHOD[\"Pole rotation (GRIB convention)\"],{},{},{}],CS[ellipsoidal,2],\
             AXIS[\"latitude\",north,ORDER[1],{DEG}],AXIS[\"longitude\",east,ORDER[2],{DEG}]]",
            num(a),
            num(rf),
            p("Latitude of the southern pole", south_lat),
            p("Longitude of the southern pole", south_lon),
            p("Axis rotation", angle),
        );
        CrsInfo { epsg: None, wkt: Some(wkt), proj4: None }
    }

    /// The CRS of a latitude/longitude grid: EPSG:4326 for WGS 84 regular grids, as GDAL.
    fn geographic_crs(&self, regular: bool) -> CrsInfo {
        if regular && self.is_wgs84() {
            return CrsInfo::from_epsg(4326);
        }
        let wkt = format!("{},AXIS[\"Latitude\",NORTH],AXIS[\"Longitude\",EAST]]", self.geogcs());
        CrsInfo { epsg: None, wkt: Some(wkt), proj4: None }
    }
}

/// A number as GDAL writes it in WKT: integers as such, else 15 significant digits (PROJ's WKT
/// formatter, as C's `%.15g`), so that an inverse flattening such as Clarke 1866's is written
/// `294.978698213911`, not `294.97869821391095`.
fn num(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return format!("{}", v as i64);
    }
    if !v.is_finite() {
        return format!("{v}");
    }
    let sci = format!("{v:.14e}");
    let exp: i32 = sci.split_once('e').and_then(|(_, e)| e.parse().ok()).unwrap_or(0);
    let trim = |s: String| -> String { if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s } };
    if (-5..15).contains(&exp) {
        trim(format!("{v:.*}", (14 - exp).max(0) as usize))
    } else {
        let (mant, _) = sci.split_once('e').unwrap_or((&sci, ""));
        format!("{}e{}{:02}", trim(mant.to_string()), if exp < 0 { '-' } else { '+' }, exp.abs())
    }
}

/// The projection of a projected grid, as `SetGribMetaData` sets it (`SetMercator`, `SetPS`,
/// `SetLCC`), with PROJ's forward formulas (`merc.cpp`, `stere.cpp`, `lcc.cpp`) for the grid's
/// first point.
enum Projection {
    /// Mercator: one standard parallel (`SetMercator`) when the latitude of true scale is 0, else
    /// two (`SetMercator2SP`).
    Mercator { lat_ts: f64 },
    /// Polar stereographic: latitude of true scale, central meridian.
    Polar { lat_ts: f64, lon0: f64 },
    /// Lambert conformal conic, two standard parallels: lat_1, lat_2, latitude and longitude of
    /// origin.
    Lambert { lat1: f64, lat2: f64, lat0: f64, lon0: f64 },
    /// Transverse Mercator (`SetTM`), whose grids give their extent in metres (no `forward`).
    Transverse { lat0: f64, lon0: f64, k: f64, x0: f64, y0: f64 },
    /// Albers conic equal area (`SetACEA`): standard parallels, latitude and longitude of the
    /// centre.
    Albers { lat1: f64, lat2: f64, lat0: f64, lon0: f64 },
    /// Lambert azimuthal equal area (`SetLAEA`): latitude and longitude of the centre.
    Laea { lat0: f64, lon0: f64 },
}

impl Projection {
    fn of(g: &Grid, _e: &Ellipsoid) -> Projection {
        match g.template {
            10 => Projection::Mercator { lat_ts: g.mesh_lat },
            20 => Projection::Polar { lat_ts: g.mesh_lat, lon0: g.orient_lon },
            31 => Projection::Albers { lat1: g.scale_lat1, lat2: g.scale_lat2, lat0: g.mesh_lat, lon0: lon360to180(g.orient_lon) },
            140 => Projection::Laea { lat0: g.mesh_lat, lon0: lon360to180(g.orient_lon) },
            _ => Projection::Lambert { lat1: g.scale_lat1, lat2: g.scale_lat2, lat0: g.mesh_lat, lon0: lon360to180(g.orient_lon) },
        }
    }

    fn wkt(&self, e: &Ellipsoid) -> String {
        let p = |name: &str, v: f64| format!("PARAMETER[\"{name}\",{}]", num(v));
        let (method, params) = match *self {
            Projection::Mercator { lat_ts } => {
                if lat_ts == 0.0 {
                    (
                        "Mercator_1SP",
                        vec![p("central_meridian", 0.0), p("scale_factor", 1.0), p("false_easting", 0.0), p("false_northing", 0.0)],
                    )
                } else {
                    (
                        "Mercator_2SP",
                        vec![
                            p("standard_parallel_1", lat_ts),
                            p("central_meridian", 0.0),
                            p("false_easting", 0.0),
                            p("false_northing", 0.0),
                        ],
                    )
                }
            }
            Projection::Polar { lat_ts, lon0 } => (
                "Polar_Stereographic",
                vec![p("latitude_of_origin", lat_ts), p("central_meridian", lon0), p("false_easting", 0.0), p("false_northing", 0.0)],
            ),
            Projection::Transverse { lat0, lon0, k, x0, y0 } => (
                "Transverse_Mercator",
                vec![
                    p("latitude_of_origin", lat0),
                    p("central_meridian", lon0),
                    p("scale_factor", k),
                    p("false_easting", x0),
                    p("false_northing", y0),
                ],
            ),
            Projection::Albers { lat1, lat2, lat0, lon0 } => (
                "Albers_Conic_Equal_Area",
                vec![
                    p("latitude_of_center", lat0),
                    p("longitude_of_center", lon0),
                    p("standard_parallel_1", lat1),
                    p("standard_parallel_2", lat2),
                    p("false_easting", 0.0),
                    p("false_northing", 0.0),
                ],
            ),
            Projection::Laea { lat0, lon0 } => (
                "Lambert_Azimuthal_Equal_Area",
                vec![p("latitude_of_center", lat0), p("longitude_of_center", lon0), p("false_easting", 0.0), p("false_northing", 0.0)],
            ),
            Projection::Lambert { lat1, lat2, lat0, lon0 } => (
                "Lambert_Conformal_Conic_2SP",
                vec![
                    p("latitude_of_origin", lat0),
                    p("central_meridian", lon0),
                    p("standard_parallel_1", lat1),
                    p("standard_parallel_2", lat2),
                    p("false_easting", 0.0),
                    p("false_northing", 0.0),
                ],
            ),
        };
        // GDAL's axes of polar stereographic projections: towards the equator.
        let axes = match *self {
            Projection::Polar { lat_ts, .. } if lat_ts >= 0.0 => "AXIS[\"Easting\",SOUTH],AXIS[\"Northing\",SOUTH]",
            Projection::Polar { .. } => "AXIS[\"Easting\",NORTH],AXIS[\"Northing\",NORTH]",
            Projection::Lambert { .. }
            | Projection::Mercator { .. }
            | Projection::Transverse { .. }
            | Projection::Albers { .. }
            | Projection::Laea { .. } => "AXIS[\"Easting\",EAST],AXIS[\"Northing\",NORTH]",
        };
        format!("PROJCS[\"unnamed\",{}],PROJECTION[\"{method}\"],{},UNIT[\"Metre\",1],{axes}]", e.geogcs(), params.join(","))
    }

    /// Projects a longitude and latitude (degrees) on an ellipsoid, as PROJ 9.7 does for GDAL's
    /// transformation (`merc.cpp`, `lcc.cpp`, `stere.cpp`, `tsfn.cpp`, `msfn.cpp`, `adjlon.cpp`).
    fn forward(&self, e: &Ellipsoid, lon: f64, lat: f64) -> (f64, f64) {
        use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, PI};
        if matches!(self, Projection::Albers { .. } | Projection::Laea { .. }) {
            // As the GRIB driver of #232: the equal-area projections through terrio's CRS engine, from
            // the projection's geographic CRS (longitude, latitude); NaN when it cannot.
            let src = crate::crs::SpatialRef::from_wkt(&format!("{}]", e.geogcs()));
            let dst = crate::crs::SpatialRef::from_wkt(&self.wkt(e));
            return crate::crs::Transformer::new(&src, &dst).and_then(|t| t.transform(lon, lat)).unwrap_or((f64::NAN, f64::NAN));
        }
        const EPS10: f64 = 1e-10;
        let rad = |d: f64| d * (PI / 180.0);
        let es = if e.sphere { 0.0 } else { 1.0 / e.inv_flattening() * (2.0 - 1.0 / e.inv_flattening()) };
        let ecc = es.sqrt();
        let tsfn = |phi: f64, sinphi: f64| -> f64 {
            let cosphi = phi.cos();
            (ecc * (ecc * sinphi).atanh()).exp() * if sinphi > 0.0 { cosphi / (1.0 + sinphi) } else { (1.0 - sinphi) / cosphi }
        };
        let msfn = |sinphi: f64, cosphi: f64| cosphi / (1.0 - es * sinphi * sinphi).sqrt();
        let adjlon = |mut l: f64| {
            if l.abs() < PI + 1e-12 {
                return l;
            }
            l += PI;
            l -= 2.0 * PI * (l / (2.0 * PI)).floor();
            l - PI
        };
        let (phi, lam0) = (
            rad(lat),
            match *self {
                Projection::Polar { lon0, .. } | Projection::Lambert { lon0, .. } => rad(lon0),
                Projection::Mercator { .. } | Projection::Transverse { .. } | Projection::Albers { .. } | Projection::Laea { .. } => 0.0,
            },
        );
        let mut lam = adjlon(rad(lon) - lam0);
        let (x, y) = match *self {
            Projection::Transverse { .. } | Projection::Albers { .. } | Projection::Laea { .. } => (0.0, 0.0),
            Projection::Mercator { lat_ts } => {
                let phits = rad(lat_ts).abs();
                let k0 = if lat_ts == 0.0 {
                    1.0
                } else if es != 0.0 {
                    msfn(phits.sin(), phits.cos())
                } else {
                    phits.cos()
                };
                let y = if es != 0.0 {
                    let (sphi, cphi) = (phi.sin(), phi.cos());
                    k0 * ((sphi / cphi).asinh() - ecc * (ecc * sphi).atanh())
                } else {
                    k0 * phi.tan().asinh()
                };
                (k0 * lam, y)
            }
            Projection::Lambert { lat1, lat2, lat0, .. } => {
                let (phi1, phi2, phi0) = (rad(lat1), rad(lat2), rad(lat0));
                let mut sinphi = phi1.sin();
                let cosphi = phi1.cos();
                let mut n = sinphi;
                let secant = (phi1 - phi2).abs() >= EPS10;
                let (c, rho0);
                if es != 0.0 {
                    let m1 = msfn(sinphi, cosphi);
                    let ml1 = tsfn(phi1, sinphi);
                    if secant {
                        sinphi = phi2.sin();
                        n = (m1 / msfn(sinphi, phi2.cos())).ln();
                        n /= (ml1 / tsfn(phi2, sinphi)).ln();
                    }
                    c = m1 * ml1.powf(-n) / n;
                    rho0 = if (phi0.abs() - FRAC_PI_2).abs() < EPS10 { 0.0 } else { c * tsfn(phi0, phi0.sin()).powf(n) };
                } else {
                    if secant {
                        n = (cosphi / phi2.cos()).ln() / ((FRAC_PI_4 + 0.5 * phi2).tan() / (FRAC_PI_4 + 0.5 * phi1).tan()).ln();
                    }
                    c = cosphi * (FRAC_PI_4 + 0.5 * phi1).tan().powf(n) / n;
                    rho0 = if (phi0.abs() - FRAC_PI_2).abs() < EPS10 { 0.0 } else { c * (FRAC_PI_4 + 0.5 * phi0).tan().powf(-n) };
                }
                let rho = if (phi.abs() - FRAC_PI_2).abs() < EPS10 {
                    0.0
                } else if es != 0.0 {
                    c * tsfn(phi, phi.sin()).powf(n)
                } else {
                    c * (FRAC_PI_4 + 0.5 * phi).tan().powf(-n)
                };
                lam *= n;
                (rho * lam.sin(), rho0 - rho * lam.cos())
            }
            Projection::Polar { lat_ts, .. } => {
                let south = lat_ts < 0.0;
                let phits = rad(lat_ts).abs();
                let (coslam, sinlam) = (lam.cos(), lam.sin());
                if es != 0.0 {
                    let akm1 = if (phits - FRAC_PI_2).abs() < EPS10 {
                        2.0 / ((1.0 + ecc).powf(1.0 + ecc) * (1.0 - ecc).powf(1.0 - ecc)).sqrt()
                    } else {
                        let t = phits.sin();
                        phits.cos() / tsfn(phits, t) / (1.0 - (t * ecc) * (t * ecc)).sqrt()
                    };
                    let (mut p, mut cl, mut sp) = (phi, coslam, phi.sin());
                    if south {
                        p = -p;
                        cl = -cl;
                        sp = -sp;
                    }
                    let x = if (p - FRAC_PI_2).abs() < 1e-15 { 0.0 } else { akm1 * tsfn(p, sp) };
                    (x * sinlam, -x * cl)
                } else {
                    let akm1 = if (phits - FRAC_PI_2).abs() >= EPS10 { phits.cos() / (FRAC_PI_4 - 0.5 * phits).tan() } else { 2.0 };
                    let (mut p, mut cl) = (phi, coslam);
                    if !south {
                        cl = -cl;
                        p = -p;
                    }
                    let y = akm1 * (FRAC_PI_4 + 0.5 * p).tan();
                    (sinlam * y, y * cl)
                }
            }
        };
        (e.a * x, e.a * y)
    }
}

/// A decoded field: degrib's grid (scanning mode 0100, rows south to north) of Float64 values.
pub(crate) struct Decoded {
    pub nx: usize,
    pub ny: usize,
    pub values: Vec<f64>,
}

/// Decodes a field of a file of `file_len` bytes as degrib's `ReadGrib2Record` does.
fn decode_field(f: &Field, file_len: usize) -> Result<Decoded> {
    let grid = Grid::parse(f.sect3)?;
    let ngrdpts = uoct(f.sect3, 7, 4) as u32 as usize;
    // degrib's guard against grids a short file cannot hold: at most 1000 points a byte above
    // 100 Mi points.
    if ngrdpts > 100 * 1024 * 1024 && ngrdpts / 1000 > file_len {
        return Err(Error::InvalidData("file too short for its grid".into()));
    }
    let s5 = f.sect5;
    if s5.len() < 11 {
        return Err(Error::InvalidData("truncated data representation section".into()));
    }
    let ndpts = uoct(s5, 6, 4) as u32 as usize;
    let drtn = uoct(s5, 10, 2) as u16;
    let template_len = match drtn {
        0 | 41 | 40010 => 21,
        2 => 47,
        3 => 49,
        4 => 12,
        40 | 40000 => 23,
        42 => 25,
        _ => return Err(Error::Unsupported(format!("GRIB2 data representation template 5.{drtn} is not supported"))),
    };
    if s5.len() < template_len {
        return Err(Error::InvalidData(format!("truncated data representation template 5.{drtn}")));
    }
    // The bitmap (section 6), g2clib's `g2_unpack6` and `g2_getfld`.
    let bitmap_section = match f.sect6.get(5).copied() {
        Some(0) => Some(f.sect6),
        Some(254) => Some(f.previous_bitmap.ok_or_else(|| Error::InvalidData("bitmap 254 without a previous bitmap".into()))?),
        _ => None,
    };
    let bitmap: Option<Vec<bool>> = match bitmap_section {
        Some(s) => {
            let mut r = BitReader::new(&s[6..]);
            let mut bm = try_vec(ngrdpts)?;
            bm.extend((0..ngrdpts).map(|_| r.read(1) == Some(1)));
            Some(bm)
        }
        None => None,
    };
    match &bitmap {
        Some(_) if ngrdpts < ndpts => return Err(Error::InvalidData("more data points than grid points".into())),
        None if ngrdpts != ndpts => return Err(Error::InvalidData("data points do not match grid points".into())),
        _ => {}
    }
    let fld = unpack(drtn, s5, f.sect7, ndpts)?;
    // Expand the data to the grid: zeros where the bitmap masks points.
    let fld = match &bitmap {
        Some(bm) => {
            let mut grid_values = try_filled(ngrdpts, 0f32)?;
            let mut n = 0;
            for (j, &set) in bm.iter().enumerate() {
                if set {
                    if n >= ndpts {
                        return Err(Error::InvalidData("bitmap sets more points than the data holds".into()));
                    }
                    grid_values[j] = fld[n];
                    n += 1;
                }
            }
            grid_values
        }
        None => fld,
    };
    let (nx, ny) = (grid.nx as usize, grid.ny as usize);
    // degrib keeps integer fields (code table 5.1) as integers.
    let field_type = if matches!(drtn, 4) { 0 } else { uoct(s5, 21, 1) };
    if field_type > 1 {
        return Err(Error::InvalidData(format!("invalid type of original field values {field_type}")));
    }
    let as_value = |v: f32| -> f64 { if field_type == 1 { float_to_i32_clamp(v) as f64 } else { v as f64 } };
    // Reorder to scanning mode 0100 (`TransferFloat`); the bitmap follows.
    let (mut ain, ib): (Vec<f64>, Option<Vec<bool>>) = if grid.scan_octet.is_none() || grid.scan & 0xf0 == SCAN_J_POSITIVE {
        let mut ain = try_vec(fld.len())?;
        ain.extend(fld.iter().map(|&v| as_value(v)));
        (ain, bitmap)
    } else {
        if nx == 0 || ny != ngrdpts / nx {
            return Err(Error::InvalidData("Nx * Ny != number of points".into()));
        }
        let mut ain = try_filled(ngrdpts, 0f64)?;
        let mut ib = match &bitmap {
            Some(_) => Some(try_filled(ngrdpts, false)?),
            None => None,
        };
        for (i, &v) in fld.iter().enumerate() {
            let (x, y) = scan_index_to_xy(i, grid.scan, nx, ny);
            let k = x + y * nx;
            ain[k] = as_value(v);
            if let (Some(ib), Some(bm)) = (ib.as_mut(), bitmap.as_ref()) {
                ib[k] = bm[i];
            }
        }
        (ain, ib)
    };
    // Missing values (`ParseSect5`, `unpk_g2ncep`) and unit conversion (`ParseGrid`).
    let (f_miss, miss_pri, miss_sec) = missing_values(s5, drtn, field_type);
    if f_miss > 2 {
        return Err(Error::InvalidData(format!("invalid missing value management {f_miss}")));
    }
    let conversion = tables::unit_conversion(f);
    let convert = |v: f64| -> f64 { conversion.map_or(v, |c| c.apply(v)) };
    let (mut min, mut max) = (f64::INFINITY, f64::NEG_INFINITY);
    for v in ain.iter_mut() {
        let missing = (f_miss >= 1 && *v == miss_pri) || (f_miss == 2 && *v == miss_sec);
        if !missing {
            *v = convert(*v);
            min = min.min(*v);
            max = max.max(*v);
        }
    }
    // Valid values the conversion made equal to a missing value move the missing values above
    // the data (`f_readjust`); GDAL reports the file's missing value as nodata all the same (it
    // reads it from the metadata, without the data).
    if f_miss >= 1 && min <= max {
        let (mut xp, mut xs, mut readjust) = (miss_pri, miss_sec, false);
        if miss_pri >= min && miss_pri <= max {
            xp = max + 1.0;
            readjust = true;
        }
        if f_miss == 2 && miss_sec >= min && miss_sec <= max {
            xs = max + 2.0;
            readjust = true;
        }
        if readjust {
            let raw: Vec<f64> = reordered_raw(&fld, &grid, nx, ny, as_value)?;
            for (k, v) in raw.iter().enumerate() {
                if *v == miss_pri {
                    ain[k] = xp;
                } else if f_miss == 2 && *v == miss_sec {
                    ain[k] = xs;
                }
            }
        }
    }
    if let Some(ib) = &ib
        && f_miss != 1
        && f_miss != 2
    {
        for (v, &set) in ain.iter_mut().zip(ib) {
            if !set {
                *v = BITMAP_MISSING;
            }
        }
    }
    // `ParseGrid` places the points by degrib's scanning mode, 0100 (in place) unless
    // `ParseSect3` stopped before reading it.
    if grid.gdal_scan & 0xf0 != SCAN_J_POSITIVE {
        let mut placed = try_filled(ain.len(), 0f64)?;
        for (i, &v) in ain.iter().enumerate() {
            let (x, y) = scan_index_to_xy(i, grid.gdal_scan, nx, ny);
            placed[x + y * nx] = v;
        }
        ain = placed;
    }
    Ok(Decoded { nx, ny, values: ain })
}

/// The missing value management of complex packing (0 for none, 1 primary, 2 primary and
/// secondary) and the missing values, as `unpk_g2ncep` reads them.
fn missing_values(s5: &[u8], drtn: u16, field_type: i32) -> (u8, f64, f64) {
    if !matches!(drtn, 2 | 3) {
        return (0, 0.0, 0.0);
    }
    let (p, s) = if field_type == 0 {
        (f32::from_bits(uoct(s5, 24, 4) as u32) as f64, f32::from_bits(uoct(s5, 28, 4) as u32) as f64)
    } else {
        (uoct(s5, 24, 4) as f32 as f64, uoct(s5, 28, 4) as f32 as f64)
    };
    (uoct(s5, 23, 1).clamp(0, 255) as u8, p, s)
}

/// A field's nodata value, from its sections: complex packing's primary missing value, else 9999
/// when a bitmap masks points.
fn nodata_of(f: &Field) -> Option<f64> {
    let drtn = uoct(f.sect5, 10, 2) as u16;
    let field_type = if drtn == 4 { 0 } else { uoct(f.sect5, 21, 1) };
    let (f_miss, miss_pri, _) = missing_values(f.sect5, drtn, field_type);
    if f_miss == 1 || f_miss == 2 {
        return Some(miss_pri);
    }
    matches!(f.sect6.get(5), Some(0 | 254)).then_some(BITMAP_MISSING)
}

/// The raw values (before unit conversion) in scanning mode 0100, for `f_readjust`.
fn reordered_raw(fld: &[f32], grid: &Grid, nx: usize, ny: usize, as_value: impl Fn(f32) -> f64) -> Result<Vec<f64>> {
    if grid.scan_octet.is_none() || grid.scan & 0xf0 == SCAN_J_POSITIVE {
        let mut out = try_vec(fld.len())?;
        out.extend(fld.iter().map(|&v| as_value(v)));
        return Ok(out);
    }
    let mut out = try_filled(fld.len(), 0f64)?;
    for (i, &v) in fld.iter().enumerate() {
        let (x, y) = scan_index_to_xy(i, grid.scan, nx, ny);
        out[x + y * nx] = as_value(v);
    }
    Ok(out)
}

/// degrib's `ScanIndex2XY`: the column and row (from the south-west, 0-based) of the `i`th point
/// of a grid in scanning mode `scan`.
fn scan_index_to_xy(i: usize, scan: u8, nx: usize, ny: usize) -> (usize, usize) {
    let (mut x, mut y);
    if scan & SCAN_J_CONSECUTIVE != 0 {
        x = i / ny;
        y = if scan & SCAN_BOUSTROPHEDON != 0 && x % 2 == 1 { ny - 1 - i % ny } else { i % ny };
    } else {
        y = i / nx;
        x = if scan & SCAN_BOUSTROPHEDON != 0 && y % 2 == 1 { nx - 1 - i % nx } else { i % nx };
    }
    if scan & SCAN_I_NEGATIVE != 0 {
        x = nx - 1 - x;
    }
    if scan & SCAN_J_POSITIVE == 0 {
        y = ny - 1 - y;
    }
    (x, y)
}

/// grib2api.c's `FloatToSInt4Clamp`.
fn float_to_i32_clamp(v: f32) -> i32 {
    if v as f64 >= i32::MAX as f64 {
        i32::MAX
    } else if v as f64 <= i32::MIN as f64 {
        i32::MIN
    } else if v.is_nan() {
        0
    } else {
        v as i32
    }
}

/// Big-endian bit reader, as g2clib's `gbit`/`gbits`.
struct BitReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        BitReader { buf, pos: 0 }
    }

    fn at(buf: &'a [u8], pos: usize) -> Self {
        BitReader { buf, pos }
    }

    /// The next `n` bits, or `None` past the end of the buffer. g2clib keeps values in 32-bit
    /// integers: of a longer field (only in damaged files), the last 32 bits.
    fn read(&mut self, n: u32) -> Option<u32> {
        if n == 0 {
            return Some(0);
        }
        let end = self.pos.checked_add(n as usize)?;
        if end > self.buf.len().saturating_mul(8) {
            return None;
        }
        let mut v = 0u64;
        let mut left = n;
        while left > 0 {
            let k = left.min(32);
            v = ((v << k) | self.read_at_most_32(k) as u64) & 0xffff_ffff;
            left -= k;
        }
        Some(v as u32)
    }

    /// The next `n` bits, 1 to 32 of them, known to be in the buffer.
    fn read_at_most_32(&mut self, n: u32) -> u32 {
        let (start, end) = (self.pos, self.pos + n as usize);
        let (first, last) = (start / 8, (end - 1) / 8);
        let v = self.buf[first..=last].iter().fold(0u64, |v, &b| (v << 8) | b as u64);
        let shift = (last - first + 1) * 8 - (start % 8) - n as usize;
        self.pos = end;
        ((v >> shift) & ((1u64 << n) - 1)) as u32
    }

    /// `n` values of `bits` bits, as g2clib's `gbits` (which leaves the values it cannot read 0).
    fn read_many(&mut self, bits: u32, n: usize) -> Result<Vec<i32>> {
        let mut out = try_filled(n, 0i32)?;
        for v in out.iter_mut() {
            match self.read(bits) {
                Some(x) => *v = x as i32,
                None => break,
            }
        }
        Ok(out)
    }
}

/// g2clib's `int_power`: `x` to the integer power `y`, by squaring.
fn int_power(mut x: f64, mut y: i32) -> f64 {
    if y < 0 {
        y = -y;
        x = 1.0 / x;
    }
    let mut value = 1.0;
    while y != 0 {
        if y & 1 != 0 {
            value *= x;
        }
        x *= x;
        y >>= 1;
    }
    value
}

/// g2clib's `DoubleToFloatClamp`.
fn clamp_f32(v: f64) -> f32 {
    if v >= f32::MAX as f64 {
        f32::MAX
    } else if v <= -f32::MAX as f64 {
        -f32::MAX
    } else {
        v as f32
    }
}

/// g2clib's `rdieee`: an IEEE single, with infinities and NaNs read as ±1e37.
fn rdieee(bits: u32) -> f32 {
    if (bits >> 23) & 0xff == 255 {
        let sign: f64 = if bits >> 31 == 1 { -1.0 } else { 1.0 };
        (sign * 1e37) as f32
    } else {
        f32::from_bits(bits)
    }
}

/// The scaling of the packings (`R`, `E`, `D`, bits per value), octets 12-20 of section 5.
struct Scaling {
    reference: f32,
    bscale: f32,
    dscale: f32,
    nbits: u32,
}

impl Scaling {
    fn of(s5: &[u8]) -> Scaling {
        Scaling {
            reference: rdieee(uoct(s5, 12, 4) as u32),
            bscale: clamp_f32(int_power(2.0, soct(s5, 16, 2))),
            dscale: clamp_f32(int_power(10.0, -soct(s5, 18, 2))),
            nbits: uoct(s5, 20, 1) as u32,
        }
    }

    /// `(((float)ifld * bscale) + ref) * dscale`, in single precision.
    fn value(&self, i: i32) -> f32 {
        ((i as f32 * self.bscale) + self.reference) * self.dscale
    }
}

/// Unpacks section 7 to `ndpts` values, as g2clib's `g2_unpack7`.
fn unpack(drtn: u16, s5: &[u8], s7: &[u8], ndpts: usize) -> Result<Vec<f32>> {
    let data = &s7[5..];
    match drtn {
        0 => {
            let sc = Scaling::of(s5);
            if sc.nbits == 0 {
                return try_filled(ndpts, sc.reference * sc.dscale);
            }
            let ints = BitReader::new(data).read_many(sc.nbits.min(32), ndpts)?;
            scaled(&ints, |i| sc.value(i))
        }
        2 | 3 => complex(drtn, s5, data, s7.len(), ndpts),
        4 => match uoct(s5, 12, 1) {
            1 => {
                if data.len() / 4 < ndpts {
                    return Err(Error::InvalidData("IEEE data section too short".into()));
                }
                let mut out = try_vec(ndpts)?;
                out.extend(data.as_chunks::<4>().0.iter().take(ndpts).map(|c| f32::from_be_bytes([c[0], c[1], c[2], c[3]])));
                Ok(out)
            }
            2 => {
                if data.len() / 8 < ndpts {
                    return Err(Error::InvalidData("IEEE data section too short".into()));
                }
                let mut out = try_vec(ndpts)?;
                out.extend(data.as_chunks::<8>().0.iter().take(ndpts).map(|c| clamp_f32(f64::from_be_bytes(*c))));
                Ok(out)
            }
            // g2clib reports the precision and leaves the field zeros.
            _ => try_filled(ndpts, 0.0),
        },
        40 | 40000 => jpeg2000(s5, data, ndpts),
        41 | 40010 => png(s5, data, ndpts),
        42 => ccsds(s5, data, ndpts),
        _ => Err(Error::Unsupported(format!("GRIB2 data representation template 5.{drtn} is not supported"))),
    }
}

/// The values of packed integers, allocated as [`try_vec`].
fn scaled(ints: &[i32], value: impl Fn(i32) -> f32) -> Result<Vec<f32>> {
    let mut out = try_vec(ints.len())?;
    out.extend(ints.iter().map(|&i| value(i)));
    Ok(out)
}

/// Complex packing, with spatial differencing for template 5.3: g2clib's `comunpack`.
fn complex(drtn: u16, s5: &[u8], data: &[u8], lensec: usize, ndpts: usize) -> Result<Vec<f32>> {
    let bad = || Error::InvalidData("invalid complex packing".into());
    let sc = Scaling::of(s5);
    let t = |i: usize| -> i32 {
        match i {
            0 => uoct(s5, 12, 4),
            1 => soct(s5, 16, 2),
            2 => soct(s5, 18, 2),
            3 => uoct(s5, 20, 1),
            4 => uoct(s5, 21, 1),
            5 => uoct(s5, 22, 1),
            6 => uoct(s5, 23, 1),
            7 => uoct(s5, 24, 4),
            8 => uoct(s5, 28, 4),
            9 => uoct(s5, 32, 4),
            10 => uoct(s5, 36, 1),
            11 => uoct(s5, 37, 1),
            12 => uoct(s5, 38, 4),
            13 => uoct(s5, 42, 1),
            14 => uoct(s5, 43, 4),
            15 => uoct(s5, 47, 1),
            16 => uoct(s5, 48, 1),
            17 => uoct(s5, 49, 1),
            _ => 0,
        }
    };
    let nbitsgref = t(3) as u32;
    let itype = t(4);
    let ngroups = t(9);
    let nbitsgwidth = t(11) as u32;
    let nbitsglen = t(15) as u32;
    let nbitsd = if drtn == 3 { t(17) as u32 * 8 } else { 0 };
    let ndpts_i = ndpts as i64;
    if ngroups == 0 {
        return try_filled(ndpts, sc.reference);
    }
    if ngroups < 0 || (ngroups as i64 - 10) > ndpts_i / 2 {
        return Err(bad());
    }
    if t(12) < 0 || t(14) < 0 || t(14) as i64 > ndpts_i {
        return Err(bad());
    }
    let ng = ngroups as usize;
    if nbitsglen == 0
        && ((ng > 1 && t(12) as i64 != (ndpts_i - t(14) as i64) / (ng as i64 - 1))
            || t(12) as i64 * (ng as i64 - 1) + t(14) as i64 != ndpts_i)
    {
        return Err(bad());
    }
    let mgmt = t(6);
    let (rmiss1, rmiss2) = if itype == 0 { (rdieee(t(7) as u32), rdieee(t(8) as u32)) } else { (t(7) as f32, t(8) as f32) };
    let mut r = BitReader::new(data);
    let (mut ival1, mut ival2, mut minsd) = (0i32, 0i32, 0i32);
    if drtn == 3 && nbitsd != 0 {
        ival1 = r.read(nbitsd).ok_or_else(bad)? as i32;
        if t(16) == 2 {
            ival2 = r.read(nbitsd).ok_or_else(bad)? as i32;
        }
        let isign = r.read(1).ok_or_else(bad)?;
        minsd = r.read(nbitsd - 1).ok_or_else(bad)? as i32;
        if isign == 1 && minsd != i32::MIN {
            minsd = -minsd;
        }
    }
    let align = |r: &mut BitReader, bits: u32| {
        let used = bits as usize * ng;
        if !used.is_multiple_of(8) {
            r.pos += 8 - used % 8;
        }
    };
    let gref: Vec<i32> = if nbitsgref != 0 {
        let start = r.pos;
        let v = r.read_many(nbitsgref, ng)?;
        if start + nbitsgref as usize * ng > data.len() * 8 {
            return Err(bad());
        }
        align(&mut r, nbitsgref);
        v
    } else {
        try_filled(ng, 0)?
    };
    let mut gwidth: Vec<i32> = if nbitsgwidth != 0 {
        let start = r.pos;
        let v = r.read_many(nbitsgwidth, ng)?;
        if start + nbitsgwidth as usize * ng > data.len() * 8 {
            return Err(bad());
        }
        align(&mut r, nbitsgwidth);
        v
    } else {
        try_filled(ng, 0)?
    };
    for w in gwidth.iter_mut() {
        if *w > i32::MAX - t(10) {
            return Err(bad());
        }
        *w += t(10);
    }
    let mut glen: Vec<i32> = if nbitsglen != 0 {
        let start = r.pos;
        let v = r.read_many(nbitsglen, ng)?;
        if start + nbitsglen as usize * ng > data.len() * 8 {
            return Err(bad());
        }
        align(&mut r, nbitsglen);
        v
    } else {
        try_filled(ng, 0)?
    };
    for l in glen.iter_mut().take(ng - 1) {
        let v = *l as i64 * t(13) as i64 + t(12) as i64;
        if *l < 0 || v > i32::MAX as i64 {
            return Err(bad());
        }
        *l = v as i32;
    }
    glen[ng - 1] = t(14);
    let (mut tot_bit, mut tot_len) = (0i64, 0i64);
    for j in 0..ng {
        if gwidth[j] < 0 || glen[j] < 0 {
            return Err(bad());
        }
        tot_bit += gwidth[j] as i64 * glen[j] as i64;
        tot_len += glen[j] as i64;
        if tot_bit > i32::MAX as i64 || tot_len > i32::MAX as i64 {
            return Err(bad());
        }
    }
    if tot_len != ndpts_i || tot_bit as f64 / 8.0 > lensec as f64 {
        return Err(bad());
    }
    let mut ifld = try_filled(ndpts, 0i32)?;
    let mut ifldmiss: Vec<u8> = Vec::new();
    let mut non = 0usize;
    if mgmt == 0 {
        let mut n = 0;
        for j in 0..ng {
            let len = glen[j] as usize;
            if gwidth[j] != 0 {
                let mut gr = BitReader::at(data, r.pos);
                let vals = gr.read_many(gwidth[j] as u32, len)?;
                for (k, v) in vals.into_iter().enumerate() {
                    ifld[n + k] = v.wrapping_add(gref[j]);
                }
            } else {
                ifld[n..n + len].fill(gref[j]);
            }
            n += len;
            r.pos += gwidth[j] as usize * len;
        }
    } else if mgmt == 1 || mgmt == 2 {
        ifldmiss = try_filled(ndpts, 0u8)?;
        let mut n = 0;
        for j in 0..ng {
            let len = glen[j] as usize;
            if gwidth[j] != 0 {
                let msng1 = int_power(2.0, gwidth[j]) as i64 - 1;
                let msng2 = msng1 - 1;
                let mut gr = BitReader::at(data, r.pos);
                let vals = gr.read_many(gwidth[j] as u32, len)?;
                r.pos += gwidth[j] as usize * len;
                for v in vals {
                    if v as i64 == msng1 {
                        ifldmiss[n] = 1;
                    } else if mgmt == 2 && v as i64 == msng2 {
                        ifldmiss[n] = 2;
                    } else {
                        ifld[non] = v.wrapping_add(gref[j]);
                        non += 1;
                    }
                    n += 1;
                }
            } else {
                let msng1 = int_power(2.0, nbitsgref as i32) as i64 - 1;
                let msng2 = msng1 - 1;
                if gref[j] as i64 == msng1 {
                    ifldmiss[n..n + len].fill(1);
                } else if mgmt == 2 && gref[j] as i64 == msng2 {
                    ifldmiss[n..n + len].fill(2);
                } else {
                    ifld[non..non + len].fill(gref[j]);
                    non += len;
                }
                n += len;
            }
        }
    }
    if drtn == 3 {
        let count = if mgmt == 0 { ndpts } else { non };
        match t(16) {
            1 => {
                if count > 0 {
                    ifld[0] = ival1;
                }
                for n in 1..count {
                    let v = ifld[n] as i64 + minsd as i64 + ifld[n - 1] as i64;
                    if v > i32::MAX as i64 || v < i32::MIN as i64 {
                        return Err(bad());
                    }
                    ifld[n] = v as i32;
                }
            }
            2 => {
                if count > 0 {
                    ifld[0] = ival1;
                }
                if count > 1 {
                    ifld[1] = ival2;
                }
                for n in 2..count {
                    let v = ifld[n] as f64 + minsd as f64 + 2.0 * ifld[n - 1] as f64 - ifld[n - 2] as f64;
                    if v > i32::MAX as f64 || v < i32::MIN as f64 {
                        return Err(bad());
                    }
                    ifld[n] = v as i32;
                }
            }
            _ => {}
        }
    }
    let mut fld = try_filled(ndpts, 0f32)?;
    if mgmt == 0 {
        for (o, &i) in fld.iter_mut().zip(&ifld) {
            *o = sc.value(i);
        }
    } else if mgmt == 1 || mgmt == 2 {
        let mut k = 0;
        for (n, o) in fld.iter_mut().enumerate() {
            *o = match ifldmiss[n] {
                0 => {
                    k += 1;
                    sc.value(ifld[k - 1])
                }
                1 => rmiss1,
                _ => rmiss2,
            };
        }
    }
    Ok(fld)
}

/// JPEG 2000 packing (5.40), g2clib's `jpcunpack` with the codestream decoded by terrio's port of
/// OpenJPEG (GDAL decodes it with its JP2OpenJPEG driver).
#[cfg(feature = "jp2")]
fn jpeg2000(s5: &[u8], data: &[u8], ndpts: usize) -> Result<Vec<f32>> {
    let sc = Scaling::of(s5);
    if sc.nbits == 0 {
        return try_filled(ndpts, sc.reference * sc.dscale);
    }
    let bad = |e: String| Error::InvalidData(format!("JPEG 2000 packing: {e}"));
    // `dec_jpeg2000`'s checks, from the headers, before decoding.
    let (w, h, bands) = super::jp2::size_of_bytes(data).map_err(|e| bad(e.to_string()))?;
    if bands != 1 {
        return Err(bad("colour image, grayscale expected".into()));
    }
    if h == 0 || w > ndpts / h || w < ndpts / h / 100 {
        return Err(bad("image size does not match the grid".into()));
    }
    let ds = super::jp2::read_bytes(data).map_err(|e| bad(e.to_string()))?;
    let r = &ds.raster;
    let mut ints = try_filled(ndpts, 0i32)?;
    for (o, v) in ints.iter_mut().zip(r.data.to_f64_vec()) {
        *o = v as i32;
    }
    scaled(&ints, |i| sc.value(i))
}

#[cfg(not(feature = "jp2"))]
fn jpeg2000(_s5: &[u8], _data: &[u8], _ndpts: usize) -> Result<Vec<f32>> {
    Err(Error::disabled("jp2"))
}

/// CCSDS packing (5.42), g2clib's `aecunpack`, with the CCSDS 121.0-B-3 stream decoded by
/// `rust-aec` (GDAL decodes it with libaec): the samples laid out as libaec writes them into
/// GDAL's buffer of `(nbits + 7) / 8` bytes a sample, read back as big-endian integers, then
/// `refD + bdscale * X` in single precision.
fn ccsds(s5: &[u8], data: &[u8], ndpts: usize) -> Result<Vec<f32>> {
    let ref_v = rdieee(uoct(s5, 12, 4) as u32);
    let bscale = int_power(2.0, soct(s5, 16, 2)) as f32;
    let dscale = int_power(10.0, -soct(s5, 18, 2)) as f32;
    let (bdscale, ref_d) = (bscale * dscale, ref_v * dscale);
    let nbits = uoct(s5, 20, 1) as u32;
    if nbits == 0 {
        return try_filled(ndpts, ref_d);
    }
    let nbytes = nbits.div_ceil(8) as usize;
    // aecunpack fails (and g2_unpack7 ignores it, leaving the field zeros) on a buffer past
    // INT_MAX bytes, or when libaec reports an error.
    if ndpts != 0 && nbytes > i32::MAX as usize / ndpts {
        return try_filled(ndpts, 0.0);
    }
    let mut ctemp = try_filled(ndpts * nbytes, 0u8)?;
    let (flags, block, rsi) = (uoct(s5, 22, 1) as u8, uoct(s5, 23, 1) as u32, soct(s5, 24, 2));
    if !aec_decode(data, nbits, flags, block, rsi, &mut ctemp) {
        return try_filled(ndpts, 0.0);
    }
    let ints = BitReader::new(&ctemp).read_many(nbytes as u32 * 8, ndpts)?;
    scaled(&ints, |i| ref_d + bdscale * i as f32)
}

/// Decodes a CCSDS stream into `out` as libaec's `aec_buffer_decode` fills GDAL's buffer: whole
/// samples of libaec's width (3 bytes for 17 to 24 bits only with `AEC_DATA_3BYTE`) until the
/// buffer is full. False where libaec fails, or where its behaviour is undefined (parameters
/// rust-aec refuses).
fn aec_decode(data: &[u8], nbits: u32, flags: u8, block: u32, rsi: i32, out: &mut [u8]) -> bool {
    if !(1..=32).contains(&nbits) || rsi <= 0 {
        return false;
    }
    let width = match nbits {
        1..=8 => 1,
        9..=16 => 2,
        17..=24 if flags & 2 != 0 => 3,
        _ => 4,
    };
    let samples = out.len() / width;
    let params = rust_aec::AecParams::new(nbits as u8, block, rsi as u32, rust_aec::flags_from_grib2_ccsds_flags(flags));
    rust_aec::decode_into(data, params, samples, &mut out[..samples * width]).is_ok()
}

/// PNG packing (5.41), g2clib's `pngunpack` and `dec_png`: the image's samples as libpng gives
/// them (`PNG_TRANSFORM_IDENTITY`), `bit_depth / 8` bytes a pixel, then `nbits`-bit integers.
fn png(s5: &[u8], data: &[u8], ndpts: usize) -> Result<Vec<f32>> {
    let rd = |bits: u32| rdieee(bits);
    let ref_v = rd(uoct(s5, 12, 4) as u32);
    let bscale = int_power(2.0, soct(s5, 16, 2)) as f32;
    let dscale = int_power(10.0, -soct(s5, 18, 2)) as f32;
    let bdscale = bscale * dscale;
    let ref_d = ref_v * dscale;
    let nbits = uoct(s5, 20, 1) as u32;
    if nbits == 0 {
        return try_filled(ndpts, ref_d);
    }
    let nbytes = (nbits / 8) as usize;
    let mut ctemp = try_filled(ndpts.checked_mul(nbytes).ok_or_else(|| Error::InvalidData("grid too large".into()))?, 0u8)?;
    // A failed decode leaves the samples zeros, as g2clib's (whose error g2_unpack7 ignores).
    if let Some(samples) = png_samples(data, ndpts, nbits)? {
        let n = samples.len().min(ctemp.len());
        ctemp[..n].copy_from_slice(&samples[..n]);
    }
    let ints = BitReader::new(&ctemp).read_many(nbits.min(32), ndpts)?;
    scaled(&ints, |i| ref_d + bdscale * i as f32)
}

/// The PNG's rows, `bit_depth / 8` bytes a pixel, when its size and depth are those expected.
fn png_samples(data: &[u8], ndpts: usize, nbits: u32) -> Result<Option<Vec<u8>>> {
    Ok(png_samples_sized(data, ndpts, nbits)
        .map(|(size, w, h, bytes)| -> Result<Option<Vec<u8>>> {
            let mut dec = png::Decoder::new(std::io::Cursor::new(data));
            dec.set_transformations(png::Transformations::IDENTITY);
            let Ok(mut reader) = dec.read_info() else { return Ok(None) };
            let mut buf = try_filled(size, 0u8)?;
            let Ok(frame) = reader.next_frame(&mut buf) else { return Ok(None) };
            let line = frame.line_size;
            let mut out = try_vec(w * h * bytes)?;
            for row in buf.chunks(line).take(h) {
                out.extend_from_slice(&row[..(w * bytes).min(row.len())]);
            }
            Ok(Some(out))
        })
        .transpose()?
        .flatten())
}

/// The PNG's output buffer size, width, height and bytes a sample, when its size and depth are
/// those expected.
fn png_samples_sized(data: &[u8], ndpts: usize, nbits: u32) -> Option<(usize, usize, usize, usize)> {
    let mut dec = png::Decoder::new(std::io::Cursor::new(data));
    dec.set_transformations(png::Transformations::IDENTITY);
    let reader = dec.read_info().ok()?;
    let info = reader.info();
    let (w, h) = (info.width as usize, info.height as usize);
    if w.checked_mul(h) != Some(ndpts) {
        return None;
    }
    let bit_depth = match info.color_type {
        png::ColorType::Rgb => 24,
        png::ColorType::Rgba => 32,
        _ => info.bit_depth as u32,
    };
    if bit_depth != nbits {
        return None;
    }
    Some((reader.output_buffer_size()?, w, h, (bit_depth / 8) as usize))
}

#[cfg(test)]
mod tests {
    /// WKT numbers as GDAL writes them (15 significant digits), as the Clarke 1866 grids of GDAL's
    /// test suite (`two_bands_with_bitmap.grib2`) show.
    #[test]
    fn wkt_numbers_as_gdal() {
        use super::num;
        assert_eq!(num(294.97869821391095), "294.978698213911");
        assert_eq!(num(0.0174532925199433), "0.0174532925199433");
        assert_eq!(num(6378206.4), "6378206.4");
        assert_eq!(num(298.257223563), "298.257223563");
        assert_eq!(num(-117.0), "-117");
        assert_eq!(num(33.5), "33.5");
        assert_eq!(num(0.9996), "0.9996");
        assert_eq!(num(1e-7), "1e-07");
    }

    /// The bytes of a GRIB file, as WebAssembly builds read them (without a file system), give the
    /// dataset the file does: native builds read bytes through a file, so nothing else runs this.
    #[test]
    fn reads_from_memory_as_webassembly_does() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/grib");
        for name in ["complex_sd2_nodata.grib2", "bitmap254.grib2", "lcc.grib2"] {
            let path = dir.join(name);
            let file = crate::open_raster(path.to_str().unwrap()).unwrap();
            let bytes = std::fs::read(&path).unwrap();
            let crate::Dataset::Raster(mem) = crate::memory::codec::open(bytes, "forecast", &Default::default()).unwrap() else {
                panic!("{name}: not a raster");
            };
            assert_eq!(mem.raster.data.to_f64_vec(), file.raster.data.to_f64_vec(), "{name}");
            assert_eq!((mem.geo_transform(), mem.nodata(), mem.crs()), (file.geo_transform(), file.nodata(), file.crs()), "{name}");
            assert_eq!(mem.band_metadata, file.band_metadata, "{name}");
        }
    }

    /// Damaged files fail with an error, never a panic: a tile server opens untrusted files. Every
    /// byte of each test file is overwritten in turn (and the file cut there), and read. Files over
    /// 8 KiB are left to the fuzz campaigns: each costs five reads per byte, and the MRMS
    /// fixture of #232 (15,501 bytes, 3000 x 1500 points) alone would take hours in a debug build.
    #[test]
    fn damaged_files_do_not_panic() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/grib");
        let mut reads = 0;
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "grib2") {
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            if bytes.len() > 8 << 10 {
                continue;
            }
            for i in 0..bytes.len() {
                for v in [0x00, 0xff, bytes[i].wrapping_add(1), bytes[i] ^ 0x80] {
                    let mut b = bytes.clone();
                    b[i] = v;
                    let _ = super::read_bytes(&b);
                    reads += 1;
                }
                let _ = super::read_bytes(&bytes[..i]);
            }
        }
        assert!(reads > 10_000, "{reads}");
    }
}

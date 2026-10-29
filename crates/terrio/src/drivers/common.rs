//! Helpers shared by terrio's own raster drivers: raw sample decoding and encoding, sidecar
//! files, and building rasters.

use crate::crs::SpatialRef;
use crate::{Error, RasterDataset, Result};
use std::path::{Path, PathBuf};
use wbraster::raster::RasterData;
use wbraster::{CrsInfo, DataType, Raster, RasterConfig};

/// How the bands of a raw raster are interleaved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Interleave {
    /// Band sequential: every row of band 1, then band 2.
    Bsq,
    /// Band interleaved by line: row 1 of every band, then row 2.
    Bil,
    /// Band interleaved by pixel.
    Bip,
}

impl Interleave {
    /// The byte offset of sample `(band, row, col)` in a file with this layout, in samples.
    fn index(self, band: usize, row: usize, col: usize, cols: usize, rows: usize, bands: usize) -> usize {
        match self {
            Self::Bsq => (band * rows + row) * cols + col,
            Self::Bil => (row * bands + band) * cols + col,
            Self::Bip => (row * cols + col) * bands + band,
        }
    }
}

/// A raw pixel layout.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Layout {
    pub cols: usize,
    pub rows: usize,
    pub bands: usize,
    pub data_type: DataType,
    pub little_endian: bool,
    pub interleave: Interleave,
    /// Bytes to skip at the start of the file.
    pub offset: usize,
    /// Whether rows are stored bottom-up (SAGA).
    pub bottom_up: bool,
}

macro_rules! typed {
    ($dt:expr, $mac:ident) => {
        match $dt {
            DataType::U8 => $mac!(u8, U8),
            DataType::I8 => $mac!(i8, I8),
            DataType::U16 => $mac!(u16, U16),
            DataType::I16 => $mac!(i16, I16),
            DataType::U32 => $mac!(u32, U32),
            DataType::I32 => $mac!(i32, I32),
            DataType::U64 => $mac!(u64, U64),
            DataType::I64 => $mac!(i64, I64),
            DataType::F32 => $mac!(f32, F32),
            DataType::F64 => $mac!(f64, F64),
        }
    };
}

/// Decodes raw samples into band-major raster data.
pub(crate) fn decode(bytes: &[u8], l: &Layout) -> Result<RasterData> {
    let size = l.data_type.size_bytes();
    let n = l.cols * l.rows * l.bands;
    let body = bytes.get(l.offset..).unwrap_or_default();
    if body.len() < n * size {
        return Err(Error::InvalidData(format!("expected {} bytes of pixels, found {}", n * size, body.len())));
    }
    macro_rules! go {
        ($t:ty, $v:ident) => {{
            let mut out: Vec<$t> = vec![Default::default(); n];
            for b in 0..l.bands {
                for r in 0..l.rows {
                    let file_row = if l.bottom_up { l.rows - 1 - r } else { r };
                    for c in 0..l.cols {
                        let i = l.interleave.index(b, file_row, c, l.cols, l.rows, l.bands) * size;
                        let a: [u8; size_of::<$t>()] = body[i..i + size].try_into().unwrap_or_default();
                        out[(b * l.rows + r) * l.cols + c] = if l.little_endian { <$t>::from_le_bytes(a) } else { <$t>::from_be_bytes(a) };
                    }
                }
            }
            RasterData::$v(out)
        }};
    }
    Ok(typed!(l.data_type, go))
}

/// Encodes a raster's samples in a raw layout (the layout's size and type must be the raster's).
pub(crate) fn encode(r: &Raster, l: &Layout) -> Vec<u8> {
    let size = r.data_type.size_bytes();
    let n = r.cols * r.rows * r.bands;
    let mut out = vec![0u8; l.offset + n * size];
    macro_rules! go {
        ($t:ty, $v:ident) => {{
            if let RasterData::$v(v) = &r.data {
                for b in 0..r.bands {
                    for row in 0..r.rows {
                        let file_row = if l.bottom_up { r.rows - 1 - row } else { row };
                        for c in 0..r.cols {
                            let x = v[(b * r.rows + row) * r.cols + c];
                            let i = l.offset + l.interleave.index(b, file_row, c, r.cols, r.rows, r.bands) * size;
                            let bytes = if l.little_endian { x.to_le_bytes() } else { x.to_be_bytes() };
                            out[i..i + size].copy_from_slice(&bytes);
                        }
                    }
                }
            }
        }};
    }
    typed!(r.data_type, go);
    out
}

/// A raw raster opened for its header: the dataset without pixels, and its samples' layout and
/// file, from which a window reads only the rows it covers.
pub(crate) struct RawRaster {
    /// The dataset without pixels.
    pub header: RasterDataset,
    window: RawWindow,
}

/// Where the samples of a raw raster are read from.
#[derive(Clone)]
enum RawSource {
    File(PathBuf),
    #[cfg(remote)]
    Http(crate::http::RangeReader),
}

/// A raw raster read by window, as GDAL's raw drivers read it: each row of each band a window
/// covers is read at its offset in the data file, rows next to each other in one read.
#[derive(Clone)]
struct RawWindow {
    source: RawSource,
    layout: Layout,
    /// The band stored for each band of the dataset, numbered from 0, when they differ (Idrisi
    /// RGB24 stores blue first).
    stored_bands: Option<Vec<usize>>,
    /// The local files the raster is read from: its header and data files.
    inputs: Vec<String>,
}

/// The most bytes read at once: rows next to each other are read together up to this size (a row
/// is never split, so a row longer than this is read alone), so that a whole raster is read
/// without holding its file in memory.
const RAW_READ_BYTES: usize = 16 << 20;

/// What the buffer of a raw raster's samples is called in an allocation's refusal.
const RAW_SAMPLES: &str = "a raw raster's samples";

#[cfg(test)]
thread_local! {
    /// The bytes of raw data files read by this thread, for tests.
    pub(crate) static RAW_BYTES_READ: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

impl RawRaster {
    /// A raw raster whose samples are in `data` with `layout`, described by `header` (a dataset
    /// without pixels), read from the files `inputs` besides `data`. Fails, as reading it whole
    /// would, when the file is too short to hold the samples.
    pub(crate) fn open(header: RasterDataset, data: &Path, layout: Layout, inputs: &[&Path]) -> Result<Self> {
        check_len(&layout, std::fs::metadata(data)?.len())?;
        let mut files: Vec<String> =
            std::iter::once(data).chain(inputs.iter().copied()).map(|p| p.to_string_lossy().into_owned()).collect();
        files.dedup();
        let source = RawSource::File(data.to_path_buf());
        Ok(Self { header, window: RawWindow { source, layout, stored_bands: None, inputs: files } })
    }

    /// A raw raster whose samples are in a remote file, read with range requests.
    #[cfg(remote)]
    pub(crate) fn open_remote(header: RasterDataset, reader: crate::http::RangeReader, layout: Layout) -> Result<Self> {
        check_len(&layout, reader.len())?;
        Ok(Self { header, window: RawWindow { source: RawSource::Http(reader), layout, stored_bands: None, inputs: Vec::new() } })
    }

    /// The band stored for each band of the dataset, numbered from 0.
    pub(crate) fn with_stored_bands(mut self, bands: Vec<usize>) -> Self {
        self.window.stored_bands = Some(bands);
        self
    }

    /// Reads every sample: the dataset as read whole.
    pub(crate) fn read(self) -> Result<RasterDataset> {
        let mut ds = self.header;
        let l = &self.window.layout;
        let all: Vec<usize> = (1..=l.bands).collect();
        ds.raster.data = self.window.read_window(crate::Window { x: 0, y: 0, width: l.cols, height: l.rows }, &all)?;
        Ok(ds)
    }

    /// The raster to be read by window.
    pub(crate) fn into_lazy(self) -> crate::LazyRaster {
        crate::LazyRaster::from_source(self.header, Box::new(self.window))
    }
}

/// Fails, as [`decode`] does, when a file of `len` bytes is too short for a layout's samples.
fn check_len(l: &Layout, len: u64) -> Result<()> {
    let too_large = || Error::InvalidData(format!("{} x {} x {} samples are too many", l.cols, l.rows, l.bands));
    let n = l.cols.checked_mul(l.rows).and_then(|n| n.checked_mul(l.bands)).ok_or_else(too_large)?;
    let bytes = n.checked_mul(l.data_type.size_bytes()).ok_or_else(too_large)? as u64;
    let body = len.saturating_sub(l.offset as u64);
    if body < bytes {
        return Err(Error::InvalidData(format!("expected {bytes} bytes of pixels, found {body}")));
    }
    Ok(())
}

impl RawWindow {
    /// Reads a window of some bands (numbered from 1), band after band.
    fn read_window(&self, window: crate::Window, bands: &[usize]) -> Result<RasterData> {
        let l = &self.layout;
        let size = l.data_type.size_bytes();
        // The samples' buffer, sized from the file's header: checked against the memory the
        // process can use, then allocated fallibly (an error rather than an abort).
        let n = crate::dataset::fallible::len(&[window.width, window.height, bands.len()], RAW_SAMPLES)?;
        crate::dataset::fallible::check_usable(n.checked_mul(size), || {
            format!("{RAW_SAMPLES} ({} x {} x {})", window.width, window.height, bands.len())
        })
        .map_err(|message| Error::Io(std::io::Error::new(std::io::ErrorKind::OutOfMemory, message)))?;
        // The samples of a row of the window are `stride` samples apart in the file.
        let stride = if l.interleave == Interleave::Bip { l.bands } else { 1 };
        let span = if window.width == 0 { 0 } else { ((window.width - 1) * stride + 1) * size };
        // Each row of each band read: its offset in the file and in the window, in file order.
        let mut rows = Vec::with_capacity(window.height * bands.len());
        for (i, &b) in bands.iter().enumerate() {
            let stored = self.stored_bands.as_ref().and_then(|s| s.get(b - 1).copied()).unwrap_or(b - 1);
            for r in 0..window.height {
                let row = window.y + r;
                let file_row = if l.bottom_up { l.rows - 1 - row } else { row };
                let sample = l.interleave.index(stored, file_row, window.x, l.cols, l.rows, l.bands);
                rows.push((l.offset as u64 + (sample * size) as u64, (i * window.height + r) * window.width));
            }
        }
        rows.sort_unstable();
        macro_rules! go {
            ($t:ty, $v:ident) => {{
                let mut out: Vec<$t> = crate::dataset::fallible::filled(n, <$t>::default(), RAW_SAMPLES)?;
                if span > 0 {
                    self.for_each_row(&rows, span, |bytes, at| {
                        for (k, v) in out[at..at + window.width].iter_mut().enumerate() {
                            let i = k * stride * size;
                            let a: [u8; size_of::<$t>()] = bytes[i..i + size].try_into().unwrap_or_default();
                            *v = if l.little_endian { <$t>::from_le_bytes(a) } else { <$t>::from_be_bytes(a) };
                        }
                    })?;
                }
                RasterData::$v(out)
            }};
        }
        Ok(typed!(l.data_type, go))
    }

    /// Reads the rows `(offset, index)` (sorted by offset) of `span` bytes each, calling `f` with
    /// each row's bytes and index: rows next to each other in one read, up to `RAW_READ_BYTES`.
    fn for_each_row(&self, rows: &[(u64, usize)], span: usize, mut f: impl FnMut(&[u8], usize)) -> Result<()> {
        use std::io::{Read, Seek, SeekFrom};
        // Runs of rows that follow one another (or overlap, as the bands of a BIP row do).
        let mut runs = Vec::new();
        let mut i = 0;
        while i < rows.len() {
            let start = rows[i].0;
            let mut end = start + span as u64;
            let mut j = i + 1;
            while j < rows.len() && rows[j].0 <= end && rows[j].0 + span as u64 - start <= RAW_READ_BYTES as u64 {
                end = end.max(rows[j].0 + span as u64);
                j += 1;
            }
            runs.push((i..j, start, end));
            i = j;
        }
        let mut reader: Box<dyn ReadSeek> = match &self.source {
            RawSource::File(p) => Box::new(std::fs::File::open(p)?),
            #[cfg(remote)]
            RawSource::Http(r) => Box::new(r.clone()),
        };
        let mut fetched = 0;
        let mut buf = Vec::new();
        for (k, (range, start, end)) in runs.iter().cloned().enumerate() {
            if k >= fetched {
                fetched = self.prefetch(&runs[k..])? + k;
            }
            // A run is at most `RAW_READ_BYTES`, or one row (as long as the header says): a row
            // longer than the memory the process can use is refused, and the buffer allocated
            // fallibly.
            let len = usize::try_from(end - start).ok();
            let len = crate::dataset::fallible::check_usable(len, || "a raw raster's row".to_string())
                .map_err(|message| Error::Io(std::io::Error::new(std::io::ErrorKind::OutOfMemory, message)))?;
            if len > buf.capacity() {
                buf.clear();
                crate::dataset::fallible::reserve(&mut buf, len, "a raw raster's rows")?;
            }
            buf.resize(len, 0);
            reader.seek(SeekFrom::Start(start))?;
            reader.read_exact(&mut buf)?;
            #[cfg(test)]
            RAW_BYTES_READ.with(|c| c.set(c.get() + buf.len() as u64));
            for &(offset, at) in &rows[range] {
                let o = (offset - start) as usize;
                f(&buf[o..o + span], at);
            }
        }
        Ok(())
    }

    /// Fetches the blocks of a remote file's next runs, over up to `RAW_READ_BYTES`, together
    /// (runs a few blocks apart in one request) before they are read. Returns how many runs it
    /// fetched: all of them for a local file.
    fn prefetch(&self, runs: &[(std::ops::Range<usize>, u64, u64)]) -> Result<usize> {
        #[cfg(remote)]
        if let RawSource::Http(r) = &self.source {
            let start = runs.first().map_or(0, |(_, s, _)| *s);
            let next = runs.iter().take_while(|(_, _, e)| e - start <= RAW_READ_BYTES as u64).count().max(1);
            let ranges: Vec<(u64, u64)> = runs[..next].iter().map(|(_, s, e)| (*s, e - s)).collect();
            r.prefetch(&ranges)?;
            return Ok(next);
        }
        Ok(runs.len())
    }
}

trait ReadSeek: std::io::Read + std::io::Seek {}
impl<T: std::io::Read + std::io::Seek> ReadSeek for T {}

impl crate::cog::WindowSource for RawWindow {
    /// Reads open the file anew: handles share nothing but the remote block cache.
    fn shared_clone(&self) -> Option<Box<dyn crate::cog::WindowSource>> {
        Some(Box::new(self.clone()))
    }

    fn whole_read_keeps_metadata(&self) -> bool {
        true
    }

    fn overviews(&self) -> Vec<(usize, usize)> {
        Vec::new()
    }

    /// A row, as GDAL's raw drivers' blocks.
    fn block_size(&self) -> Option<(usize, usize)> {
        Some((self.layout.cols, 1))
    }

    fn input_paths(&self) -> Result<Vec<String>> {
        Ok(self.inputs.clone())
    }

    fn streamed(&self) -> Option<&dyn crate::cog::StreamedSource> {
        Some(self)
    }

    fn read(&mut self, _level: usize, window: crate::Window, bands: &[usize]) -> Result<RasterData> {
        self.read_window(window, bands)
    }
}

impl crate::cog::StreamedSource for RawWindow {
    fn inputs(&self) -> Vec<String> {
        self.inputs.clone()
    }

    fn read_rows(&self, window: crate::Window, bands: &[usize]) -> Result<RasterData> {
        self.read_window(window, bands)
    }
}

/// Builds a raster from band-major data and a north-up GDAL geotransform.
pub(crate) fn make_raster(
    cols: usize,
    rows: usize,
    bands: usize,
    data: RasterData,
    gt: [f64; 6],
    crs: CrsInfo,
    nodata: Option<f64>,
) -> Result<Raster> {
    let cfg = raster_config(cols, rows, bands, data.data_type(), gt, crs, nodata)?;
    let mut r = crate::pixel::raster_from_data(cfg, data)?;
    // A south-up grid (positive pixel height) keeps its origin at the top.
    if gt[5] > 0.0 {
        r.y_min = gt[3];
    }
    Ok(r)
}

/// The raster [`make_raster`] builds from samples of `data_type`, without them: its data buffer
/// is empty.
pub(crate) fn raster_header(
    cols: usize,
    rows: usize,
    bands: usize,
    data_type: DataType,
    gt: [f64; 6],
    crs: CrsInfo,
    nodata: Option<f64>,
) -> Result<Raster> {
    if bands == 0 {
        // The error (or empty raster) of a raster without bands.
        return make_raster(cols, rows, 0, RasterData::new_filled(data_type, 0, 0.0), gt, crs, nodata);
    }
    let cfg = raster_config(cols, rows, bands, data_type, gt, crs, nodata)?;
    Ok(Raster {
        cols: cfg.cols,
        rows: cfg.rows,
        bands: cfg.bands,
        x_min: cfg.x_min,
        // A south-up grid (positive pixel height) keeps its origin at the top.
        y_min: if gt[5] > 0.0 { gt[3] } else { cfg.y_min },
        cell_size_x: cfg.cell_size,
        cell_size_y: gt[5].abs(),
        nodata: cfg.nodata,
        data_type,
        crs: cfg.crs,
        metadata: cfg.metadata,
        data: RasterData::new_filled(data_type, 0, 0.0),
    })
}

fn raster_config(
    cols: usize,
    rows: usize,
    bands: usize,
    data_type: DataType,
    gt: [f64; 6],
    crs: CrsInfo,
    nodata: Option<f64>,
) -> Result<RasterConfig> {
    if gt[2] != 0.0 || gt[4] != 0.0 {
        return Err(Error::Unsupported("rotated geotransforms are not supported".into()));
    }
    Ok(RasterConfig {
        cols,
        rows,
        bands,
        x_min: gt[0],
        y_min: gt[3] + gt[5] * rows as f64,
        cell_size: gt[1],
        cell_size_y: Some(gt[5].abs()),
        nodata: nodata.unwrap_or(-9999.0),
        data_type,
        crs,
        metadata: Vec::new(),
    })
}

/// A raster's data converted to another type (values rounded and saturated for integers).
pub(crate) fn convert(r: &Raster, to: DataType) -> Raster {
    if r.data_type == to {
        return r.clone();
    }
    let mut out = r.clone();
    out.data = RasterData::from_f64_vec(to, r.data.to_f64_vec().into_iter().map(|v| if is_int(to) { v.round() } else { v }).collect());
    out.data_type = to;
    out
}

/// Whether a type is an integer type.
pub(crate) fn is_int(t: DataType) -> bool {
    !matches!(t, DataType::F32 | DataType::F64)
}

/// The file next to `path` with another extension, matching the extension's case when a file
/// with either case exists.
pub(crate) fn sibling(path: &Path, ext: &str) -> PathBuf {
    let lower = path.with_extension(ext.to_ascii_lowercase());
    if lower.exists() {
        return lower;
    }
    let upper = path.with_extension(ext.to_ascii_uppercase());
    if upper.exists() { upper } else { lower }
}

/// The CRS in a `.prj` file next to `path`, if there is one.
pub(crate) fn read_prj(path: &Path) -> Option<CrsInfo> {
    let text = std::fs::read_to_string(sibling(path, "prj")).ok()?;
    crs_from_text(&text)
}

/// A CRS from WKT, a PROJ string or an EPSG reference.
pub(crate) fn crs_from_text(text: &str) -> Option<CrsInfo> {
    let text = text.trim();
    // A WKT whose outermost node carries an EPSG identifier (WKT2 `ID`, WKT1 `AUTHORITY`).
    if let Some(code) = root_epsg(text) {
        let mut info = CrsInfo::from_epsg(code);
        if info.wkt.is_none() {
            info.wkt = Some(text.to_string());
        }
        return Some(info);
    }
    let mut s = SpatialRef::parse(text).ok()?;
    if s.is_unknown() {
        return None;
    }
    // The registry misidentifies some ESRI names (GCS_North_American_1983 as NAD83(2011)).
    if let Some(code) = esri_name_epsg(text) {
        s.epsg = Some(code);
    } else if let Some(code) = s.epsg {
        // ...and matches some custom CRSs to registered ones of another kind: keep a match only
        // when both are projected, or both geographic.
        let head = text.split('[').next().unwrap_or_default().trim().to_ascii_uppercase();
        let projected = head.starts_with("PROJ");
        let geographic = head.starts_with("GEOG");
        let registered = SpatialRef::from_epsg(code).ok().and_then(|r| r.to_wbp().ok());
        let ok = registered.is_some_and(|r| {
            let kind = &r.projection.params().kind;
            let geocentric = matches!(kind, wbprojection::ProjectionKind::Geocentric | wbprojection::ProjectionKind::Vertical);
            !geocentric && ((projected && !r.is_geographic()) || (geographic && r.is_geographic()) || (!projected && !geographic))
        });
        if !ok {
            s.epsg = None;
        }
    }
    Some(CrsInfo { epsg: s.epsg, wkt: s.wkt.or_else(|| s.epsg.and_then(|c| wbprojection::to_ogc_wkt(c).ok())), proj4: s.proj4 })
}

/// The EPSG code in the identifier that closes a WKT's outermost node.
fn root_epsg(wkt: &str) -> Option<u32> {
    let body = wkt.trim_end().strip_suffix(']')?;
    for key in ["ID[\"EPSG\",", "AUTHORITY[\"EPSG\",\""] {
        if let Some(i) = body.rfind(key) {
            let rest = &body[i + key.len()..];
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            let after = rest[digits.len()..].trim_start_matches('"');
            // Only the identifier's closing bracket may follow.
            if !digits.is_empty() && after.trim() == "]" {
                return digits.parse().ok();
            }
        }
    }
    None
}

/// The EPSG code of a WKT whose outermost name is a common ESRI name, as GDAL writes them in
/// `.prj` files.
fn esri_name_epsg(wkt: &str) -> Option<u32> {
    let start = wkt.find('"')? + 1;
    let name = &wkt[start..start + wkt[start..].find('"')?];
    let geographic = match name {
        "GCS_WGS_1984" => Some(4326),
        "GCS_North_American_1983" => Some(4269),
        "GCS_North_American_1927" => Some(4267),
        "GCS_ETRS_1989" => Some(4258),
        "WGS_1984_Web_Mercator_Auxiliary_Sphere" => Some(3857),
        _ => None,
    };
    if geographic.is_some() {
        return geographic;
    }
    let utm = |prefix: &str| -> Option<(u32, bool)> {
        let rest = name.strip_prefix(prefix)?;
        let (zone, hemi) = rest.split_at(rest.len().checked_sub(1)?);
        Some((zone.parse().ok()?, hemi == "N"))
    };
    match (utm("WGS_1984_UTM_Zone_"), utm("NAD_1983_UTM_Zone_"), utm("NAD_1927_UTM_Zone_")) {
        (Some((z, true)), _, _) => Some(32600 + z),
        (Some((z, false)), _, _) => Some(32700 + z),
        (_, Some((z, true)), _) => Some(26900 + z),
        (_, _, Some((z, true))) => Some(26700 + z),
        _ => None,
    }
}

/// A raster's CRS, identifying its EPSG code from its WKT only when the match is plausible (or
/// else its ESRI code).
pub(crate) fn spatial_ref(crs: &CrsInfo) -> SpatialRef {
    let epsg = crs.epsg.or_else(|| crs.wkt.as_deref().and_then(crs_from_text).and_then(|c| c.epsg));
    SpatialRef { epsg, wkt: crs.wkt.clone(), proj4: crs.proj4.clone(), esri: None }.with_esri()
}

/// The CRS as WKT that GDAL recognizes.
pub(crate) fn esri_wkt(crs: &CrsInfo) -> Option<String> {
    super::wkt::gdal_wkt(&spatial_ref(crs))
}

/// Writes a `.prj` file next to `path` when the CRS is known.
pub(crate) fn write_prj(path: &Path, crs: &CrsInfo) -> Result<()> {
    if let Some(w) = esri_wkt(crs) {
        std::fs::write(path.with_extension("prj"), w)?;
    }
    Ok(())
}

/// Parses a number as GDAL does, accepting `nan` and `inf`.
pub(crate) fn parse_f64(s: &str) -> Option<f64> {
    let s = s.trim();
    match s.to_ascii_lowercase().as_str() {
        "nan" | "-nan" | "+nan" => Some(f64::NAN),
        "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
        "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
        _ => s.parse().ok(),
    }
}

/// Formats a value so that it parses back to the same number of the raster's type.
pub(crate) fn format_value(v: f64, t: DataType) -> String {
    if v.is_nan() {
        "nan".into()
    } else if v.is_infinite() {
        if v > 0.0 { "inf".into() } else { "-inf".into() }
    } else if t == DataType::F32 {
        format!("{}", v as f32)
    } else {
        format!("{v}")
    }
}

/// Formats a number as C's `printf("%.<precision>g")` does (GDAL's metadata and histograms):
/// `precision` significant digits, trailing zeros removed, scientific notation for exponents
/// below -4 or from `precision` up.
pub(crate) fn fmt_g(v: f64, precision: usize) -> String {
    if v.is_nan() {
        return if v.is_sign_negative() { "-nan".into() } else { "nan".into() };
    }
    if v.is_infinite() {
        return if v > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let p = precision.max(1);
    if v == 0.0 {
        return if v.is_sign_negative() { "-0".into() } else { "0".into() };
    }
    // The exponent after rounding to `p` significant digits.
    let sci = format!("{:.*e}", p - 1, v);
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let trim =
        |s: &str| -> String { if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s.to_string() } };
    if exp < -4 || exp >= p as i32 {
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{}e{sign}{:02}", trim(mantissa), exp.abs())
    } else {
        trim(&format!("{:.*}", (p as i32 - 1 - exp).max(0) as usize, v))
    }
}

/// What GDAL's `.aux.xml` sidecar (PAM) holds for formats that cannot store it themselves.
#[derive(Debug, Default)]
pub(crate) struct Pam {
    /// The CRS.
    pub crs: Option<CrsInfo>,
    /// The geotransform.
    pub geo_transform: Option<[f64; 6]>,
    /// The first band's nodata value.
    pub nodata: Option<f64>,
    /// Direct dataset-level fallback, distinct from a band-1 declaration.
    pub dataset_nodata: Option<f64>,
    /// Explicit band nodata, with one-based band indices.
    pub band_nodata: Vec<(usize, f64)>,
}

/// Reads the `.aux.xml` sidecar of a dataset, if there is one.
pub(crate) fn read_pam(path: &Path) -> Option<Pam> {
    if crate::config::sidecars_disabled() {
        return None;
    }
    let mut aux = path.as_os_str().to_owned();
    aux.push(".aux.xml");
    Some(parse_pam(&String::from_utf8(crate::memfs::read(std::path::Path::new(&aux)).ok()?).ok()?))
}

/// Parses the text of a `.aux.xml` sidecar.
pub(crate) fn parse_pam(xml: &str) -> Pam {
    let element = |name: &str| -> Option<String> {
        let start = xml.find(&format!("<{name}"))?;
        let body = &xml[start..];
        let open_end = body.find('>')? + 1;
        let close = body.find(&format!("</{name}>"))?;
        Some(xml_unescape(body[open_end..close].trim()))
    };
    let crs = element("SRS").and_then(|w| crs_from_text(&w));
    let geo_transform = element("GeoTransform").and_then(|g| {
        let v: Vec<f64> = g.split(',').filter_map(|x| x.trim().parse().ok()).collect();
        <[f64; 6]>::try_from(v).ok()
    });
    let root = crate::ops::vrt::xml::parse(xml).ok();
    let band_nodata: Vec<(usize, f64)> = root
        .as_ref()
        .map(|root| {
            root.children
                .iter()
                .filter(|e| e.name == "PAMRasterBand")
                .filter_map(|e| Some((e.attr("band")?.parse::<usize>().ok()?, parse_f64(&e.child("NoDataValue")?.text)?)))
                .collect()
        })
        .unwrap_or_default();
    let dataset_nodata = root.as_ref().and_then(|root| root.child("NoDataValue").and_then(|e| parse_f64(&e.text)));
    let nodata = band_nodata.iter().find(|(b, _)| *b == 1).map(|(_, v)| *v).or(dataset_nodata);
    Pam { crs, geo_transform, nodata, dataset_nodata, band_nodata }
}

/// Records differing band nodata for TIFF, which has only one embedded nodata tag.
pub(crate) fn write_band_nodata_pam(path: &Path, values: &[Option<f64>]) -> Result<()> {
    let mut aux = path.as_os_str().to_owned();
    aux.push(".aux.xml");
    let aux = Path::new(&aux);
    use quick_xml::events::{BytesEnd, BytesStart, BytesText, Event};
    let existing = match crate::memfs::read(aux) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => b"<PAMDataset/>".to_vec(),
        Err(e) => return Err(e.into()),
    };
    let mut reader = quick_xml::Reader::from_reader(existing.as_slice());
    reader.config_mut().expand_empty_elements = true;
    let mut writer = quick_xml::Writer::new(Vec::new());
    let mut depth = 0usize;
    let mut seen_root = false;
    let mut band = None;
    let mut seen = vec![false; values.len()];
    let invalid = |e: quick_xml::Error| crate::Error::InvalidArgument(format!("invalid PAM XML: {e}"));
    let write_nodata = |writer: &mut quick_xml::Writer<Vec<u8>>, value: Option<f64>| -> Result<()> {
        if let Some(value) = value {
            writer.write_event(Event::Start(BytesStart::new("NoDataValue")))?;
            writer.write_event(Event::Text(BytesText::new(&super::gtiff_write::format_nodata(value))))?;
            writer.write_event(Event::End(BytesEnd::new("NoDataValue")))?;
        }
        Ok(())
    };
    loop {
        let event = reader.read_event().map_err(invalid)?;
        match &event {
            Event::Start(e) if (depth == 1 || depth == 2 && band.is_some()) && e.name().as_ref() == "NoDataValue" => {
                reader.read_to_end(e.name()).map_err(invalid)?;
                continue;
            }
            Event::Start(e) => {
                if depth == 0 {
                    if seen_root || e.name().as_ref() != "PAMDataset" {
                        return Err(Error::InvalidData("expected one PAMDataset root".into()));
                    }
                    seen_root = true;
                }
                if depth == 1 && e.name().as_ref() == "PAMRasterBand" {
                    band = e
                        .attributes()
                        .filter_map(std::result::Result::ok)
                        .find(|a| a.key.as_ref() == "band")
                        .and_then(|a| a.value.parse::<usize>().ok())
                        .and_then(|b| b.checked_sub(1))
                        .filter(|b| *b < values.len());
                    if let Some(b) = band {
                        seen[b] = true;
                    }
                }
                depth += 1;
            }
            Event::End(_) => {
                if depth == 2 {
                    if let Some(b) = band.take() {
                        write_nodata(&mut writer, values[b])?;
                    }
                } else if depth == 1 {
                    for (b, value) in values.iter().enumerate().filter(|(b, _)| !seen[*b]) {
                        let mut element = BytesStart::new("PAMRasterBand");
                        let number = (b + 1).to_string();
                        element.push_attribute(("band", number.as_str()));
                        writer.write_event(Event::Start(element))?;
                        write_nodata(&mut writer, *value)?;
                        writer.write_event(Event::End(BytesEnd::new("PAMRasterBand")))?;
                    }
                }
                depth = depth.checked_sub(1).ok_or_else(|| Error::InvalidData("unbalanced PAM XML".into()))?;
            }
            Event::Eof => break,
            _ => {}
        }
        writer.write_event(event)?;
    }
    if depth != 0 || !seen_root {
        return Err(Error::InvalidData("unclosed or missing PAMDataset root".into()));
    }
    crate::memfs::write(aux, writer.into_inner())?;
    Ok(())
}

/// Writes the CRS to a `.aux.xml` sidecar, as GDAL does for formats without a CRS of their own.
pub(crate) fn write_pam(path: &Path, crs: &CrsInfo) -> Result<()> {
    write_pam_with(path, Some(crs), None, 0)
}

/// Writes a `.aux.xml` sidecar with a CRS and a nodata value for each of `bands` bands. Nothing
/// is written when there is nothing to record.
pub(crate) fn write_pam_with(path: &Path, crs: Option<&CrsInfo>, nodata: Option<f64>, bands: usize) -> Result<()> {
    if let Some(xml) = pam_xml(crs, nodata, bands) {
        let mut aux = path.as_os_str().to_owned();
        aux.push(".aux.xml");
        std::fs::write(aux, xml)?;
    }
    Ok(())
}

/// The text of a `.aux.xml` sidecar with a CRS and a nodata value for each of `bands` bands;
/// `None` when there is nothing to record.
pub(crate) fn pam_xml(crs: Option<&CrsInfo>, nodata: Option<f64>, bands: usize) -> Option<String> {
    let wkt = crs.and_then(esri_wkt);
    if wkt.is_none() && (nodata.is_none() || bands == 0) {
        return None;
    }
    let mut xml = String::from("<PAMDataset>\n");
    if let Some(wkt) = wkt {
        let wkt = wkt.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;");
        xml.push_str(&format!("  <SRS dataAxisToSRSAxisMapping=\"1,2\">{wkt}</SRS>\n"));
    }
    if let Some(nd) = nodata {
        for b in 1..=bands {
            xml.push_str(&format!(
                "  <PAMRasterBand band=\"{b}\">\n    <NoDataValue>{}</NoDataValue>\n  </PAMRasterBand>\n",
                super::gtiff_write::format_nodata(nd)
            ));
        }
    }
    xml.push_str("</PAMDataset>\n");
    Some(xml)
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// A raster whose geotransform is GDAL's default (no georeferencing).
pub(crate) const NO_GEOTRANSFORM: [f64; 6] = [0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

/// GDAL's WKT of WGS 84 in latitude, longitude order (`SRS_WKT_WGS84_LAT_LONG`), which it gives
/// the geolocation arrays of swaths.
pub(crate) const WGS84_LAT_LONG_WKT: &str = "GEOGCS[\"WGS 84\",DATUM[\"WGS_1984\",SPHEROID[\"WGS 84\",6378137,298.257223563,AUTHORITY[\"EPSG\",\"7030\"]],AUTHORITY[\"EPSG\",\"6326\"]],PRIMEM[\"Greenwich\",0,AUTHORITY[\"EPSG\",\"8901\"]],UNIT[\"degree\",0.0174532925199433,AUTHORITY[\"EPSG\",\"9122\"]],AXIS[\"Latitude\",NORTH],AXIS[\"Longitude\",EAST],AUTHORITY[\"EPSG\",\"4326\"]]";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pam_nodata_preserves_dataset_fallback_and_other_xml() {
        let xml =
            "<PAMDataset><NoDataValue>-9</NoDataValue><PAMRasterBand band=\"2\"><NoDataValue>-2</NoDataValue></PAMRasterBand></PAMDataset>";
        assert_eq!(parse_pam(xml).nodata, Some(-9.));
        assert_eq!(parse_pam(&xml.replace("<NoDataValue>-9</NoDataValue>", "")).nodata, None);
        let metadata = "<Metadata domain=\"xml:test\">before<b attr=\"v\"/>after<![CDATA[<literal>]]><!--keep--></Metadata>";
        let existing = format!(
            "<PAMDataset>{metadata}<PAMRasterBand band=\"1\"><Description>band</Description><NoDataValue>-1</NoDataValue></PAMRasterBand><PAMRasterBand band=\"2\"/></PAMDataset>"
        );
        let dir = tempfile::tempdir().unwrap();
        let mount = crate::memfs::Mount::new(Vec::new());
        for root in [dir.path(), mount.path()] {
            let path = root.join("output.tif");
            let aux = root.join("output.tif.aux.xml");
            crate::memfs::write(&aux, &existing).unwrap();
            write_band_nodata_pam(&path, &[None, Some(0.1), Some(-3.)]).unwrap();
            let updated = String::from_utf8(crate::memfs::read(&aux).unwrap()).unwrap();
            assert!(updated.contains(&metadata.replace("<b attr=\"v\"/>", "<b attr=\"v\"></b>")));
            assert!(updated.contains("<Description>band</Description>"));
            assert!(!updated.contains("<NoDataValue>-1</NoDataValue>"));
            assert_eq!(parse_pam(&updated).band_nodata, vec![(2, 0.1), (3, -3.)]);
            assert_eq!(parse_pam(&updated).nodata, None);
            crate::memfs::write(&aux, "<PAMDataset/>").unwrap();
            write_band_nodata_pam(&path, &[Some(-8.)]).unwrap();
            assert_eq!(read_pam(&path).unwrap().nodata, Some(-8.));
        }
        let broken = dir.path().join("broken.tif");
        crate::memfs::write(&dir.path().join("broken.tif.aux.xml"), "<PAMDataset><Metadata>").unwrap();
        assert!(write_band_nodata_pam(&broken, &[Some(1.)]).is_err());
    }

    #[test]
    fn formats_like_printf_g() {
        assert_eq!(fmt_g(0.1, 8), "0.1");
        assert_eq!(fmt_g(9.96921e36, 8), "9.96921e+36");
        assert_eq!(fmt_g(123456789.0, 8), "1.2345679e+08");
        assert_eq!(fmt_g(1e-5, 15), "1e-05");
        assert_eq!(fmt_g(0.0001, 15), "0.0001");
        assert_eq!(fmt_g(-2.5, 15), "-2.5");
        assert_eq!(fmt_g(100.0, 3), "100");
        assert_eq!(fmt_g(1000.0, 3), "1e+03");
        assert_eq!(fmt_g(0.1f32 as f64, 8), "0.1");
        assert_eq!(fmt_g(0.0, 15), "0");
    }

    #[test]
    fn raw_layouts_round_trip() {
        let data: Vec<i16> = (0..24).map(|i| i as i16 - 5).collect();
        let r = make_raster(4, 3, 2, RasterData::I16(data.clone()), [0.0, 1.0, 0.0, 3.0, 0.0, -1.0], CrsInfo::default(), None).unwrap();
        for interleave in [Interleave::Bsq, Interleave::Bil, Interleave::Bip] {
            for (little, bottom_up) in [(true, false), (false, true)] {
                let l = Layout {
                    cols: 4,
                    rows: 3,
                    bands: 2,
                    data_type: DataType::I16,
                    little_endian: little,
                    interleave,
                    offset: 3,
                    bottom_up,
                };
                let bytes = encode(&r, &l);
                let RasterData::I16(back) = decode(&bytes, &l).unwrap() else { panic!() };
                assert_eq!(back, data);
            }
        }
    }

    /// The samples of a raw raster are allocated fallibly, after a check against the memory the
    /// process can use: a header whose grid holds more (a 64 GiB sparse data file here) is refused
    /// with an error naming the buffer, where an infallible allocation ends the process.
    #[test]
    fn raw_samples_larger_than_the_usable_memory_are_refused() {
        let (cols, rows) = (1usize << 18, 1usize << 18);
        let usable = crate::dataset::fallible::usable_memory().unwrap_or(u64::MAX);
        if (cols * rows) as u64 <= usable {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse.bin");
        std::fs::File::create(&path).unwrap().set_len((cols * rows) as u64).unwrap();
        let l = Layout {
            cols,
            rows,
            bands: 1,
            data_type: DataType::U8,
            little_endian: true,
            interleave: Interleave::Bsq,
            offset: 0,
            bottom_up: false,
        };
        let header = raster_header(cols, rows, 1, DataType::U8, [0.0, 1.0, 0.0, 0.0, 0.0, -1.0], CrsInfo::default(), None).unwrap();
        let raw = RawRaster::open(RasterDataset::with_driver(header, crate::formats::RasterDriver::EHdr, false), &path, l, &[]).unwrap();
        let err = raw.read().map(|_| ()).unwrap_err().to_string();
        assert!(err.contains("a raw raster's samples (262144 x 262144 x 1)") && err.contains("of memory the process can use"), "{err}");
    }

    /// Sets this process's soft limit `resource` to `bytes`, its hard limit kept (a process may not
    /// raise a finite hard limit).
    #[cfg(target_os = "linux")]
    fn set_soft_limit(resource: libc::__rlimit_resource_t, bytes: u64) {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `limit` is a valid rlimit, written by the first call and read by the second.
        assert_eq!(unsafe { libc::getrlimit(resource, &mut limit) }, 0);
        limit.rlim_cur = bytes;
        // SAFETY: as above.
        assert_eq!(
            unsafe { libc::setrlimit(resource, &limit) },
            0,
            "a soft limit of {bytes} bytes under a hard limit of {}",
            limit.rlim_max
        );
    }

    /// The address space this process has mapped (`VmSize`).
    #[cfg(target_os = "linux")]
    fn mapped_bytes() -> u64 {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let kb =
            status.lines().find_map(|l| l.strip_prefix("VmSize:")).and_then(|v| v.trim().strip_suffix("kB")?.trim().parse::<u64>().ok());
        kb.unwrap() * 1024
    }

    /// A raw raster of `cols` x `rows` x `bands` bytes in `interleave`, its data file at `path` sparse
    /// but for the bytes `set` (offset, value).
    #[cfg(target_os = "linux")]
    fn sparse_raw(path: &Path, (cols, rows, bands): (usize, usize, usize), interleave: Interleave, set: &[(u64, u8)]) -> RawRaster {
        use std::os::unix::fs::FileExt;
        let file = std::fs::File::create(path).unwrap();
        file.set_len((cols * rows * bands) as u64).unwrap();
        for &(offset, value) in set {
            file.write_all_at(&[value], offset).unwrap();
        }
        let l = Layout { cols, rows, bands, data_type: DataType::U8, little_endian: true, interleave, offset: 0, bottom_up: false };
        let header = raster_header(cols, rows, bands, DataType::U8, [0.0, 1.0, 0.0, 0.0, 0.0, -1.0], CrsInfo::default(), None).unwrap();
        RawRaster::open(RasterDataset::with_driver(header, crate::formats::RasterDriver::EHdr, false), path, l, &[]).unwrap()
    }

    /// The samples' buffer of a raw raster is allocated fallibly: a size that the check against the
    /// usable memory lets through but the allocator cannot give is an error naming the buffer, where
    /// an infallible allocation aborts the process. In a process of its own whose address-space
    /// limit (the soft one, the hard one kept) leaves 64 MiB unmapped: a raster of as many samples
    /// as the memory the process can use (that limit, which counts what is mapped already), its data
    /// file sparse, is read whole.
    #[cfg(target_os = "linux")]
    #[test]
    fn raw_samples_the_allocator_cannot_give_are_an_error() {
        crate::par::alone(120, || {
            let dir = tempfile::tempdir().unwrap();
            let limit = mapped_bytes() + (64 << 20);
            set_soft_limit(libc::RLIMIT_AS, limit);
            assert_eq!(crate::dataset::fallible::usable_memory(), Some(limit));
            let cols = 1 << 16;
            let rows = limit as usize / cols;
            let raw = sparse_raw(&dir.path().join("samples.bin"), (cols, rows, 1), Interleave::Bsq, &[]);
            let err = raw.read().map(|_| ()).unwrap_err().to_string();
            assert_eq!(err, format!("cannot allocate {} bytes for a raw raster's samples", cols * rows));
            true
        });
    }

    /// A run of a raw raster's rows longer than the memory the process can use is refused before its
    /// buffer is allocated: where the system overcommits memory, the allocation succeeds and the
    /// process is killed as the buffer is filled. Rows next to each other are read together up to
    /// 16 MiB, so a longer run is one row, and a band's row in a BIP raster spans the samples of every
    /// band from its first sample to its last. In a process of its own whose resident-set limit
    /// (which the usable memory counts, and Linux does not enforce) is 64 MiB: band 1 of the one row
    /// of 32,768 samples of 4,096 bands (134,213,633 bytes from its first sample to its last) is
    /// refused; of 1,024 bands (33,553,409 bytes), read. The data files are sparse.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_raw_row_longer_than_the_usable_memory_is_refused() {
        crate::par::alone(120, || {
            set_soft_limit(libc::RLIMIT_RSS, 64 << 20);
            assert_eq!(crate::dataset::fallible::usable_memory(), Some(64 << 20));
            let dir = tempfile::tempdir().unwrap();
            let cols = 1 << 15;
            let read = |bands: usize| -> Result<Vec<u8>> {
                // Band 1's first and last samples.
                let set = [(0, 7), (((cols - 1) * bands) as u64, 9)];
                let raw = sparse_raw(&dir.path().join(format!("{bands}.bin")), (cols, 1, bands), Interleave::Bip, &set);
                let options = crate::WindowOptions { bands: vec![1], overview: None };
                let RasterData::U8(samples) = raw.into_lazy().read_window(crate::Window::new(0, 0, cols, 1), &options)?.raster.data else {
                    panic!("bytes")
                };
                Ok(samples)
            };
            match read(4096) {
                Err(e) => assert_eq!(
                    e.to_string(),
                    "a raw raster's row: 134213633 bytes, more than the 67108864 bytes of memory the process can use"
                ),
                Ok(samples) => panic!("a row of 134213633 bytes read: {} samples", samples.len()),
            }
            let samples = read(1024).unwrap();
            assert_eq!((samples.len(), samples[0], samples[cols - 1]), (cols, 7, 9));
            assert_eq!(samples[1..cols - 1].iter().filter(|&&v| v != 0).count(), 0);
            true
        });
    }

    #[test]
    fn raw_windows_read_the_rows_they_cover() {
        let (cols, rows, bands) = (7, 5, 3);
        let data: Vec<i16> = (0..cols * rows * bands).map(|i| i as i16 * 3 - 50).collect();
        let gt = [10.0, 1.0, 0.0, 20.0, 0.0, -1.0];
        let r = make_raster(cols, rows, bands, RasterData::I16(data.clone()), gt, CrsInfo::default(), None).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw.bin");
        for interleave in [Interleave::Bsq, Interleave::Bil, Interleave::Bip] {
            for (little, bottom_up) in [(true, false), (false, true)] {
                let l = Layout { cols, rows, bands, data_type: DataType::I16, little_endian: little, interleave, offset: 5, bottom_up };
                std::fs::write(&path, encode(&r, &l)).unwrap();
                let open = || {
                    let header = raster_header(cols, rows, bands, DataType::I16, gt, CrsInfo::default(), None).unwrap();
                    RawRaster::open(RasterDataset::with_driver(header, crate::formats::RasterDriver::EHdr, false), &path, l, &[]).unwrap()
                };
                // Read whole, as `decode` reads it.
                let RasterData::I16(whole) = open().read().unwrap().raster.data else { panic!() };
                assert_eq!(whole, data, "{interleave:?}");
                let mut lazy = open().into_lazy();
                for (x, y, w, h) in [(0, 0, cols, rows), (2, 1, 3, 2), (6, 4, 1, 1), (0, 3, cols, 2)] {
                    for wanted in [vec![1, 2, 3], vec![3, 1], vec![2]] {
                        let options = crate::WindowOptions { bands: wanted.clone(), overview: None };
                        let RasterData::I16(got) = lazy.read_window(crate::Window::new(x, y, w, h), &options).unwrap().raster.data else {
                            panic!()
                        };
                        let want: Vec<i16> = wanted
                            .iter()
                            .flat_map(|b| (y..y + h).flat_map(move |row| (x..x + w).map(move |c| ((b - 1) * rows + row) * cols + c)))
                            .map(|i| data[i])
                            .collect();
                        assert_eq!(got, want, "{interleave:?} {little} {bottom_up} {x},{y} {w}x{h} {wanted:?}");
                    }
                }
                // A band sequential window reads its samples and nothing else.
                if interleave == Interleave::Bsq {
                    let before = RAW_BYTES_READ.with(|c| c.get());
                    lazy.read_window(crate::Window::new(2, 1, 3, 2), &crate::WindowOptions { bands: vec![2], overview: None }).unwrap();
                    assert_eq!(RAW_BYTES_READ.with(|c| c.get()) - before, 3 * 2 * 2);
                }
            }
        }
        // A file too short for its samples fails to open, as it fails to be read whole.
        let l = Layout {
            cols,
            rows,
            bands,
            data_type: DataType::I16,
            little_endian: true,
            interleave: Interleave::Bsq,
            offset: 0,
            bottom_up: false,
        };
        std::fs::write(&path, [0u8; 10]).unwrap();
        let header = raster_header(cols, rows, bands, DataType::I16, gt, CrsInfo::default(), None).unwrap();
        let err =
            RawRaster::open(RasterDataset::with_driver(header, crate::formats::RasterDriver::EHdr, false), &path, l, &[]).err().unwrap();
        assert_eq!(err.to_string(), decode(&[0u8; 10], &l).unwrap_err().to_string());
    }

    #[test]
    fn raster_headers_are_rasters_without_samples() {
        for gt in [[10.0, 2.0, 0.0, 20.0, 0.0, -3.0], [10.0, 2.0, 0.0, 20.0, 0.0, 3.0]] {
            let crs = CrsInfo::from_epsg(4326);
            let full = make_raster(4, 3, 2, RasterData::F32(vec![1.0; 24]), gt, crs.clone(), Some(-1.0)).unwrap();
            let mut header = raster_header(4, 3, 2, DataType::F32, gt, crs, Some(-1.0)).unwrap();
            assert_eq!(header.data.len(), 0);
            header.data = full.data.clone();
            assert_eq!(format!("{header:?}"), format!("{full:?}"));
        }
    }
}

#[cfg(test)]
mod band_pam_tests {
    #[test]
    fn updates_disk_and_virtual_pam_without_reordering_mixed_content() {
        let dir = tempfile::tempdir().unwrap();
        let mount = crate::memfs::Mount::new([]);
        for root in [dir.path(), mount.path()] {
            let path = root.join("raster.tif");
            let aux = root.join("raster.tif.aux.xml");
            let content = r#"<PAMDataset><SRS>EPSG:4326</SRS><!--keep--><Metadata><MDI key="text">before<Part/>after</MDI></Metadata><PAMRasterBand band="1"><NoDataValue>9</NoDataValue><Metadata><MDI key="STATISTICS_MINIMUM">2</MDI></Metadata></PAMRasterBand></PAMDataset>"#;
            crate::memfs::write(&aux, content).unwrap();
            super::write_band_nodata_pam(&path, &[Some(-1.), Some(-2.)]).unwrap();
            let text = String::from_utf8(crate::memfs::read(&aux).unwrap()).unwrap();
            assert!(text.contains("before<Part></Part>after"));
            assert!(text.contains("<!--keep-->"));
            assert!(text.contains("STATISTICS_MINIMUM"));
            assert!(text.contains("<SRS>EPSG:4326</SRS>"));
            assert_eq!(super::parse_pam(&text).band_nodata, vec![(1, -1.), (2, -2.)]);
        }
    }
}

#[cfg(test)]
mod dataset_pam_fallback_tests {
    #[test]
    fn dataset_fallback_applies_to_missing_band_declarations() {
        let pam = super::parse_pam(
            "<PAMDataset><NoDataValue>-10</NoDataValue><PAMRasterBand band=\"2\"><NoDataValue>-20</NoDataValue></PAMRasterBand></PAMDataset>",
        );
        let mut ds = crate::ops::build_raster(
            crate::ops::Grid { width: 1, height: 1, geo_transform: [0., 1., 0., 1., 0., -1.] },
            vec![vec![1.], vec![2.]],
            wbraster::DataType::F64,
            None,
            &Default::default(),
            crate::ops::convert_value,
        )
        .unwrap();
        super::super::nodata::apply(pam, &mut ds);
        assert_eq!(ds.band_nodata, vec![Some(-10.), Some(-20.)]);
    }
}

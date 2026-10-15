//! Reading point clouds: LAS, LAZ, COPC, EPT, PLY and E57, whole or in chunks.

use super::copc::{Copc, CopcInfo, CopcNode};
use super::crs::LasCrs;
use super::las::{Columns, LasHeader, Layout, record_core};
use super::values::{Dimension, ValueType, Values};
use super::{PointCloud, PointFilter, PointReadOptions};
use crate::crs::SpatialRef;
use crate::{Error, Result};
use std::io::{Cursor, Read, Seek, SeekFrom};

/// The points decoded at a time.
const CHUNK_POINTS: usize = 65_536;
/// The points of a chunk when reading in chunks, by default.
pub const DEFAULT_CHUNK_POINTS: usize = 1 << 20;

/// Where the bytes of a point cloud come from.
pub(crate) enum Source {
    File(std::io::BufReader<std::fs::File>),
    Memory(Cursor<Vec<u8>>),
    #[cfg(not(target_arch = "wasm32"))]
    Http(crate::http::RangeReader),
}

impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Source::File(f) => f.read(buf),
            Source::Memory(c) => c.read(buf),
            #[cfg(not(target_arch = "wasm32"))]
            Source::Http(h) => h.read(buf),
        }
    }
}

impl Seek for Source {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        match self {
            Source::File(f) => f.seek(pos),
            Source::Memory(c) => c.seek(pos),
            #[cfg(not(target_arch = "wasm32"))]
            Source::Http(h) => h.seek(pos),
        }
    }
}

impl Source {
    /// Fetches byte ranges `(offset, length)` ahead of reading them, when the source is remote.
    pub(crate) fn prefetch(&self, ranges: &[(u64, u64)]) -> Result<()> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Source::Http(h) = self {
            h.prefetch(ranges)?;
        }
        let _ = ranges;
        Ok(())
    }

    /// The bytes at `offset`.
    pub(crate) fn read_at(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        self.seek(SeekFrom::Start(offset))?;
        let mut b = vec![0u8; len];
        self.read_exact(&mut b)?;
        Ok(b)
    }

    /// The length of a remote file, `None` for local files and memory.
    pub(crate) fn remote_len(&self) -> Option<u64> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Source::Http(h) = self {
            return Some(h.len());
        }
        None
    }

    /// Whether the bytes are fetched over HTTP.
    pub(crate) fn is_remote(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        if let Source::Http(_) = self {
            return true;
        }
        false
    }

    /// An empty source, to stand in for one moved elsewhere for a while.
    fn placeholder() -> Self {
        Source::Memory(Cursor::new(Vec::new()))
    }

    /// Opens a local file (or downloads or extracts a remote or archived one).
    fn open_local(path: &str) -> Result<(Self, crate::vfs::LocalPath)> {
        let local = crate::vfs::resolve(path)?;
        let file = std::fs::File::open(local.path()).map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("'{path}': {e}"))))?;
        Ok((Source::File(std::io::BufReader::with_capacity(1 << 20, file)), local))
    }
}

/// A point cloud format's reader. Points are read whole or in chunks: [`FormatReader::start`]
/// sets up the chunks, [`FormatReader::next_chunk`] returns them until there are no more.
pub(crate) trait FormatReader: Send + Sync {
    /// The header: for formats other than LAS, made up from the file's metadata (point count,
    /// bounds, a point format that holds the dimensions, scale and offset).
    fn header(&self) -> &LasHeader;
    /// The CRS.
    fn crs(&self) -> &LasCrs;
    /// The dimensions: `(name, type)`.
    fn dimensions(&self) -> Vec<(String, ValueType)>;
    /// The format's name.
    fn format_name(&self) -> &'static str;
    /// Prepares reading the points in chunks of about `chunk` points.
    fn start(&mut self, options: &PointReadOptions, chunk: usize) -> Result<()>;
    /// The next chunk, `None` when all the points are read.
    fn next_chunk(&mut self) -> Result<Option<Vec<Dimension>>>;
    /// Reads all the points.
    fn read(&mut self, options: &PointReadOptions) -> Result<Vec<Dimension>> {
        self.start(options, DEFAULT_CHUNK_POINTS)?;
        let mut parts = Vec::new();
        while let Some(c) = self.next_chunk()? {
            parts.push(c);
        }
        Ok(concat(parts, &self.dimensions()))
    }
    /// The header to give the points read: [`FormatReader::header`], with what the points need
    /// from the file (a LAS file's waveform data packets).
    fn cloud_header(&mut self) -> Result<LasHeader> {
        Ok(self.header().clone())
    }
    /// The COPC information record.
    fn copc_info(&self) -> Option<&CopcInfo> {
        None
    }
    /// The COPC octree nodes.
    fn copc_nodes(&mut self) -> Result<Vec<CopcNode>> {
        Ok(Vec::new())
    }
    /// The EPT metadata.
    fn ept_info(&self) -> Option<&super::ept::EptInfo> {
        None
    }
    /// The `.wdp` file of the waveforms.
    fn waveform_file(&self) -> Option<&std::path::Path> {
        None
    }
    /// The waveform data packets: their bytes, from the file or its `.wdp` companion.
    fn waveform_data(&mut self, _offset: u64, _len: usize) -> Result<Vec<u8>> {
        Err(Error::Unsupported(format!("{} files have no waveform data packets", self.format_name())))
    }
}

/// Concatenates chunks of the same dimensions; an empty list gives empty columns of `dims`.
pub(crate) fn concat(parts: Vec<Vec<Dimension>>, dims: &[(String, ValueType)]) -> Vec<Dimension> {
    let mut parts = parts.into_iter();
    let Some(mut out) = parts.next() else {
        return dims.iter().map(|(n, t)| Dimension::new(n, Values::with_capacity(*t, 0))).collect();
    };
    for p in parts {
        for (d, o) in out.iter_mut().zip(p) {
            d.values.extend_from(&o.values);
        }
    }
    out
}

/// Which points of a chunk to keep: those that pass a filter, every `stride`-th point of the
/// file (`first` is the index of the chunk's first point).
pub(crate) fn apply_options(dims: Vec<Dimension>, filter: &PointFilter, stride: u64, first: u64) -> Vec<Dimension> {
    if filter.is_empty() && stride <= 1 {
        return dims;
    }
    let cloud = PointCloud::new(dims, SpatialRef::default());
    let n = cloud.len();
    let mut keep: Vec<bool> = (0..n).map(|i| stride <= 1 || (first + i as u64).is_multiple_of(stride)).collect();
    if !filter.is_empty() {
        let pass = cloud.filter_mask(filter);
        for (k, p) in keep.iter_mut().zip(pass) {
            *k &= p;
        }
    }
    cloud.select(&keep).dimensions
}

/// The stride that thins `n` points to about `max_points`.
pub(crate) fn stride_for(n: u64, max_points: Option<u64>) -> u64 {
    match max_points {
        Some(m) if m > 0 && n > m => n.div_ceil(m),
        _ => 1,
    }
}

/// An open point cloud file: its header is read, its points are read on demand, whole or in
/// chunks.
pub struct PointCloudReader {
    inner: Box<dyn FormatReader>,
    name: String,
}

impl std::fmt::Debug for PointCloudReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PointCloudReader")
            .field("name", &self.name)
            .field("format", &self.inner.format_name())
            .field("header", self.inner.header())
            .finish_non_exhaustive()
    }
}

/// The point cloud format of a path, by its name: `ept.json` (or an `ept://` path) is EPT,
/// `.ply` PLY, `.e57` E57; others are read as LAS, LAZ or COPC.
fn format_of(path: &str) -> &'static str {
    if path.starts_with("ept://") {
        return "EPT";
    }
    let name = crate::vfs::Location::parse(path).file_name().to_ascii_lowercase();
    let name = name.split(['?', '#']).next().unwrap_or_default();
    if name == "ept.json" {
        "EPT"
    } else if name.ends_with(".ply") {
        "PLY"
    } else if name.ends_with(".e57") {
        "E57"
    } else {
        "LAS"
    }
}

/// Opens a point cloud: a LAS, LAZ or COPC file, an EPT dataset (its `ept.json`), or a PLY or E57
/// file; a path, URL, `s3://` object or zip archive member. Only the header (or metadata) is
/// read. Remote COPC files and EPT datasets are read in place, with HTTP requests for the parts
/// read; other remote files are downloaded first.
pub fn open_points(path: &str) -> Result<PointCloudReader> {
    let location = crate::vfs::Location::parse(path);
    let name = location.file_name();
    let inner: Box<dyn FormatReader> = match format_of(path) {
        "EPT" => Box::new(super::ept::EptReader::open(path)?),
        "PLY" => {
            let (source, local) = Source::open_local(path)?;
            Box::new(super::ply::PlyReader::new(source, Some(local))?)
        }
        "E57" => {
            let (source, local) = Source::open_local(path)?;
            Box::new(super::e57::E57File::new(source, Some(local))?)
        }
        _ => Box::new(LasReader::open(path)?),
    };
    Ok(PointCloudReader { inner, name })
}

/// Opens a point cloud held in memory: LAS, LAZ, COPC, PLY or E57, told apart by their first
/// bytes.
pub fn open_points_bytes(bytes: Vec<u8>, name: &str) -> Result<PointCloudReader> {
    let inner: Box<dyn FormatReader> = if bytes.starts_with(b"ply") {
        Box::new(super::ply::PlyReader::new(Source::Memory(Cursor::new(bytes)), None)?)
    } else if bytes.starts_with(b"ASTM-E57") {
        Box::new(super::e57::E57File::new(Source::Memory(Cursor::new(bytes)), None)?)
    } else {
        let mut source = Source::Memory(Cursor::new(bytes));
        let header = LasHeader::read(&mut source)?;
        Box::new(LasReader::new(source, header, None, None)?)
    };
    Ok(PointCloudReader { inner, name: name.to_string() })
}

impl PointCloudReader {
    /// The header. For formats other than LAS, LAZ and COPC, one made up from the file's
    /// metadata: the point count, bounds, a point format that holds the dimensions, a scale and
    /// an offset (those of the coordinates, when the format has them).
    pub fn header(&self) -> &LasHeader {
        self.inner.header()
    }

    /// The file name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The format: `COPC`, `LAZ`, `LAS`, `EPT`, `PLY` or `E57`.
    pub fn format_name(&self) -> &'static str {
        self.inner.format_name()
    }

    /// The horizontal CRS.
    pub fn crs(&self) -> &SpatialRef {
        &self.inner.crs().horizontal
    }

    /// The EPSG code and name of the vertical CRS, when the file declares one.
    pub fn vertical_crs(&self) -> (Option<u32>, Option<String>) {
        let c = self.inner.crs();
        (c.vertical_epsg, c.vertical_name.clone())
    }

    /// The dimensions of the points: `(name, type)`.
    pub fn dimensions(&self) -> Vec<(String, ValueType)> {
        self.inner.dimensions()
    }

    /// The COPC information record, for COPC files.
    pub fn copc_info(&self) -> Option<&CopcInfo> {
        self.inner.copc_info()
    }

    /// The nodes of a COPC file's octree (loading every hierarchy page). Empty for other files.
    pub fn copc_nodes(&mut self) -> Result<Vec<CopcNode>> {
        self.inner.copc_nodes()
    }

    /// The metadata of an EPT dataset.
    pub fn ept_info(&self) -> Option<&super::ept::EptInfo> {
        self.inner.ept_info()
    }

    /// A point cloud without points, with the header (and, for LAS files with waveforms, the
    /// waveform data packets), CRS and dimensions of the points a read gives: the template of a
    /// [`super::PointWriter`].
    pub fn template(&mut self) -> Result<PointCloud> {
        let dimensions = self.inner.dimensions().iter().map(|(n, t)| Dimension::new(n, Values::with_capacity(*t, 0))).collect();
        Ok(PointCloud { header: self.inner.cloud_header()?, crs: self.inner.crs().horizontal.clone(), dimensions })
    }

    /// The `.wdp` file that holds the waveforms of a LAS file's points, when they are in one.
    pub fn waveform_file(&self) -> Option<&std::path::Path> {
        self.inner.waveform_file()
    }

    /// Reads the points.
    pub fn read(&mut self, options: &PointReadOptions) -> Result<PointCloud> {
        let dimensions = self.inner.read(options)?;
        Ok(PointCloud { header: self.inner.cloud_header()?, crs: self.inner.crs().horizontal.clone(), dimensions })
    }

    /// Reads the points in chunks of about `chunk_points` points (the default when 0), for files
    /// larger than memory: an iterator of point clouds, each with the file's header.
    ///
    /// ```no_run
    /// let mut f = terrio::pointcloud::open_points("big.laz")?;
    /// let mut ground = 0;
    /// for chunk in f.chunks(&Default::default(), 1_000_000)? {
    ///     ground += chunk?.classification_counts().iter().filter(|(c, _)| *c == 2).map(|(_, n)| n).sum::<u64>();
    /// }
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn chunks(&mut self, options: &PointReadOptions, chunk_points: usize) -> Result<PointChunks<'_>> {
        self.start_chunks(options, chunk_points)?;
        Ok(PointChunks { reader: self })
    }

    /// Prepares reading the points in chunks, which [`PointCloudReader::next_chunk`] returns.
    pub fn start_chunks(&mut self, options: &PointReadOptions, chunk_points: usize) -> Result<()> {
        let chunk = if chunk_points == 0 { DEFAULT_CHUNK_POINTS } else { chunk_points };
        self.inner.start(options, chunk)
    }

    /// The next chunk of points after [`PointCloudReader::start_chunks`], `None` when all are
    /// read.
    pub fn next_chunk(&mut self) -> Result<Option<PointCloud>> {
        let Some(dimensions) = self.inner.next_chunk()? else { return Ok(None) };
        Ok(Some(PointCloud { header: self.inner.header().clone(), crs: self.inner.crs().horizontal.clone(), dimensions }))
    }

    /// The wave packet descriptors (LAS point formats 4, 5, 9 and 10), by index.
    pub fn wave_packet_descriptors(&self) -> Vec<super::waveform::WavePacketDescriptor> {
        super::waveform::WavePacketDescriptor::from_header(self.inner.header())
    }

    /// The waveform of a point: its samples, from the waveform data packets in the file or in its
    /// `.wdp` companion file, as its `wavepacket_index`, `wavepacket_offset` and
    /// `wavepacket_size` dimensions locate them.
    pub fn waveform(&mut self, index: u8, offset: u64, size: u32) -> Result<super::waveform::Waveform> {
        let descriptor = self
            .wave_packet_descriptors()
            .into_iter()
            .find(|d| d.index == index)
            .ok_or_else(|| Error::InvalidArgument(format!("no wave packet descriptor {index}")))?;
        let bytes = self.inner.waveform_data(offset, size as usize)?;
        super::waveform::Waveform::decode(descriptor, &bytes)
    }

    /// The bytes of the waveform data packet record (the 60-byte record header, then the
    /// packets), for LAS files that hold waveforms. Empty for other files.
    pub fn waveform_record(&mut self) -> Result<Vec<u8>> {
        let size = self.inner.header().waveform_data_size;
        if size == 0 {
            return Ok(Vec::new());
        }
        self.inner.waveform_data(0, super::las::EVLR_HEADER_LEN + size as usize)
    }
}

/// The chunks of points of a [`PointCloudReader`]: see [`PointCloudReader::chunks`].
pub struct PointChunks<'a> {
    reader: &'a mut PointCloudReader,
}

impl Iterator for PointChunks<'_> {
    type Item = Result<PointCloud>;

    fn next(&mut self) -> Option<Self::Item> {
        self.reader.next_chunk().transpose()
    }
}

/// A LAS, LAZ or COPC file.
pub(crate) struct LasReader {
    source: Source,
    header: LasHeader,
    layout: Layout,
    copc: Option<Copc>,
    crs: LasCrs,
    cursor: LasCursor,
    /// The `.wdp` file next to the file, for external waveforms.
    wdp: Option<std::path::PathBuf>,
    /// Keeps a downloaded or extracted copy alive.
    _local: Option<crate::vfs::LocalPath>,
}

/// Where a chunked read of a LAS, LAZ or COPC file is.
enum LasCursor {
    None,
    Sequential {
        /// The LASzip decompressor, which holds the source while it reads.
        laz: Option<Box<laz::LasZipDecompressor<'static, Source>>>,
        done: u64,
        stride: u64,
        filter: PointFilter,
        chunk: usize,
    },
    Copc {
        nodes: Vec<CopcNode>,
        next: usize,
        filter: PointFilter,
        chunk: usize,
    },
}

impl LasReader {
    fn open(path: &str) -> Result<Self> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(remote) = crate::vfs::Location::parse(path).remote() {
            let mut http = crate::http::RangeReader::open(&remote)?;
            let header = LasHeader::read(&mut http)?;
            if Copc::is_copc(&header) {
                return LasReader::new(Source::Http(http), header, None, None);
            }
            // A LAS or LAZ file is read whole: download it.
        }
        let (mut source, local) = Source::open_local(path)?;
        let header = LasHeader::read(&mut source)?;
        let wdp = local.path().with_extension("wdp");
        let wdp = (header.global_encoding & super::las::WAVEFORM_EXTERNAL_BIT != 0 && wdp.exists()).then_some(wdp);
        LasReader::new(source, header, wdp, Some(local))
    }

    fn new(mut source: Source, header: LasHeader, wdp: Option<std::path::PathBuf>, local: Option<crate::vfs::LocalPath>) -> Result<Self> {
        let copc = if Copc::is_copc(&header) { Some(Copc::open(&mut source, &header)?) } else { None };
        let layout = Layout::new(&header);
        let crs = super::crs::header_crs(&header);
        Ok(Self { source, header, layout, copc, crs, cursor: LasCursor::None, wdp, _local: local })
    }

    /// Ends a chunked read: gives the source back from the LASzip decompressor.
    fn finish_chunks(&mut self) {
        if let LasCursor::Sequential { laz: Some(d), .. } = std::mem::replace(&mut self.cursor, LasCursor::None) {
            self.source = d.into_inner();
        }
    }

    fn read_las(&mut self, options: &PointReadOptions) -> Result<Vec<Dimension>> {
        let h = &self.header;
        let n = h.point_count;
        let rl = h.point_record_length as usize;
        let stride = stride_for(n, options.max_points);
        let capacity = if options.filter.is_empty() { (n / stride) as usize } else { CHUNK_POINTS };
        let mut columns = Columns::new(&self.layout, capacity.min(1 << 28));
        self.source.seek(SeekFrom::Start(h.offset_to_point_data as u64))?;
        let mut buf = vec![0u8; CHUNK_POINTS * rl];
        let mut records = if h.compressed { Records::Laz(laz_decompressor(&mut self.source, h)?) } else { Records::Raw(&mut self.source) };
        let (scale, offset, format) = (h.scale, h.offset, h.point_format);
        let mut done = 0u64;
        while done < n {
            let k = ((n - done) as usize).min(CHUNK_POINTS);
            let bytes = &mut buf[..k * rl];
            records.fill(bytes).map_err(|e| Error::InvalidData(format!("point {done} of {n}: {e}")))?;
            decode_records(bytes, rl, done, stride, &options.filter, format, &scale, &offset, &mut columns);
            done += k as u64;
        }
        Ok(columns.into_dimensions())
    }
}

/// Decodes records into columns, keeping every `stride`-th point (counting from `first`) that
/// passes a filter.
#[allow(clippy::too_many_arguments)]
fn decode_records(
    bytes: &[u8],
    rl: usize,
    first: u64,
    stride: u64,
    filter: &PointFilter,
    format: u8,
    scale: &[f64; 3],
    offset: &[f64; 3],
    columns: &mut Columns,
) {
    for (j, rec) in bytes.chunks_exact(rl).enumerate() {
        if stride > 1 && !(first + j as u64).is_multiple_of(stride) {
            continue;
        }
        if !filter.is_empty() && !filter.keeps(&record_core(rec, format, scale, offset)) {
            continue;
        }
        columns.push(rec, scale, offset);
    }
}

impl FormatReader for LasReader {
    fn header(&self) -> &LasHeader {
        &self.header
    }

    fn crs(&self) -> &LasCrs {
        &self.crs
    }

    fn dimensions(&self) -> Vec<(String, ValueType)> {
        self.layout.fields.iter().map(|f| (f.name.clone(), f.value_type)).collect()
    }

    fn format_name(&self) -> &'static str {
        if self.copc.is_some() {
            "COPC"
        } else if self.header.compressed {
            "LAZ"
        } else {
            "LAS"
        }
    }

    fn read(&mut self, options: &PointReadOptions) -> Result<Vec<Dimension>> {
        self.finish_chunks();
        match &mut self.copc {
            Some(copc) => copc.read(&mut self.source, &self.header, &self.layout, options),
            None => self.read_las(options),
        }
    }

    fn cloud_header(&mut self) -> Result<LasHeader> {
        let mut h = self.header.clone();
        // Points that refer to waveform data packets in the file carry them along, so that
        // writing them preserves their waveforms.
        let waves = self.layout.fields.iter().any(|f| f.name == "wavepacket_index");
        if waves && h.waveform_data_size > 0 && h.global_encoding & super::las::WAVEFORM_EXTERNAL_BIT == 0 {
            let data = self.waveform_data(super::las::EVLR_HEADER_LEN as u64, h.waveform_data_size as usize)?;
            if let Some(v) = h.evlrs.iter_mut().find(|v| v.is_waveform_data()) {
                v.data = data;
            }
        }
        Ok(h)
    }

    fn start(&mut self, options: &PointReadOptions, chunk: usize) -> Result<()> {
        self.finish_chunks();
        if let Some(copc) = &mut self.copc {
            let nodes = copc.select(&mut self.source, options)?;
            self.cursor = LasCursor::Copc { nodes, next: 0, filter: options.filter.clone(), chunk };
            return Ok(());
        }
        let h = &self.header;
        self.source.seek(SeekFrom::Start(h.offset_to_point_data as u64))?;
        let laz = if h.compressed {
            let vlr = laz_vlr(
                &h.find_vlr(super::las::LASZIP_USER_ID, super::las::LASZIP_RECORD_ID)
                    .ok_or_else(|| Error::InvalidData("the points are compressed but there is no LASzip record".into()))?
                    .data,
            )?;
            let source = std::mem::replace(&mut self.source, Source::placeholder());
            Some(Box::new(laz::LasZipDecompressor::new(source, vlr).map_err(|e| Error::InvalidData(format!("LAZ: {e}")))?))
        } else {
            None
        };
        self.cursor = LasCursor::Sequential {
            laz,
            done: 0,
            stride: stride_for(h.point_count, options.max_points),
            filter: options.filter.clone(),
            chunk,
        };
        Ok(())
    }

    fn next_chunk(&mut self) -> Result<Option<Vec<Dimension>>> {
        let h = &self.header;
        let out = match &mut self.cursor {
            LasCursor::None => None,
            LasCursor::Sequential { laz, done, stride, filter, chunk } => {
                let n = h.point_count;
                let rl = h.point_record_length as usize;
                let mut columns = Columns::new(&self.layout, (*chunk).min(1 << 24));
                let mut buf = vec![0u8; CHUNK_POINTS.min(*chunk) * rl];
                // Decode until the chunk is full (of points that pass the filter) or the file
                // ends.
                while *done < n && columns.len() < *chunk {
                    let k = ((n - *done) as usize).min(CHUNK_POINTS).min(*chunk);
                    let bytes = &mut buf[..k * rl];
                    let filled = match laz {
                        Some(d) => Records::Laz2(d).fill(bytes),
                        None => Records::Raw(&mut self.source).fill(bytes),
                    };
                    filled.map_err(|e| Error::InvalidData(format!("point {done} of {n}: {e}")))?;
                    decode_records(bytes, rl, *done, *stride, filter, h.point_format, &h.scale, &h.offset, &mut columns);
                    *done += k as u64;
                }
                (columns.len() > 0 || *done < n).then(|| columns.into_dimensions())
            }
            LasCursor::Copc { nodes, next, filter, chunk } => {
                if *next >= nodes.len() {
                    None
                } else {
                    let mut j = *next;
                    let mut count = 0usize;
                    while j < nodes.len() && (j == *next || count + nodes[j].point_count.max(0) as usize <= *chunk) {
                        count += nodes[j].point_count.max(0) as usize;
                        j += 1;
                    }
                    let mut columns = Columns::new(&self.layout, count);
                    let copc = self.copc.as_ref().ok_or_else(|| Error::InvalidData("not a COPC file".into()))?;
                    copc.decode(&mut self.source, h, &self.layout, &nodes[*next..j], filter, &mut columns)?;
                    *next = j;
                    Some(columns.into_dimensions())
                }
            }
        };
        if out.is_none() {
            self.finish_chunks();
        }
        Ok(out)
    }

    fn copc_info(&self) -> Option<&CopcInfo> {
        self.copc.as_ref().map(|c| &c.info)
    }

    fn copc_nodes(&mut self) -> Result<Vec<CopcNode>> {
        self.finish_chunks();
        match &mut self.copc {
            Some(c) => c.all_nodes(&mut self.source),
            None => Ok(Vec::new()),
        }
    }

    fn waveform_file(&self) -> Option<&std::path::Path> {
        self.wdp.as_deref()
    }

    fn waveform_data(&mut self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if len as u64 > 1 << 34 {
            return Err(Error::InvalidData(format!("waveform data of {len} bytes")));
        }
        if self.header.global_encoding & super::las::WAVEFORM_EXTERNAL_BIT != 0 {
            let path = self.wdp.as_ref().ok_or_else(|| Error::InvalidData("the waveforms are in a .wdp file, which is missing".into()))?;
            let mut f = std::fs::File::open(path)?;
            f.seek(SeekFrom::Start(offset))?;
            let mut b = vec![0u8; len];
            f.read_exact(&mut b)?;
            return Ok(b);
        }
        if self.header.waveform_data_size == 0 || self.header.start_of_waveform_data == 0 {
            return Err(Error::InvalidData("the file has no waveform data packets".into()));
        }
        self.finish_chunks();
        self.source.read_at(self.header.start_of_waveform_data + offset, len)
    }
}

/// The point records of a LAS or LAZ file, read in order.
enum Records<'a> {
    Raw(&'a mut Source),
    Laz(laz::LasZipDecompressor<'a, &'a mut Source>),
    Laz2(&'a mut laz::LasZipDecompressor<'static, Source>),
}

impl Records<'_> {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<()> {
        // `laz` panics on some corrupt data: report it as an error.
        let laz = |r: std::thread::Result<std::io::Result<()>>| match r {
            Ok(r) => r.map_err(|e| Error::InvalidData(format!("LAZ: {e}"))),
            Err(_) => Err(Error::InvalidData("the LAZ data could not be decompressed".into())),
        };
        match self {
            Records::Raw(s) => s.read_exact(bytes).map_err(|e| Error::InvalidData(format!("the point records end early: {e}"))),
            Records::Laz(d) => laz(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.decompress_many(bytes)))),
            Records::Laz2(d) => laz(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.decompress_many(bytes)))),
        }
    }
}

/// The LASzip decompressor of a LAZ file's points; the source is at the start of the points.
fn laz_decompressor<'a>(source: &'a mut Source, h: &LasHeader) -> Result<laz::LasZipDecompressor<'a, &'a mut Source>> {
    let vlr = h
        .find_vlr(super::las::LASZIP_USER_ID, super::las::LASZIP_RECORD_ID)
        .ok_or_else(|| Error::InvalidData("the points are compressed but there is no LASzip record".into()))?;
    laz::LasZipDecompressor::new(source, laz_vlr(&vlr.data)?).map_err(|e| Error::InvalidData(format!("LAZ: {e}")))
}

/// Parses a LASzip record. Files written by LASzip's first, unchunked ("pointwise") compressor
/// have a chunk size of 0, which `laz` reads as variable-size chunks with a chunk table they do
/// not have (and panics): give them a fixed chunk size, which the pointwise decompressor ignores.
pub(crate) fn laz_vlr(data: &[u8]) -> Result<laz::LazVlr> {
    let mut data = data.to_vec();
    let compressor = data.get(0..2).map(|b| u16::from_le_bytes([b[0], b[1]]));
    if compressor == Some(1) && data.len() >= 16 {
        data[12..16].copy_from_slice(&(u32::MAX - 1).to_le_bytes());
    }
    laz::LazVlr::from_buffer(&data).map_err(|e| Error::InvalidData(format!("LASzip record: {e}")))
}

/// A header for points of a format other than LAS: the count, the bounds, the smallest LAS 1.4
/// point format that holds the dimensions, and a scale and offset for the coordinates (the given
/// ones, or 0.001, or 1e-7 for geographic CRSs, with the bounds' minimum rounded down).
pub(crate) fn made_up_header(
    dims: &[(String, ValueType)],
    point_count: u64,
    bounds: Option<[f64; 6]>,
    crs: &SpatialRef,
    scale_offset: Option<([f64; 3], [f64; 3])>,
) -> LasHeader {
    let probe: Vec<Dimension> = dims.iter().map(|(n, t)| Dimension::new(n, Values::with_capacity(*t, 0))).collect();
    let mut probe = PointCloud::new(probe, crs.clone());
    probe.header.point_format = super::write::format_for(&probe, 6, false);
    let mut h = probe.header;
    h.point_count = point_count;
    h.system_identifier = String::new();
    let b = bounds.unwrap_or([0.0; 6]);
    h.min = [b[0], b[1], b[2]];
    h.max = [b[3], b[4], b[5]];
    match scale_offset {
        Some((s, o)) => {
            h.scale = s;
            h.offset = o;
        }
        None => {
            let s = if crs.is_geographic() { 1e-7 } else { 0.001 };
            h.scale = [s, s, 0.001];
            h.offset = [super::round_offset(b[0], s), super::round_offset(b[1], s), super::round_offset(b[2], 0.001)];
        }
    }
    h.point_record_length = super::las::standard_record_length(h.point_format) as u16;
    h
}

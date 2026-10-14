//! Reading LAS, LAZ and COPC files.

use super::copc::{Copc, CopcInfo, CopcNode};
use super::las::{Columns, LasHeader, Layout, record_core};
use super::{PointCloud, PointReadOptions};
use crate::crs::SpatialRef;
use crate::{Error, Result};
use std::io::{Cursor, Read, Seek, SeekFrom};

/// The points decoded at a time.
const CHUNK_POINTS: usize = 65_536;

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
}

/// An open point cloud file: its header is read, its points are read on demand.
pub struct PointCloudReader {
    source: Source,
    header: LasHeader,
    layout: Layout,
    copc: Option<Copc>,
    crs: super::crs::LasCrs,
    name: String,
    /// Keeps a downloaded or extracted copy alive.
    _local: Option<crate::vfs::LocalPath>,
}

impl std::fmt::Debug for PointCloudReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PointCloudReader").field("name", &self.name).field("header", &self.header).finish_non_exhaustive()
    }
}

/// Opens a LAS, LAZ or COPC file: a path, URL, `s3://` object or zip archive member. Only the
/// header is read. Remote COPC files are read in place, with HTTP range requests; other remote
/// files are downloaded first.
pub fn open_points(path: &str) -> Result<PointCloudReader> {
    let location = crate::vfs::Location::parse(path);
    let name = location.file_name();
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(remote) = location.remote() {
        let mut http = crate::http::RangeReader::open(&remote)?;
        let header = LasHeader::read(&mut http)?;
        if Copc::is_copc(&header) {
            return PointCloudReader::new(Source::Http(http), header, name, None);
        }
        // A LAS or LAZ file is read whole: download it.
        let local = crate::vfs::resolve(path)?;
        let mut source = Source::File(std::io::BufReader::new(std::fs::File::open(local.path())?));
        let header = LasHeader::read(&mut source)?;
        return PointCloudReader::new(source, header, name, Some(local));
    }
    let local = crate::vfs::resolve(path)?;
    let file = std::fs::File::open(local.path()).map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("'{path}': {e}"))))?;
    let mut source = Source::File(std::io::BufReader::with_capacity(1 << 20, file));
    let header = LasHeader::read(&mut source)?;
    PointCloudReader::new(source, header, name, Some(local))
}

/// Opens a LAS, LAZ or COPC file held in memory.
pub fn open_points_bytes(bytes: Vec<u8>, name: &str) -> Result<PointCloudReader> {
    let mut source = Source::Memory(Cursor::new(bytes));
    let header = LasHeader::read(&mut source)?;
    PointCloudReader::new(source, header, name.to_string(), None)
}

impl PointCloudReader {
    fn new(mut source: Source, header: LasHeader, name: String, local: Option<crate::vfs::LocalPath>) -> Result<Self> {
        let copc = if Copc::is_copc(&header) { Some(Copc::open(&mut source, &header)?) } else { None };
        let layout = Layout::new(&header);
        let crs = super::crs::header_crs(&header);
        Ok(Self { source, header, layout, copc, crs, name, _local: local })
    }

    /// The header.
    pub fn header(&self) -> &LasHeader {
        &self.header
    }

    /// The file name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The format: `COPC`, `LAZ` or `LAS`.
    pub fn format_name(&self) -> &'static str {
        if self.copc.is_some() {
            "COPC"
        } else if self.header.compressed {
            "LAZ"
        } else {
            "LAS"
        }
    }

    /// The horizontal CRS.
    pub fn crs(&self) -> &SpatialRef {
        &self.crs.horizontal
    }

    /// The EPSG code and name of the vertical CRS, when the file declares one.
    pub fn vertical_crs(&self) -> (Option<u32>, Option<String>) {
        (self.crs.vertical_epsg, self.crs.vertical_name.clone())
    }

    /// The dimensions of the points: `(name, type)`.
    pub fn dimensions(&self) -> Vec<(String, super::ValueType)> {
        self.layout.fields.iter().map(|f| (f.name.clone(), f.value_type)).collect()
    }

    /// The COPC information record, for COPC files.
    pub fn copc_info(&self) -> Option<&CopcInfo> {
        self.copc.as_ref().map(|c| &c.info)
    }

    /// The nodes of a COPC file's octree (loading every hierarchy page). Empty for other files.
    pub fn copc_nodes(&mut self) -> Result<Vec<CopcNode>> {
        match &mut self.copc {
            Some(c) => c.all_nodes(&mut self.source),
            None => Ok(Vec::new()),
        }
    }

    /// Reads the points.
    pub fn read(&mut self, options: &PointReadOptions) -> Result<PointCloud> {
        let dimensions = match &mut self.copc {
            Some(copc) => copc.read(&mut self.source, &self.header, &self.layout, options)?,
            None => self.read_las(options)?,
        };
        Ok(PointCloud { header: self.header.clone(), crs: self.crs.horizontal.clone(), dimensions })
    }

    fn read_las(&mut self, options: &PointReadOptions) -> Result<Vec<super::Dimension>> {
        let h = &self.header;
        let n = h.point_count;
        let rl = h.point_record_length as usize;
        let stride = match options.max_points {
            Some(m) if m > 0 && n > m => n.div_ceil(m),
            _ => 1,
        };
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
            records.fill(bytes).map_err(|e| Error::InvalidData(format!("{}: point {done} of {n}: {e}", self.name)))?;
            for (j, rec) in bytes.chunks_exact(rl).enumerate() {
                if stride > 1 && !(done + j as u64).is_multiple_of(stride) {
                    continue;
                }
                if !options.filter.is_empty() && !options.filter.keeps(&record_core(rec, format, &scale, &offset)) {
                    continue;
                }
                columns.push(rec, &scale, &offset);
            }
            done += k as u64;
        }
        Ok(columns.into_dimensions())
    }
}

/// The point records of a LAS or LAZ file, read in order.
enum Records<'a> {
    Raw(&'a mut Source),
    Laz(laz::LasZipDecompressor<'a, &'a mut Source>),
}

impl Records<'_> {
    fn fill(&mut self, bytes: &mut [u8]) -> Result<()> {
        match self {
            Records::Raw(s) => s.read_exact(bytes).map_err(|e| Error::InvalidData(format!("the point records end early: {e}"))),
            Records::Laz(d) => {
                // `laz` panics on some corrupt data: report it as an error.
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| d.decompress_many(bytes))) {
                    Ok(r) => r.map_err(|e| Error::InvalidData(format!("LAZ: {e}"))),
                    Err(_) => Err(Error::InvalidData("the LAZ data could not be decompressed".into())),
                }
            }
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

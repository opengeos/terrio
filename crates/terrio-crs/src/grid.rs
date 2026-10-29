//! Datum shift and geoid grids, read and interpolated as PROJ (<https://proj.org>, MIT licence)
//! does: NTv2 (`.gsb`), GTX (`.gtx`) and the GeoTIFF grids of PROJ-data (`.tif`, from
//! <https://cdn.proj.org>), with their subgrids. PROJ's notice is in `NOTICE`.
//!
//! Grids are looked up by name in, in order: grids registered in memory ([`register_grid`]), the
//! directories of `TERRIO_GRID_DIR`, `PROJ_DATA` and `PROJ_LIB`, PROJ's user directory (where
//! `projsync` writes), and terrio's download cache. When `TERRIO_NETWORK` (or, when it is not set,
//! `PROJ_NETWORK`) is `ON`, a grid found nowhere is downloaded from the PROJ CDN
//! (`TERRIO_CDN_ENDPOINT`, `PROJ_NETWORK_ENDPOINT`, default `https://cdn.proj.org`): read in place
//! with HTTP range requests, as PROJ reads it, fetching only the header and the blocks of the
//! points transformed; or, with `TERRIO_GRID_DOWNLOAD=YES`, downloaded whole into the cache
//! (`TERRIO_GRID_CACHE`, default `~/.cache/terrio/grids`), where later runs find it.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::{
    Arc, Mutex, OnceLock, PoisonError,
    atomic::{AtomicU64, Ordering},
};

const DEG_TO_RAD: f64 = std::f64::consts::PI / 180.0;
const SEC_TO_RAD: f64 = DEG_TO_RAD / 3600.0;
/// PROJ's relative tolerance at grid edges.
const REL_TOLERANCE: f64 = 1e-5;
const TWO_PI: f64 = 2.0 * std::f64::consts::PI;

/// The area a grid covers and its resolution, in radians (geographic grids) or metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Extent {
    pub west: f64,
    pub south: f64,
    pub east: f64,
    pub north: f64,
    pub res_x: f64,
    pub res_y: f64,
    pub geographic: bool,
}

impl Extent {
    fn world() -> Self {
        Self {
            west: -std::f64::consts::PI,
            south: -std::f64::consts::FRAC_PI_2,
            east: std::f64::consts::PI,
            north: std::f64::consts::FRAC_PI_2,
            res_x: std::f64::consts::PI,
            res_y: std::f64::consts::FRAC_PI_2,
            geographic: true,
        }
    }

    fn full_world_longitude(&self) -> bool {
        self.geographic && self.east - self.west + self.res_x >= TWO_PI - 1e-10
    }

    fn contains(&self, o: &Extent) -> bool {
        o.west >= self.west && o.east <= self.east && o.south >= self.south && o.north <= self.north
    }

    /// Whether a point is in the extent, as PROJ's `isPointInExtent`.
    fn holds(&self, mut x: f64, y: f64, eps: f64) -> bool {
        if !(y + eps >= self.south && y - eps <= self.north) {
            return false;
        }
        if self.full_world_longitude() {
            return true;
        }
        if self.geographic {
            if x + eps < self.west {
                x += TWO_PI;
            } else if x - eps > self.east {
                x -= TWO_PI;
            }
        }
        x + eps >= self.west && x - eps <= self.east
    }
}

/// The values of one grid (an NTv2 subfile, a GTX file or a GeoTIFF image).
enum Values {
    /// Samples in memory, each row-major from the southern row, west to east.
    Mem(Vec<Vec<f32>>),
    /// A GeoTIFF image, decoded by block on first use.
    Tiff(TiffImage),
    /// PROJ's `null` grid: zero everywhere.
    Null,
}

/// A GeoTIFF image whose blocks are decoded when first read.
struct TiffImage {
    file: Arc<TiffFile>,
    ifd: usize,
    block_w: usize,
    block_h: usize,
    blocks_per_row: usize,
    blocks: usize,
    planar: bool,
    bottom_up: bool,
    /// Decoded blocks, by block id (planes after one another when planar), each decoded when
    /// first read: reading one afterwards takes no lock, as many threads read them at once.
    cache: Vec<OnceLock<Block>>,
}

/// A decoded block: its data width and raw values.
type Block = (usize, Vec<f64>);

/// Where a GeoTIFF grid's bytes are read from: memory, or a remote file read by range.
enum GridReader {
    Bytes(Cursor<Arc<[u8]>>),
    #[cfg(remote)]
    Remote(Box<dyn crate::network::GridStream>),
}

impl std::io::Read for GridReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            GridReader::Bytes(c) => c.read(buf),
            #[cfg(remote)]
            GridReader::Remote(r) => r.read(buf),
        }
    }
}

impl std::io::Seek for GridReader {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        match self {
            GridReader::Bytes(c) => c.seek(pos),
            #[cfg(remote)]
            GridReader::Remote(r) => r.seek(pos),
        }
    }
}

/// A TIFF decoder over a grid file.
type TiffDecoder = tiff::decoder::Decoder<GridReader>;

struct TiffFile {
    /// The decoder and the image it is on.
    decoder: Mutex<(TiffDecoder, usize)>,
}

/// One grid with its metadata.
struct Source {
    values: Values,
    width: usize,
    height: usize,
    samples: usize,
    /// GDAL metadata items by (sample, name); sample -1 for the dataset's.
    meta: HashMap<(i32, String), String>,
    nodata: Option<f32>,
    scale_offset: Vec<(f64, f64)>,
    /// The file's first grid, whose metadata a grid without `TYPE` inherits (generic grids).
    first: Option<Arc<Source>>,
    subfile_type: u32,
    format: Format,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Format {
    Ntv2,
    Gtx,
    Tiff,
    Null,
}

impl Source {
    fn meta(&self, key: &str, sample: i32) -> &str {
        match self.meta.get(&(sample, key.to_string())) {
            Some(v) => v,
            None => self.first.as_ref().map(|f| f.meta(key, sample)).unwrap_or(""),
        }
    }

    /// The value of a sample at a column and a row counted from the south, scaled as GDAL's
    /// metadata says unless it is nodata (PROJ's `GTiffGrid::valueAt`).
    fn value(&self, sample: usize, x: usize, y: usize) -> Option<f32> {
        if x >= self.width || y >= self.height || sample >= self.samples {
            return None;
        }
        match &self.values {
            Values::Null => Some(0.0),
            Values::Mem(v) => v.get(sample)?.get(y * self.width + x).copied(),
            Values::Tiff(img) => {
                let row = if img.bottom_up { y } else { self.height - 1 - y };
                let (bx, ox) = (x / img.block_w, x % img.block_w);
                let (by, oy) = (row / img.block_h, row % img.block_h);
                let mut id = (by * img.blocks_per_row + bx) as u32;
                if img.planar {
                    id += (sample * img.blocks) as u32;
                }
                let block = img.block(id)?;
                let (w, data) = (&block.0, &block.1);
                let mut off = ox + oy * w;
                if !img.planar {
                    off = off * self.samples + sample;
                }
                let raw = *data.get(off)?;
                match self.scale_offset.get(sample) {
                    Some(&(scale, offset)) if self.nodata != Some(raw as f32) => Some((raw * scale + offset) as f32),
                    _ => Some(raw as f32),
                }
            }
        }
    }

    fn is_nodata(&self, v: f32) -> bool {
        self.nodata == Some(v) || v.is_nan()
    }
}

impl TiffImage {
    fn block(&self, id: u32) -> Option<&Block> {
        let slot = self.cache.get(id as usize)?;
        if let Some(b) = slot.get() {
            return Some(b);
        }
        let decoded = {
            let mut guard = self.file.decoder.lock().unwrap_or_else(PoisonError::into_inner);
            let (dec, current) = &mut *guard;
            if *current != self.ifd {
                dec.seek_to_image(self.ifd).ok()?;
                *current = self.ifd;
            }
            let (w, _) = dec.chunk_data_dimensions(id);
            let values = match dec.read_chunk(id).ok()? {
                tiff::decoder::DecodingResult::F32(v) => v.into_iter().map(f64::from).collect(),
                tiff::decoder::DecodingResult::F64(v) => v,
                tiff::decoder::DecodingResult::I16(v) => v.into_iter().map(f64::from).collect(),
                tiff::decoder::DecodingResult::U16(v) => v.into_iter().map(f64::from).collect(),
                tiff::decoder::DecodingResult::I32(v) => v.into_iter().map(f64::from).collect(),
                tiff::decoder::DecodingResult::U32(v) => v.into_iter().map(f64::from).collect(),
                _ => return None,
            };
            (w as usize, values)
        };
        // A block decoded by two threads at once keeps the first.
        Some(slot.get_or_init(|| decoded))
    }
}

/// How a grid is read by the step that uses it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Use {
    /// Horizontal shifts (`hgridshift`): the samples of the latitude and longitude offsets, the
    /// factor to radians, whether longitudes are positive east, and NTv2's west-positive
    /// longitudes.
    H { lat: usize, lon: usize, to_rad: f64, east: bool, ntv2: bool },
    /// Vertical offsets (`vgridshift`): the sample.
    V { sample: usize },
    /// Any other grid (`gridshift`, `xyzgridshift`, `deformation`).
    Generic,
}

/// A grid with its subgrids, as PROJ nests them.
#[derive(Clone)]
pub(crate) struct Grid {
    pub ext: Extent,
    src: Arc<Source>,
    usage: Use,
    children: Vec<Grid>,
}

impl Grid {
    fn width(&self) -> usize {
        self.src.width
    }

    fn height(&self) -> usize {
        self.src.height
    }

    fn is_null(&self) -> bool {
        self.src.format == Format::Null
    }

    /// The type of a generic grid (`HORIZONTAL_OFFSET`, `GEOGRAPHIC_3D_OFFSET`...).
    fn grid_type(&self) -> &str {
        self.src.meta("TYPE", -1)
    }

    /// The most refined grid holding a point.
    fn at(&self, x: f64, y: f64, horizontal: bool) -> &Grid {
        for c in &self.children {
            let eps = if horizontal { (c.ext.res_x + c.ext.res_y) * REL_TOLERANCE } else { 0.0 };
            if c.ext.holds(x, y, eps) {
                return c.at(x, y, horizontal);
            }
        }
        self
    }

    /// The horizontal shift (longitude, latitude) in radians at a node, as PROJ's `valueAt` of
    /// horizontal grids (single precision, longitudes positive east).
    fn hshift(&self, x: usize, y: usize) -> Option<(f32, f32)> {
        if self.is_null() {
            return Some((0.0, 0.0));
        }
        let Use::H { lat, lon, to_rad, east, ntv2 } = self.usage else { return None };
        let la = self.src.value(lat, x, y)?;
        let lo = self.src.value(lon, x, y)?;
        let la = (la as f64 * to_rad) as f32;
        let mut lo = (lo as f64 * to_rad) as f32;
        if ntv2 || !east {
            lo = -lo;
        }
        Some((lo, la))
    }
}

/// The grids of one file, as a step uses them.
pub(crate) struct GridSet {
    grids: Vec<Grid>,
}

impl GridSet {
    fn at(&self, x: f64, y: f64, horizontal: bool) -> Option<&Grid> {
        for g in &self.grids {
            if g.is_null() {
                return Some(g);
            }
            let eps = if horizontal { (g.ext.res_x + g.ext.res_y) * REL_TOLERANCE } else { 0.0 };
            if g.ext.holds(x, y, eps) {
                return Some(g.at(x, y, horizontal));
            }
        }
        None
    }

    fn at_type(&self, kind: &str, x: f64, y: f64) -> Option<&Grid> {
        for g in &self.grids {
            if g.is_null() {
                return Some(g);
            }
            if g.grid_type() != kind {
                continue;
            }
            if g.ext.holds(x, y, 0.0) {
                return Some(g.at(x, y, false));
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------------------------
// Reading grid files.

/// A grid file read: its format and grids in file order.
struct GridFile {
    /// Each grid's extent and values, with its name and its parent's name (NTv2 subfile names,
    /// or GDAL metadata's `grid_name` and `parent_grid_name`).
    sources: Vec<(Extent, Arc<Source>, String, String)>,
    format: Format,
}

fn null_file() -> GridFile {
    let src = Arc::new(Source {
        values: Values::Null,
        width: 3,
        height: 3,
        samples: 3,
        meta: HashMap::new(),
        nodata: None,
        scale_offset: Vec::new(),
        first: None,
        subfile_type: 0,
        format: Format::Null,
    });
    GridFile { sources: vec![(Extent::world(), src, String::new(), String::new())], format: Format::Null }
}

fn f64_at(b: &[u8], off: usize, le: bool) -> Option<f64> {
    let s: [u8; 8] = b.get(off..off + 8)?.try_into().ok()?;
    Some(if le { f64::from_le_bytes(s) } else { f64::from_be_bytes(s) })
}

fn i32_at(b: &[u8], off: usize, le: bool) -> Option<i32> {
    let s: [u8; 4] = b.get(off..off + 4)?.try_into().ok()?;
    Some(if le { i32::from_le_bytes(s) } else { i32::from_be_bytes(s) })
}

fn f32_at(b: &[u8], off: usize, le: bool) -> Option<f32> {
    let s: [u8; 4] = b.get(off..off + 4)?.try_into().ok()?;
    Some(if le { f32::from_le_bytes(s) } else { f32::from_be_bytes(s) })
}

/// Reads an NTv2 file (`GS_TYPE=SECONDS`), as PROJ's `NTv2GridSet::open`.
fn read_ntv2(b: &[u8]) -> Option<GridFile> {
    if b.get(56..63)? != b"SECONDS" {
        return None;
    }
    let le = b[8] == 11;
    let subfiles = i32_at(b, 40, le)?.max(0) as usize;
    let mut pos = 176;
    let mut out = Vec::new();
    for _ in 0..subfiles {
        let h = b.get(pos..pos + 176)?;
        if &h[..8] != b"SUB_NAME" {
            return None;
        }
        let name = String::from_utf8_lossy(&h[8..16]).to_string();
        let parent = String::from_utf8_lossy(&h[24..32]).to_string();
        let g = |i: usize| f64_at(h, 72 + 16 * i, le);
        let ext = Extent {
            south: g(0)? * DEG_TO_RAD / 3600.0,
            north: g(1)? * DEG_TO_RAD / 3600.0,
            east: -g(2)? * DEG_TO_RAD / 3600.0,
            west: -g(3)? * DEG_TO_RAD / 3600.0,
            res_y: g(4)? * DEG_TO_RAD / 3600.0,
            res_x: g(5)? * DEG_TO_RAD / 3600.0,
            geographic: true,
        };
        if !(ext.west < ext.east && ext.south < ext.north && ext.res_x > 1e-10 && ext.res_y > 1e-10) {
            return None;
        }
        let cols = ((ext.east - ext.west) / ext.res_x + 0.5).abs() as usize + 1;
        let rows = ((ext.north - ext.south) / ext.res_y + 0.5).abs() as usize + 1;
        let count = i32_at(h, 168, le)?.max(0) as usize;
        if count / cols != rows {
            return None;
        }
        let data = pos + 176;
        let (mut lat, mut lon) = (vec![0f32; cols * rows], vec![0f32; cols * rows]);
        for y in 0..rows {
            for x in 0..cols {
                // Rows run east to west.
                let off = data + 16 * (y * cols + (cols - 1 - x));
                lat[y * cols + x] = f32_at(b, off, le)?;
                lon[y * cols + x] = f32_at(b, off + 4, le)?;
            }
        }
        let src = Arc::new(Source {
            values: Values::Mem(vec![lat, lon]),
            width: cols,
            height: rows,
            samples: 2,
            meta: HashMap::new(),
            nodata: None,
            scale_offset: Vec::new(),
            first: None,
            subfile_type: 0,
            format: Format::Ntv2,
        });
        out.push((ext, src, name, parent));
        pos = data + count * 16;
    }
    Some(GridFile { sources: out, format: Format::Ntv2 })
}

/// Reads a GTX vertical grid, as PROJ's `GTXVerticalShiftGrid::open`.
fn read_gtx(b: &[u8]) -> Option<GridFile> {
    let (yo, mut xo, ys, xs) = (f64_at(b, 0, false)?, f64_at(b, 8, false)?, f64_at(b, 16, false)?, f64_at(b, 24, false)?);
    let (rows, cols) = (i32_at(b, 32, false)?, i32_at(b, 36, false)?);
    if cols <= 0 || rows <= 0 || !(-360.0..=360.0).contains(&xo) || !(-90.0..=90.0).contains(&yo) {
        return None;
    }
    if xo >= 180.0 {
        xo -= 360.0;
    }
    let (rows, cols) = (rows as usize, cols as usize);
    let ext = Extent {
        west: xo * DEG_TO_RAD,
        south: yo * DEG_TO_RAD,
        res_x: xs * DEG_TO_RAD,
        res_y: ys * DEG_TO_RAD,
        east: (xo + xs * (cols - 1) as f64) * DEG_TO_RAD,
        north: (yo + ys * (rows - 1) as f64) * DEG_TO_RAD,
        geographic: true,
    };
    let v: Option<Vec<f32>> = (0..rows * cols).map(|i| f32_at(b, 40 + 4 * i, false)).collect();
    let src = Arc::new(Source {
        values: Values::Mem(vec![v?]),
        width: cols,
        height: rows,
        samples: 1,
        meta: HashMap::new(),
        nodata: None,
        scale_offset: Vec::new(),
        first: None,
        subfile_type: 0,
        format: Format::Gtx,
    });
    Some(GridFile { sources: vec![(ext, src, String::new(), String::new())], format: Format::Gtx })
}

/// Parses the items of a `GDAL_METADATA` tag as PROJ does.
fn parse_gdal_metadata(text: &str) -> HashMap<(i32, String), String> {
    let mut out = HashMap::new();
    let mut rest = text;
    while let Some(start) = rest.find("<Item ") {
        let item = &rest[start..];
        let Some(end_tag) = item.find('>') else { break };
        let Some(end_value) = item[end_tag..].find('<').map(|i| i + end_tag) else { break };
        let tag = &item[..end_tag];
        let value = &item[end_tag + 1..end_value];
        let attr = |name: &str| {
            let p = tag.find(&format!("{name}=\""))? + name.len() + 2;
            let q = tag[p..].find('"')?;
            Some(tag[p..p + q].to_string())
        };
        let Some(name) = attr("name") else { break };
        let sample = attr("sample").and_then(|s| s.parse().ok()).unwrap_or(-1);
        let value = value.replace("&amp;", "&").replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"");
        out.insert((sample, name), value);
        rest = &item[end_value + 1..];
    }
    out
}

/// Reads the images of a PROJ GeoTIFF grid, as PROJ's `GTiffDataset::nextGrid`.
fn read_tiff(reader: GridReader) -> Option<GridFile> {
    use tiff::decoder::{Decoder, Limits};
    use tiff::tags::Tag;
    let mut dec = Decoder::new(reader).ok()?.with_limits(Limits::unlimited());
    let mut out = Vec::new();
    let mut infos = Vec::new();
    let mut ifd = 0usize;
    loop {
        let num = |dec: &mut TiffDecoder, t: Tag| dec.find_tag(t).ok().flatten().and_then(|v| v.into_u64().ok());
        let width = num(&mut dec, Tag::ImageWidth)? as usize;
        let height = num(&mut dec, Tag::ImageLength)? as usize;
        let samples = num(&mut dec, Tag::SamplesPerPixel).unwrap_or(1) as usize;
        let planar = num(&mut dec, Tag::PlanarConfiguration).unwrap_or(1) == 2 && samples > 1;
        let subfile_type = num(&mut dec, Tag::NewSubfileType).unwrap_or(0) as u32;
        let (block_w, block_h) = match num(&mut dec, Tag::TileWidth) {
            Some(tw) => (tw as usize, num(&mut dec, Tag::TileLength)? as usize),
            None => (width, (num(&mut dec, Tag::RowsPerStrip).unwrap_or(height as u64) as usize).min(height)),
        };
        let geokeys = dec.find_tag(Tag::GeoKeyDirectoryTag).ok().flatten().and_then(|v| v.into_u16_vec().ok()).unwrap_or_default();
        let mut geographic = true;
        let mut pixel_is_area = false;
        let mut i = 4;
        while i + 3 < geokeys.len() {
            match geokeys[i] {
                1024 if geokeys[i + 3] == 1 => geographic = false,
                1025 if geokeys[i + 3] == 1 => pixel_is_area = true,
                _ => {}
            }
            i += 4;
        }
        let f64s = |dec: &mut TiffDecoder, t: Tag| dec.find_tag(t).ok().flatten().and_then(|v| v.into_f64_vec().ok());
        let (mut west, h_res, mut north, v_res);
        if let Some(m) = f64s(&mut dec, Tag::ModelTransformationTag).filter(|m| m.len() == 16) {
            if m[1] != 0.0 || m[4] != 0.0 {
                return None;
            }
            west = m[3];
            h_res = m[0];
            north = m[7];
            v_res = -m[5];
        } else {
            let scale = f64s(&mut dec, Tag::ModelPixelScaleTag).filter(|s| s.len() == 3)?;
            let tie = f64s(&mut dec, Tag::ModelTiepointTag).filter(|t| t.len() == 6)?;
            h_res = scale[0];
            v_res = scale[1];
            west = tie[3] - tie[0] * h_res;
            north = tie[4] + tie[1] * v_res;
        }
        if pixel_is_area {
            west += 0.5 * h_res;
            north -= 0.5 * v_res;
        }
        let mul = if geographic { DEG_TO_RAD } else { 1.0 };
        let mut ext = Extent {
            west: west * mul,
            north: north * mul,
            res_x: h_res * mul,
            res_y: v_res.abs() * mul,
            east: (west + h_res * (width as f64 - 1.0)) * mul,
            south: (north - v_res * (height as f64 - 1.0)) * mul,
            geographic,
        };
        if v_res < 0.0 {
            std::mem::swap(&mut ext.north, &mut ext.south);
        }
        let text = dec.find_tag(Tag::Unknown(42112)).ok().flatten().and_then(|v| v.into_string().ok()).unwrap_or_default();
        let meta = parse_gdal_metadata(&text);
        let nodata = dec
            .find_tag(Tag::GdalNodata)
            .ok()
            .flatten()
            .and_then(|v| v.into_string().ok())
            .and_then(|s| s.trim_matches(char::from(0)).trim().parse::<f64>().ok())
            .map(|v| v as f32);
        let mut scale_offset = Vec::new();
        for smp in 0..samples {
            let (scale, offset) = (role_value(&text, smp as i32, "scale"), role_value(&text, smp as i32, "offset"));
            if scale.is_some() || offset.is_some() {
                scale_offset.resize(samples, (1.0, 0.0));
                scale_offset[smp] = (scale.unwrap_or(1.0), offset.unwrap_or(0.0));
            }
        }
        let blocks_per_row = width.div_ceil(block_w.max(1));
        let blocks = blocks_per_row * height.div_ceil(block_h.max(1));
        // The chunks the file has: no more blocks than these can be decoded, whatever its size
        // tags say.
        let offsets = if dec.get_chunk_type() == tiff::decoder::ChunkType::Tile { Tag::TileOffsets } else { Tag::StripOffsets };
        let chunks = dec.find_tag(offsets).ok().flatten().and_then(|v| v.into_u64_vec().ok()).map_or(0, |v| v.len());
        infos.push((
            ext,
            width,
            height,
            samples,
            planar,
            subfile_type,
            block_w,
            block_h,
            blocks_per_row,
            blocks,
            v_res < 0.0,
            meta,
            nodata,
            scale_offset,
            ifd,
            chunks,
        ));
        ifd += 1;
        if !dec.more_images() || dec.next_image().is_err() {
            break;
        }
    }
    let file = Arc::new(TiffFile { decoder: Mutex::new((dec, ifd.saturating_sub(1))) });
    let mut first: Option<Arc<Source>> = None;
    for (
        ext,
        width,
        height,
        samples,
        planar,
        subfile_type,
        block_w,
        block_h,
        blocks_per_row,
        blocks,
        bottom_up,
        meta,
        nodata,
        scale_offset,
        ifd,
        chunks,
    ) in infos
    {
        let grid_name = meta.get(&(-1, "grid_name".to_string())).cloned().unwrap_or_default();
        let parent = meta.get(&(-1, "parent_grid_name".to_string())).cloned().unwrap_or_default();
        let inherits = first
            .as_ref()
            .filter(|f| !meta.contains_key(&(-1, "TYPE".to_string())) && f.meta.contains_key(&(-1, "TYPE".to_string())))
            .cloned();
        let src = Arc::new(Source {
            values: Values::Tiff(TiffImage {
                file: file.clone(),
                ifd,
                block_w,
                block_h,
                blocks_per_row,
                blocks,
                planar,
                bottom_up,
                cache: (0..(if planar { blocks.saturating_mul(samples) } else { blocks }).min(chunks)).map(|_| OnceLock::new()).collect(),
            }),
            width,
            height,
            samples,
            meta,
            nodata,
            scale_offset,
            first: inherits,
            subfile_type,
            format: Format::Tiff,
        });
        if first.is_none() {
            first = Some(src.clone());
        }
        out.push((ext, src, grid_name, parent));
    }
    Some(GridFile { sources: out, format: Format::Tiff })
}

/// The value of the `GDAL_METADATA` item of a sample with a role (`scale`, `offset`).
fn role_value(text: &str, sample: i32, role: &str) -> Option<f64> {
    let mut rest = text;
    while let Some(start) = rest.find("<Item ") {
        let item = &rest[start..];
        let end_tag = item.find('>')?;
        let end_value = item[end_tag..].find('<')? + end_tag;
        let tag = &item[..end_tag];
        if tag.contains(&format!("sample=\"{sample}\"")) && tag.contains(&format!("role=\"{role}\"")) {
            return item[end_tag + 1..end_value].trim().parse().ok();
        }
        rest = &item[end_value + 1..];
    }
    None
}

fn read_file(name: &str, bytes: Arc<[u8]>) -> Option<GridFile> {
    let b: &[u8] = &bytes;
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".gtx") {
        return read_gtx(b);
    }
    if b.len() >= 55 && &b[..8] == b"NUM_OREC" && &b[48..55] == b"GS_TYPE" {
        return read_ntv2(b);
    }
    if b.len() >= 4 && (&b[..4] == b"II*\0" || &b[..4] == b"MM\0*" || &b[..4] == b"II+\0" || &b[..4] == b"MM\0+") {
        return read_tiff(GridReader::Bytes(Cursor::new(bytes)));
    }
    None
}

// ---------------------------------------------------------------------------------------------
// Finding grids.

/// The search paths and network setting set by the API, overriding the environment.
#[derive(Default)]
struct Config {
    paths: Option<Vec<std::path::PathBuf>>,
    network: Option<bool>,
}

fn config() -> &'static Mutex<Config> {
    crate::host::before_lock();
    static C: OnceLock<Mutex<Config>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(Config::default()))
}

/// Sets the directories datum shift and geoid grids are looked up in, in order, instead of those
/// of the environment (`TERRIO_GRID_DIR`, `PROJ_DATA`, `PROJ_LIB`, PROJ's user directory and
/// terrio's download cache); `None` goes back to the environment's. As PROJ's
/// `proj_context_set_search_paths`. Transformers created afterwards use them.
pub fn set_grid_search_paths(paths: Option<Vec<std::path::PathBuf>>) {
    {
        let mut c = config().lock().unwrap_or_else(PoisonError::into_inner);
        c.paths = paths;
    }
    reset_grid_cache();
}

/// Enables or disables downloading grids from the PROJ CDN, instead of following the
/// `TERRIO_NETWORK` (or `PROJ_NETWORK`) environment variable; `None` goes back to it. As PROJ's
/// `proj_context_set_enable_network`.
pub fn set_network_enabled(enabled: Option<bool>) {
    {
        let mut c = config().lock().unwrap_or_else(PoisonError::into_inner);
        c.network = enabled;
    }
    reset_grid_cache();
}

/// Grids registered in memory, by name.
fn registered() -> &'static Mutex<HashMap<String, Arc<[u8]>>> {
    crate::host::before_lock();
    static R: OnceLock<Mutex<HashMap<String, Arc<[u8]>>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashMap::new()))
}

// Resets invalidate in-flight cache writes as well as entries already stored.
static CACHE_GENERATION: AtomicU64 = AtomicU64::new(0);

fn cache_generation() -> u64 {
    CACHE_GENERATION.load(Ordering::SeqCst)
}

/// Files read, by name (`None` when not found or not readable) and cache generation.
type FileCache = Mutex<HashMap<String, (u64, Option<Arc<GridFile>>)>>;
fn files() -> &'static FileCache {
    crate::host::before_lock();
    static F: OnceLock<FileCache> = OnceLock::new();
    F.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A late lookup must not overwrite a result cached after a reset. Check while
/// holding the cache lock so reset either invalidates this write or clears it.
fn cache_file_result(name: &str, generation: u64, file: Option<Arc<GridFile>>) {
    let mut cache = files().lock().unwrap_or_else(PoisonError::into_inner);
    if generation == cache_generation() {
        cache.insert(name.to_string(), (generation, file));
    }
}

/// Makes a grid file available under a name (such as `us_noaa_conus.tif`), from its bytes: for
/// WebAssembly, where there is no file system, or to use a grid without installing it.
pub fn register_grid(name: &str, bytes: Vec<u8>) {
    {
        let mut r = registered().lock().unwrap_or_else(PoisonError::into_inner);
        r.insert(name.to_string(), Arc::from(bytes));
    }
    {
        let mut f = files().lock().unwrap_or_else(PoisonError::into_inner);
        f.remove(name);
    }
    {
        let mut a = availability().lock().unwrap_or_else(PoisonError::into_inner);
        a.clear();
    }
}

/// Old names of PROJ-data grids (before PROJ 7), by current name, from the EPSG table.
fn old_names(name: &str) -> Vec<String> {
    super::epsg::grid_alternatives(name)
}

/// Whether network access to the PROJ CDN is enabled.
pub(crate) fn network_enabled() -> bool {
    if let Some(n) = config().lock().unwrap_or_else(PoisonError::into_inner).network {
        return n;
    }
    let on = |v: String| matches!(v.trim().to_ascii_uppercase().as_str(), "ON" | "YES" | "TRUE" | "1");
    #[cfg(not(target_family = "wasm"))]
    {
        if let Some(v) = crate::host::config_any(&["TERRIO_NETWORK", "PROJ_NETWORK"]) {
            return on(v);
        }
    }
    let _ = on;
    false
}

#[cfg(not(target_family = "wasm"))]
fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(Into::into)
}

/// The directory downloaded grids are kept in.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn cache_dir() -> Option<std::path::PathBuf> {
    if let Some(d) = std::env::var_os("TERRIO_GRID_CACHE") {
        return Some(d.into());
    }
    if let Some(d) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(std::path::PathBuf::from(d).join("terrio").join("grids"));
    }
    if cfg!(windows)
        && let Some(d) = std::env::var_os("LOCALAPPDATA")
    {
        return Some(std::path::PathBuf::from(d).join("terrio").join("grids"));
    }
    home().map(|h| h.join(".cache").join("terrio").join("grids"))
}

/// The directories grids are looked up in.
#[cfg(not(target_family = "wasm"))]
fn search_dirs() -> Vec<std::path::PathBuf> {
    if let Some(p) = config().lock().unwrap_or_else(PoisonError::into_inner).paths.clone() {
        return p;
    }
    // Unit tests do not see the grids installed on the machine (PROJ's directories, terrio's
    // download cache), which change the transformations PROJ and terrio pick: their results are
    // those of a machine without grids. Tests needing grids set the search paths or register them.
    if cfg!(test) {
        std::env::var_os("TERRIO_GRID_DIR").map(|v| std::env::split_paths(&v).collect()).unwrap_or_default()
    } else {
        machine_search_dirs()
    }
}

/// The directories grids are looked up in by default: those of `TERRIO_GRID_DIR`, `PROJ_DATA`
/// and `PROJ_LIB`, PROJ's user directory and terrio's download cache.
#[cfg(not(target_family = "wasm"))]
fn machine_search_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = Vec::new();
    for var in ["TERRIO_GRID_DIR", "PROJ_DATA", "PROJ_LIB"] {
        if let Some(v) = std::env::var_os(var) {
            dirs.extend(std::env::split_paths(&v));
        }
    }
    // PROJ's user writable directory, where projsync and PROJ's network cache put grids.
    if let Some(d) = std::env::var_os("XDG_DATA_HOME") {
        dirs.push(std::path::PathBuf::from(d).join("proj"));
    } else if cfg!(windows) {
        if let Some(d) = std::env::var_os("LOCALAPPDATA") {
            dirs.push(std::path::PathBuf::from(d).join("proj"));
        }
    } else if let Some(h) = home() {
        dirs.push(if cfg!(target_os = "macos") { h.join("Library/Application Support/proj") } else { h.join(".local/share/proj") });
    }
    dirs.extend(cache_dir());
    dirs
}

/// The bytes of a grid file, by name or path, from memory or the local directories.
fn find_local(name: &str) -> Option<Arc<[u8]>> {
    if let Some(b) = registered().lock().unwrap_or_else(PoisonError::into_inner).get(name) {
        return Some(b.clone());
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let p = std::path::Path::new(name);
        if p.is_absolute() || name.starts_with("./") || name.starts_with("../") {
            return std::fs::read(p).ok().map(Arc::from);
        }
        for d in search_dirs() {
            let f = d.join(name);
            if f.is_file() {
                return std::fs::read(f).ok().map(Arc::from);
            }
        }
    }
    None
}

/// Whether a grid file is present locally (by its name or an old name).
fn present_locally(name: &str) -> bool {
    if registered().lock().unwrap_or_else(PoisonError::into_inner).contains_key(name) {
        return true;
    }
    #[cfg(not(target_family = "wasm"))]
    {
        let p = std::path::Path::new(name);
        if p.is_absolute() {
            return p.is_file();
        }
        let dirs = search_dirs();
        let mut names = vec![name.to_string()];
        names.extend(old_names(name));
        return names.iter().any(|n| dirs.iter().any(|d| d.join(n).is_file()));
    }
    #[allow(unreachable_code)]
    false
}

/// The URL of a grid on the PROJ CDN.
#[cfg(remote)]
fn cdn_url(name: &str) -> Option<String> {
    if name.contains(['/', '\\']) || !name.ends_with(".tif") {
        return None;
    }
    let endpoint =
        crate::host::config_any(&["TERRIO_CDN_ENDPOINT", "PROJ_NETWORK_ENDPOINT"]).unwrap_or_else(|| "https://cdn.proj.org".into());
    Some(format!("{}/{name}", endpoint.trim_end_matches('/')))
}

/// Whether grids from the CDN are downloaded whole (`TERRIO_GRID_DOWNLOAD=YES`) rather than read
/// by range.
#[cfg(remote)]
fn download_whole() -> bool {
    std::env::var("TERRIO_GRID_DOWNLOAD").is_ok_and(|v| matches!(v.trim().to_ascii_uppercase().as_str(), "YES" | "ON" | "TRUE" | "1"))
}

/// A GeoTIFF grid on the PROJ CDN, read in place by HTTP range requests. `Err(true)` when the
/// CDN does not have it (status 404), `Err(false)` when it could not be read in place.
#[cfg(remote)]
fn open_remote(name: &str) -> Result<GridFile, bool> {
    let url = cdn_url(name).ok_or(true)?;
    let reader = crate::network::open(&url).map_err(|e| e.kind() == std::io::ErrorKind::NotFound)?;
    read_tiff(GridReader::Remote(reader)).ok_or(false)
}

#[cfg(remote)]
fn download(name: &str) -> Option<Arc<[u8]>> {
    let url = cdn_url(name)?;
    let dir = cache_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let tmp = dir.join(format!("{name}.part{}", std::process::id()));
    if crate::network::download(&url, &tmp).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return None;
    }
    let path = dir.join(name);
    std::fs::rename(&tmp, &path).ok()?;
    std::fs::read(path).ok().map(Arc::from)
}

/// The grid file of a name, read once.
fn load(name: &str) -> Option<Arc<GridFile>> {
    if name == "null" {
        static NULL: OnceLock<Arc<GridFile>> = OnceLock::new();
        return Some(NULL.get_or_init(|| Arc::new(null_file())).clone());
    }
    let generation = cache_generation();
    if let Some((_, f)) = files().lock().unwrap_or_else(PoisonError::into_inner).get(name).filter(|(g, _)| *g == generation) {
        return f.clone();
    }
    let bytes = find_local(name);
    if bytes.is_none() {
        for old in old_names(name) {
            if let Some(b) = find_local(&old) {
                let file = read_file(&old, b).map(Arc::new);
                cache_file_result(name, generation, file.clone());
                return file;
            }
        }
    }
    // Read in place; a grid that cannot be read so (a server without range requests, a failed
    // request) is downloaded instead, unless the CDN does not have it.
    #[cfg(remote)]
    if bytes.is_none() && network_enabled() && !download_whole() {
        match open_remote(name) {
            Ok(file) => {
                let file = Arc::new(file);
                cache_file_result(name, generation, Some(file.clone()));
                return Some(file);
            }
            Err(true) => {
                fetch_failures().lock().unwrap_or_else(PoisonError::into_inner).insert(name.to_string());
                cache_file_result(name, generation, None);
                return None;
            }
            Err(false) => {}
        }
    }
    #[cfg(remote)]
    let bytes = match bytes {
        None if network_enabled() => {
            let got = download(name);
            if got.is_none() {
                fetch_failures().lock().unwrap_or_else(PoisonError::into_inner).insert(name.to_string());
            }
            got
        }
        b => b,
    };
    let file = bytes.and_then(|b| read_file(name, b)).map(Arc::new);
    cache_file_result(name, generation, file.clone());
    file
}

/// The grids the network was asked for and did not give (the CDN does not have them, or the
/// request failed).
fn fetch_failures() -> &'static Mutex<HashSet<String>> {
    crate::host::before_lock();
    static F: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    F.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Whether a grid was asked for on the network and not obtained: an operation needing it fails
/// without another taking its place, as PROJ's `proj_trans` stops on a network error (with the
/// network on and the grid missing on the CDN, PROJ 9.7.1 and 9.8.1 fail the point).
pub(crate) fn fetch_failed(name: &str) -> bool {
    fetch_failures().lock().unwrap_or_else(PoisonError::into_inner).contains(name)
}

fn availability() -> &'static Mutex<HashMap<String, (u64, bool)>> {
    crate::host::before_lock();
    static A: OnceLock<Mutex<HashMap<String, (u64, bool)>>> = OnceLock::new();
    A.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Holds the grids' process-wide locks (settings, grids registered in memory, files read, failed
/// fetches, availability, grid sets built) across a fork, for [`crate::host::hold_for_fork`]: the forking thread
/// takes them before the process is copied and lets them go after, so that the child never starts
/// with one held by a thread it does not have.
#[cfg(all(unix, not(target_family = "wasm")))]
pub(crate) fn hold_for_fork(held: &mut Vec<Box<dyn std::any::Any>>) {
    held.push(Box::new(config().lock().unwrap_or_else(PoisonError::into_inner)));
    held.push(Box::new(registered().lock().unwrap_or_else(PoisonError::into_inner)));
    held.push(Box::new(files().lock().unwrap_or_else(PoisonError::into_inner)));
    held.push(Box::new(fetch_failures().lock().unwrap_or_else(PoisonError::into_inner)));
    held.push(Box::new(availability().lock().unwrap_or_else(PoisonError::into_inner)));
    held.push(Box::new(sets_cache().lock().unwrap_or_else(PoisonError::into_inner)));
}

/// Whether a grid can be used: present locally, or (with network access enabled) on the PROJ
/// CDN. This is what makes PROJ consider an operation needing it.
pub(crate) fn available(name: &str) -> bool {
    if name == "null" {
        return true;
    }
    let generation = cache_generation();
    if let Some((_, a)) = availability().lock().unwrap_or_else(PoisonError::into_inner).get(name).copied().filter(|(g, _)| *g == generation)
    {
        return a;
    }
    let a = present_locally(name) || (network_enabled() && name.ends_with(".tif") && !name.contains(['/', '\\']));
    {
        let mut m = availability().lock().unwrap_or_else(PoisonError::into_inner);
        if generation == cache_generation() {
            m.insert(name.to_string(), (generation, a));
        }
    }
    a
}

/// Forgets which grids were found, so that the next lookups search again (after the grid
/// directories or network setting change).
pub fn reset_grid_cache() {
    CACHE_GENERATION.fetch_add(1, Ordering::SeqCst);
    {
        let mut a = availability().lock().unwrap_or_else(PoisonError::into_inner);
        a.clear();
    }
    {
        let mut f = files().lock().unwrap_or_else(PoisonError::into_inner);
        f.clear();
    }
    fetch_failures().lock().unwrap_or_else(PoisonError::into_inner).clear();
    sets_cache().lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// The names of a step's `grids=` list, with whether each is optional (`@` prefix).
pub(crate) fn grid_names(spec: &str) -> Vec<(String, bool)> {
    spec.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| match s.strip_prefix('@') {
            Some(n) => (n.to_string(), true),
            None => (s.to_string(), false),
        })
        .collect()
}

/// Builds the nesting of a file's grids as PROJ's `insertIntoHierarchy` (and, for NTv2, its
/// parent names) does.
fn hierarchy(file: &GridFile, keep: impl Fn(&Source, usize) -> Option<Use>) -> Vec<Grid> {
    // Grids in an arena: each with its children's indices.
    let mut nodes: Vec<(Grid, Vec<usize>)> = Vec::new();
    let mut top: Vec<usize> = Vec::new();
    let mut named: HashMap<String, usize> = HashMap::new();
    // Inserts a grid under node `at` or one of its descendants whose extent holds it.
    fn insert_under(nodes: &mut Vec<(Grid, Vec<usize>)>, at: usize, new: usize) {
        let ext = nodes[new].0.ext;
        let children = nodes[at].1.clone();
        for c in children {
            if nodes[c].0.ext.contains(&ext) {
                insert_under(nodes, c, new);
                return;
            }
        }
        nodes[at].1.push(new);
    }
    for (i, (ext, src, name, parent)) in file.sources.iter().enumerate() {
        if file.format == Format::Tiff && src.subfile_type != 0 && src.subfile_type != 2 {
            continue;
        }
        let Some(usage) = keep(src, i) else { continue };
        let id = nodes.len();
        nodes.push((Grid { ext: *ext, src: src.clone(), usage, children: Vec::new() }, Vec::new()));
        if file.format == Format::Ntv2 {
            match named.get(parent) {
                Some(&p) => nodes[p].1.push(id),
                None => top.push(id),
            }
            named.insert(name.clone(), id);
            continue;
        }
        if !name.is_empty() {
            named.insert(name.clone(), id);
        }
        if !parent.is_empty() {
            if let Some(&p) = named.get(parent)
                && nodes[p].0.ext.contains(ext)
            {
                nodes[p].1.push(id);
                continue;
            }
        } else if !name.is_empty() {
            top.push(id);
            continue;
        }
        // By extents.
        let kind = src.meta("TYPE", -1).to_string();
        let parent =
            top.iter().copied().find(|&t| (kind.is_empty() || nodes[t].0.src.meta("TYPE", -1) == kind) && nodes[t].0.ext.contains(ext));
        match parent {
            Some(t) => insert_under(&mut nodes, t, id),
            None => top.push(id),
        }
    }
    fn build(nodes: &[(Grid, Vec<usize>)], i: usize) -> Grid {
        let mut g = nodes[i].0.clone();
        g.children = nodes[i].1.iter().map(|&c| build(nodes, c)).collect();
        g
    }
    top.iter().map(|&t| build(&nodes, t)).collect()
}

/// Grid sets built, by grid name and use.
type SetsCache = Mutex<HashMap<(String, char), (u64, Arc<GridSet>)>>;

fn sets_cache() -> &'static SetsCache {
    crate::host::before_lock();
    static CACHE: OnceLock<SetsCache> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn sets<F>(spec: &str, kind: char, build: F) -> Option<Vec<Arc<GridSet>>>
where
    F: Fn(&GridFile) -> Option<Vec<Grid>>,
{
    let cache = sets_cache();
    let mut out = Vec::new();
    for (name, optional) in grid_names(spec) {
        let generation = cache_generation();
        if let Some((_, s)) =
            cache.lock().unwrap_or_else(PoisonError::into_inner).get(&(name.clone(), kind)).filter(|(g, _)| *g == generation)
        {
            out.push(s.clone());
            continue;
        }
        match load(&name).and_then(|f| build(&f)).filter(|g| !g.is_empty()) {
            Some(grids) => {
                let s = Arc::new(GridSet { grids });
                let mut cache = cache.lock().unwrap_or_else(PoisonError::into_inner);
                if generation == cache_generation() {
                    cache.insert((name, kind), (generation, s.clone()));
                }
                out.push(s);
            }
            None if optional => {}
            None => return None,
        }
    }
    Some(out)
}

/// The horizontal shift grids of a `grids=` list (PROJ's `pj_hgrid_init`), `None` when a
/// required one is missing.
pub(crate) fn hgrids(spec: &str) -> Option<Vec<Arc<GridSet>>> {
    sets(spec, 'h', |f| {
        if f.format == Format::Null {
            return Some(hierarchy(f, |_, _| Some(Use::Generic)));
        }
        if f.format == Format::Ntv2 {
            return Some(hierarchy(f, |_, _| Some(Use::H { lat: 0, lon: 1, to_rad: SEC_TO_RAD, east: true, ntv2: true })));
        }
        if f.format != Format::Tiff {
            return None;
        }
        // Defaults carry over from one image to the next, as in PROJ.
        let state = std::cell::Cell::new((0usize, 1usize, SEC_TO_RAD, true));
        let ok = std::cell::Cell::new(true);
        let grids = hierarchy(f, |src, ifd| {
            if src.samples < 2 {
                if ifd == 0 {
                    ok.set(false);
                }
                return None;
            }
            let (mut lat, mut lon, mut to_rad, mut east) = state.get();
            let (mut any, mut found_lat, mut found_lon) = (false, false, false);
            for i in 0..src.samples {
                let d = src.meta("DESCRIPTION", i as i32);
                any |= !d.is_empty();
                if d == "latitude_offset" {
                    lat = i;
                    found_lat = true;
                } else if d == "longitude_offset" {
                    lon = i;
                    found_lon = true;
                }
            }
            if any && !found_lat && !found_lon {
                if ifd == 0 {
                    ok.set(false);
                }
                return None;
            }
            if found_lat != found_lon {
                ok.set(false);
                return None;
            }
            if found_lon {
                match src.meta("positive_value", lon as i32) {
                    "west" => east = false,
                    "east" => east = true,
                    "" => {}
                    _ => {
                        ok.set(false);
                        return None;
                    }
                }
            }
            let (ul, uo) = (src.meta("UNITTYPE", lat as i32), src.meta("UNITTYPE", lon as i32));
            if ul != uo {
                ok.set(false);
                return None;
            }
            match ul {
                "arc-second" | "arc-seconds per year" => to_rad = SEC_TO_RAD,
                "radian" => to_rad = 1.0,
                "degree" => to_rad = DEG_TO_RAD,
                "" => {}
                _ => {
                    ok.set(false);
                    return None;
                }
            }
            state.set((lat, lon, to_rad, east));
            Some(Use::H { lat, lon, to_rad, east, ntv2: false })
        });
        ok.get().then_some(grids)
    })
}

/// The vertical shift grids of a `grids=` list (PROJ's `pj_vgrid_init`).
pub(crate) fn vgrids(spec: &str) -> Option<Vec<Arc<GridSet>>> {
    sets(spec, 'v', |f| match f.format {
        Format::Null => Some(hierarchy(f, |_, _| Some(Use::V { sample: 0 }))),
        Format::Gtx => Some(hierarchy(f, |_, _| Some(Use::V { sample: 0 }))),
        Format::Tiff => {
            let ok = std::cell::Cell::new(true);
            let sample = std::cell::Cell::new(0usize);
            let grids = hierarchy(f, |src, ifd| {
                let mut any = false;
                let mut found = false;
                for i in 0..src.samples {
                    let d = src.meta("DESCRIPTION", i as i32);
                    any |= !d.is_empty();
                    if matches!(d, "geoid_undulation" | "vertical_offset" | "hydroid_height" | "ellipsoidal_height_offset") {
                        sample.set(i);
                        found = true;
                    }
                }
                if any && !found {
                    if ifd == 0 {
                        ok.set(false);
                    }
                    return None;
                }
                (sample.get() < src.samples).then_some(Use::V { sample: sample.get() })
            });
            ok.get().then_some(grids)
        }
        Format::Ntv2 => None,
    })
}

/// The generic grids of a `grids=` list (PROJ's `pj_generic_grid_init`).
pub(crate) fn generic_grids(spec: &str) -> Option<Vec<Arc<GridSet>>> {
    sets(spec, 'g', |f| match f.format {
        Format::Tiff | Format::Null => Some(hierarchy(f, |_, _| Some(Use::Generic))),
        _ => None,
    })
}

// ---------------------------------------------------------------------------------------------
// Interpolation, after PROJ's grids.cpp and transformations/*.cpp.

fn find_hgrid(sets: &[Arc<GridSet>], lon: f64, lat: f64) -> Option<(&Grid, &GridSet)> {
    sets.iter().find_map(|s| s.at(lon, lat, true).map(|g| (g, s.as_ref())))
}

/// PROJ's `pj_hgrid_interpolate`: the shift at a point relative to the grid's origin.
fn hgrid_interpolate(t: (f64, f64), g: &Grid) -> Option<(f64, f64)> {
    let ext = &g.ext;
    let (tl, tp) = (t.0 / ext.res_x, t.1 / ext.res_y);
    let mut il = if tl.is_nan() { 0 } else { tl.floor() as i64 };
    let mut ip = if tp.is_nan() { 0 } else { tp.floor() as i64 };
    let (mut fl, mut fp) = (tl - il as f64, tp - ip as f64);
    let (w, h) = (g.width() as i64, g.height() as i64);
    if il < 0 {
        if il == -1 && fl > 1.0 - 10.0 * REL_TOLERANCE {
            il += 1;
            fl = 0.0;
        } else {
            return None;
        }
    } else if il + 1 >= w {
        if il + 1 == w && fl < 10.0 * REL_TOLERANCE {
            il -= 1;
            fl = 1.0;
        } else {
            return None;
        }
    }
    if ip < 0 {
        if ip == -1 && fp > 1.0 - 10.0 * REL_TOLERANCE {
            ip += 1;
            fp = 0.0;
        } else {
            return None;
        }
    } else if ip + 1 >= h {
        if ip + 1 == h && fp < 10.0 * REL_TOLERANCE {
            ip -= 1;
            fp = 1.0;
        } else {
            return None;
        }
    }
    if il < 0 || ip < 0 {
        return None;
    }
    let (x, y) = (il as usize, ip as usize);
    let f00 = g.hshift(x, y)?;
    let f10 = g.hshift(x + 1, y)?;
    let f01 = g.hshift(x, y + 1)?;
    let f11 = g.hshift(x + 1, y + 1)?;
    let mut m10 = fl;
    let mut m11 = m10;
    let mut m01 = 1.0 - fl;
    let mut m00 = m01;
    m11 *= fp;
    m01 *= fp;
    let fp = 1.0 - fp;
    m00 *= fp;
    m10 *= fp;
    let lam = m00 * f00.0 as f64 + m10 * f10.0 as f64 + m01 * f01.0 as f64 + m11 * f11.0 as f64;
    let phi = m00 * f00.1 as f64 + m10 * f10.1 as f64 + m01 * f01.1 as f64 + m11 * f11.1 as f64;
    Some((lam, phi))
}

fn adjlon(mut lon: f64) -> f64 {
    if lon.abs() < std::f64::consts::PI + 1e-12 {
        return lon;
    }
    lon += std::f64::consts::PI;
    lon -= TWO_PI * (lon / TWO_PI).floor();
    lon - std::f64::consts::PI
}

/// Applies horizontal shift grids to a point in radians (PROJ's `pj_hgrid_apply`). `None`
/// outside the grids.
pub(crate) fn hgrid_apply(sets: &[Arc<GridSet>], lon: f64, lat: f64, inverse: bool) -> Option<(f64, f64)> {
    const MAX_ITERATIONS: usize = 10;
    const TOL: f64 = 1e-12;
    let (mut grid, _) = find_hgrid(sets, lon, lat)?;
    if grid.is_null() {
        return Some((lon, lat));
    }
    let normalize = |g: &Grid| {
        let ext = &g.ext;
        let eps = (ext.res_x + ext.res_y) * REL_TOLERANCE;
        let mut l = lon - ext.west;
        if l + eps < 0.0 {
            l += TWO_PI;
        } else if l - eps > ext.east - ext.west {
            l -= TWO_PI;
        }
        (l, lat - ext.south)
    };
    let mut tb = normalize(grid);
    let t = hgrid_interpolate(tb, grid)?;
    if !inverse {
        return Some((lon + t.0, lat + t.1));
    }
    let mut t = (tb.0 - t.0, tb.1 - t.1);
    let mut i = MAX_ITERATIONS;
    let toltol = TOL * TOL;
    let mut del: Option<(f64, f64)>;
    loop {
        del = hgrid_interpolate(t, grid);
        let (dl, dp) = match del {
            Some(d) => d,
            None => {
                // The iteration can leave the grid: try the grid holding the new guess.
                let lp = (t.0 + grid.ext.west, t.1 + grid.ext.south);
                match find_hgrid(sets, lp.0, lp.1) {
                    Some((g, _)) if !std::ptr::eq(g, grid) && !g.is_null() => {
                        grid = g;
                        t = (lp.0 - grid.ext.west, lp.1 - grid.ext.south);
                        tb = normalize(grid);
                        i -= 1;
                        if i == 0 {
                            return None;
                        }
                        continue;
                    }
                    _ => break,
                }
            }
        };
        let dif = (t.0 + dl - tb.0, t.1 + dp - tb.1);
        t = (t.0 - dif.0, t.1 - dif.1);
        i -= 1;
        if i == 0 || dif.0 * dif.0 + dif.1 * dif.1 <= toltol {
            break;
        }
    }
    if i == 0 {
        return None;
    }
    Some((adjlon(t.0 + grid.ext.west), t.1 + grid.ext.south))
}

/// The vertical offset at a point in radians, times `multiplier` (PROJ's `read_vgrid_value`).
pub(crate) fn vgrid_value(sets: &[Arc<GridSet>], lon: f64, lat: f64, multiplier: f64) -> Option<f64> {
    if lon.is_nan() || lat.is_nan() {
        return None;
    }
    let g = sets.iter().find_map(|s| s.at(lon, lat, false))?;
    if g.is_null() {
        return Some(0.0);
    }
    let ext = &g.ext;
    if !ext.geographic {
        return None;
    }
    let Use::V { sample } = g.usage else { return None };
    let (w, h) = (g.width() as f64, g.height() as f64);
    let mut gx = (lon - ext.west) / ext.res_x;
    if lon < ext.west {
        gx = if ext.full_world_longitude() { ((gx + w) % w + w) % w } else { (lon + TWO_PI - ext.west) / ext.res_x };
    } else if lon > ext.east {
        gx = if ext.full_world_longitude() { ((gx + w) % w + w) % w } else { (lon - TWO_PI - ext.west) / ext.res_x };
    }
    let mut gy = (lat - ext.south) / ext.res_y;
    let ix = gx.floor() as i64;
    if ix < 0 || ix >= w as i64 {
        return None;
    }
    let iy = gy.floor() as i64;
    if iy < 0 || iy >= h as i64 {
        return None;
    }
    gx -= ix as f64;
    gy -= iy as f64;
    let mut ix2 = ix + 1;
    if ix2 >= w as i64 {
        ix2 = if ext.full_world_longitude() { 0 } else { w as i64 - 1 };
    }
    let iy2 = (iy + 1).min(h as i64 - 1);
    let val = |x: i64, y: i64| g.src.value(sample, x as usize, y as usize);
    let (a, b, c, d) = (val(ix, iy)?, val(ix2, iy)?, val(ix, iy2)?, val(ix2, iy2)?);
    let nodata = |v: f32| match g.src.format {
        Format::Gtx => v as f64 * multiplier > 1000.0 || (v as f64 * multiplier) < -1000.0 || v == -88.8888f32,
        _ => g.src.is_nodata(v),
    };
    let gxy = gx * gy;
    let weights = [(a, 1.0 - gx - gy + gxy), (b, gx - gxy), (c, gy - gxy), (d, gxy)];
    let valid: Vec<(f32, f64)> = weights.iter().copied().filter(|(v, _)| !nodata(*v)).collect();
    let value = match valid.len() {
        0 => return None,
        4 => weights.iter().map(|(v, wt)| *v as f64 * wt).sum::<f64>(),
        _ => {
            let total: f64 = valid.iter().map(|(_, wt)| wt).sum();
            valid.iter().map(|(v, wt)| *v as f64 * wt).sum::<f64>() / total
        }
    };
    Some(value * multiplier)
}

/// The grid of a generic grid list holding a point (PROJ's `pj_find_generic_grid`).
fn find_generic(sets: &[Arc<GridSet>], lon: f64, lat: f64) -> Option<&Grid> {
    sets.iter().find_map(|s| s.at(lon, lat, false))
}

/// The bilinear interpolation of three samples (PROJ's
/// `pj_bilinear_interpolation_three_samples`), for `xyzgridshift` and `deformation`.
/// `names` gives the samples' descriptions, with the default sample of each.
pub(crate) fn bilinear3(sets: &[Arc<GridSet>], lon: f64, lat: f64, names: [&str; 3], unit: &str) -> Option<[f64; 3]> {
    let g = find_generic(sets, lon, lat)?;
    if g.is_null() {
        return Some([0.0; 3]);
    }
    let src = &g.src;
    if src.samples < 3 {
        return None;
    }
    let mut idx = [0usize, 1, 2];
    for i in 0..src.samples {
        let d = src.meta("DESCRIPTION", i as i32);
        for k in 0..3 {
            if d == names[k] {
                idx[k] = i;
            }
        }
    }
    let u = src.meta("UNITTYPE", idx[0] as i32);
    if !u.is_empty() && u != unit {
        return None;
    }
    let ext = &g.ext;
    if !ext.geographic {
        return None;
    }
    let mut gx = (lon - ext.west) / ext.res_x;
    if lon < ext.west {
        gx = (lon + TWO_PI - ext.west) / ext.res_x;
    } else if lon > ext.east {
        gx = (lon - TWO_PI - ext.west) / ext.res_x;
    }
    let gy = (lat - ext.south) / ext.res_y;
    let (ix, iy) = (gx as i64, gy as i64);
    if ix < 0 || iy < 0 {
        return None;
    }
    let ix2 = (ix + 1).min(g.width() as i64 - 1);
    let iy2 = (iy + 1).min(g.height() as i64 - 1);
    let v = |x: i64, y: i64, k: usize| src.value(idx[k], x as usize, y as usize);
    let fl = gx - ix as f64;
    let fp = gy - iy as f64;
    let mut m10 = fl;
    let mut m11 = m10;
    let mut m01 = 1.0 - fl;
    let mut m00 = m01;
    m11 *= fp;
    m01 *= fp;
    let fp = 1.0 - fp;
    m00 *= fp;
    m10 *= fp;
    let mut out = [0.0; 3];
    for (k, o) in out.iter_mut().enumerate() {
        *o = m00 * v(ix, iy, k)? as f64 + m10 * v(ix2, iy, k)? as f64 + m01 * v(ix, iy2, k)? as f64 + m11 * v(ix2, iy2, k)? as f64;
    }
    Some(out)
}

/// What a `gridshift` step's grids hold, from their types (PROJ's `checkGridTypes`).
#[derive(Debug, Clone)]
pub(crate) struct GridShiftKinds {
    main: &'static str,
    aux: Option<&'static str>,
    has_horizontal: bool,
    has_geog3d: bool,
    offset: (f64, f64),
}

/// The kinds of a `gridshift` step's grids, `None` for an unsupported mix.
pub(crate) fn gridshift_kinds(sets: &[Arc<GridSet>]) -> Option<GridShiftKinds> {
    let (mut h, mut g3, mut eh, mut vv, mut gv) = (false, false, false, false, false);
    let mut count = 0;
    let (mut ox, mut oy) = (String::new(), String::new());
    fn walk<'a>(g: &'a Grid, out: &mut Vec<&'a Grid>) {
        out.push(g);
        for c in &g.children {
            walk(c, out);
        }
    }
    for s in sets {
        let mut all = Vec::new();
        for g in &s.grids {
            walk(g, &mut all);
        }
        for g in all {
            if g.is_null() {
                continue;
            }
            count += 1;
            match g.grid_type() {
                "HORIZONTAL_OFFSET" => {
                    h = true;
                    if ox.is_empty() {
                        ox = g.src.meta("constant_offset", 0).to_string();
                    }
                    if oy.is_empty() {
                        oy = g.src.meta("constant_offset", 1).to_string();
                    }
                }
                "GEOGRAPHIC_3D_OFFSET" => g3 = true,
                "ELLIPSOIDAL_HEIGHT_OFFSET" => eh = true,
                "VERTICAL_OFFSET_VERTICAL_TO_VERTICAL" => vv = true,
                "VERTICAL_OFFSET_GEOGRAPHIC_TO_VERTICAL" => gv = true,
                _ => return None,
            }
        }
    }
    let mut offset = (0.0, 0.0);
    if !ox.is_empty() || !oy.is_empty() {
        if count > 1 {
            return None;
        }
        offset = (ox.parse().ok()?, oy.parse().ok()?);
    }
    if (eh as u8 + vv as u8 + gv as u8) > 1 {
        return None;
    }
    let main = if g3 {
        "GEOGRAPHIC_3D_OFFSET"
    } else if !h {
        if eh {
            "ELLIPSOIDAL_HEIGHT_OFFSET"
        } else if gv {
            "VERTICAL_OFFSET_GEOGRAPHIC_TO_VERTICAL"
        } else {
            "VERTICAL_OFFSET_VERTICAL_TO_VERTICAL"
        }
    } else {
        "HORIZONTAL_OFFSET"
    };
    let aux = if h {
        if eh {
            Some("ELLIPSOIDAL_HEIGHT_OFFSET")
        } else if gv {
            Some("VERTICAL_OFFSET_GEOGRAPHIC_TO_VERTICAL")
        } else if vv {
            Some("VERTICAL_OFFSET_VERTICAL_TO_VERTICAL")
        } else {
            None
        }
    } else {
        None
    };
    Some(GridShiftKinds { main, aux, has_horizontal: h, has_geog3d: g3, offset })
}

/// PROJ's `gridshiftData::grid_interpolate`: the (x, y, z) shift at a point, and whether the
/// interpolation was biquadratic.
fn gridshift_interpolate(kind: &str, x: f64, y: f64, g: &Grid, interpolation: Option<&str>, skip_z: bool) -> Option<([f64; 3], bool)> {
    let src = &g.src;
    let projected = !g.ext.geographic;
    let (mut sx, mut sy, mut sz): (i64, i64, i64) = (-1, -1, -1);
    let mut en_offset = false;
    for i in 0..src.samples {
        let d = src.meta("DESCRIPTION", i as i32);
        let unit = src.meta("UNITTYPE", i as i32);
        match d {
            "latitude_offset" if !projected => {
                if !unit.is_empty() && unit != "arc-second" {
                    return None;
                }
                sy = i as i64;
            }
            "longitude_offset" if !projected => {
                if !unit.is_empty() && unit != "arc-second" {
                    return None;
                }
                sx = i as i64;
            }
            "easting_offset" if projected => {
                if !unit.is_empty() && unit != "metre" {
                    return None;
                }
                en_offset = true;
                sx = i as i64;
            }
            "northing_offset" if projected => {
                if !unit.is_empty() && unit != "metre" {
                    return None;
                }
                en_offset = true;
                sy = i as i64;
            }
            "ellipsoidal_height_offset" | "geoid_undulation" | "hydroid_height" | "vertical_offset" => {
                if !unit.is_empty() && unit != "metre" {
                    return None;
                }
                sz = i as i64;
            }
            _ => {}
        }
    }
    if src.samples >= 2 && sy < 0 && sx < 0 && kind == "HORIZONTAL_OFFSET" {
        if projected {
            en_offset = true;
            sx = 0;
            sy = 1;
        } else {
            sx = 1;
            sy = 0;
        }
    }
    if (kind == "HORIZONTAL_OFFSET" || kind == "GEOGRAPHIC_3D_OFFSET") && (sy < 0 || sx < 0) {
        return None;
    }
    if matches!(
        kind,
        "ELLIPSOIDAL_HEIGHT_OFFSET"
            | "VERTICAL_OFFSET_GEOGRAPHIC_TO_VERTICAL"
            | "VERTICAL_OFFSET_VERTICAL_TO_VERTICAL"
            | "GEOGRAPHIC_3D_OFFSET"
    ) && sz < 0
    {
        return None;
    }
    let method = interpolation.map(str::to_string).unwrap_or_else(|| {
        let m = src.meta("interpolation_method", -1);
        if m.is_empty() { "bilinear".into() } else { m.to_string() }
    });
    if method != "bilinear" && method != "biquadratic" {
        return None;
    }
    let bilinear = method == "bilinear" || g.width() < 3 || g.height() < 3;
    let sz = if skip_z { -1 } else { sz };
    let ext = &g.ext;
    let fx = (x - ext.west) / ext.res_x;
    let mut ix = if fx.is_nan() { 0 } else { fx.floor() as i64 };
    let fy = (y - ext.south) / ext.res_y;
    let mut iy = if fy.is_nan() { 0 } else { fy.floor() as i64 };
    let (mut frx, mut fry) = (fx - ix as f64, fy - iy as f64);
    let (w, h) = (g.width() as i64, g.height() as i64);
    if ix < 0 {
        if ix == -1 && frx > 1.0 - 10.0 * REL_TOLERANCE {
            ix += 1;
            frx = 0.0;
        } else {
            return None;
        }
    } else if ix + 1 >= w {
        if ix + 1 == w && frx < 10.0 * REL_TOLERANCE {
            ix -= 1;
            frx = 1.0;
        } else {
            return None;
        }
    }
    if iy < 0 {
        if iy == -1 && fry > 1.0 - 10.0 * REL_TOLERANCE {
            iy += 1;
            fry = 0.0;
        } else {
            return None;
        }
    } else if iy + 1 >= h {
        if iy + 1 == h && fry < 10.0 * REL_TOLERANCE {
            iy -= 1;
            fry = 1.0;
        } else {
            return None;
        }
    }
    // A sample's values on an n x n block from (x0, y0), checking nodata as PROJ's `valuesAt`.
    let read = |s: i64, x0: i64, y0: i64, n: i64| -> Option<Vec<f64>> {
        let mut v = Vec::with_capacity((n * n) as usize);
        for yy in 0..n {
            for xx in 0..n {
                let val = src.value(s as usize, (x0 + xx) as usize, (y0 + yy) as usize)?;
                if src.is_nodata(val) {
                    return None;
                }
                v.push(val as f64);
            }
        }
        Some(v)
    };
    let mut val = [0.0f64; 3];
    if bilinear {
        let mut m10 = frx;
        let mut m11 = m10;
        let mut m01 = 1.0 - frx;
        let mut m00 = m01;
        m11 *= fry;
        m01 *= fry;
        let fry2 = 1.0 - fry;
        m00 *= fry2;
        m10 *= fry2;
        let interp = |v: &[f64]| m00 * v[0] + m10 * v[1] + m01 * v[2] + m11 * v[3];
        if sx >= 0 && sy >= 0 {
            val[0] = interp(&read(sx, ix, iy, 2)?);
            val[1] = interp(&read(sy, ix, iy, 2)?);
        }
        if sz >= 0 {
            val[2] = interp(&read(sz, ix, iy, 2)?);
        }
    } else {
        if (frx <= 0.5 && ix > 0) || ix + 2 == w {
            ix -= 1;
            frx += 1.0;
        }
        if (fry <= 0.5 && iy > 0) || iy + 2 == h {
            iy -= 1;
            fry += 1.0;
        }
        let quad = |t: f64, f0: f64, f1: f64, f2: f64| {
            let df0 = f1 - f0;
            let df1 = f2 - f1;
            let d2f0 = df1 - df0;
            f0 + t * df0 + 0.5 * t * (t - 1.0) * d2f0
        };
        let interp = |v: &[f64]| {
            let rows: Vec<f64> = (0..3).map(|j| quad(frx, v[3 * j], v[3 * j + 1], v[3 * j + 2])).collect();
            quad(fry, rows[0], rows[1], rows[2])
        };
        if sx >= 0 && sy >= 0 {
            val[0] = interp(&read(sx, ix, iy, 3)?);
            val[1] = interp(&read(sy, ix, iy, 3)?);
        }
        if sz >= 0 {
            val[2] = interp(&read(sz, ix, iy, 3)?);
        }
    }
    if sx >= 0 && sy >= 0 && !en_offset {
        val[0] *= SEC_TO_RAD;
        val[1] *= SEC_TO_RAD;
    }
    Some((val, !bilinear))
}

fn normalize_x(g: &Grid, x: f64) -> f64 {
    let ext = &g.ext;
    if ext.geographic {
        let eps = (ext.res_x + ext.res_y) * REL_TOLERANCE;
        if x < ext.west - eps {
            return x + TWO_PI;
        } else if x > ext.east + eps {
            return x - TWO_PI;
        }
    }
    x
}

/// PROJ's `gridshiftData::grid_apply_internal`.
#[allow(clippy::too_many_arguments)]
fn gridshift_apply_internal<'a>(
    sets: &'a [Arc<GridSet>],
    kind: &str,
    vertical_only: bool,
    p: [f64; 3],
    inverse: bool,
    mut g: &'a Grid,
    interpolation: Option<&str>,
    skip_z: bool,
) -> Option<[f64; 3]> {
    let mut nx = normalize_x(g, p[0]);
    let (mut shift, biquadratic) = gridshift_interpolate(kind, nx, p[1], g, interpolation, skip_z)?;
    if !inverse {
        return Some([p[0] + shift[0], p[1] + shift[1], p[2] + shift[2]]);
    }
    if vertical_only {
        return Some([p[0], p[1], p[2] - shift[2]]);
    }
    let mut guess = (nx - shift[0], p[1] - shift[1]);
    if !biquadratic {
        let mut i = 10;
        let toltol = 1e-12 * 1e-12;
        loop {
            match gridshift_interpolate(kind, guess.0, guess.1, g, interpolation, skip_z) {
                Some((s, _)) => {
                    shift = s;
                    let diff = (guess.0 + shift[0] - nx, guess.1 + shift[1] - p[1]);
                    guess = (guess.0 - diff.0, guess.1 - diff.1);
                    i -= 1;
                    if i == 0 || diff.0 * diff.0 + diff.1 * diff.1 <= toltol {
                        break;
                    }
                }
                None => {
                    shift[2] = 0.0;
                    let found = sets.iter().find_map(|s| s.at_type(kind, guess.0, guess.1));
                    match found {
                        Some(ng) if !std::ptr::eq(ng, g) && !ng.is_null() => {
                            g = ng;
                            nx = normalize_x(g, p[0]);
                            i -= 1;
                            if i == 0 {
                                break;
                            }
                        }
                        _ => break,
                    }
                }
            }
        }
        if i == 0 {
            return None;
        }
    }
    let x = if g.ext.geographic { adjlon(guess.0) } else { guess.0 };
    Some([x, guess.1, p[2] - shift[2]])
}

/// Applies a `gridshift` step (PROJ's `gridshiftData::apply` with the constant offsets).
pub(crate) fn gridshift_apply(
    sets: &[Arc<GridSet>],
    kinds: &GridShiftKinds,
    p: [f64; 3],
    inverse: bool,
    interpolation: Option<&str>,
    skip_z: bool,
) -> Option<[f64; 3]> {
    let mut xyz = p;
    if inverse {
        xyz[0] -= kinds.offset.0;
        xyz[1] -= kinds.offset.1;
    }
    let mut kind = kinds.main;
    let mut found_geog3d = false;
    let mut g = sets.iter().find_map(|s| s.at_type(kind, xyz[0], xyz[1]));
    if g.is_none() {
        if kinds.has_geog3d && kinds.has_horizontal {
            kind = "HORIZONTAL_OFFSET";
            g = sets.iter().find_map(|s| s.at_type(kind, xyz[0], xyz[1]));
        }
    } else if kinds.has_geog3d {
        found_geog3d = true;
    }
    let g = g?;
    let mut out = if g.is_null() {
        xyz
    } else {
        gridshift_apply_internal(sets, kind, !(kinds.has_geog3d || kinds.has_horizontal), xyz, inverse, g, interpolation, skip_z)?
    };
    if !found_geog3d && let Some(aux) = kinds.aux {
        let ag = sets.iter().find_map(|s| s.at_type(aux, out[0], out[1]))?;
        if !ag.is_null() {
            out = gridshift_apply_internal(sets, aux, true, out, inverse, ag, interpolation, skip_z)?;
        }
    }
    if !inverse {
        out[0] += kinds.offset.0;
        out[1] += kinds.offset.1;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    /// Every process-wide lock of the grids is one `host::hold_for_fork` takes across a fork (an
    /// embedding library's fork handlers hold them), so that a child never starts with one held
    /// by a thread it does not have.
    #[test]
    #[cfg(all(unix, not(target_family = "wasm")))]
    fn hold_for_fork_takes_every_grid_lock() {
        use crate::host::assert_held_for_fork as held;
        held("settings", |wait| {
            let _g = config().lock().unwrap_or_else(PoisonError::into_inner);
            wait();
        });
        held("registered grids", |wait| {
            let _g = registered().lock().unwrap_or_else(PoisonError::into_inner);
            wait();
        });
        held("files", |wait| {
            let _g = files().lock().unwrap_or_else(PoisonError::into_inner);
            wait();
        });
        held("failed fetches", |wait| {
            let _g = fetch_failures().lock().unwrap_or_else(PoisonError::into_inner);
            wait();
        });
        held("availability", |wait| {
            let _g = availability().lock().unwrap_or_else(PoisonError::into_inner);
            wait();
        });
        held("grid sets", |wait| {
            let _g = sets_cache().lock().unwrap_or_else(PoisonError::into_inner);
            wait();
        });
    }

    use super::*;

    #[test]
    fn gdal_metadata_items() {
        let m = parse_gdal_metadata(
            r#"<GDALMetadata><Item name="TYPE">HORIZONTAL_OFFSET</Item><Item name="DESCRIPTION" sample="0" role="description">latitude_offset</Item><Item name="UNITTYPE" sample="1" role="unittype">arc-second</Item></GDALMetadata>"#,
        );
        assert_eq!(m.get(&(-1, "TYPE".into())).map(String::as_str), Some("HORIZONTAL_OFFSET"));
        assert_eq!(m.get(&(0, "DESCRIPTION".into())).map(String::as_str), Some("latitude_offset"));
        assert_eq!(m.get(&(1, "UNITTYPE".into())).map(String::as_str), Some("arc-second"));
        assert_eq!(role_value(r#"<Item name="x" sample="2" role="scale">0.5</Item>"#, 2, "scale"), Some(0.5));
    }

    #[test]
    fn null_grid_shifts_nothing() {
        let sets = hgrids("null").unwrap();
        assert_eq!(hgrid_apply(&sets, 0.1, 0.2, false), Some((0.1, 0.2)));
        assert!(hgrids("@surely_missing_grid.tif").unwrap().is_empty());
        assert!(hgrids("surely_missing_grid.tif").is_none());
    }
}

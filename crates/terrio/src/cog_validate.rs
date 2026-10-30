// SPDX-FileCopyrightText: Copyright (c) 2021, cogeotiff
// SPDX-FileCopyrightText: Copyright (c) 2017, Even Rouault
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT AND BSD-3-Clause
//
// Derived from (translated to Rust and modified):
//   rio-cogeo 7.0.4:
//     rio_cogeo/cogeo.py
//   GDAL v3.13.3:
//     swig/python/gdal-utils/osgeo_utils/samples/validate_cloud_optimized_geotiff.py

//! Cloud Optimized GeoTIFF validation: the checks of rio-cogeo's `cog_validate`, itself the
//! rasterio port of GDAL's `validate_cloud_optimized_geotiff.py`, read from the TIFF structure
//! (headers and chunk offsets only, so a remote file costs a few range requests).
//!
//! A file is a valid COG when it is a TIFF whose full-resolution image, larger than 512 pixels,
//! is tiled, with tiled overviews of increasing decimation; whose image directories (IFDs) come
//! first, the main one within the first 300 bytes and each overview's after the image before it;
//! and whose image data comes after the IFDs, from the smallest overview to the full resolution.
//! A large image without overviews is a warning, as a GDAL layout invalidated by later edits
//! (`KNOWN_INCOMPATIBLE_EDITION=YES`) is an error. The errors and warnings are rio-cogeo's, word
//! for word.
//!
//! [`validate_cog_with_options`] and [`validate_cog_bytes`] also offer a GDAL
//! 3.13 profile with exact IFD placement and optional full checks of compressed
//! block leaders, trailers, row ordering and interleaved masks. Automatic full
//! checking stays off for URLs and on for local files or uploaded bytes. Full
//! checking covers the main TIFF and internal overviews; external `.ovr` files
//! make the report invalid and receive header checks only. Explicit remote full
//! checks cost several boundary reads per block. GDAL metadata parsing is bounded
//! to 4 MiB per file and 128 nesting levels. The
//! original [`validate_cog`] API retains rio-cogeo compatibility.
//!
//! When a local file has a `.ovr` sidecar and no internal overviews, the sidecar's images are its
//! overviews, as GDAL (and so rio-cogeo) reads them, and their offsets are checked as rio-cogeo
//! checks them.
//!
//! Beyond rio-cogeo, the IFDs of masks (GDAL's internal per-dataset masks) are checked to come
//! before the image data, as GDAL lays them out in a COG. These findings go to
//! [`CogValidation::layout_notes`], not to the warnings, so that the result (strict or not) stays
//! rio-cogeo's.
//!
//! ```no_run
//! let v = terrio::validate_cog("dem.tif", false)?;
//! if !v.is_valid {
//!     eprintln!("not a COG: {}", v.errors.join("; "));
//! }
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::vfs::Location;
use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom};

/// The most image directories read.
const MAX_IFDS: usize = 1024;
/// The largest tag value array read (chunk offsets or byte counts), in values: 4 Mi blocks per
/// image, such as about 1.1 trillion pixels in 512 x 512 tiles. Arrays must also lie within the
/// file, which bounds what a crafted file can make the validator allocate or fetch.
const MAX_CHUNKS: u64 = 1 << 22;

/// The outcome of [`validate_cog`].
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct CogValidation {
    /// Whether the file is a valid COG: no errors (and, with `strict`, no warnings).
    pub is_valid: bool,
    /// What makes the file not a COG.
    pub errors: Vec<String>,
    /// What a COG should do better.
    pub warnings: Vec<String>,
    /// The offsets of the IFDs of the full-resolution image, then of each overview.
    pub ifd_offsets: Vec<u64>,
    /// The offsets of the first block of the full-resolution image, then of each overview, as
    /// rio-cogeo finds them (images without a stored block are left out). The GDAL
    /// profile records 0 for images with no stored block.
    pub data_offsets: Vec<u64>,
    /// The overview decimation factors, as rasterio reports them (`round(width / overview width)`).
    pub overview_factors: Vec<usize>,
    /// Layout findings beyond rio-cogeo's checks (masks' IFDs after image data), which do not
    /// affect `is_valid`.
    pub layout_notes: Vec<String>,
    /// GDAL's structural metadata, the `GDAL_STRUCTURAL_METADATA_SIZE` block after the header
    /// (`LAYOUT=IFDS_BEFORE_DATA`...), when the file has one.
    pub structural_metadata: Option<String>,
}

/// Structural rules used by the validator. The original API retains rio-cogeo compatibility.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CogValidationProfile {
    /// rio-cogeo's header-only validation rules.
    #[default]
    RioCogeo,
    /// GDAL 3.13's exact IFD placement and optional full block-layout checks.
    Gdal,
}

/// Validation controls shared by native and uploaded TIFF inputs.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CogValidationOptions {
    /// Treat missing-overview warnings as errors.
    pub strict: bool,
    /// Select rio-cogeo compatibility or GDAL 3.13 validation.
    pub profile: CogValidationProfile,
    /// In the GDAL profile, inspect every block's structural promises. `None`
    /// enables these checks for local/uploaded inputs and disables them for URLs.
    pub full_check: Option<bool>,
}

/// One image directory: what validation needs of it.
#[derive(Debug, Clone, Default)]
struct Ifd {
    offset: u64,
    width: u64,
    height: u64,
    subfile_type: u64,
    samples: u64,
    planar: u64,
    tile_interleave: bool,
    /// Tile width and height, when tiled.
    tile: Option<(u64, u64)>,
    rows_per_strip: u64,
    offsets: Vec<u64>,
    byte_counts: Vec<u64>,
}

impl Ifd {
    fn is_reduced(&self) -> bool {
        self.subfile_type & 1 != 0
    }

    fn is_mask(&self) -> bool {
        self.subfile_type & 4 != 0
    }

    /// GDAL's block size: the tile, or (width, rows per strip) for strips.
    fn block(&self) -> (u64, u64) {
        self.tile.unwrap_or((self.width, self.rows_per_strip.clamp(1, self.height.max(1))))
    }

    /// The blocks per row and column.
    fn grid(&self) -> (u64, u64) {
        let (bw, bh) = self.block();
        (self.width.div_ceil(bw.max(1)), self.height.div_ceil(bh.max(1)))
    }

    /// The offset of the first stored block of the first band in row order, among the first
    /// `columns` columns and `rows` rows of the block grid (all of it when `None`). A block is
    /// stored when its offset and byte count are not zero, as GDAL's `BLOCK_OFFSET_x_y` reports.
    ///
    /// The search walks the stored offsets, whose index order is row order, so that its cost is
    /// bounded by the offset array rather than by the grid the image's size tags declare.
    fn first_stored_block(&self, columns: Option<u64>, rows: Option<u64>) -> Option<u64> {
        let (across, down) = self.grid();
        let across = across.max(1);
        let columns = columns.map_or(across, |c| c.min(across));
        let rows = rows.map_or(down, |r| r.min(down));
        self.offsets
            .iter()
            .zip(&self.byte_counts)
            .enumerate()
            .map(|(i, (&o, &c))| (i as u64 % across, i as u64 / across, o, c))
            .take_while(|&(_, y, _, _)| y < rows)
            .find_map(|(x, _, o, c)| (x < columns && o != 0 && c != 0).then_some(o))
    }

    /// The offset of the first stored block, in row order: GDAL's validation script's
    /// `get_block_offset`.
    fn first_block_offset(&self) -> Option<u64> {
        self.first_stored_block(None, None)
    }
}

/// The parsed structure of a TIFF file.
struct Tiff {
    ifds: Vec<Ifd>,
    ghost: Option<String>,
    expected_ifd_offset: u64,
}

/// Decodes numbers in the file's byte order.
#[derive(Clone, Copy)]
struct Endian {
    le: bool,
}

impl Endian {
    fn u16(self, b: &[u8]) -> u16 {
        let a = [b[0], b[1]];
        if self.le { u16::from_le_bytes(a) } else { u16::from_be_bytes(a) }
    }

    fn u32(self, b: &[u8]) -> u32 {
        let a = [b[0], b[1], b[2], b[3]];
        if self.le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
    }

    fn u64(self, b: &[u8]) -> u64 {
        let a: [u8; 8] = b[..8].try_into().unwrap_or_default();
        if self.le { u64::from_le_bytes(a) } else { u64::from_be_bytes(a) }
    }

    /// The integer values of a SHORT, LONG or LONG8 tag.
    fn ints(self, typ: u16, data: &[u8]) -> Vec<u64> {
        match typ {
            1 => data.iter().map(|v| *v as u64).collect(),
            3 => data.as_chunks::<2>().0.iter().map(|c| self.u16(c) as u64).collect(),
            4 | 13 => data.as_chunks::<4>().0.iter().map(|c| self.u32(c) as u64).collect(),
            16 | 18 => data.as_chunks::<8>().0.iter().map(|c| self.u64(c)).collect(),
            _ => Vec::new(),
        }
    }
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::InvalidData(msg.into())
}

/// Extract just the interleave item, without constructing an XML tree whose
/// recursive drop could overflow on deeply nested, untrusted metadata.
fn tile_interleave(data: &[u8]) -> Result<bool> {
    use quick_xml::events::Event;
    let text = std::str::from_utf8(data).map_err(|e| invalid(format!("GDAL metadata XML: {e}")))?;
    let mut reader = quick_xml::Reader::from_str(text.trim_end_matches('\0'));
    let mut depth = 0usize;
    let mut target = None;
    let mut content = String::new();
    let mut tile = false;
    let mut root = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                if depth == 0 && std::mem::replace(&mut root, true) {
                    return Err(invalid("GDAL metadata XML: multiple root elements"));
                }
                depth += 1;
                if depth > 128 {
                    return Err(invalid("GDAL metadata XML exceeds 128 nesting levels"));
                }
                let mut name = false;
                let mut domain = false;
                for attribute in element.attributes() {
                    let attribute = attribute.map_err(|e| invalid(format!("GDAL metadata XML: {e}")))?;
                    let value = attribute
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|e| invalid(format!("GDAL metadata XML: {e}")))?;
                    match attribute.key.as_ref() {
                        "name" => name = value == "INTERLEAVE",
                        "domain" => domain = value == "IMAGE_STRUCTURE",
                        _ => (),
                    }
                }
                if element.local_name().as_ref() == "Item" && name && domain {
                    target = Some(depth);
                    content.clear();
                }
            }
            Ok(Event::Empty(element)) => {
                if depth == 0 && std::mem::replace(&mut root, true) {
                    return Err(invalid("GDAL metadata XML: multiple root elements"));
                }
                for attribute in element.attributes() {
                    let attribute = attribute.map_err(|e| invalid(format!("GDAL metadata XML: {e}")))?;
                    attribute
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map_err(|e| invalid(format!("GDAL metadata XML: {e}")))?;
                }
            }
            Ok(Event::Text(text)) if target == Some(depth) => content.push_str(&text.xml10_content()),
            Ok(Event::CData(text)) if target == Some(depth) => content.push_str(&text.xml10_content()),
            Ok(Event::GeneralRef(reference)) => {
                let value = reference
                    .resolve_char_ref()
                    .ok()
                    .flatten()
                    .or_else(|| match &*reference {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        _ => None,
                    })
                    .ok_or_else(|| invalid("GDAL metadata XML: unresolved entity"))?;
                if depth == 0 {
                    return Err(invalid("GDAL metadata XML: reference outside root element"));
                }
                if target == Some(depth) {
                    content.push(value);
                }
            }
            Ok(Event::End(_)) => {
                if target == Some(depth) {
                    tile |= content.trim() == "TILE";
                    target = None;
                }
                depth = depth.checked_sub(1).ok_or_else(|| invalid("GDAL metadata XML: unmatched closing tag"))?;
            }
            Ok(Event::Text(text)) if depth == 0 && !text.xml10_content().trim().is_empty() => {
                return Err(invalid("GDAL metadata XML: text outside root element"));
            }
            Ok(Event::CData(_)) if depth == 0 => return Err(invalid("GDAL metadata XML: CDATA outside root element")),
            Ok(Event::Eof) => {
                return if depth != 0 {
                    Err(invalid("GDAL metadata XML: unclosed element"))
                } else if !root {
                    Err(invalid("GDAL metadata XML: missing root element"))
                } else {
                    Ok(tile)
                };
            }
            Err(error) => return Err(invalid(format!("GDAL metadata XML: {error}"))),
            _ => (),
        }
    }
}

/// Reads the IFDs of a TIFF. `None` when the stream is not a TIFF. A TIFF is read only if the
/// process-wide raster allow-list has the GTiff driver (`shown` names it in the refusal), as GDAL's
/// validation opens it with that driver.
fn read_tiff<R: Read + Seek>(f: &mut R, shown: &str, read_metadata: bool) -> Result<Option<Tiff>> {
    let mut header = [0u8; 16];
    f.seek(SeekFrom::Start(0))?;
    let mut n = 0;
    while n < 8 {
        let k = f.read(&mut header[n..8])?;
        if k == 0 {
            return Ok(None);
        }
        n += k;
    }
    let e = match &header[..2] {
        b"II" => Endian { le: true },
        b"MM" => Endian { le: false },
        _ => return Ok(None),
    };
    let big = match e.u16(&header[2..4]) {
        42 => false,
        43 => true,
        _ => return Ok(None),
    };
    crate::formats::check_driver_allowed("GTiff", shown, crate::formats::Kind::Raster)?;
    let mut next = if big {
        f.read_exact(&mut header[8..16])?;
        e.u64(&header[8..16])
    } else {
        e.u32(&header[4..8]) as u64
    };
    let ghost = read_ghost(f, if big { 16 } else { 8 })?;
    let ghost_size = ghost.as_ref().map_or(0, |(_, size)| *size);
    let ghost = ghost.map(|(text, _)| text);
    // Value arrays must lie within the stream, checked before they are allocated or fetched.
    let stream_len = f.seek(SeekFrom::End(0))?;
    let (entry_len, count_len, inline) = if big { (20usize, 8usize, 8usize) } else { (12, 2, 4) };
    // The bytes of the arrays read so far, of every image: see below.
    let mut stored_bytes: u64 = 0;
    let mut ifds: Vec<Ifd> = Vec::new();
    let mut metadata_bytes = 0u64;
    while next != 0 && ifds.len() < MAX_IFDS {
        if ifds.iter().any(|i| i.offset == next) {
            return Err(invalid("the TIFF's image directories loop"));
        }
        f.seek(SeekFrom::Start(next))?;
        let mut b = [0u8; 8];
        f.read_exact(&mut b[..count_len])?;
        let count = if big { e.u64(&b) } else { e.u16(&b) as u64 };
        if count > 4096 {
            return Err(invalid(format!("a TIFF image directory of {count} entries")));
        }
        let mut entries = vec![0u8; count as usize * entry_len];
        f.read_exact(&mut entries)?;
        let mut ifd = Ifd { offset: next, samples: 1, planar: 1, rows_per_strip: u64::MAX, ..Default::default() };
        let mut tile = (None, None);
        // (tag, type, offset, length) of each array stored outside its entry, and the inline values.
        let mut stored: Vec<(u16, u16, u64, usize)> = Vec::new();
        let mut values: Vec<(u16, u16, Vec<u8>)> = Vec::new();
        for entry in entries.chunks_exact(entry_len) {
            let tag = e.u16(&entry[0..2]);
            if !matches!(tag, 254 | 256 | 257 | 273 | 277 | 278 | 279 | 284 | 322 | 323 | 324 | 325 | 42112) {
                continue;
            }
            let typ = e.u16(&entry[2..4]);
            let (n, value) = if big { (e.u64(&entry[4..12]), &entry[12..20]) } else { (e.u32(&entry[4..8]) as u64, &entry[8..12]) };
            let size = match typ {
                1 => 1,
                2 if tag == 42112 => 1,
                3 => 2,
                4 | 13 => 4,
                16 | 18 => 8,
                _ => continue,
            };
            if tag == 42112 {
                // The compatibility profile never needed this tag. Do not make
                // unrelated metadata a new default-profile failure condition.
                if !read_metadata {
                    continue;
                }
                metadata_bytes = metadata_bytes.saturating_add(n.saturating_mul(size));
                if metadata_bytes > 4 * 1024 * 1024 {
                    return Err(invalid("GDAL metadata exceeds the 4 MiB per-file validation limit"));
                }
            }
            if n > MAX_CHUNKS {
                return Err(invalid(format!("TIFF tag {tag} has {n} values")));
            }
            let len = (n * size) as usize;
            if len <= inline {
                values.push((tag, typ, value[..len].to_vec()));
            } else {
                let off = if big { e.u64(value) } else { e.u32(value) as u64 };
                if off.checked_add(len as u64).is_none_or(|end| end > stream_len) {
                    return Err(invalid(format!("TIFF tag {tag}'s {n} values run past the end of the file")));
                }
                // The images of a file do not share their arrays, which together fit in it: more
                // is refused, before it is read. Every image's offsets and byte counts are held,
                // so images pointing to one array would hold it once per image.
                stored_bytes += len as u64;
                if stored_bytes > stream_len {
                    return Err(invalid(format!("the TIFF's tag arrays take more than its {stream_len} bytes")));
                }
                stored.push((tag, typ, off, len));
            }
        }
        f.read_exact(&mut b[..if big { 8 } else { 4 }])?;
        next = if big { e.u64(&b) } else { e.u32(&b) as u64 };
        let stored = stored.into_iter().map(|(tag, typ, off, len)| -> Result<_> {
            let mut data = vec![0; len];
            f.seek(SeekFrom::Start(off))?;
            f.read_exact(&mut data)?;
            Ok((tag, typ, data))
        });
        for item in values.into_iter().map(Ok).chain(stored) {
            let (tag, typ, data) = item?;
            if tag == 42112 {
                ifd.tile_interleave = tile_interleave(&data)?;
                continue;
            }
            let v = e.ints(typ, &data);
            let first = v.first().copied().unwrap_or(0);
            match tag {
                254 => ifd.subfile_type = first,
                256 => ifd.width = first,
                257 => ifd.height = first,
                277 => ifd.samples = first.max(1),
                278 => ifd.rows_per_strip = first,
                284 => ifd.planar = first,
                322 => tile.0 = Some(first),
                323 => tile.1 = Some(first),
                273 | 324 => ifd.offsets = v,
                279 | 325 => ifd.byte_counts = v,
                _ => {}
            }
        }
        if let (Some(w), Some(h)) = tile {
            ifd.tile = Some((w, h));
        }
        ifds.push(ifd);
    }
    let mut expected_ifd_offset = if big { 16 } else { 8 };
    if ghost.is_some() {
        expected_ifd_offset += "GDAL_STRUCTURAL_METADATA_SIZE=000000 bytes\n".len() as u64 + ghost_size;
        expected_ifd_offset += expected_ifd_offset % 2;
    }
    Ok(Some(Tiff { ifds, ghost, expected_ifd_offset }))
}

/// GDAL's structural metadata block (`GDAL_STRUCTURAL_METADATA_SIZE=...`) at `offset`, if any.
fn read_ghost<R: Read + Seek>(f: &mut R, offset: u64) -> Result<Option<(String, u64)>> {
    const KEY: &[u8] = b"GDAL_STRUCTURAL_METADATA_SIZE=";
    f.seek(SeekFrom::Start(offset))?;
    let mut head = [0u8; 43];
    let mut n = 0;
    while n < head.len() {
        let k = f.read(&mut head[n..])?;
        if k == 0 {
            return Ok(None);
        }
        n += k;
    }
    if !head.starts_with(KEY) {
        return Ok(None);
    }
    let size: usize = match std::str::from_utf8(&head[KEY.len()..KEY.len() + 6]).ok().and_then(|s| s.trim().parse().ok()) {
        Some(n) if n < 1 << 16 => n,
        _ => return Ok(None),
    };
    let mut body = vec![0u8; size];
    f.read_exact(&mut body)?;
    Ok(Some((String::from_utf8_lossy(&body).into_owned(), size as u64)))
}

/// Validates a Cloud Optimized GeoTIFF, local or remote (HTTP(S) or S3, read with range
/// requests), as rio-cogeo's `cog_validate` does (see the `cog_validate` module documentation).
/// `strict` makes warnings errors, as rio-cogeo's.
///
/// A file that is not a TIFF is not valid (`"The file is not a GeoTIFF"`); a file that cannot be
/// read is an error.
pub fn validate_cog(path: &str, strict: bool) -> Result<CogValidation> {
    validate_cog_with_options(path, &CogValidationOptions { strict, ..Default::default() })
}

/// Validates a native/VSI input, with GDAL 3.13 full checks available through `profile`.
/// Automatic full checks are disabled for remote inputs to preserve bounded range I/O.
pub fn validate_cog_with_options(path: &str, options: &CogValidationOptions) -> Result<CogValidation> {
    if options.full_check == Some(true) && options.profile != CogValidationProfile::Gdal {
        return Err(Error::InvalidArgument("full_check requires the gdal validation profile".into()));
    }
    let loc = Location::parse(path);
    match &loc {
        #[cfg(remote)]
        Location::Url(_) | Location::S3 { .. } => {
            let remote = loc.remote().ok_or_else(|| Error::InvalidArgument(format!("'{path}' is not remote")))?;
            validate_reader(&mut crate::http::RangeReader::open(&remote)?, path, None, options, false)
        }
        Location::Local(p) => {
            let mut file = std::fs::File::open(p)?;
            let mut ovr = p.clone().into_os_string();
            ovr.push(".ovr");
            let ovr = std::path::Path::new(&ovr);
            let sidecar = if ovr.is_file() {
                Some(
                    read_tiff(&mut std::io::BufReader::new(std::fs::File::open(ovr)?), &ovr.to_string_lossy(), false)
                        .ok()
                        .flatten()
                        .unwrap_or(Tiff { ifds: Vec::new(), ghost: None, expected_ifd_offset: 8 }),
                )
            } else {
                None
            };
            // Full checks make tiny reads at distant compressed-block boundaries.
            // A BufReader refills after each absolute seek and reads payload bytes
            // that validation does not need. Read the local file directly here.
            if options.profile == CogValidationProfile::Gdal && options.full_check.unwrap_or(true) {
                validate_reader(&mut file, path, sidecar, options, true)
            } else {
                validate_reader(&mut std::io::BufReader::new(file), path, sidecar, options, true)
            }
        }
        _ => {
            let local = crate::vfs::resolve(path)?;
            validate_reader(&mut std::io::BufReader::new(std::fs::File::open(local.path())?), path, None, options, false)
        }
    }
}

/// Validates an uploaded TIFF without decoding pixels or creating a temporary file.
/// External `.ovr` sidecars are unavailable for a standalone byte buffer.
pub fn validate_cog_bytes(bytes: &[u8], options: &CogValidationOptions) -> Result<CogValidation> {
    validate_reader(&mut std::io::Cursor::new(bytes), "a GeoTIFF in memory", None, options, true)
}

/// Remote full checks are opt-in, but still bound their boundary-read workload.
/// This limits client read calls (and therefore range fetches, excluding configured
/// transport retries); header/offset-table requests occur before this budget.
struct BoundaryReader<'a, R> {
    inner: &'a mut R,
    remaining: usize,
}
impl<R: Read> Read for BoundaryReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(std::io::Error::other(
                "remote COG full check exceeds 10,000 boundary reads; download the TIFF and validate locally",
            ));
        }
        self.remaining -= 1;
        self.inner.read(bytes)
    }
}
impl<R: Seek> Seek for BoundaryReader<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(position)
    }
}

fn validate_reader<R: Read + Seek>(
    reader: &mut R,
    shown: &str,
    sidecar: Option<Tiff>,
    options: &CogValidationOptions,
    local: bool,
) -> Result<CogValidation> {
    if options.full_check == Some(true) && options.profile != CogValidationProfile::Gdal {
        return Err(Error::InvalidArgument("full_check requires the gdal validation profile".into()));
    }
    let full = options.profile == CogValidationProfile::Gdal && options.full_check.unwrap_or(local);
    let tiff = read_tiff(reader, shown, full)?;
    let mut full_errors = Vec::new();
    if full && let Some(tiff) = &tiff {
        if local {
            full_check(reader, tiff, &mut full_errors)?;
        } else {
            full_check(&mut BoundaryReader { inner: reader, remaining: 10_000 }, tiff, &mut full_errors)?;
        }
    }
    let mut result = validate(tiff, sidecar, options.strict, options.profile == CogValidationProfile::Gdal);
    result.errors.extend(full_errors);
    result.is_valid = result.errors.is_empty() && (!options.strict || result.warnings.is_empty());
    Ok(result)
}

const MAX_BLOCK_FINDINGS: usize = 1000;
const BLOCK_FINDING_LIMIT: &str = "Block validation stopped at 1000 findings; additional findings or blocks may be omitted";
fn finding_limit(errors: &mut Vec<String>) -> bool {
    if errors.len() < MAX_BLOCK_FINDINGS {
        return false;
    }
    if errors.last().is_none_or(|error| error != BLOCK_FINDING_LIMIT) {
        errors.truncate(MAX_BLOCK_FINDINGS);
        errors.push(BLOCK_FINDING_LIMIT.into());
    }
    true
}

/// GDAL structural promises concern compressed block bytes, so these checks do
/// not decode pixels. Offset arrays remain bounded by the parser's MAX_CHUNKS.
fn full_check<R: Read + Seek>(reader: &mut R, tiff: &Tiff, errors: &mut Vec<String>) -> Result<()> {
    let ghost = tiff.ghost.as_deref().unwrap_or("");
    let order = ghost.contains("BLOCK_ORDER=ROW_MAJOR");
    let leader = ghost.contains("BLOCK_LEADER=SIZE_AS_UINT4");
    let trailer = ghost.contains("BLOCK_TRAILER=LAST_4_BYTES_REPEATED");
    let interleaved_mask = ghost.contains("MASK_INTERLEAVED_WITH_IMAGERY=YES");
    if !order && !leader && !trailer && !interleaved_mask {
        return Ok(());
    }
    let length = reader.seek(SeekFrom::End(0))?;
    let Some(main) = tiff.ifds.first() else {
        return Ok(());
    };
    let tile_interleave = main.tile_interleave;
    let images: Vec<_> = tiff
        .ifds
        .iter()
        .enumerate()
        .filter(|(index, image)| *index == 0 || image.is_reduced() && !image.is_mask() && image.samples == main.samples)
        .collect();
    // GDAL ScanDirectories assigns each mask to the first preceding, unmatched
    // image with matching dimensions and band count. Positional mask indexing
    // drifts when an unrelated reduced IFD was excluded from the overview list.
    let mut masks = vec![None; images.len()];
    for (mask_index, mask) in tiff.ifds.iter().enumerate().filter(|(_, i)| i.is_mask()) {
        if mask.samples != 1 && mask.samples != main.samples {
            continue;
        }
        if let Some((level, _)) = images.iter().enumerate().find(|(level, (image_index, image))| {
            *image_index < mask_index
                && masks[*level].is_none()
                && mask.is_reduced() == image.is_reduced()
                && mask.width == image.width
                && mask.height == image.height
        }) {
            masks[level] = Some(mask);
        }
    }
    for (level, (_, image)) in images.iter().enumerate() {
        if finding_limit(errors) {
            return Ok(());
        }
        let name = if level == 0 { "Main resolution image".to_string() } else { format!("Overview {}", level - 1) };
        let mask = masks[level];
        let blocks = image.grid().0.checked_mul(image.grid().1).ok_or_else(|| invalid("block grid overflows"))?;
        let planes = if image.planar == 2 && blocks > 0 { image.samples } else { 1 };
        let expected = blocks.checked_mul(planes).ok_or_else(|| invalid("block count overflows"))?;
        if expected != image.offsets.len() as u64 || expected != image.byte_counts.len() as u64 {
            errors.push(format!("{name}: block offset/count arrays do not match the image grid"));
            continue;
        }
        // Metadata can declare an enormous grid, but we have just checked that
        // it fits the bounded arrays before converting to usize or iterating.
        let blocks = blocks as usize;
        let mut last = 0;
        for plane in 0..planes as usize {
            if tile_interleave {
                last = 0;
            }
            let band_name = if image.planar == 2 { format!("Band {} of {}", plane + 1, name.to_lowercase()) } else { name.clone() };
            let mask = if interleaved_mask && (image.planar != 2 || tile_interleave && plane + 1 == planes as usize) { mask } else { None };
            check_blocks(
                reader,
                length,
                image,
                plane * blocks..(plane + 1) * blocks,
                &band_name,
                mask,
                (order, leader, trailer),
                &mut last,
                errors,
            )?;
            if finding_limit(errors) {
                return Ok(());
            }
        }
        if tile_interleave && order {
            let mut last = 0;
            for block in 0..blocks {
                for plane in 0..planes as usize {
                    if finding_limit(errors) {
                        return Ok(());
                    }
                    let offset = image.offsets[plane * blocks + block];
                    if offset != 0 {
                        if offset < last {
                            errors.push(format!("{name}: interleaved block offsets are not in row-major order"));
                        }
                        last = offset;
                    }
                }
            }
        }
        // GDAL full_check_band checks internal masks separately with the same
        // block promises, even without MASK_INTERLEAVED_WITH_IMAGERY.
        if let Some(mask) = mask {
            let expected = mask.grid().0.checked_mul(mask.grid().1).ok_or_else(|| invalid("mask block grid overflows"))?;
            if expected != mask.offsets.len() as u64 || expected != mask.byte_counts.len() as u64 {
                errors.push(format!("Mask band of {}: block offset/count arrays do not match the image grid", name.to_lowercase()));
                continue;
            }
            let mut last = 0;
            check_blocks(
                reader,
                length,
                mask,
                0..mask.offsets.len(),
                &format!("Mask band of {}", name.to_lowercase()),
                None,
                (order, leader, trailer),
                &mut last,
                errors,
            )?;
        }
    }
    finding_limit(errors);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn check_blocks<R: Read + Seek>(
    reader: &mut R,
    length: u64,
    image: &Ifd,
    indices: std::ops::Range<usize>,
    name: &str,
    mask: Option<&Ifd>,
    flags: (bool, bool, bool),
    last: &mut u64,
    errors: &mut Vec<String>,
) -> Result<()> {
    let (order, leader, trailer) = flags;
    let across = image.grid().0.max(1);
    let blocks = across.checked_mul(image.grid().1).ok_or_else(|| invalid("block grid overflows"))?.max(1);
    if mask.is_some_and(|mask| mask.block() != image.block()) {
        errors.push(format!("{name}: mask block size is different from its imagery band"));
    }
    let mask = mask.filter(|mask| mask.block() == image.block());
    for index in indices {
        if finding_limit(errors) {
            return Ok(());
        }
        let mut offset = image.offsets[index];
        let count = image.byte_counts.get(index).copied().unwrap_or(0);
        let block = index as u64 % blocks;
        let (x, y) = (block % across, block / across);
        if offset != 0 {
            if order && offset < *last {
                errors.push(format!("{name}: offset of block ({x}, {y}) is smaller than previous block"));
            }
            if leader {
                if offset < 4 || offset > length {
                    errors.push(format!("{name}: block ({x}, {y}) leader lies outside the file"));
                } else {
                    let mut bytes = [0; 4];
                    reader.seek(SeekFrom::Start(offset - 4))?;
                    reader.read_exact(&mut bytes)?;
                    let stored = u32::from_le_bytes(bytes) as u64;
                    if stored != count {
                        errors.push(format!("{name}: for block ({x}, {y}), size in leader bytes is {stored} instead of {count}"));
                    }
                }
            }
            if trailer && count >= 4 {
                if offset.checked_add(count).and_then(|end| end.checked_add(4)).is_none_or(|end| end > length) {
                    errors.push(format!("{name}: block ({x}, {y}) trailer lies outside the file"));
                } else {
                    let mut bytes = [0; 8];
                    reader.seek(SeekFrom::Start(offset + count - 4))?;
                    reader.read_exact(&mut bytes)?;
                    if bytes[..4] != bytes[4..] {
                        errors.push(format!("{name}: for block ({x}, {y}), trailer bytes are invalid"));
                    }
                }
            }
        }
        if let Some(mask) = mask {
            let mask_offset = mask.offsets.get(block as usize).copied().unwrap_or(0);
            if offset > 0 && mask_offset > 0 {
                let expected = offset.checked_add(count).and_then(|end| end.checked_add(4 * (u64::from(leader) + u64::from(trailer))));
                if expected != Some(mask_offset) {
                    errors.push(format!("Mask of {name}: for block ({x}, {y}), unexpected interleaved offset {mask_offset}"));
                }
            } else if offset == 0 && mask_offset > 0 {
                if order && mask_offset < *last {
                    errors.push(format!("Mask of {name}: offset of block ({x}, {y}) is smaller than previous block"));
                }
                offset = mask_offset;
            }
        }
        // GDAL 3.13.3 resets ordering after an empty block too. Keep this
        // observable policy rather than silently tightening the GDAL profile.
        *last = offset;
    }
    Ok(())
}

/// Validates parsed TIFF structure, with the structure of its `.ovr` sidecar when it has one.
fn validate(tiff: Option<Tiff>, sidecar: Option<Tiff>, strict: bool, gdal: bool) -> CogValidation {
    let mut v = CogValidation::default();
    let Some(Tiff { ifds, ghost, expected_ifd_offset }) = tiff.filter(|t| !t.ifds.is_empty()) else {
        v.errors.push("The file is not a GeoTIFF".into());
        return v;
    };
    v.structural_metadata = ghost.clone();
    let main = &ifds[0];
    // GDAL's internal overviews: reduced-resolution images that are not masks, with the main
    // image's samples; and the masks of the main image and of each overview.
    let mut overviews: Vec<&Ifd> = ifds[1..].iter().filter(|i| i.is_reduced() && !i.is_mask() && i.samples == main.samples).collect();
    let masks: Vec<&Ifd> = ifds[1..].iter().filter(|i| i.is_mask()).collect();
    let external_ovr = sidecar.is_some();
    // Without internal overviews, GDAL takes the sidecar's images as the overviews.
    if overviews.is_empty()
        && let Some(sc) = &sidecar
    {
        overviews = sc.ifds.iter().filter(|i| i.is_reduced() && !i.is_mask() && i.samples == main.samples).collect();
    }

    if external_ovr {
        v.errors.push("Overviews found in external .ovr file. They should be internal".into());
    }
    let factors: Vec<usize> = overviews.iter().map(|o| (main.width as f64 / o.width.max(1) as f64).round_ties_even() as usize).collect();
    v.overview_factors = factors.clone();
    let (block_w, block_h) = main.block();
    if main.width > 512 || main.height > 512 {
        if block_w == main.width && (!gdal || block_w > 1024) {
            v.errors.push("The file is greater than 512xH or 512xW, but is not tiled".into());
        }
        if overviews.is_empty() {
            v.warnings.push("The file is greater than 512xH or 512xW, it is recommended to include internal overviews".into());
        }
    }
    if gdal && main.offset != expected_ifd_offset {
        v.errors.push(format!("The offset of the main IFD should be {expected_ifd_offset}. It is {} instead", main.offset));
    } else if !gdal && main.offset > 300 {
        v.errors.push(format!("The offset of the main IFD should be < 300. It is {} instead", main.offset));
    }
    v.ifd_offsets.push(main.offset);
    if ghost.as_deref().is_some_and(|g| g.contains("KNOWN_INCOMPATIBLE_EDITION=YES")) {
        v.errors.push(
            "This file used to have optimizations in its layout, but those have been, at least partly, invalidated by later changes".into(),
        );
    }
    let mut sorted = factors.clone();
    sorted.sort_unstable();
    if !gdal && !factors.is_empty() && factors != sorted {
        v.errors.push("Overviews should be sorted".into());
    }
    for (ix, (o, dec)) in overviews.iter().zip(&factors).enumerate() {
        if !gdal && *dec <= 1 {
            v.errors.push(format!("Invalid Decimation {dec} for overview level {ix}"));
        }
        if gdal {
            let previous = if ix == 0 { main } else { overviews[ix - 1] };
            if o.width > previous.width || o.height > previous.height {
                v.errors.push(if ix == 0 {
                    "First overview has larger dimension than main band".into()
                } else {
                    format!("Overview of index {ix} has larger dimension than overview of index {}", ix - 1)
                });
            }
        }
        let previous = *v.ifd_offsets.last().unwrap_or(&0);
        v.ifd_offsets.push(o.offset);
        if o.offset < previous {
            if ix == 0 {
                v.errors.push(format!(
                    "The offset of the IFD for overview of index {ix} is {}, whereas it should be greater than the one of the main image, which is at byte {previous}",
                    o.offset
                ));
            } else {
                v.errors.push(format!(
                    "The offset of the IFD for overview of index {ix} is {}, whereas it should be greater than the one of index {}, which is at byte {previous}",
                    o.offset,
                    ix - 1
                ));
            }
        }
    }

    // The first block of each image, scanned as rio-cogeo scans it: rows of the main image's
    // block height and columns of its block width swapped (rasterio's block shapes are (rows,
    // cols)), overviews by the main image's block size over `size / factor` pixels.
    let scan = |ifd: &Ifd, width: u64, height: u64| -> Option<u64> {
        if gdal {
            return ifd.first_block_offset();
        }
        let yblocks = height.div_ceil(block_w.max(1));
        let xblocks = width.div_ceil(block_h.max(1));
        ifd.first_stored_block(Some(xblocks), Some(yblocks))
    };
    if let Some(off) = scan(main, main.width, main.height).or(gdal.then_some(0)) {
        v.data_offsets.push(off);
    }
    for (o, dec) in overviews.iter().zip(&factors) {
        let d = (*dec as u64).max(1);
        if let Some(off) = scan(o, main.width / d, main.height / d).or(gdal.then_some(0)) {
            v.data_offsets.push(off);
        }
    }
    let data = &v.data_offsets;
    let mut errors = Vec::new();
    if let (Some(last_data), Some(last_ifd)) = (data.last(), v.ifd_offsets.last())
        && *last_data != 0
        && last_data < last_ifd
    {
        errors.push(if overviews.is_empty() {
            "The offset of the first block of the image should be after its IFD".to_string()
        } else {
            "The offset of the first block of the smallest overview should be after its IFD".to_string()
        });
    }
    for i in (1..data.len().saturating_sub(1)).rev() {
        if data[i] != 0 && data[i] < data[i + 1] {
            errors.push(format!(
                "The offset of the first block of overview of index {} should be after the one of the overview of index {i}",
                i - 1
            ));
        }
    }
    if data.len() >= 2 && data[0] != 0 && data[0] < data[1] {
        errors.push(format!(
            "The offset of the first block of the main resolution image should be after the one of the overview of index {}",
            overviews.len() as i64 - 1
        ));
    }
    v.errors.extend(errors);
    for (ix, o) in overviews.iter().enumerate() {
        if (o.width > 512 || o.height > 512) && o.block().0 == o.width && (!gdal || o.block().0 > 1024) {
            v.errors.push(format!("Overview of index {ix} is not tiled"));
        }
    }

    // Masks, as GDAL's COG driver lays them out (beyond rio-cogeo): IFDs before any image data,
    // and each mask's data before the data of the image it masks.
    let first_data = ifds.iter().filter_map(|i| i.first_block_offset()).min();
    for m in &masks {
        // An overview's mask has the overview's size.
        let which = if !m.is_reduced() {
            "mask of the main image".to_string()
        } else if let Some(ix) = overviews.iter().position(|o| (o.width, o.height) == (m.width, m.height)) {
            format!("mask of overview {ix}")
        } else {
            "mask of an overview".to_string()
        };
        if first_data.is_some_and(|d| m.offset > d) {
            v.layout_notes.push(format!("The IFD of the {which} is at byte {}, after image data", m.offset));
        }
    }
    v.is_valid = v.errors.is_empty() && !(strict && !v.warnings.is_empty());
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(raster: &wbraster::Raster, options: &crate::RasterWriteOptions) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.tif").to_string_lossy().into_owned();
        crate::write_raster(raster, &p, options).unwrap();
        (dir, p)
    }

    fn raster(cols: usize, rows: usize) -> wbraster::Raster {
        let cfg = wbraster::RasterConfig {
            cols,
            rows,
            bands: 1,
            cell_size: 1.0,
            nodata: -9999.0,
            data_type: wbraster::DataType::F32,
            crs: wbraster::CrsInfo::from_epsg(32610),
            ..Default::default()
        };
        let data = wbraster::raster::RasterData::F32((0..cols * rows).map(|i| (i % 1000) as f32).collect());
        wbraster::Raster::from_data_native(cfg, data).unwrap()
    }

    fn driver(d: crate::formats::RasterDriver, co: &[(&str, &str)]) -> crate::RasterWriteOptions {
        crate::RasterWriteOptions {
            driver: Some(d),
            creation_options: co.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn terrio_cogs_are_valid() {
        use crate::formats::RasterDriver::Cog;
        for (cols, rows) in [(100, 80), (1300, 700), (2100, 1500)] {
            let (_d, p) = write(&raster(cols, rows), &driver(Cog, &[("COMPRESS", "DEFLATE")]));
            let v = validate_cog(&p, true).unwrap();
            assert!(v.is_valid, "{cols}x{rows}: {v:?}");
            assert!(v.structural_metadata.as_deref().unwrap().contains("LAYOUT=IFDS_BEFORE_DATA"));
            let expected: Vec<usize> = (1..v.ifd_offsets.len()).map(|i| 1 << i).collect();
            assert_eq!(v.overview_factors, expected);
        }
    }

    #[test]
    fn geotiff_layouts_are_not_cogs() {
        use crate::formats::RasterDriver::GTiff;
        // Strips, larger than 512 pixels: not tiled, no overviews, IFD after the data.
        let (_d, p) = write(&raster(1300, 700), &driver(GTiff, &[]));
        let v = validate_cog(&p, false).unwrap();
        assert!(!v.is_valid);
        assert!(v.errors.contains(&"The file is greater than 512xH or 512xW, but is not tiled".to_string()), "{v:?}");
        assert!(v.warnings.iter().any(|w| w.contains("recommended to include internal overviews")));
        // Small, tiled, and laid out as a GeoTIFF: overviews after the full resolution.
        let mut o = driver(GTiff, &[("TILED", "YES")]);
        o.overviews = vec![2, 4];
        let (_d, p) = write(&raster(1300, 700), &o);
        let v = validate_cog(&p, false).unwrap();
        assert!(!v.is_valid, "{v:?}");
    }

    #[test]
    fn small_files_and_strictness() {
        use crate::formats::RasterDriver::GTiff;
        // Small strips are fine; a large tiled file without overviews only warns.
        let (_d, p) = write(&raster(100, 80), &driver(GTiff, &[]));
        assert!(validate_cog(&p, false).unwrap().errors.iter().all(|e| !e.contains("tiled")));
        let o = driver(crate::formats::RasterDriver::Cog, &[]);
        let (_d, p) = write(&raster(1300, 700), &crate::RasterWriteOptions { tile_size: Some(2048), ..o });
        let v = validate_cog(&p, false).unwrap();
        assert!(v.is_valid, "{v:?}");
        assert_eq!(v.warnings.len(), 1);
        assert!(!validate_cog(&p, true).unwrap().is_valid);
    }

    #[test]
    fn external_overviews_are_reported_and_used() {
        use crate::formats::RasterDriver::{Cog, GTiff};
        let (dir, p) = write(&raster(1300, 700), &driver(Cog, &[]));
        // A COG beside a sidecar holding another COG's images: internal overviews still win.
        std::fs::copy(&p, format!("{p}.ovr")).unwrap();
        let v = validate_cog(&p, false).unwrap();
        assert_eq!(v.errors, vec!["Overviews found in external .ovr file. They should be internal".to_string()]);
        // A tiled GeoTIFF without internal overviews takes the sidecar's.
        let tiled = dir.path().join("tiled.tif").to_string_lossy().into_owned();
        crate::write_raster(&raster(1300, 700), &tiled, &crate::RasterWriteOptions { tile_size: Some(256), ..driver(GTiff, &[]) }).unwrap();
        std::fs::copy(&p, format!("{tiled}.ovr")).unwrap();
        let v = validate_cog(&tiled, false).unwrap();
        // The sidecar's full-resolution IFD is not an overview: only its two reduced images are.
        assert_eq!(v.overview_factors, vec![2, 4], "{v:?}");
        assert!(!v.errors.iter().any(|e| e.contains("Invalid Decimation")), "{v:?}");
        assert!(v.warnings.is_empty());
    }

    #[test]
    fn terrio_masks_need_no_layout_notes() {
        use crate::formats::RasterDriver::GTiff;
        let mut o = driver(GTiff, &[("TILED", "YES")]);
        o.mask = Some(vec![1; 600 * 520]);
        let (_d, p) = write(&raster(600, 520), &o);
        let v = validate_cog(&p, true).unwrap();
        // terrio's GeoTIFF writer puts IFDs first: nothing to note.
        assert!(v.layout_notes.is_empty(), "{v:?}");
    }

    /// A little-endian classic TIFF with one IFD at byte 8 of `(tag, type, count, value)`
    /// entries, followed by `data`, which starts at byte `10 + 12 * entries + 4`.
    fn crafted(entries: &[(u16, u16, u32, u32)], data: &[u8]) -> Vec<u8> {
        let mut b = b"II*\0".to_vec();
        b.extend(8u32.to_le_bytes());
        b.extend((entries.len() as u16).to_le_bytes());
        for (tag, typ, count, value) in entries {
            b.extend(tag.to_le_bytes());
            b.extend(typ.to_le_bytes());
            b.extend(count.to_le_bytes());
            b.extend(value.to_le_bytes());
        }
        b.extend(0u32.to_le_bytes());
        b.extend(data);
        b
    }

    fn validate_bytes(bytes: &[u8]) -> Result<CogValidation> {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("crafted.tif");
        std::fs::write(&p, bytes).unwrap();
        validate_cog(p.to_str().unwrap(), false)
    }

    #[test]
    fn crafted_grids_are_searched_by_their_stored_offsets() {
        // A 2^31 x 2^31 image of 1 x 1 tiles (about 2^62 blocks) with four offsets: the search
        // walks the four offsets, not the grid.
        let data_at = 10 + 12 * 6 + 4;
        let entries = |offsets: [u32; 4]| {
            let mut data: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
            data.extend([1u32; 4].iter().flat_map(|c| c.to_le_bytes()));
            let e = [
                (256, 4, 1, 1 << 31),
                (257, 4, 1, 1 << 31),
                (322, 3, 1, 1),
                (323, 3, 1, 1),
                (324, 4, 4, data_at),
                (325, 4, 4, data_at + 16),
            ];
            crafted(&e, &data)
        };
        let v = validate_bytes(&entries([0; 4])).unwrap();
        assert!(v.data_offsets.is_empty(), "{v:?}");
        // The first stored block in row order: (2, 0).
        let v = validate_bytes(&entries([0, 0, 500, 400])).unwrap();
        assert_eq!(v.data_offsets, vec![500]);
    }

    #[test]
    fn tag_arrays_past_the_end_are_rejected_before_reading() {
        // 4 Mi offsets claimed at byte 1,000,000 of a file of about a hundred bytes.
        let e = [(256, 4, 1, 1024), (257, 4, 1, 1024), (322, 3, 1, 256), (323, 3, 1, 256), (324, 4, 1 << 22, 1_000_000)];
        let err = validate_bytes(&crafted(&e, &[])).unwrap_err();
        assert!(err.to_string().contains("past the end of the file"), "{err}");
        // More values than any image needs.
        let e = [(256, 4, 1, 1024), (324, 4, (1 << 22) + 1, 100)];
        assert!(validate_bytes(&crafted(&e, &[])).is_err());
    }

    /// A little-endian classic TIFF of `images` image directories after two arrays of 64 LONGs
    /// (offsets at byte 8, byte counts at byte 264) that every directory points to.
    fn sharing_arrays(images: u32) -> Vec<u8> {
        let (offsets, counts, first, ifd_len): (u32, u32, u32, u32) = (8, 8 + 256, 8 + 512, 2 + 6 * 12 + 4);
        let mut b = b"II*\0".to_vec();
        b.extend(first.to_le_bytes());
        b.extend((0..64u32).flat_map(|i| (first + images * ifd_len + i).to_le_bytes()));
        b.extend((0..64u32).flat_map(|_| 1u32.to_le_bytes()));
        for i in 0..images {
            let entries: [(u16, u16, u32, u32); 6] =
                [(256, 4, 1, 2048), (257, 4, 1, 2048), (322, 3, 1, 256), (323, 3, 1, 256), (324, 4, 64, offsets), (325, 4, 64, counts)];
            b.extend(6u16.to_le_bytes());
            for (tag, typ, count, value) in entries {
                b.extend(tag.to_le_bytes());
                b.extend(typ.to_le_bytes());
                b.extend(count.to_le_bytes());
                b.extend(value.to_le_bytes());
            }
            let next = if i + 1 < images { first + (i + 1) * ifd_len } else { 0 };
            b.extend(next.to_le_bytes());
        }
        b.extend([0u8; 64]);
        b
    }

    #[test]
    fn tag_arrays_shared_by_images_are_refused_before_reading() {
        // One directory: its two arrays (512 bytes) fit in the file (662 bytes).
        let one = sharing_arrays(1);
        assert_eq!(one.len(), 662);
        let v = validate_bytes(&one).unwrap();
        assert_eq!(v.data_offsets, vec![598], "{v:?}");
        // Two directories pointing to the same arrays: 1,024 bytes of arrays in a file of 740.
        let two = sharing_arrays(2);
        assert_eq!(two.len(), 740);
        let err = validate_bytes(&two).unwrap_err();
        assert_eq!(err.to_string(), "the TIFF's tag arrays take more than its 740 bytes");
    }

    #[test]
    fn metadata_is_streamed_bounded_and_ignored_by_legacy_profile() {
        let large = crafted(&[(256, 4, 1, 1), (257, 4, 1, 1), (42112, 2, (1 << 22) + 1, 100)], &[]);
        assert!(validate_cog_bytes(&large, &Default::default()).is_ok());
        let options = CogValidationOptions { profile: CogValidationProfile::Gdal, full_check: Some(false), ..Default::default() };
        assert!(validate_cog_bytes(&large, &options).is_ok());
        let full = CogValidationOptions { full_check: Some(true), ..options };
        assert!(validate_cog_bytes(&large, &full).unwrap_err().to_string().contains("4 MiB"));
        assert!(tile_interleave(br#"<GDALMetadata><Item name="INTERLEAVE" domain="IMAGE_STRUCTURE">TILE</Item></GDALMetadata>"#).unwrap());
        assert!(tile_interleave("<a>".repeat(100_000).as_bytes()).unwrap_err().to_string().contains("128 nesting"));
    }

    #[test]
    fn mask_notes_name_the_image_they_mask() {
        let image = |offset, width, subfile_type, data| Ifd {
            offset,
            width,
            height: width,
            subfile_type,
            samples: 1,
            tile: Some((256, 256)),
            rows_per_strip: u64::MAX,
            offsets: vec![data; (width as usize).div_ceil(256).pow(2)],
            byte_counts: vec![1; (width as usize).div_ceil(256).pow(2)],
            ..Default::default()
        };
        // IFDs of the main image and its overview, then image data, then the masks' IFDs: the
        // main image's mask first, as GDAL orders them.
        let ifds = vec![image(16, 1024, 0, 9000), image(200, 512, 1, 8000), image(10_000, 1024, 4, 7500), image(10_200, 512, 5, 7000)];
        let v = validate(Some(Tiff { ifds, ghost: None, expected_ifd_offset: 8 }), None, false, false);
        assert_eq!(
            v.layout_notes,
            vec![
                "The IFD of the mask of the main image is at byte 10000, after image data".to_string(),
                "The IFD of the mask of overview 0 is at byte 10200, after image data".to_string(),
            ]
        );
    }

    #[test]
    fn gdal_full_checks_detect_corrupted_leaders_trailers_and_row_order() {
        let (_dir, path) = write(&raster(600, 600), &driver(crate::formats::RasterDriver::Cog, &[("COMPRESS", "DEFLATE")]));
        let bytes = std::fs::read(path).unwrap();
        let options = CogValidationOptions { profile: CogValidationProfile::Gdal, ..Default::default() };
        assert!(validate_cog_bytes(&bytes, &options).unwrap().is_valid);
        let tiff = read_tiff(&mut std::io::Cursor::new(&bytes), "t.tif", true).unwrap().unwrap();
        let mut cursor = std::io::Cursor::new(&bytes);
        let error = full_check(&mut BoundaryReader { inner: &mut cursor, remaining: 1 }, &tiff, &mut Vec::new()).unwrap_err();
        assert!(error.to_string().contains("boundary reads"));
        let image = &tiff.ifds[0];
        let offset = image.offsets[0] as usize;
        let count = image.byte_counts[0] as usize;
        for (position, message) in [(offset - 4, "size in leader"), (offset + count, "trailer bytes")] {
            let mut damaged = bytes.clone();
            damaged[position] ^= 1;
            let result = validate_cog_bytes(&damaged, &options).unwrap();
            assert!(!result.is_valid, "{result:?}");
            assert!(result.errors.iter().any(|error| error.contains(message)), "{result:?}");
            assert!(validate_cog_bytes(&damaged, &CogValidationOptions { full_check: Some(false), ..options.clone() }).unwrap().is_valid);
            assert!(validate_cog_bytes(&damaged, &Default::default()).unwrap().is_valid);
        }
        let mut tiff = tiff;
        tiff.ifds[0].offsets.swap(0, 1);
        let mut errors = Vec::new();
        full_check(&mut std::io::Cursor::new(&bytes), &tiff, &mut errors).unwrap();
        assert!(errors.iter().any(|error| error.contains("smaller than previous")), "{errors:?}");
    }
    #[test]
    fn gdal_profile_retains_its_distinct_small_strip_threshold_and_exact_ifd_rule() {
        let (_dir, path) = write(&raster(800, 600), &Default::default());
        let bytes = std::fs::read(path).unwrap();
        let options = CogValidationOptions { profile: CogValidationProfile::Gdal, ..Default::default() };
        let result = validate_cog_bytes(&bytes, &options).unwrap();
        assert!(!result.errors.iter().any(|error| error.contains("not tiled")), "{result:?}");
        assert!(validate_cog_bytes(&bytes, &Default::default()).unwrap().errors.iter().any(|error| error.contains("not tiled")));
        let mut tiff = read_tiff(&mut std::io::Cursor::new(&bytes), "t.tif", true).unwrap().unwrap();
        tiff.ifds[0].offset = 10;
        assert!(validate(Some(tiff), None, false, true).errors.iter().any(|error| error.contains("should be 8")));
        assert!(validate_cog_bytes(&bytes, &CogValidationOptions { full_check: Some(true), ..Default::default() }).is_err());
    }
    #[test]
    fn full_check_with_structural_metadata_but_no_ifd_is_invalid() {
        let ghost = "BLOCK_ORDER=ROW_MAJOR\n";
        let header = format!("GDAL_STRUCTURAL_METADATA_SIZE={:06} bytes\n", ghost.len());
        let bytes = [b"II*\0\0\0\0\0".as_slice(), header.as_bytes(), ghost.as_bytes()].concat();
        let options = CogValidationOptions { profile: CogValidationProfile::Gdal, ..Default::default() };
        assert!(!validate_cog_bytes(&bytes, &options).unwrap().is_valid);
    }
    #[test]
    fn full_block_findings_are_bounded_for_uploaded_and_local_inputs() {
        let image = Ifd {
            width: 5000,
            height: 1,
            samples: 1,
            planar: 1,
            tile: Some((1, 1)),
            offsets: vec![16; 5000],
            byte_counts: vec![8; 5000],
            ..Default::default()
        };
        let tiff = Tiff { ifds: vec![image], ghost: Some("BLOCK_LEADER=SIZE_AS_UINT4".into()), expected_ifd_offset: 8 };
        let mut errors = Vec::new();
        full_check(&mut std::io::Cursor::new(vec![0; 32]), &tiff, &mut errors).unwrap();
        assert_eq!(errors.len(), MAX_BLOCK_FINDINGS + 1);
        assert_eq!(errors.last().unwrap(), BLOCK_FINDING_LIMIT);
    }
    #[test]
    fn ascii_numeric_tags_are_ignored_and_ghost_offsets_use_raw_bytes() {
        let ascii = crafted(&[(256, 4, 1, 20), (257, 4, 1, 20), (322, 2, 1, 65), (323, 3, 1, 16)], &[]);
        assert!(read_tiff(&mut std::io::Cursor::new(ascii), "t.tif", false).unwrap().unwrap().ifds[0].tile.is_none());
        let original = crafted(&[(256, 4, 1, 1), (257, 4, 1, 1)], &[]);
        let ghost = b"GDAL_STRUCTURAL_METADATA_SIZE=000001 bytes\n\xff";
        let offset = (8 + ghost.len()).next_multiple_of(2);
        let mut bytes = original[..8].to_vec();
        bytes[4..8].copy_from_slice(&(offset as u32).to_le_bytes());
        bytes.extend_from_slice(ghost);
        bytes.resize(offset, 0);
        bytes.extend_from_slice(&original[8..]);
        let tiff = read_tiff(&mut std::io::Cursor::new(bytes), "t.tif", true).unwrap().unwrap();
        assert_eq!(tiff.expected_ifd_offset, offset as u64);
    }
    #[test]
    fn gdal_sparse_blocks_reset_row_order_state() {
        let image =
            Ifd { width: 3, height: 1, tile: Some((1, 1)), offsets: vec![16, 0, 8], byte_counts: vec![1, 0, 1], ..Default::default() };
        let mut errors = Vec::new();
        let mut last = 0;
        check_blocks(
            &mut std::io::Cursor::new(vec![0; 32]),
            32,
            &image,
            0..3,
            "Main resolution image",
            None,
            (true, false, false),
            &mut last,
            &mut errors,
        )
        .unwrap();
        assert!(errors.is_empty(), "GDAL resets on the empty middle block: {errors:?}");
    }
    #[test]
    fn repeated_overview_sizes_pair_masks_by_sequence() {
        let main = Ifd {
            width: 4,
            height: 1,
            tile: Some((1, 1)),
            samples: 1,
            planar: 1,
            offsets: vec![16, 20, 24, 28],
            byte_counts: vec![1; 4],
            ..Default::default()
        };
        let overview = Ifd { width: 1, height: 1, subfile_type: 1, offsets: vec![40], byte_counts: vec![1], ..main.clone() };
        let second = Ifd { offsets: vec![80], ..overview.clone() };
        let mask = Ifd { subfile_type: 4, offsets: vec![17, 21, 25, 29], ..main.clone() };
        let first_mask = Ifd { subfile_type: 5, offsets: vec![41], ..overview.clone() };
        let second_mask = Ifd { offsets: vec![81], ..first_mask.clone() };
        let tiff = Tiff {
            ifds: vec![main, overview, second, mask, first_mask, second_mask],
            ghost: Some("MASK_INTERLEAVED_WITH_IMAGERY=YES".into()),
            expected_ifd_offset: 8,
        };
        let mut errors = Vec::new();
        full_check(&mut std::io::Cursor::new(vec![0; 100]), &tiff, &mut errors).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn unrelated_reduced_image_does_not_shift_overview_mask_assignment() {
        let main = Ifd {
            width: 4,
            height: 1,
            tile: Some((1, 1)),
            samples: 1,
            planar: 1,
            offsets: vec![16, 20, 24, 28],
            byte_counts: vec![1; 4],
            ..Default::default()
        };
        let unrelated = Ifd { width: 2, samples: 2, subfile_type: 1, offsets: vec![32; 2], byte_counts: vec![1; 2], ..main.clone() };
        let overview = Ifd { width: 1, subfile_type: 1, offsets: vec![40], byte_counts: vec![1], ..main.clone() };
        let unrelated_mask = Ifd { width: 2, subfile_type: 5, offsets: vec![33, 34], byte_counts: vec![1; 2], ..main.clone() };
        let mask = Ifd { subfile_type: 5, offsets: vec![45], ..overview.clone() };
        let tiff = Tiff {
            ifds: vec![main, unrelated, overview, unrelated_mask, mask],
            ghost: Some("MASK_INTERLEAVED_WITH_IMAGERY=YES".into()),
            expected_ifd_offset: 8,
        };
        let mut errors = Vec::new();
        full_check(&mut std::io::Cursor::new(vec![0; 100]), &tiff, &mut errors).unwrap();
        assert!(errors.iter().any(|e| e.contains("Mask of Overview 0") && e.contains("unexpected interleaved offset 45")), "{errors:?}");
    }

    #[test]
    fn full_check_rejects_truncated_mask_block_arrays() {
        let main = Ifd {
            width: 1,
            height: 1,
            tile: Some((1, 1)),
            samples: 1,
            planar: 1,
            offsets: vec![16],
            byte_counts: vec![1],
            ..Default::default()
        };
        let mask = Ifd { subfile_type: 4, offsets: vec![], byte_counts: vec![], ..main.clone() };
        let tiff = Tiff { ifds: vec![main, mask], ghost: Some("MASK_INTERLEAVED_WITH_IMAGERY=YES".into()), expected_ifd_offset: 8 };
        let mut errors = Vec::new();
        full_check(&mut std::io::Cursor::new(vec![0; 32]), &tiff, &mut errors).unwrap();
        assert!(errors.iter().any(|e| e.to_ascii_lowercase().contains("mask") && e.contains("arrays")), "{errors:?}");
    }

    #[test]
    fn zero_block_grid_does_not_iterate_unbounded_sample_planes() {
        let tiff = Tiff {
            ifds: vec![Ifd { width: 0, height: 1, tile: Some((1, 1)), planar: 2, samples: u64::MAX, ..Default::default() }],
            ghost: Some("BLOCK_ORDER=ROW_MAJOR".into()),
            expected_ifd_offset: 8,
        };
        let mut errors = Vec::new();
        full_check(&mut std::io::Cursor::new(Vec::<u8>::new()), &tiff, &mut errors).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
    }

    #[test]
    fn invalid_full_check_options_are_rejected_before_opening_inputs() {
        let options = CogValidationOptions { profile: CogValidationProfile::RioCogeo, full_check: Some(true), ..Default::default() };
        for path in ["http://127.0.0.1:1/missing.tif", "/nonexistent/terrio-cog-option-test.tif"] {
            assert!(matches!(validate_cog_with_options(path, &options), Err(Error::InvalidArgument(_))));
        }
    }

    #[test]
    fn full_metadata_rejects_malformed_xml_after_a_valid_item() {
        let xml = br#"<GDALMetadata><Item name="INTERLEAVE" domain="IMAGE_STRUCTURE">TILE</Item></GDALMetadata><broken"#;
        assert!(tile_interleave(xml).is_err());
        for invalid in [
            br#"<GDALMetadata><Item name="OTHER">&undefined;</Item></GDALMetadata>"#.as_slice(),
            b"",
            b"plain text",
            b"<!-- no root -->",
            b"<a/><b/>",
            b"<a/>tail",
            b"<![CDATA[text]]>",
        ] {
            assert!(tile_interleave(invalid).is_err(), "{invalid:?}");
        }
        assert!(!tile_interleave(b"<!-- comment --><GDALMetadata/> ").unwrap());
        assert!(!tile_interleave(b"<GDALMetadata><Item name='OTHER'>&amp;&#65;</Item></GDALMetadata>").unwrap());
        assert!(tile_interleave(br#"<GDALMetadata><Item bad></Item></GDALMetadata>"#).is_err());
        assert!(
            tile_interleave(br#"<GDALMetadata><Item name="INTERLEAVE" domain="IMAGE_STRUCTURE">TI&#76;E</Item></GDALMetadata>"#).unwrap()
        );
    }

    #[test]
    fn local_full_checks_read_only_boundary_bytes_without_buffer_refills() {
        struct Measured {
            file: std::fs::File,
            bytes: usize,
        }
        impl Read for Measured {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let n = self.file.read(buffer)?;
                self.bytes += n;
                Ok(n)
            }
        }
        impl Seek for Measured {
            fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                self.file.seek(position)
            }
        }
        let (_dir, path) =
            write(&raster(2048, 2048), &driver(crate::formats::RasterDriver::Cog, &[("COMPRESS", "DEFLATE"), ("BLOCKSIZE", "128")]));
        let options = CogValidationOptions { profile: CogValidationProfile::Gdal, ..Default::default() };
        let mut buffered = std::io::BufReader::new(Measured { file: std::fs::File::open(&path).unwrap(), bytes: 0 });
        let reference = validate_reader(&mut buffered, "t.tif", None, &options, true).unwrap();
        let buffered_bytes = buffered.get_ref().bytes;
        let mut direct = Measured { file: std::fs::File::open(&path).unwrap(), bytes: 0 };
        let actual = validate_reader(&mut direct, "t.tif", None, &options, true).unwrap();
        assert!(actual.is_valid, "{actual:?}");
        assert_eq!(actual, reference);
        assert!(direct.bytes < buffered_bytes);
        assert_eq!(validate_cog_with_options(&path, &options).unwrap(), actual);
        println!("local COG full validation: one source open per call, buffered={buffered_bytes} bytes, direct={} bytes", direct.bytes);
    }
    #[test]
    fn not_a_tiff() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.tif");
        std::fs::write(&p, b"not a tiff at all").unwrap();
        let v = validate_cog(p.to_str().unwrap(), false).unwrap();
        assert!(!v.is_valid);
        assert_eq!(v.errors, vec!["The file is not a GeoTIFF".to_string()]);
        assert!(validate_cog(dir.path().join("missing.tif").to_str().unwrap(), false).is_err());
    }
}

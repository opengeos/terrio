//! JPEG 2000 (`.jp2`, `.j2k`), coded with `openjp2`, a pure-Rust port of OpenJPEG (the codec
//! behind GDAL's JP2OpenJPEG driver), so decoded pixels match GDAL's, signed samples included, in
//! native and WebAssembly builds alike.
//!
//! Georeferencing is read from the GeoJP2 box (a small GeoTIFF) or, failing that, from the GML
//! (GMLJP2) box, as GDAL reads them; it is written as a GeoJP2 box. A 1-bit alpha channel of 8-bit
//! colours is promoted to 0 and 255, as GDAL does. Nodata goes to a `.aux.xml` sidecar, where GDAL
//! keeps it for JPEG 2000. Files are written losslessly (the reversible 5/3 wavelet), with GDAL's
//! codestream comment.
//!
//! The port is also built as a C library, whose C allocator calls `wasm32-unknown-unknown` has no
//! libc to answer: terrio defines them there (`wasm_libc`). The port releases null pointers with
//! Rust's allocator too, which [`crate::NullSafeAllocator`] tolerates.

use super::common;
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use codec::{Component, decode, encode};
use std::path::Path;
use wbraster::raster::RasterData;
use wbraster::{CrsInfo, DataType, Raster};

/// The UUID of the GeoJP2 box.
const GEOJP2_UUID: [u8; 16] = [0xb1, 0x4b, 0xf8, 0xbd, 0x08, 0x3d, 0x4b, 0x43, 0xa5, 0xae, 0x8c, 0xd7, 0xd5, 0xa6, 0xce, 0x03];

/// The codec: OpenJPEG's Rust port, driven through its C-style API over in-memory streams.
mod codec {
    use crate::{Error, Result};
    use core::ffi::c_void;
    use openjp2::openjpeg::*;
    use wbraster::raster::RasterData;

    unsafe extern "C" {
        /// C's allocator, which OpenJPEG's port releases its parameters' strings with (terrio
        /// supplies it in WebAssembly).
        fn malloc(size: usize) -> *mut c_void;
    }

    /// An in-memory buffer that OpenJPEG streams write to.
    struct Buffer {
        data: Vec<u8>,
        pos: usize,
    }

    /// The bytes OpenJPEG streams read from.
    struct Source<'a> {
        data: &'a [u8],
        pos: usize,
    }

    unsafe extern "C" fn read_fn(dst: *mut c_void, n: usize, user: *mut c_void) -> usize {
        // SAFETY: `user` is the `Source` registered with the stream, alive while the stream is.
        let b = unsafe { &mut *(user as *mut Source) };
        let left = b.data.len().saturating_sub(b.pos);
        if left == 0 || dst.is_null() {
            return usize::MAX; // end of stream
        }
        let k = left.min(n);
        // SAFETY: OpenJPEG passes a buffer of at least `n` bytes.
        unsafe { std::ptr::copy_nonoverlapping(b.data.as_ptr().add(b.pos), dst as *mut u8, k) };
        b.pos += k;
        k
    }

    unsafe extern "C" fn read_skip_fn(n: i64, user: *mut c_void) -> i64 {
        // SAFETY: as in `read_fn`.
        let b = unsafe { &mut *(user as *mut Source) };
        b.pos = (b.pos as i64 + n).max(0) as usize;
        n
    }

    unsafe extern "C" fn read_seek_fn(n: i64, user: *mut c_void) -> i32 {
        // SAFETY: as in `read_fn`.
        let b = unsafe { &mut *(user as *mut Source) };
        b.pos = n.max(0) as usize;
        1
    }

    unsafe extern "C" fn write_fn(src: *mut c_void, n: usize, user: *mut c_void) -> usize {
        // SAFETY: `user` is the `Buffer` registered with the stream, alive while the stream is;
        // `src` holds `n` bytes.
        let b = unsafe { &mut *(user as *mut Buffer) };
        let bytes = unsafe { std::slice::from_raw_parts(src as *const u8, n) };
        let end = b.pos + n;
        if b.data.len() < end {
            b.data.resize(end, 0);
        }
        b.data[b.pos..end].copy_from_slice(bytes);
        b.pos = end;
        n
    }

    unsafe extern "C" fn skip_fn(n: i64, user: *mut c_void) -> i64 {
        // SAFETY: as in `write_fn`.
        let b = unsafe { &mut *(user as *mut Buffer) };
        let new = (b.pos as i64 + n).max(0) as usize;
        b.pos = new;
        n
    }

    unsafe extern "C" fn seek_fn(n: i64, user: *mut c_void) -> i32 {
        // SAFETY: as in `write_fn`.
        let b = unsafe { &mut *(user as *mut Buffer) };
        b.pos = n.max(0) as usize;
        1
    }

    /// One component to encode.
    pub(super) struct Component {
        pub precision: u32,
        pub signed: bool,
        pub data: Vec<i32>,
    }

    /// Copies the components into one buffer, band after band, of the type their precision and
    /// signedness need.
    fn collect(comps: &[&[i32]], precision: u32, signed: bool) -> RasterData {
        fn gather<T>(comps: &[&[i32]], f: impl Fn(i32) -> T) -> Vec<T> {
            let mut out = Vec::with_capacity(comps.iter().map(|c| c.len()).sum());
            for c in comps {
                out.extend(c.iter().map(|&v| f(v)));
            }
            out
        }
        match (precision, signed) {
            (0..=8, false) => RasterData::U8(gather(comps, |v| v as u8)),
            (0..=8, true) => RasterData::I8(gather(comps, |v| v as i8)),
            (9..=16, false) => RasterData::U16(gather(comps, |v| v as u16)),
            (9..=16, true) => RasterData::I16(gather(comps, |v| v as i16)),
            (_, false) => RasterData::U32(gather(comps, |v| v as u32)),
            (_, true) => RasterData::I32(gather(comps, |v| v)),
        }
    }

    /// Decodes a JP2 file or a J2K codestream: `(width, height, pixels band after band)`, the
    /// pixels of the type the widest component needs. A 1-bit alpha component (`alpha`, its
    /// index) of 8-bit colours is promoted to 8 bits (0 or 255), as GDAL does.
    pub(super) fn decode(bytes: &[u8], alpha: Option<usize>) -> Result<(usize, usize, RasterData)> {
        let format = if bytes.starts_with(openjp2::J2K_CODESTREAM_MAGIC) { OPJ_CODEC_J2K } else { OPJ_CODEC_JP2 };
        let mut source = Source { data: bytes, pos: 0 };
        // SAFETY: the stream, codec and image are created, used and destroyed here; the source
        // outlives the stream.
        unsafe {
            let stream = opj_stream_default_create(1);
            opj_stream_set_read_function(stream, Some(read_fn));
            opj_stream_set_skip_function(stream, Some(read_skip_fn));
            opj_stream_set_seek_function(stream, Some(read_seek_fn));
            opj_stream_set_user_data_length(stream, bytes.len() as u64);
            opj_stream_set_user_data(stream, &mut source as *mut Source as *mut c_void, None);
            let codec = opj_create_decompress(format);
            let mut params = opj_dparameters_t::default();
            let mut image: *mut opj_image_t = std::ptr::null_mut();
            let ok = !codec.is_null()
                && opj_setup_decoder(codec, &mut params) != 0
                && opj_read_header(stream, codec, &mut image) != 0
                && opj_decode(codec, stream, image) != 0
                && opj_end_decompress(codec, stream) != 0;
            let result = if ok && !image.is_null() {
                let img = &*image;
                let comps = std::slice::from_raw_parts(img.comps, img.numcomps as usize);
                let (w, h) = (comps[0].w as usize, comps[0].h as usize);
                if comps.iter().any(|c| c.w as usize != w || c.h as usize != h || c.data.is_null()) {
                    Err(Error::Unsupported("JPEG 2000 components with different sizes are not supported".into()))
                } else {
                    let mut planes: Vec<&[i32]> = comps.iter().map(|c| std::slice::from_raw_parts(c.data, w * h)).collect();
                    let mut precisions: Vec<u32> = comps.iter().map(|c| c.prec).collect();
                    let promoted: Vec<i32>;
                    if let Some(a) = alpha.filter(|&a| a < comps.len()) {
                        let colour = if a == 0 && comps.len() > 1 { 1 } else { 0 };
                        if precisions[a] == 1 && precisions[colour] == 8 {
                            promoted = planes[a].iter().map(|v| v * 255).collect();
                            planes[a] = &promoted;
                            precisions[a] = 8;
                        }
                    }
                    let precision = precisions.iter().copied().max().unwrap_or(8);
                    let signed = comps[0].sgnd != 0;
                    Ok((w, h, collect(&planes, precision, signed)))
                }
            } else {
                Err(Error::InvalidData("cannot decode the JPEG 2000 codestream".into()))
            };
            if !image.is_null() {
                opj_image_destroy(image);
            }
            if !codec.is_null() {
                opj_destroy_codec(codec);
            }
            opj_stream_destroy(stream);
            result
        }
    }

    /// Encodes components as a lossless J2K codestream.
    pub(super) fn encode(w: usize, h: usize, comps: &[Component]) -> Result<Vec<u8>> {
        let mut parms: Vec<openjp2::opj_image_comptparm> = comps
            .iter()
            .map(|c| openjp2::opj_image_comptparm {
                dx: 1,
                dy: 1,
                w: w as u32,
                h: h as u32,
                x0: 0,
                y0: 0,
                prec: c.precision,
                bpp: c.precision,
                sgnd: c.signed as u32,
            })
            .collect();
        let color = if comps.len() == 3 || comps.len() == 4 { OPJ_CLRSPC_SRGB } else { OPJ_CLRSPC_GRAY };
        let buffer = Box::into_raw(Box::new(Buffer { data: Vec::new(), pos: 0 }));
        // SAFETY: as in `decode`.
        unsafe {
            let image = opj_image_create(parms.len() as u32, parms.as_mut_ptr(), color);
            if image.is_null() {
                drop(Box::from_raw(buffer));
                return Err(Error::InvalidData("cannot create the JPEG 2000 image".into()));
            }
            let img = &mut *image;
            img.x0 = 0;
            img.y0 = 0;
            img.x1 = w as u32;
            img.y1 = h as u32;
            for (dst, src) in std::slice::from_raw_parts_mut(img.comps, img.numcomps as usize).iter_mut().zip(comps) {
                std::slice::from_raw_parts_mut(dst.data, w * h).copy_from_slice(&src.data);
            }
            let mut params = opj_cparameters_t::default();
            opj_set_default_encoder_parameters(&mut params);
            params.tcp_numlayers = 1;
            params.tcp_rates[0] = 0.0;
            params.cp_disto_alloc = 1;
            params.irreversible = 0;
            // GDAL's comment. The parameters own it and release it with C's `free`. (Without one,
            // the port makes its own with Rust's allocator and releases it with C's, which traps
            // in WebAssembly.)
            let comment = format!("Created by OpenJPEG version {OPJ_VERSION}. LOSSLESS settings used\0");
            let copy = malloc(comment.len()) as *mut u8;
            if !copy.is_null() {
                std::ptr::copy_nonoverlapping(comment.as_ptr(), copy, comment.len());
                params.cp_comment = copy as *mut core::ffi::c_char;
            }
            // As many resolution levels as the image allows, up to OpenJPEG's default of 6.
            let mut levels = 6;
            while levels > 1 && (w.min(h) >> (levels - 1)) == 0 {
                levels -= 1;
            }
            params.numresolution = levels;
            params.tcp_mct = (comps.len() >= 3 && comps[..3].iter().all(|c| c.precision == comps[0].precision)) as core::ffi::c_char;
            if w > 1024 || h > 1024 {
                params.tile_size_on = 1;
                params.cp_tdx = 1024;
                params.cp_tdy = 1024;
            }
            let codec = opj_create_compress(OPJ_CODEC_J2K);
            let stream = opj_stream_default_create(0);
            opj_stream_set_write_function(stream, Some(write_fn));
            opj_stream_set_skip_function(stream, Some(skip_fn));
            opj_stream_set_seek_function(stream, Some(seek_fn));
            opj_stream_set_user_data(stream, buffer as *mut c_void, None);
            let ok = !codec.is_null()
                && opj_setup_encoder(codec, &mut params, image) != 0
                && opj_start_compress(codec, image, stream) != 0
                && opj_encode(codec, stream) != 0
                && opj_end_compress(codec, stream) != 0;
            if !codec.is_null() {
                opj_destroy_codec(codec);
            }
            opj_stream_destroy(stream);
            opj_image_destroy(image);
            let data = Box::from_raw(buffer).data;
            if ok { Ok(data) } else { Err(Error::InvalidData("JPEG 2000 encoding failed".into())) }
        }
    }
}

/// The C library functions OpenJPEG's port calls that `wasm32-unknown-unknown` has no libc to
/// supply: the allocator (`malloc`, `calloc`, `realloc`, `free`), served by Rust's allocator with
/// each block led by a header holding its size, and `strcpy`. (The compiler's builtins supply
/// `memcpy`, `memset` and the maths functions.)
#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
mod wasm_libc {
    use core::ffi::c_void;
    use std::alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc as re_alloc};

    /// The header size and the alignment of every block, as C's `max_align_t`.
    const HEADER: usize = 16;

    fn layout(size: usize) -> Option<Layout> {
        Layout::from_size_align(size.checked_add(HEADER)?, HEADER).ok()
    }

    /// Records the size in the header of a fresh block and returns the caller's pointer.
    ///
    /// # Safety
    /// `base` is null or a block of at least `HEADER + size` bytes.
    unsafe fn finish(base: *mut u8, size: usize) -> *mut c_void {
        if base.is_null() {
            return std::ptr::null_mut();
        }
        // SAFETY: the block starts with `HEADER` bytes, aligned for a `usize`.
        unsafe {
            (base as *mut usize).write(size);
            base.add(HEADER) as *mut c_void
        }
    }

    /// The block start and size of a pointer returned by `malloc`.
    ///
    /// # Safety
    /// `ptr` came from this allocator and was not freed.
    unsafe fn block(ptr: *mut c_void) -> (*mut u8, usize) {
        // SAFETY: as documented.
        unsafe {
            let base = (ptr as *mut u8).sub(HEADER);
            (base, (base as *const usize).read())
        }
    }

    #[unsafe(no_mangle)]
    extern "C" fn malloc(size: usize) -> *mut c_void {
        // SAFETY: the layout is not zero-sized, and the block holds the header and `size` bytes.
        layout(size).map_or(std::ptr::null_mut(), |l| unsafe { finish(alloc(l), size) })
    }

    #[unsafe(no_mangle)]
    extern "C" fn calloc(n: usize, size: usize) -> *mut c_void {
        let Some(total) = n.checked_mul(size) else { return std::ptr::null_mut() };
        // SAFETY: as in `malloc`.
        layout(total).map_or(std::ptr::null_mut(), |l| unsafe { finish(alloc_zeroed(l), total) })
    }

    /// # Safety
    /// `ptr` is null or came from this allocator and was not freed.
    #[unsafe(no_mangle)]
    unsafe extern "C" fn realloc(ptr: *mut c_void, size: usize) -> *mut c_void {
        if ptr.is_null() {
            return malloc(size);
        }
        let Some(new) = layout(size) else { return std::ptr::null_mut() };
        // SAFETY: `ptr` is a live block of this allocator, with its layout rebuilt from the header.
        unsafe {
            let (base, old) = block(ptr);
            let Some(l) = layout(old) else { return std::ptr::null_mut() };
            finish(re_alloc(base, l, new.size()), size)
        }
    }

    /// # Safety
    /// `ptr` is null or came from this allocator and was not freed.
    #[unsafe(no_mangle)]
    unsafe extern "C" fn free(ptr: *mut c_void) {
        if ptr.is_null() {
            return;
        }
        // SAFETY: as in `realloc`.
        unsafe {
            let (base, size) = block(ptr);
            if let Some(l) = layout(size) {
                dealloc(base, l);
            }
        }
    }

    /// # Safety
    /// `src` is a NUL-terminated string and `dst` holds at least as many bytes.
    #[unsafe(no_mangle)]
    unsafe extern "C" fn strcpy(dst: *mut core::ffi::c_char, src: *const core::ffi::c_char) -> *mut core::ffi::c_char {
        // SAFETY: as documented.
        unsafe {
            let n = core::ffi::CStr::from_ptr(src).to_bytes_with_nul().len();
            std::ptr::copy_nonoverlapping(src, dst, n);
        }
        dst
    }
}

/// The top-level boxes of a JP2 file: `(type, payload)`.
fn boxes(b: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 8 <= b.len() {
        let len = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
        let typ = [b[i + 4], b[i + 5], b[i + 6], b[i + 7]];
        let (header, total) = match len {
            0 => (8, b.len() - i),
            1 if i + 16 <= b.len() => (16, u64::from_be_bytes(b[i + 8..i + 16].try_into().unwrap_or_default()) as usize),
            _ => (8, len),
        };
        if total < header || i + total > b.len() {
            break;
        }
        out.push((typ, &b[i + header..i + total]));
        i += total;
    }
    out
}

/// The index of the alpha component, from the channel definition box (`jp2h/cdef`).
fn alpha_channel(b: &[u8]) -> Option<usize> {
    let header = boxes(b).into_iter().find(|(t, _)| t == b"jp2h")?.1;
    let cdef = boxes(header).into_iter().find(|(t, _)| t == b"cdef")?.1;
    let n = u16::from_be_bytes([*cdef.first()?, *cdef.get(1)?]) as usize;
    (0..n).find_map(|i| {
        let e = cdef.get(2 + i * 6..2 + i * 6 + 6)?;
        (u16::from_be_bytes([e[2], e[3]]) == 1).then_some(u16::from_be_bytes([e[0], e[1]]) as usize)
    })
}

/// The geotransform and CRS from the GeoJP2 box, or the GMLJP2 box.
fn georeferencing(b: &[u8]) -> (Option<[f64; 6]>, Option<CrsInfo>) {
    let top = boxes(b);
    if let Some((_, payload)) = top.iter().find(|(t, p)| t == b"uuid" && p.starts_with(&GEOJP2_UUID))
        && let Some(tags) = crate::geotiff::read_geo_tags_from(std::io::Cursor::new(&payload[16..]))
    {
        let crs = tags.epsg.map(CrsInfo::from_epsg).or(tags.crs);
        let gt = tags
            .geo_transform
            .map(|gt| if tags.pixel_is_point { [gt[0] - gt[1] / 2.0, gt[1], gt[2], gt[3] - gt[5] / 2.0, gt[4], gt[5]] } else { gt });
        if gt.is_some() || crs.is_some() {
            return (gt, crs);
        }
    }
    // GMLJP2: the XML inside the association boxes.
    let xml = top
        .iter()
        .filter(|(t, _)| t == b"asoc")
        .flat_map(|(_, p)| p.windows(5).position(|w| w == b"<?xml" || w == b"<gml:").map(|i| String::from_utf8_lossy(&p[i..]).into_owned()))
        .find(|x| x.contains("RectifiedGrid"));
    xml.map(|x| gml_georeferencing(&x)).unwrap_or((None, None))
}

/// Parses a GMLJP2 `RectifiedGrid` as GDAL does (`GDALJP2Metadata::ParseGMLCoverageDesc`): the
/// origin (a pixel centre) and offset vectors, the axes swapped when the CRS is named by URN or
/// URL and EPSG orders it latitude or northing first, unless the grid names its axes east and
/// north. Rotated grids give no geotransform.
fn gml_georeferencing(xml: &str) -> (Option<[f64; 6]>, Option<CrsInfo>) {
    let grid = &xml[xml.find("RectifiedGrid").unwrap_or(0)..];
    let attr = |s: &str| s.split("srsName=\"").nth(1).and_then(|s| s.split('"').next()).map(str::to_string);
    // The origin point's CRS, or the first one named.
    let origin_xml = grid.find("<gml:origin").map(|i| &grid[i..]).unwrap_or(grid);
    let srs = origin_xml.find("</gml:origin>").and_then(|end| attr(&origin_xml[..end])).or_else(|| attr(grid)).or_else(|| attr(xml));
    let epsg = srs.as_deref().and_then(|s| s.rsplit([':', '/']).next()).and_then(|c| c.parse::<u32>().ok());
    let numbers = |tag: &str| -> Vec<Vec<f64>> {
        grid.split(&format!("<gml:{tag}"))
            .skip(1)
            .filter_map(|s| s.split_once('>').map(|(_, rest)| rest.split('<').next().unwrap_or_default()))
            .map(|t| t.split([' ', ',', '\n', '\t']).filter_map(|v| v.trim().parse().ok()).collect())
            .collect()
    };
    let crs = epsg.map(CrsInfo::from_epsg);
    let origin = numbers("pos").into_iter().next().or_else(|| numbers("coordinates").into_iter().next());
    let vectors = numbers("offsetVector");
    let (Some(o), [v1, v2, ..]) = (origin, vectors.as_slice()) else {
        return (None, crs);
    };
    if o.len() < 2 || v1.len() < 2 || v2.len() < 2 {
        return (None, crs);
    }
    // GDAL's arithmetic, in its order.
    let mut gt = [o[0], v1[0], v2[0], o[1], v1[1], v2[1]];
    gt[0] -= gt[1] * 0.5;
    gt[0] -= gt[2] * 0.5;
    gt[3] -= gt[4] * 0.5;
    gt[3] -= gt[5] * 0.5;
    let by_urn = srs.as_deref().is_some_and(|s| {
        let lower = s.to_ascii_lowercase();
        (lower.starts_with("urn:") && lower.contains(":def:")) || lower.starts_with("http://www.opengis.net/def/crs/")
    });
    let axis_names: Vec<String> = element_texts(grid, "axisName");
    let east_north = axis_names.len() >= 2
        && ["EAST", "LONG"].iter().any(|p| axis_names[0].to_ascii_uppercase().starts_with(p))
        && ["NORTH", "LAT"].iter().any(|p| axis_names[1].to_ascii_uppercase().starts_with(p));
    if by_urn && epsg.is_some_and(northing_first) && !east_north {
        gt.swap(0, 3);
        if xml.contains("GDAL_JP2K_ALT_OFFSETVECTOR_ORDER=TRUE") {
            gt.swap(1, 5);
            gt.swap(2, 4);
        } else {
            gt.swap(1, 4);
            gt.swap(2, 5);
        }
    }
    if gt[2] != 0.0 || gt[4] != 0.0 {
        return (None, crs);
    }
    (Some(gt), crs)
}

/// The text of each `<gml:{tag}>` element.
fn element_texts(xml: &str, tag: &str) -> Vec<String> {
    xml.split(&format!("<gml:{tag}"))
        .skip(1)
        .filter_map(|s| s.split_once('>').map(|(_, rest)| rest.split('<').next().unwrap_or_default().trim().to_string()))
        .collect()
}

/// Whether EPSG orders the axes of a CRS latitude or northing first (GDAL's
/// `EPSGTreatsAsLatLong` and `EPSGTreatsAsNorthingEasting`): its first axis points north or south.
fn northing_first(code: u32) -> bool {
    let Some(wkt) = crate::crs::epsg_wkt(code) else { return false };
    // The CRS's own axes are the last two.
    let axes: Vec<&str> = wkt.match_indices("AXIS[").map(|(i, _)| &wkt[i..]).collect();
    let [.., first, _] = axes.as_slice() else { return false };
    let direction = first.split(']').next().and_then(|a| a.rsplit(',').next()).unwrap_or_default().trim().to_ascii_uppercase();
    direction == "NORTH" || direction == "SOUTH"
}

/// Whether the bytes start as a JP2 file or a J2K codestream does.
pub(crate) fn is_jpeg2000(b: &[u8]) -> bool {
    b.starts_with(&[0, 0, 0, 0x0c, b'j', b'P', b' ', b' ', 0x0d, 0x0a, 0x87, 0x0a]) || b.starts_with(openjp2::J2K_CODESTREAM_MAGIC)
}

/// Reads a JPEG 2000 file.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    read_bytes(&std::fs::read(path)?).map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))
}

/// Reads a JP2 file or a J2K codestream held in memory.
pub(crate) fn read_bytes(bytes: &[u8]) -> Result<RasterDataset> {
    let (gt, crs) = georeferencing(bytes);
    let (cols, rows, data) = decode(bytes, alpha_channel(bytes))?;
    let bands = data.len() / (cols * rows).max(1);
    let raster = common::make_raster(cols, rows, bands, data, gt.unwrap_or(common::NO_GEOTRANSFORM), crs.unwrap_or_default(), None)?;
    Ok(RasterDataset { raster, driver: RasterDriver::JP2, has_nodata: false })
}

/// Writes a lossless JP2 with a GeoJP2 box; nodata goes to a `.aux.xml` sidecar.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path) -> Result<()> {
    std::fs::write(path, to_bytes(r)?)?;
    common::write_pam_with(path, None, nodata, r.bands)
}

/// Encodes a raster as a lossless JP2 file with a GeoJP2 box.
pub(crate) fn to_bytes(r: &Raster) -> Result<Vec<u8>> {
    let (precision, signed) = match r.data_type {
        DataType::U8 => (8, false),
        DataType::I8 => (8, true),
        DataType::U16 => (16, false),
        DataType::I16 => (16, true),
        t => {
            return Err(Error::Unsupported(format!(
                "JPEG 2000 output holds 8- and 16-bit integers, not {}",
                crate::info::gdal_type_name(t)
            )));
        }
    };
    let n = r.cols * r.rows;
    let band = |b: usize| -> Vec<i32> {
        let range = b * n..(b + 1) * n;
        match &r.data {
            RasterData::U8(v) => v[range].iter().map(|x| *x as i32).collect(),
            RasterData::I8(v) => v[range].iter().map(|x| *x as i32).collect(),
            RasterData::U16(v) => v[range].iter().map(|x| *x as i32).collect(),
            RasterData::I16(v) => v[range].iter().map(|x| *x as i32).collect(),
            _ => unreachable!("checked above"),
        }
    };
    let comps: Vec<Component> = (0..r.bands).map(|b| Component { precision, signed, data: band(b) }).collect();
    let codestream = encode(r.cols, r.rows, &comps)?;

    let mut out = Vec::new();
    let mut put = |typ: &[u8; 4], payload: &[u8]| {
        out.extend(((payload.len() + 8) as u32).to_be_bytes());
        out.extend(typ);
        out.extend(payload);
    };
    put(b"jP  ", &[0x0d, 0x0a, 0x87, 0x0a]);
    put(b"ftyp", b"jp2 \0\0\0\0jp2 ");
    let mut ihdr = Vec::new();
    ihdr.extend((r.rows as u32).to_be_bytes());
    ihdr.extend((r.cols as u32).to_be_bytes());
    ihdr.extend((r.bands as u16).to_be_bytes());
    ihdr.extend([(precision - 1) as u8 | if signed { 0x80 } else { 0 }, 7, 0, 0]);
    let enumcs: u32 = if r.bands == 3 || r.bands == 4 { 16 } else { 17 };
    let mut colr = vec![1u8, 0, 0];
    colr.extend(enumcs.to_be_bytes());
    let mut jp2h = Vec::new();
    jp2h.extend(((ihdr.len() + 8) as u32).to_be_bytes());
    jp2h.extend(b"ihdr");
    jp2h.extend(&ihdr);
    jp2h.extend(((colr.len() + 8) as u32).to_be_bytes());
    jp2h.extend(b"colr");
    jp2h.extend(&colr);
    put(b"jp2h", &jp2h);
    // GeoJP2: a 1x1 GeoTIFF carrying the georeferencing of the full image.
    let tiny_cfg = wbraster::RasterConfig {
        cols: 1,
        rows: 1,
        bands: 1,
        x_min: r.x_min,
        y_min: r.y_max() - r.cell_size_y,
        cell_size: r.cell_size_x,
        cell_size_y: Some(r.cell_size_y),
        data_type: DataType::U8,
        crs: r.crs.clone(),
        ..Default::default()
    };
    let tiny = Raster::from_data_native(tiny_cfg, RasterData::U8(vec![0]))?;
    let tiff = super::gtiff_write::encode(&tiny, &Default::default())?;
    let mut uuid = GEOJP2_UUID.to_vec();
    uuid.extend(tiff);
    put(b"uuid", &uuid);
    put(b"jp2c", &codestream);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_signed_samples() {
        let data: Vec<i16> = (0..40 * 30).map(|i: i32| ((i * 37) % 3000 - 1500) as i16).collect();
        let r = common::make_raster(
            40,
            30,
            1,
            RasterData::I16(data.clone()),
            [500000.0, 10.0, 0.0, 4100000.0, 0.0, -10.0],
            CrsInfo::from_epsg(32611),
            None,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.jp2");
        write(&r, Some(-1500.0), &p).unwrap();
        let ds = read(&p).unwrap();
        assert_eq!(ds.raster.data_i16().unwrap(), &data[..]);
        assert_eq!(ds.crs().epsg, Some(32611));
        assert_eq!(ds.geo_transform(), [500000.0, 10.0, 0.0, 4100000.0, 0.0, -10.0]);
    }

    #[test]
    fn parses_gmljp2_origins() {
        let xml = r#"<gml:RectifiedGrid dimension="2"><gml:origin><gml:Point srsName="urn:ogc:def:crs:EPSG::32633"><gml:pos>300005 5000035</gml:pos></gml:Point></gml:origin><gml:offsetVector srsName="urn:ogc:def:crs:EPSG::32633">10 0</gml:offsetVector><gml:offsetVector srsName="urn:ogc:def:crs:EPSG::32633">0 -10</gml:offsetVector></gml:RectifiedGrid>"#;
        let (gt, crs) = gml_georeferencing(xml);
        assert_eq!(gt.unwrap(), [300000.0, 10.0, 0.0, 5000040.0, 0.0, -10.0]);
        assert_eq!(crs.unwrap().epsg, Some(32633));
    }
}

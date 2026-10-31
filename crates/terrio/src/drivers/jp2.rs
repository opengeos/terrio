// SPDX-FileCopyrightText: Copyright (c) 2005, Frank Warmerdam <warmerdam@pobox.com>
// SPDX-FileCopyrightText: Copyright (c) 2010-2015, Even Rouault <even dot rouault at spatialys dot com>
// SPDX-FileCopyrightText: Copyright (c) 2015, European Union Satellite Centre
// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     gcore/gdaljp2metadata.cpp

//! JPEG 2000 (`.jp2`, `.j2k`), coded with `openjp2`, a pure-Rust port of OpenJPEG (the codec
//! behind GDAL's JP2OpenJPEG driver), so decoded pixels match GDAL's, signed samples included, in
//! native and WebAssembly builds alike.
//!
//! Files are read in place by window and resolution level, as GDAL's driver reads them: the
//! resolution levels are the overviews (each halving the size, down to about 128 pixels), and a
//! window decodes only the tiles, precincts and code blocks it covers, the tiles in parallel
//! (OpenJPEG's decode area and reduce factor), from local files, HTTP range requests or bytes.
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
use codec::{Component, encode};
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

    /// A seekable byte stream OpenJPEG streams read from: a file, a remote file read by range
    /// requests, or bytes in memory.
    pub(crate) trait ReadSeek: std::io::Read + std::io::Seek + Send + Sync {}
    impl<T: std::io::Read + std::io::Seek + Send + Sync> ReadSeek for T {}

    /// What OpenJPEG streams read from.
    struct Source<'a> {
        r: &'a mut dyn ReadSeek,
        /// The first read error, reported once OpenJPEG is done (it takes errors for the end of
        /// a truncated stream, and decodes what it read).
        error: Option<std::io::Error>,
    }

    unsafe extern "C" fn read_fn(dst: *mut c_void, n: usize, user: *mut c_void) -> usize {
        // SAFETY: `user` is the `Source` registered with the stream, alive while the stream is.
        let s = unsafe { &mut *(user as *mut Source) };
        if dst.is_null() {
            return usize::MAX;
        }
        // SAFETY: OpenJPEG passes a buffer of at least `n` bytes.
        let buf = unsafe { std::slice::from_raw_parts_mut(dst as *mut u8, n) };
        let mut k = 0;
        while k < n {
            match s.r.read(&mut buf[k..]) {
                Ok(0) => break,
                Ok(m) => k += m,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => {
                    s.error.get_or_insert(e);
                    break;
                }
            }
        }
        if k == 0 { usize::MAX } else { k } // `usize::MAX`: end of stream
    }

    unsafe extern "C" fn read_skip_fn(n: i64, user: *mut c_void) -> i64 {
        // SAFETY: as in `read_fn`.
        let s = unsafe { &mut *(user as *mut Source) };
        match s.r.seek(std::io::SeekFrom::Current(n)) {
            Ok(_) => n,
            Err(e) => {
                s.error.get_or_insert(e);
                -1
            }
        }
    }

    unsafe extern "C" fn read_seek_fn(n: i64, user: *mut c_void) -> i32 {
        // SAFETY: as in `read_fn`.
        let s = unsafe { &mut *(user as *mut Source) };
        match s.r.seek(std::io::SeekFrom::Start(n.max(0) as u64)) {
            Ok(_) => 1,
            Err(e) => {
                s.error.get_or_insert(e);
                0
            }
        }
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
    fn collect(comps: &[&[i32]], precision: u32, signed: bool) -> Result<RasterData> {
        fn gather<T>(comps: &[&[i32]], f: impl Fn(i32) -> T) -> Result<Vec<T>> {
            let mut out = Vec::new();
            crate::dataset::fallible::reserve(&mut out, comps.iter().map(|c| c.len()).sum(), "a JPEG 2000 image")?;
            for c in comps {
                out.extend(c.iter().map(|&v| f(v)));
            }
            Ok(out)
        }
        Ok(match (precision, signed) {
            (0..=8, false) => RasterData::U8(gather(comps, |v| v as u8)?),
            (0..=8, true) => RasterData::I8(gather(comps, |v| v as i8)?),
            (9..=16, false) => RasterData::U16(gather(comps, |v| v as u16)?),
            (9..=16, true) => RasterData::I16(gather(comps, |v| v as i16)?),
            (_, false) => RasterData::U32(gather(comps, |v| v as u32)?),
            (_, true) => RasterData::I32(gather(comps, |v| v)?),
        })
    }

    /// What a codestream's main header tells: the image area on the reference grid, the
    /// components, the tiling and the number of resolution levels.
    #[derive(Debug, Clone)]
    pub(super) struct Header {
        /// The image area `(x0, y0, x1, y1)` on the reference grid.
        pub area: [u32; 4],
        /// The precision (bits) of each component.
        pub precisions: Vec<u32>,
        /// Whether the samples are signed.
        pub signed: bool,
        /// Whether a component is subsampled.
        pub subsampled: bool,
        /// The tile size.
        pub tile: (u32, u32),
        /// The origin of the tile grid on the reference grid.
        pub tile_origin: (u32, u32),
        /// The number of resolution levels of the first component (of the default tile).
        pub resolutions: u32,
    }

    /// Opens a codec over a stream positioned at the file's start and reads the main header,
    /// then runs `f` with the codec, stream and image, and releases them.
    ///
    /// # Safety
    /// `f` must not keep the pointers it is given.
    unsafe fn with_codec<T>(
        r: &mut dyn ReadSeek,
        len: u64,
        j2k: bool,
        f: impl FnOnce(*mut opj_codec_t, *mut opj_stream_t, *mut opj_image_t) -> Result<T>,
    ) -> Result<T> {
        r.seek(std::io::SeekFrom::Start(0))?;
        let format = if j2k { OPJ_CODEC_J2K } else { OPJ_CODEC_JP2 };
        let mut source = Source { r, error: None };
        // SAFETY: the stream, codec and image are created, used and destroyed here; the source
        // outlives the stream.
        unsafe {
            // A small buffer: a window's decoding reads the header of every tile part before
            // its own (skipping their data), and remote files fetch what the buffer asks for.
            let stream = opj_stream_create(16 * 1024, 1);
            opj_stream_set_read_function(stream, Some(read_fn));
            opj_stream_set_skip_function(stream, Some(read_skip_fn));
            opj_stream_set_seek_function(stream, Some(read_seek_fn));
            opj_stream_set_user_data_length(stream, len);
            opj_stream_set_user_data(stream, &mut source as *mut Source as *mut c_void, None);
            let codec = opj_create_decompress(format);
            let mut params = opj_dparameters_t::default();
            let mut image: *mut opj_image_t = std::ptr::null_mut();
            let ok = !codec.is_null() && opj_setup_decoder(codec, &mut params) != 0 && opj_read_header(stream, codec, &mut image) != 0;
            let result = if ok && !image.is_null() {
                f(codec, stream, image)
            } else {
                Err(Error::InvalidData("cannot read the JPEG 2000 codestream header".into()))
            };
            if !image.is_null() {
                opj_image_destroy(image);
            }
            if !codec.is_null() {
                opj_destroy_codec(codec);
            }
            opj_stream_destroy(stream);
            match source.error.take() {
                Some(e) => Err(e.into()),
                None => result,
            }
        }
    }

    /// Reads the main header of a JP2 file or a J2K codestream (`j2k`).
    pub(super) fn read_header(r: &mut dyn ReadSeek, len: u64, j2k: bool) -> Result<Header> {
        // SAFETY: the pointers are those `with_codec` passes, valid during the call.
        unsafe {
            with_codec(r, len, j2k, |codec, _, image| {
                let img = &*image;
                let comps = std::slice::from_raw_parts(img.comps, img.numcomps as usize);
                if comps.is_empty() {
                    return Err(Error::InvalidData("JPEG 2000 image without components".into()));
                }
                let mut info = opj_get_cstr_info(codec);
                let (tile, tile_origin, resolutions) = if info.is_null() {
                    ((img.x1 - img.x0, img.y1 - img.y0), (img.x0, img.y0), 1)
                } else {
                    let i = &*info;
                    let tccp = i.m_default_tile_info.tccp_info;
                    let res = if tccp.is_null() { 1 } else { (*tccp).numresolutions };
                    let out = ((i.tdx, i.tdy), (i.tx0, i.ty0), res);
                    opj_destroy_cstr_info(&mut info);
                    out
                };
                Ok(Header {
                    area: [img.x0, img.y0, img.x1, img.y1],
                    precisions: comps.iter().map(|c| c.prec).collect(),
                    signed: comps[0].sgnd != 0,
                    subsampled: comps.iter().any(|c| c.dx != 1 || c.dy != 1),
                    tile,
                    tile_origin,
                    resolutions,
                })
            })
        }
    }

    /// Decodes an area of an image from a stream, at a resolution level: `area` is `(x0, y0, x1,
    /// y1)` on the reference grid (the full-resolution image, offset by the image origin), `None`
    /// for the whole image; `reduce` the number of resolution levels dropped (each halving the
    /// size). Only the tiles, precincts and code blocks the area needs are decoded.
    pub(super) fn decode_area(
        r: &mut dyn ReadSeek,
        len: u64,
        j2k: bool,
        area: Option<[u32; 4]>,
        reduce: u32,
        alpha: Option<usize>,
    ) -> Result<(usize, usize, RasterData)> {
        // SAFETY: the pointers are those `with_codec` passes, valid during the call.
        unsafe {
            with_codec(r, len, j2k, |codec, stream, image| {
                if reduce > 0 && opj_set_decoded_resolution_factor(codec, reduce) == 0 {
                    return Err(Error::InvalidData(format!("cannot decode JPEG 2000 resolution level {reduce}")));
                }
                if let Some([x0, y0, x1, y1]) = area {
                    // The decode area is on the reference grid; the image's components must
                    // know the resolution factor first (as GDAL sets them).
                    let img = &mut *image;
                    for c in std::slice::from_raw_parts_mut(img.comps, img.numcomps as usize) {
                        c.factor = reduce;
                    }
                    if opj_set_decode_area(codec, image, x0 as i32, y0 as i32, x1 as i32, y1 as i32) == 0 {
                        return Err(Error::InvalidArgument("invalid JPEG 2000 decode area".into()));
                    }
                }
                if opj_decode(codec, stream, image) == 0 || opj_end_decompress(codec, stream) == 0 {
                    return Err(Error::InvalidData("cannot decode the JPEG 2000 codestream".into()));
                }
                let img = &*image;
                let comps = std::slice::from_raw_parts(img.comps, img.numcomps as usize);
                let (w, h) = (comps[0].w as usize, comps[0].h as usize);
                if comps.iter().any(|c| c.w as usize != w || c.h as usize != h || c.data.is_null()) {
                    return Err(Error::Unsupported("JPEG 2000 components with different sizes are not supported".into()));
                }
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
                Ok((w, h, collect(&planes, precision, signed)?))
            })
        }
    }

    /// The data type `collect` gives components of these precisions, the alpha one promoted as
    /// `decode_area` promotes it.
    pub(super) fn data_type(precisions: &[u32], signed: bool, alpha: Option<usize>) -> wbraster::DataType {
        let mut p = precisions.to_vec();
        if let Some(a) = alpha.filter(|&a| a < p.len()) {
            let colour = if a == 0 && p.len() > 1 { 1 } else { 0 };
            if p[a] == 1 && p[colour] == 8 {
                p[a] = 8;
            }
        }
        match (p.iter().copied().max().unwrap_or(8), signed) {
            (0..=8, false) => wbraster::DataType::U8,
            (0..=8, true) => wbraster::DataType::I8,
            (9..=16, false) => wbraster::DataType::U16,
            (9..=16, true) => wbraster::DataType::I16,
            (_, false) => wbraster::DataType::U32,
            (_, true) => wbraster::DataType::I32,
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
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
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
        if total < header || i.checked_add(total).is_none_or(|end| end > b.len()) {
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

/// Reads the boxes of a JP2 file before its codestream (`jp2c`), as a buffer of boxes that
/// [`boxes`] parses, without reading the codestream. Empty for a J2K codestream.
fn header_boxes(r: &mut dyn codec::ReadSeek) -> Result<Vec<u8>> {
    use std::io::SeekFrom;
    let len = r.seek(SeekFrom::End(0))?;
    let mut pos = 0u64;
    let mut out = Vec::new();
    while pos + 8 <= len {
        r.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 16];
        r.read_exact(&mut h[..8])?;
        let size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        let (header, total) = match size {
            0 => (8, len - pos),
            1 => {
                r.read_exact(&mut h[8..16])?;
                (16, u64::from_be_bytes(h[8..16].try_into().unwrap_or_default()))
            }
            n => (8, n),
        };
        let Some(end) = pos.checked_add(total) else { break };
        if total < header || end > len || &h[4..8] == b"jp2c" {
            break;
        }
        // Metadata boxes are small; a box larger than this is not one terrio reads.
        if total <= 64 << 20 {
            let mut b = vec![0u8; total as usize];
            r.seek(SeekFrom::Start(pos))?;
            r.read_exact(&mut b)?;
            out.extend(b);
        }
        pos = end;
    }
    Ok(out)
}

/// Where a JPEG 2000 file's bytes are: each read opens its own stream, so that tiles are decoded
/// in parallel.
enum StreamSource {
    File(std::path::PathBuf),
    Bytes(std::sync::Arc<[u8]>),
    #[cfg(remote)]
    Http(crate::http::RangeReader),
}

impl StreamSource {
    fn open(&self) -> Result<Box<dyn codec::ReadSeek>> {
        Ok(match self {
            Self::File(p) => Box::new(std::io::BufReader::with_capacity(1 << 16, std::fs::File::open(p)?)),
            Self::Bytes(b) => Box::new(std::io::Cursor::new(b.clone())),
            #[cfg(remote)]
            Self::Http(r) => Box::new(r.clone()),
        })
    }
}

/// A JPEG 2000 file read by window and resolution level, as GDAL's JP2OpenJPEG driver reads it:
/// the resolution levels as overviews, a window decoding only the tiles, precincts and code blocks
/// it covers (the tiles in parallel).
struct Jp2Window {
    referenced: bool,
    source: StreamSource,
    len: u64,
    j2k: bool,
    header: codec::Header,
    alpha: Option<usize>,
    overviews: Vec<(usize, usize)>,
    block: (usize, usize),
}

impl Jp2Window {
    /// Opens a JP2 file or a J2K codestream: its boxes, georeferencing and main header. Returns
    /// the reader and the dataset header (without pixels).
    fn open(source: StreamSource) -> Result<(Self, RasterDataset)> {
        use std::io::SeekFrom;
        let mut stream = source.open()?;
        let len = stream.seek(SeekFrom::End(0))?;
        stream.seek(SeekFrom::Start(0))?;
        let mut magic = [0u8; 12];
        stream.read_exact(&mut magic[..len.min(12) as usize])?;
        let j2k = magic.starts_with(openjp2::J2K_CODESTREAM_MAGIC);
        let boxes_bytes = if j2k { Vec::new() } else { header_boxes(stream.as_mut())? };
        let (gt, crs) = georeferencing(&boxes_bytes);
        let alpha = alpha_channel(&boxes_bytes);
        let header = codec::read_header(stream.as_mut(), len, j2k)?;
        if header.subsampled {
            return Err(Error::Unsupported("JPEG 2000 components with different sizes are not supported".into()));
        }
        let [x0, y0, x1, y1] = header.area;
        let (w, h) = ((x1 - x0) as usize, (y1 - y0) as usize);
        let palette =
            boxes(&boxes_bytes).into_iter().find(|(t, _)| t == b"jp2h").is_some_and(|(_, p)| boxes(p).iter().any(|(t, _)| t == b"pclr"));
        let mut header = header;
        let (bands, data_type) = if palette {
            // OpenJPEG applies the palette when decoding: the bands and type are those of a
            // decoded pixel.
            let mut s = source.open()?;
            let (_, _, px) = codec::decode_area(s.as_mut(), len, j2k, Some([x0, y0, x0 + 1, y0 + 1]), 0, alpha)?;
            let n = px.len().max(1);
            header.precisions = vec![header.precisions[0]; n];
            (n, px.data_type())
        } else {
            (header.precisions.len(), codec::data_type(&header.precisions, header.signed, alpha))
        };
        let (overviews, block) = levels(w, h, header.tile, if palette { 0 } else { header.resolutions });
        let raster = Raster {
            cols: w,
            rows: h,
            bands,
            x_min: 0.0,
            y_min: 0.0,
            cell_size_x: 1.0,
            cell_size_y: 1.0,
            nodata: -9999.0,
            data_type,
            crs: crs.unwrap_or_default(),
            metadata: Vec::new(),
            data: RasterData::new_filled(data_type, 0, 0.0),
        };
        let mut ds = RasterDataset::with_driver(raster, RasterDriver::JP2, false);
        // Without georeferencing, unit pixels with the image's bottom at y = 0, as terrio's other
        // readers do.
        ds.set_geo_transform(gt.unwrap_or([0.0, 1.0, 0.0, h as f64, 0.0, -1.0]));
        let reversibility = reversibility(source.open()?.as_mut(), len, j2k).unwrap_or(None);
        ds.metadata_domains = image_structure(&header.precisions, alpha, reversibility);
        if !palette {
            ds.band_metadata = color_interpretation(&boxes_bytes, bands)
                .into_iter()
                .map(|c| crate::BandMetadata { color_interp: Some(c.to_string()), ..Default::default() })
                .collect();
        }
        Ok((Self { referenced: gt.is_some(), source, len, j2k, header, alpha, overviews, block }, ds))
    }

    /// Decodes an area of the reference grid at a resolution level, on a stream of its own.
    fn decode(&self, area: [u32; 4], reduce: u32) -> Result<(usize, usize, RasterData)> {
        let mut stream = self.source.open()?;
        codec::decode_area(stream.as_mut(), self.len, self.j2k, Some(area), reduce, self.alpha)
    }
}

/// The colour interpretation GDAL gives each band (`JP2OPJLikeRasterBand::GetColorInterpretation`):
/// the alpha channel of the `cdef` box; gray for one or two bands in the greyscale colour space;
/// red, green and blue (the `cdef` box's, else the first three) in the sRGB and sYCC spaces
/// (`colr` box enumerations 16, 18, 20 and 21; 17 is greyscale); undefined otherwise, codestreams
/// without a JP2 header included.
fn color_interpretation(b: &[u8], bands: usize) -> Vec<&'static str> {
    #[derive(PartialEq)]
    enum Space {
        Gray,
        Rgb,
        Other,
    }
    let header = boxes(b).into_iter().find(|(t, _)| t == b"jp2h").map(|(_, p)| boxes(p)).unwrap_or_default();
    let space = header
        .iter()
        .find(|(t, _)| t == b"colr")
        .and_then(|(_, c)| (c.first() == Some(&1) && c.len() >= 7).then(|| u32::from_be_bytes([c[3], c[4], c[5], c[6]])))
        .map_or(Space::Other, |e| match e {
            17 => Space::Gray,
            16 | 18 | 20 | 21 => Space::Rgb,
            _ => Space::Other,
        });
    let (mut rgb, mut alpha) = ([Some(0usize), Some(1), Some(2)], None);
    if let Some((_, c)) = header.iter().find(|(t, _)| t == b"cdef")
        && c.len() == 2 + bands * 6
        && u16::from_be_bytes([c[0], c[1]]) as usize == bands
    {
        rgb = [None; 3];
        for e in c[2..].as_chunks::<6>().0.iter() {
            let (cn, typ, asoc) =
                (u16::from_be_bytes([e[0], e[1]]) as usize, u16::from_be_bytes([e[2], e[3]]), u16::from_be_bytes([e[4], e[5]]));
            if cn >= bands {
                break;
            }
            match (typ, asoc) {
                (0, 1..=3) => rgb[asoc as usize - 1] = Some(cn),
                (1, _) => alpha = Some(cn),
                _ => {}
            }
        }
    }
    (0..bands)
        .map(|i| {
            if alpha == Some(i) {
                "Alpha"
            } else if bands <= 2 && space == Space::Gray {
                "Gray"
            } else if space == Space::Rgb && rgb.contains(&Some(i)) {
                ["Red", "Green", "Blue"][rgb.iter().position(|&c| c == Some(i)).unwrap_or(0)]
            } else {
                "Undefined"
            }
        })
        .collect()
}

/// GDAL's `IMAGE_STRUCTURE` metadata of a JPEG 2000 dataset and of its bands: the pixel
/// interleave of several bands and the compression's reversibility
/// (`GDALJP2AbstractDataset::GetMetadata`), each band's compression and its bit depth when not a
/// whole number of bytes, a 1-bit alpha band of 8-bit colours counting as 8 bits
/// (`JP2OPJLikeRasterBand`).
fn image_structure(precisions: &[u32], alpha: Option<usize>, reversibility: Option<&'static str>) -> Vec<crate::dataset::MetadataDomain> {
    let domain = || "IMAGE_STRUCTURE".to_string();
    let mut dataset = Vec::new();
    if precisions.len() > 1 {
        dataset.push(("INTERLEAVE".to_string(), "PIXEL".to_string()));
    }
    if let Some(r) = reversibility {
        dataset.push(("COMPRESSION_REVERSIBILITY".to_string(), r.to_string()));
    }
    let mut out = Vec::new();
    if !dataset.is_empty() {
        out.push((None, domain(), dataset));
    }
    let colour = |a: usize| if a == 0 && precisions.len() > 1 { 1 } else { 0 };
    for (b, &p) in precisions.iter().enumerate() {
        let promoted = alpha == Some(b) && p == 1 && precisions.get(colour(b)) == Some(&8);
        let mut items = Vec::new();
        if p % 8 != 0 && !promoted {
            items.push(("NBITS".to_string(), p.to_string()));
        }
        items.push(("COMPRESSION".to_string(), "JPEG2000".to_string()));
        out.push((Some(b), domain(), items));
    }
    out
}

/// The reversibility GDAL reports for a JPEG 2000 codestream (`GDALGetJPEG2000Reversibility`):
/// from the wavelet of the last COD marker before the first tile's data (9-7 lossy, 5-3 lossless
/// "possibly"), and for the 5-3 wavelet, from the last COM marker of Kakadu's or GDAL's
/// OpenJPEG writer. `None` without a COD marker.
fn reversibility(r: &mut dyn codec::ReadSeek, len: u64, j2k: bool) -> Result<Option<&'static str>> {
    use std::io::SeekFrom;
    // The codestream: the whole file, or the `jp2c` box.
    let mut start = 0u64;
    if !j2k {
        let mut pos = 0u64;
        loop {
            if pos + 8 > len {
                return Ok(None);
            }
            r.seek(SeekFrom::Start(pos))?;
            let mut h = [0u8; 16];
            r.read_exact(&mut h[..8])?;
            let size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
            let (header, total) = match size {
                0 => (8, len - pos),
                1 => {
                    r.read_exact(&mut h[8..16])?;
                    (16, u64::from_be_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]))
                }
                n => (8, n),
            };
            if &h[4..8] == b"jp2c" {
                start = pos + header;
                break;
            }
            match pos.checked_add(total) {
                Some(end) if total >= header && end <= len => pos = end,
                _ => return Ok(None),
            }
        }
    }
    // The main header and the first tile-part header, up to its data (SOD): small.
    let n = (len - start).min(1 << 20) as usize;
    let mut cs = vec![0u8; n];
    r.seek(SeekFrom::Start(start))?;
    r.read_exact(&mut cs)?;
    let (mut transformation, mut comment) = (None, None);
    let mut pos = 2;
    while pos + 4 <= cs.len() && cs[pos] == 0xff {
        let marker = cs[pos + 1];
        if marker == 0x93 || marker == 0xd9 {
            break;
        }
        let seg = u16::from_be_bytes([cs[pos + 2], cs[pos + 3]]) as usize;
        let body = cs.get(pos + 4..pos + 2 + seg).unwrap_or_default();
        match marker {
            0x52 => transformation = body.get(9).copied(),
            0x64 => comment = body.get(2..).map(|t| String::from_utf8_lossy(t).into_owned()),
            _ => {}
        }
        if seg < 2 {
            break;
        }
        pos += 2 + seg;
    }
    Ok(match transformation {
        Some(0) => Some("LOSSY"),
        Some(1) => {
            Some(match comment.as_deref() {
                Some(c) if c.starts_with("Kdu-Layer-Info: log_2{Delta-D(squared-error)/Delta-L(bytes)}, L(bytes)") => {
                    if c.contains("-192.0,") { "LOSSLESS" } else { "LOSSY" }
                }
                Some(c) if c.starts_with("Kdu-Layer-Info: log_2{Delta-D(MSE)/[2^16*Delta-L(bytes)]}, L(bytes)") => {
                    if c.contains("-256.0,") { "LOSSLESS" } else { "LOSSY" }
                }
                Some(c) if c.starts_with("Created by OpenJPEG") && c.contains("LOSSLESS settings used") => "LOSSLESS",
                Some(c) if c.starts_with("Created by OpenJPEG") && c.contains("LOSSY settings used") => "LOSSY",
                _ => "LOSSLESS (possibly)",
            })
        }
        _ => None,
    })
}

/// `ceil(a / 2^r)`, as JPEG 2000 maps reference grid coordinates to a resolution level.
fn ceil_shift(a: u32, r: u32) -> usize {
    (((a as u64) + (1u64 << r) - 1) >> r) as usize
}

/// The overview sizes and block size GDAL's JP2OpenJPEG driver gives an image of `w` x `h` pixels
/// in tiles of `tile` with `resolutions` resolution levels: each level halves the size (rounding
/// up), down to about 128 pixels, and needs either a decode area or even tile sizes.
fn levels(w: usize, h: usize, tile: (u32, u32), resolutions: u32) -> (Vec<(usize, usize)>, (usize, usize)) {
    let (mut tw, mut th) = (tile.0 as usize, tile.1 as usize);
    let single = w == tw && h == th;
    // The tile is the block (`JP2OPJLikeDataset::Open`), unless one of the cases below applies.
    let mut block = (tw, th);
    let decode_area = if w <= 1024 && h <= 1024 && tw < 32 && th < 32 {
        block = (w, h);
        true
    } else if w < tw && h < th {
        block = (w.min(2048), h.min(2048));
        true
    } else {
        let area = single && (w > 1024 || h > 1024);
        if area && !(th == 1 && tw < 20 * 1024 * 1024) {
            block = (block.0.min(1024), block.1.min(1024));
        }
        area
    };
    let mut out = Vec::new();
    let (mut ow, mut oh) = (w, h);
    while out.len() + 1 < resolutions as usize && (ow > 128 || oh > 128) && (decode_area || (tw % 2 == 0 && th % 2 == 0)) {
        ow = ow.div_ceil(2);
        oh = oh.div_ceil(2);
        if !decode_area {
            tw /= 2;
            th /= 2;
        }
        out.push((ow, oh));
    }
    (out, block)
}

impl crate::cog::WindowSource for Jp2Window {
    fn geotransform_present(&self) -> Option<bool> {
        Some(self.referenced)
    }
    fn overviews(&self) -> Vec<(usize, usize)> {
        self.overviews.clone()
    }

    fn block_size(&self) -> Option<(usize, usize)> {
        Some(self.block)
    }

    fn streamed(&self) -> Option<&dyn crate::cog::StreamedSource> {
        Some(self)
    }

    fn read(&mut self, level: usize, window: crate::Window, bands: &[usize]) -> Result<RasterData> {
        self.read_level(level, window, bands)
    }
}

impl crate::cog::StreamedSource for Jp2Window {
    fn inputs(&self) -> Vec<String> {
        match &self.source {
            StreamSource::File(p) => vec![p.to_string_lossy().into_owned()],
            _ => Vec::new(),
        }
    }

    fn read_rows(&self, window: crate::Window, bands: &[usize]) -> Result<RasterData> {
        self.read_level(0, window, bands)
    }
}

impl Jp2Window {
    /// Reads a window of some bands (numbered from 1) of a resolution level, band after band.
    fn read_level(&self, level: usize, window: crate::Window, bands: &[usize]) -> Result<RasterData> {
        let [x0, y0, x1, y1] = self.header.area;
        let r = level as u32;
        let edge =
            |o: u32, p: usize, end: u32| -> u32 { o.saturating_add(u32::try_from(p).unwrap_or(u32::MAX).saturating_mul(1 << r)).min(end) };
        let area =
            [edge(x0, window.x, x1), edge(y0, window.y, y1), edge(x0, window.x + window.width, x1), edge(y0, window.y + window.height, y1)];
        // The tiles the area covers, decoded in parallel when there are several.
        let (tw, th) = (self.header.tile.0.max(1), self.header.tile.1.max(1));
        let (tx0, ty0) = self.header.tile_origin;
        let span = |a0: u32, a1: u32, o: u32, t: u32| ((a0 - o.min(a0)) / t)..=((a1 - 1 - o.min(a1 - 1)) / t);
        let cols: Vec<u32> = span(area[0], area[2], tx0, tw).collect();
        let rows: Vec<u32> = span(area[1], area[3], ty0, th).collect();
        let parts: Vec<[u32; 4]> = rows
            .iter()
            .flat_map(|j| {
                cols.iter().map(move |i| {
                    [area[0].max(tx0 + i * tw), area[1].max(ty0 + j * th), area[2].min(tx0 + (i + 1) * tw), area[3].min(ty0 + (j + 1) * th)]
                })
            })
            .collect();
        let (w, h) = (window.width, window.height);
        let all = self.header.precisions.len();
        let len = crate::dataset::fallible::len(&[w, h, all], "the JPEG 2000 window")?;
        let window_buffer = |t| crate::dataset::fallible::raster_data(t, len, 0.0, "the JPEG 2000 window");
        // A window of several parts: its buffer first, fallibly, as its size comes from the header.
        let mut out: Option<RasterData> = None;
        if parts.len() > 1 {
            out = Some(window_buffer(codec::data_type(&self.header.precisions, self.header.signed, self.alpha))?);
        }
        let decoded: Vec<Result<(usize, usize, RasterData)>> = if parts.len() == 1 {
            vec![self.decode(area, r)]
        } else {
            crate::par::map_range_coarse(parts.len(), |k| self.decode(parts[k], r))
        };
        let (ox, oy) = (ceil_shift(area[0], r), ceil_shift(area[1], r));
        for (part, d) in parts.iter().zip(decoded) {
            let (pw, ph, data) = d?;
            let (px, py) = (ceil_shift(part[0], r) - ox, ceil_shift(part[1], r) - oy);
            if px + pw > w || py + ph > h {
                return Err(Error::InvalidData(format!("JPEG 2000 decoded {pw}x{ph} pixels at ({px}, {py}) for a {w}x{h} window")));
            }
            if parts.len() == 1 && (pw, ph) == (w, h) {
                out = Some(data);
                break;
            }
            if out.as_ref().is_none_or(|o| o.data_type() != data.data_type()) {
                drop(out.take());
                out = Some(window_buffer(data.data_type())?);
            }
            let Some(dst) = out.as_mut() else { unreachable!("allocated above") };
            macro_rules! place {
                ($($v:ident),*) => {
                    match (dst, &data) {
                        $((RasterData::$v(o), RasterData::$v(src)) => {
                            for b in 0..all {
                                for row in 0..ph {
                                    let s0 = b * pw * ph + row * pw;
                                    let d0 = b * w * h + (py + row) * w + px;
                                    o[d0..d0 + pw].copy_from_slice(&src[s0..s0 + pw]);
                                }
                            }
                        })*
                        _ => return Err(Error::InvalidData("JPEG 2000 tiles of different sample types".into())),
                    }
                };
            }
            place!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64);
        }
        let data = out.ok_or_else(|| Error::InvalidData("empty JPEG 2000 window".into()))?;
        if data.len() != w * h * all {
            return Err(Error::InvalidData(format!("JPEG 2000 decoded {} values for a {w}x{h} window of {all} bands", data.len())));
        }
        if bands.len() == all && bands.iter().enumerate().all(|(i, b)| *b == i + 1) {
            return Ok(data);
        }
        let n = w * h;
        macro_rules! pick {
            ($($v:ident),*) => {
                match &data {
                    $(RasterData::$v(src) => {
                        let mut out = Vec::new();
                        crate::dataset::fallible::reserve(&mut out, crate::dataset::fallible::len(&[bands.len(), n], "the JPEG 2000 window")?, "the JPEG 2000 window")?;
                        for b in bands {
                            out.extend_from_slice(&src[(b - 1) * n..b * n]);
                        }
                        RasterData::$v(out)
                    })*
                }
            };
        }
        Ok(pick!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64))
    }
}

/// Opens a JPEG 2000 file (a local path, URL or object store path) to be read by window and
/// resolution level. `None` when the file is not JPEG 2000.
pub(crate) fn open_lazy(path: &str) -> Option<Result<crate::LazyRaster>> {
    use crate::vfs::Location;
    use std::io::Read;
    let source = match Location::parse(path) {
        Location::Local(p) => {
            let mut f = std::fs::File::open(&p).ok()?;
            let mut magic = [0u8; 12];
            let n = f.read(&mut magic).ok()?;
            if !is_jpeg2000(&magic[..n]) {
                return None;
            }
            StreamSource::File(p)
        }
        #[cfg(remote)]
        loc @ (Location::Url(_) | Location::S3 { .. }) => {
            let lower = path.to_ascii_lowercase();
            let lower = lower.split(['?', '#']).next().unwrap_or_default();
            if ![".jp2", ".j2k", ".j2c", ".jpx"].iter().any(|e| lower.ends_with(e)) {
                return None;
            }
            let remote = loc.remote()?;
            // Small blocks: tile part headers are read one after another, their data skipped.
            let options = crate::http::HttpOptions { block_size: 16 * 1024, ..crate::http::HttpOptions::from_env() };
            match crate::http::RangeReader::open_with(&remote, options) {
                Ok(r) => StreamSource::Http(r),
                Err(e) => return Some(Err(e)),
            }
        }
        _ => return None,
    };
    // Read as JPEG 2000, found by its content (a local file, whatever its name) or its name: only
    // if the JP2 driver is allowed.
    if let Err(e) = super::check_recognised(crate::formats::RasterDriver::JP2, path) {
        return Some(Err(e));
    }
    Some(open_lazy_source(source, Some(path)))
}

/// Opens a JPEG 2000 file held in memory to be read by window and resolution level.
pub(crate) fn open_lazy_bytes(bytes: Vec<u8>) -> Result<crate::LazyRaster> {
    open_lazy_source(StreamSource::Bytes(bytes.into()), None)
}

fn open_lazy_source(source: StreamSource, path: Option<&str>) -> Result<crate::LazyRaster> {
    let (reader, mut ds) = Jp2Window::open(source).map_err(|e| match path {
        Some(p) => Error::InvalidData(format!("{p}: {e}")),
        None => e,
    })?;
    if let Some(p) = path {
        super::nodata::apply_pam(p, &mut ds);
    }
    Ok(crate::LazyRaster::from_source(ds, Box::new(reader)))
}

/// Reads a JPEG 2000 file, its tiles decoded in parallel.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    read_source(StreamSource::File(path.to_path_buf())).map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))
}

/// The width, height and band count of a JP2 file or J2K codestream held in memory, from its
/// headers, without decoding it.
#[cfg_attr(not(feature = "grib"), allow(dead_code))]
pub(crate) fn size_of_bytes(bytes: &[u8]) -> Result<(usize, usize, usize)> {
    let (_, ds) = Jp2Window::open(StreamSource::Bytes(bytes.into()))?;
    Ok((ds.raster.cols, ds.raster.rows, ds.raster.bands))
}

/// Reads a JP2 file or a J2K codestream held in memory.
pub(crate) fn read_bytes(bytes: &[u8]) -> Result<RasterDataset> {
    read_source(StreamSource::Bytes(bytes.into()))
}

fn read_source(source: StreamSource) -> Result<RasterDataset> {
    use crate::cog::WindowSource;
    let (mut reader, mut ds) = Jp2Window::open(source)?;
    let (w, h, bands) = (ds.raster.cols, ds.raster.rows, ds.raster.bands);
    let all: Vec<usize> = (1..=bands).collect();
    ds.raster.data = reader.read(0, crate::Window::full(w, h), &all)?;
    Ok(ds)
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
    fn pipeline_exports_unreferenced_codestream_without_inventing_a_transform() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("image.j2k");
        let output = directory.path().join("image.tif");
        let bytes = codec::encode(4, 4, &[codec::Component { precision: 8, signed: false, data: vec![7; 16] }]).unwrap();
        std::fs::write(&source, bytes).unwrap();
        let reader = crate::LazyRaster::open(source.to_str().unwrap()).unwrap();
        assert!(!reader.geotransform_present());
        crate::ops::pipeline::Pipeline::parse(&format!("read '{}' ! write '{}'", source.display(), output.display()))
            .unwrap()
            .run()
            .unwrap();
        let tags = crate::geotiff::read_geo_tags(&output).unwrap();
        assert!(tags.geo_transform.is_none());
        assert_eq!(crate::open_raster(output.to_str().unwrap()).unwrap().raster.data.to_f64_vec(), vec![7.; 16]);
    }
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

    /// A tiled JPEG 2000 file written as a COG or a GeoTIFF from a lazy read, a strip of rows at a
    /// time, is what is written from the file read whole.
    #[test]
    fn tiled_files_are_written_in_strips() {
        use crate::{Dataset, RasterWriteOptions, formats::RasterDriver as D};
        let dir = tempfile::tempdir().unwrap();
        let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
        // 3 x 3 tiles of 1024 pixels, in strips of 1024 rows; and 3 bands in strips of 1024 rows.
        let (w, h) = (2300, 2100);
        let one = RasterData::U16((0..w * h).map(|i| ((i * 7919) % 4099) as u16).collect());
        let three = RasterData::U8((0..w * h * 3).map(|i| ((i * 31) % 251) as u8).collect());
        for (name, bands, data) in [("one", 1, one), ("three", 3, three)] {
            let gt = [500000.0, 10.0, 0.0, 4100000.0, 0.0, -10.0];
            let r = common::make_raster(w, h, bands, data, gt, CrsInfo::from_epsg(32611), None).unwrap();
            let src = path(&format!("{name}.jp2"));
            write(&r, Some(0.0), Path::new(&src)).unwrap();
            let Dataset::Raster(whole) = crate::open(&src).unwrap() else { panic!("not a raster") };
            for driver in [D::Cog, D::GTiff] {
                let options = RasterWriteOptions { driver: Some(driver), compression: Some("DEFLATE".into()), ..Default::default() };
                let (a, b) = (path(&format!("{name}_whole.tif")), path(&format!("{name}_strips.tif")));
                crate::write_raster_dataset(&whole, &a, &options).unwrap();
                crate::open_raster_lazy(&src).unwrap().write(&b, &options).unwrap();
                if driver == D::Cog {
                    assert!(std::fs::read(&a).unwrap() == std::fs::read(&b).unwrap(), "{name}: COGs differ");
                }
                let (Dataset::Raster(a), Dataset::Raster(b)) = (crate::open(&a).unwrap(), crate::open(&b).unwrap()) else {
                    panic!("not rasters")
                };
                assert!(a.raster.data.to_f64_vec() == b.raster.data.to_f64_vec(), "{name} {driver:?}: pixels differ");
                assert_eq!(a.geo_transform(), b.geo_transform());
                assert_eq!(a.crs().epsg, b.crs().epsg);
                assert_eq!((a.raster.nodata, a.has_nodata), (b.raster.nodata, b.has_nodata));
                assert_eq!(a.raster.metadata, b.raster.metadata);
            }
        }
    }

    #[test]
    fn parses_gmljp2_origins() {
        let xml = r#"<gml:RectifiedGrid dimension="2"><gml:origin><gml:Point srsName="urn:ogc:def:crs:EPSG::32633"><gml:pos>300005 5000035</gml:pos></gml:Point></gml:origin><gml:offsetVector srsName="urn:ogc:def:crs:EPSG::32633">10 0</gml:offsetVector><gml:offsetVector srsName="urn:ogc:def:crs:EPSG::32633">0 -10</gml:offsetVector></gml:RectifiedGrid>"#;
        let (gt, crs) = gml_georeferencing(xml);
        assert_eq!(gt.unwrap(), [300000.0, 10.0, 0.0, 5000040.0, 0.0, -10.0]);
        assert_eq!(crs.unwrap().epsg, Some(32633));
    }
}

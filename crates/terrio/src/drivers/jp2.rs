//! JPEG 2000 (`.jp2`, `.j2k`), coded with `openjp2`, a pure-Rust port of OpenJPEG (the codec
//! behind GDAL's JP2OpenJPEG driver), so decoded pixels match GDAL's, signed samples included.
//!
//! Georeferencing is read from the GeoJP2 box (a small GeoTIFF) or, failing that, from the GML
//! (GMLJP2) box, as GDAL reads them; it is written as a GeoJP2 box. Nodata goes to a `.aux.xml`
//! sidecar, where GDAL keeps it for JPEG 2000. Files are written losslessly (the reversible 5/3
//! wavelet).

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
#[cfg(not(target_arch = "wasm32"))]
mod codec {
    use crate::{Error, Result};
    use core::ffi::c_void;
    use openjp2::openjpeg::*;

    /// An in-memory buffer that OpenJPEG streams read from or write to.
    struct Buffer {
        data: Vec<u8>,
        pos: usize,
    }

    unsafe extern "C" fn read_fn(dst: *mut c_void, n: usize, user: *mut c_void) -> usize {
        // SAFETY: `user` is the `Buffer` registered with the stream, alive while the stream is.
        let b = unsafe { &mut *(user as *mut Buffer) };
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

    unsafe extern "C" fn write_fn(src: *mut c_void, n: usize, user: *mut c_void) -> usize {
        // SAFETY: as in `read_fn`; `src` holds `n` bytes.
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
        // SAFETY: as in `read_fn`.
        let b = unsafe { &mut *(user as *mut Buffer) };
        let new = (b.pos as i64 + n).max(0) as usize;
        b.pos = new;
        n
    }

    unsafe extern "C" fn seek_fn(n: i64, user: *mut c_void) -> i32 {
        // SAFETY: as in `read_fn`.
        let b = unsafe { &mut *(user as *mut Buffer) };
        b.pos = n.max(0) as usize;
        1
    }

    /// One decoded component.
    pub(super) struct Component {
        pub precision: u32,
        pub signed: bool,
        pub data: Vec<i32>,
    }

    /// Decodes a JP2 file or a J2K codestream: `(width, height, components)`.
    pub(super) fn decode(bytes: Vec<u8>) -> Result<(usize, usize, Vec<Component>)> {
        let format = if bytes.starts_with(openjp2::J2K_CODESTREAM_MAGIC) { OPJ_CODEC_J2K } else { OPJ_CODEC_JP2 };
        let len = bytes.len() as u64;
        let buffer = Box::into_raw(Box::new(Buffer { data: bytes, pos: 0 }));
        // SAFETY: the stream, codec and image are created, used and destroyed here; the buffer
        // outlives the stream and is reclaimed at the end.
        unsafe {
            let stream = opj_stream_default_create(1);
            opj_stream_set_read_function(stream, Some(read_fn));
            opj_stream_set_skip_function(stream, Some(skip_fn));
            opj_stream_set_seek_function(stream, Some(seek_fn));
            opj_stream_set_user_data_length(stream, len);
            opj_stream_set_user_data(stream, buffer as *mut c_void, None);
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
                    let out = comps
                        .iter()
                        .map(|c| Component {
                            precision: c.prec,
                            signed: c.sgnd != 0,
                            data: std::slice::from_raw_parts(c.data, w * h).to_vec(),
                        })
                        .collect();
                    Ok((w, h, out))
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
            drop(Box::from_raw(buffer));
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

/// Without OpenJPEG (WebAssembly builds), JPEG 2000 is not available.
#[cfg(target_arch = "wasm32")]
mod codec {
    use crate::{Error, Result};

    /// One decoded component.
    pub(super) struct Component {
        pub precision: u32,
        pub signed: bool,
        pub data: Vec<i32>,
    }

    pub(super) fn decode(_bytes: Vec<u8>) -> Result<(usize, usize, Vec<Component>)> {
        Err(Error::Unsupported("JPEG 2000 is not available in WebAssembly builds".into()))
    }

    pub(super) fn encode(_w: usize, _h: usize, _comps: &[Component]) -> Result<Vec<u8>> {
        Err(Error::Unsupported("JPEG 2000 is not available in WebAssembly builds".into()))
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

/// Parses a GMLJP2 `RectifiedGrid`: origin (a pixel centre) and offset vectors, with the axes
/// swapped for geographic EPSG CRSs, whose axis order is latitude first.
fn gml_georeferencing(xml: &str) -> (Option<[f64; 6]>, Option<CrsInfo>) {
    let grid = &xml[xml.find("RectifiedGrid").unwrap_or(0)..];
    let srs = grid
        .split("srsName=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .or_else(|| xml.split("srsName=\"").nth(1).and_then(|s| s.split('"').next()));
    let epsg = srs.and_then(|s| s.rsplit([':', '/']).next()).and_then(|c| c.parse::<u32>().ok());
    let numbers = |tag: &str| -> Vec<Vec<f64>> {
        grid.split(&format!("<gml:{tag}"))
            .skip(1)
            .filter_map(|s| s.split_once('>').map(|(_, rest)| rest.split('<').next().unwrap_or_default()))
            .map(|t| t.split([' ', ',', '\n', '\t']).filter_map(|v| v.trim().parse().ok()).collect())
            .collect()
    };
    let origin = numbers("pos").into_iter().next().or_else(|| numbers("coordinates").into_iter().next());
    let vectors = numbers("offsetVector");
    let (Some(o), [v1, v2, ..]) = (origin, vectors.as_slice()) else {
        return (None, epsg.map(CrsInfo::from_epsg));
    };
    if o.len() < 2 || v1.len() < 2 || v2.len() < 2 {
        return (None, epsg.map(CrsInfo::from_epsg));
    }
    let geographic = epsg.is_some_and(|c| crate::crs::SpatialRef::from_epsg(c).is_ok_and(|s| s.is_geographic()));
    let (ox, oy, a, b, c, d) = if geographic { (o[1], o[0], v1[1], v1[0], v2[1], v2[0]) } else { (o[0], o[1], v1[0], v1[1], v2[0], v2[1]) };
    let gt = [ox - (a + c) / 2.0, a, c, oy - (b + d) / 2.0, b, d];
    (Some(gt), epsg.map(CrsInfo::from_epsg))
}

/// Reads a JPEG 2000 file.
pub(crate) fn read(path: &Path) -> Result<RasterDataset> {
    let bytes = std::fs::read(path)?;
    let (gt, crs) = georeferencing(&bytes);
    let (cols, rows, comps) = decode(bytes).map_err(|e| Error::InvalidData(format!("{}: {e}", path.display())))?;
    let max_prec = comps.iter().map(|c| c.precision).max().unwrap_or(8);
    let signed = comps.first().is_some_and(|c| c.signed);
    let all: Vec<i32> = comps.into_iter().flat_map(|c| c.data).collect();
    let data = match (max_prec, signed) {
        (0..=8, false) => RasterData::U8(all.into_iter().map(|v| v as u8).collect()),
        (0..=8, true) => RasterData::I8(all.into_iter().map(|v| v as i8).collect()),
        (9..=16, false) => RasterData::U16(all.into_iter().map(|v| v as u16).collect()),
        (9..=16, true) => RasterData::I16(all.into_iter().map(|v| v as i16).collect()),
        (_, false) => RasterData::U32(all.into_iter().map(|v| v as u32).collect()),
        (_, true) => RasterData::I32(all),
    };
    let bands = data.len() / (cols * rows).max(1);
    let raster = common::make_raster(cols, rows, bands, data, gt.unwrap_or(common::NO_GEOTRANSFORM), crs.unwrap_or_default(), None)?;
    Ok(RasterDataset { raster, driver: RasterDriver::JP2, has_nodata: false })
}

/// Writes a lossless JP2 with a GeoJP2 box; nodata goes to a `.aux.xml` sidecar.
pub(crate) fn write(r: &Raster, nodata: Option<f64>, path: &Path) -> Result<()> {
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
    let values: Vec<i32> = match &r.data {
        RasterData::U8(v) => v.iter().map(|x| *x as i32).collect(),
        RasterData::I8(v) => v.iter().map(|x| *x as i32).collect(),
        RasterData::U16(v) => v.iter().map(|x| *x as i32).collect(),
        RasterData::I16(v) => v.iter().map(|x| *x as i32).collect(),
        _ => unreachable!("checked above"),
    };
    let comps: Vec<Component> = values.chunks(n).map(|c| Component { precision, signed, data: c.to_vec() }).collect();
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
    std::fs::write(path, out)?;
    common::write_pam_with(path, None, nodata, r.bands)
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

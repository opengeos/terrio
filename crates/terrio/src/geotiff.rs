//! Reads a GeoTIFF's georeferencing tags directly, to fill in what the raster driver misses:
//! `ModelTransformationTag` geotransforms, user-defined projected CRSs (common in NASA products)
//! and `GDAL_NODATA`.

use std::io::{Read, Seek, SeekFrom};

const MODEL_PIXEL_SCALE: u16 = 33550;
const MODEL_TIEPOINT: u16 = 33922;
const MODEL_TRANSFORMATION: u16 = 34264;
const GEO_KEY_DIRECTORY: u16 = 34735;
const GEO_DOUBLE_PARAMS: u16 = 34736;
const GEO_ASCII_PARAMS: u16 = 34737;
const GDAL_NODATA: u16 = 42113;
const PHOTOMETRIC: u16 = 262;

/// The georeferencing read from a GeoTIFF's first image.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct GeoTags {
    /// GDAL-style geotransform: `[x_origin, pixel_width, row_rotation, y_origin, col_rotation, pixel_height]`.
    pub geo_transform: Option<[f64; 6]>,
    /// The EPSG code of the CRS, from the GeoKeys.
    pub epsg: Option<u32>,
    /// Whether the raster type is `RasterPixelIsPoint`.
    pub pixel_is_point: bool,
    /// The `GDAL_NODATA` value.
    pub nodata: Option<f64>,
    /// For palette images, the file offset of the `PhotometricInterpretation` value.
    pub palette_photometric_offset: Option<u64>,
}

/// One TIFF tag's raw values.
enum Values {
    Shorts(Vec<u16>),
    Doubles(Vec<f64>),
    Ascii(String),
}

/// Reads the georeferencing tags of a TIFF file's first image. Returns `None` when the file is not
/// a TIFF.
pub(crate) fn read_geo_tags(path: &std::path::Path) -> Option<GeoTags> {
    let mut f = std::io::BufReader::new(std::fs::File::open(path).ok()?);
    let mut header = [0u8; 16];
    f.read_exact(&mut header[..8]).ok()?;
    let le = match &header[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |b: &[u8]| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) };
    let u32_at = |b: &[u8]| {
        let a = [b[0], b[1], b[2], b[3]];
        if le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) }
    };
    let u64_at = |b: &[u8]| {
        let a: [u8; 8] = b[..8].try_into().unwrap_or_default();
        if le { u64::from_le_bytes(a) } else { u64::from_be_bytes(a) }
    };
    let big = match u16_at(&header[2..4]) {
        42 => false,
        43 => true,
        _ => return None,
    };
    let ifd_offset = if big {
        f.read_exact(&mut header[8..16]).ok()?;
        u64_at(&header[8..16])
    } else {
        u32_at(&header[4..8]) as u64
    };

    f.seek(SeekFrom::Start(ifd_offset)).ok()?;
    let count = if big {
        let mut b = [0u8; 8];
        f.read_exact(&mut b).ok()?;
        u64_at(&b)
    } else {
        let mut b = [0u8; 2];
        f.read_exact(&mut b).ok()?;
        u16_at(&b) as u64
    };
    let entry_len = if big { 20 } else { 12 };
    let mut entries = vec![0u8; (count as usize).min(4096) * entry_len];
    f.read_exact(&mut entries).ok()?;

    let mut tags: Vec<(u16, Values)> = Vec::new();
    let mut palette_offset = None;
    for e in entries.chunks(entry_len) {
        let tag = u16_at(&e[0..2]);
        if tag == PHOTOMETRIC && u16_at(&e[2..4]) == 3 {
            let value = &e[if big { 12 } else { 8 }..];
            if u16_at(value) == 3 {
                let entry_start = f.stream_position().ok()? - entries.len() as u64;
                let index = (e.as_ptr() as usize - entries.as_ptr() as usize) as u64;
                palette_offset = Some(entry_start + index + if big { 12 } else { 8 });
            }
            continue;
        }
        if !matches!(
            tag,
            MODEL_PIXEL_SCALE
                | MODEL_TIEPOINT
                | MODEL_TRANSFORMATION
                | GEO_KEY_DIRECTORY
                | GEO_DOUBLE_PARAMS
                | GEO_ASCII_PARAMS
                | GDAL_NODATA
        ) {
            continue;
        }
        let typ = u16_at(&e[2..4]);
        let (n, inline, value_bytes) =
            if big { (u64_at(&e[4..12]), &e[12..20], 8usize) } else { (u32_at(&e[4..8]) as u64, &e[8..12], 4usize) };
        let size = match typ {
            2 => 1,  // ASCII
            3 => 2,  // SHORT
            12 => 8, // DOUBLE
            _ => continue,
        };
        let len = (n as usize).checked_mul(size)?;
        if len > 1 << 24 {
            continue;
        }
        let data = if len <= value_bytes {
            inline[..len].to_vec()
        } else {
            let off = if big { u64_at(inline) } else { u32_at(inline) as u64 };
            let pos = f.stream_position().ok()?;
            f.seek(SeekFrom::Start(off)).ok()?;
            let mut d = vec![0u8; len];
            f.read_exact(&mut d).ok()?;
            f.seek(SeekFrom::Start(pos)).ok()?;
            d
        };
        let values = match typ {
            2 => Values::Ascii(String::from_utf8_lossy(&data).trim_end_matches('\0').to_string()),
            3 => Values::Shorts(data.chunks(2).map(u16_at).collect()),
            _ => Values::Doubles(data.chunks(8).map(|c| f64::from_bits(u64_at(c))).collect()),
        };
        tags.push((tag, values));
    }

    let doubles = |t: u16| {
        tags.iter().find_map(|(k, v)| match v {
            Values::Doubles(d) if *k == t => Some(d.as_slice()),
            _ => None,
        })
    };
    let ascii = |t: u16| {
        tags.iter().find_map(|(k, v)| match v {
            Values::Ascii(s) if *k == t => Some(s.as_str()),
            _ => None,
        })
    };
    let keys = tags.iter().find_map(|(k, v)| match v {
        Values::Shorts(s) if *k == GEO_KEY_DIRECTORY => Some(s.as_slice()),
        _ => None,
    });

    let mut out = GeoTags { palette_photometric_offset: palette_offset, ..Default::default() };
    if let Some(m) = doubles(MODEL_TRANSFORMATION).filter(|m| m.len() >= 8) {
        out.geo_transform = Some([m[3], m[0], m[1], m[7], m[4], m[5]]);
    } else if let (Some(scale), Some(tie)) = (doubles(MODEL_PIXEL_SCALE), doubles(MODEL_TIEPOINT))
        && scale.len() >= 2
        && tie.len() >= 6
    {
        let (i, j, x, y) = (tie[0], tie[1], tie[3], tie[4]);
        out.geo_transform = Some([x - i * scale[0], scale[0], 0.0, y + j * scale[1], 0.0, -scale[1]]);
    }
    out.nodata = ascii(GDAL_NODATA).and_then(|s| parse_nodata(s.trim()));

    if let Some(keys) = keys {
        let key = |id: u16| -> Option<(u16, u16, u16)> {
            keys.get(4..)?.chunks(4).find(|k| k.len() == 4 && k[0] == id).map(|k| (k[1], k[2], k[3]))
        };
        let short_key = |id: u16| key(id).filter(|(loc, _, _)| *loc == 0).map(|(_, _, v)| v);
        let ascii_key = |id: u16| {
            key(id).and_then(|(loc, n, off)| {
                (loc == GEO_ASCII_PARAMS).then(|| ascii(GEO_ASCII_PARAMS)).flatten().map(|s| {
                    let start = (off as usize).min(s.len());
                    let end = (start + n as usize).min(s.len());
                    s[start..end].trim_end_matches('|').to_string()
                })
            })
        };
        out.pixel_is_point = short_key(1025) == Some(2);
        let projected = short_key(3072);
        let geographic = short_key(2048);
        out.epsg = match (short_key(1024), projected, geographic) {
            (_, Some(code), _) if code != 0 && code != 32767 => Some(code as u32),
            (Some(2), _, Some(code)) if code != 0 && code != 32767 => Some(code as u32),
            (Some(1), _, _) => user_defined_projected(short_key(3074), geographic, ascii_key(1026).or_else(|| ascii_key(3073))),
            _ => None,
        };
    }
    Some(out)
}

fn parse_nodata(s: &str) -> Option<f64> {
    match s.to_ascii_lowercase().as_str() {
        "nan" => Some(f64::NAN),
        "inf" | "+inf" => Some(f64::INFINITY),
        "-inf" => Some(f64::NEG_INFINITY),
        _ => s.parse().ok(),
    }
}

/// The EPSG code of a user-defined projected CRS, from its projection code (UTM zones) or its
/// citation (such as `WGS 84 / UTM zone 10N`).
fn user_defined_projected(projection: Option<u16>, geographic: Option<u16>, citation: Option<String>) -> Option<u32> {
    // Datum: WGS 84 unless the geographic CRS says NAD83 or the citation does.
    let citation = citation.unwrap_or_default();
    let nad83 = geographic == Some(4269) || citation.contains("NAD83");
    let nad27 = geographic == Some(4267) || citation.contains("NAD27");
    let utm = match projection {
        Some(p @ 16001..=16060) => Some(((p - 16000) as u32, true)),
        Some(p @ 16101..=16160) => Some(((p - 16100) as u32, false)),
        _ => parse_utm_citation(&citation),
    };
    if let Some((zone, north)) = utm {
        return match (nad83, nad27, north) {
            (true, _, true) if (1..=23).contains(&zone) => Some(26900 + zone),
            (_, true, true) if (1..=22).contains(&zone) => Some(26700 + zone),
            (false, false, true) => Some(32600 + zone),
            (false, false, false) => Some(32700 + zone),
            _ => None,
        };
    }
    let name = citation.split('|').next().unwrap_or_default().trim();
    if name.is_empty() { None } else { wbprojection::epsg_from_srs_reference(name).or_else(|| identify_by_name(name)) }
}

/// Parses `UTM zone 10N` (or `UTM Zone 10, Northern Hemisphere`) from a citation.
fn parse_utm_citation(c: &str) -> Option<(u32, bool)> {
    let lower = c.to_ascii_lowercase();
    let rest = &lower[lower.find("utm zone")? + "utm zone".len()..];
    let rest = rest.trim_start();
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let zone: u32 = digits.parse().ok()?;
    let after = &rest[digits.len()..];
    let north = !(after.starts_with('s') || after.contains("south"));
    Some((zone, north))
}

/// Finds an EPSG code by its CRS name, among the codes terrio knows.
fn identify_by_name(name: &str) -> Option<u32> {
    wbprojection::epsg::known_epsg_codes()
        .into_iter()
        .find(|code| wbprojection::epsg::epsg_info(*code).is_some_and(|i| i.name.eq_ignore_ascii_case(name)))
}

/// Reads a GeoTIFF (or BigTIFF, or COG) into memory: the full-resolution image of the first IFD.
pub(crate) fn read_geotiff(path: &std::path::Path) -> crate::Result<crate::RasterDataset> {
    use crate::Error;
    use tiff::decoder::{Decoder, DecodingResult, Limits};
    use tiff::tags::Tag;
    use wbraster::raster::RasterData;

    let tiff_err = |e: tiff::TiffError| Error::InvalidData(format!("{}: {e}", path.display()));
    let tags = read_geo_tags(path).unwrap_or_default();
    // The decoder rejects palette images; read their indices as grayscale instead.
    let file = PatchedReader {
        inner: std::io::BufReader::new(std::fs::File::open(path)?),
        pos: 0,
        patch: tags.palette_photometric_offset.map(|off| (off, if is_little_endian(path) { [1u8, 0] } else { [0u8, 1] })),
    };
    let mut dec = Decoder::new(file).map_err(tiff_err)?.with_limits(Limits::unlimited());
    let (w, h) = dec.dimensions().map_err(tiff_err)?;
    let (w, h) = (w as usize, h as usize);
    let samples = dec.find_tag_unsigned::<u16>(Tag::SamplesPerPixel).map_err(tiff_err)?.unwrap_or(1).max(1) as usize;
    let planar = dec.find_tag_unsigned::<u16>(Tag::PlanarConfiguration).map_err(tiff_err)? == Some(2);
    let (cw, ch) = dec.chunk_dimensions();
    let (cw, ch) = (cw as usize, ch as usize);
    let across = w.div_ceil(cw);
    let down = h.div_ceil(ch);
    let per_plane = across * down;
    let plane_count = if planar { samples } else { 1 };
    let band_len = w * h;

    // Band-major output, typed after the first chunk decoded.
    let mut data: Option<RasterData> = None;
    for plane in 0..plane_count {
        for cy in 0..down {
            for cx in 0..across {
                let idx = (plane * per_plane + cy * across + cx) as u32;
                let chunk = dec.read_chunk(idx).map_err(tiff_err)?;
                let (dw, dh) = dec.chunk_data_dimensions(idx);
                let (dw, dh) = (dw as usize, dh as usize);
                let spp = if planar { 1 } else { samples };
                let x0 = cx * cw;
                let y0 = cy * ch;
                macro_rules! place {
                    ($vals:expr, $variant:ident, $conv:expr) => {{
                        let vals = $vals;
                        let out = data.get_or_insert_with(|| RasterData::$variant(vec![Default::default(); band_len * samples]));
                        let RasterData::$variant(out) = out else {
                            return Err(Error::InvalidData("TIFF chunks differ in sample type".into()));
                        };
                        // Rows in a chunk may be padded to the full chunk width.
                        let stride = if dh == 0 { 0 } else { vals.len() / (dh * spp) };
                        for row in 0..dh.min(h - y0) {
                            for col in 0..dw.min(w - x0) {
                                let src = (row * stride + col) * spp;
                                let dst = (y0 + row) * w + x0 + col;
                                for s in 0..spp {
                                    let band = if planar { plane } else { s };
                                    if let Some(v) = vals.get(src + s) {
                                        out[band * band_len + dst] = $conv(*v);
                                    }
                                }
                            }
                        }
                    }};
                }
                match chunk {
                    DecodingResult::U8(v) => place!(v, U8, |x| x),
                    DecodingResult::U16(v) => place!(v, U16, |x| x),
                    DecodingResult::U32(v) => place!(v, U32, |x| x),
                    DecodingResult::U64(v) => place!(v, U64, |x| x),
                    DecodingResult::I8(v) => place!(v, I8, |x| x),
                    DecodingResult::I16(v) => place!(v, I16, |x| x),
                    DecodingResult::I32(v) => place!(v, I32, |x| x),
                    DecodingResult::I64(v) => place!(v, I64, |x| x),
                    DecodingResult::F16(v) => place!(v, F32, |x: half::f16| x.to_f32()),
                    DecodingResult::F32(v) => place!(v, F32, |x| x),
                    DecodingResult::F64(v) => place!(v, F64, |x| x),
                }
            }
        }
    }
    let data = data.ok_or_else(|| Error::InvalidData(format!("{}: no image data", path.display())))?;
    let cfg = wbraster::RasterConfig {
        cols: w,
        rows: h,
        bands: samples,
        nodata: tags.nodata.unwrap_or(-9999.0),
        data_type: data.data_type(),
        crs: tags.epsg.map(wbraster::CrsInfo::from_epsg).unwrap_or_default(),
        ..Default::default()
    };
    let mut raster = wbraster::Raster::from_data_native(cfg, data)?;
    match tags.geo_transform {
        Some(gt) if gt[2] == 0.0 && gt[4] == 0.0 && gt[1] > 0.0 && gt[5] < 0.0 => {
            // PixelIsPoint tie points refer to pixel centres; shift to the corner, as GDAL does.
            let (dx, dy) = if tags.pixel_is_point { (gt[1] / 2.0, gt[5] / 2.0) } else { (0.0, 0.0) };
            raster.cell_size_x = gt[1];
            raster.cell_size_y = -gt[5];
            raster.x_min = gt[0] - dx;
            raster.y_min = gt[3] - dy + gt[5] * h as f64;
        }
        Some(_) => {
            return Err(Error::Unsupported(format!("{}: rotated or south-up geotransforms are not supported", path.display())));
        }
        None => {
            raster.cell_size_x = 1.0;
            raster.cell_size_y = 1.0;
            raster.x_min = 0.0;
            raster.y_min = 0.0;
        }
    }
    Ok(crate::RasterDataset { raster, driver: crate::formats::RasterDriver::GTiff, has_nodata: tags.nodata.is_some() })
}

fn is_little_endian(path: &std::path::Path) -> bool {
    let mut b = [0u8; 2];
    std::fs::File::open(path).and_then(|mut f| f.read_exact(&mut b)).is_ok() && &b == b"II"
}

/// A reader that replaces two bytes at one file offset.
struct PatchedReader<R> {
    inner: R,
    pos: u64,
    patch: Option<(u64, [u8; 2])>,
}

impl<R: Read> Read for PatchedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if let Some((off, bytes)) = self.patch {
            for (i, b) in bytes.iter().enumerate() {
                let at = off + i as u64;
                if at >= self.pos && at < self.pos + n as u64 {
                    buf[(at - self.pos) as usize] = *b;
                }
            }
        }
        self.pos += n as u64;
        Ok(n)
    }
}

impl<R: Seek> Seek for PatchedReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.pos = self.inner.seek(pos)?;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_user_defined_utm() {
        assert_eq!(user_defined_projected(Some(16010), None, Some("WGS 84 / UTM zone 10N".into())), Some(32610));
        assert_eq!(user_defined_projected(None, Some(4269), Some("NAD83 / UTM zone 15N".into())), Some(26915));
        assert_eq!(user_defined_projected(Some(16133), None, None), Some(32733));
    }
}

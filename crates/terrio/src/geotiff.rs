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
const GDAL_METADATA: u16 = 42112;
const GDAL_NODATA: u16 = 42113;
const PHOTOMETRIC: u16 = 262;

/// The georeferencing read from a GeoTIFF's first image.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct GeoTags {
    /// GDAL-style geotransform: `[x_origin, pixel_width, row_rotation, y_origin, col_rotation, pixel_height]`.
    pub geo_transform: Option<[f64; 6]>,
    /// The EPSG code of the CRS, from the GeoKeys.
    pub epsg: Option<u32>,
    /// A PROJ string for user-defined projected CRSs without an EPSG code, built from the GeoKeys'
    /// projection parameters.
    pub proj4: Option<String>,
    /// A CRS without an EPSG code, from user-defined GeoKeys.
    pub crs: Option<wbraster::CrsInfo>,
    /// Dataset metadata items from the `GDAL_METADATA` tag.
    pub metadata: Vec<(String, String)>,
    /// Band metadata items from the `GDAL_METADATA` tag: `(band from 0, name, value)`.
    pub band_metadata: Vec<(usize, String, String)>,
    /// Whether the raster type is `RasterPixelIsPoint`.
    pub pixel_is_point: bool,
    /// The `GDAL_NODATA` value.
    pub nodata: Option<f64>,
    /// For palette images, the file offsets of the `PhotometricInterpretation` values, in every
    /// image (overviews included).
    pub palette_photometric_offsets: Vec<u64>,
    /// Whether the file is little-endian.
    pub little_endian: bool,
    /// Ground control points: the tie points of a file without a pixel scale or transformation
    /// matrix, as GDAL reads them.
    pub gcps: Vec<crate::Gcp>,
    /// A GeoKeys projection method terrio cannot read (or whose parameters are incomplete),
    /// which left the raster without CRS: warned of by [`GeoTags::warn_unreadable`].
    pub unreadable_method: Option<u16>,
}

impl GeoTags {
    /// The geotransform of the pixels' corners: PixelIsPoint tie points refer to pixel centres,
    /// shifted by half a pixel as GDAL does. Rotated and south-up geotransforms included.
    pub(crate) fn corner_geo_transform(&self) -> Option<[f64; 6]> {
        let gt = self.geo_transform?;
        if !self.pixel_is_point {
            return Some(gt);
        }
        Some([gt[0] - (gt[1] * 0.5 + gt[2] * 0.5), gt[1], gt[2], gt[3] - (gt[4] * 0.5 + gt[5] * 0.5), gt[4], gt[5]])
    }
}

/// One TIFF tag's raw values.
enum Values {
    Shorts(Vec<u16>),
    Doubles(Vec<f64>),
    Ascii(String),
}

/// The most images (IFDs) looked at for palette images.
const MAX_IFDS: usize = 1024;

/// Reads the georeferencing tags of a TIFF file's first image. Returns `None` when the file is not
/// a TIFF.
pub(crate) fn read_geo_tags(path: &std::path::Path) -> Option<GeoTags> {
    read_geo_tags_from(std::io::BufReader::new(std::fs::File::open(path).ok()?))
}

/// Reads the georeferencing tags of a TIFF stream's first image, and finds the palette images
/// among all its images. Returns `None` when the stream is not a TIFF.
pub(crate) fn read_geo_tags_from<R: Read + Seek>(mut f: R) -> Option<GeoTags> {
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
                | GDAL_METADATA
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
    // The other images: overviews and masks.
    let mut palette_offsets: Vec<u64> = palette_offset.into_iter().collect();
    let mut seen = vec![ifd_offset];
    let mut next = read_offset(&mut f, big, le);
    while let Some(offset) = next.filter(|o| *o != 0 && seen.len() < MAX_IFDS && !seen.contains(o)) {
        seen.push(offset);
        next = palette_offset_in_ifd(&mut f, offset, big, le, &mut palette_offsets);
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

    let mut out = GeoTags { palette_photometric_offsets: palette_offsets, little_endian: le, ..Default::default() };
    let has_scale = doubles(MODEL_PIXEL_SCALE).is_some_and(|s| s.len() >= 2);
    if let Some(tie) = doubles(MODEL_TIEPOINT).filter(|t| t.len() >= 6)
        && doubles(MODEL_TRANSFORMATION).is_none()
        && (tie.len() > 6 || !has_scale)
    {
        out.gcps = tie
            .as_chunks::<6>()
            .0
            .iter()
            .enumerate()
            .map(|(i, t)| crate::Gcp { id: (i + 1).to_string(), info: String::new(), pixel: t[0], line: t[1], x: t[3], y: t[4], z: t[5] })
            .collect();
    }
    if let Some(m) = doubles(MODEL_TRANSFORMATION).filter(|m| m.len() >= 8) {
        out.geo_transform = Some([m[3], m[0], m[1], m[7], m[4], m[5]]);
    } else if let (Some(scale), Some(tie)) = (doubles(MODEL_PIXEL_SCALE), doubles(MODEL_TIEPOINT))
        && scale.len() >= 2
        && tie.len() == 6
    {
        let (i, j, x, y) = (tie[0], tie[1], tie[3], tie[4]);
        // A negative Y scale is taken as positive, as GDAL does: the image is north-up.
        let sy = scale[1].abs();
        out.geo_transform = Some([x - i * scale[0], scale[0], 0.0, y + j * sy, 0.0, -sy]);
    }
    out.nodata = ascii(GDAL_NODATA).and_then(|s| parse_nodata(s.trim()));
    out.metadata = ascii(GDAL_METADATA).map(crate::drivers::gtiff_write::parse_gdal_metadata).unwrap_or_default();
    out.band_metadata = ascii(GDAL_METADATA).map(crate::drivers::gtiff_write::parse_gdal_band_metadata).unwrap_or_default();

    if let Some(keys) = keys {
        apply_geokeys(&mut out, keys, doubles(GEO_DOUBLE_PARAMS), ascii(GEO_ASCII_PARAMS));
    }
    if out.pixel_is_point {
        // GCPs of PixelIsPoint files refer to pixel centres, as GDAL reads them.
        for g in &mut out.gcps {
            g.pixel += 0.5;
            g.line += 0.5;
        }
    }
    Some(out)
}

/// The CRS of a GeoKey directory and its double and ASCII parameters (as stored in GeoTIFF tags
/// or LAS `LASF_Projection` records).
pub(crate) fn crs_from_geokeys(keys: &[u16], doubles: Option<&[f64]>, ascii: Option<&str>) -> wbraster::CrsInfo {
    let mut out = GeoTags::default();
    apply_geokeys(&mut out, keys, doubles, ascii);
    out.warn_unreadable("GeoKeys");
    out.crs_info()
}

/// Sets the CRS (and the raster type) of `out` from a GeoKey directory.
fn apply_geokeys(out: &mut GeoTags, keys: &[u16], doubles: Option<&[f64]>, ascii: Option<&str>) {
    let key =
        |id: u16| -> Option<(u16, u16, u16)> { keys.get(4..)?.chunks(4).find(|k| k.len() == 4 && k[0] == id).map(|k| (k[1], k[2], k[3])) };
    let short_key = |id: u16| key(id).filter(|(loc, _, _)| *loc == 0).map(|(_, _, v)| v);
    let ascii_key = |id: u16| {
        key(id).and_then(|(loc, n, off)| {
            (loc == GEO_ASCII_PARAMS).then_some(ascii).flatten().map(|s| {
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
    if out.epsg.is_none() {
        // Geographic and projected user-defined CRSs, and ESRI PE citations.
        let k = crate::drivers::geokeys::GeoKeys { keys, doubles: doubles.unwrap_or_default(), ascii: ascii.unwrap_or_default() };
        if let Some(crs) = crate::drivers::geokeys::projected_on_epsg_geographic(&k) {
            out.crs = Some(crs);
            return;
        }
        if short_key(1024) == Some(1) && crate::drivers::geokeys::esri_pe_crs(&k).is_none() {
            out.proj4 = crate::drivers::geokeys::user_defined_proj4(&k);
            out.unreadable_method = crate::drivers::geokeys::unmapped_method(&k);
        }
        if out.proj4.is_none() {
            out.crs = crate::drivers::geokeys::user_defined_crs(&k);
            out.epsg = out.crs.as_ref().and_then(|c| c.epsg);
        }
    }
}

impl GeoTags {
    /// Takes the CRS of the `.aux.xml` sidecar of the file at `path`, when it holds one: it
    /// overrides the GeoKeys, as GDAL reads it (GDAL writes there the CRSs GeoKeys cannot hold).
    pub(crate) fn apply_sidecar_crs(&mut self, path: &std::path::Path) {
        if let Some(crs) = crate::drivers::common::read_pam(path).and_then(|p| p.crs) {
            self.epsg = crs.epsg;
            self.proj4 = None;
            self.crs = Some(crs);
            self.unreadable_method = None;
        }
    }

    /// Warns of a GeoKeys projection method that left the raster without CRS (see
    /// [`GeoTags::unreadable_method`]), once the sidecar had its chance to give one. `source`
    /// names the file in the warning.
    pub(crate) fn warn_unreadable(&self, source: &str) {
        if let Some(method) = self.unreadable_method {
            crate::warnings::warn(format!(
                "{source}: GeoTIFF projection method {method} (ProjCoordTransGeoKey) is not supported, or its parameters are incomplete: the raster has no CRS"
            ));
        }
    }

    /// The CRS, from the EPSG code or the user-defined parameters.
    pub(crate) fn crs_info(&self) -> wbraster::CrsInfo {
        match (self.epsg, &self.proj4) {
            (Some(code), _) => wbraster::CrsInfo::from_epsg(code),
            (None, Some(p)) => crate::crs::SpatialRef::from_proj4(p).map(|s| s.to_crs_info()).unwrap_or_default(),
            (None, None) => self.crs.clone().unwrap_or_default(),
        }
    }
}

/// Reads a file offset (4 bytes, or 8 in a BigTIFF) at the stream's position.
fn read_offset<R: Read>(f: &mut R, big: bool, le: bool) -> Option<u64> {
    let mut b = [0u8; 8];
    if big {
        f.read_exact(&mut b).ok()?;
        Some(if le { u64::from_le_bytes(b) } else { u64::from_be_bytes(b) })
    } else {
        f.read_exact(&mut b[..4]).ok()?;
        let a = [b[0], b[1], b[2], b[3]];
        Some(if le { u32::from_le_bytes(a) } else { u32::from_be_bytes(a) } as u64)
    }
}

/// Looks for a palette `PhotometricInterpretation` in the IFD at `offset`, recording its value's
/// offset. Returns the offset of the next IFD.
fn palette_offset_in_ifd<R: Read + Seek>(f: &mut R, offset: u64, big: bool, le: bool, out: &mut Vec<u64>) -> Option<u64> {
    let u16_at = |b: &[u8]| if le { u16::from_le_bytes([b[0], b[1]]) } else { u16::from_be_bytes([b[0], b[1]]) };
    f.seek(SeekFrom::Start(offset)).ok()?;
    let count = if big {
        read_offset(f, true, le)?
    } else {
        let mut b = [0u8; 2];
        f.read_exact(&mut b).ok()?;
        u16_at(&b) as u64
    };
    let start = offset + if big { 8 } else { 2 };
    let entry_len = if big { 20 } else { 12 };
    let mut entries = vec![0u8; (count as usize).min(4096) * entry_len];
    f.read_exact(&mut entries).ok()?;
    for (i, e) in entries.chunks(entry_len).enumerate() {
        let value = if big { 12 } else { 8 };
        if u16_at(&e[0..2]) == PHOTOMETRIC && u16_at(&e[2..4]) == 3 && u16_at(&e[value..]) == 3 {
            out.push(start + (i * entry_len + value) as u64);
        }
    }
    read_offset(f, big, le)
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

/// A TIFF decoder over a local file.
pub(crate) type FileDecoder = tiff::decoder::Decoder<PatchedReader<std::io::BufReader<std::fs::File>>>;

/// Opens a decoder over a local TIFF file whose tags were read with [`read_geo_tags`].
pub(crate) fn open_decoder(path: &std::path::Path, tags: &GeoTags) -> crate::Result<FileDecoder> {
    let file = PatchedReader::new(std::io::BufReader::new(std::fs::File::open(path)?), tags);
    let dec = tiff::decoder::Decoder::new(file).map_err(|e| crate::Error::InvalidData(format!("{}: {e}", path.display())))?;
    Ok(dec.with_limits(tiff::decoder::Limits::unlimited()))
}

/// Opens another decoder over the same file, for decoding chunks in parallel.
pub(crate) type Reopen<'a> = &'a (dyn Fn() -> crate::Result<FileDecoder> + Sync);

/// A decoded chunk: its index, its samples and the size `(width, height)` of the image data it
/// holds.
pub(crate) type DecodedChunk = (u32, tiff::decoder::DecodingResult, (usize, usize));

/// Decodes one chunk, with `codec` when the `tiff` crate does not decode the image's compression.
fn read_chunk<R: Read + Seek>(
    dec: &mut tiff::decoder::Decoder<R>,
    codec: Option<&crate::tiff_codecs::ChunkCodec>,
    idx: u32,
) -> tiff::TiffResult<tiff::decoder::DecodingResult> {
    match codec {
        Some(c) => c.read(dec, idx),
        None => dec.read_chunk(idx),
    }
}

/// Decodes chunks (tiles or strips) of the image `ifd`, handing each to `place`, in no particular
/// order. With `reopen`, several threads decode, each with its own decoder over the file, while
/// the calling thread places the chunks; `dec`, positioned on the image, decodes the chunks no
/// thread did (all of them without `reopen`).
pub(crate) fn decode_chunks<R: Read + Seek>(
    dec: &mut tiff::decoder::Decoder<R>,
    reopen: Option<Reopen>,
    ifd: usize,
    chunks: &[u32],
    err: &(dyn Fn(tiff::TiffError) -> crate::Error + Sync),
    mut place: impl FnMut(DecodedChunk) -> crate::Result<()>,
) -> crate::Result<()> {
    // The compressions the `tiff` crate does not decode (alone).
    let codec = crate::tiff_codecs::ChunkCodec::new(dec).map_err(err)?;
    let codec = codec.as_ref();
    // Chunks `..first` were decoded by other threads.
    #[cfg(not(target_arch = "wasm32"))]
    let first = match reopen {
        Some(reopen) => decode_in_threads(reopen, ifd, chunks, codec, err, &mut place)?,
        None => 0,
    };
    #[cfg(target_arch = "wasm32")]
    let first = {
        let _ = (reopen, ifd);
        0
    };
    for &idx in &chunks[first..] {
        let c = read_chunk(dec, codec, idx).map_err(err)?;
        let (w, h) = dec.chunk_data_dimensions(idx);
        place((idx, c, (w as usize, h as usize)))?;
    }
    Ok(())
}

/// Decodes chunks on several threads, each with its own decoder over the file, handing them to
/// `place` on the calling thread. Returns how many of the first chunks were decoded: a thread that
/// cannot open the file leaves its share to the caller.
#[cfg(not(target_arch = "wasm32"))]
fn decode_in_threads(
    reopen: Reopen,
    ifd: usize,
    chunks: &[u32],
    codec: Option<&crate::tiff_codecs::ChunkCodec>,
    err: &(dyn Fn(tiff::TiffError) -> crate::Error + Sync),
    place: &mut impl FnMut(DecodedChunk) -> crate::Result<()>,
) -> crate::Result<usize> {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let workers = crate::par::threads().min(chunks.len());
    if workers > 1 {
        // The next chunk to decode, and whether to stop.
        let (next, stop) = (AtomicUsize::new(0), AtomicBool::new(false));
        std::thread::scope(|scope| -> crate::Result<()> {
            // A bounded queue: decoding never runs far ahead of placing.
            let (tx, rx) = std::sync::mpsc::sync_channel::<crate::Result<DecodedChunk>>(workers * 2);
            for _ in 0..workers {
                let (tx, next, stop) = (tx.clone(), &next, &stop);
                scope.spawn(move || {
                    // A thread that cannot open the file leaves the chunks to the others.
                    let Ok(mut d) = reopen() else { return };
                    if ifd != 0 && d.seek_to_image(ifd).is_err() {
                        return;
                    }
                    while !stop.load(Ordering::Relaxed) {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&idx) = chunks.get(i) else { break };
                        let r = read_chunk(&mut d, codec, idx).map_err(err).map(|c| {
                            let (w, h) = d.chunk_data_dimensions(idx);
                            (idx, c, (w as usize, h as usize))
                        });
                        let failed = r.is_err();
                        if tx.send(r).is_err() || failed {
                            break;
                        }
                    }
                });
            }
            drop(tx);
            let mut result = Ok(());
            for r in &rx {
                if let Err(e) = r.and_then(&mut *place) {
                    result = Err(e);
                    break;
                }
            }
            // Stop the workers early on an error.
            stop.store(true, Ordering::Relaxed);
            drop(rx);
            result
        })?;
        return Ok(next.into_inner().min(chunks.len()));
    }
    Ok(0)
}

/// Reads a GeoTIFF (or BigTIFF, or COG) into memory: the full-resolution image of the first IFD.
pub(crate) fn read_geotiff(path: &std::path::Path) -> crate::Result<crate::RasterDataset> {
    use crate::Error;
    use crate::cog::{ChunkGeom, Window, place};
    use tiff::decoder::DecodingResult;
    use tiff::tags::Tag;
    use wbraster::raster::RasterData;

    let tiff_err = |e: tiff::TiffError| Error::InvalidData(format!("{}: {e}", path.display()));
    let mut tags = read_geo_tags(path).unwrap_or_default();
    tags.apply_sidecar_crs(path);
    tags.warn_unreadable(&path.display().to_string());
    // The decoder rejects palette images; read their indices as grayscale instead.
    let mut dec = open_decoder(path, &tags)?;
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
    let spp = if planar { 1 } else { samples };
    let window = Window::full(w, h);
    // Each sample of a chunk goes to its band.
    let maps: Vec<Vec<(usize, usize)>> =
        if planar { (0..samples).map(|p| vec![(0, p)]).collect() } else { vec![(0..samples).map(|s| (s, s)).collect()] };
    let chunks: Vec<u32> = (0..plane_count * per_plane).map(|i| i as u32).collect();

    // Band-major output, typed after the first chunk decoded.
    let mut data: Option<RasterData> = None;
    let reopen = || open_decoder(path, &tags);
    decode_chunks(&mut dec, Some(&reopen), 0, &chunks, &tiff_err, |(idx, chunk, (dw, dh))| {
        let idx = idx as usize;
        let (plane, rest) = (idx / per_plane, idx % per_plane);
        let geom = ChunkGeom { x0: (rest % across) * cw, y0: (rest / across) * ch, dw, dh, spp };
        let map = &maps[if planar { plane } else { 0 }];
        macro_rules! place {
            ($($out:ident <- $res:ident),*) => {
                match chunk {
                    $(DecodingResult::$res(v) => {
                        let out = data.get_or_insert_with(|| RasterData::$out(vec![Default::default(); band_len * samples]));
                        let RasterData::$out(out) = out else {
                            return Err(Error::InvalidData("TIFF chunks differ in sample type".into()));
                        };
                        place(out, &v, &geom, window, map);
                    })*
                    DecodingResult::F16(v) => {
                        let out = data.get_or_insert_with(|| RasterData::F32(vec![0.0; band_len * samples]));
                        let RasterData::F32(out) = out else {
                            return Err(Error::InvalidData("TIFF chunks differ in sample type".into()));
                        };
                        let v: Vec<f32> = v.iter().map(|x| x.to_f32()).collect();
                        place(out, &v, &geom, window, map);
                    }
                }
            };
        }
        place!(U8 <- U8, U16 <- U16, U32 <- U32, U64 <- U64, I8 <- I8, I16 <- I16, I32 <- I32, I64 <- I64, F32 <- F32, F64 <- F64);
        Ok(())
    })?;
    let data = data.ok_or_else(|| Error::InvalidData(format!("{}: no image data", path.display())))?;
    let cfg = wbraster::RasterConfig {
        cols: w,
        rows: h,
        bands: samples,
        nodata: tags.nodata.unwrap_or(-9999.0),
        data_type: data.data_type(),
        crs: tags.crs_info(),
        metadata: tags.metadata.clone(),
        ..Default::default()
    };
    let raster = crate::pixel::raster_from_data(cfg, data)?;
    let has_nodata = tags.nodata.is_some();
    let mut ds = crate::RasterDataset::with_driver(raster, crate::formats::RasterDriver::GTiff, has_nodata);
    match tags.corner_geo_transform() {
        Some(gt) => ds.set_geo_transform(gt),
        None => {
            let r = &mut ds.raster;
            r.cell_size_x = 1.0;
            r.cell_size_y = 1.0;
            r.x_min = 0.0;
            r.y_min = 0.0;
        }
    }
    Ok(ds)
}

/// A reader that replaces two bytes at some file offsets.
pub(crate) struct PatchedReader<R> {
    inner: R,
    pos: u64,
    patches: Vec<(u64, [u8; 2])>,
}

impl<R> PatchedReader<R> {
    /// Wraps a reader positioned at the start of a TIFF, so that its palette images read as
    /// grayscale (the decoder rejects palette images).
    pub(crate) fn new(inner: R, tags: &GeoTags) -> Self {
        let gray = if tags.little_endian { [1u8, 0] } else { [0u8, 1] };
        Self { inner, pos: 0, patches: tags.palette_photometric_offsets.iter().map(|off| (*off, gray)).collect() }
    }

    /// The wrapped reader.
    pub(crate) fn get_ref(&self) -> &R {
        &self.inner
    }
}

impl<R: Read> Read for PatchedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        for (off, bytes) in &self.patches {
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

    /// The GeoKeys GDAL 3.12 writes for a WGS 84 CRS with no EPSG code and the projection method
    /// `method`, with `params` (key, value) as GeoDoubleParams.
    fn gdal_keys(method: u16, params: &[(u16, f64)]) -> (Vec<u16>, Vec<f64>) {
        let mut keys = vec![1, 1, 0, 0, 1024, 0, 1, 1, 1025, 0, 1, 1, 2048, 0, 1, 32767, 2050, 0, 1, 6326, 2054, 0, 1, 9102];
        keys.extend([2057, GEO_DOUBLE_PARAMS, 1, 0, 2059, GEO_DOUBLE_PARAMS, 1, 1]);
        keys.extend([3072, 0, 1, 32767, 3074, 0, 1, 32767, 3075, 0, 1, method, 3076, 0, 1, 9001]);
        let mut doubles = vec![6378137.0, 298.257223563];
        for (id, v) in params {
            keys.extend([*id, GEO_DOUBLE_PARAMS, 1, doubles.len() as u16]);
            doubles.push(*v);
        }
        keys[3] = (keys.len() / 4 - 1) as u16;
        (keys, doubles)
    }

    #[test]
    fn reads_standard_parallels_of_mercator_and_polar_stereographic() {
        // (method, parameters as GDAL writes them, point, PROJ's coordinates for it)
        type Case = (u16, &'static [(u16, f64)], (f64, f64), (f64, f64));
        let cases: [Case; 5] = [
            // +proj=merc +lat_ts=30
            (7, &[(3078, 30.0), (3080, 0.0), (3081, 0.0), (3082, 0.0), (3083, 0.0)], (5.0, 60.0), (482431.4012544825, 7248377.351122094)),
            // +proj=merc +k=0.997 +lon_0=10
            (
                7,
                &[(3080, 10.0), (3081, 0.0), (3082, 0.0), (3083, 0.0), (3092, 0.997)],
                (5.0, 60.0),
                (-554927.6616044687, 8337610.452855245),
            ),
            // +proj=stere +lat_0=90 +lat_ts=70 +lon_0=-45 (variant B)
            (
                15,
                &[(3081, 70.0), (3082, 0.0), (3083, 0.0), (3092, 1.0), (3095, -45.0)],
                (5.0, 75.0),
                (1251624.3097635235, -1050237.4966950903),
            ),
            // +proj=stere +lat_0=-90 +lat_ts=-71 +lon_0=0 (variant B, south)
            (
                15,
                &[(3081, -71.0), (3082, 0.0), (3083, 0.0), (3092, 1.0), (3095, 0.0)],
                (5.0, -75.0),
                (142829.37034579527, 1632547.17342291),
            ),
            // +proj=stere +lat_0=90 +k=0.994 +lon_0=0 (variant A)
            (
                15,
                &[(3081, 90.0), (3082, 0.0), (3083, 0.0), (3092, 0.994), (3095, 0.0)],
                (5.0, 75.0),
                (145946.66590136578, -1668178.0246662465),
            ),
        ];
        let wgs84 = crate::crs::SpatialRef::parse("EPSG:4326").unwrap();
        for (method, params, (lon, lat), (x, y)) in cases {
            let (keys, doubles) = gdal_keys(method, params);
            let info = crs_from_geokeys(&keys, Some(&doubles), None);
            assert_eq!(info.epsg, None, "{info:?}");
            let crs = crate::crs::SpatialRef::from_raster_crs(&info);
            // The CRS, and its WKT (as terrio reports it) read back.
            let wkt = crate::crs::SpatialRef::parse(&crs.to_wkt().unwrap()).unwrap();
            for c in [&crs, &wkt] {
                let (tx, ty) = crate::crs::Transformer::new(&wgs84, c).unwrap().transform(lon, lat).unwrap();
                assert!((tx - x).abs() < 1e-3 && (ty - y).abs() < 1e-3, "{c:?}: ({tx}, {ty}) vs ({x}, {y})");
            }
        }
    }

    #[test]
    fn warns_of_a_projection_method_it_cannot_read() {
        // A WGS 84 CRS projected with method 4 (oblique Mercator, Laborde), which terrio does
        // not map.
        let keys = [1, 1, 0, 4, 1024, 0, 1, 1, 2048, 0, 1, 4326, 3072, 0, 1, 32767, 3075, 0, 1, 4];
        let _ = crate::warnings::take();
        assert!(crs_from_geokeys(&keys, None, None).wkt.is_none());
        let w = crate::warnings::take();
        assert!(w.len() == 1 && w[0].contains("projection method 4"), "{w:?}");
    }
}

// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/gtiff/gtiffdataset_read.cpp, gt_wkt_srs.cpp
// No code is translated from these sources.

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
const RPC_COEFFICIENTS: u16 = 50844;

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
    /// Descriptive properties, including TIFF photometric colors and GDAL role overrides.
    pub band_properties: Vec<crate::BandMetadata>,
    /// TIFF palette converted from UInt16 channels to Byte RGBA.
    pub color_table: Vec<[u8; 4]>,
    /// Whether the raster type is `RasterPixelIsPoint`.
    pub pixel_is_point: bool,
    /// The `GDAL_NODATA` value.
    pub nodata: Option<f64>,
    /// One-based nodata overrides from PAM.
    pub band_nodata: Vec<(usize, f64)>,
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
    /// The 92 values of the `RPCCoefficientTag`.
    pub rpc: Option<Vec<f64>>,
    /// Why libtiff cannot read the first image's directory, so that GDAL cannot open the file:
    /// a structural tag whose type or count it cannot read (see [`structural_tag_problem`]).
    pub unreadable_structure: Option<String>,
    /// The baseline TIFF tags GDAL lists as `TIFFTAG_*` dataset metadata (`TIFFTAG_SOFTWARE`,
    /// `TIFFTAG_XRESOLUTION`...), formatted as GDAL formats them.
    pub tiff_tag_items: Vec<(String, String)>,
    /// The `OVERVIEW_RESAMPLING` item of the `IMAGE_STRUCTURE` domain (COGs), which GDAL reports
    /// as the `RESAMPLING` metadata of overview bands.
    pub overview_resampling: Option<String>,
}

/// The structural tags libtiff's `TIFFReadDirectory` reads first, and fails the directory on
/// when it cannot (`TIFFFetchNormalTag(tif, dp, 0)`, without recovery; GDAL 3.12.1's libtiff
/// 4.7): their names, and whether they hold one value (`TIFF_SETGET_UINT16` and `_UINT32`;
/// ExtraSamples is an array).
const STRUCTURAL_TAGS: [(u16, &str, bool); 10] = [
    (256, "ImageWidth", true),
    (257, "ImageLength", true),
    (32997, "ImageDepth", true),
    (322, "TileWidth", true),
    (323, "TileLength", true),
    (32998, "TileDepth", true),
    (284, "PlanarConfiguration", true),
    (278, "RowsPerStrip", true),
    (277, "SamplesPerPixel", true),
    (338, "ExtraSamples", false),
];

/// Why libtiff refuses a structural tag of type `typ` holding `count` values, as
/// `TIFFReadDirEntryShort`, `TIFFReadDirEntryLong` and `TIFFReadDirEntryShortArray` do: only
/// integer types (BYTE, SHORT, LONG, LONG8 and their signed forms) are read, and a single-valued
/// tag must hold exactly one value. `None` for other tags, and for readable ones.
fn structural_tag_problem(tag: u16, typ: u16, count: u64) -> Option<String> {
    let &(_, name, single) = STRUCTURAL_TAGS.iter().find(|(t, ..)| *t == tag)?;
    if !matches!(typ, 1 | 3 | 4 | 6 | 8 | 9 | 16 | 17) {
        return Some(format!("incompatible type {typ} for \"{name}\""));
    }
    if single && count != 1 {
        return Some(format!("incorrect count {count} for \"{name}\""));
    }
    None
}

impl GeoTags {
    /// An error when libtiff cannot read the first image's directory (see
    /// [`GeoTags::unreadable_structure`]): such a file is refused at open, as GDAL refuses it.
    pub(crate) fn check_structure(&self) -> crate::Result<()> {
        match &self.unreadable_structure {
            Some(problem) => Err(crate::Error::InvalidData(format!("TIFF directory: {problem}"))),
            None => Ok(()),
        }
    }
}

/// The compressions libtiff has a codec for (`_TIFFBuiltinCODECS`, GDAL 3.12.1's libtiff 4.7),
/// and the JPEG XL ones GDAL registers with it: GDAL opens no image of another, whatever its build.
const LIBTIFF_CODECS: [u16; 23] =
    [1, 2, 3, 4, 5, 6, 7, 8, 32766, 32771, 32773, 32809, 32909, 32946, 34661, 34676, 34677, 34887, 34925, 50000, 50001, 50002, 52546];

/// GDAL's checks of an image's tags before it reads a band (`GTiffDataset::OpenOffset`), made
/// before terrio does any work per band: a compression no codec decodes, and tiles of more than
/// 2^31 - 1 pixels a side, refused with GDAL's errors. A file declaring thousands of samples per
/// pixel besides is refused in no time, as GDAL refuses it.
pub(crate) fn check_open<R: Read + Seek>(
    dec: &mut tiff::decoder::Decoder<R>,
    tiff_err: impl Fn(tiff::TiffError) -> crate::Error,
) -> crate::Result<()> {
    let compression = dec.find_tag_unsigned::<u16>(tiff::tags::Tag::Compression).map_err(tiff_err)?.unwrap_or(1);
    if !LIBTIFF_CODECS.contains(&compression) {
        // `GTIFFGetCompressionMethodName`: the codes GDAL names without a codec for them.
        let codec = match compression {
            32908 => "PIXARFILM".to_string(),
            32947 => "DCS".to_string(),
            34712 => "JP2000".to_string(),
            code => format!("of code {code}"),
        };
        return Err(crate::Error::InvalidData(format!("Cannot open TIFF file due to missing codec {codec}.")));
    }
    if dec.get_chunk_type() == tiff::decoder::ChunkType::Tile {
        let (w, h) = dec.chunk_dimensions();
        if w > i32::MAX as u32 || h > i32::MAX as u32 {
            return Err(crate::Error::InvalidData(format!("Too large block size: {w} x {h}")));
        }
    }
    Ok(())
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

/// The ASCII baseline tags GDAL reports as `TIFFTAG_*` metadata, in its order.
const ASCII_TAGS: [(u16, &str); 7] = [
    (269, "TIFFTAG_DOCUMENTNAME"),
    (270, "TIFFTAG_IMAGEDESCRIPTION"),
    (305, "TIFFTAG_SOFTWARE"),
    (306, "TIFFTAG_DATETIME"),
    (315, "TIFFTAG_ARTIST"),
    (316, "TIFFTAG_HOSTCOMPUTER"),
    (33432, "TIFFTAG_COPYRIGHT"),
];
const X_RESOLUTION: u16 = 282;
const Y_RESOLUTION: u16 = 283;
const RESOLUTION_UNIT: u16 = 296;
const MIN_SAMPLE_VALUE: u16 = 280;
const MAX_SAMPLE_VALUE: u16 = 281;

/// The most images (IFDs) looked at for palette images.
const MAX_IFDS: usize = 1024;

/// Reads the georeferencing tags of a TIFF file's first image. Returns `None` when the file is not
/// a TIFF.
pub(crate) fn read_geo_tags(path: &std::path::Path) -> Option<GeoTags> {
    read_geo_tags_from(std::io::BufReader::new(std::fs::File::open(path).ok()?))
}

/// Refuses a file whose first bytes show a TIFF when the process-wide raster allow-list leaves out
/// the GTiff driver, before a reader of its tags opens it (`read_gcps`, `ops::rpc::read_rpc`,
/// `open_raster_with_tiff_limits`, `ops::overview::delete_overviews`), as a de-registered GDAL
/// driver opens no file. `shown` names it in the refusal. Other files are left to the reader,
/// which reads no TIFF from them.
pub(crate) fn check_tiff_allowed(path: &std::path::Path, shown: &str) -> crate::Result<()> {
    let mut head = [0u8; 4];
    let n = std::fs::File::open(path).and_then(|mut f| f.read(&mut head)).unwrap_or(0);
    if crate::formats::sniff(&head[..n]) == Some("GTIFF") {
        crate::formats::check_driver_allowed("GTiff", shown, crate::formats::Kind::Raster)?;
    }
    Ok(())
}

/// Reads the georeferencing tags of a TIFF stream's first image, and finds the palette images
/// among all its images. Returns `None` when the stream is not a TIFF.
pub(crate) fn read_geo_tags_from<R: Read + Seek>(f: R) -> Option<GeoTags> {
    read_geo_tags_impl(f, false)
}

/// Comparison-only TIFF metadata; ordinary lazy opens avoid its out-of-line reads.
pub(crate) fn read_compare_geo_tags(path: &std::path::Path) -> Option<GeoTags> {
    read_geo_tags_impl(std::io::BufReader::new(std::fs::File::open(path).ok()?), true)
}

fn read_geo_tags_impl<R: Read + Seek>(mut f: R, compare_metadata: bool) -> Option<GeoTags> {
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
    let mut unreadable_structure = None;
    let mut structural_seen: Vec<u16> = Vec::new();
    for e in entries.chunks(entry_len) {
        let tag = u16_at(&e[0..2]);
        // The first entry of each structural tag (libtiff ignores duplicates).
        if STRUCTURAL_TAGS.iter().any(|(t, ..)| *t == tag) && !structural_seen.contains(&tag) {
            structural_seen.push(tag);
            let count = if big { u64_at(&e[4..12]) } else { u32_at(&e[4..8]) as u64 };
            if unreadable_structure.is_none() {
                unreadable_structure = structural_tag_problem(tag, u16_at(&e[2..4]), count);
            }
        }
        if tag == PHOTOMETRIC && u16_at(&e[2..4]) == 3 {
            let value = &e[if big { 12 } else { 8 }..];
            if u16_at(value) == 3 {
                let entry_start = f.stream_position().ok()? - entries.len() as u64;
                let index = (e.as_ptr() as usize - entries.as_ptr() as usize) as u64;
                palette_offset = Some(entry_start + index + if big { 12 } else { 8 });
            }
        }
        if !matches!(
            tag,
            PHOTOMETRIC
                | 258
                | 259
                | 277
                | 284
                | 320
                | 332
                | 338
                | 339
                | MODEL_PIXEL_SCALE
                | MODEL_TIEPOINT
                | MODEL_TRANSFORMATION
                | GEO_KEY_DIRECTORY
                | GEO_DOUBLE_PARAMS
                | GEO_ASCII_PARAMS
                | GDAL_METADATA
                | GDAL_NODATA
                | RPC_COEFFICIENTS
                | 269
                | 270
                | 305
                | 306
                | 315
                | 316
                | 33432
                | X_RESOLUTION
                | Y_RESOLUTION
                | RESOLUTION_UNIT
                | MIN_SAMPLE_VALUE
                | MAX_SAMPLE_VALUE
        ) {
            continue;
        }
        if !compare_metadata
            && (ASCII_TAGS.iter().any(|(extra, _)| *extra == tag)
                || matches!(tag, X_RESOLUTION | Y_RESOLUTION | RESOLUTION_UNIT | MIN_SAMPLE_VALUE | MAX_SAMPLE_VALUE))
        {
            continue;
        }
        let typ = u16_at(&e[2..4]);
        let (n, inline, value_bytes) =
            if big { (u64_at(&e[4..12]), &e[12..20], 8usize) } else { (u32_at(&e[4..8]) as u64, &e[8..12], 4usize) };
        let size = match typ {
            2 => 1,  // ASCII
            3 => 2,  // SHORT
            5 => 8,  // RATIONAL
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
            // A zero denominator gives a non-finite value, which is not reported.
            5 => Values::Doubles(data.as_chunks::<8>().0.iter().map(|c| f64::from(u32_at(&c[..4])) / f64::from(u32_at(&c[4..]))).collect()),
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

    let mut out = GeoTags { palette_photometric_offsets: palette_offsets, little_endian: le, unreadable_structure, ..Default::default() };
    let shorts = |tag: u16| {
        tags.iter().find_map(|(k, v)| match v {
            Values::Shorts(s) if *k == tag => Some(s.as_slice()),
            _ => None,
        })
    };
    let photometric = shorts(PHOTOMETRIC).and_then(|s| s.first()).copied().unwrap_or(1);
    if photometric == 3
        && let Some(map) = shorts(320)
    {
        let n = map.len() / 3;
        out.color_table = (0..n).map(|i| [(map[i] / 257) as u8, (map[n + i] / 257) as u8, (map[2 * n + i] / 257) as u8, 255]).collect();
    }
    let samples = shorts(277).and_then(|s| s.first()).copied().unwrap_or(1) as usize;
    let jpeg = shorts(259).and_then(|s| s.first()).copied() == Some(7);
    let base = match photometric {
        2 | 6 | 8 | 9 | 10 => 3,
        5 => 4,
        _ => 1,
    };
    // 8-bit CMYK that GDAL reads through libtiff's RGBA interface is read as RGBA here too
    // (`cog::reads_cmyk_as_rgba`, the same rule on the same tags): its bands are red, green, blue
    // and alpha.
    let first = |tag: u16, default: u16| shorts(tag).and_then(|s| s.first()).copied().unwrap_or(default);
    let cmyk_as_rgba = photometric == 5
        && first(258, 1) == 8
        && first(339, 1) == 1
        && first(332, 1) == 1
        && samples >= 4
        && (first(284, 1) != 2 || samples == 4);
    out.band_properties = (0..if cmyk_as_rgba { 4 } else { samples })
        .map(|i| {
            let color = match (photometric, i) {
                (5, 0) if cmyk_as_rgba => "Red",
                (5, 1) if cmyk_as_rgba => "Green",
                (5, 2) if cmyk_as_rgba => "Blue",
                (5, 3) if cmyk_as_rgba => "Alpha",
                (0 | 1, 0) => "Gray",
                (2, 0) => "Red",
                (2, 1) => "Green",
                (2, 2) => "Blue",
                (6, 0) if jpeg => "Red",
                (6, 1) if jpeg => "Green",
                (6, 2) if jpeg => "Blue",
                (6, 0) => "YCbCr_Y",
                (6, 1) => "YCbCr_Cb",
                (6, 2) => "YCbCr_Cr",
                (5, 0) => "Cyan",
                (5, 1) => "Magenta",
                (5, 2) => "Yellow",
                (5, 3) => "Black",
                (3, 0) => "Palette",
                _ if i >= base && shorts(338).and_then(|v| v.get(i - base)).is_some_and(|v| matches!(v, 1 | 2)) => "Alpha",
                _ => "Undefined",
            };
            crate::BandMetadata { color_interp: Some(color.into()), ..Default::default() }
        })
        .collect();
    if let Some(xml) = ascii(GDAL_METADATA) {
        for item in crate::drivers::gtiff_write::gdal_metadata_items(xml) {
            let Some(i) = item.attr("sample").and_then(|s| s.parse::<usize>().ok()) else { continue };
            let Some(band) = out.band_properties.get_mut(i) else { continue };
            if item.attr("domain").is_some() {
                continue;
            }
            // The band properties GDAL writes as items with a role (`GDALMetadata` of the GTiff driver).
            match item.attr("role") {
                Some("colorinterp") => band.color_interp = Some(color_interpretation(&item.text).into()),
                Some("scale") => band.scale = item.text.trim().parse().ok(),
                Some("offset") => band.offset = item.text.trim().parse().ok(),
                Some("unittype") => band.unit = Some(crate::drivers::gtiff_write::xml_unescape(&item.text)),
                Some("description") => band.description = Some(crate::drivers::gtiff_write::xml_unescape(&item.text)),
                _ => {}
            }
        }
    }
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
    if compare_metadata {
        for (tag, name) in ASCII_TAGS {
            if let Some(v) = ascii(tag) {
                out.tiff_tag_items.push((name.to_string(), v.to_string()));
            }
        }
        let first = |t: u16| doubles(t).and_then(|d| d.first()).copied();
        if let (Some(x), Some(y)) = (first(X_RESOLUTION), first(Y_RESOLUTION))
            && x.is_finite()
            && y.is_finite()
        {
            // libtiff holds resolutions as floats.
            let g8 = |v: f64| crate::drivers::common::fmt_g(f64::from(v as f32), 8);
            out.tiff_tag_items.push(("TIFFTAG_XRESOLUTION".into(), g8(x)));
            out.tiff_tag_items.push(("TIFFTAG_YRESOLUTION".into(), g8(y)));
            let unit = shorts(RESOLUTION_UNIT).and_then(|s| s.first()).copied().unwrap_or(2);
            let name = match unit {
                1 => "unitless",
                2 => "pixels/inch",
                3 => "pixels/cm",
                _ => "unknown",
            };
            out.tiff_tag_items.push(("TIFFTAG_RESOLUTIONUNIT".into(), format!("{unit} ({name})")));
        }
        for (tag, name) in [(MIN_SAMPLE_VALUE, "TIFFTAG_MINSAMPLEVALUE"), (MAX_SAMPLE_VALUE, "TIFFTAG_MAXSAMPLEVALUE")] {
            if let Some(v) = shorts(tag).and_then(|s| s.first()) {
                out.tiff_tag_items.push((name.to_string(), v.to_string()));
            }
        }
        out.overview_resampling = ascii(GDAL_METADATA).and_then(|xml| {
            crate::drivers::gtiff_write::gdal_metadata_items(xml)
                .into_iter()
                .find(|i| i.attr("domain") == Some("IMAGE_STRUCTURE") && i.attr("name") == Some("OVERVIEW_RESAMPLING"))
                .map(|i| i.text.trim().to_string())
        });
    }
    out.rpc = doubles(RPC_COEFFICIENTS).filter(|v| v.len() == 92).map(<[f64]>::to_vec);
    let first = |tag: u16| shorts(tag).and_then(|s| s.first()).copied();
    // The nodata of 64-bit integer samples is the integer `strtoll` or `strtoull` reads at the
    // start of the text, as GDAL 3.12.1 reads it (`GTiffDataset::OpenOffset`, for 33 to 64 bits
    // of SampleFormat 2 or 1, libtiff's default): `9.2233720368547758e+18`, which GDAL writes for
    // 2^63 given as a double, is 9. Other samples take the text as a number.
    out.nodata = ascii(GDAL_NODATA).filter(|s| !s.is_empty()).and_then(|s| match (first(258).unwrap_or(1), first(339).unwrap_or(1)) {
        (33..=64, 2) => Some(c_strtoll(s) as f64),
        (33..=64, 1) => Some(c_strtoull(s) as f64),
        _ => parse_nodata(s.trim()),
    });
    // GDAL rounds the nodata value of Float32 bands to Float32, so that it equals the samples
    // written with it (-9999.1 is -9999.099609375).
    if first(258) == Some(32) && first(339) == Some(3) {
        out.nodata = out.nodata.map(|v| v as f32 as f64);
    }
    out.metadata = ascii(GDAL_METADATA).map(crate::drivers::gtiff_write::parse_gdal_metadata).unwrap_or_default();
    out.band_metadata = ascii(GDAL_METADATA).map(crate::drivers::gtiff_write::parse_gdal_band_metadata).unwrap_or_default();

    if let Some(keys) = keys {
        apply_geokeys(&mut out, keys, doubles(GEO_DOUBLE_PARAMS), ascii(GEO_ASCII_PARAMS));
    }
    // Raster type is a GeoKey, exposed as dataset metadata by GDAL.
    out.metadata.retain(|(k, _)| k != "AREA_OR_POINT");
    if keys.and_then(|k| k.get(4..)).is_some_and(|entries| entries.as_chunks::<4>().0.iter().any(|k| k[0] == 1025)) {
        out.metadata.push(("AREA_OR_POINT".into(), if out.pixel_is_point { "Point" } else { "Area" }.into()));
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

/// Canonical GDAL band roles; unknown metadata roles map to Undefined as GDAL does.
fn color_interpretation(value: &str) -> &'static str {
    const NAMES: &[&str] = &[
        "Undefined",
        "Gray",
        "Palette",
        "Red",
        "Green",
        "Blue",
        "Alpha",
        "Hue",
        "Saturation",
        "Lightness",
        "Cyan",
        "Magenta",
        "Yellow",
        "Black",
        "YCbCr_Y",
        "YCbCr_Cb",
        "YCbCr_Cr",
        "Pan",
        "Coastal",
        "RedEdge",
        "NIR",
        "SWIR",
        "MWIR",
        "LWIR",
        "TIR",
        "OtherIR",
        "IR_Reserved_1",
        "IR_Reserved_2",
        "IR_Reserved_3",
        "IR_Reserved_4",
        "SAR_Ka",
        "SAR_K",
        "SAR_Ku",
        "SAR_X",
        "SAR_C",
        "SAR_S",
        "SAR_L",
        "SAR_P",
        "SAR_Reserved_1",
        "SAR_Reserved_2",
    ];
    NAMES.iter().copied().find(|name| name.eq_ignore_ascii_case(value.trim())).unwrap_or("Undefined")
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
                // Offsets count bytes, and the text may hold multi-byte characters (invalid
                // bytes are decoded to U+FFFD): slice the bytes.
                let b = s.as_bytes();
                let start = (off as usize).min(b.len());
                let end = (start + n as usize).min(b.len());
                String::from_utf8_lossy(&b[start..end]).trim_end_matches('|').to_string()
            })
        })
    };
    out.pixel_is_point = short_key(1025) == Some(2);
    let k = crate::drivers::geokeys::GeoKeys { keys, doubles: doubles.unwrap_or_default(), ascii: ascii.unwrap_or_default() };
    let projected = short_key(3072);
    let geographic = short_key(2048);
    out.epsg = match (short_key(1024), projected, geographic) {
        (_, Some(code), _) if code != 0 && code != 32767 => Some(code as u32),
        (Some(2), _, Some(code)) if code != 0 && code != 32767 => Some(code as u32),
        (Some(1), _, _) => {
            // A registered CRS only in the GeoKeys' linear unit: GDAL builds the CRS from the
            // keys (a ProjectionGeoKey's conversion in metres, in ProjLinearUnitsGeoKey's unit),
            // and gives it a registered CRS's code only when the two are equivalent
            // (gt_wkt_srs.cpp `GTIFGetOGISDefnAsOSR`): UTM zone 33N in US survey feet is no
            // EPSG:32633.
            let size = crate::drivers::geokeys::linear_unit_size(&k);
            // A user-defined datum whose citation names WGS 84 is WGS 84's, as GDAL identifies it.
            let cites_wgs84 = ascii_key(2049).is_some_and(|c| {
                c.split('|')
                    .any(|p| p.strip_prefix("Datum = ").is_some_and(|d| matches!(d.trim(), "World Geodetic System 1984" | "WGS_1984")))
            });
            let datum = if cites_wgs84 && short_key(2050) == Some(32767) { Some(6326) } else { short_key(2050) };
            user_defined_projected(short_key(3074), geographic, datum, ascii_key(1026).or_else(|| ascii_key(3073)))
                .filter(|code| crate::crs::SpatialRef::from_epsg(*code).is_ok_and(|s| (s.metres_per_unit() / size - 1.0).abs() < 1e-9))
        }
        _ => None,
    };
    if out.epsg.is_none() {
        // Geographic and projected user-defined CRSs, and ESRI PE citations.
        if let Some(crs) = crate::drivers::geokeys::projected_on_epsg_geographic(&k) {
            out.crs = Some(crs);
            return;
        }
        if short_key(1024) == Some(1) && crate::drivers::geokeys::esri_pe_crs(&k).is_none() {
            out.proj4 = crate::drivers::geokeys::user_defined_proj4(&k);
            out.unreadable_method = crate::drivers::geokeys::unmapped_method(&k);
            // Where GDAL reads it otherwise than the WKT of its PROJ string: an ESRI CRS named by
            // the citation, a linear unit PROJ strings do not name, an inverse flattening
            // computed from the axes.
            if let Some(crs) = out.proj4.as_deref().and_then(|p| crate::drivers::geokeys::user_defined_projected_crs(&k, p)) {
                out.epsg = None;
                out.crs = Some(crs);
                out.proj4 = None;
                return;
            }
        }
        if out.proj4.is_none() {
            out.crs = crate::drivers::geokeys::user_defined_crs(&k);
            out.epsg = out.crs.as_ref().and_then(|c| c.epsg);
        }
    }
    // A vertical CRS (VerticalCSTypeGeoKey) on an EPSG horizontal CRS: the compound CRS, as GDAL
    // reads it (EPSG:4326 with EGM96 heights is `EPSG:4326+5773`). GeoTIFF stores no code for the
    // compound: GDAL 3.12.1 takes the registry's compound CRS of the two codes when there is one
    // (`GTIFGetOGISDefnAsOSR`), EPSG:9707 for those.
    if let Some(h) = out.epsg
        && let Some(v) = short_key(4096).filter(|c| *c != 0 && *c != 32767)
        && let Ok(c) = crate::crs::SpatialRef::compound(h, u32::from(v))
    {
        out.epsg = None;
        let registered =
            crate::crs::compound_code(h, u32::from(v), c.name().as_deref()).and_then(|code| crate::crs::SpatialRef::from_epsg(code).ok());
        out.crs = Some(registered.unwrap_or(c).to_crs_info());
    }
}

impl GeoTags {
    /// Takes the CRS of the `.aux.xml` sidecar of the file at `path`, when it holds one: it
    /// overrides the GeoKeys, as GDAL reads it (GDAL writes there the CRSs GeoKeys cannot hold).
    /// Returns whether the sidecar was read.
    pub(crate) fn apply_sidecar_crs(&mut self, path: &std::path::Path) -> bool {
        let Some(pam) = crate::drivers::common::read_pam(path) else {
            return false;
        };
        self.nodata = self.nodata.or(pam.dataset_nodata);
        self.band_nodata = pam.band_nodata;
        if let Some(crs) = pam.crs {
            self.epsg = crs.epsg;
            self.proj4 = None;
            self.crs = Some(crs);
            self.unreadable_method = None;
        }
        true
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

/// The sign and magnitude of the base-10 integer C's `strtoll` and `strtoull` read at the start
/// of `s`: after white space, an optional sign, then digits (none read as 0). A magnitude past
/// `u64::MAX` is returned as `u64::MAX + 1`, which both functions take as an overflow.
fn c_leading_integer(s: &str) -> (bool, u128) {
    const OVERFLOW: u128 = u64::MAX as u128 + 1;
    let s = s.trim_start_matches([' ', '\t', '\n', '\u{b}', '\u{c}', '\r']);
    let (negative, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let magnitude = digits.bytes().take_while(u8::is_ascii_digit).fold(0u128, |m, d| (m * 10 + u128::from(d - b'0')).min(OVERFLOW));
    (negative, magnitude)
}

/// `strtoll(s, NULL, 10)`: `i64::MIN` or `i64::MAX` on overflow.
fn c_strtoll(s: &str) -> i64 {
    // The magnitude is at most 2^64: its negation fits an i128.
    let (negative, m) = c_leading_integer(s);
    let value = if negative { -(m as i128) } else { m as i128 };
    i64::try_from(value).unwrap_or(if negative { i64::MIN } else { i64::MAX })
}

/// `strtoull(s, NULL, 10)`: a negative value wraps (`-1` is `u64::MAX`), and an overflow is
/// `u64::MAX`.
fn c_strtoull(s: &str) -> u64 {
    let (negative, m) = c_leading_integer(s);
    match u64::try_from(m) {
        Ok(m) if negative => m.wrapping_neg(),
        Ok(m) => m,
        Err(_) => u64::MAX,
    }
}

/// The EPSG code of a user-defined projected CRS, from its projection code (UTM zones) or its
/// citation (such as `WGS 84 / UTM zone 10N`). The datum is the geographic CRS's, else the
/// GeogGeodeticDatumGeoKey's `datum`: a UTM zone on another datum than WGS 84, NAD83 or NAD27
/// (NAD83(HARN), ETRS89...) is no WGS 84 UTM zone, and is left to the citation's name.
fn user_defined_projected(projection: Option<u16>, geographic: Option<u16>, datum: Option<u16>, citation: Option<String>) -> Option<u32> {
    let citation = citation.unwrap_or_default();
    let defined = |c: &u16| *c != 0 && *c != 32767;
    // A datum of the GeoKeys' own (a user-defined GeogGeodeticDatumGeoKey on a user-defined
    // geographic CRS, as GDAL writes a datum it has no code for) is none of the registry's: GDAL
    // names it after its ellipsoid (UTM zone 33N on GRS 1980 is not WGS 84's).
    let own_datum = !geographic.is_some_and(|c| defined(&c)) && datum == Some(32767);
    // A geographic CRS terrio does not know leaves the datum to the datum key, else the citation.
    let datum = geographic.filter(defined).and_then(|c| crate::crs::geographic_datum(c as u32)).or(datum.filter(defined).map(u32::from));
    // Datum: WGS 84 unless the datum is NAD83 or NAD27, or the citation says so when the GeoKeys
    // name no datum.
    let (nad83, nad27, other) = match datum {
        Some(6269) => (true, false, false),
        Some(6267) => (false, true, false),
        Some(6326) => (false, false, false),
        Some(_) => (false, false, true),
        None => (citation.contains("NAD83"), citation.contains("NAD27"), own_datum),
    };
    let utm = match projection {
        Some(p @ 16001..=16060) => Some(((p - 16000) as u32, true)),
        Some(p @ 16101..=16160) => Some(((p - 16100) as u32, false)),
        _ => parse_utm_citation(&citation),
    };
    if let Some((zone, north)) = utm.filter(|_| !other) {
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
pub(crate) fn open_decoder(path: &std::path::Path, tags: &GeoTags, limits: crate::cog::TiffReadLimits) -> crate::Result<FileDecoder> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let file = PatchedReader::new(std::io::BufReader::new(file), tags);
    let dec = tiff::decoder::Decoder::new(file).map_err(|e| crate::Error::InvalidData(format!("{}: {e}", path.display())))?;
    Ok(dec.with_limits(limits.tiff_within(len)))
}

/// The `tiff` crate's bounds for a file of `len` bytes, which [`crate::TiffReadLimits::tiff_within`]
/// applies with the caller's limits: the tag values the crate allocates are read from the file, so
/// bounded by its size (tag values decode to 24 bytes each). Decoded chunks are bounded by
/// [`crate::TiffReadLimits::chunk_bytes`], checked before their buffers are allocated (see
/// [`read_chunk`]). A chunk's stored byte count is not bounded by the file: an uncompressed strip
/// is read as far as the file goes, as GDAL reads it (a 4 x 4 strip declaring 4 GiB reads).
pub(crate) fn file_limits(len: u64) -> tiff::decoder::Limits {
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    let mut limits = tiff::decoder::Limits::unlimited();
    limits.decoding_buffer_size = len.saturating_mul(32);
    limits.ifd_value_size = len;
    limits
}

/// Refuses the chunks `(offset, byte count)` whose stored bytes run past the end of a file of `len`
/// bytes when they are compressed: GDAL (libtiff) reads a compressed chunk's stored bytes whole,
/// and fails there, where the `tiff` crate reads a stream as far as it goes. An uncompressed chunk
/// is read as far as its pixels go, as GDAL reads it. Chunks without bytes (sparse) are not read.
pub(crate) fn check_stored_chunks(compression: u16, chunks: impl IntoIterator<Item = (u64, u64)>, len: u64) -> crate::Result<()> {
    if compression == 1 {
        return Ok(());
    }
    for (offset, count) in chunks.into_iter().filter(|(_, n)| *n > 0) {
        if offset.checked_add(count).is_none_or(|end| end > len) {
            return Err(crate::Error::InvalidData(format!(
                "TIFF: a compressed chunk of {count} bytes at offset {offset} runs past the end of the file ({len} bytes)"
            )));
        }
    }
    Ok(())
}

/// Opens another decoder over the same TIFF (a local file, or bytes in memory), for decoding
/// chunks in parallel.
pub(crate) type Reopen<'a, D> = &'a (dyn Fn() -> crate::Result<tiff::decoder::Decoder<D>> + Sync);

/// A decoded chunk: its index, its samples and the size `(width, height)` of the image data it
/// holds.
pub(crate) type DecodedChunk = (u32, tiff::decoder::DecodingResult, (usize, usize));

/// Decodes one chunk as [`read_chunk`] does, turning a panic in a decoder (a bug that a corrupt
/// file reaches) into an error: the file is refused, the caller and its other threads go on.
fn decode_chunk<R: Read + Seek>(
    dec: &mut tiff::decoder::Decoder<R>,
    codec: Option<&crate::tiff_codecs::ChunkCodec>,
    idx: u32,
    limit: usize,
    err: &(dyn Fn(tiff::TiffError) -> crate::Error + Sync),
) -> crate::Result<DecodedChunk> {
    let decoded = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| read_chunk(dec, codec, idx, limit)));
    match decoded {
        Ok(chunk) => {
            let chunk = chunk.map_err(err)?;
            let (w, h) = dec.chunk_data_dimensions(idx);
            Ok((idx, chunk, (w as usize, h as usize)))
        }
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown error".to_string());
            Err(crate::Error::InvalidData(format!("TIFF chunk {idx} could not be decoded: {message}")))
        }
    }
}

/// The offsets and byte counts of the chunks (tiles or strips) of one TIFF image, from its
/// `TileOffsets` and `TileByteCounts` (or `StripOffsets` and `StripByteCounts`). The `tiff` crate
/// keeps its own copy out of reach: its `get_tag_u64_vec` reads and decodes the tags again.
#[derive(Debug, Default)]
pub(crate) struct ChunkTables {
    pub offsets: Vec<u64>,
    pub counts: Vec<u64>,
}

impl ChunkTables {
    /// The tables of the image the decoder is positioned on, read whole.
    pub(crate) fn read<R: Read + Seek>(dec: &mut tiff::decoder::Decoder<R>) -> tiff::TiffResult<Self> {
        use tiff::tags::Tag;
        let (offsets, counts) = match dec.get_chunk_type() {
            tiff::decoder::ChunkType::Tile => (Tag::TileOffsets, Tag::TileByteCounts),
            tiff::decoder::ChunkType::Strip => (Tag::StripOffsets, Tag::StripByteCounts),
        };
        Ok(Self { offsets: dec.get_tag_u64_vec(offsets)?, counts: dec.get_tag_u64_vec(counts)? })
    }

    /// The offset and byte count of chunk `i`, `None` past the end of either table.
    pub(crate) fn get(&self, i: usize) -> Option<(u64, u64)> {
        Some((*self.offsets.get(i)?, *self.counts.get(i)?))
    }
}

/// Decodes one chunk, with `codec` when the `tiff` crate does not decode the image's compression.
/// A chunk the crate decodes is refused before its buffer is allocated when it takes more than
/// `limit` bytes ([`crate::TiffReadLimits::chunk_bytes`]): the buffer is allocated here, so the
/// crate's own check of its decoding limit is not reached.
fn read_chunk<R: Read + Seek>(
    dec: &mut tiff::decoder::Decoder<R>,
    codec: Option<&crate::tiff_codecs::ChunkCodec>,
    idx: u32,
    limit: usize,
) -> tiff::TiffResult<tiff::decoder::DecodingResult> {
    match codec {
        Some(c) => c.read(dec, idx),
        None => {
            // Into a buffer allocated here, fallibly, rather than by the crate, once its size is
            // known to be within the limit: a header can declare a chunk of terabytes, which a
            // fallible allocation does not refuse where the system overcommits memory.
            let layout = dec.image_chunk_buffer_layout(idx)?;
            if layout.len > limit {
                return Err(tiff::TiffError::LimitsExceeded);
            }
            let mut chunk = chunk_buffer(layout.sample_type, layout.len)?;
            let mut bytes = chunk.as_buffer(0);
            dec.read_chunk_bytes(idx, bytes.as_bytes_mut())?;
            Ok(chunk)
        }
    }
}

/// A chunk's buffer of `bytes` bytes of samples of type `t`, allocated fallibly.
fn chunk_buffer(t: Option<tiff::decoder::DecodingSampleType>, bytes: usize) -> tiff::TiffResult<tiff::decoder::DecodingResult> {
    use crate::dataset::fallible::filled_now as filled;
    use tiff::decoder::{DecodingResult as R, DecodingSampleType as S};
    let what = "a TIFF chunk";
    let n = |size: usize| bytes.div_ceil(size);
    Ok(match t {
        Some(S::U8) => R::U8(filled(n(1), 0, what)?),
        Some(S::I8) => R::I8(filled(n(1), 0, what)?),
        Some(S::U16) => R::U16(filled(n(2), 0, what)?),
        Some(S::I16) => R::I16(filled(n(2), 0, what)?),
        Some(S::F16) => R::F16(filled(n(2), half::f16::ZERO, what)?),
        Some(S::U32) => R::U32(filled(n(4), 0, what)?),
        Some(S::I32) => R::I32(filled(n(4), 0, what)?),
        Some(S::F32) => R::F32(filled(n(4), 0.0, what)?),
        Some(S::U64) => R::U64(filled(n(8), 0, what)?),
        Some(S::I64) => R::I64(filled(n(8), 0, what)?),
        Some(S::F64) => R::F64(filled(n(8), 0.0, what)?),
        None => return Err(tiff::TiffError::UnsupportedError(tiff::TiffUnsupportedError::UnknownInterpretation)),
    })
}

/// Decodes chunks (tiles or strips) of the image `ifd`, handing each to `place`, in no particular
/// order, with `codec` when the `tiff` crate does not decode the image's compression (alone). With
/// `reopen`, several threads decode, each with its own decoder over the file, while the calling
/// thread places the chunks; `dec`, positioned on the image, decodes the chunks no thread did (all
/// of them without `reopen`). A chunk taking more than `limit` bytes
/// ([`crate::TiffReadLimits::chunk_bytes`]) is refused, whichever decodes it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_chunks<R: Read + Seek, D: Read + Seek>(
    dec: &mut tiff::decoder::Decoder<R>,
    reopen: Option<Reopen<D>>,
    ifd: usize,
    chunks: &[u32],
    codec: Option<&crate::tiff_codecs::ChunkCodec>,
    limit: usize,
    err: &(dyn Fn(tiff::TiffError) -> crate::Error + Sync),
    mut place: impl FnMut(DecodedChunk) -> crate::Result<()>,
) -> crate::Result<()> {
    // Chunks `..first` were decoded by other threads.
    #[cfg(threads)]
    let first = match reopen {
        Some(reopen) => decode_in_threads(reopen, ifd, chunks, codec, limit, err, &mut place)?,
        None => 0,
    };
    #[cfg(not(threads))]
    let first = {
        let _ = (reopen, ifd);
        0
    };
    for &idx in &chunks[first..] {
        place(decode_chunk(dec, codec, idx, limit, err)?)?;
    }
    Ok(())
}

/// Decodes chunks on several threads, each with its own decoder over the file, handing them to
/// `place` on the calling thread. Returns how many of the first chunks were decoded: a thread that
/// cannot open the file leaves its share to the caller.
#[cfg(not(target_family = "wasm"))]
fn decode_in_threads<D: Read + Seek>(
    reopen: Reopen<D>,
    ifd: usize,
    chunks: &[u32],
    codec: Option<&crate::tiff_codecs::ChunkCodec>,
    limit: usize,
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
                // A thread that cannot be started (its stack not allocated) leaves the chunks to
                // the others, or to the caller.
                let _ = std::thread::Builder::new().spawn_scoped(scope, move || {
                    // A thread that cannot open the file leaves the chunks to the others.
                    let Ok(mut d) = reopen() else { return };
                    if ifd != 0 && d.seek_to_image(ifd).is_err() {
                        return;
                    }
                    while !stop.load(Ordering::Relaxed) {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(&idx) = chunks.get(i) else { break };
                        let r = decode_chunk(&mut d, codec, idx, limit, err);
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

/// Decodes chunks on the thread pool of WebAssembly built with threads, a few per thread at a
/// time, each thread with its own decoder over the TIFF's bytes, and hands them to `place` on the
/// calling thread. Returns how many of the first chunks were decoded (none without a pool).
#[cfg(all(threads, target_family = "wasm"))]
fn decode_in_threads<D: Read + Seek>(
    reopen: Reopen<D>,
    ifd: usize,
    chunks: &[u32],
    codec: Option<&crate::tiff_codecs::ChunkCodec>,
    limit: usize,
    err: &(dyn Fn(tiff::TiffError) -> crate::Error + Sync),
    place: &mut impl FnMut(DecodedChunk) -> crate::Result<()>,
) -> crate::Result<usize> {
    let workers = crate::par::threads().min(chunks.len());
    if workers <= 1 {
        return Ok(0);
    }
    let open = || -> crate::Result<tiff::decoder::Decoder<D>> {
        let mut d = reopen()?;
        if ifd != 0 {
            d.seek_to_image(ifd).map_err(err)?;
        }
        Ok(d)
    };
    // A batch at a time: decoding never runs far ahead of placing.
    for batch in chunks.chunks(workers * 4) {
        let decoded = crate::par::map_init(batch, open, |d, &idx| -> crate::Result<DecodedChunk> {
            let d = d.as_mut().map_err(|e| crate::Error::InvalidData(e.to_string()))?;
            let c = read_chunk(d, codec, idx, limit).map_err(err)?;
            let (w, h) = d.chunk_data_dimensions(idx);
            Ok((idx, c, (w as usize, h as usize)))
        });
        for c in decoded {
            place(c?)?;
        }
    }
    Ok(chunks.len())
}

/// Reads a GeoTIFF (or BigTIFF, or COG) into memory: the full-resolution image of the first IFD.
pub(crate) fn read_geotiff(path: &std::path::Path) -> crate::Result<crate::RasterDataset> {
    read_geotiff_with_limits(path, crate::TiffReadLimits::default())
}

pub(crate) fn read_geotiff_with_limits(path: &std::path::Path, limits: crate::cog::TiffReadLimits) -> crate::Result<crate::RasterDataset> {
    use crate::Error;
    use crate::cog::{ChunkGeom, Window, place};
    use tiff::decoder::DecodingResult;
    use tiff::tags::Tag;
    use wbraster::raster::RasterData;

    let tiff_err = |e: tiff::TiffError| match e {
        tiff::TiffError::LimitsExceeded => limits.error()(e),
        e => Error::InvalidData(format!("{}: {e}", path.display())),
    };
    let mut tags = read_geo_tags(path).unwrap_or_default();
    tags.check_structure()?;
    tags.apply_sidecar_crs(path);
    tags.warn_unreadable(&path.display().to_string());
    // The decoder rejects palette images; read their indices as grayscale instead.
    let mut dec = open_decoder(path, &tags, limits)?;
    let (w, h) = dec.dimensions().map_err(tiff_err)?;
    check_open(&mut dec, tiff_err)?;
    let (w, h) = (w as usize, h as usize);
    let samples = dec.find_tag_unsigned::<u16>(Tag::SamplesPerPixel).map_err(tiff_err)?.unwrap_or(1).max(1) as usize;
    let planar = dec.find_tag_unsigned::<u16>(Tag::PlanarConfiguration).map_err(tiff_err)? == Some(2);
    // Complex samples are two bands each, their real then their imaginary parts, which a chunk
    // holds as two samples (see `RasterDataset::complex`).
    let complex = crate::cog::sample_type(&mut dec).is_ok_and(|(_, complex)| complex);
    let parts = if complex { 2 } else { 1 };
    let (cw, ch) = dec.chunk_dimensions();
    let (cw, ch) = (cw as usize, ch as usize);
    let across = w.div_ceil(cw);
    let down = h.div_ceil(ch);
    let per_plane = across * down;
    let plane_count = if planar { samples } else { 1 };
    let raster_len = crate::dataset::fallible::len(&[w, h, samples, parts], "the raster")?;
    let chunk_count = crate::dataset::fallible::len(&[plane_count, per_plane], "the TIFF chunk list")?;
    if chunk_count > u32::MAX as usize {
        return Err(Error::InvalidData(format!("{}: {chunk_count} chunks", path.display())));
    }
    let spp = if planar { parts } else { samples * parts };
    let window = Window::full(w, h);
    // Each sample of a chunk (each part of a complex one) goes to its band.
    let maps: Vec<Vec<(usize, usize)>> = if planar {
        (0..samples).map(|p| (0..parts).map(|i| (i, p * parts + i)).collect()).collect()
    } else {
        vec![(0..samples * parts).map(|s| (s, s)).collect()]
    };
    let mut chunks: Vec<u32> = Vec::new();
    crate::dataset::fallible::reserve(&mut chunks, chunk_count, "the TIFF chunk list")?;
    chunks.extend((0..chunk_count).map(|i| i as u32));
    // Compressed chunks whose stored bytes run past the end of the file: refused, as GDAL does.
    let compression = dec.find_tag_unsigned::<u16>(Tag::Compression).map_err(tiff_err)?.unwrap_or(1);
    if compression != 1 {
        let (offsets_tag, counts_tag) = match dec.get_chunk_type() {
            tiff::decoder::ChunkType::Tile => (Tag::TileOffsets, Tag::TileByteCounts),
            tiff::decoder::ChunkType::Strip => (Tag::StripOffsets, Tag::StripByteCounts),
        };
        let offsets = dec.get_tag_u64_vec(offsets_tag).map_err(tiff_err)?;
        let counts = dec.get_tag_u64_vec(counts_tag).map_err(tiff_err)?;
        check_stored_chunks(compression, offsets.into_iter().zip(counts), std::fs::metadata(path)?.len())?;
    }

    // Band-major output, typed after the first chunk decoded.
    let mut data: Option<RasterData> = None;
    let reopen = || open_decoder(path, &tags, limits);
    // The compressions the `tiff` crate does not decode (alone), with the same limit.
    let codec = crate::tiff_codecs::ChunkCodec::new(&mut dec, limits.chunk_bytes, None).map_err(tiff_err)?;
    decode_chunks(&mut dec, Some(&reopen), 0, &chunks, codec.as_ref(), limits.chunk_bytes, &tiff_err, |(idx, chunk, (dw, dh))| {
        let idx = idx as usize;
        let (plane, rest) = (idx / per_plane, idx % per_plane);
        let geom = ChunkGeom { x0: (rest % across) * cw, y0: (rest / across) * ch, dw, dh, spp };
        let map = &maps[if planar { plane } else { 0 }];
        macro_rules! place {
            ($($out:ident <- $res:ident),*) => {
                match chunk {
                    $(DecodingResult::$res(v) => {
                        let out = match &mut data {
                            Some(out) => out,
                            None => data.insert(RasterData::$out(crate::dataset::fallible::filled(raster_len, Default::default(), "the raster")?)),
                        };
                        let RasterData::$out(out) = out else {
                            return Err(Error::InvalidData("TIFF chunks differ in sample type".into()));
                        };
                        place(out, &v, &geom, window, map);
                    })*
                    DecodingResult::F16(v) => {
                        let out = match &mut data {
                            Some(out) => out,
                            None => data.insert(RasterData::F32(crate::dataset::fallible::filled(raster_len, 0.0, "the raster")?)),
                        };
                        let RasterData::F32(out) = out else {
                            return Err(Error::InvalidData("TIFF chunks differ in sample type".into()));
                        };
                        let mut f = Vec::new();
                        crate::dataset::fallible::reserve(&mut f, v.len(), "a TIFF chunk")?;
                        f.extend(v.iter().map(|x| x.to_f32()));
                        let v = f;
                        place(out, &v, &geom, window, map);
                    }
                }
            };
        }
        place!(U8 <- U8, U16 <- U16, U32 <- U32, U64 <- U64, I8 <- I8, I16 <- I16, I32 <- I32, I64 <- I64, F32 <- F32, F64 <- F64);
        Ok(())
    })?;
    let data = data.ok_or_else(|| Error::InvalidData(format!("{}: no image data", path.display())))?;
    // CMYK as GDAL reads it, as RGBA (as the lazy reader does).
    // Its bands' colour interpretations are the tags' (`GeoTags::band_properties`), which apply
    // the same rule.
    let (data, samples) = match data {
        RasterData::U8(v) if crate::cog::reads_cmyk_as_rgba(&mut dec, samples, wbraster::DataType::U8)? => {
            (crate::cog::cmyk_to_rgba(&v[..4 * w * h], w * h, &[1, 2, 3, 4])?, 4)
        }
        data => (data, samples * parts),
    };
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
    ds.complex = complex;
    ds.band_metadata = tags.band_properties.clone();
    ds.color_table = tags.color_table.clone();
    ds.gcps = tags.gcps.clone();
    if !tags.band_nodata.is_empty() {
        // Each band of the raster, both parts of a complex band with that band's value.
        ds.band_nodata = (0..samples)
            .map(|r| {
                let b = r / parts + 1;
                tags.band_nodata.iter().find(|(i, _)| *i == b).map(|(_, v)| *v).or(tags.nodata)
            })
            .collect();
        ds.has_nodata = ds.band_nodata[0].is_some();
        ds.raster.nodata = ds.band_nodata[0].unwrap_or(-9999.);
    }
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

    /// A little-endian classic TIFF of one 4 x 2 Byte strip whose ImageWidth entry has type
    /// `width_type` and count `width_count`.
    fn tiff_with_width_entry(width_type: u16, width_count: u32) -> Vec<u8> {
        let tags: [(u16, u16, u32, u32); 8] = [
            (256, width_type, width_count, 4),
            (257, 3, 1, 2),
            (258, 3, 1, 8),
            (259, 3, 1, 1),
            (262, 3, 1, 1),
            (273, 4, 1, 0),
            (278, 3, 1, 2),
            (279, 4, 1, 8),
        ];
        let data_offset = 8 + 2 + tags.len() as u32 * 12 + 4;
        let mut b = b"II*\0".to_vec();
        b.extend(8u32.to_le_bytes());
        b.extend((tags.len() as u16).to_le_bytes());
        for (tag, typ, count, value) in tags {
            b.extend(tag.to_le_bytes());
            b.extend(typ.to_le_bytes());
            b.extend(count.to_le_bytes());
            b.extend((if tag == 273 { data_offset } else { value }).to_le_bytes());
        }
        b.extend(0u32.to_le_bytes());
        b.extend([1u8, 2, 3, 4, 5, 6, 7, 8]);
        b
    }

    /// The colour interpretations GDAL gives an RGBA GeoTIFF's bands are kept by the whole-file
    /// reader, and a GeoTIFF or COG written from it is RGB with an alpha band, as GDAL's
    /// `CreateCopy` writes it (`terrio.convert`); a gray file gets none.
    #[test]
    fn rgba_colour_interpretations_go_from_the_reader_to_the_writer() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("rgba.tif").to_str().unwrap().to_string();
        let cfg = wbraster::RasterConfig { cols: 40, rows: 30, bands: 4, data_type: wbraster::DataType::U8, ..Default::default() };
        let r = wbraster::Raster::from_data_native(cfg, wbraster::raster::RasterData::U8((0..4800).map(|i| (i % 251) as u8).collect()))
            .unwrap();
        let rgba = |o: &[(&str, &str)]| crate::RasterWriteOptions {
            driver: Some(crate::formats::RasterDriver::GTiff),
            creation_options: o.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            ..Default::default()
        };
        let ds = crate::RasterDataset::with_driver(r, crate::formats::RasterDriver::GTiff, false);
        crate::write_raster_dataset(&ds, &src, &rgba(&[("PHOTOMETRIC", "RGB"), ("ALPHA", "YES")])).unwrap();
        let read = read_geotiff(std::path::Path::new(&src)).unwrap();
        let names: Vec<Option<&str>> = read.band_metadata.iter().map(|b| b.color_interp.as_deref()).collect();
        assert_eq!(names, [Some("Red"), Some("Green"), Some("Blue"), Some("Alpha")]);
        let photometric_and_extra = |path: &str| {
            let mut dec = tiff::decoder::Decoder::new(std::fs::File::open(path).unwrap()).unwrap();
            let p = dec.find_tag_unsigned::<u16>(tiff::tags::Tag::PhotometricInterpretation).unwrap();
            (p, dec.find_tag_unsigned_vec::<u16>(tiff::tags::Tag::ExtraSamples).unwrap())
        };
        for driver in [crate::formats::RasterDriver::GTiff, crate::formats::RasterDriver::Cog] {
            let out = dir.path().join(format!("{driver:?}.tif")).to_str().unwrap().to_string();
            crate::write_raster_dataset(&read, &out, &crate::RasterWriteOptions { driver: Some(driver), ..Default::default() }).unwrap();
            assert_eq!(photometric_and_extra(&out), (Some(2), Some(vec![2])), "{driver:?}");
        }
        // Each option is derived unless given, as GDAL does: the alpha with the photometric given.
        for (photometric, expected) in [("RGB", (Some(2), Some(vec![2]))), ("MINISBLACK", (Some(1), Some(vec![0, 0, 2])))] {
            let out = dir.path().join(format!("{photometric}.tif")).to_str().unwrap().to_string();
            crate::write_raster_dataset(&read, &out, &rgba(&[("PHOTOMETRIC", photometric)])).unwrap();
            assert_eq!(photometric_and_extra(&out), expected, "{photometric}");
        }
        // An alpha third Byte band: three Byte bands are RGB, so GDAL writes no alpha; nor is the
        // file refused for want of an extra sample.
        let gga = dir.path().join("gga.tif").to_str().unwrap().to_string();
        let cfg = wbraster::RasterConfig { cols: 4, rows: 3, bands: 3, data_type: wbraster::DataType::U8, ..Default::default() };
        let r = wbraster::Raster::from_data_native(cfg, wbraster::raster::RasterData::U8(vec![1; 36])).unwrap();
        let ds = crate::RasterDataset::with_driver(r, crate::formats::RasterDriver::GTiff, false);
        crate::write_raster_dataset(&ds, &gga, &rgba(&[("PHOTOMETRIC", "MINISBLACK"), ("ALPHA", "YES")])).unwrap();
        let read = read_geotiff(std::path::Path::new(&gga)).unwrap();
        assert_eq!(read.band_metadata.last().and_then(|b| b.color_interp.as_deref()), Some("Alpha"));
        let out = dir.path().join("gga-copy.tif").to_str().unwrap().to_string();
        crate::write_raster_dataset(&read, &out, &rgba(&[])).unwrap();
        assert_eq!(photometric_and_extra(&out), (Some(2), None));
        // A gray file: gray, as GDAL reads it, and written as it was: GDAL writes no colour
        // interpretation its photometric implies (GDAL 3.12.1 copies a gray file without
        // GDAL_METADATA).
        let gray = dir.path().join("gray.tif").to_str().unwrap().to_string();
        let cfg = wbraster::RasterConfig { cols: 4, rows: 3, bands: 1, data_type: wbraster::DataType::U8, ..Default::default() };
        let r = wbraster::Raster::from_data_native(cfg, wbraster::raster::RasterData::U8(vec![1; 12])).unwrap();
        crate::write_raster_dataset(&crate::RasterDataset::with_driver(r, crate::formats::RasterDriver::GTiff, false), &gray, &rgba(&[]))
            .unwrap();
        let read = read_geotiff(std::path::Path::new(&gray)).unwrap();
        assert_eq!(read.band_metadata.iter().map(|b| b.color_interp.as_deref()).collect::<Vec<_>>(), [Some("Gray")]);
        let gdal_metadata = |path: &str| {
            let mut dec = tiff::decoder::Decoder::new(std::fs::File::open(path).unwrap()).unwrap();
            dec.find_tag(tiff::tags::Tag::Unknown(42112)).unwrap().is_some()
        };
        for (name, source) in [("gray-copy.tif", &read), ("rgba-copy.tif", &read_geotiff(std::path::Path::new(&src)).unwrap())] {
            let out = dir.path().join(name).to_str().unwrap().to_string();
            crate::write_raster_dataset(source, &out, &rgba(&[])).unwrap();
            assert!(!gdal_metadata(&out), "{name}");
        }
        // Three gray Byte bands are written as RGB (as GDAL writes them), so their colour
        // interpretations are kept in GDAL_METADATA, as GDAL keeps them.
        let gray3 = dir.path().join("gray3.tif").to_str().unwrap().to_string();
        let cfg = wbraster::RasterConfig { cols: 4, rows: 3, bands: 3, data_type: wbraster::DataType::U8, ..Default::default() };
        let r = wbraster::Raster::from_data_native(cfg, wbraster::raster::RasterData::U8(vec![1; 36])).unwrap();
        let ds = crate::RasterDataset::with_driver(r, crate::formats::RasterDriver::GTiff, false);
        crate::write_raster_dataset(&ds, &gray3, &rgba(&[("PHOTOMETRIC", "MINISBLACK")])).unwrap();
        let read = read_geotiff(std::path::Path::new(&gray3)).unwrap();
        let out = dir.path().join("gray3-copy.tif").to_str().unwrap().to_string();
        crate::write_raster_dataset(&read, &out, &rgba(&[])).unwrap();
        assert_eq!(photometric_and_extra(&out), (Some(2), None));
        assert!(gdal_metadata(&out));
        let names: Vec<String> =
            (1..=3).map(|b| read_geotiff(std::path::Path::new(&out)).unwrap().band_color_interp(b).to_string()).collect();
        assert_eq!(names, ["Gray", "Undefined", "Undefined"]);
    }

    /// A structural tag libtiff cannot read (in a fuzzed file, ImageWidth retyped
    /// UNDEFINED) makes GDAL refuse the file at open: terrio refuses it there too, rather than
    /// spending minutes decoding what the crate made of it. Integer types are read.
    #[test]
    fn a_directory_libtiff_cannot_read_is_refused_at_open() {
        for (typ, count, refused) in
            [(3u16, 1u32, false), (4, 1, false), (1, 1, false), (7, 1, true), (2, 1, true), (11, 1, true), (3, 2, true)]
        {
            let bytes = tiff_with_width_entry(typ, count);
            let tags = read_geo_tags_from(std::io::Cursor::new(&bytes)).unwrap();
            assert_eq!(tags.check_structure().is_err(), refused, "ImageWidth of type {typ}, count {count}");
            let opened = crate::cog::LazyRaster::from_tiff_bytes(bytes);
            assert_eq!(opened.is_err(), refused, "ImageWidth of type {typ}, count {count}");
        }
        // Not structural: a tag of any type is left to the reader.
        assert_eq!(structural_tag_problem(305, 7, 3), None);
        // ExtraSamples is an array.
        assert_eq!(structural_tag_problem(338, 3, 2), None);
        assert!(structural_tag_problem(338, 7, 2).is_some());
    }

    /// Found by the `tiff` fuzz target: a citation GeoKey whose offset falls inside a multi-byte
    /// character of the ASCII parameters panicked (`PanicException` in Python).
    #[test]
    fn a_citation_inside_a_multibyte_character_is_read() {
        let mut out = GeoTags::default();
        let keys = [1, 1, 0, 2, 1024, 0, 1, 1, 1026, GEO_ASCII_PARAMS, 3, 1];
        apply_geokeys(&mut out, &keys, None, Some("\u{fffd}abc|"));
    }

    #[test]
    fn metadata_xml_handles_quotes_entities_and_validated_color_roles() {
        use crate::drivers::gtiff_write::{gdal_metadata_items, parse_gdal_band_metadata, parse_gdal_metadata};
        let xml = "<GDALMetadata><Item name='COLORINTERP' sample='0' role='colorinterp'>Gr&#97;y</Item><Item name='title'>a&amp;lt;b &amp;amp;lt;literal</Item><Item name='note' sample='1'>c&amp;amp;d &amp;amp;amp;literal</Item></GDALMetadata>";
        let items = gdal_metadata_items(xml);
        assert_eq!(items[0].attr("role"), Some("colorinterp"));
        assert_eq!(color_interpretation(&items[0].text), "Gray");
        assert_eq!(color_interpretation("not a color &amp; role"), "Undefined");
        assert_eq!(parse_gdal_metadata(xml), [("title".into(), "a<b &lt;literal".into())]);
        assert_eq!(parse_gdal_band_metadata(xml), [(1, "note".into(), "c&d &amp;literal".into())]);
    }

    #[test]
    fn vertical_geokeys_make_a_compound_crs() {
        // The GeoKeys GDAL 3.13 writes for EPSG:4326+5773: geographic, PixelIsArea, 4326, 5773.
        let keys = [1, 1, 0, 4, 1024, 0, 1, 2, 1025, 0, 1, 1, 2048, 0, 1, 4326, 4096, 0, 1, 5773];
        let crs = crate::crs::SpatialRef::from_raster_crs(&crs_from_geokeys(&keys, None, None));
        assert!(crs.to_wkt().unwrap().starts_with("COMPD_CS[\"WGS 84 + EGM96 height\","), "{crs:?}");
        // The registry's compound CRS of the two codes, as GDAL finds it (its WKT declares
        // EPSG:9707), not its horizontal CRS, which would drop the vertical CRS again.
        assert_eq!(crs.epsg, Some(9707));
        assert!(crs.to_wkt().unwrap().ends_with(",AUTHORITY[\"EPSG\",\"9707\"]]"));
        assert_eq!(crs.compound_parts(), Some((4326, 5773)));
        assert!(crs.same_as(&crate::crs::SpatialRef::parse("EPSG:4326+5773").unwrap()));
        // Heights are EGM96 heights: WGS 84 ellipsoidal heights differ by the geoid undulation.
        let t = crate::crs::Transformer::new(&crs, &crate::crs::SpatialRef::from_epsg(4979).unwrap()).unwrap();
        let direct = crate::crs::Transformer::new(
            &crate::crs::SpatialRef::parse("EPSG:4326+5773").unwrap(),
            &crate::crs::SpatialRef::from_epsg(4979).unwrap(),
        )
        .unwrap();
        assert_eq!(t.transform_3d(153.0, -27.0, 0.0).ok(), direct.transform_3d(153.0, -27.0, 0.0).ok());
        // Without the vertical key, or with an unknown or user-defined one: the horizontal CRS.
        for v in [None, Some(32767), Some(1)] {
            let mut keys = keys[..16].to_vec();
            keys[3] = 3;
            if let Some(v) = v {
                keys.extend([4096, 0, 1, v]);
                keys[3] = 4;
            }
            assert_eq!(crs_from_geokeys(&keys, None, None).epsg, Some(4326), "{v:?}");
        }
    }

    #[test]
    fn reads_integers_as_c_strtoll_and_strtoull_do() {
        // GDAL's text for 2^63 given as a double, and its negation: their leading integers.
        assert_eq!((c_strtoull("9.2233720368547758e+18"), c_strtoll("-9.2233720368547758e+18")), (9, -9));
        assert_eq!((c_strtoull("9223372036854775808"), c_strtoll("-9223372036854775808")), (1 << 63, i64::MIN));
        // Overflows give the bounds; a negative unsigned value wraps.
        assert_eq!((c_strtoull("18446744073709551616"), c_strtoull("-1")), (u64::MAX, u64::MAX));
        assert_eq!((c_strtoll("9223372036854775808"), c_strtoll("-9223372036854775809")), (i64::MAX, i64::MIN));
        // White space and a sign before the digits; no digits read as 0.
        assert_eq!((c_strtoll(" \t+12abc"), c_strtoll("nan"), c_strtoull("-")), (12, 0, 0));
    }

    #[test]
    fn identifies_user_defined_utm() {
        assert_eq!(user_defined_projected(Some(16010), None, None, Some("WGS 84 / UTM zone 10N".into())), Some(32610));
        assert_eq!(user_defined_projected(None, Some(4269), None, Some("NAD83 / UTM zone 15N".into())), Some(26915));
        assert_eq!(user_defined_projected(Some(16133), None, None, None), Some(32733));
        assert_eq!(user_defined_projected(Some(16015), Some(32767), Some(6269), None), Some(26915));
        // The geographic CRS's datum wins over the citation, as its code does for GDAL.
        assert_eq!(user_defined_projected(Some(16015), Some(4326), None, Some("NAD83 / UTM zone 15N".into())), Some(32615));
        assert_eq!(user_defined_projected(Some(16015), Some(4269), Some(6326), None), Some(26915));
        // A UTM zone on NAD83(HARN), by its geographic CRS or its datum, is no WGS 84 (or NAD83)
        // UTM zone (#209).
        assert_eq!(user_defined_projected(Some(16033), Some(32767), Some(6152), None), None);
        // ECOSTRESS: user-defined keys whose geographic citation names WGS 84's datum; GDAL reads EPSG:32610.
        let keys = [
            1, 1, 0, 8, 1024, 0, 1, 1, 1026, 34737, 22, 0, 2048, 0, 1, 32767, 2049, 34737, 92, 22, 2050, 0, 1, 32767, 3072, 0, 1, 32767,
            3074, 0, 1, 16010, 3076, 0, 1, 9001,
        ];
        let ascii = "WGS 84 / UTM zone 10N|GCS Name = WGS 84|Datum = World Geodetic System 1984|Ellipsoid = WGS 84|Primem = Greenwich||";
        assert_eq!(crs_from_geokeys(&keys, None, Some(ascii)).epsg, Some(32610));
        // The same keys with another datum named keep it: not WGS 84's zone.
        let ascii = ascii.replace("World Geodetic System 1984", "Some Local Datum        ");
        assert_eq!(crs_from_geokeys(&keys, None, Some(&ascii)).epsg, None);
        assert_eq!(user_defined_projected(Some(16010), Some(4152), None, Some("NAD83 / UTM zone 10N".into())), None);
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

    /// The GeoKeys of PDAL's `autzen_trim.las`: a user-defined geographic CRS on NAD83(HARN)
    /// (GeogGeodeticDatumGeoKey 6152) under a user-defined Lambert conformal conic in feet.
    fn autzen_keys() -> (Vec<u16>, Vec<f64>, &'static str) {
        let keys = vec![
            1, 1, 0, 21, 1024, 0, 1, 1, 1025, 0, 1, 1, 1026, 34737, 38, 0, 2048, 0, 1, 32767, 2049, 34737, 60, 38, 2050, 0, 1, 6152, 2054,
            0, 1, 9102, 2057, 34736, 1, 7, 2059, 34736, 1, 6, 2061, 34736, 1, 8, 3059, 0, 1, 1, 3072, 0, 1, 32767, 3074, 0, 1, 32767, 3075,
            0, 1, 8, 3076, 0, 1, 9002, 3078, 34736, 1, 2, 3079, 34736, 1, 3, 3084, 34736, 1, 1, 3085, 34736, 1, 0, 3086, 34736, 1, 4, 3087,
            34736, 1, 5,
        ];
        let doubles = vec![41.75, -120.5, 43.0, 45.5, 1312335.958005249, 0.0, 298.257222101, 6378137.0, 0.0];
        (keys, doubles, "NAD_1983_HARN_Lambert_Conformal_Conic|GCS Name = GCS_North_American_1983_HARN|Primem = Greenwich||")
    }

    #[test]
    fn keeps_the_datum_of_a_user_defined_geographic_crs() {
        // autzen_trim.las: NAD83(HARN) / Oregon GIC Lambert (ft), as GDAL and PDAL read it, not
        // the same projection on plain NAD83 (#209).
        let (keys, doubles, ascii) = autzen_keys();
        let crs = crate::crs::SpatialRef::from_raster_crs(&crs_from_geokeys(&keys, Some(&doubles), Some(ascii)));
        assert_eq!(crs.epsg, Some(2994), "{crs:?}");
        let harn = crate::crs::SpatialRef::parse("EPSG:2994").unwrap();
        let (x, y) = crate::crs::Transformer::new(&crs, &harn).unwrap().transform(636001.76, 848935.2).unwrap();
        assert!((x - 636001.76).abs() < 1e-6 && (y - 848935.2).abs() < 1e-6, "({x}, {y})");

        // The same datum under a geographic CRS: NAD83(HARN), EPSG:4152.
        let geographic = [1, 1, 0, 5, 1024, 0, 1, 2, 2048, 0, 1, 32767, 2050, 0, 1, 6152, 2057, 34736, 1, 0, 2059, 34736, 1, 1];
        let info = crs_from_geokeys(&geographic, Some(&[6378137.0, 298.257222101]), None);
        assert_eq!(crate::crs::SpatialRef::from_raster_crs(&info).epsg, Some(4152), "{info:?}");
        // Not with another ellipsoid than the datum's.
        assert_eq!(crate::crs::geographic_on_datum(6152, "+ellps=clrk66"), None);

        // A UTM zone on it, by its ProjectionGeoKey alone as GDAL writes it: NAD83(HARN) / UTM
        // zone 10N (EPSG:3740), not WGS 84 / UTM zone 10N.
        let utm = [
            1, 1, 0, 8, 1024, 0, 1, 1, 2048, 0, 1, 32767, 2050, 0, 1, 6152, 2057, 34736, 1, 0, 2059, 34736, 1, 1, 3072, 0, 1, 32767, 3074,
            0, 1, 16010, 3076, 0, 1, 9001,
        ];
        let info = crs_from_geokeys(&utm, Some(&[6378137.0, 298.257222101]), None);
        assert_eq!(crate::crs::SpatialRef::from_raster_crs(&info).epsg, Some(3740), "{info:?}");
        // And by the geographic CRS's code, whatever the citation says.
        let mut on_4152 = utm;
        on_4152[8..16].copy_from_slice(&[2048, 0, 1, 4152, 3073, 34737, 21, 0]);
        let info = crs_from_geokeys(&on_4152, Some(&[6378137.0, 298.257222101]), Some("NAD83 / UTM zone 10N|"));
        assert_eq!(crate::crs::SpatialRef::from_raster_crs(&info).epsg, Some(3740), "{info:?}");
    }

    /// A UTM zone as GDAL 3.12.1 writes it for a CRS without EPSG code (ProjectionGeoKey 16033
    /// and ProjLinearUnitsGeoKey `unit`), on a user-defined geographic CRS: on the WGS 84 datum
    /// (GeogGeodeticDatumGeoKey 6326), else on a datum of its own based on GRS 1980.
    fn utm_keys(unit: u16, wgs84: bool) -> (Vec<u16>, Vec<f64>) {
        let mut keys = vec![1, 1, 0, 0, 1024, 0, 1, 1, 1025, 0, 1, 1, 2048, 0, 1, 32767, 2054, 0, 1, 9102];
        let rf = match wgs84 {
            true => {
                keys.extend([2050, 0, 1, 6326]);
                298.257223563
            }
            false => {
                keys.extend([2050, 0, 1, 32767, 2056, 0, 1, 7019]);
                298.257222101
            }
        };
        keys.extend([2057, GEO_DOUBLE_PARAMS, 1, 0, 2059, GEO_DOUBLE_PARAMS, 1, 1, 3072, 0, 1, 32767, 3074, 0, 1, 16033, 3076, 0, 1, unit]);
        keys[3] = (keys.len() / 4 - 1) as u16;
        (keys, vec![6378137.0, rf])
    }

    /// The coordinates of (16°E, 48°N) in the CRS read from GeoKeys.
    fn utm_point(keys: &[u16], doubles: &[f64], ascii: Option<&str>) -> (crate::crs::SpatialRef, (f64, f64)) {
        let crs = crate::crs::SpatialRef::from_raster_crs(&crs_from_geokeys(keys, Some(doubles), ascii));
        let wgs84 = crate::crs::SpatialRef::from_epsg(4326).unwrap();
        let xy = crate::crs::Transformer::new(&wgs84, &crs).unwrap().transform(16.0, 48.0).unwrap();
        (crs, xy)
    }

    #[test]
    fn a_utm_zone_in_feet_is_in_feet() {
        // GDAL reads a transverse Mercator in US survey feet without EPSG code, the zone's false
        // easting (500000 m) in feet: not EPSG:32633 in metres. Coordinates are PROJ's for the
        // CRS GDAL reads (rasterio 1.5.0).
        let (mut keys, doubles) = utm_keys(9003, true);
        let (crs, (x, y)) = utm_point(&keys, &doubles, None);
        let wkt = crs.to_wkt().unwrap();
        assert_eq!(crs.epsg, None, "{wkt}");
        assert!(wkt.contains("PARAMETER[\"false_easting\",1640416.66666667]") && wkt.contains("UNIT[\"US survey foot\""), "{wkt}");
        assert!((x - 1885150.7982145923).abs() < 1e-6 && (y - 17443482.20363316).abs() < 1e-6, "({x}, {y})");
        // Nor by a citation naming the zone in metres.
        keys.extend([1026, GEO_ASCII_PARAMS, 22, 0]);
        keys[3] += 1;
        let (crs, (x2, _)) = utm_point(&keys, &doubles, Some("WGS 84 / UTM zone 33N|"));
        assert_eq!((crs.epsg, x2), (None, x));
        // In metres it is the zone.
        let (keys, doubles) = utm_keys(9001, true);
        assert_eq!(utm_point(&keys, &doubles, None).0.epsg, Some(32633));
    }

    #[test]
    fn a_utm_zone_on_a_datum_of_its_own_is_not_wgs_84s() {
        // GRS 1980 under a user-defined datum, in international feet, then in Clarke's feet (an
        // EPSG unit PROJ strings have no name for, which GDAL names from the registry).
        let (keys, doubles) = utm_keys(9001, false);
        assert_eq!(utm_point(&keys, &doubles, None).0.epsg, None);
        let (keys, doubles) = utm_keys(9002, false);
        let (crs, (x, y)) = utm_point(&keys, &doubles, None);
        assert_eq!(crs.epsg, None);
        assert!((x - 1885154.5685259511).abs() < 1e-6 && (y - 17443517.090266943).abs() < 1e-6, "({x}, {y})");
        let (keys, doubles) = utm_keys(9005, false);
        let (crs, (x, y)) = utm_point(&keys, &doubles, None);
        let wkt = crs.to_wkt().unwrap();
        assert!(wkt.contains("UNIT[\"Clarke's foot\",0.3047972654,AUTHORITY[\"EPSG\",\"9005\"]]"), "{wkt}");
        assert!((x - 1885171.4818787542).abs() < 1e-6 && (y - 17443673.59115212).abs() < 1e-6, "({x}, {y})");
        // A user-defined unit, named by the citation GDAL writes for it.
        let (mut keys, mut doubles) = utm_keys(32767, false);
        keys.extend([3073, GEO_ASCII_PARAMS, 20, 0, 3077, GEO_DOUBLE_PARAMS, 1, 2]);
        keys[3] += 2;
        doubles.push(0.5);
        let (crs, (x, _)) = utm_point(&keys, &doubles, Some("LUnits = half metre|"));
        assert!(crs.to_wkt().unwrap().contains("UNIT[\"half metre\",0.5]"), "{:?}", crs.to_wkt());
        assert!((x - 1885154.5685259511 * 0.3048 / 0.5).abs() < 1e-3, "{x}");
    }

    #[test]
    fn a_geographic_crs_in_grads_is_in_grads() {
        // As GDAL writes WGS 84 in grads (GeogAngularUnitsGeoKey 9105): coordinates in grads, no
        // EPSG code.
        let keys = [
            1, 1, 0, 7, 1024, 0, 1, 2, 1025, 0, 1, 1, 2048, 0, 1, 32767, 2049, 34737, 72, 0, 2050, 0, 1, 6326, 2054, 0, 1, 9105, 2057,
            34736, 1, 0, 2059, 34736, 1, 1,
        ];
        let ascii = "GCS Name = unknown|Ellipsoid = WGS 84|Primem = Greenwich|AUnits = grad||";
        let info = crs_from_geokeys(&keys, Some(&[6378137.0, 298.257223563]), Some(ascii));
        let crs = crate::crs::SpatialRef::from_raster_crs(&info);
        assert_eq!(crs.epsg, None, "{info:?}");
        assert!(info.wkt.as_deref().unwrap().contains("UNIT[\"grad\",0.015707963267949"), "{info:?}");
        let wgs84 = crate::crs::SpatialRef::from_epsg(4326).unwrap();
        let (x, y) = crate::crs::Transformer::new(&wgs84, &crs).unwrap().transform(2.0, 46.0).unwrap();
        assert!((x - 2.2222222222222223).abs() < 1e-9 && (y - 51.11111111111111).abs() < 1e-9, "({x}, {y})");

        // A Lambert conic on it, its angles in grads, as GDAL (3.9.1 and later) writes them.
        let keys = [
            1, 1, 0, 14, 1024, 0, 1, 1, 1025, 0, 1, 1, 2048, 0, 1, 32767, 2050, 0, 1, 32767, 2054, 0, 1, 9105, 2056, 0, 1, 32767, 2057,
            34736, 1, 0, 2059, 34736, 1, 1, 3072, 0, 1, 32767, 3074, 0, 1, 32767, 3075, 0, 1, 9, 3076, 0, 1, 9001, 3080, 34736, 1, 2, 3081,
            34736, 1, 3, 3082, 34736, 1, 4, 3083, 34736, 1, 5, 3092, 34736, 1, 6,
        ];
        let doubles = [6378249.2, 293.466021293627, 2.5969212999999995, 51.99999999999999, 600000.0, 2200000.0, 0.99987742];
        let crs = crate::crs::SpatialRef::from_raster_crs(&crs_from_geokeys(&keys, Some(&doubles), None));
        let (x, y) = crate::crs::Transformer::new(&wgs84, &crs).unwrap().transform(2.5, 46.5).unwrap();
        assert!((x - 612493.9205394582).abs() < 1e-6 && (y - 2166667.8499515457).abs() < 1e-6, "({x}, {y})");
    }

    #[test]
    fn reads_the_inverse_flattening_and_esri_crs_as_gdal() {
        // A custom LAEA on GRS 1980 as GDAL writes it: GDAL names the ellipsoid by its code and
        // computes the inverse flattening from the axes (298.257222101004).
        let keys = [
            1, 1, 0, 15, 1024, 0, 1, 1, 1025, 0, 1, 1, 1026, 34737, 8, 0, 2048, 0, 1, 32767, 2049, 34737, 83, 8, 2050, 0, 1, 32767, 2054,
            0, 1, 9102, 2056, 0, 1, 7019, 2057, 34736, 1, 0, 2059, 34736, 1, 1, 3072, 0, 1, 32767, 3075, 0, 1, 10, 3082, 34736, 1, 2, 3083,
            34736, 1, 3, 3088, 34736, 1, 4, 3089, 34736, 1, 5,
        ];
        let ascii = "unknown|GCS Name = unknown|Datum = Unknown based on GRS 1980 ellipsoid|Primem = Greenwich||";
        let doubles = [6378137.0, 298.257222101, 4321000.0, 3210000.0, 10.0, 50.0];
        let wkt = crs_from_geokeys(&keys, Some(&doubles), Some(ascii)).wkt.unwrap();
        assert!(wkt.contains("SPHEROID[\"GRS 1980\",6378137,298.257222101004,"), "{wkt}");
        // The citation names an ESRI CRS the keys define: GDAL takes ESRI:102003.
        let keys = [
            1, 1, 0, 13, 1024, 0, 1, 1, 1025, 0, 1, 1, 1026, 34737, 39, 0, 2048, 0, 1, 4269, 2054, 0, 1, 9102, 3072, 0, 1, 32767, 3074, 0,
            1, 32767, 3075, 0, 1, 11, 3076, 0, 1, 9001, 3078, 34736, 1, 0, 3079, 34736, 1, 1, 3080, 34736, 1, 2, 3081, 34736, 1, 3, 3082,
            34736, 1, 4, 3083, 34736, 1, 5,
        ];
        let doubles = [29.5, 45.5, -96.0, 37.5, 0.0, 0.0];
        let info = crs_from_geokeys(&keys, Some(&doubles), Some("USA_Contiguous_Albers_Equal_Area_Conic|"));
        let crs = crate::crs::SpatialRef::from_raster_crs(&info);
        assert_eq!((crs.esri, crs.name().as_deref()), (Some(102003), Some("USA_Contiguous_Albers_Equal_Area_Conic")), "{info:?}");
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

    /// A chunk's buffer is allocated fallibly: a size no allocator gives, past what a `Vec` holds
    /// or past the address space, is an error for that file, where `vec![0; n]` would panic
    /// ("capacity overflow") or end the process ("memory allocation of ... failed"). The limits
    /// (`TiffReadLimits`) refuse such chunks first, unless a caller lifts them.
    #[test]
    fn a_chunk_buffer_no_allocator_gives_is_an_error() {
        use tiff::decoder::DecodingSampleType as S;
        for (t, bytes) in [(S::U16, usize::MAX), (S::F64, usize::MAX - 7), (S::U8, 1 << 50), (S::F32, 1 << 52)] {
            let e = chunk_buffer(Some(t), bytes).map(|_| ()).unwrap_err().to_string();
            assert!(e.contains("cannot allocate") && e.contains("for a TIFF chunk"), "{t:?} {bytes}: {e}");
        }
        assert!(matches!(chunk_buffer(Some(S::U16), 64).unwrap(), tiff::decoder::DecodingResult::U16(v) if v.len() == 32));
    }
}

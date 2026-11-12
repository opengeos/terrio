// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrgeometryfactory.cpp
//     ogr/ogrsf_frmts/shape/ogrshapelayer.cpp
//     apps/ogr2ogr_lib.cpp
//     ogr/ogrsf_frmts/shape/shape2ogr.cpp
//   GEOS newest:
//     src/operation/polygonize/
// No code is translated from these sources.

//! Esri Shapefile: `.shp`, `.shx`, `.dbf`, `.cpg` and `.prj`.
//!
//! Reading keeps Z and M values, groups polygon rings into polygons by orientation and
//! containment, and decodes text as the `.cpg` file (or the DBF language driver) declares, falling
//! back to Latin-1 for text that is not UTF-8.
//!
//! Writing stores text as UTF-8, declared in a `.cpg` file. Field names are truncated to 10 bytes
//! and made unique the way `ogr2ogr` does it (`populati_1`, `NAME2`), and field widths follow GDAL's: 9 or 18
//! digits for integers, 24 with 15 decimals for reals, 80 characters (or the longest value) for
//! strings.

use super::profile::LayerProfile;
use crate::crs::SpatialRef;
use crate::ops::simplify::open_ring;
use crate::{Error, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Ring};

const NULL_SHAPE: i32 = 0;

/// Finds a sidecar file (`dbf`, `prj`, `cpg`) next to the `.shp`, in either letter case.
fn sidecar(base: &Path, ext: &str) -> Option<PathBuf> {
    [ext.to_string(), ext.to_ascii_uppercase()].into_iter().map(|e| base.with_extension(e)).find(|p| p.exists())
}

/// Decode a present `.prj` without silently treating unreadable text as a missing CRS.
/// Invalid UTF-8 remains unsupported; never guess an encoding or alter a CRS name.
pub(crate) fn decode_prj(bytes: Vec<u8>, name: &str) -> Option<String> {
    match String::from_utf8(bytes) {
        Ok(text) => Some(text),
        Err(error) => {
            let name = quote_prj_text(name);
            crate::warnings::warn(format!("{name} is not UTF-8 text ({}): the layer is read without a CRS", error.utf8_error()));
            None
        }
    }
}

/// Read a sidecar that was found next to a Shapefile. Absence is handled by the caller.
pub(crate) fn read_prj(path: &Path) -> Option<String> {
    match std::fs::read(path) {
        Ok(bytes) => decode_prj(bytes, &path.display().to_string()),
        Err(error) => {
            warn_prj_read_error(&path.to_string_lossy(), &error);
            None
        }
    }
}

/// Report a present sidecar whose payload cannot be read, including an archive member.
pub(crate) fn warn_prj_read_error(name: &str, error: &impl std::fmt::Display) {
    let name = quote_prj_text(name);
    crate::warnings::warn(format!("{name} cannot be read ({error}): the layer is read without a CRS"));
}

/// How the rings of a polygon record make polygons: the methods of GDAL's
/// `OGR_ORGANIZE_POLYGONS`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OrganizePolygons {
    /// Clockwise rings are exteriors and each counter-clockwise ring a hole of the smallest
    /// exterior containing it (`ONLY_CCW`, GDAL's default for shapefiles).
    #[default]
    OnlyCcw,
    /// By containment, whatever their winding (`DEFAULT`): a ring inside an odd number of others
    /// is a hole of the smallest ring containing it. For files that wind every ring one way.
    Containment,
    /// Each counter-clockwise ring is a hole of the clockwise ring before it
    /// (`CCW_INNER_JUST_AFTER_CW_OUTER`). When the first ring is not clockwise, as `ONLY_CCW`.
    CcwInnerJustAfterCwOuter,
    /// Every ring is a polygon of its own (`SKIP`).
    Skip,
}

impl OrganizePolygons {
    /// The method GDAL names `name` (`ONLY_CCW`, `DEFAULT`, `CCW_INNER_JUST_AFTER_CW_OUTER` or
    /// `SKIP`), in any letter case.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name.trim().to_ascii_uppercase().as_str() {
            "ONLY_CCW" => Self::OnlyCcw,
            "DEFAULT" => Self::Containment,
            "CCW_INNER_JUST_AFTER_CW_OUTER" => Self::CcwInnerJustAfterCwOuter,
            "SKIP" => Self::Skip,
            _ => return None,
        })
    }
}

/// Options for reading shapefiles.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShapefileOptions {
    /// How the rings of a polygon record make polygons. By default, the method the
    /// `TERRIO_ORGANIZE_POLYGONS` or `OGR_ORGANIZE_POLYGONS` environment variable names, and
    /// otherwise `ONLY_CCW`, as in GDAL.
    pub organize_polygons: Option<OrganizePolygons>,
}

impl ShapefileOptions {
    /// Sets an option by name: `ORGANIZE_POLYGONS`, one of the methods of GDAL's
    /// `OGR_ORGANIZE_POLYGONS` (`ONLY_CCW`, `DEFAULT`, `CCW_INNER_JUST_AFTER_CW_OUTER`, `SKIP`).
    /// Returns `false` for other names.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        if !key.eq_ignore_ascii_case("ORGANIZE_POLYGONS") {
            return Ok(false);
        }
        let method = OrganizePolygons::from_name(value)
            .ok_or_else(|| Error::InvalidArgument(format!("unknown ORGANIZE_POLYGONS method '{value}'")))?;
        self.organize_polygons = Some(method);
        Ok(true)
    }

    /// The method in effect: the option, else the environment's, else `ONLY_CCW`.
    pub(crate) fn method(&self) -> OrganizePolygons {
        self.organize_polygons.or_else(|| method_from_env(|name| std::env::var(name).ok())).unwrap_or_default()
    }
}

/// The method of the first of `TERRIO_ORGANIZE_POLYGONS` and `OGR_ORGANIZE_POLYGONS` that names
/// one, so a variable set to an unknown method does not hide the next.
fn method_from_env(get: impl Fn(&str) -> Option<String>) -> Option<OrganizePolygons> {
    ["TERRIO_ORGANIZE_POLYGONS", "OGR_ORGANIZE_POLYGONS"].iter().find_map(|&name| get(name).and_then(|v| OrganizePolygons::from_name(&v)))
}

/// Reads a shapefile. `path` names the `.shp` file.
pub fn read(path: &str, options: &ShapefileOptions) -> Result<Layer> {
    let base = Path::new(path).with_extension("");
    let shp = std::fs::read(path)?;
    let dbf = sidecar(&base, "dbf").map(std::fs::read).transpose()?;
    let prj = sidecar(&base, "prj").as_deref().and_then(read_prj);
    if prj.is_some() {
        crate::crs::prefetch();
    }
    let cpg = sidecar(&base, "cpg").and_then(|p| std::fs::read_to_string(p).ok());
    let name = base.file_name().and_then(|s| s.to_str()).unwrap_or("layer");
    parse(&shp, dbf.as_deref(), prj.as_deref(), cpg.as_deref(), name, options).map_err(|e| match e {
        Error::InvalidData(m) if m == "not a shapefile" => Error::InvalidData(format!("'{path}' is not a shapefile")),
        e => e,
    })
}

/// Reads up to `max` live features after skipping `skip` live features (`None`: to the end).
/// Deleted DBF slots count toward physical FIDs, but not toward either range bound.
/// Only the selected SHP records and DBF attributes are decoded. Finding their positions scans
/// DBF deletion flags and uses the `.shx` index, or scans SHP headers when the index is absent.
/// Memory holds the SHP offset index, the selected features, DBF/SHP record buffers and up to
/// 64 KiB for scanning short DBF rows. Wide rows retain sparse one-byte flag reads.
/// Every deep page scans the skipped physical prefix: short rows read that prefix in buffered
/// blocks, while wide rows require one flag read/seek per slot. No per-file deletion index is cached.
pub fn read_range(path: &str, skip: usize, max: Option<usize>, options: &ShapefileOptions) -> Result<Layer> {
    use std::io::{Read, Seek, SeekFrom};
    let method = options.method();
    let not_a_shapefile = || Error::InvalidData(format!("'{path}' is not a shapefile"));
    let base = Path::new(path).with_extension("");
    let name = base.file_name().and_then(|s| s.to_str()).unwrap_or("layer");
    let mut shp = std::io::BufReader::new(std::fs::File::open(path)?);
    let shp_len = shp.get_ref().metadata()?.len();
    let mut head = [0u8; 100];
    shp.read_exact(&mut head).map_err(|_| not_a_shapefile())?;
    let mut layer = header_layer(&head, name).map_err(|_| not_a_shapefile())?;
    let prj = sidecar(&base, "prj").as_deref().and_then(read_prj);
    if prj.is_some() {
        crate::crs::prefetch();
    }
    let cpg = sidecar(&base, "cpg").and_then(|p| std::fs::read_to_string(p).ok());

    // Where each record of the .shp starts: from the .shx when it indexes the file, else a scan
    // of the record headers.
    let shx = sidecar(&base, "shx").map(std::fs::read).transpose()?.filter(|x| x.len() >= 100 && (x.len() - 100) % 8 == 0);
    let offsets: Vec<u64> = match &shx {
        Some(x) => x[100..].as_chunks::<8>().0.iter().map(|e| 2 * u64::from(u32::from_be_bytes([e[0], e[1], e[2], e[3]]))).collect(),
        None => {
            let mut found = Vec::new();
            let mut pos = 100u64;
            while pos + 8 <= shp_len {
                found.push(pos);
                shp.seek(SeekFrom::Start(pos + 4))?;
                let mut w = [0u8; 4];
                shp.read_exact(&mut w)?;
                pos = (pos + 8 + 2 * u64::from(u32::from_be_bytes(w))).min(shp_len);
            }
            found
        }
    };

    let (fields, dbf_total, mut dbf) = match sidecar(&base, "dbf") {
        Some(p) => {
            let mut f = std::fs::File::open(p)?;
            let len = f.metadata()?.len();
            let mut first = [0u8; 32];
            f.read_exact(&mut first).map_err(|_| Error::InvalidData("truncated .dbf file".into()))?;
            let header_len = dbf_header_len(&first)?;
            let mut header = first.to_vec();
            header.resize(header_len.max(32).min(len as usize), 0);
            f.read_exact(&mut header[32..]).map_err(|_| Error::InvalidData("truncated .dbf file".into()))?;
            let l = dbf_layout(&header, cpg.as_deref())?;
            let held = (len as usize).saturating_sub(l.header_len) / l.record_len.max(1);
            let total = if l.record_len == 0 { 0 } else { l.count.min(held) };
            // Dense short records benefit from batched reads. Wide records keep sparse flag
            // reads instead of fetching tens of KiB of skipped attributes for each flag.
            let capacity = if l.record_len <= 256 { 64 * 1024 } else { 1 };
            (l.fields.clone(), total, Some((std::io::BufReader::with_capacity(capacity, f), l)))
        }
        None => (Vec::new(), 0, None),
    };
    for f in fields {
        layer.add_field(f);
    }
    let n = if dbf_total == 0 { offsets.len() } else { dbf_total.max(offsets.len()) };
    let width = layer.schema.len();
    let bound = max.map_or(n.saturating_sub(skip), |m| m.min(n.saturating_sub(skip)));
    layer.features.try_reserve_exact(bound).map_err(|_| Error::ResourceLimit("cannot allocate features".into()))?;
    let mut content = Vec::new();
    let mut record = Vec::new();
    let mut skipped = 0;
    let mut dbf_position = dbf.as_ref().map_or(0, |(_, l)| l.header_len);
    for fid in 0..n {
        if max.is_some_and(|m| layer.features.len() >= m) {
            break;
        }
        let mut flag = *b" ";
        if let Some((f, l)) = dbf.as_mut().filter(|_| fid < dbf_total) {
            // Skip within a bounded buffer when records are small, avoiding a seek/read pair
            // per slot. Large strides still seek without reading skipped attribute payloads.
            let position = l.header_len + fid * l.record_len;
            f.seek_relative((position - dbf_position) as i64)?;
            f.read_exact(&mut flag)?;
            dbf_position = position + 1;
            if flag[0] == b'*' {
                continue;
            }
        }
        if skipped < skip {
            skipped += 1;
            continue;
        }
        let attributes = if let Some((f, l)) = dbf.as_mut().filter(|_| fid < dbf_total) {
            // The deletion flag was read above; decode only this selected live record.
            if record.len() != l.record_len {
                record.try_reserve_exact(l.record_len).map_err(|_| Error::ResourceLimit("cannot allocate a .dbf record".into()))?;
                record.resize(l.record_len, 0);
            }
            record[0] = flag[0];
            f.read_exact(&mut record[1..])?;
            dbf_position += record.len() - 1;
            l.record(&record)
        } else {
            // Missing DBF slots are live shapes with null attributes, not deletions.
            vec![FieldValue::Null; width]
        };
        let geometry = match offsets.get(fid) {
            Some(&at) if at + 8 <= shp_len => {
                shp.seek(SeekFrom::Start(at + 4))?;
                let mut w = [0u8; 4];
                shp.read_exact(&mut w)?;
                let len = (2 * u64::from(u32::from_be_bytes(w))).min(shp_len - at - 8) as usize;
                content.clear();
                content.resize(len, 0);
                shp.read_exact(&mut content)?;
                parse_shape(&content, method)
            }
            _ => None,
        };
        layer.push(Feature { fid: fid as u64, geometry, attributes });
    }
    apply_prj(&mut layer, prj.as_deref());
    Ok(layer)
}

/// Reads a shapefile from the contents of its `.shp`, `.dbf`, `.prj` and `.cpg` files. `name`
/// names the layer.
pub fn parse(
    shp: &[u8],
    dbf: Option<&[u8]>,
    prj: Option<&str>,
    cpg: Option<&str>,
    name: &str,
    options: &ShapefileOptions,
) -> Result<Layer> {
    let mut layer = header_layer(shp, name)?;
    let method = options.method();

    let mut geometries = Vec::new();
    let mut pos = 100;
    while pos + 8 <= shp.len() {
        let words = u32::from_be_bytes([shp[pos + 4], shp[pos + 5], shp[pos + 6], shp[pos + 7]]) as usize;
        let end = (pos + 8 + 2 * words).min(shp.len());
        geometries.push(parse_shape(&shp[pos + 8..end], method));
        pos = end;
    }

    let (fields, records) = match dbf {
        Some(d) => parse_dbf(d, cpg)?,
        None => (Vec::new(), Vec::new()),
    };
    for f in fields {
        layer.add_field(f);
    }
    let n = if records.is_empty() { geometries.len() } else { records.len().max(geometries.len()) };
    let mut records = records.into_iter();
    let mut geometries = geometries.into_iter();
    for fid in 0..n {
        let geometry = geometries.next().flatten();
        // Past the DBF's records, empty attributes; a record the DBF marks deleted (`None`) is
        // skipped, and the next features keep their physical FIDs.
        let Some(attributes) = records.next().unwrap_or_else(|| Some(vec![FieldValue::Null; layer.schema.len()])) else {
            continue;
        };
        layer.push(Feature { fid: fid as u64, geometry, attributes });
    }
    apply_prj(&mut layer, prj);
    Ok(layer)
}

/// The layer of a `.shp` header (its first 100 bytes), without fields or features.
pub(crate) fn header_layer(shp: &[u8], name: &str) -> Result<Layer> {
    if shp.len() < 100 || u32::from_be_bytes([shp[0], shp[1], shp[2], shp[3]]) != 9994 {
        return Err(Error::InvalidData("not a shapefile".into()));
    }
    let shape_type = i32::from_le_bytes([shp[32], shp[33], shp[34], shp[35]]);
    let mut layer = Layer::new(name);
    layer.geom_type = match if shape_type == 31 { 0 } else { shape_type % 10 } {
        1 => Some(GeometryType::Point),
        3 => Some(GeometryType::LineString),
        5 => Some(GeometryType::Polygon),
        8 => Some(GeometryType::MultiPoint),
        _ => None,
    };
    Ok(layer)
}

/// Sets a layer's CRS from a `.prj` file, as GDAL's Shapefile driver does (FindBestMatch()):
/// the EPSG CRS the `.prj` is identified as, and otherwise its definition.
///
/// A definition is kept as it is when it has the form of one (`is_crs_definition`), whether terrio
/// reads it or not, or when terrio reads it (`SpatialRef::parse_literal`) and it is not WKT: a
/// PROJ string, PROJJSON. WKT goes by its form alone, since the parser reads its first root and
/// not what follows. Any other text leaves the layer without a CRS, with a warning that quotes it;
/// an empty `.prj` leaves it without, silently, as no `.prj` does. A leading byte-order mark is not
/// part of the definition.
pub(crate) fn apply_prj(layer: &mut Layer, prj: Option<&str>) {
    let Some(prj) = prj
        .map(|s| s.trim_start_matches('\u{feff}').trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '\0' | '\x1a')).trim_start())
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    // Validate WKT's entire form before identifying its EPSG code: identification reads only
    // the first root, so it must not bypass the check for trailing prose or unbalanced text.
    if !(is_crs_definition(prj) || (crs_wkt_opener(prj).is_none() && SpatialRef::parse_literal(prj).is_ok())) {
        let quoted = quote_prj_text(&prj.chars().take(80).collect::<String>());
        crate::warnings::warn(format!(
            "{}.prj holds no CRS definition terrio can read ('{quoted}'): the layer is read without a CRS",
            quote_prj_text(&layer.name)
        ));
        return;
    }
    match SpatialRef::from_wkt(prj).epsg.and_then(|code| SpatialRef::from_epsg(code).ok()) {
        Some(s) => s.apply_to_layer(layer),
        None => {
            SpatialRef::from_wkt(prj).apply_to_layer(layer);
            layer.set_crs_wkt(Some(prj.to_string()));
        }
    }
}

/// Quotes diagnostic text using the whitespace/control policy of [`quoted_as_space`].
fn quote_prj_text(text: &str) -> String {
    text.chars().map(|c| if quoted_as_space(c) { ' ' } else { c }).collect()
}

/// Quote whitespace, control codes and Unicode 15.1 General_Category=Cf format controls
/// as spaces. The Cf ranges were checked against Python's unicodedata table; printable
/// non-ASCII text is preserved, while terminal escapes, bidi controls and invisible tags are not.
fn quoted_as_space(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(c,
            '\u{00AD}' | '\u{0600}'..='\u{0605}' | '\u{061C}' | '\u{06DD}' | '\u{070F}' |
            '\u{0890}'..='\u{0891}' | '\u{08E2}' | '\u{180E}' | '\u{200B}'..='\u{200F}' |
            '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206F}' |
            '\u{FEFF}' | '\u{FFF9}'..='\u{FFFB}' | '\u{110BD}' | '\u{110CD}' |
            '\u{13430}'..='\u{1343F}' | '\u{1BCA0}'..='\u{1BCA3}' | '\u{1D173}'..='\u{1D17A}' |
            '\u{E0001}' | '\u{E0020}'..='\u{E007F}')
}

/// The keywords a CRS's WKT starts with: WKT 1's (OGC 18-010r7 B.8, and ESRI's `VERTCS`) and
/// WKT 2's (OGC 18-010r7, and 12-063r5's `IMAGECRS`).
const CRS_WKT_KEYWORDS: [&str; 24] = [
    "GEOGCS",
    "PROJCS",
    "GEOCCS",
    "VERT_CS",
    "VERTCS",
    "LOCAL_CS",
    "COMPD_CS",
    "FITTED_CS",
    "GEODCRS",
    "GEODETICCRS",
    "GEOGCRS",
    "GEOGRAPHICCRS",
    "PROJCRS",
    "PROJECTEDCRS",
    "DERIVEDPROJCRS",
    "VERTCRS",
    "VERTICALCRS",
    "ENGCRS",
    "ENGINEERINGCRS",
    "PARAMETRICCRS",
    "TIMECRS",
    "IMAGECRS",
    "COMPOUNDCRS",
    "BOUNDCRS",
];

/// The position of the bracket that opens `text`'s first WKT node, when the text before it is a
/// CRS's keyword (`PROJCS[`, `engcrs (`).
fn crs_wkt_opener(text: &str) -> Option<usize> {
    let open = text.find(['[', '('])?;
    CRS_WKT_KEYWORDS.iter().any(|k| k.eq_ignore_ascii_case(text[..open].trim())).then_some(open)
}

/// Whether `text` has the form of a CRS definition, whether terrio reads it or not: WKT whose root
/// is opened by `[` or `(` after a CRS's keyword and has its brackets and its quotes balanced, with
/// nothing after it but another definition, on the next line or after a comma (`,VERTCS[...]`); or
/// a reference (`is_crs_reference`). The roots are taken one after the other, and their brackets
/// counted, not parsed: WKT may nest deeper than a parser recurses.
fn is_crs_definition(text: &str) -> bool {
    let mut text = text.trim();
    loop {
        if !text.contains(['[', '(']) {
            return is_crs_reference(text);
        }
        let Some(open) = crs_wkt_opener(text) else {
            return false;
        };
        let (mut depth, mut quoted, mut end) = (0usize, false, None);
        // Every delimiter is ASCII, which no byte of a UTF-8 sequence of another character is.
        for (i, b) in text.bytes().enumerate().skip(open) {
            match b {
                b'"' => quoted = !quoted,
                b'[' | b'(' if !quoted => depth += 1,
                b']' | b')' if !quoted => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(i + 1);
                        break;
                    }
                }
                _ => {}
            }
        }
        let Some(end) = end else {
            return false;
        };
        let rest = &text[end..];
        let next = rest.trim_start();
        if next.is_empty() {
            return true;
        }
        let separator = &rest[..rest.len() - next.len()];
        text = match next.strip_prefix(',') {
            Some(after) => after.trim_start(),
            None if separator.contains(['\n', '\r']) => next,
            None => return false,
        };
    }
}

/// The authorities of the CRS database of PROJ 9.8.1 (`get_authorities()` of pyproj 3.8.0, which
/// bundles it).
const CRS_AUTHORITIES: [&str; 8] = ["EPSG", "ESRI", "IAU_2015", "IGNF", "NKG", "NRCAN", "OGC", "PROJ"];

/// Whether `text` refers to a CRS of a known authority (`CRS_AUTHORITIES`, in either case): an OGC
/// URN of a CRS (`urn:ogc:def:crs:IGNF::LAMB93`, or a combined one), or a single `AUTHORITY:CODE`
/// (`IGNF:LAMB93`, not `ERROR:404`).
fn is_crs_reference(text: &str) -> bool {
    let authority = |a: &str| CRS_AUTHORITIES.iter().any(|k| k.eq_ignore_ascii_case(a));
    let code = |c: &str| !c.is_empty() && c.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    // The `authority:version:code` of an OGC URN (OGC 07-092r3 7.2), its version possibly empty.
    // Every textual part of a URN is case-insensitive (7.2).
    let reference = |r: &str| {
        let mut f = r.split(':');
        matches!((f.next(), f.next(), f.next(), f.next()), (Some(a), Some(v), Some(c), None) if authority(a) && (v.is_empty() || code(v)) && code(c))
    };
    fn after<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
        text.get(..prefix.len()).filter(|p| p.eq_ignore_ascii_case(prefix)).map(|_| &text[prefix.len()..])
    }
    if let Some(r) = after(text, "urn:ogc:def:crs:") {
        return reference(r);
    }
    // A CRS combined from CRSs, or from a CRS, a CS and a conversion, or from a datum and a CS
    // (07-092r3 7.5.2, 7.5.4, 7.5.5).
    if let Some(parts) = after(text, "urn:ogc:def:crs,") {
        let kinds = ["crs", "cs", "datum", "coordinateOperation"];
        return parts
            .split(',')
            .all(|p| p.split_once(':').is_some_and(|(kind, r)| kinds.iter().any(|k| k.eq_ignore_ascii_case(kind)) && reference(r)));
    }
    text.split_once(':').is_some_and(|(a, c)| authority(a) && code(c))
}

/// Decodes a `.shp` record's content (after its 8-byte record header).
pub(crate) fn decode_shape(rec: &[u8], method: OrganizePolygons) -> Option<Geometry> {
    parse_shape(rec, method)
}

fn parse_shape(rec: &[u8], method: OrganizePolygons) -> Option<Geometry> {
    let i32_at = |o: usize| rec.get(o..o + 4).map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]));
    let f64_at = |o: usize| rec.get(o..o + 8).map(|b| f64::from_le_bytes(b.try_into().unwrap_or([0; 8])));
    let st = i32_at(0)?;
    if st == 31 {
        return None; // MultiPatch
    }
    let (has_z, has_m) = (st / 10 == 1, st / 10 == 1 || st / 10 == 2);
    // M values below -1e38 mean "no data".
    let m_value = |v: Option<f64>| v.filter(|m| *m > -1e38);
    if st % 10 == 1 {
        let (x, y) = (f64_at(4)?, f64_at(12)?);
        let z = if has_z { f64_at(20) } else { None };
        let m = m_value(if has_z {
            f64_at(28)
        } else if has_m {
            f64_at(20)
        } else {
            None
        });
        return Some(Geometry::Point(Coord { x, y, z, m }));
    }
    if !matches!(st % 10, 3 | 5 | 8) {
        return None;
    }
    let multipoint = st % 10 == 8;
    let count = |o: usize| i32_at(o).and_then(|v| usize::try_from(v).ok());
    let (num_parts, num_points, parts_at) = if multipoint { (1, count(36)?, 40) } else { (count(36)?, count(40)?, 44) };
    // Counts the record cannot hold are corrupt (GDAL reads no geometry either).
    if num_parts > rec.len() / 4 || num_points > rec.len() / 16 {
        return None;
    }
    let points_at = parts_at + if multipoint { 0 } else { 4 * num_parts };
    let mut starts: Vec<usize> =
        if multipoint { vec![0] } else { (0..num_parts).map(|i| i32_at(parts_at + 4 * i).map(|v| v as usize)).collect::<Option<_>>()? };
    starts.push(num_points);
    let z_at = points_at + 16 * num_points + 16;
    let m_at = if has_z { z_at + 8 * num_points + 16 } else { z_at };
    let coords: Vec<Coord> = (0..num_points)
        .map(|i| {
            Some(Coord {
                x: f64_at(points_at + 16 * i)?,
                y: f64_at(points_at + 16 * i + 8)?,
                z: if has_z { f64_at(z_at + 8 * i) } else { None },
                m: if has_m { m_value(f64_at(m_at + 8 * i)) } else { None },
            })
        })
        .collect::<Option<_>>()?;
    let parts: Vec<Vec<Coord>> =
        starts.windows(2).filter(|w| w[0] < w[1] && w[1] <= coords.len()).map(|w| coords[w[0]..w[1]].to_vec()).collect();
    if parts.is_empty() {
        return None;
    }
    Some(match st % 10 {
        8 => Geometry::MultiPoint(coords),
        3 if parts.len() == 1 => Geometry::LineString(parts.into_iter().next()?),
        3 => Geometry::MultiLineString(parts),
        _ => organize(parts, method)?,
    })
}

/// Groups a polygon record's rings into polygons by `method`. Since 3.13, GDAL first splits the
/// self-touching rings of a record of several parts, whatever the method (see [`split_parts`]).
fn organize(parts: Vec<Vec<Coord>>, method: OrganizePolygons) -> Option<Geometry> {
    let clockwise = |cs: &[Coord]| Ring::new(cs.to_vec()).signed_area() <= 0.0;
    match method {
        OrganizePolygons::OnlyCcw => Some(organize_rings(parts)),
        OrganizePolygons::Containment => super::polygons_from_rings(split_parts(parts).0),
        // As GDAL, which takes this method only when the first ring is clockwise, and which, when
        // it splits a ring, organizes them as it does for ONLY_CCW.
        OrganizePolygons::CcwInnerJustAfterCwOuter if !parts.first().is_some_and(|cs| clockwise(cs)) || split_parts(parts.clone()).1 => {
            Some(organize_rings(parts))
        }
        OrganizePolygons::CcwInnerJustAfterCwOuter => {
            let mut polygons: Vec<(Ring, Vec<Ring>)> = Vec::new();
            for cs in parts {
                let exterior = clockwise(&cs);
                match polygons.last_mut() {
                    Some((_, holes)) if !exterior => holes.push(open_ring(cs)),
                    _ => polygons.push((open_ring(cs), Vec::new())),
                }
            }
            Some(polygon_or_multi(polygons))
        }
        OrganizePolygons::Skip => Some(polygon_or_multi(split_parts(parts).0.into_iter().map(|cs| (open_ring(cs), Vec::new())).collect())),
    }
}

/// The rings of a record as GDAL's `organizePolygons` takes them since 3.13: in a record of
/// several parts, each ring that touches itself to enclose loops (see [`split_self_touching`]) is
/// replaced by its pieces, after the other rings. Also whether a ring was split.
fn split_parts(parts: Vec<Vec<Coord>>) -> (Vec<Vec<Coord>>, bool) {
    if parts.len() < 2 {
        return (parts, false);
    }
    let mut rings = Vec::with_capacity(parts.len());
    let mut pieces = Vec::new();
    for mut cs in parts {
        if cs.len() > 1 && (cs[0].x, cs[0].y) == (cs[cs.len() - 1].x, cs[cs.len() - 1].y) {
            cs.pop();
        }
        match split_self_touching(cs) {
            (ring, loops) if loops.is_empty() => rings.push(ring),
            (ring, loops) => {
                pieces.push(ring);
                pieces.extend(loops);
            }
        }
    }
    let split = !pieces.is_empty();
    rings.extend(pieces);
    (rings, split)
}

/// One polygon as a polygon, several as a multipolygon.
fn polygon_or_multi(mut polygons: Vec<(Ring, Vec<Ring>)>) -> Geometry {
    if polygons.len() == 1 {
        let (exterior, interiors) = polygons.pop().unwrap_or_default();
        Geometry::Polygon { exterior, interiors }
    } else {
        Geometry::MultiPolygon(polygons)
    }
}

/// Groups rings into polygons: clockwise rings are exteriors, and each counter-clockwise ring is
/// a hole of the smallest exterior containing it.
fn organize_rings(parts: Vec<Vec<Coord>>) -> Geometry {
    // GDAL organizes, and since 3.13 splits self-touching rings, only in records of several parts.
    let several = parts.len() > 1;
    let mut rings: Vec<Ring> = Vec::with_capacity(parts.len());
    let mut split_loops: Vec<Ring> = Vec::new();
    for mut cs in parts {
        if cs.len() > 1 && (cs[0].x, cs[0].y) == (cs[cs.len() - 1].x, cs[cs.len() - 1].y) {
            cs.pop();
        }
        let (ring, loops) = if several { split_self_touching(cs) } else { (cs, Vec::new()) };
        rings.push(Ring::new(ring));
        split_loops.extend(loops.into_iter().map(Ring::new));
    }
    // After the stored rings, where GDAL puts them.
    rings.extend(split_loops);
    let (mut exteriors, mut holes): (Vec<Ring>, Vec<Ring>) = rings.into_iter().partition(|r| r.signed_area() <= 0.0);
    if exteriors.is_empty() {
        // Wrongly wound: take every ring as an exterior.
        exteriors = std::mem::take(&mut holes);
    }
    let mut polygons: Vec<(Ring, Vec<Ring>)> = exteriors.into_iter().map(|r| (r, Vec::new())).collect();
    for h in holes {
        // Tested at the midpoint of the hole's first edge (to the first vertex that differs, as
        // the first may be repeated): a vertex of a hole may lie on the exterior (a hole may touch
        // it), where the even-odd test is unreliable.
        let Some(a) = h.0.first() else { continue };
        let b = h.0.iter().find(|b| (b.x, b.y) != (a.x, a.y)).unwrap_or(a);
        let (x, y) = ((a.x + b.x) / 2.0, (a.y + b.y) / 2.0);
        let owner = polygons
            .iter()
            .enumerate()
            .filter(|(_, (e, _))| ring_contains(e, x, y))
            .min_by(|a, b| a.1.0.signed_area().abs().total_cmp(&b.1.0.signed_area().abs()))
            .map(|(i, _)| i);
        match owner {
            Some(i) => polygons[i].1.push(h),
            None => polygons.push((Ring::new(h.0.into_iter().rev().collect()), Vec::new())),
        }
    }
    polygon_or_multi(polygons)
}

/// Splits off the loops of an Esri ring that touches itself at a vertex to enclose them. Esri
/// accepts such a ring; OGC simple features don't. Since 3.13, GDAL makes it valid in
/// `organizePolygons` when GEOS finds it valid under `ALLOW_SELFTOUCHING_RING_FORMING_HOLE`.
///
/// `cs` is an open ring, without its closing vertex. It is cut into cycles at each vertex it
/// passes twice: the loops between two occurrences of a vertex, and the rest, which holds the
/// stored start. The cycle with the largest area is the outer one, wherever the ring starts; the
/// ring is split when every other cycle has an area, winds the other way round and lies inside
/// it: holes of an exterior, or islands inside a hole part. The outer cycle and the others are
/// returned. Otherwise (a figure-eight, a loop outside, a spike) the ring is returned as it is,
/// with no others, as it is when it never touches itself. Consecutive duplicate points are not
/// loops.
///
/// The rings start where GEOS's polygonizer, which rebuilds them for GDAL, starts them: one vertex
/// after the stored start for the rest when it is the outer cycle, and otherwise one vertex after
/// the vertex a cycle hangs from. When the rest is inside, it comes after the loops.
fn split_self_touching(cs: Vec<Coord>) -> (Vec<Coord>, Vec<Vec<Coord>>) {
    // `+ 0.0` makes -0.0 and 0.0 the same vertex, as GDAL's comparison of doubles does.
    let key = |c: &Coord| ((c.x + 0.0).to_bits(), (c.y + 0.0).to_bits());
    let mut path: Vec<Coord> = Vec::with_capacity(cs.len());
    let mut at: HashMap<(u64, u64), usize> = HashMap::with_capacity(cs.len());
    let mut loops: Vec<Vec<Coord>> = Vec::new();
    for c in &cs {
        match at.get(&key(c)) {
            Some(&i) if i + 1 == path.len() => {}
            Some(&i) => {
                let mut lp = vec![path[i].clone()];
                lp.extend(path.split_off(i + 1));
                for removed in &lp[1..] {
                    at.remove(&key(removed));
                }
                loops.push(lp);
            }
            None => {
                at.insert(key(c), path.len());
                path.push(c.clone());
            }
        }
    }
    if loops.is_empty() {
        return (cs, Vec::new());
    }
    // The rest first, then the loops, each starting at the vertex it hangs from.
    let mut cycles = Vec::with_capacity(loops.len() + 1);
    cycles.push(path);
    cycles.append(&mut loops);
    let areas: Vec<f64> = cycles.iter().map(|c| Ring::new(c.clone()).signed_area()).collect();
    let Some(outer_i) = (0..cycles.len()).max_by(|&a, &b| areas[a].abs().total_cmp(&areas[b].abs())) else {
        return (cs, Vec::new());
    };
    let outer = areas[outer_i];
    let split = {
        let shell = Ring::new(cycles[outer_i].clone());
        // Tested at the midpoint of a cycle's first edge: its first vertex may lie on the outer
        // cycle, and no cycle holds consecutive duplicates, so the edge has a length.
        let inside = |i: usize| {
            let c = &cycles[i];
            c.len() >= 3
                && areas[i] != 0.0
                && (areas[i] > 0.0) != (outer > 0.0)
                && ring_contains(&shell, (c[0].x + c[1].x) / 2.0, (c[0].y + c[1].y) / 2.0)
        };
        outer != 0.0 && cycles[outer_i].len() >= 3 && (0..cycles.len()).filter(|&i| i != outer_i).all(inside)
    };
    if !split {
        return (cs, Vec::new());
    }
    let mut rest = cycles.remove(0);
    for lp in &mut cycles {
        lp.rotate_left(1);
    }
    if outer_i == 0 {
        rest.rotate_left(1);
        return (rest, cycles);
    }
    // The outer cycle is a loop, as when the stored ring starts inside a hole: the rest hangs from
    // the outer cycle's vertex, now its last, and starts one vertex after it.
    let outer = cycles.remove(outer_i - 1);
    if let Some(k) = outer.last().and_then(|t| rest.iter().position(|c| (c.x, c.y) == (t.x, t.y))) {
        rest.rotate_left(k + 1);
    }
    cycles.push(rest);
    (outer, cycles)
}

/// Whether a point is inside a ring (even-odd rule).
pub(crate) fn ring_contains(r: &Ring, x: f64, y: f64) -> bool {
    let cs = &r.0;
    let mut inside = false;
    let mut j = cs.len().wrapping_sub(1);
    for i in 0..cs.len() {
        let (a, b) = (&cs[i], &cs[j]);
        if (a.y > y) != (b.y > y) && x < (b.x - a.x) * (y - a.y) / (b.y - a.y) + a.x {
            inside = !inside;
        }
        j = i;
    }
    inside
}

/// The encoding of a `.dbf` file's text, decided as GDAL's Shapefile driver decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DbfEncoding {
    Utf8,
    /// A single-byte code page: the characters of bytes 0x80 to 0xFF.
    Table(&'static [char; 128]),
    /// A multi-byte code page: 932, 936, 949 or 950.
    Cjk(u16),
    /// GDAL does not recode the text (no `.cpg`, no language driver id, or an encoding it cannot
    /// recode from): its bytes reach the caller as they are, and pyogrio decodes them as
    /// ISO-8859-1.
    Raw,
}

#[path = "shapefile_codepages.rs"]
mod codepages;

#[path = "shapefile_cjk.rs"]
mod cjk;

impl DbfEncoding {
    /// The encoding of a `.dbf` whose `.cpg` holds `cpg` (its first line counts) and whose header
    /// has the language driver id `ldid` (byte 29), as GDAL decides it: the `.cpg`, else the id,
    /// else the `SHAPE_ENCODING` option.
    pub(crate) fn detect(cpg: Option<&str>, ldid: u8) -> Self {
        let cpg = cpg.map(|c| c.split(['\n', '\r']).next().unwrap_or_default()).filter(|c| !c.is_empty());
        let named = match cpg {
            Some(c) => Some(Self::from_code_page(c)),
            None if ldid != 0 => Some(Self::from_ldid(ldid)),
            None => None,
        };
        match named {
            Some(e) => e,
            None => crate::config::get("SHAPE_ENCODING").map_or(Self::Raw, |e| Self::from_code_page(&e)),
        }
    }

    /// GDAL's `OGRShapeLayer::ConvertCodePage` of a `.cpg` value: code page numbers (as `atoi`
    /// reads them) of 437 to 950 and 1250 to 1258 are `CP<n>`, `8859...` is `ISO-8859-...`,
    /// `UTF-8...` is UTF-8, and other values go to iconv as they are.
    fn from_code_page(c: &str) -> Self {
        // ESRI writes `ANSI 1252`, which GDAL reads as the code page number.
        let number = c.strip_prefix("ANSI ").unwrap_or(c);
        let digits: String = number.trim_start().chars().take_while(char::is_ascii_digit).take(9).collect();
        let n: u32 = digits.parse().unwrap_or(0);
        let upper = c.to_ascii_uppercase();
        if (437..=950).contains(&n) || (1250..=1258).contains(&n) {
            return Self::from_iconv_name(&format!("CP{n}"));
        }
        if let Some(rest) = upper.strip_prefix("8859") {
            return Self::from_iconv_name(&format!("ISO-8859-{}", rest.strip_prefix('-').unwrap_or(rest)));
        }
        if upper.starts_with("UTF-8") {
            return Self::Utf8;
        }
        Self::from_iconv_name(upper.trim())
    }

    /// The encoding iconv knows by `name` (upper case), for the encodings terrio decodes;
    /// [`DbfEncoding::Raw`] for the others, which GDAL then leaves unrecoded too when iconv does
    /// not know them.
    fn from_iconv_name(name: &str) -> Self {
        let iso = |n: &str| format!("ISO-8859-{n}");
        let canonical = match name {
            "UTF-8" | "UTF8" => return Self::Utf8,
            // The names glibc's iconv gives these code pages' tables (measured: its SJIS, EUC-KR
            // and GB18030 are other tables; WINDOWS-932, -949 and -950 are unknown to it).
            "CP932" | "WINDOWS-31J" | "MS932" => return Self::Cjk(932),
            "CP936" | "GBK" | "MS936" | "WINDOWS-936" => return Self::Cjk(936),
            "CP949" | "UHC" => return Self::Cjk(949),
            "CP950" | "BIG5" => return Self::Cjk(950),
            "LATIN1" | "L1" | "ISO88591" | "ISO-IR-100" | "CP819" | "IBM819" => iso("1"),
            "LATIN2" | "L2" => iso("2"),
            "LATIN3" | "L3" => iso("3"),
            "LATIN4" | "L4" => iso("4"),
            "CYRILLIC" => iso("5"),
            "ARABIC" => iso("6"),
            "GREEK" => iso("7"),
            "HEBREW" => iso("8"),
            "LATIN5" | "L5" => iso("9"),
            "LATIN6" | "L6" => iso("10"),
            "LATIN7" | "L7" => iso("13"),
            "LATIN8" | "L8" => iso("14"),
            "LATIN9" | "L9" => iso("15"),
            "LATIN10" | "L10" => iso("16"),
            "MS-ANSI" => "CP1252".into(),
            "MS-EE" => "CP1250".into(),
            "MS-CYRL" => "CP1251".into(),
            "MS-GREEK" => "CP1253".into(),
            "MS-TURK" => "CP1254".into(),
            "MS-HEBR" => "CP1255".into(),
            "MS-ARAB" => "CP1256".into(),
            "WINBALTRIM" => "CP1257".into(),
            n => {
                if let Some(cp) = n.strip_prefix("WINDOWS-").or_else(|| n.strip_prefix("IBM")) {
                    format!("CP{cp}")
                } else if let Some(rest) = n.strip_prefix("ISO8859-").or_else(|| n.strip_prefix("ISO_8859-")) {
                    iso(rest)
                } else {
                    n.to_string()
                }
            }
        };
        codepages::table(&canonical).map_or(Self::Raw, Self::Table)
    }

    /// GDAL's code page of a DBF language driver id (`LDID/<n>`).
    fn from_ldid(ldid: u8) -> Self {
        let cp = match ldid {
            87 => return Self::from_iconv_name("ISO-8859-1"),
            1 | 11 | 13 | 15 | 18 | 21 | 24 | 25 | 27 => 437,
            2 | 10 | 14 | 16 | 20 | 22 | 26 | 29 | 37 | 55 => 850,
            3 | 88 | 89 => 1252,
            8 | 17 | 23 | 102 => 865,
            19 | 123 => 932,
            28 | 108 => 863,
            31 | 34 | 35 | 64 | 100 | 135 => 852,
            36 => 860,
            38 | 101 => 866,
            77 | 122 => 936,
            78 | 121 => 949,
            79 | 120 => 950,
            80 | 124 => 874,
            103 => 861,
            106 | 134 => 737,
            107 | 136 => 857,
            200 => 1250,
            201 => 1251,
            202 => 1254,
            203 => 1253,
            204 => 1257,
            _ => return Self::Raw,
        };
        Self::from_iconv_name(&format!("CP{cp}"))
    }

    /// Decodes text in this encoding. Invalid UTF-8 is replaced; bytes a code page leaves
    /// undefined are skipped, as iconv's recoding in GDAL skips them.
    pub(crate) fn decode(self, bytes: &[u8]) -> String {
        match self {
            Self::Utf8 => String::from_utf8_lossy(bytes).into_owned(),
            Self::Raw => bytes.iter().map(|&b| b as char).collect(),
            Self::Table(t) => bytes
                .iter()
                .map(|&b| if b < 0x80 { b as char } else { t[usize::from(b - 0x80)] })
                .filter(|&c| c != codepages::UNDEFINED)
                .collect(),
            Self::Cjk(cp) => decode_cjk(cp, bytes),
        }
    }
}

/// Decodes a multi-byte code page as GDAL's recoding (glibc's iconv) does: `encoding_rs`'s
/// decoder where the two agree, glibc's differences from [`cjk`], and iconv's error handling in
/// GDAL: a byte that starts no character is skipped, and a character cut by the end of the text
/// ends it.
fn decode_cjk(cp: u16, bytes: &[u8]) -> String {
    use std::cmp::Ordering;
    let (encoding, invalid, chars, swallowed) = match cp {
        932 => (encoding_rs::SHIFT_JIS, cjk::INVALID_932, cjk::CHARS_932, cjk::SWALLOWED_932),
        936 => (encoding_rs::GBK, cjk::INVALID_936, cjk::CHARS_936, cjk::SWALLOWED_936),
        949 => (encoding_rs::EUC_KR, cjk::INVALID_949, cjk::CHARS_949, cjk::SWALLOWED_949),
        _ => (encoding_rs::BIG5, cjk::INVALID_950, cjk::CHARS_950, cjk::SWALLOWED_950),
    };
    // The character glibc decodes from `seq` (one byte, or a lead byte and a trail byte), whose
    // code is `code`.
    let glibc = |code: u16, seq: &[u8]| -> Option<char> {
        let rejected = invalid.binary_search_by(|&(a, b)| {
            if code < a {
                Ordering::Greater
            } else if code > b {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        });
        if rejected.is_ok() {
            return None;
        }
        if let Ok(i) = chars.binary_search_by_key(&code, |&(c, _)| c) {
            return Some(chars[i].1);
        }
        let text = encoding.decode_without_bom_handling_and_without_replacement(seq)?;
        let mut it = text.chars();
        match (it.next(), it.next()) {
            (Some(c), None) => Some(c),
            _ => None,
        }
    };
    let mut out = String::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x80 {
            out.push(char::from(b));
            i += 1;
        } else if let Some(c) = glibc(u16::from(b), &bytes[i..=i]) {
            out.push(c);
            i += 1;
        } else if let Some(&t) = bytes.get(i + 1) {
            let code = u16::from_be_bytes([b, t]);
            if swallowed.contains(&code) {
                // glibc consumes the two bytes, writes nothing and reports an error: GDAL skips
                // one more byte.
                i += 3;
                continue;
            }
            match glibc(code, &bytes[i..i + 2]) {
                Some(c) => {
                    out.push(c);
                    i += 2;
                }
                None => i += 1,
            }
        } else {
            break;
        }
    }
    out
}

/// The fields and record layout of a `.dbf` file.
pub(crate) struct DbfLayout {
    /// The fields.
    pub fields: Vec<FieldDef>,
    layout: Vec<(usize, usize, u8)>,
    encoding: DbfEncoding,
    /// The number of records the header declares.
    pub count: usize,
    /// The length of the header, where the records start.
    pub header_len: usize,
    /// The length of a record.
    pub record_len: usize,
}

/// The length of a `.dbf` header from its first 32 bytes.
pub(crate) fn dbf_header_len(d: &[u8]) -> Result<usize> {
    if d.len() < 32 {
        return Err(Error::InvalidData("truncated .dbf file".into()));
    }
    Ok(u16::from_le_bytes([d[8], d[9]]) as usize)
}

/// Reads a `.dbf` header (`d` holds at least the header).
pub(crate) fn dbf_layout(d: &[u8], cpg: Option<&str>) -> Result<DbfLayout> {
    let header_len = dbf_header_len(d)?;
    let encoding = DbfEncoding::detect(cpg, d[29]);
    let count = u32::from_le_bytes([d[4], d[5], d[6], d[7]]) as usize;
    let record_len = u16::from_le_bytes([d[10], d[11]]) as usize;
    let mut fields = Vec::new();
    let mut layout = Vec::new();
    let mut offset = 1;
    let mut pos = 32;
    while pos + 32 <= d.len().min(header_len) && d[pos] != 0x0D {
        let raw = &d[pos..pos + 11];
        let name = encoding.decode(&raw[..raw.iter().position(|&b| b == 0).unwrap_or(11)]).trim().to_string();
        let kind = d[pos + 11];
        let width = d[pos + 16] as usize;
        let decimals = d[pos + 17] as usize;
        // As shapelib and GDAL type them: numbers without decimals are integers below 19 digits.
        let t = match kind {
            b'N' | b'F' if decimals == 0 && width < 19 => FieldType::Integer,
            b'N' | b'F' | b'O' => FieldType::Float,
            b'I' => FieldType::Integer,
            b'L' => FieldType::Boolean,
            b'D' => FieldType::Date,
            _ => FieldType::Text,
        };
        let mut def = FieldDef::new(name, t).width(width);
        if t == FieldType::Float {
            def = def.precision(decimals);
        }
        fields.push(def);
        layout.push((offset, width, kind));
        offset += width;
        pos += 32;
    }
    Ok(DbfLayout { fields, layout, encoding, count, header_len, record_len })
}

impl DbfLayout {
    /// Decodes a record, with shapelib's rules for null values and blanks as GDAL builds it
    /// (`TRIM_DBF_WHITESPACE`: text loses its leading and trailing spaces).
    pub(crate) fn record(&self, rec: &[u8]) -> Vec<FieldValue> {
        let encoding = self.encoding;
        self.layout
            .iter()
            .zip(&self.fields)
            .map(|(&(off, width, kind), def)| {
                let raw = rec.get(off..(off + width).min(rec.len())).unwrap_or_default();
                if kind == b'I' && raw.len() == 4 {
                    return FieldValue::Integer(i32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as i64);
                }
                let raw = raw.split(|&b| b == 0).next().unwrap_or_default();
                let start = raw.iter().position(|&b| b != b' ').unwrap_or(raw.len());
                let end = raw.iter().rposition(|&b| b != b' ').map_or(start, |e| e + 1);
                let v = &raw[start..end];
                let null = match kind {
                    b'N' | b'F' => v.first() == Some(&b'*') || v.is_empty(),
                    b'D' => v.is_empty() || v.starts_with(b"00000000") || v == b"0",
                    b'L' => v.first() == Some(&b'?'),
                    _ => v.is_empty(),
                };
                if null {
                    return FieldValue::Null;
                }
                let ascii = || std::str::from_utf8(v).unwrap_or_default();
                match def.field_type {
                    FieldType::Integer => ascii().parse().map(FieldValue::Integer).unwrap_or(FieldValue::Null),
                    FieldType::Float => ascii().parse().map(FieldValue::Float).unwrap_or(FieldValue::Null),
                    FieldType::Boolean => FieldValue::Boolean(matches!(v.first(), Some(b'T' | b't' | b'Y' | b'y'))),
                    FieldType::Date if v.len() == 8 && v.iter().all(u8::is_ascii_digit) => {
                        let v = ascii();
                        FieldValue::Date(format!("{}-{}-{}", &v[..4], &v[4..6], &v[6..]))
                    }
                    FieldType::Date => FieldValue::Null,
                    _ => FieldValue::Text(encoding.decode(v)),
                }
            })
            .collect()
    }
}

/// A physical DBF slot; None marks a deleted record.
type DbfRecord = Option<Vec<FieldValue>>;

/// Parses a `.dbf` file into field definitions and records.
fn parse_dbf(d: &[u8], cpg: Option<&str>) -> Result<(Vec<FieldDef>, Vec<DbfRecord>)> {
    let l = dbf_layout(d, cpg)?;
    let records = read_dbf_records(d, &l, |rec| l.record(rec))?;
    Ok((l.fields, records))
}

/// Collect physical slots, invoking the record factory only for live records.
fn read_dbf_records(d: &[u8], l: &DbfLayout, make_record: impl Fn(&[u8]) -> Vec<FieldValue> + Sync) -> Result<Vec<DbfRecord>> {
    // The header's record count is not trusted: only the records the file holds are read, and
    // their list is allocated fallibly.
    let held = d.len().saturating_sub(l.header_len) / l.record_len.max(1);
    let count = if l.record_len == 0 { 0 } else { l.count.min(held) };
    let mut records: Vec<DbfRecord> = Vec::new();
    records
        .try_reserve_exact(count)
        .map_err(|_| Error::ResourceLimit(format!("the .dbf file declares {count} records, more than memory holds")))?;
    // Keep per-record vectors, decoded strings and dates on the caller's allocator arena.
    // Returning pool-thread allocations here strands freed storage in those arenas when the
    // caller drops the dataset and allocates its next working buffers (glibc; see #729).
    // Every record below `count` is whole in the file.
    let record = |i: usize| {
        let start = l.header_len + i * l.record_len;
        (d[start] != b'*').then(|| make_record(&d[start..start + l.record_len]))
    };
    #[cfg(any(target_arch = "wasm32", all(target_os = "linux", target_env = "gnu")))]
    records.extend((0..count).map(record));
    #[cfg(not(any(target_arch = "wasm32", all(target_os = "linux", target_env = "gnu"))))]
    crate::par::install(|| {
        use rayon::prelude::*;
        #[allow(clippy::disallowed_methods)] // On terrio's pool, inside `par::install`.
        records.par_extend((0..count).into_par_iter().map(record));
    });
    Ok(records)
}

/// Writes a layer as a shapefile. `path` names the `.shp` file; the other files are written next
/// to it.
pub fn write(layer: &Layer, path: &str) -> Result<()> {
    let base = Path::new(path).with_extension("");
    let (shp, shx) = encode_shapes(layer)?;
    std::fs::write(base.with_extension("shp"), shp)?;
    std::fs::write(base.with_extension("shx"), shx)?;
    std::fs::write(base.with_extension("dbf"), encode_dbf(layer)?)?;
    std::fs::write(base.with_extension("cpg"), "UTF-8")?;
    // The source definition when there is one; GDAL reads OGC WKT in `.prj` files too.
    let prj = super::output_wkt(layer);
    let prj_path = base.with_extension("prj");
    match prj {
        Some(wkt) => std::fs::write(prj_path, wkt)?,
        None if prj_path.exists() => std::fs::remove_file(prj_path)?,
        None => {}
    }
    Ok(())
}

/// Writes one layer to a ZIP containing `.shp`, `.shx`, `.dbf`, `.cpg` and,
/// when present, `.prj`. Pure Rust on native and WebAssembly, without filesystem I/O.
/// Save the returned bytes as `{stem}.shp.zip`, MIME type `application/zip`.
///
/// `stem` must be a single nonempty filename stem. Point/MultiPoint, line and
/// polygon (including multipart) geometries support XY and Z; M is refused.
/// Fields support text, integer, finite real, boolean, date and null values. DBF
/// names are laundered to ten bytes; reals use width 24 and 15 decimal places,
/// rounding to that precision (tiny magnitudes can become zero). Reals that do not
/// fit that fixed layout are refused; convert them to text explicitly.
/// Text over 254 UTF-8 bytes, empty/trailing-space/NUL text, integers requiring
/// over 18 decimal columns, and non-scalar/blob/datetime fields are refused rather
/// than silently losing data. Convert such fields explicitly before writing.
pub fn to_zip_bytes(layer: &Layer, stem: &str) -> Result<Vec<u8>> {
    use std::io::{Cursor, Write};
    if stem.is_empty() || matches!(stem, "." | "..") || stem.contains(['/', '\\', '\0', ':']) {
        return Err(Error::InvalidArgument("Shapefile archive stem must be a single nonempty filename stem".into()));
    }
    let profile = LayerProfile::of(layer);
    if profile.geometry.has_m {
        return Err(Error::Unsupported("Shapefile ZIP writing does not support M coordinates".into()));
    }
    if layer.features.iter().any(|f| f.attributes.len() > layer.schema.len()) {
        return Err(Error::InvalidArgument("Shapefile feature has more attributes than its schema".into()));
    }
    let fields = dbf_fields(layer, &profile);
    for (i, field) in layer.schema.fields().iter().enumerate() {
        let refuse = |reason: &str| Error::Unsupported(format!("Shapefile archive field '{}': {reason}", field.name));
        if !matches!(field.field_type, FieldType::Text | FieldType::Integer | FieldType::Float | FieldType::Boolean | FieldType::Date) {
            return Err(refuse("supports text, integer, finite real, boolean and date scalars only; convert explicitly"));
        }
        if field.field_type == FieldType::Integer && fields[i].width > 18 {
            return Err(refuse("integers requiring more than 18 decimal columns must be converted to text"));
        }
        for feature in &layer.features {
            let value = feature.attributes.get(i).unwrap_or(&FieldValue::Null);
            let valid = match (field.field_type, value) {
                (_, FieldValue::Null) => true,
                (FieldType::Text, FieldValue::Text(text)) => {
                    if text.len() > 254 {
                        return Err(refuse("text exceeds the DBF limit of 254 UTF-8 bytes; use a sidecar or another format"));
                    }
                    if text.is_empty() || text.ends_with(' ') || text.contains('\0') {
                        return Err(refuse("empty, trailing-space or NUL text cannot round-trip through DBF"));
                    }
                    true
                }
                (FieldType::Integer, FieldValue::Integer(_)) | (FieldType::Boolean, FieldValue::Boolean(_)) => true,
                (FieldType::Float, FieldValue::Float(x)) => {
                    if x.is_finite() && format!("{x:.15}").len() > 24 {
                        return Err(refuse("real exceeds the DBF width of 24 columns at 15 decimal places; convert to text"));
                    }
                    x.is_finite()
                }
                (FieldType::Date, FieldValue::Date(text)) => super::datetime::parse_date(text).is_some(),
                _ => false,
            };
            if !valid {
                return Err(refuse("value does not match its scalar field type or is not finite/a valid date"));
            }
        }
    }
    let (shp, shx) = encode_shapes(layer)?;
    let mut members = vec![("shp", shp), ("shx", shx), ("dbf", encode_dbf(layer)?), ("cpg", b"UTF-8".to_vec())];
    if let Some(wkt) = super::output_wkt(layer) {
        members.push(("prj", wkt.into_bytes()));
    }
    members.sort_by_key(|(ext, _)| *ext);
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (ext, bytes) in members {
        zip.start_file(format!("{stem}.{ext}"), opts)?;
        zip.write_all(&bytes)?;
    }
    Ok(zip.finish()?.into_inner())
}

/// The shape type for the layer's geometries, as GDAL chooses it.
fn shape_type(layer: &Layer, profile: &LayerProfile) -> Result<i32> {
    let has_z = profile.geometry.has_z;
    let base = match profile.features_type() {
        None if profile.no_geometry() => return Ok(NULL_SHAPE),
        Some(GeometryType::Point) => 1,
        Some(GeometryType::MultiPoint) => 8,
        Some(GeometryType::LineString | GeometryType::MultiLineString) => 3,
        Some(GeometryType::Polygon | GeometryType::MultiPolygon) => 5,
        _ => {
            return Err(Error::Unsupported(format!(
                "a shapefile holds one geometry type, and layer '{}' mixes types or has collections",
                layer.name
            )));
        }
    };
    Ok(if has_z { base + 10 } else { base })
}

#[derive(Clone, Copy)]
pub(crate) struct Bounds([f64; 6]);

impl Bounds {
    pub(crate) fn empty() -> Self {
        Self([f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY, f64::INFINITY, f64::NEG_INFINITY])
    }
    fn add(&mut self, c: &Coord) {
        let b = &mut self.0;
        b[0] = b[0].min(c.x);
        b[1] = b[1].min(c.y);
        b[2] = b[2].max(c.x);
        b[3] = b[3].max(c.y);
        let z = c.z.unwrap_or(0.0);
        b[4] = b[4].min(z);
        b[5] = b[5].max(z);
    }
    fn or_zero(self) -> [f64; 6] {
        if self.0[0].is_finite() { self.0 } else { [0.0; 6] }
    }
}

/// Encodes the `.shp` and `.shx` files.
fn encode_shapes(layer: &Layer) -> Result<(Vec<u8>, Vec<u8>)> {
    let enc = ShpEncoder::new(layer, &LayerProfile::of(layer))?;
    let mut total = Bounds::empty();
    let mut shp = vec![0u8; 100];
    let mut shx = vec![0u8; 100];
    for (i, f) in layer.features.iter().enumerate() {
        let (rec, idx) = enc.record(i as u64, f, shp.len() as u64, &mut total)?;
        shx.extend_from_slice(&idx);
        shp.extend_from_slice(&rec);
    }
    let (shp_len, shx_len) = (shp.len() as u64, shx.len() as u64);
    shp[..100].copy_from_slice(&enc.header(shp_len, &total));
    shx[..100].copy_from_slice(&enc.header(shx_len, &total));
    Ok((shp, shx))
}

/// Encodes a layer's features as shapefile records (`.shp`, `.shx` and `.dbf`), with the shape
/// type and field widths its profile decides.
pub(crate) struct ShpEncoder {
    st: i32,
    has_z: bool,
    fields: Vec<DbfField>,
}

impl ShpEncoder {
    /// An encoder for the features of `layer` (its fields), which `profile` describes.
    pub(crate) fn new(layer: &Layer, profile: &LayerProfile) -> Result<Self> {
        let st = shape_type(layer, profile)?;
        let fields = dbf_fields(layer, profile);
        let record_len = 1 + fields.iter().map(|f| f.width).sum::<usize>();
        let header_len = 32 + 32 * fields.len() + 1;
        if record_len > 65535 || header_len > 65535 {
            return Err(Error::Unsupported("too many fields for a shapefile".into()));
        }
        Ok(Self { st, has_z: st > 10, fields })
    }

    /// The `.shp` record of the `i`-th feature (from 0), written at byte `offset` of the file,
    /// with its `.shx` entry. Adds the feature's coordinates to `total`.
    pub(crate) fn record(&self, i: u64, f: &Feature, offset: u64, total: &mut Bounds) -> Result<(Vec<u8>, [u8; 8])> {
        let rec = match &f.geometry {
            Some(g) => encode_shape(g, self.st, self.has_z, total),
            None => NULL_SHAPE.to_le_bytes().to_vec(),
        };
        if (offset as usize + 8 + rec.len()) / 2 > i32::MAX as usize {
            return Err(Error::Unsupported("the shapefile would be larger than the format's 4 GB limit".into()));
        }
        let mut idx = [0u8; 8];
        idx[..4].copy_from_slice(&((offset / 2) as u32).to_be_bytes());
        idx[4..].copy_from_slice(&((rec.len() / 2) as u32).to_be_bytes());
        let mut out = Vec::with_capacity(rec.len() + 8);
        out.extend_from_slice(&((i + 1) as u32).to_be_bytes());
        out.extend_from_slice(&((rec.len() / 2) as u32).to_be_bytes());
        out.extend_from_slice(&rec);
        Ok((out, idx))
    }

    /// The 100-byte header of a `.shp` or `.shx` file of `len` bytes whose shapes are within
    /// `total`.
    pub(crate) fn header(&self, len: u64, total: &Bounds) -> [u8; 100] {
        let b = total.or_zero();
        let mut buf = [0u8; 100];
        buf[0..4].copy_from_slice(&9994u32.to_be_bytes());
        buf[24..28].copy_from_slice(&((len / 2) as u32).to_be_bytes());
        buf[28..32].copy_from_slice(&1000u32.to_le_bytes());
        buf[32..36].copy_from_slice(&self.st.to_le_bytes());
        for (k, v) in [b[0], b[1], b[2], b[3], b[4], b[5], 0.0, 0.0].iter().enumerate() {
            buf[36 + 8 * k..44 + 8 * k].copy_from_slice(&v.to_le_bytes());
        }
        buf
    }

    /// The `.dbf` header of a file of `count` records.
    pub(crate) fn dbf_header(&self, count: u64) -> Vec<u8> {
        let fields = &self.fields;
        let record_len = 1 + fields.iter().map(|f| f.width).sum::<usize>();
        let header_len = 32 + 32 * fields.len() + 1;
        let mut out = Vec::with_capacity(header_len);
        out.push(0x03);
        out.extend_from_slice(&[125, 1, 1]); // last update (2025-01-01): a fixed date keeps outputs reproducible
        out.extend_from_slice(&(count as u32).to_le_bytes());
        out.extend_from_slice(&(header_len as u16).to_le_bytes());
        out.extend_from_slice(&(record_len as u16).to_le_bytes());
        out.extend_from_slice(&[0u8; 20]);
        for f in fields {
            let mut name = [0u8; 11];
            name[..f.name.len()].copy_from_slice(f.name.as_bytes());
            out.extend_from_slice(&name);
            out.push(f.kind);
            out.extend_from_slice(&[0u8; 4]);
            out.push(f.width as u8);
            out.push(f.decimals as u8);
            out.extend_from_slice(&[0u8; 14]);
        }
        out.push(0x0D);
        out
    }

    /// Appends a feature's `.dbf` record.
    pub(crate) fn dbf_record(&self, feat: &Feature, out: &mut Vec<u8>) {
        out.push(b' ');
        for (i, f) in self.fields.iter().enumerate() {
            let v = feat.attributes.get(i).unwrap_or(&FieldValue::Null);
            let cell: Vec<u8> = match (f.kind, v) {
                (b'N', FieldValue::Null) => vec![b'*'; f.width],
                (b'D', FieldValue::Null) => vec![b'0'; f.width],
                (b'L', FieldValue::Null) => vec![b'?'],
                (_, FieldValue::Null) => vec![b' '; f.width],
                (b'N', v) => {
                    let s = if f.decimals == 0 {
                        v.as_i64().or_else(|| v.as_f64().map(|x| x.round() as i64)).map(|x| x.to_string()).unwrap_or_default()
                    } else {
                        v.as_f64().map(|x| format_real(x, f.width, f.decimals)).unwrap_or_default()
                    };
                    format!("{s:>w$}", w = f.width).into_bytes()
                }
                (b'L', v) => vec![if v.as_bool().unwrap_or(false) { b'T' } else { b'F' }],
                (b'D', v) => {
                    let digits: String = text(v).chars().filter(char::is_ascii_digit).take(8).collect();
                    format!("{digits:0<8}").into_bytes()
                }
                (_, v) => {
                    let s = text(v);
                    let mut end = s.len().min(f.width);
                    while !s.is_char_boundary(end) {
                        end -= 1;
                    }
                    let mut b = s.as_bytes()[..end].to_vec();
                    b.resize(f.width, b' ');
                    b
                }
            };
            out.extend_from_slice(&cell[..f.width.min(cell.len())]);
            out.extend(std::iter::repeat_n(b' ', f.width.saturating_sub(cell.len())));
        }
    }
}

/// Closes a ring and orients it: clockwise for exterior rings, counter-clockwise for holes.
fn oriented(r: &Ring, exterior: bool) -> Vec<Coord> {
    let mut cs = r.0.clone();
    if cs.len() > 1 && (cs[0].x, cs[0].y) != (cs[cs.len() - 1].x, cs[cs.len() - 1].y) {
        cs.push(cs[0].clone());
    }
    let clockwise = Ring::new(cs.clone()).signed_area() < 0.0;
    if clockwise != exterior {
        cs.reverse();
    }
    cs
}

fn encode_shape(g: &Geometry, st: i32, has_z: bool, total: &mut Bounds) -> Vec<u8> {
    let parts: Vec<Vec<Coord>> = match g {
        Geometry::Point(c) => vec![vec![c.clone()]],
        Geometry::MultiPoint(cs) => vec![cs.clone()],
        Geometry::LineString(cs) => vec![cs.clone()],
        Geometry::MultiLineString(ls) => ls.clone(),
        Geometry::Polygon { exterior, interiors } => {
            std::iter::once(oriented(exterior, true)).chain(interiors.iter().map(|r| oriented(r, false))).collect()
        }
        Geometry::MultiPolygon(ps) => {
            ps.iter().flat_map(|(e, hs)| std::iter::once(oriented(e, true)).chain(hs.iter().map(|r| oriented(r, false)))).collect()
        }
        Geometry::GeometryCollection(_) => Vec::new(),
    };
    let parts: Vec<Vec<Coord>> = parts.into_iter().filter(|p| !p.is_empty()).collect();
    if parts.is_empty() {
        return NULL_SHAPE.to_le_bytes().to_vec();
    }
    let mut out = st.to_le_bytes().to_vec();
    let mut b = Bounds::empty();
    for c in parts.iter().flatten() {
        b.add(c);
        total.add(c);
    }
    let put = |out: &mut Vec<u8>, v: f64| out.extend_from_slice(&v.to_le_bytes());
    let measure = |c: &Coord| c.m.filter(|m| m.is_finite() && *m > -1e38);
    if st % 10 == 1 {
        let c = &parts[0][0];
        put(&mut out, c.x);
        put(&mut out, c.y);
        if has_z {
            put(&mut out, c.z.unwrap_or(0.0));
            if let Some(m) = measure(c) {
                put(&mut out, m);
            }
        }
        return out;
    }
    let b = b.0;
    for v in &b[..4] {
        put(&mut out, *v);
    }
    let points: Vec<&Coord> = parts.iter().flatten().collect();
    let multipoint = st % 10 == 8;
    if !multipoint {
        out.extend_from_slice(&(parts.len() as i32).to_le_bytes());
    }
    out.extend_from_slice(&(points.len() as i32).to_le_bytes());
    if !multipoint {
        let mut start = 0i32;
        for p in &parts {
            out.extend_from_slice(&start.to_le_bytes());
            start += p.len() as i32;
        }
    }
    for c in &points {
        put(&mut out, c.x);
        put(&mut out, c.y);
    }
    if has_z {
        put(&mut out, b[4]);
        put(&mut out, b[5]);
        for c in &points {
            put(&mut out, c.z.unwrap_or(0.0));
        }
        let ms: Vec<f64> = points.iter().filter_map(|c| measure(c)).collect();
        // Z-only records omit the optional M section, as GDAL does. When real
        // measures exist, missing samples do not participate in the M range.
        if !ms.is_empty() {
            put(&mut out, ms.iter().copied().fold(f64::INFINITY, f64::min));
            put(&mut out, ms.iter().copied().fold(f64::NEG_INFINITY, f64::max));
            for c in points {
                put(&mut out, measure(c).unwrap_or(-1e39));
            }
        }
    }
    out
}

/// Field names as `ogr2ogr` writes them into a shapefile, unique ignoring case.
///
/// First `ogr2ogr`'s renaming: a name already taken (ignoring case) gets the first number suffix,
/// from 2, that makes it unique among the names written and the source's names (`NAME` after
/// `name` becomes `NAME2`, or `NAME3` when the source has a `name2`; the next `Name`, `Name4`).
/// Then the Shapefile driver's: at most 10 bytes, trailing spaces trimmed and `:` replaced by
/// `_`, and a name taken by then made unique as `populati_1` to `populati_9`, then
/// `populati10` to `populati99`.
pub(crate) fn dbf_field_names(names: &[String]) -> Vec<String> {
    let truncate = |s: &str, n: usize| -> String {
        let mut end = s.len().min(n);
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s[..end].to_string()
    };
    let upper = |s: &str| s.to_ascii_uppercase();
    let sources: std::collections::HashSet<String> = names.iter().map(|n| upper(n)).collect();
    let mut taken: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut last_suffix: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    let mut out: Vec<String> = Vec::new();
    for name in names {
        // ogr2ogr: a numbered name, when the name is taken.
        let mut name = name.clone();
        let radix = upper(&name);
        if taken.contains(&radix) {
            let mut n = last_suffix.get(&radix).copied().unwrap_or(1);
            loop {
                n += 1;
                let candidate = format!("{radix}{n}");
                if !taken.contains(&candidate) && !sources.contains(&candidate) {
                    name = format!("{name}{n}");
                    last_suffix.insert(radix, n);
                    break;
                }
            }
        }
        // The driver: truncated, trimmed and normalized, then made unique.
        let radix = truncate(&name, 10).trim_end_matches(|c: char| c.is_ascii_whitespace()).replace(':', "_");
        let mut candidate = radix.clone();
        let mut n = 1;
        while taken.contains(&upper(&candidate)) && n < 10 {
            candidate = format!("{}_{n}", truncate(&radix, 8));
            n += 1;
        }
        while taken.contains(&upper(&candidate)) && n < 100 {
            candidate = format!("{}{n:02}", truncate(&radix, 8));
            n += 1;
        }
        taken.insert(upper(&candidate));
        out.push(candidate);
    }
    out
}

struct DbfField {
    name: String,
    kind: u8,
    width: usize,
    decimals: usize,
}

fn dbf_fields(layer: &Layer, profile: &LayerProfile) -> Vec<DbfField> {
    let names: Vec<String> = layer.schema.fields().iter().map(|f| f.name.clone()).collect();
    let names = dbf_field_names(&names);
    layer
        .schema
        .fields()
        .iter()
        .enumerate()
        .zip(names)
        .map(|((i, f), name)| {
            let p = profile.fields.get(i).cloned().unwrap_or_default();
            let (kind, width, decimals) = match f.field_type {
                FieldType::Integer => {
                    let longest = p.longest_int;
                    (b'N', if longest <= 9 { 9 } else { longest.max(18) }, 0)
                }
                FieldType::Float => (b'N', 24, 15),
                FieldType::Boolean => (b'L', 1, 0),
                FieldType::Date => (b'D', 8, 0),
                _ => {
                    let longest = p.longest_text;
                    let declared = if f.width > 0 { f.width } else { 80 };
                    (b'C', declared.max(longest).clamp(1, 254), 0)
                }
            };
            DbfField { name, kind, width, decimals }
        })
        .collect()
}

fn text(v: &FieldValue) -> String {
    match v {
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => s.clone(),
        FieldValue::Integer(x) => x.to_string(),
        FieldValue::Float(x) => x.to_string(),
        FieldValue::Boolean(b) => b.to_string(),
        FieldValue::Blob(b) => b.iter().map(|x| format!("{x:02X}")).collect(),
        FieldValue::Null => String::new(),
    }
}

/// A real number in a numeric field: 15 decimals when they fit, fewer or an exponent otherwise.
fn format_real(v: f64, width: usize, decimals: usize) -> String {
    for d in (0..=decimals).rev() {
        let s = format!("{v:.d$}");
        if s.len() <= width {
            return s;
        }
    }
    let s = format!("{v:.e$e}", e = width.saturating_sub(8).min(15));
    s.chars().take(width).collect()
}

/// Encodes the `.dbf` file.
fn encode_dbf(layer: &Layer) -> Result<Vec<u8>> {
    let enc = ShpEncoder::new(layer, &LayerProfile::of(layer))?;
    let mut out = enc.dbf_header(layer.len() as u64);
    for feat in &layer.features {
        enc.dbf_record(feat, &mut out);
    }
    out.push(0x1A);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{Feature, FieldDef};

    /// glibc record storage is built on the calling thread; deleted slots are skipped.
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    #[test]
    fn dbf_record_storage_is_built_on_the_calling_thread() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let mut layer = Layer::new("records");
        layer.add_field(FieldDef::new("name", FieldType::Text));
        layer.add_field(FieldDef::new("day", FieldType::Date));
        for i in 0..128 {
            layer.push(Feature {
                fid: i,
                geometry: None,
                attributes: vec![FieldValue::Text(format!("record-{i}")), FieldValue::Date("2026-11-12".into())],
            });
        }
        let mut bytes = encode_dbf(&layer).unwrap();
        let layout = dbf_layout(&bytes, Some("UTF-8")).unwrap();
        bytes[layout.header_len + 5 * layout.record_len] = b'*';
        let caller = std::thread::current().id();
        let worker_allocated = AtomicBool::new(false);
        let calls = AtomicUsize::new(0);
        let records = read_dbf_records(&bytes, &layout, |record| {
            if std::thread::current().id() != caller {
                worker_allocated.store(true, Ordering::Relaxed);
            }
            calls.fetch_add(1, Ordering::Relaxed);
            layout.record(record)
        })
        .unwrap();
        assert!(!worker_allocated.load(Ordering::Relaxed), "DBF record storage was allocated on a pool thread");
        assert_eq!(calls.load(Ordering::Relaxed), 127);
        assert_eq!(records.len(), 128);
        assert!(records[5].is_none());
        assert_eq!(records[6].as_ref().unwrap(), &layer.features[6].attributes);
    }

    #[test]
    fn measure_ranges_exclude_missing_samples() {
        let coordinates = vec![
            Coord { x: 0.0, y: 0.0, z: Some(1.0), m: Some(3.0) },
            Coord { x: 1.0, y: 1.0, z: Some(2.0), m: None },
            Coord { x: 2.0, y: 2.0, z: Some(3.0), m: Some(8.0) },
        ];
        let shape = Geometry::LineString(coordinates);
        let record = encode_shape(&shape, 13, true, &mut Bounds::empty());
        // One part, three XY/Z coordinates; M range starts after the Z array.
        let at = 60 + 4 + 24 * 3;
        let value = |i| f64::from_le_bytes(record[i..i + 8].try_into().unwrap());
        assert_eq!((value(at), value(at + 8)), (3.0, 8.0));
        assert_eq!((value(at + 16), value(at + 24), value(at + 32)), (3.0, -1e39, 8.0));
        assert_eq!(parse_shape(&record, OrganizePolygons::default()).unwrap(), shape);
    }

    #[test]
    fn launders_names_like_gdal() {
        // The names ogr2ogr 3.12 writes into a shapefile from these GeoJSON properties.
        let names: Vec<String> = [
            "population_total",
            "population_totals",
            "population_totalz",
            "name",
            "NAME",
            "Name",
            "name2",
            "abcdefghi :x",
            "abc:def",
            "abcdefgh_1",
            "abcdefghij",
            "abcdefghijk",
        ]
        .map(String::from)
        .into();
        assert_eq!(
            dbf_field_names(&names),
            [
                "population",
                "populati_1",
                "populati_2",
                "name",
                "NAME3",
                "Name4",
                "name2",
                "abcdefghi",
                "abc_def",
                "abcdefgh_1",
                "abcdefghij",
                "abcdefgh_2"
            ]
        );
    }

    #[test]
    fn reads_back_z_holes_and_latin1() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.shp");
        let path = path.to_str().unwrap();
        let mut l = Layer::new("t");
        l.add_field(FieldDef::new("name", FieldType::Text));
        let ring = |x: f64, s: f64| {
            Ring::new(vec![Coord::xyz(x, 0.0, 1.0), Coord::xyz(x + s, 0.0, 2.0), Coord::xyz(x + s, s, 3.0), Coord::xyz(x, s, 4.0)])
        };
        let g = Geometry::MultiPolygon(vec![(ring(0.0, 10.0), vec![ring(1.0, 2.0)]), (ring(20.0, 5.0), vec![])]);
        l.push(Feature { fid: 0, geometry: Some(g.clone()), attributes: vec![FieldValue::Text("Zürich".into())] });
        write(&l, path).unwrap();
        let back = read(path, &ShapefileOptions::default()).unwrap();
        let Some(Geometry::MultiPolygon(ps)) = &back.features[0].geometry else { panic!("not a multipolygon") };
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].1.len(), 1);
        assert_eq!(ps[0].0.0.iter().map(|c| c.z.unwrap()).sum::<f64>(), 10.0);
        assert_eq!(back.features[0].attributes[0], FieldValue::Text("Zürich".into()));
        // Without a .cpg, text that is not UTF-8 is Latin-1.
        std::fs::remove_file(dir.path().join("t.cpg")).unwrap();
        let dbf = dir.path().join("t.dbf");
        let bytes = std::fs::read(&dbf).unwrap();
        let at = bytes.windows(7).position(|w| w == "Zürich".as_bytes()).unwrap();
        let mut latin1 = bytes[..at].to_vec();
        latin1.extend_from_slice(b"Z\xfcrich ");
        latin1.extend_from_slice(&bytes[at + 7..]);
        std::fs::write(&dbf, latin1).unwrap();
        assert_eq!(read(path, &ShapefileOptions::default()).unwrap().features[0].attributes[0], FieldValue::Text("Zürich".into()));
    }

    /// The x/y of the rings of each polygon, exterior first.
    fn polygon_rings(g: &Geometry) -> Vec<Vec<Vec<(f64, f64)>>> {
        let xy = |r: &Ring| -> Vec<(f64, f64)> { r.0.iter().map(|c| (c.x, c.y)).collect() };
        match g {
            Geometry::Polygon { exterior, interiors } => vec![std::iter::once(exterior).chain(interiors).map(xy).collect()],
            Geometry::MultiPolygon(polygons) => polygons.iter().map(|(e, is)| std::iter::once(e).chain(is).map(xy).collect()).collect(),
            _ => panic!("not polygonal: {g:?}"),
        }
    }

    #[test]
    fn organizes_rings_by_each_of_gdals_methods() {
        let cw = |x0: f64, y0: f64, x1: f64, y1: f64| vec![(x0, y0), (x0, y1), (x1, y1), (x1, y0)];
        let ccw = |x0: f64, y0: f64, x1: f64, y1: f64| vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)];
        // Records of rings stored as given, and the polygons GDAL 3.12.2 reads from them with
        // OGR_ORGANIZE_POLYGONS set to ONLY_CCW, DEFAULT, CCW_INNER_JUST_AFTER_CW_OUTER and SKIP,
        // each polygon as the indices of its rings.
        type Polygons<'a> = &'a [&'a [usize]];
        type Case<'a> = (&'a str, Vec<Vec<(f64, f64)>>, [Polygons<'a>; 4]);
        let cases: Vec<Case> = vec![
            (
                "a hole wound as its exterior",
                vec![cw(0., 0., 10., 10.), cw(2., 2., 8., 8.)],
                [&[&[0], &[1]], &[&[0, 1]], &[&[0], &[1]], &[&[0], &[1]]],
            ),
            (
                "both counter-clockwise",
                vec![ccw(0., 0., 10., 10.), ccw(2., 2., 8., 8.)],
                [&[&[0], &[1]], &[&[0, 1]], &[&[0], &[1]], &[&[0], &[1]]],
            ),
            (
                "an island in a hole, all clockwise",
                vec![cw(0., 0., 10., 10.), cw(2., 2., 8., 8.), cw(4., 4., 6., 6.)],
                [&[&[0], &[1], &[2]], &[&[0, 1], &[2]], &[&[0], &[1], &[2]], &[&[0], &[1], &[2]]],
            ),
            (
                "two exteriors, the second with a hole, all clockwise",
                vec![cw(0., 0., 4., 4.), cw(6., 0., 16., 10.), cw(8., 2., 14., 8.)],
                [&[&[0], &[1], &[2]], &[&[0], &[1, 2]], &[&[0], &[1], &[2]], &[&[0], &[1], &[2]]],
            ),
            ("properly wound", vec![cw(0., 0., 10., 10.), ccw(2., 2., 8., 8.)], [&[&[0, 1]], &[&[0, 1]], &[&[0, 1]], &[&[0], &[1]]]),
            (
                "a hole after another exterior",
                vec![cw(0., 0., 10., 10.), cw(20., 0., 30., 10.), ccw(2., 2., 8., 8.)],
                [&[&[0, 2], &[1]], &[&[0, 2], &[1]], &[&[0], &[1, 2]], &[&[0], &[1], &[2]]],
            ),
            ("the hole first", vec![ccw(2., 2., 8., 8.), cw(0., 0., 10., 10.)], [&[&[1, 0]], &[&[1, 0]], &[&[1, 0]], &[&[0], &[1]]]),
            (
                "two exteriors, each followed by its hole",
                vec![cw(0., 0., 10., 10.), ccw(2., 2., 8., 8.), cw(20., 0., 30., 10.), ccw(22., 2., 28., 8.)],
                [&[&[0, 1], &[2, 3]], &[&[0, 1], &[2, 3]], &[&[0, 1], &[2, 3]], &[&[0], &[1], &[2], &[3]]],
            ),
        ];
        let methods =
            [OrganizePolygons::OnlyCcw, OrganizePolygons::Containment, OrganizePolygons::CcwInnerJustAfterCwOuter, OrganizePolygons::Skip];
        for (name, rings, expected) in &cases {
            for (method, polygons) in methods.iter().zip(expected) {
                let g = organize(rings.iter().map(|r| closed(r)).collect(), *method).unwrap();
                let want: Vec<Vec<Vec<(f64, f64)>>> = polygons.iter().map(|p| p.iter().map(|&i| rings[i].clone()).collect()).collect();
                assert_eq!(polygon_rings(&g), want, "{name}, {method:?}");
            }
        }
        for (name, method) in
            [("only_ccw", OrganizePolygons::OnlyCcw), ("Default", OrganizePolygons::Containment), ("SKIP", OrganizePolygons::Skip)]
        {
            let mut options = ShapefileOptions::default();
            assert!(options.set("organize_polygons", name).unwrap());
            assert_eq!(options.method(), method);
        }
        assert!(ShapefileOptions::default().set("ORGANIZE_POLYGONS", "SOMETIMES").is_err());
        assert!(!ShapefileOptions::default().set("ENCODING", "UTF-8").unwrap());
    }

    #[test]
    fn splits_self_touching_rings_for_every_method() {
        // An Esri hole hanging from the bottom edge, and a stored hole: GDAL 3.13.3 splits the
        // ring whatever OGR_ORGANIZE_POLYGONS is, putting its pieces after the other rings.
        let esri = [(0., 0.), (0., 10.), (10., 10.), (10., 0.), (5., 0.), (6., 2.), (4., 2.), (5., 0.)];
        let hole = [(2., 6.), (3., 6.), (3., 7.), (2., 7.)];
        let rest = vec![(0., 10.), (10., 10.), (10., 0.), (5., 0.), (0., 0.)];
        let lp = vec![(6., 2.), (4., 2.), (5., 0.)];
        let polygon = vec![vec![rest.clone(), hole.to_vec(), lp.clone()]];
        for (method, want) in [
            (OrganizePolygons::OnlyCcw, polygon.clone()),
            (OrganizePolygons::Containment, polygon.clone()),
            (OrganizePolygons::CcwInnerJustAfterCwOuter, polygon),
            (OrganizePolygons::Skip, vec![vec![hole.to_vec()], vec![rest], vec![lp]]),
        ] {
            let g = organize(vec![closed(&esri), closed(&hole)], method).unwrap();
            assert_eq!(polygon_rings(&g), want, "{method:?}");
        }
    }

    #[test]
    fn an_unknown_environment_method_does_not_hide_the_next() {
        let env = |name: &str| match name {
            "TERRIO_ORGANIZE_POLYGONS" => Some("garbage".to_string()),
            "OGR_ORGANIZE_POLYGONS" => Some("DEFAULT".to_string()),
            _ => None,
        };
        assert_eq!(method_from_env(env), Some(OrganizePolygons::Containment));
        assert_eq!(method_from_env(|_| None), None);
    }

    fn closed(xy: &[(f64, f64)]) -> Vec<Coord> {
        let mut cs: Vec<Coord> = xy.iter().map(|&(x, y)| Coord::xy(x, y)).collect();
        cs.push(cs[0].clone());
        cs
    }

    #[test]
    fn splits_self_touching_rings_like_gdal() {
        // An Esri ring: clockwise round a 10 × 10 square, then round a hole touching the edge at
        // (5, 0), the way the ISTAT municipalities of Rutigliano and Santa Ninfa are stored. The
        // stored hole, the second part, comes before the split one, and each split ring starts one
        // vertex later than stored, both as in GDAL 3.13.
        let ring = [(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0), (5.0, 0.0), (6.0, 2.0), (4.0, 2.0), (5.0, 0.0)];
        let hole = [(2.0, 6.0), (3.0, 6.0), (3.0, 7.0), (2.0, 7.0)];
        assert_eq!(
            polygon_rings(&organize_rings(vec![closed(&ring), closed(&hole)])),
            [vec![
                vec![(0.0, 10.0), (10.0, 10.0), (10.0, 0.0), (5.0, 0.0), (0.0, 0.0)],
                hole.to_vec(),
                vec![(6.0, 2.0), (4.0, 2.0), (5.0, 0.0)]
            ]]
        );
        // Two holes on one ring, in the order the ring runs through them.
        let two = [(0.0, 0.0), (0.0, 10.0), (5.0, 10.0), (4.0, 8.0), (6.0, 8.0), (5.0, 10.0), (10.0, 10.0), (10.0, 0.0)]
            .into_iter()
            .chain([(5.0, 0.0), (6.0, 2.0), (4.0, 2.0), (5.0, 0.0)])
            .collect::<Vec<_>>();
        assert_eq!(
            polygon_rings(&organize_rings(vec![closed(&two), closed(&hole)])),
            [vec![
                vec![(0.0, 10.0), (5.0, 10.0), (10.0, 10.0), (10.0, 0.0), (5.0, 0.0), (0.0, 0.0)],
                hole.to_vec(),
                vec![(4.0, 8.0), (6.0, 8.0), (5.0, 10.0)],
                vec![(6.0, 2.0), (4.0, 2.0), (5.0, 0.0)]
            ]]
        );
        // The same ring stored from inside the hole, at (6, 2) or (4, 2): split all the same, the
        // square first. Both rings start one vertex after (5, 0), as in GDAL 3.13.1.
        for k in [5, 6] {
            let mut inside = ring.to_vec();
            inside.rotate_left(k);
            assert_eq!(
                polygon_rings(&organize_rings(vec![closed(&inside), closed(&hole)])),
                [vec![
                    vec![(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0), (5.0, 0.0)],
                    hole.to_vec(),
                    vec![(6.0, 2.0), (4.0, 2.0), (5.0, 0.0)]
                ]],
                "starting at {:?}",
                inside[0]
            );
        }
        // Two holes, stored from inside the upper one: the loop split off first comes first, the
        // rest, which holds the start, last.
        let mut upper = two.clone();
        upper.rotate_left(3);
        assert_eq!(
            polygon_rings(&organize_rings(vec![closed(&upper), closed(&hole)])),
            [vec![
                vec![(10.0, 10.0), (10.0, 0.0), (5.0, 0.0), (0.0, 0.0), (0.0, 10.0), (5.0, 10.0)],
                hole.to_vec(),
                vec![(6.0, 2.0), (4.0, 2.0), (5.0, 0.0)],
                vec![(4.0, 8.0), (6.0, 8.0), (5.0, 10.0)]
            ]]
        );
        // GDAL reads a record of a single ring as it is stored.
        assert_eq!(polygon_rings(&organize_rings(vec![closed(&ring)])), [vec![ring.to_vec()]]);
    }

    #[test]
    fn assigns_holes_touching_the_exterior() {
        // A stored hole whose first vertex lies on the exterior, at (5, 10): the even-odd test at
        // that vertex may say it is outside.
        let square = [(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0)];
        let hole = [(5.0, 10.0), (4.0, 8.0), (6.0, 8.0)];
        assert_eq!(polygon_rings(&organize_rings(vec![closed(&square), closed(&hole)])), [vec![square.to_vec(), hole.to_vec()]]);
        // The same hole with its first vertex repeated: the first edge has no length.
        let repeated = [(5.0, 10.0), (5.0, 10.0), (4.0, 8.0), (6.0, 8.0)];
        assert_eq!(polygon_rings(&organize_rings(vec![closed(&square), closed(&repeated)])), [vec![square.to_vec(), repeated.to_vec()]]);
    }

    #[test]
    fn keeps_rings_that_are_not_esri_holes() {
        // A figure-eight: the second lobe lies outside the first.
        let eight = [(0.0, 0.0), (0.0, 2.0), (2.0, 2.0), (2.0, 0.0), (4.0, 0.0), (4.0, -2.0), (2.0, -2.0), (2.0, 0.0)];
        // A spike out and back along the same edge.
        let spike = [(0.0, 0.0), (0.0, 4.0), (2.0, 4.0), (3.0, 5.0), (2.0, 4.0), (4.0, 4.0), (4.0, 0.0)];
        // A loop inside, but wound the same way as the ring: not a hole.
        let same = [(0.0, 0.0), (0.0, 10.0), (10.0, 10.0), (10.0, 0.0), (5.0, 0.0), (4.0, 2.0), (6.0, 2.0), (5.0, 0.0)];
        // Consecutive duplicate points.
        let dup = [(0.0, 0.0), (0.0, 4.0), (0.0, 4.0), (4.0, 4.0), (4.0, 0.0)];
        // An island as a second part, so that the record is one GDAL organizes.
        let island = [(20.0, 20.0), (20.0, 21.0), (21.0, 21.0), (21.0, 20.0)];
        for ring in [&eight[..], &spike, &same, &dup] {
            let g = organize_rings(vec![closed(ring), closed(&island)]);
            assert_eq!(polygon_rings(&g), [vec![ring.to_vec()], vec![island.to_vec()]], "{ring:?}");
        }
    }

    #[test]
    fn orients_rings() {
        let square = Ring::new(vec![Coord::xy(0.0, 0.0), Coord::xy(1.0, 0.0), Coord::xy(1.0, 1.0), Coord::xy(0.0, 1.0)]);
        let ext = oriented(&square, true);
        assert_eq!(ext.len(), 5);
        assert!(Ring::new(ext).signed_area() < 0.0);
        assert!(Ring::new(oriented(&square, false)).signed_area() > 0.0);
    }

    #[test]
    fn writes_dbf_like_gdal() {
        let mut l = Layer::new("t");
        for (n, t) in
            [("n", FieldType::Integer), ("x", FieldType::Float), ("s", FieldType::Text), ("d", FieldType::Date), ("b", FieldType::Boolean)]
        {
            l.add_field(FieldDef::new(n, t));
        }
        let attrs = vec![
            FieldValue::Integer(-12),
            FieldValue::Float(1.5),
            FieldValue::Text("Zürich".into()),
            FieldValue::Date("2020-01-02".into()),
            FieldValue::Boolean(true),
        ];
        l.push(Feature { fid: 0, geometry: Some(Geometry::point(1.0, 2.0)), attributes: attrs });
        l.push(Feature { fid: 1, geometry: None, attributes: vec![FieldValue::Null; 5] });
        let dbf = encode_dbf(&l).unwrap();
        let rec = &dbf[32 + 5 * 32 + 1..];
        let rec_len = 1 + 9 + 24 + 80 + 8 + 1;
        let first = String::from_utf8_lossy(&rec[..rec_len]).into_owned();
        assert!(first.starts_with("       -12       1.500000000000000Zürich"), "{first}");
        assert!(first.ends_with("20200102T"));
        let second = &rec[rec_len..2 * rec_len];
        assert_eq!(&second[1..10], b"*********");
        let (shp, shx) = encode_shapes(&l).unwrap();
        assert_eq!(shx.len(), 100 + 16);
        assert_eq!(i32::from_le_bytes(shp[32..36].try_into().unwrap()), 1);
    }

    #[test]
    fn takes_the_epsg_crs_of_an_esri_prj() {
        // GDAL's Shapefile driver replaces an ESRI .prj it identifies by the EPSG CRS, which a
        // GeoPackage then names EPSG:4326 (srs_id 4326), not a custom CRS.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.shp");
        let path = path.to_str().unwrap();
        let mut l = Layer::new("p");
        l.add_field(FieldDef::new("id", FieldType::Integer));
        l.push(Feature { fid: 0, geometry: Some(Geometry::point(1.0, 2.0)), attributes: vec![FieldValue::Integer(1)] });
        write(&l, path).unwrap();
        let esri = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
        std::fs::write(dir.path().join("p.prj"), esri).unwrap();
        let back = read(path, &ShapefileOptions::default()).unwrap();
        assert_eq!(back.crs_epsg(), Some(4326));
        assert_eq!(super::super::trusted_epsg(&back), Some(4326));
        // A .prj matching no EPSG CRS is kept as it is.
        let custom = r#"PROJCS["custom",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",123456.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",7.3],PARAMETER["Scale_Factor",0.9991],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;
        std::fs::write(dir.path().join("p.prj"), custom).unwrap();
        let back = read(path, &ShapefileOptions::default()).unwrap();
        assert_eq!(back.crs_epsg(), None);
        assert_eq!(back.crs_wkt(), Some(custom));
    }

    #[test]
    fn a_prj_that_holds_no_crs_gives_the_layer_none_with_a_warning() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.shp");
        let path = path.to_str().unwrap();
        let mut l = Layer::new("p");
        l.add_field(FieldDef::new("id", FieldType::Integer));
        l.push(Feature { fid: 0, geometry: Some(Geometry::point(1.0, 2.0)), attributes: vec![FieldValue::Integer(1)] });
        write(&l, path).unwrap();
        let shp = std::fs::read(path).unwrap();
        let dbf = std::fs::read(dir.path().join("p.dbf")).unwrap();
        // The layer's EPSG code, its WKT and the warnings, through each reader of a `.prj`: a
        // file (`read`), a page of one (`read_range`), a file in memory (`parse`) and a stream.
        let read_with = |prj: &str| {
            std::fs::write(dir.path().join("p.prj"), prj).unwrap();
            let options = ShapefileOptions::default();
            let stream = || Ok(crate::stream::open_layers(path, &Default::default(), 100)?.remove(0).layer().clone());
            let readers: [&dyn Fn() -> Result<Layer>; 4] = [
                &|| read(path, &options),
                &|| read_range(path, 0, None, &options),
                &|| parse(&shp, Some(&dbf), Some(prj), None, "p", &options),
                &stream,
            ];
            let mut readings = Vec::new();
            for reader in readers {
                let _ = crate::warnings::take();
                let layer = reader().unwrap();
                readings.push((layer.crs_epsg(), layer.crs_wkt().map(str::to_string), crate::warnings::take()));
            }
            assert!(readings.windows(2).all(|w| w[0] == w[1]), "{prj:?}: {readings:?}");
            readings.remove(0)
        };
        // Text that is not a CRS definition terrio reads: no CRS, and a warning naming the file
        // and quoting the text, whitespace, control characters (ESC, BEL, the 8-bit CSI) and the
        // invisible format characters (a right-to-left override, zero-width ones) as spaces. A
        // `.prj` naming a file is not read through, and WKT followed by prose is not a definition,
        // although the parser reads its first root.
        let esri = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
        let malformed = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984"(SPHEROID["WGS_1984",6378137,298.257223563]]]"#;
        let named = dir.path().join("other.prj");
        std::fs::write(&named, esri).unwrap();
        let named = named.to_str().unwrap();
        let named_quoted: String = named.chars().take(80).collect();
        for (prj, quoted) in [
            ("not a coordinate reference system", "not a coordinate reference system"),
            ("http://example.com/crs\n", "http://example.com/crs"),
            ("Projection GEOGRAPHIC\nDatum WGS84\nUnits DD", "Projection GEOGRAPHIC Datum WGS84 Units DD"),
            ("\x1b[31mred\x1b[0m\x07\u{9b}2J", " [31mred [0m  2J"),
            ("one line\u{2028}and another", "one line and another"),
            ("hidden\u{E0041}\u{2061}text", "hidden  text"),
            ("invoice\u{202E}fdp.exe\u{200B}\u{061C}\u{00AD}\u{FEFF}", "invoice fdp.exe    "),
            ("PROJCS[\"x\"]\nnot a coordinate reference system", "PROJCS[\"x\"] not a coordinate reference system"),
            ("ERROR:404", "ERROR:404"),
            (malformed, &malformed[..80]),
            (named, &named_quoted),
        ] {
            let warning = format!("p.prj holds no CRS definition terrio can read ('{quoted}'): the layer is read without a CRS");
            assert_eq!(read_with(prj), (None, None, vec![warning]), "{prj:?}");
        }
        // EPSG identification must not bypass full-form validation.
        for suffix in ["\nnotes", "[", "]"] {
            let text = format!("{esri}{suffix}");
            let (epsg, wkt, warnings) = read_with(&text);
            assert_eq!((epsg, wkt), (None, None), "{text}");
            assert_eq!(warnings.len(), 1, "{text}: {warnings:?}");
        }
        // An empty `.prj` (a byte-order mark alone included): no CRS, silently, as without one.
        for prj in ["", " \r\n", "\u{feff}"] {
            assert_eq!(read_with(prj), (None, None, vec![]), "{prj:?}");
        }
        // A byte-order mark is not part of the definition.
        let (epsg, wkt, warnings) = read_with(&format!("\u{feff}{esri}"));
        assert_eq!((epsg, warnings), (Some(4326), vec![]));
        assert!(wkt.is_some_and(|w| w.starts_with(r#"GEOGCS["WGS 84""#)));
        // Definitions terrio reads without an EPSG code are kept as they are, without a warning: a
        // reference in either case, and WKT followed by another definition, on the next line or
        // after a comma.
        let custom = r#"PROJCS["custom",GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",15.5],PARAMETER["scale_factor",0.9996],PARAMETER["false_easting",500000],PARAMETER["false_northing",0],UNIT["metre",1]]"#;
        let vertical = r#"VERTCS["NAVD_1988",VDATUM["North_American_Vertical_Datum_1988"],PARAMETER["Vertical_Shift",0.0],PARAMETER["Direction",1.0],UNIT["Meter",1.0]]"#;
        for prj in [
            custom,
            "+proj=longlat +ellps=GRS80 +no_defs",
            r#"PROJCS["x"]"#,
            "epsg:4326",
            "esri:102100",
            "iau_2015:49900",
            "ogc:crs84",
            &format!("{custom}\n{vertical}"),
            &format!("{custom},{vertical}"),
        ] {
            assert_eq!(read_with(&format!("{prj}\n")), (None, Some(prj.to_string()), vec![]), "{prj:?}");
        }
        // Conventional NUL / DOS EOF padding does not invalidate an otherwise complete WKT.
        for padding in ["\0", "\x1a", "\0\r\n"] {
            assert_eq!(read_with(&format!("{custom}{padding}")), (None, Some(custom.to_string()), vec![]));
        }
        // Engineering definitions retain their WKT without inventing an EPSG identity.
        let engineering = r#"ENGCRS["site grid",EDATUM["site"],CS[Cartesian,2],AXIS["(E)",east],AXIS["(N)",north],LENGTHUNIT["metre",1]]"#;
        for prj in [r#"LOCAL_CS["site grid",UNIT["metre",1]]"#, engineering, r#"ENGCRS["x"]"#] {
            assert!(SpatialRef::parse_literal(prj).is_ok(), "{prj:?}");
            assert_eq!(read_with(&format!("{prj}\n")), (None, Some(prj.to_string()), vec![]), "{prj:?}");
        }
        // Unsupported authorities and unknown codes retain definition-shaped metadata too.
        for prj in ["IGNF:LAMB93", "ignf:LAMB93", "urn:ogc:def:crs:IGNF::LAMB93", "EPSG:999999"] {
            assert!(SpatialRef::parse_literal(prj).is_err(), "{prj:?}");
            assert_eq!(read_with(&format!("{prj}\n")), (None, Some(prj.to_string()), vec![]), "{prj:?}");
        }
    }

    #[test]
    fn unreadable_prj_sidecars_warn_instead_of_becoming_silent_absences() {
        let _ = crate::warnings::take();
        assert!(decode_prj(vec![0xff], "p.prj").is_none());
        let warning = crate::warnings::take();
        assert_eq!(warning.len(), 1);
        assert!(warning[0].contains("p.prj is not UTF-8 text"));
        assert!(warning[0].contains("invalid utf-8"));
        assert_eq!(decode_prj("Réseau local".as_bytes().to_vec(), "p.prj").as_deref(), Some("Réseau local"));
        assert!(crate::warnings::take().is_empty());
        let directory = tempfile::tempdir().unwrap();
        assert!(read_prj(directory.path()).is_none());
        let warning = crate::warnings::take();
        assert_eq!(warning.len(), 1);
        assert!(warning[0].contains("cannot be read"));
        assert!(warning[0].contains(&directory.path().display().to_string()));
    }

    #[test]
    fn prj_warning_quotes_control_characters_in_the_layer_name() {
        let _ = crate::warnings::take();
        let mut layer = Layer::new("p\x1b\u{202e}");
        apply_prj(&mut layer, Some("not a CRS"));
        let warnings = crate::warnings::take();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].starts_with("p  .prj holds no CRS"), "{warnings:?}");
        assert!(!warnings[0].contains(['\x1b', '\u{202e}']));
    }

    #[test]
    fn a_crs_definition_is_told_by_its_form() {
        // Every CRS keyword, in either case, opened by either bracket.
        for k in CRS_WKT_KEYWORDS {
            for text in [format!(r#"{k}["x",UNIT["metre",1]]"#), format!(r#"{}("x")"#, k.to_ascii_lowercase())] {
                assert!(is_crs_definition(&text), "{text}");
            }
        }
        let deep = format!("GEOGCS[\"x\",{}1{}]", "A[".repeat(30_000), "]".repeat(30_000));
        for text in [
            r#"GEOGCS["x",DATUM("y",SPHEROID["z",6378137,298.257223563)]]"#,
            r#"LOCAL_CS["a]b",UNIT["metre",1]]"#,
            r#"LOCAL_CS["say ""grid""",UNIT["metre",1]]"#,
            deep.as_str(),
            // Roots one after the other, on the next line or after a comma.
            "PROJCS[\"x\"]\nVERTCS[\"y\"]",
            "LOCAL_CS[\"x\"] , VERTCS[\"y\"]\nVERTCS[\"z\"]\n",
            "ENGCRS[\"x\"]\nIGNF:LAMB93",
            // A known authority's code, in either case.
            "IGNF:LAMB93",
            "ignf:LAMB93",
            "ESRI:102100",
            "IAU_2015:49900",
            "EPSG:999999",
            "nrcan:CGVD28",
            // OGC URNs (OGC 07-092r3 7.2), in either case, and combined ones (7.5.2, 7.5.4, 7.5.5).
            "urn:ogc:def:crs:IGNF::LAMB93",
            "urn:ogc:def:crs:ignf::LAMB93",
            "URN:OGC:DEF:CRS:EPSG:9.8.15:4326",
            "urn:ogc:def:crs,crs:EPSG::4269,crs:EPSG::5713",
            "urn:ogc:def:crs,crs:EPSG::4326,cs:EPSG::4400,coordinateOperation:EPSG::16031",
            "urn:ogc:def:crs,datum:EPSG::6326,cs:EPSG::6422",
        ] {
            assert!(is_crs_definition(text), "{text}");
        }
        for text in [
            r#"FOO["x"]"#,
            "PROJCS",
            r#"PROJCS["x""#,
            r#"PROJCS["x"]]"#,
            r#"PROJCS["x"]][]"#,
            r#"PROJCS["x]"#,
            // A root followed by what is no definition.
            "PROJCS[\"x\"]\nnot a coordinate reference system",
            r#"LOCAL_CS["x"] notes [1]"#,
            r#"PROJCS["x"],"#,
            r#"PROJCS["x"],,VERTCS["y"]"#,
            r#"PROJCS["x"]VERTCS["y"]"#,
            r#"PROJCS["x"] VERTCS["y"]"#,
            "PROJCS[\"x\"]\tVERTCS[\"y\"]",
            r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984"(SPHEROID["WGS_1984",6378137,298.257223563]]]"#,
            "not a coordinate reference system",
            "http://example.com/crs",
            "IGNF:",
            ":LAMB93",
            "9IGNF:LAMB93",
            "IGNF:LAMB 93",
            "IGNF:LAMB93:2",
            // An authority that is none: other text holding a colon.
            "ERROR:404",
            "NOTE:DRAFT",
            "TODO:FIX",
            "note:draft",
            "Units:feet",
            "file:proj",
            "urn:ogc",
            "urn:ogc:def:crs:NOTE::DRAFT",
            "IGNF:LAMB/93",
            "/tmp/other.prj",
            r"C:\data\other.prj",
            "C:/data/other.prj",
            "urn:ogc:def:crs:IGNF:LAMB93",
            "urn:ogc:def:crs:IGNF::",
            "urn:ogc:def:crs:::LAMB93",
            "urn:ogc:def:crs:IGNF::LAMB 93",
            "urn:ogc:def:datum:EPSG::6326",
            "urn:ogc:def:crs,",
            "urn:ogc:def:crs,crs:EPSG::4269,unit:EPSG::9001",
        ] {
            assert!(!is_crs_definition(text), "{text}");
        }
    }

    /// A `.dbf` of one text field holding `cell`, its header declaring `count` records and the
    /// language driver id `ldid`.
    fn dbf(cell: &[u8], count: u32, ldid: u8) -> Vec<u8> {
        let width = cell.len() as u8;
        let mut d = vec![3u8, 125, 1, 1];
        d.extend_from_slice(&count.to_le_bytes());
        d.extend_from_slice(&65u16.to_le_bytes());
        d.extend_from_slice(&(1 + u16::from(width)).to_le_bytes());
        d.extend_from_slice(&[0; 17]);
        d.extend_from_slice(&[ldid, 0, 0]);
        d.extend_from_slice(b"NAME\0\0\0\0\0\0\0C\0\0\0\0");
        d.extend_from_slice(&[width, 0]);
        d.extend_from_slice(&[0; 14]);
        d.push(0x0D);
        d.push(b' ');
        d.extend_from_slice(cell);
        d.push(0x1A);
        d
    }

    /// Decodes the generated single text cell for the encoding regressions.
    fn text(d: &[u8], cpg: Option<&str>) -> String {
        let (_, records) = parse_dbf(d, cpg).unwrap();
        match &records[0].as_ref().unwrap()[0] {
            FieldValue::Text(s) => s.clone(),
            v => panic!("{v:?}"),
        }
    }

    #[test]
    fn the_cpg_then_the_language_driver_decide_the_encoding() {
        // As pyogrio 0.13.0 (GDAL 3.12.4) reads them.
        let cp1252 = b"Z\xfcrich \x80";
        assert_eq!(text(&dbf(cp1252, 1, 0), Some("1252")), "Zürich €");
        assert_eq!(text(&dbf(cp1252, 1, 0), Some("CP1252\r\n")), "Zürich €");
        assert_eq!(text(&dbf(cp1252, 1, 0), Some("LATIN1")), "Zürich \u{80}");
        assert_eq!(text(&dbf("Zürich €".as_bytes(), 1, 0x57), Some("UTF-8")), "Zürich €");
        assert_eq!(text(&dbf(cp1252, 1, 0x03), None), "Zürich €");
        assert_eq!(text(&dbf(cp1252, 1, 0x57), None), "Zürich \u{80}");
        // Nothing names an encoding: GDAL does not recode, and pyogrio reads ISO-8859-1.
        assert_eq!(text(&dbf("Zürich".as_bytes(), 1, 0), None), "ZÃ¼rich");
    }

    #[test]
    fn decodes_the_code_pages_gdal_recodes_from() {
        // .cpg values as GDAL's ConvertCodePage reads them, then language driver ids.
        let privet = "Привет";
        assert_eq!(text(&dbf(b"\xcf\xf0\xe8\xe2\xe5\xf2", 1, 0), Some("1251")), privet);
        assert_eq!(text(&dbf(b"\xcf\xf0\xe8\xe2\xe5\xf2", 1, 0), Some("WINDOWS-1251")), privet);
        assert_eq!(text(&dbf(b"\xf0\xd2\xc9\xd7\xc5\xd4", 1, 0), Some("KOI8-R")), privet);
        assert_eq!(text(&dbf(b"\x8f\xe0\xa8\xa2\xa5\xe2", 1, 0), Some("866")), privet);
        assert_eq!(text(&dbf(b"\xa3\xf3d\xbc", 1, 0), Some("88592")), "Łódź");
        assert_eq!(text(&dbf(b"\xa3\xf3d\xbc", 1, 0), Some("ISO-8859-2")), "Łódź");
        assert_eq!(text(&dbf(b"\xc3\xe5\xe9\xdc", 1, 0), Some("1253")), "Γειά");
        assert_eq!(text(&dbf(b"\xa5and\xa3", 1, 0x01), None), "Ñandú");
        assert_eq!(text(&dbf(b"\xa5and\xa3", 1, 0x02), None), "Ñandú");
        assert_eq!(text(&dbf(b"\x8f\xe0\xa8\xa2\xa5\xe2", 1, 0x26), None), privet);
        assert_eq!(text(&dbf(b"\xcf\xf0\xe8\xe2\xe5\xf2", 1, 0xC9), None), privet);
        assert_eq!(text(&dbf(b"\xcf", 1, 0), Some("ANSI 1251")), "П");
        // Undefined bytes are skipped; a name iconv does not know leaves the text unrecoded.
        assert_eq!(text(&dbf(b"a\x81b", 1, 0), Some("1252")), "ab");
        assert_eq!(text(&dbf(b"\xcf", 1, 0), Some("65001")), "\u{cf}");
    }

    #[test]
    fn decodes_multi_byte_code_pages_as_gdal_recodes_them() {
        // As glibc's iconv recodes them in GDAL: undefined bytes skipped, a cut character ends
        // the text, and glibc's tables where they differ from the WHATWG ones.
        assert_eq!(text(&dbf(b"\x80\x93\xfa\x96\x7b", 1, 0), Some("932")), "日本");
        assert_eq!(text(&dbf(b"\xd6\xd0\xce\xc4", 1, 0), Some("936")), "中文");
        assert_eq!(text(&dbf(b"\xd6\xd0\xce", 1, 0), Some("CP936")), "中");
        assert_eq!(text(&dbf(b"\xaa\xa1x", 1, 0), Some("GBK")), "x");
        assert_eq!(text(&dbf(b"\xc7\xd1\xb1\xb9", 1, 0), Some("949")), "한국");
        assert_eq!(text(&dbf(b"\xa2\xe8\xea\xbf", 1, 0), Some("949")), "");
        assert_eq!(text(&dbf(b"\xa2\xe8xyz", 1, 0), Some("UHC")), "yz");
        assert_eq!(text(&dbf(b"\xa4\xa4\xa4\xe5\x80", 1, 0x78), None), "中文\u{80}");
        assert_eq!(text(&dbf(b"\x88\x62", 1, 0), Some("950")), "b");
        assert_eq!(text(&dbf(b"\xc6\xa1", 1, 0), Some("950")), "\u{f6b1}");
    }

    #[test]
    fn the_record_count_of_the_header_is_not_trusted() {
        // 4,294,967,295 records declared, one held: one is read, and nothing is allocated for
        // the others.
        let (_, records) = parse_dbf(&dbf(b"a", u32::MAX, 0), None).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records.capacity() < 1024, "{} records allocated", records.capacity());
    }

    #[test]
    fn corrupt_counts_give_no_geometry() {
        let mut rec = 5i32.to_le_bytes().to_vec();
        rec.extend_from_slice(&[0; 32]);
        rec.extend_from_slice(&(-1i32).to_le_bytes());
        rec.extend_from_slice(&i32::MAX.to_le_bytes());
        assert_eq!(parse_shape(&rec, OrganizePolygons::default()), None);
    }

    #[test]
    fn a_range_reads_the_features_a_whole_read_numbers() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("r.shp");
        let path = path.to_str().unwrap();
        let mut l = Layer::new("r");
        l.add_field(FieldDef::new("id", FieldType::Integer));
        l.add_field(FieldDef::new("name", FieldType::Text));
        for i in 0..50i64 {
            let g = (i % 7 != 3).then(|| Geometry::point(i as f64, -(i as f64)));
            l.push(Feature { fid: i as u64, geometry: g, attributes: vec![FieldValue::Integer(i), FieldValue::Text(format!("f{i}"))] });
        }
        write(&l, path).unwrap();
        let whole = read(path, &ShapefileOptions::default()).unwrap();
        let check = |skip: usize, max: Option<usize>| {
            let part = read_range(path, skip, max, &ShapefileOptions::default()).unwrap();
            let start = skip.min(whole.features.len());
            let end = max.map_or(whole.features.len(), |m| (start + m).min(whole.features.len()));
            let key = |f: &Feature| (f.fid, f.geometry.clone(), f.attributes.clone());
            let got: Vec<_> = part.features.iter().map(key).collect();
            let want: Vec<_> = whole.features[start..end].iter().map(key).collect();
            assert_eq!(got, want, "{skip} {max:?}");
            let fields = |l: &Layer| l.schema.fields().iter().map(|f| (f.name.clone(), f.field_type, f.width)).collect::<Vec<_>>();
            assert_eq!(fields(&part), fields(&whole));
            assert_eq!(part.crs_epsg(), whole.crs_epsg());
        };
        for (skip, max) in [(0, Some(10)), (3, Some(4)), (49, Some(1)), (45, Some(20)), (60, Some(5)), (10, None), (0, None)] {
            check(skip, max);
        }
        // Without the .shx, the record headers are scanned instead.
        std::fs::remove_file(dir.path().join("r.shx")).unwrap();
        for (skip, max) in [(3, Some(4)), (49, Some(1)), (10, None)] {
            check(skip, max);
        }
    }
}

//! Dataset identification from bounded headers, with explicit deeper inspection.
#[cfg(not(target_family = "wasm"))]
use crate::Result;
use crate::formats::{Kind, RasterDriver, VectorDriver};
use serde::Serialize;

/// Bytes inspected by the header-only identifier.
pub const IDENTIFICATION_HEADER_BYTES: usize = 16 * 1024;

/// A recognized dataset format. Identification does not establish payload integrity.
#[derive(Debug, Clone, Serialize)]
pub struct Identification {
    /// GDAL-style format name.
    pub driver: String,
    /// Data models supported by the recognized container.
    pub kinds: Vec<Kind>,
    /// Whether the format supports native scientific arrays.
    pub multidimensional: bool,
    /// Whether identification required opening a dataset beyond the bounded header.
    pub deep: bool,
}
/// `found`, unless the process-wide allow-list leaves its driver out: GDAL identifies a
/// dataset only with its registered drivers (`GDALIdentifyDriver` skips the others), and the list
/// is the counterpart of de-registering the drivers it leaves out (`formats::set_allowed_drivers`),
/// so a disallowed driver identifies nothing. A dataset's kind is not known before it is
/// identified: a driver must be allowed by the list of each kind it belongs to, as `open` judges a
/// dataset (`formats::allowed_drivers(None)`); a point-cloud driver by `TERRIO_ALLOWED_DRIVERS`,
/// the only list that restricts them. The public functions return judged identifications; the
/// content check dispatches on unjudged ones and opens with the openers, which judge the list.
fn judged(found: Option<Identification>) -> Option<Identification> {
    let kind = |found: &Identification| (found.kinds == [Kind::PointCloud]).then_some(Kind::PointCloud);
    found.filter(|found| {
        crate::formats::allowed_drivers(kind(found)).is_none_or(|list| list.contains(&crate::formats::canonical_driver_name(&found.driver)))
    })
}
fn raster(driver: RasterDriver) -> Option<Identification> {
    driver.ensure_built().ok()?;
    let mut result = Identification {
        driver: driver.name().into(),
        kinds: vec![Kind::Raster],
        multidimensional: matches!(driver, RasterDriver::NetCDF | RasterDriver::HDF5 | RasterDriver::Zarr),
        deep: false,
    };
    if matches!(driver, RasterDriver::GPKG | RasterDriver::MBTiles | RasterDriver::PMTiles) {
        result.kinds.push(Kind::Vector);
    }
    Some(result)
}
fn vector(driver: VectorDriver) -> Option<Identification> {
    driver.ensure_built().ok()?;
    Some(Identification {
        driver: driver.name().into(),
        // The deep-reader fallback can return any enabled vector container.
        kinds: if matches!(driver, VectorDriver::GPKG | VectorDriver::MBTiles | VectorDriver::PMTiles) {
            vec![Kind::Raster, Kind::Vector]
        } else {
            vec![Kind::Vector]
        },
        multidimensional: false,
        deep: false,
    })
}
fn contains(bytes: &[u8], value: &[u8]) -> bool {
    bytes.windows(value.len()).any(|part| part == value)
}

/// Identifies uploaded bytes without opening a codec or copying the payload.
/// Unknown formats return `None`; no filename-only guesses are made for binary inputs.
pub fn identify_bytes(bytes: &[u8], name: &str) -> Option<Identification> {
    judged(identify_bytes_unjudged(bytes, name))
}
/// [`identify_bytes`] whatever the allow-list says (for the content check).
pub(crate) fn identify_bytes_unjudged(bytes: &[u8], name: &str) -> Option<Identification> {
    identify_header(&bytes[..bytes.len().min(IDENTIFICATION_HEADER_BYTES)], name, Some(bytes.len() as u64))
}
/// Identifies a fetched prefix without requiring the complete payload. `length`
/// is the total object size, when known; named raw HGT tiles require it.
pub fn identify_prefix(bytes: &[u8], name: &str, length: Option<u64>) -> Option<Identification> {
    if length.is_some_and(|length| length < bytes.len() as u64) {
        return None;
    }
    judged(identify_header(&bytes[..bytes.len().min(IDENTIFICATION_HEADER_BYTES)], name, length))
}
fn identify_header(bytes: &[u8], name: &str, length: Option<u64>) -> Option<Identification> {
    let filename = crate::vfs::Location::parse(name).file_name();
    let ext = crate::formats::extension(&filename).unwrap_or_default();
    if [b"II*\0".as_slice(), b"MM\0*", b"II+\0", b"MM\0+"].iter().any(|magic| bytes.starts_with(magic)) {
        return raster(RasterDriver::GTiff);
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return raster(RasterDriver::PNG);
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return raster(RasterDriver::JPEG);
    }
    if bytes.starts_with(b"\0\0\0\x0cjP  \r\n\x87\n") || bytes.starts_with(b"\xff\x4f\xff\x51") {
        return raster(RasterDriver::JP2);
    }
    if [b"CDF\x01".as_slice(), b"CDF\x02", b"CDF\x05"].iter().any(|magic| bytes.starts_with(magic)) {
        return raster(RasterDriver::NetCDF);
    }
    if [0, 512, 1024, 2048, 4096, 8192].iter().any(|&offset| bytes.get(offset..offset + 8) == Some(b"\x89HDF\r\n\x1a\n")) {
        return raster(if matches!(ext.as_str(), "nc" | "nc4" | "cdf") || contains(bytes, b"_NCProperties") {
            RasterDriver::NetCDF
        } else {
            RasterDriver::HDF5
        });
    }
    if bytes.starts_with(b"EHFA_HEADER_TAG") {
        return raster(RasterDriver::HFA);
    }
    if bytes.starts_with(b"DSBB") {
        return raster(RasterDriver::GSBG);
    }
    if bytes.starts_with(b"RUU CROSS SYSTEM MAP FORMAT") {
        return raster(RasterDriver::PCRaster);
    }
    if bytes.starts_with(b"UHL") {
        return raster(RasterDriver::DTED);
    }
    if bytes.starts_with(b"fgb\x03fgb\x01") {
        return vector(VectorDriver::FlatGeobuf);
    }
    if bytes.starts_with(b"PAR1") {
        return vector(VectorDriver::Parquet);
    }
    if bytes.starts_with(b"PMTiles\x03") {
        return raster(RasterDriver::PMTiles);
    }
    if bytes.starts_with(b"SQLite format 3\0") {
        if ext == "gpkg" || matches!(bytes.get(68..72), Some(b"GPKG" | b"GP10")) {
            return raster(RasterDriver::GPKG);
        }
        if ext == "mbtiles" {
            return raster(RasterDriver::MBTiles);
        }
        return None;
    }
    if bytes.len() >= 36 && bytes[..4] == 9994u32.to_be_bytes() && bytes[28..32] == 1000u32.to_le_bytes() {
        return vector(VectorDriver::Shapefile);
    }
    if ext == "dbf" && bytes.len() >= 32 && matches!(bytes[0], 3 | 0x30 | 0x83 | 0x8b) {
        let header = u16::from_le_bytes([bytes[8], bytes[9]]) as usize;
        let record = u16::from_le_bytes([bytes[10], bytes[11]]) as usize;
        if header >= 33
            && (header - 33).is_multiple_of(32)
            && record > (header - 33) / 32
            && length.is_none_or(|size| size >= header as u64)
            && bytes.get(header - 1).is_none_or(|last| *last == 0x0d)
        {
            return vector(VectorDriver::Shapefile);
        }
    }
    if bytes.starts_with(b"AC10") && bytes.get(4..6).is_some_and(|v| v.iter().all(u8::is_ascii_digit)) {
        return vector(VectorDriver::DWG);
    }
    if bytes.starts_with(b"AutoCAD Binary DXF") {
        return vector(VectorDriver::DXF);
    }
    if let Some(header) = bytes.get(..4) {
        let length = u32::from_be_bytes(header.try_into().expect("four bytes")) as usize;
        if (1..=65_536).contains(&length) && contains(&bytes[4..(length + 4).min(bytes.len())], b"\x0a\x09OSMHeader") {
            return vector(VectorDriver::OSM);
        }
    }
    if matches!(ext.as_str(), "mvt" | "pbf") && bytes.first() == Some(&0x1a) {
        return vector(VectorDriver::MVT);
    }
    if bytes.starts_with(b"PK\x03\x04") && ext == "kmz" {
        return vector(VectorDriver::KMZ);
    }
    #[cfg(feature = "pointcloud")]
    if bytes.starts_with(b"LASF") || bytes.starts_with(b"ASTM-E57") || bytes.starts_with(b"ply\n") || bytes.starts_with(b"ply\r\n") {
        let driver = if bytes.starts_with(b"LASF") {
            let copc = bytes.get(100..104).is_some_and(|count| count != [0, 0, 0, 0])
                && bytes
                    .get(94..96)
                    .and_then(|header| {
                        let offset = u16::from_le_bytes([header[0], header[1]]) as usize;
                        if offset >= 227 { bytes.get(offset..offset + 54) } else { None }
                    })
                    .is_some_and(|vlr| vlr[2..18].split(|byte| *byte == 0).next() == Some(b"copc".as_slice()) && vlr[18..20] == [1, 0]);
            if copc { "COPC" } else { "LAS" }
        } else if bytes.starts_with(b"ASTM-E57") {
            "E57"
        } else {
            "PLY"
        };
        return Some(Identification { driver: driver.into(), kinds: vec![Kind::PointCloud], multidimensional: false, deep: false });
    }
    #[cfg(feature = "grib")]
    if crate::drivers::grib::is_grib_bytes(bytes) {
        return raster(RasterDriver::GRIB);
    }
    if ext == "hgt" && length.is_some_and(|length| [1201u64, 3601].iter().any(|side| side * side * 2 == length)) {
        let stem = std::path::Path::new(&filename).file_stem()?.to_str()?.to_ascii_uppercase();
        let b = stem.as_bytes();
        if b.len() == 7
            && matches!(b[0], b'N' | b'S')
            && matches!(b[3], b'E' | b'W')
            && b[1..3].iter().chain(&b[4..]).all(u8::is_ascii_digit)
        {
            return raster(RasterDriver::SRTMHGT);
        }
    }
    if bytes.contains(&0) {
        return None;
    }
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim_start_matches('\u{feff}').trim_start();
    let lower = text.to_ascii_lowercase();
    if lower.starts_with("ncols") || lower.starts_with("nrows") {
        return raster(RasterDriver::AAIGrid);
    }
    if lower.starts_with("north:") && lower.contains("south:") && lower.contains("east:") {
        return raster(RasterDriver::GRASSASCIIGrid);
    }
    if lower.lines().next().is_some_and(|line| line.trim() == "envi") {
        return raster(RasterDriver::ENVI);
    }
    if lower.starts_with("datasetheader begin") {
        return raster(RasterDriver::ERS);
    }
    if ext == "sgrd" && lower.contains("cellcount_x") && lower.contains("cellcount_y") {
        return raster(RasterDriver::SAGA);
    }
    if lower.starts_with("!table") || (ext == "mif" && lower.starts_with("version")) {
        return vector(VectorDriver::MapInfo);
    }
    if ext == "dxf" && lower.contains("section") && lower.contains("header") {
        return vector(VectorDriver::DXF);
    }
    if text.starts_with('<') {
        let mut reader = quick_xml::Reader::from_str(text);
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e)) => {
                    return match e.local_name().as_ref() {
                        "gpx" => vector(VectorDriver::GPX),
                        "kml" => vector(VectorDriver::KML),
                        "osm" => vector(VectorDriver::OSM),
                        "VRTDataset" => raster(RasterDriver::VRT),
                        "FeatureCollection" => vector(VectorDriver::GML),
                        _ => None,
                    };
                }
                Ok(quick_xml::events::Event::Eof) | Err(_) => break,
                _ => (),
            }
        }
    }
    let json = text.trim_start_matches('\u{1e}').trim_start();
    let root_type = root_json_type(json);
    if root_type.as_deref() == Some("Topology") {
        return vector(VectorDriver::TopoJSON);
    }
    if [
        "FeatureCollection",
        "Feature",
        "Point",
        "MultiPoint",
        "LineString",
        "MultiLineString",
        "Polygon",
        "MultiPolygon",
        "GeometryCollection",
    ]
    .iter()
    .any(|kind| root_type.as_deref() == Some(*kind))
    {
        return vector(if text.starts_with('\u{1e}') || matches!(ext.as_str(), "geojsonl" | "geojsons" | "geojsonseq") {
            VectorDriver::GeoJSONSeq
        } else {
            VectorDriver::GeoJSON
        });
    }
    if matches!(ext.as_str(), "csv" | "tsv") && !text.is_empty() {
        return vector(VectorDriver::CSV);
    }
    None
}

// A prefix may end inside a large features array. Stop after the root type
// member, skipping nested values with serde's bounded parser rather than
// matching a geometry type embedded in an unrelated JSON object.
fn root_json_type(text: &str) -> Option<String> {
    use serde::de::{Error as _, MapAccess, Visitor};
    struct RootType<'a>(&'a mut Option<String>);
    impl<'de> Visitor<'de> for RootType<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("a JSON object")
        }
        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
            while let Some(key) = map.next_key::<String>()? {
                if key == "type" {
                    *self.0 = Some(map.next_value::<String>()?);
                    // Intentionally stop before an arbitrarily large trailing payload.
                    return Err(A::Error::custom("root type found"));
                }
                map.next_value::<serde::de::IgnoredAny>()?;
            }
            Ok(())
        }
    }
    let mut found = None;
    let _ = serde::Deserializer::deserialize_map(&mut serde_json::Deserializer::from_str(text), RootType(&mut found));
    found
}

/// Identifies a local/HTTP/S3 dataset from a bounded header. `deep` permits
/// existing dataset readers as a fallback for formats requiring companion files.
/// Those fallback readers may materialize formats without lazy metadata support.
#[cfg(not(target_family = "wasm"))]
pub fn identify(path: &str, deep: bool) -> Result<Option<Identification>> {
    Ok(judged(identify_unjudged(path, deep)?))
}
/// [`identify`] whatever the allow-list says (for the content check, which then opens
/// with the openers, each judging the list; the deeper fallback's opens judge it too).
#[cfg(not(target_family = "wasm"))]
pub(crate) fn identify_unjudged(path: &str, deep: bool) -> Result<Option<Identification>> {
    use std::io::Read;
    if path.starts_with("/vsimem/") {
        let bytes = crate::memfs::read_shared(std::path::Path::new(path))?;
        return Ok(identify_bytes_unjudged(bytes.as_ref(), path));
    }
    let location = crate::vfs::Location::parse(path);
    let identified = match &location {
        crate::vfs::Location::Local(path) if path.is_dir() => {
            let mut found =
                if path.join("hdr.adf").is_file() && path.join("w001001.adf").is_file() { raster(RasterDriver::AIG) } else { None };
            for marker in ["zarr.json", ".zgroup", ".zarray"] {
                let marker = path.join(marker);
                if marker.is_file() {
                    let mut bytes = Vec::new();
                    std::fs::File::open(&marker)?.take(IDENTIFICATION_HEADER_BYTES as u64).read_to_end(&mut bytes)?;
                    let text: String = String::from_utf8_lossy(&bytes).split_ascii_whitespace().collect();
                    if ["\"zarr_format\":2,", "\"zarr_format\":3,", "\"zarr_format\":2}", "\"zarr_format\":3}"]
                        .iter()
                        .any(|key| text.contains(key))
                    {
                        found = raster(RasterDriver::Zarr);
                    }
                    break;
                }
            }
            found
        }
        crate::vfs::Location::Local(local) => {
            if !std::fs::metadata(local)?.is_file() {
                return Err(crate::Error::Unsupported("dataset identification requires a regular file or dataset directory".into()));
            }
            let mut file = std::fs::File::open(local)?;
            let length = file.metadata()?.len();
            let mut header = Vec::new();
            file.by_ref().take(IDENTIFICATION_HEADER_BYTES as u64).read_to_end(&mut header)?;
            identify_header(&header, path, Some(length))
        }
        #[cfg(remote)]
        crate::vfs::Location::Url(_) | crate::vfs::Location::S3 { .. } => {
            let remote = location.remote().ok_or_else(|| crate::Error::InvalidArgument("not remote".into()))?;
            let (header, length) = crate::http::read_prefix(&remote, IDENTIFICATION_HEADER_BYTES)?;
            identify_header(&header, path, length)
        }
        #[cfg(not(remote))]
        crate::vfs::Location::Url(_) | crate::vfs::Location::S3 { .. } => return Err(crate::Error::disabled("remote")),
        _ => None,
    };
    if identified.is_some() || !deep {
        return Ok(identified);
    }
    let mut result = match crate::open_raster_lazy(path) {
        Ok(raster_source) => raster(raster_source.driver()),
        Err(_) => crate::open_vector(path, &Default::default()).ok().and_then(|source| vector(source.driver)),
    };
    if let Some(result) = &mut result {
        result.deep = true;
    }
    Ok(result)
}

/// Controls identification of many local paths and individual remote objects.
#[derive(Debug, Clone, Default)]
pub struct IdentifyOptions {
    /// Descend into local directories that are not recognized datasets.
    pub recursive: bool,
    /// Also descend into recognized dataset directories, such as Zarr stores.
    pub force_recursive: bool,
    /// Permit existing readers as a fallback when no header signature matches.
    pub deep: bool,
    /// Retain unknown inputs and I/O failures in the returned records.
    pub report_failures: bool,
}
/// One input and its recognized format, or a reported failure.
#[derive(Debug, Clone, Serialize)]
pub struct IdentifyRecord {
    /// Input path, with credential parameters redacted.
    pub name: String,
    /// Recognized driver metadata; fields are flattened in JSON reports.
    #[serde(flatten)]
    pub identification: Option<Identification>,
    /// Why the input could not be identified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl IdentifyRecord {
    /// Identifies an uploaded file, retaining an unknown-format record only when
    /// requested. The display name follows native URL-credential redaction.
    pub fn from_bytes(bytes: &[u8], name: &str, report_failures: bool) -> Option<Self> {
        let identification = identify_bytes(bytes, name);
        (identification.is_some() || report_failures).then(|| Self {
            name: crate::vsi::listing::redact_url(name).into_owned(),
            error: identification.is_none().then(|| "format not recognized from the available header".into()),
            identification,
        })
    }
}

/// Identifies many inputs in order, with deterministic local directory scans.
/// Recognized directories are emitted once and skipped unless force-recursive
/// scanning is requested. Directory symlinks are never followed during traversal.
/// Remote inputs are individual objects; remote directory traversal is not supported.
#[cfg(not(target_family = "wasm"))]
pub fn identify_many(paths: &[String], options: &IdentifyOptions) -> Vec<IdentifyRecord> {
    use std::collections::BTreeSet;
    let mut pending: Vec<_> = paths.iter().rev().map(|path| (path.clone(), false)).collect();
    let mut visited = BTreeSet::new();
    let mut records = Vec::new();
    while let Some((path, nested)) = pending.pop() {
        let name = crate::vsi::listing::redact_url(&path).into_owned();
        let local = match crate::vfs::Location::parse(&path) {
            crate::vfs::Location::Local(path) => Some(path),
            _ => None,
        };
        let directory = local.as_ref().is_some_and(|path| path.is_dir());
        if nested && directory && local.as_ref().is_some_and(|path| path.is_symlink()) {
            continue;
        }
        let found = identify(&path, options.deep);
        let failed = found.is_err();
        let recognized = matches!(&found, Ok(Some(_)));
        let error = match found {
            Ok(Some(identification)) => {
                records.push(IdentifyRecord { name: name.clone(), identification: Some(identification), error: None });
                None
            }
            Ok(None) => Some("format not recognized from the available header".to_string()),
            Err(error) => Some(error.to_string().replace(&path, &name)),
        };
        let recurse = directory && (options.recursive || options.force_recursive) && (!recognized || options.force_recursive);
        if recurse {
            if failed && options.report_failures {
                records.push(IdentifyRecord { name: name.clone(), identification: None, error: error.clone() });
            }
            let local = local.unwrap();
            let key = std::fs::canonicalize(&local).unwrap_or_else(|_| local.clone());
            if !visited.insert(key) {
                continue;
            }
            match std::fs::read_dir(local) {
                Ok(entries) => {
                    let mut children = Vec::new();
                    for entry in entries {
                        match entry {
                            Ok(entry) => match entry.path().into_os_string().into_string() {
                                Ok(path) => children.push(path),
                                Err(_) if options.report_failures => records.push(IdentifyRecord {
                                    name: name.clone(),
                                    identification: None,
                                    error: Some("entry path is not valid UTF-8".into()),
                                }),
                                Err(_) => (),
                            },
                            Err(error) if options.report_failures => {
                                records.push(IdentifyRecord { name: name.clone(), identification: None, error: Some(error.to_string()) })
                            }
                            Err(_) => (),
                        }
                    }
                    children.sort();
                    pending.extend(children.into_iter().rev().map(|path| (path, true)));
                }
                Err(error) if options.report_failures => {
                    records.push(IdentifyRecord { name, identification: None, error: Some(error.to_string()) })
                }
                Err(_) => (),
            }
        } else if !recognized && options.report_failures {
            records.push(IdentifyRecord { name, identification: None, error });
        }
    }
    records
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dted_user_header_is_anchored_at_the_start() {
        let mut bytes = vec![b' '; 128];
        bytes[80..83].copy_from_slice(b"UHL");
        assert!(identify_bytes(&bytes, "unknown.bin").is_none());
        bytes[..3].copy_from_slice(b"UHL");
        assert_eq!(identify_bytes(&bytes, "unknown.bin").unwrap().driver, "DTED");
    }

    #[test]
    fn bounded_signatures_do_not_trust_binary_filename_extensions() {
        assert_eq!(identify_bytes(b"II*\0garbage", "renamed.bin").unwrap().driver, "GTiff");
        assert!(identify_bytes(b"not a TIFF", "wrong.tif").is_none());
        assert!(identify_bytes(b"notes mentioning OSMHeader", "notes.txt").is_none());
        assert!(identify_bytes(&[3; 32], "fake.dbf").is_none());
        assert!(identify_bytes(b"Environmental notes", "notes.txt").is_none());
        assert_eq!(identify_bytes(b"ENVI\n samples = 1", "header.hdr").unwrap().driver, "ENVI");
        assert!(identify_bytes(br#"{"geometry":{"type":"Point","coordinates":[0,0]}}"#, "config.json").is_none());
        assert!(identify_bytes(br#"{"note":"type Point","nested":{"type":"Feature"}}"#, "config.json").is_none());
        assert_eq!(
            identify_bytes(br#"{"nested":{"type":"Point"},"type":"FeatureCollection","features":["#, "data.json").unwrap().driver,
            "GeoJSON"
        );
        assert_eq!(identify_bytes(b"\x89PNG\r\n\x1a\n", "image.bin").unwrap().driver, "PNG");
        assert_eq!(identify_bytes(br#"{"type": "FeatureCollection", "features": ["#, "data.bin").unwrap().driver, "GeoJSON");
        let data = [vec![b' '; IDENTIFICATION_HEADER_BYTES], br#"{"type":"FeatureCollection"}"#.to_vec()].concat();
        assert!(identify_bytes(&data, "data.json").is_none());
    }
    #[test]
    #[cfg(feature = "grib")]
    fn grib_signature_search_matches_the_reader_limit() {
        // The GRIB driver takes a message whose "GRIB" and
        // edition (1 or 2) are in the first kilobyte. GDAL 3.12.1 identifies "GRIB" alone there,
        // so at 301 to 1016 as here (the reader of #232 stopped at 300), and at 1017 to 1020 too.
        let mut bytes = vec![b' '; 2048];
        bytes[1017..1025].copy_from_slice(b"GRIB\0\0\0\x02");
        assert!(identify_bytes(&bytes, "data.bin").is_none());
        bytes[1016..1024].copy_from_slice(b"GRIB\0\0\0\x02");
        assert_eq!(identify_bytes(&bytes, "data.bin").unwrap().driver, "GRIB");
    }
    #[test]
    fn prefix_size_and_reported_names_are_preserved() {
        let size = 1201 * 1201 * 2;
        let found = identify_prefix(b"data", "https://example.invalid/N37W120.hgt?X-Amz-Signature=private.value", Some(size)).unwrap();
        assert_eq!(found.driver, "SRTMHGT");
        assert!(identify_prefix(b"longer", "file.bin", Some(2)).is_none());
        let record = IdentifyRecord::from_bytes(b"II*\0data", "https://example.invalid/a.tif?X-Amz-Signature=private", false).unwrap();
        let json = serde_json::to_value(record).unwrap();
        assert_eq!(json["driver"], "GTiff");
        assert!(!json["name"].as_str().unwrap().contains("private"));
        assert!(json.get("identification").is_none());
        assert!(IdentifyRecord::from_bytes(b"unknown", "file.bin", false).is_none());
        assert!(IdentifyRecord::from_bytes(b"unknown", "file.bin", true).unwrap().error.is_some());
    }
    #[test]
    #[cfg(feature = "zarr")]
    fn recursive_scans_stop_at_datasets_and_report_unknowns() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.tif"), b"II*\0header").unwrap();
        std::fs::write(root.path().join("bad.bin"), b"unknown").unwrap();
        let store = root.path().join("store.zarr");
        std::fs::create_dir(&store).unwrap();
        std::fs::write(store.join(".zgroup"), br#"{"zarr_format":2}"#).unwrap();
        std::fs::write(store.join("tile.png"), b"\x89PNG\r\n\x1a\n").unwrap();
        let paths = [root.path().to_string_lossy().into_owned()];
        let options = IdentifyOptions { recursive: true, ..Default::default() };
        let records = identify_many(&paths, &options);
        assert_eq!(records.iter().map(|r| r.identification.as_ref().unwrap().driver.as_str()).collect::<Vec<_>>(), ["GTiff", "Zarr"]);
        let forced = identify_many(&paths, &IdentifyOptions { force_recursive: true, ..Default::default() });
        assert_eq!(forced.iter().filter(|r| r.identification.is_some()).count(), 3);
        let failures = identify_many(&paths, &IdentifyOptions { report_failures: true, ..options });
        assert_eq!(failures.iter().filter(|r| r.error.is_some()).count(), 1);
    }
    #[test]
    fn deep_raw_raster_identification_uses_companion_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("raw.dat");
        let config = crate::RasterConfig { rows: 2, cols: 2, data_type: crate::DataType::F32, ..Default::default() };
        let source = crate::RasterDataset::new(crate::Raster::from_data(config, vec![0., 1., 2., 3.]).unwrap(), false);
        crate::write_raster_dataset(
            &source,
            path.to_str().unwrap(),
            &crate::RasterWriteOptions { driver: Some(RasterDriver::ENVI), ..Default::default() },
        )
        .unwrap();
        assert!(identify(path.to_str().unwrap(), false).unwrap().is_none());
        let found = identify(path.to_str().unwrap(), true).unwrap().unwrap();
        assert_eq!(found.driver, "ENVI");
        assert!(found.deep);
    }
    #[test]
    #[cfg(all(unix, feature = "zarr"))]
    fn recursive_scans_retain_dataset_directory_errors() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join(".zgroup");
        std::fs::write(&marker, br#"{"zarr_format":2}"#).unwrap();
        std::fs::set_permissions(&marker, std::fs::Permissions::from_mode(0o0)).unwrap();
        // Root may bypass file permissions; this fixture cannot induce its I/O error there.
        if std::fs::File::open(&marker).is_ok() {
            return;
        }
        let records = identify_many(
            &[root.path().to_string_lossy().into_owned()],
            &IdentifyOptions { recursive: true, report_failures: true, ..Default::default() },
        );
        assert!(records.iter().any(|record| record.name == root.path().to_str().unwrap() && record.error.is_some()));
    }
    #[test]
    #[cfg(unix)]
    fn non_utf8_children_do_not_alias_a_different_file() {
        use std::os::unix::ffi::OsStringExt;
        let root = tempfile::tempdir().unwrap();
        let invalid = root.path().join(std::ffi::OsString::from_vec(b"bad-\xff.tif".to_vec()));
        // APFS and HFS+ refuse names that are not valid UTF-8 (EILSEQ), so no such child can exist there.
        if let Err(error) = std::fs::write(invalid, b"II*\0header") {
            assert!(cfg!(target_os = "macos") && error.raw_os_error() == Some(92), "{error}");
            return;
        }
        std::fs::write(root.path().join("bad-\u{fffd}.tif"), b"II*\0header").unwrap();
        let records = identify_many(
            &[root.path().to_string_lossy().into_owned()],
            &IdentifyOptions { recursive: true, report_failures: true, ..Default::default() },
        );
        assert_eq!(records.iter().filter(|record| record.identification.is_some()).count(), 1);
        assert!(records.iter().any(|record| record.error.as_ref().is_some_and(|error| error.contains("UTF-8"))));
    }
    #[test]
    #[cfg(unix)]
    fn recursive_scans_do_not_follow_directory_symlinks() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("a.tif"), b"II*\0header").unwrap();
        std::os::unix::fs::symlink(root.path(), root.path().join("loop")).unwrap();
        let records = identify_many(
            &[root.path().to_string_lossy().into_owned()],
            &IdentifyOptions { force_recursive: true, report_failures: true, ..Default::default() },
        );
        assert_eq!(records.len(), 1);
    }
}

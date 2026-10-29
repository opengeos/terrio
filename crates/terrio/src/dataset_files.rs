//! Dataset-aware byte copies, renames and deletion, including companion files.
//!
//! These operations do not convert formats or decode payloads. Inventories distinguish
//! owned files from VRT dependencies: deleting a VRT never deletes its source rasters.
use crate::formats::{RasterDriver, VectorDriver};
use crate::vsi::{self, dataset as location};
use crate::{Error, Result};
use serde::{Serialize, Serializer};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::hash::BuildHasher;
use std::sync::atomic::{AtomicU64, Ordering};

/// File-operation options. The format hint restricts the source driver, not the output encoding.
#[derive(Debug, Clone, Default)]
pub struct FileOptions {
    /// Explicit input driver, for example `ENVI` or `ESRI Shapefile`.
    pub format: Option<String>,
    /// Replace an existing destination and its obsolete companion files.
    pub overwrite: bool,
    /// Allow trusted VRT references outside the source directory tree.
    pub allow_external_references: bool,
}
fn paths_json<S: Serializer>(paths: &[String], serializer: S) -> std::result::Result<S::Ok, S::Error> {
    paths.iter().map(|s| vsi::listing::redact_url(s).into_owned()).collect::<Vec<_>>().serialize(serializer)
}
/// Owned files of a dataset. Referenced raster assets are not owned by a VRT.
#[derive(Debug, Clone, Serialize)]
pub struct FileSet {
    /// Recognized source driver.
    pub driver: String,
    /// Owned paths. JSON reports redact URL credentials.
    #[serde(serialize_with = "paths_json")]
    pub files: Vec<String>,
    /// A directory dataset is moved/copied as one tree.
    pub directory: bool,
}
impl FileSet {
    /// Display names with URL credentials redacted, for human-readable reports.
    pub fn display_names(&self) -> Vec<String> {
        self.files.iter().map(|p| vsi::listing::redact_url(p).into_owned()).collect()
    }
}
/// Result of a dataset operation.
#[derive(Debug, Clone, Serialize)]
pub struct FileReport {
    /// Driver whose file set was used.
    pub driver: String,
    /// Final owned paths (source paths for deletion).
    #[serde(serialize_with = "paths_json")]
    pub files: Vec<String>,
    /// Bytes copied for new files; native same-filesystem moves copy no payload bytes.
    /// Backup and rollback work is excluded.
    pub bytes: u64,
    /// Staged files left after a committed operation whose cleanup failed.
    /// The operation succeeded; callers can remove these recovery files separately.
    #[serde(serialize_with = "paths_json")]
    pub recovery_files: Vec<String>,
}
impl FileReport {
    /// Cleanup locations with credentials redacted for text output.
    pub fn recovery_names(&self) -> Vec<String> {
        self.recovery_files.iter().map(|path| vsi::listing::redact_url(path).into_owned()).collect()
    }
}
fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidArgument(message.into())
}
fn stem(name: &str) -> &str {
    name.rsplit_once('.').filter(|(s, _)| !s.is_empty()).map_or(name, |(s, _)| s)
}
fn hinted(name: &str) -> Result<String> {
    if let Some(driver) = RasterDriver::from_name(name) {
        driver.ensure_built()?;
        return Ok(if driver == RasterDriver::Cog { RasterDriver::GTiff.name() } else { driver.name() }.into());
    }
    if let Some(driver) = VectorDriver::from_name(name) {
        driver.ensure_built()?;
        return Ok(driver.name().into());
    }
    #[cfg(feature = "pointcloud")]
    if ["LAS", "LAZ", "PLY", "E57", "COPC"].iter().any(|driver| driver.eq_ignore_ascii_case(name)) {
        return Ok(name.to_ascii_uppercase());
    }
    Err(invalid(format!("unknown dataset driver '{name}'")))
}
fn exists(path: &str) -> Result<bool> {
    Ok(vsi::stat(path)?.is_some())
}
fn unique_push(files: &mut Vec<String>, path: String) -> Result<()> {
    for existing in files.iter() {
        if location::same_name(existing, &path)? {
            return Ok(());
        }
    }
    files.push(path);
    Ok(())
}
fn companions(files: &mut Vec<String>, path: &str, names: impl IntoIterator<Item = String>) -> Result<()> {
    for name in names {
        let candidate = location::sibling(path, &name)?;
        if location::companion_exists(&candidate)? {
            unique_push(files, candidate)?;
        }
    }
    Ok(())
}
fn directory_driver(path: &str) -> Result<Option<String>> {
    for marker in ["zarr.json", ".zgroup", ".zarray"] {
        let marker = location::child(path, marker)?;
        if location::file_exists(&marker)? {
            // Deserialize only the required top-level field. Unknown metadata
            // is streamed and skipped rather than materialized or substring-matched.
            #[derive(serde::Deserialize)]
            struct ZarrMarker {
                zarr_format: u64,
            }
            let recognized = location::with_seekable(&marker, |reader| {
                Ok(serde_json::from_reader::<_, ZarrMarker>(reader).ok().is_some_and(|m| matches!(m.zarr_format, 2 | 3)))
            })?;
            if recognized {
                return Ok(Some("Zarr".into()));
            }
        }
    }
    let metadata = location::child(path, "metadata.json")?;
    if location::file_exists(&metadata)? {
        // Only the top-level members mark MVT; nested or quoted text does not. Unknown
        // members are streamed and skipped, as for Zarr markers. Unparseable metadata is
        // not MVT, so the directory falls through to the other driver checks.
        #[derive(serde::Deserialize)]
        struct MvtMetadata {
            format: Option<serde_json::Value>,
            vector_layers: Option<Vec<serde::de::IgnoredAny>>,
        }
        let mvt = location::with_seekable(&metadata, |reader| {
            Ok(serde_json::from_reader::<_, MvtMetadata>(reader).ok().is_some_and(|m| {
                m.format.as_ref().and_then(|f| f.as_str()).is_some_and(|f| matches!(f, "pbf" | "mvt")) || m.vector_layers.is_some()
            }))
        })?;
        if mvt {
            for entry in vsi::list(path, &vsi::ListOptions { recursive: true, ..Default::default() })? {
                let parts: Vec<_> = entry.name.trim_end_matches('/').split('/').collect();
                let integer = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
                let valid = if entry.is_dir() {
                    parts.len() <= 2 && parts.iter().all(|p| integer(p))
                } else if parts.as_slice() == ["metadata.json"] {
                    true
                } else {
                    parts.len() == 3
                        && integer(parts[0])
                        && integer(parts[1])
                        && parts[2].rsplit_once('.').is_some_and(|(y, ext)| integer(y) && matches!(ext, "pbf" | "mvt"))
                };
                if !valid {
                    return Err(invalid("MVT dataset directory contains unrelated entries; preserve them outside the tile tree"));
                }
            }
            return Ok(Some("MVT".into()));
        }
    }
    if location::file_exists(&location::child(path, "hdr.adf")?)? && location::file_exists(&location::child(path, "w001001.adf")?)? {
        return Ok(Some("AIG".into()));
    }
    // A directory is also a multi-layer datasource for these vector drivers.
    let entries = vsi::list(path, &Default::default())?;
    for (extension, driver) in [("shp", "ESRI Shapefile"), ("tab", "MapInfo File"), ("csv", "CSV")] {
        let roots: Vec<_> = entries
            .iter()
            .filter(|entry| !entry.is_dir() && crate::formats::extension(&entry.name).as_deref() == Some(extension))
            .collect();
        if !roots.is_empty() {
            let mut owned = BTreeSet::new();
            for root in roots {
                let dataset = file_list(&location::child(path, &root.name)?, Some(driver))?;
                for file in dataset.files {
                    owned.insert(location::name(&file)?);
                }
            }
            if entries.iter().any(|entry| entry.is_dir() || !owned.contains(&entry.name)) {
                return Err(invalid("vector dataset directory contains unrelated entries; select individual dataset files"));
            }
            return Ok(Some(driver.into()));
        }
    }
    Ok(None)
}
/// Inventories owned files without reading raster pixels. Unknown inputs fail rather than
/// treating an arbitrary directory or file as a dataset. `format` restricts identification.
pub fn file_list(path: &str, format: Option<&str>) -> Result<FileSet> {
    inventory(path, format)?.ok_or_else(|| invalid("dataset format is not recognized"))
}
fn inventory(path: &str, format: Option<&str>) -> Result<Option<FileSet>> {
    let _operation = location::Operation::new();
    let hint = format.map(hinted).transpose()?;
    let stat = vsi::stat(path)?.ok_or_else(|| invalid(format!("dataset '{}' does not exist", vsi::listing::redact_url(path))))?;
    if stat.is_dir() {
        let driver = directory_driver(path)?.ok_or_else(|| invalid("directory is not a recognized dataset"))?;
        if hint.as_ref().is_some_and(|hint| hint != &driver) {
            return Err(invalid("dataset does not match the input format hint"));
        }
        hinted(&driver)?;
        return Ok(Some(FileSet { driver, files: vec![path.into()], directory: true }));
    }
    let prefix = location::prefix(path, crate::inspection::IDENTIFICATION_HEADER_BYTES)?;
    let mut driver = crate::inspection::identify_prefix(&prefix, path, stat.size).map(|f| f.driver);
    let mut raw_reader = None;
    if driver.is_none() {
        let filename = location::name(path)?;
        if let Some(raw) = crate::drivers::open_raw(path) {
            let reader = raw?.into_lazy();
            driver = Some(reader.driver().name().into());
            raw_reader = Some(reader);
        } else if filename.to_ascii_lowercase().ends_with(".tab") && prefix.starts_with(b"!table") {
            driver = Some("MapInfo File".into());
        }
    }
    let Some(driver) = driver else {
        return Ok(None);
    };
    if hint.as_ref().is_some_and(|hint| hint != &driver) {
        return Err(invalid("dataset does not match the input format hint"));
    }
    let name = location::name(path)?;
    let base = stem(&name);
    let mut files = vec![path.to_string()];
    if matches!(driver.as_str(), "EHdr" | "ENVI" | "ERS" | "RST" | "SAGA") {
        // These readers have native lazy inventories, including raw headers and masks.
        let reader = match raw_reader {
            Some(reader) => reader,
            None => crate::open_raster_lazy(path)?,
        };
        for file in reader.file_list()? {
            unique_push(&mut files, file)?;
        }
    }
    let suffixes: &[&str] = match driver.as_str() {
        "ESRI Shapefile" => &["shp", "shx", "dbf", "prj", "cpg", "qix", "sbn", "sbx", "idm", "ind", "ain", "aih", "ixs", "mxs", "shp.xml"],
        "MapInfo File" if name.to_ascii_lowercase().ends_with(".mif") => &["mif", "mid"],
        "MapInfo File" => &["tab", "dat", "map", "id", "ind"],
        "CSV" => &["csvt", "prj"],
        "GML" => &["xsd", "gfs"],
        "HFA" => &["ige", "rrd"],
        "SAGA" => &["sgrd", "sdat", "prj"],
        "RST" => &["rst", "rdc", "ref", "smp"],
        "ENVI" | "EHdr" => &["hdr", "prj", "stx", "clr"],
        "AAIGrid" | "GRASSASCIIGrid" => &["prj"],
        _ => &[],
    };
    // Match the actual spelling on case-sensitive stores, including GDAL's upper-case companions.
    let mut names = Vec::new();
    for suffix in suffixes {
        names.push(format!("{base}.{suffix}"));
        names.push(format!("{base}.{}", suffix.to_ascii_uppercase()));
    }
    if matches!(driver.as_str(), "GTiff" | "JP2") {
        names.extend([format!("{base}.RPB"), format!("{base}.rpb"), format!("{base}_RPC.TXT"), format!("{base}_rpc.txt")]);
    }
    if driver == "PNG" || driver == "JPEG" || driver == "JP2" {
        let world = match driver.as_str() {
            "PNG" => "pgw",
            "JPEG" => "jgw",
            _ => "j2w",
        };
        names.extend([format!("{base}.{world}"), format!("{base}.wld"), format!("{name}w")]);
    }
    if RasterDriver::from_name(&driver).is_some() {
        names.extend([
            format!("{name}.aux.xml"),
            format!("{name}.ovr"),
            format!("{name}.msk"),
            format!("{name}.ovr.msk"),
            format!("{name}.msk.ovr"),
        ]);
    }
    if driver == "GTiff" && !location::tiff_has_transform(path)? {
        for world in [format!("{base}.tfw"), format!("{base}.TFW"), format!("{name}w"), format!("{base}.wld")] {
            let candidate = location::sibling(path, &world)?;
            if location::companion_exists(&candidate)? {
                let readable = location::file_exists(&candidate)?;
                unique_push(&mut files, candidate)?;
                if readable {
                    break;
                }
            }
        }
    }
    companions(&mut files, path, names)?;
    Ok(Some(FileSet { driver, files, directory: false }))
}

// Basename companions can be shared by distinct recognized primary files.
// A copy may read them, but destructive changes must include every owner.
fn check_shared_ownership(inventory: &FileSet, approved: &[String]) -> Result<()> {
    if inventory.directory {
        return Ok(());
    }
    let Some(primary) = inventory.files.first() else {
        return Ok(());
    };
    let name = location::name(primary)?;
    let base = stem(&name);
    // Normal inventories start with their primary file. Synthetic overwrite
    // inventories have no driver and contain only a prospective companion.
    let skip = usize::from(!inventory.driver.is_empty());
    let shared: Vec<_> = inventory.files.iter().skip(skip).collect();
    if shared.is_empty() {
        return Ok(());
    }
    for peer in location::siblings(primary)? {
        if stem(&location::name(&peer)?) != base {
            continue;
        }
        let mut approved_peer = false;
        for path in approved {
            if location::same_name(path, &peer)? {
                approved_peer = true;
                break;
            }
        }
        if approved_peer {
            continue;
        }
        let Some(other) = self::inventory(&peer, None)? else {
            continue;
        };
        for file in &other.files {
            for companion in &shared {
                if location::same_name(file, companion)? {
                    return Err(invalid(format!(
                        "dataset companion '{}' is shared with '{}'; include every owner in a batch deletion or separate the datasets first",
                        vsi::listing::redact_url(companion),
                        vsi::listing::redact_url(&peer)
                    )));
                }
            }
        }
    }
    Ok(())
}

struct Item {
    source: String,
    target: String,
    staged: String,
    installed: bool,
    rewritten: Option<Vec<u8>>,
    original: Option<String>,
    directory: bool,
    attempted: bool,
    copy_dependency: bool,
}
struct Backup {
    original: String,
    staged: String,
}
static TEMP_ID: AtomicU64 = AtomicU64::new(1);
fn temporary(path: &str) -> Result<String> {
    loop {
        let id = TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let nonce = std::collections::hash_map::RandomState::new().hash_one((path, id));
        let name = format!(".terrio-dataset-{nonce:016x}-{id}-{}", location::name(path)?);
        let path = location::sibling(path, &name)?;
        if !location::entry_exists(&path)? {
            return Ok(path);
        }
    }
}
fn mapped(path: &str, source: &str, target: &str) -> Result<String> {
    let (name, old, new) = (location::name(path)?, location::name(source)?, location::name(target)?);
    if path == source {
        return Ok(target.into());
    }
    let full_suffix = name.strip_prefix(&old).is_some_and(|suffix| suffix.is_empty() || suffix.starts_with(['.', '_']));
    let (prefix, replacement) = if full_suffix { (old.as_str(), new.as_str()) } else { (stem(&old), stem(&new)) };
    let suffix = name.strip_prefix(prefix).filter(|s| s.is_empty() || s.starts_with(['.', '_'])).ok_or_else(|| {
        invalid(format!(
            "unable to copy/rename fileset with irregular basenames: '{name}' does not start with '{prefix}'; \
             VRT dependencies must share the dataset's base name '{prefix}' to move with it"
        ))
    })?;
    location::sibling(target, &format!("{replacement}{suffix}"))
}
/// SourceFilename text is XML character data, including CDATA and character references.
fn reference_text(reader: &mut quick_xml::Reader<&[u8]>) -> Result<String> {
    use quick_xml::events::Event;
    let mut value = String::new();
    let mut cdata = false;
    loop {
        match reader.read_event().map_err(|e| Error::InvalidData(format!("VRT XML: {e}")))? {
            Event::Text(t) => {
                let text = t.xml10_content();
                if cdata {
                    if !text.trim().is_empty() {
                        return Err(invalid("mixed XML source filename content"));
                    }
                } else {
                    // CPL MiniXML skips literal leading XML whitespace before decoding
                    // entities, but preserves trailing and explicitly encoded spaces.
                    value.push_str(if value.is_empty() { text.trim_start_matches([' ', '\t', '\r', '\n']) } else { &text });
                }
            }
            Event::CData(t) => {
                if !value.is_empty() || cdata {
                    return Err(invalid("mixed XML source filename content"));
                }
                value.push_str(&t.xml10_content());
                cdata = true;
            }
            Event::GeneralRef(r) => {
                if cdata {
                    return Err(invalid("mixed XML source filename content"));
                }
                if let Some(c) = r.resolve_char_ref().map_err(|e| Error::InvalidData(e.to_string()))? {
                    value.push(c);
                } else {
                    value.push_str(match r.as_ref() {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        _ => return Err(invalid("unknown XML reference in source filename")),
                    });
                }
            }
            Event::End(_) if !value.is_empty() => return Ok(value),
            _ => return Err(invalid("invalid XML source filename content")),
        }
    }
}
fn relative_reference(element: &quick_xml::events::BytesStart<'_>, raw: bool) -> bool {
    element.attributes().flatten().find(|a| a.key.as_ref() == "relativeToVRT").map_or(raw, |a| {
        a.normalized_value(quick_xml::XmlVersion::Implicit1_0).map_or(raw, |v| crate::ops::vrt::relative_reference_flag(Some(&v), raw))
    })
}
#[derive(Default)]
struct FileGraph {
    active: HashSet<location::NameKey>,
    completed: HashSet<(location::NameKey, bool)>,
    metadata: HashMap<location::NameKey, String>,
    raw: HashSet<location::NameKey>,
    directories: HashMap<location::NameKey, String>,
    source_keys: HashSet<location::NameKey>,
}
fn remember_files(inventory: &FileSet, main: &str, graph: &mut FileGraph) -> Result<()> {
    if inventory.directory {
        for directory in &inventory.files {
            graph.directories.insert(location::name_key(directory)?, directory.clone());
        }
        return Ok(());
    }
    for file in &inventory.files {
        let primary = location::same_name(file, main)?;
        let extension = crate::formats::extension(&location::name(file)?).unwrap_or_default();
        let rewrite = match inventory.driver.as_str() {
            "VRT" | "GML" | "ERS" => primary,
            "RST" => extension == "rdc",
            "MapInfo File" => primary && extension == "tab",
            _ => false,
        };
        if rewrite {
            graph.metadata.entry(location::name_key(file)?).or_insert_with(|| inventory.driver.clone());
        }
    }
    Ok(())
}
/// Inventory VRT reference paths without opening pixel codecs. Owned-file queries and
/// deletion deliberately never enter this graph. Only descriptions are held in memory.
fn vrt_dependencies(path: &str, scope: &str, allow_external: bool, files: &mut Vec<String>, graph: &mut FileGraph) -> Result<()> {
    use quick_xml::{Reader, events::Event};
    if graph.active.len() >= 128 {
        return Err(Error::Unsupported("VRT file-set references exceed 128 levels".into()));
    }
    let active_key = location::name_key(path)?;
    if !graph.active.insert(active_key.clone()) {
        return Err(invalid("cyclic VRT file references"));
    }
    let bytes = vsi::read(path)?;
    let text = crate::drivers::vector::xml::decode(&bytes);
    let mut reader = Reader::from_str(&text);
    let mut metadata = 0usize;
    let mut bands = Vec::new();
    loop {
        let event = reader.read_event().map_err(|e| Error::InvalidData(format!("VRT XML: {e}")))?;
        if metadata > 0 {
            match event {
                Event::Start(_) => metadata += 1,
                Event::End(_) => metadata -= 1,
                Event::Eof => return Err(invalid("unterminated VRT metadata")),
                _ => (),
            }
            continue;
        }
        match event {
            Event::Start(element) if element.local_name().as_ref() == "Metadata" => metadata = 1,
            Event::Start(element) if element.local_name().as_ref() == "VRTRasterBand" => {
                bands.push(element.attributes().flatten().any(|a| {
                    a.key.as_ref() == "subClass"
                        && a.normalized_value(quick_xml::XmlVersion::Implicit1_0).is_ok_and(|v| v == "VRTRawRasterBand")
                }))
            }
            Event::End(element) if element.local_name().as_ref() == "VRTRasterBand" => {
                bands.pop();
            }
            Event::Start(element) if matches!(element.local_name().as_ref(), "SourceFilename" | "SourceDataset") => {
                let relative = relative_reference(&element, bands.last().copied().unwrap_or(false));
                let name = reference_text(&mut reader)?;
                let reference = crate::ops::vrt::resolve_reference(path, &name, relative);
                let physical = crate::ops::vrt::subdataset_parts(&reference)?.map_or(reference.as_str(), |(_, file, _)| file);
                if !allow_external && !location::in_source_tree(physical, scope)? {
                    return Err(invalid(
                        "VRT reference leaves the source directory tree; enable allow_external_references only for trusted references",
                    ));
                }
                let raw = bands.last().copied().unwrap_or(false);
                let physical_key = location::name_key(physical)?;
                if graph.active.contains(&physical_key) {
                    return Err(invalid("cyclic VRT file references"));
                }
                if graph.completed.contains(&(physical_key.clone(), raw)) {
                    continue;
                }
                if raw {
                    if !location::file_exists(physical)? {
                        return Err(invalid("raw VRT source file does not exist"));
                    }
                    if graph.source_keys.insert(physical_key.clone()) {
                        files.push(physical.into());
                    }
                    graph.raw.insert(physical_key.clone());
                    graph.completed.insert((physical_key, true));
                    continue;
                }
                let inventory = inventory(physical, None)?
                    .ok_or_else(|| Error::Unsupported("non-raw VRT reference is not a recognized dataset".into()))?;
                // A driver's inventory can include companions reached through
                // links or metadata; confine those paths before reading/copying.
                if !allow_external {
                    for file in &inventory.files {
                        if !location::in_source_tree(file, scope)? {
                            return Err(invalid("VRT companion leaves the source directory tree"));
                        }
                        if inventory.directory {
                            for entry in vsi::list(file, &vsi::ListOptions { recursive: true, ..Default::default() })? {
                                let child = location::child(file, &entry.name)?;
                                if !location::in_source_tree(&child, scope)? {
                                    return Err(invalid("VRT directory dependency leaves the source directory tree"));
                                }
                            }
                        }
                    }
                }
                remember_files(&inventory, physical, graph)?;
                let nested = inventory.driver == "VRT";
                for file in inventory.files {
                    if graph.source_keys.insert(location::name_key(&file)?) {
                        files.push(file);
                    }
                }
                if nested {
                    vrt_dependencies(physical, scope, allow_external, files, graph)?;
                }
                graph.completed.insert((physical_key, false));
            }
            Event::Eof => break,
            _ => (),
        }
    }
    graph.active.remove(&active_key);
    Ok(())
}
fn gml_schema_element(
    element: quick_xml::events::BytesStart<'_>,
    path: &str,
    names: &HashMap<location::NameKey, String>,
) -> Result<(quick_xml::events::BytesStart<'static>, bool)> {
    use quick_xml::XmlVersion;
    let mut element = element.into_owned();
    let mut attributes = Vec::new();
    let mut changed = false;
    for attribute in element.attributes() {
        let attribute = attribute.map_err(|e| Error::InvalidData(e.to_string()))?;
        let key = attribute.key.as_ref().to_string();
        let value = attribute.normalized_value(XmlVersion::Implicit1_0).map_err(|e| Error::InvalidData(e.to_string()))?;
        let local = key.rsplit(':').next().unwrap_or(&key);
        let mut tokens: Vec<String> = value.split_whitespace().map(str::to_owned).collect();
        if matches!(local, "schemaLocation" | "noNamespaceSchemaLocation") {
            for (i, token) in tokens.iter_mut().enumerate() {
                if local == "schemaLocation" && i % 2 == 0 {
                    continue;
                }
                let resolved = crate::ops::vrt::resolve_reference(path, token, true);
                if let Some(name) = names.get(&location::name_key(&resolved)?) {
                    changed |= token != name;
                    *token = name.clone();
                } else if !std::path::Path::new(token.as_str()).is_absolute()
                    && !token.contains("://")
                    && !token.starts_with("urn:")
                    && !(token.as_bytes().get(1) == Some(&b':') && token.as_bytes()[0].is_ascii_alphabetic())
                {
                    return Err(Error::Unsupported("relative GML schema reference is outside the dataset inventory".into()));
                }
            }
            attributes.push((key, tokens.join(" ")));
        } else {
            attributes.push((key, value.into_owned()));
        }
    }
    if changed {
        element.clear_attributes();
        element.extend_attributes(attributes.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    }
    Ok((element, changed))
}
fn xml_utf8_declaration(decl: quick_xml::events::BytesDecl<'_>) -> Result<quick_xml::events::BytesDecl<'static>> {
    let version = decl.version().map_err(|e| Error::InvalidData(e.to_string()))?;
    let standalone = decl.standalone().transpose().map_err(|e| Error::InvalidData(e.to_string()))?;
    Ok(quick_xml::events::BytesDecl::new(&version, Some("UTF-8"), standalone.as_deref()))
}
/// Rebase file-bearing metadata before touching either source or destination.
fn rewrite(path: &str, names: &HashMap<location::NameKey, String>, driver: &str) -> Result<Option<Vec<u8>>> {
    let filename = location::name(path)?;
    let extension = crate::formats::extension(&filename).unwrap_or_default();
    let gml = driver == "GML";
    if gml || driver == "VRT" {
        use quick_xml::{
            Reader, Writer, XmlVersion,
            events::{BytesText, Event},
        };
        let original = vsi::read(path)?;
        let text = crate::drivers::vector::xml::decode(&original);
        let mut reader = Reader::from_str(&text);
        let mut writer = Writer::new(Vec::new());
        let mut changed = false;
        let mut metadata = 0usize;
        let mut bands = Vec::new();
        loop {
            let event = reader.read_event().map_err(|e| Error::InvalidData(format!("VRT XML: {e}")))?;
            if !gml && (metadata > 0 || matches!(&event, Event::Start(e) if e.local_name().as_ref() == "Metadata")) {
                match &event {
                    Event::Start(_) => metadata += 1,
                    Event::End(_) => metadata -= 1,
                    Event::Eof => return Err(invalid("unterminated VRT metadata")),
                    _ => (),
                }
                writer.write_event(event)?;
                continue;
            }
            match event {
                Event::Decl(decl) => writer.write_event(Event::Decl(xml_utf8_declaration(decl)?))?,
                Event::Start(element) if gml => {
                    let (element, rebased) = gml_schema_element(element, path, names)?;
                    changed |= rebased;
                    writer.write_event(Event::Start(element))?;
                }
                Event::Empty(element) if gml => {
                    let (element, rebased) = gml_schema_element(element, path, names)?;
                    changed |= rebased;
                    writer.write_event(Event::Empty(element))?;
                }
                Event::Start(element) if !gml && element.local_name().as_ref() == "VRTRasterBand" => {
                    bands.push(element.attributes().flatten().any(|a| {
                        a.key.as_ref() == "subClass" && a.normalized_value(XmlVersion::Implicit1_0).is_ok_and(|v| v == "VRTRawRasterBand")
                    }));
                    writer.write_event(Event::Start(element))?;
                }
                Event::End(element) if !gml && element.local_name().as_ref() == "VRTRasterBand" => {
                    bands.pop();
                    writer.write_event(Event::End(element))?;
                }
                Event::Start(mut element) if matches!(element.local_name().as_ref(), "SourceFilename" | "SourceDataset") => {
                    let relative = relative_reference(&element, bands.last().copied().unwrap_or(false));
                    let end = element.to_end().into_owned();
                    let name = reference_text(&mut reader)?;
                    let resolved = crate::ops::vrt::resolve_reference(path, &name, relative);
                    let selector = crate::ops::vrt::subdataset_parts(&resolved)?;
                    let physical = selector.map_or(resolved.as_str(), |(_, file, _)| file);
                    let basename = names
                        .get(&location::name_key(physical)?)
                        .ok_or_else(|| Error::Unsupported("VRT reference is not covered by the dataset file set".into()))?;
                    let basename =
                        selector.map_or_else(|| basename.clone(), |(prefix, _, suffix)| format!("{prefix}\"{basename}\"{suffix}"));
                    changed |= basename != name || !relative;
                    let attributes: Vec<_> = element
                        .attributes()
                        .collect::<std::result::Result<Vec<_>, _>>()
                        .map_err(|e| Error::InvalidData(e.to_string()))?
                        .into_iter()
                        .filter(|a| a.key.as_ref() != "relativeToVRT")
                        .map(|a| {
                            Ok((
                                a.key.as_ref().to_string(),
                                a.normalized_value(XmlVersion::Implicit1_0).map_err(|e| Error::InvalidData(e.to_string()))?.into_owned(),
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    element.clear_attributes();
                    element.extend_attributes(attributes.iter().map(|(k, v)| (k.as_str(), v.as_str())));
                    element.push_attribute(("relativeToVRT", "1"));
                    writer.write_event(Event::Start(element))?;
                    writer.write_event(Event::Text(BytesText::from_escaped(crate::ops::vrt::xml::escape_filename(&basename))))?;
                    writer.write_event(Event::End(end))?;
                }
                Event::Eof => break,
                event => writer.write_event(event)?,
            }
        }
        return Ok(changed.then(|| writer.into_inner()));
    }
    if matches!(driver, "ERS" | "RST" | "MapInfo File") {
        let original = vsi::read(path)?;
        let mut output = Vec::with_capacity(original.len());
        let mut changed = false;
        // Opaque header lines may use a legacy encoding (e.g. WindowsLatin1 TAB).
        // Only file-bearing values need decoding; preserve all other bytes verbatim.
        for line in original.split_inclusive(|&b| b == b'\n') {
            let content = line.trim_ascii();
            let leading = line.len() - line.trim_ascii_start().len();
            let field = if extension == "ers" {
                content
                    .iter()
                    .position(|&b| b == b'=')
                    .filter(|&i| content[..i].trim_ascii().eq_ignore_ascii_case(b"DataFile"))
                    .map(|i| &content[i + 1..])
            } else if extension == "rdc" {
                content
                    .iter()
                    .position(|&b| b == b':')
                    .filter(|&i| content[..i].trim_ascii().eq_ignore_ascii_case(b"ref. system"))
                    .map(|i| &content[i + 1..])
            } else {
                content.get(..5).filter(|prefix| prefix.eq_ignore_ascii_case(b"File ")).map(|_| &content[5..])
            };
            if let Some(field) = field {
                let field = std::str::from_utf8(field).map_err(|_| Error::Unsupported("dataset file reference is not UTF-8".into()))?;
                let value = field.trim().trim_matches('"');
                let value = if extension == "rdc" { format!("{value}.ref") } else { value.into() };
                let resolved = location::sibling(path, &value)?;
                let replacement = match names.get(&location::name_key(&resolved)?) {
                    Some(name) => name,
                    None if extension == "rdc" => {
                        output.extend_from_slice(line);
                        continue;
                    } // Built-in reference systems have no owned .ref.
                    None => return Err(invalid("dataset header references a file outside its inventory")),
                };
                output.extend_from_slice(&line[..leading]);
                if extension == "rdc" {
                    output.extend_from_slice(b"ref. system : ");
                    output.extend_from_slice(stem(replacement).as_bytes());
                } else {
                    output.extend_from_slice(if extension == "ers" { b"DataFile = \"" } else { b"File \"" });
                    output.extend_from_slice(replacement.as_bytes());
                    output.push(b'"');
                }
                let end = line.iter().rposition(|&b| b != b'\r' && b != b'\n').map_or(0, |i| i + 1);
                output.extend_from_slice(&line[end..]);
                changed = true;
            } else {
                output.extend_from_slice(line);
            }
        }
        return Ok(changed.then_some(output));
    }
    Ok(None)
}
fn remove_owned(path: &str) -> Result<vsi::Report> {
    // Recursion on object stores deletes a prefix, not the exact object.
    // If stat fails, retain the path as a reported recovery entry rather than
    // guessing its kind and potentially deleting a broader prefix.
    let directory = vsi::stat(path)?.is_some_and(|entry| entry.is_dir());
    vsi::delete(path, directory)
}
fn cleanup<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<String> {
    paths.filter(|path| remove_owned(path).is_err()).map(str::to_owned).collect()
}
fn backup_to(source: &str, staged: &str, recovery: &mut Vec<String>) -> Result<()> {
    if let Err(error) = location::rename(source, staged) {
        if location::entry_exists(staged).unwrap_or(true) || !location::entry_exists(source).unwrap_or(false) {
            recovery.push(vsi::listing::redact_url(staged).into_owned());
        }
        return Err(error);
    }
    Ok(())
}
fn rollback(items: &mut [Item], backups: &[Backup], rename: bool) -> Vec<String> {
    let mut failed = Vec::new();
    for item in items.iter_mut().rev().filter(|item| item.attempted) {
        let current = if item.installed { &item.target } else { &item.staged };
        match location::entry_exists(current) {
            Ok(true) => {
                if rename && !item.copy_dependency && item.directory && location::entry_exists(&item.source).unwrap_or(true) {
                    failed.push(vsi::listing::redact_url(current).into_owned());
                    continue;
                }
                let result = if rename && !item.copy_dependency && item.original.is_none() {
                    location::rename(current, &item.source)
                } else {
                    remove_owned(current)
                };
                if result.is_err() {
                    failed.push(vsi::listing::redact_url(current).into_owned());
                }
            }
            Ok(false) if rename && !item.copy_dependency && item.original.is_none() => {
                if !location::entry_exists(&item.source).unwrap_or(false) {
                    // Neither copy survives, so report the source the user must recover.
                    failed.push(vsi::listing::redact_url(&item.source).into_owned());
                }
            }
            Ok(false) => (),
            Err(_) => failed.push(vsi::listing::redact_url(current).into_owned()),
        }
    }
    for item in items.iter().rev() {
        if let Some(original) = &item.original
            && location::rename(original, &item.source).is_err()
        {
            failed.push(vsi::listing::redact_url(original).into_owned());
        }
    }
    for backup in backups.iter().rev() {
        if location::rename(&backup.staged, &backup.original).is_err() {
            failed.push(vsi::listing::redact_url(&backup.staged).into_owned());
        }
    }
    failed
}

fn transfer(source: &str, target: &str, options: &FileOptions, rename: bool) -> Result<FileReport> {
    let _operation = location::Operation::new();
    let inventory = file_list(source, options.format.as_deref())?;
    let mut sources = inventory.files.clone();
    let mut graph = FileGraph::default();
    for path in &sources {
        graph.source_keys.insert(location::name_key(path)?);
    }
    remember_files(&inventory, source, &mut graph)?;
    if inventory.driver == "VRT" {
        vrt_dependencies(source, source, options.allow_external_references, &mut sources, &mut graph)?;
    }
    if rename {
        check_shared_ownership(&inventory, &inventory.files)?;
    }
    let owned_keys: HashSet<_> = inventory.files.iter().map(|path| location::name_key(path)).collect::<Result<_>>()?;
    if graph.metadata.keys().any(|metadata| graph.raw.contains(metadata)) {
        return Err(Error::Unsupported("file-bearing metadata also used as raw VRT pixels cannot be rewritten".into()));
    }
    let inputs = location::OverlapIndex::new(&sources)?;
    for directory in graph.directories.values() {
        if inputs.overlaps_other(directory)? {
            return Err(invalid("overlapping input directory and components cannot be staged independently"));
        }
    }
    let mut names = HashMap::new();
    for path in &sources {
        names.insert(location::name_key(path)?, location::name(&mapped(path, source, target)?)?);
    }
    let mut targets = HashSet::new();
    let mut items = Vec::new();
    for path in &sources {
        let destination = mapped(path, source, target)?;
        if !targets.insert(location::destination_key(&destination)?) {
            return Err(invalid("dataset components map to the same destination"));
        }
        location::check_write(&destination)?;
        let key = location::name_key(path)?;
        let directory = graph.directories.contains_key(&key);
        if vsi::stat(&destination)?.is_some_and(|entry| entry.is_dir() != directory) {
            return Err(invalid("dataset component file/directory collision"));
        }
        if rename && owned_keys.contains(&key) {
            location::check_remove(path)?;
        }
        if inputs.overlaps(&destination)? {
            return Err(invalid("dataset destination overlaps an input"));
        }
        let rewritten = match graph.metadata.get(&key) {
            Some(driver) => rewrite(path, &names, driver)?,
            None => None,
        };
        items.push(Item {
            source: path.clone(),
            staged: temporary(&destination)?,
            target: destination,
            installed: false,
            rewritten,
            original: None,
            directory,
            attempted: false,
            copy_dependency: !owned_keys.contains(&key),
        });
    }
    let mut obsolete = Vec::new();
    if exists(target)? {
        if !options.overwrite {
            return Err(invalid("dataset destination exists; use overwrite to replace it"));
        }
        // A failed inventory cannot establish which stale companions must be
        // removed. Do not silently replace only the primary file.
        let old = file_list(target, None)?;
        check_shared_ownership(&old, &old.files)?;
        obsolete = old.files;
    }
    for item in &items {
        if exists(&item.target)? && !obsolete.contains(&item.target) {
            if !options.overwrite {
                return Err(invalid("dataset companion destination exists; use overwrite to replace it"));
            }
            let companion = FileSet { driver: String::new(), files: vec![item.target.clone()], directory: false };
            check_shared_ownership(&companion, &obsolete)?;
            obsolete.push(item.target.clone());
        }
    }
    for path in &obsolete {
        // These entries are staged and unlinked, never followed for writing.
        // Destinations receiving new data already passed check_write above.
        location::check_remove(path)?;
        if inputs.overlaps(path)? {
            return Err(invalid("replaced dataset overlaps an input"));
        }
    }
    // Windows identity keys retain handles during preflight only. Release
    // them before moving or replacing any source/destination entries.
    drop(inputs);
    let mut backups = Vec::new();
    let mut uncertain_backups = Vec::new();
    let mut bytes = 0u64;
    let result = (|| {
        for item in &mut items {
            item.attempted = true;
            let report = if let Some(rewritten) = &item.rewritten {
                if rename && !item.copy_dependency {
                    let original = temporary(&item.source)?;
                    backup_to(&item.source, &original, &mut uncertain_backups)?;
                    item.original = Some(original);
                }
                vsi::write(&item.staged, rewritten)?;
                vsi::Report { bytes: rewritten.len() as u64, ..Default::default() }
            } else if rename && !item.copy_dependency {
                location::rename(&item.source, &item.staged)?
            } else {
                location::copy(&item.source, &item.staged, item.directory)?
            };
            bytes = bytes.checked_add(report.bytes).ok_or_else(|| invalid("copied byte count overflows"))?;
        }
        for path in &obsolete {
            let staged = temporary(path)?;
            backup_to(path, &staged, &mut uncertain_backups)?;
            backups.push(Backup { original: path.clone(), staged });
        }
        for item in &mut items {
            // All pre-existing destinations were backed up above. A remaining
            // entry can be a case alias of an earlier install on this filesystem.
            // Refuse and roll back rather than overwriting that component.
            if location::entry_exists(&item.target)? {
                return Err(invalid("dataset components collide on the destination filesystem"));
            }
            let report = location::rename(&item.staged, &item.target)?;
            item.installed = true;
            bytes = bytes.checked_add(report.bytes).ok_or_else(|| invalid("copied byte count overflows"))?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        let mut failed = rollback(&mut items, &backups, rename);
        failed.extend(uncertain_backups);
        if failed.is_empty() {
            return Err(error);
        }
        return Err(Error::InvalidData(format!("{error}; rollback incomplete; recovery files: {}", failed.join(", "))));
    }
    let recovery_files = cleanup(backups.iter().map(|b| b.staged.as_str()).chain(items.iter().filter_map(|item| item.original.as_deref())));
    Ok(FileReport { driver: inventory.driver, files: items.into_iter().map(|item| item.target).collect(), bytes, recovery_files })
}
/// Copies a dataset and its companion files, preserving encoded payload bytes.
pub fn copy(source: &str, target: &str, options: &FileOptions) -> Result<FileReport> {
    transfer(source, target, options, false)
}
/// Renames a dataset and its companion files. Same-filesystem native moves do not copy payloads.
pub fn rename(source: &str, target: &str, options: &FileOptions) -> Result<FileReport> {
    transfer(source, target, options, true)
}
/// Deletes owned dataset files. Referenced VRT assets are preserved. All paths are
/// checked before staging the first removal; dangerous roots and read-only stores fail.
pub fn delete(path: &str, format: Option<&str>) -> Result<FileReport> {
    Ok(delete_many(&[path.into()], format)?.remove(0))
}
/// Deletes multiple datasets after inventorying and checking every input. Overlapping owned
/// file sets are staged once; a bad input leaves all of the datasets untouched.
pub fn delete_many(paths: &[String], format: Option<&str>) -> Result<Vec<FileReport>> {
    let _operation = location::Operation::new();
    if paths.is_empty() {
        return Err(invalid("dataset deletion needs at least one input"));
    }
    let inventories = paths.iter().map(|path| file_list(path, format)).collect::<Result<Vec<_>>>()?;
    let mut files = Vec::new();
    let mut seen = HashSet::new();
    for inventory in &inventories {
        for path in &inventory.files {
            location::check_remove(path)?;
            if seen.insert(location::name_key(path)?) {
                files.push(path.clone());
            }
        }
    }
    for inventory in &inventories {
        check_shared_ownership(inventory, &files)?;
    }
    // A directory dataset already owns its descendants, even if a caller also named a layer.
    let mut owned: Vec<String> = Vec::new();
    let mut directories = location::OverlapIndex::default();
    files.sort_by_key(String::len);
    for path in files {
        if directories.contains_ancestor(&path)? {
            continue;
        }
        if vsi::stat(&path)?.is_some_and(|entry| entry.is_dir()) {
            directories.insert(&path)?;
        }
        owned.push(path);
    }
    drop(directories);
    let mut backups: Vec<Backup> = Vec::new();
    let mut uncertain_backups = Vec::new();
    let result = (|| {
        for path in &owned {
            let staged = temporary(path)?;
            backup_to(path, &staged, &mut uncertain_backups)?;
            backups.push(Backup { original: path.clone(), staged });
        }
        Ok(())
    })();
    if let Err(error) = result {
        let mut failed = rollback(&mut [], &backups, false);
        failed.extend(uncertain_backups);
        if failed.is_empty() {
            return Err(error);
        }
        return Err(Error::InvalidData(format!("{error}; rollback incomplete; recovery files: {}", failed.join(", "))));
    }
    let recovery_files = cleanup(backups.iter().map(|b| b.staged.as_str()));
    Ok(inventories
        .into_iter()
        .map(|inventory| FileReport { driver: inventory.driver, files: inventory.files, bytes: 0, recovery_files: recovery_files.clone() })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn directory() -> String {
        format!("/vsimem/dataset-files-test-{}", TEMP_ID.fetch_add(1, Ordering::Relaxed))
    }
    fn put(path: &str, bytes: &[u8]) {
        vsi::write(path, bytes).unwrap();
    }
    fn csv(root: &str, name: &str) -> String {
        let path = format!("{root}/{name}.csv");
        put(&path, b"x,y,name\n1,2,first\n3,4,second\n");
        path
    }
    #[test]
    fn vrt_relative_flags_match_gdal_sourced_and_raw_band_rules() {
        let missing = quick_xml::events::BytesStart::new("SourceFilename");
        assert!(relative_reference(&missing, true));
        assert!(!relative_reference(&missing, false));
        for (value, expected) in
            [("true", true), ("yes", true), ("random", true), ("", true), ("0", false), ("OFF", false), ("false", false)]
        {
            let mut element = quick_xml::events::BytesStart::new("SourceFilename");
            element.push_attribute(("relativeToVRT", value));
            assert_eq!(relative_reference(&element, true), expected);
        }
        for (value, expected) in [
            ("1", true),
            ("true", false),
            ("TRUE", false),
            ("yes", false),
            ("on", false),
            ("0", false),
            (" 1", true),
            ("2", true),
            ("01", true),
            ("-1", true),
        ] {
            let mut element = quick_xml::events::BytesStart::new("SourceFilename");
            element.push_attribute(("relativeToVRT", value));
            assert_eq!(relative_reference(&element, false), expected, "{value}");
        }
    }
    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn case_variant_companions_never_replace_one_another_on_native_output() {
        let source_root = directory();
        let source = csv(&source_root, "source");
        put(&format!("{source_root}/source.csvt"), b"Real,Real,String");
        put(&format!("{source_root}/source.CSVT"), b"Integer,Integer,String");
        let dir = tempfile::tempdir().unwrap();
        let probe = dir.path().join("case-probe");
        std::fs::write(&probe, b"probe").unwrap();
        let insensitive = dir.path().join("CASE-PROBE").exists();
        std::fs::remove_file(probe).unwrap();
        let target = dir.path().join("out.csv");
        let result = copy(&source, target.to_str().unwrap(), &Default::default());
        if cfg!(windows) || insensitive {
            assert!(result.is_err());
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
        } else {
            result.unwrap();
            assert_eq!(std::fs::read(dir.path().join("out.csvt")).unwrap(), b"Real,Real,String");
            assert_eq!(std::fs::read(dir.path().join("out.CSVT")).unwrap(), b"Integer,Integer,String");
        }
        assert_eq!(vsi::read(&format!("{source_root}/source.csvt")).unwrap(), b"Real,Real,String");
        assert_eq!(vsi::read(&format!("{source_root}/source.CSVT")).unwrap(), b"Integer,Integer,String");
        vsi::delete(&source_root, true).unwrap();
    }
    #[test]
    fn header_rebasing_preserves_opaque_bytes_and_line_endings() {
        let root = directory();
        let path = format!("{root}/source.tab");
        let opaque = b"!table\r\n!charset WindowsLatin1\r\nName = \"caf\xe9\"\r\n";
        put(&path, opaque);
        assert!(rewrite(&path, &HashMap::new(), "MapInfo File").unwrap().is_none());
        for (extension, driver, field, replacement) in [
            ("tab", "MapInfo File", "fIlE \"source.dat\"", "File \"copied.dat\""),
            ("ers", "ERS", "DataFile = \"source.dat\"", "DataFile = \"copied.dat\""),
            ("rdc", "RST", "ref. system : source", "ref. system : copied"),
        ] {
            let path = format!("{root}/source.{extension}");
            let source = format!("{root}/source.{}", if extension == "rdc" { "ref" } else { "dat" });
            let target = if extension == "rdc" { "copied.ref" } else { "copied.dat" };
            let names = HashMap::from([(location::name_key(&source).unwrap(), target.to_owned())]);
            for ending in ["\r\n", "\n", ""] {
                let mut bytes = opaque.to_vec();
                bytes.extend_from_slice(format!("  {field}{ending}").as_bytes());
                put(&path, &bytes);
                let mut expected = opaque.to_vec();
                expected.extend_from_slice(format!("  {replacement}{ending}").as_bytes());
                assert_eq!(rewrite(&path, &names, driver).unwrap().unwrap(), expected);
            }
        }
        vsi::delete(&root, true).unwrap();
    }
    #[test]
    #[cfg(unix)]
    fn overwrite_removes_a_dangling_obsolete_companion() {
        let dir = tempfile::tempdir().unwrap();
        let source = csv(dir.path().to_str().unwrap(), "source");
        let target = csv(dir.path().to_str().unwrap(), "target");
        let companion = dir.path().join("target.csvt");
        std::os::unix::fs::symlink("missing-type-file", &companion).unwrap();
        assert!(file_list(&target, None).unwrap().files.iter().any(|name| name.ends_with("target.csvt")));
        copy(&source, &target, &FileOptions { overwrite: true, ..Default::default() }).unwrap();
        assert!(std::fs::symlink_metadata(&companion).is_err());
        assert_eq!(std::fs::read(target).unwrap(), std::fs::read(source).unwrap());
    }

    #[test]
    #[cfg(unix)]
    fn vrt_whitespace_references_keep_the_correct_pixels_through_copy() {
        use crate::{DataType, Raster, RasterConfig, RasterDataset, Window};
        use wbraster::raster::RasterData;
        for (text, source_name, destination, expected) in [
            ("source.tif ", "source.vrt", "copied.vrt", 9.),
            (" source.tif", "source.vrt", "copied.vrt", 1.),
            ("&#32;source.tif ", " source.vrt", " copied.vrt", 7.),
            ("<![CDATA[ source.tif ]]>", " source.vrt", " copied.vrt", 7.),
        ] {
            let dir = tempfile::tempdir().unwrap();
            for (name, value) in [("source.tif", 1), ("source.tif ", 9), (" source.tif ", 7)] {
                let raster = Raster::from_data_native(
                    RasterConfig { cols: 1, rows: 1, bands: 1, data_type: DataType::U8, ..Default::default() },
                    RasterData::U8(vec![value]),
                )
                .unwrap();
                crate::write_raster_dataset(
                    &RasterDataset::new(raster, false),
                    dir.path().join(name).to_str().unwrap(),
                    &crate::RasterWriteOptions { driver: Some(RasterDriver::GTiff), ..Default::default() },
                )
                .unwrap();
            }
            let source = dir.path().join(source_name);
            std::fs::write(&source,format!("<VRTDataset rasterXSize='1' rasterYSize='1'><VRTRasterBand dataType='Byte' band='1'><SimpleSource><SourceFilename relativeToVRT='1'>{text}</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>")).unwrap();
            let out = dir.path().join("out");
            std::fs::create_dir(&out).unwrap();
            let destination = out.join(destination);
            copy(source.to_str().unwrap(), destination.to_str().unwrap(), &Default::default()).unwrap();
            for path in [&source, &destination] {
                let mut reader = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
                let data = reader.read_window(Window::new(0, 0, 1, 1), &Default::default()).unwrap();
                assert_eq!(data.raster.data.to_f64_vec(), vec![expected], "{text}: {}", path.display());
            }
        }
    }

    #[test]
    #[cfg(unix)]
    fn dangling_world_link_does_not_hide_a_readable_lower_priority_world_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.tif");
        let file = std::fs::File::create(&path).unwrap();
        tiff::encoder::TiffEncoder::new(file).unwrap().write_image::<tiff::encoder::colortype::Gray8>(1, 1, &[1]).unwrap();
        let broken = dir.path().join("source.tfw");
        std::os::unix::fs::symlink("missing-world", &broken).unwrap();
        let world = dir.path().join("source.wld");
        std::fs::write(&world, "1\n0\n0\n-1\n0.5\n0.5\n").unwrap();
        let inventory = file_list(path.to_str().unwrap(), None).unwrap();
        for owned in [broken, world] {
            assert!(inventory.files.contains(&owned.to_str().unwrap().to_owned()), "{inventory:?}");
        }
    }

    #[test]
    fn source_filename_whitespace_matches_cpl_minixml() {
        for (text, expected) in [
            (" source.tif ", "source.tif "),
            (" source.tif", "source.tif"),
            ("&#32;source.tif ", " source.tif "),
            ("<![CDATA[ source.tif ]]>", " source.tif "),
            ("<![CDATA[source.tif]]> ", "source.tif"),
        ] {
            let xml = format!("<SourceFilename>{text}</SourceFilename>");
            let mut reader = quick_xml::Reader::from_str(&xml);
            reader.read_event().unwrap();
            assert_eq!(reference_text(&mut reader).unwrap(), expected);
            assert_eq!(crate::ops::vrt::xml::parse(&xml).unwrap().text, expected);
        }
    }

    #[test]
    fn nested_or_malformed_zarr_markers_cannot_authorize_directory_deletion() {
        for bytes in [br#"{"other":{"zarr_format":2}}"#.as_slice(), br#"{"zarr_format":2} garbage"#, br#"{"zarr_format":"2"}"#] {
            let root = directory();
            put(&format!("{root}/.zgroup"), bytes);
            put(&format!("{root}/keep.txt"), b"preserve");
            assert!(file_list(&root, None).is_err());
            assert!(delete(&root, None).is_err());
            assert!(copy(&root, &format!("{root}-copy"), &Default::default()).is_err());
            assert_eq!(vsi::read(&format!("{root}/keep.txt")).unwrap(), b"preserve");
            vsi::delete(&root, true).unwrap();
        }
        let root = directory();
        // Recognition must not depend on fitting the complete marker in a sniff prefix.
        let marker = format!(r#"{{"zarr_format":2,"description":"{}"}}"#, "x".repeat(131_072));
        put(&format!("{root}/.zgroup"), marker.as_bytes());
        assert_eq!(file_list(&root, None).unwrap().driver, "Zarr");
        vsi::delete(&root, true).unwrap();
    }
    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn vrt_references_cannot_export_files_outside_the_source_tree_without_opt_in() {
        let directory = tempfile::tempdir().unwrap();
        let source_dir = directory.path().join("inputs");
        std::fs::create_dir(&source_dir).unwrap();
        let source = source_dir.join("source.vrt");
        let data = directory.path().join("source.raw");
        let target = directory.path().join("copied.vrt");
        std::fs::write(&data, [1, 2, 3, 4]).unwrap();
        let definition = |name: &str, raw: bool| {
            format!(
                "<VRTDataset rasterXSize='2' rasterYSize='2'><VRTRasterBand band='1' dataType='Byte' subClass='{}'><SourceFilename relativeToVRT='1'>{name}</SourceFilename><ImageOffset>0</ImageOffset><PixelOffset>1</PixelOffset><LineOffset>2</LineOffset></VRTRasterBand></VRTDataset>",
                if raw { "VRTRawRasterBand" } else { "VRTSourcedRasterBand" }
            )
        };
        for reference in ["../source.raw".to_owned(), data.to_str().unwrap().to_owned()] {
            std::fs::write(&source, definition(&reference, true)).unwrap();
            assert!(
                copy(source.to_str().unwrap(), target.to_str().unwrap(), &Default::default())
                    .unwrap_err()
                    .to_string()
                    .contains("source directory tree")
            );
            assert!(!target.exists());
        }
        copy(source.to_str().unwrap(), target.to_str().unwrap(), &FileOptions { allow_external_references: true, ..Default::default() })
            .unwrap();
        assert_eq!(std::fs::read(directory.path().join("copied.raw")).unwrap(), [1, 2, 3, 4]);
        assert!(data.exists());
        std::fs::remove_file(&target).unwrap();
        std::fs::write(source_dir.join("source.raw"), [1, 2, 3, 4]).unwrap();
        std::fs::write(&source, definition("source.raw", false)).unwrap();
        assert!(
            copy(
                source.to_str().unwrap(),
                target.to_str().unwrap(),
                &FileOptions { allow_external_references: true, ..Default::default() }
            )
            .unwrap_err()
            .to_string()
            .contains("not a recognized dataset")
        );
        #[cfg(unix)]
        {
            std::fs::remove_file(source_dir.join("source.raw")).unwrap();
            std::os::unix::fs::symlink(&data, source_dir.join("source.raw")).unwrap();
            std::fs::write(&source, definition("source.raw", true)).unwrap();
            assert!(copy(source.to_str().unwrap(), target.to_str().unwrap(), &Default::default()).is_err());
            assert!(!target.exists());
        }
    }

    #[test]
    #[cfg(all(unix, feature = "zarr"))]
    fn vrt_directory_dependency_links_are_confined_before_copying() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("inputs");
        let store = input.join("source.zarr");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::write(store.join(".zgroup"), b"{\"zarr_format\":2}").unwrap();
        let outside = directory.path().join("source.raw");
        std::fs::write(&outside, [1, 2, 3, 4]).unwrap();
        std::os::unix::fs::symlink(&outside, store.join("linked")).unwrap();
        let source = input.join("source.vrt");
        std::fs::write(&source, b"<VRTDataset rasterXSize='1' rasterYSize='1'><VRTRasterBand band='1' dataType='Byte'><SimpleSource><SourceFilename relativeToVRT='1'>source.zarr</SourceFilename></SimpleSource></VRTRasterBand></VRTDataset>").unwrap();
        let target = directory.path().join("copied.vrt");
        assert!(
            copy(source.to_str().unwrap(), target.to_str().unwrap(), &Default::default())
                .unwrap_err()
                .to_string()
                .contains("directory dependency")
        );
        assert!(!target.exists() && !directory.path().join("copied.zarr").exists());
        assert_eq!(std::fs::read(&outside).unwrap(), [1, 2, 3, 4]);
    }

    #[test]
    #[cfg(all(unix, feature = "zarr"))]
    fn committed_operations_report_leftover_backups_when_cleanup_fails() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.zarr");
        let target = directory.path().join("target.zarr");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join(".zgroup"), b"{\"zarr_format\":2}").unwrap();
        std::fs::create_dir_all(target.join("locked")).unwrap();
        std::fs::write(target.join(".zgroup"), b"{\"zarr_format\":2}").unwrap();
        std::fs::write(target.join("locked/old"), b"recover this").unwrap();
        std::fs::set_permissions(target.join("locked"), std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::write(target.join("locked/probe"), b"probe").is_ok() {
            std::fs::set_permissions(target.join("locked"), std::fs::Permissions::from_mode(0o700)).unwrap();
            eprintln!("skipping cleanup-failure check: a privileged runner cannot exercise a failed unlink");
            return;
        }
        let report =
            copy(source.to_str().unwrap(), target.to_str().unwrap(), &FileOptions { overwrite: true, ..Default::default() }).unwrap();
        assert_eq!(report.recovery_files.len(), 1);
        assert!(target.join(".zgroup").exists() && !target.join("locked").exists());
        let backup = std::path::Path::new(&report.recovery_files[0]);
        assert_eq!(std::fs::read(backup.join("locked/old")).unwrap(), b"recover this");
        std::fs::set_permissions(backup.join("locked"), std::fs::Permissions::from_mode(0o700)).unwrap();
        vsi::delete(backup.to_str().unwrap(), true).unwrap();
        std::fs::create_dir_all(target.join("locked")).unwrap();
        std::fs::write(target.join("locked/old"), b"recover deletion").unwrap();
        std::fs::set_permissions(target.join("locked"), std::fs::Permissions::from_mode(0o500)).unwrap();
        let report = delete(target.to_str().unwrap(), None).unwrap();
        assert!(!target.exists());
        assert_eq!(report.recovery_files.len(), 1);
        let backup = std::path::Path::new(&report.recovery_files[0]);
        assert_eq!(std::fs::read(backup.join("locked/old")).unwrap(), b"recover deletion");
        std::fs::set_permissions(backup.join("locked"), std::fs::Permissions::from_mode(0o700)).unwrap();
        vsi::delete(backup.to_str().unwrap(), true).unwrap();
    }
    #[test]
    fn vrt_cycles_fail_before_staging_any_file() {
        let root = directory();
        let source = format!("{root}/source.vrt");
        let other = format!("{root}/source_other.vrt");
        let target = format!("{root}/copied.vrt");
        let definition = |name: &str| {
            format!(
                "<VRTDataset rasterXSize='1' rasterYSize='1'><VRTRasterBand band='1' dataType='Byte'><SimpleSource><SourceFilename relativeToVRT='1'>{name}</SourceFilename></SimpleSource></VRTRasterBand></VRTDataset>"
            )
        };
        put(&source, definition("source.vrt").as_bytes());
        assert!(copy(&source, &target, &Default::default()).unwrap_err().to_string().contains("cyclic"));
        put(&source, definition("source_other.vrt").as_bytes());
        put(&other, definition("source.vrt").as_bytes());
        assert!(rename(&source, &target, &Default::default()).unwrap_err().to_string().contains("cyclic"));
        assert!(exists(&source).unwrap() && exists(&other).unwrap());
        assert!(!exists(&target).unwrap());
        assert_eq!(vsi::list(&root, &Default::default()).unwrap().len(), 2);
        vsi::delete(&root, true).unwrap();
    }
    #[test]
    fn xml_without_rebased_references_preserves_its_original_encoding() {
        let root = directory();
        for (extension, driver, bytes) in [
            ("vrt", "VRT", b"<?xml version='1.0'?>\n<VRTDataset rasterXSize='1' rasterYSize='1'><VRTRasterBand band='1' dataType='Byte'/></VRTDataset>\n".as_slice()),
            ("gml", "GML", b"<?xml version='1.0'?>\n<gml:FeatureCollection xmlns:gml='http://www.opengis.net/gml'>\n</gml:FeatureCollection>\n".as_slice()),
        ] {
            let source = format!("{root}/source.{extension}");
            let target = format!("{root}/copied.{extension}");
            put(&source, bytes);
            assert_eq!(file_list(&source, None).unwrap().driver, driver);
            copy(&source, &target, &Default::default()).unwrap();
            assert_eq!(vsi::read(&target).unwrap(), bytes);
        }
        vsi::delete(&root, true).unwrap();
    }
    #[test]
    fn copy_rename_delete_include_parts_and_preserve_unrelated_siblings() {
        let root = directory();
        let source = csv(&root, "source");
        put(&format!("{root}/source.csvt"), b"Real,Real,String");
        put(&format!("{root}/source.txt"), b"unrelated");
        put(&format!("{root}/source.csv.msk"), b"not a CSV companion");
        let target = format!("{root}/copied.csv");
        let report = copy(&source, &target, &Default::default()).unwrap();
        assert_eq!(report.files.len(), 2);
        assert_eq!(vsi::read(&target).unwrap(), vsi::read(&source).unwrap());
        assert_eq!(vsi::read(&format!("{root}/copied.csvt")).unwrap(), b"Real,Real,String");
        let moved = format!("{root}/moved.csv");
        rename(&target, &moved, &Default::default()).unwrap();
        assert!(!exists(&target).unwrap());
        assert!(!exists(&format!("{root}/copied.csvt")).unwrap());
        delete(&moved, None).unwrap();
        assert!(!exists(&moved).unwrap());
        assert_eq!(vsi::read(&format!("{root}/source.txt")).unwrap(), b"unrelated");
        assert!(!exists(&format!("{root}/copied.csv.msk")).unwrap());
        assert_eq!(vsi::read(&format!("{root}/source.csv.msk")).unwrap(), b"not a CSV companion");
        assert!(exists(&source).unwrap());
        vsi::delete(&root, true).unwrap();
    }
    #[test]
    fn overwrite_removes_stale_companions_and_preflights_all_collisions() {
        let root = directory();
        let source = csv(&root, "source");
        let target = csv(&root, "target");
        put(&format!("{root}/target.csvt"), b"stale");
        assert!(copy(&source, &target, &Default::default()).is_err());
        assert_eq!(vsi::read(&format!("{root}/target.csvt")).unwrap(), b"stale");
        copy(&source, &target, &FileOptions { overwrite: true, ..Default::default() }).unwrap();
        assert!(!exists(&format!("{root}/target.csvt")).unwrap());
        put(&format!("{root}/source.csvt"), b"types");
        put(&format!("{root}/new.csvt"), b"do not overwrite");
        let error = rename(&source, &format!("{root}/new.csv"), &Default::default()).unwrap_err();
        assert!(error.to_string().contains("companion"));
        assert!(exists(&source).unwrap());
        assert!(!exists(&format!("{root}/new.csv")).unwrap());
        assert_eq!(vsi::list(&root, &Default::default()).unwrap().len(), 4);
        vsi::delete(&root, true).unwrap();
    }
    #[test]
    fn vector_directory_with_unrelated_entries_is_refused_before_mutation() {
        let root = directory();
        let source = csv(&root, "source");
        put(&format!("{root}/source.csvt"), b"Real,Real,String");
        assert!(file_list(&root, None).unwrap().directory);
        put(&format!("{root}/README.md"), b"keep documentation");
        put(&format!("{root}/notes/private.txt"), b"keep nested notes");
        let target = format!("{root}-copy");
        for hint in [None, Some("CSV")] {
            assert!(file_list(&root, hint).unwrap_err().to_string().contains("unrelated"));
            assert!(delete(&root, hint).is_err());
        }
        assert!(rename(&root, &target, &Default::default()).is_err());
        assert!(copy(&root, &target, &Default::default()).is_err());
        assert!(exists(&source).unwrap());
        assert_eq!(vsi::read(&format!("{root}/README.md")).unwrap(), b"keep documentation");
        assert_eq!(vsi::read(&format!("{root}/notes/private.txt")).unwrap(), b"keep nested notes");
        assert!(!exists(&target).unwrap());
        vsi::delete(&root, true).unwrap();
    }
    #[test]
    fn failed_overwrite_inventory_preserves_destination_and_companions() {
        let root = directory();
        let source = csv(&root, "source");
        let target = format!("{root}/old.tif");
        let sidecar = format!("{target}.aux.xml");
        for bytes in [b"unrecognized".as_slice(), b"II*\0\x08\0\0\0truncated IFD".as_slice()] {
            put(&target, bytes);
            put(&sidecar, b"old metadata");
            assert!(copy(&source, &target, &FileOptions { overwrite: true, ..Default::default() }).is_err());
            assert_eq!(vsi::read(&target).unwrap(), bytes);
            assert_eq!(vsi::read(&sidecar).unwrap(), b"old metadata");
        }
        vsi::delete(&root, true).unwrap();
    }
    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn shared_basename_companions_require_all_owners_before_deletion() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_str().unwrap();
        let csv = csv(root, "roads");
        let shp = format!("{root}/roads.shp");
        let prj = format!("{root}/roads.prj");
        let mut layer = crate::Layer::new("roads");
        layer.add_feature(Some(wbvector::Geometry::point(1., 2.)), &[]).unwrap();
        crate::drivers::vector::shapefile::write(&layer, &shp).unwrap();
        put(&prj, b"shared projection");
        assert!(delete(&csv, None).unwrap_err().to_string().contains("shared"));
        assert!(rename(&csv, &format!("{root}/moved.csv"), &Default::default()).is_err());
        let replacement = self::csv(root, "replacement");
        assert!(copy(&replacement, &csv, &FileOptions { overwrite: true, ..Default::default() }).is_err());
        assert_eq!(vsi::read(&prj).unwrap(), b"shared projection");
        assert!(exists(&csv).unwrap() && exists(&shp).unwrap());
        put(&format!("{root}/replacement.prj"), b"replacement projection");
        std::fs::remove_file(&csv).unwrap();
        let error = copy(&replacement, &csv, &FileOptions { overwrite: true, ..Default::default() }).unwrap_err();
        assert!(error.to_string().contains("shared"));
        assert!(!exists(&csv).unwrap());
        assert_eq!(vsi::read(&prj).unwrap(), b"shared projection");
        assert!(exists(&shp).unwrap());
        self::csv(root, "roads");
        delete_many(&[csv.clone(), shp.clone()], None).unwrap();
        assert!(!exists(&csv).unwrap() && !exists(&shp).unwrap() && !exists(&prj).unwrap());
        assert!(exists(&replacement).unwrap());
    }
    #[test]
    #[cfg(all(not(target_family = "wasm"), feature = "geotiff"))]
    fn shared_rpc_and_world_files_require_every_raster_owner() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().to_str().unwrap();
        let first = format!("{root}/scene.tif");
        let second = format!("{root}/scene.tiff");
        let file = std::fs::File::create(&first).unwrap();
        tiff::encoder::TiffEncoder::new(file).unwrap().write_image::<tiff::encoder::colortype::Gray8>(2, 2, &[1; 4]).unwrap();
        std::fs::copy(&first, &second).unwrap();
        for name in ["scene.RPB", "scene.tfw"] {
            let companion = format!("{root}/{name}");
            put(&companion, b"shared bytes");
            for source in [&first, &second] {
                let files = file_list(source, None).unwrap().files;
                assert!(files.iter().any(|file| location::same_name(file, &companion).unwrap()), "{companion}: {files:?}");
            }
            assert!(delete(&first, None).unwrap_err().to_string().contains("shared"));
            assert!(rename(&first, &format!("{root}/moved.tif"), &Default::default()).is_err());
            assert_eq!(vsi::read(&companion).unwrap(), b"shared bytes");
            std::fs::remove_file(&companion).unwrap();
        }
        put(&format!("{root}/scene.RPB"), b"shared bytes");
        delete_many(&[first, second], None).unwrap();
        assert!(!std::path::Path::new(&format!("{root}/scene.RPB")).exists());
    }

    #[test]
    fn gml_unowned_relative_schema_is_refused_but_absolute_uri_is_preserved() {
        let root = directory();
        let source = format!("{root}/source.gml");
        let target = format!("{root}/copied.gml");
        let definition = |schema: &str| {
            format!(
                "<?xml version='1.0'?><gml:FeatureCollection xmlns:gml='http://www.opengis.net/gml' xmlns:xsi='http://www.w3.org/2001/XMLSchema-instance' xsi:noNamespaceSchemaLocation='{schema}'/>"
            )
        };
        put(&source, definition("../shared.xsd").as_bytes());
        let before = vsi::read(&source).unwrap();
        assert!(copy(&source, &target, &Default::default()).is_err());
        assert!(!exists(&target).unwrap());
        assert_eq!(vsi::read(&source).unwrap(), before);
        put(&source, definition("https://example.invalid/shared.xsd").as_bytes());
        copy(&source, &target, &Default::default()).unwrap();
        assert_eq!(vsi::read(&target).unwrap(), vsi::read(&source).unwrap());
        vsi::delete(&root, true).unwrap();
    }

    #[test]
    fn companion_directory_is_not_removed_by_overwrite() {
        let root = directory();
        let source = csv(&root, "source");
        put(&format!("{root}/source.csvt"), b"Real,Real,String");
        put(&format!("{root}/target.csvt/keep"), b"unrelated directory");
        let error = copy(&source, &format!("{root}/target.csv"), &FileOptions { overwrite: true, ..Default::default() }).unwrap_err();
        assert!(error.to_string().contains("directory"));
        assert!(!exists(&format!("{root}/target.csv")).unwrap());
        assert_eq!(vsi::read(&format!("{root}/target.csvt/keep")).unwrap(), b"unrelated directory");
        vsi::delete(&root, true).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn late_copy_failure_keeps_old_destination_and_removes_staged_data() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        let sidecar = source.with_extension("csvt");
        let target = directory.path().join("target.csv");
        std::fs::write(&source, b"x,y\n1,2\n").unwrap();
        std::fs::write(&sidecar, b"Real,Real").unwrap();
        std::fs::write(&target, b"original destination").unwrap();
        std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&sidecar).is_ok() {
            eprintln!("skipping permission-failure check: a privileged test runner bypasses these permissions");
            return;
        }
        let error =
            copy(source.to_str().unwrap(), target.to_str().unwrap(), &FileOptions { overwrite: true, ..Default::default() }).unwrap_err();
        assert!(error.to_string().to_ascii_lowercase().contains("permission"));
        assert_eq!(std::fs::read(&target).unwrap(), b"original destination");
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 3);
        assert_eq!(std::fs::read(&source).unwrap(), b"x,y\n1,2\n");
    }
    #[test]
    #[cfg(feature = "zarr")]
    fn directories_keep_chunks_but_unknown_and_dangerous_roots_fail() {
        let root = directory();
        let source = format!("{root}/a.zarr");
        put(&format!("{source}/.zgroup"), b"{\"zarr_format\":2}");
        put(&format!("{source}/v/0"), b"chunk");
        let target = format!("{root}/b.zarr");
        copy(&source, &target, &Default::default()).unwrap();
        assert_eq!(vsi::read(&format!("{target}/v/0")).unwrap(), b"chunk");
        assert!(rename(&source, &format!("{source}/inside"), &Default::default()).is_err());
        assert!(delete("/vsimem", None).is_err());
        assert!(delete(&root, None).is_err());
        delete(&target, None).unwrap();
        assert!(exists(&source).unwrap());
        vsi::delete(&root, true).unwrap();
    }
    #[test]
    fn batch_delete_preflights_missing_inputs_and_deduplicates_file_sets() {
        let root = directory();
        let source = csv(&root, "source");
        let other = csv(&root, "other");
        assert!(delete_many(&[source.clone(), format!("{root}/missing.csv")], None).is_err());
        assert!(exists(&source).unwrap());
        let reports = delete_many(&[source.clone(), source, other], None).unwrap();
        assert_eq!(reports.len(), 3);
        assert!(!exists(&root).unwrap());
    }
    #[test]
    fn nested_mvt_text_does_not_make_a_directory_mvt() {
        let root = directory();
        let grid = format!("{root}/grid");
        put(&format!("{grid}/metadata.json"), br#"{"encoding":{"format":"pbf"},"notes":{"vector_layers":[]}}"#);
        put(&format!("{grid}/hdr.adf"), b"header");
        put(&format!("{grid}/w001001.adf"), b"tile");
        assert_eq!(file_list(&grid, None).unwrap().driver, "AIG");
        vsi::delete(&root, true).unwrap();
    }
    #[test]
    #[cfg(feature = "tiles")]
    fn mvt_directories_are_preserved_and_invalid_store_markers_are_not_datasets() {
        let root = directory();
        let source = format!("{root}/source");
        put(&format!("{source}/metadata.json"), br#"{"format":"pbf","vector_layers":[]}"#);
        put(&format!("{source}/0/0/0.pbf"), b"encoded tile");
        let target = format!("{root}/copied");
        assert_eq!(copy(&source, &target, &Default::default()).unwrap().driver, "MVT");
        assert_eq!(vsi::read(&format!("{target}/0/0/0.pbf")).unwrap(), b"encoded tile");
        put(&format!("{source}/keep.txt"), b"unrelated user file");
        assert!(delete(&source, None).is_err());
        assert_eq!(vsi::read(&format!("{source}/keep.txt")).unwrap(), b"unrelated user file");
        assert!(copy(&source, &format!("{root}/refused"), &Default::default()).is_err());
        assert!(rename(&source, &format!("{root}/refused"), &Default::default()).is_err());
        let invalid = format!("{root}/not-a-store");
        put(&format!("{invalid}/.zgroup"), br#"{"unrelated":true}"#);
        put(&format!("{invalid}/keep.txt"), b"keep");
        assert!(delete(&invalid, None).is_err());
        assert_eq!(vsi::read(&format!("{invalid}/keep.txt")).unwrap(), b"keep");
        vsi::delete(&root, true).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn nonregular_inputs_fail_before_opening_and_symlink_deletion_keeps_targets() {
        use std::os::unix::{fs::symlink, net::UnixListener};
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("data.csv");
        let _listener = UnixListener::bind(&socket).unwrap();
        assert!(file_list(socket.to_str().unwrap(), None).unwrap_err().to_string().contains("regular"));
        let source = directory.path().join("real.csv");
        std::fs::write(&source, b"x,y\n1,2\n").unwrap();
        let alias = directory.path().join("alias.csv");
        symlink("real.csv", &alias).unwrap();
        delete(alias.to_str().unwrap(), None).unwrap();
        assert!(!alias.exists());
        assert_eq!(std::fs::read(&source).unwrap(), b"x,y\n1,2\n");
        let store = directory.path().join("real.zarr");
        std::fs::create_dir(&store).unwrap();
        std::fs::write(store.join(".zgroup"), br#"{"zarr_format":2}"#).unwrap();
        let alias = directory.path().join("alias.zarr");
        symlink("real.zarr", &alias).unwrap();
        delete(alias.to_str().unwrap(), None).unwrap();
        assert!(store.join(".zgroup").is_file());
    }
    #[test]
    fn hints_restrict_drivers_and_json_redacts_credentials() {
        let root = directory();
        let path = csv(&root, "input");
        assert_eq!(file_list(&path, Some("CSV")).unwrap().driver, "CSV");
        assert!(file_list(&path, Some("GTiff")).is_err());
        assert!(file_list(&path, Some("nonsense")).is_err());
        let report = FileReport {
            driver: "CSV".into(),
            files: vec!["https://example.invalid/a.csv?X-Amz-Signature=private".into()],
            bytes: 0,
            recovery_files: vec!["https://example.invalid/staged?X-Amz-Signature=private".into()],
        };
        assert!(!serde_json::to_string(&report).unwrap().contains("private"));
        assert!(!location::same_name("https://example.invalid/source.tif?band=1", "https://example.invalid/source.tif?band=2").unwrap());
        vsi::delete(&root, true).unwrap();
    }
    #[test]
    fn partially_staged_transfer_restores_sources_and_destinations() {
        let root = directory();
        let source = csv(&root, "source");
        let staged = format!("{root}/stage.csv");
        location::rename(&source, &staged).unwrap();
        let mut items = vec![Item {
            source: source.clone(),
            target: format!("{root}/target.csv"),
            staged: staged.clone(),
            installed: false,
            rewritten: None,
            original: None,
            directory: false,
            attempted: true,
            copy_dependency: false,
        }];
        assert!(rollback(&mut items, &[], true).is_empty());
        assert!(exists(&source).unwrap());
        assert!(!exists(&staged).unwrap());
        vsi::delete(&root, true).unwrap();
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn native_aliases_and_source_sidecar_collisions_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        std::fs::write(&source, b"x,y\n1,2\n").unwrap();
        let target = directory.path().join("alias.csv");
        std::fs::hard_link(&source, &target).unwrap();
        let options = FileOptions { overwrite: true, ..Default::default() };
        assert!(copy(source.to_str().unwrap(), target.to_str().unwrap(), &options).unwrap_err().to_string().contains("overlap"));
        assert_eq!(std::fs::read(&source).unwrap(), b"x,y\n1,2\n");
        let target = source.with_extension("csvt");
        std::fs::write(&target, b"Real,Real").unwrap();
        assert!(rename(source.to_str().unwrap(), target.to_str().unwrap(), &options).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"Real,Real");
    }
    #[test]
    fn vrt_owned_files_and_delete_do_not_open_missing_references() {
        let root = directory();
        let source = format!("{root}/broken.vrt");
        put(&source, br#"<VRTDataset rasterXSize="1" rasterYSize="1"><VRTRasterBand dataType="Byte" band="1"><SimpleSource><SourceFilename relativeToVRT="1">missing.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>"#);
        assert_eq!(file_list(&source, None).unwrap().files.as_slice(), std::slice::from_ref(&source));
        assert!(copy(&source, &format!("{root}/copy.vrt"), &Default::default()).is_err());
        assert!(exists(&source).unwrap());
        delete(&source, None).unwrap();
        assert!(!exists(&source).unwrap());
    }
    #[test]
    fn raw_vrt_sources_cdata_entities_and_metadata_are_preserved() {
        let root = directory();
        let source = format!("{root}/source.vrt");
        put(&format!("{root}/source_one&two.bin"), &[1, 2, 3, 4]);
        put(&source, br#"<VRTDataset rasterXSize="2" rasterYSize="2"><Metadata domain="xml:notes"><SourceFilename>not-an-input</SourceFilename></Metadata><VRTRasterBand dataType="Byte" band="1" subClass="VRTRawRasterBand"><SourceFilename relativeToVRT="true"><![CDATA[source_one&two.bin]]></SourceFilename><ImageOffset>0</ImageOffset><PixelOffset>1</PixelOffset><LineOffset>2</LineOffset><ByteOrder>LSB</ByteOrder></VRTRasterBand></VRTDataset>"#);
        let target = format!("{root}/copied.vrt");
        copy(&source, &target, &Default::default()).unwrap();
        assert_eq!(vsi::read(&format!("{root}/copied_one&two.bin")).unwrap(), [1, 2, 3, 4]);
        let text = String::from_utf8(vsi::read(&target).unwrap()).unwrap();
        assert!(text.contains("copied_one&amp;two.bin"));
        assert!(text.contains("not-an-input"));
        vsi::delete(&root, true).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn rollback_restores_a_temporarily_dangling_staged_link() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let original = directory.path().join("original.csv");
        let stage = directory.path().join("stage.csv");
        symlink("missing.csv", &stage).unwrap();
        let mut items = [Item {
            source: original.to_str().unwrap().into(),
            target: directory.path().join("target.csv").to_str().unwrap().into(),
            staged: stage.to_str().unwrap().into(),
            installed: false,
            rewritten: None,
            original: None,
            directory: false,
            attempted: true,
            copy_dependency: false,
        }];
        assert!(rollback(&mut items, &[], true).is_empty());
        assert_eq!(std::fs::read_link(original).unwrap(), std::path::Path::new("missing.csv"));
        assert!(std::fs::symlink_metadata(stage).is_err());
    }
    #[test]
    fn rollback_reports_the_source_when_neither_copy_survives() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source.csv");
        let mut items = [Item {
            source: source.to_str().unwrap().into(),
            target: directory.path().join("target.csv").to_str().unwrap().into(),
            staged: directory.path().join("stage.csv").to_str().unwrap().into(),
            installed: false,
            rewritten: None,
            original: None,
            directory: false,
            attempted: true,
            copy_dependency: false,
        }];
        assert_eq!(rollback(&mut items, &[], true), vec![source.to_str().unwrap().to_string()]);
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn archive_parts_copy_out_and_release_the_archive_handle() {
        use std::io::Write;
        let directory = tempfile::tempdir().unwrap();
        let archive = directory.path().join("source.zip");
        let mut writer = zip::ZipWriter::new(std::fs::File::create(&archive).unwrap());
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, data) in [("source.csv", b"x,y\n1,2\n".as_slice()), ("source.csvt", b"Real,Real".as_slice())] {
            writer.start_file(name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        writer.finish().unwrap();
        let source = format!("/vsizip/{}/source.csv", archive.display());
        let target = directory.path().join("copied.csv");
        copy(&source, target.to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"x,y\n1,2\n");
        assert_eq!(std::fs::read(target.with_extension("csvt")).unwrap(), b"Real,Real");
        std::fs::remove_file(archive).unwrap(); // Also verifies Windows handles were closed.
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn vrt_copy_rebases_sources_but_delete_preserves_referenced_assets() {
        use crate::{DataType, Raster, RasterConfig, RasterDataset};
        use wbraster::raster::RasterData;
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("same.tif");
        let raster = Raster::from_data_native(
            RasterConfig { cols: 2, rows: 2, bands: 1, data_type: DataType::U8, ..Default::default() },
            RasterData::U8(vec![1, 2, 3, 4]),
        )
        .unwrap();
        crate::write_raster_dataset(&RasterDataset::new(raster, false), source.to_str().unwrap(), &Default::default()).unwrap();
        let renamed_binary = directory.path().join("wrong-extension.vrt");
        std::fs::copy(&source, &renamed_binary).unwrap();
        let copied_binary = directory.path().join("binary-copy.tif");
        copy(renamed_binary.to_str().unwrap(), copied_binary.to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), std::fs::read(copied_binary).unwrap());
        let definition = directory.path().join("same.vrt");
        std::fs::write(&definition, "<VRTDataset rasterXSize=\"2\" rasterYSize=\"2\"><VRTRasterBand dataType=\"Byte\" band=\"1\"><SimpleSource><SourceFilename relativeToVRT=\"1\">same.tif</SourceFilename><SourceBand>1</SourceBand></SimpleSource></VRTRasterBand></VRTDataset>").unwrap();
        let other_definition = directory.path().join("another.vrt");
        std::fs::copy(&definition, &other_definition).unwrap();
        let output = directory.path().join("copied.vrt");
        copy(definition.to_str().unwrap(), output.to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(crate::open_raster(output.to_str().unwrap()).unwrap().raster.data.to_f64_vec(), vec![1., 2., 3., 4.]);
        assert!(std::fs::read_to_string(&output).unwrap().contains("copied.tif"));
        delete(output.to_str().unwrap(), None).unwrap();
        assert!(directory.path().join("copied.tif").is_file());
        let renamed = directory.path().join("renamed.vrt");
        rename(definition.to_str().unwrap(), renamed.to_str().unwrap(), &Default::default()).unwrap();
        assert!(source.exists());
        assert_eq!(crate::open_raster(other_definition.to_str().unwrap()).unwrap().raster.data.to_f64_vec(), vec![1., 2., 3., 4.]);
        assert_eq!(crate::open_raster(renamed.to_str().unwrap()).unwrap().raster.data.to_f64_vec(), vec![1., 2., 3., 4.]);
    }
}

// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/libkml/ogrlibkmllayer.cpp
//     ogr/ogrsf_frmts/libkml/ogrlibkmlfield.cpp
//   libkml newest:
//     src/kml/engine/feature_view.cc
// No code is translated from these sources.

//! KML and KMZ, read as GDAL's LIBKML driver reads them.
//!
//! Every Folder or Document below the root is a layer, and the root container is one too when it
//! holds placemarks directly (named after it, or after the file). Fields are LIBKML's: `id`,
//! `Name`, `description`, `timestamp`, `begin`, `end`, `altitudeMode`, `tessellate`, `extrude`,
//! `visibility`, `drawOrder`, `icon`, then the `ExtendedData` fields (typed by their `Schema`).
//! A `MultiGeometry` of one geometry type is a multi-part geometry, and ground overlays are
//! polygons. Writing produces a Document per layer, each with its typed Schema.

use super::in_wgs84;
use super::kml_style;
use super::xml::{self, Element};
use crate::{Error, Result};
use std::collections::HashMap;
use std::fmt::Write as _;
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Ring};

const FIXED: &[(&str, FieldType)] = &[
    ("id", FieldType::Text),
    ("Name", FieldType::Text),
    ("description", FieldType::Text),
    ("timestamp", FieldType::DateTime),
    ("begin", FieldType::DateTime),
    ("end", FieldType::DateTime),
    ("altitudeMode", FieldType::Text),
    ("tessellate", FieldType::Integer),
    ("extrude", FieldType::Integer),
    ("visibility", FieldType::Integer),
    ("drawOrder", FieldType::Integer),
    ("icon", FieldType::Text),
];

/// How many other documents of a KMZ archive its `styleUrl`s may have read for their styles.
const MAX_STYLE_DOCUMENTS: usize = 256;
/// How many decompressed bytes of text those documents may take together. Their parsed form takes
/// several times that, which WASM's memory has to hold too.
const MAX_STYLE_DOCUMENT_BYTES: u64 = 64 << 20;
/// How many decompressed bytes of text a KMZ archive's main document and the layer documents it
/// links to may take together by default (`TERRIO_KMZ_MAX_SIZE`). Reading them takes at least
/// twice that in memory, and a deflated document expands up to 1,032 times its size in the archive.
const DEFAULT_MAX_KMZ_DOCUMENT_BYTES: u64 = 512 << 20;

/// The bytes a KMZ archive's main and layer documents may take together: `TERRIO_KMZ_MAX_SIZE`,
/// else 512 MiB; the budget [`read_document`] starts from. Members are also read within the
/// archive limits (`crate::vfs::ZipLimits`).
pub(crate) fn max_kmz_document_bytes() -> u64 {
    crate::config::get("TERRIO_KMZ_MAX_SIZE").and_then(|v| v.trim().parse().ok()).unwrap_or(DEFAULT_MAX_KMZ_DOCUMENT_BYTES)
}

/// A duplicate-entry refusal is terminal, including for the optional GDAL backend.
pub(crate) const AMBIGUOUS_KMZ: &str = "KMZ: the archive holds several entries with the same name, so its main document is ambiguous";

/// A KML document's layers, and its style table: style ids and their OGR style strings.
pub type Styled = (Vec<Layer>, Vec<(String, String)>);

/// Reads a KML file.
pub fn read(path: &str) -> Result<Vec<Layer>> {
    Ok(read_styled(path)?.0)
}

/// Reads a KML file with its style table.
pub fn read_styled(path: &str) -> Result<Styled> {
    parse_styled(&xml::decode(&std::fs::read(path)?), &stem(path))
}

/// Reads a KMZ file: its main document (its first `.kml` file at the root), and the documents
/// in the archive it links to (GDAL writes one per layer under `layers/`).
/// Without a `.kml` file at the root, its first `.kml` file is the main document. An archive
/// holding two entries of the same name is refused.
pub fn read_kmz(path: &str) -> Result<Vec<Layer>> {
    Ok(read_kmz_styled(path)?.0)
}

/// Reads a KMZ file with its style table.
pub fn read_kmz_styled(path: &str) -> Result<Styled> {
    // Only the archive's directory and the members read are loaded.
    kmz_styled(std::io::BufReader::new(std::fs::File::open(path)?), &stem(path))
}

/// Parses a KMZ archive held in memory. `default_name` names a root layer without a name.
pub fn parse_kmz(bytes: Vec<u8>, default_name: &str) -> Result<Vec<Layer>> {
    Ok(parse_kmz_styled(bytes, default_name)?.0)
}

/// Parses a KMZ archive held in memory, with the style table of its documents.
pub fn parse_kmz_styled(bytes: Vec<u8>, default_name: &str) -> Result<Styled> {
    kmz_styled(std::io::Cursor::new(bytes), default_name)
}

/// The main document of a KMZ archive, its index and name: the first `.kml` entry at the archive's
/// root, in the order of its entries, whatever its name (OGC KML 2.3, Annex C.1: "If more than one
/// KML file exists at the root directory level, the first file entry (in unsorted order) shall be
/// treated as the main KML file"); else, when the root holds none, its first `.kml` entry. A `\`
/// separates folders too, as Windows tools write it.
fn main_document<R: std::io::Read + std::io::Seek>(zip: &zip::ZipArchive<R>) -> Result<(usize, String)> {
    let documents: Vec<(usize, String)> = (0..zip.len())
        .filter_map(|i| match zip.name_for_index(i)? {
            Ok(name) => Some((i, name.into_owned())),
            Err(error) => {
                crate::warnings::warn(format!("KMZ: entry {i} has an undecodable name and is skipped: {error}"));
                None
            }
        })
        .filter(|(_, name)| name.to_ascii_lowercase().ends_with(".kml"))
        .collect();
    let at_root = documents
        .iter()
        .position(|(_, name)| {
            let mut relative = name.as_str();
            while let Some(rest) = relative.strip_prefix("./").or_else(|| relative.strip_prefix(".\\")) {
                relative = rest;
            }
            !relative.contains(['/', '\\'])
        })
        .unwrap_or(0);
    documents.into_iter().nth(at_root).ok_or_else(|| Error::InvalidData("the KMZ archive holds no KML document".into()))
}

/// Reads the KML document named `name` from a KMZ archive within `budget`, the bytes left of
/// `TERRIO_KMZ_MAX_SIZE` for the archive's documents, and takes its bytes from it.
///
/// Args:
///     zip: The archive, read within the archive limits.
///     name: The document's member name.
///     budget: The bytes the archive's documents may still take; reduced by this one's.
///
/// Returns:
///     The document's text, or `Error::ResourceLimit` when it does not fit the budget.
fn read_document<R: std::io::Read + std::io::Seek>(zip: &mut crate::vfs::LimitedZip<R>, name: &str, budget: &mut u64) -> Result<String> {
    let index = zip.archive().index_for_name(name).ok_or_else(|| Error::InvalidData(format!("KMZ: '{name}' is not in the archive")))?;
    read_document_at(zip, index, budget)
}

/// [`read_document`] for the member at `index`.
pub(crate) fn read_document_at<R: std::io::Read + std::io::Seek>(
    zip: &mut crate::vfs::LimitedZip<R>,
    index: usize,
    budget: &mut u64,
) -> Result<String> {
    // A document declaring more than is left is refused before it is read. One inflating past what
    // it declares is refused by `read_index`, so no read goes past the budget.
    let (name, declared, _) = zip.entry(index)?;
    if declared > *budget {
        return Err(Error::ResourceLimit(format!(
            "KMZ: the archive's KML documents hold more than the {} bytes allowed (TERRIO_KMZ_MAX_SIZE), at '{name}'",
            max_kmz_document_bytes()
        )));
    }
    let bytes = zip.read_index(index, *budget)?;
    *budget -= bytes.len() as u64;
    Ok(xml::decode(&bytes))
}

/// [`parse_kmz_styled`] from a reader of the archive: within the archive limits
/// (`crate::vfs::ZipLimits`) and, for the main and layer documents, `TERRIO_KMZ_MAX_SIZE`.
fn kmz_styled<R: std::io::Read + std::io::Seek>(reader: R, default_name: &str) -> Result<Styled> {
    // Preserve the KMZ diagnostic while the shared archive reader enforces uniqueness.
    let mut zip = crate::vfs::LimitedZip::open(reader).map_err(|error| match error {
        Error::InvalidData(message) if message == crate::vfs::AMBIGUOUS_ZIP => Error::InvalidData(AMBIGUOUS_KMZ.into()),
        error => error,
    })?;
    let names: Vec<String> = zip.archive().file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect();
    let (main_index, main) = main_document(zip.archive())?;
    let mut documents_budget = max_kmz_document_bytes();
    let root = xml::parse_document(&read_document_at(&mut zip, main_index, &mut documents_budget)?, "kml", "KML")?;
    // The documents that are layers, each with its layer name.
    let mut links = Vec::new();
    root.find_all("NetworkLink", &mut links);
    let archived: std::collections::HashSet<&str> = names.iter().map(String::as_str).collect();
    // Each document is read, counted toward the budget and parsed once, however many links name
    // it. Its first link supplies the default name; document and folder names are kept.
    // The main document is not read again.
    let mut linked = std::collections::HashSet::from([main.as_str()]);
    let mut documents: Vec<(String, Element, String)> = Vec::new();
    for link in links {
        let Some(href) = link.child("Link").or_else(|| link.child("Url")).and_then(|l| l.child_text("href")) else { continue };
        let href = href.trim_start_matches("./");
        if !archived.contains(href) || !linked.insert(href) {
            continue;
        }
        let name = link.child_text("name").map_or_else(|| stem(href), str::to_string);
        documents.push((
            href.to_string(),
            xml::parse_document(&read_document(&mut zip, href, &mut documents_budget)?, "kml", "KML")?,
            name,
        ));
    }
    // The documents styles live in, as far as links from one to the next go. Each is read once here
    // (a layer document a styleUrl names was also read above, and counts again toward
    // `TERRIO_ZIP_MAX_SIZE`, though not toward the document budget), and
    // one that cannot be read, or past the style budget, leaves its links as raw URLs. An archive
    // past the archive limits is refused.
    let mut budget = MAX_STYLE_DOCUMENT_BYTES;
    let mut members = kml_style::Members::new();
    let mut pending = kml_style::linked_members(&root, &main);
    for (member, document, _) in &documents {
        pending.extend(kml_style::linked_members(document, member));
    }
    while let Some(name) = pending.pop() {
        if members.contains_key(&name) || !archived.contains(name.as_str()) || members.len() >= MAX_STYLE_DOCUMENTS || budget == 0 {
            continue;
        }
        // One byte past the budget tells a document that does not fit from one that just does.
        let text = match zip.read_name(&name, budget) {
            Ok(bytes) => xml::decode(&bytes),
            Err(e) if e.is_resource_refusal() => return Err(e),
            // A member inflating past its declared size is refused only by the message
            // `is_past_declared` matches. If zip rewords it, that member is skipped here like an
            // unreadable one, not refused; the vfs tests pin the message.
            Err(_) => continue,
        };
        if text.len() as u64 > budget {
            budget = 0;
            continue;
        }
        budget -= text.len() as u64;
        let Ok(document) = xml::parse(&text) else { continue };
        pending.extend(kml_style::linked_members(&document, &name));
        members.insert(name, document);
    }
    let index = kml_style::index_members(&members);
    let mut table = Vec::new();
    let mut known = HashMap::new();
    let mut layers = layers_of(&root, &main, &index, default_name, false, &mut table, &mut known);
    for (member, document, name) in &documents {
        layers.extend(layers_of(document, member, &index, name, true, &mut table, &mut known));
    }
    make_layer_names_unique(&mut layers);
    Ok((layers, table))
}

fn stem(path: &str) -> String {
    std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("layer").to_string()
}

fn is_container(e: &Element) -> bool {
    e.name == "Folder" || e.name == "Document"
}

fn is_feature(e: &Element) -> bool {
    e.name == "Placemark" || e.name == "GroundOverlay"
}

/// Parses KML text. `default_name` names a root layer without a name.
pub fn parse(text: &str, default_name: &str) -> Result<Vec<Layer>> {
    Ok(parse_styled(text, default_name)?.0)
}

/// Parses KML text, with its style table.
pub fn parse_styled(text: &str, default_name: &str) -> Result<Styled> {
    let root = xml::parse_document(text, "kml", "KML")?;
    let mut table = Vec::new();
    let mut known = HashMap::new();
    let mut layers = layers_of(&root, "", &kml_style::MemberIndex::new(), default_name, false, &mut table, &mut known);
    make_layer_names_unique(&mut layers);
    Ok((layers, table))
}

/// The layers of a document, adding its styles to `table`. `member` is its name in a KMZ archive
/// holding the documents `members`. With `root_layer`, the root container is a layer even without
/// placemarks of its own (a layer document linked from a KMZ's `doc.kml`).
fn layers_of(
    root: &Element,
    member: &str,
    members: &kml_style::MemberIndex<'_>,
    default_name: &str,
    root_layer: bool,
    table: &mut Vec<(String, String)>,
    known: &mut HashMap<String, String>,
) -> Vec<Layer> {
    // The schemas, anywhere in the document, by id and by name.
    let mut schemas = Vec::new();
    collect_schemas(root, &mut schemas);

    let mut containers: Vec<(String, Vec<&Element>, &Element)> = Vec::new();
    let top = root.children.iter().find(|c| is_container(c));
    match top {
        Some(top) => {
            let direct: Vec<&Element> = top.children.iter().filter(|c| is_feature(c)).collect();
            if !direct.is_empty() || root_layer {
                containers.push((top.child_text("name").unwrap_or(default_name).to_string(), direct, top));
            }
            collect_containers(top, &mut containers);
        }
        None => {
            let direct: Vec<&Element> = root.children.iter().filter(|c| is_feature(c)).collect();
            containers.push((default_name.to_string(), direct, root));
        }
    }
    let mut styles = kml_style::Styles::new(root, member, members);
    styles.add_to(table, known);
    containers
        .into_iter()
        .map(|(name, features, container)| {
            let mut l = layer(&name, &features, &schemas, &styles);
            l.extent = container_bounds(container);
            l
        })
        .collect()
}

/// Make names unique across a dataset: combined KMZ documents or one plain KML document.
/// ASCII case variants collide as they do for SQLite/GeoPackage table names.
fn make_layer_names_unique(layers: &mut [Layer]) {
    // Layer names must be unique: a name an earlier layer has takes the first free suffix, ` (2)`,
    // ` (3)`, and so on. Names are only ever added, so the suffixes a name found taken stay taken,
    // and its next search resumes after the last suffix it took.
    let mut taken = std::collections::HashSet::with_capacity(layers.len());
    let mut next: HashMap<String, usize> = HashMap::new();
    for layer in layers {
        let key = layer.name.to_ascii_lowercase();
        if taken.insert(key.clone()) {
            continue;
        }
        let n = next.entry(key).or_insert(2);
        layer.name = loop {
            let name = format!("{} ({n})", layer.name);
            *n += 1;
            if taken.insert(name.to_ascii_lowercase()) {
                break name;
            }
        };
    }
}

fn collect_schemas<'a>(e: &'a Element, out: &mut Vec<&'a Element>) {
    for c in &e.children {
        if c.name == "Schema" {
            out.push(c);
        } else {
            collect_schemas(c, out);
        }
    }
}

/// Every container below `e`, depth first, with its own features.
fn collect_containers<'a>(e: &'a Element, out: &mut Vec<(String, Vec<&'a Element>, &'a Element)>) {
    for c in e.children.iter().filter(|c| is_container(c)) {
        let features = c.children.iter().filter(|f| is_feature(f)).collect();
        out.push((c.child_text("name").unwrap_or(&c.name).to_string(), features, c));
        collect_containers(c, out);
    }
}

/// A container's bounds as libkml's `GetFeatureBounds` computes them, which LIBKML reports as
/// the layer's extent: the placemarks and ground overlays of the container and of every container
/// nested in it (whose features are layers of their own). `None` when nothing has bounds.
fn container_bounds(e: &Element) -> Option<wbvector::BBox> {
    let boxes = e.children.iter().filter_map(|c| {
        let b = if is_container(c) {
            container_bounds(c)
        } else if c.name == "Placemark" {
            c.children.iter().find(|g| is_geometry(g)).and_then(geometry).and_then(|g| g.bbox())
        } else if c.name == "GroundOverlay" {
            ground_overlay(c).and_then(|g| g.bbox())
        } else {
            None
        }?;
        Some((b.min_x, b.min_y, b.max_x, b.max_y))
    });
    crate::vector::extent_of(boxes).map(|(min_x, min_y, max_x, max_y)| wbvector::BBox { min_x, min_y, max_x, max_y })
}

fn simple_field_type(t: &str) -> FieldType {
    match t.to_ascii_lowercase().as_str() {
        "int" | "uint" | "short" | "ushort" | "bool" => FieldType::Integer,
        "float" | "double" => FieldType::Float,
        _ => FieldType::Text,
    }
}

/// Adds an ExtendedData field unless it is there already, renaming it as LIBKML does when its
/// name differs only in case from another field's (`NAME` next to `Name` becomes `NAME2`).
fn add_data_field(layer: &mut Layer, renames: &mut Vec<(String, String)>, name: &str, t: FieldType) {
    if renames.iter().any(|(k, _)| k == name) {
        return;
    }
    let mut field = name.to_string();
    let mut n = 2;
    while layer.schema.fields().iter().any(|f| f.name.eq_ignore_ascii_case(&field)) {
        field = format!("{name}{n}");
        n += 1;
    }
    layer.add_field(FieldDef::new(&field, t));
    renames.push((name.to_string(), field));
}

fn layer(name: &str, features: &[&Element], schemas: &[&Element], styles: &kml_style::Styles<'_>) -> Layer {
    let mut layer = Layer::new(name).with_epsg(4326);
    for (n, t) in FIXED {
        layer.add_field(FieldDef::new(*n, *t));
    }
    let mut renames: Vec<(String, String)> = Vec::new();
    // ExtendedData fields: typed by the schemas the features use, then untyped Data, in order.
    for f in features {
        let Some(ext) = f.child("ExtendedData") else { continue };
        for sd in ext.children_named("SchemaData") {
            let url = sd.attr("schemaUrl").unwrap_or_default().trim_start_matches('#');
            let schema = schemas.iter().find(|s| s.attr("id") == Some(url) || s.attr("name") == Some(url));
            if let Some(schema) = schema {
                for sf in schema.children_named("SimpleField") {
                    if let Some(n) = sf.attr("name") {
                        add_data_field(&mut layer, &mut renames, n, simple_field_type(sf.attr("type").unwrap_or("string")));
                    }
                }
            }
            for d in sd.children_named("SimpleData") {
                if let Some(n) = d.attr("name") {
                    add_data_field(&mut layer, &mut renames, n, FieldType::Text);
                }
            }
        }
        for d in ext.children_named("Data") {
            if let Some(n) = d.attr("name") {
                add_data_field(&mut layer, &mut renames, n, FieldType::Text);
            }
        }
    }
    // Feature styles, in an OGR_STYLE field when any feature has one.
    let feature_styles: Vec<Option<String>> = features.iter().map(|f| styles.of_feature(f)).collect();
    if feature_styles.iter().any(Option::is_some) && layer.schema.field_index(kml_style::FIELD).is_none() {
        layer.add_field(FieldDef::new(kml_style::FIELD, FieldType::Text));
    }
    let types: Vec<FieldType> = layer.schema.fields().iter().map(|f| f.field_type).collect();
    for (fid, f) in features.iter().enumerate() {
        let mut attrs = vec![FieldValue::Null; types.len()];
        let mut set = |name: &str, v: Option<&str>| {
            let (Some(i), Some(v)) = (layer.schema.field_index(name), v) else { return };
            let v = v.trim();
            attrs[i] = match types[i] {
                FieldType::Integer => v.parse::<f64>().map_or(FieldValue::Null, |x| FieldValue::Integer(x as i64)),
                FieldType::Float => v.parse().map_or(FieldValue::Null, FieldValue::Float),
                FieldType::DateTime => FieldValue::DateTime(v.to_string()),
                _ => FieldValue::Text(v.to_string()),
            };
        };
        set("id", f.attr("id"));
        set("Name", f.child("name").map(|n| n.text.as_str()));
        set("description", f.child("description").map(|n| n.text.as_str()));
        set("timestamp", f.child("TimeStamp").and_then(|t| t.child_text("when")));
        set("begin", f.child("TimeSpan").and_then(|t| t.child_text("begin")));
        set("end", f.child("TimeSpan").and_then(|t| t.child_text("end")));
        set("visibility", Some(f.child_text("visibility").unwrap_or("-1")));
        let geometry = if f.name == "GroundOverlay" {
            set("altitudeMode", f.child_text("altitudeMode"));
            set("drawOrder", f.child_text("drawOrder"));
            set("icon", f.child("Icon").and_then(|i| i.child_text("href")));
            ground_overlay(f)
        } else {
            let g = f.children.iter().find(|c| is_geometry(c));
            if let Some(g) = g {
                // The first simple geometry carries the flags.
                let simple = if g.name == "MultiGeometry" { g.children.iter().find(|c| is_geometry(c)).unwrap_or(g) } else { g };
                set("altitudeMode", simple.child_text("altitudeMode"));
                set("tessellate", Some(simple.child_text("tessellate").unwrap_or("-1")));
                set("extrude", Some(simple.child_text("extrude").unwrap_or("0")));
            }
            g.and_then(geometry)
        };
        if let Some(ext) = f.child("ExtendedData") {
            let field = |n: Option<&str>| renames.iter().find(|(k, _)| Some(k.as_str()) == n).map(|(_, v)| v.as_str()).unwrap_or_default();
            for d in ext.children_named("SchemaData").flat_map(|sd| sd.children_named("SimpleData")) {
                set(field(d.attr("name")), Some(&d.text));
            }
            for d in ext.children_named("Data") {
                set(field(d.attr("name")), d.child("value").map(|v| v.text.as_str()));
            }
        }
        set(kml_style::FIELD, feature_styles[fid].as_deref());
        layer.push(Feature { fid: fid as u64, geometry, attributes: attrs });
    }
    layer
}

fn is_geometry(e: &Element) -> bool {
    matches!(e.name.as_str(), "Point" | "LineString" | "LinearRing" | "Polygon" | "MultiGeometry" | "Track" | "MultiTrack")
}

fn coords(e: Option<&Element>) -> Vec<Coord> {
    let Some(e) = e else { return Vec::new() };
    // Tuples are separated by whitespace; tolerate spaces after the commas inside them.
    let text = e.text.replace(", ", ",");
    text.split_whitespace()
        .filter_map(|t| {
            let mut n = t.split(',').filter(|s| !s.is_empty()).map(|s| s.parse::<f64>());
            let x = n.next()?.ok()?;
            let y = n.next()?.ok()?;
            let z = n.next().and_then(|z| z.ok());
            Some(Coord { x, y, z, m: None })
        })
        .collect()
}

fn ring(e: Option<&Element>) -> Option<Ring> {
    let mut cs = coords(e?.child("LinearRing")?.child("coordinates"));
    if cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
        cs.pop();
    }
    Some(Ring::new(cs))
}

fn geometry(e: &Element) -> Option<Geometry> {
    Some(match e.name.as_str() {
        "Point" => Geometry::Point(coords(e.child("coordinates")).into_iter().next()?),
        "LineString" | "LinearRing" => Geometry::LineString(coords(e.child("coordinates"))),
        "Polygon" => Geometry::Polygon {
            exterior: ring(e.child("outerBoundaryIs"))?,
            interiors: e.children_named("innerBoundaryIs").filter_map(|i| ring(Some(i))).collect(),
        },
        // gx:Track: its gx:coord elements ("lon lat alt").
        "Track" => Geometry::LineString(
            e.children_named("coord")
                .filter_map(|c| {
                    let n: Vec<f64> = c.text.split_whitespace().filter_map(|v| v.parse().ok()).collect();
                    (n.len() >= 2).then(|| Coord { x: n[0], y: n[1], z: n.get(2).copied(), m: None })
                })
                .collect(),
        ),
        "MultiTrack" => Geometry::MultiLineString(
            e.children_named("Track")
                .filter_map(geometry)
                .filter_map(|g| match g {
                    Geometry::LineString(cs) => Some(cs),
                    _ => None,
                })
                .collect(),
        ),
        "MultiGeometry" => {
            let parts: Vec<Geometry> = e.children.iter().filter(|c| is_geometry(c)).filter_map(geometry).collect();
            let all = |t: GeometryType| !parts.is_empty() && parts.iter().all(|p| p.geom_type() == t);
            if all(GeometryType::Point) {
                Geometry::MultiPoint(parts.into_iter().filter_map(|p| if let Geometry::Point(c) = p { Some(c) } else { None }).collect())
            } else if all(GeometryType::LineString) {
                Geometry::MultiLineString(
                    parts.into_iter().filter_map(|p| if let Geometry::LineString(c) = p { Some(c) } else { None }).collect(),
                )
            } else if all(GeometryType::Polygon) {
                Geometry::MultiPolygon(
                    parts
                        .into_iter()
                        .filter_map(|p| if let Geometry::Polygon { exterior, interiors } = p { Some((exterior, interiors)) } else { None })
                        .collect(),
                )
            } else {
                Geometry::GeometryCollection(parts)
            }
        }
        _ => return None,
    })
}

/// A ground overlay's footprint: its LatLonBox, or its gx:LatLonQuad.
fn ground_overlay(e: &Element) -> Option<Geometry> {
    if let Some(q) = e.child("LatLonQuad") {
        return Some(Geometry::Polygon { exterior: Ring::new(coords(q.child("coordinates"))), interiors: vec![] });
    }
    let b = e.child("LatLonBox")?;
    let n = |k: &str| b.child_text(k).and_then(|v| v.parse::<f64>().ok());
    let (north, south, east, west) = (n("north")?, n("south")?, n("east")?, n("west")?);
    let ring = vec![Coord::xy(west, south), Coord::xy(east, south), Coord::xy(east, north), Coord::xy(west, north)];
    Some(Geometry::Polygon { exterior: Ring::new(ring), interiors: vec![] })
}

/// Writes layers as a KML document: a Document per layer, with its typed Schema, and a shared
/// `<Style id>` for each entry of a style table.
pub fn write_styled(layers: &[Layer], styles: &[(String, String)], path: &str) -> Result<()> {
    std::fs::write(path, to_string_styled(layers, styles)?)?;
    Ok(())
}

/// Options for writing KMZ.
#[derive(Debug, Clone, Default)]
pub struct KmzOptions {
    /// Files packed into the archive, such as icon images, each by the relative path the styles
    /// refer to it with (the `id` of `SYMBOL(id:"icons/pin.png")`). They are stored beside the
    /// layer documents, against which KML resolves relative links, so they travel with the
    /// archive.
    pub resources: Vec<(String, Vec<u8>)>,
}

/// Writes layers as a KMZ archive laid out as GDAL writes it: a `doc.kml` linking to one
/// document per layer under `layers/`, with the files of `kmz` beside those documents, and a shared
/// `<Style id>` for each entry of a style table.
pub fn write_kmz_styled(layers: &[Layer], styles: &[(String, String)], path: &str, kmz: &KmzOptions) -> Result<()> {
    std::fs::write(path, kmz_bytes_styled(layers, styles, kmz)?)?;
    Ok(())
}

/// Encodes layers as a KMZ archive. `doc.kml` and `style/style.kml` hold the whole style table, and
/// each layer document the styles its placemarks refer to, since a `#id` resolves within its own
/// document.
pub fn kmz_bytes_styled(layers: &[Layer], styles: &[(String, String)], kmz: &KmzOptions) -> Result<Vec<u8>> {
    let resources = resource_members(&kmz.resources)?;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let mut doc = String::from(HEADER);
        doc.push_str("<Document id=\"root_doc\">\n");
        doc.push_str(&kml_style::definitions(styles, |_| true));
        let mut files: Vec<(String, String)> = Vec::new();
        for l in layers {
            let mut file = format!("layers/{}.kml", schema_id(l));
            let mut n = 2;
            while files.iter().any(|(f, _)| *f == file) {
                file = format!("layers/{}_{n}.kml", schema_id(l));
                n += 1;
            }
            let _ = writeln!(doc, "  <NetworkLink><name>{}</name><Link><href>{file}</href></Link></NetworkLink>", xml::escape(&l.name));
            let mut layer_doc = String::from(HEADER);
            let _ = writeln!(layer_doc, "<Document id=\"{}\">\n<name>{}</name>", xml::escape(&schema_id(l)), xml::escape(&l.name));
            let l = in_wgs84(l)?;
            let used = referenced_styles(&l);
            layer_doc.push_str(&kml_style::definitions(styles, |id| used.contains(id)));
            layer_doc.push_str(&schema(&l));
            layer_doc.push_str(&placemarks(&l));
            layer_doc.push_str("</Document>\n</kml>\n");
            files.push((file, layer_doc));
        }
        doc.push_str("</Document>\n</kml>\n");
        // KML readers (libkml among them) take the archive's first document as the main one.
        zip.start_file("doc.kml", options)?;
        std::io::Write::write_all(&mut zip, doc.as_bytes())?;
        // Where GDAL's LIBKML driver keeps a KMZ's style table.
        if !styles.is_empty() {
            let text = format!("{HEADER}<Document>\n{}</Document>\n</kml>\n", kml_style::definitions(styles, |_| true));
            zip.start_file("style/style.kml", options)?;
            std::io::Write::write_all(&mut zip, text.as_bytes())?;
        }
        for (file, text) in &files {
            zip.start_file(file.as_str(), options)?;
            std::io::Write::write_all(&mut zip, text.as_bytes())?;
        }
        for (path, member, bytes) in &resources {
            if files.iter().any(|(file, _)| file == member) {
                return Err(Error::InvalidArgument(format!("the KMZ resource '{path}' would replace a layer document")));
            }
            zip.start_file(member.as_str(), options)?;
            std::io::Write::write_all(&mut zip, bytes)?;
        }
        zip.finish()?;
    }
    Ok(buf.into_inner())
}

/// Where each KMZ resource goes: under `layers/`, beside the documents whose relative links
/// resolve to it. Only relative paths inside the archive are taken, each once, and none with
/// characters a viewer reads as part of a URI (`?`, `#`, `%`) rather than of the member name.
fn resource_members(resources: &[(String, Vec<u8>)]) -> Result<Vec<(&str, String, &[u8])>> {
    let mut members: Vec<(&str, String, &[u8])> = Vec::with_capacity(resources.len());
    for (path, bytes) in resources {
        let parts: Vec<&str> = path.split('/').filter(|part| *part != ".").collect();
        let relative = !parts.is_empty()
            && !path.starts_with('/')
            && !path.contains(['\\', ':', '#', '?', '%'])
            && parts.iter().all(|part| !part.is_empty() && *part != "..");
        if !relative {
            return Err(Error::InvalidArgument(format!("the KMZ resource '{path}' is not a relative path inside the archive")));
        }
        let member = format!("layers/{}", parts.join("/"));
        if members.iter().any(|(_, m, _)| *m == member) {
            return Err(Error::InvalidArgument(format!("the KMZ resource '{path}' is given twice")));
        }
        members.push((path.as_str(), member, bytes.as_slice()));
    }
    Ok(members)
}

const HEADER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<kml xmlns=\"http://www.opengis.net/kml/2.2\">\n";

fn schema_id(layer: &Layer) -> String {
    layer.name.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect()
}

/// Whether a field is written as the placemark's own element rather than as ExtendedData. Like
/// GDAL, a field called `name` or `description` in any letter case is the placemark's.
fn is_own_field(name: &str) -> bool {
    FIXED.iter().any(|(n, _)| *n == name)
        || name.eq_ignore_ascii_case("name")
        || name.eq_ignore_ascii_case("description")
        || name.eq_ignore_ascii_case(kml_style::FIELD)
}

/// The fields written as ExtendedData.
fn data_fields(layer: &Layer) -> Vec<usize> {
    (0..layer.schema.len()).filter(|&i| !is_own_field(&layer.schema.fields()[i].name)).collect()
}

fn schema(l: &Layer) -> String {
    let fields = data_fields(l);
    if fields.is_empty() {
        return String::new();
    }
    let mut s = format!("<Schema name=\"{0}\" id=\"{0}\">\n", xml::escape(&schema_id(l)));
    for i in fields {
        let f = &l.schema.fields()[i];
        let t = match f.field_type {
            FieldType::Integer => "int",
            FieldType::Float => "float",
            FieldType::Boolean => "bool",
            _ => "string",
        };
        let _ = writeln!(s, "\t<SimpleField name=\"{}\" type=\"{t}\"></SimpleField>", xml::escape(&f.name));
    }
    s.push_str("</Schema>\n");
    s
}

/// Encodes layers as a KML document: a Document per layer, each with its Schema (so that readers
/// tie even an empty layer to its fields), with a shared `<Style id>` for each entry of a style
/// table.
pub fn to_string_styled(layers: &[Layer], styles: &[(String, String)]) -> Result<String> {
    let mut s = String::from(HEADER);
    s.push_str("<Document id=\"root_doc\">\n");
    s.push_str(&kml_style::definitions(styles, |_| true));
    for l in layers {
        let l = in_wgs84(l)?;
        let _ = writeln!(s, "<Document id=\"{}\"><name>{}</name>", xml::escape(&schema_id(&l)), xml::escape(&l.name));
        s.push_str(&schema(&l));
        s.push_str(&placemarks(&l));
        s.push_str("</Document>\n");
    }
    s.push_str("</Document>\n</kml>\n");
    Ok(s)
}

/// The style ids a layer's features refer to (`@id`).
fn referenced_styles(l: &Layer) -> std::collections::HashSet<String> {
    let Some(i) = l.schema.fields().iter().position(|d| d.name.eq_ignore_ascii_case(kml_style::FIELD)) else { return Default::default() };
    let mut out = std::collections::HashSet::new();
    for f in &l.features {
        if let Some(FieldValue::Text(t)) = f.attributes.get(i)
            && let Some(id) = t.strip_prefix('@')
        {
            out.insert(id.to_string());
        }
    }
    out
}

/// The placemarks of a layer.
fn placemarks(l: &Layer) -> String {
    let mut s = String::new();
    let fields = data_fields(l);
    let text = |f: &Feature, n: &str| -> Option<String> {
        let i = l.schema.field_index(n).or_else(|| l.schema.fields().iter().position(|d| d.name.eq_ignore_ascii_case(n)))?;
        match f.attributes.get(i)? {
            FieldValue::Null => None,
            FieldValue::Text(t) | FieldValue::Date(t) | FieldValue::DateTime(t) => Some(t.clone()),
            FieldValue::Integer(x) => Some(x.to_string()),
            FieldValue::Float(x) => Some(x.to_string()),
            FieldValue::Boolean(b) => Some((*b as u8).to_string()),
            FieldValue::Blob(_) => None,
        }
    };
    for f in &l.features {
        match text(f, "id") {
            Some(id) => {
                let _ = writeln!(s, "  <Placemark id=\"{}\">", xml::escape(&id));
            }
            None => s.push_str("  <Placemark>\n"),
        }
        if let Some(n) = text(f, "Name") {
            let _ = writeln!(s, "\t<name>{}</name>", xml::escape(&n));
        }
        if let Some(v) = text(f, "visibility").filter(|v| v != "-1") {
            let _ = writeln!(s, "\t<visibility>{v}</visibility>");
        }
        if let Some(d) = text(f, "description") {
            let _ = writeln!(s, "\t<description>{}</description>", xml::escape(&d));
        }
        if let Some(t) = text(f, "timestamp") {
            let _ = writeln!(s, "\t<TimeStamp><when>{}</when></TimeStamp>", xml::escape(&t));
        }
        if text(f, "begin").is_some() || text(f, "end").is_some() {
            s.push_str("\t<TimeSpan>");
            for k in ["begin", "end"] {
                if let Some(t) = text(f, k) {
                    let _ = write!(s, "<{k}>{}</{k}>", xml::escape(&t));
                }
            }
            s.push_str("</TimeSpan>\n");
        }
        // The feature's style: a reference, or the style itself, as GDAL writes it.
        if let Some(v) = text(f, kml_style::FIELD) {
            if let Some(name) = v.strip_prefix('@') {
                let _ = writeln!(s, "\t<styleUrl>#{}</styleUrl>", xml::escape(name));
            } else if kml_style::is_reference(&v) {
                let _ = writeln!(s, "\t<styleUrl>{}</styleUrl>", xml::escape(&v));
            } else {
                let style = kml_style::to_kml(&v);
                if !style.is_empty() {
                    let _ = writeln!(s, "\t<Style>{style}</Style>");
                }
            }
        }
        let values: Vec<(String, String)> = fields
            .iter()
            .filter_map(|&i| {
                let name = &l.schema.fields()[i].name;
                Some((name.clone(), text(f, name)?))
            })
            .collect();
        // Even without values, so that readers tie the layer to its schema.
        if !fields.is_empty() {
            let _ = writeln!(s, "\t<ExtendedData><SchemaData schemaUrl=\"#{}\">", xml::escape(&schema_id(l)));
            for (k, v) in values {
                let _ = writeln!(s, "\t\t<SimpleData name=\"{}\">{}</SimpleData>", xml::escape(&k), xml::escape(&v));
            }
            s.push_str("\t</SchemaData></ExtendedData>\n");
        }
        if let Some(g) = &f.geometry {
            let flags = Flags {
                altitude_mode: text(f, "altitudeMode"),
                tessellate: text(f, "tessellate").filter(|v| v != "-1"),
                extrude: text(f, "extrude").filter(|v| v != "0"),
            };
            s.push('\t');
            write_geometry(&mut s, g, &flags);
            s.push('\n');
        }
        s.push_str("  </Placemark>\n");
    }
    s
}

struct Flags {
    altitude_mode: Option<String>,
    tessellate: Option<String>,
    extrude: Option<String>,
}

fn write_flags(s: &mut String, flags: &Flags) {
    if let Some(v) = &flags.extrude {
        let _ = write!(s, "<extrude>{v}</extrude>");
    }
    if let Some(v) = &flags.tessellate {
        let _ = write!(s, "<tessellate>{v}</tessellate>");
    }
    if let Some(v) = &flags.altitude_mode {
        let _ = write!(s, "<altitudeMode>{}</altitudeMode>", xml::escape(v));
    }
}

fn write_coords(s: &mut String, cs: &[Coord], close: bool) {
    s.push_str("<coordinates>");
    let first = cs.first().filter(|f| close && cs.len() > 1 && cs.last().map(|l| (l.x, l.y)) != Some((f.x, f.y)));
    for (i, c) in cs.iter().chain(first).enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{},{}", c.x, c.y);
        if let Some(z) = c.z {
            let _ = write!(s, ",{z}");
        }
    }
    s.push_str("</coordinates>");
}

fn write_polygon(s: &mut String, exterior: &Ring, interiors: &[Ring], flags: &Flags) {
    s.push_str("<Polygon>");
    write_flags(s, flags);
    s.push_str("<outerBoundaryIs><LinearRing>");
    write_coords(s, &exterior.0, true);
    s.push_str("</LinearRing></outerBoundaryIs>");
    for r in interiors {
        s.push_str("<innerBoundaryIs><LinearRing>");
        write_coords(s, &r.0, true);
        s.push_str("</LinearRing></innerBoundaryIs>");
    }
    s.push_str("</Polygon>");
}

fn write_geometry(s: &mut String, g: &Geometry, flags: &Flags) {
    match g {
        Geometry::Point(c) => {
            s.push_str("<Point>");
            write_flags(s, flags);
            write_coords(s, std::slice::from_ref(c), false);
            s.push_str("</Point>");
        }
        Geometry::LineString(cs) => {
            s.push_str("<LineString>");
            write_flags(s, flags);
            write_coords(s, cs, false);
            s.push_str("</LineString>");
        }
        Geometry::Polygon { exterior, interiors } => write_polygon(s, exterior, interiors, flags),
        Geometry::MultiPoint(cs) => {
            s.push_str("<MultiGeometry>");
            for c in cs {
                write_geometry(s, &Geometry::Point(c.clone()), flags);
            }
            s.push_str("</MultiGeometry>");
        }
        Geometry::MultiLineString(ls) => {
            s.push_str("<MultiGeometry>");
            for l in ls {
                write_geometry(s, &Geometry::LineString(l.clone()), flags);
            }
            s.push_str("</MultiGeometry>");
        }
        Geometry::MultiPolygon(ps) => {
            s.push_str("<MultiGeometry>");
            for (e, h) in ps {
                write_polygon(s, e, h, flags);
            }
            s.push_str("</MultiGeometry>");
        }
        Geometry::GeometryCollection(gs) => {
            s.push_str("<MultiGeometry>");
            for g in gs {
                write_geometry(s, g, flags);
            }
            s.push_str("</MultiGeometry>");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read as _;

    #[test]
    fn a_kmz_carries_the_files_its_styles_refer_to() {
        let mut layer = Layer::new("pins").with_crs_epsg(4326);
        layer.add_field(FieldDef::new(kml_style::FIELD, FieldType::Text));
        layer.push(Feature {
            fid: 1,
            geometry: Some(Geometry::Point(Coord::xy(30.0, -2.0))),
            attributes: vec![FieldValue::Text("SYMBOL(id:\"icons/pin.png\")".into())],
        });
        let layers = std::slice::from_ref(&layer);
        let png = b"\x89PNG\r\n\x1a\n".to_vec();
        let kmz = KmzOptions { resources: vec![("icons/pin.png".into(), png.clone())] };
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(kmz_bytes_styled(layers, &[], &kmz).unwrap())).unwrap();
        let mut image = Vec::new();
        zip.by_name("layers/icons/pin.png").unwrap().read_to_end(&mut image).unwrap();
        assert_eq!(image, png);
        let mut doc = String::new();
        zip.by_name("layers/pins.kml").unwrap().read_to_string(&mut doc).unwrap();
        assert!(doc.contains("<href>icons/pin.png</href>"), "{doc}");
        // Without resources, the archive holds the documents only, as before.
        let plain = kmz_bytes_styled(layers, &[], &KmzOptions::default()).unwrap();
        assert_eq!(zip::ZipArchive::new(std::io::Cursor::new(plain)).unwrap().len(), 2);
        for path in [
            "",
            ".",
            "/icons/pin.png",
            "../pin.png",
            "icons/../../pin.png",
            "icons//pin.png",
            "http://x/pin.png",
            "C:\\pin.png",
            "icons/a#b.png",
            "icons/a?b.png",
            "icons/a%20b.png",
            "pins.kml",
        ] {
            let kmz = KmzOptions { resources: vec![(path.into(), png.clone())] };
            assert!(matches!(kmz_bytes_styled(layers, &[], &kmz), Err(Error::InvalidArgument(_))), "{path:?}");
        }
        let twice = KmzOptions { resources: vec![("icons/pin.png".into(), png.clone()), ("./icons/pin.png".into(), png)] };
        assert!(matches!(kmz_bytes_styled(layers, &[], &twice), Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn prefixed_attributes_keep_legacy_local_name_lookup() {
        let source = r#"<kml xmlns:p="urn:vendor"><Document><name>D</name><Placemark p:id="vendor1"><ExtendedData><Data p:name="extra"><value>payload</value></Data></ExtendedData><Point><coordinates>1,2</coordinates></Point></Placemark></Document></kml>"#;
        let layers = parse(source, "file").unwrap();
        let layer = &layers[0];
        let feature = &layer.features[0];
        assert_eq!(feature.attributes[layer.schema.field_index("id").unwrap()], FieldValue::Text("vendor1".into()));
        assert_eq!(feature.attributes[layer.schema.field_index("extra").unwrap()], FieldValue::Text("payload".into()));
        assert_eq!(feature.geometry, Some(Geometry::point(1., 2.)));
    }

    #[test]
    fn reads_like_libkml() {
        let kml = r##"<kml xmlns="http://www.opengis.net/kml/2.2"><Document><name>D</name>
            <Schema name="s" id="s"><SimpleField name="n" type="int"/><SimpleField name="x" type="double"/></Schema>
            <Placemark id="p1"><name>A</name><ExtendedData><SchemaData schemaUrl="#s"><SimpleData name="n">3</SimpleData><SimpleData name="x">1.5</SimpleData></SchemaData><Data name="extra"><value>e</value></Data></ExtendedData>
              <Point><coordinates>1,2,3</coordinates></Point></Placemark>
            <Folder><name>F</name><Placemark><MultiGeometry><Polygon><outerBoundaryIs><LinearRing><coordinates>0,0 1,0 1,1 0,0</coordinates></LinearRing></outerBoundaryIs></Polygon></MultiGeometry></Placemark>
              <Placemark><MultiGeometry><Point><coordinates>1,2</coordinates></Point><LineString><coordinates>3,4 5,6</coordinates></LineString></MultiGeometry></Placemark></Folder>
            <Folder><name>Empty</name></Folder></Document></kml>"##;
        let layers = parse(kml, "file").unwrap();
        let names: Vec<&str> = layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["D", "F", "Empty"]);
        let d = &layers[0];
        let field_names: Vec<&str> = d.schema.fields().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(&field_names[12..], ["n", "x", "extra"]);
        let get = |l: &Layer, f: usize, n: &str| l.features[f].attributes[l.schema.field_index(n).unwrap()].clone();
        assert_eq!(get(d, 0, "id"), FieldValue::Text("p1".into()));
        assert_eq!(get(d, 0, "n"), FieldValue::Integer(3));
        assert_eq!(get(d, 0, "x"), FieldValue::Float(1.5));
        assert_eq!(get(d, 0, "tessellate"), FieldValue::Integer(-1));
        assert_eq!(get(d, 0, "visibility"), FieldValue::Integer(-1));
        assert_eq!(d.features[0].geometry, Some(Geometry::point_z(1.0, 2.0, 3.0)));
        assert!(matches!(layers[1].features[0].geometry, Some(Geometry::MultiPolygon(_))));
        assert!(matches!(layers[1].features[1].geometry, Some(Geometry::GeometryCollection(_))));

        // Written and read back.
        let back = parse(&to_string_styled(&layers, &[]).unwrap(), "file").unwrap();
        assert_eq!(back.iter().map(|l| l.len()).collect::<Vec<_>>(), [1, 2, 0]);
        assert_eq!(get(&back[0], 0, "n"), FieldValue::Integer(3));
        assert_eq!(back[1].features[0].geometry, layers[1].features[0].geometry);

        // A KMZ holds a document per layer, linked from doc.kml.
        let dir = tempfile::tempdir().unwrap();
        let kmz = dir.path().join("t.kmz");
        write_kmz_styled(&layers, &[], kmz.to_str().unwrap(), &KmzOptions::default()).unwrap();
        let back = read_kmz(kmz.to_str().unwrap()).unwrap();
        assert_eq!(back.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["D", "F", "Empty"]);
        assert_eq!(get(&back[0], 0, "x"), FieldValue::Float(1.5));
    }

    #[test]
    fn layer_extent_is_the_container_bounds() {
        // As in kml_samples.kml: a folder whose only placemark has no geometry, holding a document
        // with a point. GDAL (LIBKML) reports the point as the folder layer's extent too, and an
        // extent for a folder with no placemarks of its own; the empty folder has none.
        let kml = r##"<kml><Document><Folder><name>Styles and Markup</name>
            <Document><name>Highlighted Icon</name><Placemark><Point><coordinates>-122.0856545755255,37.42243077405461,0</coordinates></Point></Placemark></Document>
            <Placemark><name>Descriptive HTML</name></Placemark></Folder>
            <Folder><name>Polygons</name><Folder><name>A</name><GroundOverlay><LatLonBox><north>3</north><south>1</south><east>4</east><west>2</west></LatLonBox></GroundOverlay></Folder>
              <Folder><name>B</name><Placemark><LineString><coordinates>5,0 6,2</coordinates></LineString></Placemark></Folder></Folder>
            <Folder><name>Empty</name></Folder></Document></kml>"##;
        let layers = parse(kml, "file").unwrap();
        let ext = |i: usize| layers[i].extent.as_ref().map(|b| [b.min_x, b.min_y, b.max_x, b.max_y]);
        let p = [-122.0856545755255, 37.42243077405461, -122.0856545755255, 37.42243077405461];
        assert_eq!(ext(0), Some(p));
        assert_eq!(ext(1), Some(p));
        assert_eq!(ext(2), Some([2.0, 0.0, 6.0, 3.0]));
        assert_eq!(ext(5), None);
    }

    const STYLED: &str = r##"<kml><Document>
      <Style id="base"><LineStyle><color>ff0000ff</color><width>2</width></LineStyle>
        <PolyStyle><color>7f00ff00</color><fill>0</fill></PolyStyle></Style>
      <StyleMap id="map"><Pair><key>normal</key><styleUrl>#base</styleUrl></Pair></StyleMap>
      <Placemark><name>shared</name><styleUrl>#base</styleUrl>
        <Polygon><outerBoundaryIs><LinearRing><coordinates>30,-2 31,-2 30,-1 30,-2</coordinates></LinearRing></outerBoundaryIs></Polygon></Placemark>
      <Placemark><name>override</name><styleUrl>#base</styleUrl><Style><LineStyle><width>4</width></LineStyle></Style>
        <Polygon><outerBoundaryIs><LinearRing><coordinates>30,-2 31,-2 30,-1 30,-2</coordinates></LinearRing></outerBoundaryIs></Polygon></Placemark>
      <Placemark><name>mapped</name><styleUrl>#map</styleUrl><Point><coordinates>30,-2</coordinates></Point></Placemark>
    </Document></kml>"##;

    fn style(layer: &Layer, row: usize) -> String {
        let i = layer.schema.field_index(kml_style::FIELD).unwrap();
        match &layer.features[row].attributes[i] {
            FieldValue::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        }
    }

    const BASE: &str = r#"PEN(c:#FF0000FF,w:2.000000px);BRUSH(id:"ogr-brush-1")"#;

    #[test]
    fn styles_keep_ids_fill_and_inline_overrides() {
        let (layers, table) = parse_styled(STYLED, "a").unwrap();
        assert_eq!(style(&layers[0], 0), "@base");
        assert_eq!(style(&layers[0], 2), "@map");
        let merged = style(&layers[0], 1);
        assert!(merged.contains("c:#FF0000FF") && merged.contains("w:4"), "{merged}");
        assert!(!merged.contains("fc:#00FF007F"), "fill 0 must not come back as a visible brush: {merged}");
        assert_eq!(table, [("base".to_string(), BASE.to_string()), ("map".to_string(), BASE.to_string())]);
    }

    #[test]
    fn a_style_table_is_written_and_read_back() {
        let (layers, table) = parse_styled(STYLED, "a").unwrap();
        let text = to_string_styled(&layers, &table).unwrap();
        assert!(text.contains(r#"<Style id="base">"#) && text.contains("<fill>0</fill>"), "{text}");
        assert!(text.contains("<styleUrl>#base</styleUrl>"), "{text}");
        let (back, back_table) = parse_styled(&text, "a").unwrap();
        assert_eq!(back_table, table);
        assert_eq!(style(&back[0], 0), "@base");
        assert_eq!(style(&back[0], 1), style(&layers[0], 1));
        // Without a table, the references are written alone, as before.
        assert!(!to_string_styled(&layers, &[]).unwrap().contains("<Style id="));
        // A KMZ's layer documents hold the styles they use, since `#id` stays in its document.
        let kmz = kmz_bytes_styled(&layers, &table, &KmzOptions::default()).unwrap();
        let (back, back_table) = parse_kmz_styled(kmz.clone(), "a").unwrap();
        assert_eq!(back_table, table);
        assert_eq!(style(&back[0], 0), "@base");
        let mut doc = String::new();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(kmz)).unwrap();
        zip.by_name("layers/a.kml").unwrap().read_to_string(&mut doc).unwrap();
        assert!(doc.contains(r#"<Style id="base">"#) && doc.contains(r#"<Style id="map">"#), "{doc}");
        // Where GDAL looks for the table.
        let mut shared = String::new();
        zip.by_name("style/style.kml").unwrap().read_to_string(&mut shared).unwrap();
        assert!(shared.contains(r#"<Style id="base">"#), "{shared}");
    }

    #[test]
    fn many_layer_documents_with_their_own_styles_keep_first_definitions() {
        // Regression for #631: each layer document brings a distinct style id. The table grows
        // once per id, and the first definition wins when a later document reuses an id.
        let main = concat!(
            r#"<kml><Document><name>links</name>"#,
            r#"<NetworkLink><Link><href>layers/m0.kml</href></Link></NetworkLink>"#,
            r#"<NetworkLink><Link><href>layers/m1.kml</href></Link></NetworkLink>"#,
            r#"<NetworkLink><Link><href>layers/m2.kml</href></Link></NetworkLink>"#,
            r#"<NetworkLink><Link><href>layers/m3.kml</href></Link></NetworkLink>"#,
            r#"</Document></kml>"#,
        );
        let m0 = r#"<kml><Document><Style id="s0"><LineStyle><color>ff0000ff</color><width>1</width></LineStyle></Style><Placemark><name>p</name><styleUrl>#s0</styleUrl><Point><coordinates>1,2</coordinates></Point></Placemark></Document></kml>"#;
        let m1 = r#"<kml><Document><Style id="s1"><LineStyle><color>ff0000ff</color><width>2</width></LineStyle></Style><Placemark><name>p</name><styleUrl>#s1</styleUrl><Point><coordinates>1,2</coordinates></Point></Placemark></Document></kml>"#;
        let m2 = r#"<kml><Document><Style id="s2"><LineStyle><color>ff0000ff</color><width>3</width></LineStyle></Style><Placemark><name>p</name><styleUrl>#s2</styleUrl><Point><coordinates>1,2</coordinates></Point></Placemark></Document></kml>"#;
        // Redefines s0 with a different width; the table keeps width 1.
        let m3 = r#"<kml><Document><Style id="s0"><LineStyle><color>ff0000ff</color><width>9</width></LineStyle></Style><Placemark><name>p</name><styleUrl>#s0</styleUrl><Point><coordinates>1,2</coordinates></Point></Placemark></Document></kml>"#;
        let (layers, table) = parse_kmz_styled(
            archive(&[("doc.kml", main), ("layers/m0.kml", m0), ("layers/m1.kml", m1), ("layers/m2.kml", m2), ("layers/m3.kml", m3)]),
            "a",
        )
        .unwrap();
        assert_eq!(layers.len(), 4);
        assert_eq!(table.len(), 3);
        assert!(table.iter().any(|(id, s)| id == "s0" && s.contains("w:1.000000px")), "{table:?}");
        assert!(table.iter().any(|(id, _)| id == "s1"));
        assert!(table.iter().any(|(id, _)| id == "s2"));
        assert_eq!(style(&layers[0], 0), "@s0");
        assert!(style(&layers[3], 0).contains("w:9.000000px"));
    }

    /// A KMZ archive of the members given.
    fn archive(members: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut buf);
        for (name, text) in members {
            zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut zip, text.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        buf.into_inner()
    }

    /// A KMZ archive of deflated members.
    fn deflated(members: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut buf);
        for (name, bytes) in members {
            zip.start_file(*name, zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated)).unwrap();
            std::io::Write::write_all(&mut zip, bytes).unwrap();
        }
        zip.finish().unwrap();
        buf.into_inner()
    }

    /// A KML document with one placemark, padded by a comment of `padding` spaces.
    fn padded(padding: usize) -> Vec<u8> {
        let mut text = b"<kml><Document><Placemark><Point><coordinates>1,2</coordinates></Point></Placemark><!--".to_vec();
        text.resize(text.len() + padding, b' ');
        text.extend_from_slice(b"--></Document></kml>");
        text
    }

    /// A `NetworkLink` to `href`, with a `<name>` when `name` is not empty.
    fn network_link(name: &str, href: &str) -> String {
        let name = if name.is_empty() { String::new() } else { format!("<name>{name}</name>") };
        format!("<NetworkLink>{name}<Link><href>{href}</href></Link></NetworkLink>")
    }

    /// Each layer's name and feature count.
    fn summary(layers: &[Layer]) -> Vec<(&str, usize)> {
        layers.iter().map(|l| (l.name.as_str(), l.features.len())).collect()
    }

    use crate::config::with_options;

    #[test]
    fn kmz_documents_are_read_within_the_document_budget() {
        let kmz = deflated(&[("doc.kml", &padded(2 << 20))]);
        assert!(kmz.len() < 16 << 10, "{} bytes", kmz.len());
        // Within the default budget, the document reads.
        assert_eq!(parse_kmz(kmz.clone(), "a").unwrap()[0].features.len(), 1);
        let refused = with_options(&[("TERRIO_KMZ_MAX_SIZE", "1048576")], || parse_kmz(kmz.clone(), "a"));
        assert!(
            matches!(&refused, Err(Error::ResourceLimit(m)) if m.contains("TERRIO_KMZ_MAX_SIZE") && m.contains("doc.kml")),
            "{refused:?}"
        );
        // A document of exactly the budget fits it.
        let exact = padded(2 << 20).len().to_string();
        assert!(with_options(&[("TERRIO_KMZ_MAX_SIZE", &exact)], || parse_kmz(kmz.clone(), "a")).is_ok());
        // The main document and the layer documents it links to share the budget.
        let main = br#"<kml><Document><NetworkLink><Link><href>layers/a.kml</href></Link></NetworkLink></Document></kml>"#;
        let linked = deflated(&[("doc.kml", main), ("layers/a.kml", &padded(1 << 20))]);
        assert_eq!(parse_kmz(linked.clone(), "a").unwrap()[0].features.len(), 1);
        let budget = (padded(1 << 20).len() + main.len() - 1).to_string();
        let refused = with_options(&[("TERRIO_KMZ_MAX_SIZE", &budget)], || parse_kmz(linked.clone(), "a"));
        assert!(matches!(&refused, Err(Error::ResourceLimit(m)) if m.contains("layers/a.kml")), "{refused:?}");
        // And the path read from a file.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("expansion.kmz");
        std::fs::write(&path, &kmz).unwrap();
        let refused = with_options(&[("TERRIO_KMZ_MAX_SIZE", "1048576")], || read_kmz(path.to_str().unwrap()));
        assert!(refused.as_ref().is_err_and(Error::is_resource_refusal), "{refused:?}");
    }

    #[test]
    fn kmz_archives_are_read_within_the_zip_limits() {
        let kmz = deflated(&[("doc.kml", &padded(1 << 20))]);
        let refused = with_options(&[("TERRIO_ZIP_MAX_RATIO", "10")], || parse_kmz(kmz.clone(), "a"));
        assert!(matches!(&refused, Err(Error::ResourceLimit(m)) if m.contains("TERRIO_ZIP_MAX_RATIO")), "{refused:?}");
        let refused = with_options(&[("TERRIO_ZIP_MAX_SIZE", "1000")], || parse_kmz(kmz.clone(), "a"));
        assert!(matches!(&refused, Err(Error::ResourceLimit(m)) if m.contains("TERRIO_ZIP_MAX_SIZE")), "{refused:?}");
        let many = deflated(&[("doc.kml", &padded(0)), ("a.txt", b""), ("b.txt", b"")]);
        let refused = with_options(&[("TERRIO_ZIP_MAX_ENTRIES", "2")], || parse_kmz(many.clone(), "a"));
        assert!(matches!(&refused, Err(Error::ResourceLimit(m)) if m.contains("TERRIO_ZIP_MAX_ENTRIES")), "{refused:?}");
        assert!(parse_kmz(many, "a").is_ok());
        // A style document past the archive limits refuses the archive, rather than its styles
        // being left out.
        let main = br#"<kml><Document><Placemark><styleUrl>styles.kml#s</styleUrl><Point><coordinates>1,2</coordinates></Point></Placemark></Document></kml>"#;
        let mut styles = br#"<kml><Document><Style id="s"/><!--"#.to_vec();
        styles.resize(styles.len() + (1 << 20), b' ');
        styles.extend_from_slice(b"--></Document></kml>");
        let styled = deflated(&[("doc.kml", main), ("styles.kml", &styles)]);
        assert!(parse_kmz(styled.clone(), "a").is_ok());
        let refused = with_options(&[("TERRIO_ZIP_MAX_RATIO", "50")], || parse_kmz(styled.clone(), "a"));
        assert!(matches!(&refused, Err(Error::ResourceLimit(m)) if m.contains("styles.kml")), "{refused:?}");
    }

    #[test]
    fn repeated_links_preserve_document_and_folder_layers() {
        let point = "<Placemark><Point><coordinates>1,2</coordinates></Point></Placemark>";
        for (document, expected) in [
            (format!("<kml><Document><name>inside</name>{point}</Document></kml>"), vec![("inside", 1)]),
            (format!("<kml><Document><Folder><name>folder</name>{point}</Folder></Document></kml>"), vec![("a", 0), ("folder", 1)]),
            (format!("<kml><Document>{point}</Document></kml>"), vec![("a", 1)]),
        ] {
            // A later named link must not change the first unnamed link's default.
            let first = network_link("", "layers/a.kml");
            let repeated = format!("{first}{}", network_link("later", "./layers/a.kml"));
            for links in [first, repeated] {
                let main = format!("<kml><Document>{links}</Document></kml>");
                let layers = parse_kmz(archive(&[("doc.kml", &main), ("layers/a.kml", &document)]), "main").unwrap();
                assert_eq!(summary(&layers), expected);
            }
        }
    }

    #[test]
    fn a_document_linked_several_times_is_one_layer() {
        // The first link to a document names its layer; the others, written with `./` or not, add
        // none. A link to a member the archive does not hold is left out.
        let links = [
            network_link("first", "./layers/a.kml"),
            network_link("missing", "layers/missing.kml"),
            network_link("second", "layers/a.kml"),
            network_link("", "layers/a.kml"),
            network_link("", "layers/b.kml"),
        ];
        let main = format!("<kml><Document>{}</Document></kml>", links.concat());
        let kmz = deflated(&[("doc.kml", main.as_bytes()), ("layers/a.kml", &padded(0)), ("layers/b.kml", &padded(0))]);
        assert_eq!(summary(&parse_kmz(kmz, "main").unwrap()), [("first", 1), ("b", 1)]);
    }

    #[test]
    fn a_document_linked_several_times_counts_once_toward_the_document_budget() {
        let main = format!("<kml><Document>{}{}</Document></kml>", network_link("", "layers/a.kml"), network_link("", "./layers/a.kml"));
        let member = padded(1 << 20);
        let kmz = deflated(&[("doc.kml", main.as_bytes()), ("layers/a.kml", &member)]);
        // A budget that holds doc.kml and the document once is enough.
        let once = (main.len() + member.len()).to_string();
        let read = with_options(&[("TERRIO_KMZ_MAX_SIZE", &once)], || parse_kmz(kmz.clone(), "main"));
        let layers = read.unwrap_or_else(|e| panic!("refused within {once} bytes: {e}"));
        assert_eq!(summary(&layers), [("a", 1)]);
        // The document still counts toward it: a byte less refuses the archive.
        let short = (main.len() + member.len() - 1).to_string();
        let refused = with_options(&[("TERRIO_KMZ_MAX_SIZE", &short)], || parse_kmz(kmz.clone(), "main"));
        assert!(matches!(&refused, Err(Error::ResourceLimit(m)) if m.contains("layers/a.kml")), "{refused:?}");
    }

    #[test]
    fn a_link_to_the_main_document_adds_no_layer() {
        let point = "<Placemark><Point><coordinates>1,2</coordinates></Point></Placemark>";
        let main = format!("<kml><Document>{point}{}{}</Document></kml>", network_link("again", "doc.kml"), network_link("", "./doc.kml"));
        let kmz = deflated(&[("doc.kml", main.as_bytes())]);
        assert_eq!(summary(&parse_kmz(kmz, "main").unwrap()), [("main", 1)]);
    }

    /// A KML document named `name` holding `points` placemarks.
    fn named(name: &str, points: usize) -> String {
        let placemarks = "<Placemark><Point><coordinates>1,2</coordinates></Point></Placemark>".repeat(points);
        format!("<kml><Document><name>{name}</name>{placemarks}</Document></kml>")
    }

    /// The main document is the first KML entry at the archive's root, in the order of its
    /// entries, whatever its name (OGC KML 2.3, Annex C.1); one in a folder is the main document
    /// only when the root holds none.
    #[test]
    fn the_main_document_is_the_first_kml_entry_at_the_archive_root() {
        let (first, second) = (named("first", 1), named("second", 3));
        for (members, expected) in [
            (vec![("a.kml", first.as_str()), ("doc.kml", second.as_str())], [("first", 1)]),
            (vec![("doc.kml", first.as_str()), ("a.kml", second.as_str())], [("first", 1)]),
            (vec![("notes.txt", "-"), ("A.KML", first.as_str()), ("DOC.KML", second.as_str())], [("first", 1)]),
            (vec![("files/b.kml", second.as_str()), ("files/doc.kml", second.as_str()), ("a.kml", first.as_str())], [("first", 1)]),
            (vec![("notes.txt", "-"), ("files/a.kml", first.as_str()), ("files/doc.kml", second.as_str())], [("first", 1)]),
            (vec![("files\\doc.kml", second.as_str()), ("a.kml", first.as_str())], [("first", 1)]),
        ] {
            let names: Vec<&str> = members.iter().map(|(name, _)| *name).collect();
            assert_eq!(summary(&parse_kmz(archive(&members), "kmz").unwrap()), expected, "{names:?}");
        }
        let refused = parse_kmz(archive(&[("notes.txt", "-")]), "kmz");
        assert!(matches!(&refused, Err(Error::InvalidData(m)) if m == "the KMZ archive holds no KML document"), "{refused:?}");
    }

    /// `kmz` with every `from` in its entries' names (and anywhere else) replaced by `to`, a name of
    /// the same length: zip writers refuse a second entry of the same name, so it is written under
    /// another.
    fn renamed(kmz: &[u8], from: &str, to: &str) -> Vec<u8> {
        assert_eq!(from.len(), to.len());
        let mut out = kmz.to_vec();
        let mut i = 0;
        while let Some(at) = out[i..].windows(from.len()).position(|w| w == from.as_bytes()) {
            out[i + at..i + at + to.len()].copy_from_slice(to.as_bytes());
            i += at + to.len();
        }
        out
    }

    #[test]
    fn leading_current_directory_components_leave_a_kml_at_the_archive_root() {
        let (first, nested) = (named("first", 1), named("nested", 3));
        for root in ["./doc.kml", "././doc.kml", r".\doc.kml"] {
            let bytes = archive(&[("files/nested.kml", nested.as_str()), (root, first.as_str())]);
            assert_eq!(summary(&parse_kmz(bytes, "kmz").unwrap()), [("first", 1)], "{root}");
        }
    }

    /// An archive holding two entries of the same name is refused, whichever entries they are: the
    /// zip reader keeps one entry per name, the last, so the first of two `doc.kml` could not be
    /// told apart from it.
    #[test]
    fn an_archive_with_two_entries_of_one_name_is_refused() {
        let (first, second) = (named("first", 1), named("second", 3));
        let two_docs = renamed(&archive(&[("doc.kml", first.as_str()), ("dox.kml", second.as_str())]), "dox.kml", "doc.kml");
        let two_icons =
            renamed(&archive(&[("doc.kml", first.as_str()), ("files/a.png", "1"), ("files/b.png", "2")]), "files/b.png", "files/a.png");
        for (label, kmz) in [("two doc.kml", two_docs), ("two files/a.png", two_icons)] {
            let refused = parse_kmz(kmz.clone(), "kmz");
            let message = "KMZ: the archive holds several entries with the same name, so its main document is ambiguous";
            assert!(matches!(&refused, Err(Error::InvalidData(m)) if m == message), "{label}: {refused:?}");
            // The same bytes read from a file.
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("duplicates.kmz");
            std::fs::write(&path, &kmz).unwrap();
            let refused = read_kmz(path.to_str().unwrap());
            assert!(matches!(&refused, Err(Error::InvalidData(m)) if m == message), "{label}: {refused:?}");
            // Public entry points must preserve the refusal even with the GDAL backend enabled.
            let path = path.to_str().unwrap();
            for refused in [
                crate::open(path).map(|_| ()),
                crate::open_vector(path, &Default::default()).map(|_| ()),
                crate::list_layers(path).map(|_| ()),
            ] {
                assert!(matches!(&refused, Err(Error::InvalidData(m)) if m == message), "{label}: {refused:?}");
            }
        }
        // Without the second name, the archive reads.
        assert_eq!(
            summary(&parse_kmz(archive(&[("doc.kml", first.as_str()), ("dox.kml", second.as_str())]), "kmz").unwrap()),
            [("first", 1)]
        );
    }

    fn pin(url: &str, inline: &str) -> String {
        format!("<Placemark><styleUrl>{url}</styleUrl>{inline}<Point><coordinates>30,-2</coordinates></Point></Placemark>")
    }

    #[test]
    fn a_style_url_into_another_kmz_member_resolves_against_it() {
        let shared = r##"<kml><Document><Style id="pin"><LineStyle><color>ff0000ff</color></LineStyle></Style>
            <StyleMap id="mapped"><Pair><key>normal</key><styleUrl>#pin</styleUrl></Pair></StyleMap></Document></kml>"##;
        let doc = format!(
            "<kml><Document>{}{}{}{}{}</Document></kml>",
            pin("styles.kml#pin", ""),
            pin("styles.kml#mapped", ""),
            pin("sub/inner.kml#pin", ""),
            pin("missing.kml#pin", ""),
            pin("styles.kml#pin", "<Style><LineStyle><width>4</width></LineStyle></Style>"),
        );
        let kmz =
            archive(&[("doc.kml", &doc), ("styles.kml", shared), ("sub/inner.kml", &format!("<kml><Document>{}</Document></kml>", ""))]);
        let (layers, table) = parse_kmz_styled(kmz, "a").unwrap();
        assert_eq!(style(&layers[0], 0), "PEN(c:#FF0000FF)");
        assert_eq!(style(&layers[0], 1), "PEN(c:#FF0000FF)");
        // A document without that style, or no such document: the URL as it is.
        assert_eq!(style(&layers[0], 2), "sub/inner.kml#pin");
        assert_eq!(style(&layers[0], 3), "missing.kml#pin");
        assert_eq!(style(&layers[0], 4), "PEN(c:#FF0000FF,w:4.000000px)");
        // Those definitions are the other document's, and not in the table of this one's.
        assert!(table.is_empty(), "{table:?}");

        // Relative to the referring document, not to the archive's root.
        let link = |file: &str| format!("<kml><Document><NetworkLink><Link><href>{file}</href></Link></NetworkLink></Document></kml>");
        let layer_doc = format!("<kml><Document>{}{}</Document></kml>", pin("s.kml#pin", ""), pin("../styles.kml#pin", ""));
        let kmz =
            archive(&[("doc.kml", &link("layers/a.kml")), ("layers/a.kml", &layer_doc), ("layers/s.kml", shared), ("styles.kml", shared)]);
        let (layers, _) = parse_kmz_styled(kmz, "a").unwrap();
        assert_eq!(style(&layers[0], 0), "PEN(c:#FF0000FF)");
        assert_eq!(style(&layers[0], 1), "PEN(c:#FF0000FF)");
    }

    #[test]
    fn an_id_two_kmz_documents_define_differently_is_not_confused() {
        let doc = |color: &str, name: &str| {
            format!(
                r##"<kml><Document><name>{name}</name><Style id="s"><LineStyle><color>{color}</color></LineStyle></Style>{}</Document></kml>"##,
                pin("#s", "")
            )
        };
        let main = r#"<kml><Document><NetworkLink><Link><href>layers/a.kml</href></Link></NetworkLink><NetworkLink><Link><href>layers/b.kml</href></Link></NetworkLink></Document></kml>"#;
        let kmz = archive(&[("doc.kml", main), ("layers/a.kml", &doc("ff0000ff", "A")), ("layers/b.kml", &doc("ff00ff00", "B"))]);
        let (layers, table) = parse_kmz_styled(kmz, "a").unwrap();
        assert_eq!(table, [("s".to_string(), "PEN(c:#FF0000FF)".to_string())]);
        assert_eq!(style(&layers[0], 0), "@s");
        assert_eq!(style(&layers[1], 0), "PEN(c:#00FF00FF)");

        // `#s` in a document that does not define it does not take another document's `s`.
        let main = r#"<kml><Document><NetworkLink><Link><href>layers/a.kml</href></Link></NetworkLink><NetworkLink><Link><href>layers/b.kml</href></Link></NetworkLink></Document></kml>"#;
        let plain = format!("<kml><Document><name>B</name>{}</Document></kml>", pin("#s", ""));
        let kmz = archive(&[("doc.kml", main), ("layers/a.kml", &doc("ff0000ff", "A")), ("layers/b.kml", &plain)]);
        let (layers, _) = parse_kmz_styled(kmz, "a").unwrap();
        assert_eq!(style(&layers[0], 0), "@s");
        assert_eq!(style(&layers[1], 0), "#s");
        // Nor beside an inline style that sets nothing OGR holds.
        let plain = format!("<kml><Document><name>B</name>{}</Document></kml>", pin("#s", "<Style><BalloonStyle/></Style>"));
        let kmz = archive(&[("doc.kml", main), ("layers/a.kml", &doc("ff0000ff", "A")), ("layers/b.kml", &plain)]);
        let (layers, _) = parse_kmz_styled(kmz, "a").unwrap();
        assert!(layers[1].schema.field_index(kml_style::FIELD).is_none());
    }

    #[test]
    fn style_documents_beyond_the_limits_stay_urls() {
        let pins: String = (0..MAX_STYLE_DOCUMENTS + 8).map(|i| pin(&format!("s{i}.kml#p"), "")).collect();
        let doc = format!("<kml><Document>{pins}</Document></kml>");
        let shared = r#"<kml><Document><Style id="p"><LineStyle><color>ff0000ff</color></LineStyle></Style></Document></kml>"#;
        let names: Vec<String> = (0..MAX_STYLE_DOCUMENTS + 8).map(|i| format!("s{i}.kml")).collect();
        let mut files: Vec<(&str, &str)> = vec![("doc.kml", &doc)];
        files.extend(names.iter().map(|n| (n.as_str(), shared)));
        let (layers, _) = parse_kmz_styled(archive(&files), "a").unwrap();
        let resolved = (0..layers[0].features.len()).filter(|&i| style(&layers[0], i).starts_with("PEN")).count();
        assert_eq!(resolved, MAX_STYLE_DOCUMENTS);
        for i in 0..layers[0].features.len() {
            let value = style(&layers[0], i);
            assert!(value.starts_with("PEN") || (value.starts_with('s') && value.ends_with(".kml#p")), "{value}");
        }
    }

    /// Dataset-wide selection names use the existing suffix policy (GDAL 3.13.3 comparison scope:
    /// geometry/attributes only; these deterministic names are Terrio's reader policy).
    #[test]
    fn kmz_layer_names_are_unique_across_documents() {
        let point = |x| format!("<Placemark><name>point {x}</name><Point><coordinates>{x},46</coordinates></Point></Placemark>");
        let folder = |name: &str, x| format!("<Folder><name>{name}</name>{}</Folder>", point(x));
        let link = |href| format!("<NetworkLink><name>roads</name><Link><href>{href}</href></Link></NetworkLink>");
        let linked = format!("<kml><Document>{}{}</Document></kml>", point(3), folder("roads", 4));
        let main = format!("<kml><Document>{}{}{}</Document></kml>", folder("roads (2)", 1), folder("roads", 2), link("layers/a.kml"));
        let layers = parse_kmz(archive(&[("doc.kml", &main), ("layers/a.kml", &linked)]), "archive").unwrap();
        assert_eq!(layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["roads (2)", "roads", "roads (3)", "roads (4)"]);
        for (layer, x) in layers.iter().zip(1..=4) {
            assert_eq!(layer.features[0].geometry, Some(Geometry::point(x as f64, 46.0)));
        }
        let main = format!("<kml><Document>{}{}</Document></kml>", link("layers/a.kml"), link("layers/b.kml"));
        let simple = format!("<kml><Document>{}</Document></kml>", point(5));
        let layers = parse_kmz(archive(&[("doc.kml", &main), ("layers/a.kml", &simple), ("layers/b.kml", &simple)]), "archive").unwrap();
        assert_eq!(layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["roads", "roads (2)"]);
    }

    /// A document's layer names are made unique in document order, in a KML file and a KMZ
    /// archive alike. A name an earlier layer has takes the first free suffix, ` (2)`, ` (3)`, and
    /// so on, and a container without a name is named after its element. A suffix a name given
    /// earlier holds is skipped, and a name given later is renamed when an earlier layer holds it,
    /// generated or not.
    #[test]
    fn layer_names_are_made_unique_in_document_order() {
        let point = "<Placemark><Point><coordinates>1,2</coordinates></Point></Placemark>";
        let folder = |name: &str, inside: &str| match name {
            "" => format!("<Folder>{point}{inside}</Folder>"),
            name => format!("<Folder><name>{name}</name>{point}{inside}</Folder>"),
        };
        let nested = [folder("", ""), folder("roads (2)", ""), format!("<Document>{point}</Document>")].concat();
        let containers = [
            folder("roads", ""),
            folder("", ""),
            folder("roads", ""),
            folder("", ""),
            folder("Folder (3)", ""),
            folder("", ""),
            folder("Folder (2)", ""),
            format!("<Document>{point}</Document>"),
            folder("doc", ""),
            folder("roads", &nested),
            folder("", ""),
        ]
        .concat();
        let doc = format!("<kml><Document><name>doc</name>{point}{containers}</Document></kml>");
        let expected = [
            "doc",
            "roads",
            "Folder",
            "roads (2)",
            "Folder (2)",
            "Folder (3)",
            "Folder (4)",
            "Folder (2) (2)",
            "Document",
            "doc (2)",
            "roads (3)",
            "Folder (5)",
            "roads (2) (2)",
            "Document (2)",
            "Folder (6)",
        ];
        let names = |layers: Vec<Layer>| layers.into_iter().map(|l| l.name).collect::<Vec<_>>();
        assert_eq!(names(parse(&doc, "kml").unwrap()), expected);
        assert_eq!(names(parse_kmz(archive(&[("doc.kml", &doc)]), "kmz").unwrap()), expected);
        let plain = format!("<kml><Document>{}{}</Document></kml>", folder("Roads", ""), folder("roads", ""));
        assert_eq!(names(parse(&plain, "kml").unwrap()), ["Roads", "roads (2)"]);
    }
}

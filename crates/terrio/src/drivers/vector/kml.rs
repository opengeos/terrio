//! KML and KMZ, read as GDAL's LIBKML driver reads them.
//!
//! Every Folder or Document below the root is a layer, and the root container is one too when it
//! holds placemarks directly (named after it, or after the file). Fields are LIBKML's: `id`,
//! `Name`, `description`, `timestamp`, `begin`, `end`, `altitudeMode`, `tessellate`, `extrude`,
//! `visibility`, `drawOrder`, `icon`, then the `ExtendedData` fields (typed by their `Schema`).
//! A `MultiGeometry` of one geometry type is a multi-part geometry, and ground overlays are
//! polygons. Writing produces a Document per layer, each with its typed Schema.

use super::kml_style;
use super::xml::{self, Element};
use crate::{Error, Result};
use std::fmt::Write as _;
use std::io::Read as _;
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

/// Reads a KML file.
pub fn read(path: &str) -> Result<Vec<Layer>> {
    parse(&xml::decode(&std::fs::read(path)?), &stem(path))
}

/// Reads a KMZ file: its main document (`doc.kml`, or the first `.kml` file), and the documents
/// in the archive it links to (GDAL writes one per layer under `layers/`).
pub fn read_kmz(path: &str) -> Result<Vec<Layer>> {
    parse_kmz(std::fs::read(path)?, &stem(path))
}

/// Parses a KMZ archive held in memory. `default_name` names a root layer without a name.
pub fn parse_kmz(bytes: Vec<u8>, default_name: &str) -> Result<Vec<Layer>> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    let names: Vec<String> = zip.file_names().filter_map(|n| n.ok().map(|n| n.into_owned())).collect();
    let main = names
        .iter()
        .find(|n| n.eq_ignore_ascii_case("doc.kml"))
        .or_else(|| names.iter().find(|n| n.to_ascii_lowercase().ends_with(".kml")))
        .ok_or_else(|| Error::InvalidData("the KMZ archive holds no KML document".into()))?
        .clone();
    let mut read_member = |name: &str| -> Result<String> {
        let mut bytes = Vec::new();
        zip.by_name(name)?.read_to_end(&mut bytes)?;
        Ok(xml::decode(&bytes))
    };
    let text = read_member(&main)?;
    let mut layers = parse(&text, default_name)?;
    let root = xml::parse(&text)?;
    let mut links = Vec::new();
    root.find_all("NetworkLink", &mut links);
    for link in links {
        let Some(href) = link.child("Link").or_else(|| link.child("Url")).and_then(|l| l.child_text("href")) else { continue };
        let href = href.trim_start_matches("./");
        if let Some(member) = names.iter().find(|n| n.as_str() == href) {
            let name = link.child_text("name").map_or_else(|| stem(member), str::to_string);
            layers.extend(parse_with(&read_member(member)?, &name, true)?);
        }
    }
    Ok(layers)
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
    parse_with(text, default_name, false)
}

/// Parses KML text; with `root_layer`, the root container is a layer even without placemarks of
/// its own (a layer document linked from a KMZ's `doc.kml`).
fn parse_with(text: &str, default_name: &str, root_layer: bool) -> Result<Vec<Layer>> {
    let root = xml::parse(text)?;
    // The schemas, anywhere in the document, by id and by name.
    let mut schemas = Vec::new();
    collect_schemas(&root, &mut schemas);

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
            containers.push((default_name.to_string(), direct, &root));
        }
    }
    let styles = kml_style::table(&root);
    let mut layers: Vec<Layer> = containers
        .into_iter()
        .map(|(name, features, container)| {
            let mut l = layer(&name, &features, &schemas, &styles);
            l.extent = container_bounds(container);
            l
        })
        .collect();
    // Layer names must be unique.
    for i in 1..layers.len() {
        let mut n = 2;
        let base = layers[i].name.clone();
        while layers[..i].iter().any(|l| l.name == layers[i].name) {
            layers[i].name = format!("{base} ({n})");
            n += 1;
        }
    }
    Ok(layers)
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

fn layer(name: &str, features: &[&Element], schemas: &[&Element], styles: &[(String, String)]) -> Layer {
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
    let feature_styles: Vec<Option<String>> = features.iter().map(|f| kml_style::of_feature(f, styles)).collect();
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

/// Writes layers as a KML document: a Document per layer, with its typed Schema.
pub fn write(layers: &[Layer], path: &str) -> Result<()> {
    std::fs::write(path, to_string(layers)?)?;
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
/// document per layer under `layers/`, with the files of `kmz` beside those documents.
pub fn write_kmz(layers: &[Layer], path: &str, kmz: &KmzOptions) -> Result<()> {
    std::fs::write(path, kmz_bytes(layers, kmz)?)?;
    Ok(())
}

/// Encodes layers as a KMZ archive.
pub fn kmz_bytes(layers: &[Layer], kmz: &KmzOptions) -> Result<Vec<u8>> {
    let resources = resource_members(&kmz.resources)?;
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let options = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let mut doc = String::from(HEADER);
        doc.push_str("<Document id=\"root_doc\">\n");
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
            layer_doc.push_str(&schema(&l));
            layer_doc.push_str(&placemarks(&l));
            layer_doc.push_str("</Document>\n</kml>\n");
            files.push((file, layer_doc));
        }
        doc.push_str("</Document>\n</kml>\n");
        // KML readers (libkml among them) take the archive's first document as the main one.
        zip.start_file("doc.kml", options)?;
        std::io::Write::write_all(&mut zip, doc.as_bytes())?;
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

fn in_wgs84(layer: &Layer) -> Result<std::borrow::Cow<'_, Layer>> {
    let srs = crate::crs::SpatialRef::from_layer(layer);
    if srs.is_unknown() || srs.epsg == Some(4326) {
        return Ok(std::borrow::Cow::Borrowed(layer));
    }
    Ok(std::borrow::Cow::Owned(crate::vector::reproject_layer(layer, &crate::crs::SpatialRef::from_epsg(4326)?)?))
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
/// tie even an empty layer to its fields).
pub fn to_string(layers: &[Layer]) -> Result<String> {
    let mut s = String::from(HEADER);
    s.push_str("<Document id=\"root_doc\">\n");
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

    #[test]
    fn a_kmz_carries_the_files_its_styles_refer_to() {
        use std::io::Read as _;
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
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(kmz_bytes(layers, &kmz).unwrap())).unwrap();
        let mut image = Vec::new();
        zip.by_name("layers/icons/pin.png").unwrap().read_to_end(&mut image).unwrap();
        assert_eq!(image, png);
        let mut doc = String::new();
        zip.by_name("layers/pins.kml").unwrap().read_to_string(&mut doc).unwrap();
        assert!(doc.contains("<href>icons/pin.png</href>"), "{doc}");
        // Without resources, the archive holds the documents only, as before.
        let plain = kmz_bytes(layers, &KmzOptions::default()).unwrap();
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
            assert!(matches!(kmz_bytes(layers, &kmz), Err(Error::InvalidArgument(_))), "{path:?}");
        }
        let twice = KmzOptions { resources: vec![("icons/pin.png".into(), png.clone()), ("./icons/pin.png".into(), png)] };
        assert!(matches!(kmz_bytes(layers, &twice), Err(Error::InvalidArgument(_))));
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
        let back = parse(&to_string(&layers).unwrap(), "file").unwrap();
        assert_eq!(back.iter().map(|l| l.len()).collect::<Vec<_>>(), [1, 2, 0]);
        assert_eq!(get(&back[0], 0, "n"), FieldValue::Integer(3));
        assert_eq!(back[1].features[0].geometry, layers[1].features[0].geometry);

        // A KMZ holds a document per layer, linked from doc.kml.
        let dir = tempfile::tempdir().unwrap();
        let kmz = dir.path().join("t.kmz");
        write_kmz(&layers, kmz.to_str().unwrap(), &KmzOptions::default()).unwrap();
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
}

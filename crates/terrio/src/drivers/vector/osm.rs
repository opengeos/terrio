//! OpenStreetMap PBF and XML, read into the layers of GDAL's OSM driver with its default `osmconf.ini`.
//!
//! - `points`: nodes with a tag other than `created_by`, `converted_by`, `source`, `time`, `ele`
//!   or `attribution`.
//! - `lines`: tagged ways that are not areas, with a computed `z_order`.
//! - `multipolygons`: closed ways tagged as areas (`osm_way_id` set) and `multipolygon` or
//!   `boundary` relations (`osm_id` set), with rings assembled from the member ways and organized
//!   by containment.
//! - `multilinestrings`: `multilinestring` and `route` relations.
//! - `other_relations`: the other relations, as geometry collections.
//!
//! Each layer has GDAL's attribute fields; the other tags go to `other_tags` in PostgreSQL
//! hstore syntax (`"key"=>"value"`). Tags are listed in key order, where GDAL keeps the file's
//! order.

use crate::{Error, Result};
use osmpbfreader::{OsmId, OsmObj, OsmPbfReader, Tags};
use std::collections::HashMap;
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Ring};

const CLOSED_WAYS_ARE_POLYGONS: &[&str] = &[
    "aeroway",
    "amenity",
    "boundary",
    "building",
    "craft",
    "geological",
    "historic",
    "landuse",
    "leisure",
    "military",
    "natural",
    "office",
    "place",
    "shop",
    "sport",
    "tourism",
    "highway=platform",
    "public_transport=platform",
];
const POINT_INSIGNIFICANT: &[&str] = &["created_by", "converted_by", "source", "time", "ele", "attribution"];
const IGNORE: &[&str] = &["created_by", "converted_by", "source", "time", "ele", "note", "todo", "openGeoDB:", "fixme", "FIXME"];

/// A layer's fields and the tags it leaves out of `other_tags`.
struct Spec {
    name: &'static str,
    geom_type: GeometryType,
    attributes: &'static [&'static str],
    way_id: bool,
    z_order: bool,
    ignore_area: bool,
}

const SPECS: [Spec; 5] = [
    Spec {
        name: "points",
        geom_type: GeometryType::Point,
        attributes: &["name", "barrier", "highway", "ref", "address", "is_in", "place", "man_made"],
        way_id: false,
        z_order: false,
        ignore_area: false,
    },
    Spec {
        name: "lines",
        geom_type: GeometryType::LineString,
        attributes: &["name", "highway", "waterway", "aerialway", "barrier", "man_made", "railway"],
        way_id: false,
        z_order: true,
        ignore_area: false,
    },
    Spec {
        name: "multilinestrings",
        geom_type: GeometryType::MultiLineString,
        attributes: &["name", "type"],
        way_id: false,
        z_order: false,
        ignore_area: true,
    },
    Spec {
        name: "multipolygons",
        geom_type: GeometryType::MultiPolygon,
        attributes: &[
            "name",
            "type",
            "aeroway",
            "amenity",
            "admin_level",
            "barrier",
            "boundary",
            "building",
            "craft",
            "geological",
            "historic",
            "land_area",
            "landuse",
            "leisure",
            "man_made",
            "military",
            "natural",
            "office",
            "place",
            "shop",
            "sport",
            "tourism",
        ],
        way_id: true,
        z_order: false,
        ignore_area: true,
    },
    Spec {
        name: "other_relations",
        geom_type: GeometryType::GeometryCollection,
        attributes: &["name", "type"],
        way_id: false,
        z_order: false,
        ignore_area: true,
    },
];

impl Spec {
    fn layer(&self) -> Layer {
        let mut l = Layer::new(self.name).with_epsg(4326);
        l.geom_type = Some(self.geom_type);
        l.add_field(FieldDef::new("osm_id", FieldType::Text));
        if self.way_id {
            l.add_field(FieldDef::new("osm_way_id", FieldType::Text));
        }
        for a in self.attributes {
            l.add_field(FieldDef::new(*a, FieldType::Text));
        }
        if self.z_order {
            l.add_field(FieldDef::new("z_order", FieldType::Integer));
        }
        l.add_field(FieldDef::new("other_tags", FieldType::Text));
        l
    }

    fn ignored(&self, key: &str) -> bool {
        if self.ignore_area && key == "area" {
            return true;
        }
        IGNORE.iter().any(|i| match i.strip_suffix(':') {
            Some(prefix) => key.split_once(':').is_some_and(|(p, _)| p == prefix),
            None => *i == key,
        })
    }

    /// A feature's attributes from its id and tags.
    fn attributes(&self, layer: &Layer, id: i64, is_way: bool, tags: &[(&str, &str)]) -> Vec<FieldValue> {
        let mut attrs = vec![FieldValue::Null; layer.schema.len()];
        let id_field = if is_way { "osm_way_id" } else { "osm_id" };
        if let Some(i) = layer.schema.field_index(id_field) {
            attrs[i] = FieldValue::Text(id.to_string());
        }
        let mut other = String::new();
        for (k, v) in tags {
            if let Some(i) = self.attributes.iter().position(|a| a == k) {
                attrs[layer.schema.field_index(self.attributes[i]).unwrap_or(0)] = FieldValue::Text(v.to_string());
                continue;
            }
            if self.ignored(k) {
                continue;
            }
            if !other.is_empty() {
                other.push(',');
            }
            hstore(&mut other, k);
            other.push_str("=>");
            hstore(&mut other, v);
        }
        if !other.is_empty() {
            attrs[layer.schema.len() - 1] = FieldValue::Text(other);
        }
        if self.z_order {
            let tag = |k: &str| tags.iter().find(|(t, _)| *t == k).map(|(_, v)| *v);
            let mut z = match tag("highway") {
                Some("minor" | "road" | "unclassified" | "residential") => 3,
                Some("tertiary_link" | "tertiary") => 4,
                Some("secondary_link" | "secondary") => 6,
                Some("primary_link" | "primary") => 7,
                Some("trunk_link" | "trunk") => 8,
                Some("motorway_link" | "motorway") => 9,
                _ => 0,
            };
            let truthy = |v: Option<&str>| matches!(v, Some("yes" | "true" | "1"));
            z += if truthy(tag("bridge")) { 10 } else { 0 };
            z -= if truthy(tag("tunnel")) { 10 } else { 0 };
            z += if tag("railway").is_some() { 5 } else { 0 };
            // SQLite's CAST: the leading integer, or 0.
            if let Some(l) = tag("layer") {
                let digits: String = l
                    .trim()
                    .chars()
                    .enumerate()
                    .take_while(|(i, c)| c.is_ascii_digit() || (*i == 0 && (*c == '-' || *c == '+')))
                    .map(|(_, c)| c)
                    .collect();
                z += 10 * digits.parse::<i64>().unwrap_or(0);
            }
            if let Some(i) = layer.schema.field_index("z_order") {
                attrs[i] = FieldValue::Integer(z);
            }
        }
        attrs
    }
}

fn hstore(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
}

fn tag_list(tags: &Tags) -> Vec<(&str, &str)> {
    tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

fn is_area(tags: &[(&str, &str)]) -> bool {
    let mut area = false;
    for (k, v) in tags {
        if *k == "area" {
            if *v == "yes" {
                return true;
            }
            if *v == "no" {
                return false;
            }
        }
        if area {
            continue;
        }
        if CLOSED_WAYS_ARE_POLYGONS.iter().any(|p| p == k || p.split_once('=') == Some((k, v))) {
            area = true;
        }
    }
    area
}

struct Way {
    nodes: Vec<i64>,
    tags: Vec<(String, String)>,
    area: bool,
}

/// A relation member: a node, way or relation id, and its role.
#[derive(Clone, Copy, PartialEq)]
enum Member {
    Node(i64),
    Way(i64),
    Relation(i64),
}

struct Relation {
    id: i64,
    members: Vec<(Member, String)>,
    tags: Vec<(String, String)>,
}

/// Collects nodes, ways and relations (from PBF or XML), then builds GDAL's five layers.
struct Builder {
    nodes: HashMap<i64, Coord>,
    ways: HashMap<i64, Way>,
    way_order: Vec<i64>,
    relations: Vec<Relation>,
    layers: Vec<Layer>,
}

impl Builder {
    fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            ways: HashMap::new(),
            way_order: Vec::new(),
            relations: Vec::new(),
            layers: SPECS.iter().map(Spec::layer).collect(),
        }
    }

    /// A node; `point` is where its feature goes (GDAL places points at the coordinates as read,
    /// and way vertices at the 1e-7 degree grid it stores nodes on).
    fn node(&mut self, id: i64, point: Coord, stored: Coord, tags: &[(&str, &str)]) {
        if tags.iter().any(|(k, _)| !POINT_INSIGNIFICANT.contains(k)) {
            let attrs = SPECS[0].attributes(&self.layers[0], id, false, tags);
            self.layers[0].push(Feature { fid: id as u64, geometry: Some(Geometry::Point(point)), attributes: attrs });
        }
        self.nodes.insert(id, stored);
    }

    fn way(&mut self, id: i64, nodes: Vec<i64>, tags: &[(&str, &str)]) {
        let closed = nodes.len() > 1 && nodes.first() == nodes.last();
        let area = closed && is_area(tags);
        self.way_order.push(id);
        let tags = tags.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        self.ways.insert(id, Way { nodes, tags, area });
    }

    fn finish(self) -> Vec<Layer> {
        let Builder { nodes, ways, way_order, relations, mut layers } = self;
        // The coordinates of a way, as GDAL keeps them: nodes it cannot find are skipped, and an
        // area's ring is closed.
        let coords = |w: &Way| -> Vec<Coord> {
            let n = if w.area { w.nodes.len() - 1 } else { w.nodes.len() };
            let mut cs: Vec<Coord> = w.nodes[..n].iter().filter_map(|id| nodes.get(id).cloned()).collect();
            if w.area && !cs.is_empty() {
                cs.push(cs[0].clone());
            }
            cs
        };

        // Lines, and the areas standing alone (unless a relation uses them as an outer ring).
        let mut standalone: Vec<i64> = Vec::new();
        for id in &way_order {
            let w = &ways[id];
            if w.nodes.len() < 2 {
                continue;
            }
            let cs = coords(w);
            if cs.len() < 2 {
                continue;
            }
            if w.area {
                // GDAL stores ways without these keys, and makes no polygon of an area with no
                // other tag.
                const STORED_WITHOUT: [&str; 7] = ["area", "created_by", "converted_by", "note", "todo", "fixme", "FIXME"];
                if w.tags.iter().any(|(k, _)| !STORED_WITHOUT.contains(&k.as_str())) {
                    standalone.push(*id);
                }
            } else if !w.tags.is_empty() {
                let tags: Vec<(&str, &str)> = w.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let attrs = SPECS[1].attributes(&layers[1], *id, false, &tags);
                layers[1].push(Feature { fid: *id as u64, geometry: Some(Geometry::LineString(cs)), attributes: attrs });
            }
        }

        let mut used_as_outer: std::collections::HashSet<i64> = Default::default();
        for r in &relations {
            let tags: Vec<(&str, &str)> = r.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let rtype = tags.iter().find(|(k, _)| *k == "type").map(|(_, v)| *v);
            let interesting = tags.iter().any(|(k, _)| *k != "type" && *k != "created_by");
            let members: Vec<(&(Member, String), Option<&Way>)> = r
                .members
                .iter()
                .filter(|m| m.1 != "subarea")
                .map(|m| (m, if let Member::Way(w) = m.0 { ways.get(&w) } else { None }))
                .collect();
            match rtype {
                Some("multipolygon" | "boundary") => {
                    let way_members: Vec<_> = members.iter().filter(|(m, _)| matches!(m.0, Member::Way(_))).collect();
                    if way_members.iter().any(|(_, w)| w.is_none()) {
                        continue; // missing ways
                    }
                    let mut rings = Vec::new();
                    let mut edges = Vec::new();
                    let mut first_outer_tags = None;
                    for (m, w) in &way_members {
                        let w = w.unwrap_or_else(|| unreachable!());
                        let cs = coords(w);
                        if first_outer_tags.is_none() && m.1 == "outer" {
                            first_outer_tags = Some(&w.tags);
                        }
                        let closed = !cs.is_empty() && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y));
                        if closed {
                            if m.1 == "outer"
                                && let Member::Way(id) = m.0
                            {
                                used_as_outer.insert(id);
                            }
                            rings.push(cs);
                        } else if !cs.is_empty() {
                            edges.push(cs);
                        }
                    }
                    rings.extend(assemble_rings(edges));
                    let rings: Vec<Vec<Coord>> = rings.into_iter().filter(|r| r.len() >= 4).collect();
                    let Some(g) = super::polygons_from_rings(rings) else { continue };
                    let g = match g {
                        Geometry::Polygon { exterior, interiors } => Geometry::MultiPolygon(vec![(exterior, interiors)]),
                        g => g,
                    };
                    // A relation tagged only with its type takes the tags of its first outer way.
                    let mut owned: Vec<(&str, &str)>;
                    let tags = if interesting {
                        &tags
                    } else {
                        owned = first_outer_tags.map(|t| t.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()).unwrap_or_default();
                        owned.push(("type", rtype.unwrap_or_default()));
                        &owned
                    };
                    let attrs = SPECS[3].attributes(&layers[3], r.id, false, tags);
                    layers[3].push(Feature { fid: r.id as u64, geometry: Some(g), attributes: attrs });
                }
                Some("multilinestring" | "route") => {
                    let lines: Vec<Vec<Coord>> = members.iter().filter_map(|(_, w)| w.map(coords)).collect();
                    if lines.is_empty() {
                        continue;
                    }
                    let attrs = SPECS[2].attributes(&layers[2], r.id, false, &tags);
                    layers[2].push(Feature { fid: r.id as u64, geometry: Some(Geometry::MultiLineString(lines)), attributes: attrs });
                }
                _ => {
                    let mut parts = Vec::new();
                    for (m, w) in &members {
                        match (m.0, w) {
                            (Member::Node(n), _) => parts.extend(nodes.get(&n).cloned().map(Geometry::Point)),
                            (Member::Way(_), Some(w)) => {
                                let cs = coords(w);
                                parts.push(if w.area {
                                    Geometry::Polygon { exterior: ring(cs), interiors: vec![] }
                                } else {
                                    Geometry::LineString(cs)
                                });
                            }
                            _ => {}
                        }
                    }
                    if parts.is_empty() {
                        continue;
                    }
                    let attrs = SPECS[4].attributes(&layers[4], r.id, false, &tags);
                    layers[4].push(Feature { fid: r.id as u64, geometry: Some(Geometry::GeometryCollection(parts)), attributes: attrs });
                }
            }
        }
        for id in standalone {
            if used_as_outer.contains(&id) {
                continue;
            }
            let w = &ways[&id];
            let tags: Vec<(&str, &str)> = w.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let attrs = SPECS[3].attributes(&layers[3], id, true, &tags);
            let g = Geometry::MultiPolygon(vec![(ring(coords(w)), vec![])]);
            layers[3].push(Feature { fid: id as u64, geometry: Some(g), attributes: attrs });
        }
        layers
    }
}

/// Reads an OSM file, PBF (`.osm.pbf`) or XML (`.osm`), into GDAL's five layers.
pub fn read(path: &str) -> Result<Vec<Layer>> {
    let bytes = std::fs::read(path)?;
    if is_xml(&bytes) { parse_xml(&bytes) } else { parse(bytes) }
}

/// Whether the bytes are OSM XML rather than PBF.
pub(crate) fn is_xml(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    let head = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    head.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'<')
}

/// Parses an OSM PBF file held in memory into GDAL's five layers.
pub fn parse(bytes: Vec<u8>) -> Result<Vec<Layer>> {
    if is_xml(&bytes) {
        return parse_xml(&bytes);
    }
    let mut reader = OsmPbfReader::new(std::io::Cursor::new(bytes));
    let mut b = Builder::new();
    for obj in reader.iter() {
        let obj = obj.map_err(|e| Error::InvalidData(format!("OSM PBF: {e}")))?;
        match obj {
            OsmObj::Node(n) => {
                let c = Coord::xy(n.decimicro_lon as f64 * 1e-7, n.decimicro_lat as f64 * 1e-7);
                b.node(n.id.0, c.clone(), c, &tag_list(&n.tags));
            }
            OsmObj::Way(w) => b.way(w.id.0, w.nodes.iter().map(|n| n.0).collect(), &tag_list(&w.tags)),
            OsmObj::Relation(r) => {
                let members = r
                    .refs
                    .iter()
                    .map(|m| {
                        let id = match m.member {
                            OsmId::Node(n) => Member::Node(n.0),
                            OsmId::Way(w) => Member::Way(w.0),
                            OsmId::Relation(r) => Member::Relation(r.0),
                        };
                        (id, m.role.to_string())
                    })
                    .collect();
                let tags = r.tags.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                b.relations.push(Relation { id: r.id.0, members, tags });
            }
        }
    }
    Ok(b.finish())
}

/// Parses OSM XML into GDAL's five layers. Tags keep the file's order.
pub fn parse_xml(bytes: &[u8]) -> Result<Vec<Layer>> {
    use quick_xml::events::Event;
    let err = |e: &dyn std::fmt::Display| Error::InvalidData(format!("OSM XML: {e}"));
    let text = super::xml::decode(bytes);
    let mut reader = quick_xml::Reader::from_str(&text);
    let mut b = Builder::new();
    // The element being read: its kind, id, coordinates, node references, members and tags.
    let mut kind: Option<&'static str> = None;
    let (mut id, mut lat, mut lon) = (0i64, String::new(), String::new());
    let mut refs: Vec<i64> = Vec::new();
    let mut members: Vec<(Member, String)> = Vec::new();
    let mut tags: Vec<(String, String)> = Vec::new();
    let attr = |e: &quick_xml::events::BytesStart, name: &str| -> Option<String> {
        let a = e.attributes().flatten().find(|a| {
            let k: &str = a.key.as_ref();
            k == name
        })?;
        a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(|v| v.into_owned())
    };
    loop {
        let ev = reader.read_event().map_err(|e| err(&e))?;
        let (start, empty) = match &ev {
            Event::Start(e) => (Some(e.clone()), false),
            Event::Empty(e) => (Some(e.clone()), true),
            _ => (None, false),
        };
        if let Some(e) = start {
            let qname = e.name();
            let name: &str = qname.as_ref();
            match name {
                "node" | "way" | "relation" => {
                    kind = Some(match name {
                        "node" => "node",
                        "way" => "way",
                        _ => "relation",
                    });
                    id = attr(&e, "id").and_then(|v| v.parse().ok()).unwrap_or(0);
                    lat = attr(&e, "lat").unwrap_or_default();
                    lon = attr(&e, "lon").unwrap_or_default();
                    refs.clear();
                    members.clear();
                    tags.clear();
                }
                "tag" if kind.is_some() => {
                    if let (Some(k), Some(v)) = (attr(&e, "k"), attr(&e, "v")) {
                        tags.push((k, v));
                    }
                }
                "nd" if kind == Some("way") => refs.extend(attr(&e, "ref").and_then(|v| v.parse::<i64>().ok())),
                "member" if kind == Some("relation") => {
                    let r: i64 = attr(&e, "ref").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let m = match attr(&e, "type").as_deref() {
                        Some("node") => Member::Node(r),
                        Some("way") => Member::Way(r),
                        _ => Member::Relation(r),
                    };
                    members.push((m, attr(&e, "role").unwrap_or_default()));
                }
                _ => {}
            }
            if empty && matches!(name, "node" | "way" | "relation") {
                finish_element(&mut b, &mut kind, id, &lat, &lon, &mut refs, &mut members, &mut tags);
            }
        }
        match ev {
            Event::End(e) if matches!(<_ as AsRef<str>>::as_ref(&e.name()), "node" | "way" | "relation") => {
                finish_element(&mut b, &mut kind, id, &lat, &lon, &mut refs, &mut members, &mut tags);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(b.finish())
}

/// Hands a finished XML element to the builder.
#[allow(clippy::too_many_arguments)]
fn finish_element(
    b: &mut Builder,
    kind: &mut Option<&'static str>,
    id: i64,
    lat: &str,
    lon: &str,
    refs: &mut Vec<i64>,
    members: &mut Vec<(Member, String)>,
    tags: &mut Vec<(String, String)>,
) {
    let t: Vec<(&str, &str)> = tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    match kind.take() {
        Some("node") => {
            if let (Ok(y), Ok(x)) = (lat.trim().parse::<f64>(), lon.trim().parse::<f64>()) {
                let grid = |v: f64| (v * 1e7 + 0.5).floor() as i64 as f64 * 1e-7;
                b.node(id, Coord::xy(x, y), Coord::xy(grid(x), grid(y)), &t);
            }
        }
        Some("way") => b.way(id, std::mem::take(refs), &t),
        Some(_) => b.relations.push(Relation { id, members: std::mem::take(members), tags: std::mem::take(tags) }),
        None => {}
    }
}

fn ring(mut cs: Vec<Coord>) -> Ring {
    if cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
        cs.pop();
    }
    Ring::new(cs)
}

/// Joins open lines sharing end points into closed rings; lines that close no ring are dropped.
fn assemble_rings(mut edges: Vec<Vec<Coord>>) -> Vec<Vec<Coord>> {
    let key = |c: &Coord| (c.x.to_bits(), c.y.to_bits());
    let mut rings = Vec::new();
    while let Some(mut chain) = edges.pop() {
        while let (Some(start), Some(end)) = (chain.first().map(key), chain.last().map(key)) {
            if start == end && chain.len() > 1 {
                rings.push(chain);
                break;
            }
            let next = edges.iter().position(|e| e.first().map(key) == Some(end) || e.last().map(key) == Some(end));
            let Some(i) = next else { break };
            let mut e = edges.swap_remove(i);
            if e.first().map(key) != Some(end) {
                e.reverse();
            }
            chain.extend(e.into_iter().skip(1));
        }
    }
    rings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_tags_like_gdal() {
        assert!(is_area(&[("building", "yes")]));
        assert!(!is_area(&[("building", "yes"), ("area", "no")]));
        assert!(is_area(&[("highway", "platform")]));
        assert!(!is_area(&[("highway", "residential")]));
        let spec = &SPECS[1];
        let l = spec.layer();
        let a =
            spec.attributes(&l, 7, false, &[("highway", "primary"), ("bridge", "yes"), ("layer", "1"), ("note", "x"), ("surface", "a\"b")]);
        assert_eq!(a[0], FieldValue::Text("7".into()));
        assert_eq!(a[l.schema.field_index("z_order").unwrap()], FieldValue::Integer(27));
        assert_eq!(a[l.schema.len() - 1], FieldValue::Text(r#""bridge"=>"yes","layer"=>"1","surface"=>"a\"b""#.into()));
    }

    #[test]
    fn reads_xml() {
        let xml = r#"<?xml version="1.0"?><osm version="0.6">
            <node id="1" lat="49" lon="2"/><node id="2" lat="50" lon="3"/>
            <node id="3" lat="49.5" lon="3"><tag k="name" v="p"/><tag k="foo" v="b&amp;r"/><tag k="bar" v="baz"/></node>
            <node id="4" lat="49" lon="3"/><node id="5" lat="50" lon="2"/>
            <way id="1"><nd ref="1"/><nd ref="2"/><tag k="highway" v="motorway"/></way>
            <way id="2"><tag k="building" v="yes"/><nd ref="1"/><nd ref="4"/><nd ref="2"/><nd ref="5"/><nd ref="1"/></way>
            <relation id="7"><member type="node" ref="3" role=""/><tag k="type" v="site"/></relation>
            </osm>"#;
        let layers = parse(xml.as_bytes().to_vec()).unwrap();
        let counts: Vec<usize> = layers.iter().map(Layer::len).collect();
        assert_eq!(counts, [1, 1, 0, 1, 1]);
        let tags = layers[0].schema.len() - 1;
        // The file's tag order is kept.
        assert_eq!(layers[0].features[0].attributes[tags], FieldValue::Text(r#""foo"=>"b&r","bar"=>"baz""#.into()));
    }

    #[test]
    fn assembles_rings() {
        let l = |pts: &[(f64, f64)]| pts.iter().map(|&(x, y)| Coord::xy(x, y)).collect::<Vec<_>>();
        let rings = assemble_rings(vec![
            l(&[(0.0, 0.0), (1.0, 0.0)]),
            l(&[(1.0, 1.0), (1.0, 0.0)]),
            l(&[(1.0, 1.0), (0.0, 0.0)]),
            l(&[(5.0, 5.0), (6.0, 6.0)]),
        ]);
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0].len(), 4);
    }
}

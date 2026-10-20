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
    fn attributes(&self, schema: &wbvector::Schema, id: i64, is_way: bool, tags: &[(&str, &str)]) -> Vec<FieldValue> {
        let mut attrs = vec![FieldValue::Null; schema.len()];
        let id_field = if is_way { "osm_way_id" } else { "osm_id" };
        if let Some(i) = schema.field_index(id_field) {
            attrs[i] = FieldValue::Text(id.to_string());
        }
        let mut other = String::new();
        for (k, v) in tags {
            if let Some(i) = self.attributes.iter().position(|a| a == k) {
                attrs[schema.field_index(self.attributes[i]).unwrap_or(0)] = FieldValue::Text(v.to_string());
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
            attrs[schema.len() - 1] = FieldValue::Text(other);
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
            if let Some(i) = schema.field_index("z_order") {
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
            let attrs = SPECS[0].attributes(&self.layers[0].schema, id, false, tags);
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
                let attrs = SPECS[1].attributes(&layers[1].schema, *id, false, &tags);
                layers[1].push(Feature { fid: *id as u64, geometry: Some(Geometry::LineString(cs)), attributes: attrs });
            }
        }

        // Member ways as relations see them: their coordinates, area flag and tags.
        let member = |id: i64| {
            ways.get(&id).map(|w| MemberWay {
                coords: coords(w),
                area: w.area,
                tags: w.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect(),
            })
        };
        let node = |id: i64| nodes.get(&id).cloned();
        let used_as_outer = relation_features(
            &relations,
            &[true; 5],
            &member,
            &node,
            &layers.iter().map(|l| l.schema.clone()).collect::<Vec<_>>(),
            &mut |i, f| {
                layers[i].push(f);
            },
        );
        for id in standalone {
            if used_as_outer.contains(&id) {
                continue;
            }
            let w = &ways[&id];
            let tags: Vec<(&str, &str)> = w.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let attrs = SPECS[3].attributes(&layers[3].schema, id, true, &tags);
            let g = Geometry::MultiPolygon(vec![(ring(coords(w)), vec![])]);
            layers[3].push(Feature { fid: id as u64, geometry: Some(g), attributes: attrs });
        }
        layers
    }
}

/// A relation's member way: its coordinates (as [`Builder`] computes them), area flag and tags.
struct MemberWay<'a> {
    coords: Vec<Coord>,
    area: bool,
    tags: Vec<(&'a str, &'a str)>,
}

/// The features of the relations, in order, to `out` with their layer index: `multipolygon` and
/// `boundary` relations to `multipolygons`, `multilinestring` and `route` relations to
/// `multilinestrings`, the others to `other_relations`; only those of the layers `wanted` are
/// built. Returns the closed ways used as outer rings, which make no polygon of their own.
/// Relations are built in parallel.
fn relation_features<'a>(
    relations: &[Relation],
    wanted: &[bool; 5],
    way: &(dyn Fn(i64) -> Option<MemberWay<'a>> + Sync),
    node: &(dyn Fn(i64) -> Option<Coord> + Sync),
    schemas: &[wbvector::Schema],
    out: &mut dyn FnMut(usize, Feature),
) -> std::collections::HashSet<i64> {
    let built = crate::par::map_range(relations.len(), |i| relation_feature(&relations[i], wanted, way, node, schemas));
    let mut used_as_outer = std::collections::HashSet::new();
    for (feature, outer) in built {
        used_as_outer.extend(outer);
        if let Some((l, f)) = feature {
            out(l, f);
        }
    }
    used_as_outer
}

/// A relation's feature and layer index, if it makes one, and the closed ways it uses as outer
/// rings.
fn relation_feature<'a>(
    r: &Relation,
    wanted: &[bool; 5],
    way: &(dyn Fn(i64) -> Option<MemberWay<'a>> + Sync),
    node: &(dyn Fn(i64) -> Option<Coord> + Sync),
    schemas: &[wbvector::Schema],
) -> (Option<(usize, Feature)>, Vec<i64>) {
    let mut outer = Vec::new();
    let rtype = r.tags.iter().find(|(k, _)| k == "type").map(|(_, v)| v.as_str());
    if !wanted[relation_layer(rtype)] {
        return (None, outer);
    }
    let tags: Vec<(&str, &str)> = r.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let interesting = tags.iter().any(|(k, _)| *k != "type" && *k != "created_by");
    let members: Vec<(&(Member, String), Option<MemberWay>)> =
        r.members.iter().filter(|m| m.1 != "subarea").map(|m| (m, if let Member::Way(w) = m.0 { way(w) } else { None })).collect();
    match rtype {
        Some("multipolygon" | "boundary") => {
            let way_members: Vec<_> = members.into_iter().filter(|(m, _)| matches!(m.0, Member::Way(_))).collect();
            if way_members.iter().any(|(_, w)| w.is_none()) {
                return (None, outer); // missing ways
            }
            let mut rings = Vec::new();
            let mut edges = Vec::new();
            let mut first_outer_tags = None;
            for (m, w) in way_members {
                let Some(w) = w else { unreachable!() };
                let cs = w.coords;
                if first_outer_tags.is_none() && m.1 == "outer" {
                    first_outer_tags = Some(w.tags);
                }
                let closed = !cs.is_empty() && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y));
                if closed {
                    if m.1 == "outer"
                        && let Member::Way(id) = m.0
                    {
                        outer.push(id);
                    }
                    rings.push(cs);
                } else if !cs.is_empty() {
                    edges.push(cs);
                }
            }
            rings.extend(assemble_rings(edges));
            let rings: Vec<Vec<Coord>> = rings.into_iter().filter(|r| r.len() >= 4).collect();
            let Some(g) = super::polygons_from_rings(rings) else { return (None, outer) };
            let g = match g {
                Geometry::Polygon { exterior, interiors } => Geometry::MultiPolygon(vec![(exterior, interiors)]),
                g => g,
            };
            // A relation tagged only with its type takes the tags of its first outer way.
            let mut owned: Vec<(&str, &str)>;
            let tags = if interesting {
                &tags
            } else {
                owned = first_outer_tags.unwrap_or_default();
                owned.push(("type", rtype.unwrap_or_default()));
                &owned
            };
            let attrs = SPECS[3].attributes(&schemas[3], r.id, false, tags);
            (Some((3, Feature { fid: r.id as u64, geometry: Some(g), attributes: attrs })), outer)
        }
        Some("multilinestring" | "route") => {
            let lines: Vec<Vec<Coord>> = members.into_iter().filter_map(|(_, w)| w.map(|w| w.coords)).collect();
            if lines.is_empty() {
                return (None, outer);
            }
            let attrs = SPECS[2].attributes(&schemas[2], r.id, false, &tags);
            (Some((2, Feature { fid: r.id as u64, geometry: Some(Geometry::MultiLineString(lines)), attributes: attrs })), outer)
        }
        _ => {
            let mut parts = Vec::new();
            for (m, w) in members {
                match (m.0, w) {
                    (Member::Node(n), _) => parts.extend(node(n).map(Geometry::Point)),
                    (Member::Way(_), Some(w)) => {
                        let cs = w.coords;
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
                return (None, outer);
            }
            let attrs = SPECS[4].attributes(&schemas[4], r.id, false, &tags);
            (Some((4, Feature { fid: r.id as u64, geometry: Some(Geometry::GeometryCollection(parts)), attributes: attrs })), outer)
        }
    }
}

/// The layer a relation of type `rtype` goes to (see [`relation_features`]).
fn relation_layer(rtype: Option<&str>) -> usize {
    match rtype {
        Some("multipolygon" | "boundary") => 3,
        Some("multilinestring" | "route") => 2,
        _ => 4,
    }
}

/// Reads an OSM file, PBF (`.osm.pbf`) or XML (`.osm`), into GDAL's five layers.
pub fn read(path: &str) -> Result<Vec<Layer>> {
    let mut file = std::fs::File::open(path)?;
    if is_sorted_pbf(&mut file)
        && let Some(layers) = read_sorted_layers(&mut file)?
    {
        return Ok(layers);
    }
    let bytes = std::fs::read(path)?;
    if is_xml(&bytes) { parse_xml(&bytes) } else { parse(bytes) }
}

/// GDAL's five layers of a PBF file sorted by type then id; `None` for other files, those that
/// say they are sorted but are not among them (the reader holding everything reads those).
fn read_sorted_layers(input: &mut dyn PbfInput) -> Result<Option<Vec<Layer>>> {
    let mut layers: Vec<Layer> = SPECS.iter().map(Spec::layer).collect();
    let sorted = read_sorted_pbf(input, [true; 5], &mut |l, features| {
        layers[l].features.extend(features);
        Ok(())
    });
    match sorted {
        Ok(sorted) => Ok(sorted.then_some(layers)),
        Err(e) if is_unsorted(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

const UNSORTED: &str = "OSM PBF: the header says the file is sorted by type then id, but it is not";

/// Whether [`read_sorted_pbf`] failed because the file is not sorted as its header says: the
/// file can still be read whole.
pub(crate) fn is_unsorted(e: &Error) -> bool {
    matches!(e, Error::InvalidData(m) if m == UNSORTED)
}

/// Whether the bytes are OSM XML rather than PBF.
pub(crate) fn is_xml(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(512)];
    let head = head.strip_prefix(b"\xef\xbb\xbf").unwrap_or(head);
    head.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'<')
}

/// A coordinate on the 1e-7 degree grid GDAL stores nodes on (`DBL_TO_INT`, then `INT_TO_DBL`),
/// where the vertices of ways are.
fn grid(v: f64) -> f64 {
    (v * 1e7 + 0.5).floor() as i64 as f64 / 1e7
}

/// Parses an OSM PBF file held in memory into GDAL's five layers.
pub fn parse(bytes: Vec<u8>) -> Result<Vec<Layer>> {
    if is_xml(&bytes) {
        return parse_xml(&bytes);
    }
    if let Some(layers) = read_sorted_layers(&mut std::io::Cursor::new(&bytes))? {
        return Ok(layers);
    }
    parse_any_order(bytes)
}

/// Parses a PBF file whatever the order of its objects, holding them all.
fn parse_any_order(bytes: Vec<u8>) -> Result<Vec<Layer>> {
    let mut reader = OsmPbfReader::new(std::io::Cursor::new(bytes));
    let mut b = Builder::new();
    for obj in reader.iter() {
        let obj = obj.map_err(|e| Error::InvalidData(format!("OSM PBF: {e}")))?;
        match obj {
            OsmObj::Node(n) => {
                // GDAL's arithmetic: nanodegrees times 1e-9 for points, and its grid for vertices.
                let nano = |d: i32| 1e-9 * (i64::from(d) * 100) as f64;
                let c = Coord::xy(nano(n.decimicro_lon), nano(n.decimicro_lat));
                b.node(n.id.0, c.clone(), Coord::xy(grid(c.x), grid(c.y)), &tag_list(&n.tags));
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

// PBF files sorted by type then id, read a batch of blocks at a time.

/// A protobuf field's value: a varint, or length-delimited bytes.
enum Pb<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}

fn pb_varint(b: &[u8], i: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *b.get(*i)?;
        *i += 1;
        v |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// The varint and length-delimited fields of a protobuf message, as `(number, value)`.
fn pb_fields(b: &[u8]) -> Option<Vec<(u64, Pb<'_>)>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let key = pb_varint(b, &mut i)?;
        match key & 7 {
            0 => out.push((key >> 3, Pb::Int(pb_varint(b, &mut i)?))),
            1 => i += 8,
            2 => {
                let n = usize::try_from(pb_varint(b, &mut i)?).ok()?;
                out.push((key >> 3, Pb::Bytes(b.get(i..i.checked_add(n)?)?)));
                i += n;
            }
            5 => i += 4,
            _ => return None,
        }
    }
    Some(out)
}

/// A PBF file being read: in memory, or a file read a batch of blocks at a time.
pub(crate) trait PbfInput: std::io::Read + std::io::Seek {}
impl<T: std::io::Read + std::io::Seek> PbfInput for T {}

/// The spans of a PBF file's data blobs (each with its length prefix and header, as
/// `OsmPbfReader` reads them), and whether its header says it is sorted by type then id. Only
/// the blob headers (and the file header) are read.
fn blob_spans(r: &mut dyn PbfInput) -> Result<(Vec<std::ops::Range<u64>>, bool)> {
    use std::io::{Read, SeekFrom};
    let bad = || Error::InvalidData("OSM PBF: bad blob".into());
    let len = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let mut r = std::io::BufReader::with_capacity(1 << 16, r);
    let mut spans = Vec::new();
    let mut sorted = false;
    let mut o = 0u64;
    while o < len {
        let mut prefix = [0u8; 4];
        r.read_exact(&mut prefix).map_err(|_| bad())?;
        let header_len = u32::from_be_bytes(prefix) as usize;
        if header_len > 64 * 1024 {
            return Err(bad());
        }
        let mut header = vec![0u8; header_len];
        r.read_exact(&mut header).map_err(|_| bad())?;
        let (mut kind, mut size) = (&[][..], 0u64);
        for (field, value) in pb_fields(&header).ok_or_else(bad)? {
            match (field, value) {
                (1, Pb::Bytes(t)) => kind = t,
                (3, Pb::Int(n)) => size = n,
                _ => {}
            }
        }
        let start = o + 4 + header_len as u64;
        let end = start.checked_add(size).filter(|&e| e <= len).ok_or_else(bad)?;
        match kind {
            b"OSMData" => {
                spans.push(o..end);
                r.seek_relative(size as i64)?;
            }
            b"OSMHeader" => {
                let mut blob = vec![0u8; size as usize];
                r.read_exact(&mut blob)?;
                sorted = header_says_sorted(&blob).ok_or_else(bad)?;
            }
            _ => r.seek_relative(size as i64)?,
        }
        o = end;
    }
    Ok((spans, sorted))
}

/// The bytes of consecutive blobs.
fn read_spans(r: &mut dyn PbfInput, spans: &[std::ops::Range<u64>]) -> Result<Vec<u8>> {
    let (Some(first), Some(last)) = (spans.first(), spans.last()) else { return Ok(Vec::new()) };
    let mut buf = vec![0u8; (last.end - first.start) as usize];
    r.seek(std::io::SeekFrom::Start(first.start))?;
    r.read_exact(&mut buf)?;
    Ok(buf)
}

/// Whether a header blob lists the `Sort.Type_then_ID` feature: nodes, then ways, then
/// relations, each by increasing id (as osmium, Geofabrik and the planet files write them).
fn header_says_sorted(blob: &[u8]) -> Option<bool> {
    let mut header = Vec::new();
    for (field, value) in pb_fields(blob)? {
        match (field, value) {
            (1, Pb::Bytes(raw)) => header = raw.to_vec(),
            (3, Pb::Bytes(zlib)) => {
                use std::io::Read;
                flate2::read::ZlibDecoder::new(zlib).read_to_end(&mut header).ok()?;
            }
            _ => {}
        }
    }
    let feature = b"Sort.Type_then_ID";
    Some(header.windows(feature.len()).any(|w| w == feature))
}

fn decode_block(span: &[u8]) -> Result<osmpbfreader::osmformat::PrimitiveBlock> {
    let err = |e: osmpbfreader::Error| Error::InvalidData(format!("OSM PBF: {e}"));
    let blob = OsmPbfReader::new(span).blobs().next().ok_or_else(|| Error::InvalidData("OSM PBF: bad blob".into()))?.map_err(err)?;
    osmpbfreader::primitive_block_from_blob(&blob).map_err(err)
}

fn relation(r: &osmpbfreader::Relation) -> Relation {
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
    Relation { id: r.id.0, members, tags }
}

/// GDAL's coordinates of a node from its decimicro degrees: nanodegrees times 1e-9 for points,
/// and its grid for way vertices.
fn node_coords(lon: i32, lat: i32) -> (Coord, Coord) {
    let nano = |d: i32| 1e-9 * (i64::from(d) * 100) as f64;
    let c = Coord::xy(nano(lon), nano(lat));
    let stored = Coord::xy(grid(c.x), grid(c.y));
    (c, stored)
}

/// Bytes appended in order, then read in place. Past 16 MB they go to a temporary file, read
/// back through a memory map once [`Spool::seal`]ed: the operating system's page cache holds them
/// rather than the process, and [`Spool::release`] drops the pages read from the process's memory.
#[derive(Default)]
struct Spool {
    buf: Vec<u8>,
    written: u64,
    #[cfg(not(target_arch = "wasm32"))]
    file: Option<std::fs::File>,
    #[cfg(not(target_arch = "wasm32"))]
    map: Option<memmap2::Mmap>,
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
const SPOOL_PAGE: usize = 1 << 24;

impl Spool {
    /// The number of bytes appended.
    fn len(&self) -> u64 {
        self.written + self.buf.len() as u64
    }

    /// Writes the bytes appended to the file when they reach a page.
    fn flush_full(&mut self) -> Result<()> {
        #[cfg(not(target_arch = "wasm32"))]
        if self.buf.len() >= SPOOL_PAGE {
            let file = match &mut self.file {
                Some(f) => f,
                None => self.file.insert(tempfile::tempfile()?),
            };
            std::io::Write::write_all(file, &self.buf)?;
            self.written += self.buf.len() as u64;
            self.buf.clear();
        }
        Ok(())
    }

    /// Ends the appending: what went to the file is mapped, with the rest written after it.
    fn seal(&mut self) -> Result<()> {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(file) = &mut self.file
            && self.map.is_none()
        {
            std::io::Write::write_all(file, &self.buf)?;
            self.written += self.buf.len() as u64;
            self.buf = Vec::new();
            // SAFETY: the file is our own unnamed temporary file, not changed while mapped.
            let map = unsafe { memmap2::Mmap::map(&*file)? };
            #[cfg(unix)]
            let _ = map.advise(memmap2::Advice::Random);
            self.map = Some(map);
        }
        Ok(())
    }

    /// The bytes appended: all of them once sealed, else those not yet written to the file.
    fn bytes(&self) -> &[u8] {
        #[cfg(not(target_arch = "wasm32"))]
        {
            debug_assert!(self.file.is_none() || self.map.is_some(), "a spool past a page is read once sealed");
            if let Some(map) = &self.map {
                return map;
            }
        }
        &self.buf
    }

    /// Drops the mapped pages read so far from the process's memory; they stay in the page cache.
    fn release(&self) {
        #[cfg(all(unix, not(target_arch = "wasm32")))]
        if let Some(map) = &self.map {
            // SAFETY: the mapping is read only and backed by the file, so its pages read back
            // unchanged.
            let _ = unsafe { map.unchecked_advise(memmap2::UncheckedAdvice::DontNeed) };
        }
    }
}

/// The nodes of a sorted file, by increasing id, in groups of 16: each group's first id and
/// where it starts, then each node as varints (the id's difference from the previous one, the
/// zigzag differences of its longitude and latitude in decimicro degrees). About 6 bytes a node,
/// in a [`Spool`], sealed before nodes are looked up.
#[derive(Default)]
struct NodeStore {
    groups: Vec<(i64, u64)>,
    bytes: Spool,
    in_group: usize,
    last: (i64, i32, i32),
}

const NODE_GROUP: usize = 16;

fn put_varint(b: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        b.push(v as u8 | 0x80);
        v >>= 7;
    }
    b.push(v as u8);
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    (v >> 1) as i64 ^ -((v & 1) as i64)
}

impl NodeStore {
    /// Adds a node; `false` when its id is not above the last one.
    fn push(&mut self, id: i64, lon: i32, lat: i32) -> Result<bool> {
        if !self.groups.is_empty() && id <= self.last.0 {
            return Ok(false);
        }
        if self.groups.is_empty() || self.in_group == NODE_GROUP {
            self.bytes.flush_full()?;
            self.groups.push((id, self.bytes.len()));
            self.in_group = 0;
            self.last = (id, 0, 0);
        }
        let (last_id, last_lon, last_lat) = self.last;
        let b = &mut self.bytes.buf;
        put_varint(b, (id - last_id) as u64);
        put_varint(b, zigzag(i64::from(lon) - i64::from(last_lon)));
        put_varint(b, zigzag(i64::from(lat) - i64::from(last_lat)));
        self.last = (id, lon, lat);
        self.in_group += 1;
        Ok(true)
    }

    /// A node's coordinates on GDAL's grid.
    fn get(&self, id: i64) -> Option<Coord> {
        self.get_raw(id).map(|[lon, lat]| node_coords(lon, lat).1)
    }

    /// A node's longitude and latitude, in decimicro degrees.
    fn get_raw(&self, id: i64) -> Option<[i32; 2]> {
        let g = self.groups.partition_point(|&(first, _)| first <= id).checked_sub(1)?;
        let data = self.bytes.bytes();
        let end = self.groups.get(g + 1).map_or(data.len(), |&(_, o)| o as usize);
        let mut i = self.groups[g].1 as usize;
        let (mut nid, mut lon, mut lat) = (self.groups[g].0, 0i64, 0i64);
        for k in 0..NODE_GROUP {
            if i >= end {
                return None;
            }
            let d = pb_varint(data, &mut i)? as i64;
            nid = if k == 0 { nid } else { nid + d };
            lon += unzigzag(pb_varint(data, &mut i)?);
            lat += unzigzag(pb_varint(data, &mut i)?);
            if nid == id {
                return Some([lon as i32, lat as i32]);
            }
            if nid > id {
                return None;
            }
        }
        None
    }
}

/// What a block of a sorted file holds: its nodes `(id, lon, lat)` in decimicro degrees, its
/// points (when wanted), its ways, and what kinds of objects it has.
struct Block {
    nodes: Vec<(i64, i32, i32)>,
    points: Vec<Feature>,
    ways: Vec<osmpbfreader::Way>,
    has_nodes: bool,
    has_relations: bool,
    out_of_order: bool,
}

fn read_block(span: &[u8], points: bool, schema: &wbvector::Schema) -> Result<Block> {
    let block = decode_block(span)?;
    let mut b =
        Block { nodes: Vec::new(), points: Vec::new(), ways: Vec::new(), has_nodes: false, has_relations: false, out_of_order: false };
    for obj in osmpbfreader::blocks::iter(&block) {
        match obj {
            OsmObj::Node(nd) => {
                b.out_of_order |= !b.ways.is_empty() || b.has_relations;
                b.has_nodes = true;
                b.nodes.push((nd.id.0, nd.decimicro_lon, nd.decimicro_lat));
                if points {
                    let tags = tag_list(&nd.tags);
                    if tags.iter().any(|(k, _)| !POINT_INSIGNIFICANT.contains(k)) {
                        let attrs = SPECS[0].attributes(schema, nd.id.0, false, &tags);
                        let point = node_coords(nd.decimicro_lon, nd.decimicro_lat).0;
                        b.points.push(Feature { fid: nd.id.0 as u64, geometry: Some(Geometry::Point(point)), attributes: attrs });
                    }
                }
            }
            OsmObj::Way(w) => {
                b.out_of_order |= b.has_relations;
                b.ways.push(w);
            }
            OsmObj::Relation(_) => b.has_relations = true,
        }
    }
    Ok(b)
}

/// A way of a sorted file, with what it gives: a line, a candidate area (written to the spill
/// until the relations say whether it stands alone), and its coordinates for the relations
/// using it.
struct WayOut {
    id: i64,
    line: Option<Feature>,
    area: Option<Vec<u8>>,
    member: Option<(Vec<u8>, usize)>,
}

/// Where a way relations use starts in its [`Spool`], and its number of coordinates.
type MemberAt = (u64, usize);

/// A way relations use, as spooled: its number of coordinates, its coordinates (the zigzag
/// differences of its longitudes and latitudes in decimicro degrees), its area flag and its tags.
fn encode_member(raw: &[[i32; 2]], area: bool, tags: &[(&str, &str)]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + 4 * raw.len());
    put_varint(&mut b, raw.len() as u64);
    let mut last = [0i64; 2];
    for c in raw {
        for (v, l) in c.iter().zip(&mut last) {
            put_varint(&mut b, zigzag(i64::from(*v) - *l));
            *l = i64::from(*v);
        }
    }
    b.push(u8::from(area));
    put_varint(&mut b, tags.len() as u64);
    for s in tags.iter().flat_map(|(k, v)| [k, v]) {
        put_varint(&mut b, s.len() as u64);
        b.extend_from_slice(s.as_bytes());
    }
    b
}

/// A spooled way (see [`encode_member`]) as relations see it, from `b[at..]`.
fn decode_member(b: &[u8], mut at: usize) -> Option<MemberWay<'_>> {
    let n = pb_varint(b, &mut at)? as usize;
    let mut coords = Vec::with_capacity(n);
    let mut last = [0i64; 2];
    for _ in 0..n {
        for l in &mut last {
            *l += unzigzag(pb_varint(b, &mut at)?);
        }
        coords.push(node_coords(last[0] as i32, last[1] as i32).1);
    }
    let area = *b.get(at)? != 0;
    at += 1;
    let mut tags = Vec::new();
    for _ in 0..pb_varint(b, &mut at)? {
        let mut pair = ["", ""];
        for s in &mut pair {
            let len = pb_varint(b, &mut at)? as usize;
            *s = std::str::from_utf8(b.get(at..at.checked_add(len)?)?).ok()?;
            at += len;
        }
        tags.push((pair[0], pair[1]));
    }
    Some(MemberWay { coords, area, tags })
}

/// What a way of a sorted file gives (see [`Builder::finish`], which this mirrors).
fn way_out(
    w: &osmpbfreader::Way,
    nodes: &NodeStore,
    wanted: &[bool; 5],
    members: &std::collections::HashSet<i64>,
    schema: &wbvector::Schema,
) -> WayOut {
    let id = w.id.0;
    let tags = tag_list(&w.tags);
    let closed = w.nodes.len() > 1 && w.nodes.first() == w.nodes.last();
    let area = closed && is_area(&tags);
    let is_member = members.contains(&id);
    let mut out = WayOut { id, line: None, area: None, member: None };
    if !(wanted[1] || wanted[3] || is_member) {
        return out;
    }
    let n = if area { w.nodes.len() - 1 } else { w.nodes.len() };
    let mut raw: Vec<[i32; 2]> = w.nodes[..n].iter().filter_map(|n| nodes.get_raw(n.0)).collect();
    if area && !raw.is_empty() {
        raw.push(raw[0]);
    }
    let cs: Vec<Coord> = raw.iter().map(|&[lon, lat]| node_coords(lon, lat).1).collect();
    if w.nodes.len() >= 2 && cs.len() >= 2 {
        if area {
            const STORED_WITHOUT: [&str; 7] = ["area", "created_by", "converted_by", "note", "todo", "fixme", "FIXME"];
            if wanted[3] && tags.iter().any(|(k, _)| !STORED_WITHOUT.contains(k)) {
                out.area = Some(encode_area(id, &cs, &tags));
            }
        } else if wanted[1] && !tags.is_empty() {
            let attrs = SPECS[1].attributes(schema, id, false, &tags);
            out.line = Some(Feature { fid: id as u64, geometry: Some(Geometry::LineString(cs.clone())), attributes: attrs });
        }
    }
    if is_member {
        out.member = Some((encode_member(&raw, area, &tags), raw.len()));
    }
    out
}

/// A candidate area as spilled: id, coordinates and tags.
fn encode_area(id: i64, cs: &[Coord], tags: &[(&str, &str)]) -> Vec<u8> {
    let mut b = Vec::with_capacity(16 + 16 * cs.len());
    b.extend_from_slice(&id.to_le_bytes());
    b.extend_from_slice(&(cs.len() as u32).to_le_bytes());
    for c in cs {
        b.extend_from_slice(&c.x.to_le_bytes());
        b.extend_from_slice(&c.y.to_le_bytes());
    }
    b.extend_from_slice(&(tags.len() as u32).to_le_bytes());
    for (k, v) in tags {
        for s in [k, v] {
            b.extend_from_slice(&(s.len() as u32).to_le_bytes());
            b.extend_from_slice(s.as_bytes());
        }
    }
    b
}

/// A spilled area: its way id, coordinates and tags.
type Area = (i64, Vec<Coord>, Vec<(String, String)>);

/// Reads the next spilled area: `None` at the end.
fn read_area(r: &mut impl std::io::Read) -> Result<Option<Area>> {
    // The end of the file only before a record: one cut short is an error.
    let mut b8 = [0u8; 8];
    let mut got = 0;
    while got < 8 {
        match r.read(&mut b8[got..]) {
            Ok(0) if got == 0 => return Ok(None),
            Ok(0) => return Err(Error::InvalidData("OSM PBF: spilled area cut short".into())),
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    let id = i64::from_le_bytes(b8);
    let u32_ = |r: &mut dyn std::io::Read| -> Result<usize> {
        let mut b = [0u8; 4];
        r.read_exact(&mut b)?;
        Ok(u32::from_le_bytes(b) as usize)
    };
    let n = u32_(r)?;
    let mut cs = Vec::with_capacity(n);
    for _ in 0..n {
        let mut b = [0u8; 16];
        r.read_exact(&mut b)?;
        let x = f64::from_le_bytes(b[..8].try_into().unwrap_or_default());
        let y = f64::from_le_bytes(b[8..].try_into().unwrap_or_default());
        cs.push(Coord::xy(x, y));
    }
    let mut tags = Vec::new();
    for _ in 0..u32_(r)? {
        let mut pair = [String::new(), String::new()];
        for s in &mut pair {
            let mut b = vec![0u8; u32_(r)?];
            r.read_exact(&mut b)?;
            *s = String::from_utf8(b).map_err(|_| Error::InvalidData("OSM PBF: spilled tag".into()))?;
        }
        let [k, v] = pair;
        tags.push((k, v));
    }
    Ok(Some((id, cs, tags)))
}

/// The relations in chunks built at once: at most 8192 relations, whose member ways (of the
/// layers `wanted`) hold at most about four million coordinates, unless one relation has more, so
/// that a run of large boundaries or long routes does not hold them all at once.
fn relation_chunks<'r>(
    relations: &'r [Relation],
    wanted: &[bool; 5],
    member_ways: &HashMap<i64, MemberAt>,
) -> impl Iterator<Item = &'r [Relation]> {
    const MAX_RELATIONS: usize = 8192;
    const MAX_COORDS: usize = 1 << 22;
    let size = |r: &Relation| -> usize {
        let rtype = r.tags.iter().find(|(k, _)| k == "type").map(|(_, v)| v.as_str());
        if !wanted[relation_layer(rtype)] {
            return 0;
        }
        r.members.iter().filter_map(|m| if let Member::Way(w) = m.0 { member_ways.get(&w).map(|c| c.1) } else { None }).sum()
    };
    let mut start = 0;
    std::iter::from_fn(move || {
        if start == relations.len() {
            return None;
        }
        let (mut end, mut coords) = (start, 0);
        while end < relations.len() && end - start < MAX_RELATIONS {
            let n = size(&relations[end]);
            if end > start && coords + n > MAX_COORDS {
                break;
            }
            coords += n;
            end += 1;
        }
        let chunk = &relations[start..end];
        start = end;
        Some(chunk)
    })
}

/// Whether a PBF file says it is sorted by type then id, so that [`read_sorted_pbf`] reads it:
/// only its first blob, the file header, is read. `false` for other files (OSM XML, vector tiles).
pub(crate) fn is_sorted_pbf(input: &mut dyn PbfInput) -> bool {
    let mut header = || -> Option<bool> {
        input.seek(std::io::SeekFrom::Start(0)).ok()?;
        let mut prefix = [0u8; 4];
        input.read_exact(&mut prefix).ok()?;
        let len = u32::from_be_bytes(prefix) as usize;
        if len > 64 * 1024 {
            return None;
        }
        let mut h = vec![0u8; len];
        input.read_exact(&mut h).ok()?;
        let (mut kind, mut size) = (&[][..], 0u64);
        for (field, value) in pb_fields(&h)? {
            match (field, value) {
                (1, Pb::Bytes(t)) => kind = t,
                (3, Pb::Int(n)) => size = n,
                _ => {}
            }
        }
        if kind != b"OSMHeader" || size > 64 << 20 {
            return None;
        }
        let mut blob = vec![0u8; size as usize];
        input.read_exact(&mut blob).ok()?;
        header_says_sorted(&blob)
    };
    header().unwrap_or(false)
}

/// The schemas of GDAL's five layers (no features).
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn layer_schemas() -> Vec<Layer> {
    SPECS.iter().map(Spec::layer).collect()
}

/// Reads a PBF file sorted by type then id (as its header says) into GDAL's five layers, a
/// batch of blocks at a time, decoded in parallel: `emit` gets each layer's features in GDAL's
/// order, as `(layer index, features)`, for the layers `wanted`. Only the nodes (about 6 bytes each)
/// and the ways relations use, in [`Spool`]s, and the areas waiting for the relations, in a
/// temporary file, are kept. `false`, having emitted nothing, when the header doesn't say the file is sorted.
pub(crate) fn read_sorted_pbf(
    input: &mut dyn PbfInput,
    wanted: [bool; 5],
    emit: &mut dyn FnMut(usize, Vec<Feature>) -> Result<()>,
) -> Result<bool> {
    let (spans, sorted) = blob_spans(input)?;
    if !sorted {
        return Ok(false);
    }
    let unsorted = || Error::InvalidData(UNSORTED.into());
    let schemas: Vec<wbvector::Schema> = SPECS.iter().map(|s| s.layer().schema).collect();
    let need_relations = wanted[2] || wanted[3] || wanted[4];

    // The relations, from the blocks at the end, and the ways they use.
    let mut tail: Vec<Vec<Relation>> = Vec::new();
    let mut last = spans.len();
    if need_relations {
        while last > 0 {
            let block = decode_block(&read_spans(input, &spans[last - 1..last])?)?;
            let mut rels = Vec::new();
            let mut only_relations = true;
            for obj in osmpbfreader::blocks::iter(&block) {
                match obj {
                    OsmObj::Relation(r) => rels.push(relation(&r)),
                    _ => only_relations = false,
                }
            }
            tail.push(rels);
            if !only_relations {
                break;
            }
            last -= 1;
        }
    }
    let relations: Vec<Relation> = tail.into_iter().rev().flatten().collect();
    let members: std::collections::HashSet<i64> =
        relations.iter().flat_map(|r| r.members.iter().filter_map(|m| if let Member::Way(w) = m.0 { Some(w) } else { None })).collect();

    // Nodes, then ways, a batch of blocks at a time.
    let mut nodes = NodeStore::default();
    // The ways relations use, in a spool of their own.
    let mut member_ways: HashMap<i64, MemberAt> = HashMap::new();
    let mut members_spool = Spool::default();
    // Candidate areas wait for the relations in a temporary file.
    let mut areas = std::io::BufWriter::with_capacity(1 << 20, super::spill()?);
    let (mut in_ways, mut in_relations) = (false, false);
    let batch = crate::par::threads().max(1) * 2;
    // Blocks from `last` on hold only relations; the one before, the last read here, may hold the
    // first relations too (read above).
    let end = if need_relations { last } else { spans.len() };
    let mut i = 0;
    while i < end {
        let n = batch.min(end - i);
        let bytes = read_spans(input, &spans[i..i + n])?;
        let base = spans[i].start;
        let blocks = crate::par::map_range(n, |k| {
            let span = &spans[i + k];
            read_block(&bytes[(span.start - base) as usize..(span.end - base) as usize], wanted[0], &schemas[0])
        });
        for (k, block) in blocks.into_iter().enumerate() {
            let block = block?;
            let at_relations = need_relations && i + k + 1 == end;
            let has_ways = !block.ways.is_empty();
            if block.out_of_order
                || (block.has_nodes && in_ways)
                || ((block.has_nodes || has_ways) && in_relations)
                || (block.has_relations && need_relations && !at_relations)
            {
                return Err(unsorted());
            }
            in_relations |= block.has_relations;
            for &(id, lon, lat) in &block.nodes {
                if !nodes.push(id, lon, lat)? {
                    return Err(unsorted());
                }
            }
            if !block.points.is_empty() {
                emit(0, block.points)?;
            }
            if block.ways.is_empty() {
                continue;
            }
            if !in_ways {
                nodes.bytes.seal()?;
            }
            in_ways = true;
            let ways = block.ways;
            let outs = crate::par::map_range(ways.len(), |w| way_out(&ways[w], &nodes, &wanted, &members, &schemas[1]));
            let mut lines = Vec::new();
            for o in outs {
                lines.extend(o.line);
                if let Some(a) = o.area {
                    std::io::Write::write_all(&mut areas, &a)?;
                }
                if let Some((m, n)) = o.member {
                    member_ways.insert(o.id, (members_spool.len(), n));
                    members_spool.buf.extend_from_slice(&m);
                    members_spool.flush_full()?;
                }
            }
            if !lines.is_empty() {
                emit(1, lines)?;
            }
            nodes.bytes.release();
        }
        i += n;
    }
    nodes.bytes.seal()?;
    members_spool.seal()?;

    // The relations, a chunk at a time, then the areas standing alone.
    let member = |id: i64| member_ways.get(&id).and_then(|&(at, _)| decode_member(members_spool.bytes(), at as usize));
    let node = |id: i64| nodes.get(id);
    let mut used_as_outer = std::collections::HashSet::new();
    for rels in relation_chunks(&relations, &wanted, &member_ways) {
        let mut out: Vec<Vec<Feature>> = vec![Vec::new(); 5];
        used_as_outer.extend(relation_features(rels, &wanted, &member, &node, &schemas, &mut |l, f| out[l].push(f)));
        for (l, features) in out.into_iter().enumerate() {
            if wanted[l] && !features.is_empty() {
                emit(l, features)?;
            }
        }
        nodes.bytes.release();
        members_spool.release();
    }
    drop(nodes);
    drop(member_ways);
    drop(members_spool);
    let mut spilled = areas.into_inner().map_err(|e| Error::Io(e.into_error()))?;
    spilled.seek(std::io::SeekFrom::Start(0))?;
    let mut r = std::io::BufReader::with_capacity(1 << 20, spilled);
    loop {
        let mut chunk = Vec::new();
        while chunk.len() < 16384
            && let Some(area) = read_area(&mut r)?
        {
            if !used_as_outer.contains(&area.0) {
                chunk.push(area);
            }
        }
        if chunk.is_empty() {
            return Ok(true);
        }
        let features = crate::par::map_range(chunk.len(), |k| {
            let (id, cs, tags) = &chunk[k];
            let tags: Vec<(&str, &str)> = tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let attrs = SPECS[3].attributes(&schemas[3], *id, true, &tags);
            Feature { fid: *id as u64, geometry: Some(Geometry::MultiPolygon(vec![(ring(cs.clone()), vec![])])), attributes: attrs }
        });
        emit(3, features)?;
    }
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
        let a = spec.attributes(
            &l.schema,
            7,
            false,
            &[("highway", "primary"), ("bridge", "yes"), ("layer", "1"), ("note", "x"), ("surface", "a\"b")],
        );
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

    /// The sorted-file reader against the reader holding everything, on the test data's PBF files
    /// (and `TERRIO_OSM_FILE`, a larger one, when set).
    #[test]
    fn sorted_files_read_like_any_order() {
        let mut files: Vec<std::path::PathBuf> = std::env::var_os("TERRIO_OSM_FILE").map(Into::into).into_iter().collect();
        if let Some(dir) = std::env::var_os("TERRIO_TEST_DATA") {
            files.extend(
                std::fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.to_string_lossy().ends_with(".osm.pbf")),
            );
        }
        for path in files {
            let bytes = std::fs::read(&path).unwrap();
            let sorted = read_sorted_layers(&mut std::io::Cursor::new(&bytes)).unwrap().expect("a sorted file");
            let any = parse_any_order(bytes).unwrap();
            for (a, b) in sorted.iter().zip(&any) {
                assert_eq!((a.name.as_str(), a.len()), (b.name.as_str(), b.len()), "{}", path.display());
                for (x, y) in a.features.iter().zip(&b.features) {
                    assert!(
                        x.fid == y.fid && x.attributes == y.attributes && x.geometry == y.geometry,
                        "{} {} {}",
                        path.display(),
                        a.name,
                        x.fid
                    );
                }
            }
        }
    }

    /// A protobuf message being encoded, for hand-built PBF files.
    #[derive(Default)]
    struct Msg(Vec<u8>);

    impl Msg {
        fn varint(&mut self, mut v: u64) {
            while v >= 0x80 {
                self.0.push(v as u8 | 0x80);
                v >>= 7;
            }
            self.0.push(v as u8);
        }
        fn int(mut self, field: u64, v: u64) -> Self {
            self.varint(field << 3);
            self.varint(v);
            self
        }
        fn bytes(mut self, field: u64, b: &[u8]) -> Self {
            self.varint((field << 3) | 2);
            self.varint(b.len() as u64);
            self.0.extend_from_slice(b);
            self
        }
        fn packed(self, field: u64, vs: &[u64]) -> Self {
            let mut p = Msg::default();
            vs.iter().for_each(|&v| p.varint(v));
            self.bytes(field, &p.0)
        }
    }

    fn zz(v: i64) -> u64 {
        ((v << 1) ^ (v >> 63)) as u64
    }

    /// A PBF file of blocks, each a list of groups (nodes, ways or relations), sorted by type then
    /// id as its header says.
    fn pbf(blocks: &[Vec<Msg>]) -> Vec<u8> {
        let strings = ["", "building", "yes", "highway", "residential", "type", "multipolygon", "name", "lake", "outer"];
        let table = strings.iter().fold(Msg::default(), |m, s| m.bytes(1, s.as_bytes()));
        let mut out = Vec::new();
        let mut blob = |kind: &str, data: Vec<u8>| {
            let blob = Msg::default().bytes(1, &data).int(2, data.len() as u64).0;
            let header = Msg::default().bytes(1, kind.as_bytes()).int(3, blob.len() as u64).0;
            out.extend_from_slice(&(header.len() as u32).to_be_bytes());
            out.extend_from_slice(&header);
            out.extend_from_slice(&blob);
        };
        let header = Msg::default().bytes(4, b"OsmSchema-V0.6").bytes(4, b"DenseNodes").bytes(5, b"Sort.Type_then_ID");
        blob("OSMHeader", header.0);
        for groups in blocks {
            let block = groups.iter().fold(Msg::default().bytes(1, &table.0), |m, g| m.bytes(2, &g.0));
            blob("OSMData", block.0);
        }
        out
    }

    /// Nodes 1 to 5 (a square and its centre), a closed way tagged as a building, a residential
    /// street, and a multipolygon relation using the building as its outer ring.
    fn objects() -> (Msg, Msg, Msg) {
        let node = |id: i64, lat: i64, lon: i64| Msg::default().int(1, zz(id)).int(8, zz(lat)).int(9, zz(lon)).0;
        let pts = [(1, 0, 0), (2, 0, 10_000), (3, 10_000, 10_000), (4, 10_000, 0), (5, 5_000, 5_000)];
        let nodes = pts.iter().fold(Msg::default(), |m, &(id, lat, lon)| m.bytes(1, &node(id, lat, lon)));
        let way = |id: u64, refs: &[i64], k: u64, v: u64| {
            let mut prev = 0;
            let deltas: Vec<u64> = refs.iter().map(|&r| zz(r - std::mem::replace(&mut prev, r))).collect();
            Msg::default().int(1, id).packed(2, &[k]).packed(3, &[v]).packed(8, &deltas).0
        };
        let ways = Msg::default().bytes(3, &way(10, &[1, 2, 3, 4, 1], 1, 2)).bytes(3, &way(11, &[1, 5, 3], 3, 4));
        let rel = Msg::default().int(1, 20).packed(2, &[5, 7]).packed(3, &[6, 8]).packed(8, &[9]).packed(9, &[zz(10)]).packed(10, &[1]);
        let relations = Msg::default().bytes(4, &rel.0);
        (nodes, ways, relations)
    }

    /// Sorted files whose relations share a block with nodes or ways read as files of one type a
    /// block, and as the reader holding everything reads them.
    #[test]
    fn reads_blocks_of_mixed_types() {
        let separate = {
            let (n, w, r) = objects();
            pbf(&[vec![n], vec![w], vec![r]])
        };
        let expected = read_sorted_layers(&mut std::io::Cursor::new(&separate)).unwrap().unwrap();
        assert_eq!(expected.iter().map(Layer::len).collect::<Vec<_>>(), [0, 1, 0, 1, 0]);
        let mixed: Vec<Vec<u8>> = vec![
            {
                let (n, w, r) = objects();
                pbf(&[vec![n, w, r]])
            },
            {
                let (n, w, r) = objects();
                pbf(&[vec![n], vec![w, r]])
            },
            {
                let (n, w, r) = objects();
                pbf(&[vec![n, w], vec![r]])
            },
        ];
        for (k, bytes) in mixed.iter().enumerate() {
            let sorted = read_sorted_layers(&mut std::io::Cursor::new(bytes)).unwrap().unwrap();
            let any = parse_any_order(bytes.clone()).unwrap();
            let content = |l: &Layer| l.features.iter().map(|f| (f.fid, f.attributes.clone(), f.geometry.clone())).collect::<Vec<_>>();
            for ((a, b), c) in sorted.iter().zip(&any).zip(&expected) {
                assert_eq!(content(a), content(b), "file {k}, {}", a.name);
                assert_eq!(content(a), content(c), "file {k}, {}", a.name);
            }
        }
    }

    /// A file whose header says it is sorted but whose objects are not is read whole, as before;
    /// the streaming reader reports it, whatever the layers wanted.
    #[test]
    fn reads_files_not_sorted_as_they_say_whole() {
        let (n, w, r) = objects();
        let ways_first = pbf(&[vec![w], vec![n], vec![r]]);
        let expected = {
            let (n, w, r) = objects();
            parse_any_order(pbf(&[vec![n], vec![w], vec![r]])).unwrap()
        };
        assert!(read_sorted_layers(&mut std::io::Cursor::new(&ways_first)).unwrap().is_none());
        let read = parse(ways_first).unwrap();
        let content = |l: &Layer| l.features.iter().map(|f| (f.fid, f.attributes.clone(), f.geometry.clone())).collect::<Vec<_>>();
        for (a, b) in read.iter().zip(&expected) {
            assert_eq!(content(a), content(b), "{}", a.name);
        }
        // Relations between the nodes and the ways, with only the lines wanted.
        let (n, w, r) = objects();
        let relations_first = pbf(&[vec![n], vec![r], vec![w]]);
        let only_lines = [false, true, false, false, false];
        let e = read_sorted_pbf(&mut std::io::Cursor::new(&relations_first), only_lines, &mut |_, _| Ok(())).unwrap_err();
        assert!(is_unsorted(&e));
        assert!(is_sorted_pbf(&mut std::io::Cursor::new(&relations_first)));
        assert!(!is_sorted_pbf(&mut std::io::Cursor::new(b"<?xml version='1.0'?><osm/>".as_slice())));
    }

    /// A layer read alone (its relations built, the others skipped) has the features it has when
    /// every layer is read.
    #[test]
    fn layers_read_alone_match_all_layers() {
        let bytes = {
            let (n, w, r) = objects();
            pbf(&[vec![n], vec![w], vec![r]])
        };
        let mut all: Vec<Vec<Feature>> = vec![Vec::new(); 5];
        read_sorted_pbf(&mut std::io::Cursor::new(&bytes), [true; 5], &mut |l, f| {
            all[l].extend(f);
            Ok(())
        })
        .unwrap();
        for l in 0..5 {
            let mut wanted = [false; 5];
            wanted[l] = true;
            let mut alone: Vec<Vec<Feature>> = vec![Vec::new(); 5];
            read_sorted_pbf(&mut std::io::Cursor::new(&bytes), wanted, &mut |k, f| {
                alone[k].extend(f);
                Ok(())
            })
            .unwrap();
            let content = |fs: &[Feature]| fs.iter().map(|f| (f.fid, f.attributes.clone(), f.geometry.clone())).collect::<Vec<_>>();
            for k in 0..5 {
                let expected = if k == l { content(&all[k]) } else { Vec::new() };
                assert_eq!(content(&alone[k]), expected, "layer {k} with {l} wanted");
            }
        }
        assert_eq!(all[3].len(), 1);
    }

    /// Relations are built in chunks of at most 8192, and of at most about four million member
    /// coordinates unless one relation has more; relations of layers not wanted count for none.
    #[test]
    fn chunks_relations_by_member_coordinates() {
        let rel = |id: i64, rtype: &str, way: i64| Relation {
            id,
            members: vec![(Member::Way(way), "outer".into())],
            tags: vec![("type".into(), rtype.into())],
        };
        let member_ways: HashMap<i64, MemberAt> = HashMap::from([(1, (0, 3 << 20)), (2, (0, 5 << 20)), (3, (0, 4))]);
        let relations = vec![
            rel(1, "multipolygon", 1),
            rel(2, "multipolygon", 1),
            rel(3, "multipolygon", 2),
            rel(4, "route", 2),
            rel(5, "multipolygon", 3),
        ];
        let sizes = |wanted: [bool; 5]| relation_chunks(&relations, &wanted, &member_ways).map(<[Relation]>::len).collect::<Vec<_>>();
        // 3 M + 3 M is over the limit; 5 M alone is taken whole.
        assert_eq!(sizes([false, false, false, true, false]), [1, 1, 1, 2]);
        assert_eq!(sizes([false, false, true, true, false]), [1, 1, 1, 1, 1]);
        assert_eq!(sizes([true, true, false, false, true]), [5]);
        let many: Vec<Relation> = (0..10_000).map(|i| rel(i, "multipolygon", 3)).collect();
        let n: Vec<usize> = relation_chunks(&many, &[true; 5], &member_ways).map(<[Relation]>::len).collect();
        assert_eq!(n, [8192, 1808]);
    }

    /// A spill read to its end gives `None`; one cut short in a record, an error.
    #[test]
    fn spilled_areas_cut_short_are_errors() {
        let area = encode_area(7, &[Coord::xy(1.0, 2.0), Coord::xy(3.0, 4.0)], &[("building", "yes")]);
        let mut whole = std::io::Cursor::new(area.clone());
        assert_eq!(read_area(&mut whole).unwrap().map(|a| a.0), Some(7));
        assert!(read_area(&mut whole).unwrap().is_none());
        for cut in [3, 12, area.len() - 1] {
            assert!(read_area(&mut std::io::Cursor::new(&area[..cut])).is_err(), "cut at {cut}");
        }
    }

    #[test]
    fn stores_nodes_compactly() {
        let mut store = NodeStore::default();
        // Ids with gaps of every size, coordinates jumping across the globe.
        let nodes: Vec<(i64, i32, i32)> = (0..1000i64)
            .map(|k| {
                let id = 5 + k * k * 1_000_003 + (k % 3);
                let lon = if k % 2 == 0 { -1_799_999_999 } else { 1_800_000_000 - k as i32 };
                (id, lon, (k as i32 - 500) * 1_799_999)
            })
            .collect();
        for &(id, lon, lat) in &nodes {
            assert!(store.push(id, lon, lat).unwrap());
        }
        assert!(!store.push(nodes[999].0, 0, 0).unwrap(), "ids must increase");
        for &(id, lon, lat) in &nodes {
            assert_eq!(store.get(id), Some(node_coords(lon, lat).1));
            assert_eq!(store.get(id + 1), None);
        }
        assert_eq!(store.get(4), None);
        assert!(store.groups.len() == 1000usize.div_ceil(NODE_GROUP));
    }

    #[test]
    fn stores_nodes_past_a_page_in_a_file() {
        let mut store = NodeStore::default();
        // Coordinates far apart take about 9 bytes a node: these fill more than a page.
        let n = 2 * SPOOL_PAGE as i64 / 11;
        let node = |k: i64| (2 * k + 1, if k % 2 == 0 { -1_799_999_999 } else { 1_799_999_999 }, (k % 1000) as i32 * 900_000);
        for k in 0..n {
            let (id, lon, lat) = node(k);
            assert!(store.push(id, lon, lat).unwrap());
        }
        assert!(store.bytes.len() > SPOOL_PAGE as u64);
        store.bytes.seal().unwrap();
        #[cfg(not(target_arch = "wasm32"))]
        assert!(store.bytes.map.is_some());
        for k in [0, 1, n / 2, n - 1].into_iter().chain((0..n).step_by(9973)) {
            let (id, lon, lat) = node(k);
            assert_eq!(store.get_raw(id), Some([lon, lat]));
            assert_eq!(store.get_raw(id + 1), None);
        }
        store.bytes.release();
        let (id, lon, lat) = node(n - 1);
        assert_eq!(store.get_raw(id), Some([lon, lat]), "released pages read back");
    }

    #[test]
    fn spooled_member_ways_read_back() {
        let raw = [[-1_799_999_999, 899_999_999], [1_800_000_000, -900_000_000], [0, 0]];
        let tags = [("name", "Lake été"), ("natural", "water")];
        let mut spool = Spool::default();
        spool.buf.extend_from_slice(&encode_member(&[], false, &[]));
        let at = spool.len() as usize;
        spool.buf.extend_from_slice(&encode_member(&raw, true, &tags));
        let w = decode_member(spool.bytes(), at).unwrap();
        assert_eq!(w.coords, raw.map(|[lon, lat]| node_coords(lon, lat).1));
        assert!(w.area);
        assert_eq!(w.tags, tags);
        let empty = decode_member(spool.bytes(), 0).unwrap();
        assert!(empty.coords.is_empty() && !empty.area && empty.tags.is_empty());
        assert!(decode_member(&spool.bytes()[..spool.bytes().len() - 1], at).is_none(), "cut short");
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

//! OpenStreetMap PBF, read into the layers of GDAL's OSM driver with its default `osmconf.ini`.
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

/// Reads an OSM PBF file into GDAL's five layers.
pub fn read(path: &str) -> Result<Vec<Layer>> {
    let mut reader = OsmPbfReader::new(std::fs::File::open(path)?);
    let mut nodes: HashMap<i64, Coord> = HashMap::new();
    let mut ways: HashMap<i64, Way> = HashMap::new();
    let mut way_order: Vec<i64> = Vec::new();
    let mut relations = Vec::new();
    let mut layers: Vec<Layer> = SPECS.iter().map(Spec::layer).collect();
    for obj in reader.iter() {
        let obj = obj.map_err(|e| Error::InvalidData(format!("OSM PBF: {e}")))?;
        match obj {
            OsmObj::Node(n) => {
                let c = Coord::xy(n.decimicro_lon as f64 * 1e-7, n.decimicro_lat as f64 * 1e-7);
                if n.tags.keys().any(|k| !POINT_INSIGNIFICANT.contains(&k.as_str())) {
                    let attrs = SPECS[0].attributes(&layers[0], n.id.0, false, &tag_list(&n.tags));
                    layers[0].push(Feature { fid: n.id.0 as u64, geometry: Some(Geometry::Point(c.clone())), attributes: attrs });
                }
                nodes.insert(n.id.0, c);
            }
            OsmObj::Way(w) => {
                let tags = tag_list(&w.tags);
                let closed = w.nodes.len() > 1 && w.nodes.first() == w.nodes.last();
                let area = closed && is_area(&tags);
                way_order.push(w.id.0);
                let tags = tags.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
                ways.insert(w.id.0, Way { nodes: w.nodes.iter().map(|n| n.0).collect(), tags, area });
            }
            OsmObj::Relation(r) => relations.push(r),
        }
    }

    // The coordinates of a way, as GDAL keeps them: nodes it cannot find are skipped, and an area's
    // ring is closed.
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
            standalone.push(*id);
        } else if !w.tags.is_empty() {
            let tags: Vec<(&str, &str)> = w.tags.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            let attrs = SPECS[1].attributes(&layers[1], *id, false, &tags);
            layers[1].push(Feature { fid: *id as u64, geometry: Some(Geometry::LineString(cs)), attributes: attrs });
        }
    }

    let mut used_as_outer: std::collections::HashSet<i64> = Default::default();
    for r in &relations {
        let tags = tag_list(&r.tags);
        let rtype = tags.iter().find(|(k, _)| *k == "type").map(|(_, v)| *v);
        let interesting = tags.iter().any(|(k, _)| *k != "type" && *k != "created_by");
        let members: Vec<(&osmpbfreader::Ref, Option<&Way>)> = r
            .refs
            .iter()
            .filter(|m| m.role.as_str() != "subarea")
            .map(|m| (m, if let OsmId::Way(w) = m.member { ways.get(&w.0) } else { None }))
            .collect();
        match rtype {
            Some("multipolygon" | "boundary") => {
                let way_members: Vec<_> = members.iter().filter(|(m, _)| m.member.is_way()).collect();
                if way_members.iter().any(|(_, w)| w.is_none()) {
                    continue; // missing ways
                }
                let mut rings = Vec::new();
                let mut edges = Vec::new();
                let mut first_outer_tags = None;
                for (m, w) in &way_members {
                    let w = w.unwrap_or_else(|| unreachable!());
                    let cs = coords(w);
                    if first_outer_tags.is_none() && m.role.as_str() == "outer" {
                        first_outer_tags = Some(&w.tags);
                    }
                    let closed = !cs.is_empty() && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y));
                    if closed {
                        if m.role.as_str() == "outer"
                            && let OsmId::Way(id) = m.member
                        {
                            used_as_outer.insert(id.0);
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
                let attrs = SPECS[3].attributes(&layers[3], r.id.0, false, tags);
                layers[3].push(Feature { fid: r.id.0 as u64, geometry: Some(g), attributes: attrs });
            }
            Some("multilinestring" | "route") => {
                let lines: Vec<Vec<Coord>> = members.iter().filter_map(|(_, w)| w.map(coords)).collect();
                if lines.is_empty() {
                    continue;
                }
                let attrs = SPECS[2].attributes(&layers[2], r.id.0, false, &tags);
                layers[2].push(Feature { fid: r.id.0 as u64, geometry: Some(Geometry::MultiLineString(lines)), attributes: attrs });
            }
            _ => {
                let mut parts = Vec::new();
                for (m, w) in &members {
                    match (m.member, w) {
                        (OsmId::Node(n), _) => parts.extend(nodes.get(&n.0).cloned().map(Geometry::Point)),
                        (OsmId::Way(_), Some(w)) => {
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
                let attrs = SPECS[4].attributes(&layers[4], r.id.0, false, &tags);
                layers[4].push(Feature { fid: r.id.0 as u64, geometry: Some(Geometry::GeometryCollection(parts)), attributes: attrs });
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
    Ok(layers)
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

// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! CityGML 2.0 and 3.0, following the OGC encoding specifications.
//!
//! Exposes ordinary XYZ faces, lines and points rather than a second city model.
//! GDAL 3.13.3 provides only a partial CityGML reader; the specifications and
//! published schemas are the reference for this implementation.

use super::city::{CITY_ID, GEOMETRY_TYPE, LOD, PARENTS, SURFACE_SEMANTICS};
use crate::crs::SpatialRef;
use crate::{Error, Result};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::{NsReader, XmlVersion};
use serde_json::{Map, Value, json};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use wbvector::{Coord, FieldValue, Geometry, Layer, Ring};

const XLINK: &str = "http://www.w3.org/1999/xlink";
const GML: &str = "http://www.opengis.net/gml";
const GML32: &str = "http://www.opengis.net/gml/3.2";

pub(crate) const MAX_INPUT_BYTES: usize = 256 * 1024 * 1024;
const MAX_NESTING: usize = 256;
const MAX_REFERENCE_VISITS: usize = 1_000_000;
const MAX_REFERENCE_COORDINATE_BYTES: usize = 64 * 1024 * 1024;

fn utf16_encoding(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    if bytes.starts_with(b"\xff\xfe") || bytes.starts_with(b"<\0") {
        Some(encoding_rs::UTF_16LE)
    } else if bytes.starts_with(b"\xfe\xff") || bytes.starts_with(b"\0<") {
        Some(encoding_rs::UTF_16BE)
    } else {
        None
    }
}

fn decode_xml(bytes: &[u8]) -> Result<Cow<'_, str>> {
    if let Some(encoding) = utf16_encoding(bytes) {
        let (text, errors) = encoding.decode_with_bom_removal(bytes);
        if errors {
            return Err(invalid("malformed UTF-16 XML"));
        }
        Ok(text)
    } else {
        let bytes = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes);
        if let Ok(Event::Decl(declaration)) = quick_xml::Reader::from_reader(bytes).read_event()
            && let Some(label) = declaration.encoding()
        {
            let label = label.map_err(invalid)?;
            if matches!(label.to_ascii_lowercase().as_str(), "iso-8859-1" | "iso_8859-1" | "latin1") {
                return Ok(Cow::Owned(bytes.iter().map(|&byte| byte as char).collect()));
            }
            let encoding = encoding_rs::Encoding::for_label(label.as_bytes())
                .ok_or_else(|| Error::Unsupported(format!("CityGML XML encoding {label}")))?;
            if encoding == encoding_rs::UTF_16LE || encoding == encoding_rs::UTF_16BE {
                return Err(invalid("UTF-16 declaration requires a matching byte signature"));
            }
            let (text, errors) = encoding.decode_without_bom_handling(bytes);
            if errors {
                return Err(invalid(format!("malformed {label} XML")));
            }
            return Ok(text);
        }
        Ok(match std::str::from_utf8(bytes) {
            Ok(text) => Cow::Borrowed(text),
            Err(_) => Cow::Owned(bytes.iter().map(|&byte| byte as char).collect()),
        })
    }
}

fn invalid(message: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("CityGML: {message}"))
}

#[derive(Default, Debug)]
struct Node {
    name: String,
    ns: String,
    attrs: Vec<(String, String, String)>,
    children: Vec<Node>,
    text: String,
}

impl Node {
    fn attr(&self, ns: &str, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, k, _)| n == ns && k == name).map(|(_, _, v)| v.as_str())
    }
    fn id(&self) -> Option<&str> {
        self.attr(GML, "id").or_else(|| self.attr(GML32, "id"))
    }
    fn gml(&self) -> bool {
        matches!(self.ns.as_str(), GML | GML32)
    }
    fn city(&self) -> bool {
        self.ns.starts_with("http://www.opengis.net/citygml/") && (self.ns.ends_with("/2.0") || self.ns.ends_with("/3.0"))
    }
    fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|n| n.name == name)
    }
}

fn namespace(value: ResolveResult<'_>) -> Result<String> {
    match value {
        ResolveResult::Bound(ns) => Ok(ns.as_ref().to_string()),
        ResolveResult::Unbound => Ok(String::new()),
        ResolveResult::Unknown(prefix) => Err(invalid(format!("unbound namespace prefix {}", prefix))),
    }
}

fn tree_charge(used: &mut usize, cost: usize, limit: usize) -> Result<()> {
    *used = used.saturating_add(cost.saturating_mul(2));
    if *used > limit {
        return Err(invalid("estimated XML tree payload exceeds its allocation limit"));
    }
    Ok(())
}
fn tree_text(node: &mut Node, text: &str, used: &mut usize, limit: usize) -> Result<()> {
    tree_charge(used, text.len(), limit)?;
    node.text.try_reserve(text.len()).map_err(invalid)?;
    node.text.push_str(text);
    Ok(())
}
fn xml(bytes: &[u8]) -> Result<Node> {
    xml_with_limit(bytes, super::city::MAX_FEATURE_BYTES)
}
fn xml_with_limit(bytes: &[u8], tree_limit: usize) -> Result<Node> {
    let mut tree_bytes = 0usize;
    let decoded = decode_xml(bytes)?;
    let mut reader = NsReader::from_str(&decoded);
    let mut stack = vec![Node::default()];
    let mut count = 0usize;
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(invalid)?;
        let ns = namespace(ns)?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                count += 1;
                if count > 5_000_000 || stack.len() > MAX_NESTING {
                    return Err(invalid("XML element or nesting limit exceeded"));
                }
                tree_charge(&mut tree_bytes, std::mem::size_of::<Node>() + e.local_name().as_ref().len() + ns.len(), tree_limit)?;
                let mut node = Node { name: e.local_name().as_ref().to_string(), ns, ..Default::default() };
                for attr in e.attributes() {
                    let attr = attr.map_err(invalid)?;
                    if attr.key.as_ref() == "xmlns" || attr.key.as_ref().starts_with("xmlns:") {
                        continue;
                    }
                    let (ns, name) = reader.resolver().resolve_attribute(attr.key);
                    let ns = namespace(ns)?;
                    let value = attr.normalized_value(XmlVersion::Implicit1_0).map_err(invalid)?;
                    tree_charge(
                        &mut tree_bytes,
                        std::mem::size_of::<(String, String, String)>() + ns.len() + name.as_ref().len() + value.len(),
                        tree_limit,
                    )?;
                    node.attrs.try_reserve(1).map_err(invalid)?;
                    node.attrs.push((ns, name.as_ref().to_string(), value.into_owned()));
                }
                if matches!(event, Event::Empty(_)) {
                    stack.last_mut().unwrap().children.try_reserve(1).map_err(invalid)?;
                    stack.last_mut().unwrap().children.push(node);
                } else {
                    stack.try_reserve(1).map_err(invalid)?;
                    stack.push(node);
                }
            }
            Event::End(_) => {
                if stack.len() <= 1 {
                    return Err(invalid("unmatched XML end tag"));
                }
                let node = stack.pop().unwrap();
                stack.last_mut().unwrap().children.try_reserve(1).map_err(invalid)?;
                stack.last_mut().unwrap().children.push(node);
            }
            Event::Text(t) => tree_text(stack.last_mut().unwrap(), &t.xml10_content(), &mut tree_bytes, tree_limit)?,
            Event::CData(t) => tree_text(stack.last_mut().unwrap(), &t.xml10_content(), &mut tree_bytes, tree_limit)?,
            Event::GeneralRef(r) => {
                let text = format!("&{};", &*r);
                let value = quick_xml::escape::unescape(&text).map_err(invalid)?;
                tree_text(stack.last_mut().unwrap(), &value, &mut tree_bytes, tree_limit)?;
            }
            Event::DocType(_) => return Err(Error::Unsupported("CityGML: DTD/entity declarations are not supported".into())),
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return Err(invalid("truncated XML document"));
    }
    let mut root = stack.pop().unwrap();
    if root.children.len() != 1 || !root.text.trim().is_empty() {
        return Err(invalid("expected one XML root"));
    }
    Ok(root.children.remove(0))
}

fn numbers(text: &str) -> Result<Vec<f64>> {
    if text.split_whitespace().count() != 16 {
        return Err(invalid("implicit matrix must have 16 values"));
    }
    text.split_whitespace()
        .map(|s| s.parse::<f64>().ok().filter(|v| v.is_finite()).ok_or_else(|| invalid("nonfinite or invalid coordinate")))
        .collect()
}

fn dimension(node: &Node, inherited: usize) -> Result<usize> {
    let Some(value) = node.attr("", "srsDimension") else {
        return Ok(inherited);
    };
    let dim = value.parse::<usize>().map_err(invalid)?;
    if !(2..=3).contains(&dim) {
        return Err(Error::Unsupported(format!("CityGML coordinate dimension {dim}")));
    }
    Ok(dim)
}

fn preflight_coordinates(node: &Node, inherited: usize, remaining: usize) -> Result<usize> {
    let dim = dimension(node, inherited)?;
    let count = if node.gml() && matches!(node.name.as_str(), "pos" | "posList") {
        node.text.split_whitespace().count().div_ceil(dim)
    } else if node.gml() && node.name == "coordinates" {
        let tuples = node.attr("", "ts").unwrap_or(" ");
        if tuples.trim().is_empty() {
            node.text.split_whitespace().count()
        } else {
            node.text.split(tuples).filter(|s| !s.trim().is_empty()).count()
        }
    } else {
        let mut total = 0usize;
        for child in &node.children {
            let cost = preflight_coordinates(child, dim, remaining.saturating_sub(total))?;
            total = total.saturating_add(cost);
        }
        return Ok(total);
    };
    let bytes = count.saturating_mul(std::mem::size_of::<Coord>());
    if bytes > remaining {
        return Err(invalid("coordinate expansion exceeds the remaining flat feature payload limit"));
    }
    Ok(bytes)
}

fn coordinates(node: &Node, inherited: usize) -> Result<Vec<Coord>> {
    if node.gml()
        && (matches!(node.name.as_str(), "Arc" | "ArcString" | "Circle" | "BSpline" | "Bezier")
            || node.name == "segments" && node.children.iter().any(|child| !child.gml() || child.name != "LineStringSegment"))
    {
        return Err(Error::Unsupported("CityGML: nonlinear curve segments are not supported".into()));
    }
    let dim = dimension(node, inherited)?;
    if node.gml() && matches!(node.name.as_str(), "pos" | "posList") {
        let count = node.text.split_whitespace().count();
        if !count.is_multiple_of(dim) || (node.name == "pos" && count != dim) {
            return Err(invalid("coordinate count does not match srsDimension"));
        }
        if let Some(declared) = node.attr("", "count")
            && declared.parse::<usize>().map_err(invalid)? != count / dim
        {
            return Err(invalid("posList count does not match coordinates"));
        }
        let mut out = Vec::new();
        out.try_reserve_exact(count / dim).map_err(invalid)?;
        let mut values = node.text.split_whitespace();
        for _ in 0..count / dim {
            let mut p = [0.0; 3];
            for value in &mut p[..dim] {
                *value = values.next().unwrap().parse::<f64>().map_err(invalid)?;
                if !value.is_finite() {
                    return Err(invalid("nonfinite or invalid coordinate"));
                }
            }
            out.push(Coord { x: p[0], y: p[1], z: (dim == 3).then_some(p[2]), m: None });
        }
        return Ok(out);
    }
    if node.gml() && node.name == "coordinates" {
        let separator = node.attr("", "cs").unwrap_or(",");
        let decimal = node.attr("", "decimal").unwrap_or(".");
        let tuples = node.attr("", "ts").unwrap_or(" ");
        let tuples: Box<dyn Iterator<Item = &str> + '_> =
            if tuples.trim().is_empty() { Box::new(node.text.split_whitespace()) } else { Box::new(node.text.split(tuples)) };
        let mut out = Vec::new();
        for tuple in tuples.filter(|p| !p.trim().is_empty()) {
            let mut values = tuple.split(separator);
            let mut p = [0.0; 3];
            for value in &mut p[..dim] {
                let text = values.next().ok_or_else(|| invalid("invalid coordinate tuple"))?;
                *value = text.trim().replace(decimal, ".").parse::<f64>().map_err(invalid)?;
                if !value.is_finite() {
                    return Err(invalid("invalid coordinate tuple"));
                }
            }
            if values.next().is_some() {
                return Err(invalid("invalid coordinate tuple"));
            }
            out.try_reserve(1).map_err(invalid)?;
            out.push(Coord { x: p[0], y: p[1], z: (dim == 3).then_some(p[2]), m: None });
        }
        return Ok(out);
    }
    if node.attr(XLINK, "href").is_some() {
        return Err(Error::Unsupported("CityGML: linked coordinate/ring content is not supported".into()));
    }
    let mut out = Vec::new();
    for child in &node.children {
        let points = coordinates(child, dim)?;
        out.try_reserve(points.len()).map_err(invalid)?;
        out.extend(points);
    }
    Ok(out)
}

fn ring(node: &Node, dim: usize) -> Result<Ring> {
    let mut points = coordinates(node, dim)?;
    points.dedup();
    if points.len() > 1 && points.first() == points.last() {
        points.pop();
    }
    let mut distinct = Vec::with_capacity(3);
    for point in &points {
        if !distinct.contains(&point) {
            distinct.push(point);
        }
        if distinct.len() == 3 {
            break;
        }
    }
    if distinct.len() < 3 {
        return Err(invalid("ring has fewer than three distinct positions"));
    }
    Ok(Ring::new(points))
}

fn primitive(node: &Node, dim: usize) -> Result<Geometry> {
    if node.name == "Point" {
        let positions = coordinates(node, dim)?;
        if positions.len() != 1 {
            return Err(invalid("Point must have one position"));
        }
        return Ok(Geometry::Point(positions[0].clone()));
    }
    if matches!(node.name.as_str(), "LineString" | "LineStringSegment") {
        let points = coordinates(node, dim)?;
        if points.len() < 2 {
            return Err(invalid("line must have at least two positions"));
        }
        return Ok(Geometry::LineString(points));
    }
    let exterior = node.child("exterior").or_else(|| node.child("outerBoundaryIs")).ok_or_else(|| invalid("polygon has no exterior"))?;
    let interiors = node
        .children
        .iter()
        .filter(|n| matches!(n.name.as_str(), "interior" | "innerBoundaryIs"))
        .map(|n| ring(n, dim))
        .collect::<Result<_>>()?;
    Ok(Geometry::Polygon { exterior: ring(exterior, dim)?, interiors })
}

fn insert_property(values: &mut Map<String, Value>, name: &str, value: Value) {
    match values.get_mut(name) {
        Some(Value::Array(items)) => items.push(value),
        Some(previous) => *previous = Value::Array(vec![previous.clone(), value]),
        None => {
            values.insert(name.into(), value);
        }
    }
}

fn scalar_property(node: &Node) -> Value {
    let mut values = Map::new();
    for child in &node.children {
        insert_property(&mut values, &child.name, scalar_property(child));
    }
    if values.is_empty() {
        if node.attr("", "uom").is_some() || node.attr("", "codeSpace").is_some() {
            values.insert("value".into(), Value::String(node.text.trim().into()));
            for key in ["uom", "codeSpace"] {
                if let Some(text) = node.attr("", key) {
                    values.insert(key.into(), Value::String(text.into()));
                }
            }
            Value::Object(values)
        } else {
            Value::String(node.text.trim().into())
        }
    } else {
        Value::Object(values)
    }
}

fn has_geometry(node: &Node) -> bool {
    node.gml()
        && matches!(
            node.name.as_str(),
            "Polygon"
                | "PolygonPatch"
                | "Triangle"
                | "Rectangle"
                | "Point"
                | "Curve"
                | "MultiSurface"
                | "Solid"
                | "LineString"
                | "LineStringSegment"
                | "Envelope"
        )
        || node.children.iter().any(has_geometry)
}

fn attributes(node: &Node) -> Value {
    let mut attrs = Map::new();
    for child in &node.children {
        if matches!(child.name.as_str(), "boundedBy" | "boundary" | "appearance" | "appearanceMember")
            || child.attr(XLINK, "href").is_some()
            || has_geometry(child)
            || child.children.iter().any(is_object)
        {
            continue;
        }
        if child.name.ends_with("Attribute") {
            if let (Some(name), Some(value)) = (child.attr("", "name"), child.child("value")) {
                let text = value.text.trim();
                let v = match child.name.as_str() {
                    "intAttribute" => text.parse::<i64>().map(|value| json!(value)).unwrap_or_else(|_| Value::String(text.into())),
                    "doubleAttribute" => text
                        .parse::<f64>()
                        .ok()
                        .and_then(serde_json::Number::from_f64)
                        .map_or_else(|| Value::String(text.into()), Value::Number),
                    "boolAttribute" if matches!(text, "true" | "1") => Value::Bool(true),
                    "boolAttribute" if matches!(text, "false" | "0") => Value::Bool(false),
                    _ => Value::String(text.into()),
                };
                insert_property(&mut attrs, name, v);
            }
        } else {
            insert_property(&mut attrs, &child.name, scalar_property(child));
        }
    }
    Value::Object(attrs)
}

fn is_object(node: &Node) -> bool {
    node.city()
        && matches!(
            node.name.as_str(),
            "Building"
                | "BuildingPart"
                | "Room"
                | "BuildingRoom"
                | "BuildingInstallation"
                | "BuildingConstructiveElement"
                | "IntBuildingInstallation"
                | "BuildingFurniture"
                | "BuildingUnit"
                | "Storey"
                | "Door"
                | "Window"
                | "Bridge"
                | "BridgePart"
                | "BridgeRoom"
                | "BridgeInstallation"
                | "BridgeConstructiveElement"
                | "BridgeConstructionElement"
                | "BridgeFurniture"
                | "Tunnel"
                | "TunnelPart"
                | "TunnelRoom"
                | "HollowSpace"
                | "TunnelInstallation"
                | "TunnelConstructiveElement"
                | "TunnelFurniture"
                | "CityFurniture"
                | "CityObjectGroup"
                | "GenericCityObject"
                | "GenericOccupiedSpace"
                | "GenericUnoccupiedSpace"
                | "LandUse"
                | "PlantCover"
                | "SolitaryVegetationObject"
                | "WaterBody"
                | "ReliefFeature"
                | "TINRelief"
                | "MassPointRelief"
                | "BreaklineRelief"
                | "Road"
                | "Railway"
                | "Track"
                | "Square"
                | "TransportationComplex"
                | "TrafficSpace"
                | "AuxiliaryTrafficSpace"
                | "TrafficArea"
                | "AuxiliaryTrafficArea"
                | "OtherConstruction"
        )
}

#[derive(Clone, Default)]
struct Context<'a> {
    dim: usize,
    crs: Option<&'a str>,
    lod: Option<&'a str>,
    semantic: Option<Rc<str>>,
    geometry_type: Option<&'a str>,
    reverse: bool,
    matrix: Option<([f64; 16], [f64; 3])>,
    expanded: bool,
    validate_only: bool,
}

#[derive(Clone, Copy)]
struct Reference {
    dimension: Option<usize>,
    swap: bool,
}

fn axis_count(value: &Value) -> Option<usize> {
    if let Some(axes) = value.get("coordinate_system").and_then(|v| v.get("axis")).and_then(Value::as_array) {
        return Some(axes.len());
    }
    if let Some(components) = value.get("components").and_then(Value::as_array) {
        return components.iter().try_fold(0usize, |sum, value| sum.checked_add(axis_count(value)?));
    }
    value.get("source_crs").and_then(axis_count)
}

struct ObjectRows<'a> {
    contexts: HashSet<(usize, Option<&'a str>)>,
    rows: Vec<usize>,
    parents: Vec<String>,
    seen: HashSet<String>,
}

fn coordinate_text_bytes(node: &Node) -> usize {
    if node.gml() && matches!(node.name.as_str(), "pos" | "posList" | "coordinates") {
        node.text.len()
    } else {
        node.children.iter().fold(0usize, |sum, child| sum.saturating_add(coordinate_text_bytes(child)))
    }
}

struct Reader<'a> {
    ids: HashMap<&'a str, &'a Node>,
    layer: Layer,
    crs: Option<SpatialRef>,
    references: HashMap<&'a str, Reference>,
    used: usize,
    next_object: usize,
    unknown_crs: bool,
    active: HashSet<&'a str>,
    active_links: HashSet<&'a str>,
    objects: HashMap<&'a str, ObjectRows<'a>>,
    validated: HashSet<(&'a str, usize, Option<&'a str>)>,
    face_semantics: HashMap<usize, Rc<str>>,
    surface_semantics: HashMap<*const Node, Rc<str>>,
    semantic_values: HashMap<Value, Rc<str>>,
    geometry_contexts: HashMap<*const Node, (usize, Option<&'a str>)>,
    coordinate_bytes: usize,
    max_coordinate_bytes: usize,
    visits: usize,
    max_reference_visits: usize,
    emitted: HashMap<(String, String, Option<&'a str>, bool), usize>,
}

fn authority_axis_order(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("epsg") && (lower.starts_with("urn:") || lower.contains("/def/crs/"))
}

fn parse_crs(name: &str) -> Result<SpatialRef> {
    Ok(if let Some(parts) = name.strip_prefix("urn:ogc:def:crs,") {
        let codes = parts
            .split(',')
            .map(|part| {
                if !part.starts_with("crs:EPSG:") {
                    return Err(invalid("unsupported compound CRS authority"));
                }
                let code = part.rsplit(':').next().unwrap_or_default();
                code.parse::<u32>().map_err(invalid)?;
                Ok(code)
            })
            .collect::<Result<Vec<_>>>()?;
        SpatialRef::parse(&format!("EPSG:{}", codes.join("+")))?
    } else {
        SpatialRef::parse(name)?
    })
}

impl<'a> Reader<'a> {
    fn link(&self, node: &'a Node) -> Result<&'a Node> {
        if let Some(href) = node.attr(XLINK, "href") {
            let id = href.strip_prefix('#').ok_or_else(|| Error::Unsupported(format!("CityGML: external XLink {href}")))?;
            self.ids.get(id).copied().ok_or_else(|| invalid(format!("unresolved XLink #{id}")))
        } else {
            Ok(node)
        }
    }

    fn reference(&mut self, name: &'a str) -> Result<Reference> {
        if self.unknown_crs {
            return Err(invalid("geometry without CRS mixed with georeferenced geometry"));
        }
        let name = name.trim();
        if let Some(reference) = self.references.get(name) {
            return Ok(*reference);
        }
        let crs = parse_crs(name)?;
        if self.crs.as_ref().is_some_and(|source| !source.same_as(&crs)) {
            return Err(Error::Unsupported("CityGML: mixed CRSs in one flat layer".into()));
        }
        let dimension = crs
            .to_projjson()
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .and_then(|value| axis_count(&value))
            .or_else(|| (crs.has_height() || crs.is_geocentric()).then_some(3));
        let reference = Reference { dimension, swap: crs.is_geographic() && authority_axis_order(name) };
        if self.crs.is_none() {
            crs.apply_to_layer(&mut self.layer);
            self.crs = Some(crs);
        }
        self.references.insert(name, reference);
        Ok(reference)
    }

    fn coordinate_settings(
        &mut self,
        node: &'a Node,
        dim: usize,
        crs: Option<&'a str>,
        common: &mut Option<(usize, Option<&'a str>)>,
    ) -> Result<()> {
        let dim = dimension(node, dim)?;
        let crs = node.attr("", "srsName").or(crs);
        if node.gml() && matches!(node.name.as_str(), "pos" | "posList" | "coordinates") {
            let dim = if node.name == "coordinates" && dim == 0 && crs.is_none() {
                let separator = node.attr("", "cs").unwrap_or(",");
                let tuples = node.attr("", "ts").unwrap_or(" ");
                let first = if tuples.trim().is_empty() {
                    node.text.split_whitespace().next()
                } else {
                    node.text.split(tuples).find(|tuple| !tuple.trim().is_empty())
                };
                self.coordinate_dimension(first.map_or(0, |tuple| tuple.split(separator).count()), None)?
            } else {
                self.coordinate_dimension(dim, crs)?
            };
            if let Some((previous_dim, previous_crs)) = *common {
                if dim != previous_dim {
                    return Err(invalid("mixed coordinate dimensions in one geometry"));
                }
                if previous_crs != crs {
                    match (previous_crs, crs) {
                        (Some(a), Some(b)) => {
                            // `reference` validates CRS equivalence against the layer once
                            // for each spelling. A single primitive also needs one axis order.
                            if self.reference(a)?.swap != self.reference(b)?.swap {
                                return Err(invalid("mixed coordinate axis orders in one geometry"));
                            }
                        }
                        _ => return Err(invalid("geometry without CRS mixed with georeferenced geometry")),
                    }
                }
            } else {
                *common = Some((dim, crs));
            }
        } else {
            for child in &node.children {
                self.coordinate_settings(child, dim, crs, common)?;
            }
        }
        Ok(())
    }

    fn coordinate_dimension(&mut self, dim: usize, crs: Option<&'a str>) -> Result<usize> {
        let dim = if dim != 0 {
            dim
        } else if let Some(name) = crs {
            self.reference(name)?.dimension.ok_or_else(|| invalid("cannot derive coordinate dimension from CRS; declare srsDimension"))?
        } else {
            3
        };
        if !(2..=3).contains(&dim) {
            return Err(Error::Unsupported(format!("CityGML coordinate dimension {dim}")));
        }
        Ok(dim)
    }

    fn coordinate_context(&mut self, node: &'a Node, context: &Context<'a>) -> Result<Context<'a>> {
        let mut common = None;
        self.coordinate_settings(node, context.dim, context.crs, &mut common)?;
        let mut resolved = context.clone();
        if let Some((dim, crs)) = common {
            resolved.dim = dim;
            resolved.crs = crs;
        } else {
            resolved.dim = self.coordinate_dimension(context.dim, context.crs)?;
        }
        Ok(resolved)
    }

    fn normalize(&mut self, geometry: Geometry, context: &Context<'a>) -> Result<Geometry> {
        let swap = if let Some(name) = context.crs {
            self.reference(name)?.swap
        } else if self.crs.is_some() {
            return Err(invalid("geometry without CRS mixed with georeferenced geometry"));
        } else {
            self.unknown_crs = true;
            false
        };
        let convert = |mut c: Coord| -> Result<Coord> {
            if let Some((matrix, anchor)) = &context.matrix {
                // Stored anchors use canonical XY. Matrix input/output follow the
                // relative geometry's declared axes until the final normalization.
                let mut anchor = *anchor;
                if swap {
                    anchor.swap(0, 1);
                }
                let p = [c.x, c.y, c.z.unwrap_or(0.0), 1.0];
                let p: [f64; 3] = std::array::from_fn(|i| (0..4).map(|j| matrix[i * 4 + j] * p[j]).sum::<f64>() + anchor[i]);
                if p.iter().any(|v| !v.is_finite()) {
                    return Err(invalid("nonfinite implicit coordinate"));
                }
                c = Coord { x: p[0], y: p[1], z: Some(p[2]), m: None };
            }
            if swap {
                std::mem::swap(&mut c.x, &mut c.y);
            }
            Ok(c)
        };
        let list = |points: Vec<Coord>| -> Result<Vec<Coord>> { points.into_iter().map(convert).collect() };
        Ok(match geometry {
            Geometry::Point(c) => Geometry::Point(convert(c)?),
            Geometry::LineString(c) => Geometry::LineString(list(c)?),
            Geometry::Polygon { exterior, interiors } => {
                let make = |ring: Ring| -> Result<Ring> {
                    let mut p = list(ring.0)?;
                    if context.reverse {
                        p.reverse();
                    }
                    Ok(Ring::new(p))
                };
                Geometry::Polygon { exterior: make(exterior)?, interiors: interiors.into_iter().map(make).collect::<Result<_>>()? }
            }
            _ => return Err(invalid("expected a flat primitive")),
        })
    }

    fn check_geometry_context(&mut self, node: &'a Node, context: &Context<'a>) -> Result<()> {
        // Resolve leaf overrides before comparing a shared primitive's context.
        let swap = context.crs.map(|name| self.reference(name).map(|r| r.swap)).transpose()?;
        if let Some((dim, crs)) = self.geometry_contexts.get(&(node as *const Node)).copied() {
            let previous = crs.map(|name| self.reference(name).map(|r| r.swap)).transpose()?;
            if dim != context.dim || previous != swap {
                return Err(invalid("conflicting effective coordinate contexts for a reused city object"));
            }
        } else {
            self.geometry_contexts.insert(node as *const Node, (context.dim, context.crs));
        }
        Ok(())
    }

    fn surface_semantic(&mut self, node: &Node) -> Rc<str> {
        if let Some(value) = self.surface_semantics.get(&(node as *const Node)) {
            value.clone()
        } else {
            let mut props = attributes(node).as_object().cloned().unwrap_or_default();
            props.insert("type".into(), Value::String(node.name.clone()));
            let value = Value::Object(props);
            let text = self.semantic_values.entry(value.clone()).or_insert_with(|| Rc::from(value.to_string())).clone();
            self.surface_semantics.insert(node as *const Node, text.clone());
            text
        }
    }

    fn update_semantic(&mut self, index: usize, value: Option<&Rc<str>>) -> Result<()> {
        if let Some(value) = value
            && !self.face_semantics.get(&index).is_some_and(|previous| Rc::ptr_eq(previous, value))
        {
            if self.face_semantics.contains_key(&index) {
                return Err(invalid("conflicting thematic classifications for a shared primitive"));
            }
            let old = match &self.layer.features[index].attributes[SURFACE_SEMANTICS] {
                FieldValue::Text(text) => text.len(),
                _ => 0,
            };
            super::city::charge_replacement(&mut self.used, old, value.len(), "CityGML")?;
            self.layer.features[index].attributes[SURFACE_SEMANTICS] = FieldValue::Text(value.to_string());
            self.face_semantics.insert(index, value.clone());
        }
        Ok(())
    }

    fn emit(&mut self, node: &'a Node, context: &Context<'a>, base: &[FieldValue]) -> Result<()> {
        if context.validate_only {
            let resolved = self.coordinate_context(node, context)?;
            self.check_geometry_context(node, &resolved)?;
            return Ok(());
        }
        let id = node.id().filter(|_| context.matrix.is_none());
        let key = id.map(|id| {
            (
                match &base[CITY_ID] {
                    FieldValue::Text(s) => s.clone(),
                    _ => String::new(),
                },
                id.to_owned(),
                context.lod,
                context.reverse,
            )
        });
        let previous = key.as_ref().and_then(|key| self.emitted.get(key)).copied();
        let validation = id.map(|id| (id, context.dim, context.crs.map(str::trim)));
        if let Some(index) = previous
            && validation.is_some_and(|key| self.validated.contains(&key))
        {
            return self.update_semantic(index, context.semantic.as_ref());
        }
        if let Some(index) = previous {
            let resolved = self.coordinate_context(node, context)?;
            self.check_geometry_context(node, &resolved)?;
            if let Some(key) = validation {
                self.validated.insert(key);
            }
            return self.update_semantic(index, context.semantic.as_ref());
        }
        // Bound numerical work for newly emitted primitives independently of
        // XML node visits; deduplicated reuse above allocates no coordinates.
        if context.expanded {
            self.coordinate_bytes = self.coordinate_bytes.saturating_add(coordinate_text_bytes(node));
            if self.coordinate_bytes > self.max_coordinate_bytes {
                return Err(invalid("XLink coordinate parsing exceeds the reference coordinate-byte limit"));
            }
        }
        let resolved = self.coordinate_context(node, context)?;
        self.check_geometry_context(node, &resolved)?;
        let remaining = super::city::MAX_FEATURE_BYTES.saturating_sub(self.used);
        preflight_coordinates(node, resolved.dim, remaining)?;
        let geometry = self.normalize(primitive(node, resolved.dim)?, &resolved)?;
        if let Some(key) = validation {
            self.validated.insert(key);
        }
        if let Some(key) = key {
            self.emitted.insert(key, self.layer.features.len());
        }
        let mut attrs = base.to_vec();
        attrs[LOD] = context.lod.map_or(FieldValue::Null, |s| FieldValue::Text(s.to_owned()));
        attrs[GEOMETRY_TYPE] = FieldValue::Text(context.geometry_type.unwrap_or(&node.name).into());
        attrs[SURFACE_SEMANTICS] = context.semantic.as_ref().map_or(FieldValue::Null, |s| FieldValue::Text(s.to_string()));
        let index = self.layer.features.len();
        super::city::push(&mut self.layer, Some(geometry), &attrs, &mut self.used)?;
        if let Some(value) = &context.semantic {
            self.face_semantics.insert(index, value.clone());
        }
        Ok(())
    }

    fn walk(&mut self, node: &'a Node, mut context: Context<'a>, base: &[FieldValue], depth: usize) -> Result<()> {
        if context.expanded {
            self.visits += 1;
            if self.visits > self.max_reference_visits {
                return Err(invalid("XLink expansion exceeds the reference node-visit limit"));
            }
        }
        if depth > MAX_NESTING {
            return Err(invalid("city-object or geometry nesting limit exceeded"));
        }
        if node.city() && matches!(node.name.as_str(), "appearance" | "appearanceMember" | "Appearance" | "Address" | "externalReference") {
            return Ok(());
        }
        if node.city() && node.name == "RasterRelief" {
            return Err(Error::Unsupported("CityGML: raster relief is not a flat vector geometry".into()));
        }
        context.dim = dimension(node, context.dim)?;
        context.crs = node.attr("", "srsName").or(context.crs);
        if node.city() && node.name.starts_with("lod") {
            let digits = node.name.as_bytes()[3..].iter().take_while(|byte| byte.is_ascii_digit()).count();
            if digits != 0 {
                context.lod = Some(&node.name[3..3 + digits]);
                context.geometry_type = None;
            }
        }
        if !context.validate_only && node.city() && node.name.ends_with("Surface") && !node.name.starts_with("lod") {
            context.semantic = Some(self.surface_semantic(node));
        }
        if node.gml() && node.name == "OrientableSurface" && node.attr("", "orientation") == Some("-") {
            context.reverse = !context.reverse;
        }
        let resolved = self.link(node)?;
        if !std::ptr::eq(node, resolved) {
            let id = resolved.id().ok_or_else(|| invalid("XLink target has no ID"))?;
            if !self.active_links.insert(id) {
                return Err(invalid("cyclic geometry XLink"));
            }
            context.expanded = true;
            let result = self.walk(resolved, context, base, depth + 1);
            self.active_links.remove(id);
            return result;
        }
        if is_object(node) {
            return self.object(node, Some(&base[CITY_ID]), context, depth + 1);
        }
        if node.city() && node.name == "ImplicitGeometry" {
            let relative = node
                .child("relativeGMLGeometry")
                .or_else(|| node.child("relativeGeometry"))
                .ok_or_else(|| invalid("implicit geometry has no relative geometry"))?;
            let reference = node.child("referencePoint").ok_or_else(|| invalid("implicit geometry has no reference point"))?;
            let point = self.link(reference)?;
            let point_context = self.coordinate_context(point, &context)?;
            if let Some(name) = point_context.crs {
                self.reference(name)?;
            }
            preflight_coordinates(point, point_context.dim, std::mem::size_of::<Coord>())?;
            let coords = coordinates(point, point_context.dim)?;
            context.crs = point_context.crs;
            if coords.len() != 1 {
                return Err(invalid("implicit referencePoint must have one position"));
            }
            let mut anchor = [coords[0].x, coords[0].y, coords[0].z.unwrap_or(0.0)];
            if point_context.crs.map(|name| self.reference(name).map(|r| r.swap)).transpose()?.unwrap_or(false) {
                anchor.swap(0, 1);
            }
            let mut matrix = [0.0; 16];
            for i in 0..4 {
                matrix[i * 5] = 1.0;
            }
            if let Some(m) = node.child("transformationMatrix") {
                let values = numbers(&m.text)?;
                if values.len() != 16 {
                    return Err(invalid("implicit matrix must have 16 values"));
                }
                matrix.copy_from_slice(&values);
            }
            if matrix[12..] != [0.0, 0.0, 0.0, 1.0] {
                return Err(Error::Unsupported("CityGML: implicit matrix must be affine".into()));
            }
            if context.matrix.is_some() {
                return Err(invalid("nested implicit geometry"));
            }
            context.matrix = Some((matrix, anchor));
            return self.walk(relative, context, base, depth + 1);
        }
        if node.gml() {
            if node.name == "segments" && node.children.iter().any(|child| !child.gml() || child.name != "LineStringSegment") {
                return Err(Error::Unsupported("CityGML: nonlinear curve segments are not supported".into()));
            }
            if context.geometry_type.is_none()
                && matches!(
                    node.name.as_str(),
                    "Solid" | "MultiSolid" | "CompositeSolid" | "MultiSurface" | "CompositeSurface" | "MultiCurve" | "Curve" | "MultiPoint"
                )
            {
                context.geometry_type = Some(&node.name);
            }
            if matches!(
                node.name.as_str(),
                "Polygon" | "PolygonPatch" | "Triangle" | "Rectangle" | "Point" | "LineString" | "LineStringSegment"
            ) {
                return self.emit(node, &context, base);
            }
            if matches!(node.name.as_str(), "Arc" | "ArcString" | "Circle" | "BSpline" | "Bezier") {
                return Err(Error::Unsupported(format!("CityGML curved primitive {}", node.name)));
            }
            if node.name == "Envelope" {
                return Ok(());
            }
        }
        for child in &node.children {
            self.walk(child, context.clone(), base, depth + 1)?;
        }
        Ok(())
    }

    fn object(&mut self, node: &'a Node, parent: Option<&FieldValue>, mut context: Context<'a>, depth: usize) -> Result<()> {
        if depth > MAX_NESTING {
            return Err(invalid("city-object or geometry nesting limit exceeded"));
        }
        context.dim = dimension(node, context.dim)?;
        let envelope = node.child("boundedBy").and_then(|n| n.child("Envelope"));
        context.crs = node.attr("", "srsName").or_else(|| envelope.and_then(|n| n.attr("", "srsName"))).or(context.crs);
        if let Some(envelope) = envelope {
            context.dim = dimension(envelope, context.dim)?;
        }
        if context.validate_only {
            context.semantic = None;
            context.geometry_type = None;
            for child in &node.children {
                self.walk(child, context.clone(), &[FieldValue::Null], depth + 1)?;
            }
            return Ok(());
        }
        if let Some(id) = node.id() {
            if self.active.contains(id) {
                return Err(invalid("cyclic city-object hierarchy"));
            }
            if self.objects.get(id).is_some_and(|o| !o.contexts.contains(&(context.dim, context.crs))) {
                let mut validation = context.clone();
                validation.validate_only = true;
                validation.semantic = None;
                validation.geometry_type = None;
                for child in &node.children {
                    // Keep the owning object's base fields for nested object references.
                    self.walk(child, validation.clone(), &[FieldValue::Null], depth + 1)?;
                }
                self.objects.get_mut(id).unwrap().contexts.insert((context.dim, context.crs));
            }
            if let Some(object) = self.objects.get_mut(id) {
                if let Some(FieldValue::Text(parent)) = parent
                    && object.seen.insert(parent.clone())
                {
                    let added = serde_json::to_string(parent).map_err(invalid)?.len() + if object.parents.is_empty() { 2 } else { 1 };
                    super::city::charge(
                        &mut self.used,
                        added.saturating_mul(object.rows.len()),
                        super::city::MAX_FEATURE_BYTES,
                        "CityGML",
                    )?;
                    object.parents.push(parent.clone());
                }
                return Ok(());
            }
        }
        context.semantic = None;
        context.geometry_type = None;
        let identity = if let Some(id) = node.id() {
            id.to_owned()
        } else {
            loop {
                let id = format!("_terrio_object_{}", self.next_object);
                self.next_object += 1;
                if !self.ids.contains_key(id.as_str()) {
                    break id;
                }
            }
        };
        if let Some(id) = node.id()
            && !self.active.insert(id)
        {
            return Err(invalid("cyclic city-object hierarchy"));
        }
        let parents = match parent {
            Some(FieldValue::Text(p)) => FieldValue::Text(json!([p]).to_string()),
            _ => FieldValue::Null,
        };
        let attrs = vec![
            FieldValue::Text(identity),
            FieldValue::Text(node.name.clone()),
            FieldValue::Null,
            parents,
            FieldValue::Text(attributes(node).to_string()),
            FieldValue::Null,
            FieldValue::Null,
            FieldValue::Null,
        ];
        let start = self.layer.features.len();
        for child in &node.children {
            self.walk(child, context.clone(), &attrs, depth + 1)?;
        }
        // Preserve parents/nonspatial objects even when only child objects have geometry.
        if !self.layer.features[start..].iter().any(|f| f.attributes[CITY_ID] == attrs[CITY_ID]) {
            super::city::push(&mut self.layer, None, &attrs, &mut self.used)?;
        }
        if let Some(id) = node.id() {
            self.active.remove(id);
            let rows =
                (start..self.layer.features.len()).filter(|&i| self.layer.features[i].attributes[CITY_ID] == attrs[CITY_ID]).collect();
            let parents: Vec<String> = match parent {
                Some(FieldValue::Text(parent)) => vec![parent.clone()],
                _ => Vec::new(),
            };
            self.objects.insert(
                id,
                ObjectRows {
                    contexts: HashSet::from([(context.dim, context.crs)]),
                    rows,
                    seen: parents.iter().cloned().collect(),
                    parents,
                },
            );
        }
        Ok(())
    }
}

pub(crate) fn read_path_bytes(path: &str, shared_xml: bool) -> Result<Vec<u8>> {
    use std::io::{Read, Seek};
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    if size > MAX_INPUT_BYTES as u64 {
        if shared_xml {
            let mut prefix = Vec::new();
            file.by_ref().take(crate::inspection::IDENTIFICATION_HEADER_BYTES as u64).read_to_end(&mut prefix)?;
            if root_probe(&prefix) == Some(false) {
                file.rewind()?;
                let mut bytes = Vec::new();
                file.read_to_end(&mut bytes)?;
                return Ok(bytes);
            }
        }
        return Err(invalid("local input exceeds the 256 MiB XML limit"));
    }
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size as usize).map_err(invalid)?;
    file.take(MAX_INPUT_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(invalid("local input grew beyond the 256 MiB XML limit"));
    }
    Ok(bytes)
}

/// Parse a CityGML 2.0 or 3.0 city model into an ordinary flat vector layer.
pub fn parse(bytes: &[u8]) -> Result<Vec<Layer>> {
    parse_with_limits(bytes, MAX_INPUT_BYTES, MAX_REFERENCE_VISITS, MAX_REFERENCE_COORDINATE_BYTES)
}

fn parse_with_limits(bytes: &[u8], input_limit: usize, max_reference_visits: usize, max_coordinate_bytes: usize) -> Result<Vec<Layer>> {
    if bytes.len() > input_limit {
        return Err(invalid(format!("input exceeds the {input_limit} byte XML limit")));
    }
    super::geojson::reserve_parse_memory(bytes.len(), "CityGML")?;
    let root = xml(bytes)?;
    if root.name != "CityModel" || !matches!(root.ns.as_str(), "http://www.opengis.net/citygml/2.0" | "http://www.opengis.net/citygml/3.0")
    {
        return Err(Error::Unsupported("CityGML: expected a CityModel in the 2.0 or 3.0 core namespace".into()));
    }
    fn index<'a>(node: &'a Node, ids: &mut HashMap<&'a str, &'a Node>) -> Result<()> {
        if let Some(id) = node.id()
            && ids.insert(id, node).is_some()
        {
            return Err(invalid(format!("duplicate gml:id {id}")));
        }
        for child in &node.children {
            index(child, ids)?;
        }
        Ok(())
    }
    let mut ids = HashMap::new();
    index(&root, &mut ids)?;
    let mut reader = Reader {
        ids,
        layer: super::city::layer("CityGML"),
        crs: None,
        references: HashMap::new(),
        used: 0,
        next_object: 0,
        unknown_crs: false,
        active: HashSet::new(),
        active_links: HashSet::new(),
        objects: HashMap::new(),
        validated: HashSet::new(),
        face_semantics: HashMap::new(),
        surface_semantics: HashMap::new(),
        semantic_values: HashMap::new(),
        geometry_contexts: HashMap::new(),
        coordinate_bytes: 0,
        max_coordinate_bytes,
        visits: 0,
        max_reference_visits,
        emitted: HashMap::new(),
    };
    let envelope = root.child("boundedBy").and_then(|n| n.child("Envelope"));
    let root_dim = dimension(&root, 0)?;
    let dim = envelope.map(|n| dimension(n, root_dim)).transpose()?.unwrap_or(root_dim);
    let crs = root.attr("", "srsName").or_else(|| envelope.and_then(|n| n.attr("", "srsName")));
    let context = Context { dim, crs, ..Default::default() };
    for member in root.children.iter().filter(|n| n.city() && n.name == "cityObjectMember") {
        let resolved = reader.link(member)?;
        if std::ptr::eq(member, resolved) {
            for node in &member.children {
                if !node.city() {
                    return Err(Error::Unsupported(format!("CityGML city object namespace {}", node.ns)));
                }
                reader.object(node, None, context.clone(), 0)?;
            }
        } else {
            reader.object(resolved, None, Context { expanded: true, ..context.clone() }, 0)?;
        }
    }
    for object in reader.objects.values() {
        if !object.parents.is_empty() {
            let value = serde_json::to_string(&object.parents).map_err(invalid)?;
            for &row in &object.rows {
                reader.layer.features[row].attributes[PARENTS] = FieldValue::Text(value.clone());
            }
        }
    }
    if reader.crs.is_none()
        && let Some(name) = crs
    {
        parse_crs(name)?.apply_to_layer(&mut reader.layer);
    }
    Ok(vec![reader.layer])
}

/// Whether a byte prefix can start an XML document in one of the supported encodings.
pub(crate) fn is_xml_prefix(bytes: &[u8]) -> bool {
    if let Some(encoding) = utf16_encoding(bytes) {
        let little = std::ptr::eq(encoding, encoding_rs::UTF_16LE);
        let payload = bytes.strip_prefix(b"\xff\xfe").or_else(|| bytes.strip_prefix(b"\xfe\xff")).unwrap_or(bytes);
        // Inspect UTF-16 code units directly: a large non-XML input needs no
        // decoded String, while arbitrary leading XML whitespace still works.
        return payload
            .as_chunks::<2>()
            .0
            .iter()
            .map(|p| if little { u16::from_le_bytes([p[0], p[1]]) } else { u16::from_be_bytes([p[0], p[1]]) })
            .find(|c| !matches!(c, 9 | 10 | 13 | 32))
            .is_some_and(|c| c == u16::from(b'<'));
    }
    bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(bytes).trim_ascii_start().starts_with(b"<")
}

/// `None` means the available prefix does not contain a complete XML root tag.
pub(crate) fn root_probe(bytes: &[u8]) -> Option<bool> {
    let text =
        if let Some(encoding) = utf16_encoding(bytes) { encoding.decode_with_bom_removal(bytes).0 } else { String::from_utf8_lossy(bytes) };
    let mut reader = NsReader::from_str(text.trim_start_matches('\u{feff}').trim_start());
    loop {
        match reader.read_resolved_event() {
            Ok((ResolveResult::Bound(ns), Event::Start(e) | Event::Empty(e))) => {
                return Some(
                    e.local_name().as_ref() == "CityModel"
                        && matches!(ns.as_ref(), "http://www.opengis.net/citygml/2.0" | "http://www.opengis.net/citygml/3.0"),
                );
            }
            Ok((_, Event::Start(_) | Event::Empty(_))) => return Some(false),
            Ok((_, Event::Eof)) | Err(_) => return None,
            _ => {}
        }
    }
}

/// Find the XML root without decoding the document body. Prefixes grow only when
/// a leading declaration/comment or the root start tag needs more input.
pub(crate) fn looks_like(bytes: &[u8]) -> bool {
    if !is_xml_prefix(bytes) {
        return false;
    }
    let mut end = bytes.len().min(8192);
    loop {
        if let Some(city) = root_probe(&bytes[..end]) {
            return city;
        }
        if end == bytes.len() {
            return false;
        }
        end = end.saturating_mul(2).min(bytes.len());
    }
}

#[cfg(test)]
#[path = "../../../tests/support/citygml.rs"]
mod test_data;

#[cfg(test)]
mod tests {
    use super::test_data::{FACE, model};

    #[test]
    fn surface_patches_do_not_duplicate_coordinates_into_object_attributes() {
        for (primitive, container, property) in [
            ("Triangle", "TriangulatedSurface", "trianglePatches"),
            ("PolygonPatch", "Surface", "patches"),
            ("Rectangle", "Surface", "patches"),
        ] {
            let positions = if primitive == "Rectangle" { "0 0 0 1 0 0 1 1 0 0 1 0 0 0 0" } else { "0 0 0 0 1 0 0 1 1 0 0 0" };
            let geometry = format!(
                "<g:{container}><g:{property}><g:{primitive}><g:exterior><g:LinearRing><g:posList>{positions}</g:posList></g:LinearRing></g:exterior></g:{primitive}></g:{property}></g:{container}>"
            );
            let text = if primitive == "Triangle" {
                model("3.0", &format!("<dem:lod>2</dem:lod><dem:tin>{geometry}</dem:tin>"), "EPSG:32632")
                    .replace(
                        "<b:Building g:id=\"house\">",
                        "<dem:TINRelief xmlns:dem=\"http://www.opengis.net/citygml/relief/3.0\" g:id=\"terrain\">",
                    )
                    .replace("</b:Building>", "</dem:TINRelief>")
            } else {
                model(
                    "3.0",
                    &format!(
                        "<b:lod2MultiSurface><g:MultiSurface><g:surfaceMember>{geometry}</g:surfaceMember></g:MultiSurface></b:lod2MultiSurface>"
                    ),
                    "EPSG:32632",
                )
            };
            let layers = super::parse(text.as_bytes()).unwrap();
            assert_eq!(layers[0].features.len(), 1);
            let wbvector::FieldValue::Text(text) = &layers[0].features[0].attributes[4] else { panic!() };
            let attrs: serde_json::Value = serde_json::from_str(text).unwrap();
            assert_eq!(attrs, if primitive == "Triangle" { serde_json::json!({"lod":"2"}) } else { serde_json::json!({}) });
            assert!(matches!(layers[0].features[0].geometry, Some(wbvector::Geometry::Polygon { .. })));
        }
    }

    #[test]
    fn utf16_non_xml_is_rejected_without_decoding_the_payload() {
        for little in [true, false] {
            for text in [
                format!("{{\"type\":\"FeatureCollection\",\"text\":\"{}\"}}", "x".repeat(100_000)),
                format!("id,name\n1,{}", "x".repeat(100_000)),
            ] {
                let mut bytes = if little { vec![0xff, 0xfe] } else { vec![0xfe, 0xff] };
                bytes.extend(text.encode_utf16().flat_map(|unit| if little { unit.to_le_bytes() } else { unit.to_be_bytes() }));
                assert!(!super::is_xml_prefix(&bytes));
                assert!(!super::looks_like(&bytes));
            }
            let text = format!("{}{}", " \t\r\n".repeat(4096), model("3.0", "", "EPSG:32632"));
            let mut bytes = if little { vec![0xff, 0xfe] } else { vec![0xfe, 0xff] };
            bytes.extend(text.encode_utf16().flat_map(|unit| if little { unit.to_le_bytes() } else { unit.to_be_bytes() }));
            assert!(super::looks_like(&bytes));
        }
    }

    #[test]
    fn uppercase_authority_urns_normalize_latitude_first_positions() {
        let text = model("3.0", "<b:lod0Geometry><g:Point><g:pos>52 4 3</g:pos></g:Point></b:lod0Geometry>", "URN:OGC:DEF:CRS:EPSG::4326");
        let layers = super::parse(text.as_bytes()).unwrap();
        let Some(wbvector::Geometry::Point(point)) = &layers[0].features[0].geometry else { panic!() };
        assert_eq!((point.x, point.y, point.z), (4.0, 52.0, Some(3.0)));
    }

    #[test]
    fn implicit_anchors_use_the_relative_geometry_axis_order() {
        for (anchor, relative) in [(false, true), (true, false), (true, true)] {
            let name = |urn| if urn { "urn:ogc:def:crs:EPSG::4326" } else { "EPSG:4326" };
            let anchor_values = if anchor { "52 4 3" } else { "4 52 3" };
            let relative_values = if relative { "2 1 4" } else { "1 2 4" };
            let body = format!(
                "<b:lod1ImplicitRepresentation><b:ImplicitGeometry><b:referencePoint><g:Point srsName=\"{}\" srsDimension=\"3\"><g:pos>{anchor_values}</g:pos></g:Point></b:referencePoint><b:relativeGeometry><g:Point srsName=\"{}\" srsDimension=\"3\"><g:pos>{relative_values}</g:pos></g:Point></b:relativeGeometry></b:ImplicitGeometry></b:lod1ImplicitRepresentation>",
                name(anchor),
                name(relative)
            );
            let layers = super::parse(model("3.0", &body, "EPSG:4326").as_bytes()).unwrap();
            let Some(wbvector::Geometry::Point(point)) = &layers[0].features[0].geometry else { panic!() };
            assert_eq!((point.x, point.y, point.z), (5.0, 54.0, Some(7.0)));
        }
    }

    #[test]
    fn deduplicated_effective_context_changes_do_not_allocate_or_reparse_coordinates() {
        let face = FACE.replace("<g:posList>", "<g:posList srsDimension=\"3\">");
        let body = format!(
            r##"<b:lod2MultiSurface>{face}</b:lod2MultiSurface><b:lod2MultiSurface srsDimension="2"><g:surfaceMember l:href="#wall"/></b:lod2MultiSurface>"##
        );
        let text = model("3.0", &body, "EPSG:32632");
        let layers = super::parse_with_limits(text.as_bytes(), super::MAX_INPUT_BYTES, super::MAX_REFERENCE_VISITS, 1).unwrap();
        assert_eq!(layers[0].features.len(), 1);
    }

    #[test]
    fn coordinate_expansion_is_preflighted_for_the_whole_primitive() {
        let root = super::xml(model("3.0", FACE, "EPSG:32632").as_bytes()).unwrap();
        let face = &root.children.last().unwrap().children[0].children[0];
        assert!(super::preflight_coordinates(face, 3, 8 * std::mem::size_of::<wbvector::Coord>()).is_err());
        assert_eq!(
            super::preflight_coordinates(face, 3, 9 * std::mem::size_of::<wbvector::Coord>()).unwrap(),
            9 * std::mem::size_of::<wbvector::Coord>()
        );
    }

    #[test]
    fn shared_surfaces_reuse_equal_semantics_and_reject_conflicting_classes() {
        let body = format!(
            r##"<b:boundedBy><b:WallSurface g:id="surface"><g:name>brick</g:name><b:lod2MultiSurface>{FACE}</b:lod2MultiSurface></b:WallSurface></b:boundedBy>{}<b:boundedBy><b:WallSurface><g:name>brick</g:name><b:lod2MultiSurface l:href="#wall"/></b:WallSurface></b:boundedBy>"##,
            r##"<b:boundedBy l:href="#surface"/>"##.repeat(1000)
        );
        let text = model("3.0", &body, "EPSG:32632");
        assert_eq!(super::parse(text.as_bytes()).unwrap()[0].features.len(), 1);
        let conflicting = text.replacen("<b:WallSurface><g:name>", "<b:RoofSurface><g:name>", 1).replacen(
            "</b:WallSurface></b:boundedBy></b:Building>",
            "</b:RoofSurface></b:boundedBy></b:Building>",
            1,
        );
        assert!(super::parse(conflicting.as_bytes()).unwrap_err().to_string().contains("conflicting thematic"));
    }

    #[test]
    fn implicit_anchor_crs_is_checked_even_with_explicit_dimension() {
        let body = r#"<b:lod1ImplicitRepresentation><b:ImplicitGeometry><b:referencePoint><g:Point srsName="EPSG:4326" srsDimension="3"><g:pos>1 2 3</g:pos></g:Point></b:referencePoint><b:relativeGeometry><g:Point srsName="EPSG:32632"><g:pos>1 2 3</g:pos></g:Point></b:relativeGeometry></b:ImplicitGeometry></b:lod1ImplicitRepresentation>"#;
        assert!(super::parse(model("3.0", body, "EPSG:32632").as_bytes()).unwrap_err().to_string().contains("mixed CRSs"));
    }

    #[test]
    fn oversized_paths_are_rejected_by_the_native_read_preflight() {
        use std::io::Write;
        let folder = tempfile::tempdir().unwrap();
        let path = folder.path().join("oversized.gml");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(model("3.0", "", "EPSG:32632").as_bytes()).unwrap();
        file.set_len(super::MAX_INPUT_BYTES as u64 + 1).unwrap();
        for shared in [false, true] {
            assert!(super::read_path_bytes(path.to_str().unwrap(), shared).unwrap_err().to_string().contains("256 MiB XML limit"));
        }
    }

    #[test]
    fn xml_tree_storage_is_limited_before_many_empty_nodes_are_allocated() {
        let text = model("3.0", &"<b:note/>".repeat(1000), "EPSG:32632");
        assert!(super::xml_with_limit(text.as_bytes(), 16 * 1024).unwrap_err().to_string().contains("XML tree payload"));
        super::xml(text.as_bytes()).unwrap();
    }

    #[test]
    fn crs84_authority_urls_keep_longitude_first() {
        for name in ["urn:ogc:def:crs:OGC:1.3:CRS83", "urn:ogc:def:crs:OGC:1.3:CRS27"] {
            assert!(!super::authority_axis_order(name));
        }
        for name in ["http://www.opengis.net/def/crs/OGC/1.3/CRS84", "urn:ogc:def:crs:OGC:1.3:CRS84"] {
            let text = model("3.0", "<b:lod1Geometry><g:Point><g:pos>4 52 30</g:pos></g:Point></b:lod1Geometry>", name);
            let layer = super::parse(text.as_bytes()).unwrap().remove(0);
            let wbvector::Geometry::Point(point) = layer.features[0].geometry.as_ref().unwrap() else { panic!() };
            assert_eq!((point.x, point.y, point.z), (4.0, 52.0, Some(30.0)));
        }
    }

    #[test]
    fn legacy_coordinate_tuple_width_matches_the_declared_dimension() {
        let body = "<b:lod1Geometry><g:Point><g:coordinates>1,2</g:coordinates></g:Point></b:lod1Geometry>";
        let text = model("2.0", body, "EPSG:32632");
        assert!(super::parse(text.as_bytes()).unwrap_err().to_string().contains("coordinate tuple"));
        super::parse(text.replace("srsDimension=\"3\"", "srsDimension=\"2\"").as_bytes()).unwrap();
        let wrong = text.replace("srsDimension=\"3\"", "srsDimension=\"2\"").replace("1,2</g:coordinates>", "1,2,3</g:coordinates>");
        assert!(super::parse(wrong.as_bytes()).unwrap_err().to_string().contains("coordinate tuple"));
        let start = text.find("<g:boundedBy>").unwrap();
        let end = text.find("</g:boundedBy>").unwrap() + "</g:boundedBy>".len();
        let unknown = format!("{}{}", &text[..start], &text[end..]);
        let layer = super::parse(unknown.as_bytes()).unwrap().remove(0);
        let wbvector::Geometry::Point(point) = layer.features[0].geometry.as_ref().unwrap() else { panic!() };
        assert_eq!((point.x, point.y, point.z), (1.0, 2.0, None));
    }

    #[test]
    fn nonlinear_segments_inside_polygon_rings_are_rejected() {
        let body = r#"<b:lod2MultiSurface><g:Polygon><g:exterior><g:Ring><g:curveMember><g:Curve><g:segments><g:Arc><g:posList>0 0 0 1 1 0 2 0 0</g:posList></g:Arc></g:segments></g:Curve></g:curveMember><g:curveMember><g:LineString><g:posList>2 0 0 0 0 0</g:posList></g:LineString></g:curveMember></g:Ring></g:exterior></g:Polygon></b:lod2MultiSurface>"#;
        assert!(super::parse(model("3.0", body, "EPSG:32632").as_bytes()).unwrap_err().to_string().contains("nonlinear"));
    }

    #[test]
    fn reused_objects_check_each_effective_coordinate_context() {
        let base = model(
            "3.0",
            "<b:consistsOfBuildingPart><b:BuildingPart g:id=\"part\"><b:lod1Geometry><g:Point><g:pos>1 2 3</g:pos></g:Point></b:lod1Geometry></b:BuildingPart></b:consistsOfBuildingPart>",
            "EPSG:32632",
        );
        for settings in ["srsName=\"EPSG:4326\"", "srsDimension=\"2\""] {
            let extra = format!(
                r##"<c:cityObjectMember><b:Building g:id="other" {settings}><b:consistsOfBuildingPart l:href="#part"/></b:Building></c:cityObjectMember>"##
            );
            let text = base.replace("</c:CityModel>", &(extra + "</c:CityModel>"));
            assert!(super::parse(text.as_bytes()).is_err());
            let overridden = text.replace("g:id=\"part\"", "g:id=\"part\" srsName=\"EPSG:32632\" srsDimension=\"3\"");
            super::parse(overridden.as_bytes()).unwrap();
            for tag in ["<g:Point>", "<g:pos>"] {
                let overridden = text.replace(tag, &tag.replace('>', " srsName=\"EPSG:32632\" srsDimension=\"3\">"));
                super::parse(overridden.as_bytes()).unwrap();
            }
        }
    }

    #[test]
    fn xlink_properties_are_not_empty_scalar_attributes() {
        let body = format!(
            r##"<b:lod2MultiSurface l:href="#wall"/><b:boundary><b:WallSurface><b:lod2MultiSurface l:href="#wall"/></b:WallSurface></b:boundary><b:lod2MultiSurface>{FACE}</b:lod2MultiSurface>"##
        );
        let layer = super::parse(model("3.0", &body, "EPSG:32632").as_bytes()).unwrap().remove(0);
        assert_eq!(layer.features[0].attributes[4], wbvector::FieldValue::Text("{}".into()));
        assert_eq!(layer.features[0].attributes[super::SURFACE_SEMANTICS], wbvector::FieldValue::Text("{\"type\":\"WallSurface\"}".into()));
    }

    #[test]
    fn repeated_links_reuse_validation_and_bound_coordinate_parsing() {
        let link = r##"<g:surfaceMember l:href="#wall"/>"##;
        let body = format!(
            "<b:lod2MultiSurface><g:MultiSurface>{}<g:surfaceMember>{FACE}</g:surfaceMember></g:MultiSurface></b:lod2MultiSurface>",
            link.repeat(5000)
        );
        let text = model("3.0", &body, "EPSG:32632");
        let layer = super::parse_with_limits(text.as_bytes(), super::MAX_INPUT_BYTES, 100_000, 256).unwrap().remove(0);
        assert_eq!(layer.features.len(), 1);
        let error = super::parse_with_limits(text.as_bytes(), super::MAX_INPUT_BYTES, 100_000, 1).unwrap_err();
        assert!(error.to_string().contains("coordinate-byte limit"));
        let mixed = text.replacen(link, &format!("{link}<g:surfaceMember srsName=\"EPSG:4326\" l:href=\"#wall\"/>"), 1);
        assert!(super::parse(mixed.as_bytes()).unwrap_err().to_string().contains("mixed CRSs"));
    }

    #[test]
    fn shared_objects_accumulate_many_parents_in_source_order() {
        let mut text = String::from(
            r#"<c:CityModel xmlns:c="http://www.opengis.net/citygml/3.0" xmlns:b="http://www.opengis.net/citygml/building/3.0" xmlns:g="http://www.opengis.net/gml/3.2" xmlns:l="http://www.w3.org/1999/xlink">"#,
        );
        for i in 0..1000 {
            text.push_str(&format!(r#"<c:cityObjectMember><b:Building g:id="parent{i}">"#));
            if i == 0 {
                text.push_str(r#"<b:consistsOfBuildingPart><b:BuildingPart g:id="shared"/></b:consistsOfBuildingPart>"#);
            } else {
                text.push_str(r##"<b:consistsOfBuildingPart l:href="#shared"/>"##);
            }
            text.push_str("</b:Building></c:cityObjectMember>");
        }
        text.push_str("</c:CityModel>");
        let layer = super::parse(text.as_bytes()).unwrap().remove(0);
        let feature = layer.features.iter().find(|f| f.attributes[super::CITY_ID] == wbvector::FieldValue::Text("shared".into())).unwrap();
        let wbvector::FieldValue::Text(parents) = &feature.attributes[super::PARENTS] else { panic!() };
        let values: Vec<String> = serde_json::from_str(parents).unwrap();
        assert_eq!(values, (0..1000).map(|i| format!("parent{i}")).collect::<Vec<_>>());
        assert_eq!(layer.features.len(), 1001);
    }

    #[test]
    fn declared_legacy_encodings_preserve_non_latin1_characters() {
        let text = format!("<?xml version=\"1.0\" encoding=\"windows-1252\"?>{}", model("3.0", "<g:name>€</g:name>", "EPSG:32632"));
        let (bytes, _, errors) = encoding_rs::WINDOWS_1252.encode(&text);
        assert!(!errors);
        let layer = super::parse(&bytes).unwrap().remove(0);
        let wbvector::FieldValue::Text(attrs) = &layer.features[0].attributes[4] else { panic!() };
        assert_eq!(serde_json::from_str::<serde_json::Value>(attrs).unwrap()["name"], "€");
        let invalid = text.replace("windows-1252", "unknown-encoding");
        assert!(super::parse(invalid.as_bytes()).unwrap_err().to_string().contains("XML encoding"));
    }

    #[test]
    fn linked_object_chains_share_the_global_nesting_limit() {
        let mut text = String::from(
            r#"<c:CityModel xmlns:c="http://www.opengis.net/citygml/3.0" xmlns:g="http://www.opengis.net/gml/3.2" xmlns:b="http://www.opengis.net/citygml/building/3.0" xmlns:l="http://www.w3.org/1999/xlink">"#,
        );
        for i in 0..600 {
            text.push_str(&format!(r#"<c:cityObjectMember><b:BuildingPart g:id="part{i}">"#));
            if i < 599 {
                text.push_str(&format!(r##"<b:consistsOfBuildingPart l:href="#part{}"/>"##, i + 1));
            }
            text.push_str("</b:BuildingPart></c:cityObjectMember>");
        }
        text.push_str("</c:CityModel>");
        assert!(super::parse(text.as_bytes()).unwrap_err().to_string().contains("nesting limit"));
    }

    #[test]
    fn inline_elements_do_not_consume_the_reference_expansion_budget() {
        let body = "<b:note>ordinary</b:note>".repeat(4096);
        let text = model("3.0", &body, "EPSG:32632");
        let layer =
            super::parse_with_limits(text.as_bytes(), super::MAX_INPUT_BYTES, 16, super::MAX_REFERENCE_COORDINATE_BYTES).unwrap().remove(0);
        assert_eq!(layer.features.len(), 1);
    }

    #[test]
    fn input_limit_is_checked_before_xml_decoding() {
        let text = model("3.0", "", "EPSG:32632");
        assert!(
            super::parse_with_limits(text.as_bytes(), text.len() - 1, super::MAX_REFERENCE_VISITS, super::MAX_REFERENCE_COORDINATE_BYTES)
                .unwrap_err()
                .to_string()
                .contains("XML limit")
        );
    }

    #[test]
    fn alternating_two_position_rings_are_rejected_explicitly() {
        let face = FACE.replace("10 0 0 10 10 0 10 10 10 10 0 10 10 0 0", "10 0 0 10 10 0 10 0 0 10 10 0 10 0 0");
        let text = model("3.0", &format!("<b:lod2MultiSurface>{face}</b:lod2MultiSurface>"), "EPSG:32632");
        assert!(super::parse(text.as_bytes()).unwrap_err().to_string().contains("three distinct"));
    }

    #[test]
    fn omitted_dimension_uses_the_crs_and_position_level_reference() {
        for (reference, position, wanted) in [("EPSG:32632", "1 2", (1.0, 2.0, None)), ("EPSG:4979", "4 52 30", (4.0, 52.0, Some(30.0)))] {
            let body = format!("<b:lod1Geometry><g:Point><g:pos srsName=\"{reference}\">{position}</g:pos></g:Point></b:lod1Geometry>");
            let text = model("3.0", &body, reference).replace("srsDimension=\"3\"", "");
            let layers = super::parse(text.as_bytes()).unwrap();
            let wbvector::Geometry::Point(point) = layers[0].features[0].geometry.as_ref().unwrap() else { panic!() };
            assert_eq!((point.x, point.y, point.z), wanted);
        }
    }

    #[test]
    fn generic_numeric_attributes_do_not_accept_json_arrays_as_numbers() {
        let body = r#"<b:intAttribute name="bad"><b:value>[1,2]</b:value></b:intAttribute><b:doubleAttribute name="height"><b:value>12.5</b:value></b:doubleAttribute>"#;
        let layers = super::parse(model("3.0", body, "EPSG:32632").as_bytes()).unwrap();
        let wbvector::FieldValue::Text(attrs) = &layers[0].features[0].attributes[4] else { panic!() };
        let attrs: serde_json::Value = serde_json::from_str(attrs).unwrap();
        assert_eq!(attrs["bad"], "[1,2]");
        assert_eq!(attrs["height"], 12.5);
    }

    #[test]
    fn malformed_xml_links_coordinates_and_namespaces_fail_explicitly() {
        let base = model("3.0", &format!("<b:lod2MultiSurface>{FACE}</b:lod2MultiSurface>"), "EPSG:32632");
        for text in [
            base.replace("10 0 0 10 10 0", "NaN 0 0 10 10 0"),
            base.replace("</c:CityModel>", ""),
            base.replace("</b:Building>", "</b:BuildingPart>"),
            base.replace("http://www.opengis.net/citygml/3.0", "urn:fake"),
            model("3.0", r##"<b:lod2MultiSurface l:href="#missing"/>"##, "EPSG:32632"),
            model("3.0", r#"<b:lod2MultiSurface l:href="https://example.invalid/model.gml#face"/>"#, "EPSG:32632"),
            model("3.0", &format!("<b:lod2Geometry>{FACE}{FACE}</b:lod2Geometry>"), "EPSG:32632"),
        ] {
            assert!(super::parse(text.as_bytes()).is_err(), "accepted {}", &text[..text.len().min(200)]);
        }
    }

    #[test]
    fn mixed_unknown_and_known_crs_are_rejected_in_both_orders() {
        let unknown = "<b:lod1Geometry><g:Point><g:pos>1 2 3</g:pos></g:Point></b:lod1Geometry>";
        let known = "<b:lod1Geometry><g:Point srsName=\"EPSG:32631\"><g:pos>1 2 3</g:pos></g:Point></b:lod1Geometry>";
        for body in [format!("{unknown}{known}"), format!("{known}{unknown}")] {
            let text = model("3.0", &body, "EPSG:32632").replace("srsName=\"EPSG:32632\"", "");
            assert!(super::parse(text.as_bytes()).unwrap_err().to_string().contains("without CRS mixed"));
        }
    }

    #[test]
    fn geometry_reference_cycles_and_branching_expansion_fail_with_bounded_work() {
        let cycle = r##"<b:lod2MultiSurface><g:CompositeSurface g:id="loop"><g:surfaceMember l:href="#loop"/><g:surfaceMember l:href="#loop"/></g:CompositeSurface></b:lod2MultiSurface>"##;
        assert!(super::parse(model("3.0", cycle, "EPSG:32632").as_bytes()).unwrap_err().to_string().contains("cyclic geometry XLink"));
        let mut graph = String::from("<b:lod2MultiSurface><g:MultiSurface>");
        for i in 0..22 {
            graph.push_str(&format!(r##"<g:CompositeSurface g:id="node{i}"><g:surfaceMember l:href="#node{}"/><g:surfaceMember l:href="#node{}"/></g:CompositeSurface>"##, i+1, i+1));
        }
        graph.push_str(r#"<g:CompositeSurface g:id="node22"/></g:MultiSurface></b:lod2MultiSurface>"#);
        assert!(super::parse(model("3.0", &graph, "EPSG:32632").as_bytes()).unwrap_err().to_string().contains("node-visit limit"));
    }

    #[test]
    fn nonlinear_curve_segments_are_rejected_by_the_native_reader() {
        let body = "<b:lod2MultiCurve><g:Curve><g:segments><g:GeodesicString><g:posList>1 2 3 4 5 6</g:posList></g:GeodesicString></g:segments></g:Curve></b:lod2MultiCurve>";
        assert!(super::parse(model("3.0", body, "EPSG:32632").as_bytes()).unwrap_err().to_string().contains("nonlinear"));
    }
}

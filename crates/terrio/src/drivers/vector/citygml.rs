// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! CityGML 2.0 and 3.0, following the OGC encoding specifications.
//!
//! Exposes ordinary XYZ faces, lines and points rather than a second city model.
//! GDAL 3.13.3 provides only a partial CityGML reader; the specifications and
//! published schemas are the reference for this implementation.

use crate::crs::SpatialRef;
use crate::{Error, Result};
use quick_xml::events::Event;
use quick_xml::name::ResolveResult;
use quick_xml::{NsReader, XmlVersion};
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet};
use wbvector::{Coord, FieldValue, Geometry, Layer, Ring};

const XLINK: &str = "http://www.w3.org/1999/xlink";
const GML: &str = "http://www.opengis.net/gml";
const GML32: &str = "http://www.opengis.net/gml/3.2";

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

fn xml(bytes: &[u8]) -> Result<Node> {
    let decoded = super::xml::decode(bytes);
    let mut reader = NsReader::from_str(&decoded);
    let mut stack = vec![Node::default()];
    let mut count = 0usize;
    loop {
        let (ns, event) = reader.read_resolved_event().map_err(invalid)?;
        let ns = namespace(ns)?;
        match event {
            Event::Start(ref e) | Event::Empty(ref e) => {
                count += 1;
                if count > 5_000_000 || stack.len() > 256 {
                    return Err(invalid("XML element or nesting limit exceeded"));
                }
                let mut node = Node { name: e.local_name().as_ref().to_string(), ns, ..Default::default() };
                for attr in e.attributes() {
                    let attr = attr.map_err(invalid)?;
                    if attr.key.as_ref() == "xmlns" || attr.key.as_ref().starts_with("xmlns:") {
                        continue;
                    }
                    let (ns, name) = reader.resolver().resolve_attribute(attr.key);
                    node.attrs.push((
                        namespace(ns)?,
                        name.as_ref().to_string(),
                        attr.normalized_value(XmlVersion::Implicit1_0).map_err(invalid)?.into_owned(),
                    ));
                }
                if matches!(event, Event::Empty(_)) {
                    stack.last_mut().unwrap().children.push(node);
                } else {
                    stack.push(node);
                }
            }
            Event::End(_) => {
                if stack.len() <= 1 {
                    return Err(invalid("unmatched XML end tag"));
                }
                let node = stack.pop().unwrap();
                stack.last_mut().unwrap().children.push(node);
            }
            Event::Text(t) => stack.last_mut().unwrap().text.push_str(&t.xml10_content()),
            Event::CData(t) => stack.last_mut().unwrap().text.push_str(&t.xml10_content()),
            Event::GeneralRef(r) => {
                let text = format!("&{};", &*r);
                let value = quick_xml::escape::unescape(&text).map_err(invalid)?;
                stack.last_mut().unwrap().text.push_str(&value);
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
    text.split_whitespace()
        .map(|s| s.parse::<f64>().ok().filter(|v| v.is_finite()).ok_or_else(|| invalid("nonfinite or invalid coordinate")))
        .collect()
}

fn dimension(node: &Node, inherited: usize) -> Result<usize> {
    let dim = node.attr("", "srsDimension").map(|d| d.parse::<usize>().map_err(invalid)).transpose()?.unwrap_or(inherited);
    if !(2..=3).contains(&dim) {
        return Err(Error::Unsupported(format!("CityGML coordinate dimension {dim}")));
    }
    Ok(dim)
}

fn coordinates(node: &Node, inherited: usize) -> Result<Vec<Coord>> {
    let dim = dimension(node, inherited)?;
    if node.gml() && matches!(node.name.as_str(), "pos" | "posList") {
        let values = numbers(&node.text)?;
        if values.len() % dim != 0 || (node.name == "pos" && values.len() != dim) {
            return Err(invalid("coordinate count does not match srsDimension"));
        }
        if let Some(count) = node.attr("", "count")
            && count.parse::<usize>().map_err(invalid)? != values.len() / dim
        {
            return Err(invalid("posList count does not match coordinates"));
        }
        return Ok(values.chunks_exact(dim).map(|p| Coord { x: p[0], y: p[1], z: (dim == 3).then(|| p[2]), m: None }).collect());
    }
    if node.gml() && node.name == "coordinates" {
        let separator = node.attr("", "cs").unwrap_or(",");
        let decimal = node.attr("", "decimal").unwrap_or(".");
        let tuples = node.attr("", "ts").unwrap_or(" ");
        let tuples: Vec<&str> =
            if tuples.trim().is_empty() { node.text.split_whitespace().collect() } else { node.text.split(tuples).collect() };
        return tuples
            .into_iter()
            .filter(|p| !p.trim().is_empty())
            .map(|p| {
                let values = p
                    .split(separator)
                    .map(|n| n.trim().replace(decimal, ".").parse::<f64>().map_err(invalid))
                    .collect::<Result<Vec<_>>>()?;
                if !(2..=3).contains(&values.len()) || values.iter().any(|v| !v.is_finite()) {
                    return Err(invalid("invalid coordinate tuple"));
                }
                Ok(Coord { x: values[0], y: values[1], z: values.get(2).copied(), m: None })
            })
            .collect();
    }
    if node.attr(XLINK, "href").is_some() {
        return Err(Error::Unsupported("CityGML: linked coordinate/ring content is not supported".into()));
    }
    let mut out = Vec::new();
    for child in &node.children {
        out.extend(coordinates(child, dim)?);
    }
    Ok(out)
}

fn ring(node: &Node, dim: usize) -> Result<Ring> {
    let mut points = coordinates(node, dim)?;
    points.dedup();
    if points.len() > 1 && points.first() == points.last() {
        points.pop();
    }
    if points.len() < 3 {
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
    node.gml() && matches!(node.name.as_str(), "Polygon" | "Point" | "Curve" | "MultiSurface" | "Solid" | "LineString" | "Envelope")
        || node.children.iter().any(has_geometry)
}

fn attributes(node: &Node) -> Value {
    let mut attrs = Map::new();
    for child in &node.children {
        if matches!(child.name.as_str(), "boundedBy" | "boundary" | "appearance" | "appearanceMember")
            || has_geometry(child)
            || child.children.iter().any(is_object)
        {
            continue;
        }
        if child.name.ends_with("Attribute") {
            if let (Some(name), Some(value)) = (child.attr("", "name"), child.child("value")) {
                let text = value.text.trim();
                let v = match child.name.as_str() {
                    "intAttribute" | "doubleAttribute" => serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.into())),
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
                | "IntBuildingInstallation"
                | "BuildingFurniture"
                | "Door"
                | "Window"
                | "Bridge"
                | "BridgePart"
                | "BridgeRoom"
                | "BridgeInstallation"
                | "BridgeFurniture"
                | "Tunnel"
                | "TunnelPart"
                | "TunnelRoom"
                | "HollowSpace"
                | "TunnelInstallation"
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
    lod: Option<String>,
    semantic: Option<Value>,
    geometry_type: Option<&'a str>,
    reverse: bool,
    matrix: Option<([f64; 16], [f64; 3])>,
}

struct Reader<'a> {
    ids: HashMap<&'a str, &'a Node>,
    layer: Layer,
    crs: Option<SpatialRef>,
    used: usize,
    next_object: usize,
    unknown_crs: bool,
    active: HashSet<&'a str>,
    active_links: HashSet<&'a str>,
    objects: HashMap<&'a str, Vec<usize>>,
    visits: usize,
    emitted: HashMap<(String, String, Option<String>, bool), usize>,
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

    fn normalize(&mut self, geometry: Geometry, context: &Context<'_>) -> Result<Geometry> {
        let mut swap = false;
        if let Some(name) = context.crs {
            if self.unknown_crs {
                return Err(invalid("geometry without CRS mixed with georeferenced geometry"));
            }
            let crs = parse_crs(name)?;
            if self.crs.as_ref().is_some_and(|s| !s.same_as(&crs)) {
                return Err(Error::Unsupported("CityGML: mixed CRSs in one flat layer".into()));
            }
            swap = crs.is_geographic() && (name.starts_with("urn:") || name.contains("/def/crs/"));
            if self.crs.is_none() {
                crs.apply_to_layer(&mut self.layer);
                self.crs = Some(crs);
            }
        } else if self.crs.is_some() {
            return Err(invalid("geometry without CRS mixed with georeferenced geometry"));
        } else {
            self.unknown_crs = true;
        }
        let convert = |mut c: Coord| -> Result<Coord> {
            if let Some((matrix, anchor)) = &context.matrix {
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

    fn emit(&mut self, node: &'a Node, context: &Context<'a>, base: &[FieldValue]) -> Result<()> {
        let mut attrs = base.to_vec();
        attrs[2] = context.lod.as_ref().map_or(FieldValue::Null, |s| FieldValue::Text(s.clone()));
        attrs[5] = FieldValue::Text(context.geometry_type.unwrap_or(&node.name).into());
        attrs[7] = context.semantic.as_ref().map_or(FieldValue::Null, |s| FieldValue::Text(s.to_string()));
        let geometry = self.normalize(primitive(node, context.dim)?, context)?;
        // A solid's XLink and the thematic boundary's inline polygon denote one face.
        if let Some(id) = node.id().filter(|_| context.matrix.is_none()) {
            let key = (
                match &base[0] {
                    FieldValue::Text(s) => s.clone(),
                    _ => String::new(),
                },
                id.to_owned(),
                context.lod.clone(),
                context.reverse,
            );
            if let Some(index) = self.emitted.get(&key) {
                if attrs[7] != FieldValue::Null {
                    let length = |v: &FieldValue| match v {
                        FieldValue::Text(s) => s.len(),
                        _ => 0,
                    };
                    let old = length(&self.layer.features[*index].attributes[7]);
                    self.used = self.used.saturating_sub(old).saturating_add(length(&attrs[7]));
                    if self.used > 256 * 1024 * 1024 {
                        return Err(invalid("flat feature payload exceeds the 256 MiB expansion limit"));
                    }
                    self.layer.features[*index].attributes[7] = attrs[7].clone();
                }
                return Ok(());
            }
            self.emitted.insert(key, self.layer.features.len());
        }
        super::city::push(&mut self.layer, Some(geometry), &attrs, &mut self.used)?;
        Ok(())
    }

    fn walk(&mut self, node: &'a Node, mut context: Context<'a>, base: &[FieldValue], depth: usize) -> Result<()> {
        self.visits += 1;
        if self.visits > 1_000_000 {
            return Err(invalid("XLink expansion exceeds the 1,000,000 node-visit limit"));
        }
        if depth > 256 {
            return Err(invalid("XLink nesting or cycle limit exceeded"));
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
            let digits: String = node.name[3..].chars().take_while(char::is_ascii_digit).collect();
            if !digits.is_empty() {
                context.lod = Some(digits);
                context.geometry_type = None;
            }
        }
        if node.city() && node.name.ends_with("Surface") && !node.name.starts_with("lod") {
            let mut props = attributes(node).as_object().cloned().unwrap_or_default();
            props.insert("type".into(), Value::String(node.name.clone()));
            context.semantic = Some(Value::Object(props));
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
            let result = self.walk(resolved, context, base, depth + 1);
            self.active_links.remove(id);
            return result;
        }
        if is_object(node) {
            return self.object(node, Some(&base[0]), context);
        }
        if node.city() && node.name == "ImplicitGeometry" {
            let relative = node
                .child("relativeGMLGeometry")
                .or_else(|| node.child("relativeGeometry"))
                .ok_or_else(|| invalid("implicit geometry has no relative geometry"))?;
            let reference = node.child("referencePoint").ok_or_else(|| invalid("implicit geometry has no reference point"))?;
            let point = self.link(reference)?;
            let coords = coordinates(point, context.dim)?;
            if coords.len() != 1 {
                return Err(invalid("implicit referencePoint must have one position"));
            }
            let anchor = [coords[0].x, coords[0].y, coords[0].z.unwrap_or(0.0)];
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

    fn object(&mut self, node: &'a Node, parent: Option<&FieldValue>, mut context: Context<'a>) -> Result<()> {
        if let Some(id) = node.id() {
            if self.active.contains(id) {
                return Err(invalid("cyclic city-object hierarchy"));
            }
            if let Some(rows) = self.objects.get(id).cloned() {
                if let Some(FieldValue::Text(parent)) = parent {
                    for row in rows {
                        let previous = &self.layer.features[row].attributes[3];
                        let (mut parents, old_len) = match previous {
                            FieldValue::Text(text) => (serde_json::from_str::<Vec<String>>(text).map_err(invalid)?, text.len()),
                            _ => (Vec::new(), 0),
                        };
                        if !parents.contains(parent) {
                            parents.push(parent.clone());
                            let text = serde_json::to_string(&parents).map_err(invalid)?;
                            self.used = self.used.saturating_sub(old_len).saturating_add(text.len());
                            if self.used > 256 * 1024 * 1024 {
                                return Err(invalid("flat feature payload exceeds the 256 MiB expansion limit"));
                            }
                            self.layer.features[row].attributes[3] = FieldValue::Text(text);
                        }
                    }
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
        context.dim = dimension(node, context.dim)?;
        let envelope = node.child("boundedBy").and_then(|n| n.child("Envelope"));
        context.crs = node.attr("", "srsName").or_else(|| envelope.and_then(|n| n.attr("", "srsName"))).or(context.crs);
        if let Some(envelope) = envelope {
            context.dim = dimension(envelope, context.dim)?;
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
            self.walk(child, context.clone(), &attrs, 0)?;
        }
        // Preserve parents/nonspatial objects even when only child objects have geometry.
        if !self.layer.features[start..].iter().any(|f| f.attributes[0] == attrs[0]) {
            super::city::push(&mut self.layer, None, &attrs, &mut self.used)?;
        }
        if let Some(id) = node.id() {
            self.active.remove(id);
            let rows = (start..self.layer.features.len()).filter(|&i| self.layer.features[i].attributes[0] == attrs[0]).collect();
            self.objects.insert(id, rows);
        }
        Ok(())
    }
}

/// Parse a CityGML 2.0 or 3.0 city model into an ordinary flat vector layer.
pub fn parse(bytes: &[u8]) -> Result<Vec<Layer>> {
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
        used: 0,
        next_object: 0,
        unknown_crs: false,
        active: HashSet::new(),
        active_links: HashSet::new(),
        objects: HashMap::new(),
        visits: 0,
        emitted: HashMap::new(),
    };
    let envelope = root.child("boundedBy").and_then(|n| n.child("Envelope"));
    let dim = envelope.map(|n| dimension(n, 3)).transpose()?.unwrap_or(3);
    let crs = root.attr("", "srsName").or_else(|| envelope.and_then(|n| n.attr("", "srsName")));
    let context = Context { dim, crs, ..Default::default() };
    for member in root.children.iter().filter(|n| n.city() && n.name == "cityObjectMember") {
        let resolved = reader.link(member)?;
        if std::ptr::eq(member, resolved) {
            for node in &member.children {
                if !node.city() {
                    return Err(Error::Unsupported(format!("CityGML city object namespace {}", node.ns)));
                }
                reader.object(node, None, context.clone())?;
            }
        } else {
            reader.object(resolved, None, context.clone())?;
        }
    }
    if reader.crs.is_none()
        && let Some(name) = crs
    {
        parse_crs(name)?.apply_to_layer(&mut reader.layer);
    }
    Ok(vec![reader.layer])
}

/// Whether the root element declares a supported CityGML model, including a partial header.
pub(crate) fn looks_like(bytes: &[u8]) -> bool {
    let text = String::from_utf8_lossy(bytes);
    let mut reader = NsReader::from_str(text.trim_start_matches('\u{feff}').trim_start());
    loop {
        match reader.read_resolved_event() {
            Ok((ResolveResult::Bound(ns), Event::Start(e) | Event::Empty(e))) => {
                return e.local_name().as_ref() == "CityModel"
                    && matches!(ns.as_ref(), "http://www.opengis.net/citygml/2.0" | "http://www.opengis.net/citygml/3.0");
            }
            Ok((_, Event::Start(_) | Event::Empty(_) | Event::Eof)) | Err(_) => return false,
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "../../../tests/support/citygml.rs"]
mod test_data;

#[cfg(test)]
mod tests {
    use super::test_data::{FACE, model};

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

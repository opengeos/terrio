// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Bounded local GML application-schema resolution; behavior reference GDAL 3.13.3.
//! External application definitions are never fetched.

use super::super::xml::Element;
use crate::{Error, Result};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use wbvector::{FieldType, GeometryType};

const XSD: &str = "http://www.w3.org/2001/XMLSchema";
const MAX_CHAIN: usize = 256;
const MAX_PROPERTIES: usize = 1_000_000;

pub(crate) fn is_gml(namespace: Option<&str>) -> bool {
    matches!(namespace, Some("http://www.opengis.net/gml" | "http://www.opengis.net/gml/3.2"))
}

fn unsupported(message: impl std::fmt::Display) -> Error {
    Error::Unsupported(format!("GML schema: {message}"))
}
fn invalid(message: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("GML schema: {message}"))
}
fn limited(message: impl std::fmt::Display) -> Error {
    Error::ResourceLimit(format!("GML schema: {message}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Whitespace {
    Preserve,
    Replace,
    Collapse,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Scalar {
    pub field_type: FieldType,
    pub whitespace: Whitespace,
}
impl Scalar {
    pub fn inferred(field_type: FieldType) -> Self {
        Self { field_type, whitespace: if field_type == FieldType::Text { Whitespace::Preserve } else { Whitespace::Collapse } }
    }
    pub fn normalize<'a>(&self, value: &'a str) -> Cow<'a, str> {
        match self.whitespace {
            Whitespace::Preserve => Cow::Borrowed(value),
            Whitespace::Replace if !value.contains(['\t', '\n', '\r']) => Cow::Borrowed(value),
            Whitespace::Replace => Cow::Owned(value.replace(['\t', '\n', '\r'], " ")),
            Whitespace::Collapse => Cow::Owned(value.split_ascii_whitespace().collect::<Vec<_>>().join(" ")),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropertyKind {
    Scalar(Scalar),
    Geometry(Option<GeometryType>),
    Metadata,
}
#[derive(Debug, Clone)]
pub(crate) struct Property<'a> {
    pub name: &'a str,
    pub namespace: Option<&'a str>,
    pub kind: PropertyKind,
}

pub(crate) struct ApplicationSchema<'a> {
    pub feature: String,
    pub namespace: Option<&'a str>,
    pub properties: Vec<Property<'a>>,
    index: HashMap<&'a str, usize>,
    gml_bases: HashSet<&'a str>,
}
impl<'a> ApplicationSchema<'a> {
    pub fn compile(root: &'a Element, feature: &str) -> Result<Self> {
        let mut resolver = Resolver::new(root)?;
        let declaration = *resolver.elements.get(feature).ok_or_else(|| unsupported(format!("no declaration for feature {feature}")))?;
        let mut model = Self {
            feature: feature.into(),
            namespace: resolver.target,
            properties: Vec::new(),
            index: HashMap::new(),
            gml_bases: HashSet::new(),
        };
        let path = vec![root, declaration];
        let complex = if let Some(kind) = declaration.attr("type") {
            let (namespace, name) = resolve(kind, &path)?;
            resolver.local(namespace)?;
            *resolver.complex.get(name).ok_or_else(|| unsupported(format!("unresolved feature type {kind}")))?
        } else {
            declaration.child("complexType").ok_or_else(|| unsupported("feature has no complex type"))?
        };
        let path = if declaration.child("complexType").is_some_and(|node| std::ptr::eq(node, complex)) {
            vec![root, declaration, complex]
        } else {
            vec![root, complex]
        };
        resolver.complex_fields(complex, &path, &mut Vec::new(), &mut model)?;
        Ok(model)
    }
    fn add(&mut self, property: Property<'a>) -> Result<()> {
        if property.name == "gml_id" {
            return Err(unsupported("application property gml_id conflicts with the source identifier"));
        }
        if let Some(index) = self.index.get(property.name) {
            let previous = &self.properties[*index];
            if previous.namespace == property.namespace && previous.kind == property.kind {
                return Ok(());
            }
            return Err(unsupported(format!("ambiguous property name {}", property.name)));
        }
        self.properties.try_reserve(1).map_err(|_| limited("cannot reserve property declarations"))?;
        self.index.try_reserve(1).map_err(|_| limited("cannot index property declarations"))?;
        self.index.insert(property.name, self.properties.len());
        self.properties.push(property);
        Ok(())
    }
    pub fn kind(&self, name: &str, namespace: Option<&str>) -> Result<PropertyKind> {
        if namespace.is_some_and(|ns| self.gml_bases.contains(ns)) {
            if matches!(name, "boundedBy" | "name" | "description" | "identifier" | "metaDataProperty") {
                return Ok(PropertyKind::Metadata);
            }
            if name == "location" {
                return Ok(PropertyKind::Geometry(None));
            }
        }
        let property =
            self.index.get(name).map(|index| &self.properties[*index]).ok_or_else(|| invalid(format!("undeclared property {name}")))?;
        if property.namespace != namespace {
            return Err(invalid(format!("property namespace differs for {name}")));
        }
        Ok(property.kind)
    }
    pub fn scalars(&self) -> impl Iterator<Item = (&str, Scalar)> {
        self.properties.iter().filter_map(|property| match property.kind {
            PropertyKind::Scalar(kind) => Some((property.name, kind)),
            _ => None,
        })
    }
    pub fn geometry_hint(&self) -> Option<GeometryType> {
        self.properties
            .iter()
            .find_map(|property| match property.kind {
                PropertyKind::Geometry(kind) => Some(kind),
                _ => None,
            })
            .flatten()
    }
}

fn resolve<'a>(value: &'a str, path: &[&'a Element]) -> Result<(Option<&'a str>, &'a str)> {
    let value = value.trim();
    let (prefix, name) = value.split_once(':').unwrap_or(("", value));
    if name.is_empty() || name.contains(':') || (value.contains(':') && prefix.is_empty()) {
        return Err(invalid("invalid QName"));
    }
    let namespace = path.iter().rev().find_map(|node| node.namespace(prefix)).filter(|namespace| !namespace.is_empty());
    if !prefix.is_empty() && namespace.is_none() {
        return Err(invalid(format!("unbound type prefix {prefix}")));
    }
    Ok((namespace, name))
}
fn enter<'a>(node: &'a Element, active: &mut Vec<&'a Element>) -> Result<()> {
    if active.iter().any(|previous| std::ptr::eq(*previous, node)) {
        return Err(invalid("cyclic type or element derivation"));
    }
    if active.len() >= MAX_CHAIN {
        return Err(limited("type derivation exceeds 256 levels"));
    }
    active.push(node);
    Ok(())
}
fn geometry_kind(name: &str) -> Option<PropertyKind> {
    use GeometryType as G;
    let hint = match name {
        "GeometryPropertyType" | "GeometryArrayPropertyType" | "MultiGeometryPropertyType" => None,
        "PointPropertyType" => Some(G::Point),
        "LineStringPropertyType" | "CurvePropertyType" | "LinearRingPropertyType" => Some(G::LineString),
        "PolygonPropertyType" | "SurfacePropertyType" => Some(G::Polygon),
        "MultiPointPropertyType" => Some(G::MultiPoint),
        "MultiLineStringPropertyType" | "MultiCurvePropertyType" => Some(G::MultiLineString),
        "MultiPolygonPropertyType" | "MultiSurfacePropertyType" => Some(G::MultiPolygon),
        _ => return None,
    };
    Some(PropertyKind::Geometry(hint))
}
fn builtin(name: &str) -> Result<Scalar> {
    let (field_type, whitespace) = match name {
        "string" => (FieldType::Text, Whitespace::Preserve),
        "normalizedString" => (FieldType::Text, Whitespace::Replace),
        "integer" | "int" | "long" | "short" | "byte" | "nonNegativeInteger" | "positiveInteger" | "negativeInteger"
        | "nonPositiveInteger" | "unsignedInt" | "unsignedShort" | "unsignedByte" | "unsignedLong" => {
            (FieldType::Integer, Whitespace::Collapse)
        }
        "decimal" | "double" | "float" => (FieldType::Float, Whitespace::Collapse),
        "boolean" => (FieldType::Boolean, Whitespace::Collapse),
        "date" => (FieldType::Date, Whitespace::Collapse),
        "dateTime" | "dateTimeStamp" => (FieldType::DateTime, Whitespace::Collapse),
        "token" | "language" | "Name" | "NCName" | "QName" | "anyURI" | "ID" | "IDREF" | "ENTITY" | "NMTOKEN" | "NMTOKENS" | "IDREFS"
        | "ENTITIES" | "base64Binary" | "hexBinary" | "duration" | "dayTimeDuration" | "yearMonthDuration" | "time" | "gYear"
        | "gMonth" | "gDay" | "gYearMonth" | "gMonthDay" | "NOTATION" | "anySimpleType" | "anyAtomicType" => {
            (FieldType::Text, Whitespace::Collapse)
        }
        _ => return Err(unsupported(format!("XML Schema type {name}"))),
    };
    Ok(Scalar { field_type, whitespace })
}

struct Resolver<'a> {
    root: &'a Element,
    target: Option<&'a str>,
    simple: HashMap<&'a str, &'a Element>,
    complex: HashMap<&'a str, &'a Element>,
    elements: HashMap<&'a str, &'a Element>,
    kinds: HashMap<usize, PropertyKind>,
    visits: usize,
}
impl<'a> Resolver<'a> {
    fn new(root: &'a Element) -> Result<Self> {
        let mut result = Self {
            root,
            target: root.attr("targetNamespace").map(str::trim).filter(|value| !value.is_empty()),
            simple: HashMap::new(),
            complex: HashMap::new(),
            elements: HashMap::new(),
            kinds: HashMap::new(),
            visits: 0,
        };
        for node in &root.children {
            let Some(name) = node.attr("name").map(str::trim) else { continue };
            let map = match node.name.as_str() {
                "simpleType" => &mut result.simple,
                "complexType" => &mut result.complex,
                "element" => &mut result.elements,
                _ => continue,
            };
            map.try_reserve(1).map_err(|_| limited("cannot index schema declarations"))?;
            if map.insert(name, node).is_some() {
                return Err(invalid(format!("duplicate declaration {name}")));
            }
        }
        Ok(result)
    }
    fn local(&self, namespace: Option<&str>) -> Result<()> {
        if namespace != self.target {
            return Err(unsupported("external application-schema types are unsupported"));
        }
        Ok(())
    }
    fn kind(&mut self, value: &'a str, path: &[&'a Element], active: &mut Vec<&'a Element>) -> Result<PropertyKind> {
        let (namespace, name) = resolve(value, path)?;
        if namespace == Some(XSD) {
            return Ok(PropertyKind::Scalar(builtin(name)?));
        }
        if is_gml(namespace) {
            if let Some(kind) = geometry_kind(name) {
                return Ok(kind);
            }
            return match name {
                "CodeType" | "CodeWithAuthorityType" | "StringOrRefType" | "ReferenceType" => {
                    Ok(PropertyKind::Scalar(Scalar { field_type: FieldType::Text, whitespace: Whitespace::Preserve }))
                }
                _ => Err(unsupported(format!("GML property type {name}"))),
            };
        }
        self.local(namespace)?;
        let node = self
            .simple
            .get(name)
            .or_else(|| self.complex.get(name))
            .copied()
            .ok_or_else(|| unsupported(format!("unresolved application type {name}")))?;
        let key = std::ptr::from_ref(node) as usize;
        if let Some(kind) = self.kinds.get(&key) {
            return Ok(*kind);
        }
        enter(node, active)?;
        let path = vec![self.root, node];
        let kind = self.inline_kind(node, &path, active);
        active.pop();
        let kind = kind?;
        self.kinds.try_reserve(1).map_err(|_| limited("cannot cache resolved types"))?;
        self.kinds.insert(key, kind);
        Ok(kind)
    }
    fn inline_kind(&mut self, node: &'a Element, path: &[&'a Element], active: &mut Vec<&'a Element>) -> Result<PropertyKind> {
        if matches!(node.name.as_str(), "list" | "union") {
            return Err(unsupported("list and union properties are unsupported"));
        }
        if matches!(node.name.as_str(), "restriction" | "extension") {
            let base = node.attr("base").ok_or_else(|| invalid("type derivation has no base"))?;
            let mut kind = self.kind(base, path, active)?;
            if node.children.iter().any(|child| matches!(child.name.as_str(), "sequence" | "choice" | "all" | "element" | "group")) {
                return Err(unsupported("complex feature properties are unsupported"));
            }
            if let Some(facet) = node.child("whiteSpace")
                && let PropertyKind::Scalar(scalar) = &mut kind
            {
                scalar.whitespace = match facet.attr("value") {
                    Some("preserve") => Whitespace::Preserve,
                    Some("replace") => Whitespace::Replace,
                    Some("collapse") => Whitespace::Collapse,
                    _ => return Err(invalid("invalid whiteSpace facet")),
                };
            }
            return Ok(kind);
        }
        for child in &node.children {
            if matches!(
                child.name.as_str(),
                "restriction" | "extension" | "simpleType" | "simpleContent" | "complexContent" | "list" | "union"
            ) {
                let mut path = path.to_vec();
                path.push(child);
                return self.inline_kind(child, &path, active);
            }
        }
        Err(unsupported("unresolved or complex property type"))
    }
    fn complex_fields(
        &mut self,
        node: &'a Element,
        path: &[&'a Element],
        active: &mut Vec<&'a Element>,
        model: &mut ApplicationSchema<'a>,
    ) -> Result<()> {
        enter(node, active)?;
        let result = self.collect(node, path, active, model);
        active.pop();
        result
    }
    fn collect(
        &mut self,
        node: &'a Element,
        path: &[&'a Element],
        active: &mut Vec<&'a Element>,
        model: &mut ApplicationSchema<'a>,
    ) -> Result<()> {
        if node.name == "element" {
            return self.property(node, path, false, active, model);
        }
        if node.name == "group" && node.attr("ref").is_some() {
            return Err(unsupported("referenced schema groups are unsupported"));
        }
        if node.name == "simpleContent" || node.name == "restriction" {
            return Err(unsupported("restricted or simple-content feature types are unsupported"));
        }
        if node.name == "extension" {
            let base = node.attr("base").ok_or_else(|| invalid("extension has no base"))?;
            let (namespace, name) = resolve(base, path)?;
            if is_gml(namespace) {
                if !matches!(name, "AbstractFeatureType" | "AbstractGMLType" | "AbstractFeatureCollectionType" | "FeatureCollectionType") {
                    return Err(unsupported(format!("GML feature base {name}")));
                }
                model.gml_bases.try_reserve(1).map_err(|_| limited("cannot retain GML base namespaces"))?;
                model.gml_bases.insert(namespace.unwrap());
            } else {
                self.local(namespace)?;
                let base = *self.complex.get(name).ok_or_else(|| unsupported(format!("unresolved extension base {name}")))?;
                self.complex_fields(base, &[self.root, base], active, model)?;
            }
        }
        for child in &node.children {
            let mut path = path.to_vec();
            path.push(child);
            self.collect(child, &path, active, model)?;
        }
        Ok(())
    }
    fn property(
        &mut self,
        node: &'a Element,
        path: &[&'a Element],
        global: bool,
        active: &mut Vec<&'a Element>,
        model: &mut ApplicationSchema<'a>,
    ) -> Result<()> {
        self.visits += 1;
        if self.visits > MAX_PROPERTIES {
            return Err(limited("schema expansion exceeds one million property visits"));
        }
        if let Some(reference) = node.attr("ref") {
            let (namespace, name) = resolve(reference, path)?;
            if is_gml(namespace) && matches!(name, "name" | "description" | "identifier" | "boundedBy" | "metaDataProperty" | "location") {
                return model.add(Property {
                    name,
                    namespace,
                    kind: if name == "location" { PropertyKind::Geometry(None) } else { PropertyKind::Metadata },
                });
            }
            self.local(namespace)?;
            let referenced = *self.elements.get(name).ok_or_else(|| unsupported(format!("unresolved element reference {name}")))?;
            enter(referenced, active)?;
            let result = self.property(referenced, &[self.root, referenced], true, active, model);
            active.pop();
            return result;
        }
        let name = node.attr("name").map(str::trim).ok_or_else(|| invalid("property has no name"))?;
        let namespace = if global
            || node.attr("form").unwrap_or_else(|| self.root.attr("elementFormDefault").unwrap_or("unqualified")) == "qualified"
        {
            self.target
        } else {
            None
        };
        let kind = if let Some(kind) = node.attr("type") {
            self.kind(kind, path, active)?
        } else {
            let inline = node
                .child("simpleType")
                .or_else(|| node.child("complexType"))
                .ok_or_else(|| unsupported(format!("property {name} has no type")))?;
            let mut path = path.to_vec();
            path.push(inline);
            self.inline_kind(inline, &path, active)?
        };
        model.add(Property { name, namespace, kind })
    }
}

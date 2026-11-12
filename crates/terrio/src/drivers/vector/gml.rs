// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/gml/gmlreader.cpp
//     ogr/ogrsf_frmts/gml/ogrgmldatasource.cpp
// No code is translated from these sources.

//! GML simple features, read and written as GDAL does.
//!
//! Reading makes one layer per feature type, with a `gml_id` field, field types from the
//! application schema (`.xsd` next to the file) or inferred from the values, geometries in GML 2
//! or 3 encodings, and the CRS from `srsName`. Authority-order EPSG names (URNs or modern URLs)
//! with north/east axes are normalized to GIS XY, including northing-first projected CRSs.
//!
//! Writing produces GML 3.2 with an `.xsd` schema, like GDAL's default output.

use super::xml::{self, Element};
#[path = "gml_schema.rs"]
mod schema;
use crate::crs::SpatialRef;
use crate::{Error, Result};
pub(crate) use schema::{ApplicationSchema, PropertyKind};
use schema::{Scalar, is_gml};
use std::fmt::Write as _;
use std::path::Path;
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer, Ring};

pub(crate) const GEOMETRIES: &[&str] = &[
    "Point",
    "LineString",
    "LinearRing",
    "Curve",
    "Polygon",
    "Surface",
    "MultiPoint",
    "MultiLineString",
    "MultiCurve",
    "MultiPolygon",
    "MultiSurface",
    "MultiGeometry",
    "GeometryCollection",
    "CompositeCurve",
    "CompositeSurface",
    "OrientableSurface",
];

/// Reads a GML file: one layer per feature type.
pub fn read(path: &str) -> Result<Vec<Layer>> {
    parse_path_bytes(&std::fs::read(path)?, path)
}

/// Parse already-read GML bytes while retaining the native sibling schema lookup.
pub(crate) fn parse_path_bytes(bytes: &[u8], path: &str) -> Result<Vec<Layer>> {
    let text = xml::decode(bytes);
    let root = xml::parse(&text)?;
    let xsd = std::fs::read(Path::new(path).with_extension("xsd")).ok().and_then(|b| xml::parse(&xml::decode(&b)).ok());
    parse(&root, xsd.as_ref())
}

/// A feature and the namespace scopes on its collection/member path.
#[derive(Clone, Copy)]
pub(crate) struct FeatureRef<'a> {
    pub element: &'a Element,
    pub parent: &'a Element,
    pub root: &'a Element,
}
impl<'a> FeatureRef<'a> {
    pub fn namespace(&self, node: &'a Element, prefix: &str) -> Option<&'a str> {
        node.namespace(prefix)
            .or_else(|| self.element.namespace(prefix))
            .or_else(|| self.parent.namespace(prefix))
            .or_else(|| self.root.namespace(prefix))
            .filter(|namespace| !namespace.is_empty())
    }
    fn id(&self) -> Option<&'a str> {
        self.element
            .attrs
            .iter()
            .find_map(|(key, value)| {
                let (prefix, local) = key.split_once(':')?;
                (local == "id" && is_gml(self.namespace(self.element, prefix))).then_some(value.as_str())
            })
            .or_else(|| {
                self.element
                    .attrs
                    .iter()
                    .find(|(key, _)| key == "fid")
                    .or_else(|| self.element.attrs.iter().find(|(key, _)| key == "id"))
                    .map(|(_, value)| value.as_str())
            })
    }
    pub fn property_kind(&self, property: &'a Element, model: &ApplicationSchema<'_>) -> Result<PropertyKind> {
        model.kind(&property.name, self.namespace(property, &property.prefix))
    }
}

fn feature_refs(root: &Element) -> Vec<FeatureRef<'_>> {
    let mut out = Vec::new();
    for parent in &root.children {
        let children = match parent.name.as_str() {
            "featureMember" | "member" => &parent.children[..parent.children.len().min(1)],
            "featureMembers" => &parent.children[..],
            _ => continue,
        };
        out.extend(children.iter().map(|element| FeatureRef { element, parent, root }));
    }
    if out.is_empty() && !root.name.ends_with("FeatureCollection") && root.children.iter().any(|c| geometry_child(c).is_some()) {
        out.push(FeatureRef { element: root, parent: root, root });
    }
    out
}

/// The geometry element inside a property element.
fn geometry_child(prop: &Element) -> Option<&Element> {
    prop.children.iter().find(|c| GEOMETRIES.contains(&c.name.as_str()))
}

/// Parses a GML document into layers.
pub fn parse(root: &Element, xsd: Option<&Element>) -> Result<Vec<Layer>> {
    parse_impl(root, xsd, None)
}

pub(crate) fn parse_with_schema(root: &Element, schema: &ApplicationSchema<'_>) -> Result<Vec<Layer>> {
    parse_impl(root, None, Some(schema))
}

fn parse_impl(root: &Element, xsd: Option<&Element>, compiled: Option<&ApplicationSchema<'_>>) -> Result<Vec<Layer>> {
    struct Raw<'a> {
        id: Option<&'a str>,
        geometry: Option<&'a Element>,
        values: Vec<(&'a str, &'a str)>,
    }
    let mut groups: Vec<(String, Vec<FeatureRef<'_>>)> = Vec::new();
    for feature in feature_refs(root) {
        match groups.iter_mut().find(|(name, _)| *name == feature.element.name) {
            Some((_, rows)) => rows.push(feature),
            None => groups.push((feature.element.name.clone(), vec![feature])),
        }
    }
    let mut layers = Vec::new();
    for (name, features) in groups {
        // Local GML has historically inferred fields when its companion schema
        // depends on unavailable imports. A WFS read supplies a strict compiled model.
        let owned = match xsd.map(|schema| ApplicationSchema::compile(schema, &name)).transpose() {
            Err(error @ Error::ResourceLimit(_)) => return Err(error),
            Err(_) => None,
            Ok(model) => model,
        };
        let model = compiled.or(owned.as_ref());
        let mut raws = Vec::new();
        for feature in features {
            if let Some(model) = compiled
                && feature.namespace(feature.element, &feature.element.prefix) != model.namespace
            {
                return Err(Error::InvalidData("GML: feature namespace differs from application schema".into()));
            }
            let mut raw = Raw { id: feature.id(), geometry: None, values: Vec::new() };
            for property in &feature.element.children {
                if property.name == "gml_id" {
                    return Err(Error::Unsupported("GML: application property gml_id conflicts with the source identifier".into()));
                }
                if let Some(model) = compiled {
                    match feature.property_kind(property, model)? {
                        PropertyKind::Metadata => continue,
                        PropertyKind::Scalar(_) => {
                            if !property.children.is_empty() {
                                return Err(Error::Unsupported("GML: complex scalar properties are unsupported".into()));
                            }
                            raw.values.push((&property.name, &property.text));
                        }
                        PropertyKind::Geometry(_) => {
                            if raw.geometry.is_none() {
                                raw.geometry = geometry_child(property);
                            }
                        }
                    }
                } else if property.name != "boundedBy" {
                    match geometry_child(property) {
                        Some(geometry) if raw.geometry.is_none() => raw.geometry = Some(geometry),
                        None if property.children.is_empty() => raw.values.push((&property.name, property.text.trim())),
                        _ => {}
                    }
                }
            }
            raws.push(raw);
        }
        let mut layer = Layer::new(&name);
        layer.add_field(FieldDef::new("gml_id", FieldType::Text));
        let mut fields: Vec<(String, Scalar)> =
            model.map(|model| model.scalars().map(|(name, scalar)| (name.into(), scalar)).collect()).unwrap_or_default();
        if compiled.is_none() && fields.is_empty() {
            let mut names = Vec::new();
            for raw in &raws {
                for (name, _) in &raw.values {
                    if !names.contains(name) {
                        names.push(*name);
                    }
                }
            }
            for name in names {
                let values = raws.iter().flat_map(|raw| raw.values.iter().filter(|(key, _)| *key == name).map(|(_, value)| *value));
                fields.push((name.into(), Scalar::inferred(super::csv::infer_type(values))));
            }
        }
        for (name, scalar) in &fields {
            layer.add_field(FieldDef::new(name, scalar.field_type));
        }
        let srs_name = raws
            .iter()
            .filter_map(|raw| raw.geometry)
            .find_map(srs_name_of)
            .or_else(|| root.child("boundedBy").and_then(|node| node.find("Envelope")).and_then(|node| node.attr("srsName")));
        let srs = match srs_name {
            // A partially recognized compound reference would attach the wrong CRS to
            // three-dimensional coordinates. Refuse the entire declaration instead.
            Some(name)
                if {
                    let upper = name.trim().to_ascii_uppercase();
                    upper.starts_with("URN:OGC:DEF:CRS,") || (upper.starts_with("EPSG:") && upper.contains('+'))
                } =>
            {
                Some(parse_srs_name(name)?)
            }
            _ => srs_name.and_then(|srs| parse_srs_name(srs).ok()),
        };
        let swap = srs_name.is_some_and(authority_axis_order) && srs.as_ref().is_some_and(SpatialRef::has_north_east_axis_order);
        let dimension = if srs.as_ref().is_some_and(|srs| srs.compound_parts().is_some()) { 3 } else { 2 };
        if let Some(srs) = &srs {
            srs.apply_to_layer(&mut layer);
        }
        // One layer uses one coordinate convention. Refuse conflicting declarations,
        // including nested geometry members, before applying its shared XY normalization.
        for raw in &raws {
            if let Some(node) = raw.geometry {
                check_geometry_reference(node, srs_name, srs.as_ref(), swap)?;
                if dimension == 3 {
                    validate_geometry_dimensions(node, dimension)?;
                }
            }
        }
        for (fid, raw) in raws.iter().enumerate() {
            let mut attributes = vec![FieldValue::Null; fields.len() + 1];
            attributes[0] = raw.id.map_or(FieldValue::Null, |id| FieldValue::Text(id.into()));
            for (key, text) in &raw.values {
                if let Some(index) = layer.schema.field_index(key) {
                    let scalar = fields[index - 1].1;
                    let text = scalar.normalize(text);
                    let parsed = if text.is_empty() { FieldValue::Null } else { value(&text, scalar.field_type) };
                    if compiled.is_some()
                        && !text.is_empty()
                        && ((matches!(scalar.field_type, FieldType::Integer | FieldType::Float) && matches!(&parsed, FieldValue::Text(_)))
                            || scalar.field_type == FieldType::Boolean && !matches!(text.as_ref(), "true" | "false" | "1" | "0"))
                    {
                        return Err(Error::InvalidData(format!("WFS: malformed value for field {key}")));
                    }
                    attributes[index] = parsed;
                }
            }
            let geometry = raw
                .geometry
                .and_then(|geometry_node| geometry(geometry_node, dimension))
                .map(|geometry| if swap { swap_xy(geometry) } else { geometry });
            layer.push(Feature { fid: fid as u64, geometry, attributes });
        }
        layers.push(layer);
    }
    if layers.is_empty() {
        return Err(Error::InvalidData("GML: no features".into()));
    }
    Ok(layers)
}

fn validate_geometry_dimensions(node: &Element, dimension: usize) -> Result<()> {
    let dimension = node.attr("srsDimension").and_then(|value| value.parse().ok()).unwrap_or(dimension);
    match node.name.as_str() {
        "MultiPoint" | "MultiLineString" | "MultiCurve" | "MultiPolygon" | "MultiSurface" | "CompositeSurface" | "MultiGeometry"
        | "GeometryCollection" => {
            for member in &node.children {
                if member.name.ends_with("Members") {
                    for child in &member.children {
                        validate_geometry_dimensions(child, dimension)?;
                    }
                } else if member.name.ends_with("Member")
                    && let Some(child) = member.children.first()
                {
                    validate_geometry_dimensions(child, dimension)?;
                }
            }
            Ok(())
        }
        "OrientableSurface" => {
            if let Some(surface) = node.find("Polygon").or_else(|| node.find("Surface")) {
                validate_geometry_dimensions(surface, dimension)?;
            }
            Ok(())
        }
        "Polygon" | "Surface" => {
            let patch = node.find("PolygonPatch").unwrap_or(node);
            for boundary in patch
                .children
                .iter()
                .filter(|child| matches!(child.name.as_str(), "exterior" | "outerBoundaryIs" | "interior" | "innerBoundaryIs"))
            {
                validate_coordinate_dimensions(boundary, dimension)?;
            }
            Ok(())
        }
        "Point" | "LineString" | "Curve" | "LinearRing" | "CompositeCurve" => validate_coordinate_dimensions(node, dimension),
        _ => Ok(()),
    }
}

fn validate_coordinate_dimensions(node: &Element, dimension: usize) -> Result<()> {
    let dimension = node.attr("srsDimension").and_then(|value| value.parse().ok()).unwrap_or(dimension);
    if matches!(node.name.as_str(), "pos" | "posList") {
        let dimension = dimension.max(2);
        let mut count = 0usize;
        for value in node.text.split_ascii_whitespace() {
            value.parse::<f64>().map_err(|_| Error::InvalidData("GML: invalid coordinate value".into()))?;
            count += 1;
        }
        if !count.is_multiple_of(dimension) {
            return Err(Error::InvalidData(format!("GML: {count} coordinate values do not form complete {dimension}-dimensional tuples")));
        }
    } else if !matches!(node.name.as_str(), "coordinates" | "coord") {
        for child in &node.children {
            validate_coordinate_dimensions(child, dimension)?;
        }
    }
    Ok(())
}

fn check_geometry_reference(node: &Element, reference: Option<&str>, layer_srs: Option<&SpatialRef>, swap: bool) -> Result<()> {
    if let Some(name) = node.attr("srsName")
        && Some(name) != reference
    {
        let srs = parse_srs_name(name).map_err(|_| Error::Unsupported("GML: a differing geometry CRS cannot be resolved".into()))?;
        let layer_srs =
            layer_srs.ok_or_else(|| Error::Unsupported("GML: layer CRS is unresolved; cannot validate a differing geometry CRS".into()))?;
        if !layer_srs.same_as(&srs) {
            return Err(Error::Unsupported("GML: mixed coordinate reference systems within a layer".into()));
        }
        if (authority_axis_order(name) && srs.has_north_east_axis_order()) != swap {
            return Err(Error::Unsupported("GML: mixed coordinate axis conventions within a layer".into()));
        }
    }
    for child in &node.children {
        check_geometry_reference(child, reference, layer_srs, swap)?;
    }
    Ok(())
}

fn value(v: &str, t: FieldType) -> FieldValue {
    match t {
        FieldType::Integer => v.parse().map_or_else(|_| FieldValue::Text(v.into()), FieldValue::Integer),
        FieldType::Float => v.parse().map_or_else(|_| FieldValue::Text(v.into()), FieldValue::Float),
        FieldType::Boolean => FieldValue::Boolean(matches!(v, "true" | "1")),
        FieldType::Date => FieldValue::Date(v.into()),
        FieldType::DateTime => FieldValue::DateTime(v.replace('/', "-")),
        _ => FieldValue::Text(v.into()),
    }
}

/// The CRS declaration the single-type GML collection parser will use.
pub(crate) fn collection_srs_name<'a>(root: &'a Element, model: &ApplicationSchema<'_>) -> Result<Option<&'a str>> {
    for feature in feature_refs(root) {
        if feature.namespace(feature.element, &feature.element.prefix) != model.namespace {
            return Err(Error::InvalidData("WFS: GetFeature returned a different feature namespace".into()));
        }
        for property in &feature.element.children {
            if matches!(feature.property_kind(property, model)?, PropertyKind::Geometry(_))
                && let Some(srs) = geometry_child(property).and_then(srs_name_of)
            {
                return Ok(Some(srs));
            }
        }
    }
    Ok(root.child("boundedBy").and_then(|node| node.find("Envelope")).and_then(|node| node.attr("srsName")))
}

fn srs_name_of(g: &Element) -> Option<&str> {
    g.attr("srsName").or_else(|| g.children.iter().find_map(srs_name_of))
}

/// GML 2 also uses the pre-URN OGC EPSG dictionary URL (GDAL 3.13.3).
/// This identifier keeps traditional XY order and does not fetch the dictionary.
pub(crate) fn parse_srs_name(name: &str) -> Result<SpatialRef> {
    let name = name.trim();
    for prefix in ["http://www.opengis.net/gml/srs/epsg.xml#", "https://www.opengis.net/gml/srs/epsg.xml#"] {
        if let Some(code) = name.strip_prefix(prefix)
            && !code.is_empty()
            && code.bytes().all(|byte| byte.is_ascii_digit())
        {
            return SpatialRef::parse_literal(&format!("EPSG:{code}"));
        }
    }
    SpatialRef::parse_literal(name)
}

/// Whether a srsName form declares authority axis order: URNs and modern `/def/crs/` URLs,
/// but not short `EPSG:` names or the old `epsg.xml#` URLs. The CRS determines whether to swap.
pub(crate) fn authority_axis_order(srs_name: &str) -> bool {
    let s = srs_name.to_ascii_lowercase();
    (s.starts_with("urn:") || s.contains("/def/crs/")) && s.contains("epsg")
}

fn swap_xy(g: Geometry) -> Geometry {
    let sw = |c: Coord| Coord { x: c.y, y: c.x, ..c };
    let sv = |cs: Vec<Coord>| cs.into_iter().map(sw).collect::<Vec<_>>();
    let sr = |r: Ring| Ring::new(sv(r.0));
    match g {
        Geometry::Point(c) => Geometry::Point(sw(c)),
        Geometry::LineString(cs) => Geometry::LineString(sv(cs)),
        Geometry::MultiPoint(cs) => Geometry::MultiPoint(sv(cs)),
        Geometry::MultiLineString(ls) => Geometry::MultiLineString(ls.into_iter().map(sv).collect()),
        Geometry::Polygon { exterior, interiors } => {
            Geometry::Polygon { exterior: sr(exterior), interiors: interiors.into_iter().map(sr).collect() }
        }
        Geometry::MultiPolygon(ps) => {
            Geometry::MultiPolygon(ps.into_iter().map(|(e, hs)| (sr(e), hs.into_iter().map(sr).collect())).collect())
        }
        Geometry::GeometryCollection(gs) => Geometry::GeometryCollection(gs.into_iter().map(swap_xy).collect()),
    }
}

/// Parses a coordinate list: `pos`, `posList`, GML 2 `coordinates` and `coord` elements under an
/// element, in document order.
fn coords(e: &Element, dim: usize) -> Vec<Coord> {
    let dim = e.attr("srsDimension").and_then(|d| d.parse().ok()).unwrap_or(dim);
    let mut out = Vec::new();
    match e.name.as_str() {
        "pos" | "posList" => {
            let nums: Vec<f64> = e.text.split_ascii_whitespace().filter_map(|t| t.parse().ok()).collect();
            let dim = if dim < 2 { 2 } else { dim };
            for c in nums.chunks_exact(dim) {
                out.push(Coord { x: c[0], y: c[1], z: (dim > 2).then(|| c[2]), m: None });
            }
        }
        "coordinates" => {
            let cs = e.attr("cs").unwrap_or(",");
            let ts = e.attr("ts").unwrap_or(" ");
            let decimal = e.attr("decimal").unwrap_or(".");
            let tuples: Vec<&str> =
                if ts.trim().is_empty() { e.text.split_whitespace().collect() } else { e.text.split(ts).map(str::trim).collect() };
            for t in tuples.into_iter().filter(|t| !t.is_empty()) {
                let nums: Vec<f64> = t.split(cs).filter_map(|n| n.trim().replace(decimal, ".").parse().ok()).collect();
                if nums.len() >= 2 {
                    out.push(Coord { x: nums[0], y: nums[1], z: nums.get(2).copied(), m: None });
                }
            }
        }
        "coord" => {
            let n = |k: &str| e.child_text(k).and_then(|t| t.parse().ok());
            if let (Some(x), Some(y)) = (n("X"), n("Y")) {
                out.push(Coord { x, y, z: n("Z"), m: None });
            }
        }
        _ => {
            for c in &e.children {
                out.extend(coords(c, dim));
            }
        }
    }
    out
}

fn ring(e: &Element, dim: usize) -> Ring {
    let mut cs = coords(e, dim);
    if cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
        cs.pop();
    }
    Ring::new(cs)
}

fn polygon(e: &Element, dim: usize) -> Option<(Ring, Vec<Ring>)> {
    // A Surface holds PolygonPatch elements: use the first.
    let e = e.find("PolygonPatch").unwrap_or(e);
    let exterior = e.child("exterior").or_else(|| e.child("outerBoundaryIs")).map(|x| ring(x, dim))?;
    let interiors = e.children.iter().filter(|c| c.name == "interior" || c.name == "innerBoundaryIs").map(|x| ring(x, dim)).collect();
    Some((exterior, interiors))
}

/// The geometries held by the member properties of a multi-geometry.
fn members(e: &Element, dim: usize) -> Vec<Geometry> {
    let mut out = Vec::new();
    for m in &e.children {
        if m.name.ends_with("Members") {
            out.extend(m.children.iter().filter_map(|g| geometry(g, dim)));
        } else if m.name.ends_with("Member") {
            out.extend(m.children.first().and_then(|g| geometry(g, dim)));
        }
    }
    out
}

/// Parses a GML geometry element.
pub fn geometry(e: &Element, dim: usize) -> Option<Geometry> {
    let dim = e.attr("srsDimension").and_then(|d| d.parse().ok()).unwrap_or(dim);
    Some(match e.name.as_str() {
        "Point" => Geometry::Point(coords(e, dim).into_iter().next()?),
        "LineString" | "Curve" | "LinearRing" | "CompositeCurve" => Geometry::LineString(coords(e, dim)),
        "Polygon" | "Surface" => {
            let (exterior, interiors) = polygon(e, dim)?;
            Geometry::Polygon { exterior, interiors }
        }
        "OrientableSurface" => geometry(e.find("Polygon").or_else(|| e.find("Surface"))?, dim)?,
        "MultiPoint" => Geometry::MultiPoint(
            members(e, dim)
                .into_iter()
                .flat_map(|g| match g {
                    Geometry::Point(c) => vec![c],
                    Geometry::MultiPoint(cs) => cs,
                    _ => vec![],
                })
                .collect(),
        ),
        "MultiLineString" | "MultiCurve" => Geometry::MultiLineString(
            members(e, dim)
                .into_iter()
                .flat_map(|g| match g {
                    Geometry::LineString(cs) => vec![cs],
                    Geometry::MultiLineString(ls) => ls,
                    _ => vec![],
                })
                .collect(),
        ),
        "MultiPolygon" | "MultiSurface" | "CompositeSurface" => Geometry::MultiPolygon(
            members(e, dim)
                .into_iter()
                .flat_map(|g| match g {
                    Geometry::Polygon { exterior, interiors } => vec![(exterior, interiors)],
                    Geometry::MultiPolygon(ps) => ps,
                    _ => vec![],
                })
                .collect(),
        ),
        "MultiGeometry" | "GeometryCollection" => Geometry::GeometryCollection(members(e, dim)),
        _ => return None,
    })
}

/// Writes a layer as GML 3.2, with an `.xsd` application schema next to it.
pub fn write(layer: &Layer, path: &str) -> Result<()> {
    let p = Path::new(path);
    let schema_name = p.with_extension("xsd").file_name().and_then(|s| s.to_str()).unwrap_or("schema.xsd").to_string();
    std::fs::write(p.with_extension("xsd"), xsd(layer))?;
    std::fs::write(path, to_string(layer, &schema_name))?;
    Ok(())
}

/// A layer name usable as an XML element name.
fn element_name(name: &str) -> String {
    let mut s: String = name.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' { c } else { '_' }).collect();
    if !s.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        s.insert(0, '_');
    }
    s
}

/// Field names the writer uses: element names, without the `gml_id` the writer adds itself.
fn written_fields(layer: &Layer) -> Vec<(usize, String)> {
    layer.schema.fields().iter().enumerate().filter(|(_, f)| f.name != "gml_id").map(|(i, f)| (i, element_name(&f.name))).collect()
}

fn xsd(layer: &Layer) -> String {
    let name = element_name(&layer.name);
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema targetNamespace="http://ogr.maptools.org/" xmlns:ogr="http://ogr.maptools.org/" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:gml="http://www.opengis.net/gml/3.2" elementFormDefault="qualified" version="1.0">
<xs:import namespace="http://www.opengis.net/gml/3.2" schemaLocation="http://schemas.opengis.net/gml/3.2.1/gml.xsd"/>
<xs:element name="FeatureCollection" type="ogr:FeatureCollectionType" substitutionGroup="gml:AbstractFeature"/>
<xs:complexType name="FeatureCollectionType">
  <xs:complexContent>
    <xs:extension base="gml:AbstractFeatureType">
      <xs:sequence minOccurs="0" maxOccurs="unbounded">
        <xs:element name="featureMember">
          <xs:complexType>
            <xs:complexContent>
              <xs:extension base="gml:AbstractFeatureMemberType">
                <xs:sequence>
                  <xs:element ref="gml:AbstractFeature"/>
                </xs:sequence>
              </xs:extension>
            </xs:complexContent>
          </xs:complexType>
        </xs:element>
      </xs:sequence>
    </xs:extension>
  </xs:complexContent>
</xs:complexType>
"#,
    );
    let _ = writeln!(s, r#"<xs:element name="{name}" type="ogr:{name}_Type" substitutionGroup="gml:AbstractFeature"/>"#);
    let _ = writeln!(
        s,
        r#"<xs:complexType name="{name}_Type">
  <xs:complexContent>
    <xs:extension base="gml:AbstractFeatureType">
      <xs:sequence>
        <xs:element name="geometryProperty" type="gml:GeometryPropertyType" nillable="true" minOccurs="0" maxOccurs="1"/>"#
    );
    for (i, fname) in written_fields(layer) {
        let base = match layer.schema.fields()[i].field_type {
            FieldType::Integer => "integer",
            FieldType::Float => "decimal",
            FieldType::Date => "date",
            FieldType::DateTime => "dateTime",
            FieldType::Boolean => "boolean",
            _ => "string",
        };
        let _ = writeln!(
            s,
            r#"        <xs:element name="{fname}" nillable="true" minOccurs="0" maxOccurs="1"><xs:simpleType><xs:restriction base="xs:{base}"/></xs:simpleType></xs:element>"#
        );
    }
    s.push_str("      </xs:sequence>\n    </xs:extension>\n  </xs:complexContent>\n</xs:complexType>\n</xs:schema>\n");
    s
}

/// Encodes a layer as GML 3.2 referring to the application schema file `schema` (none when empty).
pub fn to_string(layer: &Layer, schema: &str) -> String {
    let name = element_name(&layer.name);
    let code = super::naming_epsg(layer);
    // Only a resolvable authority declares recoverable authority axes. Keep unknown codes
    // in the legacy XY naming convention, so reading them cannot silently transpose coordinates.
    let canonical = code.and_then(|code| SpatialRef::from_epsg(code).ok());
    let swap = canonical.as_ref().is_some_and(SpatialRef::has_north_east_axis_order);
    let srs = code
        .map(|c| {
            let name = if canonical.is_some() { format!("urn:ogc:def:crs:EPSG::{c}") } else { format!("EPSG:{c}") };
            format!(r#" srsName="{name}""#)
        })
        .unwrap_or_default();
    let fields = written_fields(layer);
    let mut s = String::new();
    let _ = write!(
        s,
        r#"<?xml version="1.0" encoding="utf-8" ?>
<ogr:FeatureCollection gml:id="aFeatureCollection"{} xmlns:ogr="http://ogr.maptools.org/" xmlns:gml="http://www.opengis.net/gml/3.2">
"#,
        if schema.is_empty() {
            String::new()
        } else {
            format!(
                r#" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://ogr.maptools.org/ {}""#,
                xml::escape(schema)
            )
        }
    );
    if let Some(e) = crate::vector::layer_extent(layer) {
        let (a, b) = if swap { ((e.1, e.0), (e.3, e.2)) } else { ((e.0, e.1), (e.2, e.3)) };
        let _ = writeln!(
            s,
            "  <gml:boundedBy><gml:Envelope{srs}><gml:lowerCorner>{} {}</gml:lowerCorner><gml:upperCorner>{} {}</gml:upperCorner></gml:Envelope></gml:boundedBy>",
            a.0, a.1, b.0, b.1
        );
    }
    for (n, f) in layer.features.iter().enumerate() {
        let _ = writeln!(
            s,
            r#"  <ogr:featureMember>
    <ogr:{name} gml:id="{name}.{n}">"#
        );
        if let Some(g) = &f.geometry {
            let mut gs = String::new();
            let mut counter = 0;
            write_geometry(&mut gs, g, &srs, swap, &format!("{name}.geom.{n}"), &mut counter);
            let _ = writeln!(s, "      <ogr:geometryProperty>{gs}</ogr:geometryProperty>");
        }
        for (i, fname) in &fields {
            let v = match f.attributes.get(*i) {
                None | Some(FieldValue::Null) => continue,
                Some(FieldValue::Text(t) | FieldValue::Date(t) | FieldValue::DateTime(t)) => xml::escape(t),
                Some(FieldValue::Integer(x)) => x.to_string(),
                Some(FieldValue::Float(x)) => x.to_string(),
                Some(FieldValue::Boolean(b)) => b.to_string(),
                Some(FieldValue::Blob(b)) => b.iter().map(|x| format!("{x:02X}")).collect(),
            };
            let _ = writeln!(s, "      <ogr:{fname}>{v}</ogr:{fname}>");
        }
        let _ = writeln!(s, "    </ogr:{name}>\n  </ogr:featureMember>");
    }
    s.push_str("</ogr:FeatureCollection>\n");
    s
}

fn pos_list(s: &mut String, cs: &[Coord], close: bool, swap: bool) {
    let z = cs.iter().any(|c| c.z.is_some());
    let first = cs.first().filter(|f| close && cs.len() > 1 && cs.last().map(|l| (l.x, l.y)) != Some((f.x, f.y)));
    for (i, c) in cs.iter().chain(first).enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let (a, b) = if swap { (c.y, c.x) } else { (c.x, c.y) };
        let _ = write!(s, "{a} {b}");
        if z {
            let _ = write!(s, " {}", c.z.unwrap_or(0.0));
        }
    }
}

fn dim_attr(has_z: bool) -> &'static str {
    if has_z { r#" srsDimension="3""# } else { "" }
}

fn write_polygon(s: &mut String, exterior: &Ring, interiors: &[Ring], srs: &str, swap: bool, id: &str) {
    let z = exterior.0.iter().any(|c| c.z.is_some());
    let _ = write!(s, r#"<gml:Polygon{srs} gml:id="{id}"><gml:exterior><gml:LinearRing><gml:posList{}>"#, dim_attr(z));
    pos_list(s, &exterior.0, true, swap);
    s.push_str("</gml:posList></gml:LinearRing></gml:exterior>");
    for r in interiors {
        let _ = write!(s, "<gml:interior><gml:LinearRing><gml:posList{}>", dim_attr(z));
        pos_list(s, &r.0, true, swap);
        s.push_str("</gml:posList></gml:LinearRing></gml:interior>");
    }
    s.push_str("</gml:Polygon>");
}

fn write_geometry(s: &mut String, g: &Geometry, srs: &str, swap: bool, id: &str, counter: &mut usize) {
    let sub = |counter: &mut usize| {
        *counter += 1;
        format!("{id}.{}", *counter - 1)
    };
    match g {
        Geometry::Point(c) => {
            let _ = write!(s, r#"<gml:Point{srs} gml:id="{id}"><gml:pos{}>"#, dim_attr(c.z.is_some()));
            pos_list(s, std::slice::from_ref(c), false, swap);
            s.push_str("</gml:pos></gml:Point>");
        }
        Geometry::LineString(cs) => {
            let _ = write!(s, r#"<gml:LineString{srs} gml:id="{id}"><gml:posList{}>"#, dim_attr(cs.iter().any(|c| c.z.is_some())));
            pos_list(s, cs, false, swap);
            s.push_str("</gml:posList></gml:LineString>");
        }
        Geometry::Polygon { exterior, interiors } => write_polygon(s, exterior, interiors, srs, swap, id),
        Geometry::MultiPoint(cs) => {
            let _ = write!(s, r#"<gml:MultiPoint{srs} gml:id="{id}">"#);
            for c in cs {
                s.push_str("<gml:pointMember>");
                write_geometry(s, &Geometry::Point(c.clone()), "", swap, &sub(counter), counter);
                s.push_str("</gml:pointMember>");
            }
            s.push_str("</gml:MultiPoint>");
        }
        Geometry::MultiLineString(ls) => {
            let _ = write!(s, r#"<gml:MultiCurve{srs} gml:id="{id}">"#);
            for l in ls {
                s.push_str("<gml:curveMember>");
                write_geometry(s, &Geometry::LineString(l.clone()), "", swap, &sub(counter), counter);
                s.push_str("</gml:curveMember>");
            }
            s.push_str("</gml:MultiCurve>");
        }
        Geometry::MultiPolygon(ps) => {
            let _ = write!(s, r#"<gml:MultiSurface{srs} gml:id="{id}">"#);
            for (e, hs) in ps {
                s.push_str("<gml:surfaceMember>");
                write_polygon(s, e, hs, "", swap, &sub(counter));
                s.push_str("</gml:surfaceMember>");
            }
            s.push_str("</gml:MultiSurface>");
        }
        Geometry::GeometryCollection(gs) => {
            let _ = write!(s, r#"<gml:MultiGeometry{srs} gml:id="{id}">"#);
            for g in gs {
                s.push_str("<gml:geometryMember>");
                write_geometry(s, g, "", swap, &sub(counter), counter);
                s.push_str("</gml:geometryMember>");
            }
            s.push_str("</gml:MultiGeometry>");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// GDAL 3.13.3 keeps both compound components and normalizes only horizontal XY.
    #[test]
    fn compound_urns_keep_vertical_metadata_and_coordinates() {
        for (horizontal, vertical, pos, xy) in
            [(2154, 5720, (652_000.0, 6_862_000.0), (652_000.0, 6_862_000.0)), (4269, 5713, (52.0, 4.0), (4.0, 52.0))]
        {
            let name = format!("urn:ogc:def:crs,crs:EPSG::{horizontal},crs:EPSG::{vertical}");
            let text = format!(
                "<FeatureCollection><featureMember><point><geom><Point srsName=\"{name}\" srsDimension=\"3\"><pos>{} {} 35</pos></Point></geom></point></featureMember></FeatureCollection>",
                pos.0, pos.1
            );
            let layers = parse(&xml::parse(&text).unwrap(), None).unwrap();
            let layer = &layers[0];
            assert_eq!(layer.crs_wkt(), SpatialRef::compound(horizontal, vertical).unwrap().to_wkt().as_deref());
            assert_eq!(layer.features[0].geometry, Some(Geometry::Point(Coord { x: xy.0, y: xy.1, z: Some(35.0), m: None })));
        }
    }

    /// Compound CRS axes supply the implicit dimension; explicit GML dimensions still win.
    #[test]
    fn compound_geometry_dimensions_preserve_height_without_an_explicit_dimension() {
        let srs = "urn:ogc:def:crs,crs:EPSG::4269,crs:EPSG::5713";
        for (geometry, expected) in [
            ("<Point><pos>52 4 35</pos></Point>", Geometry::Point(Coord { x: 4., y: 52., z: Some(35.), m: None })),
            (
                "<LineString><posList>52 4 35 53 5 36</posList></LineString>",
                Geometry::LineString(vec![Coord { x: 4., y: 52., z: Some(35.), m: None }, Coord { x: 5., y: 53., z: Some(36.), m: None }]),
            ),
            ("<Point srsDimension=\"2\"><pos>52 4</pos></Point>", Geometry::point(4., 52.)),
            ("<Point srsDimension=\"3\"><pos>52 4 35</pos></Point>", Geometry::Point(Coord { x: 4., y: 52., z: Some(35.), m: None })),
        ] {
            let text = format!(
                "<FeatureCollection><boundedBy><Envelope srsName=\"{srs}\"/></boundedBy><featureMember><p><geom>{geometry}</geom></p></featureMember></FeatureCollection>"
            );
            let layers = parse(&xml::parse(&text).unwrap(), None).unwrap();
            assert_eq!(layers[0].features[0].geometry, Some(expected));
        }
    }

    #[test]
    fn incomplete_compound_coordinate_tuples_are_refused_instead_of_truncated() {
        for geometry in [
            "<Point><pos>52 4</pos></Point>",
            "<LineString><posList>52 4 53 5</posList></LineString>",
            "<MultiPoint><pointMember><Point><pos>52 4</pos></Point></pointMember></MultiPoint>",
        ] {
            let text = format!(
                "<FeatureCollection><boundedBy><Envelope srsName=\"EPSG:4269+5713\"/></boundedBy><featureMember><p><geom>{geometry}</geom></p></featureMember></FeatureCollection>"
            );
            let error = parse(&xml::parse(&text).unwrap(), None).unwrap_err();
            assert!(matches!(error, Error::InvalidData(ref message) if message.contains("complete 3-dimensional tuples")), "{error}");
        }
    }

    /// GDAL 3.13.3 compound syntax identifies both components; Terrio stores one CRS per layer.
    #[test]
    fn compound_geometry_references_must_agree_with_the_layer() {
        let first = "urn:ogc:def:crs,crs:EPSG::2154,crs:EPSG::5720";
        for second in ["EPSG:2154+5720", "urn:ogc:def:crs,crs:EPSG:6.6:2154,crs:EPSG::5720"] {
            let text = format!(
                "<FeatureCollection><featureMember><point><geom><Point srsName=\"{first}\" srsDimension=\"3\"><pos>1 2 35</pos></Point></geom></point></featureMember><featureMember><point><geom><Point srsName=\"{second}\" srsDimension=\"3\"><pos>3 4 36</pos></Point></geom></point></featureMember></FeatureCollection>"
            );
            let layers = parse(&xml::parse(&text).unwrap(), None).unwrap();
            assert_eq!(layers[0].features.len(), 2);
            assert_eq!(layers[0].crs_wkt(), SpatialRef::compound(2154, 5720).unwrap().to_wkt().as_deref());
            assert_eq!(layers[0].features[1].geometry, Some(Geometry::Point(Coord { x: 3., y: 4., z: Some(36.), m: None })));
        }
        let second = "urn:ogc:def:crs,crs:EPSG::2154,crs:EPSG::5773";
        for geometry in [
            format!(
                "<Point srsName=\"{first}\"><pos>1 2 35</pos></Point></geom></point></featureMember><featureMember><point><geom><Point srsName=\"{second}\"><pos>3 4 36</pos></Point>"
            ),
            format!(
                "<MultiPoint srsName=\"{first}\"><pointMember><Point srsName=\"{second}\" srsDimension=\"3\"><pos>3 4 36</pos></Point></pointMember></MultiPoint>"
            ),
        ] {
            let text =
                format!("<FeatureCollection><featureMember><point><geom>{geometry}</geom></point></featureMember></FeatureCollection>");
            let error = parse(&xml::parse(&text).unwrap(), None).unwrap_err();
            assert!(matches!(error, Error::Unsupported(ref message) if message.contains("mixed coordinate reference systems")), "{error}");
        }
    }

    #[test]
    fn unsupported_compound_urns_are_errors_instead_of_partial_metadata() {
        for name in [
            "urn:ogc:def:crs,crs:IGNF::LAMB93,crs:EPSG::5720",
            "urn:ogc:def:crs,crs:EPSG::2154,crs:EPSG::999999999",
            "urn:ogc:def:crs,crs:EPSG::2154",
            "EPSG:4979+5720",
            "EPSG:5720+5713",
            "EPSG:+2154+5720",
        ] {
            let text = format!(
                "<FeatureCollection><featureMember><point><geom><Point srsName=\"{name}\"><pos>1 2</pos></Point></geom></point></featureMember></FeatureCollection>"
            );
            assert!(parse(&xml::parse(&text).unwrap(), None).is_err(), "{name}");
        }
    }

    /// Metadata references use literal CRS syntax, including GDAL 3.13.3 WKT definitions.
    #[test]
    #[cfg(fs)]
    fn geometry_metadata_does_not_follow_local_crs_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("definition.wkt");
        std::fs::write(&path, SpatialRef::from_epsg(2154).unwrap().to_wkt().unwrap()).unwrap();
        let name = path.to_str().unwrap();
        assert!(SpatialRef::parse(name).is_ok(), "the control is a readable CRS definition");
        assert!(parse_srs_name(name).is_err(), "geometry metadata must remain literal");
    }

    /// GDAL 3.12.1 exposes EPSG:6708 GML coordinates in traditional easting, northing order.
    #[test]
    fn authority_order_projected_coordinates_become_easting_northing() {
        let text = r#"<gml:FeatureCollection xmlns:gml="http://www.opengis.net/gml/3.2" xmlns:ex="http://example.org/ex"><gml:featureMember><ex:p gml:id="p.1"><ex:name>a</ex:name><ex:geom><gml:Point srsName="urn:ogc:def:crs:EPSG::6708" gml:id="g.1"><gml:pos>4823832.72 561996.39</gml:pos></gml:Point></ex:geom></ex:p></gml:featureMember></gml:FeatureCollection>"#;
        let layers = parse(&xml::parse(text).unwrap(), None).unwrap();
        assert_eq!(layers[0].crs_epsg(), Some(6708));
        assert_eq!(layers[0].features[0].geometry, Some(Geometry::point(561_996.39, 4_823_832.72)));
    }

    /// GDAL 3.12.1 honors authority order only for URNs and modern CRS URLs.
    #[test]
    fn authority_identifier_forms_preserve_xy_controls() {
        for (code, authority, xy) in [
            (6708, (4_823_832.72, 561_996.39), (561_996.39, 4_823_832.72)),
            (7792, (561_996.39, 4_823_832.72), (561_996.39, 4_823_832.72)),
            (4326, (43.56, 12.24), (12.24, 43.56)),
        ] {
            for (name, pos) in [
                (format!("urn:ogc:def:crs:EPSG::{code}"), authority),
                (format!("urn:x-ogc:def:crs:EPSG:{code}"), authority),
                (format!("http://www.opengis.net/def/crs/EPSG/0/{code}"), authority),
                (format!("http://www.opengis.net/gml/srs/epsg.xml#{code}"), xy),
                (format!("EPSG:{code}"), xy),
            ] {
                let text = format!(
                    "<FeatureCollection><featureMember><point><geom><Point srsName=\"{name}\" srsDimension=\"3\"><pos>{} {} 7</pos></Point></geom></point></featureMember></FeatureCollection>",
                    pos.0, pos.1
                );
                let layer = &parse(&xml::parse(&text).unwrap(), None).unwrap()[0];
                assert_eq!(layer.crs_epsg(), Some(code), "{name}");
                assert_eq!(layer.features[0].geometry, Some(Geometry::Point(Coord { x: xy.0, y: xy.1, z: Some(7.0), m: None })), "{name}");
            }
        }
    }

    /// GDAL 3.12.1 reads EPSG:6708 GML output in N-E authority order, including its envelope.
    #[test]
    fn northing_first_projected_gml_writes_authority_order() {
        let mut layer = Layer::new("places").with_epsg(6708);
        layer.push(Feature {
            fid: 0,
            geometry: Some(Geometry::Point(Coord { x: 561_996.39, y: 4_823_832.72, z: Some(7.0), m: None })),
            attributes: vec![],
        });
        let text = to_string(&layer, "");
        assert!(text.contains(">4823832.72 561996.39 7<"), "{text}");
        assert!(text.contains("<gml:lowerCorner>4823832.72 561996.39</gml:lowerCorner>"), "{text}");
        let restored = &parse(&xml::parse(&text).unwrap(), None).unwrap()[0];
        assert_eq!(restored.features[0].geometry, layer.features[0].geometry);
        assert_eq!(restored.crs_epsg(), Some(6708));
    }

    /// GDAL 3.13.3 uses the emitted EPSG URN's axes even when source WKT has no AXIS clauses.
    #[test]
    fn gml_writer_uses_emitted_authority_axes_when_source_axes_are_absent() {
        let mut layer = Layer::new("places").with_epsg(4326);
        layer.set_crs_wkt(Some(
            r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],AUTHORITY["EPSG","4326"]]"#.into(),
        ));
        layer.push(Feature { fid: 0, geometry: Some(Geometry::point(12.24, 43.56)), attributes: vec![] });
        let text = to_string(&layer, "");
        assert!(text.contains("urn:ogc:def:crs:EPSG::4326"), "{text}");
        assert!(text.contains(">43.56 12.24<"), "{text}");
        let restored = &parse(&xml::parse(&text).unwrap(), None).unwrap()[0];
        assert_eq!(restored.features[0].geometry, layer.features[0].geometry);
    }

    #[test]
    fn gml_writer_retains_xy_when_authority_is_unknown() {
        assert!(SpatialRef::from_epsg(999999).is_err());
        let geographic = r#"GEOGCS["Custom geographic",DATUM["Custom",SPHEROID["Sphere",6371000,0]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433],AXIS["Latitude",NORTH],AXIS["Longitude",EAST],AUTHORITY["EPSG","999999"]]"#;
        let projected = format!(
            r#"PROJCS["Custom projected",{geographic},PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",0],PARAMETER["scale_factor",1],PARAMETER["false_easting",0],PARAMETER["false_northing",0],UNIT["metre",1],AXIS["Northing",NORTH],AXIS["Easting",EAST],AUTHORITY["EPSG","999999"]]"#
        );
        for wkt in [geographic, projected.as_str()] {
            let mut layer = Layer::new("places").with_epsg(999999);
            layer.set_crs_wkt(Some(wkt.into()));
            layer.push(Feature { fid: 0, geometry: Some(Geometry::point(12.24, 43.56)), attributes: vec![] });
            let text = to_string(&layer, "");
            assert!(text.contains(r#"srsName="EPSG:999999""#), "{text}");
            assert!(text.contains(">12.24 43.56<"), "{text}");
            let restored = &parse(&xml::parse(&text).unwrap(), None).unwrap()[0];
            assert_eq!(restored.features[0].geometry, layer.features[0].geometry);
        }
    }

    #[test]
    fn unresolved_layer_crs_refuses_differing_geometry_references_explicitly() {
        let text = r#"<FeatureCollection><featureMember><p><geom><Point srsName="EPSG:999999"><pos>1 2</pos></Point></geom></p></featureMember><featureMember><p><geom><Point srsName="EPSG:4326"><pos>3 4</pos></Point></geom></p></featureMember></FeatureCollection>"#;
        let error = parse(&xml::parse(text).unwrap(), None).unwrap_err();
        assert!(matches!(error, Error::Unsupported(ref message) if message.contains("layer CRS is unresolved")), "{error}");
    }

    #[test]
    fn mixed_geometry_axis_conventions_are_refused_including_nested_members() {
        let collection = |body: &str| format!("<FeatureCollection>{body}</FeatureCollection>");
        let point = |srs: &str, pos: &str| format!("<Point srsName=\"{srs}\"><pos>{pos}</pos></Point>");
        let feature = |geometry: &str| format!("<featureMember><p><geom>{geometry}</geom></p></featureMember>");
        let urn = point("urn:ogc:def:crs:EPSG::6708", "4823832.72 561996.39");
        let short = point("EPSG:6708", "561996.39 4823832.72");
        let unknown = point("EPSG:999999", "561996.39 4823832.72");
        for body in [
            format!("{}{}", feature(&urn), feature(&short)),
            feature(&format!("<MultiPoint><pointMember>{urn}</pointMember><pointMember>{unknown}</pointMember></MultiPoint>")),
            feature(&format!("<MultiPoint><pointMember>{urn}</pointMember><pointMember>{short}</pointMember></MultiPoint>")),
        ] {
            let result = parse(&xml::parse(&collection(&body)).unwrap(), None);
            assert!(matches!(result, Err(Error::Unsupported(_))));
        }
        // Identifier styles can mix when both describe the same effective E/N convention.
        let body = format!("{}{}", feature(&point("urn:ogc:def:crs:EPSG::7792", "1 2")), feature(&point("EPSG:7792", "3 4")));
        assert_eq!(parse(&xml::parse(&collection(&body)).unwrap(), None).unwrap()[0].features.len(), 2);
    }

    #[test]
    fn reads_gml2_and_gml3() {
        let gml2 = r#"<wfs:FeatureCollection xmlns:wfs="w" xmlns:gml="http://www.opengis.net/gml" xmlns:x="x"><gml:featureMember><x:roads fid="r.1"><x:name>A</x:name><x:lanes>2</x:lanes>
            <x:geom><gml:MultiLineString srsName="EPSG:4326"><gml:lineStringMember><gml:LineString><gml:coordinates>1,2 3,4</gml:coordinates></gml:LineString></gml:lineStringMember></gml:MultiLineString></x:geom></x:roads></gml:featureMember></wfs:FeatureCollection>"#;
        let layers = parse(&xml::parse(gml2).unwrap(), None).unwrap();
        let l = &layers[0];
        assert_eq!((l.name.as_str(), l.crs_epsg()), ("roads", Some(4326)));
        assert_eq!(l.schema.fields().iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["gml_id", "name", "lanes"]);
        assert_eq!(l.features[0].attributes[2], FieldValue::Integer(2));
        assert_eq!(l.features[0].geometry, Some(Geometry::MultiLineString(vec![vec![Coord::xy(1.0, 2.0), Coord::xy(3.0, 4.0)]])));

        // GML 3.2 with a URN: latitude first.
        let gml3 = r#"<ogr:FeatureCollection xmlns:ogr="o" xmlns:gml="http://www.opengis.net/gml"><ogr:featureMember><ogr:t gml:id="t.0"><ogr:geometryProperty><gml:Polygon srsName="urn:ogc:def:crs:EPSG::4326"><gml:exterior><gml:LinearRing><gml:posList>10 1 10 2 11 2 10 1</gml:posList></gml:LinearRing></gml:exterior></gml:Polygon></ogr:geometryProperty></ogr:t></ogr:featureMember></ogr:FeatureCollection>"#;
        let l = &parse(&xml::parse(gml3).unwrap(), None).unwrap()[0];
        let Some(Geometry::Polygon { exterior, .. }) = &l.features[0].geometry else { panic!("no polygon") };
        assert_eq!((exterior.0[0].x, exterior.0[0].y), (1.0, 10.0));
        assert_eq!(l.features[0].attributes[0], FieldValue::Text("t.0".into()));
    }

    #[test]
    fn application_source_identifier_collision_is_refused() {
        let root = xml::parse(r#"<FeatureCollection><featureMember><road id="r1"><gml_id>app</gml_id><geom><Point><pos>1 2</pos></Point></geom></road></featureMember></FeatureCollection>"#).unwrap();
        assert!(matches!(parse(&root, None), Err(Error::Unsupported(_))));
        let schema = xml::parse(r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema"><xs:element name="road"><xs:complexType><xs:sequence><xs:element name="rank" type="xs:int"/></xs:sequence></xs:complexType></xs:element></xs:schema>"#).unwrap();
        assert!(matches!(parse(&root, Some(&schema)), Err(Error::Unsupported(_))));
    }

    #[test]
    fn unavailable_companion_schema_types_keep_local_inference() {
        let root = xml::parse(r#"<wfs:FeatureCollection xmlns:wfs="http://www.opengis.net/wfs" xmlns:t="urn:test" xmlns:gml="http://www.opengis.net/gml"><gml:featureMember><t:road fid="r1"><t:code>007</t:code><t:geom><gml:Point><gml:pos>1 2</gml:pos></gml:Point></t:geom></t:road></gml:featureMember></wfs:FeatureCollection>"#).unwrap();
        let schema = xml::parse(r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:f="urn:foreign" targetNamespace="urn:test"><xs:import namespace="urn:foreign"/><xs:element name="road" type="f:RoadType"/></xs:schema>"#).unwrap();
        let layers = parse(&root, Some(&schema)).unwrap();
        assert_eq!(layers[0].features.len(), 1);
        assert_eq!(layers[0].features[0].geometry, Some(Geometry::point(1., 2.)));
        assert_eq!(layers[0].features[0].attributes[1], FieldValue::Integer(7));
    }

    #[test]
    fn inferred_text_keeps_internal_whitespace() {
        let root = xml::parse(
            "<FeatureCollection><featureMember><road><name>  Main   Street\nNorth  </name></road></featureMember></FeatureCollection>",
        )
        .unwrap();
        let layer = &parse(&root, None).unwrap()[0];
        assert_eq!(layer.features[0].attributes[1], FieldValue::Text("Main   Street\nNorth".into()));
    }

    #[test]
    fn partial_companion_schemas_keep_permissive_file_reads() {
        let root = xml::parse(r#"<FeatureCollection xmlns:t="urn:other" xmlns:gml="http://www.opengis.net/gml"><featureMember><t:road><gml:boundedBy/><t:rank>7</t:rank><t:extra>ignored</t:extra><t:geom><gml:Point><gml:pos>1 2</gml:pos></gml:Point></t:geom></t:road></featureMember></FeatureCollection>"#).unwrap();
        let schema = xml::parse(r#"<xs:schema xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:t="urn:test" targetNamespace="urn:test"><xs:element name="road" type="t:RoadType"/><xs:complexType name="RoadType"><xs:sequence><xs:element name="rank" type="xs:int"/></xs:sequence></xs:complexType></xs:schema>"#).unwrap();
        let layer = &parse(&root, Some(&schema)).unwrap()[0];
        assert_eq!(layer.schema.fields().len(), 2);
        assert_eq!(layer.features[0].attributes[1], FieldValue::Integer(7));
        assert_eq!(layer.features[0].geometry, Some(Geometry::point(1., 2.)));
        let mut duplicate = schema.clone();
        duplicate.children.push(schema.children[0].clone());
        let layers = parse(&root, Some(&duplicate)).unwrap();
        assert!(layers[0].schema.field_index("extra").is_some(), "bad companion schemas fall back to inference");
    }

    #[test]
    fn legacy_epsg_dictionary_identifiers_keep_traditional_xy() {
        let root = xml::parse(r#"<FeatureCollection><featureMember><road><geom><Point srsName="http://www.opengis.net/gml/srs/epsg.xml#4326"><coordinates>10,50</coordinates></Point></geom></road></featureMember></FeatureCollection>"#).unwrap();
        let layers = parse(&root, None).unwrap();
        assert_eq!(layers[0].crs_epsg(), Some(4326));
        assert_eq!(layers[0].features[0].geometry, Some(Geometry::point(10., 50.)));
        assert_eq!(
            parse_srs_name("https://www.opengis.net/gml/srs/epsg.xml#25832").unwrap().authority_code().as_deref(),
            Some("EPSG:25832")
        );
        assert!(parse_srs_name("http://www.opengis.net/gml/srs/epsg.xml#bad").is_err());
    }

    #[test]
    fn round_trips() {
        let mut l = Layer::new("places").with_epsg(4326);
        l.add_field(FieldDef::new("name", FieldType::Text));
        l.add_field(FieldDef::new("pop", FieldType::Float));
        l.push(Feature {
            fid: 0,
            geometry: Some(Geometry::point(10.5, 45.0)),
            attributes: vec![FieldValue::Text("A & B".into()), FieldValue::Float(1.5)],
        });
        let text = to_string(&l, "places.xsd");
        let schema = xml::parse(&xsd(&l)).unwrap();
        let back = &parse(&xml::parse(&text).unwrap(), Some(&schema)).unwrap()[0];
        assert_eq!(back.features[0].geometry, Some(Geometry::point(10.5, 45.0)));
        assert_eq!(back.features[0].attributes[1], FieldValue::Text("A & B".into()));
        assert_eq!(back.schema.fields()[2].field_type, FieldType::Float);
    }
}

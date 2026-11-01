// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Native CityGML encoding of flat vector primitives using the published 2.0/3.0 schemas.
//! GDAL 3.13.3 is informational; the OGC schemas define the encoding.
use crate::{Error, Result, crs::SpatialRef};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;
use std::rc::Rc;
use wbvector::{Coord, FieldValue, Layer};

use super::city_write::{COMMON_FIELDS, Primitive, field, flatten, has_city_fields, json_field, property, xyz};
use super::xml::escape;

#[path = "citygml_write_profiles.rs"]
mod profiles;

/// CityGML XML encoding version.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CityGmlVersion {
    /// CityGML 2.0, including LoD 4.
    V2_0,
    /// CityGML 3.0, including LoDs 0 through 3.
    #[default]
    V3_0,
}
impl CityGmlVersion {
    /// The namespace version identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V2_0 => "2.0",
            Self::V3_0 => "3.0",
        }
    }
}
impl std::str::FromStr for CityGmlVersion {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "2.0" => Ok(Self::V2_0),
            "3.0" => Ok(Self::V3_0),
            _ => Err(invalid("version must be 2.0 or 3.0")),
        }
    }
}
fn invalid(message: impl std::fmt::Display) -> Error {
    Error::InvalidArgument(format!("CityGML writer: {message}"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GeometryProperty {
    None,
    Any,
    Surface,
    SurfaceSingle,
    Curve,
    Points,
    Point,
    Tin,
}
struct Property {
    name: &'static str,
    geometry: GeometryProperty,
    children: &'static [&'static str],
    required: bool,
    many: bool,
}
struct Profile {
    name: &'static str,
    ancestors: &'static [&'static str],
    properties: &'static [Property],
}
impl Profile {
    fn relation(&self, child: &Profile) -> Option<usize> {
        self.properties.iter().position(|p| p.children.iter().any(|target| child.ancestors.contains(target)))
    }
}
fn profile(version: CityGmlVersion, name: &str) -> Result<&'static Profile> {
    profiles::profiles(version)
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| invalid(format!("{name} is not supported in CityGML {}", version.as_str())))
}
fn native_type(source: &str, version: CityGmlVersion) -> Result<&'static Profile> {
    let kind = match (source, version) {
        ("GenericCityObject", CityGmlVersion::V3_0) => "GenericOccupiedSpace",
        ("GenericOccupiedSpace" | "GenericUnoccupiedSpace" | "GenericLogicalSpace", CityGmlVersion::V2_0) => "GenericCityObject",
        ("BuildingRoom", CityGmlVersion::V2_0) => "Room",
        ("Room", CityGmlVersion::V3_0) => "BuildingRoom",
        ("BuildingStorey", _) => "Storey",
        ("TunnelHollowSpace" | "TunnelRoom", _) => "HollowSpace",
        ("BridgeConstructiveElement", CityGmlVersion::V2_0) => "BridgeConstructionElement",
        ("BridgeConstructionElement", CityGmlVersion::V3_0) => "BridgeConstructiveElement",
        ("TransportSquare", _) => "Square",
        ("IntBuildingInstallation", CityGmlVersion::V3_0) => "BuildingInstallation",
        ("IntBridgeInstallation", CityGmlVersion::V3_0) => "BridgeInstallation",
        ("IntTunnelInstallation", CityGmlVersion::V3_0) => "TunnelInstallation",
        _ => source,
    };
    let module = if kind.starts_with("Building") || matches!(kind, "Room" | "Storey" | "IntBuildingInstallation" | "BuildingUnit") {
        "bldg"
    } else if kind.starts_with("Bridge") || kind == "IntBridgeInstallation" {
        "brid"
    } else if kind.starts_with("Tunnel") || matches!(kind, "HollowSpace" | "IntTunnelInstallation") {
        "tun"
    } else if kind.starts_with("Generic") {
        "gen"
    } else {
        match kind {
            "Door" | "Window" if version == CityGmlVersion::V2_0 => "bldg",
            "Door" | "Window" | "OtherConstruction" => "con",
            "CityFurniture" => "frn",
            "CityObjectGroup" => "grp",
            "LandUse" => "luse",
            "SolitaryVegetationObject" | "PlantCover" => "veg",
            "ReliefFeature" | "TINRelief" | "MassPointRelief" | "BreaklineRelief" => "dem",
            "WaterBody" => "wtr",
            "Road"
            | "Waterway"
            | "Railway"
            | "Track"
            | "Square"
            | "TransportationComplex"
            | "TrafficSpace"
            | "AuxiliaryTrafficSpace"
            | "TrafficArea"
            | "AuxiliaryTrafficArea"
            | "Section"
            | "Intersection" => "tran",
            _ => return Err(invalid(format!("unsupported city_type {source:?}"))),
        }
    };
    profile(version, &format!("{module}:{kind}"))
}

type Part<'a> = (Rc<str>, Rc<Value>, Primitive<'a>);

struct Object<'a> {
    source_type: String,
    native: &'static Profile,
    layer: usize,
    attributes: Map<String, Value>,
    parents: Vec<String>,
    parts: Vec<Part<'a>>,
}
fn objects(layers: &[Layer], version: CityGmlVersion) -> Result<BTreeMap<String, Object<'_>>> {
    let mut out: BTreeMap<String, Object<'_>> = BTreeMap::new();
    let mut reserved = HashSet::new();
    let mut metadata: HashMap<String, (usize, Vec<&FieldValue>)> = HashMap::new();
    let mut semantics: HashMap<&str, Rc<Value>> = HashMap::new();
    for layer in layers.iter().filter(|l| has_city_fields(l)) {
        for row in &layer.features {
            if let FieldValue::Text(id) = field(layer, row, "city_id") {
                reserved.insert(id.clone());
            }
        }
    }
    for (li, layer) in layers.iter().enumerate() {
        let flat = has_city_fields(layer);
        for (fi, row) in layer.features.iter().enumerate() {
            let id = match if flat { field(layer, row, "city_id") } else { &FieldValue::Null } {
                FieldValue::Text(id) if !id.is_empty() => id.clone(),
                FieldValue::Null => {
                    let mut id = format!("_terrio_{li}_{fi}");
                    while !reserved.insert(id.clone()) {
                        id.push('_');
                    }
                    id
                }
                _ => return Err(invalid("city_id must be nonempty text")),
            };
            let raw: Vec<_> = layer
                .schema
                .fields()
                .iter()
                .zip(&row.attributes)
                .filter(|(def, _)| {
                    !matches!(
                        def.name.as_str(),
                        "lod" | "surface_semantics" | "semantics" | "geometry_type" | "skipped_faces" | "skipped_rings"
                    )
                })
                .map(|(_, value)| value)
                .collect();
            if !metadata.get(&id).is_some_and(|(layer, values)| *layer == li && values == &raw) {
                let kind = match if flat { field(layer, row, "city_type") } else { &FieldValue::Null } {
                    FieldValue::Text(kind) => kind.as_str(),
                    FieldValue::Null => "GenericCityObject",
                    _ => return Err(invalid("city_type must be text")),
                };
                let native = native_type(kind, version)?;
                let mut attributes = if flat {
                    match json_field(field(layer, row, "attributes"))? {
                        Value::Null => Map::new(),
                        Value::Object(map) => map,
                        _ => return Err(invalid("attributes must be a JSON object")),
                    }
                } else {
                    Map::new()
                };
                for (def, value) in layer.schema.fields().iter().zip(&row.attributes) {
                    if flat
                        && (COMMON_FIELDS.contains(&def.name.as_str()) || matches!(def.name.as_str(), "skipped_rings" | "skipped_faces"))
                    {
                        continue;
                    }
                    let value = property(value, def.field_type)?;
                    if attributes.get(&def.name).is_some_and(|old| old != &value) {
                        return Err(invalid(format!("conflicting attribute {} on {id}", def.name)));
                    }
                    attributes.insert(def.name.clone(), value);
                }
                let parents = if flat {
                    match json_field(field(layer, row, "parents"))? {
                        Value::Null => Vec::new(),
                        value => serde_json::from_value::<Vec<String>>(value).map_err(invalid)?,
                    }
                } else {
                    Vec::new()
                };
                let entry = out.entry(id.clone()).or_insert_with(|| Object {
                    source_type: kind.into(),
                    native,
                    layer: li,
                    attributes: attributes.clone(),
                    parents: parents.clone(),
                    parts: Vec::new(),
                });
                if entry.source_type != kind || entry.layer != li || entry.attributes != attributes || entry.parents != parents {
                    return Err(invalid(format!("conflicting metadata for object {id}")));
                }
                metadata.insert(id.clone(), (li, raw));
            }
            let entry = out.get_mut(&id).unwrap();
            if let Some(geometry) = &row.geometry {
                let lod = match if flat { field(layer, row, "lod") } else { &FieldValue::Null } {
                    FieldValue::Null => "0",
                    FieldValue::Text(lod) => lod,
                    _ => return Err(invalid("lod must be text")),
                };
                lod_number(lod, version)?;
                let semantic = if flat {
                    match field(layer, row, "surface_semantics") {
                        FieldValue::Text(text) => {
                            if let Some(value) = semantics.get(text.as_str()) {
                                value.clone()
                            } else {
                                let value = Rc::new(json_field(&FieldValue::Text(text.clone()))?);
                                semantics.insert(text, value.clone());
                                value
                            }
                        }
                        value => Rc::new(json_field(value)?),
                    }
                } else {
                    Rc::new(Value::Null)
                };
                if !semantic.is_null() && !semantic.is_object() {
                    return Err(invalid("surface_semantics must be a JSON object"));
                }
                if !semantic.is_null() && semantic.get("type").and_then(Value::as_str).is_none() {
                    return Err(invalid("surface semantics need a text type"));
                }
                let lod: Rc<str> = Rc::from(lod);

                let mut parts = Vec::new();
                flatten(geometry, 0, &mut parts)?;
                for part in parts {
                    part.positions(|point| xyz(point).map(|_| ()))?;
                    if !matches!(part, Primitive::Surface(..)) && !semantic.is_null() {
                        return Err(invalid("surface semantics attached to a point or line"));
                    }
                    entry.parts.push((lod.clone(), semantic.clone(), part));
                }
            }
        }
    }
    if version == CityGmlVersion::V2_0 {
        let openings: Vec<_> = out
            .iter()
            .filter(|(_, o)| matches!(o.source_type.as_str(), "Door" | "Window"))
            .map(|(id, o)| (id.clone(), o.source_type.clone(), o.parents.first().cloned()))
            .collect();
        for (id, kind, parent) in openings {
            if let Some(owner) = parent.as_ref().and_then(|p| out.get(p)) {
                let module = owner.native.name.split(':').next().unwrap();
                out.get_mut(&id).unwrap().native = profile(version, &format!("{module}:{kind}"))?;
            }
        }
    }
    Ok(out)
}
fn lod_number(lod: &str, version: CityGmlVersion) -> Result<u8> {
    let bytes = lod.as_bytes();
    if !(bytes.len() == 1 || bytes.len() == 3 && bytes[1] == b'.') || !bytes.iter().enumerate().all(|(i, b)| i == 1 || b.is_ascii_digit()) {
        return Err(invalid(format!("invalid LoD {lod:?}")));
    }
    let number = bytes[0] - b'0';
    if number > if version == CityGmlVersion::V2_0 { 4 } else { 3 } {
        return Err(invalid(format!("LoD {lod} is outside CityGML {}", version.as_str())));
    }
    Ok(number)
}

fn xml_text(value: &str) -> Result<String> {
    if value.chars().any(|c| !matches!(c,'\u{9}'|'\u{a}'|'\u{d}'|'\u{20}'..='\u{d7ff}'|'\u{e000}'..='\u{fffd}'|'\u{10000}'..='\u{10ffff}'))
    {
        return Err(invalid("text contains a character forbidden in XML 1.0"));
    }
    Ok(escape(value))
}
fn json_text(value: &Value) -> Result<String> {
    // JSON escapes preserve controls and the two XML-forbidden BMP noncharacters.
    xml_text(&value.to_string().replace('\u{fffe}', "\\ufffe").replace('\u{ffff}', "\\uffff"))
}
fn valid_id(id: &str) -> bool {
    fn start(c: char) -> bool {
        matches!(c,'A'..='Z'|'a'..='z'|'_'|'\u{c0}'..='\u{d6}'|'\u{d8}'..='\u{f6}'|'\u{f8}'..='\u{2ff}'|'\u{370}'..='\u{37d}'|'\u{37f}'..='\u{1fff}'|'\u{200c}'..='\u{200d}'|'\u{2070}'..='\u{218f}'|'\u{2c00}'..='\u{2fef}'|'\u{3001}'..='\u{d7ff}'|'\u{f900}'..='\u{fdcf}'|'\u{fdf0}'..='\u{fffd}'|'\u{10000}'..='\u{effff}')
    }
    let mut chars = id.chars();
    chars.next().is_some_and(start)
        && chars.all(|c| start(c) || matches!(c,'-'|'.'|'0'..='9'|'\u{b7}'|'\u{300}'..='\u{36f}'|'\u{203f}'..='\u{2040}'))
}
fn xml_ids(objects: &BTreeMap<String, Object<'_>>) -> BTreeMap<String, String> {
    let mut reserved: HashSet<_> = objects.keys().filter(|s| valid_id(s)).cloned().collect();
    objects
        .keys()
        .map(|id| {
            let mut xml = id.clone();
            if !valid_id(id) {
                xml = format!("_terrio_id_{}", id.as_bytes().iter().map(|b| format!("{b:02x}")).collect::<String>());
                while !reserved.insert(xml.clone()) {
                    xml.push('_');
                }
            }
            (id.clone(), xml)
        })
        .collect()
}

struct Reference {
    name: Option<String>,
    swap: bool,
}
fn references(layers: &[Layer]) -> Result<Reference> {
    let mut source: Option<SpatialRef> = None;
    let mut unknown = false;
    for layer in layers {
        let crs = (layer.crs_epsg().is_some() || layer.crs_wkt().is_some()).then(|| SpatialRef::from_layer(layer));
        if let Some(crs) = crs {
            if unknown || source.as_ref().is_some_and(|old| !old.same_as(&crs)) {
                return Err(invalid("layers have inconsistent CRSs"));
            }
            source = Some(crs);
        } else {
            if source.is_some() {
                return Err(invalid("layers have inconsistent CRSs"));
            }
            unknown = true;
        }
    }
    let Some(crs) = source else {
        return Ok(Reference { name: None, swap: false });
    };
    let epsg = |value: &Value| value.get("id").filter(|id| id["authority"] == "EPSG").and_then(|id| id["code"].as_u64());
    let name = if let Some(("EPSG", code)) = crs.authority() {
        format!("http://www.opengis.net/def/crs/EPSG/0/{code}")
    } else {
        let value: Value = serde_json::from_str(&crs.to_projjson()?).map_err(invalid)?;
        let parts = value
            .get("components")
            .and_then(Value::as_array)
            .ok_or_else(|| invalid("CRS needs an EPSG identifier or registered EPSG components"))?;
        let codes = parts
            .iter()
            .map(|p| epsg(p).ok_or_else(|| invalid("compound CRS component lacks an EPSG identifier")))
            .collect::<Result<Vec<_>>>()?;
        format!("urn:ogc:def:crs,{}", codes.into_iter().map(|c| format!("crs:EPSG::{c}")).collect::<Vec<_>>().join(","))
    };
    Ok(Reference { name: Some(name), swap: crs.is_geographic() })
}

fn generic_metadata(value: &Value, version: CityGmlVersion) -> Result<String> {
    let text = json_text(value)?;
    Ok(match version {
        CityGmlVersion::V2_0 => format!(
            "<gen:genericAttributeSet name=\"terrio:flat-object\" codeSpace=\"urn:terrio:citygml:flat-object\"><gen:stringAttribute name=\"json\"><gen:value>{text}</gen:value></gen:stringAttribute></gen:genericAttributeSet>"
        ),
        CityGmlVersion::V3_0 => format!(
            "<core:genericAttribute><gen:GenericAttributeSet><gen:name>terrio:flat-object</gen:name><gen:codeSpace>urn:terrio:citygml:flat-object</gen:codeSpace><gen:genericAttribute><gen:StringAttribute><gen:name>json</gen:name><gen:value>{text}</gen:value></gen:StringAttribute></gen:genericAttribute></gen:GenericAttributeSet></core:genericAttribute>"
        ),
    })
}
fn coordinate(point: &Coord, reference: &Reference) -> Result<String> {
    let [mut x, mut y, z] = xyz(point)?;
    if reference.swap {
        std::mem::swap(&mut x, &mut y);
    }
    Ok(format!("{x} {y} {z}"))
}
fn coordinate_key(point: &Coord) -> [u64; 3] {
    [point.x, point.y, point.z.unwrap_or(0.0)].map(|v| if v == 0.0 { 0 } else { v.to_bits() })
}
fn positions(points: &[Coord], reference: &Reference, close: bool) -> Result<String> {
    let mut points = points.to_vec();
    if close && points.len() > 1 && points.first() == points.last() {
        points.pop();
    }
    let distinct = points.iter().map(coordinate_key).collect::<HashSet<_>>().len();
    if distinct < if close { 3 } else { 2 } {
        return Err(invalid("a ring or line has too few distinct XYZ positions"));
    }
    if close {
        points.push(points[0].clone());
    }
    Ok(points.iter().map(|p| coordinate(p, reference)).collect::<Result<Vec<_>>>()?.join(" "))
}
fn primitive(part: Primitive<'_>, lod: &str, semantic: &Value, reference: &Reference, tin: bool) -> Result<String> {
    let attrs = reference.name.as_ref().map_or(String::new(), |name| format!(" srsName=\"{}\"", escape(name)));
    let annotation = format!(
        "<gml:name codeSpace=\"urn:terrio:citygml:flat-primitive\">{}</gml:name>",
        json_text(&json!({"version":1,"lod":lod,"surface_semantics":semantic}))?
    );
    Ok(match part {
        Primitive::Point(p) => {
            format!("<gml:Point{attrs} srsDimension=\"3\">{annotation}<gml:pos>{}</gml:pos></gml:Point>", coordinate(p, reference)?)
        }
        Primitive::Line(points) => format!(
            "<gml:LineString{attrs} srsDimension=\"3\">{annotation}<gml:posList>{}</gml:posList></gml:LineString>",
            positions(points, reference, false)?
        ),
        Primitive::Surface(exterior, holes) => {
            if tin && (!holes.is_empty() || exterior.0.iter().map(coordinate_key).collect::<HashSet<_>>().len() != 3) {
                return Err(invalid("TINRelief requires triangular faces without holes"));
            }
            let mut content = String::new();
            for (i, ring) in std::iter::once(exterior).chain(holes.iter()).enumerate() {
                let tag = if i == 0 { "exterior" } else { "interior" };
                write!(
                    content,
                    "<gml:{tag}><gml:LinearRing><gml:posList>{}</gml:posList></gml:LinearRing></gml:{tag}>",
                    positions(&ring.0, reference, true)?
                )
                .unwrap();
            }
            let tag = if tin { "Triangle" } else { "Polygon" };
            if tin {
                return Ok(format!("<gml:Triangle>{content}</gml:Triangle>"));
            }
            format!("<gml:{tag}{attrs} srsDimension=\"3\">{annotation}{content}</gml:{tag}>")
        }
    })
}
fn geometry(property: &Property, parts: &[&Part<'_>], reference: &Reference) -> Result<String> {
    let bodies = parts
        .iter()
        .map(|(lod, sem, p)| primitive(*p, lod, sem, reference, property.geometry == GeometryProperty::Tin))
        .collect::<Result<Vec<_>>>()?;
    let (tag, member) = match property.geometry {
        GeometryProperty::Surface => ("MultiSurface", "surfaceMember"),
        GeometryProperty::SurfaceSingle => ("CompositeSurface", "surfaceMember"),
        GeometryProperty::Curve => ("MultiCurve", "curveMember"),
        GeometryProperty::Points => ("MultiPoint", "pointMember"),
        GeometryProperty::Any => ("MultiGeometry", "geometryMember"),
        GeometryProperty::Tin => ("TriangulatedSurface", "trianglePatches"),
        GeometryProperty::Point => {
            if bodies.len() != 1 {
                return Err(invalid("LoD 0 Point property accepts exactly one point"));
            }
            return Ok(format!("<{0}>{1}</{0}>", property.name, bodies[0]));
        }
        GeometryProperty::None => return Err(invalid("expected a native geometry property")),
    };
    let body = if property.geometry == GeometryProperty::Tin {
        let patches: Vec<_> =
            parts.iter().map(|(lod, semantic, _)| json!({"version":1,"lod":lod.as_ref(),"surface_semantics":semantic.as_ref()})).collect();
        let metadata = json_text(&json!({"version":1,"patches":patches}))?;
        format!(
            "<gml:name codeSpace=\"urn:terrio:citygml:flat-patches\">{metadata}</gml:name><gml:{member}>{}</gml:{member}>",
            bodies.join("")
        )
    } else {
        bodies.into_iter().map(|s| format!("<gml:{member}>{s}</gml:{member}>")).collect()
    };
    let attrs = reference.name.as_ref().map_or(String::new(), |name| format!(" srsName=\"{}\" srsDimension=\"3\"", escape(name)));
    Ok(format!("<{0}><gml:{tag}{attrs}>{body}</gml:{tag}></{0}>", property.name))
}
fn property_for(profile: &Profile, lod: u8, part: Primitive<'_>) -> Option<usize> {
    profile.properties.iter().position(|p| {
        if p.name == "dem:tin" {
            return matches!(part, Primitive::Surface(..));
        }
        if p.name == "dem:reliefPoints" {
            return matches!(part, Primitive::Point(..));
        }
        if matches!(p.name, "dem:breaklines" | "dem:ridgeOrValleyLines") {
            return matches!(part, Primitive::Line(..));
        }
        let name = p.name.split(':').next_back().unwrap_or_default();
        if !name.starts_with(&format!("lod{lod}")) || name.contains("TerrainIntersection") {
            return false;
        }
        match p.geometry {
            GeometryProperty::Any => true,
            GeometryProperty::Surface | GeometryProperty::SurfaceSingle => matches!(part, Primitive::Surface(..)),
            GeometryProperty::Curve => matches!(part, Primitive::Line(..)),
            GeometryProperty::Point | GeometryProperty::Points => matches!(part, Primitive::Point(..)),
            _ => false,
        }
    })
}

fn boundary_for(
    owner: &Profile,
    semantic: &Value,
    lod: u8,
    part: Primitive<'_>,
    version: CityGmlVersion,
) -> Option<(usize, &'static Profile, usize)> {
    if !matches!(part, Primitive::Surface(..)) {
        return None;
    }
    let semantic = semantic.get("type").and_then(Value::as_str);
    let source = match semantic {
        Some("Window") if version == CityGmlVersion::V3_0 => "WindowSurface",
        Some("Door") if version == CityGmlVersion::V3_0 => "DoorSurface",
        Some(
            "RoofSurface"
            | "GroundSurface"
            | "WallSurface"
            | "ClosureSurface"
            | "OuterCeilingSurface"
            | "OuterFloorSurface"
            | "InteriorWallSurface"
            | "CeilingSurface"
            | "FloorSurface"
            | "WaterSurface"
            | "WaterGroundSurface"
            | "WaterClosureSurface"
            | "WindowSurface"
            | "DoorSurface"
            | "GenericThematicSurface",
        ) => semantic.unwrap(),
        None if property_for(owner, lod, part).is_none() && version == CityGmlVersion::V3_0 => "GenericThematicSurface",
        None if property_for(owner, lod, part).is_none() && owner.name == "wtr:WaterBody" => "WaterSurface",
        _ => return None,
    };
    let module = if source.starts_with("Water") {
        "wtr"
    } else if source == "GenericThematicSurface" {
        "gen"
    } else if version == CityGmlVersion::V3_0 {
        "con"
    } else {
        owner.name.split(':').next()?
    };
    let native = profile(version, &format!("{module}:{source}")).ok()?;
    Some((owner.relation(native)?, native, property_for(native, lod, part)?))
}

// Native relief LoD is integral; flat primitive annotations retain fractional
// source LoDs. A geometry-free ReliefFeature may derive its LoD from components
// only when they agree, unless the source supplies its own explicit LoD.
fn relief_lod(
    id: &str,
    objects: &BTreeMap<String, Object<'_>>,
    children: &BTreeMap<String, Vec<String>>,
    version: CityGmlVersion,
    depth: usize,
) -> Result<u8> {
    if depth > 128 {
        return Err(invalid("object hierarchy exceeds 128 levels"));
    }
    let object = &objects[id];
    let mut values = object.parts.iter().map(|p| lod_number(&p.0, version)).collect::<Result<HashSet<_>>>()?;
    let explicit = object
        .attributes
        .get("lod")
        .map(|v| {
            let text = v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string());
            lod_number(&text, version)
        })
        .transpose()?;
    if let Some(lod) = explicit {
        values.insert(lod);
    }
    if values.is_empty() {
        for child in children.get(id).into_iter().flatten() {
            if objects[child].native.properties.iter().any(|p| p.name == "dem:lod") {
                values.insert(relief_lod(child, objects, children, version, depth + 1)?);
            }
        }
    }
    if values.len() != 1 {
        return Err(invalid("relief object needs one unambiguous native integer LoD"));
    }
    Ok(*values.iter().next().unwrap())
}

fn render(
    id: &str,
    objects: &BTreeMap<String, Object<'_>>,
    children: &BTreeMap<String, Vec<String>>,
    ids: &BTreeMap<String, String>,
    reference: &Reference,
    version: CityGmlVersion,
    depth: usize,
) -> Result<String> {
    if depth > 128 {
        return Err(invalid("object hierarchy exceeds 128 levels"));
    }
    let object = &objects[id];
    let mut slots: BTreeMap<usize, String> = BTreeMap::new();
    let mut buckets: BTreeMap<usize, Vec<&Part<'_>>> = BTreeMap::new();
    let mut boundaries: BTreeMap<(usize, &'static str, usize), Vec<&Part<'_>>> = BTreeMap::new();
    for item @ (lod, sem, part) in &object.parts {
        if let Some((parent, boundary, index)) = boundary_for(object.native, sem, lod_number(lod, version)?, *part, version) {
            boundaries.entry((parent, boundary.name, index)).or_default().push(item);
            continue;
        }
        let index = property_for(object.native, lod_number(lod, version)?, *part).ok_or_else(|| {
            invalid(format!("{} cannot represent {} at LoD {lod} in CityGML {}", object.native.name, part.kind(), version.as_str()))
        })?;
        buckets.entry(index).or_default().push(item);
    }
    for (index, parts) in buckets {
        slots.insert(index, geometry(&object.native.properties[index], &parts, reference)?);
    }
    for ((parent, boundary, index), parts) in boundaries {
        let native = profile(version, boundary)?;
        let property = &object.native.properties[parent];
        let body = geometry(&native.properties[index], &parts, reference)?;
        write!(slots.entry(parent).or_default(), "<{0}><{boundary}>{body}</{boundary}></{0}>", property.name).unwrap();
    }
    if let Some(index) = object.native.properties.iter().position(|p| p.name == "dem:lod") {
        slots.insert(index, format!("<dem:lod>{}</dem:lod>", relief_lod(id, objects, children, version, depth)?));
    }
    for child in children.get(id).into_iter().flatten() {
        let child_body = render(child, objects, children, ids, reference, version, depth + 1)?;
        let (index, child_body) = if let Some(index) = object.native.relation(objects[child].native) {
            (index, child_body)
        } else if version == CityGmlVersion::V2_0 && matches!(objects[child].source_type.as_str(), "Door" | "Window") {
            let module = object.native.name.split(':').next().unwrap();
            let wall = profile(version, &format!("{module}:WallSurface"))?;
            let parent = object.native.relation(wall).ok_or_else(|| invalid("opening has no supported boundary owner"))?;
            let opening = wall.relation(objects[child].native).ok_or_else(|| invalid("opening has an incompatible boundary owner"))?;
            (parent, format!("<{0}><{1}>{child_body}</{1}></{0}>", wall.name, wall.properties[opening].name))
        } else {
            return Err(invalid(format!("{} cannot contain {}", object.native.name, objects[child].native.name)));
        };
        let property = &object.native.properties[index];
        if !property.many && slots.contains_key(&index) {
            return Err(invalid(format!("{} accepts only one child", property.name)));
        }
        write!(slots.entry(index).or_default(), "<{0}>{1}</{0}>", property.name, child_body).unwrap();
    }
    for (index, p) in object.native.properties.iter().enumerate() {
        if p.required && !slots.contains_key(&index) {
            return Err(invalid(format!("{} requires {}", object.native.name, p.name)));
        }
    }
    let metadata =
        generic_metadata(&json!({"version":1,"attributes":object.attributes,"city_id":id,"city_type":object.source_type}), version)?;
    Ok(format!(
        "<{tag} gml:id=\"{id}\">{metadata}{body}</{tag}>",
        tag = object.native.name,
        id = xml_text(&ids[id])?,
        body = slots.into_values().collect::<String>()
    ))
}

pub(crate) fn to_bytes(layers: &[Layer], version: CityGmlVersion) -> Result<Vec<u8>> {
    if layers.is_empty() {
        return Err(invalid("no layers to write"));
    }
    let objects = objects(layers, version)?;
    let reference = references(layers)?;
    let ids = xml_ids(&objects);
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, object) in &objects {
        if object.parents.len() > 1 {
            return Err(invalid(format!("{id} has multiple owning parents")));
        }
        for parent in &object.parents {
            if !objects.contains_key(parent) {
                return Err(invalid(format!("missing parent {parent} for {id}")));
            }
            let mut current = Some(parent.as_str());
            let mut visited = HashSet::from([id.as_str()]);
            while let Some(p) = current {
                if !visited.insert(p) {
                    return Err(invalid("cyclic object parents"));
                }
                current = objects.get(p).ok_or_else(|| invalid(format!("missing parent {p}")))?.parents.first().map(String::as_str);
            }
            children.entry(parent.clone()).or_default().push(id.clone());
        }
    }
    let v = version.as_str();
    let gml = if version == CityGmlVersion::V2_0 { "http://www.opengis.net/gml" } else { "http://www.opengis.net/gml/3.2" };
    let modules = [
        ("bldg", "building", "building.xsd"),
        ("brid", "bridge", "bridge.xsd"),
        ("tun", "tunnel", "tunnel.xsd"),
        ("gen", "generics", "generics.xsd"),
        ("frn", "cityfurniture", "cityFurniture.xsd"),
        ("grp", "cityobjectgroup", "cityObjectGroup.xsd"),
        ("luse", "landuse", "landUse.xsd"),
        ("veg", "vegetation", "vegetation.xsd"),
        ("dem", "relief", "relief.xsd"),
        ("wtr", "waterbody", "waterBody.xsd"),
        ("tran", "transportation", "transportation.xsd"),
    ];
    let mut xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?><core:CityModel xmlns:core=\"http://www.opengis.net/citygml/{v}\" xmlns:gml=\"{gml}\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\""
    );
    let mut locations = Vec::new();
    let used: HashSet<_> = objects.values().filter_map(|o| o.native.name.split(':').next()).chain(["gen"]).collect();
    for (prefix, module, file) in modules {
        write!(xml, " xmlns:{prefix}=\"http://www.opengis.net/citygml/{module}/{v}\"").unwrap();
        if used.contains(prefix) {
            locations.push(format!("http://www.opengis.net/citygml/{module}/{v} https://schemas.opengis.net/citygml/{module}/{v}/{file}"));
        }
    }
    if version == CityGmlVersion::V3_0 {
        xml.push_str(" xmlns:con=\"http://www.opengis.net/citygml/construction/3.0\"");
        locations.push(
            "http://www.opengis.net/citygml/construction/3.0 https://schemas.opengis.net/citygml/construction/3.0/construction.xsd".into(),
        );
    }
    write!(xml, " xsi:schemaLocation=\"{}\">", locations.join(" ")).unwrap();
    if objects.values().all(|o| o.parts.is_empty())
        && let Some(name) = &reference.name
    {
        write!(xml, "<gml:name codeSpace=\"urn:terrio:citygml:crs\">{}</gml:name>", xml_text(name)?).unwrap();
    }
    // Each native geometry declares its own CRS; a model envelope is unnecessary.
    for (id, object) in &objects {
        if object.parents.is_empty() {
            write!(
                xml,
                "<core:cityObjectMember>{}</core:cityObjectMember>",
                render(id, &objects, &children, &ids, &reference, version, 0)?
            )
            .unwrap();
        }
    }
    xml.push_str("</core:CityModel>");
    Ok(xml.into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{Feature, Geometry, Ring};
    #[test]
    fn geographic_authority_axes_and_extreme_finite_positions_are_explicit() {
        let point = |x, y, z| Feature { fid: 0, attributes: vec![], geometry: Some(Geometry::Point(Coord { x, y, z: Some(z), m: None })) };
        let mut layer = Layer::new("point");
        SpatialRef::parse("EPSG:4326").unwrap().apply_to_layer(&mut layer);
        layer.push(point(4.0, 52.0, 31.0));
        for version in [CityGmlVersion::V2_0, CityGmlVersion::V3_0] {
            let data = to_bytes(std::slice::from_ref(&layer), version).unwrap();
            assert!(std::str::from_utf8(&data).unwrap().contains("<gml:pos>52 4 31</gml:pos>"));
            assert_eq!(super::super::citygml::parse(&data).unwrap()[0].features[0].geometry, layer.features[0].geometry);
            let mut extreme = Layer::new("extreme");
            extreme.push(point(f64::MAX, -f64::MAX, f64::from_bits(1)));
            let data = to_bytes(&[extreme.clone()], version).unwrap();
            assert_eq!(super::super::citygml::parse(&data).unwrap()[0].features[0].geometry, extreme.features[0].geometry);
        }
    }

    #[test]
    fn typed_faces_keep_fractional_lod_invalid_xml_ids_and_typed_json_attributes() {
        let doc = json!({"type":"CityJSON","version":"2.0","transform":{"scale":[1,1,1],"translate":[0,0,0]},"vertices":[[10,0,0],[10,1,0],[10,1,1]],"CityObjects":{"house":{"type":"Building","children":["1 bad"]},"1 bad":{"type":"BuildingPart","parents":["house"],"attributes":{"nested":[1,true],"label":"A<&B","controls":"\u{0000}\u{fffe}"},"geometry":[{"type":"MultiSurface","lod":"2.2","boundaries":[[[0,1,2]]],"semantics":{"surfaces":[{"type":"WallSurface","label":"brick & stone"}],"values":[0]}}]}}});
        let source = super::super::cityjson::parse(&serde_json::to_vec(&doc).unwrap()).unwrap();
        for version in [CityGmlVersion::V2_0, CityGmlVersion::V3_0] {
            let output = to_bytes(&source, version).unwrap();
            assert!(!std::str::from_utf8(&output).unwrap().contains("gml:id=\"1 bad\""));
            let actual = super::super::citygml::parse(&output).unwrap();
            for expected in &source[0].features {
                let found = actual[0].features.iter().find(|f| f.attributes[0] == expected.attributes[0]).unwrap();
                for index in [0, 1, 2, 3, 4, 7] {
                    if index == 4 && expected.attributes[index] == FieldValue::Null {
                        assert_eq!(found.attributes[index], FieldValue::Text("{}".into()));
                    } else {
                        assert_eq!(found.attributes[index], expected.attributes[index], "version {}, column {index}", version.as_str());
                    }
                }
                assert_eq!(found.geometry, expected.geometry);
            }
        }
    }

    #[test]
    fn tin_patches_keep_individual_fractional_lods_and_semantics() {
        let doc = json!({"type":"CityJSON","version":"2.0","transform":{"scale":[1,1,1],"translate":[0,0,0]},
            "vertices":[[0,0,0],[1,0,1],[0,1,2],[1,1,3]],"CityObjects":{
                "terrain":{"type":"ReliefFeature","children":["tin"]},
                "tin":{"type":"TINRelief","parents":["terrain"],"geometry":[
                    {"type":"MultiSurface","lod":"2.1","boundaries":[[[0,1,2]]],
                     "semantics":{"surfaces":[{"type":"GroundSurface","material":"rock"}],"values":[0]}},
                    {"type":"MultiSurface","lod":"2.2","boundaries":[[[1,3,2]]],
                     "semantics":{"surfaces":[{"type":"GroundSurface","material":"sand"}],"values":[0]}}]}}});
        let source = super::super::cityjson::parse(&serde_json::to_vec(&doc).unwrap()).unwrap();
        for version in [CityGmlVersion::V2_0, CityGmlVersion::V3_0] {
            let output = to_bytes(&source, version).unwrap();
            let xml = std::str::from_utf8(&output).unwrap();
            assert_eq!(xml.matches("<dem:lod>2</dem:lod>").count(), 2);
            let actual = super::super::citygml::parse(&output).unwrap();
            for expected in source[0].features.iter().filter(|f| f.geometry.is_some()) {
                let found = actual[0].features.iter().find(|f| f.attributes[2] == expected.attributes[2]).unwrap();
                assert_eq!(found.geometry, expected.geometry);
                assert_eq!(found.attributes[7], expected.attributes[7]);
            }
            let corrupted = xml;
            // Remove a native patch while retaining its annotation table.
            let start = corrupted.find("<gml:Triangle>").unwrap();
            let end = start + corrupted[start..].find("</gml:Triangle>").unwrap() + "</gml:Triangle>".len();
            let corrupted = format!("{}{}", &corrupted[..start], &corrupted[end..]);
            assert!(super::super::citygml::parse(corrupted.as_bytes()).unwrap_err().to_string().contains("annotation count"));
        }
    }

    #[test]
    fn nonspatial_models_keep_crs_without_inventing_an_envelope() {
        let mut layer = Layer::new("empty");
        SpatialRef::parse("EPSG:4326").unwrap().apply_to_layer(&mut layer);
        for version in [CityGmlVersion::V2_0, CityGmlVersion::V3_0] {
            let output = to_bytes(std::slice::from_ref(&layer), version).unwrap();
            assert!(!std::str::from_utf8(&output).unwrap().contains("Envelope"));
            assert_eq!(super::super::citygml::parse(&output).unwrap()[0].crs_epsg(), Some(4326));
        }
    }

    #[test]
    fn invalid_hierarchy_and_wrong_format_options_fail_without_creating_output() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wrong.geojson");
        let options = crate::VectorWriteOptions { citygml_version: Some(CityGmlVersion::V2_0), ..Default::default() };
        assert!(
            crate::write_vector(&[Layer::new("empty")], path.to_str().unwrap(), &options)
                .unwrap_err()
                .to_string()
                .contains("citygml_version")
        );
        assert!(!path.exists());
        let doc = json!({"type":"CityJSON","version":"2.0","transform":{"scale":[1,1,1],"translate":[0,0,0]},"vertices":[],"CityObjects":{"part":{"type":"BuildingPart","parents":["missing"]}}});
        let source = super::super::cityjson::parse(&serde_json::to_vec(&doc).unwrap()).unwrap();
        assert!(to_bytes(&source, CityGmlVersion::V3_0).unwrap_err().to_string().contains("missing parent"));
    }

    #[test]
    fn generic_xyz_faces_and_json_metadata_write_both_native_versions() {
        let mut layer = Layer::new("ordinary");
        layer.push(Feature {
            fid: 0,
            attributes: vec![],
            geometry: Some(Geometry::Polygon {
                exterior: Ring::new(vec![
                    Coord { x: 10.0, y: 0.0, z: Some(0.0), m: None },
                    Coord { x: 10.0, y: 1.0, z: Some(0.0), m: None },
                    Coord { x: 10.0, y: 1.0, z: Some(1.0), m: None },
                ]),
                interiors: vec![],
            }),
        });
        for version in [CityGmlVersion::V2_0, CityGmlVersion::V3_0] {
            let output = to_bytes(std::slice::from_ref(&layer), version).unwrap();
            let layers = super::super::citygml::parse(&output).unwrap();
            assert_eq!(layers[0].features.len(), 1);
            let Geometry::Polygon { exterior, .. } = layers[0].features[0].geometry.as_ref().unwrap() else { panic!() };
            assert_eq!(exterior.0[2].z, Some(1.0));
        }
    }
}

// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! CityJSON encoding of the existing flat vector view, according to the published schemas.
//! GDAL 3.13.3 has no CityJSON writer and is not a conformance reference.
use crate::{Error, Result, crs::SpatialRef};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use wbvector::{Coord, Feature, FieldType, FieldValue, Geometry, Layer, Ring};

/// The CityJSON version to write. The current default is 2.0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CityJsonVersion {
    /// CityJSON 1.1. GenericCityObject requires CityJSON 2.0.
    V1_1,
    /// CityJSON 2.0.
    #[default]
    V2_0,
}
impl CityJsonVersion {
    /// The minor version identifier stored in the document.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1_1 => "1.1",
            Self::V2_0 => "2.0",
        }
    }
}
impl std::str::FromStr for CityJsonVersion {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        match value {
            "1.1" => Ok(Self::V1_1),
            "2.0" => Ok(Self::V2_0),
            _ => Err(invalid("version must be 1.1 or 2.0")),
        }
    }
}
fn invalid(message: impl std::fmt::Display) -> Error {
    Error::InvalidArgument(format!("CityJSON writer: {message}"))
}

#[derive(Clone, Copy)]
enum Primitive<'a> {
    Point(&'a Coord),
    Line(&'a [Coord]),
    Surface(&'a Ring, &'a [Ring]),
}
impl Primitive<'_> {
    fn positions(&self, mut f: impl FnMut(&Coord) -> Result<()>) -> Result<()> {
        match self {
            Self::Point(p) => f(p)?,
            Self::Line(line) => {
                for p in *line {
                    f(p)?;
                }
            }
            Self::Surface(exterior, holes) => {
                for ring in std::iter::once(*exterior).chain(holes.iter()) {
                    for p in &ring.0 {
                        f(p)?;
                    }
                }
            }
        }
        Ok(())
    }
    fn kind(self) -> &'static str {
        match self {
            Self::Point(_) => "MultiPoint",
            Self::Line(_) => "MultiLineString",
            Self::Surface(_, _) => "MultiSurface",
        }
    }
}
fn flatten<'a>(geometry: &'a Geometry, depth: usize, out: &mut Vec<Primitive<'a>>) -> Result<()> {
    if depth > 256 {
        return Err(invalid("geometry nesting exceeds 256"));
    }
    match geometry {
        Geometry::Point(p) => out.push(Primitive::Point(p)),
        Geometry::MultiPoint(ps) => out.extend(ps.iter().map(Primitive::Point)),
        Geometry::LineString(ps) => out.push(Primitive::Line(ps)),
        Geometry::MultiLineString(lines) => out.extend(lines.iter().map(|p| Primitive::Line(p))),
        Geometry::Polygon { exterior, interiors } => out.push(Primitive::Surface(exterior, interiors)),
        Geometry::MultiPolygon(polygons) => out.extend(polygons.iter().map(|(e, h)| Primitive::Surface(e, h))),
        Geometry::GeometryCollection(geometries) => {
            for g in geometries {
                flatten(g, depth + 1, out)?;
            }
        }
    }
    Ok(())
}
fn xyz(p: &Coord) -> Result<[f64; 3]> {
    let values = [p.x, p.y, p.z.unwrap_or(0.0)];
    if values.iter().any(|v| !v.is_finite()) {
        return Err(invalid("coordinates must be finite"));
    }
    if p.m.is_some() {
        return Err(invalid("CityJSON has XYZ coordinates; remove measure coordinates before writing"));
    }
    Ok(values)
}
struct Vertices {
    offset: [f64; 3],
    scale: [f64; 3],
    values: Vec<[i64; 3]>,
    indices: HashMap<[i64; 3], usize>,
}
impl Vertices {
    fn new(objects: &BTreeMap<String, Object<'_>>) -> Result<Self> {
        let mut lo = [f64::INFINITY; 3];
        let mut hi = [f64::NEG_INFINITY; 3];
        for object in objects.values() {
            for (_, _, primitive) in &object.primitives {
                primitive.positions(|p| {
                    for (i, v) in xyz(p)?.into_iter().enumerate() {
                        lo[i] = lo[i].min(v);
                        hi[i] = hi[i].max(v);
                    }
                    Ok(())
                })?;
            }
        }
        let offset = std::array::from_fn(|i| if lo[i].is_finite() { lo[i] / 2.0 + hi[i] / 2.0 } else { 0.0 });
        let scale = std::array::from_fn(|i| {
            if !lo[i].is_finite() || lo[i] == hi[i] {
                return 1.0;
            }
            let extent = (hi[i] - offset[i]).abs().max((lo[i] - offset[i]).abs());
            let raw = extent / ((1u64 << 51) - 1) as f64;
            raw.max(f64::from_bits(1)).log2().ceil().exp2().max(f64::from_bits(1))
        });
        Ok(Self { offset, scale, values: Vec::new(), indices: HashMap::new() })
    }
    fn add(&mut self, p: &Coord) -> Result<usize> {
        let p = xyz(p)?;
        let mut q = [0; 3];
        for i in 0..3 {
            let v = ((p[i] - self.offset[i]) / self.scale[i]).round();
            if !v.is_finite() || v.abs() > (1u64 << 52) as f64 {
                return Err(invalid("coordinate range exceeds safe integer quantization"));
            }
            q[i] = v as i64;
            if !(q[i] as f64 * self.scale[i] + self.offset[i]).is_finite() {
                q[i] -= q[i].signum();
            }
            if !(q[i] as f64 * self.scale[i] + self.offset[i]).is_finite() {
                return Err(invalid("coordinate reconstruction overflows"));
            }
        }
        if let Some(index) = self.indices.get(&q) {
            return Ok(*index);
        }
        let index = self.values.len();
        self.values.push(q);
        self.indices.insert(q, index);
        Ok(index)
    }
    fn line(&mut self, points: &[Coord], ring: bool) -> Result<Vec<usize>> {
        let mut indices = points.iter().map(|p| self.add(p)).collect::<Result<Vec<_>>>()?;
        if ring && indices.len() > 1 && indices.first() == indices.last() {
            indices.pop();
        }
        let distinct = indices.iter().collect::<HashSet<_>>().len();
        let minimum = if ring { 3 } else { 2 };
        if distinct < minimum {
            let source_distinct = points
                .iter()
                .map(|p| xyz(p).map(|v| v.map(|x| if x == 0.0 { 0 } else { x.to_bits() })))
                .collect::<Result<HashSet<_>>>()?
                .len();
            if source_distinct < minimum {
                return Err(invalid(format!("a ring or line needs at least {minimum} distinct source XYZ positions")));
            }
            return Err(invalid("a ring or line collapses under integer quantization"));
        }
        Ok(indices)
    }
    fn encode(&mut self, primitive: Primitive<'_>) -> Result<Value> {
        Ok(match primitive {
            Primitive::Point(p) => json!(self.add(p)?),
            Primitive::Line(line) => json!(self.line(line, false)?),
            Primitive::Surface(exterior, holes) => Value::Array(
                std::iter::once(exterior).chain(holes.iter()).map(|r| Ok(json!(self.line(&r.0, true)?))).collect::<Result<_>>()?,
            ),
        })
    }
}
struct Object<'a> {
    layer: usize,
    kind: String,
    attrs: Map<String, Value>,
    parents: Vec<String>,
    primitives: Vec<(Rc<str>, Option<Rc<Value>>, Primitive<'a>)>,
}
fn field<'a>(layer: &Layer, feature: &'a Feature, name: &str) -> &'a FieldValue {
    layer.schema.field_index(name).and_then(|i| feature.attributes.get(i)).unwrap_or(&FieldValue::Null)
}
fn json_field(value: &FieldValue) -> Result<Value> {
    match value {
        FieldValue::Null => Ok(Value::Null),
        FieldValue::Text(s) => serde_json::from_str(s).map_err(invalid),
        _ => Err(invalid("flat JSON metadata fields must contain JSON text")),
    }
}
fn property(value: &FieldValue, kind: FieldType) -> Result<Value> {
    Ok(match value {
        FieldValue::Null => Value::Null,
        FieldValue::Integer(v) => json!(v),
        FieldValue::Float(v) if v.is_finite() => json!(v),
        FieldValue::Float(_) => return Err(invalid("attribute numbers must be finite")),
        FieldValue::Boolean(v) => json!(v),
        FieldValue::Text(v) if kind == FieldType::Json => serde_json::from_str(v).map_err(invalid)?,
        FieldValue::Text(v) | FieldValue::Date(v) | FieldValue::DateTime(v) => json!(v),
        FieldValue::Blob(v) => json!(v.iter().map(|x| format!("{x:02X}")).collect::<String>()),
    })
}
const COMMON_FIELDS: &[&str] = &["city_id", "city_type", "lod", "parents", "attributes", "geometry_type", "semantics", "surface_semantics"];
fn has_city_fields(layer: &Layer) -> bool {
    layer.schema.field_index("city_id").is_some() && layer.schema.field_index("city_type").is_some()
}

const KINDS: &[&str] = &[
    "OtherConstruction",
    "Building",
    "BuildingPart",
    "BuildingInstallation",
    "BuildingConstructiveElement",
    "BuildingFurniture",
    "BuildingRoom",
    "BuildingUnit",
    "BuildingStorey",
    "CityObjectGroup",
    "GenericCityObject",
    "Tunnel",
    "TunnelPart",
    "TunnelInstallation",
    "TunnelConstructiveElement",
    "TunnelHollowSpace",
    "TunnelFurniture",
    "Bridge",
    "BridgePart",
    "BridgeInstallation",
    "BridgeConstructiveElement",
    "BridgeRoom",
    "BridgeFurniture",
    "Road",
    "Railway",
    "TransportSquare",
    "Waterway",
    "TINRelief",
    "WaterBody",
    "PlantCover",
    "SolitaryVegetationObject",
    "LandUse",
    "CityFurniture",
];
fn kind(name: &str, version: CityJsonVersion) -> Result<String> {
    let mapped = match name {
        "Door" | "Window" => "BuildingConstructiveElement",
        "Room" => "BuildingRoom",
        "IntBuildingInstallation" => "BuildingInstallation",
        "IntBridgeInstallation" => "BridgeInstallation",
        "IntTunnelInstallation" => "TunnelInstallation",
        "Storey" => "BuildingStorey",
        "BridgeConstructionElement" => "BridgeConstructiveElement",
        "HollowSpace" | "TunnelRoom" => "TunnelHollowSpace",
        "Square" => "TransportSquare",
        "GenericOccupiedSpace" | "GenericUnoccupiedSpace" => "GenericCityObject",
        other => other,
    };
    if !KINDS.contains(&mapped) || mapped == "GenericCityObject" && version == CityJsonVersion::V1_1 {
        return Err(invalid(format!("object type {name} cannot be represented in CityJSON {}", version.as_str())));
    }
    Ok(mapped.into())
}
fn point_kind(kind: &str) -> bool {
    matches!(
        kind,
        "OtherConstruction"
            | "BuildingInstallation"
            | "BuildingConstructiveElement"
            | "BuildingFurniture"
            | "CityObjectGroup"
            | "GenericCityObject"
            | "TunnelInstallation"
            | "TunnelConstructiveElement"
            | "TunnelFurniture"
            | "BridgeInstallation"
            | "BridgeConstructiveElement"
            | "BridgeFurniture"
            | "SolitaryVegetationObject"
            | "CityFurniture"
    )
}
fn allowed(kind: &str, primitive: Primitive<'_>) -> bool {
    match primitive {
        Primitive::Surface(_, _) => true,
        Primitive::Point(_) => point_kind(kind),
        Primitive::Line(_) => point_kind(kind) || matches!(kind, "Road" | "Railway" | "TransportSquare" | "Waterway" | "WaterBody"),
    }
}
fn lod(value: &FieldValue, version: CityJsonVersion) -> Result<String> {
    let value = match value {
        FieldValue::Null => "0",
        FieldValue::Text(s) if s.is_empty() => "0",
        FieldValue::Text(s) => s,
        _ => return Err(invalid("LoD must be text")),
    };
    let bytes = value.as_bytes();
    let valid =
        (bytes.len() == 1 || bytes.len() == 3 && bytes[1] == b'.') && bytes.iter().enumerate().all(|(i, b)| i == 1 || b.is_ascii_digit());
    if !valid || version == CityJsonVersion::V2_0 && (bytes[0] > b'3' || bytes.len() == 3 && bytes[2] > b'3') {
        return Err(invalid(format!("invalid LoD {value:?} for {}", version.as_str())));
    }
    Ok(value.into())
}
fn semantic(value: &FieldValue) -> Result<Option<Value>> {
    let mut value = json_field(value)?;
    if value.is_null() {
        return Ok(None);
    }
    let object = value.as_object_mut().ok_or_else(|| invalid("surface_semantics must be a JSON object"))?;
    let known = [
        "RoofSurface",
        "GroundSurface",
        "WallSurface",
        "ClosureSurface",
        "OuterCeilingSurface",
        "OuterFloorSurface",
        "Window",
        "Door",
        "InteriorWallSurface",
        "CeilingSurface",
        "FloorSurface",
        "WaterSurface",
        "WaterGroundSurface",
        "WaterClosureSurface",
        "TrafficArea",
        "AuxiliaryTrafficArea",
        "TransportationHole",
        "TransportationMarking",
    ];
    if !object.get("type").and_then(Value::as_str).is_some_and(|v| known.contains(&v)) {
        return Err(invalid("surface semantics require a supported core type"));
    }
    // These are indices into the source semantic table, not per-face attributes.
    object.remove("parent");
    object.remove("children");
    Ok(Some(value))
}
fn objects(layers: &[Layer], version: CityJsonVersion) -> Result<BTreeMap<String, Object<'_>>> {
    let mut objects: BTreeMap<String, Object<'_>> = BTreeMap::new();
    let mut reserved = HashSet::new();
    let mut metadata: HashMap<String, (usize, Vec<&FieldValue>)> = HashMap::new();
    let mut semantic_cache: HashMap<&str, Option<Rc<Value>>> = HashMap::new();
    for layer in layers {
        if has_city_fields(layer) {
            for f in &layer.features {
                if let FieldValue::Text(id) = field(layer, f, "city_id") {
                    reserved.insert(id.clone());
                }
            }
        }
    }
    for (li, layer) in layers.iter().enumerate() {
        let flat = has_city_fields(layer);
        for (fi, f) in layer.features.iter().enumerate() {
            let id = if flat {
                match field(layer, f, "city_id") {
                    FieldValue::Text(id) if !id.is_empty() => Some(id.clone()),
                    FieldValue::Null => None,
                    _ => return Err(invalid("city_id must be nonempty text")),
                }
            } else {
                None
            };
            let id = id.unwrap_or_else(|| {
                let mut id = format!("_terrio_{li}_{fi}");
                while reserved.contains(&id) {
                    id.push('_');
                }
                reserved.insert(id.clone());
                id
            });
            let source_kind = if flat {
                match field(layer, f, "city_type") {
                    FieldValue::Text(s) => s.as_str(),
                    FieldValue::Null => "GenericCityObject",
                    _ => return Err(invalid("city_type must be text")),
                }
            } else {
                "GenericCityObject"
            };
            let object_kind = kind(source_kind, version)?;
            let raw: Vec<&FieldValue> = layer
                .schema
                .fields()
                .iter()
                .zip(&f.attributes)
                .filter(|(def, _)| {
                    !matches!(
                        def.name.as_str(),
                        "lod" | "geometry_type" | "surface_semantics" | "semantics" | "skipped_rings" | "skipped_faces"
                    )
                })
                .map(|(_, value)| value)
                .collect();
            if !metadata.get(&id).is_some_and(|(layer, values)| *layer == li && values == &raw) {
                let parents = if flat {
                    let value = json_field(field(layer, f, "parents"))?;
                    if value.is_null() { Vec::new() } else { serde_json::from_value::<Vec<String>>(value).map_err(invalid)? }
                } else {
                    Vec::new()
                };
                let mut attrs = if flat {
                    match json_field(field(layer, f, "attributes"))? {
                        Value::Null => Map::new(),
                        Value::Object(map) => map,
                        _ => return Err(invalid("attributes must be a JSON object")),
                    }
                } else {
                    Map::new()
                };
                for (def, value) in layer.schema.fields().iter().zip(&f.attributes) {
                    if flat
                        && (COMMON_FIELDS.contains(&def.name.as_str()) || matches!(def.name.as_str(), "skipped_rings" | "skipped_faces"))
                    {
                        continue;
                    }
                    let value = property(value, def.field_type)?;
                    if attrs.get(&def.name).is_some_and(|previous| previous != &value) {
                        return Err(invalid(format!("conflicting attribute {} on {id}", def.name)));
                    }
                    attrs.insert(def.name.clone(), value);
                }
                if source_kind != object_kind {
                    if attrs.get("terrio:sourceCityType").is_some_and(|value| value != &json!(source_kind)) {
                        return Err(invalid("terrio:sourceCityType conflicts with the source classification"));
                    }
                    attrs.insert("terrio:sourceCityType".into(), json!(source_kind));
                }
                let entry = objects.entry(id.clone()).or_insert_with(|| Object {
                    layer: li,
                    kind: object_kind.clone(),
                    attrs: attrs.clone(),
                    parents: parents.clone(),
                    primitives: Vec::new(),
                });
                if entry.layer != li || entry.kind != object_kind || entry.attrs != attrs || entry.parents != parents {
                    return Err(invalid(format!("conflicting metadata for object {id}")));
                }
                metadata.insert(id.clone(), (li, raw));
            }
            let entry = objects.get_mut(&id).unwrap();
            if let Some(g) = &f.geometry {
                let lod: Rc<str> = Rc::from(lod(if flat { field(layer, f, "lod") } else { &FieldValue::Null }, version)?);
                let semantic = if flat {
                    match field(layer, f, "surface_semantics") {
                        FieldValue::Text(text) => {
                            if let Some(value) = semantic_cache.get(text.as_str()) {
                                value.clone()
                            } else {
                                let value = semantic(&FieldValue::Text(text.clone()))?.map(Rc::new);
                                semantic_cache.insert(text, value.clone());
                                value
                            }
                        }
                        value => semantic(value)?.map(Rc::new),
                    }
                } else {
                    None
                };
                let mut parts = Vec::new();
                flatten(g, 0, &mut parts)?;
                for p in parts {
                    if !allowed(&object_kind, p) {
                        return Err(invalid(format!("{} is not permitted for {object_kind}", p.kind())));
                    }
                    if !matches!(p, Primitive::Surface(_, _)) && semantic.is_some() {
                        return Err(invalid("surface semantics attached to a point or line"));
                    }
                    if object_kind == "TINRelief"
                        && matches!(p,Primitive::Surface(e,h) if !h.is_empty() || e.0.iter().map(|c|(c.x.to_bits(),c.y.to_bits(),c.z.map(f64::to_bits))).collect::<HashSet<_>>().len()!=3)
                    {
                        return Err(invalid("TINRelief requires triangular faces without holes"));
                    }
                    entry.primitives.push((lod.clone(), semantic.clone(), p));
                }
            }
        }
    }
    Ok(objects)
}
fn children(objects: &BTreeMap<String, Object<'_>>, version: CityJsonVersion) -> Result<BTreeMap<String, Vec<String>>> {
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut indegree = HashMap::new();
    for (id, object) in objects {
        let child = matches!(
            object.kind.as_str(),
            "BuildingPart"
                | "BuildingInstallation"
                | "BuildingConstructiveElement"
                | "BuildingFurniture"
                | "BuildingRoom"
                | "BuildingUnit"
                | "BuildingStorey"
                | "BridgePart"
                | "BridgeInstallation"
                | "BridgeConstructiveElement"
                | "BridgeRoom"
                | "BridgeFurniture"
                | "TunnelPart"
                | "TunnelInstallation"
                | "TunnelConstructiveElement"
                | "TunnelHollowSpace"
                | "TunnelFurniture"
        );
        if child && object.parents.len() != 1 || !child && !object.parents.is_empty() {
            return Err(invalid(format!("invalid parent count for {} {id}", object.kind)));
        }
        let mut seen = HashSet::new();
        for parent in &object.parents {
            if !seen.insert(parent) || !objects.contains_key(parent) {
                return Err(invalid(format!("duplicate or missing parent {parent} for {id}")));
            }
            let family = ["Building", "Bridge", "Tunnel"].into_iter().find(|family| object.kind.starts_with(family));
            if family.is_some_and(|family| !objects[parent].kind.starts_with(family)) {
                return Err(invalid(format!("parent {parent} has an incompatible type for {} {id}", object.kind)));
            }
            let parent_kind = objects[parent].kind.as_str();
            let compatible = match object.kind.as_str() {
                "BuildingInstallation" | "BuildingConstructiveElement" | "BuildingFurniture" | "BuildingStorey" => {
                    matches!(parent_kind, "Building" | "BuildingPart" | "BuildingRoom")
                }
                "BridgeInstallation" | "BridgeFurniture" => matches!(parent_kind, "Bridge" | "BridgePart" | "BridgeRoom"),
                "TunnelInstallation" | "TunnelConstructiveElement" | "TunnelHollowSpace" | "TunnelFurniture" => {
                    matches!(parent_kind, "Tunnel" | "TunnelPart")
                }
                _ => true,
            };
            if version == CityJsonVersion::V2_0 && !compatible {
                return Err(invalid(format!("parent {parent} has an incompatible type for {} {id}", object.kind)));
            }
            children.entry(parent.clone()).or_default().push(id.clone());
        }
        indegree.insert(id.clone(), object.parents.len());
    }
    let mut roots: VecDeque<_> = indegree.iter().filter(|(_, v)| **v == 0).map(|(id, _)| id.clone()).collect();
    let mut count = 0;
    while let Some(id) = roots.pop_front() {
        count += 1;
        if let Some(ids) = children.get(&id) {
            for child in ids {
                let remaining = indegree.get_mut(child).unwrap();
                *remaining -= 1;
                if *remaining == 0 {
                    roots.push_back(child.clone());
                }
            }
        }
    }
    if count != objects.len() {
        return Err(invalid("cyclic object parents"));
    }
    Ok(children)
}
fn crs(layers: &[Layer]) -> Result<Option<u32>> {
    let registered = |layer: &Layer| -> Result<Option<u32>> {
        if layer.crs_epsg().is_none() && layer.crs_wkt().is_none() {
            return Ok(None);
        }
        let source = SpatialRef::from_layer(layer);
        // Compound EPSG codes need the complete CRS registry. Only accept an
        // inferred code here when its full definition equals the stored WKT.
        let declared = source.epsg.filter(|code| {
            !crate::crs::is_esri_code(*code)
                && layer.crs_wkt().is_none_or(|wkt| SpatialRef::from_epsg(*code).ok().and_then(|s| s.to_wkt()).as_deref() == Some(wkt))
        });
        let code = declared
            .or_else(|| super::trusted_epsg(layer))
            .ok_or_else(|| invalid("the declared CRS has no EPSG URI; reproject to a registered CRS before writing"))?;
        if !source.same_as(&SpatialRef::from_epsg(code)?) {
            return Err(invalid("the declared CRS cannot be represented by its EPSG code"));
        }
        Ok(Some(code))
    };
    let first = registered(&layers[0])?;
    for layer in &layers[1..] {
        let other = registered(layer)?;
        let same = match (first, other) {
            (None, None) => true,
            (Some(a), Some(b)) => a == b || SpatialRef::from_epsg(a)?.same_as(&SpatialRef::from_epsg(b)?),
            _ => false,
        };
        if !same {
            return Err(invalid("layers must have one consistent CRS"));
        }
    }
    Ok(first)
}

type GeometryBuckets<'a> = BTreeMap<(Rc<str>, &'static str), Vec<(Option<Rc<Value>>, Primitive<'a>)>>;

/// Encode the flat layers; surfaces become MultiSurface, never reconstructed solids.
pub(crate) fn to_bytes(layers: &[Layer], version: CityJsonVersion) -> Result<Vec<u8>> {
    if layers.is_empty() {
        return Err(invalid("no layers to write"));
    }
    let crs = crs(layers)?;
    let objects = objects(layers, version)?;
    let children = children(&objects, version)?;
    let mut vertices = Vertices::new(&objects)?;
    let mut encoded = Map::new();
    for (id, object) in objects {
        let mut buckets: GeometryBuckets<'_> = BTreeMap::new();
        for (lod, sem, p) in object.primitives {
            buckets.entry((lod, p.kind())).or_default().push((sem, p));
        }
        let mut geometries = Vec::new();
        for ((lod, kind), parts) in buckets {
            let kind = if object.kind == "TINRelief" { "CompositeSurface" } else { kind };
            let mut boundaries = Vec::new();
            let mut surfaces = Vec::new();
            let mut semantic_indices = HashMap::new();
            let mut values = Vec::new();
            for (semantic, p) in parts {
                boundaries.push(vertices.encode(p)?);
                values.push(if let Some(semantic) = semantic {
                    let index = if let Some(&index) = semantic_indices.get(&semantic) {
                        index
                    } else {
                        let index = surfaces.len();
                        semantic_indices.insert(semantic.clone(), index);
                        surfaces.push((*semantic).clone());
                        index
                    };
                    json!(index)
                } else {
                    Value::Null
                });
            }
            let mut geometry = json!({"type":kind,"lod":lod.as_ref(),"boundaries":boundaries});
            if !surfaces.is_empty() {
                geometry["semantics"] = json!({"surfaces":surfaces,"values":values});
            }
            geometries.push(geometry);
        }
        let mut value = json!({"type":object.kind,"attributes":object.attrs});
        if !object.parents.is_empty() {
            value["parents"] = json!(object.parents);
        }
        if let Some(ids) = children.get(&id) {
            value["children"] = json!(ids);
        } else if object.kind == "CityObjectGroup" {
            value["children"] = json!([]);
        }
        if !geometries.is_empty() {
            value["geometry"] = json!(geometries);
        }
        encoded.insert(id, value);
    }
    let mut doc = json!({"type":"CityJSON","version":version.as_str(),"transform":{"scale":vertices.scale,"translate":vertices.offset},"CityObjects":encoded,"vertices":vertices.values});
    if let Some(code) = crs {
        doc["metadata"] = json!({"referenceSystem":format!("https://www.opengis.net/def/crs/EPSG/0/{code}")});
    }
    serde_json::to_vec(&doc).map_err(invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Vec<Layer> {
        let doc = json!({"type":"CityJSON","version":"2.0","transform":{"scale":[1,1,1],"translate":[0,0,0]},
            "metadata":{"referenceSystem":"https://www.opengis.net/def/crs/EPSG/0/7415"},
            "vertices":[[10,0,0],[10,10,0],[10,10,10],[10,0,10],[10,2,2],[10,3,2],[10,3,3]],
            "CityObjects":{"house":{"type":"Building","children":["part"]},"part":{"type":"BuildingPart","parents":["house"],"attributes":{"name":"α & β"},
                "geometry":[{"type":"Solid","lod":"2.2","boundaries":[[[[0,1,2,3],[4,5,6]]]],"semantics":{"surfaces":[{"type":"WallSurface","material":"brick"}],"values":[[0]]}}]}}});
        super::super::cityjson::parse(&serde_json::to_vec(&doc).unwrap()).unwrap()
    }

    #[test]
    fn both_versions_encode_flat_surfaces_holes_identity_and_classification() {
        for version in [CityJsonVersion::V1_1, CityJsonVersion::V2_0] {
            let source = source();
            let bytes = to_bytes(&source, version).unwrap();
            let doc: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(doc["version"], version.as_str());
            assert_eq!(doc["CityObjects"]["house"]["children"], json!(["part"]));
            assert_eq!(doc["CityObjects"]["part"]["parents"], json!(["house"]));
            assert_eq!(doc["CityObjects"]["part"]["geometry"][0]["type"], "MultiSurface");
            let result = super::super::cityjson::parse(&bytes).unwrap();
            assert_eq!(result[0].crs_epsg(), Some(7415));
            let feature = result[0].features.iter().find(|f| f.geometry.is_some()).unwrap();
            assert_eq!(feature.attributes[4], FieldValue::Text("{\"name\":\"α & β\"}".into()));
            assert_eq!(feature.attributes[7], FieldValue::Text("{\"type\":\"WallSurface\",\"material\":\"brick\"}".into()));
            let Geometry::Polygon { exterior, interiors } = feature.geometry.as_ref().unwrap() else { panic!() };
            assert_eq!(interiors.len(), 1);
            for (actual, expected) in exterior.0.iter().zip([[10., 0., 0.], [10., 10., 0.], [10., 10., 10.], [10., 0., 10.]]) {
                for (a, b) in xyz(actual).unwrap().into_iter().zip(expected) {
                    assert!((a - b).abs() < 1e-12);
                }
            }
        }
    }

    #[test]
    fn many_face_semantics_deduplicate_independently_of_object_key_order() {
        let mut layers = source();
        let template = layers[0].features[1].clone();
        layers[0].features.truncate(1);
        for tag in 0..1024 {
            let mut feature = template.clone();
            feature.attributes[7] = FieldValue::Text(format!("{{\"type\":\"WallSurface\",\"tag\":{tag}}}"));
            layers[0].push(feature);
        }
        let mut duplicate = template;
        duplicate.attributes[7] = FieldValue::Text("{\"tag\":0,\"type\":\"WallSurface\"}".into());
        layers[0].push(duplicate);
        let doc: Value = serde_json::from_slice(&to_bytes(&layers, CityJsonVersion::V2_0).unwrap()).unwrap();
        let semantics = &doc["CityObjects"]["part"]["geometry"][0]["semantics"];
        assert_eq!(semantics["surfaces"].as_array().unwrap().len(), 1024);
        let values = semantics["values"].as_array().unwrap();
        assert_eq!(values.len(), 1025);
        assert_eq!(values.last().unwrap(), 0);
    }

    #[test]
    fn ordinary_xyz_points_and_lines_become_generic_objects() {
        let mut layer = Layer::new("ordinary");
        layer.add_field(wbvector::FieldDef::new("label", FieldType::Text));
        layer.push(Feature {
            fid: 42,
            attributes: vec![FieldValue::Text("point".into())],
            geometry: Some(Geometry::Point(Coord { x: 4.0, y: 52.0, z: Some(30.0), m: None })),
        });
        layer.push(Feature {
            fid: 43,
            attributes: vec![FieldValue::Text("line".into())],
            geometry: Some(Geometry::LineString(vec![
                Coord { x: 0.0, y: 0.0, z: Some(0.0), m: None },
                Coord { x: 1.0, y: 1.0, z: Some(1.0), m: None },
            ])),
        });
        let bytes = to_bytes(&[layer.clone()], CityJsonVersion::V2_0).unwrap();
        let doc: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(doc["CityObjects"]["_terrio_0_0"]["type"], "GenericCityObject");
        assert_eq!(doc["CityObjects"]["_terrio_0_0"]["attributes"]["label"], "point");
        assert!(doc.get("metadata").is_none());
        let result = super::super::cityjson::parse(&bytes).unwrap();
        assert_eq!(result[0].features[0].geometry, layer.features[0].geometry);
        assert_eq!(result[0].features[1].geometry, layer.features[1].geometry);
        assert_eq!(doc["CityObjects"]["_terrio_0_1"]["geometry"][0]["type"], "MultiLineString");
        assert!(to_bytes(&[layer], CityJsonVersion::V1_1).unwrap_err().to_string().contains("GenericCityObject"));
    }

    #[test]
    fn explicit_identity_and_type_fields_enable_typed_layers_without_reader_columns() {
        let mut layer = Layer::new("buildings");
        for name in ["city_id", "city_type", "lod"] {
            layer.add_field(wbvector::FieldDef::new(name, FieldType::Text));
        }
        layer.add_field(wbvector::FieldDef::new("height", FieldType::Float));
        layer.push(Feature {
            fid: 0,
            attributes: vec![
                FieldValue::Text("b".into()),
                FieldValue::Text("Building".into()),
                FieldValue::Text("2".into()),
                FieldValue::Float(10.0),
            ],
            geometry: Some(Geometry::Polygon {
                exterior: Ring::new(vec![
                    Coord { x: 0.0, y: 0.0, z: Some(0.0), m: None },
                    Coord { x: 0.0, y: 1.0, z: Some(0.0), m: None },
                    Coord { x: 0.0, y: 1.0, z: Some(10.0), m: None },
                ]),
                interiors: vec![],
            }),
        });
        let output: Value = serde_json::from_slice(&to_bytes(&[layer], CityJsonVersion::V1_1).unwrap()).unwrap();
        assert_eq!(output["CityObjects"]["b"]["type"], "Building");
        assert_eq!(output["CityObjects"]["b"]["attributes"]["height"], 10.0);
        assert_eq!(output["CityObjects"]["b"]["geometry"][0]["lod"], "2");
    }

    #[test]
    fn metadata_cache_accepts_reordered_json_and_still_rejects_changed_values() {
        let mut layers = source();
        let mut duplicate = layers[0].features[1].clone();
        layers[0].features[1].attributes[4] = FieldValue::Text(r#"{"a":1,"b":2}"#.into());
        duplicate.attributes[4] = FieldValue::Text(r#"{"b":2,"a":1}"#.into());
        layers[0].push(duplicate);
        to_bytes(&layers, CityJsonVersion::V2_0).unwrap();
        layers[0].features.last_mut().unwrap().attributes[4] = FieldValue::Text(r#"{"b":3,"a":1}"#.into());
        assert!(to_bytes(&layers, CityJsonVersion::V2_0).unwrap_err().to_string().contains("conflicting metadata"));
    }

    #[test]
    fn conflicting_object_metadata_and_unrepresentable_crs_are_errors() {
        let mut layers = source();
        let mut duplicate = layers[0].features[1].clone();
        duplicate.attributes[4] = FieldValue::Text("{\"different\":true}".into());
        layers[0].push(duplicate);
        assert!(to_bytes(&layers, CityJsonVersion::V2_0).unwrap_err().to_string().contains("conflicting metadata"));
        for name in ["EPSG:3068+5783", "ESRI:54009"] {
            let mut layers = source();
            SpatialRef::parse(name).unwrap().apply_to_layer(&mut layers[0]);
            assert!(to_bytes(&layers, CityJsonVersion::V2_0).is_err(), "accepted {name}");
        }
    }

    #[test]
    fn missing_and_cyclic_parent_references_are_rejected() {
        let mut layers = source();
        layers[0].features[1].attributes[3] = FieldValue::Text("[\"missing\"]".into());
        assert!(to_bytes(&layers, CityJsonVersion::V2_0).unwrap_err().to_string().contains("missing parent"));
        let mut layers = source();
        layers[0].features[0].attributes[1] = FieldValue::Text("BuildingPart".into());
        layers[0].features[0].attributes[3] = FieldValue::Text("[\"part\"]".into());
        assert!(to_bytes(&layers, CityJsonVersion::V2_0).unwrap_err().to_string().contains("cyclic object parents"));
    }

    #[test]
    fn ordinary_partial_city_column_pairs_remain_attributes() {
        for name in ["city_id", "city_type"] {
            let mut layer = Layer::new("ordinary");
            layer.add_field(wbvector::FieldDef::new(name, FieldType::Text));
            layer.push(Feature { fid: 0, attributes: vec![FieldValue::Text("original".into())], geometry: None });
            let output: Value = serde_json::from_slice(&to_bytes(&[layer], CityJsonVersion::V2_0).unwrap()).unwrap();
            let object = &output["CityObjects"]["_terrio_0_0"];
            assert_eq!(object["type"], "GenericCityObject");
            assert_eq!(object["attributes"][name], "original");
        }
        assert_eq!(lod(&FieldValue::Text(String::new()), CityJsonVersion::V2_0).unwrap(), "0");
    }

    #[test]
    fn furniture_cannot_own_restricted_building_bridge_or_tunnel_children() {
        for (family, kinds) in [
            ("Building", &["BuildingInstallation", "BuildingConstructiveElement", "BuildingFurniture", "BuildingStorey"][..]),
            ("Bridge", &["BridgeInstallation", "BridgeFurniture"][..]),
            ("Tunnel", &["TunnelInstallation", "TunnelConstructiveElement", "TunnelHollowSpace", "TunnelFurniture"][..]),
        ] {
            for kind in kinds {
                let doc = json!({"type":"CityJSON","version":"2.0","transform":{"scale":[1,1,1],"translate":[0,0,0]},
                    "vertices":[],"CityObjects":{
                    "root":{"type":family},
                    "furniture":{"type":format!("{family}Furniture"),"parents":["root"]},
                    "child":{"type":kind,"parents":["furniture"]}}});
                let layers = super::super::cityjson::parse(&serde_json::to_vec(&doc).unwrap()).unwrap();
                assert!(to_bytes(&layers, CityJsonVersion::V2_0).unwrap_err().to_string().contains("incompatible type"));
            }
        }
    }

    #[test]
    fn invalid_source_lines_and_rings_are_distinguished_from_quantization_collapse() {
        let point = Coord { x: 1.0, y: 2.0, z: Some(3.0), m: None };
        for geometry in [
            Geometry::LineString(vec![]),
            Geometry::LineString(vec![point.clone()]),
            Geometry::Polygon { exterior: Ring::new(vec![point; 3]), interiors: vec![] },
        ] {
            let mut layer = Layer::new("invalid");
            layer.push(Feature { fid: 0, attributes: vec![], geometry: Some(geometry) });
            assert!(to_bytes(&[layer], CityJsonVersion::V2_0).unwrap_err().to_string().contains("distinct source XYZ"));
        }
    }

    #[test]
    fn quantization_uses_safe_integers_for_tiny_and_large_coordinate_ranges() {
        for span in [f64::from_bits(1), 1e-300, 1.0, 1e250, f64::MAX] {
            let mut layer = Layer::new("points");
            layer.push(Feature {
                fid: 0,
                attributes: vec![],
                geometry: Some(Geometry::MultiPoint(vec![
                    Coord { x: -span, y: 0.0, z: Some(0.0), m: None },
                    Coord { x: span, y: 0.0, z: Some(0.0), m: None },
                ])),
            });
            let bytes = to_bytes(&[layer], CityJsonVersion::V2_0).unwrap();
            let doc: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(
                doc["vertices"].as_array().unwrap().iter().all(|p| p.as_array().unwrap().iter().all(|v| v
                    .as_i64()
                    .unwrap()
                    .unsigned_abs()
                    <= 1u64 << 52))
            );
            let result = super::super::cityjson::parse(&bytes).unwrap();
            for (feature, expected) in result[0].features.iter().zip([-span, span]) {
                let Geometry::Point(p) = feature.geometry.as_ref().unwrap() else { panic!() };
                let ulp = |v: f64| {
                    let v = v.abs();
                    if v.next_up().is_finite() { v.next_up() - v } else { v - v.next_down() }
                };
                let scale = doc["transform"]["scale"][0].as_f64().unwrap();
                let offset = doc["transform"]["translate"][0].as_f64().unwrap();
                let bound = scale / 2.0 + 4.0 * ulp(expected).max(ulp(p.x)).max(ulp(offset));
                assert!((p.x - expected).abs() <= bound, "span {span}, reconstructed {}, bound {bound}", p.x);
            }
        }
    }
}

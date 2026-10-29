//! GeoJSON, read and written as GDAL does.
//!
//! Reading: fields in the order the properties first appear, typed from their JSON values
//! (integers, reals, booleans, ISO 8601 dates and date-times, strings; arrays and objects become
//! JSON text), string feature ids as a leading `id` field and integer ids as feature ids, and the
//! CRS from a GeoJSON 2008 `crs` member; without one, EPSG:4326, or EPSG:4979 when the layer's
//! geometry type has heights, as GDAL infers the type (see [`LayerGeom`]).
//!
//! Writing keeps the layer's coordinates and names its CRS in a `crs` member when it is not
//! WGS 84, as GDAL does by default; [`GeoJsonOptions::rfc7946`] reprojects to WGS 84 instead and
//! orients polygon rings counter-clockwise, like GDAL's `RFC7946=YES`.

use crate::crs::SpatialRef;
use crate::{Error, Result};
use serde_json::{Map, Value, json};
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer, Ring};

/// Options for writing GeoJSON.
#[derive(Debug, Clone, Copy, Default)]
pub struct GeoJsonOptions {
    /// Write RFC 7946 GeoJSON: coordinates reprojected to WGS 84, no `crs` member,
    /// counter-clockwise exterior rings (GDAL's `RFC7946=YES`).
    pub rfc7946: bool,
}

impl GeoJsonOptions {
    /// Sets an option by its GDAL name: `RFC7946` (YES/NO). Returns `false` for other names.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        if key.eq_ignore_ascii_case("RFC7946") {
            self.rfc7946 = matches!(value.to_ascii_uppercase().as_str(), "YES" | "TRUE" | "ON" | "1");
            return Ok(true);
        }
        Ok(false)
    }
}

/// Reads a GeoJSON file.
pub fn read(path: &str) -> Result<Layer> {
    let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    parse(&std::fs::read(path)?, stem)
}

/// Parses GeoJSON. `default_name` names a layer whose collection has no `name`.
pub fn parse(bytes: &[u8], default_name: &str) -> Result<Layer> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    reserve_parse_memory(bytes.len())?;
    let doc: Value = serde_json::from_slice(bytes).map_err(|e| Error::InvalidData(format!("GeoJSON: {e}")))?;
    check_integers(&doc)?;
    let features: Vec<&Value> = match doc["type"].as_str() {
        Some("FeatureCollection") => doc["features"].as_array().map(|a| a.iter().collect()).unwrap_or_default(),
        Some("Feature") => vec![&doc],
        Some(_) => vec![&doc],
        None => return Err(Error::InvalidData("GeoJSON: no type".into())),
    };
    let mut schema = SchemaScan::default();
    for f in &features {
        schema.observe(f);
    }
    if !matches!(doc["type"].as_str(), Some("FeatureCollection" | "Feature")) {
        // A bare geometry: GDAL types the layer from the geometry it reads
        // (`OGRGeoJSONLayer::DetectGeometryType()`), 3-D when a position has a height.
        schema.geom = LayerGeom::of_read_geometry(&doc);
    }
    let crs = doc["crs"]["properties"]["name"].as_str().and_then(|n| SpatialRef::parse(n).ok());
    let mut layer = schema.geojson_layer(doc["name"].as_str().unwrap_or(default_name), crs);
    let reader = FeatureDecoder::new(&layer, &schema);
    for (n, f) in features.iter().enumerate() {
        layer.push(reader.feature(n as u64, f));
    }
    declare(schema.geom);
    Ok(layer)
}

/// The memory parsing GeoJSON takes per byte of input, at most for common files: the document's
/// values and the layer built from them come to about 15 bytes per byte for points with a few
/// properties (measured on 200,000 features).
const PARSE_MEMORY_PER_BYTE: usize = 16;

/// Reserves, and releases, the address space parsing `len` bytes of GeoJSON takes, so that a
/// document too large for the memory available fails with an error rather than aborting the
/// process when an allocation fails half-way through the parse.
fn reserve_parse_memory(len: usize) -> Result<()> {
    let need = len.saturating_mul(PARSE_MEMORY_PER_BYTE);
    let mut probe: Vec<u8> = Vec::new();
    probe
        .try_reserve_exact(need)
        .map_err(|_| Error::InvalidData(format!("GeoJSON: parsing {len} bytes needs about {need} bytes of memory, more than is available")))
}

/// The fields of GeoJSON features, inferred as GDAL infers them: a scan of every feature.
#[derive(Debug, Default, Clone)]
pub(crate) struct SchemaScan {
    /// Some feature has a string `id`: ids become a leading `id` field.
    string_ids: bool,
    /// The first feature is a STAC item: foreign members become fields.
    stac: Option<bool>,
    /// The fields in the order they first appear, with their merged kinds.
    kinds: Vec<(String, Option<Kind>)>,
    /// The layer's geometry type as GDAL infers it, for the default CRS.
    geom: LayerGeom,
}

/// What the schema scan needs of a feature, without building its geometry: its id, properties,
/// whether it is a STAC item, and its geometry's type. A member given twice keeps its last value,
/// as json-c (GDAL's parser) and `serde_json::Value` keep it.
#[derive(Default)]
struct Probe {
    id: Value,
    properties: Value,
    stac_version: bool,
    geometry: Option<GeomType>,
}

/// The members of features and geometries the scan reads; any other is `Other`. Identified
/// without allocating, as derived field identifiers are.
enum Key {
    Id,
    Properties,
    StacVersion,
    Geometry,
    Type,
    Coordinates,
    Geometries,
    Measures,
    Other,
}

impl<'de> serde::Deserialize<'de> for Key {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = Key;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a member name")
            }
            fn visit_str<E>(self, k: &str) -> std::result::Result<Key, E> {
                Ok(match k {
                    "id" => Key::Id,
                    "properties" => Key::Properties,
                    "stac_version" => Key::StacVersion,
                    "geometry" => Key::Geometry,
                    "type" => Key::Type,
                    "coordinates" => Key::Coordinates,
                    "geometries" => Key::Geometries,
                    "measures" => Key::Measures,
                    _ => Key::Other,
                })
            }
        }
        d.deserialize_identifier(V)
    }
}

/// The visits of a visitor that reads every JSON value but arrays as `$v` (objects drained).
macro_rules! other_values {
    ($t:ty, $v:expr) => {
        fn visit_bool<E>(self, _: bool) -> std::result::Result<$t, E> {
            Ok($v)
        }
        fn visit_i64<E>(self, _: i64) -> std::result::Result<$t, E> {
            Ok($v)
        }
        fn visit_u64<E>(self, _: u64) -> std::result::Result<$t, E> {
            Ok($v)
        }
        fn visit_f64<E>(self, _: f64) -> std::result::Result<$t, E> {
            Ok($v)
        }
        fn visit_str<E>(self, _: &str) -> std::result::Result<$t, E> {
            Ok($v)
        }
        fn visit_unit<E>(self) -> std::result::Result<$t, E> {
            Ok($v)
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<$t, A::Error> {
            while map.next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?.is_some() {}
            Ok($v)
        }
    };
}

impl<'de> serde::Deserialize<'de> for Probe {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Probe;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a GeoJSON feature")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<Probe, A::Error> {
                let mut p = Probe::default();
                while let Some(key) = map.next_key::<Key>()? {
                    match key {
                        Key::Id => p.id = map.next_value()?,
                        Key::Properties => p.properties = map.next_value()?,
                        Key::StacVersion => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                            p.stac_version = true;
                        }
                        Key::Geometry => p.geometry = map.next_value::<GeomField>()?.0.map(|g| g.geom_type(false)),
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(p)
            }
        }
        d.deserialize_map(V)
    }
}

/// GDAL's names of GeoJSON geometry types (`asAssocGeometryTypes`), matched without case.
const GEOMETRY_TYPES: [&str; 12] = [
    "Point",
    "LineString",
    "Polygon",
    "MultiPoint",
    "MultiLineString",
    "MultiPolygon",
    "GeometryCollection",
    "CircularString",
    "CompoundCurve",
    "CurvePolygon",
    "MultiCurve",
    "MultiSurface",
];

/// The types whose height GDAL takes from their first member's (`geometries`).
const COLLECTION_TYPES: [&str; 5] = ["GeometryCollection", "CompoundCurve", "CurvePolygon", "MultiCurve", "MultiSurface"];

/// A geometry's type as GDAL's `OGRGeoJSONGetOGRGeometryType()` infers it: the index of its
/// name in [`GEOMETRY_TYPES`] (`None` for `wkbUnknown`) and whether it has a height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GeomType {
    base: Option<usize>,
    z: bool,
}

/// The layer geometry type GDAL infers from the features' geometries in turn
/// (`OGRGeoJSONUpdateLayerGeomType()`): none yet; one type, with a height when any geometry of it
/// had one; or `wkbUnknown` once two types differ (or one is unknown), which nothing changes after.
/// Folding a sequence gives the same result in any grouping, so scans of parts merge.
///
/// GDAL also tells measured types apart (JSON-FG `measures`), which can make a layer of one base
/// type `wkbUnknown`; that distinction is not kept here, only the measures' effect on heights.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum LayerGeom {
    /// No geometry yet.
    #[default]
    Empty,
    /// One geometry type: its index in [`GEOMETRY_TYPES`], and whether it has a height.
    Known(usize, bool),
    /// Mixed or unknown types (`wkbUnknown`, which has no height).
    Unknown,
}

impl LayerGeom {
    /// The layer type after a geometry of type `t`.
    fn add(self, t: GeomType) -> Self {
        let next = match t.base {
            Some(b) => LayerGeom::Known(b, t.z),
            None => LayerGeom::Unknown,
        };
        self.merge(next)
    }

    /// The layer type of these geometries followed by `other`'s.
    fn merge(self, other: Self) -> Self {
        match (self, other) {
            (LayerGeom::Empty, x) | (x, LayerGeom::Empty) => x,
            (LayerGeom::Known(a, za), LayerGeom::Known(b, zb)) if a == b => LayerGeom::Known(a, za || zb),
            _ => LayerGeom::Unknown,
        }
    }

    /// Whether the type has a height (`OGR_GT_HasZ()`).
    pub(crate) fn has_z(self) -> bool {
        matches!(self, LayerGeom::Known(_, true))
    }

    /// The type's name as pyogrio names GDAL's (`read_info`'s `geometry_type`, measures
    /// dropped): `"Unknown"` before any geometry too, the type GDAL's GeoJSON layers start with.
    fn name(self) -> String {
        match self {
            LayerGeom::Known(b, z) => format!("{}{}", GEOMETRY_TYPES[b], if z { " Z" } else { "" }),
            LayerGeom::Empty | LayerGeom::Unknown => "Unknown".to_string(),
        }
    }

    /// The type of a layer of one geometry read by GDAL (a bare geometry document): its own type,
    /// 3-D when any position has a third value (`OGRGeoJSONReadRawPoint()`, collections made 3-D
    /// by any 3-D member).
    fn of_read_geometry(g: &Value) -> Self {
        fn any_height(v: &Value) -> bool {
            match v {
                Value::Array(a) => {
                    let is_position = a.len() >= 2 && a.iter().all(Value::is_number);
                    if is_position { a.len() >= 3 } else { a.iter().any(any_height) }
                }
                _ => false,
            }
        }
        fn has_height(g: &Value) -> bool {
            any_height(&g["coordinates"]) || g["geometries"].as_array().is_some_and(|m| m.iter().any(has_height))
        }
        let Some(base) = g["type"].as_str().and_then(|k| GEOMETRY_TYPES.iter().position(|t| t.eq_ignore_ascii_case(k))) else {
            return LayerGeom::Empty;
        };
        if geometry(g).is_none() {
            return LayerGeom::Empty;
        }
        LayerGeom::Known(base, has_height(g))
    }
}

thread_local! {
    /// The layer geometry types declared on this thread inside [`with_declared_geometry_types`];
    /// `None` outside it.
    static DECLARED: std::cell::RefCell<Option<Vec<String>>> = const { std::cell::RefCell::new(None) };
}

/// Runs `f`, an open of vector datasets, and returns with its result the layer geometry type
/// GDAL's GeoJSON and GeoJSONSeq drivers declare (`OGR_L_GetGeomType()`, named as pyogrio names
/// it: `"Point Z"`, `"Unknown"`) for each layer of those formats that `f` read on this thread, in
/// reading order.
///
/// GDAL types such a layer from its features' geometry objects, not from the geometries it reads:
/// a line whose first position has two values makes a `LineString` layer even when a later
/// position has a height, a geometry of a type GDAL does not know makes the layer `Unknown`, and a
/// layer without geometries is `Unknown`. The layers terrio returns have no declared type with
/// heights, so the type is returned beside them, as [`crate::warnings`] are.
pub fn with_declared_geometry_types<T>(f: impl FnOnce() -> T) -> (T, Vec<String>) {
    /// Restores the enclosing call's record, also when `f` panics.
    struct Scope(Option<Vec<String>>);
    impl Drop for Scope {
        fn drop(&mut self) {
            let outer = self.0.take();
            DECLARED.with(|d| *d.borrow_mut() = outer);
        }
    }
    let scope = Scope(DECLARED.with(|d| d.replace(Some(Vec::new()))));
    let out = f();
    let declared = DECLARED.with(|d| d.borrow_mut().take()).unwrap_or_default();
    drop(scope);
    (out, declared)
}

/// Records the type GDAL declares for a layer read with these geometries (see
/// [`with_declared_geometry_types`]).
fn declare(geom: LayerGeom) {
    DECLARED.with(|d| {
        if let Some(types) = d.borrow_mut().as_mut() {
            types.push(geom.name());
        }
    });
}

/// What `OGRGeoJSONGetOGRGeometryType()` reads of a geometry object: its `type`, the length of
/// its first position, its first member, and its `measures` member.
#[derive(Debug, Default)]
struct GeomProbe {
    kind: Option<String>,
    /// The length of the first array down the first elements of `coordinates` (see
    /// [`first_position`]); `None` when `coordinates` is not a non-empty array.
    first_len: Option<usize>,
    /// The first element of `geometries`, when it is an object.
    member: Option<Box<GeomProbe>>,
    /// `json_object_get_boolean(measures.enabled)`, when `measures` is given.
    measures: Option<bool>,
}

impl GeomProbe {
    /// The type GDAL infers, with `upper_m` the measures flag of the geometry containing it.
    fn geom_type(&self, upper_m: bool) -> GeomType {
        const UNKNOWN: GeomType = GeomType { base: None, z: false };
        let Some(kind) = self.kind.as_deref() else { return UNKNOWN };
        let Some(base) = GEOMETRY_TYPES.iter().position(|t| t.eq_ignore_ascii_case(kind)) else { return UNKNOWN };
        let m = self.measures.unwrap_or(upper_m);
        let z = if COLLECTION_TYPES.iter().any(|t| t.eq_ignore_ascii_case(kind)) {
            self.member.as_ref().is_some_and(|g| g.geom_type(m).z)
        } else {
            matches!((m, self.first_len), (false, Some(3)) | (true, Some(4)))
        };
        GeomType { base: Some(base), z }
    }
}

/// A `geometry` member (any JSON value) as GDAL's type inference reads it: a probe for an object,
/// `None` for anything else (a null geometry, or one that is not an object).
struct GeomField(Option<GeomProbe>);

impl<'de> serde::Deserialize<'de> for GeomField {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = GeomField;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_bool<E>(self, _: bool) -> std::result::Result<GeomField, E> {
                Ok(GeomField(None))
            }
            fn visit_i64<E>(self, _: i64) -> std::result::Result<GeomField, E> {
                Ok(GeomField(None))
            }
            fn visit_u64<E>(self, _: u64) -> std::result::Result<GeomField, E> {
                Ok(GeomField(None))
            }
            fn visit_f64<E>(self, _: f64) -> std::result::Result<GeomField, E> {
                Ok(GeomField(None))
            }
            fn visit_str<E>(self, _: &str) -> std::result::Result<GeomField, E> {
                Ok(GeomField(None))
            }
            fn visit_unit<E>(self) -> std::result::Result<GeomField, E> {
                Ok(GeomField(None))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<GeomField, A::Error> {
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(GeomField(None))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<GeomField, A::Error> {
                let mut g = GeomProbe::default();
                while let Some(key) = map.next_key::<Key>()? {
                    match key {
                        Key::Type => g.kind = map.next_value::<Value>()?.as_str().map(str::to_string),
                        Key::Coordinates => g.first_len = map.next_value::<FirstPosition>()?.0,
                        Key::Geometries => g.member = map.next_value::<FirstMember>()?.0.map(Box::new),
                        Key::Measures => {
                            let v = map.next_value::<Value>()?;
                            g.measures = (!v.is_null()).then(|| json_c_boolean(&v["enabled"]));
                        }
                        _ => {
                            map.next_value::<serde::de::IgnoredAny>()?;
                        }
                    }
                }
                Ok(GeomField(Some(g)))
            }
        }
        d.deserialize_any(V)
    }
}

/// json-c's `json_object_get_boolean()`: false for null, objects and arrays.
fn json_c_boolean(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => false,
    }
}

/// The first element of `geometries`, read as a geometry; `None` for an empty array, a first
/// element that is not an object, or a value that is not an array.
struct FirstMember(Option<GeomProbe>);

impl<'de> serde::Deserialize<'de> for FirstMember {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = FirstMember;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<FirstMember, A::Error> {
                let first = seq.next_element::<GeomField>()?.and_then(|g| g.0);
                while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(FirstMember(first))
            }
            other_values!(FirstMember, FirstMember(None));
        }
        d.deserialize_any(V)
    }
}

/// `coordinates` as `OGRGeoJSONGetOGRGeometryType()` reads it: down the first elements while
/// they are non-empty arrays, the length of the last array reached (the first position's).
/// `None` when `coordinates` is not a non-empty array.
struct FirstPosition(Option<usize>);

/// An element of nested coordinates: a non-empty array, with the length of the first position
/// found down its first elements, or anything else.
enum Nested {
    Array(usize),
    Other,
}

/// The first position's length in a JSON array, or `None` for an empty one.
fn first_position<'de, A: serde::de::SeqAccess<'de>>(mut seq: A) -> std::result::Result<Option<usize>, A::Error> {
    match seq.next_element::<Nested>()? {
        None => Ok(None),
        Some(Nested::Array(n)) => {
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {}
            Ok(Some(n))
        }
        Some(Nested::Other) => {
            let mut n = 1;
            while seq.next_element::<serde::de::IgnoredAny>()?.is_some() {
                n += 1;
            }
            Ok(Some(n))
        }
    }
}

impl<'de> serde::Deserialize<'de> for Nested {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Nested;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> std::result::Result<Nested, A::Error> {
                Ok(first_position(seq)?.map_or(Nested::Other, Nested::Array))
            }
            other_values!(Nested, Nested::Other);
        }
        d.deserialize_any(V)
    }
}

impl<'de> serde::Deserialize<'de> for FirstPosition {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = FirstPosition;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, seq: A) -> std::result::Result<FirstPosition, A::Error> {
                Ok(FirstPosition(first_position(seq)?))
            }
            other_values!(FirstPosition, FirstPosition(None));
        }
        d.deserialize_any(V)
    }
}

impl SchemaScan {
    /// Adds a feature (a `Feature` object or a bare geometry).
    pub(crate) fn observe(&mut self, f: &Value) {
        let stac = *self.stac.get_or_insert_with(|| f.get("stac_version").is_some());
        self.string_ids |= f["id"].is_string();
        // GDAL types the layer from each feature's `geometry` object (`GenerateFeatureDefn()`).
        if let Some(GeomField(Some(g))) = f.get("geometry").and_then(|g| <GeomField as serde::Deserialize>::deserialize(g).ok()) {
            self.geom = self.geom.add(g.geom_type(false));
        }
        for (k, v) in attributes_of(f, stac) {
            self.add_kind(&k, v);
        }
    }

    /// Adds a feature given by its JSON text, decoding only its id and properties unless it is
    /// a STAC item.
    pub(crate) fn observe_raw(&mut self, raw: &[u8]) -> Result<()> {
        let err = |e: serde_json::Error| Error::InvalidData(format!("GeoJSON: {e}"));
        if self.stac.is_none() || self.stac == Some(true) {
            let v: Value = serde_json::from_slice(raw).map_err(err)?;
            self.observe(&v);
            return Ok(());
        }
        let p: Probe = serde_json::from_slice(raw).map_err(err)?;
        let _ = p.stac_version;
        self.string_ids |= p.id.is_string();
        if let Some(t) = p.geometry {
            self.geom = self.geom.add(t);
        }
        if let Value::Object(props) = &p.properties {
            for (k, v) in props {
                self.add_kind(k, v);
            }
        }
        Ok(())
    }

    /// Whether a feature has been observed.
    pub(crate) fn started(&self) -> bool {
        self.stac.is_some()
    }

    /// An empty scan that decodes features as this one does (STAC items or not).
    pub(crate) fn empty_like(&self) -> Self {
        Self { string_ids: false, stac: self.stac, kinds: Vec::new(), geom: LayerGeom::Empty }
    }

    /// Adds the scan of the features that follow those of this scan.
    pub(crate) fn merge(&mut self, other: SchemaScan) {
        self.string_ids |= other.string_ids;
        self.geom = self.geom.merge(other.geom);
        if self.stac.is_none() {
            self.stac = other.stac;
        }
        for (k, t) in other.kinds {
            match self.kinds.iter_mut().find(|(n, _)| *n == k) {
                Some((_, prev)) => *prev = merge(*prev, t),
                None => self.kinds.push((k, t)),
            }
        }
    }

    fn add_kind(&mut self, k: &str, v: &Value) {
        let t = kind(v);
        match self.kinds.iter_mut().find(|(n, _)| n == k) {
            Some((_, prev)) => *prev = merge(*prev, t),
            None => self.kinds.push((k.to_string(), t)),
        }
    }

    /// The layer a GeoJSON document's features make, without its features: without a `crs`
    /// member, in EPSG:4326, or EPSG:4979 when the layer's geometry type has heights, as GDAL's
    /// GeoJSON driver sets it ("If there is none defined, we use 4326 / 4979").
    pub(crate) fn geojson_layer(&self, name: &str, crs: Option<SpatialRef>) -> Layer {
        let mut layer = self.layer(name, crs.clone());
        if crs.is_none() && self.geom.has_z() {
            layer.set_crs_epsg(Some(4979));
        }
        layer
    }

    /// The layer the features make, without its features; WGS 84 (EPSG:4326) without a CRS,
    /// as GDAL's GeoJSONSeq driver has it whatever the heights.
    pub(crate) fn layer(&self, name: &str, crs: Option<SpatialRef>) -> Layer {
        let mut layer = Layer::new(name);
        if self.string_ids {
            layer.add_field(FieldDef::new("id", FieldType::Text));
        }
        for (k, t) in &self.kinds {
            if layer.schema.field_index(k).is_none() {
                layer.add_field(FieldDef::new(k, t.map_or(FieldType::Text, Kind::field_type)));
            }
        }
        match crs {
            Some(s) => s.apply_to_layer(&mut layer),
            None => layer.set_crs_epsg(Some(4326)),
        }
        layer
    }
}

/// Decodes features into the fields of a layer.
pub(crate) struct FeatureDecoder {
    names: Vec<String>,
    types: Vec<FieldType>,
    string_ids: bool,
    stac: bool,
}

impl FeatureDecoder {
    pub(crate) fn new(layer: &Layer, scan: &SchemaScan) -> Self {
        Self {
            names: layer.schema.fields().iter().map(|f| f.name.clone()).collect(),
            types: layer.schema.fields().iter().map(|f| f.field_type).collect(),
            string_ids: scan.string_ids,
            stac: scan.stac.unwrap_or(false),
        }
    }

    /// The `n`-th feature (numbered from 0) of the collection.
    pub(crate) fn feature(&self, n: u64, f: &Value) -> Feature {
        let mut attributes = vec![FieldValue::Null; self.types.len()];
        if self.string_ids {
            attributes[0] = match &f["id"] {
                Value::Null => FieldValue::Null,
                Value::String(s) => FieldValue::Text(s.clone()),
                v => FieldValue::Text(v.to_string()),
            };
        }
        for (k, v) in attributes_of(f, self.stac) {
            if let Some(i) = self.names.iter().position(|n| *n == k).filter(|&i| !(self.string_ids && i == 0)) {
                attributes[i] = value(v, self.types[i]);
            }
        }
        let fid = if self.string_ids { n } else { f["id"].as_u64().unwrap_or(n) };
        let geometry = if f["type"] == "Feature" { geometry(&f["geometry"]) } else { geometry(f) };
        Feature { fid, geometry, attributes }
    }

    /// Decodes features given by their JSON text, numbered from `first`, in parallel.
    pub(crate) fn features_raw(&self, first: u64, raw: &[Vec<u8>]) -> Result<Vec<Feature>> {
        crate::par::map_range(raw.len(), |i| {
            let v: Value = serde_json::from_slice(&raw[i]).map_err(|e| Error::InvalidData(format!("GeoJSON: {e}")))?;
            Ok(self.feature(first + i as u64, &v))
        })
        .into_iter()
        .collect()
    }
}

/// A feature's attributes: its properties and, for STAC items, its foreign members (all but
/// `type`, `id`, `geometry`, `bbox` and `properties`), each asset's members flattened to
/// `assets.<asset>.<member>`, as GDAL's `FOREIGN_MEMBERS=AUTO` reads them.
fn attributes_of(f: &Value, stac: bool) -> Vec<(String, &Value)> {
    let mut out: Vec<(String, &Value)> = f["properties"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v)).collect();
    if !stac {
        return out;
    }
    for (k, v) in f.as_object().into_iter().flatten() {
        match k.as_str() {
            "type" | "id" | "geometry" | "bbox" | "properties" => {}
            "assets" if v.is_object() => {
                for (name, asset) in v.as_object().into_iter().flatten() {
                    for (m, mv) in asset.as_object().into_iter().flatten() {
                        out.push((format!("assets.{name}.{m}"), mv));
                    }
                }
            }
            _ => out.push((k.clone(), v)),
        }
    }
    out
}

/// The type of a property value, as GDAL infers it.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Integer,
    Real,
    Boolean,
    Date,
    DateTime,
    /// A time of day: GDAL's `OFTTime`, held as text.
    Time,
    String,
    /// JSON text: objects, arrays, and fields mixing strings with other values (GDAL's string
    /// fields of the `JSON` subtype).
    Json,
}

impl Kind {
    fn field_type(self) -> FieldType {
        match self {
            Kind::Integer => FieldType::Integer,
            Kind::Real => FieldType::Float,
            Kind::Boolean => FieldType::Boolean,
            Kind::Date => FieldType::Date,
            Kind::DateTime => FieldType::DateTime,
            Kind::Time | Kind::String => FieldType::Text,
            Kind::Json => FieldType::Json,
        }
    }
}

/// The digits of `s` from `from`, `n` of them, as a number.
fn digits(s: &[u8], from: usize, n: usize) -> Option<u32> {
    let d = s.get(from..from + n)?;
    d.iter().all(u8::is_ascii_digit).then(|| d.iter().fold(0, |a, c| a * 10 + u32::from(c - b'0')))
}

/// `YYYY-MM-DD` (or with `/`), a valid month and day, as GDAL's `OGRParseDate` takes it.
fn is_date(b: &[u8]) -> bool {
    b.len() == 10
        && b[4] == b[7]
        && matches!(b[4], b'-' | b'/')
        && digits(b, 0, 4).is_some()
        && digits(b, 5, 2).is_some_and(|m| (1..=12).contains(&m))
        && digits(b, 8, 2).is_some_and(|d| (1..=31).contains(&d))
}

/// `HH:MM:SS[.sss]` followed by nothing, `Z` or an offset (`+HH`, `+HHMM`, `+HH:MM`).
fn is_time(b: &[u8]) -> bool {
    if !(b.len() >= 8
        && b[2] == b':'
        && b[5] == b':'
        && digits(b, 0, 2).is_some_and(|h| h <= 23)
        && digits(b, 3, 2).is_some_and(|m| m <= 59)
        && digits(b, 6, 2).is_some_and(|s| s <= 61))
    {
        return false;
    }
    let mut rest = &b[8..];
    if let Some(frac) = rest.strip_prefix(b".") {
        let n = frac.iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 {
            return false;
        }
        rest = &frac[n..];
    }
    match rest {
        [] | [b'Z'] => true,
        [b'+' | b'-', tz @ ..] => {
            matches!(tz.len(), 2 | 4 | 5) && (tz.len() != 5 || tz[2] == b':') && tz.iter().filter(|c| **c != b':').all(u8::is_ascii_digit)
        }
        _ => false,
    }
}

/// A date, `T` or a space, and a time. Bytes, not `str` slices: text that is not ASCII must not
/// split a character.
fn is_datetime(b: &[u8]) -> bool {
    b.len() >= 19 && is_date(&b[..10]) && (b[10] == b'T' || b[10] == b' ') && is_time(&b[11..])
}

fn kind(v: &Value) -> Option<Kind> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(_) => Kind::Boolean,
        // GDAL takes integers at the ends of the 64-bit range as clamped, and reads them as reals.
        Value::Number(n) if n.as_i64().is_some_and(|i| i == i64::MIN || i == i64::MAX) => Kind::Real,
        Value::Number(n) if n.is_i64() || n.is_u64() => Kind::Integer,
        Value::Number(_) => Kind::Real,
        Value::String(s) if is_datetime(s.as_bytes()) => Kind::DateTime,
        Value::String(s) if is_date(s.as_bytes()) => Kind::Date,
        Value::String(s) if s.len() <= 32 && is_time(s.as_bytes()) => Kind::Time,
        Value::String(_) => Kind::String,
        _ => Kind::Json,
    })
}

/// The kind of a field holding values of two kinds, as GDAL widens field types: booleans fold
/// into numbers, integers into reals, dates into date-times; strings mixed with anything else,
/// and anything mixed with JSON, make JSON text.
fn merge(a: Option<Kind>, b: Option<Kind>) -> Option<Kind> {
    use Kind as K;
    Some(match (a, b) {
        (a, None) => return a,
        (None, b) => return b,
        (Some(a), Some(b)) if a == b => a,
        (Some(K::Integer | K::Boolean), Some(K::Integer | K::Boolean)) => K::Integer,
        (Some(K::Integer | K::Real | K::Boolean), Some(K::Integer | K::Real | K::Boolean)) => K::Real,
        (Some(K::Date | K::DateTime), Some(K::Date | K::DateTime)) => K::DateTime,
        _ => K::Json,
    })
}

fn value(v: &Value, t: FieldType) -> FieldValue {
    match (t, v) {
        (_, Value::Null) => FieldValue::Null,
        (FieldType::Integer, Value::Bool(b)) => FieldValue::Integer(i64::from(*b)),
        (FieldType::Integer, v) => v.as_i64().map_or(FieldValue::Null, FieldValue::Integer),
        (FieldType::Float, Value::Bool(b)) => FieldValue::Float(f64::from(u8::from(*b))),
        (FieldType::Float, v) => v.as_f64().map_or(FieldValue::Null, FieldValue::Float),
        (FieldType::Boolean, Value::Bool(b)) => FieldValue::Boolean(*b),
        (FieldType::Date, Value::String(s)) => FieldValue::Date(s.clone()),
        (FieldType::DateTime, Value::String(s)) => FieldValue::DateTime(s.clone()),
        (_, Value::String(s)) => FieldValue::Text(s.clone()),
        (_, v) => FieldValue::Text(v.to_string()),
    }
}

/// Fails on integers beyond the 64-bit signed range anywhere in a document, which GDAL's JSON
/// parser refuses ("Unrecognized number").
fn check_integers(v: &Value) -> Result<()> {
    match v {
        Value::Number(n) if n.is_u64() && !n.is_i64() => Err(Error::InvalidData(format!("GeoJSON: unrecognized number {n}"))),
        Value::Array(a) => a.iter().try_for_each(check_integers),
        Value::Object(o) => o.values().try_for_each(check_integers),
        _ => Ok(()),
    }
}

fn position(v: &Value) -> Option<Coord> {
    let a = v.as_array()?;
    Some(Coord { x: a.first()?.as_f64()?, y: a.get(1)?.as_f64()?, z: a.get(2).and_then(Value::as_f64), m: None })
}

fn positions(v: &Value) -> Vec<Coord> {
    v.as_array().into_iter().flatten().filter_map(position).collect()
}

fn ring(v: &Value) -> Ring {
    let mut cs = positions(v);
    if cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
        cs.pop();
    }
    Ring::new(cs)
}

fn polygon(v: &Value) -> Option<(Ring, Vec<Ring>)> {
    let mut rings = v.as_array()?.iter().map(ring);
    let exterior = rings.next()?;
    Some((exterior, rings.collect()))
}

/// Parses a GeoJSON geometry object.
pub fn geometry(g: &Value) -> Option<Geometry> {
    let c = &g["coordinates"];
    let list = || c.as_array().into_iter().flatten();
    Some(match g["type"].as_str()? {
        "Point" => Geometry::Point(position(c)?),
        "MultiPoint" => Geometry::MultiPoint(positions(c)),
        "LineString" => Geometry::LineString(positions(c)),
        "MultiLineString" => Geometry::MultiLineString(list().map(positions).collect()),
        // GDAL reads `"coordinates": []` as an empty polygon (an empty exterior ring here).
        "Polygon" if c.as_array().is_some_and(Vec::is_empty) => {
            Geometry::Polygon { exterior: Ring::new(Vec::new()), interiors: Vec::new() }
        }
        "Polygon" => {
            let (exterior, interiors) = polygon(c)?;
            Geometry::Polygon { exterior, interiors }
        }
        "MultiPolygon" => Geometry::MultiPolygon(list().filter_map(polygon).collect()),
        "GeometryCollection" => {
            Geometry::GeometryCollection(g["geometries"].as_array().into_iter().flatten().filter_map(geometry).collect())
        }
        _ => return None,
    })
}

/// Writes a layer as GeoJSON.
pub fn write(layer: &Layer, path: &str, options: &GeoJsonOptions) -> Result<()> {
    std::fs::write(path, to_string(layer, options)?)?;
    Ok(())
}

/// Encodes a layer as a GeoJSON FeatureCollection.
pub fn to_string(layer: &Layer, options: &GeoJsonOptions) -> Result<String> {
    let mut w = CollectionEncoder::new(layer, *options)?;
    let mut out = w.head();
    w.features(layer, &mut out)?;
    out.push_str(CollectionEncoder::TAIL);
    Ok(out)
}

/// Encodes a FeatureCollection piece by piece: its head, its features (in as many chunks as
/// needed), its tail. The text is the same as [`to_string`] gives.
pub(crate) struct CollectionEncoder {
    head: String,
    rfc7946: bool,
    reproject: Option<SpatialRef>,
    first: bool,
}

impl CollectionEncoder {
    /// The end of the collection.
    pub(crate) const TAIL: &str = "]}\n";

    /// An encoder for the features of `layer` (its fields and CRS; its features are not read).
    pub(crate) fn new(layer: &Layer, options: GeoJsonOptions) -> Result<Self> {
        let srs = SpatialRef::from_layer(layer);
        let reproject =
            (options.rfc7946 && !srs.is_unknown() && srs.epsg != Some(4326)).then(|| SpatialRef::from_epsg(4326)).transpose()?;
        let mut doc = Map::new();
        doc.insert("type".into(), json!("FeatureCollection"));
        doc.insert("name".into(), json!(layer.name));
        if !options.rfc7946
            && !srs.is_unknown()
            && srs.epsg != Some(4326)
            && let Some(code) = super::naming_epsg(layer)
        {
            doc.insert("crs".into(), json!({"type": "name", "properties": {"name": format!("urn:ogc:def:crs:EPSG::{code}")}}));
        }
        doc.insert("features".into(), Value::Array(Vec::new()));
        let text = Value::Object(doc).to_string();
        let head = text.strip_suffix("]}").unwrap_or(&text).to_string();
        Ok(Self { head, rfc7946: options.rfc7946, reproject, first: true })
    }

    /// The head of the collection, up to the opening of its `features` array.
    pub(crate) fn head(&self) -> String {
        self.head.clone()
    }

    /// Appends the features of a chunk (a layer with the fields given to [`Self::new`]).
    pub(crate) fn features(&mut self, chunk: &Layer, out: &mut String) -> Result<()> {
        let reprojected;
        let chunk = match &self.reproject {
            Some(dst) => {
                reprojected = crate::vector::reproject_layer(chunk, dst)?;
                &reprojected
            }
            None => chunk,
        };
        let fields = chunk.schema.fields();
        let rfc = self.rfc7946;
        let texts = crate::par::map_range(chunk.features.len(), |i| feature_json(&chunk.features[i], fields, rfc).to_string());
        for t in texts {
            if !self.first {
                out.push(',');
            }
            self.first = false;
            // One feature per line, as GDAL writes it.
            out.push_str(&t.replace("{\"type\":\"Feature\",", "\n{\"type\":\"Feature\","));
        }
        Ok(())
    }
}

/// A feature as a GeoJSON `Feature` object.
fn feature_json(f: &Feature, fields: &[FieldDef], rfc7946: bool) -> Value {
    let props: Map<String, Value> = fields
        .iter()
        .zip(&f.attributes)
        .map(|(d, v)| {
            let v = match v {
                FieldValue::Null => Value::Null,
                FieldValue::Integer(x) => json!(x),
                FieldValue::Float(x) if x.is_finite() => json!(x),
                FieldValue::Float(_) => Value::Null,
                FieldValue::Boolean(b) => json!(b),
                FieldValue::Text(s) if d.field_type == FieldType::Json => serde_json::from_str(s).unwrap_or_else(|_| json!(s)),
                FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => json!(s),
                FieldValue::Blob(b) => json!(b.iter().map(|x| format!("{x:02X}")).collect::<String>()),
            };
            (d.name.clone(), v)
        })
        .collect();
    json!({
        "type": "Feature",
        "properties": props,
        "geometry": f.geometry.as_ref().map_or(Value::Null, |g| encode(g, rfc7946)),
    })
}

/// Encodes GeoJSON text sequences (RFC 8142, GDAL's GeoJSONSeq): one feature per line, in
/// WGS 84 with counter-clockwise exterior rings, as GDAL writes them.
pub(crate) struct SeqEncoder {
    reproject: Option<SpatialRef>,
}

impl SeqEncoder {
    /// An encoder for the features of `layer`.
    pub(crate) fn new(layer: &Layer) -> Result<Self> {
        let srs = SpatialRef::from_layer(layer);
        let reproject = (!srs.is_unknown() && srs.epsg != Some(4326)).then(|| SpatialRef::from_epsg(4326)).transpose()?;
        Ok(Self { reproject })
    }

    /// Appends the features of a chunk, a line each.
    pub(crate) fn features(&self, chunk: &Layer, out: &mut String) -> Result<()> {
        let reprojected;
        let chunk = match &self.reproject {
            Some(dst) => {
                reprojected = crate::vector::reproject_layer(chunk, dst)?;
                &reprojected
            }
            None => chunk,
        };
        let fields = chunk.schema.fields();
        for t in crate::par::map_range(chunk.features.len(), |i| feature_json(&chunk.features[i], fields, true).to_string()) {
            out.push_str(&t);
            out.push('\n');
        }
        Ok(())
    }
}

/// Encodes a layer as a GeoJSON text sequence.
pub fn to_seq_string(layer: &Layer) -> Result<String> {
    let mut out = String::new();
    SeqEncoder::new(layer)?.features(layer, &mut out)?;
    Ok(out)
}

/// Parses a GeoJSON text sequence (one feature per line, optionally preceded by the record
/// separator). `name` names the layer; the CRS is WGS 84.
pub fn parse_seq(bytes: &[u8], name: &str) -> Result<Layer> {
    let mut scan = SchemaScan::default();
    let lines: Vec<&[u8]> = bytes.split(|&b| b == b'\n').map(seq_record).filter(|l| !l.is_empty()).collect();
    for l in &lines {
        scan.observe_raw(l)?;
    }
    let mut layer = scan.layer(name, None);
    let decoder = FeatureDecoder::new(&layer, &scan);
    let raw: Vec<Vec<u8>> = lines.iter().map(|l| l.to_vec()).collect();
    layer.features = decoder.features_raw(0, &raw)?;
    declare(scan.geom);
    Ok(layer)
}

/// A line of a text sequence without its record separators and surrounding white space.
pub(crate) fn seq_record(line: &[u8]) -> &[u8] {
    let is_space = |b: &u8| b.is_ascii_whitespace() || *b == 0x1e;
    let start = line.iter().position(|b| !is_space(b)).unwrap_or(line.len());
    let end = line.iter().rposition(|b| !is_space(b)).map_or(start, |e| e + 1);
    &line[start..end]
}

fn pos(c: &Coord) -> Value {
    match c.z {
        Some(z) => json!([c.x, c.y, z]),
        None => json!([c.x, c.y]),
    }
}

/// A closed ring's positions; with `ccw`, exterior rings counter-clockwise and holes clockwise.
fn ring_json(r: &Ring, exterior: bool, ccw: bool) -> Value {
    let mut cs = r.0.clone();
    if ccw && (r.signed_area() > 0.0) != exterior {
        cs.reverse();
    }
    if let Some(first) = cs.first().cloned()
        && cs.len() > 1
        && cs.last().map(|l| (l.x, l.y)) != Some((first.x, first.y))
    {
        cs.push(first);
    }
    Value::Array(cs.iter().map(pos).collect())
}

fn polygon_json(e: &Ring, hs: &[Ring], ccw: bool) -> Value {
    Value::Array(std::iter::once(ring_json(e, true, ccw)).chain(hs.iter().map(|h| ring_json(h, false, ccw))).collect())
}

fn encode(g: &Geometry, ccw: bool) -> Value {
    match g {
        Geometry::Point(c) => json!({"type": "Point", "coordinates": pos(c)}),
        Geometry::MultiPoint(cs) => json!({"type": "MultiPoint", "coordinates": cs.iter().map(pos).collect::<Vec<_>>()}),
        Geometry::LineString(cs) => json!({"type": "LineString", "coordinates": cs.iter().map(pos).collect::<Vec<_>>()}),
        Geometry::MultiLineString(ls) => json!({
            "type": "MultiLineString",
            "coordinates": ls.iter().map(|l| l.iter().map(pos).collect::<Vec<_>>()).collect::<Vec<_>>(),
        }),
        Geometry::Polygon { exterior, interiors } => json!({"type": "Polygon", "coordinates": polygon_json(exterior, interiors, ccw)}),
        Geometry::MultiPolygon(ps) => {
            json!({"type": "MultiPolygon", "coordinates": ps.iter().map(|(e, h)| polygon_json(e, h, ccw)).collect::<Vec<_>>()})
        }
        Geometry::GeometryCollection(gs) => {
            json!({"type": "GeometryCollection", "geometries": gs.iter().map(|g| encode(g, ccw)).collect::<Vec<_>>()})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_long_multibyte_text() {
        // Byte 10 of these names falls inside a character; the date check must not slice there (#159).
        let text = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"name":"東京都千代田区"},"geometry":{"type":"Point","coordinates":[139.7,35.7]}},
            {"type":"Feature","properties":{"name":"2020-01-0千代田区一丁目"},"geometry":null}]}"#;
        let l = parse(text.as_bytes(), "file").unwrap();
        assert_eq!(l.schema.fields()[0].field_type, FieldType::Text);
        assert_eq!(l.features[0].attributes[0], FieldValue::Text("東京都千代田区".into()));
    }

    #[test]
    fn reads_like_gdal() {
        let text = r#"{"type":"FeatureCollection","name":"places","crs":{"type":"name","properties":{"name":"urn:ogc:def:crs:EPSG::32633"}},
            "features":[
            {"type":"Feature","id":"a","properties":{"name":"Côte d'Ivoire","n":3.0,"k":1,"when":"2020-01-02","tags":[1,2]},"geometry":{"type":"Point","coordinates":[500000,10]}},
            {"type":"Feature","id":"b","properties":{"name":null,"n":2,"k":2,"when":"2020-01-03T10:00:00Z","flag":true},"geometry":null}]}"#;
        let l = parse(text.as_bytes(), "file").unwrap();
        assert_eq!((l.name.as_str(), l.crs_epsg()), ("places", Some(32633)));
        let t: Vec<(&str, FieldType)> = l.schema.fields().iter().map(|f| (f.name.as_str(), f.field_type)).collect();
        assert_eq!(
            t,
            [
                ("id", FieldType::Text),
                ("name", FieldType::Text),
                ("n", FieldType::Float),
                ("k", FieldType::Integer),
                ("when", FieldType::DateTime),
                ("tags", FieldType::Json),
                ("flag", FieldType::Boolean)
            ]
        );
        assert_eq!(l.features[0].attributes[1], FieldValue::Text("Côte d'Ivoire".into()));
        assert_eq!(l.features[0].attributes[2], FieldValue::Float(3.0));

        // GDAL's default keeps the coordinates and names the CRS.
        let out = to_string(&l, &GeoJsonOptions::default()).unwrap();
        assert!(out.contains("urn:ogc:def:crs:EPSG::32633"), "{out}");
        let back = parse(out.as_bytes(), "file").unwrap();
        assert_eq!(back.crs_epsg(), Some(32633));
        assert_eq!(back.features[0].geometry, Some(Geometry::point(500000.0, 10.0)));
        assert_eq!(back.features[0].attributes, l.features[0].attributes);
        // RFC 7946: WGS 84.
        let out = to_string(&l, &GeoJsonOptions { rfc7946: true }).unwrap();
        let back = parse(out.as_bytes(), "file").unwrap();
        assert!(!out.contains("\"crs\""));
        let Some(Geometry::Point(c)) = &back.features[0].geometry else { panic!("no point") };
        assert!((c.x - 15.0).abs() < 1e-6, "{}", c.x);
    }

    /// GDAL 3.12.4 (pyogrio 0.13.0 `read_info`) on documents without a `crs` member: EPSG:4979
    /// when the layer type it infers has heights, else EPSG:4326, and that type as
    /// pyogrio names it (`geometry_type`). Measured with a 26-file GeoJSON matrix compared with
    /// GDAL, cases c01 to c25.
    const GDAL_LAYER_TYPES: &[(&str, &str, u32, &str)] = &[
        ("c01 point 3-D", r#"[{"type":"Point","coordinates":[10,20,30]}]"#, 4979, "Point Z"),
        ("c02 point 2-D", r#"[{"type":"Point","coordinates":[10,20]}]"#, 4326, "Point"),
        ("c03 2-D then 3-D", r#"[{"type":"Point","coordinates":[10,20]},{"type":"Point","coordinates":[10,20,30]}]"#, 4979, "Point Z"),
        ("c04 3-D then 2-D", r#"[{"type":"Point","coordinates":[10,20,30]},{"type":"Point","coordinates":[10,20]}]"#, 4979, "Point Z"),
        (
            "c05 mixed types, 3-D",
            r#"[{"type":"Point","coordinates":[10,20,30]},{"type":"LineString","coordinates":[[10,20,1],[11,21,2]]}]"#,
            4326,
            "Unknown",
        ),
        ("c06 first position 2-D", r#"[{"type":"LineString","coordinates":[[1,2],[3,4,5]]}]"#, 4326, "LineString"),
        ("c07 first position 3-D", r#"[{"type":"LineString","coordinates":[[1,2,3],[3,4]]}]"#, 4979, "LineString Z"),
        (
            "c08 collection, first member 3-D",
            r#"[{"type":"GeometryCollection","geometries":[{"type":"Point","coordinates":[10,20,30]},{"type":"Point","coordinates":[1,2]}]}]"#,
            4979,
            "GeometryCollection Z",
        ),
        (
            "c09 collection, first member 2-D",
            r#"[{"type":"GeometryCollection","geometries":[{"type":"Point","coordinates":[10,20]},{"type":"Point","coordinates":[10,20,30]}]}]"#,
            4326,
            "GeometryCollection",
        ),
        ("c10 type without case", r#"[{"type":"point","coordinates":[10,20,30]}]"#, 4979, "Point Z"),
        ("c11 null, then 3-D", r#"[null,{"type":"Point","coordinates":[10,20,30]}]"#, 4979, "Point Z"),
        ("c12 no geometry", r#"[null,null]"#, 4326, "Unknown"),
        ("c14 four values", r#"[{"type":"Point","coordinates":[10,20,30,40]}]"#, 4326, "Point"),
        ("c15 measures, three values", r#"[{"type":"Point","coordinates":[10,20,30],"measures":{"enabled":true}}]"#, 4326, "Point"),
        ("c16 measures, four values", r#"[{"type":"Point","coordinates":[10,20,30,40],"measures":{"enabled":true}}]"#, 4979, "Point Z"),
        (
            "c17 unknown type, then 3-D",
            r#"[{"type":"Pointy","coordinates":[1,2,3]},{"type":"Point","coordinates":[10,20,30]}]"#,
            4326,
            "Unknown",
        ),
        (
            "c21 empty coordinates, then 3-D",
            r#"[{"type":"Point","coordinates":[]},{"type":"Point","coordinates":[10,20,30]}]"#,
            4979,
            "Point Z",
        ),
        (
            "c22 multipolygon 3-D",
            r#"[{"type":"MultiPolygon","coordinates":[[[[0,0,1],[1,0,1],[1,1,1],[0,0,1]]]]}]"#,
            4979,
            "MultiPolygon Z",
        ),
        (
            "c23 2-D exterior, 3-D hole",
            r#"[{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]],[[0.2,0.2,5],[0.4,0.2,5],[0.4,0.4,5],[0.2,0.2,5]]]}]"#,
            4326,
            "Polygon",
        ),
        (
            "c25 mixed types, 2-D",
            r#"[{"type":"Point","coordinates":[10,20]},{"type":"LineString","coordinates":[[1,2],[3,4]]}]"#,
            4326,
            "Unknown",
        ),
    ];

    /// A FeatureCollection of features with these geometries (a JSON array), without `crs`.
    fn collection(geometries: &str) -> String {
        let gs: Vec<Value> = serde_json::from_str(geometries).unwrap();
        let features: Vec<Value> =
            gs.into_iter().enumerate().map(|(i, g)| json!({"type": "Feature", "properties": {"i": i}, "geometry": g})).collect();
        json!({"type": "FeatureCollection", "features": features}).to_string()
    }

    #[test]
    fn the_layer_type_and_the_default_crs_are_gdals() {
        for (name, geometries, epsg, declared) in GDAL_LAYER_TYPES {
            let text = collection(geometries);
            let (l, types) = with_declared_geometry_types(|| parse(text.as_bytes(), "f").unwrap());
            assert_eq!(l.crs_epsg(), Some(*epsg), "{name}: parsed whole");
            assert_eq!(types, [*declared], "{name}: parsed whole");
            // As the streaming reader scans: each feature's raw text, in parallel parts merged
            // in order (the first observed alone), then the layer.
            let doc: Value = serde_json::from_str(&text).unwrap();
            let raw: Vec<Vec<u8>> = doc["features"].as_array().unwrap().iter().map(|f| f.to_string().into_bytes()).collect();
            for split in 1..=raw.len() {
                let mut scan = SchemaScan::default();
                scan.observe_raw(&raw[0]).unwrap();
                let mut parts = [scan.empty_like(), scan.empty_like()];
                for (i, r) in raw.iter().enumerate().skip(1) {
                    parts[usize::from(i >= split)].observe_raw(r).unwrap();
                }
                for p in parts {
                    scan.merge(p);
                }
                assert_eq!(scan.geojson_layer("f", None).crs_epsg(), Some(*epsg), "{name}: streamed, split at {split}");
                assert_eq!(scan.geom.name(), *declared, "{name}: streamed, split at {split}");
            }
        }
        // Outside a call, nothing is kept; a call keeps only its own reads.
        let text = collection(r#"[{"type":"Point","coordinates":[10,20]}]"#);
        parse(text.as_bytes(), "f").unwrap();
        let ((), types) = with_declared_geometry_types(|| ());
        assert!(types.is_empty(), "{types:?}");
    }

    #[test]
    fn a_crs_member_bare_geometries_single_features_and_sequences_as_gdal() {
        // A `crs` member is kept whatever the heights (c13).
        let text = r#"{"type":"FeatureCollection","crs":{"type":"name","properties":{"name":"urn:ogc:def:crs:EPSG::32633"}},
            "features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[500000,10,30]}}]}"#;
        let (l, types) = with_declared_geometry_types(|| parse(text.as_bytes(), "f").unwrap());
        assert_eq!((l.crs_epsg(), types), (Some(32633), vec!["Point Z".to_string()]));
        // A bare geometry: the type of the geometry GDAL reads, 3-D when any position is (c18,
        // c19, c24); a single feature, its geometry's type (c20).
        for (text, epsg, declared) in [
            (r#"{"type":"LineString","coordinates":[[1,2],[3,4,5]]}"#, 4979, "LineString Z"),
            (r#"{"type":"Point","coordinates":[10,20]}"#, 4326, "Point"),
            (
                r#"{"type":"GeometryCollection","geometries":[{"type":"Point","coordinates":[10,20]},{"type":"Point","coordinates":[10,20,30]}]}"#,
                4979,
                "GeometryCollection Z",
            ),
            (r#"{"type":"Feature","properties":{"a":1},"geometry":{"type":"Point","coordinates":[10,20,30]}}"#, 4979, "Point Z"),
        ] {
            let (l, types) = with_declared_geometry_types(|| parse(text.as_bytes(), "f").unwrap());
            assert_eq!((l.crs_epsg(), types), (Some(epsg), vec![declared.to_string()]), "{text}");
        }
        // GeoJSONSeq: WGS 84 whatever the heights, as GDAL's driver; its type as GeoJSON's (c26).
        let seq = "{\"type\":\"Feature\",\"properties\":{},\"geometry\":{\"type\":\"Point\",\"coordinates\":[10,20,30]}}\n";
        let (l, types) = with_declared_geometry_types(|| parse_seq(seq.as_bytes(), "f").unwrap());
        assert_eq!((l.crs_epsg(), types), (Some(4326), vec!["Point Z".to_string()]));
    }

    #[test]
    fn the_scan_reads_any_geometry_member_and_keeps_the_last_of_a_repeated_one() {
        // Geometries that are not objects are no geometry; a repeated member keeps its last value.
        for (raw, has_z) in [
            (r#"{"type":"Feature","properties":{},"geometry":"x"}"#, false),
            (r#"{"type":"Feature","properties":{},"geometry":[1,2,3]}"#, false),
            (r#"{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":{"a":1}}}"#, false),
            (
                r#"{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[1,2]},"geometry":{"type":"Point","coordinates":[1,2,3]}}"#,
                true,
            ),
            (
                r#"{"properties":{"a":1},"type":"Feature","geometry":{"coordinates":[[[1,2,3]]],"type":"MultiLineString"},"properties":{"b":2}}"#,
                true,
            ),
        ] {
            let mut scan = SchemaScan::default();
            scan.observe_raw(b"{\"type\":\"Feature\",\"properties\":{},\"geometry\":null}").unwrap();
            scan.observe_raw(raw.as_bytes()).unwrap();
            assert_eq!(scan.geom.has_z(), has_z, "{raw}");
        }
    }

    #[test]
    fn a_document_too_large_for_memory_is_an_error() {
        // The address space for parsing is reserved first: an input whose parse cannot fit fails.
        assert!(reserve_parse_memory(usize::MAX / 4).is_err());
        assert!(reserve_parse_memory(1 << 20).is_ok());
    }

    #[test]
    fn types_fields_as_gdal_widens_them() {
        let text = r#"{"type":"FeatureCollection","features":[
            {"type":"Feature","properties":{"bi":true,"br":false,"is":1,"t":"10:20:30","bad":"2020-13-45","u":"2020-01-0é 10:20:30"},"geometry":null},
            {"type":"Feature","properties":{"bi":3,"br":1.5,"is":"x","t":"23:59:59","bad":"x","u":"y"},"geometry":null}]}"#;
        let l = parse(text.as_bytes(), "f").unwrap();
        let t: Vec<FieldType> = l.schema.fields().iter().map(|f| f.field_type).collect();
        assert_eq!(t, [FieldType::Integer, FieldType::Float, FieldType::Json, FieldType::Text, FieldType::Text, FieldType::Text]);
        assert_eq!(l.features[0].attributes[0], FieldValue::Integer(1));
        assert_eq!(l.features[1].attributes[1], FieldValue::Float(1.5));
        assert_eq!(l.features[0].attributes[2], FieldValue::Text("1".into()));
        assert!(
            parse(
                br#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"a":9223372036854775808},"geometry":null}]}"#,
                "f"
            )
            .is_err()
        );
    }
}

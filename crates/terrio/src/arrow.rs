//! Vector layers as Apache Arrow record batches, with GeoArrow geometries, and back.
//!
//! [`layer_to_arrow`] turns a layer into a schema and record batches: the attributes as typed
//! columns (integers as `Int64`, reals as `Float64`, `Boolean`, `Utf8`, `Binary`, dates as
//! `Date32`, date-times as millisecond timestamps, JSON as `Utf8` marked `arrow.json`) and the
//! geometries in one column, as [GeoArrow](https://geoarrow.org) describes them: Well-Known Binary
//! (`geoarrow.wkb`, the default, which holds any geometry) or the native encodings
//! (`geoarrow.point`, `geoarrow.linestring`, `geoarrow.polygon` and their multi-part forms, with
//! separated or interleaved coordinates), for layers whose geometries share one type. The CRS goes
//! in the column's extension metadata as PROJJSON.
//!
//! [`layer_from_arrow`] reads what other libraries produce: GeoArrow columns (WKB, WKT or native,
//! with their CRS), GDAL's `ogc.wkb` columns, tables with GeoParquet `geo` metadata (as pyarrow
//! reads GeoParquet files), and a binary column named `geometry` or `wkb_geometry` without
//! metadata. Struct columns are flattened into `parent.child` fields, and lists and maps become
//! JSON, as GDAL's Arrow and Parquet drivers do.
//!
//! [`to_ipc_stream`] and [`from_ipc`] write and read the Arrow IPC formats (streams and files).
//!
//! ```
//! use terrio::arrow::{layer_from_arrow, layer_to_arrow, ArrowWriteOptions, GeometryEncoding};
//! let mut layer = terrio::Layer::new("places").with_epsg(4326);
//! layer.add_feature(Some(terrio::Geometry::point(10.0, 50.0)), &[]).unwrap();
//! let options = ArrowWriteOptions { geometry_encoding: GeometryEncoding::GeoArrow, ..Default::default() };
//! let table = layer_to_arrow(&layer, &options)?;
//! let back = layer_from_arrow(&table.schema, &table.batches, "places")?;
//! assert_eq!(back.features[0].geometry, layer.features[0].geometry);
//! assert_eq!(back.crs_epsg(), Some(4326));
//! # Ok::<(), terrio::Error>(())
//! ```

use crate::crs::SpatialRef;
use crate::drivers::vector::datetime;
use crate::drivers::vector::geoparquet as gp;
use crate::drivers::vector::profile::LayerProfile;
use crate::{Error, Result};
use arrow_array::builder::{BinaryBuilder, StringBuilder};
use arrow_array::cast::AsArray;
use arrow_array::{
    Array, ArrayRef, BooleanArray, Date32Array, FixedSizeListArray, Float64Array, Int64Array, ListArray, RecordBatch, StructArray,
    TimestampMillisecondArray,
};
use arrow_schema::{DataType, Field, Fields, Schema, SchemaRef, TimeUnit};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Ring};

pub use arrow_array;
pub use arrow_schema;

/// The field metadata key naming an Arrow extension type.
pub const EXTENSION_NAME: &str = "ARROW:extension:name";
/// The field metadata key holding an Arrow extension type's metadata.
pub const EXTENSION_METADATA: &str = "ARROW:extension:metadata";

/// How the geometry column is encoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GeometryEncoding {
    /// ISO Well-Known Binary (`geoarrow.wkb`): any geometry, the most widely read.
    #[default]
    Wkb,
    /// GeoArrow's native encodings (`geoarrow.point`, `geoarrow.multipolygon`...): nested lists of
    /// coordinates. Needs geometries of one type (or of a type and its multi-part form, written
    /// as the multi-part form); geometry collections cannot be written this way.
    GeoArrow,
}

impl GeometryEncoding {
    /// Parses `wkb` or `geoarrow` (also `native`), ignoring case.
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "wkb" => Ok(Self::Wkb),
            "geoarrow" | "native" => Ok(Self::GeoArrow),
            other => Err(Error::InvalidArgument(format!("unknown geometry encoding '{other}' (wkb, geoarrow)"))),
        }
    }
}

/// How native GeoArrow coordinates are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CoordinateLayout {
    /// A struct of `x`, `y` (and `z`, `m`) arrays.
    #[default]
    Separated,
    /// Fixed-size lists of `[x, y(, z)(, m)]`.
    Interleaved,
}

impl CoordinateLayout {
    /// Parses `separated` or `interleaved`, ignoring case.
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "separated" | "struct" => Ok(Self::Separated),
            "interleaved" => Ok(Self::Interleaved),
            other => Err(Error::InvalidArgument(format!("unknown coordinate layout '{other}' (separated, interleaved)"))),
        }
    }
}

/// Options for [`layer_to_arrow`].
#[derive(Debug, Clone)]
pub struct ArrowWriteOptions {
    /// The geometry encoding.
    pub geometry_encoding: GeometryEncoding,
    /// The coordinate layout of native encodings.
    pub coordinates: CoordinateLayout,
    /// The name of the geometry column (`geometry` by default).
    pub geometry_column: String,
    /// When set, a first `Int64` column of this name holds the feature ids.
    pub fid_column: Option<String>,
    /// The most rows in a record batch (65536 by default).
    pub batch_size: usize,
}

impl Default for ArrowWriteOptions {
    fn default() -> Self {
        Self {
            geometry_encoding: GeometryEncoding::Wkb,
            coordinates: CoordinateLayout::Separated,
            geometry_column: "geometry".into(),
            fid_column: None,
            batch_size: 65536,
        }
    }
}

/// A layer as Arrow data: a schema and the record batches that follow it.
#[derive(Debug, Clone)]
pub struct ArrowTable {
    /// The schema.
    pub schema: SchemaRef,
    /// The record batches.
    pub batches: Vec<RecordBatch>,
}

impl ArrowTable {
    /// The number of rows.
    pub fn num_rows(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }
}

fn arrow_err(e: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("Arrow: {e}"))
}

/// The GeoArrow native geometry types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Native {
    Point,
    LineString,
    Polygon,
    MultiPoint,
    MultiLineString,
    MultiPolygon,
}

impl Native {
    fn from_type(t: GeometryType) -> Option<Self> {
        Some(match t {
            GeometryType::Point => Self::Point,
            GeometryType::LineString => Self::LineString,
            GeometryType::Polygon => Self::Polygon,
            GeometryType::MultiPoint => Self::MultiPoint,
            GeometryType::MultiLineString => Self::MultiLineString,
            GeometryType::MultiPolygon => Self::MultiPolygon,
            GeometryType::GeometryCollection => return None,
        })
    }

    fn from_name(name: &str) -> Option<Self> {
        Some(match name.to_ascii_lowercase().as_str() {
            "point" => Self::Point,
            "linestring" => Self::LineString,
            "polygon" => Self::Polygon,
            "multipoint" => Self::MultiPoint,
            "multilinestring" => Self::MultiLineString,
            "multipolygon" => Self::MultiPolygon,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Point => "point",
            Self::LineString => "linestring",
            Self::Polygon => "polygon",
            Self::MultiPoint => "multipoint",
            Self::MultiLineString => "multilinestring",
            Self::MultiPolygon => "multipolygon",
        }
    }

    /// The names of the list levels, outermost first.
    fn levels(self) -> &'static [&'static str] {
        match self {
            Self::Point => &[],
            Self::LineString => &["vertices"],
            Self::Polygon => &["rings", "vertices"],
            Self::MultiPoint => &["points"],
            Self::MultiLineString => &["linestrings", "vertices"],
            Self::MultiPolygon => &["polygons", "rings", "vertices"],
        }
    }

    fn geometry_type(self) -> GeometryType {
        match self {
            Self::Point => GeometryType::Point,
            Self::LineString => GeometryType::LineString,
            Self::Polygon => GeometryType::Polygon,
            Self::MultiPoint => GeometryType::MultiPoint,
            Self::MultiLineString => GeometryType::MultiLineString,
            Self::MultiPolygon => GeometryType::MultiPolygon,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Writing
// ---------------------------------------------------------------------------------------------

/// How an attribute column is stored.
#[derive(Debug, Clone, Copy)]
enum Kind {
    Int64,
    Float64,
    Bool,
    Utf8,
    Json,
    Binary,
    Date32,
    Timestamp { utc: bool },
}

impl Kind {
    fn data_type(self) -> DataType {
        match self {
            Kind::Int64 => DataType::Int64,
            Kind::Float64 => DataType::Float64,
            Kind::Bool => DataType::Boolean,
            Kind::Utf8 | Kind::Json => DataType::Utf8,
            Kind::Binary => DataType::Binary,
            Kind::Date32 => DataType::Date32,
            Kind::Timestamp { utc } => DataType::Timestamp(TimeUnit::Millisecond, utc.then(|| "UTC".into())),
        }
    }

    fn build(self, features: &[Feature], i: usize) -> ArrayRef {
        let values = features.iter().map(|f| f.attributes.get(i).filter(|v| !v.is_null()));
        match self {
            Kind::Int64 => Arc::new(values.map(|v| v.and_then(as_integer)).collect::<Int64Array>()),
            Kind::Float64 => Arc::new(values.map(|v| v.and_then(FieldValue::as_f64)).collect::<Float64Array>()),
            Kind::Bool => Arc::new(values.map(|v| v.and_then(FieldValue::as_bool)).collect::<BooleanArray>()),
            Kind::Binary => {
                let mut b = BinaryBuilder::new();
                for v in values {
                    b.append_option(v.and_then(FieldValue::as_blob));
                }
                Arc::new(b.finish())
            }
            Kind::Date32 => {
                Arc::new(values.map(|v| v.and_then(FieldValue::as_str).and_then(datetime::parse_date)).collect::<Date32Array>())
            }
            Kind::Timestamp { utc } => {
                let a: TimestampMillisecondArray =
                    values.map(|v| v.and_then(FieldValue::as_str).and_then(datetime::parse_datetime)).collect();
                Arc::new(if utc { a.with_timezone("UTC") } else { a })
            }
            Kind::Utf8 | Kind::Json => {
                let mut b = StringBuilder::new();
                for v in values {
                    match v {
                        None => b.append_null(),
                        Some(FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s)) => b.append_value(s),
                        Some(FieldValue::Integer(x)) => b.append_value(x.to_string()),
                        Some(FieldValue::Float(x)) => b.append_value(x.to_string()),
                        Some(FieldValue::Boolean(x)) => b.append_value(x.to_string()),
                        Some(FieldValue::Blob(x)) => b.append_value(String::from_utf8_lossy(x)),
                        Some(FieldValue::Null) => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
        }
    }
}

/// An integer value: integers, and reals that are whole numbers.
fn as_integer(v: &FieldValue) -> Option<i64> {
    match v {
        FieldValue::Integer(x) => Some(*x),
        FieldValue::Float(x) if x.fract() == 0.0 && x.abs() < 9.2e18 => Some(*x as i64),
        FieldValue::Boolean(b) => Some(i64::from(*b)),
        FieldValue::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn column_kind(profile: &LayerProfile, i: usize, t: FieldType) -> Kind {
    let p = profile.fields.get(i).cloned().unwrap_or_default();
    match t {
        FieldType::Integer => Kind::Int64,
        FieldType::Float => Kind::Float64,
        FieldType::Boolean => Kind::Bool,
        FieldType::Blob => Kind::Binary,
        FieldType::Json => Kind::Json,
        FieldType::Date if p.all_dates => Kind::Date32,
        FieldType::DateTime if p.all_datetimes => {
            // One time zone for the whole column: UTC when every value has an offset, local
            // (none) when none has; text otherwise.
            match p.with_offset {
                0 => Kind::Timestamp { utc: false },
                n if n == p.non_null => Kind::Timestamp { utc: true },
                _ => Kind::Utf8,
            }
        }
        _ => Kind::Utf8,
    }
}

/// The GeoArrow extension metadata of a layer's geometry column: its CRS as PROJJSON (or, for
/// what cannot be converted, an authority code or the WKT). `{}` for an unknown CRS.
fn crs_metadata(layer: &Layer) -> String {
    let srs = SpatialRef::from_layer(layer);
    if srs.is_unknown() {
        return "{}".into();
    }
    // As GDAL's Arrow streams: the EPSG definition of the code the CRS is identified as (an ESRI
    // `.prj` included), otherwise the stored definition.
    let epsg = crate::drivers::vector::naming_epsg(layer);
    let wkt = match epsg.and_then(|c| SpatialRef::from_epsg(c).ok()) {
        Some(s) => crate::drivers::wkt::gdal_wkt(&s),
        None => srs.to_wkt(),
    };
    // An ESRI CRS keeps its ESRI code, as PROJ identifies it.
    let esri = srs.esri.filter(|_| epsg.is_none());
    let meta = match wkt.as_deref().and_then(|w| crate::drivers::vector::projjson::from_wkt1(w, epsg)) {
        Some(mut projjson) => {
            if let Some(code) = esri {
                projjson["id"] = json!({"authority": "ESRI", "code": code});
            }
            json!({"crs": projjson, "crs_type": "projjson"})
        }
        None => match (epsg, esri, wkt) {
            (Some(code), _, _) => json!({"crs": format!("EPSG:{code}"), "crs_type": "authority_code"}),
            (None, Some(code), _) => json!({"crs": format!("ESRI:{code}"), "crs_type": "authority_code"}),
            (None, None, Some(w)) => json!({"crs": w}),
            (None, None, None) => json!({}),
        },
    };
    meta.to_string()
}

/// The native type a layer's geometries can be written as: their shared type, a mix of a type
/// and its multi-part form counting as the multi-part form.
fn native_type(layer: &Layer, profile: &LayerProfile) -> Result<Native> {
    let t = match profile.features_type() {
        Some(t) => Some(t),
        None if profile.no_geometry() => layer.geom_type,
        None => {
            return Err(Error::Unsupported(format!(
                "layer '{}' mixes geometry types, which GeoArrow's native encodings cannot hold: use WKB",
                layer.name
            )));
        }
    };
    match t.map(Native::from_type) {
        Some(Some(n)) => Ok(n),
        Some(None) => Err(Error::Unsupported(format!(
            "layer '{}' holds geometry collections, which GeoArrow's native encodings cannot hold: use WKB",
            layer.name
        ))),
        None => Err(Error::Unsupported(format!("layer '{}' has no geometry type for a native GeoArrow column: use WKB", layer.name))),
    }
}

/// Coordinates being gathered for a native column.
struct CoordBuf {
    z: bool,
    m: bool,
    values: [Vec<f64>; 4],
}

impl CoordBuf {
    fn new(z: bool, m: bool) -> Self {
        Self { z, m, values: Default::default() }
    }

    fn len(&self) -> usize {
        self.values[0].len()
    }

    fn push(&mut self, c: &Coord) {
        self.values[0].push(c.x);
        self.values[1].push(c.y);
        if self.z {
            self.values[2].push(c.z.unwrap_or(f64::NAN));
        }
        if self.m {
            self.values[3].push(c.m.unwrap_or(f64::NAN));
        }
    }

    /// A closed ring: its coordinates, and the first again when the ring does not end on it.
    fn push_ring(&mut self, r: &Ring) {
        r.0.iter().for_each(|c| self.push(c));
        if let (Some(first), Some(last)) = (r.0.first(), r.0.last())
            && (r.0.len() == 1 || first.x != last.x || first.y != last.y)
        {
            self.push(first);
        }
    }

    fn dims(&self) -> Vec<&'static str> {
        let mut d = vec!["x", "y"];
        if self.z {
            d.push("z");
        }
        if self.m {
            d.push("m");
        }
        d
    }

    fn data_type(z: bool, m: bool, layout: CoordinateLayout) -> DataType {
        let dims = Self::new(z, m).dims();
        match layout {
            CoordinateLayout::Separated => DataType::Struct(dims.iter().map(|d| Field::new(*d, DataType::Float64, false)).collect()),
            CoordinateLayout::Interleaved => {
                DataType::FixedSizeList(Arc::new(Field::new(dims.concat(), DataType::Float64, false)), dims.len() as i32)
            }
        }
    }

    fn finish(self, layout: CoordinateLayout, nulls: Option<arrow_buffer::NullBuffer>) -> Result<ArrayRef> {
        let dims = self.dims();
        let [x, y, z, m] = self.values;
        let mut columns = vec![x, y];
        if self.z {
            columns.push(z);
        }
        if self.m {
            columns.push(m);
        }
        Ok(match layout {
            CoordinateLayout::Separated => {
                let fields: Fields = dims.iter().map(|d| Field::new(*d, DataType::Float64, false)).collect();
                let arrays: Vec<ArrayRef> = columns.into_iter().map(|c| Arc::new(Float64Array::from(c)) as ArrayRef).collect();
                Arc::new(StructArray::try_new(fields, arrays, nulls).map_err(arrow_err)?)
            }
            CoordinateLayout::Interleaved => {
                let n = columns.len();
                let len = columns[0].len();
                let mut flat = Vec::with_capacity(n * len);
                for i in 0..len {
                    flat.extend(columns.iter().map(|c| c[i]));
                }
                let field = Arc::new(Field::new(dims.concat(), DataType::Float64, false));
                Arc::new(FixedSizeListArray::try_new(field, n as i32, Arc::new(Float64Array::from(flat)), nulls).map_err(arrow_err)?)
            }
        })
    }
}

/// The data type of a native geometry column.
fn native_data_type(kind: Native, z: bool, m: bool, layout: CoordinateLayout) -> DataType {
    let mut t = CoordBuf::data_type(z, m, layout);
    for name in kind.levels().iter().rev() {
        t = DataType::List(Arc::new(Field::new(*name, t, false)));
    }
    t
}

/// Builds a native geometry column from features.
fn native_column(features: &[Feature], kind: Native, z: bool, m: bool, layout: CoordinateLayout) -> Result<ArrayRef> {
    let depth = kind.levels().len();
    let mut coords = CoordBuf::new(z, m);
    // Offsets of each list level, outermost first.
    let mut offsets: Vec<Vec<usize>> = vec![vec![0]; depth];
    let mut valid = Vec::with_capacity(features.len());
    let nan = Coord { x: f64::NAN, y: f64::NAN, z: None, m: None };
    for f in features {
        valid.push(f.geometry.is_some());
        let Some(g) = &f.geometry else {
            if kind == Native::Point {
                coords.push(&nan);
            }
            if let Some(level) = offsets.first_mut() {
                let last = *level.last().unwrap_or(&0);
                level.push(last);
            }
            continue;
        };
        // Closes a list at `level`: its end is the current length of the next level (or of the
        // coordinates).
        fn close(offsets: &mut [Vec<usize>], coords: &CoordBuf, level: usize) {
            let end = if level + 1 < offsets.len() { offsets[level + 1].len() - 1 } else { coords.len() };
            offsets[level].push(end);
        }
        let polygon = |offsets: &mut Vec<Vec<usize>>, coords: &mut CoordBuf, e: &Ring, h: &[Ring], level: usize| {
            for r in std::iter::once(e).chain(h) {
                coords.push_ring(r);
                close(offsets, coords, level + 1);
            }
            close(offsets, coords, level);
        };
        match (kind, g) {
            (Native::Point, Geometry::Point(c)) => coords.push(c),
            (Native::LineString, Geometry::LineString(cs)) | (Native::MultiPoint, Geometry::MultiPoint(cs)) => {
                cs.iter().for_each(|c| coords.push(c));
                close(&mut offsets, &coords, 0);
            }
            (Native::MultiPoint, Geometry::Point(c)) => {
                coords.push(c);
                close(&mut offsets, &coords, 0);
            }
            (Native::Polygon, Geometry::Polygon { exterior, interiors }) => polygon(&mut offsets, &mut coords, exterior, interiors, 0),
            (Native::MultiLineString, Geometry::LineString(cs)) => {
                cs.iter().for_each(|c| coords.push(c));
                close(&mut offsets, &coords, 1);
                close(&mut offsets, &coords, 0);
            }
            (Native::MultiLineString, Geometry::MultiLineString(ls)) => {
                for l in ls {
                    l.iter().for_each(|c| coords.push(c));
                    close(&mut offsets, &coords, 1);
                }
                close(&mut offsets, &coords, 0);
            }
            (Native::MultiPolygon, Geometry::Polygon { exterior, interiors }) => {
                polygon(&mut offsets, &mut coords, exterior, interiors, 1);
                close(&mut offsets, &coords, 0);
            }
            (Native::MultiPolygon, Geometry::MultiPolygon(ps)) => {
                for (e, h) in ps {
                    polygon(&mut offsets, &mut coords, e, h, 1);
                }
                close(&mut offsets, &coords, 0);
            }
            (k, g) => {
                return Err(Error::Unsupported(format!(
                    "a {} cannot be written in a geoarrow.{} column: use WKB",
                    g.geom_type().as_str(),
                    k.name()
                )));
            }
        }
    }
    let nulls = valid.iter().any(|v| !v).then(|| arrow_buffer::NullBuffer::from(valid));
    if depth == 0 {
        return coords.finish(layout, nulls);
    }
    // From the innermost level out: each list's items are the level below (or the coordinates).
    let mut child = coords.finish(layout, None)?;
    for (level, name) in kind.levels().iter().enumerate().rev() {
        let offs: Vec<i32> = offsets[level]
            .iter()
            .map(|&o| i32::try_from(o).map_err(|_| Error::Unsupported("too many coordinates for one Arrow batch".into())))
            .collect::<Result<_>>()?;
        let field = Arc::new(Field::new(*name, child.data_type().clone(), false));
        let list_nulls = if level == 0 { nulls.clone() } else { None };
        child = Arc::new(ListArray::try_new(field, arrow_buffer::OffsetBuffer::new(offs.into()), child, list_nulls).map_err(arrow_err)?);
    }
    Ok(child)
}

/// Converts a layer to an Arrow schema and record batches.
pub fn layer_to_arrow(layer: &Layer, options: &ArrowWriteOptions) -> Result<ArrowTable> {
    let enc = ArrowEncoder::new(layer, &LayerProfile::of(layer), options)?;
    let size = options.batch_size.max(1);
    let chunks: Vec<&[Feature]> = if layer.features.is_empty() { vec![&[]] } else { layer.features.chunks(size).collect() };
    let batches = chunks.into_iter().map(|c| enc.encode(c)).collect::<Result<Vec<_>>>()?;
    Ok(ArrowTable { schema: enc.schema(), batches })
}

/// Encodes the features of a layer as record batches of one schema, chunk by chunk: the column
/// types depend on the values (dates that parse, time zones, the geometry type of native
/// encodings), which a [`LayerProfile`] of all the features gives.
pub struct ArrowEncoder {
    kinds: Vec<Kind>,
    native: Option<(Native, bool, bool)>,
    schema: SchemaRef,
    options: ArrowWriteOptions,
}

impl ArrowEncoder {
    /// An encoder for the features of `layer` (its fields and CRS; its features are not read),
    /// which `profile` describes.
    pub fn new(layer: &Layer, profile: &LayerProfile, options: &ArrowWriteOptions) -> Result<Self> {
        let fields = layer.schema.fields();
        let kinds: Vec<Kind> = fields.iter().enumerate().map(|(i, f)| column_kind(profile, i, f.field_type)).collect();
        let mut schema_fields: Vec<Field> = Vec::new();
        if let Some(fid) = &options.fid_column {
            schema_fields.push(Field::new(fid, DataType::Int64, false));
        }
        for (f, k) in fields.iter().zip(&kinds) {
            let mut field = Field::new(&f.name, k.data_type(), true);
            if matches!(k, Kind::Json) {
                field = field.with_metadata(HashMap::from([(EXTENSION_NAME.to_string(), "arrow.json".to_string())]));
            }
            schema_fields.push(field);
        }
        let native = match options.geometry_encoding {
            GeometryEncoding::Wkb => None,
            GeometryEncoding::GeoArrow => Some((native_type(layer, profile)?, profile.geometry.any_z, profile.geometry.has_m)),
        };
        let (ext, geom_type) = match native {
            None => ("geoarrow.wkb".to_string(), DataType::Binary),
            Some((kind, z, m)) => (format!("geoarrow.{}", kind.name()), native_data_type(kind, z, m, options.coordinates)),
        };
        let metadata = HashMap::from([(EXTENSION_NAME.to_string(), ext), (EXTENSION_METADATA.to_string(), crs_metadata(layer))]);
        schema_fields.push(Field::new(&options.geometry_column, geom_type, true).with_metadata(metadata));
        Ok(Self { kinds, native, schema: Arc::new(Schema::new(schema_fields)), options: options.clone() })
    }

    /// Whether the schema depends on the values of the features of `layer`, so that
    /// [`ArrowEncoder::new`] needs a profile of them: layers with date or date-time fields, and
    /// native geometry encodings. Otherwise an empty profile gives the same schema.
    pub fn needs_profile(layer: &Layer, options: &ArrowWriteOptions) -> bool {
        options.geometry_encoding != GeometryEncoding::Wkb
            || layer.schema.fields().iter().any(|f| matches!(f.field_type, FieldType::Date | FieldType::DateTime))
    }

    /// The schema of the record batches.
    pub fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }

    /// Encodes features as a record batch.
    pub fn encode(&self, chunk: &[Feature]) -> Result<RecordBatch> {
        let schema = &self.schema;
        let mut columns: Vec<ArrayRef> = Vec::with_capacity(schema.fields().len());
        if self.options.fid_column.is_some() {
            columns.push(Arc::new(chunk.iter().map(|f| Some(f.fid as i64)).collect::<Int64Array>()));
        }
        columns.extend(self.kinds.iter().enumerate().map(|(i, k)| k.build(chunk, i)));
        columns.push(match self.native {
            None => {
                let mut b = BinaryBuilder::new();
                for f in chunk {
                    b.append_option(f.geometry.as_ref().map(Geometry::to_wkb));
                }
                Arc::new(b.finish())
            }
            Some((kind, z, m)) => native_column(chunk, kind, z, m, self.options.coordinates)?,
        });
        RecordBatch::try_new(schema.clone(), columns).map_err(arrow_err)
    }
}

/// Writes a layer as an Arrow IPC stream (the `.arrows` format, `application/vnd.apache.arrow.stream`).
pub fn to_ipc_stream(layer: &Layer, options: &ArrowWriteOptions) -> Result<Vec<u8>> {
    let table = layer_to_arrow(layer, options)?;
    let mut out = Vec::new();
    {
        let mut w = arrow_ipc::writer::StreamWriter::try_new(&mut out, &table.schema).map_err(arrow_err)?;
        for b in &table.batches {
            w.write(b).map_err(arrow_err)?;
        }
        w.finish().map_err(arrow_err)?;
    }
    Ok(out)
}

/// Reads an Arrow IPC stream or file (Feather 2) held in memory as a layer named `name`.
pub fn from_ipc(bytes: &[u8], name: &str) -> Result<Layer> {
    let cursor = std::io::Cursor::new(bytes);
    let (schema, batches) = if bytes.starts_with(b"ARROW1") {
        let r = arrow_ipc::reader::FileReader::try_new(cursor, None).map_err(arrow_err)?;
        let schema = r.schema();
        (schema, r.collect::<std::result::Result<Vec<_>, _>>().map_err(arrow_err)?)
    } else {
        let r = arrow_ipc::reader::StreamReader::try_new(cursor, None).map_err(arrow_err)?;
        let schema = r.schema();
        (schema, r.collect::<std::result::Result<Vec<_>, _>>().map_err(arrow_err)?)
    };
    layer_from_arrow(&schema, &batches, name)
}

// ---------------------------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------------------------

/// How a geometry column is encoded.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Encoding {
    Wkb,
    Wkt,
    Native(Native),
}

/// The geometry column of a schema: its index, encoding and CRS (`None` when the metadata does
/// not say), with the other columns that are not attributes.
struct GeometryColumn {
    index: usize,
    encoding: Encoding,
    crs: Option<Value>,
    hidden: Vec<usize>,
}

/// The encoding an extension name stands for. `Err` for GeoArrow types terrio cannot read.
fn extension_encoding(name: &str) -> Option<Result<Encoding>> {
    let n = name.to_ascii_lowercase();
    match n.as_str() {
        "geoarrow.wkb" | "ogc.wkb" => Some(Ok(Encoding::Wkb)),
        "geoarrow.wkt" | "ogc.wkt" => Some(Ok(Encoding::Wkt)),
        _ => {
            let t = n.strip_prefix("geoarrow.")?;
            Some(Native::from_name(t).map(Encoding::Native).ok_or_else(|| Error::Unsupported(format!("GeoArrow type '{name}'"))))
        }
    }
}

fn find_geometry(schema: &Schema) -> Result<Option<GeometryColumn>> {
    let mut geometry: Option<GeometryColumn> = None;
    let mut hidden = Vec::new();
    for (i, f) in schema.fields().iter().enumerate() {
        let Some(ext) = f.metadata().get(EXTENSION_NAME) else { continue };
        let Some(encoding) = extension_encoding(ext) else { continue };
        hidden.push(i);
        if geometry.is_none() {
            let meta: Value = f.metadata().get(EXTENSION_METADATA).and_then(|m| serde_json::from_str(m).ok()).unwrap_or(Value::Null);
            let crs = match (&meta["crs"], meta["crs_type"].as_str()) {
                (Value::Null, _) => None,
                (Value::String(s), Some("srid")) => Some(Value::String(format!("EPSG:{s}"))),
                (c, _) => Some(c.clone()),
            };
            geometry = Some(GeometryColumn { index: i, encoding: encoding?, crs, hidden: Vec::new() });
        }
    }
    if let Some(mut g) = geometry {
        g.hidden = hidden;
        return Ok(Some(g));
    }
    // GeoParquet metadata, as pyarrow keeps it when reading a GeoParquet file.
    if let Some(geo) = schema.metadata().get("geo").and_then(|g| serde_json::from_str::<Value>(g).ok()) {
        let primary = geo["primary_column"].as_str().unwrap_or("geometry");
        if let Some(index) = schema.fields().iter().position(|f| f.name() == primary) {
            let col = &geo["columns"][primary];
            let encoding = match col["encoding"].as_str().unwrap_or("WKB").to_ascii_lowercase().as_str() {
                "wkb" => Encoding::Wkb,
                other => {
                    Encoding::Native(Native::from_name(other).ok_or_else(|| Error::Unsupported(format!("GeoParquet encoding '{other}'")))?)
                }
            };
            // In GeoParquet, a missing CRS is OGC:CRS84.
            let crs = Some(col.get("crs").cloned().unwrap_or_else(|| Value::String("EPSG:4326".into())));
            let mut hidden: Vec<usize> = geo["columns"]
                .as_object()
                .map(|c| c.keys().filter_map(|k| schema.fields().iter().position(|f| f.name() == k)).collect())
                .unwrap_or_default();
            if let Some(cov) = col["covering"]["bbox"].as_object() {
                hidden.extend(
                    cov.values().filter_map(|p| p.get(0)?.as_str()).filter_map(|c| schema.fields().iter().position(|f| f.name() == c)),
                );
            }
            return Ok(Some(GeometryColumn { index, encoding, crs, hidden }));
        }
    }
    // A binary column named as GDAL and GeoPandas name geometry columns.
    let binary = |t: &DataType| matches!(t, DataType::Binary | DataType::LargeBinary | DataType::BinaryView);
    for name in ["geometry", "geom", "wkb_geometry", "wkb"] {
        if let Some(index) = schema.fields().iter().position(|f| f.name().eq_ignore_ascii_case(name) && binary(f.data_type())) {
            return Ok(Some(GeometryColumn { index, encoding: Encoding::Wkb, crs: None, hidden: vec![index] }));
        }
    }
    Ok(None)
}

/// Native GeoArrow coordinates, read by index.
#[allow(clippy::large_enum_variant)]
enum Coords {
    Separated { x: Float64Array, y: Float64Array, z: Option<Float64Array>, m: Option<Float64Array> },
    Interleaved { values: Float64Array, n: usize, z: Option<usize>, m: Option<usize> },
}

fn doubles(a: &ArrayRef) -> Result<Float64Array> {
    a.as_primitive_opt::<arrow_array::types::Float64Type>().cloned().ok_or_else(|| arrow_err("GeoArrow coordinates must be doubles"))
}

impl Coords {
    fn new(a: &dyn Array) -> Result<Self> {
        match a.data_type() {
            DataType::Struct(fields) => {
                let s = a.as_struct();
                let col = |n: &str| fields.iter().position(|f| f.name().eq_ignore_ascii_case(n)).map(|i| doubles(s.column(i))).transpose();
                let (Some(x), Some(y)) = (col("x")?, col("y")?) else { return Err(arrow_err("GeoArrow coordinates without x and y")) };
                Ok(Self::Separated { x, y, z: col("z")?, m: col("m")? })
            }
            DataType::FixedSizeList(field, n) => {
                let l = a.as_fixed_size_list();
                let n = *n as usize;
                let dims = field.name().to_ascii_lowercase();
                // The item field names the dimensions (`xy`, `xyz`, `xym`, `xyzm`); a bare list of
                // three is `xyz`.
                let dims =
                    if dims.len() == n && dims.starts_with("xy") { dims } else { ["xy", "xyz", "xyzm"][n.clamp(2, 4) - 2].to_string() };
                if n < 2 {
                    return Err(arrow_err("GeoArrow coordinates of fewer than two dimensions"));
                }
                Ok(Self::Interleaved { values: doubles(l.values())?, n, z: dims.find('z'), m: dims.find('m') })
            }
            t => Err(arrow_err(format!("GeoArrow coordinates of type {t}"))),
        }
    }

    fn get(&self, i: usize) -> Coord {
        match self {
            Self::Separated { x, y, z, m } => {
                Coord { x: x.value(i), y: y.value(i), z: z.as_ref().map(|a| a.value(i)), m: m.as_ref().map(|a| a.value(i)) }
            }
            Self::Interleaved { values, n, z, m } => {
                let v = values.values();
                let b = i * n;
                Coord { x: v[b], y: v[b + 1], z: z.map(|k| v[b + k]), m: m.map(|k| v[b + k]) }
            }
        }
    }

    fn range(&self, r: std::ops::Range<usize>) -> Vec<Coord> {
        r.map(|i| self.get(i)).collect()
    }
}

/// A list level: its offsets (into the level below) and items.
fn list_level(a: &dyn Array) -> Result<(Vec<usize>, ArrayRef)> {
    match a.data_type() {
        DataType::List(_) => {
            let l = a.as_list::<i32>();
            Ok((l.value_offsets().iter().map(|&o| o as usize).collect(), l.values().clone()))
        }
        DataType::LargeList(_) => {
            let l = a.as_list::<i64>();
            Ok((l.value_offsets().iter().map(|&o| o as usize).collect(), l.values().clone()))
        }
        t => Err(arrow_err(format!("a GeoArrow list of type {t}"))),
    }
}

/// A ring from coordinates, without its closing point (as terrio's rings are kept).
fn ring(mut cs: Vec<Coord>) -> Ring {
    if cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
        cs.pop();
    }
    Ring::new(cs)
}

/// Decodes a native GeoArrow column.
fn native_geometries(a: &dyn Array, kind: Native) -> Result<Vec<Option<Geometry>>> {
    let n = a.len();
    let null = |i: usize| a.is_null(i);
    let out = match kind {
        Native::Point => {
            let c = Coords::new(a)?;
            (0..n)
                .map(|i| {
                    let p = c.get(i);
                    (!null(i) && !(p.x.is_nan() && p.y.is_nan())).then_some(Geometry::Point(p))
                })
                .collect()
        }
        Native::LineString | Native::MultiPoint => {
            let (o, items) = list_level(a)?;
            let c = Coords::new(items.as_ref())?;
            (0..n)
                .map(|i| {
                    (!null(i)).then(|| {
                        let cs = c.range(o[i]..o[i + 1]);
                        if kind == Native::LineString { Geometry::LineString(cs) } else { Geometry::MultiPoint(cs) }
                    })
                })
                .collect()
        }
        Native::Polygon | Native::MultiLineString => {
            let (o1, items) = list_level(a)?;
            let (o2, items) = list_level(items.as_ref())?;
            let c = Coords::new(items.as_ref())?;
            (0..n)
                .map(|i| {
                    if null(i) {
                        return None;
                    }
                    let parts: Vec<Vec<Coord>> = (o1[i]..o1[i + 1]).map(|k| c.range(o2[k]..o2[k + 1])).collect();
                    if kind == Native::MultiLineString {
                        return Some(Geometry::MultiLineString(parts));
                    }
                    let mut rings = parts.into_iter().map(ring);
                    let exterior = rings.next()?;
                    Some(Geometry::Polygon { exterior, interiors: rings.collect() })
                })
                .collect()
        }
        Native::MultiPolygon => {
            let (o1, items) = list_level(a)?;
            let (o2, items) = list_level(items.as_ref())?;
            let (o3, items) = list_level(items.as_ref())?;
            let c = Coords::new(items.as_ref())?;
            (0..n)
                .map(|i| {
                    (!null(i)).then(|| {
                        let polygons = (o1[i]..o1[i + 1])
                            .filter_map(|p| {
                                let mut rings = (o2[p]..o2[p + 1]).map(|r| ring(c.range(o3[r]..o3[r + 1])));
                                let exterior = rings.next()?;
                                Some((exterior, rings.collect()))
                            })
                            .collect();
                        Geometry::MultiPolygon(polygons)
                    })
                })
                .collect()
        }
    };
    Ok(out)
}

/// Decodes a geometry column of a batch.
fn geometries(a: &dyn Array, encoding: Encoding) -> Result<Vec<Option<Geometry>>> {
    match encoding {
        Encoding::Native(kind) => native_geometries(a, kind),
        Encoding::Wkb => Ok((0..a.len())
            .map(|i| match gp::value(a, i, FieldType::Blob) {
                FieldValue::Blob(b) => Geometry::from_wkb(&b).ok(),
                _ => None,
            })
            .collect()),
        Encoding::Wkt => Ok((0..a.len())
            .map(|i| match gp::value(a, i, FieldType::Text) {
                FieldValue::Text(s) => crate::drivers::vector::wkt::parse(&s),
                _ => None,
            })
            .collect()),
    }
}

/// Converts Arrow record batches to a layer named `name`. The features get the row numbers as
/// ids.
pub fn layer_from_arrow(schema: &Schema, batches: &[RecordBatch], name: &str) -> Result<Layer> {
    let geometry = find_geometry(schema)?;
    let hidden: &[usize] = geometry.as_ref().map_or(&[], |g| &g.hidden);
    let mut layer = Layer::new(name);
    let mut columns: Vec<gp::Column> = Vec::new();
    for (i, f) in schema.fields().iter().enumerate() {
        if hidden.contains(&i) {
            continue;
        }
        let start = layer.schema.fields().len();
        gp::add_columns(&mut layer, &mut columns, vec![i], f.name().clone(), f.data_type());
        // JSON text, as GDAL marks it.
        if f.metadata().get(EXTENSION_NAME).is_some_and(|e| e == "arrow.json") && matches!(columns.last(), Some(c) if c.path == [i]) {
            let mut def = layer.schema.fields()[start].clone();
            def.field_type = FieldType::Json;
            rebuild_field(&mut layer, start, def);
            if let Some(c) = columns.last_mut() {
                c.field_type = FieldType::Json;
            }
        }
    }
    let mut fid = 0u64;
    let mut types: Vec<GeometryType> = Vec::new();
    for batch in batches {
        let geoms = match &geometry {
            Some(g) => geometries(batch.column(g.index).as_ref(), g.encoding)?,
            None => vec![None; batch.num_rows()],
        };
        let arrays: Vec<ArrayRef> = columns.iter().map(|c| gp::nested(batch, &c.path)).collect();
        for (row, geometry) in geoms.into_iter().enumerate() {
            if let Some(g) = &geometry
                && !types.contains(&g.geom_type())
            {
                types.push(g.geom_type());
            }
            let attributes = columns.iter().zip(&arrays).map(|(c, a)| gp::value(a.as_ref(), row, c.field_type)).collect();
            layer.push(Feature { fid, geometry, attributes });
            fid += 1;
        }
    }
    if let Some(g) = &geometry {
        if let Some(crs) = &g.crs {
            gp::apply_crs(&mut layer, crs);
        }
        layer.geom_type = match g.encoding {
            Encoding::Native(kind) => Some(kind.geometry_type()),
            _ => {
                let names: Vec<&str> = types.iter().map(|t| t.as_str()).collect();
                gp::declared_type(&names)
            }
        };
    }
    Ok(layer)
}

/// Replaces a field definition, keeping its position.
fn rebuild_field(layer: &mut Layer, index: usize, def: FieldDef) {
    let mut fields: Vec<FieldDef> = layer.schema.fields().to_vec();
    fields[index] = def;
    layer.schema = wbvector::Schema::new();
    for f in fields {
        layer.schema.add_field(f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Layer {
        let mut l = Layer::new("t").with_epsg(32633);
        l.add_field(FieldDef::new("n", FieldType::Integer));
        l.add_field(FieldDef::new("x", FieldType::Float));
        l.add_field(FieldDef::new("s", FieldType::Text));
        l.add_field(FieldDef::new("ok", FieldType::Boolean));
        l.add_field(FieldDef::new("d", FieldType::Date));
        l.add_field(FieldDef::new("t", FieldType::DateTime));
        l.add_field(FieldDef::new("j", FieldType::Json));
        l.add_field(FieldDef::new("b", FieldType::Blob));
        let square = |x: f64| {
            Geometry::polygon(vec![Coord::xy(x, 0.0), Coord::xy(x + 1.0, 0.0), Coord::xy(x + 1.0, 1.0), Coord::xy(x, 1.0)], vec![])
        };
        let holed = Geometry::Polygon {
            exterior: Ring::new(vec![Coord::xy(0.0, 0.0), Coord::xy(10.0, 0.0), Coord::xy(10.0, 10.0), Coord::xy(0.0, 10.0)]),
            interiors: vec![Ring::new(vec![Coord::xy(2.0, 2.0), Coord::xy(3.0, 2.0), Coord::xy(3.0, 3.0)])],
        };
        let geoms = [
            Some(square(0.0)),
            None,
            Some(holed),
            Some(Geometry::MultiPolygon(vec![(Ring::new(vec![Coord::xy(5.0, 5.0), Coord::xy(6.0, 5.0), Coord::xy(6.0, 6.0)]), vec![])])),
        ];
        for (i, g) in geoms.into_iter().enumerate() {
            l.push(Feature {
                fid: i as u64 + 1,
                geometry: g,
                attributes: vec![
                    FieldValue::Integer(i as i64 * 1_000_000_000_000),
                    if i == 1 { FieldValue::Null } else { FieldValue::Float(i as f64 / 3.0) },
                    FieldValue::Text(format!("é {i}")),
                    FieldValue::Boolean(i % 2 == 0),
                    FieldValue::Date(format!("2020-01-0{}", i + 1)),
                    FieldValue::DateTime(format!("2020-01-01T00:00:0{i}.250Z")),
                    FieldValue::Text(format!("{{\"a\": {i}}}")),
                    FieldValue::Blob(vec![i as u8, 0, 255]),
                ],
            });
        }
        l
    }

    fn round_trip(options: &ArrowWriteOptions) -> Layer {
        let l = sample();
        let table = layer_to_arrow(&l, options).unwrap();
        assert_eq!(table.num_rows(), 4);
        let back = layer_from_arrow(&table.schema, &table.batches, "t").unwrap();
        assert_eq!(back.crs_epsg(), Some(32633));
        assert_eq!(
            back.schema.fields().iter().map(|f| f.field_type).collect::<Vec<_>>(),
            l.schema.fields().iter().map(|f| f.field_type).collect::<Vec<_>>()
        );
        for (a, b) in back.features.iter().zip(&l.features) {
            assert_eq!(a.attributes, b.attributes);
        }
        back
    }

    #[test]
    fn wkb_round_trip() {
        let back = round_trip(&ArrowWriteOptions::default());
        let l = sample();
        for (a, b) in back.features.iter().zip(&l.features) {
            assert_eq!(a.geometry, b.geometry);
        }
        assert_eq!(back.geom_type, Some(GeometryType::MultiPolygon));
    }

    #[test]
    fn native_round_trips() {
        for coordinates in [CoordinateLayout::Separated, CoordinateLayout::Interleaved] {
            let back = round_trip(&ArrowWriteOptions {
                geometry_encoding: GeometryEncoding::GeoArrow,
                coordinates,
                batch_size: 3,
                ..Default::default()
            });
            assert_eq!(back.geom_type, Some(GeometryType::MultiPolygon));
            let l = sample();
            for (a, b) in back.features.iter().zip(&l.features) {
                // Polygons come back as one-part multipolygons.
                let expected = b.geometry.clone().map(|g| match g {
                    Geometry::Polygon { exterior, interiors } => Geometry::MultiPolygon(vec![(exterior, interiors)]),
                    g => g,
                });
                assert_eq!(a.geometry, expected);
            }
        }
    }

    #[test]
    fn native_points_and_lines_with_z() {
        let mut l = Layer::new("p");
        let c = |x: f64, z: Option<f64>| Coord { x, y: x * 2.0, z, m: None };
        l.push(Feature { fid: 0, geometry: Some(Geometry::LineString(vec![c(1.0, Some(5.0)), c(2.0, Some(6.0))])), attributes: vec![] });
        l.push(Feature { fid: 1, geometry: None, attributes: vec![] });
        l.push(Feature { fid: 2, geometry: Some(Geometry::LineString(vec![c(3.0, None), c(4.0, Some(1.0))])), attributes: vec![] });
        let o = ArrowWriteOptions { geometry_encoding: GeometryEncoding::GeoArrow, ..Default::default() };
        let t = layer_to_arrow(&l, &o).unwrap();
        let f = t.schema.field(0);
        assert_eq!(f.metadata()[EXTENSION_NAME], "geoarrow.linestring");
        assert_eq!(f.metadata()[EXTENSION_METADATA], "{}");
        let back = layer_from_arrow(&t.schema, &t.batches, "p").unwrap();
        assert_eq!(back.features[0].geometry, l.features[0].geometry);
        assert_eq!(back.features[1].geometry, None);
        let Some(Geometry::LineString(cs)) = &back.features[2].geometry else { panic!() };
        assert!(cs[0].z.is_some_and(f64::is_nan));
        assert_eq!(back.crs_epsg(), None);

        let mut pts = Layer::new("pts").with_epsg(4326);
        pts.push(Feature { fid: 0, geometry: Some(Geometry::point(1.0, 2.0)), attributes: vec![] });
        pts.push(Feature { fid: 1, geometry: None, attributes: vec![] });
        let o = ArrowWriteOptions { coordinates: CoordinateLayout::Interleaved, ..o };
        let t = layer_to_arrow(&pts, &o).unwrap();
        let meta: Value = serde_json::from_str(&t.schema.field(0).metadata()[EXTENSION_METADATA]).unwrap();
        assert_eq!(meta["crs_type"], "projjson");
        assert_eq!(meta["crs"]["id"]["code"], 4326);
        let back = layer_from_arrow(&t.schema, &t.batches, "pts").unwrap();
        assert_eq!(back.features[0].geometry, Some(Geometry::point(1.0, 2.0)));
        assert_eq!(back.features[1].geometry, None);
        assert_eq!(back.crs_epsg(), Some(4326));
    }

    #[test]
    fn native_refuses_mixed_types() {
        let mut l = Layer::new("m");
        l.add_feature(Some(Geometry::point(0.0, 0.0)), &[]).unwrap();
        l.add_feature(Some(Geometry::LineString(vec![Coord::xy(0.0, 0.0), Coord::xy(1.0, 1.0)])), &[]).unwrap();
        let o = ArrowWriteOptions { geometry_encoding: GeometryEncoding::GeoArrow, ..Default::default() };
        assert!(matches!(layer_to_arrow(&l, &o), Err(Error::Unsupported(_))));
        assert!(layer_to_arrow(&l, &ArrowWriteOptions::default()).is_ok());
    }

    #[test]
    fn ipc_round_trip() {
        let l = sample();
        let bytes = to_ipc_stream(&l, &ArrowWriteOptions { fid_column: Some("fid".into()), ..Default::default() }).unwrap();
        let back = from_ipc(&bytes, "t").unwrap();
        assert_eq!(back.schema.fields()[0].name, "fid");
        assert_eq!(back.features[3].attributes[0], FieldValue::Integer(4));
        assert_eq!(back.features[2].geometry, l.features[2].geometry);
    }

    #[test]
    fn reads_geoparquet_metadata_and_plain_wkb() {
        let mut meta = HashMap::new();
        meta.insert("geo".to_string(), r#"{"version":"1.1.0","primary_column":"geom","columns":{"geom":{"encoding":"WKB"}}}"#.to_string());
        let schema = Arc::new(Schema::new_with_metadata(
            vec![Field::new("geom", DataType::Binary, true), Field::new("v", DataType::Int32, true)],
            meta,
        ));
        let wkb = Geometry::point(3.0, 4.0).to_wkb();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(arrow_array::BinaryArray::from(vec![Some(wkb.as_slice())])), Arc::new(arrow_array::Int32Array::from(vec![7]))],
        )
        .unwrap();
        let l = layer_from_arrow(&schema, std::slice::from_ref(&batch), "g").unwrap();
        assert_eq!(l.crs_epsg(), Some(4326));
        assert_eq!(l.schema.fields().len(), 1);
        assert_eq!(l.features[0].geometry, Some(Geometry::point(3.0, 4.0)));
        assert_eq!(l.features[0].attributes, [FieldValue::Integer(7)]);
        // Without metadata, a binary column named geom is still WKB, with no CRS.
        let plain = Arc::new(Schema::new(schema.fields().clone()));
        let batch = RecordBatch::try_new(plain.clone(), batch.columns().to_vec()).unwrap();
        let l = layer_from_arrow(&plain, &[batch], "g").unwrap();
        assert_eq!(l.crs_epsg(), None);
        assert_eq!(l.features[0].geometry, Some(Geometry::point(3.0, 4.0)));
    }
}

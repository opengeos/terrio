//! GeoParquet, read and written with the Arrow interface of the `parquet` crate.
//!
//! Reading follows GDAL's Parquet driver: the bounding-box covering columns are hidden, struct
//! columns are flattened into `parent.child` fields, decimals are reals, lists and maps are JSON,
//! a missing `crs` means OGC:CRS84 (EPSG:4326 here), and the layer's geometry type comes from the
//! `geometry_types` metadata. Geometries may be WKB or GeoArrow native encodings.
//!
//! The writer follows GeoParquet 1.1 as GDAL writes it: WKB geometries in a `geometry` column, a
//! `geometry_bbox` covering column, Snappy compression, and the `geo` metadata with the geometry
//! types, bounds and CRS.

use crate::crs::SpatialRef;
use crate::{Error, Result};
use arrow_array::builder::{BinaryBuilder, Float64Builder, StringBuilder};
use arrow_array::cast::AsArray;
use arrow_array::types::{
    Date32Type, Date64Type, Decimal128Type, Decimal256Type, Float16Type, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type,
    Int64Type, TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType, TimestampSecondType, UInt8Type, UInt16Type,
    UInt32Type, UInt64Type,
};
use arrow_array::{
    Array, ArrayRef, BooleanArray, Date32Array, Float64Array, Int32Array, Int64Array, RecordBatch, StructArray, TimestampMillisecondArray,
};
use arrow_schema::{DataType, Field, Fields, Schema, TimeUnit};
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::properties::WriterProperties;
use serde_json::{Value, json};
use std::sync::Arc;
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, Ring};

/// An attribute column: the path from a top-level column through struct members.
pub(crate) struct Column {
    pub(crate) path: Vec<usize>,
    pub(crate) field_type: FieldType,
}

/// Reads a GeoParquet file, skipping the features whose bounding box is outside `bbox`
/// `(min_x, min_y, max_x, max_y)`: whole row groups by the statistics of the bounding-box
/// covering column, then rows by its values (or by their geometries without one). Feature ids
/// stay the row numbers in the file, as in GDAL.
pub fn read_filtered(path: &str, bbox: Option<(f64, f64, f64, f64)>) -> Result<Layer> {
    let name = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    parse_source(|| Ok(std::fs::File::open(path)?), name, bbox)
}

/// Parses a GeoParquet (or plain Parquet) file held in memory. `name` names the layer.
pub fn parse(bytes: Vec<u8>, name: &str) -> Result<Layer> {
    let bytes = bytes::Bytes::from(bytes);
    parse_source(|| Ok(bytes.clone()), name, None)
}

/// Parses a file whose bytes `open` gives (once, or twice with a filter).
fn parse_source<R: parquet::file::reader::ChunkReader + 'static>(
    open: impl Fn() -> Result<R>,
    name: &str,
    bbox: Option<(f64, f64, f64, f64)>,
) -> Result<Layer> {
    let err = |e: &dyn std::fmt::Display| Error::InvalidData(format!("Parquet: {e}"));
    let builder = ParquetRecordBatchReaderBuilder::try_new(open()?).map_err(|e| err(&e))?;
    let geo: Value = builder
        .metadata()
        .file_metadata()
        .key_value_metadata()
        .and_then(|kv| kv.iter().find(|k| k.key == "geo"))
        .and_then(|k| k.value.as_deref())
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or(Value::Null);
    let schema = builder.schema().clone();
    let primary = geo["primary_column"].as_str().unwrap_or("geometry").to_string();
    let col = &geo["columns"][&primary];
    let encoding = col["encoding"].as_str().unwrap_or("WKB").to_ascii_lowercase();

    // Columns that are not attributes: the geometry columns and the covering columns.
    let mut hidden: Vec<String> = geo["columns"].as_object().map(|c| c.keys().cloned().collect()).unwrap_or_default();
    if let Some(cov) = col["covering"]["bbox"].as_object() {
        hidden.extend(cov.values().filter_map(|p| p.get(0)?.as_str().map(str::to_string)));
    }
    let geom_index = schema.fields().iter().position(|f| f.name() == &primary && !geo.is_null());

    let mut layer = Layer::new(name);
    let mut columns = Vec::new();
    for (i, f) in schema.fields().iter().enumerate() {
        if !hidden.contains(f.name()) {
            add_columns(&mut layer, &mut columns, vec![i], f.name().clone(), f.data_type());
        }
    }

    // The covering column's members, by top-level column and member index.
    let covering: Option<[(usize, usize); 4]> = (|| {
        let cov = col["covering"]["bbox"].as_object()?;
        let member = |k: &str| -> Option<(usize, usize)> {
            let p = cov.get(k)?.as_array()?;
            let (c, m) = (p.first()?.as_str()?, p.get(1)?.as_str()?);
            let ci = schema.fields().iter().position(|f| f.name() == c)?;
            let DataType::Struct(members) = schema.field(ci).data_type() else { return None };
            Some((ci, members.iter().position(|f| f.name() == m)?))
        };
        Some([member("xmin")?, member("ymin")?, member("xmax")?, member("ymax")?])
    })();

    // With a filter: the row groups whose statistics overlap it, then the rows that do.
    let mut builder = builder;
    let mut fids: Option<Vec<u64>> = None;
    if let Some(rect) = bbox {
        let (groups, selection, kept) = select_rows(&builder, open()?, covering, geom_index, &encoding, rect)?;
        builder = builder.with_row_groups(groups).with_row_selection(selection);
        fids = Some(kept);
    }
    let reader = builder.with_batch_size(65536).build().map_err(|e| err(&e))?;
    let mut fid = 0u64;
    for batch in reader {
        let batch = batch.map_err(|e| err(&e))?;
        let geoms = geom_index.map(|i| batch.column(i).clone());
        let arrays: Vec<ArrayRef> = columns.iter().map(|c| nested(&batch, &c.path)).collect();
        for row in 0..batch.num_rows() {
            let geometry = geoms.as_ref().and_then(|g| geometry(g.as_ref(), row, &encoding));
            let attributes = columns.iter().zip(&arrays).map(|(c, a)| value(a.as_ref(), row, c.field_type)).collect();
            let id = fids.as_ref().and_then(|f| f.get(fid as usize).copied()).unwrap_or(fid);
            layer.push(Feature { fid: id, geometry, attributes });
            fid += 1;
        }
    }

    match col.get("crs") {
        _ if geo.is_null() => {}
        None => layer.set_crs_epsg(Some(4326)),
        Some(crs) => apply_crs(&mut layer, crs),
    }
    if let Some(types) = col["geometry_types"].as_array() {
        let types: Vec<&str> = types.iter().filter_map(Value::as_str).map(|t| t.trim_end_matches(" Z").trim_end_matches(" M")).collect();
        layer.geom_type = declared_type(&types);
    }
    Ok(layer)
}

/// Sets a layer's CRS from a GeoParquet or GeoArrow `crs` value: PROJJSON, or a string PROJ
/// understands (an authority code, WKT or a PROJ string). `null` leaves the CRS unknown.
pub(crate) fn apply_crs(layer: &mut Layer, crs: &Value) {
    match crs {
        Value::Null => {}
        Value::String(s) => {
            if let Ok(s) = SpatialRef::parse(s) {
                s.apply_to_layer(layer);
            }
        }
        crs => match projjson_epsg(crs) {
            Some(code) => match SpatialRef::from_epsg(code) {
                Ok(s) => s.apply_to_layer(layer),
                Err(_) => layer.set_crs_epsg(Some(code)),
            },
            // A CRS without an EPSG code: through WKT.
            None => {
                if let Some(wkt) = super::projjson::to_wkt1(crs) {
                    SpatialRef::from_wkt(&wkt).apply_to_layer(layer);
                }
            }
        },
    }
}

/// The row groups and rows a bounding-box filter keeps, with the file row numbers of the kept
/// rows. Row groups are kept when the statistics of the covering column overlap the rectangle
/// (or are missing); rows when their covering bounding box (or else their geometry's) does.
#[allow(clippy::type_complexity)]
fn select_rows<T: parquet::file::reader::ChunkReader + 'static>(
    builder: &ParquetRecordBatchReaderBuilder<T>,
    input: T,
    covering: Option<[(usize, usize); 4]>,
    geom_index: Option<usize>,
    encoding: &str,
    rect: (f64, f64, f64, f64),
) -> Result<(Vec<usize>, parquet::arrow::arrow_reader::RowSelection, Vec<u64>)> {
    use parquet::arrow::ProjectionMask;
    use parquet::arrow::arrow_reader::{RowSelection, RowSelector};
    use parquet::file::statistics::Statistics as S;
    let err = |e: &dyn std::fmt::Display| Error::InvalidData(format!("Parquet: {e}"));
    let meta = builder.metadata().clone();
    let descr = meta.file_metadata().schema_descr();
    let schema = builder.schema().clone();
    // The leaf (Parquet column) index of a covering member.
    let leaf = |(c, m): (usize, usize)| -> Option<usize> {
        let DataType::Struct(members) = schema.field(c).data_type() else { return None };
        let path = [schema.field(c).name().as_str(), members[m].name().as_str()];
        (0..descr.num_columns()).find(|&i| descr.column(i).path().parts() == path)
    };
    let leaves: Option<[usize; 4]> = covering.and_then(|cov| Some([leaf(cov[0])?, leaf(cov[1])?, leaf(cov[2])?, leaf(cov[3])?]));
    let stat = |rg: usize, leaf: usize, max: bool| -> Option<f64> {
        match meta.row_group(rg).column(leaf).statistics()? {
            S::Double(s) => if max { s.max_opt() } else { s.min_opt() }.copied(),
            S::Float(s) => if max { s.max_opt() } else { s.min_opt() }.map(|v| *v as f64),
            _ => None,
        }
    };
    let (fxmin, fymin, fxmax, fymax) = rect;
    let mut groups = Vec::new();
    let mut first_rows = Vec::new();
    let mut start = 0u64;
    for rg in 0..meta.num_row_groups() {
        let n = meta.row_group(rg).num_rows() as u64;
        let outside = leaves.is_some_and(|l| {
            stat(rg, l[0], false).is_some_and(|v| v > fxmax)
                || stat(rg, l[1], false).is_some_and(|v| v > fymax)
                || stat(rg, l[2], true).is_some_and(|v| v < fxmin)
                || stat(rg, l[3], true).is_some_and(|v| v < fymin)
        });
        if !outside {
            groups.push(rg);
            first_rows.push(start);
        }
        start += n;
    }
    // Read only the covering column (or the geometry) of those row groups to choose the rows.
    let mask = match (leaves, geom_index) {
        (Some(l), _) => ProjectionMask::leaves(descr, l),
        (None, Some(g)) => ProjectionMask::roots(descr, [g]),
        (None, None) => ProjectionMask::all(),
    };
    let src = ParquetRecordBatchReaderBuilder::new_with_metadata(
        input,
        parquet::arrow::arrow_reader::ArrowReaderMetadata::try_new(meta.clone(), Default::default()).map_err(|e| err(&e))?,
    );
    let reader = src.with_row_groups(groups.clone()).with_projection(mask).with_batch_size(65536).build().map_err(|e| err(&e))?;
    let mut keep: Vec<bool> = Vec::new();
    for batch in reader {
        let batch = batch.map_err(|e| err(&e))?;
        let f64s = |a: &ArrayRef| -> Vec<f64> { (0..a.len()).map(|i| json_value(a.as_ref(), i).as_f64().unwrap_or(f64::NAN)).collect() };
        if leaves.is_some() {
            let s = batch.column(0).as_struct();
            let cov = covering.unwrap_or_default();
            let DataType::Struct(members) = schema.field(cov[0].0).data_type() else { continue };
            let col = |m: usize| s.column_by_name(members[m].name()).map(f64s).unwrap_or_default();
            let (x0, y0, x1, y1) = (col(cov[0].1), col(cov[1].1), col(cov[2].1), col(cov[3].1));
            for i in 0..batch.num_rows() {
                let null = s.is_null(i);
                keep.push(!null && x0[i] <= fxmax && x1[i] >= fxmin && y0[i] <= fymax && y1[i] >= fymin);
            }
        } else if geom_index.is_some() {
            let g = batch.column(0);
            for i in 0..batch.num_rows() {
                let b = geometry(g.as_ref(), i, encoding).and_then(|g| g.bbox());
                keep.push(b.is_some_and(|b| b.min_x <= fxmax && b.max_x >= fxmin && b.min_y <= fymax && b.max_y >= fymin));
            }
        } else {
            keep.extend(std::iter::repeat_n(true, batch.num_rows()));
        }
    }
    // Run-length selectors, and the file row numbers of the kept rows.
    let mut selectors: Vec<RowSelector> = Vec::new();
    let mut kept = Vec::new();
    let mut i = 0usize;
    for (g, &rg) in groups.iter().enumerate() {
        let n = meta.row_group(rg).num_rows() as usize;
        for r in 0..n {
            let k = keep.get(i).copied().unwrap_or(false);
            if k {
                kept.push(first_rows[g] + r as u64);
            }
            match selectors.last_mut() {
                Some(s) if s.skip != k => s.row_count += 1,
                _ => selectors.push(if k { RowSelector::select(1) } else { RowSelector::skip(1) }),
            }
            i += 1;
        }
    }
    Ok((groups, RowSelection::from(selectors), kept))
}

/// Adds the fields of a column, flattening structs as GDAL does.
pub(crate) fn add_columns(layer: &mut Layer, columns: &mut Vec<Column>, path: Vec<usize>, name: String, t: &DataType) {
    if let DataType::Struct(fields) = t {
        for (i, f) in fields.iter().enumerate() {
            let mut p = path.clone();
            p.push(i);
            add_columns(layer, columns, p, format!("{name}.{}", f.name()), f.data_type());
        }
        return;
    }
    let field_type = field_type(t);
    layer.add_field(FieldDef::new(name, field_type));
    columns.push(Column { path, field_type });
}

pub(crate) fn field_type(t: &DataType) -> FieldType {
    use DataType as D;
    match t {
        D::Int8 | D::Int16 | D::Int32 | D::Int64 | D::UInt8 | D::UInt16 | D::UInt32 | D::UInt64 => FieldType::Integer,
        D::Float16 | D::Float32 | D::Float64 | D::Decimal128(..) | D::Decimal256(..) => FieldType::Float,
        D::Boolean => FieldType::Boolean,
        D::Binary | D::LargeBinary | D::FixedSizeBinary(_) | D::BinaryView => FieldType::Blob,
        D::Date32 | D::Date64 => FieldType::Date,
        D::Timestamp(..) => FieldType::DateTime,
        D::List(_) | D::LargeList(_) | D::FixedSizeList(..) | D::Map(..) | D::ListView(_) | D::LargeListView(_) => FieldType::Json,
        D::Dictionary(_, v) => field_type(v),
        _ => FieldType::Text,
    }
}

/// The array at a path of struct members.
pub(crate) fn nested(batch: &RecordBatch, path: &[usize]) -> ArrayRef {
    let mut a = batch.column(path[0]).clone();
    for &i in &path[1..] {
        a = a.as_struct().column(i).clone();
    }
    a
}

pub(crate) fn value(a: &dyn Array, row: usize, t: FieldType) -> FieldValue {
    if a.is_null(row) {
        return FieldValue::Null;
    }
    if t == FieldType::Blob {
        let bytes = match a.data_type() {
            DataType::Binary => a.as_binary::<i32>().value(row).to_vec(),
            DataType::LargeBinary => a.as_binary::<i64>().value(row).to_vec(),
            DataType::BinaryView => a.as_binary_view().value(row).to_vec(),
            DataType::FixedSizeBinary(_) => a.as_fixed_size_binary().value(row).to_vec(),
            _ => return FieldValue::Null,
        };
        return FieldValue::Blob(bytes);
    }
    let v = json_value(a, row);
    match (t, v) {
        (_, Value::Null) => FieldValue::Null,
        (FieldType::Integer, v) => v.as_i64().or_else(|| v.as_u64().map(|x| x as i64)).map_or(FieldValue::Null, FieldValue::Integer),
        (FieldType::Float, v) => v.as_f64().map_or(FieldValue::Null, FieldValue::Float),
        (FieldType::Boolean, v) => v.as_bool().map_or(FieldValue::Null, FieldValue::Boolean),
        (FieldType::Date, Value::String(s)) => FieldValue::Date(s),
        (FieldType::DateTime, Value::String(s)) => FieldValue::DateTime(s),
        (_, Value::String(s)) => FieldValue::Text(s),
        (_, v) => FieldValue::Text(v.to_string()),
    }
}

/// A value as JSON: numbers, strings (dates and times in ISO 8601), lists, structs and maps.
fn json_value(a: &dyn Array, row: usize) -> Value {
    use DataType as D;
    if a.is_null(row) {
        return Value::Null;
    }
    match a.data_type() {
        D::Boolean => json!(a.as_boolean().value(row)),
        D::Int8 => json!(a.as_primitive::<Int8Type>().value(row)),
        D::Int16 => json!(a.as_primitive::<Int16Type>().value(row)),
        D::Int32 => json!(a.as_primitive::<Int32Type>().value(row)),
        D::Int64 => json!(a.as_primitive::<Int64Type>().value(row)),
        D::UInt8 => json!(a.as_primitive::<UInt8Type>().value(row)),
        D::UInt16 => json!(a.as_primitive::<UInt16Type>().value(row)),
        D::UInt32 => json!(a.as_primitive::<UInt32Type>().value(row)),
        D::UInt64 => json!(a.as_primitive::<UInt64Type>().value(row)),
        D::Float16 => json!(f64::from(a.as_primitive::<Float16Type>().value(row))),
        D::Float32 => json!(a.as_primitive::<Float32Type>().value(row) as f64),
        D::Float64 => json!(a.as_primitive::<Float64Type>().value(row)),
        D::Decimal128(_, scale) => json!(a.as_primitive::<Decimal128Type>().value(row) as f64 / 10f64.powi(*scale as i32)),
        D::Decimal256(_, scale) => {
            let unscaled: f64 = a.as_primitive::<Decimal256Type>().value(row).to_string().parse().unwrap_or(f64::NAN);
            json!(unscaled / 10f64.powi(*scale as i32))
        }
        D::Utf8 => json!(a.as_string::<i32>().value(row)),
        D::LargeUtf8 => json!(a.as_string::<i64>().value(row)),
        D::Utf8View => json!(a.as_string_view().value(row)),
        D::Date32 => json!(format_date(a.as_primitive::<Date32Type>().value(row) as i64)),
        D::Date64 => json!(format_date(a.as_primitive::<Date64Type>().value(row).div_euclid(86_400_000))),
        D::Timestamp(unit, tz) => {
            let ms = match unit {
                TimeUnit::Second => a.as_primitive::<TimestampSecondType>().value(row) * 1000,
                TimeUnit::Millisecond => a.as_primitive::<TimestampMillisecondType>().value(row),
                TimeUnit::Microsecond => a.as_primitive::<TimestampMicrosecondType>().value(row).div_euclid(1000),
                TimeUnit::Nanosecond => a.as_primitive::<TimestampNanosecondType>().value(row).div_euclid(1_000_000),
            };
            json!(format_datetime(ms, tz.is_some()))
        }
        D::Binary | D::LargeBinary | D::BinaryView | D::FixedSizeBinary(_) => {
            let FieldValue::Blob(b) = value(a, row, FieldType::Blob) else { return Value::Null };
            json!(b.iter().map(|x| format!("{x:02X}")).collect::<String>())
        }
        D::List(_) => list_json(a.as_list::<i32>().value(row).as_ref()),
        D::LargeList(_) => list_json(a.as_list::<i64>().value(row).as_ref()),
        D::FixedSizeList(..) => list_json(a.as_fixed_size_list().value(row).as_ref()),
        D::Struct(fields) => {
            let s = a.as_struct();
            Value::Object(fields.iter().enumerate().map(|(i, f)| (f.name().clone(), json_value(s.column(i).as_ref(), row))).collect())
        }
        D::Map(..) => {
            let m = a.as_map();
            let entries = m.value(row);
            let (k, v) = (entries.column(0), entries.column(1));
            Value::Object(
                (0..entries.len())
                    .map(|i| {
                        let key = match json_value(k.as_ref(), i) {
                            Value::String(s) => s,
                            other => other.to_string(),
                        };
                        (key, json_value(v.as_ref(), i))
                    })
                    .collect(),
            )
        }
        D::Dictionary(..) => match a.as_any_dictionary_opt() {
            Some(d) => {
                let key = d.normalized_keys()[row];
                json_value(d.values().as_ref(), key)
            }
            None => Value::Null,
        },
        _ => Value::Null,
    }
}

fn list_json(a: &dyn Array) -> Value {
    Value::Array((0..a.len()).map(|i| json_value(a, i)).collect())
}

/// `YYYY-MM-DD` from days since 1970-01-01 (Howard Hinnant's civil_from_days).
pub(crate) fn format_date(days: i64) -> String {
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}")
}

/// An ISO 8601 date-time from milliseconds since the Unix epoch, marked UTC when `utc`.
pub(crate) fn format_datetime(ms: i64, utc: bool) -> String {
    let days = ms.div_euclid(86_400_000);
    let t = ms.rem_euclid(86_400_000);
    let (h, mi, s, frac) = (t / 3_600_000, t / 60_000 % 60, t / 1000 % 60, t % 1000);
    let mut out = format!("{}T{h:02}:{mi:02}:{s:02}", format_date(days));
    if frac > 0 {
        out.push_str(&format!(".{frac:03}"));
    }
    if utc {
        out.push('Z');
    }
    out
}

fn geometry(a: &dyn Array, row: usize, encoding: &str) -> Option<Geometry> {
    if a.is_null(row) {
        return None;
    }
    match encoding {
        "wkb" => {
            let FieldValue::Blob(b) = value(a, row, FieldType::Blob) else { return None };
            Geometry::from_wkb(&b).ok()
        }
        "point" => coords(a).into_iter().nth(row).map(Geometry::Point),
        "linestring" => Some(Geometry::LineString(coords(list_item(a, row)?.as_ref()))),
        "multipoint" => Some(Geometry::MultiPoint(coords(list_item(a, row)?.as_ref()))),
        "polygon" => {
            let (exterior, interiors) = polygon(list_item(a, row)?.as_ref())?;
            Some(Geometry::Polygon { exterior, interiors })
        }
        "multilinestring" => {
            let parts = list_item(a, row)?;
            Some(Geometry::MultiLineString((0..parts.len()).filter_map(|i| Some(coords(list_item(parts.as_ref(), i)?.as_ref()))).collect()))
        }
        "multipolygon" => {
            let parts = list_item(a, row)?;
            Some(Geometry::MultiPolygon((0..parts.len()).filter_map(|i| polygon(list_item(parts.as_ref(), i)?.as_ref())).collect()))
        }
        _ => None,
    }
}

fn list_item(a: &dyn Array, row: usize) -> Option<ArrayRef> {
    match a.data_type() {
        DataType::List(_) => Some(a.as_list::<i32>().value(row)),
        DataType::LargeList(_) => Some(a.as_list::<i64>().value(row)),
        DataType::FixedSizeList(..) => Some(a.as_fixed_size_list().value(row)),
        _ => None,
    }
}

fn polygon(rings: &dyn Array) -> Option<(Ring, Vec<Ring>)> {
    let mut rs = (0..rings.len()).filter_map(|i| list_item(rings, i)).map(|r| {
        let mut cs = coords(r.as_ref());
        if cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
            cs.pop();
        }
        Ring::new(cs)
    });
    let exterior = rs.next()?;
    Some((exterior, rs.collect()))
}

/// GeoArrow coordinates: a struct of `x`, `y` (and `z`) arrays, or interleaved fixed-size lists.
fn coords(a: &dyn Array) -> Vec<Coord> {
    let f64s = |a: &ArrayRef| -> Vec<f64> { (0..a.len()).map(|i| json_value(a.as_ref(), i).as_f64().unwrap_or(f64::NAN)).collect() };
    match a.data_type() {
        DataType::Struct(fields) => {
            let s = a.as_struct();
            let col = |n: &str| fields.iter().position(|f| f.name() == n).map(|i| f64s(s.column(i)));
            let (Some(x), Some(y)) = (col("x"), col("y")) else { return Vec::new() };
            let z = col("z");
            (0..x.len()).map(|i| Coord { x: x[i], y: y[i], z: z.as_ref().map(|z| z[i]), m: None }).collect()
        }
        DataType::FixedSizeList(_, n) => {
            let l = a.as_fixed_size_list();
            let n = *n as usize;
            let v = f64s(l.values());
            (0..l.len()).map(|i| Coord { x: v[i * n], y: v[i * n + 1], z: (n > 2).then(|| v[i * n + 2]), m: None }).collect()
        }
        _ => Vec::new(),
    }
}

/// The EPSG code of a PROJJSON definition: its `id`, or that of a bound CRS's source.
fn projjson_epsg(v: &Value) -> Option<u32> {
    let id = v.get("id").or_else(|| v.get("ids").and_then(|ids| ids.get(0)));
    if let Some(id) = id
        && id["authority"].as_str().is_some_and(|a| a.eq_ignore_ascii_case("EPSG"))
    {
        return id["code"].as_u64().map(|c| c as u32).or_else(|| id["code"].as_str()?.parse().ok());
    }
    if id.is_some_and(|id| id["authority"] == "OGC" && id["code"] == "CRS84") {
        return Some(4326);
    }
    if let Some(code) = v.get("source_crs").and_then(projjson_epsg) {
        return Some(code);
    }
    // A geographic CRS on WGS 84 without an identifier (as GDAL writes Esri definitions).
    let datum = v["datum"]["name"].as_str().or_else(|| v["datum_ensemble"]["name"].as_str()).unwrap_or_default();
    let wgs84 = datum == "World Geodetic System 1984" || datum.starts_with("World Geodetic System 1984 ") || datum == "WGS_1984";
    (v["type"] == "GeographicCRS" && wgs84).then_some(4326)
}

/// The layer type for the `geometry_types` metadata: one type, or a type and its multi-part form.
pub(crate) fn declared_type(types: &[&str]) -> Option<GeometryType> {
    let parse = |t: &str| match t {
        "Point" => Some(GeometryType::Point),
        "LineString" => Some(GeometryType::LineString),
        "Polygon" => Some(GeometryType::Polygon),
        "MultiPoint" => Some(GeometryType::MultiPoint),
        "MultiLineString" => Some(GeometryType::MultiLineString),
        "MultiPolygon" => Some(GeometryType::MultiPolygon),
        "GeometryCollection" => Some(GeometryType::GeometryCollection),
        _ => None,
    };
    let mut parsed: Vec<GeometryType> = types.iter().map(|t| parse(t)).collect::<Option<_>>()?;
    parsed.sort_by_key(|t| t.wkb_type());
    parsed.dedup();
    use GeometryType as T;
    match parsed.as_slice() {
        [t] => Some(*t),
        [T::Point, T::MultiPoint] => Some(T::MultiPoint),
        [T::LineString, T::MultiLineString] => Some(T::MultiLineString),
        [T::Polygon, T::MultiPolygon] => Some(T::MultiPolygon),
        _ => None,
    }
}

/// Writes a layer as GeoParquet.
pub fn write(layer: &Layer, path: &str) -> Result<()> {
    std::fs::write(path, to_bytes(layer)?)?;
    Ok(())
}

const BATCH: usize = 65536;

/// Encodes a layer as a GeoParquet file.
pub fn to_bytes(layer: &Layer) -> Result<Vec<u8>> {
    let err = |e: &dyn std::fmt::Display| Error::InvalidData(format!("Parquet: {e}"));
    let fields = layer.schema.fields();
    let kinds: Vec<Kind> = fields.iter().enumerate().map(|(i, f)| column_kind(layer, i, f.field_type)).collect();
    let bbox_fields: Fields = ["xmin", "ymin", "xmax", "ymax"].iter().map(|n| Field::new(*n, DataType::Float64, false)).collect();
    let mut schema_fields: Vec<Field> = fields.iter().zip(&kinds).map(|(f, k)| Field::new(&f.name, k.data_type(), true)).collect();
    schema_fields.push(Field::new("geometry", DataType::Binary, true));
    schema_fields.push(Field::new("geometry_bbox", DataType::Struct(bbox_fields.clone()), true));

    let geo = geo_metadata(layer);
    let schema = Arc::new(Schema::new(schema_fields));
    let props = WriterProperties::builder()
        .set_compression(parquet::basic::Compression::SNAPPY)
        // GDAL's default row group size: with the covering column's statistics, readers skip
        // the row groups outside a bounding-box filter.
        .set_max_row_group_row_count(Some(BATCH))
        .set_key_value_metadata(Some(vec![parquet::file::metadata::KeyValue::new("geo".into(), geo.to_string())]))
        .build();
    let mut buf = Vec::new();
    let mut writer = ArrowWriter::try_new(&mut buf, schema.clone(), Some(props)).map_err(|e| err(&e))?;
    for chunk in layer.features.chunks(BATCH) {
        let mut columns: Vec<ArrayRef> = kinds.iter().enumerate().map(|(i, k)| k.build(chunk, i)).collect();
        let mut wkb = BinaryBuilder::new();
        let mut bounds = [Float64Builder::new(), Float64Builder::new(), Float64Builder::new(), Float64Builder::new()];
        let mut valid = Vec::with_capacity(chunk.len());
        // Each geometry's bounds and WKB, computed in parallel.
        let encoded = crate::par::map_range(chunk.len(), |j| {
            let g = chunk[j].geometry.as_ref();
            let bb = g.and_then(|g| g.bbox());
            (bb.is_some().then(|| g.map(Geometry::to_wkb)).flatten(), bb)
        });
        for (geometry, bb) in encoded {
            match geometry {
                Some(w) => wkb.append_value(w),
                None => wkb.append_null(),
            }
            valid.push(bb.is_some());
            let b = bb.map_or([0.0; 4], |b| [b.min_x, b.min_y, b.max_x, b.max_y]);
            for (builder, v) in bounds.iter_mut().zip(b) {
                builder.append_value(v);
            }
        }
        columns.push(Arc::new(wkb.finish()));
        let bbox_arrays: Vec<ArrayRef> = bounds.iter_mut().map(|b| Arc::new(b.finish()) as ArrayRef).collect();
        let bbox = StructArray::try_new(bbox_fields.clone(), bbox_arrays, Some(valid.into())).map_err(|e| err(&e))?;
        columns.push(Arc::new(bbox));
        let batch = RecordBatch::try_new(schema.clone(), columns).map_err(|e| err(&e))?;
        writer.write(&batch).map_err(|e| err(&e))?;
    }
    writer.close().map_err(|e| err(&e))?;
    Ok(buf)
}

fn geo_metadata(layer: &Layer) -> Value {
    let mut types: Vec<String> = Vec::new();
    let mut bounds: Option<[f64; 4]> = None;
    // Each geometry's type and bounds, computed in parallel, then combined in order.
    let summaries = crate::par::map_range(layer.features.len(), |i| {
        let g = layer.features[i].geometry.as_ref()?;
        let name = format!("{}{}", g.geom_type().as_str(), if crate::vector::geometry_has_z(g) { " Z" } else { "" });
        Some((name, g.bbox()))
    });
    for (name, bbox) in summaries.into_iter().flatten() {
        if !types.contains(&name) {
            types.push(name);
        }
        if let Some(b) = bbox {
            bounds = Some(match bounds {
                None => [b.min_x, b.min_y, b.max_x, b.max_y],
                Some(a) => [a[0].min(b.min_x), a[1].min(b.min_y), a[2].max(b.max_x), a[3].max(b.max_y)],
            });
        }
    }
    let mut col = json!({
        "encoding": "WKB",
        "geometry_types": types,
        "covering": {"bbox": {
            "xmin": ["geometry_bbox", "xmin"], "ymin": ["geometry_bbox", "ymin"],
            "xmax": ["geometry_bbox", "xmax"], "ymax": ["geometry_bbox", "ymax"],
        }},
    });
    if let Some(b) = bounds {
        col["bbox"] = json!(b);
    }
    // A missing `crs` means OGC:CRS84 (longitude/latitude on WGS 84) and `null` an unknown CRS;
    // others are PROJJSON, as the specification asks (WKT, which GDAL and PROJ-based readers
    // also accept, for what cannot be converted).
    let srs = SpatialRef::from_layer(layer);
    if srs.is_unknown() {
        col["crs"] = Value::Null;
    } else if super::trusted_epsg(layer) != Some(4326) {
        let wkt = super::output_wkt(layer);
        col["crs"] = match wkt.as_deref().and_then(|w| super::projjson::from_wkt1(w, super::trusted_epsg(layer))) {
            Some(j) => j,
            None => wkt.map_or(Value::Null, Value::String),
        };
    }
    json!({"version": "1.1.0", "primary_column": "geometry", "columns": {"geometry": col}})
}

/// How a field is stored.
#[derive(Clone, Copy)]
enum Kind {
    Int32,
    Int64,
    Float64,
    Utf8,
    Bool,
    Binary,
    Date32,
    Timestamp,
}

fn column_kind(layer: &Layer, i: usize, t: FieldType) -> Kind {
    let values = || layer.features.iter().filter_map(move |f| f.attributes.get(i)).filter(|v| !v.is_null());
    match t {
        FieldType::Integer if values().all(|v| v.as_i64().is_some_and(|x| i32::try_from(x).is_ok())) => Kind::Int32,
        FieldType::Integer => Kind::Int64,
        FieldType::Float => Kind::Float64,
        FieldType::Boolean => Kind::Bool,
        FieldType::Blob => Kind::Binary,
        FieldType::Date if values().all(|v| v.as_str().and_then(parse_date).is_some()) => Kind::Date32,
        FieldType::DateTime if values().all(|v| v.as_str().and_then(parse_datetime).is_some()) => Kind::Timestamp,
        _ => Kind::Utf8,
    }
}

impl Kind {
    fn data_type(self) -> DataType {
        match self {
            Kind::Int32 => DataType::Int32,
            Kind::Int64 => DataType::Int64,
            Kind::Float64 => DataType::Float64,
            Kind::Utf8 => DataType::Utf8,
            Kind::Bool => DataType::Boolean,
            Kind::Binary => DataType::Binary,
            Kind::Date32 => DataType::Date32,
            Kind::Timestamp => DataType::Timestamp(TimeUnit::Millisecond, None),
        }
    }

    fn build(self, features: &[wbvector::Feature], i: usize) -> ArrayRef {
        let values = features.iter().map(|f| f.attributes.get(i).filter(|v| !v.is_null()));
        match self {
            Kind::Int32 => Arc::new(values.map(|v| v.and_then(FieldValue::as_i64).map(|x| x as i32)).collect::<Int32Array>()),
            Kind::Int64 => Arc::new(values.map(|v| v.and_then(FieldValue::as_i64)).collect::<Int64Array>()),
            Kind::Float64 => Arc::new(values.map(|v| v.and_then(FieldValue::as_f64)).collect::<Float64Array>()),
            Kind::Bool => Arc::new(values.map(|v| v.and_then(FieldValue::as_bool)).collect::<BooleanArray>()),
            Kind::Binary => {
                let mut b = BinaryBuilder::new();
                for v in values {
                    b.append_option(v.and_then(FieldValue::as_blob));
                }
                Arc::new(b.finish())
            }
            Kind::Date32 => Arc::new(values.map(|v| v.and_then(FieldValue::as_str).and_then(parse_date)).collect::<Date32Array>()),
            Kind::Timestamp => Arc::new(TimestampMillisecondArray::from(
                values.map(|v| v.and_then(FieldValue::as_str).and_then(parse_datetime)).collect::<Vec<_>>(),
            )),
            Kind::Utf8 => {
                let mut b = StringBuilder::new();
                for v in values {
                    match v {
                        None => b.append_null(),
                        Some(FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s)) => b.append_value(s),
                        Some(FieldValue::Integer(x)) => b.append_value(x.to_string()),
                        Some(FieldValue::Float(x)) => b.append_value(x.to_string()),
                        Some(FieldValue::Boolean(x)) => b.append_value(x.to_string()),
                        Some(_) => b.append_null(),
                    }
                }
                Arc::new(b.finish())
            }
        }
    }
}

/// Days since 1970-01-01 of a `YYYY-MM-DD` date.
pub(crate) fn parse_date(s: &str) -> Option<i32> {
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[..4].parse().ok()?;
    let m: i64 = s[5..7].parse().ok()?;
    let d: i64 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Howard Hinnant's days_from_civil.
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    i32::try_from(era * 146097 + doe - 719468).ok()
}

/// Milliseconds since the Unix epoch of an ISO 8601 date-time, in UTC when it has an offset.
pub(crate) fn parse_datetime(s: &str) -> Option<i64> {
    let days = parse_date(s)? as i64;
    let rest = &s[10..];
    if rest.is_empty() {
        return Some(days * 86_400_000);
    }
    let rest = rest.strip_prefix(['T', ' '])?;
    let (h, mi, sec) = (rest.get(..2)?.parse::<i64>().ok()?, rest.get(3..5)?.parse::<i64>().ok()?, rest.get(6..8)?.parse::<i64>().ok()?);
    let mut tail = &rest[8..];
    let mut ms = 0i64;
    if let Some(frac) = tail.strip_prefix('.') {
        let n = frac.bytes().take_while(u8::is_ascii_digit).count();
        let digits = format!("{:0<3}", &frac[..n.min(3)]);
        ms = digits.parse().ok()?;
        tail = &frac[n..];
    }
    let offset_min = match tail {
        "" | "Z" => 0,
        t if t.len() >= 5 && (t.starts_with('+') || t.starts_with('-')) => {
            let sign = if t.starts_with('-') { -1 } else { 1 };
            let t = t[1..].replace(':', "");
            sign * (t.get(..2)?.parse::<i64>().ok()? * 60 + t.get(2..4)?.parse::<i64>().ok()?)
        }
        _ => return None,
    };
    Some(((days * 86_400 + h * 3600 + mi * 60 + sec - offset_min * 60) * 1000) + ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_dates() {
        assert_eq!(parse_date("1970-01-01"), Some(0));
        assert_eq!(parse_date("2000-03-01"), Some(11017));
        assert_eq!(parse_datetime("1970-01-02T00:00:01.5Z"), Some(86_401_500));
        assert_eq!(parse_datetime("1970-01-01T01:00:00+01:00"), Some(0));
    }

    #[test]
    fn formats_dates() {
        assert_eq!(format_date(0), "1970-01-01");
        assert_eq!(format_date(11017), "2000-03-01");
        assert_eq!(format_date(-1), "1969-12-31");
        assert_eq!(format_datetime(86_401_500, true), "1970-01-02T00:00:01.500Z");
    }

    #[test]
    fn identifies_projjson() {
        assert_eq!(projjson_epsg(&json!({"type": "GeographicCRS", "id": {"authority": "EPSG", "code": 4269}})), Some(4269));
        assert_eq!(
            projjson_epsg(&json!({"type": "GeographicCRS", "name": "WGS 84", "datum": {"name": "World Geodetic System 1984"}})),
            Some(4326)
        );
        assert_eq!(projjson_epsg(&json!({"type": "ProjectedCRS", "name": "x"})), None);
    }

    #[test]
    fn declared_types() {
        assert_eq!(declared_type(&["Polygon", "MultiPolygon"]), Some(GeometryType::MultiPolygon));
        assert_eq!(declared_type(&["Point"]), Some(GeometryType::Point));
        assert_eq!(declared_type(&["Point", "Polygon"]), None);
        assert_eq!(declared_type(&[]), None);
    }

    #[test]
    fn writes_geo_metadata() {
        let mut l = Layer::new("t").with_epsg(32633);
        l.add_field(FieldDef::new("n", FieldType::Integer));
        l.add_field(FieldDef::new("d", FieldType::Date));
        l.push(Feature {
            fid: 0,
            geometry: Some(Geometry::point(500000.0, 0.0)),
            attributes: vec![FieldValue::Integer(1), FieldValue::Date("2020-01-02".into())],
        });
        l.push(Feature { fid: 1, geometry: None, attributes: vec![FieldValue::Null, FieldValue::Null] });
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        write(&l, path).unwrap();
        let back = read_filtered(path, None).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back.crs_epsg(), Some(32633));
        assert_eq!(back.geom_type, Some(GeometryType::Point));
        assert_eq!(back.schema.fields().iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["n", "d"]);
        assert_eq!(back.features[0].attributes, [FieldValue::Integer(1), FieldValue::Date("2020-01-02".into())]);
        assert_eq!(back.features[0].geometry, Some(Geometry::point(500000.0, 0.0)));
        assert_eq!(back.features[1].geometry, None);
    }
}

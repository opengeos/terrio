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

use super::datetime::{format_date, format_datetime, parse_date, parse_datetime};
use super::profile::LayerProfile;
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
use parquet::arrow::arrow_writer::{ArrowLeafColumn, ArrowRowGroupWriterFactory, compute_leaves};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
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
    parse_source(|| Ok(std::fs::File::open(path)?), name, None, bbox, None)
}

/// Parses a GeoParquet (or plain Parquet) file held in memory. `name` names the layer.
pub fn parse(bytes: Vec<u8>, name: &str) -> Result<Layer> {
    let bytes = bytes::Bytes::from(bytes);
    parse_source(|| Ok(bytes.clone()), name, None, None, None)
}

/// A row group of a GeoParquet file.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RowGroupInfo {
    /// The number of rows.
    pub rows: u64,
    /// The row number (feature id) of its first row.
    pub first_row: u64,
    /// The bounding box `[min_x, min_y, max_x, max_y]` of its geometries, from the statistics of
    /// the bounding-box covering column, when the file has them.
    pub bbox: Option<[f64; 4]>,
    /// The size in bytes of its column chunks.
    pub bytes: u64,
}

/// The row groups of a GeoParquet file, from its footer.
pub(crate) fn row_groups<R: parquet::file::reader::ChunkReader + 'static>(input: R) -> Result<Vec<RowGroupInfo>> {
    use parquet::file::statistics::Statistics as S;
    let err = |e: &dyn std::fmt::Display| Error::InvalidData(format!("Parquet: {e}"));
    let builder = ParquetRecordBatchReaderBuilder::try_new(input).map_err(|e| err(&e))?;
    let meta = builder.metadata().clone();
    let descr = meta.file_metadata().schema_descr();
    let geo: Value = meta
        .file_metadata()
        .key_value_metadata()
        .and_then(|kv| kv.iter().find(|k| k.key == "geo"))
        .and_then(|k| k.value.as_deref())
        .and_then(|v| serde_json::from_str(v).ok())
        .unwrap_or(Value::Null);
    let primary = geo["primary_column"].as_str().unwrap_or("geometry");
    let cov = &geo["columns"][primary]["covering"]["bbox"];
    let leaf = |k: &str| -> Option<usize> {
        let p: Vec<&str> = cov.get(k)?.as_array()?.iter().filter_map(Value::as_str).collect();
        (0..descr.num_columns()).find(|&i| descr.column(i).path().parts() == p.as_slice())
    };
    let leaves = (|| Some([leaf("xmin")?, leaf("ymin")?, leaf("xmax")?, leaf("ymax")?]))();
    let mut out = Vec::new();
    let mut first_row = 0u64;
    for rg in meta.row_groups() {
        let stat = |leaf: usize, max: bool| -> Option<f64> {
            match rg.column(leaf).statistics()? {
                S::Double(s) => if max { s.max_opt() } else { s.min_opt() }.copied(),
                S::Float(s) => if max { s.max_opt() } else { s.min_opt() }.map(|v| *v as f64),
                _ => None,
            }
        };
        let bbox = leaves.and_then(|l| Some([stat(l[0], false)?, stat(l[1], false)?, stat(l[2], true)?, stat(l[3], true)?]));
        let bytes = rg.columns().iter().map(|c| c.byte_range().1).sum();
        out.push(RowGroupInfo { rows: rg.num_rows() as u64, first_row, bbox, bytes });
        first_row += rg.num_rows() as u64;
    }
    Ok(out)
}

/// Reads a file whose bytes the caller fetches: the rows of some row groups whose bounding box
/// intersects `bbox`, fetching only the column chunks needed.
pub(crate) fn read_sparse(
    file: &crate::sparse::SparseFile,
    name: &str,
    row_groups: Option<&[usize]>,
    bbox: Option<(f64, f64, f64, f64)>,
) -> Result<Layer> {
    let prefetch = |ranges: &[(u64, u64)]| -> Result<()> { Ok(file.prefetch(ranges)?) };
    parse_source(|| Ok(file.clone()), name, row_groups, bbox, Some(&prefetch))
}

/// Announces the byte ranges about to be read, to readers whose bytes the caller fetches.
type Prefetch<'a> = Option<crate::sparse::Prefetch<'a>>;

/// Parses a file whose bytes `open` gives (once, or twice with a filter): the rows of
/// `row_groups` (all by default) that intersect `bbox`.
fn parse_source<R: parquet::file::reader::ChunkReader + 'static>(
    open: impl Fn() -> Result<R>,
    name: &str,
    row_groups: Option<&[usize]>,
    bbox: Option<(f64, f64, f64, f64)>,
    prefetch: Prefetch<'_>,
) -> Result<Layer> {
    let mut s = ParquetStream::open(open, name, row_groups, bbox, 65536, prefetch)?;
    let mut layer = s.layer.clone();
    while let Some(batch) = s.next_batch()? {
        layer.features.extend(batch);
    }
    Ok(layer)
}

/// The features of a GeoParquet file, read a record batch at a time.
pub(crate) struct ParquetStream {
    /// The layer, without its features.
    pub layer: Layer,
    reader: parquet::arrow::arrow_reader::ParquetRecordBatchReader,
    columns: Vec<Column>,
    geom_index: Option<usize>,
    encoding: String,
    fids: Option<Vec<u64>>,
    fid: u64,
}

impl ParquetStream {
    /// Opens a file whose bytes `open` gives (once, or twice with a filter), to read the rows of
    /// `row_groups` (all by default) that intersect `bbox` in batches of `batch_size` rows.
    /// `prefetch`, when given, is told the byte ranges about to be read.
    pub(crate) fn open<R: parquet::file::reader::ChunkReader + 'static>(
        open: impl Fn() -> Result<R>,
        name: &str,
        row_groups: Option<&[usize]>,
        bbox: Option<(f64, f64, f64, f64)>,
        batch_size: usize,
        prefetch: Prefetch<'_>,
    ) -> Result<Self> {
        let err = |e: &dyn std::fmt::Display| Error::InvalidData(format!("Parquet: {e}"));
        let builder = ParquetRecordBatchReaderBuilder::try_new(open()?).map_err(|e| err(&e))?;
        let num_groups = builder.metadata().num_row_groups();
        if let Some(g) = row_groups.and_then(|g| g.iter().find(|&&g| g >= num_groups)) {
            return Err(Error::InvalidArgument(format!("no row group {g}: the file has {num_groups}")));
        }
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
        let meta = builder.metadata().clone();
        let mut groups: Vec<usize> = match row_groups {
            Some(g) => {
                let mut g = g.to_vec();
                g.sort_unstable();
                g.dedup();
                g
            }
            None => (0..num_groups).collect(),
        };
        if let Some(rect) = bbox {
            let (kept_groups, selection, kept) = select_rows(&builder, open()?, covering, geom_index, &encoding, rect, &groups, prefetch)?;
            groups = kept_groups;
            builder = builder.with_row_groups(groups.clone()).with_row_selection(selection);
            fids = Some(kept);
        } else if row_groups.is_some() {
            // Feature ids stay the row numbers in the file.
            let mut first = Vec::with_capacity(num_groups);
            let mut n = 0u64;
            for rg in meta.row_groups() {
                first.push(n);
                n += rg.num_rows() as u64;
            }
            fids = Some(groups.iter().flat_map(|&g| first[g]..first[g] + meta.row_group(g).num_rows() as u64).collect());
            builder = builder.with_row_groups(groups.clone());
        }
        if let Some(p) = prefetch {
            let ranges: Vec<(u64, u64)> = groups.iter().flat_map(|&g| meta.row_group(g).columns().iter().map(|c| c.byte_range())).collect();
            p(&ranges)?;
        }
        let reader = builder.with_batch_size(batch_size.max(1)).build().map_err(|e| err(&e))?;

        match col.get("crs") {
            _ if geo.is_null() => {}
            None => layer.set_crs_epsg(Some(4326)),
            Some(crs) => apply_crs(&mut layer, crs),
        }
        if let Some(types) = col["geometry_types"].as_array() {
            let types: Vec<&str> =
                types.iter().filter_map(Value::as_str).map(|t| t.trim_end_matches(" Z").trim_end_matches(" M")).collect();
            layer.geom_type = declared_type(&types);
        }
        Ok(Self { layer, reader, columns, geom_index, encoding, fids, fid: 0 })
    }

    /// The features of the next record batch, decoded in parallel; `None` at the end.
    pub(crate) fn next_batch(&mut self) -> Result<Option<Vec<Feature>>> {
        let Some(batch) = self.reader.next() else { return Ok(None) };
        let batch = batch.map_err(|e| Error::InvalidData(format!("Parquet: {e}")))?;
        let geoms = self.geom_index.map(|i| batch.column(i).clone());
        let arrays: Vec<ArrayRef> = self.columns.iter().map(|c| nested(&batch, &c.path)).collect();
        let (columns, encoding, fids, first) = (&self.columns, self.encoding.as_str(), &self.fids, self.fid);
        let features = crate::par::map_range(batch.num_rows(), |row| {
            let geometry = geoms.as_ref().and_then(|g| geometry(g.as_ref(), row, encoding));
            let attributes = columns.iter().zip(&arrays).map(|(c, a)| value(a.as_ref(), row, c.field_type)).collect();
            let n = first + row as u64;
            let id = fids.as_ref().and_then(|f| f.get(n as usize).copied()).unwrap_or(n);
            Feature { fid: id, geometry, attributes }
        });
        self.fid += batch.num_rows() as u64;
        Ok(Some(features))
    }
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
#[allow(clippy::too_many_arguments)]
fn select_rows<T: parquet::file::reader::ChunkReader + 'static>(
    builder: &ParquetRecordBatchReaderBuilder<T>,
    input: T,
    covering: Option<[(usize, usize); 4]>,
    geom_index: Option<usize>,
    encoding: &str,
    rect: (f64, f64, f64, f64),
    allowed: &[usize],
    prefetch: Prefetch<'_>,
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
        if !outside && allowed.binary_search(&rg).is_ok() {
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
    if let Some(p) = prefetch {
        let leaves: Vec<usize> = (0..descr.num_columns()).filter(|&i| mask.leaf_included(i)).collect();
        let ranges: Vec<(u64, u64)> = groups
            .iter()
            .flat_map(|&g| leaves.iter().map(|&l| (g, l)).collect::<Vec<_>>())
            .map(|(g, l)| meta.row_group(g).column(l).byte_range())
            .collect();
        p(&ranges)?;
    }
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
    // Run-length selectors over the row groups with rows kept (the others are not read), and
    // the file row numbers of the kept rows.
    let mut selectors: Vec<RowSelector> = Vec::new();
    let mut kept = Vec::new();
    let mut kept_groups = Vec::new();
    let mut i = 0usize;
    for (g, &rg) in groups.iter().enumerate() {
        let n = meta.row_group(rg).num_rows() as usize;
        if !keep[i.min(keep.len())..(i + n).min(keep.len())].contains(&true) {
            i += n;
            continue;
        }
        kept_groups.push(rg);
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
    Ok((kept_groups, RowSelection::from(selectors), kept))
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
    let mut def = FieldDef::new(name, field_type);
    // 64-bit integer columns are GDAL's Integer64 fields: they declare the width that keeps them
    // 64-bit when written.
    if matches!(t, DataType::Int64 | DataType::UInt32 | DataType::UInt64) {
        def.width = crate::ops::vector_schema::INTEGER64_WIDTH;
    }
    layer.add_field(def);
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
    let profile = if ParquetEncoder::needs_profile(layer) { LayerProfile::of(layer) } else { LayerProfile::new(layer) };
    let enc = ParquetEncoder::new(layer, &profile);
    let mut buf = Vec::new();
    let mut writer = enc.writer(&mut buf)?;
    for chunk in layer.features.chunks(BATCH) {
        enc.write(&mut writer, chunk)?;
    }
    writer.close()?;
    Ok(buf)
}

fn perr(e: &dyn std::fmt::Display) -> Error {
    Error::InvalidData(format!("Parquet: {e}"))
}

/// Encodes the features of a layer as record batches of a GeoParquet file, with the column
/// types its profile decides. The geometry types and extent of the `geo` metadata are gathered
/// from the batches and written in the footer.
pub(crate) struct ParquetEncoder {
    kinds: Vec<Kind>,
    bbox_fields: Fields,
    schema: Arc<Schema>,
    crs: Option<Value>,
}

/// A record batch, and what its geometries hold for the `geo` metadata.
pub(crate) type EncodedBatch = (RecordBatch, GeoStats);

/// The geometry types and extent of the features written, for the `geo` metadata.
#[derive(Debug, Clone, Default)]
pub(crate) struct GeoStats {
    /// The geometry type names, in the order they first appear.
    type_names: Vec<String>,
    /// The extent `[min_x, min_y, max_x, max_y]`.
    bbox: Option<[f64; 4]>,
}

impl GeoStats {
    /// Adds the stats of the features that follow.
    fn merge(&mut self, o: &GeoStats) {
        for n in &o.type_names {
            if !self.type_names.contains(n) {
                self.type_names.push(n.clone());
            }
        }
        if let Some(b) = o.bbox {
            self.add_bbox(b);
        }
    }

    /// Extends the extent to a geometry's bounds.
    fn add_bbox(&mut self, b: [f64; 4]) {
        self.bbox = Some(match self.bbox {
            None => b,
            Some(a) => [a[0].min(b[0]), a[1].min(b[1]), a[2].max(b[2]), a[3].max(b[3])],
        });
    }
}

impl ParquetEncoder {
    /// Whether the column types depend on the values of the features of `layer`, so that
    /// [`ParquetEncoder::new`] needs a profile of them: layers with integer, date or date-time
    /// fields. Otherwise an empty profile gives the same columns.
    pub(crate) fn needs_profile(layer: &Layer) -> bool {
        layer.schema.fields().iter().any(|f| matches!(f.field_type, FieldType::Integer | FieldType::Date | FieldType::DateTime))
    }

    /// An encoder for the features of `layer` (its fields and CRS), whose field values `profile`
    /// describes (when [`ParquetEncoder::needs_profile`]; its geometries are not used).
    pub(crate) fn new(layer: &Layer, profile: &LayerProfile) -> Self {
        let fields = layer.schema.fields();
        let kinds: Vec<Kind> = fields.iter().enumerate().map(|(i, f)| column_kind(profile, i, f)).collect();
        let bbox_fields: Fields = ["xmin", "ymin", "xmax", "ymax"].iter().map(|n| Field::new(*n, DataType::Float64, false)).collect();
        let mut schema_fields: Vec<Field> = fields.iter().zip(&kinds).map(|(f, k)| Field::new(&f.name, k.data_type(), true)).collect();
        schema_fields.push(Field::new("geometry", DataType::Binary, true));
        schema_fields.push(Field::new("geometry_bbox", DataType::Struct(bbox_fields.clone()), true));
        Self { kinds, bbox_fields, schema: Arc::new(Schema::new(schema_fields)), crs: geo_crs(layer) }
    }

    /// A Parquet writer of the file into `out`.
    pub(crate) fn writer<W: std::io::Write + Send>(&self, out: W) -> Result<ParquetFileWriter<W>> {
        let props = WriterProperties::builder()
            .set_compression(parquet::basic::Compression::SNAPPY)
            // GDAL's default row group size: with the covering column's statistics, readers skip
            // the row groups outside a bounding-box filter.
            .set_max_row_group_row_count(Some(BATCH))
            .build();
        // ArrowWriter sets up the file (its `ARROW:schema` metadata included); its row groups
        // are encoded here, their columns in parallel.
        let arrow = ArrowWriter::try_new(out, self.schema.clone(), Some(props)).map_err(|e| perr(&e))?;
        let (writer, factory) = arrow.into_serialized_writer().map_err(|e| perr(&e))?;
        Ok(ParquetFileWriter {
            writer,
            factory,
            schema: self.schema.clone(),
            pending: Vec::new(),
            rows: 0,
            groups: 0,
            crs: self.crs.clone(),
            geo: GeoStats::default(),
        })
    }

    /// Writes features, in batches of at most the row group size.
    pub(crate) fn write<W: std::io::Write + Send>(&self, writer: &mut ParquetFileWriter<W>, features: &[Feature]) -> Result<()> {
        for b in self.batches(features)? {
            writer.push(b)?;
        }
        Ok(())
    }

    /// The record batches of features, of at most the row group size.
    pub(crate) fn batches(&self, features: &[Feature]) -> Result<Vec<EncodedBatch>> {
        features.chunks(BATCH).map(|chunk| self.batch(chunk)).collect()
    }

    fn batch(&self, chunk: &[Feature]) -> Result<EncodedBatch> {
        // The columns, built in parallel.
        let mut columns: Vec<ArrayRef> = crate::par::map_range(self.kinds.len(), |i| self.kinds[i].build(chunk, i));
        let mut wkb = BinaryBuilder::new();
        let mut bounds = [Float64Builder::new(), Float64Builder::new(), Float64Builder::new(), Float64Builder::new()];
        let mut valid = Vec::with_capacity(chunk.len());
        let mut geo = GeoStats::default();
        // Each geometry's bounds, WKB, type and Z, computed in parallel.
        let encoded = crate::par::map_range(chunk.len(), |j| {
            let g = chunk[j].geometry.as_ref();
            let bb = g.and_then(|g| g.bbox());
            let t = g.map(|g| (g.geom_type(), crate::vector::geometry_has_z(g)));
            (bb.is_some().then(|| g.map(Geometry::to_wkb)).flatten(), bb, t)
        });
        for (geometry, bb, t) in encoded {
            if let Some((t, z)) = t {
                super::profile::add_type_name(&mut geo.type_names, t, z);
            }
            if let Some(b) = &bb {
                geo.add_bbox([b.min_x, b.min_y, b.max_x, b.max_y]);
            }
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
        let bbox = StructArray::try_new(self.bbox_fields.clone(), bbox_arrays, Some(valid.into())).map_err(|e| perr(&e))?;
        columns.push(Arc::new(bbox));
        Ok((RecordBatch::try_new(self.schema.clone(), columns).map_err(|e| perr(&e))?, geo))
    }
}

/// A Parquet file being written in row groups of [`BATCH`] rows, as `ArrowWriter` writes them
/// (each batch written to the column writers in turn), the columns of a row group encoded in
/// parallel.
pub(crate) struct ParquetFileWriter<W: std::io::Write + Send> {
    writer: SerializedFileWriter<W>,
    factory: ArrowRowGroupWriterFactory,
    schema: Arc<Schema>,
    pending: Vec<RecordBatch>,
    rows: usize,
    groups: usize,
    /// The `crs` of the `geo` metadata (see `geo_metadata`).
    crs: Option<Value>,
    geo: GeoStats,
}

impl<W: std::io::Write + Send> ParquetFileWriter<W> {
    /// Adds a batch, writing the row groups it completes.
    pub(crate) fn push(&mut self, (mut batch, geo): EncodedBatch) -> Result<()> {
        self.geo.merge(&geo);
        while batch.num_rows() > 0 {
            let take = batch.num_rows().min(BATCH - self.rows);
            self.pending.push(batch.slice(0, take));
            self.rows += take;
            batch = batch.slice(take, batch.num_rows() - take);
            if self.rows == BATCH {
                self.flush_group()?;
            }
        }
        Ok(())
    }

    /// Writes the pending rows as a row group.
    fn flush_group(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        // The leaf columns of each batch, by column.
        let mut by_column: Vec<Vec<ArrowLeafColumn>> = Vec::new();
        for batch in std::mem::take(&mut self.pending) {
            let mut k = 0;
            for (array, field) in batch.columns().iter().zip(self.schema.fields()) {
                for leaf in compute_leaves(field, array).map_err(|e| perr(&e))? {
                    if by_column.len() == k {
                        by_column.push(Vec::new());
                    }
                    by_column[k].push(leaf);
                    k += 1;
                }
            }
        }
        let writers = self.factory.create_column_writers(self.groups).map_err(|e| perr(&e))?;
        let work: Vec<std::sync::Mutex<Option<_>>> = writers.into_iter().zip(by_column).map(|w| std::sync::Mutex::new(Some(w))).collect();
        let chunks = crate::par::map_range(work.len(), |i| {
            let Some((mut writer, leaves)) = work[i].lock().ok().and_then(|mut w| w.take()) else {
                return Err(perr(&"column writer"));
            };
            for leaf in &leaves {
                writer.write(leaf).map_err(|e| perr(&e))?;
            }
            writer.close().map_err(|e| perr(&e))
        });
        let mut group = self.writer.next_row_group().map_err(|e| perr(&e))?;
        for chunk in chunks {
            chunk?.append_to_row_group(&mut group).map_err(|e| perr(&e))?;
        }
        group.close().map_err(|e| perr(&e))?;
        self.groups += 1;
        self.rows = 0;
        Ok(())
    }

    /// Writes the last row group and the footer, with the `geo` metadata, and returns the output.
    pub(crate) fn close(mut self) -> Result<W> {
        self.flush_group()?;
        let geo = geo_metadata(self.crs.take(), &self.geo);
        self.writer.append_key_value_metadata(parquet::file::metadata::KeyValue::new("geo".into(), geo.to_string()));
        self.writer.into_inner().map_err(|e| perr(&e))
    }
}

/// The `geo` metadata of a file whose geometries `geo` describes, with `crs` (from `geo_crs`).
fn geo_metadata(crs: Option<Value>, geo: &GeoStats) -> Value {
    let types = &geo.type_names;
    let bounds = geo.bbox;
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
    if let Some(crs) = crs {
        col["crs"] = crs;
    }
    json!({"version": "1.1.0", "primary_column": "geometry", "columns": {"geometry": col}})
}

/// The `crs` of the `geo` metadata for a layer: `None` to leave it out.
fn geo_crs(layer: &Layer) -> Option<Value> {
    // A missing `crs` means OGC:CRS84 (longitude/latitude on WGS 84) and `null` an unknown CRS;
    // others are PROJJSON, as the specification asks (WKT, which GDAL and PROJ-based readers
    // also accept, for what cannot be converted).
    let srs = SpatialRef::from_layer(layer);
    if srs.is_unknown() {
        Some(Value::Null)
    } else if super::trusted_epsg(layer) != Some(4326) {
        let wkt = super::output_wkt(layer);
        Some(match wkt.as_deref().and_then(|w| super::projjson::from_wkt1(w, super::trusted_epsg(layer))) {
            Some(j) => j,
            None => wkt.map_or(Value::Null, Value::String),
        })
    } else {
        None
    }
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

fn column_kind(profile: &LayerProfile, i: usize, f: &FieldDef) -> Kind {
    let p = profile.fields.get(i).cloned().unwrap_or_default();
    match f.field_type {
        // Integer fields at least 10 wide are 64-bit (GDAL's Integer64), whatever their values.
        FieldType::Integer if p.all_int32 && f.width < 10 => Kind::Int32,
        FieldType::Integer => Kind::Int64,
        FieldType::Float => Kind::Float64,
        FieldType::Boolean => Kind::Bool,
        FieldType::Blob => Kind::Binary,
        FieldType::Date if p.all_dates => Kind::Date32,
        FieldType::DateTime if p.all_datetimes => Kind::Timestamp,
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

#[cfg(test)]
mod tests {
    use super::*;

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

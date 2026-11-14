//! GeoParquet read as the Arrow record batches the file holds, without converting them to
//! features: the attribute columns keep their Arrow types (structs, lists and maps included), and
//! the geometry columns keep their encoding (WKB or GeoArrow native), tagged as GeoArrow extension
//! types with their CRS, as GDAL's Arrow stream interface hands them out (GDAL 3.13.3's Parquet
//! driver).
//!
//! The row groups are decoded in parallel, a window of as many as there are threads at a time,
//! and their batches come out in the file's order: memory holds a window's batches, not the file.

use super::geoparquet::{covering_leaves, file_geo, group_bboxes, perr};
use super::geoparquet_zstd::{self, SharedInput};
use crate::{Error, Result};
use arrow_array::cast::AsArray;
use arrow_array::types::{Float32Type, Float64Type};
use arrow_array::{Array, ArrayRef, BooleanArray, RecordBatch, RecordBatchReader};
use arrow_schema::{ArrowError, DataType, Field, Schema, SchemaRef};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ArrowReaderMetadata, ParquetRecordBatchReaderBuilder, RowSelection};
use parquet::file::reader::ChunkReader;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

/// What [`crate::arrow::open_geoparquet`] reads.
#[derive(Debug, Clone)]
pub struct GeoParquetReadOptions {
    /// The attribute columns to read, by name (the geometry columns are always read). By default
    /// all of them, without the bounding-box covering column.
    pub columns: Option<Vec<String>>,
    /// Only the rows whose bounding box intersects `(min_x, min_y, max_x, max_y)`, in the file's
    /// CRS: whole row groups are skipped by the statistics of the covering column, and rows by its
    /// values (or by their geometries' bounding boxes without one).
    pub bbox: Option<(f64, f64, f64, f64)>,
    /// The most rows in a batch.
    pub batch_size: usize,
}

impl Default for GeoParquetReadOptions {
    fn default() -> Self {
        Self { columns: None, bbox: None, batch_size: 65536 }
    }
}

/// Decodes the rows of a row group (all of them, or a selection) into record batches.
pub(crate) type DecodeGroup = Box<dyn Fn(usize, Option<RowSelection>) -> Result<Vec<RecordBatch>> + Send + Sync>;

/// Announces the byte ranges `(offset, length)` about to be read, to a reader that fetches them
/// (a remote file's block cache), with the most bytes worth announcing at once.
pub(crate) type Prefetcher = (Box<dyn Fn(&[(u64, u64)]) -> Result<()> + Send + Sync>, u64);

/// The record batches of a GeoParquet file, read as they are asked for: see the
/// documentation of [`crate::arrow::open_geoparquet`]. An [`arrow_array::RecordBatchReader`].
pub struct GeoParquetBatches {
    schema: SchemaRef,
    windows: GroupWindows,
    failed: bool,
}

impl std::fmt::Debug for GeoParquetBatches {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeoParquetBatches").field("schema", &self.schema).field("row_groups_left", &self.windows.groups.len()).finish()
    }
}

/// The most decoded (uncompressed) bytes of the row groups of a window, but for its first: memory
/// holds a window, however many threads decode it.
const DECODED_WINDOW_BYTES: u64 = 512 << 20;

/// The row groups of a file decoded in parallel, a window at a time, and their batches handed out
/// in the file's order: as many row groups at once as there are threads, within the bytes the
/// prefetcher takes at once. Memory holds a window's batches, not the file.
pub(crate) struct GroupWindows {
    /// The row groups left, with the rows to read of each (all by default).
    groups: VecDeque<(usize, Option<RowSelection>)>,
    ready: VecDeque<RecordBatch>,
    decode: DecodeGroup,
    /// The byte ranges of the column chunks read in each row group of the file.
    ranges: Vec<Vec<(u64, u64)>>,
    /// The bytes the columns read take decoded (uncompressed), in each row group of the file.
    decoded: Vec<u64>,
    prefetch: Option<Prefetcher>,
    /// The caller's options and STAC asset policy, for the threads that read.
    context: crate::stac::AssetReadContext,
    window: usize,
    /// Set once a window failed: its row groups are gone, so no later window is read.
    failed: bool,
}

impl GroupWindows {
    /// Decodes `groups` with `decode`; `ranges` are the byte ranges each row group of the file
    /// reads, announced to `prefetch` a window at a time. The caller's options and asset policy
    /// are those of the thread that calls this.
    pub(crate) fn new(
        groups: impl IntoIterator<Item = (usize, Option<RowSelection>)>,
        decode: DecodeGroup,
        meta: &parquet::file::metadata::ParquetMetaData,
        mask: &ProjectionMask,
        prefetch: Option<Prefetcher>,
    ) -> Self {
        let mut windows = Self {
            groups: groups.into_iter().collect(),
            ready: VecDeque::new(),
            decode,
            ranges: Vec::new(),
            decoded: Vec::new(),
            prefetch,
            context: crate::stac::AssetReadContext::capture(crate::stac::AssetReadPolicy::current()),
            window: crate::par::threads().max(1),
            failed: false,
        };
        windows.set_mask(meta, mask);
        windows
    }

    /// Takes the row groups to read the columns of `mask` from now on, as their decoder does:
    /// the ranges announced and the decoded bytes of a window. `false`, changing nothing, when
    /// batches decoded with the earlier columns are held.
    pub(crate) fn set_mask(&mut self, meta: &parquet::file::metadata::ParquetMetaData, mask: &ProjectionMask) -> bool {
        if !self.ready.is_empty() {
            return false;
        }
        let read =
            |rg: &parquet::file::metadata::RowGroupMetaData| (0..rg.num_columns()).filter(|&i| mask.leaf_included(i)).collect::<Vec<_>>();
        self.ranges = meta.row_groups().iter().map(|rg| read(rg).into_iter().map(|i| rg.column(i).byte_range()).collect()).collect();
        self.decoded =
            meta.row_groups().iter().map(|rg| read(rg).into_iter().map(|i| rg.column(i).uncompressed_size().max(0) as u64).sum()).collect();
        true
    }

    /// Announces the column chunks of each window to `prefetch` from now on.
    pub(crate) fn set_prefetcher(&mut self, prefetch: Prefetcher) {
        self.prefetch = Some(prefetch);
    }

    /// The byte ranges of the column chunks read in the row groups left.
    pub(crate) fn ranges_left(&self) -> Vec<(u64, u64)> {
        self.groups.iter().flat_map(|(g, _)| self.ranges[*g].iter().copied()).collect()
    }

    /// The next batch, decoding the next window of row groups when the batches held are out. After
    /// an error, every call fails: the rest of the file would be read without the failed window.
    pub(crate) fn next(&mut self) -> Result<Option<RecordBatch>> {
        if self.failed {
            return Err(perr(&"an earlier read of this file failed"));
        }
        let next = self.next_window();
        self.failed = next.is_err();
        next
    }

    fn next_window(&mut self) -> Result<Option<RecordBatch>> {
        while self.ready.is_empty() && !self.groups.is_empty() {
            let bytes = |g: usize| self.ranges[g].iter().map(|r| r.1).sum::<u64>();
            let budget = self.prefetch.as_ref().map_or(u64::MAX, |p| p.1);
            // within the bytes the prefetcher takes at once and the decoded bytes a window holds
            let (mut n, mut total, mut decoded) = (0, 0u64, 0u64);
            while n < self.window.min(self.groups.len())
                && (n == 0 || total + bytes(self.groups[n].0) <= budget && decoded + self.decoded[self.groups[n].0] <= DECODED_WINDOW_BYTES)
            {
                total += bytes(self.groups[n].0);
                decoded += self.decoded[self.groups[n].0];
                n += 1;
            }
            let window: Vec<(usize, Option<RowSelection>)> = self.groups.drain(..n).collect();
            if let Some((prefetch, _)) = &self.prefetch {
                let mut ranges: Vec<(u64, u64)> = window.iter().flat_map(|(g, _)| self.ranges[*g].iter().copied()).collect();
                ranges.sort_unstable();
                let _scope = self.context.enter()?;
                prefetch(&ranges)?;
            }
            let (decode, context) = (&self.decode, &self.context);
            for batches in crate::par::map_range_coarse(n, |i| {
                let _scope = context.enter()?;
                decode(window[i].0, window[i].1.clone())
            }) {
                self.ready.extend(batches?);
            }
        }
        Ok(self.ready.pop_front())
    }
}

impl GeoParquetBatches {
    /// Opens the file `input` holds; `prefetch` is told the column chunks of each window of row
    /// groups before they are decoded.
    pub(crate) fn open<R: ChunkReader + 'static>(input: R, options: &GeoParquetReadOptions, prefetch: Option<Prefetcher>) -> Result<Self> {
        let input = SharedInput(Arc::new(input));
        // With the page index, when the file has one: rows a filter leaves out skip whole pages.
        let reader_options = parquet::arrow::arrow_reader::ArrowReaderOptions::new()
            .with_page_index_policy(parquet::file::metadata::PageIndexPolicy::Optional);
        let meta = ArrowReaderMetadata::load(&input, reader_options).map_err(|e| perr(&e))?;
        let pmeta = meta.metadata().clone();
        let file_schema = meta.schema().clone();
        let geo = file_geo(&pmeta);
        let geo_columns: HashMap<String, Value> =
            geo["columns"].as_object().map(|c| c.iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default();
        let primary = geo["primary_column"].as_str().unwrap_or("geometry").to_string();
        let primary_col = geo_columns.get(&primary).cloned().unwrap_or(Value::Null);
        let covering = primary_col["covering"]["bbox"].as_object().and_then(|c| {
            let member = |k: &str| -> Option<(String, String)> {
                let p = c.get(k)?.as_array()?;
                Some((p.first()?.as_str()?.to_string(), p.get(1)?.as_str()?.to_string()))
            };
            let m = [member("xmin")?, member("ymin")?, member("xmax")?, member("ymax")?];
            // the four members of one struct column, as the specification has them
            m.iter().all(|(c, _)| *c == m[0].0).then(|| (m[0].0.clone(), m.map(|(_, n)| n)))
        });
        let index = |name: &str| file_schema.fields().iter().position(|f| f.name() == name);
        // Resolve names before projecting so a generated name selects the same physical
        // column whether it is read alone or with all the other attributes.
        let names = super::unique_column_names(file_schema.fields().iter().map(|f| f.name().clone()).collect());
        let named_fields: Vec<Field> = file_schema.fields().iter().zip(names).map(|(f, name)| f.as_ref().clone().with_name(name)).collect();

        // The output columns, in the file's order: the geometry columns, and the attributes asked
        // for (by default all but the covering column).
        // A name matches a column without case, and `name.member` names the struct column `name`
        // when it has that member (see `names_field`).
        if let Some(columns) = &options.columns
            && let Some(missing) = columns.iter().find(|c| !named_fields.iter().any(|f| names_field(f, c)))
        {
            return Err(Error::InvalidArgument(format!("no column named {missing:?} in the file")));
        }
        let wanted: Vec<usize> = (0..file_schema.fields().len())
            .filter(|&i| {
                let field = file_schema.field(i);
                geo_columns.contains_key(field.name())
                    || match &options.columns {
                        Some(columns) => columns.iter().any(|c| names_field(&named_fields[i], c)),
                        None => covering.as_ref().is_none_or(|(c, _)| c != field.name()),
                    }
            })
            .collect();
        let geom_index = index(&primary).filter(|_| !geo.is_null());
        let filter = options.bbox.map(|rect| match covering.clone().filter(|(c, _)| index(c).is_some()) {
            Some((c, members)) => (rect, Filter::Covering(c, members)),
            None => (rect, geom_index.map_or(Filter::None, |g| Filter::Geometry(file_schema.field(g).name().clone()))),
        });
        let mut read: Vec<usize> = wanted.clone();
        if let Some((_, Filter::Covering(c, _) | Filter::Geometry(c))) = &filter {
            read.extend(index(c));
        }
        read.sort_unstable();
        read.dedup();
        let descr = pmeta.file_metadata().schema_descr_ptr();
        let mask = ProjectionMask::roots(&descr, read.iter().copied());
        // With a covering column, a first pass reads it alone to select the rows, and the second
        // reads the columns handed out, of those rows only.
        let (cover_mask, main_mask) = match &filter {
            Some((_, Filter::Covering(c, _))) => {
                (Some(ProjectionMask::roots(&descr, index(c))), ProjectionMask::roots(&descr, wanted.iter().copied()))
            }
            _ => (None, mask.clone()),
        };
        // A projected batch is in source-root order, even when names repeat. The covering
        // pass reads only the wanted roots; otherwise filtering can add another root.
        let main_roots = if cover_mask.is_some() { &wanted } else { &read };
        let output_indices: Vec<usize> = wanted.iter().map(|i| main_roots.binary_search(i).expect("wanted root is read")).collect();

        // The output schema: the file's fields, the geometry columns tagged as GeoArrow.
        let fields: Vec<Field> = wanted
            .iter()
            .map(|&i| {
                let f = file_schema.field(i);
                match geo_columns.get(f.name()) {
                    Some(col) => geoarrow_field(f, col).with_name(named_fields[i].name()),
                    None => named_fields[i].clone(),
                }
            })
            .collect();
        let mut schema_meta = file_schema.metadata().clone();
        if !wanted.iter().any(|&i| covering.as_ref().is_some_and(|(c, _)| c == file_schema.field(i).name())) {
            // the covering column is not handed out: neither is its description
            let mut geo = geo.clone();
            if let Some(cols) = geo["columns"].as_object_mut() {
                cols.values_mut().filter_map(Value::as_object_mut).for_each(|c| {
                    c.remove("covering");
                });
            }
            if !geo.is_null() {
                schema_meta.insert("geo".into(), geo.to_string());
            }
        }
        let schema = Arc::new(Schema::new_with_metadata(fields, schema_meta));

        // The row groups whose statistics may hold rows in the rectangle.
        let leaves = covering_leaves(&pmeta, &geo);
        let bboxes = group_bboxes(&pmeta, leaves);
        let groups: Vec<usize> = (0..pmeta.num_row_groups())
            .filter(|&g| match (options.bbox, bboxes[g]) {
                (Some((x0, y0, x1, y1)), Some(b)) => b[0] <= x1 && b[2] >= x0 && b[1] <= y1 && b[3] >= y0,
                _ => true,
            })
            .collect();

        let batch_size = options.batch_size.max(1);
        let out = schema.clone();
        let encoding = primary_col["encoding"].as_str().unwrap_or("WKB").to_ascii_lowercase();
        let (window_meta, window_mask) = (pmeta.clone(), mask.clone());
        let decode: DecodeGroup = Box::new(move |g, _| {
            let err = |e: &dyn std::fmt::Display| perr(e);
            let read = |mask: &ProjectionMask, selection: Option<RowSelection>| {
                if geoparquet_zstd::has_zstd(&pmeta, &[g], mask) {
                    geoparquet_zstd::reader(input.clone(), &pmeta, &file_schema, mask.clone(), vec![g], selection, batch_size)
                } else {
                    let mut b = ParquetRecordBatchReaderBuilder::new_with_metadata(input.clone(), meta.clone())
                        .with_row_groups(vec![g])
                        .with_projection(mask.clone())
                        .with_batch_size(batch_size);
                    if let Some(selection) = selection {
                        b = b.with_row_selection(selection);
                    }
                    b.build()
                }
                .map_err(|e| err(&e))
            };
            let selection = match (&cover_mask, &filter) {
                (Some(cover), Some((rect, f))) => {
                    let mut keep = Vec::new();
                    for batch in read(cover, None)? {
                        keep.push(f.rows(&batch.map_err(|e| err(&e))?, *rect, &encoding)?);
                    }
                    if keep.iter().all(|k| k.true_count() == 0) {
                        return Ok(Vec::new());
                    }
                    Some(RowSelection::from_filters(&keep))
                }
                _ => None,
            };
            let selected = selection.is_some();
            let mut batches = Vec::new();
            for batch in read(&main_mask, selection)? {
                let batch = batch.map_err(|e| err(&e))?;
                let keep = match &filter {
                    Some((rect, f)) if !selected => Some(f.rows(&batch, *rect, &encoding)?),
                    _ => None,
                };
                let columns: Vec<ArrayRef> = output_indices
                    .iter()
                    .map(|&i| batch.columns().get(i).cloned().ok_or_else(|| err(&"a column not read")))
                    .collect::<Result<_>>()?;
                let projected = RecordBatch::try_new(out.clone(), columns).map_err(|e| err(&e))?;
                let projected = match keep {
                    Some(keep) => arrow_select::filter::filter_record_batch(&projected, &keep).map_err(|e| err(&e))?,
                    None => projected,
                };
                if projected.num_rows() > 0 {
                    batches.push(projected);
                }
            }
            Ok(batches)
        });
        let windows = GroupWindows::new(groups.into_iter().map(|g| (g, None)), decode, &window_meta, &window_mask, prefetch);
        Ok(Self { schema, windows, failed: false })
    }
}

impl Iterator for GeoParquetBatches {
    type Item = std::result::Result<RecordBatch, ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed {
            return None;
        }
        match self.windows.next() {
            Ok(b) => b.map(Ok),
            Err(e) => {
                self.failed = true;
                Some(Err(ArrowError::ExternalError(Box::new(e))))
            }
        }
    }
}

impl RecordBatchReader for GeoParquetBatches {
    fn schema(&self) -> SchemaRef {
        self.schema.clone()
    }
}

/// How the rows in a rectangle are found.
enum Filter {
    /// By the members `[xmin, ymin, xmax, ymax]` of the covering column of this name.
    Covering(String, [String; 4]),
    /// By the bounding boxes of the geometries of the column of this name.
    Geometry(String),
    /// Not at all: the file has no geometry.
    None,
}

impl Filter {
    /// Which rows of `batch` are in `rect`.
    fn rows(&self, batch: &RecordBatch, (x0, y0, x1, y1): (f64, f64, f64, f64), encoding: &str) -> Result<BooleanArray> {
        match self {
            Filter::Covering(c, members) => {
                let Some(s) = batch.column_by_name(c).and_then(|a| a.as_struct_opt()) else {
                    return Err(perr(&"the covering column is not a struct"));
                };
                let member = |m: &str| -> Result<Vec<f64>> {
                    let a = s.column_by_name(m).ok_or_else(|| perr(&format!("no covering member {m}")))?;
                    floats(a.as_ref())
                };
                let (bx0, by0, bx1, by1) = (member(&members[0])?, member(&members[1])?, member(&members[2])?, member(&members[3])?);
                Ok((0..batch.num_rows())
                    .map(|i| Some(!s.is_null(i) && bx0[i] <= x1 && bx1[i] >= x0 && by0[i] <= y1 && by1[i] >= y0))
                    .collect())
            }
            Filter::Geometry(g) => {
                let a = batch.column_by_name(g).ok_or_else(|| perr(&"no geometry column"))?;
                let a = a.as_ref();
                let inside = |b: [f64; 4]| b[0] <= x1 && b[2] >= x0 && b[1] <= y1 && b[3] >= y0;
                let keep: Vec<bool> = match (encoding, a.data_type()) {
                    ("wkb", DataType::Binary) => {
                        let a = a.as_binary::<i32>();
                        crate::par::map_range(a.len(), |i| !a.is_null(i) && wkb_bbox(a.value(i)).is_some_and(inside))
                    }
                    ("wkb", DataType::LargeBinary) => {
                        let a = a.as_binary::<i64>();
                        crate::par::map_range(a.len(), |i| !a.is_null(i) && wkb_bbox(a.value(i)).is_some_and(inside))
                    }
                    _ => crate::par::map_range(a.len(), |i| {
                        super::geoparquet::geometry(a, i, encoding)
                            .and_then(|g| g.bbox())
                            .is_some_and(|b| inside([b.min_x, b.min_y, b.max_x, b.max_y]))
                    }),
                };
                Ok(keep.into_iter().map(Some).collect())
            }
            Filter::None => Ok(BooleanArray::from(vec![true; batch.num_rows()])),
        }
    }
}

/// Whether the column `c` names the top-level field `field` of a file: by its name, or as a member
/// (`name.member`, at any depth) of a struct field that has that member.
fn names_field(field: &Field, c: &str) -> bool {
    let name = field.name();
    c.eq_ignore_ascii_case(name)
        || (crate::stream::is_or_under(c, name) && c.get(name.len() + 1..).is_some_and(|member| has_member(field.data_type(), member)))
}

/// Whether the struct type `data_type` has the member `path` (`a.b` for a member of a member),
/// matched without case.
fn has_member(data_type: &DataType, path: &str) -> bool {
    let DataType::Struct(children) = data_type else { return false };
    children.iter().any(|child| match path.split_once('.') {
        Some((head, rest)) => child.name().eq_ignore_ascii_case(head) && has_member(child.data_type(), rest),
        None => child.name().eq_ignore_ascii_case(path),
    })
}

/// The values of a covering member, as `f64`.
fn floats(a: &dyn Array) -> Result<Vec<f64>> {
    if let Some(a) = a.as_primitive_opt::<Float64Type>() {
        return Ok(a.values().to_vec());
    }
    if let Some(a) = a.as_primitive_opt::<Float32Type>() {
        return Ok(a.values().iter().map(|&v| v as f64).collect());
    }
    Err(perr(&format!("a covering member of type {}", a.data_type())))
}

/// A geometry column's field, tagged with its GeoArrow extension type and its CRS (OGC:CRS84
/// when the GeoParquet metadata names none, as the specification says).
fn geoarrow_field(f: &Field, col: &Value) -> Field {
    let encoding = col["encoding"].as_str().unwrap_or("WKB").to_ascii_lowercase();
    let mut ext = serde_json::Map::new();
    match col.get("crs") {
        None => {
            ext.insert("crs".into(), Value::String("OGC:CRS84".into()));
        }
        Some(Value::Null) => {}
        Some(crs) => {
            ext.insert("crs".into(), crs.clone());
        }
    }
    if let Some(edges) = col.get("edges").filter(|e| e.as_str() == Some("spherical")) {
        ext.insert("edges".into(), edges.clone());
    }
    let mut metadata = f.metadata().clone();
    metadata.insert(crate::arrow::EXTENSION_NAME.into(), format!("geoarrow.{encoding}"));
    metadata.insert(crate::arrow::EXTENSION_METADATA.into(), Value::Object(ext).to_string());
    f.clone().with_metadata(metadata)
}

/// The bounding box `[min_x, min_y, max_x, max_y]` of a WKB geometry (ISO or extended), read
/// without decoding it; `None` when it is empty or malformed.
pub(crate) fn wkb_bbox(b: &[u8]) -> Option<[f64; 4]> {
    let mut bb = [f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY];
    let mut at = 0;
    wkb_extend(b, &mut at, &mut bb, 0)?;
    (bb[0] <= bb[2] && bb[1] <= bb[3]).then_some(bb)
}

/// Extends `bb` by the geometry at `at`, moving past it.
fn wkb_extend(b: &[u8], at: &mut usize, bb: &mut [f64; 4], depth: u32) -> Option<()> {
    if depth > 32 {
        return None;
    }
    let le = match *b.get(*at)? {
        0 => false,
        1 => true,
        _ => return None,
    };
    *at += 1;
    let word = |at: &mut usize| -> Option<u32> {
        let s: [u8; 4] = b.get(*at..*at + 4)?.try_into().ok()?;
        *at += 4;
        Some(if le { u32::from_le_bytes(s) } else { u32::from_be_bytes(s) })
    };
    let raw = word(at)?;
    if raw & 0x2000_0000 != 0 {
        *at += 4; // an EWKB SRID
    }
    let code = raw & 0x0FFF_FFFF;
    let (kind, dims) = (code % 1000, code / 1000);
    let z = raw & 0x8000_0000 != 0 || dims == 1 || dims == 3;
    let m = raw & 0x4000_0000 != 0 || dims == 2 || dims == 3;
    let size = 8 * (2 + usize::from(z) + usize::from(m));
    let points = |at: &mut usize, n: u32, bb: &mut [f64; 4]| -> Option<()> {
        let end = at.checked_add((n as usize).checked_mul(size)?)?;
        let coords = b.get(*at..end)?;
        for p in coords.chunks_exact(size) {
            let f = |s: &[u8]| -> f64 {
                let s: [u8; 8] = s.try_into().unwrap_or_default();
                if le { f64::from_le_bytes(s) } else { f64::from_be_bytes(s) }
            };
            let (x, y) = (f(&p[0..8]), f(&p[8..16]));
            if !x.is_nan() && !y.is_nan() {
                bb[0] = bb[0].min(x);
                bb[1] = bb[1].min(y);
                bb[2] = bb[2].max(x);
                bb[3] = bb[3].max(y);
            }
        }
        *at = end;
        Some(())
    };
    match kind {
        1 => points(at, 1, bb),
        2 => {
            let n = word(at)?;
            points(at, n, bb)
        }
        3 => {
            for _ in 0..word(at)? {
                let n = word(at)?;
                points(at, n, bb)?;
            }
            Some(())
        }
        4..=7 => {
            for _ in 0..word(at)? {
                wkb_extend(b, at, bb, depth + 1)?;
            }
            Some(())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::wkb_bbox;
    use wbvector::{Coord, Geometry, Ring};

    #[test]
    fn repeated_columns_retain_positions_types_and_generated_selection_names() {
        use super::{GeoParquetBatches, GeoParquetReadOptions};
        use arrow_array::{Int32Array, Int64Array, RecordBatch, RecordBatchReader, StringArray};
        use arrow_schema::{DataType, Field, Schema};
        use std::sync::Arc;
        let schema = Arc::new(Schema::new(vec![
            Field::new("count", DataType::Int64, true),
            Field::new("count", DataType::Int32, true),
            Field::new("total", DataType::Utf8, true),
        ]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from(vec![1, 4])),
                Arc::new(Int32Array::from(vec![2, 5])),
                Arc::new(StringArray::from(vec!["a", "b"])),
            ],
        )
        .unwrap();
        let mut bytes = Vec::new();
        let mut writer = parquet::arrow::ArrowWriter::try_new(&mut bytes, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        for columns in [None, Some(vec!["count_2".into()])] {
            let selected = columns.is_some();
            let options = GeoParquetReadOptions { columns, ..Default::default() };
            let mut reader = GeoParquetBatches::open(bytes::Bytes::from(bytes.clone()), &options, None).unwrap();
            let names: Vec<_> = reader.schema().fields().iter().map(|f| f.name().clone()).collect();
            assert_eq!(names, if selected { vec!["count_2"] } else { vec!["count", "count_2", "total"] });
            let result = reader.next().unwrap().unwrap();
            let column = result.column_by_name("count_2").unwrap().as_any().downcast_ref::<Int32Array>().unwrap();
            assert_eq!(column.values().as_ref(), &[2, 5]);
            if !selected {
                assert_eq!(result.column(0).as_any().downcast_ref::<Int64Array>().unwrap().values().as_ref(), &[1, 4]);
                assert_eq!(result.column(2).as_any().downcast_ref::<StringArray>().unwrap().value(0), "a");
            }
        }
    }

    /// A remote file is read by range requests: its footer, then the column chunks asked for of
    /// the row groups whose statistics overlap the rectangle, not the whole file.
    #[cfg(remote)]
    #[test]
    fn reads_a_remote_file_by_its_column_chunks() {
        use crate::arrow::{GeoParquetReadOptions, open_geoparquet};
        use arrow_array::{BinaryArray, Float64Array, Int64Array, RecordBatch, StringArray, StructArray};
        use arrow_schema::{DataType, Field, Fields, Schema};
        use std::sync::Arc;
        // 4000 points along x, in row groups of 500, each row with 200 bytes of text.
        let n = 4000;
        let xs: Vec<f64> = (0..n).map(f64::from).collect();
        let coords = Fields::from(["xmin", "ymin", "xmax", "ymax"].map(|k| Field::new(k, DataType::Float64, false)).to_vec());
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("text", DataType::Utf8, false),
            Field::new("bbox", DataType::Struct(coords.clone()), false),
            Field::new("geometry", DataType::Binary, false),
        ]));
        let x = Arc::new(Float64Array::from(xs.clone()));
        let zero = Arc::new(Float64Array::from(vec![0.0; n as usize]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(Int64Array::from_iter_values(0..i64::from(n))),
                Arc::new(StringArray::from_iter_values((0..n).map(|i| format!("{i:0>200}")))),
                Arc::new(StructArray::new(coords, vec![x.clone(), zero.clone(), x, zero], None)),
                Arc::new(BinaryArray::from_iter_values(xs.iter().map(|&x| Geometry::point(x, 0.0).to_wkb()))),
            ],
        )
        .unwrap();
        let geo = serde_json::json!({"version": "1.1.0", "primary_column": "geometry", "columns": {"geometry": {
            "encoding": "WKB", "geometry_types": ["Point"],
            "covering": {"bbox": {"xmin": ["bbox", "xmin"], "ymin": ["bbox", "ymin"], "xmax": ["bbox", "xmax"], "ymax": ["bbox", "ymax"]}}}}});
        let props = parquet::file::properties::WriterProperties::builder()
            .set_max_row_group_row_count(Some(500))
            .set_key_value_metadata(Some(vec![parquet::file::metadata::KeyValue::new("geo".into(), geo.to_string())]))
            .build();
        let mut data = Vec::new();
        let mut w = parquet::arrow::ArrowWriter::try_new(&mut data, schema, Some(props)).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();

        let (base, seen) = crate::http::test_server(data.clone());
        let options = GeoParquetReadOptions { columns: Some(vec!["id".into()]), bbox: Some((10.0, -1.0, 99.5, 1.0)), ..Default::default() };
        let got: Vec<RecordBatch> = crate::config::with_options(&[("TERRIO_HTTP_BLOCK_SIZE", "4096")], || {
            open_geoparquet(&format!("{base}/points_by_chunks.parquet"), &options).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
        });
        let ids: Vec<i64> = got.iter().flat_map(|b| b.column(0).as_any().downcast_ref::<Int64Array>().unwrap().values().to_vec()).collect();
        assert_eq!(ids, (10..=99).collect::<Vec<_>>());
        assert_eq!(got[0].schema().fields().iter().map(|f| f.name().as_str()).collect::<Vec<_>>(), ["id", "geometry"]);
        let fetched = |seen: &[Option<String>]| -> usize {
            seen.iter()
                .filter_map(|r| {
                    let (a, b) = r.as_deref()?.strip_prefix("bytes=")?.split_once('-')?;
                    Some(b.parse::<usize>().ok()?.min(data.len() - 1) - a.parse::<usize>().ok()? + 1)
                })
                .sum()
        };
        {
            let seen = seen.lock().unwrap();
            assert!(seen.iter().all(Option::is_some), "only range requests");
            // the footer and one row group's id, covering and geometry chunks
            assert!(fetched(&seen) * 5 < data.len(), "{} of {} bytes fetched: {seen:?}", fetched(&seen), data.len());
        }

        // `open_vector` and `open_stream` read a URL in place too, not downloaded whole.
        let url = format!("{base}/points_by_features.parquet");
        let ids = |layer: &wbvector::Layer| -> Vec<i64> {
            layer
                .features
                .iter()
                .map(|f| match f.attributes[0] {
                    wbvector::FieldValue::Integer(i) => i,
                    ref v => panic!("{v:?}"),
                })
                .collect()
        };
        let options = crate::VectorOpenOptions { spatial_filter: Some((10.0, -1.0, 99.5, 1.0)), ..Default::default() };
        let before = fetched(&seen.lock().unwrap());
        let layer = crate::config::with_options(&[("TERRIO_HTTP_BLOCK_SIZE", "4096")], || {
            crate::open_vector(&url, &options).unwrap().layers.remove(0)
        });
        let by_open_vector = fetched(&seen.lock().unwrap()) - before;
        assert_eq!(ids(&layer), (10..=99).collect::<Vec<_>>());
        assert_eq!(layer.name, "points_by_features");
        assert!(by_open_vector * 3 < data.len(), "{by_open_vector} of {} bytes fetched by open_vector", data.len());
        let streamed = crate::config::with_options(&[("TERRIO_HTTP_BLOCK_SIZE", "4096")], || {
            crate::stream::open_layers(&url, &options, 40).unwrap().remove(0).read_all().unwrap()
        });
        assert_eq!(ids(&streamed), ids(&layer));

        // A stream opened and not read fetches the footer, not the data.
        let url = format!("{base}/points_unread.parquet");
        let before = fetched(&seen.lock().unwrap());
        let unread = crate::config::with_options(&[("TERRIO_HTTP_BLOCK_SIZE", "4096")], || {
            crate::stream::open_layers(&url, &crate::VectorOpenOptions::default(), 40).unwrap()
        });
        let at_open = fetched(&seen.lock().unwrap()) - before;
        assert!(at_open * 5 < data.len(), "{at_open} of {} bytes fetched by an open", data.len());
        drop(unread);
    }

    #[test]
    fn a_failed_window_stops_the_reader() {
        use super::{GroupWindows, perr};
        use arrow_array::{ArrayRef, Int32Array, RecordBatch};
        use std::sync::Arc;
        let batch = |g: usize| RecordBatch::try_from_iter([("g", Arc::new(Int32Array::from(vec![g as i32])) as ArrayRef)]).unwrap();
        // three row groups, a window of one each, the second failing
        let mut w = GroupWindows {
            groups: (0..3).map(|g| (g, None)).collect(),
            ready: Default::default(),
            decode: Box::new(move |g, _| if g == 1 { Err(perr(&"broken")) } else { Ok(vec![batch(g)]) }),
            ranges: vec![Vec::new(); 3],
            decoded: vec![0; 3],
            prefetch: None,
            context: crate::stac::AssetReadContext::capture(None),
            window: 1,
            failed: false,
        };
        assert!(w.next().unwrap().is_some());
        assert!(w.next().is_err());
        assert!(w.next().is_err(), "the third row group is not read after the second failed");
    }

    #[test]
    fn a_window_holds_row_groups_within_the_decoded_bytes_limit() {
        use super::{DECODED_WINDOW_BYTES, GroupWindows};
        use arrow_array::{ArrayRef, Int32Array, RecordBatch};
        use std::sync::{Arc, Mutex};
        // the row groups the first window decodes, for row groups of these decoded sizes
        let first_window = |decoded: &[u64]| -> Vec<usize> {
            let calls = Arc::new(Mutex::new(Vec::new()));
            let log = calls.clone();
            let batch = RecordBatch::try_from_iter([("g", Arc::new(Int32Array::from(vec![0i32])) as ArrayRef)]).unwrap();
            let mut w = GroupWindows {
                groups: (0..decoded.len()).map(|g| (g, None)).collect(),
                ready: Default::default(),
                decode: Box::new(move |g, _| {
                    log.lock().unwrap().push(g);
                    Ok(vec![batch.clone()])
                }),
                ranges: vec![Vec::new(); decoded.len()],
                decoded: decoded.to_vec(),
                prefetch: None,
                context: crate::stac::AssetReadContext::capture(None),
                window: 8,
                failed: false,
            };
            assert!(w.next().unwrap().is_some());
            let mut groups = calls.lock().unwrap().clone();
            groups.sort_unstable();
            groups
        };
        let half = DECODED_WINDOW_BYTES / 2;
        // two row groups of half the limit fill it exactly, so a third one does not fit
        assert_eq!(first_window(&[half, half, 1]), [0, 1]);
        assert_eq!(first_window(&[half + 1, half, 1]), [0]);
        // a first row group over the limit is read alone
        assert_eq!(first_window(&[DECODED_WINDOW_BYTES + 1, 1]), [0]);
    }

    #[test]
    fn wkb_bbox_reads_every_geometry_type() {
        let ring = |cs: &[(f64, f64)]| Ring::new(cs.iter().map(|&(x, y)| Coord::xy(x, y)).collect());
        let polygon = Geometry::Polygon {
            exterior: ring(&[(0.0, 0.0), (4.0, 0.0), (4.0, 3.0)]),
            interiors: vec![ring(&[(1.0, 1.0), (2.0, 1.0), (2.0, 2.0)])],
        };
        assert_eq!(wkb_bbox(&polygon.to_wkb()), Some([0.0, 0.0, 4.0, 3.0]));
        assert_eq!(wkb_bbox(&Geometry::point(-1.5, 2.0).to_wkb()), Some([-1.5, 2.0, -1.5, 2.0]));
        let multi = Geometry::MultiPolygon(vec![
            (ring(&[(0.0, 0.0), (1.0, 0.0), (1.0, 1.0)]), vec![]),
            (ring(&[(10.0, -5.0), (11.0, -5.0), (11.0, -4.0)]), vec![]),
        ]);
        assert_eq!(wkb_bbox(&multi.to_wkb()), Some([0.0, -5.0, 11.0, 1.0]));
        // ISO LineString Z, big-endian
        let mut line = vec![0u8];
        line.extend(1002u32.to_be_bytes());
        line.extend(2u32.to_be_bytes());
        for v in [1.0f64, 2.0, 99.0, 3.0, -4.0, 99.0] {
            line.extend(v.to_be_bytes());
        }
        assert_eq!(wkb_bbox(&line), Some([1.0, -4.0, 3.0, 2.0]));
        // an empty point (NaN coordinates), and truncated or unknown input
        let mut empty = vec![1u8];
        empty.extend(1u32.to_le_bytes());
        empty.extend(f64::NAN.to_le_bytes());
        empty.extend(f64::NAN.to_le_bytes());
        assert_eq!(wkb_bbox(&empty), None);
        assert_eq!(wkb_bbox(&polygon.to_wkb()[..20]), None);
        assert_eq!(wkb_bbox(&[1, 99, 0, 0, 0]), None);
        assert_eq!(wkb_bbox(&[]), None);
    }
}

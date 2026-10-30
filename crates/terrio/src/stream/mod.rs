//! Vector layers read and written as streams of features, for layers larger than memory.
//!
//! [`open_layers`] opens the layers of a dataset as [`FeatureReader`]s, which read the features a
//! chunk at a time: GeoPackage (a leaf page of the table at a time, through the R-tree with a
//! spatial filter), FlatGeobuf, GeoParquet (a record batch at a time, skipping row groups outside
//! a spatial filter), Shapefile, GeoJSON and GeoJSON text sequences, and CSV. GeoJSON and CSV
//! files are read twice: once to infer the fields, as GDAL does, then for the features. Other
//! formats are read whole and handed out in chunks.
//!
//! [`FeatureWriter`] writes a layer chunk by chunk: GeoJSON, GeoJSON text sequences, CSV,
//! Shapefile, FlatGeobuf (its features spooled to a temporary file, then sorted along the
//! Hilbert curve behind the spatial index, as GDAL does) and GeoParquet (a row group at a time).
//! The column types and headers of CSV, Shapefile, GeoPackage and GeoParquet files depend on the
//! values (the widths of Shapefile fields, 32 or 64-bit integers...): those writers take a
//! [`LayerProfile`] of the features, which a first pass over them computes, so that a streamed
//! file is identical to the one [`crate::write_vector`] writes. FlatGeobuf needs no first pass:
//! its types are decided as the features come, and the features spooled before they widen are
//! re-encoded. Other formats are written whole when the writer finishes.
//!
//! [`convert`] combines them, as `terrio convert` does for vector layers: the features flow from
//! the reader through the `ogr2ogr`-style options ([`crate::ops::vector_filter::Translator`]) to
//! the writer, with memory bounded by the chunk size.
//!
//! ```no_run
//! let mut readers = terrio::stream::open_layers("buildings.gpkg", &Default::default(), 65536)?;
//! let reader = &mut readers[0];
//! while let Some(chunk) = reader.read_chunk(10_000)? {
//!     println!("{} features", chunk.features.len());
//! }
//! # Ok::<(), terrio::Error>(())
//! ```

mod json;
#[cfg(feature = "sqlite")]
pub(crate) mod sqlite;

pub use crate::drivers::vector::profile::{FieldProfile, GeometryProfile, LayerProfile};

use crate::dataset::{VectorOpenOptions, VectorWriteOptions};
#[cfg(feature = "flatgeobuf")]
use crate::drivers::vector::flatgeobuf as fgb;
#[cfg(feature = "parquet")]
use crate::drivers::vector::geoparquet;
#[cfg(feature = "sqlite")]
use crate::drivers::vector::gpkg_read;
use crate::drivers::vector::{csv, geojson, shapefile};
use crate::formats::VectorDriver;
use crate::ops::vector_filter::{Translator, VectorTranslateOptions};
use crate::{Error, Result};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use wbvector::{Feature, FieldValue, Layer};

/// The default number of features in a chunk.
pub const DEFAULT_CHUNK: usize = 65536;

/// The most a chunk holds, about, in bytes: a chunk stops there even short of the number of
/// features asked for, so that layers of large geometries are read in bounded memory.
pub const CHUNK_BYTES: usize = 64 << 20;

/// The chunks of conversions to GeoPackage and FlatGeobuf, whose rows are written one after
/// another while the next chunk is read (see [`write_ahead`]): smaller chunks hold less, and cost
/// no time there.
const AHEAD_CHUNK_BYTES: usize = 16 << 20;

/// The raw bytes (WKB and record payloads) a source reads into a batch, at most about, for
/// chunks of `chunk_bytes`: an eighth, as a coordinate of 16 bytes in WKB takes 48 decoded, and
/// batches small next to a chunk keep the features buffered beyond it few.
fn source_bytes(chunk_bytes: usize) -> usize {
    chunk_bytes / 8
}

/// The memory a feature takes, about: its coordinates and its values (without their text, which
/// large geometries outweigh).
fn feature_size(f: &Feature) -> usize {
    let coords = f.geometry.as_ref().map_or(0, crate::vector::coord_count);
    std::mem::size_of::<Feature>()
        + coords * std::mem::size_of::<wbvector::Coord>()
        + f.attributes.len() * std::mem::size_of::<FieldValue>()
}

/// A source of features, read a batch at a time.
trait FeatureSource: Send {
    /// The next batch of features; `None` at the end.
    fn next_batch(&mut self) -> Result<Option<Vec<Feature>>>;
    /// Sets the raw bytes a batch holds, at most about (see [`source_bytes`]), for sources that
    /// count them.
    fn set_batch_bytes(&mut self, _bytes: usize) {}
}

/// A layer without its features: its name, fields, geometry type and CRS.
pub fn schema_of(layer: &Layer) -> Layer {
    Layer {
        name: layer.name.clone(),
        geom_type: layer.geom_type,
        crs: layer.crs.clone(),
        schema: layer.schema.clone(),
        features: Vec::new(),
        extent: None,
    }
}

/// A vector layer read as a stream of features.
pub struct FeatureReader {
    layer: Layer,
    source: Box<dyn FeatureSource>,
    /// The features read ahead, with their sizes (see [`feature_size`]), and the sum of those.
    buffer: VecDeque<(Feature, usize)>,
    buffered: usize,
    done: bool,
    /// The most a chunk holds, about, in bytes, and the most read ahead.
    chunk_bytes: usize,
}

impl FeatureReader {
    fn new(layer: Layer, source: Box<dyn FeatureSource>) -> Self {
        Self { layer: schema_of(&layer), source, buffer: VecDeque::new(), buffered: 0, done: false, chunk_bytes: CHUNK_BYTES }
    }

    /// Reads chunks of about `bytes` at most rather than [`CHUNK_BYTES`].
    fn set_chunk_bytes(&mut self, bytes: usize) {
        self.chunk_bytes = bytes;
        self.source.set_batch_bytes(source_bytes(bytes));
    }

    /// The layer, without its features: its name, fields, geometry type and CRS.
    pub fn layer(&self) -> &Layer {
        &self.layer
    }

    fn fill(&mut self, n: usize) -> Result<()> {
        while !self.done && self.buffer.len() < n && self.buffered < self.chunk_bytes {
            match self.source.next_batch()? {
                Some(batch) => {
                    let sizes = crate::par::map_range(batch.len(), |i| feature_size(&batch[i]));
                    self.buffered += sizes.iter().sum::<usize>();
                    self.buffer.extend(batch.into_iter().zip(sizes));
                }
                None => self.done = true,
            }
        }
        Ok(())
    }

    /// The next chunk of at most `max` features, and of about [`CHUNK_BYTES`] at most (at least
    /// one feature; conversions to GeoPackage and FlatGeobuf read smaller chunks), as a layer
    /// with this layer's fields; `None` once every feature has been read.
    pub fn read_chunk(&mut self, max: usize) -> Result<Option<Layer>> {
        let max = max.max(1);
        self.fill(max)?;
        if self.buffer.is_empty() {
            return Ok(None);
        }
        let (mut n, mut size) = (0, 0);
        while n < max.min(self.buffer.len()) && (n == 0 || size < self.chunk_bytes) {
            size += self.buffer[n].1;
            n += 1;
        }
        self.buffered -= size;
        let mut chunk = schema_of(&self.layer);
        chunk.features = self.buffer.drain(..n).map(|(f, _)| f).collect();
        Ok(Some(chunk))
    }

    /// Reads the remaining features into a layer.
    pub fn read_all(mut self) -> Result<Layer> {
        let mut layer = schema_of(&self.layer);
        while let Some(chunk) = self.read_chunk(DEFAULT_CHUNK)? {
            layer.features.extend(chunk.features);
        }
        Ok(layer)
    }
}

impl Iterator for FeatureReader {
    type Item = Result<Feature>;

    fn next(&mut self) -> Option<Result<Feature>> {
        if let Err(e) = self.fill(1) {
            self.done = true;
            self.buffer.clear();
            self.buffered = 0;
            return Some(Err(e));
        }
        let (f, size) = self.buffer.pop_front()?;
        self.buffered -= size;
        Some(Ok(f))
    }
}

/// Features held in memory, handed out in batches.
struct MemorySource {
    features: std::vec::IntoIter<Feature>,
    batch: usize,
}

impl FeatureSource for MemorySource {
    fn next_batch(&mut self) -> Result<Option<Vec<Feature>>> {
        let batch: Vec<Feature> = self.features.by_ref().take(self.batch).collect();
        Ok((!batch.is_empty()).then_some(batch))
    }
}

/// The drivers whose files [`open_layers`] reads as streams.
pub fn can_read_stream(driver: VectorDriver) -> bool {
    driver.missing_feature().is_none()
        && matches!(
            driver,
            VectorDriver::GPKG
                | VectorDriver::FlatGeobuf
                | VectorDriver::Parquet
                | VectorDriver::Shapefile
                | VectorDriver::GeoJSON
                | VectorDriver::GeoJSONSeq
                | VectorDriver::CSV
        )
}

/// Opens the layers of a vector dataset (those `options.layers` names, or all) as streams of
/// features, read in batches of about `batch` features. Formats terrio cannot read as streams
/// are read whole. Remote files are downloaded first (except as their drivers read them in
/// place).
pub fn open_layers(path: &str, options: &VectorOpenOptions, batch: usize) -> Result<Vec<FeatureReader>> {
    let batch = batch.max(1);
    // `.json` files may be STAC items or collections, which the dataset reader recognizes.
    let driver = VectorDriver::for_extension(path).filter(|_| crate::formats::extension(path).as_deref() != Some("json"));
    let local = crate::vfs::Location::parse(path).is_local() && std::path::Path::new(path).is_file();
    let Some(driver) = driver.filter(|d| local && can_read_stream(*d)) else {
        let ds = crate::open_vector(path, options)?;
        return Ok(ds
            .layers
            .into_iter()
            .map(|mut l| {
                let features = std::mem::take(&mut l.features);
                FeatureReader::new(l, Box::new(MemorySource { features: features.into_iter(), batch }))
            })
            .collect());
    };
    #[cfg(feature = "sqlite")]
    if driver == VectorDriver::GPKG {
        return open_gpkg(path, options, batch);
    }
    let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("layer");
    let (layer, source): (Layer, Box<dyn FeatureSource>) = match driver {
        VectorDriver::GeoJSON => open_geojson(path, stem, batch)?,
        VectorDriver::GeoJSONSeq => open_geojson_seq(path, stem, batch)?,
        VectorDriver::CSV => {
            let s = csv::CsvStream::open(path, &options.csv)?;
            (s.layer.clone(), Box::new(CsvSource { s, batch }))
        }
        VectorDriver::Shapefile => open_shapefile(path, options)?,
        #[cfg(feature = "flatgeobuf")]
        VectorDriver::FlatGeobuf => open_fgb(path, batch)?,
        #[cfg(feature = "parquet")]
        VectorDriver::Parquet => {
            let s = geoparquet::ParquetStream::open(|| Ok(File::open(path)?), stem, None, options.spatial_filter, batch, None)?;
            (s.layer.clone(), Box::new(ParquetSource(s)))
        }
        d => return Err(Error::Unsupported(format!("the {} driver is not read as a stream", d.name()))),
    };
    let layer = crate::dataset::fix_layer(layer, path, driver);
    if !options.layers.is_empty() && !options.layers.iter().any(|n| *n == layer.name || n == stem) {
        return Err(Error::InvalidArgument(format!("no layer named {} (layers: {})", options.layers.join(", "), layer.name)));
    }
    Ok(vec![FeatureReader::new(layer, source)])
}

// GeoJSON

/// The features of a GeoJSON document: a FeatureCollection read feature by feature.
struct GeoJsonSource {
    collection: json::Collection<File>,
    decoder: geojson::FeatureDecoder,
    n: u64,
    batch: usize,
}

impl FeatureSource for GeoJsonSource {
    fn next_batch(&mut self) -> Result<Option<Vec<Feature>>> {
        let mut raw = Vec::new();
        let mut buf = Vec::new();
        while raw.len() < self.batch && self.collection.next_feature(&mut buf)? {
            raw.push(std::mem::take(&mut buf));
        }
        if raw.is_empty() {
            return Ok(None);
        }
        let features = self.decoder.features_raw(self.n, &raw)?;
        self.n += raw.len() as u64;
        Ok(Some(features))
    }
}

/// Infers the fields of a batch of features in parallel, then merges them in order.
fn observe_batch(scan: &mut geojson::SchemaScan, raw: &mut Vec<Vec<u8>>) -> Result<()> {
    if raw.is_empty() {
        return Ok(());
    }
    // The first feature decides whether the features are STAC items.
    let mut start = 0;
    if !scan.started() {
        scan.observe_raw(&raw[0])?;
        start = 1;
    }
    let base = scan.empty_like();
    let parts = crate::par::map_range(crate::par::threads().max(1), |t| -> Result<geojson::SchemaScan> {
        let n = raw.len() - start;
        let (a, b) = (start + n * t / crate::par::threads().max(1), start + n * (t + 1) / crate::par::threads().max(1));
        let mut s = base.clone();
        for r in &raw[a..b] {
            s.observe_raw(r)?;
        }
        Ok(s)
    });
    for p in parts {
        scan.merge(p?);
    }
    raw.clear();
    Ok(())
}

fn open_geojson(path: &str, stem: &str, batch: usize) -> Result<(Layer, Box<dyn FeatureSource>)> {
    let mut c = json::Collection::new(File::open(path)?)?;
    let mut scan = geojson::SchemaScan::default();
    let mut raw = Vec::new();
    let mut buf = Vec::new();
    while c.next_feature(&mut buf)? {
        raw.push(std::mem::take(&mut buf));
        if raw.len() >= 16384 {
            observe_batch(&mut scan, &mut raw)?;
        }
    }
    observe_batch(&mut scan, &mut raw)?;
    if !c.has_features || c.members.get("type").and_then(|t| t.as_str()) != Some("FeatureCollection") {
        // A single feature or geometry: small enough to read whole.
        let layer = geojson::read(path)?;
        let mut l = layer;
        let features = std::mem::take(&mut l.features);
        return Ok((l, Box::new(MemorySource { features: features.into_iter(), batch })));
    }
    let name = c.members.get("name").and_then(|n| n.as_str()).unwrap_or(stem).to_string();
    let crs = c.members.get("crs").and_then(|c| c["properties"]["name"].as_str()).and_then(|n| crate::crs::SpatialRef::parse(n).ok());
    let layer = scan.layer(&name, crs);
    let decoder = geojson::FeatureDecoder::new(&layer, &scan);
    let collection = json::Collection::new(File::open(path)?)?;
    Ok((layer, Box::new(GeoJsonSource { collection, decoder, n: 0, batch })))
}

/// The features of a GeoJSON text sequence, a line each.
struct SeqSource {
    r: BufReader<File>,
    decoder: geojson::FeatureDecoder,
    n: u64,
    batch: usize,
}

/// The next non-empty record of a text sequence, without its separators.
fn next_seq_record(r: &mut BufReader<File>, line: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    loop {
        line.clear();
        if std::io::BufRead::read_until(r, b'\n', line)? == 0 {
            return Ok(None);
        }
        let rec = geojson::seq_record(line);
        if !rec.is_empty() {
            return Ok(Some(rec.to_vec()));
        }
    }
}

impl FeatureSource for SeqSource {
    fn next_batch(&mut self) -> Result<Option<Vec<Feature>>> {
        let mut raw = Vec::new();
        let mut line = Vec::new();
        while raw.len() < self.batch {
            match next_seq_record(&mut self.r, &mut line)? {
                Some(rec) => raw.push(rec),
                None => break,
            }
        }
        if raw.is_empty() {
            return Ok(None);
        }
        let features = self.decoder.features_raw(self.n, &raw)?;
        self.n += raw.len() as u64;
        Ok(Some(features))
    }
}

fn open_geojson_seq(path: &str, stem: &str, batch: usize) -> Result<(Layer, Box<dyn FeatureSource>)> {
    let mut r = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut scan = geojson::SchemaScan::default();
    let mut raw = Vec::new();
    let mut line = Vec::new();
    while let Some(rec) = next_seq_record(&mut r, &mut line)? {
        raw.push(rec);
        if raw.len() >= 16384 {
            observe_batch(&mut scan, &mut raw)?;
        }
    }
    observe_batch(&mut scan, &mut raw)?;
    let layer = scan.layer(stem, None);
    let decoder = geojson::FeatureDecoder::new(&layer, &scan);
    let r = BufReader::with_capacity(1 << 20, File::open(path)?);
    Ok((layer, Box::new(SeqSource { r, decoder, n: 0, batch })))
}

// CSV

struct CsvSource {
    s: csv::CsvStream,
    batch: usize,
}

impl FeatureSource for CsvSource {
    fn next_batch(&mut self) -> Result<Option<Vec<Feature>>> {
        self.s.next_rows(self.batch)
    }
}

// Shapefile

struct ShpSource {
    shp: BufReader<File>,
    dbf: Option<(BufReader<File>, shapefile::DbfLayout, usize)>,
    fields: usize,
    fid: u64,
    shp_done: bool,
    batch_bytes: usize,
    organize: shapefile::OrganizePolygons,
}

impl ShpSource {
    /// The next shape record's bytes: `None` at the end of the file.
    fn next_shape(&mut self) -> Result<Option<Vec<u8>>> {
        if self.shp_done {
            return Ok(None);
        }
        let mut head = [0u8; 8];
        if read_full(&mut self.shp, &mut head)? < 8 {
            self.shp_done = true;
            return Ok(None);
        }
        let words = u32::from_be_bytes([head[4], head[5], head[6], head[7]]);
        // Read as it comes rather than allocated from the (untrusted) length.
        let mut rec = Vec::new();
        (&mut self.shp).take(2 * u64::from(words)).read_to_end(&mut rec)?;
        Ok(Some(rec))
    }

    /// Up to `n` attribute records' bytes, one after another: how many were read.
    fn next_records(&mut self, n: usize, out: &mut Vec<u8>) -> Result<usize> {
        let Some((r, layout, remaining)) = &mut self.dbf else { return Ok(0) };
        let n = n.min(*remaining);
        let len = layout.record_len;
        out.resize(n * len, 0);
        let got = read_full(r, out)? / len.max(1);
        out.truncate(got * len);
        // A file cut short ends the records.
        *remaining = if got < n { 0 } else { *remaining - got };
        Ok(got)
    }
}

/// Reads as many bytes as the buffer holds or the stream has: the number read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(n)
}

impl FeatureSource for ShpSource {
    fn set_batch_bytes(&mut self, bytes: usize) {
        self.batch_bytes = bytes;
    }

    fn next_batch(&mut self) -> Result<Option<Vec<Feature>>> {
        const BATCH: usize = 8192;
        // The bytes are read in turn, then decoded in parallel; large shapes or records end a
        // batch sooner, the records read as many as the shapes.
        let record_len = self.dbf.as_ref().map_or(0, |(_, l, _)| l.record_len);
        let limit = BATCH.min(self.batch_bytes / record_len.max(1)).max(1);
        let (mut shapes, mut bytes) = (Vec::new(), 0);
        while shapes.len() < limit
            && bytes < self.batch_bytes
            && let Some(rec) = self.next_shape()?
        {
            bytes += rec.len();
            shapes.push(rec);
        }
        let mut records = Vec::new();
        // As many records as shapes; past the shapes' end, the records left, a batch at a time.
        let n = self.next_records(if shapes.is_empty() { limit } else { shapes.len() }, &mut records)?;
        let count = shapes.len().max(n);
        if count == 0 {
            return Ok(None);
        }
        let (fields, first) = (self.fields, self.fid);
        let (layout, organize) = (self.dbf.as_ref().map(|(_, l, _)| l), self.organize);
        let out = crate::par::map_range(count, |i| {
            let geometry = shapes.get(i).and_then(|rec| shapefile::decode_shape(rec, organize));
            let attributes = match layout {
                Some(l) if i < n => l.record(&records[i * l.record_len..(i + 1) * l.record_len]),
                _ => vec![wbvector::FieldValue::Null; fields],
            };
            Feature { fid: first + i as u64, geometry, attributes }
        });
        self.fid += count as u64;
        Ok(Some(out))
    }
}

fn open_shapefile(path: &str, options: &crate::VectorOpenOptions) -> Result<(Layer, Box<dyn FeatureSource>)> {
    let base = std::path::Path::new(path).with_extension("");
    let sidecar = |ext: &str| [ext.to_string(), ext.to_ascii_uppercase()].into_iter().map(|e| base.with_extension(e)).find(|p| p.exists());
    let name = base.file_name().and_then(|s| s.to_str()).unwrap_or("layer");
    let mut shp = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut head = [0u8; 100];
    let n = read_full(&mut shp, &mut head)?;
    let mut layer = shapefile::header_layer(&head[..n], name).map_err(|_| Error::InvalidData(format!("'{path}' is not a shapefile")))?;
    let cpg = sidecar("cpg").and_then(|p| std::fs::read_to_string(p).ok());
    let dbf = match sidecar("dbf") {
        Some(p) => {
            let mut r = BufReader::with_capacity(1 << 20, File::open(p)?);
            let mut start = [0u8; 32];
            let n = read_full(&mut r, &mut start)?;
            let len = shapefile::dbf_header_len(&start[..n])?;
            let mut header = start[..n].to_vec();
            header.resize(len.max(n), 0);
            let k = read_full(&mut r, &mut header[n..])?;
            header.truncate(n + k);
            let layout = shapefile::dbf_layout(&header, cpg.as_deref())?;
            // Records start at the header length (past any padding).
            r.seek(SeekFrom::Start(layout.header_len as u64))?;
            for f in &layout.fields {
                layer.add_field(f.clone());
            }
            let count = layout.count;
            Some((r, layout, count))
        }
        None => None,
    };
    let prj = sidecar("prj").and_then(|p| std::fs::read_to_string(p).ok());
    if prj.is_some() {
        crate::crs::prefetch();
    }
    shapefile::apply_prj(&mut layer, prj.as_deref());
    let fields = layer.schema.len();
    Ok((
        layer,
        Box::new(ShpSource {
            shp,
            dbf,
            fields,
            fid: 0,
            shp_done: false,
            batch_bytes: source_bytes(CHUNK_BYTES),
            organize: options.shapefile.method(),
        }),
    ))
}

// FlatGeobuf

#[cfg(feature = "flatgeobuf")]
struct FgbSource {
    r: BufReader<File>,
    header: fgb::FgbHeader,
    fid: u64,
    batch: usize,
    batch_bytes: usize,
}

#[cfg(feature = "flatgeobuf")]
impl FeatureSource for FgbSource {
    fn set_batch_bytes(&mut self, bytes: usize) {
        self.batch_bytes = bytes;
    }

    fn next_batch(&mut self) -> Result<Option<Vec<Feature>>> {
        let (mut raw, mut bytes) = (Vec::new(), 0);
        while raw.len() < self.batch && bytes < self.batch_bytes {
            let mut len = [0u8; 4];
            if read_full(&mut self.r, &mut len)? < 4 {
                break;
            }
            let n = u32::from_le_bytes(len) as usize;
            // Read as it comes rather than allocated from the (untrusted) length.
            let mut buf = len.to_vec();
            (&mut self.r).take(n as u64).read_to_end(&mut buf)?;
            if buf.len() < 4 + n {
                return Err(Error::InvalidData("FlatGeobuf: truncated feature".into()));
            }
            bytes += buf.len();
            raw.push(buf);
        }
        if raw.is_empty() {
            return Ok(None);
        }
        let (h, first) = (&self.header, self.fid);
        let features: Result<Vec<Feature>> =
            crate::par::map_range(raw.len(), |i| fgb::decode_feature(&raw[i], h, first + i as u64)).into_iter().collect();
        self.fid += raw.len() as u64;
        features.map(Some)
    }
}

#[cfg(feature = "flatgeobuf")]
fn open_fgb(path: &str, batch: usize) -> Result<(Layer, Box<dyn FeatureSource>)> {
    let mut r = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut start = [0u8; 12];
    let n = read_full(&mut r, &mut start)?;
    let len = fgb::header_len(&start[..n])?;
    let mut buf = start.to_vec();
    buf.resize(len, 0);
    if read_full(&mut r, &mut buf[12..])? < len - 12 {
        return Err(Error::InvalidData("FlatGeobuf: truncated header".into()));
    }
    let (layer, header) = fgb::parse_header(&buf)?;
    r.seek_relative(header.index_size as i64)?;
    Ok((layer, Box::new(FgbSource { r, header, fid: 0, batch, batch_bytes: source_bytes(CHUNK_BYTES) })))
}

// GeoParquet

#[cfg(feature = "parquet")]
struct ParquetSource(geoparquet::ParquetStream);

#[cfg(feature = "parquet")]
impl FeatureSource for ParquetSource {
    fn next_batch(&mut self) -> Result<Option<Vec<Feature>>> {
        self.0.next_batch()
    }
}

// GeoPackage

#[cfg(feature = "sqlite")]
struct GpkgSource {
    db: sqlite::FileDb,
    def: gpkg_read::LayerDef,
    columns: usize,
    rowid_col: Option<usize>,
    rows: GpkgRows,
    bbox: Option<gpkg_read::Rect>,
    n: i64,
    batch: usize,
    batch_bytes: usize,
}

#[cfg(feature = "sqlite")]
enum GpkgRows {
    Scan(sqlite::Cursor),
    Lookup { root: u32, ids: std::vec::IntoIter<i64> },
}

#[cfg(feature = "sqlite")]
impl FeatureSource for GpkgSource {
    fn set_batch_bytes(&mut self, bytes: usize) {
        self.batch_bytes = bytes;
    }

    fn next_batch(&mut self) -> Result<Option<Vec<Feature>>> {
        let mut rows: Vec<(i64, Vec<u8>)> = Vec::new();
        match &mut self.rows {
            GpkgRows::Scan(cursor) => {
                let mut bytes = 0;
                while rows.len() < self.batch && bytes < self.batch_bytes {
                    match cursor.next_leaf(&self.db)? {
                        Some(leaf) => {
                            bytes += leaf.iter().map(|(_, p)| p.len()).sum::<usize>();
                            rows.extend(leaf);
                        }
                        None => break,
                    }
                }
            }
            GpkgRows::Lookup { root, ids } => {
                // Ids without a row (stale R-tree entries) are skipped: only the end of the ids
                // ends the stream.
                let mut bytes = 0;
                while rows.len() < self.batch && bytes < self.batch_bytes {
                    let Some(id) = ids.next() else { break };
                    if let Some(p) = gpkg_read::Tables::lookup(&self.db, *root, id)? {
                        bytes += p.len();
                        rows.push((id, p));
                    }
                }
            }
        }
        if rows.is_empty() {
            return Ok(None);
        }
        let (def, columns, rowid_col, bbox, first) = (&self.def, self.columns, self.rowid_col, self.bbox, self.n);
        let features = crate::par::map_range(rows.len(), |i| {
            let (rowid, payload) = &rows[i];
            def.feature(&sqlite::row_values(payload, *rowid, columns, rowid_col), first + i as i64, bbox)
        });
        self.n += rows.len() as i64;
        Ok(Some(features.into_iter().flatten().collect()))
    }
}

#[cfg(feature = "sqlite")]
fn open_gpkg(path: &str, options: &VectorOpenOptions, batch: usize) -> Result<Vec<FeatureReader>> {
    let db = sqlite::FileDb::open(path)?;
    let (layers, _) = gpkg_read::contents(&db)?;
    if layers.is_empty() {
        // Rasters only: as the dataset reader reports it.
        return open_layers_whole(path, options, batch);
    }
    for w in &options.layers {
        if !layers.iter().any(|c| c.name.eq_ignore_ascii_case(w)) {
            let all: Vec<&str> = layers.iter().map(|c| c.name.as_str()).collect();
            return Err(Error::InvalidArgument(format!("no layer named {w} (layers: {})", all.join(", "))));
        }
    }
    crate::crs::prefetch();
    let mut out = Vec::new();
    for c in layers.iter().filter(|c| options.layers.is_empty() || options.layers.iter().any(|w| w.eq_ignore_ascii_case(&c.name))) {
        let db = sqlite::FileDb::open(path)?;
        let def = gpkg_read::layer_def(&db, c)?;
        let (root, sql) = db.table_root(&c.name)?;
        let columns = crate::drivers::sqlite::column_types(&sql).len();
        let rowid_col = crate::drivers::sqlite::rowid_column(&sql);
        let bbox = options.spatial_filter;
        let candidates = match (&def.geom_name, bbox, def.fid_col) {
            (Some(g), Some(rect), Some(_)) => gpkg_read::rtree_candidates(&db, &format!("rtree_{}_{g}", c.name), rect)?,
            _ => None,
        };
        let rows = match candidates {
            Some(ids) => GpkgRows::Lookup { root, ids: ids.into_iter() },
            None => GpkgRows::Scan(db.cursor(root)),
        };
        let layer = crate::dataset::fix_layer(def.layer.clone(), path, VectorDriver::GPKG);
        let source = GpkgSource { db, def, columns, rowid_col, rows, bbox, n: 0, batch, batch_bytes: source_bytes(CHUNK_BYTES) };
        out.push(FeatureReader::new(layer, Box::new(source)));
    }
    Ok(out)
}

fn open_layers_whole(path: &str, options: &VectorOpenOptions, batch: usize) -> Result<Vec<FeatureReader>> {
    let ds = crate::open_vector(path, options)?;
    Ok(ds
        .layers
        .into_iter()
        .map(|mut l| {
            let features = std::mem::take(&mut l.features);
            FeatureReader::new(l, Box::new(MemorySource { features: features.into_iter(), batch }))
        })
        .collect())
}

// Writers

/// A destination of features, written a chunk at a time.
trait FeatureSink: Send {
    fn write(&mut self, chunk: &Layer) -> Result<()>;
    /// Writes a chunk the sink may keep.
    fn write_owned(&mut self, chunk: Layer) -> Result<()> {
        self.write(&chunk)
    }
    fn finish(self: Box<Self>) -> Result<()>;
}

/// The drivers [`FeatureWriter`] writes as streams (the others are written whole when the
/// writer finishes).
pub fn can_write_stream(driver: VectorDriver) -> bool {
    driver.missing_feature().is_none()
        && matches!(
            driver,
            VectorDriver::GeoJSON
                | VectorDriver::GeoJSONSeq
                | VectorDriver::GPKG
                | VectorDriver::CSV
                | VectorDriver::Shapefile
                | VectorDriver::FlatGeobuf
                | VectorDriver::Parquet
        )
}

/// Whether the writer of a driver needs a [`LayerProfile`] of the features of `layer` (its
/// fields) to write them as a stream. GeoParquet needs one only for integer, date and date-time
/// fields, whose column types depend on the values.
#[cfg_attr(not(feature = "parquet"), allow(unused_variables))]
pub fn needs_profile(driver: VectorDriver, layer: &Layer) -> bool {
    match driver {
        #[cfg(feature = "parquet")]
        VectorDriver::Parquet => geoparquet::ParquetEncoder::needs_profile(layer),
        _ => matches!(driver, VectorDriver::CSV | VectorDriver::Shapefile | VectorDriver::GPKG),
    }
}

/// A vector layer written as a stream of features.
pub struct FeatureWriter {
    sink: Box<dyn FeatureSink>,
}

impl FeatureWriter {
    /// Creates the file of a layer (`layer` gives its name, fields, geometry type and CRS; its
    /// features are not written) at `path`. Writers that [`needs_profile`] use `profile`, the
    /// profile of all the features to be written; without it they write the features whole when
    /// the writer finishes.
    pub fn create(path: &str, layer: &Layer, profile: Option<&LayerProfile>, options: &VectorWriteOptions) -> Result<Self> {
        if !crate::vfs::Location::parse(path).is_local() {
            return Err(Error::Unsupported(format!("cannot write to '{path}': only local outputs are supported")));
        }
        let driver = match options.driver {
            Some(d) => d,
            None => VectorDriver::for_extension(path)
                .ok_or_else(|| Error::InvalidArgument(format!("cannot tell the output format of '{path}': give it explicitly")))?,
        };
        let schema = schema_of(layer);
        let streamed = can_write_stream(driver) && (profile.is_some() || !needs_profile(driver, &schema));
        let sink: Box<dyn FeatureSink> = match (streamed, driver, profile) {
            (false, ..) => Box::new(WholeSink {
                layer: schema,
                path: path.to_string(),
                options: VectorWriteOptions { driver: Some(driver), ..options.clone() },
            }),
            (true, VectorDriver::GeoJSON, _) => {
                let enc = geojson::CollectionEncoder::new(&schema, &options.geojson)?;
                let mut out = BufWriter::with_capacity(1 << 20, File::create(path)?);
                out.write_all(enc.head().as_bytes())?;
                Box::new(GeoJsonSink { out, enc })
            }
            (true, VectorDriver::GeoJSONSeq, _) => Box::new(SeqSink {
                out: BufWriter::with_capacity(1 << 20, File::create(path)?),
                enc: geojson::SeqEncoder::new(&schema, &options.geojson)?,
            }),
            (true, VectorDriver::CSV, Some(p)) => {
                let enc = csv::CsvEncoder::new(&schema, p, &options.csv)?;
                let mut out = BufWriter::with_capacity(1 << 20, File::create(path)?);
                out.write_all(enc.header().as_bytes())?;
                Box::new(CsvSink { out, enc })
            }
            (true, VectorDriver::Shapefile, Some(p)) => Box::new(ShpSink::create(path, &schema, p)?),
            #[cfg(feature = "flatgeobuf")]
            (true, VectorDriver::FlatGeobuf, _) => Box::new(FgbSink::create(path, &schema)?),
            #[cfg(feature = "sqlite")]
            (true, VectorDriver::GPKG, Some(p)) => match GpkgSink::create(path, &schema, p)? {
                Some(sink) => Box::new(sink),
                // A field named fid would give the rowids, which must then be checked unique over
                // all the features: written whole.
                None => Box::new(WholeSink {
                    layer: schema,
                    path: path.to_string(),
                    options: VectorWriteOptions { driver: Some(driver), ..options.clone() },
                }),
            },
            #[cfg(feature = "parquet")]
            (true, VectorDriver::Parquet, p) => {
                let enc = geoparquet::ParquetEncoder::new(&schema, p.unwrap_or(&LayerProfile::new(&schema)));
                let writer = enc.writer(BufWriter::with_capacity(1 << 20, File::create(path)?))?;
                Box::new(ParquetSink::new(writer, enc))
            }
            _ => unreachable!("every streamed driver is handled"),
        };
        Ok(Self { sink })
    }

    /// Writes a chunk of features (a layer with the fields given to [`FeatureWriter::create`]).
    pub fn write(&mut self, chunk: &Layer) -> Result<()> {
        self.sink.write(chunk)
    }

    /// Writes a chunk of features, as [`FeatureWriter::write`], without copying those the
    /// writer keeps.
    pub(crate) fn write_owned(&mut self, chunk: Layer) -> Result<()> {
        self.sink.write_owned(chunk)
    }

    /// Completes the file.
    pub fn finish(self) -> Result<()> {
        self.sink.finish()
    }
}

/// A GeoPackage written as the features come: the layer's table is streamed to the file
/// ([`StreamTable`](crate::drivers::vector::sqlite::StreamTable)), its rows encoded a chunk at a
/// time, keeping only each geometry's bounds for the R-tree; the metadata tables, R-tree and
/// schema are written after it. The tables and values are those of the whole-layer writer,
/// whose schema decisions (geometry type, Z, MEDIUMINT) are taken from the profile.
#[cfg(feature = "sqlite")]
struct GpkgSink {
    table: crate::drivers::vector::sqlite::StreamTable<BufWriter<File>>,
    builder: crate::drivers::vector::geopackage::Builder,
    cols: crate::drivers::vector::geopackage::LayerColumns,
    layer: Layer,
    extent: crate::drivers::vector::geopackage::LayerExtent,
    count: usize,
}

#[cfg(feature = "sqlite")]
impl GpkgSink {
    /// `None` when the layer has an integer field named fid (see `FeatureWriter::create`).
    fn create(path: &str, schema: &Layer, profile: &LayerProfile) -> Result<Option<Self>> {
        use wbvector::FieldType;
        let fields = schema.schema.fields();
        if fields.iter().any(|f| f.name.eq_ignore_ascii_case("fid") && f.field_type == FieldType::Integer) {
            return Ok(None);
        }
        let mut builder = crate::drivers::vector::geopackage::Builder::new();
        builder.claim(&schema.name)?;
        let g = &profile.geometry;
        let all_none = g.count == 0;
        let declared = if all_none { schema.geom_type } else { g.first.filter(|(_, same)| *same).map(|(t, _)| t) };
        let small = |i: usize| profile.fields.get(i).is_none_or(|f| f.no_int_beyond_int32);
        let cols = builder.layer_columns(schema, all_none, declared, g.has_z, None, small);
        // The file is created anew: pages left over from a larger file would remain at its end.
        let out = BufWriter::with_capacity(1 << 20, File::create(path)?);
        Ok(Some(Self {
            table: crate::drivers::vector::sqlite::StreamTable::new(out),
            builder,
            cols,
            layer: schema.clone(),
            extent: Default::default(),
            count: 0,
        }))
    }
}

#[cfg(feature = "sqlite")]
impl FeatureSink for GpkgSink {
    fn write(&mut self, chunk: &Layer) -> Result<()> {
        let fields = self.layer.schema.fields();
        let first = self.count as i64 + 1;
        let (cols, features) = (&self.cols, &chunk.features);
        let rows = crate::par::map_range(features.len(), |n| {
            let (rowid, values, bbox) = cols.row(&features[n], first + n as i64, fields);
            (rowid, crate::drivers::vector::sqlite::record(&values), bbox)
        });
        for (rowid, rec, bbox) in rows {
            self.table.push(rowid, &rec)?;
            self.extent.add(rowid, bbox);
        }
        self.count += features.len();
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<()> {
        let Self { table, mut builder, cols, layer, extent, count } = *self;
        let (root, mut next, mut out) = table.finish_with_output()?;
        let built = crate::drivers::vector::sqlite::BuiltTable { name: layer.name.clone(), sql: cols.sql.clone(), root };
        // The R-tree's tables follow the layer's.
        let mut write = |t, rows: &mut crate::drivers::vector::sqlite::Rows| {
            let (built, after, _) = crate::drivers::vector::sqlite::stream_table(&mut out, next, t, rows)?;
            next = after;
            Ok(built)
        };
        let table = crate::drivers::vector::geopackage::LayerTable::Built(built);
        builder.finish_layer(&layer.name, &cols, count, count as i64, extent, table, Some(&mut write))?;
        let (page1, rest) = builder.finish_parts(next)?;
        out.seek(SeekFrom::Start(u64::from(next - 1) * page1.len() as u64))?;
        out.write_all(&rest)?;
        out.seek(SeekFrom::Start(0))?;
        out.write_all(&page1)?;
        out.flush()?;
        Ok(())
    }
}

/// A GeoPackage of several layers whose features arrive in any interleaving (a chunk of one
/// layer, then of another): each layer's rows are encoded as they come into a temporary file of
/// its own, with their bounds, then copied into the database one table after another
/// ([`StreamTable`](crate::drivers::vector::sqlite::StreamTable)). The profile deciding the
/// schema (geometry type, Z, MEDIUMINT) is gathered on the way, as it only shows in the tables'
/// definitions, written last. The tables and values are those of the whole-layer writer.
#[cfg(feature = "sqlite")]
pub(crate) struct GpkgLayersSink {
    out: BufWriter<File>,
    builder: crate::drivers::vector::geopackage::Builder,
    layers: Vec<SpilledLayer>,
}

#[cfg(feature = "sqlite")]
struct SpilledLayer {
    schema: Layer,
    cols: crate::drivers::vector::geopackage::LayerColumns,
    profile: LayerProfile,
    rows: BufWriter<Box<dyn crate::drivers::vector::Spill>>,
    count: usize,
}

#[cfg(feature = "sqlite")]
impl GpkgLayersSink {
    /// `None` when a layer has an integer field named fid (which gives the rowids, to be checked
    /// unique over all the features), or neither a geometry type nor a CRS (whether it is a
    /// spatial table depends on all its geometries): such layers are written whole.
    pub(crate) fn create(path: &str, schemas: &[Layer]) -> Result<Option<Self>> {
        use wbvector::FieldType;
        let mut builder = crate::drivers::vector::geopackage::Builder::new();
        let mut layers = Vec::with_capacity(schemas.len());
        for schema in schemas {
            let fid = schema.schema.fields().iter().any(|f| f.name.eq_ignore_ascii_case("fid") && f.field_type == FieldType::Integer);
            if fid || (schema.geom_type.is_none() && crate::crs::SpatialRef::from_layer(schema).is_unknown()) {
                return Ok(None);
            }
            builder.claim(&schema.name)?;
            // The columns as rows are encoded; their declared types are settled at the end.
            let cols = builder.layer_columns(schema, false, None, false, None, |_| true);
            let rows = BufWriter::with_capacity(1 << 20, crate::drivers::vector::spill()?);
            layers.push(SpilledLayer { schema: schema_of(schema), cols, profile: LayerProfile::new(schema), rows, count: 0 });
        }
        // The file is created anew: pages left over from a larger file would remain at its end.
        let out = BufWriter::with_capacity(1 << 20, File::create(path)?);
        Ok(Some(Self { out, builder, layers }))
    }

    /// Adds a chunk of features of layer `i`.
    pub(crate) fn write(&mut self, i: usize, chunk: &Layer) -> Result<()> {
        let l = &mut self.layers[i];
        l.profile.add(&chunk.features);
        let fields = l.schema.schema.fields();
        let first = l.count as i64 + 1;
        let (cols, features) = (&l.cols, &chunk.features);
        let rows = crate::par::map_range(features.len(), |n| {
            let (rowid, values, bbox) = cols.row(&features[n], first + n as i64, fields);
            let rec = crate::drivers::vector::sqlite::record(&values);
            let mut b = Vec::with_capacity(rec.len() + 45);
            b.extend_from_slice(&rowid.to_le_bytes());
            match bbox {
                Some(bb) => {
                    b.push(1);
                    for v in [bb.min_x, bb.min_y, bb.max_x, bb.max_y] {
                        b.extend_from_slice(&v.to_le_bytes());
                    }
                }
                None => b.push(0),
            }
            b.extend_from_slice(&(rec.len() as u32).to_le_bytes());
            b.extend_from_slice(&rec);
            b
        });
        for r in rows {
            l.rows.write_all(&r)?;
        }
        l.count += features.len();
        Ok(())
    }

    /// Writes the tables, one after another, then the rest of the database.
    pub(crate) fn finish(self) -> Result<()> {
        use crate::drivers::vector::geopackage::{LayerExtent, LayerTable};
        use crate::drivers::vector::sqlite::{BuiltTable, StreamTable};
        let Self { mut out, mut builder, layers } = self;
        let mut next = 2;
        for l in layers {
            let mut rows = l.rows.into_inner().map_err(|e| Error::Io(e.into_error()))?;
            rows.seek(SeekFrom::Start(0))?;
            let mut rows = BufReader::with_capacity(1 << 20, rows);
            let mut table = StreamTable::starting_at(&mut out, next);
            let mut extent = LayerExtent::default();
            let mut head = [0u8; 9];
            let mut rec = Vec::new();
            for _ in 0..l.count {
                rows.read_exact(&mut head)?;
                let rowid = i64::from_le_bytes(head[..8].try_into().unwrap_or_default());
                let bbox = if head[8] == 1 {
                    let mut b = [0u8; 32];
                    rows.read_exact(&mut b)?;
                    let v = |k: usize| f64::from_le_bytes(b[8 * k..8 * k + 8].try_into().unwrap_or_default());
                    Some(wbvector::BBox::new(v(0), v(1), v(2), v(3)))
                } else {
                    None
                };
                let mut len = [0u8; 4];
                rows.read_exact(&mut len)?;
                rec.resize(u32::from_le_bytes(len) as usize, 0);
                rows.read_exact(&mut rec)?;
                table.push(rowid, &rec)?;
                extent.add(rowid, bbox);
            }
            let (root, after) = table.finish()?;
            next = after;
            // The R-tree's tables follow the layer's.
            let mut write = |t, rows: &mut crate::drivers::vector::sqlite::Rows| {
                let (built, after, _) = crate::drivers::vector::sqlite::stream_table(&mut out, next, t, rows)?;
                next = after;
                Ok(built)
            };
            // The schema decisions of the whole-layer writer, from the profile.
            let g = &l.profile.geometry;
            let all_none = g.count == 0;
            let declared = if all_none { l.schema.geom_type } else { l.profile.uniform_type() };
            let small = |i: usize| l.profile.fields.get(i).is_none_or(|f| f.no_int_beyond_int32);
            let cols = builder.layer_columns(&l.schema, all_none, declared, g.has_z, None, small);
            let built = BuiltTable { name: l.schema.name.clone(), sql: cols.sql.clone(), root };
            builder.finish_layer(&l.schema.name, &cols, l.count, l.count as i64, extent, LayerTable::Built(built), Some(&mut write))?;
        }
        let (page1, rest) = builder.finish_parts(next)?;
        out.seek(SeekFrom::Start(u64::from(next - 1) * page1.len() as u64))?;
        out.write_all(&rest)?;
        out.seek(SeekFrom::Start(0))?;
        out.write_all(&page1)?;
        out.flush()?;
        Ok(())
    }
}

/// Collects the features, written whole at the end.
struct WholeSink {
    layer: Layer,
    path: String,
    options: VectorWriteOptions,
}

impl FeatureSink for WholeSink {
    fn write(&mut self, chunk: &Layer) -> Result<()> {
        self.layer.features.extend(chunk.features.iter().cloned());
        Ok(())
    }

    fn write_owned(&mut self, chunk: Layer) -> Result<()> {
        self.layer.features.extend(chunk.features);
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<()> {
        crate::write_vector(std::slice::from_ref(&self.layer), &self.path, &self.options)
    }
}

struct GeoJsonSink {
    out: BufWriter<File>,
    enc: geojson::CollectionEncoder,
}

impl FeatureSink for GeoJsonSink {
    fn write(&mut self, chunk: &Layer) -> Result<()> {
        let mut text = String::new();
        self.enc.features(chunk, &mut text)?;
        self.out.write_all(text.as_bytes())?;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.out.write_all(geojson::CollectionEncoder::TAIL.as_bytes())?;
        self.out.flush()?;
        Ok(())
    }
}

struct SeqSink {
    out: BufWriter<File>,
    enc: geojson::SeqEncoder,
}

impl FeatureSink for SeqSink {
    fn write(&mut self, chunk: &Layer) -> Result<()> {
        let mut text = String::new();
        self.enc.features(chunk, &mut text)?;
        self.out.write_all(text.as_bytes())?;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

struct CsvSink {
    out: BufWriter<File>,
    enc: csv::CsvEncoder,
}

impl FeatureSink for CsvSink {
    fn write(&mut self, chunk: &Layer) -> Result<()> {
        let mut text = String::new();
        self.enc.rows(&chunk.features, &mut text);
        self.out.write_all(text.as_bytes())?;
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

struct ShpSink {
    shp: BufWriter<File>,
    shx: BufWriter<File>,
    dbf: BufWriter<File>,
    enc: shapefile::ShpEncoder,
    total: shapefile::Bounds,
    count: u64,
    shp_len: u64,
    shx_len: u64,
}

impl ShpSink {
    fn create(path: &str, layer: &Layer, profile: &LayerProfile) -> Result<Self> {
        let enc = shapefile::ShpEncoder::new(layer, profile)?;
        let base = std::path::Path::new(path).with_extension("");
        let open =
            |ext: &str| -> Result<BufWriter<File>> { Ok(BufWriter::with_capacity(1 << 20, File::create(base.with_extension(ext))?)) };
        let (mut shp, mut shx, mut dbf) = (open("shp")?, open("shx")?, open("dbf")?);
        shp.write_all(&[0u8; 100])?;
        shx.write_all(&[0u8; 100])?;
        dbf.write_all(&enc.dbf_header(0))?;
        std::fs::write(base.with_extension("cpg"), "UTF-8")?;
        let prj_path = base.with_extension("prj");
        match crate::drivers::vector::output_wkt(layer) {
            Some(wkt) => std::fs::write(prj_path, wkt)?,
            None if prj_path.exists() => std::fs::remove_file(prj_path)?,
            None => {}
        }
        Ok(Self { shp, shx, dbf, enc, total: shapefile::Bounds::empty(), count: 0, shp_len: 100, shx_len: 100 })
    }
}

impl FeatureSink for ShpSink {
    fn write(&mut self, chunk: &Layer) -> Result<()> {
        let mut dbf = Vec::new();
        for f in &chunk.features {
            let (rec, idx) = self.enc.record(self.count, f, self.shp_len, &mut self.total)?;
            self.shp.write_all(&rec)?;
            self.shx.write_all(&idx)?;
            self.shp_len += rec.len() as u64;
            self.shx_len += 8;
            self.enc.dbf_record(f, &mut dbf);
            self.count += 1;
        }
        self.dbf.write_all(&dbf)?;
        Ok(())
    }

    fn finish(self: Box<Self>) -> Result<()> {
        let s = *self;
        let patch = |w: BufWriter<File>, head: &[u8], tail: &[u8]| -> Result<()> {
            let mut f = w.into_inner().map_err(|e| e.into_error())?;
            f.write_all(tail)?;
            f.seek(SeekFrom::Start(0))?;
            f.write_all(head)?;
            f.flush()?;
            Ok(())
        };
        patch(s.shp, &s.enc.header(s.shp_len, &s.total), &[])?;
        patch(s.shx, &s.enc.header(s.shx_len, &s.total), &[])?;
        patch(s.dbf, &s.enc.dbf_header(s.count), &[0x1A])?;
        Ok(())
    }
}

/// A FlatGeobuf written in one pass over the features. The column and geometry types depend on
/// the values (32 or 64-bit integers, one geometry type or several, Z): they are decided from
/// the features seen so far, and when a chunk widens them the features already spooled are
/// re-encoded with the final types as the file is assembled, so that it is identical to the one
/// [`crate::write_vector`] writes.
#[cfg(feature = "flatgeobuf")]
struct FgbSink {
    path: String,
    tmp_path: String,
    tmp: BufWriter<File>,
    layer: Layer,
    profile: LayerProfile,
    enc: fgb::FgbEncoder,
    /// The features encoded with earlier types: where each run starts, and how to decode it. The
    /// features from `fresh` on are encoded with `enc`.
    stale: Vec<(usize, fgb::FgbHeader)>,
    fresh: usize,
    nodes: Vec<fgb::NodeItem>,
    /// Each feature's offset in the temporary file, and the end of the last.
    offsets: Vec<u64>,
}

#[cfg(feature = "flatgeobuf")]
impl FgbSink {
    fn create(path: &str, layer: &Layer) -> Result<Self> {
        let tmp_path = format!("{path}.terrio-features");
        let tmp = BufWriter::with_capacity(1 << 20, File::create(&tmp_path)?);
        let profile = LayerProfile::new(layer);
        Ok(Self {
            path: path.to_string(),
            tmp_path,
            tmp,
            layer: layer.clone(),
            enc: fgb::FgbEncoder::new(layer, &profile),
            profile,
            stale: Vec::new(),
            fresh: 0,
            nodes: Vec::new(),
            offsets: vec![0],
        })
    }

    /// Re-encodes the features encoded with earlier types into a new temporary file, followed
    /// by the others as they are.
    fn reencode_stale(&mut self) -> Result<()> {
        const BATCH: usize = 65536;
        let new_path = format!("{}.2", self.tmp_path);
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&new_path)?);
        let mut tmp = BufReader::with_capacity(1 << 20, File::open(&self.tmp_path)?);
        let mut offsets = vec![0u64];
        let mut offset = 0u64;
        for (run, (start, header)) in self.stale.iter().enumerate() {
            let end = self.stale.get(run + 1).map_or(self.fresh, |s| s.0);
            for first in (*start..end).step_by(BATCH) {
                let last = (first + BATCH).min(end);
                let mut raw = Vec::with_capacity(last - first);
                for i in first..last {
                    let mut buf = vec![0; (self.offsets[i + 1] - self.offsets[i]) as usize];
                    tmp.read_exact(&mut buf)?;
                    raw.push(buf);
                }
                let features: Result<Vec<Feature>> =
                    crate::par::map_range(raw.len(), |i| fgb::decode_feature_exact(&raw[i], header, (first + i) as u64))
                        .into_iter()
                        .collect();
                for (bytes, _) in self.enc.encode(&features?, first as u64) {
                    out.write_all(&bytes)?;
                    offset += bytes.len() as u64;
                    offsets.push(offset);
                }
            }
        }
        // The rest is copied as it is, shifted.
        let shift = |o: u64| o - self.offsets[self.fresh] + offset;
        offsets.extend(self.offsets[self.fresh + 1..].iter().map(|o| shift(*o)));
        std::io::copy(&mut tmp, &mut out)?;
        out.flush()?;
        drop((out, tmp));
        std::fs::rename(&new_path, &self.tmp_path)?;
        self.offsets = offsets;
        self.stale.clear();
        self.fresh = 0;
        Ok(())
    }

    fn assemble(&mut self) -> Result<()> {
        self.tmp.flush()?;
        if !self.stale.is_empty() {
            self.reencode_stale()?;
        }
        let index = !self.profile.geometry.missing_bbox && !self.nodes.is_empty();
        let extent = fgb::calc_extent(&self.nodes);
        let mut out = BufWriter::with_capacity(1 << 20, File::create(&self.path)?);
        out.write_all(&self.enc.header(&self.layer, self.nodes.len() as u64, &extent, index))?;
        let mut tmp = File::open(&self.tmp_path)?;
        if index {
            let offsets = &self.offsets;
            let (tree, order) = fgb::sorted_index(&mut self.nodes, &extent, |i| offsets[i + 1] - offsets[i])?;
            tree.stream_write(&mut out)?;
            drop(tree);
            self.nodes = Vec::new();
            let mut buf = Vec::new();
            for i in order {
                let (off, end) = (offsets[i as usize], offsets[i as usize + 1]);
                buf.resize((end - off) as usize, 0);
                tmp.seek(SeekFrom::Start(off))?;
                tmp.read_exact(&mut buf)?;
                out.write_all(&buf)?;
            }
        } else {
            std::io::copy(&mut tmp, &mut out)?;
        }
        out.flush()?;
        Ok(())
    }
}

#[cfg(feature = "flatgeobuf")]
impl FeatureSink for FgbSink {
    fn write(&mut self, chunk: &Layer) -> Result<()> {
        self.profile.add(&chunk.features);
        let enc = fgb::FgbEncoder::new(&self.layer, &self.profile);
        if !enc.same_encoding(&self.enc) {
            let n = self.nodes.len();
            if n > self.fresh {
                self.stale.push((self.fresh, self.enc.decoder()));
                self.fresh = n;
            }
            self.enc = enc;
        }
        let mut offset = *self.offsets.last().unwrap_or(&0);
        for (bytes, node) in self.enc.encode(&chunk.features, self.nodes.len() as u64) {
            self.tmp.write_all(&bytes)?;
            offset += bytes.len() as u64;
            self.offsets.push(offset);
            self.nodes.push(node);
        }
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> Result<()> {
        let result = self.assemble();
        let _ = std::fs::remove_file(&self.tmp_path);
        result
    }
}

#[cfg(feature = "flatgeobuf")]
impl Drop for FgbSink {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.tmp_path);
        let _ = std::fs::remove_file(format!("{}.2", self.tmp_path));
    }
}

/// Record batches are built as chunks come, and encoded on a thread of their own (in WebAssembly,
/// right away), so that reading goes on while row groups are encoded.
#[cfg(feature = "parquet")]
struct ParquetSink {
    enc: geoparquet::ParquetEncoder,
    #[cfg(not(target_family = "wasm"))]
    encoder: Option<(std::sync::mpsc::SyncSender<geoparquet::EncodedBatch>, std::thread::JoinHandle<Result<()>>)>,
    /// Set when the sink is dropped unfinished: the file is left without its footer.
    #[cfg(not(target_family = "wasm"))]
    abort: std::sync::Arc<std::sync::atomic::AtomicBool>,
    #[cfg(target_family = "wasm")]
    writer: geoparquet::ParquetFileWriter<BufWriter<File>>,
}

#[cfg(feature = "parquet")]
impl ParquetSink {
    fn new(mut writer: geoparquet::ParquetFileWriter<BufWriter<File>>, enc: geoparquet::ParquetEncoder) -> Self {
        #[cfg(not(target_family = "wasm"))]
        {
            // Two batches in flight at most.
            let (tx, rx) = std::sync::mpsc::sync_channel::<geoparquet::EncodedBatch>(2);
            let abort = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let aborted = abort.clone();
            let handle = std::thread::spawn(move || {
                for batch in rx {
                    // Dropped unfinished: the rest is not encoded.
                    if aborted.load(std::sync::atomic::Ordering::SeqCst) {
                        break;
                    }
                    writer.push(batch)?;
                }
                if !aborted.load(std::sync::atomic::Ordering::SeqCst) {
                    writer.close()?.flush()?;
                }
                Ok(())
            });
            Self { enc, encoder: Some((tx, handle)), abort }
        }
        #[cfg(target_family = "wasm")]
        {
            let _ = &mut writer;
            Self { enc, writer }
        }
    }
}

#[cfg(feature = "parquet")]
impl FeatureSink for ParquetSink {
    fn write(&mut self, chunk: &Layer) -> Result<()> {
        #[cfg(not(target_family = "wasm"))]
        {
            let Some((tx, _)) = &self.encoder else { return Ok(()) };
            for batch in self.enc.batches(&chunk.features)? {
                if tx.send(batch).is_err() {
                    // The encoder stopped on an error, which finishing reports.
                    return self.stop();
                }
            }
            Ok(())
        }
        #[cfg(target_family = "wasm")]
        {
            self.enc.write(&mut self.writer, &chunk.features)
        }
    }

    fn finish(self: Box<Self>) -> Result<()> {
        #[cfg(not(target_family = "wasm"))]
        {
            let mut s = self;
            s.stop()
        }
        #[cfg(target_family = "wasm")]
        {
            self.writer.close()?.flush()?;
            Ok(())
        }
    }
}

#[cfg(all(feature = "parquet", not(target_family = "wasm")))]
impl ParquetSink {
    /// Ends the encoder thread: the file is complete, or its error.
    fn stop(&mut self) -> Result<()> {
        let Some((tx, handle)) = self.encoder.take() else { return Ok(()) };
        drop(tx);
        handle.join().map_err(|_| Error::InvalidData("Parquet: the encoder thread panicked".into()))?
    }
}

#[cfg(all(feature = "parquet", not(target_family = "wasm")))]
impl Drop for ParquetSink {
    /// An unfinished sink waits for its encoder, which leaves the file without a footer.
    fn drop(&mut self) {
        self.abort.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.stop();
    }
}

// Arrow

/// A layer read as a stream of Arrow record batches (an `arrow_array::RecordBatchReader`), for
/// Arrow consumers: the C stream interface, IPC streams, Parquet writers.
#[cfg(feature = "parquet")]
#[cfg_attr(docsrs, doc(cfg(feature = "parquet")))]
pub struct ArrowStream {
    reader: FeatureReader,
    enc: crate::arrow::ArrowEncoder,
    batch: usize,
    started: bool,
}

#[cfg(feature = "parquet")]
impl ArrowStream {
    /// Encodes the features of `reader`. When the schema depends on the values
    /// ([`crate::arrow::ArrowEncoder::needs_profile`]), `profile` must describe all the features
    /// the reader will give.
    pub fn new(reader: FeatureReader, profile: Option<&LayerProfile>, options: &crate::arrow::ArrowWriteOptions) -> Result<Self> {
        let empty = LayerProfile::new(reader.layer());
        let enc = crate::arrow::ArrowEncoder::new(reader.layer(), profile.unwrap_or(&empty), options)?;
        Ok(Self { reader, enc, batch: options.batch_size.max(1), started: false })
    }

    /// Opens the layer `layer` (or the first) of a dataset as record batches, reading it twice
    /// when the schema depends on the values (once for the profile).
    pub fn open(path: &str, layer: Option<&str>, options: &VectorOpenOptions, arrow: &crate::arrow::ArrowWriteOptions) -> Result<Self> {
        let pick = |mut readers: Vec<FeatureReader>| -> Result<FeatureReader> {
            let i = match layer {
                Some(name) => readers
                    .iter()
                    .position(|r| r.layer().name.eq_ignore_ascii_case(name))
                    .ok_or_else(|| Error::InvalidArgument(format!("no layer named {name}")))?,
                None if readers.is_empty() => return Err(Error::InvalidArgument("the dataset has no vector layers".into())),
                None => 0,
            };
            Ok(readers.swap_remove(i))
        };
        let batch = arrow.batch_size.max(1);
        let reader = pick(open_layers(path, options, batch)?)?;
        let profile = if crate::arrow::ArrowEncoder::needs_profile(reader.layer(), arrow) {
            let mut first = pick(open_layers(path, options, batch)?)?;
            let mut p = LayerProfile::new(first.layer());
            while let Some(c) = first.read_chunk(batch)? {
                p.add(&c.features);
            }
            Some(p)
        } else {
            None
        };
        Self::new(reader, profile.as_ref(), arrow)
    }

    /// The layer, without its features.
    pub fn layer(&self) -> &Layer {
        self.reader.layer()
    }

    /// The schema of the record batches.
    pub fn schema(&self) -> arrow_schema::SchemaRef {
        self.enc.schema()
    }

    /// The next record batch; `None` at the end. A layer without features gives one empty batch.
    pub fn next_batch(&mut self) -> Result<Option<arrow_array::RecordBatch>> {
        match self.reader.read_chunk(self.batch)? {
            Some(c) => {
                self.started = true;
                self.enc.encode(&c.features).map(Some)
            }
            None if !self.started => {
                self.started = true;
                self.enc.encode(&[]).map(Some)
            }
            None => Ok(None),
        }
    }
}

#[cfg(feature = "parquet")]
impl Iterator for ArrowStream {
    type Item = std::result::Result<arrow_array::RecordBatch, arrow_schema::ArrowError>;

    fn next(&mut self) -> Option<Self::Item> {
        self.next_batch().map_err(|e| arrow_schema::ArrowError::ExternalError(Box::new(e))).transpose()
    }
}

#[cfg(feature = "parquet")]
impl arrow_array::RecordBatchReader for ArrowStream {
    fn schema(&self) -> arrow_schema::SchemaRef {
        self.enc.schema()
    }
}

// Conversion

/// Options for [`convert`].
#[derive(Debug, Clone)]
pub struct ConvertOptions {
    /// How to open the input.
    pub open: VectorOpenOptions,
    /// The `ogr2ogr`-style options (`--sql` is not available on streams).
    pub translate: VectorTranslateOptions,
    /// How to write the output.
    pub write: VectorWriteOptions,
    /// The number of features read, translated and written at a time (fewer when they take
    /// more than [`CHUNK_BYTES`]).
    pub chunk_size: usize,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        Self {
            open: VectorOpenOptions::default(),
            translate: VectorTranslateOptions::default(),
            write: VectorWriteOptions::default(),
            chunk_size: DEFAULT_CHUNK,
        }
    }
}

/// Whether [`convert`] streams a conversion from `src` to `dst`, rather than reading the layers
/// whole: both formats are read and written as streams, and no option needs the whole layer.
/// OpenStreetMap PBF files (`.pbf`) stream when their header says they are sorted by type then id,
/// as most are; files that turn out not to be are then converted whole.
pub fn can_convert_stream(src: &str, dst: &str, options: &ConvertOptions) -> bool {
    let out = options.write.driver.or_else(|| VectorDriver::for_extension(dst));
    let ext = crate::formats::extension(src);
    let input = VectorDriver::for_extension(src).filter(|_| ext.as_deref() != Some("json"));
    let pbf = || input == Some(VectorDriver::OSM) && ext.as_deref() == Some("pbf") && osm_sorted(src);
    options.translate.sql.is_none()
        && out.is_some_and(can_write_stream)
        && (input.is_some_and(can_read_stream) || pbf())
        && crate::vfs::Location::parse(src).is_local()
        && !same_dataset(src, dst)
}

/// Whether `src` is an OpenStreetMap PBF file sorted by type then id.
fn osm_sorted(src: &str) -> bool {
    #[cfg(all(feature = "osm", not(target_family = "wasm")))]
    return File::open(src).is_ok_and(|mut f| crate::drivers::vector::osm::is_sorted_pbf(&mut f));
    #[cfg(not(all(feature = "osm", not(target_family = "wasm"))))]
    {
        let _ = src;
        false
    }
}

/// Whether writing `dst` would overwrite `src` (the same file, or the same shapefile through
/// another of its files): such conversions read the input whole before writing.
fn same_dataset(src: &str, dst: &str) -> bool {
    let key = |p: &str| {
        let p = std::path::Path::new(p);
        let dir = p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
        let shp = matches!(VectorDriver::for_extension(&p.to_string_lossy()), Some(VectorDriver::Shapefile));
        let name = if shp { p.file_stem() } else { p.file_name() };
        Some((std::fs::canonicalize(dir).ok()?, name?.to_os_string(), shp))
    };
    // The same file under another name (a symbolic link, say).
    if let (Ok(a), Ok(b)) = (std::fs::canonicalize(src), std::fs::canonicalize(dst))
        && a == b
    {
        return true;
    }
    match (key(src), key(dst)) {
        (Some((a, x, s)), Some((b, y, t))) => a == b && x == y && s == t,
        _ => false,
    }
}

/// Converts a vector dataset, as `terrio convert` does, a chunk of features at a time when
/// [`can_convert_stream`] says so (and otherwise reading the layers whole). Formats whose writers
/// [`needs_profile`] read the input twice: once for the profile, then to write. Several layers
/// written to a GeoPackage go one after another, and OpenStreetMap PBF files sorted by type then
/// id (as osmium, Geofabrik and the planet write them) are read a block at a time.
pub fn convert(src: &str, dst: &str, options: &ConvertOptions) -> Result<()> {
    if let Some(done) = convert_osm(src, dst, options) {
        return done;
    }
    if !can_convert_stream(src, dst, options) || VectorDriver::for_extension(src) == Some(VectorDriver::OSM) {
        return convert_whole(src, dst, options);
    }
    let driver = output_driver(dst, options);
    let chunk = options.chunk_size.max(1);
    let mut readers = open_layers(src, &options.open, chunk)?;
    if options.translate.layer_name.is_some() && readers.len() > 1 {
        return Err(Error::InvalidArgument("a new layer name needs a single input layer: choose one".into()));
    }
    match readers.len() {
        0 => return Err(Error::InvalidArgument("no layers to write".into())),
        1 => {}
        #[cfg(feature = "sqlite")]
        _ if driver == VectorDriver::GPKG => {
            for r in &mut readers {
                r.set_chunk_bytes(AHEAD_CHUNK_BYTES);
            }
            let mut translators = readers.iter().map(|r| Translator::new(r.layer(), &options.translate)).collect::<Result<Vec<_>>>()?;
            let schemas: Vec<Layer> = translators.iter().map(|t| t.schema().clone()).collect();
            let Some(mut sink) = GpkgLayersSink::create(dst, &schemas)? else {
                drop(readers);
                return convert_whole(src, dst, options);
            };
            let written = (|| {
                for (k, (reader, translator)) in readers.iter_mut().zip(&mut translators).enumerate() {
                    let next =
                        || if translator.is_done() { Ok(None) } else { reader.read_chunk(chunk)?.map(|c| translator.apply(c)).transpose() };
                    write_ahead(next, |c| sink.write(k, &c))?;
                }
                sink.finish()
            })();
            return remove_on_error(dst, driver, written);
        }
        _ => return Err(one_layer_only(driver)),
    }
    let ahead = matches!(driver, VectorDriver::GPKG | VectorDriver::FlatGeobuf);
    let chunk_bytes = if ahead { AHEAD_CHUNK_BYTES } else { CHUNK_BYTES };
    let mut reader = readers.remove(0);
    reader.set_chunk_bytes(chunk_bytes);
    let mut translator = Translator::new(reader.layer(), &options.translate)?;
    let profile = if needs_profile(driver, translator.schema()) {
        let mut first = open_layers(src, &options.open, chunk)?.remove(0);
        first.set_chunk_bytes(chunk_bytes);
        let mut t = Translator::new(first.layer(), &options.translate)?;
        let mut p = LayerProfile::new(t.schema());
        while !t.is_done()
            && let Some(c) = first.read_chunk(chunk)?
        {
            p.add(&t.apply(c)?.features);
        }
        Some(p)
    } else {
        None
    };
    let mut writer = FeatureWriter::create(dst, translator.schema(), profile.as_ref(), &options.write)?;
    let written = (|| {
        let mut next = || if translator.is_done() { Ok(None) } else { reader.read_chunk(chunk)?.map(|c| translator.apply(c)).transpose() };
        if ahead {
            write_ahead(next, |c| writer.write_owned(c))?;
        } else {
            while let Some(c) = next()? {
                writer.write_owned(c)?;
            }
        }
        writer.finish()
    })();
    remove_on_error(dst, driver, written)
}

/// Writes the chunks `next` gives with `write` until `next` gives `None`: the next chunk is read
/// on another thread while one is written (in turn on WebAssembly), so that writing, often
/// serial, does not keep the threads reading idle. At most one chunk waits to be written.
fn write_ahead(mut next: impl FnMut() -> Result<Option<Layer>> + Send, mut write: impl FnMut(Layer) -> Result<()>) -> Result<()> {
    #[cfg(target_family = "wasm")]
    {
        while let Some(c) = next()? {
            write(c)?;
        }
        Ok(())
    }
    #[cfg(not(target_family = "wasm"))]
    std::thread::scope(|s| {
        let (tx, rx) = std::sync::mpsc::sync_channel(0);
        s.spawn(move || {
            loop {
                let c = next();
                let end = !matches!(c, Ok(Some(_)));
                // The receiver is gone when writing failed.
                if tx.send(c).is_err() || end {
                    return;
                }
            }
        });
        loop {
            match rx.recv().map_err(|_| Error::InvalidData("the chunk reader stopped".into()))?? {
                Some(c) => write(c)?,
                None => return Ok(()),
            }
        }
    })
}

fn convert_whole(src: &str, dst: &str, options: &ConvertOptions) -> Result<()> {
    let ds = crate::open_vector(src, &options.open)?;
    let layers = crate::ops::vector_filter::translate_owned(ds.layers, &options.translate)?;
    crate::write_vector(&layers, dst, &options.write)
}

fn output_driver(dst: &str, options: &ConvertOptions) -> VectorDriver {
    options.write.driver.or_else(|| VectorDriver::for_extension(dst)).unwrap_or(VectorDriver::GeoJSON)
}

fn one_layer_only(driver: VectorDriver) -> Error {
    Error::InvalidArgument(format!("the {} format holds one layer: choose a layer, or write a GeoPackage", driver.name()))
}

/// A conversion that fails partway leaves no truncated output behind (one that fails before the
/// output is created leaves what was there).
fn remove_on_error(dst: &str, driver: VectorDriver, written: Result<()>) -> Result<()> {
    if written.is_err() {
        let path = std::path::Path::new(dst);
        let _ = std::fs::remove_file(path);
        if driver == VectorDriver::Shapefile {
            for ext in ["shp", "shx", "dbf", "prj", "cpg"] {
                let _ = std::fs::remove_file(path.with_extension(ext));
            }
        }
    }
    written
}

/// Converts an OpenStreetMap PBF file sorted by type then id a block at a time (see
/// [`read_sorted_pbf`](crate::drivers::vector::osm::read_sorted_pbf)). `None` for other inputs
/// and outputs, which are converted whole.
fn convert_osm(src: &str, dst: &str, options: &ConvertOptions) -> Option<Result<()>> {
    #[cfg(not(all(feature = "osm", not(target_family = "wasm"))))]
    {
        let _ = (src, dst, options);
        None
    }
    #[cfg(all(feature = "osm", not(target_family = "wasm")))]
    {
        let local = crate::vfs::Location::parse(src).is_local();
        if options.translate.sql.is_some()
            || VectorDriver::for_extension(src) != Some(VectorDriver::OSM)
            || !local
            || !can_write_stream(output_driver(dst, options))
            || same_dataset(src, dst)
        {
            return None;
        }
        let mut file = match File::open(src) {
            Ok(f) => f,
            Err(e) => return Some(Err(e.into())),
        };
        // Other files (unsorted, or not OpenStreetMap: vector tiles are `.pbf` files too) are
        // converted whole, as are files not sorted as their header says (the partial output
        // removed).
        if !crate::drivers::vector::osm::is_sorted_pbf(&mut file) {
            return None;
        }
        match convert_sorted_osm(&mut file, src, dst, options) {
            Err(e) if crate::drivers::vector::osm::is_unsorted(&e) => None,
            r => Some(r),
        }
    }
}

#[cfg(all(feature = "osm", not(target_family = "wasm")))]
#[cfg_attr(not(feature = "sqlite"), allow(unused_variables))]
fn convert_sorted_osm(data: &mut File, src: &str, dst: &str, options: &ConvertOptions) -> Result<()> {
    use crate::drivers::vector::osm;
    let driver = output_driver(dst, options);
    let schemas = osm::layer_schemas();
    // The layers asked for, as `open_vector` selects them.
    let selected: Vec<usize> = if options.open.layers.is_empty() {
        (0..schemas.len()).collect()
    } else {
        let s: Vec<usize> = (0..schemas.len()).filter(|&i| options.open.layers.contains(&schemas[i].name)).collect();
        if s.is_empty() {
            let names: Vec<&str> = schemas.iter().map(|l| l.name.as_str()).collect();
            return Err(Error::InvalidArgument(format!(
                "no layer named {} (layers: {})",
                options.open.layers.join(", "),
                names.join(", ")
            )));
        }
        s
    };
    let mut wanted = [false; 5];
    for &i in &selected {
        wanted[i] = true;
    }
    let chunk_of = |l: usize, features: Vec<Feature>| {
        let mut c = schema_of(&schemas[l]);
        c.features = features;
        c
    };
    let mut translators = selected.iter().map(|&i| Translator::new(&schemas[i], &options.translate)).collect::<Result<Vec<_>>>()?;
    #[cfg(feature = "sqlite")]
    if driver == VectorDriver::GPKG {
        let out: Vec<Layer> = translators.iter().map(|t| t.schema().clone()).collect();
        let Some(mut sink) = GpkgLayersSink::create(dst, &out)? else { return convert_whole(src, dst, options) };
        let written = (|| {
            osm::read_sorted_pbf(data, wanted, &mut |l, features| {
                let Some(k) = selected.iter().position(|&s| s == l) else { return Ok(()) };
                if translators[k].is_done() {
                    return Ok(());
                }
                let c = translators[k].apply(chunk_of(l, features))?;
                sink.write(k, &c)
            })?;
            sink.finish()
        })();
        return remove_on_error(dst, driver, written);
    }
    let [l] = selected[..] else { return Err(one_layer_only(driver)) };
    let profile = if needs_profile(driver, translators[0].schema()) {
        let mut t = Translator::new(&schemas[l], &options.translate)?;
        let mut p = LayerProfile::new(t.schema());
        osm::read_sorted_pbf(data, wanted, &mut |_, features| {
            if !t.is_done() {
                p.add(&t.apply(chunk_of(l, features))?.features);
            }
            Ok(())
        })?;
        Some(p)
    } else {
        None
    };
    let translator = &mut translators[0];
    let mut writer = FeatureWriter::create(dst, translator.schema(), profile.as_ref(), &options.write)?;
    let written = (|| {
        osm::read_sorted_pbf(data, wanted, &mut |_, features| {
            if translator.is_done() {
                return Ok(());
            }
            writer.write_owned(translator.apply(chunk_of(l, features))?)
        })?;
        writer.finish()
    })();
    remove_on_error(dst, driver, written)
}

#[cfg(test)]
mod tests;

// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/gti/gdaltileindexdataset.cpp
// No code is translated from these sources.

//! Indexed raster catalogs (GDAL GTI): lazy, spatially selected mosaics.
//!
//! GeoPackage and FlatGeobuf indexes are queried through their spatial indexes for every
//! output window. Source datasets are opened only when selected, in a bounded LRU cache.
use crate::cog::{LazyRaster, Window, WindowOptions, WindowSource};
use crate::crs::SpatialRef;
use crate::ops::warp::{Nodata, WarpOptions};
use crate::ops::{Resampling, apply_gt, invert_gt};
use crate::{Error, RasterDataset, Result, VectorOpenOptions};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use wbraster::raster::RasterData;
use wbraster::{DataType, Raster, RasterConfig};
use wbvector::{FieldValue, Layer};

// At most 1 MiB retained after a read; larger coverage results belong only to the caller.
const MASK_CACHE_PIXELS: usize = 1024 * 1024;

/// Options of a raster catalog. Missing output characteristics are inferred from the index
/// and its first tile. Explicit options override GTI XML and layer metadata.
#[derive(Debug, Clone, Default)]
pub struct CatalogOptions {
    /// The tile index layer; required when the index contains several layers.
    pub layer: Option<String>,
    /// The string field naming source tiles (default `location`).
    pub location_field: Option<String>,
    /// The field controlling compositing order; higher values are rendered last by default.
    pub sort_field: Option<String>,
    /// Reverse the default ascending sort order.
    pub sort_descending: Option<bool>,
    /// Output resolution, in the index CRS.
    pub resolution: Option<(f64, f64)>,
    /// Output extent, in the index CRS.
    pub extent: Option<(f64, f64, f64, f64)>,
    /// Output CRS. Must match the index CRS; sources may use different CRSs.
    pub srs: Option<SpatialRef>,
    /// Number of output bands.
    pub bands: Option<usize>,
    /// Output type: inferred with required tile metadata, otherwise Byte.
    pub data_type: Option<DataType>,
    /// Output nodata (zero fill without one).
    pub nodata: Option<f64>,
    /// Alpha band, numbered from 1, when output includes one. Zero alpha masks the source.
    pub alpha_band: Option<usize>,
    /// Maximum open source datasets (default 32, must be positive).
    pub cache_size: Option<usize>,
    /// Resampling for mixed grids and CRSs (default nearest).
    pub resampling: Option<Resampling>,
}

/// Cumulative logical I/O counters, useful for checking small-window access and cache bounds.
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct CatalogStats {
    /// Persistent index query readers opened (zero for an in-memory index).
    pub index_opens: usize,
    /// Spatial selection requests.
    pub index_queries: usize,
    /// Index features returned by spatial queries.
    pub selected_features: usize,
    /// Source dataset opens (including reopens after eviction).
    pub source_opens: usize,
    /// Cache hits.
    pub cache_hits: usize,
    /// Data/alpha sample values requested before reprojection; separate mask images are excluded.
    pub source_pixels: usize,
    /// Destination sample values warped, after clipping each tile to the output window.
    pub warped_pixels: usize,
    /// Largest number of simultaneously cached source datasets.
    pub peak_cached_sources: usize,
}

struct Entry {
    location: String,
    order: FieldValue,
    fid: u64,
}
#[derive(Clone)]
struct IndexCell {
    feature: usize,
    envelope: rstar::AABB<[f64; 2]>,
}
impl rstar::RTreeObject for IndexCell {
    type Envelope = rstar::AABB<[f64; 2]>;
    fn envelope(&self) -> Self::Envelope {
        self.envelope
    }
}
trait IndexReader: Send + Sync {
    fn first(&mut self) -> Result<Layer>;
    fn query(&mut self, bounds: Option<Bounds>) -> Result<Layer>;
}
#[cfg(feature = "sqlite")]
impl IndexReader for crate::drivers::vector::gpkg_read::IndexedLayer {
    fn first(&mut self) -> Result<Layer> {
        crate::drivers::vector::gpkg_read::IndexedLayer::first(self)
    }
    fn query(&mut self, bounds: Option<Bounds>) -> Result<Layer> {
        crate::drivers::vector::gpkg_read::IndexedLayer::query(self, bounds)
    }
}
#[cfg(feature = "flatgeobuf")]
struct FgbIndex {
    file: std::fs::File,
    prepared: crate::drivers::vector::flatgeobuf::PreparedIndex,
}
#[cfg(feature = "flatgeobuf")]
impl IndexReader for FgbIndex {
    fn first(&mut self) -> Result<Layer> {
        self.prepared.first(&mut self.file)
    }
    fn query(&mut self, bounds: Option<Bounds>) -> Result<Layer> {
        self.prepared.query(&mut self.file, bounds)
    }
}
enum Index {
    File { path: String, layer: String, reader: Box<dyn IndexReader> },
    Memory { layer: Box<Layer>, tree: rstar::RTree<IndexCell> },
}
/// A named catalog source retained without materializing its pixels.
/// Paths are opened only when selected and closed on cache eviction. Supplied raster handles
/// retain their existing reader; encoded bytes are opened on first selection.
pub struct CatalogInput(Input);
enum Input {
    Path(String),
    Raster(LazyRaster),
    Bytes(Vec<u8>, String),
    Failed(String),
}
impl CatalogInput {
    /// Defers opening a native path until spatial selection.
    pub fn path(path: impl Into<String>) -> Self {
        Self(Input::Path(path.into()))
    }
    /// Takes an existing raster handle, preserving its lazy window and overview reads.
    pub fn raster(raster: LazyRaster) -> Self {
        Self(Input::Raster(raster))
    }
    /// Defers decoding/opening an encoded file until spatial selection.
    pub fn bytes(bytes: Vec<u8>, name: impl Into<String>) -> Self {
        Self(Input::Bytes(bytes, name.into()))
    }
    fn open(&mut self) -> Result<LazyRaster> {
        if let Input::Bytes(bytes, name) = &mut self.0 {
            let opened = virtual_bytes(bytes, name).and_then(|reader| match reader {
                Some(reader) => Ok(reader),
                None => crate::memory::open_raster_lazy_bytes(std::mem::take(bytes), name),
            });
            self.0 = match opened {
                Ok(reader) => Input::Raster(reader),
                Err(error) => Input::Failed(error.to_string()),
            };
        }
        match &mut self.0 {
            Input::Path(path) => {
                if is_catalog(path) {
                    return Err(Error::Unsupported("nested GTI source catalogs are not supported".into()));
                }
                LazyRaster::open(path)
            }
            Input::Raster(reader) => Ok(reader.shared()),
            Input::Failed(message) => Err(bad(message)),
            Input::Bytes(..) => unreachable!(),
        }
    }
    fn inputs(&self) -> Result<Vec<String>> {
        match &self.0 {
            Input::Path(path) => crate::ops::vrt::source_input_paths(path),
            Input::Raster(reader) => reader.input_paths(),
            Input::Bytes(bytes, name) => virtual_bytes(bytes, name)?.map_or_else(|| Ok(Vec::new()), |reader| reader.input_paths()),
            Input::Failed(message) => Err(bad(message)),
        }
    }
}
// Unlike ordinary encoded tiles, a VRT may still read physical references after open.
// Both opening and overwrite inventory must resolve them against the same filename.
fn virtual_bytes(bytes: &[u8], name: &str) -> Result<Option<LazyRaster>> {
    let text = std::str::from_utf8(bytes);
    let vrt = crate::formats::extension(name).as_deref() == Some("vrt") || text.is_ok_and(|s| s.trim_start().starts_with("<VRTDataset"));
    if !vrt {
        return Ok(None);
    }
    let text = text.map_err(|_| bad("invalid VRT XML encoding"))?;
    crate::ops::vrt::VirtualRaster::from_xml(text)?.into_lazy(Path::new(name).parent().unwrap_or(Path::new(".")), HashMap::new()).map(Some)
}
/// A catalog whose pixels are assembled on demand. Local catalogs use the index's on-disk
/// spatial index. [`RasterCatalog::from_layer`] supports in-memory indexes and sources in WASM.
pub struct RasterCatalog {
    index: Index,
    schema: Layer,
    base: PathBuf,
    options: CatalogOptions,
    header: RasterDataset,
    cache: VecDeque<(String, LazyRaster, Option<usize>)>,
    memory: Option<HashMap<String, CatalogInput>>,
    stats: CatalogStats,
    last_mask: Option<(Window, Vec<u8>)>,
    input_paths: Option<Vec<String>>,
}
fn bad(message: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("GTI: {message}"))
}
fn extent(layer: &Layer) -> Option<(f64, f64, f64, f64)> {
    let b = layer.extent.clone().or_else(|| {
        layer.features.iter().filter_map(|f| f.geometry.as_ref()?.bbox()).reduce(|a, b| wbvector::BBox {
            min_x: a.min_x.min(b.min_x),
            min_y: a.min_y.min(b.min_y),
            max_x: a.max_x.max(b.max_x),
            max_y: a.max_y.max(b.max_y),
        })
    })?;
    Some((b.min_x, b.min_y, b.max_x, b.max_y))
}
fn field_cmp(a: &FieldValue, b: &FieldValue) -> std::cmp::Ordering {
    match (a, b) {
        (FieldValue::Integer(a), FieldValue::Integer(b)) => a.cmp(b),
        (FieldValue::Null, FieldValue::Null) => std::cmp::Ordering::Equal,
        (FieldValue::Null, _) => std::cmp::Ordering::Less,
        (_, FieldValue::Null) => std::cmp::Ordering::Greater,
        _ => match (a.as_f64(), b.as_f64()) {
            (Some(a), Some(b)) => a.total_cmp(&b),
            _ => a.as_str().unwrap_or_default().cmp(b.as_str().unwrap_or_default()),
        },
    }
}
impl RasterCatalog {
    /// Opens a GTI XML descriptor or GeoPackage/FlatGeobuf index. Metadata inference opens
    /// at most one tile; GeoTIFF/COG pixels remain lazy. Other formats follow their readers.
    pub fn open(path: &str, mut options: CatalogOptions) -> Result<Self> {
        let path = if path.get(..4).is_some_and(|p| p.eq_ignore_ascii_case("GTI:")) { &path[4..] } else { path };
        let descriptor = Path::new(path);
        let mut index_path = path.to_string();
        if descriptor.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("gti")) {
            let root = crate::drivers::vector::xml::parse(&std::fs::read_to_string(descriptor)?)?;
            if root.name != "GDALTileIndexDataset" {
                return Err(bad("expected GDALTileIndexDataset"));
            }
            let index = root.child_text("IndexDataset").ok_or_else(|| bad("IndexDataset is required"))?;
            index_path = if Path::new(index).is_absolute() {
                index.into()
            } else {
                descriptor.parent().unwrap_or(Path::new(".")).join(index).to_string_lossy().into_owned()
            };
            apply_xml(&root, &mut options)?;
        }
        let (reader, schema, bounds, metadata) = index_header(&index_path, options.layer.as_deref())?;
        apply_metadata(&metadata, &mut options)?;
        let layer = schema.name.clone();
        let base = Path::new(&index_path).parent().unwrap_or(Path::new(".")).to_path_buf();
        Self::initialize(Index::File { path: index_path, layer, reader }, schema, bounds, base, options, None)
    }
    /// Opens an index layer and named source rasters already held in memory, including WASM.
    pub fn from_layer(layer: Layer, sources: HashMap<String, RasterDataset>, options: CatalogOptions) -> Result<Self> {
        Self::from_sources(layer, sources.into_iter().map(|(name, ds)| (name, CatalogInput::raster(ds.into()))).collect(), options)
    }
    /// Opens supplied lazy sources without decoding their pixels.
    pub fn from_lazy_layer(layer: Layer, sources: HashMap<String, LazyRaster>, options: CatalogOptions) -> Result<Self> {
        Self::from_sources(layer, sources.into_iter().map(|(name, raster)| (name, CatalogInput::raster(raster))).collect(), options)
    }
    /// Opens an in-memory index with named lazy readers, deferred paths or encoded files.
    /// Sources outside the spatial selection are never opened or decoded.
    pub fn from_sources(layer: Layer, sources: HashMap<String, CatalogInput>, options: CatalogOptions) -> Result<Self> {
        let bounds = extent(&layer);
        let schema = crate::stream::schema_of(&layer);
        let tree = rstar::RTree::bulk_load(
            layer
                .features
                .iter()
                .enumerate()
                .filter_map(|(feature, f)| {
                    let b = f.geometry.as_ref()?.bbox()?;
                    [b.min_x, b.min_y, b.max_x, b.max_y]
                        .iter()
                        .all(|v| v.is_finite())
                        .then(|| IndexCell { feature, envelope: rstar::AABB::from_corners([b.min_x, b.min_y], [b.max_x, b.max_y]) })
                })
                .collect(),
        );
        Self::initialize(Index::Memory { layer: Box::new(layer), tree }, schema, bounds, PathBuf::new(), options, Some(sources))
    }
    fn initialize(
        index: Index,
        schema: Layer,
        bounds: Option<(f64, f64, f64, f64)>,
        base: PathBuf,
        options: CatalogOptions,
        memory: Option<HashMap<String, CatalogInput>>,
    ) -> Result<Self> {
        if options.cache_size == Some(0) {
            return Err(Error::InvalidArgument("GTI cache_size must be positive".into()));
        }
        let location = options.location_field.as_deref().unwrap_or("location");
        if schema.schema.field_index(location).is_none() {
            return Err(bad(format!("no location field '{location}'")));
        }
        if let Some(sort) = &options.sort_field
            && schema.schema.field_index(sort).is_none()
        {
            return Err(bad(format!("no sort field '{sort}'")));
        }
        let srs = options.srs.clone().unwrap_or_else(|| SpatialRef::from_layer(&schema));
        let index_srs = SpatialRef::from_layer(&schema);
        if !index_srs.is_unknown() && !srs.same_as(&index_srs) {
            return Err(Error::Unsupported("GTI output SRS must match the index SRS; warp the catalog for another output CRS".into()));
        }
        let bounds = options.extent.or(bounds).ok_or_else(|| bad("index extent is missing; specify an extent"))?;
        let placeholder = RasterDataset::new(
            Raster::from_data_native(
                RasterConfig { cols: 1, rows: 1, bands: 1, data_type: DataType::F64, ..Default::default() },
                RasterData::F64(vec![0.0]),
            )
            .map_err(bad)?,
            false,
        );
        let index_opens = usize::from(matches!(&index, Index::File { .. }));
        let mut catalog = Self {
            index,
            schema,
            base,
            options,
            header: placeholder,
            cache: VecDeque::new(),
            memory,
            stats: CatalogStats { index_opens, ..Default::default() },
            last_mask: None,
            input_paths: None,
        };
        let needs_first = catalog.options.resolution.is_none() || catalog.options.bands.is_none();
        let first = if needs_first {
            let layer = match &mut catalog.index {
                Index::Memory { layer, .. } => {
                    let mut first = crate::stream::schema_of(layer);
                    if let Some(feature) = layer.features.first() {
                        first.features.push(feature.clone());
                    }
                    first
                }
                Index::File { reader, .. } => reader.first()?,
            };
            let field = layer.schema.field_index(catalog.options.location_field.as_deref().unwrap_or("location")).unwrap();
            let location = layer
                .features
                .first()
                .and_then(|f| f.attributes.get(field))
                .and_then(FieldValue::as_str)
                .ok_or_else(|| bad("tile index is empty or has no string location"))?
                .to_string();
            catalog.stats.selected_features += 1;
            let (tile, alpha) = catalog.source(&location)?;
            Some((tile.geo_transform(), tile.width(), tile.height(), tile.crs(), tile.band_count(), tile.data_type(), tile.nodata(), alpha))
        } else {
            None
        };
        let resolution = if let Some(r) = catalog.options.resolution {
            r
        } else {
            let (gt, w, h, source_srs, ..) = first.as_ref().unwrap();
            let (gt, _, _) = crate::ops::warp::suggested_grid(*w, *h, *gt, source_srs, &srs)?;
            (gt[1].abs(), gt[5].abs())
        };
        if !resolution.0.is_finite()
            || !resolution.1.is_finite()
            || resolution.0 <= 0.0
            || resolution.1 <= 0.0
            || ![bounds.0, bounds.1, bounds.2, bounds.3].iter().all(|v| v.is_finite())
            || bounds.0 >= bounds.2
            || bounds.1 >= bounds.3
        {
            return Err(bad("invalid output resolution or extent"));
        }
        let width = ((bounds.2 - bounds.0) / resolution.0).round().max(1.0);
        let height = ((bounds.3 - bounds.1) / resolution.1).round().max(1.0);
        if !width.is_finite() || !height.is_finite() || width >= usize::MAX as f64 || height >= usize::MAX as f64 {
            return Err(bad("catalog dimensions exceed this platform's limits"));
        }
        let (width, height) = (width as usize, height as usize);
        let bands = catalog.options.bands.or(first.as_ref().map(|f| f.4)).unwrap();
        let data_type = catalog.options.data_type.or(first.as_ref().map(|f| f.5)).unwrap_or(DataType::U8);
        if bands == 0 || catalog.options.alpha_band.is_some_and(|b| b == 0 || b > bands) {
            return Err(bad("invalid band count or alpha band"));
        }
        // GDAL inspects a tile only when resolution or band count is missing.
        // Optional type/nodata/alpha can then be inherited; otherwise type defaults
        // to Byte and output validity characteristics require catalog metadata.
        if catalog.options.alpha_band.is_none() {
            catalog.options.alpha_band = first.as_ref().and_then(|f| f.7).filter(|band| *band <= bands);
        }
        let nodata = catalog.options.nodata.or(first.and_then(|f| f.6));
        let raster = Raster {
            cols: width,
            rows: height,
            bands,
            x_min: bounds.0,
            y_min: bounds.1,
            cell_size_x: resolution.0,
            cell_size_y: resolution.1,
            nodata: nodata.unwrap_or(-9999.0),
            data_type,
            crs: srs.to_crs_info(),
            metadata: Vec::new(),
            data: RasterData::new_filled(data_type, 0, 0.0),
        };
        let mut header = RasterDataset::with_driver(raster, crate::formats::RasterDriver::GTI, nodata.is_some());
        header.set_geo_transform([bounds.0, resolution.0, 0.0, bounds.3, 0.0, -resolution.1]);
        catalog.header = header;
        Ok(catalog)
    }
    /// Metadata without reading pixels.
    pub fn header(&self) -> &RasterDataset {
        &self.header
    }
    /// Cumulative I/O counters.
    pub fn stats(&self) -> CatalogStats {
        self.stats
    }
    /// Exposes this catalog to all ordinary lazy raster APIs.
    pub fn into_lazy(self) -> LazyRaster {
        LazyRaster::from_source(self.header.clone(), Box::new(CatalogSource(std::sync::Mutex::new(self))))
    }
    fn select(&mut self, bounds: Option<(f64, f64, f64, f64)>) -> Result<Vec<Entry>> {
        self.stats.index_queries += 1;
        let location = self.schema.schema.field_index(self.options.location_field.as_deref().unwrap_or("location")).unwrap();
        let sort = self.options.sort_field.as_ref().and_then(|name| self.schema.schema.field_index(name));
        let mut rows = Vec::new();
        let mut add = |feature: &wbvector::Feature| -> Result<()> {
            // Raster footprints are half-open: touching edges and degenerate bounds
            // contribute no output pixels, for both native and in-memory indexes.
            if let Some(bounds) = bounds
                && !feature.geometry.as_ref().is_some_and(|g| {
                    g.bbox().is_some_and(|b| b.min_x < bounds.2 && b.max_x > bounds.0 && b.min_y < bounds.3 && b.max_y > bounds.1)
                })
            {
                return Ok(());
            }
            let path =
                feature.attributes.get(location).and_then(FieldValue::as_str).ok_or_else(|| bad("tile location must be a string"))?;
            rows.push(Entry {
                location: path.into(),
                order: sort.and_then(|i| feature.attributes.get(i)).cloned().unwrap_or(FieldValue::Null),
                fid: feature.fid,
            });
            Ok(())
        };
        match &mut self.index {
            Index::File { reader, .. } => {
                for feature in reader.query(bounds)?.features {
                    add(&feature)?;
                }
            }
            Index::Memory { layer, tree } => {
                if let Some(b) = bounds {
                    for cell in tree.locate_in_envelope_intersecting(rstar::AABB::from_corners([b.0, b.1], [b.2, b.3])) {
                        add(&layer.features[cell.feature])?;
                    }
                } else {
                    for feature in &layer.features {
                        add(feature)?;
                    }
                }
            }
        }
        rows.sort_by(|a, b| {
            let order = field_cmp(&a.order, &b.order);
            (if self.options.sort_descending.unwrap_or(false) { order.reverse() } else { order }).then(a.fid.cmp(&b.fid))
        });
        self.stats.selected_features += rows.len();
        Ok(rows)
    }
    fn source(&mut self, location: &str) -> Result<(&mut LazyRaster, Option<usize>)> {
        if let Some(i) = self.cache.iter().position(|(p, _, _)| p == location) {
            let entry = self.cache.remove(i).unwrap();
            self.cache.push_back(entry);
            self.stats.cache_hits += 1;
        } else {
            // Close the evicted native reader before opening its replacement.
            if self.cache.len() >= self.options.cache_size.unwrap_or(32) {
                self.cache.pop_front();
            }
            let mut source = if let Some(memory) = &mut self.memory {
                memory.get_mut(location).ok_or_else(|| bad(format!("source '{location}' is missing")))?.open()?
            } else {
                let path = if Path::new(location).is_absolute() || location.contains("://") || location.starts_with("/vsi") {
                    location.into()
                } else {
                    self.base.join(location).to_string_lossy().into_owned()
                };
                if is_catalog(&path) {
                    return Err(Error::Unsupported("nested GTI source catalogs are not supported".into()));
                }
                LazyRaster::open(&path)?
            };
            let source_alpha = source.alpha_band();
            self.cache.push_back((location.into(), source, source_alpha));
            self.stats.source_opens += 1;
            self.stats.peak_cached_sources = self.stats.peak_cached_sources.max(self.cache.len());
        }
        let (_, source, alpha) = self.cache.back_mut().unwrap();
        Ok((source, *alpha))
    }
    /// Dataset-level validity: covered pixels are 255, uncovered or masked pixels are 0.
    pub fn read_mask(&mut self, window: Window) -> Result<Vec<u8>> {
        if self.last_mask.as_ref().is_none_or(|(w, _)| *w != window) {
            self.render_window(window, &[], true)?;
        }
        if window.width.saturating_mul(window.height) > MASK_CACHE_PIXELS {
            return Ok(self.last_mask.take().unwrap().1);
        }
        Ok(self.last_mask.as_ref().unwrap().1.clone())
    }
    /// Reads and composites a window; `bands` is 1-based, empty for every band.
    pub fn read_window(&mut self, window: Window, bands: &[usize]) -> Result<RasterDataset> {
        self.render_window(window, bands, false)
    }
    fn render_window(&mut self, window: Window, bands: &[usize], coverage_only: bool) -> Result<RasterDataset> {
        if !window.fits(self.header.raster.cols, self.header.raster.rows) {
            return Err(Error::InvalidArgument("GTI window is outside the catalog".into()));
        }
        let bands = if bands.is_empty() { (1..=self.header.raster.bands).collect::<Vec<_>>() } else { bands.to_vec() };
        if bands.iter().any(|b| *b == 0 || *b > self.header.raster.bands) {
            return Err(bad("band is out of range"));
        }
        let mut wanted = bands.clone();
        wanted.sort_unstable();
        wanted.dedup();
        let gt = self.header.geo_transform();
        let origin = apply_gt(&gt, window.x as f64, window.y as f64);
        let dst_gt = [origin.0, gt[1], 0.0, origin.1, 0.0, gt[5]];
        let bounds = (origin.0, origin.1 + gt[5] * window.height as f64, origin.0 + gt[1] * window.width as f64, origin.1);
        let entries = self.select(Some(bounds))?;
        let srs = self.header.crs();
        let count = self.header.raster.bands;
        let alpha = self.options.alpha_band;
        let n = window.width.checked_mul(window.height).ok_or_else(|| bad("window dimensions overflow"))?;
        let total = n.checked_mul(wanted.len()).ok_or_else(|| bad("window band count overflows"))?;
        let elements = n.checked_mul(bands.len()).ok_or_else(|| bad("selected bands overflow the window"))?;
        let mut coverage = filled_buffer(n, 0u8)?;
        let fill = self.header.nodata().unwrap_or(0.0);
        let mut out = filled_buffer(if coverage_only { 0 } else { total }, fill)?;
        for entry in entries {
            let resampling = self.options.resampling.unwrap_or_default();
            let (tile, detected_alpha) = self.source(&entry.location)?;
            let tile_gt = tile.geo_transform();
            let corners =
                [(0.0, 0.0), (tile.width() as f64, 0.0), (0.0, tile.height() as f64), (tile.width() as f64, tile.height() as f64)]
                    .map(|(x, y)| apply_gt(&tile_gt, x, y));
            let footprint = (
                corners.iter().map(|p| p.0).fold(f64::INFINITY, f64::min),
                corners.iter().map(|p| p.1).fold(f64::INFINITY, f64::min),
                corners.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max),
                corners.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max),
            );
            let same_crs = tile.crs().same_as(&srs) || tile.crs().is_unknown() || srs.is_unknown();
            // A densified envelope conservatively bounds transformed tile edges. If a
            // forward transform crosses its valid domain, keep the whole request and
            // let warp's inverse transform determine valid destination pixels.
            let footprint = if same_crs {
                footprint
            } else {
                let ct = crate::ops::Ct::new(&tile.crs(), &srs)?;
                // A footprint can cross the projection domain; retain the conservative
                // full-window fallback only for bounds sampling, not invalid CRS pairs.
                crate::ops::transform_bounds(&ct, footprint, 21).unwrap_or(bounds)
            };
            let cx0 = ((footprint.0 - origin.0) / gt[1]).floor().clamp(0.0, window.width as f64) as usize;
            let cy0 = ((footprint.3 - origin.1) / gt[5]).floor().clamp(0.0, window.height as f64) as usize;
            let cx1 = ((footprint.2 - origin.0) / gt[1]).ceil().clamp(0.0, window.width as f64) as usize;
            let cy1 = ((footprint.1 - origin.1) / gt[5]).ceil().clamp(0.0, window.height as f64) as usize;
            if cx1 <= cx0 || cy1 <= cy0 {
                continue;
            }
            let (cw, ch) = (cx1 - cx0, cy1 - cy0);
            let sub_origin = apply_gt(&dst_gt, cx0 as f64, cy0 as f64);
            let sub_gt = [sub_origin.0, gt[1], 0.0, sub_origin.1, 0.0, gt[5]];
            let sub_bounds = (sub_origin.0, sub_origin.1 + gt[5] * ch as f64, sub_origin.0 + gt[1] * cw as f64, sub_origin.1);
            let src_bounds =
                if same_crs { sub_bounds } else { crate::ops::transform_bounds(&crate::ops::Ct::new(&srs, &tile.crs())?, sub_bounds, 21)? };
            let inv = invert_gt(&tile.geo_transform()).ok_or_else(|| bad("source geotransform is not invertible"))?;
            let points =
                [(src_bounds.0, src_bounds.1), (src_bounds.0, src_bounds.3), (src_bounds.2, src_bounds.1), (src_bounds.2, src_bounds.3)]
                    .map(|(x, y)| apply_gt(&inv, x, y));
            let kernel = match resampling {
                Resampling::Nearest => 0.0,
                Resampling::Bilinear => 1.0,
                Resampling::Cubic | Resampling::CubicSpline => 2.0,
                Resampling::Lanczos => 3.0,
                _ => 0.0,
            };
            let span_x =
                points.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max) - points.iter().map(|p| p.0).fold(f64::INFINITY, f64::min);
            let span_y =
                points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max) - points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
            let pad_x = (kernel * (span_x / cw as f64).max(1.0)).ceil();
            let pad_y = (kernel * (span_y / ch as f64).max(1.0)).ceil();
            let x0 = (points.iter().map(|p| p.0).fold(f64::INFINITY, f64::min).floor() - pad_x).max(0.0).min(tile.width() as f64) as usize;
            let y0 = (points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min).floor() - pad_y).max(0.0).min(tile.height() as f64) as usize;
            let x1 =
                (points.iter().map(|p| p.0).fold(f64::NEG_INFINITY, f64::max).ceil() + pad_x).max(0.0).min(tile.width() as f64) as usize;
            let y1 =
                (points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max).ceil() + pad_y).max(0.0).min(tile.height() as f64) as usize;
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let src_window = Window::new(x0, y0, x1 - x0, y1 - y0);
            let source_bands = tile.band_count();
            if source_bands != count && source_bands != count + 1 && !(alpha == Some(count) && source_bands + 1 == count) {
                return Err(bad("tile bands must match catalog, optionally with one extra alpha band"));
            }
            let mask = if tile.has_mask() { tile.read_mask(src_window, None)? } else { None };
            // Extra untagged data bands are ignored, as in GDAL GTI.
            let source_alpha = detected_alpha.or(alpha).filter(|b| *b <= source_bands);
            let generated_alpha = source_bands + 1 == count && wanted.contains(&count);
            let mut read_bands = wanted.iter().copied().filter(|b| *b <= source_bands).collect::<Vec<_>>();
            if let Some(alpha) = source_alpha {
                read_bands.push(alpha);
            }
            // Synthesizing alpha requires validity of all data bands. Ordinary subset
            // reads need only their requested samples and the source alpha/mask.
            if generated_alpha {
                read_bands.extend(1..=source_bands);
            }
            read_bands.sort_unstable();
            read_bands.dedup();
            let source_alpha = source_alpha.and_then(|b| read_bands.binary_search(&b).ok());
            let mut source = tile.read_window(src_window, &WindowOptions { bands: read_bands.clone(), ..Default::default() })?;
            let sn = src_window.width * src_window.height;
            let data = std::mem::replace(&mut source.raster.data, RasterData::F64(Vec::new()));
            #[allow(clippy::unnecessary_cast)]
            let mut values = match data {
                RasterData::F64(values) => values,
                data => crate::pixel::with_data!(&data, values => values.iter().map(|&v|v as f64).collect::<Vec<f64>>()),
            };
            let mut invalid = filled_buffer(sn, false)?;
            for (i, invalid) in invalid.iter_mut().enumerate() {
                *invalid = mask.as_ref().is_some_and(|m| m[i] == 0) || source_alpha.is_some_and(|b| values[b * sn + i] == 0.0);
            }
            let nodata = source.nodata();
            for band in values.chunks_mut(sn) {
                for (value, &invalid) in band.iter_mut().zip(&invalid) {
                    if invalid || nodata.is_some_and(|nd| crate::ops::is_nodata(*value, nd)) {
                        *value = f64::NAN;
                    }
                }
            }
            let values = if read_bands == wanted {
                values
            } else {
                let mut selected = Vec::with_capacity(sn * wanted.len());
                for band in &wanted {
                    if generated_alpha && *band == count {
                        selected.extend((0..sn).map(|i| {
                            if (0..read_bands.len()).any(|b| Some(b) != source_alpha && !values[b * sn + i].is_nan()) {
                                255.0
                            } else {
                                f64::NAN
                            }
                        }));
                    } else {
                        let offset = read_bands.binary_search(band).unwrap() * sn;
                        selected.extend_from_slice(&values[offset..offset + sn]);
                    }
                }
                selected
            };
            source.raster.bands = wanted.len();
            source.raster.data = RasterData::F64(values);
            source.raster.data_type = DataType::F64;
            source.raster.nodata = f64::NAN;
            source.has_nodata = true;
            self.stats.source_pixels += sn * read_bands.len();
            self.stats.warped_pixels += cw * ch * wanted.len();
            let warped = crate::ops::warp::warp(
                &[&source],
                &WarpOptions {
                    dst_srs: Some(srs.clone()),
                    dst_transform: Some(sub_gt),
                    size: Some((cw, ch)),
                    resampling,
                    src_nodata: Nodata::Auto,
                    dst_nodata: Nodata::Values(vec![f64::NAN]),
                    output_type: Some(DataType::F64),
                    error_threshold: 0.0,
                    ..Default::default()
                },
            )?;
            for (b, band) in wanted.iter().enumerate() {
                for (k, value) in warped.raster.band_view(b).as_slice().iter().copied().enumerate() {
                    let i = (cy0 + k / cw) * window.width + cx0 + k % cw;
                    if !value.is_nan() {
                        if !coverage_only {
                            out[b * n + i] = value;
                        }
                        if alpha != Some(*band) {
                            coverage[i] = 255;
                        }
                    }
                }
            }
        }
        // Dataset coverage is the union of every data band. A subset cannot
        // populate that cache; a later mask request renders the full band set.
        self.last_mask = (wanted.len() == count && (coverage_only || n <= MASK_CACHE_PIXELS)).then_some((window, coverage));
        // Coverage is a union across data bands, so nodata may require reading
        // every band. Mask-only requests avoid the composite and typed output.
        if coverage_only {
            return Ok(self.header.clone());
        }
        let mut data = RasterData::new_filled(self.header.raster.data_type, 0, 0.0);
        crate::pixel::with_data!(&mut data, v => {
            v.try_reserve_exact(elements).map_err(|_| bad("cannot allocate catalog output"))?;
            v.resize(elements, 0 as _);
        });
        crate::pixel::with_data!(&mut data, v => {
            for (dst, band) in bands.iter().enumerate() {
                let source = wanted.binary_search(band).unwrap();
                for i in 0..n {
                    v[dst*n+i] = crate::ops::warp_round(out[source*n+i], self.header.raster.data_type) as _;
                }
            }
        });
        let mut raster = self.header.raster.clone();
        raster.cols = window.width;
        raster.rows = window.height;
        raster.bands = bands.len();
        raster.data = data;
        let mut ds = RasterDataset::with_driver(raster, crate::formats::RasterDriver::GTI, self.header.has_nodata);
        ds.set_geo_transform(dst_gt);
        Ok(ds)
    }
}

type Bounds = (f64, f64, f64, f64);
type Metadata = Vec<(String, String)>;
type IndexHeader = (Box<dyn IndexReader>, Layer, Option<Bounds>, Metadata);
#[cfg_attr(not(any(feature = "sqlite", feature = "flatgeobuf")), allow(unused_variables))]
fn index_header(path: &str, wanted: Option<&str>) -> Result<IndexHeader> {
    let extension = crate::formats::extension(path).unwrap_or_default();
    #[cfg(feature = "sqlite")]
    if extension == "gpkg" {
        use crate::drivers::sqlite::Value;
        use crate::drivers::vector::gpkg_read;
        let db = crate::drivers::sqlite_edit::open_consistent(Path::new(path))?;
        let (contents, _) = gpkg_read::contents(&db)?;
        let metadata_table = db.table("gpkg_metadata").ok();
        let references = db.table("gpkg_metadata_reference").ok();
        let metadata_for = |table: Option<&str>| -> Metadata {
            let (Some(metadata), Some(references)) = (&metadata_table, &references) else { return Vec::new() };
            let mut items = Vec::new();
            for row in &references.rows {
                let scope = references.get(row, "reference_scope").and_then(Value::as_str);
                let name = references.get(row, "table_name").and_then(Value::as_str);
                if !((table.is_none() && scope == Some("geopackage")) || (table.is_some() && scope == Some("table") && name == table)) {
                    continue;
                }
                let id = references.get(row, "md_file_id").and_then(Value::as_i64);
                if let Some(text) = metadata
                    .rows
                    .iter()
                    .find(|r| metadata.get(r, "id").and_then(Value::as_i64) == id)
                    .and_then(|r| metadata.get(r, "metadata"))
                    .and_then(Value::as_str)
                    && let Ok(root) = crate::drivers::vector::xml::parse(text)
                {
                    let mut elements = Vec::new();
                    root.find_all("MDI", &mut elements);
                    for element in elements {
                        if let Some(key) = element.attr("key") {
                            items.push((key.into(), element.text.trim().into()));
                        }
                    }
                }
            }
            items
        };
        let dataset_metadata = metadata_for(None);
        let name = wanted.or_else(|| dataset_metadata.iter().find(|(k, _)| k == "TILE_INDEX_LAYER").map(|(_, v)| v.as_str()));
        let content = if let Some(name) = name {
            contents.iter().find(|c| c.name == name).ok_or_else(|| bad(format!("no index layer '{name}'")))?
        } else {
            if contents.len() != 1 {
                return Err(bad("IndexLayer is required for an index with several layers"));
            }
            &contents[0]
        };
        let contents_table = db.table("gpkg_contents")?;
        let bounds = contents_table
            .rows
            .iter()
            .find(|r| contents_table.get(r, "table_name").and_then(Value::as_str) == Some(content.name.as_str()))
            .and_then(|r| {
                Some((
                    contents_table.get(r, "min_x")?.as_f64()?,
                    contents_table.get(r, "min_y")?.as_f64()?,
                    contents_table.get(r, "max_x")?.as_f64()?,
                    contents_table.get(r, "max_y")?.as_f64()?,
                ))
            });
        let metadata = metadata_for(Some(&content.name));
        let reader = gpkg_read::IndexedLayer::new(db, content)?;
        let layer = reader.layer().clone();
        return Ok((Box::new(reader), layer, bounds, metadata));
    }
    #[cfg(feature = "flatgeobuf")]
    if extension == "fgb" {
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let prepared = crate::drivers::vector::flatgeobuf::PreparedIndex::open(&mut file, len)?;
        if wanted.is_some_and(|w| w != prepared.layer.name) {
            return Err(bad("no requested FlatGeobuf layer"));
        }
        let metadata = prepared
            .metadata
            .as_deref()
            .and_then(|text| serde_json::from_str::<HashMap<String, serde_json::Value>>(text).ok())
            .map(|m| m.into_iter().map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect())
            .unwrap_or_default();
        let layer = prepared.layer.clone();
        let bounds = prepared.info.extent.map(|b| (b[0], b[1], b[2], b[3]));
        return Ok((Box::new(FgbIndex { file, prepared }), layer, bounds, metadata));
    }
    Err(Error::Unsupported(format!("GTI index '{path}' requires an enabled GeoPackage or FlatGeobuf driver (found '{extension}')")))
}
fn apply_metadata(metadata: &[(String, String)], options: &mut CatalogOptions) -> Result<()> {
    let get = |key: &str| metadata.iter().rev().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str());
    let num =
        |key: &str| -> Result<Option<f64>> { get(key).map(|v| v.parse::<f64>().map_err(|_| bad(format!("invalid {key}")))).transpose() };
    if options.location_field.is_none() {
        options.location_field = get("LOCATION_FIELD").map(str::to_string);
    }
    if options.sort_field.is_none() {
        options.sort_field = get("SORT_FIELD").map(str::to_string);
    }
    if options.sort_descending.is_none() {
        options.sort_descending = get("SORT_FIELD_ASC").map(|v| matches!(v.to_ascii_lowercase().as_str(), "no" | "false" | "0"));
    }
    match (num("XSIZE")?, num("YSIZE")?, get("GEOTRANSFORM")) {
        (None, None, None) => {}
        (Some(width), Some(height), Some(transform)) => {
            if [width, height].iter().any(|size| !size.is_finite() || *size <= 0.0 || size.fract() != 0.0 || *size >= usize::MAX as f64) {
                return Err(bad("XSIZE/YSIZE must be positive integer dimensions"));
            }
            let coefficients = transform
                .split(|c: char| c == ',' || c.is_whitespace())
                .filter(|s| !s.is_empty())
                .map(|s| s.parse::<f64>().map_err(|_| bad("invalid GEOTRANSFORM")))
                .collect::<Result<Vec<_>>>()?;
            let gt: [f64; 6] = coefficients.try_into().map_err(|_| bad("GEOTRANSFORM requires six coefficients"))?;
            if gt.iter().any(|value| !value.is_finite()) {
                return Err(bad("GEOTRANSFORM coefficients must be finite"));
            }
            if gt[2] != 0.0 || gt[4] != 0.0 || gt[1] <= 0.0 || gt[5] >= 0.0 {
                return Err(Error::Unsupported("GTI output GEOTRANSFORM must be north-up without rotation".into()));
            }
            options.resolution.get_or_insert((gt[1], -gt[5]));
            options.extent.get_or_insert((gt[0], gt[3] + gt[5] * height, gt[0] + gt[1] * width, gt[3]));
        }
        _ => return Err(bad("XSIZE, YSIZE and GEOTRANSFORM must be supplied together")),
    }
    if options.resolution.is_none()
        && let (Some(x), Some(y)) = (num("RESX")?, num("RESY")?)
    {
        options.resolution = Some((x, y));
    }
    if options.extent.is_none()
        && let (Some(a), Some(b), Some(c), Some(d)) = (num("MINX")?, num("MINY")?, num("MAXX")?, num("MAXY")?)
    {
        options.extent = Some((a, b, c, d));
    }
    if options.bands.is_none() {
        options.bands = get("BAND_COUNT").map(|v| v.parse::<usize>().map_err(|_| bad("invalid BAND_COUNT"))).transpose()?;
    }
    if options.data_type.is_none() {
        options.data_type = get("DATA_TYPE").map(crate::ops::parse_data_type).transpose()?;
    }
    if options.nodata.is_none() {
        options.nodata =
            get("NODATA").and_then(|v| v.split(',').next()).map(|v| v.parse::<f64>().map_err(|_| bad("invalid NODATA"))).transpose()?;
    }
    if options.srs.is_none() {
        options.srs = get("SRS").map(SpatialRef::parse).transpose()?;
    }
    if options.alpha_band.is_none() {
        options.alpha_band =
            get("COLOR_INTERPRETATION").and_then(|v| v.split(',').position(|v| v.trim().eq_ignore_ascii_case("alpha"))).map(|i| i + 1);
    }
    if options.resampling.is_none()
        && let Some(value) = get("RESAMPLING")
    {
        options.resampling = Some(Resampling::parse(value)?);
    }
    Ok(())
}
fn apply_xml(root: &crate::drivers::vector::xml::Element, options: &mut CatalogOptions) -> Result<()> {
    for unsupported in ["SQL", "SpatialSQL", "Filter", "Overview"] {
        if root.child(unsupported).is_some() {
            return Err(Error::Unsupported(format!("GTI XML {unsupported} is not supported")));
        }
    }
    if options.layer.is_none() {
        options.layer = root.child_text("IndexLayer").map(str::to_string);
    }
    let names = [
        ("LocationField", "LOCATION_FIELD"),
        ("SortField", "SORT_FIELD"),
        ("SortFieldAsc", "SORT_FIELD_ASC"),
        ("SRS", "SRS"),
        ("ResX", "RESX"),
        ("ResY", "RESY"),
        ("GeoTransform", "GEOTRANSFORM"),
        ("XSize", "XSIZE"),
        ("YSize", "YSIZE"),
        ("MinX", "MINX"),
        ("MinY", "MINY"),
        ("MaxX", "MAXX"),
        ("MaxY", "MAXY"),
        ("BandCount", "BAND_COUNT"),
        ("Resampling", "RESAMPLING"),
    ];
    let mut metadata =
        names.iter().filter_map(|(xml, key)| root.child_text(xml).map(|v| (key.to_string(), v.to_string()))).collect::<Vec<_>>();
    let bands = root.children_named("Band").collect::<Vec<_>>();
    if !bands.is_empty() {
        if options.bands.is_none() {
            options.bands = Some(bands.len());
        }
        if let Some(data_type) = bands[0].attr("dataType") {
            metadata.push(("DATA_TYPE".into(), data_type.into()));
        }
        if let Some(nodata) = bands[0].child_text("NoDataValue") {
            metadata.push(("NODATA".into(), nodata.into()));
        }
        if options.alpha_band.is_none() {
            options.alpha_band = bands
                .iter()
                .find(|b| b.child_text("ColorInterp").is_some_and(|v| v.eq_ignore_ascii_case("alpha")))
                .and_then(|b| b.attr("band"))
                .and_then(|v| v.parse().ok());
        }
    }
    apply_metadata(&metadata, options)
}
/// Whether a name explicitly denotes a GTI raster catalog.
pub(crate) fn is_catalog(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    lower.starts_with("gti:") || lower.ends_with(".gti") || lower.ends_with(".gti.gpkg") || lower.ends_with(".gti.fgb")
}
pub(crate) fn open_lazy(path: &str) -> Option<Result<LazyRaster>> {
    is_catalog(path).then(|| RasterCatalog::open(path, Default::default()).map(RasterCatalog::into_lazy))
}

struct CatalogSource(std::sync::Mutex<RasterCatalog>);
impl WindowSource for CatalogSource {
    fn has_mask(&self) -> bool {
        true
    }
    fn read_mask(&mut self, level: usize, window: Window) -> Result<Option<Vec<u8>>> {
        if level != 0 {
            return Err(bad("catalog has no explicit overviews"));
        }
        self.0.lock().map_err(|_| bad("catalog lock poisoned"))?.read_mask(window).map(Some)
    }
    fn overviews(&self) -> Vec<(usize, usize)> {
        Vec::new()
    }
    fn block_size(&self) -> Option<(usize, usize)> {
        Some((256, 256))
    }
    fn streamed(&self) -> Option<&dyn crate::cog::StreamedSource> {
        Some(self)
    }
    fn read(&mut self, level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
        if level != 0 {
            return Err(bad("catalog has no explicit overviews"));
        }
        Ok(self.0.lock().map_err(|_| bad("catalog lock poisoned"))?.read_window(window, bands)?.raster.data)
    }
}
impl crate::cog::StreamedSource for CatalogSource {
    fn inputs(&self) -> Vec<String> {
        // Legacy inventory callers cannot represent errors; writers use checked_inputs.
        self.checked_inputs().unwrap_or_default()
    }
    fn checked_inputs(&self) -> Result<Vec<String>> {
        // The writer checks these before creating its destination, so overwriting a source
        // tile uses a completed in-memory read instead of truncating a tile still needed.
        let _inventory = crate::ops::vrt::SourceInventoryScope::enter();
        let mut catalog = self.0.lock().map_err(|_| bad("catalog lock poisoned"))?;
        if let Some(paths) = &catalog.input_paths {
            return Ok(paths.clone());
        }
        let mut paths = Vec::new();
        let mut inventoried = HashSet::new();
        if let Some(sources) = &catalog.memory {
            for source in sources.values() {
                if let Input::Path(path) = &source.0
                    && !inventoried.insert(path.clone())
                {
                    continue;
                }
                paths.extend(source.inputs()?);
            }
            catalog.input_paths = Some(paths.clone());
            return Ok(paths);
        }
        let field = catalog.schema.schema.field_index(catalog.options.location_field.as_deref().unwrap_or("location")).unwrap();
        let mut add = |layer: &Layer| -> Result<()> {
            for feature in &layer.features {
                if let Some(location) = feature.attributes.get(field).and_then(FieldValue::as_str) {
                    let path = if Path::new(location).is_absolute() || location.contains("://") || location.starts_with("/vsi") {
                        location.into()
                    } else {
                        catalog.base.join(location).to_string_lossy().into_owned()
                    };
                    if inventoried.insert(path.clone()) {
                        paths.extend(crate::ops::vrt::source_input_paths(&path)?);
                    }
                }
            }
            Ok(())
        };
        match &catalog.index {
            Index::Memory { layer, .. } => add(layer)?,
            Index::File { path, layer, .. } => {
                let readers =
                    crate::stream::open_layers(path, &VectorOpenOptions { layers: vec![layer.clone()], ..Default::default() }, 4096)?;
                for mut reader in readers {
                    while let Some(chunk) = reader.read_chunk(4096)? {
                        add(&chunk)?;
                    }
                }
                paths.push(path.clone());
            }
        }
        catalog.input_paths = Some(paths.clone());
        Ok(paths)
    }
    fn read_rows(&self, window: Window, bands: &[usize]) -> Result<RasterData> {
        Ok(self.0.lock().map_err(|_| bad("catalog lock poisoned"))?.read_window(window, bands)?.raster.data)
    }
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    #[test]
    fn size_metadata_and_xml_define_resolution() {
        let metadata = [
            ("XSIZE", "12"),
            ("YSIZE", "6"),
            ("GEOTRANSFORM", "10,2,0,32,0,-2"),
            ("MINX", "10"),
            ("MINY", "20"),
            ("MAXX", "34"),
            ("MAXY", "32"),
        ]
        .map(|(key, value)| (key.into(), value.into()));
        let mut options = CatalogOptions::default();
        apply_metadata(&metadata, &mut options).unwrap();
        assert_eq!(options.resolution, Some((2., 2.)));
        let xml = crate::drivers::vector::xml::parse("<GDALTileIndexDataset><XSize>12</XSize><YSize>6</YSize><GeoTransform>10,2,0,32,0,-2</GeoTransform><MinX>10</MinX><MinY>20</MinY><MaxX>34</MaxX><MaxY>32</MaxY></GDALTileIndexDataset>").unwrap();
        let mut options = CatalogOptions::default();
        apply_xml(&xml, &mut options).unwrap();
        assert_eq!(options.resolution, Some((2., 2.)));
        let mut explicit = CatalogOptions { resolution: Some((3., 3.)), ..Default::default() };
        apply_metadata(&metadata, &mut explicit).unwrap();
        assert_eq!(explicit.resolution, Some((3., 3.)));
    }
    #[test]
    fn size_metadata_requires_a_valid_complete_grid() {
        for (width, height) in [("0", "2"), ("2.5", "2"), ("NaN", "2"), ("2", "-1")] {
            let metadata =
                [("XSIZE".into(), width.into()), ("YSIZE".into(), height.into()), ("GEOTRANSFORM".into(), "0,1,0,4,0,-1".into())];
            let mut options = CatalogOptions { extent: Some((0., 0., 4., 4.)), ..Default::default() };
            assert!(apply_metadata(&metadata, &mut options).is_err());
        }
        let metadata = [("XSIZE".into(), "2".into()), ("YSIZE".into(), "2".into())];
        assert!(apply_metadata(&metadata, &mut Default::default()).is_err());
        assert!(apply_metadata(&metadata[..1], &mut Default::default()).is_err());
        for transform in ["0,1,0.1,4,0,-1", "0,1,0,4,0", "NaN,1,0,4,0,-1"] {
            let metadata = [("XSIZE".into(), "2".into()), ("YSIZE".into(), "2".into()), ("GEOTRANSFORM".into(), transform.into())];
            assert!(apply_metadata(&metadata, &mut Default::default()).is_err());
        }
    }
}

/// Allocate fallibly so capacity overflow and allocation failure are reported.
fn filled_buffer<T: Clone>(count: usize, value: T) -> Result<Vec<T>> {
    let mut buffer = Vec::new();
    buffer.try_reserve_exact(count).map_err(|_| bad("cannot allocate catalog window buffer"))?;
    buffer.resize(count, value);
    Ok(buffer)
}

#[cfg(test)]
mod allocation_tests {
    #[test]
    fn oversized_window_buffer_returns_error() {
        assert!(super::filled_buffer(usize::MAX, 0.0f64).is_err());
    }
}

#[cfg(test)]
mod lazy_source_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Count(Arc<AtomicUsize>);
    impl WindowSource for Count {
        fn overviews(&self) -> Vec<(usize, usize)> {
            Vec::new()
        }
        fn read(&mut self, _: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
            let n = window.width * window.height * bands.len();
            self.0.fetch_add(n, Ordering::Relaxed);
            Ok(RasterData::F64(vec![7.; n]))
        }
    }
    #[test]
    fn supplied_lazy_sources_read_only_selected_windows_and_outlive_handles() {
        let mut layer = Layer::new("tiles");
        layer.add_field(wbvector::FieldDef::new("location", wbvector::FieldType::Text));
        let mut sources = HashMap::new();
        let reads = Arc::new(AtomicUsize::new(0));
        for (i, x) in [0., 10.].into_iter().enumerate() {
            let name = format!("{i}.tif");
            let header = crate::ops::raster_header(
                crate::ops::Grid { width: 4, height: 4, geo_transform: [x, 1., 0., 4., 0., -1.] },
                DataType::F64,
                1,
                None,
                &Default::default(),
            )
            .unwrap();
            let mut original = LazyRaster::from_source(header, Box::new(Count(reads.clone())));
            sources.insert(name.clone(), original.shared());
            drop(original);
            let ring = wbvector::geometry::Ring(vec![
                wbvector::Coord::xy(x, 0.),
                wbvector::Coord::xy(x + 4., 0.),
                wbvector::Coord::xy(x + 4., 4.),
                wbvector::Coord::xy(x, 4.),
                wbvector::Coord::xy(x, 0.),
            ]);
            let mut feature =
                wbvector::Feature::with_geometry(i as u64, wbvector::Geometry::Polygon { exterior: ring, interiors: Vec::new() }, 1);
            feature.attributes[0] = FieldValue::Text(name);
            layer.push(feature);
        }
        let mut catalog = RasterCatalog::from_lazy_layer(
            layer,
            sources,
            CatalogOptions {
                resolution: Some((1., 1.)),
                bands: Some(1),
                data_type: Some(DataType::F64),
                cache_size: Some(1),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(reads.load(Ordering::Relaxed), 0);
        assert_eq!(catalog.read_window(Window::new(0, 0, 1, 1), &[1]).unwrap().raster.data.to_f64_vec(), vec![7.]);
        assert_eq!(reads.load(Ordering::Relaxed), 1);
        assert_eq!(catalog.stats().source_opens, 1);
        catalog.header.raster.cols = 2048;
        catalog.header.raster.rows = 1024;
        let mask = catalog.read_mask(Window::new(0, 0, 2048, 1024)).unwrap();
        assert_eq!(mask.len(), 2048 * 1024);
        assert_eq!(mask.iter().filter(|&&value| value == 255).count(), 32);
        assert!(catalog.last_mask.is_none());
    }
}

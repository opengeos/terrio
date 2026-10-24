//! Indexed raster catalogs (GDAL GTI): lazy, spatially selected mosaics.
//!
//! GeoPackage and FlatGeobuf indexes are queried through their spatial indexes for every
//! output window. Source datasets are opened only when selected, in a bounded LRU cache.
use crate::cog::{LazyRaster, Window, WindowOptions, WindowSource};
use crate::crs::SpatialRef;
use crate::ops::warp::{Nodata, WarpOptions};
use crate::ops::{Resampling, apply_gt, invert_gt};
use crate::{Error, RasterDataset, Result, VectorOpenOptions};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wbraster::raster::RasterData;
use wbraster::{DataType, Raster, RasterConfig};
use wbvector::{FieldValue, Layer};

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
    /// Output type.
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
    /// Index features returned by spatial queries.
    pub selected_features: usize,
    /// Source dataset opens (including reopens after eviction).
    pub source_opens: usize,
    /// Cache hits.
    pub cache_hits: usize,
    /// Source pixels requested (across all bands, before reprojection).
    pub source_pixels: usize,
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
enum Index {
    File { path: String, layer: String },
    Memory { layer: Box<Layer>, tree: rstar::RTree<IndexCell> },
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
    memory: Option<Arc<HashMap<String, RasterDataset>>>,
    stats: CatalogStats,
    last_mask: Option<(Window, Vec<u8>)>,
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
        let (schema, bounds, metadata) = index_header(&index_path, options.layer.as_deref())?;
        apply_metadata(&metadata, &mut options)?;
        let layer = schema.name.clone();
        let base = Path::new(&index_path).parent().unwrap_or(Path::new(".")).to_path_buf();
        Self::initialize(Index::File { path: index_path, layer }, schema, bounds, base, options, None)
    }
    /// Opens an index layer and named source rasters already held in memory, including WASM.
    pub fn from_layer(layer: Layer, sources: HashMap<String, RasterDataset>, options: CatalogOptions) -> Result<Self> {
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
        Self::initialize(Index::Memory { layer: Box::new(layer), tree }, schema, bounds, PathBuf::new(), options, Some(Arc::new(sources)))
    }
    fn initialize(
        index: Index,
        schema: Layer,
        bounds: Option<(f64, f64, f64, f64)>,
        base: PathBuf,
        options: CatalogOptions,
        memory: Option<Arc<HashMap<String, RasterDataset>>>,
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
        let mut catalog = Self {
            index,
            schema,
            base,
            options,
            header: placeholder,
            cache: VecDeque::new(),
            memory,
            stats: CatalogStats::default(),
            last_mask: None,
        };
        let needs_first = catalog.options.resolution.is_none() || catalog.options.bands.is_none() || catalog.options.data_type.is_none();
        let first = if needs_first {
            let layer = match &catalog.index {
                Index::Memory { layer, .. } => {
                    let mut first = crate::stream::schema_of(layer);
                    if let Some(feature) = layer.features.first() {
                        first.features.push(feature.clone());
                    }
                    first
                }
                Index::File { path, layer } => {
                    let mut readers =
                        crate::stream::open_layers(path, &VectorOpenOptions { layers: vec![layer.clone()], ..Default::default() }, 1)?;
                    readers[0].read_chunk(1)?.ok_or_else(|| bad("tile index is empty"))?
                }
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
        let data_type = catalog.options.data_type.or(first.as_ref().map(|f| f.5)).unwrap();
        if bands == 0 || catalog.options.alpha_band.is_some_and(|b| b == 0 || b > bands) {
            return Err(bad("invalid band count or alpha band"));
        }
        if catalog.options.alpha_band.is_none() {
            catalog.options.alpha_band = first.as_ref().and_then(|f| f.7);
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
        let layers = match &self.index {
            Index::File { path, layer } => {
                crate::open_vector(path, &VectorOpenOptions { layers: vec![layer.clone()], spatial_filter: bounds, ..Default::default() })?
                    .layers
            }
            Index::Memory { layer, tree } => {
                let mut selected = crate::stream::schema_of(layer);
                selected.features = if let Some(b) = bounds {
                    tree.locate_in_envelope_intersecting(&rstar::AABB::from_corners([b.0, b.1], [b.2, b.3]))
                        .map(|cell| layer.features[cell.feature].clone())
                        .collect()
                } else {
                    layer.features.clone()
                };
                vec![selected]
            }
        };
        let location = self.schema.schema.field_index(self.options.location_field.as_deref().unwrap_or("location")).unwrap();
        let sort = self.options.sort_field.as_ref().and_then(|name| self.schema.schema.field_index(name));
        let mut rows = Vec::new();
        for layer in layers {
            for feature in layer.features {
                if let Some(bounds) = bounds
                    && !feature.geometry.as_ref().is_some_and(|g| {
                        g.bbox().is_some_and(|b| b.min_x < bounds.2 && b.max_x > bounds.0 && b.min_y < bounds.3 && b.max_y > bounds.1)
                    })
                {
                    continue;
                }
                let path =
                    feature.attributes.get(location).and_then(FieldValue::as_str).ok_or_else(|| bad("tile location must be a string"))?;
                rows.push(Entry {
                    location: path.into(),
                    order: sort.and_then(|i| feature.attributes.get(i)).cloned().unwrap_or(FieldValue::Null),
                    fid: feature.fid,
                });
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
            let mut source = if let Some(memory) = &self.memory {
                let ds = memory.get(location).ok_or_else(|| bad(format!("source '{location}' is missing")))?;
                let r = &ds.raster;
                let raster = Raster {
                    cols: r.cols,
                    rows: r.rows,
                    bands: r.bands,
                    x_min: r.x_min,
                    y_min: r.y_min,
                    cell_size_x: r.cell_size_x,
                    cell_size_y: r.cell_size_y,
                    nodata: r.nodata,
                    data_type: r.data_type,
                    crs: r.crs.clone(),
                    metadata: r.metadata.clone(),
                    data: RasterData::new_filled(r.data_type, 0, 0.0),
                };
                let mut header = RasterDataset::with_driver(raster, ds.driver, ds.has_nodata);
                header.affine = ds.affine;
                header.complex = ds.complex;
                header.geolocation = ds.geolocation.clone();
                LazyRaster::from_source(header, Box::new(MemoryTile { datasets: memory.clone(), name: location.into() }))
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
            if self.cache.len() >= self.options.cache_size.unwrap_or(32) {
                self.cache.pop_front();
            }
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
            self.read_window(window, &[])?;
        }
        Ok(self.last_mask.as_ref().unwrap().1.clone())
    }
    /// Reads and composites a window; `bands` is 1-based, empty for every band.
    pub fn read_window(&mut self, window: Window, bands: &[usize]) -> Result<RasterDataset> {
        if !window.fits(self.header.raster.cols, self.header.raster.rows) {
            return Err(Error::InvalidArgument("GTI window is outside the catalog".into()));
        }
        let bands = if bands.is_empty() { (1..=self.header.raster.bands).collect::<Vec<_>>() } else { bands.to_vec() };
        if bands.iter().any(|b| *b == 0 || *b > self.header.raster.bands) {
            return Err(bad("band is out of range"));
        }
        let gt = self.header.geo_transform();
        let origin = apply_gt(&gt, window.x as f64, window.y as f64);
        let dst_gt = [origin.0, gt[1], 0.0, origin.1, 0.0, gt[5]];
        let bounds = (origin.0, origin.1 + gt[5] * window.height as f64, origin.0 + gt[1] * window.width as f64, origin.1);
        let entries = self.select(Some(bounds))?;
        let srs = self.header.crs();
        let count = self.header.raster.bands;
        let alpha = self.options.alpha_band;
        let n = window.width.checked_mul(window.height).ok_or_else(|| bad("window dimensions overflow"))?;
        let total = n.checked_mul(count).ok_or_else(|| bad("window band count overflows"))?;
        let elements = n.checked_mul(bands.len()).ok_or_else(|| bad("selected bands overflow the window"))?;
        let mut coverage = vec![0u8; n];
        let fill = self.header.nodata().unwrap_or(0.0);
        let mut out = vec![fill; total];
        for entry in entries {
            let resampling = self.options.resampling.unwrap_or_default();
            let (tile, detected_alpha) = self.source(&entry.location)?;
            let src_bounds = if tile.crs().same_as(&srs) || tile.crs().is_unknown() || srs.is_unknown() {
                bounds
            } else {
                crate::ops::transform_bounds(&crate::ops::Ct::new(&srs, &tile.crs())?, bounds, 21)?
            };
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
            let pad_x = (kernel * (span_x / window.width as f64).max(1.0)).ceil();
            let pad_y = (kernel * (span_y / window.height as f64).max(1.0)).ceil();
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
            let mask = tile.read_mask(src_window, None)?;
            let mut source = tile.read_window(src_window, &WindowOptions::default())?;
            let sn = src_window.width * src_window.height;
            let read_bands = source.raster.bands;
            let mut source_bands = read_bands;
            if source_bands != count && source_bands != count + 1 && !(alpha == Some(count) && source_bands + 1 == count) {
                return Err(bad("tile bands must match catalog, optionally with one extra alpha band"));
            }
            let source_alpha =
                if source_bands == count + 1 { Some(source_bands) } else { detected_alpha.or(alpha).filter(|b| *b <= source_bands) };
            let mut values = (0..source_bands).flat_map(|b| crate::ops::band_f64(&source.raster, b)).collect::<Vec<_>>();
            for i in 0..sn {
                let invalid = mask.as_ref().is_some_and(|m| m[i] == 0) || source_alpha.is_some_and(|b| values[(b - 1) * sn + i] == 0.0);
                for b in 0..source_bands {
                    let value = &mut values[b * sn + i];
                    if invalid || source.nodata().is_some_and(|nd| crate::ops::is_nodata(*value, nd)) {
                        *value = f64::NAN;
                    }
                }
            }
            if source_bands + 1 == count {
                let alpha = (0..sn)
                    .map(|i| if (0..source_bands).any(|b| !values[b * sn + i].is_nan()) { 255.0 } else { f64::NAN })
                    .collect::<Vec<_>>();
                values.extend(alpha);
                source_bands += 1;
                source.raster.bands = source_bands;
            }
            source.raster.data = RasterData::F64(values);
            source.raster.data_type = DataType::F64;
            source.raster.nodata = f64::NAN;
            source.has_nodata = true;
            self.stats.source_pixels += sn * read_bands;
            let warped = crate::ops::warp::warp(
                &[&source],
                &WarpOptions {
                    dst_srs: Some(srs.clone()),
                    dst_transform: Some(dst_gt),
                    size: Some((window.width, window.height)),
                    resampling,
                    src_nodata: Nodata::Auto,
                    dst_nodata: Nodata::Values(vec![f64::NAN]),
                    output_type: Some(DataType::F64),
                    error_threshold: 0.0,
                    ..Default::default()
                },
            )?;
            for b in 0..count {
                for (i, value) in crate::ops::band_f64(&warped.raster, b).into_iter().enumerate() {
                    if !value.is_nan() {
                        out[b * n + i] = value;
                        if alpha != Some(b + 1) {
                            coverage[i] = 255;
                        }
                    }
                }
            }
        }
        self.last_mask = Some((window, coverage));
        let mut data = RasterData::new_filled(self.header.raster.data_type, elements, 0.0);
        crate::pixel::with_data!(&mut data, v => {
            for (dst, band) in bands.iter().enumerate() {
                for i in 0..n {
                    v[dst*n+i] = crate::ops::warp_round(out[(band-1)*n+i], self.header.raster.data_type) as _;
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
fn index_header(path: &str, _wanted: Option<&str>) -> Result<(Layer, Option<Bounds>, Metadata)> {
    let extension = crate::formats::extension(path).unwrap_or_default();
    #[cfg(feature = "sqlite")]
    if extension == "gpkg" {
        use crate::drivers::sqlite::{Db, Value};
        use crate::drivers::vector::gpkg_read;
        let db = Db::open(Path::new(path))?;
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
        let name = _wanted.or_else(|| dataset_metadata.iter().find(|(k, _)| k == "TILE_INDEX_LAYER").map(|(_, v)| v.as_str()));
        let content = if let Some(name) = name {
            contents.iter().find(|c| c.name == name).ok_or_else(|| bad(format!("no index layer '{name}'")))?
        } else {
            if contents.len() != 1 {
                return Err(bad("IndexLayer is required for an index with several layers"));
            }
            &contents[0]
        };
        let layer = gpkg_read::layer_def(&db, content)?.layer;
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
        return Ok((layer, bounds, metadata_for(Some(&content.name))));
    }
    #[cfg(feature = "flatgeobuf")]
    if extension == "fgb" {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let (info, layer) = crate::drivers::vector::flatgeobuf::read_header(&mut file, len)?;
        if _wanted.is_some_and(|w| w != layer.name) {
            return Err(bad("no requested FlatGeobuf layer"));
        }
        file.seek(SeekFrom::Start(0))?;
        let mut header = vec![0; info.header_size as usize];
        file.read_exact(&mut header)?;
        let metadata = flatgeobuf::size_prefixed_root_as_header(&header[8..])
            .ok()
            .and_then(|h| h.metadata())
            .and_then(|text| serde_json::from_str::<HashMap<String, serde_json::Value>>(text).ok())
            .map(|m| m.into_iter().map(|(k, v)| (k, v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect())
            .unwrap_or_default();
        return Ok((layer, info.extent.map(|b| (b[0], b[1], b[2], b[3])), metadata));
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
    for unsupported in ["SQL", "SpatialSQL", "Filter", "Overview", "GeoTransform"] {
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
        // The writer checks these before creating its destination, so overwriting a source
        // tile uses a completed in-memory read instead of truncating a tile still needed.
        let catalog = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut paths = Vec::new();
        let field = catalog.schema.schema.field_index(catalog.options.location_field.as_deref().unwrap_or("location")).unwrap();
        let mut add = |layer: &Layer| {
            for feature in &layer.features {
                if let Some(path) = feature.attributes.get(field).and_then(FieldValue::as_str)
                    && !path.contains("://")
                    && !path.starts_with("/vsi")
                {
                    paths.push(if Path::new(path).is_absolute() {
                        path.into()
                    } else {
                        catalog.base.join(path).to_string_lossy().into_owned()
                    });
                }
            }
        };
        match &catalog.index {
            Index::Memory { layer, .. } => add(layer),
            Index::File { path, layer } => {
                if let Ok(readers) =
                    crate::stream::open_layers(path, &VectorOpenOptions { layers: vec![layer.clone()], ..Default::default() }, 4096)
                {
                    for mut reader in readers {
                        while let Ok(Some(chunk)) = reader.read_chunk(4096) {
                            add(&chunk);
                        }
                    }
                }
                paths.push(path.clone());
            }
        }
        paths
    }
    fn read_rows(&self, window: Window, bands: &[usize]) -> Result<RasterData> {
        Ok(self.0.lock().map_err(|_| bad("catalog lock poisoned"))?.read_window(window, bands)?.raster.data)
    }
}

struct MemoryTile {
    datasets: Arc<HashMap<String, RasterDataset>>,
    name: String,
}
impl WindowSource for MemoryTile {
    fn overviews(&self) -> Vec<(usize, usize)> {
        Vec::new()
    }
    fn read(&mut self, level: usize, window: Window, bands: &[usize]) -> Result<RasterData> {
        if level != 0 {
            return Err(bad("in-memory catalog tiles have no overviews"));
        }
        Ok(crate::cog::cut(&self.datasets[&self.name].raster, window, bands))
    }
}

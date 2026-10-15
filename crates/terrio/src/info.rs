//! Dataset summaries, like `gdalinfo` and `ogrinfo`: [`info`] for an open dataset, [`info_path`]
//! for a path (reading GeoTIFFs and point clouds by their headers). Summaries print as text
//! ([`Info::to_text`]) and serialize to JSON.

use crate::LazyRaster;
use crate::crs::{SpatialRef, Transformer};
use crate::dataset::{Dataset, RasterDataset, VectorDataset};
use serde::Serialize;
use std::fmt::Write as _;

pub use crate::histogram::Histogram;

/// A summary of a dataset.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Info {
    /// A raster summary.
    Raster(Box<RasterInfo>),
    /// A vector summary.
    Vector(VectorInfo),
    /// A point cloud summary.
    #[serde(rename = "pointcloud")]
    PointCloud(Box<crate::pointcloud::info::PointCloudInfo>),
}

/// A raster summary.
#[derive(Debug, Clone, Serialize)]
pub struct RasterInfo {
    /// The driver's short name.
    pub driver: String,
    /// Width and height in pixels.
    pub size: [usize; 2],
    /// The number of bands.
    pub band_count: usize,
    /// The geotransform.
    pub geo_transform: [f64; 6],
    /// The CRS.
    pub crs: CrsSummary,
    /// The bounds `[min_x, min_y, max_x, max_y]` in the raster's CRS.
    pub extent: [f64; 4],
    /// The bounds in WGS 84 longitude/latitude, when the CRS is known.
    pub wgs84_extent: Option<[f64; 4]>,
    /// The bands.
    pub bands: Vec<BandInfo>,
    /// The overview sizes `[width, height]`, largest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub overviews: Vec<[usize; 2]>,
    /// Metadata key/value pairs.
    pub metadata: Vec<(String, String)>,
    /// The rasters the file holds besides this one, as GDAL's subdatasets.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subdatasets: Vec<Subdataset>,
}

/// A dataset inside a file (GDAL's subdatasets): a raster table of a GeoPackage, a netCDF
/// variable, an HDF5 dataset.
#[derive(Debug, Clone, Serialize)]
pub struct Subdataset {
    /// The name that opens it, such as `GPKG:file.gpkg:table`.
    pub name: String,
    /// A description.
    pub description: String,
}

/// One band's summary.
#[derive(Debug, Clone, Serialize)]
pub struct BandInfo {
    /// The 1-based band number.
    pub band: usize,
    /// The data type, named as GDAL names it (`Byte`, `Int16`, `Float32`).
    pub data_type: String,
    /// The nodata value, if any. NaN and infinities serialize as `"nan"`, `"inf"` and `"-inf"`.
    #[serde(serialize_with = "serialize_nodata")]
    pub nodata: Option<f64>,
    /// Statistics, when requested.
    pub statistics: Option<Statistics>,
    /// The histogram, when requested: `gdalinfo -hist`'s (see [`InfoOptions::histogram`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub histogram: Option<Histogram>,
}

fn serialize_nodata<S: serde::Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        None => s.serialize_none(),
        Some(x) if x.is_nan() => s.serialize_str("nan"),
        Some(x) if x.is_infinite() => s.serialize_str(if *x > 0.0 { "inf" } else { "-inf" }),
        Some(x) => s.serialize_f64(*x),
    }
}

/// Band statistics over the valid (not nodata) pixels.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Statistics {
    /// Minimum.
    pub min: f64,
    /// Maximum.
    pub max: f64,
    /// Mean.
    pub mean: f64,
    /// Population standard deviation.
    pub std_dev: f64,
    /// The number of valid pixels.
    pub valid_count: u64,
}

/// A CRS summary.
#[derive(Debug, Clone, Serialize)]
pub struct CrsSummary {
    /// The EPSG code.
    pub epsg: Option<u32>,
    /// The name.
    pub name: Option<String>,
    /// The WKT definition.
    pub wkt: Option<String>,
}

impl CrsSummary {
    pub(crate) fn new(s: &SpatialRef) -> Self {
        Self { epsg: s.epsg, name: s.name(), wkt: s.to_wkt() }
    }
}

/// A vector summary.
#[derive(Debug, Clone, Serialize)]
pub struct VectorInfo {
    /// The driver's short name.
    pub driver: String,
    /// The layers.
    pub layers: Vec<LayerInfo>,
    /// The rasters the file also holds, as GDAL's subdatasets.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub subdatasets: Vec<Subdataset>,
}

/// One layer's summary.
#[derive(Debug, Clone, Serialize)]
pub struct LayerInfo {
    /// The layer name.
    pub name: String,
    /// The geometry type, or `Unknown` when the layer mixes types.
    pub geometry_type: String,
    /// The number of features.
    pub feature_count: usize,
    /// The CRS.
    pub crs: CrsSummary,
    /// The bounds `[min_x, min_y, max_x, max_y]`, when there are geometries.
    pub extent: Option<[f64; 4]>,
    /// The attribute fields.
    pub fields: Vec<FieldInfo>,
}

/// An attribute field's summary.
#[derive(Debug, Clone, Serialize)]
pub struct FieldInfo {
    /// The field name.
    pub name: String,
    /// The field type.
    pub field_type: String,
}

/// Options for [`info`].
#[derive(Debug, Clone, Copy, Default)]
pub struct InfoOptions {
    /// Compute band statistics (reads every pixel).
    pub statistics: bool,
    /// Compute band histograms as `gdalinfo -hist` does (reads every pixel): 256 buckets over
    /// the type's range for Byte and Int8 bands, otherwise over GDAL's approximate minimum and
    /// maximum (or the exact ones when statistics are computed too) widened by half a bucket; a
    /// histogram stored in the `.aux.xml` sidecar is used as it is.
    pub histogram: bool,
}

/// Summarizes a dataset.
pub fn info(ds: &Dataset, options: &InfoOptions) -> Info {
    match ds {
        Dataset::Raster(r) => Info::Raster(Box::new(raster_info(r, options))),
        Dataset::Vector(v) => Info::Vector(vector_info(v)),
    }
}

/// Summarizes a raster dataset.
pub fn raster_info(ds: &RasterDataset, options: &InfoOptions) -> RasterInfo {
    raster_info_at(ds, options, None, None)
}

/// Summarizes a raster dataset read from `path` (whose sidecar files hold stored statistics and
/// histograms), with its lazily opened GeoTIFF (its blocks and overviews) when there is one.
fn raster_info_at(ds: &RasterDataset, options: &InfoOptions, path: Option<&str>, mut lazy: Option<&mut LazyRaster>) -> RasterInfo {
    let r = &ds.raster;
    let crs = ds.crs();
    let extent = [r.x_min, r.y_min, r.x_max(), r.y_max()];
    let bands = (0..r.bands)
        .map(|b| {
            let statistics = options.statistics.then(|| band_statistics(ds, b)).flatten();
            let histogram = options
                .histogram
                .then(|| {
                    let ctx = histogram_context(ds, b, path, lazy.as_deref_mut());
                    crate::histogram::default_histogram(ds, b, &ctx, statistics.map(|s| (s.min, s.max)))
                })
                .flatten();
            BandInfo { band: b + 1, data_type: gdal_type_name(r.data_type).to_string(), nodata: ds.nodata(), statistics, histogram }
        })
        .collect();
    RasterInfo {
        driver: ds.driver.name().to_string(),
        size: [r.cols, r.rows],
        band_count: r.bands,
        geo_transform: ds.geo_transform(),
        wgs84_extent: wgs84_extent(&crs, extent),
        crs: CrsSummary::new(&crs),
        extent,
        bands,
        overviews: Vec::new(),
        metadata: r.metadata.iter().filter(|(k, _)| subdataset_index(k).is_none()).cloned().collect(),
        subdatasets: subdatasets_from_metadata(&r.metadata),
    }
}

/// The index and kind (`NAME` or `DESC`) of a `SUBDATASET_N_NAME` metadata key.
fn subdataset_index(key: &str) -> Option<(usize, bool)> {
    let rest = key.strip_prefix("SUBDATASET_")?;
    let (n, kind) = rest.split_once('_')?;
    let is_name = match kind {
        "NAME" => true,
        "DESC" => false,
        _ => return None,
    };
    Some((n.parse().ok()?, is_name))
}

/// The subdatasets a driver listed as GDAL's `SUBDATASET_N_NAME`/`SUBDATASET_N_DESC` metadata.
fn subdatasets_from_metadata(metadata: &[(String, String)]) -> Vec<Subdataset> {
    let mut out: Vec<Subdataset> = Vec::new();
    for (k, v) in metadata {
        let Some((n, is_name)) = subdataset_index(k) else { continue };
        if n == 0 {
            continue;
        }
        while out.len() < n {
            out.push(Subdataset { name: String::new(), description: String::new() });
        }
        if is_name {
            out[n - 1].name = v.clone();
        } else {
            out[n - 1].description = v.clone();
        }
    }
    out.retain(|s| !s.name.is_empty());
    out
}

/// What GDAL knows of a band besides its pixels, for its default histogram: a histogram or range
/// stored with the dataset, its block size and the overview approximate statistics use.
fn histogram_context(ds: &RasterDataset, band: usize, path: Option<&str>, lazy: Option<&mut LazyRaster>) -> crate::histogram::BandContext {
    let mut ctx = crate::histogram::BandContext::default();
    let local = path.filter(|p| std::path::Path::new(p).is_file());
    if let Some(p) = local {
        ctx.stored_histogram = crate::histogram::pam_histogram(p, band + 1);
        ctx.stored_range = crate::histogram::stored_range(ds, p, band + 1);
    }
    match lazy {
        Some(l) if l.block_size().is_some() => {
            ctx.block = l.block_size();
            let overviews = l.overviews();
            if ctx.stored_range.is_none()
                && let Some(i) = crate::histogram::sample_overview(l.width(), l.height(), &overviews)
            {
                let (w, h) = overviews[i];
                let options = crate::WindowOptions { bands: vec![band + 1], overview: Some(i) };
                ctx.overview = l.read_window(crate::Window::full(w, h), &options).ok().map(|o| crate::ops::band_f64(&o.raster, 0));
            }
        }
        _ => ctx.block = crate::histogram::driver_block(ds),
    }
    ctx
}

/// Summarizes a raster opened lazily. Only statistics and histograms read the pixels.
pub fn lazy_raster_info(r: &mut LazyRaster, options: &InfoOptions) -> crate::Result<RasterInfo> {
    lazy_raster_info_at(r, options, None)
}

fn lazy_raster_info_at(r: &mut LazyRaster, options: &InfoOptions, path: Option<&str>) -> crate::Result<RasterInfo> {
    let overviews = r.overviews().into_iter().map(|(w, h)| [w, h]).collect();
    if options.histogram {
        let ds = r.read()?;
        let mut info = raster_info_at(&ds, options, path, Some(r));
        info.overviews = overviews;
        return Ok(info);
    }
    if options.statistics {
        // Strip by strip: a large raster is never in memory whole.
        let stats = lazy_band_statistics(r)?;
        let header = r.header();
        let mut info = raster_info_at(&header, &InfoOptions { statistics: false, ..*options }, path, None);
        for (band, s) in info.bands.iter_mut().zip(stats) {
            band.statistics = s;
        }
        info.overviews = overviews;
        return Ok(info);
    }
    let crs = r.crs();
    let gt = r.geo_transform();
    let (w, h) = (r.width() as f64, r.height() as f64);
    let extent = [gt[0], gt[3] + h * gt[5], gt[0] + w * gt[1], gt[3]];
    let bands = (0..r.band_count())
        .map(|b| BandInfo {
            band: b + 1,
            data_type: gdal_type_name(r.data_type()).to_string(),
            nodata: r.nodata(),
            statistics: None,
            histogram: None,
        })
        .collect();
    Ok(RasterInfo {
        driver: r.driver().name().to_string(),
        size: [r.width(), r.height()],
        band_count: r.band_count(),
        geo_transform: gt,
        wgs84_extent: wgs84_extent(&crs, extent),
        crs: CrsSummary::new(&crs),
        extent,
        bands,
        overviews,
        metadata: r.metadata(),
        subdatasets: Vec::new(),
    })
}

/// Summarizes the dataset at a path. GeoTIFFs (local or remote) are read in place, fetching only
/// their headers unless statistics are asked for; other datasets are read whole.
///
/// ```no_run
/// use terrio::info::{Info, InfoOptions, info_path};
///
/// let summary = info_path("https://example.com/cog.tif", &InfoOptions::default())?;
/// print!("{}", summary.to_text()); // as `terrio info` prints it
/// println!("{}", serde_json::to_string_pretty(&summary).unwrap()); // as `terrio info --json`
/// if let Info::Raster(r) = &summary {
///     println!("{} bands", r.bands.len());
/// }
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn info_path(path: &str, options: &InfoOptions) -> crate::Result<Info> {
    if let Some(result) = crate::stac::info(path, options) {
        return result;
    }
    if let Some(r) = crate::drivers::open_lazy(path) {
        return Ok(Info::Raster(Box::new(lazy_raster_info(&mut r?, options)?)));
    }
    if crate::pointcloud::is_point_cloud_path(path) {
        let mut r = crate::pointcloud::open_points(path)?;
        return Ok(Info::PointCloud(Box::new(crate::pointcloud::info::point_cloud_info(&mut r, options.statistics)?)));
    }
    #[cfg(feature = "gdal")]
    if crate::gdal_backend::strip_prefix(path).is_some() {
        return Ok(info(&crate::open(path)?, options));
    }
    // Tile archives (local or remote) are read in place: rasters by their headers only.
    let loc = crate::vfs::Location::parse(path);
    if !matches!(loc, crate::vfs::Location::Zip { .. }) && crate::tiles::archive_kind(&loc.file_name()).is_some() {
        let archive = crate::tiles::TileArchive::open(path)?;
        if archive.is_vector() {
            return Ok(info(&crate::tiles::archive_dataset(archive, &loc.file_name(), &Default::default())?, options));
        }
        let mut r = LazyRaster::from_tiles(crate::tiles::raster::TileRaster::new(archive, None)?);
        return Ok(Info::Raster(Box::new(lazy_raster_info(&mut r, options)?)));
    }
    if !matches!(loc, crate::vfs::Location::Zip { .. })
        && let Some(mut r) = LazyRaster::open_tiff(path)?
    {
        return Ok(Info::Raster(Box::new(lazy_raster_info_at(&mut r, options, Some(path))?)));
    }
    if let Some(v) = gpkg_vector_info(path)? {
        return Ok(Info::Vector(v));
    }
    if let Some(v) = osm_vector_info(path)? {
        return Ok(Info::Vector(v));
    }
    let mut summary = match crate::open(path)? {
        Dataset::Raster(r) => Info::Raster(Box::new(raster_info_at(&r, options, Some(path), None))),
        Dataset::Vector(v) => Info::Vector(vector_info(&v)),
    };
    // A GeoPackage's raster tables: all of them besides vector layers, or besides the first.
    if crate::formats::extension(path).as_deref() == Some("gpkg")
        && let Ok(local) = crate::vfs::resolve(path)
    {
        let subdatasets: Vec<Subdataset> = crate::drivers::gpkg::subdatasets(local.as_str())
            .into_iter()
            .map(|(name, description)| Subdataset { name, description })
            .collect();
        match &mut summary {
            Info::Vector(v) => v.subdatasets = subdatasets,
            Info::Raster(r) if subdatasets.len() > 1 => r.subdatasets = subdatasets,
            Info::Raster(_) | Info::PointCloud(_) => {}
        }
    }
    Ok(summary)
}

/// Computes a band's statistics over its valid pixels. `None` when every pixel is nodata.
pub fn band_statistics(ds: &RasterDataset, band: usize) -> Option<Statistics> {
    let mut acc = StatsAcc::new(ds.nodata());
    acc.add(ds.raster.band_view(band).as_slice());
    acc.finish()
}

/// Statistics accumulated over pixels given in order (Welford's method): adding a band's pixels
/// whole, or row by row in order, gives the same values.
struct StatsAcc {
    nodata: Option<f64>,
    n: u64,
    min: f64,
    max: f64,
    mean: f64,
    m2: f64,
}

impl StatsAcc {
    fn new(nodata: Option<f64>) -> Self {
        Self { nodata, n: 0, min: f64::INFINITY, max: f64::NEG_INFINITY, mean: 0.0, m2: 0.0 }
    }

    fn add(&mut self, values: &[f64]) {
        for &v in values {
            if v.is_nan() || self.nodata.is_some_and(|nd| v == nd) {
                continue;
            }
            self.n += 1;
            self.min = self.min.min(v);
            self.max = self.max.max(v);
            let d = v - self.mean;
            self.mean += d / self.n as f64;
            self.m2 += d * (v - self.mean);
        }
    }

    fn finish(&self) -> Option<Statistics> {
        (self.n > 0).then(|| Statistics {
            min: self.min,
            max: self.max,
            mean: self.mean,
            std_dev: (self.m2 / self.n as f64).sqrt(),
            valid_count: self.n,
        })
    }
}

/// Each band's statistics, read from a lazily opened raster a strip of rows at a time, in order,
/// so that only the strip is in memory: the values are those of [`band_statistics`] over the
/// whole raster.
fn lazy_band_statistics(r: &mut LazyRaster) -> crate::Result<Vec<Option<Statistics>>> {
    let (w, h, bands) = (r.width(), r.height(), r.band_count());
    // Whole blocks, about 64 MB of float64 values per strip.
    let block = r.block_size().map_or(256, |(_, bh)| bh.max(1));
    let rows = ((64 << 20) / (8 * w.max(1) * bands.max(1))).max(1).div_ceil(block) * block;
    let mut acc: Vec<StatsAcc> = (0..bands).map(|_| StatsAcc::new(r.nodata())).collect();
    let mut y = 0;
    while y < h {
        let n = rows.min(h - y);
        let strip = r.read_window(crate::cog::Window::new(0, y, w, n), &crate::cog::WindowOptions::default())?;
        for (b, a) in acc.iter_mut().enumerate() {
            a.add(strip.raster.band_view(b).as_slice());
        }
        y += n;
    }
    Ok(acc.iter().map(StatsAcc::finish).collect())
}

/// GDAL's name for a data type.
pub fn gdal_type_name(t: wbraster::DataType) -> &'static str {
    use wbraster::DataType as T;
    match t {
        T::U8 => "Byte",
        T::I8 => "Int8",
        T::U16 => "UInt16",
        T::I16 => "Int16",
        T::U32 => "UInt32",
        T::I32 => "Int32",
        T::U64 => "UInt64",
        T::I64 => "Int64",
        T::F32 => "Float32",
        T::F64 => "Float64",
    }
}

/// Summarizes a vector dataset.
pub fn vector_info(ds: &VectorDataset) -> VectorInfo {
    // LIBKML reports a container's bounds, which take in its nested containers' features too.
    let kml = matches!(ds.driver, crate::formats::VectorDriver::KML | crate::formats::VectorDriver::KMZ);
    let extent = |l: &wbvector::Layer| match (&l.extent, kml) {
        (Some(b), true) => Some((b.min_x, b.min_y, b.max_x, b.max_y)),
        _ => crate::vector::layer_extent(l),
    };
    let layers = ds.layers.iter().map(|l| layer_info(l, l.len(), crate::vector::geometry_type_name(l), extent(l))).collect();
    VectorInfo { driver: ds.driver.name().to_string(), layers, subdatasets: Vec::new() }
}

/// Summarizes a layer, given its feature count, geometry type name and extent.
fn layer_info(l: &wbvector::Layer, feature_count: usize, geometry_type: String, extent: Option<(f64, f64, f64, f64)>) -> LayerInfo {
    let crs = SpatialRef::from_layer(l);
    LayerInfo {
        name: l.name.clone(),
        geometry_type,
        feature_count,
        crs: CrsSummary::new(&crs),
        extent: extent.map(|b| [b.0, b.1, b.2, b.3]),
        fields: l
            .schema
            .fields()
            .iter()
            .map(|f| FieldInfo { name: f.name.clone(), field_type: f.field_type.as_str().to_string() })
            .collect(),
    }
}

/// Summarizes the vector layers of a local GeoPackage without reading their attributes. `None`
/// when it is not a local GeoPackage file with vector layers.
fn gpkg_vector_info(path: &str) -> crate::Result<Option<VectorInfo>> {
    if crate::formats::extension(path).as_deref() != Some("gpkg")
        || !matches!(crate::vfs::Location::parse(path), crate::vfs::Location::Local(_))
        || !std::path::Path::new(path).is_file()
    {
        return Ok(None);
    }
    let Some((layers, rasters)) = crate::drivers::vector::gpkg_read::summarize(path)? else { return Ok(None) };
    let layers = layers.into_iter().map(|s| layer_info(&s.layer, s.feature_count, s.geometry_type, s.extent)).collect();
    // The raster tables, as `drivers::gpkg::subdatasets` names them.
    let subdatasets = rasters
        .into_iter()
        .map(|c| {
            let desc = if c.description.is_empty() { c.name.clone() } else { c.description.clone() };
            Subdataset { name: format!("GPKG:{path}:{}", c.name), description: format!("{} - {desc}", c.name) }
        })
        .collect();
    Ok(Some(VectorInfo { driver: crate::formats::VectorDriver::GPKG.name().to_string(), layers, subdatasets }))
}

/// Summarizes the layers of a local OpenStreetMap PBF file sorted by type then id a block at a
/// time, as [`vector_info`] summarizes them read whole. `None` for other files.
#[cfg(not(target_arch = "wasm32"))]
fn osm_vector_info(path: &str) -> crate::Result<Option<VectorInfo>> {
    use crate::drivers::vector::osm;
    if crate::formats::extension(path).as_deref() != Some("pbf")
        || crate::formats::VectorDriver::for_extension(path) != Some(crate::formats::VectorDriver::OSM)
        || !matches!(crate::vfs::Location::parse(path), crate::vfs::Location::Local(_))
        || !std::path::Path::new(path).is_file()
    {
        return Ok(None);
    }
    let mut file = std::fs::File::open(path)?;
    if !osm::is_sorted_pbf(&mut file) {
        return Ok(None);
    }
    let schemas = osm::layer_schemas();
    /// What a layer's summary needs of its features.
    #[derive(Clone, Default)]
    struct Acc {
        count: usize,
        extent: Option<(f64, f64, f64, f64)>,
        geometries: usize,
        /// One of the first 100 geometries has Z coordinates.
        has_z: bool,
    }
    let mut acc = vec![Acc::default(); schemas.len()];
    let read = osm::read_sorted_pbf(&mut file, [true; 5], &mut |l, features| {
        let a = &mut acc[l];
        a.count += features.len();
        let boxes = features.iter().filter_map(|f| f.geometry.as_ref()?.bbox()).map(|b| (b.min_x, b.min_y, b.max_x, b.max_y));
        a.extent = crate::vector::extent_of(a.extent.into_iter().chain(boxes));
        for g in features.iter().filter_map(|f| f.geometry.as_ref()) {
            if a.geometries < 100 {
                a.has_z |= crate::vector::geometry_has_z(g);
            }
            a.geometries += 1;
        }
        Ok(())
    });
    match read {
        // A file not sorted as its header says is summarized read whole.
        Err(e) if osm::is_unsorted(&e) => return Ok(None),
        r => r?,
    };
    let layers = schemas
        .iter()
        .zip(acc)
        .map(|(l, a)| {
            let types = l.geom_type.into_iter().take(usize::from(a.geometries > 0));
            layer_info(l, a.count, crate::vector::geometry_type_name_of(l.geom_type, types, a.has_z), a.extent)
        })
        .collect();
    Ok(Some(VectorInfo { driver: crate::formats::VectorDriver::OSM.name().to_string(), layers, subdatasets: Vec::new() }))
}

#[cfg(target_arch = "wasm32")]
fn osm_vector_info(_path: &str) -> crate::Result<Option<VectorInfo>> {
    Ok(None)
}

fn wgs84_extent(crs: &SpatialRef, e: [f64; 4]) -> Option<[f64; 4]> {
    if crs.is_unknown() {
        return None;
    }
    let wgs84 = SpatialRef::from_epsg(4326).ok()?;
    let t = Transformer::new(crs, &wgs84).ok()?;
    let b = t.transform_bounds((e[0], e[1], e[2], e[3])).ok()?;
    Some([b.0, b.1, b.2, b.3])
}

impl Info {
    /// The summary as text, in the spirit of `gdalinfo` / `ogrinfo`.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        match self {
            Info::Raster(r) => {
                let _ = writeln!(s, "Driver: {}", r.driver);
                let _ = writeln!(s, "Size: {} x {}, {} band(s)", r.size[0], r.size[1], r.band_count);
                let _ = writeln!(s, "CRS: {}", crs_line(&r.crs));
                let g = r.geo_transform;
                let _ = writeln!(s, "Origin: ({}, {})", g[0], g[3]);
                let _ = writeln!(s, "Pixel size: ({}, {})", g[1], g[5]);
                let _ = writeln!(s, "Extent: {}", fmt_box(&r.extent));
                if let Some(w) = &r.wgs84_extent {
                    let _ = writeln!(s, "WGS 84 extent: {}", fmt_box(w));
                }
                if !r.overviews.is_empty() {
                    let sizes: Vec<String> = r.overviews.iter().map(|[w, h]| format!("{w}x{h}")).collect();
                    let _ = writeln!(s, "Overviews: {}", sizes.join(", "));
                }
                for (k, v) in &r.metadata {
                    let _ = writeln!(s, "Metadata: {k}={v}");
                }
                write_subdatasets(&mut s, &r.subdatasets);
                for b in &r.bands {
                    let _ = write!(s, "Band {}: {}", b.band, b.data_type);
                    if let Some(nd) = b.nodata {
                        let _ = write!(s, ", nodata={nd}");
                    }
                    let _ = writeln!(s);
                    if let Some(st) = &b.statistics {
                        let _ = writeln!(
                            s,
                            "  min={} max={} mean={:.6} std_dev={:.6} valid={}",
                            st.min, st.max, st.mean, st.std_dev, st.valid_count
                        );
                    }
                    if let Some(h) = &b.histogram {
                        s.push_str(&h.to_text());
                    }
                }
            }
            Info::PointCloud(p) => s.push_str(&p.to_text()),
            Info::Vector(v) => {
                let _ = writeln!(s, "Driver: {}", v.driver);
                for l in &v.layers {
                    let _ = writeln!(s, "\nLayer: {}", l.name);
                    let _ = writeln!(s, "Geometry: {}", l.geometry_type);
                    let _ = writeln!(s, "Features: {}", l.feature_count);
                    let _ = writeln!(s, "CRS: {}", crs_line(&l.crs));
                    if let Some(e) = &l.extent {
                        let _ = writeln!(s, "Extent: {}", fmt_box(e));
                    }
                    for f in &l.fields {
                        let _ = writeln!(s, "  {}: {}", f.name, f.field_type);
                    }
                }
                if !v.subdatasets.is_empty() {
                    let _ = writeln!(s);
                }
                write_subdatasets(&mut s, &v.subdatasets);
            }
        }
        s
    }
}

fn write_subdatasets(s: &mut String, subdatasets: &[Subdataset]) {
    if subdatasets.is_empty() {
        return;
    }
    let _ = writeln!(s, "Subdatasets:");
    for (i, d) in subdatasets.iter().enumerate() {
        let _ = writeln!(s, "  SUBDATASET_{}_NAME={}", i + 1, d.name);
        let _ = writeln!(s, "  SUBDATASET_{}_DESC={}", i + 1, d.description);
    }
}

fn crs_line(c: &CrsSummary) -> String {
    match (c.epsg, &c.name) {
        (Some(code), Some(name)) => format!("EPSG:{code} ({name})"),
        (Some(code), None) => format!("EPSG:{code}"),
        (None, Some(name)) => name.clone(),
        (None, None) => "unknown".into(),
    }
}

fn fmt_box(b: &[f64; 4]) -> String {
    format!("({}, {}) - ({}, {})", b[0], b[1], b[2], b[3])
}

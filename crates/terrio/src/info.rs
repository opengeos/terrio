//! Dataset summaries, like `gdalinfo` and `ogrinfo`.

use crate::LazyRaster;
use crate::crs::{SpatialRef, Transformer};
use crate::dataset::{Dataset, RasterDataset, VectorDataset};
use serde::Serialize;
use std::fmt::Write as _;

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
    let r = &ds.raster;
    let crs = ds.crs();
    let extent = [r.x_min, r.y_min, r.x_max(), r.y_max()];
    let bands = (0..r.bands)
        .map(|b| BandInfo {
            band: b + 1,
            data_type: gdal_type_name(r.data_type).to_string(),
            nodata: ds.nodata(),
            statistics: options.statistics.then(|| band_statistics(ds, b)).flatten(),
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
        metadata: r.metadata.clone(),
    }
}

/// Summarizes a raster opened lazily. Only statistics read the pixels.
pub fn lazy_raster_info(r: &mut LazyRaster, options: &InfoOptions) -> crate::Result<RasterInfo> {
    let overviews = r.overviews().into_iter().map(|(w, h)| [w, h]).collect();
    if options.statistics {
        let mut info = raster_info(&r.read()?, options);
        info.overviews = overviews;
        return Ok(info);
    }
    let crs = r.crs();
    let gt = r.geo_transform();
    let (w, h) = (r.width() as f64, r.height() as f64);
    let extent = [gt[0], gt[3] + h * gt[5], gt[0] + w * gt[1], gt[3]];
    let bands = (0..r.band_count())
        .map(|b| BandInfo { band: b + 1, data_type: gdal_type_name(r.data_type()).to_string(), nodata: r.nodata(), statistics: None })
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
        metadata: Vec::new(),
    })
}

/// Summarizes the dataset at a path. GeoTIFFs (local or remote) are read in place, fetching only
/// their headers unless statistics are asked for; other datasets are read whole.
pub fn info_path(path: &str, options: &InfoOptions) -> crate::Result<Info> {
    if crate::pointcloud::is_point_cloud_path(path) {
        let mut r = crate::pointcloud::open_points(path)?;
        return Ok(Info::PointCloud(Box::new(crate::pointcloud::info::point_cloud_info(&mut r, options.statistics)?)));
    }
    if !matches!(crate::vfs::Location::parse(path), crate::vfs::Location::Zip { .. })
        && let Some(mut r) = LazyRaster::open_tiff(path)?
    {
        return Ok(Info::Raster(Box::new(lazy_raster_info(&mut r, options)?)));
    }
    Ok(info(&crate::open(path)?, options))
}

/// Computes a band's statistics over its valid pixels. `None` when every pixel is nodata.
pub fn band_statistics(ds: &RasterDataset, band: usize) -> Option<Statistics> {
    let nodata = ds.nodata();
    let (mut n, mut min, mut max, mut mean, mut m2) = (0u64, f64::INFINITY, f64::NEG_INFINITY, 0.0, 0.0);
    for v in ds.raster.band_view(band).as_slice().iter().copied() {
        if v.is_nan() || nodata.is_some_and(|nd| v == nd) {
            continue;
        }
        n += 1;
        min = min.min(v);
        max = max.max(v);
        let d = v - mean;
        mean += d / n as f64;
        m2 += d * (v - mean);
    }
    (n > 0).then(|| Statistics { min, max, mean, std_dev: (m2 / n as f64).sqrt(), valid_count: n })
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
    let layers = ds
        .layers
        .iter()
        .map(|l| {
            let crs = SpatialRef::from_layer(l);
            LayerInfo {
                name: l.name.clone(),
                geometry_type: crate::vector::geometry_type_name(l),
                feature_count: l.len(),
                crs: CrsSummary::new(&crs),
                extent: crate::vector::layer_extent(l).map(|b| [b.0, b.1, b.2, b.3]),
                fields: l
                    .schema
                    .fields()
                    .iter()
                    .map(|f| FieldInfo { name: f.name.clone(), field_type: f.field_type.as_str().to_string() })
                    .collect(),
            }
        })
        .collect();
    VectorInfo { driver: ds.driver.name().to_string(), layers }
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
            }
        }
        s
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

//! Point clouds (LAS, LAZ, COPC, PLY, E57) from bytes.

use super::{Dataset, Inner, crs_string, js_err, to_js};
use serde::Deserialize;
use terrio::pointcloud::filters::{self, DimRange, OutlierMethod, OutlierOptions, SampleOptions, SmrfOptions, VoxelMode};
use terrio::pointcloud::grid::{GridMethod, GridOptions, GridStat};
use terrio::pointcloud::{
    self, PointCloud, PointCloudReader, PointFilter, PointFormat, PointReadOptions, PointWriteOptions, ReturnKind, Values,
};
use terrio::{LazyRaster, VectorWriteOptions};
use wasm_bindgen::prelude::*;

/// Options for reading points, as a JavaScript object.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct ReadOptions {
    /// Keep the points in `[minX, minY, maxX, maxY]`.
    bounds: Option<[f64; 4]>,
    /// Keep the points with z in `[min, max]`.
    z_range: Option<[f64; 2]>,
    /// Keep the points of these classifications.
    classes: Vec<u8>,
    /// Keep these returns: `first`, `last`, `intermediate`, `only`.
    returns: Vec<String>,
    /// COPC: the deepest octree level to read.
    max_level: Option<u32>,
    /// COPC: read the levels needed for this point spacing.
    resolution: Option<f64>,
    /// At most about this many points.
    max_points: Option<u64>,
}

impl ReadOptions {
    fn parse(options: JsValue) -> Result<PointReadOptions, JsError> {
        let o: ReadOptions = if options.is_undefined() || options.is_null() {
            ReadOptions::default()
        } else {
            let text: String = js_sys::JSON::stringify(&options).map_err(|_| JsError::new("invalid options"))?.into();
            serde_json::from_str(&text).map_err(|e| JsError::new(&format!("options: {e}")))?
        };
        Ok(PointReadOptions {
            filter: PointFilter {
                bounds: o.bounds,
                z_range: o.z_range.map(|z| (z[0], z[1])),
                classes: o.classes,
                returns: o.returns.iter().map(|r| ReturnKind::parse(r)).collect::<terrio::Result<_>>().map_err(js_err)?,
            },
            max_level: o.max_level,
            resolution: o.resolution,
            max_points: o.max_points,
        })
    }
}

/// Options for gridding, as a JavaScript object.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct JsGridOptions {
    method: String,
    stat: Vec<String>,
    dimension: String,
    radius: Option<f64>,
    power: f64,
    window_size: usize,
    bin_mode: bool,
    bounds: Option<[f64; 4]>,
    nodata: f64,
}

impl Default for JsGridOptions {
    fn default() -> Self {
        Self {
            method: "statistics".into(),
            stat: Vec::new(),
            dimension: "z".into(),
            radius: None,
            power: 1.0,
            window_size: 0,
            bin_mode: false,
            bounds: None,
            nodata: -9999.0,
        }
    }
}

/// Parses an options object, or the defaults when it is undefined.
fn parse<T: for<'de> Deserialize<'de> + Default>(options: JsValue) -> Result<T, JsError> {
    if options.is_undefined() || options.is_null() {
        return Ok(T::default());
    }
    let text: String = js_sys::JSON::stringify(&options).map_err(|_| JsError::new("invalid options"))?.into();
    serde_json::from_str(&text).map_err(|e| JsError::new(&format!("options: {e}")))
}

/// Options of `PointCloud.smrf`, as a JavaScript object (PDAL's `filters.smrf` options).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct JsSmrfOptions {
    cell: f64,
    slope: f64,
    window: Option<f64>,
    scalar: f64,
    threshold: f64,
    cut: f64,
    ignore: Option<String>,
    returns: Vec<String>,
    ground_class: u8,
    other_class: u8,
    only_ground: bool,
}

impl Default for JsSmrfOptions {
    fn default() -> Self {
        let d = SmrfOptions::default();
        Self {
            cell: d.cell,
            slope: d.slope,
            window: d.window,
            scalar: d.scalar,
            threshold: d.threshold,
            cut: d.cut,
            ignore: None,
            returns: vec!["last".into(), "only".into()],
            ground_class: d.ground_class,
            other_class: d.other_class,
            only_ground: d.only_ground,
        }
    }
}

/// Options of `PointCloud.outlier`, as a JavaScript object (PDAL's `filters.outlier` options).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct JsOutlierOptions {
    method: String,
    min_k: usize,
    radius: f64,
    mean_k: usize,
    multiplier: f64,
    classification: u8,
}

impl Default for JsOutlierOptions {
    fn default() -> Self {
        let d = OutlierOptions::default();
        Self {
            method: "statistical".into(),
            min_k: d.min_k,
            radius: d.radius,
            mean_k: d.mean_k,
            multiplier: d.multiplier,
            classification: d.class,
        }
    }
}

/// An open LAS, LAZ, COPC, PLY or E57 file: its header is read, its points on demand.
#[wasm_bindgen]
pub struct PointCloudFile {
    inner: PointCloudReader,
}

/// Points in memory: one typed array per dimension.
#[wasm_bindgen(js_name = PointCloud)]
pub struct JsPointCloud {
    inner: PointCloud,
}

/// Opens a LAS, LAZ, COPC, PLY or E57 file from its bytes, reading only its header.
#[wasm_bindgen(js_name = openPoints)]
pub fn open_points(bytes: Vec<u8>, filename: &str) -> Result<PointCloudFile, JsError> {
    Ok(PointCloudFile { inner: pointcloud::open_points_bytes(bytes, filename).map_err(js_err)? })
}

/// Reads the points of a LAS, LAZ, COPC, PLY or E57 file from its bytes. `options`: `bounds`, `zRange`,
/// `classes`, `returns`, `maxLevel`, `resolution`, `maxPoints`.
#[wasm_bindgen(js_name = readPoints)]
pub fn read_points(bytes: Vec<u8>, filename: &str, options: JsValue) -> Result<JsPointCloud, JsError> {
    open_points(bytes, filename)?.read(options)
}

/// Makes points from typed arrays: `{ x, y, z, intensity, classification, ... }`, with `x`, `y`
/// and `z` at least. Other arrays than the standard LAS dimensions become extra dimensions of
/// their type.
#[wasm_bindgen(js_name = pointsFromArrays)]
pub fn points_from_arrays(arrays: &js_sys::Object, crs: Option<String>) -> Result<JsPointCloud, JsError> {
    let mut dims = Vec::new();
    for entry in js_sys::Object::entries(arrays).iter() {
        let pair = js_sys::Array::from(&entry);
        let name = pair.get(0).as_string().unwrap_or_default();
        let a = pair.get(1);
        macro_rules! try_types {
            ($($t:ident => $v:ident),*) => {
                'found: {
                    $(if let Some(x) = a.dyn_ref::<js_sys::$t>() {
                        break 'found Some(Values::$v(x.to_vec()));
                    })*
                    if let Some(x) = a.dyn_ref::<js_sys::Array>() {
                        break 'found Some(Values::F64(x.iter().map(|v| v.as_f64().unwrap_or(f64::NAN)).collect()));
                    }
                    None
                }
            };
        }
        let values = try_types!(
            Uint8Array => U8, Int8Array => I8, Uint16Array => U16, Int16Array => I16, Uint32Array => U32,
            Int32Array => I32, BigUint64Array => U64, BigInt64Array => I64, Float32Array => F32, Float64Array => F64
        )
        .ok_or_else(|| JsError::new(&format!("'{name}' is not a typed array or an array")))?;
        dims.push(terrio::pointcloud::Dimension::new(&name, values));
    }
    for d in &mut dims {
        if let Some(t) = pointcloud::standard_value_type(&d.name) {
            d.values = d.values.cast(t);
        }
    }
    for c in ["x", "y", "z"] {
        if !dims.iter().any(|d| d.name == c) {
            return Err(JsError::new(&format!("no '{c}' array")));
        }
    }
    let n = dims[0].values.len();
    if dims.iter().any(|d| d.values.len() != n) {
        return Err(JsError::new("the arrays differ in length"));
    }
    let crs = crs.as_deref().map(terrio::crs::SpatialRef::parse).transpose().map_err(js_err)?.unwrap_or_default();
    Ok(JsPointCloud { inner: pointcloud::from_dimensions(dims, crs) })
}

#[wasm_bindgen]
impl PointCloudFile {
    /// `"LAS"`, `"LAZ"`, `"COPC"`, `"PLY"` or `"E57"`.
    #[wasm_bindgen(getter)]
    pub fn format(&self) -> String {
        self.inner.format_name().into()
    }

    /// The number of points.
    #[wasm_bindgen(getter, js_name = pointCount)]
    pub fn point_count(&self) -> f64 {
        self.inner.header().point_count as f64
    }

    /// The CRS: `EPSG:xxxx` when its code is known, otherwise its WKT.
    #[wasm_bindgen(getter)]
    pub fn crs(&self) -> Option<String> {
        crs_string(self.inner.crs())
    }

    /// The header.
    #[wasm_bindgen(getter)]
    pub fn header(&self) -> Result<JsValue, JsError> {
        to_js(self.inner.header())
    }

    /// The summary `terrio info --json` prints; `stats` reads every point.
    pub fn info(&mut self, stats: Option<bool>) -> Result<JsValue, JsError> {
        to_js(&pointcloud::info::point_cloud_info(&mut self.inner, stats.unwrap_or(false)).map_err(js_err)?)
    }

    /// Reads the points. `options`: `bounds`, `zRange`, `classes`, `returns`, `maxLevel`,
    /// `resolution`, `maxPoints`.
    pub fn read(&mut self, options: JsValue) -> Result<JsPointCloud, JsError> {
        let options = ReadOptions::parse(options)?;
        Ok(JsPointCloud { inner: self.inner.read(&options).map_err(js_err)? })
    }
}

#[wasm_bindgen(js_class = PointCloud)]
impl JsPointCloud {
    /// The number of points.
    #[wasm_bindgen(getter)]
    pub fn length(&self) -> usize {
        self.inner.len()
    }

    /// The dimension names.
    #[wasm_bindgen(getter)]
    pub fn dimensions(&self) -> Vec<String> {
        self.inner.dimensions.iter().map(|d| d.name.clone()).collect()
    }

    /// The CRS: `EPSG:xxxx` when its code is known, otherwise its WKT.
    #[wasm_bindgen(getter)]
    pub fn crs(&self) -> Option<String> {
        crs_string(&self.inner.crs)
    }

    /// The bounds of the points: `[minX, minY, minZ, maxX, maxY, maxZ]`.
    #[wasm_bindgen(getter)]
    pub fn bounds(&self) -> Option<Vec<f64>> {
        self.inner.bounds().map(|b| b.to_vec())
    }

    /// A dimension's values as a typed array of its type (`Float64Array` for `x`, `y`, `z` and
    /// scaled extra dimensions).
    pub fn get(&self, name: &str) -> Result<JsValue, JsError> {
        let d = self.inner.dim(name).ok_or_else(|| JsError::new(&format!("no dimension '{name}'")))?;
        let values = if d.scale.is_some() || d.offset.is_some() { Values::F64(d.to_f64()) } else { d.values.clone() };
        Ok(match values {
            Values::U8(v) => js_sys::Uint8Array::from(&v[..]).into(),
            Values::I8(v) => js_sys::Int8Array::from(&v[..]).into(),
            Values::U16(v) => js_sys::Uint16Array::from(&v[..]).into(),
            Values::I16(v) => js_sys::Int16Array::from(&v[..]).into(),
            Values::U32(v) => js_sys::Uint32Array::from(&v[..]).into(),
            Values::I32(v) => js_sys::Int32Array::from(&v[..]).into(),
            Values::U64(v) => js_sys::BigUint64Array::from(&v[..]).into(),
            Values::I64(v) => js_sys::BigInt64Array::from(&v[..]).into(),
            Values::F32(v) => js_sys::Float32Array::from(&v[..]).into(),
            Values::F64(v) => js_sys::Float64Array::from(&v[..]).into(),
        })
    }

    /// The points as a LAS, LAZ, COPC, PLY or E57 file: `filename`'s extension (`.las`, `.laz`,
    /// `.copc.laz`, `.ply`, `.e57`) tells the format unless `format` does; `pointFormat` (0 to
    /// 10).
    #[wasm_bindgen(js_name = toBytes)]
    pub fn to_bytes(&self, filename: &str, format: Option<String>, point_format: Option<u8>) -> Result<Vec<u8>, JsError> {
        let format = format
            .map(|f| PointFormat::from_name(&f).ok_or_else(|| JsError::new(&format!("unknown point cloud format '{f}'"))))
            .transpose()?;
        let options = PointWriteOptions { format, point_format, ..Default::default() };
        pointcloud::write_points_bytes(&self.inner, filename, &options).map_err(js_err)
    }

    /// The points as a vector file (`points.fgb`, `points.geojson`...), one point feature each
    /// with the dimensions as attributes (all, or `dimensions`).
    #[wasm_bindgen(js_name = toVectorBytes)]
    pub fn to_vector_bytes(&self, filename: &str, dimensions: Option<Vec<String>>) -> Result<Vec<u8>, JsError> {
        let options = pointcloud::vector::ToLayerOptions { dimensions: dimensions.unwrap_or_default(), ..Default::default() };
        let layer = pointcloud::vector::to_layer(&self.inner, &options).map_err(js_err)?;
        terrio::memory::write_vector_bytes(&[layer], filename, &VectorWriteOptions::default()).map_err(js_err)
    }

    /// Grids the points into a raster, as PDAL's `writers.gdal` does, or interpolated as
    /// `gdal_grid` does. `options`: `method` (`statistics`, the default; `linear`, a TIN; or
    /// `nearest`), `stat` (a list of `min`, `max`, `mean`, `idw`, `count`, `stdev`; `mean` by
    /// default), `dimension`, `radius`, `power`, `windowSize`, `binMode`, `bounds`, `nodata`.
    pub fn grid(&self, resolution: f64, options: JsValue) -> Result<Dataset, JsError> {
        let o: JsGridOptions = parse(options)?;
        let stats = o.stat.iter().map(|s| GridStat::parse(s)).collect::<terrio::Result<_>>().map_err(js_err)?;
        let options = GridOptions {
            resolution,
            method: GridMethod::parse(&o.method).map_err(js_err)?,
            stats,
            dimension: o.dimension,
            radius: o.radius,
            power: o.power,
            window_size: o.window_size,
            bin_mode: o.bin_mode,
            bounds: o.bounds,
            nodata: o.nodata,
            data_type: terrio::DataType::F64,
        };
        let ds = pointcloud::grid::grid(&self.inner, &options).map_err(js_err)?;
        Ok(Dataset { inner: Inner::Raster(LazyRaster::from(ds)) })
    }

    /// The points within ranges of dimensions, as PDAL's `filters.range`: `limits` such as
    /// `"Classification[2:2],Z[0:100)"`.
    pub fn range(&self, limits: &str) -> Result<JsPointCloud, JsError> {
        let ranges = DimRange::parse_list(limits).map_err(js_err)?;
        Ok(JsPointCloud { inner: filters::range(&self.inner, &ranges).map_err(js_err)? })
    }

    /// Poisson sampling, as PDAL's `filters.sample`: the points at least `radius` from those
    /// kept before them.
    pub fn sample(&self, radius: f64) -> Result<JsPointCloud, JsError> {
        let o = SampleOptions { radius: Some(radius), ..Default::default() };
        Ok(JsPointCloud { inner: filters::sample(&self.inner, &o).map_err(js_err)? })
    }

    /// The first point of each voxel, as PDAL's `filters.voxeldownsize`; `mode` is `center`
    /// (the default) or `first`.
    #[wasm_bindgen(js_name = voxelDownsize)]
    pub fn voxel_downsize(&self, cell: f64, mode: Option<String>) -> Result<JsPointCloud, JsError> {
        let mode = VoxelMode::parse(mode.as_deref().unwrap_or("center")).map_err(js_err)?;
        Ok(JsPointCloud { inner: filters::voxel_downsize(&self.inner, cell, mode).map_err(js_err)? })
    }

    /// The point nearest the center of each voxel, as PDAL's
    /// `filters.voxelcenternearestneighbor`.
    #[wasm_bindgen(js_name = voxelCenterNearestNeighbor)]
    pub fn voxel_center_nearest_neighbor(&self, cell: f64) -> Result<JsPointCloud, JsError> {
        Ok(JsPointCloud { inner: filters::voxel_center_nearest_neighbor(&self.inner, cell).map_err(js_err)? })
    }

    /// Classifies outliers as noise, as PDAL's `filters.outlier`. `options`: `method`
    /// (`statistical` or `radius`), `minK`, `radius`, `meanK`, `multiplier`, `classification`.
    pub fn outlier(&self, options: JsValue) -> Result<JsPointCloud, JsError> {
        let o: JsOutlierOptions = parse(options)?;
        let o = OutlierOptions {
            method: OutlierMethod::parse(&o.method).map_err(js_err)?,
            min_k: o.min_k,
            radius: o.radius,
            mean_k: o.mean_k,
            multiplier: o.multiplier,
            class: o.classification,
        };
        Ok(JsPointCloud { inner: filters::outlier(&self.inner, &o).map_err(js_err)? })
    }

    /// Classifies ground points with the Simple Morphological Filter, as PDAL's `filters.smrf`.
    /// `options`: `cell`, `slope`, `window`, `scalar`, `threshold`, `cut`, `ignore`, `returns`,
    /// `groundClass`, `otherClass`, `onlyGround`.
    pub fn smrf(&self, options: JsValue) -> Result<JsPointCloud, JsError> {
        let o: JsSmrfOptions = parse(options)?;
        let o = SmrfOptions {
            cell: o.cell,
            slope: o.slope,
            window: o.window,
            scalar: o.scalar,
            threshold: o.threshold,
            cut: o.cut,
            ignore: o.ignore.as_deref().map(DimRange::parse_list).transpose().map_err(js_err)?.unwrap_or_default(),
            returns: o.returns.iter().map(|r| ReturnKind::parse(r)).collect::<terrio::Result<_>>().map_err(js_err)?,
            ground_class: o.ground_class,
            other_class: o.other_class,
            only_ground: o.only_ground,
        };
        Ok(JsPointCloud { inner: filters::smrf(&self.inner, &o).map_err(js_err)? })
    }

    /// Classifies ground points with the Progressive Morphological Filter, as PDAL's
    /// `filters.pmf`, with its defaults (cell size `cellSize`, 1 by default).
    pub fn pmf(&self, cell_size: Option<f64>) -> Result<JsPointCloud, JsError> {
        let o = filters::PmfOptions { cell: cell_size.unwrap_or(1.0), ..Default::default() };
        Ok(JsPointCloud { inner: filters::pmf(&self.inner, &o).map_err(js_err)? })
    }

    /// Classifies ground points with the Cloth Simulation Filter, as PDAL's `filters.csf`, with
    /// its defaults (cloth resolution `resolution`, 1 by default).
    pub fn csf(&self, resolution: Option<f64>) -> Result<JsPointCloud, JsError> {
        let o = filters::CsfOptions { resolution: resolution.unwrap_or(1.0), ..Default::default() };
        Ok(JsPointCloud { inner: filters::csf(&self.inner, &o).map_err(js_err)? })
    }

    /// Classifies low noise (class 7) with the Extended Local Minimum, as PDAL's `filters.elm`
    /// (`cell` 10 and `threshold` 1 by default).
    pub fn elm(&self, cell: Option<f64>, threshold: Option<f64>) -> Result<JsPointCloud, JsError> {
        Ok(JsPointCloud { inner: filters::elm(&self.inner, cell.unwrap_or(10.0), threshold.unwrap_or(1.0), 7).map_err(js_err)? })
    }

    /// Heights above the nearest ground points (`height_above_ground`), as PDAL's
    /// `filters.hag_nn` (`count` 1 by default).
    #[wasm_bindgen(js_name = hagNn)]
    pub fn hag_nn(&self, count: Option<usize>) -> Result<JsPointCloud, JsError> {
        let o = filters::HagOptions { count: count.unwrap_or(1), ..filters::HagOptions::nn() };
        Ok(JsPointCloud { inner: filters::hag_nn(&self.inner, &o).map_err(js_err)? })
    }

    /// Heights above a triangulation of the nearest ground points (`height_above_ground`), as
    /// PDAL's `filters.hag_delaunay` (`count` 10 by default).
    #[wasm_bindgen(js_name = hagDelaunay)]
    pub fn hag_delaunay(&self, count: Option<usize>) -> Result<JsPointCloud, JsError> {
        let o = filters::HagOptions { count: count.unwrap_or(10), ..filters::HagOptions::delaunay() };
        Ok(JsPointCloud { inner: filters::hag_delaunay(&self.inner, &o).map_err(js_err)? })
    }

    /// The points in a box (`([xmin, xmax], [ymin, ymax])`) or a polygon (WKT or GeoJSON), as
    /// PDAL's `filters.crop`.
    pub fn crop(&self, geometry: &str) -> Result<JsPointCloud, JsError> {
        let o = if geometry.trim_start().starts_with('(') {
            filters::CropOptions { bounds: vec![filters::CropBox::parse(geometry).map_err(js_err)?], ..Default::default() }
        } else {
            filters::CropOptions { polygons: vec![geometry.to_string()], ..Default::default() }
        };
        Ok(JsPointCloud { inner: filters::crop(&self.inner, &o).map_err(js_err)? })
    }

    /// Every `step`-th point, as PDAL's `filters.decimation`.
    pub fn decimation(&self, step: f64) -> Result<JsPointCloud, JsError> {
        Ok(JsPointCloud { inner: filters::decimation(&self.inner, step, 0, None).map_err(js_err)? })
    }

    /// The points for which an expression holds, as PDAL's `filters.expression`, such as
    /// `"Classification == 2 && Z > 100"`.
    pub fn expression(&self, expression: &str) -> Result<JsPointCloud, JsError> {
        Ok(JsPointCloud { inner: filters::expression(&self.inner, &[expression.to_string()]).map_err(js_err)? })
    }

    /// Assigns values with an expression, as PDAL's `filters.assign` `value`, such as
    /// `"Classification = 2 WHERE HeightAboveGround < 0.5"`.
    pub fn assign(&self, value: &str) -> Result<JsPointCloud, JsError> {
        let o = filters::AssignOptions { values: vec![value.to_string()], ..Default::default() };
        Ok(JsPointCloud { inner: filters::assign(&self.inner, &o).map_err(js_err)? })
    }

    /// Normals and curvature (`normal_x`, `normal_y`, `normal_z`, `curvature`), as PDAL's
    /// `filters.normal` (`knn` 8 by default).
    pub fn normal(&self, knn: Option<usize>) -> Result<JsPointCloud, JsError> {
        let o = filters::NormalOptions { knn: knn.unwrap_or(8), ..Default::default() };
        Ok(JsPointCloud { inner: filters::normal(&self.inner, &o).map_err(js_err)? })
    }

    /// Euclidean clusters (`cluster_id`), as PDAL's `filters.cluster` (`tolerance` 1 by
    /// default).
    pub fn cluster(&self, tolerance: Option<f64>) -> Result<JsPointCloud, JsError> {
        let o = filters::ClusterOptions { tolerance: tolerance.unwrap_or(1.0), ..Default::default() };
        Ok(JsPointCloud { inner: filters::cluster(&self.inner, &o).map_err(js_err)? })
    }
}

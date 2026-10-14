//! Point clouds (LAS, LAZ, COPC) from bytes.

use super::{Dataset, Inner, crs_string, js_err, to_js};
use serde::Deserialize;
use terrio::pointcloud::grid::{GridOptions, GridStat};
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

/// An open LAS, LAZ or COPC file: its header is read, its points on demand.
#[wasm_bindgen]
pub struct PointCloudFile {
    inner: PointCloudReader,
}

/// Points in memory: one typed array per dimension.
#[wasm_bindgen(js_name = PointCloud)]
pub struct JsPointCloud {
    inner: PointCloud,
}

/// Opens a LAS, LAZ or COPC file from its bytes, reading only its header.
#[wasm_bindgen(js_name = openPoints)]
pub fn open_points(bytes: Vec<u8>, filename: &str) -> Result<PointCloudFile, JsError> {
    Ok(PointCloudFile { inner: pointcloud::open_points_bytes(bytes, filename).map_err(js_err)? })
}

/// Reads the points of a LAS, LAZ or COPC file from its bytes. `options`: `bounds`, `zRange`,
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
    /// `"LAS"`, `"LAZ"` or `"COPC"`.
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

    /// The points as a LAS, LAZ or COPC file: `filename`'s extension (`.las`, `.laz`,
    /// `.copc.laz`) tells the format unless `format` does; `pointFormat` (0 to 10).
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

    /// Grids the points into a raster, as PDAL's `writers.gdal` does. `options`: `stat` (a list of
    /// `min`, `max`, `mean`, `idw`, `count`, `stdev`; `mean` by default), `dimension`, `radius`,
    /// `power`, `windowSize`, `binMode`, `bounds`, `nodata`.
    pub fn grid(&self, resolution: f64, options: JsValue) -> Result<Dataset, JsError> {
        let o: JsGridOptions = parse(options)?;
        let stats = o.stat.iter().map(|s| GridStat::parse(s)).collect::<terrio::Result<_>>().map_err(js_err)?;
        let options = GridOptions {
            resolution,
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
}

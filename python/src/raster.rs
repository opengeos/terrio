//! The `Raster` class: a raster in memory, or a GeoTIFF read by window.

use crate::Detach;
use crate::{PyResultExt, crs_string, invalid};
use numpy::{PyArray1, PyArrayDescrMethods, PyArrayDyn, PyArrayMethods, PyUntypedArray, PyUntypedArrayMethods};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use terrio::crs::SpatialRef;
use terrio::formats::RasterDriver;
use terrio::ops::Resampling;
use terrio::ops::overview::OverviewSelection;
use terrio::ops::warp::RasterDataRef;
use terrio::{DataType, LazyRaster, RasterDataset, RasterWriteOptions, ResampledReadOptions, Window, WindowOptions};
use wbraster::raster::RasterData;

/// A metadata domain as Python gets it: the band (from 1, 0 for the dataset), the domain, its items.
type BandDomain = (usize, String, Vec<(String, String)>);

/// A raster dataset: a grid of pixels in one or more bands, with georeferencing.
///
/// Rasters come from `terrio.open`, `terrio.open_raster` or `Raster.from_array`. A raster opened
/// with `lazy=True` (GeoTIFFs and COGs, local or remote) holds only its headers; `read` then
/// decodes, and for remote files fetches, only the tiles a window needs.
///
/// Bands are numbered from 1 and overviews from 0, as in GDAL.
///
/// A raster opened lazily from a path can be read from several threads at once: a read that
/// overlaps another reads with a second reader of the same path (see `Readers`).
#[pyclass(module = "terrio", name = "Raster", dict)]
pub struct PyRaster {
    pub(crate) inner: LazyRaster,
}

/// More readers of a raster opened lazily from `path`, kept in its `_readers` attribute: a read
/// that overlaps one in progress (which holds the raster's own reader) takes one of them, or
/// opens one, and leaves it for the next. Opening again reads the headers from the process-wide
/// caches. Each reader has its own file handle; their decoded chunks share the process-wide block
/// cache, under its one byte cap, without being shared between readers.
#[pyclass(frozen)]
pub(crate) struct Readers {
    path: String,
    /// The options of the thread that opened the raster (an `Env`, which is thread-local), so
    /// that a reader opened on another thread sees the dataset as the raster does: its sidecars,
    /// such as a `.msk` mask, ignored under `EMPTY_DIR` as they were at the open.
    options: std::collections::HashMap<String, Option<String>>,
    spare: std::sync::Mutex<Vec<LazyRaster>>,
}

/// The most spare readers a raster keeps between reads. Each holds an open file for as long as
/// the raster lives, so a burst of many threads must not leave one
/// per thread; a reader past the bound is dropped, and reopening one later costs little because
/// its headers come from the process-wide caches. Four covers a small thread pool reading one
/// raster.
const MAX_SPARE_READERS: usize = 4;

impl Readers {
    /// Readers of `path`. A relative local path is made absolute now, so that a reader opened
    /// after the working directory changes opens the same file; URLs, `/vsi` paths, archive
    /// members and other selectors are kept as given.
    pub(crate) fn new(path: &str) -> Self {
        let absolute = match terrio::vfs::Location::parse(path) {
            terrio::vfs::Location::Local(p) if p.is_relative() && p.exists() => std::path::absolute(p).ok(),
            _ => None,
        };
        let path = absolute.as_deref().and_then(std::path::Path::to_str).unwrap_or(path);
        let mut options = terrio::config::thread_options();
        // Freeze the resolved limit, including an unset value, for readers opened later.
        let limit = "TERRIO_TIFF_MAX_CHUNK_BYTES";
        options.insert(limit.to_owned(), terrio::config::get(limit));
        Self { path: path.to_owned(), options, spare: Default::default() }
    }

    /// Opens another reader with the options the raster was opened with, on whatever thread.
    fn open(&self) -> terrio::Result<LazyRaster> {
        struct Scope;
        impl Drop for Scope {
            fn drop(&mut self) {
                terrio::config::pop_scope();
            }
        }
        terrio::config::push_scope(self.options.clone());
        let _scope = Scope;
        terrio::open_raster_lazy(&self.path)
    }
}

#[pymethods]
impl Readers {
    /// The number of spare readers kept.
    fn __len__(&self) -> usize {
        self.spare.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
    }
}

impl PyRaster {
    /// Runs `read` on the raster's reader or, while another thread reads the raster, on another
    /// reader of its path (see [`Readers`]). A raster opened otherwise has one reader, which one
    /// thread at a time reads: an overlapping read raises, as before.
    fn with_reader<T>(slf: &Bound<'_, Self>, read: impl FnOnce(&mut LazyRaster) -> PyResult<T>) -> PyResult<T> {
        let busy = match slf.try_borrow_mut() {
            Ok(mut this) => return read(&mut this.inner),
            Err(busy) => busy,
        };
        let py = slf.py();
        let Ok(readers) = slf.getattr(pyo3::intern!(py, "_readers")) else { return Err(busy.into()) };
        let readers = readers.cast::<Readers>()?.get();
        let lock = || readers.spare.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let spare = lock().pop();
        let mut reader = match spare {
            Some(r) => r,
            None => py.detach_result(|| readers.open()).py()?,
        };
        let out = read(&mut reader);
        let mut spare = lock();
        if spare.len() < MAX_SPARE_READERS {
            spare.push(reader);
        }
        out
    }

    /// Wraps a raster in memory.
    pub(crate) fn from_dataset(ds: RasterDataset) -> Self {
        Self { inner: LazyRaster::from(ds) }
    }

    /// Runs `f` on the whole raster in memory, reading it first when it is lazy.
    pub(crate) fn with_dataset<R>(&mut self, f: impl FnOnce(&RasterDataset) -> terrio::Result<R>) -> terrio::Result<R> {
        match self.inner.dataset() {
            Some(ds) => f(ds),
            None => f(&self.inner.read()?),
        }
    }

    /// Writes the raster to `path` with `options`, streaming tile archives by tile row when lazy
    /// and falling back to a whole read when the source inventory is unavailable.
    pub(crate) fn write_with_options(&mut self, py: Python<'_>, path: &str, mut options: RasterWriteOptions) -> PyResult<()> {
        py.detach_result(|| {
            self.inner.prepare_output_mask(path, &mut options, Window::full(self.inner.width(), self.inner.height()), None)?;
            if self.inner.can_stream_tile_archive(path, &options) {
                return self.inner.write(path, &options);
            }
            self.with_dataset(|ds| terrio::write_raster_dataset(ds, path, &options))
        })
        .py()
    }

    fn crs_ref(&self) -> SpatialRef {
        self.inner.crs()
    }

    /// Whether the bands are complex (each held as a real and an imaginary band), in memory or
    /// read by window (complex GeoTIFFs).
    fn is_complex(&self) -> bool {
        self.inner.is_complex()
    }
}

/// Complex values from bands of real parts followed by bands of imaginary parts (band `k`'s
/// parts are bands `2k` and `2k + 1`), as a NumPy array of `complex64` (`complex128` for
/// 64-bit floats) of the given shape.
fn complex_to_numpy<'py>(py: Python<'py>, data: RasterData, plane: usize, shape: &[usize]) -> PyResult<Bound<'py, PyAny>> {
    complex_to_numpy_as(py, data, plane, shape, false)
}

fn complex_to_numpy_as<'py>(
    py: Python<'py>,
    data: RasterData,
    plane: usize,
    shape: &[usize],
    force_double: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let double = force_double || data.data_type() == DataType::F64;
    let v = data.to_f64_vec();
    let bands = v.len() / (2 * plane).max(1);
    let pairs = (0..bands).flat_map(|b| (0..plane).map(move |i| (b, i)));
    if double {
        let c: Vec<numpy::Complex64> =
            pairs.map(|(b, i)| numpy::Complex64::new(v[2 * b * plane + i], v[(2 * b + 1) * plane + i])).collect();
        Ok(PyArray1::from_vec(py, c).reshape(shape)?.into_any())
    } else {
        let c: Vec<numpy::Complex32> =
            pairs.map(|(b, i)| numpy::Complex32::new(v[2 * b * plane + i] as f32, v[(2 * b + 1) * plane + i] as f32)).collect();
        Ok(PyArray1::from_vec(py, c).reshape(shape)?.into_any())
    }
}

/// Moves pixels into a NumPy array of the given shape, without converting them.
pub(crate) fn to_numpy<'py>(py: Python<'py>, data: RasterData, shape: &[usize]) -> PyResult<Bound<'py, PyAny>> {
    macro_rules! arr {
        ($($v:ident),*) => {
            match data {
                $(RasterData::$v(v) => PyArray1::from_vec(py, v).reshape(shape)?.into_any(),)*
            }
        };
    }
    Ok(arr!(U8, I8, U16, I16, U32, I32, U64, I64, F32, F64))
}

/// Copies a NumPy array's values, in C order, into raster pixels.
pub(crate) fn from_numpy(array: &Bound<'_, PyUntypedArray>) -> PyResult<RasterData> {
    let py = array.py();
    let dtype = array.dtype();
    macro_rules! try_types {
        ($($t:ty => $v:ident),*) => {
            $(if dtype.is_equiv_to(&numpy::dtype::<$t>(py)) {
                // A slice copy, numpy making strided arrays contiguous first: walking the array
                // element by element cost a bounds-checked iterator step per value, where a
                // slice copy is one memcpy.
                // Only C-contiguous arrays are read in place: `as_slice` on a Fortran-ordered
                // array returns its memory order, not C order, so other layouts go through numpy.
                let a = array.cast::<PyArrayDyn<$t>>()?;
                if a.is_c_contiguous() && let Ok(values) = a.readonly().as_slice() {
                    return Ok(RasterData::$v(values.to_vec()));
                }
                let c = py.import("numpy")?.call_method1("ascontiguousarray", (a,))?;
                let c = c.cast::<PyArrayDyn<$t>>()?.readonly();
                return Ok(RasterData::$v(c.as_slice().map_err(|e| invalid(e.to_string()))?.to_vec()));
            })*
        };
    }
    try_types!(u8 => U8, i8 => I8, u16 => U16, i16 => I16, u32 => U32, i32 => I32, u64 => U64, i64 => I64, f32 => F32, f64 => F64);
    if dtype.is_equiv_to(&numpy::dtype::<bool>(py)) {
        let a = array.cast::<PyArrayDyn<bool>>()?.readonly();
        return Ok(RasterData::U8(a.as_array().iter().map(|b| *b as u8).collect()));
    }
    Err(invalid(format!("unsupported array dtype {dtype}: use a supported array in native byte order")))
}

/// The physical component type selected by a complex NumPy/GDAL sample name.
pub(crate) fn complex_dtype(dtype: &str) -> Option<DataType> {
    match dtype.to_ascii_lowercase().as_str() {
        "complex_int16" | "cint16" => Some(DataType::I16),
        "complex_int32" | "cint32" => Some(DataType::I32),
        "complex64" | "cfloat32" => Some(DataType::F32),
        "complex128" | "cfloat64" => Some(DataType::F64),
        _ => None,
    }
}

/// The physical storage type and complex flag shared by both TIFF row writers.
pub(crate) fn storage_type(dtype: &str) -> PyResult<(DataType, bool)> {
    if let Some(component) = complex_dtype(dtype) {
        return Ok((component, true));
    }
    let data_type = match dtype {
        "uint8" | "bool" => DataType::U8,
        "int8" => DataType::I8,
        "uint16" => DataType::U16,
        "int16" => DataType::I16,
        "uint32" => DataType::U32,
        "int32" => DataType::I32,
        "uint64" => DataType::U64,
        "int64" => DataType::I64,
        "float32" => DataType::F32,
        "float64" => DataType::F64,
        other => return Err(invalid(format!("unsupported dtype {other}"))),
    };
    Ok((data_type, false))
}

/// Complex band-major arrays become adjacent real/imaginary component planes.
pub(crate) fn raster_array_data(array: &Bound<'_, PyUntypedArray>, target: Option<DataType>) -> PyResult<(RasterData, bool)> {
    let py = array.py();
    let dtype = array.dtype();
    let shape = array.shape();
    if shape.len() < 2 {
        return Err(invalid("expected nonempty raster rows and columns"));
    }
    let plane = shape[shape.len() - 2] * shape[shape.len() - 1];
    macro_rules! complex {
        ($c:ty, $variant:ident, $to_f64:expr) => {
            if dtype.is_equiv_to(&numpy::dtype::<$c>(py)) {
                let contiguous = py.import("numpy")?.call_method1("ascontiguousarray", (array,))?;
                let readonly = contiguous.cast::<PyArrayDyn<$c>>()?.readonly();
                let values = readonly.as_slice().map_err(|e| invalid(e.to_string()))?;
                if let Some(target) = target {
                    // Band-major, each band's real plane then its imaginary plane. An empty array
                    // has no bands, so the chunk size only has to be nonzero.
                    let components = values
                        .chunks_exact(plane.max(1))
                        .flat_map(|band| band.iter().map(|v| $to_f64(v.re)).chain(band.iter().map(|v| $to_f64(v.im))));
                    return Ok((terrio::complex_components_from_iter(components, target).py()?, true));
                }
                let mut parts = Vec::with_capacity(values.len() * 2);
                if plane != 0 {
                    for band in values.chunks_exact(plane) {
                        parts.extend(band.iter().map(|v| v.re));
                        parts.extend(band.iter().map(|v| v.im));
                    }
                }
                let data = RasterData::$variant(parts);
                return Ok((data, true));
            }
        };
    }
    complex!(numpy::Complex32, F32, f64::from);
    complex!(numpy::Complex64, F64, std::convert::identity::<f64>);
    if target.is_some() {
        return Err(invalid("a complex sample type requires a complex NumPy array in native byte order"));
    }
    Ok((from_numpy(array)?, false))
}

/// Runs `f` on a NumPy array's values in C order, read where they are when the array is
/// C-contiguous and not writeable, copied first (`from_numpy`) otherwise. `f` may release the
/// GIL only when `borrowed` is false. A read-only flag does not exclude writable aliases, so
/// the borrowed path must keep Python threads from mutating those aliases. As with other NumPy
/// borrows, callers must exclude native writers already running outside the GIL. While borrowed,
/// `f` must not call Python (including warning callbacks or error conversion).
pub(crate) fn with_numpy<R>(array: &Bound<'_, PyUntypedArray>, f: impl FnOnce(RasterDataRef<'_>, bool) -> R) -> PyResult<R> {
    let py = array.py();
    let writeable: bool = array.getattr("flags")?.getattr("writeable")?.extract()?;
    if !writeable && array.is_c_contiguous() {
        let dtype = array.dtype();
        macro_rules! try_types {
            ($($t:ty => $v:ident),*) => {
                $(if dtype.is_equiv_to(&numpy::dtype::<$t>(py))
                    && let Ok(a) = array.cast::<PyArrayDyn<$t>>()
                    && let Ok(a) = a.try_readonly()
                    && let Ok(values) = a.as_slice()
                {
                    return Ok(f(RasterDataRef::$v(values), true));
                })*
            };
        }
        try_types!(u8 => U8, i8 => I8, u16 => U16, i16 => I16, u32 => U32, i32 => I32, u64 => U64, i64 => I64, f32 => F32, f64 => F64);
    }
    let data = from_numpy(array)?;
    Ok(f(RasterDataRef::from(&data), false))
}

/// Parses a `band` argument: `None` (all bands), an int, or a sequence of ints. Returns the bands
/// and whether a single band was asked for.
fn parse_bands(band: Option<&Bound<'_, PyAny>>) -> PyResult<(Vec<usize>, bool)> {
    match band {
        None => Ok((Vec::new(), false)),
        Some(b) => match b.extract::<usize>() {
            Ok(i) => Ok((vec![i], true)),
            Err(_) => Ok((b.extract::<Vec<usize>>().map_err(|_| invalid("band must be an int or a sequence of ints"))?, false)),
        },
    }
}

/// Parses the `overview` argument of a resampled read: `"AUTO"`, `"AUTO-n"`, `"NONE"`, an
/// overview number (an int or its digits), or `None` for `"AUTO"`.
fn parse_overview_selection(overview: Option<&Bound<'_, PyAny>>) -> PyResult<OverviewSelection> {
    match overview {
        None => Ok(OverviewSelection::Auto),
        Some(o) => match o.extract::<usize>() {
            Ok(i) => Ok(OverviewSelection::Level(i)),
            Err(_) => {
                let text =
                    o.extract::<String>().map_err(|_| invalid("overview must be \"AUTO\", \"AUTO-n\", \"NONE\" or an overview number"))?;
                OverviewSelection::parse(&text).py()
            }
        },
    }
}

impl PyRaster {
    /// The options of a resampled read, from the arguments `read_resampled` takes, with the bands
    /// as the raster numbers them (a complex band's real and imaginary parts), and whether a
    /// single band was asked for.
    fn resampled_options(
        &self,
        size: (usize, usize),
        band: Option<&Bound<'_, PyAny>>,
        window: Option<(f64, f64, f64, f64)>,
        resampling: &str,
        overview: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<(ResampledReadOptions, bool)> {
        let (mut bands, single) = parse_bands(band)?;
        if self.is_complex() {
            if bands.is_empty() {
                bands = (1..=self.inner.band_count()).collect();
            } else {
                let count = self.inner.band_count() / 2;
                if let Some(b) = bands.iter().find(|b| **b == 0 || **b > count) {
                    return Err(invalid(format!("no band {b}: the raster has {count} band(s)")));
                }
                bands = bands.iter().flat_map(|b| [2 * b - 1, 2 * b]).collect();
            }
        }
        let options = ResampledReadOptions {
            bands,
            window,
            size,
            resampling: Resampling::parse(resampling).py()?,
            overview: parse_overview_selection(overview)?,
        };
        Ok((options, single))
    }
}

#[pymethods]
impl PyRaster {
    /// The width in pixels.
    #[getter]
    fn width(&self) -> usize {
        self.inner.width()
    }

    /// The height in pixels.
    #[getter]
    fn height(&self) -> usize {
        self.inner.height()
    }

    /// The number of bands.
    #[getter]
    fn count(&self) -> usize {
        if self.is_complex() { self.inner.band_count() / 2 } else { self.inner.band_count() }
    }

    /// The shape of the array `read()` returns: `(count, height, width)`.
    #[getter]
    fn shape(&self) -> (usize, usize, usize) {
        (self.count(), self.inner.height(), self.inner.width())
    }

    /// The pixel type, as a NumPy dtype name such as `"uint8"` or `"float32"`; `"complex64"` or
    /// `"complex128"` for complex bands (GDAL's `CInt16`... `CFloat64`).
    #[getter]
    fn dtype(&self) -> &'static str {
        match (self.is_complex(), self.inner.data_type()) {
            (true, DataType::F64) => "complex128",
            (true, _) => "complex64",
            (false, t) => t.as_str(),
        }
    }

    /// GDAL's name of the bands' data type: `Byte`, `UInt16`, `Float32`... and `CInt16`,
    /// `CInt32`, `CFloat32` or `CFloat64` for complex bands (whose `dtype` is `complex64` or
    /// `complex128`).
    #[getter]
    fn data_type(&self) -> String {
        let name = terrio::info::gdal_type_name(self.inner.data_type());
        if self.is_complex() { format!("C{name}") } else { name.to_string() }
    }

    /// Where the longitude and latitude of each pixel are, for swaths and other rasters
    /// georeferenced by geolocation arrays: GDAL's `GEOLOCATION` metadata items (`X_DATASET`,
    /// `Y_DATASET`, `PIXEL_STEP`...), or `None`.
    #[getter]
    fn geolocation(&self) -> Option<Vec<(String, String)>> {
        self.inner.dataset().and_then(|d| d.geolocation.as_ref()).map(|g| g.items())
    }

    /// The description and metadata of each band, for formats that report them per band (GRIB):
    /// one dictionary per band with its `description`, `color_interp` (GDAL's name, or `None`) and
    /// metadata `items` (GDAL's default domain), or an empty list. JPEG 2000 colour channels
    /// also retain their `color_association` from the stored CDEF box.
    #[getter]
    fn band_metadata<'py>(&self, py: Python<'py>) -> PyResult<Vec<Bound<'py, PyDict>>> {
        self.inner
            .band_metadata()
            .iter()
            .map(|m| {
                let d = PyDict::new(py);
                d.set_item("description", &m.description)?;
                d.set_item("color_interp", &m.color_interp)?;
                if let Some(association) = m.color_association {
                    d.set_item("color_association", association)?;
                }
                let items = PyDict::new(py);
                for (k, v) in &m.items {
                    items.set_item(k, v)?;
                }
                d.set_item("items", items)?;
                Ok(d)
            })
            .collect()
    }

    /// The nodata value of each band, when the bands declare different ones (GRIB); otherwise an
    /// empty list (every band has `nodata`).
    #[getter]
    fn band_nodata(&self) -> Vec<Option<f64>> {
        self.inner.band_nodata_values().to_vec()
    }

    /// Metadata domains other than the default one, as GDAL reports them, for formats that report
    /// them (JPEG 2000's `IMAGE_STRUCTURE`): `(band, domain, items)` triples, the band from 1, 0
    /// for the dataset.
    #[getter]
    fn metadata_domains(&self) -> Vec<BandDomain> {
        self.inner.metadata_domains().iter().map(|(b, d, items)| (b.map_or(0, |b| b + 1), d.clone(), items.clone())).collect()
    }

    /// The color table of band 1, for formats whose reader keeps one (paletted PNGs): `(r, g, b,
    /// a)` entries from index 0, or `None`. A GeoTIFF's color map is read by `terrio.rio`.
    #[getter]
    fn color_table(&self) -> Option<Vec<(u8, u8, u8, u8)>> {
        let table = self.inner.color_table();
        (!table.is_empty()).then(|| table.iter().map(|c| (c[0], c[1], c[2], c[3])).collect())
    }

    /// The metadata items a driver other than the GeoTIFF one read, as `(key, value)` pairs:
    /// dataset items (netCDF's `NC_GLOBAL#title`, `NETCDF_DIM_time_VALUES`...), band items as
    /// `BAND_<n>#<key>`, and subdatasets as `SUBDATASET_<n>_NAME`. Empty for GeoTIFFs, whose
    /// metadata is in their `GDAL_METADATA` tag.
    #[getter]
    fn metadata(&self) -> Vec<(String, String)> {
        self.inner.metadata()
    }

    /// Names of subdatasets declared in the open raster's metadata, without reopening it.
    #[getter]
    fn subdatasets(&self) -> Vec<String> {
        self.inner.subdatasets().into_iter().map(|s| s.name).collect()
    }

    /// The nodata value, or `None` when the raster declares none.
    #[getter]
    fn nodata(&self) -> Option<f64> {
        self.inner.nodata()
    }

    /// The CRS: `"EPSG:xxxx"` (or `"ESRI:xxxx"`) when its code is known, otherwise its WKT, or
    /// `None`.
    #[getter]
    fn crs(&self) -> Option<String> {
        crs_string(&self.crs_ref())
    }

    /// The CRS as WKT, or `None` when it is unknown.
    #[getter]
    fn crs_wkt(&self) -> Option<String> {
        self.crs_ref().to_wkt()
    }

    /// The GDAL-style geotransform `(x_origin, pixel_width, row_rotation, y_origin,
    /// column_rotation, pixel_height)`.
    #[getter]
    fn transform(&self) -> (f64, f64, f64, f64, f64, f64) {
        let g = self.inner.geo_transform();
        (g[0], g[1], g[2], g[3], g[4], g[5])
    }

    /// Whether a GeoTIFF gives a geotransform (`None` for other formats): without one,
    /// `transform` is `(0, 1, 0, height, 0, -1)`, which a georeferenced file can also have.
    #[getter]
    fn has_transform(&self) -> Option<bool> {
        self.inner.geotiff_has_geo_transform()
    }

    /// The pixel size `(x_resolution, y_resolution)`, both positive.
    #[getter]
    fn res(&self) -> (f64, f64) {
        let g = self.inner.geo_transform();
        (g[1].hypot(g[4]), g[2].hypot(g[5]))
    }

    /// The extent `(min_x, min_y, max_x, max_y)` in CRS units.
    #[getter]
    fn bounds(&self) -> (f64, f64, f64, f64) {
        let g = self.inner.geo_transform();
        let (w, h) = (self.inner.width() as f64, self.inner.height() as f64);
        let xs = [g[0], g[0] + w * g[1], g[0] + h * g[2], g[0] + w * g[1] + h * g[2]];
        let ys = [g[3], g[3] + w * g[4], g[3] + h * g[5], g[3] + w * g[4] + h * g[5]];
        let min = |v: [f64; 4]| v.iter().copied().fold(f64::INFINITY, f64::min);
        let max = |v: [f64; 4]| v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        (min(xs), min(ys), max(xs), max(ys))
    }

    /// The name of the format driver that read the raster, such as `"GTiff"`.
    #[getter]
    fn driver(&self) -> &'static str {
        self.inner.driver().name()
    }

    /// The overview sizes `[(width, height), ...]`, largest first. Empty unless lazy.
    #[getter]
    fn overviews(&self) -> Vec<(usize, usize)> {
        self.inner.overviews()
    }

    /// Whether pixels are read on demand from the file, rather than held in memory.
    #[getter]
    fn is_lazy(&self) -> bool {
        self.inner.is_lazy()
    }

    /// Reads pixels into a NumPy array of the raster's own dtype.
    ///
    /// Args:
    ///     band: The band to read (an int, numbered from 1), a sequence of bands, or `None` for
    ///         all bands.
    ///     window: The pixels to read, `(col_off, row_off, width, height)`, or `None` for the
    ///         whole image. With `overview`, in the overview's pixels.
    ///     overview: Read this overview (0 is the largest) instead of the full resolution.
    ///     out_dtype: NumPy output dtype. complex128 preserves every Int32 complex component.
    ///
    /// Returns:
    ///     numpy.ndarray: An array of shape `(rows, cols)` when `band` is an int, otherwise
    ///     `(bands, rows, cols)`; complex bands come as `complex64` (or `complex128`) values.
    ///
    /// Raises:
    ///     InvalidArgumentError: If a band, the window or the overview does not exist.
    ///     ResourceLimitError: If the native read exceeds its resource policy, or its subsequent
    ///         NumPy conversion exceeds usable memory. Native window storage is checked by the
    ///         reader; NumPy working buffers are checked after that read and before conversion.
    ///         Conversion admission applies to both real and complex rasters.
    #[pyo3(signature = (band=None, window=None, overview=None, *, out_dtype=None))]
    fn read<'py>(
        slf: &Bound<'py, Self>,
        band: Option<&Bound<'py, PyAny>>,
        window: Option<(usize, usize, usize, usize)>,
        overview: Option<usize>,
        out_dtype: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let py = slf.py();
        let out_dtype = out_dtype.map(|d| py.import("numpy")?.call_method1("dtype", (d,))).transpose()?;
        let dtype_name = out_dtype.as_ref().map(|d| d.getattr("name")?.extract::<String>()).transpose()?;
        let double = matches!(dtype_name.as_deref(), Some("complex128" | "float64" | "int32" | "uint32" | "int64" | "uint64"));
        let (mut bands, single) = parse_bands(band)?;
        Self::with_reader(slf, |inner| {
            let complex = inner.is_complex();
            if complex {
                // A complex band's real and imaginary parts.
                if bands.is_empty() {
                    bands = (1..=inner.band_count()).collect();
                } else {
                    // Checked by the complex bands' numbers, which the parts' numbers are not.
                    let count = inner.band_count() / 2;
                    if let Some(b) = bands.iter().find(|b| **b == 0 || **b > count) {
                        return Err(invalid(format!("no band {b}: the raster has {count} band(s)")));
                    }
                    bands = bands.iter().flat_map(|b| [2 * b - 1, 2 * b]).collect();
                }
            }
            let window = match window {
                Some((x, y, w, h)) => Window::new(x, y, w, h),
                None => {
                    let (w, h) =
                        match overview {
                            None => (inner.width(), inner.height()),
                            Some(i) => inner.overviews().get(i).copied().ok_or_else(|| {
                                invalid(format!("no overview {i}: the raster has {} overview(s)", inner.overviews().len()))
                            })?,
                        };
                    Window::full(w, h)
                }
            };
            let options = WindowOptions { bands, overview };
            let ds = py.detach_result(|| inner.read_window(window, &options)).py()?;
            let r = ds.raster;
            let count = if complex { r.bands / 2 } else { r.bands };
            let shape = if single { vec![r.rows, r.cols] } else { vec![count, r.rows, r.cols] };
            // Admit NumPy's result and known conversion buffers, which can be wider
            // than the component pixels returned by Rust's window reader.
            let wide_complex = complex && (double || r.data_type == DataType::F64);
            let working_size = if complex { if wide_complex { 16 } else { 8 } } else { r.data_type.size_bytes() };
            let working_name = if complex { if wide_complex { "complex128" } else { "complex64" } } else { r.data_type.as_str() };
            let itemsize = out_dtype.as_ref().map(|d| d.getattr("itemsize")?.extract::<usize>()).transpose()?.unwrap_or(working_size);
            let casts = match &out_dtype {
                Some(d) => dtype_name.as_deref() != Some(working_name) || !d.getattr("isnative")?.extract::<bool>()?,
                None => false,
            };
            let samples = count.checked_mul(r.rows).and_then(|n| n.checked_mul(r.cols));
            let native_bytes = samples.and_then(|n| n.checked_mul(working_size));
            let conversion_peak = if complex {
                r.data
                    .len()
                    .checked_mul(r.data_type.size_bytes())
                    .and_then(|raw| r.data.len().checked_mul(std::mem::size_of::<f64>()).and_then(|float| raw.checked_add(float)))
                    .and_then(|held| native_bytes.and_then(|output| held.checked_add(output)))
            } else {
                native_bytes
            };
            let cast_peak = if casts {
                native_bytes.and_then(|held| samples.and_then(|n| n.checked_mul(itemsize)).and_then(|output| held.checked_add(output)))
            } else {
                native_bytes
            };
            let bytes = conversion_peak
                .zip(cast_peak)
                .map(|(a, b)| a.max(b))
                .filter(|&n| n <= isize::MAX as usize)
                .ok_or_else(|| terrio::Error::ResourceLimit("a NumPy raster window exceeds the address space".into()))
                .py()?;
            if bytes > 1 << 24
                && let Some(most) = terrio::usable_memory()
                && bytes as u64 > most
            {
                return Err(terrio::Error::ResourceLimit(format!(
                    "a NumPy raster window working set requires {bytes} bytes, more than the {most} bytes of memory the process can use"
                )))
                .py();
            }
            let array =
                if complex { complex_to_numpy_as(py, r.data, r.rows * r.cols, &shape, double)? } else { to_numpy(py, r.data, &shape)? };
            match &out_dtype {
                Some(d) => {
                    let options = PyDict::new(py);
                    options.set_item("copy", false)?;
                    array.call_method("astype", (d,), Some(&options))
                }
                None => Ok(array),
            }
        })
    }

    /// Reads the pixels at some columns of some rows, as nearest-neighbour resampling picks them,
    /// from the blocks that hold them alone (GDAL's decimated read), when the format's reader can
    /// (Zarr).
    ///
    /// Args:
    ///     bands: The bands, numbered from 1 (empty for all).
    ///     cols: The column of each output column, in the level's pixels.
    ///     rows: The row of each output row.
    ///     overview: Read this overview (0 is the largest) instead of the full resolution.
    ///
    /// Returns:
    ///     numpy.ndarray | None: `(bands, len(rows), len(cols))` in the raster's own dtype, or
    ///     `None` when the reader reads the window spanning the pixels instead (other formats,
    ///     complex bands).
    ///
    /// Raises:
    ///     InvalidArgumentError: If a band, a pixel or the overview does not exist.
    #[pyo3(signature = (bands, cols, rows, overview=None))]
    fn read_sampled<'py>(
        &mut self,
        py: Python<'py>,
        bands: Vec<usize>,
        cols: Vec<usize>,
        rows: Vec<usize>,
        overview: Option<usize>,
    ) -> PyResult<Option<Bound<'py, PyAny>>> {
        if self.is_complex() {
            return Ok(None);
        }
        let options = WindowOptions { bands, overview };
        let inner = &mut self.inner;
        let data = py.detach_result(|| inner.read_sampled(&cols, &rows, &options)).py()?;
        let count = if options.bands.is_empty() { self.inner.band_count() } else { options.bands.len() };
        data.map(|d| to_numpy(py, d, &[count, rows.len(), cols.len()])).transpose()
    }

    /// Reads a window into an output of another size, as GDAL's `RasterIO` reads it
    /// (`gdal_translate -outsize`, rasterio's `read(window=..., out_shape=..., resampling=...)`).
    ///
    /// The stored resolution is the overview `RasterIO` would choose (or the one `overview`
    /// names), so a small output of a large raster reads only an overview's blocks, and for a
    /// remote COG fetches only its tiles. Invalid pixels (masked out, nodata, or transparent in an
    /// alpha band) carry no weight in the kernels.
    ///
    /// Args:
    ///     size: The output `(width, height)`.
    ///     band: The band to read (an int, numbered from 1), a sequence of bands, or `None` for
    ///         all bands.
    ///     window: The source window `(col_off, row_off, width, height)` in full-resolution
    ///         pixels, possibly fractional, within the raster; `None` for the whole raster.
    ///     resampling: `"nearest"` (the default), `"bilinear"`, `"cubic"`, `"cubicspline"`,
    ///         `"lanczos"`, `"average"`, `"rms"` or `"mode"`.
    ///     overview: The stored resolution read, as `gdal_translate -ovr` takes it: `"AUTO"` (the
    ///         default, `RasterIO`'s choice), `"AUTO-n"`, `"NONE"` (full resolution) or an
    ///         overview number (0 is the largest). The window stays in full-resolution pixels.
    ///
    /// Returns:
    ///     numpy.ndarray: The pixels in the raster's own dtype, `(height, width)` when `band` is
    ///     an int, otherwise `(bands, height, width)`.
    ///
    /// Raises:
    ///     InvalidArgumentError: If a band does not exist, the output or the window is empty,
    ///         the window is not within the raster, or the method is not a known name.
    ///     UnsupportedError: If the method applies to warps only (`min`, `max`, `med`, `q1`,
    ///         `q3`, `sum`).
    #[pyo3(signature = (size, band=None, window=None, resampling="nearest", overview=None))]
    fn read_resampled<'py>(
        &mut self,
        py: Python<'py>,
        size: (usize, usize),
        band: Option<&Bound<'py, PyAny>>,
        window: Option<(f64, f64, f64, f64)>,
        resampling: &str,
        overview: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let (options, single) = self.resampled_options(size, band, window, resampling, overview)?;
        let complex = self.is_complex();
        let inner = &mut self.inner;
        let r = py.detach_result(|| inner.read_resampled(&options)).py()?.raster;
        let bands = if complex { r.bands / 2 } else { r.bands };
        let shape = if single { vec![r.rows, r.cols] } else { vec![bands, r.rows, r.cols] };
        if complex {
            return complex_to_numpy(py, r.data, r.rows * r.cols, &shape);
        }
        to_numpy(py, r.data, &shape)
    }

    /// Reads the mask bands of the bands `read_resampled` reads, with the same arguments, as
    /// GDAL's `GetMaskBand()->RasterIO()` reads them.
    ///
    /// A per-dataset mask is resampled from its own overviews; a nodata value masks the pixels
    /// `read_resampled` reads that are that value; an alpha band is the other bands' mask; bands
    /// without one are all valid. Bands with a nodata value are read for their masks:
    /// `read_resampled_with_masks` reads pixels and masks without reading them twice.
    ///
    /// Args:
    ///     size: The output `(width, height)`.
    ///     band: The band (an int, numbered from 1), a sequence of bands, or `None` for all.
    ///     window: The source window, as for `read_resampled`.
    ///     resampling: The method, as for `read_resampled`.
    ///     overview: The stored resolution, as for `read_resampled`.
    ///
    /// Returns:
    ///     numpy.ndarray: uint8, 0 for invalid pixels and 255 for valid ones (or values between
    ///     where a kernel resampled the mask), `(height, width)` when `band` is an int, otherwise
    ///     `(bands, height, width)`.
    ///
    /// Raises:
    ///     InvalidArgumentError: As `read_resampled`.
    #[pyo3(signature = (size, band=None, window=None, resampling="nearest", overview=None))]
    fn read_resampled_masks<'py>(
        &mut self,
        py: Python<'py>,
        size: (usize, usize),
        band: Option<&Bound<'py, PyAny>>,
        window: Option<(f64, f64, f64, f64)>,
        resampling: &str,
        overview: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let (options, single) = self.resampled_options(size, band, window, resampling, overview)?;
        let complex = self.is_complex();
        let inner = &mut self.inner;
        let mut masks = py.detach_result(|| inner.read_resampled_masks(&options)).py()?;
        let (w, h) = size;
        let plane = w * h;
        if complex {
            // A complex band's mask is its real part's.
            masks = masks.chunks(plane).step_by(2).flatten().copied().collect();
        }
        let bands = masks.len() / plane.max(1);
        let shape = if single { vec![h, w] } else { vec![bands, h, w] };
        to_numpy(py, RasterData::U8(masks), &shape)
    }

    /// Reads what `read_resampled` and `read_resampled_masks` read, at once: nodata masks are
    /// computed from the pixels read, rather than from a second read of their bands.
    ///
    /// Args:
    ///     size: The output `(width, height)`.
    ///     band: The band (an int, numbered from 1), a sequence of bands, or `None` for all.
    ///     window: The source window, as for `read_resampled`.
    ///     resampling: The method, as for `read_resampled`.
    ///     overview: The stored resolution, as for `read_resampled`.
    ///
    /// Returns:
    ///     tuple[numpy.ndarray, numpy.ndarray]: The pixels, as `read_resampled` returns them, and
    ///     the uint8 masks, as `read_resampled_masks` returns them.
    ///
    /// Raises:
    ///     InvalidArgumentError: As `read_resampled`.
    #[pyo3(signature = (size, band=None, window=None, resampling="nearest", overview=None))]
    fn read_resampled_with_masks<'py>(
        &mut self,
        py: Python<'py>,
        size: (usize, usize),
        band: Option<&Bound<'py, PyAny>>,
        window: Option<(f64, f64, f64, f64)>,
        resampling: &str,
        overview: Option<&Bound<'py, PyAny>>,
    ) -> PyResult<(Bound<'py, PyAny>, Bound<'py, PyAny>)> {
        let (options, single) = self.resampled_options(size, band, window, resampling, overview)?;
        let complex = self.is_complex();
        let inner = &mut self.inner;
        let (ds, mut masks) = py.detach_result(|| inner.read_resampled_with_masks(&options)).py()?;
        let r = ds.raster;
        let plane = r.rows * r.cols;
        let bands = if complex { r.bands / 2 } else { r.bands };
        let shape = if single { vec![r.rows, r.cols] } else { vec![bands, r.rows, r.cols] };
        if complex {
            // A complex band's mask is its real part's.
            masks = masks.chunks(plane).step_by(2).flatten().copied().collect();
        }
        let masks = to_numpy(py, RasterData::U8(masks), &shape)?;
        let pixels = if complex { complex_to_numpy(py, r.data, plane, &shape)? } else { to_numpy(py, r.data, &shape)? };
        Ok((pixels, masks))
    }

    /// Whether the raster has a per-dataset mask (a GeoTIFF's internal mask, GDAL's
    /// `GMF_PER_DATASET`).
    #[getter]
    fn has_mask(&self) -> bool {
        self.inner.has_mask()
    }

    /// The block (tile or strip) size `(width, height)` reads are best aligned to, when the raster
    /// is read on demand.
    #[getter]
    fn block_size(&self) -> Option<(usize, usize)> {
        self.inner.block_size()
    }

    /// Reads the per-dataset mask: 255 for valid pixels, 0 for invalid ones.
    ///
    /// Args:
    ///     window: The pixels to read, `(col_off, row_off, width, height)`, or `None` for the
    ///         whole image (of the overview, with `overview`).
    ///     overview: Read the mask of this overview (0 is the largest).
    ///
    /// Returns:
    ///     numpy.ndarray | None: A uint8 array `(rows, cols)`, or `None` without a mask.
    ///
    /// Raises:
    ///     InvalidArgumentError: If the window or the overview does not exist.
    #[pyo3(signature = (window=None, overview=None))]
    fn read_mask<'py>(
        slf: &Bound<'py, Self>,
        window: Option<(usize, usize, usize, usize)>,
        overview: Option<usize>,
    ) -> PyResult<Option<Bound<'py, PyAny>>> {
        let py = slf.py();
        Self::with_reader(slf, |inner| {
            let window = match window {
                Some((x, y, w, h)) => Window::new(x, y, w, h),
                None => {
                    let (w, h) = match overview {
                        None => (inner.width(), inner.height()),
                        Some(i) => inner.overviews().get(i).copied().ok_or_else(|| invalid(format!("no overview {i}")))?,
                    };
                    Window::full(w, h)
                }
            };
            let mask = py.detach_result(|| inner.read_mask(window, overview)).py()?;
            mask.map(|m| to_numpy(py, RasterData::U8(m), &[window.height, window.width])).transpose()
        })
    }

    /// Internal overview metadata from the native mask discovery, without reading pixels.
    fn _overview_has_mask(&self, overview: usize) -> PyResult<bool> {
        self.inner.overviews().get(overview).ok_or_else(|| invalid(format!("no overview {overview}")))?;
        Ok(self.inner.level_has_own_mask(overview))
    }

    /// Internal xarray metadata for a selected overview's own nodata values.
    fn _overview_nodata(&self, overview: usize) -> PyResult<Vec<Option<f64>>> {
        self.inner.overviews().get(overview).ok_or_else(|| invalid(format!("no overview {overview}")))?;
        let bands: Vec<usize> = (1..=self.inner.band_count()).collect();
        Ok(self.inner.level_band_nodata(&bands, Some(overview)))
    }

    /// Internal xarray mask read: only the selected level's own dataset mask, without
    /// sampling the full-resolution mask when an overview has none (GDAL 3.13.3).
    #[pyo3(signature = (window, overview=None))]
    fn _read_level_mask<'py>(
        slf: &Bound<'py, Self>,
        window: (usize, usize, usize, usize),
        overview: Option<usize>,
    ) -> PyResult<Option<Bound<'py, PyAny>>> {
        let py = slf.py();
        let (x, y, width, height) = window;
        let window = Window::new(x, y, width, height);
        Self::with_reader(slf, |inner| {
            let mask = py.detach_result(|| inner.read_level_mask(window, overview)).py()?;
            mask.map(|m| to_numpy(py, RasterData::U8(m), &[height, width])).transpose()
        })
    }

    /// Creates a raster from a NumPy array.
    ///
    /// Args:
    ///     array: A 2-D `(rows, cols)` or 3-D `(bands, rows, cols)` integer, float or complex
    ///         array (booleans are stored as uint8). Complex bands count once.
    ///     complex_dtype: Optional CInt16, CInt32, CFloat32 or CFloat64 output sample type for
    ///         complex input; complex_int16/complex_int32/complex64/complex128 are aliases.
    ///         Integer components are rounded and saturated as GDAL 3.13.3 does.
    ///     transform: The GDAL-style geotransform `(x_origin, pixel_width, row_rotation,
    ///         y_origin, column_rotation, pixel_height)`: north-up, rotated or south-up.
    ///     crs: The CRS: `"EPSG:4326"`, WKT or a PROJ string. `None` leaves it unknown.
    ///     nodata: The nodata value, or `None`.
    ///     metadata: Dataset metadata items: a mapping, or `(key, value)` pairs. GeoTIFFs store
    ///         them in the `GDAL_METADATA` tag.
    ///
    /// Returns:
    ///     Raster: The raster, in memory.
    ///
    /// Raises:
    ///     InvalidArgumentError: If the array is not supported or the transform not invertible.
    ///     CRSError: If the CRS cannot be parsed.
    #[staticmethod]
    #[pyo3(signature = (array, transform, crs=None, nodata=None, metadata=None, *, complex_dtype=None))]
    fn from_array(
        array: &Bound<'_, PyUntypedArray>,
        transform: [f64; 6],
        crs: Option<&str>,
        nodata: Option<f64>,
        metadata: Option<&Bound<'_, PyAny>>,
        complex_dtype: Option<&str>,
    ) -> PyResult<Self> {
        let metadata = metadata.map(string_pairs).transpose()?.unwrap_or_default();
        let shape = array.shape().to_vec();
        let (bands, rows, cols) = match shape[..] {
            [r, c] => (1, r, c),
            [b, r, c] => (b, r, c),
            _ => return Err(invalid(format!("expected a 2-D or 3-D array, got {} dimensions", shape.len()))),
        };
        if bands == 0 || rows == 0 || cols == 0 {
            return Err(invalid("the array is empty"));
        }
        let g = transform;
        if g[1] * g[5] - g[2] * g[4] == 0.0 || g.iter().any(|v| !v.is_finite()) {
            return Err(invalid("the transform is not invertible"));
        }
        let crs = crs.map(SpatialRef::parse).transpose().py()?.unwrap_or_default();
        let target = complex_dtype
            .map(|name| self::complex_dtype(name).ok_or_else(|| invalid(format!("unsupported complex sample type {name}"))))
            .transpose()?;
        let (data, complex) = raster_array_data(array, target)?;
        let cfg = terrio::RasterConfig {
            cols,
            rows,
            bands: bands * if complex { 2 } else { 1 },
            nodata: nodata.unwrap_or(-9999.0),
            data_type: data.data_type(),
            crs: crs.to_crs_info(),
            metadata,
            ..Default::default()
        };
        let raster = terrio::raster_from_data(cfg, data).py()?;
        let mut ds = RasterDataset::new(raster, nodata.is_some());
        ds.complex = complex;
        ds.set_geo_transform(g);
        Ok(Self::from_dataset(ds))
    }

    /// Writes the raster to a file.
    ///
    /// Args:
    ///     path: The output path. Its extension tells the format unless `driver` is given.
    ///     driver: The format's short name, such as `"GTiff"`, `"COG"` or `"AAIGrid"`.
    ///     compress: GeoTIFF/COG compression: `"NONE"`, `"LZW"`, `"DEFLATE"`, `"PACKBITS"`,
    ///         `"JPEG"` or `"WEBP"`.
    ///     tile_size: Write GeoTIFF tiles of this size (COG tiles default to 512).
    ///     bigtiff: Write a BigTIFF.
    ///     options: Format-specific creation options, as GDAL names them, such as
    ///         `{"TILED": "YES", "BLOCKXSIZE": "256", "INTERLEAVE": "BAND"}` for GeoTIFF,
    ///         `{"FORMAT": "ZARR_V3", "GEOREFERENCING_CONVENTION": "SPATIAL_PROJ"}` for Zarr, or
    ///         `{"FORMAT": "NC4", "COMPRESS": "DEFLATE", "ZLEVEL": "6"}` for netCDF-4.
    ///     band_metadata: Band metadata, one dictionary per band with any of the keys
    ///         `description`, `scale`, `offset`, `unit`, `color_interp` (a GDAL name such as
    ///         `"Red"`) and `items` (a mapping of metadata items). JPEG 2000 also accepts
    ///         `color_association` to retain a stored CDEF colour association when copying;
    ///         leave it unset when assigning new colour roles.
    ///     colormap: The color table of a single-band Byte or UInt16 GeoTIFF: `(r, g, b, a)`
    ///         entries from index 0.
    ///     overviews: GeoTIFF overviews to build, as decimation factors such as `[2, 4]`.
    ///     overview_resampling: As GDAL names them: `"nearest"`, `"average"`, `"rms"`,
    ///         `"bilinear"`, `"cubic"`, `"cubicspline"`, `"lanczos"`, `"mode"` (and terrio's
    ///         `"min"` and `"max"` for GeoTIFF overviews). GeoTIFF overviews default to
    ///         `"nearest"`; COGs to GDAL's COG driver's `"cubic"` (`"nearest"` when paletted).
    ///         The `OVERVIEW_RESAMPLING` and `RESAMPLING` creation options come first.
    ///     gcps: GeoTIFF ground control points, in the raster's CRS: `(pixel, line, x, y, z)`
    ///         tuples, written as tie points instead of the geotransform.
    ///     mask: A per-dataset validity mask, a 2-D uint8 array (0 for invalid pixels), written
    ///         as GDAL's internal GeoTIFF mask. If omitted, GeoTIFF/COG preserve the source mask.
    ///     rpc: The 92 rational polynomial coefficients of the GeoTIFF `RPCCoefficientTag`.
    ///
    /// Raises:
    ///     TerrioError: If the raster cannot be written.
    #[pyo3(signature = (
        path, driver=None, compress=None, tile_size=None, bigtiff=false, options=None, *,
        band_metadata=None, colormap=None, overviews=None, overview_resampling=None, gcps=None,
        mask=None, rpc=None
    ))]
    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        py: Python<'_>,
        path: &str,
        driver: Option<&str>,
        compress: Option<String>,
        tile_size: Option<u32>,
        bigtiff: bool,
        options: Option<std::collections::BTreeMap<String, String>>,
        band_metadata: Option<Vec<Bound<'_, PyDict>>>,
        colormap: Option<Vec<[u8; 4]>>,
        overviews: Option<Vec<usize>>,
        overview_resampling: Option<String>,
        gcps: Option<Vec<GcpTuple>>,
        mask: Option<numpy::PyReadonlyArray2<'_, u8>>,
        rpc: Option<Vec<f64>>,
    ) -> PyResult<()> {
        let mut options_ = write_options(driver, compress, tile_size, bigtiff)?;
        options_.gcps = parse_gcps(gcps);
        options_.rpc = rpc.unwrap_or_default();
        if let Some(m) = mask {
            let shape = m.shape();
            if shape != [self.inner.height(), self.inner.width()] {
                return Err(invalid(format!(
                    "the mask is {}x{}, the raster {}x{}",
                    shape[0],
                    shape[1],
                    self.inner.height(),
                    self.inner.width()
                )));
            }
            options_.mask = Some(m.as_array().iter().copied().collect());
        }
        options_.creation_options = validated_creation_options(options)?;
        options_.band_metadata = band_metadata.unwrap_or_default().iter().map(parse_band_metadata).collect::<PyResult<_>>()?;
        options_.color_table = colormap.unwrap_or_default();
        options_.overviews = overviews.unwrap_or_default();
        options_.overview_resampling = overview_resampling;
        self.write_with_options(py, path, options_)
    }

    /// Encodes the raster in a file format, in memory, preserving its dataset mask for GeoTIFF/COG.
    ///
    /// Args:
    ///     filename: A file name whose extension tells the format, such as `"out.tif"`.
    ///     driver: The format's short name, overriding the extension.
    ///     compress: GeoTIFF/COG compression, as for `write`.
    ///     tile_size: GeoTIFF tile size, as for `write`.
    ///
    /// Returns:
    ///     bytes: The file's contents. Formats made of several files come as a zip archive.
    #[pyo3(signature = (filename="raster.tif", driver=None, compress=None, tile_size=None))]
    #[allow(clippy::wrong_self_convention)]
    fn to_bytes<'py>(
        &mut self,
        py: Python<'py>,
        filename: &str,
        driver: Option<&str>,
        compress: Option<String>,
        tile_size: Option<u32>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let mut options = write_options(driver, compress, tile_size, false)?;
        let bytes = py
            .detach_result(|| {
                self.inner.prepare_output_mask(filename, &mut options, Window::full(self.inner.width(), self.inner.height()), None)?;
                self.with_dataset(|ds| terrio::memory::write_raster_bytes(ds, filename, &options))
            })
            .py()?;
        Ok(PyBytes::new(py, &bytes))
    }

    /// Reads the whole raster, for `numpy.asarray(raster)`.
    ///
    /// Args:
    ///     dtype: The dtype to convert to, or `None` for the raster's own.
    ///     copy: Accepted for NumPy compatibility.
    ///
    /// Returns:
    ///     numpy.ndarray: The pixels, `(bands, rows, cols)`.
    #[pyo3(signature = (dtype=None, copy=None))]
    fn __array__<'py>(slf: &Bound<'py, Self>, dtype: Option<&Bound<'py, PyAny>>, copy: Option<bool>) -> PyResult<Bound<'py, PyAny>> {
        let _ = copy;
        Self::read(slf, None, None, None, dtype)
    }

    /// The raster as a PNG of up to 512 pixels a side, which notebooks display; `None` when it
    /// cannot be drawn (complex bands, say), so that the notebook falls back to the text form.
    fn _repr_png_(&mut self, py: Python<'_>) -> Option<Py<pyo3::types::PyBytes>> {
        let options = terrio::ops::render::RenderOptions { max_size: crate::render::NOTEBOOK_SIZE, ..Default::default() };
        let inner = &mut self.inner;
        let png = py.detach_result(|| terrio::ops::render::render_raster(inner, &options)?.to_png()).ok()?;
        Some(pyo3::types::PyBytes::new(py, &png).unbind())
    }

    fn __repr__(&self) -> String {
        format!(
            "<terrio.Raster {}x{}x{} {} crs={} driver={}{}>",
            self.inner.band_count(),
            self.inner.height(),
            self.inner.width(),
            self.dtype(),
            self.crs_ref().label(),
            self.driver(),
            if self.inner.is_lazy() { " lazy" } else { "" }
        )
    }
}

/// A GCP as Python gives it: `(pixel, line, x, y, z)`.
pub(crate) type GcpTuple = (f64, f64, f64, f64, f64);

/// GCPs from `(pixel, line, x, y, z)` tuples.
pub(crate) fn parse_gcps(gcps: Option<Vec<GcpTuple>>) -> Vec<terrio::Gcp> {
    gcps.unwrap_or_default()
        .into_iter()
        .enumerate()
        .map(|(i, (pixel, line, x, y, z))| terrio::Gcp { id: (i + 1).to_string(), info: String::new(), pixel, line, x, y, z })
        .collect()
}

/// Raster write options from Python arguments.
pub(crate) fn write_options(
    driver: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<RasterWriteOptions> {
    let driver = driver.map(|d| RasterDriver::from_name(d).ok_or_else(|| invalid(format!("unknown raster format '{d}'")))).transpose()?;
    Ok(RasterWriteOptions { driver, compression: compress, tile_size, bigtiff, ..Default::default() })
}

/// Output options shared by operations that write a raster destination.
pub(crate) fn write_options_with_creation(
    driver: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
    creation_options: Option<std::collections::BTreeMap<String, String>>,
) -> PyResult<RasterWriteOptions> {
    let mut options = write_options(driver, compress, tile_size, bigtiff)?;
    options.creation_options = validated_creation_options(creation_options)?;
    Ok(options)
}

/// Raster creation-option mappings have case-insensitive, unambiguous names across writers.
pub(crate) fn validated_creation_options(mapping: Option<std::collections::BTreeMap<String, String>>) -> PyResult<Vec<(String, String)>> {
    let mut keys = std::collections::HashSet::new();
    let mut options = Vec::new();
    for (key, value) in mapping.unwrap_or_default() {
        if !keys.insert(key.to_ascii_uppercase()) {
            return Err(invalid(format!("duplicate creation option '{key}' (names are case-insensitive)")));
        }
        options.push((key, value));
    }
    Ok(options)
}

/// `(key, value)` string pairs from a mapping or a sequence of pairs, in order.
pub(crate) fn string_pairs(obj: &Bound<'_, PyAny>) -> PyResult<Vec<(String, String)>> {
    let items = if obj.hasattr("items")? { obj.call_method0("items")? } else { obj.clone() };
    let mut out = Vec::new();
    for item in items.try_iter()? {
        let (k, v): (Bound<'_, PyAny>, Bound<'_, PyAny>) = item?.extract()?;
        out.push((k.str()?.to_string(), v.str()?.to_string()));
    }
    Ok(out)
}

/// A band's metadata from a dictionary with the keys `description`, `scale`, `offset`, `unit`,
/// `color_interp`, `color_association` and `items`.
pub(crate) fn parse_band_metadata(d: &Bound<'_, PyDict>) -> PyResult<terrio::BandMetadata> {
    let get = |key: &str| -> PyResult<Option<Bound<'_, PyAny>>> { Ok(d.get_item(key)?.filter(|v| !v.is_none())) };
    for key in d.keys() {
        let key: String = key.extract()?;
        if !matches!(key.as_str(), "description" | "scale" | "offset" | "unit" | "color_interp" | "color_association" | "items") {
            return Err(invalid(format!("unknown band metadata key '{key}'")));
        }
    }
    Ok(terrio::BandMetadata {
        description: get("description")?.map(|v| v.extract()).transpose()?,
        scale: get("scale")?.map(|v| v.extract()).transpose()?,
        offset: get("offset")?.map(|v| v.extract()).transpose()?,
        unit: get("unit")?.map(|v| v.extract()).transpose()?,
        color_interp: get("color_interp")?.map(|v| v.extract()).transpose()?,
        color_association: get("color_association")?.map(|v| v.extract()).transpose()?,
        items: get("items")?.map(|v| string_pairs(&v)).transpose()?.unwrap_or_default(),
    })
}

/// A GeoTIFF written a few rows at a time, without the whole raster in memory: rows of blocks are
/// compressed and written as soon as they are complete, the image directory at the end. The
/// lossless compressions, tiles or strips, interleaving, georeferencing (rotated or by GCPs),
/// metadata, color tables, RPCs and an internal mask are written as `Raster.write` writes them;
/// overviews and COGs, which need the whole raster, are not.
///
/// Args:
///     path: The output path.
///     width: The number of columns.
///     height: The number of rows.
///     count: The number of bands.
///     dtype: The NumPy data type name, such as `"uint16"`.
///     transform: The GDAL-style geotransform (north-up, rotated or south-up).
///     crs: The CRS, or `None`.
///     nodata: The nodata value, or `None`.
///     metadata: Dataset metadata items.
///     compress: `"NONE"`, `"LZW"`, `"DEFLATE"`, `"ZSTD"` or `"PACKBITS"`.
///     tile_size: Tiles of this size (else strips).
///     bigtiff: Write a BigTIFF (by default, when the file may exceed 4 GB).
///     options: GDAL creation options (`TILED`, `BLOCKXSIZE`, `BLOCKYSIZE`, `INTERLEAVE`,
///         `PHOTOMETRIC`, `ALPHA`, `PREDICTOR`, `GEOREFERENCED`).
///     band_metadata: One dictionary per band, as for `Raster.write`.
///     colormap: A color table, as for `Raster.write`.
///     gcps: Ground control points `(pixel, line, x, y, z)`, written instead of the transform.
///     rpc: The 92 RPC coefficients.
///     mask: Write an internal mask, whose rows `write` then takes too.
///
/// Raises:
///     TerrioError: If the options are not supported or the file cannot be created.
#[pyclass(module = "terrio", name = "GeoTiffWriter")]
pub struct PyGeoTiffWriter {
    inner: Option<terrio::GeoTiffWriter>,
    width: usize,
    count: usize,
    data_type: DataType,
    complex: bool,
}

#[pymethods]
impl PyGeoTiffWriter {
    #[new]
    #[pyo3(signature = (
        path, width, height, count, dtype, transform, crs=None, nodata=None, metadata=None, *,
        compress=None, tile_size=None, bigtiff=false, options=None, band_metadata=None, colormap=None,
        gcps=None, rpc=None, mask=false
    ))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        path: &str,
        width: usize,
        height: usize,
        count: usize,
        dtype: &str,
        transform: [f64; 6],
        crs: Option<&str>,
        nodata: Option<f64>,
        metadata: Option<&Bound<'_, PyAny>>,
        compress: Option<String>,
        tile_size: Option<u32>,
        bigtiff: bool,
        options: Option<std::collections::BTreeMap<String, String>>,
        band_metadata: Option<Vec<Bound<'_, PyDict>>>,
        colormap: Option<Vec<[u8; 4]>>,
        gcps: Option<Vec<GcpTuple>>,
        rpc: Option<Vec<f64>>,
        mask: bool,
    ) -> PyResult<Self> {
        if width == 0 || height == 0 || count == 0 {
            return Err(invalid("the raster is empty"));
        }
        let (data_type, complex) = storage_type(dtype)?;
        let crs = crs.map(SpatialRef::parse).transpose().py()?.unwrap_or_default();
        let cfg = terrio::RasterConfig {
            cols: 1,
            rows: 1,
            bands: 1,
            nodata: nodata.unwrap_or(-9999.0),
            data_type,
            crs: crs.to_crs_info(),
            metadata: metadata.map(string_pairs).transpose()?.unwrap_or_default(),
            ..Default::default()
        };
        let mut raster = terrio::Raster::new(cfg);
        raster.cols = width;
        raster.rows = height;
        raster.bands = count * if complex { 2 } else { 1 };
        raster.data = RasterData::new_filled(data_type, 0, 0.0);
        let mut header = RasterDataset::new(raster, nodata.is_some());
        header.complex = complex;
        header.set_geo_transform(transform);
        let mut o = write_options(Some("GTiff"), compress, tile_size, bigtiff)?;
        o.creation_options = validated_creation_options(options)?;
        o.band_metadata = band_metadata.unwrap_or_default().iter().map(parse_band_metadata).collect::<PyResult<_>>()?;
        o.color_table = colormap.unwrap_or_default();
        o.gcps = parse_gcps(gcps);
        o.rpc = rpc.unwrap_or_default();
        if mask {
            o.mask = Some(Vec::new());
        }
        let inner = terrio::GeoTiffWriter::create(path, &header, &o).py()?;
        Ok(Self { inner: Some(inner), width, count, data_type, complex })
    }

    /// The number of rows written so far.
    #[getter]
    fn rows_written(&self) -> usize {
        self.inner.as_ref().map_or(0, |w| w.rows_written())
    }

    /// Appends rows: a `(count, rows, width)` array (or `(rows, width)` for one band) of the
    /// writer's dtype, and the mask's `(rows, width)` uint8 rows (0 for invalid) when it has one.
    ///
    /// Args:
    ///     array: The rows. Real output requires the matching NumPy dtype. Complex output
    ///         accepts complex64 or complex128 and converts components to the selected storage
    ///         type; integer components round and saturate as GDAL 3.13.3 does. Use complex128
    ///         input to avoid narrowing components before integer rounding.
    ///     mask: The mask's rows.
    ///
    /// Raises:
    ///     InvalidArgumentError: If the shapes or dtype do not match, or the writer is closed.
    #[pyo3(signature = (array, mask=None))]
    fn write(&mut self, py: Python<'_>, array: &Bound<'_, PyUntypedArray>, mask: Option<numpy::PyReadonlyArray2<'_, u8>>) -> PyResult<()> {
        let shape = array.shape().to_vec();
        let (bands, rows, cols) = match shape[..] {
            [r, c] => (1, r, c),
            [b, r, c] => (b, r, c),
            _ => return Err(invalid("expected a 2-D or 3-D array")),
        };
        if bands != self.count || cols != self.width {
            return Err(invalid(format!("expected {} bands of {} columns, got {bands} of {cols}", self.count, self.width)));
        }
        let (data, complex) = raster_array_data(array, self.complex.then_some(self.data_type))?;
        if complex != self.complex || data.data_type() != self.data_type {
            return Err(invalid("the array's dtype is not the writer's"));
        }
        let mask: Option<Vec<u8>> = mask.map(|m| m.as_array().iter().copied().collect());
        let w = self.inner.as_mut().ok_or_else(|| invalid("the writer is closed"))?;
        py.detach_result(|| w.write_rows(&data, rows, mask.as_deref())).py()
    }

    /// Writes the directory and closes the file.
    ///
    /// Raises:
    ///     InvalidArgumentError: If rows are missing.
    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        match self.inner.take() {
            Some(w) => py.detach_result(|| w.finish()).py(),
            None => Ok(()),
        }
    }
}

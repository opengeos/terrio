//! The `Raster` class: a raster in memory, or a GeoTIFF read by window.

use crate::{PyResultExt, crs_string, invalid};
use numpy::{PyArray1, PyArrayDescrMethods, PyArrayDyn, PyArrayMethods, PyUntypedArray, PyUntypedArrayMethods};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use terrio::crs::SpatialRef;
use terrio::formats::RasterDriver;
use terrio::{DataType, LazyRaster, RasterDataset, RasterWriteOptions, Window, WindowOptions};
use wbraster::raster::RasterData;

/// A raster dataset: a grid of pixels in one or more bands, with georeferencing.
///
/// Rasters come from `terrio.open`, `terrio.open_raster` or `Raster.from_array`. A raster opened
/// with `lazy=True` (GeoTIFFs and COGs, local or remote) holds only its headers; `read` then
/// decodes, and for remote files fetches, only the tiles a window needs.
///
/// Bands are numbered from 1 and overviews from 0, as in GDAL.
#[pyclass(module = "terrio", name = "Raster")]
pub struct PyRaster {
    pub(crate) inner: LazyRaster,
}

impl PyRaster {
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

    fn crs_ref(&self) -> SpatialRef {
        self.inner.crs()
    }

    /// Whether the bands are complex (each held as a real and an imaginary band).
    fn is_complex(&self) -> bool {
        self.inner.dataset().is_some_and(|d| d.complex)
    }
}

/// Complex values from bands of real parts followed by bands of imaginary parts (band `k`'s
/// parts are bands `2k` and `2k + 1`), as a NumPy array of `complex64` (`complex128` for
/// 64-bit floats) of the given shape.
fn complex_to_numpy<'py>(py: Python<'py>, data: RasterData, plane: usize, shape: &[usize]) -> PyResult<Bound<'py, PyAny>> {
    let double = data.data_type() == DataType::F64;
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

/// The NumPy name of a pixel type.
fn dtype_name(t: DataType) -> &'static str {
    match t {
        DataType::U8 => "uint8",
        DataType::I8 => "int8",
        DataType::U16 => "uint16",
        DataType::I16 => "int16",
        DataType::U32 => "uint32",
        DataType::I32 => "int32",
        DataType::U64 => "uint64",
        DataType::I64 => "int64",
        DataType::F32 => "float32",
        DataType::F64 => "float64",
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
                let a = array.cast::<PyArrayDyn<$t>>()?.readonly();
                return Ok(RasterData::$v(a.as_array().iter().copied().collect()));
            })*
        };
    }
    try_types!(u8 => U8, i8 => I8, u16 => U16, i16 => I16, u32 => U32, i32 => I32, u64 => U64, i64 => I64, f32 => F32, f64 => F64);
    if dtype.is_equiv_to(&numpy::dtype::<bool>(py)) {
        let a = array.cast::<PyArrayDyn<bool>>()?.readonly();
        return Ok(RasterData::U8(a.as_array().iter().map(|b| *b as u8).collect()));
    }
    Err(invalid(format!("unsupported array dtype {dtype}: use an integer or float32/float64 array")))
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
            (false, t) => dtype_name(t),
        }
    }

    /// Where the longitude and latitude of each pixel are, for swaths and other rasters
    /// georeferenced by geolocation arrays: GDAL's `GEOLOCATION` metadata items (`X_DATASET`,
    /// `Y_DATASET`, `PIXEL_STEP`...), or `None`.
    #[getter]
    fn geolocation(&self) -> Option<Vec<(String, String)>> {
        self.inner.dataset().and_then(|d| d.geolocation.as_ref()).map(|g| g.items())
    }

    /// The metadata items a driver other than the GeoTIFF one read, as `(key, value)` pairs:
    /// dataset items (netCDF's `NC_GLOBAL#title`, `NETCDF_DIM_time_VALUES`...), band items as
    /// `BAND_<n>#<key>`, and subdatasets as `SUBDATASET_<n>_NAME`. Empty for GeoTIFFs, whose
    /// metadata is in their `GDAL_METADATA` tag.
    #[getter]
    fn metadata(&self) -> Vec<(String, String)> {
        self.inner.metadata()
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
    ///
    /// Returns:
    ///     numpy.ndarray: An array of shape `(rows, cols)` when `band` is an int, otherwise
    ///     `(bands, rows, cols)`; complex bands come as `complex64` (or `complex128`) values.
    ///
    /// Raises:
    ///     InvalidArgumentError: If a band, the window or the overview does not exist.
    #[pyo3(signature = (band=None, window=None, overview=None))]
    fn read<'py>(
        &mut self,
        py: Python<'py>,
        band: Option<&Bound<'py, PyAny>>,
        window: Option<(usize, usize, usize, usize)>,
        overview: Option<usize>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let (mut bands, single) = parse_bands(band)?;
        let complex = self.is_complex();
        if complex {
            // A complex band's real and imaginary parts.
            if bands.is_empty() {
                bands = (1..=self.inner.band_count()).collect();
            } else {
                // Checked by the complex bands' numbers, which the parts' numbers are not.
                let count = self.count();
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
                        None => (self.inner.width(), self.inner.height()),
                        Some(i) => self.inner.overviews().get(i).copied().ok_or_else(|| {
                            invalid(format!("no overview {i}: the raster has {} overview(s)", self.inner.overviews().len()))
                        })?,
                    };
                Window::full(w, h)
            }
        };
        let options = WindowOptions { bands, overview };
        let inner = &mut self.inner;
        let ds = py.detach(|| inner.read_window(window, &options)).py()?;
        let r = ds.raster;
        if complex {
            let shape = if single { vec![r.rows, r.cols] } else { vec![r.bands / 2, r.rows, r.cols] };
            return complex_to_numpy(py, r.data, r.rows * r.cols, &shape);
        }
        let shape = if single { vec![r.rows, r.cols] } else { vec![r.bands, r.rows, r.cols] };
        to_numpy(py, r.data, &shape)
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
        &mut self,
        py: Python<'py>,
        window: Option<(usize, usize, usize, usize)>,
        overview: Option<usize>,
    ) -> PyResult<Option<Bound<'py, PyAny>>> {
        let window = match window {
            Some((x, y, w, h)) => Window::new(x, y, w, h),
            None => {
                let (w, h) = match overview {
                    None => (self.inner.width(), self.inner.height()),
                    Some(i) => self.inner.overviews().get(i).copied().ok_or_else(|| invalid(format!("no overview {i}")))?,
                };
                Window::full(w, h)
            }
        };
        let inner = &mut self.inner;
        let mask = py.detach(|| inner.read_mask(window, overview)).py()?;
        mask.map(|m| to_numpy(py, RasterData::U8(m), &[window.height, window.width])).transpose()
    }

    /// Creates a raster from a NumPy array.
    ///
    /// Args:
    ///     array: A 2-D `(rows, cols)` or 3-D `(bands, rows, cols)` array of an integer or float
    ///         dtype (booleans are stored as uint8).
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
    #[pyo3(signature = (array, transform, crs=None, nodata=None, metadata=None))]
    fn from_array(
        array: &Bound<'_, PyUntypedArray>,
        transform: [f64; 6],
        crs: Option<&str>,
        nodata: Option<f64>,
        metadata: Option<&Bound<'_, PyAny>>,
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
        let data = from_numpy(array)?;
        let cfg = terrio::RasterConfig {
            cols,
            rows,
            bands,
            nodata: nodata.unwrap_or(-9999.0),
            data_type: data.data_type(),
            crs: crs.to_crs_info(),
            metadata,
            ..Default::default()
        };
        let raster = terrio::Raster::from_data_native(cfg, data).map_err(terrio::Error::from).py()?;
        let mut ds = RasterDataset::new(raster, nodata.is_some());
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
    ///     band_metadata: GeoTIFF band metadata, one dictionary per band with any of the keys
    ///         `description`, `scale`, `offset`, `unit`, `color_interp` (a GDAL name such as
    ///         `"Red"`) and `items` (a mapping of metadata items).
    ///     colormap: The color table of a single-band Byte or UInt16 GeoTIFF: `(r, g, b, a)`
    ///         entries from index 0.
    ///     overviews: GeoTIFF overviews to build, as decimation factors such as `[2, 4]`.
    ///     overview_resampling: `"nearest"`, `"average"`, `"min"`, `"max"` or `"mode"`.
    ///         Defaults to `"nearest"` for GeoTIFF and `"average"` for COG.
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
        options_.creation_options = options.unwrap_or_default().into_iter().collect();
        options_.band_metadata = band_metadata.unwrap_or_default().iter().map(parse_band_metadata).collect::<PyResult<_>>()?;
        options_.color_table = colormap.unwrap_or_default();
        options_.overviews = overviews.unwrap_or_default();
        options_.overview_resampling = overview_resampling;
        let mut options = options_;
        // A raster read by window is written as a tile archive a tile row at a time.
        let driver = options.driver.or_else(|| terrio::formats::RasterDriver::for_extension(path));
        let tile_archive = matches!(driver, Some(terrio::formats::RasterDriver::PMTiles | terrio::formats::RasterDriver::MBTiles));
        py.detach(|| self.inner.prepare_output_mask(path, &mut options, Window::full(self.inner.width(), self.inner.height()), None))
            .py()?;
        if self.inner.dataset().is_none() && tile_archive {
            return py.detach(|| self.inner.write(path, &options)).py();
        }
        py.detach(|| self.with_dataset(|ds| terrio::write_raster_dataset(ds, path, &options))).py()
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
            .detach(|| {
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
    fn __array__<'py>(&mut self, py: Python<'py>, dtype: Option<&Bound<'py, PyAny>>, copy: Option<bool>) -> PyResult<Bound<'py, PyAny>> {
        let _ = copy;
        let a = self.read(py, None, None, None)?;
        match dtype {
            Some(d) => a.call_method1("astype", (d,)),
            None => Ok(a),
        }
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
/// `color_interp` and `items`.
pub(crate) fn parse_band_metadata(d: &Bound<'_, PyDict>) -> PyResult<terrio::BandMetadata> {
    let get = |key: &str| -> PyResult<Option<Bound<'_, PyAny>>> { Ok(d.get_item(key)?.filter(|v| !v.is_none())) };
    for key in d.keys() {
        let key: String = key.extract()?;
        if !matches!(key.as_str(), "description" | "scale" | "offset" | "unit" | "color_interp" | "items") {
            return Err(invalid(format!("unknown band metadata key '{key}'")));
        }
    }
    Ok(terrio::BandMetadata {
        description: get("description")?.map(|v| v.extract()).transpose()?,
        scale: get("scale")?.map(|v| v.extract()).transpose()?,
        offset: get("offset")?.map(|v| v.extract()).transpose()?,
        unit: get("unit")?.map(|v| v.extract()).transpose()?,
        color_interp: get("color_interp")?.map(|v| v.extract()).transpose()?,
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
        raster.bands = count;
        raster.data = RasterData::new_filled(data_type, 0, 0.0);
        let mut header = RasterDataset::new(raster, nodata.is_some());
        header.set_geo_transform(transform);
        let mut o = write_options(Some("GTiff"), compress, tile_size, bigtiff)?;
        o.creation_options = options.unwrap_or_default().into_iter().collect();
        o.band_metadata = band_metadata.unwrap_or_default().iter().map(parse_band_metadata).collect::<PyResult<_>>()?;
        o.color_table = colormap.unwrap_or_default();
        o.gcps = parse_gcps(gcps);
        o.rpc = rpc.unwrap_or_default();
        if mask {
            o.mask = Some(Vec::new());
        }
        let inner = terrio::GeoTiffWriter::create(path, &header, &o).py()?;
        Ok(Self { inner: Some(inner), width, count, data_type })
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
    ///     array: The rows.
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
        let data = from_numpy(array)?;
        if data.data_type() != self.data_type {
            return Err(invalid("the array's dtype is not the writer's"));
        }
        let mask: Option<Vec<u8>> = mask.map(|m| m.as_array().iter().copied().collect());
        let w = self.inner.as_mut().ok_or_else(|| invalid("the writer is closed"))?;
        py.detach(|| w.write_rows(&data, rows, mask.as_deref())).py()
    }

    /// Writes the directory and closes the file.
    ///
    /// Raises:
    ///     InvalidArgumentError: If rows are missing.
    fn close(&mut self, py: Python<'_>) -> PyResult<()> {
        match self.inner.take() {
            Some(w) => py.detach(|| w.finish()).py(),
            None => Ok(()),
        }
    }
}

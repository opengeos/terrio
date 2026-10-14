//! The `Raster` class: a raster in memory, or a GeoTIFF read by window.

use crate::{PyResultExt, crs_string, invalid};
use numpy::{PyArray1, PyArrayDescrMethods, PyArrayDyn, PyArrayMethods, PyUntypedArray, PyUntypedArrayMethods};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
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
fn from_numpy(array: &Bound<'_, PyUntypedArray>) -> PyResult<RasterData> {
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
        self.inner.band_count()
    }

    /// The shape of the array `read()` returns: `(count, height, width)`.
    #[getter]
    fn shape(&self) -> (usize, usize, usize) {
        (self.inner.band_count(), self.inner.height(), self.inner.width())
    }

    /// The pixel type, as a NumPy dtype name such as `"uint8"` or `"float32"`.
    #[getter]
    fn dtype(&self) -> &'static str {
        dtype_name(self.inner.data_type())
    }

    /// The nodata value, or `None` when the raster declares none.
    #[getter]
    fn nodata(&self) -> Option<f64> {
        self.inner.nodata()
    }

    /// The CRS: `"EPSG:xxxx"` when its EPSG code is known, otherwise its WKT, or `None`.
    #[getter]
    fn crs(&self) -> Option<String> {
        crs_string(&self.crs_ref())
    }

    /// The CRS as WKT, or `None` when it is unknown.
    #[getter]
    fn crs_wkt(&self) -> Option<String> {
        self.crs_ref().to_wkt()
    }

    /// The GDAL-style geotransform `(x_origin, pixel_width, 0, y_origin, 0, -pixel_height)`.
    #[getter]
    fn transform(&self) -> (f64, f64, f64, f64, f64, f64) {
        let g = self.inner.geo_transform();
        (g[0], g[1], g[2], g[3], g[4], g[5])
    }

    /// The pixel size `(x_resolution, y_resolution)`, both positive.
    #[getter]
    fn res(&self) -> (f64, f64) {
        let g = self.inner.geo_transform();
        (g[1].abs(), g[5].abs())
    }

    /// The extent `(min_x, min_y, max_x, max_y)` in CRS units.
    #[getter]
    fn bounds(&self) -> (f64, f64, f64, f64) {
        let g = self.inner.geo_transform();
        let (w, h) = (self.inner.width() as f64, self.inner.height() as f64);
        (g[0], g[3] + h * g[5], g[0] + w * g[1], g[3])
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
    ///     `(bands, rows, cols)`.
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
        let (bands, single) = parse_bands(band)?;
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
        let shape = if single { vec![r.rows, r.cols] } else { vec![r.bands, r.rows, r.cols] };
        to_numpy(py, r.data, &shape)
    }

    /// Creates a raster from a NumPy array.
    ///
    /// Args:
    ///     array: A 2-D `(rows, cols)` or 3-D `(bands, rows, cols)` array of an integer or float
    ///         dtype (booleans are stored as uint8).
    ///     transform: The GDAL-style geotransform `(x_origin, pixel_width, 0, y_origin, 0,
    ///         -pixel_height)` of a north-up raster.
    ///     crs: The CRS: `"EPSG:4326"`, WKT or a PROJ string. `None` leaves it unknown.
    ///     nodata: The nodata value, or `None`.
    ///
    /// Returns:
    ///     Raster: The raster, in memory.
    ///
    /// Raises:
    ///     InvalidArgumentError: If the array or the transform is not supported.
    ///     CRSError: If the CRS cannot be parsed.
    #[staticmethod]
    #[pyo3(signature = (array, transform, crs=None, nodata=None))]
    fn from_array(array: &Bound<'_, PyUntypedArray>, transform: [f64; 6], crs: Option<&str>, nodata: Option<f64>) -> PyResult<Self> {
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
        if g[2] != 0.0 || g[4] != 0.0 || g[1] <= 0.0 || g[5] >= 0.0 {
            return Err(invalid("only north-up transforms (no rotation, positive pixel width, negative pixel height) are supported"));
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
            ..Default::default()
        };
        let mut raster = terrio::Raster::from_data_native(cfg, data).map_err(terrio::Error::from).py()?;
        raster.x_min = g[0];
        raster.cell_size_x = g[1];
        raster.cell_size_y = -g[5];
        raster.y_min = g[3] + rows as f64 * g[5];
        Ok(Self::from_dataset(RasterDataset::new(raster, nodata.is_some())))
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
    ///
    /// Raises:
    ///     TerrioError: If the raster cannot be written.
    #[pyo3(signature = (path, driver=None, compress=None, tile_size=None, bigtiff=false))]
    fn write(
        &mut self,
        py: Python<'_>,
        path: &str,
        driver: Option<&str>,
        compress: Option<String>,
        tile_size: Option<u32>,
        bigtiff: bool,
    ) -> PyResult<()> {
        let options = write_options(driver, compress, tile_size, bigtiff)?;
        py.detach(|| self.with_dataset(|ds| terrio::write_raster_dataset(ds, path, &options))).py()
    }

    /// Encodes the raster in a file format, in memory.
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
        let options = write_options(driver, compress, tile_size, false)?;
        let bytes = py.detach(|| self.with_dataset(|ds| terrio::memory::write_raster_bytes(ds, filename, &options))).py()?;
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

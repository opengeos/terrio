//! The COG writer that writes a few rows at a time: `terrio.rio`'s COG writes stream through it.

use crate::Detach;
use crate::raster::{GcpTuple, from_numpy, parse_band_metadata, parse_gcps, string_pairs, write_options};
use crate::{PyResultExt, invalid};
use numpy::{PyUntypedArray, PyUntypedArrayMethods};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use terrio::crs::SpatialRef;
use terrio::{DataType, RasterDataset};
use wbraster::raster::RasterData;

/// A Cloud Optimized GeoTIFF written a few rows at a time, without the whole raster in memory:
/// each level's rows of tiles are compressed as they are complete (into temporary files), its
/// overviews computed from the rows they need as GDAL's COG driver computes them, and the file
/// laid out at the end, the same, byte for byte, as `Raster.write(..., driver="COG")` writes it.
/// Masks, overview factors, a source's overviews to copy and compressions only the whole raster
/// is encoded with are not written this way.
///
/// Args:
///     path: The output path.
///     width: The number of columns.
///     height: The number of rows.
///     count: The number of bands.
///     dtype: The NumPy data type name, such as `"uint16"`.
///     transform: The GDAL-style geotransform.
///     crs: The CRS, or `None`.
///     nodata: The nodata value, or `None`.
///     metadata: Dataset metadata items.
///     compress: The compression, as for `Raster.write`.
///     bigtiff: Write a BigTIFF (by default, when the file may exceed 4 GB).
///     options: GDAL creation options (COG's: `BLOCKSIZE`, `OVERVIEW_RESAMPLING`, `PREDICTOR`...).
///     band_metadata: One dictionary per band, as for `Raster.write`.
///     colormap: A color table, as for `Raster.write`.
///     gcps: Ground control points `(pixel, line, x, y, z)`, written instead of the transform.
///     rpc: The 92 RPC coefficients.
///     overview_resampling: The overviews' resampling when the options do not set it.
///
/// Raises:
///     TerrioError: If the options are not supported or the file cannot be created.
#[pyclass(module = "terrio", name = "CogWriter")]
pub struct PyCogWriter {
    inner: Option<terrio::GeoTiffWriter>,
    width: usize,
    count: usize,
    data_type: DataType,
}

#[pymethods]
impl PyCogWriter {
    #[new]
    #[pyo3(signature = (
        path, width, height, count, dtype, transform, crs=None, nodata=None, metadata=None, *,
        compress=None, bigtiff=false, options=None, band_metadata=None, colormap=None, gcps=None,
        rpc=None, overview_resampling=None
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
        bigtiff: bool,
        options: Option<std::collections::BTreeMap<String, String>>,
        band_metadata: Option<Vec<Bound<'_, PyDict>>>,
        colormap: Option<Vec<[u8; 4]>>,
        gcps: Option<Vec<GcpTuple>>,
        rpc: Option<Vec<f64>>,
        overview_resampling: Option<String>,
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
        let mut o = write_options(Some("COG"), compress, None, bigtiff)?;
        o.creation_options = options.unwrap_or_default().into_iter().collect();
        o.band_metadata = band_metadata.unwrap_or_default().iter().map(parse_band_metadata).collect::<PyResult<_>>()?;
        o.color_table = colormap.unwrap_or_default();
        o.gcps = parse_gcps(gcps);
        o.rpc = rpc.unwrap_or_default();
        o.overview_resampling = overview_resampling;
        let inner = terrio::GeoTiffWriter::create(path, &header, &o).py()?;
        Ok(Self { inner: Some(inner), width, count, data_type })
    }

    /// The number of rows written so far.
    #[getter]
    fn rows_written(&self) -> usize {
        self.inner.as_ref().map_or(0, |w| w.rows_written())
    }

    /// Appends rows: a `(count, rows, width)` array (or `(rows, width)` for one band) of the
    /// writer's dtype.
    ///
    /// Args:
    ///     array: The rows.
    ///
    /// Raises:
    ///     InvalidArgumentError: If the shapes or dtype do not match, or the writer is closed.
    fn write(&mut self, py: Python<'_>, array: &Bound<'_, PyUntypedArray>) -> PyResult<()> {
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
        let w = self.inner.as_mut().ok_or_else(|| invalid("the writer is closed"))?;
        py.detach_result(|| w.write_rows(&data, rows, None)).py()
    }

    /// Computes the last overview rows, lays the file out and closes it.
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

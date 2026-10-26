//! Pixel-wise raster operations: `unscale`, `nodata_to_alpha`, `reclassify` and `as_features`,
//! with keyword arguments named after the command-line options.

use crate::PyResultExt;
use crate::invalid;
use crate::ops::{finish_raster, input_raster, vector_write_options, write_mode};
use crate::raster::PyRaster;
use crate::vector::PyLayer;
use pyo3::prelude::*;
use terrio::ops::as_features::{AsFeaturesOptions, PixelGeometry};
use terrio::ops::parse_data_type;
use terrio::ops::pixel::{NodataToAlphaOptions, ReclassifyOptions, UnscaleOptions, read_mapping_file};

/// Registers the functions in the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(unscale, m)?)?;
    m.add_function(wrap_pyfunction!(nodata_to_alpha, m)?)?;
    m.add_function(wrap_pyfunction!(reclassify, m)?)?;
    m.add_function(wrap_pyfunction!(as_features, m)?)?;
    Ok(())
}

/// Applies band scales and offsets, like `terrio unscale` (GDAL's `gdal raster unscale`).
///
/// Args:
///     src: The raster: a path or a `Raster`.
///     dst: Also write the result to this path.
///     ot: The output data type; by default `"Float32"`, or `"Float64"` for Float64 sources.
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///
/// Returns:
///     Raster: `value * scale + offset` per band; nodata pixels keep the nodata value.
#[pyfunction]
#[pyo3(signature = (src, dst=None, *, ot=None, format=None, compress=None, tile_size=None, bigtiff=false))]
#[allow(clippy::too_many_arguments)]
fn unscale(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    ot: Option<&str>,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    let o = UnscaleOptions { output_type: ot.map(parse_data_type).transpose().py()? };
    let ds = input_raster(py, src)?;
    let out = py.detach(|| terrio::ops::pixel::unscale(&ds, &o)).py()?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff)
}

/// Replaces nodata values with an alpha band, like `terrio nodata-to-alpha` (GDAL's
/// `gdal raster nodata-to-alpha`).
///
/// Args:
///     src: The raster: a path or a `Raster`.
///     dst: Also write the result to this path.
///     nodata: A nodata value overriding the source's, or a sequence of one per band (then a
///         pixel is transparent where every band has its value).
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///
/// Returns:
///     Raster: The bands without nodata values, then an alpha band of their type (0 or 255).
#[pyfunction]
#[pyo3(signature = (src, dst=None, *, nodata=None, format=None, compress=None, tile_size=None, bigtiff=false))]
#[allow(clippy::too_many_arguments)]
fn nodata_to_alpha(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    nodata: Option<&Bound<'_, PyAny>>,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    let nodata: Vec<f64> = match nodata {
        None => Vec::new(),
        Some(v) => match v.extract::<f64>() {
            Ok(x) => vec![x],
            Err(_) => v.extract().map_err(|_| invalid("nodata is a number or a sequence of numbers"))?,
        },
    };
    let o = NodataToAlphaOptions { nodata };
    let ds = input_raster(py, src)?;
    let out = py.detach(|| terrio::ops::pixel::nodata_to_alpha(&ds, &o)).py()?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff)
}

/// Maps values or ranges of values to new values, like `terrio reclassify` (GDAL's
/// `gdal raster reclassify`).
///
/// Args:
///     src: The raster: a path or a `Raster`.
///     mapping: GDAL's mapping: `from=to` entries separated by `;`, `from` a value, an interval
///         (`"[a,b)"`, `"(a,b]"`, `"(-inf,b]"`...), `NO_DATA`, `NAN` or `DEFAULT`, `to` a value,
///         `NO_DATA` or `PASS_THROUGH`. `"@path"` reads the entries from a file, one per line.
///     dst: Also write the result to this path.
///     ot: The output data type; by default that of the source.
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///
/// Returns:
///     Raster: The reclassified bands, with the source's nodata values.
#[pyfunction]
#[pyo3(signature = (src, mapping, dst=None, *, ot=None, format=None, compress=None, tile_size=None, bigtiff=false))]
#[allow(clippy::too_many_arguments)]
fn reclassify(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    mapping: &str,
    dst: Option<&str>,
    ot: Option<&str>,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    let mapping = match mapping.strip_prefix('@') {
        Some(path) => read_mapping_file(path).py()?,
        None => mapping.to_string(),
    };
    let o = ReclassifyOptions { mapping, output_type: ot.map(parse_data_type).transpose().py()? };
    let ds = input_raster(py, src)?;
    let out = py.detach(|| terrio::ops::pixel::reclassify(&ds, &o)).py()?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff)
}

/// One vector feature per pixel, like `terrio as-features` (GDAL's `gdal raster as-features`).
///
/// Args:
///     src: The raster: a path or a `Raster`.
///     dst: Also write the features to this path.
///     bands: The bands to report, numbered from 1; all by default.
///     geometry_type: `"none"` (an attribute table), `"point"` (pixel centres) or `"polygon"`
///         (pixel squares).
///     skip_nodata: Leave out the pixels whose first reported band has its nodata value.
///     include_xy: Add `CENTER_X` and `CENTER_Y` fields.
///     include_row_col: Add `ROW` and `COL` fields.
///     layer_name: The layer name.
///     format: The output format's short name, for `dst`.
///     append: Add the features to the layer of the same name in an existing output.
///     overwrite: Replace the layer of the same name in an existing output.
///
/// Returns:
///     Layer: The features, with a `BAND_<n>` field per reported band.
#[pyfunction]
#[pyo3(signature = (
    src, dst=None, *, bands=None, geometry_type="none", skip_nodata=false, include_xy=false, include_row_col=false,
    layer_name="pixels", format=None, append=false, overwrite=false
))]
#[allow(clippy::too_many_arguments)]
fn as_features(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    bands: Option<Vec<usize>>,
    geometry_type: &str,
    skip_nodata: bool,
    include_xy: bool,
    include_row_col: bool,
    layer_name: &str,
    format: Option<&str>,
    append: bool,
    overwrite: bool,
) -> PyResult<PyLayer> {
    if dst.is_none() && (format.is_some() || append || overwrite) {
        return Err(invalid("format, append and overwrite require dst"));
    }
    let o = AsFeaturesOptions {
        bands: bands.unwrap_or_default(),
        geometry: PixelGeometry::parse(geometry_type).py()?,
        skip_nodata,
        include_xy,
        include_row_col,
        layer_name: layer_name.to_string(),
    };
    let ds = input_raster(py, src)?;
    let write = dst.map(|_| vector_write_options(format)).transpose()?;
    let mode = write_mode(append, overwrite)?;
    let layer = py
        .detach(|| -> terrio::Result<terrio::Layer> {
            let layer = terrio::ops::as_features::as_features(&ds, &o)?;
            if let (Some(path), Some(w)) = (dst, &write) {
                terrio::ops::vector_filter::write_vector_with_mode(std::slice::from_ref(&layer), path, w, mode)?;
            }
            Ok(layer)
        })
        .py()?;
    Ok(PyLayer { inner: layer })
}

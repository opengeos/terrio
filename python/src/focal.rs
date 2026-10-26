//! Focal statistics, collar cleaning and LUT enhancement: `neighbors`, `nearblack`,
//! `clean_collar`, `enhance` and `enhance_luts`, with keyword arguments.

use crate::ops::{finish_raster, input_raster, raster_path};
use crate::raster::{PyRaster, write_options};
use crate::{PyResultExt, invalid};
use pyo3::prelude::*;
use terrio::ops::enhance::{BandLut, EnhanceOptions};
use terrio::ops::nearblack::CollarRequest;
use terrio::ops::neighbors::{NeighborMethod, NeighborsNodata, NeighborsOptions};

/// Registers the functions in the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(neighbors, m)?)?;
    m.add_function(wrap_pyfunction!(nearblack, m)?)?;
    m.add_function(wrap_pyfunction!(clean_collar, m)?)?;
    m.add_function(wrap_pyfunction!(enhance, m)?)?;
    m.add_function(wrap_pyfunction!(enhance_luts, m)?)?;
    Ok(())
}

/// One string or a sequence of strings.
fn strings(v: &Bound<'_, PyAny>, what: &str) -> PyResult<Vec<String>> {
    if let Ok(s) = v.extract::<String>() {
        return Ok(vec![s]);
    }
    v.extract::<Vec<String>>().map_err(|_| invalid(format!("{what} is a string or a sequence of strings")))
}

/// Focal statistics and convolution kernels, like `terrio neighbors` (GDAL's
/// `gdal raster neighbors`).
///
/// Args:
///     src: The raster: a path or a `Raster`.
///     dst: Also write the result to this path; the returned Raster and intermediate
///         output are still held in memory. Use the CLI for strip-based file processing.
///     kernel: A kernel or a list of kernels: `"u"`, `"v"`, `"edge1"`, `"edge2"`, `"sharpen"`,
///         `"gaussian"`, `"unsharp-masking"`, `"equal"`, or a matrix such as
///         `"[[0,1,0],[1,1,1],[0,1,0]]"`. Each input band gives one output band per kernel.
///     method: `"sum"`, `"mean"`, `"min"`, `"max"`, `"stddev"`, `"median"` or `"mode"`, or one
///         per kernel. By default `"sum"` for kernels whose coefficients sum to zero, `"mean"`
///         for others.
///     size: The neighbourhood size (odd, 3 to 99).
///     band: The band to filter, from 1. Every band by default.
///     ot: The output data type (`"Float64"` by default).
///     nodata: The output nodata value: a number or `"none"`. By default the input band's.
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///
/// Returns:
///     Raster: The filtered raster, in memory.
#[pyfunction]
#[pyo3(signature = (
    src, dst=None, *, kernel, method=None, size=None, band=None, ot="Float64", nodata=None, format=None, compress=None,
    tile_size=None, bigtiff=false
))]
#[allow(clippy::too_many_arguments)]
fn neighbors(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    kernel: &Bound<'_, PyAny>,
    method: Option<&Bound<'_, PyAny>>,
    size: Option<usize>,
    band: Option<usize>,
    ot: &str,
    nodata: Option<&Bound<'_, PyAny>>,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    let methods = match method {
        Some(m) => strings(m, "method")?.iter().map(|m| NeighborMethod::parse(m)).collect::<terrio::Result<_>>().py()?,
        None => Vec::new(),
    };
    let nodata = match nodata {
        None => NeighborsNodata::Source,
        Some(v) => match v.extract::<String>() {
            Ok(s) => NeighborsNodata::parse(&s).py()?,
            Err(_) => NeighborsNodata::Value(v.extract().map_err(|_| invalid("nodata is a number or 'none'"))?),
        },
    };
    let o = NeighborsOptions {
        band,
        kernels: strings(kernel, "kernel")?,
        methods,
        size,
        output_type: terrio::ops::parse_data_type(ot).py()?,
        nodata,
    };
    let ds = input_raster(py, src)?;
    let out = py.detach(|| terrio::ops::neighbors::neighbors(&ds, &o)).py()?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff)
}

/// Runs a collar cleaning: `clean_collar` applies GDAL's clean-collar defaults (an input's alpha
/// band or mask kept).
#[allow(clippy::too_many_arguments)]
fn collar(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    colors: Option<&Bound<'_, PyAny>>,
    white: bool,
    color_threshold: i32,
    pixel_distance: i32,
    add_alpha: bool,
    add_mask: bool,
    algorithm: &str,
    keep: bool,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    let has_mask = keep
        && match raster_path(src)? {
            Some(path) => py.detach(|| terrio::open_raster_lazy(&path)).py()?.has_mask(),
            None => src.cast::<PyRaster>().map_err(|_| invalid("expected a path or a terrio.Raster"))?.borrow().inner.has_mask(),
        };
    let ds = input_raster(py, src)?;
    let colors = match colors {
        None => Vec::new(),
        Some(c) => {
            let flat = c.extract::<Vec<i32>>().is_ok_and(|v| !v.is_empty());
            let items: Vec<Bound<'_, PyAny>> =
                if c.extract::<String>().is_ok() || flat { vec![c.clone()] } else { c.try_iter()?.collect::<PyResult<_>>()? };
            items
                .iter()
                .map(|item| match item.extract::<String>() {
                    Ok(text) => Ok(text),
                    Err(_) => item
                        .extract::<Vec<i32>>()
                        .map(|components| components.iter().map(i32::to_string).collect::<Vec<_>>().join(","))
                        .map_err(|_| invalid("a color is 'black', 'white', 'r,g,b' or a sequence of integers")),
                })
                .collect::<PyResult<Vec<_>>>()?
        }
    };
    let o = CollarRequest {
        colors,
        white,
        near: color_threshold,
        max_non_black: pixel_distance,
        set_alpha: add_alpha,
        set_mask: add_mask,
        algorithm: Some(algorithm.into()),
        clean_collar: keep,
    }
    .resolve(&ds, has_mask)
    .py()?;
    if o.set_mask && dst.is_none() {
        return Err(invalid("a mask (add_mask, or an input with a mask) needs dst, the file that holds it"));
    }
    let collar = py.detach(|| terrio::ops::nearblack::nearblack(&ds, &o)).py()?;
    let Some(mask) = collar.mask else { return finish_raster(py, collar.dataset, dst, format, compress, tile_size, bigtiff) };
    // A mask is kept by the file it is written to.
    let path = dst.expect("mask output requires dst, checked before processing");
    let mut options = write_options(format, compress, tile_size, bigtiff)?;
    options.mask = Some(mask);
    py.detach(|| terrio::write_raster_dataset(&collar.dataset, path, &options)).py()?;
    Ok(PyRaster { inner: py.detach(|| terrio::open_raster_lazy(path)).py()? })
}

/// Sets the nearly black (or white) borders of an image to exact black (or white), like
/// `terrio nearblack` (GDAL's `nearblack`, two scan passes by default).
///
/// Args:
///     src: The raster: a path or a `Raster` (normally of Byte bands).
///     dst: Also write the result to this path (needed with `add_mask`).
///     colors: The collar colors: `"black"`, `"white"`, `"r,g,b"` or sequences of integers, one
///         value per non-alpha band. Black by default.
///     white: Search for nearly white pixels, set to 255.
///     color_threshold: How far from the colors values can be (`-near`).
///     pixel_distance: How many consecutive non-collar pixels the search goes past (`-nb`).
///     add_alpha: Add an alpha band, transparent in the collar.
///     add_mask: Add a per-dataset mask, invalid in the collar (written with `dst`).
///     algorithm: `"twopasses"` or `"floodfill"`.
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///
/// Returns:
///     Raster: The cleaned Byte raster (read from `dst` when it has a mask).
#[pyfunction]
#[pyo3(signature = (
    src, dst=None, *, colors=None, white=false, color_threshold=15, pixel_distance=2, add_alpha=false, add_mask=false,
    algorithm="twopasses", format=None, compress=None, tile_size=None, bigtiff=false
))]
#[allow(clippy::too_many_arguments)]
fn nearblack(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    colors: Option<&Bound<'_, PyAny>>,
    white: bool,
    color_threshold: i32,
    pixel_distance: i32,
    add_alpha: bool,
    add_mask: bool,
    algorithm: &str,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    collar(
        py,
        src,
        dst,
        colors,
        white,
        color_threshold,
        pixel_distance,
        add_alpha,
        add_mask,
        algorithm,
        false,
        format,
        compress,
        tile_size,
        bigtiff,
    )
}

/// Cleans the noisy collar of an image, like `terrio clean-collar` (GDAL's
/// `gdal raster clean-collar`): a flood fill from the edges by default, and an input's alpha band
/// or mask is kept updated.
///
/// Args:
///     src: The raster: a path or a `Raster` (normally of Byte bands).
///     dst: Also write the result to this path (needed when the output has a mask).
///     colors: The collar colors: `"black"`, `"white"`, `"r,g,b"` or sequences of integers.
///     white: Search for nearly white pixels, set to 255.
///     color_threshold: How far from the colors values can be.
///     pixel_distance: How many consecutive non-collar pixels the search goes past.
///     add_alpha: Add an alpha band, transparent in the collar.
///     add_mask: Add a per-dataset mask, invalid in the collar (written with `dst`).
///     algorithm: `"floodfill"` or `"twopasses"`.
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///
/// Returns:
///     Raster: The cleaned Byte raster (read from `dst` when it has a mask).
#[pyfunction]
#[pyo3(signature = (
    src, dst=None, *, colors=None, white=false, color_threshold=15, pixel_distance=2, add_alpha=false, add_mask=false,
    algorithm="floodfill", format=None, compress=None, tile_size=None, bigtiff=false
))]
#[allow(clippy::too_many_arguments)]
fn clean_collar(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    colors: Option<&Bound<'_, PyAny>>,
    white: bool,
    color_threshold: i32,
    pixel_distance: i32,
    add_alpha: bool,
    add_mask: bool,
    algorithm: &str,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    collar(
        py,
        src,
        dst,
        colors,
        white,
        color_threshold,
        pixel_distance,
        add_alpha,
        add_mask,
        algorithm,
        true,
        format,
        compress,
        tile_size,
        bigtiff,
    )
}

/// The tables of an enhancement: equalization, a LUT file or a linear source range.
fn luts(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    ds: &terrio::RasterDataset,
    equalize: bool,
    config: Option<&str>,
    src_scale: Option<(f64, f64)>,
    bins: usize,
) -> PyResult<Vec<BandLut>> {
    use terrio::ops::enhance as e;
    let given = usize::from(equalize) + usize::from(config.is_some()) + usize::from(src_scale.is_some());
    if given != 1 {
        return Err(invalid("give one of equalize=True, config or src_scale"));
    }
    if equalize {
        return match raster_path(src)? {
            Some(path) => py.detach(|| e::equalization_luts_from_path(&path, bins)).py(),
            None => py.detach(|| e::equalization_luts(ds, bins)).py(),
        };
    }
    if let Some(config) = config {
        let text = std::fs::read_to_string(config).map_err(|err| invalid(format!("{config}: {err}")))?;
        return e::parse_luts(&text, ds.band_count(), bins).py();
    }
    let (lo, hi) = src_scale.expect("a source range");
    e::linear_luts(ds.band_count(), lo, hi).py()
}

/// Enhances the contrast of a raster with lookup tables, like `terrio enhance` (GDAL's
/// `gdalenhance`).
///
/// Args:
///     src: The raster: a path or a `Raster`.
///     dst: Also write the result to this path; the returned Raster and intermediate
///         output are still held in memory. Use the CLI for strip-based file processing.
///     equalize: Compute histogram-equalization tables (`gdalenhance -equalize`).
///     config: Read the tables from this LUT file (`gdalenhance -config`).
///     src_scale: Stretch this `(min, max)` source range linearly over the bins instead.
///     lut_bins: The number of bins (256, as in GDAL).
///     ot: The output data type (`"Byte"`, as GDAL writes).
///     format: The output format's short name, for `dst`.
///     compress: GeoTIFF/COG compression, for `dst`.
///     tile_size: GeoTIFF/COG tile size, for `dst`.
///     bigtiff: Write a BigTIFF, for `dst`.
///
/// Returns:
///     Raster: The enhanced raster, in memory.
#[pyfunction]
#[pyo3(signature = (
    src, dst=None, *, equalize=false, config=None, src_scale=None, lut_bins=256, ot="Byte", format=None, compress=None,
    tile_size=None, bigtiff=false
))]
#[allow(clippy::too_many_arguments)]
fn enhance(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    equalize: bool,
    config: Option<&str>,
    src_scale: Option<(f64, f64)>,
    lut_bins: usize,
    ot: &str,
    format: Option<&str>,
    compress: Option<String>,
    tile_size: Option<u32>,
    bigtiff: bool,
) -> PyResult<PyRaster> {
    let ds = input_raster(py, src)?;
    let tables = luts(py, src, &ds, equalize, config, src_scale, lut_bins)?;
    let o = EnhanceOptions { bins: lut_bins, output_type: terrio::ops::parse_data_type(ot).py()? };
    let out = py.detach(|| terrio::ops::enhance::enhance(&ds, &tables, &o)).py()?;
    finish_raster(py, out, dst, format, compress, tile_size, bigtiff)
}

/// Computes histogram-equalization tables, as `gdalenhance -equalize -config FILE` writes them.
///
/// Args:
///     src: The raster: a path or a `Raster`.
///     config: Also write the tables to this LUT file.
///     lut_bins: The number of bins (256, as in GDAL).
///
/// Returns:
///     str: The LUT file's text: a line per band, `1:Band min:ScaleMin max:ScaleMax` and the
///     table.
#[pyfunction]
#[pyo3(signature = (src, *, config=None, lut_bins=256))]
fn enhance_luts(py: Python<'_>, src: &Bound<'_, PyAny>, config: Option<&str>, lut_bins: usize) -> PyResult<String> {
    let tables = match raster_path(src)? {
        Some(path) => py.detach(|| terrio::ops::enhance::equalization_luts_from_path(&path, lut_bins)).py()?,
        None => {
            let ds = input_raster(py, src)?;
            py.detach(|| terrio::ops::enhance::equalization_luts(&ds, lut_bins)).py()?
        }
    };
    let text = terrio::ops::enhance::format_luts(&tables);
    if let Some(path) = config {
        std::fs::write(path, &text).map_err(|err| invalid(format!("{path}: {err}")))?;
    }
    Ok(text)
}

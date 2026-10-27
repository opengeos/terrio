//! Raster queries: `compare` (GDAL's `gdal raster compare`), `pixel_info` (`gdal raster
//! pixel-info`, `gdallocationinfo`) and `xyz` (`gdal2xyz.py`).

use crate::raster::PyRaster;
use crate::{PyResultExt, invalid};
use pyo3::prelude::*;
use terrio::LazyRaster;
use terrio::ops::compare::CompareOptions;
use terrio::ops::pixel_info::{IncludeFields, PixelInfoOptions, Position, PositionCrs};
use terrio::ops::xyz::XyzOptions;

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(compare, m)?)?;
    m.add_function(wrap_pyfunction!(pixel_info, m)?)?;
    m.add_function(wrap_pyfunction!(xyz, m)?)?;
    Ok(())
}

/// A JSON value as a Python object.
fn json_to_python<'py>(py: Python<'py>, v: &serde_json::Value) -> PyResult<Bound<'py, PyAny>> {
    let text = serde_json::to_string(v).map_err(|e| invalid(e.to_string()))?;
    py.import("json")?.call_method1("loads", (text,))
}

/// Runs `f` on the raster given as a path (opened lazily) or a `Raster`.
fn with_raster<T: Send>(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    f: impl FnOnce(&mut LazyRaster) -> terrio::Result<T> + Send,
) -> PyResult<T> {
    match crate::ops::raster_path(src)? {
        Some(path) => crate::with_warnings(py, || f(&mut terrio::open_raster_lazy(&path)?)),
        None => {
            let mut r = src.cast::<PyRaster>().map_err(|_| invalid("expected a path or a terrio.Raster"))?.borrow_mut();
            f(&mut r.inner).py()
        }
    }
}

/// Compares two rasters, as `terrio compare` (GDAL's `gdal raster compare`): files byte for byte,
/// CRS, geotransform, sizes, band descriptions, data types, nodata, color interpretations,
/// pixels, overviews, masks, metadata and subdatasets.
///
/// Args:
///     reference: The reference raster: a path or a `Raster`.
///     input: The raster compared with it: a path or a `Raster`. The binary, RPC and subdataset
///         comparisons need two paths.
///     skip_all_optional: Skip every optional comparison.
///     skip_binary: Skip the byte-by-byte comparison of the files.
///     skip_crs: Skip the CRS comparison.
///     skip_geotransform: Skip the geotransform comparison.
///     skip_overview: Skip the overview comparison.
///     skip_metadata: Skip the metadata comparison.
///     skip_rpc: Skip the RPC metadata comparison.
///     skip_geolocation: Skip the GEOLOCATION metadata comparison.
///     skip_subdataset: Skip the subdataset comparison.
///
/// Returns:
///     dict: `difference_count` (GDAL's exit status), `differences` (`kind`, `message`, and
///     `band` or `subdataset` when they apply) and `bands` (per band, overview and mask:
///     `pixels`, `pixels_differing`, `max_abs_diff` and the `data_type` compared in).
#[pyfunction]
#[pyo3(signature = (reference, input, *, skip_all_optional=false, skip_binary=false, skip_crs=false, skip_geotransform=false,
    skip_overview=false, skip_metadata=false, skip_rpc=false, skip_geolocation=false, skip_subdataset=false))]
#[allow(clippy::too_many_arguments)]
fn compare<'py>(
    py: Python<'py>,
    reference: &Bound<'py, PyAny>,
    input: &Bound<'py, PyAny>,
    skip_all_optional: bool,
    skip_binary: bool,
    skip_crs: bool,
    skip_geotransform: bool,
    skip_overview: bool,
    skip_metadata: bool,
    skip_rpc: bool,
    skip_geolocation: bool,
    skip_subdataset: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let options = if skip_all_optional {
        CompareOptions::skip_all_optional()
    } else {
        CompareOptions {
            skip_binary,
            skip_crs,
            skip_geotransform,
            skip_overview,
            skip_metadata,
            skip_rpc,
            skip_geolocation,
            skip_subdataset,
        }
    };
    let comparison = match (crate::ops::raster_path(reference)?, crate::ops::raster_path(input)?) {
        (Some(a), Some(b)) => crate::with_warnings(py, || terrio::ops::compare::compare(&a, &b, &options))?,
        (None, None) if reference.is(input) => {
            let mut a = open_or_borrow(py, reference)?;
            let mut copy = a.raster().shared();
            terrio::ops::compare::compare_rasters(a.raster(), &mut copy, &options).py()?
        }
        _ => {
            let mut a = open_or_borrow(py, reference)?;
            let mut b = open_or_borrow(py, input)?;
            terrio::ops::compare::compare_rasters(a.raster(), b.raster(), &options).py()?
        }
    };
    json_to_python(py, &comparison.to_json())
}

/// A raster opened from a path, or a `Raster` borrowed.
enum Held<'py> {
    Opened(LazyRaster),
    Borrowed(PyRefMut<'py, PyRaster>),
}

impl Held<'_> {
    fn raster(&mut self) -> &mut LazyRaster {
        match self {
            Held::Opened(r) => r,
            Held::Borrowed(r) => &mut r.inner,
        }
    }
}

fn open_or_borrow<'py>(py: Python<'py>, src: &Bound<'py, PyAny>) -> PyResult<Held<'py>> {
    match crate::ops::raster_path(src)? {
        Some(path) => Ok(Held::Opened(crate::with_warnings(py, || terrio::open_raster_lazy(&path))?)),
        None => Ok(Held::Borrowed(src.cast::<PyRaster>().map_err(|_| invalid("expected a path or a terrio.Raster"))?.borrow_mut())),
    }
}

/// Queries pixel values at positions, as `terrio pixel-info` (GDAL's `gdal raster pixel-info`
/// and `gdallocationinfo`). Only the blocks around the positions are read.
///
/// Args:
///     src: The raster: a path or a `Raster`.
///     positions: `(x, y)` or `(x, y, extra_text)` tuples: column and line, or coordinates in
///         `position_crs`.
///     position_crs: `"pixel"` (the default), `"dataset"` (the raster's CRS) or a CRS, X being
///         the longitude or easting. For `position_dataset`, the layer's CRS by default.
///     position_dataset: Query at the points (or centroids) of a vector dataset instead: a path,
///         `Layer` or `VectorDataset`.
///     layer: The layer of `position_dataset`, when it has several.
///     include_fields: Fields of `position_dataset` to copy: names, or `"ALL"` (the default) or
///         `"NONE"`.
///     bands: Bands to query, from 1 (all by default).
///     overview: Query this overview (from 0); positions stay full-resolution.
///     resampling: `"nearest"` (the default), `"bilinear"`, `"cubic"` or `"cubicspline"`.
///     promote_pixel_value_to_z: Put the value as the Z of GeoJSON points (one band).
///     format: `"geojson"` (a dict, the default), `"json"` (a list of dicts with `input`,
///         `column`, `line`, `xy` and `bands`), `"csv"`, `"text"` (gdallocationinfo's report) or
///         `"value"` (gdallocationinfo -valonly) as strings.
///
/// Returns:
///     dict | list | str: The results in `format`. With dst, also writes a vector
///         layer (output_format selects its driver); field_sep separates value-format bands.
#[pyfunction]
#[pyo3(signature = (src, positions=None, *, position_crs=None, position_dataset=None, layer=None, include_fields=None,
    bands=None, overview=None, resampling="nearest", promote_pixel_value_to_z=false, format="geojson", field_sep="\n", dst=None, output_format=None))]
#[allow(clippy::too_many_arguments)]
fn pixel_info<'py>(
    py: Python<'py>,
    src: &Bound<'py, PyAny>,
    positions: Option<Vec<Bound<'py, PyAny>>>,
    position_crs: Option<&str>,
    position_dataset: Option<&Bound<'py, PyAny>>,
    layer: Option<String>,
    include_fields: Option<Vec<String>>,
    bands: Option<Vec<usize>>,
    overview: Option<usize>,
    resampling: &str,
    promote_pixel_value_to_z: bool,
    format: &str,
    field_sep: &str,
    dst: Option<&str>,
    output_format: Option<&str>,
) -> PyResult<Bound<'py, PyAny>> {
    let format = format.to_ascii_lowercase();
    if !matches!(format.as_str(), "geojson" | "json" | "csv" | "text" | "value" | "valonly") {
        return Err(invalid(format!("unknown format '{format}': use geojson, json, csv, text or value")));
    }
    if dst.is_none() && output_format.is_some() {
        return Err(invalid("output_format requires dst"));
    }
    let output_driver = output_format
        .map(|name| terrio::formats::VectorDriver::from_name(name).ok_or_else(|| invalid(format!("unknown vector format '{name}'"))))
        .transpose()?;
    use terrio::ops::Resampling;
    let resampling = match Resampling::parse(resampling).py()? {
        r @ (Resampling::Nearest | Resampling::Bilinear | Resampling::Cubic | Resampling::CubicSpline) => r,
        _ => return Err(invalid("resampling must be nearest, bilinear, cubic or cubicspline")),
    };
    let options = PixelInfoOptions {
        bands: bands.unwrap_or_default(),
        overview,
        position_crs: position_crs.map(PositionCrs::parse).transpose().py()?,
        resampling,
        promote_pixel_value_to_z,
        include_fields: IncludeFields::parse(&include_fields.unwrap_or_default()),
    };
    if position_dataset.is_some() && positions.is_some() {
        return Err(invalid("give positions or a position_dataset, not both"));
    }
    let info = if let Some(ds) = position_dataset {
        let layers = crate::ops::input_layers(py, ds, layer.clone().map(|l| vec![l]))?;
        let points = match (layers.len(), &layer) {
            (0, _) => return Err(invalid("the position dataset has no vector layer")),
            (1, _) | (_, Some(_)) => layers.into_iter().next().expect("one layer"),
            _ => return Err(invalid("the position dataset has more than one layer: choose one with layer")),
        };
        with_raster(py, src, |r| terrio::ops::pixel_info::pixel_info_layer(r, &points, &options))?
    } else {
        let mut list = Vec::new();
        for p in positions.unwrap_or_default() {
            let (x, y, extra) = match p.extract::<(f64, f64, String)>() {
                Ok((x, y, e)) => (x, y, Some(e)),
                Err(_) => {
                    let (x, y) = p.extract::<(f64, f64)>().map_err(|_| invalid("positions are (x, y) or (x, y, text) tuples"))?;
                    (x, y, None)
                }
            };
            list.push(Position { x, y, extra });
        }
        if list.is_empty() {
            return Err(invalid("give positions or a position_dataset"));
        }
        with_raster(py, src, |r| terrio::ops::pixel_info::pixel_info(r, &list, &options))?
    };
    if let Some(dst) = dst {
        let output = std::path::Path::new(dst);
        let aliases = |path: &std::path::Path| {
            path == output || std::fs::canonicalize(path).ok().zip(std::fs::canonicalize(output).ok()).is_some_and(|(a, b)| a == b)
        };
        if crate::ops::raster_path(src)?.is_some_and(|path| aliases(std::path::Path::new(&path)))
            || position_dataset.and_then(|p| p.extract::<std::path::PathBuf>().ok()).is_some_and(|p| aliases(&p))
        {
            return Err(invalid("the output would overwrite an input"));
        }
        let driver = output_driver;
        let layer_name = output.file_stem().and_then(|s| s.to_str()).unwrap_or("pixel_info");
        let layer = info.to_layer(layer_name).py()?;
        crate::with_warnings(py, || terrio::write_vector(&[layer], dst, &terrio::VectorWriteOptions { driver, ..Default::default() }))?;
    }
    match format.as_str() {
        "geojson" => py.import("json")?.call_method1("loads", (info.to_geojson_string(),)),
        "json" => json_to_python(py, &info.to_json()),
        "csv" => Ok(info.to_csv().into_pyobject(py)?.into_any()),
        "text" => Ok(info.to_text().into_pyobject(py)?.into_any()),
        "value" | "valonly" => Ok(info.to_values(field_sep).into_pyobject(py)?.into_any()),
        other => Err(invalid(format!("unknown format '{other}': use geojson, json, csv, text or value"))),
    }
}

/// Writes raster pixels as XYZ text, as `terrio xyz` (GDAL's `gdal2xyz.py`): a line per pixel with
/// the X and Y of its center and the values of the bands, read a strip at a time.
///
/// Args:
///     src: The raster: a path or a `Raster`.
///     dst: The output text file. When omitted, the text is returned.
///     bands: Bands to write, from 1 (band 1 by default).
///     all_bands: Write all bands.
///     skip: Write every Nth column and row: `N`, or `(columns, rows)`.
///     srcwin: Only this window, `(x_offset, y_offset, width, height)` in pixels.
///     skip_nodata: Leave out pixels whose values all equal the source nodata.
///     src_nodata: The source nodata: a number, or one per band. By default, the bands'.
///     dst_nodata: Values written for nodata pixels: a number, or one per band.
///     delimiter: The separator, `" "` by default (`","` for CSV).
///
/// Returns:
///     int | str: The number of lines written to `dst`, or the text.
#[pyfunction]
#[pyo3(signature = (src, dst=None, *, bands=None, all_bands=false, skip=None, srcwin=None, skip_nodata=false,
    src_nodata=None, dst_nodata=None, delimiter=" "))]
#[allow(clippy::too_many_arguments)]
fn xyz<'py>(
    py: Python<'py>,
    src: &Bound<'py, PyAny>,
    dst: Option<std::path::PathBuf>,
    bands: Option<Vec<usize>>,
    all_bands: bool,
    skip: Option<&Bound<'py, PyAny>>,
    srcwin: Option<(usize, usize, usize, usize)>,
    skip_nodata: bool,
    src_nodata: Option<&Bound<'py, PyAny>>,
    dst_nodata: Option<&Bound<'py, PyAny>>,
    delimiter: &str,
) -> PyResult<Bound<'py, PyAny>> {
    let numbers = |v: Option<&Bound<'py, PyAny>>| -> PyResult<Option<Vec<f64>>> {
        match v {
            None => Ok(None),
            Some(v) => match v.extract::<f64>() {
                Ok(x) => Ok(Some(vec![x])),
                Err(_) => Ok(Some(v.extract::<Vec<f64>>().map_err(|_| invalid("nodata values are a number or a list of numbers"))?)),
            },
        }
    };
    let skip = match skip {
        None => (1, 1),
        Some(s) => match s.extract::<usize>() {
            Ok(n) => (n, n),
            Err(_) => s.extract::<(usize, usize)>().map_err(|_| invalid("skip is a number or (columns, rows)"))?,
        },
    };
    let options = XyzOptions {
        bands: if all_bands { Vec::new() } else { bands.filter(|b| !b.is_empty()).unwrap_or_else(|| vec![1]) },
        skip,
        srcwin: srcwin.map(|(x, y, w, h)| [x, y, w, h]),
        skip_nodata,
        src_nodata: numbers(src_nodata)?,
        dst_nodata: numbers(dst_nodata)?,
        delimiter: delimiter.to_string(),
    };
    match dst {
        Some(path) => {
            if crate::ops::raster_path(src)?.is_some_and(|input| {
                std::path::Path::new(&input) == path.as_path()
                    || std::fs::canonicalize(input).ok().zip(std::fs::canonicalize(&path).ok()).is_some_and(|(a, b)| a == b)
            }) {
                return Err(invalid("the output would overwrite the input"));
            }
            let lines = with_raster(py, src, |r| terrio::ops::xyz::write_xyz_file(r, &path, &options))?;
            Ok(lines.into_pyobject(py)?.into_any())
        }
        None => {
            let text = with_raster(py, src, |r| {
                let mut out = Vec::new();
                terrio::ops::xyz::write_xyz(r, &mut out, &options)?;
                Ok(String::from_utf8_lossy(&out).into_owned())
            })?;
            Ok(text.into_pyobject(py)?.into_any())
        }
    }
}

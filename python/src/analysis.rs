//! Thin bindings for raster-to-vector analysis and existing topology operations.
use crate::ops::input_raster;
use crate::vector::PyLayer;
use crate::{Detach, PyResultExt};
use pyo3::prelude::*;
use std::collections::BTreeMap;
use terrio::ops::{contour as lines, footprint as area, topology};

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(contour, m)?)?;
    m.add_function(wrap_pyfunction!(footprint, m)?)?;
    m.add_function(wrap_pyfunction!(buffer, m)?)?;
    m.add_function(wrap_pyfunction!(dissolve, m)?)?;
    m.add_function(wrap_pyfunction!(check_geometry, m)?)?;
    m.add_function(wrap_pyfunction!(make_valid, m)?)?;
    m.add_function(wrap_pyfunction!(zonal_stats, m)?)?;
    m.add_function(wrap_pyfunction!(singlepart, m)?)?;
    Ok(())
}

/// Extract contour lines from a Raster or path. The input raster is materialized.
#[pyfunction]
#[pyo3(signature=(src, *, band=1, interval=None, offset=0., levels=None, nodata=None, ignore_nodata=false, with_z=false, field_name="ELEV", layer_name="contour"))]
#[allow(clippy::too_many_arguments)]
fn contour(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    band: usize,
    interval: Option<f64>,
    offset: f64,
    levels: Option<Vec<f64>>,
    nodata: Option<f64>,
    ignore_nodata: bool,
    with_z: bool,
    field_name: &str,
    layer_name: &str,
) -> PyResult<PyLayer> {
    let input = input_raster(py, src)?;
    let options = lines::ContourOptions {
        band,
        interval,
        offset,
        levels: levels.unwrap_or_default(),
        nodata,
        ignore_nodata,
        with_z,
        field_name: field_name.into(),
        layer_name: layer_name.into(),
    };
    let inner = py.detach_result(|| lines::contour(&input, &options)).py()?;
    Ok(PyLayer { inner })
}

/// Exact valid-data footprint of a Raster/path, optionally restricted by a mask.
/// Validity uses finite samples and dataset nodata; alpha/external masks require an explicit mask.
/// Attributes are explicit text fields; no source-path attribute is added automatically.
#[pyfunction]
#[pyo3(signature=(src, *, bands=None, intersection=false, mask=None, singlepart=false, attributes=None))]
fn footprint(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    bands: Option<Vec<usize>>,
    intersection: bool,
    mask: Option<&Bound<'_, PyAny>>,
    singlepart: bool,
    attributes: Option<BTreeMap<String, String>>,
) -> PyResult<PyLayer> {
    let input = input_raster(py, src)?;
    let options = area::FootprintOptions {
        bands: bands.unwrap_or_default(),
        intersection,
        mask: mask.map(|value| input_raster(py, value)).transpose()?,
        singlepart,
        attributes: attributes.unwrap_or_default().into_iter().collect(),
    };
    let inner = py.detach_result(|| area::footprint(&input, &options)).py()?;
    Ok(PyLayer { inner })
}

fn vector_op(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    layer: Option<String>,
    apply: impl FnOnce(&terrio::Layer) -> terrio::Result<terrio::Layer> + Send,
) -> PyResult<PyLayer> {
    let input = crate::vector_geom::one_layer(py, src, layer)?;
    let inner = py.detach_result(|| apply(&input)).py()?;
    Ok(PyLayer { inner })
}

/// Planar buffer in CRS units, preserving attributes and feature IDs.
#[pyfunction]
#[pyo3(signature=(src, distance, *, quadrant_segments=30, layer=None))]
fn buffer(py: Python<'_>, src: &Bound<'_, PyAny>, distance: f64, quadrant_segments: usize, layer: Option<String>) -> PyResult<PyLayer> {
    vector_op(py, src, layer, |input| topology::buffer(input, &topology::BufferOptions { distance, quadrant_segments }))
}

/// Dissolve an entire layer, or groups selected by attribute field names.
#[pyfunction]
#[pyo3(signature=(src, *, fields=None, layer=None))]
fn dissolve(py: Python<'_>, src: &Bound<'_, PyAny>, fields: Option<Vec<String>>, layer: Option<String>) -> PyResult<PyLayer> {
    vector_op(py, src, layer, |input| topology::dissolve(input, &fields.unwrap_or_default()))
}

/// Return a layer with is_valid and valid_reason fields; source geometries are retained.
#[pyfunction]
#[pyo3(signature=(src, *, layer=None))]
fn check_geometry(py: Python<'_>, src: &Bound<'_, PyAny>, layer: Option<String>) -> PyResult<PyLayer> {
    vector_op(py, src, layer, |input| Ok(topology::validity(input)))
}

/// Repair invalid geometry, preserving attributes, IDs and already-valid Z/M values.
#[pyfunction]
#[pyo3(signature=(src, *, layer=None))]
fn make_valid(py: Python<'_>, src: &Bound<'_, PyAny>, layer: Option<String>) -> PyResult<PyLayer> {
    vector_op(py, src, layer, topology::make_valid)
}

/// Summarize raster bands for polygon zones. All input raster pixels are materialized.
/// Prefix defaults are raster1, raster2, etc.; fields are PREFIX_bN_STATISTIC.
#[pyfunction]
#[pyo3(signature=(zones, rasters, *, bands=None, prefixes=None, stats=None, all_touched=false, t_srs=None, decimal_places=None, layer=None))]
#[allow(clippy::too_many_arguments)]
fn zonal_stats(
    py: Python<'_>,
    zones: &Bound<'_, PyAny>,
    rasters: Vec<Bound<'_, PyAny>>,
    bands: Option<Vec<usize>>,
    prefixes: Option<Vec<String>>,
    stats: Option<Vec<String>>,
    all_touched: bool,
    t_srs: Option<String>,
    decimal_places: Option<u8>,
    layer: Option<String>,
) -> PyResult<PyLayer> {
    use terrio::ops::zonal::{Statistic, ZonalInput, ZonalOptions};
    let zones = crate::vector_geom::one_layer(py, zones, layer)?;
    let rasters = rasters.iter().map(|src| input_raster(py, src)).collect::<PyResult<Vec<_>>>()?;
    let prefixes = prefixes.unwrap_or_else(|| (1..=rasters.len()).map(|i| format!("raster{i}")).collect());
    if prefixes.iter().any(|p| p.trim().is_empty()) {
        return Err(pyo3::exceptions::PyValueError::new_err("raster field prefixes cannot be empty"));
    }
    if prefixes.len() != rasters.len() {
        return Err(pyo3::exceptions::PyValueError::new_err("give one prefix per raster, or omit prefixes"));
    }
    let bands = bands.unwrap_or_else(|| vec![1]);
    let options = ZonalOptions {
        stats: stats
            .map(|names| names.iter().map(|s| Statistic::parse(s)).collect::<terrio::Result<Vec<_>>>())
            .transpose()
            .py()?
            .unwrap_or_else(|| ZonalOptions::default().stats),
        all_touched,
        dst_srs: t_srs.as_deref().map(terrio::crs::SpatialRef::parse).transpose().py()?,
        decimal_places,
    };
    let inner = py
        .detach_result(|| {
            let mut inputs = Vec::new();
            for (raster, prefix) in rasters.iter().zip(&prefixes) {
                for &band in &bands {
                    inputs.push(ZonalInput { prefix: format!("{prefix}_b{band}"), raster, band });
                }
            }
            terrio::ops::zonal::zonal_stats(&zones, &inputs, &options)
        })
        .py()?;
    Ok(PyLayer { inner })
}

/// Recursively split multipart geometries/collections; IDs restart at one.
#[pyfunction]
#[pyo3(signature=(src, *, layer=None))]
fn singlepart(py: Python<'_>, src: &Bound<'_, PyAny>, layer: Option<String>) -> PyResult<PyLayer> {
    vector_op(py, src, layer, |layer| Ok(terrio::ops::geometry::singleparts(layer)))
}

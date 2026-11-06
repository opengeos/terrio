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
    Ok(PyLayer::from(inner))
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
        mask: mask.map(|value| input_raster(py, value).map(std::sync::Arc::unwrap_or_clone)).transpose()?,
        singlepart,
        attributes: attributes.unwrap_or_default().into_iter().collect(),
    };
    let inner = py.detach_result(|| area::footprint(&input, &options)).py()?;
    Ok(PyLayer::from(inner))
}

fn vector_op(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    layer: Option<String>,
    apply: impl FnOnce(&terrio::Layer) -> terrio::Result<terrio::Layer> + Send,
) -> PyResult<PyLayer> {
    let input = crate::vector_geom::one_layer(py, src, layer)?;
    let inner = py.detach_result(|| apply(&input)).py()?;
    Ok(PyLayer::from(inner))
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

/// Summarize raster bands for polygon zones; rasters are read lazily, by block.
/// Prefix defaults are raster1, raster2, etc.; fields are PREFIX_bN_STATISTIC.
#[pyfunction]
#[pyo3(signature=(zones, rasters, *, bands=None, prefixes=None, stats=None, all_touched=false, t_srs=None, decimal_places=None, layer=None, fractional=false, weights=None, weight_band=1))]
#[allow(clippy::too_many_arguments)]
fn zonal_stats(
    py: Python<'_>,
    zones: &Bound<'_, PyAny>,
    rasters: &Bound<'_, PyAny>,
    bands: Option<Vec<usize>>,
    prefixes: Option<Vec<String>>,
    stats: Option<Vec<String>>,
    all_touched: bool,
    t_srs: Option<String>,
    decimal_places: Option<u8>,
    layer: Option<String>,
    fractional: bool,
    weights: Option<&Bound<'_, PyAny>>,
    weight_band: usize,
) -> PyResult<PyLayer> {
    use terrio::ops::zonal::{Statistic, ZonalBand, ZonalOptions, ZonalSource, ZonalWeight};
    if prefixes.as_ref().is_some_and(|p| p.iter().any(|p| p.trim().is_empty())) {
        return Err(pyo3::exceptions::PyValueError::new_err("raster field prefixes cannot be empty"));
    }
    let stats = stats
        .map(|names| names.iter().map(|s| Statistic::parse(s)).collect::<terrio::Result<Vec<_>>>())
        .transpose()
        .py()?
        .unwrap_or_else(|| ZonalOptions::default().stats);
    let weights =
        weights.filter(|_| stats.iter().any(|s| s.uses_weights())).map(|src| crate::ops::input_lazy_raster(py, src)).transpose()?;
    let options = ZonalOptions {
        fractional,
        weights: weights.map(|raster| ZonalWeight::new(raster, weight_band)),
        stats,
        all_touched,
        dst_srs: t_srs.as_deref().map(terrio::crs::SpatialRef::parse).transpose().py()?,
        decimal_places,
    };
    let zones = crate::vector_geom::one_layer(py, zones, layer)?;
    let bands = bands.unwrap_or_else(|| vec![1]);
    // A DataFrame of labelled rows becomes its records; a dict is one labelled row.
    let items = if rasters.hasattr("to_dict")? && !rasters.is_instance_of::<pyo3::types::PyDict>() {
        rasters.call_method1("to_dict", ("records",))?.extract::<Vec<Bound<'_, PyAny>>>()?
    } else if rasters.is_instance_of::<pyo3::types::PyDict>() {
        vec![rasters.clone()]
    } else {
        crate::ops::raster_items(rasters)?
    };
    let labelled = items.iter().any(|i| i.is_instance_of::<pyo3::types::PyDict>());
    if labelled && prefixes.is_some() {
        return Err(pyo3::exceptions::PyValueError::new_err(
            "prefixes cannot be combined with labelled inputs (dicts): their fields are named TIME_LABEL",
        ));
    }
    let prefixes = prefixes.unwrap_or_else(|| (1..=items.len()).map(|i| format!("raster{i}")).collect());
    if prefixes.len() != items.len() {
        return Err(pyo3::exceptions::PyValueError::new_err("give one prefix per raster, or omit prefixes"));
    }
    // Sources in first-appearance order; labelled rows naming the raster (its path, or the same
    // terrio.Raster) of an earlier item or row share its source, as in the CLI.
    #[derive(PartialEq, Eq, Hash)]
    enum Key {
        Path(String),
        Object(usize),
    }
    let key = |raster: &Bound<'_, PyAny>| -> PyResult<Key> {
        Ok(crate::ops::raster_path(raster)?.map_or(Key::Object(raster.as_ptr() as usize), Key::Path))
    };
    let mut sources: Vec<ZonalSource> = Vec::new();
    let mut seen: std::collections::HashMap<Key, usize> = std::collections::HashMap::new();
    for (item, prefix) in items.iter().zip(&prefixes) {
        let Ok(row) = item.cast::<pyo3::types::PyDict>() else {
            seen.entry(key(item)?).or_insert(sources.len());
            sources.push(ZonalSource {
                raster: zonal_raster(py, item)?,
                bands: bands.iter().map(|&band| ZonalBand::new(band, format!("{prefix}_b{band}"))).collect(),
            });
            continue;
        };
        // None, NaN, NaT (which differ from themselves) and pandas' NA, a DataFrame's missing
        // values, are absent.
        let missing = |v: &Bound<'_, PyAny>| v.is_none() || v.ne(v).unwrap_or(false) || v.get_type().name().is_ok_and(|n| n == "NAType");
        let get = |key: &str| -> PyResult<Option<Bound<'_, PyAny>>> { Ok(row.get_item(key)?.filter(|v| !missing(v))) };
        for key in row.keys() {
            let key: String = key.extract()?;
            if !matches!(key.as_str(), "raster" | "time" | "label" | "band") {
                return Err(pyo3::exceptions::PyValueError::new_err(format!(
                    "unknown input key '{key}' (expected raster, time, label, band)"
                )));
            }
        }
        let raster = get("raster")?
            .ok_or_else(|| pyo3::exceptions::PyValueError::new_err("each input dict needs a 'raster' (a path or a terrio.Raster)"))?;
        // A date or datetime (a DataFrame's Timestamp) in ISO 8601, its date alone at midnight
        // (naive or UTC, as JavaScript names a Date at midnight UTC).
        let text = |v: Bound<'_, PyAny>| -> PyResult<String> {
            if v.hasattr("isoformat")? {
                let iso: String = v.call_method0("isoformat")?.extract()?;
                return Ok(terrio::ops::zonal::time_text(&iso));
            }
            Ok(v.str()?.to_string())
        };
        let time = get("time")?.map(text).transpose()?;
        let label = get("label")?.map(text).transpose()?;
        let row_bands = match get("band")? {
            // A DataFrame's band column is float when some row has none: 2.0 is band 2.
            Some(b) => vec![
                b.extract::<usize>()
                    .or_else(|e| b.extract::<f64>().ok().filter(|f| f.fract() == 0.0 && *f >= 1.0).map(|f| f as usize).ok_or(e))?,
            ],
            None => bands.clone(),
        };
        let row_bands =
            terrio::ops::zonal::labelled_bands(&row_bands, time.as_deref(), label.as_deref(), |band| format!("{prefix}_b{band}"));
        match seen.get(&key(&raster)?) {
            Some(&i) => sources[i].bands.extend(row_bands),
            None => {
                seen.insert(key(&raster)?, sources.len());
                sources.push(ZonalSource { raster: zonal_raster(py, &raster)?, bands: row_bands });
            }
        }
    }
    let inner = crate::with_warnings(py, || terrio::ops::zonal::zonal_stats(&zones, &mut sources, &options))?;
    Ok(PyLayer::from(inner))
}

/// A zonal raster: a path, opened only while it is read (see `ZonalRaster::Path`), or a `Raster`.
fn zonal_raster(py: Python<'_>, src: &Bound<'_, PyAny>) -> PyResult<terrio::ops::zonal::ZonalRaster> {
    Ok(match crate::ops::raster_path(src)? {
        Some(path) => terrio::ops::zonal::ZonalRaster::Path(path),
        None => crate::ops::input_lazy_raster(py, src)?.into(),
    })
}

/// Recursively split multipart geometries/collections; IDs restart at one.
#[pyfunction]
#[pyo3(signature=(src, *, layer=None))]
fn singlepart(py: Python<'_>, src: &Bound<'_, PyAny>, layer: Option<String>) -> PyResult<PyLayer> {
    vector_op(py, src, layer, |layer| Ok(terrio::ops::geometry::singleparts(layer)))
}

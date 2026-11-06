//! Thin bindings for raster-to-vector analysis and existing topology operations.
use crate::ops::input_raster;
use crate::vector::PyLayer;
use crate::{Detach, PyResultExt};
use numpy::PyUntypedArrayMethods;
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
    m.add_class::<PyZonalReducer>()?;
    m.add_class::<PyZonalStream>()?;
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
        mask: mask.map(|value| input_raster(py, value)).transpose()?,
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

/// Zonal statistics of arrays you produce strip by strip, equal to `zonal_stats` over a raster
/// holding the same values: the zones are computed once for the grid, then each `stream()`
/// reduces one band, fed the `windows()` in order.
///
/// Args:
///     zones: Anything `zonal_stats` accepts (a `Layer`, a path, GeoJSON...).
///     width: The width of the grid, in pixels.
///     height: The height of the grid, in pixels.
///     transform: The grid's geotransform, in GDAL's order.
///     crs: The grid's CRS; may be omitted only if the zones have none.
///     stats: Statistics (default: count, min, max, mean, sum, stddev); weighted ones are not
///         supported (use `zonal_stats`).
///     all_touched: Include every pixel touched by a zone, not only those with centres inside.
///     fractional: Weight pixels by the fraction of them a zone covers.
///     layer: The layer of `zones` to use.
///     strip_rows: Rows per strip of `windows()` (default: terrio's strip size for one float64
///         band of this width). Give a tiled source's block height to read each block once.
#[pyclass(module = "terrio", name = "ZonalReducer", frozen)]
pub struct PyZonalReducer {
    inner: terrio::ops::zonal::ZonalReducer,
    /// Each statistic's canonical name, and whether it is an integer column.
    stats: Vec<(String, bool)>,
}

#[pymethods]
impl PyZonalReducer {
    #[new]
    #[pyo3(signature = (zones, width, height, transform, crs=None, *, stats=None, all_touched=false, fractional=false, layer=None, strip_rows=None))]
    #[allow(clippy::too_many_arguments)]
    fn new(
        py: Python<'_>,
        zones: &Bound<'_, PyAny>,
        width: usize,
        height: usize,
        transform: [f64; 6],
        crs: Option<&str>,
        stats: Option<Vec<String>>,
        all_touched: bool,
        fractional: bool,
        layer: Option<String>,
        strip_rows: Option<usize>,
    ) -> PyResult<Self> {
        use terrio::ops::rasterize::Grid;
        use terrio::ops::zonal::{Statistic, ZonalOptions, ZonalReducer};
        let parsed = match &stats {
            Some(names) => names.iter().map(|s| Statistic::parse(s)).collect::<terrio::Result<Vec<_>>>().py()?,
            None => ZonalOptions::default().stats,
        };
        // Canonical names (`stddev` for `std`), as `zonal_stats` names its fields.
        let names: Vec<String> = parsed.iter().map(|s| s.name()).collect();
        let stats = names
            .into_iter()
            .zip(&parsed)
            .map(|(name, &s)| (name, (s == Statistic::Count && !fractional) || s == Statistic::Variety))
            .collect();
        let options = ZonalOptions { fractional, stats: parsed, all_touched, ..Default::default() };
        let zones = crate::vector_geom::one_layer(py, zones, layer)?;
        let grid = Grid {
            cols: width,
            rows: height,
            geo_transform: transform,
            crs: crs.map(terrio::crs::SpatialRef::parse).transpose().py()?.unwrap_or_default(),
        };
        let inner = crate::with_warnings(py, || ZonalReducer::new(&zones, grid, &options, strip_rows))?;
        Ok(Self { inner, stats })
    }

    /// The windows to supply, in order, as `(x, y, width, height)`: strips of `strip_rows` rows
    /// holding zone pixels, cropped to the rows and columns of those pixels. Slice
    /// `array[y:y+h, x:x+w]`.
    fn windows(&self) -> Vec<(usize, usize, usize, usize)> {
        self.inner.windows().iter().map(|w| (w.x, w.y, w.width, w.height)).collect()
    }

    /// The number of zone features.
    #[getter]
    fn feature_count(&self) -> usize {
        self.inner.feature_count()
    }

    /// The statistics computed, by their canonical names (`stddev` for `std`), as fields name them.
    #[getter]
    fn stats(&self) -> Vec<String> {
        self.stats.iter().map(|(name, _)| name.clone()).collect()
    }

    /// A reduction of one band over `windows()`; pixels equal to `nodata`, or not finite, are
    /// left out.
    #[pyo3(signature = (nodata=None))]
    fn stream(&self, nodata: Option<f64>) -> PyZonalStream {
        PyZonalStream {
            reducer: self.inner.clone(),
            stream: std::sync::Mutex::new(Some(self.inner.stream(nodata))),
            stats: self.stats.clone(),
        }
    }
}

/// One band reduced by a `ZonalReducer`: `add` each window's array in order, then `finish`.
#[pyclass(module = "terrio", name = "ZonalStream", frozen)]
pub struct PyZonalStream {
    /// Shares the reducer's windows, for the shape check.
    reducer: terrio::ops::zonal::ZonalReducer,
    stream: std::sync::Mutex<Option<terrio::ops::zonal::ZonalStream>>,
    stats: Vec<(String, bool)>,
}

impl PyZonalStream {
    /// The stream, unless another thread is using it: waiting would hold this thread's GIL
    /// against the one inside `add`, which needs it back.
    fn lock(&self) -> PyResult<std::sync::MutexGuard<'_, Option<terrio::ops::zonal::ZonalStream>>> {
        self.stream.try_lock().map_err(|e| match e {
            std::sync::TryLockError::WouldBlock => crate::invalid("the stream is in use by another thread"),
            std::sync::TryLockError::Poisoned(_) => crate::invalid("the stream is poisoned"),
        })
    }
}

#[pymethods]
impl PyZonalStream {
    /// Adds window number `window` (of `ZonalReducer.windows`): a 2-D integer, float32, float64 or
    /// bool array of shape `(height, width)`, of any strides. Windows must come in order.
    fn add(&self, py: Python<'_>, window: usize, array: &Bound<'_, numpy::PyUntypedArray>) -> PyResult<()> {
        let mut guard = self.lock()?;
        let stream = guard.as_mut().ok_or_else(|| crate::invalid("the stream is finished"))?;
        // The shape of the window expected next; the stream reports a window out of order.
        if window == stream.next_window()
            && let Some(&terrio::Window { width: w, height: h, .. }) = self.reducer.windows().get(window)
        {
            let shape = array.shape();
            if shape != [h, w] {
                let got = shape.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(", ");
                return Err(crate::invalid(format!("window {window}: expected (h, w) = ({h}, {w}), got an array of shape ({got})")));
            }
        }
        let data = crate::raster::from_numpy(array)?;
        let result = py.detach_result(|| stream.add(window, &data));
        // `add` checks its arguments before adding a pixel; any other error (a panic) may leave
        // part of the window added, so the stream is dropped.
        if let Err(e) = &result
            && !matches!(e, terrio::Error::InvalidArgument(_))
        {
            *guard = None;
        }
        result.py()
    }

    /// The statistics, as `{name: array}` with one value per zone feature, in layer order: int64
    /// for `count` (unless fractional) and `variety`, float64 (NaN for null) for the others.
    fn finish<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, pyo3::types::PyDict>> {
        use numpy::IntoPyArray;
        use terrio::FieldValue;
        let stream = self.lock()?.take();
        let stream = stream.ok_or_else(|| crate::invalid("the stream is finished"))?;
        let columns = py.detach_result(|| stream.finish()).py()?;
        let out = pyo3::types::PyDict::new(py);
        for ((name, integer), column) in self.stats.iter().zip(columns) {
            if *integer {
                let values = column.iter().map(|v| if let FieldValue::Integer(i) = v { *i } else { 0 }).collect::<Vec<_>>();
                out.set_item(name, values.into_pyarray(py))?;
            } else {
                let values = column
                    .iter()
                    .map(|v| match v {
                        FieldValue::Float(x) => *x,
                        FieldValue::Integer(i) => *i as f64,
                        _ => f64::NAN,
                    })
                    .collect::<Vec<_>>();
                out.set_item(name, values.into_pyarray(py))?;
            }
        }
        Ok(out)
    }
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

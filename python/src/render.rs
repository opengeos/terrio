//! Pictures of datasets: `terrio.render` and the notebook display of rasters.
use crate::raster::PyRaster;
use crate::vector::layers_from_object;
use crate::{Detach, PyResultExt};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use terrio::ops::render::{self as draw, Colormap, RenderOptions, Stretch};

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(render, m)?)?;
    Ok(())
}

/// The longest side of a raster's picture in a notebook.
pub(crate) const NOTEBOOK_SIZE: usize = 512;

// The default is written out, because Python shows a default that is not a literal as `...`.
const _: () = assert!(draw::DEFAULT_MAX_SIZE == 1024);

/// Whether `src` is a string of GeoJSON, an object, rather than a path.
fn is_geojson_text(src: &Bound<'_, PyAny>) -> bool {
    src.extract::<String>().is_ok_and(|s| s.trim_start().starts_with('{'))
}

/// Draws a raster or vector dataset as a PNG, reading as little of a raster as the size allows.
///
/// A raster is read from its coarsest overview that still has the pixels needed, so a remote COG
/// transfers a small part of itself. Pixels are drawn in the raster's own grid; vector layers are
/// drawn north up. Nodata and masked pixels are transparent.
///
/// Args:
///     src: A raster or vector path, a Raster, or vector layers (VectorDataset, Layer, list of
///         layers, or GeoJSON as a mapping or as text).
///     width: The width in pixels. With only one of width and height, the other keeps the
///         aspect ratio.
///     height: The height in pixels.
///     max_size: The longest side when neither is given; images are not enlarged to reach it.
///     bands: One band, or three drawn as red, green and blue, numbered from 1. By default, from
///         the color interpretation.
///     colormap: The colormap of one band or a vector attribute: gray, viridis, magma, inferno,
///         plasma, terrain, rdbu or rdylgn, with _r to reverse.
///     stretch: "auto", "minmax", "pLOW,pHIGH" (percentiles) or "LOW,HIGH" (values).
///     layers: Vector: the layers to draw. By default all, which must share a CRS.
///     attribute: Vector: color features by this numeric field.
///
/// Returns:
///     bytes: The PNG.
#[pyfunction]
#[pyo3(signature = (src, *, width=None, height=None, max_size=1024, bands=None, colormap=None, stretch="auto", layers=None, attribute=None))]
#[allow(clippy::too_many_arguments)]
fn render(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    width: Option<usize>,
    height: Option<usize>,
    max_size: usize,
    bands: Option<Vec<usize>>,
    colormap: Option<&str>,
    stretch: &str,
    layers: Option<Vec<String>>,
    attribute: Option<String>,
) -> PyResult<Py<PyBytes>> {
    let options = RenderOptions {
        width,
        height,
        max_size,
        bands: bands.unwrap_or_default(),
        colormap: colormap.map(Colormap::parse).transpose().py()?,
        stretch: Stretch::parse(stretch).py()?,
        attribute,
    };
    // An empty list of layers is no filter, as in the JavaScript binding.
    let layers = layers.filter(|names| !names.is_empty());
    let vector_only = options.attribute.is_some() || layers.is_some();
    let png = if let Ok(raster) = src.cast::<PyRaster>() {
        if vector_only {
            return Err(crate::invalid("attribute and layers are for vector datasets"));
        }
        let mut raster = raster.try_borrow_mut().map_err(|e| crate::invalid(e.to_string()))?;
        let inner = &mut raster.inner;
        crate::with_warnings(py, || draw::render_raster(inner, &options)?.to_png())?
    } else if let Some(path) = src.extract::<std::path::PathBuf>().ok().filter(|_| !is_geojson_text(src)) {
        let path = path.into_os_string().into_string().map_err(|_| crate::invalid("paths must be UTF-8"))?;
        let names = layers.clone().unwrap_or_default();
        crate::with_warnings(py, || match draw::open_source(&path, &names, vector_only)? {
            draw::Source::Raster(mut raster) => draw::render_raster(&mut raster, &options)?.to_png(),
            draw::Source::Vector(layers) => draw::render_vector(layers, &options)?.to_png(),
        })?
    } else {
        let all = draw::select_layers(layers_from_object(src, None, None)?, &layers.unwrap_or_default()).py()?;
        py.detach_result(|| draw::render_vector(all, &options)?.to_png()).py()?
    };
    Ok(PyBytes::new(py, &png).unbind())
}

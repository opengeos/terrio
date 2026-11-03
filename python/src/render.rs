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

/// Draws a raster or vector dataset as a PNG, reading as little of a raster as the size allows.
///
/// A raster is read from its coarsest overview that still has the pixels needed, so a remote COG
/// transfers a small part of itself. Pixels are drawn in the raster's own grid; vector layers are
/// drawn north up. Nodata and masked pixels are transparent.
///
/// Args:
///     src: A raster or vector path, a Raster, or vector layers (VectorDataset, Layer, list of
///         layers or GeoJSON).
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
#[pyo3(signature = (src, *, width=None, height=None, max_size=draw::DEFAULT_MAX_SIZE, bands=None, colormap=None, stretch="auto", layers=None, attribute=None))]
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
    let vector_only = options.attribute.is_some() || layers.is_some();
    let png = if let Ok(raster) = src.cast::<PyRaster>() {
        let mut raster = raster.try_borrow_mut().map_err(|e| crate::invalid(e.to_string()))?;
        let inner = &mut raster.inner;
        crate::with_warnings(py, || draw::render_raster(inner, &options)?.to_png())?
    } else if let Ok(path) = src.extract::<std::path::PathBuf>() {
        let path = path.into_os_string().into_string().map_err(|_| crate::invalid("paths must be UTF-8"))?;
        crate::with_warnings(py, || {
            let raster = if vector_only { None } else { terrio::open_raster_lazy(&path).ok() };
            match raster {
                Some(mut raster) => draw::render_raster(&mut raster, &options)?.to_png(),
                None => {
                    let open = terrio::VectorOpenOptions { layers: layers.clone().unwrap_or_default(), ..Default::default() };
                    let vector = terrio::open_vector(&path, &open).map_err(|e| {
                        if vector_only {
                            e
                        } else {
                            terrio::Error::InvalidArgument(format!("{path}: neither a raster nor a vector dataset: {e}"))
                        }
                    })?;
                    draw::render_vector(&vector.into_resolved_layers(), &options)?.to_png()
                }
            }
        })?
    } else {
        let mut all = layers_from_object(src, None, None)?;
        if let Some(names) = layers {
            all.retain(|l| names.contains(&l.name));
        }
        py.detach_result(|| draw::render_vector(&all, &options)?.to_png()).py()?
    };
    Ok(PyBytes::new(py, &png).unbind())
}

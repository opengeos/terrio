//! Folder raster tiles without materializing a lazy input.
use crate::{PyResultExt, invalid};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use terrio::ops::raster_tile::{self as tile, TileOptions};
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(raster_tiles, m)?)?;
    Ok(())
}
/// Write XYZ/TMS tiles to a directory. Options match Rust TileOptions (snake_case);
/// grid={tile_size, min_zoom, max_zoom, limits={min_x,...}} controls the matrix.
/// With dry_run, return a plan without reading pixels or creating output.
#[pyfunction]
#[pyo3(signature=(source, output, *, resume=false, dry_run=false, **kwargs))]
fn raster_tiles(
    py: Python<'_>,
    source: &Bound<'_, PyAny>,
    output: std::path::PathBuf,
    resume: bool,
    dry_run: bool,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyAny>> {
    let json = py.import("json")?;
    let options: TileOptions = if let Some(kwargs) = kwargs {
        let text: String = json.call_method1("dumps", (kwargs,))?.extract()?;
        serde_json::from_str(&text).map_err(|e| invalid(e.to_string()))?
    } else {
        TileOptions::default()
    };
    let run = |r: &mut terrio::LazyRaster| -> terrio::Result<String> {
        if dry_run {
            serde_json::to_string(&tile::plan(&r.header(), &options)?.0).map_err(|e| terrio::Error::InvalidData(e.to_string()))
        } else {
            serde_json::to_string(&tile::to_directory(r, &options, &output, resume)?).map_err(|e| terrio::Error::InvalidData(e.to_string()))
        }
    };
    let result = if let Ok(raster) = source.extract::<Py<crate::raster::PyRaster>>() {
        let mut raster = raster.bind(py).try_borrow_mut()?;
        let lazy = &mut raster.inner;
        py.detach(|| run(lazy)).py()?
    } else {
        let path: std::path::PathBuf = source.extract().map_err(|_| invalid("expected a Raster or raster path"))?;
        let path = path.to_str().ok_or_else(|| invalid("input path must be UTF-8"))?;
        py.detach(|| run(&mut terrio::open_raster_lazy(path)?)).py()?
    };
    Ok(json.call_method1("loads", (result,))?.unbind())
}

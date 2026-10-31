//! Folder raster tiles without materializing a lazy input.
use crate::{Detach, PyResultExt, invalid};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use terrio::ops::raster_tile::{self as tile, TileOptions};
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(raster_tiles, m)?)?;
    m.add_function(wrap_pyfunction!(retile, m)?)?;
    Ok(())
}

/// Retile Raster/path inputs, retaining lazy windows. Options match Rust
/// RetileOptions; tile_size=[width,height], levels, overlap, row_directories,
/// csv and tile_index control the file layout. Dry runs write nothing.
#[pyfunction]
#[pyo3(signature=(sources, output, *, resume=false, dry_run=false, **kwargs))]
fn retile(
    py: Python<'_>,
    sources: &Bound<'_, PyAny>,
    output: std::path::PathBuf,
    resume: bool,
    dry_run: bool,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyAny>> {
    use terrio::ops::raster_retile::{self as re, RetileOptions, RetileSource};
    enum Arg {
        Path(String),
        Raster(Py<crate::raster::PyRaster>),
    }
    enum Input {
        File(Box<terrio::LazyRaster>),
        Python(Py<crate::raster::PyRaster>),
    }
    impl terrio::ops::warp::WarpSource for Input {
        fn info(&mut self) -> terrio::Result<terrio::ops::warp::WarpSourceInfo> {
            match self {
                Self::File(r) => terrio::ops::warp::WarpSource::info(r.as_mut()),
                Self::Python(obj) => Python::attach(|py| {
                    let mut r = obj.bind(py).try_borrow_mut().map_err(|e| terrio::Error::InvalidArgument(e.to_string()))?;
                    terrio::ops::warp::WarpSource::info(&mut r.inner)
                }),
            }
        }
        fn read_window(&mut self, w: terrio::Window, o: &terrio::WindowOptions) -> terrio::Result<terrio::RasterDataset> {
            match self {
                Self::File(r) => r.read_window(w, o),
                Self::Python(obj) => Python::attach(|py| {
                    let mut r = obj.bind(py).try_borrow_mut().map_err(|e| terrio::Error::InvalidArgument(e.to_string()))?;
                    let reader = &mut r.inner;
                    py.detach_result(|| reader.read_window(w, o))
                }),
            }
        }
    }
    let items: Vec<Bound<'_, PyAny>> =
        if sources.cast::<crate::raster::PyRaster>().is_ok() || sources.extract::<std::path::PathBuf>().is_ok() {
            vec![sources.clone()]
        } else {
            sources.extract().map_err(|_| invalid("expected a Raster/path or sequence of them"))?
        };
    let args = items
        .iter()
        .map(|item| {
            if let Ok(r) = item.extract::<Py<crate::raster::PyRaster>>() {
                return Ok(Arg::Raster(r));
            }
            let path: std::path::PathBuf = item.extract().map_err(|_| invalid("expected a Raster or raster path"))?;
            Ok(Arg::Path(path.into_os_string().into_string().map_err(|_| invalid("raster paths must be UTF-8"))?))
        })
        .collect::<PyResult<Vec<_>>>()?;
    let json = py.import("json")?;
    let options: RetileOptions = if let Some(kwargs) = kwargs {
        let text: String = json.call_method1("dumps", (kwargs,))?.extract()?;
        serde_json::from_str(&text).map_err(|e| invalid(e.to_string()))?
    } else {
        RetileOptions::default()
    };
    let text = py
        .detach_result(move || {
            let mut inputs = args
                .into_iter()
                .enumerate()
                .map(|(i, arg)| match arg {
                    Arg::Path(name) => Ok((name.clone(), Input::File(Box::new(terrio::open_raster_lazy(&name)?)))),
                    Arg::Raster(r) => Ok((format!("input_{}.tif", i + 1), Input::Python(r))),
                })
                .collect::<terrio::Result<Vec<_>>>()?;
            let mut sources: Vec<_> = inputs.iter_mut().map(|(name, source)| RetileSource { name: name.clone(), source }).collect();
            let result = if dry_run {
                serde_json::to_value(re::plan(&mut sources, &options)?)
            } else {
                serde_json::to_value(re::to_directory(&mut sources, &options, &output, resume)?)
            }
            .map_err(|e| terrio::Error::InvalidData(e.to_string()))?;
            serde_json::to_string(&result).map_err(|e| terrio::Error::InvalidData(e.to_string()))
        })
        .py()?;
    Ok(json.call_method1("loads", (text,))?.unbind())
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
        py.detach_result(|| run(lazy)).py()?
    } else {
        let path: std::path::PathBuf = source.extract().map_err(|_| invalid("expected a Raster or raster path"))?;
        let path = path.to_str().ok_or_else(|| invalid("input path must be UTF-8"))?;
        py.detach_result(|| run(&mut terrio::open_raster_lazy(path)?)).py()?
    };
    Ok(json.call_method1("loads", (result,))?.unbind())
}

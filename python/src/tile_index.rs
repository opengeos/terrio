//! Native and in-memory footprint indexes. Geometry/validation/serialization live in terrio.
use crate::raster::PyRaster;
use crate::vector::{PyLayer, PyVectorDataset};
use crate::{PyResultExt, invalid};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};
use serde::Deserialize;
use std::collections::BTreeMap;
use terrio::crs::SpatialRef;
use terrio::formats::VectorDriver;
use terrio::ops::tile_index as ti;

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyDatasetIndex>()?;
    m.add_function(wrap_pyfunction!(raster_index, m)?)?;
    m.add_function(wrap_pyfunction!(vector_index, m)?)?;
    Ok(())
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Number {
    Value(f64),
    Text(String),
}
impl Number {
    fn value(&self) -> terrio::Result<f64> {
        match self {
            Self::Value(v) => Ok(*v),
            Self::Text(s) => s.parse().map_err(|_| terrio::Error::InvalidArgument("invalid nodata value".into())),
        }
    }
}
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Args {
    output_layer: Option<String>,
    location_name: Option<String>,
    output_crs: Option<String>,
    source_crs_field_name: Option<String>,
    source_crs_format: Option<String>,
    metadata: BTreeMap<String, serde_json::Value>,
    min_pixel_size: Option<f64>,
    max_pixel_size: Option<f64>,
    fetch_metadata: Vec<[String; 3]>,
    resolution: Option<[f64; 2]>,
    bbox: Option<[f64; 4]>,
    ot: Option<String>,
    band_count: Option<usize>,
    nodata: Vec<Number>,
    color_interpretations: Vec<String>,
    mask: bool,
    source_layer_names: Vec<String>,
    source_layer_indexes: Vec<usize>,
    accept_different_crs: bool,
    accept_different_schemas: bool,
    dataset_name_only: bool,
    recursive: bool,
    filename_filters: Vec<String>,
    absolute_path: bool,
    skip_errors: bool,
    format: Option<String>,
    overwrite: bool,
}
impl Args {
    fn parse(py: Python<'_>, kwargs: Option<&Bound<'_, PyDict>>) -> PyResult<Self> {
        let Some(kwargs) = kwargs else { return Ok(Self::default()) };
        let json: String = py.import("json")?.call_method1("dumps", (kwargs,))?.extract()?;
        serde_json::from_str(&json).map_err(|e| invalid(format!("index options: {e}; use strings for non-finite nodata")))
    }
    fn common(&self) -> terrio::Result<ti::IndexOptions> {
        Ok(ti::IndexOptions {
            layer_name: self.output_layer.clone(),
            location_field: self.location_name.clone().unwrap_or_else(|| "location".into()),
            output_crs: self.output_crs.as_deref().map(SpatialRef::parse).transpose()?,
            source_crs_field: self.source_crs_field_name.clone(),
            source_crs_format: ti::SourceCrsFormat::parse(self.source_crs_format.as_deref().unwrap_or("auto"))?,
            metadata: self
                .metadata
                .iter()
                .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())))
                .collect(),
        })
    }
    fn inputs(&self) -> ti::IndexInputOptions {
        ti::IndexInputOptions {
            recursive: self.recursive,
            filename_filters: self.filename_filters.clone(),
            absolute_path: self.absolute_path,
            skip_errors: self.skip_errors,
        }
    }
    fn raster(&self) -> terrio::Result<ti::RasterIndexOptions> {
        if !self.source_layer_names.is_empty()
            || !self.source_layer_indexes.is_empty()
            || self.accept_different_crs
            || self.accept_different_schemas
            || self.dataset_name_only
        {
            return Err(terrio::Error::InvalidArgument("vector-only index options given to raster_index".into()));
        }
        Ok(ti::RasterIndexOptions {
            index: self.common()?,
            min_pixel_size: self.min_pixel_size,
            max_pixel_size: self.max_pixel_size,
            fetch_metadata: self
                .fetch_metadata
                .iter()
                .map(|[key, name, kind]| ti::MetadataField::from_type_name(key, name, kind))
                .collect::<terrio::Result<_>>()?,
            gti: ti::GtiOptions {
                resolution: self.resolution,
                extent: self.bbox,
                data_type: self.ot.as_deref().map(terrio::ops::parse_data_type).transpose()?,
                band_count: self.band_count,
                nodata: self.nodata.iter().map(Number::value).collect::<terrio::Result<_>>()?,
                color_interpretations: self.color_interpretations.clone(),
                mask: self.mask,
            },
        })
    }
    fn vector(&self) -> terrio::Result<ti::VectorIndexOptions> {
        if self.min_pixel_size.is_some()
            || self.max_pixel_size.is_some()
            || !self.fetch_metadata.is_empty()
            || self.resolution.is_some()
            || self.bbox.is_some()
            || self.ot.is_some()
            || self.band_count.is_some()
            || !self.nodata.is_empty()
            || !self.color_interpretations.is_empty()
            || self.mask
            || self.skip_errors
        {
            return Err(terrio::Error::InvalidArgument("raster-only index options given to vector_index".into()));
        }
        Ok(ti::VectorIndexOptions {
            index: self.common()?,
            accept_different_crs: self.accept_different_crs,
            accept_different_schemas: self.accept_different_schemas,
            dataset_name_only: self.dataset_name_only,
        })
    }
    fn selection(&self) -> ti::VectorIndexSelection {
        ti::VectorIndexSelection { names: self.source_layer_names.clone(), indexes: self.source_layer_indexes.clone() }
    }
    fn mapped_inputs(&self) -> PyResult<()> {
        if self.recursive || !self.filename_filters.is_empty() || self.absolute_path || self.skip_errors {
            return Err(invalid("discovery options apply to path inputs, not supplied source mappings"));
        }
        Ok(())
    }
}
fn write_options(format: Option<&str>) -> PyResult<terrio::VectorWriteOptions> {
    let driver = format.map(|f| VectorDriver::from_name(f).ok_or_else(|| invalid(format!("unknown vector format '{f}'")))).transpose()?;
    Ok(terrio::VectorWriteOptions { driver, ..Default::default() })
}
/// A polygon index with source attributes and GTI metadata kept together.
#[pyclass(module = "terrio", name = "DatasetIndex")]
pub struct PyDatasetIndex {
    inner: ti::DatasetIndex,
}
#[pymethods]
impl PyDatasetIndex {
    /// Number of indexed datasets/layers.
    #[getter]
    fn count(&self) -> usize {
        self.inner.layer.features.len()
    }
    /// The polygon layer, suitable for ordinary vector operations.
    #[getter]
    fn layer(&self) -> PyLayer {
        PyLayer { inner: self.inner.layer.clone() }
    }
    /// Driver metadata retained when writing GeoPackage/FlatGeobuf.
    #[getter]
    fn metadata(&self) -> BTreeMap<String, String> {
        self.inner.metadata.iter().cloned().collect()
    }
    /// Serialize to bytes. Use .shp.zip for a Shapefile archive.
    #[pyo3(signature=(filename="index.gpkg", *, format=None))]
    fn to_bytes<'py>(&self, py: Python<'py>, filename: &str, format: Option<&str>) -> PyResult<Bound<'py, PyBytes>> {
        let options = write_options(format)?;
        let bytes = py.detach(|| self.inner.to_bytes(filename, &options)).py()?;
        Ok(PyBytes::new(py, &bytes))
    }
    /// Write an index. Overwrite applies to single-file datasets; multi-file formats need a new basename.
    #[pyo3(signature=(path, *, format=None, overwrite=false))]
    fn write(&self, py: Python<'_>, path: &str, format: Option<&str>, overwrite: bool) -> PyResult<()> {
        let options = write_options(format)?;
        py.detach(|| self.inner.write(path, &options, overwrite)).py()
    }
}
fn paths(sources: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
    let paths =
        if let Ok(path) = sources.extract::<std::path::PathBuf>() { vec![path] } else { sources.extract::<Vec<std::path::PathBuf>>()? };
    paths.into_iter().map(|p| p.into_os_string().into_string().map_err(|_| invalid("index paths must be UTF-8"))).collect()
}
fn finish(py: Python<'_>, index: ti::DatasetIndex, dst: Option<&str>, args: &Args) -> PyResult<PyDatasetIndex> {
    if let Some(dst) = dst {
        let options = write_options(args.format.as_deref())?;
        py.detach(|| index.write(dst, &options, args.overwrite)).py()?;
    } else if args.format.is_some() || args.overwrite {
        return Err(invalid("format/overwrite require dst; otherwise use DatasetIndex.write/to_bytes"));
    }
    Ok(PyDatasetIndex { inner: index })
}
/// Create affine raster extent polygons without reading pixels through lazy-capable drivers.
/// Sources are paths (with native discovery options), or a mapping of stored locations to
/// Raster/path values. Keyword options mirror the CLI using snake_case; GTI options include
/// resolution, bbox, ot, band_count, nodata (a list; strings for nan/inf), color_interpretations,
/// mask and fetch_metadata (KEY,FIELD,TYPE tuples). Returns a DatasetIndex; dst optionally writes it.
#[pyfunction]
#[pyo3(signature=(sources, dst=None, **kwargs))]
fn raster_index(
    py: Python<'_>,
    sources: &Bound<'_, PyAny>,
    dst: Option<&str>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<PyDatasetIndex> {
    let args = Args::parse(py, kwargs)?;
    let options = args.raster().py()?;
    let mut protected_paths = Vec::new();
    let mut index = if let Ok(mapping) = sources.cast::<PyDict>() {
        args.mapped_inputs()?;
        let mut headers = Vec::new();
        for (location, source) in mapping.iter() {
            let location: String = location.extract()?;
            let header = if let Some(path) = crate::ops::raster_path(&source)? {
                protected_paths.push(path.clone());
                crate::with_warnings(py, || ti::RasterIndexSource::open_path(&path, location))?
            } else {
                let raster = source
                    .cast::<PyRaster>()
                    .map_err(|_| invalid("mapped raster inputs must be Raster objects or paths"))?
                    .try_borrow()
                    .map_err(|e| invalid(e.to_string()))?;
                ti::RasterIndexSource::from_lazy(location, &raster.inner).py()?
            };
            headers.push(header);
        }
        crate::with_warnings(py, || ti::raster_index(&headers, &options))?
    } else {
        let paths = paths(sources)?;
        crate::with_warnings(py, || ti::raster_index_paths(&paths, &args.inputs(), &options))?
    };
    for path in protected_paths {
        index.protect_source_path(&path).py()?;
    }
    finish(py, index, dst, &args)
}
/// Create layer envelope polygons. Paths use cached bounds where available and stream other
/// supported formats. Mappings associate stored locations with paths, Layer/VectorDataset
/// objects or GeoJSON. source_layer_names/indexes select original layers without renumbering.
#[pyfunction]
#[pyo3(signature=(sources, dst=None, **kwargs))]
fn vector_index(
    py: Python<'_>,
    sources: &Bound<'_, PyAny>,
    dst: Option<&str>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<PyDatasetIndex> {
    let args = Args::parse(py, kwargs)?;
    let options = args.vector().py()?;
    let selection = args.selection();
    let mut protected_paths = Vec::new();
    let mut index = if let Ok(mapping) = sources.cast::<PyDict>() {
        args.mapped_inputs()?;
        let mut headers = Vec::new();
        for (location, source) in mapping.iter() {
            let location: String = location.extract()?;
            let group = if let Ok(path) = source.extract::<std::path::PathBuf>() {
                let path = path.into_os_string().into_string().map_err(|_| invalid("index paths must be UTF-8"))?;
                protected_paths.push(path.clone());
                let mut group = crate::with_warnings(py, || ti::vector_index_sources(&path))?;
                for s in &mut group {
                    s.location = location.clone();
                }
                group
            } else if let Ok(layer) = source.cast::<PyLayer>() {
                vec![ti::VectorIndexSource::from_layer(&location, 0, &layer.borrow().inner)]
            } else if source.cast::<PyVectorDataset>().is_ok() {
                source
                    .getattr("layers")?
                    .try_iter()?
                    .enumerate()
                    .map(|(i, l)| {
                        let l = l?;
                        let layer = l.cast::<PyLayer>()?.borrow();
                        Ok(ti::VectorIndexSource::from_layer(&location, i, &layer.inner))
                    })
                    .collect::<PyResult<Vec<_>>>()?
            } else {
                crate::ops::input_layers(py, &source, None)?
                    .iter()
                    .enumerate()
                    .map(|(i, l)| ti::VectorIndexSource::from_layer(&location, i, l))
                    .collect()
            };
            headers.extend(group);
        }
        let headers = selection.apply(headers).py()?;
        crate::with_warnings(py, || ti::vector_index(&headers, &options))?
    } else {
        let paths = paths(sources)?;
        crate::with_warnings(py, || ti::vector_index_paths(&paths, &args.inputs(), &selection, &options))?
    };
    for path in protected_paths {
        index.protect_source_path(&path).py()?;
    }
    finish(py, index, dst, &args)
}

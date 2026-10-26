//! Dataset-level vector operations: concatenation, partitioning, field type changes, points
//! from coordinate fields, dataset creation and schema export (GDAL's `gdal vector concat`,
//! `partition`, `set-field-type`, `make-point`, `create` and `export-schema`).

use crate::ops::input_layers;
use crate::vector::{PyLayer, PyVectorDataset};
use crate::{PyResultExt, invalid, with_warnings};
use pyo3::prelude::*;
use terrio::crs::SpatialRef;
use terrio::formats::VectorDriver;
use terrio::ops::concat::{ConcatMode, ConcatOptions, ConcatSource, FieldStrategy};
#[cfg(not(target_family = "wasm"))]
use terrio::ops::partition::{PartitionOptions, PartitionScheme, PartitionWriteMode};
use terrio::ops::vector_schema::{self as vs, FieldSelector, OgrSchema, OgrType};
use terrio::{VectorDataset, VectorOpenOptions};

/// Registers the functions in the module.
pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(concat, m)?)?;
    #[cfg(not(target_family = "wasm"))]
    m.add_function(wrap_pyfunction!(partition, m)?)?;
    m.add_function(wrap_pyfunction!(set_field_type, m)?)?;
    m.add_function(wrap_pyfunction!(make_point, m)?)?;
    m.add_function(wrap_pyfunction!(export_schema, m)?)?;
    m.add_function(wrap_pyfunction!(create_vector, m)?)?;
    m.add_function(wrap_pyfunction!(rename_layer, m)?)?;
    m.add_function(wrap_pyfunction!(edit_vector, m)?)?;
    m.add_function(wrap_pyfunction!(update_vector, m)?)?;
    Ok(())
}

fn srs(s: Option<&str>) -> PyResult<Option<SpatialRef>> {
    s.map(SpatialRef::parse).transpose().py()
}

fn driver(name: Option<&str>) -> PyResult<Option<VectorDriver>> {
    name.map(|d| VectorDriver::from_name(d).ok_or_else(|| invalid(format!("unknown vector format '{d}'")))).transpose()
}

/// The UTF-8 path a Python object stands for (`str` or `os.PathLike`), when it is one.
fn path_of(obj: &Bound<'_, PyAny>) -> Option<String> {
    obj.extract::<std::path::PathBuf>().ok().and_then(|p| p.into_os_string().into_string().ok())
}

/// The layers and driver of a source: a path (read), a `VectorDataset`, a `Layer`, a list of
/// layers or GeoJSON.
fn source(py: Python<'_>, src: &Bound<'_, PyAny>, layers: Option<Vec<String>>) -> PyResult<(Vec<terrio::Layer>, Option<VectorDriver>)> {
    if let Ok(path) = src.extract::<std::path::PathBuf>() {
        let path = path.into_os_string().into_string().map_err(|_| invalid("paths must be UTF-8"))?;
        let options = VectorOpenOptions { layers: layers.unwrap_or_default(), ..Default::default() };
        let ds = with_warnings(py, || terrio::open_vector(&path, &options))?;
        return Ok((ds.layers, Some(ds.driver)));
    }
    let d = src.cast::<PyVectorDataset>().ok().and_then(|ds| VectorDriver::from_name(&ds.borrow().driver_name()));
    Ok((input_layers(py, src, layers)?, d))
}

fn require_new_output(dst: Option<&str>) -> PyResult<()> {
    if let Some(path) = dst
        && std::path::Path::new(path).exists()
    {
        return Err(invalid(format!("'{path}' already exists: choose a new output path")));
    }
    Ok(())
}

fn dataset(py: Python<'_>, layers: Vec<terrio::Layer>, driver: Option<VectorDriver>) -> PyResult<PyVectorDataset> {
    PyVectorDataset::new(py, VectorDataset { layers, driver: driver.unwrap_or(VectorDriver::GPKG) })
}

/// Concatenates vector datasets into one or several layers, like `terrio concat`
/// (GDAL's `gdal vector concat` and `ogrmerge.py`).
///
/// Args:
///     srcs: The sources: paths, `VectorDataset`s, `Layer`s or lists of layers.
///     dst: Also write the result to this path.
///     layers: Only these layers of each source.
///     mode: `"merge-per-layer-name"` (one output layer per layer name, the default),
///         `"stack"` (one per source layer) or `"single"` (one output layer).
///     output_layer: The output layer name (`single`, `"merged"` by default) or name template
///         (`stack`, `"{AUTO_NAME}"` by default) with `{AUTO_NAME}`, `{DS_NAME}`,
///         `{DS_BASENAME}`, `{DS_INDEX}`, `{LAYER_NAME}` and `{LAYER_INDEX}`.
///     source_layer_field_name: Add a field naming each feature's source layer.
///     source_layer_field_content: The template of that field's values (`"{AUTO_NAME}"`).
///     field_strategy: `"union"` (every field, the default) or `"intersection"`.
///     src_crs: The CRS of the sources, overriding what they declare.
///     dst_crs: Reproject every source to this CRS (otherwise to the first layer's).
///     format: The output format's short name, for `dst`.
///
/// Returns:
///     VectorDataset: The concatenated layers, in name order.
#[pyfunction]
#[pyo3(signature = (srcs, dst=None, *, layers=None, mode="merge-per-layer-name", output_layer=None, source_layer_field_name=None, source_layer_field_content=None, field_strategy="union", src_crs=None, dst_crs=None, format=None))]
#[allow(clippy::too_many_arguments)]
fn concat(
    py: Python<'_>,
    srcs: Vec<Bound<'_, PyAny>>,
    dst: Option<&str>,
    layers: Option<Vec<String>>,
    mode: &str,
    output_layer: Option<String>,
    source_layer_field_name: Option<String>,
    source_layer_field_content: Option<String>,
    field_strategy: &str,
    src_crs: Option<&str>,
    dst_crs: Option<&str>,
    format: Option<&str>,
) -> PyResult<PyVectorDataset> {
    require_new_output(dst)?;
    let mut sources = Vec::with_capacity(srcs.len());
    for (i, s) in srcs.iter().enumerate() {
        let name = path_of(s).unwrap_or_else(|| format!("dataset{i}"));
        let (mut ls, _) = source(py, s, None)?;
        if let Some(names) = &layers {
            ls.retain(|l| names.contains(&l.name));
        }
        sources.push(ConcatSource { name, layers: ls });
    }
    let o = ConcatOptions {
        mode: ConcatMode::parse(mode).py()?,
        output_layer,
        source_layer_field_name,
        source_layer_field_content,
        field_strategy: FieldStrategy::parse(field_strategy).py()?,
        src_crs: srs(src_crs)?,
        dst_crs: srs(dst_crs)?,
        ..Default::default()
    };
    let out = with_warnings(py, || terrio::ops::concat::concat(&sources, &o))?;
    if let Some(path) = dst {
        let options = terrio::VectorWriteOptions { driver: driver(format)?, ..Default::default() };
        py.detach(|| terrio::write_vector(&out, path, &options)).py()?;
    }
    dataset(py, out, None)
}

/// Partitions a vector dataset into files by field values, like `terrio partition`
/// (GDAL's `gdal vector partition`).
///
/// Args:
///     src: A path, `VectorDataset`, `Layer` or list of layers.
///     dst: The output directory.
///     fields: The fields to partition on (String or Integer fields, or `"OGR_GEOMETRY"`).
///     layers: Only these layers of `src`.
///     scheme: `"hive"` (`field=value` directories, the default) or `"flat"`.
///     pattern: The file name pattern with `{LAYER_NAME}`, `{FIELD_VALUE}` and one `%d`.
///     feature_limit: The most features per file.
///     max_file_size: The largest estimated file size, in bytes.
///     omit_partitioned_fields: Leave the partitioning fields out of the files.
///     format: The output format. By default, the input's.
///     overwrite: Replace an existing output directory that looks partitioned.
///     append: Add files to an existing output directory.
///
/// Returns:
///     list[str]: The files written.
#[cfg(not(target_family = "wasm"))]
#[pyfunction]
#[pyo3(signature = (src, dst, fields=None, *, layers=None, scheme="hive", pattern=None, feature_limit=None, max_file_size=None, omit_partitioned_fields=false, format=None, overwrite=false, append=false))]
#[allow(clippy::too_many_arguments)]
fn partition(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: &str,
    fields: Option<Vec<String>>,
    layers: Option<Vec<String>>,
    scheme: &str,
    pattern: Option<String>,
    feature_limit: Option<usize>,
    max_file_size: Option<u64>,
    omit_partitioned_fields: bool,
    format: Option<&str>,
    overwrite: bool,
    append: bool,
) -> PyResult<Vec<String>> {
    if overwrite && append {
        return Err(invalid("overwrite and append exclude each other"));
    }
    if overwrite && let Some(input) = path_of(src) {
        terrio::ops::partition::check_input_directory(&input, dst).py()?;
    }
    let (ls, src_driver) = source(py, src, layers)?;
    let d = match driver(format)? {
        Some(d) => d,
        None => src_driver
            .filter(|d| d.info().write && !d.info().extensions.is_empty())
            .ok_or_else(|| invalid("cannot infer the output format: give format"))?,
    };
    let o = PartitionOptions {
        fields: fields.unwrap_or_default(),
        scheme: PartitionScheme::parse(scheme).py()?,
        pattern,
        feature_limit,
        max_file_size,
        omit_partitioned_fields,
    };
    let mode = if overwrite {
        PartitionWriteMode::Overwrite
    } else if append {
        PartitionWriteMode::Append
    } else {
        PartitionWriteMode::Create
    };
    let options = terrio::VectorWriteOptions { driver: Some(d), ..Default::default() };
    with_warnings(py, || terrio::ops::partition::partition_to_dir(&ls, dst, &o, &options, mode))
}

/// Changes the type of vector fields, converting their values as GDAL does, like
/// `terrio set-field-type` (GDAL's `gdal vector set-field-type`).
///
/// Args:
///     src: A path, `VectorDataset`, `Layer` or list of layers.
///     field_type: The new type (`"Integer"`, `"Integer64"`, `"Real"`, `"String"`, `"Date"`,
///         `"Time"`, `"DateTime"`, `"Binary"`) or subtype (`"Boolean"`, `"Int16"`,
///         `"Float32"`, `"JSON"`, `"UUID"`).
///     field_name: The field to change.
///     src_field_type: Change every field of this type or subtype instead.
///     layers: Only these layers of `src`.
///     active_layer: Change only this layer (the others are kept as they are).
///
/// Returns:
///     VectorDataset: The layers. Features whose value cannot be converted (a date to a
///     number) are left out, with a warning, as in GDAL.
#[pyfunction]
#[pyo3(signature = (src, field_type, *, field_name=None, src_field_type=None, layers=None, active_layer=None))]
fn set_field_type(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    field_type: &str,
    field_name: Option<String>,
    src_field_type: Option<&str>,
    layers: Option<Vec<String>>,
    active_layer: Option<String>,
) -> PyResult<PyVectorDataset> {
    let to = OgrType::parse(field_type).py()?;
    let selector = match (field_name, src_field_type) {
        (Some(_), Some(_)) => return Err(invalid("give field_name or src_field_type, not both")),
        (Some(n), None) => FieldSelector::Name(n),
        (None, Some(t)) => FieldSelector::Type(OgrType::parse(t).py()?),
        (None, None) => return Err(invalid("give field_name or src_field_type")),
    };
    let (ls, d) = source(py, src, layers)?;
    let out = with_warnings(py, || vs::set_field_type_in(&ls, &selector, to, active_layer.as_deref()))?;
    dataset(py, out, d)
}

/// Replaces the geometries of a layer with points read from coordinate fields, like
/// `terrio make-point` (GDAL's `gdal vector make-point`).
///
/// Args:
///     src: A path, `VectorDataset`, `Layer` or list of layers (the first layer is used).
///     x: The field holding X coordinates.
///     y: The field holding Y coordinates.
///     z: The field holding Z coordinates.
///     m: The field holding M values.
///     crs: The CRS assigned to the points. By default, none (as in GDAL).
///     layer: The layer to read. By default, the first.
///
/// Returns:
///     Layer: The layer with point geometries and all its fields.
#[pyfunction]
#[pyo3(signature = (src, x, y, *, z=None, m=None, crs=None, layer=None))]
#[allow(clippy::too_many_arguments)]
fn make_point(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    x: String,
    y: String,
    z: Option<String>,
    m: Option<String>,
    crs: Option<&str>,
    layer: Option<String>,
) -> PyResult<PyLayer> {
    let (ls, _) = source(py, src, layer.map(|l| vec![l]))?;
    let first = ls.into_iter().next().ok_or_else(|| invalid("no input vector layer"))?;
    let o = terrio::ops::make_point::MakePointOptions { x, y, z, m, crs: srs(crs)? };
    let inner = with_warnings(py, || terrio::ops::make_point::make_point(&first, &o))?;
    Ok(PyLayer { inner })
}

/// The OGR_SCHEMA of a vector dataset, like `terrio export-schema` (GDAL's
/// `gdal vector export-schema`).
///
/// Args:
///     src: A path (GeoPackages are described from their table declarations), or a
///         `VectorDataset`, `Layer` or list of layers.
///     layers: Only these layers.
///
/// Returns:
///     dict: The OGR_SCHEMA document: `{"layers": [...]}`.
#[pyfunction]
#[pyo3(signature = (src, *, layers=None))]
fn export_schema<'py>(py: Python<'py>, src: &Bound<'py, PyAny>, layers: Option<Vec<String>>) -> PyResult<Bound<'py, PyAny>> {
    let schema = match src.extract::<std::path::PathBuf>() {
        Ok(path) => {
            let path = path.into_os_string().into_string().map_err(|_| invalid("paths must be UTF-8"))?;
            let names = layers.unwrap_or_default();
            with_warnings(py, || vs::export_schema_path(&path, &names))?
        }
        Err(_) => {
            let (ls, d) = source(py, src, layers)?;
            vs::export_schema(&ls, d)
        }
    };
    py.import("json")?.call_method1("loads", (schema.to_json(),))
}

/// Creates a vector dataset (empty layers), like `terrio create` (GDAL's `gdal vector create`),
/// from an OGR_SCHEMA, a template dataset, or explicit fields.
///
/// Args:
///     dst: The dataset to create.
///     schema: An OGR_SCHEMA (a dict, a JSON string or a path to a JSON file).
///     like: A template dataset (path, `VectorDataset`, `Layer` or list of layers).
///     layers: The template layers of `schema` or `like`.
///     output_layer: The layer name (required without `schema` and `like`).
///     geometry_type: The geometry type, such as `"POINT"` or `"MULTIPOLYGON"`.
///     crs: The CRS.
///     fid: The FID column name (new GeoPackages).
///     fields: Fields as `"NAME:TYPE[(WIDTH[,PRECISION])]"`.
///     format: The output format's short name.
///     update: Add the layers to an existing GeoPackage.
///     overwrite: Replace an existing dataset.
#[pyfunction]
#[pyo3(signature = (dst, *, schema=None, like=None, layers=None, output_layer=None, geometry_type=None, crs=None, fid=None, fields=None, format=None, update=false, overwrite=false))]
#[allow(clippy::too_many_arguments)]
fn create_vector(
    py: Python<'_>,
    dst: &str,
    schema: Option<&Bound<'_, PyAny>>,
    like: Option<&Bound<'_, PyAny>>,
    layers: Option<Vec<String>>,
    output_layer: Option<String>,
    geometry_type: Option<&str>,
    crs: Option<&str>,
    fid: Option<String>,
    fields: Option<Vec<String>>,
    format: Option<&str>,
    update: bool,
    overwrite: bool,
) -> PyResult<()> {
    let explicit = geometry_type.is_some() || crs.is_some() || fid.is_some() || fields.is_some();
    if schema.is_some() as u8 + like.is_some() as u8 > 1 || (explicit && (schema.is_some() || like.is_some())) {
        return Err(invalid("give one of schema, like, or explicit fields"));
    }
    let doc = match (schema, like) {
        (Some(s), _) => {
            let text = if let Some(t) = path_of(s) {
                if t.trim_start().starts_with('{') { t } else { std::fs::read_to_string(&t).map_err(|e| invalid(format!("{t}: {e}")))? }
            } else {
                py.import("json")?.call_method1("dumps", (s,))?.extract::<String>()?
            };
            let mut doc = OgrSchema::from_json(&text).py()?;
            if let Some(names) = &layers {
                doc.layers.retain(|l| names.contains(&l.name));
            }
            Some(doc)
        }
        (None, Some(l)) => Some(match path_of(l).ok_or(()) {
            Ok(path) => {
                let names = layers.clone().unwrap_or_default();
                with_warnings(py, || vs::export_schema_path(&path, &names))?
            }
            Err(_) => {
                let (ls, d) = source(py, l, layers.clone())?;
                vs::export_schema(&ls, d)
            }
        }),
        (None, None) => None,
    };
    let crs = srs(crs)?;
    let created =
        vs::created_layers(doc.as_ref(), output_layer.as_deref(), &fields.unwrap_or_default(), geometry_type, crs.as_ref(), fid.as_deref())
            .py()?;
    let o = vs::CreateOptions { write: terrio::VectorWriteOptions { driver: driver(format)?, ..Default::default() }, update, overwrite };
    with_warnings(py, || vs::create(&created, dst, &o))
}

/// Renames layers on a copy, optionally writing dst or changing a local GeoPackage in place.
/// Naming rules and uniqueness suffixes follow GDAL's vector rename-layer command.
#[pyfunction]
#[pyo3(signature = (src, dst=None, *, in_place=false, input_layer=None, output_layer=None, ascii=false, lower_case=false, filename_compatible=false, reserved_characters=None, replacement_character=None, max_length=None, format=None))]
#[allow(clippy::too_many_arguments)]
fn rename_layer(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    in_place: bool,
    input_layer: Option<String>,
    output_layer: Option<String>,
    ascii: bool,
    lower_case: bool,
    filename_compatible: bool,
    reserved_characters: Option<String>,
    replacement_character: Option<char>,
    max_length: Option<usize>,
    format: Option<&str>,
) -> PyResult<PyVectorDataset> {
    if !in_place {
        require_new_output(dst)?;
    }
    use terrio::ops::vector_edit as ve;
    let o = ve::RenameOptions {
        input_layer,
        output_layer,
        ascii,
        lower_case,
        filename_compatible,
        reserved_characters,
        replacement_character,
        max_length,
    };
    if in_place {
        if dst.is_some() || format.is_some() {
            return Err(invalid("in_place excludes dst and format"));
        }
        let path = path_of(src).ok_or_else(|| invalid("in_place requires a local dataset path"))?;
        with_warnings(py, || ve::rename_layers_in_place(&path, &o))?;
        let (ls, d) = source(py, src, None)?;
        return dataset(py, ls, d);
    }
    let (mut ls, d) = source(py, src, None)?;
    with_warnings(py, || ve::rename_layers(&mut ls, &o))?;
    let write = terrio::VectorWriteOptions { driver: driver(format)?, ..Default::default() };
    if let Some(dst) = dst {
        with_warnings(py, || terrio::write_vector(&ls, dst, &write))?;
    }
    dataset(py, ls, d)
}

/// Assigns vector CRS/type without transforming geometries. Metadata edits need path input
/// and dst, or in_place=True (a local GeoPackage). In-memory metadata is unsupported.
#[pyfunction]
#[pyo3(signature = (src, dst=None, *, in_place=false, active_layer=None, crs=None, geometry_type=None, metadata=None, unset_metadata=None, layer_metadata=None, unset_layer_metadata=None, unset_fid=false, format=None))]
#[allow(clippy::too_many_arguments)]
fn edit_vector(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: Option<&str>,
    in_place: bool,
    active_layer: Option<String>,
    crs: Option<&str>,
    geometry_type: Option<&str>,
    metadata: Option<Vec<String>>,
    unset_metadata: Option<Vec<String>>,
    layer_metadata: Option<Vec<String>>,
    unset_layer_metadata: Option<Vec<String>>,
    unset_fid: bool,
    format: Option<&str>,
) -> PyResult<PyVectorDataset> {
    if !in_place {
        require_new_output(dst)?;
    }
    use terrio::ops::vector_edit as ve;
    let o = ve::EditOptions {
        active_layer,
        crs: crs.map(ve::parse_crs_override).transpose().py()?,
        geometry_type: geometry_type.map(vs::parse_geometry_type).transpose().py()?,
        metadata: ve::parse_metadata_items(&metadata.unwrap_or_default()).py()?,
        unset_metadata: unset_metadata.unwrap_or_default(),
        layer_metadata: ve::parse_metadata_items(&layer_metadata.unwrap_or_default()).py()?,
        unset_layer_metadata: unset_layer_metadata.unwrap_or_default(),
        unset_fid,
    };
    let write = terrio::VectorWriteOptions { driver: driver(format)?, ..Default::default() };
    if in_place {
        if dst.is_some() || format.is_some() {
            return Err(invalid("in_place excludes dst and format"));
        }
        let path = path_of(src).ok_or_else(|| invalid("in_place requires a local dataset path"))?;
        with_warnings(py, || ve::edit_in_place(&path, &o))?;
        let (ls, d) = source(py, src, None)?;
        return dataset(py, ls, d);
    }
    if let (Some(path), Some(dst)) = (path_of(src), dst) {
        with_warnings(py, || ve::edit_to(&path, dst, &o, &write))?;
        let ds = with_warnings(py, || terrio::open_vector(dst, &Default::default()))?;
        return dataset(py, ds.layers, Some(ds.driver));
    }
    if !(o.metadata.is_empty() && o.unset_metadata.is_empty() && o.layer_metadata.is_empty() && o.unset_layer_metadata.is_empty()) {
        return Err(invalid("metadata edits require input and output paths, or in_place=True"));
    }
    let (mut ls, d) = source(py, src, None)?;
    with_warnings(py, || ve::edit_layers(&mut ls, &o))?;
    if let Some(dst) = dst {
        with_warnings(py, || terrio::write_vector(&ls, dst, &write))?;
    }
    dataset(py, ls, d)
}

/// Updates a destination dataset by FID or key fields. A destination path is changed in
/// place; an in-memory destination is copied. Returns the resulting VectorDataset.
#[pyfunction]
#[pyo3(signature = (src, dst, *, input_layer=None, output_layer=None, key=None, mode="merge"))]
fn update_vector(
    py: Python<'_>,
    src: &Bound<'_, PyAny>,
    dst: &Bound<'_, PyAny>,
    input_layer: Option<String>,
    output_layer: Option<String>,
    key: Option<Vec<String>>,
    mode: &str,
) -> PyResult<PyVectorDataset> {
    use terrio::ops::vector_edit as ve;
    let (ls, _) = source(py, src, None)?;
    let o = ve::UpdateOptions { input_layer, output_layer, key: key.unwrap_or_default(), mode: ve::UpdateMode::parse(mode).py()? };
    if let Some(path) = path_of(dst) {
        if path_of(src).and_then(|p| std::fs::canonicalize(p).ok()).zip(std::fs::canonicalize(&path).ok()).is_some_and(|(a, b)| a == b) {
            return Err(invalid("input and output datasets must be different"));
        }
        with_warnings(py, || ve::update(&ls, &path, &o))?;
        let (out, d) = source(py, dst, None)?;
        return dataset(py, out, d);
    }
    let (mut out, d) = source(py, dst, None)?;
    with_warnings(py, || ve::update_layers(&mut out, &ls, &o))?;
    dataset(py, out, d)
}

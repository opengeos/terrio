//! Dataset-level vector operations, as `terrio concat`, `partition`, `set-field-type`,
//! `make-point`, `create` and `export-schema` offer them (GDAL's `gdal vector` commands of the
//! same names). Files are bytes: `partition` returns the parts' paths and bytes, and
//! `createVector` the new dataset's bytes.

use crate::ops::{file_bytes, input_layers};
use crate::{Dataset, Inner, js_err, parse_options, to_js};
use serde::Deserialize;
use terrio::crs::SpatialRef;
use terrio::formats::VectorDriver;
use terrio::ops::concat::{ConcatMode, ConcatOptions, ConcatSource, FieldStrategy};
use terrio::ops::partition::{PartitionOptions, PartitionScheme};
use terrio::ops::vector_schema::{self as vs, FieldSelector, OgrSchema, OgrType};
use wasm_bindgen::prelude::*;

fn invalid(msg: impl std::fmt::Display) -> JsError {
    JsError::new(&msg.to_string())
}

fn srs(s: Option<&str>) -> Result<Option<SpatialRef>, JsError> {
    s.map(SpatialRef::parse).transpose().map_err(js_err)
}

/// The layers of a source and the driver that read them, when known.
fn source(v: &JsValue) -> Result<(Vec<terrio::Layer>, Option<VectorDriver>), JsError> {
    if let Some(inner) = Dataset::lookup(v)
        && let Inner::Vector(layers, driver) = &*inner.borrow()
    {
        return Ok((layers.clone(), Some(*driver)));
    }
    if let Some((bytes, name)) = file_bytes(v) {
        let ds = terrio::memory::open_vector_bytes(bytes, &name, &Default::default()).map_err(js_err)?;
        return Ok((ds.layers, Some(ds.driver)));
    }
    Ok((input_layers(v)?, None))
}

/// The name a source goes by in name templates: its file name, when given.
fn source_name(v: &JsValue, i: usize) -> String {
    file_bytes(v).map(|(_, n)| n).filter(|n| n != "data").unwrap_or_else(|| format!("dataset{i}"))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct ConcatArgs {
    layers: Option<Vec<String>>,
    mode: Option<String>,
    output_layer: Option<String>,
    source_layer_field_name: Option<String>,
    source_layer_field_content: Option<String>,
    field_strategy: Option<String>,
    src_crs: Option<String>,
    dst_crs: Option<String>,
}

/// Concatenates vector datasets, as `terrio concat` (GDAL's `gdal vector concat`). `srcs`: an
/// array of `Dataset`s, file bytes (`{ bytes, filename }` gives the name templates use) or
/// GeoJSON. Options: `layers`, `mode` (`merge-per-layer-name`, `stack`, `single`),
/// `outputLayer`, `sourceLayerFieldName`, `sourceLayerFieldContent`, `fieldStrategy`
/// (`union`, `intersection`), `srcCrs`, `dstCrs`.
#[wasm_bindgen]
pub fn concat(srcs: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let a: ConcatArgs = parse_options(&options)?;
    if !js_sys::Array::is_array(&srcs) {
        return Err(invalid("srcs must be an array"));
    }
    let mut sources = Vec::new();
    for (i, v) in js_sys::Array::from(&srcs).iter().enumerate() {
        let (mut layers, _) = source(&v)?;
        if let Some(names) = &a.layers {
            layers.retain(|l| names.contains(&l.name));
        }
        sources.push(ConcatSource { name: source_name(&v, i), layers });
    }
    let o = ConcatOptions {
        mode: ConcatMode::parse(a.mode.as_deref().unwrap_or("merge-per-layer-name")).map_err(js_err)?,
        output_layer: a.output_layer,
        source_layer_field_name: a.source_layer_field_name,
        source_layer_field_content: a.source_layer_field_content,
        field_strategy: FieldStrategy::parse(a.field_strategy.as_deref().unwrap_or("union")).map_err(js_err)?,
        src_crs: srs(a.src_crs.as_deref())?,
        dst_crs: srs(a.dst_crs.as_deref())?,
        ..Default::default()
    };
    let layers = terrio::ops::concat::concat(&sources, &o).map_err(js_err)?;
    Ok(Dataset::from_layers(layers, VectorDriver::GPKG))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct PartitionArgs {
    fields: Vec<String>,
    scheme: Option<String>,
    pattern: Option<String>,
    feature_limit: Option<usize>,
    max_file_size: Option<u64>,
    omit_partitioned_fields: bool,
    format: Option<String>,
}

/// Partitions vector data by field values, as `terrio partition` (GDAL's
/// `gdal vector partition`), returning the parts as `[{ path, bytes }]` (paths relative to the
/// output directory). Options: `fields`, `scheme` (`hive`, `flat`), `pattern`, `featureLimit`,
/// `maxFileSize` (bytes), `omitPartitionedFields`, `format` (by default the input's).
#[wasm_bindgen]
pub fn partition(src: JsValue, options: JsValue) -> Result<JsValue, JsError> {
    let a: PartitionArgs = parse_options(&options)?;
    let (layers, src_driver) = source(&src)?;
    let driver = match a.format.as_deref() {
        Some(f) => VectorDriver::from_name(f).ok_or_else(|| invalid(format!("unknown vector format '{f}'")))?,
        None => src_driver.filter(|d| d.info().write).ok_or_else(|| invalid("give the output format"))?,
    };
    let o = PartitionOptions {
        fields: a.fields,
        scheme: PartitionScheme::parse(a.scheme.as_deref().unwrap_or("hive")).map_err(js_err)?,
        pattern: a.pattern,
        feature_limit: a.feature_limit,
        max_file_size: a.max_file_size,
        omit_partitioned_fields: a.omit_partitioned_fields,
    };
    let parts = terrio::ops::partition::partition(&layers, &o, driver).map_err(js_err)?;
    let write = terrio::VectorWriteOptions { driver: Some(driver), ..Default::default() };
    let out = js_sys::Array::new();
    for p in parts {
        let bytes = terrio::memory::write_vector_bytes(std::slice::from_ref(&p.layer), &p.path, &write).map_err(js_err)?;
        let item = js_sys::Object::new();
        js_sys::Reflect::set(&item, &"path".into(), &JsValue::from_str(&p.path)).map_err(|_| invalid("cannot build the result"))?;
        js_sys::Reflect::set(&item, &"bytes".into(), &js_sys::Uint8Array::from(&bytes[..]))
            .map_err(|_| invalid("cannot build the result"))?;
        out.push(&item);
    }
    Ok(out.into())
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct SetFieldTypeArgs {
    field_name: Option<String>,
    src_field_type: Option<String>,
    active_layer: Option<String>,
}

/// Changes the type of vector fields with GDAL's value conversions, as `terrio set-field-type`
/// (GDAL's `gdal vector set-field-type`). `fieldType`: a type (`Integer`, `Integer64`, `Real`,
/// `String`, `Date`, `Time`, `DateTime`, `Binary`) or subtype (`Boolean`, `Int16`,
/// `Float32`, `JSON`, `UUID`). Options: `fieldName` or `srcFieldType`, `activeLayer`.
#[wasm_bindgen(js_name = setFieldType)]
pub fn set_field_type(src: JsValue, field_type: &str, options: JsValue) -> Result<Dataset, JsError> {
    let a: SetFieldTypeArgs = parse_options(&options)?;
    let to = OgrType::parse(field_type).map_err(js_err)?;
    let selector = match (a.field_name, a.src_field_type) {
        (Some(n), None) => FieldSelector::Name(n),
        (None, Some(t)) => FieldSelector::Type(OgrType::parse(&t).map_err(js_err)?),
        _ => return Err(invalid("give fieldName or srcFieldType")),
    };
    let (layers, driver) = source(&src)?;
    let changed = |l: &terrio::Layer| a.active_layer.as_ref().is_none_or(|n| &l.name == n);
    let first = layers.iter().find(|l| changed(l)).ok_or_else(|| invalid("no layer to change"))?;
    let has = |l: &terrio::Layer, n: &str| l.schema.fields().iter().any(|f| f.name.eq_ignore_ascii_case(n));
    if let FieldSelector::Name(n) = &selector
        && !has(first, n)
    {
        return Err(invalid(format!("cannot find field '{n}' in layer '{}'", first.name)));
    }
    let out = layers
        .iter()
        .map(|l| {
            let skip = !changed(l) || matches!(&selector, FieldSelector::Name(n) if !has(l, n));
            if skip { Ok(l.clone()) } else { vs::set_field_type(l, &selector, to) }
        })
        .collect::<terrio::Result<Vec<_>>>()
        .map_err(js_err)?;
    Ok(Dataset::from_layers(out, driver.unwrap_or(VectorDriver::GeoJSON)))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct MakePointArgs {
    z: Option<String>,
    m: Option<String>,
    crs: Option<String>,
    layer: Option<String>,
}

/// Point geometries from coordinate fields, as `terrio make-point` (GDAL's
/// `gdal vector make-point`), of the first layer (or `layer`). Options: `z`, `m`, `crs`
/// (assigned to the points), `layer`.
#[wasm_bindgen(js_name = makePoint)]
pub fn make_point(src: JsValue, x: &str, y: &str, options: JsValue) -> Result<Dataset, JsError> {
    let a: MakePointArgs = parse_options(&options)?;
    let (layers, _) = source(&src)?;
    let layer = match &a.layer {
        Some(n) => layers.into_iter().find(|l| &l.name == n).ok_or_else(|| invalid(format!("no layer named {n}")))?,
        None => layers.into_iter().next().ok_or_else(|| invalid("no input vector layer"))?,
    };
    let o = terrio::ops::make_point::MakePointOptions { x: x.into(), y: y.into(), z: a.z, m: a.m, crs: srs(a.crs.as_deref())? };
    let out = terrio::ops::make_point::make_point(&layer, &o).map_err(js_err)?;
    Ok(Dataset::from_layers(vec![out], VectorDriver::GeoJSON))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct SchemaArgs {
    layers: Option<Vec<String>>,
}

/// The OGR_SCHEMA document of vector data, as `terrio export-schema` (GDAL's
/// `gdal vector export-schema`). Options: `layers`.
#[wasm_bindgen(js_name = exportSchema)]
pub fn export_schema(src: JsValue, options: JsValue) -> Result<JsValue, JsError> {
    let a: SchemaArgs = parse_options(&options)?;
    let (mut layers, driver) = source(&src)?;
    if let Some(names) = &a.layers {
        layers.retain(|l| names.contains(&l.name));
    }
    to_js(&vs::export_schema(&layers, driver))
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct CreateArgs {
    schema: Option<serde_json::Value>,
    layers: Option<Vec<String>>,
    output_layer: Option<String>,
    geometry_type: Option<String>,
    crs: Option<String>,
    fid: Option<String>,
    fields: Vec<String>,
    format: Option<String>,
}

/// A new dataset of empty layers, as `terrio create` (GDAL's `gdal vector create`), as the
/// bytes of the file `filename` (whose extension tells the format, unless `format` does).
/// Options: `schema` (an OGR_SCHEMA object or JSON text) or `like` (vector data whose layers
/// are templates), `layers`, `outputLayer`, or `geometryType`, `crs` and `fields`
/// (`"NAME:TYPE(WIDTH,PRECISION)"`).
#[wasm_bindgen(js_name = createVector)]
pub fn create_vector(filename: &str, options: JsValue) -> Result<Vec<u8>, JsError> {
    let (rest, taken) = crate::ops::split_options(&options, &["like"])?;
    let a: CreateArgs = parse_options(&rest)?;
    let doc = match (&a.schema, &taken[0]) {
        (Some(_), Some(_)) => return Err(invalid("give schema or like, not both")),
        (Some(s), None) => {
            let text = match s {
                serde_json::Value::String(t) => t.clone(),
                v => v.to_string(),
            };
            Some(OgrSchema::from_json(&text).map_err(js_err)?)
        }
        (None, Some(l)) => {
            let (layers, driver) = source(l)?;
            Some(vs::export_schema(&layers, driver))
        }
        (None, None) => None,
    };
    let templated = doc.is_some();
    let mut created = match doc {
        Some(mut d) => {
            if let Some(names) = &a.layers {
                d.layers.retain(|l| names.contains(&l.name));
            }
            d.layers.iter().map(vs::layer_from_schema).collect::<terrio::Result<Vec<_>>>().map_err(js_err)?
        }
        None => {
            let name = a.output_layer.clone().ok_or_else(|| invalid("give outputLayer"))?;
            let mut layer = terrio::Layer::new(name);
            for spec in &a.fields {
                layer.add_field(vs::parse_field_spec(spec).map_err(js_err)?);
            }
            let (t, z, m) =
                a.geometry_type.as_deref().map(vs::parse_geometry_type).transpose().map_err(js_err)?.unwrap_or((None, false, false));
            layer.geom_type = t;
            if let Some(c) = srs(a.crs.as_deref())? {
                c.apply_to_layer(&mut layer);
            }
            vec![vs::CreatedLayer { layer, spatial: a.geometry_type.is_some(), z, m, fid: a.fid.clone() }]
        }
    };
    if let (Some(name), true) = (&a.output_layer, templated) {
        if created.len() != 1 {
            return Err(invalid("outputLayer needs a single template layer: choose one with layers"));
        }
        created[0].layer.name = name.clone();
    }
    if created.is_empty() {
        return Err(invalid("no layer to create"));
    }
    let driver = a
        .format
        .as_deref()
        .map(|f| VectorDriver::from_name(f).ok_or_else(|| invalid(format!("unknown vector format '{f}'"))))
        .transpose()?;
    let layers: Vec<terrio::Layer> = created.into_iter().map(|c| c.layer).collect();
    let write = terrio::VectorWriteOptions { driver, ..Default::default() };
    terrio::memory::write_vector_bytes(&layers, filename, &write).map_err(js_err)
}

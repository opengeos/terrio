//! Footprint indexes from uploaded rasters/vectors, retaining GTI metadata in output bytes.
use crate::ops::{file_bytes, input_layers, invalid};
use crate::{Dataset, Inner, js_err, parse_options, to_js};
use serde::Deserialize;
use std::collections::BTreeMap;
use terrio::crs::SpatialRef;
use terrio::formats::VectorDriver;
use terrio::ops::tile_index as ti;
use wasm_bindgen::prelude::*;

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
#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase", deny_unknown_fields)]
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
}
impl Args {
    fn parse(options: &JsValue) -> Result<Self, JsError> {
        for key in ["minPixelSize", "maxPixelSize"] {
            if !options.is_null() && !options.is_undefined() {
                let value = js_sys::Reflect::get(options, &JsValue::from_str(key)).map_err(|_| invalid("invalid index options"))?;
                if value.as_f64().is_some_and(|v| !v.is_finite()) {
                    return Err(invalid("pixel-size limits must be finite and nonnegative"));
                }
            }
        }
        parse_options(options)
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
    fn raster(&self) -> terrio::Result<ti::RasterIndexOptions> {
        if !self.source_layer_names.is_empty()
            || !self.source_layer_indexes.is_empty()
            || self.accept_different_crs
            || self.accept_different_schemas
            || self.dataset_name_only
        {
            return Err(terrio::Error::InvalidArgument("vector-only options given to rasterIndex".into()));
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
        {
            return Err(terrio::Error::InvalidArgument("raster-only options given to vectorIndex".into()));
        }
        Ok(ti::VectorIndexOptions {
            index: self.common()?,
            accept_different_crs: self.accept_different_crs,
            accept_different_schemas: self.accept_different_schemas,
            dataset_name_only: self.dataset_name_only,
        })
    }
}
/// Polygon index plus driver metadata, for GeoPackage/FlatGeobuf GTI output.
#[wasm_bindgen]
pub struct DatasetIndex {
    inner: ti::DatasetIndex,
}
#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct WriteArgs {
    format: Option<String>,
}
#[wasm_bindgen]
impl DatasetIndex {
    /// Number of indexed datasets/layers.
    #[wasm_bindgen(getter)]
    pub fn count(&self) -> usize {
        self.inner.layer.features.len()
    }
    /// The polygons as an ordinary vector Dataset (without driver metadata).
    pub fn layer(&self) -> Dataset {
        Dataset::from_layers(vec![self.inner.layer.clone()], VectorDriver::GeoJSON)
    }
    /// Layer metadata retained when serializing the index.
    #[wasm_bindgen(getter)]
    pub fn metadata(&self) -> Result<JsValue, JsError> {
        to_js(&self.inner.metadata.iter().cloned().collect::<BTreeMap<_, _>>())
    }
    /// Encode the index with metadata. Default filename index.gpkg; use .shp.zip for Shapefile.
    /// Options: format (otherwise inferred from filename).
    #[wasm_bindgen(js_name=toBytes)]
    pub fn to_bytes(&self, filename: Option<String>, options: JsValue) -> Result<Vec<u8>, JsError> {
        let a: WriteArgs = parse_options(&options)?;
        let driver = a
            .format
            .as_deref()
            .map(|f| VectorDriver::from_name(f).ok_or_else(|| invalid(format!("unknown vector format '{f}'"))))
            .transpose()?;
        self.inner
            .to_bytes(filename.as_deref().unwrap_or("index.gpkg"), &terrio::VectorWriteOptions { driver, ..Default::default() })
            .map_err(js_err)
    }
}
fn entries(sources: &JsValue) -> Result<Vec<(String, JsValue)>, JsError> {
    if !sources.is_object() || sources.is_null() || js_sys::Array::is_array(sources) || Dataset::lookup(sources).is_some() {
        return Err(invalid("give an object mapping stored source locations to datasets or uploaded files"));
    }
    js_sys::Object::entries(&js_sys::Object::from(sources.clone()))
        .iter()
        .map(|entry| {
            let pair = js_sys::Array::from(&entry);
            Ok((pair.get(0).as_string().ok_or_else(|| invalid("source locations must be strings"))?, pair.get(1)))
        })
        .collect()
}
fn raster_source(location: String, value: &JsValue) -> Result<ti::RasterIndexSource, JsError> {
    if let Some(inner) = Dataset::lookup(value) {
        return match &*inner.borrow() {
            Inner::Raster(r) => ti::RasterIndexSource::from_lazy(location, r).map_err(js_err),
            Inner::Vector(..) => Err(invalid("expected a raster source")),
        };
    }
    if let Some((bytes, name)) = file_bytes(value) {
        let raster = terrio::memory::open_raster_lazy_bytes(bytes, &name).map_err(js_err)?;
        return ti::RasterIndexSource::from_lazy(location, &raster).map_err(js_err);
    }
    Err(invalid("expected a raster Dataset, Uint8Array or {bytes,filename}"))
}
/// Create raster extent polygons from {storedLocation: Dataset/file} mappings, without
/// decoding pixels through lazy-capable readers. Options use camelCase CLI concepts:
/// outputLayer, locationName, outputCrs, sourceCrsFieldName/Format, metadata,
/// minPixelSize/maxPixelSize, fetchMetadata (KEY,FIELD,TYPE triples), resolution, bbox,
/// ot, bandCount, nodata (an array; strings for nan/inf), colorInterpretations and mask.
#[wasm_bindgen(js_name=rasterIndex)]
pub fn raster_index(sources: JsValue, options: JsValue) -> Result<DatasetIndex, JsError> {
    let args = Args::parse(&options)?;
    let options = args.raster().map_err(js_err)?;
    let headers = entries(&sources)?.into_iter().map(|(location, value)| raster_source(location, &value)).collect::<Result<Vec<_>, _>>()?;
    Ok(DatasetIndex { inner: ti::raster_index(&headers, &options).map_err(js_err)? })
}
/// Create vector layer envelope polygons from {storedLocation: Dataset/file/GeoJSON}
/// mappings. Common options match rasterIndex; sourceLayerNames/sourceLayerIndexes select
/// original layers. acceptDifferentCrs, acceptDifferentSchemas and datasetNameOnly are optional.
#[wasm_bindgen(js_name=vectorIndex)]
pub fn vector_index(sources: JsValue, options: JsValue) -> Result<DatasetIndex, JsError> {
    let args = Args::parse(&options)?;
    let options = args.vector().map_err(js_err)?;
    let selection = ti::VectorIndexSelection { names: args.source_layer_names, indexes: args.source_layer_indexes };
    let mut headers = Vec::new();
    for (location, value) in entries(&sources)? {
        let group: Vec<ti::VectorIndexSource> = if let Some(inner) = Dataset::lookup(&value) {
            match &*inner.borrow() {
                Inner::Vector(layers, _) => {
                    layers.iter().enumerate().map(|(i, l)| ti::VectorIndexSource::from_layer(&location, i, l)).collect()
                }
                Inner::Raster(_) => return Err(invalid("expected a vector source")),
            }
        } else {
            input_layers(&value)?.iter().enumerate().map(|(i, l)| ti::VectorIndexSource::from_layer(&location, i, l)).collect()
        };
        headers.extend(group);
    }
    let headers = selection.apply(headers).map_err(js_err)?;
    Ok(DatasetIndex { inner: ti::vector_index(&headers, &options).map_err(js_err)? })
}

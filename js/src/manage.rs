//! Raster creation and GeoTIFF management through in-memory file bytes.
use crate::{Dataset, Inner, js_err, parse_options};
use serde::Deserialize;
use terrio::ops::{Resampling, create, edit, overview, update};
use wasm_bindgen::prelude::*;

fn source(src: &JsValue) -> Result<terrio::LazyRaster, JsError> {
    if let Some(inner) = Dataset::lookup(src) {
        return match &mut *inner.borrow_mut() {
            Inner::Raster(r) => Ok(r.shared()),
            _ => Err(JsError::new("expected a raster dataset")),
        };
    }
    let (bytes, name) = crate::ops::file_bytes(src).ok_or_else(|| JsError::new("expected a raster dataset or file bytes"))?;
    terrio::memory::open_raster_lazy_bytes(bytes, &name).map_err(js_err)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct CreateArgs {
    size: Option<(usize, usize)>,
    band_count: Option<usize>,
    ot: Option<String>,
    crs: Option<String>,
    bbox: Option<[f64; 4]>,
    nodata: Option<f64>,
    unset_nodata: bool,
    burn: Vec<f64>,
    metadata: std::collections::BTreeMap<String, String>,
    copy_metadata: bool,
}

/// Creates a filled raster Dataset. `like` is a raster or null; options override its header.
/// Options: size, bandCount, ot, crs, bbox, nodata, unsetNodata, burn, metadata, copyMetadata.
#[wasm_bindgen(js_name = createRaster)]
pub fn create_raster(like: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let a: CreateArgs = parse_options(&options)?;
    let template = if like.is_null() || like.is_undefined() { None } else { Some(source(&like)?) };
    let o = create::CreateOptions {
        size: a.size,
        band_count: a.band_count,
        data_type: a.ot.as_deref().map(terrio::ops::parse_data_type).transpose().map_err(js_err)?,
        crs: a.crs.as_deref().map(terrio::crs::SpatialRef::parse).transpose().map_err(js_err)?,
        bbox: a.bbox,
        nodata: a.nodata,
        unset_nodata: a.unset_nodata,
        burn: a.burn,
        metadata: a.metadata.into_iter().collect(),
        copy_metadata: a.copy_metadata,
        ..Default::default()
    };
    Ok(Dataset::from_raster(create::create_raster(template.as_ref(), &o).map_err(js_err)?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct AddArgs {
    levels: Vec<usize>,
    min_size: usize,
    resampling: String,
    compression: Option<String>,
    ignore_cog_layout_break: bool,
}
impl Default for AddArgs {
    fn default() -> Self {
        Self { levels: Vec::new(), min_size: 256, resampling: "nearest".into(), compression: None, ignore_cog_layout_break: false }
    }
}

/// Adds internal GeoTIFF overview levels and returns updated file bytes.
/// Options: levels, minSize, resampling, compression, ignoreCogLayoutBreak.
#[wasm_bindgen(js_name = addOverviews)]
pub fn add_overviews(bytes: Vec<u8>, options: JsValue) -> Result<Vec<u8>, JsError> {
    let a: AddArgs = parse_options(&options)?;
    let o = overview::OverviewOptions {
        levels: a.levels,
        min_size: a.min_size,
        resampling: Resampling::parse(&a.resampling).map_err(js_err)?,
        compression: a.compression,
        ignore_cog_layout_break: a.ignore_cog_layout_break,
        external: false,
    };
    overview::add_overviews_to_bytes(&bytes, &o).map_err(js_err)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct DeleteArgs {
    ignore_cog_layout_break: bool,
}

/// Deletes internal overview links and returns updated GeoTIFF bytes.
#[wasm_bindgen(js_name = deleteOverviews)]
pub fn delete_overviews(bytes: Vec<u8>, options: JsValue) -> Result<Vec<u8>, JsError> {
    let a: DeleteArgs = parse_options(&options)?;
    overview::delete_overviews_from_bytes(
        &bytes,
        &overview::DeleteOptions { external: false, ignore_cog_layout_break: a.ignore_cog_layout_break },
    )
    .map_err(js_err)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct RefreshArgs {
    levels: Vec<usize>,
    resampling: Option<String>,
    bbox: Vec<[f64; 4]>,
    ignore_cog_layout_break: bool,
}

/// Refreshes internal GeoTIFF overviews. Options: levels, resampling, bbox, ignoreCogLayoutBreak.
#[wasm_bindgen(js_name = refreshOverviews)]
pub fn refresh_overviews(bytes: Vec<u8>, options: JsValue) -> Result<Vec<u8>, JsError> {
    let a: RefreshArgs = parse_options(&options)?;
    let o = overview::RefreshOptions {
        levels: a.levels,
        resampling: a.resampling.as_deref().map(Resampling::parse).transpose().map_err(js_err)?,
        bbox: a.bbox,
        external: false,
        ignore_cog_layout_break: a.ignore_cog_layout_break,
    };
    overview::refresh_overviews_in_bytes(&bytes, &o).map_err(js_err)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct UpdateArgs {
    resampling: String,
    geometry: Option<String>,
    geometry_crs: Option<String>,
    no_update_overviews: bool,
    error_threshold: Option<f64>,
    ignore_cog_layout_break: bool,
}
impl Default for UpdateArgs {
    fn default() -> Self {
        Self {
            resampling: "nearest".into(),
            geometry: None,
            geometry_crs: None,
            no_update_overviews: false,
            error_threshold: None,
            ignore_cog_layout_break: false,
        }
    }
}

/// Warps valid source samples into destination GeoTIFF bytes and returns the updated file.
/// Options: resampling, geometry, geometryCrs, noUpdateOverviews, errorThreshold, ignoreCogLayoutBreak.
#[wasm_bindgen(js_name = updateRaster)]
pub fn update_raster(src: JsValue, dst: Vec<u8>, options: JsValue) -> Result<Vec<u8>, JsError> {
    let a: UpdateArgs = parse_options(&options)?;
    if a.geometry.is_none() && a.geometry_crs.is_some() {
        return Err(JsError::new("geometryCrs requires geometry"));
    }
    let crs = a.geometry_crs.as_deref().map(terrio::crs::SpatialRef::parse).transpose().map_err(js_err)?;
    let o = update::UpdateOptions {
        resampling: Resampling::parse(&a.resampling).map_err(js_err)?,
        geometry: a.geometry.as_deref().map(|g| update::parse_geometry(g, crs)).transpose().map_err(js_err)?,
        no_update_overviews: a.no_update_overviews,
        error_threshold: a.error_threshold,
        ignore_cog_layout_break: a.ignore_cog_layout_break,
    };
    update::update_raster_bytes_owned(&mut source(&src)?, dst, &o).map_err(js_err)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct EditArgs {
    crs: Option<String>,
    unset_crs: bool,
    geotransform: Option<[f64; 6]>,
    bbox: Option<[f64; 4]>,
    gcps: Vec<[f64; 5]>,
    nodata: Option<f64>,
    unset_nodata: bool,
    metadata: std::collections::BTreeMap<String, String>,
    unset_metadata: Vec<String>,
    unset_all_metadata: bool,
    stats: bool,
    approx_stats: bool,
    unset_stats: bool,
    histogram: bool,
    ignore_cog_layout_break: bool,
}

/// Edits GeoTIFF bytes without resampling. Histograms need a sidecar and are refused.
#[wasm_bindgen(js_name = editRaster)]
pub fn edit_raster(bytes: Vec<u8>, options: JsValue) -> Result<Vec<u8>, JsError> {
    let a: EditArgs = parse_options(&options)?;
    let options = edit::EditOptions {
        crs: a.crs.as_deref().map(terrio::crs::SpatialRef::parse).transpose().map_err(js_err)?,
        unset_crs: a.unset_crs,
        geo_transform: a.geotransform,
        bbox: a.bbox,
        gcps: a
            .gcps
            .into_iter()
            .enumerate()
            .map(|(i, [pixel, line, x, y, z])| terrio::Gcp { id: (i + 1).to_string(), info: String::new(), pixel, line, x, y, z })
            .collect(),
        nodata: a.nodata,
        unset_nodata: a.unset_nodata,
        metadata: a.metadata.into_iter().collect(),
        unset_metadata: a.unset_metadata,
        unset_all_metadata: a.unset_all_metadata,
        stats: a.stats,
        approx_stats: a.approx_stats,
        unset_stats: a.unset_stats,
        histogram: a.histogram,
        ignore_cog_layout_break: a.ignore_cog_layout_break,
    };
    edit::edit_raster_bytes(&bytes, &options).map_err(js_err)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MoveArgs {
    t_srs: String,
    #[serde(default)]
    s_srs: Option<String>,
    #[serde(default)]
    error_threshold: Option<f64>,
    #[serde(default)]
    ignore_cog_layout_break: bool,
}

/// Reports or changes georeferencing without resampling. Returns {bytes, report};
/// errorThreshold is required to write an acceptable affine transformation.
#[wasm_bindgen(js_name = moveRaster)]
pub fn move_raster(bytes: Vec<u8>, options: JsValue) -> Result<JsValue, JsError> {
    let a: MoveArgs = parse_options(&options)?;
    if a.t_srs.trim().is_empty() {
        return Err(JsError::new("tSrs is required"));
    }
    let options = edit::MoveOptions {
        t_srs: terrio::crs::SpatialRef::parse(&a.t_srs).map_err(js_err)?,
        s_srs: a.s_srs.as_deref().map(terrio::crs::SpatialRef::parse).transpose().map_err(js_err)?,
        error_threshold: a.error_threshold,
        ignore_cog_layout_break: a.ignore_cog_layout_break,
    };
    let (bytes, report) = edit::move_raster_bytes(&bytes, &options).map_err(js_err)?;
    let result = js_sys::Object::new();
    js_sys::Reflect::set(&result, &"bytes".into(), &js_sys::Uint8Array::from(bytes.as_slice()))
        .map_err(|_| JsError::new("cannot set move bytes"))?;
    let report = crate::to_js(&serde_json::json!({"geotransform": report.geo_transform, "corners": report.corners,
        "errors": report.errors, "maxError": report.max_error, "updated": report.updated}))?;
    js_sys::Reflect::set(&result, &"report".into(), &report).map_err(|_| JsError::new("cannot set move report"))?;
    Ok(result.into())
}

/// Validates uploaded TIFF structure without decoding pixels. Options: profile
/// (`rio-cogeo` or `gdal`), strict, fullCheck (default automatic; true for bytes).
/// GDAL full checks inspect all compressed block leaders/trailers and ordering.
#[wasm_bindgen(js_name = validateCog)]
pub fn validate_cog(bytes: &[u8], options: JsValue) -> Result<JsValue, JsError> {
    #[derive(Deserialize, Default)]
    #[serde(rename_all = "camelCase", default, deny_unknown_fields)]
    struct Args {
        strict: bool,
        profile: terrio::CogValidationProfile,
        full_check: Option<bool>,
    }
    let args: Args = parse_options(&options)?;
    let options = terrio::CogValidationOptions { strict: args.strict, profile: args.profile, full_check: args.full_check };
    crate::to_js(&terrio::validate_cog_bytes(bytes, &options).map_err(js_err)?)
}

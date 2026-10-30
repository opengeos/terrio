//! Vector clipping through the shared core.
use crate::ops::{file_bytes, input_layers, invalid};
use crate::{Dataset, Inner, js_err, parse_options};
use serde::Deserialize;
use terrio::crs::SpatialRef;
use terrio::formats::{Kind, VectorDriver};
use terrio::ops::vector_clip::{ClipRegion, ClipTemplateOptions};
use wasm_bindgen::prelude::*;

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct Args {
    bbox: Option<[f64; 4]>,
    geometry: Option<serde_json::Value>,
    like: Option<serde::de::IgnoredAny>,
    crs: Option<String>,
    layers: Option<Vec<String>>,
    like_layer: Option<String>,
    like_where: Option<String>,
    like_sql: Option<String>,
}

/// Clips vector features. Supply exactly one of `bbox: [xmin,ymin,xmax,ymax]`,
/// `geometry` (polygon WKT or GeoJSON text/object), or `like` (vector/raster Dataset,
/// uploaded bytes, or GeoJSON). `crs` references bbox/geometry; GeoJSON defaults to
/// WGS84. Optional `layers` selects input layers. `likeLayer`, `likeWhere` and
/// `likeSql` select/filter a polygon template. Raster template bounds use metadata;
/// lazy raster pixels are not loaded. Returns a Dataset retaining attributes and
/// input CRS, with sequential feature IDs. Line cuts interpolate Z/M; constructed
/// polygon topology is XY.
#[wasm_bindgen(js_name=clipVector)]
pub fn clip_vector(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let args: Args = parse_options(&options)?;
    if usize::from(args.bbox.is_some()) + usize::from(args.geometry.is_some()) + usize::from(args.like.is_some()) != 1 {
        return Err(invalid("provide exactly one of bbox, geometry or like"));
    }
    if args.like.is_none() && (args.like_layer.is_some() || args.like_where.is_some() || args.like_sql.is_some()) {
        return Err(invalid("likeLayer/Where/Sql require like"));
    }
    if args.like.is_some() && args.crs.is_some() {
        return Err(invalid("crs applies only to bbox/geometry"));
    }
    let crs = args.crs.as_deref().map(SpatialRef::parse).transpose().map_err(js_err)?;
    let template = ClipTemplateOptions { layer: args.like_layer, where_clause: args.like_where, sql: args.like_sql };
    let region = if let Some(bbox) = args.bbox {
        ClipRegion::bbox(bbox, crs).map_err(js_err)?
    } else if let Some(geometry) = args.geometry {
        let text = match geometry {
            serde_json::Value::String(s) => s,
            g => g.to_string(),
        };
        ClipRegion::parse(&text, crs).map_err(js_err)?
    } else {
        let like = js_sys::Reflect::get(&options, &JsValue::from_str("like")).map_err(|_| invalid("cannot read like"))?;
        template_region(&like, &template)?
    };
    let mut layers = input_layers(&src)?;
    if let Some(names) = args.layers {
        layers = terrio::ops::vector_filter::select_layers(layers, &names).map_err(js_err)?;
    }
    let out = layers.iter().map(|l| terrio::ops::vector_clip::clip(l, &region)).collect::<terrio::Result<Vec<_>>>().map_err(js_err)?;
    Ok(Dataset::from_layers(out, VectorDriver::GeoJSON))
}

fn raster_options(options: &ClipTemplateOptions) -> Result<(), JsError> {
    if options.layer.is_some() || options.where_clause.is_some() || options.sql.is_some() {
        Err(invalid("likeLayer/Where/Sql require a vector template"))
    } else {
        Ok(())
    }
}
fn template_region(like: &JsValue, options: &ClipTemplateOptions) -> Result<ClipRegion, JsError> {
    if let Some(inner) = Dataset::lookup(like) {
        return match &*inner.borrow() {
            Inner::Raster(r) => {
                raster_options(options)?;
                ClipRegion::from_raster(&r.header()).map_err(js_err)
            }
            Inner::Vector(layers, _) => ClipRegion::from_layers(layers, options).map_err(js_err),
        };
    }
    if let Some((bytes, name)) = file_bytes(like) {
        let kind = terrio::inspection::identify_bytes(&bytes, &name);
        let has_raster = kind.as_ref().is_some_and(|i| i.kinds.contains(&Kind::Raster));
        let has_vector = kind.as_ref().is_some_and(|i| i.kinds.contains(&Kind::Vector));
        if has_raster && !has_vector {
            raster_options(options)?;
            return ClipRegion::from_raster(&terrio::memory::open_raster_lazy_bytes(bytes, &name).map_err(js_err)?.header())
                .map_err(js_err);
        }
        if has_raster && has_vector {
            return match terrio::memory::open_bytes(bytes, &name).map_err(js_err)? {
                terrio::Dataset::Raster(r) => {
                    raster_options(options)?;
                    ClipRegion::from_raster(&r).map_err(js_err)
                }
                terrio::Dataset::Vector(v) => ClipRegion::from_layers(&v.layers, options).map_err(js_err),
            };
        }
        return ClipRegion::from_layers(
            &terrio::memory::open_vector_bytes(bytes, &name, &Default::default()).map_err(js_err)?.layers,
            options,
        )
        .map_err(js_err);
    }
    ClipRegion::from_layers(&input_layers(like)?, options).map_err(js_err)
}

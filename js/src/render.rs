//! Pictures of datasets: `render` (RGBA for a canvas) and `renderPng`.
use crate::ops::{file_bytes, invalid, selected_input_layers};
use crate::{Dataset, Inner, js_err, parse_options};
use serde::Deserialize;
use terrio::ops::render::{self as draw, Colormap, Image, RenderOptions, Stretch};
use wasm_bindgen::prelude::*;

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "camelCase")]
struct Args {
    width: Option<usize>,
    height: Option<usize>,
    max_size: usize,
    bands: Vec<usize>,
    colormap: Option<String>,
    stretch: String,
    layers: Vec<String>,
    attribute: Option<String>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            width: None,
            height: None,
            max_size: draw::DEFAULT_MAX_SIZE,
            bands: Vec::new(),
            colormap: None,
            stretch: "auto".into(),
            layers: Vec::new(),
            attribute: None,
        }
    }
}

fn image(src: &JsValue, options: &JsValue) -> Result<Image, JsError> {
    let args: Args = parse_options(options)?;
    let render_options = RenderOptions {
        width: args.width,
        height: args.height,
        max_size: args.max_size,
        bands: args.bands,
        colormap: args.colormap.as_deref().map(Colormap::parse).transpose().map_err(js_err)?,
        stretch: Stretch::parse(&args.stretch).map_err(js_err)?,
        attribute: args.attribute,
    };
    if let Some(inner) = Dataset::lookup(src) {
        let raster = matches!(&*inner.borrow(), Inner::Raster(_));
        if raster {
            if render_options.attribute.is_some() || !args.layers.is_empty() {
                return Err(invalid("attribute and layers are for vector datasets"));
            }
            let mut inner = inner.try_borrow_mut().map_err(|e| invalid(e.to_string()))?;
            if let Inner::Raster(source) = &mut *inner {
                return draw::render_raster(source, &render_options).map_err(js_err);
            }
        }
    } else if let Some((bytes, name)) = file_bytes(src)
        && render_options.attribute.is_none()
        && args.layers.is_empty()
        && let Ok(mut source) = terrio::memory::open_raster_lazy_bytes(bytes, &name)
    {
        return draw::render_raster(&mut source, &render_options).map_err(js_err);
    }
    draw::render_vector(selected_input_layers(src, &args.layers)?, &render_options).map_err(js_err)
}

/// Draws a raster or vector dataset as RGBA pixels: {width, height, data}, data a
/// Uint8ClampedArray ready for `new ImageData(data, width, height)`. src is a Dataset, file
/// bytes ({bytes, filename}) or GeoJSON. Options: width, height (one keeps the aspect ratio),
/// maxSize (1024; no enlarging), bands (one, or three as RGB), colormap (gray, viridis, magma,
/// inferno, plasma, terrain, rdbu, rdylgn; _r reverses), stretch ("auto", "minmax",
/// "pLOW,pHIGH", "LOW,HIGH"), layers and attribute (vector). Rasters are read from the coarsest
/// fitting overview; nodata and masked pixels are transparent.
#[wasm_bindgen]
pub fn render(src: JsValue, options: JsValue) -> Result<JsValue, JsError> {
    let image = image(&src, &options)?;
    let out = js_sys::Object::new();
    let set = |key: &str, value: &JsValue| js_sys::Reflect::set(&out, &JsValue::from_str(key), value).map(|_| ());
    set("width", &JsValue::from(image.width as u32)).map_err(|_| invalid("cannot build the result"))?;
    set("height", &JsValue::from(image.height as u32)).map_err(|_| invalid("cannot build the result"))?;
    set("data", &js_sys::Uint8ClampedArray::from(image.rgba.as_slice()).into()).map_err(|_| invalid("cannot build the result"))?;
    Ok(out.into())
}

/// Draws a raster or vector dataset as PNG bytes, with the options of `render`.
#[wasm_bindgen(js_name = renderPng)]
pub fn render_png(src: JsValue, options: JsValue) -> Result<Vec<u8>, JsError> {
    image(&src, &options)?.to_png().map_err(js_err)
}

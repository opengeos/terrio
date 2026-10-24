//! The processing operations, as the command line and Python offer them: `warp`, `translate`,
//! `rasterize`, `polygonize`, `dem`, `calc`, `mosaic`, `proximity` and `sieve`.
//!
//! Inputs are `Dataset`s, or the bytes of a file (a `Uint8Array`, or `{ bytes, filename }` when
//! the format cannot be told from the bytes); vector inputs may also be GeoJSON (an object or a
//! string). Outputs are `Dataset`s, written to bytes with `toBytes`. Options are objects with
//! camelCase names after the command-line options (`tSrs`, `srcNodata`, `cropToCutline`...).

use crate::{Dataset, Inner, js_err, parse_options};
use serde::Deserialize;
use terrio::crs::SpatialRef;
use terrio::formats::VectorDriver;
use terrio::ops::Resampling;
use terrio::ops::calc::{CalcExtent, CalcInput, CalcNodata, CalcOptions};
use terrio::ops::dem::{ColorMap, ColorSelection, DemOptions, DemProduct, GradientAlg, Shading, TriAlg};
use terrio::ops::mosaic::{MosaicOptions, MosaicSource, ResolutionStrategy};
use terrio::ops::parse_data_type;
use terrio::ops::polygonize::PolygonizeOptions;
use terrio::ops::proximity::{ProximityOptions, parse_values};
use terrio::ops::rasterize::{Grid, MergeAlg, RasterizeOptions};
use terrio::ops::sieve::{SieveMask, SieveOptions};
use terrio::ops::translate::{Scale, Size, TranslateOptions};
use terrio::ops::vector_filter::VectorTranslateOptions;
use terrio::ops::warp::{Cutline, Nodata, WarpOptions};
use terrio::{Layer, RasterDataset};
use wasm_bindgen::prelude::*;

fn invalid(msg: impl std::fmt::Display) -> JsError {
    JsError::new(&msg.to_string())
}

/// The bytes of a file given as a `Uint8Array` or `{ bytes, filename }`, with its file name.
fn file_bytes(v: &JsValue) -> Option<(Vec<u8>, String)> {
    if let Some(a) = v.dyn_ref::<js_sys::Uint8Array>() {
        return Some((a.to_vec(), "data".into()));
    }
    if v.is_object() && !js_sys::Array::is_array(v) {
        let bytes = js_sys::Reflect::get(v, &JsValue::from_str("bytes")).ok()?;
        let a = bytes.dyn_ref::<js_sys::Uint8Array>()?;
        let name = js_sys::Reflect::get(v, &JsValue::from_str("filename")).ok()?.as_string().unwrap_or_else(|| "data".into());
        return Some((a.to_vec(), name));
    }
    None
}

/// A raster given to an operation: a `Dataset`, or a file's bytes.
pub(crate) fn input_raster(v: &JsValue) -> Result<RasterDataset, JsError> {
    if let Some(inner) = Dataset::lookup(v) {
        return match &mut *inner.borrow_mut() {
            Inner::Raster(r) => r.read().map_err(js_err),
            Inner::Vector(..) => Err(invalid("expected a raster, got a vector dataset")),
        };
    }
    if let Some((bytes, name)) = file_bytes(v) {
        return terrio::memory::open_raster_bytes(bytes, &name).map_err(js_err);
    }
    Err(invalid("expected a raster: a Dataset, a Uint8Array or { bytes, filename }"))
}

/// Rasters given to an operation: one, or an array of them.
fn input_rasters(v: &JsValue) -> Result<Vec<RasterDataset>, JsError> {
    let out: Vec<RasterDataset> = if js_sys::Array::is_array(v) {
        js_sys::Array::from(v).iter().map(|i| input_raster(&i)).collect::<Result<_, _>>()?
    } else {
        vec![input_raster(v)?]
    };
    if out.is_empty() {
        return Err(invalid("no input rasters"));
    }
    Ok(out)
}

/// Vector layers given to an operation: a `Dataset`, a file's bytes, or GeoJSON (an object or a
/// string).
pub(crate) fn input_layers(v: &JsValue) -> Result<Vec<Layer>, JsError> {
    if let Some(inner) = Dataset::lookup(v) {
        return match &*inner.borrow() {
            Inner::Vector(layers, _) => Ok(layers.clone()),
            Inner::Raster(_) => Err(invalid("expected a vector dataset, got a raster")),
        };
    }
    if let Some((bytes, name)) = file_bytes(v) {
        return terrio::memory::open_vector_bytes(bytes, &name, &Default::default()).map(|d| d.layers).map_err(js_err);
    }
    if v.is_object() || v.is_string() {
        let ds = crate::from_geojson(v.clone(), None, None)?;
        let inner = ds.inner.borrow();
        if let Inner::Vector(layers, _) = &*inner {
            return Ok(layers.clone());
        }
    }
    Err(invalid("expected vector data: a Dataset, a Uint8Array, { bytes, filename } or GeoJSON"))
}

/// Takes the options that are JavaScript values (datasets) out of an options object, returning
/// them and a copy of the object without them, for the others to be parsed.
fn split_options(options: &JsValue, keys: &[&str]) -> Result<(JsValue, Vec<Option<JsValue>>), JsError> {
    if options.is_undefined() || options.is_null() {
        return Ok((JsValue::UNDEFINED, vec![None; keys.len()]));
    }
    if !options.is_object() {
        return Err(invalid("options must be an object"));
    }
    let copy = js_sys::Object::assign(&js_sys::Object::new(), &js_sys::Object::from(options.clone()));
    let mut taken = Vec::with_capacity(keys.len());
    for k in keys {
        let key = JsValue::from_str(k);
        let v = js_sys::Reflect::get(&copy, &key).unwrap_or(JsValue::UNDEFINED);
        let _ = js_sys::Reflect::delete_property(&copy, &key);
        taken.push((!v.is_undefined() && !v.is_null()).then_some(v));
    }
    Ok((copy.into(), taken))
}

fn srs(s: Option<&str>) -> Result<Option<SpatialRef>, JsError> {
    s.map(SpatialRef::parse).transpose().map_err(js_err)
}

/// A number, a list of numbers, or a word (`"none"`).
#[derive(Deserialize, Clone)]
#[serde(untagged)]
enum NumberOrList {
    Number(f64),
    List(Vec<f64>),
    Word(String),
}

fn nodata_arg(v: Option<NumberOrList>) -> Result<Nodata, JsError> {
    Ok(match v {
        None => Nodata::Auto,
        Some(NumberOrList::Number(x)) => Nodata::Values(vec![x]),
        Some(NumberOrList::List(v)) => Nodata::Values(v),
        Some(NumberOrList::Word(s)) => Nodata::parse(&s).map_err(js_err)?,
    })
}

fn numbers(v: Option<NumberOrList>) -> Result<Vec<f64>, JsError> {
    match v {
        None => Ok(Vec::new()),
        Some(NumberOrList::Number(x)) => Ok(vec![x]),
        Some(NumberOrList::List(v)) => Ok(v),
        Some(NumberOrList::Word(s)) => Err(invalid(format!("expected a number or a list of numbers, got '{s}'"))),
    }
}

fn tuple2(v: Option<[f64; 2]>) -> Option<(f64, f64)> {
    v.map(|[a, b]| (a, b))
}

fn tuple4(v: Option<[f64; 4]>) -> Option<(f64, f64, f64, f64)> {
    v.map(|[a, b, c, d]| (a, b, c, d))
}

/// Options of [`warp`].
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct WarpArgs {
    s_srs: Option<String>,
    t_srs: Option<String>,
    tr: Option<[f64; 2]>,
    te: Option<[f64; 4]>,
    te_srs: Option<String>,
    ts: Option<[usize; 2]>,
    tap: bool,
    resampling: Option<String>,
    src_nodata: Option<NumberOrList>,
    dst_nodata: Option<NumberOrList>,
    ot: Option<String>,
    crop_to_cutline: bool,
    et: Option<f64>,
    wm: Option<f64>,
}

/// Reprojects, resamples, crops or mosaics rasters, as `terrio warp` (GDAL's `gdalwarp`).
///
/// `src`: a raster or an array of them. Options: `sSrs`, `tSrs`, `tr` (`[xres, yres]`), `te`
/// (`[minX, minY, maxX, maxY]`), `teSrs`, `ts` (`[width, height]`), `tap`, `resampling`
/// (`near`, `bilinear`, `cubic`, `cubicspline`, `lanczos`, `average`, `rms`, `mode`, `min`,
/// `max`, `med`, `q1`, `q3`, `sum`), `srcNodata` and `dstNodata` (a number, one per band, or
/// `"none"`), `ot`, `cutline` (polygons: a `Dataset`, bytes or GeoJSON), `cropToCutline`, `et`
/// (the approximate transformer's error in pixels, 0.125 by default) and `wm` (working memory in
/// bytes).
#[wasm_bindgen]
pub fn warp(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let (rest, taken) = split_options(&options, &["cutline"])?;
    let a: WarpArgs = parse_options(&rest)?;
    if a.tap && a.tr.is_none() {
        return Err(invalid("tap needs tr"));
    }
    let cutline = match &taken[0] {
        Some(c) => {
            let ds = terrio::VectorDataset { layers: input_layers(c)?, driver: VectorDriver::GeoJSON };
            Some(Cutline::from_vector(&ds).map_err(js_err)?)
        }
        None => None,
    };
    let mut o = WarpOptions {
        src_srs: srs(a.s_srs.as_deref())?,
        dst_srs: srs(a.t_srs.as_deref())?,
        resolution: tuple2(a.tr),
        extent: tuple4(a.te),
        extent_srs: srs(a.te_srs.as_deref())?,
        size: a.ts.map(|[w, h]| (w, h)),
        target_aligned_pixels: a.tap,
        resampling: Resampling::parse(a.resampling.as_deref().unwrap_or("near")).map_err(js_err)?,
        src_nodata: nodata_arg(a.src_nodata)?,
        dst_nodata: nodata_arg(a.dst_nodata)?,
        output_type: a.ot.as_deref().map(parse_data_type).transpose().map_err(js_err)?,
        cutline,
        crop_to_cutline: a.crop_to_cutline,
        error_threshold: a.et.unwrap_or(0.125),
        ..Default::default()
    };
    if let Some(wm) = a.wm {
        o.memory_limit = wm;
    }
    let sources = input_rasters(&src)?;
    let refs: Vec<&RasterDataset> = sources.iter().collect();
    Ok(Dataset::from_raster(terrio::ops::warp::warp(&refs, &o).map_err(js_err)?))
}

/// An output size: pixels, or a percentage such as `"50%"`.
#[derive(Deserialize)]
#[serde(untagged)]
enum SizeArg {
    Pixels(usize),
    Text(String),
}

impl SizeArg {
    fn size(self) -> Result<Size, JsError> {
        match self {
            SizeArg::Pixels(n) => Ok(Size::Pixels(n)),
            SizeArg::Text(s) => Size::parse(&s).map_err(js_err),
        }
    }
}

/// `true`, or a list of 2 or 4 numbers.
#[derive(Deserialize)]
#[serde(untagged)]
enum ScaleArg {
    Flag(bool),
    Range(Vec<f64>),
}

/// Options of [`translate`].
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct TranslateArgs {
    bands: Option<Vec<usize>>,
    ot: Option<String>,
    srcwin: Option<[f64; 4]>,
    projwin: Option<[f64; 4]>,
    projwin_srs: Option<String>,
    outsize: Option<(SizeArg, SizeArg)>,
    tr: Option<[f64; 2]>,
    resampling: Option<String>,
    scale: Option<ScaleArg>,
    exponent: Option<f64>,
    a_nodata: Option<NumberOrList>,
    a_srs: Option<String>,
    a_ullr: Option<[f64; 4]>,
}

/// Subsets, resizes, rescales or converts a raster, as `terrio translate` (GDAL's
/// `gdal_translate`).
///
/// Options: `bands` (numbered from 1), `ot`, `srcwin` (`[colOff, rowOff, width, height]`),
/// `projwin` (`[ulx, uly, lrx, lry]`), `projwinSrs`, `outsize` (`[width, height]`, pixels or
/// percentages such as `"50%"`; 0 keeps the aspect ratio), `tr`, `resampling`, `scale` (`true`,
/// `[srcMin, srcMax]` or `[srcMin, srcMax, dstMin, dstMax]`), `exponent`, `aNodata` (a number or
/// `"none"`), `aSrs`, `aUllr`.
#[wasm_bindgen]
pub fn translate(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let a: TranslateArgs = parse_options(&options)?;
    let out_size = a.outsize.map(|(w, h)| Ok::<_, JsError>((w.size()?, h.size()?))).transpose()?;
    let scale = match a.scale {
        None => None,
        Some(ScaleArg::Flag(b)) => b.then(Scale::default),
        Some(ScaleArg::Range(v)) => match v[..] {
            [a, b] => Some(Scale { src: Some((a, b)), dst: None }),
            [a, b, c, d] => Some(Scale { src: Some((a, b)), dst: Some((c, d)) }),
            _ => return Err(invalid("scale is true or a list of 2 or 4 numbers")),
        },
    };
    let assign_nodata = match a.a_nodata {
        None => None,
        Some(NumberOrList::Number(x)) => Some(Some(x)),
        Some(NumberOrList::Word(s)) if s.eq_ignore_ascii_case("none") => Some(None),
        Some(_) => return Err(invalid("aNodata is a number or 'none'")),
    };
    let o = TranslateOptions {
        bands: a.bands.unwrap_or_default(),
        output_type: a.ot.as_deref().map(parse_data_type).transpose().map_err(js_err)?,
        src_win: tuple4(a.srcwin),
        proj_win: tuple4(a.projwin),
        proj_win_srs: srs(a.projwin_srs.as_deref())?,
        out_size,
        resolution: tuple2(a.tr),
        resampling: a.resampling.as_deref().map(Resampling::parse).transpose().map_err(js_err)?,
        scale,
        exponent: a.exponent,
        assign_nodata,
        assign_srs: srs(a.a_srs.as_deref())?,
        assign_ullr: tuple4(a.a_ullr),
    };
    let ds = input_raster(&src)?;
    Ok(Dataset::from_raster(terrio::ops::translate::translate(&ds, &o).map_err(js_err)?))
}

/// Options of [`rasterize`].
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct RasterizeArgs {
    layers: Option<Vec<String>>,
    sql: Option<String>,
    burn: Option<NumberOrList>,
    attribute: Option<String>,
    r#where: Option<String>,
    tr: Option<[f64; 2]>,
    ts: Option<[usize; 2]>,
    te: Option<[f64; 4]>,
    tap: bool,
    ot: Option<String>,
    init: Option<NumberOrList>,
    nodata: Option<f64>,
    all_touched: bool,
    add: bool,
    a_srs: Option<String>,
}

/// Burns vector geometries into a raster, as `terrio rasterize` (GDAL's `gdal_rasterize`).
///
/// `src`: vector data (a `Dataset`, bytes or GeoJSON). Options: `burn` (a value, or one per
/// band) or `attribute`, `layers`, `sql`, `where`, `tr`, `ts`, `te`, `tap`, `like` (a raster
/// whose grid to use), `ot`, `init`, `nodata`, `allTouched`, `add`, `aSrs`.
#[wasm_bindgen]
pub fn rasterize(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let (rest, taken) = split_options(&options, &["like"])?;
    let a: RasterizeArgs = parse_options(&rest)?;
    let burn_values = numbers(a.burn)?;
    if burn_values.is_empty() && a.attribute.is_none() {
        return Err(invalid("give burn or attribute"));
    }
    let all = input_layers(&src)?;
    let named = a.layers.is_some();
    let all: Vec<Layer> = match &a.layers {
        Some(names) => all.into_iter().filter(|l| names.contains(&l.name)).collect(),
        None => all,
    };
    let to_burn = match &a.sql {
        Some(sql) => {
            let o = VectorTranslateOptions { sql: Some(sql.clone()), ..Default::default() };
            terrio::ops::vector_filter::translate(&all, &o).map_err(js_err)?
        }
        None if !named && all.len() != 1 => {
            let names: Vec<&str> = all.iter().map(|l| l.name.as_str()).collect();
            return Err(invalid(format!("the dataset has {} layers: choose with layers or sql ({})", all.len(), names.join(", "))));
        }
        None => all,
    };
    let grid = match &taken[0] {
        Some(l) => Some(Grid::like(&input_raster(l)?)),
        None => None,
    };
    let o = RasterizeOptions {
        burn_values,
        attribute: a.attribute,
        where_clause: a.r#where,
        resolution: tuple2(a.tr),
        size: a.ts.map(|[w, h]| (w, h)),
        extent: tuple4(a.te),
        target_aligned: a.tap,
        grid,
        data_type: a.ot.as_deref().map(parse_data_type).transpose().map_err(js_err)?,
        init: numbers(a.init)?,
        nodata: a.nodata,
        all_touched: a.all_touched,
        merge: if a.add { MergeAlg::Add } else { MergeAlg::Replace },
        assign_srs: srs(a.a_srs.as_deref())?,
    };
    Ok(Dataset::from_raster(terrio::ops::rasterize::rasterize(&to_burn, &o).map_err(js_err)?))
}

/// Options of [`polygonize`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct PolygonizeArgs {
    band: usize,
    field: String,
    layer_name: String,
    connectedness: u8,
    nomask: bool,
}

impl Default for PolygonizeArgs {
    fn default() -> Self {
        Self { band: 1, field: "DN".into(), layer_name: "polygonized".into(), connectedness: 4, nomask: false }
    }
}

/// Turns a raster band into polygons of connected pixels of the same value, as
/// `terrio polygonize` (GDAL's `gdal_polygonize`). Returns a vector `Dataset`.
///
/// Options: `band` (1), `field` (`"DN"`), `layerName` (`"polygonized"`), `connectedness` (4 or
/// 8), `nomask` (keep nodata pixels), `mask` (a raster: pixels where its first band is 0 are
/// left out).
#[wasm_bindgen]
pub fn polygonize(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let (rest, taken) = split_options(&options, &["mask"])?;
    let a: PolygonizeArgs = parse_options(&rest)?;
    if a.band == 0 {
        return Err(invalid("bands are numbered from 1"));
    }
    if a.connectedness != 4 && a.connectedness != 8 {
        return Err(invalid("connectedness is 4 or 8"));
    }
    let mask = taken[0].as_ref().map(input_raster).transpose()?;
    let ds = input_raster(&src)?;
    let o = PolygonizeOptions {
        band: a.band - 1,
        field_name: a.field,
        layer_name: a.layer_name,
        connectedness8: a.connectedness == 8,
        ignore_nodata: a.nomask,
        mask,
    };
    let layer = terrio::ops::polygonize::polygonize(&ds, &o).map_err(js_err)?;
    Ok(Dataset::from_layers(vec![layer], VectorDriver::GeoJSON))
}

/// Options of [`dem`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct DemArgs {
    color_table: Option<String>,
    band: usize,
    z: f64,
    scale: Option<f64>,
    xscale: Option<f64>,
    yscale: Option<f64>,
    azimuth: Option<f64>,
    altitude: Option<f64>,
    combined: bool,
    multidirectional: bool,
    igor: bool,
    alg: Option<String>,
    percent: bool,
    trigonometric: bool,
    zero_for_flat: bool,
    compute_edges: bool,
    alpha: bool,
    exact_color_entry: bool,
    nearest_color_entry: bool,
}

impl Default for DemArgs {
    fn default() -> Self {
        Self {
            color_table: None,
            band: 1,
            z: 1.0,
            scale: None,
            xscale: None,
            yscale: None,
            azimuth: None,
            altitude: None,
            combined: false,
            multidirectional: false,
            igor: false,
            alg: None,
            percent: false,
            trigonometric: false,
            zero_for_flat: false,
            compute_edges: false,
            alpha: false,
            exact_color_entry: false,
            nearest_color_entry: false,
        }
    }
}

/// Computes a terrain product of a DEM, as `terrio dem` (GDAL's `gdaldem`).
///
/// `processing`: `hillshade`, `slope`, `aspect`, `color-relief`, `TRI`, `TPI` or `roughness`.
/// Options: `band`, `z`, `scale`, `xscale`, `yscale`, `azimuth`, `altitude`, `combined`,
/// `multidirectional`, `igor`, `alg` (`Horn` or `ZevenbergenThorne`; `Riley` or `Wilson` for
/// TRI), `percent`, `trigonometric`, `zeroForFlat`, `computeEdges`, and for color relief
/// `colorTable` (the text of a color file: lines of `value R G B [A]`), `alpha`,
/// `exactColorEntry`, `nearestColorEntry`.
#[wasm_bindgen]
pub fn dem(src: JsValue, processing: &str, options: JsValue) -> Result<Dataset, JsError> {
    let a: DemArgs = parse_options(&options)?;
    if [a.combined, a.multidirectional, a.igor].iter().filter(|b| **b).count() > 1 {
        return Err(invalid("combined, multidirectional and igor exclude each other"));
    }
    if a.multidirectional && a.azimuth.is_some() {
        return Err(invalid("multidirectional and azimuth exclude each other"));
    }
    if a.igor && a.altitude.is_some() {
        return Err(invalid("igor and altitude exclude each other"));
    }
    if a.exact_color_entry && a.nearest_color_entry {
        return Err(invalid("exactColorEntry and nearestColorEntry exclude each other"));
    }
    let color = processing.eq_ignore_ascii_case("color-relief");
    let product = if color { None } else { Some(DemProduct::parse(processing).map_err(js_err)?) };
    if color != a.color_table.is_some() {
        return Err(invalid("colorTable is needed by color-relief, and only by it"));
    }
    let mut o = DemOptions {
        band: a.band,
        z: a.z,
        scale: a.scale,
        xscale: a.xscale,
        yscale: a.yscale,
        azimuth: a.azimuth.unwrap_or(315.0),
        altitude: a.altitude.unwrap_or(45.0),
        shading: if a.combined {
            Shading::Combined
        } else if a.multidirectional {
            Shading::MultiDirectional
        } else if a.igor {
            Shading::Igor
        } else {
            Shading::Standard
        },
        slope_percent: a.percent,
        trigonometric: a.trigonometric,
        zero_for_flat: a.zero_for_flat,
        compute_edges: a.compute_edges,
        alpha: a.alpha,
        color_selection: if a.exact_color_entry {
            ColorSelection::Exact
        } else if a.nearest_color_entry {
            ColorSelection::Nearest
        } else {
            ColorSelection::Interpolate
        },
        ..Default::default()
    };
    if let Some(alg) = &a.alg {
        match product {
            Some(DemProduct::Tri) => o.tri = TriAlg::parse(alg).map_err(js_err)?,
            Some(DemProduct::Hillshade | DemProduct::Slope | DemProduct::Aspect) => o.gradient = GradientAlg::parse(alg).map_err(js_err)?,
            _ => return Err(invalid("alg applies to hillshade, slope, aspect and TRI")),
        }
    }
    let ds = input_raster(&src)?;
    let out = match (product, a.color_table) {
        (Some(p), _) => terrio::ops::dem::dem(&ds, p, &o),
        (None, Some(text)) => ColorMap::parse(&text, &ds, a.band, None).and_then(|map| terrio::ops::dem::color_relief(&ds, &map, &o)),
        (None, None) => unreachable!("checked"),
    };
    Ok(Dataset::from_raster(out.map_err(js_err)?))
}

/// Options of [`calc`].
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct CalcArgs {
    bands: std::collections::HashMap<String, usize>,
    r#type: Option<String>,
    nodata: Option<NumberOrList>,
    hide_nodata: bool,
    extent: Option<String>,
    projwin: Option<[f64; 4]>,
    all_bands: Option<String>,
    projection_check: bool,
}

/// Evaluates band math over rasters, as `terrio calc` (GDAL's `gdal_calc.py`), with NumPy's
/// typing rules so that results match `gdal_calc.py`.
///
/// `expression`: an expression, or an array of them (one per output band), in NumPy syntax.
/// `inputs`: the rasters by the names the expressions use, `{ A: dem, B: other }`. Options:
/// `bands` (the band of each input, `{ A: 2 }`; 1 by default), `type`, `nodata` (a number or
/// `"none"`), `hideNodata`, `extent` (`ignore`, `fail`, `union`, `intersect`), `projwin`,
/// `allBands` (the input whose every band is processed), `projectionCheck`.
#[wasm_bindgen]
pub fn calc(expression: JsValue, inputs: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let exprs: Vec<String> = match expression.as_string() {
        Some(s) => vec![s],
        None if js_sys::Array::is_array(&expression) => js_sys::Array::from(&expression)
            .iter()
            .map(|e| e.as_string().ok_or_else(|| invalid("expressions are strings")))
            .collect::<Result<_, _>>()?,
        None => return Err(invalid("expression is a string or an array of strings")),
    };
    let a: CalcArgs = parse_options(&options)?;
    if !inputs.is_object() {
        return Err(invalid("inputs is an object of rasters by name, such as { A: dem }"));
    }
    let mut named: Vec<(String, RasterDataset)> = Vec::new();
    for entry in js_sys::Object::entries(&js_sys::Object::from(inputs)).iter() {
        let pair = js_sys::Array::from(&entry);
        let name = pair.get(0).as_string().unwrap_or_default();
        named.push((name, input_raster(&pair.get(1))?));
    }
    if named.is_empty() {
        return Err(invalid("no input rasters"));
    }
    for n in a.bands.keys() {
        if !named.iter().any(|(m, _)| m == n) {
            return Err(invalid(format!("a band is given for {n}, which is not an input")));
        }
    }
    let rank = |n: &str| -> usize {
        match n.as_bytes() {
            [c] if c.is_ascii_lowercase() => (c - b'a') as usize,
            [c] if c.is_ascii_uppercase() => 26 + (c - b'A') as usize,
            _ => 52,
        }
    };
    named.sort_by_key(|(n, _)| rank(n));
    let nodata = match a.nodata {
        None => CalcNodata::Default,
        Some(NumberOrList::Number(x)) => CalcNodata::Value(x),
        Some(NumberOrList::Word(s)) if s.eq_ignore_ascii_case("none") => CalcNodata::None,
        Some(_) => return Err(invalid("nodata is a number or 'none'")),
    };
    let o = CalcOptions {
        calc: exprs,
        output_type: a.r#type.as_deref().map(parse_data_type).transpose().map_err(js_err)?,
        nodata,
        hide_nodata: a.hide_nodata,
        extent: a.extent.as_deref().map(CalcExtent::parse).transpose().map_err(js_err)?.unwrap_or_default(),
        projwin: tuple4(a.projwin),
        all_bands: a.all_bands,
        projection_check: a.projection_check,
    };
    let inputs: Vec<CalcInput> =
        named.iter().map(|(n, ds)| CalcInput { name: n.clone(), ds, band: a.bands.get(n).copied().unwrap_or(1) }).collect();
    Ok(Dataset::from_raster(terrio::ops::calc::calc(&inputs, &o).map_err(js_err)?))
}

/// Options of [`mosaic`].
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct MosaicArgs {
    resolution: Option<String>,
    tr: Option<[f64; 2]>,
    te: Option<[f64; 4]>,
    tap: bool,
    separate: bool,
    bands: Option<Vec<usize>>,
    nodata: Option<NumberOrList>,
    src_nodata: Option<NumberOrList>,
    dst_nodata: Option<NumberOrList>,
    resampling: Option<String>,
    t_srs: Option<String>,
}

/// Mosaics rasters into one, as `terrio mosaic` (GDAL's `gdalbuildvrt` then `gdal_translate`).
///
/// `srcs`: an array of rasters. Options: `resolution` (`highest`, `lowest` or `average`), `tr`,
/// `te`, `tap`, `separate` (each source in its own band), `bands`, `nodata` (both `srcNodata`
/// and `dstNodata`), `srcNodata`, `dstNodata`, `resampling`, `tSrs`.
#[wasm_bindgen]
pub fn mosaic(srcs: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let a: MosaicArgs = parse_options(&options)?;
    let resolution = match a.tr {
        Some([x, y]) => ResolutionStrategy::User(x, y),
        None => ResolutionStrategy::parse(a.resolution.as_deref().unwrap_or("average")).map_err(js_err)?,
    };
    let nodata = a.nodata.map(|n| nodata_arg(Some(n))).transpose()?;
    let pick = |v: Option<NumberOrList>| -> Result<Nodata, JsError> {
        match (v, &nodata) {
            (Some(v), _) => nodata_arg(Some(v)),
            (None, Some(n)) => Ok(n.clone()),
            (None, None) => Ok(Nodata::Auto),
        }
    };
    let o = MosaicOptions {
        resolution,
        extent: tuple4(a.te),
        target_aligned_pixels: a.tap,
        separate: a.separate,
        bands: a.bands.unwrap_or_default(),
        src_nodata: pick(a.src_nodata)?,
        dst_nodata: pick(a.dst_nodata)?,
        resampling: Resampling::parse(a.resampling.as_deref().unwrap_or("near")).map_err(js_err)?,
        dst_srs: srs(a.t_srs.as_deref())?,
    };
    let inputs = input_rasters(&srcs)?;
    // Sources are told apart by name.
    let sources: Vec<MosaicSource> = inputs.iter().enumerate().map(|(i, ds)| MosaicSource { path: format!("<source {i}>"), ds }).collect();
    Ok(Dataset::from_raster(terrio::ops::mosaic::mosaic(&sources, &o).map_err(js_err)?))
}

/// Target values: a list of integers or a `"1,2,3"` string.
#[derive(Deserialize)]
#[serde(untagged)]
enum ValuesArg {
    List(Vec<i32>),
    Text(String),
}

/// Options of [`proximity`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct ProximityArgs {
    band: usize,
    values: Option<ValuesArg>,
    distunits: String,
    maxdist: Option<f64>,
    nodata: Option<f64>,
    use_input_nodata: bool,
    fixed_buf_val: Option<f64>,
    ot: String,
}

impl Default for ProximityArgs {
    fn default() -> Self {
        Self {
            band: 1,
            values: None,
            distunits: "PIXEL".into(),
            maxdist: None,
            nodata: None,
            use_input_nodata: false,
            fixed_buf_val: None,
            ot: "Float32".into(),
        }
    }
}

/// The distance from every pixel to the nearest target pixel, as `terrio proximity` (GDAL's
/// `gdal_proximity.py`).
///
/// Options: `band`, `values` (the target values; every non-zero pixel by default), `distunits`
/// (`PIXEL` or `GEO`), `maxdist`, `nodata`, `useInputNodata`, `fixedBufVal`, `ot` (`Float32`).
#[wasm_bindgen]
pub fn proximity(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let a: ProximityArgs = parse_options(&options)?;
    let values = match a.values {
        None => Vec::new(),
        Some(ValuesArg::List(v)) => v,
        Some(ValuesArg::Text(s)) => parse_values(&s),
    };
    let geo_units = match a.distunits.to_ascii_uppercase().as_str() {
        "PIXEL" => false,
        "GEO" => true,
        _ => return Err(invalid(format!("unrecognized distunits '{}' (PIXEL, GEO)", a.distunits))),
    };
    let o = ProximityOptions {
        band: a.band,
        values,
        geo_units,
        max_distance: a.maxdist,
        nodata: a.nodata,
        use_input_nodata: a.use_input_nodata,
        fixed_value: a.fixed_buf_val,
        output_type: parse_data_type(&a.ot).map_err(js_err)?,
    };
    let ds = input_raster(&src)?;
    Ok(Dataset::from_raster(terrio::ops::proximity::proximity(&ds, &o).map_err(js_err)?))
}

/// Options of [`sieve`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct SieveArgs {
    band: usize,
    threshold: usize,
    connectedness: u8,
}

impl Default for SieveArgs {
    fn default() -> Self {
        Self { band: 1, threshold: 2, connectedness: 4 }
    }
}

/// Merges raster areas smaller than a threshold into their largest neighbour, as
/// `terrio sieve` (GDAL's `gdal_sieve.py`).
///
/// Options: `band`, `threshold` (2), `connectedness` (4 or 8), `mask` (`"default"`: the band's
/// nodata pixels are left out; `"none"`; or a raster whose band 1 is 0 where pixels are left
/// out).
#[wasm_bindgen]
pub fn sieve(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    let (rest, taken) = split_options(&options, &["mask"])?;
    let a: SieveArgs = parse_options(&rest)?;
    if a.connectedness != 4 && a.connectedness != 8 {
        return Err(invalid("connectedness is 4 or 8"));
    }
    let word = taken[0].as_ref().and_then(|m| m.as_string()).map(|s| s.to_ascii_lowercase());
    let mask_ds = match (&taken[0], word.as_deref()) {
        (Some(_), Some("default" | "none")) | (None, _) => None,
        (Some(_), Some(w)) => return Err(invalid(format!("mask is 'default', 'none' or a raster, not '{w}'"))),
        (Some(m), None) => Some(input_raster(m)?),
    };
    let mask = match (&mask_ds, word.as_deref()) {
        (Some(m), _) => SieveMask::Raster(m),
        (None, Some("none")) => SieveMask::None,
        _ => SieveMask::Default,
    };
    let ds = input_raster(&src)?;
    let o = SieveOptions { band: a.band, threshold: a.threshold, eight_connected: a.connectedness == 8, mask };
    Ok(Dataset::from_raster(terrio::ops::sieve::sieve(&ds, &o).map_err(js_err)?))
}

/// Options of [`fill_nodata`].
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct FillNodataArgs {
    band: usize,
    max_distance: f64,
    interpolation: String,
    smoothing_iterations: usize,
}

impl Default for FillNodataArgs {
    fn default() -> Self {
        Self { band: 1, max_distance: 100.0, interpolation: "inv_dist".into(), smoothing_iterations: 0 }
    }
}

/// Fills holes using GDAL's four-quadrant interpolation, returning a single-band raster.
/// Options: `band`, `mask` (a raster with zero at holes), `maxDistance` (pixels,
/// 100 by default, 0 for unlimited), `interpolation` (`inv_dist` or `nearest`),
/// `smoothingIterations`. Without a mask, uses the source's mask or nodata.
#[wasm_bindgen(js_name = fillNodata)]
pub fn fill_nodata(src: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    use terrio::ops::fill_nodata::{FillNodataOptions, Interpolation, raster_mask};
    let (rest, taken) = split_options(&options, &["mask"])?;
    let a: FillNodataArgs = parse_options(&rest)?;
    let interpolation = Interpolation::parse(&a.interpolation).map_err(js_err)?;
    let read = |r: &mut terrio::LazyRaster| -> Result<(RasterDataset, Option<Vec<u8>>), JsError> {
        let window = terrio::Window::new(0, 0, r.width(), r.height());
        let mask = if taken[0].is_none() { r.read_mask(window, None).map_err(js_err)? } else { None };
        Ok((r.read().map_err(js_err)?, mask))
    };
    let (ds, default_mask) = if let Some(inner) = Dataset::lookup(&src) {
        let mut inner = inner.borrow_mut();
        match &mut *inner {
            Inner::Raster(r) => read(r)?,
            Inner::Vector(..) => return Err(invalid("expected a raster, got a vector dataset")),
        }
    } else if let Some((bytes, name)) = file_bytes(&src) {
        read(&mut terrio::memory::open_raster_lazy_bytes(bytes, &name).map_err(js_err)?)?
    } else {
        return Err(invalid("expected a raster: a Dataset, a Uint8Array or { bytes, filename }"));
    };
    let mask = taken[0].as_ref().map(input_raster).transpose()?;
    let bytes = mask.as_ref().map(|m| raster_mask(m, ds.raster.cols, ds.raster.rows)).transpose().map_err(js_err)?.or(default_mask);
    let o = FillNodataOptions {
        band: a.band,
        mask: bytes.as_deref(),
        max_distance: a.max_distance,
        interpolation,
        smoothing_iterations: a.smoothing_iterations,
    };
    Ok(Dataset::from_raster(terrio::ops::fill_nodata::fill_nodata(&ds, &o).map_err(js_err)?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", default, deny_unknown_fields)]
struct PansharpenArgs {
    weights: Vec<f64>,
    bands: Vec<usize>,
    pan_band: usize,
    resampling: String,
    bitdepth: Option<u8>,
    nodata: Option<serde_json::Value>,
    spat_adjust: String,
    ot: Option<String>,
}
impl Default for PansharpenArgs {
    fn default() -> Self {
        Self {
            weights: vec![],
            bands: vec![],
            pan_band: 1,
            resampling: "cubic".into(),
            bitdepth: None,
            nodata: None,
            spat_adjust: "union".into(),
            ot: None,
        }
    }
}

/// Weighted Brovey pansharpening. `spectral` is a raster/bytes or an array of them;
/// all bands contribute intensity. Options: `weights`, output `bands` (from 1),
/// `panBand`, `resampling` (default cubic), `bitdepth`, `nodata` (number or "none"),
/// `spatAdjust` (union/intersection/none), `ot` (output type).
#[wasm_bindgen]
pub fn pansharpen(pan: JsValue, spectral: JsValue, options: JsValue) -> Result<Dataset, JsError> {
    use terrio::ops::pansharpen::{PansharpenExtent, PansharpenNodata, PansharpenOptions, SpectralBand};
    let a: PansharpenArgs = parse_options(&options)?;
    let pan = input_raster(&pan)?;
    let datasets = input_rasters(&spectral)?;
    let inputs =
        datasets.iter().flat_map(|ds| (1..=ds.raster.bands).map(move |band| SpectralBand { dataset: ds, band })).collect::<Vec<_>>();
    let nodata = match a.nodata {
        None | Some(serde_json::Value::Null) => PansharpenNodata::Auto,
        Some(serde_json::Value::String(s)) if s.eq_ignore_ascii_case("none") => PansharpenNodata::None,
        Some(serde_json::Value::Number(n)) => PansharpenNodata::Value(n.as_f64().ok_or_else(|| invalid("nodata must be a finite number"))?),
        _ => return Err(invalid("nodata must be a number or 'none'")),
    };
    let o = PansharpenOptions {
        pan_band: a.pan_band,
        weights: a.weights,
        output_bands: a.bands,
        resampling: Resampling::parse(&a.resampling).map_err(js_err)?,
        bit_depth: a.bitdepth,
        nodata,
        extent: PansharpenExtent::parse(&a.spat_adjust).map_err(js_err)?,
        output_type: a.ot.as_deref().map(parse_data_type).transpose().map_err(js_err)?,
    };
    Ok(Dataset::from_raster(terrio::ops::pansharpen::pansharpen(&pan, &inputs, &o).map_err(js_err)?))
}

// SPDX-FileCopyrightText: Copyright (c) 2020, Even Rouault <even dot rouault at spatialys.com>
// SPDX-FileCopyrightText: Terrio contributors
// SPDX-License-Identifier: MIT
//
// Derived from (translated to Rust and modified):
//   GDAL v3.13.3:
//     frmts/stacta/stactadataset.cpp

//! STAC items with tiled assets (STACTA), read as GDAL's STACTA driver reads them.
//!
//! An item of the [tiled-assets extension](https://github.com/stac-extensions/tiled-assets)
//! describes a pyramid of raster tiles ("metatiles"): its `tiles:tile_matrix_sets` (OGC tile
//! matrix sets, version 1 or 2), the `tiles:tile_matrix_links` limits of the tiles that exist in
//! each tile matrix, and `asset_templates` whose `href` names the tile files
//! (`./{TileMatrixSet}/{TileMatrix}/{TileRow}/{TileCol}.tif`, resolved against the item's
//! location). As in GDAL:
//!
//! - The last tile matrix (the most detailed) is the raster, over the tiles its limits allow (the
//!   whole matrix without limits); the others are its overviews, in reverse order, cut to the
//!   raster's extent as `gdal_translate -projwin` cuts them when their extent differs.
//! - The band count, data types and nodata values are those of the asset template's
//!   `raster:bands` (or STAC 1.1 `bands`) when it has them, else those of a first tile (the first
//!   tile of the first tile matrix that opens).
//! - A tile that cannot be opened is an error, unless `GDAL_STACTA_SKIP_MISSING_METATILE` is
//!   `YES`: its pixels are then the nodata value (0 without one).
//! - The item's properties are the dataset's metadata, but for the `tiles:` and `proj:` ones,
//!   written as GDAL writes them.
//! - An item with several asset templates or tile matrix sets lists them as subdatasets,
//!   `STACTA:"item.json":asset` or `STACTA:"item.json":asset:tile_matrix_set`, as GDAL lists
//!   them. terrio opens the first of them and lists them all, as it does for other files of
//!   several rasters.
//!
//! terrio keeps one data type and nodata value per raster: bands of different `raster:bands`
//! data types are read as `Float64`, and the first band's nodata value is the raster's. Tiles
//! are assembled through 64-bit floats, so `Int64` and `UInt64` values beyond 2^53 lose
//! precision. A remote item does not name local files.
//!
//! Names: a local `.json` item, the URL of one, or `STACTA:"item.json"`,
//! `STACTA:"item.json":asset` and `STACTA:"item.json":asset:tile_matrix_set` (local or remote).

use crate::cog::{LazyRaster, Window, WindowOptions, WindowSource};
use crate::crs::SpatialRef;
use crate::formats::RasterDriver;
use crate::info::{Info, InfoOptions};
use crate::{Error, RasterDataset, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use wbraster::DataType;
use wbraster::raster::RasterData;

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("STACTA: {msg}"))
}

/// Whether a STAC document is an item with tiled assets: asset templates and tile matrix sets.
pub(crate) fn is_stacta(doc: &Value) -> bool {
    doc["asset_templates"].is_object() && doc["properties"]["tiles:tile_matrix_sets"].is_object()
}

/// A `STACTA:` name's item, and the asset and tile matrix set it asks for.
type Name = (String, Option<String>, Option<String>);

/// A `STACTA:"item":asset:tms` name: the item, and the asset and tile matrix set asked for.
/// `Err` for a `STACTA:` name of more parts, which GDAL does not open.
fn parse_name(path: &str) -> Option<Result<Name>> {
    let rest = path.trim().strip_prefix("STACTA:")?;
    let (item, tail) = match rest.strip_prefix('"') {
        Some(q) => match q.find('"') {
            Some(end) => (q[..end].to_string(), q[end + 1..].to_string()),
            None => return Some(Err(bad(format!("unterminated quote in '{path}'")))),
        },
        // Unquoted: a URL whole, else the item up to the first colon.
        None if rest.contains("://") => (rest.to_string(), String::new()),
        None => match rest.split_once(':') {
            Some((item, tail)) => (item.to_string(), format!(":{tail}")),
            None => (rest.to_string(), String::new()),
        },
    };
    let parts: Vec<String> = match tail.strip_prefix(':') {
        Some(t) => t.split(':').map(str::to_string).collect(),
        None if tail.is_empty() => Vec::new(),
        None => return Some(Err(bad(format!("invalid name '{path}'")))),
    };
    if parts.len() > 2 {
        return Some(Err(bad(format!("invalid name '{path}': STACTA:\"item.json\":asset:tile_matrix_set at most"))));
    }
    let mut parts = parts.into_iter().filter(|s| !s.is_empty());
    Some(Ok((item, parts.next(), parts.next())))
}

/// Whether a local file is a STACTA item, as GDAL identifies one: a `.json` file whose first
/// bytes name the tiled-assets extension among its `stac_extensions`.
fn sniff_local(path: &str) -> bool {
    let base = path.split(['?', '#']).next().unwrap_or(path).to_ascii_lowercase();
    if !base.ends_with(".json") {
        return false;
    }
    let crate::vfs::Location::Local(p) = crate::vfs::Location::parse(path) else { return false };
    let mut head = Vec::with_capacity(32768);
    let Ok(f) = std::fs::File::open(p) else { return false };
    if std::io::Read::read_to_end(&mut std::io::Read::take(f, 32768), &mut head).is_err() {
        return false;
    }
    let head = String::from_utf8_lossy(&head);
    head.contains("\"stac_extensions\"")
        && (head.contains("\"tiled-assets\"")
            || head.contains("https:\\/\\/stac-extensions.github.io\\/tiled-assets\\/")
            || head.contains("https://stac-extensions.github.io/tiled-assets/"))
}

/// A STACTA item: its document, its properties as GDAL's metadata items, and its location.
pub(crate) struct Item {
    doc: Value,
    metadata: Vec<(String, String)>,
    loc: String,
}

impl Item {
    /// Parses an item's bytes. `None` when they are not JSON.
    pub(crate) fn parse(bytes: &[u8], loc: &str) -> Option<Self> {
        let doc: Value = serde_json::from_slice(bytes).ok()?;
        let text = std::str::from_utf8(bytes).ok()?;
        Some(Self { metadata: properties_metadata(text).unwrap_or_default(), doc, loc: loc.to_string() })
    }

    fn load(loc: &str) -> Result<Self> {
        let bytes = crate::stac::fetch(loc)?;
        Self::parse(&bytes, loc).ok_or_else(|| bad(format!("'{loc}' is not JSON")))
    }

    /// Whether the item is one of tiled assets.
    pub(crate) fn is_stacta(&self) -> bool {
        is_stacta(&self.doc)
    }
}

/// What an item opens as: a raster, or the subdatasets of an item of several.
enum Opened {
    Raster(Box<LazyRaster>),
    Container(Vec<(String, String)>),
}

/// An item, and the asset and tile matrix set asked for.
type Named = (Item, Option<String>, Option<String>);

/// The item a name or path is, and the asset and tile matrix set it asks for. `None` when the
/// path is neither a `STACTA:` name nor a local STACTA item.
fn named_or_local(path: &str) -> Option<Result<Named>> {
    if let Some(name) = parse_name(path) {
        return Some(name.and_then(|(item, asset, tms)| Ok((Item::load(&item)?, asset, tms))));
    }
    if !sniff_local(path) {
        return None;
    }
    match Item::load(path) {
        Ok(item) if item.is_stacta() => Some(Ok((item, None, None))),
        // Not tiled assets after all: a STAC item or GeoJSON, opened as such.
        Ok(_) => None,
        Err(e) => Some(Err(e)),
    }
}

/// Opens a `STACTA:` name or a local STACTA item lazily (an item of several subdatasets as its
/// first). `None` when `path` is neither.
pub(crate) fn open_named_or_local(path: &str) -> Option<Result<LazyRaster>> {
    Some(named_or_local(path)?.and_then(|(item, asset, tms)| open_first(item, asset.as_deref(), tms.as_deref())))
}

/// Opens a STACTA dataset lazily: a `STACTA:` name, a local item, or a remote `.json` one.
/// `None` when `path` is none of these.
///
/// A remote `.json` document is fetched to see whether it is an item of tiled assets (only on
/// this lazy path: `open` and `info` route remote items through the STAC module's single
/// fetch); one that cannot be fetched is that error, and one that is not JSON or not tiled
/// assets is left to the other drivers.
pub(crate) fn open_lazy(path: &str) -> Option<Result<LazyRaster>> {
    if let Some(r) = open_named_or_local(path) {
        return Some(r);
    }
    let base = path.split(['?', '#']).next().unwrap_or(path).to_ascii_lowercase();
    let loc = crate::vfs::Location::parse(path);
    if !base.ends_with(".json") || loc.is_local() || matches!(loc, crate::vfs::Location::Zip { .. }) {
        return None;
    }
    let bytes = match crate::stac::fetch(path) {
        Ok(b) => b,
        Err(e) => return Some(Err(e)),
    };
    let item = Item::parse(&bytes, path)?;
    item.is_stacta().then(|| open_first(item, None, None))
}

/// Opens an item, an item of several subdatasets as its first (listing them all).
pub(crate) fn open_first(item: Item, asset: Option<&str>, tms: Option<&str>) -> Result<LazyRaster> {
    match open_item(&item, asset, tms, Vec::new())? {
        Opened::Raster(r) => Ok(*r),
        Opened::Container(subs) => open_container_first(&item, subs),
    }
}

fn subdataset_metadata(subs: &[(String, String)]) -> Vec<(String, String)> {
    let mut md = Vec::new();
    for (i, (name, desc)) in subs.iter().enumerate() {
        md.push((format!("SUBDATASET_{}_NAME", i + 1), name.clone()));
        md.push((format!("SUBDATASET_{}_DESC", i + 1), desc.clone()));
    }
    md
}

fn open_container_first(item: &Item, subs: Vec<(String, String)>) -> Result<LazyRaster> {
    let (asset, tms) = first_choice(&item.doc);
    match open_item(item, Some(&asset), Some(&tms), subdataset_metadata(&subs))? {
        Opened::Raster(r) => Ok(*r),
        Opened::Container(_) => Err(bad("no raster")),
    }
}

/// The first asset template and tile matrix set of an item.
fn first_choice(doc: &Value) -> (String, String) {
    let first = |v: &Value| v.as_object().and_then(|o| o.keys().next().cloned()).unwrap_or_default();
    (first(&doc["asset_templates"]), first(&doc["properties"]["tiles:tile_matrix_sets"]))
}

/// Summarizes a `STACTA:` name or local STACTA item. `None` when `path` is neither.
pub(crate) fn info(path: &str, options: &InfoOptions) -> Option<Result<Info>> {
    Some(named_or_local(path)?.and_then(|(item, asset, tms)| info_item(item, asset.as_deref(), tms.as_deref(), options)))
}

/// Summarizes an item: its raster, or for an item of several subdatasets, its first (listing
/// them all), or only the list when the first cannot be opened.
pub(crate) fn info_item(item: Item, asset: Option<&str>, tms: Option<&str>, options: &InfoOptions) -> Result<Info> {
    let mut r = match open_item(&item, asset, tms, Vec::new())? {
        Opened::Raster(r) => *r,
        Opened::Container(subs) => match open_container_first(&item, subs.clone()) {
            Ok(r) => r,
            Err(_) => return Ok(Info::Raster(Box::new(container_info(subs)))),
        },
    };
    Ok(Info::Raster(Box::new(crate::info::lazy_raster_info(&mut r, options)?)))
}

/// The summary of an item of several subdatasets that lists them only, as `gdalinfo` shows it.
fn container_info(subs: Vec<(String, String)>) -> crate::info::RasterInfo {
    crate::info::RasterInfo {
        driver: RasterDriver::STACTA.name().to_string(),
        size: [0, 0],
        band_count: 0,
        geo_transform: [0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
        crs: crate::info::CrsSummary::new(&SpatialRef::default()),
        extent: [0.0; 4],
        wgs84_extent: None,
        bands: Vec::new(),
        overviews: Vec::new(),
        metadata: Vec::new(),
        subdatasets: subs.into_iter().map(|(name, description)| crate::info::Subdataset { name, description }).collect(),
        geolocation: Vec::new(),
    }
}

/// A tile matrix of a tile matrix set, as GDAL parses it.
struct TileMatrix {
    id: String,
    res: f64,
    top_left: (f64, f64),
    tile_w: usize,
    tile_h: usize,
    matrix_w: i64,
    matrix_h: i64,
}

/// The CRS (as given) and tile matrices of a tile matrix set (OGC TMS 1.0 or 2.0), as GDAL's
/// `TileMatrixSet::parse` reads them: resolutions from the scale denominators, with the OGC
/// standard pixel of 0.28 mm, in the CRS's units.
fn parse_tms(set: &Value) -> Result<(String, Vec<TileMatrix>)> {
    let v2 = set.get("crs").is_some() && set.get("tileMatrices").is_some();
    if !v2 && set["type"] != "TileMatrixSetType" && set.get("tileMatrix").is_none() {
        return Err(bad("Expected type = TileMatrixSetType"));
    }
    let crs_of = |j: &Value| -> String {
        match j {
            Value::String(s) => s.clone(),
            Value::Object(o) => match (o.get("uri").and_then(Value::as_str), o.get("wkt")) {
                (Some(u), _) if !u.is_empty() => u.to_string(),
                (_, Some(Value::String(w))) => w.clone(),
                (_, Some(w @ Value::Object(_))) => w.to_string(),
                _ => String::new(),
            },
            _ => String::new(),
        }
    };
    let crs_text = crs_of(&set[if v2 { "crs" } else { "supportedCRS" }]);
    let crs = SpatialRef::parse(&crs_text).map_err(|_| bad(format!("Cannot parse CRS {crs_text}")))?;
    let metres_per_unit = crs.metres_per_unit();
    let mut out = Vec::new();
    let mut last_scale = f64::MAX;
    for m in set[if v2 { "tileMatrices" } else { "tileMatrix" }].as_array().into_iter().flatten() {
        let id = json_text(&m[if v2 { "id" } else { "identifier" }]);
        let scale = m["scaleDenominator"].as_f64().unwrap_or(0.0);
        if scale >= last_scale || scale <= 0.0 {
            return Err(bad("Invalid scale denominator or non-decreasing series of scale denominators"));
        }
        last_scale = scale;
        let corner = &m[if v2 { "pointOfOrigin" } else { "topLeftCorner" }];
        let top_left = match corner.as_array() {
            Some(a) if a.len() == 2 => (a[0].as_f64().unwrap_or(f64::NAN), a[1].as_f64().unwrap_or(f64::NAN)),
            _ => (0.0, 0.0),
        };
        let int = |k: &str| m[k].as_i64().unwrap_or(0);
        let (tw, th, mw, mh) = (int("tileWidth"), int("tileHeight"), int("matrixWidth"), int("matrixHeight"));
        for (name, v) in [("tileWidth", tw), ("tileHeight", th), ("matrixWidth", mw), ("matrixHeight", mh)] {
            if v <= 0 || v > i32::MAX as i64 {
                return Err(bad(format!("Invalid {name}: {v}")));
            }
        }
        if tw > i32::MAX as i64 / th {
            return Err(bad(format!("tileWidth({tw}) x tileHeight({th}) larger than INT_MAX")));
        }
        out.push(TileMatrix {
            id,
            res: scale * 0.28e-3 / metres_per_unit,
            top_left,
            tile_w: tw as usize,
            tile_h: th as usize,
            matrix_w: mw,
            matrix_h: mh,
        });
    }
    if out.is_empty() {
        return Err(bad("No tileMatrix defined"));
    }
    Ok((crs_text, out))
}

/// The value of a JSON scalar as text.
fn json_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

/// The first and last tile columns and rows of a tile matrix the item limits.
#[derive(Clone, Copy)]
struct Limits {
    min_col: i64,
    max_col: i64,
    min_row: i64,
    max_row: i64,
}

/// A tile matrix as a level of the raster: its tiles, and the window of them the level shows.
#[derive(Debug, Clone)]
struct Level {
    /// The tiles' hrefs, with `{TileRow}` and `{TileCol}` to replace.
    template: String,
    tile_w: usize,
    tile_h: usize,
    /// The tile matrix's first tile column and row the level's tiles start at.
    min_col: i64,
    min_row: i64,
    /// The size of the level's tiles together, in pixels.
    raw_w: usize,
    raw_h: usize,
    /// The level's window of the tiles: its top left corner (which may be outside them) and size.
    off_x: i64,
    off_y: i64,
    width: usize,
    height: usize,
}

/// A STACTA raster read by window: the tiles a window touches are opened and read.
struct Tiled {
    levels: Vec<Level>,
    data_type: DataType,
    /// The value of pixels outside the tiles and of missing ones: the nodata value, else 0.
    fill: f64,
    skip_missing: bool,
    block: (usize, usize),
    /// Tiles opened (`None` for missing ones), by href, reused by later windows.
    tiles: Mutex<HashMap<String, Option<LazyRaster>>>,
}

/// The data type of a `raster:bands` `data_type`.
fn stac_data_type(name: &str) -> Option<Result<DataType>> {
    Some(Ok(match name {
        "int8" => DataType::I8,
        "int16" => DataType::I16,
        "int32" => DataType::I32,
        "int64" => DataType::I64,
        "uint8" => DataType::U8,
        "uint16" => DataType::U16,
        "uint32" => DataType::U32,
        "uint64" => DataType::U64,
        "float32" => DataType::F32,
        "float64" => DataType::F64,
        "cint16" | "cint32" | "cfloat32" | "cfloat64" => {
            return Some(Err(Error::Unsupported(format!("STACTA: complex bands ({name})"))));
        }
        _ => return None,
    }))
}

/// Whether a location is on the local file system (a local zip archive's members included).
fn local_backed(loc: &crate::vfs::Location) -> bool {
    match loc {
        crate::vfs::Location::Local(_) => true,
        crate::vfs::Location::Zip { archive, .. } => local_backed(archive),
        _ => false,
    }
}

/// The resolved href template of an asset template, with `{TileMatrixSet}` replaced. An item
/// that is not on the local file system does not name local files (`file://`, local paths,
/// `/vsizip/` of local archives): an error.
fn href_template(item_loc: &str, href: &str, tms: &str) -> Result<String> {
    let href = href.replace("{TileMatrixSet}", tms);
    let resolved = match href.strip_prefix("file://") {
        Some(rest) => rest.to_string(),
        None => {
            let href = match href.strip_prefix("azure://") {
                Some(rest) => format!("az://{rest}"),
                None => href.clone(),
            };
            crate::stac::resolve_href(item_loc, href.strip_prefix("./").unwrap_or(&href))
        }
    };
    let parse = crate::vfs::Location::parse;
    if !local_backed(&parse(item_loc)) && local_backed(&parse(&resolved)) {
        return Err(bad(format!("the remote item '{item_loc}' names local files ({href})")));
    }
    Ok(resolved)
}

/// Whether a configuration option is true, as GDAL's `CPLTestBool`.
fn config_true(key: &str) -> bool {
    std::env::var(key).is_ok_and(|v| !matches!(v.trim().to_ascii_uppercase().as_str(), "NO" | "FALSE" | "OFF" | "0"))
}

/// Opens an asset of an item at a tile matrix set (by default, the first of each), or lists the
/// subdatasets of an item of several when neither is asked for. `extra` is metadata to add.
fn open_item(item: &Item, asset: Option<&str>, tms: Option<&str>, extra: Vec<(String, String)>) -> Result<Opened> {
    let doc = &item.doc;
    let props = doc["properties"].as_object().ok_or_else(|| bad("Missing properties"))?;
    let templates = doc["asset_templates"].as_object().ok_or_else(|| bad("Missing asset_templates"))?;
    if templates.is_empty() {
        return Err(bad("Empty asset_templates"));
    }
    let sets = props
        .get("tiles:tile_matrix_sets")
        .and_then(Value::as_object)
        .ok_or_else(|| bad("Missing properties[\"tiles:tile_matrix_sets\"]"))?;
    if sets.is_empty() {
        return Err(bad("Empty properties[\"tiles:tile_matrix_sets\"]"));
    }
    if (templates.len() >= 2 || sets.len() >= 2) && asset.is_none() && tms.is_none() {
        let mut subs = Vec::new();
        for a in templates.keys() {
            if sets.len() >= 2 {
                for t in sets.keys() {
                    subs.push((format!("STACTA:\"{}\":{a}:{t}", item.loc), format!("Asset {a}, tile matrix set {t}")));
                }
            } else {
                subs.push((format!("STACTA:\"{}\":{a}", item.loc), format!("Asset {a}")));
            }
        }
        return Ok(Opened::Container(subs));
    }
    let asset = asset.map(str::to_string).unwrap_or_else(|| templates.keys().next().cloned().unwrap_or_default());
    let template =
        templates.get(&asset).filter(|t| t.is_object()).ok_or_else(|| bad(format!("Cannot find asset_templates[\"{asset}\"]")))?;
    let tms = tms.map(str::to_string).unwrap_or_else(|| sets.keys().next().cloned().unwrap_or_default());
    let set = sets
        .get(&tms)
        .filter(|s| s.is_object())
        .ok_or_else(|| bad(format!("Cannot find properties[\"tiles:tile_matrix_sets\"][\"{tms}\"]")))?;
    let (crs_text, matrices) = parse_tms(set)?;
    let href = template["href"].as_str().unwrap_or_default();
    if href.is_empty() {
        return Err(bad(format!("Cannot find asset_templates[\"{asset}\"][\"href\"]")));
    }
    let url_template = href_template(&item.loc, href, &tms)?;

    // The limits of the tile matrices, by identifier.
    let mut limits: HashMap<String, Limits> = HashMap::new();
    if let Some(links) = props.get("tiles:tile_matrix_links") {
        if !links.is_object() {
            return Err(bad("Invalid type for properties[\"tiles:tile_matrix_links\"]"));
        }
        if let Some(l) = links[tms.as_str()]["limits"].as_object() {
            for (id, lim) in l {
                let g = |k: &str| lim[k].as_i64().unwrap_or(0);
                limits.insert(
                    id.clone(),
                    Limits {
                        min_col: g("min_tile_col"),
                        max_col: g("max_tile_col"),
                        min_row: g("min_tile_row"),
                        max_row: g("max_tile_row"),
                    },
                );
            }
        }
    }
    let skip_missing = config_true("GDAL_STACTA_SKIP_MISSING_METATILE");

    // Bands from the asset template's raster:bands (STAC 1.1: bands), when they agree with its
    // eo:bands.
    let bands_11 = template["bands"].as_array();
    let eo_bands = bands_11.or_else(|| template["eo:bands"].as_array());
    let raster_bands = bands_11.or_else(|| template["raster:bands"].as_array());
    let mut types: Vec<DataType> = Vec::new();
    let mut nodatas: Vec<Option<f64>> = Vec::new();
    if let Some(rb) = raster_bands
        && eo_bands.is_none_or(|eo| eo.len() == rb.len())
    {
        for (i, b) in rb.iter().enumerate() {
            if !b.is_object() {
                return Err(bad(format!("Wrong raster:bands[{i}]")));
            }
            let name = b["data_type"].as_str().unwrap_or_default();
            types.push(stac_data_type(name).ok_or_else(|| bad(format!("Wrong raster:bands[{i}].data_type = {name}")))??);
            nodatas.push(match &b["nodata"] {
                Value::String(s) if s == "inf" => Some(f64::INFINITY),
                Value::String(s) if s == "-inf" => Some(f64::NEG_INFINITY),
                Value::String(s) if s == "nan" => Some(f64::NAN),
                Value::Number(n) => n.as_f64(),
                _ => None,
            });
        }
    }

    let tile_href = |template: &str, matrix: &str, row: i64, col: i64| {
        template.replace("{TileMatrix}", matrix).replace("{TileRow}", &row.to_string()).replace("{TileCol}", &col.to_string())
    };
    // Without raster:bands, a first tile tells the band count, data type and nodata value: the
    // first tile of the first tile matrix that opens.
    let mut cache: HashMap<String, Option<LazyRaster>> = HashMap::new();
    let mut proto_block = None;
    let (band_count, data_type, nodata) = if types.is_empty() {
        let mut proto = None;
        for m in &matrices {
            let (col, row) = limits.get(&m.id).map_or((0, 0), |l| (l.min_col, l.min_row));
            let url = tile_href(&url_template, &m.id, row, col);
            match LazyRaster::open(&url) {
                Ok(r) => {
                    proto = Some((url, r));
                    break;
                }
                Err(e) if !skip_missing => return Err(bad(format!("Cannot open {url}: {e}"))),
                Err(_) => {}
            }
        }
        let (url, r) = proto.ok_or_else(|| bad("Cannot find prototype dataset"))?;
        let found = (r.band_count(), r.data_type(), r.nodata());
        proto_block = r.block_size();
        cache.insert(url, Some(r));
        found
    } else {
        let dt = if types.iter().all(|t| *t == types[0]) { types[0] } else { DataType::F64 };
        (types.len(), dt, nodatas[0])
    };
    let fill = nodata.unwrap_or(0.0);

    // The levels, from the most detailed (the last tile matrix): the first valid one is the
    // raster, the others its overviews, cut to its extent when theirs differs.
    let mut levels: Vec<Level> = Vec::new();
    let mut extent = [0.0f64; 4]; // min x, min y, max x, max y
    let mut res0 = 0.0;
    for m in matrices.iter().rev() {
        let lim = limits.get(&m.id);
        let (min_col, min_row) = lim.map_or((0, 0), |l| (l.min_col, l.min_row));
        // Limits outside GDAL's 32-bit integers are as invalid as empty ones: the level is skipped.
        let int32 = |v: i64| (0..=i32::MAX as i64).contains(&v);
        if lim.is_some_and(|l| ![l.min_col, l.max_col, l.min_row, l.max_row].into_iter().all(int32)) {
            continue;
        }
        let (mw, mh) = lim.map_or((m.matrix_w, m.matrix_h), |l| (l.max_col - l.min_col + 1, l.max_row - l.min_row + 1));
        if mw <= 0 || mh <= 0 || m.tile_w as i64 > i32::MAX as i64 / mw || m.tile_h as i64 > i32::MAX as i64 / mh {
            continue;
        }
        let (raw_w, raw_h) = ((mw as usize) * m.tile_w, (mh as usize) * m.tile_h);
        let x0 = m.top_left.0 + (min_col * m.tile_w as i64) as f64 * m.res;
        let y0 = m.top_left.1 - (min_row * m.tile_h as i64) as f64 * m.res;
        let mut level = Level {
            template: url_template.replace("{TileMatrix}", &m.id),
            tile_w: m.tile_w,
            tile_h: m.tile_h,
            min_col,
            min_row,
            raw_w,
            raw_h,
            off_x: 0,
            off_y: 0,
            width: raw_w,
            height: raw_h,
        };
        if levels.is_empty() {
            extent = [x0, y0 - raw_h as f64 * m.res, x0 + raw_w as f64 * m.res, y0];
            res0 = m.res;
            levels.push(level);
            continue;
        }
        let ovr = [x0, y0 - raw_h as f64 * m.res, x0 + raw_w as f64 * m.res, y0];
        let same = (0..4).all(|i| (extent[i] - ovr[i]).abs() < 1e-10 * extent[i].abs());
        if !same {
            // As `gdal_translate -projwin` (nearest): the window snapped to the level's pixels.
            let xoff = ((extent[0] - x0) / m.res + 0.001).floor();
            let yoff = ((extent[3] - y0) / -m.res + 0.001).floor();
            let (ulx, uly) = (x0 + xoff * m.res, y0 - yoff * m.res);
            let xsize = ((extent[2] - ulx) / m.res - 0.001).ceil();
            let ysize = ((extent[1] - uly) / -m.res - 0.001).ceil();
            if !(xsize > 0.0 && ysize > 0.0) {
                continue;
            }
            (level.off_x, level.off_y, level.width, level.height) = (xoff as i64, yoff as i64, xsize as usize, ysize as usize);
        }
        levels.push(level);
    }
    let first = levels.first().ok_or_else(|| bad("Cannot find valid tile matrix"))?;
    let crs = SpatialRef::parse(&crs_text).map_err(|_| bad(format!("Cannot parse CRS {crs_text}")))?;
    // The block size: the first tile's when it divides the tiles, else 256 x 256.
    let block = match proto_block {
        Some((bw, bh)) if bw > 0 && bh > 0 && first.tile_w % bw == 0 && first.tile_h % bh == 0 => (bw, bh),
        _ => (256, 256),
    };
    let mut metadata = item.metadata.clone();
    metadata.extend(extra);
    let raster = wbraster::Raster {
        cols: first.width,
        rows: first.height,
        bands: band_count,
        x_min: extent[0],
        y_min: extent[3] - first.height as f64 * res0,
        cell_size_x: res0,
        cell_size_y: res0,
        nodata: nodata.unwrap_or(-9999.0),
        data_type,
        crs: crs.to_crs_info(),
        metadata,
        data: RasterData::new_filled(data_type, 0, 0.0),
    };
    let header = RasterDataset::with_driver(raster, RasterDriver::STACTA, nodata.is_some());
    let source = Tiled { levels, data_type, fill, skip_missing, block, tiles: Mutex::new(cache) };
    Ok(Opened::Raster(Box::new(LazyRaster::from_source(header, Box::new(source)))))
}

impl Tiled {
    /// Reads a window of a tile, or `None` when it is missing and missing tiles are skipped.
    fn read_tile(&self, href: &str, w: Window, bands: &[usize]) -> Result<Option<Vec<f64>>> {
        let cached = self.tiles.lock().ok().and_then(|mut m| m.remove(href));
        let tile = match cached {
            Some(t) => t,
            None => match LazyRaster::open(href) {
                Ok(r) => Some(r),
                Err(_) if self.skip_missing => None,
                Err(e) => {
                    return Err(bad(format!(
                        "Cannot open {href}: {e} (GDAL_STACTA_SKIP_MISSING_METATILE=YES reads missing tiles as nodata)"
                    )));
                }
            },
        };
        let out = match tile {
            Some(mut r) => {
                let data = r.read_window(w, &WindowOptions { bands: bands.to_vec(), overview: None });
                self.keep(href, Some(r));
                Some(data?.raster.data.to_f64_vec())
            }
            None => {
                self.keep(href, None);
                None
            }
        };
        Ok(out)
    }

    fn keep(&self, href: &str, tile: Option<LazyRaster>) {
        if let Ok(mut m) = self.tiles.lock() {
            // A few hundred tiles at most stay open.
            if m.len() >= 256 {
                m.clear();
            }
            m.insert(href.to_string(), tile);
        }
    }
}

impl WindowSource for Tiled {
    fn overviews(&self) -> Vec<(usize, usize)> {
        self.levels[1..].iter().map(|l| (l.width, l.height)).collect()
    }

    fn block_size(&self) -> Option<(usize, usize)> {
        Some(self.block)
    }

    fn read(&mut self, level: usize, w: Window, bands: &[usize]) -> Result<RasterData> {
        let l = self.levels.get(level).ok_or_else(|| bad(format!("no overview {}", level.saturating_sub(1))))?;
        if !w.fits(l.width, l.height) {
            return Err(bad(format!("window outside the {}x{} level", l.width, l.height)));
        }
        let n = w.width * w.height;
        let mut out = vec![self.fill; n * bands.len()];
        // The window in the tiles' pixels, and its part over them.
        let (ax0, ay0) = (l.off_x + w.x as i64, l.off_y + w.y as i64);
        let (ax1, ay1) = (ax0 + w.width as i64, ay0 + w.height as i64);
        let (cx0, cy0, cx1, cy1) = (ax0.max(0), ay0.max(0), ax1.min(l.raw_w as i64), ay1.min(l.raw_h as i64));
        if cx0 >= cx1 || cy0 >= cy1 {
            return Ok(RasterData::from_f64_vec(self.data_type, out));
        }
        let (tw, th) = (l.tile_w as i64, l.tile_h as i64);
        let mut tiles = Vec::new();
        for ty in cy0 / th..=(cy1 - 1) / th {
            for tx in cx0 / tw..=(cx1 - 1) / tw {
                // The part of the tile in the window, in the tile's pixels.
                let (x0, y0) = (cx0.max(tx * tw) - tx * tw, cy0.max(ty * th) - ty * th);
                let (x1, y1) = (cx1.min((tx + 1) * tw) - tx * tw, cy1.min((ty + 1) * th) - ty * th);
                let href =
                    l.template.replace("{TileRow}", &(ty + l.min_row).to_string()).replace("{TileCol}", &(tx + l.min_col).to_string());
                let sub = Window::new(x0 as usize, y0 as usize, (x1 - x0) as usize, (y1 - y0) as usize);
                tiles.push((href, tx * tw + x0 - ax0, ty * th + y0 - ay0, sub));
            }
        }
        let this: &Tiled = self;
        let reads = crate::par::map_range(tiles.len(), |i| this.read_tile(&tiles[i].0, tiles[i].3, bands));
        for ((_, dx, dy, sub), r) in tiles.iter().zip(reads) {
            let Some(values) = r? else { continue };
            let (dx, dy) = (*dx as usize, *dy as usize);
            let m = sub.width * sub.height;
            for b in 0..bands.len() {
                for y in 0..sub.height {
                    let src = &values[b * m + y * sub.width..b * m + (y + 1) * sub.width];
                    let start = b * n + (dy + y) * w.width + dx;
                    out[start..start + sub.width].copy_from_slice(src);
                }
            }
        }
        Ok(RasterData::from_f64_vec(self.data_type, out))
    }
}

/// The metadata items of an item's properties, as GDAL's STACTA driver sets them: every
/// property but `tiles:tile_matrix_links`, `tiles:tile_matrix_sets` and the `proj:` ones, in
/// order, strings as they are, `null` as nothing, and other values as json-c writes them
/// (numbers as written in the file, arrays and objects spaced: `[ "a", 1 ]`, `{ "k": true }`).
fn properties_metadata(text: &str) -> Option<Vec<(String, String)>> {
    #[derive(serde::Deserialize)]
    struct Doc<'a> {
        #[serde(borrow)]
        properties: Option<&'a serde_json::value::RawValue>,
    }
    let doc: Doc = serde_json::from_str(text).ok()?;
    let mut p = JsonC { s: doc.properties?.get().as_bytes(), i: 0 };
    p.ws();
    if p.peek() != Some(b'{') {
        return None;
    }
    p.i += 1;
    let mut out = Vec::new();
    loop {
        p.ws();
        match p.peek()? {
            b'}' => break,
            b',' => {
                p.i += 1;
                continue;
            }
            _ => {}
        }
        let key = p.string()?;
        p.ws();
        p.i += 1; // ':'
        p.ws();
        let value = match p.peek()? {
            b'"' => p.string()?,
            b'n' => {
                p.literal();
                String::new()
            }
            _ => p.value()?,
        };
        if key != "tiles:tile_matrix_links" && key != "tiles:tile_matrix_sets" && !key.starts_with("proj:") {
            out.push((key, value));
        }
    }
    Some(out)
}

/// A reader of JSON text that writes values back as json-c does (`JSON_C_TO_STRING_SPACED`).
struct JsonC<'a> {
    s: &'a [u8],
    i: usize,
}

impl JsonC<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn ws(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    /// A number, `true`, `false` or `null`, as written.
    fn literal(&mut self) -> String {
        let start = self.i;
        while self.peek().is_some_and(|c| !matches!(c, b',' | b']' | b'}') && !c.is_ascii_whitespace()) {
            self.i += 1;
        }
        String::from_utf8_lossy(&self.s[start..self.i]).into_owned()
    }

    /// A string, unescaped.
    fn string(&mut self) -> Option<String> {
        let start = self.i;
        self.i += 1;
        loop {
            match self.peek()? {
                b'\\' => self.i += 2,
                b'"' => {
                    self.i += 1;
                    break;
                }
                _ => self.i += 1,
            }
        }
        serde_json::from_slice(self.s.get(start..self.i)?).ok()
    }

    /// A value, written as json-c writes it.
    fn value(&mut self) -> Option<String> {
        self.ws();
        match self.peek()? {
            b'"' => Some(escape_jsonc(&self.string()?)),
            open @ (b'[' | b'{') => {
                let close = if open == b'[' { b']' } else { b'}' };
                self.i += 1;
                let mut parts = Vec::new();
                loop {
                    self.ws();
                    match self.peek()? {
                        c if c == close => {
                            self.i += 1;
                            break;
                        }
                        b',' => self.i += 1,
                        _ if open == b'{' => {
                            let key = escape_jsonc(&self.string()?);
                            self.ws();
                            self.i += 1; // ':'
                            parts.push(format!("{key}: {}", self.value()?));
                        }
                        _ => parts.push(self.value()?),
                    }
                }
                let (o, c) = if open == b'[' { ('[', ']') } else { ('{', '}') };
                Some(if parts.is_empty() { format!("{o} {c}") } else { format!("{o} {} {c}", parts.join(", ")) })
            }
            _ => Some(self.literal()),
        }
    }
}

/// A string as json-c writes it: quoted, with `"`, `\`, `/` and control characters escaped.
fn escape_jsonc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '/' => out.push_str("\\/"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names() {
        let ok = |s: &str| parse_name(s).unwrap().unwrap();
        assert_eq!(
            ok(r#"STACTA:"https://x.org/a.json":dem:WebMercatorQuad"#),
            ("https://x.org/a.json".into(), Some("dem".into()), Some("WebMercatorQuad".into()))
        );
        assert_eq!(ok(r#"STACTA:"a.json":dem"#), ("a.json".into(), Some("dem".into()), None));
        assert_eq!(ok("STACTA:a.json"), ("a.json".into(), None, None));
        assert_eq!(ok("STACTA:a.json:dem"), ("a.json".into(), Some("dem".into()), None));
        assert_eq!(ok("STACTA:https://x.org/a.json"), ("https://x.org/a.json".into(), None, None));
        assert!(parse_name(r#"STACTA:"a.json":a:b:c"#).unwrap().is_err());
        assert!(parse_name("a.json").is_none());
    }

    #[test]
    fn resolves_templates() {
        assert_eq!(
            href_template("https://x.org/c/item.json", "./{TileMatrixSet}/{TileMatrix}/{TileRow}/{TileCol}.tif", "WGS").unwrap(),
            "https://x.org/c/WGS/{TileMatrix}/{TileRow}/{TileCol}.tif"
        );
        assert_eq!(
            href_template("/data/item.json", "{TileMatrix}/{TileCol}/{TileRow}.tif", "Q").unwrap(),
            "/data/{TileMatrix}/{TileCol}/{TileRow}.tif"
        );
        assert_eq!(href_template("/data/item.json", "file:///tiles/{TileMatrix}.tif", "Q").unwrap(), "/tiles/{TileMatrix}.tif");
        assert!(href_template("https://x.org/item.json", "file:///etc/{TileMatrix}.tif", "Q").is_err());
        assert!(href_template("https://x.org/item.json", "/vsizip//home/a.zip/{TileMatrix}.tif", "Q").is_err());
        assert_eq!(href_template("https://x.org/c/item.json", "/t/{TileMatrix}.tif", "Q").unwrap(), "https://x.org/t/{TileMatrix}.tif");
        assert_eq!(href_template("/data/item.json", "s3://b/{TileMatrix}.tif", "Q").unwrap(), "s3://b/{TileMatrix}.tif");
    }

    #[test]
    fn writes_metadata_as_gdal() {
        let text = r#"{"properties": {"a": 0.1, "b": 10.0, "c": [0.1, {"q": "x\"y/z"}, [], {}], "d": "é/\\", "n": null,
            "g": 1e-07, "t": true, "proj:epsg": 4326, "tiles:tile_matrix_sets": {}, "u": "tab\there\u0001"}}"#;
        let md = properties_metadata(text).unwrap();
        let get = |k: &str| md.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("a"), Some("0.1"));
        assert_eq!(get("b"), Some("10.0"));
        assert_eq!(get("c"), Some(r#"[ 0.1, { "q": "x\"y\/z" }, [ ], { } ]"#));
        assert_eq!(get("d"), Some("é/\\"));
        assert_eq!(get("n"), Some(""));
        assert_eq!(get("g"), Some("1e-07"));
        assert_eq!(get("t"), Some("true"));
        assert_eq!(get("u"), Some("tab\there\u{1}"));
        assert_eq!(get("proj:epsg"), None);
        assert_eq!(get("tiles:tile_matrix_sets"), None);
        assert_eq!(md.len(), 8);
    }
}

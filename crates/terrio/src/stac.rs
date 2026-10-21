//! SpatioTemporal Asset Catalog (STAC) documents as datasets, as GDAL's STACIT driver and its
//! GeoJSON driver read them.
//!
//! - A STAC Item, ItemCollection (such as a STAC API search result), Collection or Catalog (local
//!   or remote JSON) opens as a vector layer of item footprints with their properties, read as
//!   GDAL reads STAC GeoJSON: the properties, the item's other members (`stac_version`, `links`,
//!   `stac_extensions`, `collection`) and each asset's members as `assets.<asset>.<member>`.
//!   Search results are followed through their `next` links, and collections through their
//!   `item` and `items` links, up to 1000 items by default (`max_items=N`, 0 for no limit).
//! - `STAC:"item.json":asset` (or `item.json!asset`) opens an asset of an item, its href
//!   resolved against the item's location. Cloud Optimized GeoTIFFs are read in place.
//! - `STACIT:"source":asset=name` mosaics an asset of the items of a search or item collection,
//!   as GDAL's STACIT driver does: one subdataset per collection, asset and CRS
//!   (`collection=`, `asset=` and `crs=` filters), items drawn in ascending datetime order onto
//!   a grid at the items' average resolution (`resolution=highest|lowest|average`), read by
//!   window from the items' files.
//!
//! [`crate::info::info_path`] lists an item's assets, or a collection's mosaics, as subdatasets.

use crate::cog::{LazyRaster, Window, WindowOptions, WindowSource};
use crate::crs::SpatialRef;
use crate::formats::{RasterDriver, VectorDriver};
use crate::info::{Info, InfoOptions, Subdataset};
use crate::ops::vrt::SourceRead;
use crate::{Dataset, Error, RasterDataset, Result, VectorDataset};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use wbraster::DataType;
use wbraster::raster::RasterData;

/// The most items read by default, as in GDAL's STACIT driver.
pub const DEFAULT_MAX_ITEMS: usize = 1000;

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("STAC: {msg}"))
}

fn is_url(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// A location's bytes: a local file, a URL or object storage, or a zip archive member.
fn fetch(loc: &str) -> Result<Vec<u8>> {
    use crate::vfs::Location;
    match Location::parse(loc) {
        Location::Local(p) => Ok(std::fs::read(p)?),
        #[cfg(not(target_arch = "wasm32"))]
        l @ (Location::Url(_) | Location::S3 { .. }) => {
            let remote = l.remote().ok_or_else(|| bad(format!("'{loc}' is not remote")))?;
            crate::http::get(&remote)
        }
        _ => {
            let local = crate::vfs::resolve(loc)?;
            Ok(std::fs::read(local.path())?)
        }
    }
}

fn fetch_json(loc: &str) -> Result<Value> {
    serde_json::from_slice(&fetch(loc)?).map_err(|e| bad(format!("{loc}: {e}")))
}

/// An href resolved against the location of the document holding it.
pub(crate) fn resolve_href(base: &str, href: &str) -> String {
    if href.contains("://") || href.starts_with("/vsi") {
        return href.to_string();
    }
    if is_url(base) {
        if href.starts_with('/') {
            let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
            let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
            return format!("{scheme}://{host}{href}");
        }
    } else if href.starts_with('/') || std::path::Path::new(href).is_absolute() {
        return href.to_string();
    }
    let dir = base.split(['?', '#']).next().unwrap_or(base);
    let dir = match dir.rfind(['/', '\\']) {
        Some(i) => &dir[..i],
        None => "",
    };
    let mut parts: Vec<&str> = if dir.is_empty() { Vec::new() } else { dir.split('/').collect() };
    for seg in href.split('/') {
        match seg {
            "." | "" => {}
            ".." if parts.len() > 1 && parts.last() != Some(&"..") => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// What kind of STAC document a JSON value is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Item,
    ItemCollection,
    Collection,
    Catalog,
}

fn kind(v: &Value) -> Option<Kind> {
    let is_item = |f: &Value| f["type"] == "Feature" && f.get("stac_version").is_some();
    match v["type"].as_str()? {
        "Feature" if is_item(v) => Some(Kind::Item),
        "FeatureCollection" if v.get("stac_version").is_some() || v["features"].as_array().is_some_and(|f| f.iter().any(is_item)) => {
            Some(Kind::ItemCollection)
        }
        "Collection" if v.get("stac_version").is_some() => Some(Kind::Collection),
        "Catalog" if v.get("stac_version").is_some() => Some(Kind::Catalog),
        _ => None,
    }
}

/// Items with the location each was read from (their hrefs are relative to it).
struct Items {
    items: Vec<(Value, String)>,
    /// Whether the item limit stopped the reading.
    truncated: bool,
}

/// Reads the items of a STAC document at `loc` (already parsed as `doc`), following search
/// pages and collection links, up to `max_items` items (0: no limit).
fn load_items(doc: Value, loc: &str, max_items: usize) -> Result<Items> {
    let mut out = Items { items: Vec::new(), truncated: false };
    let full = |out: &Items| max_items > 0 && out.items.len() >= max_items;
    match kind(&doc) {
        Some(Kind::Item) => out.items.push((doc, loc.to_string())),
        Some(Kind::ItemCollection) => read_pages(doc, loc, max_items, &mut out)?,
        Some(Kind::Collection | Kind::Catalog) => {
            let mut queue = vec![(doc, loc.to_string(), 0usize)];
            while let Some((doc, base, depth)) = queue.pop() {
                for link in doc["links"].as_array().into_iter().flatten() {
                    if full(&out) {
                        out.truncated = true;
                        return Ok(out);
                    }
                    let Some(href) = link["href"].as_str() else { continue };
                    let href = resolve_href(&base, href);
                    match link["rel"].as_str() {
                        Some("item") => {
                            let item = fetch_json(&href)?;
                            if kind(&item) == Some(Kind::Item) {
                                out.items.push((item, href));
                            }
                        }
                        Some("items") => read_pages(fetch_json(&href)?, &href, max_items, &mut out)?,
                        Some("child") if depth < 8 => queue.push((fetch_json(&href)?, href, depth + 1)),
                        _ => {}
                    }
                }
            }
        }
        None => return Err(bad(format!("'{loc}' is not a STAC item, item collection, collection or catalog"))),
    }
    Ok(out)
}

/// Reads the items of a page of results and of the pages its `next` links lead to.
fn read_pages(mut doc: Value, loc: &str, max_items: usize, out: &mut Items) -> Result<()> {
    let mut cur = loc.to_string();
    let mut pages = 0;
    loop {
        pages += 1;
        for f in doc["features"].as_array().into_iter().flatten() {
            if max_items > 0 && out.items.len() >= max_items {
                out.truncated = true;
                return Ok(());
            }
            out.items.push((f.clone(), cur.clone()));
        }
        // https://github.com/radiantearth/stac-api-spec/tree/release/v1.0.0/item-search#pagination
        let next = doc["links"].as_array().into_iter().flatten().find(|l| {
            l["rel"] == "next"
                && l["type"].as_str().is_none_or(|t| t == "application/geo+json" || t.is_empty())
                && l["method"].as_str().is_none_or(|m| m.eq_ignore_ascii_case("GET"))
        });
        let Some(href) = next.and_then(|l| l["href"].as_str()) else { return Ok(()) };
        let href = resolve_href(&cur, href);
        if href == cur || pages > 10_000 {
            return Ok(());
        }
        doc = fetch_json(&href)?;
        cur = href;
    }
}

/// The footprints of items as a vector layer, read as GDAL reads STAC GeoJSON.
fn footprints(items: &[(Value, String)], name: &str) -> Result<wbvector::Layer> {
    let features: Vec<&Value> = items.iter().map(|(v, _)| v).collect();
    let fc = json!({"type": "FeatureCollection", "features": features});
    let bytes = serde_json::to_vec(&fc).map_err(bad)?;
    crate::drivers::vector::geojson::parse(&bytes, name)
}

/// The layer name of a STAC document: the file name's stem, or the collection's id.
fn layer_name(doc: &Value, loc: &str) -> String {
    if matches!(kind(doc), Some(Kind::Collection | Kind::Catalog))
        && let Some(id) = doc["id"].as_str()
    {
        return id.to_string();
    }
    let path = loc.split(['?', '#']).next().unwrap_or(loc);
    let file = path.trim_end_matches('/').rsplit(['/', '\\']).next().unwrap_or(path);
    let stem = file.split('.').next().unwrap_or(file);
    if stem.is_empty() { "items".into() } else { stem.to_string() }
}

/// Whether an asset can hold raster or vector data (not metadata, thumbnails or text).
fn is_data_asset(asset: &Value) -> bool {
    let t = asset["type"].as_str().unwrap_or_default();
    if matches!(t, "application/json" | "application/xml" | "text/plain" | "text/html") {
        return false;
    }
    !asset["roles"].as_array().into_iter().flatten().any(|r| matches!(r.as_str(), Some("thumbnail" | "info" | "metadata" | "overview")))
}

// ---------------------------------------------------------------------------------------------
// Dataset names

/// A parsed `STAC:` or `STACIT:` dataset name.
#[derive(Debug, Clone, PartialEq)]
enum Name {
    /// An asset of an item: `STAC:"item.json":asset` or `item.json!asset`.
    Asset { item: String, asset: String },
    /// A mosaic of the items of a search or collection: `STACIT:"source":key=value,...`.
    Mosaic { source: String, filters: Filters },
}

/// The filters and options of a `STACIT:` name.
#[derive(Debug, Clone, Default, PartialEq)]
struct Filters {
    collection: Option<String>,
    asset: Option<String>,
    crs: Option<String>,
    resolution: Option<String>,
    max_items: Option<usize>,
}

/// Splits `"quoted":rest` or `unquoted:rest` (the rest after the last colon when it looks like
/// `key=value` options or an asset name).
fn split_source(rest: &str, options_have_equals: bool) -> (String, Option<String>) {
    if let Some(q) = rest.strip_prefix('"')
        && let Some(end) = q.find('"')
    {
        let tail = q[end + 1..].strip_prefix(':').map(str::to_string).filter(|s| !s.is_empty());
        return (q[..end].to_string(), tail);
    }
    if let Some((src, tail)) = rest.rsplit_once(':')
        && !tail.contains('/')
        && !tail.is_empty()
        && (!options_have_equals || tail.contains('='))
        && !src.is_empty()
        && !src.ends_with(":/")
        && src.len() > 1
    {
        return (src.to_string(), Some(tail.to_string()));
    }
    (rest.to_string(), None)
}

fn parse_name(path: &str) -> Option<Name> {
    let p = path.trim();
    if let Some(rest) = p.strip_prefix("STACIT:").or_else(|| p.strip_prefix("stacit:")) {
        let (source, tail) = split_source(rest, true);
        let mut filters = Filters::default();
        for kv in tail.iter().flat_map(|t| t.split(',')) {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            match k.trim().to_ascii_lowercase().as_str() {
                "collection" => filters.collection = Some(v.to_string()),
                "asset" => filters.asset = Some(v.to_string()),
                "crs" => filters.crs = Some(v.to_string()),
                "resolution" => filters.resolution = Some(v.to_ascii_uppercase()),
                "max_items" => filters.max_items = v.parse().ok(),
                _ => {}
            }
        }
        return Some(Name::Mosaic { source, filters });
    }
    if let Some(rest) = p.strip_prefix("STAC:").or_else(|| p.strip_prefix("stac:")) {
        let (item, asset) = split_source(rest, false);
        return Some(Name::Asset { item, asset: asset? });
    }
    // item.json!asset
    let lower = p.to_ascii_lowercase();
    for ext in [".json!", ".geojson!"] {
        if let Some(i) = lower.find(ext) {
            let (item, asset) = (&p[..i + ext.len() - 1], &p[i + ext.len()..]);
            if !asset.is_empty() && !asset.contains('/') {
                return Some(Name::Asset { item: item.to_string(), asset: asset.to_string() });
            }
        }
    }
    None
}

/// The href of an asset of an item, resolved against the item's location.
fn asset_href(item_loc: &str, asset: &str) -> Result<String> {
    let doc = fetch_json(item_loc)?;
    let item = match kind(&doc) {
        Some(Kind::Item) => doc,
        _ => return Err(bad(format!("'{item_loc}' is not a STAC item"))),
    };
    let a = &item["assets"][asset];
    let href = a["href"].as_str().ok_or_else(|| {
        let names: Vec<&String> = item["assets"].as_object().map(|o| o.keys().collect()).unwrap_or_default();
        let names: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        bad(format!("'{item_loc}' has no asset '{asset}' (assets: {})", names.join(", ")))
    })?;
    Ok(resolve_href(item_loc, href))
}

// ---------------------------------------------------------------------------------------------
// STACIT: assets grouped by collection, asset and CRS

/// An item's asset in a mosaic.
#[derive(Debug, Clone)]
struct AssetItem {
    href: String,
    datetime: String,
    width: usize,
    height: usize,
    bounds: [f64; 4],
}

/// Assets of one asset name, by CRS.
#[derive(Debug, Default)]
struct AssetGroup {
    bands: Value,
    by_crs: BTreeMap<String, Vec<AssetItem>>,
}

type Groups = BTreeMap<String, BTreeMap<String, AssetGroup>>;

/// GDAL's sanitized CRS name for the `crs=` filter: runs of non-alphanumeric characters become
/// one underscore.
fn sanitize_crs(v: &str) -> String {
    let mut out = String::new();
    let mut last_alnum = true;
    for ch in v.chars() {
        if !ch.is_ascii_alphanumeric() {
            if last_alnum {
                out.push('_');
            }
            last_alnum = false;
        } else {
            out.push(ch);
            last_alnum = true;
        }
    }
    if out.ends_with('_') {
        out.pop();
    }
    out
}

/// Groups the assets of items as GDAL's STACIT driver does (`ParseAsset()`).
fn group_assets(items: &[(Value, String)], f: &Filters) -> Groups {
    let mut groups: Groups = BTreeMap::new();
    let crs_filter = f.crs.as_deref().map(sanitize_crs);
    for (item, base) in items {
        let has_proj = item["stac_extensions"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|e| e.as_str().is_some_and(|s| s == "proj" || s.starts_with("https://stac-extensions.github.io/projection/")));
        let props = &item["properties"];
        if !has_proj || !props.is_object() || !item["assets"].is_object() {
            continue;
        }
        let collection = match &item["collection"] {
            Value::String(s) => s.clone(),
            Value::Null => String::new(),
            v => v.to_string(),
        };
        if f.collection.as_ref().is_some_and(|c| *c != collection) {
            continue;
        }
        for (name, asset) in item["assets"].as_object().into_iter().flatten() {
            if f.asset.as_ref().is_some_and(|a| a != name) {
                continue;
            }
            let t = asset["type"].as_str().unwrap_or_default();
            if matches!(t, "application/json" | "application/xml" | "text/plain")
                || asset["roles"].as_array().into_iter().flatten().any(|r| matches!(r.as_str(), Some("thumbnail" | "info" | "metadata")))
            {
                continue;
            }
            let Some(href) = asset["href"].as_str().filter(|h| !h.is_empty()) else { continue };
            let get = |k: &str| if asset.get(k).is_some() { &asset[k] } else { &props[k] };
            let crs = if !get("proj:code").is_null() && get("proj:code").as_str().is_some() {
                get("proj:code").as_str().unwrap_or_default().to_string()
            } else if !get("proj:epsg").is_null() {
                // Older items give ESRI codes (54009) there too.
                let code = get("proj:epsg");
                let esri = code.as_u64().and_then(|c| u32::try_from(c).ok()).is_some_and(crate::crs::is_esri_code);
                format!("{}:{code}", if esri { "ESRI" } else { "EPSG" })
            } else if let Some(w) = get("proj:wkt2").as_str() {
                w.to_string()
            } else if get("proj:projjson").is_object() {
                get("proj:projjson").to_string()
            } else {
                continue;
            };
            if crs_filter.as_ref().is_some_and(|c| *c != sanitize_crs(&crs)) {
                continue;
            }
            let nums = |k: &str| -> Option<Vec<f64>> { get(k).as_array().map(|a| a.iter().filter_map(Value::as_f64).collect()) };
            let bbox = nums("proj:bbox").filter(|b| b.len() == 4);
            let shape = nums("proj:shape").filter(|s| s.len() == 2);
            let transform = nums("proj:transform").filter(|t| t.len() == 6 || t.len() == 9);
            if let Some(t) = &transform
                && (t[0] <= 0.0
                    || t[1] != 0.0
                    || t[3] != 0.0
                    || t[4] >= 0.0
                    || (t.len() == 9 && (t[6] != 0.0 || t[7] != 0.0 || t[8] != 1.0)))
            {
                continue;
            }
            let (width, height, bounds) = match (&bbox, &shape, &transform) {
                (Some(b), Some(s), _) => (s[1] as i64, s[0] as i64, [b[0], b[1], b[2], b[3]]),
                (Some(b), None, Some(t)) => {
                    if b[0] != t[2] || b[3] != t[5] {
                        continue;
                    }
                    (((b[2] - b[0]) / t[0]) as i64, ((b[3] - b[1]) / -t[4]) as i64, [b[0], b[1], b[2], b[3]])
                }
                (None, Some(s), Some(t)) => {
                    let (w, h) = (s[1] as i64, s[0] as i64);
                    (w, h, [t[2], t[5] + h as f64 * t[4], t[2] + w as f64 * t[0], t[5]])
                }
                _ => continue,
            };
            if width <= 0 || height <= 0 {
                continue;
            }
            let entry = groups.entry(collection.clone()).or_default().entry(name.clone()).or_insert_with(|| AssetGroup {
                bands: if asset["bands"].is_array() { asset["bands"].clone() } else { asset["eo:bands"].clone() },
                by_crs: BTreeMap::new(),
            });
            entry.by_crs.entry(crs).or_default().push(AssetItem {
                href: resolve_href(base, href),
                datetime: props["datetime"].as_str().unwrap_or_default().to_string(),
                width: width as usize,
                height: height as usize,
                bounds,
            });
        }
    }
    groups
}

/// The subdatasets of grouped assets, named and described as GDAL's STACIT driver does.
fn stacit_subdatasets(source: &str, groups: &Groups) -> Vec<Subdataset> {
    let mut out = Vec::new();
    for (collection, assets) in groups {
        for (asset, group) in assets {
            let (arg, text) = if groups.len() > 1 {
                (format!("collection={collection},asset={asset}"), format!("Collection {collection}, Asset {asset}"))
            } else {
                (format!("asset={asset}"), format!("Asset {asset}"))
            };
            if group.by_crs.len() == 1 {
                out.push(Subdataset { name: format!("STACIT:\"{source}\":{arg}"), description: format!("{text} of {source}") });
            } else {
                for crs in group.by_crs.keys() {
                    out.push(Subdataset {
                        name: format!("STACIT:\"{source}\":{arg},crs={}", sanitize_crs(crs)),
                        description: format!("{text} of {source} in CRS {crs}"),
                    });
                }
            }
        }
    }
    out
}

/// The CRS of a STAC CRS string (`EPSG:32613`, WKT 2 or PROJJSON).
fn crs_of(s: &str) -> SpatialRef {
    if s.trim_start().starts_with('{')
        && let Ok(v) = serde_json::from_str::<Value>(s)
    {
        if let Some(code) = crate::drivers::vector::projjson::epsg(&v)
            && let Ok(r) = SpatialRef::from_epsg(code)
        {
            return r;
        }
        if let Some(w) = crate::drivers::vector::projjson::to_wkt1(&v) {
            return SpatialRef::from_wkt(&w);
        }
    }
    SpatialRef::parse(s).unwrap_or_default()
}

/// A mosaic of the files of a STACIT subdataset, read by window.
struct Mosaic {
    items: Vec<(AssetItem, Option<LazyRaster>)>,
    geo_transform: [f64; 6],
    data_type: DataType,
    nodata: Option<f64>,
}

impl WindowSource for Mosaic {
    fn overviews(&self) -> Vec<(usize, usize)> {
        Vec::new()
    }

    fn read(&mut self, _level: usize, w: Window, bands: &[usize]) -> Result<RasterData> {
        let (dw, dh) = (w.width, w.height);
        let gt = self.geo_transform;
        let (rx, ry) = (gt[1], -gt[5]);
        let mut out: Vec<Vec<f64>> = vec![vec![self.nodata.unwrap_or(0.0); dw * dh]; bands.len()];
        for (item, raster) in &mut self.items {
            let [xmin, ymin, xmax, ymax] = item.bounds;
            let (dxo, dxs) = ((xmin - gt[0]) / rx, (xmax - xmin) / rx);
            let (dyo, dys) = ((gt[3] - ymax) / ry, (ymax - ymin) / ry);
            let (x0, x1) = (dxo.max(w.x as f64), (dxo + dxs).min((w.x + dw) as f64));
            let (y0, y1) = (dyo.max(w.y as f64), (dyo + dys).min((w.y + dh) as f64));
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
            let (scx, scy) = (item.width as f64 / dxs, item.height as f64 / dys);
            let clamp = |v: f64, max: usize| (v.max(0.0) as usize).min(max);
            let sx0 = clamp(((x0 - dxo) * scx).floor() - 1.0, item.width);
            let sx1 = clamp(((x1 - dxo) * scx).ceil() + 1.0, item.width);
            let sy0 = clamp(((y0 - dyo) * scy).floor() - 1.0, item.height);
            let sy1 = clamp(((y1 - dyo) * scy).ceil() + 1.0, item.height);
            if sx1 <= sx0 || sy1 <= sy0 {
                continue;
            }
            if raster.is_none() {
                *raster = Some(LazyRaster::open(&item.href)?);
            }
            let r = raster.as_mut().ok_or_else(|| bad("unreachable"))?;
            let (sw, sh) = (sx1 - sx0, sy1 - sy0);
            if sx1 > r.width() || sy1 > r.height() {
                return Err(bad(format!("{} is {}x{}, smaller than its STAC metadata says", item.href, r.width(), r.height())));
            }
            let sub = r.read_window(Window::new(sx0, sy0, sw, sh), &WindowOptions { bands: bands.to_vec(), overview: None })?;
            for (bi, buf) in out.iter_mut().enumerate() {
                let values = crate::ops::BandView::of(&sub.raster, bi);
                let valid: Option<Vec<bool>> = self.nodata.map(|nd| values.valid(nd));
                let req = SourceRead {
                    values,
                    valid: valid.as_deref(),
                    width: sw,
                    height: sh,
                    src_type: sub.raster.data_type,
                    src_rect: [-(sx0 as f64), -(sy0 as f64), item.width as f64, item.height as f64],
                    dst_rect: [dxo - w.x as f64, dyo - w.y as f64, dxs, dys],
                    resampling: crate::ops::Resampling::Nearest,
                    working_float: self.nodata.is_some(),
                    invalid_value: None,
                };
                req.read(dw, dh, buf, |v| v);
            }
        }
        Ok(RasterData::from_f64_vec(self.data_type, out.concat()))
    }
}

/// Opens the one mosaic of grouped assets (GDAL's `STACITDataset::SetupDataset()`).
fn open_mosaic(source: &str, groups: Groups, resolution: Option<&str>) -> Result<LazyRaster> {
    let group_count: usize = groups.values().flat_map(|a| a.values()).map(|g| g.by_crs.len()).sum();
    if group_count == 0 {
        return Err(bad(format!("no compatible asset found in '{source}'")));
    }
    if group_count > 1 {
        let subs = stacit_subdatasets(source, &groups);
        let names: Vec<String> = subs.iter().take(8).map(|s| s.name.clone()).collect();
        return Err(Error::InvalidArgument(format!(
            "'{source}' holds {group_count} mosaics (assets or CRSs): choose one, such as {}{}",
            names.join(", "),
            if subs.len() > 8 { ", ..." } else { "" }
        )));
    }
    let (_, assets) = groups.into_iter().next().ok_or_else(|| bad("no collection"))?;
    let (_, group) = assets.into_iter().next().ok_or_else(|| bad("no asset"))?;
    let (crs, mut items) = group.by_crs.into_iter().next().ok_or_else(|| bad("no CRS"))?;
    let (mut xmin, mut ymin, mut xmax, mut ymax) = (f64::MAX, f64::MAX, -f64::MAX, -f64::MAX);
    let (mut rx, mut ry) = (0.0, 0.0);
    let strategy = resolution.unwrap_or("AVERAGE");
    for it in &items {
        xmin = xmin.min(it.bounds[0]);
        ymin = ymin.min(it.bounds[1]);
        xmax = xmax.max(it.bounds[2]);
        ymax = ymax.max(it.bounds[3]);
        let (tx, ty) = ((it.bounds[2] - it.bounds[0]) / it.width as f64, (it.bounds[3] - it.bounds[1]) / it.height as f64);
        if rx != 0.0 && strategy.eq_ignore_ascii_case("HIGHEST") {
            rx = f64::min(rx, tx);
            ry = f64::min(ry, ty);
        } else if rx != 0.0 && strategy.eq_ignore_ascii_case("LOWEST") {
            rx = f64::max(rx, tx);
            ry = f64::max(ry, ty);
        } else {
            rx += tx;
            ry += ty;
        }
    }
    if strategy.eq_ignore_ascii_case("AVERAGE") {
        rx /= items.len() as f64;
        ry /= items.len() as f64;
    }
    let (w, h) = (((xmax - xmin) / rx).round(), ((ymax - ymin) / ry).round());
    if !(rx > 0.0 && ry > 0.0 && w >= 1.0 && h >= 1.0 && w < i32::MAX as f64 && h < i32::MAX as f64) {
        return Err(bad("invalid computed dataset dimensions"));
    }
    let (w, h) = (w as usize, h as usize);
    // The first item tells the band count, data type and nodata value.
    let first_href = items[0].href.clone();
    let first = LazyRaster::open(&first_href)?;
    let (bands, data_type, nodata) = (first.band_count(), first.data_type(), first.nodata());
    // Ascending datetime: later items are drawn over earlier ones.
    items.sort_by(|a, b| a.datetime.cmp(&b.datetime));
    let srs = crs_of(&crs);
    let mut metadata = Vec::new();
    for (i, b) in group.bands.as_array().into_iter().flatten().enumerate().take(bands) {
        if let Some(name) = b["name"].as_str() {
            metadata.push((format!("BAND_{}_NAME", i + 1), name.to_string()));
        }
    }
    let raster = wbraster::Raster {
        cols: w,
        rows: h,
        bands,
        x_min: xmin,
        y_min: ymax - h as f64 * ry,
        cell_size_x: rx,
        cell_size_y: ry,
        nodata: nodata.unwrap_or(-9999.0),
        data_type,
        crs: srs.to_crs_info(),
        metadata,
        data: RasterData::new_filled(data_type, 0, 0.0),
    };
    let header = RasterDataset::with_driver(raster, RasterDriver::STACIT, nodata.is_some());
    let mut items: Vec<(AssetItem, Option<LazyRaster>)> = items.into_iter().map(|i| (i, None)).collect();
    if let Some(slot) = items.iter_mut().find(|(i, _)| i.href == first_href) {
        slot.1 = Some(first);
    }
    let mosaic = Mosaic { items, geo_transform: [xmin, rx, 0.0, ymax, 0.0, -ry], data_type, nodata };
    Ok(LazyRaster::from_source(header, Box::new(mosaic)))
}

// ---------------------------------------------------------------------------------------------
// Entry points

/// Whether a path may be a STAC document: a `.json` file or URL, or an API endpoint.
fn may_be_stac(path: &str) -> bool {
    let base = path.split(['?', '#']).next().unwrap_or(path).to_ascii_lowercase();
    if base.ends_with(".json") || base.ends_with(".geojson") {
        return true;
    }
    // Zarr stores (`.../collections/.../x.zarr/...`) are not STAC documents.
    if base.split('/').any(|s| s.ends_with(".zarr")) {
        return false;
    }
    is_url(path) && (base.contains("/search") || base.ends_with("/items") || base.contains("/collections/"))
}

/// A STAC document at `path`: `None` when it is not one (having read at most its first bytes
/// for local files). A remote JSON document that is not STAC is opened as GeoJSON.
enum Probe {
    Stac(Value),
    Other(Box<Dataset>),
}

fn probe(path: &str) -> Option<Result<Probe>> {
    if !may_be_stac(path) {
        return None;
    }
    let loc = crate::vfs::Location::parse(path);
    if let crate::vfs::Location::Local(p) = &loc {
        // Only the first bytes, for files that are not STAC.
        let mut head = vec![0u8; 65536];
        let n = std::fs::File::open(p).and_then(|mut f| std::io::Read::read(&mut f, &mut head)).ok()?;
        if !String::from_utf8_lossy(&head[..n]).contains("\"stac_version\"") {
            return None;
        }
    } else if !matches!(loc, crate::vfs::Location::Url(_) | crate::vfs::Location::S3 { .. }) {
        return None;
    }
    let bytes = match fetch(path) {
        Ok(b) => b,
        Err(e) => return Some(Err(e)),
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(v) if kind(&v).is_some() => Some(Ok(Probe::Stac(v))),
        _ if loc.is_local() => None,
        _ => Some(crate::memory::open_bytes(bytes, &loc.file_name()).map(|ds| Probe::Other(Box::new(ds)))),
    }
}

/// The footprints layer of a STAC document.
fn footprint_dataset(doc: Value, path: &str, max_items: usize) -> Result<(VectorDataset, Groups)> {
    let name = layer_name(&doc, path);
    let items = load_items(doc, path, max_items)?;
    let groups = group_assets(&items.items, &Filters::default());
    let layer = footprints(&items.items, &name)?;
    Ok((crate::dataset::finish_vector(vec![layer], path, VectorDriver::GeoJSON), groups))
}

/// Opens a STAC dataset name or document. `None` when `path` is neither.
pub(crate) fn open(path: &str) -> Option<Result<Dataset>> {
    match parse_name(path) {
        Some(Name::Asset { item, asset }) => return Some(asset_href(&item, &asset).and_then(|href| crate::open(&href))),
        Some(Name::Mosaic { .. }) => return Some(open_lazy(path)?.and_then(|mut r| r.read()).map(Dataset::Raster)),
        None => {}
    }
    Some(match probe(path)? {
        Ok(Probe::Stac(doc)) => footprint_dataset(doc, path, DEFAULT_MAX_ITEMS).map(|(v, _)| Dataset::Vector(v)),
        Ok(Probe::Other(ds)) => Ok(*ds),
        Err(e) => Err(e),
    })
}

/// Opens a `STAC:` asset or `STACIT:` mosaic lazily. `None` when `path` is neither.
pub(crate) fn open_lazy(path: &str) -> Option<Result<LazyRaster>> {
    Some(match parse_name(path)? {
        Name::Asset { item, asset } => asset_href(&item, &asset).and_then(|href| LazyRaster::open(&href)),
        Name::Mosaic { source, filters } => (|| {
            let doc = fetch_json(&source)?;
            let mut max = filters.max_items.unwrap_or(DEFAULT_MAX_ITEMS);
            if filters.max_items.is_none()
                && let Some(i) = source.to_ascii_lowercase().find("&limit=")
            {
                let digits: String = source[i + 7..].chars().take_while(char::is_ascii_digit).collect();
                max = max.max(digits.parse().unwrap_or(0));
            }
            let items = load_items(doc, &source, max)?;
            let groups = group_assets(&items.items, &filters);
            open_mosaic(&source, groups, filters.resolution.as_deref())
        })(),
    })
}

/// Summarizes a STAC dataset: the footprints of a document with its assets (an item's) or
/// mosaics (a collection's) as subdatasets, an asset, or a mosaic. `None` when `path` is not one.
pub(crate) fn info(path: &str, options: &InfoOptions) -> Option<Result<Info>> {
    match parse_name(path) {
        Some(Name::Asset { item, asset }) => {
            return Some(asset_href(&item, &asset).and_then(|href| crate::info::info_path(&href, options)));
        }
        Some(Name::Mosaic { source, filters }) => {
            let several = (|| -> Result<Option<Info>> {
                let doc = fetch_json(&source)?;
                let max = filters.max_items.unwrap_or(DEFAULT_MAX_ITEMS);
                let name = layer_name(&doc, &source);
                let items = load_items(doc, &source, max)?;
                let groups = group_assets(&items.items, &filters);
                let count: usize = groups.values().flat_map(|a| a.values()).map(|g| g.by_crs.len()).sum();
                if count <= 1 {
                    return Ok(None);
                }
                let layer = footprints(&items.items, &name)?;
                let ds = crate::dataset::finish_vector(vec![layer], &source, VectorDriver::GeoJSON);
                let mut v = crate::info::vector_info(&ds);
                v.subdatasets = stacit_subdatasets(&source, &groups);
                Ok(Some(Info::Vector(v)))
            })();
            return Some(match several {
                Ok(Some(i)) => Ok(i),
                Ok(None) => {
                    open_lazy(path)?.and_then(|mut r| crate::info::lazy_raster_info(&mut r, options)).map(|r| Info::Raster(Box::new(r)))
                }
                Err(e) => Err(e),
            });
        }
        None => {}
    }
    let doc = match probe(path)? {
        Ok(Probe::Stac(doc)) => doc,
        Ok(Probe::Other(ds)) => return Some(Ok(crate::info::info(&ds, options))),
        Err(e) => return Some(Err(e)),
    };
    let is_item = kind(&doc) == Some(Kind::Item);
    let assets: Vec<(String, Value)> =
        if is_item { doc["assets"].as_object().into_iter().flatten().map(|(k, v)| (k.clone(), v.clone())).collect() } else { Vec::new() };
    Some(footprint_dataset(doc, path, DEFAULT_MAX_ITEMS).map(|(ds, groups)| {
        let mut v = crate::info::vector_info(&ds);
        v.subdatasets = if is_item {
            assets
                .iter()
                .filter(|(_, a)| is_data_asset(a))
                .map(|(k, a)| Subdataset {
                    name: format!("STAC:\"{path}\":{k}"),
                    description: match a["title"].as_str() {
                        Some(t) => format!("Asset {k}: {t}"),
                        None => format!("Asset {k}"),
                    },
                })
                .collect()
        } else {
            stacit_subdatasets(path, &groups)
        };
        Info::Vector(v)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names() {
        assert_eq!(
            parse_name(r#"STACIT:"https://x.org/search?a=1&b=2":asset=red,crs=EPSG_32613"#),
            Some(Name::Mosaic {
                source: "https://x.org/search?a=1&b=2".into(),
                filters: Filters { asset: Some("red".into()), crs: Some("EPSG_32613".into()), ..Default::default() }
            })
        );
        assert_eq!(
            parse_name("STACIT:items.json:asset=red"),
            Some(Name::Mosaic { source: "items.json".into(), filters: Filters { asset: Some("red".into()), ..Default::default() } })
        );
        assert_eq!(
            parse_name("STACIT:https://x.org/items.json"),
            Some(Name::Mosaic { source: "https://x.org/items.json".into(), filters: Filters::default() })
        );
        assert_eq!(parse_name(r#"STAC:"C:\d\item.json":B04"#), Some(Name::Asset { item: r"C:\d\item.json".into(), asset: "B04".into() }));
        assert_eq!(
            parse_name("STAC:https://x.org/item.json:B04"),
            Some(Name::Asset { item: "https://x.org/item.json".into(), asset: "B04".into() })
        );
        assert_eq!(parse_name("dir/item.json!visual"), Some(Name::Asset { item: "dir/item.json".into(), asset: "visual".into() }));
        assert_eq!(parse_name("dir/item.json"), None);
        assert_eq!(parse_name("a.zip!b.json"), None);
    }

    #[test]
    fn resolves_hrefs() {
        assert_eq!(resolve_href("https://x.org/a/b/item.json?q=1", "./B04.tif"), "https://x.org/a/b/B04.tif");
        assert_eq!(resolve_href("https://x.org/a/b/item.json", "../c/B04.tif"), "https://x.org/a/c/B04.tif");
        assert_eq!(resolve_href("https://x.org/a/item.json", "/c/B04.tif"), "https://x.org/c/B04.tif");
        assert_eq!(resolve_href("dir/item.json", "B04.tif"), "dir/B04.tif");
        assert_eq!(resolve_href("item.json", "B04.tif"), "B04.tif");
        assert_eq!(resolve_href("/data/item.json", "s3://b/k.tif"), "s3://b/k.tif");
    }

    #[test]
    fn sanitizes_crs_like_gdal() {
        assert_eq!(sanitize_crs("EPSG:32613"), "EPSG_32613");
        assert_eq!(sanitize_crs("{\"a\": 1}"), "_a_1");
    }
}

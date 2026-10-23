//! STAC items with tiled assets (STACTA), as GDAL's STACTA driver reads them.
//!
//! A STAC item of the tiled-assets extension describes a pyramid of raster tiles: its
//! `tiles:tile_matrix_sets` (OGC tile matrix sets), the `tiles:tile_matrix_links` limits of the
//! tiles that exist at each tile matrix, and `asset_templates` whose `href` names the tile files
//! (`.../{TileMatrix}/{TileRow}/{TileCol}.tif`, resolved against the item's location). It opens
//! as one raster read by window: its most detailed tile matrix is the full resolution, over the
//! extent of the tiles its limits allow, and the coarser ones its overviews. The band count, data
//! type and nodata value are those of its first tile. A tile that does not exist is an error,
//! unless `GDAL_STACTA_SKIP_MISSING_METATILE` (or `TERRIO_STACTA_SKIP_MISSING_TILE`) is `YES`:
//! its pixels are then nodata.
//!
//! Names: the item's path or URL (`.json`), or `STACTA:"item.json":asset:tile_matrix_set`, as
//! GDAL names an asset and a tile matrix set of an item that has several.

use crate::cog::{LazyRaster, Window, WindowOptions, WindowSource};
use crate::crs::SpatialRef;
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use wbraster::DataType;
use wbraster::raster::RasterData;

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("STACTA: {msg}"))
}

/// Whether a STAC document is an item with tiled assets.
pub(crate) fn is_stacta(doc: &Value) -> bool {
    doc["type"] == "Feature" && doc["asset_templates"].is_object() && doc["properties"]["tiles:tile_matrix_sets"].is_object()
}

/// A `STACTA:"item":asset:tms` name: the item, and the asset and tile matrix set asked for.
fn parse_name(path: &str) -> Option<(String, Option<String>, Option<String>)> {
    let rest = path.trim().strip_prefix("STACTA:").or_else(|| path.trim().strip_prefix("stacta:"))?;
    let (item, tail) = match rest.strip_prefix('"') {
        Some(q) => {
            let end = q.find('"')?;
            (q[..end].to_string(), q[end + 1..].strip_prefix(':').unwrap_or(""))
        }
        None => (rest.to_string(), ""),
    };
    let mut parts = tail.split(':').filter(|s| !s.is_empty()).map(str::to_string);
    Some((item, parts.next(), parts.next()))
}

/// One tile matrix of the pyramid, as a level of the raster.
#[derive(Debug, Clone)]
struct Level {
    /// The tile matrix's identifier, for `{TileMatrix}`.
    id: String,
    res_x: f64,
    res_y: f64,
    tile_w: usize,
    tile_h: usize,
    /// The tiles that exist: columns and rows, inclusive.
    cols: (i64, i64),
    rows: (i64, i64),
    /// The raster's origin in this matrix's pixels, from the matrix's top-left corner.
    off_x: i64,
    off_y: i64,
    width: usize,
    height: usize,
}

/// A STACTA raster read by window: the tiles a window touches are opened and read.
struct Tiled {
    template: String,
    tms: String,
    levels: Vec<Level>,
    data_type: DataType,
    nodata: Option<f64>,
    skip_missing: bool,
    /// Tiles opened, by level and tile, reused by later windows.
    open: Mutex<HashMap<(usize, i64, i64), LazyRaster>>,
}

/// The EPSG CRS of a tile matrix set's `supportedCRS` (`.../def/crs/EPSG/0/3857`, CRS84).
fn crs_of(v: &Value) -> SpatialRef {
    let uri = v.as_str().map(str::to_string).or_else(|| v["uri"].as_str().map(str::to_string)).unwrap_or_default();
    let upper = uri.to_ascii_uppercase();
    if upper.ends_with("CRS84") || upper.ends_with("OGC/1.3/CRS84") {
        return SpatialRef::from_epsg(4326).unwrap_or_default();
    }
    if let Some(code) = uri.rsplit('/').next().and_then(|c| c.parse::<u32>().ok())
        && upper.contains("EPSG")
    {
        return SpatialRef::from_epsg(code).unwrap_or_default();
    }
    SpatialRef::parse(&uri).unwrap_or_default()
}

/// A tile's href: the template with its variables replaced, resolved against the item.
fn tile_href(template: &str, item_loc: &str, tms: &str, level: &str, row: i64, col: i64) -> String {
    let href = template
        .replace("{TileMatrixSet}", tms)
        .replace("{TileMatrix}", level)
        .replace("{TileRow}", &row.to_string())
        .replace("{TileCol}", &col.to_string());
    crate::stac::resolve_href(item_loc, &href)
}

/// Whether an error is a missing tile (a 404, or no such file).
fn is_missing(e: &Error) -> bool {
    match e {
        Error::Io(io) => io.kind() == std::io::ErrorKind::NotFound,
        Error::Http(m) => m.contains("status 404") || m.contains("status 403"),
        _ => false,
    }
}

impl Tiled {
    /// Reads a window of a tile (numbered in its matrix) of a level, or `None` when the tile is
    /// missing and missing tiles are skipped.
    fn read_tile(&self, level: usize, row: i64, col: i64, w: Window, bands: &[usize]) -> Result<Option<RasterData>> {
        let key = (level, row, col);
        let cached = self.open.lock().ok().and_then(|mut m| m.remove(&key));
        let mut raster = match cached {
            Some(r) => r,
            None => match LazyRaster::open(&self.template_href(level, row, col)) {
                Ok(r) => r,
                Err(e) if self.skip_missing && is_missing(&e) => return Ok(None),
                Err(e) if is_missing(&e) => {
                    return Err(bad(format!(
                        "tile {}/{row}/{col} is missing: {e} (set GDAL_STACTA_SKIP_MISSING_METATILE=YES to read it as nodata)",
                        self.levels[level].id
                    )));
                }
                Err(e) => return Err(e),
            },
        };
        let out = raster.read_window(w, &WindowOptions { bands: bands.to_vec(), overview: None })?;
        if let Ok(mut m) = self.open.lock() {
            // A few hundred tiles at most stay open.
            if m.len() > 256 {
                m.clear();
            }
            m.insert(key, raster);
        }
        Ok(Some(out.raster.data))
    }

    fn template_href(&self, level: usize, row: i64, col: i64) -> String {
        tile_href(&self.template, "", &self.tms, &self.levels[level].id, row, col)
    }
}

impl WindowSource for Tiled {
    fn overviews(&self) -> Vec<(usize, usize)> {
        self.levels[1..].iter().map(|l| (l.width, l.height)).collect()
    }

    fn block_size(&self) -> Option<(usize, usize)> {
        Some((self.levels[0].tile_w, self.levels[0].tile_h))
    }

    fn read(&mut self, level: usize, w: Window, bands: &[usize]) -> Result<RasterData> {
        let l = self.levels.get(level).cloned().ok_or_else(|| bad(format!("no overview {}", level.saturating_sub(1))))?;
        if !w.fits(l.width, l.height) {
            return Err(bad(format!("window outside the {}x{} level", l.width, l.height)));
        }
        let (tw, th) = (l.tile_w as i64, l.tile_h as i64);
        // The window in the matrix's pixels.
        let (ax0, ay0) = (l.off_x + w.x as i64, l.off_y + w.y as i64);
        let (ax1, ay1) = (ax0 + w.width as i64, ay0 + w.height as i64);
        let mut tiles = Vec::new();
        for row in ay0.div_euclid(th)..=(ay1 - 1).div_euclid(th) {
            for col in ax0.div_euclid(tw)..=(ax1 - 1).div_euclid(tw) {
                if (l.cols.0..=l.cols.1).contains(&col) && (l.rows.0..=l.rows.1).contains(&row) {
                    tiles.push((row, col));
                }
            }
        }
        let this: &Tiled = self;
        let reads = crate::par::map_range(tiles.len(), |i| {
            let (row, col) = tiles[i];
            // The part of the tile in the window, in the tile's pixels.
            let (tx0, ty0) = ((ax0 - col * tw).max(0), (ay0 - row * th).max(0));
            let (tx1, ty1) = ((ax1 - col * tw).min(tw), (ay1 - row * th).min(th));
            let sub = Window::new(tx0 as usize, ty0 as usize, (tx1 - tx0) as usize, (ty1 - ty0) as usize);
            this.read_tile(level, row, col, sub, bands).map(|d| (row, col, sub, d))
        });
        let n = w.width * w.height;
        let mut out = vec![self.nodata.unwrap_or(0.0); n * bands.len()];
        for r in reads {
            let (row, col, sub, data) = r?;
            let Some(data) = data else { continue };
            let values = data.to_f64_vec();
            // The tile part's position in the window.
            let (dx, dy) = ((col * tw + sub.x as i64 - ax0) as usize, (row * th + sub.y as i64 - ay0) as usize);
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

/// Opens a STACTA dataset name or item lazily. `None` when `path` is neither.
pub(crate) fn open_lazy(path: &str) -> Option<Result<LazyRaster>> {
    let (item, asset, tms) = match parse_name(path) {
        Some(named) => named,
        None => {
            let lower = path.split(['?', '#']).next().unwrap_or(path).to_ascii_lowercase();
            if !lower.ends_with(".json") {
                return None;
            }
            // A local file is looked at only when its first bytes name tiled assets.
            if let crate::vfs::Location::Local(p) = crate::vfs::Location::parse(path) {
                let mut head = vec![0u8; 65536];
                let n = std::fs::File::open(p).and_then(|mut f| std::io::Read::read(&mut f, &mut head)).ok()?;
                if !String::from_utf8_lossy(&head[..n]).contains("tiles:tile_matrix_sets") {
                    return None;
                }
            } else {
                return None;
            }
            (path.to_string(), None, None)
        }
    };
    Some(open_item(&item, asset.as_deref(), tms.as_deref()))
}

/// A tile matrix of a set: its resolution, tile size, top left corner, size in tiles and, when
/// the item limits it, its first and last tile columns and rows.
struct Matrix {
    id: String,
    res: f64,
    tile_w: usize,
    tile_h: usize,
    corner: [f64; 2],
    cols: i64,
    rows: i64,
    limit: Option<(i64, i64, i64, i64)>,
}

impl Matrix {
    /// The first and last tile columns, then rows.
    fn range(&self) -> (i64, i64, i64, i64) {
        self.limit.unwrap_or((0, self.cols - 1, 0, self.rows - 1))
    }
}

/// Opens the tiled asset of an item: `asset` and `tms` choose among several (by default the
/// first of each).
fn open_item(item_loc: &str, asset: Option<&str>, tms: Option<&str>) -> Result<LazyRaster> {
    let doc: Value = serde_json::from_slice(&crate::stac::fetch(item_loc)?).map_err(|e| bad(format!("{item_loc}: {e}")))?;
    if !is_stacta(&doc) {
        return Err(bad(format!("'{item_loc}' is not a STAC item with tiled assets")));
    }
    let templates = doc["asset_templates"].as_object().ok_or_else(|| bad("no asset_templates"))?;
    let (_, template) = match asset {
        Some(a) => templates.iter().find(|(k, _)| k.as_str() == a).ok_or_else(|| bad(format!("no asset template '{a}'")))?,
        None => templates.iter().next().ok_or_else(|| bad("no asset template"))?,
    };
    let href = template["href"].as_str().ok_or_else(|| bad("an asset template without href"))?;
    let template = crate::stac::resolve_href(item_loc, href);
    let sets = doc["properties"]["tiles:tile_matrix_sets"].as_object().ok_or_else(|| bad("no tile matrix sets"))?;
    let (tms_id, set) = match tms {
        Some(t) => sets.iter().find(|(k, _)| k.as_str() == t).ok_or_else(|| bad(format!("no tile matrix set '{t}'")))?,
        None => sets.iter().next().ok_or_else(|| bad("no tile matrix set"))?,
    };
    let limits = &doc["properties"]["tiles:tile_matrix_links"][tms_id.as_str()]["limits"];
    let crs = crs_of(&set["supportedCRS"]);
    // Metres (or degrees) per pixel of a tile matrix: its scale denominator, with the OGC
    // standard pixel of 0.28 mm, in the CRS's units.
    let per_unit = if crs.epsg == Some(4326) { 2.0 * std::f64::consts::PI * 6_378_137.0 / 360.0 } else { 1.0 };
    let mut matrices: Vec<Matrix> = Vec::new();
    for m in set["tileMatrix"].as_array().into_iter().flatten() {
        let id = m["identifier"].as_str().map(str::to_string).or_else(|| m["id"].as_str().map(str::to_string)).unwrap_or_default();
        let res = m["scaleDenominator"].as_f64().ok_or_else(|| bad("a tile matrix without scaleDenominator"))? * 0.00028 / per_unit;
        let tlc = m["topLeftCorner"]
            .as_array()
            .or_else(|| m["pointOfOrigin"].as_array())
            .and_then(|a| Some([a.first()?.as_f64()?, a.get(1)?.as_f64()?]))
            .ok_or_else(|| bad("a tile matrix without topLeftCorner"))?;
        let get = |k: &str| m[k].as_u64().map(|v| v as usize);
        let (tw, th) = (get("tileWidth").unwrap_or(256), get("tileHeight").unwrap_or(256));
        let (mw, mh) = (get("matrixWidth").unwrap_or(1) as i64, get("matrixHeight").unwrap_or(1) as i64);
        let lim = &limits[id.as_str()];
        let limit = (lim.is_object()).then(|| {
            let g = |k: &str| lim[k].as_i64().unwrap_or(0);
            (g("min_tile_col"), g("max_tile_col"), g("min_tile_row"), g("max_tile_row"))
        });
        // With limits, only the tile matrices listed exist.
        if limits.is_object() && limit.is_none() {
            continue;
        }
        matrices.push(Matrix { id, res, tile_w: tw, tile_h: th, corner: tlc, cols: mw, rows: mh, limit });
    }
    // From the most detailed.
    matrices.sort_by(|a, b| a.res.total_cmp(&b.res));
    let m0 = matrices.first().ok_or_else(|| bad("no tile matrix"))?;
    let (c0, c1, r0, r1) = m0.range();
    let (res0, tlc0) = (m0.res, m0.corner);
    let (x0, y0) = (tlc0[0] + (c0 * m0.tile_w as i64) as f64 * res0, tlc0[1] - (r0 * m0.tile_h as i64) as f64 * res0);
    let (width, height) = (((c1 - c0 + 1) * m0.tile_w as i64) as usize, ((r1 - r0 + 1) * m0.tile_h as i64) as usize);
    let levels: Vec<Level> = matrices
        .iter()
        .map(|m| {
            let (c0, c1, r0, r1) = m.range();
            let f = m.res / res0;
            Level {
                id: m.id.clone(),
                res_x: m.res,
                res_y: m.res,
                tile_w: m.tile_w,
                tile_h: m.tile_h,
                cols: (c0, c1),
                rows: (r0, r1),
                off_x: ((x0 - m.corner[0]) / m.res).round() as i64,
                off_y: ((m.corner[1] - y0) / m.res).round() as i64,
                width: ((width as f64 / f).round() as usize).max(1),
                height: ((height as f64 / f).round() as usize).max(1),
            }
        })
        .collect();
    let env_yes = |k: &str| std::env::var(k).is_ok_and(|v| matches!(v.trim().to_ascii_uppercase().as_str(), "YES" | "ON" | "TRUE" | "1"));
    let skip_missing = env_yes("GDAL_STACTA_SKIP_MISSING_METATILE") || env_yes("TERRIO_STACTA_SKIP_MISSING_TILE");
    // The first tile tells the band count, data type and nodata value.
    let first_href = tile_href(&template, "", tms_id, &levels[0].id, levels[0].rows.0, levels[0].cols.0);
    let first = LazyRaster::open(&first_href).map_err(|e| bad(format!("its first tile {first_href}: {e}")))?;
    let (bands, data_type, nodata) = (first.band_count(), first.data_type(), first.nodata());
    let raster = wbraster::Raster {
        cols: width,
        rows: height,
        bands,
        x_min: x0,
        y_min: y0 - height as f64 * res0,
        cell_size_x: levels[0].res_x,
        cell_size_y: levels[0].res_y,
        nodata: nodata.unwrap_or(-9999.0),
        data_type,
        crs: crs.to_crs_info(),
        metadata: Vec::new(),
        data: RasterData::new_filled(data_type, 0, 0.0),
    };
    let header = RasterDataset::with_driver(raster, RasterDriver::STACTA, nodata.is_some());
    let mut open = HashMap::new();
    open.insert((0, levels[0].rows.0, levels[0].cols.0), first);
    let source = Tiled { template, tms: tms_id.clone(), levels, data_type, nodata, skip_missing, open: Mutex::new(open) };
    Ok(LazyRaster::from_source(header, Box::new(source)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_and_templates() {
        assert_eq!(
            parse_name(r#"STACTA:"https://x.org/a.json":dem:WebMercatorQuad"#),
            Some(("https://x.org/a.json".into(), Some("dem".into()), Some("WebMercatorQuad".into())))
        );
        assert_eq!(parse_name("STACTA:a.json"), Some(("a.json".into(), None, None)));
        assert_eq!(parse_name("a.json"), None);
        assert_eq!(
            tile_href("./{TileMatrixSet}/{TileMatrix}/{TileRow}/{TileCol}.tif", "https://x.org/c/item.json", "WGS", "2", 0, 1),
            "https://x.org/c/WGS/2/0/1.tif"
        );
        assert_eq!(crs_of(&serde_json::json!("http://www.opengis.net/def/crs/EPSG/0/3857")).epsg, Some(3857));
        assert_eq!(crs_of(&serde_json::json!("http://www.opengis.net/def/crs/OGC/1.3/CRS84")).epsg, Some(4326));
    }
}

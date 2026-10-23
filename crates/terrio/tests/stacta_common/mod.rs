//! Pyramids of GeoTIFF tiles and STAC items of tiled assets for the STACTA tests.

#![allow(dead_code)]

use serde_json::{Value, json};
use std::path::Path;
use terrio::RasterDataset;

/// A tile of `size` x `size` pixels whose values encode its matrix, row and column.
pub fn tile(size: usize, matrix: u32, row: u32, col: u32) -> RasterDataset {
    let cfg = terrio::RasterConfig {
        cols: size,
        rows: size,
        bands: 1,
        x_min: 0.0,
        y_min: 0.0,
        cell_size: 1.0,
        nodata: -1.0,
        data_type: terrio::DataType::I32,
        crs: terrio::crs::SpatialRef::from_epsg(4326).unwrap().to_crs_info(),
        ..Default::default()
    };
    let values = (0..size * size).map(|i| (matrix * 1_000_000 + row * 10_000 + col * 1000) as i32 + i as i32).collect();
    RasterDataset::new(terrio::Raster::from_data_native(cfg, terrio::raster::RasterData::I32(values)).unwrap(), true)
}

/// Writes the tiles `{dir}/{prefix}{matrix}/{row}/{col}.tif` of 16 x 16 pixels.
pub fn write_tiles(dir: &Path, prefix: &str, tiles: &[(u32, u32, u32)]) {
    for &(m, r, c) in tiles {
        let p = dir.join(format!("{prefix}{m}/{r}/{c}.tif"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        terrio::write_raster_dataset(&tile(16, m, r, c), p.to_str().unwrap(), &Default::default()).unwrap();
    }
}

/// A tile matrix set like WorldCRS84Quad with 16-pixel tiles: matrix 0 is one tile over the
/// west half of the world (11.25 degrees per pixel), matrix 1 two by two tiles over it.
pub fn quad() -> Value {
    // Scale denominators of the resolutions, as GDAL converts them back (0.28 mm pixels, metres
    // per degree on the WGS 84 semi-major axis).
    let deg = 2.0 * std::f64::consts::PI * 6_378_137.0 / 360.0;
    let scale = |res: f64| res * deg / 0.00028;
    let m = |id: &str, res: f64, n: u32| {
        json!({"identifier": id, "scaleDenominator": scale(res), "topLeftCorner": [-180, 90], "tileWidth": 16, "tileHeight": 16,
               "matrixWidth": n, "matrixHeight": n})
    };
    json!({"type": "TileMatrixSetType", "identifier": "Quad", "supportedCRS": "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
           "tileMatrix": [m("0", 180.0 / 16.0, 1), m("1", 90.0 / 16.0, 2)]})
}

/// An item of tiled assets, with limits on its tile matrices when given.
pub fn item(limits: Option<Value>, templates: Value) -> Value {
    let mut props = json!({
        "datetime": "2024-01-01T00:00:00Z", "gsd": 10.0, "instruments": ["a", "b/c"], "proj:epsg": 4326,
        "tiles:tile_matrix_sets": {"Quad": quad()}
    });
    if let Some(l) = limits {
        props["tiles:tile_matrix_links"] = json!({"Quad": {"url": "#", "limits": l}});
    }
    json!({"stac_version": "1.0.0", "stac_extensions": ["tiled-assets"], "id": "t", "type": "Feature",
           "bbox": [-180, -90, 0, 90], "geometry": null, "properties": props, "asset_templates": templates})
}

pub fn write_item(dir: &Path, name: &str, item: &Value) -> String {
    let p = dir.join(name);
    std::fs::write(&p, serde_json::to_vec_pretty(item).unwrap()).unwrap();
    p.to_str().unwrap().to_string()
}

pub fn read(r: &mut terrio::LazyRaster, w: terrio::Window, overview: Option<usize>) -> Vec<f64> {
    r.read_window(w, &terrio::WindowOptions { bands: Vec::new(), overview }).unwrap().raster.data.to_f64_vec()
}

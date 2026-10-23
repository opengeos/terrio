//! STAC items with tiled assets (STACTA): pyramids of GeoTIFF tiles written here, read by window
//! at full resolution and from their overviews, as GDAL's STACTA driver reads them.

use serde_json::{Value, json};
use std::path::Path;
use terrio::RasterDataset;

/// A tile of `size` x `size` pixels whose values encode its matrix, row and column.
fn tile(size: usize, matrix: u32, row: u32, col: u32) -> RasterDataset {
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
fn write_tiles(dir: &Path, prefix: &str, tiles: &[(u32, u32, u32)]) {
    for &(m, r, c) in tiles {
        let p = dir.join(format!("{prefix}{m}/{r}/{c}.tif"));
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        terrio::write_raster_dataset(&tile(16, m, r, c), p.to_str().unwrap(), &Default::default()).unwrap();
    }
}

/// A tile matrix set like WorldCRS84Quad with 16-pixel tiles: matrix 0 is one tile over the
/// west half of the world (11.25 degrees per pixel), matrix 1 two by two tiles over it.
fn quad() -> Value {
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
fn item(limits: Option<Value>, templates: Value) -> Value {
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

fn write_item(dir: &Path, name: &str, item: &Value) -> String {
    let p = dir.join(name);
    std::fs::write(&p, serde_json::to_vec_pretty(item).unwrap()).unwrap();
    p.to_str().unwrap().to_string()
}

fn read(r: &mut terrio::LazyRaster, w: terrio::Window, overview: Option<usize>) -> Vec<f64> {
    r.read_window(w, &terrio::WindowOptions { bands: Vec::new(), overview }).unwrap().raster.data.to_f64_vec()
}

#[test]
fn reads_a_tiled_asset_pyramid() {
    let dir = tempfile::tempdir().unwrap();
    write_tiles(dir.path(), "Quad/", &[(0, 0, 0), (1, 0, 0), (1, 0, 1), (1, 1, 0), (1, 1, 1)]);
    let limits = json!({
        "0": {"min_tile_col": 0, "max_tile_col": 0, "min_tile_row": 0, "max_tile_row": 0},
        "1": {"min_tile_col": 0, "max_tile_col": 1, "min_tile_row": 0, "max_tile_row": 1}});
    let templates = json!({"data": {"href": "./{TileMatrixSet}/{TileMatrix}/{TileRow}/{TileCol}.tif"}});
    let json = write_item(dir.path(), "item.json", &item(Some(limits), templates));
    let mut r = terrio::open_raster_lazy(&json).unwrap();
    assert_eq!(r.driver().name(), "STACTA");
    assert_eq!((r.width(), r.height(), r.band_count()), (32, 32, 1));
    assert_eq!((r.data_type(), r.nodata()), (terrio::DataType::I32, Some(-1.0)));
    assert_eq!(r.overviews(), vec![(16, 16)]);
    let gt = r.geo_transform();
    assert!((gt[0] + 180.0).abs() < 1e-9 && (gt[3] - 90.0).abs() < 1e-9 && (gt[1] - 90.0 / 16.0).abs() < 1e-12, "{gt:?}");
    assert_eq!(r.crs().epsg, Some(4326));
    // The properties, as GDAL writes them, but for the tiles: and proj: ones.
    let md = r.metadata();
    let get = |k: &str| md.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert_eq!(get("gsd").as_deref(), Some("10.0"));
    assert_eq!(get("instruments").as_deref(), Some(r#"[ "a", "b\/c" ]"#));
    assert_eq!(get("proj:epsg"), None);
    assert_eq!(get("tiles:tile_matrix_sets"), None);
    // A window across the four full-resolution tiles: each pixel is its tile's.
    let w = read(&mut r, terrio::Window::new(14, 14, 4, 4), None);
    let at = |row: usize, col: usize| w[row * 4 + col];
    assert_eq!(at(0, 0), (1_000_000 + 14 * 16 + 14) as f64); // tile 1/0/0, pixel (14, 14)
    assert_eq!(at(0, 2), (1_000_000 + 1000 + 14 * 16) as f64); // tile 1/0/1, pixel (14, 0)
    assert_eq!(at(2, 0), (1_000_000 + 10_000 + 14) as f64); // tile 1/1/0, pixel (0, 14)
    assert_eq!(at(3, 3), (1_000_000 + 10_000 + 1000 + 16 + 1) as f64); // tile 1/1/1, pixel (1, 1)
    // The overview: matrix 0.
    let ov = read(&mut r, terrio::Window::new(0, 0, 16, 16), Some(0));
    assert_eq!((ov[0], ov[255]), (0.0, 255.0));
    // The whole raster.
    let all = r.read().unwrap();
    assert_eq!(all.raster.data.to_f64_vec()[31], (1_000_000 + 1000 + 15) as f64);
    // Named with an asset and a tile matrix set, as GDAL names them.
    assert_eq!(terrio::open_raster_lazy(&format!("STACTA:\"{json}\":data:Quad")).unwrap().width(), 32);
    assert!(terrio::open_raster_lazy(&format!("STACTA:\"{json}\":nothing")).is_err());
    // terrio info.
    let terrio::info::Info::Raster(info) = terrio::info::info_path(&json, &Default::default()).unwrap() else { panic!("not a raster") };
    assert_eq!((info.driver.as_str(), info.size, info.overviews.clone()), ("STACTA", [32, 32], vec![[16, 16]]));
}

#[test]
fn cuts_overviews_to_the_raster_extent() {
    // The full resolution is limited to the east column of matrix 1 (90 W to 0); matrix 0 covers
    // 180 W to 0: its overview is cut to the raster's extent, as `gdal_translate -projwin` cuts
    // it in GDAL's driver.
    let dir = tempfile::tempdir().unwrap();
    write_tiles(dir.path(), "", &[(0, 0, 0), (1, 0, 1), (1, 1, 1)]);
    let limits = json!({"1": {"min_tile_col": 1, "max_tile_col": 1, "min_tile_row": 0, "max_tile_row": 1}});
    let json = write_item(dir.path(), "item.json", &item(Some(limits), json!({"data": {"href": "{TileMatrix}/{TileRow}/{TileCol}.tif"}})));
    let mut r = terrio::open_raster_lazy(&json).unwrap();
    assert_eq!((r.width(), r.height()), (16, 32));
    assert!((r.geo_transform()[0] + 90.0).abs() < 1e-9);
    assert_eq!(r.overviews(), vec![(8, 16)]);
    // Columns 8 to 15 of tile 0/0/0.
    let ov = read(&mut r, terrio::Window::new(0, 0, 8, 16), Some(0));
    assert_eq!((ov[0], ov[7], ov[8]), (8.0, 15.0, 24.0));
    let w = read(&mut r, terrio::Window::new(0, 15, 1, 2), None);
    assert_eq!(w, vec![(1_000_000 + 1000 + 15 * 16) as f64, (1_010_000 + 1000) as f64]);
}

#[test]
fn reads_missing_tiles_as_an_error_or_nodata() {
    let dir = tempfile::tempdir().unwrap();
    // Tile 1/1/1 is missing.
    write_tiles(dir.path(), "", &[(0, 0, 0), (1, 0, 0), (1, 0, 1), (1, 1, 0)]);
    let json = write_item(dir.path(), "item.json", &item(None, json!({"data": {"href": "./{TileMatrix}/{TileRow}/{TileCol}.tif"}})));
    let mut r = terrio::open_raster_lazy(&json).unwrap();
    let err = r.read_window(terrio::Window::new(0, 0, 32, 32), &Default::default()).unwrap_err().to_string();
    assert!(err.contains("Cannot open") && err.contains("1/1/1.tif"), "{err}");
    // A window of tiles that exist reads.
    assert_eq!(read(&mut r, terrio::Window::new(0, 0, 2, 1), None), vec![1_000_000.0, 1_000_001.0]);
    // SAFETY: no other test reads this variable, and each opens its own item.
    unsafe { std::env::set_var("GDAL_STACTA_SKIP_MISSING_METATILE", "YES") };
    let opened = terrio::open_raster_lazy(&json);
    unsafe { std::env::remove_var("GDAL_STACTA_SKIP_MISSING_METATILE") };
    let mut r = opened.unwrap();
    let w = read(&mut r, terrio::Window::new(15, 15, 2, 2), None);
    assert_eq!(w, vec![(1_000_000 + 15 * 16 + 15) as f64, (1_001_000 + 15 * 16) as f64, (1_010_000 + 15) as f64, -1.0]);
}

#[test]
fn lists_subdatasets_and_reads_raster_bands() {
    let dir = tempfile::tempdir().unwrap();
    write_tiles(dir.path(), "a/", &[(0, 0, 0), (1, 0, 0), (1, 0, 1), (1, 1, 0), (1, 1, 1)]);
    write_tiles(dir.path(), "b/", &[(0, 0, 0), (1, 0, 0), (1, 0, 1), (1, 1, 0), (1, 1, 1)]);
    let templates = json!({
        "first": {"href": "./a/{TileMatrix}/{TileRow}/{TileCol}.tif"},
        "second": {"href": "./b/{TileMatrix}/{TileRow}/{TileCol}.tif", "raster:bands": [{"data_type": "float64", "nodata": "nan"}]}
    });
    let json = write_item(dir.path(), "item.json", &item(None, templates));
    let terrio::info::Info::Raster(info) = terrio::info::info_path(&json, &Default::default()).unwrap() else { panic!("not a raster") };
    let subs: Vec<(String, String)> = info.subdatasets.iter().map(|s| (s.name.clone(), s.description.clone())).collect();
    assert_eq!(
        subs,
        vec![
            (format!("STACTA:\"{json}\":first"), "Asset first".to_string()),
            (format!("STACTA:\"{json}\":second"), "Asset second".to_string())
        ]
    );
    // The first subdataset opens.
    assert_eq!(info.size, [32, 32]);
    // The second: its data type and nodata value are the template's raster:bands'.
    let r = terrio::open_raster_lazy(&subs[1].0).unwrap();
    assert_eq!(r.data_type(), terrio::DataType::F64);
    assert!(r.nodata().is_some_and(f64::is_nan));
    let ds = terrio::open_raster(&subs[1].0).unwrap();
    assert_eq!(ds.raster.data.to_f64_vec()[0], 1_000_000.0);
}

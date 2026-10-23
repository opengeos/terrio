//! STAC items with tiled assets (STACTA): pyramids of GeoTIFF tiles written here, read by window
//! at full resolution and from their overviews, as GDAL's STACTA driver reads them.

mod stacta_common;

use serde_json::json;
use stacta_common::{item, read, write_item, write_tiles};

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

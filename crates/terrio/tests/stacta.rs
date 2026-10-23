//! STAC items with tiled assets (STACTA): a pyramid of GeoTIFF tiles written here, read by window
//! at full resolution and from its overview, as GDAL's STACTA driver reads it.

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

#[test]
fn reads_a_tiled_asset_pyramid() {
    let dir = tempfile::tempdir().unwrap();
    // WorldCRS84Quad-like: matrix 0 is 1 x 1 tile of 16 x 16 pixels over the west half, matrix
    // 1 is 2 x 2 tiles (a quarter each) over the same.
    for (m, rows, cols) in [(0u32, 1u32, 1u32), (1, 2, 2)] {
        for r in 0..rows {
            for c in 0..cols {
                let p = dir.path().join(format!("{m}/{r}/{c}.tif"));
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                terrio::write_raster_dataset(&tile(16, m, r, c), p.to_str().unwrap(), &Default::default()).unwrap();
            }
        }
    }
    // Scale denominators of 16-pixel tiles of 90 degrees (matrix 0) and 45 degrees (matrix 1).
    let deg = 2.0 * std::f64::consts::PI * 6_378_137.0 / 360.0;
    let scale = |res: f64| res * deg / 0.00028;
    let item = serde_json::json!({
        "stac_version": "1.0.0", "stac_extensions": ["tiled-assets"], "id": "t", "type": "Feature",
        "bbox": [-180, -90, 0, 90], "geometry": null,
        "properties": {
            "datetime": "2024-01-01T00:00:00Z",
            "tiles:tile_matrix_links": {"Quad": {"url": "#", "limits": {
                "0": {"min_tile_col": 0, "max_tile_col": 0, "min_tile_row": 0, "max_tile_row": 0},
                "1": {"min_tile_col": 0, "max_tile_col": 1, "min_tile_row": 0, "max_tile_row": 1}}}},
            "tiles:tile_matrix_sets": {"Quad": {"type": "TileMatrixSetType", "identifier": "Quad",
                "supportedCRS": "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
                "tileMatrix": [
                    {"identifier": "0", "scaleDenominator": scale(180.0 / 16.0), "topLeftCorner": [-180, 90], "tileWidth": 16, "tileHeight": 16, "matrixWidth": 1, "matrixHeight": 1},
                    {"identifier": "1", "scaleDenominator": scale(90.0 / 16.0), "topLeftCorner": [-180, 90], "tileWidth": 16, "tileHeight": 16, "matrixWidth": 2, "matrixHeight": 2}]}}},
        "asset_templates": {"data": {"href": "./{TileMatrix}/{TileRow}/{TileCol}.tif"}}
    });
    let json = dir.path().join("item.json");
    std::fs::write(&json, serde_json::to_vec(&item).unwrap()).unwrap();
    let mut r = terrio::open_raster_lazy(json.to_str().unwrap()).unwrap();
    assert_eq!((r.width(), r.height(), r.band_count()), (32, 32, 1));
    assert_eq!(r.overviews(), vec![(16, 16)]);
    let gt = r.geo_transform();
    assert!((gt[0] + 180.0).abs() < 1e-9 && (gt[3] - 90.0).abs() < 1e-9 && (gt[1] - 90.0 / 16.0).abs() < 1e-9, "{gt:?}");
    // A window across the four full-resolution tiles: each pixel is its tile's.
    let w = r.read_window(terrio::Window::new(14, 14, 4, 4), &Default::default()).unwrap().raster.data.to_f64_vec();
    let at = |row: usize, col: usize| w[row * 4 + col];
    assert_eq!(at(0, 0), (1_000_000 + 14 * 16 + 14) as f64); // tile 1/0/0, pixel (14, 14)
    assert_eq!(at(0, 2), (1_000_000 + 1000 + 14 * 16) as f64); // tile 1/0/1, pixel (14, 0)
    assert_eq!(at(2, 0), (1_000_000 + 10_000 + 14) as f64); // tile 1/1/0, pixel (0, 14)
    assert_eq!(at(3, 3), (1_000_000 + 10_000 + 1000 + 16 + 1) as f64); // tile 1/1/1, pixel (1, 1)
    // The overview: matrix 0.
    let o = terrio::WindowOptions { bands: Vec::new(), overview: Some(0) };
    let ov = r.read_window(terrio::Window::new(0, 0, 16, 16), &o).unwrap().raster.data.to_f64_vec();
    assert_eq!(ov[0], 0.0);
    assert_eq!(ov[255], 255.0);
    // Named with an asset and a tile matrix set, as GDAL names them.
    let named = format!("STACTA:\"{}\":data:Quad", json.display());
    assert_eq!(terrio::open_raster_lazy(&named).unwrap().width(), 32);
}

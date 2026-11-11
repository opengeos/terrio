//! `terrio warp` into a polar grid whose interior holds the pole writes every pixel its source
//! covers, up to the pole (#825).

use std::process::Command;

/// A longitude and latitude source of 1° pixels from 90° N to 30° N, at every longitude, each
/// holding its row number plus one, warped by nearest neighbour into NSIDC's 25 km north grid
/// (EPSG:3413, 304 x 448 pixels), which holds the North Pole and which the source covers whole:
/// no pixel is left at 0.
#[test]
fn a_warp_into_a_polar_grid_holding_the_pole_writes_every_pixel() {
    let dir = tempfile::tempdir().unwrap();
    let (arctic, out) = (dir.path().join("arctic.tif"), dir.path().join("out.tif"));
    let cfg = terrio::RasterConfig {
        cols: 360,
        rows: 60,
        bands: 1,
        x_min: -180.,
        y_min: 30.,
        cell_size: 1.,
        data_type: terrio::DataType::U8,
        ..Default::default()
    };
    let values = (0..360 * 60).map(|i| (i / 360 + 1) as f64).collect();
    let mut src = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, values).unwrap(), false);
    src.raster.crs.epsg = Some(4326);
    terrio::write_raster_dataset(&src, arctic.to_str().unwrap(), &Default::default()).unwrap();
    let args = ["--t-srs", "EPSG:3413", "--te", "-3850000", "-5350000", "3750000", "5850000", "--tr", "25000", "25000", "-r", "near"];
    let result = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("warp").arg(&arctic).arg(&out).args(args).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let warped = terrio::open_raster(out.to_str().unwrap()).unwrap();
    let pixels = warped.raster.data.to_f64_vec();
    let zeros = pixels.iter().filter(|v| **v == 0.0).count();
    assert_eq!((warped.raster.cols, warped.raster.rows, zeros), (304, 448, 0), "pixels left at 0");
}

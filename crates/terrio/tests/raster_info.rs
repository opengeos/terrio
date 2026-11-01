//! `terrio info` of rasters whose grids are not north-up. The expected corners are those of
//! `gdalinfo -json` (GDAL 3.13.3) on the same geotransform.

use terrio::info::{Info, InfoOptions, info_path, raster_info};
use terrio::{DataType, Raster, RasterConfig, RasterDataset, RasterWriteOptions, write_raster_dataset};

/// A 10 x 20 Byte raster in UTM zone 13N, with a given geotransform.
fn dataset(gt: [f64; 6]) -> RasterDataset {
    let cfg = RasterConfig {
        cols: 10,
        rows: 20,
        bands: 1,
        data_type: DataType::U8,
        crs: wbraster::CrsInfo::from_epsg(32613),
        ..Default::default()
    };
    let mut ds = RasterDataset::new(Raster::new(cfg), false);
    ds.set_geo_transform(gt);
    ds
}

const ROTATED: [f64; 6] = [500000.0, 30.0, 10.0, 4400000.0, 10.0, -30.0];

/// The box of `gdalinfo -json`'s `wgs84Extent` for `ROTATED`, which it prints to 7 decimals.
fn assert_wgs84_extent(extent: Option<[f64; 4]>) {
    let expected = [-105.0, 39.7445014, -104.9941641, 39.7508085];
    let extent = extent.unwrap();
    assert!(extent.iter().zip(expected).all(|(a, b)| (a - b).abs() < 1e-7), "{extent:?}");
}

#[test]
fn rotated_extent_covers_every_corner() {
    let ds = dataset(ROTATED);
    let expected = [500000.0, 4399400.0, 500500.0, 4400100.0];
    let eager = raster_info(&ds, &InfoOptions::default());
    assert_eq!(eager.extent, expected);
    assert_wgs84_extent(eager.wgs84_extent);

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rotated.tif");
    let path = path.to_str().unwrap();
    write_raster_dataset(&ds, path, &RasterWriteOptions::default()).unwrap();
    let Info::Raster(info) = info_path(path, &InfoOptions::default()).unwrap() else { panic!("a raster") };
    assert_eq!(info.geo_transform, ROTATED);
    assert_eq!(info.extent, expected);
    assert_wgs84_extent(info.wgs84_extent);

    let text = Info::Raster(info).to_text();
    assert!(text.contains("Geotransform: (500000, 30, 10, 4400000, 10, -30)"), "{text}");
    assert!(!text.contains("Pixel size"), "{text}");
}

#[test]
fn south_up_extent_is_ordered() {
    let gt = [500000.0, 30.0, 0.0, 4400000.0, 0.0, 30.0];
    assert_eq!(raster_info(&dataset(gt), &InfoOptions::default()).extent, [500000.0, 4400000.0, 500300.0, 4400600.0]);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("south_up.tif");
    let path = path.to_str().unwrap();
    write_raster_dataset(&dataset(gt), path, &RasterWriteOptions::default()).unwrap();
    let Info::Raster(info) = info_path(path, &InfoOptions::default()).unwrap() else { panic!("a raster") };
    assert_eq!(info.extent, [500000.0, 4400000.0, 500300.0, 4400600.0]);
    assert!(Info::Raster(info).to_text().contains("Pixel size: (30, 30)"));
}

#[test]
fn north_up_extent_is_unchanged() {
    let gt = [500000.0, 30.0, 0.0, 4400000.0, 0.0, -30.0];
    let info = raster_info(&dataset(gt), &InfoOptions::default());
    assert_eq!(info.extent, [500000.0, 4399400.0, 500300.0, 4400000.0]);
}

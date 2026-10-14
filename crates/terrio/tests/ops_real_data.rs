//! Raster operations on real-world files, with expected values taken from GDAL 3.12
//! (`gdalwarp`, `gdal_translate`, `gdalbuildvrt`).
//!
//! The files live outside the repository: set `TERRIO_TEST_DATA` to their directory (see
//! `scripts/fetch_test_data.sh`). Without it, these tests pass without checking anything.

use std::path::PathBuf;
use terrio::crs::SpatialRef;
use terrio::ops::mosaic::{MosaicOptions, MosaicSource, build_vrt, mosaic};
use terrio::ops::translate::{TranslateOptions, translate};
use terrio::ops::warp::{WarpOptions, warp};
use terrio::ops::{Resampling, vrt::VrtDataset};

fn data(name: &str) -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let p = dir.join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

/// Asserts that two geotransforms agree to a small fraction of a pixel.
fn assert_gt(a: [f64; 6], b: [f64; 6], tol_px: f64) {
    for i in 0..6 {
        assert!((a[i] - b[i]).abs() <= tol_px * b[1].abs(), "geotransform[{i}]: {a:?} != {b:?}");
    }
}

fn value(ds: &terrio::RasterDataset, row: usize, col: usize) -> f64 {
    ds.raster.get(0, row as isize, col as isize)
}

#[test]
fn warp_nad83_dem_to_utm() {
    let Some(p) = data("usgs_dem.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let o = WarpOptions { dst_srs: Some(SpatialRef::from_epsg(32613).unwrap()), ..Default::default() };
    let out = warp(&[&ds], &o).unwrap();
    assert_eq!((out.raster.cols, out.raster.rows), (3151, 4055));
    assert_gt(out.geo_transform(), [413260.9626369694, 27.575389108661362, 0.0, 4428422.65095158, 0.0, -27.575389108661362], 1e-4);
    assert_eq!(out.nodata(), Some(-999999.0));
    assert_eq!(out.crs().epsg, Some(32613));
    assert_eq!(value(&out, 100, 100), 2908.941650390625);
    assert_eq!(value(&out, 2000, 1500), 2740.14453125);
    assert_eq!(value(&out, 2027, 1050), 3170.70849609375);

    let o = WarpOptions { resampling: Resampling::Bilinear, ..o };
    let out = warp(&[&ds], &o).unwrap();
    for (r, c, v) in [(100, 100, 2908.421630859375), (2000, 1500, 2739.4599609375), (2027, 1050, 3169.11572265625)] {
        assert!((value(&out, r, c) - v).abs() < 1e-3, "({r}, {c}): {} != {v}", value(&out, r, c));
    }
}

#[test]
fn warp_utm_to_geographic() {
    let Some(p) = data("ECOv002_L2T_LSTE_26860_001_10SGD_20230401T203733_0710_01_LST.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let o = WarpOptions { dst_srs: Some(SpatialRef::parse("EPSG:4326").unwrap()), ..Default::default() };
    let out = warp(&[&ds], &o).unwrap();
    assert_eq!((out.raster.cols, out.raster.rows), (1766, 1458));
    assert_gt(
        out.geo_transform(),
        [-120.82892726886365, 0.0006968076460905677, 0.0, 35.223132916637724, 0.0, -0.0006968076460905677],
        1e-4,
    );
    assert!(out.nodata().unwrap().is_nan());
    assert_eq!(value(&out, 100, 100), 285.760009765625);
    assert_eq!(value(&out, 729, 588), 298.6199951171875);
}

#[test]
fn translate_projwin() {
    let Some(p) = data("usgs_dem.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let o = TranslateOptions { proj_win: Some((-105.7, 39.8, -105.3, 39.5)), ..Default::default() };
    let out = translate(&ds, &o).unwrap();
    assert_eq!((out.raster.cols, out.raster.rows), (1440, 1080));
    assert_gt(
        out.geo_transform(),
        [-105.70000000013535, 0.00027777777803598015, 0.0, 39.800000000310646, 0.0, -0.0002777777777870148],
        1e-9,
    );
    assert_eq!(value(&out, 100, 100), 2974.4296875);
    assert_eq!(value(&out, 540, 480), 3114.020751953125);
}

#[test]
fn mosaic_and_vrt_of_adjacent_tiles() {
    let (Some(a), Some(b)) = (data("USGS_1_n40w107.tif"), data("usgs_dem.tif")) else { return };
    let (da, db) = (terrio::open_raster(&a).unwrap(), terrio::open_raster(&b).unwrap());
    let sources = [MosaicSource { path: a.clone(), ds: &da }, MosaicSource { path: b.clone(), ds: &db }];
    let out = mosaic(&sources, &MosaicOptions::default()).unwrap();
    assert_eq!((out.raster.cols, out.raster.rows), (7212, 3612));
    assert_gt(
        out.geo_transform(),
        [-107.00166666688256, 0.00027777777791148956, 0.0, 40.00166666698402, 0.0, -0.0002777777777870148],
        1e-9,
    );
    for (r, c, v) in [(100, 100, 2862.404052734375), (2000, 1500, 3800.40185546875), (1806, 2404, 3385.1103515625)] {
        assert_eq!(value(&out, r, c), v, "({r}, {c})");
    }

    // The same mosaic as a VRT file, opened again.
    let dir = tempfile::tempdir().unwrap();
    let vrt_path = dir.path().join("mosaic.vrt");
    let vrt = build_vrt(&sources, &MosaicOptions::default(), Some(dir.path())).unwrap();
    vrt.write(&vrt_path.to_string_lossy()).unwrap();
    let back = terrio::open_raster(&vrt_path.to_string_lossy()).unwrap();
    assert_eq!(back.driver, terrio::formats::RasterDriver::VRT);
    assert_eq!(back.geo_transform(), out.geo_transform());
    assert_eq!(value(&back, 1806, 2404), 3385.1103515625);
    assert_eq!(back.nodata(), Some(-999999.0));
}

#[test]
fn reads_vrt_written_by_gdal_translate() {
    // `gdal_translate -of VRT -srcwin 10 20 300 200 -scale 500 1500 0 1 -ot Float32`, with the
    // source path relative to the VRT.
    let Some(p) = data("las_vegas_dem.tif") else { return };
    let dir = tempfile::tempdir().unwrap();
    let src = std::fs::canonicalize(&p).unwrap();
    let xml = format!(
        r#"<VRTDataset rasterXSize="300" rasterYSize="200">
  <GeoTransform>0, 1, 0, 0, 0, -1</GeoTransform>
  <VRTRasterBand dataType="Float32" band="1">
    <NoDataValue>-inf</NoDataValue>
    <ComplexSource>
      <SourceFilename relativeToVRT="0">{}</SourceFilename>
      <SourceBand>1</SourceBand>
      <SrcRect xOff="10" yOff="20" xSize="300" ySize="200" />
      <DstRect xOff="0" yOff="0" xSize="300" ySize="200" />
      <ScaleOffset>-0.5</ScaleOffset>
      <ScaleRatio>0.001</ScaleRatio>
      <NODATA>-inf</NODATA>
    </ComplexSource>
  </VRTRasterBand>
</VRTDataset>"#,
        src.display()
    );
    let path = dir.path().join("subset.vrt");
    std::fs::write(&path, xml).unwrap();
    let vrt = VrtDataset::open(&path.to_string_lossy()).unwrap();
    assert_eq!(vrt.bands[0].sources[0].scale, Some((-0.5, 0.001)));
    let out = terrio::open_raster(&path.to_string_lossy()).unwrap();
    let ds = terrio::open_raster(&p).unwrap();
    let expected = (ds.raster.get(0, 25, 15) * 0.001 - 0.5) as f32 as f64;
    assert_eq!(value(&out, 5, 5), expected);
}

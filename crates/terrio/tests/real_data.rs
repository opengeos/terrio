//! Tests on real-world files, with expected values taken from GDAL 3.12.
//!
//! The files live outside the repository: set `TERRIO_TEST_DATA` to their directory (see
//! `scripts/fetch_test_data.sh`). Without it, these tests pass without checking anything.

use std::path::PathBuf;
use terrio::Dataset;

fn data(name: &str) -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let p = dir.join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

fn assert_close(a: f64, b: f64, what: &str) {
    assert!((a - b).abs() <= 1e-9 * b.abs().max(1.0), "{what}: {a} != {b}");
}

#[test]
fn geotiff_model_transformation() {
    let Some(p) = data("cog.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let gt = ds.geo_transform();
    assert_close(gt[0], 6.860074498707138, "origin x");
    assert_close(gt[3], 53.416_342_076_542_28, "origin y");
    assert_close(gt[1], 0.000089831528412, "pixel width");
    assert_eq!(ds.crs().epsg, Some(4326));
    assert_eq!((ds.raster.cols, ds.raster.rows, ds.raster.bands), (343, 206, 4));
    // Old-style deflate (compression 32946): values must decode.
    let s = terrio::info::band_statistics(&ds, 0).unwrap();
    assert_eq!((s.min, s.max), (488.0, 2102.0));
}

#[test]
fn geotiff_user_defined_utm() {
    let Some(p) = data("ECOv002_L2T_LSTE_26860_001_10SGD_20230401T203733_0710_01_LST.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    assert_eq!(ds.crs().epsg, Some(32610));
    assert!(ds.nodata().unwrap().is_nan());
}

#[test]
fn geotiff_palette_with_predictor() {
    let Some(p) = data("OPERA_L3_DSWx_WTR.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    assert_eq!(ds.nodata(), Some(255.0));
    let s = terrio::info::band_statistics(&ds, 0).unwrap();
    assert_eq!((s.min, s.max), (0.0, 253.0));
    assert!((s.mean - 1.7177237138564778).abs() < 1e-6, "{}", s.mean);
}

#[test]
fn geotiff_lzw_floating_point_predictor() {
    let Some(p) = data("usgs_dem.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    assert_eq!(ds.crs().epsg, Some(4269));
    assert_eq!((ds.raster.cols, ds.raster.rows), (3612, 3612));
}

#[test]
fn shapefile_in_zip() {
    let Some(p) = data("nybb.zip") else { return };
    let Dataset::Vector(v) = terrio::open(&format!("{p}!nybb_16a/nybb.shp")).unwrap() else { panic!("not a vector") };
    assert_eq!(v.layers[0].len(), 5);
    assert_eq!(terrio::crs::SpatialRef::from_epsg(6538).unwrap().name().unwrap(), "NAD83(2011) / New York Long Island");
}

#[test]
fn geopackage_layers() {
    let Some(p) = data("multi.gpkg") else { return };
    assert_eq!(terrio::list_layers(&p).unwrap(), vec!["buildings", "roads"]);
    let v = terrio::open_vector(&p, &Default::default()).unwrap();
    assert_eq!(v.layers.iter().map(|l| l.len()).collect::<Vec<_>>(), vec![540, 78]);
}

#[test]
fn geojson_defaults_to_wgs84() {
    let Some(p) = data("countries.geojson") else { return };
    let v = terrio::open_vector(&p, &Default::default()).unwrap();
    assert_eq!(v.layers[0].crs_epsg(), Some(4326));
    assert_eq!(v.layers[0].name, "countries");
}

#[test]
fn geotiff_user_defined_albers() {
    let Some(p) = data("nlcd_knoxville.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let crs = ds.crs();
    assert!(!crs.is_unknown(), "the custom Albers CRS must be read from the GeoKeys");
    // GDAL: (-84.38694942782567, 36.340361661300086).
    let t = terrio::crs::Transformer::new(&crs, &terrio::crs::SpatialRef::from_epsg(4326).unwrap()).unwrap();
    let (lon, lat) = t.transform(1030275.0, 1539885.0).unwrap();
    assert!((lon - -84.38694942782567).abs() < 1e-9 && (lat - 36.340361661300086).abs() < 1e-9, "{lon} {lat}");
}

/// The GDAL statistics of `srtm_sub.tif`, which every `srtm_sub` copy must reproduce.
const SRTM_SUB_STATS: (f64, f64, f64) = (370.0, 2843.0, 1603.6905150000036);

/// Checks a copy of `srtm_sub.tif` written by GDAL in another format.
fn check_srtm_sub(name: &str, data_type: terrio::DataType, nodata: f64) {
    // A netCDF variable: `NETCDF:"file":variable`.
    let p = match name.split_once(':') {
        Some((file, var)) => match data(file) {
            Some(f) => format!("NETCDF:\"{f}\":{var}"),
            None => return,
        },
        None => match data(name) {
            Some(p) => p,
            None => return,
        },
    };
    let ds = terrio::open_raster(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!((ds.raster.cols, ds.raster.rows, ds.raster.bands), (500, 400, 1), "{name}");
    assert_eq!(ds.raster.data_type, data_type, "{name}");
    assert_eq!(ds.nodata(), Some(nodata), "{name}");
    assert_eq!(ds.crs().epsg, Some(4326), "{name}");
    let gt = ds.geo_transform();
    assert!((gt[0] - -119.947_443_593_553).abs() < 1e-7 && (gt[3] - 37.818_439_678_407).abs() < 1e-7, "{name}: {gt:?}");
    assert!((gt[1] - 0.000_808_483_755_7).abs() < 1e-9, "{name}: {gt:?}");
    let s = terrio::info::band_statistics(&ds, 0).unwrap();
    assert_eq!((s.min, s.max), (SRTM_SUB_STATS.0, SRTM_SUB_STATS.1), "{name}");
    assert!((s.mean - SRTM_SUB_STATS.2).abs() < 1e-6, "{name}: {}", s.mean);
}

#[test]
fn srtm_sub_in_gdal_written_formats() {
    use terrio::DataType::{F32, I16};
    check_srtm_sub("srtm_sub.bil", I16, -32768.0);
    check_srtm_sub("srtm_sub.sdat", I16, -32768.0);
    check_srtm_sub("srtm_sub.ers", I16, -32768.0);
    check_srtm_sub("srtm_sub.rst", I16, -32768.0);
    check_srtm_sub("srtm_sub.jp2", I16, -32768.0);
    check_srtm_sub("srtm_sub_v3.zarr", I16, -32768.0);
    check_srtm_sub("srtm_sub_blosc.zarr", I16, -32768.0);
    check_srtm_sub("srtm_sub.grd", F32, 1.701_41e38_f32 as f64);
    check_srtm_sub("srtm_sub.nc:Band1", I16, -32768.0);
}

#[test]
fn netcdf4_variables_as_subdatasets() {
    let Some(p) = data("wind_global.nc") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    // The first variable, as GDAL's first subdataset; south-up latitudes are turned north-up.
    assert_eq!((ds.raster.cols, ds.raster.rows), (360, 181));
    assert_eq!(ds.geo_transform(), [-0.5, 1.0, 0.0, 90.5, 0.0, -1.0]);
    assert!(ds.nodata().unwrap().is_nan());
    let s = terrio::info::band_statistics(&ds, 0).unwrap();
    assert_eq!((s.min, s.max), (-21.61, 25.61));
    let v = terrio::open_raster(&format!("NETCDF:\"{p}\":v_wind")).unwrap();
    let s = terrio::info::band_statistics(&v, 0).unwrap();
    assert_eq!((s.min, s.max), (-22.77, 21.12));
}

#[test]
fn gdal_derived_las_vegas_dem() {
    for name in ["lv_dem.asc", "lv_dem.envi", "lv_dem.img", "lv_dem.jp2", "lv_dem.zarr"] {
        let Some(p) = data(name) else { return };
        let ds = terrio::open_raster(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((ds.raster.cols, ds.raster.rows), (970, 498), "{name}");
        assert_eq!(ds.crs().epsg, Some(3857), "{name}");
        assert_eq!(ds.geo_transform(), [-12824908.0, 1.0, 0.0, 4317505.0, 0.0, -1.0], "{name}");
    }
    // GDAL types an ASCII grid with decimals as Float32, and the JPEG 2000 copy is signed.
    let Some(p) = data("lv_dem.asc") else { return };
    assert_eq!(terrio::open_raster(&p).unwrap().raster.data_type, terrio::DataType::F32);
    let Some(p) = data("lv_dem.jp2") else { return };
    let jp2 = terrio::open_raster(&p).unwrap();
    assert_eq!(jp2.raster.data_type, terrio::DataType::I16);
    let s = terrio::info::band_statistics(&jp2, 0).unwrap();
    assert_eq!((s.min, s.max), (-32767.0, 679.0));
}

#[test]
fn geopackage_gridded_coverage_at_full_resolution() {
    let Some(p) = data("dem.gpkg") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    assert_eq!((ds.raster.cols, ds.raster.rows), (2880, 1773));
    assert_eq!(ds.raster.data_type, terrio::DataType::F32);
    assert_close(ds.geo_transform()[1], 33.239_125_326_623_25, "pixel width");
    assert_eq!(ds.crs().epsg, Some(3857));
}

#[test]
fn geotiff_and_cog_written_by_terrio_read_back() {
    let Some(p) = data("srtm_sub.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (driver, compression) in [(terrio::formats::RasterDriver::GTiff, "LZW"), (terrio::formats::RasterDriver::Cog, "DEFLATE")] {
        let out = dir.path().join(format!("{}.tif", driver.name()));
        let out = out.to_str().unwrap();
        let opts = terrio::RasterWriteOptions { driver: Some(driver), compression: Some(compression.into()), ..Default::default() };
        terrio::write_raster_dataset(&ds, out, &opts).unwrap();
        let back = terrio::open_raster(out).unwrap();
        assert_eq!(back.raster.data_i16(), ds.raster.data_i16());
        assert_eq!(back.nodata(), Some(-32768.0));
        assert_eq!(back.geo_transform(), ds.geo_transform());
    }
}

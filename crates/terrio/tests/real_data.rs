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

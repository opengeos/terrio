//! The optional GDAL backend (`cargo test --features gdal`): formats terrio has no driver for,
//! opened through GDAL, and datasets GDAL and terrio both read, compared value for value.
//!
//! The tests that need real-world files skip themselves without `TERRIO_TEST_DATA` (see
//! `scripts/fetch_test_data.sh`).

#![cfg(feature = "gdal")]

use gdal::vector::{LayerAccess, LayerOptions, OGRFieldType, OGRwkbGeometryType};
use std::path::PathBuf;
use terrio::formats::{RasterDriver, VectorDriver};
use terrio::raster::RasterData;
use terrio::{FieldValue, Raster, RasterConfig, RasterDataset};

fn data(name: &str) -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let p = dir.join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

/// A small georeferenced Int16 raster with a nodata value.
fn sample() -> RasterDataset {
    let cfg = RasterConfig {
        cols: 7,
        rows: 5,
        bands: 2,
        x_min: 500_000.0,
        y_min: 4_100_000.0,
        cell_size: 30.0,
        nodata: -32768.0,
        data_type: terrio::DataType::I16,
        crs: terrio::crs::SpatialRef::from_epsg(32613).unwrap().to_crs_info(),
        ..Default::default()
    };
    let values: Vec<i16> = (0..70).map(|i| if i == 3 { -32768 } else { i * 37 - 900 }).collect();
    RasterDataset::new(Raster::from_data_native(cfg, RasterData::I16(values)).unwrap(), true)
}

fn assert_same_raster(a: &RasterDataset, b: &RasterDataset, what: &str) {
    assert_eq!((a.raster.cols, a.raster.rows, a.raster.bands), (b.raster.cols, b.raster.rows, b.raster.bands), "{what}: size");
    for (x, y) in a.geo_transform().iter().zip(b.geo_transform()) {
        assert!((x - y).abs() <= 1e-9 * y.abs().max(1.0), "{what}: geotransform {:?} != {:?}", a.geo_transform(), b.geo_transform());
    }
    assert_eq!(a.crs().epsg, b.crs().epsg, "{what}: CRS");
    assert_eq!(a.nodata().map(f64::to_bits), b.nodata().map(f64::to_bits), "{what}: nodata");
    assert_eq!(a.raster.data_type, b.raster.data_type, "{what}: data type");
    let (va, vb) = (a.raster.data.to_f64_vec(), b.raster.data.to_f64_vec());
    let differing = va.iter().zip(&vb).filter(|(x, y)| x.to_bits() != y.to_bits() && !(x.is_nan() && y.is_nan())).count();
    assert_eq!(differing, 0, "{what}: {differing} pixels differ");
}

#[test]
fn gdal_driver_listed() {
    assert_eq!(RasterDriver::from_name("gdal"), Some(RasterDriver::Gdal));
    assert_eq!(VectorDriver::from_name("GDAL"), Some(VectorDriver::Gdal));
    assert!(RasterDriver::HFA.info().write, "HFA is written through GDAL");
    assert!(terrio::gdal_backend::gdal_version().starts_with('3'));
    assert!(terrio::gdal_backend::has_driver("NITF"));
    assert!(!terrio::gdal_backend::has_driver("NoSuchDriver"));
    assert_eq!(terrio::gdal_backend::strip_prefix("gdal:x.hdf"), Some("x.hdf"));
    assert_eq!(terrio::gdal_backend::strip_prefix("x.hdf"), None);
}

/// Formats terrio cannot read or write itself: written with GDAL's drivers, then read back
/// through the fallback of `terrio::open_raster`, without a prefix.
#[test]
fn rasters_in_formats_terrio_lacks() {
    let dir = tempfile::tempdir().unwrap();
    let src = sample();
    for (driver, ext) in [("NITF", "ntf"), ("HFA", "img"), ("KEA", "kea"), ("PCIDSK", "pix"), ("BMP", "bmp")] {
        if !terrio::gdal_backend::has_driver(driver) {
            continue;
        }
        let path = dir.path().join(format!("sample.{ext}")).to_string_lossy().into_owned();
        let one_band = driver == "BMP";
        let mut input = src.clone();
        if one_band {
            // BMP holds Byte bands only.
            let cfg = RasterConfig {
                cols: 7,
                rows: 5,
                x_min: 500_000.0,
                y_min: 4_100_000.0,
                cell_size: 30.0,
                data_type: terrio::DataType::U8,
                ..Default::default()
            };
            let values = (0..35).map(|i| i as u8 * 7).collect();
            input = RasterDataset::new(Raster::from_data_native(cfg, RasterData::U8(values)).unwrap(), false);
        }
        terrio::gdal_backend::write_raster(&input, &path, driver, &[]).unwrap();
        let back = terrio::open_raster(&path).unwrap_or_else(|e| panic!("{driver}: {e}"));
        let expected_driver = if driver == "HFA" { RasterDriver::HFA } else { RasterDriver::Gdal };
        assert_eq!(back.driver, expected_driver, "{driver}");
        if driver == "BMP" {
            // A world file gives BMP its georeferencing; it has no CRS.
            assert_eq!(back.raster.data.to_f64_vec(), input.raster.data.to_f64_vec(), "{driver}");
            continue;
        }
        assert_same_raster(&back, &input, driver);
        // Forcing GDAL gives the same.
        assert_same_raster(&terrio::open_raster(&format!("GDAL:{path}")).unwrap(), &input, driver);
    }
}

/// ERDAS Imagine: read by terrio's own driver, written through GDAL by `write_raster`.
#[test]
fn hfa_written_through_gdal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample.img").to_string_lossy().into_owned();
    let src = sample();
    terrio::write_raster_dataset(&src, &path, &Default::default()).unwrap();
    let gdal_view = terrio::gdal_backend::open_raster(&path).unwrap();
    assert_same_raster(&gdal_view, &src, "HFA (GDAL)");
    let native = terrio::open_raster(&path).unwrap();
    assert_eq!(native.driver, RasterDriver::HFA);
    assert_same_raster(&native, &src, "HFA (terrio)");
}

#[test]
fn missing_files_are_not_retried() {
    let e = terrio::open("/no/such/file.hdf").unwrap_err();
    assert!(matches!(e, terrio::Error::Io(_)), "{e}");
    let e = terrio::open_raster("GDAL:/no/such/file.hdf").unwrap_err();
    assert!(e.to_string().contains("GDAL"), "{e}");
}

/// A SQLite database (not a GeoPackage) and a DXF drawing, created with GDAL, read through the
/// fallback.
#[test]
fn vectors_in_formats_terrio_lacks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("places.sqlite").to_string_lossy().into_owned();
    {
        let driver = gdal::DriverManager::get_driver_by_name("SQLite").unwrap();
        let mut ds = driver.create_vector_only(&path).unwrap();
        let srs = gdal::spatial_ref::SpatialRef::from_epsg(4326).unwrap();
        let options = LayerOptions { name: "places", srs: Some(&srs), ty: OGRwkbGeometryType::wkbPoint, options: None };
        let layer = ds.create_layer(options).unwrap();
        layer
            .create_defn_fields(&[("name", OGRFieldType::OFTString), ("pop", OGRFieldType::OFTInteger64), ("area", OGRFieldType::OFTReal)])
            .unwrap();
        for (name, pop, area, x, y) in [("Knoxville", 190_740i64, 270.6, -83.92, 35.96), ("Nashville", 689_447, 1362.0, -86.78, 36.16)] {
            let mut f = gdal::vector::Feature::new(layer.defn()).unwrap();
            f.set_field_string(0, name).unwrap();
            f.set_field_integer64(1, pop).unwrap();
            f.set_field_double(2, area).unwrap();
            f.set_geometry(gdal::vector::Geometry::from_wkt(&format!("POINT ({x} {y})")).unwrap()).unwrap();
            f.create(&layer).unwrap();
        }
    }
    assert_eq!(terrio::list_layers(&path).unwrap(), vec!["places".to_string()]);
    let ds = terrio::open_vector(&path, &Default::default()).unwrap();
    assert_eq!(ds.driver, VectorDriver::Gdal);
    let layer = &ds.layers[0];
    assert_eq!(layer.name, "places");
    assert_eq!(layer.crs_epsg(), Some(4326));
    assert_eq!(layer.geom_type, Some(terrio::GeometryType::Point));
    let names: Vec<&str> = layer.schema.fields().iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["name", "pop", "area"]);
    assert_eq!(layer.features.len(), 2);
    let f = &layer.features[1];
    assert_eq!(f.attributes, vec![FieldValue::Text("Nashville".into()), FieldValue::Integer(689_447), FieldValue::Float(1362.0)]);
    match &f.geometry {
        Some(terrio::Geometry::Point(c)) => assert_eq!((c.x, c.y), (-86.78, 36.16)),
        g => panic!("{g:?}"),
    }
    // GDAL's spatial filter.
    let options = terrio::VectorOpenOptions { spatial_filter: Some((-85.0, 35.0, -83.0, 37.0)), ..Default::default() };
    assert_eq!(terrio::open_vector(&path, &options).unwrap().layers[0].features.len(), 1);
    // And `terrio::open` on the same file.
    assert!(matches!(terrio::open(&path).unwrap(), terrio::Dataset::Vector(_)));

    // A DXF drawing with a curve: GDAL linearizes it.
    let dxf = dir.path().join("arc.dxf").to_string_lossy().into_owned();
    {
        let driver = gdal::DriverManager::get_driver_by_name("DXF").unwrap();
        let mut ds = driver.create_vector_only(&dxf).unwrap();
        let mut layer = ds.create_layer(LayerOptions { name: "entities", ..Default::default() }).unwrap();
        let arc = gdal::vector::Geometry::from_wkt("CIRCULARSTRING (0 0, 1 1, 2 0)").unwrap();
        layer.create_feature(arc).unwrap();
    }
    let ds = terrio::open_vector(&dxf, &Default::default()).unwrap();
    let geometry = ds.layers[0].features[0].geometry.as_ref().unwrap();
    assert!(matches!(geometry, terrio::Geometry::LineString(c) if c.len() > 3), "{geometry:?}");
}

/// Rasters terrio reads itself, read again through GDAL: the two views must be identical.
#[test]
fn rasters_match_terrio_drivers() {
    for name in ["srtm_sub.tif", "landsat.tif", "nlcd_knoxville.tif", "usgs_dem.tif", "lv_dem.img", "srtm_sub.rst", "srtm_sub.ers"] {
        let Some(p) = data(name) else { return };
        let native = terrio::open_raster(&p).unwrap();
        let via_gdal = terrio::open_raster(&format!("GDAL:{p}")).unwrap();
        assert_eq!(via_gdal.driver, RasterDriver::Gdal);
        assert_same_raster(&via_gdal, &native, name);
    }
}

/// Subdatasets: a netCDF file's variables, listed and opened by GDAL's names.
#[test]
fn subdatasets_through_gdal() {
    let Some(p) = data("wind_global.nc") else { return };
    let subdatasets = terrio::gdal_backend::subdatasets(&p).unwrap();
    assert_eq!(subdatasets.len(), 2);
    assert!(subdatasets[1].0.ends_with(":v_wind"), "{subdatasets:?}");
    // The container opens its first subdataset and lists the others.
    let first = terrio::open_raster(&format!("GDAL:{p}")).unwrap();
    assert!(first.raster.metadata.iter().any(|(k, v)| k == "SUBDATASET_2_NAME" && v == &subdatasets[1].0));
    // GDAL's subdataset name against terrio's own netCDF driver.
    let via_gdal = terrio::open_raster(&format!("GDAL:{}", subdatasets[1].0)).unwrap();
    let native = terrio::open_raster(&subdatasets[1].0).unwrap();
    assert_eq!(native.driver, RasterDriver::NetCDF);
    assert_eq!(via_gdal.raster.data.to_f64_vec().len(), native.raster.data.to_f64_vec().len());
    let differing = via_gdal
        .raster
        .data
        .to_f64_vec()
        .iter()
        .zip(native.raster.data.to_f64_vec())
        .filter(|(a, b)| a.to_bits() != b.to_bits() && !(a.is_nan() && b.is_nan()))
        .count();
    assert_eq!(differing, 0);
}

/// Vector datasets terrio reads itself, read again through GDAL.
#[test]
fn vectors_match_terrio_drivers() {
    for name in ["countries.gpkg", "countries.fgb", "countries.geojson", "ne_countries.zip"] {
        let Some(p) = data(name) else { return };
        let native = terrio::open_vector(&p, &Default::default()).unwrap();
        let via_gdal = terrio::open_vector(&format!("GDAL:{p}"), &Default::default()).unwrap();
        assert_eq!(via_gdal.layers.len(), native.layers.len(), "{name}");
        for (a, b) in via_gdal.layers.iter().zip(&native.layers) {
            assert_eq!(a.features.len(), b.features.len(), "{name}");
            assert_eq!(a.crs_epsg(), b.crs_epsg(), "{name}");
            let fields = |l: &terrio::Layer| l.schema.fields().iter().map(|f| f.name.to_ascii_lowercase()).collect::<Vec<_>>();
            assert_eq!(fields(a), fields(b), "{name}");
            for (fa, fb) in a.features.iter().zip(&b.features) {
                assert_eq!(fa.geometry, fb.geometry, "{name}: feature {}", fb.fid);
            }
        }
    }
}

/// A real DEM converted by GDAL into formats terrio has no driver for (GRIB2, HDF4, NITF), then
/// opened by plain path: the pixels must be the DEM's.
#[test]
fn real_raster_in_formats_terrio_lacks() {
    let Some(p) = data("srtm_sub.tif") else { return };
    let src = terrio::open_raster(&p).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (driver, ext) in [("GRIB", "grib2"), ("HDF4Image", "hdf"), ("NITF", "ntf")] {
        if !terrio::gdal_backend::has_driver(driver) {
            continue;
        }
        let path = dir.path().join(format!("srtm_sub.{ext}")).to_string_lossy().into_owned();
        terrio::gdal_backend::write_raster(&src, &path, driver, &[]).unwrap();
        let back = terrio::open_raster(&path).unwrap_or_else(|e| panic!("{driver}: {e}"));
        assert_eq!(back.driver, RasterDriver::Gdal, "{driver}");
        assert_eq!((back.raster.cols, back.raster.rows), (src.raster.cols, src.raster.rows), "{driver}");
        assert_eq!(back.crs().epsg, Some(4326), "{driver}");
        // GRIB and HDF4 keep coordinates to fewer digits; NITF keeps geographic corners in
        // whole arc seconds.
        let tolerance = if driver == "NITF" { 3e-4 } else { 1e-6 };
        for (a, b) in back.geo_transform().iter().zip(src.geo_transform()) {
            assert!((a - b).abs() < tolerance, "{driver}: {:?} != {:?}", back.geo_transform(), src.geo_transform());
        }
        let (a, b) = (back.raster.data.to_f64_vec(), src.raster.data.to_f64_vec());
        assert_eq!(a, b, "{driver}: pixels");
    }
}

/// An HDF5 file (netCDF-4) through GDAL's HDF5 driver: its datasets are subdatasets.
#[test]
fn hdf5_subdatasets() {
    let Some(p) = data("wind_global.nc") else { return };
    if !terrio::gdal_backend::has_driver("HDF5") {
        return;
    }
    let name = format!("HDF5:\"{p}\"://v_wind");
    let via_hdf5 = terrio::open_raster(&name).unwrap();
    assert_eq!(via_hdf5.driver, RasterDriver::Gdal);
    let native = terrio::open_raster(&format!("NETCDF:\"{p}\":v_wind")).unwrap();
    assert_eq!((via_hdf5.raster.cols, via_hdf5.raster.rows), (native.raster.cols, native.raster.rows));
    assert_eq!(via_hdf5.raster.data.to_f64_vec(), native.raster.data.to_f64_vec());
}

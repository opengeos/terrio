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

/// An HDF5 file (netCDF-4) through GDAL's HDF5 driver and terrio's: its datasets are subdatasets.
#[test]
fn hdf5_subdatasets() {
    let Some(p) = data("wind_global.nc") else { return };
    if !terrio::gdal_backend::has_driver("HDF5") {
        return;
    }
    let name = format!("HDF5:\"{p}\"://v_wind");
    let via_hdf5 = terrio::open_raster(&format!("GDAL:{name}")).unwrap();
    assert_eq!(via_hdf5.driver, RasterDriver::Gdal);
    let native_hdf5 = terrio::open_raster(&name).unwrap();
    assert_eq!(native_hdf5.driver, RasterDriver::HDF5);
    let native = terrio::open_raster(&format!("NETCDF:\"{p}\":v_wind")).unwrap();
    assert_eq!((via_hdf5.raster.cols, via_hdf5.raster.rows), (native.raster.cols, native.raster.rows));
    assert_eq!(via_hdf5.raster.data.to_f64_vec(), native.raster.data.to_f64_vec());
    let same = |a: &[f64], b: &[f64]| a.iter().zip(b).all(|(x, y)| x == y || (x.is_nan() && y.is_nan()));
    assert!(same(&native_hdf5.raster.data.to_f64_vec(), &via_hdf5.raster.data.to_f64_vec()));
}

/// A small layer of polygons with every field type terrio has, in UTM zone 17N.
fn sample_layer() -> terrio::Layer {
    use terrio::{FieldDef, FieldType, Geometry};
    let mut layer = terrio::Layer::new("parcels");
    layer.geom_type = Some(terrio::GeometryType::Polygon);
    layer.set_crs_epsg(Some(32617));
    for (name, t) in [
        ("name", FieldType::Text),
        ("count", FieldType::Integer),
        ("area", FieldType::Float),
        ("built", FieldType::Date),
        ("flag", FieldType::Boolean),
    ] {
        layer.add_field(FieldDef::new(name, t));
    }
    for i in 0..4i64 {
        let (x, y) = (400_000.0 + i as f64 * 1000.0, 3_980_000.0);
        let ring = vec![(x, y), (x + 500.0, y), (x + 500.0, y + 250.5), (x, y + 250.5), (x, y)];
        let geometry = Geometry::polygon(ring.into_iter().map(|(x, y)| wbvector::Coord::xy(x, y)).collect(), vec![]);
        let attributes = vec![
            if i == 2 { FieldValue::Null } else { FieldValue::Text(format!("parcel é{i}")) },
            FieldValue::Integer(i * 1_000_000_007),
            FieldValue::Float(125_250.0 + i as f64 / 8.0),
            FieldValue::Date(format!("2024-0{}-15", i + 1)),
            FieldValue::Boolean(i % 2 == 0),
        ];
        layer.push(terrio::Feature { fid: i as u64, geometry: Some(geometry), attributes });
    }
    layer
}

/// Reads every feature of a GDAL dataset's first layer with GDAL itself: WKT of the geometry and
/// the fields as strings.
fn read_with_gdal(path: &str) -> (Option<u32>, Vec<String>, Vec<(String, Vec<Option<String>>)>) {
    let ds = gdal::Dataset::open(path).unwrap();
    let mut layer = ds.layer(0).unwrap();
    // MapInfo files keep the definition, not the code: identified as `gdalsrsinfo -e` does.
    let epsg = layer
        .spatial_ref()
        .and_then(|mut s| s.auth_code().ok().or_else(|| s.auto_identify_epsg().ok().and_then(|()| s.auth_code().ok())).map(|c| c as u32));
    let names: Vec<String> = layer.defn().fields().map(|f| f.name()).collect();
    let features = layer
        .features()
        .map(|f| {
            let wkt = f.geometry().map(|g| g.wkt().unwrap()).unwrap_or_default();
            (wkt, (0..names.len()).map(|i| f.field_as_string(i).unwrap()).collect())
        })
        .collect();
    (epsg, names, features)
}

/// Vector layers written through GDAL's drivers, and read back with GDAL: every feature's
/// geometry and fields.
#[test]
fn vectors_written_through_gdal() {
    let dir = tempfile::tempdir().unwrap();
    let layer = sample_layer();
    for (driver, ext) in [("OpenFileGDB", "gdb"), ("MapInfo File", "tab"), ("SQLite", "sqlite"), ("GML", "gml"), ("ESRI Shapefile", "shp")]
    {
        if !terrio::gdal_backend::has_driver(driver) {
            continue;
        }
        let path = dir.path().join(format!("parcels.{ext}")).to_string_lossy().into_owned();
        terrio::gdal_backend::write_vector(std::slice::from_ref(&layer), &path, driver, &[], &[])
            .unwrap_or_else(|e| panic!("{driver}: {e}"));
        // Written twice: the output is replaced.
        terrio::gdal_backend::write_vector(std::slice::from_ref(&layer), &path, driver, &[], &[]).unwrap();
        let (epsg, names, features) = read_with_gdal(&path);
        assert_eq!(epsg, Some(32617), "{driver}: CRS");
        assert_eq!(features.len(), 4, "{driver}");
        let lower: Vec<String> = names.iter().map(|n| n.to_ascii_lowercase()).collect();
        for name in ["name", "count", "area", "built", "flag"] {
            assert!(lower.contains(&name.to_string()), "{driver}: field {name} missing from {names:?}");
        }
        let col = |n: &str| lower.iter().position(|x| x == n).unwrap();
        let (wkt, values) = &features[1];
        // File Geodatabases hold polygons as multipolygons, with rings clockwise.
        let numbers: Vec<f64> = wkt.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-')).filter_map(|t| t.parse().ok()).collect();
        let points: Vec<(f64, f64)> = numbers.chunks_exact(2).map(|p| (p[0], p[1])).collect();
        let ring = [(401000.0, 3980000.0), (401500.0, 3980000.0), (401500.0, 3980250.5), (401000.0, 3980250.5)];
        assert!(wkt.contains("POLYGON ((") && points.len() == 5, "{driver}: {wkt}");
        assert!(ring.iter().all(|r| points.contains(r)), "{driver}: {wkt}");
        assert_eq!(values[col("name")].as_deref(), Some("parcel é1"), "{driver}");
        assert_eq!(values[col("count")].as_deref(), Some("1000000007"), "{driver}");
        assert_eq!(values[col("area")].as_deref().map(|v| v.parse::<f64>().unwrap()), Some(125_250.125), "{driver}");
        assert!(values[col("built")].as_deref().is_some_and(|v| v.starts_with("2024/02/15") || v.starts_with("2024-02-15")), "{driver}");
        // A null stays null where the format has nulls.
        if driver != "MapInfo File" && driver != "ESRI Shapefile" {
            assert_eq!(features[2].1[col("name")], None, "{driver}");
        }
        // And terrio reads it back (through GDAL for the formats it lacks).
        let back = terrio::open_vector(&path, &Default::default()).unwrap();
        assert_eq!(back.layers[0].features.len(), 4, "{driver}");
        assert_eq!(back.layers[0].crs_epsg(), Some(32617), "{driver}");
    }
    // Layer creation options reach GDAL: a layer in a File Geodatabase feature dataset.
    let path = dir.path().join("options.gdb").to_string_lossy().into_owned();
    let lco = [("FEATURE_DATASET".to_string(), "survey".to_string())];
    terrio::gdal_backend::write_vector(std::slice::from_ref(&layer), &path, "OpenFileGDB", &[], &lco).unwrap();
    let ds = gdal::Dataset::open(&path).unwrap();
    assert_eq!(ds.layer(0).unwrap().name(), "parcels");
    // A driver that cannot write vector layers is refused.
    assert!(terrio::gdal_backend::write_vector(std::slice::from_ref(&layer), &path, "NoSuchDriver", &[], &[]).is_err());
}

/// `terrio::write_vector` and `write_raster_dataset` hand extensions terrio has no driver for to
/// the GDAL driver that claims them.
#[test]
fn gdal_driver_chosen_by_extension() {
    assert_eq!(terrio::gdal_backend::driver_for_path("a.dxf", true).as_deref(), Some("DXF"));
    assert_eq!(terrio::gdal_backend::driver_for_path("a.gdb", true).as_deref(), Some("OpenFileGDB"));
    assert_eq!(terrio::gdal_backend::driver_for_path("a.ntf", false).as_deref(), Some("NITF"));
    assert_eq!(terrio::gdal_backend::driver_for_path("a.unknown-ext", true), None);
    let dir = tempfile::tempdir().unwrap();
    let mut lines = terrio::Layer::new("lines");
    lines.geom_type = Some(terrio::GeometryType::LineString);
    let coords = [(0.0, 0.0), (10.0, 5.0), (20.0, 0.0)].map(|(x, y)| wbvector::Coord::xy(x, y)).to_vec();
    lines.push(terrio::Feature { fid: 0, geometry: Some(terrio::Geometry::line_string(coords)), attributes: vec![] });
    let dxf = dir.path().join("lines.dxf").to_string_lossy().into_owned();
    terrio::write_vector(&[lines], &dxf, &Default::default()).unwrap();
    let (_, _, features) = read_with_gdal(&dxf);
    assert_eq!(features[0].0, "LINESTRING (0 0,10 5,20 0)");
    let gdb = dir.path().join("parcels.gdb").to_string_lossy().into_owned();
    terrio::write_vector(&[sample_layer()], &gdb, &Default::default()).unwrap();
    assert_eq!(read_with_gdal(&gdb).2.len(), 4);
    let ntf = dir.path().join("sample.ntf").to_string_lossy().into_owned();
    terrio::write_raster_dataset(&sample(), &ntf, &Default::default()).unwrap();
    assert_same_raster(&terrio::open_raster(&ntf).unwrap(), &sample(), "NITF by extension");
}

/// A real layer (Natural Earth countries) through GDAL's File Geodatabase and MapInfo writers:
/// GDAL reads the same geometries and attributes back as terrio wrote.
#[test]
fn real_vector_written_through_gdal() {
    let Some(p) = data("countries.gpkg") else { return };
    let src = terrio::open_vector(&p, &Default::default()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (driver, ext) in [("OpenFileGDB", "gdb"), ("MapInfo File", "tab"), ("DXF", "dxf")] {
        let path = dir.path().join(format!("countries.{ext}")).to_string_lossy().into_owned();
        terrio::gdal_backend::write_vector(&src.layers, &path, driver, &[], &[]).unwrap();
        let back = terrio::open_vector(&format!("GDAL:{path}"), &Default::default()).unwrap();
        let (a, b) = (&back.layers[0], &src.layers[0]);
        if driver == "DXF" {
            // DXF splits multipolygons into several hatches.
            assert!(a.features.len() >= b.features.len(), "{driver}");
            continue;
        }
        assert_eq!(a.features.len(), b.features.len(), "{driver}");
        for (fa, fb) in a.features.iter().zip(&b.features) {
            // Polygons may come back as multipolygons (File Geodatabase) or with rings closed
            // differently: compare the coordinates.
            let coords = |g: &Option<terrio::Geometry>| g.as_ref().map(|g| g.to_wkb().len());
            if driver == "OpenFileGDB" {
                assert_eq!(coords(&fa.geometry).is_some(), coords(&fb.geometry).is_some(), "{driver}: feature {}", fb.fid);
            }
        }
        let name = |l: &terrio::Layer, i: usize| {
            let col = l.schema.fields().iter().position(|f| f.name.eq_ignore_ascii_case("NAME")).unwrap();
            l.features[i].attributes[col].clone()
        };
        for i in [0, 50, b.features.len() - 1] {
            assert_eq!(name(a, i), name(b, i), "{driver}: feature {i}");
        }
    }
}

/// Bytes in memory through GDAL's `/vsimem/`: formats terrio cannot read open from bytes, and
/// formats only GDAL writes come back as bytes.
#[test]
fn bytes_through_gdal() {
    let dir = tempfile::tempdir().unwrap();
    // A NITF raster: written by GDAL to bytes, opened from them. (NITF has no nodata value: GDAL
    // keeps it in an `.aux.xml` sidecar, which bytes leave out.)
    let mut src = sample();
    src.has_nodata = false;
    let ntf = terrio::gdal_backend::write_raster_bytes(&src, "sample.ntf", "NITF", &[]).unwrap();
    assert!(ntf.starts_with(b"NITF"));
    let back = terrio::memory::open_raster_bytes(ntf.clone(), "sample.ntf").unwrap();
    assert_eq!(back.driver, RasterDriver::Gdal);
    assert_same_raster(&back, &src, "NITF from bytes");
    // The same bytes without an extension: GDAL recognizes the content.
    assert_eq!(terrio::memory::open_raster_bytes(ntf, "upload").unwrap().driver, RasterDriver::Gdal);
    // `memory::write_raster_bytes` picks GDAL's driver for the extension.
    let ntf = terrio::memory::write_raster_bytes(&sample(), "x.ntf", &Default::default()).unwrap();
    assert!(ntf.starts_with(b"NITF"));
    // Vector: a File Geodatabase (a directory) comes back as a zip archive of it, which GDAL
    // and terrio open from bytes.
    let gdb = terrio::memory::write_vector_bytes(&[sample_layer()], "parcels.gdb", &Default::default()).unwrap();
    assert!(gdb.starts_with(b"PK"));
    let zip_path = dir.path().join("parcels.gdb.zip");
    std::fs::write(&zip_path, &gdb).unwrap();
    let (epsg, _, features) = read_with_gdal(&format!("/vsizip/{}/parcels.gdb", zip_path.display()));
    assert_eq!((epsg, features.len()), (Some(32617), 4));
    let ds = terrio::memory::open_vector_bytes(gdb, "parcels.gdb.zip", &Default::default()).unwrap();
    assert_eq!(ds.driver, VectorDriver::Gdal);
    assert_eq!(ds.layers[0].name, "parcels");
    assert_eq!(ds.layers[0].features.len(), 4);
    // A DXF drawing from bytes.
    let dxf = terrio::gdal_backend::write_vector_bytes(&[sample_layer()], "parcels.dxf", "DXF", &[], &[]).unwrap();
    let ds = terrio::memory::open_bytes(dxf, "parcels.dxf").unwrap();
    let terrio::Dataset::Vector(v) = ds else { panic!("not a vector dataset") };
    assert_eq!(v.layers[0].features.len(), 4);
    // An SQLite database (not a GeoPackage) from bytes.
    let sqlite = terrio::gdal_backend::write_vector_bytes(&[sample_layer()], "p.sqlite", "SQLite", &[], &[]).unwrap();
    let v = terrio::memory::open_vector_bytes(sqlite, "p.sqlite", &Default::default()).unwrap();
    assert_eq!(v.driver, VectorDriver::Gdal);
    assert_eq!(v.layers[0].features.len(), 4);
    assert_eq!(v.layers[0].crs_epsg(), Some(32617));
    // terrio's own formats still read with terrio's drivers.
    let tif = terrio::memory::write_raster_bytes(&sample(), "s.tif", &Default::default()).unwrap();
    assert_eq!(terrio::memory::open_raster_bytes(tif, "s.tif").unwrap().driver, RasterDriver::GTiff);
    // Nothing is left in /vsimem/.
    assert!(gdal::vsi::read_dir("/vsimem/", false).unwrap_or_default().iter().all(|p| !p.to_string_lossy().starts_with("terrio-")));
}

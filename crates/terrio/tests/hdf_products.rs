//! Product-specific HDF5 and netCDF georeferencing, complex datasets and netCDF groups, against
//! the values GDAL 3.12 reports for the same files: HDF-EOS5 grids and swaths, COSMO-SkyMed,
//! complex (compound) datasets, netCDF geolocation arrays (CF `coordinates`, NASA ocean color,
//! EMIT, irregular axes), and NASA HDF-EOS5 products.
//!
//! The files come from `scripts/fetch_test_data.sh` (GDAL's test files in `gdal_hdf5` and
//! `gdal_netcdf`; NASA products in `hdfeos5`, fetched only with Earthdata Login credentials). The
//! tests skip themselves without `TERRIO_TEST_DATA` or the files.

// Geotransforms as `gdalinfo` prints them, digit for digit.
#![allow(clippy::excessive_precision)]

use std::path::PathBuf;
use terrio::RasterDataset;

fn data(name: &str) -> Option<String> {
    let p = PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?).join(name);
    p.exists().then(|| p.to_string_lossy().into_owned())
}

fn open(name: &str) -> RasterDataset {
    terrio::open_raster(name).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn assert_gt(ds: &RasterDataset, expected: [f64; 6], what: &str) {
    let gt = ds.geo_transform();
    for (a, b) in gt.iter().zip(expected) {
        assert!((a - b).abs() <= 1e-9 * b.abs().max(1.0), "{what}: {gt:?} != {expected:?}");
    }
}

fn subdataset_desc(ds: &RasterDataset, i: usize) -> String {
    let key = format!("SUBDATASET_{i}_DESC");
    ds.raster.metadata.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone()).unwrap_or_default()
}

/// HDF-EOS5 grids: the geotransform and CRS of their GCTP projection, as GDAL reads them.
#[test]
fn hdfeos5_grids() {
    let Some(utm) = data("gdal_hdf5/dummy_HDFEOS_with_utm_projection.h5") else { return };
    let ds = open(&utm);
    assert_eq!(ds.crs().epsg, Some(32611));
    assert_gt(&ds, [440720.0, 60.0, 0.0, 3751320.0, 0.0, -60.0], "UTM grid");

    let sinu = open(&data("gdal_hdf5/dummy_HDFEOS_with_sinu_projection.h5").unwrap());
    // MODIS's sphere: no EPSG code, as GDAL.
    assert_eq!(sinu.crs().epsg, None);
    let proj = sinu.crs().proj4.unwrap_or_default();
    assert!(proj.contains("+proj=sinu") && proj.contains("+R=6371007.181"), "{proj}");
    assert_gt(&sinu, [-1111950.519667, 277987.62991675, 0.0, 5559752.598333, 0.0, -222390.1039332], "sinusoidal grid");
    assert_eq!(sinu.band_count(), 3);

    let geo = open(&data("gdal_hdf5/dummy_HDFEOS_with_geo_projection.h5").unwrap());
    assert_eq!(geo.crs().epsg, Some(4326));
    let gt = geo.geo_transform();
    assert!((gt[0] - -117.641169444442625).abs() < 1e-9 && (gt[3] - 33.900602777778275).abs() < 1e-9, "{gt:?}");
}

/// HDF-EOS5 swaths: rows, columns and bands from the dimension maps, the longitude and latitude
/// fields as geolocation arrays (GDAL's `GEOLOCATION` metadata), and GDAL's subdataset
/// descriptions with dimension names.
#[test]
fn hdfeos5_swaths() {
    let Some(p) = data("gdal_hdf5/dummy_HDFEOS_swath.h5") else { return };
    let container = open(&p);
    assert_eq!(
        subdataset_desc(&container, 1),
        "[(Band=2)x(AlongTrack=3)x(CrossTrack=4)] //HDFEOS/SWATHS/MySwath/Data_Fields/MyDataField (8-bit unsigned character)"
    );
    let ds = open(&format!("HDF5:\"{p}\"://HDFEOS/SWATHS/MySwath/Data_Fields/MyDataField"));
    assert_eq!((ds.raster.cols, ds.raster.rows, ds.band_count()), (4, 3, 2));
    let g = ds.geolocation.expect("geolocation arrays");
    assert_eq!(g.x_dataset, format!("HDF5:\"{p}\"://HDFEOS/SWATHS/MySwath/Geolocation_Fields/Longitude"));
    assert_eq!(g.y_dataset, format!("HDF5:\"{p}\"://HDFEOS/SWATHS/MySwath/Geolocation_Fields/Latitude"));
    assert_eq!((g.x_band, g.y_band, g.pixel_offset, g.pixel_step, g.line_offset, g.line_step), (1, 1, 0.0, 1.0, 0.0, 1.0));
    assert!(g.pixel_center && g.srs.contains("AXIS[\"Latitude\",NORTH]"));
    let items = g.items();
    assert_eq!(items.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>()[1..5], ["X_DATASET", "X_BAND", "Y_DATASET", "Y_BAND"]);
    // The longitudes are a raster of their own, which reads back.
    let lon = open(&g.x_dataset);
    assert_eq!((lon.raster.cols, lon.raster.rows), (4, 3));
}

/// Complex (compound) HDF5 datasets: one complex band of CFloat32 as GDAL reads it, real and
/// imaginary parts as bands; statistics of the real parts.
#[test]
fn hdf5_complex_datasets() {
    let Some(p) = data("gdal_hdf5/complex.h5") else { return };
    let container = open(&p);
    assert_eq!(subdataset_desc(&container, 2), "[5x5] //f32 (complex, 32-bit floating-point)");
    // GDAL reads k+kj in f16, 2k+2kj in f32, 3k+3kj in f64 (as the file holds them).
    for (name, t, step) in [("f16", "CFloat32", 1.0), ("f32", "CFloat32", 2.0), ("f64", "CFloat64", 3.0)] {
        let ds = open(&format!("HDF5:\"{p}\"://{name}"));
        assert!(ds.complex, "{name}");
        assert_eq!((ds.band_count(), ds.raster.bands), (1, 2), "{name}");
        assert_eq!(ds.data_type_name(), t, "{name}");
        let v = ds.raster.data.to_f64_vec();
        let expected: Vec<f64> = (0..25).map(|k| f64::from(k) * step).collect();
        assert_eq!(&v[..25], &expected[..], "{name} real parts");
        assert_eq!(&v[25..], &expected[..], "{name} imaginary parts");
        let info = terrio::info::raster_info(&ds, &terrio::info::InfoOptions { statistics: true, ..Default::default() });
        assert_eq!(info.band_count, 1);
        let s = info.bands[0].statistics.unwrap();
        assert_eq!((s.min, s.max, s.mean), (0.0, 24.0 * step, 12.0 * step), "{name}");
    }
}

/// Complex rasters written as complex GeoTIFF samples (`SampleFormat` 6, GDAL's CFloat32).
#[test]
fn complex_geotiff_written() {
    let Some(p) = data("gdal_hdf5/complex.h5") else { return };
    let ds = open(&format!("HDF5:\"{p}\"://f32"));
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("c.tif");
    terrio::write_raster_dataset(&ds, out.to_str().unwrap(), &Default::default()).unwrap();
    let bytes = std::fs::read(&out).unwrap();
    let dec = tiff::decoder::Decoder::new(std::io::Cursor::new(bytes));
    let mut dec = dec.unwrap();
    let format = dec.find_tag_unsigned_vec::<u16>(tiff::tags::Tag::SampleFormat).unwrap().unwrap();
    let bits = dec.find_tag_unsigned_vec::<u16>(tiff::tags::Tag::BitsPerSample).unwrap().unwrap();
    assert_eq!((format, bits), (vec![6], vec![64]));
}

/// COSMO-SkyMed: the geocoded product's UTM projection and geotransform, WGS 84 for the others,
/// as GDAL reads them.
#[test]
fn cosmo_skymed_georeferencing() {
    let Some(gec) = data("gdal_hdf5/CSK_GEC.h5") else { return };
    let ds = open(&format!("HDF5:\"{gec}\"://S01/SBI"));
    assert_eq!(ds.crs().epsg, Some(32633));
    assert_gt(&ds, [275592.5, 2.5, 0.0, 4998152.5, 0.0, -2.5], "CSK GEC");
    let dgm = open(&format!("HDF5:\"{}\"://S01/SBI", data("gdal_hdf5/CSK_DGM.h5").unwrap()));
    assert_eq!(dgm.crs().epsg, Some(4326));
}

/// Groups that contain themselves (hard links to an ancestor) are walked once.
#[test]
fn recursive_groups_terminate() {
    let Some(p) = data("gdal_hdf5/recursive_groups.h5") else { return };
    let start = std::time::Instant::now();
    // No raster in it (GDAL opens an empty container): an error, quickly.
    assert!(terrio::open_raster(&p).is_err());
    assert!(start.elapsed().as_secs() < 10);
}

/// netCDF geolocation arrays, as GDAL's netCDF driver finds them.
#[test]
fn netcdf_geolocation_arrays() {
    let Some(dir) = data("gdal_netcdf") else { return };
    let geoloc = |name: &str| {
        let p = format!("{dir}/{name}");
        let ds = terrio::open_raster(&p).unwrap_or_else(|e| panic!("{name}: {e}"));
        let g = ds.geolocation.clone().unwrap_or_else(|| panic!("{name}: no geolocation"));
        (p, ds, g)
    };
    // CF `coordinates`.
    let (p, _, g) = geoloc("sentinel5p_fake.nc");
    assert_eq!((g.x_dataset, g.y_dataset), (format!("NETCDF:\"{p}\":longitude"), format!("NETCDF:\"{p}\":latitude")));
    // Irregular longitude and latitude axes: no geotransform, no CRS.
    let (p, ds, g) = geoloc("GLMELT_4X5.OCN.nc");
    assert_eq!(g.x_dataset, format!("NETCDF:\"{p}\":lon"));
    assert_eq!(ds.crs().epsg, None);
    assert_eq!(ds.geo_transform()[1], 1.0);
    // NASA ocean color level 2 and EMIT.
    let snpp = format!("{dir}/fake_SNPP_VIIRS.20230406T024200.L2.OC.NRT.nc");
    let ds = open(&format!("NETCDF:\"{snpp}\":/geophysical_data/aot_862"));
    assert_eq!(ds.geolocation.unwrap().x_dataset, format!("NETCDF:\"{snpp}\":/navigation_data/longitude"));
    let emit = format!("{dir}/fake_EMIT_L2B_MIN.nc");
    let ds = open(&format!("NETCDF:\"{emit}\":some_var"));
    assert_eq!(ds.geolocation.unwrap().y_dataset, format!("NETCDF:\"{emit}\":/location/lat"));
    // No CRS from longitude and latitude axes alone.
    assert_eq!(open(&format!("{dir}/cf_lon_lat_with_coordinates_no_crs.nc")).crs().epsg, None);
    // A netCDF-4 variable written without fill value has no nodata value.
    assert_eq!(open(&format!("{dir}/uint16_netcdf4_without_fill.nc")).nodata(), None);
}

/// NASA HDF-EOS5 products (fetched with Earthdata Login credentials only): the georeferencing GDAL
/// reports.
#[test]
fn nasa_hdfeos5_products() {
    if let Some(p) = data("hdfeos5/VNP46A2.A2022365.h11v09.002.2025135012224.h5") {
        // VIIRS Black Marble: a geographic grid.
        let ds = open(&format!("HDF5:\"{p}\"://HDFEOS/GRIDS/VIIRS_Grid_DNB_2d/Data_Fields/DNB_BRDF-Corrected_NTL"));
        assert_eq!(ds.crs().epsg, Some(4326));
        assert_gt(&ds, [-70.0, 10.0 / 2400.0, 0.0, 0.0, 0.0, -10.0 / 2400.0], "VNP46A2");
    }
    if let Some(p) = data("hdfeos5/VNP09GA.A2026284.h01v11.002.2026286234935.h5") {
        // VIIRS surface reflectance: a sinusoidal grid on MODIS's sphere.
        let ds = open(&format!("HDF5:\"{p}\"://HDFEOS/GRIDS/VIIRS_Grid_1km_2D/Data_Fields/SensorAzimuth_1"));
        assert_eq!(ds.crs().epsg, None);
        assert_gt(
            &ds,
            [-18903158.834332998842001, 926.625433054997870, 0.0, -2223901.039332999847829, 0.0, -926.625433055833355],
            "VNP09GA",
        );
    }
    if let Some(p) = data("hdfeos5/AMSR_U2_L3_SeaIce12km_B04_20250831.he5") {
        // AMSR2 sea ice: polar stereographic grids (XDim and YDim given as the grid's sizes).
        let ds = open(&format!("HDF5:\"{p}\"://HDFEOS/GRIDS/NpPolarGrid12km/Data_Fields/SI_12km_NH_18H_ASC"));
        assert_gt(&ds, [-3850000.0, 12500.0, 0.0, 5850000.0, 0.0, -12500.0], "AU_SI12");
        let proj = ds.crs().proj4.unwrap_or_default();
        assert!(proj.contains("+lat_ts=70") && proj.contains("+lon_0=-45") && proj.contains("+R=6378273"), "{proj}");
    }
    if let Some(p) = data("hdfeos5/AMSR_U2_L2_Rain_V02_202508312319_A.he5") {
        // AMSR2 precipitation: a swath.
        let container = open(&p);
        assert_eq!(
            subdataset_desc(&container, 1),
            "[(nscans=1980)x(npixs=392)] //HDFEOS/SWATHS/AMSR2_L1R/Data_Fields/CloudWaterPath (32-bit floating-point)"
        );
        let ds = open(&format!("HDF5:\"{p}\"://HDFEOS/SWATHS/AMSR2_L1R/Data_Fields/CloudWaterPath"));
        assert_eq!(ds.geolocation.unwrap().x_dataset, format!("HDF5:\"{p}\"://HDFEOS/SWATHS/AMSR2_L1R/Geolocation_Fields/Longitude"));
    }
}

/// Rasters written into netCDF-4 groups, read back by their group paths.
#[test]
fn netcdf_groups_written() {
    let make = |v: f32| {
        let cfg = terrio::RasterConfig {
            cols: 6,
            rows: 4,
            bands: 1,
            x_min: 500_000.0,
            y_min: 4_100_000.0,
            cell_size: 30.0,
            nodata: -9999.0,
            data_type: terrio::DataType::F32,
            crs: terrio::crs::SpatialRef::from_epsg(32613).unwrap().to_crs_info(),
            ..Default::default()
        };
        let data = terrio::raster::RasterData::F32((0..24).map(|i| i as f32 * v).collect());
        RasterDataset::new(terrio::Raster::from_data_native(cfg, data).unwrap(), true)
    };
    let (a, b) = (make(1.0), make(2.0));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("groups.nc").to_string_lossy().into_owned();
    terrio::write_netcdf_groups(&[("/terrain/a", &a), ("/terrain/b", &b)], &path, &Default::default()).unwrap();
    for (group, src) in [("/terrain/a", &a), ("/terrain/b", &b)] {
        let back = open(&format!("NETCDF:\"{path}\":{group}/Band1"));
        assert_eq!(back.raster.data.to_f64_vec(), src.raster.data.to_f64_vec(), "{group}");
        assert_eq!(back.crs().epsg, Some(32613), "{group}");
        assert_eq!(back.geo_transform(), src.geo_transform(), "{group}");
        assert_eq!(back.nodata(), Some(-9999.0), "{group}");
    }
    // One raster in a group, with the GROUP creation option.
    let one = dir.path().join("one.nc").to_string_lossy().into_owned();
    let options = terrio::RasterWriteOptions { creation_options: vec![("GROUP".into(), "/science/grid".into())], ..Default::default() };
    terrio::write_raster_dataset(&a, &one, &options).unwrap();
    assert_eq!(open(&format!("NETCDF:\"{one}\":/science/grid/Band1")).raster.data.to_f64_vec(), a.raster.data.to_f64_vec());
    // Classic formats have no groups.
    let options = terrio::RasterWriteOptions {
        creation_options: vec![("GROUP".into(), "/g".into()), ("FORMAT".into(), "NC".into())],
        ..Default::default()
    };
    assert!(terrio::write_raster_dataset(&a, &one, &options).is_err());
}

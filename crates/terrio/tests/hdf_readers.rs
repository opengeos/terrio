// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Reader compatibility through public APIs, with and without the HEC-RAS engine.
//! The NetCDF and HDF5 raster expectations match GDAL 3.12.1. The HEC-RAS values come
//! from the synthetic plan documented in `scripts/make_hecras_fixture.py`.

use std::path::PathBuf;
use terrio::DataType;
use terrio::formats::RasterDriver;

fn fixture(name: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name).to_string_lossy().into_owned()
}

#[test]
fn netcdf_arrays_and_hdf5_subdatasets_read_without_the_engine() {
    let path = fixture("netcdf_bands/emit_bands.nc");
    let expected: serde_json::Value = serde_json::from_str(include_str!("data/netcdf_bands/expected.json")).unwrap();
    let expected = &expected["emit_bands.nc:radiance"];
    let raster = terrio::open_raster(&format!(r#"NETCDF:"{path}":radiance"#)).unwrap();
    assert_eq!(raster.driver, RasterDriver::NetCDF);
    assert_eq!((raster.raster.cols, raster.raster.rows, raster.raster.bands), (4, 3, 5));
    assert_eq!(raster.raster.data_type, DataType::F32);
    let values: Vec<_> = expected["values"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap()).collect();
    assert_eq!(raster.raster.data.to_f64_vec(), values);

    // This path shares byte-backed HDF5 storage with netcdf-reader, then reads a selected band.
    let arrays = terrio::multidim::open_bytes_owned(std::fs::read(&path).unwrap()).unwrap();
    let selected = arrays.array("radiance").unwrap().view().isel("bands", 2).unwrap().read().unwrap();
    assert_eq!(selected.shape, [3, 4]);
    assert_eq!(selected.data_type, DataType::F32);
    assert_eq!(selected.data().unwrap().to_f64_vec(), (0..12).map(|i| (i * 5 + 2) as f64).collect::<Vec<_>>());

    let raster = terrio::open_raster(&format!(r#"HDF5:"{path}"://location/glt_x"#)).unwrap();
    assert_eq!(raster.driver, RasterDriver::HDF5);
    assert_eq!((raster.raster.cols, raster.raster.rows, raster.raster.bands), (4, 3, 1));
    assert_eq!(raster.raster.data_type, DataType::I32);
    assert_eq!(raster.raster.data.to_f64_vec(), (0..12).map(|i| i as f64).collect::<Vec<_>>());
}

#[cfg(feature = "hecras")]
#[test]
fn hecras_vectors_and_hdf5_rasters_read_in_the_same_build() {
    let path = fixture("hecras/results_mesh_xs.h5");
    let options = terrio::VectorOpenOptions { layers: vec!["mesh_cells".into()], ..Default::default() };
    let vector = terrio::open_vector(&path, &options).unwrap();
    assert_eq!(vector.driver, terrio::formats::VectorDriver::HECRAS);
    assert_eq!(vector.layers.len(), 1);
    assert_eq!(vector.layers[0].features.iter().map(|f| f.fid).collect::<Vec<_>>(), [0, 1]);

    // The engine owns its reader; the same file's raster goes through Terrio's direct reader.
    let raster = terrio::open_raster(&format!(r#"HDF5:"{path}"://Geometry/2D_Flow_Areas/Good/Cells_Center_Coordinate"#)).unwrap();
    assert_eq!(raster.driver, RasterDriver::HDF5);
    assert_eq!((raster.raster.cols, raster.raster.rows, raster.raster.bands), (2, 3, 1));
    assert_eq!(raster.raster.data_type, DataType::F64);
    assert_eq!(raster.raster.data.to_f64_vec(), [0.5, 0.5, 1.5, 0.5, 2.5, 0.5]);
}

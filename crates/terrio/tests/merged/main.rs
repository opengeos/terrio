//! The integration tests that neither change nor measure process-wide state, built as modules of
//! one test binary: one compile and link of terrio's test dependencies instead of one per file.
//! A file that sets driver allow-lists, environment variables, configuration or CRS settings (grid
//! paths, network), reads process-wide counters, installs a global allocator, measures the process's
//! memory, runs its own test binary again, or is run by name (CI, docs, scripts) is a test target of
//! its own instead (`[[test]]` in Cargo.toml). Run one
//! file's tests with `cargo test -p terrio --test merged <file>::`.

// Each file declares the shared helpers (`tests/common/`) it uses as its own modules, as when it was
// a binary of its own.
#![allow(clippy::duplicate_mod)]

#[path = "../cad.rs"]
mod cad;
#[path = "../catalog.rs"]
mod catalog;
#[path = "../citygml.rs"]
mod citygml;
#[path = "../cityjson.rs"]
mod cityjson;
#[path = "../crs_compat.rs"]
mod crs_compat;
#[path = "../deep_wkt_sinks.rs"]
mod deep_wkt_sinks;
#[path = "../dem_calc_real_data.rs"]
mod dem_calc_real_data;
#[path = "../epsg_raster_wkt.rs"]
mod epsg_raster_wkt;
#[path = "../esrijson.rs"]
mod esrijson;
#[path = "../esrijson_service.rs"]
mod esrijson_service;
#[path = "../esripbf_service.rs"]
mod esripbf_service;
#[path = "../flatgeobuf_hostile.rs"]
mod flatgeobuf_hostile;
#[path = "../gdal_redirect_policy.rs"]
mod gdal_redirect_policy;
#[path = "../geocentric.rs"]
mod geocentric;
#[path = "../geolocation_warp.rs"]
mod geolocation_warp;
#[path = "../geopackage.rs"]
mod geopackage;
#[path = "../geoparquet_arrow.rs"]
mod geoparquet_arrow;
#[path = "../georeferencing.rs"]
mod georeferencing;
#[path = "../geotiff_complex.rs"]
mod geotiff_complex;
#[path = "../geotiff_compound_crs.rs"]
mod geotiff_compound_crs;
#[path = "../geotiff_unit_names.rs"]
mod geotiff_unit_names;
#[path = "../gpkg_layers_writer.rs"]
mod gpkg_layers_writer;
#[path = "../grib.rs"]
mod grib;
#[path = "../hdf5_compound.rs"]
mod hdf5_compound;
#[path = "../hdf_products.rs"]
mod hdf_products;
#[path = "../hecras_driver.rs"]
mod hecras_driver;
#[path = "../hecras_results.rs"]
mod hecras_results;
#[path = "../hecras_synthetic.rs"]
mod hecras_synthetic;
#[path = "../identify_features.rs"]
mod identify_features;
#[path = "../jts_behavior.rs"]
mod jts_behavior;
#[path = "../layer_selection.rs"]
mod layer_selection;
#[path = "../lazy_tiff_errors.rs"]
mod lazy_tiff_errors;
#[path = "../multidim.rs"]
mod multidim;
#[path = "../oapif_service.rs"]
mod oapif_service;
#[path = "../operation_band_nodata.rs"]
mod operation_band_nodata;
#[path = "../ops_real_data.rs"]
mod ops_real_data;
#[path = "../parquet_zstd.rs"]
mod parquet_zstd;
#[path = "../pointcloud.rs"]
mod pointcloud;
#[path = "../pointcloud_features.rs"]
mod pointcloud_features;
#[path = "../pointcloud_more.rs"]
mod pointcloud_more;
#[path = "../pointcloud_write_filters.rs"]
mod pointcloud_write_filters;
#[path = "../processing.rs"]
mod processing;
#[path = "../proj_string_wkt.rs"]
mod proj_string_wkt;
#[path = "../raster_analysis.rs"]
mod raster_analysis;
#[path = "../raster_file_identity.rs"]
mod raster_file_identity;
#[path = "../raster_info.rs"]
mod raster_info;
#[path = "../raster_manage.rs"]
mod raster_manage;
#[path = "../raster_publication.rs"]
mod raster_publication;
#[path = "../raster_retile.rs"]
mod raster_retile;
#[path = "../raw_lazy.rs"]
mod raw_lazy;
#[path = "../real_data.rs"]
mod real_data;
#[path = "../render_remote.rs"]
mod render_remote;
#[path = "../resampled_reads.rs"]
mod resampled_reads;
#[path = "../rgb_extra_samples.rs"]
mod rgb_extra_samples;
#[path = "../rotated_pole.rs"]
mod rotated_pole;
#[path = "../shapefile_deleted.rs"]
mod shapefile_deleted;
#[path = "../sparse_prefetch.rs"]
mod sparse_prefetch;
#[path = "../sparse_reads.rs"]
mod sparse_reads;
#[path = "../sqlite_encodings.rs"]
mod sqlite_encodings;
#[path = "../stac_geozarr.rs"]
mod stac_geozarr;
#[path = "../stacta.rs"]
mod stacta;
#[path = "../tiff_limits.rs"]
mod tiff_limits;
#[path = "../topology.rs"]
mod topology;
#[path = "../unreferenced_writes.rs"]
mod unreferenced_writes;
#[path = "../user_crs.rs"]
mod user_crs;
#[path = "../vector_ops.rs"]
mod vector_ops;
#[path = "../vector_sql.rs"]
mod vector_sql;
#[path = "../white_is_zero.rs"]
mod white_is_zero;
#[path = "../wkt2.rs"]
mod wkt2;
#[path = "../zip_duplicate_names.rs"]
mod zip_duplicate_names;
#[path = "../zonal_remote.rs"]
mod zonal_remote;
#[path = "../zstd_input_limits.rs"]
mod zstd_input_limits;
#[path = "../zstd_tiff_samples.rs"]
mod zstd_tiff_samples;

/// Every integration test file is built once: a module of this binary, or a test target of its own
/// in Cargo.toml.
#[test]
fn every_integration_test_file_is_built_once() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let main = std::fs::read_to_string(dir.join("tests/merged/main.rs")).unwrap();
    let manifest = std::fs::read_to_string(dir.join("Cargo.toml")).unwrap();
    let merged: Vec<&str> = main.lines().filter_map(|l| l.strip_prefix("mod ")?.strip_suffix(';')).collect();
    let apart: Vec<&str> = manifest.lines().filter_map(|l| l.strip_prefix("path = \"tests/")?.strip_suffix(".rs\"")).collect();
    for entry in std::fs::read_dir(dir.join("tests")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "rs") {
            let name = path.file_stem().unwrap().to_str().unwrap();
            let built = usize::from(merged.contains(&name)) + usize::from(apart.contains(&name));
            assert_eq!(built, 1, "tests/{name}.rs: a module of tests/merged/main.rs or a [[test]] of Cargo.toml, once");
        }
    }
}

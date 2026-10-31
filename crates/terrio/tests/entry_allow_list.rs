//! Process-wide allow-lists of one kind and the entry points that open a dataset of either kind, in
//! a binary of its own because a list once set cannot be lifted: under a raster list with PMTiles
//! and Zarr and a vector list without them, rasters read from a PMTiles archive and Zarr stores
//! (under any name) open as before, while the tiles API, whose archives may hold vector tiles,
//! refuses the archive; a point cloud, which only `TERRIO_ALLOWED_DRIVERS` restricts, is described
//! by `info_path` as before; the multidimensional API refuses a netCDF file. Format identification
//! judges a driver by the list of every kind it belongs to, as `open` does: PMTiles identifies
//! nothing, and the content check, which reads an archive through the tiles API, refuses it; the
//! Zarr store and the point cloud are identified; netCDF is neither identified nor checked.

#![cfg(all(feature = "tiles", feature = "zarr", feature = "pointcloud", feature = "netcdf-hdf5"))]

use terrio::formats::{Kind, set_allowed_drivers};
use terrio::pointcloud::{Dimension, PointCloud, Values};
use terrio::raster::RasterData;
use terrio::{DataType, RasterDataset};

fn refused<T>(result: terrio::Result<T>) -> String {
    match result {
        Ok(_) => panic!("opened"),
        Err(e) => e.to_string(),
    }
}

/// A 5 x 3 one-band raster in UTM zone 10N.
fn raster() -> RasterDataset {
    let (cols, rows) = (5, 3);
    let data = RasterData::from_f64_vec(DataType::U8, (0..cols * rows).map(|v| v as f64).collect());
    let cfg = terrio::RasterConfig {
        cols,
        rows,
        bands: 1,
        nodata: -1.0,
        data_type: DataType::U8,
        crs: terrio::crs::SpatialRef::from_epsg(32610).unwrap().to_crs_info(),
        ..Default::default()
    };
    let mut r = terrio::Raster::from_data_native(cfg, data).unwrap();
    r.x_min = 500000.0;
    r.y_min = 4000000.0;
    r.cell_size_x = 30.0;
    r.cell_size_y = 30.0;
    RasterDataset::new(r, true)
}

#[test]
fn lists_of_one_kind_leave_the_opens_of_other_kinds_as_they_were() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
    let (pmtiles, store, las) = (path("t.pmtiles"), path("store"), path("x.las"));
    terrio::write_raster_dataset(&raster(), &pmtiles, &Default::default()).unwrap();
    terrio::write_raster_dataset(&raster(), &path("s.zarr"), &Default::default()).unwrap();
    std::fs::rename(path("s.zarr"), &store).unwrap();
    let dims = vec![
        Dimension::new("x", Values::F64(vec![0.0, 1.0])),
        Dimension::new("y", Values::F64(vec![0.0, 1.0])),
        Dimension::new("z", Values::F64(vec![5.0, 6.0])),
    ];
    terrio::pointcloud::write_points(&PointCloud::new(dims, Default::default()), &las, &Default::default()).unwrap();
    let nc = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/rotated_pole/eur11.nc");

    set_allowed_drivers(Some(&["GTiff", "PMTiles", "Zarr"]), Some(Kind::Raster)).unwrap();
    set_allowed_drivers(Some(&["GeoJSON"]), Some(Kind::Vector)).unwrap();
    assert_eq!(terrio::open_raster(&pmtiles).unwrap().driver.name(), "PMTiles");
    assert!(terrio::open_raster_lazy(&pmtiles).is_ok());
    assert_eq!(
        refused(terrio::tiles::TileArchive::open(&pmtiles)),
        format!("'{pmtiles}' holds PMTiles data, and the PMTiles driver is not allowed (allowed: GEOJSON)")
    );
    assert!(matches!(terrio::info::info_path(&las, &Default::default()), Ok(terrio::info::Info::PointCloud(_))));
    assert_eq!(terrio::multidim::open(&store).unwrap().driver, "Zarr");
    assert_eq!(
        refused(terrio::multidim::open(nc)),
        format!("'{nc}' holds netCDF data, and the netCDF driver is not allowed (allowed: GTIFF, PMTILES, ZARR)")
    );

    let found = |p: &str| terrio::inspection::identify(p, false).unwrap().map(|i| i.driver);
    assert_eq!(found(&pmtiles), None);
    assert_eq!(found(&store).as_deref(), Some("Zarr"));
    assert_eq!(found(&las).as_deref(), Some("LAS"));
    assert_eq!(found(nc), None);
    assert_eq!(
        refused(terrio::check::check_path(&pmtiles, &Default::default())),
        format!("'{pmtiles}' holds PMTiles data, and the PMTiles driver is not allowed (allowed: GEOJSON)")
    );
    assert!(refused(terrio::check::check_path(nc, &Default::default())).contains("the netCDF driver is not allowed"));
}

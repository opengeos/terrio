// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

//! Zonal statistics over raster paths under options set for the calling thread, as
//! `terrio.rio.Env` sets them: the threads that open the paths for their headers, and those that
//! reduce them, see those options too. With several paths, threads other than the calling one
//! open and reduce them.

#![cfg(not(target_family = "wasm"))]

use std::collections::HashMap;
use terrio::ops::zonal::{Statistic, ZonalBand, ZonalOptions, ZonalRaster, ZonalSource, zonal_stats};
use terrio::vector::Coord;
use terrio::{DataType, Feature, FieldValue, Geometry, Layer, Raster, RasterConfig, RasterDataset};

/// A layer of one zone, the square from (0, 0) to (2, 2), without a CRS.
fn zones() -> Layer {
    let mut zones = Layer::new("zones");
    let square = vec![Coord::xy(0.0, 0.0), Coord::xy(2.0, 0.0), Coord::xy(2.0, 2.0), Coord::xy(0.0, 2.0), Coord::xy(0.0, 0.0)];
    zones.features.push(Feature { fid: 0, geometry: Some(Geometry::polygon(square, vec![])), attributes: vec![] });
    zones
}

/// The sum of band 1 of each raster over the zone, with `options` set for the calling thread.
fn sums(paths: &[String], options: &[(&str, &str)]) -> Result<Vec<FieldValue>, String> {
    let mut sources: Vec<ZonalSource> = paths
        .iter()
        .enumerate()
        .map(|(i, path)| ZonalSource { raster: ZonalRaster::Path(path.clone()), bands: vec![ZonalBand::new(1, format!("r{i}"))] })
        .collect();
    let zonal = ZonalOptions { stats: vec![Statistic::Sum], ..Default::default() };
    terrio::config::push_scope(options.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect::<HashMap<_, _>>());
    let out = zonal_stats(&zones(), &mut sources, &zonal);
    terrio::config::pop_scope();
    out.map(|layer| layer.features[0].attributes.clone()).map_err(|e| e.to_string())
}

/// `n` Esri ASCII grids of the values 1 to 4 over the zone.
fn grids(dir: &std::path::Path, n: usize) -> Vec<String> {
    (0..n)
        .map(|i| {
            let path = dir.join(format!("g{i}.asc"));
            std::fs::write(&path, "ncols 2\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2\n3 4\n").unwrap();
            path.to_str().unwrap().to_string()
        })
        .collect()
}

#[test]
fn a_driver_allow_list_set_for_the_thread_refuses_every_raster_path() {
    let dir = tempfile::tempdir().unwrap();
    for n in [1, 4, 8] {
        let paths = grids(dir.path(), n);
        assert_eq!(sums(&paths, &[]), Ok(vec![FieldValue::Float(10.0); n]), "{n} grids, no list");
        let refused = sums(&paths, &[("TERRIO_ALLOWED_DRIVERS", "GTiff")]).unwrap_err();
        assert!(refused.ends_with("the AAIGRID driver is not allowed (allowed: GTIFF)"), "{n} grids: {refused}");
    }
}

/// Four PNG rasters of the values 1 to 4, each georeferenced over the zone by itself and far
/// from it by a world file next to it: under `TERRIO_DISABLE_READDIR_ON_OPEN=EMPTY_DIR` set for
/// the calling thread, neither the threads that open them for their headers nor those that reduce
/// them read the world files, so the zone covers each raster.
#[test]
fn options_set_for_the_thread_reach_the_threads_reading_raster_paths() {
    let dir = tempfile::tempdir().unwrap();
    let paths: Vec<String> = (0..4)
        .map(|i| {
            let cfg = RasterConfig { cols: 2, rows: 2, bands: 1, data_type: DataType::U8, ..Default::default() };
            let mut ds = RasterDataset::new(Raster::from_data(cfg, vec![1.0, 2.0, 3.0, 4.0]).unwrap(), false);
            ds.set_geo_transform([0.0, 1.0, 0.0, 2.0, 0.0, -1.0]);
            let path = dir.path().join(format!("p{i}.png"));
            let options = terrio::RasterWriteOptions { driver: Some(terrio::formats::RasterDriver::PNG), ..Default::default() };
            terrio::write_raster_dataset(&ds, path.to_str().unwrap(), &options).unwrap();
            for sidecar in ["wld", "png.aux.xml"] {
                let _ = std::fs::remove_file(path.with_extension(sidecar));
            }
            std::fs::write(path.with_extension("pgw"), "10\n0\n0\n-10\n500005\n4999995\n").unwrap();
            path.to_str().unwrap().to_string()
        })
        .collect();
    let sidecars_off = [("TERRIO_DISABLE_READDIR_ON_OPEN", "EMPTY_DIR")];
    assert_eq!(sums(&paths, &sidecars_off), Ok(vec![FieldValue::Float(10.0); 4]));
    assert_eq!(sums(&paths, &[]), Ok(vec![FieldValue::Null; 4]), "the world files place the rasters far from the zone");
}

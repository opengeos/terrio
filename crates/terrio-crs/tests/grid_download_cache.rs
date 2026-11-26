// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Grids downloaded whole enter the cache only verified, through a scratch file of each download's
//! own: concurrent downloads of one grid by threads of one process all succeed, and a download
//! that is not a grid or is larger than the limit leaves nothing in the cache. Its own test
//! binary: the grid transport, network setting and cache directory are process-wide.
#![cfg(all(feature = "grid-transport", not(target_family = "wasm")))]

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use terrio_crs::{
    SpatialRef, Transformer,
    network::{self, GridStream, GridTransport, MAX_GRID_DOWNLOAD_BYTES},
};

/// Threads transforming at once with the concurrently downloaded grid.
const THREADS: usize = 4;

/// A geoid grid of 2 x 2 nodes, from 110°W to 90°W and 30°N to 50°N, of 0.5 m everywhere.
fn geoid_grid() -> Vec<u8> {
    use tiff::{
        encoder::{TiffEncoder, colortype::Gray32Float},
        tags::Tag,
    };
    let mut bytes = Cursor::new(Vec::new());
    {
        let mut encoder = TiffEncoder::new(&mut bytes).unwrap();
        let mut image = encoder.new_image::<Gray32Float>(2, 2).unwrap();
        image.encoder().write_tag(Tag::ModelPixelScaleTag, &[20.0_f64, 20.0, 0.0][..]).unwrap();
        image.encoder().write_tag(Tag::ModelTiepointTag, &[0.0_f64, 0.0, 0.0, -110.0, 50.0, 0.0][..]).unwrap();
        image.write_data(&[0.5_f32; 4]).unwrap();
    }
    bytes.into_inner()
}

/// A CDN that cannot be read in place. Each download writes half of the grid, waits (for a
/// while at most) until every thread is downloading too, then writes the rest; it records the
/// scratch paths it was given.
#[derive(Default)]
struct WholeOnly {
    paths: Mutex<Vec<PathBuf>>,
    arrived: Condvar,
}
impl GridTransport for WholeOnly {
    fn open(&self, _: &str) -> std::io::Result<Box<dyn GridStream>> {
        Err(std::io::Error::other("no range requests"))
    }
    fn download(&self, url: &str, path: &Path) -> std::io::Result<()> {
        let grid = geoid_grid();
        if url.ends_with("/terrio_not_a_grid.tif") {
            return std::fs::write(path, b"not a grid");
        }
        if url.ends_with("/terrio_oversized_grid.tif") {
            std::fs::write(path, &grid)?;
            return std::fs::File::options().write(true).open(path)?.set_len(MAX_GRID_DOWNLOAD_BYTES + 1);
        }
        if !url.ends_with("/terrio_concurrent_grid.tif") {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        let (head, tail) = grid.split_at(grid.len() / 2);
        let mut file = std::fs::File::create(path)?;
        std::io::Write::write_all(&mut file, head)?;
        let mut paths = self.paths.lock().unwrap();
        paths.push(path.to_path_buf());
        self.arrived.notify_all();
        let (paths, _) = self.arrived.wait_timeout_while(paths, Duration::from_secs(2), |p| p.len() < THREADS).unwrap();
        drop(paths);
        std::io::Write::write_all(&mut file, tail)
    }
}

fn geoid_transformer(grid: &str) -> Transformer {
    Transformer::new(
        &SpatialRef::parse(&format!("+proj=longlat +datum=WGS84 +geoidgrids={grid} +vunits=m")).unwrap(),
        &SpatialRef::from_epsg(4979).unwrap(),
    )
    .unwrap()
}

/// The names of the files in a directory.
fn files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

#[test]
fn downloaded_grids_enter_the_cache_verified_through_their_own_scratch_files() {
    let cache = tempfile::tempdir().unwrap();
    // SAFETY: the only test of this binary, before it starts any thread of its own.
    unsafe { std::env::set_var("TERRIO_GRID_CACHE", cache.path()) };
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    terrio_crs::set_network_enabled(Some(true));
    let transport = Arc::new(WholeOnly::default());
    network::set_grid_transport(transport.clone());

    let heights: Vec<_> = std::thread::scope(|s| {
        let threads: Vec<_> =
            (0..THREADS).map(|_| s.spawn(|| geoid_transformer("terrio_concurrent_grid.tif").transform_3d(-100.0, 40.0, 100.0))).collect();
        threads.into_iter().map(|t| t.join().unwrap()).collect()
    });
    for h in &heights {
        let (_, _, h) = h.as_ref().unwrap();
        assert!((h - 100.5).abs() < 1e-9, "{heights:?}");
    }
    let paths = transport.paths.lock().unwrap().clone();
    assert_eq!(paths.len(), THREADS, "downloads that overlapped: {paths:?}");
    let mut unique = paths.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), paths.len(), "scratch paths shared: {paths:?}");
    assert!(paths.iter().all(|p| p.parent() == Some(cache.path())), "{paths:?}");

    for grid in ["terrio_not_a_grid.tif", "terrio_oversized_grid.tif"] {
        let _ = geoid_transformer(grid).transform_3d(-100.0, 40.0, 100.0);
    }
    // Only the verified grid is cached, and no scratch file is left behind.
    assert_eq!(files(cache.path()), ["terrio_concurrent_grid.tif"]);
    assert_eq!(std::fs::read(cache.path().join("terrio_concurrent_grid.tif")).unwrap(), geoid_grid());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(cache.path().join("terrio_concurrent_grid.tif")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o644, "{mode:o}");
    }
}

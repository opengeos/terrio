//! Grids named by a path in a CRS definition: read from it by default, as PROJ reads them, but
//! treated as missing once the search paths are set from code; relative names stay inside the
//! search directories, and only regular files are read (never a FIFO or a device). Its own test
//! binary: grid search paths are process-wide, and its tests take turns with them.
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use terrio_crs::{SpatialRef, Transformer};

static CONFIG: Mutex<()> = Mutex::new(());

/// Holds the process-wide grid settings for one test, and restores them to the environment's
/// when the test ends, passing or failing.
struct Settings(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

impl Settings {
    fn take() -> Self {
        let guard = CONFIG.lock().unwrap_or_else(|e| e.into_inner());
        terrio_crs::set_network_enabled(Some(false));
        Settings(guard)
    }
}

impl Drop for Settings {
    fn drop(&mut self) {
        terrio_crs::set_grid_search_paths(None);
        terrio_crs::set_network_enabled(None);
    }
}

/// A 2 x 2 GTX geoid grid from (0°, 0°), 1° apart: 10, 20 (south row), 30, 40 metres.
fn write_tiny_gtx(path: &Path) {
    let mut b = Vec::new();
    for v in [0.0f64, 0.0, 1.0, 1.0] {
        b.extend(v.to_be_bytes());
    }
    for v in [2i32, 2] {
        b.extend(v.to_be_bytes());
    }
    for v in [10.0f32, 20.0, 30.0, 40.0] {
        b.extend(v.to_be_bytes());
    }
    std::fs::write(path, b).unwrap();
}

/// The ellipsoidal height of 100 m above the geoid at (0.25°, 0.5°), with the geoid grid named
/// `grid`: 122.5 m when the grid is read, and `None` when it is missing (a geoid grid named in a
/// PROJ string is required, so the point is refused).
fn ellipsoidal_height(grid: &str) -> Option<f64> {
    let src = SpatialRef::parse(&format!("+proj=longlat +datum=WGS84 +geoidgrids={grid} +vunits=m")).unwrap();
    let t = Transformer::new(&src, &SpatialRef::from_epsg(4979).unwrap()).unwrap();
    t.transform_3d(0.25, 0.5, 100.0).ok().map(|(_, _, h)| h)
}

fn read(h: Option<f64>) -> bool {
    h.is_some_and(|h| (h - 122.5).abs() < 1e-9)
}

fn missing(h: Option<f64>) -> bool {
    h.is_none()
}

#[test]
fn grids_named_by_path_are_read_only_without_search_paths_set_from_code() {
    let _settings = Settings::take();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tiny.gtx");
    write_tiny_gtx(&path);
    let name = path.to_str().unwrap();

    terrio_crs::set_grid_search_paths(None);
    assert!(read(ellipsoidal_height(name)), "an absolute path is read by default");

    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    let _ = terrio_crs::take_warnings();
    assert!(missing(ellipsoidal_height(name)), "an absolute path is not read with search paths set");
    assert!(terrio_crs::take_warnings().iter().any(|w| w.contains("named by a path is not read")), "and the refusal is reported");

    terrio_crs::set_grid_search_paths(Some(vec![dir.path().to_path_buf()]));
    assert!(missing(ellipsoidal_height(name)), "even when it is inside a search directory");
    assert!(read(ellipsoidal_height("tiny.gtx")), "the same grid is found by name");
}

#[test]
fn grid_files_over_the_size_limit_are_not_read() {
    let _settings = Settings::take();
    let dir = tempfile::tempdir().unwrap();
    // A sparse file just over 2 GiB, starting as a valid grid.
    let path = dir.path().join("huge.gtx");
    write_tiny_gtx(&path);
    std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len((2 << 30) + 1).unwrap();
    terrio_crs::set_grid_search_paths(Some(vec![dir.path().to_path_buf()]));
    let _ = terrio_crs::take_warnings();
    assert!(missing(ellipsoidal_height("huge.gtx")));
    assert!(terrio_crs::take_warnings().iter().any(|w| w.contains("larger than")), "and the refusal is reported");
}

#[test]
fn relative_grid_names_stay_inside_the_search_directories() {
    let _settings = Settings::take();
    let dir = tempfile::tempdir().unwrap();
    let search = dir.path().join("search");
    std::fs::create_dir_all(search.join("sub")).unwrap();
    write_tiny_gtx(&dir.path().join("outside.gtx"));
    write_tiny_gtx(&search.join("sub").join("inside.gtx"));

    terrio_crs::set_grid_search_paths(Some(vec![search]));
    assert!(read(ellipsoidal_height("sub/inside.gtx")), "a subdirectory of a search directory");
    assert!(missing(ellipsoidal_height("sub/../../outside.gtx")), "a name leaving the search directory");
    assert!(missing(ellipsoidal_height("../outside.gtx")), "a path relative to the working directory");
}

/// Transforms a point with a special file (a FIFO, a device) as its horizontal grid, failing the
/// test if the transformation blocks or does not return.
#[cfg(unix)]
fn transform_with_special_grid(fifo: &Path) -> bool {
    let wkt = format!(
        "GEOGCS[\"x\",DATUM[\"x\",SPHEROID[\"WGS 84\",6378137,298.257223563]],PRIMEM[\"Greenwich\",0],\
         UNIT[\"degree\",0.0174532925199433],EXTENSION[\"PROJ4\",\"+proj=longlat +ellps=WGS84 +nadgrids={} +no_defs\"]]",
        fifo.display()
    );
    let src = SpatialRef::from_wkt(&wkt);
    let t = Transformer::new(&src, &SpatialRef::from_epsg(4326).unwrap()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(t.transform(5.0, 52.0).is_ok());
    });
    rx.recv_timeout(Duration::from_secs(10)).expect("transforming blocked on the FIFO")
}

#[cfg(unix)]
#[test]
fn fifos_named_as_grids_are_never_opened() {
    let _settings = Settings::take();
    let dir = tempfile::tempdir().unwrap();
    let fifo = dir.path().join("grid.gsb");
    let status = std::process::Command::new("mkfifo").arg(&fifo).status().expect("mkfifo");
    assert!(status.success());

    // With search paths set from code, the path is not looked at.
    terrio_crs::set_grid_search_paths(Some(Vec::new()));
    assert!(!transform_with_special_grid(&fifo));
    // By default, the path is read only when it is a regular file.
    terrio_crs::set_grid_search_paths(None);
    assert!(!transform_with_special_grid(&fifo));
    // Nor inside a search directory.
    terrio_crs::set_grid_search_paths(Some(vec![dir.path().to_path_buf()]));
    let src = SpatialRef::parse("+proj=longlat +ellps=WGS84 +nadgrids=grid.gsb +no_defs").unwrap();
    let t = Transformer::new(&src, &SpatialRef::from_epsg(4326).unwrap()).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(t.transform(5.0, 52.0).is_ok());
    });
    assert!(!rx.recv_timeout(Duration::from_secs(10)).expect("transforming blocked on the FIFO"));
}

#[cfg(unix)]
#[test]
fn devices_named_as_grids_are_never_read() {
    let _settings = Settings::take();
    terrio_crs::set_grid_search_paths(None);
    // /dev/zero would otherwise be read without end.
    assert!(!transform_with_special_grid(Path::new("/dev/zero")));
}

//! The process-wide allow-list (`formats::set_allowed_drivers`) and point clouds. A list once set
//! cannot be lifted, so the test runs itself again in a child process for each list, named by an
//! environment variable. A list for every kind (`None`) or for point clouds alone restricts every
//! point-cloud reader, as `TERRIO_ALLOWED_DRIVERS` does: LAS, LAZ, COPC, PLY, E57 and EPT files,
//! the tiles of an EPT dataset, bytes, caller-fetched COPC files, identification and `info_path`.
//! With `TERRIO_ALLOWED_DRIVERS` set too, in the environment or in a thread's scope, a driver must
//! be in both. Writing an EPT dataset, which reads back the LAS records it encodes, is not refused.

#![cfg(feature = "pointcloud")]

use std::collections::HashMap;
use std::path::Path;
use terrio::formats::{Kind, allowed_drivers, set_allowed_drivers};
use terrio::pointcloud::{Dimension, EptDataType, PointCloud, PointWriteOptions, Values};

/// The environment variables that name a child's scenario and the directory of the datasets.
const CHILD: &str = "TERRIO_TEST_POINT_CLOUD_ALLOW_LIST";
const DATASETS: &str = "TERRIO_TEST_POINT_CLOUD_DATASETS";

/// The number of points read, or the error's message.
type Outcome = Result<usize, String>;

/// Three points.
fn cloud() -> PointCloud {
    let dims = vec![
        Dimension::new("x", Values::F64(vec![0.0, 1.0, 2.0])),
        Dimension::new("y", Values::F64(vec![0.0, 1.0, 0.0])),
        Dimension::new("z", Values::F64(vec![5.0, 6.0, 7.0])),
    ];
    PointCloud::new(dims, Default::default())
}

fn path(dir: &Path, name: &str) -> String {
    dir.join(name).to_str().unwrap().to_string()
}

/// The refusal of a dataset `driver` is about to read.
fn holds(shown: &str, driver: &str, allowed: &str) -> Outcome {
    Err(format!("'{shown}' holds {driver} data, and the {driver} driver is not allowed (allowed: {allowed})"))
}

/// Writes the point clouds the readers are tried on: a file of each format, and EPT datasets with
/// LAZ tiles (`laz/`) and with binary tiles (`bin/`).
fn write_datasets(dir: &Path) -> terrio::Result<()> {
    for name in ["x.las", "x.laz", "x.copc.laz", "x.ply", "x.e57", "laz/ept.json"] {
        terrio::pointcloud::write_points(&cloud(), &path(dir, name), &PointWriteOptions::default())?;
    }
    let binary = PointWriteOptions { ept_data_type: EptDataType::Binary, ..Default::default() };
    terrio::pointcloud::write_points(&cloud(), &path(dir, "bin/ept.json"), &binary)
}

/// `read_points` of the file `name`.
fn read(dir: &Path, name: &str) -> Outcome {
    terrio::pointcloud::read_points(&path(dir, name), &Default::default()).map(|c| c.len()).map_err(|e| e.to_string())
}

/// `open_points_bytes` of the file's bytes, read.
fn read_bytes(dir: &Path, name: &str) -> Outcome {
    let bytes = std::fs::read(dir.join(name)).unwrap();
    terrio::pointcloud::open_points_bytes(bytes, name)
        .and_then(|mut r| r.read(&Default::default()))
        .map(|c| c.len())
        .map_err(|e| e.to_string())
}

/// `open_points_sparse` of the file, fetched whole by the caller, read.
fn read_sparse(dir: &Path, name: &str) -> Outcome {
    let bytes = std::fs::read(dir.join(name)).unwrap();
    let file = terrio::sparse::SparseFile::new(bytes.len() as u64);
    file.insert(0, &bytes);
    terrio::pointcloud::open_points_sparse(file, name)
        .and_then(|mut r| r.read(&Default::default()))
        .map(|c| c.len())
        .map_err(|e| e.to_string())
}

/// The driver `inspection::identify` names for the file, if any.
fn identified(dir: &Path, name: &str) -> Option<String> {
    terrio::inspection::identify(&path(dir, name), false).unwrap().map(|found| found.driver)
}

/// `f` under `TERRIO_ALLOWED_DRIVERS=value` set for the calling thread.
fn in_scope<T>(value: &str, f: impl FnOnce() -> T) -> T {
    terrio::config::push_scope(HashMap::from([("TERRIO_ALLOWED_DRIVERS".to_string(), Some(value.to_string()))]));
    let out = f();
    terrio::config::pop_scope();
    out
}

#[test]
fn process_lists_restrict_every_point_cloud_reader() {
    let Ok(scenario) = std::env::var(CHILD) else {
        // Written here, under no list, for every child to read.
        let dir = tempfile::tempdir().unwrap();
        write_datasets(dir.path()).unwrap();
        let scenarios = [
            ("every kind: GTiff", None),
            ("every kind: EPT", None),
            ("point clouds: LAS", None),
            ("every kind: LAS, PLY; a thread's scope: PLY, LAZ", None),
            ("every kind: LAS, PLY; the environment: PLY, LAZ", Some("PLY,LAZ")),
        ];
        let mut failed = Vec::new();
        for (scenario, environment) in scenarios {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "process_lists_restrict_every_point_cloud_reader"]).env(CHILD, scenario).env(DATASETS, dir.path());
            command.env_remove("TERRIO_ALLOWED_DRIVERS");
            if let Some(value) = environment {
                command.env("TERRIO_ALLOWED_DRIVERS", value);
            }
            let output = command.output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !output.status.success() || !stdout.contains("1 passed") {
                failed.push(format!("{scenario}:\n{stdout}{}", String::from_utf8_lossy(&output.stderr)));
            }
        }
        assert!(failed.is_empty(), "{} of {} scenarios failed:\n{}", failed.len(), scenarios.len(), failed.join("\n"));
        return;
    };
    let datasets = std::env::var(DATASETS).unwrap();
    let d = Path::new(&datasets);
    let p = |name: &str| path(d, name);
    match scenario.as_str() {
        "every kind: GTiff" => {
            set_allowed_drivers(Some(&["GTiff"]), None).unwrap();
            assert_eq!(allowed_drivers(Some(Kind::PointCloud)), Some(vec!["GTIFF".to_string()]));
            // Without the LAS family in the list, a LAS, LAZ or COPC header is not even read.
            for name in ["x.las", "x.laz", "x.copc.laz"] {
                assert_eq!(read(d, name), holds(&p(name), "LAS", "GTIFF"), "{name}");
            }
            assert_eq!(read(d, "x.ply"), holds(&p("x.ply"), "PLY", "GTIFF"));
            assert_eq!(read(d, "x.e57"), holds(&p("x.e57"), "E57", "GTIFF"));
            for name in ["laz/ept.json", "bin/ept.json"] {
                assert_eq!(read(d, name), holds(&p(name), "EPT", "GTIFF"), "{name}");
            }
            for (name, driver) in [("x.las", "LAS"), ("x.laz", "LAS"), ("x.ply", "PLY"), ("x.e57", "E57")] {
                assert_eq!(read_bytes(d, name), holds(name, driver, "GTIFF"), "{name} bytes");
            }
            assert_eq!(read_sparse(d, "x.copc.laz"), holds("x.copc.laz", "LAS", "GTIFF"));
            assert_eq!(identified(d, "x.las"), None);
            let info = terrio::info::info_path(&p("x.las"), &Default::default()).map(|_| ()).map_err(|e| e.to_string());
            assert_eq!(info, Err(format!("'{}': the LAS driver is not allowed (allowed: GTIFF)", p("x.las"))));
            // Writing is not reading: an EPT dataset is written, its LAS records read back as it
            // encodes them, whatever its tiles.
            let later = tempfile::tempdir().unwrap();
            for (name, data_type) in [("laz/ept.json", EptDataType::Laszip), ("bin/ept.json", EptDataType::Binary)] {
                let options = PointWriteOptions { ept_data_type: data_type, ..Default::default() };
                let written = terrio::pointcloud::write_points(&cloud(), &path(later.path(), name), &options);
                assert_eq!(written.map_err(|e| e.to_string()), Ok(()), "{name} written");
            }
        }
        "every kind: EPT" => {
            set_allowed_drivers(Some(&["EPT"]), None).unwrap();
            for name in ["x.las", "x.laz", "x.copc.laz"] {
                assert_eq!(read(d, name), holds(&p(name), "LAS", "EPT"), "{name}");
            }
            assert_eq!(read(d, "x.ply"), holds(&p("x.ply"), "PLY", "EPT"));
            assert_eq!(read(d, "x.e57"), holds(&p("x.e57"), "E57", "EPT"));
            // The dataset opens; its LAZ tiles are LAS data, refused as they are read.
            assert_eq!(read(d, "laz/ept.json"), holds("0-0-0-0.laz", "LAS", "EPT"));
            assert_eq!(read(d, "bin/ept.json"), Ok(3));
            for (name, driver) in [("x.las", "LAS"), ("x.laz", "LAS"), ("x.ply", "PLY"), ("x.e57", "E57")] {
                assert_eq!(read_bytes(d, name), holds(name, driver, "EPT"), "{name} bytes");
            }
            assert_eq!(read_sparse(d, "x.copc.laz"), holds("x.copc.laz", "LAS", "EPT"));
            assert_eq!(identified(d, "x.las"), None);
        }
        "point clouds: LAS" => {
            set_allowed_drivers(Some(&["LAS"]), Some(Kind::PointCloud)).unwrap();
            assert_eq!(allowed_drivers(Some(Kind::PointCloud)), Some(vec!["LAS".to_string()]));
            for kind in [Some(Kind::Raster), Some(Kind::Vector), None] {
                assert_eq!(allowed_drivers(kind), None, "{kind:?}: a point-cloud list leaves the other kinds alone");
            }
            assert_eq!(read(d, "x.las"), Ok(3));
            assert_eq!(read(d, "x.laz"), holds(&p("x.laz"), "LAZ", "LAS"));
            assert_eq!(read(d, "x.copc.laz"), holds(&p("x.copc.laz"), "COPC", "LAS"));
            assert_eq!(read(d, "x.ply"), holds(&p("x.ply"), "PLY", "LAS"));
            assert_eq!(read(d, "x.e57"), holds(&p("x.e57"), "E57", "LAS"));
            assert_eq!(read(d, "bin/ept.json"), holds(&p("bin/ept.json"), "EPT", "LAS"));
            assert_eq!(read_bytes(d, "x.las"), Ok(3));
            assert_eq!(read_bytes(d, "x.laz"), holds("x.laz", "LAZ", "LAS"));
            assert_eq!(read_sparse(d, "x.copc.laz"), holds("x.copc.laz", "COPC", "LAS"));
            assert_eq!(identified(d, "x.las").as_deref(), Some("LAS"));
            assert_eq!(identified(d, "x.ply"), None);
            let geojson = p("x.geojson");
            std::fs::write(&geojson, r#"{"type":"FeatureCollection","features":[]}"#).unwrap();
            assert!(terrio::open_vector(&geojson, &Default::default()).is_ok(), "vectors are not restricted");
        }
        "every kind: LAS, PLY; a thread's scope: PLY, LAZ" => {
            set_allowed_drivers(Some(&["LAS", "PLY"]), None).unwrap();
            assert_eq!(read(d, "x.las"), Ok(3));
            assert_eq!(read(d, "x.ply"), Ok(3));
            assert_eq!(read(d, "x.laz"), holds(&p("x.laz"), "LAZ", "LAS, PLY"));
            in_scope("PLY,LAZ", || {
                assert_eq!(allowed_drivers(Some(Kind::PointCloud)), Some(vec!["PLY".to_string()]));
                assert_eq!(read(d, "x.ply"), Ok(3));
                assert_eq!(read(d, "x.las"), holds(&p("x.las"), "LAS", "PLY"), "the scope leaves LAS out");
                assert_eq!(read(d, "x.laz"), holds(&p("x.laz"), "LAS", "PLY"), "the process list leaves LAZ out");
            });
        }
        "every kind: LAS, PLY; the environment: PLY, LAZ" => {
            set_allowed_drivers(Some(&["LAS", "PLY"]), None).unwrap();
            assert_eq!(allowed_drivers(Some(Kind::PointCloud)), Some(vec!["PLY".to_string()]));
            assert_eq!(read(d, "x.ply"), Ok(3));
            assert_eq!(read(d, "x.las"), holds(&p("x.las"), "LAS", "PLY"), "the environment leaves LAS out");
            assert_eq!(read(d, "x.laz"), holds(&p("x.laz"), "LAS", "PLY"), "the process list leaves LAZ out");
            // Under a list without the LAS family, an EPT dataset is still written.
            let later = tempfile::tempdir().unwrap();
            let written = terrio::pointcloud::write_points(&cloud(), &path(later.path(), "ept.json"), &PointWriteOptions::default());
            assert_eq!(written.map_err(|e| e.to_string()), Ok(()));
        }
        other => panic!("no scenario {other:?}"),
    }
}

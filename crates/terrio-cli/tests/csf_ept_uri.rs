//! Explicit EPT URIs must win over coincident local directory names.
#![cfg(unix)]

#[test]
fn csf_reads_ept_uri_even_when_a_local_ept_prefix_directory_exists() {
    use terrio::pointcloud::{self, Dimension, PointCloud, PointFormat, PointWriteOptions, Values};
    let dir = tempfile::tempdir().unwrap();
    let cloud = PointCloud::new(
        vec![
            Dimension::new("x", Values::F64(vec![0., 1., 2., 3.])),
            Dimension::new("y", Values::F64(vec![0., 1., 0., 1.])),
            Dimension::new("z", Values::F64(vec![0.; 4])),
        ],
        Default::default(),
    );
    pointcloud::write_points(
        &cloud,
        dir.path().join("dataset/ept.json").to_str().unwrap(),
        &PointWriteOptions { format: Some(PointFormat::Ept), ..Default::default() },
    )
    .unwrap();
    std::os::unix::fs::symlink(".", dir.path().join("ept:")).unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_terrio"))
        .args(["filter", "csf", "ept://dataset", "classified.las"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let result = pointcloud::read_points(dir.path().join("classified.las").to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(result.len(), 4);
    assert!(result.dim("classification").is_some());
}

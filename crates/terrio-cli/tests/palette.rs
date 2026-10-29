//! Palette commands must reject destructive destinations before writing.
use std::process::{Command, Output};

fn run(directory: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(directory).output().unwrap()
}

#[test]
fn existing_outputs_and_input_aliases_remain_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(run(root, &["create-raster", "indices.tif", "--size", "2,2", "--burn", "1"]).status.success());
    std::fs::write(root.join("colors.txt"), "0 0 0 0\n1 248 16 32\n").unwrap();
    let attached = run(root, &["attach-palette", "colors.txt", "indices.tif", "indexed.tif"]);
    assert!(attached.status.success(), "{}", String::from_utf8_lossy(&attached.stderr));
    let indexed = root.join("indexed.tif");
    let original = std::fs::read(&indexed).unwrap();
    std::fs::hard_link(&indexed, root.join("alias.tif")).unwrap();
    for args in [
        vec!["palette-to-rgb", "indexed.tif", "indexed.tif", "--overwrite"],
        vec!["palette-to-rgb", "indexed.tif", "alias.tif", "--overwrite"],
        vec!["attach-palette", "indexed.tif", "indices.tif", "alias.tif", "--overwrite"],
        vec!["palette-to-rgb", "indexed.tif", "alias.tif"],
    ] {
        let output = run(root, &args);
        assert!(!output.status.success(), "unexpected success for {args:?}");
        assert_eq!(std::fs::read(&indexed).unwrap(), original);
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&indexed, root.join("symlink.tif")).unwrap();
        assert!(!run(root, &["palette-to-rgb", "indexed.tif", "symlink.tif", "--overwrite"]).status.success());
        assert_eq!(std::fs::read(&indexed).unwrap(), original);
    }
}

#[test]
fn palette_raster_sidecars_cannot_be_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert!(run(root, &["create-raster", "indices.tif", "--size", "2,2", "--burn", "1"]).status.success());
    std::fs::write(root.join("colors.txt"), "0 0 0 0\n1 248 16 32\n").unwrap();
    assert!(run(root, &["attach-palette", "colors.txt", "indices.tif", "palette.tif"]).status.success());
    assert!(run(root, &["create-raster", "palette.tif.msk", "--size", "2,2", "--burn", "255", "-f", "GTiff"]).status.success());
    let original = std::fs::read(root.join("palette.tif.msk")).unwrap();
    let output = run(root, &["attach-palette", "palette.tif", "indices.tif", "palette.tif.msk", "--overwrite", "-f", "GTiff"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("input"));
    assert_eq!(std::fs::read(root.join("palette.tif.msk")).unwrap(), original);
    assert!(run(root, &["palette-to-rgb", "palette.tif", "expanded.png"]).status.success());
    assert!(run(root, &["attach-palette", "colors.txt", "palette.tif", "attached.png"]).status.success());
    for args in [
        vec!["palette-to-rgb", "palette.tif", "expanded.pmtiles"],
        vec!["palette-to-rgb", "palette.tif", "expanded.mbtiles"],
        vec!["attach-palette", "colors.txt", "palette.tif", "attached.pmtiles"],
        vec!["attach-palette", "colors.txt", "palette.tif", "attached.mbtiles"],
    ] {
        let output = run(root, &args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("dataset masks"));
        assert!(!root.join(args.last().unwrap()).exists());
    }
}

#[test]
fn supplied_tables_ignore_color_count_but_generation_still_validates_it() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    assert!(run(root, &["create-raster", "indices.tif", "--size", "2,2", "--burn", "1"]).status.success());
    std::fs::write(root.join("colors.txt"), "0 0 0 0\n1 248 16 32\n").unwrap();
    assert!(run(root, &["attach-palette", "colors.txt", "indices.tif", "indexed.tif"]).status.success());
    assert!(run(root, &["palette-to-rgb", "indexed.tif", "rgb.tif"]).status.success());
    for count in [0usize, 1, 257, 65535, usize::MAX] {
        let value = count.to_string();
        let name = format!("count-{count}.tif");
        let result = run(root, &["rgb-to-palette", "rgb.tif", &name, "--color-map", "colors.txt", "--color-count", &value]);
        assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
        let actual = terrio::open_raster_lazy(root.join(&name).to_str().unwrap()).unwrap().read().unwrap();
        assert_eq!(actual.raster.data.to_f64_vec(), vec![1.; 4]);
        let rejected = format!("rejected-{count}.tif");
        let result = run(root, &["rgb-to-palette", "rgb.tif", &rejected, "--color-count", &value]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("2..256"));
        assert!(!root.join(rejected).exists());
    }
}

//! Index discovery and safe publication through the public commands.
use std::path::Path;
use std::process::{Command, Output};
fn run(args: &[&str], directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(directory).output().unwrap()
}
fn ok(args: &[&str], directory: &Path) {
    let output = run(args, directory);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}
#[test]
fn discovery_shapefile_parts_and_invalid_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    ok(&["create-raster", "source.tif", "--size", "4,4", "--crs", "EPSG:4326", "--bbox", "0,0,4,4"], path);
    ok(&["raster-index", ".", "index.shp", "--filename-filter", "*.tif"], path);
    for extension in ["shp", "shx", "dbf", "prj"] {
        assert!(path.join(format!("index.{extension}")).exists());
    }
    let before = std::fs::read(path.join("index.dbf")).unwrap();
    assert!(!run(&["raster-index", "source.tif", "index.shp", "--overwrite"], path).status.success());
    assert_eq!(std::fs::read(path.join("index.dbf")).unwrap(), before);
    ok(&["raster-index", "source.tif", "catalog.shp.zip"], path);
    let bytes = std::fs::read(path.join("catalog.shp.zip")).unwrap();
    let terrio::Dataset::Vector(dataset) = terrio::memory::open_bytes(bytes, "catalog.shp.zip").unwrap() else { panic!("not vector") };
    assert_eq!(dataset.layers[0].name, "catalog");
    let source = std::fs::read(path.join("source.tif")).unwrap();
    assert!(!run(&["raster-index", "source.tif", "source.tif", "--format", "GPKG", "--overwrite"], path).status.success());
    assert_eq!(std::fs::read(path.join("source.tif")).unwrap(), source);
    ok(&["gti-create", "source.tif", "index.gpkg", "--band-count", "1", "--ot", "Byte"], path);
    ok(&["gti-create", "source.tif", "grid.gpkg", "--resolution", "1,1", "--bbox=-1,-2,4,4"], path);
    for options in [vec!["--resolution", "1"], vec!["--bbox=1,2,3"]] {
        let mut args = vec!["gti-create", "source.tif", "invalid.gpkg"];
        args.extend(options);
        assert!(!run(&args, path).status.success());
        assert!(!path.join("invalid.gpkg").exists());
    }
    let bytes = std::fs::read(path.join("index.gpkg")).unwrap();
    assert_eq!(run(&["raster-index", "source.tif", "index.gpkg", "--overwrite", "--min-pixel-size=-1"], path).status.code(), Some(1));
    assert_eq!(std::fs::read(path.join("index.gpkg")).unwrap(), bytes);
}

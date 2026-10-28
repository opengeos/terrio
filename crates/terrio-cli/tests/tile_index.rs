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

#[test]
fn relative_inputs_work_when_the_catalog_is_in_another_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    std::fs::create_dir(path.join("tiles")).unwrap();
    std::fs::create_dir(path.join("out")).unwrap();
    ok(&["create-raster", "tiles/a.tif", "--size", "4,4", "--burn", "7", "--crs", "EPSG:4326", "--bbox", "0,0,4,4"], path);
    ok(&["raster-index", "tiles/a.tif", "out/catalog.gpkg"], path);
    ok(&["catalog", "out/catalog.gpkg", "rendered.tif"], path);
    let raster = terrio::open_raster(path.join("rendered.tif").to_str().unwrap()).unwrap();
    assert!(raster.raster.data.to_f64_vec().iter().all(|v| *v == 7.));
    let index = terrio::open_vector(path.join("out/catalog.gpkg").to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(index.layers[0].features[0].attributes[0], terrio::FieldValue::Text("../tiles/a.tif".into()));
}

#[test]
fn layer_selection_does_not_allow_replacing_an_unselected_input() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let json =
        br#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;
    std::fs::write(path.join("roads.geojson"), json).unwrap();
    std::fs::write(path.join("counties.geojson"), json).unwrap();
    let output = run(
        &[
            "vector-index",
            "roads.geojson",
            "counties.geojson",
            "counties.geojson",
            "--source-layer-name",
            "roads",
            "--format",
            "GPKG",
            "--overwrite",
        ],
        path,
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("indexed source"));
    assert_eq!(std::fs::read(path.join("counties.geojson")).unwrap(), json);
}

#[test]
fn skipped_unreadable_raster_inputs_cannot_be_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    ok(&["create-raster", "a.tif", "--size", "2,2", "--crs", "EPSG:4326", "--bbox", "0,0,2,2"], path);
    let bytes = b"not,a,raster\nkeep,this,text\n";
    std::fs::write(path.join("data.csv"), bytes).unwrap();
    let output = run(&["raster-index", "a.tif", "data.csv", "data.csv", "--skip-errors", "--format", "GPKG", "--overwrite"], path);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("indexed source"));
    assert_eq!(std::fs::read(path.join("data.csv")).unwrap(), bytes);
}

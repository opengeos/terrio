//! The dataset-level vector commands (`concat`, `partition`, `set-field-type`, `make-point`,
//! `create`, `export-schema`) run as the command line runs them.

use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(dir).output().unwrap()
}

fn ok(args: &[&str], dir: &Path) -> String {
    let result = run(args, dir);
    assert!(result.status.success(), "{args:?}: {}", String::from_utf8_lossy(&result.stderr));
    String::from_utf8_lossy(&result.stdout).into_owned()
}

fn fails(args: &[&str], dir: &Path) -> String {
    let result = run(args, dir);
    assert!(!result.status.success(), "{args:?} should fail");
    String::from_utf8_lossy(&result.stderr).into_owned()
}

fn layers(path: &Path) -> Vec<terrio::Layer> {
    terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap().layers
}

const PLACES: &str = r#"{"type":"FeatureCollection","features":[
{"type":"Feature","properties":{"state":"TN","n":1,"lon":"-86.5","lat":36.1},"geometry":{"type":"Point","coordinates":[0,0]}},
{"type":"Feature","properties":{"state":"NY","n":2,"lon":"-74.0","lat":40.7},"geometry":{"type":"Point","coordinates":[1,1]}},
{"type":"Feature","properties":{"state":"TN","n":3,"lon":"-84.0","lat":35.9},"geometry":{"type":"Point","coordinates":[2,2]}}]}"#;

#[test]
fn concat_partition_and_points() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.geojson"), PLACES).unwrap();
    std::fs::write(d.join("b.geojson"), PLACES).unwrap();

    ok(&["concat", "a.geojson", "b.geojson", "single.gpkg", "--mode", "single", "--source-layer-field-name", "src"], d);
    let merged = layers(&d.join("single.gpkg"));
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].name, "merged");
    assert_eq!(merged[0].len(), 6);
    assert_eq!(merged[0].features[3].attributes[0], terrio::FieldValue::Text("b".into()));
    ok(&["concat", "a.geojson", "b.geojson", "stack.gpkg", "--mode", "stack", "--output-layer", "{DS_INDEX}_{LAYER_NAME}"], d);
    assert_eq!(layers(&d.join("stack.gpkg")).iter().map(|l| l.name.clone()).collect::<Vec<_>>(), ["0_a", "1_b"]);
    assert!(fails(&["concat", "a.geojson", "x.gpkg", "--output-layer", "x"], d).contains("merge-per-layer-name"));

    ok(&["partition", "a.geojson", "parts", "--field", "state", "-f", "GPKG"], d);
    assert_eq!(layers(&d.join("parts/a/state=TN/part_0000000001.gpkg"))[0].len(), 2);
    assert!(fails(&["partition", "a.geojson", "parts", "--field", "state", "-f", "GPKG"], d).contains("already exists"));
    ok(&["partition", "a.geojson", "parts", "--field", "state", "-f", "GPKG", "--overwrite"], d);
    // A directory that is not a partitioned dataset is not removed.
    std::fs::create_dir_all(d.join("keep/sub")).unwrap();
    std::fs::write(d.join("keep/sub/precious.txt"), "x").unwrap();
    assert!(fails(&["partition", "a.geojson", "keep", "--field", "state", "-f", "GPKG", "--overwrite"], d).contains("refusing"));
    assert!(d.join("keep/sub/precious.txt").exists());
    ok(&["partition", "a.geojson", "flat", "--field", "n", "--scheme", "flat", "-f", "GeoJSON", "--omit-partitioned-field"], d);
    assert_eq!(layers(&d.join("flat/a_2_0000000001.json"))[0].schema.field_index("n"), None);

    ok(&["make-point", "a.geojson", "pts.gpkg", "--x", "lon", "--y", "lat", "--dst-crs", "EPSG:4326"], d);
    let pts = layers(&d.join("pts.gpkg"));
    assert_eq!(pts[0].crs_epsg(), Some(4326));
    let Some(terrio::Geometry::Point(c)) = &pts[0].features[1].geometry else { panic!("not a point") };
    assert_eq!((c.x, c.y), (-74.0, 40.7));
    assert!(fails(&["make-point", "a.geojson", "bad.gpkg", "--x", "state", "--y", "lat"], d).contains("invalid value"));
}

#[test]
fn set_field_type_create_and_export_schema() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.geojson"), PLACES).unwrap();
    ok(&["set-field-type", "a.geojson", "typed.gpkg", "--field-name", "lon", "--dst-field-type", "Real"], d);
    assert_eq!(layers(&d.join("typed.gpkg"))[0].features[0].attributes[2], terrio::FieldValue::Float(-86.5));
    ok(&["set-field-type", "a.geojson", "strings.gpkg", "--src-field-type", "Integer", "--field-type", "String"], d);
    assert_eq!(layers(&d.join("strings.gpkg"))[0].features[2].attributes[1], terrio::FieldValue::Text("3".into()));
    assert!(
        fails(&["set-field-type", "a.geojson", "x.gpkg", "--field-name", "nope", "--dst-field-type", "Real"], d)
            .contains("cannot find field")
    );

    ok(
        &[
            "create",
            "new.gpkg",
            "--output-layer",
            "sites",
            "--geometry-type",
            "POINT",
            "--crs",
            "EPSG:32633",
            "--fid",
            "id",
            "--field",
            "name:STRING(20)",
            "--field",
            "total:INTEGER64",
        ],
        d,
    );
    let schema: serde_json::Value = serde_json::from_str(&ok(&["export-schema", "new.gpkg"], d)).unwrap();
    let layer = &schema["layers"][0];
    assert_eq!(layer["name"], "sites");
    assert_eq!(layer["fidColumnName"], "id");
    assert_eq!(layer["geometryFields"][0]["type"], "Point");
    assert_eq!(layer["geometryFields"][0]["coordinateSystem"]["authid"], "EPSG:32633");
    assert_eq!(layer["fields"][0]["width"], 20);
    assert_eq!(layer["fields"][1]["type"], "Integer64");
    assert!(fails(&["create", "new.gpkg", "--output-layer", "x"], d).contains("already exists"));

    // A schema file and a template make the same layers; --update adds layers.
    ok(&["export-schema", "new.gpkg", "-o", "schema.json"], d);
    ok(&["create", "copy.gpkg", "--schema", "schema.json"], d);
    let copy: serde_json::Value = serde_json::from_str(&ok(&["export-schema", "copy.gpkg"], d)).unwrap();
    assert_eq!(copy, schema);
    ok(&["create", "copy.gpkg", "--like", "a.geojson", "--output-layer", "places", "--update"], d);
    assert_eq!(terrio::list_layers(d.join("copy.gpkg").to_str().unwrap()).unwrap(), ["sites", "places"]);
    assert!(fails(&["create", "copy.gpkg", "--like", "a.geojson", "--output-layer", "places", "--update"], d).contains("already has"));
}

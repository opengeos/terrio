//! `terrio info` and `terrio convert` on a HEC-RAS plan HDF file (the HECRAS driver), with open
//! options, on the synthetic plan `crates/terrio/tests/data/hecras/results_mesh_xs.h5`.

use serde_json::Value;
use std::path::PathBuf;
use std::process::Command;
use terrio::FieldValue;

fn plan() -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../terrio/tests/data/hecras/results_mesh_xs.h5").to_string_lossy().into_owned()
}

fn terrio(args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).output().unwrap();
    (out.status.success(), String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned())
}

#[test]
fn info_lists_the_hec_ras_layers() {
    let (ok, out, err) = terrio(&["info", "--json", &plan()]);
    assert!(ok, "{err}");
    let info: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(info["driver"], "HECRAS", "{info}");
    let layers: Vec<_> = info["layers"].as_array().unwrap().iter().map(|l| l["name"].as_str().unwrap().to_string()).collect();
    assert!(layers.contains(&"mesh_cells".to_string()) && layers.contains(&"cross_sections".to_string()), "{layers:?}");
}

#[test]
fn info_and_convert_take_open_options() {
    let (ok, out, err) = terrio(&[
        "info",
        "--json",
        "-l",
        "mesh_cells",
        "--oo",
        "CELL_SUMMARIES=Maximum Water Surface",
        "--oo",
        "CELL_ATTRIBUTES=MINIMUM_ELEVATION",
        &plan(),
    ]);
    assert!(ok, "{err}");
    assert!(out.contains("maximum_water_surface_time") && out.contains("minimum_elevation"), "{out}");

    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("cells.geojson");
    let dst = dst.to_string_lossy();
    let (ok, _, err) = terrio(&["convert", &plan(), &dst, "-l", "mesh_cells", "--oo", "CELL_SERIES=Water Surface", "--oo", "TIME_STEP=1"]);
    assert!(ok, "{err}");
    let back = terrio::open_vector(&dst, &Default::default()).unwrap();
    let layer = &back.layers[0];
    let i = layer.schema.fields().iter().position(|f| f.name == "water_surface").unwrap();
    let values: Vec<_> = layer.features.iter().map(|f| f.attributes[i].clone()).collect();
    assert_eq!(values, [FieldValue::Float(12.75), FieldValue::Float(12.25)]);
}

#[test]
fn bad_open_options_are_errors() {
    for args in [
        vec!["info", "-l", "mesh_cells", "--oo", "CELL_SERIES=Water Surface"],
        vec!["info", "-l", "mesh_cells", "--oo", "FACE_ATTRIBUTES=MANNINGS_N"],
        vec!["info", "-l", "mesh_cells", "--oo", "NO_SUCH_OPTION=1"],
    ] {
        let mut args = args;
        let plan = plan();
        args.push(&plan);
        let (ok, _, err) = terrio(&args);
        assert!(!ok, "{args:?}");
        assert!(!err.is_empty());
    }
}

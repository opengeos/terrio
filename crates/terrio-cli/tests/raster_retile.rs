//! Retile/publication CLI and MCP wiring, with actual encoded files.
use std::process::{Command, Stdio};

fn fixture(path: &std::path::Path, world: bool) {
    let (cols, rows) = if world { (128, 64) } else { (7, 5) };
    let config = terrio::RasterConfig {
        cols,
        rows,
        bands: 1,
        data_type: terrio::DataType::U8,
        cell_size: if world { 2.8125 } else { 1. },
        x_min: if world { -180. } else { 0. },
        y_min: if world { -90. } else { 0. },
        crs: terrio::crs::SpatialRef::from_epsg(4326).unwrap().to_crs_info(),
        ..Default::default()
    };
    let raster = terrio::Raster::from_data_native(config, terrio::raster::RasterData::U8(vec![100; cols * rows])).unwrap();
    let bytes = terrio::memory::write_raster_bytes(&terrio::RasterDataset::new(raster, false), "source.tif", &Default::default()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn retile_cli_plans_without_files_then_writes_and_resumes_the_layout() {
    let directory = tempfile::tempdir().unwrap();
    fixture(&directory.path().join("source.tif"), false);
    let run = |tail: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_terrio"))
            .current_dir(directory.path())
            .args([
                "retile",
                "source.tif",
                "-o",
                "tiles",
                "--tile-size",
                "3",
                "3",
                "--levels",
                "1",
                "--csv",
                "tiles.csv",
                "--tile-index",
                "tiles.shp",
            ])
            .args(tail)
            .output()
            .unwrap()
    };
    let output = run(&["--dry-run"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let plan: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(plan["planned"], 7);
    assert!(!directory.path().join("tiles").exists());
    let output = run(&[]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    for file in ["source_1_1.tif", "1/source_1_1.tif", "tiles.csv", "tiles.shp", "tiles.dbf", "tiles.shx"] {
        assert!(directory.path().join("tiles").join(file).exists(), "{file}");
    }
    let output = run(&["--resume"]);
    assert!(output.status.success());
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["resumed"], 7);
    assert_eq!(report["written"], 0);
    assert!(!run(&[]).status.success());
}

#[test]
fn tile_cli_emits_publications_and_rejects_invalid_dry_run_options() {
    let directory = tempfile::tempdir().unwrap();
    fixture(&directory.path().join("world.tif"), true);
    let run = |tail: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_terrio"))
            .current_dir(directory.path())
            .args([
                "tile",
                "world.tif",
                "tiles",
                "--tiling-scheme",
                "WorldCRS84Quad",
                "--tile-size",
                "64",
                "--min-zoom",
                "0",
                "--max-zoom",
                "0",
            ])
            .args(tail)
            .output()
            .unwrap()
    };
    assert!(!run(&["--dry-run", "--webviewer", "unknown"]).status.success());
    assert!(!directory.path().join("tiles").exists());
    let output = run(&["--webviewer", "all", "--kml", "--title", "Published map"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    for file in ["leaflet.html", "openlayers.html", "mapml.mapml", "stacta.json", "doc.kml", "0/0/0.kml"] {
        assert!(directory.path().join("tiles").join(file).exists(), "{file}");
    }
    let raster = terrio::open_raster(directory.path().join("tiles/stacta.json").to_str().unwrap()).unwrap();
    assert_eq!((raster.raster.cols, raster.raster.rows, raster.raster.bands), (128, 64, 2));
}

#[test]
fn retile_is_exposed_as_a_writing_mcp_tool() {
    use std::io::{BufRead, BufReader, Write};
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    writeln!(child.stdin.take().unwrap(), "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
    let line = BufReader::new(child.stdout.take().unwrap()).lines().next().unwrap().unwrap();
    let response: serde_json::Value = serde_json::from_str(&line).unwrap();
    let tool = response["result"]["tools"].as_array().unwrap().iter().find(|tool| tool["name"] == "retile").unwrap();
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    assert!(tool["description"].as_str().unwrap().contains("--tile-index"));
    child.kill().unwrap();
    child.wait().unwrap();
}

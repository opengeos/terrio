//! `terrio mcp` over standard input and output, as an MCP client runs it.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// Sends `messages` to `terrio mcp`, one per line, and returns its replies by id, waiting for one
/// per request before closing its standard input (which stops the commands still running).
fn session(messages: &[Value]) -> HashMap<u64, Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for m in messages {
        writeln!(stdin, "{m}").unwrap();
    }
    let requests = messages.iter().filter(|m| m.get("id").is_some()).count();
    // Tool calls reply when their command is done, in any order.
    let replies: HashMap<u64, Value> = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .take(requests)
        .map(|l| {
            let v: Value = serde_json::from_str(&l.unwrap()).unwrap();
            (v["id"].as_u64().unwrap(), v)
        })
        .collect();
    drop(stdin);
    assert!(child.wait().unwrap().success());
    replies
}

fn call(id: u32, name: &str, args: &[&str]) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "tools/call", "params": {"name": name, "arguments": {"args": args}}})
}

#[test]
fn serves_the_commands() {
    let replies = session(&[
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        call(1, "formats", &["--json"]),
        call(2, "info", &["/no/such/file.tif"]),
        call(3, "info", &["--no-such-option"]),
    ]);
    assert_eq!(replies.len(), 4);
    assert_eq!(replies[&0]["result"]["protocolVersion"], "2025-06-18");

    let formats = &replies[&1]["result"];
    assert_eq!(formats["isError"], false);
    let list: Value = serde_json::from_str(formats["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(list.as_array().unwrap().iter().any(|f| f["name"] == "GTiff"));

    for (reply, code) in [(&replies[&2], "exit code 1"), (&replies[&3], "exit code 2")] {
        assert_eq!(reply["result"]["isError"], true);
        assert!(reply["result"]["content"][0]["text"].as_str().unwrap().contains(code), "{reply}");
    }
}

#[test]
fn topology_tools_are_discoverable_and_execute_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("point.geojson");
    let output = dir.path().join("buffer.gpkg");
    std::fs::write(&input,r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"site"},"geometry":{"type":"Point","coordinates":[0,0]}}]}"#).unwrap();
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "buffer", &[input.to_str().unwrap(), output.to_str().unwrap(), "--distance", "1"]),
    ]);
    let tools = replies[&1]["result"]["tools"].as_array().unwrap();
    for name in ["buffer", "overlay", "dissolve", "is-valid", "make-valid", "clean"] {
        assert!(tools.iter().any(|t| t["name"] == name), "missing {name}");
    }
    assert_eq!(replies[&2]["result"]["isError"], false);
    assert_eq!(terrio::open_vector(output.to_str().unwrap(), &Default::default()).unwrap().layers[0].len(), 1);
}

#[test]
fn vector_dataset_tools_are_discoverable_and_execute() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("points.geojson");
    let output = dir.path().join("schema.json");
    std::fs::write(&input, r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"site"},"geometry":{"type":"Point","coordinates":[0,0]}}]}"#).unwrap();
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "export-schema", &[input.to_str().unwrap(), "-o", output.to_str().unwrap()]),
    ]);
    let tools = replies[&1]["result"]["tools"].as_array().unwrap();
    for name in ["concat", "partition", "set-field-type", "make-point", "create", "export-schema"] {
        assert!(tools.iter().any(|t| t["name"] == name), "missing {name}");
    }
    assert_eq!(replies[&2]["result"]["isError"], false);
    let schema: Value = serde_json::from_str(&std::fs::read_to_string(&output).unwrap()).unwrap();
    assert_eq!(schema["layers"][0]["fields"][0]["name"], "name");
}

#[test]
fn viewshed_tool_writes_height_raster() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("ridge.tif");
    let output = dir.path().join("ground.tif");
    let cfg = terrio::RasterConfig { cols: 5, rows: 1, cell_size: 10.0, data_type: terrio::DataType::F64, ..Default::default() };
    let mut ds = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, vec![0.0, 0.0, 10.0, 0.0, 0.0]).unwrap(), false);
    ds.raster.crs.epsg = Some(32611);
    ds.set_geo_transform([0.0, 10.0, 0.0, 10.0, 0.0, -10.0]);
    terrio::write_raster_dataset(&ds, input.to_str().unwrap(), &Default::default()).unwrap();
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "viewshed", &[input.to_str().unwrap(), output.to_str().unwrap(), "--ox", "5", "--oy", "5", "--om", "GROUND", "--cc", "0"]),
    ]);
    assert!(replies[&1]["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "viewshed"));
    assert_eq!(replies[&2]["result"]["isError"], false);
    let out = terrio::open_raster(output.to_str().unwrap()).unwrap();
    assert_eq!(out.raster.data_type, terrio::DataType::F64);
    assert_eq!(out.raster.get(0, 0, 3), 14.0);
    assert_eq!(out.raster.get(0, 0, 4), 18.0);
}

#[test]
fn fill_nodata_is_discoverable_and_executes() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("holes.asc");
    let output = dir.path().join("filled.tif");
    std::fs::write(&input, "ncols 3\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\nNODATA_value -9999\n1 -9999 3\n").unwrap();
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "fill-nodata", &[input.to_str().unwrap(), output.to_str().unwrap(), "--interpolation", "nearest"]),
    ]);
    assert!(replies[&1]["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "fill-nodata"));
    assert_eq!(replies[&2]["result"]["isError"], false);
    assert_eq!(terrio::open_raster(output.to_str().unwrap()).unwrap().raster.get(0, 0, 1), 1.0);
}

#[test]
fn pixel_tools_are_discoverable_and_execute() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("classes.asc");
    std::fs::write(&input, "ncols 3\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\nNODATA_value -9999\n1 -9999 30\n").unwrap();
    let mapping = dir.path().join("map.txt");
    std::fs::write(&mapping, "[0,10)=1 # low\n[10,100)=2\nNO_DATA=NO_DATA\n").unwrap();
    let path = |name: &str| dir.path().join(name).to_str().unwrap().to_string();
    let (unscaled, alpha, classes, pixels) = (path("unscaled.tif"), path("alpha.tif"), path("classes.tif"), path("pixels.csv"));
    let input = input.to_str().unwrap();
    let at_mapping = format!("@{}", mapping.to_str().unwrap());
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "unscale", &[input, &unscaled, "--ot", "Float64"]),
        call(3, "nodata-to-alpha", &[input, &alpha]),
        call(4, "reclassify", &[input, &classes, "-m", &at_mapping]),
        call(5, "as-features", &[input, &pixels, "--skip-nodata", "--include-row-col"]),
    ]);
    let tools = replies[&1]["result"]["tools"].as_array().unwrap();
    for name in ["unscale", "nodata-to-alpha", "reclassify", "as-features"] {
        assert!(tools.iter().any(|t| t["name"] == name), "{name}");
    }
    for id in 2..=5 {
        assert_eq!(replies[&id]["result"]["isError"], false, "{:?}", replies[&id]);
    }
    assert_eq!(terrio::open_raster(&unscaled).unwrap().raster.data_type, terrio::DataType::F64);
    let a = terrio::open_raster(&alpha).unwrap();
    assert_eq!((a.raster.bands, a.raster.get(1, 0, 1), a.raster.get(1, 0, 2)), (2, 0.0, 255.0));
    let c = terrio::open_raster(&classes).unwrap();
    assert_eq!((c.raster.get(0, 0, 0), c.raster.get(0, 0, 1), c.raster.get(0, 0, 2)), (1.0, -9999.0, 2.0));
    let l = terrio::open_vector(&pixels, &Default::default()).unwrap();
    assert_eq!(l.layers[0].features.len(), 2);
}

#[test]
fn focal_collar_and_enhance_tools_are_discoverable_and_execute() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("image.asc");
    let output = dir.path().join("max.tif");
    std::fs::write(&input, "ncols 3\nnrows 1\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 5 3\n").unwrap();
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "neighbors", &[input.to_str().unwrap(), output.to_str().unwrap(), "--kernel", "equal", "--method", "max"]),
    ]);
    let tools = replies[&1]["result"]["tools"].as_array().unwrap();
    for name in ["neighbors", "clean-collar", "nearblack", "enhance"] {
        assert!(tools.iter().any(|t| t["name"] == name), "missing {name}");
    }
    assert_eq!(replies[&2]["result"]["isError"], false, "{}", replies[&2]);
    let out = terrio::open_raster(output.to_str().unwrap()).unwrap();
    assert_eq!(out.raster.data.to_f64_vec(), vec![5.0, 5.0, 5.0]);
}

#[test]
fn catalog_is_exposed_and_reads_an_indexed_window() {
    let dir = tempfile::tempdir().unwrap();
    let tile = dir.path().join("tile.asc");
    std::fs::write(&tile, "ncols 2\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2\n3 4\n").unwrap();
    let json = dir.path().join("index.geojson");
    std::fs::write(&json,r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"location":"tile.asc"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[2,0],[2,2],[0,2],[0,0]]]}}]}"#).unwrap();
    let index = dir.path().join("index.gpkg");
    let vector = terrio::open_vector(json.to_str().unwrap(), &Default::default()).unwrap();
    terrio::write_vector(&vector.layers, index.to_str().unwrap(), &Default::default()).unwrap();
    let output = dir.path().join("window.tif");
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "catalog", &[index.to_str().unwrap(), output.to_str().unwrap(), "--window", "0", "0", "1", "1", "--stats"]),
    ]);
    assert!(replies[&1]["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "catalog"));
    assert_eq!(replies[&2]["result"]["isError"], false, "{:?}", replies[&2]);
    let stats: Value = serde_json::from_str(replies[&2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(stats["source_opens"], 1);
    assert_eq!(terrio::open_raster(output.to_str().unwrap()).unwrap().raster.band_view(0).as_slice(), &[1.0]);
}

#[test]
fn warp_serializes_and_reads_vrt_without_materializing_output() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("source.asc");
    let output = dir.path().join("warped.vrt");
    std::fs::write(&input, "ncols 3\nnrows 3\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2 3\n4 5 6\n7 8 9\n").unwrap();
    let replies = session(&[call(1, "warp", &[input.to_str().unwrap(), output.to_str().unwrap(), "--ts", "2", "2"])]);
    assert_eq!(replies[&1]["result"]["isError"], false, "{:?}", replies[&1]);
    assert!(std::fs::read_to_string(&output).unwrap().contains("VRTWarpedDataset"));
    let mut result = terrio::open_raster_lazy(output.to_str().unwrap()).unwrap();
    assert_eq!(result.read().unwrap().raster.data.to_f64_vec(), vec![1., 3., 7., 9.]);
}

#[test]
fn pansharpen_tool_uses_all_spectral_bands() {
    let dir = tempfile::tempdir().unwrap();
    let pan_path = dir.path().join("pan.tif");
    let spectral_path = dir.path().join("spectral.tif");
    let output = dir.path().join("sharp.tif");
    for (path, bands, values) in [(&pan_path, 1, vec![80.0; 4]), (&spectral_path, 2, vec![20.0, 20.0, 20.0, 20.0, 40.0, 40.0, 40.0, 40.0])]
    {
        let cfg = terrio::RasterConfig { cols: 2, rows: 2, bands, cell_size: 1.0, data_type: terrio::DataType::U16, ..Default::default() };
        let mut ds = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, values).unwrap(), false);
        ds.raster.crs.epsg = Some(32611);
        ds.set_geo_transform([0.0, 1.0, 0.0, 2.0, 0.0, -1.0]);
        terrio::write_raster_dataset(&ds, path.to_str().unwrap(), &Default::default()).unwrap();
    }
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "pansharpen", &[pan_path.to_str().unwrap(), spectral_path.to_str().unwrap(), output.to_str().unwrap()]),
    ]);
    assert!(replies[&1]["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "pansharpen"));
    assert_eq!(replies[&2]["result"]["isError"], false);
    let out = terrio::open_raster(output.to_str().unwrap()).unwrap();
    assert_eq!(out.raster.get(0, 0, 0), 53.0);
    assert_eq!(out.raster.get(1, 0, 0), 107.0);
}

#[test]
fn coordinate_operation_warnings_and_controls_reach_mcp() {
    let replies = session(&[
        call(1, "crs", &["EPSG:4610", "--to", "EPSG:4326", "110", "35"]),
        call(2, "crs", &["EPSG:4610", "--to", "EPSG:4326", "110", "35", "--ct-opt", "ALLOW_BALLPARK=NO"]),
    ]);
    assert_eq!(replies[&1]["result"]["isError"], false);
    assert!(replies[&1]["result"]["content"][0]["text"].as_str().unwrap().contains("ballpark"));
    assert_eq!(replies[&2]["result"]["isError"], true);
    assert!(replies[&2]["result"]["content"][0]["text"].as_str().unwrap().contains("ALLOW_BALLPARK"));
}

#[test]
fn geocentric_xyz_is_available_to_mcp() {
    let replies = session(&[call(1, "crs", &["EPSG:4979", "--to", "EPSG:4978", "--dim", "3", "147", "-42", "100"])]);
    assert_eq!(replies[&1]["result"]["isError"], false);
    let text = replies[&1]["result"]["content"][0]["text"].as_str().unwrap();
    let values: Vec<f64> = text.split_whitespace().filter_map(|v| v.parse().ok()).collect();
    assert_eq!(values.len(), 3);
    for (actual, expected) in values.iter().zip([-3981230.6461390946, 2585441.4118733606, -4245670.74916175]) {
        assert!((actual - expected).abs() < 0.001);
    }
}

#[test]
fn projjson_definitions_are_available_through_cli_and_mcp() {
    let reference: Value = serde_json::from_str(include_str!("../../terrio-crs/tests/data/projjson_reference.json")).unwrap();
    let case = reference["cases"].as_array().unwrap().iter().find(|c| c["name"] == "Web Mercator with IDs").unwrap();
    let source = case["src"].to_string();
    let target = case["dst"].to_string();
    let replies = session(&[call(1, "crs", &[&source, "--to", &target, "12", "45"])]);
    let result = &replies[&1]["result"];
    assert_eq!(result["isError"], false, "{result}");
    let coordinates: Vec<f64> =
        result["content"][0]["text"].as_str().unwrap().lines().next().unwrap().split_whitespace().map(|s| s.parse().unwrap()).collect();
    assert_eq!(coordinates.len(), 2);
    for (axis, value) in coordinates.iter().enumerate() {
        assert!((value - case["expected"][1][axis].as_f64().unwrap()).abs() < 0.001);
    }
}

#[test]
fn iau_planetary_coordinates_and_body_rejection_through_mcp() {
    assert_eq!(terrio::crs::SpatialRef::from_iau(49900).unwrap().authority(), Some(("IAU_2015", 49900)));
    let replies = session(&[
        call(1, "crs", &["IAU_2015:49900", "--to", "IAU_2015:49910", "137.4", "-4.6"]),
        call(2, "crs", &["IAU_2015:49902", "--to", "IAU_2015:49912", "10", "45"]),
        call(3, "crs", &["IAU_2015:49900", "--to", "EPSG:4326", "0", "0"]),
        call(4, "crs", &["IAU:2020:49900"]),
        call(5, "crs", &["IAU_2015:49900", "--to", "IAU_2015:49901", "45", "-10", "--ct-opt", "ALLOW_BALLPARK=NO"]),
        call(6, "crs", &["IAU_2015:49901", "--to", "IAU_2015:49902", "45", "-10", "--ct-opt", "ALLOW_BALLPARK=NO"]),
    ]);
    for (id, expected) in [(1, [8144343.439702274, -272663.6086072086]), (2, [592746.9752330622, 2664514.735491729])] {
        let result = &replies[&id]["result"];
        assert_eq!(result["isError"], false, "{result}");
        let coordinates: Vec<f64> =
            result["content"][0]["text"].as_str().unwrap().lines().next().unwrap().split_whitespace().map(|s| s.parse().unwrap()).collect();
        assert_eq!(coordinates.len(), 2);
        for (actual, expected) in coordinates.iter().zip(expected) {
            assert!((actual - expected).abs() < 0.001);
        }
    }
    assert_eq!(replies[&3]["result"]["isError"], true);
    assert!(replies[&3]["result"]["content"][0]["text"].as_str().unwrap().contains("celestial body"));
    assert_eq!(replies[&4]["result"]["isError"], true);
    assert_eq!(replies[&5]["result"]["isError"], true);
    assert_eq!(replies[&6]["result"]["isError"], false);
}

#[test]
fn tilted_perspective_coordinates_and_backside_refusal_through_mcp() {
    let target = "+proj=tpers +lat_0=-42 +lon_0=147 +h=3000000 +tilt=20 +azi=30 +datum=WGS84 +units=m";
    let replies = session(&[
        call(1, "crs", &["OGC:CRS84", "--to", target, "150", "-40"]),
        call(2, "crs", &["OGC:CRS84", "--to", target, "-33", "42"]),
    ]);
    assert_eq!(replies[&1]["result"]["isError"], false);
    let values: Vec<f64> =
        replies[&1]["result"]["content"][0]["text"].as_str().unwrap().split_whitespace().filter_map(|v| v.parse().ok()).collect();
    assert_eq!(values.len(), 2);
    assert!((values[0] - 107928.4911266584).abs() < 0.001);
    assert!((values[1] - 323680.07658763544).abs() < 0.001);
    assert_eq!(replies[&2]["result"]["isError"], true);
}

#[test]
fn vector_geometry_tools_are_discoverable_and_execute() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("line.geojson");
    let output = dir.path().join("simple.geojson");
    std::fs::write(&input, r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"road"},"geometry":{"type":"LineString","coordinates":[[0,0],[5,0.1],[10,0]]}}]}"#).unwrap();
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "simplify", &[input.to_str().unwrap(), output.to_str().unwrap(), "--tolerance", "1"]),
    ]);
    let tools = replies[&1]["result"]["tools"].as_array().unwrap();
    for name in ["simplify", "segmentize", "convex-hull", "concave-hull", "swap-xy", "set-geom-type", "combine", "sort"] {
        assert!(tools.iter().any(|t| t["name"] == name), "missing {name}");
    }
    assert_eq!(replies[&2]["result"]["isError"], false);
    let layer = terrio::open_vector(output.to_str().unwrap(), &Default::default()).unwrap().layers.remove(0);
    assert_eq!(layer.features[0].geometry.as_ref().unwrap().all_coords().len(), 2);
}

#[test]
fn compare_and_pixel_info_report_results_rather_than_failures() {
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, values: Vec<f64>| {
        let cfg = terrio::RasterConfig { cols: 3, rows: 1, cell_size: 10.0, data_type: terrio::DataType::F32, ..Default::default() };
        let mut ds = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, values).unwrap(), false);
        ds.raster.crs.epsg = Some(32611);
        ds.set_geo_transform([0.0, 10.0, 0.0, 10.0, 0.0, -10.0]);
        let path = dir.path().join(name);
        terrio::write_raster_dataset(&ds, path.to_str().unwrap(), &Default::default()).unwrap();
        path.to_str().unwrap().to_string()
    };
    let a = write("a.tif", vec![1.0, 2.0, 3.0]);
    let b = write("b.tif", vec![1.0, 2.0, 4.0]);
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "compare", &[&a, &b, "--skip-binary"]),
        call(3, "pixel-info", &["-f", "text", &a, "2.5", "0.5"]),
        call(4, "compare", &[&a, "missing.tif"]),
    ]);
    let tools = replies[&1]["result"]["tools"].as_array().unwrap();
    for name in ["compare", "pixel-info", "xyz"] {
        assert!(tools.iter().any(|t| t["name"] == name), "{name}");
    }
    assert_eq!(replies[&2]["result"]["isError"], false);
    let text = replies[&2]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("1: pixels differing: 1") && text.contains("exited with status 2"), "{text}");
    assert_eq!(replies[&3]["result"]["isError"], false);
    assert!(replies[&3]["result"]["content"][0]["text"].as_str().unwrap().contains("Value: 3"));
    assert_eq!(replies[&4]["result"]["isError"], true);
}

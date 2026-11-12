//! `terrio mcp` over standard input and output, as an MCP client runs it.

use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// Sends `messages` to `terrio mcp`, one per line, and returns its replies by id, waiting for one
/// per request before closing its standard input (which stops the commands still running).
fn session(messages: &[Value]) -> HashMap<u64, Value> {
    session_with_env(messages, &[])
}

fn session_with_env(messages: &[Value], env: &[(&str, &str)]) -> HashMap<u64, Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio"))
        .arg("mcp")
        .envs(env.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
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
fn sql_tool_executes_grouped_and_spatial_queries() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("places.geojson");
    let grouped = dir.path().join("groups.gpkg");
    let spatial = dir.path().join("centroids.gpkg");
    let duplicates = dir.path().join("duplicates.gpkg");
    std::fs::write(&input, r#"{"type":"FeatureCollection","name":"places","features":[{"type":"Feature","properties":{"v":2},"geometry":{"type":"Point","coordinates":[0,0]}},{"type":"Feature","properties":{"v":2},"geometry":{"type":"Point","coordinates":[1,0]}}]}"#).unwrap();
    let replies = session(&[
        call(1, "sql", &[input.to_str().unwrap(), grouped.to_str().unwrap(), "--query", "SELECT v,COUNT(*) AS n FROM places GROUP BY v"]),
        call(
            2,
            "sql",
            &[input.to_str().unwrap(), spatial.to_str().unwrap(), "--query", "SELECT ST_Centroid(ST_Buffer(geometry,1)) FROM places"],
        ),
        call(3, "sql", &[input.to_str().unwrap(), duplicates.to_str().unwrap(), "--query", "SELECT v,v AS v FROM places"]),
        call(
            4,
            "convert",
            &[input.to_str().unwrap(), duplicates.to_str().unwrap(), "--sql", "SELECT v AS n,COUNT(*) AS n FROM places GROUP BY v"],
        ),
    ]);
    assert_eq!(replies[&1]["result"]["isError"], false);
    assert_eq!(replies[&2]["result"]["isError"], false);
    for id in [3, 4] {
        assert_eq!(replies[&id]["result"]["isError"], true);
        assert!(replies[&id]["result"]["content"][0]["text"].as_str().unwrap().contains("duplicate output field"));
    }
    let groups = terrio::open_vector(grouped.to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(groups.layers[0].features[0].attributes, vec![terrio::FieldValue::Integer(2), terrio::FieldValue::Integer(2)]);
    let points = terrio::open_vector(spatial.to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(points.layers[0].len(), 2);
    assert!(points.layers[0].features.iter().all(|f| matches!(f.geometry, Some(terrio::Geometry::Point(_)))));
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

/// `list-crs`: a read-only tool, printing as `projinfo --list-crs` does, or JSON.
#[test]
fn lists_registry_crs() {
    let replies = session(&[
        json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"}),
        call(2, "list-crs", &["gda94", "albers", "--authority", "EPSG", "--kind", "projected", "--json"]),
        call(3, "list-crs", &["EPSG:3785", "--include-deprecated"]),
        call(4, "list-crs", &["--kind", "geographic"]),
    ]);
    let tools = replies[&1]["result"]["tools"].as_array().unwrap();
    let tool = tools.iter().find(|t| t["name"] == "list-crs").unwrap();
    assert_eq!(tool["annotations"]["readOnlyHint"], true);

    let list: Value = serde_json::from_str(replies[&2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    let albers = list.as_array().unwrap().iter().find(|e| e["code"] == 3577).unwrap();
    assert_eq!(albers["authority"], "EPSG");
    assert_eq!(albers["name"], "GDA94 / Australian Albers");
    assert_eq!(albers["kind"], "projected");
    assert_eq!(albers["bounds"], json!([112.85, -43.7, 153.69, -9.86]));

    let text = replies[&3]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("EPSG:3785 \"Popular Visualisation CRS / Mercator\" [deprecated]"), "{text}");

    assert_eq!(replies[&4]["result"]["isError"], true);
    assert!(replies[&4]["result"]["content"][0]["text"].as_str().unwrap().contains("geographic_2d"));
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
        call(
            3,
            "viewshed",
            &[input.to_str().unwrap(), dir.path().join("accum.tif").to_str().unwrap(), "--om", "ACCUM", "--os", "2", "--cc", "0"],
        ),
    ]);
    assert!(replies[&1]["result"]["tools"].as_array().unwrap().iter().any(|t| t["name"] == "viewshed"));
    assert_eq!(replies[&2]["result"]["isError"], false);
    let out = terrio::open_raster(output.to_str().unwrap()).unwrap();
    assert_eq!(out.raster.data_type, terrio::DataType::F64);
    assert_eq!(out.raster.get(0, 0, 3), 14.0);
    assert_eq!(out.raster.get(0, 0, 4), 18.0);
    assert_eq!(replies[&3]["result"]["isError"], false);
    let accum = terrio::open_raster(dir.path().join("accum.tif").to_str().unwrap()).unwrap();
    assert_eq!(accum.raster.data_type, terrio::DataType::U8);
    assert_eq!((0..5).map(|x| accum.raster.get(0, 0, x)).collect::<Vec<_>>(), [170.0, 170.0, 255.0, 170.0, 170.0]);
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
    let image = path("byte.tif");
    let blended = path("blended.tif");
    let cfg = terrio::RasterConfig { cols: 3, rows: 1, data_type: terrio::DataType::U8, ..Default::default() };
    let raster = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, vec![30., 80., 170.]).unwrap(), false);
    terrio::write_raster_dataset(&raster, &image, &Default::default()).unwrap();
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "unscale", &[input, &unscaled, "--ot", "Float64"]),
        call(3, "nodata-to-alpha", &[input, &alpha]),
        call(4, "reclassify", &[input, &classes, "-m", &at_mapping]),
        call(5, "as-features", &[input, &pixels, "--skip-nodata", "--include-row-col"]),
        call(6, "blend", &[&image, &image, &blended, "--operator", "multiply"]),
    ]);
    let tools = replies[&1]["result"]["tools"].as_array().unwrap();
    for name in ["unscale", "nodata-to-alpha", "reclassify", "as-features", "blend"] {
        assert!(tools.iter().any(|t| t["name"] == name), "{name}");
    }
    for id in 2..=6 {
        assert_eq!(replies[&id]["result"]["isError"], false, "{:?}", replies[&id]);
    }
    assert_eq!(terrio::open_raster(&unscaled).unwrap().raster.data_type, terrio::DataType::F64);
    assert_eq!(terrio::open_raster(&blended).unwrap().raster.data.get_f64(0), 4.);
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

/// The HECRAS driver through the CLI-derived tools: `info` (read-only) and `convert`, with open
/// options, as an MCP client calls them.
#[test]
fn hec_ras_files_through_the_tools() {
    let plan = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../terrio/tests/data/hecras/results_mesh_xs.h5");
    let plan = plan.to_string_lossy();
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("sections.fgb");
    let dst = dst.to_string_lossy();
    let replies = session(&[
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        call(1, "info", &["--json", &plan]),
        call(2, "convert", &[&plan, &dst, "-l", "cross_sections", "--oo", "XS_SUMMARIES=Maximum Water Surface"]),
    ]);
    let info = &replies[&1]["result"];
    assert_eq!(info["isError"], false, "{info}");
    let v: Value = serde_json::from_str(info["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(v["driver"], "HECRAS");
    assert_eq!(replies[&2]["result"]["isError"], false, "{}", replies[&2]);
    let back = terrio::open_vector(&dst, &Default::default()).unwrap();
    assert!(back.layers[0].schema.fields().iter().any(|f| f.name == "maximum_water_surface_flow_at_max_wsel"));
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
fn interrupted_goode_homolosine_coordinates_and_refusal_through_mcp() {
    // rust-proj-capability's seed-igh fixture (PROJ 9.9.0), and a point in the interruption
    // between the northern lobes, which PROJ's inverse refuses.
    let target = "+proj=igh +datum=WGS84 +units=m +no_defs";
    let replies = session(&[
        call(1, "crs", &["OGC:CRS84", "--to", target, "147", "-42"]),
        call(2, "crs", &[target, "--to", "OGC:CRS84", "-4110000", "7840000"]),
    ]);
    assert_eq!(replies[&1]["result"]["isError"], false);
    let values: Vec<f64> =
        replies[&1]["result"]["content"][0]["text"].as_str().unwrap().split_whitespace().filter_map(|v| v.parse().ok()).collect();
    assert_eq!(values.len(), 2);
    assert!((values[0] - 16168031.516279552).abs() < 0.001);
    assert!((values[1] + 4674913.235357148).abs() < 0.001);
    assert_eq!(replies[&2]["result"]["isError"], true);
}

#[test]
fn oblique_mollweide_coordinates_and_refusal_through_mcp() {
    // rust-proj-capability's seed-ob_tran-moll fixture (PROJ 9.9.0), and a point beyond the
    // rotated Mollweide's ellipse, which PROJ's inverse refuses.
    let target = "+proj=ob_tran +o_proj=moll +o_lon_p=0 +o_lat_p=45 +lon_0=147 +datum=WGS84 +units=m +no_defs";
    let replies = session(&[
        call(1, "crs", &["OGC:CRS84", "--to", target, "147", "-42"]),
        call(2, "crs", &[target, "--to", "OGC:CRS84", "40000000", "0"]),
    ]);
    assert_eq!(replies[&1]["result"]["isError"], false);
    let values: Vec<f64> =
        replies[&1]["result"]["content"][0]["text"].as_str().unwrap().split_whitespace().filter_map(|v| v.parse().ok()).collect();
    assert_eq!(values.len(), 2);
    assert!(values[0].abs() < 0.001);
    assert!((values[1] + 8921410.650523769).abs() < 0.001);
    assert_eq!(replies[&2]["result"]["isError"], true);
}

#[test]
fn quadrilateralized_spherical_cube_coordinates_through_mcp() {
    // rust-proj-capability's method-qsc fixture (PROJ 9.9.0), forward and back.
    let target = "+proj=qsc +lat_0=0 +lon_0=0 +datum=WGS84 +units=m +no_defs";
    let replies = session(&[
        call(1, "crs", &["OGC:CRS84", "--to", target, "10", "10"]),
        call(2, "crs", &[target, "--to", "OGC:CRS84", "1689641.80879759", "1701418.7180670935"]),
    ]);
    let values = |id: u64| -> Vec<f64> {
        assert_eq!(replies[&id]["result"]["isError"], false);
        replies[&id]["result"]["content"][0]["text"].as_str().unwrap().split_whitespace().filter_map(|v| v.parse().ok()).collect()
    };
    let forward = values(1);
    assert_eq!(forward.len(), 2);
    assert!((forward[0] - 1689641.80879759).abs() < 0.001);
    assert!((forward[1] - 1701418.7180670935).abs() < 0.001);
    let inverse = values(2);
    assert_eq!(inverse.len(), 2);
    assert!((inverse[0] - 10.0).abs() < 1e-8 && (inverse[1] - 10.0).abs() < 1e-8);
}

#[test]
fn healpix_and_rhealpix_coordinates_through_mcp() {
    // rust-proj-capability's method-healpix and method-rhealpix fixtures (PROJ 9.9.0), forward
    // and back.
    for (target, x, y) in [
        ("+proj=healpix +datum=WGS84 +units=m +no_defs", 16344055.09510788, -5009843.133575987),
        ("+proj=rhealpix +datum=WGS84 +units=m +no_defs", -20009043.56085063, -8674831.599318739),
    ] {
        let (xs, ys) = (x.to_string(), y.to_string());
        let replies = session(&[
            call(1, "crs", &["OGC:CRS84", "--to", target, "147", "-42"]),
            call(2, "crs", &[target, "--to", "OGC:CRS84", &xs, &ys]),
        ]);
        let values = |id: u64| -> Vec<f64> {
            assert_eq!(replies[&id]["result"]["isError"], false);
            replies[&id]["result"]["content"][0]["text"].as_str().unwrap().split_whitespace().filter_map(|v| v.parse().ok()).collect()
        };
        let forward = values(1);
        assert_eq!(forward.len(), 2);
        assert!((forward[0] - x).abs() < 0.001 && (forward[1] - y).abs() < 0.001, "{target} {forward:?}");
        let inverse = values(2);
        assert_eq!(inverse.len(), 2);
        assert!((inverse[0] - 147.0).abs() < 1e-8 && (inverse[1] + 42.0).abs() < 1e-8, "{target} {inverse:?}");
    }
}

#[test]
fn isea_coordinates_through_mcp() {
    // PROJ 9.8.1 on the authalic sphere of WGS 84, forward and back.
    let (source, target) = ("+proj=longlat +R=6371007.1809184747", "+proj=isea +R=6371007.1809184747 +units=m");
    let (x, y) = (11127619.110975388, -6172668.500855954);
    let (xs, ys) = (x.to_string(), y.to_string());
    let replies = session(&[call(1, "crs", &[source, "--to", target, "147", "-42"]), call(2, "crs", &[target, "--to", source, &xs, &ys])]);
    let values = |id: u64| -> Vec<f64> {
        assert_eq!(replies[&id]["result"]["isError"], false);
        replies[&id]["result"]["content"][0]["text"].as_str().unwrap().split_whitespace().filter_map(|v| v.parse().ok()).collect()
    };
    let forward = values(1);
    assert_eq!(forward.len(), 2);
    assert!((forward[0] - x).abs() < 0.001 && (forward[1] - y).abs() < 0.001, "{forward:?}");
    let inverse = values(2);
    assert_eq!(inverse.len(), 2);
    assert!((inverse[0] - 147.0).abs() < 1e-8 && (inverse[1] + 42.0).abs() < 1e-8, "{inverse:?}");
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
fn vsi_tool_lists_copies_and_refuses_dangerous_deletes() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir_all(d.join("t/a")).unwrap();
    std::fs::write(d.join("t/a/y.txt"), "abc").unwrap();
    let t = d.join("t");
    let out = d.join("out");
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "vsi", &["list", "-R", "-l", "--json", t.to_str().unwrap()]),
        call(3, "vsi", &["copy", "-r", t.to_str().unwrap(), out.to_str().unwrap(), "--json"]),
        call(4, "vsi", &["delete", "-r", "/"]),
    ]);
    let tools = replies[&1]["result"]["tools"].as_array().unwrap();
    let vsi = tools.iter().find(|t| t["name"] == "vsi").expect("a vsi tool");
    let description = vsi["description"].as_str().unwrap();
    assert!(description.contains("`vsi sync`") && description.contains("--strategy"), "{description}");
    assert_eq!(vsi["annotations"]["destructiveHint"], true);
    let listing: Value = serde_json::from_str(replies[&2]["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(listing[1]["name"], "a/y.txt");
    assert_eq!(listing[1]["size"], 3);
    assert_eq!(replies[&3]["result"]["isError"], false);
    assert_eq!(std::fs::read_to_string(out.join("a/y.txt")).unwrap(), "abc");
    assert_eq!(replies[&4]["result"]["isError"], true);
    assert!(replies[&4]["result"]["content"][0]["text"].as_str().unwrap().contains("refusing"));
}

#[test]
fn vector_grid_and_lineref_tools_execute_the_cli() {
    let dir = tempfile::tempdir().unwrap();
    let points = dir.path().join("points.geojson");
    std::fs::write(
        &points,
        r#"{"type":"FeatureCollection","features":[
        {"type":"Feature","properties":{"v":1},"geometry":{"type":"Point","coordinates":[0.5,0.5]}},
        {"type":"Feature","properties":{"v":2},"geometry":{"type":"Point","coordinates":[1.5,0.5]}},
        {"type":"Feature","properties":{"v":3},"geometry":{"type":"Point","coordinates":[0.5,1.5]}},
        {"type":"Feature","properties":{"v":4},"geometry":{"type":"Point","coordinates":[1.5,1.5]}}]}"#,
    )
    .unwrap();
    let raster = dir.path().join("grid.tif");
    let line = dir.path().join("route.geojson");
    std::fs::write(&line, r#"{"type":"FeatureCollection","crs":{"type":"name","properties":{"name":"EPSG:3857"}},"features":[{"type":"Feature","properties":{},"geometry":{"type":"LineString","coordinates":[[0,0],[1000,0],[1000,1000]]}}]}"#).unwrap();
    let stones = dir.path().join("stones.geojson");
    std::fs::write(
        &stones,
        r#"{"type":"FeatureCollection","crs":{"type":"name","properties":{"name":"EPSG:3857"}},"features":[
        {"type":"Feature","properties":{"pos":1200},"geometry":{"type":"Point","coordinates":[200,0]}},
        {"type":"Feature","properties":{"pos":2000},"geometry":{"type":"Point","coordinates":[1000,200]}}]}"#,
    )
    .unwrap();
    let parts = dir.path().join("parts.gpkg");
    let p = |p: &std::path::Path| p.to_str().unwrap().to_string();
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "vector-grid", &["nearest", &p(&points), &p(&raster), "--zfield", "v", "--extent", "0", "0", "2", "2", "--size", "2", "2"]),
        call(
            3,
            "lineref",
            &["create", "--lines", &p(&line), "--points", &p(&stones), "--position-field", "pos", "--step", "500", &p(&parts)],
        ),
    ]);
    let tools = replies[&1]["result"]["tools"].as_array().unwrap();
    for name in ["vector-grid", "lineref"] {
        assert!(tools.iter().any(|t| t["name"] == name), "missing {name}");
    }
    assert_eq!(replies[&2]["result"]["isError"], false, "{}", replies[&2]);
    let out = terrio::open_raster(raster.to_str().unwrap()).unwrap();
    assert_eq!(out.raster.get(0, 0, 0), 3.0);
    assert_eq!(out.raster.get(0, 1, 1), 2.0);
    assert_eq!(replies[&3]["result"]["isError"], false, "{}", replies[&3]);
    assert!(!replies[&3].to_string().contains("could not be measured in UTM"));
    let replies = session(&[call(1, "lineref", &["get-pos", "--parts", &p(&parts), "-x", "100", "-y", "0"])]);
    let text = replies[&1]["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("1086.956522"), "{text}");
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

#[test]
fn file_lists_are_refused_without_reading_the_file() {
    let tmp = tempfile::tempdir().unwrap();
    // A list naming a path that does not exist: its contents would appear in the open error.
    let list = tmp.path().join("secret.txt");
    std::fs::write(&list, "line-that-must-not-leak\n").unwrap();
    let list = format!("@{}", list.display());
    let out = tmp.path().join("out.tif");
    let replies = session(&[
        json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        call(1, "mosaic", &[&list, out.to_str().unwrap()]),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ]);
    // The tools whose help offers `@FILE` say that tool calls refuse it; the others do not mention it.
    let tools = replies[&2]["result"]["tools"].as_array().unwrap();
    let description = |name: &str| tools.iter().find(|t| t["name"] == name).unwrap()["description"].as_str().unwrap().to_string();
    // `filter merge` is reached through the `filter` tool, whose description lists subcommands.
    for name in ["mosaic", "warp", "concat", "filter", "raster-index", "vector-index", "gti-create"] {
        let d = description(name);
        assert!(d.ends_with("Over MCP, `@FILE` lists are refused: pass each input as an item of `args`."), "{name}: {d}");
    }
    assert!(!description("info").contains("@FILE"));
    // Any tool whose description mentions `@FILE` must carry the note, including one added later.
    for t in tools {
        let d = t["description"].as_str().unwrap();
        if d.contains("`@FILE`") {
            assert!(d.ends_with("Over MCP, `@FILE` lists are refused: pass each input as an item of `args`."), "{}: {d}", t["name"]);
        }
    }
    let result = &replies[&1]["result"];
    assert_eq!(result["isError"], true);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("@FILE lists are not read in MCP tool calls"), "{text}");
    assert!(!text.contains("line-that-must-not-leak"), "{text}");
}

#[test]
fn index_tools_are_discoverable_and_execute() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("roads.geojson");
    let output = dir.path().join("index.gpkg");
    std::fs::write(&input, r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"LineString","coordinates":[[0,0],[2,3]]}}]}"#).unwrap();
    let replies = session(&[
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        call(2, "vector-index", &[input.to_str().unwrap(), output.to_str().unwrap()]),
    ]);
    let tools = replies[&1]["result"]["tools"].as_array().unwrap();
    for name in ["raster-index", "vector-index", "gti-create"] {
        assert!(tools.iter().any(|t| t["name"] == name), "missing {name}");
    }
    assert_eq!(replies[&2]["result"]["isError"], false);
    assert_eq!(terrio::open_vector(output.to_str().unwrap(), &Default::default()).unwrap().layers[0].len(), 1);
}

#[test]
fn gpkg_repack_is_a_mutating_tool_and_preserves_vector_data() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("sites.geojson");
    let target = dir.path().join("sites.gpkg");
    let geojson = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"kept"},"geometry":{"type":"Point","coordinates":[12,45]}}]}"#;
    std::fs::write(&source, geojson).unwrap();
    let input = terrio::open_vector(source.to_str().unwrap(), &Default::default()).unwrap();
    terrio::write_vector(&input.layers, target.to_str().unwrap(), &Default::default()).unwrap();
    let expected = terrio::open_vector(target.to_str().unwrap(), &Default::default()).unwrap();
    let replies =
        session(&[json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}), call(2, "gpkg-repack", &[target.to_str().unwrap(), "--json"])]);
    let tool = replies[&1]["result"]["tools"].as_array().unwrap().iter().find(|tool| tool["name"] == "gpkg-repack").unwrap();
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    assert_eq!(replies[&2]["result"]["isError"], false);
    let output = terrio::open_vector(target.to_str().unwrap(), &Default::default()).unwrap();
    let encode = |layers: &[terrio::Layer]| terrio::memory::write_vector_bytes(layers, "sites.geojson", &Default::default()).unwrap();
    assert_eq!(encode(&expected.layers), encode(&output.layers));
}

#[test]
fn two_point_oblique_mercator_coordinates_and_refusal_through_mcp() {
    // rust-proj-capability's method-omerc-2pt fixture (PROJ 9.9.0), and PROJ's refusal of equal latitudes.
    let target = "+proj=omerc +lat_1=-40 +lon_1=140 +lat_2=-44 +lon_2=150 +k=1 +datum=WGS84 +units=m +no_defs";
    let equal = "+proj=omerc +lat_1=-40 +lon_1=140 +lat_2=-40 +lon_2=150 +datum=WGS84";
    let replies = session(&[
        call(1, "crs", &["OGC:CRS84", "--to", target, "147", "-42"]),
        call(2, "crs", &["OGC:CRS84", "--to", equal, "147", "-42"]),
    ]);
    assert_eq!(replies[&1]["result"]["isError"], false);
    let values: Vec<f64> =
        replies[&1]["result"]["content"][0]["text"].as_str().unwrap().split_whitespace().filter_map(|v| v.parse().ok()).collect();
    assert_eq!(values.len(), 2);
    assert!((values[0] - 4687059.434028593).abs() < 0.001);
    assert!((values[1] - -5279802.3632845115).abs() < 0.001);
    assert_eq!(replies[&2]["result"]["isError"], true);
}

#[cfg(not(target_family = "wasm"))]
#[path = "../../terrio/tests/common/oapif.rs"]
mod oapif_fixture;

#[cfg(not(target_family = "wasm"))]
#[test]
fn oapif_conversion_through_mcp_bounds_plain_limits_and_keeps_filter_order() {
    if !terrio::formats::VectorDriver::OAPIF.info().read {
        return;
    }
    let server = oapif_fixture::Server::new();
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("limited.gpkg");
    let filtered = dir.path().join("filtered.gpkg");
    let url = format!("OAPIF:{}/alias", server.url);
    let replies = session(&[
        call(1, "convert", &[&url, plain.to_str().unwrap(), "--oo", "PAGE_SIZE=2", "--limit", "3"]),
        call(2, "convert", &[&url, filtered.to_str().unwrap(), "--oo", "PAGE_SIZE=2", "--where", "value > 4", "--limit", "1"]),
    ]);
    for id in [1, 2] {
        assert_eq!(replies[&id]["result"]["isError"], false, "{}", replies[&id]);
    }
    let a = terrio::open_vector(plain.to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(a.layers[0].len(), 3);
    for (i, expected) in [0.5, 1.5, 2.5].into_iter().enumerate() {
        assert_eq!(a.layers[0].features[i].get(&a.layers[0].schema, "value").unwrap(), &terrio::FieldValue::Float(expected));
    }
    let b = terrio::open_vector(filtered.to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(b.layers[0].len(), 1);
    assert_eq!(b.layers[0].features[0].get(&b.layers[0].schema, "value").unwrap(), &terrio::FieldValue::Float(4.5));
    // Plain limit needs two item pages; filtering must inspect all three pages.
    assert_eq!(server.requests.lock().unwrap().iter().filter(|(u, _)| u.contains("/items")).count(), 5);
}

#[test]
fn oapif_open_options_are_rejected_on_other_vector_sources() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("in.geojson");
    let output = dir.path().join("out.gpkg");
    std::fs::write(&input, r#"{"type":"FeatureCollection","features":[]}"#).unwrap();
    for option in ["CRS=EPSG:3857", "PAGE_SIZE=1000", "OAPIF_MAX_FEATURES=0", "OAPIF_SKIP_FEATURES=0"] {
        let replies = session(&[call(1, "convert", &[input.to_str().unwrap(), output.to_str().unwrap(), "--oo", option])]);
        assert_eq!(replies[&1]["result"]["isError"], true, "{}", replies[&1]);
        assert!(replies[&1]["result"]["content"][0]["text"].as_str().unwrap().contains("OAPIF"));
        assert!(!output.exists());
    }
}

#[test]
fn zonal_tool_computes_fractional_weighted_statistics() {
    let dir = tempfile::tempdir().unwrap();
    let zones = dir.path().join("zones.geojson");
    let input = dir.path().join("values.tif");
    let weights = dir.path().join("weights.tif");
    let output = dir.path().join("statistics.geojson");
    std::fs::write(&zones, r#"{"type":"Polygon","coordinates":[[[0.5,0],[2,0],[2,1],[0.5,1],[0.5,0]]]}"#).unwrap();
    for (path, data) in [(&input, vec![10., 30.]), (&weights, vec![2., 4.])] {
        let cfg = terrio::RasterConfig { cols: 2, rows: 1, cell_size: 1., data_type: terrio::DataType::F64, ..Default::default() };
        let mut ds = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, data).unwrap(), false);
        ds.raster.crs.epsg = Some(4326);
        ds.set_geo_transform([0., 1., 0., 1., 0., -1.]);
        terrio::write_raster_dataset(&ds, path.to_str().unwrap(), &Default::default()).unwrap();
    }
    let replies = session(&[call(
        1,
        "zonal-stats",
        &[
            zones.to_str().unwrap(),
            output.to_str().unwrap(),
            "--raster",
            input.to_str().unwrap(),
            "--weights",
            weights.to_str().unwrap(),
            "--fractional",
            "--prefix",
            "v",
            "--stats",
            "count,weighted_mean,weighted_sum",
        ],
    )]);
    assert_eq!(replies[&1]["result"]["isError"], false, "{}", replies[&1]);
    let value: Value = serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    assert_eq!(value["features"][0]["properties"]["v_b1_count"], 1.5);
    assert_eq!(value["features"][0]["properties"]["v_b1_weighted_mean"], 26.);
    assert_eq!(value["features"][0]["properties"]["v_b1_weighted_sum"], 130.);
    let unused = dir.path().join("unweighted.geojson");
    let missing = dir.path().join("missing.tif");
    let replies = session(&[call(
        1,
        "zonal-stats",
        &[
            zones.to_str().unwrap(),
            unused.to_str().unwrap(),
            "--raster",
            input.to_str().unwrap(),
            "--weights",
            missing.to_str().unwrap(),
            "--stats",
            "count",
        ],
    )]);
    assert_eq!(replies[&1]["result"]["isError"], false, "{}", replies[&1]);
}

#[test]
fn preview_returns_the_image_it_draws() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("grid.asc");
    std::fs::write(&input, "ncols 2\nnrows 2\nxllcorner 0\nyllcorner 0\ncellsize 1\n1 2\n3 4\n").unwrap();
    let named = dir.path().join("named.png");
    let replies = session(&[
        call(1, "preview", &[input.to_str().unwrap()]),
        call(2, "preview", &[input.to_str().unwrap(), "-o", named.to_str().unwrap()]),
        call(3, "preview", &[input.to_str().unwrap(), "-o", "-"]),
    ]);
    for id in [1, 2] {
        let result = &replies[&id]["result"];
        assert_eq!(result["isError"], false, "{result}");
        let image = result["content"].as_array().unwrap().iter().find(|c| c["type"] == "image").expect("an image");
        assert_eq!(image["mimeType"], "image/png");
        assert!(image["data"].as_str().unwrap().starts_with("iVBORw0KGgo"));
    }
    assert!(named.exists());
    assert_eq!(replies[&3]["result"]["isError"], true);
}

/// A failed redirected write must fail the command, rather than report truncated output as success.
#[cfg(target_os = "linux")]
#[test]
fn crs_listing_propagates_output_errors() {
    for json in [false, true] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_terrio"));
        command.args(["list-crs", "3577"]);
        if json {
            command.arg("--json");
        }
        let full = std::fs::OpenOptions::new().write(true).open("/dev/full").unwrap();
        let output = command.stdout(full).output().unwrap();
        assert!(!output.status.success(), "json={json}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("space"), "{output:?}");
    }
}

#[test]
fn crs_tools_confine_grids_to_host_search_directories() {
    let dir = tempfile::tempdir().unwrap();
    let search = dir.path().join("search");
    std::fs::create_dir(&search).unwrap();
    // A 2x2 GTX: the geoid height at (0.25, 0.5) is exactly 22.5 m.
    let mut bytes = Vec::new();
    for value in [0.0f64, 0., 1., 1.] {
        bytes.extend(value.to_be_bytes());
    }
    for value in [2i32, 2] {
        bytes.extend(value.to_be_bytes());
    }
    for value in [10.0f32, 20., 30., 40.] {
        bytes.extend(value.to_be_bytes());
    }
    let outside = dir.path().join("outside.gtx");
    std::fs::write(&outside, &bytes).unwrap();
    std::fs::write(search.join("inside.gtx"), &bytes).unwrap();
    let definition = |grid: &str| format!("+proj=longlat +datum=WGS84 +geoidgrids={grid} +vunits=m");
    let absolute = definition(outside.to_str().unwrap());
    let named = definition("inside.gtx");
    let coordinates = ["--to", "EPSG:4979", "--dim", "3", "0.25", "0.5", "100"];
    let args = |crs: &str| {
        let mut args = vec![crs.to_string()];
        args.extend(coordinates.iter().map(|s| s.to_string()));
        args
    };
    let direct = Command::new(env!("CARGO_BIN_EXE_terrio"))
        .arg("crs")
        .args(args(&absolute))
        .env_remove("TERRIO_MCP_TOOL")
        .env("TERRIO_NETWORK", "OFF")
        .output()
        .unwrap();
    assert!(direct.status.success(), "{}", String::from_utf8_lossy(&direct.stderr));
    assert!(String::from_utf8_lossy(&direct.stdout).contains("122.5"));
    let absolute_args = args(&absolute);
    let named_args = args(&named);
    let replies = session_with_env(
        &[
            call(1, "crs", &absolute_args.iter().map(String::as_str).collect::<Vec<_>>()),
            call(2, "crs", &named_args.iter().map(String::as_str).collect::<Vec<_>>()),
        ],
        &[("TERRIO_GRID_DIR", search.to_str().unwrap()), ("TERRIO_NETWORK", "OFF")],
    );
    assert_eq!(replies[&1]["result"]["isError"], true, "MCP read an arbitrary path-named grid");
    assert!(replies[&1]["result"]["content"][0]["text"].as_str().unwrap().contains("cannot transform"));
    assert_eq!(replies[&2]["result"]["isError"], false);
    assert!(replies[&2]["result"]["content"][0]["text"].as_str().unwrap().contains("122.5"));
}

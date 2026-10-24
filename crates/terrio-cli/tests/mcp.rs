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

//! SOZip command round trips, failed-write preservation and MCP dispatch.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Output, Stdio};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(["vsi", "sozip"]).args(args).output().unwrap()
}

#[test]
fn commands_round_trip_and_preserve_failed_outputs() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("numbers.csv");
    let archive = dir.path().join("data.zip");
    let optimized = dir.path().join("optimized.zip");
    let bytes: Vec<u8> = (0..20_000).flat_map(|i| format!("{i},{}\n", i * 7919 % 10007).into_bytes()).collect();
    std::fs::write(&source, &bytes).unwrap();
    let (source, archive, optimized) = (source.to_str().unwrap(), archive.to_str().unwrap(), optimized.to_str().unwrap());
    let result = run(&["create", "-j", "--enable-sozip", "yes", "--sozip-chunk-size", "4K", source, archive]);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let result = run(&["list", "--json", archive]);
    assert!(result.status.success());
    let listed: Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(listed[0]["name"], "numbers.csv");
    assert_eq!(listed[0]["sozip_chunk_size"], 4096);
    let result = run(&["validate", "--json", archive]);
    assert!(result.status.success());
    assert_eq!(serde_json::from_slice::<Value>(&result.stdout).unwrap()["valid"], true);
    assert_eq!(terrio::vsi::read(&format!("/vsizip/{archive}/numbers.csv")).unwrap(), bytes);
    let before = std::fs::read(archive).unwrap();
    assert!(!run(&["create", "-j", source, archive]).status.success());
    assert_eq!(std::fs::read(archive).unwrap(), before);
    assert!(run(&["optimize", "--enable-sozip", "yes", "--sozip-chunk-size", "8K", archive, optimized]).status.success());
    let before = std::fs::read(optimized).unwrap();
    assert!(!run(&["optimize", archive, optimized]).status.success());
    assert!(!run(&["optimize", "--overwrite", "--sozip-chunk-size", "0", archive, optimized]).status.success());
    assert_eq!(std::fs::read(optimized).unwrap(), before);

    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    let request = json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"vsi","arguments":{"args":["sozip","validate","--json",optimized]}}});
    writeln!(input, "{request}").unwrap();
    let reply = BufReader::new(child.stdout.take().unwrap()).lines().next().unwrap().unwrap();
    drop(input);
    assert!(child.wait().unwrap().success());
    let reply: Value = serde_json::from_str(&reply).unwrap();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    let report: Value = serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(report["valid"], true);
    assert_eq!(report["sozip_files"], json!(["numbers.csv"]));
}

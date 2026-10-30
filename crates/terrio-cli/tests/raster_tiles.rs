//! Separate-file raster tile command and MCP discovery.
use std::process::Command;
fn run(args: &[&str], path: &std::path::Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(path).output().unwrap()
}
#[test]
fn tiles_plan_generate_and_resume() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();
    let out = run(
        &["create-raster", "input.tif", "--size", "5,3", "--crs", "EPSG:4326", "--bbox", "0,0,5,3", "--burn", "7", "--ot", "Byte"],
        path,
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let opts = ["tile", "input.tif", "tiles", "--tiling-scheme", "raster", "--tile-size", "2", "--resampling", "nearest"];
    let mut dry = opts.to_vec();
    dry.push("--dry-run");
    let out = run(&dry, path);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!path.join("tiles").exists());
    let out = run(&opts, path);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["written"], 6);
    assert!(std::fs::read(path.join("tiles/1/0/0.png")).unwrap().starts_with(b"\x89PNG"));
    assert!(!run(&opts, path).status.success());
    let mut resumed = opts.to_vec();
    resumed.push("--resume");
    let out = run(&resumed, path);
    assert!(out.status.success());
    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["resumed"], 6);
    let mut invalid = opts.to_vec();
    // Use a fresh output path: max_tiles must reject the plan before creating it.
    invalid[2] = "invalid";
    invalid.extend(["--max-tiles", "1"]);
    assert!(!run(&invalid, path).status.success());
    assert!(!path.join("invalid").exists());
}
#[test]
fn tile_is_discoverable_via_mcp() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", serde_json::json!({"jsonrpc":"2.0","id":1,"method":"tools/list"})).unwrap();
    let line = BufReader::new(child.stdout.take().unwrap()).lines().next().unwrap().unwrap();
    let reply: serde_json::Value = serde_json::from_str(&line).unwrap();
    let tool = reply["result"]["tools"].as_array().unwrap().iter().find(|t| t["name"] == "tile").unwrap();
    assert!(tool["description"].as_str().unwrap().contains("--overview-resampling"));
    assert_eq!(tool["annotations"]["readOnlyHint"], false);
    drop(input);
    assert!(child.wait().unwrap().success());
}

//! Public validation verdicts and the read-only MCP contract.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
#[test]
fn cli_and_mcp_return_structured_read_only_verdicts() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("bad.gpkg");
    std::fs::write(&path, b"not sqlite").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_terrio")).args(["gpkg-validate", path.to_str().unwrap(), "--json"]).output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["valid"], false);
    assert_eq!(report["complete"], true);
    assert_eq!(report["errors"][0]["requirement"], 1);
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut input = child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"gpkg-validate","arguments":{"args":[path.to_str().unwrap(),"--json"]}}}),
    ] {
        writeln!(input, "{message}").unwrap();
    }
    let replies = BufReader::new(child.stdout.take().unwrap())
        .lines()
        .take(2)
        .map(|line| serde_json::from_str::<Value>(&line.unwrap()).unwrap())
        .collect::<Vec<_>>();
    drop(input);
    assert!(child.wait().unwrap().success());
    let tools = replies.iter().find(|r| r["id"] == 1).unwrap()["result"]["tools"].as_array().unwrap();
    let tool = tools.iter().find(|t| t["name"] == "gpkg-validate").unwrap();
    assert_eq!(tool["annotations"]["readOnlyHint"], true);
    let result = &replies.iter().find(|r| r["id"] == 2).unwrap()["result"];
    assert_eq!(result["isError"], false);
    let text = result["content"][0]["text"].as_str().unwrap();
    let rpc = serde_json::Deserializer::from_str(text).into_iter::<Value>().next().unwrap().unwrap();
    assert!(text.contains("exited with status 1"));
    assert_eq!(rpc, report);
    assert_eq!(std::fs::read(&path).unwrap(), b"not sqlite");
}

#[cfg(unix)]
#[test]
fn validation_accepts_non_utf8_native_paths() {
    use std::os::unix::ffi::OsStringExt;
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(std::ffi::OsString::from_vec(b"bad-\xff.gpkg".to_vec()));
    // APFS and HFS+ refuse names that are not valid UTF-8 (EILSEQ), so no such file can exist there.
    if let Err(error) = std::fs::write(&path, b"not sqlite") {
        assert!(cfg!(target_os = "macos") && error.raw_os_error() == Some(92), "{error}");
        return;
    }
    let output = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("gpkg-validate").arg(&path).arg("--json").output().unwrap();
    assert_eq!(output.status.code(), Some(1));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["errors"][0]["requirement"], 1);
}

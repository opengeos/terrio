//! `terrio mcp` over standard input and output, as an MCP client runs it.

use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};

/// Sends `messages` to `terrio mcp`, one per line, and returns its replies.
fn session(messages: &[Value]) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    for m in messages {
        writeln!(stdin, "{m}").unwrap();
    }
    drop(stdin);
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
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
    // The notification gets no reply.
    assert_eq!(replies.len(), 4);
    assert_eq!(replies[0]["result"]["protocolVersion"], "2025-06-18");

    let formats = &replies[1]["result"];
    assert_eq!(formats["isError"], false);
    let list: Value = serde_json::from_str(formats["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(list.as_array().unwrap().iter().any(|f| f["name"] == "GTiff"));

    for (reply, code) in [(&replies[2], "exit code 1"), (&replies[3], "exit code 2")] {
        assert_eq!(reply["result"]["isError"], true);
        assert!(reply["result"]["content"][0]["text"].as_str().unwrap().contains(code), "{reply}");
    }
}

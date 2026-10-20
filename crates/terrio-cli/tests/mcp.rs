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

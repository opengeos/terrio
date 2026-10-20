//! `terrio mcp`: a Model Context Protocol server, so that an AI agent (Claude Code, Claude Desktop,
//! Cursor, VS Code…) can run terrio's commands. Each command is a tool whose arguments are its
//! command line, and whose description is its `--help`.
//!
//! Sans I/O: [`Server::handle`] takes one JSON-RPC message and gives back the reply, so the stdio
//! loop ([`serve`]) and the tests drive the same code. A tool call runs the command in a child
//! process of terrio itself: the command's output is captured rather than written over the
//! protocol on standard output, and a crash or an out-of-memory stays the child's.

use clap::CommandFactory;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{BufRead, Write};
use std::process::{Command, Stdio};

/// The protocol versions this server speaks, newest first.
const VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// The most bytes of a command's output returned to the agent; the rest is cut.
const MAX_OUTPUT: usize = 100_000;

/// What the model is told about the server when it connects.
const INSTRUCTIONS: &str = "terrio reads, writes and converts geospatial rasters, vector datasets and point clouds in pure Rust, with GDAL-style commands.\n\
- Each tool runs one `terrio` command; `args` is its command line after the command name, one argument per item, e.g. info with [\"dem.tif\", \"--json\"].\n\
- The tool's description is the command's --help: its arguments and options.\n\
- Inputs can be local paths, URLs, cloud objects (s3://, gs://, az://) or zip members; outputs are local files. Use absolute paths.\n\
- Start with info (--json) to see what a dataset holds; formats lists the formats terrio reads and writes.";

/// Commands that only read: they write no file.
const READ_ONLY: &[&str] = &["info", "formats", "crs"];

type RpcError = (i64, String);

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// The server.
pub struct Server {
    /// The command line that runs terrio: the binary, or a Python interpreter with `-m terrio`.
    terrio: Vec<OsString>,
}

impl Server {
    /// A server whose tools run `terrio` (the program and any arguments before the command).
    pub fn new(terrio: Vec<OsString>) -> Self {
        Self { terrio }
    }

    /// Handles one message (a line of JSON): the reply to send, or `None` for a notification or
    /// a response.
    pub fn handle(&self, message: &str) -> Option<String> {
        let msg: Value = match serde_json::from_str(message) {
            Ok(v) => v,
            Err(e) => return Some(error(&Value::Null, PARSE_ERROR, &format!("not JSON: {e}")).to_string()),
        };
        let reply = match &msg {
            // A batch (protocol 2025-03-26): one reply per request, none when all are notifications.
            Value::Array(batch) if batch.is_empty() => error(&Value::Null, INVALID_REQUEST, "empty batch"),
            Value::Array(batch) => {
                let replies: Vec<Value> = batch.iter().filter_map(|m| self.handle_value(m)).collect();
                if replies.is_empty() {
                    return None;
                }
                Value::Array(replies)
            }
            _ => self.handle_value(&msg)?,
        };
        Some(reply.to_string())
    }

    fn handle_value(&self, msg: &Value) -> Option<Value> {
        let Some(obj) = msg.as_object() else {
            return Some(error(&Value::Null, INVALID_REQUEST, "expected a JSON-RPC object"));
        };
        let id = obj.get("id").cloned();
        let Some(method) = obj.get("method").and_then(Value::as_str) else {
            // A response to a request of ours (we send none), or junk.
            return id
                .filter(|_| !obj.contains_key("result") && !obj.contains_key("error"))
                .map(|id| error(&id, INVALID_REQUEST, "expected a method"));
        };
        // Notifications (no id) get no reply.
        let id = id?;
        if !matches!(id, Value::String(_) | Value::Number(_)) {
            return Some(error(&Value::Null, INVALID_REQUEST, "id must be a string or a number"));
        }
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        Some(match self.call(method, &params) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, message)) => error(&id, code, &message),
        })
    }

    fn call(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => Ok(initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_list()})),
            "tools/call" => {
                let name = params.get("name").and_then(Value::as_str).ok_or((INVALID_PARAMS, "name is required".to_string()))?;
                if !commands().any(|c| c.get_name() == name) {
                    return Err((INVALID_PARAMS, format!("unknown tool '{name}'")));
                }
                let args = match params.get("arguments").and_then(|a| a.get("args")) {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(items)) => items
                        .iter()
                        .map(|v| v.as_str().map(OsString::from))
                        .collect::<Option<Vec<_>>>()
                        .ok_or((INVALID_PARAMS, "args must be an array of strings".to_string()))?,
                    Some(_) => return Err((INVALID_PARAMS, "args must be an array of strings".into())),
                };
                Ok(self.call_tool(name, args))
            }
            _ => Err((METHOD_NOT_FOUND, format!("unknown method '{method}'"))),
        }
    }

    /// Runs `terrio <name> <args>`; a failing command is a result the model reads (`isError`),
    /// not a protocol error.
    fn call_tool(&self, name: &str, args: Vec<OsString>) -> Value {
        let (program, before) = self.terrio.split_first().expect("the terrio command line has a program");
        let output = Command::new(program).args(before).arg(name).args(args).stdin(Stdio::null()).output();
        let (text, failed) = match output {
            Ok(out) => {
                let mut text = clip(&out.stdout);
                let stderr = clip(&out.stderr);
                if !stderr.is_empty() {
                    if !text.is_empty() && !text.ends_with('\n') {
                        text.push('\n');
                    }
                    text.push_str(&stderr);
                }
                if !out.status.success() {
                    let code = out.status.code().map_or_else(|| "a signal".to_string(), |c| format!("exit code {c}"));
                    text.push_str(&format!("\n(terrio {name} failed with {code})"));
                }
                if text.trim().is_empty() {
                    text = format!("terrio {name} finished.");
                }
                (text, !out.status.success())
            }
            Err(e) => (format!("could not run terrio: {e}"), true),
        };
        json!({"content": [{"type": "text", "text": text}], "isError": failed})
    }
}

/// Serves MCP over standard input and output, one JSON-RPC message per line, until the client
/// closes standard input.
pub fn serve(terrio: Vec<OsString>) -> std::io::Result<()> {
    let server = Server::new(terrio);
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = server.handle(&line) {
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// The command line that runs this terrio binary.
pub fn this_binary() -> std::io::Result<Vec<OsString>> {
    Ok(vec![std::env::current_exe()?.into_os_string()])
}

fn initialize(params: &Value) -> Value {
    // Speak the client's version when we know it, else our newest.
    let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or("");
    let version = VERSIONS.iter().find(|v| **v == asked).copied().unwrap_or(VERSIONS[0]);
    json!({
        "protocolVersion": version,
        "capabilities": {"tools": {"listChanged": false}},
        "serverInfo": {"name": "terrio", "title": "terrio", "version": env!("CARGO_PKG_VERSION")},
        "instructions": INSTRUCTIONS,
    })
}

/// The commands served as tools: all but `mcp` itself and `help`.
fn commands() -> impl Iterator<Item = clap::Command> {
    let mut cli = crate::Cli::command();
    cli.build();
    cli.get_subcommands().filter(|c| !matches!(c.get_name(), "mcp" | "help")).cloned().collect::<Vec<_>>().into_iter()
}

fn tool_list() -> Vec<Value> {
    commands()
        .map(|mut c| {
            let name = c.get_name().to_string();
            let ro = READ_ONLY.contains(&name.as_str());
            json!({
                "name": name,
                "description": c.render_long_help().to_string().trim_end(),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "args": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": format!("The command line after `terrio {name}`, one argument per item"),
                        }
                    },
                    "required": ["args"],
                },
                // Commands other than these write files, which can overwrite existing ones.
                "annotations": {"readOnlyHint": ro, "destructiveHint": !ro, "idempotentHint": true, "openWorldHint": true},
            })
        })
        .collect()
}

/// Output as text, cut to [`MAX_OUTPUT`] bytes.
fn clip(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    if text.len() <= MAX_OUTPUT {
        return text.into_owned();
    }
    let mut end = MAX_OUTPUT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… (output cut at {MAX_OUTPUT} of {} bytes)", &text[..end], text.len())
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    // These calls run no command (tests/mcp.rs does, through the binary).
    fn server() -> Server {
        Server::new(vec!["terrio".into()])
    }

    fn request(s: &Server, method: &str, params: Value) -> Value {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        serde_json::from_str(&s.handle(&msg).expect("a reply")).unwrap()
    }

    #[test]
    fn initializes_with_the_clients_version() {
        let r = request(&server(), "initialize", json!({"protocolVersion": "2025-03-26"}));
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(r["result"]["serverInfo"]["name"], "terrio");
        let r = request(&server(), "initialize", json!({"protocolVersion": "1999-01-01"}));
        assert_eq!(r["result"]["protocolVersion"], VERSIONS[0]);
    }

    #[test]
    fn lists_every_command_but_mcp() {
        let r = request(&server(), "tools/list", json!({}));
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"info") && names.contains(&"convert") && names.contains(&"filter"));
        assert!(!names.contains(&"mcp") && !names.contains(&"help"));
        let info = tools.iter().find(|t| t["name"] == "info").unwrap();
        assert!(info["description"].as_str().unwrap().contains("--json"));
        assert_eq!(info["annotations"]["readOnlyHint"], true);
        let convert = tools.iter().find(|t| t["name"] == "convert").unwrap();
        assert_eq!(convert["annotations"]["readOnlyHint"], false);
    }

    #[test]
    fn rejects_bad_calls() {
        let s = server();
        assert_eq!(request(&s, "tools/call", json!({"name": "help", "arguments": {"args": []}}))["error"]["code"], INVALID_PARAMS);
        assert_eq!(request(&s, "tools/call", json!({"name": "info", "arguments": {"args": [1]}}))["error"]["code"], INVALID_PARAMS);
        assert_eq!(request(&s, "nope", json!({}))["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(serde_json::from_str::<Value>(&s.handle("{").unwrap()).unwrap()["error"]["code"], PARSE_ERROR);
        // Notifications get no reply.
        assert!(s.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    }

    #[test]
    fn clips_long_output() {
        let long = "é".repeat(MAX_OUTPUT);
        let clipped = clip(long.as_bytes());
        assert!(clipped.len() < long.len() && clipped.ends_with("bytes)"));
    }
}

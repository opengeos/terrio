//! `terrio mcp`: a Model Context Protocol server, so that an AI agent (Claude Code, Claude Desktop,
//! Cursor, VS Code…) can run terrio's commands. Each command is a tool whose arguments are its
//! command line, and whose description is its `--help`.
//!
//! [`Server::handle`] takes one JSON-RPC message and gives back the reply, so the stdio loop
//! ([`serve`]) and the tests drive the same code. A tool call runs the command in a child process
//! of terrio itself, on a thread of its own that sends the reply when the command is done: the
//! command's output is captured rather than written over the protocol on standard output, a crash
//! or an out-of-memory stays the child's, and the server keeps answering (`ping`, other calls)
//! meanwhile. A cancelled call (`notifications/cancelled`) kills its command, and so does the
//! client going away.

use clap::CommandFactory;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, Read, Write};
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// The protocol versions this server speaks, newest first.
const VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// The most bytes of a command's output (standard output and error together) returned to the
/// agent; the rest is read and dropped.
const MAX_OUTPUT: usize = 100_000;

/// What the model is told about the server when it connects.
const INSTRUCTIONS: &str = "terrio reads, writes and converts geospatial rasters, vector datasets and point clouds in pure Rust, with GDAL-style commands.\n\
- Each tool runs one `terrio` command; `args` is its command line after the command name, one argument per item, e.g. info with [\"dem.tif\", \"--json\"].\n\
- The tool's description is the command's --help: its arguments and options.\n\
- Inputs can be local paths, URLs, cloud objects (s3://, gs://, az://) or zip members; outputs are local files. Use absolute paths.\n\
- Start with info (--json) to see what a dataset holds; formats lists the formats terrio reads and writes.";

/// Commands that only read: they write no file.
const READ_ONLY: &[&str] = &["info", "formats", "crs", "mdinfo", "mdread", "compare"];

type RpcError = (i64, String);

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// The server.
pub struct Server {
    /// The command line that runs terrio: the binary, or a Python interpreter with `-m terrio`.
    terrio: Arc<Vec<OsString>>,
    /// Where the replies of tool calls go, one line of JSON each, once their command is done.
    send: Sender<String>,
    /// The tool calls running, by request id (as JSON), with the flag that cancels each.
    running: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
    workers: Vec<JoinHandle<()>>,
}

impl Server {
    /// A server whose tools run `terrio` (the program and any arguments before the command), and
    /// send their replies to `send`.
    pub fn new(terrio: Vec<OsString>, send: Sender<String>) -> Self {
        Self { terrio: Arc::new(terrio), send, running: Arc::default(), workers: Vec::new() }
    }

    /// Kills the commands still running and waits for their threads: the client has gone.
    pub fn shutdown(mut self) {
        for cancel in self.running.lock().unwrap().values() {
            cancel.store(true, Ordering::Relaxed);
        }
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }

    /// Handles one message (a line of JSON): the reply to send, or `None` for a notification, a
    /// response, or a tool call, whose reply goes to the server's `send` once its command is done.
    pub fn handle(&mut self, message: &str) -> Option<String> {
        let msg: Value = match serde_json::from_str(message) {
            Ok(v) => v,
            Err(e) => return Some(error(&Value::Null, PARSE_ERROR, &format!("not JSON: {e}")).to_string()),
        };
        let reply = match &msg {
            // A batch (protocol 2025-03-26): one reply per request, none when all are notifications.
            Value::Array(batch) if batch.is_empty() => error(&Value::Null, INVALID_REQUEST, "empty batch"),
            // Its tool calls run here, since the batch's replies go back together.
            Value::Array(batch) => {
                let replies: Vec<Value> = batch.iter().filter_map(|m| self.handle_value(m, true)).collect();
                if replies.is_empty() {
                    return None;
                }
                Value::Array(replies)
            }
            _ => self.handle_value(&msg, false)?,
        };
        Some(reply.to_string())
    }

    /// The reply to `msg`; a tool call's runs on a thread of its own and is sent from there,
    /// unless `inline`.
    fn handle_value(&mut self, msg: &Value, inline: bool) -> Option<Value> {
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
        if method == "notifications/cancelled" {
            let request = obj.get("params").and_then(|p| p.get("requestId")).map(Value::to_string);
            if let Some(cancel) = request.and_then(|r| self.running.lock().unwrap().get(&r).cloned()) {
                cancel.store(true, Ordering::Relaxed);
            }
        }
        // Notifications (no id) get no reply.
        let id = id?;
        if !matches!(id, Value::String(_) | Value::Number(_)) {
            return Some(error(&Value::Null, INVALID_REQUEST, "id must be a string or a number"));
        }
        let params = obj.get("params").cloned().unwrap_or(Value::Null);
        let result = |r: Value| json!({"jsonrpc": "2.0", "id": id, "result": r});
        if method == "tools/call" {
            return Some(match tool_call(&params) {
                Err((code, message)) => error(&id, code, &message),
                Ok((name, args)) if inline => result(call_tool(&self.terrio, &name, args, &AtomicBool::new(false))?),
                Ok((name, args)) => {
                    self.spawn(id, name, args);
                    return None;
                }
            });
        }
        Some(match self.call(method, &params) {
            Ok(r) => result(r),
            Err((code, message)) => error(&id, code, &message),
        })
    }

    /// Runs a tool call on a thread of its own, which sends its reply unless it was cancelled.
    fn spawn(&mut self, id: Value, name: String, args: Vec<OsString>) {
        let key = id.to_string();
        let cancel = Arc::new(AtomicBool::new(false));
        self.running.lock().unwrap().insert(key.clone(), cancel.clone());
        let (terrio, running, send) = (self.terrio.clone(), self.running.clone(), self.send.clone());
        self.workers.retain(|w| !w.is_finished());
        self.workers.push(std::thread::spawn(move || {
            let reply = call_tool(&terrio, &name, args, &cancel);
            // Unless a request reusing the id took the entry meanwhile.
            let mut running = running.lock().unwrap();
            if running.get(&key).is_some_and(|c| Arc::ptr_eq(c, &cancel)) {
                running.remove(&key);
            }
            drop(running);
            // A cancelled request gets no reply.
            if let Some(r) = reply {
                let _ = send.send(json!({"jsonrpc": "2.0", "id": id, "result": r}).to_string());
            }
        }));
    }

    fn call(&self, method: &str, params: &Value) -> Result<Value, RpcError> {
        match method {
            "initialize" => Ok(initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": tool_list()})),
            _ => Err((METHOD_NOT_FOUND, format!("unknown method '{method}'"))),
        }
    }
}

/// A tool call's name and command-line arguments.
fn tool_call(params: &Value) -> Result<(String, Vec<OsString>), RpcError> {
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
    Ok((name.to_string(), args))
}

/// Runs `terrio <name> <args>`: the tool's result, or `None` when `cancel` was set and the command
/// killed. A failing command is a result the model reads (`isError`), not a protocol error.
fn call_tool(terrio: &[OsString], name: &str, args: Vec<OsString>, cancel: &AtomicBool) -> Option<Value> {
    let (text, failed) = match run(terrio, name, args, cancel) {
        Ok(None) => return None,
        Ok(Some((status, stdout, stderr))) => {
            // Standard error, which says why a command failed, first: standard output gets the room
            // it leaves.
            let stderr = clip(String::from_utf8_lossy(&stderr.0).into_owned(), stderr.1, MAX_OUTPUT);
            let room = MAX_OUTPUT.saturating_sub(stderr.len());
            let mut text = clip(String::from_utf8_lossy(&stdout.0).into_owned(), stdout.1, room);
            if !stderr.is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&stderr);
            }
            let failed = !status.success();
            if failed {
                let code = status.code().map_or_else(|| "a signal".to_string(), |c| format!("exit code {c}"));
                text.push_str(&format!("\n(terrio {name} failed with {code})"));
            }
            if text.trim().is_empty() {
                text = format!("terrio {name} finished.");
            }
            (text, failed)
        }
        Err(e) => (format!("could not run terrio: {e}"), true),
    };
    Some(json!({"content": [{"type": "text", "text": text}], "isError": failed}))
}

/// A command's output: its first [`MAX_OUTPUT`] bytes, and how many it wrote.
type Output = (Vec<u8>, usize);

/// Runs the command, killing it if `cancel` is set (`None`): its exit status, standard output and
/// standard error.
fn run(terrio: &[OsString], name: &str, args: Vec<OsString>, cancel: &AtomicBool) -> std::io::Result<Option<(ExitStatus, Output, Output)>> {
    let (program, before) = terrio.split_first().expect("the terrio command line has a program");
    let mut child = Command::new(program)
        .args(before)
        .arg(name)
        .args(args)
        .env("TERRIO_MCP_TOOL", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Both pipes are drained as the command writes, so that it never blocks on a full one.
    let stdout = read_bounded(child.stdout.take().expect("piped"));
    let stderr = read_bounded(child.stderr.take().expect("piped"));
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e);
            }
        }
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let joined = |h: JoinHandle<Output>| h.join().unwrap_or_default();
    Ok(Some((status, joined(stdout), joined(stderr))))
}

/// Reads `r` to its end on a thread, keeping the first [`MAX_OUTPUT`] bytes.
fn read_bounded(mut r: impl Read + Send + 'static) -> JoinHandle<Output> {
    std::thread::spawn(move || {
        let (mut kept, mut total, mut buf) = (Vec::new(), 0, [0u8; 8192]);
        loop {
            let n = match r.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                // A signal: the pipe is still open, so keep draining it.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let room = MAX_OUTPUT.saturating_sub(kept.len());
            kept.extend_from_slice(&buf[..n.min(room)]);
            total += n;
        }
        (kept, total)
    })
}

/// Serves MCP over standard input and output, one JSON-RPC message per line, until the client
/// closes standard input.
pub fn serve(terrio: Vec<OsString>) -> std::io::Result<()> {
    let (send, replies) = std::sync::mpsc::channel::<String>();
    // One writer, so that replies from tool calls' threads never interleave.
    let writer = std::thread::spawn(move || -> std::io::Result<()> {
        let mut out = std::io::stdout().lock();
        for reply in replies {
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
        Ok(())
    });
    let mut server = Server::new(terrio, send.clone());
    let read = (|| {
        for line in std::io::stdin().lock().lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            if let Some(reply) = server.handle(&line) {
                let _ = send.send(reply);
            }
        }
        Ok(())
    })();
    server.shutdown();
    drop(send);
    writer.join().unwrap_or(Ok(())).and(read)
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

/// Whether the help of `c` or of one of its subcommands (`filter merge`) offers `@FILE` lists.
fn offers_file_lists(c: &mut clap::Command) -> bool {
    c.render_long_help().to_string().contains("`@FILE`") || c.get_subcommands_mut().any(offers_file_lists)
}

fn tool_list() -> Vec<Value> {
    commands()
        .map(|mut c| {
            let name = c.get_name().to_string();
            let ro = READ_ONLY.contains(&name.as_str());
            let mut description = c.render_long_help().to_string().trim_end().to_string();
            // A group of subcommands (`vsi list`, `vsi copy`...): each one's help too.
            let subcommands: Vec<clap::Command> = c.get_subcommands().filter(|s| s.get_name() != "help").cloned().collect();
            for mut sub in subcommands {
                let help = sub.render_long_help().to_string();
                description.push_str(&format!("\n\n`{name} {}`:\n{}", sub.get_name(), help.trim_end()));
            }
            // The help offers `@FILE` input lists, which tool calls refuse (`expand_file_lists`).
            if offers_file_lists(&mut c) {
                description.push_str("\n\nOver MCP, `@FILE` lists are refused: pass each input as an item of `args`.");
            }
            json!({
                "name": name,
                "description": description,
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
                "annotations": {"readOnlyHint": ro, "destructiveHint": !ro, "idempotentHint": ro, "openWorldHint": true},
            })
        })
        .collect()
}

/// `text`, cut to `max` bytes, saying so when it is or when the command wrote `total` bytes, more
/// than were kept.
fn clip(mut text: String, total: usize, max: usize) -> String {
    if text.len() <= max && total <= text.len() {
        return text;
    }
    let mut end = max.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    format!("{text}\n… (output cut at {end} of {} bytes)", total.max(text.len()))
}

fn error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

#[cfg(test)]
mod tests {
    use super::*;

    // These calls run no command (tests/mcp.rs does, through the binary).
    fn server() -> Server {
        Server::new(vec!["terrio".into()], std::sync::mpsc::channel().0)
    }

    fn request(s: &mut Server, method: &str, params: Value) -> Value {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params}).to_string();
        serde_json::from_str(&s.handle(&msg).expect("a reply")).unwrap()
    }

    #[test]
    fn initializes_with_the_clients_version() {
        let r = request(&mut server(), "initialize", json!({"protocolVersion": "2025-03-26"}));
        assert_eq!(r["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(r["result"]["serverInfo"]["name"], "terrio");
        let r = request(&mut server(), "initialize", json!({"protocolVersion": "1999-01-01"}));
        assert_eq!(r["result"]["protocolVersion"], VERSIONS[0]);
    }

    #[test]
    fn lists_every_command_but_mcp() {
        let r = request(&mut server(), "tools/list", json!({}));
        let tools = r["result"]["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"info") && names.contains(&"convert") && names.contains(&"filter"));
        for name in ["centroid", "singlepart", "vertices", "footprint", "sample", "contour", "zonal-stats", "sql"] {
            assert!(names.contains(&name), "missing processing command {name}");
        }
        assert!(!names.contains(&"mcp") && !names.contains(&"help"));
        for name in ["sample", "zonal-stats"] {
            let tool = tools.iter().find(|t| t["name"] == name).unwrap();
            let help = tool["description"].as_str().unwrap();
            assert!(help.contains("--t-srs") && help.contains("--decimal-places"));
        }
        let info = tools.iter().find(|t| t["name"] == "info").unwrap();
        assert!(info["description"].as_str().unwrap().contains("--json"));
        assert_eq!(info["annotations"]["readOnlyHint"], true);
        let convert = tools.iter().find(|t| t["name"] == "convert").unwrap();
        assert_eq!(convert["annotations"]["readOnlyHint"], false);
        assert_eq!(convert["annotations"]["idempotentHint"], false);
    }

    #[test]
    fn rejects_bad_calls() {
        let s = &mut server();
        assert_eq!(request(s, "tools/call", json!({"name": "help", "arguments": {"args": []}}))["error"]["code"], INVALID_PARAMS);
        assert_eq!(request(s, "tools/call", json!({"name": "info", "arguments": {"args": [1]}}))["error"]["code"], INVALID_PARAMS);
        assert_eq!(request(s, "nope", json!({}))["error"]["code"], METHOD_NOT_FOUND);
        assert_eq!(serde_json::from_str::<Value>(&s.handle("{").unwrap()).unwrap()["error"]["code"], PARSE_ERROR);
        // Notifications get no reply.
        assert!(s.handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
    }

    /// A tool call runs while the server answers other requests, and a cancelled one is killed
    /// and gets no reply.
    #[cfg(unix)]
    #[test]
    fn answers_while_a_call_runs_and_cancels_it() {
        let (send, replies) = std::sync::mpsc::channel();
        // `sh -c 'sleep 30' info`: a command that takes a while.
        let mut s = Server::new(vec!["sh".into(), "-c".into(), "sleep 30".into()], send);
        let call = json!({"jsonrpc": "2.0", "id": "slow", "method": "tools/call", "params": {"name": "info", "arguments": {"args": []}}});
        assert!(s.handle(&call.to_string()).is_none());
        assert!(request(&mut s, "ping", json!({}))["result"].is_object());
        let started = std::time::Instant::now();
        s.handle(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": "slow"}}).to_string());
        s.shutdown();
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(replies.try_recv().is_err());
    }

    #[test]
    fn clips_long_output() {
        let long = "é".repeat(MAX_OUTPUT);
        let clipped = clip(long.clone(), long.len(), MAX_OUTPUT);
        assert!(clipped.len() < long.len() && clipped.ends_with(&format!("of {} bytes)", long.len())));
        // Output read only in part says how much there was.
        assert!(clip("abc".into(), MAX_OUTPUT + 1, MAX_OUTPUT).ends_with(&format!("of {} bytes)", MAX_OUTPUT + 1)));
        assert_eq!(clip("abc".into(), 3, MAX_OUTPUT), "abc");
        assert_eq!(clip("abc".into(), 3, 1), "a\n… (output cut at 1 of 3 bytes)");
    }
    #[cfg(unix)]
    #[test]
    fn panicked_query_tool_is_an_error() {
        let command = vec!["sh".into(), "-c".into(), "echo \"thread 'main' panicked at source.rs:1\" >&2; exit 101".into()];
        let result = call_tool(&command, "compare", Vec::new(), &AtomicBool::new(false)).unwrap();
        assert_eq!(result["isError"], true);
    }
}

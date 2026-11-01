//! `terrio ui` through the binary: a real server answering real HTTP.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};

/// A running `terrio ui`, killed when dropped.
struct Ui {
    child: Child,
    addr: String,
    token: String,
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start(cwd: &std::path::Path) -> Ui {
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio"))
        .args(["ui", "--no-open"])
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
    let url = line.trim().strip_prefix("terrio ui: http://").unwrap_or_else(|| panic!("unexpected first line: {line}"));
    let (addr, token) = url.split_once("/#token=").unwrap();
    assert!(addr.starts_with("127.0.0.1:"), "listens on the loopback interface only: {addr}");
    assert_eq!(token.len(), 32);
    Ui { addr: addr.to_string(), token: token.to_string(), child }
}

impl Ui {
    /// Sends a request with the given extra headers: the status, the raw head and the body.
    fn send(&self, method: &str, path: &str, headers: &str, body: &str) -> (u16, String, String) {
        let mut stream = TcpStream::connect(&self.addr).unwrap();
        write!(stream, "{method} {path} HTTP/1.1\r\nHost: {}\r\n{headers}Content-Length: {}\r\n\r\n{body}", self.addr, body.len()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap();
        (head.split(' ').nth(1).unwrap().parse().unwrap(), head.to_string(), body.to_string())
    }

    fn api(&self, path: &str, body: Value) -> (u16, Value) {
        let token = format!("X-Terrio-Token: {}\r\n", self.token);
        let (status, _, reply) = self.send("POST", path, &token, &body.to_string());
        (status, serde_json::from_str(&reply).unwrap())
    }

    fn run(&self, name: &str, args: &[&str]) -> (u16, Value) {
        self.api("/api/run", json!({"name": name, "arguments": {"args": args}}))
    }
}

const POINTS: &str =
    r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"a":1},"geometry":{"type":"Point","coordinates":[1,2]}}]}"#;

#[test]
fn serves_the_page_without_the_token_and_the_api_only_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let ui = start(dir.path());

    let (status, head, page) = ui.send("GET", "/", "", "");
    assert_eq!(status, 200);
    assert!(head.contains("Content-Security-Policy: default-src 'none'") && page.contains("<title>Terrio</title>"));
    assert!(!page.contains(&ui.token), "the page must not embed the secret");

    assert_eq!(ui.send("GET", "/api/catalog", "", "").0, 403);
    let (status, _, _) = ui.send("POST", "/api/run", "", &json!({"name": "formats"}).to_string());
    assert_eq!(status, 403);
    assert_eq!(ui.send("GET", "/missing", "", "").0, 404);
    assert_eq!(ui.send("DELETE", "/", "", "").0, 405);

    let token = format!("X-Terrio-Token: {}\r\n", ui.token);
    let (status, _, catalog) = ui.send("GET", "/api/catalog", &token, "");
    assert_eq!(status, 200);
    let catalog: Value = serde_json::from_str(&catalog).unwrap();
    assert_eq!(catalog["catalog"]["schema_version"], "0.1");
    let names: Vec<&str> =
        catalog["catalog"]["command"]["subcommands"].as_array().unwrap().iter().map(|c| c["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"warp") && names.contains(&"vsi"));
    assert!(!names.contains(&"ui") && !names.contains(&"mcp"), "the servers are not commands to run from a form");
}

#[test]
fn runs_commands_and_reports_failures_as_results() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("points.geojson"), POINTS).unwrap();
    let ui = start(dir.path());

    // A relative path is taken from the directory `terrio ui` started in.
    let (status, reply) = ui.run("convert", &["points.geojson", "points.csv"]);
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["isError"], false, "{reply}");
    assert!(std::fs::read_to_string(dir.path().join("points.csv")).unwrap().contains('1'));

    let (status, reply) = ui.run("info", &["missing.tif"]);
    assert_eq!(status, 200);
    assert_eq!(reply["isError"], true);
    assert!(reply["content"][0]["text"].as_str().unwrap().contains("missing.tif"), "{reply}");

    // Nested commands take their path as the first arguments.
    let (status, reply) = ui.run("vsi", &["list", "."]);
    assert_eq!((status, &reply["isError"]), (200, &json!(false)), "{reply}");
    assert!(reply["content"][0]["text"].as_str().unwrap().contains("points.geojson"), "{reply}");

    for name in ["ui", "mcp", "not-a-command"] {
        let (status, reply) = ui.run(name, &[]);
        assert_eq!(status, 400, "{name}: {reply}");
    }
    assert_eq!(ui.send("POST", "/api/run", &format!("X-Terrio-Token: {}\r\n", ui.token), "not json").0, 400);
}

#[test]
fn lists_directories_for_the_file_picker() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("points.geojson"), POINTS).unwrap();
    let ui = start(dir.path());
    let (status, reply) = ui.api("/api/list", json!({}));
    assert_eq!(status, 200, "{reply}");
    assert_eq!(reply["entries"][0]["name"], "points.geojson");
    assert_eq!(reply["entries"][0]["size"], POINTS.len());
    let (status, _) = ui.api("/api/list", json!({"path": dir.path().join("missing")}));
    assert_eq!(status, 400);
}

#[test]
fn is_not_in_the_catalog_and_a_metadata_request_does_not_start_it() {
    let output = Command::new(env!("CARGO_BIN_EXE_terrio")).args(["ui", "--json-usage"]).stdin(Stdio::null()).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}

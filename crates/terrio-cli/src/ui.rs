//! `terrio ui`: a form for every command, in the browser, served from this machine.
//!
//! The page is generated in the browser from the same catalog as `--json-usage`, so a new command
//! or option gets its form without a change here, and runs commands exactly as MCP does: each one
//! in a child process of terrio ([`crate::mcp::call_tool`]). Closing the connection, which is how
//! the page cancels a run, kills the command.
//!
//! The server runs commands as the user, so a web page or another local user must not reach it.
//! It listens on the loopback interface only; every API request needs the session's random token
//! in a header, which a cross-site page cannot send without a preflight this server never grants;
//! and the `Host` and `Origin` headers must name this server, which stops DNS rebinding. The token
//! travels in the URL's fragment, which browsers do not send to servers or in `Referer` headers.

use crate::mcp;
use serde_json::{Value, json};
use std::ffi::OsString;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{MAIN_SEPARATOR_STR, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The page: one file with no external resources, so that it works offline.
const PAGE: &str = include_str!("ui.html");
/// The longest request head accepted, in bytes.
const MAX_HEAD: usize = 64 << 10;
/// The longest request body accepted, in bytes.
const MAX_BODY: usize = 1 << 20;
/// How long a client may take to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// How often a run checks whether its command is done and its client still there.
const POLL: Duration = Duration::from_millis(50);
const CONTENT_SECURITY_POLICY: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

/// Arguments of `terrio ui`.
#[derive(clap::Args)]
pub struct Args {
    /// The port to listen on, on 127.0.0.1. 0 picks a free one.
    #[arg(long, default_value_t = 0)]
    port: u16,
    /// Print the address without opening a browser.
    #[arg(long)]
    no_open: bool,
}

/// What every connection needs.
struct Session {
    /// The command line that runs terrio.
    terrio: Vec<OsString>,
    /// The secret every API request must carry.
    token: String,
    /// The port listened on, which `Host` and `Origin` headers must name.
    port: u16,
    /// The `--json-usage` catalog, as JSON.
    catalog: String,
}

/// Serves the page until the process is stopped.
pub fn serve(args: Args, terrio: Vec<OsString>) -> std::io::Result<()> {
    // The binary dies on SIGPIPE so that `terrio formats | head` exits quietly; here, a browser
    // that goes away must not stop the server. Commands restore the default when they start.
    #[cfg(unix)]
    // SAFETY: changes this process's handling of a signal, with a handler that runs no code.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    let listener = TcpListener::bind(("127.0.0.1", args.port))?;
    let session = session(terrio, listener.local_addr()?.port())?;
    let url = format!("http://127.0.0.1:{}/#token={}", session.port, session.token);
    let mut out = std::io::stdout();
    let _ = writeln!(out, "terrio ui: {url}\nPress Ctrl-C to stop.");
    let _ = out.flush();
    if !args.no_open {
        open_browser(&url);
    }
    accept(&listener, &Arc::new(session));
    Ok(())
}

/// A session with a new token.
fn session(terrio: Vec<OsString>, port: u16) -> std::io::Result<Session> {
    let mut secret = [0u8; 16];
    getrandom::fill(&mut secret).map_err(|e| std::io::Error::other(format!("no random numbers for the session token: {e}")))?;
    let token = secret.iter().map(|b| format!("{b:02x}")).collect();
    let catalog = crate::usage::requested(&["terrio".into(), "--json-usage".into()]).expect("the root catalog").to_string();
    Ok(Session { terrio, token, port, catalog })
}

/// Answers connections, each on a thread of its own, since a run lasts as long as its command.
fn accept(listener: &TcpListener, session: &Arc<Session>) {
    for stream in listener.incoming().flatten() {
        let session = session.clone();
        // The stack of `run_as`: checking a command's name builds the command catalog. A thread
        // that cannot start drops its connection, and the server carries on.
        let _ = std::thread::Builder::new().stack_size(crate::STACK_SIZE).spawn(move || connection(stream, &session));
    }
}

/// Opens `url` in the default browser. Failing is fine: the address has been printed.
fn open_browser(url: &str) {
    let mut command = if cfg!(target_os = "macos") {
        Command::new("open")
    } else if cfg!(windows) {
        // Not `cmd /c start`, whose parser would reinterpret the address.
        let mut c = Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        Command::new("xdg-open")
    };
    let spawned = command.arg(url).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    if let Ok(mut child) = spawned {
        std::thread::spawn(move || child.wait());
    }
}

/// A request's parts that matter here.
#[derive(Debug)]
struct Request {
    method: String,
    path: String,
    /// Header names in lower case, with their values.
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// Reads one request; a malformed or oversized one is an error whose text goes to the client.
fn read_request(stream: &mut impl Read) -> Result<Request, String> {
    let (mut data, mut buf) = (Vec::new(), [0u8; 8192]);
    let head_end = loop {
        if let Some(end) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break end;
        }
        if data.len() > MAX_HEAD {
            return Err("request head too long".into());
        }
        match stream.read(&mut buf) {
            Ok(0) => return Err("connection closed during the request".into()),
            Ok(n) => data.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    };
    let head = std::str::from_utf8(&data[..head_end]).map_err(|_| "request head is not UTF-8")?;
    let mut lines = head.split("\r\n");
    let mut start = lines.next().unwrap_or_default().split(' ');
    let (method, target) = (start.next().unwrap_or_default().to_string(), start.next().ok_or("malformed request line")?);
    let path = target.split('?').next().unwrap_or_default().to_string();
    let headers: Vec<(String, String)> =
        lines.filter_map(|l| l.split_once(':')).map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let length = headers.iter().find(|(n, _)| n == "content-length").map_or(Ok(0), |(_, v)| v.parse::<usize>());
    let length = length.map_err(|_| "bad content length")?;
    if length > MAX_BODY {
        return Err("request body too long".into());
    }
    let mut body = data[head_end + 4..].to_vec();
    while body.len() < length {
        match stream.read(&mut buf) {
            Ok(0) => return Err("connection closed during the request".into()),
            Ok(n) => body.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e.to_string()),
        }
    }
    body.truncate(length);
    Ok(Request { method, path, headers, body })
}

/// Handles one connection, which carries one request.
fn connection(mut stream: TcpStream, session: &Session) {
    let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
    let _ = stream.set_write_timeout(Some(REQUEST_TIMEOUT));
    let reply = match read_request(&mut stream) {
        Ok(request) => match check(&request, session) {
            Ok(()) if request.method == "POST" && request.path == "/api/run" => match parse_run(&request) {
                Ok((name, args)) => match run_until_client_leaves(&stream, session, &name, args) {
                    Some(result) => (200, "application/json", result.to_string()),
                    None => return,
                },
                Err(message) => failure(400, &message),
            },
            Ok(()) => route(&request, session),
            Err(reply) => reply,
        },
        Err(message) => failure(400, &message),
    };
    respond(&mut stream, reply);
}

/// A reply's status, content type and body.
type Reply = (u16, &'static str, String);

fn failure(status: u16, message: &str) -> Reply {
    (status, "application/json", json!({"error": message}).to_string())
}

/// Refuses a request not meant for this server, or an API request without the token.
fn check(request: &Request, session: &Session) -> Result<(), Reply> {
    let hosts = [format!("127.0.0.1:{}", session.port), format!("localhost:{}", session.port)];
    if !request.header("host").is_some_and(|h| hosts.iter().any(|a| a == h)) {
        return Err(failure(403, "wrong Host header"));
    }
    if request.header("origin").is_some_and(|o| !o.strip_prefix("http://").is_some_and(|h| hosts.iter().any(|a| a == h))) {
        return Err(failure(403, "wrong Origin header"));
    }
    if request.path.starts_with("/api/") && !request.header("x-terrio-token").is_some_and(|t| same_secret(t, &session.token)) {
        return Err(failure(403, "missing or wrong session token: open the address `terrio ui` printed"));
    }
    Ok(())
}

/// Compares secrets in a time that does not depend on where they differ.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

/// Every request but a run, which needs the connection.
fn route(request: &Request, session: &Session) -> Reply {
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "/") => (200, "text/html; charset=utf-8", PAGE.to_string()),
        ("GET", "/api/catalog") => {
            let cwd = std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default();
            let catalog = format!("{{\"cwd\":{},\"separator\":{},\"catalog\":{}}}", json!(cwd), json!(MAIN_SEPARATOR_STR), session.catalog);
            (200, "application/json", catalog)
        }
        ("POST", "/api/list") => match list_directory(&request.body) {
            Ok(listing) => (200, "application/json", listing.to_string()),
            Err(message) => failure(400, &message),
        },
        (_, "/" | "/api/catalog" | "/api/list" | "/api/run") => failure(405, "method not allowed"),
        _ => failure(404, "not found"),
    }
}

/// A run's command and arguments: `{"name": …, "arguments": {"args": […]}}`, as an MCP tool call.
fn parse_run(request: &Request) -> Result<(String, Vec<OsString>), String> {
    let params: Value = serde_json::from_slice(&request.body).map_err(|e| e.to_string())?;
    mcp::tool_call(&params).map_err(|(_, message)| message)
}

/// Runs a command: its result, or `None` when the client closed the connection, which kills it.
fn run_until_client_leaves(stream: &TcpStream, session: &Session, name: &str, args: Vec<OsString>) -> Option<Value> {
    let cancel = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let run = scope.spawn(|| mcp::call_tool(&session.terrio, name, args, &cancel));
        let watching = stream.set_nonblocking(true).is_ok();
        let mut buf = [0u8; 1];
        while !run.is_finished() {
            if watching {
                let gone = match stream.peek(&mut buf) {
                    Ok(n) => n == 0,
                    Err(e) => !matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted),
                };
                if gone {
                    cancel.store(true, Ordering::Relaxed);
                }
            }
            std::thread::sleep(POLL);
        }
        let _ = stream.set_nonblocking(false);
        run.join()
            .unwrap_or_else(|_| Some(json!({"content": [{"type": "text", "text": "the command's thread panicked"}], "isError": true})))
    })
}

/// The entries of the directory named in the request (`{"path": …}`; the working directory when
/// empty), directories first. Names that are not valid Unicode are left out: the page could not
/// send them back.
fn list_directory(body: &[u8]) -> Result<Value, String> {
    let params: Value = serde_json::from_slice(body).map_err(|e| e.to_string())?;
    let path = match params.get("path").and_then(Value::as_str) {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => std::env::current_dir().map_err(|e| e.to_string())?,
    };
    let path = std::path::absolute(&path).map_err(|e| e.to_string())?;
    let mut entries: Vec<(bool, String, Option<u64>)> = std::fs::read_dir(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            // Follows symbolic links, so that a link to a directory can be opened.
            let metadata = std::fs::metadata(entry.path()).ok();
            let is_dir = metadata.as_ref().is_some_and(|m| m.is_dir());
            Some((is_dir, name, metadata.filter(|m| m.is_file()).map(|m| m.len())))
        })
        .collect();
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase())));
    let entries: Vec<Value> = entries.into_iter().map(|(dir, name, size)| json!({"name": name, "dir": dir, "size": size})).collect();
    Ok(json!({"path": display(&path), "parent": path.parent().map(display), "entries": entries}))
}

fn display(path: &Path) -> String {
    path.display().to_string()
}

fn respond(stream: &mut TcpStream, (status, content_type, body): Reply) {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nContent-Security-Policy: {CONTENT_SECURITY_POLICY}\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(body.as_bytes()));
    let _ = stream.shutdown(Shutdown::Write);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    /// Serves a session on a free port, with `terrio` as its command line.
    fn start(terrio: Vec<OsString>) -> (SocketAddr, Arc<Session>) {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .stack_size(crate::STACK_SIZE)
                .spawn_scoped(scope, || {
                    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
                    let addr = listener.local_addr().unwrap();
                    let session = Arc::new(session(terrio, addr.port()).unwrap());
                    let serving = session.clone();
                    std::thread::spawn(move || accept(&listener, &serving));
                    (addr, session)
                })
                .unwrap()
                .join()
                .unwrap()
        })
    }

    fn request(session: &Session, method: &str, path: &str, body: &str) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            headers: vec![("host".into(), format!("127.0.0.1:{}", session.port)), ("x-terrio-token".into(), session.token.clone())],
            body: body.as_bytes().to_vec(),
        }
    }

    fn test_session() -> Session {
        Session { terrio: vec![], token: "secret".into(), port: 8000, catalog: "{}".into() }
    }

    #[test]
    fn refuses_foreign_hosts_origins_and_missing_tokens() {
        let s = test_session();
        assert!(check(&request(&s, "GET", "/api/catalog", ""), &s).is_ok());

        // DNS rebinding: the attacker's name resolves to 127.0.0.1, but the header names it.
        let mut r = request(&s, "GET", "/", "");
        r.headers[0].1 = "evil.example:8000".into();
        assert_eq!(check(&r, &s).unwrap_err().0, 403);
        // The right name on another port is another server.
        let mut r = request(&s, "GET", "/", "");
        r.headers[0].1 = "127.0.0.1:8001".into();
        assert_eq!(check(&r, &s).unwrap_err().0, 403);

        let mut r = request(&s, "POST", "/api/run", "");
        r.headers.push(("origin".into(), "https://evil.example".into()));
        assert_eq!(check(&r, &s).unwrap_err().0, 403);
        let mut r = request(&s, "POST", "/api/run", "");
        r.headers.push(("origin".into(), "http://localhost:8000".into()));
        assert!(check(&r, &s).is_ok());

        let mut r = request(&s, "POST", "/api/run", "");
        r.headers[1].1 = "wrong!".into();
        assert_eq!(check(&r, &s).unwrap_err().0, 403);
        let mut r = request(&s, "POST", "/api/run", "");
        r.headers.truncate(1);
        assert_eq!(check(&r, &s).unwrap_err().0, 403);
        // The page holds no secret, so it needs no token.
        let mut r = request(&s, "GET", "/", "");
        r.headers.truncate(1);
        assert!(check(&r, &s).is_ok());
    }

    #[test]
    fn reads_a_request_sent_in_pieces_and_refuses_oversized_ones() {
        let raw = b"POST /api/list?x=1 HTTP/1.1\r\nHost: a\r\nContent-Length: 5\r\n\r\nhelloEXTRA";
        let r = read_request(&mut &raw[..]).unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str(), r.header("host"), &r.body[..]), ("POST", "/api/list", Some("a"), &b"hello"[..]));

        let raw = format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", MAX_BODY + 1);
        assert!(read_request(&mut raw.as_bytes()).unwrap_err().contains("too long"));
        let raw = vec![b'a'; MAX_HEAD + 8193];
        assert!(read_request(&mut &raw[..]).unwrap_err().contains("too long"));
        assert!(read_request(&mut &b"GET"[..]).is_err());
        assert!(read_request(&mut &b"POST / HTTP/1.1\r\nContent-Length: x\r\n\r\n"[..]).unwrap_err().contains("content length"));
    }

    #[test]
    fn lists_a_directory_with_folders_first() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.tif"), b"12345").unwrap();
        std::fs::create_dir(dir.path().join("z")).unwrap();
        let body = json!({"path": dir.path().to_str().unwrap()}).to_string();
        let listing = list_directory(body.as_bytes()).unwrap();
        assert_eq!(listing["entries"], json!([{"name": "z", "dir": true, "size": null}, {"name": "a.tif", "dir": false, "size": 5}]));
        assert_eq!(listing["parent"], json!(dir.path().parent().unwrap().to_str().unwrap()));

        let missing = json!({"path": dir.path().join("missing").to_str().unwrap()}).to_string();
        assert!(list_directory(missing.as_bytes()).unwrap_err().contains("missing"));
    }

    #[test]
    #[cfg(unix)]
    fn closing_the_connection_kills_the_command() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        let script = format!("echo $$ > '{}'; exec sleep 30", pid_file.display());
        let (addr, session) = start(vec!["sh".into(), "-c".into(), script.into()]);
        let body = json!({"name": "info", "arguments": {"args": []}}).to_string();
        let mut client = TcpStream::connect(addr).unwrap();
        write!(
            client,
            "POST /api/run HTTP/1.1\r\nHost: {addr}\r\nX-Terrio-Token: {}\r\nContent-Length: {}\r\n\r\n{body}",
            session.token,
            body.len()
        )
        .unwrap();

        let started = std::time::Instant::now();
        let pid = loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file).ok().and_then(|p| p.trim().parse::<i32>().ok()) {
                break pid;
            }
            assert!(started.elapsed() < Duration::from_secs(20), "the command did not start");
            std::thread::sleep(POLL);
        };
        // SAFETY: signal 0 only asks whether the process exists.
        assert_eq!(unsafe { libc::kill(pid, 0) }, 0, "the command is running");
        drop(client);
        // SAFETY: as above.
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(started.elapsed() < Duration::from_secs(20), "the command outlived its client");
            std::thread::sleep(POLL);
        }
    }
}

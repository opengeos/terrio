//! `terrio vsi` against a local S3-compatible server: listings (paged), uploads, downloads,
//! server-side copies, sync strategies, deletions bounded by prefix, and refusals.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Store {
    /// `bucket/key` -> (bytes, last modified as seconds since the epoch).
    objects: BTreeMap<String, (Vec<u8>, i64)>,
    /// Every request: method, path and query.
    log: Vec<String>,
    /// Whether every request carried a SigV4 `Authorization`.
    unsigned: usize,
    /// The time of writes: now, advancing with each write so that later writes are newer.
    clock: i64,
}

fn iso(secs: i64) -> String {
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.000Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(&s[i + 1..i + 3], 16).unwrap());
            i += 3;
        } else {
            out.push(if b[i] == b'+' { b' ' } else { b[i] });
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

/// Lists keys as S3's ListObjectsV2 does, two per page.
fn list(store: &Store, bucket: &str, query: &BTreeMap<String, String>) -> String {
    let prefix = query.get("prefix").cloned().unwrap_or_default();
    let delimiter = query.get("delimiter").cloned();
    let start: usize = query.get("continuation-token").map(|t| t.parse().unwrap()).unwrap_or(0);
    let mut items: Vec<(bool, String)> = Vec::new();
    for key in store.objects.keys().filter_map(|k| k.strip_prefix(&format!("{bucket}/"))) {
        let Some(rest) = key.strip_prefix(&prefix) else { continue };
        match delimiter.as_deref().and_then(|d| rest.find(d)) {
            Some(i) => {
                let p = format!("{prefix}{}/", &rest[..i]);
                if !items.contains(&(true, p.clone())) {
                    items.push((true, p));
                }
            }
            None => items.push((false, key.to_string())),
        }
    }
    let page: Vec<_> = items.iter().skip(start).take(2).collect();
    let truncated = start + 2 < items.len();
    let mut xml = String::from("<ListBucketResult>");
    for (dir, k) in page {
        if *dir {
            xml += &format!("<CommonPrefixes><Prefix>{k}</Prefix></CommonPrefixes>");
        } else {
            let (data, at) = &store.objects[&format!("{bucket}/{k}")];
            xml += &format!(
                "<Contents><Key>{k}</Key><LastModified>{}</LastModified><ETag>&quot;{}&quot;</ETag><Size>{}</Size></Contents>",
                iso(*at),
                terrio::vsi::md5_hex(data),
                data.len()
            );
        }
    }
    xml += &format!("<IsTruncated>{truncated}</IsTruncated>");
    if truncated {
        xml += &format!("<NextContinuationToken>{}</NextContinuationToken>", start + 2);
    }
    xml + "</ListBucketResult>"
}

fn serve() -> (String, Arc<Mutex<Store>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs() as i64;
    let store = Arc::new(Mutex::new(Store { clock: now, ..Default::default() }));
    let shared = store.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let store = shared.clone();
            std::thread::spawn(move || {
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut stream = stream;
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 {
                        return;
                    }
                    let mut parts = line.split_whitespace();
                    let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("/").to_string());
                    let mut headers = BTreeMap::new();
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                            break;
                        }
                        if let Some((k, v)) = h.split_once(':') {
                            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                        }
                    }
                    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
                    let mut body = vec![0u8; len];
                    reader.read_exact(&mut body).unwrap();
                    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                    let query: BTreeMap<String, String> = query
                        .split('&')
                        .filter(|p| !p.is_empty())
                        .map(|p| p.split_once('=').unwrap_or((p, "")))
                        .map(|(k, v)| (decode(k), decode(v)))
                        .collect();
                    let path = decode(path.trim_start_matches('/'));
                    let mut s = store.lock().unwrap();
                    s.log.push(format!(
                        "{method} /{path}{}",
                        if query.is_empty() { String::new() } else { format!("?{}", query.keys().cloned().collect::<Vec<_>>().join("&")) }
                    ));
                    if !headers.get("authorization").is_some_and(|a| a.starts_with("AWS4-HMAC-SHA256 Credential=TESTKEY/")) {
                        s.unsigned += 1;
                    }
                    let (bucket, key) = path.split_once('/').unwrap_or((&path, ""));
                    let (status, extra, out): (&str, String, Vec<u8>) = match method.as_str() {
                        "GET" if query.contains_key("list-type") => ("200 OK", String::new(), list(&s, bucket, &query).into_bytes()),
                        "GET" => match s.objects.get(&path) {
                            None => ("404 Not Found", String::new(), b"<Error><Code>NoSuchKey</Code></Error>".to_vec()),
                            Some((data, at)) => {
                                let modified = format!("Last-Modified: {}\r\nETag: \"{}\"\r\n", http_date(*at), terrio::vsi::md5_hex(data));
                                match headers.get("range").and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.split_once('-')) {
                                    Some((a, b)) if !data.is_empty() => {
                                        let a: usize = a.parse().unwrap();
                                        let b: usize = b.parse::<usize>().unwrap().min(data.len() - 1);
                                        let cr = format!("{modified}Content-Range: bytes {a}-{b}/{}\r\n", data.len());
                                        ("206 Partial Content", cr, data[a..=b].to_vec())
                                    }
                                    Some(_) => ("416 Range Not Satisfiable", format!("{modified}Content-Range: bytes */0\r\n"), Vec::new()),
                                    None => ("200 OK", modified, data.clone()),
                                }
                            }
                        },
                        "PUT" => {
                            s.clock += 10;
                            let at = s.clock;
                            let data = match headers.get("x-amz-copy-source") {
                                Some(src) => s.objects.get(&decode(src.trim_start_matches('/'))).map(|(d, _)| d.clone()),
                                None => Some(body),
                            };
                            match data {
                                Some(d) => {
                                    s.objects.insert(path.clone(), (d, at));
                                    ("200 OK", String::new(), b"<CopyObjectResult></CopyObjectResult>".to_vec())
                                }
                                None => ("404 Not Found", String::new(), Vec::new()),
                            }
                        }
                        "DELETE" => {
                            s.objects.remove(&path);
                            ("204 No Content", String::new(), Vec::new())
                        }
                        _ => ("405 Method Not Allowed", String::new(), Vec::new()),
                    };
                    let _ = key;
                    drop(s);
                    let head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n{extra}\r\n", out.len());
                    if stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&out)).is_err() {
                        return;
                    }
                }
            });
        }
    });
    (url, store)
}

fn http_date(secs: i64) -> String {
    let iso = iso(secs);
    let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let m: usize = iso[5..7].parse().unwrap();
    format!("Mon, {} {} {} {} GMT", &iso[8..10], months[m - 1], &iso[0..4], &iso[11..19])
}

fn terrio(endpoint: &str, dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio"))
        .args(args)
        .current_dir(dir)
        .env("AWS_S3_ENDPOINT", endpoint)
        .env("AWS_ACCESS_KEY_ID", "TESTKEY")
        .env("AWS_SECRET_ACCESS_KEY", "secret")
        .env("AWS_REGION", "us-east-1")
        .env_remove("AWS_PROFILE")
        .env_remove("AWS_NO_SIGN_REQUEST")
        .env("AWS_VIRTUAL_HOSTING", "FALSE")
        .output()
        .unwrap()
}

fn ok(endpoint: &str, dir: &Path, args: &[&str]) -> String {
    let out = terrio(endpoint, dir, args);
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn keys(store: &Mutex<Store>) -> Vec<String> {
    store.lock().unwrap().objects.keys().cloned().collect()
}

#[test]
fn copies_lists_syncs_and_deletes_on_s3() {
    let (endpoint, store) = serve();
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    std::fs::create_dir_all(d.join("t/a/b")).unwrap();
    std::fs::write(d.join("t/x.txt"), "hello\n").unwrap();
    std::fs::write(d.join("t/a/y.txt"), "abc\n").unwrap();
    std::fs::write(d.join("t/a/b/z.txt"), "z\n").unwrap();
    std::fs::write(d.join("empty.txt"), "").unwrap();
    // Siblings that a deletion of `bkt/dir` must not touch.
    store.lock().unwrap().objects.insert("bkt/dir2/keep.txt".into(), (b"k".to_vec(), 1));
    store.lock().unwrap().objects.insert("bkt/dirkeep".into(), (b"k".to_vec(), 1));

    // Upload a tree: into a new prefix, as GDAL does.
    ok(&endpoint, d, &["vsi", "copy", "-r", "t", "s3://bkt/dir", "-q"]);
    assert!(keys(&store).contains(&"bkt/dir/a/b/z.txt".to_string()), "{:?}", keys(&store));
    assert_eq!(store.lock().unwrap().unsigned, 0, "every request is signed");
    ok(&endpoint, d, &["vsi", "copy", "empty.txt", "/vsis3/bkt/dir/", "-q"]);

    // Listings, paged two keys at a time.
    let listing = ok(&endpoint, d, &["vsi", "list", "-R", "s3://bkt/dir"]);
    assert_eq!(listing.lines().collect::<Vec<_>>(), ["a", "a/b", "a/b/z.txt", "a/y.txt", "empty.txt", "x.txt"]);
    assert_eq!(ok(&endpoint, d, &["vsi", "ls", "/vsis3/bkt/dir/"]).lines().collect::<Vec<_>>(), ["a", "empty.txt", "x.txt"]);
    let long: serde_json::Value = serde_json::from_str(&ok(&endpoint, d, &["vsi", "list", "-l", "-f", "json", "s3://bkt/dir/a"])).unwrap();
    assert_eq!(long[1]["name"], "y.txt");
    assert_eq!(long[1]["size"], 4);
    assert_eq!(long[1]["etag"], terrio::vsi::md5_hex(b"abc\n"));
    assert!(long[1]["last_modification_date"].as_str().unwrap().ends_with('Z'));
    let one = ok(&endpoint, d, &["vsi", "list", "-l", "s3://bkt/dir/empty.txt"]);
    assert!(one.contains(" 0 ") && one.trim_end().ends_with("s3://bkt/dir/empty.txt"), "{one}");

    // Download the tree; a server-side copy within S3 (no PUT body through terrio).
    ok(&endpoint, d, &["vsi", "copy", "-r", "s3://bkt/dir", "down", "-q"]);
    assert_eq!(std::fs::read_to_string(d.join("down/a/b/z.txt")).unwrap(), "z\n");
    assert_eq!(std::fs::read(d.join("down/empty.txt")).unwrap(), b"");
    ok(&endpoint, d, &["vsi", "copy", "s3://bkt/dir/x.txt", "s3://bkt/copied.txt", "-q"]);
    assert_eq!(store.lock().unwrap().objects["bkt/copied.txt"].0, b"hello\n");

    // Sync: the second run copies nothing, by ETag or by timestamp; overwrite copies all.
    let sync = |strategy: &str| -> serde_json::Value {
        serde_json::from_str(&ok(&endpoint, d, &["vsi", "sync", "-r", "t/", "s3://bkt/synced", "--strategy", strategy, "--json"])).unwrap()
    };
    let first = sync("timestamp");
    assert_eq!(first["copied"], 3, "{first}");
    assert_eq!(sync("ETag")["unchanged"], 3);
    assert_eq!(sync("timestamp")["unchanged"], 3);
    assert_eq!(sync("overwrite")["copied"], 3);
    std::fs::write(d.join("t/a/y.txt"), "abcd\n").unwrap();
    let r = sync("ETag");
    assert_eq!((r["copied"].as_u64(), r["unchanged"].as_u64()), (Some(1), Some(2)));
    // And back down: up to date by ETag (local MD5).
    ok(&endpoint, d, &["vsi", "sync", "-r", "s3://bkt/synced/", "back", "-q"]);
    let r: serde_json::Value =
        serde_json::from_str(&ok(&endpoint, d, &["vsi", "sync", "-r", "s3://bkt/synced/", "back", "--strategy", "ETag", "--json"]))
            .unwrap();
    assert_eq!(r["unchanged"], 3);

    // Move: the source keys are gone once copied.
    ok(&endpoint, d, &["vsi", "move", "s3://bkt/synced", "s3://bkt/moved", "-q"]);
    assert!(!keys(&store).iter().any(|k| k.starts_with("bkt/synced/")));
    assert!(keys(&store).contains(&"bkt/moved/a/y.txt".to_string()));

    // Deletion: refused without -r for a prefix, then only below `dir/`.
    let refused = terrio(&endpoint, d, &["vsi", "delete", "s3://bkt/dir"]);
    assert!(!refused.status.success());
    assert!(keys(&store).contains(&"bkt/dir/x.txt".to_string()));
    let r: serde_json::Value = serde_json::from_str(&ok(&endpoint, d, &["vsi", "rm", "-r", "s3://bkt/dir", "--json"])).unwrap();
    assert_eq!(r["deleted"], 4);
    let left = keys(&store);
    assert!(!left.iter().any(|k| k.starts_with("bkt/dir/")), "{left:?}");
    assert!(left.contains(&"bkt/dir2/keep.txt".to_string()) && left.contains(&"bkt/dirkeep".to_string()), "{left:?}");
    for target in ["s3://bkt", "s3://bkt/", "/vsis3/bkt", ""] {
        let out = terrio(&endpoint, d, &["vsi", "delete", "-r", target]);
        assert!(!out.status.success(), "{target} was deleted");
    }
    assert!(left.len() == keys(&store).len());
    let missing = terrio(&endpoint, d, &["vsi", "delete", "s3://bkt/nothing-here"]);
    assert!(String::from_utf8_lossy(&missing.stderr).contains("does not exist"));

    // Measured requests: the directory check (an object GET, a one-key listing), then a listing of
    // 7 keys (6 files and the directory marker), 2 per page: 4 requests.
    store.lock().unwrap().log.clear();
    let stats = terrio(&endpoint, d, &["vsi", "list", "-R", "--stats", "s3://bkt/moved"]);
    let stderr = String::from_utf8_lossy(&stats.stderr);
    let log = store.lock().unwrap().log.clone();
    assert!(log.iter().all(|l| l.starts_with("GET")), "{log:?}");
    assert!(log.len() <= 6, "{log:?}");
    assert!(stderr.contains(&format!("{} HTTP request(s)", log.len())), "{stderr} vs {log:?}");
}

#[test]
fn refused_directory_operations_do_not_create_targets() {
    let (endpoint, _) = serve();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir(d.join("source")).unwrap();
    let out = terrio(&endpoint, d, &["vsi", "move", "source", "source/sub/"]);
    assert!(!out.status.success());
    assert!(!d.join("source/sub").exists());
    let out = terrio(&endpoint, d, &["vsi", "copy", "source", "new/"]);
    assert!(!out.status.success());
    assert!(!d.join("new").exists());
    std::fs::write(d.join("source/file"), b"x").unwrap();
    let text = ok(&endpoint, d, &["vsi", "list", "source", "--tree", "--abs"]);
    assert_eq!(text.trim(), d.join("source/file").to_str().unwrap());
}

#[test]
fn ordinary_html_with_links_is_copied_as_a_file() {
    let (endpoint, store) = serve();
    let data = b"<html><title>A page</title><a href='relative.html'>Read more</a></html>".to_vec();
    store.lock().unwrap().objects.insert("bkt/page.html".into(), (data.clone(), 1_700_000_000));
    store.lock().unwrap().objects.insert("bkt/page.html/".into(), (data.clone(), 1_700_000_000));
    let dir = tempfile::tempdir().unwrap();
    ok(&endpoint, dir.path(), &["vsi", "copy", &format!("{endpoint}/bkt/page.html"), "output"]);
    assert_eq!(std::fs::read(dir.path().join("output")).unwrap(), data);
}

#[test]
fn copies_large_http_binary_without_content_type() {
    let (endpoint, store) = serve();
    let data = vec![42; (5 << 20) + 7];
    store.lock().unwrap().objects.insert("bkt/binary".into(), (data.clone(), 1_700_000_000));
    store.lock().unwrap().objects.insert("bkt/binary/".into(), (data.clone(), 1_700_000_000));
    let dir = tempfile::tempdir().unwrap();
    let output = terrio(&endpoint, dir.path(), &["vsi", "copy", &format!("{endpoint}/bkt/binary"), "output", "--stats"]);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    // Index sniff (4096 bytes), one-byte stat, and one streamed payload copy.
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(&format!("3 HTTP request(s), {} byte(s) received", data.len() + 4097)),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(store.lock().unwrap().log.len(), 3);
    assert_eq!(std::fs::read(dir.path().join("output")).unwrap(), data);
}

#[test]
fn empty_s3_directories_and_absolute_trees() {
    let (endpoint, store) = serve();
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::create_dir(d.join("empty")).unwrap();
    ok(&endpoint, d, &["vsi", "copy", "empty", "s3://bkt/empty", "-r"]);
    assert!(keys(&store).contains(&"bkt/empty/".to_string()));
    let listing = ok(&endpoint, d, &["vsi", "list", "s3://bkt/empty", "-f", "json"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&listing).unwrap(), serde_json::json!([]));
    ok(&endpoint, d, &["vsi", "delete", "s3://bkt/empty", "-r"]);
    assert!(!keys(&store).contains(&"bkt/empty/".to_string()));
    std::fs::create_dir_all(d.join("root/sub")).unwrap();
    std::fs::write(d.join("root/sub/file"), b"x").unwrap();
    let tree = ok(&endpoint, d, &["vsi", "list", "root", "-R", "--tree", "--abs", "-f", "json"]);
    let v: serde_json::Value = serde_json::from_str(&tree).unwrap();
    assert_eq!(v[0]["name"], d.join("root/sub").to_str().unwrap());
    assert_eq!(v[0]["entries"][0], d.join("root/sub/file").to_str().unwrap());
    let file = ok(&endpoint, d, &["vsi", "list", "root/sub/file", "--tree", "--abs", "-f", "json"]);
    let v: serde_json::Value = serde_json::from_str(&file).unwrap();
    assert_eq!(v[0], d.join("root/sub/file").to_str().unwrap());
}

#[test]
fn refuses_dangerous_local_targets() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    std::fs::create_dir(d.join("sub")).unwrap();
    std::fs::write(d.join("sub/f.txt"), "f").unwrap();
    for target in ["/", ".", "..", "", " ", "/vsimem/"] {
        let out =
            Command::new(env!("CARGO_BIN_EXE_terrio")).args(["vsi", "delete", "-r", target]).current_dir(d.join("sub")).output().unwrap();
        assert!(!out.status.success(), "{target:?} was accepted");
    }
    assert!(d.join("sub/f.txt").exists());
    // The current directory given by its absolute path, and a parent.
    for target in [d.join("sub"), d.to_path_buf()] {
        let out = Command::new(env!("CARGO_BIN_EXE_terrio"))
            .args(["vsi", "delete", "-r", target.to_str().unwrap()])
            .current_dir(d.join("sub"))
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("refusing"), "{}", String::from_utf8_lossy(&out.stderr));
    }
    assert!(d.join("sub/f.txt").exists());
    // Archives and plain HTTP are read only.
    for target in ["/vsizip/a.zip/f", "https://example.com/f.tif"] {
        let out = Command::new(env!("CARGO_BIN_EXE_terrio")).args(["vsi", "delete", target]).current_dir(d).output().unwrap();
        assert!(!out.status.success());
    }
}

#[test]
fn bounded_depth_uses_delimiter_listings_only() {
    let (endpoint, store) = serve();
    let tmp = tempfile::tempdir().unwrap();
    for name in ["bkt/root/a/file.txt", "bkt/root/b/c/deep.txt", "bkt/root/c d/file.txt"] {
        store.lock().unwrap().objects.insert(name.into(), (b"data".to_vec(), 1));
    }
    let listed = ok(&endpoint, tmp.path(), &["vsi", "list", "-R", "--depth", "2", "s3://bkt/root"]);
    assert_eq!(listed.lines().collect::<Vec<_>>(), ["a", "a/file.txt", "b", "b/c", "c d", "c d/file.txt"]);
    let store = store.lock().unwrap();
    let requests: Vec<_> = store.log.iter().filter(|line| line.contains("list-type")).collect();
    assert!(!requests.is_empty());
    assert!(requests.iter().all(|line| line.contains("delimiter")), "{requests:?}");
}

#[test]
fn signed_file_tree_names_and_recursive_single_object_refusal() {
    let (endpoint, store) = serve();
    let tmp = tempfile::tempdir().unwrap();
    store.lock().unwrap().objects.insert("bkt/file".into(), (b"file bytes".to_vec(), 1));
    let url = format!("{endpoint}/bkt/file?sig=PRIVATE_TEST_SIGNATURE");
    let output = ok(&endpoint, tmp.path(), &["vsi", "list", &url, "--tree", "--abs", "-l", "-f", "json"]);
    let value: serde_json::Value = serde_json::from_str(&output).unwrap();
    assert_eq!(value[0]["name"], format!("{endpoint}/bkt/file?<redacted>"));
    assert!(!output.contains("PRIVATE_TEST_SIGNATURE"));
    let refused = terrio(&endpoint, tmp.path(), &["vsi", "delete", "-r", "s3://bkt/file"]);
    assert!(!refused.status.success());
    assert!(store.lock().unwrap().objects.contains_key("bkt/file"));
    let deleted = ok(&endpoint, tmp.path(), &["vsi", "delete", "--json", "s3://bkt/file"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&deleted).unwrap()["deleted"], 1);
    assert!(!store.lock().unwrap().objects.contains_key("bkt/file"));

    std::fs::create_dir(tmp.path().join("same")).unwrap();
    std::fs::write(tmp.path().join("same/same"), "child").unwrap();
    let one = ok(&endpoint, tmp.path(), &["vsi", "list", "same", "--tree", "--abs", "-f", "json"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&one).unwrap()[0], tmp.path().join("same/same").to_str().unwrap());
}

#[test]
fn recursive_delete_of_an_object_that_is_also_a_prefix_keeps_the_object() {
    let (endpoint, store) = serve();
    let tmp = tempfile::tempdir().unwrap();
    {
        let mut s = store.lock().unwrap();
        s.objects.insert("bkt/x".into(), (b"object".to_vec(), 1));
        s.objects.insert("bkt/x/child".into(), (b"child".to_vec(), 1));
    }
    // As GDAL 3.13.3 does (compare_vsi_with_gdal.py): `-r` removes the directory `x/`, and the
    // exact object `x` is deleted only without `-r`.
    let deleted = ok(&endpoint, tmp.path(), &["vsi", "delete", "-r", "--json", "s3://bkt/x"]);
    assert_eq!(serde_json::from_str::<serde_json::Value>(&deleted).unwrap()["deleted"], 1);
    {
        let s = store.lock().unwrap();
        assert!(s.objects.contains_key("bkt/x"));
        assert!(!s.objects.contains_key("bkt/x/child"));
    }
    ok(&endpoint, tmp.path(), &["vsi", "delete", "s3://bkt/x"]);
    assert!(!store.lock().unwrap().objects.contains_key("bkt/x"));
}

#[test]
fn missing_destination_directory_error_names_the_missing_directory() {
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    std::fs::write(&source, "keep source").unwrap();
    let destination = tmp.path().join("missing");
    let endpoint = "http://127.0.0.1:1";
    let out = terrio(endpoint, tmp.path(), &["vsi", "copy", source.to_str().unwrap(), &format!("{}/", destination.display())]);
    assert!(!out.status.success());
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(error.contains("destination directory") && error.contains("does not exist"));
    assert!(error.contains(destination.to_str().unwrap()));
    assert!(!destination.exists());
    assert_eq!(std::fs::read_to_string(source).unwrap(), "keep source");
}

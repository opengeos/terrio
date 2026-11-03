// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Local OAPIF fixture shared by native driver and CLI/MCP integration checks.
use serde_json::json;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
pub struct Server {
    pub url: String,
    pub requests: Arc<Mutex<Vec<(String, String)>>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    pub fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let record = requests.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let worker = std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(_) => break,
                };
                if done.load(Ordering::Relaxed) {
                    break;
                }
                stream.set_nonblocking(false).unwrap();
                stream.set_read_timeout(Some(std::time::Duration::from_secs(10))).unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut first = String::new();
                if reader.read_line(&mut first).unwrap_or(0) == 0 {
                    continue;
                }
                let target = first.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let mut accept = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                        break;
                    }
                    if let Some((key, value)) = line.split_once(':')
                        && key.eq_ignore_ascii_case("accept")
                    {
                        if !accept.is_empty() {
                            accept.push_str(", ");
                        }
                        accept.push_str(value.trim());
                    }
                }
                record.lock().unwrap().push((target.clone(), accept));
                if target == "/alias" {
                    write!(stream, "HTTP/1.1 307 Temporary Redirect\r\nLocation: /api/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                        .unwrap();
                    continue;
                }
                let path = target.split('?').next().unwrap();
                let body = match path {
                "/api/" => json!({"links":[{"rel":"data","href":"collections","type":"application/json"}]}),
                "/api/collections" => json!({"collections":[{"id":"places","title":"Places","links":[
                    {"rel":"items","href":"/api/collections/places/items","type":"application/geo+json"},
                    {"rel":"describedby","href":"/api/schema","type":"application/schema+json"}]}]}),
                "/api/schema" => json!({"type":"object","properties":{"name":{"type":"string"},"value":{"type":"number"},"missing":{"type":"integer"}}}),
                "/api/collections/places/items" => {
                    let parameter = |name| target.split_once('?').map(|(_,q)|q).unwrap_or("").split('&').find_map(|p|p.split_once('=').filter(|(k,_)|*k==name).map(|(_,v)|v.to_owned()));
                    let offset = parameter("offset").and_then(|s|s.parse::<usize>().ok()).unwrap_or(0);
                    let limit = parameter("limit").and_then(|s|s.parse::<usize>().ok()).unwrap_or(5);
                    let end = offset.saturating_add(limit).min(5);
                    let features: Vec<_> = (offset..end).map(|i|json!({"type":"Feature","id":i,"properties":{"name":format!("p{i}"),"value":i as f64+0.5},"geometry":{"type":"Point","coordinates":[i as f64+10.,i as f64+20.,3.]}})).collect();
                    let links = if end<5 {vec![json!({"rel":"next","href":format!("?offset={end}#page-{end}")})]}else{Vec::new()};
                    json!({"type":"FeatureCollection","numberMatched":5,"numberReturned":features.len(),"features":features,"links":links})
                },
                _ => json!({"error":"unexpected test route"}),
            }.to_string();
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        Self { url, requests, stop, worker: Some(worker) }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(w) = self.worker.take() {
            w.join().unwrap();
        }
    }
}

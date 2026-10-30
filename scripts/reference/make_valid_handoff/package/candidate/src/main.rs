//! Single-request JSON transport; no geometry algorithm belongs in this wrapper.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{Value, json};
use std::io::{self, Read};
use tile_repair_candidate::{IPt, Polygon};

fn request() -> Result<Value, String> {
    let mut text = String::new();
    io::stdin().take(16 * 1024 * 1024 + 1).read_to_string(&mut text).map_err(|e| e.to_string())?;
    if text.len() > 16 * 1024 * 1024 {
        return Err("transport request exceeds 16 MiB; use the direct Rust API for stress tests".into());
    }
    let request: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let polygons: Vec<Polygon<IPt>> =
        serde_json::from_value(request.get("polygons").cloned().ok_or("missing polygons")?).map_err(|e| e.to_string())?;
    let Some(repair) = tile_repair_candidate::implementation() else {
        return Ok(json!({"status": "unimplemented"}));
    };
    let output = repair(&polygons);
    if output.iter().flatten().flatten().any(|p| !p.0.is_finite() || !p.1.is_finite()) {
        return Err("candidate returned non-finite coordinates".into());
    }
    Ok(json!({"status": "ok", "polygons": output}))
}

fn main() {
    let response = match request() {
        Ok(response) => response,
        Err(message) => json!({"status": "error", "message": message}),
    };
    println!("{response}");
}

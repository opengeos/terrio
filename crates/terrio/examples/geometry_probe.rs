//! JSON-lines bridge for scripts/compare_jts_behavior.py. Inputs are WKB hex;
//! outputs are WKB hex (or STR feature IDs).
use serde_json::{Value, json};
use std::io::{self, BufRead};
use terrio::{
    Feature, Geometry, Layer,
    ops::{hull, simplify, vector_geom},
};

fn decode(hex: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    (0..hex.len()).step_by(2).map(|i| Ok(u8::from_str_radix(hex.get(i..i + 2).ok_or("odd WKB hex length")?, 16)?)).collect()
}
fn run(v: &Value) -> Result<Value, Box<dyn std::error::Error>> {
    let operation = v["operation"].as_str().ok_or("missing operation")?;
    let bytes = decode(v["input"].as_str().ok_or("missing WKB")?)?;
    let g = Geometry::from_wkb(&bytes)?;
    let ratio = v["ratio"].as_f64().unwrap_or(0.0);
    let holes = v["holes"].as_bool().unwrap_or(false);
    if operation == "strtree" {
        let Geometry::GeometryCollection(parts) = g else {
            return Err("STR input must be a collection".into());
        };
        let mut layer = Layer::new("probe");
        layer.features = parts.into_iter().enumerate().map(|(id, g)| Feature::with_geometry(id as u64, g, 0)).collect();
        return Ok(json!(vector_geom::sort(&layer, vector_geom::SortMethod::StrTree).features.iter().map(|f| f.fid).collect::<Vec<_>>()));
    }
    let result = match operation {
        "simplify" => simplify::simplify_preserve_topology(&g, ratio)?,
        "convex" => hull::convex_hull(&g)?,
        "concave" => hull::concave_hull(&g, ratio, holes)?,
        "polygon_hull" => hull::concave_hull_of_polygons(&g, ratio, v["tight"].as_bool().unwrap_or(false), holes)?,
        _ => return Err("unknown operation".into()),
    };
    Ok(json!(result.to_wkb().iter().map(|b| format!("{b:02x}")).collect::<String>()))
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    for line in io::stdin().lock().lines() {
        let value: Value = serde_json::from_str(&line?)?;
        let response = match run(&value) {
            Ok(output) => json!({"output":output}),
            Err(e) => json!({"error":e.to_string()}),
        };
        println!("{response}");
    }
    Ok(())
}

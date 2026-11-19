// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! The comparison of a PROJJSON with PROJ 9.9.0's over the #1066 corpus, shared by
//! `tests/projjson_corpus.rs` (terrio's PROJJSON writer) and the CRS model's unit test
//! (`src/model/oracle_tests.rs`, #1058 step 2). See `projjson_corpus.rs` for what is compared.

use std::collections::{BTreeMap, HashSet};
use std::f64::consts::PI;
use std::fmt::Write as _;
use std::io::Read;

use serde_json::Value;

/// Keys left out of the comparison: where and when a CRS is used, not what it is.
const LEFT_OUT: [&str; 7] = ["$schema", "usages", "scope", "area", "bbox", "remarks", "remark"];

/// A leaf of a normalised PROJJSON tree.
#[derive(Debug, Clone, PartialEq)]
enum Leaf {
    Number(f64),
    /// A value given with its unit, in metres, degrees or unity, and the unit's kind.
    Quantity(f64, String),
    Text(String),
    Bool(bool),
    Null,
}

impl Leaf {
    fn same(&self, other: &Leaf) -> bool {
        match (self, other) {
            (Leaf::Number(a) | Leaf::Quantity(a, _), Leaf::Number(b) | Leaf::Quantity(b, _)) => {
                // A unit's kind when both sides give one (a bare number is in the implied unit).
                let kinds = match (self, other) {
                    (Leaf::Quantity(_, x), Leaf::Quantity(_, y)) => x == y,
                    _ => true,
                };
                kinds && (a == b || (a - b).abs() <= 1e-9 * a.abs().max(b.abs()))
            }
            _ => self == other,
        }
    }
}

/// The factor of a unit to metres, degrees or unity, and its kind.
fn unit(u: &Value) -> Option<(f64, String)> {
    match u {
        Value::String(name) => match name.as_str() {
            "metre" => Some((1.0, "length".into())),
            "degree" => Some((1.0, "angle".into())),
            "unity" => Some((1.0, "scale".into())),
            _ => None,
        },
        Value::Object(o) => {
            let factor = o.get("conversion_factor")?.as_f64()?;
            match o.get("type")?.as_str()? {
                "LinearUnit" => Some((factor, "length".into())),
                "AngularUnit" => Some((factor * 180.0 / PI, "angle".into())),
                "ScaleUnit" => Some((factor, "scale".into())),
                other => Some((factor, other.to_string())),
            }
        }
        _ => None,
    }
}

/// The key of an array element in a path: its EPSG-style ID or its name, else its index. Axes and
/// the components of a compound CRS are kept by index, since their order matters.
fn element_key(field: &str, i: usize, v: &Value) -> String {
    if field != "axis" && field != "components" {
        if let Some(code) = v.get("id").and_then(|id| id.get("code")) {
            return format!("[#{}]", code.to_string().trim_matches('"'));
        }
        if let Some(name) = v.get("name").and_then(Value::as_str) {
            return format!("[{name}]");
        }
    }
    format!("[{i}]")
}

/// The leaves of a PROJJSON value by path, values with a unit in metres, degrees or unity.
fn flatten(v: &Value, path: &str, field: &str, out: &mut BTreeMap<String, Leaf>) {
    match v {
        Value::Object(o) => {
            // A value with its unit, `{"value": 3, "unit": "metre"}`, compared as the bare number in
            // metres, degrees or unity it is.
            if let (Some(value), Some(u)) = (o.get("value").and_then(Value::as_f64), o.get("unit"))
                && let Some((factor, kind)) = unit(u)
            {
                out.insert(path.to_string(), Leaf::Quantity(value * factor, kind));
                return;
            }
            let is_crs = o.get("type").and_then(Value::as_str).is_some_and(|t| t.ends_with("CRS"));
            for (k, x) in o {
                // Kept: the IDs of CRSs and methods; parameters are keyed by theirs. Left out: the
                // names of axes, conversions and parameters, axis abbreviations, ensemble members.
                let left_out = match k.as_str() {
                    "id" | "ids" => !is_crs && field != "method",
                    "name" => matches!(field, "axis" | "conversion" | "parameters"),
                    "abbreviation" | "members" | "accuracy" => true,
                    _ => LEFT_OUT.contains(&k.as_str()),
                };
                if left_out {
                    continue;
                }
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                // A unit alone (an axis's): its kind, and its factor as a number.
                if k == "unit"
                    && let Some((factor, kind)) = unit(x)
                {
                    out.insert(format!("{p}.factor"), Leaf::Number(factor));
                    out.insert(p, Leaf::Text(kind));
                    continue;
                }
                flatten(x, &p, k, out);
            }
        }
        Value::Array(a) => {
            for (i, x) in a.iter().enumerate() {
                flatten(x, &format!("{path}{}", element_key(field, i, x)), field, out);
            }
        }
        Value::Number(n) => {
            out.insert(path.to_string(), Leaf::Number(n.as_f64().unwrap()));
        }
        Value::String(s) => {
            out.insert(path.to_string(), Leaf::Text(s.clone()));
        }
        Value::Bool(b) => {
            out.insert(path.to_string(), Leaf::Bool(*b));
        }
        Value::Null => {
            out.insert(path.to_string(), Leaf::Null);
        }
    }
}

/// The paths at which a PROJJSON differs from PROJ's, joined by commas (empty when none).
pub fn compare(ours: &Value, proj: &Value) -> String {
    let (mut a, mut b) = (BTreeMap::new(), BTreeMap::new());
    flatten(ours, "", "", &mut a);
    flatten(proj, "", "", &mut b);
    let mut paths: Vec<&str> = a.keys().chain(b.keys()).map(String::as_str).collect();
    paths.sort_unstable();
    paths.dedup();
    paths.retain(|p| match (a.get(*p), b.get(*p)) {
        (Some(x), Some(y)) => !x.same(y),
        _ => true,
    });
    // A number that differs carries the decade of the difference (`~-3` for millimetres to
    // centimetres), so that a larger error at a path that already differs shows too.
    let decade = |p: &str| match (a.get(p), b.get(p)) {
        (Some(Leaf::Number(x) | Leaf::Quantity(x, _)), Some(Leaf::Number(y) | Leaf::Quantity(y, _))) if x != y => {
            format!("{p}~{}", (x - y).abs().log10().floor())
        }
        _ => p.to_string(),
    };
    paths.into_iter().map(decade).collect::<Vec<_>>().join(",")
}

/// The corpus cases, `(code, form, text, PROJ's PROJJSON)`, from its zstd-compressed JSON lines.
pub fn corpus(parts: &[&[u8]]) -> Vec<(String, String, String, Value)> {
    let mut text = String::new();
    for mut bytes in parts.iter().copied() {
        ruzstd::decoding::StreamingDecoder::new(&mut bytes).unwrap().read_to_string(&mut text).unwrap();
    }
    text.lines()
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            (v[0].as_str().unwrap().to_string(), v[1].as_str().unwrap().to_string(), v[2].as_str().unwrap().to_string(), v[3].clone())
        })
        .collect()
}

/// The differences of every case (`differences(text, proj)`), grouped: one line per set of
/// differing paths and form, with the CRSs that have it.
pub fn grouped(corpus: &[(String, String, String, Value)], differences: fn(&str, &Value) -> String) -> String {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4));
    let chunk = corpus.len().div_ceil(threads);
    let found: Vec<(String, String, String)> = std::thread::scope(|s| {
        let handles: Vec<_> = corpus
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter().map(|(code, form, text, proj)| (differences(text, proj), form.clone(), code.clone())).collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    let mut groups: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for (d, form, code) in found {
        if !d.is_empty() {
            groups.entry((d, form)).or_default().push(code);
        }
    }
    let mut out = String::new();
    for ((d, form), codes) in groups {
        writeln!(out, "{form}\t{d}\t{}", codes.join(" ")).unwrap();
    }
    out
}

/// Compares the grouped differences with those recorded in `path`, or records them with
/// `TERRIO_RECORD=1`; on a change, panics with the CRSs it moves.
pub fn check_recorded(path: &str, found: &str) {
    if std::env::var("TERRIO_RECORD").as_deref() == Ok("1") {
        std::fs::write(path, found).unwrap();
        return;
    }
    // Line by line, as a checkout may end lines with CRLF.
    let recorded = std::fs::read_to_string(path).unwrap_or_default().replace("\r\n", "\n");
    if recorded != found {
        // Per CRS and form, so that a change shows which CRSs it moves.
        let split = |text: &str| -> HashSet<String> {
            text.lines()
                .flat_map(|l| {
                    let mut f = l.splitn(3, '\t');
                    let (form, d, codes) = (f.next().unwrap(), f.next().unwrap(), f.next().unwrap_or(""));
                    codes.split(' ').map(move |c| format!("{c}\t{form}\t{d}")).collect::<Vec<_>>()
                })
                .collect()
        };
        let (old, new) = (split(&recorded), split(found));
        let mut gone: Vec<_> = old.difference(&new).collect();
        let mut came: Vec<_> = new.difference(&old).collect();
        gone.sort_unstable();
        came.sort_unstable();
        panic!(
            "differences from PROJ 9.9.0 changed: {} recorded lines gone, {} new (record a deliberate change with TERRIO_RECORD=1)\nno longer:\n{}\nnew:\n{}",
            gone.len(),
            came.len(),
            gone.iter().take(20).map(|l| l.as_str()).collect::<Vec<_>>().join("\n"),
            came.iter().take(20).map(|l| l.as_str()).collect::<Vec<_>>().join("\n")
        );
    }
}

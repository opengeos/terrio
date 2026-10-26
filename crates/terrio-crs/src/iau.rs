//! IAU_2015 definitions from PROJ 9.8.1; see data/iau-source.json and NOTICE.
use std::collections::HashMap;
use std::io::Read;
use std::sync::OnceLock;

use crate::{Error, Result, wkt1};

pub(crate) struct Entry {
    pub body: u32,
    pub base: u32,
    pub datum: u32,
    pub geographic: bool,
    pub ocentric: bool,
    pub proj: String,
    pub wkt: String,
    normalized_wkt: OnceLock<Option<wkt1::Node>>,
}

pub(crate) fn get(code: u32) -> Option<&'static Entry> {
    static TABLE: OnceLock<HashMap<u32, Entry>> = OnceLock::new();
    TABLE
        .get_or_init(|| {
            let mut bytes = &include_bytes!("../data/iau.tsv.zst")[..];
            let mut decoder = ruzstd::decoding::StreamingDecoder::new(&mut bytes).expect("bundled IAU table is zstd");
            let mut text = String::new();
            decoder.read_to_string(&mut text).expect("bundled IAU table is UTF-8");
            text.lines()
                .map(|line| {
                    let columns: Vec<_> = line.splitn(9, '\t').collect();
                    assert_eq!(columns.len(), 9, "bundled IAU row");
                    let number = |index: usize| columns[index].parse().expect("bundled IAU numeric code");
                    (
                        number(0),
                        Entry {
                            body: number(1),
                            base: number(2),
                            datum: number(7),
                            geographic: columns[3] != "p",
                            ocentric: columns[3] == "c",
                            proj: columns[5].to_string(),
                            wkt: columns[8].to_string(),
                            normalized_wkt: OnceLock::new(),
                        },
                    )
                })
                .collect()
        })
        .get(&code)
}

fn bad() -> Error {
    Error::Crs("invalid IAU reference; supported registry is IAU_2015 (version 2015)".into())
}

/// Recognizes authority, versioned OGC URN and URL forms without guessing versions.
pub(crate) fn reference(text: &str) -> Option<Result<u32>> {
    let upper = text.trim().to_ascii_uppercase();
    let parse = |authority: &str, version: &str, code: &str| {
        let supported = match authority {
            "IAU_2015" => version.is_empty(),
            "IAU" => version == "2015",
            _ => false,
        };
        if !supported || code.is_empty() || !code.bytes().all(|c| c.is_ascii_digit()) {
            return Err(bad());
        }
        code.parse().map_err(|_| bad())
    };
    if let Some(rest) = upper.strip_prefix("URN:OGC:DEF:CRS:") {
        let mut parts = rest.split(':');
        let authority = parts.next()?;
        if authority == "IAU" || authority.starts_with("IAU_") {
            let (Some(version), Some(code), None) = (parts.next(), parts.next(), parts.next()) else { return Some(Err(bad())) };
            return Some(parse(authority, version, code));
        }
    }
    for prefix in [
        "HTTP://WWW.OPENGIS.NET/DEF/CRS/",
        "HTTPS://WWW.OPENGIS.NET/DEF/CRS/",
        "HTTP://OPENGIS.NET/DEF/CRS/",
        "HTTPS://OPENGIS.NET/DEF/CRS/",
    ] {
        if let Some(rest) = upper.strip_prefix(prefix) {
            let mut parts = rest.split('/');
            let authority = parts.next()?;
            if authority == "IAU" || authority.starts_with("IAU_") {
                let (Some(version), Some(code), None) = (parts.next(), parts.next(), parts.next()) else { return Some(Err(bad())) };
                return Some(parse(authority, version, code));
            }
        }
    }
    let (authority, rest) = upper.split_once(':')?;
    if authority == "IAU" || authority.starts_with("IAU_") {
        if authority == "IAU" {
            let (version, code) = rest.split_once(':').unwrap_or(("2015", rest));
            return Some(parse(authority, version, code));
        }
        return Some(parse(authority, "", rest));
    }
    None
}

/// An explicit IAU identifier of an outermost WKT node.
pub(crate) fn declared(wkt: &str) -> Option<u32> {
    if !wkt.to_ascii_uppercase().contains("\"IAU") {
        return None;
    }
    let root = wkt1::parse(wkt)?;
    declared_node(&root)
}

fn declared_node(root: &wkt1::Node) -> Option<u32> {
    let id =
        root.children("ID").find(|id| id.name().is_some_and(|a| a.eq_ignore_ascii_case("IAU") || a.eq_ignore_ascii_case("IAU_2015")))?;
    let authority = id.name()?;
    let code = match id.args.get(1)? {
        wkt1::Arg::Num(n) if *n >= 0.0 && *n <= u32::MAX as f64 && n.fract() == 0.0 => *n as u32,
        wkt1::Arg::Str(s) => s.parse().ok()?,
        _ => return None,
    };
    let version = match id.args.get(2) {
        Some(wkt1::Arg::Num(n)) => n.to_string(),
        Some(wkt1::Arg::Str(s)) => s.clone(),
        None => String::new(),
        _ => return None,
    };
    let valid = if authority.eq_ignore_ascii_case("IAU_2015") { version.is_empty() } else { version == "2015" };
    valid.then_some(code)
}

/// Identifies a registered definition, rather than trusting a caller-supplied root ID.
pub(crate) fn identify(wkt: &str, proj: Option<&str>) -> Option<u32> {
    if !wkt.to_ascii_uppercase().contains("\"IAU") {
        return None;
    }
    let mut root = wkt1::parse(wkt)?;
    let code = declared_node(&root)?;
    let entry = get(code)?;
    if proj.is_some_and(|p| p != entry.proj) {
        return None;
    }
    if wkt == entry.wkt {
        return Some(code);
    }
    let remove_root_id = |root: &mut wkt1::Node| {
        root.args.retain(|arg| !matches!(arg, wkt1::Arg::Node(node) if node.key.eq_ignore_ascii_case("ID")));
    };
    remove_root_id(&mut root);
    let canonical = entry.normalized_wkt.get_or_init(|| {
        let mut root = wkt1::parse(&entry.wkt)?;
        remove_root_id(&mut root);
        Some(root)
    });
    (Some(&root) == canonical.as_ref()).then_some(code)
}

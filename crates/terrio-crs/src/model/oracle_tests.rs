// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! The model against PROJ 9.9.0, through PROJJSON: every text of the #1066 corpus (each EPSG and
//! ESRI CRS's WKT 2, GDAL WKT 1 and ESRI WKT 1, as PROJ writes them) is read by [`super::read`],
//! rendered as PROJJSON by [`render`], and compared field by field with the PROJJSON PROJ writes
//! for the same text, as `tests/projjson_corpus.rs` compares terrio's writer
//! (`tests/support/projjson_compare.rs`). The differences are recorded in
//! `tests/data/projjson_model_diffs.tsv` (one file: the model does not depend on
//! `wbprojection-fallback`); re-record a deliberate change with `TERRIO_RECORD=1`.
//!
//! The rendering adds nothing PROJ takes from its database. It names WKT 1 methods and parameters
//! as EPSG does by the table GDAL's PROJJSON writer already has (`projjson_gdal::method1`), so
//! that the comparison measures what the model reads rather than how it names. CRS, datum and
//! ellipsoid names stay as written (ESRI's `GCS_` and `D_` names, GDAL's underscores): PROJ maps
//! those through its database, which is #1058 step 4.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use super::*;

#[path = "../../tests/support/projjson_compare.rs"]
#[allow(dead_code)]
mod projjson_compare;

use projjson_compare::{check_recorded, compare, corpus, grouped};

const DIFFS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/projjson_model_diffs.tsv");

fn id_json(i: &Id) -> Value {
    let code = i.code.parse::<u64>().map_or_else(|_| json!(i.code), |c| json!(c));
    let mut v = json!({"authority": i.authority, "code": code});
    if let Some(version) = &i.version {
        v["version"] = json!(version.parse::<f64>().map_or_else(|_| json!(version), |n| json!(n)));
    }
    v
}

/// An object's `id` (one identifier) or `ids` (several), as PROJ writes them.
fn with_ids(o: &mut Map<String, Value>, ident: &Ident) {
    match ident.ids.as_slice() {
        [] => {}
        [one] => {
            o.insert("id".into(), id_json(one));
        }
        many => {
            o.insert("ids".into(), Value::Array(many.iter().map(id_json).collect()));
        }
    }
}

fn unit_json(u: &Unit) -> Value {
    let kind = match u.kind {
        UnitKind::Length => "LinearUnit",
        UnitKind::Angle => "AngularUnit",
        UnitKind::Scale => "ScaleUnit",
        UnitKind::Time => "TimeUnit",
        UnitKind::Unknown => "Unit",
    };
    json!({"type": kind, "name": u.name, "conversion_factor": u.factor})
}

fn cs_json(cs: &Cs) -> Value {
    let subtype = match &cs.kind {
        CsKind::Ellipsoidal => "ellipsoidal".to_string(),
        CsKind::Cartesian => "Cartesian".to_string(),
        CsKind::Spherical => "spherical".to_string(),
        CsKind::Vertical => "vertical".to_string(),
        CsKind::Other(k) => k.clone(),
    };
    let axes: Vec<Value> = cs
        .axes
        .iter()
        .map(|a| {
            let mut v = json!({"name": a.name, "direction": a.direction});
            if let Some(u) = &a.unit {
                v["unit"] = unit_json(u);
            }
            if let Some(m) = a.meridian {
                v["meridian"] = json!({"longitude": {"value": m, "unit": "degree"}});
            }
            v
        })
        .collect();
    json!({"subtype": subtype, "axis": axes})
}

fn ellipsoid_json(e: &Ellipsoid) -> Value {
    if e.rf == 0.0 {
        json!({"name": e.ident.name, "radius": e.a})
    } else {
        json!({"name": e.ident.name, "semi_major_axis": e.a, "inverse_flattening": e.rf})
    }
}

fn geodetic_json(ident: &Ident, datum: &Datum, cs: &Cs, frame_epoch: Option<f64>) -> Value {
    let mut o = Map::new();
    let kind = if cs.kind == CsKind::Ellipsoidal { "GeographicCRS" } else { "GeodeticCRS" };
    o.insert("type".into(), json!(kind));
    o.insert("name".into(), json!(ident.name));
    let pm = datum.prime_meridian();
    let mut d = Map::new();
    match datum {
        Datum::Frame { ident, anchor_epoch, .. } => {
            d.insert("type".into(), json!(if frame_epoch.is_some() { "DynamicGeodeticReferenceFrame" } else { "GeodeticReferenceFrame" }));
            d.insert("name".into(), json!(ident.name));
            if let Some(e) = frame_epoch {
                d.insert("frame_reference_epoch".into(), json!(e));
            }
            if let Some(e) = anchor_epoch {
                d.insert("anchor_epoch".into(), json!(e));
            }
        }
        Datum::Ensemble { ident, .. } => {
            d.insert("name".into(), json!(ident.name));
        }
    }
    d.insert("ellipsoid".into(), ellipsoid_json(datum.ellipsoid()));
    // Greenwich is left out, as PROJ leaves it.
    if pm.longitude != 0.0 {
        d.insert("prime_meridian".into(), json!({"name": pm.ident.name, "longitude": {"value": pm.longitude, "unit": "degree"}}));
    }
    let key = if matches!(datum, Datum::Ensemble { .. }) { "datum_ensemble" } else { "datum" };
    o.insert(key.into(), Value::Object(d));
    o.insert("coordinate_system".into(), cs_json(cs));
    with_ids(&mut o, ident);
    Value::Object(o)
}

/// A WKT 1 parameter's value in degrees, metres or unity, by its kind.
fn value_in(p: &Param, kind: char, c: &Conversion) -> f64 {
    let u = p.unit.clone().unwrap_or_else(|| match kind {
        'a' => c.angular.clone(),
        'l' => c.linear.clone(),
        _ => Unit::unity(),
    });
    if u.kind == UnitKind::Angle || (kind == 'a' && u.kind == UnitKind::Unknown) { p.value * u.degrees() } else { p.value * u.factor }
}

fn conversion_json(c: &Conversion) -> Value {
    let quantity = |v: f64, kind: char| {
        let unit = match kind {
            'a' => "degree",
            'l' => "metre",
            _ => "unity",
        };
        json!({"value": v, "unit": unit})
    };
    // A method PROJ knows by an EPSG ID or name (WKT 2), or GDAL's WKT 1 name.
    let wkt1 = c.method.ids.is_empty() && c.params.iter().all(|p| p.ident.ids.is_empty() && p.unit.is_none());
    let by_name = |names: &[&str]| names.iter().find_map(|n| c.params.iter().find(|p| wkt1::norm(&p.ident.name) == *n).map(|p| p.value));
    if wkt1 && let Some((name, code, list)) = crate::projjson_gdal::method1(&c.method.name, &by_name) {
        let parameters: Vec<Value> = list
            .iter()
            .map(|&(pname, pcode, kind)| {
                // By GDAL's WKT 1 names, else by EPSG's (ESRI writes some so).
                let names = crate::projjson_gdal::wkt1_names(pcode);
                let found = names
                    .iter()
                    .find_map(|n| c.params.iter().find(|p| wkt1::norm(&p.ident.name) == *n))
                    .or_else(|| c.params.iter().find(|p| wkt1::norm(&p.ident.name) == wkt1::norm(pname)));
                // Without its rectified grid angle, Hotine's is its azimuth, as PROJ reads ESRI's.
                let azimuth = || c.params.iter().find(|p| wkt1::norm(&p.ident.name) == "azimuth").map(|p| value_in(p, 'a', c));
                let absent = if pcode == 8814 {
                    azimuth().unwrap_or(0.0)
                } else if kind == 's' {
                    1.0
                } else {
                    0.0
                };
                let v = found.map_or(absent, |p| value_in(p, kind, c));
                let mut q = quantity(v, kind);
                q["name"] = json!(pname);
                q["id"] = json!({"authority": "EPSG", "code": pcode});
                q
            })
            .collect();
        let mut m = json!({"name": name});
        if code != 0 {
            m["id"] = json!({"authority": "EPSG", "code": code});
        }
        return json!({"method": m, "parameters": parameters});
    }
    let mut m = Map::new();
    m.insert("name".into(), json!(c.method.name));
    with_ids(&mut m, &c.method);
    let parameters: Vec<Value> = c
        .params
        .iter()
        .map(|p| {
            let code = p.ident.ids.iter().find(|i| i.authority.eq_ignore_ascii_case("EPSG")).and_then(|i| i.code.parse().ok());
            // The kind of its own unit (EPSG gives some scale factors in metres, which PROJ keeps),
            // else of the parameter.
            let own = p.unit.as_ref().and_then(|u| match u.kind {
                UnitKind::Angle => Some('a'),
                UnitKind::Length => Some('l'),
                UnitKind::Scale => Some('s'),
                _ => None,
            });
            let kind = own.or_else(|| {
                crate::wkt2::parameter_info(&p.ident.name, code).map(|(_, k)| match k {
                    "ANGLEUNIT" => 'a',
                    "LENGTHUNIT" => 'l',
                    _ => 's',
                })
            });
            let mut o = match kind {
                Some(k) => quantity(value_in(p, k, c), k),
                None => json!({"value": p.value}),
            };
            o["name"] = json!(p.ident.name);
            let mut extra = Map::new();
            with_ids(&mut extra, &p.ident);
            for (k, v) in extra {
                o[k] = v;
            }
            o
        })
        .collect();
    json!({"method": Value::Object(m), "parameters": parameters})
}

/// The model as PROJJSON, in PROJ 9.9.0's shape.
fn render(crs: &Crs) -> Value {
    match crs {
        Crs::Geodetic { ident, datum, cs, frame_epoch } => geodetic_json(ident, datum, cs, *frame_epoch),
        Crs::Projected { ident, base, conversion, cs } => {
            let mut o = Map::new();
            o.insert("type".into(), json!("ProjectedCRS"));
            o.insert("name".into(), json!(ident.name));
            o.insert("base_crs".into(), render(base));
            o.insert("conversion".into(), conversion_json(conversion));
            o.insert("coordinate_system".into(), cs_json(cs));
            with_ids(&mut o, ident);
            Value::Object(o)
        }
        Crs::Vertical { ident, datum, ensemble, anchor_epoch, frame_epoch, cs, .. } => {
            let mut o = Map::new();
            o.insert("type".into(), json!("VerticalCRS"));
            o.insert("name".into(), json!(ident.name));
            if *ensemble {
                o.insert("datum_ensemble".into(), json!({"name": datum.name}));
            } else {
                let kind = if frame_epoch.is_some() { "DynamicVerticalReferenceFrame" } else { "VerticalReferenceFrame" };
                let mut d = json!({"type": kind, "name": datum.name});
                if let Some(e) = frame_epoch {
                    d["frame_reference_epoch"] = json!(e);
                }
                if let Some(e) = anchor_epoch {
                    d["anchor_epoch"] = json!(e);
                }
                o.insert("datum".into(), d);
            }
            o.insert("coordinate_system".into(), cs_json(cs));
            with_ids(&mut o, ident);
            Value::Object(o)
        }
        Crs::Compound { ident, parts } => {
            let mut o = Map::new();
            o.insert("type".into(), json!("CompoundCRS"));
            o.insert("name".into(), json!(ident.name));
            o.insert("components".into(), Value::Array(parts.iter().map(render).collect()));
            with_ids(&mut o, ident);
            Value::Object(o)
        }
        Crs::Bound { source, transformation: t } => {
            let c = Conversion {
                ident: t.ident.clone(),
                method: t.method.clone(),
                params: t.params.clone(),
                angular: Unit::degree(),
                linear: Unit::metre(),
            };
            let mut tr = conversion_json(&c);
            tr["name"] = json!(t.ident.name);
            json!({"type": "BoundCRS", "source_crs": render(source), "target_crs": render(&t.target), "transformation": tr})
        }
        Crs::Other { ident, .. } => {
            let mut o = Map::new();
            o.insert("type".into(), json!("EngineeringCRS"));
            o.insert("name".into(), json!(ident.name));
            with_ids(&mut o, ident);
            Value::Object(o)
        }
    }
}

/// The paths at which the model's PROJJSON differs from PROJ's, or `!` when it reads none.
fn differences(text: &str, proj: &Value) -> String {
    match read(text) {
        Some(m) => compare(&render(&m.crs), proj),
        None => "!unread".into(),
    }
}

/// The paths at which terrio's PROJJSON writer differs from PROJ's, as `tests/projjson_corpus.rs`
/// records them, `None` when it writes none.
fn writer_differences(text: &str, proj: &Value) -> Option<BTreeMap<String, Option<i32>>> {
    let json = crate::SpatialRef::from_wkt(text).to_projjson().ok()?;
    let ours: Value = serde_json::from_str(&json).ok()?;
    Some(paths(&compare(&ours, proj)))
}

/// The paths of a list of differences, each with the decade of a number's difference.
fn paths(d: &str) -> BTreeMap<String, Option<i32>> {
    d.split(',')
        .filter(|p| !p.is_empty())
        .map(|p| match p.rsplit_once('~') {
            Some((path, decade)) => (path.to_string(), decade.parse().ok()),
            None => (p.to_string(), None),
        })
        .collect()
}

/// Whether a CRS has a WKT 1 conversion whose method GDAL's table (`projjson_gdal::method1`) does
/// not name: ESRI's `Lambert_Conformal_Conic` or `Gauss_Kruger`, which #1058 step 5a's method table
/// will. Its conversion is rendered as written, so not compared.
fn unnamed_method(crs: &Crs) -> bool {
    match crs {
        Crs::Projected { conversion: c, .. } => {
            let wkt1 = c.method.ids.is_empty() && c.params.iter().all(|p| p.ident.ids.is_empty() && p.unit.is_none());
            wkt1 && crate::projjson_gdal::method1(&c.method.name, &|_| None).is_none()
        }
        Crs::Compound { parts, .. } => parts.iter().any(unnamed_method),
        Crs::Bound { source, .. } => unnamed_method(source),
        _ => false,
    }
}

/// The paths of the coordinate systems whose axes PROJ takes from its database (#1058 step 4): a
/// geographic CRS without axes that names a code (`AUTHORITY`, a WKT 2 base CRS's `ID`: PROJ gives
/// it that CRS's axes, EPSG:27500's base CRS in grads) or that ESRI names (ESRI's dialect: PROJ
/// finds the CRS by its name).
fn database_axes(crs: &Crs, prefix: &str, esri: bool, out: &mut Vec<String>) {
    let at = |key: &str| if prefix.is_empty() { key.to_string() } else { format!("{prefix}.{key}") };
    match crs {
        Crs::Geodetic { cs, ident, .. } if !cs.explicit && cs.kind == CsKind::Ellipsoidal && (esri || !ident.ids.is_empty()) => {
            out.push(at("coordinate_system"))
        }
        Crs::Projected { base, .. } => database_axes(base, &at("base_crs"), esri, out),
        Crs::Compound { parts, .. } => {
            for (i, p) in parts.iter().enumerate() {
                database_axes(p, &format!("{}[{i}]", at("components")), esri, out);
            }
        }
        _ => {}
    }
}

/// The prefixes of the CRSs whose datum ensemble PROJ replaces by a datum from its database
/// (EPSG:3857's, 4855's base CRS): PROJ's PROJJSON has a `datum` where the model's has a
/// `datum_ensemble`.
fn substituted_ensembles(model: &BTreeMap<String, Option<i32>>) -> Vec<String> {
    model
        .keys()
        .filter_map(|p| p.strip_suffix(".datum.type"))
        .filter(|prefix| model.keys().any(|q| q.starts_with(&format!("{prefix}.datum_ensemble."))))
        .map(str::to_string)
        .collect()
}

/// Whether a path is of a name or identifier, which PROJ maps or adds through its database.
fn naming(path: &str) -> bool {
    path.split('.').any(|s| s == "name" || s.starts_with("name[") || s == "id" || s.starts_with("ids") || s == "authority" || s == "code")
}

fn the_corpus() -> Vec<(String, String, String, Value)> {
    corpus(&[
        &include_bytes!("../../tests/data/projjson_corpus_epsg.jsonl.zst")[..],
        &include_bytes!("../../tests/data/projjson_corpus_esri.jsonl.zst")[..],
    ])
}

#[test]
fn every_crs_reads_into_the_model_as_recorded_against_proj() {
    check_recorded(DIFFS, &grouped(&the_corpus(), differences));
}

/// The model reads every value at least as terrio's PROJJSON writer does: where the writer's
/// PROJJSON has PROJ's value at a path, the model's has it too, and where both differ from PROJ's
/// by a number, the model's is not further off. Left out, as the model leaves them to later steps:
/// names and identifiers, which PROJ maps or adds through its database; the axes PROJ takes from its
/// database (`database_axes`) and a datum ensemble it replaces (`substituted_ensembles`); the
/// conversion of a WKT 1 whose method GDAL's table does not name (its parameters are then keyed by
/// name, PROJ's by code, so no value compares), of a WKT 1 with a PROJ4 extension (PROJ computes
/// from the extension), and of a Mercator PROJ reads as Pseudo Mercator. A text the writer writes
/// no PROJJSON for has nothing to compare.
#[test]
fn the_model_reads_no_value_worse_than_the_writer() {
    let corpus = the_corpus();
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(4));
    let chunk = corpus.len().div_ceil(threads);
    let worse: Vec<String> = std::thread::scope(|s| {
        let handles: Vec<_> = corpus
            .chunks(chunk)
            .map(|part| {
                s.spawn(move || {
                    part.iter()
                        .filter_map(|(code, form, text, proj)| {
                            let writer = writer_differences(text, proj)?;
                            let model = paths(&differences(text, proj));
                            let m = read(text);
                            let unnamed = m.as_ref().is_some_and(|m| unnamed_method(&m.crs));
                            let extended = text.contains("\"PROJ4\"");
                            let pseudo_mercator = proj["conversion"]["method"]["id"]["code"] == 1024;
                            let mut implicit = Vec::new();
                            if let Some(m) = &m {
                                database_axes(&m.crs, "", m.dialect == Dialect::Wkt1Esri, &mut implicit);
                            }
                            let substituted = substituted_ensembles(&model);
                            let worse: Vec<&String> = model
                                .iter()
                                .filter(|(p, decade)| match writer.get(*p) {
                                    // Both off: worse only by a larger decade.
                                    Some(w) => matches!((decade, w), (Some(a), Some(b)) if a > b),
                                    None => true,
                                })
                                .map(|(p, _)| p)
                                .filter(|p| {
                                    !naming(p)
                                        && !((unnamed || extended || pseudo_mercator) && p.contains("conversion"))
                                        && !implicit.iter().any(|c| p.starts_with(&format!("{c}.axis")) && p.ends_with(".direction"))
                                        && !substituted.iter().any(|c| p.starts_with(&format!("{c}.datum")))
                                })
                                .collect();
                            (!worse.is_empty()).then(|| format!("{code} {form}: {worse:?}"))
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap()).collect()
    });
    assert!(worse.is_empty(), "{} cases read worse than the writer:\n{}", worse.len(), worse.join("\n"));
}

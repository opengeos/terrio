//! Strict PROJJSON v0.7 subset translated to the shared WKT2 reader.
use serde_json::Value;

use crate::{Def, Error, Result, SpatialRef};

type Object = serde_json::Map<String, Value>;
fn bad(message: impl std::fmt::Display) -> Error {
    Error::Crs(format!("unsupported PROJJSON: {message}"))
}
fn object<'a>(value: &'a Value, allowed: &[&str]) -> Result<&'a Object> {
    let o = value.as_object().ok_or_else(|| bad("expected an object"))?;
    for k in o.keys() {
        if !allowed.contains(&k.as_str())
            && !["$schema", "type", "id", "ids", "scope", "area", "bbox", "usages", "remarks"].contains(&k.as_str())
        {
            return Err(bad(format!("unsupported member '{k}'")));
        }
    }
    if let Some(schema) = o.get("$schema")
        && schema.as_str() != Some("https://proj.org/schemas/v0.7/projjson.schema.json")
    {
        return Err(bad("only schema v0.7 is supported"));
    }
    Ok(o)
}
fn required<'a>(o: &'a Object, key: &str) -> Result<&'a Value> {
    o.get(key).ok_or_else(|| bad(format!("missing '{key}'")))
}
fn string<'a>(o: &'a Object, key: &str) -> Result<&'a str> {
    required(o, key)?.as_str().ok_or_else(|| bad(format!("'{key}' must be a string")))
}
fn number(value: &Value) -> Result<f64> {
    value.as_f64().filter(|n| n.is_finite()).ok_or_else(|| bad("expected a finite number"))
}
fn q(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}
fn node(key: &str, args: &[String]) -> String {
    format!("{key}[{}]", args.join(","))
}
fn named(key: &str, name: &str, args: Vec<String>) -> String {
    let mut out = vec![q(name)];
    out.extend(args);
    node(key, &out)
}
fn typed(o: &Object, kind: &str) -> Result<()> {
    if o.get("type").is_some_and(|t| t.as_str() != Some(kind)) {
        return Err(bad(format!("expected type {kind}")));
    }
    Ok(())
}
fn id(o: &Object, args: &mut Vec<String>) -> Result<()> {
    if o.contains_key("id") && o.contains_key("ids") {
        return Err(bad("id and ids are mutually exclusive"));
    }
    let mut ids: Vec<&Value> = match (o.get("id"), o.get("ids")) {
        (Some(id), _) => vec![id],
        (_, Some(ids)) => ids.as_array().ok_or_else(|| bad("ids must be an array"))?.iter().collect(),
        _ => vec![],
    };
    // The shared WKT reader looks at the first ID; prefer its known authority.
    ids.sort_by_key(|v| !v.get("authority").and_then(Value::as_str).is_some_and(|a| a.eq_ignore_ascii_case("EPSG")));
    for value in ids {
        let i = object(value, &["authority", "code", "version", "authority_citation", "uri"])?;
        let code = required(i, "code")?;
        let code = if let Some(s) = code.as_str() {
            q(s)
        } else if let Some(n) = code.as_u64() {
            n.to_string()
        } else {
            return Err(bad("invalid identifier code"));
        };
        let mut values = vec![q(string(i, "authority")?), code];
        if let Some(version) = i.get("version") {
            values.push(match version {
                Value::String(s) => q(s),
                Value::Number(n) => q(&n.to_string()),
                _ => return Err(bad("invalid identifier version")),
            });
        }
        args.push(node("ID", &values));
    }
    Ok(())
}
/// Unit kind and SI conversion factor; strings are the PROJJSON standard units.
fn unit(value: &Value) -> Result<(String, f64, String)> {
    let (kind, factor, name) = match value.as_str() {
        Some("metre") => ("LENGTHUNIT", 1.0, "metre"),
        Some("degree") => ("ANGLEUNIT", std::f64::consts::PI / 180.0, "degree"),
        Some("radian") => ("ANGLEUNIT", 1.0, "radian"),
        Some("grad") => ("ANGLEUNIT", std::f64::consts::PI / 200.0, "grad"),
        Some("unity") => ("SCALEUNIT", 1.0, "unity"),
        Some("parts per million") => ("SCALEUNIT", 1e-6, "parts per million"),
        Some("arc-second") => ("ANGLEUNIT", std::f64::consts::PI / 648000.0, "arc-second"),
        Some(other) => return Err(bad(format!("unknown unit '{other}'; use an explicit unit object"))),
        None => {
            let o = object(value, &["name", "conversion_factor"])?;
            let kind = match string(o, "type")? {
                "LinearUnit" => "LENGTHUNIT",
                "AngularUnit" => "ANGLEUNIT",
                "ScaleUnit" => "SCALEUNIT",
                _ => return Err(bad("unsupported unit type")),
            };
            (kind, number(required(o, "conversion_factor")?)?, string(o, "name")?)
        }
    };
    if factor <= 0.0 {
        return Err(bad("unit factor must be positive"));
    }
    Ok((kind.into(), factor, named(kind, name, vec![factor.to_string()])))
}
fn measure(value: &Value, expected: &str) -> Result<f64> {
    if value.is_number() {
        return number(value);
    }
    let o = object(value, &["value", "unit"])?;
    let (kind, factor, _) = unit(required(o, "unit")?)?;
    if kind != expected {
        return Err(bad("measure has wrong unit type"));
    }
    let result = number(required(o, "value")?)? * factor;
    if !result.is_finite() {
        return Err(bad("measure overflows its SI unit"));
    }
    Ok(result)
}
fn ellipsoid(value: &Value) -> Result<String> {
    let o = object(value, &["name", "semi_major_axis", "semi_minor_axis", "inverse_flattening", "radius"])?;
    typed(o, "Ellipsoid")?;
    let (a, rf) = if let Some(radius) = o.get("radius") {
        if ["semi_major_axis", "semi_minor_axis", "inverse_flattening"].iter().any(|k| o.contains_key(*k)) {
            return Err(bad("conflicting sphere and ellipsoid parameters"));
        }
        (measure(radius, "LENGTHUNIT")?, 0.0)
    } else {
        let a = measure(required(o, "semi_major_axis")?, "LENGTHUNIT")?;
        let rf = match (o.get("inverse_flattening"), o.get("semi_minor_axis")) {
            (Some(rf), None) => number(rf)?,
            (None, Some(b)) => {
                let b = measure(b, "LENGTHUNIT")?;
                if b <= 0.0 || b > a {
                    return Err(bad("invalid semi-minor axis"));
                }
                if a == b { 0.0 } else { a / (a - b) }
            }
            _ => return Err(bad("ellipsoid needs exactly one flattening representation")),
        };
        (a, rf)
    };
    if a <= 0.0 || !a.is_finite() || (rf != 0.0 && rf <= 1.0) {
        return Err(bad("invalid ellipsoid"));
    }
    let mut args = vec![a.to_string(), rf.to_string(), named("LENGTHUNIT", "metre", vec!["1".into()])];
    id(o, &mut args)?;
    Ok(named("ELLIPSOID", string(o, "name")?, args))
}
fn datum(value: &Value) -> Result<(String, Option<String>)> {
    let o = object(value, &["name", "ellipsoid", "prime_meridian", "members", "accuracy"])?;
    let ensemble = o.get("type").and_then(Value::as_str) == Some("DatumEnsemble") || o.contains_key("members");
    typed(o, if ensemble { "DatumEnsemble" } else { "GeodeticReferenceFrame" })?;
    let mut args = Vec::new();
    if ensemble {
        let members = required(o, "members")?.as_array().ok_or_else(|| bad("ensemble members must be an array"))?;
        if members.is_empty() {
            return Err(bad("empty datum ensemble"));
        }
        for member in members {
            let member = object(member, &["name"])?;
            let mut values = vec![];
            id(member, &mut values)?;
            args.push(named("MEMBER", string(member, "name")?, values));
        }
    } else if o.contains_key("accuracy") {
        return Err(bad("accuracy is only supported on datum ensembles"));
    }
    args.push(ellipsoid(required(o, "ellipsoid")?)?);
    if ensemble {
        let accuracy = required(o, "accuracy")?
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|n| n.is_finite())
            .ok_or_else(|| bad("ensemble accuracy must be a numeric string"))?;
        if accuracy < 0.0 {
            return Err(bad("negative ensemble accuracy"));
        }
        args.push(node("ENSEMBLEACCURACY", &[accuracy.to_string()]));
    }
    id(o, &mut args)?;
    let pm = o
        .get("prime_meridian")
        .map(|value| -> Result<String> {
            let p = object(value, &["name", "longitude"])?;
            typed(p, "PrimeMeridian")?;
            let longitude = required(p, "longitude")?;
            let longitude = if longitude.is_number() { number(longitude)? } else { measure(longitude, "ANGLEUNIT")?.to_degrees() };
            if !longitude.is_finite() {
                return Err(bad("prime meridian overflows degrees"));
            }
            Ok(named(
                "PRIMEM",
                string(p, "name")?,
                vec![longitude.to_string(), named("ANGLEUNIT", "degree", vec![(std::f64::consts::PI / 180.0).to_string()])],
            ))
        })
        .transpose()?;
    Ok((named(if ensemble { "ENSEMBLE" } else { "DATUM" }, string(o, "name")?, args), pm))
}
fn coordinate_system(value: &Value, geographic: bool) -> Result<(Vec<String>, String)> {
    let o = object(value, &["subtype", "axis"])?;
    typed(o, "CoordinateSystem")?;
    let subtype = string(o, "subtype")?;
    if subtype != if geographic { "ellipsoidal" } else { "Cartesian" } {
        return Err(bad("unsupported coordinate-system subtype"));
    }
    let axes = required(o, "axis")?.as_array().ok_or_else(|| bad("axis must be an array"))?;
    if axes.len() != 2 {
        return Err(bad("only two-dimensional coordinate systems are supported"));
    }
    let mut out = vec![node("CS", &[subtype.into(), "2".into()])];
    let mut kinds = Vec::new();
    let mut units = Vec::new();
    for axis in axes {
        let a = object(axis, &["name", "abbreviation", "direction", "unit"])?;
        typed(a, "Axis")?;
        let direction = string(a, "direction")?;
        let group = match direction {
            "east" | "west" if !geographic || direction == "east" => 0,
            "north" | "south" if !geographic || direction == "north" => 1,
            _ => return Err(bad("unsupported axis direction")),
        };
        kinds.push(group);
        let (kind, factor, u) = unit(required(a, "unit")?)?;
        if kind != if geographic { "ANGLEUNIT" } else { "LENGTHUNIT" } {
            return Err(bad("axis has wrong unit type"));
        }
        units.push((factor, u.clone()));
        let name = format!("{} ({})", string(a, "name")?, string(a, "abbreviation")?);
        out.push(named("AXIS", &name, vec![direction.into(), u]));
    }
    if kinds[0] == kinds[1] || units[0].0 != units[1].0 {
        return Err(bad("axes must span east/north with a common unit"));
    }
    Ok((out, units.remove(0).1))
}
fn operation(value: &Value, bound: bool) -> Result<String> {
    let o = object(value, &["name", "method", "parameters"])?;
    typed(o, if bound { "AbridgedTransformation" } else { "Conversion" })?;
    let m = object(required(o, "method")?, &["name"])?;
    typed(m, "OperationMethod")?;
    let mut method_args = vec![];
    id(m, &mut method_args)?;
    let mut args = vec![named("METHOD", string(m, "name")?, method_args)];
    let parameters = required(o, "parameters")?.as_array().ok_or_else(|| bad("parameters must be an array"))?;
    let mut seen = std::collections::HashSet::new();
    let mut identities = std::collections::HashSet::new();
    for parameter in parameters {
        let p = object(parameter, &["name", "value", "unit"])?;
        typed(p, "ParameterValue")?;
        let name = string(p, "name")?;
        if !seen.insert(crate::wkt1::norm(name)) {
            return Err(bad("duplicate operation parameter"));
        }
        let raw = number(required(p, "value")?)?;
        let mut values = vec![raw.to_string()];
        let (unit_kind, factor, unit_wkt) = unit(required(p, "unit")?)?;
        if !(raw * factor).is_finite() {
            return Err(bad("operation parameter overflows its SI unit"));
        }
        values.push(unit_wkt);
        id(p, &mut values)?;
        let encoded = named("PARAMETER", name, values);
        let tree = crate::wkt1::parse(&encoded).ok_or_else(|| bad("invalid parameter"))?;
        let (identity, expected) = if bound {
            let code = helmert_parameter(&tree).ok_or_else(|| bad("unknown or inconsistent Helmert parameter"))?;
            let kind = match code {
                8605..=8607 => "LENGTHUNIT",
                8608..=8610 => "ANGLEUNIT",
                8611 => "SCALEUNIT",
                _ => return Err(bad("unknown Helmert parameter")),
            };
            (code, kind)
        } else {
            crate::wkt2::parameter_info(name, tree.epsg()).ok_or_else(|| bad("unknown or inconsistent projection parameter"))?
        };
        if !identities.insert(identity) {
            return Err(bad("duplicate operation parameter identity"));
        }
        if unit_kind != expected {
            return Err(bad("operation parameter has wrong unit type"));
        }
        if bound {
            // WKT2 abridged transformations have fixed implicit units: metres,
            // arc-seconds and a scale multiplier (not a ppm difference).
            let si = raw * factor;
            let value = match identity {
                8605..=8607 => si,
                8608..=8610 => si.to_degrees() * 3600.0,
                8611 => 1.0 + si,
                _ => unreachable!(),
            };
            let mut values = vec![value.to_string()];
            id(p, &mut values)?;
            args.push(named("PARAMETER", name, values));
        } else {
            args.push(encoded);
        }
    }
    id(o, &mut args)?;
    Ok(named(if bound { "ABRIDGEDTRANSFORMATION" } else { "CONVERSION" }, string(o, "name")?, args))
}
fn helmert_parameter(p: &crate::wkt1::Node) -> Option<u32> {
    let named = match crate::wkt1::norm(p.name()?).as_str() {
        "xaxistranslation" => Some(8605),
        "yaxistranslation" => Some(8606),
        "zaxistranslation" => Some(8607),
        "xaxisrotation" => Some(8608),
        "yaxisrotation" => Some(8609),
        "zaxisrotation" => Some(8610),
        "scaledifference" => Some(8611),
        _ => None,
    };
    if let (Some(a), Some(b)) = (named, p.epsg())
        && a != b
    {
        return None;
    }
    p.epsg().or(named)
}
fn crs(value: &Value, base: bool, depth: usize) -> Result<String> {
    if depth > 8 {
        return Err(bad("CRS nesting exceeds 8 levels"));
    }
    let o = object(
        value,
        &["name", "datum", "datum_ensemble", "coordinate_system", "base_crs", "conversion", "source_crs", "target_crs", "transformation"],
    )?;
    let kind = string(o, "type")?;
    let mut args;
    let key = match kind {
        "GeographicCRS" | "GeodeticCRS" => {
            if ["base_crs", "conversion", "source_crs", "target_crs", "transformation"].iter().any(|k| o.contains_key(*k)) {
                return Err(bad("unexpected geographic CRS components"));
            }
            let d = match (o.get("datum"), o.get("datum_ensemble")) {
                (Some(d), None) | (None, Some(d)) => d,
                _ => return Err(bad("CRS needs exactly one datum or ensemble")),
            };
            let (datum, pm) = datum(d)?;
            args = vec![datum];
            args.push(pm.unwrap_or_else(|| {
                named(
                    "PRIMEM",
                    "Greenwich",
                    vec!["0".into(), named("ANGLEUNIT", "degree", vec![(std::f64::consts::PI / 180.0).to_string()])],
                )
            }));
            let (cs, unit) = coordinate_system(required(o, "coordinate_system")?, true)?;
            // WKT2 BASEGEOGCRS has an angular unit, but no coordinate-system axes.
            if base {
                args.push(unit);
            } else {
                args.extend(cs);
            }
            if base { "BASEGEOGCRS" } else { "GEOGCRS" }
        }
        "ProjectedCRS" if !base => {
            if ["datum", "datum_ensemble", "source_crs", "target_crs", "transformation"].iter().any(|k| o.contains_key(*k)) {
                return Err(bad("unexpected projected CRS components"));
            }
            args = vec![crs(required(o, "base_crs")?, true, depth + 1)?, operation(required(o, "conversion")?, false)?];
            args.extend(coordinate_system(required(o, "coordinate_system")?, false)?.0);
            "PROJCRS"
        }
        "BoundCRS" if !base => {
            if ["datum", "datum_ensemble", "coordinate_system", "base_crs", "conversion"].iter().any(|k| o.contains_key(*k)) {
                return Err(bad("unexpected bound CRS components"));
            }
            if required(o, "source_crs")?.get("type").and_then(Value::as_str) == Some("BoundCRS") {
                return Err(bad("nested bound sources are unsupported"));
            }
            let source = crs(required(o, "source_crs")?, false, depth + 1)?;
            let target_value = required(o, "target_crs")?;
            if !matches!(target_value.get("type").and_then(Value::as_str), Some("GeographicCRS" | "GeodeticCRS")) {
                return Err(bad("bound target must be a geographic CRS"));
            }
            let target = crs(target_value, false, depth + 1)?;
            let target_root = crate::wkt1::parse(&target).ok_or_else(|| bad("invalid bound target"))?;
            let target_datum = target_root.child("DATUM").or_else(|| target_root.child("ENSEMBLE"));
            if !target_datum.is_some_and(|d| {
                d.epsg() == Some(6326)
                    || d.children("ID").next().is_none()
                        && matches!(
                            crate::wkt1::norm(d.name().unwrap_or("")).as_str(),
                            "worldgeodeticsystem1984" | "worldgeodeticsystem1984ensemble"
                        )
            }) {
                return Err(bad("bound target must use the WGS 84 datum or ensemble"));
            }
            let target_def = Def::from_wkt(&target)?;
            let wgs84 = crate::math::Ell::wgs84();
            if !target_def.is_geographic()
                || !target_def.is_wgs84()
                || (target_def.ell.a - wgs84.a).abs() > 1e-6
                || (target_def.ell.es - wgs84.es).abs() > 1e-14
                || !crate::wkt_proj(&target).is_some_and(|p| !p.contains("+pm="))
            {
                return Err(bad("bound target must be a WGS 84 geographic CRS at Greenwich"));
            }
            let operation = operation(required(o, "transformation")?, true)?;
            let tree = crate::wkt1::parse(&operation).ok_or_else(|| bad("invalid bound operation"))?;
            let method = tree.child("METHOD").ok_or_else(|| bad("missing bound transformation method"))?;
            let name = crate::wkt1::norm(method.name().unwrap_or(""));
            // 0: translations; 1: position vector; 2: coordinate frame.
            let by_id = match method.epsg() {
                Some(9603 | 1031 | 1035) => Some(0),
                Some(9606 | 1033 | 1037) => Some(1),
                Some(9607 | 1032 | 1038) => Some(2),
                _ => None,
            };
            let by_name = if name.starts_with("geocentrictranslations") {
                Some(0)
            } else if name.starts_with("positionvector") {
                Some(1)
            } else if name.starts_with("coordinateframe") {
                Some(2)
            } else {
                None
            };
            if let (Some(a), Some(b)) = (by_id, by_name)
                && a != b
            {
                return Err(bad("inconsistent Helmert method name and identifier"));
            }
            let kind = by_id.or(by_name).ok_or_else(|| bad("bound transformation must identify a supported Helmert method"))?;
            let expected = if kind == 0 { 3 } else { 7 };
            let codes: std::collections::HashSet<_> = tree.children("PARAMETER").filter_map(helmert_parameter).collect();
            if tree.children("PARAMETER").count() != expected
                || codes.len() != expected
                || !(8605..8605 + expected as u32).all(|c| codes.contains(&c))
            {
                return Err(bad("incomplete or duplicate Helmert parameters"));
            }
            return Ok(node("BOUNDCRS", &[node("SOURCECRS", &[source]), node("TARGETCRS", &[target]), operation]));
        }
        _ => return Err(bad(format!("unsupported CRS type '{kind}'"))),
    };
    let mut identifiers = vec![];
    id(o, &mut identifiers)?;
    // A root ID would make the general WKT reader resolve the registry CRS,
    // replacing supplied conversion parameters after serialization/reopening.
    // Validate those identifiers but retain only component IDs in exported WKT.
    if depth != 0 {
        args.extend(identifiers);
    }
    Ok(named(key, string(o, "name")?, args))
}

pub(crate) fn parse(text: &str) -> Result<SpatialRef> {
    let value: Value = serde_json::from_str(text).map_err(|e| bad(format!("invalid JSON: {e}")))?;
    let wkt = crs(&value, false, 0)?;
    // Parse the complete supplied definition before considering any authority IDs.
    // Keep WKT as the transformation source so an ID cannot replace caller parameters.
    Def::from_wkt(&wkt)?;
    Ok(SpatialRef { wkt: Some(wkt), ..Default::default() })
}

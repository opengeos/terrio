// Written to match the behavior of:
//   PROJ 9.8.1:
//     src/iso19111/io.cpp (JSONFormatter)
// No code is translated from these sources.

//! PROJJSON text with its keys in PROJ's order, whatever order the `serde_json::Map` holds them
//! in. terrio-crs does not enable serde_json's `preserve_order` feature: Cargo would turn it on
//! for every crate of a dependent's build, changing the key order of their JSON.

use serde_json::Value;

/// Every key PROJ 9.8.1 writes in the PROJJSON of a registry CRS (and `ids`, its list of several
/// identifiers), in an order that keeps the relative order of the keys of each object PROJ writes
/// (`tests/projjson_key_order.rs` checks it against PROJ's objects). One order serves every
/// object: PROJ never writes two keys in both orders.
const KEY_ORDER: &[&str] = &[
    "$schema",
    "type",
    "name",
    "source_crs",
    "target_crs",
    "transformation",
    "components",
    "members",
    "frame_reference_epoch",
    "anchor",
    "anchor_epoch",
    "semi_major_axis",
    "semi_minor_axis",
    "inverse_flattening",
    "radius",
    "ellipsoid",
    "accuracy",
    "prime_meridian",
    "longitude",
    "datum",
    "datum_ensemble",
    "base_crs",
    "conversion",
    "method",
    "parameters",
    "value",
    "coordinate_system",
    "subtype",
    "axis",
    "abbreviation",
    "direction",
    "meridian",
    "unit",
    "conversion_factor",
    "usages",
    "scope",
    "area",
    "bbox",
    "south_latitude",
    "west_longitude",
    "north_latitude",
    "east_longitude",
    "id",
    "ids",
    "authority",
    "code",
    "version",
    "remarks",
];

/// A key's place in [`KEY_ORDER`]; keys outside it come after, in the map's order.
fn rank(key: &str) -> usize {
    KEY_ORDER.iter().position(|k| *k == key).unwrap_or(KEY_ORDER.len())
}

/// The compact JSON text of a PROJJSON value, its objects' keys in PROJ's order.
pub(crate) fn to_string(value: &Value) -> String {
    let mut out = String::new();
    write(value, &mut out);
    out
}

fn write(value: &Value, out: &mut String) {
    match value {
        Value::Object(o) => {
            let mut entries: Vec<_> = o.iter().collect();
            entries.sort_by_key(|(k, _)| rank(k));
            out.push('{');
            for (i, (k, v)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::from(k.as_str()).to_string());
                out.push(':');
                write(v, out);
            }
            out.push('}');
        }
        Value::Array(a) => {
            out.push('[');
            for (i, v) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write(v, out);
            }
            out.push(']');
        }
        _ => out.push_str(&value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn keys_are_written_in_proj_s_order_and_values_as_serde_json_writes_them() {
        let v = json!({"id": {"code": 4326, "authority": "EPSG"}, "name": "a \"b\"", "type": "GeographicCRS", "x": [1.5, null, true]});
        assert_eq!(to_string(&v), r#"{"type":"GeographicCRS","name":"a \"b\"","id":{"authority":"EPSG","code":4326},"x":[1.5,null,true]}"#);
    }

    #[test]
    fn the_order_names_each_key_once() {
        for (i, k) in KEY_ORDER.iter().enumerate() {
            assert_eq!(rank(k), i, "{k}");
        }
    }

    /// Every kind of object in the PROJJSON PROJ 9.8.1 writes for its registry CRSs
    /// (`scripts/gen_projjson_key_order.py`) has its keys in the order.
    #[test]
    fn the_order_is_proj_s() {
        let objects = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/projjson_key_order.txt"));
        assert_eq!(objects.lines().count(), 50);
        for keys in objects.lines() {
            let ranks: Vec<usize> = keys.split(',').map(rank).collect();
            assert!(ranks.iter().all(|r| *r < KEY_ORDER.len()) && ranks.is_sorted_by(|a, b| a < b), "{keys}");
        }
    }
}

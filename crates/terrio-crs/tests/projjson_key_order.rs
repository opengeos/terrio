//! `SpatialRef::to_projjson` writes each object's keys in the order PROJ 9.8.1 writes them,
//! whether or not serde_json's `preserve_order` feature is on in the build. The key orders of
//! PROJ's objects are `data/projjson_key_order.txt` (`scripts/gen_projjson_key_order.py`).

use std::collections::HashSet;
use terrio_crs::SpatialRef;

/// The pairs of keys PROJ writes in one object, first key first.
fn proj_pairs() -> HashSet<(String, String)> {
    let mut pairs = HashSet::new();
    for line in include_str!("data/projjson_key_order.txt").lines() {
        let keys: Vec<&str> = line.split(',').collect();
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                pairs.insert((a.to_string(), b.to_string()));
            }
        }
    }
    pairs
}

/// The keys of each object of a JSON text, in the text's order. A scan of the text, since a
/// parsed `Value` holds its keys in its own order.
fn key_lists(json: &str) -> Vec<Vec<String>> {
    // An open object (its keys, and whether a key comes next) or array (`None`).
    let mut open: Vec<Option<(Vec<String>, bool)>> = Vec::new();
    let mut done = Vec::new();
    let mut chars = json.chars();
    while let Some(c) = chars.next() {
        match c {
            '{' => open.push(Some((Vec::new(), true))),
            '[' => open.push(None),
            '}' => done.push(open.pop().flatten().unwrap().0),
            ']' => assert!(open.pop().unwrap().is_none()),
            ',' => {
                if let Some(Some((_, next_is_key))) = open.last_mut() {
                    *next_is_key = true;
                }
            }
            '"' => {
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => s.push(chars.next().unwrap()),
                        '"' => break,
                        c => s.push(c),
                    }
                }
                if let Some(Some((keys, next_is_key @ true))) = open.last_mut() {
                    keys.push(s);
                    *next_is_key = false;
                }
            }
            _ => {}
        }
    }
    assert!(open.is_empty());
    done
}

#[test]
fn the_scan_finds_each_object_s_keys() {
    let lists = key_lists(r#"{"b":{"y":"x,\"z\"","x":[1,{"q":2,"p":[]}]},"a":"{"}"#);
    assert_eq!(lists, [vec!["q", "p"], vec!["y", "x"], vec!["b", "a"]]);
}

/// Every CRS kind terrio writes (every 7th EPSG and ESRI code, IAU_2015 CRSs, bound, compound
/// and planetary CRSs): no two keys in the other order than PROJ's, and no key PROJ lacks. Two
/// keys PROJ never writes in one object have no order to check.
#[test]
fn keys_are_in_proj_s_order() {
    let pairs = proj_pairs();
    let known: HashSet<&str> = include_str!("data/projjson_key_order.txt").lines().flat_map(|l| l.split(',')).collect();
    let mut definitions: Vec<String> =
        ["EPSG:4326", "EPSG:4979", "EPSG:4978", "EPSG:5773", "EPSG:9707", "EPSG:31467", "EPSG:2985"].map(String::from).to_vec();
    definitions.extend((2000..32800).step_by(7).map(|c| format!("EPSG:{c}")));
    definitions.extend((53001..54100).step_by(7).map(|c| format!("ESRI:{c}")));
    definitions.extend((10000..100000).step_by(997).map(|c| format!("IAU_2015:{c}")));
    definitions.extend(
        [
            "+proj=longlat +ellps=bessel +towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7",
            "+proj=tmerc +ellps=GRS80 +towgs84=1,2,3",
            "+proj=longlat +a=2440530 +b=2438260 +axis=nwu",
            "+proj=longlat +a=2440530 +b=2438260 +geoc",
            "+proj=lcc +lat_1=45 +lat_0=40 +datum=NAD83 +units=us-ft",
        ]
        .map(String::from),
    );
    let mut written = 0;
    for d in &definitions {
        let Ok(json) = SpatialRef::parse(d).and_then(|s| s.to_projjson()) else { continue };
        written += 1;
        assert!(json.starts_with(r#"{"$schema":"#), "{d}");
        for keys in key_lists(&json) {
            for (i, a) in keys.iter().enumerate() {
                assert!(known.contains(a.as_str()), "{d}: {a}");
                for b in &keys[i + 1..] {
                    assert!(!pairs.contains(&(b.clone(), a.clone())), "{d}: {a} before {b}");
                }
            }
        }
    }
    assert!(written > 1000, "{written}");
}

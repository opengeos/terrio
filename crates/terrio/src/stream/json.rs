//! A scanner of GeoJSON FeatureCollections that yields the text of each feature without reading
//! the whole document, so that collections larger than memory are read feature by feature.

use crate::{Error, Result};
use serde_json::{Map, Value};
use std::io::{BufRead, BufReader, Read};

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("GeoJSON: {msg}"))
}

/// Reads JSON values from a byte stream.
struct Scanner<R: Read> {
    r: BufReader<R>,
}

impl<R: Read> Scanner<R> {
    /// The next byte that is not white space (nor a byte order mark), not consumed.
    fn skip_ws(&mut self) -> Result<Option<u8>> {
        loop {
            let buf = self.r.fill_buf()?;
            if buf.is_empty() {
                return Ok(None);
            }
            match buf.iter().position(|b| !(b.is_ascii_whitespace() || matches!(b, 0xEF | 0xBB | 0xBF))) {
                Some(i) => {
                    let b = buf[i];
                    self.r.consume(i);
                    return Ok(Some(b));
                }
                None => {
                    let n = buf.len();
                    self.r.consume(n);
                }
            }
        }
    }

    fn bump(&mut self) {
        self.r.consume(1);
    }

    /// Appends the text of the next value to `out`.
    fn value(&mut self, out: &mut Vec<u8>) -> Result<()> {
        let first = self.skip_ws()?.ok_or_else(|| bad("unexpected end of file"))?;
        let structured = matches!(first, b'{' | b'[' | b'"');
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut started = false;
        loop {
            let buf = self.r.fill_buf()?;
            if buf.is_empty() {
                if !structured && started {
                    return Ok(());
                }
                return Err(bad("unexpected end of file"));
            }
            let mut end = None;
            for (i, &b) in buf.iter().enumerate() {
                if !structured {
                    if matches!(b, b',' | b'}' | b']') || b.is_ascii_whitespace() {
                        end = Some((i, false));
                        break;
                    }
                    started = true;
                    continue;
                }
                if in_string {
                    if escaped {
                        escaped = false;
                    } else if b == b'\\' {
                        escaped = true;
                    } else if b == b'"' {
                        in_string = false;
                        if depth == 0 {
                            end = Some((i + 1, true));
                            break;
                        }
                    }
                    continue;
                }
                match b {
                    b'"' => in_string = true,
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth = depth.checked_sub(1).ok_or_else(|| bad("unbalanced brackets"))?;
                        if depth == 0 {
                            end = Some((i + 1, true));
                            break;
                        }
                    }
                    _ => {}
                }
            }
            match end {
                Some((n, _)) => {
                    out.extend_from_slice(&buf[..n]);
                    self.r.consume(n);
                    return Ok(());
                }
                None => {
                    let n = buf.len();
                    out.extend_from_slice(buf);
                    self.r.consume(n);
                }
            }
        }
    }
}

#[derive(PartialEq)]
enum State {
    Members,
    Features,
    Done,
}

/// The features of a GeoJSON document, read one at a time; the document's other members
/// (`type`, `name`, `crs`...) are collected as they are met.
pub(crate) struct Collection<R: Read> {
    s: Scanner<R>,
    state: State,
    /// The members other than `features` read so far (all of them once the features are read).
    pub members: Map<String, Value>,
    /// Whether the document has a `features` array.
    pub has_features: bool,
}

impl<R: Read> Collection<R> {
    /// Starts reading a document, which must be a JSON object.
    pub(crate) fn new(r: R) -> Result<Self> {
        let mut s = Scanner { r: BufReader::with_capacity(1 << 20, r) };
        match s.skip_ws()? {
            Some(b'{') => s.bump(),
            _ => return Err(bad("the document is not a JSON object")),
        }
        Ok(Self { s, state: State::Members, members: Map::new(), has_features: false })
    }

    /// Reads the next feature's text into `out` (cleared first). `false` at the end of the
    /// document.
    pub(crate) fn next_feature(&mut self, out: &mut Vec<u8>) -> Result<bool> {
        loop {
            match self.state {
                State::Done => return Ok(false),
                State::Members => match self.s.skip_ws()? {
                    None => return Err(bad("unexpected end of file")),
                    Some(b'}') => {
                        self.s.bump();
                        self.state = State::Done;
                    }
                    Some(b',') => self.s.bump(),
                    Some(_) => {
                        let mut raw = Vec::new();
                        self.s.value(&mut raw)?;
                        let key: String = serde_json::from_slice(&raw).map_err(bad)?;
                        if self.s.skip_ws()? != Some(b':') {
                            return Err(bad("expected ':'"));
                        }
                        self.s.bump();
                        if key == "features" && self.s.skip_ws()? == Some(b'[') {
                            self.s.bump();
                            self.has_features = true;
                            self.state = State::Features;
                            continue;
                        }
                        raw.clear();
                        self.s.value(&mut raw)?;
                        let v: Value = serde_json::from_slice(&raw).map_err(bad)?;
                        self.members.insert(key, v);
                    }
                },
                State::Features => match self.s.skip_ws()? {
                    None => return Err(bad("unexpected end of file")),
                    Some(b']') => {
                        self.s.bump();
                        self.state = State::Members;
                    }
                    Some(b',') => self.s.bump(),
                    Some(_) => {
                        out.clear();
                        self.s.value(out)?;
                        return Ok(true);
                    }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_features_and_members() {
        let text = br#"{"type":"FeatureCollection","features":[ {"type":"Feature","properties":{"a":"x]}\"y"},"geometry":null} ,
            {"type":"Feature","properties":{"b":[1,{"c":2}]},"geometry":{"type":"Point","coordinates":[1,2]}}],"name":"n","crs":null}"#;
        // A tiny buffer would not change the result: the scanner refills as it goes.
        let mut c = Collection::new(&text[..]).unwrap();
        let mut out = Vec::new();
        let mut features = Vec::new();
        while c.next_feature(&mut out).unwrap() {
            features.push(serde_json::from_slice::<Value>(&out).unwrap());
        }
        assert_eq!(features.len(), 2);
        assert_eq!(features[0]["properties"]["a"], "x]}\"y");
        assert_eq!(features[1]["properties"]["b"][1]["c"], 2);
        assert_eq!(c.members["name"], "n");
        assert_eq!(c.members["type"], "FeatureCollection");
        assert!(c.members["crs"].is_null());
        assert!(c.has_features);
    }
}

//! A small XML element tree, for the XML vector formats (GML, GPX, KML).
//!
//! Names are local names: namespace prefixes are dropped, which is how the formats' readers look
//! elements up.

use crate::{Error, Result};
use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

/// An XML element.
#[derive(Debug, Clone, Default)]
pub struct Element {
    /// The local name (without namespace prefix).
    pub name: String,
    /// The namespace prefix, empty when there is none.
    pub prefix: String,
    /// The attributes, by qualified name, unescaped.
    pub attrs: Vec<(String, String)>,
    /// The child elements, in document order.
    pub children: Vec<Element>,
    /// The text content directly inside the element (not its children's), unescaped.
    pub text: String,
}

impl Element {
    /// An ordinary attribute by local name, preferring an unqualified attribute.
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(key, _)| key == name && key != "xmlns")
            .or_else(|| {
                self.attrs.iter().find(|(key, _)| key != "xmlns" && !key.starts_with("xmlns:") && key.rsplit(':').next() == Some(name))
            })
            .map(|(_, value)| value.as_str())
    }

    /// A namespace declaration on this element, separate from ordinary attributes.
    pub fn namespace(&self, prefix: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(key, _)| if prefix.is_empty() { key == "xmlns" } else { key.strip_prefix("xmlns:") == Some(prefix) })
            .map(|(_, value)| value.as_str())
    }

    /// The first child with a local name.
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }

    /// The children with a local name.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }

    /// The trimmed text of the first child with a local name, when it is not empty.
    pub fn child_text(&self, name: &str) -> Option<&str> {
        self.child(name).map(|c| c.text.trim()).filter(|t| !t.is_empty())
    }

    /// The first descendant (depth first, this element included) with a local name.
    pub fn find(&self, name: &str) -> Option<&Element> {
        if self.name == name {
            return Some(self);
        }
        self.children.iter().find_map(|c| c.find(name))
    }

    /// Every descendant with a local name, in document order, not looking inside matches.
    pub fn find_all<'a>(&'a self, name: &str, out: &mut Vec<&'a Element>) {
        for c in &self.children {
            if c.name == name {
                out.push(c);
            } else {
                c.find_all(name, out);
            }
        }
    }
}

fn local(name: &str) -> String {
    name.rsplit_once(':').map_or(name, |(_, l)| l).to_string()
}

fn element(e: &BytesStart, strict: bool) -> Result<Element> {
    let mut attrs = Vec::new();
    for attribute in e.attributes() {
        let attribute = match attribute {
            Ok(attribute) => attribute,
            Err(error) if strict => return Err(Error::InvalidData(format!("XML attribute: {error}"))),
            Err(_) => continue,
        };
        let value = match attribute.normalized_value(XmlVersion::Implicit1_0) {
            Ok(value) => value.into_owned(),
            Err(error) if strict => return Err(Error::InvalidData(format!("XML attribute: {error}"))),
            Err(_) => String::new(),
        };
        attrs.push((attribute.key.as_ref().to_string(), value));
    }
    let name = e.name();
    let qname: &str = name.as_ref();
    let prefix = qname.rsplit_once(':').map(|(p, _)| p.to_string()).unwrap_or_default();
    Ok(Element { name: local(qname), prefix, attrs, ..Default::default() })
}

/// Decodes a document's bytes: UTF-8 (with or without a byte order mark), or Latin-1 when the
/// bytes are not UTF-8.
pub fn decode(bytes: &[u8]) -> String {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Parses a document and returns its root element.
pub fn parse(text: &str) -> Result<Element> {
    parse_with_limits(text, usize::MAX, usize::MAX, false)
}

/// Parse legacy XML with a caller-specific element bound and a 256-level nesting bound.
pub fn parse_limited(text: &str, max_elements: usize) -> Result<Element> {
    parse_with_limits(text, max_elements, 256, false)
}

/// Parse well-formed service XML within the same element and nesting budgets.
#[cfg(remote)]
pub fn parse_strict_limited(text: &str, max_elements: usize) -> Result<Element> {
    parse_with_limits(text, max_elements, 256, true)
}

fn parse_with_limits(text: &str, max_elements: usize, max_depth: usize, strict: bool) -> Result<Element> {
    let err = |e: &dyn std::fmt::Display| Error::InvalidData(format!("XML: {e}"));
    let mut reader = Reader::from_str(text);
    reader.config_mut().check_end_names = strict;
    let mut stack: Vec<Element> = vec![Element::default()];
    let mut elements = 0usize;
    loop {
        let event = reader.read_event().map_err(|e| err(&e))?;
        if matches!(&event, Event::Start(_) | Event::Empty(_)) {
            if elements >= max_elements {
                return Err(Error::ResourceLimit(format!("XML: element count exceeds {max_elements}")));
            }
            elements += 1;
        }
        if matches!(&event, Event::Start(_) | Event::Empty(_)) && stack.len() > max_depth {
            return Err(Error::ResourceLimit(format!("XML: nesting exceeds {max_depth} element levels")));
        }
        match event {
            Event::Start(e) => stack.push(element(&e, strict)?),
            Event::Empty(e) => {
                let el = element(&e, strict)?;
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(el);
                }
            }
            Event::End(_) => {
                if stack.len() > 1 {
                    let el = stack.pop().unwrap_or_default();
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(el);
                    }
                }
            }
            Event::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t.xml10_content());
                }
            }
            Event::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&t.xml10_content());
                }
            }
            Event::GeneralRef(r) => {
                let c = match r.resolve_char_ref().ok().flatten() {
                    Some(c) => Some(c),
                    None => match &*r {
                        "amp" => Some('&'),
                        "lt" => Some('<'),
                        "gt" => Some('>'),
                        "quot" => Some('"'),
                        "apos" => Some('\''),
                        _ => None,
                    },
                };
                if strict && c.is_none() {
                    return Err(Error::InvalidData(format!("XML: unresolved entity reference &{};", &*r)));
                }
                if let (Some(c), Some(top)) = (c, stack.last_mut()) {
                    top.text.push(c);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if strict && stack.len() != 1 {
        return Err(Error::InvalidData("XML: unclosed elements in a truncated document".into()));
    }
    // Preserve the legacy permissive reader for ordinary file formats.
    while stack.len() > 1 {
        let el = stack.pop().unwrap_or_default();
        if let Some(parent) = stack.last_mut() {
            parent.children.push(el);
        }
    }
    let doc = stack.pop().unwrap_or_default();
    if strict && (doc.children.len() != 1 || !doc.text.trim().is_empty()) {
        return Err(Error::InvalidData("XML: expected one root element".into()));
    }
    doc.children.into_iter().next().ok_or_else(|| Error::InvalidData("XML: no root element".into()))
}

/// Escapes text for XML content and attribute values.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn caller_element_limit_counts_empty_siblings() {
        assert!(super::parse_limited("<a><b/><b/><b/></a>", 3).unwrap_err().to_string().contains("element count"));
        assert_eq!(super::parse_limited("<a><b/><b/></a>", 3).unwrap().children.len(), 2);
    }
    use super::*;

    #[test]
    fn deep_trees_fail_before_recursive_traversal_or_drop() {
        let document = "<a>".repeat(100_000);
        assert!(parse_limited(&document, 65_536).unwrap_err().to_string().contains("256 element levels"));
        let allowed = format!("{}{}", "<a>".repeat(256), "</a>".repeat(256));
        let root = parse_limited(&allowed, 65_536).unwrap();
        assert!(root.find("missing").is_none());
        let empty_leaf = format!("{}<b/>{}", "<a>".repeat(256), "</a>".repeat(256));
        assert!(parse_limited(&empty_leaf, 65_536).is_err());
        assert!(parse(&empty_leaf).is_ok());
    }
    #[test]
    fn parses_elements_text_and_entities() {
        let root = parse(r#"<?xml version="1.0"?><a:root xmlns:a="x" a:id="1"><b>T &amp; &#233;<![CDATA[<c>]]></b><b/></a:root>"#).unwrap();
        assert_eq!((root.name.as_str(), root.prefix.as_str()), ("root", "a"));
        assert_eq!(root.attr("id"), Some("1"));
        assert_eq!(root.children_named("b").count(), 2);
        assert_eq!(root.child_text("b"), Some("T & é<c>"));
        assert_eq!(escape("a<b & \"c\""), "a&lt;b &amp; &quot;c&quot;");
    }
}

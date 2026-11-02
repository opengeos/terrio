// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/libkml/ogrlibkmlstyle.cpp
//     ogr/ogrfeaturestyle.cpp
// No code is translated from these sources.

//! KML styles and OGR feature style strings (`PEN(c:#FF0000FF,w:2.000000px);BRUSH(...)`), the
//! form GDAL gives feature styles in and keeps in an `OGR_STYLE` field.
//!
//! Reading converts `<Style>` elements as GDAL's LIBKML driver does: `LineStyle` to `PEN`,
//! `PolyStyle` to `BRUSH`, `IconStyle` to `SYMBOL` and `LabelStyle` to `LABEL`, colors from
//! KML's `aabbggrr` to `#RRGGBBAA`. A `StyleMap` stands for its `normal` style. A placemark that
//! only refers to a style of its document keeps the reference (`@id`), and the document's
//! definitions make the dataset's style table. Writing goes the other way.
//!
//! Three things follow KML rather than GDAL 3.13, which loses them: PolyStyle `fill` and `outline`
//! become OGR's null brush and pen, a placemark's inline sub-styles merge over its shared style,
//! and a `styleUrl` into another document of a KMZ resolves against that document.

use super::xml::{self, Element};
use std::collections::HashMap;
use std::fmt::Write as _;

/// The name of the field holding feature styles.
pub const FIELD: &str = "OGR_STYLE";

/// OGR's null brush, for `<fill>0</fill>`.
const NULL_BRUSH: &str = "ogr-brush-1";
/// OGR's null pen, for `<outline>0</outline>`.
const NULL_PEN: &str = "ogr-pen-1";

/// A KML color (`aabbggrr`, optionally with `#`) as `#RRGGBBAA`.
fn color(kml: &str) -> Option<String> {
    let h = kml.trim().trim_start_matches('#');
    if h.len() != 8 || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let (a, b, g, r) = (&h[0..2], &h[2..4], &h[4..6], &h[6..8]);
    Some(format!("#{r}{g}{b}{a}").to_ascii_uppercase())
}

/// An OGR color (`#RRGGBB` or `#RRGGBBAA`) as KML's `aabbggrr`.
fn kml_color(ogr: &str) -> Option<String> {
    let h = ogr.trim().trim_start_matches('#');
    if !(h.len() == 6 || h.len() == 8) || !h.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let a = if h.len() == 8 { &h[6..8] } else { "ff" };
    Some(format!("{a}{}{}{}", &h[4..6], &h[2..4], &h[0..2]).to_ascii_lowercase())
}

/// Whether a KML boolean (`0`, `false`) is off.
fn is_off(v: &str) -> bool {
    matches!(v.trim(), "0" | "false")
}

/// A `<Style>` element as an OGR style string; `None` when it sets nothing.
pub fn from_kml(style: &Element) -> Option<String> {
    merged(&[style])
}

/// The style a stack of `<Style>` elements makes, the first over the rest, as an OGR style string:
/// each child of a sub-style (a `LineStyle`'s `width`, say) comes from the first element setting
/// it, as KML viewers merge an inline style over a shared one. `None` when they set nothing.
fn merged(stack: &[&Element]) -> Option<String> {
    // The first value of a sub-style's child down the stack.
    let child = |sub: &str, name: &str| stack.iter().find_map(|s| s.child(sub)?.child(name));
    let text = |sub: &str, name: &str| stack.iter().find_map(|s| s.child(sub)?.child_text(name));
    let number = |sub: &str, name: &str| text(sub, name)?.parse::<f64>().ok();
    let has = |sub: &str| stack.iter().any(|s| s.child(sub).is_some());
    let mut tools = Vec::new();
    // PolyStyle's flags: GDAL ignores them, losing an outline-only polygon.
    let no_fill = text("PolyStyle", "fill").is_some_and(is_off);
    let no_outline = text("PolyStyle", "outline").is_some_and(is_off);
    if has("LineStyle") || no_outline {
        let mut p = Vec::new();
        if let Some(c) = text("LineStyle", "color").and_then(color) {
            p.push(format!("c:{c}"));
        }
        if let Some(w) = number("LineStyle", "width") {
            p.push(format!("w:{w:.6}px"));
        }
        if no_outline {
            p.push(format!("id:\"{NULL_PEN}\""));
        }
        tools.push(("PEN", p));
    }
    if no_fill {
        tools.push(("BRUSH", vec![format!("id:\"{NULL_BRUSH}\"")]));
    } else if has("PolyStyle") {
        tools.push(("BRUSH", text("PolyStyle", "color").and_then(color).map(|c| vec![format!("fc:{c}")]).unwrap_or_default()));
    }
    if has("IconStyle") {
        let mut p = Vec::new();
        if let Some(href) = child("IconStyle", "Icon").and_then(|ic| ic.child_text("href")) {
            p.push(format!("id:\"{href}\""));
        }
        if let Some(a) = number("IconStyle", "heading") {
            p.push(format!("a:{a:.6}"));
        }
        if let Some(c) = text("IconStyle", "color").and_then(color) {
            p.push(format!("c:{c}"));
        }
        if let Some(s) = number("IconStyle", "scale") {
            p.push(format!("s:{s:.6}"));
        }
        if let Some(h) = child("IconStyle", "hotSpot") {
            for (k, a) in [("dx", "x"), ("dy", "y")] {
                if let Some(v) = h.attr(a).and_then(|v| v.trim().parse::<f64>().ok()) {
                    p.push(format!("{k}:{v:.6}"));
                }
            }
        }
        tools.push(("SYMBOL", p));
    }
    if has("LabelStyle") {
        let mut p = Vec::new();
        if let Some(c) = text("LabelStyle", "color").and_then(color) {
            p.push(format!("c:{c}"));
        }
        if let Some(s) = number("LabelStyle", "scale") {
            p.push(format!("w:{:.6}", s * 100.0));
        }
        tools.push(("LABEL", p));
    }
    let s: Vec<String> = tools.into_iter().filter(|(_, p)| !p.is_empty()).map(|(t, p)| format!("{t}({})", p.join(","))).collect();
    (!s.is_empty()).then(|| s.join(";"))
}

/// The other KML documents of a KMZ archive, by member name, that `styleUrl`s can point into.
pub type Members = std::collections::BTreeMap<String, Element>;

/// How deep `StyleMap`s may refer to each other, which also ends a cycle of them.
const MAX_STYLE_DEPTH: usize = 8;

/// The definitions of a document (`<Style>` and `<StyleMap>` with an id), first of an id winning.
type Definitions<'a> = HashMap<&'a str, &'a Element>;

/// One KML document's styles, as a placemark's `styleUrl` finds them.
pub struct Styles<'a> {
    /// The document's own definitions, in document order.
    own: Vec<&'a Element>,
    /// The document's name in its KMZ archive; empty outside one.
    member: &'a str,
    /// The definitions of each document, by name, this one's included.
    documents: HashMap<&'a str, Definitions<'a>>,
    /// Ids whose definition here differs from the dataset's style table, which `@id` could not
    /// tell apart: this document's placemarks get the style itself.
    shadowed: Vec<String>,
}

impl<'a> Styles<'a> {
    /// The styles of the document at `root`, which is `member` of a KMZ archive holding `members`.
    pub fn new(root: &'a Element, member: &'a str, members: &'a Members) -> Self {
        let mut own = Vec::new();
        collect_definitions(root, &mut own);
        let mut documents: HashMap<&'a str, Definitions<'a>> = HashMap::with_capacity(members.len() + 1);
        for (name, document) in members {
            let mut defined = Vec::new();
            collect_definitions(document, &mut defined);
            documents.insert(name, index(&defined));
        }
        documents.insert(member, index(&own));
        Self { own, member, documents, shadowed: Vec::new() }
    }

    /// Adds the document's styles to the dataset's style table. An id another document of the
    /// archive already defines differently keeps that definition in the table; the placemarks of
    /// this document that use it get the style itself.
    pub fn add_to(&mut self, table: &mut Vec<(String, String)>) {
        for (id, style) in self.table() {
            match table.iter().find(|(k, _)| *k == id) {
                Some((_, known)) if *known != style => self.shadowed.push(id),
                Some(_) => {}
                None => table.push((id, style)),
            }
        }
    }

    /// The styles the document defines, by id, as OGR style strings, in document order. A style
    /// map stands for its `normal` style.
    fn table(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for e in &self.own {
            let Some(id) = e.attr("id") else { continue };
            if out.iter().any(|(k, _)| k == id) {
                continue;
            }
            let mut stack = Vec::new();
            self.resolve(e, self.member, 0, &mut stack);
            if let Some(s) = merged(&stack) {
                out.push((id.to_string(), s));
            }
        }
        out
    }

    /// A placemark's style: a reference to a style in the same document stays a reference
    /// (`@id`), as in GDAL. Beside an inline `<Style>`, the sub-styles merge, inline over shared.
    /// A `styleUrl` into another document of the KMZ resolves against it, relative to this one;
    /// other URLs stay as they are.
    pub fn of_feature(&self, f: &Element) -> Option<String> {
        let inline = f.child("Style").filter(|s| !s.children.is_empty());
        let Some(url) = f.child_text("styleUrl").map(str::trim) else { return inline.and_then(from_kml) };
        let mut stack: Vec<&Element> = Vec::new();
        self.follow(url, self.member, 0, &mut stack);
        let own_reference = inline.is_none() && url.strip_prefix('#').is_some_and(|id| !self.shadowed.iter().any(|s| s == id));
        if own_reference || (inline.is_none() && stack.is_empty()) {
            return Some(reference(url));
        }
        stack.splice(0..0, inline);
        merged(&stack).or_else(|| Some(reference(url)))
    }

    /// Pushes the `<Style>` elements a definition comes to, strongest first: a `<Style>` itself,
    /// or a `StyleMap`'s `normal` pair (its inline style over the style its `styleUrl` names).
    fn resolve<'b>(&'b self, e: &'b Element, member: &str, depth: usize, out: &mut Vec<&'b Element>) {
        if e.name == "Style" {
            out.push(e);
            return;
        }
        let Some(pair) = e.children_named("Pair").find(|p| p.child_text("key").map(str::trim) == Some("normal")) else { return };
        out.extend(pair.child("Style"));
        if let Some(url) = pair.child_text("styleUrl") {
            self.follow(url.trim(), member, depth + 1, out);
        }
    }

    /// Pushes the `<Style>` elements a `styleUrl` leads to; none when it leads nowhere in the
    /// archive.
    fn follow<'b>(&'b self, url: &str, member: &str, depth: usize, out: &mut Vec<&'b Element>) {
        if depth >= MAX_STYLE_DEPTH {
            return;
        }
        let Some((document, id)) = url.split_once('#') else { return };
        let target = if document.is_empty() { Some(member.to_string()) } else { member_of(document, member) };
        let Some((name, definitions)) = target.and_then(|t| self.documents.get_key_value(t.as_str())) else { return };
        if let Some(e) = definitions.get(id) {
            self.resolve(e, name, depth, out);
        }
    }
}

/// How an unresolved `styleUrl` reads in a style field.
fn reference(url: &str) -> String {
    url.strip_prefix('#').map_or_else(|| url.to_string(), |id| format!("@{id}"))
}

/// Every `<Style>` and `<StyleMap>` with an id below `e`, in document order. Placemarks' own
/// styles are left out: they are not shared.
fn collect_definitions<'a>(e: &'a Element, out: &mut Vec<&'a Element>) {
    for c in &e.children {
        match c.name.as_str() {
            "Style" | "StyleMap" if c.attr("id").is_some() => out.push(c),
            "Placemark" | "Style" | "StyleMap" => {}
            _ => collect_definitions(c, out),
        }
    }
}

/// Definitions by id, the first of an id winning.
fn index<'a>(defined: &[&'a Element]) -> Definitions<'a> {
    let mut by_id = Definitions::with_capacity(defined.len());
    for e in defined {
        if let Some(id) = e.attr("id") {
            by_id.entry(id).or_insert(*e);
        }
    }
    by_id
}

/// The member a relative link in document `from` names; `None` when it leaves the archive.
pub fn member_of(link: &str, from: &str) -> Option<String> {
    if link.contains("://") || link.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for part in link.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

/// The members a document's `styleUrl`s name, directly: the other documents to load to resolve
/// them. Only `.kml` members count.
pub fn linked_members(root: &Element, from: &str) -> Vec<String> {
    let mut urls = Vec::new();
    root.find_all("styleUrl", &mut urls);
    let mut out: Vec<String> = Vec::new();
    for url in urls {
        let Some((document, _)) = url.text.trim().split_once('#') else { continue };
        if !document.to_ascii_lowercase().ends_with(".kml") {
            continue;
        }
        if let Some(name) = member_of(document, from).filter(|n| !out.contains(n)) {
            out.push(name);
        }
    }
    out
}

/// Splits on `sep` outside quotes and parentheses.
fn split_top(s: &str, sep: char) -> Vec<&str> {
    let (mut out, mut depth, mut quoted, mut start) = (Vec::new(), 0i32, false, 0);
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => depth -= 1,
            c if c == sep && !quoted && depth == 0 => {
                out.push(&s[start..i]);
                start = i + c.len_utf8();
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out
}

/// The tools of an OGR style string: `(name, [(param, value)])`.
fn tools(style: &str) -> Vec<(String, Vec<(String, String)>)> {
    split_top(style, ';')
        .into_iter()
        .filter_map(|t| {
            let t = t.trim();
            let open = t.find('(')?;
            let body = t[open + 1..].strip_suffix(')')?;
            let params = split_top(body, ',')
                .into_iter()
                .filter_map(|p| {
                    let (k, v) = p.split_once(':')?;
                    Some((k.trim().to_ascii_lowercase(), v.trim().trim_matches('"').to_string()))
                })
                .collect();
            Some((t[..open].trim().to_ascii_uppercase(), params))
        })
        .collect()
}

/// The leading number of a value such as `2.5px`.
fn value_num(v: &str) -> Option<f64> {
    let end = v.find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+' || c == 'e' || c == 'E')).unwrap_or(v.len());
    v[..end].parse().ok()
}

/// The KML of a style: the body of a `<Style>` element (its sub-styles), empty when the string
/// sets nothing KML can hold. The OGR null brush (`BRUSH(id:"ogr-brush-1")`) and null pen
/// (`PEN(id:"ogr-pen-1")`) are written as PolyStyle `<fill>0</fill>` and `<outline>0</outline>`.
pub fn to_kml(style: &str) -> String {
    let mut s = String::new();
    let get = |p: &[(String, String)], k: &str| p.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let tools = tools(style);
    let null = |name: &str, id: &str| tools.iter().any(|(t, p)| t == name && get(p, "id").as_deref() == Some(id));
    let (null_pen, has_brush) = (null("PEN", "ogr-pen-1"), tools.iter().any(|(t, _)| t == "BRUSH"));
    for (tool, p) in &tools {
        let p = p.as_slice();
        match tool.as_str() {
            "SYMBOL" => {
                s.push_str("<IconStyle>");
                if let Some(c) = get(p, "c").and_then(|c| kml_color(&c)) {
                    let _ = write!(s, "<color>{c}</color>");
                }
                if let Some(v) = get(p, "s").and_then(|v| value_num(&v)) {
                    let _ = write!(s, "<scale>{v}</scale>");
                }
                if let Some(v) = get(p, "a").and_then(|v| value_num(&v)) {
                    let _ = write!(s, "<heading>{v}</heading>");
                }
                if let Some(id) = get(p, "id").filter(|id| !id.starts_with("ogr-sym-")) {
                    let _ = write!(s, "<Icon><href>{}</href></Icon>", xml::escape(&id));
                }
                if let (Some(x), Some(y)) = (get(p, "dx").and_then(|v| value_num(&v)), get(p, "dy").and_then(|v| value_num(&v))) {
                    let _ = write!(s, "<hotSpot x=\"{x}\" y=\"{y}\" xunits=\"fraction\" yunits=\"fraction\"/>");
                }
                s.push_str("</IconStyle>");
            }
            "LABEL" => {
                s.push_str("<LabelStyle>");
                if let Some(c) = get(p, "c").and_then(|c| kml_color(&c)) {
                    let _ = write!(s, "<color>{c}</color>");
                }
                if let Some(v) = get(p, "w").and_then(|v| value_num(&v)) {
                    let _ = write!(s, "<scale>{}</scale>", v / 100.0);
                }
                s.push_str("</LabelStyle>");
            }
            "PEN" => {
                s.push_str("<LineStyle>");
                if let Some(c) = get(p, "c").and_then(|c| kml_color(&c)) {
                    let _ = write!(s, "<color>{c}</color>");
                }
                if let Some(v) = get(p, "w").and_then(|v| value_num(&v)) {
                    let _ = write!(s, "<width>{v}</width>");
                }
                s.push_str("</LineStyle>");
                if null_pen && !has_brush {
                    s.push_str("<PolyStyle><outline>0</outline></PolyStyle>");
                }
            }
            "BRUSH" => {
                s.push_str("<PolyStyle>");
                if let Some(c) = get(p, "fc").and_then(|c| kml_color(&c)) {
                    let _ = write!(s, "<color>{c}</color>");
                }
                if get(p, "id").as_deref() == Some("ogr-brush-1") {
                    s.push_str("<fill>0</fill>");
                }
                if null_pen {
                    s.push_str("<outline>0</outline>");
                }
                s.push_str("</PolyStyle>");
            }
            _ => {}
        }
    }
    s
}

/// A `<Style>` element for each id of a style table that `wanted` accepts, one per line.
pub fn definitions(table: &[(String, String)], wanted: impl Fn(&str) -> bool) -> String {
    let mut s = String::new();
    for (id, style) in table.iter().filter(|(id, _)| wanted(id)) {
        let _ = writeln!(s, "<Style id=\"{}\">{}</Style>", xml::escape(id), to_kml(style));
    }
    s
}

/// Replaces each `@id` of the layers' `OGR_STYLE` fields with its definition in a style table.
pub fn resolve_references(layers: &mut [wbvector::Layer], table: &[(String, String)]) {
    for layer in layers {
        let Some(i) = layer.schema.field_index(FIELD) else { continue };
        for feature in &mut layer.features {
            let Some(wbvector::FieldValue::Text(value)) = feature.attributes.get_mut(i) else { continue };
            let definition = value.strip_prefix('@').and_then(|id| table.iter().find(|(k, _)| k == id));
            if let Some((_, style)) = definition {
                value.clone_from(style);
            }
        }
    }
}

/// Whether a style value refers to a style rather than describing one: `@name`, or another
/// document's URL.
pub fn is_reference(v: &str) -> bool {
    v.starts_with('@') || !v.contains('(')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_like_gdal() {
        let doc = xml::parse(
            r#"<kml><Document>
            <Style id="s1"><IconStyle><color>ff00ff00</color><scale>1.1</scale><heading>45</heading><Icon><href>http://x/i.png</href></Icon><hotSpot x="0.5" y="0" xunits="fraction" yunits="fraction"/></IconStyle><LabelStyle><color>ff0000ff</color><scale>1.5</scale></LabelStyle></Style>
            <Style id="s2"><LineStyle><color>7f00ffff</color><width>3.5</width></LineStyle><PolyStyle><color>4000ff00</color><fill>1</fill></PolyStyle></Style>
            <StyleMap id="m"><Pair><key>normal</key><styleUrl>#s2</styleUrl></Pair><Pair><key>highlight</key><styleUrl>#s1</styleUrl></Pair></StyleMap>
            </Document></kml>"#,
        )
        .unwrap();
        let members = Members::new();
        let t = Styles::new(&doc, "", &members).table();
        let get = |k: &str| t.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
        // As GDAL's LIBKML driver gives them.
        assert_eq!(
            get("s1"),
            Some(
                r#"SYMBOL(id:"http://x/i.png",a:45.000000,c:#00FF00FF,s:1.100000,dx:0.500000,dy:0.000000);LABEL(c:#FF0000FF,w:150.000000)"#
            )
        );
        assert_eq!(get("s2"), Some("PEN(c:#FFFF007F,w:3.500000px);BRUSH(fc:#00FF0040)"));
        assert_eq!(get("m"), get("s2"));
        // Back to KML, and again to the same string.
        let back = xml::parse(&format!("<Style>{}</Style>", to_kml(get("s1").unwrap()))).unwrap();
        assert_eq!(from_kml(&back).as_deref(), get("s1"));
        let back = xml::parse(&format!("<Style>{}</Style>", to_kml(get("s2").unwrap()))).unwrap();
        assert_eq!(from_kml(&back).as_deref(), get("s2"));
    }

    #[test]
    fn null_brush_and_pen_turn_fill_and_outline_off() {
        let body = to_kml(r#"PEN(c:#FF0000FF,w:2px,id:"ogr-pen-1");BRUSH(fc:#00FF007F,id:"ogr-brush-1")"#);
        let style = xml::parse(&format!("<Style>{body}</Style>")).unwrap();
        let poly = style.child("PolyStyle").unwrap();
        assert_eq!(
            (poly.child_text("color"), poly.child_text("fill"), poly.child_text("outline")),
            (Some("7f00ff00"), Some("0"), Some("0"))
        );
        assert_eq!(style.child("LineStyle").unwrap().child_text("width"), Some("2"));
        // A null pen alone still turns a polygon's outline off.
        assert!(to_kml(r#"PEN(id:"ogr-pen-1")"#).ends_with("<PolyStyle><outline>0</outline></PolyStyle>"));
        // A null brush beside an ordinary pen turns only the fill off; a null pen beside an
        // ordinary brush, only the outline.
        for (ogr, fill, outline) in [
            (r#"PEN(c:#FF0000FF);BRUSH(fc:#00FF007F,id:"ogr-brush-1")"#, Some("0"), None),
            (r#"PEN(id:"ogr-pen-1");BRUSH(fc:#00FF007F)"#, None, Some("0")),
        ] {
            let style = xml::parse(&format!("<Style>{}</Style>", to_kml(ogr))).unwrap();
            let poly = style.child("PolyStyle").unwrap();
            assert_eq!(
                (poly.child_text("color"), poly.child_text("fill"), poly.child_text("outline")),
                (Some("7f00ff00"), fill, outline),
                "{ogr}"
            );
        }
        // Other brushes and pens write neither flag.
        let plain = to_kml("PEN(c:#FF0000FF);BRUSH(fc:#00FF007F)");
        assert!(!plain.contains("<fill>") && !plain.contains("<outline>"), "{plain}");
    }

    /// The style of each placemark of a document.
    fn styles_of(kml: &str) -> Vec<Option<String>> {
        let doc = xml::parse(kml).unwrap();
        let members = Members::new();
        let styles = Styles::new(&doc, "", &members);
        let mut placemarks = Vec::new();
        doc.find_all("Placemark", &mut placemarks);
        placemarks.into_iter().map(|p| styles.of_feature(p)).collect()
    }

    #[test]
    fn fill_and_outline_flags_survive_reading_and_writing() {
        for (poly, expected) in [
            ("<PolyStyle><color>7f00ff00</color><fill>0</fill></PolyStyle>", r#"BRUSH(id:"ogr-brush-1")"#),
            ("<PolyStyle><fill>false</fill></PolyStyle>", r#"BRUSH(id:"ogr-brush-1")"#),
            ("<PolyStyle><outline>0</outline></PolyStyle>", r#"PEN(id:"ogr-pen-1")"#),
            ("<LineStyle><width>2</width></LineStyle><PolyStyle><outline>0</outline></PolyStyle>", r#"PEN(w:2.000000px,id:"ogr-pen-1")"#),
            ("<PolyStyle><color>7f00ff00</color><fill>1</fill><outline>1</outline></PolyStyle>", "BRUSH(fc:#00FF007F)"),
        ] {
            let style = xml::parse(&format!("<Style>{poly}</Style>")).unwrap();
            let ogr = from_kml(&style).unwrap();
            assert_eq!(ogr, expected);
            let back = xml::parse(&format!("<Style>{}</Style>", to_kml(&ogr))).unwrap();
            assert_eq!(from_kml(&back).as_deref(), Some(expected), "{poly}");
        }
    }

    #[test]
    fn a_reference_stays_one_and_inline_sub_styles_merge_over_it() {
        let got = styles_of(
            r##"<kml><Document>
            <Style id="base"><LineStyle><color>ff0000ff</color><width>2</width></LineStyle><PolyStyle><color>7f00ff00</color><fill>0</fill></PolyStyle></Style>
            <StyleMap id="map"><Pair><key>normal</key><styleUrl>#base</styleUrl></Pair></StyleMap>
            <Placemark><styleUrl>#base</styleUrl></Placemark>
            <Placemark><styleUrl>#map</styleUrl></Placemark>
            <Placemark><styleUrl>#base</styleUrl><Style><LineStyle><width>4</width></LineStyle></Style></Placemark>
            <Placemark><styleUrl>#base</styleUrl><Style><PolyStyle><fill>1</fill></PolyStyle></Style></Placemark>
            <Placemark><styleUrl>#base</styleUrl><Style/></Placemark>
            <Placemark><styleUrl>#missing</styleUrl></Placemark>
            <Placemark><styleUrl>#missing</styleUrl><Style><LineStyle><width>4</width></LineStyle></Style></Placemark>
            <Placemark><styleUrl>http://x/s.kml#a</styleUrl></Placemark>
            <Placemark/>
            </Document></kml>"##,
        );
        let get = |i: usize| got[i].as_deref();
        assert_eq!(get(0), Some("@base"));
        assert_eq!(get(1), Some("@map"));
        // The width is the placemark's; the color and the fill flag are the shared style's.
        assert_eq!(get(2), Some(r#"PEN(c:#FF0000FF,w:4.000000px);BRUSH(id:"ogr-brush-1")"#));
        // An inline flag turns the shared one back on, and the shared color with it.
        assert_eq!(get(3), Some("PEN(c:#FF0000FF,w:2.000000px);BRUSH(fc:#00FF007F)"));
        assert_eq!(get(4), Some("@base"));
        assert_eq!(get(5), Some("@missing"));
        assert_eq!(get(6), Some("PEN(w:4.000000px)"));
        assert_eq!(get(7), Some("http://x/s.kml#a"));
        assert_eq!(get(8), None);
    }

    #[test]
    fn style_maps_in_a_cycle_end() {
        let doc = xml::parse(
            r##"<kml><Document>
            <StyleMap id="a"><Pair><key>normal</key><styleUrl>#b</styleUrl></Pair></StyleMap>
            <StyleMap id="b"><Pair><key>normal</key><styleUrl>#a</styleUrl></Pair></StyleMap>
            </Document></kml>"##,
        )
        .unwrap();
        let members = Members::new();
        assert!(Styles::new(&doc, "", &members).table().is_empty());
    }

    #[test]
    fn links_resolve_relative_to_their_document() {
        assert_eq!(member_of("styles.kml", "doc.kml").as_deref(), Some("styles.kml"));
        assert_eq!(member_of("styles.kml", "layers/a.kml").as_deref(), Some("layers/styles.kml"));
        assert_eq!(member_of("../styles.kml", "layers/a.kml").as_deref(), Some("styles.kml"));
        assert_eq!(member_of("./s/styles.kml", "doc.kml").as_deref(), Some("s/styles.kml"));
        assert_eq!(member_of("../styles.kml", "doc.kml"), None);
        assert_eq!(member_of("http://x/styles.kml", "doc.kml"), None);
        assert_eq!(member_of("/styles.kml", "doc.kml"), None);
    }
}

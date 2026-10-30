//! KML styles and OGR feature style strings (`PEN(c:#FF0000FF,w:2.000000px);BRUSH(...)`), the
//! form GDAL gives feature styles in and keeps in an `OGR_STYLE` field.
//!
//! Reading converts `<Style>` elements as GDAL's LIBKML driver does: `LineStyle` to `PEN`,
//! `PolyStyle` to `BRUSH`, `IconStyle` to `SYMBOL` and `LabelStyle` to `LABEL`, colors from
//! KML's `aabbggrr` to `#RRGGBBAA`. A `StyleMap` stands for its `normal` style. Writing goes the
//! other way.

use super::xml::{self, Element};
use std::fmt::Write as _;

/// The name of the field holding feature styles.
pub const FIELD: &str = "OGR_STYLE";

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

fn num(e: &Element, name: &str) -> Option<f64> {
    e.child_text(name)?.parse().ok()
}

/// A `<Style>` element as an OGR style string; `None` when it sets nothing.
pub fn from_kml(style: &Element) -> Option<String> {
    let mut tools = Vec::new();
    if let Some(l) = style.child("LineStyle") {
        let mut p = Vec::new();
        if let Some(c) = l.child_text("color").and_then(color) {
            p.push(format!("c:{c}"));
        }
        if let Some(w) = num(l, "width") {
            p.push(format!("w:{w:.6}px"));
        }
        tools.push(("PEN", p));
    }
    if let Some(poly) = style.child("PolyStyle") {
        tools.push(("BRUSH", poly.child_text("color").and_then(color).map(|c| vec![format!("fc:{c}")]).unwrap_or_default()));
    }
    if let Some(i) = style.child("IconStyle") {
        let mut p = Vec::new();
        if let Some(href) = i.child("Icon").and_then(|ic| ic.child_text("href")) {
            p.push(format!("id:\"{href}\""));
        }
        if let Some(a) = num(i, "heading") {
            p.push(format!("a:{a:.6}"));
        }
        if let Some(c) = i.child_text("color").and_then(color) {
            p.push(format!("c:{c}"));
        }
        if let Some(s) = num(i, "scale") {
            p.push(format!("s:{s:.6}"));
        }
        if let Some(h) = i.child("hotSpot") {
            for (k, a) in [("dx", "x"), ("dy", "y")] {
                if let Some(v) = h.attr(a).and_then(|v| v.trim().parse::<f64>().ok()) {
                    p.push(format!("{k}:{v:.6}"));
                }
            }
        }
        tools.push(("SYMBOL", p));
    }
    if let Some(l) = style.child("LabelStyle") {
        let mut p = Vec::new();
        if let Some(c) = l.child_text("color").and_then(color) {
            p.push(format!("c:{c}"));
        }
        if let Some(s) = num(l, "scale") {
            p.push(format!("w:{:.6}", s * 100.0));
        }
        tools.push(("LABEL", p));
    }
    let s: Vec<String> = tools.into_iter().filter(|(_, p)| !p.is_empty()).map(|(t, p)| format!("{t}({})", p.join(","))).collect();
    (!s.is_empty()).then(|| s.join(";"))
}

/// The styles a document defines (`<Style>` and `<StyleMap>` with an id, anywhere), by id, as
/// OGR style strings. A style map stands for its `normal` style.
pub fn table(root: &Element) -> Vec<(String, String)> {
    let mut styles: Vec<(String, String)> = Vec::new();
    let mut maps: Vec<&Element> = Vec::new();
    fn walk<'a>(e: &'a Element, styles: &mut Vec<(String, String)>, maps: &mut Vec<&'a Element>) {
        for c in &e.children {
            match c.name.as_str() {
                "Style" => {
                    if let (Some(id), Some(s)) = (c.attr("id"), from_kml(c)) {
                        styles.push((id.to_string(), s));
                    }
                }
                "StyleMap" if c.attr("id").is_some() => maps.push(c),
                "Placemark" => {}
                _ => walk(c, styles, maps),
            }
        }
    }
    walk(root, &mut styles, &mut maps);
    for m in maps {
        let normal = m.children_named("Pair").find(|p| p.child_text("key") == Some("normal"));
        let s = normal.and_then(|p| match p.child("Style") {
            Some(st) => from_kml(st),
            None => {
                let url = p.child_text("styleUrl")?;
                let id = url.strip_prefix('#')?;
                styles.iter().find(|(k, _)| k == id).map(|(_, v)| v.clone())
            }
        });
        if let (Some(id), Some(s)) = (m.attr("id"), s) {
            styles.push((id.to_string(), s));
        }
    }
    styles
}

/// A placemark's style: its own `<Style>`, else its `styleUrl` resolved through the document's
/// styles (`@id` when the document does not define it; other documents' URLs as they are).
pub fn of_feature(f: &Element, table: &[(String, String)]) -> Option<String> {
    if let Some(s) = f.child("Style").and_then(from_kml) {
        return Some(s);
    }
    let url = f.child_text("styleUrl")?;
    match url.strip_prefix('#') {
        Some(id) => Some(table.iter().find(|(k, _)| k == id).map_or_else(|| format!("@{id}"), |(_, v)| v.clone())),
        None => Some(url.to_string()),
    }
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
        let t = table(&doc);
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
}

// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT
//! Opening a workbook to edit: the model's values, and everything else as it was written.

use super::super::datetime;
use super::super::xlsx::{
    CellKind, MAX_CELLS, MAX_PART_BYTES, MAX_SHEET_BYTES, MAX_WORKBOOK_ENTRIES, attribute, reference_text, resolve_target, serial_parts,
    style_kinds, take_cells, xml_error,
};
use super::{
    Align, BorderStyle, Cell, CellValue, Comment, Dimension, Format, Hyperlink, Orientation, Range, Relationship, SharedStrings,
    StyleTable, Validation, VerticalAlign, Workbook, Worksheet, invalid, parse_cell,
};
use crate::{Error, Result};
use quick_xml::events::{BytesStart, Event};
use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Seek};

/// What was read of a sheet that the model does not hold, or holds only in part.
#[derive(Debug, Clone)]
pub(super) struct SheetSource {
    pub(super) part: String,
    pub(super) rel_id: String,
    pub(super) sheet_id: String,
    /// The root element's start tag, with its namespaces.
    pub(super) root: String,
    /// The elements as they were read, by name, until an edit makes the model write them.
    pub(super) sections: BTreeMap<String, Vec<String>>,
    pub(super) row_attributes: BTreeMap<u32, String>,
    pub(super) relationships: Vec<Relationship>,
    /// The sheet-scoped defined names: name, hidden, reference.
    pub(super) names: Vec<(String, bool, String)>,
    /// Validations other than lists, and links inside the workbook, which the model does not hold.
    pub(super) other_validations: Vec<String>,
    pub(super) other_hyperlinks: Vec<String>,
    /// The elements' names, in the order they were read.
    pub(super) order: Vec<String>,
    /// The relationships of the links the model holds.
    pub(super) link_ids: Vec<String>,
    /// The sheet's properties as read, for what the model does not hold of them.
    pub(super) read_sheet_pr: Option<String>,
}

impl SheetSource {
    /// The relationship ids that elements kept as read refer to.
    pub(super) fn referenced_ids(&self) -> std::collections::HashSet<String> {
        let mut ids = std::collections::HashSet::new();
        for raw in self.sections.values().flatten() {
            let mut rest = raw.as_str();
            while let Some(i) = rest.find("r:id=") {
                rest = &rest[i + 5..];
                let Some(quote) = rest.chars().next().filter(|c| matches!(c, '"' | '\'')) else { continue };
                let value = &rest[quote.len_utf8()..];
                if let Some(end) = value.find(quote) {
                    ids.insert(value[..end].to_string());
                }
            }
        }
        ids
    }
}

/// The value of an attribute of the first element of a name in some XML.
pub(super) fn first_attribute(xml: &str, element: &str, name: &str) -> Option<String> {
    let mut reader = quick_xml::Reader::from_str(xml);
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == element => return attribute(&e, name),
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// A start tag written again with these attributes set, whatever their values were.
pub(super) fn start_with(xml: &str, set: &[(&str, &str)]) -> String {
    let Some(e) = first_start(xml) else { return xml.to_string() };
    let mut out = format!("<{}", e.name().as_ref());
    for attr in e.attributes().flatten() {
        if !set.iter().any(|(name, _)| *name == attr.key.as_ref()) {
            out.push_str(&format!(" {}=\"{}\"", attr.key.as_ref(), attr.value));
        }
    }
    for (name, value) in set {
        out.push_str(&format!(" {name}=\"{value}\""));
    }
    out.push('>');
    out
}

/// The attributes of the sheet properties read, and their children: what the model writes again
/// around its own.
pub(super) fn sheet_properties(xml: &str) -> (String, Vec<(String, String)>) {
    let attributes = first_start(xml)
        .map_or_else(String::new, |e| e.attributes().flatten().map(|a| format!(" {}=\"{}\"", a.key.as_ref(), a.value)).collect());
    let children = split_children(xml, "sheetPr").map(|(_, c)| c).unwrap_or_default();
    (attributes, children)
}

/// Whether a cell read holds the formula a range of cells shares.
pub(super) fn holds_shared_master(cell: &str) -> bool {
    let mut reader = quick_xml::Reader::from_str(cell);
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) if e.local_name().as_ref() == "f" => {
                return attribute(&e, "t").as_deref() == Some("shared") && attribute(&e, "ref").is_some();
            }
            Ok(Event::Eof) | Err(_) => return false,
            _ => {}
        }
    }
}

/// An empty element written again with an attribute set, whatever its value was.
pub(super) fn with_attribute(xml: &str, name: &str, value: &str) -> String {
    let Some(e) = first_start(xml) else { return xml.to_string() };
    let mut out = format!("<{}", e.name().as_ref());
    for attr in e.attributes().flatten() {
        if attr.key.as_ref() != name {
            out.push_str(&format!(" {}=\"{}\"", attr.key.as_ref(), attr.value));
        }
    }
    out.push_str(&format!(" {name}=\"{value}\"/>"));
    out
}

/// What was read of a stylesheet.
#[derive(Debug, Clone)]
pub(super) struct StyleSource {
    pub(super) root: String,
    pub(super) sections: BTreeMap<String, Vec<String>>,
    pub(super) num_formats: Vec<String>,
    /// The elements' names, in the order they were read.
    pub(super) order: Vec<String>,
    fonts: Vec<String>,
    fills: Vec<String>,
    borders: Vec<String>,
    xfs: Vec<String>,
    next_num_format: usize,
}

/// What was read of a package that the model does not hold.
#[derive(Debug, Clone)]
pub(super) struct PackageSource {
    /// The parts kept as they are.
    pub(super) parts: BTreeMap<String, Vec<u8>>,
    pub(super) defaults: Vec<(String, String)>,
    pub(super) overrides: Vec<(String, String)>,
    pub(super) workbook_part: String,
    pub(super) workbook_root: String,
    pub(super) workbook_sections: BTreeMap<String, Vec<String>>,
    pub(super) workbook_order: Vec<String>,
    pub(super) workbook_relationships: Vec<Relationship>,
    pub(super) sheet_parts: Vec<String>,
    /// Workbook-scoped names read: the name, the start tag with its attributes, and the reference.
    pub(super) other_names: Vec<(String, String, String)>,
    pub(super) core_part: Option<String>,
    core: Option<String>,
    styles: Option<StyleSource>,
    strings: Vec<String>,
    plain_strings: HashMap<String, usize>,
    strings_root: Option<String>,
}

impl PackageSource {
    /// The style table to add to, starting from what was read.
    pub(super) fn style_table(&self) -> StyleTable {
        let mut table = StyleTable::new();
        let Some(styles) = &self.styles else { return table };
        if !styles.fonts.is_empty() {
            table.fonts = styles.fonts.clone();
        }
        if !styles.fills.is_empty() {
            table.fills = styles.fills.clone();
        }
        if !styles.borders.is_empty() {
            table.borders = styles.borders.clone();
        }
        if !styles.xfs.is_empty() {
            table.xfs = styles.xfs.clone();
        }
        table.seen.clear();
        for (kind, list) in [("fonts", &table.fonts), ("fills", &table.fills), ("borders", &table.borders), ("xfs", &table.xfs)] {
            table.seen.extend(list.iter().enumerate().map(|(i, item)| ((kind, item.clone()), i)));
        }
        table.first_num_format = styles.next_num_format;
        table.source = Some(styles.clone());
        table
    }

    /// The shared strings to add to, starting from what was read.
    pub(super) fn shared_strings(&self) -> SharedStrings {
        SharedStrings { root: self.strings_root.clone(), items: self.strings.clone(), plain: self.plain_strings.clone(), count: 0 }
    }
}

/// A relationship's target as a part name, from the directory of the part it belongs to.
pub(super) fn resolve(base: &str, target: &str) -> String {
    resolve_target(base, target)
}

/// The properties part, with `set` replacing what was read: a child's local name and its XML.
pub(super) fn core_properties(source: Option<&PackageSource>, root: &str, set: &[(&str, String)]) -> String {
    let header = super::XML_HEADER;
    let Some((read_root, children)) = source.and_then(|s| s.core.as_deref()).and_then(|text| split_children(text, "core").ok()) else {
        let body: String = set.iter().map(|(_, xml)| format!("{xml}\n")).collect();
        return format!("{header}{root}\n{body}</cp:coreProperties>\n");
    };
    let close = read_root.name.clone();
    let mut start = read_root.start;
    for (prefix, uri) in [
        ("dc", "http://purl.org/dc/elements/1.1/"),
        ("dcterms", "http://purl.org/dc/terms/"),
        ("xsi", "http://www.w3.org/2001/XMLSchema-instance"),
    ] {
        start = with_namespace(start, prefix, uri);
    }
    let mut out = format!("{header}{start}\n");
    for (name, xml) in &children {
        if !set.iter().any(|(n, _)| n == name) {
            out.push_str(xml);
            out.push('\n');
        }
    }
    for (_, xml) in set {
        out.push_str(xml);
        out.push('\n');
    }
    out.push_str(&format!("</{close}>\n"));
    out
}

struct Root {
    /// The start tag, unchanged.
    start: String,
    /// The element's name with its prefix.
    name: String,
}

/// An XML document's root start tag and its children: their local names and text.
fn split_children(text: &str, part: &str) -> Result<(Root, Vec<(String, String)>)> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut root = None;
    let mut children = Vec::new();
    let (mut depth, mut child_start, mut child_name) = (0usize, 0usize, String::new());
    loop {
        let before = reader.buffer_position() as usize;
        let event = reader.read_event().map_err(|e| xml_error(part, e))?;
        let after = reader.buffer_position() as usize;
        match event {
            Event::Start(e) => {
                if root.is_none() {
                    root = Some(Root { start: text[before..after].to_string(), name: e.name().as_ref().to_string() });
                } else {
                    if depth == 1 {
                        (child_start, child_name) = (before, e.local_name().as_ref().to_string());
                    }
                    depth += 1;
                    continue;
                }
                depth = 1;
            }
            Event::Empty(e) => {
                if root.is_none() {
                    return Ok((Root { start: text[before..after].replace("/>", ">"), name: e.name().as_ref().to_string() }, children));
                }
                if depth == 1 {
                    children.push((e.local_name().as_ref().to_string(), text[before..after].to_string()));
                }
            }
            Event::End(_) => {
                depth = depth.saturating_sub(1);
                if depth == 1 {
                    children.push((std::mem::take(&mut child_name), text[child_start..after].to_string()));
                }
            }
            Event::Eof => {
                let root = root.ok_or_else(|| Error::InvalidData(format!("XLSX: '{part}' holds no element")))?;
                return Ok((root, children));
            }
            _ => {}
        }
    }
}

/// A root's start tag that declares a namespace prefix the model writes, if it does not yet: a
/// program may declare it only on the elements that use it.
fn with_namespace(start: String, prefix: &str, uri: &str) -> String {
    if start.contains(&format!("xmlns:{prefix}=")) {
        return start;
    }
    let end = start.rfind('>').unwrap_or(start.len());
    format!("{} xmlns:{prefix}=\"{uri}\"{}", start[..end].trim_end(), &start[end..])
}

/// Refuses a root that names its namespace by a prefix: the model writes elements without one.
fn plain_root(root: &Root, wanted: &str, part: &str) -> Result<()> {
    if root.name != wanted {
        return Err(Error::Unsupported(format!("XLSX: cannot edit '{part}', whose root is '{}', not '{wanted}'", root.name)));
    }
    Ok(())
}

fn sections_of(children: Vec<(String, String)>) -> BTreeMap<String, Vec<String>> {
    let mut sections: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, xml) in children {
        sections.entry(name).or_default().push(xml);
    }
    sections
}

/// The text inside an element, its references resolved.
fn element_text(xml: &str) -> String {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut out = String::new();
    while let Ok(event) = reader.read_event() {
        match event {
            Event::Text(t) => out.push_str(&t.xml10_content()),
            Event::CData(t) => out.push_str(&t.xml10_content()),
            Event::GeneralRef(r) => out.extend(reference_text(&r)),
            Event::Eof => break,
            _ => {}
        }
    }
    out
}

/// The attributes of an element that starts a piece of XML.
fn first_start(xml: &str) -> Option<BytesStart<'static>> {
    let mut reader = quick_xml::Reader::from_str(xml);
    loop {
        match reader.read_event().ok()? {
            Event::Start(e) | Event::Empty(e) => return Some(e.into_owned()),
            Event::Eof => return None,
            _ => {}
        }
    }
}

/// A part's bytes, within the limits on a part and on the workbook.
fn read_bytes<R: Read + Seek>(zip: &mut zip::ZipArchive<R>, index: usize, total: &mut u64) -> Result<(String, Vec<u8>)> {
    let mut file = zip.by_index(index)?;
    let name = file.name()?.to_string();
    let too_large = || Error::ResourceLimit(format!("XLSX: part '{name}' expands past the limit"));
    if file.size() > MAX_PART_BYTES {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.by_ref().take(MAX_PART_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_PART_BYTES {
        return Err(too_large());
    }
    *total += bytes.len() as u64;
    if *total > MAX_SHEET_BYTES {
        return Err(Error::ResourceLimit(format!("XLSX: the workbook expands to more than {MAX_SHEET_BYTES} bytes")));
    }
    Ok((name, bytes))
}

fn rels_of(text: &str, part: &str) -> Result<Vec<Relationship>> {
    let mut reader = quick_xml::Reader::from_str(text);
    let mut out = Vec::new();
    loop {
        match reader.read_event().map_err(|e| xml_error(part, e))? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "Relationship" => {
                if let (Some(id), Some(kind), Some(target)) = (attribute(&e, "Id"), attribute(&e, "Type"), attribute(&e, "Target")) {
                    if out.len() >= MAX_WORKBOOK_ENTRIES {
                        return Err(Error::ResourceLimit(format!("XLSX: '{part}' declares too many relationships")));
                    }
                    let external = attribute(&e, "TargetMode").is_some_and(|m| m.eq_ignore_ascii_case("external"));
                    out.push(Relationship { id, kind, target, external });
                }
            }
            Event::Eof => return Ok(out),
            _ => {}
        }
    }
}

fn rgb_of(e: &BytesStart<'_>) -> Option<u32> {
    let value = attribute(e, "rgb")?;
    u32::from_str_radix(value.get(value.len().checked_sub(6)?..)?, 16).ok()
}

/// A number format's code by its built-in id.
fn builtin_format(id: u32) -> Option<&'static str> {
    Some(match id {
        1 => "0",
        2 => "0.00",
        3 => "#,##0",
        4 => "#,##0.00",
        9 => "0%",
        10 => "0.00%",
        11 => "0.00E+00",
        14 => "mm-dd-yy",
        15 => "d-mmm-yy",
        16 => "d-mmm",
        17 => "mmm-yy",
        18 => "h:mm AM/PM",
        19 => "h:mm:ss AM/PM",
        20 => "h:mm",
        21 => "h:mm:ss",
        22 => "m/d/yy h:mm",
        49 => "@",
        _ => return None,
    })
}

/// Reads what a font element sets into a format.
fn apply_font(xml: &str, format: &mut Format) {
    let mut reader = quick_xml::Reader::from_str(xml);
    while let Ok(event) = reader.read_event() {
        match event {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                "b" => format.bold = attribute(&e, "val").is_none_or(|v| v != "0" && v != "false"),
                "i" => format.italic = attribute(&e, "val").is_none_or(|v| v != "0" && v != "false"),
                "u" => format.underline = attribute(&e, "val").is_none_or(|v| v != "none"),
                "sz" => {
                    format.font_size_tenths = attribute(&e, "val").and_then(|v| v.parse::<f64>().ok()).map(|v| (v * 10.0).round() as u32)
                }
                "name" => format.font_name = attribute(&e, "val"),
                "color" => format.font_color = rgb_of(&e),
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
}

fn apply_fill(xml: &str, format: &mut Format) {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut solid = false;
    while let Ok(event) = reader.read_event() {
        match event {
            Event::Start(e) | Event::Empty(e) => match e.local_name().as_ref() {
                "patternFill" => solid = attribute(&e, "patternType").as_deref() == Some("solid"),
                "fgColor" if solid => format.fill = rgb_of(&e),
                _ => {}
            },
            Event::Eof => break,
            _ => {}
        }
    }
}

fn apply_border(xml: &str, format: &mut Format) {
    let mut reader = quick_xml::Reader::from_str(xml);
    let mut side = None;
    while let Ok(event) = reader.read_event() {
        match event {
            Event::Start(e) | Event::Empty(e) => {
                let local = e.local_name();
                let index = ["left", "right", "top", "bottom"].iter().position(|s| *s == local.as_ref());
                if let Some(i) = index {
                    let style = attribute(&e, "style").and_then(|s| match s.as_str() {
                        "thin" => Some(BorderStyle::Thin),
                        "medium" => Some(BorderStyle::Medium),
                        "thick" => Some(BorderStyle::Thick),
                        "dashed" => Some(BorderStyle::Dashed),
                        "dotted" => Some(BorderStyle::Dotted),
                        "double" => Some(BorderStyle::Double),
                        _ => None,
                    });
                    format.borders[i] = style.map(|s| (s, None));
                    side = style.map(|_| i);
                } else if local.as_ref() == "color"
                    && let Some(i) = side
                    && let Some(border) = &mut format.borders[i]
                {
                    border.1 = rgb_of(&e);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
}

/// The format each cell style makes.
fn parse_formats(source: &StyleSource, codes: &HashMap<u32, String>) -> Vec<Format> {
    source
        .xfs
        .iter()
        .map(|xf| {
            let mut format = Format::default();
            let Some(e) = first_start(xf) else { return format };
            let id = |name: &str| attribute(&e, name).and_then(|v| v.parse::<usize>().ok());
            if let Some(font) = id("fontId").filter(|&i| i != 0).and_then(|i| source.fonts.get(i)) {
                apply_font(font, &mut format);
            }
            if let Some(fill) = id("fillId").filter(|&i| i != 0).and_then(|i| source.fills.get(i)) {
                apply_fill(fill, &mut format);
            }
            if let Some(border) = id("borderId").filter(|&i| i != 0).and_then(|i| source.borders.get(i)) {
                apply_border(border, &mut format);
            }
            if let Some(code) = id("numFmtId").map(|i| i as u32).filter(|&i| i != 0) {
                format.num_format = codes.get(&code).cloned().or_else(|| builtin_format(code).map(str::to_string));
            }
            let mut reader = quick_xml::Reader::from_str(xf);
            while let Ok(event) = reader.read_event() {
                match event {
                    Event::Start(c) | Event::Empty(c) => match c.local_name().as_ref() {
                        "alignment" => {
                            format.align = attribute(&c, "horizontal").and_then(|v| match v.as_str() {
                                "left" => Some(Align::Left),
                                "center" => Some(Align::Center),
                                "right" => Some(Align::Right),
                                "fill" => Some(Align::Fill),
                                "justify" => Some(Align::Justify),
                                _ => None,
                            });
                            format.vertical = attribute(&c, "vertical").and_then(|v| match v.as_str() {
                                "top" => Some(VerticalAlign::Top),
                                "center" => Some(VerticalAlign::Center),
                                "bottom" => Some(VerticalAlign::Bottom),
                                _ => None,
                            });
                            format.wrap = attribute(&c, "wrapText").is_some_and(|v| v == "1" || v == "true");
                            format.indent = attribute(&c, "indent").and_then(|v| v.parse().ok());
                            format.rotation = attribute(&c, "textRotation")
                                .and_then(|v| v.parse::<i16>().ok())
                                .filter(|v| *v <= 180)
                                .map(|v| if v > 90 { 90 - v } else { v });
                        }
                        "protection" => {
                            format.locked = attribute(&c, "locked").map(|v| v == "1" || v == "true");
                            format.formula_hidden = attribute(&c, "hidden").is_some_and(|v| v == "1" || v == "true");
                        }
                        _ => {}
                    },
                    Event::Eof => break,
                    _ => {}
                }
            }
            format
        })
        .collect()
}

/// The children of a list element such as `fonts`.
fn list_items(sections: &BTreeMap<String, Vec<String>>, name: &str) -> Result<Vec<String>> {
    let Some(raw) = sections.get(name).and_then(|r| r.first()) else { return Ok(Vec::new()) };
    if !raw.contains('>') || raw.trim_end().ends_with("/>") && !raw.contains("</") {
        return Ok(Vec::new());
    }
    Ok(split_children(raw, "styles.xml")?.1.into_iter().map(|(_, xml)| xml).collect())
}

fn read_styles(text: &str) -> Result<(StyleSource, Vec<Format>, Vec<CellKind>)> {
    let (root, children) = split_children(text, "styles.xml")?;
    plain_root(&root, "styleSheet", "styles.xml")?;
    let order: Vec<String> = children.iter().map(|(n, _)| n.clone()).collect();
    let sections = sections_of(children);
    let num_formats = list_items(&sections, "numFmts")?;
    let mut codes = HashMap::new();
    for item in &num_formats {
        if let Some(e) = first_start(item)
            && let (Some(id), Some(code)) = (attribute(&e, "numFmtId").and_then(|v| v.parse::<u32>().ok()), attribute(&e, "formatCode"))
        {
            codes.insert(id, code);
        }
    }
    let source = StyleSource {
        root: root.start,
        fonts: list_items(&sections, "fonts")?,
        fills: list_items(&sections, "fills")?,
        borders: list_items(&sections, "borders")?,
        xfs: list_items(&sections, "cellXfs")?,
        next_num_format: codes.keys().copied().max().map_or(164, |max| (max as usize + 1).max(164)),
        num_formats,
        order,
        sections: {
            let mut other = sections;
            for name in ["numFmts", "fonts", "fills", "borders", "cellXfs"] {
                other.remove(name);
            }
            other
        },
    };
    let formats = parse_formats(&source, &codes);
    Ok((source, formats, style_kinds(text)?))
}

/// A number cell's date or date-time, when its style says it is one.
fn dated(text: &str, kind: CellKind, date1904: bool) -> Option<CellValue> {
    let (days, millis) = serial_parts(text, date1904)?;
    match kind {
        CellKind::Date if millis == 0 => Some(CellValue::Date(datetime::format_date(days))),
        CellKind::Date | CellKind::DateTime => {
            let (seconds, ms) = (millis / 1000, millis % 1000);
            let fraction = if ms == 0 { String::new() } else { format!(".{ms:03}") };
            Some(CellValue::DateTime(format!(
                "{}T{:02}:{:02}:{:02}{fraction}",
                datetime::format_date(days),
                seconds / 3600,
                seconds / 60 % 60,
                seconds % 60
            )))
        }
        _ => None,
    }
}

struct ReadParts<'a> {
    strings: &'a [String],
    formats: &'a [Format],
    kinds: &'a [CellKind],
    date1904: bool,
}

/// A cell's value from its type and the text inside it.
fn cell_value(
    kind_attribute: Option<&str>,
    formula: Option<String>,
    value: Option<String>,
    inline: Option<String>,
    style: CellKind,
    parts: &ReadParts<'_>,
) -> Result<CellValue> {
    let read = match kind_attribute {
        Some("s") => match value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
            Some(v) => {
                let found = v.parse::<usize>().ok().and_then(|i| parts.strings.get(i));
                CellValue::Text(
                    found.cloned().ok_or_else(|| Error::InvalidData(format!("XLSX: shared string '{v}' is not in the workbook")))?,
                )
            }
            None => CellValue::Blank,
        },
        Some("inlineStr") => CellValue::Text(inline.unwrap_or_default()),
        Some("str") => value.map_or(CellValue::Blank, CellValue::Text),
        Some("b") => value.map_or(CellValue::Blank, |v| CellValue::Boolean(v.trim() != "0")),
        Some("e") => value.map_or(CellValue::Blank, CellValue::Error),
        _ => match value {
            None => CellValue::Blank,
            Some(v) => {
                dated(&v, style, parts.date1904).unwrap_or_else(|| v.trim().parse().map_or(CellValue::Text(v.clone()), CellValue::Number))
            }
        },
    };
    Ok(match formula {
        Some(formula) => CellValue::Formula { formula, cached: (read != CellValue::Blank).then(|| Box::new(read)) },
        None => read,
    })
}

/// Reads the rows and cells of a sheet's `sheetData`.
fn read_sheet_data(
    xml: &str,
    part: &str,
    sheet: &mut Worksheet,
    source: &mut SheetSource,
    parts: &ReadParts<'_>,
    budget: &mut usize,
) -> Result<()> {
    let mut reader = quick_xml::Reader::from_str(xml);
    let (mut row, mut column) = (0u32, 0u32);
    loop {
        let before = reader.buffer_position() as usize;
        match reader.read_event().map_err(|e| xml_error(part, e))? {
            Event::Start(e) | Event::Empty(e) if e.local_name().as_ref() == "row" => {
                take_cells(budget, 4)?;
                row = attribute(&e, "r").and_then(|v| v.parse().ok()).unwrap_or(row + 1);
                if row == 0 || row as usize > super::MAX_ROWS {
                    return Err(Error::ResourceLimit(format!("XLSX: '{part}' holds a row past a sheet's limit")));
                }
                column = 0;
                let mut kept = String::new();
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() != "r" {
                        kept.push(' ');
                        kept.push_str(attr.key.as_ref());
                        kept.push_str("=\"");
                        kept.push_str(&attr.value);
                        kept.push('"');
                    }
                }
                source.row_attributes.insert(row, kept);
                let number = |name: &str| attribute(&e, name).and_then(|v| v.parse::<f64>().ok());
                let hidden = attribute(&e, "hidden").is_some_and(|v| v == "1" || v == "true");
                let outline = attribute(&e, "outlineLevel").and_then(|v| v.parse().ok()).unwrap_or(0);
                let style = attribute(&e, "s").and_then(|v| v.parse::<usize>().ok()).and_then(|s| parts.formats.get(s));
                if number("ht").is_some() || hidden || outline > 0 || style.is_some() {
                    sheet.rows.insert(row, Dimension { size: number("ht"), hidden, outline, format: style.cloned() });
                }
            }
            event @ (Event::Start(_) | Event::Empty(_)) => {
                let empty = matches!(event, Event::Empty(_));
                let (Event::Start(e) | Event::Empty(e)) = event else { unreachable!() };
                if e.local_name().as_ref() != "c" {
                    continue;
                }
                take_cells(budget, 1)?;
                let reference = attribute(&e, "r");
                let at = match reference.as_deref().map(parse_cell).transpose() {
                    Ok(Some((r, c))) => (r, c),
                    Ok(None) => (row, column + 1),
                    Err(_) => return Err(Error::InvalidData(format!("XLSX: '{part}' has a cell at '{}'", reference.unwrap_or_default()))),
                };
                if at.1 <= column || at.0 != row {
                    return Err(Error::InvalidData(format!("XLSX: '{part}' has a cell out of order at row {}", at.0)));
                }
                column = at.1;
                let kind_attribute = attribute(&e, "t");
                let style_index = attribute(&e, "s").and_then(|v| v.parse::<usize>().ok()).unwrap_or(0);
                let (mut formula, mut value, mut inline) = (None, None, None);
                if !empty {
                    let mut depth = 1usize;
                    let (mut in_formula, mut in_value, mut in_inline, mut phonetic) = (false, false, false, false);
                    while depth > 0 {
                        match reader.read_event().map_err(|e| xml_error(part, e))? {
                            Event::Start(c) => {
                                depth += 1;
                                match c.local_name().as_ref() {
                                    "f" => {
                                        in_formula = true;
                                        formula.get_or_insert_with(String::new);
                                    }
                                    "v" => {
                                        in_value = true;
                                        value.get_or_insert_with(String::new);
                                    }
                                    "rPh" => phonetic = true,
                                    "t" if !phonetic => {
                                        in_inline = true;
                                        inline.get_or_insert_with(String::new);
                                    }
                                    _ => {}
                                }
                            }
                            Event::Empty(c) if c.local_name().as_ref() == "f" => {
                                formula.get_or_insert_with(String::new);
                            }
                            Event::End(c) => {
                                depth -= 1;
                                match c.local_name().as_ref() {
                                    "f" => in_formula = false,
                                    "v" => in_value = false,
                                    "t" => in_inline = false,
                                    "rPh" => phonetic = false,
                                    _ => {}
                                }
                            }
                            event @ (Event::Text(_) | Event::CData(_) | Event::GeneralRef(_)) => {
                                let text = match event {
                                    Event::Text(t) => t.xml10_content().into_owned(),
                                    Event::CData(t) => t.xml10_content().into_owned(),
                                    Event::GeneralRef(r) => reference_text(&r).map(String::from).unwrap_or_default(),
                                    _ => unreachable!(),
                                };
                                let target = if in_formula {
                                    &mut formula
                                } else if in_value {
                                    &mut value
                                } else if in_inline {
                                    &mut inline
                                } else {
                                    continue;
                                };
                                target.get_or_insert_with(String::new).push_str(&text);
                            }
                            Event::Eof => return Err(Error::InvalidData(format!("XLSX: '{part}' ends inside a cell"))),
                            _ => {}
                        }
                    }
                }
                let after = reader.buffer_position() as usize;
                let style = parts.kinds.get(style_index).copied().unwrap_or(CellKind::General);
                let value = cell_value(kind_attribute.as_deref(), formula, value, inline, style, parts)?;
                let format = parts.formats.get(style_index).cloned().unwrap_or_default();
                // A cell without its reference is written from the model, which adds it.
                let raw = reference.is_some().then(|| xml[before..after].to_string());
                sheet.cells.insert(at, Cell { value, format, raw });
            }
            Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

/// Reads the settings the model holds from a sheet's other elements.
fn read_settings(sheet: &mut Worksheet, source: &mut SheetSource, part: &str, formats: &[Format], budget: &mut usize) -> Result<()> {
    let sections = source.sections.clone();
    let each = |name: &str, f: &mut dyn FnMut(&BytesStart<'_>, &str)| -> Result<()> {
        for raw in sections.get(name).into_iter().flatten() {
            let mut reader = quick_xml::Reader::from_str(raw);
            loop {
                let before = reader.buffer_position() as usize;
                match reader.read_event().map_err(|e| xml_error(part, e))? {
                    Event::Start(e) | Event::Empty(e) => f(&e, &raw[before..]),
                    Event::Eof => break,
                    _ => {}
                }
            }
        }
        Ok(())
    };
    each("sheetPr", &mut |e, _| match e.local_name().as_ref() {
        "tabColor" => sheet.tab_color = rgb_of(e),
        "pageSetUpPr" if attribute(e, "fitToPage").is_some_and(|v| v == "1" || v == "true") => sheet.print.fit_to = Some((1, 1)),
        _ => {}
    })?;
    each("sheetViews", &mut |e, _| match e.local_name().as_ref() {
        "sheetView" => {
            sheet.zoom = attribute(e, "zoomScale").and_then(|v| v.parse().ok());
            sheet.show_gridlines = attribute(e, "showGridLines").is_none_or(|v| v != "0" && v != "false");
        }
        "pane" if attribute(e, "state").is_some_and(|v| v.starts_with("frozen")) => {
            let split = |name: &str| attribute(e, name).and_then(|v| v.parse::<f64>().ok()).map_or(0, |v| v as u32);
            sheet.frozen = Some((split("ySplit"), split("xSplit")));
        }
        _ => {}
    })?;
    let mut over_budget = None;
    each("cols", &mut |e, _| {
        if e.local_name().as_ref() != "col" || over_budget.is_some() {
            return;
        }
        let bound = |name: &str| attribute(e, name).and_then(|v| v.parse::<u32>().ok());
        let (Some(min), Some(max)) = (bound("min"), bound("max")) else { return };
        let max = max.min(super::MAX_COLUMNS as u32);
        // Each column of a range is held: the work is charged before it is done.
        if let Err(e) = take_cells(budget, max.saturating_sub(min) as usize + 1) {
            over_budget = Some(e);
            return;
        }
        let width = attribute(e, "width").and_then(|v| v.parse().ok());
        let hidden = attribute(e, "hidden").is_some_and(|v| v == "1" || v == "true");
        let outline = attribute(e, "outlineLevel").and_then(|v| v.parse().ok()).unwrap_or(0);
        let format = attribute(e, "style").and_then(|v| v.parse::<usize>().ok()).and_then(|s| formats.get(s)).cloned();
        for column in min..=max {
            sheet.columns.insert(column, Dimension { size: width, hidden, outline, format: format.clone() });
        }
    })?;
    if let Some(e) = over_budget {
        return Err(e);
    }
    each("autoFilter", &mut |e, _| {
        if e.local_name().as_ref() == "autoFilter" {
            sheet.autofilter = attribute(e, "ref").and_then(|r| Range::parse(&r).ok());
        }
    })?;
    each("mergeCells", &mut |e, _| {
        if e.local_name().as_ref() == "mergeCell"
            && let Some(range) = attribute(e, "ref").and_then(|r| Range::parse(&r).ok())
        {
            sheet.merges.push(range);
        }
    })?;
    take_cells(budget, sheet.merges.len())?;
    each("pageMargins", &mut |e, _| {
        let side = |name: &str| attribute(e, name).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        sheet.print.margins = Some([side("left"), side("right"), side("top"), side("bottom"), side("header"), side("footer")]);
    })?;
    each("pageSetup", &mut |e, _| {
        sheet.print.orientation = attribute(e, "orientation").and_then(|v| match v.as_str() {
            "landscape" => Some(Orientation::Landscape),
            "portrait" => Some(Orientation::Portrait),
            _ => None,
        });
        sheet.print.paper_size = attribute(e, "paperSize").and_then(|v| v.parse().ok());
        if sheet.print.fit_to.is_some() {
            let pages = |name: &str| attribute(e, name).and_then(|v| v.parse().ok()).unwrap_or(1);
            sheet.print.fit_to = Some((pages("fitToWidth"), pages("fitToHeight")));
        }
    })?;
    for raw in sections.get("headerFooter").into_iter().flatten() {
        for (name, xml) in split_children(raw, part)?.1 {
            match name.as_str() {
                "oddHeader" => sheet.print.header = Some(element_text(&xml)),
                "oddFooter" => sheet.print.footer = Some(element_text(&xml)),
                _ => {}
            }
        }
    }
    for raw in sections.get("dataValidations").into_iter().flatten() {
        for (_, xml) in split_children(raw, part)?.1 {
            let list = first_start(&xml).and_then(|e| {
                if attribute(&e, "type").as_deref() != Some("list") {
                    return None;
                }
                let range = attribute(&e, "sqref").and_then(|r| Range::parse(&r).ok())?;
                let formula = split_children(&xml, part).ok()?.1.into_iter().find(|(n, _)| n == "formula1")?.1;
                let text = element_text(&formula);
                let inner = text.strip_prefix('"')?.strip_suffix('"')?;
                Some(Validation { range, items: inner.replace("\"\"", "\"").split(',').map(str::to_string).collect() })
            });
            match list {
                Some(v) => sheet.validations.push(v),
                None => source.other_validations.push(xml),
            }
        }
    }
    for raw in sections.get("hyperlinks").into_iter().flatten() {
        for (_, xml) in split_children(raw, part)?.1 {
            let link = first_start(&xml).and_then(|e| {
                let id = attribute(&e, "id")?;
                let cell = parse_cell(attribute(&e, "ref")?.split(':').next()?).ok()?;
                let url = source.relationships.iter().find(|r| r.id == id && r.external)?.target.clone();
                Some((Hyperlink { cell, url }, id))
            });
            match link {
                Some((link, id)) => {
                    sheet.hyperlinks.push(link);
                    source.link_ids.push(id);
                }
                None => source.other_hyperlinks.push(xml),
            }
        }
    }
    Ok(())
}

/// The comments of a comments part.
fn read_comments(text: &str, part: &str) -> Result<Vec<Comment>> {
    let (_, children) = split_children(text, part)?;
    let mut authors = Vec::new();
    let mut comments = Vec::new();
    for (name, xml) in children {
        match name.as_str() {
            "authors" => authors = split_children(&xml, part)?.1.into_iter().map(|(_, a)| element_text(&a)).collect(),
            "commentList" => {
                for (_, comment) in split_children(&xml, part)?.1 {
                    let Some(e) = first_start(&comment) else { continue };
                    let Some(cell) = attribute(&e, "ref").and_then(|r| parse_cell(&r).ok()) else { continue };
                    let author = attribute(&e, "authorId")
                        .and_then(|v| v.parse::<usize>().ok())
                        .and_then(|i| authors.get(i).cloned())
                        .unwrap_or_default();
                    comments.push(Comment { cell, text: element_text(&comment), author });
                }
            }
            _ => {}
        }
    }
    Ok(comments)
}

impl Workbook {
    /// Opens a workbook file to edit.
    pub fn open(path: &str) -> Result<Self> {
        Self::open_bytes(&std::fs::read(path)?)
    }

    /// Opens a workbook held in memory to edit. What the model does not hold is kept and written
    /// back: other parts, a sheet's other elements, and cells and settings that are not edited. A
    /// workbook past the limits on parts, cells or bytes is refused.
    pub fn open_bytes(bytes: &[u8]) -> Result<Self> {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
        if zip.len() > MAX_WORKBOOK_ENTRIES {
            return Err(Error::ResourceLimit(format!("XLSX: the archive holds more than {MAX_WORKBOOK_ENTRIES} parts")));
        }
        let mut total = 0u64;
        let mut all: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        for index in 0..zip.len() {
            let (name, data) = read_bytes(&mut zip, index, &mut total)?;
            if !name.ends_with('/') {
                all.insert(name, data);
            }
        }
        let text = |all: &BTreeMap<String, Vec<u8>>, name: &str| all.get(name).map(|b| decode(b));
        let package = text(&all, "_rels/.rels").map(|t| rels_of(&t, "_rels/.rels")).transpose()?.unwrap_or_default();
        let workbook_part = package
            .iter()
            .find(|r| r.kind.ends_with("/officeDocument"))
            .map_or_else(|| "xl/workbook.xml".to_string(), |r| resolve_target("", &r.target));
        let book = text(&all, &workbook_part).ok_or_else(|| Error::InvalidData("XLSX: the archive holds no workbook".into()))?;
        let base = workbook_part.rsplit_once('/').map_or(String::new(), |(d, _)| format!("{d}/"));
        let book_rels_part = super::rels_part(&workbook_part);
        let workbook_relationships = text(&all, &book_rels_part).map(|t| rels_of(&t, &book_rels_part)).transpose()?.unwrap_or_default();
        let (root, children) = split_children(&book, &workbook_part)?;
        plain_root(&root, "workbook", &workbook_part)?;
        let workbook_order: Vec<String> = children.iter().map(|(n, _)| n.clone()).collect();
        let workbook_sections = sections_of(children);
        let date1904 = workbook_sections
            .get("workbookPr")
            .and_then(|r| r.first())
            .and_then(|x| first_start(x))
            .and_then(|e| attribute(&e, "date1904"))
            .is_some_and(|v| v == "1" || v == "true");

        let mut budget = MAX_CELLS;
        let find =
            |suffix: &str| workbook_relationships.iter().find(|r| r.kind.ends_with(suffix)).map(|r| resolve_target(&base, &r.target));
        // Strings as read keep their rich text; the plain ones are shared with new cells.
        let strings_part = find("/sharedStrings");
        let (mut raw_strings, mut plain_strings, mut strings) = (Vec::new(), HashMap::new(), Vec::new());
        let mut strings_root = None;
        if let Some(sst) = strings_part.as_deref().and_then(|p| text(&all, p)) {
            let (sst_root, items) = split_children(&sst, "sharedStrings.xml")?;
            plain_root(&sst_root, "sst", "sharedStrings.xml")?;
            strings_root = Some(sst_root.start);
            for (i, (_, si)) in items.into_iter().enumerate() {
                take_cells(&mut budget, 1 + si.len() / 32)?;
                let mut reader = quick_xml::Reader::from_str(&si);
                let (mut value, mut phonetic, mut rich) = (String::new(), 0usize, false);
                while let Ok(event) = reader.read_event() {
                    match event {
                        Event::Start(e) => match e.local_name().as_ref() {
                            "rPh" => phonetic += 1,
                            "r" => rich = true,
                            _ => {}
                        },
                        Event::End(e) if e.local_name().as_ref() == "rPh" => phonetic = phonetic.saturating_sub(1),
                        Event::Text(t) if phonetic == 0 => value.push_str(&t.xml10_content()),
                        Event::CData(t) if phonetic == 0 => value.push_str(&t.xml10_content()),
                        Event::GeneralRef(r) if phonetic == 0 => value.extend(reference_text(&r)),
                        Event::Eof => break,
                        _ => {}
                    }
                }
                if !rich && !si.contains("rPh") {
                    plain_strings.entry(value.clone()).or_insert(i);
                }
                strings.push(value);
                raw_strings.push(si);
            }
        }
        let styles_part = find("/styles");
        let (styles, formats, kinds) = match styles_part.as_deref().and_then(|p| text(&all, p)) {
            Some(xml) => {
                let (source, formats, kinds) = read_styles(&xml)?;
                (Some(source), formats, kinds)
            }
            None => (None, Vec::new(), Vec::new()),
        };
        let parts = ReadParts { strings: &strings, formats: &formats, kinds: &kinds, date1904 };

        // The defined names, those of a sheet going with it.
        let mut local_names: HashMap<usize, Vec<(String, bool, String)>> = HashMap::new();
        let mut other_names = Vec::new();
        for raw in workbook_sections.get("definedNames").into_iter().flatten() {
            for (_, xml) in split_children(raw, &workbook_part)?.1 {
                let Some(e) = first_start(&xml) else { continue };
                let name = attribute(&e, "name").unwrap_or_default();
                let reference = element_text(&xml);
                match attribute(&e, "localSheetId").and_then(|v| v.parse::<usize>().ok()) {
                    Some(sheet) => local_names.entry(sheet).or_default().push((
                        name,
                        attribute(&e, "hidden").is_some_and(|v| v == "1" || v == "true"),
                        reference,
                    )),
                    None => {
                        let start = xml.find('>').map_or(xml.clone(), |i| xml[..=i].replace("/>", ">"));
                        other_names.push((name, start, reference));
                    }
                }
            }
        }

        let mut workbook = Workbook { date1904, ..Workbook::default() };
        let mut modelled: Vec<String> = vec!["[Content_Types].xml".into(), workbook_part.clone(), book_rels_part];
        modelled.extend(strings_part.clone());
        modelled.extend(styles_part.clone());
        // Excel builds the calculation chain again, and an edit could leave it wrong.
        modelled.extend(find("/calcChain"));
        let mut sheet_parts = Vec::new();
        let mut index = 0usize;
        for raw in workbook_sections.get("sheets").into_iter().flatten() {
            for (_, xml) in split_children(raw, &workbook_part)?.1 {
                let Some(e) = first_start(&xml) else { continue };
                let (Some(name), Some(rel_id)) = (attribute(&e, "name"), attribute(&e, "id")) else { continue };
                let sheet_id = attribute(&e, "sheetId").unwrap_or_else(|| (index + 1).to_string());
                let target = workbook_relationships.iter().find(|r| r.id == rel_id).filter(|r| r.kind.ends_with("/worksheet"));
                let Some(part) = target.map(|r| resolve_target(&base, &r.target)) else {
                    return Err(Error::Unsupported(format!("XLSX: cannot edit '{name}', which is not a worksheet")));
                };
                let sheet_text = text(&all, &part).ok_or_else(|| Error::InvalidData(format!("XLSX: the workbook lacks part '{part}'")))?;
                let rels_name = super::rels_part(&part);
                let relationships = text(&all, &rels_name).map(|t| rels_of(&t, &rels_name)).transpose()?.unwrap_or_default();
                let (root, children) = split_children(&sheet_text, &part)?;
                plain_root(&root, "worksheet", &part)?;
                let order: Vec<String> = children.iter().map(|(n, _)| n.clone()).filter(|n| n != "dimension").collect();
                let mut sections = sections_of(children);
                let data = sections.remove("sheetData");
                // Written again from what the sheet holds.
                sections.remove("dimension");
                let mut sheet = Worksheet::new(name);
                let state = attribute(&e, "state");
                sheet.hidden = state.as_deref().is_some_and(|s| s == "hidden" || s == "veryHidden");
                sheet.very_hidden = state.as_deref() == Some("veryHidden");
                let mut source = SheetSource {
                    part: part.clone(),
                    rel_id,
                    sheet_id,
                    root: with_namespace(root.start, "r", super::OD_RELS),
                    sections,
                    row_attributes: BTreeMap::new(),
                    relationships,
                    names: local_names.remove(&index).unwrap_or_default(),
                    other_validations: Vec::new(),
                    other_hyperlinks: Vec::new(),
                    order,
                    link_ids: Vec::new(),
                    read_sheet_pr: None,
                };
                for xml in data.iter().flatten() {
                    read_sheet_data(xml, &part, &mut sheet, &mut source, &parts, &mut budget)?;
                }
                read_settings(&mut sheet, &mut source, &part, &formats, &mut budget)?;
                source.read_sheet_pr = source.sections.get("sheetPr").and_then(|r| r.first()).cloned();
                let sheet_base = part.rsplit_once('/').map_or(String::new(), |(d, _)| format!("{d}/"));
                for r in source.relationships.iter().filter(|r| r.kind.ends_with("/comments")) {
                    let comments_part = resolve_target(&sheet_base, &r.target);
                    if let Some(xml) = text(&all, &comments_part) {
                        sheet.comments = read_comments(&xml, &comments_part)?;
                    }
                }
                for (defined, _, reference) in &source.names {
                    let range = reference.rsplit('!').next().and_then(|r| Range::parse(r).ok());
                    if defined == "_xlnm.Print_Area" {
                        sheet.print.area = range;
                    }
                }
                sheet.source = Some(source);
                modelled.push(part.clone());
                modelled.push(rels_name);
                sheet_parts.push(part);
                workbook.sheets.push(sheet);
                index += 1;
            }
        }
        if workbook.sheets.len() > MAX_WORKBOOK_ENTRIES - 2 {
            return Err(Error::ResourceLimit("XLSX: the workbook declares too many sheets".into()));
        }

        let content =
            text(&all, "[Content_Types].xml").ok_or_else(|| Error::InvalidData("XLSX: the archive holds no content types".into()))?;
        let (mut defaults, mut overrides) = (Vec::new(), Vec::new());
        for (name, xml) in split_children(&content, "[Content_Types].xml")?.1 {
            let Some(e) = first_start(&xml) else { continue };
            match (name.as_str(), attribute(&e, "Extension"), attribute(&e, "PartName"), attribute(&e, "ContentType")) {
                ("Default", Some(extension), _, Some(kind)) => defaults.push((extension, kind)),
                ("Override", _, Some(part), Some(kind)) => overrides.push((part, kind)),
                _ => {}
            }
        }
        if let Some(calc) = find("/calcChain") {
            overrides.retain(|(part, _)| part.trim_start_matches('/') != calc);
        }
        let calc_kind = "/calcChain";
        let workbook_relationships: Vec<Relationship> =
            workbook_relationships.into_iter().filter(|r| !r.kind.ends_with(calc_kind)).collect();
        let core_part = package.iter().find(|r| r.kind.ends_with("/core-properties")).map(|r| resolve_target("", &r.target));
        let core = core_part.as_deref().and_then(|p| text(&all, p));
        modelled.extend(core_part.clone());
        // The sheet parts of `overrides` stay; other modelled parts are written from the model.
        for name in &modelled {
            all.remove(name);
        }
        workbook.source = Some(Box::new(PackageSource {
            parts: all,
            defaults,
            overrides,
            workbook_part,
            workbook_root: with_namespace(root.start, "r", super::OD_RELS),
            workbook_sections,
            workbook_order,
            workbook_relationships,
            sheet_parts,
            other_names,
            core_part,
            core,
            styles,
            strings: raw_strings,
            plain_strings,
            strings_root,
        }));
        Ok(workbook)
    }

    /// A sheet's table as a layer, read as the XLSX driver reads it with these options. The workbook
    /// is written to memory to be read, so this costs a save.
    pub fn layer(&self, sheet: &str, options: &crate::XlsxOptions) -> Result<wbvector::Layer> {
        let name = self.sheet(sheet)?.name.clone();
        let mut layers = super::super::xlsx::parse(self.to_bytes()?, std::slice::from_ref(&name), options)?;
        layers.drain(..).find(|l| l.name == name).ok_or_else(|| invalid(format!("no sheet named '{name}'")))
    }

    /// Adds a sheet holding a layer's table: a header row of its field names, then its features, with
    /// geometries as the options' `GEOMETRY` asks. Returns the new sheet to lay out further.
    pub fn add_layer(&mut self, layer: &wbvector::Layer, options: &crate::XlsxOptions) -> Result<&mut Worksheet> {
        use wbvector::FieldValue;
        let extra = super::super::xlsx::geometry_header(layer, options)?;
        let names: Vec<&str> = extra.iter().copied().chain(layer.schema.fields().iter().map(|f| f.name.as_str())).collect();
        self.check_new_name(&layer.name)?;
        // Built whole before it joins the workbook, so that a refusal leaves no part of it.
        let mut built = Worksheet::new(layer.name.clone());
        let sheet = &mut built;
        let plain = Format::new();
        for (i, name) in names.iter().enumerate() {
            sheet.write_at(1, i as u32 + 1, *name, &plain)?;
        }
        for (row, feature) in layer.features.iter().enumerate() {
            let geometry = super::super::xlsx::geometry_values(feature, &extra);
            for (column, value) in geometry.iter().chain(feature.attributes.iter()).enumerate() {
                let value = match value {
                    FieldValue::Null => continue,
                    FieldValue::Integer(v) => CellValue::Number(*v as f64),
                    FieldValue::Float(v) if v.is_finite() => CellValue::Number(*v),
                    FieldValue::Float(_) => continue,
                    FieldValue::Boolean(v) => CellValue::Boolean(*v),
                    FieldValue::Text(v) => CellValue::Text(v.clone()),
                    FieldValue::Date(v) => CellValue::Date(v.clone()),
                    FieldValue::DateTime(v) => CellValue::DateTime(v.clone()),
                    FieldValue::Blob(v) => CellValue::Text(v.iter().map(|b| format!("{b:02X}")).collect()),
                };
                sheet.write_at(row as u32 + 2, column as u32 + 1, value, &plain)?;
            }
        }
        self.sheets.push(built);
        Ok(self.sheets.last_mut().expect("a sheet was just added"))
    }
}

fn decode(bytes: &[u8]) -> String {
    super::super::xml::decode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn part(bytes: &[u8], name: &str) -> Option<Vec<u8>> {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
        let mut out = Vec::new();
        zip.by_name(name).ok()?.read_to_end(&mut out).unwrap();
        Some(out)
    }

    fn text_part(bytes: &[u8], name: &str) -> String {
        String::from_utf8(part(bytes, name).unwrap_or_else(|| panic!("{name}"))).unwrap()
    }

    /// A workbook as a spreadsheet program writes it, with what the model does not hold.
    fn foreign_workbook() -> Vec<u8> {
        const NS: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
        const R: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
        const P: &str = "http://schemas.openxmlformats.org/package/2006/relationships";
        let parts: Vec<(&str, String)> = vec![
            ("[Content_Types].xml", "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.ms-excel.sheet.macroEnabled.main+xml\"/><Override PartName=\"/xl/calcChain.xml\" ContentType=\"application/x\"/><Override PartName=\"/xl/charts/chart1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.drawingml.chart+xml\"/></Types>".to_string()),
            ("_rels/.rels", format!("<Relationships xmlns=\"{P}\"><Relationship Id=\"rId1\" Type=\"{R}/officeDocument\" Target=\"xl/workbook.xml\"/><Relationship Id=\"rId2\" Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\"/></Relationships>")),
            ("docProps/core.xml", "<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:title>Old</dc:title><dc:subject>Keep</dc:subject></cp:coreProperties>".to_string()),
            ("xl/workbook.xml", format!("<workbook xmlns=\"{NS}\" xmlns:r=\"{R}\"><workbookPr codeName=\"Book\"/><bookViews><workbookView activeTab=\"1\"/></bookViews><sheets><sheet name=\"Data\" sheetId=\"7\" r:id=\"rId1\"/><sheet name=\"Other\" sheetId=\"9\" state=\"veryHidden\" r:id=\"rId2\"/></sheets><definedNames><definedName name=\"_xlnm.Print_Area\" localSheetId=\"0\">Data!$A$1:$B$3</definedName><definedName name=\"rate\" comment=\"c\">Data!$B$2</definedName></definedNames><calcPr calcId=\"191029\"/></workbook>")),
            ("xl/_rels/workbook.xml.rels", format!("<Relationships xmlns=\"{P}\"><Relationship Id=\"rId1\" Type=\"{R}/worksheet\" Target=\"worksheets/sheet1.xml\"/><Relationship Id=\"rId2\" Type=\"{R}/worksheet\" Target=\"/xl/worksheets/sheet2.xml\"/><Relationship Id=\"rId3\" Type=\"{R}/styles\" Target=\"styles.xml\"/><Relationship Id=\"rId4\" Type=\"{R}/sharedStrings\" Target=\"sharedStrings.xml\"/><Relationship Id=\"rId5\" Type=\"{R}/calcChain\" Target=\"calcChain.xml\"/><Relationship Id=\"rId6\" Type=\"{R}/theme\" Target=\"theme/theme1.xml\"/></Relationships>")),
            ("xl/calcChain.xml", "<calcChain/>".to_string()),
            ("xl/theme/theme1.xml", "<theme name=\"Keep\"/>".to_string()),
            ("xl/vbaProject.bin", "macro bytes".to_string()),
            ("xl/charts/chart1.xml", "<chart name=\"Keep\"/>".to_string()),
            ("xl/styles.xml", format!("<styleSheet xmlns=\"{NS}\"><numFmts count=\"1\"><numFmt numFmtId=\"170\" formatCode=\"yyyy-mm-dd\"/></numFmts><fonts count=\"2\"><font><sz val=\"11\"/><name val=\"Aptos\"/></font><font><b/><sz val=\"14\"/><color theme=\"1\"/><name val=\"Aptos\"/></font></fonts><fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills><borders count=\"1\"><border/></borders><cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs><cellXfs count=\"3\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/><xf numFmtId=\"0\" fontId=\"1\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/><xf numFmtId=\"170\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/></cellXfs><cellStyles count=\"1\"><cellStyle name=\"Custom\" xfId=\"0\"/></cellStyles><dxfs count=\"1\"><dxf><font><b/></font></dxf></dxfs></styleSheet>")),
            ("xl/sharedStrings.xml", format!("<sst xmlns=\"{NS}\" count=\"2\" uniqueCount=\"2\"><si><r><rPr><b/></rPr><t>rich</t></r><r><t> text</t></r></si><si><t>plain</t></si></sst>")),
            ("xl/worksheets/sheet1.xml", format!("<worksheet xmlns=\"{NS}\" xmlns:r=\"{R}\" xmlns:x14ac=\"urn:x\"><dimension ref=\"A1:B3\"/><sheetViews><sheetView tabSelected=\"1\" workbookViewId=\"0\"><pane ySplit=\"1\" topLeftCell=\"A2\" state=\"frozen\"/></sheetView></sheetViews><cols><col min=\"1\" max=\"2\" width=\"20\" customWidth=\"1\"/></cols><sheetData><row r=\"1\" spans=\"1:2\" x14ac:dyDescent=\"0.25\"><c r=\"A1\" s=\"1\" t=\"s\"><v>0</v></c><c r=\"B1\" t=\"s\"><v>1</v></c></row><row r=\"2\" ht=\"30\" customHeight=\"1\"><c r=\"A2\" s=\"2\"><v>45293</v></c><c r=\"B2\"><f>1+1</f><v>2</v></c></row><row r=\"3\"><c r=\"A3\" t=\"b\"><v>1</v></c><c r=\"B3\" t=\"inlineStr\"><is><t>inline</t></is></c></row></sheetData><mergeCells count=\"1\"><mergeCell ref=\"D1:E1\"/></mergeCells><conditionalFormatting sqref=\"A1\"><cfRule type=\"expression\" dxfId=\"0\" priority=\"1\"><formula>TRUE</formula></cfRule></conditionalFormatting><dataValidations count=\"2\"><dataValidation type=\"whole\" sqref=\"F1\"><formula1>1</formula1></dataValidation><dataValidation type=\"list\" sqref=\"F2\"><formula1>\"a,b\"</formula1></dataValidation></dataValidations><hyperlinks><hyperlink ref=\"A1\" r:id=\"rId2\"/><hyperlink ref=\"A2\" location=\"Other!A1\"/></hyperlinks><pageSetup scale=\"70\" orientation=\"landscape\" r:id=\"rId9\"/><drawing r:id=\"rId1\"/><tableParts count=\"0\"/><extLst><ext uri=\"keep\"/></extLst></worksheet>")),
            ("xl/worksheets/_rels/sheet1.xml.rels", format!("<Relationships xmlns=\"{P}\"><Relationship Id=\"rId1\" Type=\"{R}/drawing\" Target=\"../drawings/drawing1.xml\"/><Relationship Id=\"rId2\" Type=\"{R}/hyperlink\" Target=\"https://example.com/\" TargetMode=\"External\"/><Relationship Id=\"rId9\" Type=\"{R}/printerSettings\" Target=\"../printerSettings/p.bin\"/><Relationship Id=\"rId3\" Type=\"{R}/comments\" Target=\"../comments1.xml\"/></Relationships>")),
            ("xl/comments1.xml", format!("<comments xmlns=\"{NS}\"><authors><author>me</author></authors><commentList><comment ref=\"B1\" authorId=\"0\"><text><r><t>note</t></r></text></comment></commentList></comments>")),
            ("xl/drawings/drawing1.xml", "<wsDr name=\"Keep\"/>".to_string()),
            ("xl/printerSettings/p.bin", "printer".to_string()),
            ("xl/worksheets/sheet2.xml", format!("<worksheet xmlns=\"{NS}\"><sheetData/></worksheet>")),
        ];
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, body) in parts {
            zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap().into_inner()
    }

    #[test]
    fn the_model_reads_what_a_workbook_holds() {
        let workbook = Workbook::open_bytes(&foreign_workbook()).unwrap();
        assert_eq!(workbook.sheet_names(), ["Data", "Other"]);
        let s = workbook.sheet("Data").unwrap();
        assert_eq!(s.value("A1").unwrap(), Some(&CellValue::Text("rich text".into())));
        assert_eq!(s.value("A2").unwrap(), Some(&CellValue::Date("2024-01-02".into())));
        assert_eq!(s.value("B2").unwrap(), Some(&CellValue::formula("1+1", 2.0)));
        assert_eq!(s.value("A3").unwrap(), Some(&CellValue::Boolean(true)));
        assert_eq!(s.value("B3").unwrap(), Some(&CellValue::Text("inline".into())));
        assert_eq!(s.format("A1").unwrap(), Some(&Format::new().bold().font("Aptos").font_size(14.0)));
        assert_eq!(
            s.format("A2").unwrap().unwrap(),
            &Format::new().num_format("yyyy-mm-dd").font("Aptos").font_size(11.0).clone().tap_default()
        );
        assert_eq!(s.merged_ranges(), ["D1:E1"]);
        assert_eq!(s.column_width("B").unwrap(), Some(20.0));
        assert_eq!(s.row_height(2), Some(30.0));
        assert_eq!(s.frozen_panes(), Some((1, 0)));
        assert_eq!(s.hyperlink_of("A1").unwrap(), Some("https://example.com/"));
        assert_eq!(s.comment_of("B1").unwrap(), Some(("note", "me")));
        assert!(workbook.sheet("Other").unwrap().is_hidden());
    }

    trait TapDefault {
        fn tap_default(self) -> Format;
    }

    impl TapDefault for Format {
        /// The first font is the default, which a format does not read into its fields.
        fn tap_default(mut self) -> Format {
            self.font_name = None;
            self.font_size_tenths = None;
            self
        }
    }

    #[test]
    fn saving_unedited_keeps_cells_styles_and_other_parts() {
        let source = foreign_workbook();
        let bytes = Workbook::open_bytes(&source).unwrap().to_bytes().unwrap();
        for name in [
            "xl/theme/theme1.xml",
            "xl/vbaProject.bin",
            "xl/charts/chart1.xml",
            "xl/drawings/drawing1.xml",
            "xl/printerSettings/p.bin",
            "xl/comments1.xml",
        ] {
            assert_eq!(part(&bytes, name), part(&source, name), "{name}");
        }
        let sheet = text_part(&bytes, "xl/worksheets/sheet1.xml");
        for kept in [
            "<c r=\"A1\" s=\"1\" t=\"s\"><v>0</v></c>",
            "<c r=\"B2\"><f>1+1</f><v>2</v></c>",
            "<row r=\"1\" spans=\"1:2\" x14ac:dyDescent=\"0.25\">",
            "xmlns:x14ac=\"urn:x\"",
            "<conditionalFormatting",
            "<pageSetup scale=\"70\"",
            "<drawing r:id=\"rId1\"/>",
            "<tableParts",
            "<ext uri=\"keep\"/>",
            "tabSelected=\"1\"",
        ] {
            assert!(sheet.contains(kept), "{kept}: {sheet}");
        }
        assert!(sheet.find("<sheetData>").unwrap() < sheet.find("<conditionalFormatting").unwrap());
        assert!(text_part(&bytes, "xl/sharedStrings.xml").contains("<r><rPr><b/></rPr><t>rich</t></r>"));
        let styles = text_part(&bytes, "xl/styles.xml");
        assert!(styles.contains("<color theme=\"1\"/>") && styles.contains("<dxfs count=\"1\">") && styles.contains("name=\"Custom\""));
        let book = text_part(&bytes, "xl/workbook.xml");
        assert!(book.contains("codeName=\"Book\"") && book.contains("sheetId=\"7\"") && book.contains("state=\"veryHidden\""));
        assert!(book.contains("comment=\"c\"") && book.contains("calcId=\"191029\"") && !book.contains("fullCalcOnLoad"));
        assert!(book.contains("name=\"_xlnm.Print_Area\" localSheetId=\"0\""));
        // The macro-enabled type stays, and the calculation chain goes with its references.
        let types = text_part(&bytes, "[Content_Types].xml");
        assert!(types.contains("macroEnabled") && !types.contains("calcChain"));
        assert!(part(&bytes, "xl/calcChain.xml").is_none());
        assert!(!text_part(&bytes, "xl/_rels/workbook.xml.rels").contains("calcChain"));
        assert!(text_part(&bytes, "docProps/core.xml").contains("<dc:subject>Keep</dc:subject>"));
    }

    #[test]
    fn editing_one_cell_changes_only_it() {
        let mut workbook = Workbook::open_bytes(&foreign_workbook()).unwrap();
        let bold = Format::new().bold().fill(0xFF0000);
        workbook.sheet_mut("Data").unwrap().write("B3", "edited", &bold).unwrap();
        let bytes = workbook.to_bytes().unwrap();
        assert_eq!(bytes, workbook.to_bytes().unwrap());
        let sheet = text_part(&bytes, "xl/worksheets/sheet1.xml");
        assert!(sheet.contains("<c r=\"A1\" s=\"1\" t=\"s\"><v>0</v></c>"));
        assert!(sheet.contains("<c r=\"B3\" s=\"3\" t=\"s\"><v>2</v></c>"), "{sheet}");
        // Old styles and strings stay where they are; new ones follow.
        let styles = text_part(&bytes, "xl/styles.xml");
        assert!(
            styles.contains("<cellXfs count=\"4\">") && styles.contains("<fonts count=\"3\">") && styles.contains("<color theme=\"1\"/>")
        );
        let strings = text_part(&bytes, "xl/sharedStrings.xml");
        assert!(strings.contains("rich</t>") && strings.contains("<si><t>edited</t></si>"));
        assert!(text_part(&bytes, "xl/workbook.xml").contains("<calcPr calcId=\"191029\" fullCalcOnLoad=\"1\"/>"));
        let reopened = Workbook::open_bytes(&bytes).unwrap();
        let s = reopened.sheet("Data").unwrap();
        assert_eq!(s.value("B3").unwrap(), Some(&CellValue::Text("edited".into())));
        assert_eq!(s.value("A1").unwrap(), Some(&CellValue::Text("rich text".into())));
    }

    #[test]
    fn editing_a_setting_writes_it_and_keeps_what_the_model_lacks() {
        let mut workbook = Workbook::open_bytes(&foreign_workbook()).unwrap();
        let s = workbook.sheet_mut("Data").unwrap();
        s.merge("G1:H1").unwrap();
        s.set_column_width("A", 33.0).unwrap();
        s.validate_list("F3", &["c"]).unwrap();
        s.hyperlink("B3", "https://example.org/").unwrap();
        s.comment("A3", "new", "you").unwrap();
        let bytes = workbook.to_bytes().unwrap();
        let sheet = text_part(&bytes, "xl/worksheets/sheet1.xml");
        assert!(sheet.contains("<mergeCell ref=\"D1:E1\"/><mergeCell ref=\"G1:H1\"/>"));
        assert!(sheet.contains("<col min=\"1\" max=\"1\" width=\"33\"") && sheet.contains("<col min=\"2\" max=\"2\" width=\"20\""));
        assert!(sheet.contains("type=\"whole\"") && sheet.contains("&quot;a,b&quot;") && sheet.contains("&quot;c&quot;"), "{sheet}");
        assert!(sheet.contains("location=\"Other!A1\"") && sheet.contains("<hyperlink ref=\"A1\" r:id=\"rIdLink1\"/>"));
        assert!(sheet.contains("<drawing r:id=\"rId1\"/>") && sheet.contains("<legacyDrawing r:id=\"rIdNoteShapes1\"/>"));
        let rels = text_part(&bytes, "xl/worksheets/_rels/sheet1.xml.rels");
        assert!(rels.contains("drawing1.xml") && rels.contains("https://example.org/") && !rels.contains("Id=\"rId3\""));
        // The notes are written anew, under the next free name.
        assert!(part(&bytes, "xl/comments1.xml").is_none() && part(&bytes, "xl/comments2.xml").is_some());
        let reopened = Workbook::open_bytes(&bytes).unwrap();
        let s = reopened.sheet("Data").unwrap();
        assert_eq!(s.comment_of("A3").unwrap(), Some(("new", "you")));
        assert_eq!(s.comment_of("B1").unwrap(), Some(("note", "me")));
    }

    #[test]
    fn sheets_are_added_moved_and_removed_in_an_opened_workbook() {
        let mut workbook = Workbook::open_bytes(&foreign_workbook()).unwrap();
        workbook.add_sheet("New").unwrap().write("A1", 1.0, &Format::new()).unwrap();
        workbook.move_sheet("New", 0).unwrap();
        workbook.remove_sheet("Other");
        let bytes = workbook.to_bytes().unwrap();
        let book = text_part(&bytes, "xl/workbook.xml");
        assert!(book.find("name=\"New\"").unwrap() < book.find("name=\"Data\"").unwrap());
        assert!(!book.contains("Other") && book.contains("localSheetId=\"1\"") && book.contains("activeTab=\"0\""));
        // The removed sheet's relationship is gone, whatever its part name is used for now.
        assert!(!text_part(&bytes, "xl/_rels/workbook.xml.rels").contains("/xl/worksheets/sheet2.xml"));
        let reopened = Workbook::open_bytes(&bytes).unwrap();
        assert_eq!(reopened.sheet_names(), ["New", "Data"]);
    }

    #[test]
    fn namespaces_the_model_writes_are_declared() {
        assert_eq!(with_namespace("<a x=\"1\">".into(), "r", "u"), "<a x=\"1\" xmlns:r=\"u\">");
        assert_eq!(with_namespace("<a xmlns:r=\"v\">".into(), "r", "u"), "<a xmlns:r=\"v\">");
        let mut workbook = Workbook::new();
        workbook.add_sheet("s").unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(workbook.to_bytes().unwrap())).unwrap();
        let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for i in 0..zip.len() {
            let mut file = zip.by_index(i).unwrap();
            let name = file.name().unwrap().to_string();
            let mut body = String::new();
            file.read_to_string(&mut body).unwrap();
            // As openpyxl writes them: the prefix on the elements that use it.
            let body = if name == "xl/workbook.xml" {
                body.replace(&format!(" xmlns:r=\"{}\"", super::super::OD_RELS), "")
                    .replace("<sheet ", &format!("<sheet xmlns:r=\"{}\" ", super::super::OD_RELS))
            } else if name == "xl/worksheets/sheet1.xml" {
                body.replace(&format!(" xmlns:r=\"{}\"", super::super::OD_RELS), "")
            } else {
                body
            };
            out.start_file(name.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
            out.write_all(body.as_bytes()).unwrap();
        }
        let source = out.finish().unwrap().into_inner();
        let bytes = Workbook::open_bytes(&source).unwrap().to_bytes().unwrap();
        assert!(
            text_part(&bytes, "xl/workbook.xml").contains("<workbook xmlns=") && text_part(&bytes, "xl/workbook.xml").contains("xmlns:r=")
        );
        assert!(text_part(&bytes, "xl/worksheets/sheet1.xml").contains("xmlns:r="));
    }

    #[test]
    fn unlisted_elements_and_the_workbooks_order_are_kept() {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(foreign_workbook())).unwrap();
        let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for i in 0..zip.len() {
            let mut file = zip.by_index(i).unwrap();
            let name = file.name().unwrap().to_string();
            let mut body = String::new();
            file.read_to_string(&mut body).unwrap();
            let body = match name.as_str() {
                "xl/worksheets/sheet1.xml" => {
                    body.replace("<drawing ", "<mc:AlternateContent xmlns:mc=\"urn:mc\"><mc:Choice/></mc:AlternateContent><drawing ")
                }
                "xl/workbook.xml" => body.replace(
                    "<calcPr calcId=\"191029\"/>",
                    "<calcPr calcId=\"191029\" fullCalcOnLoad=\"0\"/><oleSize ref=\"A1\"/><pivotCaches/><fileRecoveryPr/><extLst/>",
                ),
                _ => body,
            };
            out.start_file(name.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
            out.write_all(body.as_bytes()).unwrap();
        }
        let mut workbook = Workbook::open_bytes(&out.finish().unwrap().into_inner()).unwrap();
        workbook.sheet_mut("Data").unwrap().write("C9", 1.0, &Format::new()).unwrap();
        workbook.define_name("rate", "Data!$C$9").unwrap();
        let bytes = workbook.to_bytes().unwrap();
        let sheet = text_part(&bytes, "xl/worksheets/sheet1.xml");
        assert!(sheet.find("<mc:AlternateContent").unwrap() < sheet.find("<drawing ").unwrap());
        let book = text_part(&bytes, "xl/workbook.xml");
        let at = |s: &str| book.find(s).unwrap_or_else(|| panic!("{s}: {book}"));
        assert!(
            at("<calcPr") < at("<oleSize")
                && at("<oleSize") < at("<pivotCaches")
                && at("<pivotCaches") < at("<fileRecoveryPr")
                && at("<fileRecoveryPr") < at("<extLst")
        );
        assert!(book.contains("fullCalcOnLoad=\"1\"") && !book.contains("fullCalcOnLoad=\"0\""));
        assert_eq!(book.matches("name=\"rate\"").count(), 1);
    }

    #[test]
    fn new_relationships_take_ids_not_in_use() {
        let mut workbook = Workbook::open_bytes(&foreign_workbook()).unwrap();
        let s = workbook.sheet_mut("Data").unwrap();
        s.source.as_mut().unwrap().relationships.push(Relationship {
            id: "rIdLink1".into(),
            kind: "x/drawing".into(),
            target: "d.xml".into(),
            external: false,
        });
        s.source.as_mut().unwrap().relationships.push(Relationship {
            id: "rIdNoteShapes1".into(),
            kind: "x/other".into(),
            target: "o.xml".into(),
            external: false,
        });
        s.hyperlink("C1", "https://example.net/").unwrap();
        s.comment("C1", "c", "me").unwrap();
        workbook.add_sheet("Fresh").unwrap();
        let bytes = workbook.to_bytes().unwrap();
        for rels in ["xl/worksheets/_rels/sheet1.xml.rels", "xl/_rels/workbook.xml.rels"] {
            let text = text_part(&bytes, rels);
            let mut ids: Vec<&str> = text.split("Id=\"").skip(1).map(|r| &r[..r.find('"').unwrap()]).collect();
            let all = ids.len();
            ids.sort_unstable();
            ids.dedup();
            assert_eq!(ids.len(), all, "{text}");
        }
    }

    #[test]
    fn columns_keep_their_width_and_style_when_another_changes() {
        let mut workbook = Workbook::open_bytes(&foreign_workbook()).unwrap();
        let s = workbook.sheet_mut("Data").unwrap();
        s.columns.get_mut(&2).unwrap().format = Some(Format::new().bold());
        s.set_column_width("E", 12.0).unwrap();
        let sheet = text_part(&workbook.to_bytes().unwrap(), "xl/worksheets/sheet1.xml");
        assert!(
            sheet.contains("<col min=\"1\" max=\"1\" width=\"20\" customWidth=\"1\"/>")
                && sheet.contains("<col min=\"5\" max=\"5\" width=\"12\""),
            "{sheet}"
        );
        let mut book = Workbook::new();
        let s = book.add_sheet("s").unwrap();
        for c in ["A", "B", "C"] {
            s.set_column_width(c, 9.0).unwrap();
        }
        assert!(text_part(&book.to_bytes().unwrap(), "xl/worksheets/sheet1.xml").contains("<col min=\"1\" max=\"3\" width=\"9\""));
    }

    #[test]
    fn a_hidden_active_sheet_gets_a_new_view() {
        let mut workbook = Workbook::open_bytes(&foreign_workbook()).unwrap();
        workbook.sheet_mut("Other").unwrap().set_hidden(false);
        workbook.sheet_mut("Data").unwrap().set_hidden(true);
        let book = text_part(&workbook.to_bytes().unwrap(), "xl/workbook.xml");
        assert!(book.contains("activeTab=\"1\""), "{book}");
        let mut workbook = Workbook::open_bytes(&foreign_workbook()).unwrap();
        workbook.sheet_mut("Other").unwrap().set_hidden(true);
        // The read view's active sheet, the second, is now hidden.
        assert!(text_part(&workbook.to_bytes().unwrap(), "xl/workbook.xml").contains("activeTab=\"0\""));
    }

    #[test]
    fn a_package_without_a_properties_relationship_gets_one() {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(foreign_workbook())).unwrap();
        let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for i in 0..zip.len() {
            let mut file = zip.by_index(i).unwrap();
            let name = file.name().unwrap().to_string();
            if name == "docProps/core.xml" {
                continue;
            }
            let mut body = String::new();
            file.read_to_string(&mut body).unwrap();
            if name == "_rels/.rels" {
                let i = body.find("<Relationship Id=\"rId2\"").unwrap();
                body = format!("{}</Relationships>", &body[..i]);
            }
            out.start_file(name.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
            out.write_all(body.as_bytes()).unwrap();
        }
        let mut workbook = Workbook::open_bytes(&out.finish().unwrap().into_inner()).unwrap();
        workbook.set_title("T");
        let bytes = workbook.to_bytes().unwrap();
        assert!(text_part(&bytes, "_rels/.rels").contains("core-properties\" Target=\"docProps/core.xml\""));
        assert!(text_part(&bytes, "docProps/core.xml").contains("<dc:title>T</dc:title>"));
    }

    #[test]
    fn a_refused_layer_leaves_no_sheet() {
        let mut layer = wbvector::Layer::new("bad");
        layer.add_field(wbvector::FieldDef::new("t", wbvector::FieldType::Text));
        layer.features.push(wbvector::Feature { fid: 0, geometry: None, attributes: vec![wbvector::FieldValue::Text("x".repeat(40_000))] });
        let mut workbook = Workbook::new();
        assert!(workbook.add_layer(&layer, &Default::default()).is_err());
        assert!(workbook.sheet_names().is_empty());
    }

    /// The test workbook with some parts replaced.
    fn with_parts(changes: &[(&str, &dyn Fn(String) -> String)]) -> Vec<u8> {
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(foreign_workbook())).unwrap();
        let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for i in 0..zip.len() {
            let mut file = zip.by_index(i).unwrap();
            let name = file.name().unwrap().to_string();
            let mut body = String::new();
            file.read_to_string(&mut body).unwrap();
            let body = changes.iter().filter(|(n, _)| *n == name).fold(body, |b, (_, f)| f(b));
            out.start_file(name.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
            out.write_all(body.as_bytes()).unwrap();
        }
        out.finish().unwrap().into_inner()
    }

    #[test]
    fn the_packages_own_relationships_are_kept() {
        let custom = "<Relationship Id=\"rId9\" Type=\"urn:custom-properties\" Target=\"docProps/custom.xml\"/>";
        let add = |b: String| b.replace("</Relationships>", &format!("{custom}</Relationships>"));
        let bytes = Workbook::open_bytes(&with_parts(&[("_rels/.rels", &add)])).unwrap().to_bytes().unwrap();
        let rels = text_part(&bytes, "_rels/.rels");
        assert!(rels.contains(custom) && !rels.contains("extended-properties"), "{rels}");
    }

    #[test]
    fn sheet_properties_keep_what_the_model_lacks() {
        let add = |b: String| b.replace("<dimension", "<sheetPr codeName=\"Sheet1\"><outlinePr summaryBelow=\"0\"/></sheetPr><dimension");
        let mut workbook = Workbook::open_bytes(&with_parts(&[("xl/worksheets/sheet1.xml", &add)])).unwrap();
        workbook.sheet_mut("Data").unwrap().set_tab_color(0x00FF00);
        let sheet = text_part(&workbook.to_bytes().unwrap(), "xl/worksheets/sheet1.xml");
        assert!(
            sheet.contains("<sheetPr codeName=\"Sheet1\"><tabColor rgb=\"FF00FF00\"/><outlinePr summaryBelow=\"0\"/></sheetPr>"),
            "{sheet}"
        );
    }

    #[test]
    fn a_relationship_mark_in_text_does_not_stop_a_save() {
        let add =
            |b: String| b.replace("<pageSetup", "<headerFooter><oddHeader>&amp;Ar:id=\u{e9}r:id=</oddHeader></headerFooter><pageSetup");
        let workbook = Workbook::open_bytes(&with_parts(&[("xl/worksheets/sheet1.xml", &add)])).unwrap();
        assert!(workbook.to_bytes().is_ok());
    }

    #[test]
    fn repeated_elements_are_written_once() {
        let add = |b: String| {
            b.replace("<pageSetup", "<conditionalFormatting sqref=\"B1\"><cfRule type=\"expression\" priority=\"2\"><formula>1</formula></cfRule></conditionalFormatting><pageSetup")
        };
        let bytes = Workbook::open_bytes(&with_parts(&[("xl/worksheets/sheet1.xml", &add)])).unwrap().to_bytes().unwrap();
        assert_eq!(text_part(&bytes, "xl/worksheets/sheet1.xml").matches("<conditionalFormatting").count(), 2);
    }

    #[test]
    fn styles_and_strings_keep_what_the_model_lacks() {
        let styles = |b: String| b.replace("<cellStyleXfs", "<mc:AlternateContent xmlns:mc=\"urn:mc\"/><cellStyleXfs");
        let strings = |b: String| b.replace("<sst ", "<sst xmlns:x=\"urn:x\" ");
        let mut workbook = Workbook::open_bytes(&with_parts(&[("xl/styles.xml", &styles), ("xl/sharedStrings.xml", &strings)])).unwrap();
        workbook.sheet_mut("Data").unwrap().write("C1", "new", &Format::new().italic()).unwrap();
        let bytes = workbook.to_bytes().unwrap();
        let styles = text_part(&bytes, "xl/styles.xml");
        assert!(styles.find("<borders").unwrap() < styles.find("<mc:AlternateContent").unwrap());
        assert!(styles.find("<mc:AlternateContent").unwrap() < styles.find("<cellStyleXfs").unwrap());
        let strings = text_part(&bytes, "xl/sharedStrings.xml");
        assert!(strings.contains("xmlns:x=\"urn:x\"") && strings.contains("uniqueCount=\"3\""), "{strings}");
    }

    #[test]
    fn the_master_of_a_shared_formula_is_not_replaced() {
        let shared = |b: String| {
            b.replace("<c r=\"B2\"><f>1+1</f><v>2</v></c>", "<c r=\"B2\"><f t=\"shared\" ref=\"B2:B3\" si=\"0\">A2*2</f><v>2</v></c>")
                .replace("<c r=\"B3\" t=\"inlineStr\"><is><t>inline</t></is></c>", "<c r=\"B3\"><f t=\"shared\" si=\"0\"/><v>2</v></c>")
        };
        let mut workbook = Workbook::open_bytes(&with_parts(&[("xl/worksheets/sheet1.xml", &shared)])).unwrap();
        let s = workbook.sheet_mut("Data").unwrap();
        assert!(matches!(s.write("B2", 1.0, &Format::new()), Err(Error::InvalidArgument(m)) if m.contains("share")));
        assert!(s.merge_write("A2:B2", "t", &Format::new()).is_err());
        assert!(s.merged_ranges().iter().all(|r| r != "A2:B2"));
        // The cells sharing it may change.
        s.write("B3", 5.0, &Format::new()).unwrap();
    }

    #[test]
    fn properties_set_replace_those_read() {
        let mut workbook = Workbook::open_bytes(&foreign_workbook()).unwrap();
        workbook.set_title("New");
        let core = text_part(&workbook.to_bytes().unwrap(), "docProps/core.xml");
        assert!(core.contains("<dc:title>New</dc:title>") && !core.contains("Old") && core.contains("Keep"));
    }

    #[test]
    fn dates_are_written_in_the_workbooks_date_system() {
        let mut workbook = Workbook::new();
        workbook.date1904 = true;
        workbook.add_sheet("s").unwrap().write("A1", CellValue::Date("2024-01-02".into()), &Format::new()).unwrap();
        assert!(text_part(&workbook.to_bytes().unwrap(), "xl/worksheets/sheet1.xml").contains(&format!("<v>{}</v>", 45293 - 1462)));
    }

    #[test]
    fn a_workbook_the_model_writes_opens_as_it_was_written() {
        let mut workbook = Workbook::new();
        let s = workbook.add_sheet("a").unwrap();
        s.write("B2", "text", &Format::new().bold().fill(0x00FF00).border_thin().align(Align::Center).wrap().num_format("0.00")).unwrap();
        s.merge("C1:D1").unwrap();
        s.autofilter("A1:B2").unwrap();
        s.freeze_panes(1, 1).unwrap();
        s.set_zoom(150).unwrap();
        s.print_margins(0.5, 0.5, 0.5, 0.5, 0.5, 0.5).unwrap();
        s.print_footer("&Pf").unwrap();
        s.validate_list("A1", &["x", "y"]).unwrap();
        let bytes = workbook.to_bytes().unwrap();
        let again = Workbook::open_bytes(&bytes).unwrap();
        let s = again.sheet("a").unwrap();
        let format = s.format("B2").unwrap().unwrap();
        assert!(format.bold && format.wrap);
        assert_eq!(format.fill, Some(0x00FF00));
        assert_eq!(format.align, Some(Align::Center));
        assert_eq!(format.num_format.as_deref(), Some("0.00"));
        assert_eq!(format.borders[0], Some((BorderStyle::Thin, None)));
        assert_eq!(s.autofilter_range().as_deref(), Some("A1:B2"));
        assert_eq!(s.frozen_panes(), Some((1, 1)));
        assert_eq!(s.print.margins, Some([0.5; 6]));
        assert_eq!(s.print.footer.as_deref(), Some("&Pf"));
        assert_eq!(s.validations[0].items, ["x", "y"]);
        assert!(!again.to_bytes().unwrap().is_empty());
    }

    #[test]
    fn layers_view_sheets_both_ways() {
        let mut layer = wbvector::Layer::new("points");
        layer.add_field(wbvector::FieldDef::new("name", wbvector::FieldType::Text));
        layer.add_field(wbvector::FieldDef::new("n", wbvector::FieldType::Integer));
        layer.features.push(wbvector::Feature {
            fid: 0,
            geometry: None,
            attributes: vec![wbvector::FieldValue::Text("a".into()), wbvector::FieldValue::Integer(3)],
        });
        let mut workbook = Workbook::new();
        workbook.add_layer(&layer, &Default::default()).unwrap().set_column_width("A", 30.0).unwrap();
        let read = workbook.layer("points", &Default::default()).unwrap();
        assert_eq!(read.features[0].attributes[1], wbvector::FieldValue::Integer(3));
        assert_eq!(read.schema.fields()[0].name, "name");
        assert!(workbook.layer("nope", &Default::default()).is_err());
    }

    #[test]
    fn what_cannot_be_read_is_refused() {
        assert!(Workbook::open_bytes(b"not a zip").is_err());
        let replace = |name: &str, from: &str, to: &str| {
            let mut zip = zip::ZipArchive::new(std::io::Cursor::new(foreign_workbook())).unwrap();
            let mut out = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            for i in 0..zip.len() {
                let mut file = zip.by_index(i).unwrap();
                let file_name = file.name().unwrap().to_string();
                let mut body = String::new();
                file.read_to_string(&mut body).unwrap();
                out.start_file(file_name.as_str(), zip::write::SimpleFileOptions::default()).unwrap();
                let body = if file_name == name { body.replace(from, to) } else { body };
                out.write_all(body.as_bytes()).unwrap();
            }
            out.finish().unwrap().into_inner()
        };
        let swapped = replace("xl/worksheets/sheet1.xml", "<c r=\"B1\" t=\"s\"><v>1</v></c>", "<c r=\"A1\"/>");
        assert!(matches!(Workbook::open_bytes(&swapped), Err(Error::InvalidData(_))));
        let lost = replace("xl/worksheets/sheet1.xml", "<c r=\"B1\" t=\"s\"><v>1</v></c>", "<c r=\"B1\" t=\"s\"><v>9</v></c>");
        assert!(matches!(Workbook::open_bytes(&lost), Err(Error::InvalidData(_))));
        let prefixed = replace(
            "xl/worksheets/sheet2.xml",
            "<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetData/></worksheet>",
            "<x:worksheet xmlns:x=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><x:sheetData/></x:worksheet>",
        );
        assert!(matches!(Workbook::open_bytes(&prefixed), Err(Error::Unsupported(_))));
    }
}

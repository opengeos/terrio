// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/gml/gmlreader.cpp
//     ogr/ogrsf_frmts/gml/ogrgmldatasource.cpp
// No code is translated from these sources.

//! GML simple features, read and written as GDAL does.
//!
//! Reading makes one layer per feature type, with a `gml_id` field, field types from the
//! application schema (`.xsd` next to the file) or inferred from the values, geometries in GML 2
//! or 3 encodings, and the CRS from `srsName`. Coordinates in latitude/longitude order (an EPSG
//! geographic CRS named by URN or URL) are swapped into longitude/latitude.
//!
//! Writing produces GML 3.2 with an `.xsd` schema, like GDAL's default output.

use super::xml::{self, Element};
use crate::crs::SpatialRef;
use crate::{Error, Result};
use std::fmt::Write as _;
use std::path::Path;
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer, Ring};

const GEOMETRIES: &[&str] = &[
    "Point",
    "LineString",
    "LinearRing",
    "Curve",
    "Polygon",
    "Surface",
    "MultiPoint",
    "MultiLineString",
    "MultiCurve",
    "MultiPolygon",
    "MultiSurface",
    "MultiGeometry",
    "GeometryCollection",
    "CompositeCurve",
    "CompositeSurface",
    "OrientableSurface",
];

/// Reads a GML file: one layer per feature type.
pub fn read(path: &str) -> Result<Vec<Layer>> {
    parse_path_bytes(&std::fs::read(path)?, path)
}

/// Parse already-read GML bytes while retaining the native sibling schema lookup.
pub(crate) fn parse_path_bytes(bytes: &[u8], path: &str) -> Result<Vec<Layer>> {
    let text = xml::decode(bytes);
    let root = xml::parse(&text)?;
    let xsd = std::fs::read(Path::new(path).with_extension("xsd")).ok().and_then(|b| xml::parse(&xml::decode(&b)).ok());
    parse(&root, xsd.as_ref())
}

/// The feature elements of a collection (or the root itself when it is a feature).
fn feature_elements(root: &Element) -> Vec<&Element> {
    let mut out = Vec::new();
    for c in &root.children {
        match c.name.as_str() {
            "featureMember" | "member" => out.extend(c.children.first()),
            "featureMembers" => out.extend(c.children.iter()),
            _ => {}
        }
    }
    if out.is_empty() && !root.name.ends_with("FeatureCollection") && root.children.iter().any(|c| geometry_child(c).is_some()) {
        out.push(root);
    }
    out
}

/// The geometry element inside a property element.
fn geometry_child(prop: &Element) -> Option<&Element> {
    prop.children.iter().find(|c| GEOMETRIES.contains(&c.name.as_str()))
}

/// Parses a GML document into layers.
pub fn parse(root: &Element, xsd: Option<&Element>) -> Result<Vec<Layer>> {
    struct Raw<'a> {
        id: Option<&'a str>,
        geometry: Option<&'a Element>,
        values: Vec<(&'a str, &'a str)>,
    }
    let mut groups: Vec<(String, Vec<Raw>)> = Vec::new();
    for f in feature_elements(root) {
        let mut raw = Raw { id: f.attr("id").or_else(|| f.attr("fid")), geometry: None, values: Vec::new() };
        for p in &f.children {
            if p.name == "boundedBy" {
                continue;
            }
            match geometry_child(p) {
                Some(g) => {
                    if raw.geometry.is_none() {
                        raw.geometry = Some(g);
                    }
                }
                None if p.children.is_empty() => raw.values.push((&p.name, p.text.trim())),
                None => {}
            }
        }
        match groups.iter_mut().find(|(n, _)| *n == f.name) {
            Some((_, v)) => v.push(raw),
            None => groups.push((f.name.clone(), vec![raw])),
        }
    }

    let mut layers = Vec::new();
    for (name, raws) in groups {
        let mut layer = Layer::new(&name);
        layer.add_field(FieldDef::new("gml_id", FieldType::Text).not_null());
        let declared = xsd.map(|x| xsd_fields(x, &name)).unwrap_or_default();
        let mut fields: Vec<(String, FieldType)> = declared;
        if fields.is_empty() {
            let mut names: Vec<&str> = Vec::new();
            for r in &raws {
                for (n, _) in &r.values {
                    if !names.contains(n) {
                        names.push(n);
                    }
                }
            }
            for n in names {
                let values = raws.iter().flat_map(|r| r.values.iter().filter(|(k, _)| *k == n).map(|(_, v)| *v));
                fields.push((n.to_string(), super::csv::infer_type(values)));
            }
        }
        for (n, t) in &fields {
            layer.add_field(FieldDef::new(n, *t));
        }

        // The CRS: the first srsName on a geometry (or the collection's envelope).
        let srs_name = raws
            .iter()
            .filter_map(|r| r.geometry)
            .find_map(|g| srs_name_of(g))
            .or_else(|| root.child("boundedBy").and_then(|b| b.find("Envelope")).and_then(|e| e.attr("srsName")));
        let srs = srs_name.and_then(|s| SpatialRef::parse(s).ok());
        let swap = srs_name.is_some_and(lat_lon_order) && srs.as_ref().is_some_and(SpatialRef::is_geographic);
        if let Some(s) = &srs {
            s.apply_to_layer(&mut layer);
        }

        for (fid, r) in raws.iter().enumerate() {
            let mut attributes = vec![FieldValue::Null; fields.len() + 1];
            attributes[0] = r.id.map_or(FieldValue::Null, |id| FieldValue::Text(id.to_string()));
            for (k, v) in &r.values {
                if let Some(i) = fields.iter().position(|(n, _)| n == k) {
                    attributes[i + 1] = if v.is_empty() { FieldValue::Null } else { value(v, fields[i].1) };
                }
            }
            let geometry = r.geometry.and_then(|g| geometry(g, 2)).map(|g| if swap { swap_xy(g) } else { g });
            layer.push(Feature { fid: fid as u64, geometry, attributes });
        }
        layers.push(layer);
    }
    if layers.is_empty() {
        return Err(Error::InvalidData("GML: no features".into()));
    }
    Ok(layers)
}

fn value(v: &str, t: FieldType) -> FieldValue {
    match t {
        FieldType::Integer => v.parse().map_or_else(|_| FieldValue::Text(v.into()), FieldValue::Integer),
        FieldType::Float => v.parse().map_or_else(|_| FieldValue::Text(v.into()), FieldValue::Float),
        FieldType::Boolean => FieldValue::Boolean(matches!(v, "true" | "1")),
        FieldType::Date => FieldValue::Date(v.into()),
        FieldType::DateTime => FieldValue::DateTime(v.replace('/', "-")),
        _ => FieldValue::Text(v.into()),
    }
}

/// The fields an application schema declares for a feature type, in order.
fn xsd_fields(xsd: &Element, feature: &str) -> Vec<(String, FieldType)> {
    let type_name = xsd
        .children_named("element")
        .find(|e| e.attr("name") == Some(feature))
        .and_then(|e| e.attr("type"))
        .map(|t| t.rsplit(':').next().unwrap_or(t).to_string())
        .unwrap_or_else(|| format!("{feature}Type"));
    let Some(ct) = xsd.children_named("complexType").find(|c| c.attr("name") == Some(&type_name)) else { return Vec::new() };
    let mut elements = Vec::new();
    ct.find_all("element", &mut elements);
    elements
        .into_iter()
        .filter_map(|e| {
            let name = e.attr("name")?;
            let base = e.attr("type").or_else(|| e.find("restriction").and_then(|r| r.attr("base"))).unwrap_or("string");
            if base.starts_with("gml:") {
                return None;
            }
            let t = match base.rsplit(':').next().unwrap_or(base) {
                "integer" | "int" | "long" | "short" | "byte" | "nonNegativeInteger" | "positiveInteger" | "negativeInteger"
                | "nonPositiveInteger" | "unsignedInt" | "unsignedShort" | "unsignedByte" | "unsignedLong" => FieldType::Integer,
                "decimal" | "double" | "float" => FieldType::Float,
                "date" => FieldType::Date,
                "dateTime" => FieldType::DateTime,
                "boolean" => FieldType::Boolean,
                _ => FieldType::Text,
            };
            Some((name.to_string(), t))
        })
        .collect()
}

fn srs_name_of(g: &Element) -> Option<&str> {
    g.attr("srsName").or_else(|| g.children.iter().find_map(srs_name_of))
}

/// Whether a srsName form declares the authority's axis order (latitude first for EPSG geographic
/// CRSs): URNs and `http://www.opengis.net/def/crs/` URLs, but not `EPSG:4326` or the old
/// `epsg.xml#4326` URLs.
fn lat_lon_order(srs_name: &str) -> bool {
    let s = srs_name.to_ascii_lowercase();
    (s.starts_with("urn:") || s.contains("/def/crs/")) && s.contains("epsg")
}

fn swap_xy(g: Geometry) -> Geometry {
    let sw = |c: Coord| Coord { x: c.y, y: c.x, ..c };
    let sv = |cs: Vec<Coord>| cs.into_iter().map(sw).collect::<Vec<_>>();
    let sr = |r: Ring| Ring::new(sv(r.0));
    match g {
        Geometry::Point(c) => Geometry::Point(sw(c)),
        Geometry::LineString(cs) => Geometry::LineString(sv(cs)),
        Geometry::MultiPoint(cs) => Geometry::MultiPoint(sv(cs)),
        Geometry::MultiLineString(ls) => Geometry::MultiLineString(ls.into_iter().map(sv).collect()),
        Geometry::Polygon { exterior, interiors } => {
            Geometry::Polygon { exterior: sr(exterior), interiors: interiors.into_iter().map(sr).collect() }
        }
        Geometry::MultiPolygon(ps) => {
            Geometry::MultiPolygon(ps.into_iter().map(|(e, hs)| (sr(e), hs.into_iter().map(sr).collect())).collect())
        }
        Geometry::GeometryCollection(gs) => Geometry::GeometryCollection(gs.into_iter().map(swap_xy).collect()),
    }
}

/// Parses a coordinate list: `pos`, `posList`, GML 2 `coordinates` and `coord` elements under an
/// element, in document order.
fn coords(e: &Element, dim: usize) -> Vec<Coord> {
    let dim = e.attr("srsDimension").and_then(|d| d.parse().ok()).unwrap_or(dim);
    let mut out = Vec::new();
    match e.name.as_str() {
        "pos" | "posList" => {
            let nums: Vec<f64> = e.text.split_ascii_whitespace().filter_map(|t| t.parse().ok()).collect();
            let dim = if dim < 2 { 2 } else { dim };
            for c in nums.chunks_exact(dim) {
                out.push(Coord { x: c[0], y: c[1], z: (dim > 2).then(|| c[2]), m: None });
            }
        }
        "coordinates" => {
            let cs = e.attr("cs").unwrap_or(",");
            let ts = e.attr("ts").unwrap_or(" ");
            let decimal = e.attr("decimal").unwrap_or(".");
            let tuples: Vec<&str> =
                if ts.trim().is_empty() { e.text.split_whitespace().collect() } else { e.text.split(ts).map(str::trim).collect() };
            for t in tuples.into_iter().filter(|t| !t.is_empty()) {
                let nums: Vec<f64> = t.split(cs).filter_map(|n| n.trim().replace(decimal, ".").parse().ok()).collect();
                if nums.len() >= 2 {
                    out.push(Coord { x: nums[0], y: nums[1], z: nums.get(2).copied(), m: None });
                }
            }
        }
        "coord" => {
            let n = |k: &str| e.child_text(k).and_then(|t| t.parse().ok());
            if let (Some(x), Some(y)) = (n("X"), n("Y")) {
                out.push(Coord { x, y, z: n("Z"), m: None });
            }
        }
        _ => {
            for c in &e.children {
                out.extend(coords(c, dim));
            }
        }
    }
    out
}

fn ring(e: &Element, dim: usize) -> Ring {
    let mut cs = coords(e, dim);
    if cs.len() > 1 && cs.first().map(|c| (c.x, c.y)) == cs.last().map(|c| (c.x, c.y)) {
        cs.pop();
    }
    Ring::new(cs)
}

fn polygon(e: &Element, dim: usize) -> Option<(Ring, Vec<Ring>)> {
    // A Surface holds PolygonPatch elements: use the first.
    let e = e.find("PolygonPatch").unwrap_or(e);
    let exterior = e.child("exterior").or_else(|| e.child("outerBoundaryIs")).map(|x| ring(x, dim))?;
    let interiors = e.children.iter().filter(|c| c.name == "interior" || c.name == "innerBoundaryIs").map(|x| ring(x, dim)).collect();
    Some((exterior, interiors))
}

/// The geometries held by the member properties of a multi-geometry.
fn members(e: &Element, dim: usize) -> Vec<Geometry> {
    let mut out = Vec::new();
    for m in &e.children {
        if m.name.ends_with("Members") {
            out.extend(m.children.iter().filter_map(|g| geometry(g, dim)));
        } else if m.name.ends_with("Member") {
            out.extend(m.children.first().and_then(|g| geometry(g, dim)));
        }
    }
    out
}

/// Parses a GML geometry element.
pub fn geometry(e: &Element, dim: usize) -> Option<Geometry> {
    let dim = e.attr("srsDimension").and_then(|d| d.parse().ok()).unwrap_or(dim);
    Some(match e.name.as_str() {
        "Point" => Geometry::Point(coords(e, dim).into_iter().next()?),
        "LineString" | "Curve" | "LinearRing" | "CompositeCurve" => Geometry::LineString(coords(e, dim)),
        "Polygon" | "Surface" => {
            let (exterior, interiors) = polygon(e, dim)?;
            Geometry::Polygon { exterior, interiors }
        }
        "OrientableSurface" => geometry(e.find("Polygon").or_else(|| e.find("Surface"))?, dim)?,
        "MultiPoint" => Geometry::MultiPoint(
            members(e, dim)
                .into_iter()
                .flat_map(|g| match g {
                    Geometry::Point(c) => vec![c],
                    Geometry::MultiPoint(cs) => cs,
                    _ => vec![],
                })
                .collect(),
        ),
        "MultiLineString" | "MultiCurve" => Geometry::MultiLineString(
            members(e, dim)
                .into_iter()
                .flat_map(|g| match g {
                    Geometry::LineString(cs) => vec![cs],
                    Geometry::MultiLineString(ls) => ls,
                    _ => vec![],
                })
                .collect(),
        ),
        "MultiPolygon" | "MultiSurface" | "CompositeSurface" => Geometry::MultiPolygon(
            members(e, dim)
                .into_iter()
                .flat_map(|g| match g {
                    Geometry::Polygon { exterior, interiors } => vec![(exterior, interiors)],
                    Geometry::MultiPolygon(ps) => ps,
                    _ => vec![],
                })
                .collect(),
        ),
        "MultiGeometry" | "GeometryCollection" => Geometry::GeometryCollection(members(e, dim)),
        _ => return None,
    })
}

/// Writes a layer as GML 3.2, with an `.xsd` application schema next to it.
pub fn write(layer: &Layer, path: &str) -> Result<()> {
    let p = Path::new(path);
    let schema_name = p.with_extension("xsd").file_name().and_then(|s| s.to_str()).unwrap_or("schema.xsd").to_string();
    std::fs::write(p.with_extension("xsd"), xsd(layer))?;
    std::fs::write(path, to_string(layer, &schema_name))?;
    Ok(())
}

/// A layer name usable as an XML element name.
fn element_name(name: &str) -> String {
    let mut s: String = name.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' { c } else { '_' }).collect();
    if !s.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        s.insert(0, '_');
    }
    s
}

/// Field names the writer uses: element names, without the `gml_id` the writer adds itself.
fn written_fields(layer: &Layer) -> Vec<(usize, String)> {
    layer.schema.fields().iter().enumerate().filter(|(_, f)| f.name != "gml_id").map(|(i, f)| (i, element_name(&f.name))).collect()
}

fn xsd(layer: &Layer) -> String {
    let name = element_name(&layer.name);
    let mut s = String::from(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<xs:schema targetNamespace="http://ogr.maptools.org/" xmlns:ogr="http://ogr.maptools.org/" xmlns:xs="http://www.w3.org/2001/XMLSchema" xmlns:gml="http://www.opengis.net/gml/3.2" elementFormDefault="qualified" version="1.0">
<xs:import namespace="http://www.opengis.net/gml/3.2" schemaLocation="http://schemas.opengis.net/gml/3.2.1/gml.xsd"/>
<xs:element name="FeatureCollection" type="ogr:FeatureCollectionType" substitutionGroup="gml:AbstractFeature"/>
<xs:complexType name="FeatureCollectionType">
  <xs:complexContent>
    <xs:extension base="gml:AbstractFeatureType">
      <xs:sequence minOccurs="0" maxOccurs="unbounded">
        <xs:element name="featureMember">
          <xs:complexType>
            <xs:complexContent>
              <xs:extension base="gml:AbstractFeatureMemberType">
                <xs:sequence>
                  <xs:element ref="gml:AbstractFeature"/>
                </xs:sequence>
              </xs:extension>
            </xs:complexContent>
          </xs:complexType>
        </xs:element>
      </xs:sequence>
    </xs:extension>
  </xs:complexContent>
</xs:complexType>
"#,
    );
    let _ = writeln!(s, r#"<xs:element name="{name}" type="ogr:{name}_Type" substitutionGroup="gml:AbstractFeature"/>"#);
    let _ = writeln!(
        s,
        r#"<xs:complexType name="{name}_Type">
  <xs:complexContent>
    <xs:extension base="gml:AbstractFeatureType">
      <xs:sequence>
        <xs:element name="geometryProperty" type="gml:GeometryPropertyType" nillable="true" minOccurs="0" maxOccurs="1"/>"#
    );
    for (i, fname) in written_fields(layer) {
        let base = match layer.schema.fields()[i].field_type {
            FieldType::Integer => "integer",
            FieldType::Float => "decimal",
            FieldType::Date => "date",
            FieldType::DateTime => "dateTime",
            FieldType::Boolean => "boolean",
            _ => "string",
        };
        let _ = writeln!(
            s,
            r#"        <xs:element name="{fname}" nillable="true" minOccurs="0" maxOccurs="1"><xs:simpleType><xs:restriction base="xs:{base}"/></xs:simpleType></xs:element>"#
        );
    }
    s.push_str("      </xs:sequence>\n    </xs:extension>\n  </xs:complexContent>\n</xs:complexType>\n</xs:schema>\n");
    s
}

/// Encodes a layer as GML 3.2 referring to the application schema file `schema` (none when empty).
pub fn to_string(layer: &Layer, schema: &str) -> String {
    let name = element_name(&layer.name);
    let code = super::naming_epsg(layer);
    let geographic = SpatialRef::from_layer(layer).is_geographic();
    // EPSG geographic CRSs named by URN take latitude first.
    let swap = code.is_some() && geographic;
    let srs = code.map(|c| format!(r#" srsName="urn:ogc:def:crs:EPSG::{c}""#)).unwrap_or_default();
    let fields = written_fields(layer);
    let mut s = String::new();
    let _ = write!(
        s,
        r#"<?xml version="1.0" encoding="utf-8" ?>
<ogr:FeatureCollection gml:id="aFeatureCollection"{} xmlns:ogr="http://ogr.maptools.org/" xmlns:gml="http://www.opengis.net/gml/3.2">
"#,
        if schema.is_empty() {
            String::new()
        } else {
            format!(
                r#" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://ogr.maptools.org/ {}""#,
                xml::escape(schema)
            )
        }
    );
    if let Some(e) = crate::vector::layer_extent(layer) {
        let (a, b) = if swap { ((e.1, e.0), (e.3, e.2)) } else { ((e.0, e.1), (e.2, e.3)) };
        let _ = writeln!(
            s,
            "  <gml:boundedBy><gml:Envelope{srs}><gml:lowerCorner>{} {}</gml:lowerCorner><gml:upperCorner>{} {}</gml:upperCorner></gml:Envelope></gml:boundedBy>",
            a.0, a.1, b.0, b.1
        );
    }
    for (n, f) in layer.features.iter().enumerate() {
        let _ = writeln!(
            s,
            r#"  <ogr:featureMember>
    <ogr:{name} gml:id="{name}.{n}">"#
        );
        if let Some(g) = &f.geometry {
            let mut gs = String::new();
            let mut counter = 0;
            write_geometry(&mut gs, g, &srs, swap, &format!("{name}.geom.{n}"), &mut counter);
            let _ = writeln!(s, "      <ogr:geometryProperty>{gs}</ogr:geometryProperty>");
        }
        for (i, fname) in &fields {
            let v = match f.attributes.get(*i) {
                None | Some(FieldValue::Null) => continue,
                Some(FieldValue::Text(t) | FieldValue::Date(t) | FieldValue::DateTime(t)) => xml::escape(t),
                Some(FieldValue::Integer(x)) => x.to_string(),
                Some(FieldValue::Float(x)) => x.to_string(),
                Some(FieldValue::Boolean(b)) => b.to_string(),
                Some(FieldValue::Blob(b)) => b.iter().map(|x| format!("{x:02X}")).collect(),
            };
            let _ = writeln!(s, "      <ogr:{fname}>{v}</ogr:{fname}>");
        }
        let _ = writeln!(s, "    </ogr:{name}>\n  </ogr:featureMember>");
    }
    s.push_str("</ogr:FeatureCollection>\n");
    s
}

fn pos_list(s: &mut String, cs: &[Coord], close: bool, swap: bool) {
    let z = cs.iter().any(|c| c.z.is_some());
    let first = cs.first().filter(|f| close && cs.len() > 1 && cs.last().map(|l| (l.x, l.y)) != Some((f.x, f.y)));
    for (i, c) in cs.iter().chain(first).enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let (a, b) = if swap { (c.y, c.x) } else { (c.x, c.y) };
        let _ = write!(s, "{a} {b}");
        if z {
            let _ = write!(s, " {}", c.z.unwrap_or(0.0));
        }
    }
}

fn dim_attr(has_z: bool) -> &'static str {
    if has_z { r#" srsDimension="3""# } else { "" }
}

fn write_polygon(s: &mut String, exterior: &Ring, interiors: &[Ring], srs: &str, swap: bool, id: &str) {
    let z = exterior.0.iter().any(|c| c.z.is_some());
    let _ = write!(s, r#"<gml:Polygon{srs} gml:id="{id}"><gml:exterior><gml:LinearRing><gml:posList{}>"#, dim_attr(z));
    pos_list(s, &exterior.0, true, swap);
    s.push_str("</gml:posList></gml:LinearRing></gml:exterior>");
    for r in interiors {
        let _ = write!(s, "<gml:interior><gml:LinearRing><gml:posList{}>", dim_attr(z));
        pos_list(s, &r.0, true, swap);
        s.push_str("</gml:posList></gml:LinearRing></gml:interior>");
    }
    s.push_str("</gml:Polygon>");
}

fn write_geometry(s: &mut String, g: &Geometry, srs: &str, swap: bool, id: &str, counter: &mut usize) {
    let sub = |counter: &mut usize| {
        *counter += 1;
        format!("{id}.{}", *counter - 1)
    };
    match g {
        Geometry::Point(c) => {
            let _ = write!(s, r#"<gml:Point{srs} gml:id="{id}"><gml:pos{}>"#, dim_attr(c.z.is_some()));
            pos_list(s, std::slice::from_ref(c), false, swap);
            s.push_str("</gml:pos></gml:Point>");
        }
        Geometry::LineString(cs) => {
            let _ = write!(s, r#"<gml:LineString{srs} gml:id="{id}"><gml:posList{}>"#, dim_attr(cs.iter().any(|c| c.z.is_some())));
            pos_list(s, cs, false, swap);
            s.push_str("</gml:posList></gml:LineString>");
        }
        Geometry::Polygon { exterior, interiors } => write_polygon(s, exterior, interiors, srs, swap, id),
        Geometry::MultiPoint(cs) => {
            let _ = write!(s, r#"<gml:MultiPoint{srs} gml:id="{id}">"#);
            for c in cs {
                s.push_str("<gml:pointMember>");
                write_geometry(s, &Geometry::Point(c.clone()), "", swap, &sub(counter), counter);
                s.push_str("</gml:pointMember>");
            }
            s.push_str("</gml:MultiPoint>");
        }
        Geometry::MultiLineString(ls) => {
            let _ = write!(s, r#"<gml:MultiCurve{srs} gml:id="{id}">"#);
            for l in ls {
                s.push_str("<gml:curveMember>");
                write_geometry(s, &Geometry::LineString(l.clone()), "", swap, &sub(counter), counter);
                s.push_str("</gml:curveMember>");
            }
            s.push_str("</gml:MultiCurve>");
        }
        Geometry::MultiPolygon(ps) => {
            let _ = write!(s, r#"<gml:MultiSurface{srs} gml:id="{id}">"#);
            for (e, hs) in ps {
                s.push_str("<gml:surfaceMember>");
                write_polygon(s, e, hs, "", swap, &sub(counter));
                s.push_str("</gml:surfaceMember>");
            }
            s.push_str("</gml:MultiSurface>");
        }
        Geometry::GeometryCollection(gs) => {
            let _ = write!(s, r#"<gml:MultiGeometry{srs} gml:id="{id}">"#);
            for g in gs {
                s.push_str("<gml:geometryMember>");
                write_geometry(s, g, "", swap, &sub(counter), counter);
                s.push_str("</gml:geometryMember>");
            }
            s.push_str("</gml:MultiGeometry>");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_gml2_and_gml3() {
        let gml2 = r#"<wfs:FeatureCollection xmlns:wfs="w" xmlns:gml="g" xmlns:x="x"><gml:featureMember><x:roads fid="r.1"><x:name>A</x:name><x:lanes>2</x:lanes>
            <x:geom><gml:MultiLineString srsName="EPSG:4326"><gml:lineStringMember><gml:LineString><gml:coordinates>1,2 3,4</gml:coordinates></gml:LineString></gml:lineStringMember></gml:MultiLineString></x:geom></x:roads></gml:featureMember></wfs:FeatureCollection>"#;
        let layers = parse(&xml::parse(gml2).unwrap(), None).unwrap();
        let l = &layers[0];
        assert_eq!((l.name.as_str(), l.crs_epsg()), ("roads", Some(4326)));
        assert_eq!(l.schema.fields().iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["gml_id", "name", "lanes"]);
        assert_eq!(l.features[0].attributes[2], FieldValue::Integer(2));
        assert_eq!(l.features[0].geometry, Some(Geometry::MultiLineString(vec![vec![Coord::xy(1.0, 2.0), Coord::xy(3.0, 4.0)]])));

        // GML 3.2 with a URN: latitude first.
        let gml3 = r#"<ogr:FeatureCollection xmlns:ogr="o" xmlns:gml="g"><ogr:featureMember><ogr:t gml:id="t.0"><ogr:geometryProperty><gml:Polygon srsName="urn:ogc:def:crs:EPSG::4326"><gml:exterior><gml:LinearRing><gml:posList>10 1 10 2 11 2 10 1</gml:posList></gml:LinearRing></gml:exterior></gml:Polygon></ogr:geometryProperty></ogr:t></ogr:featureMember></ogr:FeatureCollection>"#;
        let l = &parse(&xml::parse(gml3).unwrap(), None).unwrap()[0];
        let Some(Geometry::Polygon { exterior, .. }) = &l.features[0].geometry else { panic!("no polygon") };
        assert_eq!((exterior.0[0].x, exterior.0[0].y), (1.0, 10.0));
        assert_eq!(l.features[0].attributes[0], FieldValue::Text("t.0".into()));
    }

    #[test]
    fn round_trips() {
        let mut l = Layer::new("places").with_epsg(4326);
        l.add_field(FieldDef::new("name", FieldType::Text));
        l.add_field(FieldDef::new("pop", FieldType::Float));
        l.push(Feature {
            fid: 0,
            geometry: Some(Geometry::point(10.5, 45.0)),
            attributes: vec![FieldValue::Text("A & B".into()), FieldValue::Float(1.5)],
        });
        let text = to_string(&l, "places.xsd");
        let schema = xml::parse(&xsd(&l)).unwrap();
        let back = &parse(&xml::parse(&text).unwrap(), Some(&schema)).unwrap()[0];
        assert_eq!(back.features[0].geometry, Some(Geometry::point(10.5, 45.0)));
        assert_eq!(back.features[0].attributes[1], FieldValue::Text("A & B".into()));
        assert_eq!(back.schema.fields()[2].field_type, FieldType::Float);
    }
}

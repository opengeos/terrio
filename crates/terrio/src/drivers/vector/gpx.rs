// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/gpx/ogrgpxlayer.cpp
// No code is translated from these sources.

//! GPX, read into GDAL's five layers: `waypoints`, `routes`, `tracks`, `route_points` and
//! `track_points`, with GDAL's fields (GPX 1.0 files have `url`/`urlname` and track point
//! `course`/`speed` fields instead of links). Simple `<extensions>` elements become extra fields
//! named `prefix_name`.

use super::xml::{self, Element};
use crate::Result;
use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

const LINKS: &[&str] = &["link1_href", "link1_text", "link1_type", "link2_href", "link2_text", "link2_type"];

/// The fields of a waypoint-like element (`wpt`, `rtept`, `trkpt`).
fn point_fields(gpx10: bool, track_point: bool) -> Vec<(&'static str, FieldType)> {
    let mut f = vec![("ele", FieldType::Float), ("time", FieldType::DateTime)];
    if gpx10 && track_point {
        f.extend([("course", FieldType::Float), ("speed", FieldType::Float)]);
    }
    f.extend([("magvar", FieldType::Float), ("geoidheight", FieldType::Float)]);
    f.extend(["name", "cmt", "desc", "src"].map(|n| (n, FieldType::Text)));
    if gpx10 {
        f.extend([("url", FieldType::Text), ("urlname", FieldType::Text)]);
    } else {
        f.extend(LINKS.iter().map(|n| (*n, FieldType::Text)));
    }
    f.extend(["sym", "type", "fix"].map(|n| (n, FieldType::Text)));
    f.push(("sat", FieldType::Integer));
    f.extend(["hdop", "vdop", "pdop", "ageofdgpsdata"].map(|n| (n, FieldType::Float)));
    f.push(("dgpsid", FieldType::Integer));
    f
}

/// The fields of a route or track.
fn line_fields() -> Vec<(&'static str, FieldType)> {
    let mut f: Vec<(&str, FieldType)> = ["name", "cmt", "desc", "src"].map(|n| (n, FieldType::Text)).into();
    f.extend(LINKS.iter().map(|n| (*n, FieldType::Text)));
    f.push(("number", FieldType::Integer));
    f.push(("type", FieldType::Text));
    f
}

/// A feature's geometry and its values by field name.
type Row = (Option<Geometry>, Vec<(String, String)>);

/// A layer being built: GDAL's fields, then extension fields as they are found.
struct Builder {
    layer: Layer,
    rows: Vec<Row>,
}

impl Builder {
    fn new(name: &str, geom_type: GeometryType, fields: &[(&str, FieldType)]) -> Self {
        let mut layer = Layer::new(name).with_epsg(4326);
        layer.geom_type = Some(geom_type);
        for (n, t) in fields {
            layer.add_field(FieldDef::new(*n, *t));
        }
        Self { layer, rows: Vec::new() }
    }

    fn push(&mut self, geometry: Option<Geometry>, values: Vec<(String, String)>) {
        self.rows.push((geometry, values));
    }

    fn finish(mut self) -> Layer {
        // Extension fields, typed from their values.
        let mut ext: Vec<String> = Vec::new();
        for (_, values) in &self.rows {
            for (k, _) in values {
                if self.layer.schema.field_index(k).is_none() && !ext.contains(k) {
                    ext.push(k.clone());
                }
            }
        }
        for name in &ext {
            let values = self.rows.iter().flat_map(|(_, vs)| vs.iter().filter(|(k, _)| k == name).map(|(_, v)| v.as_str()));
            let t = match super::csv::infer_type(values) {
                t @ (FieldType::Integer | FieldType::Float) => t,
                _ => FieldType::Text,
            };
            self.layer.add_field(FieldDef::new(name, t));
        }
        let types: Vec<FieldType> = self.layer.schema.fields().iter().map(|f| f.field_type).collect();
        for (fid, (geometry, values)) in std::mem::take(&mut self.rows).into_iter().enumerate() {
            let mut attributes = vec![FieldValue::Null; types.len()];
            for (k, v) in values {
                if let Some(i) = self.layer.schema.field_index(&k) {
                    attributes[i] = match types[i] {
                        FieldType::Integer => v.trim().parse().map_or(FieldValue::Null, FieldValue::Integer),
                        FieldType::Float => v.trim().parse().map_or(FieldValue::Null, FieldValue::Float),
                        FieldType::DateTime => FieldValue::DateTime(v),
                        _ => FieldValue::Text(v),
                    };
                }
            }
            self.layer.push(Feature { fid: fid as u64, geometry, attributes });
        }
        self.layer
    }
}

/// The values of an element's simple children, its links and its extensions.
fn values(e: &Element, gpx10: bool) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut links = 0;
    for c in &e.children {
        match c.name.as_str() {
            "link" if links < 2 => {
                links += 1;
                if let Some(h) = c.attr("href") {
                    out.push((format!("link{links}_href"), h.to_string()));
                }
                for k in ["text", "type"] {
                    if let Some(t) = c.child_text(k) {
                        out.push((format!("link{links}_{k}"), t.to_string()));
                    }
                }
            }
            "url" | "urlname" if !gpx10 || matches!(e.name.as_str(), "rte" | "trk") => {
                let key = if c.name == "url" { "link1_href" } else { "link1_text" };
                out.push((key.to_string(), c.text.trim().to_string()));
            }
            "extensions" => {
                for x in c.children.iter().filter(|x| x.children.is_empty() && !x.text.trim().is_empty()) {
                    let name = if x.prefix.is_empty() { x.name.clone() } else { format!("{}_{}", x.prefix, x.name) };
                    out.push((name, x.text.trim().to_string()));
                }
            }
            _ if c.children.is_empty() && !c.text.trim().is_empty() => out.push((c.name.clone(), c.text.trim().to_string())),
            _ => {}
        }
    }
    out
}

fn coord(e: &Element) -> Option<Coord> {
    Some(Coord::xy(e.attr("lon")?.trim().parse().ok()?, e.attr("lat")?.trim().parse().ok()?))
}

/// Reads a GPX file into GDAL's five layers.
pub fn read(path: &str) -> Result<Vec<Layer>> {
    parse(&xml::decode(&std::fs::read(path)?))
}

/// Parses GPX text.
pub fn parse(text: &str) -> Result<Vec<Layer>> {
    let root = xml::parse(text)?;
    let gpx10 = root.attr("version").is_some_and(|v| v.trim() == "1.0");
    let mut waypoints = Builder::new("waypoints", GeometryType::Point, &point_fields(gpx10, false));
    let mut routes = Builder::new("routes", GeometryType::LineString, &line_fields());
    let mut tracks = Builder::new("tracks", GeometryType::MultiLineString, &line_fields());
    let ids = |names: &[&'static str]| names.iter().map(|n| (*n, FieldType::Integer)).collect::<Vec<_>>();
    let mut route_points =
        Builder::new("route_points", GeometryType::Point, &[ids(&["route_fid", "route_point_id"]), point_fields(gpx10, false)].concat());
    let mut track_points = Builder::new(
        "track_points",
        GeometryType::Point,
        &[ids(&["track_fid", "track_seg_id", "track_seg_point_id"]), point_fields(gpx10, true)].concat(),
    );

    for e in &root.children {
        match e.name.as_str() {
            "wpt" => waypoints.push(coord(e).map(Geometry::Point), values(e, gpx10)),
            "rte" => {
                let fid = routes.rows.len();
                let mut line = Vec::new();
                for (i, p) in e.children_named("rtept").enumerate() {
                    let c = coord(p);
                    line.extend(c.clone());
                    let mut v = vec![("route_fid".to_string(), fid.to_string()), ("route_point_id".to_string(), i.to_string())];
                    v.extend(values(p, gpx10));
                    route_points.push(c.map(Geometry::Point), v);
                }
                routes.push((!line.is_empty()).then_some(Geometry::LineString(line)), values(e, gpx10));
            }
            "trk" => {
                let fid = tracks.rows.len();
                let mut segments = Vec::new();
                for (s, seg) in e.children_named("trkseg").enumerate() {
                    let mut line = Vec::new();
                    for (i, p) in seg.children_named("trkpt").enumerate() {
                        let c = coord(p);
                        line.extend(c.clone());
                        let mut v = vec![
                            ("track_fid".to_string(), fid.to_string()),
                            ("track_seg_id".to_string(), s.to_string()),
                            ("track_seg_point_id".to_string(), i.to_string()),
                        ];
                        v.extend(values(p, gpx10));
                        track_points.push(c.map(Geometry::Point), v);
                    }
                    if !line.is_empty() {
                        segments.push(line);
                    }
                }
                tracks.push((!segments.is_empty()).then_some(Geometry::MultiLineString(segments)), values(e, gpx10));
            }
            _ => {}
        }
    }
    Ok([waypoints, routes, tracks, route_points, track_points].into_iter().map(Builder::finish).collect())
}

/// Element names, in schema order, of a waypoint's and a route's or track's simple content.
const POINT_ELEMENTS: &[&str] = &[
    "ele",
    "time",
    "magvar",
    "geoidheight",
    "name",
    "cmt",
    "desc",
    "src",
    "LINK",
    "sym",
    "type",
    "fix",
    "sat",
    "hdop",
    "vdop",
    "pdop",
    "ageofdgpsdata",
    "dgpsid",
];
const LINE_ELEMENTS: &[&str] = &["name", "cmt", "desc", "src", "LINK", "number", "type"];

/// Writes a layer as GPX 1.1, as GDAL does: points as waypoints, lines as routes and multi-lines
/// as tracks. A `track_points` or `route_points` layer with GDAL's `track_fid`/`route_fid`
/// fields is written back as tracks or routes. Fields named after GPX elements become those
/// elements; the others are written as `<extensions>`.
pub fn write(layer: &Layer, path: &str) -> Result<()> {
    write_layers(std::slice::from_ref(layer), path)
}

/// Writes several layers into one GPX file. When the `routes` and `route_points` layers (or
/// `tracks` and `track_points`) are both written, the points, with their attributes, make the
/// routes (tracks).
pub fn write_layers(layers: &[Layer], path: &str) -> Result<()> {
    std::fs::write(path, layers_to_string(layers)?)?;
    Ok(())
}

/// Encodes layers as a GPX document.
pub fn layers_to_string(layers: &[Layer]) -> Result<String> {
    let find = |n: &str| layers.iter().find(|l| l.name == n);
    let mut s = String::from(HEADER);
    for l in layers {
        let points = match l.name.as_str() {
            "routes" => find("route_points"),
            "tracks" => find("track_points"),
            "route_points" if find("routes").is_some() => continue,
            "track_points" if find("tracks").is_some() => continue,
            _ => None,
        };
        s.push_str(&body(l, points)?);
    }
    s.push_str("</gpx>\n");
    Ok(s)
}

const HEADER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<gpx version=\"1.1\" creator=\"terrio\" xmlns=\"http://www.topografix.com/GPX/1/1\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xmlns:ogr=\"http://osgeo.org/gdal\" xsi:schemaLocation=\"http://www.topografix.com/GPX/1/1 http://www.topografix.com/GPX/1/1/gpx.xsd\">\n";

fn text_of(v: &FieldValue) -> Option<String> {
    Some(match v {
        FieldValue::Null => return None,
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => s.clone(),
        FieldValue::Integer(x) => x.to_string(),
        FieldValue::Float(x) => x.to_string(),
        FieldValue::Boolean(b) => b.to_string(),
        FieldValue::Blob(_) => return None,
    })
}

/// Writes an element's simple content: GPX elements in schema order, links, then extensions.
fn write_content(s: &mut String, layer: &Layer, f: &Feature, elements: &[&str], skip: &[&str], indent: &str) {
    let get = |n: &str| layer.schema.field_index(n).and_then(|i| f.attributes.get(i)).and_then(text_of);
    for e in elements {
        if *e == "LINK" {
            for k in 1..=2 {
                let Some(href) = get(&format!("link{k}_href")).or_else(|| (k == 1).then(|| get("url")).flatten()) else { continue };
                s.push_str(&format!("{indent}<link href=\"{}\">", xml::escape(&href)));
                if let Some(t) = get(&format!("link{k}_text")).or_else(|| (k == 1).then(|| get("urlname")).flatten()) {
                    s.push_str(&format!("<text>{}</text>", xml::escape(&t)));
                }
                if let Some(t) = get(&format!("link{k}_type")) {
                    s.push_str(&format!("<type>{}</type>", xml::escape(&t)));
                }
                s.push_str("</link>\n");
            }
        } else if let Some(v) = get(e) {
            s.push_str(&format!("{indent}<{e}>{}</{e}>\n", xml::escape(&v)));
        }
    }
    let standard = |n: &str| {
        elements.contains(&n) || skip.contains(&n) || n == "url" || n == "urlname" || n.starts_with("link1_") || n.starts_with("link2_")
    };
    let extra: Vec<(String, String)> = layer
        .schema
        .fields()
        .iter()
        .zip(&f.attributes)
        .filter(|(d, _)| !standard(&d.name))
        .filter_map(|(d, v)| Some((d.name.clone(), text_of(v)?)))
        .collect();
    if !extra.is_empty() {
        s.push_str(&format!("{indent}<extensions>\n"));
        for (k, v) in extra {
            let k = xml_name(&k);
            s.push_str(&format!("{indent}  <ogr:{k}>{}</ogr:{k}>\n", xml::escape(&v)));
        }
        s.push_str(&format!("{indent}</extensions>\n"));
    }
}

fn xml_name(name: &str) -> String {
    let mut s: String = name.chars().map(|c| if c.is_alphanumeric() || c == '_' || c == '-' || c == '.' { c } else { '_' }).collect();
    if !s.starts_with(|c: char| c.is_alphabetic() || c == '_') {
        s.insert(0, '_');
    }
    s
}

fn point_attrs(c: &Coord) -> String {
    format!("lat=\"{}\" lon=\"{}\"", c.y, c.x)
}

#[cfg(test)]
fn to_string(layer: &Layer) -> Result<String> {
    Ok(format!("{HEADER}{}</gpx>\n", body(layer, None)?))
}

/// A layer in WGS 84, reprojected when its CRS is another one.
fn in_wgs84(layer: &Layer) -> Result<std::borrow::Cow<'_, Layer>> {
    let srs = crate::crs::SpatialRef::from_layer(layer);
    if srs.is_unknown() || srs.epsg == Some(4326) {
        return Ok(std::borrow::Cow::Borrowed(layer));
    }
    Ok(std::borrow::Cow::Owned(crate::vector::reproject_layer(layer, &crate::crs::SpatialRef::from_epsg(4326)?)?))
}

fn int_field(layer: &Layer, f: &Feature, n: &str) -> Option<i64> {
    layer.schema.field_index(n).and_then(|i| f.attributes.get(i)).and_then(FieldValue::as_i64)
}

/// Points of a `route_points` or `track_points` layer, by route or track, then segment.
type Grouped<'a> = std::collections::BTreeMap<i64, std::collections::BTreeMap<i64, Vec<&'a Feature>>>;

fn group_points<'a>(points: &'a Layer, parent: &str, segment: Option<&str>) -> Grouped<'a> {
    let mut out: Grouped = Default::default();
    for f in &points.features {
        if !matches!(f.geometry, Some(Geometry::Point(_))) {
            continue;
        }
        let p = int_field(points, f, parent).unwrap_or(0);
        let s = segment.and_then(|n| int_field(points, f, n)).unwrap_or(0);
        out.entry(p).or_default().entry(s).or_default().push(f);
    }
    out
}

const ROUTE_IDS: &[&str] = &["route_fid", "route_point_id"];
const TRACK_IDS: &[&str] = &["track_fid", "track_seg_id", "track_seg_point_id"];

fn write_point(s: &mut String, tag: &str, layer: &Layer, f: &Feature, skip: &[&str], indent: &str) {
    let Some(Geometry::Point(c)) = &f.geometry else { return };
    s.push_str(&format!("{indent}<{tag} {}>\n", point_attrs(c)));
    write_content(s, layer, f, POINT_ELEMENTS, skip, &format!("{indent}  "));
    s.push_str(&format!("{indent}</{tag}>\n"));
}

/// The GPX elements of a layer's features. `points` is the `route_points` or `track_points` layer
/// holding the vertices of a `routes` or `tracks` layer, with their attributes.
fn body(layer: &Layer, points: Option<&Layer>) -> Result<String> {
    let layer = in_wgs84(layer)?;
    let layer = layer.as_ref();
    let points = points.map(in_wgs84).transpose()?;
    let points = points.as_deref();
    let has = |n: &str| layer.schema.field_index(n).is_some();
    let mut s = String::new();
    if layer.name == "track_points" && has("track_fid") {
        for segments in group_points(layer, "track_fid", Some("track_seg_id")).values() {
            s.push_str("<trk>\n");
            for seg in segments.values() {
                s.push_str("  <trkseg>\n");
                for f in seg {
                    write_point(&mut s, "trkpt", layer, f, TRACK_IDS, "    ");
                }
                s.push_str("  </trkseg>\n");
            }
            s.push_str("</trk>\n");
        }
        return Ok(s);
    }
    if layer.name == "route_points" && has("route_fid") {
        for segments in group_points(layer, "route_fid", None).values() {
            s.push_str("<rte>\n");
            for f in segments.values().flatten() {
                write_point(&mut s, "rtept", layer, f, ROUTE_IDS, "  ");
            }
            s.push_str("</rte>\n");
        }
        return Ok(s);
    }
    let route_points = points.filter(|_| layer.name == "routes").map(|p| (p, group_points(p, "route_fid", None)));
    let track_points = points.filter(|_| layer.name == "tracks").map(|p| (p, group_points(p, "track_fid", Some("track_seg_id"))));
    for (fid, f) in layer.features.iter().enumerate() {
        let fid = fid as i64;
        match &f.geometry {
            Some(Geometry::Point(_)) => write_point(&mut s, "wpt", layer, f, &[], ""),
            Some(Geometry::MultiPoint(cs)) => {
                for c in cs {
                    write_point(&mut s, "wpt", layer, &Feature { geometry: Some(Geometry::Point(c.clone())), ..f.clone() }, &[], "");
                }
            }
            Some(Geometry::LineString(_)) | None if layer.name == "routes" || matches!(f.geometry, Some(Geometry::LineString(_))) => {
                s.push_str("<rte>\n");
                write_content(&mut s, layer, f, LINE_ELEMENTS, &[], "  ");
                match (&route_points, &f.geometry) {
                    (Some((p, groups)), _) => {
                        for pt in groups.get(&fid).into_iter().flat_map(|g| g.values().flatten()) {
                            write_point(&mut s, "rtept", p, pt, ROUTE_IDS, "  ");
                        }
                    }
                    (None, Some(Geometry::LineString(cs))) => {
                        for c in cs {
                            s.push_str(&format!("  <rtept {}>{}</rtept>\n", point_attrs(c), ele(c)));
                        }
                    }
                    _ => {}
                }
                s.push_str("</rte>\n");
            }
            Some(Geometry::MultiLineString(_)) | None
                if layer.name == "tracks" || matches!(f.geometry, Some(Geometry::MultiLineString(_))) =>
            {
                s.push_str("<trk>\n");
                write_content(&mut s, layer, f, LINE_ELEMENTS, &[], "  ");
                match (&track_points, &f.geometry) {
                    (Some((p, groups)), _) => {
                        for seg in groups.get(&fid).into_iter().flat_map(|g| g.values()) {
                            s.push_str("  <trkseg>\n");
                            for pt in seg {
                                write_point(&mut s, "trkpt", p, pt, TRACK_IDS, "    ");
                            }
                            s.push_str("  </trkseg>\n");
                        }
                    }
                    (None, Some(Geometry::MultiLineString(ls))) => {
                        for l in ls {
                            s.push_str("  <trkseg>\n");
                            for c in l {
                                s.push_str(&format!("    <trkpt {}>{}</trkpt>\n", point_attrs(c), ele(c)));
                            }
                            s.push_str("  </trkseg>\n");
                        }
                    }
                    _ => {}
                }
                s.push_str("</trk>\n");
            }
            None => {}
            Some(g) => {
                return Err(crate::Error::Unsupported(format!("GPX holds points and lines, not {}", g.geom_type().as_str())));
            }
        }
    }
    Ok(s)
}

fn ele(c: &Coord) -> String {
    c.z.map(|z| format!("<ele>{z}</ele>")).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_gdal_layers() {
        let gpx = r#"<gpx version="1.1" xmlns="http://www.topografix.com/GPX/1/1" xmlns:ext="e">
            <wpt lat="12.3" lon="45.6"><ele>75.1</ele><time>2013-01-02T02:03:00Z</time><name>w</name>
              <link href="http://a"><text>t</text></link><sat>5</sat><extensions><ext:aaa>1</ext:aaa></extensions></wpt>
            <rte><name>r</name><rtept lat="10" lon="20"/><rtept lat="11" lon="21"/></rte>
            <trk><name>t</name><trkseg><trkpt lat="1" lon="2"><ele>3</ele></trkpt><trkpt lat="1.1" lon="2.1"/></trkseg><trkseg><trkpt lat="1.2" lon="2.2"/></trkseg></trk>
        </gpx>"#;
        let layers = parse(gpx).unwrap();
        let names: Vec<&str> = layers.iter().map(|l| l.name.as_str()).collect();
        assert_eq!(names, ["waypoints", "routes", "tracks", "route_points", "track_points"]);
        let counts: Vec<usize> = layers.iter().map(|l| l.len()).collect();
        assert_eq!(counts, [1, 1, 1, 2, 3]);
        let w = &layers[0];
        let get = |l: &Layer, f: usize, n: &str| l.features[f].attributes[l.schema.field_index(n).unwrap()].clone();
        assert_eq!(get(w, 0, "ele"), FieldValue::Float(75.1));
        assert_eq!(get(w, 0, "link1_href"), FieldValue::Text("http://a".into()));
        assert_eq!(get(w, 0, "sat"), FieldValue::Integer(5));
        assert_eq!(get(w, 0, "ext_aaa"), FieldValue::Integer(1));
        assert_eq!(w.features[0].geometry, Some(Geometry::point(45.6, 12.3)));
        let Some(Geometry::MultiLineString(segs)) = &layers[2].features[0].geometry else { panic!("no track") };
        assert_eq!(segs.len(), 2);
        assert_eq!(get(&layers[4], 2, "track_seg_id"), FieldValue::Integer(1));
        assert_eq!(get(&layers[3], 1, "route_point_id"), FieldValue::Integer(1));

        // Written back, each layer reads the same.
        for (i, l) in layers.iter().enumerate() {
            let back = parse(&to_string(l).unwrap()).unwrap();
            assert_eq!(back[i].len(), l.len(), "{}", l.name);
            assert_eq!(back[i].features[0].geometry, l.features[0].geometry, "{}", l.name);
        }
        let back = parse(&to_string(&layers[0]).unwrap()).unwrap();
        assert_eq!(get(&back[0], 0, "ogr_ext_aaa"), FieldValue::Integer(1));
    }
}

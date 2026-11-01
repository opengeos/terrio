// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/geojson/ogresrijsonreader.cpp
//     ogr/ogresrijsongeometry.cpp
// No code is translated from these sources.

//! The query results of an ArcGIS feature service: their paging, fields, values, geometries and
//! spatial reference, apart from how a page is encoded.
//!
//! Fields are typed as GDAL's ESRIJSON driver types them. Terrio's data model has no subtypes, so
//! `esriFieldTypeSmallInteger` is an integer and `esriFieldTypeSingle` a real, and GUIDs are
//! text. Dates are UTC date-times from milliseconds since 1970. The object id field
//! (`esriFieldTypeOID`) is an integer field that also gives each feature its id.
//!
//! Polygon rings are sorted into polygons by containment, as GDAL's `organizePolygons` does,
//! whatever their orientation. A geometry's coordinates are XY, then Z when it has Z, then M when
//! it has M: with three numbers, the third is M only when the geometry has M and no Z.

use crate::crs::SpatialRef;
use wbvector::{Coord, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

/// One page of a query's results.
pub(crate) struct Page {
    pub layer: Layer,
    /// The service has more features (`exceededTransferLimit`).
    #[cfg_attr(not(remote), allow(dead_code))]
    pub more: bool,
    /// The features without an object id, whose ids are their place in the page, in order.
    #[cfg_attr(not(remote), allow(dead_code))]
    pub by_position: Vec<usize>,
}

/// The most bytes read from a service for one dataset: a service that never stops paging ends here.
#[cfg(remote)]
const MAX_TOTAL_BYTES: u64 = 4 << 30;

/// Whether a URL is likely a feature-service query, by GDAL's signs: `f=json`, `f=pjson` or
/// `resultRecordCount=`, as parameters of its own, without the `/items?` of OGC API Features. What
/// it answers decides.
#[cfg(remote)]
pub(crate) fn is_query_url(url: &str) -> bool {
    (url.starts_with("http://") || url.starts_with("https://"))
        && (matches!(query_value(url, "f"), Some("json" | "pjson")) || query_value(url, "resultRecordCount").is_some())
        && !url.contains("/items?")
}

/// The first bytes of a query's answer, from a plain request: the answer is made per request, and
/// many ArcGIS servers ignore ranges.
#[cfg(remote)]
pub(crate) fn query_head(remote: &crate::http::Remote, maximum: u64) -> crate::Result<Vec<u8>> {
    use std::io::Read;
    crate::http::with_reader(remote, |reader| {
        let mut head = Vec::new();
        reader.take(maximum).read_to_end(&mut head)?;
        Ok(head)
    })
}

/// The value of a query parameter of a URL.
#[cfg(remote)]
fn query_value<'a>(url: &'a str, key: &str) -> Option<&'a str> {
    let query = url.split('#').next()?.split_once('?')?.1;
    query.split('&').find_map(|pair| pair.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| v))
}

/// A URL with a query parameter set, replacing the one it has.
#[cfg(remote)]
fn with_query_value(url: &str, key: &str, value: &str) -> String {
    let (url, fragment) = url.split_once('#').map_or((url, None), |(u, f)| (u, Some(f)));
    let (base, query) = url.split_once('?').unwrap_or((url, ""));
    let mut pairs: Vec<String> =
        query.split('&').filter(|p| !p.is_empty() && p.split('=').next() != Some(key)).map(str::to_string).collect();
    pairs.push(format!("{key}={value}"));
    let fragment = fragment.map(|f| format!("#{f}")).unwrap_or_default();
    format!("{base}?{}{fragment}", pairs.join("&"))
}

/// Whether to read a query's following pages, as GDAL's `FEATURE_SERVER_PAGING` decides: by
/// default unless the URL gives a `resultOffset`; with the option, as it says.
#[cfg(remote)]
fn pages_wanted(url: &str, option: Option<bool>) -> bool {
    option.unwrap_or_else(|| query_value(url, "resultOffset").is_none())
}

/// The layer of a query's first page and the pages after it. The following requests ask for as
/// many features as the first page holds when the URL does not say, as GDAL's do, from
/// `resultOffset` on. Reading stops once `stop_at` features are read. A page that adds nothing while
/// saying there are more is an error, as is a page with other fields, and a page the same as the one
/// before it, by its bytes or its first object id: the service ignores the offset. Features without
/// object ids are numbered by their place in the whole layer.
#[cfg(remote)]
pub(crate) fn read_pages(
    url: &str,
    first: &[u8],
    option: Option<bool>,
    stop_at: Option<usize>,
    decode: &dyn Fn(&[u8]) -> crate::Result<Page>,
) -> crate::Result<Layer> {
    let invalid = |message: String| crate::Error::InvalidData(format!("feature service: {message}"));
    let Page { mut layer, mut more, by_position } = decode(first)?;
    // A service that ignores the offset answers the same features again, whatever else changes in
    // its answer: the first object id of a page shows it.
    let first_object_id = |l: &Layer, by_position: &[usize]| {
        l.features.iter().enumerate().find(|(i, _)| by_position.binary_search(i).is_err()).map(|(_, f)| f.fid)
    };
    let mut previous_id = first_object_id(&layer, &by_position);
    if !pages_wanted(url, option) {
        return Ok(layer);
    }
    let start: u64 = query_value(url, "resultOffset").and_then(|v| v.parse().ok()).unwrap_or(0);
    let base = match query_value(url, "resultRecordCount") {
        Some(_) => url.to_string(),
        None => with_query_value(url, "resultRecordCount", &layer.features.len().to_string()),
    };
    let names = |l: &Layer| l.schema.fields().iter().map(|f| f.name.clone()).collect::<Vec<_>>();
    let fields = names(&layer);
    let mut total = first.len() as u64;
    let mut previous = std::borrow::Cow::Borrowed(first);
    while more && stop_at.is_none_or(|n| layer.features.len() < n) {
        if layer.features.is_empty() {
            return Err(invalid("the service says there are more features, but its page has none".into()));
        }
        let offset = start.saturating_add(layer.features.len() as u64);
        let bytes = crate::http::get(&crate::http::Remote::Url(with_query_value(&base, "resultOffset", &offset.to_string())))?;
        total = total.saturating_add(bytes.len() as u64);
        if total > MAX_TOTAL_BYTES {
            return Err(invalid(format!("the results exceed {MAX_TOTAL_BYTES} bytes; ask for fewer features")));
        }
        let page = decode(&bytes)?;
        let id = first_object_id(&page.layer, &page.by_position);
        if bytes == *previous || (id.is_some() && id == previous_id) {
            return Err(invalid(format!(
                "the page at offset {offset} is the same as the one before it: the service seems to ignore the offset (FEATURE_SERVER_PAGING=NO reads one page)"
            )));
        }
        previous_id = id;
        if page.more && page.layer.features.is_empty() {
            return Err(invalid(format!("the page at offset {offset} has no features, but says there are more")));
        }
        // A last page without features and without `fields` names none.
        if !page.layer.features.is_empty() && names(&page.layer) != fields {
            return Err(invalid(format!("the page at offset {offset} has other fields than the first")));
        }
        more = page.more;
        // Features without an object id are numbered by their place in the whole layer.
        for (i, mut feature) in page.layer.features.into_iter().enumerate() {
            if page.by_position.binary_search(&i).is_ok() {
                feature.fid = layer.features.len() as u64;
            }
            layer.push(feature);
        }
        previous = std::borrow::Cow::Owned(bytes);
    }
    Ok(layer)
}

/// The Terrio type of an `esriFieldType…` name, as GDAL maps it; unknown names are text.
pub(crate) fn field_type(esri: &str) -> FieldType {
    match esri {
        "esriFieldTypeOID" | "esriFieldTypeSmallInteger" | "esriFieldTypeInteger" | "esriFieldTypeBigInteger" => FieldType::Integer,
        "esriFieldTypeSingle" | "esriFieldTypeDouble" => FieldType::Float,
        "esriFieldTypeDate" | "esriFieldTypeTimestampOffset" => FieldType::DateTime,
        "esriFieldTypeDateOnly" => FieldType::Date,
        _ => FieldType::Text,
    }
}

/// A field of a query result: its definition, and whether it is the object id field.
pub(crate) struct Field {
    pub def: FieldDef,
    pub object_id: bool,
}

/// A field from its name, `esriFieldType…` name and `length`. A length of 2,147,483,647 means
/// none is known, as GDAL reads it; dates take none.
pub(crate) fn field(name: &str, esri: &str, length: Option<i64>) -> Field {
    let field_type = field_type(esri);
    let mut def = FieldDef::new(name, field_type);
    if let Some(width) = length.filter(|&w| w > 0 && w != i64::from(i32::MAX)).and_then(|w| usize::try_from(w).ok())
        && !matches!(field_type, FieldType::DateTime)
    {
        def = def.width(width);
    }
    Field { def, object_id: esri == "esriFieldTypeOID" }
}

/// A date-time value from milliseconds since 1970-01-01, UTC.
pub(crate) fn date_time(ms: i64) -> FieldValue {
    FieldValue::DateTime(super::datetime::format_datetime(ms, true))
}

/// The spatial reference a service names: `latestWkid`, else `wkid`, else WKT. A WKID that is
/// not an EPSG code is tried as one of the ESRI authority's (102100 and its kin). The text comes
/// from a service, so it is read only as WKT (`SpatialRef::parse` would also open a file it names),
/// and text without WKT's `KEYWORD[` start names no CRS.
pub(crate) fn spatial_reference(latest_wkid: Option<u32>, wkid: Option<u32>, wkt: Option<&str>) -> Option<SpatialRef> {
    if let Some(code) = latest_wkid.filter(|&c| c > 0).or(wkid.filter(|&c| c > 0)) {
        return SpatialRef::from_epsg(code).or_else(|_| SpatialRef::from_esri(code)).ok();
    }
    let wkt = wkt.map(str::trim)?;
    let keyword = wkt.split_once('[')?.0.trim_end();
    if keyword.is_empty() || !keyword.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let crs = SpatialRef::from_wkt(wkt);
    (!crs.is_unknown()).then_some(crs)
}

/// The layer geometry type an `esriGeometry…` name declares, as GDAL declares it.
pub(crate) fn layer_geometry_type(esri: &str) -> Option<GeometryType> {
    match esri {
        "esriGeometryPoint" => Some(GeometryType::Point),
        "esriGeometryMultipoint" => Some(GeometryType::MultiPoint),
        "esriGeometryPolyline" => Some(GeometryType::LineString),
        "esriGeometryPolygon" => Some(GeometryType::Polygon),
        _ => None,
    }
}

/// A coordinate from a position's X, Y and the values after them (`None` for a null one): Z when
/// the geometry has Z, then M when it has M. One value in a geometry with M and no Z is M.
pub(crate) fn coordinate(x: f64, y: f64, extra: &[Option<f64>], has_z: bool, has_m: bool) -> Coord {
    let first = extra.first().copied().flatten();
    let second = extra.get(1).copied().flatten();
    let (z, m) = if extra.len() == 1 && has_m && !has_z { (None, first) } else { (first, second) };
    Coord { x, y, z, m }
}

/// A polyline's geometry: one path is a line string, several a multi line string, none no geometry.
pub(crate) fn polyline(mut paths: Vec<Vec<Coord>>) -> Option<Geometry> {
    match paths.len() {
        0 => None,
        1 => paths.pop().map(Geometry::LineString),
        _ => Some(Geometry::MultiLineString(paths)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(x: f64, y: f64) -> Coord {
        Coord { x, y, z: None, m: None }
    }

    #[cfg(remote)]
    #[test]
    fn query_url_signs_are_recognized() {
        assert!(is_query_url("https://x.org/arcgis/rest/services/A/FeatureServer/0/query?where=1=1&f=json"));
        assert!(is_query_url("https://x.org/a/query?f=pjson"));
        assert!(is_query_url("https://x.org/a/query?resultRecordCount=10"));
        assert!(!is_query_url("https://x.org/ogc/collections/a/items?f=json"));
        assert!(!is_query_url("https://x.org/a/query?f=geojson"));
        assert!(!is_query_url("/local/query?f=json"));
        // Only as parameters of their own.
        assert!(!is_query_url("https://x.org/a.tif?conf=json"));
        assert!(!is_query_url("https://x.org/a?pdf=json"));
        assert!(!is_query_url("https://x.org/a?f=jsonp"));
        assert!(!is_query_url("https://x.org/f=json"));
    }

    #[cfg(remote)]
    #[test]
    fn query_parameters_are_read_and_replaced() {
        let url = "https://x.org/q?where=a%3D1&resultOffset=5&f=json#top";
        assert_eq!(query_value(url, "resultOffset"), Some("5"));
        assert_eq!(query_value(url, "offset"), None);
        assert_eq!(query_value("https://x.org/q", "f"), None);
        assert_eq!(with_query_value(url, "resultOffset", "9"), "https://x.org/q?where=a%3D1&f=json&resultOffset=9#top");
        assert_eq!(with_query_value("https://x.org/q", "f", "json"), "https://x.org/q?f=json");
    }

    #[cfg(remote)]
    #[test]
    fn paging_follows_the_option_else_the_offset() {
        assert!(pages_wanted("https://x.org/q?f=json", None));
        assert!(!pages_wanted("https://x.org/q?f=json&resultOffset=0", None));
        assert!(pages_wanted("https://x.org/q?f=json&resultOffset=0", Some(true)));
        assert!(!pages_wanted("https://x.org/q?f=json", Some(false)));
    }

    #[test]
    fn esri_field_types_map_to_terrio_types() {
        let cases = [
            ("esriFieldTypeOID", FieldType::Integer),
            ("esriFieldTypeSmallInteger", FieldType::Integer),
            ("esriFieldTypeBigInteger", FieldType::Integer),
            ("esriFieldTypeSingle", FieldType::Float),
            ("esriFieldTypeDouble", FieldType::Float),
            ("esriFieldTypeDate", FieldType::DateTime),
            ("esriFieldTypeDateOnly", FieldType::Date),
            ("esriFieldTypeTimeOnly", FieldType::Text),
            ("esriFieldTypeGUID", FieldType::Text),
            ("esriFieldTypeGlobalID", FieldType::Text),
            ("esriFieldTypeString", FieldType::Text),
            ("somethingNew", FieldType::Text),
        ];
        for (esri, t) in cases {
            assert_eq!(field_type(esri), t, "{esri}");
        }
    }

    #[test]
    fn widths_follow_lengths_except_the_unknown_one_and_dates() {
        assert_eq!(field("NAME", "esriFieldTypeString", Some(50)).def.width, 50);
        assert_eq!(field("NAME", "esriFieldTypeString", Some(2_147_483_647)).def.width, 0);
        assert_eq!(field("WHEN", "esriFieldTypeDate", Some(8)).def.width, 0);
        assert!(field("OBJECTID", "esriFieldTypeOID", Some(4)).object_id);
        assert!(!field("NAME", "esriFieldTypeString", None).object_id);
    }

    #[test]
    fn dates_are_utc_date_times() {
        assert_eq!(date_time(0), FieldValue::DateTime("1970-01-01T00:00:00Z".into()));
        assert_eq!(date_time(1_700_000_000_123), FieldValue::DateTime("2023-11-14T22:13:20.123Z".into()));
        assert_eq!(date_time(-86_400_000), FieldValue::DateTime("1969-12-31T00:00:00Z".into()));
    }

    #[test]
    fn the_latest_wkid_wins_and_esri_codes_are_known() {
        assert_eq!(spatial_reference(Some(3857), Some(102100), None).and_then(|c| c.authority()), Some(("EPSG", 3857)));
        assert_eq!(spatial_reference(None, Some(4326), None).and_then(|c| c.authority()), Some(("EPSG", 4326)));
        assert!(spatial_reference(None, Some(102100), None).is_some());
        assert!(spatial_reference(None, None, None).is_none());
        // A latest WKID of zero names none: the WKID still does.
        assert_eq!(spatial_reference(Some(0), Some(4326), None).and_then(|c| c.authority()), Some(("EPSG", 4326)));
    }

    #[test]
    fn only_wkt_is_read_from_a_services_text() {
        let wkt = r#"GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]"#;
        assert!(spatial_reference(None, None, Some(wkt)).is_some());
        // Neither junk nor a path: `SpatialRef::parse` would open a file named here.
        for text in ["not a crs", "", "/etc/passwd", "../a[b].prj", "EPSG:4326"] {
            assert!(spatial_reference(None, None, Some(text)).is_none(), "{text}");
        }
    }

    #[test]
    fn positions_read_z_and_m_as_the_geometry_declares() {
        let p = |extra: &[Option<f64>], z, m| {
            let c = coordinate(1.0, 2.0, extra, z, m);
            (c.z, c.m)
        };
        assert_eq!(p(&[], false, false), (None, None));
        assert_eq!(p(&[Some(3.0)], true, false), (Some(3.0), None));
        assert_eq!(p(&[Some(3.0)], false, true), (None, Some(3.0)));
        assert_eq!(p(&[Some(3.0)], true, true), (Some(3.0), None));
        assert_eq!(p(&[Some(3.0), Some(4.0)], true, true), (Some(3.0), Some(4.0)));
        assert_eq!(p(&[None, Some(4.0)], true, true), (None, Some(4.0)));
    }

    #[test]
    fn one_path_is_a_line_several_a_multi_line_and_none_no_geometry() {
        assert!(matches!(polyline(vec![vec![c(0.0, 0.0), c(1.0, 1.0)]]), Some(Geometry::LineString(l)) if l.len() == 2));
        assert!(matches!(polyline(vec![vec![c(0.0, 0.0)], vec![c(1.0, 1.0)]]), Some(Geometry::MultiLineString(l)) if l.len() == 2));
        assert!(polyline(Vec::new()).is_none());
    }

    #[test]
    fn rings_become_polygons_by_containment_whatever_their_order() {
        let square = |x: f64, s: f64| vec![c(x, 0.0), c(x + s, 0.0), c(x + s, s), c(x, s), c(x, 0.0)];
        // A hole listed before its exterior, and a second exterior.
        let rings = vec![square(1.0, 1.0), square(0.0, 4.0), square(10.0, 1.0)];
        let polygon = crate::drivers::vector::polygons_from_rings;
        let Some(Geometry::MultiPolygon(polygons)) = polygon(rings) else { panic!("not a multi polygon") };
        assert_eq!(polygons.len(), 2);
        assert_eq!(polygons.iter().map(|(_, holes)| holes.len()).sum::<usize>(), 1);
        assert!(polygon(vec![vec![c(0.0, 0.0), c(1.0, 1.0)]]).is_none());
    }

    #[test]
    fn geometry_type_names_declare_layer_types() {
        assert_eq!(layer_geometry_type("esriGeometryPolyline"), Some(GeometryType::LineString));
        assert_eq!(layer_geometry_type("esriGeometryPolygon"), Some(GeometryType::Polygon));
        assert_eq!(layer_geometry_type("esriGeometryEnvelope"), None);
    }
}

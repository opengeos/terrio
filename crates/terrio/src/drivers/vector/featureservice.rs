// SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
// SPDX-License-Identifier: MIT

// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrsf_frmts/geojson/ogresrijsonreader.cpp
//     ogr/ogresrijsongeometry.cpp
// No code is translated from these sources.

//! What the `ESRIJSON` and `ESRIPBF` drivers share: the query results of an ArcGIS feature
//! service, as JSON (`f=json`) or as the FeatureCollection protocol buffer (`f=pbf`), give the
//! same fields, values, geometries and spatial reference.
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
use wbvector::{Coord, FieldDef, FieldType, FieldValue, Geometry, GeometryType};

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
    if let Some(code) = latest_wkid.or(wkid).filter(|&c| c > 0) {
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

/// A coordinate from a position's numbers: X, Y, then Z when the geometry has Z, then M when it
/// has M. Three numbers in a geometry with M and no Z are X, Y and M.
pub(crate) fn coordinate(values: &[f64], has_z: bool, has_m: bool) -> Option<Coord> {
    let (x, y) = (*values.first()?, *values.get(1)?);
    let (z, m) = match values.len() {
        2 => (None, None),
        3 if has_m && !has_z => (None, values.get(2).copied()),
        3 => (values.get(2).copied(), None),
        _ => (values.get(2).copied(), values.get(3).copied()),
    };
    Some(Coord { x, y, z, m })
}

/// A polyline's geometry: one path is a line string, several a multi line string.
pub(crate) fn polyline(mut paths: Vec<Vec<Coord>>) -> Geometry {
    match paths.len() {
        1 => Geometry::LineString(paths.pop().unwrap_or_default()),
        0 => Geometry::LineString(Vec::new()),
        _ => Geometry::MultiLineString(paths),
    }
}

/// A polygon's geometry, its rings sorted into exteriors and holes by containment. `None`
/// when no ring has three vertices.
pub(crate) fn polygon(rings: Vec<Vec<Coord>>) -> Option<Geometry> {
    super::polygons_from_rings(rings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(x: f64, y: f64) -> Coord {
        Coord { x, y, z: None, m: None }
    }

    #[test]
    fn fields_are_typed_as_gdal_types_them() {
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
        let p = |v: &[f64], z, m| coordinate(v, z, m).map(|c| (c.z, c.m));
        assert_eq!(p(&[1.0, 2.0], false, false), Some((None, None)));
        assert_eq!(p(&[1.0, 2.0, 3.0], true, false), Some((Some(3.0), None)));
        assert_eq!(p(&[1.0, 2.0, 3.0], false, true), Some((None, Some(3.0))));
        assert_eq!(p(&[1.0, 2.0, 3.0], true, true), Some((Some(3.0), None)));
        assert_eq!(p(&[1.0, 2.0, 3.0, 4.0], true, true), Some((Some(3.0), Some(4.0))));
        assert_eq!(p(&[1.0], false, false), None);
    }

    #[test]
    fn one_path_is_a_line_and_several_a_multi_line() {
        assert!(matches!(polyline(vec![vec![c(0.0, 0.0), c(1.0, 1.0)]]), Geometry::LineString(l) if l.len() == 2));
        assert!(matches!(polyline(vec![vec![c(0.0, 0.0)], vec![c(1.0, 1.0)]]), Geometry::MultiLineString(l) if l.len() == 2));
    }

    #[test]
    fn rings_become_polygons_by_containment_whatever_their_order() {
        let square = |x: f64, s: f64| vec![c(x, 0.0), c(x + s, 0.0), c(x + s, s), c(x, s), c(x, 0.0)];
        // A hole listed before its exterior, and a second exterior.
        let rings = vec![square(1.0, 1.0), square(0.0, 4.0), square(10.0, 1.0)];
        let Some(Geometry::MultiPolygon(polygons)) = polygon(rings) else { panic!("not a multi polygon") };
        assert_eq!(polygons.len(), 2);
        assert_eq!(polygons.iter().map(|(_, holes)| holes.len()).sum::<usize>(), 1);
        assert!(polygon(vec![vec![c(0.0, 0.0), c(1.0, 1.0)]]).is_none());
    }

    #[test]
    fn layer_types_are_declared_as_gdal_declares_them() {
        assert_eq!(layer_geometry_type("esriGeometryPolyline"), Some(GeometryType::LineString));
        assert_eq!(layer_geometry_type("esriGeometryPolygon"), Some(GeometryType::Polygon));
        assert_eq!(layer_geometry_type("esriGeometryEnvelope"), None);
    }
}

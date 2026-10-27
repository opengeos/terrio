//! Point geometries from coordinate fields, as `gdal vector make-point` makes them, for any
//! vector input.

use crate::crs::SpatialRef;
use crate::{Error, Result};
use wbvector::{Coord, FieldType, FieldValue, Geometry, GeometryType, Layer};

/// Options of [`make_point`].
#[derive(Debug, Clone, Default)]
pub struct MakePointOptions {
    /// The field holding X coordinates.
    pub x: String,
    /// The field holding Y coordinates.
    pub y: String,
    /// The field holding Z coordinates.
    pub z: Option<String>,
    /// The field holding M values.
    pub m: Option<String>,
    /// The CRS assigned to the points (none by default, as in GDAL: the input's CRS is not
    /// kept, since the coordinates need not be in it).
    pub crs: Option<SpatialRef>,
}

/// The index of a coordinate field, which must be numeric or text.
fn coordinate_field(layer: &Layer, dim: &str, name: &str) -> Result<usize> {
    let i = layer
        .schema
        .field_index(name)
        .or_else(|| layer.schema.fields().iter().position(|f| f.name.eq_ignore_ascii_case(name)))
        .ok_or_else(|| Error::InvalidArgument(format!("specified {dim} field name '{name}' does not exist")))?;
    let t = layer.schema.fields()[i].field_type;
    if !matches!(t, FieldType::Integer | FieldType::Float | FieldType::Text) {
        return Err(Error::InvalidArgument(format!("invalid {dim} field type: {t}")));
    }
    Ok(i)
}

/// A coordinate from a field value: numbers as they are (null as 0, as GDAL reads it), text
/// parsed whole (surrounding spaces allowed), an error otherwise.
fn coordinate(layer: &Layer, i: usize, v: Option<&FieldValue>) -> Result<f64> {
    match v {
        Some(FieldValue::Integer(x)) => Ok(*x as f64),
        Some(FieldValue::Float(x)) => Ok(*x),
        Some(FieldValue::Boolean(b)) => Ok(f64::from(u8::from(*b))),
        Some(FieldValue::Text(s)) => s
            .trim()
            .parse::<f64>()
            .map_err(|_| Error::InvalidData(format!("invalid value in field {}: {s}", layer.schema.fields()[i].name))),
        _ if layer.schema.fields()[i].field_type == FieldType::Text => {
            Err(Error::InvalidData(format!("invalid value in field {}: (null)", layer.schema.fields()[i].name)))
        }
        _ => Ok(0.0),
    }
}

/// Replaces the geometries of a layer with points read from coordinate fields, keeping every
/// field, as `gdal vector make-point` does. Text fields must hold numbers; a value that does
/// not parse is an error.
pub fn make_point(layer: &Layer, o: &MakePointOptions) -> Result<Layer> {
    let xi = coordinate_field(layer, "X", &o.x)?;
    let yi = coordinate_field(layer, "Y", &o.y)?;
    let zi = o.z.as_deref().map(|z| coordinate_field(layer, "Z", z)).transpose()?;
    let mi = o.m.as_deref().map(|m| coordinate_field(layer, "M", m)).transpose()?;
    let mut out = layer.clone();
    out.geom_type = Some(GeometryType::Point);
    out.extent = None;
    out.set_crs_epsg(None);
    out.set_crs_wkt(None);
    if let Some(c) = &o.crs {
        c.apply_to_layer(&mut out);
    }
    for f in &mut out.features {
        let get = |i: usize| coordinate(layer, i, f.attributes.get(i));
        let c = Coord { x: get(xi)?, y: get(yi)?, z: zi.map(get).transpose()?, m: mi.map(get).transpose()? };
        f.geometry = Some(Geometry::Point(c));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{Feature, FieldDef};

    fn layer() -> Layer {
        let mut l = Layer::new("t").with_epsg(32613);
        l.add_field(FieldDef::new("lon", FieldType::Text));
        l.add_field(FieldDef::new("lat", FieldType::Float));
        l.add_field(FieldDef::new("h", FieldType::Integer));
        for (x, y, h) in [(" -105.5 ", 40.25, 1600), ("-104", 39.0, 1700)] {
            l.push(Feature {
                fid: 0,
                geometry: None,
                attributes: vec![FieldValue::Text(x.into()), FieldValue::Float(y), FieldValue::Integer(h)],
            });
        }
        l
    }

    #[test]
    fn non_finite_text_coordinates_follow_gdal() {
        let options = MakePointOptions { x: "lon".into(), y: "lat".into(), ..Default::default() };
        for text in ["NaN", "inf", "infinity", "-inf"] {
            let mut input = layer();
            input.features[0].attributes[0] = FieldValue::Text(text.into());
            let output = make_point(&input, &options).unwrap();
            let Some(Geometry::Point(point)) = output.features[0].geometry.as_ref() else { panic!() };
            assert!(!point.x.is_finite());
        }
    }

    #[test]
    fn points_from_fields() {
        let o = MakePointOptions {
            x: "lon".into(),
            y: "lat".into(),
            z: Some("h".into()),
            crs: Some(SpatialRef::from_epsg(4326).unwrap()),
            ..Default::default()
        };
        let out = make_point(&layer(), &o).unwrap();
        assert_eq!(out.crs_epsg(), Some(4326));
        assert_eq!(out.schema.len(), 3);
        let Some(Geometry::Point(c)) = &out.features[0].geometry else { panic!() };
        assert_eq!((c.x, c.y, c.z, c.m), (-105.5, 40.25, Some(1600.0), None));
        // Without --dst-crs the points have no CRS, as in GDAL.
        let out = make_point(&layer(), &MakePointOptions { x: "lon".into(), y: "lat".into(), ..Default::default() }).unwrap();
        assert!(out.crs_epsg().is_none());
    }

    #[test]
    fn null_numeric_coordinate_is_zero_like_gdal() {
        let mut l = layer();
        l.features[0].attributes[1] = FieldValue::Null;
        let out = make_point(&l, &MakePointOptions { x: "lon".into(), y: "lat".into(), ..Default::default() }).unwrap();
        let Some(Geometry::Point(c)) = &out.features[0].geometry else { panic!() };
        assert_eq!(c.y, 0.);
    }

    #[test]
    fn rejects_bad_fields_and_values() {
        assert!(make_point(&layer(), &MakePointOptions { x: "nope".into(), y: "lat".into(), ..Default::default() }).is_err());
        let mut l = layer();
        l.features[1].attributes[0] = FieldValue::Text("12abc".into());
        assert!(make_point(&l, &MakePointOptions { x: "lon".into(), y: "lat".into(), ..Default::default() }).is_err());
    }
}

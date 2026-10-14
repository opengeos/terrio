//! Point clouds as vector layers of points with attributes, and back; rasters as points.

use super::{Dimension, PointCloud, ValueType, Values};
use crate::crs::SpatialRef;
use crate::{Error, Result};
use wbvector::{Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};

/// How to turn points into a vector layer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ToLayerOptions {
    /// The layer name. By default, `points`.
    pub name: Option<String>,
    /// The dimensions written as attributes. Empty means all but `x` and `y`.
    pub dimensions: Vec<String>,
    /// Write 2D points; by default the points are 3D, with `z` as their third coordinate.
    pub two_d: bool,
}

/// A vector layer of the points, one feature each, with the dimensions as attributes.
pub fn to_layer(cloud: &PointCloud, options: &ToLayerOptions) -> Result<Layer> {
    let xs = cloud.coords("x").ok_or_else(|| Error::InvalidArgument("the points have no x".into()))?;
    let ys = cloud.coords("y").ok_or_else(|| Error::InvalidArgument("the points have no y".into()))?;
    let zs = cloud.coords("z");
    let dims: Vec<&Dimension> = if options.dimensions.is_empty() {
        cloud.dimensions.iter().filter(|d| d.name != "x" && d.name != "y").collect()
    } else {
        options
            .dimensions
            .iter()
            .map(|n| {
                cloud.dim(n).ok_or_else(|| {
                    Error::InvalidArgument(format!("no dimension '{n}' (dimensions: {})", cloud.dimension_names().join(", ")))
                })
            })
            .collect::<Result<_>>()?
    };
    let mut layer = Layer::new(options.name.clone().unwrap_or_else(|| "points".into())).with_geom_type(GeometryType::Point);
    let integer = |d: &Dimension| {
        d.scale.is_none() && d.offset.is_none() && !matches!(d.values.value_type(), ValueType::F32 | ValueType::F64 | ValueType::U64)
    };
    for d in &dims {
        layer.add_field(FieldDef::new(d.name.clone(), if integer(d) { FieldType::Integer } else { FieldType::Float }));
    }
    cloud.crs.apply_to_layer(&mut layer);
    let ints: Vec<bool> = dims.iter().map(|d| integer(d)).collect();
    layer.features.reserve(cloud.len());
    for i in 0..cloud.len() {
        let geometry = match (zs, options.two_d) {
            (Some(z), false) => Geometry::point_z(xs[i], ys[i], z[i]),
            _ => Geometry::point(xs[i], ys[i]),
        };
        let attributes = dims
            .iter()
            .zip(&ints)
            .map(|(d, int)| if *int { FieldValue::Integer(d.values.get_f64(i) as i64) } else { FieldValue::Float(d.get(i)) })
            .collect();
        layer.push(Feature { fid: i as u64, geometry: Some(geometry), attributes });
    }
    Ok(layer)
}

/// A point cloud of a layer's point geometries (multipoints contribute each of their points), with
/// its numeric attributes as dimensions. Attributes named like the standard LAS dimensions
/// (`intensity`, `classification`...) keep their LAS types; others become `float64` extra-bytes
/// dimensions. `z` comes from the geometries, or from a `z` attribute.
pub fn from_layer(layer: &Layer) -> Result<PointCloud> {
    let fields = layer.schema.fields();
    let numeric: Vec<usize> =
        (0..fields.len()).filter(|&k| matches!(fields[k].field_type, FieldType::Integer | FieldType::Float | FieldType::Boolean)).collect();
    let mut x = Vec::new();
    let mut y = Vec::new();
    let mut z = Vec::new();
    let mut attrs: Vec<Vec<f64>> = vec![Vec::new(); numeric.len()];
    let z_field = numeric.iter().position(|&k| fields[k].name.eq_ignore_ascii_case("z"));
    for f in &layer.features {
        let coords: Vec<wbvector::Coord> = match &f.geometry {
            Some(Geometry::Point(c)) => vec![c.clone()],
            Some(Geometry::MultiPoint(cs)) => cs.clone(),
            _ => continue,
        };
        for c in coords {
            x.push(c.x);
            y.push(c.y);
            let values: Vec<f64> = numeric
                .iter()
                .map(|&k| match f.attributes.get(k) {
                    Some(FieldValue::Integer(v)) => *v as f64,
                    Some(FieldValue::Float(v)) => *v,
                    Some(FieldValue::Boolean(b)) => f64::from(u8::from(*b)),
                    _ => f64::NAN,
                })
                .collect();
            z.push(c.z.or_else(|| z_field.map(|p| values[p])).unwrap_or(0.0));
            for (a, v) in attrs.iter_mut().zip(values) {
                a.push(v);
            }
        }
    }
    if x.is_empty() {
        return Err(Error::InvalidArgument(format!("layer '{}' has no point geometries", layer.name)));
    }
    let mut dims = vec![Dimension::new("x", Values::F64(x)), Dimension::new("y", Values::F64(y)), Dimension::new("z", Values::F64(z))];
    for (k, a) in numeric.iter().zip(attrs) {
        let name = fields[*k].name.to_ascii_lowercase();
        if matches!(name.as_str(), "x" | "y" | "z") || dims.iter().any(|d| d.name == name) {
            continue;
        }
        match super::standard_value_type(&name) {
            Some(t) => dims.push(Dimension::new(&name, Values::F64(a).cast(t))),
            None => dims.push(Dimension::new(&fields[*k].name, Values::F64(a))),
        }
    }
    Ok(super::write::from_dimensions(dims, SpatialRef::from_layer(layer)))
}

/// A point cloud of a raster band: one point at the centre of each valid cell, with the cell
/// value as `z`.
pub fn from_raster(ds: &crate::RasterDataset, band: usize) -> Result<PointCloud> {
    let r = &ds.raster;
    if band >= r.bands {
        return Err(Error::InvalidArgument(format!("no band {} (the raster has {})", band + 1, r.bands)));
    }
    let gt = ds.geo_transform();
    let nodata = ds.nodata();
    let view = r.band_view(band);
    let values = view.as_slice();
    let (mut x, mut y, mut z) = (Vec::new(), Vec::new(), Vec::new());
    for row in 0..r.rows {
        for col in 0..r.cols {
            let v = values[row * r.cols + col];
            if v.is_nan() || nodata.is_some_and(|nd| v == nd) {
                continue;
            }
            x.push(gt[0] + (col as f64 + 0.5) * gt[1] + (row as f64 + 0.5) * gt[2]);
            y.push(gt[3] + (col as f64 + 0.5) * gt[4] + (row as f64 + 0.5) * gt[5]);
            z.push(v);
        }
    }
    let n = x.len();
    let dims = vec![
        Dimension::new("x", Values::F64(x)),
        Dimension::new("y", Values::F64(y)),
        Dimension::new("z", Values::F64(z)),
        Dimension::new("intensity", Values::zeros(ValueType::U16, n)),
        Dimension::new("return_number", Values::U8(vec![1; n])),
        Dimension::new("number_of_returns", Values::U8(vec![1; n])),
        Dimension::new("classification", Values::zeros(ValueType::U8, n)),
    ];
    Ok(super::write::from_dimensions(dims, ds.crs()))
}

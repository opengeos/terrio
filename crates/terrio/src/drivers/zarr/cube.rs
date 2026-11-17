//! Zarr v3 stores of zonal statistics cubes: the variables over `(geometry, ..., statistic)`, the
//! zones as CF-1.8 polygon geometries with a CF grid mapping, their fields as coordinates along
//! `geometry`, and the CRS and extent as `proj:` and `spatial:` attributes.

use super::*;
use crate::ops::zonal_cube::{CubeValues, ZonalCube};
use wbvector::{Coord, FieldValue, Geometry, Ring};

/// Arrays of the CF encoding, and the dimensions they use; a field of the same name is suffixed.
const RESERVED: [&str; 12] = [
    "geometry",
    "geometry_container",
    "geometry_x",
    "geometry_y",
    "geometry_node_count",
    "geometry_part_node_count",
    "geometry_interior_ring",
    "spatial_ref",
    "feature_id",
    "time",
    "band",
    "statistic",
];

/// Writes `cube` to the new Zarr v3 store `path`.
pub(crate) fn write(cube: &ZonalCube, path: &Path) -> Result<()> {
    if memfs::exists(path) {
        return Err(Error::InvalidArgument(format!("{} already exists", path.display())));
    }
    let polygons = polygons(cube)?;
    let srs = crate::crs::SpatialRef::from_layer(&cube.zones);

    // The geometries: nodes, then one entry per ring.
    let (mut xs, mut ys, mut node_count, mut part_nodes, mut interior) = (vec![], vec![], vec![], vec![], vec![]);
    for parts in &polygons {
        let before = xs.len();
        for (exterior, holes) in parts {
            for (ring, hole) in std::iter::once((*exterior, false)).chain(holes.iter().map(|h| (h, true))) {
                // CF: exterior rings anticlockwise, holes clockwise.
                let coords: Vec<&Coord> = if (signed_area(ring.coords()) > 0.0) == hole {
                    ring.coords().iter().rev().collect()
                } else {
                    ring.coords().iter().collect()
                };
                xs.extend(coords.iter().map(|c| c.x));
                ys.extend(coords.iter().map(|c| c.y));
                part_nodes.push(count(coords.len())?);
                interior.push(i32::from(hole));
            }
        }
        node_count.push(count(xs.len() - before)?);
    }
    let fold = |v: &[f64], f: fn(f64, f64) -> f64, init: f64| v.iter().copied().fold(init, f);
    let bbox = [
        fold(&xs, f64::min, f64::INFINITY),
        fold(&ys, f64::min, f64::INFINITY),
        fold(&xs, f64::max, f64::NEG_INFINITY),
        fold(&ys, f64::max, f64::NEG_INFINITY),
    ];

    let mut attrs = json!({"Conventions": "CF-1.8", "spatial:bbox": bbox});
    let mut conventions = vec![convention("spatial", SPATIAL_UUID, "spatial")];
    if !srs.is_unknown() {
        proj_attrs(&srs, &mut attrs);
        if attrs.get("proj:code").is_some() || attrs.get("proj:projjson").is_some() {
            conventions.insert(0, convention("proj", PROJ_UUID, "proj"));
        }
    }
    attrs["zarr_conventions"] = json!(conventions);
    write_group(path, &attrs, Version::V3, &mut serde_json::Map::new(), "")?;

    let geographic = !srs.is_unknown() && srs.is_geographic();
    let axis = |axis: &str, degrees: (&str, &str), projected: &str| {
        let mut a = json!({"axis": axis});
        if !srs.is_unknown() {
            if geographic {
                a["standard_name"] = json!(degrees.0);
                a["units"] = json!(degrees.1);
            } else {
                a["standard_name"] = json!(projected);
            }
        }
        a
    };
    numeric(
        path,
        "geometry_x",
        &["geometry_node"],
        DataType::F64,
        &f64_bytes(&xs),
        axis("X", ("longitude", "degrees_east"), "projection_x_coordinate"),
    )?;
    numeric(
        path,
        "geometry_y",
        &["geometry_node"],
        DataType::F64,
        &f64_bytes(&ys),
        axis("Y", ("latitude", "degrees_north"), "projection_y_coordinate"),
    )?;
    numeric(path, "geometry_node_count", &["geometry"], DataType::I32, &i32_bytes(&node_count), json!({}))?;
    numeric(path, "geometry_part_node_count", &["geometry_part"], DataType::I32, &i32_bytes(&part_nodes), json!({}))?;
    numeric(path, "geometry_interior_ring", &["geometry_part"], DataType::I32, &i32_bytes(&interior), json!({}))?;
    let container = json!({
        "geometry_type": "polygon",
        "node_count": "geometry_node_count",
        "node_coordinates": "geometry_x geometry_y",
        "part_node_count": "geometry_part_node_count",
        "interior_ring": "geometry_interior_ring",
        "grid_mapping": "spatial_ref",
        "variable_name": "geometry",
    });
    scalar(path, "geometry_container", container)?;
    let mut gm = json!({});
    if let Some(w) = super::super::wkt::gdal_wkt(&srs) {
        gm["crs_wkt"] = json!(w);
        gm["spatial_ref"] = json!(w);
    }
    scalar(path, "spatial_ref", gm)?;

    // The features' identifiers and fields, along `geometry`.
    let fids: Vec<i64> = cube.zones.features.iter().map(|f| f.fid as i64).collect();
    numeric(path, "feature_id", &["geometry"], DataType::I64, &i64_bytes(&fids), json!({}))?;
    let mut taken: Vec<String> = RESERVED.iter().map(|s| s.to_string()).collect();
    taken.extend(cube.variables.iter().map(|v| v.name.clone()));
    let mut coordinates = vec!["feature_id".to_string()];
    for (i, field) in cube.zones.schema.fields().iter().enumerate() {
        let name = unique(&field.name, &mut taken);
        let column: Vec<&FieldValue> = cube.zones.features.iter().map(|f| f.attributes.get(i).unwrap_or(&FieldValue::Null)).collect();
        write_field(path, &name, &column)?;
        coordinates.push(name);
    }

    // The dimensions' labels, and the variables.
    let mut labelled = std::collections::BTreeMap::new();
    for var in &cube.variables {
        for dim in &var.dims {
            labelled.entry(dim.name.clone()).or_insert_with(|| dim.labels.clone());
        }
    }
    for (dim, labels) in &labelled {
        let times: Option<Vec<i64>> = if dim == "time" {
            labels.iter().map(|l| crate::drivers::vector::datetime::parse_datetime(l).map(|ms| ms.div_euclid(1000))).collect()
        } else {
            None
        };
        match times {
            Some(seconds) => {
                let attrs =
                    json!({"units": "seconds since 1970-01-01T00:00:00", "calendar": "proleptic_gregorian", "standard_name": "time"});
                numeric(path, dim, &[dim.as_str()], DataType::I64, &i64_bytes(&seconds), attrs)?
            }
            None => strings(path, dim, &[dim.as_str()], labels, json!({}))?,
        }
    }
    let names: Vec<String> = cube.statistics.iter().map(|s| s.name()).collect();
    strings(path, "statistic", &["statistic"], &names, json!({}))?;
    for (v, var) in cube.variables.iter().enumerate() {
        let shape = cube.shape(v);
        let mut dims = vec!["geometry"];
        dims.extend(var.dims.iter().map(|d| d.name.as_str()));
        dims.push("statistic");
        let (data_type, bytes) = match &var.values {
            CubeValues::F32(values) => (DataType::F32, values.iter().flat_map(|v| v.to_le_bytes()).collect()),
            CubeValues::F64(values) => (DataType::F64, f64_bytes(values)),
        };
        let attrs = json!({"geometry": "geometry_container", "grid_mapping": "spatial_ref", "coordinates": coordinates.join(" ")});
        write_array_v3(path, &var.name, &shape, &dims, data_type, &bytes, Some(f64::NAN), attrs)?;
    }
    Ok(())
}

/// A polygon: its exterior ring and its holes.
type Polygon<'a> = (&'a Ring, &'a [Ring]);

/// The polygons of each zone, or an error naming a zone that has none.
fn polygons(cube: &ZonalCube) -> Result<Vec<Vec<Polygon<'_>>>> {
    cube.zones
        .features
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let why =
                |what: &str| Error::InvalidArgument(format!("feature {i} (FID {}) {what}: CF geometries in Zarr cannot store it", f.fid));
            let parts: Vec<Polygon> = match &f.geometry {
                None => return Err(why("has no geometry")),
                Some(Geometry::Polygon { exterior, interiors }) => vec![(exterior, interiors.as_slice())],
                Some(Geometry::MultiPolygon(parts)) => parts.iter().map(|(e, h)| (e, h.as_slice())).collect(),
                Some(_) => return Err(why("is not a polygon")),
            };
            if parts.is_empty() || parts.iter().any(|(e, _)| e.coords().is_empty()) {
                return Err(why("has an empty geometry"));
            }
            Ok(parts)
        })
        .collect()
}

/// The signed area of a ring (shoelace formula): positive when anticlockwise.
fn signed_area(coords: &[Coord]) -> f64 {
    coords.windows(2).map(|w| w[0].x * w[1].y - w[1].x * w[0].y).sum::<f64>() / 2.0
}

fn count(n: usize) -> Result<i32> {
    i32::try_from(n).map_err(|_| Error::InvalidArgument("too many vertices for a Zarr cube's CF geometries".into()))
}

/// `name`, or `name_2`, `name_3`... when it is taken.
fn unique(name: &str, taken: &mut Vec<String>) -> String {
    let mut out = name.to_string();
    let mut k = 2;
    while taken.contains(&out) {
        out = format!("{name}_{k}");
        k += 1;
    }
    taken.push(out.clone());
    out
}

/// A field: integers as int64 (float64 with NaN when one is null), floats as float64, others as
/// strings (null as "").
fn write_field(path: &Path, name: &str, column: &[&FieldValue]) -> Result<()> {
    let dims = ["geometry"];
    let integers: Option<Vec<i64>> = column.iter().map(|v| if let FieldValue::Integer(i) = v { Some(*i) } else { None }).collect();
    if let Some(values) = integers {
        return numeric(path, name, &dims, DataType::I64, &i64_bytes(&values), json!({}));
    }
    let numbers: Option<Vec<f64>> = column
        .iter()
        .map(|v| match v {
            FieldValue::Integer(i) => Some(*i as f64),
            FieldValue::Float(x) => Some(*x),
            FieldValue::Null => Some(f64::NAN),
            _ => None,
        })
        .collect();
    if let Some(values) = numbers.filter(|v| v.iter().any(|x| !x.is_nan())) {
        return write_array_v3(path, name, &[values.len()], &dims, DataType::F64, &f64_bytes(&values), Some(f64::NAN), json!({}));
    }
    let text: Vec<String> = column
        .iter()
        .map(|v| match v {
            FieldValue::Null => String::new(),
            FieldValue::Text(s) | FieldValue::Date(s) => s.clone(),
            FieldValue::Integer(i) => i.to_string(),
            FieldValue::Float(x) => x.to_string(),
            FieldValue::Boolean(b) => b.to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    strings(path, name, &dims, &text, json!({}))
}

fn numeric(path: &Path, name: &str, dims: &[&str], data_type: DataType, bytes: &[u8], attrs: Value) -> Result<()> {
    let n = bytes.len() / data_type.size_bytes();
    write_array_v3(path, name, &[n], dims, data_type, bytes, None, attrs)
}

/// A scalar int32 array holding only attributes (a CF container or grid mapping variable).
fn scalar(path: &Path, name: &str, attrs: Value) -> Result<()> {
    write_array_v3(path, name, &[], &[], DataType::I32, &0i32.to_le_bytes(), Some(0.0), attrs)
}

/// One array of one chunk, zstd-compressed.
#[allow(clippy::too_many_arguments)]
fn write_array_v3(
    path: &Path,
    name: &str,
    shape: &[usize],
    dims: &[&str],
    data_type: DataType,
    bytes: &[u8],
    fill: Option<f64>,
    attrs: Value,
) -> Result<()> {
    let chunks: Vec<usize> = shape.iter().map(|&s| s.max(1)).collect();
    let array = NewArray { shape, chunks: &chunks, data_type, bytes, fill, dims: json!(dims), attrs };
    write_array(&path.join(name), &array, Some("ZSTD"), Version::V3)?;
    Ok(())
}

/// A string array (Zarr v3 `string` with the `vlen-utf8` codec), one chunk, uncompressed.
fn strings(path: &Path, name: &str, dims: &[&str], values: &[String], attrs: Value) -> Result<()> {
    let dir = path.join(name);
    memfs::create_dir_all(&dir.join("c"))?;
    let mut chunk = count(values.len())?.to_le_bytes().to_vec();
    for v in values {
        chunk.extend(count(v.len())?.to_le_bytes());
        chunk.extend(v.as_bytes());
    }
    memfs::write(&dir.join("c").join("0"), chunk)?;
    let meta = json!({
        "zarr_format": 3,
        "node_type": "array",
        "shape": [values.len()],
        "data_type": "string",
        "chunk_grid": {"name": "regular", "configuration": {"chunk_shape": [values.len().max(1)]}},
        "chunk_key_encoding": {"name": "default", "configuration": {"separator": "/"}},
        "fill_value": "",
        "codecs": [{"name": "vlen-utf8", "configuration": {}}],
        "attributes": attrs,
        "dimension_names": dims,
    });
    memfs::write(&dir.join("zarr.json"), serde_json::to_string_pretty(&meta).unwrap_or_default())?;
    Ok(())
}

fn f64_bytes(values: &[f64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn i32_bytes(values: &[i32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn i64_bytes(values: &[i64]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

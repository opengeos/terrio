//! The reading half of `terrio.pyogrio`: a vector layer as pyogrio's `read` returns it, opened
//! through `terrio::open_vector` (and so through the driver allow-list).

use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList, PyString};
use terrio::formats::VectorDriver;
use terrio::{FieldValue, Geometry, VectorOpenOptions};

pub(crate) fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(pyogrio_read, m)?)?;
    Ok(())
}

/// Reads the first layer of a vector dataset, keeping the features `skip_features` to
/// `skip_features + max_features`.
///
/// Returns a dict: `crs` (`"EPSG:xxxx"`, WKT or None), `crs_wkt`, `driver`, `layer`,
/// `feature_count` (of the range), `fields` (`(name, type, width, precision)` per field),
/// `columns` (a list of values per field: int, float, bool, str or None), `wkb` (ISO WKB per
/// feature, without M values, None for no geometry; None when `read_geometry` is false), `fids`,
/// and `geometry_type`: the layer geometry type GDAL's GeoJSON drivers declare for a GeoJSON or
/// GeoJSONSeq layer (as pyogrio names it), None for other drivers.
#[pyfunction]
#[pyo3(signature = (path, skip_features=0, max_features=None, read_geometry=true))]
fn pyogrio_read<'py>(
    py: Python<'py>,
    path: &str,
    skip_features: usize,
    max_features: Option<usize>,
    read_geometry: bool,
) -> PyResult<Bound<'py, PyDict>> {
    // The range is read by the driver: the Shapefile driver decodes only those records.
    let options = VectorOpenOptions { skip_features, max_features, ..Default::default() };
    let (ds, declared) = terrio::vector::with_declared_geometry_types(|| crate::with_warnings(py, || terrio::open_vector(path, &options)));
    let ds = ds?;
    // One layer read by a GeoJSON driver: the type it declares; else the reader infers one.
    let geometry_type = match (ds.driver, declared.as_slice()) {
        (VectorDriver::GeoJSON | VectorDriver::GeoJSONSeq, [declared]) => Some(declared.clone()),
        _ => None,
    };
    let driver = ds.driver.name().to_string();
    let Some(layer) = ds.layers.into_iter().next() else {
        return Err(crate::invalid(format!("'{path}' has no layer")));
    };
    let total = layer.features.len();

    let srs = terrio::crs::SpatialRef::from_layer(&layer);
    let out = PyDict::new(py);
    out.set_item("crs", crate::crs_string(&srs))?;
    out.set_item("crs_wkt", layer.crs_wkt())?;
    out.set_item("driver", driver)?;
    out.set_item("layer", &layer.name)?;
    out.set_item("feature_count", total)?;
    out.set_item("geometry_type", geometry_type)?;
    let fields: Vec<(String, &str, usize, usize)> =
        layer.schema.fields().iter().map(|f| (f.name.clone(), f.field_type.as_str(), f.width, f.precision)).collect();
    out.set_item("fields", fields)?;
    let columns = PyList::empty(py);
    for i in 0..layer.schema.len() {
        let values: Vec<Bound<'py, PyAny>> =
            layer.features.iter().map(|f| value_to_py(py, f.attributes.get(i).unwrap_or(&FieldValue::Null))).collect::<PyResult<_>>()?;
        columns.append(PyList::new(py, values)?)?;
    }
    out.set_item("columns", columns)?;
    if read_geometry {
        let wkb: Vec<Bound<'py, PyAny>> = layer
            .features
            .iter()
            .map(|f| match &f.geometry {
                Some(g) => {
                    let mut buf = Vec::new();
                    iso_wkb(g, &mut buf);
                    PyBytes::new(py, &buf).into_any()
                }
                None => py.None().into_bound(py),
            })
            .collect();
        out.set_item("wkb", PyList::new(py, wkb)?)?;
    } else {
        out.set_item("wkb", py.None())?;
    }
    out.set_item("fids", layer.features.iter().map(|f| f.fid).collect::<Vec<u64>>())?;
    Ok(out)
}

fn value_to_py<'py>(py: Python<'py>, v: &FieldValue) -> PyResult<Bound<'py, PyAny>> {
    Ok(match v {
        FieldValue::Integer(i) => i.into_pyobject(py)?.into_any(),
        FieldValue::Float(f) => f.into_pyobject(py)?.into_any(),
        FieldValue::Text(s) | FieldValue::Date(s) | FieldValue::DateTime(s) => PyString::new(py, s).into_any(),
        FieldValue::Boolean(b) => b.into_pyobject(py)?.to_owned().into_any(),
        FieldValue::Blob(b) => PyBytes::new(py, b).into_any(),
        FieldValue::Null => py.None().into_bound(py),
    })
}

/// ISO WKB, little endian, as GDAL exports geometries to pyogrio: Z kept, M dropped (pyogrio
/// reads measured geometries without their measures), rings closed, and an empty exterior ring
/// written as an empty polygon.
fn iso_wkb(g: &Geometry, out: &mut Vec<u8>) {
    let z = terrio::vector::geometry_has_z(g);
    let head = |out: &mut Vec<u8>, code: u32| {
        out.push(1);
        out.extend_from_slice(&(code + if z { 1000 } else { 0 }).to_le_bytes());
    };
    let coord = |out: &mut Vec<u8>, c: &terrio::vector::Coord| {
        out.extend_from_slice(&c.x.to_le_bytes());
        out.extend_from_slice(&c.y.to_le_bytes());
        if z {
            out.extend_from_slice(&c.z.unwrap_or(0.0).to_le_bytes());
        }
    };
    let points = |out: &mut Vec<u8>, cs: &[terrio::vector::Coord]| {
        out.extend_from_slice(&(cs.len() as u32).to_le_bytes());
        cs.iter().for_each(|c| coord(out, c));
    };
    let ring = |out: &mut Vec<u8>, r: &terrio::vector::Ring| {
        let cs = &r.0;
        let closed = cs.len() > 1 && (cs[0].x, cs[0].y) != (cs[cs.len() - 1].x, cs[cs.len() - 1].y);
        out.extend_from_slice(&((cs.len() + usize::from(closed)) as u32).to_le_bytes());
        cs.iter().for_each(|c| coord(out, c));
        if closed {
            coord(out, &cs[0]);
        }
    };
    let polygon = |out: &mut Vec<u8>, e: &terrio::vector::Ring, holes: &[terrio::vector::Ring]| {
        head(out, 3);
        if e.0.is_empty() && holes.is_empty() {
            out.extend_from_slice(&0u32.to_le_bytes());
            return;
        }
        out.extend_from_slice(&((1 + holes.len()) as u32).to_le_bytes());
        ring(out, e);
        holes.iter().for_each(|h| ring(out, h));
    };
    match g {
        Geometry::Point(c) => {
            head(out, 1);
            coord(out, c);
        }
        Geometry::LineString(cs) => {
            head(out, 2);
            points(out, cs);
        }
        Geometry::Polygon { exterior, interiors } => polygon(out, exterior, interiors),
        Geometry::MultiPoint(cs) => {
            head(out, 4);
            out.extend_from_slice(&(cs.len() as u32).to_le_bytes());
            for c in cs {
                head(out, 1);
                coord(out, c);
            }
        }
        Geometry::MultiLineString(ls) => {
            head(out, 5);
            out.extend_from_slice(&(ls.len() as u32).to_le_bytes());
            for l in ls {
                head(out, 2);
                points(out, l);
            }
        }
        Geometry::MultiPolygon(ps) => {
            head(out, 6);
            out.extend_from_slice(&(ps.len() as u32).to_le_bytes());
            for (e, holes) in ps {
                polygon(out, e, holes);
            }
        }
        Geometry::GeometryCollection(gs) => {
            head(out, 7);
            out.extend_from_slice(&(gs.len() as u32).to_le_bytes());
            for g in gs {
                iso_wkb(g, out);
            }
        }
    }
}

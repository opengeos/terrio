//! Line, polygon and point families stored under `Geometry/`: 1D cross-section cut lines, river
//! lines, storage areas, structure centerlines, river junctions and pump stations, the 2D
//! boundary-condition lines, breaklines, refinement regions, reference lines and points, and
//! pipe-network conduits and nodes.
//!
//! The engine reads each family exactly (attribute table, Info/Parts/Points, one source
//! identity per feature). This module only builds terrio features from them:
//!
//! - one feature per engine feature, in stored order, with `source_row` (the row of the family's
//!   attribute table, or of its Info table when it has none) as the identity field;
//! - every attribute-table column as a field named after the stored column (snake case), with
//!   its stored values (text, integer, float; bitfields as bytes);
//! - a line with one part is a LineString, with several a MultiLineString; a polygon family's
//!   single part is a Polygon. Multi-part polygons are refused: which parts are holes is not a
//!   proven layout.

use super::{HecRasHdf, error, field_name, id_field};
use crate::vector::{Coord, Ring};
use crate::{Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};
use ras_commander_fast_hdf::hdf::AttributeType;
use ras_commander_fast_hdf::hdf::lines::GeometryFamily;
use ras_commander_fast_hdf::hdf::metadata::{CompoundColumn, CompoundTable, MetadataValue};
use ras_commander_fast_hdf::hdf::xs_geometry::{OneDLineFamily, OneDPointData};
use ras_commander_fast_hdf::hdf::{Identity, NumericArray, NumericValues};
use ras_commander_fast_hdf::{Capability, Reported};
use std::ops::Range;

/// The family layers, by layer name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Family {
    OneD(OneDLineFamily, Shape),
    TwoD(GeometryFamily, Shape),
    ReferencePoints,
    /// A one-point-per-row family (`Attributes` plus `Points`).
    Points(PointFamily),
}

/// The point families the engine reads with one stored point per attribute row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PointFamily {
    Junctions,
    PumpStations,
    PipeNodes,
}

impl PointFamily {
    fn group(self) -> &'static str {
        match self {
            PointFamily::Junctions => "Geometry/Junctions",
            PointFamily::PumpStations => "Geometry/Pump Stations",
            PointFamily::PipeNodes => "Geometry/Pipe Nodes",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shape {
    Line,
    Polygon,
}

/// Layer names of the family layers, with their engine families, in listing order.
pub(crate) const FAMILY_LAYERS: &[(&str, Family)] = &[
    ("cross_sections", Family::OneD(OneDLineFamily::CrossSectionCutLines, Shape::Line)),
    ("river_centerlines", Family::OneD(OneDLineFamily::RiverCenterlines, Shape::Line)),
    ("river_bank_lines", Family::OneD(OneDLineFamily::RiverBankLines, Shape::Line)),
    ("river_edge_lines", Family::OneD(OneDLineFamily::RiverEdgeLines, Shape::Line)),
    ("river_flow_paths", Family::OneD(OneDLineFamily::RiverFlowPaths, Shape::Line)),
    ("storage_areas", Family::OneD(OneDLineFamily::StorageAreaPolygons, Shape::Polygon)),
    ("structures", Family::OneD(OneDLineFamily::StructureCenterlines, Shape::Line)),
    ("bc_lines", Family::TwoD(GeometryFamily::BoundaryConditions, Shape::Line)),
    ("breaklines", Family::TwoD(GeometryFamily::Breaklines, Shape::Line)),
    ("refinement_regions", Family::TwoD(GeometryFamily::RefinementRegions, Shape::Polygon)),
    ("reference_lines", Family::TwoD(GeometryFamily::ReferenceLines, Shape::Line)),
    ("reference_points", Family::ReferencePoints),
    ("river_junctions", Family::Points(PointFamily::Junctions)),
    ("pump_stations", Family::Points(PointFamily::PumpStations)),
    ("pipe_conduits", Family::OneD(OneDLineFamily::PipeConduits, Shape::Line)),
    ("pipe_nodes", Family::Points(PointFamily::PipeNodes)),
];

/// The byte budget of each family dataset under a metadata and an array limit: the smaller.
/// Family datasets are attribute tables (metadata) and coordinate arrays, which the engine
/// bounds with one limit, so a caller's limit of either kind is never exceeded. The defaults give
/// the engine's metadata default, 64 MiB.
pub(crate) fn family_budget(metadata_max_bytes: usize, max_bytes: usize) -> usize {
    metadata_max_bytes.min(max_bytes)
}

impl HecRasHdf {
    /// Whether the file stores the family: the engine reads it, within `max_bytes` per dataset,
    /// and reports anything but canonical absence. A family missing a dataset it needs (the
    /// engine's `NotFound`, such as packed-line `Info` without its `Parts`) is not listed.
    /// Unsupported, corrupt or over-budget families are listed, so that reading them shows the
    /// refusal; host I/O failures propagate.
    pub(crate) fn family_present(&self, family: Family, max_bytes: usize) -> crate::Result<bool> {
        use ras_commander_fast_hdf::ErrorKind;
        let s = &self.session;
        let status = match family {
            Family::OneD(f, _) => s.one_d_line_family(f, max_bytes).map(|r| r.is_unavailable()),
            Family::TwoD(f, _) => s.geometry_family(f, max_bytes).map(|r| r.is_unavailable()),
            Family::ReferencePoints => s.reference_point_geometry(max_bytes).map(|r| r.is_unavailable()),
            Family::Points(f) => self.point_data(f, max_bytes).map(|r| r.is_unavailable()),
        };
        match status {
            Ok(unavailable) => Ok(!unavailable),
            Err(e) if e.kind == ErrorKind::NotFound => Ok(false),
            Err(e) if matches!(e.kind, ErrorKind::Io | ErrorKind::Internal | ErrorKind::FileNotFound) => Err(error::from_engine(e)),
            Err(_) => Ok(true),
        }
    }

    /// Reads one family layer. The smaller of the two budgets ([`family_budget`]) bounds each
    /// dataset the engine reads (the attribute table, the Info, Parts and Points arrays), and
    /// `metadata_max_bytes` the root attributes (the CRS); data above them is refused, never
    /// decoded.
    pub(crate) fn family_layer(&self, name: &str, family: Family, metadata_max_bytes: usize, max_bytes: usize) -> crate::Result<Layer> {
        let max_bytes = family_budget(metadata_max_bytes, max_bytes);
        let s = &self.session;
        let mut builder = Builder::new(self, name, metadata_max_bytes)?;
        match family {
            Family::OneD(f, shape) => {
                let data = error::take(s.one_d_line_family(f, max_bytes).map_err(error::from_engine)?, f.group_path())?;
                let table = data.attributes.as_ref().map(|a| &a.table);
                let points = xy(&data.points, f.group_path())?;
                builder.declare(table, if shape == Shape::Polygon { GeometryType::Polygon } else { GeometryType::LineString })?;
                for feature in &data.features {
                    let geometry = parts_geometry(&points, &feature.parts, shape, name, &feature.id)?;
                    builder.push(&feature.id, geometry, table)?;
                }
            }
            Family::TwoD(f, shape) => {
                let data = error::take(s.geometry_family(f, max_bytes).map_err(error::from_engine)?, f.group_path())?;
                let table = Some(&data.attributes.table);
                let points = xy(&data.points, f.group_path())?;
                builder.declare(table, if shape == Shape::Polygon { GeometryType::Polygon } else { GeometryType::LineString })?;
                for feature in &data.features {
                    let geometry = parts_geometry(&points, &feature.parts, shape, name, &feature.id)?;
                    builder.push(&feature.id, geometry, table)?;
                }
            }
            Family::ReferencePoints => {
                let data = s.reference_point_geometry(max_bytes).map_err(error::from_engine)?;
                let data = error::take(data, "Geometry/Reference Points")?;
                let table = Some(&data.attributes.table);
                let points = xy(&data.points, "Geometry/Reference Points")?;
                builder.declare(table, GeometryType::Point)?;
                for feature in &data.features {
                    let [x, y] = *usize::try_from(feature.point_row)
                        .ok()
                        .and_then(|r| points.get(r))
                        .ok_or_else(|| crate::Error::InvalidData(format!("{name} row {}: point row out of range", feature.id.row)))?;
                    builder.push(&feature.id, Geometry::Point(Coord::xy(x, y)), table)?;
                }
            }
            Family::Points(f) => {
                let data = error::take(self.point_data(f, max_bytes).map_err(error::from_engine)?, f.group())?;
                let table = Some(&data.attributes.table);
                let points = xy(&data.points, f.group())?;
                builder.declare(table, GeometryType::Point)?;
                // Each feature's point is the `Points` row of its own attribute row.
                for id in &data.features {
                    let [x, y] = *usize::try_from(id.row)
                        .ok()
                        .and_then(|r| points.get(r))
                        .ok_or_else(|| crate::Error::InvalidData(format!("{name} row {}: no stored point", id.row)))?;
                    builder.push(id, Geometry::Point(Coord::xy(x, y)), table)?;
                }
            }
        }
        Ok(builder.layer)
    }

    fn point_data(&self, family: PointFamily, max_bytes: usize) -> ras_commander_fast_hdf::Result<Reported<OneDPointData>> {
        match family {
            PointFamily::Junctions => self.session.river_junctions(max_bytes),
            PointFamily::PumpStations => self.session.pump_stations(max_bytes),
            PointFamily::PipeNodes => self.session.pipe_nodes(max_bytes),
        }
    }
}

/// Builds a family layer: `source_row`, then one field per attribute-table column.
struct Builder {
    layer: Layer,
}

impl Builder {
    fn new(hdf: &HecRasHdf, name: &str, metadata_max_bytes: usize) -> crate::Result<Self> {
        let mut layer = Layer::new(name);
        if let Some(wkt) = hdf.crs_within(metadata_max_bytes)? {
            layer = layer.with_crs_wkt(wkt);
        }
        Ok(Self { layer })
    }

    fn declare(&mut self, table: Option<&CompoundTable>, geom: GeometryType) -> crate::Result<()> {
        self.layer.geom_type = Some(geom);
        self.layer.add_field(id_field("source_row", FieldType::Integer));
        declare_columns(&mut self.layer, table.map(|t| t.columns.as_slice()).unwrap_or_default())
    }

    fn push(&mut self, id: &Identity, geometry: Geometry, table: Option<&CompoundTable>) -> crate::Result<()> {
        let row = usize::try_from(id.row).map_err(|_| crate::Error::InvalidData("row overflow".into()))?;
        let mut attributes =
            vec![FieldValue::Integer(i64::try_from(id.row).map_err(|_| crate::Error::InvalidData("row overflow".into()))?)];
        for column in table.map(|t| t.columns.as_slice()).unwrap_or_default() {
            attributes.push(column_value(column, row, &self.layer.name)?);
        }
        let mut feature = Feature::new();
        feature.fid = self.layer.features.len() as u64;
        feature.geometry = Some(geometry);
        feature.attributes = attributes;
        self.layer.push(feature);
        Ok(())
    }
}

fn column_capability(value: &MetadataValue) -> Capability {
    value.capability()
}

/// Adds one field per stored attribute-table column, named after the column (snake case), with
/// its stored value type (text, integer, float; bitfields as bytes). Unsupported or corrupt
/// columns, and names that collide, are errors.
pub(crate) fn declare_columns(layer: &mut Layer, columns: &[CompoundColumn]) -> crate::Result<()> {
    for column in columns {
        let kind = match (&column.value, &column.attribute_type) {
            (MetadataValue::Strings(_), _) => FieldType::Text,
            (MetadataValue::Signed(_) | MetadataValue::Unsigned(_), _) => FieldType::Integer,
            (MetadataValue::Float(_), _) => FieldType::Float,
            (MetadataValue::Bitfield(_), AttributeType::Bitfield { .. }) => FieldType::Blob,
            (MetadataValue::Unsupported { .. } | MetadataValue::Corrupt { .. }, _) => {
                let what = format!("{} column {:?}", layer.name, column.name);
                error::require(&column_capability(&column.value), &what)?;
                return Err(crate::Error::InvalidData(format!("{what}: refused by the engine but reported as supported")));
            }
            _ => return Err(crate::Error::Unsupported(format!("{} column {:?}: unmapped value type", layer.name, column.name))),
        };
        let name = field_name(&column.name);
        if name.is_empty() || layer.schema.fields().iter().any(|f| f.name == name) {
            return Err(crate::Error::Unsupported(format!(
                "{} column {:?} gives an empty or repeated field name {name:?}",
                layer.name, column.name
            )));
        }
        layer.add_field(FieldDef::new(name, kind));
    }
    Ok(())
}

/// The stored value of `column` at `row`, as [`declare_columns`] typed it.
pub(crate) fn column_value(column: &CompoundColumn, row: usize, layer: &str) -> crate::Result<FieldValue> {
    let missing = || crate::Error::InvalidData(format!("{layer} column {:?}: no row {row}", column.name));
    Ok(match (&column.value, &column.attribute_type) {
        (MetadataValue::Strings(v), _) => FieldValue::Text(v.get(row).ok_or_else(missing)?.clone()),
        (MetadataValue::Signed(v), _) => FieldValue::Integer(*v.get(row).ok_or_else(missing)?),
        (MetadataValue::Unsigned(v), _) => FieldValue::Integer(
            i64::try_from(*v.get(row).ok_or_else(missing)?)
                .map_err(|_| crate::Error::Unsupported(format!("{layer} column {:?}: value above i64", column.name)))?,
        ),
        (MetadataValue::Float(v), _) => FieldValue::Float(*v.get(row).ok_or_else(missing)?),
        (MetadataValue::Bitfield(v), AttributeType::Bitfield { size, .. }) => {
            let size = *size;
            FieldValue::Blob(v.get(row * size..(row + 1) * size).ok_or_else(missing)?.to_vec())
        }
        _ => return Err(crate::Error::Unsupported(format!("{layer} column {:?}: unmapped value type", column.name))),
    })
}

/// Rows of a stored `(n, 2)` float Points array (f32 widened exactly).
fn xy(points: &NumericArray, what: &str) -> crate::Result<Vec<[f64; 2]>> {
    if points.shape.len() != 2 || points.shape[1] != 2 {
        return Err(crate::Error::Unsupported(format!("{what} Points: shape {:?}, not (rows, 2)", points.shape)));
    }
    Ok(match &points.values {
        NumericValues::F64(v) => v.as_chunks::<2>().0.to_vec(),
        NumericValues::F32(v) => v.as_chunks::<2>().0.iter().map(|c| [f64::from(c[0]), f64::from(c[1])]).collect(),
        _ => return Err(crate::Error::Unsupported(format!("{what} Points: not floats"))),
    })
}

fn parts_geometry(points: &[[f64; 2]], parts: &[Range<u64>], shape: Shape, layer: &str, id: &Identity) -> crate::Result<Geometry> {
    let bad = |m: &str| crate::Error::InvalidData(format!("{layer} row {}: {m}", id.row));
    let mut coords = Vec::with_capacity(parts.len());
    for part in parts {
        let range = usize::try_from(part.start).map_err(|_| bad("range"))?..usize::try_from(part.end).map_err(|_| bad("range"))?;
        let slice = points.get(range).ok_or_else(|| bad("part outside Points"))?;
        coords.push(slice.iter().map(|p| Coord::xy(p[0], p[1])).collect::<Vec<_>>());
    }
    match (shape, coords.len()) {
        (_, 0) => Err(bad("no parts")),
        (Shape::Line, 1) => Ok(Geometry::LineString(coords.pop().unwrap_or_default())),
        (Shape::Line, _) => Ok(Geometry::MultiLineString(coords)),
        (Shape::Polygon, 1) => {
            let mut ring = coords.pop().unwrap_or_default();
            if ring.len() > 1 && ring.first() == ring.last() {
                ring.pop();
            }
            if ring.len() < 3 {
                return Err(bad("polygon with fewer than three vertices"));
            }
            Ok(Geometry::Polygon { exterior: Ring::new(ring), interiors: Vec::new() })
        }
        (Shape::Polygon, n) => Err(crate::Error::Unsupported(format!(
            "{layer} row {}: {n}-part polygon; which parts are holes is not a proven layout",
            id.row
        ))),
    }
}

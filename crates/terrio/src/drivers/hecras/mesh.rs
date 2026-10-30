//! 2D mesh layers: cell polygons, cell centre points, face lines, face points and area perimeters, with
//! optional stored values joined by cell or face: summary results, static mesh arrays, time-step
//! values and the engine's reductions of stored time series.

use super::{HecRasHdf, MESH_CELL_POINTS, MESH_CELLS, MESH_FACE_POINTS, MESH_FACES, MESH_PERIMETERS, error, id_field};
use crate::vector::{Coord, Ring};
use crate::{Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer};
use ras_commander_fast_hdf::hdf::mesh_geometry::MeshGeometryField;
use ras_commander_fast_hdf::hdf::metadata::{CompoundTable, MetadataValue};
use ras_commander_fast_hdf::hdf::{DEFAULT_MAX_BYTES, DEFAULT_READ_MAX_BYTES, NumericValues};
use ras_commander_fast_hdf::mesh::{GeometryPolicy, OmissionKind};

/// What to read into a mesh layer.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MeshOptions {
    /// Only these 2D flow areas (exact names), in this order. `None` reads every mesh.
    pub meshes: Option<Vec<String>>,
    /// The engine's geometry policy for cell polygons. [`GeometryPolicy::Strict`] (the default)
    /// fails on an invalid cell polygon; [`GeometryPolicy::QuarantineInvalid`] leaves it out and
    /// reports it in [`MeshLayer::omitted`]. Structural corruption fails under both.
    pub policy: GeometryPolicy,
    /// Stored `Summary Output` variables (exact dataset names, such as `Maximum Water Surface`
    /// or `Maximum Face Velocity`) to join: cell variables to cell layers, face variables to
    /// the face layer. Needs a plan HDF with results and the `results` feature.
    pub summaries: Vec<String>,
    /// Stored static mesh arrays to join ([`MeshAttribute`]).
    pub attributes: Vec<MeshAttribute>,
    /// Stored `Unsteady Time Series` variables (exact dataset names, such as `Water Surface`,
    /// `Face Velocity` or `Depth`) to join at [`MeshOptions::time_step`]. Needs the `results`
    /// feature.
    pub series: Vec<String>,
    /// The stored output row (zero-based) that [`MeshOptions::series`] are read at. Required
    /// with `series`, and only allowed with them.
    pub time_step: Option<u64>,
    /// Stored `Unsteady Time Series` variables reduced over every stored row by the engine,
    /// joined with the row the extreme was found at. Needs the `results` feature.
    pub extremes: Vec<(String, Extreme)>,
    /// Byte limit for each metadata object the engine consults.
    pub metadata_max_bytes: usize,
    /// Byte limit for each array read (one mesh topology, one summary array, one time row).
    pub max_bytes: usize,
}

impl Default for MeshOptions {
    fn default() -> Self {
        Self {
            meshes: None,
            policy: GeometryPolicy::Strict,
            summaries: Vec::new(),
            attributes: Vec::new(),
            series: Vec::new(),
            time_step: None,
            extremes: Vec::new(),
            metadata_max_bytes: DEFAULT_MAX_BYTES,
            max_bytes: DEFAULT_READ_MAX_BYTES,
        }
    }
}

impl MeshOptions {
    /// Reads only these meshes.
    pub fn with_meshes<S: Into<String>>(mut self, meshes: impl IntoIterator<Item = S>) -> Self {
        self.meshes = Some(meshes.into_iter().map(Into::into).collect());
        self
    }

    /// Joins a stored summary variable.
    pub fn with_summary(mut self, variable: impl Into<String>) -> Self {
        self.summaries.push(variable.into());
        self
    }

    /// Joins a stored static mesh array.
    pub fn with_attribute(mut self, attribute: MeshAttribute) -> Self {
        self.attributes.push(attribute);
        self
    }

    /// Joins a stored time-series variable at [`MeshOptions::time_step`].
    pub fn with_series(mut self, variable: impl Into<String>) -> Self {
        self.series.push(variable.into());
        self
    }

    /// Sets the stored output row that time-series variables are read at.
    pub fn at_time_step(mut self, row: u64) -> Self {
        self.time_step = Some(row);
        self
    }

    /// Joins the engine's reduction of a stored time-series variable over every stored row.
    pub fn with_extreme(mut self, variable: impl Into<String>, extreme: Extreme) -> Self {
        self.extremes.push((variable.into(), extreme));
        self
    }

    /// Sets the geometry policy.
    pub fn with_policy(mut self, policy: GeometryPolicy) -> Self {
        self.policy = policy;
        self
    }

    fn has_joins(&self) -> bool {
        !(self.summaries.is_empty() && self.attributes.is_empty() && self.series.is_empty() && self.extremes.is_empty())
    }
}

/// A stored static mesh array joined by element. Each reads one stored dataset of
/// `Geometry/2D Flow Areas/<mesh>`; values are the stored numbers (`f32` widened exactly).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MeshAttribute {
    /// `minimum_elevation`: `Cells Minimum Elevation` on cell layers, `Faces Minimum Elevation`
    /// on the face layer.
    MinimumElevation,
    /// `mannings_n`: `Cells Center Manning's n` (cell layers only).
    ManningsN,
    /// `normal_x`, `normal_y`, `length`: the three columns of `Faces NormalUnitVector and
    /// Length` (face layer only).
    FaceNormalAndLength,
    /// `cell_a`, `cell_b`: the two stored cell indices of `Faces Cell Indexes` (face layer only),
    /// unconverted.
    FaceCells,
}

/// Which extreme of a stored time series to join.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Extreme {
    /// `<variable>_min` and `<variable>_min_row`.
    Minimum,
    /// `<variable>_max` and `<variable>_max_row`.
    Maximum,
}

/// A cell the engine did not turn into a polygon.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct OmittedCell {
    /// The 2D flow area.
    pub mesh_name: String,
    /// The stored cell index.
    pub cell_id: i64,
    /// Why: fewer than three faces (ghost and boundary cells), none, or invalid geometry
    /// (quarantine policy only).
    pub kind: OmissionKind,
    /// The engine's reason text (display only).
    pub reason: String,
}

/// A summary variable joined to a layer.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SummaryField {
    /// The stored variable name.
    pub variable: String,
    /// The field holding its value row.
    pub field: String,
    /// The field holding its stored time row, when the engine proves the second row is `Time`.
    /// Values are the stored numbers (days in HEC-RAS files); no timestamp is derived.
    pub time_field: Option<String>,
    /// The units the engine reports for the dataset, exactly as stored; `None` when it reports
    /// none it can prove. Never inferred.
    pub units: Option<String>,
    /// The dataset paths read, one per mesh.
    pub datasets: Vec<String>,
}

/// A static attribute, time-step value or series extreme joined to a layer.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct JoinedField {
    /// What was read: the stored dataset name (`Cells Minimum Elevation`, `Water Surface`).
    pub source: String,
    /// The fields it fills, in schema order.
    pub fields: Vec<String>,
    /// The units the engine reports for the dataset, exactly as stored; `None` when it reports
    /// none it can prove (static mesh arrays carry no unit attribute). Never inferred.
    pub units: Option<String>,
    /// The dataset paths read, one per mesh.
    pub datasets: Vec<String>,
}

/// The stored output row that time-step values were read at.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct TimeStep {
    /// The stored row (zero-based).
    pub row: u64,
    /// The stored `Time` value (days, unconverted), when stored.
    pub days: Option<f64>,
    /// The stored `Time Date Stamp` text, exactly as stored. No time zone is implied.
    pub date_stamp: Option<String>,
}

/// A mesh layer and what the engine reported while building it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct MeshLayer {
    /// The layer, in the stored CRS.
    pub layer: Layer,
    /// Cells left out of a cell polygon layer, by mesh (empty for other layers).
    pub omitted: Vec<OmittedCell>,
    /// The joined summary variables.
    pub summaries: Vec<SummaryField>,
    /// The joined static attributes, time-step values and series extremes, in that order.
    pub joined: Vec<JoinedField>,
    /// The output row of the joined time-step values (`None` without [`MeshOptions::series`]).
    pub time_step: Option<TimeStep>,
}

/// The element a mesh layer has one feature per.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Element {
    Cell,
    Face,
}

impl Element {
    fn label(self) -> &'static str {
        match self {
            Element::Cell => "cells",
            Element::Face => "faces",
        }
    }
}

/// One requested join.
#[derive(Clone, Copy)]
enum Join<'a> {
    Summary(&'a str),
    Attribute(MeshAttribute),
    Series(&'a str, u64),
    Extreme(&'a str, Extreme),
}

/// One join for one mesh: per field, one value per stored element.
pub(crate) struct Column {
    source: String,
    dataset: String,
    units: Option<String>,
    elements: usize,
    fields: Vec<(String, FieldType)>,
    /// `values[field][element]`.
    values: Vec<Vec<FieldValue>>,
}

impl Column {
    fn push_values(&self, id: i64, mesh: &str, out: &mut Vec<FieldValue>) -> crate::Result<()> {
        let i = usize::try_from(id).ok().filter(|&i| i < self.elements).ok_or_else(|| {
            crate::Error::InvalidData(format!("{mesh} element {id} is outside {} ({} elements)", self.dataset, self.elements))
        })?;
        out.extend(self.values.iter().map(|v| v[i].clone()));
        Ok(())
    }
}

impl HecRasHdf {
    fn mesh_list(&self, options: &MeshOptions) -> crate::Result<Vec<String>> {
        let meshes = match &options.meshes {
            Some(m) => m.clone(),
            None => self.mesh_names()?,
        };
        if meshes.is_empty() {
            return Err(crate::Error::InvalidArgument(format!("{}: no 2D flow areas to read", self.path.display())));
        }
        match (options.series.is_empty(), options.time_step) {
            (false, None) => Err(crate::Error::InvalidArgument("time-series values need a time_step (stored output row)".into())),
            (true, Some(_)) => Err(crate::Error::InvalidArgument("a time_step needs at least one time-series variable".into())),
            _ => Ok(meshes),
        }
    }

    fn new_layer(&self, name: &str, geom: GeometryType, id: Option<&str>, options: &MeshOptions) -> crate::Result<Layer> {
        let mut layer = Layer::new(name).with_geom_type(geom);
        if let Some(wkt) = self.crs_within(options.metadata_max_bytes)? {
            layer = layer.with_crs_wkt(wkt);
        }
        layer.add_field(id_field("mesh_name", FieldType::Text));
        if let Some(id) = id {
            layer.add_field(id_field(id, FieldType::Integer));
        }
        Ok(layer)
    }

    fn read_column(&self, mesh: &str, join: Join<'_>, element: Element, options: &MeshOptions) -> crate::Result<Column> {
        match join {
            Join::Attribute(attribute) => attribute_column(self, mesh, attribute, element, options),
            Join::Summary(variable) => results::summary_column(self, mesh, variable, element, options),
            Join::Series(variable, row) => results::series_column(self, mesh, variable, row, element, options),
            Join::Extreme(variable, extreme) => results::extreme_column(self, mesh, variable, extreme, element, options),
        }
    }

    /// Builds a per-element layer: `rows(mesh)` gives each feature's element ID, geometry and extra
    /// attributes, and the requested joins of `element` are added by element ID.
    fn element_layer(
        &self,
        name: &str,
        geom: GeometryType,
        element: Element,
        extra: &[(&str, FieldType)],
        options: &MeshOptions,
        mut rows: impl FnMut(&str, &mut Vec<OmittedCell>) -> crate::Result<Vec<(i64, Geometry, Vec<FieldValue>)>>,
    ) -> crate::Result<MeshLayer> {
        let meshes = self.mesh_list(options)?;
        let mut layer = self.new_layer(name, geom, Some(if element == Element::Cell { "cell_id" } else { "face_id" }), options)?;
        for (field, kind) in extra {
            layer.add_field(FieldDef::new(*field, *kind));
        }
        let joins: Vec<Join<'_>> = options
            .summaries
            .iter()
            .map(|v| Join::Summary(v))
            .chain(options.attributes.iter().map(|a| Join::Attribute(*a)))
            .chain(options.series.iter().map(|v| Join::Series(v, options.time_step.unwrap_or_default())))
            .chain(options.extremes.iter().map(|(v, e)| Join::Extreme(v, *e)))
            .collect();
        let time_step = match options.time_step {
            Some(row) => Some(results::time_step(self, row, options)?),
            None => None,
        };
        let mut reports: Vec<JoinedField> = Vec::new();
        let mut omitted = Vec::new();
        for mesh in &meshes {
            let features = rows(mesh, &mut omitted)?;
            let mut columns = Vec::with_capacity(joins.len());
            for (i, join) in joins.iter().enumerate() {
                let column = self.read_column(mesh, *join, element, options)?;
                if element == Element::Face && column.elements != features.len() {
                    return Err(crate::Error::InvalidData(format!(
                        "{mesh} {}: {} elements for {} stored faces",
                        column.dataset,
                        column.elements,
                        features.len()
                    )));
                }
                if reports.len() == i {
                    reports.push(declare(&mut layer, &column)?);
                } else {
                    same_column(&layer, &reports[i], &column)?;
                }
                reports[i].datasets.push(column.dataset.clone());
                columns.push(column);
            }
            for (id, geometry, extra_values) in features {
                let mut feature = Feature::new();
                feature.fid = layer.features.len() as u64;
                feature.geometry = Some(geometry);
                feature.attributes = vec![FieldValue::Text(mesh.clone()), FieldValue::Integer(id)];
                feature.attributes.extend(extra_values);
                for column in &columns {
                    column.push_values(id, mesh, &mut feature.attributes)?;
                }
                layer.push(feature);
            }
        }
        let joined = reports.split_off(options.summaries.len());
        let summaries = reports
            .into_iter()
            .map(|r| SummaryField {
                variable: r.source,
                field: r.fields[0].clone(),
                time_field: r.fields.get(1).cloned(),
                units: r.units,
                datasets: r.datasets,
            })
            .collect();
        Ok(MeshLayer { layer, omitted, summaries, joined, time_step })
    }

    /// Reads the 2D mesh cell polygons as the [`MESH_CELLS`] layer: `mesh_name`, `cell_id`, then
    /// the joined cell values ([`MeshOptions`]).
    ///
    /// Every requested mesh must read completely: one corrupt or unsupported mesh fails the call.
    pub fn mesh_cells(&self, options: &MeshOptions) -> crate::Result<MeshLayer> {
        self.element_layer(MESH_CELLS, GeometryType::Polygon, Element::Cell, &[], options, |mesh, omitted| {
            let named = self.session.read_topology_bounded(mesh, options.max_bytes).map_err(error::from_engine)?;
            let packed = named.topology.assemble_with_session(options.policy, &self.session).map_err(error::from_engine)?;
            omitted.extend(packed.diagnostics.iter().map(|d| OmittedCell {
                mesh_name: mesh.to_string(),
                cell_id: d.cell_id,
                kind: d.kind,
                reason: d.reason.clone(),
            }));
            let mut out = Vec::with_capacity(packed.cell_ids.len());
            for (row, &cell_id) in packed.cell_ids.iter().enumerate() {
                let ring = slice(&packed.coordinates, &packed.offsets, row, mesh, "cell", cell_id)?;
                out.push((cell_id, polygon(ring, mesh, &format!("cell {cell_id}"))?, Vec::new()));
            }
            Ok(out)
        })
    }

    /// Reads every stored cell centre as the [`MESH_CELL_POINTS`] layer: `mesh_name`, `cell_id`,
    /// `is_ghost`, then the joined cell values ([`MeshOptions`]).
    ///
    /// Ghost cells are the stored rows at and above the physical cell count the engine reads
    /// from `Geometry/2D Flow Areas/Attributes`; files without that table (HEC-RAS 5.0) are
    /// refused rather than guessed.
    pub fn mesh_cell_points(&self, options: &MeshOptions) -> crate::Result<MeshLayer> {
        let extra = [("is_ghost", FieldType::Boolean)];
        self.element_layer(MESH_CELL_POINTS, GeometryType::Point, Element::Cell, &extra, options, |mesh, _| {
            let geometry = self.session.mesh_geometry(mesh, options.metadata_max_bytes).map_err(error::from_engine)?;
            let physical = geometry.mesh.physical_cells;
            let array = self
                .session
                .read_mesh_geometry_cached(&geometry, MeshGeometryField::CellCenters, None, options.max_bytes)
                .map_err(error::from_engine)?;
            let xy = xy_rows(array.shape.as_slice(), array.values, mesh, "Cells Center Coordinate")?;
            Ok(xy
                .into_iter()
                .enumerate()
                .map(|(i, [x, y])| (i as i64, Geometry::Point(Coord::xy(x, y)), vec![FieldValue::Boolean(i as u64 >= physical)]))
                .collect())
        })
    }

    /// Reads every stored face as the [`MESH_FACES`] layer: `mesh_name`, `face_id`, then the
    /// joined face values ([`MeshOptions`]). Lines are the engine's face polylines (start point,
    /// stored perimeter vertices, end point), as ras-commander's `get_mesh_cell_faces` builds
    /// them.
    pub fn mesh_faces(&self, options: &MeshOptions) -> crate::Result<MeshLayer> {
        self.element_layer(MESH_FACES, GeometryType::LineString, Element::Face, &[], options, |mesh, _| {
            let named = self.session.read_topology_bounded(mesh, options.max_bytes).map_err(error::from_engine)?;
            let lines = named.topology.face_lines().map_err(error::from_engine)?;
            let mut out = Vec::with_capacity(lines.face_ids.len());
            for (row, &face_id) in lines.face_ids.iter().enumerate() {
                let line = slice(&lines.coordinates, &lines.offsets, row, mesh, "face", face_id)?;
                out.push((face_id, Geometry::LineString(line.iter().map(|p| Coord::xy(p[0], p[1])).collect()), Vec::new()));
            }
            Ok(out)
        })
    }

    /// Reads every stored face point (mesh corner) as the [`MESH_FACE_POINTS`] layer:
    /// `mesh_name`, `face_point_id` (the stored row of `FacePoints Coordinate`), `is_perimeter`
    /// (the stored `FacePoints Is Perimeter` integer, unconverted: HEC-RAS writes -1 on the
    /// perimeter and 0 inside) and `elevation` (the stored `FacePoints Elevation`, f32 widened
    /// exactly; null for a mesh that stores none).
    pub fn mesh_face_points(&self, options: &MeshOptions) -> crate::Result<MeshLayer> {
        use ras_commander_fast_hdf::hdf::corner_elevations::CornerElevationLayout;
        let meshes = self.mesh_list(options)?;
        if options.has_joins() {
            return Err(crate::Error::InvalidArgument("values join to cell and face layers, not face points".into()));
        }
        let mut layer = self.new_layer(MESH_FACE_POINTS, GeometryType::Point, Some("face_point_id"), options)?;
        layer.add_field(FieldDef::new("is_perimeter", FieldType::Integer));
        layer.add_field(FieldDef::new("elevation", FieldType::Float));
        let s = &self.session;
        for mesh in &meshes {
            let geometry = s.mesh_geometry(mesh, options.metadata_max_bytes).map_err(error::from_engine)?;
            let coordinates = s
                .read_mesh_geometry_cached(&geometry, MeshGeometryField::FacePoints, None, options.max_bytes)
                .map_err(error::from_engine)?;
            let xy = xy_rows(coordinates.shape.as_slice(), coordinates.values, mesh, "FacePoints Coordinate")?;
            let perimeter = s
                .read_mesh_geometry_cached(&geometry, MeshGeometryField::FacePointIsPerimeter, None, options.max_bytes)
                .map_err(error::from_engine)?;
            if perimeter.shape.as_slice() != [xy.len() as u64] {
                return Err(crate::Error::InvalidData(format!(
                    "{mesh} FacePoints Is Perimeter: shape {:?} for {} face points",
                    perimeter.shape,
                    xy.len()
                )));
            }
            let perimeter = super::numbers(perimeter.values, "FacePoints Is Perimeter")?;
            if super::kind_of(&perimeter) != FieldType::Integer {
                return Err(crate::Error::Unsupported(format!("{mesh} FacePoints Is Perimeter: not integers")));
            }
            let reported =
                s.mesh_corner_elevations(mesh, CornerElevationLayout::Geometry, None, options.max_bytes).map_err(error::from_engine)?;
            let elevation = if reported.is_unavailable() {
                None
            } else {
                let corners = reported.value().filter(|_| reported.is_supported()).ok_or_else(|| {
                    error::require(reported.capability(), &format!("{mesh} FacePoints Elevation"))
                        .err()
                        .unwrap_or_else(|| crate::Error::InvalidData(format!("{mesh} FacePoints Elevation: supported without a value")))
                })?;
                // The engine checks the length against `FacePoints Coordinate`.
                Some(corners.values.clone())
            };
            for (i, ([x, y], is_perimeter)) in xy.into_iter().zip(perimeter).enumerate() {
                let mut feature = Feature::new();
                feature.fid = layer.features.len() as u64;
                feature.geometry = Some(Geometry::Point(Coord::xy(x, y)));
                let z = elevation.as_ref().and_then(|e| e.get(i)).map_or(FieldValue::Null, |&v| FieldValue::Float(f64::from(v)));
                feature.attributes = vec![FieldValue::Text(mesh.clone()), FieldValue::Integer(i as i64), is_perimeter, z];
                layer.push(feature);
            }
        }
        Ok(MeshLayer { layer, omitted: Vec::new(), summaries: Vec::new(), joined: Vec::new(), time_step: None })
    }

    /// Reads each 2D flow area's stored perimeter as the [`MESH_PERIMETERS`] layer: one polygon
    /// per mesh with `mesh_name`, then every column of the stored 2D flow area table
    /// (`Geometry/2D Flow Areas/Attributes`: `name`, `mann`, `cell_count`, spacing and
    /// tolerances, ...), joined by the stored `Name`. Files without that table (HEC-RAS 5.0) give
    /// `mesh_name` only. An area without a stored `Perimeter` dataset is an error.
    pub fn mesh_perimeters(&self, options: &MeshOptions) -> crate::Result<MeshLayer> {
        let meshes = self.mesh_list(options)?;
        if options.has_joins() {
            return Err(crate::Error::InvalidArgument("values join to cell and face layers, not perimeters".into()));
        }
        let mut layer = self.new_layer(MESH_PERIMETERS, GeometryType::Polygon, None, options)?;
        let table = self.flow_area_table(options)?;
        let columns = table.as_ref().map(|t| t.columns.as_slice()).unwrap_or_default();
        super::families::declare_columns(&mut layer, columns)?;
        for mesh in &meshes {
            let p = self.session.read_mesh_perimeter(mesh, options.max_bytes).map_err(error::from_engine)?;
            let mut feature = Feature::new();
            feature.fid = layer.features.len() as u64;
            feature.geometry = Some(polygon(&p.ring, mesh, "perimeter")?);
            feature.attributes = vec![FieldValue::Text(mesh.clone())];
            if let Some(table) = &table {
                let row = flow_area_row(table, mesh)?;
                for column in columns {
                    feature.attributes.push(super::families::column_value(column, row, MESH_PERIMETERS)?);
                }
            }
            layer.push(feature);
        }
        Ok(MeshLayer { layer, omitted: Vec::new(), summaries: Vec::new(), joined: Vec::new(), time_step: None })
    }

    /// The stored `Geometry/2D Flow Areas/Attributes` table, or `None` when the file has none.
    fn flow_area_table(&self, options: &MeshOptions) -> crate::Result<Option<CompoundTable>> {
        const PATH: &str = "Geometry/2D Flow Areas/Attributes";
        match self.session.simple_compound(PATH, options.metadata_max_bytes) {
            Ok(table) => Ok(Some(table)),
            Err(e) if e.kind == ras_commander_fast_hdf::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(error::from_engine(e)),
        }
    }
}

/// Adds a column's fields to the layer (a name already in the schema is an error).
fn declare(layer: &mut Layer, column: &Column) -> crate::Result<JoinedField> {
    for (name, kind) in &column.fields {
        if name.is_empty() || layer.schema.fields().iter().any(|f| &f.name == name) {
            return Err(crate::Error::InvalidArgument(format!("{:?} gives the field {name:?} twice, or an empty one", column.source)));
        }
        layer.add_field(FieldDef::new(name.clone(), *kind));
    }
    Ok(JoinedField {
        source: column.source.clone(),
        fields: column.fields.iter().map(|f| f.0.clone()).collect(),
        units: column.units.clone(),
        datasets: Vec::new(),
    })
}

/// Checks that a later mesh's `column` fits the fields the first mesh declared (`report`): the
/// same field names, types and units, so every value of a field is of the field's type.
fn same_column(layer: &Layer, report: &JoinedField, column: &Column) -> crate::Result<()> {
    let declared = |name: &str| layer.schema.fields().iter().find(|f| f.name == name).map(|f| f.field_type);
    if report.fields.iter().ne(column.fields.iter().map(|f| &f.0))
        || column.fields.iter().any(|(name, kind)| declared(name) != Some(*kind))
        || report.units != column.units
    {
        return Err(crate::Error::Unsupported(format!(
            "{}: meshes store it with different rows, types or units; it cannot be one field",
            column.source
        )));
    }
    Ok(())
}

/// A stored static mesh array as a column.
fn attribute_column(
    hdf: &HecRasHdf,
    mesh: &str,
    attribute: MeshAttribute,
    element: Element,
    options: &MeshOptions,
) -> crate::Result<Column> {
    use MeshGeometryField as G;
    let (field, names): (G, &[&str]) = match (attribute, element) {
        (MeshAttribute::MinimumElevation, Element::Cell) => (G::CellMinimumElevation, &["minimum_elevation"]),
        (MeshAttribute::MinimumElevation, Element::Face) => (G::FaceMinimumElevation, &["minimum_elevation"]),
        (MeshAttribute::ManningsN, Element::Cell) => (G::CellManningsN, &["mannings_n"]),
        (MeshAttribute::FaceNormalAndLength, Element::Face) => (G::FaceNormalsAndLength, &["normal_x", "normal_y", "length"]),
        (MeshAttribute::FaceCells, Element::Face) => (G::FaceCellIndexes, &["cell_a", "cell_b"]),
        _ => {
            return Err(crate::Error::InvalidArgument(format!(
                "{attribute:?} is not stored on {}; join it to the matching layer",
                element.label()
            )));
        }
    };
    let s = &hdf.session;
    let geometry = s.mesh_geometry(mesh, options.metadata_max_bytes).map_err(error::from_engine)?;
    let dataset = geometry.fields.iter().find(|d| d.field == field).map_or_else(|| field.dataset_name().to_string(), |d| d.path.clone());
    let array = s.read_mesh_geometry_cached(&geometry, field, None, options.max_bytes).map_err(error::from_engine)?;
    let k = names.len();
    let elements = match array.shape.as_slice() {
        [n] if k == 1 => *n,
        [n, c] if *c == k as u64 => *n,
        shape => return Err(crate::Error::InvalidData(format!("{dataset}: shape {shape:?} for {k} fields"))),
    };
    let elements = usize::try_from(elements).map_err(|_| crate::Error::Unsupported(format!("{dataset}: too many elements")))?;
    let flat = super::numbers(array.values, &dataset)?;
    super::check_value_count(flat.len(), elements as u64, k as u64, &dataset)?;
    let kind = super::kind_of(&flat);
    let mut values = vec![Vec::with_capacity(elements); k];
    for (i, v) in flat.into_iter().enumerate() {
        values[i % k].push(v);
    }
    Ok(Column {
        source: field.dataset_name().to_string(),
        dataset,
        units: None,
        elements,
        fields: names.iter().map(|n| (n.to_string(), kind)).collect(),
        values,
    })
}

/// Rows of an `(n, 2)` float array (f32 widened exactly).
fn xy_rows(shape: &[u64], values: NumericValues, mesh: &str, what: &str) -> crate::Result<Vec<[f64; 2]>> {
    if shape.len() != 2 || shape[1] != 2 {
        return Err(crate::Error::InvalidData(format!("{mesh} {what}: shape {shape:?}, not (rows, 2)")));
    }
    let v: Vec<f64> = match values {
        NumericValues::F64(v) => v,
        NumericValues::F32(v) => v.into_iter().map(f64::from).collect(),
        _ => return Err(crate::Error::Unsupported(format!("{mesh} {what}: coordinates are not floats"))),
    };
    super::check_value_count(v.len(), shape[0], 2, &format!("{mesh} {what}"))?;
    Ok(v.as_chunks::<2>().0.to_vec())
}

/// Packed row `row` of `coordinates`/`offsets`.
fn slice<'a>(coordinates: &'a [[f64; 2]], offsets: &[u64], row: usize, mesh: &str, what: &str, id: i64) -> crate::Result<&'a [[f64; 2]]> {
    let bad = |m: &str| crate::Error::InvalidData(format!("HEC-RAS engine {what} {id} of {mesh}: {m}"));
    let (start, end) = match (offsets.get(row), offsets.get(row + 1)) {
        (Some(&s), Some(&e)) => (usize::try_from(s).map_err(|_| bad("offset"))?, usize::try_from(e).map_err(|_| bad("offset"))?),
        _ => return Err(bad("missing offset")),
    };
    coordinates.get(start..end).ok_or_else(|| bad("offset out of range"))
}

/// A closed engine ring (last vertex = first) as a terrio polygon, whose rings store no closing
/// vertex. `what` names the ring in errors (`cell 3`, `perimeter`).
fn polygon(ring: &[[f64; 2]], mesh: &str, what: &str) -> crate::Result<Geometry> {
    match ring {
        [first, .., last] if ring.len() >= 4 && first == last => {
            let coords = ring[..ring.len() - 1].iter().map(|p| Coord::xy(p[0], p[1])).collect();
            Ok(Geometry::Polygon { exterior: Ring::new(coords), interiors: Vec::new() })
        }
        _ => Err(crate::Error::InvalidData(format!("HEC-RAS engine {what} of {mesh}: not a closed ring of at least three vertices"))),
    }
}

/// The row of `mesh` in the stored 2D flow area table: the one row whose stored `Name` is
/// exactly the mesh name. No row, or several, is an error.
fn flow_area_row(table: &CompoundTable, mesh: &str) -> crate::Result<usize> {
    let names = match table.columns.iter().find(|c| c.name == "Name").map(|c| &c.value) {
        Some(MetadataValue::Strings(v)) => v,
        _ => return Err(crate::Error::Unsupported("Geometry/2D Flow Areas/Attributes: no text Name column".into())),
    };
    let mut rows = names.iter().enumerate().filter(|(_, n)| n.as_str() == mesh).map(|(i, _)| i);
    match (rows.next(), rows.next()) {
        (Some(row), None) => Ok(row),
        (None, _) => Err(crate::Error::InvalidData(format!("Geometry/2D Flow Areas/Attributes: no row named {mesh:?}"))),
        (Some(_), Some(_)) => Err(crate::Error::InvalidData(format!("Geometry/2D Flow Areas/Attributes: several rows named {mesh:?}"))),
    }
}

/// Floats only (`f32` widened exactly); integer result counters are left for a later, explicit
/// mapping.
pub(crate) fn floats(values: NumericValues, what: &str) -> crate::Result<Vec<f64>> {
    match values {
        NumericValues::F32(v) => Ok(v.into_iter().map(f64::from).collect()),
        NumericValues::F64(v) => Ok(v),
        _ => Err(crate::Error::Unsupported(format!("{what}: non-float result arrays are not mapped yet"))),
    }
}

mod results {
    use super::*;
    use crate::drivers::hecras::field_name;
    use ras_commander_fast_hdf::hdf::Slice;
    use ras_commander_fast_hdf::hdf::mesh_results::{ElementDomain, MeshResultVariable};

    fn domain_check(mesh: &str, variable: &str, domain: ElementDomain, element: Element) -> crate::Result<()> {
        let want = match element {
            Element::Cell => ElementDomain::Cell,
            Element::Face => ElementDomain::Face,
        };
        if domain != want {
            return Err(crate::Error::InvalidArgument(format!(
                "{mesh} {variable}: stored on {}, not {}; join it to the matching layer",
                domain.as_str(),
                element.label()
            )));
        }
        Ok(())
    }

    fn elements_of(n: u64, mesh: &str, variable: &str) -> crate::Result<usize> {
        usize::try_from(n).map_err(|_| crate::Error::Unsupported(format!("{mesh} {variable}: too many elements")))
    }

    fn units(hdf: &HecRasHdf, path: &str) -> crate::Result<Option<String>> {
        Ok(hdf.session.dataset_units(path).map_err(error::from_engine)?.units.value().cloned())
    }

    fn float_column(source: &str, dataset: String, units: Option<String>, fields: Vec<(String, Vec<f64>)>) -> Column {
        let elements = fields.first().map_or(0, |f| f.1.len());
        Column {
            source: source.to_string(),
            dataset,
            units,
            elements,
            fields: fields.iter().map(|f| (f.0.clone(), FieldType::Float)).collect(),
            values: fields.into_iter().map(|f| f.1.into_iter().map(FieldValue::Float).collect()).collect(),
        }
    }

    /// A `Summary Output` variable: its value row, and its time row when the engine proves one.
    pub(super) fn summary_column(
        hdf: &HecRasHdf,
        mesh: &str,
        variable: &str,
        element: Element,
        options: &MeshOptions,
    ) -> crate::Result<Column> {
        let s = &hdf.session;
        let inventory = s.mesh_summary_inventory(mesh, options.metadata_max_bytes).map_err(error::from_engine)?;
        error::require(&inventory.capability, &format!("{mesh} Summary Output"))?;
        let v = inventory
            .variables
            .iter()
            .find(|v| v.name == variable)
            .ok_or_else(|| crate::Error::InvalidArgument(format!("HEC-RAS not_found: {mesh} Summary Output has no {variable:?}")))?;
        error::require(&v.capability, &format!("{mesh} {variable}"))?;
        domain_check(mesh, variable, v.domain, element)?;
        let array =
            s.read_mesh_summary_values(mesh, variable, None, options.metadata_max_bytes, options.max_bytes).map_err(error::from_engine)?;
        let (rows, elements) = match array.shape.as_slice() {
            [n] => (1, *n),
            [r, n] => (*r, *n),
            _ => return Err(crate::Error::Unsupported(format!("{mesh} {variable}: shape {:?}", array.shape))),
        };
        let elements = elements_of(elements, mesh, variable)?;
        let values = floats(array.values, &format!("{mesh} {variable}"))?;
        super::super::check_value_count(values.len(), rows, elements as u64, &format!("{mesh} {variable}"))?;
        let field = field_name(variable);
        let fields = match (rows, v.time_of_extreme_row) {
            (1, _) => vec![(field, values)],
            (2, Some(1)) => {
                let (a, b) = values.split_at(elements);
                let time = format!("{field}_time");
                vec![(field, a.to_vec()), (time, b.to_vec())]
            }
            _ => {
                return Err(crate::Error::Unsupported(format!(
                    "{mesh} {variable}: {rows} stored rows; only a value row, or a value row and a proven Time row, map to fields"
                )));
            }
        };
        Ok(float_column(variable, v.path.clone(), units(hdf, &v.path)?, fields))
    }

    fn series_variable(
        hdf: &HecRasHdf,
        mesh: &str,
        variable: &str,
        element: Element,
        options: &MeshOptions,
    ) -> crate::Result<MeshResultVariable> {
        let inventory = hdf.session.mesh_series_inventory(mesh, options.metadata_max_bytes).map_err(error::from_engine)?;
        error::require(&inventory.capability, &format!("{mesh} Unsteady Time Series"))?;
        let v =
            inventory.variables.into_iter().find(|v| v.name == variable).ok_or_else(|| {
                crate::Error::InvalidArgument(format!("HEC-RAS not_found: {mesh} Unsteady Time Series has no {variable:?}"))
            })?;
        error::require(&v.capability, &format!("{mesh} {variable}"))?;
        domain_check(mesh, variable, v.domain, element)?;
        Ok(v)
    }

    /// A stored time-series variable at one stored output row.
    pub(super) fn series_column(
        hdf: &HecRasHdf,
        mesh: &str,
        variable: &str,
        row: u64,
        element: Element,
        options: &MeshOptions,
    ) -> crate::Result<Column> {
        let v = series_variable(hdf, mesh, variable, element, options)?;
        let rows = Slice::new(row, row.saturating_add(1), 1);
        let selection = hdf
            .session
            .read_mesh_series(mesh, variable, rows, None, options.metadata_max_bytes, options.max_bytes)
            .map_err(error::from_engine)?;
        let n = match selection.values.shape.as_slice() {
            [1, n] => *n,
            shape => return Err(crate::Error::InvalidData(format!("{mesh} {variable} row {row}: shape {shape:?}, not (1, elements)"))),
        };
        let values = floats(selection.values.values, &format!("{mesh} {variable}"))?;
        if values.len() as u64 != n {
            return Err(crate::Error::InvalidData(format!("{mesh} {variable} row {row}: {} values for {n} elements", values.len())));
        }
        Ok(float_column(variable, v.path.clone(), units(hdf, &v.path)?, vec![(field_name(variable), values)]))
    }

    /// The engine's reduction of a stored time series over every stored row, with the row it
    /// found the extreme at (first occurrence; a NaN propagates, as numpy's `argmax`).
    pub(super) fn extreme_column(
        hdf: &HecRasHdf,
        mesh: &str,
        variable: &str,
        extreme: Extreme,
        element: Element,
        options: &MeshOptions,
    ) -> crate::Result<Column> {
        let v = series_variable(hdf, mesh, variable, element, options)?;
        let rows = *v.shape.first().ok_or_else(|| crate::Error::InvalidData(format!("{mesh} {variable}: no rows")))?;
        let r = hdf
            .session
            .reduce_mesh_series(mesh, variable, Slice::new(0, rows, 1), None, options.metadata_max_bytes, options.max_bytes)
            .map_err(error::from_engine)?;
        let n = elements_of(r.elements.len() as u64, mesh, variable)?;
        let values = floats(r.values.values, &format!("{mesh} {variable}"))?;
        if values.len() != 2 * n || r.time_indices.len() != 2 * n {
            return Err(crate::Error::InvalidData(format!("{mesh} {variable}: reduction is not (2, {n})")));
        }
        let (range, suffix) = match extreme {
            Extreme::Minimum => (0..n, "min"),
            Extreme::Maximum => (n..2 * n, "max"),
        };
        let field = format!("{}_{suffix}", field_name(variable));
        let at = r.time_indices[range.clone()]
            .iter()
            .map(|&i| i64::try_from(i).map(FieldValue::Integer).map_err(|_| crate::Error::InvalidData("row above i64".into())))
            .collect::<crate::Result<Vec<_>>>()?;
        Ok(Column {
            source: variable.to_string(),
            dataset: v.path.clone(),
            units: units(hdf, &v.path)?,
            elements: n,
            fields: vec![(field.clone(), FieldType::Float), (format!("{field}_row"), FieldType::Integer)],
            values: vec![values[range].iter().map(|&x| FieldValue::Float(x)).collect(), at],
        })
    }

    /// The stored time of an output row.
    pub(super) fn time_step(hdf: &HecRasHdf, row: u64, options: &MeshOptions) -> crate::Result<TimeStep> {
        let t = hdf
            .session
            .unsteady_row_times(Slice::new(row, row.saturating_add(1), 1), options.metadata_max_bytes)
            .map_err(error::from_engine)?;
        Ok(TimeStep { row, days: t.days.and_then(|d| d.first().copied()), date_stamp: t.raw_strings.and_then(|d| d.into_iter().next()) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(kind: FieldType, units: Option<&str>) -> Column {
        Column {
            source: "Cells Minimum Elevation".into(),
            dataset: "Area/Cells Minimum Elevation".into(),
            units: units.map(String::from),
            elements: 0,
            fields: vec![("minimum_elevation".into(), kind)],
            values: vec![Vec::new()],
        }
    }

    #[test]
    fn a_join_stored_with_another_type_or_unit_in_a_later_mesh_is_refused() {
        let mut layer = Layer::new("mesh_cells");
        let report = declare(&mut layer, &column(FieldType::Float, Some("ft"))).unwrap();
        assert!(same_column(&layer, &report, &column(FieldType::Float, Some("ft"))).is_ok());
        for later in [column(FieldType::Integer, Some("ft")), column(FieldType::Float, Some("m"))] {
            let e = same_column(&layer, &report, &later).unwrap_err();
            assert!(matches!(e, crate::Error::Unsupported(_)), "{e}");
        }
    }
}

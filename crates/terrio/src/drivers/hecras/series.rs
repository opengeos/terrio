//! Per-feature results at one stored unsteady output row, joined to the `structures`,
//! `bc_lines`, `reference_lines` and `reference_points` layers by the identities the engine
//! links:
//!
//! - structures: the engine's structure inventories (SA/2D area connections and lateral
//!   structures; 1D inline structures, bridges and culverts) give each stored structure group
//!   its `Geometry/Structures/Attributes` row;
//! - boundary-condition lines: the engine links each 2D area's boundary series to its line by
//!   the exact stored `Name` and `SA-2D`;
//! - reference lines and points: the engine links each result column to its geometry
//!   `Attributes` row (`FamilyDataset::geometry_locations`); a variable whose columns the
//!   engine does not all link is refused.
//!
//! Each stored column becomes one field. A feature the engine links no series to gets nulls (it
//! stores none); an engine refusal is an error.

use super::families::Family;
use super::{HecRasHdf, error, field_name};
use crate::{FieldDef, FieldValue, Layer};
use ras_commander_fast_hdf::hdf::families::ReadFamily;
use ras_commander_fast_hdf::hdf::lines::GeometryFamily;
use ras_commander_fast_hdf::hdf::metadata::{MetadataValue, TypedAttribute};
use ras_commander_fast_hdf::hdf::stored::StoredValues;
use ras_commander_fast_hdf::hdf::structures::{StructureFamilyKind, StructureSeries};
use ras_commander_fast_hdf::hdf::structures_1d::OneDStructureKind;
use ras_commander_fast_hdf::hdf::xs_geometry::OneDLineFamily;
use ras_commander_fast_hdf::hdf::{DEFAULT_MAX_BYTES, DEFAULT_READ_MAX_BYTES, Identity, NumericArray, ResultMode, Slice};
use ras_commander_fast_hdf::{Capability, ErrorKind};
use std::collections::HashMap;

const STRUCTURE_ATTRIBUTES: &str = "Geometry/Structures/Attributes";
const REFERENCE_SERIES: &str = "Results/Unsteady/Output/Output Blocks/Base Output/Unsteady Time Series";

/// What to join to the `structures` or `bc_lines` layer.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct FeatureSeriesOptions {
    /// For `structures`: stored series names relative to each structure group (exact, such as
    /// `Structure Variables`). For `reference_lines` and `reference_points`: stored variable
    /// names (`Water Surface`, `Flow`, `Velocity`, ...). Ignored by `bc_lines`, whose series are
    /// named after each line.
    pub variables: Vec<String>,
    /// The stored unsteady output row (zero-based).
    pub row: u64,
    /// Byte limit for each metadata object the engine consults.
    pub metadata_max_bytes: usize,
    /// Byte limit for each array read.
    pub max_bytes: usize,
}

impl FeatureSeriesOptions {
    /// Reads output row `row`.
    pub fn at_row(row: u64) -> Self {
        Self { variables: Vec::new(), row, metadata_max_bytes: DEFAULT_MAX_BYTES, max_bytes: DEFAULT_READ_MAX_BYTES }
    }

    /// Joins a stored structure series.
    pub fn with_variable(mut self, variable: impl Into<String>) -> Self {
        self.variables.push(variable.into());
        self
    }
}

/// One stored series joined to a layer.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SeriesField {
    /// The stored series name (`Structure Variables`, or the boundary-condition line's name).
    pub variable: String,
    /// The fields it fills, in schema order: one per stored column label.
    pub fields: Vec<String>,
    /// The stored column labels, exactly as stored.
    pub labels: Vec<String>,
    /// The stored unit of each field, exactly as stored; `None` when not stored.
    pub units: Vec<Option<String>>,
    /// The datasets read, one per linked feature.
    pub datasets: Vec<String>,
}

/// A family layer with per-feature values at one output row.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SeriesLayer {
    /// The layer: the family layer, then the joined fields.
    pub layer: Layer,
    /// The joined series.
    pub fields: Vec<SeriesField>,
    /// The stored `Time Date Stamp` of the row, exactly as stored.
    pub row_label: Option<String>,
}

/// A stored column label with its stored unit.
struct Label {
    name: String,
    unit: Option<String>,
}

/// One feature's values of one series.
struct Values {
    id: Identity,
    dataset: String,
    labels: Vec<Label>,
    values: Vec<FieldValue>,
}

impl HecRasHdf {
    /// Reads the `structures` layer and joins each requested structure series at one output row.
    /// Each stored column becomes `<series>_<column label>`.
    pub fn structures_at(&self, options: &FeatureSeriesOptions) -> crate::Result<SeriesLayer> {
        if options.variables.is_empty() {
            return Err(crate::Error::InvalidArgument("no structure series to join".into()));
        }
        let mut layer = self.family_layer(
            "structures",
            Family::OneD(OneDLineFamily::StructureCenterlines, super::families::Shape::Line),
            options.metadata_max_bytes,
            options.max_bytes,
        )?;
        let row_label = self.row_label(options)?;
        let groups = self.structure_groups(options)?;
        let mut fields = Vec::new();
        for variable in &options.variables {
            let mut found = Vec::new();
            for g in &groups {
                let Some(series) = g.series.iter().find(|s| &s.relative == variable) else { continue };
                // A series this layer's rows cannot carry is an error, never left out.
                if g.id.table != STRUCTURE_ATTRIBUTES {
                    return Err(crate::Error::Unsupported(format!(
                        "structure {:?} stores {variable:?} but is identified by {}, not {STRUCTURE_ATTRIBUTES}",
                        g.name, g.id.table
                    )));
                }
                let array = g.read(self, series, options)?;
                found.push(values(g.id.clone(), series, array)?);
            }
            if found.is_empty() {
                return Err(crate::Error::InvalidArgument(format!("HEC-RAS not_found: no structure stores {variable:?}")));
            }
            fields.push(join(&mut layer, variable, &format!("{}_", field_name(variable)), found)?);
        }
        Ok(SeriesLayer { layer, fields, row_label })
    }

    /// Reads the `bc_lines` layer and joins each line's stored boundary series at one output
    /// row: one field per stored column (`stage`, `flow`, ...), linked by the engine through the
    /// exact `Name` and `SA-2D`. The per-face series are not line values and are not joined. A
    /// 2D flow area that stores no boundary series is passed over; its lines get nulls.
    pub fn bc_lines_at(&self, options: &FeatureSeriesOptions) -> crate::Result<SeriesLayer> {
        let mut layer = self.family_layer(
            "bc_lines",
            Family::TwoD(GeometryFamily::BoundaryConditions, super::families::Shape::Line),
            options.metadata_max_bytes,
            options.max_bytes,
        )?;
        let row_label = self.row_label(options)?;
        let s = &self.session;
        let mut found = Vec::new();
        for area in self.mesh_names()? {
            let links = s.mesh_boundary_links(&area, options.metadata_max_bytes).map_err(error::from_engine)?;
            let links = error::take(links, &format!("{area} boundary-condition links"))?;
            error::require(&links.geometry_capability, &format!("{area} boundary-condition lines"))?;
            // An area that stores no boundary series (no group of them: canonically absent) has
            // none to join; another area's still join. Any other refusal fails the layer.
            if matches!(links.series_capability, Capability::Unavailable { .. }) && links.matched.is_empty() {
                continue;
            }
            error::require(&links.series_capability, &format!("{area} boundary-condition series"))?;
            let inventory = s.mesh_boundary_inventory(&area, options.metadata_max_bytes).map_err(error::from_engine)?;
            for m in &links.matched {
                let v =
                    inventory.variables.iter().find(|v| v.name == m.name).ok_or_else(|| {
                        crate::Error::InvalidData(format!("{area} boundary series {:?} linked but not inventoried", m.name))
                    })?;
                error::require(&v.capability, &m.series_path)?;
                let labels = column_labels(&v.attributes, &m.series_path)?;
                let sel = s
                    .read_mesh_boundary_series(&area, &m.name, row_slice(options.row), None, options.metadata_max_bytes, options.max_bytes)
                    .map_err(error::from_engine)?;
                let values = row_values(sel.values, labels.len(), &m.series_path)?;
                found.push(Values { id: m.geometry.clone(), dataset: m.series_path.clone(), labels, values });
            }
        }
        if found.is_empty() {
            return Err(crate::Error::InvalidArgument("HEC-RAS not_found: no boundary-condition line has a stored series".into()));
        }
        let fields = vec![join(&mut layer, "Boundary Conditions", "", found)?];
        Ok(SeriesLayer { layer, fields, row_label })
    }

    /// Reads the `reference_lines` layer and joins stored reference-line variables at one output
    /// row (`water_surface`, `flow`, ...), by the geometry row the engine links to each column.
    pub fn reference_lines_at(&self, options: &FeatureSeriesOptions) -> crate::Result<SeriesLayer> {
        let layer = self.family_layer(
            "reference_lines",
            Family::TwoD(GeometryFamily::ReferenceLines, super::families::Shape::Line),
            options.metadata_max_bytes,
            options.max_bytes,
        )?;
        self.reference_join(layer, ReadFamily::ReferenceLines, "Reference Lines", "Geometry/Reference Lines/Attributes", options)
    }

    /// Reads the `reference_points` layer and joins stored reference-point variables at one
    /// output row, by the geometry row the engine links to each column.
    pub fn reference_points_at(&self, options: &FeatureSeriesOptions) -> crate::Result<SeriesLayer> {
        let layer = self.family_layer("reference_points", Family::ReferencePoints, options.metadata_max_bytes, options.max_bytes)?;
        self.reference_join(layer, ReadFamily::ReferencePoints, "Reference Points", "Geometry/Reference Points/Attributes", options)
    }

    fn reference_join(
        &self,
        mut layer: Layer,
        family: ReadFamily,
        group: &str,
        table: &str,
        options: &FeatureSeriesOptions,
    ) -> crate::Result<SeriesLayer> {
        if options.variables.is_empty() {
            return Err(crate::Error::InvalidArgument(format!("no {group} variables to join")));
        }
        let s = &self.session;
        let row_label = self.row_label(options)?;
        let inventory = s.read_family_inventory(family, options.metadata_max_bytes).map_err(error::from_engine)?;
        error::require(&inventory.capability, &format!("{group} results"))?;
        let mut fields = Vec::new();
        for variable in &options.variables {
            let path = format!("{REFERENCE_SERIES}/{group}/{variable}");
            let d = inventory
                .datasets
                .iter()
                .find(|d| d.stored.path == path)
                .ok_or_else(|| crate::Error::InvalidArgument(format!("HEC-RAS not_found: {path}")))?;
            error::require(&d.stored.capability, &path)?;
            if !matches!(d.role.as_str(), "time_series" | "direction_series") {
                return Err(crate::Error::Unsupported(format!("{path}: stored as {}, not one value per time row", d.role)));
            }
            let columns = match d.stored.shape.as_slice() {
                [_, n] => *n,
                shape => return Err(crate::Error::Unsupported(format!("{path}: shape {shape:?}"))),
            };
            if d.geometry_locations.len() as u64 != columns || d.geometry_locations.iter().any(|id| id.table != table) {
                return Err(crate::Error::Unsupported(format!(
                    "HEC-RAS unsupported(identity_layout): {path}: the engine links {} of {columns} columns to {table} rows",
                    d.geometry_locations.iter().filter(|id| id.table == table).count()
                )));
            }
            let slices = [row_slice(options.row), Slice::new(0, columns, 1)];
            let array = match s
                .read_family_dataset(family, &path, Some(&slices), options.metadata_max_bytes, options.max_bytes)
                .map_err(error::from_engine)?
            {
                StoredValues::Numeric(a) => a,
                _ => return Err(crate::Error::Unsupported(format!("{path}: not numeric"))),
            };
            let values = row_values(array, columns as usize, &path)?;
            let unit = d.stored.attributes.iter().find(|a| a.name == "Units").and_then(|a| match &a.value {
                MetadataValue::Strings(v) if v.len() == 1 => Some(v[0].clone()),
                _ => None,
            });
            let found = d
                .geometry_locations
                .iter()
                .zip(values)
                .map(|(id, value)| Values {
                    id: id.clone(),
                    dataset: path.clone(),
                    labels: vec![Label { name: variable.clone(), unit: unit.clone() }],
                    values: vec![value],
                })
                .collect();
            fields.push(join(&mut layer, variable, "", found)?);
        }
        Ok(SeriesLayer { layer, fields, row_label })
    }

    fn row_label(&self, options: &FeatureSeriesOptions) -> crate::Result<Option<String>> {
        let t = self.session.unsteady_row_times(row_slice(options.row), options.metadata_max_bytes).map_err(error::from_engine)?;
        Ok(t.raw_strings.and_then(|d| d.into_iter().next()))
    }

    /// Every stored unsteady structure group the engine inventories, from every qualified
    /// family. An absent family is skipped; a refused one is an error.
    fn structure_groups(&self, options: &FeatureSeriesOptions) -> crate::Result<Vec<Group>> {
        let s = &self.session;
        let mut out = Vec::new();
        for kind in [StructureFamilyKind::Sa2dAreaConnection, StructureFamilyKind::LateralStructure] {
            match s.structure_inventory(kind, options.metadata_max_bytes) {
                Ok(inv) => out.extend(inv.structures.into_iter().map(|d| Group {
                    source: Source::TwoD(kind),
                    name: d.name,
                    id: d.identity.id,
                    series: d.series,
                })),
                Err(e) if e.kind == ErrorKind::NotFound => {}
                Err(e) => return Err(error::from_engine(e)),
            }
        }
        let one_d = [OneDStructureKind::InlineStructure, OneDStructureKind::Bridge, OneDStructureKind::Culvert];
        for c in s.one_d_structure_capabilities(options.metadata_max_bytes).map_err(error::from_engine)? {
            if c.mode != ResultMode::Unsteady || !one_d.contains(&c.kind) || matches!(c.status, Capability::Unavailable { .. }) {
                continue;
            }
            error::require(&c.status, &c.group)?;
            let inv = s.one_d_structure_inventory(c.kind, ResultMode::Unsteady, options.metadata_max_bytes).map_err(error::from_engine)?;
            let kind = c.kind;
            out.extend(inv.structures.into_iter().map(|d| Group {
                source: Source::OneD(kind),
                name: d.name,
                id: d.identity.id,
                series: d.series,
            }));
        }
        Ok(out)
    }
}

#[derive(Clone, Copy)]
enum Source {
    TwoD(StructureFamilyKind),
    OneD(OneDStructureKind),
}

/// One stored structure group with its geometry identity.
struct Group {
    source: Source,
    name: String,
    id: Identity,
    series: Vec<StructureSeries>,
}

impl Group {
    fn read(&self, hdf: &HecRasHdf, series: &StructureSeries, options: &FeatureSeriesOptions) -> crate::Result<NumericArray> {
        error::require(&series.capability, &series.path)?;
        let columns = match series.shape.as_slice() {
            [_, c] => *c,
            shape => return Err(crate::Error::Unsupported(format!("{}: shape {shape:?}, not (rows, columns)", series.path))),
        };
        let s = &hdf.session;
        match self.source {
            Source::TwoD(kind) => s.read_structure_series(
                kind,
                Some(&self.name),
                &series.relative,
                row_slice(options.row),
                Slice::new(0, columns, 1),
                options.max_bytes,
            ),
            Source::OneD(kind) => s.read_one_d_structure_series(
                kind,
                ResultMode::Unsteady,
                Some(&self.name),
                &series.relative,
                row_slice(options.row),
                None,
                options.metadata_max_bytes,
                options.max_bytes,
            ),
        }
        .map_err(error::from_engine)
    }
}

fn row_slice(row: u64) -> Slice {
    Slice::new(row, row.saturating_add(1), 1)
}

/// One structure's series values with the engine's column labels.
fn values(id: Identity, series: &StructureSeries, array: NumericArray) -> crate::Result<Values> {
    let labels: Vec<Label> = error::take(series.columns.clone(), &format!("{} column labels", series.path))?
        .labels
        .into_iter()
        .map(|l| Label { name: l.name, unit: l.unit })
        .collect();
    let values = row_values(array, labels.len(), &series.path)?;
    Ok(Values { id, dataset: series.path.clone(), labels, values })
}

/// The stored `Columns` names of a boundary series, each with the unit stored in the attribute
/// of the same name.
fn column_labels(attributes: &[TypedAttribute], what: &str) -> crate::Result<Vec<Label>> {
    let text = |name: &str| {
        attributes.iter().find(|a| a.name == name).and_then(|a| match &a.value {
            MetadataValue::Strings(v) => Some(v.clone()),
            _ => None,
        })
    };
    let names = text("Columns").ok_or_else(|| crate::Error::Unsupported(format!("{what}: no stored Columns labels")))?;
    Ok(names
        .into_iter()
        .map(|name| {
            let unit = text(&name).filter(|u| u.len() == 1).map(|mut u| u.remove(0));
            Label { name, unit }
        })
        .collect())
}

/// The single row of a `(1, columns)` read.
fn row_values(array: NumericArray, columns: usize, what: &str) -> crate::Result<Vec<FieldValue>> {
    if array.shape.as_slice() != [1, columns as u64] {
        return Err(crate::Error::InvalidData(format!("{what}: read shape {:?} for {columns} labelled columns", array.shape)));
    }
    super::numbers(array.values, what)
}

/// Adds one field per distinct column label (in first-seen order, `prefix` + label) and fills
/// each linked feature; unlinked features get nulls.
fn join(layer: &mut Layer, variable: &str, prefix: &str, found: Vec<Values>) -> crate::Result<SeriesField> {
    let mut index = HashMap::with_capacity(layer.len());
    for (i, f) in layer.features.iter().enumerate() {
        if let Some(FieldValue::Integer(r)) = f.attributes.first() {
            index.insert(*r, i);
        }
    }
    let mut report =
        SeriesField { variable: variable.to_string(), fields: Vec::new(), labels: Vec::new(), units: Vec::new(), datasets: Vec::new() };
    let mut slot: HashMap<String, usize> = HashMap::new();
    let mut table: Vec<Vec<FieldValue>> = Vec::new();
    let mut seen = vec![false; layer.len()];
    for v in found {
        let row = i64::try_from(v.id.row).map_err(|_| crate::Error::InvalidData("row overflow".into()))?;
        let feature = match index.get(&row) {
            Some(&f) if !seen[f] => f,
            _ => {
                return Err(crate::Error::InvalidData(format!("{}: {} row {} matches no single feature", v.dataset, v.id.table, v.id.row)));
            }
        };
        seen[feature] = true;
        for (label, value) in v.labels.iter().zip(v.values) {
            let name = format!("{prefix}{}", field_name(&label.name));
            let k = match slot.get(&name) {
                Some(&k) => {
                    if report.units[k] != label.unit {
                        return Err(crate::Error::Unsupported(format!(
                            "{variable}: column {:?} is stored with different units",
                            label.name
                        )));
                    }
                    if report.labels[k] != label.name {
                        return Err(crate::Error::Unsupported(format!(
                            "{variable}: columns {:?} and {:?} give the same field",
                            report.labels[k], label.name
                        )));
                    }
                    k
                }
                None => {
                    if name.is_empty() || name == prefix || layer.schema.fields().iter().any(|f| f.name == name) {
                        return Err(crate::Error::Unsupported(format!(
                            "{variable}: column {:?} gives an empty or repeated field",
                            label.name
                        )));
                    }
                    slot.insert(name.clone(), report.fields.len());
                    report.fields.push(name);
                    report.labels.push(label.name.clone());
                    report.units.push(label.unit.clone());
                    table.push(vec![FieldValue::Null; layer.len()]);
                    report.fields.len() - 1
                }
            };
            if table[k][feature] != FieldValue::Null {
                return Err(crate::Error::Unsupported(format!("{}: column {:?} stored twice", v.dataset, label.name)));
            }
            table[k][feature] = value;
        }
        report.datasets.push(v.dataset);
    }
    for (name, column) in report.fields.iter().zip(table) {
        let kind = super::kind_of(&column.iter().filter(|v| **v != FieldValue::Null).cloned().collect::<Vec<_>>());
        layer.add_field(FieldDef::new(name.clone(), kind));
        for (feature, v) in layer.features.iter_mut().zip(column) {
            feature.attributes.push(v);
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Feature;

    /// A layer of `n` features whose first attribute is their source row, as family layers are.
    fn layer(n: i64) -> Layer {
        let mut layer = Layer::new("structures");
        layer.add_field(FieldDef::new("source_row", crate::FieldType::Integer));
        for row in 0..n {
            let mut f = Feature::new();
            f.fid = row as u64;
            f.attributes = vec![FieldValue::Integer(row)];
            layer.push(f);
        }
        layer
    }

    fn values(row: u64, label: &str, unit: &str, value: f64) -> Values {
        Values {
            id: Identity::new("plan.p01.hdf", STRUCTURE_ATTRIBUTES, row),
            dataset: format!("series {row}"),
            labels: vec![Label { name: label.into(), unit: Some(unit.into()) }],
            values: vec![FieldValue::Float(value)],
        }
    }

    #[test]
    fn labels_that_give_one_field_must_be_the_same_label() {
        // The same stored label on two features shares its field.
        let mut l = layer(2);
        let field = join(&mut l, "Structure Variables", "sv_", vec![values(0, "Flow", "cfs", 1.0), values(1, "Flow", "cfs", 2.0)]).unwrap();
        assert_eq!(field.fields, ["sv_flow"]);
        assert_eq!(field.labels, ["Flow"]);
        // Distinct stored labels that normalize to one field are refused, as different units are.
        let mut l = layer(2);
        let e = join(&mut l, "Structure Variables", "sv_", vec![values(0, "Flow-Rate", "cfs", 1.0), values(1, "Flow Rate", "cfs", 2.0)])
            .unwrap_err();
        assert!(matches!(&e, crate::Error::Unsupported(m) if m.contains("same field")), "{e}");
        let mut l = layer(2);
        let e =
            join(&mut l, "Structure Variables", "sv_", vec![values(0, "Flow", "cfs", 1.0), values(1, "Flow", "m3/s", 2.0)]).unwrap_err();
        assert!(matches!(&e, crate::Error::Unsupported(m) if m.contains("different units")), "{e}");
    }
}

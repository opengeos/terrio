//! 1D values joined to the `cross_sections` layer: stored `Summary Output/Cross Sections`
//! variables, stored steady or unsteady cross-section results at one profile or time row, and
//! the stored per-section geometry tables (station-elevation, Manning's n, ineffective areas) as
//! JSON fields.
//!
//! The engine validates which result column belongs to which cross section (River, Reach and
//! RS against `Geometry/Cross Sections/Attributes`) and reports each column's geometry
//! identity. This module joins by that identity (the attribute-table row, which is the layer's
//! `source_row`); a section without a value, or a value without a section, is an error.

use super::families::{Family, Shape};
use super::{HecRasHdf, error, field_name, kind_of, numbers};
use crate::{FieldDef, FieldType, FieldValue, Layer};
use ras_commander_fast_hdf::hdf::one_d_summary::OneDElementDomain;
use ras_commander_fast_hdf::hdf::xs_geometry::{OneDLineFamily, XsGeometryField, XsGeometryValues};
use ras_commander_fast_hdf::hdf::xs_results::XsResultDomain;
use ras_commander_fast_hdf::hdf::{DEFAULT_MAX_BYTES, DEFAULT_READ_MAX_BYTES, Identity, ResultMode, Slice};
use std::collections::HashMap;

/// The attribute table cross-section identities refer to.
const XS_ATTRIBUTES: &str = "Geometry/Cross Sections/Attributes";

/// What to join to the `cross_sections` layer.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CrossSectionOptions {
    /// Stored `Summary Output/Cross Sections` variables (exact dataset names, such as
    /// `Maximum Water Surface`, `Maximum Flow` or `Maximum Channel Velocity`).
    pub summaries: Vec<String>,
    /// Stored cross-section result variables (exact names below the `Cross Sections` result
    /// group, such as `Water Surface`, `Flow` or `Velocity Channel`) read at
    /// [`CrossSectionOptions::row`].
    pub results: Vec<String>,
    /// Which results [`CrossSectionOptions::results`] are: unsteady time rows (the default) or
    /// steady profiles.
    pub mode: ResultMode,
    /// The stored row of [`CrossSectionOptions::results`]: an unsteady output row or a steady
    /// profile index (zero-based). Required with `results`, and only allowed with them.
    pub row: Option<u64>,
    /// Stored per-section geometry tables to join as JSON fields ([`CrossSectionTable`]).
    pub geometry: Vec<CrossSectionTable>,
    /// Byte limit for each metadata object the engine consults.
    pub metadata_max_bytes: usize,
    /// Byte limit for each array read.
    pub max_bytes: usize,
}

impl Default for CrossSectionOptions {
    fn default() -> Self {
        Self {
            summaries: Vec::new(),
            results: Vec::new(),
            mode: ResultMode::Unsteady,
            row: None,
            geometry: Vec::new(),
            metadata_max_bytes: DEFAULT_MAX_BYTES,
            max_bytes: DEFAULT_READ_MAX_BYTES,
        }
    }
}

impl CrossSectionOptions {
    /// Joins a stored summary variable.
    pub fn with_summary(mut self, variable: impl Into<String>) -> Self {
        self.summaries.push(variable.into());
        self
    }

    /// Joins a stored unsteady result variable at output row `row`. Every result variable is
    /// read in one mode at one row: this sets them for those already added too.
    pub fn with_unsteady(mut self, variable: impl Into<String>, row: u64) -> Self {
        self.results.push(variable.into());
        self.mode = ResultMode::Unsteady;
        self.row = Some(row);
        self
    }

    /// Joins a stored per-section geometry table as a JSON field.
    pub fn with_table(mut self, table: CrossSectionTable) -> Self {
        self.geometry.push(table);
        self
    }

    /// Joins a stored steady result variable at profile index `profile`. Every result variable
    /// is read in one mode at one row: this sets them for those already added too.
    pub fn with_steady(mut self, variable: impl Into<String>, profile: u64) -> Self {
        self.results.push(variable.into());
        self.mode = ResultMode::Steady;
        self.row = Some(profile);
        self
    }
}

/// A stored per-section table of `Geometry/Cross Sections`, joined to `cross_sections` as one
/// JSON field per section (stored rows in stored order; `[]` for a section without rows).
/// Numbers are the stored values (`f32` widened exactly) in their shortest exact decimal form;
/// a non-finite number, which JSON cannot hold, is `null`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CrossSectionTable {
    /// `station_elevation`: `Station Elevation Info`/`Values`, `[[station, elevation], ...]`.
    StationElevation,
    /// `mannings_n`: `Manning's n Info`/`Values`, `[[station, n], ...]`.
    ManningsN,
    /// `ineffective_areas`: `Ineffective Info`/`Ineffective Blocks`, one object per stored block
    /// with its stored columns (`{"Left Sta": .., "Right Sta": .., "Elevation": ..,
    /// "Permanent": ..}`).
    IneffectiveAreas,
}

impl CrossSectionTable {
    fn engine(self) -> XsGeometryField {
        match self {
            Self::StationElevation => XsGeometryField::StationElevation,
            Self::ManningsN => XsGeometryField::ManningsN,
            Self::IneffectiveAreas => XsGeometryField::IneffectiveAreas,
        }
    }

    /// The field name.
    pub fn field(self) -> &'static str {
        match self {
            Self::StationElevation => "station_elevation",
            Self::ManningsN => "mannings_n",
            Self::IneffectiveAreas => "ineffective_areas",
        }
    }
}

/// One stored variable joined to the cross sections.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct CrossSectionField {
    /// The stored variable name.
    pub variable: String,
    /// The dataset read.
    pub dataset: String,
    /// The fields it fills, in schema order: one per stored row of a summary variable (the
    /// value row as `<variable>`, a proven time row as `<variable>_time`, any other labelled row
    /// as `<variable>_<label>`), or one for a result row.
    pub fields: Vec<String>,
    /// The stored label of each field's row, exactly as stored (`Max WSEL`, `Flow at Max WSEL`).
    pub labels: Vec<Option<String>>,
    /// The stored unit of each field, exactly as stored; `None` when not proven. Never inferred.
    pub units: Vec<Option<String>>,
}

/// The `cross_sections` layer with joined results.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CrossSectionLayer {
    /// The layer: the `cross_sections` family layer, then the joined fields.
    pub layer: Layer,
    /// The joined variables, summaries first.
    pub fields: Vec<CrossSectionField>,
    /// The steady profile name or unsteady `Time Date Stamp` of the result row, exactly as stored.
    pub row_label: Option<String>,
}

/// Values of one field, one per stored element.
struct Column {
    field: String,
    label: Option<String>,
    unit: Option<String>,
    kind: FieldType,
    values: Vec<FieldValue>,
}

impl HecRasHdf {
    /// Reads the `cross_sections` layer (cut lines with their stored attribute columns) and
    /// joins the requested stored results by the engine's validated cross-section identity.
    pub fn cross_sections(&self, options: &CrossSectionOptions) -> crate::Result<CrossSectionLayer> {
        match (options.results.is_empty(), options.row) {
            (false, None) => return Err(crate::Error::InvalidArgument("cross-section results need a row (profile or time row)".into())),
            (true, Some(_)) => return Err(crate::Error::InvalidArgument("a result row needs at least one result variable".into())),
            _ => {}
        }
        let mut layer = self.family_layer(
            "cross_sections",
            Family::OneD(OneDLineFamily::CrossSectionCutLines, Shape::Line),
            options.metadata_max_bytes,
            options.max_bytes,
        )?;
        let row_index = source_rows(&layer)?;
        let mut fields = Vec::new();
        let mut row_label = None;
        for variable in &options.summaries {
            let (ids, columns, dataset) = self.xs_summary(variable, options)?;
            fields.push(join(&mut layer, &row_index, variable, &dataset, &ids, columns)?);
        }
        if let Some(row) = options.row {
            row_label = self.xs_row_label(options.mode, row, options)?;
            for variable in &options.results {
                let (ids, columns, dataset) = self.xs_result(variable, row, options)?;
                fields.push(join(&mut layer, &row_index, variable, &dataset, &ids, columns)?);
            }
        }
        for &table in &options.geometry {
            let (ids, columns, dataset) = self.xs_table(table, options)?;
            fields.push(join(&mut layer, &row_index, table.field(), &dataset, &ids, columns)?);
        }
        Ok(CrossSectionLayer { layer, fields, row_label })
    }

    /// One stored per-section table as one JSON value per record (a cross-section row).
    fn xs_table(&self, table: CrossSectionTable, options: &CrossSectionOptions) -> crate::Result<(Vec<Identity>, Vec<Column>, String)> {
        use serde_json::Value;
        let selection = self
            .session
            .read_xs_geometry(table.engine(), None, options.metadata_max_bytes, options.max_bytes)
            .map_err(error::from_engine)?;
        let dataset = selection.values_path.clone();
        let bad = |m: String| crate::Error::InvalidData(format!("{dataset}: {m}"));
        let number = |v: &FieldValue| match v {
            FieldValue::Float(x) => serde_json::Number::from_f64(*x).map_or(Value::Null, Value::Number),
            FieldValue::Integer(i) => Value::from(*i),
            FieldValue::Text(t) => Value::from(t.as_str()),
            FieldValue::Blob(b) => Value::from(b.clone()),
            _ => Value::Null,
        };
        // Per stored Values row: a JSON array of numbers, or an object of stored columns.
        let rows: Vec<Value> = match selection.values {
            XsGeometryValues::Numeric(array) => {
                let width = match array.shape.as_slice() {
                    [_, c] if *c > 0 => *c as usize,
                    [0] => 1,
                    shape => return Err(bad(format!("shape {shape:?}, not (rows, columns)"))),
                };
                numbers(array.values, &dataset)?.chunks(width).map(|r| Value::Array(r.iter().map(number).collect())).collect()
            }
            XsGeometryValues::Compound(compound) => {
                let t = &compound.table;
                let n = t.shape.first().copied().unwrap_or(0) as usize;
                let mut out = Vec::with_capacity(n);
                for row in 0..n {
                    let mut object = serde_json::Map::new();
                    for column in &t.columns {
                        object.insert(column.name.clone(), number(&super::families::column_value(column, row, &dataset)?));
                    }
                    out.push(Value::Object(object));
                }
                out
            }
            _ => return Err(crate::Error::Unsupported(format!("{dataset}: unmapped value layout"))),
        };
        let mut ids = Vec::with_capacity(selection.records.len());
        let mut values = Vec::with_capacity(selection.records.len());
        for record in &selection.records {
            let count = record.stored_rows.end.checked_sub(record.stored_rows.start).ok_or_else(|| bad("reversed record rows".into()))?;
            let start = usize::try_from(record.offset).map_err(|_| bad("record offset".into()))?;
            let end = start.checked_add(count as usize).filter(|&e| e <= rows.len()).ok_or_else(|| bad("record outside Values".into()))?;
            ids.push(record.id.clone());
            values.push(FieldValue::Text(Value::Array(rows[start..end].to_vec()).to_string()));
        }
        let column = Column { field: table.field().to_string(), label: None, unit: None, kind: FieldType::Json, values };
        Ok((ids, vec![column], dataset))
    }

    fn xs_summary(&self, variable: &str, options: &CrossSectionOptions) -> crate::Result<(Vec<Identity>, Vec<Column>, String)> {
        let s = &self.session;
        let inventory = s.one_d_summary_inventory(options.metadata_max_bytes).map_err(error::from_engine)?;
        error::require(&inventory.capability, "Summary Output")?;
        let v = inventory.variables.iter().find(|v| v.family == "Cross Sections" && v.name == variable).ok_or_else(|| {
            crate::Error::InvalidArgument(format!("HEC-RAS not_found: Summary Output/Cross Sections has no {variable:?}"))
        })?;
        error::require(&v.capability, &format!("Summary Output/Cross Sections/{variable}"))?;
        if v.domain != OneDElementDomain::CrossSection {
            return Err(crate::Error::Unsupported(format!("Summary Output/Cross Sections/{variable}: not one column per cross section")));
        }
        let selection = s
            .read_one_d_summary("Cross Sections", variable, None, options.metadata_max_bytes, options.max_bytes)
            .map_err(error::from_engine)?;
        let ids = selection
            .element_ids
            .ok_or_else(|| crate::Error::InvalidData(format!("{variable}: the engine reported no cross-section identities")))?;
        let n = ids.len();
        let rows = match (v.element_axis, selection.values.shape.as_slice()) {
            (Some(0), [m]) if *m as usize == n => 1,
            (Some(1), [r, m]) if *m as usize == n => *r as usize,
            (_, shape) => {
                return Err(crate::Error::Unsupported(format!(
                    "Summary Output/Cross Sections/{variable}: shape {shape:?} is not one value row per cross section"
                )));
            }
        };
        let values = numbers(selection.values.values, variable)?;
        super::check_value_count(values.len(), rows as u64, n as u64, &format!("Summary Output/Cross Sections/{variable}"))?;
        let base = field_name(variable);
        let labels: Vec<Option<(String, Option<String>)>> = match (&v.row_labels, rows) {
            (Some(l), r) if l.len() == r => l.iter().map(|c| Some((c.name.clone(), c.unit.clone()))).collect(),
            (None, 1) => vec![None],
            _ => {
                return Err(crate::Error::Unsupported(format!(
                    "Summary Output/Cross Sections/{variable}: {rows} rows without proven row labels"
                )));
            }
        };
        let time_rows = &v.time_of_extreme_rows;
        let mut columns = Vec::with_capacity(rows);
        for (r, label) in labels.into_iter().enumerate() {
            let field = if r == 0 {
                base.clone()
            } else if time_rows.len() == 1 && time_rows[0] == r as u64 {
                format!("{base}_time")
            } else {
                let l = label.as_ref().map(|l| field_name(&l.0)).unwrap_or_default();
                if l.is_empty() {
                    return Err(crate::Error::Unsupported(format!(
                        "Summary Output/Cross Sections/{variable}: row {r} has no usable label"
                    )));
                }
                format!("{base}_{l}")
            };
            let (kind, row_values) = column(&values[r * n..(r + 1) * n]);
            let (label, unit) = label.map_or((None, None), |(l, u)| (Some(l), u));
            columns.push(Column { field, label, unit, kind, values: row_values });
        }
        Ok((ids, columns, v.path.clone()))
    }

    fn xs_result(&self, variable: &str, row: u64, options: &CrossSectionOptions) -> crate::Result<(Vec<Identity>, Vec<Column>, String)> {
        let s = &self.session;
        let inventory = s.xs_result_inventory(options.mode, options.metadata_max_bytes).map_err(error::from_engine)?;
        error::require(&inventory.capability, &inventory.group)?;
        let v = inventory
            .variables
            .iter()
            .find(|v| v.name == variable)
            .ok_or_else(|| crate::Error::InvalidArgument(format!("HEC-RAS not_found: {} has no {variable:?}", inventory.group)))?;
        error::require(&v.capability, &v.path)?;
        if v.domain != XsResultDomain::CrossSection {
            return Err(crate::Error::Unsupported(format!(
                "{}: stored as {}, not one value per cross section and row",
                v.path,
                v.domain.as_str()
            )));
        }
        let selection = s
            .read_xs_result(
                options.mode,
                variable,
                Slice::new(row, row.saturating_add(1), 1),
                None,
                options.metadata_max_bytes,
                options.max_bytes,
            )
            .map_err(error::from_engine)?;
        let n = selection.section_ids.len();
        if selection.values.shape.as_slice() != [1, n as u64] {
            return Err(crate::Error::InvalidData(format!("{} row {row}: shape {:?}, not (1, {n})", v.path, selection.values.shape)));
        }
        let (kind, values) = column(&numbers(selection.values.values, variable)?);
        let label = v.labels.value().and_then(|l| (l.len() == 1).then(|| l[0].clone()));
        let unit = v.units.units.value().cloned().or_else(|| label.as_ref().and_then(|l| l.unit.clone()));
        let column = Column { field: field_name(variable), label: label.map(|l| l.name), unit, kind, values };
        Ok((selection.section_ids, vec![column], v.path.clone()))
    }

    fn xs_row_label(&self, mode: ResultMode, row: u64, options: &CrossSectionOptions) -> crate::Result<Option<String>> {
        match mode {
            ResultMode::Steady => {
                let names = self.session.steady_profile_names(options.metadata_max_bytes).map_err(error::from_engine)?;
                let i = usize::try_from(row).ok().filter(|&i| i < names.len()).ok_or_else(|| {
                    crate::Error::InvalidArgument(format!("steady profile {row} is outside the {} stored profiles", names.len()))
                })?;
                Ok(Some(names[i].clone()))
            }
            _ => {
                let t = self
                    .session
                    .unsteady_row_times(Slice::new(row, row.saturating_add(1), 1), options.metadata_max_bytes)
                    .map_err(error::from_engine)?;
                Ok(t.raw_strings.and_then(|d| d.into_iter().next()))
            }
        }
    }
}

/// The layer feature index of each `source_row` (rows of `Geometry/Cross Sections/Attributes`).
fn source_rows(layer: &Layer) -> crate::Result<HashMap<u64, usize>> {
    let mut index = HashMap::with_capacity(layer.len());
    for (i, f) in layer.features.iter().enumerate() {
        let row = match f.attributes.first() {
            Some(FieldValue::Integer(r)) => u64::try_from(*r).map_err(|_| crate::Error::InvalidData("negative source_row".into()))?,
            _ => return Err(crate::Error::InvalidData("cross section without source_row".into())),
        };
        if index.insert(row, i).is_some() {
            return Err(crate::Error::InvalidData(format!("cross_sections: source_row {row} twice")));
        }
    }
    Ok(index)
}

/// Adds `columns` to the layer, value `i` of each going to the feature of identity `ids[i]`.
/// Every feature must receive exactly one value.
fn join(
    layer: &mut Layer,
    index: &HashMap<u64, usize>,
    variable: &str,
    dataset: &str,
    ids: &[Identity],
    columns: Vec<Column>,
) -> crate::Result<CrossSectionField> {
    if ids.len() != layer.len() {
        return Err(crate::Error::InvalidData(format!("{dataset}: {} result columns for {} cross sections", ids.len(), layer.len())));
    }
    let mut target = vec![usize::MAX; ids.len()];
    let mut seen = vec![false; layer.len()];
    for (i, id) in ids.iter().enumerate() {
        let feature = match index.get(&id.row) {
            Some(&f) if id.table == XS_ATTRIBUTES && !seen[f] => f,
            _ => {
                return Err(crate::Error::InvalidData(format!(
                    "{dataset}: column {i} ({} row {}) matches no single cross section",
                    id.table, id.row
                )));
            }
        };
        seen[feature] = true;
        target[i] = feature;
    }
    let mut report = CrossSectionField {
        variable: variable.to_string(),
        dataset: dataset.to_string(),
        fields: Vec::new(),
        labels: Vec::new(),
        units: Vec::new(),
    };
    for column in columns {
        if layer.schema.fields().iter().any(|f| f.name == column.field) {
            return Err(crate::Error::InvalidArgument(format!("{variable:?} gives the field {:?} twice", column.field)));
        }
        layer.add_field(FieldDef::new(column.field.clone(), column.kind));
        let mut by_feature = vec![FieldValue::Null; layer.len()];
        for (i, v) in column.values.into_iter().enumerate() {
            by_feature[target[i]] = v;
        }
        for (feature, v) in layer.features.iter_mut().zip(by_feature) {
            feature.attributes.push(v);
        }
        report.fields.push(column.field);
        report.labels.push(column.label);
        report.units.push(column.unit);
    }
    Ok(report)
}

fn column(values: &[FieldValue]) -> (FieldType, Vec<FieldValue>) {
    (kind_of(values), values.to_vec())
}

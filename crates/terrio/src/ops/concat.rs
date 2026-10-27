//! Concatenating vector layers, as `gdal vector concat` (and `ogrmerge.py`) do: layers of the
//! same name merged (`merge-per-layer-name`), every layer kept apart under a name built from a
//! template (`stack`), or everything merged into one layer (`single`), with the union or the
//! intersection of the source fields, an optional field naming each feature's source layer,
//! and optional reprojection to a common CRS.

use crate::crs::{SpatialRef, TransformOptions};
use crate::ops::vector_schema::{self as vs, OgrType};
use crate::{Error, Result};
use wbvector::{Feature, FieldValue, Layer};

/// How output layers are made from source layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConcatMode {
    /// One output layer per distinct source layer name.
    #[default]
    MergePerLayerName,
    /// One output layer per source layer, named from a template.
    Stack,
    /// One output layer.
    Single,
}

impl ConcatMode {
    /// Parses `merge-per-layer-name`, `stack` or `single`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "merge-per-layer-name" => Self::MergePerLayerName,
            "stack" => Self::Stack,
            "single" => Self::Single,
            _ => return Err(Error::InvalidArgument(format!("unknown concat mode '{s}': expected merge-per-layer-name, stack or single"))),
        })
    }
}

/// How the fields of an output layer are made from those of its source layers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FieldStrategy {
    /// Every field of every source layer.
    #[default]
    Union,
    /// The fields every source layer has.
    Intersection,
}

impl FieldStrategy {
    /// Parses `union` or `intersection`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "union" => Self::Union,
            "intersection" => Self::Intersection,
            _ => return Err(Error::InvalidArgument(format!("unknown field strategy '{s}': expected union or intersection"))),
        })
    }
}

/// A source dataset: its name (a path, whose base name the name templates use) and layers.
#[derive(Debug, Clone)]
pub struct ConcatSource {
    /// The dataset name.
    pub name: String,
    /// Its layers.
    pub layers: Vec<Layer>,
}

/// Options of [`concat()`].
#[derive(Debug, Clone, Default)]
pub struct ConcatOptions {
    /// How output layers are made.
    pub mode: ConcatMode,
    /// The output layer name (`single` mode, `merged` by default) or name template (`stack`
    /// mode, `{AUTO_NAME}` by default). Not allowed in `merge-per-layer-name` mode.
    pub output_layer: Option<String>,
    /// The name of a field added first to each output layer, naming each feature's source
    /// (`source_ds_lyr` when only the content is given).
    pub source_layer_field_name: Option<String>,
    /// The template of that field's values (`{AUTO_NAME}` by default).
    pub source_layer_field_content: Option<String>,
    /// Union or intersection of the source fields.
    pub field_strategy: FieldStrategy,
    /// The CRS of every source layer, overriding what they declare.
    pub src_crs: Option<SpatialRef>,
    /// Reproject every source layer to this CRS.
    pub dst_crs: Option<SpatialRef>,
    /// Coordinate operation controls for the reprojection.
    pub transform_options: TransformOptions,
}

/// The base name of a dataset name: its file name without extension.
fn basename(ds: &str) -> String {
    let file = ds.rsplit(['/', '\\']).next().unwrap_or(ds);
    match file.rfind('.') {
        Some(i) if i > 0 => file[..i].to_string(),
        _ => file.to_string(),
    }
}

/// A name from a template, as GDAL builds it: `{AUTO_NAME}` (`{DS_BASENAME}_{LAYER_NAME}`, or
/// the layer name when both are the same), `{DS_NAME}`, `{DS_BASENAME}`, `{DS_INDEX}`,
/// `{LAYER_NAME}` and `{LAYER_INDEX}` (indexes from 0).
pub fn layer_name_template(template: &str, ds_index: usize, ds_name: &str, layer_index: usize, layer_name: &str) -> String {
    let base = basename(ds_name);
    let base = if base.is_empty() { ds_name.to_string() } else { base };
    let auto = if base == layer_name { base.clone() } else { format!("{base}_{layer_name}") };
    let (ds_index, layer_index) = (ds_index.to_string(), layer_index.to_string());
    fill_template(
        template,
        &[
            ("AUTO_NAME", &auto),
            ("DS_BASENAME", &base),
            ("DS_NAME", ds_name),
            ("DS_INDEX", &ds_index),
            ("LAYER_NAME", layer_name),
            ("LAYER_INDEX", &layer_index),
        ],
    )
}

/// A template with its `{NAME}` variables replaced, in one pass over the template (values
/// holding braces are not substituted again). Unknown variables are kept.
pub(crate) fn fill_template(template: &str, vars: &[(&str, &str)]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        match after.find('}').and_then(|close| vars.iter().find(|(k, _)| *k == &after[..close]).map(|(_, v)| (close, *v))) {
            Some((close, v)) => {
                out.push_str(v);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Concatenates the layers of datasets, as `gdal vector concat` does. Output layers come in
/// the order of their names (as GDAL orders them); each has the geometry type and CRS of its
/// first source layer (or `dst_crs`), and the union (or intersection) of the source fields,
/// fields of the same name merged into a type that holds both (reals over integers, 64-bit
/// over 32-bit integers, strings otherwise). Features of source layers in another CRS are
/// reprojected to the output layer's.
pub fn concat(sources: &[ConcatSource], o: &ConcatOptions) -> Result<Vec<Layer>> {
    let template = match (o.mode, &o.output_layer) {
        (ConcatMode::MergePerLayerName, Some(_)) => {
            return Err(Error::InvalidArgument("an output layer name cannot be given in merge-per-layer-name mode".into()));
        }
        (ConcatMode::Single, None) => "merged".to_string(),
        (ConcatMode::Stack, None) => "{AUTO_NAME}".to_string(),
        (_, Some(t)) => t.clone(),
        (ConcatMode::MergePerLayerName, None) => String::new(),
    };
    let content = o.source_layer_field_content.clone().unwrap_or_else(|| "{AUTO_NAME}".into());
    let source_field = match (&o.source_layer_field_name, &o.source_layer_field_content) {
        (Some(n), _) => Some(n.clone()),
        (None, Some(_)) => Some("source_ds_lyr".to_string()),
        (None, None) => None,
    };
    // Output layer name -> (dataset index, layer index) of its sources, in name order.
    let mut groups: std::collections::BTreeMap<String, Vec<(usize, usize)>> = Default::default();
    for (di, ds) in sources.iter().enumerate() {
        for (li, l) in ds.layers.iter().enumerate() {
            if o.dst_crs.is_some() && o.src_crs.is_none() && SpatialRef::from_layer(l).is_unknown() {
                return Err(Error::Crs(format!("layer '{}' of '{}' has no spatial reference system", l.name, ds.name)));
            }
            let name = match o.mode {
                ConcatMode::Single => template.clone(),
                ConcatMode::MergePerLayerName => l.name.clone(),
                ConcatMode::Stack => layer_name_template(&template, di, &ds.name, li, &l.name),
            };
            groups.entry(name).or_default().push((di, li));
        }
    }
    groups.into_iter().map(|(name, members)| merge(sources, &name, &members, source_field.as_deref(), &content, o)).collect()
}

fn merge(
    sources: &[ConcatSource],
    name: &str,
    members: &[(usize, usize)],
    source_field: Option<&str>,
    content: &str,
    o: &ConcatOptions,
) -> Result<Layer> {
    // The source layers, with their CRS overridden, and reprojected to the output CRS (that of
    // the first layer, or `dst_crs`) as GDAL's union layers reproject them.
    let mut layers: Vec<Layer> = members.iter().map(|&(di, li)| sources[di].layers[li].clone()).collect();
    if let Some(s) = &o.src_crs {
        layers.iter_mut().for_each(|l| s.apply_to_layer(l));
    }
    let target = o.dst_crs.clone().or_else(|| layers.first().map(SpatialRef::from_layer).filter(|s| !s.is_unknown()));
    if let Some(t) = &target {
        for l in &mut layers {
            if !SpatialRef::from_layer(l).is_unknown() {
                *l = crate::vector::reproject_layer_with_options(std::mem::replace(l, Layer::new("")), t, &o.transform_options)?;
            }
        }
    }

    // The output fields: (name, type, width, precision), with the source field first.
    struct Out {
        name: String,
        t: OgrType,
        width: usize,
        precision: usize,
    }
    let mut fields: Vec<Out> = Vec::new();
    let source_types: Vec<Vec<OgrType>> = layers.iter().map(|l| (0..l.schema.len()).map(|i| vs::ogr_type(l, i)).collect()).collect();
    let merge_into = |f: &mut Out, t: OgrType, def: &wbvector::FieldDef| {
        f.t = vs::merge_types(f.t, t);
        if f.width != def.width || f.precision != def.precision {
            f.width = 0;
            f.precision = 0;
        }
    };
    match o.field_strategy {
        FieldStrategy::Union => {
            for (l, types) in layers.iter().zip(&source_types) {
                for (def, &t) in l.schema.fields().iter().zip(types) {
                    match fields.iter_mut().find(|f| f.name == def.name) {
                        Some(f) => merge_into(f, t, def),
                        None => fields.push(Out { name: def.name.clone(), t, width: def.width, precision: def.precision }),
                    }
                }
            }
        }
        FieldStrategy::Intersection => {
            if let Some(first) = layers.first() {
                fields = first
                    .schema
                    .fields()
                    .iter()
                    .zip(&source_types[0])
                    .map(|(d, &t)| Out { name: d.name.clone(), t, width: d.width, precision: d.precision })
                    .collect();
            }
            for (l, types) in layers.iter().zip(&source_types).skip(1) {
                fields.retain_mut(|f| match l.schema.fields().iter().position(|d| d.name.eq_ignore_ascii_case(&f.name)) {
                    Some(i) => {
                        merge_into(f, types[i], &l.schema.fields()[i]);
                        true
                    }
                    None => false,
                });
            }
        }
    }

    if let Some(source_field) = source_field
        && fields.iter().any(|field| field.name.eq_ignore_ascii_case(source_field))
    {
        return Err(Error::InvalidArgument(format!("source-layer field '{source_field}' already exists; choose an unused name")));
    }
    let first = layers.first();
    let mut out = Layer::new(name);
    out.geom_type = first.and_then(|l| l.geom_type).filter(|t| layers.iter().all(|l| l.geom_type == Some(*t)));
    if let Some(t) = &target {
        t.apply_to_layer(&mut out);
    }
    if let Some(sf) = source_field {
        out.add_field(wbvector::FieldDef::new(sf, wbvector::FieldType::Text));
    }
    for f in &fields {
        out.add_field(vs::field_def(&f.name, f.t, f.width, f.precision));
    }

    let offset = usize::from(source_field.is_some());
    let mut fid = 0u64;
    for ((l, types), &(di, li)) in layers.iter().zip(&source_types).zip(members) {
        let label = source_field.map(|_| layer_name_template(content, di, &sources[di].name, li, &sources[di].layers[li].name));
        // For each output field, the source field it comes from.
        let map: Vec<Option<usize>> = fields
            .iter()
            .map(|f| match o.field_strategy {
                // Union fields are merged by exact name, intersection fields ignoring case.
                FieldStrategy::Union => l.schema.fields().iter().position(|d| d.name == f.name),
                FieldStrategy::Intersection => l.schema.fields().iter().position(|d| d.name.eq_ignore_ascii_case(&f.name)),
            })
            .collect();
        for feature in &l.features {
            fid += 1;
            let mut attributes = Vec::with_capacity(fields.len() + offset);
            if let Some(label) = &label {
                attributes.push(FieldValue::Text(label.clone()));
            }
            for (f, m) in fields.iter().zip(&map) {
                attributes.push(match m {
                    Some(i) => match feature.attributes.get(*i) {
                        Some(v) => vs::convert_value(v, &l.schema.fields()[*i], types[*i], f.t),
                        None => FieldValue::Null,
                    },
                    None => FieldValue::Null,
                });
            }
            out.push(Feature { fid, geometry: feature.geometry.clone(), attributes });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{FieldDef, FieldType, Geometry};

    fn layer(name: &str, fields: &[(&str, FieldType)], rows: &[Vec<FieldValue>]) -> Layer {
        let mut l = Layer::new(name).with_epsg(4326);
        for (n, t) in fields {
            l.add_field(FieldDef::new(*n, *t));
        }
        for (i, r) in rows.iter().enumerate() {
            l.push(Feature { fid: i as u64, geometry: Some(Geometry::point(i as f64, 1.0)), attributes: r.clone() });
        }
        l
    }

    fn sources() -> Vec<ConcatSource> {
        let a = layer(
            "a",
            &[("id", FieldType::Integer), ("name", FieldType::Text)],
            &[vec![FieldValue::Integer(1), FieldValue::Text("x".into())]],
        );
        let b1 = layer("b", &[("id", FieldType::Float)], &[vec![FieldValue::Float(2.5)]]);
        let b2 = layer(
            "b",
            &[("ID", FieldType::Integer), ("extra", FieldType::Text)],
            &[vec![FieldValue::Integer(3), FieldValue::Text("e".into())]],
        );
        vec![ConcatSource { name: "data/one.gpkg".into(), layers: vec![a, b1] }, ConcatSource { name: "two.gpkg".into(), layers: vec![b2] }]
    }

    #[test]
    fn source_layer_field_cannot_duplicate_an_output_field() {
        let options = ConcatOptions { mode: ConcatMode::Single, source_layer_field_name: Some("name".into()), ..Default::default() };
        let error = concat(&sources(), &options).unwrap_err();
        assert!(error.to_string().contains("already exists"));
    }

    #[test]
    fn merges_layers_of_the_same_name() {
        let out = concat(&sources(), &ConcatOptions::default()).unwrap();
        assert_eq!(out.iter().map(|l| (l.name.as_str(), l.len())).collect::<Vec<_>>(), [("a", 1), ("b", 2)]);
        // Union by exact name: `id` (real) and `ID` stay apart.
        let names: Vec<&str> = out[1].schema.fields().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["id", "ID", "extra"]);
        let values: Vec<Vec<FieldValue>> = out[1].features.iter().map(|f| f.attributes.clone()).collect();
        assert_eq!(
            values,
            [
                vec![FieldValue::Float(2.5), FieldValue::Null, FieldValue::Null],
                vec![FieldValue::Null, FieldValue::Integer(3), FieldValue::Text("e".into())]
            ]
        );
    }

    #[test]
    fn stacks_with_templates_and_source_fields() {
        let o = ConcatOptions {
            mode: ConcatMode::Stack,
            source_layer_field_content: Some("{DS_INDEX}:{LAYER_NAME}".into()),
            ..Default::default()
        };
        let out = concat(&sources(), &o).unwrap();
        assert_eq!(out.iter().map(|l| l.name.as_str()).collect::<Vec<_>>(), ["one_a", "one_b", "two_b"]);
        assert_eq!(out[2].schema.fields()[0].name, "source_ds_lyr");
        assert_eq!(out[2].features[0].attributes[0], FieldValue::Text("1:b".into()));
    }

    #[test]
    fn single_layer_with_intersection() {
        let o = ConcatOptions { mode: ConcatMode::Single, field_strategy: FieldStrategy::Intersection, ..Default::default() };
        let out = concat(&sources(), &o).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "merged");
        // `id` is in every layer (ignoring case); integers and reals merge into reals.
        assert_eq!(out[0].schema.fields().iter().map(|f| (f.name.as_str(), f.field_type)).collect::<Vec<_>>(), [("id", FieldType::Float)]);
        assert_eq!(out[0].features.iter().map(|f| f.attributes[0].clone()).collect::<Vec<_>>(), [1.0, 2.5, 3.0].map(FieldValue::Float));
        assert!(concat(&sources(), &ConcatOptions { output_layer: Some("x".into()), ..Default::default() }).is_err());
    }

    #[test]
    fn mixed_declared_geometry_types_are_not_misreported() {
        let mut sources = sources();
        sources[0].layers[0].geom_type = Some(wbvector::GeometryType::Point);
        sources[0].layers[1].geom_type = Some(wbvector::GeometryType::LineString);
        let out = concat(&sources, &ConcatOptions { mode: ConcatMode::Single, ..Default::default() }).unwrap();
        assert_eq!(out[0].geom_type, None);
    }

    #[test]
    fn fills_templates_once() {
        assert_eq!(layer_name_template("{LAYER_NAME}-{DS_INDEX}", 3, "d.gpkg", 0, "x{DS_INDEX}"), "x{DS_INDEX}-3");
        assert_eq!(fill_template("{A}{B}{C}", &[("A", "{B}"), ("B", "b")]), "{B}b{C}");
    }

    #[test]
    fn reprojects_to_a_common_crs() {
        let o = ConcatOptions { mode: ConcatMode::Single, dst_crs: Some(SpatialRef::from_epsg(3857).unwrap()), ..Default::default() };
        let out = concat(&sources(), &o).unwrap();
        assert_eq!(out[0].crs_epsg(), Some(3857));
        let Some(Geometry::Point(c)) = &out[0].features[1].geometry else { panic!() };
        assert!((c.x - 0.0).abs() < 1e-6 && c.y > 100_000.0, "{c:?}");
    }
}

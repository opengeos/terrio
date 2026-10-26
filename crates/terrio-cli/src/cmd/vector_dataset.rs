//! Dataset-level vector commands: `concat`, `partition`, `set-field-type`, `make-point`,
//! `create` and `export-schema` (GDAL's `gdal vector concat`, `partition`, `set-field-type`,
//! `make-point`, `create` and `export-schema`).

use super::processing::{VectorInput, VectorOutput};
use terrio::crs::SpatialRef;
use terrio::formats::VectorDriver;
use terrio::ops::concat::{ConcatMode, ConcatOptions, ConcatSource, FieldStrategy};
use terrio::ops::partition::{PartitionOptions, PartitionScheme, PartitionWriteMode};
use terrio::ops::vector_schema::{self as vs, FieldSelector, OgrSchema, OgrType};
use terrio::{Error, Result};

fn srs(s: &Option<String>) -> Result<Option<SpatialRef>> {
    s.as_deref().map(SpatialRef::parse).transpose()
}

fn driver(format: Option<&str>) -> Result<Option<VectorDriver>> {
    format.map(|f| VectorDriver::from_name(f).ok_or_else(|| Error::InvalidArgument(format!("unknown vector format '{f}'")))).transpose()
}

#[derive(clap::Args)]
pub struct ConcatArgs {
    /// The input vector datasets, then the output dataset.
    #[arg(required = true, num_args = 2.., value_name = "INPUT... OUTPUT")]
    paths: Vec<String>,
    /// Read only this layer of each input (repeat for several). By default, every layer.
    #[arg(long = "layer", short = 'l', visible_alias = "input-layer")]
    layers: Vec<String>,
    /// The output format's short name. By default, chosen from the output's extension.
    #[arg(short = 'f', long = "format", visible_alias = "of")]
    format: Option<String>,
    /// merge-per-layer-name (one output layer per layer name), stack (one per input layer) or
    /// single (one output layer).
    #[arg(long, default_value = "merge-per-layer-name")]
    mode: String,
    /// The output layer name (single mode, "merged" by default), or name template (stack mode,
    /// "{AUTO_NAME}" by default) with {AUTO_NAME}, {DS_NAME}, {DS_BASENAME}, {DS_INDEX},
    /// {LAYER_NAME} and {LAYER_INDEX}.
    #[arg(long = "output-layer", visible_alias = "nln", value_name = "NAME")]
    output_layer: Option<String>,
    /// Add a field naming each feature's source layer (first in the output layer).
    #[arg(long = "source-layer-field-name", value_name = "NAME")]
    source_layer_field_name: Option<String>,
    /// The template of that field's values ("{AUTO_NAME}" by default); alone, it adds a field
    /// named source_ds_lyr.
    #[arg(long = "source-layer-field-content", value_name = "TEMPLATE")]
    source_layer_field_content: Option<String>,
    /// union (every source field) or intersection (the fields of every source layer).
    #[arg(long = "field-strategy", default_value = "union")]
    field_strategy: String,
    /// The CRS of the inputs, overriding what they declare.
    #[arg(short = 's', long = "src-crs", visible_alias = "s-srs", value_name = "CRS")]
    src_crs: Option<String>,
    /// Reproject every input to this CRS.
    #[arg(short = 'd', long = "dst-crs", visible_alias = "t-srs", value_name = "CRS")]
    dst_crs: Option<String>,
    /// Append features to layers of the same name in an existing GeoPackage.
    #[arg(long, conflicts_with = "overwrite")]
    append: bool,
    /// Replace layers of the same name in an existing output, keeping other GeoPackage layers.
    #[arg(long)]
    overwrite: bool,
}

/// `terrio concat`: `gdal vector concat` and `ogrmerge.py`.
pub fn concat(a: ConcatArgs) -> Result<()> {
    let (output, inputs) = a.paths.split_last().ok_or_else(|| Error::InvalidArgument("give inputs and an output".into()))?;
    let options = terrio::VectorOpenOptions { layers: Vec::new(), ..Default::default() };
    let mut sources = Vec::with_capacity(inputs.len());
    for input in inputs {
        let mut layers = super::quiet(|| terrio::open_vector(input, &options))?.layers;
        if !a.layers.is_empty() {
            layers.retain(|l| a.layers.contains(&l.name));
        }
        sources.push(ConcatSource { name: input.clone(), layers });
    }
    let o = ConcatOptions {
        mode: ConcatMode::parse(&a.mode)?,
        output_layer: a.output_layer,
        source_layer_field_name: a.source_layer_field_name,
        source_layer_field_content: a.source_layer_field_content,
        field_strategy: FieldStrategy::parse(&a.field_strategy)?,
        src_crs: srs(&a.src_crs)?,
        dst_crs: srs(&a.dst_crs)?,
        ..Default::default()
    };
    let layers = terrio::ops::concat::concat(&sources, &o)?;
    if layers.is_empty() {
        return Err(Error::InvalidArgument("no input layer".into()));
    }
    VectorOutput::new(output, a.format, a.append, a.overwrite).write(layers)
}

/// A size such as `500MB` or `2GB` (1024-based; plain numbers are bytes).
fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let t = s.trim();
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let v: f64 = num.parse().map_err(|_| format!("invalid size '{s}'"))?;
    let m: f64 = match unit.trim().to_ascii_uppercase().as_str() {
        "" | "B" => 1.0,
        "K" | "KB" => 1024.0,
        "M" | "MB" => 1024.0 * 1024.0,
        "G" | "GB" => 1024.0 * 1024.0 * 1024.0,
        "T" | "TB" => 1024.0f64.powi(4),
        _ => return Err(format!("invalid size unit in '{s}': use MB or GB")),
    };
    Ok((v * m) as u64)
}

#[derive(clap::Args)]
pub struct PartitionArgs {
    #[command(flatten)]
    input: VectorInput,
    /// The output directory.
    output: String,
    /// The output format's short name. By default, the input's format.
    #[arg(short = 'f', long = "format", visible_alias = "of")]
    format: Option<String>,
    /// Partition on this field (repeat for nested partitions): String or Integer fields, or
    /// OGR_GEOMETRY for the geometry type.
    #[arg(long = "field")]
    fields: Vec<String>,
    /// hive (field=value directories) or flat (files named from the field values).
    #[arg(long, default_value = "hive")]
    scheme: String,
    /// The file name pattern, with {LAYER_NAME}, {FIELD_VALUE} and one %d part number
    /// ("part_%010d" for hive, "{LAYER_NAME}_{FIELD_VALUE}_%010d" for flat).
    #[arg(long)]
    pattern: Option<String>,
    /// The most features per file.
    #[arg(long = "feature-limit", value_name = "N")]
    feature_limit: Option<usize>,
    /// The largest estimated file size (MB or GB suffix), as GDAL estimates it.
    #[arg(long = "max-file-size", value_name = "SIZE", value_parser = parse_size)]
    max_file_size: Option<u64>,
    /// Leave the partitioning fields out of the files (automatic for Hive GeoParquet).
    #[arg(long = "omit-partitioned-field")]
    omit_partitioned_field: bool,
    /// Replace an existing output directory that looks like a partitioned dataset.
    #[arg(long, conflicts_with = "append")]
    overwrite: bool,
    /// Add files to an existing output directory.
    #[arg(long)]
    append: bool,
}

/// `terrio partition`: `gdal vector partition`.
pub fn partition(a: PartitionArgs) -> Result<()> {
    if a.overwrite {
        terrio::ops::partition::check_input_directory(&a.input.input, &a.output)?;
    }
    let ds = super::quiet(|| {
        terrio::open_vector(&a.input.input, &terrio::VectorOpenOptions { layers: a.input.layers.clone(), ..Default::default() })
    })?;
    let driver = match driver(a.format.as_deref())? {
        Some(d) => d,
        None if ds.driver.info().write && !ds.driver.info().extensions.is_empty() => ds.driver,
        None => return Err(Error::InvalidArgument("cannot infer the output format: give it with --format".into())),
    };
    let o = PartitionOptions {
        fields: a.fields,
        scheme: PartitionScheme::parse(&a.scheme)?,
        pattern: a.pattern,
        feature_limit: a.feature_limit,
        max_file_size: a.max_file_size,
        omit_partitioned_fields: a.omit_partitioned_field,
    };
    let mode = if a.overwrite {
        PartitionWriteMode::Overwrite
    } else if a.append {
        PartitionWriteMode::Append
    } else {
        PartitionWriteMode::Create
    };
    let options = terrio::VectorWriteOptions { driver: Some(driver), ..Default::default() };
    let written = terrio::ops::partition::partition_to_dir(&ds.layers, &a.output, &o, &options, mode)?;
    eprintln!("terrio: wrote {} file(s)", written.len());
    Ok(())
}

#[derive(clap::Args)]
pub struct SetFieldTypeArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Change only this layer (the others are copied). By default, every layer.
    #[arg(long = "active-layer", value_name = "NAME")]
    active_layer: Option<String>,
    /// The field to change.
    #[arg(long = "field-name", required_unless_present = "src_field_type", conflicts_with = "src_field_type")]
    field_name: Option<String>,
    /// Change every field of this type or subtype instead.
    #[arg(long = "src-field-type", visible_alias = "input-field-type", value_name = "TYPE")]
    src_field_type: Option<String>,
    /// The new type (Integer, Integer64, Real, String, Date, Time, DateTime, Binary) or subtype
    /// (Boolean, Int16, Float32, JSON, UUID).
    #[arg(long = "dst-field-type", visible_aliases = ["field-type", "output-field-type"], value_name = "TYPE")]
    dst_field_type: String,
}

/// `terrio set-field-type`: `gdal vector set-field-type`.
pub fn set_field_type(a: SetFieldTypeArgs) -> Result<()> {
    let to = OgrType::parse(&a.dst_field_type)?;
    let selector = match (&a.field_name, &a.src_field_type) {
        (Some(n), _) => FieldSelector::Name(n.clone()),
        (None, Some(t)) => FieldSelector::Type(OgrType::parse(t)?),
        (None, None) => return Err(Error::InvalidArgument("give --field-name or --src-field-type".into())),
    };
    let layers = a.input.open()?;
    let layers = vs::set_field_type_in(&layers, &selector, to, a.active_layer.as_deref())?;
    a.output.write(layers)
}

#[derive(clap::Args)]
pub struct MakePointArgs {
    /// The input vector dataset.
    input: String,
    #[command(flatten)]
    output: VectorOutput,
    /// The layer to read. By default, the first.
    #[arg(long = "layer", short = 'l', visible_alias = "input-layer")]
    layer: Option<String>,
    /// The field holding X coordinates.
    #[arg(long)]
    x: String,
    /// The field holding Y coordinates.
    #[arg(long)]
    y: String,
    /// The field holding Z coordinates.
    #[arg(long)]
    z: Option<String>,
    /// The field holding M values.
    #[arg(long)]
    m: Option<String>,
    /// The CRS of the points (assigned, not reprojected). By default, none.
    #[arg(long = "dst-crs", visible_aliases = ["output-crs", "a-srs"], value_name = "CRS")]
    dst_crs: Option<String>,
}

/// `terrio make-point`: `gdal vector make-point`.
pub fn make_point(a: MakePointArgs) -> Result<()> {
    let layers = a.layer.iter().cloned().collect();
    let ds = super::quiet(|| terrio::open_vector(&a.input, &terrio::VectorOpenOptions { layers, ..Default::default() }))?;
    let layer = ds.layers.into_iter().next().ok_or_else(|| Error::InvalidArgument("no input vector layer".into()))?;
    let o = terrio::ops::make_point::MakePointOptions { x: a.x, y: a.y, z: a.z, m: a.m, crs: srs(&a.dst_crs)? };
    a.output.write(vec![terrio::ops::make_point::make_point(&layer, &o)?])
}

#[derive(clap::Args)]
pub struct ExportSchemaArgs {
    /// The input vector dataset.
    input: String,
    /// Only these layers (repeat for several). By default, every layer.
    #[arg(long = "layer", short = 'l', visible_alias = "input-layer")]
    layers: Vec<String>,
    /// Write the schema to this file instead of standard output.
    #[arg(short = 'o', long)]
    output: Option<String>,
    /// Replace an existing schema output.
    #[arg(long)]
    overwrite: bool,
}

/// `terrio export-schema`: `gdal vector export-schema`.
pub fn export_schema(a: ExportSchemaArgs) -> Result<()> {
    let schema = super::quiet(|| vs::export_schema_path(&a.input, &a.layers))?;
    let json = schema.to_json();
    match a.output {
        Some(path) => {
            if path == a.input || super::rasterout::same_file(&path, &a.input) {
                return Err(Error::InvalidArgument("the output would overwrite the input".into()));
            }
            existing_output(&path, a.overwrite)?;
            std::fs::write(path, json + "\n")?;
        }
        None => println!("{json}"),
    }
    Ok(())
}

#[derive(clap::Args)]
pub struct CreateArgs {
    /// The dataset to create.
    output: String,
    /// The output format's short name. By default, chosen from the output's extension.
    #[arg(short = 'f', long = "format", visible_alias = "of")]
    format: Option<String>,
    /// An OGR_SCHEMA document, or a file holding one (as `terrio export-schema` writes).
    #[arg(long, conflicts_with_all = ["like", "geometry_type", "crs", "fields", "fid"])]
    schema: Option<String>,
    /// Use the layers of this dataset as templates (their fields, geometry types and CRSs).
    #[arg(long, visible_alias = "input", value_name = "DATASET")]
    like: Option<String>,
    /// The template layers of --like or --schema (repeat for several). By default, every layer.
    #[arg(long = "layer", short = 'l', visible_alias = "input-layer")]
    layers: Vec<String>,
    /// The layer name (a single layer; required without --like or --schema).
    #[arg(long = "output-layer", visible_alias = "nln", value_name = "NAME")]
    output_layer: Option<String>,
    /// The geometry type: GEOMETRY, POINT, LINESTRING, POLYGON, MULTIPOINT, MULTILINESTRING,
    /// MULTIPOLYGON or GEOMETRYCOLLECTION (Z, M or ZM suffixes are accepted, not recorded).
    #[arg(long = "geometry-type", conflicts_with = "like")]
    geometry_type: Option<String>,
    /// The CRS of the layer.
    #[arg(long, conflicts_with = "like")]
    crs: Option<String>,
    /// The FID column name (GeoPackage).
    #[arg(long, conflicts_with = "like")]
    fid: Option<String>,
    /// A field, NAME:TYPE[(WIDTH[,PRECISION])], TYPE one of INTEGER, INTEGER64, REAL, STRING,
    /// DATE, TIME, DATETIME, BINARY (repeat for several).
    #[arg(long = "field", conflicts_with = "like", value_name = "NAME:TYPE")]
    fields: Vec<String>,
    /// Add the layers to an existing GeoPackage.
    #[arg(long, conflicts_with = "overwrite")]
    update: bool,
    /// Replace an existing dataset.
    #[arg(long)]
    overwrite: bool,
}

/// `terrio create`: `gdal vector create`.
pub fn create(a: CreateArgs) -> Result<()> {
    let schema = match (&a.schema, &a.like) {
        (Some(s), _) => {
            let text = if s.trim_start().starts_with('{') { s.clone() } else { std::fs::read_to_string(s)? };
            let mut schema = OgrSchema::from_json(&text)?;
            if !a.layers.is_empty() {
                schema.layers.retain(|l| a.layers.contains(&l.name));
            }
            Some(schema)
        }
        (None, Some(like)) => Some(super::quiet(|| vs::export_schema_path(like, &a.layers))?),
        (None, None) => None,
    };
    let crs = srs(&a.crs)?;
    let created = vs::created_layers(
        schema.as_ref(),
        a.output_layer.as_deref(),
        &a.fields,
        a.geometry_type.as_deref(),
        crs.as_ref(),
        a.fid.as_deref(),
    )?;
    let o = vs::CreateOptions {
        write: terrio::VectorWriteOptions { driver: driver(a.format.as_deref())?, ..Default::default() },
        update: a.update,
        overwrite: a.overwrite,
    };
    vs::create(&created, &a.output, &o)
}

#[derive(clap::Args)]
pub struct RenameLayerArgs {
    /// The input vector dataset.
    input: String,
    /// The output dataset. Without it, a GeoPackage input is changed in place.
    output: Option<String>,
    /// The output format's short name. By default, chosen from the output's extension.
    #[arg(short = 'f', long = "format", visible_alias = "of", requires = "output")]
    format: Option<String>,
    /// The layer to rename to --output-layer (needed when there are several layers).
    #[arg(short = 'l', long = "input-layer", visible_alias = "layer", requires = "output_layer")]
    input_layer: Option<String>,
    /// The new name of --input-layer, or of the only layer.
    #[arg(long = "output-layer", visible_alias = "nln", value_name = "NAME")]
    output_layer: Option<String>,
    /// Replace accented Latin characters with ASCII ones, others with --replacement-character.
    #[arg(long)]
    ascii: bool,
    /// Lower-case ASCII letters.
    #[arg(long = "lower-case")]
    lower_case: bool,
    /// Make names usable as file names on Linux, Windows and macOS.
    #[arg(long = "filename-compatible")]
    filename_compatible: bool,
    /// Characters to remove (or replace with --replacement-character).
    #[arg(long = "reserved-characters", value_name = "CHARS")]
    reserved_characters: Option<String>,
    /// The character replacing removed characters.
    #[arg(long = "replacement-character", value_name = "CHAR")]
    replacement_character: Option<char>,
    /// Truncate names to this many characters.
    #[arg(long = "max-length", value_name = "N")]
    max_length: Option<usize>,
    /// Replace an existing output.
    #[arg(long)]
    overwrite: bool,
}

/// `terrio rename-layer`: `gdal vector rename-layer`, in place for GeoPackages.
pub fn rename_layer(a: RenameLayerArgs) -> Result<()> {
    let o = terrio::ops::vector_edit::RenameOptions {
        input_layer: a.input_layer,
        output_layer: a.output_layer,
        ascii: a.ascii,
        lower_case: a.lower_case,
        filename_compatible: a.filename_compatible,
        reserved_characters: a.reserved_characters,
        replacement_character: a.replacement_character,
        max_length: a.max_length,
    };
    match &a.output {
        None => {
            for (old, new) in terrio::ops::vector_edit::rename_layers_in_place(&a.input, &o)? {
                eprintln!("terrio: renamed '{old}' to '{new}'");
            }
            Ok(())
        }
        Some(out) => {
            let mut layers = super::quiet(|| terrio::open_vector(&a.input, &Default::default()))?.layers;
            terrio::ops::vector_edit::rename_layers(&mut layers, &o)?;
            existing_output(out, a.overwrite)?;
            let options = terrio::VectorWriteOptions { driver: driver(a.format.as_deref())?, ..Default::default() };
            terrio::write_vector(&layers, out, &options)
        }
    }
}

/// Refuses an existing output unless it is to be replaced.
fn existing_output(path: &str, overwrite: bool) -> Result<()> {
    if std::path::Path::new(path).exists() && !overwrite {
        return Err(Error::InvalidArgument(format!("'{path}' already exists: give --overwrite to replace it")));
    }
    Ok(())
}

#[derive(clap::Args)]
pub struct EditArgs {
    /// The input vector dataset.
    input: String,
    /// The output dataset. Without it, a GeoPackage input is changed in place.
    output: Option<String>,
    /// The output format's short name. By default, chosen from the output's extension.
    #[arg(short = 'f', long = "format", visible_alias = "of", requires = "output")]
    format: Option<String>,
    /// Change only this layer. By default, every layer.
    #[arg(long = "active-layer", visible_alias = "layer", short = 'l', value_name = "NAME")]
    active_layer: Option<String>,
    /// Assign this CRS without reprojecting ("none" removes it).
    #[arg(long, visible_alias = "a-srs")]
    crs: Option<String>,
    /// The declared geometry type (GEOMETRY, POINT, LINESTRING, POLYGON, MULTIPOINT,
    /// MULTILINESTRING, MULTIPOLYGON, GEOMETRYCOLLECTION, with Z, M or ZM). Geometries are not
    /// changed.
    #[arg(long = "geometry-type")]
    geometry_type: Option<String>,
    /// Set a dataset metadata item (repeat for several).
    #[arg(long, value_name = "KEY=VALUE")]
    metadata: Vec<String>,
    /// Remove a dataset metadata item (repeat for several).
    #[arg(long = "unset-metadata", value_name = "KEY")]
    unset_metadata: Vec<String>,
    /// Set a layer metadata item (repeat for several).
    #[arg(long = "layer-metadata", value_name = "KEY=VALUE")]
    layer_metadata: Vec<String>,
    /// Remove a layer metadata item (repeat for several).
    #[arg(long = "unset-layer-metadata", value_name = "KEY")]
    unset_layer_metadata: Vec<String>,
    /// Renumber the features and drop the FID column name (with an output).
    #[arg(long = "unset-fid", requires = "output")]
    unset_fid: bool,
    /// Replace an existing output.
    #[arg(long)]
    overwrite: bool,
}

/// `terrio edit`: `gdal vector edit`, in place for GeoPackages.
pub fn edit(a: EditArgs) -> Result<()> {
    use terrio::ops::vector_edit as ve;
    let o = ve::EditOptions {
        active_layer: a.active_layer,
        crs: a.crs.as_deref().map(ve::parse_crs_override).transpose()?,
        geometry_type: a.geometry_type.as_deref().map(vs::parse_geometry_type).transpose()?,
        metadata: ve::parse_metadata_items(&a.metadata)?,
        unset_metadata: a.unset_metadata,
        layer_metadata: ve::parse_metadata_items(&a.layer_metadata)?,
        unset_layer_metadata: a.unset_layer_metadata,
        unset_fid: a.unset_fid,
    };
    match &a.output {
        None => ve::edit_in_place(&a.input, &o),
        Some(out) => {
            existing_output(out, a.overwrite)?;
            let options = terrio::VectorWriteOptions { driver: driver(a.format.as_deref())?, ..Default::default() };
            super::quiet(|| ve::edit_to(&a.input, out, &o, &options))
        }
    }
}

#[derive(clap::Args)]
pub struct UpdateArgs {
    /// The input vector dataset, whose features update the output.
    input: String,
    /// The existing dataset to update (in place).
    output: String,
    /// The input layer (needed when the input has several).
    #[arg(short = 'l', long = "input-layer", visible_alias = "layer")]
    input_layer: Option<String>,
    /// The output layer (needed when the output has several).
    #[arg(long = "output-layer", visible_alias = "nln")]
    output_layer: Option<String>,
    /// A key field matching features (repeat for several). By default, feature ids.
    #[arg(long)]
    key: Vec<String>,
    /// merge (update matched features, append the others), update-only or append-only.
    #[arg(long, default_value = "merge")]
    mode: String,
}

/// `terrio update`: `gdal vector update`.
pub fn update(a: UpdateArgs) -> Result<()> {
    use terrio::ops::vector_edit as ve;
    if std::fs::canonicalize(&a.input).ok().zip(std::fs::canonicalize(&a.output).ok()).is_some_and(|(i, o)| i == o) {
        return Err(Error::InvalidArgument("input and output datasets must be different".into()));
    }
    let layers = a.input_layer.iter().cloned().collect();
    let src = super::quiet(|| terrio::open_vector(&a.input, &terrio::VectorOpenOptions { layers, ..Default::default() }))?.layers;
    let o =
        ve::UpdateOptions { input_layer: a.input_layer, output_layer: a.output_layer, key: a.key, mode: ve::UpdateMode::parse(&a.mode)? };
    let c = super::quiet(|| ve::update(&src, &a.output, &o))?;
    eprintln!("terrio: {} feature(s) updated, {} appended, {} unchanged or skipped", c.updated, c.appended, c.unchanged);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_size;

    #[test]
    fn parses_sizes() {
        assert_eq!(parse_size("2MB"), Ok(2 << 20));
        assert_eq!(parse_size("1.5 GB"), Ok(3 << 29));
        assert_eq!(parse_size("1048576"), Ok(1 << 20));
        assert!(parse_size("3 parsecs").is_err());
    }
}

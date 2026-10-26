//! Partitioning a vector dataset into files by field values, as `gdal vector partition` does:
//! Hive-style directories (`field=value/part_0000000001.ext`) or flat file names
//! (`{LAYER_NAME}_{FIELD_VALUE}_0000000001.ext`), with parts limited in feature count or
//! estimated file size.

use crate::formats::VectorDriver;
use crate::{Error, Result};
use wbvector::{FieldType, FieldValue, Geometry, Layer};

/// The value Hive partitioning gives null values.
pub const NULL_MARKER: &str = "__HIVE_DEFAULT_PARTITION__";

/// The smallest size a part file is estimated to have, as GDAL estimates it.
const MIN_FILE_SIZE: u64 = 65536;

/// How partitions are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PartitionScheme {
    /// Nested `field=value` directories under a directory per layer.
    #[default]
    Hive,
    /// Files directly in the output directory, named from the layer name and field values.
    Flat,
}

impl PartitionScheme {
    /// Parses `hive` or `flat`.
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s.to_ascii_lowercase().as_str() {
            "hive" => Self::Hive,
            "flat" => Self::Flat,
            _ => return Err(Error::InvalidArgument(format!("unknown partitioning scheme '{s}': expected hive or flat"))),
        })
    }
}

/// Options of [`partition`].
#[derive(Debug, Clone, Default)]
pub struct PartitionOptions {
    /// The fields to partition on, in directory order: String or Integer fields, or
    /// `OGR_GEOMETRY` for the geometry type. May be empty when a limit is given.
    pub fields: Vec<String>,
    /// The layout.
    pub scheme: PartitionScheme,
    /// The file name pattern, with `{LAYER_NAME}`, `{FIELD_VALUE}` and one `%d`, `%5d` or
    /// `%05d` part number (`part_%010d` for Hive, `{LAYER_NAME}_{FIELD_VALUE}_%010d` flat).
    pub pattern: Option<String>,
    /// The most features per part.
    pub feature_limit: Option<usize>,
    /// The largest estimated size of a part, in bytes (at least 1 MiB).
    pub max_file_size: Option<u64>,
    /// Leave the partitioning fields out of the parts (automatic for Hive GeoParquet).
    pub omit_partitioned_fields: bool,
}

/// One part: its path (relative to the output directory, `/`-separated, with the extension)
/// and its features.
#[derive(Debug, Clone)]
pub struct Partition {
    /// The relative path.
    pub path: String,
    /// The layer to write.
    pub layer: Layer,
}

/// Percent-encodes a name as GDAL encodes partition keys and file names: bytes outside `!` to
/// DEL, and `:`, `/`, `\`, `>`, `%`, `=`.
pub fn percent_encode(s: &str) -> String {
    if matches!(s, "." | "..") {
        return "%2E".repeat(s.len());
    }
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if c > 32 && c <= 127 && !matches!(c, b':' | b'/' | b'\\' | b'>' | b'%' | b'=') {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

/// A parsed file name pattern: the text before and after the part number, its digit count and
/// whether it pads with zeros.
struct Pattern {
    prefix: String,
    suffix: String,
    digits: usize,
    zeros: bool,
}

fn parse_pattern(p: &str) -> Result<Pattern> {
    // Part files stay in the output directory (or their partition's).
    if p.contains(['/', '\\']) || p.contains("..") {
        return Err(Error::InvalidArgument(format!("pattern '{p}': path separators and '..' are not allowed")));
    }
    let at = p.find('%').ok_or_else(|| Error::InvalidArgument("missing '%' character in pattern".into()))?;
    if p[at + 1..].contains('%') {
        return Err(Error::InvalidArgument("a single '%' character is expected in pattern".into()));
    }
    let spec: String = p[at + 1..].chars().take_while(char::is_ascii_digit).collect();
    if !p[at + 1 + spec.len()..].starts_with('d') {
        return Err(Error::InvalidArgument("pattern value must include a single '%[0]?[1-9]?[0]?d' part number specification".into()));
    }
    let digits = spec.parse::<usize>().unwrap_or(0);
    if digits > 10 {
        return Err(Error::InvalidArgument("number of digits in part number specification should be in [1,10] range".into()));
    }
    Ok(Pattern { prefix: p[..at].to_string(), suffix: p[at + 2 + spec.len()..].to_string(), digits, zeros: spec.starts_with('0') })
}

impl Pattern {
    fn name(&self, counter: usize) -> String {
        let n = counter.to_string();
        let pad = if self.zeros { '0' } else { ' ' };
        let fill: String = std::iter::repeat_n(pad, self.digits.saturating_sub(n.len())).collect();
        format!("{}{fill}{n}{}", self.prefix, self.suffix)
    }
}

/// The file extension GDAL gives a format's files (its first extension).
pub fn driver_extension(d: VectorDriver) -> &'static str {
    match d {
        VectorDriver::GeoJSON => "json",
        VectorDriver::MapInfo => "tab",
        _ => d.info().extensions.first().copied().unwrap_or("dat"),
    }
}

/// The size of a geometry's WKB encoding.
fn wkb_size(g: &Geometry) -> u64 {
    let (z, m) = crate::vector::geometry_dims(g);
    let coord = 16 + 8 * (u64::from(z) + u64::from(m));
    let ring = |n: usize| 4 + n as u64 * coord;
    match g {
        Geometry::Point(_) => 5 + coord,
        Geometry::LineString(cs) => 9 + cs.len() as u64 * coord,
        Geometry::MultiPoint(cs) => 9 + cs.len() as u64 * (5 + coord),
        Geometry::Polygon { exterior, interiors } => 9 + ring(exterior.0.len()) + interiors.iter().map(|r| ring(r.0.len())).sum::<u64>(),
        Geometry::MultiLineString(ls) => 9 + ls.iter().map(|l| 9 + l.len() as u64 * coord).sum::<u64>(),
        Geometry::MultiPolygon(ps) => {
            9 + ps.iter().map(|(e, h)| 9 + ring(e.0.len()) + h.iter().map(|r| ring(r.0.len())).sum::<u64>()).sum::<u64>()
        }
        Geometry::GeometryCollection(gs) => 9 + gs.iter().map(wkb_size).sum::<u64>(),
    }
}

/// GDAL's estimate of the bytes a feature adds to a file (binary formats store numbers in their
/// size, text formats in their decimal length).
fn feature_size(layer: &Layer, f: &wbvector::Feature, omitted: &[bool], wide: &[bool], binary: bool) -> u64 {
    let fields = layer.schema.fields();
    let mut n = 16 + 4 * fields.len() as u64;
    for (i, def) in fields.iter().enumerate() {
        if omitted.get(i).copied().unwrap_or(false) {
            continue;
        }
        let v = f.attributes.get(i);
        n += match def.field_type {
            FieldType::Integer | FieldType::Boolean => match (wide[i], binary) {
                (false, true) => 4,
                (false, false) => 11,
                (true, true) => 8,
                (true, false) => 21,
            },
            FieldType::Float => {
                if binary {
                    8
                } else {
                    15
                }
            }
            FieldType::Text | FieldType::Json => 4 + v.and_then(FieldValue::as_str).map_or(0, |s| s.len() as u64),
            FieldType::Blob => 4 + v.and_then(FieldValue::as_blob).map_or(0, |b| b.len() as u64),
            FieldType::Date => 4 + 11,
            FieldType::DateTime => 4 + 30,
        };
    }
    n += 4;
    if let Some(g) = &f.geometry {
        n += wkb_size(g);
    }
    n
}

/// GDAL's name of a geometry's type, as a partition key (`POINT`, `MULTIPOLYGONZ`).
fn geometry_key(g: &Geometry) -> String {
    let (z, m) = crate::vector::geometry_dims(g);
    format!("{}{}{}", g.geom_type().as_str().to_ascii_uppercase(), if z { "Z" } else { "" }, if m { "M" } else { "" })
}

/// One partitioning field: its index (`None` for the geometry) and encoded name.
struct Key {
    index: Option<usize>,
    name: String,
}

/// Partitions layers as `gdal vector partition` does, returning the parts to write with the
/// extension `extension`: features grouped by the values of the partitioning fields (in the
/// order they first appear), each group split into parts of at most `feature_limit` features
/// and `max_file_size` estimated bytes (`binary` tells how the format stores numbers), parts
/// numbered from 1. `driver` is the output format, for its size estimates and the automatic
/// omission of Hive-partitioned fields in GeoParquet.
pub fn partition(layers: &[Layer], o: &PartitionOptions, driver: VectorDriver) -> Result<Vec<Partition>> {
    if o.fields.is_empty() && o.feature_limit.is_none() && o.max_file_size.is_none() {
        return Err(Error::InvalidArgument("when no field is given, a feature limit and/or a maximum file size must be".into()));
    }
    if o.feature_limit == Some(0) {
        return Err(Error::InvalidArgument("the feature limit must be positive".into()));
    }
    if let Some(s) = o.max_file_size
        && s < 1024 * 1024
    {
        return Err(Error::InvalidArgument("the maximum file size should be at least one MB".into()));
    }
    let omit = o.omit_partitioned_fields || (driver == VectorDriver::Parquet && o.scheme == PartitionScheme::Hive);
    let binary = matches!(driver, VectorDriver::Parquet | VectorDriver::GPKG | VectorDriver::FlatGeobuf);
    // GDAL's estimate of the spatial index of GeoPackage and FlatGeobuf parts.
    let (per_feature, per_log2) = match driver {
        VectorDriver::GPKG => (36u64, 1u64),
        VectorDriver::FlatGeobuf => (1, 40),
        _ => (0, 0),
    };
    let extension = driver_extension(driver);
    let default_pattern = match (o.scheme, o.fields.is_empty()) {
        (PartitionScheme::Hive, _) => "part_%010d",
        (PartitionScheme::Flat, true) => "{LAYER_NAME}_%010d",
        (PartitionScheme::Flat, false) => "{LAYER_NAME}_{FIELD_VALUE}_%010d",
    };
    let pattern_text = o.pattern.clone().unwrap_or_else(|| default_pattern.to_string());
    parse_pattern(&pattern_text)?;

    let mut out = Vec::new();
    for layer in layers {
        let fields = layer.schema.fields();
        let mut keys = Vec::new();
        for name in &o.fields {
            match layer.schema.field_index(name).or_else(|| fields.iter().position(|f| f.name.eq_ignore_ascii_case(name))) {
                Some(i) => {
                    if !matches!(fields[i].field_type, FieldType::Text | FieldType::Integer | FieldType::Boolean) {
                        return Err(Error::Unsupported(format!(
                            "field '{name}' not valid for partitioning: only fields of type String, Integer or Integer64, or geometry fields, are accepted"
                        )));
                    }
                    keys.push(Key { index: Some(i), name: percent_encode(name) });
                }
                None if name == "OGR_GEOMETRY" => keys.push(Key { index: None, name: "OGR_GEOMETRY".into() }),
                None => return Err(Error::InvalidArgument(format!("cannot find field '{name}' in layer '{}'", layer.name))),
            }
        }
        // Whether each field is a 64-bit integer field, for the size estimates.
        let wide: Vec<bool> = (0..fields.len())
            .map(|i| crate::ops::vector_schema::ogr_type(layer, i).field_type == crate::ops::vector_schema::OgrFieldType::Integer64)
            .collect();
        let omitted: Vec<bool> = (0..fields.len()).map(|i| omit && keys.iter().any(|k| k.index == Some(i))).collect();
        let by_geometry = keys.iter().any(|k| k.index.is_none());

        // Features by key, keys in order of appearance.
        let mut order: Vec<String> = Vec::new();
        let mut groups: std::collections::HashMap<String, Vec<usize>> = Default::default();
        for (n, f) in layer.features.iter().enumerate() {
            let key = keys
                .iter()
                .map(|k| {
                    let value = match k.index {
                        None => f.geometry.as_ref().map_or_else(|| NULL_MARKER.to_string(), geometry_key),
                        Some(i) => match f.attributes.get(i) {
                            Some(FieldValue::Text(s)) => percent_encode(s),
                            Some(FieldValue::Integer(v)) => v.to_string(),
                            Some(FieldValue::Boolean(b)) => u8::from(*b).to_string(),
                            _ => NULL_MARKER.to_string(),
                        },
                    };
                    format!("{}={value}", k.name)
                })
                .collect::<Vec<_>>()
                .join("/");
            groups
                .entry(key.clone())
                .or_insert_with(|| {
                    order.push(key);
                    Vec::new()
                })
                .push(n);
        }

        let layer_dir = if layer.name.is_empty() { "%00".to_string() } else { percent_encode(&layer.name) };
        for key in order {
            let members = &groups[&key];
            let field_value = key
                .split('/')
                .map(|t| match t.split_once('=') {
                    Some((_, v)) if v == NULL_MARKER => "__NULL__".to_string(),
                    Some((_, v)) if !v.is_empty() => v.to_string(),
                    _ => "__EMPTY__".to_string(),
                })
                .collect::<Vec<_>>()
                .join("_");
            let substitute = |s: &str| crate::ops::concat::fill_template(s, &[("LAYER_NAME", &layer_dir), ("FIELD_VALUE", &field_value)]);
            let pattern = parse_pattern(&pattern_text)?;
            let pattern = Pattern { prefix: substitute(&pattern.prefix), suffix: substitute(&pattern.suffix), ..pattern };
            let dir = match o.scheme {
                PartitionScheme::Hive if key.is_empty() => layer_dir.clone(),
                PartitionScheme::Hive => format!("{layer_dir}/{key}"),
                PartitionScheme::Flat => String::new(),
            };
            // The schema of the parts: without the partitioned fields when omitted, with the
            // geometry type when partitioning on it.
            let mut template = Layer::new(layer.name.clone());
            template.geom_type = layer.geom_type;
            template.crs = layer.crs.clone();
            for (i, d) in fields.iter().enumerate() {
                if !omitted[i] {
                    template.add_field(d.clone());
                }
            }
            if by_geometry {
                template.geom_type = members.first().and_then(|&n| layer.features[n].geometry.as_ref()).map(|g| g.geom_type());
            }

            let mut counter = 1;
            let mut part = template.clone();
            let mut size = MIN_FILE_SIZE;
            let flush = |part: &mut Layer, counter: usize, out: &mut Vec<Partition>| {
                let file = format!("{}.{extension}", pattern.name(counter));
                let path = if dir.is_empty() { file } else { format!("{dir}/{file}") };
                out.push(Partition { path, layer: std::mem::replace(part, template.clone()) });
            };
            for &n in members {
                let f = &layer.features[n];
                let count = part.len() as u64;
                let index = if f.geometry.is_some() && per_feature > 0 && count > 0 {
                    (count * per_feature + (count as f64).log2().ceil() as u64) * per_log2
                } else {
                    0
                };
                let full = o.feature_limit.is_some_and(|l| part.len() >= l) || o.max_file_size.is_some_and(|m| size + index >= m);
                if full && !part.is_empty() {
                    flush(&mut part, counter, &mut out);
                    counter += 1;
                    size = MIN_FILE_SIZE;
                }
                let mut feature = f.clone();
                if omit {
                    feature.attributes = f.attributes.iter().enumerate().filter(|(i, _)| !omitted[*i]).map(|(_, v)| v.clone()).collect();
                }
                if o.max_file_size.is_some() {
                    size += feature_size(layer, f, &omitted, &wide, binary);
                }
                part.push(feature);
            }
            if !part.is_empty() {
                flush(&mut part, counter, &mut out);
            }
        }
    }
    // Two parts of one path would be written into one file.
    let mut seen = std::collections::HashSet::new();
    if let Some(p) = out.iter().find(|p| !seen.insert(p.path.clone())) {
        return Err(Error::InvalidArgument(format!(
            "several partitions would be written to '{}': give a pattern with {{FIELD_VALUE}} (and {{LAYER_NAME}})",
            p.path
        )));
    }
    Ok(out)
}

/// Refuses overwriting a partition directory that contains the input dataset.
/// Call before partitioning a path source; in-memory sources have no path to protect.
pub fn check_input_directory(input: &str, output: &str) -> Result<()> {
    if std::fs::canonicalize(input).ok().zip(std::fs::canonicalize(output).ok()).is_some_and(|(input, output)| input.starts_with(output)) {
        return Err(Error::InvalidArgument("partition output directory contains the input dataset".into()));
    }
    Ok(())
}

/// What to do with an existing output directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PartitionWriteMode {
    /// Fail when it exists.
    #[default]
    Create,
    /// Remove it first, when it looks like the output of a partitioning.
    Overwrite,
    /// Keep the parts it has, numbering new parts after them.
    Append,
}

/// Partitions layers into files under `dir` (see [`partition`]), written with `options` (whose
/// driver is the output format), and returns the paths written. An existing `dir` is refused,
/// replaced (`Overwrite`, only when it looks like the output of a partitioning: Hive
/// `layer/field=value` directories, or flat files only) or added to (`Append`: new parts are
/// numbered after the existing ones; GDAL also fills the last existing part while it has
/// room). The input layers are materialized: callers must ensure source files are not
/// inside an overwrite target; path-based callers can use [`check_input_directory`].
#[cfg(not(target_family = "wasm"))]
pub fn partition_to_dir(
    layers: &[Layer],
    dir: &str,
    o: &PartitionOptions,
    options: &crate::VectorWriteOptions,
    mode: PartitionWriteMode,
) -> Result<Vec<String>> {
    use std::path::Path;
    let driver = options.driver.ok_or_else(|| Error::InvalidArgument("partitioning needs an output format".into()))?;
    driver.ensure_built()?;
    if !driver.info().write {
        return Err(Error::Unsupported(format!("the {} driver cannot write", driver.name())));
    }
    let parts = partition(layers, o, driver)?;
    let root = Path::new(dir);
    let new_root = !root.exists();
    let marker = root.join(".terrio-partition");
    let signature = format!("terrio partition {:?} {:?}\n", o.scheme, driver);
    if root.exists() {
        match mode {
            PartitionWriteMode::Create => {
                return Err(Error::InvalidArgument(format!("'{dir}' already exists: overwrite or append to it")));
            }
            PartitionWriteMode::Overwrite => {
                if std::fs::read_to_string(&marker).ok().as_deref() != Some(signature.as_str())
                    || !looks_partitioned(root, o.scheme, driver)?
                {
                    return Err(Error::InvalidArgument(format!(
                        "refusing to remove '{dir}': it does not look like a partitioned dataset; remove it yourself if you are sure"
                    )));
                }
                std::fs::remove_dir_all(root)?;
            }
            PartitionWriteMode::Append => {}
        }
    }
    let mut written = Vec::with_capacity(parts.len());
    // In append mode, part numbers continue those in the directory.
    let mut appended_keys: std::collections::HashMap<String, usize> = Default::default();
    for p in parts {
        let mut path = root.join(&p.path);
        if mode == PartitionWriteMode::Append {
            path = append_target(&path, &mut appended_keys);
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let s = path.to_string_lossy().into_owned();
        crate::ops::vector_filter::write_vector_with_mode(
            std::slice::from_ref(&p.layer),
            &s,
            options,
            crate::ops::vector_filter::WriteMode::Append,
        )?;
        written.push(s);
    }
    if new_root || mode == PartitionWriteMode::Overwrite {
        std::fs::write(marker, signature)?;
    }
    Ok(written)
}

/// Where a part goes when appending: numbered after the highest-numbered existing part of its
/// directory and name pattern.
#[cfg(not(target_family = "wasm"))]
fn append_target(path: &std::path::Path, seen: &mut std::collections::HashMap<String, usize>) -> std::path::PathBuf {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else { return path.to_path_buf() };
    let (stem, ext) = name.rsplit_once('.').unwrap_or((name, ""));
    // The part number: the last run of digits (possibly padded with spaces).
    let Some(end) = stem.rfind(|c: char| c.is_ascii_digit()) else { return path.to_path_buf() };
    let start = stem[..=end].rfind(|c: char| !c.is_ascii_digit() && c != ' ').map_or(0, |i| i + 1);
    let (prefix, suffix) = (&stem[..start], &stem[end + 1..]);
    let width = end + 1 - start;
    let number: usize = stem[start..=end].trim().parse().unwrap_or(1);
    let key = format!("{}|{prefix}|{suffix}", dir.display());
    let base = *seen.entry(key).or_insert_with(|| {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries
                    .flatten()
                    .filter_map(|e| {
                        let n = e.file_name().to_string_lossy().into_owned();
                        let s = n.rsplit_once('.').map_or(n.as_str(), |(s, _)| s).to_string();
                        s.strip_prefix(prefix).and_then(|r| r.strip_suffix(suffix)).and_then(|mid| mid.trim().parse::<usize>().ok())
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    });
    let pad = if stem[start..=end].starts_with('0') { '0' } else { ' ' };
    let digits = (base + number).to_string();
    let fill: String = std::iter::repeat_n(pad, width.saturating_sub(digits.len())).collect();
    let dot = if ext.is_empty() { "" } else { "." };
    dir.join(format!("{prefix}{fill}{digits}{suffix}{dot}{ext}"))
}

/// Whether a directory looks like the output of a partitioning, as GDAL checks before
/// replacing it: empty, or (Hive) holding a `layer/field=value` directory, or (flat) no
/// directories.
#[cfg(not(target_family = "wasm"))]
fn looks_partitioned(root: &std::path::Path, scheme: PartitionScheme, driver: VectorDriver) -> Result<bool> {
    fn directory(path: &std::path::Path, scheme: PartitionScheme, driver: VectorDriver, depth: usize) -> Result<bool> {
        if depth > 256 || std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Ok(false);
        }
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if depth == 0 && entry.file_name() == ".terrio-partition" && kind.is_file() {
                continue;
            }
            if kind.is_dir() && scheme == PartitionScheme::Hive {
                // The first directory is the layer; subsequent ones are field=value.
                if (depth > 0 && !entry.file_name().to_string_lossy().contains('='))
                    || !directory(&entry.path(), scheme, driver, depth + 1)?
                {
                    return Ok(false);
                }
            } else if kind.is_file() && (scheme == PartitionScheme::Flat || depth >= 1) {
                let path = entry.path();
                let accepted = path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| {
                    ext.eq_ignore_ascii_case(driver_extension(driver))
                        || (driver == VectorDriver::Shapefile
                            && ["shp", "shx", "dbf", "prj", "cpg"].iter().any(|e| ext.eq_ignore_ascii_case(e)))
                });
                if !accepted {
                    return Ok(false);
                }
            } else {
                return Ok(false);
            }
        }
        Ok(true)
    }
    directory(root, scheme, driver, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{Feature, FieldDef};

    fn layer() -> Layer {
        let mut l = Layer::new("my layer").with_epsg(4326);
        l.add_field(FieldDef::new("state", FieldType::Text));
        l.add_field(FieldDef::new("n", FieldType::Integer));
        for (i, s) in ["TN", "New York", "TN", "", "TN"].iter().enumerate() {
            let v = if s.is_empty() { FieldValue::Null } else { FieldValue::Text((*s).into()) };
            l.push(Feature {
                fid: i as u64,
                geometry: Some(Geometry::point(i as f64, 0.0)),
                attributes: vec![v, FieldValue::Integer(i as i64 % 2)],
            });
        }
        l
    }

    #[test]
    fn hive_partitions_with_limits() {
        let o = PartitionOptions { fields: vec!["state".into()], feature_limit: Some(2), ..Default::default() };
        let parts = partition(&[layer()], &o, VectorDriver::GPKG).unwrap();
        let paths: Vec<(&str, usize)> = parts.iter().map(|p| (p.path.as_str(), p.layer.len())).collect();
        assert_eq!(
            paths,
            [
                ("my%20layer/state=TN/part_0000000001.gpkg", 2),
                ("my%20layer/state=TN/part_0000000002.gpkg", 1),
                ("my%20layer/state=New%20York/part_0000000001.gpkg", 1),
                ("my%20layer/state=__HIVE_DEFAULT_PARTITION__/part_0000000001.gpkg", 1),
            ]
        );
        assert_eq!(parts[0].layer.schema.len(), 2);
    }

    #[test]
    fn flat_partitions_and_omitted_fields() {
        let o = PartitionOptions {
            fields: vec!["state".into(), "n".into()],
            scheme: PartitionScheme::Flat,
            omit_partitioned_fields: true,
            ..Default::default()
        };
        let parts = partition(&[layer()], &o, VectorDriver::GeoJSON).unwrap();
        assert_eq!(parts[0].path, "my%20layer_TN_0_0000000001.json");
        assert_eq!(parts[0].layer.len(), 3);
        assert_eq!(parts.last().unwrap().path, "my%20layer___NULL___1_0000000001.json");
        assert_eq!(parts[0].layer.schema.len(), 0);
        let o = PartitionOptions { pattern: Some("p%3d_{FIELD_VALUE}".into()), ..o };
        assert_eq!(partition(&[layer()], &o, VectorDriver::GeoJSON).unwrap()[0].path, "p  1_TN_0.json");
    }

    #[test]
    fn validates_options() {
        let base = PartitionOptions { fields: vec!["state".into()], ..Default::default() };
        assert!(partition(&[layer()], &PartitionOptions::default(), VectorDriver::GPKG).is_err());
        assert!(partition(&[layer()], &PartitionOptions { pattern: Some("part".into()), ..base.clone() }, VectorDriver::GPKG).is_err());
        assert!(partition(&[layer()], &PartitionOptions { max_file_size: Some(1000), ..base.clone() }, VectorDriver::GPKG).is_err());
        for bad in ["../x_%d", "a/b_%d", "a\\b_%d"] {
            assert!(
                partition(&[layer()], &PartitionOptions { pattern: Some(bad.into()), ..base.clone() }, VectorDriver::GPKG).is_err(),
                "{bad}"
            );
        }
        let flat = PartitionOptions { scheme: PartitionScheme::Flat, pattern: Some("part_%d".into()), ..base.clone() };
        assert!(partition(&[layer()], &flat, VectorDriver::GPKG).unwrap_err().to_string().contains("several partitions"));
        assert!(partition(&[layer()], &PartitionOptions { fields: vec!["missing".into()], ..base }, VectorDriver::GPKG).is_err());
        let parts = partition(&[layer()], &PartitionOptions { feature_limit: Some(4), ..Default::default() }, VectorDriver::GPKG).unwrap();
        assert_eq!(
            parts.iter().map(|p| p.path.as_str()).collect::<Vec<_>>(),
            ["my%20layer/part_0000000001.gpkg", "my%20layer/part_0000000002.gpkg"]
        );
    }

    #[test]
    fn partitions_by_geometry_type() {
        let mut l = layer();
        l.features[1].geometry = Some(Geometry::LineString(vec![wbvector::Coord::xy(0.0, 0.0), wbvector::Coord::xy(1.0, 1.0)]));
        let o = PartitionOptions { fields: vec!["OGR_GEOMETRY".into()], ..Default::default() };
        let parts = partition(&[l], &o, VectorDriver::GPKG).unwrap();
        assert_eq!(parts[1].path, "my%20layer/OGR_GEOMETRY=LINESTRING/part_0000000001.gpkg");
        assert_eq!(parts[1].layer.geom_type, Some(wbvector::GeometryType::LineString));
    }
    #[test]
    fn dot_layer_names_stay_in_output_root() {
        for name in ["", ".", ".."] {
            let mut l = layer();
            l.name = name.into();
            let parts = partition(&[l], &PartitionOptions { feature_limit: Some(2), ..Default::default() }, VectorDriver::GPKG).unwrap();
            assert!(parts.iter().all(|p| !std::path::Path::new(&p.path).is_absolute()
                && !std::path::Path::new(&p.path).components().any(|c| c == std::path::Component::ParentDir)));
        }
    }

    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn hive_overwrite_refuses_unrelated_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("parts");
        let write = crate::VectorWriteOptions { driver: Some(VectorDriver::GPKG), ..Default::default() };
        let options = PartitionOptions { fields: vec!["state".into()], ..Default::default() };
        let written = partition_to_dir(&[layer()], root.to_str().unwrap(), &options, &write, PartitionWriteMode::Create).unwrap();
        let notes = std::path::Path::new(&written[0]).parent().unwrap().join("notes.txt");
        std::fs::write(&notes, "keep me").unwrap();
        assert!(partition_to_dir(&[layer()], root.to_str().unwrap(), &options, &write, PartitionWriteMode::Overwrite).is_err());
        assert_eq!(std::fs::read_to_string(notes).unwrap(), "keep me");
    }

    #[test]
    fn limit_only_hive_output_can_be_overwritten() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("parts");
        let options = PartitionOptions { feature_limit: Some(1), ..Default::default() };
        let write = crate::VectorWriteOptions { driver: Some(VectorDriver::GPKG), ..Default::default() };
        partition_to_dir(&[layer()], root.to_str().unwrap(), &options, &write, PartitionWriteMode::Create).unwrap();
        partition_to_dir(&[layer()], root.to_str().unwrap(), &options, &write, PartitionWriteMode::Overwrite).unwrap();
    }

    #[test]
    fn flat_overwrite_preserves_unrelated_files_and_invalid_options() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("parts");
        std::fs::create_dir(&root).unwrap();
        let notes = root.join("notes.txt");
        std::fs::write(&notes, "keep me").unwrap();
        let write = crate::VectorWriteOptions { driver: Some(VectorDriver::GPKG), ..Default::default() };
        let options = PartitionOptions { scheme: PartitionScheme::Flat, feature_limit: Some(2), ..Default::default() };
        assert!(partition_to_dir(&[layer()], root.to_str().unwrap(), &options, &write, PartitionWriteMode::Overwrite).is_err());
        assert_eq!(std::fs::read_to_string(notes).unwrap(), "keep me");
    }
}

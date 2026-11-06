//! Geometry and topology operations, raster footprints, point sampling and raster analysis.

use terrio::formats::VectorDriver;
use terrio::ops::footprint::FootprintOptions;
use terrio::ops::sample::{SampleInput, SampleOptions};
use terrio::ops::vector_filter::{WriteMode, write_vector_with_mode};
use terrio::{Error, Layer, Result};

#[derive(clap::Args)]
pub struct VectorInput {
    /// The input vector dataset.
    pub(crate) input: String,
    /// Read only this layer (repeat to choose multiple layers). By default, every layer.
    #[arg(long = "layer", short = 'l')]
    pub(crate) layers: Vec<String>,
}

impl VectorInput {
    pub(crate) fn open(&self) -> Result<Vec<Layer>> {
        Ok(super::quiet(|| {
            terrio::open_vector(&self.input, &terrio::VectorOpenOptions { layers: self.layers.clone(), ..Default::default() })
        })?
        .into_resolved_layers())
    }
}

#[derive(clap::Args)]
pub struct VectorOutput {
    /// The output vector dataset.
    pub(crate) output: String,
    /// The output format's short name. By default, chosen from the output's extension.
    #[arg(short = 'f', long = "format")]
    pub(crate) format: Option<String>,
    /// The output layer name (requires a single input layer).
    #[arg(long, visible_alias = "output-layer", value_name = "NAME")]
    pub(crate) nln: Option<String>,
    /// Append features to layers of the same name in an existing GeoPackage.
    #[arg(long, conflicts_with = "overwrite")]
    pub(crate) append: bool,
    /// Replace layers of the same name in an existing output, keeping other GeoPackage layers.
    #[arg(long)]
    pub(crate) overwrite: bool,
}

impl VectorOutput {
    /// An output without a layer name.
    pub(crate) fn new(output: &str, format: Option<String>, append: bool, overwrite: bool) -> Self {
        Self { output: output.to_string(), format, nln: None, append, overwrite }
    }

    /// The output driver: `--format`, else the one of the output's extension.
    pub(crate) fn driver(&self) -> Result<Option<VectorDriver>> {
        match self.format.as_deref() {
            Some(f) => VectorDriver::from_name(f).map(Some).ok_or_else(|| Error::InvalidArgument(format!("unknown vector format '{f}'"))),
            None => Ok(VectorDriver::for_extension(&self.output)),
        }
    }

    pub(crate) fn write(&self, layers: Vec<Layer>) -> Result<()> {
        self.write_with_geometry_types(layers, false)
    }

    fn write_with_geometry_types(&self, mut layers: Vec<Layer>, preserve: bool) -> Result<()> {
        if let Some(name) = &self.nln {
            if layers.len() != 1 {
                return Err(Error::InvalidArgument("--nln requires a single input layer; choose one with --layer".into()));
            }
            layers[0].name = name.clone();
        }
        let driver = self
            .format
            .as_deref()
            .map(|f| VectorDriver::from_name(f).ok_or_else(|| Error::InvalidArgument(format!("unknown vector format '{f}'"))))
            .transpose()?;
        let mode = if self.append {
            WriteMode::Append
        } else if self.overwrite {
            WriteMode::Overwrite
        } else {
            WriteMode::Create
        };
        let preserve_declared_geometry_type =
            preserve && driver.or_else(|| VectorDriver::for_extension(&self.output)) == Some(VectorDriver::GPKG);
        write_vector_with_mode(
            &layers,
            &self.output,
            &terrio::VectorWriteOptions { driver, preserve_declared_geometry_type, ..Default::default() },
            mode,
        )
    }
}

#[derive(clap::Args)]
pub struct GeometryArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
}

pub fn centroid(a: GeometryArgs) -> Result<()> {
    let layers = a.input.open()?.iter().map(terrio::ops::geometry::centroids).collect::<Result<Vec<_>>>()?;
    a.output.write(layers)
}

pub fn singlepart(a: GeometryArgs) -> Result<()> {
    let layers = a.input.open()?.iter().map(terrio::ops::geometry::singleparts).collect();
    a.output.write(layers)
}

pub fn vertices(a: GeometryArgs) -> Result<()> {
    let layers = a.input.open()?.iter().map(terrio::ops::geometry::vertices).collect();
    a.output.write(layers)
}

#[derive(clap::Args)]
pub struct BufferArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Distance in CRS units (degrees in a geographic CRS). Negative values erode polygons.
    #[arg(short = 'd', long, allow_hyphen_values = true)]
    distance: f64,
    /// Segments per quadrant of round caps and joins, from 1 to 4096.
    #[arg(long = "quad-segs", default_value_t = 30)]
    quadrant_segments: usize,
}

pub fn buffer(a: BufferArgs) -> Result<()> {
    let options = terrio::ops::topology::BufferOptions { distance: a.distance, quadrant_segments: a.quadrant_segments };
    let layers = a.input.open()?.iter().map(|l| terrio::ops::topology::buffer(l, &options)).collect::<Result<_>>()?;
    a.output.write(layers)
}

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum OverlayOperation {
    Intersection,
    Union,
    Difference,
    #[value(alias = "xor")]
    SymmetricDifference,
}

#[derive(clap::Args)]
pub struct OverlayArgs {
    #[command(flatten)]
    input: VectorInput,
    /// Right-hand polygon dataset, in the input CRS.
    overlay: String,
    #[command(flatten)]
    output: VectorOutput,
    /// Polygon overlay operation. Touching lines and points are omitted.
    #[arg(long, value_enum, default_value_t = OverlayOperation::Intersection)]
    operation: OverlayOperation,
    /// Select a single layer of the right-hand dataset (required when it has multiple layers).
    #[arg(long = "overlay-layer")]
    overlay_layer: Option<String>,
}

pub fn overlay(a: OverlayArgs) -> Result<()> {
    let right = super::quiet(|| {
        terrio::open_vector(&a.overlay, &terrio::VectorOpenOptions { layers: a.overlay_layer.into_iter().collect(), ..Default::default() })
    })?;
    if right.layers.len() != 1 {
        return Err(Error::InvalidArgument("overlay needs one right-hand layer; choose it with --overlay-layer".into()));
    }
    use terrio::ops::topology::OverlayOperation as Op;
    let op = match a.operation {
        OverlayOperation::Intersection => Op::Intersection,
        OverlayOperation::Union => Op::Union,
        OverlayOperation::Difference => Op::Difference,
        OverlayOperation::SymmetricDifference => Op::SymmetricDifference,
    };
    let layers = a.input.open()?.iter().map(|l| terrio::ops::topology::overlay(l, &right.layers[0], op)).collect::<Result<_>>()?;
    a.output.write(layers)
}

#[derive(clap::Args)]
pub struct DissolveArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Group by these fields (repeat or comma-separate). By default, dissolve the whole layer.
    #[arg(long = "field", visible_alias = "by", value_delimiter = ',')]
    fields: Vec<String>,
}

pub fn dissolve(a: DissolveArgs) -> Result<()> {
    let layers = a.input.open()?.iter().map(|l| terrio::ops::topology::dissolve(l, &a.fields)).collect::<Result<_>>()?;
    a.output.write(layers)
}

pub fn validity(a: GeometryArgs) -> Result<()> {
    a.output.write(a.input.open()?.iter().map(terrio::ops::topology::validity).collect())
}

pub fn make_valid(a: GeometryArgs) -> Result<()> {
    let layers = a.input.open()?.iter().map(terrio::ops::topology::make_valid).collect::<Result<_>>()?;
    a.output.write(layers)
}

#[derive(clap::Args)]
pub struct CleanArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Optional XY snap grid spacing in CRS units. Zero disables snapping. First duplicate wins.
    #[arg(long = "snap-grid", default_value_t = 0.0, allow_hyphen_values = true)]
    snap_grid: f64,
}

pub fn clean(a: CleanArgs) -> Result<()> {
    let options = terrio::ops::topology::CleanOptions { snap_grid: a.snap_grid };
    let layers = a.input.open()?.iter().map(|l| terrio::ops::topology::clean(l, &options)).collect::<Result<_>>()?;
    a.output.write(layers)
}

#[derive(clap::Args)]
pub struct FootprintArgs {
    /// The input raster. Its finite, non-nodata pixels define the footprint.
    input: String,
    #[command(flatten)]
    output: VectorOutput,
    /// Bands from 1, comma-separated. By default, every band.
    #[arg(short = 'b', long = "band", value_delimiter = ',')]
    bands: Vec<usize>,
    /// Require every selected band to be valid. By default, at least one must be valid.
    #[arg(long)]
    intersection: bool,
    /// Include only finite, nonzero, non-nodata pixels of this mask's first band, on the input grid.
    #[arg(long, value_name = "RASTER")]
    mask: Option<String>,
    /// Emit one polygon feature per connected region, preserving holes and attributes.
    #[arg(long)]
    singlepart: bool,
    /// Add a text attribute to every feature (repeat for multiple fields).
    #[arg(long = "attribute", value_name = "NAME=VALUE", value_parser = parse_footprint_attribute)]
    attributes: Vec<(String, String)>,
    /// Field holding the input path (remote URL credentials, query and fragment omitted).
    #[arg(long, default_value = "location", conflicts_with = "no_location")]
    location_field: String,
    /// Omit the default source-path attribute.
    #[arg(long)]
    no_location: bool,
}

fn parse_footprint_attribute(value: &str) -> std::result::Result<(String, String), String> {
    let (name, value) = value.split_once('=').ok_or("expected NAME=VALUE")?;
    if name.trim().is_empty() {
        return Err("attribute name must not be empty".into());
    }
    Ok((name.to_owned(), value.to_owned()))
}

// Preserve local filenames verbatim; remote source identifiers must not persist authentication.
fn footprint_location(input: &str) -> String {
    let Some((scheme, rest)) = input.split_once("://") else { return input.to_owned() };
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(authority, _)| (authority, &rest[authority.len()..]));
    let host = authority.rsplit_once('@').map_or(authority, |(_, host)| host);
    format!("{scheme}://{host}{path}")
}

pub fn footprint(a: FootprintArgs) -> Result<()> {
    let raster = super::quiet(|| terrio::open_raster(&a.input))?;
    let mask = a.mask.as_deref().map(|p| super::quiet(|| terrio::open_raster(p))).transpose()?;
    let mut attributes = a.attributes;
    if !a.no_location {
        attributes.insert(0, (a.location_field, footprint_location(&a.input)));
    }
    let options = FootprintOptions { bands: a.bands, intersection: a.intersection, mask, singlepart: a.singlepart, attributes };
    a.output.write(vec![terrio::ops::footprint::footprint(&raster, &options)?])
}

#[derive(clap::Args)]
pub struct SampleArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// An input raster (repeat for multiple rasters). `sample` loads each raster into memory;
    /// `zonal-stats` reads them lazily, by block, and also takes `@FILE` lists of paths.
    #[arg(long = "raster", value_name = "PATH")]
    rasters: Vec<String>,
    /// Bands from every raster, from 1, comma-separated. By default, band 1.
    #[arg(short = 'b', long = "band", value_delimiter = ',', default_value = "1")]
    bands: Vec<usize>,
    /// Output field prefix, one per --raster in the same order. By default, its file stem.
    /// Base field names are PREFIX_bN; zonal statistics append the statistic name.
    #[arg(long = "prefix", value_name = "NAME")]
    prefixes: Vec<String>,
    /// Reproject the output points or polygons after analysis (requires an input CRS).
    #[arg(long = "t-srs", value_name = "CRS")]
    t_srs: Option<String>,
    /// Round newly computed values to this many decimal places (0–15); counts stay integers.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u8).range(0..=15))]
    decimal_places: Option<u8>,
}

pub fn sample(a: SampleArgs) -> Result<()> {
    if a.rasters.is_empty() {
        return Err(Error::InvalidArgument("give at least one --raster".into()));
    }
    let options =
        SampleOptions { dst_srs: a.t_srs.as_deref().map(terrio::crs::SpatialRef::parse).transpose()?, decimal_places: a.decimal_places };
    let (rasters, prefixes) = sample_sources(&a)?;
    let mut inputs = Vec::new();
    for (i, raster) in rasters.iter().enumerate() {
        for &band in &a.bands {
            inputs.push(SampleInput { field_name: format!("{}_b{band}", prefixes[i]), raster, band });
        }
    }
    let layers =
        a.input.open()?.iter().map(|l| terrio::ops::sample::sample_with_options(l, &inputs, &options)).collect::<Result<Vec<_>>>()?;
    a.output.write(layers)
}

fn sample_sources(a: &SampleArgs) -> Result<(Vec<terrio::RasterDataset>, Vec<String>)> {
    let prefixes = sample_prefixes(&a.prefixes, &a.rasters)?;
    let rasters = a.rasters.iter().map(|p| super::quiet(|| terrio::open_raster(p))).collect::<Result<Vec<_>>>()?;
    Ok((rasters, prefixes))
}

/// Field prefixes for each `--raster`: its `--prefix`, else its file stem.
fn sample_prefixes(given: &[String], rasters: &[String]) -> Result<Vec<String>> {
    if !given.is_empty() && given.len() != rasters.len() {
        return Err(Error::InvalidArgument(format!(
            "give one --prefix per raster ({} prefixes for {} rasters, @FILE lists counted by their paths), or omit all prefixes",
            given.len(),
            rasters.len()
        )));
    }
    if given.iter().any(|p| p.trim().is_empty()) {
        return Err(Error::InvalidArgument("raster field prefixes cannot be empty".into()));
    }
    let mut prefixes = Vec::new();
    for (i, path) in rasters.iter().enumerate() {
        let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("raster");
        let prefix = given.get(i).map_or(stem, String::as_str);
        prefixes.push(prefix.to_string());
    }
    Ok(prefixes)
}

#[derive(clap::Args)]
pub struct ZonalArgs {
    #[command(flatten)]
    sources: SampleArgs,
    /// Statistics, comma-separated: count,min,max,mean,sum,stddev,variance,cv,median,
    /// quantile_P (P from 0 to 100, e.g. quantile_25),mode (alias majority),minority,variety,
    /// weighted_mean,weighted_sum,weighted_stddev. Uses population standard deviation.
    #[arg(long, value_delimiter = ',', default_value = "count,min,max,mean,sum,stddev")]
    stats: Vec<String>,
    /// Include every pixel touched by each polygon, instead of pixels whose centres are inside.
    #[arg(long)]
    all_touched: bool,
    /// Weight pixels by the polygon/cell intersection fraction. Incompatible with --all-touched.
    #[arg(long, conflicts_with = "all_touched")]
    fractional: bool,
    /// Weighting raster, aligned to every input grid (for weighted_mean, weighted_sum, weighted_stddev).
    #[arg(long)]
    weights: Option<String>,
    /// Weighting band, numbered from 1.
    #[arg(long, default_value_t = 1)]
    weight_band: usize,
    /// A CSV table of labelled inputs, with a header: `path` (required), and optional `time`
    /// (such as 2024-07-01), `label` (such as B04) and `band` (a band number; by default each
    /// --band). Table rows naming the same path, or a --raster path, share its read. Labelled rows name
    /// fields TIME_LABEL_STATISTIC (TIME_LABEL_bN_STATISTIC when a row without `band` takes
    /// several --band); others FILE_STEM_bN_STATISTIC. Quote fields containing commas.
    #[arg(long, value_name = "FILE")]
    inputs: Option<String>,
}

/// The most statistic columns `zonal-stats` adds, in any format: FlatGeobuf's column limit.
const MAX_COLUMNS: usize = u16::MAX as usize;

/// SQLite's default column limit, which GeoPackage tables share (feature ID and geometry included).
const GPKG_MAX_COLUMNS: usize = 2000;

/// One row of `--inputs`.
struct InputRow {
    path: String,
    time: Option<String>,
    label: Option<String>,
    band: Option<usize>,
}

/// The records of a CSV text, with RFC 4180 quoting: `"a,b"`, `""` for a quote, and line breaks
/// inside quotes. Unquoted fields are trimmed, quoted ones kept as written; blank lines are
/// skipped. An unterminated quote is an error.
fn csv_records(text: &str) -> std::result::Result<Vec<Vec<String>>, String> {
    let (mut records, mut record) = (Vec::new(), Vec::new());
    let (mut field, mut quoted, mut was_quoted) = (String::new(), false, false);
    let end_field = |field: &mut String, was_quoted: &mut bool| {
        let f = std::mem::take(field);
        if std::mem::take(was_quoted) { f } else { f.trim().to_string() }
    };
    let end_record = |record: &mut Vec<String>, records: &mut Vec<Vec<String>>| {
        let r = std::mem::take(record);
        if r.iter().any(|f| !f.is_empty()) {
            records.push(r);
        }
    };
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                field.push('"');
                chars.next();
            }
            '"' if quoted => quoted = false,
            '"' if !was_quoted && field.trim().is_empty() => {
                field.clear();
                (quoted, was_quoted) = (true, true);
            }
            ',' if !quoted => record.push(end_field(&mut field, &mut was_quoted)),
            '\n' | '\r' if !quoted => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                record.push(end_field(&mut field, &mut was_quoted));
                end_record(&mut record, &mut records);
            }
            // After a closing quote, only whitespace before the separator.
            c if was_quoted && !quoted && c.is_whitespace() => {}
            c if was_quoted && !quoted => return Err(format!("text '{c}' after a closing quote")),
            c => field.push(c),
        }
    }
    if quoted {
        return Err("a quoted field is not closed".into());
    }
    record.push(end_field(&mut field, &mut was_quoted));
    end_record(&mut record, &mut records);
    Ok(records)
}

/// The rows of an `--inputs` CSV table.
fn read_inputs(file: &str) -> Result<Vec<InputRow>> {
    let text = super::read_list_text(file, "--inputs tables")?;
    let bad = |why: String| Error::InvalidArgument(format!("--inputs {file}: {why}"));
    let mut records = csv_records(&text).map_err(bad)?.into_iter();
    let header: Vec<String> = records.next().ok_or_else(|| bad("empty table".into()))?.iter().map(|h| h.to_ascii_lowercase()).collect();
    if let Some(unknown) = header.iter().find(|h| !matches!(h.as_str(), "path" | "time" | "label" | "band")) {
        return Err(bad(format!("unknown column '{unknown}' (expected path, time, label, band)")));
    }
    if let Some(twice) = header.iter().enumerate().find_map(|(i, h)| header[..i].contains(h).then_some(h)) {
        return Err(bad(format!("column '{twice}' appears twice in the header")));
    }
    let column = |name: &str| header.iter().position(|h| h == name);
    let path = column("path").ok_or_else(|| bad("the header has no 'path' column".into()))?;
    let (time, label, band) = (column("time"), column("label"), column("band"));
    let mut rows = Vec::new();
    for (n, fields) in records.enumerate() {
        if fields.len() > header.len() {
            return Err(bad(format!(
                "row {} has {} fields, the header {} (quote fields containing commas)",
                n + 1,
                fields.len(),
                header.len()
            )));
        }
        let get = |i: Option<usize>| i.and_then(|i| fields.get(i)).filter(|v| !v.is_empty()).cloned();
        let row_path = get(Some(path)).ok_or_else(|| bad(format!("row {} has no path", n + 1)))?;
        let row_band = match get(band) {
            Some(b) => Some(
                b.parse::<usize>()
                    .ok()
                    .filter(|&b| b > 0)
                    .ok_or_else(|| bad(format!("row {}: band '{b}' is not a band number from 1", n + 1)))?,
            ),
            None => None,
        };
        rows.push(InputRow { path: row_path, time: get(time), label: get(label), band: row_band });
    }
    if rows.is_empty() {
        return Err(bad("the table lists no inputs".into()));
    }
    Ok(rows)
}

pub fn zonal(a: ZonalArgs) -> Result<()> {
    use terrio::ops::zonal::{Statistic, ZonalBand, ZonalOptions, ZonalRaster, ZonalSource, ZonalWeight, zonal_stats};
    let stats: Vec<_> = a.stats.iter().map(|s| Statistic::parse(s)).collect::<Result<_>>()?;
    let weights = a
        .weights
        .as_deref()
        .filter(|_| stats.iter().any(|s| s.uses_weights()))
        .map(|p| super::quiet(|| terrio::open_raster_lazy(p)))
        .transpose()?;
    let options = ZonalOptions {
        fractional: a.fractional,
        weights: weights.map(|raster| ZonalWeight::new(raster, a.weight_band)),
        stats,
        all_touched: a.all_touched,
        dst_srs: a.sources.t_srs.as_deref().map(terrio::crs::SpatialRef::parse).transpose()?,
        decimal_places: a.sources.decimal_places,
    };
    let rasters = super::expand_file_lists(&a.sources.rasters)?;
    if rasters.is_empty() && !a.sources.prefixes.is_empty() {
        return Err(Error::InvalidArgument(
            "--prefix names --raster fields; give it with --raster (--inputs rows are named TIME_LABEL or FILE_STEM_bN)".into(),
        ));
    }
    let prefixes = sample_prefixes(&a.sources.prefixes, &rasters)?;
    let rows = a.inputs.as_deref().map(read_inputs).transpose()?.unwrap_or_default();
    if rasters.is_empty() && rows.is_empty() {
        return Err(Error::InvalidArgument("give at least one --raster or an --inputs table".into()));
    }
    // (path, bands) in first-appearance order: --raster paths, then table rows by path.
    let mut plan: Vec<(String, Vec<ZonalBand>)> = rasters
        .iter()
        .zip(&prefixes)
        .map(|(path, prefix)| {
            (path.clone(), a.sources.bands.iter().map(|&band| ZonalBand::new(band, format!("{prefix}_b{band}"))).collect())
        })
        .collect();
    let mut by_path: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (i, (path, _)) in plan.iter().enumerate() {
        by_path.entry(path.clone()).or_insert(i);
    }
    for row in rows {
        let bands = row.band.map_or_else(|| a.sources.bands.clone(), |b| vec![b]);
        let stem = std::path::Path::new(&row.path).file_stem().and_then(|s| s.to_str()).unwrap_or("raster").to_string();
        let labelled =
            terrio::ops::zonal::labelled_bands(&bands, row.time.as_deref(), row.label.as_deref(), |band| format!("{stem}_b{band}"));
        match by_path.get(&row.path) {
            Some(&i) => plan[i].1.extend(labelled),
            None => {
                by_path.insert(row.path.clone(), plan.len());
                plan.push((row.path, labelled));
            }
        }
    }
    let columns: usize = plan.iter().map(|(_, b)| b.len()).sum::<usize>() * options.stats.len();
    // Every feature holds a value per column: a bound for every format (FlatGeobuf's own).
    if columns > MAX_COLUMNS {
        return Err(Error::InvalidArgument(format!(
            "zonal statistics would add {columns} columns; at most {MAX_COLUMNS} are written: request fewer statistics or inputs"
        )));
    }
    let layers = a.sources.input.open()?;
    if a.sources.output.driver()? == Some(VectorDriver::GPKG)
        && let Some(l) = layers.iter().find(|l| l.schema.len() + columns + 2 > GPKG_MAX_COLUMNS)
    {
        return Err(Error::InvalidArgument(format!(
            "zonal statistics would add {columns} columns to {} (layer '{}'); GeoPackage allows {GPKG_MAX_COLUMNS}: request fewer statistics or inputs, or write FlatGeobuf or Parquet",
            a.sources.output.output, l.name
        )));
    }
    // Paths: each raster is open only while it is read, so lists of thousands of files stay
    // within the open-file limit.
    let mut sources: Vec<ZonalSource> =
        plan.into_iter().map(|(path, bands)| ZonalSource { raster: ZonalRaster::Path(path), bands }).collect();
    let layers = super::quiet(|| layers.iter().map(|l| zonal_stats(l, &mut sources, &options)).collect::<Result<Vec<_>>>())?;
    a.sources.output.write(layers)
}

#[derive(clap::Args)]
pub struct SqlArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Read-only SQL subset: expressions, DISTINCT, grouped summaries, attribute LEFT JOIN and spatial functions.
    #[arg(short = 'q', long = "query", alias = "sql")]
    query: String,
}

pub fn sql(a: SqlArgs) -> Result<()> {
    let result = terrio::ops::sql::query(&a.input.open()?, &a.query)?;
    a.output.write(vec![result])
}

#[derive(clap::Args)]
pub struct ContourArgs {
    /// The input elevation raster.
    input: String,
    #[command(flatten)]
    output: VectorOutput,
    /// Band to contour, from 1.
    #[arg(short = 'b', long, default_value_t = 1)]
    band: usize,
    /// Positive elevation interval. Supply an interval or fixed levels.
    #[arg(short = 'i', long)]
    interval: Option<f64>,
    /// Offset for interval levels.
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    offset: f64,
    /// Fixed elevation levels, comma-separated. Can be combined with an interval.
    #[arg(long, value_delimiter = ',', allow_negative_numbers = true)]
    levels: Vec<f64>,
    /// Elevation field name.
    #[arg(short = 'a', long = "field", default_value = "ELEV")]
    field: String,
    /// Include elevation in each vertex's Z coordinate.
    #[arg(long = "3d")]
    with_z: bool,
    /// Override the input nodata value.
    #[arg(long, allow_negative_numbers = true)]
    nodata: Option<f64>,
    /// Include declared nodata pixels as data. Non-finite values are always excluded.
    #[arg(long, conflicts_with = "nodata")]
    ignore_nodata: bool,
}

pub fn contour(a: ContourArgs) -> Result<()> {
    let ds = super::quiet(|| terrio::open_raster(&a.input))?;
    let options = terrio::ops::contour::ContourOptions {
        band: a.band,
        interval: a.interval,
        offset: a.offset,
        levels: a.levels,
        field_name: a.field,
        with_z: a.with_z,
        nodata: a.nodata,
        ignore_nodata: a.ignore_nodata,
        ..Default::default()
    };
    a.output.write(vec![terrio::ops::contour::contour(&ds, &options)?])
}

#[cfg(test)]
mod footprint_tests {
    use super::footprint_location;

    #[test]
    fn source_locations_preserve_paths_without_persisting_url_authentication() {
        for (input, expected) in [
            ("scene?1#2.tif", "scene?1#2.tif"),
            ("/data/a@b.tif", "/data/a@b.tif"),
            ("https://user:password@example.org/a@b.tif?token=example#fragment", "https://example.org/a@b.tif"),
            ("https://example.org?token=example", "https://example.org"),
            ("s3://bucket/key.tif?X-Amz-Signature=example", "s3://bucket/key.tif"),
            ("az://container/key.tif?sig=example", "az://container/key.tif"),
            ("gs://bucket/key.tif", "gs://bucket/key.tif"),
            ("/vsicurl/https://user:password@example.org/a.tif?token=example", "/vsicurl/https://example.org/a.tif"),
            ("GDAL:ftp://user:password@example.org/a.tif?token=example", "GDAL:ftp://example.org/a.tif"),
            ("abfs://user@example.org/a.tif?sig=example", "abfs://example.org/a.tif"),
            ("s3a://bucket/a.tif?sig=example", "s3a://bucket/a.tif"),
            ("/vsicurl?url=https://user:password@example.org/a.tif?token=example", "/vsicurl?url=https://example.org/a.tif"),
            ("https://user:pa%3Fss%23word@example.org/a.tif?token=example", "https://example.org/a.tif"),
        ] {
            assert_eq!(footprint_location(input), expected);
        }
    }
}

/// Polygon coverage validation across each selected layer.
#[derive(clap::Args)]
pub struct CheckCoverageArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Flag narrow gaps up to this distance in input CRS units. Zero disables gap detection.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    maximum_gap_width: f64,
    /// Retain valid input features with empty line geometry.
    #[arg(long)]
    include_valid: bool,
}

pub fn check_coverage(a: CheckCoverageArgs) -> Result<()> {
    let input = a.input.open()?;
    let options = terrio::ops::coverage::CoverageCheckOptions { maximum_gap_width: a.maximum_gap_width, include_valid: a.include_valid };
    let layers = terrio::ops::coverage::check_coverage_layers(&input, &options)?;
    a.output.write(layers)
}

/// Shared-edge coverage simplification, retaining attributes and CRS.
#[derive(clap::Args)]
pub struct SimplifyCoverageArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Square root of the triangle-area threshold, in input CRS units.
    #[arg(long, allow_hyphen_values = true)]
    tolerance: f64,
    /// Keep exterior and hole boundaries unchanged; simplify shared edges only.
    #[arg(long)]
    preserve_boundary: bool,
    /// Process this layer and copy other selected input layers unchanged.
    #[arg(long)]
    active_layer: Option<String>,
}

pub fn simplify_coverage(a: SimplifyCoverageArgs) -> Result<()> {
    let input = a.input.open()?;
    let options =
        terrio::ops::coverage_simplify::CoverageSimplifyOptions { tolerance: a.tolerance, preserve_boundary: a.preserve_boundary };
    let layers = terrio::ops::coverage_simplify::simplify_coverage_layers(&input, &options, a.active_layer.as_deref())?;
    a.output.write_with_geometry_types(layers, true)
}

/// Attribute-preserving polygon, line and point layer algebra.
#[derive(clap::Args)]
pub struct LayerAlgebraArgs {
    /// intersection, union, sym-difference, identity, update, clip or erase.
    operation: terrio::ops::layer_algebra::LayerAlgebraOperation,
    #[command(flatten)]
    input: VectorInput,
    /// Method vector dataset, in the input CRS.
    method: String,
    #[command(flatten)]
    output: VectorOutput,
    /// Select one method layer (required for a multi-layer method dataset).
    #[arg(long)]
    method_layer: Option<String>,
    /// Include lower-dimensional intersections (clip always includes them).
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    keep_lower_dimension_geometries: bool,
    /// Input attribute prefix; defaults to input_ with both schemas, otherwise empty.
    #[arg(long)]
    input_prefix: Option<String>,
    /// Method attribute prefix; defaults to method_ with both schemas, otherwise empty.
    #[arg(long)]
    method_prefix: Option<String>,
}
pub fn layer_algebra(a: LayerAlgebraArgs) -> Result<()> {
    if !a.output.append
        && !a.output.overwrite
        && (std::fs::symlink_metadata(&a.output.output).is_ok() || terrio::vsi::stat(&a.output.output)?.is_some())
    {
        return Err(Error::InvalidArgument("output exists; use --append or --overwrite explicitly".into()));
    }
    let right = super::quiet(|| {
        terrio::open_vector(&a.method, &terrio::VectorOpenOptions { layers: a.method_layer.into_iter().collect(), ..Default::default() })
    })?;
    if right.layers.len() != 1 {
        return Err(Error::InvalidArgument("layer algebra needs one method layer; choose it with --method-layer".into()));
    }
    let options = terrio::ops::layer_algebra::LayerAlgebraOptions {
        keep_lower_dimension_geometries: a.keep_lower_dimension_geometries,
        input_prefix: a.input_prefix,
        method_prefix: a.method_prefix,
    };
    let layers = a
        .input
        .open()?
        .iter()
        .map(|l| terrio::ops::layer_algebra::layer_algebra(l, &right.layers[0], a.operation, &options))
        .collect::<Result<Vec<_>>>()?;
    a.output.write(layers)
}

/// Polygon coverage cleaning, with inactive-layer copying.
#[derive(clap::Args)]
pub struct CleanCoverageArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Clean only this layer and copy all other selected layers unchanged.
    #[arg(long)]
    active_layer: Option<String>,
    /// Snap nodes in input CRS units. Negative uses extent diameter / 1e8; zero disables proximity snapping.
    #[arg(long, default_value_t = -1.0, allow_hyphen_values = true)]
    snapping_distance: f64,
    /// Assign overlaps by longest-border, max-area, min-area or min-index.
    #[arg(long, default_value = "longest-border")]
    merge_strategy: terrio::ops::coverage_clean::CoverageMergeStrategy,
    /// Fill enclosed gaps with inscribed diameter at most this width; zero disables filling.
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    maximum_gap_width: f64,
}
pub fn clean_coverage(a: CleanCoverageArgs) -> Result<()> {
    let input = a.input.open()?;
    let options = terrio::ops::coverage_clean::CoverageCleanOptions {
        snapping_distance: a.snapping_distance,
        merge_strategy: a.merge_strategy,
        maximum_gap_width: a.maximum_gap_width,
    };
    let layers = terrio::ops::coverage_clean::clean_coverage_layers(&input, a.active_layer.as_deref(), &options)?;
    a.output.write(layers)
}

#[cfg(test)]
mod inputs_tests {
    use super::csv_records;

    #[test]
    fn csv_records_follow_rfc_4180_quoting() {
        let text = "path,label\n\" a,b.tif \",\"two\nlines\"\r\n\nc.tif , \"say \"\"hi\"\"\"  \n";
        assert_eq!(csv_records(text).unwrap(), [vec!["path", "label"], vec![" a,b.tif ", "two\nlines"], vec!["c.tif", "say \"hi\""]]);
        assert!(csv_records("path\n\"open.tif\n").unwrap_err().contains("not closed"));
        assert!(csv_records("path\n\"a\"b\n").unwrap_err().contains("after a closing quote"));
    }
}

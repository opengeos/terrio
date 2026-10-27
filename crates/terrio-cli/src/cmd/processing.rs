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
        .layers)
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

    pub(crate) fn write(&self, mut layers: Vec<Layer>) -> Result<()> {
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
        write_vector_with_mode(&layers, &self.output, &terrio::VectorWriteOptions { driver, ..Default::default() }, mode)
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
    /// An input raster (repeat for multiple rasters). Each raster is loaded into memory.
    #[arg(long = "raster", required = true, value_name = "PATH")]
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
    if !a.prefixes.is_empty() && a.prefixes.len() != a.rasters.len() {
        return Err(Error::InvalidArgument("give one --prefix per --raster, or omit all prefixes".into()));
    }
    if a.prefixes.iter().any(|p| p.trim().is_empty()) {
        return Err(Error::InvalidArgument("raster field prefixes cannot be empty".into()));
    }
    let rasters = a.rasters.iter().map(|p| super::quiet(|| terrio::open_raster(p))).collect::<Result<Vec<_>>>()?;
    let mut prefixes = Vec::new();
    for (i, path) in a.rasters.iter().enumerate() {
        let stem = std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("raster");
        let prefix = a.prefixes.get(i).map_or(stem, String::as_str);
        prefixes.push(prefix.to_string());
    }
    Ok((rasters, prefixes))
}

#[derive(clap::Args)]
pub struct ZonalArgs {
    #[command(flatten)]
    sources: SampleArgs,
    /// Statistics, comma-separated: count,min,max,mean,sum,stddev,median. Uses population standard deviation.
    #[arg(long, value_delimiter = ',', default_value = "count,min,max,mean,sum,stddev")]
    stats: Vec<String>,
    /// Include every pixel touched by each polygon, instead of pixels whose centres are inside.
    #[arg(long)]
    all_touched: bool,
}

pub fn zonal(a: ZonalArgs) -> Result<()> {
    use terrio::ops::zonal::{Statistic, ZonalInput, ZonalOptions, zonal_stats};
    let options = ZonalOptions {
        stats: a.stats.iter().map(|s| Statistic::parse(s)).collect::<Result<_>>()?,
        all_touched: a.all_touched,
        dst_srs: a.sources.t_srs.as_deref().map(terrio::crs::SpatialRef::parse).transpose()?,
        decimal_places: a.sources.decimal_places,
    };
    let (rasters, prefixes) = sample_sources(&a.sources)?;
    let mut inputs = Vec::new();
    for (i, raster) in rasters.iter().enumerate() {
        for &band in &a.sources.bands {
            inputs.push(ZonalInput { prefix: format!("{}_b{band}", prefixes[i]), raster, band });
        }
    }
    let layers = a.sources.input.open()?.iter().map(|l| zonal_stats(l, &inputs, &options)).collect::<Result<Vec<_>>>()?;
    a.sources.output.write(layers)
}

#[derive(clap::Args)]
pub struct SqlArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Read-only OGR SQL subset: expressions, DISTINCT, summaries and attribute LEFT JOIN.
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

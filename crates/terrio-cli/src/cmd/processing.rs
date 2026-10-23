//! Geometry operations, raster footprints and point sampling.

use terrio::formats::VectorDriver;
use terrio::ops::footprint::FootprintOptions;
use terrio::ops::sample::SampleInput;
use terrio::ops::vector_filter::{WriteMode, write_vector_with_mode};
use terrio::{Error, Layer, Result};

#[derive(clap::Args)]
pub struct VectorInput {
    /// The input vector dataset.
    input: String,
    /// Read only this layer (repeat to choose multiple layers). By default, every layer.
    #[arg(long = "layer", short = 'l')]
    layers: Vec<String>,
}

impl VectorInput {
    fn open(&self) -> Result<Vec<Layer>> {
        Ok(super::quiet(|| {
            terrio::open_vector(&self.input, &terrio::VectorOpenOptions { layers: self.layers.clone(), ..Default::default() })
        })?
        .layers)
    }
}

#[derive(clap::Args)]
pub struct VectorOutput {
    /// The output vector dataset.
    output: String,
    /// The output format's short name. By default, chosen from the output's extension.
    #[arg(short = 'f', long = "format")]
    format: Option<String>,
    /// The output layer name (requires a single input layer).
    #[arg(long, value_name = "NAME")]
    nln: Option<String>,
    /// Append features to layers of the same name in an existing GeoPackage.
    #[arg(long, conflicts_with = "overwrite")]
    append: bool,
    /// Replace layers of the same name in an existing output, keeping other GeoPackage layers.
    #[arg(long)]
    overwrite: bool,
}

impl VectorOutput {
    fn write(&self, mut layers: Vec<Layer>) -> Result<()> {
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
}

pub fn footprint(a: FootprintArgs) -> Result<()> {
    let raster = super::quiet(|| terrio::open_raster(&a.input))?;
    let mask = a.mask.as_deref().map(|p| super::quiet(|| terrio::open_raster(p))).transpose()?;
    let options = FootprintOptions { bands: a.bands, intersection: a.intersection, mask };
    a.output.write(vec![terrio::ops::footprint::footprint(&raster, &options)?])
}

#[derive(clap::Args)]
pub struct SampleArgs {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// A raster to sample (repeat for multiple rasters). Each raster is loaded into memory.
    #[arg(long = "raster", required = true, value_name = "PATH")]
    rasters: Vec<String>,
    /// Bands to sample from every raster, from 1, comma-separated. By default, band 1.
    #[arg(short = 'b', long = "band", value_delimiter = ',', default_value = "1")]
    bands: Vec<usize>,
    /// Output field prefix, one per --raster in the same order. By default, its file stem.
    /// Fields are PREFIX_bN; name collisions get _2, _3, etc.
    #[arg(long = "prefix", value_name = "NAME")]
    prefixes: Vec<String>,
}

pub fn sample(a: SampleArgs) -> Result<()> {
    if !a.prefixes.is_empty() && a.prefixes.len() != a.rasters.len() {
        return Err(Error::InvalidArgument("give one --prefix per --raster, or omit all prefixes".into()));
    }
    if a.prefixes.iter().any(|p| p.trim().is_empty()) {
        return Err(Error::InvalidArgument("sample prefixes cannot be empty".into()));
    }
    let rasters = a.rasters.iter().map(|p| super::quiet(|| terrio::open_raster(p))).collect::<Result<Vec<_>>>()?;
    let mut inputs = Vec::new();
    for (i, raster) in rasters.iter().enumerate() {
        let stem = std::path::Path::new(&a.rasters[i]).file_stem().and_then(|s| s.to_str()).unwrap_or("raster");
        let prefix = a.prefixes.get(i).map_or(stem, String::as_str);
        for &band in &a.bands {
            inputs.push(SampleInput { field_name: format!("{prefix}_b{band}"), raster, band });
        }
    }
    let layers = a.input.open()?.iter().map(|l| terrio::ops::sample::sample(l, &inputs)).collect::<Result<Vec<_>>>()?;
    a.output.write(layers)
}

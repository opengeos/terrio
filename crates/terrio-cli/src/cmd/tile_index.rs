//! Raster/vector footprint indexes and GTI-compatible metadata.
use terrio::crs::SpatialRef;
use terrio::formats::VectorDriver;
use terrio::ops::tile_index::{
    self as ti, GtiOptions, IndexInputOptions, IndexOptions, MetadataField, RasterIndexOptions, SourceCrsFormat, VectorIndexOptions,
    VectorIndexSelection,
};
use terrio::{Error, Result, VectorWriteOptions};

#[derive(clap::Args)]
pub struct CommonArgs {
    /// Input datasets/directories followed by the output. `@FILE` reads input paths.
    #[arg(required=true,num_args=1..,value_name="INPUT... OUTPUT")]
    paths: Vec<String>,
    /// Output vector format (otherwise inferred from the filename).
    #[arg(short = 'f', long = "format", visible_alias = "of")]
    format: Option<String>,
    /// Output layer name. Raster: output basename; vector: tileindex.
    #[arg(long = "output-layer", visible_alias = "nln")]
    output_layer: Option<String>,
    /// Field containing source locations.
    #[arg(long = "location-name", default_value = "location")]
    location_name: String,
    /// Store absolute local source paths.
    #[arg(long)]
    absolute_path: bool,
    /// Scan subdirectories too.
    #[arg(long)]
    recursive: bool,
    /// Filename pattern (* and ?) for directory entries, repeatable.
    #[arg(long = "filename-filter")]
    filename_filters: Vec<String>,
    /// Target CRS (warped raster extent or transformed vector corners).
    #[arg(long = "output-crs")]
    output_crs: Option<String>,
    /// Add a field containing each original source CRS.
    #[arg(long = "source-crs-field-name")]
    source_crs_field: Option<String>,
    /// auto, WKT, EPSG or PROJ.
    #[arg(long = "source-crs-format", default_value = "auto")]
    source_crs_format: String,
    /// Layer metadata KEY=VALUE (GeoPackage/FlatGeobuf), repeatable.
    #[arg(long)]
    metadata: Vec<String>,
    /// Atomically replace a single-file output. Multi-file formats need a new basename.
    #[arg(long)]
    overwrite: bool,
}
impl CommonArgs {
    fn options(&self) -> Result<IndexOptions> {
        Ok(IndexOptions {
            layer_name: self.output_layer.clone(),
            location_field: self.location_name.clone(),
            output_crs: self.output_crs.as_deref().map(SpatialRef::parse).transpose()?,
            source_crs_field: self.source_crs_field.clone(),
            source_crs_format: SourceCrsFormat::parse(&self.source_crs_format)?,
            metadata: terrio::ops::vector_edit::parse_metadata_items(&self.metadata)?,
        })
    }
    fn inputs(&self, skip_errors: bool) -> IndexInputOptions {
        IndexInputOptions {
            recursive: self.recursive,
            filename_filters: self.filename_filters.clone(),
            absolute_path: self.absolute_path,
            skip_errors,
        }
    }
    fn paths(&self) -> Result<(&str, Vec<String>)> {
        if self.paths.len() < 2 {
            return Err(Error::InvalidArgument("give inputs and an output".into()));
        }
        let (output, inputs) = self.paths.split_last().ok_or_else(|| Error::InvalidArgument("give inputs and an output".into()))?;
        Ok((output, super::expand_file_lists(inputs)?))
    }
    fn write(&self, index: &ti::DatasetIndex, output: &str) -> Result<()> {
        let driver = self
            .format
            .as_deref()
            .map(|s| VectorDriver::from_name(s).ok_or_else(|| Error::InvalidArgument(format!("unknown vector format '{s}'"))))
            .transpose()?;
        index.write(output, &VectorWriteOptions { driver, ..Default::default() }, self.overwrite)
    }
}
/// `terrio raster-index` (also `gti-create`).
#[derive(clap::Args)]
pub struct RasterArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Skip unsupported/unreadable raster inputs with a warning.
    #[arg(long)]
    skip_errors: bool,
    /// Minimum sqrt(envelope area / pixel count) after reprojection.
    #[arg(long)]
    min_pixel_size: Option<f64>,
    /// Maximum envelope-based pixel size in index coordinates.
    #[arg(long)]
    max_pixel_size: Option<f64>,
    /// Copy metadata: KEY,FIELD,TYPE (String, Integer or Real), repeatable.
    #[arg(long = "fetch-metadata")]
    fetch_metadata: Vec<String>,
    /// GTI virtual mosaic resolution in index CRS units (x,y).
    #[arg(long, value_parser = pair)]
    resolution: Option<[f64; 2]>,
    /// GTI virtual mosaic extent xmin,ymin,xmax,ymax.
    #[arg(long, value_parser = bounds, allow_hyphen_values = true)]
    bbox: Option<[f64; 4]>,
    /// GTI output sample type (otherwise inferred by the reader).
    #[arg(long = "ot", visible_alias = "datatype")]
    ot: Option<String>,
    /// GTI virtual raster band count.
    #[arg(long)]
    band_count: Option<usize>,
    /// GTI nodata value, repeatable or comma separated (one or one per band).
    #[arg(long, value_delimiter = ',', allow_hyphen_values = true)]
    nodata: Vec<f64>,
    /// GTI band color interpretations (gray,red,green,blue,alpha,undefined).
    #[arg(long = "color-interpretation", value_delimiter = ',')]
    color_interpretations: Vec<String>,
    /// Declare a GTI dataset mask.
    #[arg(long)]
    mask: bool,
}
fn numbers<const N: usize>(s: &str) -> std::result::Result<[f64; N], String> {
    s.split(',')
        .map(|v| v.parse::<f64>().map_err(|e| e.to_string()))
        .collect::<std::result::Result<Vec<_>, _>>()?
        .try_into()
        .map_err(|_| format!("expected {N} comma-separated numbers"))
}
fn pair(s: &str) -> std::result::Result<[f64; 2], String> {
    numbers(s)
}
fn bounds(s: &str) -> std::result::Result<[f64; 4], String> {
    numbers(s)
}
fn metadata_field(s: &str) -> Result<MetadataField> {
    let parts: Vec<_> = s.split(',').collect();
    let [key, name, kind] = parts.as_slice() else {
        return Err(Error::InvalidArgument("fetch-metadata needs KEY,FIELD,TYPE".into()));
    };
    MetadataField::from_type_name(*key, *name, kind)
}
/// Creates a raster footprint index, optionally configured as a GTI mosaic.
pub fn raster(a: RasterArgs) -> Result<()> {
    let (output, paths) = a.common.paths()?;
    let o = RasterIndexOptions {
        index: a.common.options()?,
        min_pixel_size: a.min_pixel_size,
        max_pixel_size: a.max_pixel_size,
        fetch_metadata: a.fetch_metadata.iter().map(|s| metadata_field(s)).collect::<Result<Vec<_>>>()?,
        gti: GtiOptions {
            resolution: a.resolution,
            extent: a.bbox,
            data_type: a.ot.as_deref().map(terrio::ops::parse_data_type).transpose()?,
            band_count: a.band_count,
            nodata: a.nodata,
            color_interpretations: a.color_interpretations,
            mask: a.mask,
        },
    };
    let index = super::quiet(|| ti::raster_index_paths(&paths, &a.common.inputs(a.skip_errors), &o))?;
    a.common.write(&index, output)
}
/// `terrio vector-index`.
#[derive(clap::Args)]
pub struct VectorArgs {
    #[command(flatten)]
    common: CommonArgs,
    /// Select layers with these names across the inputs, repeatable.
    #[arg(long = "source-layer-name")]
    layers: Vec<String>,
    /// Add original zero-based source layer numbers, repeatable.
    #[arg(long = "source-layer-index")]
    layer_indexes: Vec<usize>,
    /// Accept differing source CRSs without reprojection.
    #[arg(long)]
    accept_different_crs: bool,
    /// Accept differing source field schemas.
    #[arg(long)]
    accept_different_schemas: bool,
    /// Omit the comma and layer index from locations.
    #[arg(long)]
    dataset_name_only: bool,
}
/// Creates one envelope feature per selected vector layer.
pub fn vector(a: VectorArgs) -> Result<()> {
    let (output, paths) = a.common.paths()?;
    let o = VectorIndexOptions {
        index: a.common.options()?,
        accept_different_crs: a.accept_different_crs,
        accept_different_schemas: a.accept_different_schemas,
        dataset_name_only: a.dataset_name_only,
    };
    let selection = VectorIndexSelection { names: a.layers, indexes: a.layer_indexes };
    let index = super::quiet(|| ti::vector_index_paths(&paths, &a.common.inputs(false), &selection, &o))?;
    a.common.write(&index, output)
}

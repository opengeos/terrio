//! Polygon clipping of vector features.
use super::processing::{VectorInput, VectorOutput};
use terrio::crs::SpatialRef;
use terrio::ops::vector_clip::{ClipRegion, ClipTemplateOptions};
use terrio::{Error, Result};

#[derive(clap::Args)]
#[command(group(clap::ArgGroup::new("region").args(["bbox", "geometry", "like"]).required(true)))]
pub struct Args {
    #[command(flatten)]
    input: VectorInput,
    #[command(flatten)]
    output: VectorOutput,
    /// Clip to xmin,ymin,xmax,ymax in the input CRS, unless --bbox-crs is supplied.
    #[arg(long, value_delimiter = ',', allow_hyphen_values = true)]
    bbox: Option<Vec<f64>>,
    #[arg(long, requires = "bbox")]
    bbox_crs: Option<String>,
    /// Polygon/multipolygon as WKT (input CRS) or GeoJSON (WGS84 by default).
    #[arg(long)]
    geometry: Option<String>,
    #[arg(long, requires = "geometry")]
    geometry_crs: Option<String>,
    /// Polygon vector template (unioned), or a raster's rectangular bounds.
    #[arg(long)]
    like: Option<String>,
    #[arg(long, requires = "like")]
    like_layer: Option<String>,
    /// Filter template features with the supported OGR SQL WHERE subset.
    #[arg(long, requires = "like", conflicts_with = "like_sql")]
    like_where: Option<String>,
    /// Query the template with the supported vector SELECT subset.
    #[arg(long, requires = "like")]
    like_sql: Option<String>,
}

pub fn run(a: Args) -> Result<()> {
    if !a.output.append
        && !a.output.overwrite
        && (std::fs::symlink_metadata(&a.output.output).is_ok() || terrio::vsi::stat(&a.output.output)?.is_some())
    {
        return Err(Error::InvalidArgument("output exists; use --append or --overwrite explicitly".into()));
    }
    let region = if let Some(bbox) = a.bbox {
        ClipRegion::bbox(
            bbox.try_into().map_err(|_| Error::InvalidArgument("bbox needs four coordinates".into()))?,
            a.bbox_crs.as_deref().map(SpatialRef::parse).transpose()?,
        )?
    } else if let Some(text) = a.geometry {
        ClipRegion::parse(&text, a.geometry_crs.as_deref().map(SpatialRef::parse).transpose()?)?
    } else {
        let path = a.like.as_deref().unwrap();
        ClipRegion::from_path(path, &ClipTemplateOptions { layer: a.like_layer, where_clause: a.like_where, sql: a.like_sql })?
    };
    let input = terrio::ops::vector_filter::open_layers(
        &a.input.input,
        &terrio::VectorOpenOptions { layers: a.input.layers, ..Default::default() },
    )?;
    let layers = input.iter().map(|l| terrio::ops::vector_clip::clip(l, &region)).collect::<Result<Vec<_>>>()?;
    a.output.write(layers)
}

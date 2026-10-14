//! `terrio rasterize`.

use terrio::VectorOpenOptions;
use terrio::crs::SpatialRef;
use terrio::ops::rasterize::{Grid, MergeAlg, RasterizeOptions};
use terrio::ops::vector_filter::VectorTranslateOptions;

#[derive(clap::Args)]
pub struct Args {
    /// The vector dataset.
    input: String,
    /// The output raster.
    output: String,
    /// The layers to burn. Needed when the dataset has several.
    #[arg(short, long = "layer")]
    layers: Vec<String>,
    /// Burn the result of "SELECT fields FROM layer [WHERE ...]" instead of whole layers.
    #[arg(long, value_name = "STATEMENT")]
    sql: Option<String>,
    /// A value to burn; give one per output band.
    #[arg(long, value_name = "VALUE", allow_negative_numbers = true, required_unless_present = "attribute")]
    burn: Vec<f64>,
    /// Burn the values of this attribute field.
    #[arg(short = 'a', long = "attribute", value_name = "FIELD", conflicts_with = "burn")]
    attribute: Option<String>,
    /// Only burn the features matching this OGR SQL expression.
    #[arg(long = "where", value_name = "EXPR")]
    where_clause: Option<String>,
    /// The pixel size.
    #[arg(long, num_args = 2, value_names = ["XRES", "YRES"])]
    tr: Option<Vec<f64>>,
    /// The size in pixels; one of them may be 0 to keep the aspect ratio.
    #[arg(long, num_args = 2, value_names = ["WIDTH", "HEIGHT"], conflicts_with = "tr")]
    ts: Option<Vec<usize>>,
    /// The extent. By default, that of the layers.
    #[arg(long, num_args = 4, value_names = ["XMIN", "YMIN", "XMAX", "YMAX"], allow_negative_numbers = true)]
    te: Option<Vec<f64>>,
    /// Align the extent on multiples of the pixel size.
    #[arg(long, requires = "tr")]
    tap: bool,
    /// Use the grid (size, georeferencing, CRS) of this raster.
    #[arg(long, value_name = "RASTER", conflicts_with_all = ["tr", "ts", "te"])]
    like: Option<String>,
    /// The output data type: Byte, Int8, UInt16, Int16, UInt32, Int32, UInt64, Int64, Float32 or
    /// Float64 (the default).
    #[arg(long, value_name = "TYPE")]
    ot: Option<String>,
    /// The initial pixel values, one per band or one for all. 0 by default.
    #[arg(long, value_name = "VALUE", allow_negative_numbers = true)]
    init: Vec<f64>,
    /// The nodata value of the output.
    #[arg(long = "a-nodata", value_name = "VALUE", allow_negative_numbers = true)]
    a_nodata: Option<f64>,
    /// Burn all pixels touched by lines or polygons, not only those whose centre is inside.
    #[arg(long)]
    at: bool,
    /// Add the burn values to the pixels instead of replacing them.
    #[arg(long)]
    add: bool,
    /// The output CRS. Geometries are taken to be in it, without reprojecting.
    #[arg(long = "a-srs", value_name = "CRS")]
    a_srs: Option<String>,
    #[command(flatten)]
    out: super::rasterout::OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let ds = super::quiet(|| terrio::open_vector(&a.input, &VectorOpenOptions { layers: a.layers.clone() }))?;
    let layers = match &a.sql {
        Some(sql) => {
            terrio::ops::vector_filter::translate(&ds.layers, &VectorTranslateOptions { sql: Some(sql.clone()), ..Default::default() })?
        }
        None if a.layers.is_empty() && ds.layers.len() != 1 => {
            return Err(terrio::Error::InvalidArgument(format!(
                "the dataset has {} layers: choose with --layer or --sql ({})",
                ds.layers.len(),
                ds.layers.iter().map(|l| l.name.as_str()).collect::<Vec<_>>().join(", ")
            )));
        }
        None => ds.layers,
    };
    let grid = match &a.like {
        Some(path) => Some(Grid::like(&super::quiet(|| terrio::open_raster(path))?)),
        None => None,
    };
    let data_type = a.ot.as_deref().map(terrio::ops::parse_data_type).transpose()?;
    let options = RasterizeOptions {
        burn_values: a.burn,
        attribute: a.attribute,
        where_clause: a.where_clause,
        resolution: a.tr.as_deref().map(|v| (v[0], v[1])),
        size: a.ts.as_deref().map(|v| (v[0], v[1])),
        extent: a.te.as_deref().map(|v| (v[0], v[1], v[2], v[3])),
        target_aligned: a.tap,
        grid,
        data_type,
        init: a.init,
        nodata: a.a_nodata,
        all_touched: a.at,
        merge: if a.add { MergeAlg::Add } else { MergeAlg::Replace },
        assign_srs: a.a_srs.as_deref().map(SpatialRef::parse).transpose()?,
    };
    let out = terrio::ops::rasterize::rasterize(&layers, &options)?;
    a.out.write(&out, &a.output)
}

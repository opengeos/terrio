//! `terrio catalog`: indexed raster selection and compositing.
use super::rasterout::{OutputArgs, srs};
use terrio::catalog::{CatalogOptions, RasterCatalog};
use terrio::ops::{Resampling, parse_data_type};
#[derive(clap::Args)]
pub struct Args {
    /// A GeoPackage/FlatGeobuf tile index or a GTI XML descriptor.
    index: String,
    /// The raster output.
    output: String,
    /// The index layer, required for an index of several layers.
    #[arg(long)]
    layer: Option<String>,
    /// String field containing source paths (default location).
    #[arg(long)]
    location_field: Option<String>,
    /// Z-order field: larger values are drawn last by default.
    #[arg(long)]
    sort_field: Option<String>,
    /// Draw smaller sort-field values last.
    #[arg(long)]
    sort_descending: bool,
    /// Output pixel resolution in the index CRS.
    #[arg(long,num_args=2,value_names=["XRES","YRES"])]
    tr: Option<Vec<f64>>,
    /// Output extent in the index CRS.
    #[arg(long,num_args=4,allow_negative_numbers=true,value_names=["XMIN","YMIN","XMAX","YMAX"])]
    te: Option<Vec<f64>>,
    /// Assign the index CRS if missing; otherwise must match the index CRS.
    #[arg(long)]
    srs: Option<String>,
    /// Number of output bands (default from the first tile).
    #[arg(long)]
    band_count: Option<usize>,
    /// Output data type (default from the first tile).
    #[arg(long)]
    ot: Option<String>,
    /// Output nodata, allowing NaN (default from the first tile).
    #[arg(long, allow_hyphen_values = true)]
    nodata: Option<f64>,
    /// An output alpha band, numbered from 1; zero alpha masks the source.
    #[arg(long)]
    alpha_band: Option<usize>,
    /// Maximum cached source datasets (default 32).
    #[arg(long)]
    cache_size: Option<usize>,
    /// Resampling for mixed grids and CRSs.
    #[arg(short = 'r', long)]
    resampling: Option<String>,
    /// Read only this output pixel window.
    #[arg(long,num_args=4,value_names=["XOFF","YOFF","WIDTH","HEIGHT"])]
    window: Option<Vec<usize>>,
    /// Print logical I/O counters as JSON after a window read.
    #[arg(long, requires = "window")]
    stats: bool,
    #[command(flatten)]
    out: OutputArgs,
}
pub fn run(a: Args) -> terrio::Result<()> {
    let options = CatalogOptions {
        layer: a.layer,
        location_field: a.location_field,
        sort_field: a.sort_field,
        sort_descending: a.sort_descending.then_some(true),
        resolution: a.tr.map(|v| (v[0], v[1])),
        extent: a.te.map(|v| (v[0], v[1], v[2], v[3])),
        srs: srs(&a.srs)?,
        bands: a.band_count,
        data_type: a.ot.as_deref().map(parse_data_type).transpose()?,
        nodata: a.nodata,
        alpha_band: a.alpha_band,
        cache_size: a.cache_size,
        resampling: a.resampling.as_deref().map(Resampling::parse).transpose()?,
    };
    let mut catalog = super::quiet(|| RasterCatalog::open(&a.index, options))?;
    if let Some(w) = a.window {
        let output = super::quiet(|| catalog.read_window(terrio::Window::new(w[0], w[1], w[2], w[3]), &[]))?;
        a.out.write(&output, &a.output)?;
        if a.stats {
            println!("{}", serde_json::to_string(&catalog.stats()).map_err(|e| terrio::Error::InvalidData(e.to_string()))?);
        }
        Ok(())
    } else {
        super::quiet(|| catalog.into_lazy().write(&a.output, &a.out.options()?))
    }
}

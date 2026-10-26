//! `terrio neighbors`.

use super::rasterout::{OutputArgs, open, open_lazy, same_file};
use terrio::ops::neighbors::{NeighborMethod, NeighborsNodata, NeighborsOptions, neighbors, neighbors_to_path};

#[derive(clap::Args)]
pub struct Args {
    /// The input raster.
    input: String,
    /// The output path.
    output: String,
    /// The band to filter, from 1. By default every band is filtered.
    #[arg(short = 'b', long = "band")]
    band: Option<usize>,
    /// A kernel: u, v, edge1, edge2, sharpen, gaussian, unsharp-masking, equal, or a matrix
    /// such as [[0,1,0],[1,1,1],[0,1,0]] (rows in order). Repeat for several output bands per
    /// input band.
    #[arg(long = "kernel", required = true, allow_hyphen_values = true)]
    kernels: Vec<String>,
    /// How weighted neighbours are combined: sum, mean, min, max, stddev, median or mode. Once
    /// for every kernel, or once per kernel. By default sum for kernels whose coefficients sum to
    /// zero (u, v, edge1, edge2), mean for others.
    #[arg(long = "method")]
    methods: Vec<String>,
    /// The neighbourhood size: an odd number from 3 to 99 (any for equal; 3 or 5 for gaussian;
    /// 5 for unsharp-masking; 3 for the other named kernels).
    #[arg(long)]
    size: Option<usize>,
    /// The output data type (Float64 by default).
    #[arg(long = "ot", alias = "datatype", alias = "output-data-type", default_value = "Float64")]
    ot: String,
    /// The output nodata value: a number, nan, inf, -inf or none. By default the input band's.
    #[arg(long, allow_hyphen_values = true)]
    nodata: Option<String>,
    #[command(flatten)]
    out: OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let o = NeighborsOptions {
        band: a.band,
        kernels: a.kernels,
        methods: a.methods.iter().map(|m| NeighborMethod::parse(m)).collect::<terrio::Result<_>>()?,
        size: a.size,
        output_type: terrio::ops::parse_data_type(&a.ot)?,
        nodata: a.nodata.as_deref().map(NeighborsNodata::parse).transpose()?.unwrap_or_default(),
    };
    // The input read whole first when it is also the output.
    if same_file(&a.input, &a.output) {
        return a.out.write(&neighbors(&open(&a.input)?, &o)?, &a.output);
    }
    let mut src = open_lazy(&a.input)?;
    super::quiet(|| neighbors_to_path(&mut src, &o, &a.output, &a.out.options()?))
}

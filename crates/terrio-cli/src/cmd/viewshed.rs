//! `terrio viewshed` (also exposed automatically as an MCP tool).
use super::rasterout::{OutputArgs, open};
use terrio::ops::viewshed::{ViewshedMode, ViewshedOptions, viewshed};

#[derive(clap::Args)]
pub struct Args {
    /// Projected DEM, then output path.
    input: String,
    output: String,
    /// Observer easting in the DEM CRS.
    #[arg(long, allow_negative_numbers = true)]
    ox: f64,
    /// Observer northing in the DEM CRS.
    #[arg(long, allow_negative_numbers = true)]
    oy: f64,
    /// Observer height above the DEM, in elevation units.
    #[arg(long, default_value_t = 2.0, allow_negative_numbers = true)]
    oz: f64,
    /// Target height above the DEM (NORMAL only).
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    tz: f64,
    /// Maximum distance in projected CRS units; zero is unlimited. Also crops the output.
    #[arg(long, default_value_t = 0.0)]
    md: f64,
    /// Curvature coefficient (1 minus refraction). 0 disables curvature, 1 disables refraction.
    #[arg(long, allow_negative_numbers = true)]
    cc: Option<f64>,
    /// NORMAL visibility, DEM minimum visible elevation, or GROUND minimum height above DEM.
    #[arg(long, default_value = "NORMAL")]
    om: String,
    /// Output value outside the maximum distance.
    #[arg(long, default_value_t = 0)]
    ov: u8,
    /// Input band, from 1. Numeric nodata is treated as elevation, as in GDAL.
    #[arg(short = 'b', long, default_value_t = 1)]
    band: usize,
    #[command(flatten)]
    out: OutputArgs,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let ds = open(&a.input)?;
    let o = ViewshedOptions {
        band: a.band,
        observer_x: a.ox,
        observer_y: a.oy,
        observer_height: a.oz,
        target_height: a.tz,
        max_distance: a.md,
        curvature_coefficient: a.cc,
        mode: ViewshedMode::parse(&a.om)?,
        out_of_range: a.ov,
    };
    a.out.write(&viewshed(&ds, &o)?, &a.output)
}

#[cfg(test)]
mod tests {
    use clap::Parser;
    #[test]
    fn parses_negative_curvature() {
        // Full-parser tests need the same stack as the CLI worker on Windows.
        std::thread::Builder::new()
            .stack_size(crate::STACK_SIZE)
            .spawn(|| {
                let cli =
                    crate::Cli::try_parse_from(["terrio", "viewshed", "dem.tif", "out.tif", "--ox", "5", "--oy", "5", "--cc", "-0.5"])
                        .unwrap();
                let crate::Command::Viewshed(args) = cli.command else { panic!("wrong command") };
                assert_eq!(args.cc, Some(-0.5));
            })
            .unwrap()
            .join()
            .unwrap();
    }
}

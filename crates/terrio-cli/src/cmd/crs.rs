//! `terrio crs`.

use terrio::crs::{SpatialRef, TransformOptions, Transformer};

#[derive(clap::Args)]
pub struct Args {
    /// The CRS: `EPSG:4326`, `EPSG:4326+5773` (a compound CRS), WKT, PROJJSON, a PROJ string, or a file
    /// holding one.
    crs: String,
    /// Print the WKT definition.
    #[arg(long)]
    wkt: bool,
    /// Transform coordinates into this CRS. Reads `x y` pairs from the arguments, or lines of
    /// `x y`, `x y z` or `x y z t` (t: a decimal year) from standard input when none are given,
    /// as `gdaltransform` does (printing `transformation failed.` for a point that fails).
    /// Heights go through geoid models between CRSs with heights (geographic 3D and compound
    /// CRSs).
    #[arg(long, value_name = "CRS")]
    to: Option<String>,
    /// The coordinate epoch of the source coordinates (a decimal year), for a dynamic source CRS,
    /// as `gdaltransform -s_coord_epoch`.
    #[arg(long = "s_coord_epoch", visible_alias = "s-coord-epoch", value_name = "YEAR")]
    s_coord_epoch: Option<f64>,
    /// The coordinate epoch of the target coordinates, for a dynamic target CRS, as
    /// `gdaltransform -t_coord_epoch`.
    #[arg(long = "t_coord_epoch", visible_alias = "t-coord-epoch", value_name = "YEAR")]
    t_coord_epoch: Option<f64>,
    /// Coordinate operation options: ALLOW_BALLPARK=YES|NO, ONLY_BEST=YES|NO.
    #[arg(long = "ct-opt", visible_aliases = ["to-option", "ct_opt"], value_name = "KEY=VALUE")]
    transform_options: Vec<String>,
    /// Number of ordinates per positional point (2: XY, 3: XYZ, 4: XYZ and epoch).
    #[arg(long = "dim", default_value_t = 2, value_parser = clap::value_parser!(u8).range(2..=4))]
    dimension: u8,
    /// Coordinates to transform, grouped according to --dim (XY by default).
    #[arg(allow_negative_numbers = true)]
    coords: Vec<f64>,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let src = SpatialRef::parse(&a.crs)?;
    let Some(to) = a.to else {
        println!("{}", src.label());
        if let Some(name) = src.name() {
            println!("Name: {name}");
        }
        println!("Geographic: {}", src.is_geographic());
        if src.is_geocentric() {
            println!("Geocentric: true");
        }
        if a.wkt
            && let Some(w) = src.to_wkt()
        {
            println!("{w}");
        }
        return Ok(());
    };
    let dst = SpatialRef::parse(&to)?;
    let opts = TransformOptions {
        source_coordinate_epoch: a.s_coord_epoch,
        target_coordinate_epoch: a.t_coord_epoch,
        ..super::vector_opts::coordinate_options(&a.transform_options)?
    };
    let t = Transformer::with_options(&src, &dst, &opts)?;
    let print = |nums: &[f64]| -> terrio::Result<()> {
        match nums {
            [x, y] => {
                let (tx, ty) = t.transform(*x, *y)?;
                println!("{tx} {ty}");
            }
            [x, y, z] => {
                let (tx, ty, tz) = t.transform_3d(*x, *y, *z)?;
                println!("{tx} {ty} {tz}");
            }
            [x, y, z, time, ..] => {
                let (tx, ty, tz) = t.transform_4d(*x, *y, *z, *time)?;
                println!("{tx} {ty} {tz}");
            }
            _ => {}
        }
        Ok(())
    };
    if !a.coords.is_empty() {
        let dimension = usize::from(a.dimension);
        if !a.coords.len().is_multiple_of(dimension) {
            return Err(terrio::Error::InvalidArgument(format!("coordinates come in groups of {dimension} ordinates")));
        }
        for p in a.coords.chunks(dimension) {
            print(p)?;
        }
        return Ok(());
    }
    // As gdaltransform: a point that fails prints "transformation failed." and the others go on.
    for line in std::io::stdin().lines() {
        let line = line?;
        let nums: Vec<f64> = line.split([' ', ',', '\t']).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
        if nums.len() >= 2 && print(&nums).is_err() {
            println!("transformation failed.");
        }
    }
    Ok(())
}

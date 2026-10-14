//! `terrio crs`.

use terrio::crs::{SpatialRef, Transformer};

#[derive(clap::Args)]
pub struct Args {
    /// The CRS: `EPSG:4326`, WKT, a PROJ string, or a file holding one.
    crs: String,
    /// Print the WKT definition.
    #[arg(long)]
    wkt: bool,
    /// Transform coordinates into this CRS. Reads `x y` pairs from the arguments, or from standard
    /// input when none are given.
    #[arg(long, value_name = "CRS")]
    to: Option<String>,
    /// Coordinates to transform: x y [x y ...].
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
        if a.wkt
            && let Some(w) = src.to_wkt()
        {
            println!("{w}");
        }
        return Ok(());
    };
    let dst = SpatialRef::parse(&to)?;
    let t = Transformer::new(&src, &dst)?;
    let print = |x: f64, y: f64| -> terrio::Result<()> {
        let (tx, ty) = t.transform(x, y)?;
        println!("{tx} {ty}");
        Ok(())
    };
    if !a.coords.is_empty() {
        if !a.coords.len().is_multiple_of(2) {
            return Err(terrio::Error::InvalidArgument("coordinates come in x y pairs".into()));
        }
        for p in a.coords.chunks(2) {
            print(p[0], p[1])?;
        }
        return Ok(());
    }
    for line in std::io::stdin().lines() {
        let line = line?;
        let nums: Vec<f64> = line.split([' ', ',', '\t']).filter(|s| !s.is_empty()).filter_map(|s| s.parse().ok()).collect();
        if nums.len() >= 2 {
            print(nums[0], nums[1])?;
        }
    }
    Ok(())
}

// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdaltransform.rst
//   PROJ 9.8.1:
//     docs/source/apps/projinfo.rst (--list-crs)
// No code is translated from these sources.

//! `terrio crs` and `terrio list-crs`.

use clap::builder::TypedValueParser;
use std::io::Write;
use terrio::crs::{AUTHORITIES, CrsKind, CrsQuery, SpatialRef, TransformOptions, Transformer, list_crs};

#[derive(clap::Args)]
pub struct Args {
    /// The CRS: `EPSG:4326`, `IAU_2015:49900`, `EPSG:4326+5773` (compound), WKT, PROJJSON, PROJ, or a file
    /// holding one.
    crs: String,
    /// Print the WKT definition.
    #[arg(long)]
    wkt: bool,
    /// Print only the PROJJSON definition, so that the output is a PROJJSON file (refused for a CRS
    /// it would describe wrongly).
    #[arg(long, conflicts_with_all = ["to", "wkt", "coords", "dimension", "transform_options", "s_coord_epoch", "t_coord_epoch"])]
    projjson: bool,
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
    /// Coordinate operation options: ALLOW_BALLPARK=YES|NO, ONLY_BEST=YES|NO, PROMOTE_TO_3D=YES|NO.
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
    if a.projjson {
        println!("{}", src.to_projjson()?);
        return Ok(());
    }
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

#[derive(clap::Args)]
pub struct ListArgs {
    /// Words the CRSs must match, whatever their case: each in the name, or beginning the code or
    /// `AUTHORITY:CODE` (`Albers`, `3577`, `EPSG:3577`, `gda94 albers`). Every CRS when none.
    text: Vec<String>,
    /// List only the CRSs of this authority (repeatable).
    #[arg(long, value_name = "AUTHORITY", value_parser = authority_parser(), ignore_case = true)]
    authority: Vec<String>,
    /// List only the CRSs of this kind (repeatable).
    #[arg(long, value_name = "KIND", value_parser = kind_parser(), ignore_case = true)]
    kind: Vec<CrsKind>,
    /// List deprecated CRSs too, marked `[deprecated]`.
    #[arg(long, visible_alias = "allow-deprecated")]
    include_deprecated: bool,
    /// Print JSON: an array of `{authority, code, name, kind, deprecated, area_name, bounds,
    /// projection_method, celestial_body}`, bounds being west, south, east and north in degrees.
    #[arg(long)]
    json: bool,
}

/// The authorities `--authority` accepts, whatever their case.
fn authority_parser() -> impl TypedValueParser<Value = String> {
    clap::builder::PossibleValuesParser::new(AUTHORITIES).map(|a| a.to_string())
}

/// The kinds `--kind` accepts, whatever their case.
fn kind_parser() -> impl TypedValueParser<Value = CrsKind> {
    clap::builder::PossibleValuesParser::new(CrsKind::ALL.map(CrsKind::as_str))
        .try_map(|k: String| k.parse::<CrsKind>().map_err(|e| e.to_string()))
}

/// Lists registry CRSs, one per line as `projinfo --list-crs` prints them: `EPSG:3577 "GDA94 /
/// Australian Albers"`, then ` [deprecated]` for a deprecated CRS.
pub fn list(a: ListArgs) -> terrio::Result<()> {
    let query =
        CrsQuery { authorities: a.authority, kinds: a.kind, include_deprecated: a.include_deprecated, text: Some(a.text.join(" ")) };
    let entries = list_crs(&query);
    if a.json {
        let json = serde_json::to_string_pretty(&entries).map_err(|e| terrio::Error::InvalidData(e.to_string()))?;
        // As the listing below, a closed pipe (`| head`) ends the output.
        if let Err(error) = writeln!(std::io::stdout().lock(), "{json}")
            && error.kind() != std::io::ErrorKind::BrokenPipe
        {
            return Err(error.into());
        }
        return Ok(());
    }
    let mut out = std::io::stdout().lock();
    for e in entries {
        let deprecated = if e.deprecated { " [deprecated]" } else { "" };
        // A closed pipe (`| head`) ends the listing.
        if let Err(error) = writeln!(out, "{}:{} \"{}\"{deprecated}", e.authority, e.code, e.name) {
            if error.kind() == std::io::ErrorKind::BrokenPipe {
                break;
            }
            return Err(error.into());
        }
    }
    Ok(())
}

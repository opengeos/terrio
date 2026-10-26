//! `terrio lineref`: linear referencing, like GDAL's `ogrlineref`.

use terrio::formats::VectorDriver;
use terrio::ops::lineref::LineRefOptions;
use terrio::ops::vector_filter::{WriteMode, write_vector_with_mode};
use terrio::{Error, Layer, Result};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(clap::Subcommand)]
enum Mode {
    /// Cut a route into parts of equal position length from reference points with known
    /// positions (ogrlineref -create).
    Create(CreateArgs),
    /// The position of coordinates on the parts (ogrlineref -get_pos).
    GetPos(GetPosArgs),
    /// The coordinates of a position on the parts (ogrlineref -get_coord).
    GetCoord(GetCoordArgs),
    /// The route between two positions (ogrlineref -get_subline).
    GetSubline(GetSublineArgs),
}

/// A layer of a vector dataset.
fn open_layer(path: &str, layer: Option<&str>) -> Result<Layer> {
    let layers = super::quiet(|| {
        terrio::ops::vector_filter::open_layers(
            path,
            &terrio::VectorOpenOptions { layers: layer.into_iter().map(str::to_string).collect(), ..Default::default() },
        )
    })?;
    layers.into_iter().next().ok_or_else(|| Error::InvalidArgument(format!("{path} has no layer")))
}

/// The output options.
#[derive(clap::Args)]
struct Output {
    /// The output vector dataset.
    output: String,
    /// The output format's short name. By default, chosen from the output's extension.
    #[arg(short = 'f', long = "format")]
    format: Option<String>,
    /// The output layer name (ogrlineref -on). By default, the output file's name.
    #[arg(long = "layer-name", value_name = "NAME")]
    layer_name: Option<String>,
}

impl Output {
    fn name(&self) -> String {
        self.layer_name
            .clone()
            .unwrap_or_else(|| std::path::Path::new(&self.output).file_stem().and_then(|s| s.to_str()).unwrap_or("parts").to_string())
    }

    fn write(&self, layer: Layer) -> Result<()> {
        let driver = self
            .format
            .as_deref()
            .map(|f| VectorDriver::from_name(f).ok_or_else(|| Error::InvalidArgument(format!("unknown vector format '{f}'"))))
            .transpose()?;
        write_vector_with_mode(&[layer], &self.output, &terrio::VectorWriteOptions { driver, ..Default::default() }, WriteMode::Create)
    }
}

#[derive(clap::Args)]
struct CreateArgs {
    /// The route lines (ogrlineref -l): the first feature, or each route with --line-field.
    #[arg(long, value_name = "PATH")]
    lines: String,
    /// The layer of the route lines (-ln). By default, the first.
    #[arg(long = "lines-layer", value_name = "NAME")]
    lines_layer: Option<String>,
    /// The field identifying routes (-lf), with --point-field.
    #[arg(long = "line-field", value_name = "FIELD", requires = "point_field")]
    line_field: Option<String>,
    /// The reference points (repers, milestones) with known positions (-p).
    #[arg(long, value_name = "PATH")]
    points: String,
    /// The layer of the reference points (-pn). By default, the first.
    #[arg(long = "points-layer", value_name = "NAME")]
    points_layer: Option<String>,
    /// The field of the points' positions (-pm).
    #[arg(long = "position-field", value_name = "FIELD")]
    position_field: String,
    /// The field identifying each point's route (-pf), with --line-field.
    #[arg(long = "point-field", value_name = "FIELD", requires = "line_field")]
    point_field: Option<String>,
    /// The output field receiving route identifiers (-of).
    #[arg(long = "id-field", value_name = "FIELD", default_value = "uniq_uid")]
    id_field: String,
    /// The length of the parts, in position units (-s).
    #[arg(short = 's', long)]
    step: f64,
    #[command(flatten)]
    out: Output,
}

#[derive(clap::Args)]
struct PartsArgs {
    /// The parts, from `lineref create` or ogrlineref (-r).
    #[arg(long, value_name = "PATH")]
    parts: String,
    /// The layer of the parts (-rn). By default, the first.
    #[arg(long = "parts-layer", value_name = "NAME")]
    parts_layer: Option<String>,
}

#[derive(clap::Args)]
struct GetPosArgs {
    #[command(flatten)]
    parts: PartsArgs,
    /// The X coordinate (-x).
    #[arg(short = 'x', allow_negative_numbers = true)]
    x: f64,
    /// The Y coordinate (-y).
    #[arg(short = 'y', allow_negative_numbers = true)]
    y: f64,
    /// Print JSON with full precision, rather than ogrlineref -quiet's six decimals.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct GetCoordArgs {
    #[command(flatten)]
    parts: PartsArgs,
    /// The position (-m).
    #[arg(short = 'm', long, allow_negative_numbers = true)]
    position: f64,
    /// Print JSON with full precision, rather than ogrlineref -quiet's "x,y,z" with six decimals.
    #[arg(long)]
    json: bool,
}

#[derive(clap::Args)]
struct GetSublineArgs {
    #[command(flatten)]
    parts: PartsArgs,
    /// The first position (-mb).
    #[arg(long, allow_negative_numbers = true)]
    begin: f64,
    /// The last position (-me).
    #[arg(long, allow_negative_numbers = true)]
    end: f64,
    #[command(flatten)]
    out: Output,
}

pub fn run(a: Args) -> Result<()> {
    match a.mode {
        Mode::Create(a) => {
            let lines = open_layer(&a.lines, a.lines_layer.as_deref())?;
            let points = open_layer(&a.points, a.points_layer.as_deref())?;
            let options = LineRefOptions {
                position_field: a.position_field,
                step: a.step,
                line_id_field: a.line_field,
                point_id_field: a.point_field,
                output_id_field: a.id_field,
                layer_name: Some(a.out.name()),
            };
            let parts = terrio::ops::lineref::create_reference(&lines, &points, &options)?;
            a.out.write(parts)
        }
        Mode::GetPos(a) => {
            let parts = open_layer(&a.parts.parts, a.parts.parts_layer.as_deref())?;
            let p = terrio::ops::lineref::position(&parts, a.x, a.y)?;
            if a.json {
                println!("{}", serde_json::json!({ "position": p }));
            } else {
                println!("{p:.6}");
            }
            Ok(())
        }
        Mode::GetCoord(a) => {
            let parts = open_layer(&a.parts.parts, a.parts.parts_layer.as_deref())?;
            let coords = terrio::ops::lineref::coordinates(&parts, a.position)?;
            if a.json {
                println!("{}", serde_json::json!({ "coordinates": coords }));
            } else {
                for [x, y, z] in coords {
                    println!("{x:.6},{y:.6},{z:.6}");
                }
            }
            Ok(())
        }
        Mode::GetSubline(a) => {
            let parts = open_layer(&a.parts.parts, a.parts.parts_layer.as_deref())?;
            let out = terrio::ops::lineref::subline(&parts, a.begin, a.end, &a.out.name())?;
            a.out.write(out)
        }
    }
}

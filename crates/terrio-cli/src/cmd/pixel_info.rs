//! `terrio pixel-info` (also exposed automatically as an MCP tool).
use std::io::Read;
use terrio::ops::Resampling;
use terrio::ops::pixel_info::{IncludeFields, PixelInfoOptions, Position, PositionCrs, pixel_info, pixel_info_layer};
use terrio::{Error, Result};

#[derive(clap::Args)]
pub struct Args {
    /// The raster to query.
    input: String,
    /// Positions: column and line (or X and Y with --position-crs), as `X Y` pairs or `X,Y`. Read
    /// from standard input (an `X Y` pair per line, with any text after it reported as extra
    /// input) when neither these nor --position-dataset are given.
    #[arg(value_name = "X Y", allow_negative_numbers = true)]
    positions: Vec<String>,
    /// A position, `X,Y` (repeat for several).
    #[arg(short = 'p', long = "position", visible_alias = "pos", value_name = "X,Y", allow_negative_numbers = true)]
    position: Vec<String>,
    /// What positions are in: `pixel` (column, line; the default), `dataset` (the raster's CRS)
    /// or a CRS (`EPSG:4326`, `WGS84`, WKT...), X being the longitude or easting. For
    /// --position-dataset, the layer's CRS by default.
    #[arg(long, value_name = "pixel|dataset|CRS")]
    position_crs: Option<String>,
    /// A vector dataset whose points (or the centroids of other geometries) are the positions.
    #[arg(long, value_name = "PATH", conflicts_with_all = ["positions", "position"])]
    position_dataset: Option<String>,
    /// The layer of --position-dataset (required when it has several).
    #[arg(short = 'l', long = "layer", visible_alias = "input-layer")]
    layer: Option<String>,
    /// Fields of --position-dataset copied to the output: names, `ALL` (the default) or `NONE`.
    #[arg(long = "include-field", value_delimiter = ',')]
    include_fields: Vec<String>,
    /// Bands to query, from 1 (repeat, or comma-separated). By default, all bands.
    #[arg(short = 'b', long = "band", value_delimiter = ',')]
    bands: Vec<usize>,
    /// Query this overview (from 0) instead of full resolution; positions stay full-resolution.
    #[arg(long, visible_alias = "ovr")]
    overview: Option<usize>,
    /// nearest (the default), bilinear, cubic or cubicspline.
    #[arg(short = 'r', long, default_value = "nearest")]
    resampling: String,
    /// Put the pixel value as the Z of GeoJSON points (a single band).
    #[arg(long)]
    promote_pixel_value_to_z: bool,
    /// The output format: geojson (gdal raster pixel-info's default), csv, text
    /// (gdallocationinfo's report), value (gdallocationinfo -valonly) or json. With --output, a
    /// vector format's short name, by default from the output's extension.
    #[arg(short = 'f', long = "format", visible_alias = "of")]
    format: Option<String>,
    /// The separator of values for `--format value` (a line break by default).
    #[arg(long, value_name = "SEP")]
    field_sep: Option<String>,
    /// Write to this file rather than standard output: GeoJSON (`.json`, `.geojson`), CSV
    /// (`.csv`) or another vector format, as a layer of points.
    #[arg(short = 'o', long)]
    output: Option<String>,
}

/// Parses position arguments: numbers, or `X,Y` pairs.
fn parse_positions(values: &[String]) -> Result<Vec<Position>> {
    let mut numbers = Vec::new();
    for v in values {
        for part in v.split(',').filter(|p| !p.trim().is_empty()) {
            numbers.push(part.trim().parse::<f64>().map_err(|_| Error::InvalidArgument(format!("invalid position value '{part}'")))?);
        }
    }
    if numbers.len() % 2 != 0 {
        return Err(Error::InvalidArgument("An even number of values must be specified for 'position' argument".into()));
    }
    Ok(numbers.chunks(2).map(|c| Position::new(c[0], c[1])).collect())
}

pub fn run(a: Args) -> Result<()> {
    let aliases = |path: &str, input: &str| path == input || super::rasterout::same_file(path, input);
    if a.output.as_deref().is_some_and(|path| aliases(path, &a.input) || a.position_dataset.as_deref().is_some_and(|d| aliases(path, d))) {
        return Err(Error::InvalidArgument("the output would overwrite the input".into()));
    }
    let options = PixelInfoOptions {
        bands: a.bands.clone(),
        overview: a.overview,
        position_crs: a.position_crs.as_deref().map(PositionCrs::parse).transpose()?,
        resampling: match Resampling::parse(&a.resampling)? {
            r @ (Resampling::Nearest | Resampling::Bilinear | Resampling::Cubic | Resampling::CubicSpline) => r,
            _ => return Err(Error::InvalidArgument("--resampling must be nearest, bilinear, cubic or cubicspline".into())),
        },
        promote_pixel_value_to_z: a.promote_pixel_value_to_z,
        include_fields: IncludeFields::parse(&a.include_fields),
    };
    let mut raster = super::rasterout::open_lazy(&a.input)?;
    let info = if let Some(path) = &a.position_dataset {
        let open = terrio::VectorOpenOptions { layers: a.layer.iter().cloned().collect(), ..Default::default() };
        let ds = super::quiet(|| terrio::open_vector(path, &open))?;
        let layer = match (ds.layers.len(), &a.layer) {
            (0, _) => return Err(Error::InvalidArgument(format!("Dataset '{path}' has no vector layer"))),
            (_, Some(name)) => ds
                .layers
                .iter()
                .find(|l| &l.name == name)
                .ok_or_else(|| Error::InvalidArgument(format!("Cannot find layer '{name}' in '{path}'")))?,
            (1, None) => &ds.layers[0],
            _ => {
                return Err(Error::InvalidArgument(format!(
                    "Dataset '{path}' has more than one vector layer. Please specify the 'layer' argument"
                )));
            }
        };
        pixel_info_layer(&mut raster, layer, &options)?
    } else {
        let mut positions = parse_positions(&a.positions)?;
        positions.extend(parse_positions(&a.position)?);
        if positions.is_empty() {
            let mut text = String::new();
            std::io::stdin().read_to_string(&mut text)?;
            positions = Position::parse_lines(&text)?;
            if positions.is_empty() {
                return Err(Error::InvalidArgument(
                    "no positions: give X Y values, --position, --position-dataset or lines on standard input".into(),
                ));
            }
        }
        pixel_info(&mut raster, &positions, &options)?
    };
    let ext = a.output.as_deref().and_then(|p| std::path::Path::new(p).extension().and_then(|e| e.to_str())).map(str::to_ascii_lowercase);
    let format = match (a.format.as_deref().map(str::to_ascii_lowercase), ext.as_deref()) {
        (Some(f), _) => f,
        (None, Some("csv")) => "csv".into(),
        (None, Some("json" | "geojson")) | (None, None) => "geojson".into(),
        (None, Some(_)) => "vector".into(),
    };
    let text = match format.as_str() {
        "geojson" => info.to_geojson_string() + "\n",
        "csv" => info.to_csv(),
        "text" => info.to_text(),
        "value" | "valonly" => info.to_values(a.field_sep.as_deref().unwrap_or("\n")),
        "json" => serde_json::to_string_pretty(&info.to_json()).map_err(|e| Error::InvalidArgument(e.to_string()))? + "\n",
        other => {
            let Some(path) = &a.output else {
                return Err(Error::InvalidArgument(format!(
                    "unknown output format '{other}': use geojson, csv, text, value or json (other vector formats need --output)"
                )));
            };
            if path == "-" {
                return Err(Error::InvalidArgument(
                    "vector output on standard output is not supported; choose a text format or a file path".into(),
                ));
            }
            let driver = if other == "vector" {
                None
            } else {
                Some(
                    terrio::formats::VectorDriver::from_name(other)
                        .ok_or_else(|| Error::InvalidArgument(format!("unknown vector format '{other}'")))?,
                )
            };
            let name = info
                .layer_name
                .clone()
                .unwrap_or_else(|| std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("pixel_info").to_string());
            let options = terrio::VectorWriteOptions { driver, ..Default::default() };
            return terrio::write_vector(&[info.to_layer(&name)?], path, &options);
        }
    };
    match &a.output {
        Some(path) if path != "-" => std::fs::write(path, text)?,
        _ => {
            use std::io::Write;
            std::io::stdout().lock().write_all(text.as_bytes())?;
        }
    }
    // gdallocationinfo exits with 1 when a location is off the raster.
    if matches!(format.as_str(), "text" | "value" | "valonly") && info.off_raster_count() > 0 {
        super::set_exit_code(1);
    }
    Ok(())
}

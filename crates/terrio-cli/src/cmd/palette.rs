//! Indexed color conversion and palette attachment.
use super::rasterout::{OutputArgs, open_lazy};
use terrio::Result;
use terrio::ops::palette::{self, PaletteOptions};

#[derive(clap::Args)]
pub struct ImageArgs {
    /// Input raster.
    input: String,
    /// Output raster.
    output: String,
    /// Replace an existing output; input files and aliases cannot be replaced.
    #[arg(long)]
    overwrite: bool,
    #[command(flatten)]
    out: OutputArgs,
}
impl ImageArgs {
    fn check(&self, palette: Option<&str>) -> Result<()> {
        let mut paths = open_lazy(&self.input)?.file_list()?;
        paths.push(self.input.clone());
        if let Some(path) = palette {
            paths.extend(palette::color_table_files(path)?);
        }
        palette::check_output(&self.output, &paths.iter().map(String::as_str).collect::<Vec<_>>(), self.overwrite)
    }
    fn write_masked(&self, mut output: terrio::LazyRaster) -> Result<()> {
        let mut options = self.out.options()?;
        output.prepare_output_mask(&self.output, &mut options, terrio::Window::full(output.width(), output.height()), None)?;
        terrio::write_raster_dataset(&output.read()?, &self.output, &options)
    }
}
/// RGB-to-palette command options.
#[derive(clap::Args)]
pub struct RgbArgs {
    #[command(flatten)]
    image: ImageArgs,
    /// Number of generated colors.
    #[arg(short = 'n', long, default_value_t = 256)]
    color_count: usize,
    /// Palette raster or indexed TXT/CPT/QML/QLR color file instead of generation.
    #[arg(long)]
    color_map: Option<String>,
    /// Reserve a palette index for invalid source pixels (otherwise automatic).
    #[arg(long)]
    output_nodata: Option<u8>,
    /// Disable GDAL-compatible integer error diffusion.
    #[arg(long)]
    no_dither: bool,
    /// Component precision, 5 (default) or 8 bits.
    #[arg(long,default_value="5",value_parser=bits)]
    bit_depth: u8,
}
fn bits(s: &str) -> std::result::Result<u8, String> {
    match s {
        "5" => Ok(5),
        "8" => Ok(8),
        _ => Err("bit depth must be 5 or 8".into()),
    }
}
/// Runs RGB quantization.
pub fn rgb(a: RgbArgs) -> Result<()> {
    if a.color_map.is_none() && !(2..=256).contains(&a.color_count) {
        return Err(terrio::Error::InvalidArgument("palette needs 2..256 generated colors".into()));
    }
    a.image.check(a.color_map.as_deref())?;
    let options = PaletteOptions {
        color_count: a.color_count,
        bit_depth: a.bit_depth,
        dither: !a.no_dither,
        color_table: a.color_map.as_deref().map(palette::read_color_table).transpose()?,
        output_nodata: a.output_nodata,
        mask: None,
    };
    let output = palette::rgb_to_palette_lazy(&mut open_lazy(&a.image.input)?, &options)?;
    a.image.out.write(&output, &a.image.output)
}
/// Palette expansion command options.
#[derive(clap::Args)]
pub struct ExpandArgs {
    #[command(flatten)]
    image: ImageArgs,
    /// Output four RGBA bands, making pixels excluded by a dataset mask transparent.
    #[arg(long)]
    rgba: bool,
}
/// Runs palette expansion.
pub fn expand(a: ExpandArgs) -> Result<()> {
    a.image.check(None)?;
    let output = palette::palette_to_rgb_lazy(&mut open_lazy(&a.image.input)?, a.rgba)?;
    a.image.write_masked(output)
}
/// Palette attachment command options.
#[derive(clap::Args)]
pub struct AttachArgs {
    /// Palette raster or TXT/CPT/QML/QLR color file.
    palette: String,
    #[command(flatten)]
    image: ImageArgs,
}
/// Attaches a table without changing indices.
pub fn attach(a: AttachArgs) -> Result<()> {
    a.image.check(Some(&a.palette))?;
    let table = palette::read_color_table(&a.palette)?;
    let mut source = open_lazy(&a.image.input)?;
    a.image.write_masked(palette::attach_palette_lazy(&mut source, &table)?)
}

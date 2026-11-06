//! `terrio preview`: a picture of a dataset, as a PNG file or in the terminal.

use std::io::{IsTerminal, Write};
use terrio::ops::render::{self, Colormap, Image, RenderOptions, Renderer, Source, Stretch};
use terrio::{Error, Result};

/// How an image reaches the terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Display {
    /// Kitty's graphics protocol.
    Kitty,
    /// iTerm2's inline images.
    Iterm,
    /// Sixel graphics, in a fixed palette of 216 colors.
    Sixel,
    /// Unicode half blocks in 24-bit color, which most terminals show.
    Blocks,
}

/// Arguments of `terrio preview`.
#[derive(clap::Args)]
pub struct Args {
    /// Input raster or vector dataset (local, remote or archive member).
    pub input: String,
    /// Output PNG, or - for its bytes on standard output. Without it, draws in the terminal.
    #[arg(short = 'o', long)]
    pub output: Option<String>,
    /// Replace an existing output file.
    #[arg(long)]
    pub overwrite: bool,
    /// Image width in pixels. With only one of width and height, the other keeps the aspect ratio.
    #[arg(long)]
    pub width: Option<usize>,
    /// Image height in pixels.
    #[arg(long)]
    pub height: Option<usize>,
    /// Longest side when no width or height is given; images are not enlarged to reach it.
    #[arg(long, default_value_t = render::DEFAULT_MAX_SIZE)]
    pub max_size: usize,
    #[command(flatten)]
    pub style: Style,
    /// How to draw in the terminal. By default, detected from the environment, and refused when
    /// standard output is not a terminal.
    #[arg(long, value_parser = ["auto", "kitty", "iterm", "sixel", "blocks"], default_value = "auto")]
    pub display: String,
}

/// What to draw and in which colors, shared with `terrio view`.
#[derive(clap::Args)]
pub struct Style {
    /// One band, or three as red,green,blue. By default, from the color interpretation.
    #[arg(short = 'b', long, value_delimiter = ',')]
    pub bands: Vec<usize>,
    /// Colormap of one band or a vector attribute: gray, viridis, magma, inferno, plasma, terrain,
    /// rdbu or rdylgn; add _r to reverse.
    #[arg(short = 'c', long)]
    pub colormap: Option<String>,
    /// Value range: auto, minmax, pLOW,pHIGH (percentiles) or LOW,HIGH (values).
    #[arg(short = 's', long, default_value = "auto", allow_hyphen_values = true)]
    pub stretch: String,
    /// Vector: draw this layer. By default, all layers, which must share a CRS.
    #[arg(short = 'l', long)]
    pub layer: Vec<String>,
    /// Vector: color features by this numeric field.
    #[arg(short = 'a', long)]
    pub attribute: Option<String>,
}

impl Style {
    /// The drawing options these arguments ask for.
    pub fn options(&self) -> Result<RenderOptions> {
        Ok(RenderOptions {
            bands: self.bands.clone(),
            colormap: self.colormap.as_deref().map(Colormap::parse).transpose()?,
            stretch: Stretch::parse(&self.stretch)?,
            attribute: self.attribute.clone(),
            ..Default::default()
        })
    }
}

/// Opens `input` for drawing again and again.
pub fn renderer(input: &str, style: &Style) -> Result<Renderer> {
    let options = style.options()?;
    match open(input, style)? {
        Source::Raster(raster) => Renderer::raster(raster, &options),
        Source::Vector(layers) => Renderer::vector(layers, &options),
    }
}

/// Opens the dataset at `input`, quietly: driver messages of the raster attempt are not news if
/// the dataset turns out to be a vector one.
fn open(input: &str, style: &Style) -> Result<Source> {
    let vector_options = style.attribute.is_some() || !style.layer.is_empty();
    super::quiet(|| render::open_source(input, &style.layer, vector_options))
}

/// Runs `terrio preview`.
pub fn run(mut args: Args) -> Result<()> {
    let display = match args.output.as_deref() {
        Some(_) if args.display != "auto" => {
            return Err(Error::InvalidArgument("--display draws in the terminal: leave it out with -o".into()));
        }
        Some("-") if std::io::stdout().is_terminal() => {
            return Err(Error::InvalidArgument("standard output is a terminal: pipe the PNG bytes, or give -o FILE".into()));
        }
        Some(_) => None,
        None => {
            if args.display == "auto" && !std::io::stdout().is_terminal() {
                return Err(Error::InvalidArgument("standard output is not a terminal: give -o FILE, or -o - for PNG bytes".into()));
            }
            Some(choose_display(&args.display, &|name| std::env::var(name).ok()))
        }
    };
    // A terminal's columns limit block drawing, so read no more than they show.
    if display == Some(Display::Blocks) && args.width.is_none() && args.height.is_none() {
        args.max_size = args.max_size.min(terminal_columns());
    }
    if let Some(path) = args.output.as_deref().filter(|p| *p != "-") {
        let extension = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("");
        if !extension.eq_ignore_ascii_case("png") {
            return Err(Error::InvalidArgument(format!("the output is a PNG: name it .png, not '{path}'")));
        }
        terrio::ops::palette::check_output(path, &[args.input.as_str()], args.overwrite)?;
    }
    let options = RenderOptions { width: args.width, height: args.height, max_size: args.max_size, ..args.style.options()? };
    let image = match open(&args.input, &args.style)? {
        Source::Raster(mut raster) => render::render_raster(&mut raster, &options)?,
        Source::Vector(layers) => render::render_vector(layers, &options)?,
    };
    match (args.output.as_deref(), display) {
        (Some("-"), _) => write_stdout(&image.to_png()?),
        (Some(path), _) => {
            // Encoded first, so that a failure to encode leaves no file behind.
            let png = image.to_png()?;
            if args.overwrite { replace(path, &png) } else { create(path, &png) }
        }
        (None, Some(display)) => write_stdout(&encode(&image, display)?),
        (None, None) => unreachable!("no output means a display"),
    }
}

/// Writes a new file, refusing one that appeared since the check. A failed write removes it.
fn create(path: &str, png: &[u8]) -> Result<()> {
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    let written = file.write_all(png);
    if written.is_err() {
        drop(file);
        let _ = std::fs::remove_file(path);
    }
    written.map_err(Error::from)
}

/// Writes beside `path` and renames over it, so a failed write leaves any earlier file as it was.
fn replace(path: &str, png: &[u8]) -> Result<()> {
    let target = std::path::Path::new(path);
    let name = target.file_name().ok_or_else(|| Error::InvalidArgument(format!("{path}: not a file name")))?;
    // WASI has no process ids (`std::process::id` panics there); one program runs at a time.
    #[cfg(target_os = "wasi")]
    let pid = 0;
    #[cfg(not(target_os = "wasi"))]
    let pid = std::process::id();
    let temporary = target.with_file_name(format!(".{}.{pid}.tmp", name.to_string_lossy()));
    create(&temporary.to_string_lossy(), png)?;
    std::fs::rename(&temporary, target).inspect_err(|_| {
        let _ = std::fs::remove_file(&temporary);
    })?;
    Ok(())
}

fn write_stdout(bytes: &[u8]) -> Result<()> {
    let mut out = std::io::stdout().lock();
    out.write_all(bytes)?;
    out.flush()?;
    Ok(())
}

/// The way to draw, given the `--display` value and the environment. tmux and screen do not pass
/// graphics on without configuration, so inside them it is blocks unless asked otherwise.
pub fn choose_display(requested: &str, env: &dyn Fn(&str) -> Option<String>) -> Display {
    match requested {
        "kitty" => return Display::Kitty,
        "iterm" => return Display::Iterm,
        "sixel" => return Display::Sixel,
        "blocks" => return Display::Blocks,
        _ => {}
    }
    let term = env("TERM").unwrap_or_default();
    let program = env("TERM_PROGRAM").unwrap_or_default();
    if env("TMUX").is_some() || term.starts_with("screen") || term.starts_with("tmux") {
        Display::Blocks
    } else if env("KITTY_WINDOW_ID").is_some() || term == "xterm-kitty" || program == "ghostty" {
        Display::Kitty
    } else if matches!(program.as_str(), "iTerm.app" | "WezTerm") {
        Display::Iterm
    } else if term.contains("sixel") || term == "foot" || term == "mlterm" {
        Display::Sixel
    } else {
        Display::Blocks
    }
}

/// The terminal's width in cells: `COLUMNS`, else what `stty size` reports for the controlling
/// terminal, else 80. (`stty` rather than the `TIOCGWINSZ` ioctl, which needs `unsafe`.)
fn terminal_columns() -> usize {
    let positive = |text: &str| text.trim().parse::<usize>().ok().filter(|v| *v > 0);
    if let Some(columns) = std::env::var("COLUMNS").ok().as_deref().and_then(positive) {
        return columns;
    }
    #[cfg(unix)]
    if let Ok(tty) = std::fs::File::open("/dev/tty")
        && let Ok(output) = std::process::Command::new("stty").arg("size").stdin(tty).stderr(std::process::Stdio::null()).output()
        && let Some(columns) = String::from_utf8_lossy(&output.stdout).split_whitespace().nth(1).and_then(positive)
    {
        return columns;
    }
    80
}

/// The bytes that draw `image` in a terminal.
pub fn encode(image: &Image, display: Display) -> Result<Vec<u8>> {
    Ok(match display {
        Display::Kitty => kitty(&image.to_png()?),
        Display::Iterm => iterm(&image.to_png()?),
        Display::Sixel => sixel(image),
        Display::Blocks => blocks(image),
    })
}

/// Base64, as `crate::mcp` and the terminal protocols send binary data.
pub(crate) fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = u32::from(chunk[0]) << 16 | u32::from(*chunk.get(1).unwrap_or(&0)) << 8 | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
    }
    out
}

/// A PNG in Kitty's graphics protocol, in the chunks of up to 4096 bytes it requires.
fn kitty(png: &[u8]) -> Vec<u8> {
    let encoded = base64(png);
    let mut out = Vec::with_capacity(encoded.len() + encoded.len() / 128 + 64);
    let chunks: Vec<&[u8]> = encoded.as_bytes().chunks(4096).collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < chunks.len());
        let head = if i == 0 { format!("\x1b_Gf=100,a=T,q=2,m={more};") } else { format!("\x1b_Gm={more};") };
        out.extend_from_slice(head.as_bytes());
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out.push(b'\n');
    out
}

/// A PNG as iTerm2's inline image.
fn iterm(png: &[u8]) -> Vec<u8> {
    format!("\x1b]1337;File=inline=1;size={};preserveAspectRatio=1:{}\x07\n", png.len(), base64(png)).into_bytes()
}

/// Colored half blocks: each cell shows two pixels, its upper one in the foreground color.
fn blocks(image: &Image) -> Vec<u8> {
    let mut out = String::new();
    let pixel = |col: usize, row: usize| -> Option<&[u8]> {
        let at = (row * image.width + col) * 4;
        let p = &image.rgba[at..at + 4];
        (p[3] >= 128).then_some(p)
    };
    for row in (0..image.height).step_by(2) {
        for col in 0..image.width {
            let upper = pixel(col, row);
            let lower = if row + 1 < image.height { pixel(col, row + 1) } else { None };
            match (upper, lower) {
                (None, None) => out.push_str("\x1b[0m "),
                (Some(u), None) => out.push_str(&format!("\x1b[0;38;2;{};{};{}m▀", u[0], u[1], u[2])),
                (None, Some(l)) => out.push_str(&format!("\x1b[0;38;2;{};{};{}m▄", l[0], l[1], l[2])),
                (Some(u), Some(l)) => out.push_str(&format!("\x1b[38;2;{};{};{};48;2;{};{};{}m▀", u[0], u[1], u[2], l[0], l[1], l[2])),
            }
        }
        out.push_str("\x1b[0m\n");
    }
    out.into_bytes()
}

/// A sixel image in a 6x6x6 color cube; transparent pixels are left as they are on screen.
fn sixel(image: &Image) -> Vec<u8> {
    let mut out = format!("\x1bP0;1;0q\"1;1;{};{}", image.width, image.height);
    let levels = [0u32, 20, 40, 60, 80, 100];
    for i in 0..216 {
        out.push_str(&format!("#{i};2;{};{};{}", levels[i / 36], levels[i / 6 % 6], levels[i % 6]));
    }
    let index = |col: usize, row: usize| -> Option<usize> {
        let at = (row * image.width + col) * 4;
        let p = &image.rgba[at..at + 4];
        let level = |v: u8| (usize::from(v) * 5 + 127) / 255;
        (p[3] >= 128).then(|| level(p[0]) * 36 + level(p[1]) * 6 + level(p[2]))
    };
    for top in (0..image.height).step_by(6) {
        let rows = 6.min(image.height - top);
        let mut used = [false; 216];
        for row in top..top + rows {
            for col in 0..image.width {
                if let Some(i) = index(col, row) {
                    used[i] = true;
                }
            }
        }
        for color in (0..216).filter(|&c| used[c]) {
            out.push_str(&format!("#{color}"));
            let (mut run, mut last) = (0usize, None);
            let flush = |out: &mut String, run: usize, last: Option<u8>| {
                if let Some(c) = last {
                    if run > 3 {
                        out.push_str(&format!("!{run}{}", c as char));
                    } else {
                        (0..run).for_each(|_| out.push(c as char));
                    }
                }
            };
            for col in 0..image.width {
                let bits = (0..rows).filter(|r| index(col, top + r) == Some(color)).fold(0u8, |b, r| b | 1 << r);
                let c = 63 + bits;
                if last == Some(c) {
                    run += 1;
                } else {
                    flush(&mut out, run, last);
                    (run, last) = (1, Some(c));
                }
            }
            flush(&mut out, run, last);
            out.push('$');
        }
        out.push('-');
    }
    out.push_str("\x1b\\\n");
    out.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Image {
        // Red, transparent; blue, green.
        Image { width: 2, height: 2, rgba: vec![255, 0, 0, 255, 0, 0, 0, 0, 0, 0, 255, 255, 0, 255, 0, 255] }
    }

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |name| pairs.iter().find(|(n, _)| *n == name).map(|(_, v)| (*v).to_string())
    }

    #[test]
    fn the_display_follows_the_request_then_the_environment() {
        assert_eq!(choose_display("sixel", &env(&[("TERM", "xterm-kitty")])), Display::Sixel);
        assert_eq!(choose_display("auto", &env(&[("TERM", "xterm-kitty")])), Display::Kitty);
        assert_eq!(choose_display("auto", &env(&[("TERM_PROGRAM", "iTerm.app")])), Display::Iterm);
        assert_eq!(choose_display("auto", &env(&[("TERM", "foot")])), Display::Sixel);
        assert_eq!(choose_display("auto", &env(&[("TERM", "xterm-256color")])), Display::Blocks);
        assert_eq!(choose_display("auto", &env(&[("TERM", "xterm-kitty"), ("TMUX", "x")])), Display::Blocks);
    }

    #[test]
    fn base64_pads_as_the_standard_says() {
        assert_eq!(base64(b"Man"), "TWFu");
        assert_eq!(base64(b"Ma"), "TWE=");
        assert_eq!(base64(b"M"), "TQ==");
        assert_eq!(base64(b""), "");
    }

    #[test]
    fn kitty_splits_its_payload_into_chunks() {
        let out = String::from_utf8(kitty(&vec![7u8; 5000])).unwrap();
        assert!(out.starts_with("\x1b_Gf=100,a=T,q=2,m=1;"));
        assert!(out.contains("\x1b_Gm=0;"));
        assert!(out.split("\x1b\\").all(|part| part.len() <= 4096 + 32));
    }

    #[test]
    fn blocks_draw_two_pixels_a_cell_and_leave_transparency_blank() {
        let out = String::from_utf8(blocks(&image())).unwrap();
        assert_eq!(out.lines().count(), 1);
        assert!(out.contains("\x1b[38;2;255;0;0;48;2;0;0;255m▀"));
        assert!(out.contains("\x1b[0;38;2;0;255;0m▄"));
        assert!(out.ends_with("\x1b[0m\n"));
    }

    #[test]
    fn sixel_uses_a_cube_color_per_pixel_and_terminates() {
        let out = String::from_utf8(sixel(&image())).unwrap();
        assert!(out.starts_with("\x1bP0;1;0q\"1;1;2;2"));
        assert!(out.ends_with("\x1b\\\n"));
        // Red is cube entry 180, on the top row: a sixel with its lowest bit set, then none.
        assert!(out.contains("#180@?$"), "{out}");
        assert!(out.contains("#30?A$"), "{out}");
    }
}

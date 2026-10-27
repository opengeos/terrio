//! The subcommands.

pub mod calc;
pub mod catalog;
pub mod clean_collar;
pub mod compare;
pub mod convert;
pub mod crs;
pub mod dem;
pub mod enhance;
pub mod fill_nodata;
pub mod filter;
pub mod formats;
pub mod grid;
pub mod info;
pub mod lineref;
pub mod mosaic;
pub mod multidim;
pub mod neighbors;

pub mod pansharpen;
pub mod pixel;
pub mod pixel_info;
pub mod points;
pub mod polygonize;
pub mod processing;
pub mod proximity;
pub mod rasterize;
pub mod rasterout;
pub mod sieve;
pub mod translate;
pub mod vector_dataset;
pub mod vector_geom;
pub mod vector_grid;
pub mod vector_opts;
pub mod viewshed;
pub mod warp;
pub mod xyz;

thread_local! {
    /// The exit status of a command that succeeded, when it is not 0 (`terrio compare`'s
    /// difference count, `terrio pixel-info`'s locations off the raster).
    static EXIT_CODE: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// Sets the exit status of the command running on this thread, should it succeed.
pub fn set_exit_code(code: i32) {
    EXIT_CODE.with(|c| c.set(code));
}

/// Takes the exit status set by [`set_exit_code`], resetting it to 0.
pub fn take_exit_code() -> i32 {
    EXIT_CODE.with(|c| c.replace(0))
}

/// Runs `f` with standard output discarded, so that drivers' diagnostic prints do not mix with the
/// command's output.
pub fn quiet<T>(f: impl FnOnce() -> T) -> T {
    let _gag = gag::Gag::stdout().ok();
    f()
}

/// The largest file list [`expand_file_lists`] reads: about a million paths.
pub const MAX_FILE_LIST: u64 = 64 << 20;

/// The inputs with each `@FILE` replaced by the paths listed in `FILE`, one per line, as GDAL's
/// commands read `--input @filelist.txt`. Lines are trimmed and blank lines skipped; listed paths
/// are taken as written (relative ones from the working directory), not expanded again. A list
/// must be a regular file of at most [`MAX_FILE_LIST`] bytes, so that `@/dev/zero` or a huge file
/// is refused instead of read without end.
///
/// MCP tool calls (`TERRIO_MCP_TOOL` set) refuse lists before touching the file: errors name the
/// inputs that fail to open, so a client could otherwise read any text file back line by line.
/// They pass their inputs as an array instead.
pub fn expand_file_lists(inputs: &[String]) -> terrio::Result<Vec<String>> {
    use std::io::Read;
    let mut out = Vec::with_capacity(inputs.len());
    for input in inputs {
        let Some(list) = input.strip_prefix('@') else {
            out.push(input.clone());
            continue;
        };
        if std::env::var_os("TERRIO_MCP_TOOL").is_some() {
            return Err(terrio::Error::InvalidArgument(
                "@FILE lists are not read in MCP tool calls: pass each input as an argument (name a file beginning with @ as ./@name)"
                    .into(),
            ));
        }
        let meta = std::fs::metadata(list).map_err(|e| unreadable(list, e))?;
        if !meta.is_file() {
            return Err(unreadable(list, "not a regular file"));
        }
        if meta.len() > MAX_FILE_LIST {
            return Err(unreadable(list, format_args!("larger than {} MiB", MAX_FILE_LIST >> 20)));
        }
        // The size is checked again on the bytes read: the file may have grown since `metadata`,
        // and a pseudo-file reports a size of 0 while it yields data without end.
        let mut text = String::new();
        let read = std::fs::File::open(list).and_then(|f| f.take(MAX_FILE_LIST + 1).read_to_string(&mut text));
        match read {
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => return Err(unreadable(list, "not UTF-8 text (save it as UTF-8)")),
            Err(e) => return Err(unreadable(list, e)),
            Ok(_) if text.len() as u64 > MAX_FILE_LIST => {
                return Err(unreadable(list, format_args!("larger than {} MiB", MAX_FILE_LIST >> 20)));
            }
            Ok(_) => {}
        }
        let before = out.len();
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        out.extend(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from));
        if out.len() == before {
            return Err(terrio::Error::InvalidArgument(format!("the file list {list} names no inputs")));
        }
    }
    Ok(out)
}

/// The error for an `@FILE` input whose list cannot be used, because of `why`.
fn unreadable(list: &str, why: impl std::fmt::Display) -> terrio::Error {
    terrio::Error::InvalidArgument(format!("cannot read the file list {list}: {why}"))
}

/// The input path to open with `driver`: `gdal` (any case) forces the GDAL backend, which needs
/// a build with the `gdal` feature; other names leave the path to terrio's format detection.
pub fn with_driver(input: &str, driver: Option<&str>) -> terrio::Result<String> {
    match driver {
        Some(d) if d.eq_ignore_ascii_case("gdal") => {
            if cfg!(feature = "gdal") {
                Ok(format!("GDAL:{input}"))
            } else {
                Err(terrio::Error::Unsupported("this terrio was built without the GDAL backend (the `gdal` feature)".into()))
            }
        }
        _ => Ok(input.to_string()),
    }
}

// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/index.rst
// No code is translated from these sources.

//! The subcommands.

pub mod calc;
pub mod catalog;
pub mod clean_collar;
pub mod compare;
pub mod convert;
pub mod create;
pub mod crs;
pub mod dataset_files;
pub mod dem;
pub mod edit;
pub mod enhance;
pub mod fill_nodata;
pub mod filter;
pub mod formats;
pub mod grid;
pub mod info;
pub mod inspection;
pub mod lineref;
pub mod mosaic;
pub mod multidim;
pub mod neighbors;
pub mod overviews;
pub mod parquet_metadata;

pub mod palette;
pub mod pansharpen;
pub mod pixel;
pub mod pixel_info;
pub mod points;
pub mod polygonize;
pub mod preview;
pub mod processing;
pub mod proximity;
pub mod raster_retile;
pub mod raster_tile;
pub mod rasterize;
pub mod rasterout;
pub mod sieve;
pub mod tile_index;
pub mod translate;
pub mod update;
pub mod validate_cog;
pub mod vector_clip;
pub mod vector_dataset;
pub mod vector_geom;
pub mod vector_grid;
pub mod vector_opts;
pub mod viewshed;
pub mod vsi;
pub mod warp;
pub mod xyz;

thread_local! {
    /// The exit status of a command that succeeded, when it is not 0 (`terrio compare`'s
    /// difference count, `terrio pixel-info`'s locations off the raster).
    static EXIT_CODE: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// Parses an optional CRS argument.
pub fn srs(s: &Option<String>) -> terrio::Result<Option<terrio::crs::SpatialRef>> {
    s.as_deref().map(terrio::crs::SpatialRef::parse).transpose()
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
/// command's output. WASI (the browser build) has no descriptors to redirect, so there `f` prints.
pub fn quiet<T>(f: impl FnOnce() -> T) -> T {
    #[cfg(not(target_os = "wasi"))]
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
///
/// An input given directly whose file name holds `*` or `?` is replaced by the entries of its
/// folder that match, in name order (see [`expand_wildcards`]), for the terrio UI, MCP clients and
/// Windows shells, which do not expand wildcards themselves.
pub fn expand_file_lists(inputs: &[String]) -> terrio::Result<Vec<String>> {
    let mut out = Vec::with_capacity(inputs.len());
    for input in inputs {
        let Some(list) = input.strip_prefix('@') else {
            match expand_wildcards(input)? {
                Some(matches) => out.extend(matches),
                None => out.push(input.clone()),
            }
            continue;
        };
        let text = read_list_text(list, "@FILE lists")?;
        let before = out.len();
        out.extend(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from));
        if out.len() == before {
            return Err(terrio::Error::InvalidArgument(format!("the file list {list} names no inputs")));
        }
    }
    Ok(out)
}

/// The text of a list file named on the command line (`@FILE`, `--inputs FILE`): a regular UTF-8
/// file of at most [`MAX_FILE_LIST`] bytes, without a byte-order mark. MCP tool calls refuse
/// `what` before touching the file, since errors would echo its lines.
pub fn read_list_text(list: &str, what: &str) -> terrio::Result<String> {
    use std::io::Read;
    if std::env::var_os("TERRIO_MCP_TOOL").is_some() {
        let hint = if what.starts_with("--inputs") {
            "pass each raster as a --raster argument"
        } else {
            "pass each input as an argument (name a file beginning with @ as ./@name)"
        };
        return Err(terrio::Error::InvalidArgument(format!("{what} are not read in MCP tool calls: {hint}")));
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
    Ok(text.strip_prefix('\u{feff}').map(String::from).unwrap_or(text))
}

/// The local files matching `input` when its file name (the part after the last separator) holds
/// the wildcards `*` (any run of characters) or `?` (one character); `None` for any other input.
///
/// Only regular files match (symbolic links are followed), not folders: a folder dataset such as a
/// Zarr store is named directly. As in a POSIX shell, names beginning with `.` match only a pattern
/// that does too, and the matches are sorted by name. A matching name that is not valid Unicode is
/// an error rather than left out, as is a failure while listing the folder, so that a pattern never
/// stands for part of its matches. Wildcards in folder names are not expanded. Matching ignores case on
/// Windows, as its file systems do. An input that exists as written, URLs (`://`) and `/vsi` paths
/// are taken literally, so that `?` in a query string or a file name is left alone. A pattern that
/// matches nothing is an error, rather than being passed on to fail to open.
pub fn expand_wildcards(input: &str) -> terrio::Result<Option<Vec<String>>> {
    let separator = if cfg!(windows) { &['/', '\\'][..] } else { &['/'][..] };
    let split = input.rfind(separator).map_or(0, |i| i + 1);
    let (folder, pattern) = input.split_at(split);
    if !pattern.contains(['*', '?']) || input.contains("://") || input.starts_with("/vsi") || std::path::Path::new(input).exists() {
        return Ok(None);
    }
    let dir = std::path::Path::new(if folder.is_empty() { "." } else { folder });
    let entries =
        std::fs::read_dir(dir).map_err(|e| terrio::Error::InvalidArgument(format!("cannot list {} for {input}: {e}", dir.display())))?;
    let fold = |s: &str| if cfg!(windows) { s.to_lowercase() } else { s.to_string() };
    let pattern_chars: Vec<char> = fold(pattern).chars().collect();
    let listing_error = |e: std::io::Error| terrio::Error::InvalidArgument(format!("cannot list {} for {input}: {e}", dir.display()));
    let mut names = Vec::new();
    for entry in entries {
        let entry = entry.map_err(listing_error)?;
        let os_name = entry.file_name();
        let lossy = os_name.to_string_lossy();
        if (lossy.starts_with('.') && !pattern.starts_with('.'))
            || !wildcard_match(&pattern_chars, &fold(&lossy).chars().collect::<Vec<_>>())
            || !std::fs::metadata(entry.path()).is_ok_and(|m| m.is_file())
        {
            continue;
        }
        match os_name.into_string() {
            Ok(name) => names.push(name),
            Err(name) => {
                return Err(terrio::Error::InvalidArgument(format!(
                    "{} matches {input} but its name is not valid Unicode: name it on its own",
                    dir.join(name).display()
                )));
            }
        }
    }
    if names.is_empty() {
        return Err(terrio::Error::InvalidArgument(format!("no files match {input}")));
    }
    names.sort();
    Ok(Some(names.into_iter().map(|name| format!("{folder}{name}")).collect()))
}

/// Whether `name` matches `pattern`, where `*` stands for any run of characters and `?` for one.
fn wildcard_match(pattern: &[char], name: &[char]) -> bool {
    // Greedy matching that backtracks to the last `*` only: linear in practice, and quadratic at
    // worst, never exponential.
    let (mut p, mut n) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some('*') => {
                star = Some((p, n));
                p += 1;
            }
            Some(&c) if c == '?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((sp, sn)) => {
                    p = sp + 1;
                    n = sn + 1;
                    star = Some((sp, sn + 1));
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == '*')
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

pub mod gpkg;

pub mod pipeline;

pub mod rpftoc;

#[cfg(test)]
mod tests {
    use super::wildcard_match;

    fn matches(pattern: &str, name: &str) -> bool {
        wildcard_match(&pattern.chars().collect::<Vec<_>>(), &name.chars().collect::<Vec<_>>())
    }

    #[test]
    fn wildcards_match_runs_and_single_characters() {
        assert!(matches("*.tif", "a.tif"));
        assert!(matches("*.tif", ".tif"));
        assert!(!matches("*.tif", "a.tiff"));
        assert!(matches("*.tif*", "a.tiff"));
        assert!(matches("tile_??.tif", "tile_01.tif"));
        assert!(!matches("tile_??.tif", "tile_1.tif"));
        assert!(matches("*_*_b4.tif", "lc08_2024_b4.tif"));
        assert!(!matches("*_*_b4.tif", "lc08_b4.tif"));
        assert!(matches("*", ""));
        assert!(matches("a*b*c", "aXbYbZc"));
        // Non-ASCII names match by character, not by byte.
        assert!(matches("??.tif", "éè.tif"));
        // Backtracking stays quadratic at worst.
        let long = "a".repeat(5000);
        assert!(!matches("*a*a*a*a*a*b", &long));
    }
}

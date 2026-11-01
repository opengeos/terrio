// Written to match the behavior of:
//   GDAL v3.13.3:
//     doc/source/programs/gdal_vsi.rst
//     doc/source/programs/gdal_vsi_sozip.rst
// No code is translated from these sources.

//! `terrio vsi`: file management on local, in-memory, remote and archive locations (GDAL's
//! `gdal vsi`).

use terrio::Error;
use terrio::vsi::{self, CopyOptions, ListOptions, Report, SyncOptions};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
#[command(next_display_order = None)]
enum Command {
    /// List a directory or describe a file.
    ///
    /// Supports local, /vsimem/, S3, GCS, Azure, HTTP index pages, zip and tar archives (like `gdal
    /// vsi list`).
    #[command(visible_alias = "ls")]
    List(ListArgs),
    /// Copy a file or directory between locations.
    ///
    /// Directories need -r; archive members can be copied out (like `gdal vsi copy`).
    #[command(visible_alias = "cp")]
    Copy(CopyArgs),
    /// Delete a file or directory.
    ///
    /// Directories and their contents need -r (like `gdal vsi delete`). Refuses empty paths, roots,
    /// the home and current directories, whole buckets, archives and HTTP.
    #[command(visible_aliases = ["rm", "rmdir", "del"])]
    Delete(DeleteArgs),
    /// Move or rename a file or directory.
    ///
    /// The source is deleted only once it was copied in full (like `gdal vsi move`).
    #[command(visible_aliases = ["mv", "ren", "rename"])]
    Move(MoveArgs),
    /// Copy files missing or out of date in the target.
    ///
    /// Nothing is deleted (like `gdal vsi sync`).
    Sync(SyncArgs),
    /// Seek-optimized ZIP (SOZip 1.0).
    ///
    /// Create, list, optimize, validate (like `gdal vsi sozip`).
    Sozip(SozipArgs),
}

#[derive(clap::Args)]
struct SozipArgs {
    #[command(subcommand)]
    command: SozipCommand,
}

#[derive(clap::Subcommand)]
#[command(next_display_order = None)]
enum SozipCommand {
    /// Create a SOZip archive, or add files to an existing one.
    ///
    /// Like `gdal vsi sozip create`.
    Create(SozipCreateArgs),
    /// List an archive's members with their SOZip status.
    ///
    /// Like `gdal vsi sozip list`.
    List(SozipListArgs),
    /// Rewrite a zip archive as a SOZip archive.
    ///
    /// Like `gdal vsi sozip optimize`.
    Optimize(SozipOptimizeArgs),
    /// Check an archive and its SOZip indexes.
    ///
    /// Exits with status 1 when invalid (like `gdal vsi sozip validate`).
    Validate(SozipValidateArgs),
}

#[derive(clap::Args)]
struct SozipWriteArgs {
    /// Replace an existing output (create adds to it otherwise; optimize refuses it).
    #[arg(long)]
    overwrite: bool,
    /// When to seek-optimize a file: auto (files of at least --sozip-min-file-size), yes (every
    /// file larger than a chunk) or no.
    #[arg(long, default_value = "auto")]
    enable_sozip: String,
    /// The uncompressed size of a chunk, in bytes or with a K, M or G suffix.
    #[arg(long, default_value = "32768")]
    sozip_chunk_size: String,
    /// The smallest file seek-optimized in auto mode, in bytes or with a K, M or G suffix.
    #[arg(long, default_value = "1M")]
    sozip_min_file_size: String,
    /// Print nothing on success.
    #[arg(short = 'q', long)]
    quiet: bool,
}

#[derive(clap::Args)]
struct SozipCreateArgs {
    /// The files (or directories, with -r) to add, then the output archive.
    #[arg(required = true, num_args = 2.., value_names = ["INPUT", "OUTPUT"])]
    paths: Vec<String>,
    /// Add the files of directories, recursively.
    #[arg(short = 'r', long)]
    recursive: bool,
    /// Store files by their names alone, without directories.
    #[arg(short = 'j', long = "junk-paths", visible_alias = "no-paths")]
    junk_paths: bool,
    /// A Content-Type stored with each file added.
    #[arg(long)]
    content_type: Option<String>,
    #[command(flatten)]
    write: SozipWriteArgs,
}

#[derive(clap::Args)]
struct SozipOptimizeArgs {
    /// The zip archive to rewrite.
    input: String,
    /// The SOZip archive to write.
    output: String,
    #[command(flatten)]
    write: SozipWriteArgs,
}

#[derive(clap::Args)]
struct SozipListArgs {
    /// The zip archive.
    input: String,
    /// JSON output.
    #[arg(long)]
    json: bool,
    /// Print the HTTP requests sent and bytes received to standard error.
    #[arg(long)]
    stats: bool,
}

#[derive(clap::Args)]
struct SozipValidateArgs {
    /// The zip archive.
    input: String,
    /// Print each check.
    #[arg(short = 'v', long)]
    verbose: bool,
    /// JSON output.
    #[arg(long)]
    json: bool,
}

/// A size in bytes, or with a K, M or G suffix (powers of 1024).
fn parse_size(s: &str) -> terrio::Result<u64> {
    let t = s.trim();
    let (num, mult) = match t.chars().last().map(|c| c.to_ascii_uppercase()) {
        Some('K') => (&t[..t.len() - 1], 1u64 << 10),
        Some('M') => (&t[..t.len() - 1], 1 << 20),
        Some('G') => (&t[..t.len() - 1], 1 << 30),
        _ => (t, 1),
    };
    num.trim().parse::<u64>().ok().and_then(|n| n.checked_mul(mult)).ok_or_else(|| Error::InvalidArgument(format!("invalid size '{s}'")))
}

fn sozip_options(w: &SozipWriteArgs) -> terrio::Result<vsi::sozip::SozipOptions> {
    let chunk = parse_size(&w.sozip_chunk_size)?;
    let chunk =
        u32::try_from(chunk).ok().filter(|c| *c > 0).ok_or_else(|| Error::InvalidArgument("--sozip-chunk-size: 1 byte to 4 GiB".into()))?;
    Ok(vsi::sozip::SozipOptions {
        enable: w.enable_sozip.parse()?,
        chunk_size: chunk,
        min_file_size: parse_size(&w.sozip_min_file_size)?,
        overwrite: w.overwrite,
        ..Default::default()
    })
}

fn sozip(args: SozipArgs) -> terrio::Result<()> {
    use vsi::sozip;
    match args.command {
        SozipCommand::Create(a) => {
            let (output, inputs) = a.paths.split_last().ok_or_else(|| Error::InvalidArgument("no output".into()))?;
            let inputs: Vec<&str> = inputs.iter().map(String::as_str).collect();
            let options = sozip::SozipOptions {
                recursive: a.recursive,
                junk_paths: a.junk_paths,
                content_type: a.content_type.clone(),
                ..sozip_options(&a.write)?
            };
            let r = sozip::create(&inputs, output, &options)?;
            if !a.write.quiet {
                eprintln!("terrio: {} file(s) added, {} seek-optimized ({} bytes)", r.added, r.seek_optimized, r.bytes);
            }
        }
        SozipCommand::Optimize(a) => {
            let r = sozip::optimize(&a.input, &a.output, &sozip_options(&a.write)?)?;
            if !a.write.quiet {
                eprintln!("terrio: {} file(s) written, {} seek-optimized ({} bytes)", r.added, r.seek_optimized, r.bytes);
            }
        }
        SozipCommand::List(a) => {
            let members = sozip::list(&a.input)?;
            if a.json {
                println!("{}", serde_json::to_string_pretty(&members).map_err(|e| Error::InvalidArgument(e.to_string()))?);
            } else {
                println!("{}", sozip::LIST_HEADER);
                for m in &members {
                    println!("{}", m.list_line());
                }
            }
            if a.stats {
                print_stats();
            }
        }
        SozipCommand::Validate(a) => {
            let v = sozip::validate(&a.input)?;
            let displayed = vsi::redact_diagnostic_location(&a.input);
            if a.json {
                println!("{}", serde_json::to_string_pretty(&v).map_err(|e| Error::InvalidArgument(e.to_string()))?);
            } else {
                if a.verbose {
                    for m in &v.messages {
                        println!("{m}");
                    }
                } else {
                    for m in v.messages.iter().filter(|m| m.starts_with("* ")) {
                        println!("{m}");
                    }
                }
                for e in &v.errors {
                    println!("Error: {e}");
                }
                println!("-----");
                match (v.valid, v.sozip_files.len()) {
                    (false, _) => println!("{} is not a valid .zip file, or has invalid SOZip indexes.", displayed),
                    (true, 0) => println!("{} is a valid .zip file, but does not contain any SOZip-enabled files.", displayed),
                    (true, n) => println!("{} is a valid .zip file, and contains {n} SOZip-enabled file(s).", displayed),
                }
            }
            if !v.valid {
                return Err(Error::InvalidData(format!("'{}': not a valid SOZip archive", displayed)));
            }
        }
    }
    Ok(())
}

#[derive(clap::Args)]
struct ListArgs {
    /// The directory or file.
    path: String,
    /// Use a long listing format: permissions, size, date.
    #[arg(short = 'l', long = "long", visible_alias = "long-listing")]
    long: bool,
    /// List subdirectories recursively.
    #[arg(short = 'R', long)]
    recursive: bool,
    /// The deepest level listed with -R (1: the directory's own entries).
    #[arg(long)]
    depth: Option<usize>,
    /// Print absolute paths.
    #[arg(long = "abs", visible_alias = "absolute-path")]
    absolute: bool,
    /// Output format: text or json.
    #[arg(short = 'f', long = "of", visible_aliases = ["format", "output-format"], default_value = "text")]
    format: String,
    /// JSON output (as -f json).
    #[arg(long)]
    json: bool,
    /// With JSON output, nest the entries of directories (`{"name", "entries"}`).
    #[arg(long)]
    tree: bool,
    /// Print the HTTP requests sent and bytes received to standard error.
    #[arg(long)]
    stats: bool,
}

#[derive(clap::Args)]
struct CopyArgs {
    /// The file or directory to copy.
    source: String,
    /// The destination: an existing directory to copy into, or the new name.
    destination: String,
    /// Copy directories recursively.
    #[arg(short = 'r', long)]
    recursive: bool,
    /// Go on after a file fails to copy, reporting it.
    #[arg(long)]
    skip_errors: bool,
    #[command(flatten)]
    report: ReportArgs,
}

#[derive(clap::Args)]
struct DeleteArgs {
    /// The file or directory to delete.
    path: String,
    /// Delete a directory and everything below it.
    #[arg(short = 'r', short_alias = 'R', long)]
    recursive: bool,
    #[command(flatten)]
    report: ReportArgs,
}

#[derive(clap::Args)]
struct MoveArgs {
    /// The file or directory to move.
    source: String,
    /// The destination: an existing directory to move into, or the new name.
    destination: String,
    #[command(flatten)]
    report: ReportArgs,
}

#[derive(clap::Args)]
struct SyncArgs {
    /// The source file or directory; with a trailing slash, its contents are synchronized into
    /// the destination, without one the directory itself.
    source: String,
    /// The destination directory (or file).
    destination: String,
    /// Synchronize subdirectories too.
    #[arg(short = 'r', long)]
    recursive: bool,
    /// When a target is up to date: timestamp (size and date), ETag (entity tags, or a local
    /// file's MD5 against an S3 object's) or overwrite (always copy).
    #[arg(long, default_value = "timestamp")]
    strategy: String,
    #[command(flatten)]
    report: ReportArgs,
}

#[derive(clap::Args)]
struct ReportArgs {
    /// Print nothing on success.
    #[arg(short = 'q', long)]
    quiet: bool,
    /// Print what was done as JSON on standard output.
    #[arg(long)]
    json: bool,
    /// Print the HTTP requests sent and bytes received to standard error.
    #[arg(long)]
    stats: bool,
}

pub fn run(args: Args) -> terrio::Result<()> {
    match args.command {
        Command::List(a) => list(a),
        Command::Copy(a) => {
            let r = vsi::copy(&a.source, &a.destination, &CopyOptions { recursive: a.recursive, skip_errors: a.skip_errors })?;
            report(&a.report, &r)
        }
        Command::Delete(a) => {
            let r = vsi::delete(&a.path, a.recursive)?;
            report(&a.report, &r)
        }
        Command::Move(a) => {
            let r = vsi::rename(&a.source, &a.destination)?;
            report(&a.report, &r)
        }
        Command::Sozip(a) => sozip(a),
        Command::Sync(a) => {
            let options = SyncOptions { recursive: a.recursive, strategy: a.strategy.parse()? };
            let r = vsi::sync(&a.source, &a.destination, &options)?;
            report(&a.report, &r)
        }
    }
}

fn list(a: ListArgs) -> terrio::Result<()> {
    let json = a.json || a.format.eq_ignore_ascii_case("json");
    if !json && !a.format.eq_ignore_ascii_case("text") {
        return Err(Error::InvalidArgument(format!("unknown output format '{}' (text or json)", a.format)));
    }
    let options = ListOptions { recursive: a.recursive, depth: a.depth, absolute: a.absolute, details: a.long };
    if json {
        let value = vsi::list_json(&a.path, &options, a.tree)?;
        println!("{}", serde_json::to_string_pretty(&value).map_err(|e| Error::InvalidArgument(e.to_string()))?);
    } else {
        for e in vsi::list(&a.path, &options)? {
            println!("{}", if a.long { e.long_line() } else { e.name });
        }
    }
    if a.stats {
        print_stats();
    }
    Ok(())
}

fn report(args: &ReportArgs, r: &Report) -> terrio::Result<()> {
    if args.json {
        println!("{}", serde_json::to_string_pretty(r).map_err(|e| Error::InvalidArgument(e.to_string()))?);
    } else if !args.quiet {
        let mut parts = Vec::new();
        if r.copied > 0 || r.deleted == 0 {
            parts.push(format!("{} file(s) copied ({} bytes)", r.copied, r.bytes));
        }
        if r.unchanged > 0 {
            parts.push(format!("{} up to date", r.unchanged));
        }
        if r.deleted > 0 {
            parts.push(format!("{} deleted", r.deleted));
        }
        eprintln!("terrio: {}", parts.join(", "));
    }
    for (path, error) in &r.errors {
        eprintln!("terrio: skipped {path}: {error}");
    }
    if args.stats {
        print_stats();
    }
    if !r.errors.is_empty() {
        return Err(Error::InvalidData(format!("{} VSI operation(s) failed", r.errors.len())));
    }
    Ok(())
}

/// Prints the HTTP requests and bytes of this process.
pub fn print_stats() {
    let s = terrio::http::fetch_stats();
    eprintln!("terrio: {} HTTP request(s), {} byte(s) received", s.requests, s.bytes);
}

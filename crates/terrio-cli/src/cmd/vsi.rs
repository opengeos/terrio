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
enum Command {
    /// List a directory (or describe a file): local, /vsimem/, S3, GCS, Azure, HTTP index pages,
    /// zip and tar archives (like `gdal vsi list`).
    #[command(visible_alias = "ls")]
    List(ListArgs),
    /// Copy a file, or a directory with -r, between locations; archive members can be copied out
    /// (like `gdal vsi copy`).
    #[command(visible_alias = "cp")]
    Copy(CopyArgs),
    /// Delete a file, or a directory and its contents with -r (like `gdal vsi delete`). Refuses
    /// empty paths, roots, the home and current directories, whole buckets, archives and HTTP.
    #[command(visible_aliases = ["rm", "rmdir", "del"])]
    Delete(DeleteArgs),
    /// Move or rename a file or directory (like `gdal vsi move`): the source is deleted only once
    /// it was copied in full.
    #[command(visible_aliases = ["mv", "ren", "rename"])]
    Move(MoveArgs),
    /// Copy the files missing or out of date in the target; nothing is deleted (like
    /// `gdal vsi sync`).
    Sync(SyncArgs),
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

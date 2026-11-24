//! CPU-instruction and peak-memory benchmarks of terrio commands, run under Valgrind's Callgrind by gungraun: each
//! line of `cases.txt` is a case, `<name> <terrio arguments>`, run in this process with
//! `terrio_cli::run`. `build.rs` turns the lines into the cases of the `cli` function, so the
//! `bench` workflow's comment names each `cli <name>`.
//!
//! Run: `cargo bench -p terrio-cli --bench commands` (needs `valgrind` and `gungraun-runner` of the
//! same version as the `gungraun` dev-dependency).
//!
//! Inputs are synthetic and written by terrio into a temporary directory for each case, only those
//! its line names: no downloads. A placeholder in braces stands for a path:
//!
//! - `{out}`: an output path without extension (add one: `{out}.tif`), new for each run;
//! - `{zones_gpkg}`, `{zones_fgb}`, `{zones_parquet}`: 1024 wavy polygons in UTM zone 31N;
//!   `{zones_geoarrow}` is the GeoParquet with GeoArrow polygons (separated coordinates) in row groups
//!   of 128, whose coordinate statistics a bbox read can skip by;
//! - `{<raster>}` or `{<raster>_<format>}`: a raster of 512 x 512 pixels, `dem` (Float32 in UTM
//!   zone 31N, with a nodata block), `dem4326` (the same in EPSG:4326), `int16` (the DEM in whole
//!   metres), `classes` (Byte, 4 classes: 1, 3, 5, 7) or `rgb` (three Byte bands); as a tiled (256 x 256)
//!   GeoTIFF with a compression (`none` by default, `deflate`, `deflate_p2` for DEFLATE with
//!   predictor 2...), or `zarr` or `nc`;
//! - `{fresh_<input>}`: a copy of the input for each run, for commands that change their input.
//!
//! Each case runs its command twice, with one worker thread (`RAYON_NUM_THREADS=1`): first to load
//! the EPSG table, start the thread pool and fill caches, then counted, on every thread. Valgrind
//! instruments only the second run (`--instr-atstart=no` and client requests, see [`run`]), so the
//! setup and the first run are not counted, and run faster.
//!
//! The memory is the peak heap of the counted run beyond what was alive when it started, kept by
//! [`Counting`] and reported to DHAT as one ad-hoc event, as in `crates/terrio/benches/instructions.rs`.

use gungraun::prelude::*;
use gungraun::{Callgrind, Dhat, EntryPoint};
use std::alloc::{GlobalAlloc, Layout, System};
use std::ffi::OsString;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use terrio::raster::CrsInfo;
use terrio::vector::Coord;
use terrio::{DataType, Feature, Geometry, GeometryType, Layer, Raster, RasterConfig, RasterDataset, RasterWriteOptions};

/// Callgrind's `START_INSTRUMENTATION` and `STOP_INSTRUMENTATION` client requests
/// (`valgrind/callgrind.h`: `VG_USERREQ_TOOL_BASE('C','T') + 4` and `+ 5`).
const START: usize = 0x4354_0004;
const STOP: usize = 0x4354_0005;
/// DHAT's `DHAT_AD_HOC_EVENT` client request (`valgrind/dhat.h`: `VG_USERREQ_TOOL_BASE('D','H')`).
const AD_HOC_EVENT: usize = 0x4448_0000;

/// The system allocator, counting the heap bytes alive and their peak since [`run`] reset it.
struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

impl Counting {
    fn grew(by: usize) {
        PEAK.fetch_max(LIVE.fetch_add(by, Relaxed) + by, Relaxed);
    }
}

// SAFETY: every call goes to `System` unchanged; only the counters are added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            Self::grew(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            Self::grew(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                Self::grew(new_size - layout.size());
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Sends a client request to Valgrind: Valgrind's "special instruction preamble" for x86_64
/// (`valgrind/valgrind.h`), which does nothing outside Valgrind. Elsewhere it is not sent, and
/// nothing is counted: the CI runner is x86_64.
fn client_request(request: usize, arg: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let args: [usize; 6] = [request, arg, 0, 0, 0, 0];
        // SAFETY: the rotations of rdi add up to 128 bits, and `xchg rbx, rbx` changes nothing;
        // Valgrind recognizes the sequence, reads `args` and writes rdx.
        unsafe {
            std::arch::asm!(
                "rol rdi, 3", "rol rdi, 13", "rol rdi, 61", "rol rdi, 51", "xchg rbx, rbx",
                in("rax") args.as_ptr(),
                inout("rdi") 0usize => _,
                inout("rdx") 0usize => _,
                options(nostack),
            );
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (request, arg);
}

/// Pixels on a side of every raster input, 262 144 pixels in all.
const SIDE: usize = 512;
const NODATA: f64 = -9999.0;
/// 30 m pixels in UTM zone 31N.
const UTM: [f64; 6] = [400_000.0, 30.0, 0.0, 5_100_000.0, 0.0, -30.0];

/// A `SIDE` x `SIDE` raster of `bands` bands on `gt` in `epsg`, `value(band, row, col)` per pixel.
fn grid(
    data_type: DataType,
    bands: usize,
    nodata: Option<f64>,
    epsg: u32,
    gt: [f64; 6],
    value: impl Fn(usize, usize, usize) -> f64,
) -> RasterDataset {
    let values = (0..bands * SIDE * SIDE).map(|i| value(i / (SIDE * SIDE), i / SIDE % SIDE, i % SIDE)).collect();
    let cfg = RasterConfig {
        cols: SIDE,
        rows: SIDE,
        bands,
        data_type,
        nodata: nodata.unwrap_or(NODATA),
        crs: CrsInfo::from_epsg(epsg),
        ..Default::default()
    };
    let mut ds = RasterDataset::new(Raster::from_data(cfg, values).expect("synthetic raster"), nodata.is_some());
    ds.set_geo_transform(gt);
    ds
}

/// Smooth terrain in metres, with a nodata block.
fn elevation(row: usize, col: usize) -> f64 {
    if (200..230).contains(&row) && (300..350).contains(&col) {
        NODATA
    } else {
        (col as f64 / 23.0).sin() * (row as f64 / 17.0).cos() * 1000.0 + 1500.0
    }
}

/// The raster input `name`.
fn raster(name: &str) -> RasterDataset {
    let step = 1.0 / SIDE as f64;
    match name {
        "dem" => grid(DataType::F32, 1, Some(NODATA), 32631, UTM, |_, row, col| elevation(row, col)),
        // One degree over France (2° to 3° E, 45° to 46° N), in EPSG:4326.
        "dem4326" => grid(DataType::F32, 1, Some(NODATA), 4326, [2.0, step, 0.0, 46.0, 0.0, -step], |_, row, col| elevation(row, col)),
        "int16" => grid(DataType::I16, 1, Some(NODATA), 32631, UTM, |_, row, col| elevation(row, col).round()),
        "classes" => grid(DataType::U8, 1, None, 32631, UTM, |_, row, col| {
            ((col as f64 / 37.0).sin() + (row as f64 / 29.0).cos() + 2.0).floor() * 2.0 + 1.0
        }),
        "rgb" => grid(DataType::U8, 3, None, 32631, UTM, |band, row, col| {
            let (fx, fy) = (19.0 + 7.0 * band as f64, 13.0 + 5.0 * band as f64);
            (128.0 + 100.0 * (col as f64 / fx).sin() * (row as f64 / fy).cos()).round()
        }),
        other => panic!("cases.txt: unknown raster '{other}'"),
    }
}

/// 1024 polygons of 32 vertices with wavy edges, one in each cell of a 32 x 32 grid over the
/// rasters in UTM zone 31N.
fn zones() -> Layer {
    let cell = 30.0 * SIDE as f64 / 32.0;
    let mut zones = Layer::new("zones").with_crs_epsg(32631).with_geom_type(GeometryType::Polygon);
    for k in 0..32 * 32 {
        let (cx, cy) = (UTM[0] + cell * ((k % 32) as f64 + 0.5), UTM[3] - cell * ((k / 32) as f64 + 0.5));
        let ring: Vec<Coord> = (0..=32)
            .map(|i| {
                let a = std::f64::consts::TAU * (i % 32) as f64 / 32.0;
                let r = 0.4 * cell * (1.0 + 0.1 * (5.0 * a).sin());
                Coord::xy(cx + r * a.cos(), cy + r * a.sin())
            })
            .collect();
        zones.features.push(Feature { fid: k as u64, geometry: Some(Geometry::polygon(ring, vec![])), attributes: Vec::new() });
    }
    zones
}

/// Writes `layer` as GeoParquet with GeoArrow geometries, which terrio's GeoParquet writer does not
/// write (it writes WKB), in row groups of 128 rows.
fn write_geoarrow(layer: &Layer, path: &Path) {
    use terrio::arrow::{ArrowWriteOptions, GeometryEncoding, layer_to_arrow};
    let options = ArrowWriteOptions { geometry_encoding: GeometryEncoding::GeoArrow, ..Default::default() };
    let table = layer_to_arrow(layer, &options).expect("GeoArrow table");
    let crs = terrio::crs::SpatialRef::from_epsg(32631).and_then(|s| s.to_projjson()).expect("PROJJSON");
    let geo = format!(
        r#"{{"version":"1.1.0","primary_column":"geometry","columns":{{"geometry":{{"encoding":"polygon","geometry_types":["Polygon"],"crs":{crs}}}}}}}"#
    );
    let props = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_row_count(Some(128))
        .set_key_value_metadata(Some(vec![parquet::file::metadata::KeyValue::new("geo".into(), geo)]))
        .build();
    let file = std::fs::File::create(path).expect("GeoArrow input");
    let mut writer = parquet::arrow::ArrowWriter::try_new(file, table.schema, Some(props)).expect("Parquet writer");
    for batch in &table.batches {
        writer.write(batch).expect("GeoArrow batch");
    }
    writer.close().expect("GeoArrow input");
}

/// The path of input `name` in `dir`, written there the first time.
fn input(dir: &Path, name: &str) -> PathBuf {
    let (base, format) = name.split_once('_').unwrap_or((name, "none"));
    let ext = match format {
        "gpkg" | "fgb" | "parquet" | "zarr" | "nc" => format,
        "geoarrow" => "parquet",
        _ => "tif",
    };
    let path = dir.join(format!("{name}.{ext}"));
    if path.exists() {
        return path;
    }
    let text = path.to_str().expect("UTF-8 temporary path");
    if base == "zones" && format == "geoarrow" {
        write_geoarrow(&zones(), &path);
        return path;
    }
    if base == "zones" {
        terrio::write_vector(&[zones()], text, &Default::default()).expect("vector input");
        return path;
    }
    let mut options = RasterWriteOptions::default();
    if ext == "tif" {
        let (compression, predictor) = format.split_once("_p").map_or((format, None), |(c, p)| (c, Some(p)));
        options.compression = Some(compression.to_ascii_uppercase());
        options.tile_size = Some(256);
        options.creation_options.extend(predictor.map(|p| ("PREDICTOR".to_string(), p.to_string())));
    }
    terrio::write_raster_dataset(&raster(base), text, &options).expect("raster input");
    path
}

/// The command line of `command` (the line of `cases.txt` after the name) for run `k`, its
/// placeholders replaced by paths in `dir`.
fn command_line(dir: &Path, command: &str, k: usize) -> Vec<OsString> {
    let run = dir.join(format!("run{k}"));
    std::fs::create_dir(&run).expect("run directory");
    let path = |name: &str| {
        if name == "out" {
            return run.join("out");
        }
        let Some(name) = name.strip_prefix("fresh_") else { return input(dir, name) };
        let src = input(dir, name);
        let copy = run.join(src.file_name().expect("input file name"));
        std::fs::copy(&src, &copy).expect("copy of an input");
        copy
    };
    let mut line = vec![OsString::from("terrio")];
    for arg in command.split_whitespace() {
        let mut out = String::new();
        let mut rest = arg;
        while let Some((before, after)) = rest.split_once('{') {
            let (name, tail) = after.split_once('}').unwrap_or_else(|| panic!("cases.txt: unclosed brace in '{arg}'"));
            out.push_str(before);
            out.push_str(path(name).to_str().expect("UTF-8 temporary path"));
            rest = tail;
        }
        out.push_str(rest);
        line.push(out.into());
    }
    line
}

/// A case's temporary directory and its command lines: the uncounted run's, then the counted one's.
type Case = (tempfile::TempDir, [Vec<OsString>; 2]);

fn prepare(command: &str) -> Case {
    let dir = tempfile::tempdir().expect("temporary directory");
    let lines = [1, 2].map(|k| command_line(dir.path(), command, k));
    (dir, lines)
}

/// Runs the command once uninstrumented, then again counting the instructions of every thread,
/// terrio's pool workers' too: Callgrind's default entry point would count only this thread's.
/// Reports the peak heap of the counted run to DHAT.
fn run((dir, [warm, counted]): Case) -> tempfile::TempDir {
    let shown = format!("{counted:?}");
    assert_eq!(terrio_cli::run(warm), 0, "{shown}");
    let before = LIVE.load(Relaxed);
    PEAK.store(before, Relaxed);
    client_request(START, 0);
    let code = terrio_cli::run(black_box(counted));
    client_request(STOP, 0);
    client_request(AD_HOC_EVENT, PEAK.load(Relaxed) - before);
    assert_eq!(code, 0, "{shown}");
    dir
}

include!(concat!(env!("OUT_DIR"), "/cases.rs"));

library_benchmark_group!(name = commands, benchmarks = [cli]);

main!(
    config = LibraryBenchmarkConfig::default()
        .env("RAYON_NUM_THREADS", "1")
        .tool(Callgrind::with_args(["--instr-atstart=no"]).entry_point(EntryPoint::None))
        // The ad-hoc event of `run`, wherever it is sent.
        .tool(Dhat::with_args(["--mode=ad-hoc"]).entry_point(EntryPoint::None));
    library_benchmark_groups = commands
);

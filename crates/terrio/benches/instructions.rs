//! CPU-instruction benchmarks of terrio's hot paths, run under Valgrind's Callgrind by gungraun.
//! Instruction counts repeat to about 1% on any machine, so the `bench` workflow compares a pull
//! request with its merge base on a shared CI runner and comments the changes.
//!
//! Run: `cargo bench -p terrio --bench instructions` (needs `valgrind` and `gungraun-runner` of
//! the same version as the `gungraun` dev-dependency). Inputs are synthetic: no downloads.
//!
//! Each benchmark runs its operation twice, with one worker thread (`RAYON_NUM_THREADS=1`): first
//! to load the EPSG table, start the thread pool and fill caches, then counted, on every thread.
//! Valgrind instruments only the second run (`--instr-atstart=no` and client requests, see
//! [`measured`]), so the setup and the first run are not counted, and run faster.

use gungraun::prelude::*;
use gungraun::{Callgrind, EntryPoint};
use std::hint::black_box;
use terrio::crs::SpatialRef;
use terrio::ops::Resampling;
use terrio::ops::dem::{DemOptions, DemProduct, dem};
use terrio::ops::warp::{WarpOptions, warp};
use terrio::ops::zonal::{ZonalBand, ZonalOptions, ZonalSource, zonal_stats};
use terrio::vector::Coord;
use terrio::{DataType, Feature, Geometry, Layer, LazyRaster, Raster, RasterConfig, RasterDataset, RasterWriteOptions};

/// Callgrind's `START_INSTRUMENTATION` and `STOP_INSTRUMENTATION` client requests
/// (`valgrind/callgrind.h`: `VG_USERREQ_TOOL_BASE('C','T') + 4` and `+ 5`).
const START: usize = 0x4354_0004;
const STOP: usize = 0x4354_0005;

/// Sends a client request to Valgrind: Valgrind's "special instruction preamble" for x86_64
/// (`valgrind/valgrind.h`), which does nothing outside Valgrind. Elsewhere it is not sent, and
/// nothing is counted: the CI runner is x86_64.
fn client_request(request: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let args: [usize; 6] = [request, 0, 0, 0, 0, 0];
        // SAFETY: the rotations of rdi add up to 128 bits and leave it unchanged, and `xchg rbx,
        // rbx` changes nothing; Valgrind recognizes the sequence, reads `args` and writes rdx.
        unsafe {
            std::arch::asm!(
                "rol rdi, 3", "rol rdi, 13", "rol rdi, 61", "rol rdi, 51", "xchg rbx, rbx",
                in("rax") args.as_ptr(),
                inout("rdx") 0usize => _,
                options(nostack),
            );
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = request;
}

/// Runs `f` once uninstrumented, then again counting the instructions of every thread, terrio's
/// pool workers' too: Callgrind's default entry point would count only this thread's.
fn measured<R>(f: impl Fn() -> R) -> R {
    drop(black_box(f()));
    client_request(START);
    let out = black_box(f());
    client_request(STOP);
    out
}

/// Pixels on a side of every input: 1 M pixels, a few seconds each under Callgrind.
const SIDE: usize = 1024;
const NODATA: f64 = -9999.0;

/// A smooth Float32 DEM of `SIDE` x `SIDE` pixels on `gt` in `epsg`, with a nodata block.
fn surface(epsg: u32, gt: [f64; 6]) -> RasterDataset {
    let values = (0..SIDE * SIDE)
        .map(|i| {
            let (row, col) = (i / SIDE, i % SIDE);
            if (400..460).contains(&row) && (600..700).contains(&col) {
                NODATA
            } else {
                (col as f64 / 23.0).sin() * (row as f64 / 17.0).cos() * 1000.0 + 1500.0
            }
        })
        .collect();
    let cfg = RasterConfig {
        cols: SIDE,
        rows: SIDE,
        bands: 1,
        data_type: DataType::F32,
        nodata: NODATA,
        crs: wbraster::CrsInfo::from_epsg(epsg),
        ..Default::default()
    };
    let mut ds = RasterDataset::new(Raster::from_data(cfg, values).expect("synthetic raster"), true);
    ds.set_geo_transform(gt);
    ds
}

/// One degree in EPSG:4326 over France (UTM zone 31N).
fn geographic() -> RasterDataset {
    let step = 1.0 / SIDE as f64;
    surface(4326, [2.0, step, 0.0, 46.0, 0.0, -step])
}

/// 30 m pixels in UTM zone 31N.
fn projected() -> RasterDataset {
    surface(32631, [400_000.0, 30.0, 0.0, 5_100_000.0, 0.0, -30.0])
}

/// EPSG:4326 to UTM 31N at 90 m (about 1 M output pixels); the EPSG table is loaded here.
fn reproject(resampling: Resampling, grid_invariant: bool) -> (RasterDataset, WarpOptions) {
    let dst = SpatialRef::from_epsg(32631).expect("EPSG:32631");
    let options = WarpOptions { dst_srs: Some(dst), resolution: Some((90.0, 90.0)), resampling, grid_invariant, ..Default::default() };
    (geographic(), options)
}

/// The same grid shifted by half a pixel: the per-pixel loop without a CRS change.
fn shift() -> (RasterDataset, WarpOptions) {
    let ds = geographic();
    let step = 1.0 / SIDE as f64;
    let half = step / 2.0;
    let extent = (2.0 + half, 45.0 + half, 3.0 - half, 46.0 - half);
    (ds, WarpOptions { extent: Some(extent), resolution: Some((step, step)), ..Default::default() })
}

#[library_benchmark]
#[bench::reproject_near(setup = reproject, args = (Resampling::Nearest, true))]
#[bench::reproject_bilinear(setup = reproject, args = (Resampling::Bilinear, true))]
#[bench::reproject_cubic(setup = reproject, args = (Resampling::Cubic, true))]
#[bench::reproject_near_gdal_grid(setup = reproject, args = (Resampling::Nearest, false))]
#[bench::half_pixel_shift_near(setup = shift)]
fn warp_f32((ds, options): (RasterDataset, WarpOptions)) -> RasterDataset {
    measured(|| warp(&[black_box(&ds)], black_box(&options)).expect("warp"))
}

/// The UTM DEM and a product of it.
fn terrain(product: DemProduct) -> (RasterDataset, DemProduct) {
    (projected(), product)
}

#[library_benchmark]
#[bench::slope(setup = terrain, args = (DemProduct::Slope))]
#[bench::hillshade(setup = terrain, args = (DemProduct::Hillshade))]
fn dem_f32((ds, product): (RasterDataset, DemProduct)) -> RasterDataset {
    measured(|| dem(black_box(&ds), product, &DemOptions::default()).expect("dem"))
}

/// A tiled (256 x 256) DEFLATE GeoTIFF of Float32 with the floating-point predictor.
fn deflate_predictor3() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("temporary directory");
    let path = dir.path().join("dem.tif").to_string_lossy().into_owned();
    let options = RasterWriteOptions {
        compression: Some("DEFLATE".into()),
        tile_size: Some(256),
        creation_options: vec![("PREDICTOR".into(), "3".into())],
        ..Default::default()
    };
    terrio::write_raster_dataset(&projected(), &path, &options).expect("GeoTIFF write");
    (dir, path)
}

#[library_benchmark]
#[bench::deflate_predictor3(setup = deflate_predictor3)]
fn read_geotiff_f32((dir, path): (tempfile::TempDir, String)) -> (tempfile::TempDir, RasterDataset) {
    (dir, measured(|| terrio::open_raster(black_box(&path)).expect("GeoTIFF read")))
}

/// 1024 octagons, one in each cell of a 32 x 32 grid over the raster, and the raster.
fn octagons() -> (Layer, ZonalSource) {
    let ds = projected();
    let cell = 30.0 * SIDE as f64 / 32.0;
    let mut zones = Layer::new("zones").with_crs_epsg(32631);
    for k in 0..32 * 32 {
        let (cx, cy) = (400_000.0 + cell * ((k % 32) as f64 + 0.5), 5_100_000.0 - cell * ((k / 32) as f64 + 0.5));
        let ring: Vec<Coord> = (0..=8)
            .map(|i| {
                let a = std::f64::consts::TAU * (i % 8) as f64 / 8.0 + 0.3;
                Coord::xy(cx + 0.45 * cell * a.cos(), cy + 0.45 * cell * a.sin())
            })
            .collect();
        zones.features.push(Feature { fid: k as u64, geometry: Some(Geometry::polygon(ring, vec![])), attributes: Vec::new() });
    }
    (zones, ZonalSource { raster: LazyRaster::from(ds).into(), bands: vec![ZonalBand::new(1, "dem")] })
}

#[library_benchmark]
#[bench::octagons(setup = octagons)]
fn zonal_stats_f32((zones, source): (Layer, ZonalSource)) -> Layer {
    let sources = std::cell::RefCell::new([source]);
    measured(|| zonal_stats(black_box(&zones), &mut *sources.borrow_mut(), &ZonalOptions::default()).expect("zonal statistics"))
}

library_benchmark_group!(name = warp_group, benchmarks = [warp_f32]);
library_benchmark_group!(name = dem_group, benchmarks = [dem_f32]);
library_benchmark_group!(name = read_group, benchmarks = [read_geotiff_f32]);
library_benchmark_group!(name = zonal_group, benchmarks = [zonal_stats_f32]);

main!(
    config = LibraryBenchmarkConfig::default()
        .env("RAYON_NUM_THREADS", "1")
        .tool(Callgrind::with_args(["--instr-atstart=no"]).entry_point(EntryPoint::None));
    library_benchmark_groups = warp_group, dem_group, read_group, zonal_group
);

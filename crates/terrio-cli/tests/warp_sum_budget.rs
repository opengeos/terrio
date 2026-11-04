//! The memory budget of a `sum` warp (`TERRIO_WARP_SUM_MAX_BYTES`), through the real executable.

use std::path::Path;
use std::process::{Command, Output};

fn run(dir: &Path, budget: &str, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_terrio"));
    command.args(args).current_dir(dir).env("TERRIO_WARP_SUM_MAX_BYTES", budget).output().unwrap()
}

fn total(path: &Path) -> f64 {
    let r = terrio::open_raster(path.to_str().unwrap()).unwrap().raster;
    (0..r.rows).flat_map(|row| (0..r.cols).map(move |col| (row, col))).map(|(row, col)| r.get(0, row as isize, col as isize)).sum()
}

#[test]
fn a_sum_over_its_memory_budget_is_refused_before_its_source_is_read() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    // 32 x 32 Float32 ones with an overview at 4. A `sum` holds 4 bytes per source pixel and 8 of
    // shares: 12,288 bytes at full resolution, 768 for the overview's 64 pixels.
    let cfg = terrio::RasterConfig {
        cols: 32,
        rows: 32,
        x_min: 0.,
        y_min: 0.,
        cell_size: 1.,
        data_type: terrio::DataType::F32,
        ..Default::default()
    };
    let mut r = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, vec![1.0; 1024]).unwrap(), false);
    r.raster.crs.epsg = Some(3857);
    terrio::write_raster_dataset(&r, d.join("ones.tif").to_str().unwrap(), &Default::default()).unwrap();
    assert!(run(d, "20000", &["overviews", "ones.tif", "4", "-r", "average"]).status.success());
    let sum = ["warp", "ones.tif", "out.tif", "-r", "sum", "--ts", "1", "1", "--ot", "Float64"];

    let refused = run(d, "10000", &sum);
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{message}");
    assert!(message.contains("TERRIO_WARP_SUM_MAX_BYTES=10000") && message.contains("--overview n"), "{message}");

    let under = run(d, "20000", &sum);
    assert!(under.status.success(), "{}", String::from_utf8_lossy(&under.stderr));
    assert_eq!(total(&d.join("out.tif")), 1024.0);
    let level = run(d, "10000", &[&sum[..], &["--overview", "0"]].concat());
    assert!(level.status.success(), "{}", String::from_utf8_lossy(&level.stderr));
    assert_eq!(total(&d.join("out.tif")), 64.0);
    let average = run(d, "10000", &["warp", "ones.tif", "avg.tif", "-r", "average", "--ts", "1", "1", "--overview", "NONE"]);
    assert!(average.status.success(), "{}", String::from_utf8_lossy(&average.stderr));
    assert_eq!(total(&d.join("avg.tif")), 1.0);
}

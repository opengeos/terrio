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

/// Writes `name`: `bands` bands of 32 x 32 Float32 ones, of 1-unit pixels in EPSG:3857.
fn write_ones(dir: &Path, name: &str, bands: usize) {
    let cfg = terrio::RasterConfig {
        cols: 32,
        rows: 32,
        bands,
        x_min: 0.,
        y_min: 0.,
        cell_size: 1.,
        data_type: terrio::DataType::F32,
        ..Default::default()
    };
    let mut r = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, vec![1.0; 1024 * bands]).unwrap(), false);
    r.raster.crs.epsg = Some(3857);
    terrio::write_raster_dataset(&r, dir.join(name).to_str().unwrap(), &Default::default()).unwrap();
}

#[test]
fn a_sum_over_its_memory_budget_is_refused_before_its_source_is_read() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    // 32 x 32 Float32 ones with an overview at 4. A `sum` holds 4 bytes per source pixel and 8 of
    // shares: 12,288 bytes at full resolution, 768 for the overview's 64 pixels.
    write_ones(d, "ones.tif", 1);
    assert!(run(d, "20000", &["overviews", "ones.tif", "4", "-r", "average"]).status.success());
    let sum = ["warp", "ones.tif", "out.tif", "-r", "sum", "--ts", "1", "1", "--ot", "Float64", "--overview", "NONE"];

    let refused = run(d, "10000", &sum);
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{message}");
    assert!(message.contains("TERRIO_WARP_SUM_MAX_BYTES=10000") && message.contains("--overview n"), "{message}");
    assert!(message.contains("`overview=n` for `terrio.warp`, or a source opened with `overview_level=n` in `terrio.rio`"), "{message}");

    let under = run(d, "20000", &sum);
    assert!(under.status.success(), "{}", String::from_utf8_lossy(&under.stderr));
    assert_eq!(total(&d.join("out.tif")), 1024.0);
    // The level the message names: its 64 pixels, each counted for the 16 source pixels it stands for.
    let level = run(d, "10000", &[&sum[..10], &["--overview", "0"]].concat());
    assert!(level.status.success(), "{}", String::from_utf8_lossy(&level.stderr));
    assert_eq!(total(&d.join("out.tif")), 1024.0);
    let average = run(d, "10000", &["warp", "ones.tif", "avg.tif", "-r", "average", "--ts", "1", "1", "--overview", "NONE"]);
    assert!(average.status.success(), "{}", String::from_utf8_lossy(&average.stderr));
    assert_eq!(total(&d.join("avg.tif")), 1.0);
}

/// An 18919 x 18919 Float32 VRT holding 32 x 32 ones in its corner.
const LARGE: &str = r#"<VRTDataset rasterXSize="18919" rasterYSize="18919">
  <SRS>EPSG:3857</SRS>
  <GeoTransform>0, 1, 0, 32, 0, -1</GeoTransform>
  <VRTRasterBand dataType="Float32" band="1">
    <SimpleSource>
      <SourceFilename relativeToVRT="1">ones.tif</SourceFilename>
      <SourceBand>1</SourceBand>
      <SrcRect xOff="0" yOff="0" xSize="32" ySize="32"/>
      <DstRect xOff="0" yOff="0" xSize="32" ySize="32"/>
    </SimpleSource>
  </VRTRasterBand>
</VRTDataset>"#;

#[test]
fn a_sum_just_over_the_default_budget_is_refused() {
    // 18919 x 18919 pixels of 12 bytes: 4,295,142,732, just over the default 4 GiB.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    std::fs::write(d.join("large.vrt"), LARGE).unwrap();
    write_ones(d, "ones.tif", 1);
    let mut command = Command::new(env!("CARGO_BIN_EXE_terrio"));
    command.args(["warp", "large.vrt", "out.tif", "-r", "sum", "--ts", "1", "1"]).current_dir(d);
    let refused = command.env_remove("TERRIO_WARP_SUM_MAX_BYTES").output().unwrap();
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{message}");
    assert!(message.contains("about 4295142732 bytes") && message.contains("TERRIO_WARP_SUM_MAX_BYTES=4294967296"), "{message}");
}

#[test]
fn a_sum_counts_the_bytes_of_every_band() {
    // 32 x 32 Float32 ones summed into 1 x 1 under 14,000 bytes. Each band holds 4 bytes per
    // pixel and the shares 8, so one band (12,288) fits and two (16,384) do not.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    write_ones(d, "one.tif", 1);
    write_ones(d, "two.tif", 2);
    let refused = run(d, "14000", &["warp", "two.tif", "out.tif", "-r", "sum", "--ts", "1", "1", "--ot", "Float64"]);
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{message}");
    assert!(message.contains("about 16384 bytes"), "{message}");
    let fits = run(d, "14000", &["warp", "one.tif", "out.tif", "-r", "sum", "--ts", "1", "1", "--ot", "Float64"]);
    assert!(fits.status.success(), "{}", String::from_utf8_lossy(&fits.stderr));
    assert_eq!(total(&d.join("out.tif")), 1024.0);
}

#[test]
fn a_sum_counts_a_window_converted_to_another_working_type() {
    // Warped VRTs of 32 x 32 ones summed into 1 x 1, read under 16,000 bytes. A window converted to
    // another working type is held at once in its source type, as f64 values and, unless that type
    // is Float64, in it: Float32 to Float64 holds 4 + 8 bytes a pixel (20,480 with the shares) and
    // Byte to Float32 1 + 8 + 4 (21,504), while Float32 kept as Float32 holds 4 (12,288) and fits.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    write_ones(d, "ones.tif", 1);
    let writes: [&[&str]; 4] = [
        &["translate", "ones.tif", "bytes.tif", "--ot", "Byte"],
        &["warp", "ones.tif", "f64.vrt", "-r", "sum", "--ts", "1", "1", "--ot", "Float64"],
        &["warp", "bytes.tif", "f32.vrt", "-r", "sum", "--ts", "1", "1", "--ot", "Float32"],
        &["warp", "ones.tif", "same.vrt", "-r", "sum", "--ts", "1", "1"],
    ];
    for args in writes {
        assert!(run(d, "16000", args).status.success(), "{args:?}");
    }
    for (vrt, estimate) in [("f64.vrt", "about 20480 bytes"), ("f32.vrt", "about 21504 bytes")] {
        let refused = run(d, "16000", &["translate", vrt, "out.tif"]);
        let message = String::from_utf8_lossy(&refused.stderr);
        assert!(!refused.status.success(), "{vrt}: {message}");
        assert!(message.contains(estimate), "{vrt}: {message}");
    }
    let fits = run(d, "16000", &["translate", "same.vrt", "out.tif"]);
    assert!(fits.status.success(), "{}", String::from_utf8_lossy(&fits.stderr));
    assert_eq!(total(&d.join("out.tif")), 1024.0);
}

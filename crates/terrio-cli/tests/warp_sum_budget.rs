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
    // 32 x 32 Float32 ones with an overview at 4. A `sum` holds the 4 bytes of each source pixel:
    // 4,096 bytes at full resolution, 256 for the overview's 64 pixels.
    write_ones(d, "ones.tif", 1);
    assert!(run(d, "20000", &["overviews", "ones.tif", "4", "-r", "average"]).status.success());
    let base = ["warp", "ones.tif", "out.tif", "-r", "sum", "--ts", "1", "1", "--ot", "Float64"];
    let sum = [&base[..], &["--overview", "NONE"]].concat();

    let refused = run(d, "4000", &sum);
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{message}");
    assert!(message.contains("about 4096 bytes") && message.contains("TERRIO_WARP_SUM_MAX_BYTES=4000"), "{message}");
    assert!(message.contains("--overview n"), "{message}");
    assert!(message.contains("`overview=n` for `terrio.warp`, or a source opened with `overview_level=n` in `terrio.rio`"), "{message}");

    let under = run(d, "5000", &sum);
    assert!(under.status.success(), "{}", String::from_utf8_lossy(&under.stderr));
    assert_eq!(total(&d.join("out.tif")), 1024.0);
    // The level the message names: its 64 pixels, each counted for the 16 source pixels it stands for.
    let level = run(d, "4000", &[&base[..], &["--overview", "0"]].concat());
    assert!(level.status.success(), "{}", String::from_utf8_lossy(&level.stderr));
    assert_eq!(total(&d.join("out.tif")), 1024.0);
    let average = run(d, "4000", &["warp", "ones.tif", "avg.tif", "-r", "average", "--ts", "1", "1", "--overview", "NONE"]);
    assert!(average.status.success(), "{}", String::from_utf8_lossy(&average.stderr));
    assert_eq!(total(&d.join("avg.tif")), 1.0);
}

#[test]
fn an_automatic_sum_reads_the_overview_within_a_budget_the_full_resolution_exceeds() {
    // 32 x 32 Float32 ones with an overview at 4, summed into 1 x 1 under 4,000 bytes: the full
    // resolution would hold 4,096 bytes; `--overview AUTO`, the default, reads the overview (256)
    // and counts each of its pixels for the 16 source pixels it stands for.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    write_ones(d, "ones.tif", 1);
    assert!(run(d, "20000", &["overviews", "ones.tif", "4", "-r", "average"]).status.success());
    for auto in [&[][..], &["--overview", "AUTO"][..]] {
        let args = [&["warp", "ones.tif", "out.tif", "-r", "sum", "--ts", "1", "1", "--ot", "Float64"][..], auto].concat();
        let read = run(d, "4000", &args);
        assert!(read.status.success(), "{auto:?}: {}", String::from_utf8_lossy(&read.stderr));
        assert_eq!(total(&d.join("out.tif")), 1024.0, "{auto:?}");
    }
}

/// A 23171 x 23171 Float32 VRT holding 32 x 32 ones in its corner.
const LARGE: &str = r#"<VRTDataset rasterXSize="23171" rasterYSize="23171">
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
    // Two sources are held whole: twice 23171 x 23171 pixels of 4 bytes, 4,295,161,928, just over
    // the default 4 GiB.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    std::fs::write(d.join("large.vrt"), LARGE).unwrap();
    write_ones(d, "ones.tif", 1);
    let mut command = Command::new(env!("CARGO_BIN_EXE_terrio"));
    command.args(["warp", "large.vrt", "large.vrt", "out.tif", "-r", "sum", "--ts", "1", "1"]).current_dir(d);
    let refused = command.env_remove("TERRIO_WARP_SUM_MAX_BYTES").output().unwrap();
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{message}");
    assert!(message.contains("about 4295161928 bytes") && message.contains("TERRIO_WARP_SUM_MAX_BYTES=4294967296"), "{message}");
}

#[test]
fn a_sum_counts_the_bytes_of_every_band() {
    // 32 x 32 Float32 ones summed into 1 x 1 under 6,000 bytes. Each band holds 4 bytes per
    // pixel, so one band (4,096) fits and two (8,192) do not.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    write_ones(d, "one.tif", 1);
    write_ones(d, "two.tif", 2);
    let refused = run(d, "6000", &["warp", "two.tif", "out.tif", "-r", "sum", "--ts", "1", "1", "--ot", "Float64"]);
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success(), "{message}");
    assert!(message.contains("about 8192 bytes"), "{message}");
    let fits = run(d, "6000", &["warp", "one.tif", "out.tif", "-r", "sum", "--ts", "1", "1", "--ot", "Float64"]);
    assert!(fits.status.success(), "{}", String::from_utf8_lossy(&fits.stderr));
    assert_eq!(total(&d.join("out.tif")), 1024.0);
}

#[test]
fn a_sum_counts_a_window_converted_to_another_working_type() {
    // Warped VRTs of 32 x 32 ones summed into 1 x 1, read under 10,000 bytes. A window converted to
    // another working type is held at once in its source type, as f64 values and, unless that type
    // is Float64, in it: Float32 to Float64 holds 4 + 8 bytes a pixel (12,288) and Byte to Float32
    // 1 + 8 + 4 (13,312), while Float32 kept as Float32 holds 4 (4,096) and fits.
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
    for (vrt, estimate) in [("f64.vrt", "about 12288 bytes"), ("f32.vrt", "about 13312 bytes")] {
        let refused = run(d, "10000", &["translate", vrt, "out.tif"]);
        let message = String::from_utf8_lossy(&refused.stderr);
        assert!(!refused.status.success(), "{vrt}: {message}");
        assert!(message.contains(estimate), "{vrt}: {message}");
    }
    let fits = run(d, "10000", &["translate", "same.vrt", "out.tif"]);
    assert!(fits.status.success(), "{}", String::from_utf8_lossy(&fits.stderr));
    assert_eq!(total(&d.join("out.tif")), 1024.0);
}

#[test]
fn a_sum_read_a_strip_at_a_time_counts_the_two_strips_it_holds() {
    // A warped VRT of 32 x 32 Float32 ones summed into 1 x 1 as Float64, read under 6,000 bytes.
    // Within its working memory limit, 64 MiB, the read holds the whole window, 4 + 8 bytes a
    // pixel: 12,288 bytes, over the budget. Within a limit of 2,000 bytes, of which a row of the
    // window takes 384 (source plus conversion: 12 bytes per pixel), it reads 5 rows at a time
    // and holds two strips at once, one summed while the next is read: 10 rows, 3,840 bytes, which
    // fit.
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    write_ones(d, "ones.tif", 1);
    assert!(run(d, "6000", &["warp", "ones.tif", "whole.vrt", "-r", "sum", "--ts", "1", "1", "--ot", "Float64"]).status.success());
    let whole = std::fs::read_to_string(d.join("whole.vrt")).unwrap();
    assert!(whole.contains("<WarpMemoryLimit>67108864</WarpMemoryLimit>"), "{whole}");
    std::fs::write(d.join("strips.vrt"), whole.replace("<WarpMemoryLimit>67108864<", "<WarpMemoryLimit>2000<")).unwrap();
    let refused = run(d, "6000", &["translate", "whole.vrt", "out.tif"]);
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success() && message.contains("about 12288 bytes"), "{message}");
    let fits = run(d, "6000", &["translate", "strips.vrt", "out.tif"]);
    assert!(fits.status.success(), "{}", String::from_utf8_lossy(&fits.stderr));
    assert_eq!(total(&d.join("out.tif")), 1024.0);
    let refused = run(d, "3800", &["translate", "strips.vrt", "out.tif"]);
    let message = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success() && message.contains("about 3840 bytes"), "{message}");
}

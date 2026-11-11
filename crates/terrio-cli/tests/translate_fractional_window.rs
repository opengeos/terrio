//! `terrio translate` resamples a window at a fractional offset, read at its own size, at that
//! offset, whether its input is read whole (an Esri ASCII grid) or by window (a GeoTIFF) (#757).

use std::path::Path;
use std::process::Command;

/// The x origin and the pixels of `terrio translate INPUT OUTPUT args...`.
fn translate(input: &Path, output: &Path, args: &[&str]) -> (f64, Vec<f64>) {
    let result = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("translate").arg(input).arg(output).args(args).output().unwrap();
    assert!(result.status.success(), "{args:?}: {}", String::from_utf8_lossy(&result.stderr));
    let out = terrio::open_raster(output.to_str().unwrap()).unwrap();
    (out.geo_transform()[0], out.raster.data.to_f64_vec())
}

/// A ramp of 8 x 4 pixels of 1 m, each holding ten times its column, in an Esri ASCII grid and
/// in a GeoTIFF: `--srcwin 0.25 0 7 4` covers three quarters of each column and a quarter of the
/// next, which a bilinear kernel at each centre and an average both give 2.5 more than the
/// column holds, and nearest neighbour the column itself.
#[test]
fn a_fractional_srcwin_is_resampled_from_a_file_read_whole_as_from_one_read_by_window() {
    let dir = tempfile::tempdir().unwrap();
    let (asc, tif) = (dir.path().join("ramp.asc"), dir.path().join("ramp.tif"));
    // Written with a decimal point, the grid is read as float32 values (as integers, it would
    // round what is resampled from it).
    let row: Vec<String> = (0..8).map(|c| format!("{}.0", 10 * c)).collect();
    let header = "ncols 8\nnrows 4\nxllcorner 0\nyllcorner 0\ncellsize 1\n";
    std::fs::write(&asc, format!("{header}{}", format!("{}\n", row.join(" ")).repeat(4))).unwrap();
    let ramp = terrio::open_raster(asc.to_str().unwrap()).unwrap();
    terrio::write_raster_dataset(&ramp, tif.to_str().unwrap(), &terrio::RasterWriteOptions::default()).unwrap();
    for (method, first) in [("nearest", 0.0), ("bilinear", 2.5), ("average", 2.5)] {
        let expected: Vec<f64> = (0..28).map(|k| first + 10.0 * (k % 7) as f64).collect();
        for input in [&asc, &tif] {
            let output = dir.path().join(format!("{method}.tif"));
            let (x0, pixels) = translate(input, &output, &["--srcwin", "0.25", "0", "7", "4", "-r", method]);
            assert_eq!((x0, pixels), (0.25, expected.clone()), "{method} from {}", input.display());
        }
    }
}

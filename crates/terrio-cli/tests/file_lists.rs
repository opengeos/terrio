//! `@FILE` input lists (as GDAL's `--input @filelist.txt`) through the real executable.

use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(dir).output().unwrap()
}

fn ok(args: &[&str], dir: &Path) {
    let r = run(args, dir);
    assert!(r.status.success(), "{args:?}: {}", String::from_utf8_lossy(&r.stderr));
}

fn tile(dir: &Path, name: &str, x_min: f64, value: f64) {
    let cfg =
        terrio::RasterConfig { cols: 2, rows: 2, x_min, y_min: 0., cell_size: 1., data_type: terrio::DataType::F64, ..Default::default() };
    let mut r = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, vec![value; 4]).unwrap(), false);
    r.raster.crs.epsg = Some(4326);
    terrio::write_raster_dataset(&r, dir.join(name).to_str().unwrap(), &Default::default()).unwrap();
}

fn pixels(path: &Path) -> (usize, usize, Vec<f64>) {
    let r = terrio::open_raster(path.to_str().unwrap()).unwrap().raster;
    let values =
        (0..r.rows).flat_map(|row| (0..r.cols).map(move |col| (row, col))).map(|(row, col)| r.get(0, row as isize, col as isize)).collect();
    (r.cols, r.rows, values)
}

#[test]
fn raster_commands_read_inputs_from_a_file_list() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    tile(d, "a.tif", 0., 1.);
    tile(d, "b.tif", 2., 2.);
    tile(d, "c.tif", 4., 3.);
    // Blank lines and surrounding whitespace are ignored, and a byte-order mark and CRLF line
    // endings (a list saved on Windows) are accepted.
    std::fs::write(d.join("list.txt"), "\u{feff}a.tif\r\n\r\n  b.tif  \r\n").unwrap();
    ok(&["mosaic", "a.tif", "b.tif", "c.tif", "explicit.tif"], d);
    // A list mixes with paths given directly, in order.
    ok(&["mosaic", "@list.txt", "c.tif", "listed.tif"], d);
    let expected = pixels(&d.join("explicit.tif"));
    assert_eq!(expected.0, 6);
    assert_eq!(pixels(&d.join("listed.tif")), expected);
    ok(&["warp", "@list.txt", "c.tif", "warped.tif"], d);
    assert_eq!(pixels(&d.join("warped.tif")), expected);
    // A virtual mosaic references the listed inputs.
    ok(&["mosaic", "@list.txt", "listed.vrt"], d);
    let vrt = std::fs::read_to_string(d.join("listed.vrt")).unwrap();
    assert!(vrt.contains("a.tif") && vrt.contains("b.tif") && !vrt.contains("list.txt"), "{vrt}");
}

#[test]
fn vector_concat_reads_inputs_from_a_file_list() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    for (name, x) in [("a", 0), ("b", 1)] {
        let json = format!(
            r#"{{"type":"FeatureCollection","name":"points","features":[{{"type":"Feature","properties":{{}},"geometry":{{"type":"Point","coordinates":[{x},0]}}}}]}}"#
        );
        std::fs::write(d.join(format!("{name}.geojson")), json).unwrap();
    }
    std::fs::write(d.join("list.txt"), "a.geojson\nb.geojson\n").unwrap();
    ok(&["concat", "@list.txt", "merged.gpkg", "--mode", "single"], d);
    let merged = terrio::open_vector(d.join("merged.gpkg").to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(merged.layers[0].len(), 2);
}

#[test]
fn bad_file_lists_are_usage_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    tile(d, "a.tif", 0., 1.);
    std::fs::write(d.join("empty.txt"), "\n  \n").unwrap();
    for (list, message) in
        [("@missing.txt", "cannot read the file list missing.txt"), ("@empty.txt", "the file list empty.txt names no inputs")]
    {
        let r = run(&["mosaic", list, "out.tif"], d);
        assert!(!r.status.success());
        let stderr = String::from_utf8_lossy(&r.stderr);
        assert!(stderr.contains(message), "{stderr}");
    }
    assert!(!d.join("out.tif").exists());
}

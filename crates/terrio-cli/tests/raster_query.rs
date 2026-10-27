//! `terrio compare`, `terrio pixel-info` and `terrio xyz` through the command line.

use std::io::Write;
use std::process::{Command, Stdio};

fn terrio(args: &[&str], stdin: Option<&str>) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        if let Some(text) = stdin {
            input.write_all(text.as_bytes()).unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    (out.status.code().unwrap(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
}

fn write(dir: &std::path::Path, name: &str, values: Vec<f64>, nodata: Option<f64>) -> String {
    let cfg = terrio::RasterConfig {
        cols: 4,
        rows: 2,
        cell_size: 10.0,
        nodata: nodata.unwrap_or(-9999.0),
        data_type: terrio::DataType::F32,
        ..Default::default()
    };
    let mut ds = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, values).unwrap(), nodata.is_some());
    ds.raster.crs.epsg = Some(32611);
    ds.set_geo_transform([500000.0, 10.0, 0.0, 4000020.0, 0.0, -10.0]);
    let path = dir.join(name);
    terrio::write_raster_dataset(&ds, path.to_str().unwrap(), &Default::default()).unwrap();
    path.to_str().unwrap().to_string()
}

#[test]
fn compare_exits_with_the_difference_count() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(dir.path(), "a.tif", (0..8).map(f64::from).collect(), None);
    let b = write(dir.path(), "b.tif", (0..8).map(|v| if v == 5 { 9.0 } else { f64::from(v) }).collect(), Some(0.0));
    assert_eq!(terrio(&["compare", &a, &a], None).0, 0);
    let (code, out, _) = terrio(&["compare", &a, &b], None);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(code as usize, lines.len());
    assert!(lines[0].starts_with("Reference file has size "), "{}", lines[0]);
    assert!(lines.contains(&"Reference band 1 has no nodata value, but input band has no data value 0.000000."));
    assert!(lines.contains(&"1: maximum pixel value difference: 4.000000"));
    let (code, json, _) = terrio(&["compare", "--json", "--skip-all-optional", &a, &b], None);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["difference_count"].as_u64().unwrap() as i32, code);
    assert_eq!(v["bands"][0]["pixels_differing"], 1);
    assert_eq!(v["bands"][0]["max_abs_diff"], 4.0);
}

#[test]
fn pixel_info_reads_positions_from_arguments_and_standard_input() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(dir.path(), "a.tif", (0..8).map(f64::from).collect(), None);
    let (code, out, _) = terrio(&["pixel-info", "-f", "text", &a, "1.5", "1.5"], None);
    assert_eq!((code, out.as_str()), (0, "Report:\n  Location: (1P,1L)\n  Band 1:\n    Value: 5\n"));
    let (code, out, _) = terrio(&["pixel-info", "-f", "value", "--position-crs", "dataset", &a], Some("500015 4000005 a b\n600000 0\n"));
    assert_eq!((code, out.as_str()), (1, "5\n\n"));
    let (_, out, _) = terrio(&["pixel-info", "-f", "csv", &a, "-p", "0.5,0.5"], None);
    assert_eq!(out, "geom_x,geom_y,extra_content,column,line,band_1_raw_value,band_1_unscaled_value\n0.5,0.5,,0.5,0.5,0,0\n");
    let (_, out, _) = terrio(&["pixel-info", &a, "1", "1", "-r", "bilinear"], None);
    let g: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(g["features"][0]["properties"]["bands"][0]["raw_value"], 2.5);
    let output = Command::new(env!("CARGO_BIN_EXE_terrio"))
        .args(["pixel-info", "-f", "value", &a, "0.5", "0.5", "-o", "-"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"0\n");
    assert!(!dir.path().join("-").exists());
    let layer = dir.path().join("out.gpkg");
    let (code, _, err) = terrio(&["pixel-info", &a, "1.5", "0.5", "-o", layer.to_str().unwrap()], None);
    assert_eq!(code, 0, "{err}");
    let points = terrio::open_vector(layer.to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(points.layers[0].features.len(), 1);
}

#[test]
fn xyz_streams_lines() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(dir.path(), "a.tif", (0..8).map(f64::from).collect(), Some(3.0));
    let (code, out, _) = terrio(&["xyz", &a, "--skipnodata", "--csv"], None);
    assert_eq!(code, 0);
    assert_eq!(out.lines().count(), 7);
    assert_eq!(out.lines().next().unwrap(), "500005.000,4000015.000,0");
    let path = dir.path().join("a.xyz");
    assert_eq!(terrio(&["xyz", &a, path.to_str().unwrap(), "--srcwin", "2", "1", "2", "1"], None).0, 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "500025.000 4000005.000 6\n500035.000 4000005.000 7\n");
    // Refused options leave no output behind, and the input is never overwritten.
    let refused = dir.path().join("refused.xyz");
    assert_eq!(terrio(&["xyz", &a, refused.to_str().unwrap(), "--srcwin", "3", "0", "5", "1"], None).0, 1);
    assert!(!refused.exists());
    let before = std::fs::read(&a).unwrap();
    assert_eq!(terrio(&["xyz", &a, &a], None).0, 1);
    assert_eq!(std::fs::read(&a).unwrap(), before);
}

#[test]
fn pixel_info_refuses_missing_layers_and_positions() {
    let dir = tempfile::tempdir().unwrap();
    let a = write(dir.path(), "a.tif", (0..8).map(f64::from).collect(), None);
    let (code, _, err) = terrio(&["pixel-info", &a], Some(""));
    assert_eq!(code, 1);
    assert!(err.contains("no positions"), "{err}");
    let points = dir.path().join("p.geojson");
    std::fs::write(
        &points,
        r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[1,1]}}]}"#,
    )
    .unwrap();
    let (code, _, _) = terrio(&["pixel-info", "--position-dataset", points.to_str().unwrap(), "-l", "nope", &a], None);
    assert_eq!(code, 1);
}

#[test]
fn pixel_info_refuses_to_replace_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = write(dir.path(), "source.tif", vec![1.; 8], None);
    let bytes = std::fs::read(&path).unwrap();
    let (status, _, err) = terrio(&["pixel-info", &path, "0", "0", "-o", &path], None);
    assert_ne!(status, 0);
    assert!(err.contains("overwrite the input"));
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

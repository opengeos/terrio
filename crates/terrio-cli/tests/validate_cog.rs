//! `terrio validate-cog` through the command line and MCP.

use std::process::Command;

fn terrio(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).output().unwrap();
    (out.status.code().unwrap(), String::from_utf8(out.stdout).unwrap())
}

/// A 1300 x 900 Float32 raster written with `driver` (`None` for a striped GeoTIFF).
fn write(dir: &std::path::Path, name: &str, driver: Option<terrio::formats::RasterDriver>) -> String {
    let (cols, rows) = (1300, 900);
    let cfg = terrio::RasterConfig { cols, rows, cell_size: 10.0, data_type: terrio::DataType::F32, ..Default::default() };
    let values = (0..cols * rows).map(|i| (i % 1000) as f64).collect();
    let mut ds = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, values).unwrap(), false);
    ds.raster.crs.epsg = Some(32611);
    ds.set_geo_transform([500000.0, 10.0, 0.0, 4000000.0, 0.0, -10.0]);
    let path = dir.join(name);
    let options = terrio::RasterWriteOptions { driver, compression: Some("DEFLATE".into()), ..Default::default() };
    terrio::write_raster_dataset(&ds, path.to_str().unwrap(), &options).unwrap();
    path.to_str().unwrap().to_string()
}

#[test]
fn reports_cogs_and_other_tiffs() {
    let dir = tempfile::tempdir().unwrap();
    let cog = write(dir.path(), "cog.tif", Some(terrio::formats::RasterDriver::Cog));
    let strips = write(dir.path(), "strips.tif", None);

    let (code, out) = terrio(&["validate-cog", &cog]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("is a valid cloud optimized GeoTIFF"), "{out}");

    let (code, out) = terrio(&["validate-cog", &strips]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("The file is greater than 512xH or 512xW, but is not tiled"), "{out}");
    assert!(out.contains("it is recommended to include internal overviews"), "{out}");
    assert!(out.contains("is NOT a valid cloud optimized GeoTIFF"), "{out}");

    let (code, out) = terrio(&["validate-cog", "--json", &cog]);
    assert_eq!(code, 0, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["valid"], true);
    assert_eq!(v["overview_factors"], serde_json::json!([2, 4]));
    assert!(v["structural_metadata"].as_str().unwrap().contains("LAYOUT=IFDS_BEFORE_DATA"), "{v}");

    let (code, _) = terrio(&["validate-cog", &format!("{cog}.missing")]);
    assert_ne!(code, 0);
}

#[test]
fn gdal_profile_and_full_check_options_are_exposed() {
    let dir = tempfile::tempdir().unwrap();
    let cog = write(dir.path(), "cog.tif", Some(terrio::formats::RasterDriver::Cog));
    for full in ["yes", "no", "auto"] {
        let (code, output) = terrio(&["validate-cog", &cog, "--profile", "gdal", "--full-check", full, "--json"]);
        assert_eq!(code, 0, "{output}");
        let report: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(report["valid"], true);
    }
    assert_ne!(terrio(&["validate-cog", &cog, "--full-check", "yes"]).0, 0);
    assert_ne!(terrio(&["validate-cog", &cog, "--profile", "unknown"]).0, 0);
}

//! Raster-management CLI integration, including preservation on invalid requests.
use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(directory).output().unwrap()
}

fn ok(args: &[&str], directory: &Path) {
    let output = run(args, directory);
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn invalid_overwrite_preserves_existing_raster() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("existing.tif");
    ok(&["create-raster", "existing.tif", "--size", "8,8", "--burn", "7"], directory.path());
    let original = std::fs::read(&path).unwrap();
    for args in [
        vec!["create-raster", "existing.tif", "--overwrite", "--like", "missing.tif"],
        vec!["create-raster", "existing.tif", "--overwrite", "--size", "0,0"],
        vec!["create-raster", "existing.tif", "--overwrite", "--size", "8,8", "--burn", "1", "--burn", "2"],
    ] {
        assert_eq!(run(&args, directory.path()).status.code(), Some(1));
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
    ok(&["create-raster", "existing.tif", "--overwrite", "--like", "existing.tif", "--burn", "9"], directory.path());
    let result = terrio::open_raster(path.to_str().unwrap()).unwrap();
    assert!(result.raster.data.to_f64_vec().iter().all(|v| *v == 9.));
}

/// `convert -f COG --co OVERVIEW_RESAMPLING=AREA_MATCHED` keeps the valid area of a ragged band at
/// every level and records it in the band metadata; a raster without nodata is refused.
#[test]
fn convert_writes_area_matched_cog_overviews() {
    let directory = tempfile::tempdir().unwrap();
    let d = directory.path();
    let (cols, rows) = (301usize, 213usize);
    let nd = -9999.0;
    let values: Vec<f64> = (0..rows)
        .flat_map(|y| {
            (0..cols).map(move |x| {
                let centre = 100.0 + 60.0 * (x as f64 / 37.0).sin();
                let distance = (y as f64 - centre).abs();
                if distance < 4.5 { 4.5 - distance } else { nd }
            })
        })
        .collect();
    let valid = values.iter().filter(|v| **v != nd).count();
    let cfg = terrio::RasterConfig { cols, rows, nodata: nd, data_type: terrio::DataType::F32, ..Default::default() };
    let r = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, values).unwrap(), true);
    terrio::write_raster_dataset(&r, d.join("depth.tif").to_str().unwrap(), &Default::default()).unwrap();
    let convert = |input: &str, output: &str| {
        let args = ["convert", input, output, "-f", "COG", "--compress", "DEFLATE", "--co", "BLOCKSIZE=64"];
        run(&[&args[..], &["--co", "OVERVIEW_RESAMPLING=AREA_MATCHED"]].concat(), d)
    };
    let out = convert("depth.tif", "cog.tif");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut lazy = terrio::LazyRaster::open(d.join("cog.tif").to_str().unwrap()).unwrap();
    let sizes = lazy.overviews();
    assert_eq!(sizes, [(150, 106), (75, 53), (37, 26)]);
    let mut ratios = Vec::new();
    for (i, (w, h)) in sizes.iter().enumerate() {
        let level = lazy.read_window(terrio::Window::full(*w, *h), &terrio::WindowOptions { bands: vec![], overview: Some(i) }).unwrap();
        let kept = level.raster.data.to_f64_vec().iter().filter(|v| **v != nd).count();
        // round(valid * w * h / (cols * rows)), half to even.
        assert_eq!(kept, (valid as f64 * (w * h) as f64 / (cols * rows) as f64).round_ties_even() as usize, "{w}x{h}");
        ratios.push(kept as f64 * (cols * rows) as f64 / (w * h) as f64 / valid as f64);
    }
    let written = terrio::open_raster(d.join("cog.tif").to_str().unwrap()).unwrap();
    let item = |name: &str| written.band_metadata[0].items.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap();
    let stored: Vec<f64> = item("OVERVIEW_AREA_RATIOS").split(',').map(|v| v.parse().unwrap()).collect();
    assert_eq!(stored.len(), ratios.len());
    assert!(stored.iter().zip(&ratios).all(|(s, r)| (s - r).abs() < 1e-12), "{stored:?} {ratios:?}");
    assert_eq!(item("OVERVIEW_BASE_VALID_PIXELS"), valid.to_string());
    // Without a nodata value there is no valid area to keep.
    let cfg = terrio::RasterConfig { cols, rows, data_type: terrio::DataType::F32, ..Default::default() };
    let plain = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, vec![1.0; cols * rows]).unwrap(), false);
    terrio::write_raster_dataset(&plain, d.join("plain.tif").to_str().unwrap(), &Default::default()).unwrap();
    let out = convert("plain.tif", "plain_cog.tif");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("nodata"), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn invalid_overview_and_move_options_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    ok(&["create-raster", "input.tif", "--size", "32,32", "--burn", "7", "--crs", "EPSG:3857"], directory.path());
    ok(&["overviews", "input.tif", "2", "4"], directory.path());
    for level in ["0", "1"] {
        assert_eq!(run(&["overviews", "input.tif", "--refresh", "--levels", level], directory.path()).status.code(), Some(1));
    }
    for option in ["--stats", "--hist", "--unset-nodata", "--unset-all-metadata"] {
        assert_eq!(run(&["edit-raster", "input.tif", "--t-srs", "EPSG:4326", option], directory.path()).status.code(), Some(2));
    }
    assert_eq!(run(&["create-raster", "bad.png", "--like", "input.tif", "--copy-overviews"], directory.path()).status.code(), Some(1));
    assert!(!directory.path().join("bad.png").exists());
}

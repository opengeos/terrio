//! GDAL 3.13.2 reference behavior for per-dataset mask conversion and warping (#431).

use std::path::Path;
use std::process::Command;
use terrio::{DataType, LazyRaster, Raster, RasterConfig, RasterDataset, RasterWriteOptions, Window, WindowOptions};

fn run(dir: &Path, args: &[&str]) {
    let result = Command::new(env!("CARGO_BIN_EXE_terrio")).current_dir(dir).args(args).output().unwrap();
    assert!(result.status.success(), "{args:?}: {}", String::from_utf8_lossy(&result.stderr));
}

fn source(dir: &Path, external: bool) -> Vec<u8> {
    let (cols, rows) = (65, 35);
    let raster =
        Raster::from_data(RasterConfig { cols, rows, data_type: DataType::U8, ..Default::default() }, vec![100.; cols * rows]).unwrap();
    let mut dataset = RasterDataset::new(raster, false);
    dataset.set_geo_transform([0., 1., 0., rows as f64, 0., -1.]);
    dataset.raster.crs.epsg = Some(3857);
    let mask: Vec<u8> = (0..cols * rows).map(|i| if i % cols < 31 { 0 } else { 255 }).collect();
    let options = RasterWriteOptions {
        mask: (!external).then(|| mask.clone()),
        tile_size: Some(16),
        overviews: vec![2, 4],
        overview_resampling: Some("nearest".into()),
        ..Default::default()
    };
    terrio::write_raster_dataset(&dataset, dir.join("source.tif").to_str().unwrap(), &options).unwrap();
    if external {
        dataset.raster.data = Raster::from_data(
            RasterConfig { cols, rows, data_type: DataType::U8, ..Default::default() },
            mask.iter().map(|v| *v as f64).collect(),
        )
        .unwrap()
        .data;
        dataset.raster.metadata.push(("INTERNAL_MASK_FLAGS_1".into(), "2".into()));
        terrio::write_raster_dataset(
            &dataset,
            dir.join("source.tif.msk").to_str().unwrap(),
            &RasterWriteOptions { driver: Some(terrio::formats::RasterDriver::GTiff), mask: None, ..options },
        )
        .unwrap();
    }
    mask
}

#[test]
fn convert_retains_internal_and_sidecar_masks_and_selected_windows() {
    for external in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let mask = source(d, external);
        let mut input = terrio::open_raster_lazy(d.join("source.tif").to_str().unwrap()).unwrap();
        for format in ["GTiff", "COG"] {
            for (extra, window, overview) in [
                (vec![], Window::full(65, 35), None),
                (vec!["--srcwin", "27", "3", "17", "19"], Window::new(27, 3, 17, 19), None),
                (vec!["--overview", "0"], Window::full(33, 18), Some(0)),
                (vec!["--overview", "1", "--srcwin", "2", "1", "12", "7"], Window::new(2, 1, 12, 7), Some(1)),
            ] {
                let mut args = vec!["convert", "source.tif", "out.tif", "-f", format, "--tile-size", "16"];
                args.extend(extra);
                run(d, &args);
                let expected_mask = input.read_mask(window, overview).unwrap();
                let expected = input.read_window(window, &WindowOptions { overview, ..Default::default() }).unwrap();
                let mut out = terrio::open_raster_lazy(d.join("out.tif").to_str().unwrap()).unwrap();
                assert!(out.has_mask());
                assert_eq!(out.read_mask(Window::full(window.width, window.height), None).unwrap(), expected_mask);
                let actual = out.read().unwrap();
                assert_eq!(actual.raster.data.to_f64_vec(), expected.raster.data.to_f64_vec());
                assert_eq!(actual.geo_transform(), expected.geo_transform());
                assert_eq!(actual.raster.data_type, DataType::U8);
                if overview.is_none() && window.width == 65 {
                    assert_eq!(expected_mask, Some(mask.clone()));
                }
                for (i, (w, h)) in out.overviews().into_iter().enumerate() {
                    assert!(out.has_overview_mask(i));
                    assert!(out.read_mask(Window::full(w, h), Some(i)).unwrap().is_some());
                }
            }
        }
        // Replacing the source uses the whole-image fallback, which must retain validity too.
        run(d, &["convert", "source.tif", "source.tif", "-f", "COG", "--tile-size", "16"]);
        let mut out = terrio::open_raster_lazy(d.join("source.tif").to_str().unwrap()).unwrap();
        assert_eq!(out.read_mask(Window::full(65, 35), None).unwrap(), Some(mask));
    }
}

#[test]
fn warp_uses_masks_in_streamed_memory_and_multiple_source_paths() {
    for external in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let mask = source(d, external);
        for inputs in [vec!["source.tif"], vec!["source.tif", "source.tif"]] {
            for overview in ["NONE", "AUTO", "0"] {
                let mut args = vec!["warp"];
                args.extend(inputs.clone());
                args.extend(["warped.tif", "--ts", "65", "35", "--overview", overview]);
                run(d, &args);
                let output = terrio::open_raster(d.join("warped.tif").to_str().unwrap()).unwrap();
                if overview != "0" {
                    assert_eq!(output.raster.data.to_f64_vec(), mask.iter().map(|v| if *v == 0 { 0. } else { 100. }).collect::<Vec<_>>());
                }
            }
        }
        let options = terrio::ops::warp::WarpOptions { overview: terrio::ops::overview::OverviewSelection::None, ..Default::default() };
        let mut lazy = terrio::open_raster_lazy(d.join("source.tif").to_str().unwrap()).unwrap();
        for mut input in [LazyRaster::from_masked_dataset(lazy.read().unwrap(), mask.clone()).unwrap(), lazy] {
            let output = terrio::ops::warp::warp_lazy(&mut input, &options).unwrap();
            assert_eq!(output.raster.data.to_f64_vec(), mask.iter().map(|v| if *v == 0 { 0. } else { 100. }).collect::<Vec<_>>());
        }
    }
}

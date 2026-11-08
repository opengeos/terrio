//! `terrio translate` reads a resized output from overviews, as `gdal_translate` does: the one
//! GDAL's `RasterIO` chooses, or the level `--overview` names.

use std::path::{Path, PathBuf};
use std::process::Command;
use terrio::ops::Resampling;
use terrio::ops::overview::OverviewSelection;
use terrio::raster::RasterData;
use terrio::{ResampledReadOptions, Window, WindowOptions};

/// A raster of 64 x 48 pixels, three Byte bands and a mask, with overviews of 32 x 24, 16 x 12
/// and 8 x 6 pixels, each with a mask of its own.
fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../terrio/tests/data/resampled_reads/rgb_mask.tif")
}

fn translate(dir: &Path, args: &[&str]) -> terrio::RasterDataset {
    let out = dir.join("out.tif");
    let result = Command::new(env!("CARGO_BIN_EXE_terrio")).arg("translate").arg(fixture()).arg(&out).args(args).output().unwrap();
    assert!(result.status.success(), "{args:?}: {}", String::from_utf8_lossy(&result.stderr));
    terrio::open_raster(out.to_str().unwrap()).unwrap()
}

fn bytes(data: &RasterData) -> Vec<u8> {
    match data {
        RasterData::U8(v) => v.clone(),
        other => panic!("not Byte: {:?}", other.data_type()),
    }
}

#[test]
fn resized_outputs_are_read_from_overviews() {
    let dir = tempfile::tempdir().unwrap();
    let mut src = terrio::open_raster_lazy(fixture().to_str().unwrap()).unwrap();
    let overview = |src: &mut terrio::LazyRaster, i: usize, w: Window| {
        bytes(&src.read_window(w, &WindowOptions { bands: vec![], overview: Some(i) }).unwrap().raster.data)
    };
    // Half the size: overview 0, copied.
    let out = translate(dir.path(), &["--outsize", "32", "24"]);
    assert_eq!(bytes(&out.raster.data), overview(&mut src, 0, Window::full(32, 24)));
    assert_eq!(out.geo_transform(), [500000.0, 60.0, 0.0, 4100000.0, 0.0, -60.0]);
    // From full resolution, by nearest neighbour.
    let out = translate(dir.path(), &["--outsize", "32", "24", "--overview", "NONE"]);
    let none = ResampledReadOptions { size: (32, 24), overview: OverviewSelection::None, ..Default::default() };
    assert_eq!(bytes(&out.raster.data), bytes(&src.read_resampled(&none).unwrap().raster.data));
    assert_ne!(bytes(&out.raster.data), overview(&mut src, 0, Window::full(32, 24)));
    // A level alone, at its resolution without an output size (`gdal_translate -ovr 1`).
    let out = translate(dir.path(), &["--ovr", "1", "--srcwin", "8", "4", "40", "32"]);
    assert_eq!((out.raster.cols, out.raster.rows), (10, 8));
    assert_eq!(bytes(&out.raster.data), overview(&mut src, 1, Window::new(2, 1, 10, 8)));
    assert_eq!(out.geo_transform(), [500240.0, 120.0, 0.0, 4099880.0, 0.0, -120.0]);
}

#[test]
fn resampled_outputs_are_resampled_reads() {
    let dir = tempfile::tempdir().unwrap();
    let mut src = terrio::open_raster_lazy(fixture().to_str().unwrap()).unwrap();
    let read = |src: &mut terrio::LazyRaster, bands: Vec<usize>, resampling| {
        let o = ResampledReadOptions { bands, window: Some((3.0, 5.0, 54.0, 38.0)), size: (13, 9), resampling, ..Default::default() };
        bytes(&src.read_resampled(&o).unwrap().raster.data)
    };
    let out = translate(dir.path(), &["--srcwin", "3", "5", "54", "38", "--outsize", "13", "9", "-r", "cubic"]);
    assert_eq!(bytes(&out.raster.data), read(&mut src, vec![], Resampling::Cubic));
    // Bands that are not the first ones in order are read band by band, as gdal_translate's
    // virtual dataset reads them: each as a read of that band alone.
    let out = translate(dir.path(), &["--srcwin", "3", "5", "54", "38", "--outsize", "13", "9", "-r", "cubic", "-b", "3", "-b", "1"]);
    let mut expected = read(&mut src, vec![3], Resampling::Cubic);
    expected.extend(read(&mut src, vec![1], Resampling::Cubic));
    assert_eq!(bytes(&out.raster.data), expected);
}

#[test]
fn overview_selections_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_terrio"))
        .arg("translate")
        .arg(fixture())
        .arg(dir.path().join("out.tif"))
        .args(["--outsize", "8", "6", "--overview", "SOME"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("SOME"));
}

#[test]
fn windows_past_the_edge_are_read_by_window() {
    // A window past the right edge by less than an output pixel is requested past the edge in
    // proportion, as gdal_translate's VRT source requests it (GDAL 3.12.1 writes these values),
    // rather than translated from the raster read whole.
    let dir = tempfile::tempdir().unwrap();
    let mut src = terrio::open_raster_lazy(fixture().to_str().unwrap()).unwrap();
    for (name, resampling) in [("near", Resampling::Nearest), ("bilinear", Resampling::Bilinear), ("average", Resampling::Average)] {
        let out = translate(dir.path(), &["--srcwin", "10", "5", "54.3", "20", "--outsize", "13", "9", "-r", name]);
        let o = ResampledReadOptions { window: Some((10.0, 5.0, 54.3, 20.0)), size: (13, 9), resampling, ..Default::default() };
        assert_eq!(bytes(&out.raster.data), bytes(&src.read_resampled(&o).unwrap().raster.data), "{name}");
    }
}

#[cfg(feature = "gdal")]
#[test]
fn gdal_named_inputs_are_translated_whole() {
    // The lazy reader refuses `gdal:` names: they are read whole through GDAL, as before.
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("out.tif");
    let input = format!("gdal:{}", fixture().display());
    let mut expected = None;
    for overview in ["AUTO", "AUTO-0"] {
        let result = Command::new(env!("CARGO_BIN_EXE_terrio"))
            .args(["translate", &input])
            .arg(&out)
            .args(["--outsize", "32", "24", "--overview", overview])
            .output()
            .unwrap();
        assert!(result.status.success(), "{overview}: {}", String::from_utf8_lossy(&result.stderr));
        let raster = terrio::open_raster(out.to_str().unwrap()).unwrap();
        assert_eq!((raster.raster.cols, raster.raster.rows, raster.raster.bands), (32, 24, 3));
        if let Some(values) = &expected {
            assert_eq!(&bytes(&raster.raster.data), values);
        } else {
            expected = Some(bytes(&raster.raster.data));
        }
    }
}

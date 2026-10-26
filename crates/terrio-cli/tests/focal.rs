//! `terrio neighbors`, `clean-collar`, `nearblack` and `enhance` through the real executable.

use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(dir).output().unwrap()
}

fn ok(args: &[&str], dir: &Path) {
    let r = run(args, dir);
    assert!(r.status.success(), "{args:?}: {}", String::from_utf8_lossy(&r.stderr));
}

fn fails(args: &[&str], dir: &Path) -> String {
    let r = run(args, dir);
    assert!(!r.status.success(), "{args:?} succeeded");
    String::from_utf8_lossy(&r.stderr).into_owned()
}

/// A georeferenced single-band raster of `values`, `w` pixels wide.
fn write(path: &Path, w: usize, values: Vec<f64>, data_type: terrio::DataType, nodata: Option<f64>) {
    let cfg = terrio::RasterConfig {
        cols: w,
        rows: values.len() / w,
        data_type,
        cell_size: 10.,
        nodata: nodata.unwrap_or(0.),
        ..Default::default()
    };
    let mut r = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, values).unwrap(), nodata.is_some());
    r.raster.crs.epsg = Some(32611);
    terrio::write_raster_dataset(&r, path.to_str().unwrap(), &Default::default()).unwrap();
}

/// A 6x5 RGB Byte image with a dark, noisy collar around a bright middle.
fn collar_image(path: &Path) {
    let (w, h) = (6, 5);
    let mut values = Vec::new();
    for b in 0..3 {
        for y in 0..h {
            for x in 0..w {
                let inside = (1..w - 1).contains(&x) && (1..h - 1).contains(&y);
                values.push(if inside { 120. + b as f64 } else { ((x + y + b) % 7) as f64 });
            }
        }
    }
    let cfg = terrio::RasterConfig { cols: w, rows: h, bands: 3, data_type: terrio::DataType::U8, cell_size: 1., ..Default::default() };
    let r = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, values).unwrap(), false);
    terrio::write_raster_dataset(&r, path.to_str().unwrap(), &Default::default()).unwrap();
}

#[test]
fn neighbors_computes_focal_statistics_with_gdal_defaults() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    write(&d.join("dem.tif"), 3, vec![1., 2., 3., 4., -1., 6., 7., 8., 9.], terrio::DataType::F32, Some(-1.));
    ok(&["neighbors", "dem.tif", "mean.tif", "--kernel", "equal"], d);
    let mean = terrio::open_raster(d.join("mean.tif").to_str().unwrap()).unwrap();
    assert_eq!(mean.raster.data_type, terrio::DataType::F64);
    assert_eq!(mean.band_nodata(1), Some(-1.));
    assert_eq!(mean.raster.get(0, 1, 1), -1.);
    assert_eq!(mean.raster.get(0, 0, 0), (1. + 1. + 2. + 1. + 1. + 2. + 4. + 4.) / 8.);
    ok(
        &[
            "neighbors",
            "dem.tif",
            "max.tif",
            "--kernel",
            "[[0,1,0],[1,1,1],[0,1,0]]",
            "--method",
            "max",
            "--ot",
            "Int16",
            "--nodata",
            "none",
        ],
        d,
    );
    let max = terrio::open_raster(d.join("max.tif").to_str().unwrap()).unwrap();
    assert_eq!(max.raster.data_type, terrio::DataType::I16);
    assert_eq!(max.band_nodata(1), None);
    assert_eq!(max.raster.get(0, 0, 1), 3.);
    assert!(fails(&["neighbors", "dem.tif", "x.tif", "--kernel", "sharpen", "--size", "5"], d).contains("size 5"));
    assert!(fails(&["neighbors", "dem.tif", "x.tif", "--kernel", "[[1,1],[1,1]]"], d).contains("odd square"));
    assert!(
        fails(&["neighbors", "dem.tif", "x.tif", "--kernel", "equal", "--ot", "Byte", "--nodata", "-5"], d).contains("cannot represent")
    );
}

#[test]
fn clean_collar_writes_alpha_and_updates_in_place_safely() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    collar_image(&d.join("scene.tif"));
    ok(&["clean-collar", "scene.tif", "clean.tif", "--add-alpha", "--pixel-distance", "0"], d);
    let clean = terrio::open_raster(d.join("clean.tif").to_str().unwrap()).unwrap();
    assert_eq!(clean.band_count(), 4);
    assert_eq!(clean.band_color_interp(4), "Alpha");
    assert_eq!(clean.raster.get(0, 0, 0), 0.);
    assert_eq!(clean.raster.get(3, 0, 0), 0.);
    assert_eq!(clean.raster.get(0, 2, 2), 120.);
    assert_eq!(clean.raster.get(3, 2, 2), 255.);
    ok(&["nearblack", "clean.tif", "rgba.tif", "--color", "black", "--nb", "0"], d);
    ok(&["nearblack", "clean.tif", "rgba-alpha.tif", "--color", "black", "--add-alpha", "--nb", "0"], d);
    // nearblack's two passes with a mask.
    ok(&["nearblack", "scene.tif", "masked.tif", "--add-mask", "--nb", "0"], d);
    let mut masked = terrio::open_raster_lazy(d.join("masked.tif").to_str().unwrap()).unwrap();
    let mask = masked.read_mask(terrio::Window::full(6, 5), None).unwrap().unwrap();
    assert_eq!((mask[0], mask[2 * 6 + 2]), (0, 255));

    // In place: the input is replaced only once the result is written.
    let before = std::fs::read(d.join("scene.tif")).unwrap();
    let files_before: std::collections::BTreeSet<_> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert!(fails(&["clean-collar", "scene.tif"], d).contains("--update"));
    assert!(fails(&["clean-collar", "--update", "scene.tif", "--compress", "NO-SUCH-CODEC"], d).contains("NO-SUCH-CODEC"));
    assert_eq!(std::fs::read(d.join("scene.tif")).unwrap(), before);
    let files_after: std::collections::BTreeSet<_> = std::fs::read_dir(d).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(files_before, files_after, "no temporary file is left");
    ok(&["clean-collar", "--update", "scene.tif", "--pixel-distance", "0"], d);
    let updated = terrio::open_raster(d.join("scene.tif").to_str().unwrap()).unwrap();
    assert_eq!(updated.band_count(), 3);
    assert_eq!((updated.raster.get(0, 0, 0), updated.raster.get(0, 2, 2)), (0., 120.));
    // Formats other than GeoTIFF and read-only files are refused, unchanged.
    write(&d.join("grid.asc"), 2, vec![1., 2., 3., 4.], terrio::DataType::F32, None);
    let asc = std::fs::read(d.join("grid.asc")).unwrap();
    assert!(fails(&["clean-collar", "--update", "grid.asc"], d).contains("GeoTIFF"));
    assert_eq!(std::fs::read(d.join("grid.asc")).unwrap(), asc);
    let mut perms = std::fs::metadata(d.join("scene.tif")).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(d.join("scene.tif"), perms).unwrap();
    assert!(fails(&["clean-collar", "--update", "scene.tif"], d).contains("read-only"));
}

#[test]
fn enhance_equalizes_and_reads_and_writes_lut_files() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    write(&d.join("image.tif"), 4, (0..16).map(|v| (v * 16) as f64).collect(), terrio::DataType::U8, None);
    ok(&["enhance", "image.tif", "--equalize", "--config", "lut.txt"], d);
    let text = std::fs::read_to_string(d.join("lut.txt")).unwrap();
    assert!(text.starts_with("1:Band -0.5:ScaleMin 255.5:ScaleMax "), "{text}");
    assert_eq!(text.split_whitespace().count(), 3 + 256);
    assert!(fails(&["enhance", "image.tif", "--config", "lut.txt"], d).contains("give an output"));
    assert_eq!(std::fs::read_to_string(d.join("lut.txt")).unwrap(), text);
    let input = std::fs::read(d.join("image.tif")).unwrap();
    assert!(fails(&["enhance", "image.tif", "--equalize", "--config", "image.tif"], d).contains("must not be the input"));
    assert_eq!(std::fs::read(d.join("image.tif")).unwrap(), input);
    ok(&["enhance", "image.tif", "eq.tif", "--equalize"], d);
    ok(&["enhance", "image.tif", "lut.tif", "--config", "lut.txt"], d);
    let (eq, lut) = (
        terrio::open_raster(d.join("eq.tif").to_str().unwrap()).unwrap(),
        terrio::open_raster(d.join("lut.tif").to_str().unwrap()).unwrap(),
    );
    assert_eq!(eq.raster.data.to_f64_vec(), lut.raster.data.to_f64_vec());
    ok(&["enhance", "image.tif", "linear.tif", "--src-scale", "0", "256"], d);
    let linear = terrio::open_raster(d.join("linear.tif").to_str().unwrap()).unwrap();
    assert_eq!(linear.raster.get(0, 0, 1), 16.);
    assert!(fails(&["enhance", "image.tif", "x.tif"], d).contains("--equalize"));
    std::fs::write(d.join("bad.txt"), "1:Band 0:ScaleMin 1:ScaleMax 1 2 3\n").unwrap();
    assert!(fails(&["enhance", "image.tif", "x.tif", "--config", "bad.txt"], d).contains("corrupt"));
}

#[test]
fn non_byte_update_warns_and_matches_gdal_byte_conversion() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    write(&d.join("float.tif"), 4, vec![0., 500., 123.75, 0.], terrio::DataType::F32, None);
    let result = run(&["nearblack", "float.tif", "--update", "--pixel-distance", "0"], d);
    assert!(result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("clamped or rounded"));
    let output = terrio::open_raster(d.join("float.tif").to_str().unwrap()).unwrap();
    assert_eq!(output.raster.data.to_f64_vec(), vec![0., 255., 124., 0.]);
    assert!(fails(&["enhance", "float.tif", "out.tif", "--equalize", "--config", "lut.txt"], d).contains("requires no raster output"));
}

#[cfg(unix)]
#[test]
fn collar_update_follows_source_symlink_and_preserves_mode() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.tif");
    write(&source, 4, vec![0., 120., 120., 0.], terrio::DataType::U8, None);
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o640)).unwrap();
    let link = dir.path().join("link.tif");
    symlink(&source, &link).unwrap();
    ok(&["nearblack", "link.tif", "--update", "--nb", "0"], dir.path());
    assert!(std::fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::metadata(source).unwrap().permissions().mode() & 0o777, 0o640);
}

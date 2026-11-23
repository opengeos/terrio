//! External EHdr overviews, with GDAL 3.13.2 as the interoperability reference.

#![cfg(not(target_family = "wasm"))]

use std::path::{Path, PathBuf};
use std::process::Command;
use terrio::ops::Resampling;
use terrio::ops::overview::{OverviewOptions, add_overviews, add_overviews_with_progress};
use terrio::{DataType, Error, LazyRaster, Window, WindowOptions};

const SIZE: (usize, usize) = (9, 7);
const LEVELS: [(usize, usize); 2] = [(5, 4), (3, 2)];

fn grid(dir: &Path, name: &str, data_type: DataType, nodata: Option<f64>) -> PathBuf {
    let path = dir.join(format!("{name}.flt"));
    let (bits, pixel) = match data_type {
        DataType::F32 => (32, "FLOAT"),
        DataType::I16 => (16, "SIGNEDINT"),
        _ => unreachable!(),
    };
    let mut header = format!(
        "NCOLS 9\nNROWS 7\nNBANDS 1\nNBITS {bits}\nPIXELTYPE {pixel}\nBYTEORDER I\nLAYOUT BIL\nULXMAP 141\nULYMAP -31\nXDIM 2\nYDIM 2\n"
    );
    if let Some(nd) = nodata {
        header.push_str(&format!("NODATA {nd}\n"));
    }
    let mut bytes = Vec::new();
    for y in 0..SIZE.1 {
        for x in 0..SIZE.0 {
            // An all-nodata first cell and mixed-validity neighbors at factor 2.
            let valid = !(x < 2 && y < 2 || x == 3 && y == 0);
            let value = (y * SIZE.0 + x) as f64;
            let value = if valid { value } else { nodata.unwrap_or(value) };
            match data_type {
                DataType::F32 => bytes.extend((value as f32).to_le_bytes()),
                DataType::I16 => bytes.extend((value as i16).to_le_bytes()),
                _ => unreachable!(),
            }
        }
    }
    std::fs::write(&path, bytes).unwrap();
    std::fs::write(path.with_extension("hdr"), header).unwrap();
    path
}

fn options() -> OverviewOptions {
    OverviewOptions { levels: vec![2, 4], external: true, resampling: Resampling::Average, ..Default::default() }
}

#[test]
fn builds_and_rebuilds_sidecar_without_changing_source() {
    let dir = tempfile::tempdir().unwrap();
    let path = grid(dir.path(), "grid", DataType::F32, Some(-0.1));
    let hdr = path.with_extension("hdr");
    let sidecar = dir.path().join("grid.flt.ovr");
    let before = [std::fs::read(&path).unwrap(), std::fs::read(&hdr).unwrap()];
    #[cfg(unix)]
    for file in [&path, &hdr] {
        let mut permissions = std::fs::metadata(file).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(file, permissions).unwrap();
    }
    let name = path.to_str().unwrap();
    let opts = options();
    assert!(LazyRaster::open(name).unwrap().overviews().is_empty());
    assert!(matches!(add_overviews_with_progress(name, &opts, &mut |_| false), Err(Error::Cancelled)));
    assert!(!sidecar.exists());
    let mut progress = Vec::new();
    assert_eq!(
        add_overviews_with_progress(name, &opts, &mut |f| {
            progress.push(f);
            true
        })
        .unwrap(),
        LEVELS
    );
    assert_eq!((progress.first(), progress.last()), (Some(&0.0), Some(&1.0)));
    assert!(progress.len() > 2 && progress.windows(2).all(|p| p[0] <= p[1]), "{progress:?}");

    let mut raster = LazyRaster::open(name).unwrap();
    assert_eq!(raster.overviews(), LEVELS);
    assert_eq!(raster.nodata(), Some(-0.1));
    assert!(raster.file_list().unwrap().contains(&sidecar.to_str().unwrap().to_owned()));
    let window = WindowOptions { overview: Some(0), ..Default::default() };
    let row = raster.read_window(Window::new(0, 0, 3, 1), &window).unwrap();
    assert_eq!(row.raster.data_type, DataType::F32);
    assert_eq!(row.nodata(), Some(-0.1_f32 as f64));
    assert_eq!(row.raster.data.to_f64_vec(), [-0.1_f32, 7.113_636_5, 8.563_637].map(f64::from));
    assert_eq!(row.geo_transform(), [140.0, 3.6, 0.0, -30.0, 0.0, -3.5]);
    // A selected overview reads its TIFF blocks without opening the raw data file.
    let moved = dir.path().join("original.flt");
    std::fs::rename(&path, &moved).unwrap();
    let shared = raster.shared().read_window(Window::new(0, 0, 3, 1), &window).unwrap();
    assert_eq!(shared.raster.data.to_f64_vec(), row.raster.data.to_f64_vec());
    std::fs::rename(&moved, &path).unwrap();
    drop(raster);

    let saved = std::fs::read(&sidecar).unwrap();
    for threshold in [0.3, 1.0] {
        let mut calls = Vec::new();
        assert!(matches!(
            add_overviews_with_progress(name, &opts, &mut |f| {
                calls.push(f);
                f < threshold
            }),
            Err(Error::Cancelled)
        ));
        assert!(calls.windows(2).all(|p| p[0] <= p[1]), "{calls:?}");
        let last = *calls.last().unwrap();
        assert!(last >= threshold && (last == 1.0) == (threshold == 1.0), "{calls:?}");
        assert_eq!(std::fs::read(&sidecar).unwrap(), saved);
    }
    assert_eq!([std::fs::read(&path).unwrap(), std::fs::read(&hdr).unwrap()], before);
    #[cfg(unix)]
    for file in [&path, &hdr] {
        assert!(std::fs::metadata(file).unwrap().permissions().readonly());
    }
}

fn gdal(tool: &str, args: &[&str]) -> Vec<u8> {
    let output = Command::new(tool).args(args).output().unwrap_or_else(|e| panic!("{tool}: {e}"));
    assert!(output.status.success(), "{tool} {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    output.stdout
}

/// Ask GDAL to select and export the overview itself, then compare its native samples and metadata.
fn check_levels(name: &str, raster: &mut LazyRaster, nodata: Option<f64>, tolerance: f64) {
    let dir = tempfile::tempdir().unwrap();
    let info: serde_json::Value = serde_json::from_slice(&gdal("gdalinfo", &["-json", name])).unwrap();
    assert_eq!(info["driverShortName"], "EHdr");
    let gd_levels = info["bands"][0]["overviews"].as_array().unwrap();
    assert_eq!(raster.overviews(), LEVELS);
    assert_eq!(gd_levels.len(), LEVELS.len());
    for (i, (w, h)) in LEVELS.into_iter().enumerate() {
        assert_eq!(gd_levels[i]["size"], serde_json::json!([w, h]));
        let dest = dir.path().join(format!("overview-{i}.bin"));
        gdal("gdal_translate", &["-q", "-of", "ENVI", "-ovr", &i.to_string(), name, dest.to_str().unwrap()]);
        let gd = LazyRaster::open(dest.to_str().unwrap()).unwrap().read().unwrap();
        let ovr = raster.read_window(Window::full(w, h), &WindowOptions { overview: Some(i), ..Default::default() }).unwrap();
        assert_eq!((gd.raster.cols, gd.raster.rows), (w, h));
        assert_eq!(ovr.raster.data_type, gd.raster.data_type);
        let no_data_equal = |a: Option<f64>, b: Option<f64>| a == b || matches!((a, b), (Some(x), Some(y)) if x.is_nan() && y.is_nan());
        assert!(no_data_equal(ovr.nodata(), nodata), "overview {i}: {:?}", ovr.nodata());
        assert!(no_data_equal(gd.nodata(), nodata), "GDAL overview {i}: {:?}", gd.nodata());
        for (pixel, (a, b)) in ovr.raster.data.to_f64_vec().into_iter().zip(gd.raster.data.to_f64_vec()).enumerate() {
            let invalid = |x: f64| nodata.is_some_and(|nd| nd == x || (nd.is_nan() && x.is_nan()));
            assert_eq!(invalid(a), invalid(b), "{name} overview {i}, pixel {pixel}: {a} vs {b}");
            assert!(invalid(a) || (a - b).abs() <= tolerance, "{name} overview {i}, pixel {pixel}: {a} vs {b}");
        }
    }
}

#[test]
#[ignore = "requires gdaladdo, gdalinfo and gdal_translate"]
fn gdal_and_terrio_read_both_builders_overviews() {
    for (data_type, nodata, resampling, compression) in [
        (DataType::F32, Some(-0.1_f32 as f64), Resampling::Average, "NONE"),
        (DataType::F32, Some(-9999.0), Resampling::Average, "DEFLATE"),
        (DataType::F32, Some(f64::NAN), Resampling::Average, "DEFLATE"),
        (DataType::I16, Some(-9999.0), Resampling::Average, "NONE"),
        (DataType::I16, None, Resampling::Nearest, "NONE"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let gd = grid(dir.path(), "gdal", data_type, nodata);
        let tr = grid(dir.path(), "terrio", data_type, nodata);
        let (gd, tr) = (gd.to_str().unwrap(), tr.to_str().unwrap());
        let method = if resampling == Resampling::Average { "average" } else { "nearest" };
        gdal("gdaladdo", &["-q", "-ro", "-r", method, "--config", "COMPRESS_OVERVIEW", compression, gd, "2", "4"]);
        let mut reference = LazyRaster::open(gd).unwrap();
        check_levels(gd, &mut reference, nodata, 0.0);

        // Compare Terrio's samples with GDAL's independently built levels. Then ask GDAL
        // to select the Terrio sidecar from its EHdr dataset and export those same levels.
        let opts = OverviewOptions { compression: Some(compression.into()), resampling, ..options() };
        assert_eq!(add_overviews(tr, &opts).unwrap(), LEVELS);
        let mut built = LazyRaster::open(tr).unwrap();
        let tolerance = if data_type == DataType::F32 { 4e-6 } else { 0.0 };
        check_levels(gd, &mut built, nodata, tolerance);
        check_levels(tr, &mut built, nodata, 0.0);
        drop(reference);

        // Rebuild only the first level of the GDAL sidecar and keep the other level.
        assert_eq!(add_overviews(gd, &OverviewOptions { levels: vec![2], ..opts }).unwrap(), LEVELS);
        check_levels(gd, &mut built, nodata, tolerance);
    }
}

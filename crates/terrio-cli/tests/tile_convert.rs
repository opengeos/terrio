//! `terrio convert` streams a GeoTIFF into a tile archive, with the zoom levels it is given: the
//! archive is the one the in-memory writer writes, byte for byte.
use std::path::Path;
use std::process::{Command, Output};

const MAX_GM: f64 = 6378137.0 * std::f64::consts::PI;

fn run(args: &[&str], directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(directory).output().unwrap()
}

/// A 3-band Byte raster in EPSG:3857 at zoom 11's resolution, covering 3 x 2 tiles exactly.
fn rgb_raster() -> terrio::RasterDataset {
    let res = 2.0 * MAX_GM / 256.0 / 2048.0;
    let (w, h) = (768, 512);
    let mut v = Vec::with_capacity(3 * w * h);
    for b in 0..3 {
        for y in 0..h {
            for x in 0..w {
                v.push(((x * 3 + y * 5 + b * 70) % 251) as f64);
            }
        }
    }
    let cfg = terrio::RasterConfig {
        cols: w,
        rows: h,
        bands: 3,
        x_min: -MAX_GM + 1000.0 * 256.0 * res,
        y_min: MAX_GM - 702.0 * 256.0 * res,
        cell_size: res,
        data_type: terrio::DataType::U8,
        crs: terrio::crs::SpatialRef::from_epsg(3857).unwrap().to_crs_info(),
        ..Default::default()
    };
    let data = terrio::raster::RasterData::from_f64_vec(terrio::DataType::U8, v);
    terrio::RasterDataset::new(terrio::Raster::from_data_native(cfg, data).unwrap(), false)
}

#[test]
fn streamed_convert_writes_the_archive_the_in_memory_writer_writes() {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("input.tif");
    terrio::write_raster_dataset(&rgb_raster(), input.to_str().unwrap(), &terrio::RasterWriteOptions::default()).unwrap();
    // Both archives are named `tiles.pmtiles` (in two directories): the archive's name and
    // description are its file name's stem.
    std::fs::create_dir(directory.path().join("streamed")).unwrap();
    std::fs::create_dir(directory.path().join("memory")).unwrap();
    let output = run(&["convert", "input.tif", "streamed/tiles.pmtiles", "--minzoom", "9", "--maxzoom", "11"], directory.path());
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let options = terrio::RasterWriteOptions {
        creation_options: vec![("MINZOOM".into(), "9".into()), ("MAXZOOM".into(), "11".into())],
        ..Default::default()
    };
    let whole = terrio::open_raster(input.to_str().unwrap()).unwrap();
    let memory = directory.path().join("memory").join("tiles.pmtiles");
    terrio::write_raster_dataset(&whole, memory.to_str().unwrap(), &options).unwrap();
    let streamed = std::fs::read(directory.path().join("streamed").join("tiles.pmtiles")).unwrap();
    assert!(!streamed.is_empty());
    assert!(streamed == std::fs::read(&memory).unwrap(), "the streamed archive differs from the in-memory one");
}

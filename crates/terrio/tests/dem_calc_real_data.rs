//! Terrain analysis and raster algebra on real-world files, with expected outputs taken from
//! GDAL 3.12 (`gdaldem`, `gdal_calc.py`, `gdal_proximity.py`, `gdal_sieve.py`).
//!
//! Each output is checked pixel for pixel through a checksum: the wrapping sum of the bit
//! patterns of every pixel value as `f64` (NaN made canonical), computed on GDAL's outputs.
//!
//! The files live outside the repository: set `TERRIO_TEST_DATA` to their directory (see
//! `scripts/fetch_test_data.sh`). Without it, these tests pass without checking anything.

use std::path::PathBuf;
use terrio::DataType;
use terrio::RasterDataset;
use terrio::ops::calc::{CalcInput, CalcOptions, calc};
use terrio::ops::dem::{ColorMap, DemOptions, DemProduct, Shading, color_relief, dem};
use terrio::ops::proximity::{ProximityOptions, proximity};
use terrio::ops::sieve::{SieveOptions, sieve};

fn data(name: &str) -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let p = dir.join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

fn checksum(ds: &RasterDataset) -> u64 {
    let r = &ds.raster;
    let mut sum = 0u64;
    for b in 0..r.bands {
        for row in 0..r.rows {
            for col in 0..r.cols {
                let v = r.get(b as isize, row as isize, col as isize);
                let v = if v.is_nan() { f64::NAN } else { v };
                sum = sum.wrapping_add(v.to_bits());
            }
        }
    }
    sum
}

fn check(ds: &RasterDataset, sum: u64, dt: DataType, nodata: Option<f64>, at_100_100: &[f64]) {
    assert_eq!(ds.raster.data_type, dt);
    assert_eq!(ds.nodata(), nodata);
    for (b, v) in at_100_100.iter().enumerate() {
        assert_eq!(ds.raster.get(b as isize, 100, 100), *v, "band {}", b + 1);
    }
    assert_eq!(checksum(ds), sum, "the pixels differ from GDAL's");
}

#[test]
fn hillshade_variants() {
    let Some(p) = data("usgs_dem.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let combined = dem(&ds, DemProduct::Hillshade, &DemOptions { shading: Shading::Combined, ..Default::default() }).unwrap();
    check(&combined, 15878988598666592256, DataType::U8, Some(0.0), &[239.0]);
    let o = DemOptions { shading: Shading::MultiDirectional, compute_edges: true, ..Default::default() };
    check(&dem(&ds, DemProduct::Hillshade, &o).unwrap(), 8798063347044974592, DataType::U8, Some(0.0), &[186.0]);
}

#[test]
fn slope_aspect() {
    let Some(p) = data("usgs_dem.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    // A geographic DEM: the scale is derived from the ellipsoid, as gdaldem does since 3.11.
    check(
        &dem(&ds, DemProduct::Slope, &DemOptions::default()).unwrap(),
        4411762426349879296,
        DataType::F32,
        Some(-9999.0),
        &[12.8257474899292],
    );
    let Some(p) = data("srtm90.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let o = DemOptions { slope_percent: true, scale: Some(111120.0), ..Default::default() };
    check(&dem(&ds, DemProduct::Slope, &o).unwrap(), 4079834113941438464, DataType::F32, Some(-9999.0), &[18.087970733642578]);
    check(
        &dem(&ds, DemProduct::Aspect, &DemOptions::default()).unwrap(),
        12480844632483168256,
        DataType::F32,
        Some(-9999.0),
        &[337.380126953125],
    );
}

#[test]
fn tri_tpi_roughness() {
    let Some(lv) = data("las_vegas_dem.tif") else { return };
    let ds = terrio::open_raster(&lv).unwrap();
    let edges = DemOptions { compute_edges: true, ..Default::default() };
    check(&dem(&ds, DemProduct::Tri, &edges).unwrap(), 12166556407352524800, DataType::F32, Some(-9999.0), &[0.10052847862243652]);
    check(
        &dem(&ds, DemProduct::Roughness, &DemOptions::default()).unwrap(),
        858595985746558976,
        DataType::F32,
        Some(-9999.0),
        &[0.08642578125],
    );
    let Some(p) = data("dem.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    check(
        &dem(&ds, DemProduct::Tpi, &DemOptions::default()).unwrap(),
        1321379996085780480,
        DataType::F32,
        Some(-9999.0),
        &[-0.7987518310546875],
    );
}

#[test]
fn color_relief_with_alpha() {
    let Some(p) = data("srtm90.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let text = "nv 0 0 0 0\n1500 46 154 88\n2500 224 108 31\n3500 215 244 244\n4000 white\n";
    let colors = ColorMap::parse(text, &ds, 1, None).unwrap();
    let out = color_relief(&ds, &colors, &DemOptions { alpha: true, ..Default::default() }).unwrap();
    check(&out, 14204775437191610368, DataType::U8, None, &[46.0, 154.0, 88.0, 255.0]);
}

#[test]
fn calc_like_gdal_calc() {
    let Some(p) = data("landsat.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let inputs = [CalcInput { name: "A".into(), ds: &ds, band: 3 }, CalcInput { name: "B".into(), ds: &ds, band: 4 }];
    let o = CalcOptions {
        calc: vec!["(B.astype(float32) - A) / (B.astype(float32) + A)".into()],
        output_type: Some(DataType::F32),
        ..Default::default()
    };
    check(&calc(&inputs, &o).unwrap(), 6150992430163296256, DataType::F32, Some(f32::MAX as f64), &[0.49450549483299255]);
    let o = CalcOptions { calc: vec!["A + B".into()], ..Default::default() };
    check(&calc(&inputs, &o).unwrap(), 15433202604301090816, DataType::U8, Some(255.0), &[91.0]);
    let Some(p) = data("srtm90.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let o = CalcOptions { calc: vec!["where(A > 2000, A / 7, -A)".into()], ..Default::default() };
    let out = calc(&[CalcInput { name: "A".into(), ds: &ds, band: 1 }], &o).unwrap();
    check(&out, 3878637218142617600, DataType::I16, Some(-32768.0), &[-807.0]);
}

#[test]
fn proximity_and_sieve() {
    let Some(p) = data("nlcd_knoxville.tif") else { return };
    let ds = terrio::open_raster(&p).unwrap();
    let o = ProximityOptions { values: vec![11], geo_units: true, ..Default::default() };
    check(&proximity(&ds, &o).unwrap(), 5472858509957660672, DataType::F32, None, &[3536.43896484375]);
    let o = SieveOptions { threshold: 20, eight_connected: true, ..Default::default() };
    check(&sieve(&ds, &o).unwrap(), 9095652766172315648, DataType::U8, None, &[41.0]);
}

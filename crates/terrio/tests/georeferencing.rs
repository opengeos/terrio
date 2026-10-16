//! Rotated and south-up geotransforms, ground control points, internal masks and GeoTIFFs written
//! in rows, checked against GDAL 3.12: the values `gdalwarp` gives for the same files, and GDAL's
//! reading of terrio's files.

use terrio::ops::gcp::GcpMethod;
use terrio::ops::warp::{WarpOptions, warp};
use terrio::raster::RasterData;
use terrio::{DataType, Gcp, GeoTiffWriter, RasterConfig, RasterDataset, RasterWriteOptions, Window, WindowOptions};

/// A raster of `bands` x `rows` x `cols` values `f(band, row, col)`, in EPSG:32611.
fn raster(cols: usize, rows: usize, bands: usize, gt: [f64; 6], f: impl Fn(usize, usize, usize) -> u8) -> RasterDataset {
    let mut data = Vec::with_capacity(cols * rows * bands);
    for b in 0..bands {
        for r in 0..rows {
            for c in 0..cols {
                data.push(f(b, r, c));
            }
        }
    }
    let cfg = RasterConfig {
        cols,
        rows,
        bands,
        data_type: DataType::U8,
        crs: terrio::crs::SpatialRef::from_epsg(32611).unwrap().to_crs_info(),
        ..Default::default()
    };
    let mut ds = RasterDataset::new(terrio::Raster::from_data_native(cfg, RasterData::U8(data)).unwrap(), false);
    ds.set_geo_transform(gt);
    ds
}

fn sums(ds: &RasterDataset) -> Vec<i64> {
    (0..ds.raster.bands).map(|b| ds.raster.band_view(b).as_slice().iter().map(|v| *v as i64).sum()).collect()
}

fn assert_gt(a: [f64; 6], b: [f64; 6]) {
    for (x, y) in a.iter().zip(b) {
        assert!((x - y).abs() <= 1e-9 * y.abs().max(1.0), "{a:?} != {b:?}");
    }
}

/// The 3-band raster of the rotated and south-up files.
fn small(gt: [f64; 6]) -> RasterDataset {
    raster(50, 40, 3, gt, |b, r, c| ((b * 2000 + r * 50 + c) % 251) as u8)
}

#[test]
fn rotated_and_south_up_round_trip_and_warp_as_gdal() {
    let dir = tempfile::tempdir().unwrap();
    // gdalwarp's output grids and band sums for these files (GDAL 3.12).
    type Expected<'a> = (&'a str, [f64; 6], WarpOptions, [f64; 6], (usize, usize), &'a [i64]);
    let rot = [500000.0, 10.0, 2.0, 4100000.0, 1.5, -10.0];
    let south = [500000.0, 10.0, 0.0, 4000000.0, 0.0, 10.0];
    let to_4326 = || Some(terrio::crs::SpatialRef::from_epsg(4326).unwrap());
    let cases: Vec<Expected> = vec![
        (
            "rot",
            rot,
            WarpOptions::default(),
            [500000.0, 10.383206422380136, 0.0, 4100075.0, 0.0, -10.383206422380136],
            (56, 46),
            &[238494, 238258, 238273],
        ),
        (
            "rot",
            rot,
            WarpOptions { dst_srs: to_4326(), resampling: terrio::ops::Resampling::Cubic, ..Default::default() },
            [-117.0, 0.00011166640706706097, 0.0, 37.04689841957796, 0.0, -0.00011166640706706097],
            (58, 38),
            &[207975, 207943, 207931],
        ),
        (
            "south",
            south,
            WarpOptions { resampling: terrio::ops::Resampling::Bilinear, ..Default::default() },
            [500000.0, 10.0, 0.0, 4000400.0, 0.0, -10.0],
            (50, 40),
            &[249028, 249092, 249156],
        ),
    ];
    for (name, gt, o, out_gt, size, expected) in cases {
        let path = dir.path().join(format!("{name}.tif"));
        let p = path.to_str().unwrap();
        let ds = small(gt);
        assert_eq!(ds.geo_transform(), gt);
        terrio::write_raster_dataset(&ds, p, &Default::default()).unwrap();
        let back = terrio::open_raster(p).unwrap();
        assert_eq!(back.geo_transform(), gt, "{name}");
        assert_eq!(back.raster.data_u8().unwrap(), ds.raster.data_u8().unwrap(), "{name}");
        let mut lazy = terrio::open_raster_lazy(p).unwrap();
        assert_eq!(lazy.geo_transform(), gt, "{name}");
        let w = lazy.read_window(Window::new(7, 5, 13, 10), &WindowOptions::default()).unwrap();
        assert_gt(w.geo_transform(), [gt[0] + 7.0 * gt[1] + 5.0 * gt[2], gt[1], gt[2], gt[3] + 7.0 * gt[4] + 5.0 * gt[5], gt[4], gt[5]]);
        let out = warp(&[&back], &o).unwrap();
        assert_gt(out.geo_transform(), out_gt);
        assert_eq!((out.raster.cols, out.raster.rows), size, "{name}");
        assert_eq!(sums(&out), expected, "{name}");
    }
    // Formats that cannot hold a rotation refuse it.
    let err = terrio::write_raster_dataset(&small(rot), dir.path().join("rot.asc").to_str().unwrap(), &Default::default());
    assert!(err.is_err());
}

/// The GCPs of a 150 x 120 raster: a 4 x 4 grid, with some distortion.
fn gcps() -> Vec<Gcp> {
    let mut out = Vec::new();
    for r in [0.0, 40.0, 80.0, 120.0] {
        for c in [0.0, 50.0, 100.0, 150.0] {
            let x = 500000.0 + c * 10.0 + r * 1.5 + 0.002 * c * r;
            let y = 4100000.0 - r * 10.0 + c * 0.8 - 0.0005 * c * c;
            out.push(Gcp { id: String::new(), info: String::new(), pixel: c, line: r, x, y, z: 0.0 });
        }
    }
    out
}

#[test]
fn gcps_round_trip_and_warp_as_gdalwarp() {
    let dir = tempfile::tempdir().unwrap();
    let ds = raster(150, 120, 1, [0.0, 1.0, 0.0, 120.0, 0.0, -1.0], |_, r, c| ((c * 7 + r * 3) % 251) as u8);
    let all = gcps();
    let corners: Vec<Gcp> = [0, 3, 12, 15].iter().map(|i| all[*i].clone()).collect();
    let utm = || terrio::crs::SpatialRef::from_epsg(32611).ok();
    let grid16 = [500000.0000000001, 10.586439352810354, 0.0, 4100108.750000001, 0.0, -10.586439352810354];
    // (GCPs, method, order, output CRS, resampling, gdalwarp's grid, size, sum)
    type Case<'a> = (&'a [Gcp], GcpMethod, u32, u32, terrio::ops::Resampling, [f64; 6], (usize, usize), i64);
    use terrio::ops::Resampling::{Bilinear, Nearest};
    let cases: Vec<Case> = vec![
        (&all, GcpMethod::Auto, 0, 32611, Nearest, grid16, (162, 124), 2045269),
        (
            &all,
            GcpMethod::Auto,
            1,
            32611,
            Nearest,
            [499991.0, 10.586439352810354, 0.0, 4100110.0, 0.0, -10.586439352810354],
            (162, 124),
            2044626,
        ),
        (&all, GcpMethod::Auto, 3, 32611, Nearest, grid16, (162, 124), 2045294),
        (&all, GcpMethod::Tps, 0, 32611, Nearest, grid16, (162, 124), 2044598),
        (
            &corners,
            GcpMethod::Auto,
            0,
            32611,
            Nearest,
            [500000.00000000006, 10.58643935281061, 0.0, 4100108.75, 0.0, -10.58643935281061],
            (162, 124),
            2043538,
        ),
        (&corners, GcpMethod::Tps, 0, 32611, Nearest, grid16, (162, 124), 2043895),
        (
            &all,
            GcpMethod::Auto,
            0,
            4326,
            Bilinear,
            [-117.0, 0.00011275124874449687, 0.0, 37.047201588192486, 0.0, -0.00011275124874449687],
            (171, 105),
            1826797,
        ),
    ];
    for (i, (points, method, order, epsg, resampling, gt, size, sum)) in cases.into_iter().enumerate() {
        let path = dir.path().join(format!("g{i}.tif"));
        let p = path.to_str().unwrap();
        let o = RasterWriteOptions { gcps: points.to_vec(), ..Default::default() };
        terrio::write_raster_dataset(&ds, p, &o).unwrap();
        let read = terrio::read_gcps(p).unwrap();
        assert_eq!(read.len(), points.len());
        assert!(read.iter().zip(points).all(|(a, b)| (a.pixel, a.line, a.x, a.y) == (b.pixel, b.line, b.x, b.y)));
        let src = terrio::open_raster(p).unwrap();
        assert_eq!(src.crs().epsg, Some(32611));
        let o = WarpOptions {
            gcps: read,
            gcp_method: method,
            order,
            dst_srs: terrio::crs::SpatialRef::from_epsg(epsg).ok().or_else(utm),
            resampling,
            ..Default::default()
        };
        let out = warp(&[&src], &o).unwrap();
        assert_gt(out.geo_transform(), gt);
        assert_eq!((out.raster.cols, out.raster.rows), size, "case {i}");
        assert_eq!(sums(&out), [sum], "case {i}");
    }
}

#[test]
fn internal_masks_and_rows_written_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let ds = small([500000.0, 10.0, 0.0, 4100000.0, 0.0, -10.0]);
    let mut mask = vec![255u8; 50 * 40];
    for r in 10..20 {
        for c in 5..30 {
            mask[r * 50 + c] = 0;
        }
    }
    let tiled = vec![("TILED", "YES"), ("BLOCKXSIZE", "16"), ("BLOCKYSIZE", "16")];
    let planar_tiles = [tiled.clone(), vec![("INTERLEAVE", "BAND")]].concat();
    // LERC (lossless here) is encoded by terrio's writer too, so rows are written with it.
    for (name, compression, co) in [
        ("tiles", "DEFLATE", tiled.clone()),
        ("strips", "DEFLATE", vec![("BLOCKYSIZE", "7")]),
        ("planar_strips", "DEFLATE", vec![("BLOCKYSIZE", "7"), ("INTERLEAVE", "BAND")]),
        ("planar_tiles", "ZSTD", planar_tiles),
        ("lerc_tiles", "LERC_ZSTD", tiled),
    ] {
        let creation_options: Vec<(String, String)> = co.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let o =
            RasterWriteOptions { compression: Some(compression.into()), creation_options, mask: Some(mask.clone()), ..Default::default() };
        let whole = dir.path().join(format!("{name}.tif"));
        terrio::write_raster_dataset(&ds, whole.to_str().unwrap(), &o).unwrap();
        // The same file, written 6 rows at a time (less than a block), and 40 (several blocks).
        let mut written = vec![whole];
        for step in [6, 40] {
            let rows = dir.path().join(format!("{name}_rows{step}.tif"));
            let mut header = ds.clone();
            header.raster.data = RasterData::U8(Vec::new());
            let mut w = GeoTiffWriter::create(rows.to_str().unwrap(), &header, &o).unwrap();
            let band = 50 * 40;
            for y in (0..40).step_by(step) {
                let n = step.min(40 - y);
                let mut chunk = Vec::new();
                for b in 0..3 {
                    chunk.extend_from_slice(&ds.raster.data_u8().unwrap()[b * band + y * 50..b * band + (y + n) * 50]);
                }
                w.write_rows(&RasterData::U8(chunk), n, Some(&mask[y * 50..(y + n) * 50])).unwrap();
            }
            assert_eq!(w.rows_written(), 40);
            w.finish().unwrap();
            written.push(rows);
        }
        for p in &written {
            let p = p.to_str().unwrap();
            let back = terrio::open_raster(p).unwrap();
            assert_eq!(back.raster.data_u8().unwrap(), ds.raster.data_u8().unwrap(), "{p}");
            assert_eq!(back.geo_transform(), ds.geo_transform(), "{p}");
            let mut lazy = terrio::open_raster_lazy(p).unwrap();
            assert!(lazy.has_mask(), "{p}");
            assert_eq!(lazy.read_mask(Window::new(0, 0, 50, 40), None).unwrap().unwrap(), mask, "{p}");
            let part = lazy.read_mask(Window::new(3, 8, 10, 5), None).unwrap().unwrap();
            assert_eq!(part, (8..13).flat_map(|r| (3..13).map(move |c| (r, c))).map(|(r, c)| mask[r * 50 + c]).collect::<Vec<_>>());
        }
    }
    // Rows must not go past the raster.
    let mut header = ds.clone();
    header.raster.data = RasterData::U8(Vec::new());
    let mut w = GeoTiffWriter::create(dir.path().join("x.tif").to_str().unwrap(), &header, &Default::default()).unwrap();
    assert!(w.write_rows(&RasterData::U8(vec![0; 50 * 41 * 3]), 41, None).is_err());
    assert!(w.finish().is_err());
}

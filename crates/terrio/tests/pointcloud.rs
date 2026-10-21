//! Point clouds: LAS, LAZ and COPC reading and writing, filters, gridding and conversion to
//! vectors.
//!
//! The synthetic tests run everywhere. The real-data tests read the files of
//! `scripts/fetch_test_data.sh` from `TERRIO_TEST_DATA` (they pass without checking anything when
//! it is not set); their expected values come from laspy 2.7 and PDAL 2.9
//! (`scripts/compare_pointclouds.py` compares every point of every file). The tests that read
//! public cloud files are ignored by default: run them with `cargo test -- --ignored`.

use std::path::PathBuf;
use terrio::crs::SpatialRef;
use terrio::pointcloud::grid::{GridOptions, GridStat, grid};
use terrio::pointcloud::{
    self, Dimension, PointCloud, PointFilter, PointFormat, PointReadOptions, PointWriteOptions, ReturnKind, ValueType, Values,
};

fn data(name: &str) -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let p = dir.join("pointcloud").join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

/// A deterministic pseudo-random sequence in [0, 1).
fn noise(n: usize, seed: u64) -> Vec<f64> {
    let mut s = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (s >> 11) as f64 / (1u64 << 53) as f64
        })
        .collect()
}

/// A synthetic point cloud with every standard dimension of a point format and two extra
/// dimensions, coordinates on the 0.01 grid.
fn synthetic(format: u8, n: usize) -> PointCloud {
    let r = noise(n * 8, format as u64 + 1);
    // Coordinates as LAS stores them: integers times the scale, plus the offset.
    let q = |v: f64, offset: f64| ((v - offset) / 0.01).round() * 0.01 + offset;
    let x: Vec<f64> = (0..n).map(|i| q(500_000.0 + r[i] * 1000.0, 500_000.0)).collect();
    let y: Vec<f64> = (0..n).map(|i| q(4_000_000.0 + r[n + i] * 1000.0, 4_000_000.0)).collect();
    let z: Vec<f64> = (0..n).map(|i| q(100.0 + r[2 * n + i] * 50.0, 0.0)).collect();
    let max_ret = if format >= 6 { 15.0 } else { 7.0 };
    let nr: Vec<u8> = (0..n).map(|i| 1 + (r[3 * n + i] * max_ret) as u8).collect();
    let rn: Vec<u8> = (0..n).map(|i| 1 + ((r[4 * n + i] * nr[i] as f64) as u8).min(nr[i] - 1)).collect();
    let class: Vec<u8> = (0..n).map(|i| (r[5 * n + i] * if format >= 6 { 255.0 } else { 31.0 }) as u8).collect();
    let mut dims = vec![
        Dimension::new("x", Values::F64(x)),
        Dimension::new("y", Values::F64(y)),
        Dimension::new("z", Values::F64(z)),
        Dimension::new("intensity", Values::U16((0..n).map(|i| (r[6 * n + i] * 65535.0) as u16).collect())),
        Dimension::new("return_number", Values::U8(rn)),
        Dimension::new("number_of_returns", Values::U8(nr)),
        Dimension::new("classification", Values::U8(class)),
        Dimension::new("point_source_id", Values::U16((0..n).map(|i| i as u16).collect())),
        Dimension::new("user_data", Values::U8((0..n).map(|i| (i % 256) as u8).collect())),
        Dimension::new("synthetic", Values::U8((0..n).map(|i| (i % 2) as u8).collect())),
        Dimension::new("edge_of_flight_line", Values::U8((0..n).map(|i| (i % 3 == 0) as u8).collect())),
    ];
    if format >= 6 {
        dims.push(Dimension::new("scan_angle", Values::I16((0..n).map(|i| (i as i16 % 20000) - 10000).collect())));
        dims.push(Dimension::new("overlap", Values::U8((0..n).map(|i| (i % 5 == 0) as u8).collect())));
        dims.push(Dimension::new("scanner_channel", Values::U8((0..n).map(|i| (i % 4) as u8).collect())));
    } else {
        dims.push(Dimension::new("scan_angle_rank", Values::I8((0..n).map(|i| ((i % 180) as i16 - 90) as i8).collect())));
    }
    if ![0, 2].contains(&format) {
        dims.push(Dimension::new("gps_time", Values::F64((0..n).map(|i| 1e8 + r[7 * n + i] * 1e5).collect())));
    }
    if [2, 3, 5, 7, 8, 10].contains(&format) {
        for (k, c) in ["red", "green", "blue"].iter().enumerate() {
            dims.push(Dimension::new(c, Values::U16((0..n).map(|i| (i * (k + 1)) as u16).collect())));
        }
    }
    if [8, 10].contains(&format) {
        dims.push(Dimension::new("nir", Values::U16((0..n).map(|i| (i * 7) as u16).collect())));
    }
    if [4, 5, 9, 10].contains(&format) {
        dims.push(Dimension::new("wavepacket_index", Values::U8(vec![1; n])));
        dims.push(Dimension::new("wavepacket_offset", Values::U64((0..n).map(|i| i as u64 * 1000).collect())));
        dims.push(Dimension::new("wavepacket_size", Values::U32(vec![256; n])));
        dims.push(Dimension::new("return_point_wave_location", Values::F32((0..n).map(|i| i as f32 * 0.5).collect())));
        for t in ["x_t", "y_t", "z_t"] {
            dims.push(Dimension::new(t, Values::F32((0..n).map(|i| i as f32 * 1e-3).collect())));
        }
    }
    dims.push(Dimension::new("height", Values::F32((0..n).map(|i| i as f32 * 0.25).collect())));
    let mut scaled = Dimension::new("deviation", Values::I32((0..n).map(|i| i as i32 - 500).collect()));
    scaled.scale = Some(0.01);
    scaled.offset = Some(10.0);
    dims.push(scaled);
    let mut cloud = pointcloud::from_dimensions(dims, SpatialRef::from_epsg(32617).unwrap());
    cloud.header.point_format = format;
    cloud.header.version = (1, if format >= 6 { 4 } else { 2 });
    cloud.header.scale = [0.01; 3];
    cloud.header.offset = [500_000.0, 4_000_000.0, 0.0];
    cloud
}

/// Asserts that two clouds have the same dimensions and values, in any order of dimensions.
fn assert_same_points(a: &PointCloud, b: &PointCloud, what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: point count");
    for d in &a.dimensions {
        let o = b.dim(&d.name).unwrap_or_else(|| panic!("{what}: no {}", d.name));
        assert_eq!(d.values, o.values, "{what}: {}", d.name);
        assert_eq!((d.scale, d.offset), (o.scale, o.offset), "{what}: scale and offset of {}", d.name);
    }
}

#[test]
fn every_point_format_round_trips_through_las_and_laz() {
    for format in 0..=10u8 {
        let cloud = synthetic(format, 2500);
        for ext in ["las", "laz"] {
            let bytes = pointcloud::write_points_bytes(&cloud, &format!("a.{ext}"), &Default::default()).unwrap();
            let mut r = pointcloud::open_points_bytes(bytes, "a").unwrap();
            assert_eq!(r.format_name(), ext.to_uppercase());
            assert_eq!(r.header().point_format, format);
            assert_eq!(r.crs().epsg, Some(32617));
            let back = r.read(&Default::default()).unwrap();
            assert_same_points(&cloud, &back, &format!("format {format} {ext}"));
            let h = r.header();
            assert_eq!(h.point_count, 2500);
            assert_eq!(h.min, [back.bounds().unwrap()[0], back.bounds().unwrap()[1], back.bounds().unwrap()[2]]);
            // Counts by return: 5 slots before LAS 1.4, 15 from it.
            let slots = h.points_by_return.len();
            let rn = cloud.dim("return_number").unwrap().to_f64();
            let expected = rn.iter().filter(|r| (1.0..=slots as f64).contains(*r)).count() as u64;
            assert_eq!(h.points_by_return.iter().sum::<u64>(), expected, "format {format}: counts by return");
        }
    }
}

#[test]
fn copc_round_trips_and_reads_by_level_and_bounds() {
    for format in [6u8, 7, 8] {
        let cloud = synthetic(format, 25_000);
        let options = PointWriteOptions {
            format: Some(PointFormat::Copc),
            copc_max_depth: Some(3),
            copc_leaf_points: Some(2000),
            ..Default::default()
        };
        let bytes = pointcloud::write_points_bytes(&cloud, "a.copc.laz", &options).unwrap();
        let mut r = pointcloud::open_points_bytes(bytes, "a").unwrap();
        assert_eq!(r.format_name(), "COPC");
        let nodes = r.copc_nodes().unwrap();
        assert_eq!(nodes.iter().map(|n| n.point_count as usize).sum::<usize>(), 25_000);
        // Every point once, whatever the order.
        let back = r.read(&Default::default()).unwrap();
        let key = |c: &PointCloud| {
            let mut v: Vec<(i64, u16)> = (0..c.len())
                .map(|i| {
                    (
                        (c.dim("x").unwrap().values.get_f64(i) * 100.0).round() as i64,
                        c.dim("point_source_id").unwrap().values.get_f64(i) as u16,
                    )
                })
                .collect();
            v.sort_unstable();
            v
        };
        assert_eq!(key(&cloud), key(&back), "format {format}");
        // Levels add up, and the root holds a sample of the whole extent.
        let info = *r.copc_info().unwrap();
        let mut previous = 0;
        for level in 0..=3 {
            let n = r.read(&PointReadOptions { max_level: Some(level), ..Default::default() }).unwrap().len();
            assert!(n > previous || (level > 1 && n == previous), "level {level}");
            previous = n;
        }
        assert_eq!(previous, 25_000);
        // A resolution is a level: spacing / 2^l needs levels 0 to l.
        let at = |res: f64, r: &mut pointcloud::PointCloudReader| {
            r.read(&PointReadOptions { resolution: Some(res), ..Default::default() }).unwrap().len()
        };
        let l1 = r.read(&PointReadOptions { max_level: Some(1), ..Default::default() }).unwrap().len();
        assert_eq!(at(info.spacing / 1.5, &mut r), l1);
        // Bounds: exactly the points inside, as filtering everything would give.
        let b = [500_200.0, 4_000_100.0, 500_450.0, 4_000_700.0];
        let filter = PointFilter { bounds: Some(b), ..Default::default() };
        let inside = r.read(&PointReadOptions { filter: filter.clone(), ..Default::default() }).unwrap();
        assert_eq!(key(&inside), key(&cloud.filter(&filter)));
        // max_points: the deepest level whose count fits.
        let limited = r.read(&PointReadOptions { max_points: Some(l1 as u64), ..Default::default() }).unwrap();
        assert_eq!(limited.len(), l1);
    }
}

#[test]
fn filters_keep_bounds_classes_and_returns() {
    let cloud = synthetic(6, 5000);
    let bytes = pointcloud::write_points_bytes(&cloud, "a.laz", &Default::default()).unwrap();
    let filter = PointFilter {
        bounds: Some([500_100.0, 4_000_100.0, 500_900.0, 4_000_500.0]),
        z_range: Some((110.0, 140.0)),
        classes: vec![2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30],
        returns: vec![ReturnKind::First, ReturnKind::Only],
    };
    let read = pointcloud::open_points_bytes(bytes, "a")
        .unwrap()
        .read(&PointReadOptions { filter: filter.clone(), ..Default::default() })
        .unwrap();
    let expected = cloud.filter(&filter);
    assert!(!read.is_empty() && read.len() < cloud.len() / 4);
    assert_same_points(&expected, &read, "filtered");
    for i in 0..read.len() {
        let rn = read.dim("return_number").unwrap().values.get_f64(i);
        let nr = read.dim("number_of_returns").unwrap().values.get_f64(i);
        assert!((rn == 1.0 && nr > 1.0) || nr == 1.0);
    }
}

#[test]
fn legacy_and_new_scan_angles_convert() {
    let cloud = synthetic(1, 100);
    let options = PointWriteOptions { point_format: Some(6), ..Default::default() };
    let bytes = pointcloud::write_points_bytes(&cloud, "a.las", &options).unwrap();
    let back = pointcloud::open_points_bytes(bytes, "a").unwrap().read(&Default::default()).unwrap();
    assert_eq!(back.header.version, (1, 4));
    let rank = cloud.dim("scan_angle_rank").unwrap();
    let angle = back.dim("scan_angle").unwrap();
    for i in 0..100 {
        assert_eq!(angle.values.get_f64(i), (rank.values.get_f64(i) / 0.006).round());
    }
}

#[test]
fn grids_statistics_as_pdal_defines_them() {
    // Four points; cells of 1 with the default radius (√2).
    let dims = vec![
        Dimension::new("x", Values::F64(vec![0.5, 1.5, 1.6, 3.0])),
        Dimension::new("y", Values::F64(vec![0.5, 0.5, 0.6, 2.0])),
        Dimension::new("z", Values::F64(vec![10.0, 20.0, 30.0, 40.0])),
    ];
    let cloud = PointCloud::new(dims, SpatialRef::default());
    let options = GridOptions { resolution: 1.0, stats: GridStat::all(), bounds: Some([0.0, 0.0, 3.0, 2.0]), ..Default::default() };
    let g = grid(&cloud, &options).unwrap();
    let r = &g.raster;
    assert_eq!((r.cols, r.rows, r.bands), (4, 3, 6));
    let at = |band: usize, row: usize, col: usize| r.band_view(band).as_slice()[row * r.cols + col];
    // Cell (0, 0), bottom row: the point at its centre and the two within √2 of it.
    let (row, col) = (2, 0);
    assert_eq!(at(0, row, col), 10.0);
    assert_eq!(at(1, row, col), 30.0);
    assert_eq!(at(2, row, col), 20.0);
    assert_eq!(at(3, row, col), 10.0, "a point at the centre gives the IDW value");
    assert_eq!(at(4, row, col), 3.0);
    assert!((at(5, row, col) - (200.0f64 / 3.0).sqrt()).abs() < 1e-12);
    // A cell no point reaches is nodata, except for the count.
    assert_eq!(at(0, 0, 0), -9999.0);
    assert_eq!(at(4, 0, 0), 0.0);
    // Bin mode: each point in its own cell.
    let bins = grid(&cloud, &GridOptions { bin_mode: true, stats: vec![GridStat::Count], ..options }).unwrap();
    let total: f64 = bins.raster.band_view(0).as_slice().iter().sum();
    assert_eq!(total, 4.0);
}

#[test]
fn points_become_vector_layers_and_back() {
    let cloud = synthetic(7, 300);
    let layer = pointcloud::vector::to_layer(&cloud, &Default::default()).unwrap();
    assert_eq!(layer.len(), 300);
    assert_eq!(layer.schema.fields().len(), cloud.dimensions.len() - 2);
    let back = pointcloud::vector::from_layer(&layer).unwrap();
    assert_eq!(back.len(), 300);
    for name in ["x", "y", "z", "intensity", "classification", "red"] {
        let a = cloud.dim(name).unwrap();
        let b = back.dim(name).unwrap();
        assert_eq!(a.values.value_type(), b.values.value_type(), "{name}");
        assert_eq!(a.to_f64(), b.to_f64(), "{name}");
    }
    assert_eq!(back.dim("deviation").unwrap().values.value_type(), ValueType::F64);
}

// Real-world files.

#[test]
fn reads_las_1_0_to_1_4_files_as_laspy_does() {
    let Some(p) = data("autzen_trim.laz") else { return };
    let cloud = pointcloud::read_points(&p, &Default::default()).unwrap();
    assert_eq!(cloud.len(), 110_000);
    let v = |name: &str, i: usize| cloud.dim(name).unwrap().values.get_f64(i);
    assert_eq!((v("x", 0), v("y", 0), v("z", 0)), (637177.98, 849393.9500000001, 411.19));
    assert_eq!((v("intensity", 0), v("classification", 0), v("red", 0)), (4.0, 1.0, 84.0));
    assert_eq!(v("gps_time", 0), 245379.39843682514);
    assert_eq!((v("x", 109_999), v("gps_time", 109_999)), (636037.88, 245385.91112104454));
    assert_eq!(cloud.classification_counts(), vec![(1, 83_893), (2, 26_107)]);
    let intensity: f64 = cloud.dim("intensity").unwrap().to_f64().iter().sum();
    assert_eq!(intensity, 11_220_547.0);
    // The same points uncompressed.
    let las = pointcloud::read_points(&data("autzen_trim.las").unwrap(), &Default::default()).unwrap();
    assert_eq!(las.dimensions, cloud.dimensions);
    // Versions 1.0 to 1.2, formats 0 to 3, with GeoKeys.
    for name in ["1.0_0.las", "1.0_1.las", "1.1_0.las", "1.1_1.las", "1.2_0.las", "1.2_1.las", "1.2_2.las", "1.2_3.las"] {
        let mut r = pointcloud::open_points(&data(name).unwrap()).unwrap();
        assert_eq!(r.crs().epsg, Some(26915), "{name}");
        let format: u8 = name[4..5].parse().unwrap();
        assert_eq!(r.header().point_format, format, "{name}");
        assert_eq!(r.read(&Default::default()).unwrap().len(), 1, "{name}");
    }
}

#[test]
fn reads_extra_bytes_and_crs_records() {
    let Some(p) = data("extrabytes.las") else { return };
    let cloud = pointcloud::read_points(&p, &Default::default()).unwrap();
    let v = |name: &str| cloud.dim(name).unwrap_or_else(|| panic!("no {name}")).values.get_f64(0);
    assert_eq!((v("Colors_0"), v("Colors_1"), v("Colors_2")), (68.0, 77.0, 88.0));
    assert_eq!((v("Flags_0"), v("Intensity"), v("Time")), (1.0, 143.0, 245_380.0));
    assert_eq!(cloud.dim("Time").unwrap().values.value_type(), ValueType::U64);
    // Two extra bytes records: both are read, as PDAL does.
    let several = pointcloud::open_points(&data("las_with_several_extra_byte_bloc.laz").unwrap()).unwrap();
    let names: Vec<String> = several.dimensions().into_iter().map(|d| d.0).collect();
    assert!(names.contains(&"Deviation".to_string()) && names.contains(&"confidence".to_string()));
    assert_eq!(several.crs().epsg, Some(2154));
    // WKT records: plain, compound, and with a vertical CRS inside the projected one.
    let r = pointcloud::open_points(&data("test1_4.las").unwrap()).unwrap();
    assert_eq!((r.crs().epsg, r.vertical_crs().0), (Some(2903), Some(5703)));
    let r = pointcloud::open_points(&data("simple.copc.laz").unwrap()).unwrap();
    assert_eq!((r.crs().epsg, r.vertical_crs().0), (Some(2991), Some(6360)));
    let r = pointcloud::open_points(&data("file_with_both_wkt_and_geotiff_vlrs.las").unwrap()).unwrap();
    // In US survey feet, NAD83(2011) / Nebraska (ftUS) as GDAL identifies it, not the zone in
    // metres (6516).
    assert_eq!(r.crs().epsg, Some(6880));
    let r = pointcloud::open_points(&data("test_utm16.las").unwrap()).unwrap();
    assert_eq!(r.crs().epsg, Some(26916));
    // User-defined GeoKeys in feet (Lambert conformal conic, NAD83 HARN): GDAL puts the first
    // corner at -123.0733952, 44.0499062.
    let mut r = pointcloud::open_points(&data("autzen_trim.las").unwrap()).unwrap();
    let w = pointcloud::info::point_cloud_info(&mut r, false).unwrap().wgs84_bounds.unwrap();
    assert!((w[0] - -123.0733952).abs() < 1e-4 && (w[1] - 44.0499062).abs() < 1e-4, "{w:?}");
    // EVLRs.
    let r = pointcloud::open_points(&data("1_4_w_evlr.laz").unwrap()).unwrap();
    assert_eq!(r.header().evlrs.len(), 1);
}

#[test]
fn reads_the_first_laszip_compressor() {
    // LASzip 1.2's unchunked compressor, which laspy (lazrs) and PDAL (lazperf) cannot read: the
    // same points as simple.laz.
    let Some(p) = data("simple-laszip-compressor-version-1.2r0.laz") else { return };
    let old = pointcloud::read_points(&p, &Default::default()).unwrap();
    let new = pointcloud::read_points(&data("simple.laz").unwrap(), &Default::default()).unwrap();
    assert_eq!(old.len(), 1065);
    assert_eq!(old.dimensions, new.dimensions);
}

#[test]
fn reads_copc_files_by_bounds_and_level_as_pdal_does() {
    let Some(p) = data("madison.copc.laz") else { return };
    let mut r = pointcloud::open_points(&p).unwrap();
    assert_eq!(r.format_name(), "COPC");
    let levels: Vec<usize> =
        (0..3).map(|l| r.read(&PointReadOptions { max_level: Some(l), ..Default::default() }).unwrap().len()).collect();
    assert_eq!(levels, vec![36_870, 422_783, 1_436_089]);
    // PDAL's readers.copc with the same bounds reads 371839 points.
    let bounds = [2354499.99, 1077499.996, 2355999.99, 1079499.992];
    let filter = PointFilter { bounds: Some(bounds), ..Default::default() };
    let inside = r.read(&PointReadOptions { filter, ..Default::default() }).unwrap();
    assert_eq!(inside.len(), 371_839);
    let all = r.read(&Default::default()).unwrap();
    assert_eq!(all.len(), 4_068_294);
    assert_eq!(
        all.classification_counts(),
        vec![(1, 85_630), (2, 1_790_151), (3, 570_182), (4, 592_670), (7, 377), (9, 5_827), (11, 1_023_457)]
    );
    let info = pointcloud::info::point_cloud_info(&mut r, false).unwrap();
    assert_eq!(info.crs.epsg, Some(2255));
    assert_eq!(info.copc.unwrap().points_per_level.iter().sum::<u64>(), 4_068_294);
}

#[test]
fn grids_like_pdal_writers_gdal() {
    // PDAL 2.9: `writers.gdal` with resolution 2, output_type all, data_type float64.
    let Some(p) = data("autzen_trim.laz") else { return };
    let cloud = pointcloud::read_points(&p, &Default::default()).unwrap();
    let g = grid(&cloud, &GridOptions { resolution: 2.0, stats: GridStat::all(), ..Default::default() }).unwrap();
    assert_eq!(g.geo_transform(), [636001.76, 2.0, 0.0, 849499.2000000001, 0.0, -2.0]);
    let r = &g.raster;
    assert_eq!((r.cols, r.rows), (589, 282));
    let expected = [
        (98922, 41905867.29000001, 409.48),
        (98922, 42516402.480000004, 451.38),
        (98922, 42178489.050201505, 430.23900000000003),
        (98922, 42176567.85525868, 426.7287813034756),
        (166098, 690697.0, 20.0),
        (98922, 226443.04373304988, 16.161371198014116),
    ];
    for (band, (count, sum, at)) in expected.iter().enumerate() {
        let v = r.band_view(band);
        let valid: Vec<f64> = v.as_slice().iter().copied().filter(|x| *x != -9999.0).collect();
        let total: f64 = valid.iter().sum();
        assert_eq!(valid.len(), *count, "band {band}");
        assert!((total - sum).abs() < 1e-6 * sum.abs(), "band {band}: {total} vs {sum}");
        assert!((v.as_slice()[100 * r.cols + 200] - at).abs() < 1e-9, "band {band}");
    }
}

#[test]
fn converts_with_reprojection_and_to_vector_layers() {
    let Some(p) = data("test_utm16.las") else { return };
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("wgs84.laz");
    let options = pointcloud::ConvertOptions { target_srs: Some(SpatialRef::from_epsg(4326).unwrap()), ..Default::default() };
    pointcloud::convert(&p, out.to_str().unwrap(), &options).unwrap();
    let back = pointcloud::read_points(out.to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(back.crs.epsg, Some(4326));
    assert_eq!(back.header.scale[0], 1e-7);
    // From gdaltransform (PROJ 9), stored to 1e-7 degrees.
    let x = back.dim("x").unwrap().values.get_f64(0);
    let y = back.dim("y").unwrap().values.get_f64(0);
    assert!((x - -83.4275978058665).abs() < 1e-7 && (y - 39.0125990690686).abs() < 1e-7, "{x} {y}");
    let gpkg = dir.path().join("points.gpkg");
    pointcloud::convert(&p, gpkg.to_str().unwrap(), &Default::default()).unwrap();
    let layer = &terrio::open_vector(gpkg.to_str().unwrap(), &Default::default()).unwrap().layers[0];
    assert_eq!(layer.len(), 10);
    assert_eq!(SpatialRef::from_layer(layer).epsg, Some(26916));
}

/// Zips files into an archive.
fn zip_files(archive: &std::path::Path, files: &[(&str, &std::path::Path)]) {
    use std::io::Write;
    let mut zip = zip::ZipWriter::new(std::fs::File::create(archive).unwrap());
    for (name, path) in files {
        zip.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
        zip.write_all(&std::fs::read(path).unwrap()).unwrap();
    }
    zip.finish().unwrap();
}

#[test]
fn opens_the_point_cloud_of_a_zip_archive() {
    // As swisstopo's swissSURFACE3D tiles: one point cloud in a zip, opened as the archive.
    let cloud = synthetic(1, 100);
    let dir = tempfile::tempdir().unwrap();
    let las = dir.path().join("tile.las");
    pointcloud::write_points(&cloud, las.to_str().unwrap(), &Default::default()).unwrap();
    let readme = dir.path().join("readme.txt");
    std::fs::write(&readme, "a tile").unwrap();
    let ply = dir.path().join("tile.ply");
    pointcloud::write_points(&cloud, ply.to_str().unwrap(), &Default::default()).unwrap();
    // Archives whose names do not tell what they hold: the listing does.
    for (archive, member) in [("tile.las.zip", &las), ("las.zip", &las), ("ply.zip", &ply)] {
        let zip = dir.path().join(archive);
        let name = member.file_name().unwrap().to_str().unwrap();
        zip_files(&zip, &[("readme.txt", &readme), (&format!("dir/{name}"), member)]);
        let path = zip.to_str().unwrap();
        assert!(pointcloud::is_point_cloud_path(path), "{archive}");
        let r = pointcloud::open_points(path).unwrap();
        assert_eq!(r.header().point_count, 100, "{archive}");
        assert_eq!(r.format_name(), if member == &ply { "PLY" } else { "LAS" }, "{archive}");
        let terrio::info::Info::PointCloud(info) = terrio::info::info_path(path, &Default::default()).unwrap() else {
            panic!("{archive}: not a point cloud")
        };
        assert_eq!(info.point_count, 100, "{archive}");
    }
    // A remote archive is not listed: its name tells.
    assert!(pointcloud::is_point_cloud_path("https://example.com/tile.las.zip"));
    assert!(!pointcloud::is_point_cloud_path("https://example.com/tile.zip"));
}

#[test]
#[ignore = "reads a public cloud file"]
fn reads_remote_copc_in_place() {
    let url = "https://s3.amazonaws.com/hobu-lidar/autzen-classified.copc.laz";
    let mut r = pointcloud::open_points(url).unwrap();
    assert_eq!(r.header().point_count, 10_653_336);
    let options = PointReadOptions {
        filter: PointFilter { bounds: Some([637000.0, 851000.0, 637500.0, 851500.0]), ..Default::default() },
        resolution: Some(7.27),
        ..Default::default()
    };
    // PDAL's readers.copc reads the same 18901 points.
    assert_eq!(r.read(&options).unwrap().len(), 18_901);
}

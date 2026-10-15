//! Point clouds: EPT, PLY and E57; reading and writing in chunks (files larger than memory);
//! waveforms; PDAL's filters (SMRF, outliers, ranges, sampling, voxels); TIN and
//! nearest-neighbour gridding.
//!
//! The synthetic tests run everywhere. The real-data tests read the files of
//! `scripts/fetch_test_data.sh` from `TERRIO_TEST_DATA` (they pass without checking anything when
//! it is not set); their expected values come from PDAL 2.9 and GDAL 3.12 (`gdal_grid`), and
//! `scripts/compare_pointclouds.py` compares every point. The tests that read public cloud data
//! are ignored by default: run them with `cargo test -- --ignored`.

use std::path::PathBuf;
use terrio::crs::SpatialRef;
use terrio::pointcloud::filters::{self, DimRange, OutlierMethod, OutlierOptions, SampleOptions, SmrfOptions, VoxelMode};
use terrio::pointcloud::grid::{GridMethod, GridOptions, grid};
use terrio::pointcloud::{
    self, Dimension, PlyEncoding, PointCloud, PointFilter, PointFormat, PointReadOptions, PointWriteOptions, PointWriter, Values, Vlr,
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

/// Points on the 0.01 grid over 100 x 100 m, with intensities, classes, returns and GPS times.
fn cloud(n: usize, seed: u64) -> PointCloud {
    let r = noise(n * 4, seed);
    let q = |v: f64| (v * 100.0).round() / 100.0;
    let dims = vec![
        Dimension::new("x", Values::F64((0..n).map(|i| q(500_000.0 + r[i] * 100.0)).collect())),
        Dimension::new("y", Values::F64((0..n).map(|i| q(4_000_000.0 + r[n + i] * 100.0)).collect())),
        Dimension::new("z", Values::F64((0..n).map(|i| q(100.0 + r[2 * n + i] * 10.0)).collect())),
        Dimension::new("intensity", Values::U16((0..n).map(|i| (i % 1000) as u16).collect())),
        Dimension::new("return_number", Values::U8(vec![1; n])),
        Dimension::new("number_of_returns", Values::U8(vec![1; n])),
        Dimension::new("classification", Values::U8((0..n).map(|i| (i % 7) as u8).collect())),
        Dimension::new("gps_time", Values::F64((0..n).map(|i| 1e5 + r[3 * n + i]).collect())),
    ];
    pointcloud::from_dimensions(dims, SpatialRef::from_epsg(32617).unwrap())
}

fn assert_same_points(a: &PointCloud, b: &PointCloud, what: &str) {
    assert_eq!(a.len(), b.len(), "{what}: point count");
    for d in &a.dimensions {
        let o = b.dim(&d.name).unwrap_or_else(|| panic!("{what}: no {}", d.name));
        assert_eq!(d.values, o.values, "{what}: {}", d.name);
    }
}

#[test]
fn ply_round_trips_in_ascii_and_binary() {
    let c = cloud(500, 1);
    for encoding in [PlyEncoding::Ascii, PlyEncoding::BinaryLittleEndian, PlyEncoding::BinaryBigEndian] {
        let options = PointWriteOptions { ply_encoding: encoding, ..Default::default() };
        let bytes = pointcloud::write_points_bytes(&c, "a.ply", &options).unwrap();
        let text = String::from_utf8_lossy(&bytes[..400]).to_string();
        // PDAL's names and sized types.
        assert!(text.contains("property float64 x") && text.contains("property uint8 returnnumber"), "{text}");
        assert!(text.contains("property float64 gpstime"), "{text}");
        let mut r = pointcloud::open_points_bytes(bytes, "a.ply").unwrap();
        assert_eq!(r.format_name(), "PLY");
        assert_eq!(r.header().point_count, 500);
        let back = r.read(&Default::default()).unwrap();
        assert_same_points(&c, &back, &format!("PLY {encoding:?}"));
    }
    // Fixed decimals, as PDAL's precision.
    let options = PointWriteOptions { ply_precision: Some(2), dimensions: vec!["x".into(), "y".into(), "z".into()], ..Default::default() };
    let bytes = pointcloud::write_points_bytes(&c, "a.ply", &options).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let first = text.lines().skip_while(|l| *l != "end_header").nth(1).unwrap();
    assert_eq!(first.split(' ').count(), 3);
    assert!(first.split(' ').all(|v| v.split('.').nth(1).is_some_and(|d| d.len() == 2)), "{first}");
}

#[test]
fn e57_round_trips() {
    let mut c = cloud(300, 2);
    for (k, name) in ["red", "green", "blue"].iter().enumerate() {
        c.dimensions.push(Dimension::new(name, Values::U16((0..300).map(|i| (i * 200 + k) as u16).collect())));
    }
    c.dimensions.push(Dimension::new("normal_z", Values::F64((0..300).map(|i| i as f64 / 300.0).collect())));
    let bytes = pointcloud::write_points_bytes(&c, "a.e57", &Default::default()).unwrap();
    let mut r = pointcloud::open_points_bytes(bytes, "a.e57").unwrap();
    assert_eq!(r.format_name(), "E57");
    let back = r.read(&Default::default()).unwrap();
    // Intensities and colours keep their values (limits of 0 to 65535); terrio's own dimensions
    // come back as doubles.
    for name in ["x", "y", "z", "intensity", "red", "normal_z"] {
        assert_eq!(back.dim(name).unwrap().to_f64(), c.dim(name).unwrap().to_f64(), "{name}");
    }
    for name in ["classification", "gps_time"] {
        assert_eq!(back.dim(name).unwrap().to_f64(), c.dim(name).unwrap().to_f64(), "{name}");
    }
}

#[test]
fn reads_in_chunks_the_points_read_whole() {
    let c = cloud(150_000, 3);
    let options = PointReadOptions { filter: PointFilter { classes: vec![1, 2, 3], ..Default::default() }, ..Default::default() };
    for name in ["a.las", "a.laz", "a.copc.laz", "a.ply"] {
        let bytes = pointcloud::write_points_bytes(&c, name, &Default::default()).unwrap();
        let mut r = pointcloud::open_points_bytes(bytes, name).unwrap();
        let whole = r.read(&options).unwrap();
        let mut parts = PointCloud::new(Vec::new(), SpatialRef::default());
        let mut chunks = 0;
        for chunk in r.chunks(&options, 20_000).unwrap() {
            let chunk = chunk.unwrap();
            assert!(chunk.len() <= 20_000 + 65_536, "{name}: a chunk of {}", chunk.len());
            parts.append(&chunk).unwrap();
            chunks += 1;
        }
        assert!(chunks > 1, "{name}: {chunks} chunks");
        assert_same_points(&whole, &parts, name);
    }
}

#[test]
fn writes_in_chunks_what_is_written_whole() {
    let c = cloud(120_000, 4);
    let dir = tempfile::tempdir().unwrap();
    for ext in ["las", "laz", "ply"] {
        let whole = dir.path().join(format!("whole.{ext}"));
        let parts = dir.path().join(format!("parts.{ext}"));
        pointcloud::write_points(&c, whole.to_str().unwrap(), &Default::default()).unwrap();
        let mut w = PointWriter::create(parts.to_str().unwrap(), &c, &Default::default()).unwrap();
        for k in 0..6 {
            w.write(&c.slice(k * 20_000, (k + 1) * 20_000)).unwrap();
        }
        w.finish().unwrap();
        let a = pointcloud::read_points(whole.to_str().unwrap(), &Default::default()).unwrap();
        let b = pointcloud::read_points(parts.to_str().unwrap(), &Default::default()).unwrap();
        assert_same_points(&a, &b, ext);
        assert_eq!((a.header.min, a.header.max, a.header.point_count), (b.header.min, b.header.max, b.header.point_count), "{ext}");
    }
}

/// The points (in hundredths) of each node (level, x, y, z) of a COPC file.
type NodePoints = std::collections::BTreeMap<(i32, i32, i32, i32), Vec<(i64, i64, i64)>>;

/// The points of each node of a COPC file, sorted.
fn copc_nodes(path: &str) -> NodePoints {
    let bytes = std::fs::read(path).unwrap();
    let mut r = pointcloud::open_points_bytes(bytes, "a.copc.laz").unwrap();
    let nodes = r.copc_nodes().unwrap();
    let cloud = r.read(&Default::default()).unwrap();
    // A read returns the nodes' points in file order.
    let mut by_offset = nodes.clone();
    by_offset.sort_by_key(|n| n.offset);
    let (x, y, z) = (cloud.coords("x").unwrap(), cloud.coords("y").unwrap(), cloud.coords("z").unwrap());
    let mut out = std::collections::BTreeMap::new();
    let mut at = 0;
    for n in by_offset.iter().filter(|n| n.point_count > 0) {
        let mut pts: Vec<(i64, i64, i64)> = (at..at + n.point_count as usize)
            .map(|i| ((x[i] * 100.0).round() as i64, (y[i] * 100.0).round() as i64, (z[i] * 100.0).round() as i64))
            .collect();
        pts.sort();
        out.insert((n.key.level, n.key.x, n.key.y, n.key.z), pts);
        at += n.point_count as usize;
    }
    out
}

#[test]
fn copc_built_out_of_core_has_the_octree_built_in_memory() {
    let c = cloud(200_000, 5);
    let dir = tempfile::tempdir().unwrap();
    let whole = dir.path().join("whole.copc.laz");
    let parts = dir.path().join("parts.copc.laz");
    let options = PointWriteOptions { copc_leaf_points: Some(3000), copc_memory_points: Some(25_000), ..Default::default() };
    pointcloud::write_points(&c, whole.to_str().unwrap(), &options).unwrap();
    let mut w = PointWriter::create(parts.to_str().unwrap(), &c, &options).unwrap();
    for k in 0..10 {
        w.write(&c.slice(k * 20_000, (k + 1) * 20_000)).unwrap();
    }
    w.finish().unwrap();
    let a = copc_nodes(whole.to_str().unwrap());
    let b = copc_nodes(parts.to_str().unwrap());
    assert!(a.len() > 20, "{} nodes", a.len());
    assert_eq!(a, b, "the same points in the same nodes");
}

#[test]
fn ranges_select_as_pdal_filters_range() {
    let c = cloud(10_000, 6);
    let r = DimRange::parse_list("Classification[2:2], Classification[5:5], Z(102:108]").unwrap();
    let kept = filters::range(&c, &r).unwrap();
    let class = c.dim("classification").unwrap().to_f64();
    let z = c.coords("z").unwrap();
    let expected = (0..c.len()).filter(|&i| (class[i] == 2.0 || class[i] == 5.0) && z[i] > 102.0 && z[i] <= 108.0).count();
    assert_eq!(kept.len(), expected);
    let negated = filters::range(&c, &DimRange::parse_list("Classification![2:2]").unwrap()).unwrap();
    assert_eq!(negated.len(), class.iter().filter(|c| **c != 2.0).count());
    assert!(filters::range(&c, &DimRange::parse_list("Nope[0:1]").unwrap()).is_err());
}

#[test]
fn thins_by_voxel_and_sample() {
    // Points on a 0.5 grid, four per voxel of 1.
    let mut x = Vec::new();
    let mut y = Vec::new();
    for i in 0..20 {
        for j in 0..20 {
            x.push(i as f64 * 0.5);
            y.push(j as f64 * 0.5);
        }
    }
    let n = x.len();
    let c = PointCloud::new(
        vec![Dimension::new("x", Values::F64(x)), Dimension::new("y", Values::F64(y)), Dimension::new("z", Values::F64(vec![0.0; n]))],
        SpatialRef::default(),
    );
    assert_eq!(filters::voxel_center_nearest_neighbor(&c, 1.0).unwrap().len(), 100);
    // The first point's voxel is centred on it: voxels from -0.5.
    let first = filters::voxel_downsize(&c, 1.0, VoxelMode::First).unwrap();
    assert_eq!(first.len(), 121);
    let center = filters::voxel_downsize(&c, 1.0, VoxelMode::Center).unwrap();
    assert_eq!(center.coords("x").unwrap()[1], 0.0);
    assert_eq!(center.coords("y").unwrap()[1], 1.0);
    // Poisson sampling: no two points kept closer than the radius.
    let s = filters::sample(&c, &SampleOptions { radius: Some(0.9), ..Default::default() }).unwrap();
    let (sx, sy) = (s.coords("x").unwrap(), s.coords("y").unwrap());
    for i in 0..s.len() {
        for j in 0..i {
            assert!((sx[i] - sx[j]).hypot(sy[i] - sy[j]) >= 0.9);
        }
    }
    assert_eq!(s.len(), 100);
}

#[test]
fn classifies_outliers_and_ground() {
    // A flat ground with a box on it, and a point far below.
    let mut x = Vec::new();
    let mut y = Vec::new();
    let mut z = Vec::new();
    for i in 0..80 {
        for j in 0..80 {
            let (px, py) = (i as f64 * 0.5, j as f64 * 0.5);
            x.push(px);
            y.push(py);
            z.push(if (15.0..25.0).contains(&px) && (15.0..25.0).contains(&py) { 10.0 } else { 0.0 });
        }
    }
    x.push(20.0);
    y.push(5.0);
    z.push(-30.0);
    let n = x.len();
    let c = PointCloud::new(
        vec![
            Dimension::new("x", Values::F64(x)),
            Dimension::new("y", Values::F64(y)),
            Dimension::new("z", Values::F64(z.clone())),
            Dimension::new("return_number", Values::U8(vec![1; n])),
            Dimension::new("number_of_returns", Values::U8(vec![1; n])),
        ],
        SpatialRef::default(),
    );
    let o = filters::outlier(&c, &OutlierOptions::default()).unwrap();
    let class = o.dim("classification").unwrap().to_f64();
    assert_eq!(class[n - 1], 7.0, "the low point is noise");
    let radius = filters::outlier_mask(&c, &OutlierOptions { method: OutlierMethod::Radius, radius: 2.0, ..Default::default() }).unwrap();
    assert!(radius[n - 1] && !radius[0]);
    let g = filters::smrf(&c.select(&(0..n).map(|i| i < n - 1).collect::<Vec<_>>()), &SmrfOptions::default()).unwrap();
    let class = g.dim("classification").unwrap().to_f64();
    for i in 0..n - 1 {
        assert_eq!(class[i], if z[i] > 5.0 { 1.0 } else { 2.0 }, "point {i} at z {}", z[i]);
    }
}

#[test]
fn grids_a_plane_linearly_and_the_nearest_values() {
    let r = noise(600, 7);
    let x: Vec<f64> = r[..200].iter().map(|v| v * 50.0).collect();
    let y: Vec<f64> = r[200..400].iter().map(|v| v * 50.0).collect();
    let z: Vec<f64> = x.iter().zip(&y).map(|(a, b)| 3.0 + 0.5 * a - 0.25 * b).collect();
    let c = PointCloud::new(
        vec![Dimension::new("x", Values::F64(x.clone())), Dimension::new("y", Values::F64(y.clone())), Dimension::new("z", Values::F64(z))],
        SpatialRef::default(),
    );
    let o = GridOptions {
        resolution: 2.0,
        method: GridMethod::Linear,
        radius: Some(0.0),
        bounds: Some([0.0, 0.0, 49.0, 49.0]),
        ..Default::default()
    };
    let g = grid(&c, &o).unwrap();
    let rr = &g.raster;
    let band = rr.band_view(0);
    let mut inside = 0;
    for row in 0..rr.rows {
        for col in 0..rr.cols {
            let v = band.as_slice()[row * rr.cols + col];
            if v == -9999.0 {
                continue;
            }
            let (cx, cy) = (1.0 + col as f64 * 2.0, 1.0 + (rr.rows - 1 - row) as f64 * 2.0);
            assert!((v - (3.0 + 0.5 * cx - 0.25 * cy)).abs() < 1e-9, "linear interpolation of a plane is exact");
            inside += 1;
        }
    }
    assert!(inside > 400, "{inside} cells inside the triangulation");
    // Nearest: each cell takes the value of the nearest point.
    let g = grid(&c, &GridOptions { method: GridMethod::Nearest, ..o }).unwrap();
    let band = g.raster.band_view(0);
    let (cx, cy) = (1.0 + 10.0 * 2.0, 1.0 + (g.raster.rows - 1 - 5) as f64 * 2.0);
    let nearest = (0..x.len()).min_by(|&a, &b| (x[a] - cx).hypot(y[a] - cy).total_cmp(&(x[b] - cx).hypot(y[b] - cy))).unwrap();
    assert_eq!(band.as_slice()[5 * g.raster.cols + 10], 3.0 + 0.5 * x[nearest] - 0.25 * y[nearest]);
}

#[test]
fn keeps_waveforms_when_writing() {
    // Point format 4 with a wave packet descriptor (8 bits, 4 samples) and the packets in the
    // file.
    let n = 3;
    let mut c = PointCloud::new(
        vec![
            Dimension::new("x", Values::F64(vec![1.0, 2.0, 3.0])),
            Dimension::new("y", Values::F64(vec![1.0, 2.0, 3.0])),
            Dimension::new("z", Values::F64(vec![1.0, 2.0, 3.0])),
            Dimension::new("gps_time", Values::F64(vec![1.0, 2.0, 3.0])),
            Dimension::new("wavepacket_index", Values::U8(vec![1; n])),
            Dimension::new("wavepacket_offset", Values::U64(vec![60, 64, 68])),
            Dimension::new("wavepacket_size", Values::U32(vec![4; n])),
        ],
        SpatialRef::default(),
    );
    c.header.point_format = 4;
    c.header.version = (1, 3);
    let mut descriptor = vec![8u8, 0];
    descriptor.extend(4u32.to_le_bytes());
    descriptor.extend(1000u32.to_le_bytes());
    descriptor.extend(0.5f64.to_le_bytes());
    descriptor.extend(1.0f64.to_le_bytes());
    c.header.vlrs.push(Vlr::new("LASF_Spec", 100, "Waveform Packet Descriptor", descriptor));
    c.header.evlrs.push(Vlr::new("LASF_Spec", 65535, "Waveform Data Packets", (0u8..12).collect()));
    for ext in ["las", "laz"] {
        let bytes = pointcloud::write_points_bytes(&c, &format!("w.{ext}"), &Default::default()).unwrap();
        let mut r = pointcloud::open_points_bytes(bytes, "w").unwrap();
        assert_eq!(r.header().version, (1, 3));
        assert_eq!(r.header().global_encoding & 2, 2, "{ext}: waveforms in the file");
        assert_eq!(r.header().waveform_data_size, 12);
        let d = r.wave_packet_descriptors();
        assert_eq!((d[0].index, d[0].bits_per_sample, d[0].number_of_samples, d[0].temporal_sample_spacing), (1, 8, 4, 1000));
        let w = r.waveform(1, 64, 4).unwrap();
        assert_eq!(w.samples, vec![4, 5, 6, 7]);
        assert_eq!(w.volts(), vec![3.0, 3.5, 4.0, 4.5]);
        // Read and written again, the points keep their packets.
        let back = r.read(&Default::default()).unwrap();
        let again = pointcloud::write_points_bytes(&back, &format!("w2.{ext}"), &Default::default()).unwrap();
        let mut r2 = pointcloud::open_points_bytes(again, "w2").unwrap();
        assert_eq!(r2.waveform(1, 68, 4).unwrap().samples, vec![8, 9, 10, 11]);
        let record = r2.waveform_record().unwrap();
        assert_eq!(&record[60..], &(0u8..12).collect::<Vec<_>>()[..]);
    }
}

#[test]
fn reads_ply_e57_and_ept_as_pdal_does() {
    // PDAL 2.9's readers.ply, readers.e57 and readers.ept.
    let Some(p) = data("ply/text_extradim.ply") else { return };
    let c = pointcloud::read_points(&p, &Default::default()).unwrap();
    assert_eq!(c.dimension_names(), ["x", "y", "z", "normal_x", "normal_y", "normal_z", "red", "green", "blue", "alpha", "omg"]);
    assert_eq!(c.coords("x").unwrap()[0], -2.64944);
    assert_eq!(c.dim("normal_x").unwrap().values.get_f64(0), -0.02375520020723343);
    assert_eq!(c.dim("blue").unwrap().values, Values::I8(vec![64]));
    assert_eq!(c.dim("omg").unwrap().values, Values::I32(vec![1234]));
    let sums = |c: &PointCloud, name: &str| c.dim(name).unwrap().to_f64().iter().sum::<f64>();
    let c = pointcloud::read_points(&data("e57/bunnyDouble.e57").unwrap(), &Default::default()).unwrap();
    assert_eq!(c.len(), 30571);
    assert!((sums(&c, "x") - -841.093298).abs() < 1e-9 && (sums(&c, "y") - 3151.1987419999996).abs() < 1e-9);
    let c = pointcloud::read_points(&data("e57/A_B.e57").unwrap(), &Default::default()).unwrap();
    // Two scans, posed; colours of 0 to 255 scaled to 0 to 65535.
    assert_eq!(c.len(), 6);
    assert!((sums(&c, "x") - -263.4897003173828).abs() < 1e-9);
    assert_eq!((sums(&c, "red"), sums(&c, "green"), sums(&c, "intensity")), (131070.0, 393210.0, 131968.0));
    let c = pointcloud::read_points(&data("e57/tiny_spherical.e57").unwrap(), &Default::default()).unwrap();
    assert!((sums(&c, "z") - 229.2969676192078).abs() < 1e-9, "spherical coordinates to x, y, z");
    let c = pointcloud::read_points(&data("e57/scaled_integer_intensity.e57").unwrap(), &Default::default()).unwrap();
    assert_eq!(c.dim("intensity").unwrap().values, Values::U16(vec![0, 32768]));
    let mut r = pointcloud::open_points(&data("ept/lone-star-laszip/ept.json").unwrap()).unwrap();
    assert_eq!((r.format_name(), r.header().point_count, r.crs().epsg), ("EPT", 518_862, Some(26912)));
    let c = r.read(&Default::default()).unwrap();
    assert_eq!(c.len(), 518_862);
    assert!((sums(&c, "z") - 1208260675.9405).abs() < 1e-3);
    assert_eq!((sums(&c, "intensity"), sums(&c, "origin_id")), (562919835.0, 746092.0));
    // Binary and Zstandard tiles: the same points. PDAL 2.9 rounds binary tiles' coordinates
    // to integers; terrio keeps them.
    let b = pointcloud::read_points(&data("ept/ellipsoid-binary/ept.json").unwrap(), &Default::default()).unwrap();
    let z = pointcloud::read_points(&data("ept/ellipsoid-zstandard/ept.json").unwrap(), &Default::default()).unwrap();
    assert_eq!(b.len(), 100_000);
    assert_same_points(&b, &z, "binary and Zstandard EPT");
    assert!(b.dim("withheld").is_none(), "binary tiles have the flags only with a ClassFlags dimension");
}

#[test]
fn reads_ept_by_bounds_and_resolution_as_pdal_does() {
    // PDAL 2.9's readers.ept with bounds and resolution.
    let Some(p) = data("ept/lone-star-laszip/ept.json") else { return };
    let mut r = pointcloud::open_points(&p).unwrap();
    let bounds = [515378.2, 4918356.8, 515388.4, 4918373.6];
    let read = |r: &mut pointcloud::PointCloudReader, b: Option<[f64; 4]>, res: Option<f64>| {
        let o = PointReadOptions { filter: PointFilter { bounds: b, ..Default::default() }, resolution: res, ..Default::default() };
        r.read(&o).unwrap().len()
    };
    assert_eq!(read(&mut r, Some(bounds), None), 82_402);
    assert_eq!(read(&mut r, None, Some(1.0)), 41_998);
    assert_eq!(read(&mut r, None, Some(0.1)), 479_269);
    assert_eq!(read(&mut r, Some(bounds), Some(0.3)), 48_781);
}

#[test]
fn reads_and_keeps_full_waveforms() {
    // An airborne survey with full waveforms (LAS 1.3, point format 4, packets in the file);
    // the samples are the file's bytes at the offsets LASlib reads them from.
    let Some(p) = data("waveform/full_waveform_tutorial_data.las") else { return };
    let mut r = pointcloud::open_points(&p).unwrap();
    assert_eq!((r.header().point_format, r.header().waveform_data_size), (4, 95_749_632));
    let d = r.wave_packet_descriptors();
    assert_eq!((d.len(), d[0].bits_per_sample, d[0].number_of_samples), (1, 8, 256));
    for (offset, sum, first) in [(60, 3782, [12, 11, 12, 12, 12]), (316, 3951, [12, 11, 11, 12, 12]), (124732, 3914, [12, 12, 11, 11, 11])]
    {
        let w = r.waveform(1, offset, 256).unwrap();
        assert_eq!(w.samples.iter().sum::<u32>(), sum);
        assert_eq!(&w.samples[..5], &first);
    }
    let c = r.read(&PointReadOptions { max_points: Some(100_000), ..Default::default() }).unwrap();
    let dir = tempfile::tempdir().unwrap();
    for ext in ["las", "laz"] {
        let out = dir.path().join(format!("w.{ext}"));
        pointcloud::write_points(&c, out.to_str().unwrap(), &Default::default()).unwrap();
        let mut back = pointcloud::open_points(out.to_str().unwrap()).unwrap();
        assert_eq!(back.header().waveform_data_size, 95_749_632, "{ext}");
        let offsets = back.read(&Default::default()).unwrap();
        assert_eq!(offsets.dim("wavepacket_offset").unwrap().values, c.dim("wavepacket_offset").unwrap().values);
        assert_eq!(back.waveform(1, 124732, 256).unwrap().samples.iter().sum::<u32>(), 3914, "{ext}");
    }
}

/// autzen_trim.las, read once.
fn autzen() -> Option<PointCloud> {
    Some(pointcloud::read_points(&data("autzen_trim.las")?, &Default::default()).unwrap())
}

#[test]
fn filters_as_pdal_does() {
    // PDAL 2.9's filters on autzen_trim.las (110 000 points, in feet): the same points or the
    // same classes.
    let Some(c) = autzen() else { return };
    let ground = |c: &PointCloud| c.dim("classification").unwrap().to_f64().iter().filter(|v| **v == 2.0).count();
    assert_eq!(ground(&filters::smrf(&c, &SmrfOptions::default()).unwrap()), 86_660);
    let o = SmrfOptions { slope: 0.2, window: Some(16.0), threshold: 0.45, scalar: 1.2, ..Default::default() };
    assert_eq!(ground(&filters::smrf(&c, &o).unwrap()), 87_029);
    let outliers = |o: OutlierOptions| filters::outlier_mask(&c, &o).unwrap().iter().filter(|v| **v).count();
    assert_eq!(outliers(OutlierOptions::default()), 4133);
    assert_eq!(outliers(OutlierOptions { mean_k: 12, multiplier: 2.5, ..Default::default() }), 2665);
    assert_eq!(outliers(OutlierOptions { method: OutlierMethod::Radius, radius: 2.0, min_k: 4, ..Default::default() }), 95_514);
    assert_eq!(filters::voxel_center_nearest_neighbor(&c, 2.0).unwrap().len(), 89_358);
    assert_eq!(filters::voxel_downsize(&c, 1.0, VoxelMode::First).unwrap().len(), 108_895);
    assert_eq!(filters::sample(&c, &SampleOptions { radius: Some(1.0), ..Default::default() }).unwrap().len(), 103_846);
    assert_eq!(filters::sample(&c, &SampleOptions { cell: Some(1.5), ..Default::default() }).unwrap().len(), 84_741);
    let r = DimRange::parse_list("Classification[2:2],Z[420:500),Intensity![0:10]").unwrap();
    assert_eq!(filters::range(&c, &r).unwrap().len(), 20_849);
}

#[test]
fn grids_like_gdal_grid() {
    // gdal_grid (GDAL 3.12) -a linear and -a nearest on autzen_trim.las, the same grid.
    let Some(c) = autzen() else { return };
    let b = c.bounds().unwrap();
    let run = |method, res, radius| {
        let g = grid(&c, &GridOptions { resolution: res, method, radius, ..Default::default() }).unwrap();
        let v = g.raster.band_view(0).as_slice().to_vec();
        (v, g.raster.cols)
    };
    assert_eq!((b[0], b[1]), (636001.76, 848935.2000000001));
    // Linear: 16 of the 18 518 cells differ, where several points share an x and y (qhull and
    // terrio's triangulation keep different ones).
    let (v, cols) = run(GridMethod::Linear, 6.0, Some(0.0));
    assert_eq!(v.iter().filter(|x| **x == -9999.0).count(), 2995);
    for (row, col, expected) in [(10, 10, 406.7671718206382), (40, 100, 424.9247460737469), (80, 150, 427.5330505617978)] {
        assert!((v[row * cols + col] - expected).abs() < 1e-9, "linear ({row}, {col})");
    }
    let total: f64 = v.iter().filter(|x| **x != -9999.0).sum();
    assert!((total - 6545684.477735).abs() < 200.0, "{total}");
    // Nearest: identical.
    let (v, cols) = run(GridMethod::Nearest, 10.0, None);
    assert!((v.iter().sum::<f64>() - 2833138.81).abs() < 1e-6);
    assert_eq!((v[10 * cols + 10], v[40 * cols + 100], v[50 * cols + 110]), (407.74, 426.94, 431.92));
    let (v, _) = run(GridMethod::Nearest, 6.0, Some(8.0));
    assert_eq!(v.iter().filter(|x| **x == -9999.0).count(), 4437);
}

#[test]
fn converts_to_ply_and_e57_and_streams_grids() {
    let Some(p) = data("autzen_trim.laz") else { return };
    let dir = tempfile::tempdir().unwrap();
    let c = pointcloud::read_points(&p, &Default::default()).unwrap();
    for ext in ["ply", "e57"] {
        let out = dir.path().join(format!("a.{ext}"));
        pointcloud::convert(&p, out.to_str().unwrap(), &Default::default()).unwrap();
        let back = pointcloud::read_points(out.to_str().unwrap(), &Default::default()).unwrap();
        assert_eq!(back.len(), c.len());
        assert_eq!(back.coords("x").unwrap(), c.coords("x").unwrap(), "{ext}");
        assert!(!PointFormat::for_path(out.to_str().unwrap()).unwrap().is_las());
    }
    // Statistics gridded in chunks are those gridded in memory.
    let o = GridOptions { resolution: 2.0, stats: terrio::pointcloud::grid::GridStat::all(), ..Default::default() };
    let mut r = pointcloud::open_points(&p).unwrap();
    let streamed = terrio::pointcloud::grid::grid_reader(&mut r, &Default::default(), &o, None).unwrap();
    let whole = grid(&c, &o).unwrap();
    assert_eq!(streamed.raster.band_view(2).as_slice(), whole.raster.band_view(2).as_slice());
}

#[test]
#[ignore = "reads public cloud data"]
fn reads_remote_ept_in_place() {
    // USGS 3DEP lidar of Columbia County, Wisconsin: 11.5 billion points. PDAL's readers.ept
    // reads the same 347 376 points.
    let url = "https://s3-us-west-2.amazonaws.com/usgs-lidar-public/WI_8County_Columbia_2020/ept.json";
    let mut r = pointcloud::open_points(url).unwrap();
    assert_eq!(r.header().point_count, 11_532_376_686);
    let options = PointReadOptions {
        filter: PointFilter { bounds: Some([-9951440.5, 5382600.0, -9951140.5, 5382900.0]), ..Default::default() },
        resolution: Some(2.0),
        ..Default::default()
    };
    assert_eq!(r.read(&options).unwrap().len(), 347_376);
}

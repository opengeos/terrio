//! Point clouds: EPT datasets and E57 files written in chunks; PDAL's ground (PMF, CSF, ELM), height
//! above ground, selection (crop, decimation, expression, assign, merge), tiling (splitter,
//! chipper) and feature (normal, cluster) filters.
//!
//! The synthetic tests run everywhere. The real-data tests read `autzen_trim.las` of
//! `scripts/fetch_test_data.sh` from `TERRIO_TEST_DATA` (they pass without checking anything when
//! it is not set); their expected values come from PDAL 2.9, and
//! `scripts/compare_pointclouds.py` compares every point with PDAL's.

use std::path::PathBuf;
use terrio::crs::SpatialRef;
use terrio::pointcloud::filters::{
    self, AssignOptions, ClusterOptions, CropBox, CropOptions, CsfOptions, DimRange, HagOptions, NormalOptions, PmfOptions, SmrfOptions,
    SplitterOptions,
};
use terrio::pointcloud::{self, Dimension, EptDataType, PointCloud, PointWriteOptions, PointWriter, Values};

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

/// The points sorted by gps_time (unique in the synthetic clouds), as rows of every dimension.
fn sorted_rows(c: &PointCloud, names: &[&str]) -> Vec<Vec<f64>> {
    let cols: Vec<Vec<f64>> = names.iter().map(|n| c.dim(n).unwrap_or_else(|| panic!("no {n}")).to_f64()).collect();
    let mut rows: Vec<Vec<f64>> = (0..c.len()).map(|i| cols.iter().map(|col| col[i]).collect()).collect();
    let t = names.iter().position(|n| *n == "gps_time").unwrap();
    rows.sort_by(|a, b| a[t].total_cmp(&b[t]));
    rows
}

#[test]
fn ept_round_trips_in_every_data_type() {
    let c = cloud(60_000, 11);
    let names = ["x", "y", "z", "intensity", "return_number", "number_of_returns", "classification", "gps_time"];
    let dir = tempfile::tempdir().unwrap();
    for (data_type, memory) in [(EptDataType::Laszip, None), (EptDataType::Binary, Some(7_000)), (EptDataType::Zstandard, None)] {
        let path = dir.path().join(format!("{data_type:?}")).join("ept.json");
        let options =
            PointWriteOptions { ept_data_type: data_type, copc_leaf_points: Some(5_000), copc_memory_points: memory, ..Default::default() };
        pointcloud::write_points(&c, path.to_str().unwrap(), &options).unwrap();
        let mut r = pointcloud::open_points(path.to_str().unwrap()).unwrap();
        assert_eq!(r.format_name(), "EPT");
        let info = r.ept_info().unwrap().clone();
        assert_eq!(info.points, 60_000, "{data_type:?}");
        assert_eq!(info.span, 128);
        assert_eq!(info.schema[0].name, "X");
        let back = r.read(&Default::default()).unwrap();
        // The points as a LAS file holds them (scaled integers).
        let las = pointcloud::open_points_bytes(pointcloud::write_points_bytes(&c, "a.las", &Default::default()).unwrap(), "a.las")
            .unwrap()
            .read(&Default::default())
            .unwrap();
        assert_eq!(sorted_rows(&back, &names), sorted_rows(&las, &names), "{data_type:?}");
        // An octree: nodes below the root, and the root's resolution reads a sample.
        let hierarchy: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.with_file_name("ept-hierarchy").join("0-0-0-0.json")).unwrap()).unwrap();
        assert!(hierarchy.as_object().unwrap().len() > 8, "{data_type:?}: {hierarchy}");
        let root = r.read(&pointcloud::PointReadOptions { max_level: Some(0), ..Default::default() }).unwrap();
        assert!(root.len() < c.len() && !root.is_empty(), "{data_type:?}: {} root points", root.len());
    }
}

#[test]
fn ept_written_in_chunks_out_of_core_is_the_same() {
    let c = cloud(40_000, 12);
    let dir = tempfile::tempdir().unwrap();
    let whole = dir.path().join("whole/ept.json");
    let parts = dir.path().join("parts/ept.json");
    let options = PointWriteOptions { copc_leaf_points: Some(3_000), ..Default::default() };
    pointcloud::write_points(&c, whole.to_str().unwrap(), &options).unwrap();
    let options = PointWriteOptions { copc_memory_points: Some(6_000), ..options };
    let mut w = PointWriter::create(&format!("ept://{}", parts.parent().unwrap().display()), &c, &options).unwrap();
    for k in 0..8 {
        w.write(&c.slice(k * 5_000, (k + 1) * 5_000)).unwrap();
    }
    w.finish().unwrap();
    let read = |p: &PathBuf| std::fs::read_to_string(p.with_file_name("ept-hierarchy").join("0-0-0-0.json")).unwrap();
    assert_eq!(read(&whole), read(&parts), "the same octree");
    let names = ["x", "y", "z", "gps_time"];
    let a = pointcloud::read_points(whole.to_str().unwrap(), &Default::default()).unwrap();
    let b = pointcloud::read_points(parts.to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(sorted_rows(&a, &names), sorted_rows(&b, &names));
}

#[test]
fn e57_written_in_chunks_is_written_whole() {
    let c = cloud(50_000, 13);
    let dir = tempfile::tempdir().unwrap();
    let whole = dir.path().join("whole.e57");
    let parts = dir.path().join("parts.e57");
    pointcloud::write_points(&c, whole.to_str().unwrap(), &Default::default()).unwrap();
    let mut w = PointWriter::create(parts.to_str().unwrap(), &c, &Default::default()).unwrap();
    for k in 0..5 {
        w.write(&c.slice(k * 10_000, (k + 1) * 10_000)).unwrap();
    }
    w.finish().unwrap();
    let a = pointcloud::read_points(whole.to_str().unwrap(), &Default::default()).unwrap();
    let b = pointcloud::read_points(parts.to_str().unwrap(), &Default::default()).unwrap();
    assert_eq!(a.len(), 50_000);
    for name in ["x", "y", "z", "intensity", "classification", "gps_time"] {
        assert_eq!(a.dim(name).unwrap().to_f64(), b.dim(name).unwrap().to_f64(), "{name}");
    }
}

#[test]
fn e57_writer_dropped_unfinished_leaves_no_complete_file() {
    let c = cloud(20_000, 15);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("dropped.e57");
    let mut w = PointWriter::create(path.to_str().unwrap(), &c, &Default::default()).unwrap();
    w.write(&c.slice(0, 10_000)).unwrap();
    drop(w);
    // Not finished: no E57 file that reads as the first chunk alone.
    assert!(pointcloud::read_points(path.to_str().unwrap(), &Default::default()).is_err());
}

#[test]
fn ground_filters_keep_empty_clouds_empty() {
    let empty = cloud(10, 17).slice(0, 0);
    assert_eq!(filters::pmf(&empty, &PmfOptions::default()).unwrap().len(), 0);
    assert_eq!(filters::csf(&empty, &CsfOptions::default()).unwrap().len(), 0);
}

#[test]
fn assigns_scaled_extra_bytes_dimensions_in_their_units() {
    let mut c = cloud(10, 16);
    let mut hag = Dimension::new("height_above_ground", Values::U16(vec![0; 10]));
    hag.scale = Some(0.01);
    hag.offset = Some(-1.0);
    c.dimensions.push(hag);
    let a = filters::assign(&c, &AssignOptions { values: vec!["HeightAboveGround = 0.5".into()], ..Default::default() }).unwrap();
    let d = a.dim("height_above_ground").unwrap();
    // Stored as (0.5 - -1) / 0.01 = 150, read back as 0.5.
    assert_eq!(d.values.get_f64(0), 150.0);
    assert!((d.get(0) - 0.5).abs() < 1e-9);
    // Out of the stored type's range: (700 + 1) / 0.01 > 65535.
    assert!(filters::assign(&c, &AssignOptions { values: vec!["HeightAboveGround = 700".into()], ..Default::default() }).is_err());
}

#[test]
fn selects_points_as_pdal_filters() {
    let c = cloud(2_000, 14);
    // Decimation: the points at offset + round(k × step), as PDAL streams them.
    let d = filters::decimation(&c, 2.5, 3, Some(20)).unwrap();
    let t = c.dim("gps_time").unwrap().to_f64();
    assert_eq!(d.dim("gps_time").unwrap().to_f64(), [3, 6, 8, 11, 13, 16, 18].map(|i| t[i]).to_vec());
    // Expressions and assignments.
    let e = filters::expression(&c, &["Classification == 2 && Intensity < 500".into()]).unwrap();
    assert!(e.len() > 100 && e.dim("classification").unwrap().to_f64().iter().all(|v| *v == 2.0));
    let a = filters::assign(
        &c,
        &AssignOptions {
            assignments: vec!["Classification[2:3]=9".into()],
            condition: Some(DimRange::parse("Intensity[0:499]").unwrap()),
            values: vec!["UserScore = Z * 2 WHERE Classification == 9".into(), "Intensity = Intensity + 0.5".into()],
        },
    )
    .unwrap();
    let (cls, ints, z, score) = (
        a.dim("classification").unwrap().to_f64(),
        a.dim("intensity").unwrap().to_f64(),
        a.dim("z").unwrap().to_f64(),
        a.dim("user_score").unwrap().to_f64(),
    );
    for i in 0..c.len() {
        let low = i % 1000 < 500;
        let was = (i % 7) as f64;
        assert_eq!(cls[i], if low && (was == 2.0 || was == 3.0) { 9.0 } else { was });
        // Rounded half away from zero, as PDAL converts doubles to integers.
        assert_eq!(ints[i], (i % 1000) as f64 + if low { 1.0 } else { 0.0 });
        assert_eq!(score[i], if cls[i] == 9.0 { z[i] * 2.0 } else { 0.0 });
    }
    // Crops: a box, a polygon with a hole and a circle, in turn.
    let o = CropOptions {
        bounds: vec![CropBox::parse("([500010, 500020], [4000010, 4000020])").unwrap()],
        polygons: vec!["POLYGON((500000 4000000, 500050 4000000, 500050 4000050, 500000 4000050, 500000 4000000), (500010 4000010, 500020 4000010, 500020 4000020, 500010 4000020, 500010 4000010))".into()],
        points: vec![vec![500070.0, 4000070.0]],
        distance: 10.0,
        ..Default::default()
    };
    let k = filters::crop_indices(&c, &o).unwrap();
    let (xs, ys) = (c.dim("x").unwrap().to_f64(), c.dim("y").unwrap().to_f64());
    let in_box = |i: usize, x0: f64, y0: f64, x1: f64, y1: f64| xs[i] >= x0 && xs[i] <= x1 && ys[i] >= y0 && ys[i] <= y1;
    let poly: Vec<usize> = (0..c.len())
        .filter(|&i| {
            in_box(i, 500000.0, 4000000.0, 500050.0, 4000050.0)
                && !(in_box(i, 500010.0, 4000010.0, 500020.0, 4000020.0)
                    && xs[i] > 500010.0
                    && xs[i] < 500020.0
                    && ys[i] > 4000010.0
                    && ys[i] < 4000020.0)
        })
        .collect();
    let boxed: Vec<usize> = (0..c.len()).filter(|&i| in_box(i, 500010.0, 4000010.0, 500020.0, 4000020.0)).collect();
    let circle: Vec<usize> = (0..c.len()).filter(|&i| (xs[i] - 500070.0).powi(2) + (ys[i] - 4000070.0).powi(2) < 100.0).collect();
    assert_eq!(k, [poly, boxed, circle].concat());
    // Merges: the dimensions of both, zeros where one has none.
    let mut other = cloud(10, 15);
    other.dimensions.retain(|d| d.name != "intensity");
    let m = filters::merge(&[c.clone(), other]).unwrap();
    assert_eq!(m.len(), 2_010);
    assert_eq!(m.dim("intensity").unwrap().to_f64()[2_000..], [0.0; 10]);
}

#[test]
fn splits_into_tiles_as_pdal() {
    let c = cloud(3_000, 16);
    let tiles = filters::splitter(&c, &SplitterOptions { length: 25.0, origin: Some([500_000.0, 4_000_000.0]), buffer: 0.0 }).unwrap();
    let (xs, ys) = (c.dim("x").unwrap().to_f64(), c.dim("y").unwrap().to_f64());
    let mut keys: Vec<(i32, i32)> = Vec::new();
    for i in 0..c.len() {
        let k = (((xs[i] - 500_000.0) / 25.0).floor() as i32, ((ys[i] - 4_000_000.0) / 25.0).floor() as i32);
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    // The squares in the order of their first points.
    assert_eq!(tiles.iter().map(|(k, _)| *k).collect::<Vec<_>>(), keys);
    assert_eq!(tiles.iter().map(|(_, t)| t.len()).sum::<usize>(), 3_000);
    let buffered = filters::splitter(&c, &SplitterOptions { length: 25.0, origin: Some([500_000.0, 4_000_000.0]), buffer: 2.0 }).unwrap();
    assert!(buffered.iter().map(|(_, t)| t.len()).sum::<usize>() > 3_000);
    let chips = filters::chipper(&c, 400).unwrap();
    assert_eq!(chips.len(), 8);
    assert_eq!(chips.iter().map(PointCloud::len).collect::<Vec<_>>(), [375; 8]);
}

#[test]
fn computes_normals_heights_and_clusters() {
    // A tilted plane, z = 0.1 x + 0.2 y: the normals are (-0.1, -0.2, 1), normalized, and the
    // curvature 0.
    let n = 400;
    let xs: Vec<f64> = (0..n).map(|i| (i % 20) as f64).collect();
    let ys: Vec<f64> = (0..n).map(|i| (i / 20) as f64).collect();
    let zs: Vec<f64> = (0..n).map(|i| 0.1 * xs[i] + 0.2 * ys[i]).collect();
    let mut classes = vec![2u8; n];
    let mut z2 = zs.clone();
    for i in (0..n).step_by(5) {
        classes[i] = 1;
        z2[i] += 3.0;
    }
    let c = pointcloud::from_dimensions(
        vec![
            Dimension::new("x", Values::F64(xs.clone())),
            Dimension::new("y", Values::F64(ys.clone())),
            Dimension::new("z", Values::F64(zs)),
            Dimension::new("classification", Values::U8(classes)),
        ],
        SpatialRef::default(),
    );
    let nrm = filters::normal(&c, &NormalOptions::default()).unwrap();
    let len = (0.01f64 + 0.04 + 1.0).sqrt();
    for (name, v) in [("normal_x", -0.1 / len), ("normal_y", -0.2 / len), ("normal_z", 1.0 / len)] {
        assert!(nrm.dim(name).unwrap().to_f64().iter().all(|x| (x - v).abs() < 1e-5), "{name}");
    }
    assert!(nrm.dim("curvature").unwrap().to_f64().iter().all(|x| x.abs() < 1e-6));
    // Heights above the plane's ground points.
    let mut lifted = c.clone();
    lifted.dim_mut("z").unwrap().values = Values::F64(z2);
    for hag in [
        filters::hag_nn(&lifted, &HagOptions { count: 4, ..HagOptions::nn() }).unwrap(),
        filters::hag_delaunay(&lifted, &HagOptions::delaunay()).unwrap(),
    ] {
        let h = hag.dim("height_above_ground").unwrap().to_f64();
        for (i, h) in h.iter().enumerate() {
            let interior = (1..19).contains(&(i % 20)) && (1..19).contains(&(i / 20));
            if i % 5 == 0 && interior {
                assert!((h - 3.0).abs() < 0.11, "point {i}: {h}");
            } else if i % 5 != 0 {
                assert_eq!(*h, 0.0);
            }
        }
    }
    // Clusters: two groups of points 10 apart.
    let pts: Vec<f64> = (0..20).map(|i| if i < 10 { i as f64 * 0.5 } else { 100.0 + i as f64 * 0.5 }).collect();
    let g = pointcloud::from_dimensions(
        vec![
            Dimension::new("x", Values::F64(pts.clone())),
            Dimension::new("y", Values::F64(vec![0.0; 20])),
            Dimension::new("z", Values::F64(vec![0.0; 20])),
        ],
        SpatialRef::default(),
    );
    let ids = filters::cluster(&g, &ClusterOptions::default()).unwrap().dim("cluster_id").unwrap().to_f64();
    assert_eq!(ids, [vec![1.0; 10], vec![2.0; 10]].concat());
    let ids = filters::cluster(&g, &ClusterOptions { tolerance: 0.5, ..Default::default() }).unwrap().dim("cluster_id").unwrap().to_f64();
    assert_eq!(ids, (1..=20).map(|v| v as f64).collect::<Vec<_>>(), "neighbours closer than the tolerance only");
}

#[test]
fn classifies_ground_by_cloth_simulation() {
    // Flat ground with a 10 m high building: the roof is not ground, the ground around it is.
    let (mut xs, mut ys, mut zs) = (Vec::new(), Vec::new(), Vec::new());
    for i in 0..60 {
        for j in 0..60 {
            let roof = (25..35).contains(&i) && (25..35).contains(&j);
            xs.push(i as f64 + 0.25);
            ys.push(j as f64 + 0.5);
            zs.push(if roof { 10.0 } else { 0.01 * ((i * 7 + j * 3) % 5) as f64 });
        }
    }
    let n = xs.len();
    let c = pointcloud::from_dimensions(
        vec![
            Dimension::new("x", Values::F64(xs.clone())),
            Dimension::new("y", Values::F64(ys.clone())),
            Dimension::new("z", Values::F64(zs)),
            Dimension::new("return_number", Values::U8(vec![1; n])),
            Dimension::new("number_of_returns", Values::U8(vec![1; n])),
        ],
        SpatialRef::default(),
    );
    let classes = filters::csf(&c, &CsfOptions::default()).unwrap().dim("classification").unwrap().to_f64();
    for k in 0..n {
        let (i, j) = (xs[k] as usize, ys[k] as usize);
        if (25..35).contains(&i) && (25..35).contains(&j) {
            assert_eq!(classes[k], 1.0, "roof point {i}, {j}");
        } else if !(20..40).contains(&i) || !(20..40).contains(&j) {
            assert_eq!(classes[k], 2.0, "ground point {i}, {j}");
        }
    }
}

fn autzen() -> Option<PointCloud> {
    Some(pointcloud::read_points(&data("autzen_trim.las")?, &Default::default()).unwrap())
}

#[test]
fn filters_as_pdal_does_on_autzen() {
    // PDAL 2.9's filters on autzen_trim.las (110 000 points, in feet).
    let Some(c) = autzen() else { return };
    let count = |c: &PointCloud, class: f64| c.dim("classification").unwrap().to_f64().iter().filter(|v| **v == class).count();
    assert_eq!(count(&filters::pmf(&c, &PmfOptions::default()).unwrap(), 2.0), 81_954);
    let o = PmfOptions { cell: 2.0, max_window: 20.0, slope: 0.5, initial_distance: 0.3, ..Default::default() };
    assert_eq!(count(&filters::pmf(&c, &o).unwrap(), 2.0), 76_047);
    let o = CsfOptions { resolution: 5.0, rigidness: 2, ..Default::default() };
    assert_eq!(count(&filters::csf(&c, &o).unwrap(), 2.0), 35_440);
    assert_eq!(count(&filters::elm(&c, 10.0, 1.0, 7).unwrap(), 7.0), 202);
    assert_eq!(count(&filters::elm(&c, 5.0, 0.5, 7).unwrap(), 7.0), 3_256);
    let ground = filters::smrf(&c, &SmrfOptions::default()).unwrap();
    let hag = filters::hag_nn(&ground, &HagOptions::nn()).unwrap().dim("height_above_ground").unwrap().to_f64();
    assert_eq!(hag.iter().copied().fold(f64::MIN, f64::max), 106.43);
    assert!((hag.iter().sum::<f64>() - 625_301.02).abs() < 1e-6);
    let hag = filters::hag_delaunay(&ground, &HagOptions::delaunay()).unwrap().dim("height_above_ground").unwrap().to_f64();
    assert!((hag.iter().sum::<f64>() - 625_690.167_010_587_2).abs() < 1e-6);
    let crop = |o: CropOptions| filters::crop(&c, &o).unwrap().len();
    assert_eq!(
        crop(CropOptions { bounds: vec![CropBox::parse("([636500, 636800], [849000, 849300])").unwrap()], ..Default::default() }),
        22_964
    );
    assert_eq!(
        crop(CropOptions {
            polygons: vec!["POLYGON((636200 849000, 637000 849100, 636600 849450, 636200 849000))".into()],
            ..Default::default()
        }),
        36_601
    );
    assert_eq!(crop(CropOptions { points: vec![vec![636600.0, 849200.0]], distance: 100.0, ..Default::default() }), 8_564);
    assert_eq!(filters::decimation(&c, 3.0, 0, None).unwrap().len(), 36_667);
    assert_eq!(filters::expression(&c, &["Classification == 2 && Z > 430 || ReturnNumber > 2".into()]).unwrap().len(), 3_993);
    assert_eq!(filters::splitter(&c, &SplitterOptions { length: 300.0, ..Default::default() }).unwrap().len(), 14);
    assert_eq!(filters::chipper(&c, 5000).unwrap().len(), 22);
    let nrm = filters::normal(&c, &NormalOptions::default()).unwrap();
    for (name, v) in [
        ("normal_x", 0.017_769_004_928_684_78),
        ("normal_y", 0.017_304_874_606_955_2),
        ("normal_z", 0.999_692_354_566_484_5),
        ("curvature", 0.000_590_883_264_191_16),
    ] {
        assert!((nrm.dim(name).unwrap().get(0) - v).abs() < 1e-9, "{name}");
    }
    let ids = filters::cluster(&c, &ClusterOptions::default()).unwrap().dim("cluster_id").unwrap().to_f64();
    assert_eq!(ids.iter().copied().fold(0.0, f64::max), 103_568.0);
}

/// Writing over an EPT dataset replaces it once the new one is complete: a writer dropped
/// unfinished leaves the previous dataset as it was, and no work directory stays behind.
#[test]
fn ept_datasets_are_replaced_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("set/ept.json");
    let path = path.to_str().unwrap();
    let first = cloud(20_000, 13);
    let options = PointWriteOptions { copc_leaf_points: Some(3_000), ..Default::default() };
    pointcloud::write_points(&first, path, &options).unwrap();
    let points = |p: &str| pointcloud::open_points(p).unwrap().ept_info().unwrap().points;
    assert_eq!(points(path), 20_000);
    // A writer of another cloud, dropped before finishing.
    let second = cloud(30_000, 14);
    let mut w = PointWriter::create(path, &second, &options).unwrap();
    w.write(&second.slice(0, 10_000)).unwrap();
    drop(w);
    assert_eq!(points(path), 20_000);
    assert_eq!(pointcloud::read_points(path, &Default::default()).unwrap().len(), 20_000);
    // Written whole: the second replaces the first.
    pointcloud::write_points(&second, path, &options).unwrap();
    assert_eq!(points(path), 30_000);
    assert_eq!(pointcloud::read_points(path, &Default::default()).unwrap().len(), 30_000);
    let left: Vec<String> =
        std::fs::read_dir(dir.path().join("set")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(!left.iter().any(|n| n.starts_with(".terrio")), "{left:?}");
}

/// A stream writer refused for its dimensions leaves an existing E57 file as it was.
#[test]
fn e57_writers_refused_leave_files_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.e57");
    let path = path.to_str().unwrap();
    let c = cloud(1_000, 15);
    pointcloud::write_points(&c, path, &Default::default()).unwrap();
    let before = std::fs::read(path).unwrap();
    let options = PointWriteOptions { dimensions: vec!["no_such_dimension".into()], ..Default::default() };
    assert!(PointWriter::create(path, &c, &options).is_err());
    assert_eq!(std::fs::read(path).unwrap(), before);
}

/// Every node of an EPT dataset's hierarchy (all its pages): its count, which must be its tile's
/// number of points, and its parent, which must be in the hierarchy.
fn check_hierarchy(dir: &std::path::Path) -> std::collections::BTreeMap<String, u64> {
    let ept: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("ept.json")).unwrap()).unwrap();
    let rl: u64 = ept["schema"].as_array().unwrap().iter().map(|d| d["size"].as_u64().unwrap()).sum();
    let kind = ept["dataType"].as_str().unwrap().to_string();
    let mut counts = std::collections::BTreeMap::new();
    let mut pages = vec!["0-0-0-0".to_string()];
    while let Some(p) = pages.pop() {
        let page: serde_json::Map<String, serde_json::Value> =
            serde_json::from_slice(&std::fs::read(dir.join(format!("ept-hierarchy/{p}.json"))).unwrap()).unwrap();
        for (k, n) in page {
            match n.as_i64().unwrap() {
                -1 if k != p => pages.push(k),
                n => {
                    assert!(n > 0, "node {k} of {n} points");
                    counts.insert(k, n as u64);
                }
            }
        }
    }
    for (k, n) in &counts {
        let tile = |ext: &str| dir.join(format!("ept-data/{k}.{ext}"));
        let points = match kind.as_str() {
            "laszip" => pointcloud::open_points(tile("laz").to_str().unwrap()).unwrap().header().point_count,
            "binary" => std::fs::metadata(tile("bin")).unwrap().len() / rl,
            _ => zstd::decode_all(&std::fs::read(tile("zst")).unwrap()[..]).unwrap().len() as u64 / rl,
        };
        assert_eq!(points, *n, "node {k}");
        let p: Vec<i64> = k.split('-').map(|v| v.parse().unwrap()).collect();
        if p[0] > 0 {
            let parent = format!("{}-{}-{}-{}", p[0] - 1, p[1] >> 1, p[2] >> 1, p[3] >> 1);
            assert!(counts.contains_key(&parent), "node {k} without its parent");
        }
    }
    assert_eq!(counts.values().sum::<u64>(), ept["points"].as_u64().unwrap());
    counts
}

/// Points appended to an EPT dataset, in every data type, in memory and out of core: every point
/// once, the hierarchy valid, `ept.json` and the source list updated.
#[test]
fn ept_datasets_take_points_appended() {
    let c = cloud(60_000, 21);
    let names = ["x", "y", "z", "intensity", "return_number", "number_of_returns", "classification", "gps_time"];
    let b = c.bounds().unwrap();
    let cube = [b[0], b[1], b[2], b[3], b[4], b[5]];
    // An octree deeper than the points' own (a cube 30 times wider), with leaves to split.
    let wide = [b[0] - 1_500.0, b[1] - 1_500.0, b[2] - 1_500.0, b[3] + 1_500.0, b[4] + 1_500.0, b[5] + 1_500.0];
    let las = pointcloud::open_points_bytes(pointcloud::write_points_bytes(&c, "a.las", &Default::default()).unwrap(), "a.las")
        .unwrap()
        .read(&Default::default())
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    for (data_type, memory) in [(EptDataType::Laszip, None), (EptDataType::Binary, Some(3_000)), (EptDataType::Zstandard, None)] {
        let path = dir.path().join(format!("{data_type:?}")).join("ept.json");
        let path_s = path.to_str().unwrap();
        let options = PointWriteOptions {
            ept_data_type: data_type,
            ept_bounds: Some(wide),
            copc_leaf_points: Some(4_000),
            copc_memory_points: memory,
            ..Default::default()
        };
        pointcloud::write_points(&c.slice(0, 20_000), path_s, &options).unwrap();
        let before = check_hierarchy(path.parent().unwrap());
        // Two sources: the second in chunks.
        let mut a = pointcloud::EptAppender::open(path_s, &options).unwrap();
        a.start_source("second.laz", &c).unwrap();
        a.write(&c.slice(20_000, 30_000)).unwrap();
        a.start_source("third.laz", &c).unwrap();
        for k in 0..3 {
            a.write(&c.slice(30_000 + k * 10_000, 40_000 + k * 10_000)).unwrap();
        }
        a.finish().unwrap();
        let after = check_hierarchy(path.parent().unwrap());
        assert!(after.len() > before.len(), "{data_type:?}: leaves split");
        let mut r = pointcloud::open_points(path_s).unwrap();
        let info = r.ept_info().unwrap().clone();
        assert_eq!(info.points, 60_000, "{data_type:?}");
        let near = |a: &[f64; 6], b: &[f64; 6]| a.iter().zip(b).all(|(x, y)| (x - y).abs() < 1e-6);
        assert!(near(&info.bounds_conforming, &cube), "{data_type:?}: {:?}", info.bounds_conforming);
        let back = r.read(&Default::default()).unwrap();
        assert_eq!(sorted_rows(&back, &names), sorted_rows(&las, &names), "{data_type:?}");
        // The root's resolution still reads a sample.
        let root = r.read(&pointcloud::PointReadOptions { max_level: Some(0), ..Default::default() }).unwrap();
        assert!(root.len() < 60_000 && root.len() >= before["0-0-0-0"] as usize, "{data_type:?}: {} root points", root.len());
        let sources: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.with_file_name("ept-sources").join("manifest.json")).unwrap()).unwrap();
        let points: Vec<u64> = sources.as_array().unwrap().iter().map(|s| s["points"].as_u64().unwrap()).collect();
        assert_eq!(points, [20_000, 10_000, 30_000], "{data_type:?}: {sources}");
        assert_eq!(sources[2]["path"], "third.laz");
        assert!(path.with_file_name("ept-sources").join("2.json").exists());
        let left: Vec<String> =
            std::fs::read_dir(path.parent().unwrap()).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(!left.iter().any(|n| n.starts_with(".terrio")), "{left:?}");
    }
}

/// Points outside an EPT dataset's cube are refused, and the dataset is left as it was; so are
/// points in another CRS.
#[test]
fn ept_appends_outside_the_cube_are_refused() {
    let c = cloud(10_000, 22);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("set/ept.json");
    let path = path.to_str().unwrap();
    pointcloud::write_points(&c, path, &Default::default()).unwrap();
    let before = std::fs::read(dir.path().join("set/ept.json")).unwrap();
    let mut far = cloud(100, 23);
    let x: Vec<f64> = far.dim("x").unwrap().to_f64().iter().map(|v| v + 1_000.0).collect();
    far.dimensions[0] = Dimension::new("x", Values::F64(x));
    let mut a = pointcloud::EptAppender::open(path, &Default::default()).unwrap();
    a.start_source("far.laz", &far).unwrap();
    let e = a.write(&far).unwrap_err().to_string();
    assert!(e.contains("outside the EPT dataset's cube"), "{e}");
    // One append at a time.
    assert!(pointcloud::EptAppender::open(path, &Default::default()).is_err());
    drop(a);
    // A lock file left behind (by a process that ended) does not lock the dataset on Unix.
    #[cfg(unix)]
    {
        std::fs::write(dir.path().join("set/.terrio-ept-append.lock"), "999999").unwrap();
        drop(pointcloud::EptAppender::open(path, &Default::default()).unwrap());
    }
    assert_eq!(std::fs::read(dir.path().join("set/ept.json")).unwrap(), before);
    assert!(!dir.path().join("set/ept-sources").exists());
    // A chunk refused changes nothing: the points written after it are appended alone.
    let mut a = pointcloud::EptAppender::open(path, &Default::default()).unwrap();
    a.start_source("far.laz", &far).unwrap();
    assert!(a.write(&far).is_err());
    a.write(&c.slice(0, 500)).unwrap();
    a.finish().unwrap();
    assert_eq!(pointcloud::read_points(path, &Default::default()).unwrap().len(), 10_500);
    // Options that only apply to new datasets are refused with --append.
    let options = pointcloud::ConvertOptions {
        append: true,
        write: PointWriteOptions { ept_bounds: Some([0.0; 6]), ..Default::default() },
        ..Default::default()
    };
    assert!(pointcloud::convert(path, path, &options).is_err());
    let mut other = cloud(100, 24);
    other.crs = SpatialRef::from_epsg(4326).unwrap();
    let mut a = pointcloud::EptAppender::open(path, &Default::default()).unwrap();
    assert!(a.start_source("other.laz", &other).is_err());
    // Writing points outside the bounds given to the writer fails too.
    let options = PointWriteOptions { ept_bounds: Some([0.0, 0.0, 0.0, 1.0, 1.0, 1.0]), ..Default::default() };
    assert!(pointcloud::write_points(&c, dir.path().join("small/ept.json").to_str().unwrap(), &options).is_err());
}

/// An EPT dataset written by Entwine, with hierarchy pages of its own (PDAL's lone-star test
/// data), merged with itself: every point twice, the pages kept, the copies' `OriginId` their
/// source's position in the source list.
#[test]
fn ept_datasets_written_by_entwine_take_points_appended() {
    let Some(src) = data("ept/lone-star-laszip/ept.json") else { return };
    let src_dir = std::path::Path::new(&src).parent().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let dst = dir.path().join("lone-star");
    std::fs::create_dir_all(&dst).unwrap();
    for sub in ["ept-data", "ept-hierarchy"] {
        std::fs::create_dir_all(dst.join(sub)).unwrap();
        for e in std::fs::read_dir(src_dir.join(sub)).unwrap() {
            let e = e.unwrap();
            std::fs::copy(e.path(), dst.join(sub).join(e.file_name())).unwrap();
        }
    }
    std::fs::copy(&src, dst.join("ept.json")).unwrap();
    let dst_json = dst.join("ept.json");
    let options = pointcloud::ConvertOptions { append: true, ..Default::default() };
    pointcloud::convert(&src, dst_json.to_str().unwrap(), &options).unwrap();
    let counts = check_hierarchy(&dst);
    assert_eq!(counts.values().sum::<u64>(), 2 * 518_862);
    assert_eq!(std::fs::read_dir(dst.join("ept-hierarchy")).unwrap().count(), 6, "the pages kept");
    let a = pointcloud::read_points(&src, &Default::default()).unwrap();
    let b = pointcloud::read_points(dst_json.to_str().unwrap(), &Default::default()).unwrap();
    let names = ["x", "y", "z", "intensity", "classification", "gps_time"];
    let doubled: Vec<Vec<f64>> = sorted_rows(&a, &names).into_iter().flat_map(|r| [r.clone(), r]).collect();
    let mut got = sorted_rows(&b, &names);
    got.sort_by(|p, q| p.iter().zip(q).map(|(x, y)| x.total_cmp(y)).find(|o| o.is_ne()).unwrap_or(std::cmp::Ordering::Equal));
    let mut want = doubled;
    want.sort_by(|p, q| p.iter().zip(q).map(|(x, y)| x.total_cmp(y)).find(|o| o.is_ne()).unwrap_or(std::cmp::Ordering::Equal));
    assert_eq!(got, want);
    // The dataset has no source list: one is made, an entry for each origin of its points, and
    // the copies' OriginId follows theirs.
    let last = a.dim("origin_id").unwrap().to_f64().into_iter().fold(0.0, f64::max);
    let origin = b.dim("origin_id").unwrap().to_f64();
    assert_eq!(origin.iter().filter(|o| **o == last + 1.0).count(), 518_862);
    let list: serde_json::Value = serde_json::from_slice(&std::fs::read(dst.join("ept-sources/manifest.json")).unwrap()).unwrap();
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), last as usize + 2);
    assert_eq!(list.iter().map(|s| s["points"].as_u64().unwrap()).sum::<u64>(), 2 * 518_862);
}

/// An update interrupted once complete (its `COMMITTED` marker written) is completed by the next
/// append; one interrupted before is discarded.
#[test]
fn ept_appends_interrupted_are_completed_or_discarded() {
    let c = cloud(10_000, 25);
    let dir = tempfile::tempdir().unwrap();
    let set = dir.path().join("set");
    let path = set.join("ept.json");
    pointcloud::write_points(&c, path.to_str().unwrap(), &Default::default()).unwrap();
    let mut ept: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    ept["marker"] = serde_json::json!("recovered");
    let done = set.join(".terrio-ept-append-0");
    std::fs::create_dir_all(done.join("ept-data")).unwrap();
    std::fs::write(done.join("ept.json"), serde_json::to_string(&ept).unwrap()).unwrap();
    std::fs::write(done.join("COMMITTED"), "").unwrap();
    let partial = set.join(".terrio-ept-append-1");
    std::fs::create_dir_all(partial.join("ept-data")).unwrap();
    std::fs::write(partial.join("ept.json"), "{}").unwrap();
    let a = pointcloud::EptAppender::open(path.to_str().unwrap(), &Default::default()).unwrap();
    a.finish().unwrap();
    let ept: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(ept["marker"], "recovered");
    assert!(!done.exists() && !partial.exists());
    assert_eq!(pointcloud::read_points(path.to_str().unwrap(), &Default::default()).unwrap().len(), 10_000);
}

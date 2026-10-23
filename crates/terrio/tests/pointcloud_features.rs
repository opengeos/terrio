//! Point clouds: PDAL's neighbourhood filters (dbscan, covariance features, eigenvalues,
//! approximate coplanarity, rank, local outlier factors, nearest-neighbour distances, radial
//! density, reciprocity, miniball, z smoothing), ordering and statistics filters (head, tail,
//! sort, locate, IQR, MAD), skewness balancing, and the filters that read rasters and vector
//! layers (dem, colorization, overlay).
//!
//! The synthetic tests run everywhere. The real-data tests read `autzen_trim.las` of
//! `scripts/fetch_test_data.sh` from `TERRIO_TEST_DATA` (they pass without checking anything when
//! it is not set); their expected values come from PDAL 2.9, and
//! `scripts/compare_pointclouds.py` compares every point with PDAL's.

use std::path::PathBuf;
use terrio::crs::SpatialRef;
use terrio::pointcloud::filters::{
    self, BandDimension, CovarianceFeaturesOptions, DbscanOptions, DimRange, EigenvaluesOptions, FeatureMode, Neighbourhood,
    NnDistanceMode, OverlayOptions,
};
use terrio::pointcloud::{self, Dimension, PointCloud, Values};
use terrio::{FieldDef, FieldType, FieldValue, Geometry, Layer, Raster, RasterConfig, RasterDataset};

fn data(name: &str) -> Option<String> {
    let dir = PathBuf::from(std::env::var_os("TERRIO_TEST_DATA")?);
    let p = dir.join("pointcloud").join(name);
    assert!(p.exists(), "{} is missing from TERRIO_TEST_DATA", p.display());
    Some(p.to_string_lossy().into_owned())
}

/// Points from coordinates, with intensities 0, 1, 2...
fn points(xyz: &[[f64; 3]]) -> PointCloud {
    let col = |k: usize| Values::F64(xyz.iter().map(|p| p[k]).collect());
    pointcloud::from_dimensions(
        vec![
            Dimension::new("x", col(0)),
            Dimension::new("y", col(1)),
            Dimension::new("z", col(2)),
            Dimension::new("intensity", Values::U16((0..xyz.len() as u16).collect())),
            Dimension::new("classification", Values::U8(vec![1; xyz.len()])),
        ],
        SpatialRef::default(),
    )
}

/// A square grid of `n × n` points one unit apart, on the plane z = 0.1 x + 0.2 y.
fn plane(n: usize) -> Vec<[f64; 3]> {
    (0..n * n)
        .map(|i| {
            let (x, y) = ((i % n) as f64, (i / n) as f64);
            [x, y, 0.1 * x + 0.2 * y]
        })
        .collect()
}

fn col(c: &PointCloud, name: &str) -> Vec<f64> {
    c.dim(name).unwrap_or_else(|| panic!("no {name}")).to_f64()
}

#[test]
fn clusters_by_density() {
    // Two dense rows of points and a lone point between them.
    let mut xyz: Vec<[f64; 3]> = (0..10).map(|i| [i as f64 * 0.5, 0.0, 0.0]).collect();
    xyz.push([20.0, 0.0, 0.0]);
    xyz.extend((0..10).map(|i| [40.0 + i as f64 * 0.5, 0.0, 0.0]));
    let c = filters::dbscan(&points(&xyz), &DbscanOptions { min_points: 3, ..Default::default() }).unwrap();
    let ids = c.dim("cluster_id").unwrap();
    assert!(matches!(ids.values, Values::I64(_)));
    assert_eq!(ids.to_f64(), [vec![0.0; 10], vec![-1.0], vec![1.0; 10]].concat());
    // In x alone, as in 3D here; too few points around each: all noise.
    let o = DbscanOptions { min_points: 30, dimensions: vec!["X".into()], ..Default::default() };
    assert!(col(&filters::dbscan(&points(&xyz), &o).unwrap(), "cluster_id").iter().all(|v| *v == -1.0));
    let o = DbscanOptions { dimensions: vec!["Nope".into()], ..Default::default() };
    assert!(filters::dbscan(&points(&xyz), &o).is_err());
}

#[test]
fn computes_covariance_features() {
    // A tilted plane: two equal eigenvalues and a third of 0, the normal off the vertical.
    let c = points(&plane(20));
    let o = CovarianceFeaturesOptions { feature_set: vec!["all".into(), "Density".into()], ..Default::default() };
    assert!(filters::covariance_features(&c, &o).is_err(), "Density needs OptimalKNN and OptimalRadius");
    let o = CovarianceFeaturesOptions {
        feature_set: vec!["Linearity".into(), "planarity".into(), " SCATTERING ".into(), "DemantkeVerticality".into(), "none".into()],
        mode: FeatureMode::Raw,
        ..Default::default()
    };
    let f = filters::covariance_features(&c, &o).unwrap();
    assert!(f.dim("verticality").is_none() && f.dim("none").is_none());
    let interior = |i: usize| (3..17).contains(&(i % 20)) && (3..17).contains(&(i / 20));
    let (scattering, demantke) = (col(&f, "scattering"), col(&f, "demantke_verticality"));
    let normal_z = 1.0 / (1.0f64 + 0.01 + 0.04).sqrt();
    for i in (0..400).filter(|&i| interior(i)) {
        assert!(scattering[i].abs() < 1e-9, "point {i}: {}", scattering[i]);
        // The offsets from the mean are rounded to floats, as PDAL rounds them.
        assert!((demantke[i] - (1.0 - normal_z)).abs() < 1e-7, "point {i}: {}", demantke[i]);
    }
    // A line: linearity 1.
    let line: Vec<[f64; 3]> = (0..50).map(|i| [i as f64, 2.0 * i as f64, 0.5 * i as f64]).collect();
    let f = filters::covariance_features(&points(&line), &CovarianceFeaturesOptions::default()).unwrap();
    assert!(col(&f, "linearity").iter().all(|v| (v - 1.0).abs() < 1e-6));
    assert!(FeatureMode::parse("SQRT").is_ok() && FeatureMode::parse("cube").is_err());
}

#[test]
fn computes_eigenvalues_and_ranks() {
    let c = points(&plane(15));
    let o = EigenvaluesOptions { normalize: true, ..Default::default() };
    let e = filters::eigenvalues(&c, &o).unwrap();
    let sums: Vec<f64> = (0..c.len()).map(|i| (0..3).map(|k| e.dim(&format!("eigenvalue{k}")).unwrap().get(i)).sum()).collect();
    assert!(sums.iter().all(|s| (s - 1.0).abs() < 1e-12));
    assert!(col(&e, "eigenvalue0").iter().all(|v| v.abs() < 1e-9));
    // With a radius: the neighbourhoods with fewer than min_k points are skipped (0).
    let o = EigenvaluesOptions { neighbours: Neighbourhood { knn: 8, stride: 1, radius: Some(0.5), min_k: 3 }, normalize: false };
    assert!(col(&filters::eigenvalues(&c, &o).unwrap(), "eigenvalue2").iter().all(|v| *v == 0.0));
    // A plane is of rank 2, approximately coplanar.
    assert!(col(&filters::estimate_rank(&c, 8, 0.01).unwrap(), "rank").iter().all(|v| *v == 2.0));
    let coplanar = col(&filters::approximate_coplanar(&c, 8, 25.0, 6.0).unwrap(), "coplanar");
    assert!(coplanar.iter().filter(|v| **v == 1.0).count() > 150);
}

#[test]
fn measures_nearest_neighbours() {
    // A grid one unit apart, and a point far from it.
    let mut xyz: Vec<[f64; 3]> = (0..100).map(|i| [(i % 10) as f64, (i / 10) as f64, 0.0]).collect();
    xyz.push([100.0, 100.0, 0.0]);
    let c = points(&xyz);
    let lof = filters::lof(&c, 5).unwrap();
    let factors = col(&lof, "local_outlier_factor");
    let max = factors.iter().copied().fold(f64::MIN, f64::max);
    assert_eq!(factors[100], max, "the lone point is the most outlying");
    assert!(lof.dim("nndistance").is_some() && lof.dim("local_reachability_distance").is_some());
    // The 4 nearest neighbours of an interior grid point are 1 away.
    let d = col(&filters::nndistance(&c, 4, NnDistanceMode::Kth).unwrap(), "nndistance");
    assert_eq!(d[55], 1.0);
    let d = col(&filters::nndistance(&c, 8, NnDistanceMode::Avg).unwrap(), "nndistance");
    assert!((d[55] - (4.0 + 4.0 * 2f64.sqrt()) / 8.0).abs() < 1e-12);
    // Within 1.1: the point itself and its 4 neighbours.
    let density = col(&filters::radial_density(&c, 1.1).unwrap(), "radial_density");
    let pdal_pi = 314_159.0 / 100_000.0;
    assert!((density[55] - 5.0 / (4.0 / 3.0 * pdal_pi * 1.1f64.powi(3))).abs() < 1e-12);
    assert_eq!(col(&filters::reciprocity(&c, 4).unwrap(), "reciprocity")[100], 100.0);
    let m = col(&filters::miniball(&c, 8).unwrap(), "miniball");
    assert!(m[55].abs() < 1e-12, "a grid point is at the centre of its 8 neighbours");
    assert!(NnDistanceMode::parse("AVG").is_ok() && NnDistanceMode::parse("max").is_err());
}

#[test]
fn smooths_elevations() {
    // Three points at each place, a spike in the middle.
    let xyz = vec![[0.0, 0.0, 1.0], [0.5, 0.0, 2.0], [0.0, 0.5, 3.0], [0.2, 0.2, 100.0], [10.0, 10.0, 7.0]];
    let c = filters::zsmooth(&points(&xyz), "Smoothed", 1.0, 50.0).unwrap();
    let s = col(&c, "smoothed");
    assert_eq!(s[3], 2.0, "the median of 1, 2 and 3");
    assert_eq!(s[4], 7.0, "a point alone keeps its z");
    let low = col(&filters::zsmooth(&points(&xyz), "Smoothed", 1.0, 0.0).unwrap(), "smoothed");
    assert_eq!(low[3], 1.0);
    assert!(filters::zsmooth(&points(&xyz), "Z", 1.0, 50.0).is_err());
}

#[test]
fn balances_skewness() {
    // Ground near 0, then trees: the points come out sorted by z, the low ones ground.
    let mut xyz: Vec<[f64; 3]> = (0..200).map(|i| [i as f64, 0.0, ((i * 37) % 11) as f64 * 0.01]).collect();
    xyz.extend((0..40).map(|i| [i as f64, 1.0, 10.0 + i as f64]));
    let c = filters::skewness_balancing(&points(&xyz), 2, 1, false).unwrap();
    let z = col(&c, "z");
    assert!(z.windows(2).all(|w| w[0] <= w[1]));
    // The ground is the lowest points, the trees among the others.
    let classes = col(&c, "classification");
    let ground = classes.iter().take_while(|c| **c == 2.0).count();
    assert!(ground > 0 && ground < 200 && classes[ground..].iter().all(|c| *c == 1.0), "{ground}");
    assert!(filters::skewness_balancing(&points(&xyz), 2, 2, false).is_err());
}

#[test]
fn selects_and_orders_points() {
    let xyz: Vec<[f64; 3]> = (0..20).map(|i| [i as f64, (i % 3) as f64, ((i * 7) % 20) as f64]).collect();
    let c = points(&xyz);
    assert_eq!(col(&filters::head(&c, 3, false), "x"), [0.0, 1.0, 2.0]);
    assert_eq!(filters::head(&c, 3, true).len(), 17);
    assert_eq!(col(&filters::tail(&c, 2, false), "x"), [18.0, 19.0]);
    assert_eq!(filters::tail(&c, 50, false).len(), 20);
    assert_eq!(filters::tail(&c, 50, true).len(), 0);
    let s = filters::sort(&c, &["Z".into()], true).unwrap();
    assert!(col(&s, "z").windows(2).all(|w| w[0] >= w[1]));
    // By y, then x: the last dimension orders, the first breaks ties.
    let s = filters::sort(&c, &["X".into(), "Y".into()], false).unwrap();
    assert_eq!(col(&s, "x")[..7], [0.0, 3.0, 6.0, 9.0, 12.0, 15.0, 18.0]);
    assert_eq!(col(&filters::locate(&c, "Z", false).unwrap(), "z"), [19.0]);
    assert_eq!(col(&filters::locate(&c, "z", true).unwrap(), "x"), [0.0]);
    // An outlier beyond the fences.
    let mut xyz = xyz;
    xyz.push([0.0, 0.0, 1000.0]);
    let c = points(&xyz);
    assert_eq!(filters::iqr(&c, "Z", 1.5).unwrap().len(), 20);
    assert_eq!(filters::mad(&c, "Z", 2.0, 1.4862).unwrap().len(), 20);
    assert!(filters::sort(&c, &[], false).is_err() && filters::locate(&c, "nope", true).is_err());
}

/// A 10 × 10 raster of 1-unit cells from (0, 0), two bands: x + 10 y, and 100 − that.
fn raster() -> RasterDataset {
    let cfg = RasterConfig { cols: 10, rows: 10, bands: 2, x_min: 0.0, y_min: 0.0, cell_size: 1.0, ..Default::default() };
    let mut data = Vec::new();
    for band in 0..2 {
        for row in 0..10 {
            for c in 0..10 {
                // Rows from the top: y of the row's centre is 9.5 - row.
                let v = c as f64 + 10.0 * (9 - row) as f64;
                data.push(if band == 0 { v } else { 100.0 - v });
            }
        }
    }
    RasterDataset::new(Raster::from_data(cfg, data).unwrap(), true)
}

#[test]
fn keeps_points_near_a_raster_surface() {
    // Points at the cell centres, z the cell's value plus 0, 1, 2 or 3; one outside.
    let mut xyz: Vec<[f64; 3]> = (0..100).map(|i| [(i % 10) as f64 + 0.5, (i / 10) as f64 + 0.5, i as f64 + (i % 4) as f64]).collect();
    xyz.push([20.0, 20.0, 0.0]);
    let c = points(&xyz);
    let kept = filters::dem(&c, &raster(), &DimRange::parse("Z[0:1.5]").unwrap(), 1).unwrap();
    assert_eq!(kept.len(), 50, "z up to 1.5 above the surface");
    assert!(filters::dem(&c, &raster(), &DimRange::parse("Z[0:1]").unwrap(), 3).is_err());
}

#[test]
fn colorizes_points_from_raster_bands() {
    let xyz = vec![[0.5, 9.5, 0.0], [3.2, 0.1, 0.0], [10.5, 5.0, 0.0]];
    let dims = BandDimension::parse_list("Red:1:2, Shade:2").unwrap();
    let c = filters::colorization(&points(&xyz), &raster(), &dims).unwrap();
    // Doubles, as PDAL makes them; the point outside keeps 0.
    assert!(matches!(c.dim("red").unwrap().values, Values::F64(_)));
    assert_eq!(col(&c, "red"), [180.0, 6.0, 0.0]);
    assert_eq!(col(&c, "shade"), [10.0, 97.0, 0.0]);
    let rgb = BandDimension::rgb();
    assert!(filters::colorization(&points(&xyz), &raster(), &rgb).is_err(), "no third band");
}

#[test]
fn overlays_polygon_attributes() {
    let mut layer = Layer::new("zones");
    layer.add_field(FieldDef::new("name", FieldType::Text));
    layer.add_field(FieldDef::new("zone", FieldType::Integer));
    let square = |x0: f64, holes: Vec<Vec<wbvector::Coord>>| {
        let ring = [(x0, 0.0), (x0 + 10.0, 0.0), (x0 + 10.0, 10.0), (x0, 10.0), (x0, 0.0)];
        Geometry::polygon(ring.iter().map(|&(x, y)| wbvector::Coord::xy(x, y)).collect(), holes)
    };
    let hole: Vec<wbvector::Coord> =
        [(2.0, 2.0), (4.0, 2.0), (4.0, 4.0), (2.0, 4.0), (2.0, 2.0)].iter().map(|&(x, y)| wbvector::Coord::xy(x, y)).collect();
    for (k, g) in [square(0.0, vec![hole]), square(5.0, vec![])].into_iter().enumerate() {
        let attributes = vec![FieldValue::Text(format!("{}7", k + 1)), FieldValue::Integer(k as i64 + 3)];
        layer.push(terrio::Feature { fid: k as u64, geometry: Some(g), attributes });
    }
    let xyz = vec![[1.0, 1.0, 0.0], [3.0, 3.0, 0.0], [7.0, 5.0, 0.0], [12.0, 5.0, 0.0], [30.0, 5.0, 0.0], [0.0, 5.0, 0.0]];
    let c = points(&xyz);
    // By default the second attribute: the first polygon has the point, the hole does not.
    let o = OverlayOptions { dimension: "Classification".into(), column: None };
    assert_eq!(col(&filters::overlay(&c, &layer, &o).unwrap(), "classification"), [3.0, 1.0, 3.0, 4.0, 1.0, 3.0]);
    let o = OverlayOptions { dimension: "Intensity".into(), column: Some("NAME".into()) };
    assert_eq!(col(&filters::overlay(&c, &layer, &o).unwrap(), "intensity"), [17.0, 1.0, 17.0, 27.0, 4.0, 17.0]);
    assert!(filters::overlay(&c, &layer, &OverlayOptions { dimension: "Nope".into(), column: None }).is_err());
    assert!(filters::overlay(&c, &layer, &OverlayOptions { dimension: "Intensity".into(), column: Some("x".into()) }).is_err());
}

fn autzen() -> Option<PointCloud> {
    Some(pointcloud::read_points(&data("autzen_trim.las")?, &Default::default()).unwrap())
}

#[test]
fn neighbourhood_filters_as_pdal_does_on_autzen() {
    // PDAL 2.9's filters on autzen_trim.las (110 000 points, in feet).
    let Some(c) = autzen() else { return };
    let sum = |c: &PointCloud, name: &str| col(c, name).iter().sum::<f64>();
    let o = DbscanOptions { eps: 3.0, min_points: 10, ..Default::default() };
    let ids = col(&filters::dbscan(&c, &o).unwrap(), "cluster_id");
    assert_eq!((ids.iter().copied().fold(f64::MIN, f64::max), ids.iter().filter(|v| **v == -1.0).count()), (359.0, 95_169));
    let e = filters::eigenvalues(&c, &EigenvaluesOptions::default()).unwrap();
    assert_eq!(e.dim("eigenvalue2").unwrap().get(0), 16.185_960_628_220_393);
    let f = filters::covariance_features(&c, &CovarianceFeaturesOptions::default()).unwrap();
    assert_eq!(f.dim("linearity").unwrap().get(0), 0.677_748_811_127_886_5);
    assert_eq!(f.dim("verticality").unwrap().get(0), 0.032_007_386_653_545_196);
    let lof = filters::lof(&c, 10).unwrap();
    assert_eq!(lof.dim("local_outlier_factor").unwrap().get(0), 1.031_589_756_323_879_4);
    assert_eq!(sum(&filters::reciprocity(&c, 8).unwrap(), "reciprocity"), 1_061_100.0);
    assert_eq!(col(&filters::estimate_rank(&c, 8, 0.01).unwrap(), "rank").iter().filter(|r| **r == 3.0).count(), 37_754);
    assert_eq!(col(&filters::approximate_coplanar(&c, 8, 25.0, 6.0).unwrap(), "coplanar").iter().filter(|r| **r == 1.0).count(), 75_369);
    assert_eq!(filters::iqr(&c, "Z", 1.5).unwrap().len(), 82_454);
    assert_eq!(filters::mad(&c, "Z", 2.0, 1.4862).unwrap().len(), 80_067);
    let ground = filters::skewness_balancing(&c, 2, 1, false).unwrap();
    assert_eq!(col(&ground, "classification").iter().filter(|v| **v == 2.0).count(), 102_234);
}

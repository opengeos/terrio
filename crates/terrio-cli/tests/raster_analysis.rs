//! The raster-analysis and SQL commands through the real executable and format drivers.

use std::path::Path;
use std::process::Command;

fn run(args: &[&str], dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(dir).output().unwrap()
}
fn ok(args: &[&str], dir: &Path) {
    let r = run(args, dir);
    assert!(r.status.success(), "{args:?}: {}", String::from_utf8_lossy(&r.stderr));
}
fn layer(path: &Path) -> terrio::Layer {
    terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap().layers.remove(0)
}

#[test]
fn contours_zonal_statistics_and_sql_share_vector_outputs() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let cfg = terrio::RasterConfig { cols: 4, rows: 4, data_type: terrio::DataType::F64, cell_size: 1., ..Default::default() };
    let mut r = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, (1..=16).map(|v| v as f64).collect()).unwrap(), false);
    r.raster.crs.epsg = Some(4326);
    terrio::write_raster_dataset(&r, d.join("dem.tif").to_str().unwrap(), &Default::default()).unwrap();
    std::fs::write(d.join("zones.geojson"),r#"{"type":"FeatureCollection","name":"zones","features":[{"type":"Feature","properties":{"name":"all"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[4,0],[4,4],[0,4],[0,0]]]}}]}"#).unwrap();
    ok(&["contour", "dem.tif", "contours.gpkg", "--levels", "4,8,12", "--3d"], d);
    let contours = layer(&d.join("contours.gpkg"));
    assert!(contours.len() >= 3);
    for f in &contours.features {
        let z = f.attributes[1].as_f64().unwrap();
        assert!(f.geometry.as_ref().unwrap().all_coords().iter().all(|c| c.z == Some(z)));
    }
    ok(
        &[
            "zonal-stats",
            "zones.geojson",
            "stats.gpkg",
            "--raster",
            "dem.tif",
            "--raster",
            "dem.tif",
            "--prefix",
            "a",
            "--prefix",
            "b",
            "--stats",
            "count,mean,median",
        ],
        d,
    );
    let stats = layer(&d.join("stats.gpkg"));
    assert_eq!(stats.features[0].attributes.len(), 7);
    assert_eq!(
        stats.features[0].attributes[1..],
        [
            terrio::FieldValue::Integer(16),
            terrio::FieldValue::Float(8.5),
            terrio::FieldValue::Float(8.5),
            terrio::FieldValue::Integer(16),
            terrio::FieldValue::Float(8.5),
            terrio::FieldValue::Float(8.5)
        ]
    );
    ok(
        &[
            "sql",
            "stats.gpkg",
            "query.gpkg",
            "--query",
            "SELECT name AS label, a_b1_mean FROM zones WHERE a_b1_count = 16 LIMIT 1",
            "--nln",
            "result",
        ],
        d,
    );
    let query = layer(&d.join("query.gpkg"));
    assert_eq!(query.name, "result");
    assert_eq!(query.features[0].attributes, [terrio::FieldValue::Text("all".into()), terrio::FieldValue::Float(8.5)]);
    for args in [
        vec!["contour", "dem.tif", "bad.gpkg"],
        vec!["contour", "dem.tif", "bad.gpkg", "--levels", "NaN"],
        vec!["zonal-stats", "zones.geojson", "bad.gpkg", "--raster", "dem.tif", "--stats", "bogus"],
        vec!["sql", "stats.gpkg", "bad.gpkg", "--query", "SELECT * FROM missing"],
    ] {
        let r = run(&args, d);
        assert!(!r.status.success());
        assert!(!d.join("bad.gpkg").exists());
    }
}

#[test]
fn zonal_and_point_sampling_support_output_crs_and_decimal_places() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let cfg = terrio::RasterConfig { cols: 2, rows: 1, data_type: terrio::DataType::F64, cell_size: 1., ..Default::default() };
    let mut raster = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, vec![1.125, 2.125]).unwrap(), false);
    raster.raster.crs.epsg = Some(4326);
    for path in ["a.tif", "b.tif"] {
        terrio::write_raster_dataset(&raster, d.join(path).to_str().unwrap(), &Default::default()).unwrap();
    }
    std::fs::write(d.join("zones.geojson"), r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"original":1.23456789},"geometry":{"type":"Polygon","coordinates":[[[0,0],[2,0],[2,1],[0,1],[0,0]]]}}]}"#).unwrap();
    std::fs::write(d.join("points.geojson"), r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"original":1.23456789},"geometry":{"type":"Point","coordinates":[0.5,0.5]}},{"type":"Feature","properties":{"original":2.3456789},"geometry":{"type":"Point","coordinates":[10,10]}}]}"#).unwrap();
    for (command, source) in [("zonal-stats", "zones.geojson"), ("sample", "points.geojson"), ("point-sample", "points.geojson")] {
        let output = format!("{command}.gpkg");
        let mut args =
            vec![command, source, &output, "--raster", "a.tif", "--raster", "b.tif", "--t-srs", "EPSG:3857", "--decimal-places", "2"];
        if command == "zonal-stats" {
            args.extend(["--stats", "count,mean"]);
        }
        ok(&args, d);
        let out = layer(&d.join(&output));
        assert_eq!(out.crs_epsg(), Some(3857));
        assert_eq!(out.features[0].attributes[0], terrio::FieldValue::Float(1.23456789));
        if command == "zonal-stats" {
            assert_eq!(
                out.features[0].attributes[1..],
                [
                    terrio::FieldValue::Integer(2),
                    terrio::FieldValue::Float(1.63),
                    terrio::FieldValue::Integer(2),
                    terrio::FieldValue::Float(1.63)
                ]
            );
        } else {
            assert_eq!(out.features[0].attributes[1..], [terrio::FieldValue::Float(1.13), terrio::FieldValue::Float(1.13)]);
            assert_eq!(out.features[1].attributes[1..], [terrio::FieldValue::Null, terrio::FieldValue::Null]);
            let Some(terrio::Geometry::Point(c)) = &out.features[0].geometry else { panic!() };
            assert!((c.x - 55_659.74539663678).abs() < 1e-6);
        }
        for extra in [vec!["--decimal-places", "16"], vec!["--decimal-places", "-1"], vec!["--t-srs", "not-a-crs"]] {
            let mut args = vec![command, source, "bad.gpkg", "--raster", "a.tif"];
            args.extend(extra);
            assert!(!run(&args, d).status.success());
            assert!(!d.join("bad.gpkg").exists());
        }
    }
}

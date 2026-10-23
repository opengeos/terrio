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

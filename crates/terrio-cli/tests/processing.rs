//! Run the real commands through vector/raster I/O, including aliases and error handling.

use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_terrio")).args(args).current_dir(dir).output().unwrap()
}

fn ok(args: &[&str], dir: &Path) {
    let result = run(args, dir);
    assert!(result.status.success(), "{args:?}: {}", String::from_utf8_lossy(&result.stderr));
}

fn first(path: &Path) -> terrio::Layer {
    terrio::open_vector(path.to_str().unwrap(), &Default::default()).unwrap().layers.remove(0)
}

#[test]
fn topology_commands_preserve_attributes_and_use_output_modes() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("a.geojson"), r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"group":"east"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[1,0],[1,1],[0,1],[0,0]]]}},{"type":"Feature","properties":{"group":"east"},"geometry":{"type":"Polygon","coordinates":[[[1,0],[2,0],[2,1],[1,1],[1,0]]]}},{"type":"Feature","properties":{"group":"west"},"geometry":null}]}"#).unwrap();
    std::fs::write(d.join("b.geojson"), r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"zone":7},"geometry":{"type":"Polygon","coordinates":[[[0.5,0],[1.5,0],[1.5,1],[0.5,1],[0.5,0]]]}}]}"#).unwrap();
    std::fs::write(d.join("invalid.geojson"), r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"bowtie"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[2,2],[0,2],[2,0],[0,0]]]}}]}"#).unwrap();
    ok(&["buffer", "a.geojson", "buffer.gpkg", "--distance", "0.1", "--quad-segs", "16", "--nln", "buffered"], d);
    assert_eq!(first(&d.join("buffer.gpkg")).len(), 3);
    let before = first(&d.join("buffer.gpkg")).features[0].geometry.clone();
    // Default output replaces the dataset; append adds features to the named layer.
    ok(&["buffer", "a.geojson", "buffer.gpkg", "--distance", "1", "--nln", "buffered"], d);
    assert_ne!(first(&d.join("buffer.gpkg")).features[0].geometry, before);
    ok(&["buffer", "a.geojson", "buffer.gpkg", "--distance", "1", "--nln", "buffered", "--append"], d);
    assert_eq!(first(&d.join("buffer.gpkg")).len(), 6);
    ok(&["buffer", "a.geojson", "buffer.gpkg", "--distance", "-0.1", "--nln", "buffered", "--overwrite"], d);
    assert_eq!(first(&d.join("buffer.gpkg")).len(), 3);
    ok(&["dissolve", "a.geojson", "dissolved.gpkg", "--field", "group"], d);
    let dissolved = first(&d.join("dissolved.gpkg"));
    assert_eq!(dissolved.len(), 2);
    assert_eq!(dissolved.schema.len(), 2);
    assert_eq!(dissolved.features[0].attributes[1], terrio::FieldValue::Integer(2));
    ok(&["is-valid", "invalid.geojson", "validity.gpkg"], d);
    assert_eq!(first(&d.join("validity.gpkg")).features[0].attributes[1], terrio::FieldValue::Boolean(false));
    ok(&["make-valid", "invalid.geojson", "fixed.gpkg"], d);
    let fixed = first(&d.join("fixed.gpkg"));
    assert_eq!(fixed.len(), 1);
    assert_eq!(terrio::ops::topology::validity(&fixed).features[0].attributes[1], terrio::FieldValue::Boolean(true));
    ok(&["clean", "a.geojson", "cleaned.gpkg", "--snap-grid", "0.1"], d);
    assert_eq!(first(&d.join("cleaned.gpkg")).len(), 3);
    for op in ["intersection", "union", "difference", "symmetric-difference"] {
        let name = format!("{op}.gpkg");
        ok(&["overlay", "a.geojson", "b.geojson", &name, "--operation", op], d);
        let out = first(&d.join(&name));
        assert!(!out.is_empty());
        assert!(out.schema.fields().iter().any(|f| f.name == "a_group"));
        assert!(out.schema.fields().iter().any(|f| f.name == "b_zone"));
    }
    for args in [
        vec!["buffer", "a.geojson", "bad.gpkg", "--distance", "NaN"],
        vec!["buffer", "a.geojson", "bad.gpkg", "--distance", "1", "--quad-segs", "0"],
        vec!["buffer", "invalid.geojson", "bad.gpkg", "--distance", "1"],
        vec!["dissolve", "a.geojson", "bad.gpkg", "--field", "missing"],
        vec!["clean", "a.geojson", "bad.gpkg", "--snap-grid", "-1"],
        vec!["overlay", "a.geojson", "b.geojson", "bad.gpkg", "--operation", "invalid"],
    ] {
        assert!(!run(&args, d).status.success(), "{args:?}");
        assert!(!d.join("bad.gpkg").exists());
    }
}

#[test]
fn geometry_commands_sample_multiple_rasters_and_create_a_footprint() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("input.geojson"), r#"{"type":"FeatureCollection","features":[{"type":"Feature","id":9,"properties":{"label":"square"},"geometry":{"type":"MultiPolygon","coordinates":[[[[0,0],[1,0],[1,1],[0,1],[0,0]]],[[[1,1],[2,1],[2,2],[1,2],[1,1]]]]}}]}"#).unwrap();
    ok(&["multipart-to-singlepart", "input.geojson", "parts.gpkg", "--nln", "parts"], d);
    assert_eq!(first(&d.join("parts.gpkg")).len(), 2);
    ok(&["centroids", "parts.gpkg", "points.gpkg", "--layer", "parts"], d);
    let points = first(&d.join("points.gpkg"));
    assert_eq!(points.len(), 2);
    let Some(terrio::Geometry::Point(c)) = &points.features[0].geometry else { panic!() };
    assert_eq!((c.x, c.y), (0.5, 0.5));
    ok(&["extract-vertices", "input.geojson", "vertices.gpkg"], d);
    assert_eq!(first(&d.join("vertices.gpkg")).len(), 10);

    let cfg =
        terrio::RasterConfig { cols: 2, rows: 2, cell_size: 1.0, data_type: terrio::DataType::F64, nodata: -99.0, ..Default::default() };
    let mut ds = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, vec![10.0, 20.0, -99.0, 40.0]).unwrap(), true);
    ds.raster.crs.epsg = Some(4326);
    for name in ["a.tif", "b.tif"] {
        terrio::write_raster_dataset(&ds, d.join(name).to_str().unwrap(), &Default::default()).unwrap();
    }
    ok(&["sample", "points.gpkg", "samples.gpkg", "--raster", "a.tif", "--raster", "b.tif", "--prefix", "a", "--prefix", "b"], d);
    let samples = first(&d.join("samples.gpkg"));
    assert_eq!(samples.features[0].attributes[1..], [terrio::FieldValue::Null, terrio::FieldValue::Null]);
    assert_eq!(samples.features[1].attributes[1..], [terrio::FieldValue::Float(20.0), terrio::FieldValue::Float(20.0)]);
    assert_eq!(samples.schema.fields()[1].name, "a_b1");
    assert_eq!(samples.schema.fields()[2].name, "b_b1");
    ok(&["footprint", "a.tif", "footprint.gpkg"], d);
    assert_eq!(first(&d.join("footprint.gpkg")).len(), 1);

    for (args, message) in [
        (vec!["sample", "points.gpkg", "bad.gpkg", "--raster", "a.tif", "--band", "0"], "band 0"),
        (vec!["sample", "input.geojson", "bad.gpkg", "--raster", "a.tif"], "not a point"),
        (vec!["sample", "points.gpkg", "bad.gpkg", "--raster", "a.tif", "--prefix", "x", "--prefix", "y"], "one --prefix"),
        (vec!["footprint", "a.tif", "bad.gpkg", "--band", "0"], "band 0"),
    ] {
        let result = run(&args, d);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains(message), "{}", String::from_utf8_lossy(&result.stderr));
        assert!(!d.join("bad.gpkg").exists());
    }
}

#[test]
fn sample_keeps_valid_points_after_a_projection_failure() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("sites.geojson"), r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"site":0},"geometry":{"type":"Point","coordinates":[0,0]}},{"type":"Feature","properties":{"site":1},"geometry":{"type":"Point","coordinates":[180,0]}},{"type":"Feature","properties":{"site":2},"geometry":{"type":"Point","coordinates":[0,0]}}]}"#).unwrap();
    let cfg = terrio::RasterConfig {
        cols: 1,
        rows: 1,
        x_min: -1.0,
        y_min: -1.0,
        cell_size: 2.0,
        data_type: terrio::DataType::F64,
        ..Default::default()
    };
    let mut ds = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, vec![42.0]).unwrap(), false);
    ds.raster.crs = terrio::crs::SpatialRef::parse("+proj=ortho +lat_0=0 +lon_0=0 +datum=WGS84 +units=m").unwrap().to_crs_info();
    terrio::write_raster_dataset(&ds, d.join("ortho.tif").to_str().unwrap(), &Default::default()).unwrap();
    let result = run(&["sample", "sites.geojson", "samples.gpkg", "--raster", "ortho.tif"], d);
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert_eq!(stderr.lines().filter(|l| l.contains("could not transform")).count(), 1);
    assert!(stderr.contains("warning") && stderr.contains("ortho_b1") && stderr.contains("1 point into"));
    let samples = first(&d.join("samples.gpkg"));
    assert_eq!(samples.len(), 3);
    for (i, value) in [terrio::FieldValue::Float(42.0), terrio::FieldValue::Null, terrio::FieldValue::Float(42.0)].into_iter().enumerate() {
        assert_eq!(samples.features[i].attributes[0], terrio::FieldValue::Integer(i as i64));
        assert_eq!(samples.features[i].attributes[1], value);
    }
}

#[test]
fn vector_conversion_refuses_ballparks_and_reports_fallbacks() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("point.geojson"), r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[110,35]}}]}"#).unwrap();
    let args = ["convert", "point.geojson", "out.geojson", "--s-srs", "EPSG:4610", "--t-srs", "EPSG:4326"];
    let output = run(&args, dir.path());
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ballpark"));
    let mut strict = args.to_vec();
    strict[2] = "strict.geojson";
    strict.extend(["--ct-opt", "ALLOW_BALLPARK=NO"]);
    let output = run(&strict, dir.path());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("ALLOW_BALLPARK"));
    assert!(!dir.path().join("strict.geojson").exists());
}

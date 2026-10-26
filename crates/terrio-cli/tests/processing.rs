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

#[test]
fn crs_promotes_2d_crss_to_3d_on_request() {
    // RD New to WGS 84 3D: the height is kept (h = 0) unless the 2D CRS is promoted, when it goes
    // through the datum transformation (43.6 m, as GDAL after `PromoteTo3D`).
    let dir = tempfile::tempdir().unwrap();
    let height = |extra: &[&str]| {
        let args = [&["crs", "EPSG:28992", "--to", "EPSG:4979", "--dim", "3", "85000", "447000", "0"], extra].concat();
        let output = run(&args, dir.path());
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let out = String::from_utf8(output.stdout).unwrap();
        out.split_whitespace().nth(2).unwrap().parse::<f64>().unwrap()
    };
    assert_eq!(height(&[]), 0.0);
    assert!((height(&["--ct-opt", "PROMOTE_TO_3D=YES"]) - 43.6).abs() < 0.1);
}

#[test]
fn point_cloud_reprojection_rejects_coordinate_options_before_reading() {
    let dir = tempfile::tempdir().unwrap();
    let output = run(&["convert", "missing.las", "out.las", "--t-srs", "EPSG:4326", "--ct-opt", "ONLY_BEST=YES"], dir.path());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--ct-opt is not supported for point-cloud reprojection"));
    assert!(!dir.path().join("out.las").exists());
}

#[test]
fn footprint_writes_source_and_custom_attributes_to_singlepart_shapefiles() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let cfg = terrio::RasterConfig { cols: 2, rows: 2, data_type: terrio::DataType::F64, nodata: 0., ..Default::default() };
    let mut ds = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, vec![1., 0., 0., 1.]).unwrap(), true);
    ds.raster.crs.epsg = Some(4326);
    terrio::write_raster_dataset(&ds, d.join("scene.tif").to_str().unwrap(), &Default::default()).unwrap();
    ok(&["footprint", "scene.tif", "parts.shp", "--singlepart", "--attribute", "label=a=b", "--attribute", "empty="], d);
    let parts = first(&d.join("parts.shp"));
    assert_eq!(parts.len(), 2);
    assert_eq!(parts.crs_epsg(), Some(4326));
    assert_eq!(parts.schema.fields().iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["location", "label", "empty"]);
    for feature in &parts.features {
        assert_eq!(&feature.attributes[..2], &[terrio::FieldValue::Text("scene.tif".into()), terrio::FieldValue::Text("a=b".into())]);
        assert!(matches!(feature.geometry, Some(terrio::Geometry::Polygon { .. })));
    }
    ok(&["footprint", "scene.tif", "multi.gpkg", "--location-field", "source"], d);
    let multi = first(&d.join("multi.gpkg"));
    assert_eq!(multi.len(), 1);
    assert_eq!(multi.schema.fields()[0].name, "source");
    assert_eq!(multi.features[0].attributes, [terrio::FieldValue::Text("scene.tif".into())]);
    ok(&["footprint", "scene.tif", "bare.gpkg", "--no-location", "--attribute", "kind=test"], d);
    assert_eq!(first(&d.join("bare.gpkg")).schema.len(), 1);
    for extra in [
        vec!["--attribute", "missing"],
        vec!["--attribute", "=value"],
        vec!["--attribute", "LOCATION=duplicate"],
        vec!["--location-field", ""],
    ] {
        let mut args = vec!["footprint", "scene.tif", "bad.shp"];
        args.extend(extra);
        assert!(!run(&args, d).status.success());
        assert!(!d.join("bad.shp").exists());
        assert!(!d.join("bad.dbf").exists());
    }
}

#[test]
fn footprint_long_source_paths_follow_dbf_limits_and_survive_in_geopackage() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let source = format!("{}/{}/scene.tif", "a".repeat(130), "b".repeat(130));
    let path = d.join(&source);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let cfg = terrio::RasterConfig { cols: 1, rows: 1, data_type: terrio::DataType::F64, ..Default::default() };
    let ds = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, vec![1.]).unwrap(), false);
    terrio::write_raster_dataset(&ds, path.to_str().unwrap(), &Default::default()).unwrap();
    ok(&["footprint", &source, "long.shp"], d);
    assert_eq!(first(&d.join("long.shp")).features[0].attributes[0], terrio::FieldValue::Text(source[..254].into()));
    ok(&["footprint", &source, "long.gpkg"], d);
    assert_eq!(first(&d.join("long.gpkg")).features[0].attributes[0], terrio::FieldValue::Text(source));
}

#[test]
fn vector_geometry_commands_stream_and_write_whole_layers() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(d.join("lines.geojson"), r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"k":"a","v":1},"geometry":{"type":"LineString","coordinates":[[0,0],[5,0.1],[10,0]]}},{"type":"Feature","properties":{"k":"b","v":2},"geometry":{"type":"LineString","coordinates":[[20,0],[30,0]]}},{"type":"Feature","properties":{"k":"a","v":3},"geometry":null}]}"#).unwrap();
    let coords = |path: &str, i: usize| -> Vec<(f64, f64)> {
        let l = first(&d.join(path));
        l.features[i].geometry.as_ref().unwrap().all_coords().iter().map(|c| (c.x, c.y)).collect()
    };
    // GeoJSON to GeoJSON streams; GeoPackage is written whole.
    for out in ["simple.geojson", "simple.gpkg"] {
        ok(&["simplify", "lines.geojson", out, "--tolerance", "1"], d);
        assert_eq!(coords(out, 0), [(0.0, 0.0), (10.0, 0.0)]);
        assert!(first(&d.join(out)).features[2].geometry.is_none(), "null geometries stay null");
    }
    ok(&["segmentize", "lines.geojson", "dense.geojson", "--max-length", "3"], d);
    assert_eq!(coords("dense.geojson", 1).len(), 5);
    ok(&["swap-xy", "lines.geojson", "swapped.geojson"], d);
    assert_eq!(coords("swapped.geojson", 1), [(0.0, 20.0), (0.0, 30.0)]);
    ok(&["convex-hull", "lines.geojson", "hull.geojson"], d);
    assert_eq!(first(&d.join("hull.geojson")).features[0].geometry.as_ref().unwrap().geom_type(), terrio::GeometryType::Polygon);
    ok(&["concave-hull", "lines.geojson", "concave.geojson", "--ratio", "0.5"], d);
    ok(&["set-geom-type", "lines.geojson", "multi.gpkg", "--multi"], d);
    let multi = first(&d.join("multi.gpkg"));
    assert_eq!(multi.geom_type, Some(terrio::GeometryType::MultiLineString));
    ok(&["set-geom-type", "lines.geojson", "points.geojson", "--geometry-type", "POINT", "--skip"], d);
    assert_eq!(first(&d.join("points.geojson")).len(), 1, "only the null geometry is kept");
    ok(&["combine", "lines.geojson", "combined.gpkg", "--group-by", "k", "--add-extra-fields", "sometimes-identical"], d);
    let combined = first(&d.join("combined.gpkg"));
    assert_eq!(combined.len(), 2);
    assert_eq!(combined.schema.fields().iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["k", "v"]);
    ok(&["sort", "lines.geojson", "sorted.geojson", "--method", "strtree"], d);
    assert!(first(&d.join("sorted.geojson")).features[2].geometry.is_none());
    ok(&["convert", "lines.geojson", "converted.geojson", "--simplify", "1"], d);
    assert_eq!(coords("converted.geojson", 0), [(0.0, 0.0), (10.0, 0.0)]);
    // Append goes through the whole-layer path.
    ok(&["simplify", "lines.geojson", "simple.gpkg", "--tolerance", "1", "--append"], d);
    assert_eq!(first(&d.join("simple.gpkg")).len(), 6);
    for bad in [
        &["simplify", "lines.geojson", "x.geojson", "--tolerance", "-1"][..],
        &["concave-hull", "lines.geojson", "x.geojson", "--ratio", "2"],
        &["set-geom-type", "lines.geojson", "x.geojson", "--curve"],
        &["set-geom-type", "lines.geojson", "x.geojson", "--geometry-type", "POINT", "--multi"],
        &["combine", "lines.geojson", "x.geojson", "--group-by", "missing"],
        &["convert", "lines.geojson", "x.geojson", "--simplify", "1", "--segmentize", "1"],
    ] {
        assert!(!run(bad, d).status.success(), "{bad:?} should fail");
    }
}

#[test]
fn standalone_simplify_propagates_errors_on_streamed_and_whole_layer_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    std::fs::write(d.join("singleton.geojson"), r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"LineString","coordinates":[[1,2]]}}]}"#).unwrap();
    for extra in [None, Some("--overwrite")] {
        let mut args = vec!["simplify", "singleton.geojson", "simplified.geojson", "--tolerance", "1"];
        args.extend(extra);
        let output = run(&args, d);
        assert!(!output.status.success(), "{args:?}");
        assert!(String::from_utf8_lossy(&output.stderr).contains("at least two points"));
    }
    ok(&["convert", "singleton.geojson", "converted.geojson", "--simplify", "1"], d);
    assert_eq!(first(&d.join("converted.geojson")).features[0].geometry.as_ref().unwrap().all_coords().len(), 1);
}

#[test]
fn streamed_geometry_commands_refuse_one_name_for_multiple_layers() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("multiple.gpkg");
    let mut a = terrio::Layer::new("a");
    a.push(terrio::Feature { fid: 1, geometry: Some(terrio::Geometry::point(0., 0.)), attributes: vec![] });
    let mut b = a.clone();
    b.name = "b".into();
    terrio::write_vector(&[a, b], source.to_str().unwrap(), &Default::default()).unwrap();
    let output = run(&["simplify", "multiple.gpkg", "output.gpkg", "--tolerance", "1", "--nln", "same"], dir.path());
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("single input layer"));
    assert!(!dir.path().join("output.gpkg").exists());
}

#[test]
fn vector_grid_accepts_negative_search_axes_and_refuses_large_grids() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    std::fs::write(
        d.join("points.geojson"),
        r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[0,0,1]}}]}"#,
    )
    .unwrap();
    ok(
        &[
            "vector-grid",
            "nearest",
            "points.geojson",
            "grid.tif",
            "--radius1",
            "-1",
            "--radius2",
            "-1",
            "--extent",
            "-1",
            "-1",
            "1",
            "1",
            "--size",
            "2",
            "2",
        ],
        d,
    );
    let missing = run(&["vector-grid", "nearest", "points.geojson", "partial.tif", "-l", "points", "-l", "typo"], d);
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("typo"));
    assert!(!d.join("partial.tif").exists());
    let result = run(&["vector-grid", "nearest", "points.geojson", "large.tif", "--size", "100000", "100000"], d);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("too large"));
    assert!(!d.join("large.tif").exists());
}

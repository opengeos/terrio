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

#[test]
fn zonal_results_do_not_depend_on_thread_count() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    for (i, name) in ["a.tif", "b.tif", "c.tif"].iter().enumerate() {
        let cfg =
            terrio::RasterConfig { cols: 300, rows: 300, bands: 2, data_type: terrio::DataType::F32, cell_size: 1., ..Default::default() };
        let values = (0..2 * 300 * 300).map(|v| ((v * (i + 3) * 7919) % 1013) as f64 / 7.).collect();
        let mut r = terrio::RasterDataset::new(terrio::Raster::from_data(cfg, values).unwrap(), false);
        r.raster.crs.epsg = Some(4326);
        let options = terrio::RasterWriteOptions { tile_size: Some(64), ..Default::default() };
        terrio::write_raster_dataset(&r, d.join(name).to_str().unwrap(), &options).unwrap();
    }
    let mut features = Vec::new();
    for k in 0..40 {
        let (x, y) = ((k * 37 % 250) as f64 + 0.3, (k * 53 % 250) as f64 + 0.6);
        let (w, h) = (5. + (k % 7) as f64 * 6.1, 4. + (k % 5) as f64 * 9.7);
        features.push(format!(
            r#"{{"type":"Feature","properties":{{"k":{k}}},"geometry":{{"type":"Polygon","coordinates":[[[{x},{y}],[{},{y}],[{},{}],[{x},{}],[{x},{y}]]]}}}}"#,
            x + w,
            x + w,
            y + h,
            y + h
        ));
    }
    std::fs::write(d.join("zones.geojson"), format!(r#"{{"type":"FeatureCollection","features":[{}]}}"#, features.join(","))).unwrap();
    let stats = "count,min,max,mean,sum,stddev,variance,cv,median,quantile_25,mode,minority,variety";
    for fractional in [false, true] {
        let mut outputs = Vec::new();
        for threads in ["1", "4"] {
            let out = format!("out_{threads}_{fractional}.geojson");
            let mut args = vec!["zonal-stats", "zones.geojson", &out, "--raster", "a.tif", "--raster", "b.tif", "--raster", "c.tif"];
            args.extend(["--band", "1,2", "--stats", stats]);
            if fractional {
                args.push("--fractional");
            }
            let r =
                Command::new(env!("CARGO_BIN_EXE_terrio")).args(&args).env("RAYON_NUM_THREADS", threads).current_dir(d).output().unwrap();
            assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
            outputs.push(std::fs::read(d.join(&out)).unwrap());
        }
        assert_eq!(outputs[0], outputs[1]);
        let out = layer(&d.join(format!("out_1_{fractional}.geojson")));
        let filled = out.features.iter().filter(|f| f.get(&out.schema, "c_b2_quantile_25").is_ok_and(|v| !v.is_null())).count();
        assert!(filled >= 30, "{filled} zones with statistics");
    }
}

/// JPEG XL tiles decode on terrio's pool (`par::install`) from the threads that read strips ahead.
/// With at least as many sources as pool threads, sources reduced on the pool itself left no
/// thread to decode on, and the command never finished.
#[test]
fn zonal_jxl_sources_finish_with_more_sources_than_threads() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let cfg = terrio::RasterConfig { cols: 512, rows: 512, data_type: terrio::DataType::U8, cell_size: 1., ..Default::default() };
    let mut r =
        terrio::RasterDataset::new(terrio::Raster::from_data(cfg, (0..512 * 512).map(|v| (v % 251) as f64).collect()).unwrap(), false);
    r.raster.crs.epsg = Some(3857);
    let options = terrio::RasterWriteOptions { compression: Some("JXL".into()), tile_size: Some(128), ..Default::default() };
    let mut args = vec!["zonal-stats".to_string(), "zones.geojson".into(), "out.geojson".into(), "--stats".into(), "count".into()];
    for i in 0..4 {
        terrio::write_raster_dataset(&r, d.join(format!("s{i}.tif")).to_str().unwrap(), &options).unwrap();
        args.extend(["--raster".into(), format!("s{i}.tif")]);
    }
    std::fs::write(d.join("zones.geojson"),r#"{"type":"FeatureCollection","crs":{"type":"name","properties":{"name":"EPSG:3857"}},"features":[{"type":"Feature","properties":{},"geometry":{"type":"Polygon","coordinates":[[[0,0],[512,0],[512,512],[0,512],[0,0]]]}}]}"#).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_terrio")).args(&args).env("RAYON_NUM_THREADS", "2").current_dir(d).spawn().unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            panic!("zonal-stats over JPEG XL sources did not finish within 120 s");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };
    assert!(status.success());
    let out = layer(&d.join("out.geojson"));
    assert_eq!(out.features[0].get(&out.schema, "s3_b1_count").unwrap(), &terrio::FieldValue::Integer(512 * 512));
}

/// Two dated single-band `B04.tif` rasters (as Sentinel-2 assets are named) and one zone.
fn dated_rasters(d: &Path) {
    for (date, offset) in [("2024-07-01", 0.), ("2024-07-11", 100.)] {
        std::fs::create_dir_all(d.join(date)).unwrap();
        let cfg = terrio::RasterConfig { cols: 4, rows: 4, data_type: terrio::DataType::F64, cell_size: 1., ..Default::default() };
        let mut r =
            terrio::RasterDataset::new(terrio::Raster::from_data(cfg, (1..=16).map(|v| v as f64 + offset).collect()).unwrap(), false);
        r.raster.crs.epsg = Some(4326);
        terrio::write_raster_dataset(&r, d.join(date).join("B04.tif").to_str().unwrap(), &Default::default()).unwrap();
    }
    std::fs::write(d.join("zones.geojson"),r#"{"type":"FeatureCollection","name":"zones","features":[{"type":"Feature","properties":{"name":"all"},"geometry":{"type":"Polygon","coordinates":[[[0,0],[4,0],[4,4],[0,4],[0,0]]]}}]}"#).unwrap();
}

#[test]
fn zonal_inputs_csv_labels_dates_and_bands() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    dated_rasters(d);
    std::fs::write(d.join("inputs.csv"), "path,time,label\n2024-07-01/B04.tif,2024-07-01,B04\n\"2024-07-11/B04.tif\",2024-07-11,B04\n")
        .unwrap();
    ok(&["zonal-stats", "zones.geojson", "labelled.gpkg", "--inputs", "inputs.csv", "--stats", "mean"], d);
    let out = layer(&d.join("labelled.gpkg"));
    assert_eq!(out.features[0].get(&out.schema, "2024_07_01_B04_mean").unwrap(), &terrio::FieldValue::Float(8.5));
    assert_eq!(out.features[0].get(&out.schema, "2024_07_11_B04_mean").unwrap(), &terrio::FieldValue::Float(108.5));
    // Unlabelled rows keep FILE_STEM_bN names; `band` selects a band number.
    std::fs::write(d.join("plain.csv"), "path,band\n2024-07-01/B04.tif,1\n").unwrap();
    ok(&["zonal-stats", "zones.geojson", "plain.gpkg", "--inputs", "plain.csv", "--stats", "mean"], d);
    let out = layer(&d.join("plain.gpkg"));
    assert_eq!(out.features[0].get(&out.schema, "B04_b1_mean").unwrap(), &terrio::FieldValue::Float(8.5));
    for (bad, why) in [
        ("time,label\nx,y\n", "path"),
        ("path,colour\na.tif,red\n", "colour"),
        ("path,band\n2024-07-01/B04.tif,0\n", "band"),
        ("path,label,path\na.tif,B04,b.tif\n", "appears twice"),
        ("path,label,label\na.tif,B04,B05\n", "appears twice"),
        ("path,PATH\na.tif,b.tif\n", "appears twice"),
    ] {
        std::fs::write(d.join("bad.csv"), bad).unwrap();
        let r = run(&["zonal-stats", "zones.geojson", "bad.gpkg", "--inputs", "bad.csv"], d);
        assert!(!r.status.success());
        assert!(String::from_utf8_lossy(&r.stderr).contains(why), "{}", String::from_utf8_lossy(&r.stderr));
    }
    // --prefix names --raster fields, so a table-only run says it needs --raster.
    let r = run(&["zonal-stats", "zones.geojson", "prefix.gpkg", "--inputs", "plain.csv", "--prefix", "a"], d);
    assert!(!r.status.success());
    assert!(String::from_utf8_lossy(&r.stderr).contains("give it with --raster"), "{}", String::from_utf8_lossy(&r.stderr));
}

#[test]
fn zonal_raster_file_list() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    dated_rasters(d);
    std::fs::write(d.join("list.txt"), "2024-07-01/B04.tif\n\n2024-07-11/B04.tif\n").unwrap();
    ok(&["zonal-stats", "zones.geojson", "listed.geojson", "--raster", "@list.txt", "--prefix", "a", "--prefix", "b"], d);
    ok(
        &[
            "zonal-stats",
            "zones.geojson",
            "flags.geojson",
            "--raster",
            "2024-07-01/B04.tif",
            "--raster",
            "2024-07-11/B04.tif",
            "--prefix",
            "a",
            "--prefix",
            "b",
        ],
        d,
    );
    assert_eq!(std::fs::read(d.join("listed.geojson")).unwrap(), std::fs::read(d.join("flags.geojson")).unwrap());
}

#[test]
fn zonal_too_many_columns_for_geopackage_says_so() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    dated_rasters(d);
    let mut csv = String::from("path,time,label\n");
    for i in 0..400 {
        csv.push_str(&format!("2024-07-01/B04.tif,t{i},B04\n"));
    }
    std::fs::write(d.join("many.csv"), csv).unwrap();
    let r = run(&["zonal-stats", "zones.geojson", "many.gpkg", "--inputs", "many.csv"], d);
    assert!(!r.status.success());
    let err = String::from_utf8_lossy(&r.stderr);
    assert!(err.contains("2400 columns") && err.contains("GeoPackage allows 2000"), "{err}");
    ok(&["zonal-stats", "zones.geojson", "many.fgb", "--inputs", "many.csv"], d);
    assert_eq!(layer(&d.join("many.fgb")).schema.len(), 1 + 2400);
}

#[test]
fn zonal_inputs_tables_are_refused_in_mcp_tool_calls() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    dated_rasters(d);
    std::fs::write(d.join("inputs.csv"), "path\n2024-07-01/B04.tif\n").unwrap();
    for args in [&["--inputs", "inputs.csv"][..], &["--raster", "@inputs.csv"][..]] {
        let r = Command::new(env!("CARGO_BIN_EXE_terrio"))
            .args(["zonal-stats", "zones.geojson", "out.geojson"])
            .args(args)
            .env("TERRIO_MCP_TOOL", "1")
            .current_dir(d)
            .output()
            .unwrap();
        assert!(!r.status.success());
        assert!(String::from_utf8_lossy(&r.stderr).contains("not read in MCP tool calls"), "{}", String::from_utf8_lossy(&r.stderr));
    }
}

/// Rasters listed by path are opened only while they are read: more of them than the open-file
/// limit allows at once still run.
#[cfg(unix)]
#[test]
fn zonal_reads_more_rasters_than_the_open_file_limit() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    dated_rasters(d);
    let list: String = (0..200).map(|i| format!("r{i}.tif\n")).collect();
    for i in 0..200 {
        std::os::unix::fs::symlink(d.join("2024-07-01/B04.tif"), d.join(format!("r{i}.tif"))).unwrap();
    }
    std::fs::write(d.join("list.txt"), list).unwrap();
    // The binary is the script's $0, not text in it; few threads keep their open rasters under the limit.
    let r = Command::new("sh")
        .args(["-c", r#"ulimit -n 64 && exec "$0" zonal-stats zones.geojson out.fgb --raster @list.txt --stats mean"#])
        .arg(env!("CARGO_BIN_EXE_terrio"))
        .env("RAYON_NUM_THREADS", "4")
        .current_dir(d)
        .output()
        .unwrap();
    assert!(r.status.success(), "{}", String::from_utf8_lossy(&r.stderr));
    let out = layer(&d.join("out.fgb"));
    assert_eq!(out.features[0].get(&out.schema, "r199_b1_mean").unwrap(), &terrio::FieldValue::Float(8.5));
}

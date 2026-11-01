use super::*;
use crate::ops::vector_filter::VectorTranslateOptions;
use wbvector::{Coord, FieldDef, FieldType, FieldValue, Geometry};

/// A layer with every field type, null values and geometries, empty geometries, Z and
/// integers beyond 32 bits.
fn sample(n: usize) -> Layer {
    let mut l = Layer::new("sample").with_epsg(4326);
    l.add_field(FieldDef::new("name", FieldType::Text));
    l.add_field(FieldDef::new("i", FieldType::Integer));
    l.add_field(FieldDef::new("x", FieldType::Float));
    l.add_field(FieldDef::new("d", FieldType::Date));
    for k in 0..n {
        let x = -100.0 + k as f64 * 0.37;
        let g = match k % 4 {
            0 => Some(Geometry::point(x, 40.0 + (k % 7) as f64)),
            1 => Some(Geometry::point(x, -10.5)),
            2 => None,
            _ => Some(Geometry::point(x + 0.25, 12.0)),
        };
        let i = if k == 7 { FieldValue::Integer(5_000_000_000) } else { FieldValue::Integer(k as i64 * 3 - 20) };
        let name = if k % 5 == 0 { FieldValue::Null } else { FieldValue::Text(format!("feature {k} é \"q\", x")) };
        l.push(Feature {
            fid: k as u64,
            geometry: g,
            attributes: vec![name, i, FieldValue::Float(k as f64 / 3.0), FieldValue::Date(format!("2024-01-{:02}", k % 28 + 1))],
        });
    }
    l
}

fn polygons(n: usize) -> Layer {
    let mut l = Layer::new("polys").with_epsg(32633);
    l.add_field(FieldDef::new("v", FieldType::Integer));
    for k in 0..n {
        let x = 500000.0 + 10.0 * k as f64;
        let ring = vec![Coord::xyz(x, 0.0, 1.0), Coord::xyz(x + 5.0, 0.0, 2.0), Coord::xyz(x + 5.0, 5.0, 3.0), Coord::xyz(x, 5.0, 4.0)];
        l.push(Feature {
            fid: k as u64 + 1,
            geometry: Some(Geometry::polygon(ring, vec![])),
            attributes: vec![FieldValue::Integer(k as i64)],
        });
    }
    l
}

fn same_features(a: &Layer, b: &Layer) {
    same_content(a, b);
    assert_eq!(a.name, b.name);
}

/// The same fields, CRS and features (layers read from files of different names).
fn same_content(a: &Layer, b: &Layer) {
    assert_eq!(
        a.schema.fields().iter().map(|f| (&f.name, f.field_type)).collect::<Vec<_>>(),
        b.schema.fields().iter().map(|f| (&f.name, f.field_type)).collect::<Vec<_>>()
    );
    assert_eq!(a.crs_epsg(), b.crs_epsg());
    assert_eq!(a.geom_type, b.geom_type);
    assert_eq!(a.features.len(), b.features.len());
    for (x, y) in a.features.iter().zip(&b.features) {
        assert_eq!(x.fid, y.fid);
        assert_eq!(x.attributes, y.attributes);
        assert_eq!(x.geometry, y.geometry);
    }
}

const FORMATS: &[(&str, VectorDriver)] = &[
    ("geojson", VectorDriver::GeoJSON),
    ("geojsonl", VectorDriver::GeoJSONSeq),
    ("csv", VectorDriver::CSV),
    ("shp", VectorDriver::Shapefile),
    ("fgb", VectorDriver::FlatGeobuf),
    ("parquet", VectorDriver::Parquet),
    ("gpkg", VectorDriver::GPKG),
];

#[test]
fn streamed_reads_match_whole_reads() {
    let dir = tempfile::tempdir().unwrap();
    for layer in [sample(53), polygons(41)] {
        for (ext, _) in FORMATS {
            let path = dir.path().join(format!("{}.{ext}", layer.name)).to_string_lossy().into_owned();
            crate::write_vector(std::slice::from_ref(&layer), &path, &Default::default()).unwrap();
            let whole = crate::open_vector(&path, &Default::default()).unwrap().layers.remove(0);
            let mut readers = open_layers(&path, &Default::default(), 4).unwrap();
            assert_eq!(readers.len(), 1, "{ext}");
            let mut streamed = schema_of(readers[0].layer());
            while let Some(c) = readers[0].read_chunk(7).unwrap() {
                assert!(c.features.len() <= 7);
                streamed.features.extend(c.features);
            }
            same_features(&whole, &streamed);
            // Feature by feature.
            let r = open_layers(&path, &Default::default(), 5).unwrap().remove(0);
            let fids: Vec<u64> = r.map(|f| f.unwrap().fid).collect();
            assert_eq!(fids, whole.features.iter().map(|f| f.fid).collect::<Vec<_>>(), "{ext}");
        }
    }
}

#[test]
fn spatial_filters_apply_to_streams() {
    let dir = tempfile::tempdir().unwrap();
    let layer = sample(200);
    for ext in ["gpkg", "parquet"] {
        let path = dir.path().join(format!("s.{ext}")).to_string_lossy().into_owned();
        crate::write_vector(std::slice::from_ref(&layer), &path, &Default::default()).unwrap();
        let options = VectorOpenOptions { spatial_filter: Some((-90.0, 0.0, -50.0, 50.0)), ..Default::default() };
        let whole = crate::open_vector(&path, &options).unwrap().layers.remove(0);
        let streamed = open_layers(&path, &options, 9).unwrap().remove(0).read_all().unwrap();
        assert!(!whole.features.is_empty() && whole.features.len() < 150, "{ext}: {}", whole.features.len());
        same_features(&whole, &streamed);
    }
}

#[test]
fn streamed_writes_match_whole_writes() {
    let dir = tempfile::tempdir().unwrap();
    for layer in [sample(53), polygons(41)] {
        for (ext, driver) in FORMATS {
            if !can_write_stream(*driver) {
                continue;
            }
            let whole = dir.path().join(format!("whole_{}.{ext}", layer.name)).to_string_lossy().into_owned();
            let streamed = dir.path().join(format!("stream_{}.{ext}", layer.name)).to_string_lossy().into_owned();
            crate::write_vector(std::slice::from_ref(&layer), &whole, &Default::default()).unwrap();
            let profile = needs_profile(*driver, &layer).then(|| LayerProfile::of(&layer));
            let mut w = FeatureWriter::create(&streamed, &layer, profile.as_ref(), &Default::default()).unwrap();
            for c in layer.features.chunks(6) {
                let mut chunk = schema_of(&layer);
                chunk.features = c.to_vec();
                w.write(&chunk).unwrap();
            }
            w.finish().unwrap();
            // GeoParquet row groups and GeoPackage pages are laid out differently when streamed.
            if matches!(driver, VectorDriver::Parquet | VectorDriver::GPKG) {
                let a = crate::open_vector(&whole, &Default::default()).unwrap().layers.remove(0);
                let b = crate::open_vector(&streamed, &Default::default()).unwrap().layers.remove(0);
                same_content(&a, &b);
                continue;
            }
            let exts: &[&str] = if *driver == VectorDriver::Shapefile { &["shp", "shx", "dbf", "prj", "cpg"] } else { &[ext] };
            for e in exts {
                let a = std::fs::read(std::path::Path::new(&whole).with_extension(e)).unwrap();
                let b = std::fs::read(std::path::Path::new(&streamed).with_extension(e)).unwrap();
                assert!(a == b, "{} .{e} differs", layer.name);
            }
        }
        assert!(!dir.path().read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("terrio-features")));
    }
}

/// The `geo` metadata of a GeoParquet file.
fn parquet_geo(path: &str) -> serde_json::Value {
    use parquet::file::reader::FileReader;
    let reader = parquet::file::reader::SerializedFileReader::new(File::open(path).unwrap()).unwrap();
    let kv = reader.metadata().file_metadata().key_value_metadata().unwrap();
    let geo = kv.iter().find(|k| k.key == "geo").and_then(|k| k.value.as_deref()).unwrap();
    serde_json::from_str(geo).unwrap()
}

/// Layers without integer or date fields stream to GeoParquet without a profile, their geometry
/// types and extent gathered as they are written.
#[test]
fn geoparquet_streams_text_fields_without_a_profile() {
    let dir = tempfile::tempdir().unwrap();
    let mut layer = Layer::new("t").with_epsg(4326);
    layer.add_field(FieldDef::new("name", FieldType::Text));
    for k in 0..40 {
        let x = k as f64 - 10.0;
        let geometry = match k % 5 {
            0 => None,
            1 => Some(Geometry::point(x, 2.0)),
            2 => Some(Geometry::multi_point(vec![Coord::xy(x, -3.0), Coord::xy(x + 1.0, 4.5)])),
            3 if k > 20 => Some(Geometry::point_z(x, 1.0, 9.0)),
            _ => Some(Geometry::point(x, 0.0)),
        };
        layer.push(Feature { fid: k, geometry, attributes: vec![FieldValue::Text(format!("f{k}"))] });
    }
    assert!(!needs_profile(VectorDriver::Parquet, &layer));
    assert!(needs_profile(VectorDriver::Parquet, &sample(1)));
    let whole = dir.path().join("whole.parquet").to_string_lossy().into_owned();
    let streamed = dir.path().join("stream.parquet").to_string_lossy().into_owned();
    crate::write_vector(std::slice::from_ref(&layer), &whole, &Default::default()).unwrap();
    let mut w = FeatureWriter::create(&streamed, &layer, None, &Default::default()).unwrap();
    for c in layer.features.chunks(6) {
        let mut chunk = schema_of(&layer);
        chunk.features = c.to_vec();
        w.write(&chunk).unwrap();
    }
    w.finish().unwrap();
    let geo = parquet_geo(&streamed);
    assert_eq!(geo, parquet_geo(&whole));
    let col = &geo["columns"]["geometry"];
    assert_eq!(col["geometry_types"], serde_json::json!(["Point", "MultiPoint", "Point Z"]));
    assert_eq!(col["bbox"], serde_json::json!([-9.0, -3.0, 29.0, 4.5]));
    let a = crate::open_vector(&whole, &Default::default()).unwrap().layers.remove(0);
    let b = crate::open_vector(&streamed, &Default::default()).unwrap().layers.remove(0);
    same_content(&a, &b);
}

#[test]
fn converts_as_streams_like_whole_conversions() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src.gpkg").to_string_lossy().into_owned();
    crate::write_vector(&[sample(300)], &src, &Default::default()).unwrap();
    let translate = VectorTranslateOptions {
        where_clause: Some("i > 10".into()),
        limit: Some(150),
        select: Some(vec!["i".into(), "name".into()]),
        dst_srs: Some(crate::crs::SpatialRef::from_epsg(3857).unwrap()),
        ..Default::default()
    };
    for ext in ["shp", "fgb", "geojsonl", "csv", "parquet", "geojson", "gpkg"] {
        let streamed = dir.path().join(format!("s.{ext}")).to_string_lossy().into_owned();
        let whole = dir.path().join(format!("w.{ext}")).to_string_lossy().into_owned();
        let options = ConvertOptions { translate: translate.clone(), chunk_size: 16, ..Default::default() };
        assert!(can_convert_stream(&src, &streamed, &options), "{ext}");
        convert(&src, &streamed, &options).unwrap();
        let layers =
            crate::ops::vector_filter::translate(&crate::open_vector(&src, &Default::default()).unwrap().layers, &translate).unwrap();
        crate::write_vector(&layers, &whole, &Default::default()).unwrap();
        let a = crate::open_vector(&whole, &Default::default()).unwrap().layers.remove(0);
        let b = crate::open_vector(&streamed, &Default::default()).unwrap().layers.remove(0);
        assert_eq!(a.features.len(), 150, "{ext}");
        same_content(&a, &b);
    }
}

/// Shapefiles read as streams (a batch of records read, then decoded in parallel) are those read
/// whole: past a batch, and with fewer shapes than records or fewer records than shapes.
#[test]
fn shapefiles_streamed_match_whole() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str, ext: &str| dir.path().join(format!("{name}.{ext}"));
    crate::write_vector(&[sample(9000)], &path("all", "shp").to_string_lossy(), &Default::default()).unwrap();
    // The same files with the .shp, then the .dbf, cut short: points are 28-byte records and null
    // shapes 12, and the first 8500 features hold 6375 points and 2125 null shapes.
    let dbf = std::fs::read(path("all", "dbf")).unwrap();
    let (header_len, record_len) = (u16::from_le_bytes([dbf[8], dbf[9]]) as usize, u16::from_le_bytes([dbf[10], dbf[11]]) as usize);
    for (name, shp_len, dbf_len) in [
        ("all", None, None),
        ("few_shapes", Some(100 + 28 * 6375 + 12 * 2125), None),
        ("few_records", None, Some(header_len + record_len * 8600)),
    ] {
        for ext in ["shp", "shx", "dbf", "prj", "cpg"] {
            let mut bytes = std::fs::read(path("all", ext)).unwrap();
            match ext {
                "shp" => bytes.truncate(shp_len.unwrap_or(bytes.len())),
                "dbf" => bytes.truncate(dbf_len.unwrap_or(bytes.len())),
                _ => {}
            }
            std::fs::write(path(name, ext), bytes).unwrap();
        }
        let p = path(name, "shp").to_string_lossy().into_owned();
        let whole = crate::open_vector(&p, &Default::default()).unwrap().layers.remove(0);
        let streamed = open_layers(&p, &Default::default(), DEFAULT_CHUNK).unwrap().remove(0).read_all().unwrap();
        assert_eq!(streamed.features.len(), 9000, "{name}");
        same_features(&whole, &streamed);
    }
}

#[test]
fn reads_geojson_members_after_the_features() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("late.geojson");
    std::fs::write(
        &path,
        r#"{"features":[{"type":"Feature","id":"a","properties":{"k":1},"geometry":{"type":"Point","coordinates":[500000,1]}},
        {"type":"Feature","id":7,"properties":{"k":2.5,"s":"x"},"geometry":null}],"type":"FeatureCollection","name":"late",
        "crs":{"type":"name","properties":{"name":"urn:ogc:def:crs:EPSG::32633"}}}"#,
    )
    .unwrap();
    let path = path.to_string_lossy().into_owned();
    let whole = crate::open_vector(&path, &Default::default()).unwrap().layers.remove(0);
    let streamed = open_layers(&path, &Default::default(), 1).unwrap().remove(0).read_all().unwrap();
    same_features(&whole, &streamed);
    assert_eq!(streamed.crs_epsg(), Some(32633));
    assert_eq!(streamed.name, "late");
}

#[test]
fn arrow_streams_match_whole_tables() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.fgb").to_string_lossy().into_owned();
    crate::write_vector(&[sample(77)], &path, &Default::default()).unwrap();
    let whole = crate::open_vector(&path, &Default::default()).unwrap().layers.remove(0);
    for encoding in [crate::arrow::GeometryEncoding::Wkb, crate::arrow::GeometryEncoding::GeoArrow] {
        let options = crate::arrow::ArrowWriteOptions { geometry_encoding: encoding, batch_size: 10, ..Default::default() };
        let table = crate::arrow::layer_to_arrow(&whole, &options).unwrap();
        let stream = ArrowStream::open(&path, None, &Default::default(), &options).unwrap();
        assert_eq!(stream.schema(), table.schema);
        let batches: Vec<_> = stream.map(|b| b.unwrap()).collect();
        assert_eq!(batches.len(), 8);
        assert_eq!(batches, table.batches);
    }
}

#[test]
fn converts_a_file_onto_itself_whole() {
    let dir = tempfile::tempdir().unwrap();
    let layer = sample(40);
    for ext in ["geojson", "shp", "fgb"] {
        let path = dir.path().join(format!("same.{ext}")).to_string_lossy().into_owned();
        crate::write_vector(std::slice::from_ref(&layer), &path, &Default::default()).unwrap();
        let options = ConvertOptions::default();
        assert!(!can_convert_stream(&path, &path, &options), "{ext}");
        let other = dir.path().join(format!("other.{ext}")).to_string_lossy().into_owned();
        assert!(can_convert_stream(&path, &other, &options), "{ext}");
        #[cfg(unix)]
        {
            let link = dir.path().join(format!("link.{ext}"));
            std::os::unix::fs::symlink(&path, &link).unwrap();
            assert!(!can_convert_stream(&path, &link.to_string_lossy(), &options), "{ext}");
        }
        convert(&path, &path, &options).unwrap();
        let read = crate::open_vector(&path, &Default::default()).unwrap().layers.remove(0);
        assert_eq!(read.features.len(), 40, "{ext}");
    }
}

#[test]
fn failed_conversions_leave_no_output() {
    let dir = tempfile::tempdir().unwrap();
    // A FlatGeobuf whose last feature is cut short: reading fails after features were written.
    let src = dir.path().join("cut.fgb").to_string_lossy().into_owned();
    crate::write_vector(&[sample(40)], &src, &Default::default()).unwrap();
    let bytes = std::fs::read(&src).unwrap();
    std::fs::write(&src, &bytes[..bytes.len() - 10]).unwrap();
    // Shapefiles profile the layer first, so they fail before the output is created.
    for ext in ["geojson", "geojsonl", "shp", "parquet"] {
        let dst = dir.path().join(format!("out.{ext}"));
        let options = ConvertOptions { chunk_size: 1, ..Default::default() };
        assert!(can_convert_stream(&src, &dst.to_string_lossy(), &options));
        assert!(convert(&src, &dst.to_string_lossy(), &options).is_err(), "{ext}");
        assert!(!dst.exists() && !dst.with_extension("dbf").exists(), "{ext}");
    }
}

#[test]
fn corrupt_geopackages_are_errors_not_panics() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.gpkg").to_string_lossy().into_owned();
    crate::write_vector(&[sample(300)], &path, &Default::default()).unwrap();
    let good = std::fs::read(&path).unwrap();
    let read = |bytes: &[u8]| {
        std::fs::write(&path, bytes).unwrap();
        open_layers(&path, &Default::default(), 50).and_then(|mut r| match r.pop() {
            Some(r) => r.read_all().map(|_| ()),
            None => Ok(()),
        })
    };
    // Invalid page sizes; reserved space leaving too few usable bytes.
    for (offset, value) in [(16usize, 0u8), (17, 7)] {
        let mut b = good.clone();
        b[offset] = value;
        assert!(read(&b).is_err(), "header byte {offset}");
    }
    let mut b = good.clone();
    b[16..18].copy_from_slice(&512u16.to_be_bytes());
    b[20] = 100;
    assert!(read(&b).is_err());
    // Huge cell counts, then a huge payload size in the first cell, on every page.
    for page in 0..good.len() / 4096 {
        let h = if page == 0 { 100 } else { 0 };
        let mut b = good.clone();
        b[page * 4096 + h + 3..page * 4096 + h + 5].copy_from_slice(&[0xff, 0xff]);
        let _ = read(&b);
        let mut b = good.clone();
        let ptr = page * 4096 + h + if b[page * 4096 + h] == 0x05 { 12 } else { 8 };
        let cell = page * 4096 + u16::from_be_bytes([b[ptr], b[ptr + 1]]) as usize;
        if b[page * 4096 + h] == 0x0d && cell + 9 <= b.len() {
            b[cell..cell + 9].copy_from_slice(&[0xff; 9]);
            let _ = read(&b);
        }
    }
}

#[test]
fn converts_several_layers_to_a_geopackage_like_whole_conversions() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("two.gpkg").to_string_lossy().into_owned();
    crate::write_vector(&[sample(120), polygons(77)], &src, &Default::default()).unwrap();
    let options = ConvertOptions { chunk_size: 9, ..Default::default() };
    let streamed = dir.path().join("s.gpkg").to_string_lossy().into_owned();
    let whole = dir.path().join("w.gpkg").to_string_lossy().into_owned();
    assert!(can_convert_stream(&src, &streamed, &options));
    convert(&src, &streamed, &options).unwrap();
    crate::write_vector(&crate::open_vector(&src, &Default::default()).unwrap().layers, &whole, &Default::default()).unwrap();
    let a = crate::open_vector(&whole, &Default::default()).unwrap().layers;
    let b = crate::open_vector(&streamed, &Default::default()).unwrap().layers;
    assert_eq!(a.len(), 2);
    assert_eq!(b.len(), a.len());
    for (x, y) in a.iter().zip(&b) {
        same_features(x, y);
    }
}

/// OpenStreetMap PBF files convert as streams like whole conversions (with `TERRIO_TEST_DATA`).
#[test]
fn converts_osm_like_whole_conversions() {
    let Some(dir) = std::env::var_os("TERRIO_TEST_DATA") else { return };
    let src = std::path::Path::new(&dir).join("liechtenstein.osm.pbf").to_string_lossy().into_owned();
    if !std::path::Path::new(&src).exists() {
        return;
    }
    let out = tempfile::tempdir().unwrap();
    let whole = crate::open_vector(&src, &Default::default()).unwrap().layers;
    // All five layers into a GeoPackage.
    let gpkg = out.path().join("osm.gpkg").to_string_lossy().into_owned();
    let options = ConvertOptions { chunk_size: 100, ..Default::default() };
    assert!(can_convert_stream(&src, &gpkg, &options));
    convert(&src, &gpkg, &options).unwrap();
    let w = out.path().join("whole.gpkg").to_string_lossy().into_owned();
    crate::write_vector(&whole, &w, &Default::default()).unwrap();
    let a = crate::open_vector(&w, &Default::default()).unwrap().layers;
    let b = crate::open_vector(&gpkg, &Default::default()).unwrap().layers;
    assert_eq!(a.len(), 5);
    assert_eq!(b.len(), a.len());
    for (x, y) in a.iter().zip(&b) {
        same_features(x, y);
    }
    // One layer, filtered and limited, to a FlatGeobuf (profiled first) and to GeoJSON.
    let translate = VectorTranslateOptions { where_clause: Some("highway IS NOT NULL".into()), limit: Some(500), ..Default::default() };
    let open = VectorOpenOptions { layers: vec!["lines".into()], ..Default::default() };
    let lines = crate::ops::vector_filter::translate(&whole[1..2], &translate).unwrap();
    for ext in ["fgb", "geojson"] {
        let path = out.path().join(format!("lines.{ext}")).to_string_lossy().into_owned();
        let options = ConvertOptions { open: open.clone(), translate: translate.clone(), chunk_size: 64, ..Default::default() };
        convert(&src, &path, &options).unwrap();
        let w = out.path().join(format!("whole_lines.{ext}")).to_string_lossy().into_owned();
        crate::write_vector(&lines, &w, &Default::default()).unwrap();
        assert_eq!(std::fs::read(&w).unwrap(), std::fs::read(&path).unwrap(), "{ext}");
    }
    // Several layers into a format holding one.
    let all = ConvertOptions::default();
    assert!(convert(&src, &out.path().join("all.fgb").to_string_lossy(), &all).is_err());
}

/// A vector tile named `.pbf` is not taken for an OpenStreetMap file: it converts whole.
#[test]
fn converts_vector_tiles_named_pbf_whole() {
    use crate::tiles::mvt;
    let dir = tempfile::tempdir().unwrap();
    let f = mvt::MvtFeature {
        id: Some(3),
        geom_type: mvt::POINT,
        geometry: mvt::encode_points(&[(100, 200)]),
        tags: vec![("name".into(), mvt::MvtValue::String("a".into()))],
        size: 0.0,
    };
    let tile = mvt::encode(&[mvt::MvtLayer { name: "l".into(), extent: 4096, features: vec![f] }]);
    let src = dir.path().join("0.pbf").to_string_lossy().into_owned();
    std::fs::write(&src, tile).unwrap();
    let dst = dir.path().join("out.geojson").to_string_lossy().into_owned();
    let whole = dir.path().join("whole.geojson").to_string_lossy().into_owned();
    convert(&src, &dst, &ConvertOptions::default()).unwrap();
    crate::write_vector(&crate::open_vector(&src, &Default::default()).unwrap().layers, &whole, &Default::default()).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), std::fs::read(&whole).unwrap());
}

/// FlatGeobuf streams without a profile: when a later chunk widens the types (an integer beyond
/// 32 bits, Z, a second geometry type, the first geometry after features without), the features
/// already spooled are re-encoded, and the file is the one written whole.
#[test]
fn flatgeobuf_streams_without_a_profile_when_types_widen() {
    let dir = tempfile::tempdir().unwrap();
    assert!(!needs_profile(VectorDriver::FlatGeobuf, &sample(1)));
    let mut layer = Layer::new("t").with_epsg(4326);
    layer.add_field(FieldDef::new("a", FieldType::Integer));
    layer.add_field(FieldDef::new("b", FieldType::Integer));
    layer.add_field(FieldDef::new("name", FieldType::Text));
    for k in 0..60i64 {
        let x = k as f64 - 10.0;
        let geometry = match k {
            0..=4 => None,
            _ if k % 7 == 0 => Some(Geometry::line_string(vec![Coord::xy(x, 0.0), Coord::xy(x + 1.0, 1.0)])),
            _ if k > 30 && k % 5 == 0 => Some(Geometry::point_z(x, 1.0, 9.0)),
            _ => Some(Geometry::point(x, 2.0)),
        };
        let a = if k == 45 { FieldValue::Integer(1 << 40) } else { FieldValue::Integer(k) };
        let b = if k == 20 { FieldValue::Integer(-(1 << 35)) } else { FieldValue::Null };
        layer.push(Feature { fid: k as u64, geometry, attributes: vec![a, b, FieldValue::Text(format!("f{k}"))] });
    }
    let whole = dir.path().join("whole.fgb").to_string_lossy().into_owned();
    crate::write_vector(std::slice::from_ref(&layer), &whole, &Default::default()).unwrap();
    for size in [1, 4, 6, 60] {
        let streamed = dir.path().join(format!("stream_{size}.fgb")).to_string_lossy().into_owned();
        let mut w = FeatureWriter::create(&streamed, &layer, None, &Default::default()).unwrap();
        for c in layer.features.chunks(size) {
            let mut chunk = schema_of(&layer);
            chunk.features = c.to_vec();
            w.write(&chunk).unwrap();
        }
        w.finish().unwrap();
        assert!(std::fs::read(&whole).unwrap() == std::fs::read(&streamed).unwrap(), "chunks of {size} differ");
    }
    assert!(!dir.path().read_dir().unwrap().any(|e| e.unwrap().file_name().to_string_lossy().contains("terrio-features")));
}

/// Re-encoded FlatGeobuf features keep what they had: rings closed with a different Z, rings of
/// two equal points, multipolygons, and float, date, date-time and blob values, when an integer
/// widens after them.
#[test]
fn flatgeobuf_reencodes_rings_and_values_as_they_were() {
    let dir = tempfile::tempdir().unwrap();
    let mut layer = Layer::new("t").with_epsg(32633);
    for (name, t) in [
        ("i", FieldType::Integer),
        ("f", FieldType::Float),
        ("d", FieldType::Date),
        ("dt", FieldType::DateTime),
        ("b", FieldType::Blob),
        ("ok", FieldType::Boolean),
    ] {
        layer.add_field(FieldDef::new(name, t));
    }
    for k in 0..30i64 {
        let x = 10.0 * k as f64;
        // Closed explicitly, with another Z on the closing vertex.
        let closed = vec![Coord::xyz(x, 0.0, 1.0), Coord::xyz(x + 5.0, 0.0, 2.0), Coord::xyz(x + 5.0, 5.0, 3.0), Coord::xyz(x, 0.0, 9.0)];
        let degenerate = vec![Coord::xyz(x, 1.0, 1.0), Coord::xyz(x, 1.0, 1.0)];
        let geometry = match k % 3 {
            0 => Geometry::polygon(closed, vec![degenerate]),
            1 => Geometry::multi_polygon(vec![(closed.clone(), vec![]), (degenerate, vec![])]),
            _ => Geometry::polygon(closed[..3].to_vec(), vec![]),
        };
        let i = if k == 25 { FieldValue::Integer(1 << 40) } else { FieldValue::Integer(k) };
        let attributes = vec![
            i,
            FieldValue::Float(k as f64 / 3.0),
            FieldValue::Date(format!("2024-01-{:02}", k % 28 + 1)),
            FieldValue::DateTime(format!("2024-01-01T00:00:{:02}Z", k % 60)),
            FieldValue::Blob(vec![k as u8, 0, 255]),
            FieldValue::Boolean(k % 2 == 0),
        ];
        layer.push(Feature { fid: k as u64, geometry: Some(geometry), attributes });
    }
    let whole = dir.path().join("whole.fgb").to_string_lossy().into_owned();
    crate::write_vector(std::slice::from_ref(&layer), &whole, &Default::default()).unwrap();
    let streamed = dir.path().join("stream.fgb").to_string_lossy().into_owned();
    let mut w = FeatureWriter::create(&streamed, &layer, None, &Default::default()).unwrap();
    for c in layer.features.chunks(4) {
        let mut chunk = schema_of(&layer);
        chunk.features = c.to_vec();
        w.write(&chunk).unwrap();
    }
    w.finish().unwrap();
    assert!(std::fs::read(&whole).unwrap() == std::fs::read(&streamed).unwrap());
}

/// Chunks of large geometries stop at about `CHUNK_BYTES`, short of the number of features
/// asked for, and keep every feature in order.
#[test]
fn chunks_stop_at_their_size() {
    let mut layer = Layer::new("big").with_epsg(4326);
    layer.add_field(FieldDef::new("k", FieldType::Integer));
    // A little over a third of a chunk each, whatever the size of a coordinate.
    let n = CHUNK_BYTES / 3 / std::mem::size_of::<Coord>() + 1;
    let line: Vec<Coord> = (0..n).map(|i| Coord::xy(i as f64, 0.0)).collect();
    for k in 0..6 {
        layer.push(Feature {
            fid: k,
            geometry: Some(Geometry::line_string(line.clone())),
            attributes: vec![FieldValue::Integer(k as i64)],
        });
    }
    let source = MemorySource { features: std::mem::take(&mut layer.features).into_iter(), batch: 4 };
    let mut reader = FeatureReader::new(layer, Box::new(source));
    let mut sizes = Vec::new();
    let mut fids = Vec::new();
    while let Some(chunk) = reader.read_chunk(DEFAULT_CHUNK).unwrap() {
        sizes.push(chunk.features.len());
        fids.extend(chunk.features.iter().map(|f| f.fid));
    }
    assert_eq!(sizes, [3, 3]);
    assert_eq!(fids, (0..6).collect::<Vec<_>>());

    // Chunks of `AHEAD_CHUNK_BYTES` (a quarter of `CHUNK_BYTES`) hold one of them each.
    let mut layer = schema_of(&reader.layer);
    let features = (0..6).map(|k| Feature {
        fid: k,
        geometry: Some(Geometry::line_string(line.clone())),
        attributes: vec![FieldValue::Integer(k as i64)],
    });
    let source = MemorySource { features: features.collect::<Vec<_>>().into_iter(), batch: 4 };
    layer.features.clear();
    let mut reader = FeatureReader::new(layer, Box::new(source));
    reader.set_chunk_bytes(AHEAD_CHUNK_BYTES);
    let mut sizes = Vec::new();
    while let Some(chunk) = reader.read_chunk(DEFAULT_CHUNK).unwrap() {
        sizes.push(chunk.features.len());
    }
    assert_eq!(sizes, [1; 6]);
}

/// Chunks read ahead are written in order, each once; an error reading or writing stops the
/// writing and is returned, and a panic in either is propagated.
#[test]
fn chunks_written_ahead_are_written_in_order() {
    let chunks = |n: u64, fail: Option<u64>| {
        let mut k = 0;
        move || {
            k += 1;
            if Some(k) == fail {
                return Err(Error::InvalidData("read".into()));
            }
            let mut l = Layer::new("t");
            l.push(Feature { fid: k, geometry: None, attributes: Vec::new() });
            Ok((k <= n).then_some(l))
        }
    };
    let mut fids = Vec::new();
    write_ahead(chunks(20, None), |c| {
        fids.push(c.features[0].fid);
        Ok(())
    })
    .unwrap();
    assert_eq!(fids, (1..=20).collect::<Vec<_>>());

    let mut fids = Vec::new();
    let read_fails = write_ahead(chunks(20, Some(6)), |c| {
        fids.push(c.features[0].fid);
        Ok(())
    });
    assert!(read_fails.is_err());
    assert_eq!(fids, (1..6).collect::<Vec<_>>());
    let write = |c: Layer| if c.features[0].fid == 3 { Err(Error::InvalidData("write".into())) } else { Ok(()) };
    assert!(write_ahead(chunks(1000, None), write).is_err());

    let read = || -> Result<Option<Layer>> { panic!("read") };
    assert!(std::panic::catch_unwind(|| write_ahead(read, |_| Ok(()))).is_err());
    let write = |_: Layer| -> Result<()> { panic!("write") };
    assert!(std::panic::catch_unwind(|| write_ahead(chunks(1000, None), write)).is_err());
}

/// GeoParquet row groups (of 65,536 rows) are those of the whole-layer writer whatever the size of
/// the chunks.
#[test]
fn geoparquet_row_groups_do_not_follow_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let mut layer = Layer::new("t").with_epsg(4326);
    layer.add_field(FieldDef::new("name", FieldType::Text));
    for k in 0..65_546 {
        layer.push(Feature { fid: k, geometry: Some(Geometry::point(k as f64, 0.0)), attributes: vec![FieldValue::Text(format!("f{k}"))] });
    }
    let whole = dir.path().join("whole.parquet").to_string_lossy().into_owned();
    crate::write_vector(std::slice::from_ref(&layer), &whole, &Default::default()).unwrap();
    let streamed = dir.path().join("stream.parquet").to_string_lossy().into_owned();
    let mut w = FeatureWriter::create(&streamed, &layer, None, &Default::default()).unwrap();
    for c in layer.features.chunks(1000) {
        let mut chunk = schema_of(&layer);
        chunk.features = c.to_vec();
        w.write_owned(chunk).unwrap();
    }
    w.finish().unwrap();
    let groups = |p: &str| geoparquet::row_groups(File::open(p).unwrap()).unwrap().len();
    assert_eq!(groups(&streamed), 2);
    assert_eq!(groups(&streamed), groups(&whole));
    let a = crate::open_vector(&whole, &Default::default()).unwrap().layers.remove(0);
    let b = crate::open_vector(&streamed, &Default::default()).unwrap().layers.remove(0);
    same_content(&a, &b);
}

#[test]
fn geopackage_streams_register_optional_measures_across_chunks() {
    let directory = tempfile::tempdir().unwrap();
    let mut layer = Layer::new("mixed").with_epsg(4326);
    layer.features = vec![
        Feature {
            fid: 1,
            attributes: vec![],
            geometry: Some(Geometry::Point(wbvector::Coord { m: Some(3.), ..wbvector::Coord::xy(1., 2.) })),
        },
        Feature { fid: 2, attributes: vec![], geometry: Some(Geometry::point(4., 5.)) },
    ];
    let check = |path: &str| {
        let db = crate::drivers::sqlite::Db::new(std::fs::read(path).unwrap()).unwrap();
        let table = db.table("gpkg_geometry_columns").unwrap();
        assert_eq!(table.get(&table.rows[0], "m").unwrap().as_i64(), Some(2));
        let back = crate::open_vector(path, &Default::default()).unwrap();
        for (a, b) in layer.features.iter().zip(&back.layers[0].features) {
            assert_eq!(a.geometry, b.geometry);
        }
    };
    let single = directory.path().join("single.gpkg").to_string_lossy().into_owned();
    let mut writer = FeatureWriter::create(&single, &layer, Some(&LayerProfile::of(&layer)), &Default::default()).unwrap();
    writer.write(&layer).unwrap();
    writer.finish().unwrap();
    check(&single);
    let multi = directory.path().join("multi.gpkg").to_string_lossy().into_owned();
    let mut sink = GpkgLayersSink::create(&multi, &[schema_of(&layer)], false).unwrap().unwrap();
    for feature in &layer.features {
        let mut chunk = schema_of(&layer);
        chunk.features.push(feature.clone());
        sink.write(0, &chunk).unwrap();
    }
    sink.finish().unwrap();
    check(&multi);
}

#[test]
fn geopackage_streams_register_optional_heights_across_chunks() {
    let directory = tempfile::tempdir().unwrap();
    let mut layer = Layer::new("mixed").with_epsg(4326);
    layer.features = vec![
        Feature {
            fid: 1,
            attributes: vec![],
            geometry: Some(Geometry::Point(wbvector::Coord { z: Some(3.), ..wbvector::Coord::xy(1., 2.) })),
        },
        Feature { fid: 2, attributes: vec![], geometry: Some(Geometry::point(4., 5.)) },
    ];
    layer.geom_type = Some(wbvector::GeometryType::Point);
    let check = |path: &str| {
        let db = crate::drivers::sqlite::Db::new(std::fs::read(path).unwrap()).unwrap();
        let table = db.table("gpkg_geometry_columns").unwrap();
        assert_eq!(table.get(&table.rows[0], "z").unwrap().as_i64(), Some(2));
        let back = crate::open_vector(path, &Default::default()).unwrap();
        for (a, b) in layer.features.iter().zip(&back.layers[0].features) {
            assert_eq!(a.geometry, b.geometry);
        }
    };
    let single = directory.path().join("single.gpkg").to_string_lossy().into_owned();
    let mut writer = FeatureWriter::create(&single, &layer, Some(&LayerProfile::of(&layer)), &Default::default()).unwrap();
    writer.write(&layer).unwrap();
    writer.finish().unwrap();
    check(&single);
    let multi = directory.path().join("multi.gpkg").to_string_lossy().into_owned();
    let mut sink = GpkgLayersSink::create(&multi, &[schema_of(&layer)], false).unwrap().unwrap();
    for feature in &layer.features {
        let mut chunk = schema_of(&layer);
        chunk.features.push(feature.clone());
        sink.write(0, &chunk).unwrap();
    }
    sink.finish().unwrap();
    check(&multi);
}

#[test]
fn geopackage_streams_register_z_found_after_the_first_coordinate() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("late-z.gpkg").to_string_lossy().into_owned();
    let mut layer = Layer::new("late_z").with_epsg(4326);
    layer.features.push(Feature {
        fid: 1,
        attributes: vec![],
        geometry: Some(Geometry::LineString(vec![
            wbvector::Coord::xy(0., 0.),
            wbvector::Coord { z: Some(12.), ..wbvector::Coord::xy(1., 1.) },
        ])),
    });
    let mut writer = FeatureWriter::create(&path, &layer, Some(&LayerProfile::of(&layer)), &Default::default()).unwrap();
    writer.write(&layer).unwrap();
    writer.finish().unwrap();
    let db = crate::drivers::sqlite::Db::new(std::fs::read(&path).unwrap()).unwrap();
    let table = db.table("gpkg_geometry_columns").unwrap();
    assert_eq!(table.get(&table.rows[0], "z").unwrap().as_i64(), Some(1));
    let back = crate::open_vector(&path, &Default::default()).unwrap();
    let coordinates = back.layers[0].features[0].geometry.as_ref().unwrap().all_coords();
    assert_eq!((coordinates[0].z, coordinates[1].z), (Some(0.), Some(12.)));
}

#[test]
fn geopackage_streams_preserve_measures_and_register_them() {
    let dir = tempfile::tempdir().unwrap();
    let mut layer = polygons(3);
    for feature in &mut layer.features {
        crate::vector::map_coords(feature.geometry.as_mut().unwrap(), &mut |c| {
            c.m = Some(c.x + c.y);
            Ok(())
        })
        .unwrap();
    }
    for preserve in [false, true] {
        layer.geom_type = if preserve { None } else { Some(wbvector::GeometryType::Polygon) };
        let check = |path: &str| {
            let back = crate::open_vector(path, &Default::default()).unwrap();
            same_content(&layer, &back.layers[0]);
            let db = crate::drivers::sqlite::Db::new(std::fs::read(path).unwrap()).unwrap();
            let t = db.table("gpkg_geometry_columns").unwrap();
            assert_eq!(t.get(&t.rows[0], "m").unwrap().as_i64(), Some(1));
            assert_eq!(t.get(&t.rows[0], "z").unwrap().as_i64(), Some(if preserve { 2 } else { 1 }));
            assert_eq!(t.get(&t.rows[0], "geometry_type_name").unwrap().as_str(), Some(if preserve { "GEOMETRY" } else { "POLYGON" }));
        };
        let single = dir.path().join(format!("single-{preserve}.gpkg")).to_string_lossy().into_owned();
        let options = VectorWriteOptions { preserve_declared_geometry_type: preserve, ..Default::default() };
        let mut writer = FeatureWriter::create(&single, &layer, Some(&LayerProfile::of(&layer)), &options).unwrap();
        writer.write(&layer).unwrap();
        writer.finish().unwrap();
        check(&single);
        let multi = dir.path().join(format!("multi-{preserve}.gpkg")).to_string_lossy().into_owned();
        let mut sink = GpkgLayersSink::create(&multi, &[schema_of(&layer)], preserve).unwrap().unwrap();
        sink.write(0, &layer).unwrap();
        sink.finish().unwrap();
        check(&multi);
    }
}

/// A GeoJSON collection without a `crs` member streams in the CRS GDAL gives it, as a whole read
/// does: EPSG:4979 when its geometry type has heights; a text sequence stays in WGS 84.
#[test]
fn geojson_streams_take_gdals_default_crs() {
    let dir = tempfile::tempdir().unwrap();
    for (coords, epsg) in [("[10,20,30]", 4979), ("[10,20]", 4326)] {
        let path = dir.path().join(format!("p{epsg}.geojson"));
        let feature = format!(r#"{{"type":"Feature","properties":{{"a":1}},"geometry":{{"type":"Point","coordinates":{coords}}}}}"#);
        std::fs::write(&path, format!(r#"{{"type":"FeatureCollection","features":[{feature}]}}"#)).unwrap();
        let path = path.to_string_lossy().into_owned();
        let readers = open_layers(&path, &Default::default(), 4).unwrap();
        assert_eq!(readers[0].layer().crs_epsg(), Some(epsg), "streamed {coords}");
        assert_eq!(crate::open_vector(&path, &Default::default()).unwrap().layers[0].crs_epsg(), Some(epsg), "whole {coords}");
    }
    let seq = dir.path().join("p.geojsonl");
    std::fs::write(&seq, "{\"type\":\"Feature\",\"properties\":{},\"geometry\":{\"type\":\"Point\",\"coordinates\":[10,20,30]}}\n")
        .unwrap();
    let readers = open_layers(&seq.to_string_lossy(), &Default::default(), 4).unwrap();
    assert_eq!(readers[0].layer().crs_epsg(), Some(4326), "sequence");
}

#[test]
fn geopackage_streams_reject_incompatible_preserved_types() {
    let directory = tempfile::tempdir().unwrap();
    let mut layer = polygons(1);
    layer.geom_type = Some(wbvector::GeometryType::Point);
    let options = VectorWriteOptions { preserve_declared_geometry_type: true, ..Default::default() };
    let single = directory.path().join("bad.gpkg").to_string_lossy().into_owned();
    assert!(FeatureWriter::create(&single, &layer, Some(&LayerProfile::of(&layer)), &options).is_err());
    assert!(!std::path::Path::new(&single).exists());
    let multi = directory.path().join("bad-multi.gpkg").to_string_lossy().into_owned();
    let mut sink = GpkgLayersSink::create(&multi, &[schema_of(&layer)], true).unwrap().unwrap();
    assert!(sink.write(0, &layer).unwrap_err().to_string().contains("does not match declared"));
}

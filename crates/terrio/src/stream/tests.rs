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
            let profile = needs_profile(*driver).then(|| LayerProfile::of(&layer));
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
    for ext in ["geojson", "geojsonl", "shp"] {
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

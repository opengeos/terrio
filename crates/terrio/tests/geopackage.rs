//! GeoPackage spatial filters through the R-tree, and updates of existing files, on real-world
//! files, with expected values from GDAL 3.12 (`ogr2ogr -spat`, `ogr2ogr -append`; see
//! `scripts/compare_gpkg_with_gdal.py`).
//!
//! The files live outside the repository: set `TERRIO_TEST_DATA` to their directory (see
//! `scripts/fetch_test_data.sh`). Without it, these tests pass without checking anything.

use terrio::ops::vector_filter::{VectorTranslateOptions, WriteMode, translate, write_vector_with_mode};
use terrio::{Feature, FieldDef, FieldType, FieldValue, Geometry, Layer, VectorOpenOptions, VectorWriteOptions, signed_fid};

#[path = "common/test_data.rs"]
mod test_data;
use test_data::data;

fn open(path: &str, bbox: Option<(f64, f64, f64, f64)>) -> terrio::Layer {
    let o = VectorOpenOptions { spatial_filter: bbox, ..Default::default() };
    terrio::open_vector(path, &o).unwrap().layers.remove(0)
}

fn bbox_hits(layer: &terrio::Layer, (x0, y0, x1, y1): (f64, f64, f64, f64)) -> Vec<u64> {
    layer
        .features
        .iter()
        .filter(|f| {
            f.geometry.as_ref().and_then(|g| g.bbox()).is_some_and(|b| b.min_x <= x1 && b.max_x >= x0 && b.min_y <= y1 && b.max_y >= y0)
        })
        .map(|f| f.fid)
        .collect()
}

#[test]
fn rtree_filters_match_a_scan_and_gdal() {
    // (file, rectangle, features ogr2ogr -spat keeps)
    for (name, rect, expected) in [("UScounties.gpkg", (-90.0, 35.0, -85.0, 40.0), 253), ("countries.gpkg", (-10.0, 35.0, 30.0, 60.0), 42)]
    {
        let Some(path) = data(name) else { return };
        let all = open(&path, None);
        let filtered = open(&path, Some(rect));
        // The R-tree finds the features a scan finds, with their ids and values.
        assert_eq!(filtered.features.iter().map(|f| f.fid).collect::<Vec<_>>(), bbox_hits(&all, rect), "{name}");
        for f in &filtered.features {
            let same = all.features.iter().find(|g| g.fid == f.fid).unwrap();
            assert_eq!(f.attributes, same.attributes);
            assert_eq!(f.geometry, same.geometry);
        }
        // And the exact filter keeps what GDAL keeps.
        let o = VectorTranslateOptions { spatial_filter: Some(rect), ..Default::default() };
        assert_eq!(translate(&[filtered], &o).unwrap()[0].len(), expected, "{name}");
    }
}

#[test]
fn appends_to_a_gdal_file_as_ogr2ogr() {
    let Some(path) = data("countries.gpkg") else { return };
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("c.gpkg");
    std::fs::copy(&path, &out).unwrap();
    let out = out.to_str().unwrap();
    let layer = open(&path, None);
    write_vector_with_mode(std::slice::from_ref(&layer), out, &Default::default(), WriteMode::Append).unwrap();
    let back = open(out, None);
    // ogr2ogr -append: 352 features, new ids from 177.
    assert_eq!(back.len(), 352);
    assert_eq!(back.features[176].fid, 177);
    assert_eq!(back.features[176].attributes, layer.features[0].attributes);
    assert_eq!(back.features[351].geometry, layer.features[175].geometry);
    // The rebuilt R-tree finds both copies.
    let rect = (-10.0, 35.0, 30.0, 60.0);
    assert_eq!(bbox_hits(&open(out, Some(rect)), rect).len(), 2 * bbox_hits(&layer, rect).len());
    // Overwriting gives the layer back.
    write_vector_with_mode(std::slice::from_ref(&layer), out, &Default::default(), WriteMode::Overwrite).unwrap();
    assert_eq!(open(out, None).len(), 176);
}

fn layer_with_json_columns() -> Layer {
    let mut layer = Layer::new("points").with_epsg(4326);
    layer.geom_type = Some(terrio::GeometryType::Point);
    layer.add_field(FieldDef::new("payload", FieldType::Json).width(80));
    layer.add_field(FieldDef::new("plain", FieldType::Text));
    // The writer already reserves "fid" for the SQLite rowid.
    layer.add_field(FieldDef::new("fid", FieldType::Json));
    let text = FieldValue::Text("[9007199254740993, 2]".into());
    layer.push(Feature { fid: 1, geometry: Some(Geometry::point(151.2, -33.9)), attributes: vec![text.clone(), text, FieldValue::Null] });
    layer
}

/// GDAL 3.13.3 recognizes the schema-extension MIME type even for an all-null field.
#[test]
fn json_subtype_survives_memory_and_streaming_copies() {
    use terrio::ops::vector_schema::{OgrSchema, export_schema_bytes, export_schema_path};

    let layer = layer_with_json_columns();
    let bytes = terrio::memory::write_vector_bytes(std::slice::from_ref(&layer), "source.gpkg", &Default::default()).unwrap();
    let dataset = terrio::memory::open_vector_bytes(bytes.clone(), "source.gpkg", &Default::default()).unwrap();
    let read = &dataset.layers[0];
    let types = ["String(JSON)", "String", "String(JSON)"];
    assert_eq!(types, std::array::from_fn(|i| dataset.ogr_type(read, i).to_string()));
    assert_eq!(read.features[0].attributes, layer.features[0].attributes);
    assert_eq!(read.schema.fields()[2].name, "fid_1");

    let assert_schema = |schema: OgrSchema| {
        let fields = &schema.layers[0].fields;
        assert_eq!(fields[0].sub_type.as_deref(), Some("JSON"));
        assert_eq!(fields[0].width, Some(80));
        assert_eq!(fields[1].sub_type, None);
        assert_eq!(fields[2].name.as_deref(), Some("fid_1"));
        assert_eq!(fields[2].sub_type.as_deref(), Some("JSON"));
    };
    assert_schema(export_schema_bytes(bytes.clone(), "source.gpkg", None).unwrap());

    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.gpkg");
    let copy = dir.path().join("copy.gpkg");
    std::fs::write(&source, bytes).unwrap();
    let source = source.to_str().unwrap();
    let copy = copy.to_str().unwrap();
    let mut reader = terrio::stream::open_layers(source, &Default::default(), 1).unwrap().remove(0);
    assert_eq!(types, std::array::from_fn(|i| reader.ogr_type(i).to_string()));
    assert_eq!(reader.next().unwrap().unwrap().attributes, layer.features[0].attributes);
    drop(reader);

    terrio::stream::convert(source, copy, &Default::default()).unwrap();
    assert_schema(export_schema_path(copy, &[]).unwrap());
    assert_eq!(open(copy, None).features[0].attributes, layer.features[0].attributes);
}

#[test]
fn layer_updates_keep_json_declarations_with_their_columns() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("updates.gpkg");
    let path = path.to_str().unwrap();
    let mut plain = Layer::new("plain");
    plain.add_field(FieldDef::new("payload", FieldType::Text));
    terrio::write_vector(&[plain], path, &Default::default()).unwrap();

    let layer = layer_with_json_columns();
    let mut second = layer.clone();
    second.name = "second".into();
    write_vector_with_mode(std::slice::from_ref(&layer), path, &Default::default(), WriteMode::Append).unwrap();
    write_vector_with_mode(std::slice::from_ref(&second), path, &Default::default(), WriteMode::Append).unwrap();
    write_vector_with_mode(&[second], path, &Default::default(), WriteMode::Append).unwrap();

    let mut replacement = Layer::new("points");
    replacement.add_field(FieldDef::new("payload", FieldType::Text));
    write_vector_with_mode(&[replacement], path, &Default::default(), WriteMode::Overwrite).unwrap();
    let ds = terrio::open_vector(path, &Default::default()).unwrap();
    for layer in &ds.layers {
        let expected = if layer.name == "second" { "String(JSON)" } else { "String" };
        assert_eq!(ds.ogr_type(layer, 0).to_string(), expected, "{}", layer.name);
        if layer.name == "second" {
            assert_eq!(layer.features.len(), 2);
        }
    }
}

const SOURCE_POINTS: &str = r#"{"type":"FeatureCollection","features":[
  {"type":"Feature","id":91,"properties":{},"geometry":{"type":"Point","coordinates":[0,0]}},
  {"type":"Feature","id":205,"properties":{},"geometry":{"type":"Point","coordinates":[1,1]}}
]}"#;

fn ids(layer: &Layer) -> Vec<i64> {
    layer.features.iter().map(signed_fid).collect()
}

fn preserve_fid() -> VectorWriteOptions {
    VectorWriteOptions { preserve_fid: true, ..Default::default() }
}

/// The explicit IDs also match `ogr2ogr -preserve_fid` in GDAL 3.13.2.
#[test]
fn preserves_geojson_ids_only_on_request() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("points.geojson");
    std::fs::write(&src, SOURCE_POINTS).unwrap();
    let src = src.to_str().unwrap();
    let mut layer = open(src, None);
    assert_eq!(ids(&layer), [91, 205]);
    let path = dir.path().join("points.gpkg");
    let path = path.to_str().unwrap();
    for (options, expected) in [(Default::default(), [1, 2]), (preserve_fid(), [91, 205])] {
        terrio::write_vector(std::slice::from_ref(&layer), path, &options).unwrap();
        assert_eq!(ids(&open(path, None)), expected);
    }

    // The legacy integer attribute overrides Feature::fid by default.
    layer.add_field(FieldDef::new("FID", FieldType::Integer));
    layer.features[0].attributes = vec![FieldValue::Integer(701)];
    layer.features[1].attributes = vec![FieldValue::Integer(802)];
    terrio::write_vector(std::slice::from_ref(&layer), path, &Default::default()).unwrap();
    assert_eq!(ids(&open(path, None)), [701, 802]);
    let before = std::fs::read(path).unwrap();
    let err = terrio::write_vector(&[layer], path, &preserve_fid()).unwrap_err();
    assert!(matches!(err, terrio::Error::InvalidArgument(msg) if msg.contains("value 701 conflicts with feature ID 91")));
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn writes_signed_feature_ids_and_indexes_per_layer() {
    let numbers = [i64::MAX, -1, i64::MIN, 0, 9_007_199_254_740_993];
    let mut layer = Layer::new("points").with_epsg(4326);
    layer.add_field(FieldDef::new("FID", FieldType::Integer));
    for (i, id) in numbers.into_iter().enumerate() {
        let attributes = match id {
            -1 => Vec::new(),
            0 => vec![FieldValue::Null],
            _ => vec![FieldValue::Integer(id)],
        };
        layer.push(Feature { fid: id as u64, attributes, geometry: Some(Geometry::point(i as f64, 1.0)) });
    }
    let mut second = layer.clone();
    second.name = "second".into();
    let bytes = terrio::memory::write_vector_bytes(&[layer.clone(), second], "signed.gpkg", &preserve_fid()).unwrap();
    let layers = terrio::memory::open_vector_bytes(bytes.clone(), "signed.gpkg", &Default::default()).unwrap().layers;
    for read in layers {
        assert_eq!(ids(&read), [i64::MIN, -1, 0, 9_007_199_254_740_993, i64::MAX]);
        assert!(read.schema.fields().is_empty());
        for feature in &read.features {
            let i = numbers.iter().position(|&id| id == signed_fid(feature)).unwrap();
            assert_eq!(feature.geometry, Some(Geometry::point(i as f64, 1.0)));
        }
    }
    let read = terrio::memory::open_vector_bytes(
        bytes,
        "signed.gpkg",
        &VectorOpenOptions { spatial_filter: Some((1.9, 0.9, 2.1, 1.1)), ..Default::default() },
    )
    .unwrap();
    assert_eq!(ids(&read.layers[0]), [i64::MIN]);
    let mut invalid = layer.clone();
    invalid.features[0].attributes = vec![FieldValue::Float(12.5)];
    assert!(
        terrio::memory::write_vector_bytes(&[invalid], "bad.gpkg", &preserve_fid()).unwrap_err().to_string().contains("integer or null")
    );
    let err = terrio::memory::write_vector_bytes(&[layer], "bad.geojson", &preserve_fid()).unwrap_err();
    assert!(err.to_string().contains("preserve_fid applies to GPKG"));
}

#[test]
fn append_and_replace_preserve_ids_and_refuse_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("points.gpkg");
    let path = path.to_str().unwrap();
    let points = |name, ids: &[i64]| {
        let mut layer = Layer::new(name).with_epsg(4326);
        for (i, &id) in ids.iter().enumerate() {
            layer.push(Feature { fid: id as u64, geometry: Some(Geometry::point(i as f64, 1.0)), attributes: Vec::new() });
        }
        layer
    };
    let options = preserve_fid();
    terrio::write_vector(&[points("first", &[91, 205]), points("second", &[91])], path, &options).unwrap();
    write_vector_with_mode(&[points("first", &[i64::MAX, 107, i64::MIN])], path, &options, WriteMode::Append).unwrap();
    assert_eq!(ids(&open(path, None)), [i64::MIN, 91, 107, 205, i64::MAX]);
    let before = std::fs::read(path).unwrap();
    for (mode, incoming, message) in [
        (WriteMode::Append, vec![501, 205], "feature ID 205 already exists"),
        (WriteMode::Overwrite, vec![501, 501], "duplicate feature ID 501"),
        (WriteMode::Create, vec![501, 501], "duplicate feature ID 501"),
    ] {
        let err = write_vector_with_mode(&[points("first", &incoming)], path, &options, mode).unwrap_err();
        assert!(matches!(err, terrio::Error::InvalidArgument(msg) if msg.contains("'first'") && msg.contains(message)));
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
    let err = write_vector_with_mode(&[points("first", &[501])], path, &Default::default(), WriteMode::Append).unwrap_err();
    assert!(err.to_string().contains("SQLite's integer limit"));
    assert_eq!(std::fs::read(path).unwrap(), before);

    write_vector_with_mode(&[points("first", &[501, 150])], path, &options, WriteMode::Overwrite).unwrap();
    let layers = terrio::open_vector(path, &Default::default()).unwrap().layers;
    let by_name = |name| ids(layers.iter().find(|l| l.name == name).unwrap());
    assert_eq!(by_name("first"), [150, 501]);
    assert_eq!(by_name("second"), [91]);
}

#[test]
fn buffered_writer_checks_ids_across_chunks_before_replacing_output() {
    use terrio::stream::{FeatureWriter, LayerProfile, schema_of};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("points.gpkg");
    std::fs::write(&path, b"existing").unwrap();
    let path = path.to_str().unwrap();
    let mut layer = terrio::memory::open_vector_bytes(SOURCE_POINTS.as_bytes().to_vec(), "points.geojson", &Default::default())
        .unwrap()
        .layers
        .remove(0);
    let schema = schema_of(&layer);
    let mut profile = LayerProfile::new(&schema);
    profile.add(&layer.features);
    let mut writer = FeatureWriter::create(path, &schema, Some(&profile), &preserve_fid()).unwrap();
    writer.write(&layer).unwrap();
    layer.features.pop();
    writer.write(&layer).unwrap();
    assert!(writer.finish().unwrap_err().to_string().contains("duplicate feature ID 91"));
    assert_eq!(std::fs::read(path).unwrap(), b"existing");
}

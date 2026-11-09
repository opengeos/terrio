//! GeoParquet read as Arrow record batches (`terrio::arrow::open_geoparquet`), against the
//! feature reader on the files of `tests/data/parquet_zstd`: 500 rows in 5 row groups, a
//! bounding-box covering column, and 30 rows in the filter, as GDAL 3.13.3 reads them.
#![cfg(feature = "parquet")]

use std::path::PathBuf;
use std::sync::Arc;
use terrio::arrow::arrow_array::cast::AsArray;
use terrio::arrow::arrow_array::types::Int64Type;
use terrio::arrow::arrow_array::{Array, RecordBatch, RecordBatchReader};
use terrio::arrow::{GeoParquetReadOptions, open_geoparquet};
use terrio::{FieldValue, Geometry, Layer, VectorOpenOptions};

const FILES: [&str; 4] = ["snappy.parquet", "zstd_v1.parquet", "zstd_v2.parquet", "zstd_mixed.parquet"];
const FILTER: (f64, f64, f64, f64) = (-5.5, -2.5, 5.5, 2.5);

fn path(file: &str) -> String {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/parquet_zstd").join(file).to_str().unwrap().to_string()
}

fn features(path: &str, spatial_filter: Option<(f64, f64, f64, f64)>) -> Layer {
    let options = VectorOpenOptions { spatial_filter, ..Default::default() };
    terrio::open_vector(path, &options).unwrap().layers.remove(0)
}

fn batches(path: &str, options: &GeoParquetReadOptions) -> (Vec<String>, Vec<RecordBatch>) {
    let reader = open_geoparquet(path, options).unwrap();
    let names = reader.schema().fields().iter().map(|f| f.name().clone()).collect();
    (names, reader.collect::<Result<Vec<_>, _>>().unwrap())
}

/// The ids and geometries of the rows, in order.
fn rows(batches: &[RecordBatch]) -> Vec<(i64, Option<Geometry>)> {
    batches
        .iter()
        .flat_map(|b| {
            let ids = b.column_by_name("id").unwrap().as_primitive::<Int64Type>().clone();
            let wkb = b.column_by_name("geometry").unwrap().as_binary::<i32>().clone();
            (0..b.num_rows()).map(move |i| (ids.value(i), (!wkb.is_null(i)).then(|| Geometry::from_wkb(wkb.value(i)).unwrap())))
        })
        .collect()
}

fn expected(layer: &Layer) -> Vec<(i64, Option<Geometry>)> {
    let id = layer.schema.fields().iter().position(|f| f.name == "id").unwrap();
    layer
        .features
        .iter()
        .map(|f| {
            let FieldValue::Integer(id) = f.attributes[id] else { panic!("id {:?}", f.attributes[id]) };
            (id, f.geometry.clone())
        })
        .collect()
}

#[test]
fn reads_the_rows_of_every_row_group_in_order() {
    for file in FILES {
        let (names, got) = batches(&path(file), &GeoParquetReadOptions::default());
        // the covering column is left out, as GDAL hides it
        assert_eq!(names, ["id", "name", "value", "tags", "geometry"], "{file}");
        assert_eq!(rows(&got), expected(&features(&path(file), None)), "{file}");
        assert_eq!(rows(&got).len(), 500, "{file}");
    }
}

#[test]
fn tags_the_geometry_column_as_geoarrow_wkb_with_its_crs() {
    let reader = open_geoparquet(&path("zstd_v1.parquet"), &GeoParquetReadOptions::default()).unwrap();
    let schema = reader.schema();
    let field = schema.field_with_name("geometry").unwrap();
    assert_eq!(field.metadata().get(terrio::arrow::EXTENSION_NAME).map(String::as_str), Some("geoarrow.wkb"));
    let ext: serde_json::Value = serde_json::from_str(&field.metadata()[terrio::arrow::EXTENSION_METADATA]).unwrap();
    assert!(!ext["crs"].is_null(), "{ext}");
    // the `geo` metadata no longer describes the covering column that is left out
    let geo: serde_json::Value = serde_json::from_str(&schema.metadata()["geo"]).unwrap();
    assert!(geo["columns"]["geometry"].get("covering").is_none(), "{geo}");
}

#[test]
fn filters_rows_by_a_rectangle_like_the_feature_reader() {
    for file in FILES {
        let options = GeoParquetReadOptions { bbox: Some(FILTER), ..Default::default() };
        let (_, got) = batches(&path(file), &options);
        let want = expected(&features(&path(file), Some(FILTER)));
        assert_eq!(want.len(), 30, "{file}");
        assert_eq!(rows(&got), want, "{file}");
    }
}

#[test]
fn filters_by_the_geometries_without_a_covering_column() {
    // The Snappy file written again without its covering column and its description.
    let (_, all) = batches(&path("snappy.parquet"), &GeoParquetReadOptions { columns: Some(vec!["id".into()]), ..Default::default() });
    let source = parquet::file::reader::SerializedFileReader::new(std::fs::File::open(path("snappy.parquet")).unwrap()).unwrap();
    let geo: serde_json::Value = {
        use parquet::file::reader::FileReader;
        let kv = source.metadata().file_metadata().key_value_metadata().unwrap();
        serde_json::from_str(kv.iter().find(|k| k.key == "geo").unwrap().value.as_deref().unwrap()).unwrap()
    };
    let mut geo = geo;
    geo["columns"]["geometry"].as_object_mut().unwrap().remove("covering");
    let file = tempfile::NamedTempFile::new().unwrap();
    let schema = Arc::new(all[0].schema().as_ref().clone().with_metadata(Default::default()));
    let props = parquet::file::properties::WriterProperties::builder()
        .set_max_row_group_row_count(Some(100))
        .set_key_value_metadata(Some(vec![parquet::file::metadata::KeyValue::new("geo".into(), geo.to_string())]))
        .build();
    let mut w = parquet::arrow::ArrowWriter::try_new(file.reopen().unwrap(), schema.clone(), Some(props)).unwrap();
    for b in &all {
        w.write(&RecordBatch::try_new(schema.clone(), b.columns().to_vec()).unwrap()).unwrap();
    }
    w.close().unwrap();
    let rewritten = file.path().to_str().unwrap();

    let options = GeoParquetReadOptions { bbox: Some(FILTER), ..Default::default() };
    let (names, got) = batches(rewritten, &options);
    assert_eq!(names, ["id", "geometry"]);
    assert_eq!(rows(&got), expected(&features(&path("snappy.parquet"), Some(FILTER))));
}

#[test]
fn reads_the_columns_asked_for() {
    let only =
        |columns: &[&str]| GeoParquetReadOptions { columns: Some(columns.iter().map(|c| c.to_string()).collect()), ..Default::default() };
    let (names, got) = batches(&path("zstd_v1.parquet"), &only(&["value", "id"]));
    // in the file's order, with the geometry
    assert_eq!(names, ["id", "value", "geometry"]);
    assert_eq!(got.iter().map(RecordBatch::num_rows).sum::<usize>(), 500);
    // the covering column when asked for
    assert_eq!(batches(&path("zstd_v1.parquet"), &only(&["bbox"])).0, ["bbox", "geometry"]);
    // names without case, and a struct column by one of its members, as the feature reader takes them
    assert_eq!(batches(&path("zstd_v1.parquet"), &only(&["VALUE", "BBox.xmin"])).0, ["value", "bbox", "geometry"]);
    let error = open_geoparquet(&path("zstd_v1.parquet"), &only(&["nope"])).unwrap_err().to_string();
    assert!(error.contains("nope"), "{error}");
}

#[test]
fn hands_out_batches_of_at_most_the_batch_size() {
    let options = GeoParquetReadOptions { batch_size: 30, ..Default::default() };
    let (_, got) = batches(&path("zstd_mixed.parquet"), &options);
    assert!(got.iter().all(|b| b.num_rows() <= 30));
    assert_eq!(rows(&got), expected(&features(&path("zstd_mixed.parquet"), None)));
}

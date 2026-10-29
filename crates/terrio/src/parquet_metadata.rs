//! Metadata-only Parquet dataset aggregation. Input data pages are never decoded.
use crate::{Error, Result};

/// Output installation options.
#[derive(Debug, Clone, Default)]
pub struct MetadataOptions {
    /// Replace an existing output. Inputs and their aliases are always protected.
    pub overwrite: bool,
}
/// The contents of the generated metadata-only file.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MetadataReport {
    /// Number of explicitly supplied input files, in input order.
    pub files: usize,
    /// Number of row groups across those files.
    pub row_groups: usize,
    /// Total rows.
    pub rows: u64,
    /// Encoded output size.
    pub bytes: u64,
    /// Non-geospatial metadata keys whose values differ or are absent in some inputs.
    pub omitted_metadata_keys: Vec<String>,
}
/// Create a metadata-only Parquet file referencing the input partitions.
/// Schema, Arrow field metadata and geospatial coordinate systems must agree.
/// Geospatial bounds/types are merged; other keys are retained when identical.
pub fn create(inputs: &[String], output: &str, options: &MetadataOptions) -> Result<MetadataReport> {
    #[cfg(feature = "parquet")]
    {
        enabled::create(inputs, output, options)
    }
    #[cfg(not(feature = "parquet"))]
    {
        let _ = (inputs, output, options);
        Err(Error::disabled("parquet"))
    }
}

#[cfg(feature = "parquet")]
mod enabled {
    use super::*;
    use crate::vsi::{self, dataset as location};
    use parquet::arrow::{encode_arrow_schema, parquet_to_arrow_schema};
    use parquet::file::metadata::{FileMetaData, KeyValue, ParquetMetaData, ParquetMetaDataReader, ParquetMetaDataWriter};
    use serde_json::Value;
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::{Read, SeekFrom};

    fn invalid(message: impl std::fmt::Display) -> Error {
        Error::InvalidData(format!("Parquet metadata: {message}"))
    }
    fn footer(reader: &mut dyn vsi::ReadSeek) -> Result<ParquetMetaData> {
        let size = reader.seek(SeekFrom::End(0))?;
        if size < 12 {
            return Err(invalid("file is too short"));
        }
        reader.seek(SeekFrom::Start(0))?;
        let mut header = [0; 4];
        reader.read_exact(&mut header)?;
        reader.seek(SeekFrom::End(-8))?;
        let mut tail = [0; 8];
        reader.read_exact(&mut tail)?;
        if &header == b"PARE" || &tail[4..] == b"PARE" {
            return Err(Error::Unsupported("encrypted Parquet footers are unsupported".into()));
        }
        if &header != b"PAR1" {
            return Err(invalid("invalid file header"));
        }
        let length = parquet::file::metadata::FooterTail::try_from(tail).map_err(invalid)?.metadata_length();
        if length as u64 > size - 12 {
            return Err(invalid("footer extends before the file header"));
        }
        reader.seek(SeekFrom::Start(size - 8 - length as u64))?;
        // Grow with bytes actually received, not an untrusted remote footer length.
        let mut data = Vec::with_capacity(length.min(1 << 20));
        reader.take(length as u64).read_to_end(&mut data)?;
        if data.len() != length {
            return Err(invalid("truncated footer"));
        }
        ParquetMetaDataReader::decode_metadata(&data).map_err(invalid)
    }
    fn metadata_map(metadata: &ParquetMetaData) -> Result<BTreeMap<String, Option<String>>> {
        let mut result = BTreeMap::new();
        for entry in metadata.file_metadata().key_value_metadata().into_iter().flatten() {
            if result.insert(entry.key.clone(), entry.value.clone()).is_some() {
                return Err(invalid("duplicate file metadata key"));
            }
        }
        Ok(result)
    }
    fn geo_shape(mut value: Value) -> Result<Value> {
        let primary = value["primary_column"].as_str().ok_or_else(|| invalid("GeoParquet primary_column is missing"))?.to_owned();
        let columns = value["columns"].as_object_mut().ok_or_else(|| invalid("GeoParquet columns are missing"))?;
        if !columns.contains_key(&primary) {
            return Err(invalid("GeoParquet primary column is not declared"));
        }
        for info in columns.values_mut() {
            let info = info.as_object_mut().ok_or_else(|| invalid("invalid GeoParquet column metadata"))?;
            info.remove("bbox");
            info.remove("geometry_types");
        }
        Ok(value)
    }
    fn bounds(info: &Value) -> Result<Option<Vec<Value>>> {
        let Some(value) = info.get("bbox").filter(|value| !value.is_null()) else {
            return Ok(None);
        };
        let values = value.as_array().filter(|v| matches!(v.len(), 4 | 6)).ok_or_else(|| invalid("invalid GeoParquet bbox"))?;
        for value in values {
            if !value.as_f64().is_some_and(f64::is_finite) {
                return Err(invalid("nonfinite GeoParquet bbox"));
            }
        }
        let dimensions = values.len() / 2;
        // Axis zero may wrap across the antimeridian; merge_geo omits that bbox.
        if (1..dimensions).any(|axis| values[axis].as_f64().unwrap() > values[axis + dimensions].as_f64().unwrap()) {
            return Err(invalid("inverted GeoParquet bbox"));
        }
        Ok(Some(values.clone()))
    }
    fn merge_geo(values: &[Value]) -> Result<Value> {
        let mut result = geo_shape(values[0].clone())?;
        for value in &values[1..] {
            if geo_shape(value.clone())? != result {
                return Err(invalid("GeoParquet coordinate systems/encodings/column definitions differ"));
            }
        }
        for (name, info) in result["columns"].as_object_mut().unwrap() {
            let originals: Vec<_> = values.iter().map(|value| &value["columns"][name.as_str()]).collect();
            let mut kinds = BTreeSet::new();
            let mut unknown = false;
            for original in &originals {
                let types = original
                    .get("geometry_types")
                    .map(|value| value.as_array().ok_or_else(|| invalid("invalid geometry_types array")))
                    .transpose()?;
                if types.is_none_or(Vec::is_empty) {
                    unknown = true;
                }
                if let Some(types) = types {
                    for kind in types {
                        kinds.insert(kind.as_str().ok_or_else(|| invalid("invalid geometry type"))?.to_owned());
                    }
                }
            }
            info["geometry_types"] = serde_json::json!(if unknown { Vec::new() } else { kinds.into_iter().collect() });
            let boxes = originals.iter().map(|value| bounds(value)).collect::<Result<Vec<_>>>()?;
            if boxes.iter().any(Option::is_none) {
                continue;
            }
            let boxes: Vec<_> = boxes.into_iter().flatten().collect();
            let dimensions = boxes[0].len() / 2;
            // A mixed-dimensional extent has no complete Z range. Wrapped longitude
            // intervals also cannot be unioned with Cartesian min/max arithmetic.
            // bbox is optional: retain all geometry metadata without inventing an extent.
            if boxes.iter().any(|bbox| bbox.len() != dimensions * 2 || bbox[0].as_f64().unwrap() > bbox[bbox.len() / 2].as_f64().unwrap()) {
                continue;
            }
            let mut bbox: Vec<_> = (0..dimensions)
                .map(|axis| boxes[0][axis].clone())
                .chain((0..dimensions).map(|axis| boxes[0][axis + boxes[0].len() / 2].clone()))
                .collect();
            for other in &boxes[1..] {
                for axis in 0..dimensions {
                    if other[axis].as_f64().unwrap() < bbox[axis].as_f64().unwrap() {
                        bbox[axis] = other[axis].clone();
                    }
                    let high = &other[axis + other.len() / 2];
                    if high.as_f64().unwrap() > bbox[axis + dimensions].as_f64().unwrap() {
                        bbox[axis + dimensions] = high.clone();
                    }
                }
            }
            info["bbox"] = Value::Array(bbox);
        }
        Ok(result)
    }
    pub(super) fn create(inputs: &[String], output: &str, options: &MetadataOptions) -> Result<MetadataReport> {
        let _operation = location::Operation::new();
        if inputs.is_empty() {
            return Err(Error::InvalidArgument("Parquet metadata needs at least one input".into()));
        }
        location::check_write(output)?;
        if location::entry_exists(output)? && !options.overwrite {
            return Err(Error::InvalidArgument("metadata output exists; use overwrite to replace it".into()));
        }
        let mut protected = location::OverlapIndex::new(inputs)?;
        if protected.overlaps(output)? {
            return Err(Error::InvalidArgument("metadata output overlaps an input".into()));
        }
        let mut metadata = Vec::new();
        let mut relatives = Vec::new();
        for input in inputs {
            // Establish a representable relative location before opening a footer.
            relatives.push(location::relative_file(input, output)?);
            metadata.push(location::with_seekable(input, footer)?);
        }
        let first = metadata[0].file_metadata();
        let arrow = parquet_to_arrow_schema(first.schema_descr(), first.key_value_metadata()).map_err(invalid)?;
        let mut maps = Vec::new();
        let mut row_groups = Vec::new();
        let mut rows = 0i64;
        for ((input, part), relative) in inputs.iter().zip(&metadata).zip(&relatives) {
            let mut references = std::collections::HashMap::new();
            let file = part.file_metadata();
            if first.schema().get_fields() != file.schema().get_fields() {
                return Err(invalid("partition schemas differ"));
            }
            let fields = parquet_to_arrow_schema(file.schema_descr(), file.key_value_metadata()).map_err(invalid)?;
            if fields.fields() != arrow.fields() {
                return Err(invalid("Arrow field types/metadata differ"));
            }
            let mut part_rows = 0i64;
            for group in part.row_groups() {
                if group.num_rows() < 0 {
                    return Err(invalid("negative row-group count"));
                }
                part_rows = part_rows.checked_add(group.num_rows()).ok_or_else(|| invalid("row count overflows"))?;
                let mut columns = Vec::new();
                for column in group.columns() {
                    let relative = match column.file_path().filter(|name| !name.is_empty()) {
                        Some(reference) => {
                            if !references.contains_key(reference) {
                                let physical = location::resolve_file(input, reference)?;
                                protected.insert(&physical)?;
                                references.insert(reference.to_owned(), location::relative_file(&physical, output)?);
                            }
                            &references[reference]
                        }
                        None => relative,
                    };
                    columns.push(column.clone().into_builder().set_file_path(relative.clone()).build().map_err(invalid)?);
                }
                row_groups.push(group.clone().into_builder().set_column_metadata(columns).build().map_err(invalid)?);
            }
            if part_rows != file.num_rows() {
                return Err(invalid("file and row-group counts disagree"));
            }
            rows = rows.checked_add(part_rows).ok_or_else(|| invalid("total row count overflows"))?;
            maps.push(metadata_map(part)?);
        }
        let mut common = maps[0].clone();
        let all: BTreeSet<_> = maps.iter().flat_map(|map| map.keys().cloned()).collect();
        common.retain(|key, value| maps.iter().all(|map| map.get(key) == Some(value)));
        common.remove("ARROW:schema");
        common.remove("geo");
        let geos = maps
            .iter()
            .map(|map| {
                map.get("geo")
                    .map(|text| {
                        text.as_deref()
                            .ok_or_else(|| invalid("empty GeoParquet metadata"))
                            .and_then(|text| serde_json::from_str::<Value>(text).map_err(invalid))
                    })
                    .transpose()
            })
            .collect::<Result<Vec<_>>>()?;
        if geos.iter().any(Option::is_some) {
            if geos.iter().any(Option::is_none) {
                return Err(invalid("GeoParquet metadata is missing from some partitions"));
            }
            let merged = merge_geo(&geos.into_iter().flatten().collect::<Vec<_>>())?;
            for name in merged["columns"].as_object().unwrap().keys() {
                arrow.field_with_name(name).map_err(invalid)?;
            }
            common.insert("geo".into(), Some(serde_json::to_string(&merged).map_err(invalid)?));
        }
        let omitted_metadata_keys =
            all.into_iter().filter(|key| key != "ARROW:schema" && key != "geo" && !common.contains_key(key)).collect();
        let arrow_metadata = common.iter().filter_map(|(key, value)| value.as_ref().map(|value| (key.clone(), value.clone()))).collect();
        let encoded = encode_arrow_schema(&arrow.clone().with_metadata(arrow_metadata));
        common.insert("ARROW:schema".into(), Some(encoded));
        let kv = common.into_iter().map(|(key, value)| KeyValue::new(key, value)).collect();
        let version = metadata.iter().map(|part| part.file_metadata().version()).max().unwrap();
        let orders = first
            .column_orders()
            .filter(|orders| metadata.iter().all(|part| part.file_metadata().column_orders() == Some(*orders)))
            .cloned();
        let file = FileMetaData::new(
            version,
            rows,
            Some(format!("terrio {}", env!("CARGO_PKG_VERSION"))),
            Some(kv),
            first.schema_descr_ptr(),
            orders,
        );
        let row_group_count = row_groups.len();
        let result = ParquetMetaData::new(file, row_groups);
        let mut bytes = b"PAR1".to_vec();
        ParquetMetaDataWriter::new(&mut bytes, &result).finish().map_err(invalid)?;
        if protected.overlaps(output)? {
            return Err(Error::InvalidArgument("metadata output overlaps a referenced partition".into()));
        }
        drop(protected);
        vsi::write(output, &bytes)?;
        Ok(MetadataReport {
            files: inputs.len(),
            row_groups: row_group_count,
            rows: rows as u64,
            bytes: bytes.len() as u64,
            omitted_metadata_keys,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        #[test]
        fn reads_header_and_footer_without_touching_data_pages() {
            use std::io::{Cursor, Read, Seek};
            use std::sync::Arc;
            let schema = Arc::new(arrow_schema::Schema::new(vec![arrow_schema::Field::new("value", arrow_schema::DataType::Int32, false)]));
            let batch =
                arrow_array::RecordBatch::try_new(schema.clone(), vec![Arc::new(arrow_array::Int32Array::from(vec![1, 2, 3]))]).unwrap();
            let mut writer = parquet::arrow::ArrowWriter::try_new(Vec::new(), schema, None).unwrap();
            writer.write(&batch).unwrap();
            let bytes = writer.into_inner().unwrap();
            let length = u32::from_le_bytes(bytes[bytes.len() - 8..bytes.len() - 4].try_into().unwrap()) as usize;
            struct Guard {
                cursor: Cursor<Vec<u8>>,
                footer_start: u64,
            }
            impl Read for Guard {
                fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                    let pos = self.cursor.position();
                    assert!((pos < 4 && pos + out.len() as u64 <= 4) || pos >= self.footer_start, "data pages read");
                    self.cursor.read(out)
                }
            }
            impl Seek for Guard {
                fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
                    self.cursor.seek(pos)
                }
            }
            let mut guard = Guard { footer_start: (bytes.len() - 8 - length) as u64, cursor: Cursor::new(bytes) };
            assert_eq!(footer(&mut guard).unwrap().file_metadata().num_rows(), 3);
        }
        #[test]
        fn truncated_remote_footer_does_not_request_the_declared_length_at_once() {
            use std::io::{Read, Seek};
            struct Truncated {
                position: u64,
                largest: usize,
            }
            const SIZE: u64 = 16 * 1024 * 1024;
            impl Read for Truncated {
                fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
                    if self.position == 0 && bytes.len() == 4 {
                        bytes.copy_from_slice(b"PAR1");
                        self.position += 4;
                        return Ok(4);
                    }
                    if self.position == SIZE - 8 && bytes.len() == 8 {
                        bytes[..4].copy_from_slice(&(8 * 1024 * 1024u32).to_le_bytes());
                        bytes[4..].copy_from_slice(b"PAR1");
                        self.position += 8;
                        return Ok(8);
                    }
                    self.largest = self.largest.max(bytes.len());
                    Ok(0)
                }
            }
            impl Seek for Truncated {
                fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
                    self.position = match position {
                        SeekFrom::Start(n) => n,
                        SeekFrom::Current(n) => (self.position as i64 + n) as u64,
                        SeekFrom::End(n) => (SIZE as i64 + n) as u64,
                    };
                    Ok(self.position)
                }
            }
            let mut reader = Truncated { position: 0, largest: 0 };
            assert!(footer(&mut reader).unwrap_err().to_string().contains("truncated footer"));
            assert!(reader.largest <= 1024 * 1024, "untrusted length controlled the read allocation");
        }

        #[test]
        fn external_page_indexes_decode_and_prune_the_referenced_partition() {
            use parquet::arrow::arrow_reader::{
                ArrowReaderMetadata, ArrowReaderOptions, ParquetRecordBatchReaderBuilder, RowSelection, RowSelector,
            };
            use parquet::file::metadata::PageIndexPolicy;
            use parquet::file::properties::{EnabledStatistics, WriterProperties};
            use std::sync::Arc;
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("part.parquet");
            let output = dir.path().join("_metadata");
            let schema = Arc::new(arrow_schema::Schema::new(vec![arrow_schema::Field::new("value", arrow_schema::DataType::Int32, false)]));
            let batch =
                arrow_array::RecordBatch::try_new(schema.clone(), vec![Arc::new(arrow_array::Int32Array::from(vec![1, 2, 3, 4, 5, 6]))])
                    .unwrap();
            let props = WriterProperties::builder()
                .set_statistics_enabled(EnabledStatistics::Page)
                .set_data_page_row_count_limit(2)
                .set_write_batch_size(2)
                .build();
            let mut writer = parquet::arrow::ArrowWriter::try_new(std::fs::File::create(&path).unwrap(), schema, Some(props)).unwrap();
            writer.write(&batch).unwrap();
            writer.close().unwrap();
            super::create(&[path.to_str().unwrap().to_owned()], output.to_str().unwrap(), &MetadataOptions::default()).unwrap();
            let catalog = footer(&mut std::fs::File::open(&output).unwrap()).unwrap();
            let physical = output.parent().unwrap().join(catalog.row_group(0).column(0).file_path().unwrap());
            assert_eq!(physical, path);
            // A metadata-only catalog references both data pages and their indexes in file_path.
            // Load actual index payloads using those retained offsets, not just presence flags.
            let file = std::fs::File::open(&physical).unwrap();
            let original = ParquetMetaDataReader::new().with_page_index_policy(PageIndexPolicy::Required).parse_and_finish(&file).unwrap();
            let mut reader = ParquetMetaDataReader::new_with_metadata(catalog).with_page_index_policy(PageIndexPolicy::Required);
            reader.read_page_indexes(&file).unwrap();
            let indexed = reader.finish().unwrap();
            assert_eq!(indexed.column_index(), original.column_index());
            assert_eq!(indexed.offset_index(), original.offset_index());
            assert!(indexed.offset_index().unwrap()[0][0].page_locations().len() > 1);
            let arrow = ArrowReaderMetadata::try_new(Arc::new(indexed), ArrowReaderOptions::default()).unwrap();
            let selection = RowSelection::from(vec![RowSelector::skip(4), RowSelector::select(2)]);
            let batches = ParquetRecordBatchReaderBuilder::new_with_metadata(file, arrow)
                .with_row_selection(selection)
                .build()
                .unwrap()
                .collect::<std::result::Result<Vec<_>, _>>()
                .unwrap();
            let values: Vec<_> = batches
                .iter()
                .flat_map(|batch| batch.column(0).as_any().downcast_ref::<arrow_array::Int32Array>().unwrap().values().iter().copied())
                .collect();
            assert_eq!(values, vec![5, 6]);
        }
        #[test]
        fn geo_union_and_incompatible_crs() {
            let a = serde_json::json!({"version":"1.1.0","primary_column":"geometry","columns":{"geometry":{"encoding":"WKB","geometry_types":["Point"],"bbox":[1,2,3,4]}}});
            let mut b = a.clone();
            b["columns"]["geometry"]["bbox"] = serde_json::json!([-2, 0, 8, 9]);
            b["columns"]["geometry"]["geometry_types"] = serde_json::json!(["Polygon"]);
            let merged = merge_geo(&[a.clone(), b.clone()]).unwrap();
            assert_eq!(merged["columns"]["geometry"]["bbox"], serde_json::json!([-2, 0, 8, 9]));
            assert_eq!(merged["columns"]["geometry"]["geometry_types"], serde_json::json!(["Point", "Polygon"]));
            let mut mixed = b.clone();
            mixed["columns"]["geometry"]["bbox"] = serde_json::json!([-2, 0, 1, 8, 9, 4]);
            mixed["columns"]["geometry"]["geometry_types"] = serde_json::json!(["Point Z"]);
            assert!(merge_geo(&[a.clone(), mixed]).unwrap()["columns"]["geometry"].get("bbox").is_none());
            let mut wrapped = b.clone();
            wrapped["columns"]["geometry"]["bbox"] = serde_json::json!([170, 0, -170, 9]);
            assert!(merge_geo(&[a.clone(), wrapped]).unwrap()["columns"]["geometry"].get("bbox").is_none());
            let mut unknown = b.clone();
            unknown["columns"]["geometry"].as_object_mut().unwrap().remove("bbox");
            unknown["columns"]["geometry"]["geometry_types"] = serde_json::json!([]);
            let merged = merge_geo(&[a.clone(), unknown]).unwrap();
            assert!(merged["columns"]["geometry"].get("bbox").is_none());
            assert_eq!(merged["columns"]["geometry"]["geometry_types"], serde_json::json!([]));
            b["columns"]["geometry"]["crs"] = Value::Null;
            assert!(merge_geo(&[a.clone(), b.clone()]).is_err());
            b["columns"]["geometry"].as_object_mut().unwrap().remove("crs");
            b["columns"]["geometry"]["geometry_types"] = serde_json::json!("Point");
            assert!(merge_geo(&[a, b]).is_err());
        }
    }
}

#[cfg(all(test, feature = "parquet"))]
mod tests {
    use super::*;
    use arrow_array::{Int32Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use parquet::arrow::ArrowWriter;
    use parquet::file::reader::{FileReader, SerializedFileReader};
    use std::sync::Arc;

    fn partition(path: &std::path::Path, name: &str) {
        let schema = Arc::new(Schema::new(vec![Field::new(name, DataType::Int32, false)]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(Int32Array::from(vec![2, 7, 11]))]).unwrap();
        let mut writer = ArrowWriter::try_new(std::fs::File::create(path).unwrap(), schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
    }

    #[test]
    fn aggregate_partitions_and_preserve_output_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.parquet");
        let b = dir.path().join("b.parquet");
        let output = dir.path().join("_metadata");
        partition(&a, "value");
        partition(&b, "value");
        let inputs = vec![a.to_str().unwrap().to_owned(), b.to_str().unwrap().to_owned()];
        let target = output.to_str().unwrap();
        let report = create(&inputs, target, &MetadataOptions::default()).unwrap();
        assert_eq!((report.files, report.row_groups, report.rows), (2, 2, 6));
        let reader = SerializedFileReader::new(std::fs::File::open(&output).unwrap()).unwrap();
        let metadata = reader.metadata();
        assert_eq!(metadata.file_metadata().num_rows(), 6);
        assert_eq!(metadata.row_group(0).column(0).file_path(), Some("a.parquet"));
        assert_eq!(metadata.row_group(1).column(0).file_path(), Some("b.parquet"));
        assert!(metadata.row_group(0).column(0).statistics().is_some());
        let saved = std::fs::read(&output).unwrap();
        assert!(create(&inputs, target, &MetadataOptions::default()).is_err());
        partition(&b, "other");
        assert!(create(&inputs, target, &MetadataOptions { overwrite: true }).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), saved);
        assert!(create(&inputs, inputs[0].as_str(), &MetadataOptions { overwrite: true }).is_err());
    }
    #[test]
    fn rebase_nested_memory_metadata_and_protect_referenced_partitions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("part.parquet");
        partition(&path, "value");
        let root = "/vsimem/parquet-core-rebase";
        let input = format!("{root}/a.parquet");
        let first = format!("{root}/nested/_metadata");
        let output = format!("{root}/_metadata");
        crate::vsi::write(&input, &std::fs::read(path).unwrap()).unwrap();
        create(std::slice::from_ref(&input), &first, &MetadataOptions::default()).unwrap();
        create(std::slice::from_ref(&first), &output, &MetadataOptions::default()).unwrap();
        let bytes = crate::vsi::read(&output).unwrap();
        let parsed = enabled_metadata(&bytes);
        assert_eq!(parsed.row_group(0).column(0).file_path(), Some("a.parquet"));
        let saved = crate::vsi::read(&input).unwrap();
        assert!(create(&[first], &input, &MetadataOptions { overwrite: true }).is_err());
        assert_eq!(crate::vsi::read(&input).unwrap(), saved);
        crate::vsi::delete(root, true).unwrap();
    }
    fn enabled_metadata(bytes: &[u8]) -> parquet::file::metadata::ParquetMetaData {
        let length = u32::from_le_bytes(bytes[bytes.len() - 8..bytes.len() - 4].try_into().unwrap()) as usize;
        parquet::file::metadata::ParquetMetaDataReader::decode_metadata(&bytes[bytes.len() - 8 - length..bytes.len() - 8]).unwrap()
    }
}

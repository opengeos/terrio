//! Conservative row group statistics and a filter-column pass for OGR SQL predicates.

use super::{Column, ParquetStream, nested, perr, value};
use crate::Result;
use crate::ops::sql::Expression;
use arrow_array::BooleanArray;
use arrow_schema::{DataType, Schema};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::RowSelection;
use parquet::file::metadata::RowGroupMetaData;
use parquet::file::statistics::Statistics;
use wbvector::{Feature, FieldValue};

/// Bounds in the same representation as decoded attributes. Other types stay unpruned.
fn bounds(group: &RowGroupMetaData, schema: &Schema, column: &Column) -> Option<(FieldValue, FieldValue)> {
    let mut field = schema.fields().get(column.path[0])?.as_ref();
    let mut names = vec![field.name().as_str()];
    for &member in &column.path[1..] {
        let DataType::Struct(fields) = field.data_type() else { return None };
        field = fields.get(member)?.as_ref();
        names.push(field.name().as_str());
    }
    let mut matching = group.columns().iter().filter(|c| c.column_descr().path().parts() == names);
    let chunk = matching.next()?;
    // Repeated physical names are legal. Their resolved public names identify positions,
    // so a path shared by multiple leaves cannot identify trustworthy bounds for this field.
    if matching.next().is_some() {
        return None;
    }
    let stats = chunk.statistics()?;
    // Inexact strings, legacy ordering and unsupported logical types cannot justify pruning.
    match (field.data_type(), stats) {
        (DataType::Int8 | DataType::Int16 | DataType::Int32, Statistics::Int32(s)) => {
            Some((FieldValue::Integer(i64::from(*s.min_opt()?)), FieldValue::Integer(i64::from(*s.max_opt()?))))
        }
        (DataType::Int64, Statistics::Int64(s)) => Some((FieldValue::Integer(*s.min_opt()?), FieldValue::Integer(*s.max_opt()?))),
        (DataType::Float32, Statistics::Float(s)) => {
            Some((FieldValue::Float(f64::from(*s.min_opt()?)), FieldValue::Float(f64::from(*s.max_opt()?))))
        }
        (DataType::Float64, Statistics::Double(s)) => Some((FieldValue::Float(*s.min_opt()?), FieldValue::Float(*s.max_opt()?))),
        (DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View, Statistics::ByteArray(s))
            if stats.min_is_exact() && stats.max_is_exact() && !stats.is_min_max_deprecated() =>
        {
            Some((
                FieldValue::Text(std::str::from_utf8(s.min_opt()?.data()).ok()?.into()),
                FieldValue::Text(std::str::from_utf8(s.max_opt()?.data()).ok()?.into()),
            ))
        }
        _ => None,
    }
}

impl ParquetStream {
    /// Applies a predicate before decoding payload columns, preserving original file row IDs.
    pub(crate) fn filter_attributes(&mut self, expression: &Expression) -> Result<()> {
        if self.fid != 0 {
            return Err(perr(&"attribute filters must precede the first batch"));
        }
        let predicate = expression.bind(&self.layer.schema)?;
        let mut indices: Vec<_> =
            expression.fields().into_iter().filter_map(|name| crate::ops::sql::field_index(&self.layer.schema, name)).collect();
        indices.sort_unstable();
        indices.dedup();
        let mut roots: Vec<_> = indices.iter().map(|&i| self.columns[i].path[0]).collect();
        roots.sort_unstable();
        roots.dedup();
        let mask = ProjectionMask::roots(self.meta.file_metadata().schema_descr(), roots.iter().copied());
        let mut groups = Vec::new();
        let mut fids = Vec::new();
        let mut feature = Feature { fid: 0, geometry: None, attributes: vec![FieldValue::Null; self.columns.len()] };
        let first: Vec<u64> = self
            .meta
            .row_groups()
            .iter()
            .scan(0, |n, g| {
                let first = *n;
                *n += g.num_rows() as u64;
                Some(first)
            })
            .collect();
        let candidates: Vec<_> = self
            .windows
            .pending()
            .into_iter()
            .filter(|(g, _)| predicate.may_match_ranges(|i| bounds(self.meta.row_group(*g), &self.file_schema, &self.columns[i])))
            .collect();
        // The predicate columns' chunks of each candidate group. With a prefetcher (a remote
        // file), they are fetched together, at most its budget at a time (a group alone when it
        // is larger), rather than one group per round trip.
        let chunks = |g: usize| {
            let rg = self.meta.row_group(g);
            (0..rg.num_columns()).filter(|&i| mask.leaf_included(i)).map(|i| rg.column(i).byte_range()).collect::<Vec<_>>()
        };
        let budget = self.windows.prefetch_budget();
        let mut announced = 0;
        for (k, (g, spatial)) in candidates.iter().cloned().enumerate() {
            if let Some(budget) = budget
                && k == announced
            {
                let mut ranges = chunks(g);
                let mut total = ranges.iter().fold(0u64, |t, r| t.saturating_add(r.1));
                announced += 1;
                while let Some((next, _)) = candidates.get(announced) {
                    let more = chunks(*next);
                    let size = more.iter().fold(0u64, |t, r| t.saturating_add(r.1));
                    if total.saturating_add(size) > budget {
                        break;
                    }
                    total = total.saturating_add(size);
                    ranges.extend(more);
                    announced += 1;
                }
                self.windows.prefetch(&mut ranges)?;
            }
            let mut keep = Vec::new();
            let mut row = 0;
            // Read just this group's predicate columns. The regular decoder later reads the
            // selected output columns, and no payload from groups with zero matching rows.
            for batch in (self.read_columns)(g, mask.clone())? {
                let batch = batch.map_err(|e| perr(&e))?;
                let arrays: Vec<_> = indices
                    .iter()
                    .map(|&i| {
                        let mut path = self.columns[i].path.clone();
                        path[0] = roots.binary_search(&path[0]).unwrap_or_default();
                        nested(&batch, &path)
                    })
                    .collect();
                for r in 0..batch.num_rows() {
                    feature.fid = first[g] + row;
                    for (&i, array) in indices.iter().zip(&arrays) {
                        feature.attributes[i] = value(array.as_ref(), r, self.columns[i].field_type);
                    }
                    keep.push(predicate.matches(&feature));
                    row += 1;
                }
            }
            if row != self.meta.row_group(g).num_rows() as u64 {
                return Err(perr(&"predicate row count mismatch"));
            }
            let selection = RowSelection::from_filters(&[BooleanArray::from(keep)]);
            let selection = spatial.map_or_else(|| selection.clone(), |s| s.intersection(&selection));
            if !selection.selects_any() {
                continue;
            }
            let mut row = first[g];
            for selector in selection.iter() {
                if !selector.skip {
                    fids.extend(row..row + selector.row_count as u64);
                }
                row += selector.row_count as u64;
            }
            groups.push((g, Some(selection)));
        }
        self.windows.select_groups(groups);
        self.fids = Some(fids);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbvector::{FieldDef, FieldType, Geometry, Layer};

    #[test]
    fn late_attribute_filters_leave_unread_fids_unchanged() {
        let mut layer = Layer::new("t");
        layer.add_field(FieldDef::new("v", FieldType::Integer));
        for i in 0..20 {
            layer.push(Feature { fid: i, geometry: None, attributes: vec![FieldValue::Integer(i as i64)] });
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().to_str().unwrap();
        super::super::write(&layer, path).unwrap();
        let mut stream = ParquetStream::open(|| super::super::open_file(path), "t", None, None, 4, None, None).unwrap();
        assert_eq!(stream.next_batch().unwrap().unwrap().len(), 4);
        assert!(stream.filter_attributes(&Expression::parse("v > 10").unwrap()).unwrap_err().to_string().contains("first batch"));
        let remaining = stream.read_all().unwrap();
        assert_eq!(remaining.features.iter().map(|f| f.fid).collect::<Vec<_>>(), (4..20).collect::<Vec<_>>());
    }

    /// A query on a GeoParquet file, local or remote, filters its rows in the reader and gives
    /// what the same query gives on the layer read whole, also when the reader cannot evaluate
    /// the clause.
    #[test]
    fn sql_on_a_path_matches_the_query_on_the_whole_layer() {
        let mut layer = Layer::new("t");
        layer.crs = Some(wbvector::Crs::new().with_epsg(4326));
        layer.add_field(FieldDef::new("id", FieldType::Integer));
        layer.add_field(FieldDef::new("name", FieldType::Text));
        for i in 0..50 {
            layer.push(Feature {
                fid: i,
                geometry: Some(Geometry::point(i as f64, 0.0)),
                attributes: vec![FieldValue::Integer(i as i64), FieldValue::Text(format!("n{i}"))],
            });
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.parquet");
        let path = path.to_str().unwrap();
        super::super::write(&layer, path).unwrap();
        // The file on disk, and served over HTTP (the remote reader).
        #[allow(unused_mut)]
        let mut sources = vec![path.to_string()];
        #[cfg(remote)]
        sources.push(format!("{}/t.parquet", crate::http::test_server(std::fs::read(path).unwrap()).0));
        for path in &sources {
            sql_on_a_source_matches_the_query_on_the_whole_layer(path);
        }
    }

    fn sql_on_a_source_matches_the_query_on_the_whole_layer(path: &str) {
        let whole = crate::open_vector(path, &Default::default()).unwrap().into_resolved_layers();
        assert!(whole[0].features.iter().all(|feature| feature.geometry.is_some()));
        assert!(whole[0].crs.is_some());
        for statement in [
            "SELECT * FROM t WHERE id < 7",
            "SELECT name FROM t WHERE id IN (3, 40, 99) ORDER BY id DESC",
            "SELECT * FROM t WHERE id BETWEEN 10 AND 12 OR name = 'n45' LIMIT 2",
            "SELECT * FROM t WHERE FID >= 47",
            "SELECT COUNT(*) FROM t WHERE id > 20",
            "SELECT * FROM t WHERE missing > 1 OR id = 1",
            "SELECT * FROM t WHERE ST_Area(geometry) = 0 AND id < 3",
            "SELECT * FROM t WHERE t.id < 7",
            "SELECT * FROM t",
        ] {
            let expected = crate::ops::sql::query(&whole, statement);
            let got = crate::ops::sql::query_path(path, statement, &Default::default());
            match (got, expected) {
                (Ok(got), Ok(expected)) => {
                    let rows = |l: &Layer| {
                        l.features
                            .iter()
                            .map(|f| (f.fid, f.geometry.as_ref().map(Geometry::to_wkb), f.attributes.clone()))
                            .collect::<Vec<_>>()
                    };
                    assert_eq!(rows(&got), rows(&expected), "{path}: {statement}");
                    let crs = |layer: &Layer| layer.crs.as_ref().map(|crs| (crs.epsg, crs.wkt.clone()));
                    assert_eq!(crs(&got), crs(&expected), "{path}: {statement}");
                    assert_eq!(got.geom_type, expected.geom_type, "{path}: {statement}");
                }
                (Err(got), Err(expected)) => assert_eq!(got.to_string(), expected.to_string(), "{path}: {statement}"),
                (got, expected) => {
                    panic!(
                        "{path}: {statement}: got {:?}, expected {:?}",
                        got.map(|l| l.features.len()),
                        expected.map(|l| l.features.len())
                    )
                }
            }
        }
    }

    /// A file of 200 rows in row groups of 10: `id` (0..200) and an 8 KB `name` per row.
    fn grouped_file() -> bytes::Bytes {
        use arrow_array::{Int64Array, RecordBatch, StringArray};
        use arrow_schema::Field;
        use std::sync::Arc;
        let schema = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, false), Field::new("name", DataType::Utf8, false)]));
        let ids: Vec<i64> = (0..200).collect();
        let names: Vec<String> = ids.iter().map(|i| format!("{i:0>8192}")).collect();
        let batch =
            RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(ids)), Arc::new(StringArray::from(names))]).unwrap();
        let props = parquet::file::properties::WriterProperties::builder().set_max_row_group_row_count(Some(10)).build();
        let mut data = Vec::new();
        let mut writer = parquet::arrow::ArrowWriter::try_new(&mut data, schema, Some(props)).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        data.into()
    }

    /// With a prefetcher (a remote file), the predicate pass announces the filter columns of the
    /// row groups its statistics keep before reading them, many groups per call, within the
    /// prefetcher's budget.
    #[test]
    fn predicate_columns_are_prefetched_together_within_the_budget() {
        use std::sync::{Arc, Mutex};
        let data = grouped_file();
        let meta = parquet::arrow::arrow_reader::ArrowReaderMetadata::load(&data, Default::default()).unwrap().metadata().clone();
        let id_chunk = |g: usize| meta.row_group(g).column(0).byte_range();
        // `id < 35 OR id >= 180`: groups 0..4 and 18..20.
        let kept: Vec<usize> = (0..4).chain(18..20).collect();
        // Two of the largest kept chunks: the kept groups in pairs.
        let pair = 2 * kept.iter().map(|&g| id_chunk(g).1).max().unwrap();
        for (budget, calls) in [(u64::MAX, 1), (pair, 3)] {
            // The ranges of each call to the prefetcher.
            type Calls = Arc<Mutex<Vec<Vec<(u64, u64)>>>>;
            let seen = Calls::default();
            let record = seen.clone();
            let prefetch = move |r: &[(u64, u64)]| {
                record.lock().unwrap().push(r.to_vec());
                Ok(())
            };
            let mut stream = ParquetStream::open(|| Ok(data.clone()), "t", None, None, 64, None, None)
                .unwrap()
                .with_prefetcher((Box::new(prefetch), budget));
            stream.filter_attributes(&Expression::parse("id < 35 OR id >= 180").unwrap()).unwrap();
            let seen = std::mem::take(&mut *seen.lock().unwrap());
            assert_eq!(seen.len(), calls, "budget {budget}");
            assert_eq!(seen.concat(), kept.iter().map(|&g| id_chunk(g)).collect::<Vec<_>>(), "budget {budget}");
            let fids: Vec<u64> = stream.read_all().unwrap().features.iter().map(|f| f.fid).collect();
            assert_eq!(fids, (0..35).chain(180..200).collect::<Vec<_>>());
        }
    }

    /// A query on a remote GeoParquet file filters its rows in the reader, as on a local one.
    #[cfg(remote)]
    #[test]
    fn sql_on_a_url_matches_the_query_on_a_local_path() {
        let data = grouped_file();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.parquet");
        std::fs::write(&path, &data).unwrap();
        let (base, requests) = crate::http::test_server(data.to_vec());
        let statement = "SELECT id FROM t WHERE id < 35 OR id IN (150, 199)";
        let local = crate::ops::sql::query_path(path.to_str().unwrap(), statement, &Default::default()).unwrap();
        let remote = crate::ops::sql::query_path(&format!("{base}/t.parquet"), statement, &Default::default()).unwrap();
        let rows = |l: &Layer| l.features.iter().map(|f| (f.fid, f.attributes.clone())).collect::<Vec<_>>();
        assert_eq!(rows(&remote), rows(&local));
        assert_eq!(remote.features.len(), 37);
        // Only the 6 row groups whose statistics may match are read, not the 20 of the file: a
        // whole read would fetch the file's 1.6 MB of names. A request without a range (or with
        // another form) counts as the whole file.
        let fetched: usize = requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| {
                r.as_deref()
                    .and_then(|r| {
                        let (a, b) = r.strip_prefix("bytes=")?.split_once('-')?;
                        Some(b.parse::<usize>().ok()?.min(data.len() - 1) + 1 - a.parse::<usize>().ok()?)
                    })
                    .unwrap_or(data.len())
            })
            .sum();
        assert!(fetched < data.len() / 2, "{fetched} of {} bytes", data.len());
    }

    #[test]
    fn sql_on_a_path_reports_predicate_read_errors() {
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
        use parquet::file::metadata::ParquetMetaDataWriter;
        let mut layer = Layer::new("t");
        layer.add_field(FieldDef::new("id", FieldType::Integer));
        for i in 0..20 {
            layer.push(Feature { fid: i, geometry: None, attributes: vec![FieldValue::Integer(i as i64)] });
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.parquet");
        let path = path.to_str().unwrap();
        super::super::write(&layer, path).unwrap();
        let mut bytes = std::fs::read(path).unwrap();
        let meta = ParquetRecordBatchReaderBuilder::try_new(bytes::Bytes::from(bytes.clone())).unwrap().metadata().as_ref().clone();
        let group = meta.row_group(0).clone().into_builder().set_num_rows(21).build().unwrap();
        let meta = meta.into_builder().set_row_groups(vec![group]).build();
        // Keep the data pages intact but make their row count disagree with the footer.
        ParquetMetaDataWriter::new(&mut bytes, &meta).finish().unwrap();
        std::fs::write(path, &bytes).unwrap();
        // On disk, and served over HTTP (the remote reader).
        #[allow(unused_mut)]
        let mut sources = vec![path.to_string()];
        #[cfg(remote)]
        sources.push(format!("{}/t.parquet", crate::http::test_server(bytes).0));
        for path in &sources {
            let error = crate::ops::sql::query_path(path, "SELECT * FROM t WHERE id < 7", &Default::default()).unwrap_err();
            assert!(error.to_string().contains("predicate row count mismatch"), "{path}: {error}");
        }
    }

    /// `IN` keeps a row group when one of its values may be in the group's range.
    #[test]
    fn in_lists_prune_by_ranges() {
        let mut layer = Layer::new("t");
        layer.add_field(FieldDef::new("v", FieldType::Integer));
        let may = |text: &str| {
            let e = Expression::parse(text).unwrap().bind(&layer.schema).unwrap();
            e.may_match_ranges(|_| Some((FieldValue::Integer(10), FieldValue::Integer(20))))
        };
        assert!(!may("v IN (1, 25)"));
        assert!(may("v IN (1, 15)"));
        assert!(may("v NOT IN (1, 25)"));
        assert!(may("v IN (1, v)"));
        assert!(!may("v IN (1, 2) AND v > 0"));
    }

    #[test]
    fn duplicate_physical_paths_cannot_prune_the_wrong_column() {
        use arrow_array::{Int64Array, RecordBatch};
        use arrow_schema::Field;
        use std::sync::Arc;
        let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Int64, false), Field::new("v", DataType::Int64, false)]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int64Array::from(vec![0, 0, 0])), Arc::new(Int64Array::from(vec![100, 101, 102]))],
        )
        .unwrap();
        let mut data = Vec::new();
        let mut writer = parquet::arrow::ArrowWriter::try_new(&mut data, schema, None).unwrap();
        writer.write(&batch).unwrap();
        writer.close().unwrap();
        let data = bytes::Bytes::from(data);
        let mut stream = ParquetStream::open(|| Ok(data.clone()), "t", None, None, 64, None, None).unwrap();
        stream.filter_attributes(&Expression::parse("v_2 BETWEEN 100 AND 101").unwrap()).unwrap();
        let layer = stream.read_all().unwrap();
        assert_eq!(layer.features.iter().map(|f| f.fid).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(layer.features[1].attributes, [FieldValue::Integer(0), FieldValue::Integer(101)]);
    }

    #[test]
    fn string_ranges_never_exclude_case_folded_matches() {
        let mut layer = Layer::new("strings");
        layer.add_field(FieldDef::new("s", FieldType::Text));
        let mut values = ["", "0001", "0067", "415", "A", "B", "Z", "[", "_", "a", "b", "z", "é"];
        values.sort();
        for start in 0..values.len() {
            for end in start..values.len() {
                for literal in values {
                    for op in ["=", "<", "<=", ">", ">=", "<>"] {
                        let expression = Expression::parse(&format!("s {op} '{literal}'")).unwrap().bind(&layer.schema).unwrap();
                        let may_match = expression
                            .may_match_ranges(|_| Some((FieldValue::Text(values[start].into()), FieldValue::Text(values[end].into()))));
                        for value in &values[start..=end] {
                            let feature = Feature { fid: 0, geometry: None, attributes: vec![FieldValue::Text((*value).into())] };
                            assert!(may_match || !expression.matches(&feature), "{start}..{end}: {value} {op} {literal}");
                        }
                    }
                }
            }
        }
        let expression = Expression::parse("s = '0067'").unwrap().bind(&layer.schema).unwrap();
        assert!(!expression.may_match_ranges(|_| Some((FieldValue::Text("0001".into()), FieldValue::Text("0002".into())))));
    }
}

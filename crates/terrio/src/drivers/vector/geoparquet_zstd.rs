//! Zstandard column chunks of Parquet files, without the `parquet` crate's Zstandard, which is the
//! zstd C library. A chunk's pages are read as if they were not compressed, and then decompressed
//! here with `crate::zstandard`. Other compressions stay with the crate.

use std::sync::Arc;

use arrow_schema::{Fields, Schema};
use bytes::Bytes;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::{ParquetRecordBatchReader, RowGroups, RowSelection};
use parquet::arrow::parquet_to_arrow_field_levels;
use parquet::basic::Compression;
use parquet::column::page::{Page, PageIterator, PageMetadata, PageReader};
use parquet::errors::{ParquetError, Result};
use parquet::file::metadata::{ColumnChunkMetaData, ParquetMetaData, RowGroupMetaData};
use parquet::file::reader::{ChunkReader, Length};
use parquet::file::serialized_reader::SerializedPageReader;

/// One opened input shared by a metadata reader and a page reader, so that a file is not opened
/// again for its Zstandard chunks.
pub(super) struct SharedInput<R>(pub(super) Arc<R>);

impl<R> Clone for SharedInput<R> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<R: Length> Length for SharedInput<R> {
    fn len(&self) -> u64 {
        self.0.len()
    }
}

impl<R: ChunkReader> ChunkReader for SharedInput<R> {
    type T = R::T;

    fn get_read(&self, start: u64) -> Result<Self::T> {
        self.0.get_read(start)
    }

    fn get_bytes(&self, start: u64, length: usize) -> Result<Bytes> {
        self.0.get_bytes(start, length)
    }
}

/// Whether a chunk of the projected columns of `groups` is compressed with Zstandard.
pub(super) fn has_zstd(meta: &ParquetMetaData, groups: &[usize], mask: &ProjectionMask) -> bool {
    groups.iter().any(|&g| {
        meta.row_group(g)
            .columns()
            .iter()
            .enumerate()
            .any(|(i, c)| mask.leaf_included(i) && matches!(c.compression(), Compression::ZSTD(_)))
    })
}

/// A reader of the rows of `groups`, like `ParquetRecordBatchReaderBuilder`'s, that also reads
/// Zstandard chunks.
pub(super) fn reader<R: ChunkReader + 'static>(
    input: R,
    meta: &Arc<ParquetMetaData>,
    schema: &Schema,
    mask: ProjectionMask,
    groups: Vec<usize>,
    selection: Option<RowSelection>,
    batch_size: usize,
) -> Result<ParquetRecordBatchReader> {
    let fields: &Fields = schema.fields();
    let levels = parquet_to_arrow_field_levels(meta.file_metadata().schema_descr(), mask, Some(fields))?;
    // As `ParquetRecordBatchReaderBuilder` does, to avoid allocating a large buffer.
    let batch_size = batch_size.min(meta.file_metadata().num_rows() as usize).max(1);
    let row_groups = ZstdRowGroups { input: Arc::new(input), meta: meta.clone(), groups };
    ParquetRecordBatchReader::try_new_with_row_groups(&levels, &row_groups, batch_size, selection)
}

struct ZstdRowGroups<R> {
    input: Arc<R>,
    meta: Arc<ParquetMetaData>,
    groups: Vec<usize>,
}

impl<R: ChunkReader + 'static> RowGroups for ZstdRowGroups<R> {
    fn num_rows(&self) -> usize {
        self.groups.iter().map(|&g| self.meta.row_group(g).num_rows() as usize).sum()
    }

    fn column_chunks(&self, i: usize) -> Result<Box<dyn PageIterator>> {
        Ok(Box::new(Chunks { input: self.input.clone(), meta: self.meta.clone(), column: i, groups: self.groups.clone().into_iter() }))
    }

    fn row_groups(&self) -> Box<dyn Iterator<Item = &RowGroupMetaData> + '_> {
        Box::new(self.groups.iter().map(|&g| self.meta.row_group(g)))
    }

    fn metadata(&self) -> &ParquetMetaData {
        &self.meta
    }
}

/// The page readers of a column's chunks. Metadata is read without its page index, so pages are
/// found from their headers.
struct Chunks<R> {
    input: Arc<R>,
    meta: Arc<ParquetMetaData>,
    column: usize,
    groups: std::vec::IntoIter<usize>,
}

impl<R: ChunkReader + 'static> Chunks<R> {
    fn pages(&self, group: usize) -> Result<Box<dyn PageReader>> {
        let rg = self.meta.row_group(group);
        let chunk = rg.column(self.column);
        let rows = rg.num_rows() as usize;
        if !matches!(chunk.compression(), Compression::ZSTD(_)) {
            return Ok(Box::new(SerializedPageReader::new(self.input.clone(), chunk, rows, None)?));
        }
        let raw: ColumnChunkMetaData = chunk.clone().into_builder().set_compression(Compression::UNCOMPRESSED).build()?;
        let limit = usize::try_from(chunk.uncompressed_size()).map_err(|e| ParquetError::External(Box::new(e)))?;
        Ok(Box::new(ZstdPages { pages: SerializedPageReader::new(self.input.clone(), &raw, rows, None)?, limit }))
    }
}

impl<R: ChunkReader + 'static> Iterator for Chunks<R> {
    type Item = Result<Box<dyn PageReader>>;

    fn next(&mut self) -> Option<Self::Item> {
        let group = self.groups.next()?;
        Some(self.pages(group))
    }
}

impl<R: ChunkReader + 'static> PageIterator for Chunks<R> {}

/// The pages of a Zstandard chunk, decompressed.
struct ZstdPages<R: ChunkReader> {
    pages: SerializedPageReader<R>,
    /// The chunk's uncompressed size: no page decompresses to more.
    limit: usize,
}

impl<R: ChunkReader> ZstdPages<R> {
    fn decompress(&self, data: &[u8]) -> Result<Vec<u8>> {
        // A page with no non-null values may have no data at all.
        if data.is_empty() {
            return Ok(Vec::new());
        }
        crate::zstandard::decompress(data, self.limit).map_err(|e| ParquetError::External(Box::new(e)))
    }

    fn page(&self, page: Page) -> Result<Page> {
        Ok(match page {
            Page::DataPage { buf, num_values, encoding, def_level_encoding, rep_level_encoding, statistics } => {
                let buf = Bytes::from(self.decompress(&buf)?);
                Page::DataPage { buf, num_values, encoding, def_level_encoding, rep_level_encoding, statistics }
            }
            // The levels of a v2 page come first, never compressed.
            Page::DataPageV2 {
                buf,
                num_values,
                encoding,
                num_nulls,
                num_rows,
                def_levels_byte_len,
                rep_levels_byte_len,
                is_compressed,
                statistics,
            } => {
                let levels = def_levels_byte_len as usize + rep_levels_byte_len as usize;
                let buf = match is_compressed {
                    true if levels > buf.len() => return Err(ParquetError::General("Invalid page header".to_string())),
                    true => {
                        let values = self.decompress(&buf[levels..])?;
                        Bytes::from([&buf[..levels], &values].concat())
                    }
                    false => buf,
                };
                Page::DataPageV2 {
                    buf,
                    num_values,
                    encoding,
                    num_nulls,
                    num_rows,
                    def_levels_byte_len,
                    rep_levels_byte_len,
                    is_compressed: false,
                    statistics,
                }
            }
            Page::DictionaryPage { buf, num_values, encoding, is_sorted } => {
                Page::DictionaryPage { buf: Bytes::from(self.decompress(&buf)?), num_values, encoding, is_sorted }
            }
        })
    }
}

impl<R: ChunkReader> Iterator for ZstdPages<R> {
    type Item = Result<Page>;

    fn next(&mut self) -> Option<Self::Item> {
        self.get_next_page().transpose()
    }
}

impl<R: ChunkReader> PageReader for ZstdPages<R> {
    fn get_next_page(&mut self) -> Result<Option<Page>> {
        self.pages.get_next_page()?.map(|page| self.page(page)).transpose()
    }

    fn peek_next_page(&mut self) -> Result<Option<PageMetadata>> {
        self.pages.peek_next_page()
    }

    fn skip_next_page(&mut self) -> Result<()> {
        self.pages.skip_next_page()
    }

    fn at_record_boundary(&mut self) -> Result<bool> {
        self.pages.at_record_boundary()
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs::File;
    use std::path::PathBuf;

    use super::super::geoparquet::ParquetStream;

    /// How many times reading a fixture opens it.
    fn opens(file: &str, bbox: Option<(f64, f64, f64, f64)>) -> u32 {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data/parquet_zstd").join(file);
        let count = Cell::new(0);
        let open = || {
            count.set(count.get() + 1);
            Ok(File::open(&path)?)
        };
        assert!(ParquetStream::open(open, "t", None, bbox, 64, None).is_ok());
        count.get()
    }

    /// Zstandard chunks open a file no more often than others, so a remote file's footer is not
    /// fetched again.
    #[test]
    fn zstd_chunks_add_no_opens() {
        let bbox = Some((-5.5, -2.5, 5.5, 2.5));
        for file in ["snappy.parquet", "zstd_v1.parquet", "zstd_mixed.parquet"] {
            assert_eq!(opens(file, None), 1, "{file}");
            assert_eq!(opens(file, bbox), 2, "{file}");
        }
    }
}

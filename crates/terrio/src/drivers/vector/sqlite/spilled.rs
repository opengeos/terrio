//! Build interior b-tree pages from a temporary sequence of (page, largest rowid) pairs.
//! Keeping the last two groups permits the same singleton rebalance as the in-memory writer.
use super::*;
use std::io::{BufReader, BufWriter, Read, SeekFrom, Write};

impl<W: Write + std::io::Seek> StreamTable<W> {
    pub(super) fn finish_spilled(mut self, rows: BufWriter<Box<dyn super::super::Spill>>, mut count: usize) -> Result<(u32, u32, W)> {
        let mut file = rows.into_inner().map_err(|e| crate::Error::Io(e.into_error()))?;
        while count > 1 {
            file.seek(SeekFrom::Start(0))?;
            let mut input = BufReader::new(file);
            let mut output = BufWriter::new(super::super::spill()?);
            let mut next_count = 0;
            let mut previous: Option<Vec<(u32, i64)>> = None;
            let mut group = Vec::new();
            let mut used = 12;
            for _ in 0..count {
                let mut b = [0; 12];
                input.read_exact(&mut b)?;
                let pair = (u32::from_le_bytes(b[..4].try_into().unwrap()), i64::from_le_bytes(b[4..].try_into().unwrap()));
                let len = 4 + varint_len(pair.1 as u64) + 2;
                if !group.is_empty() && used + len > PAGE_SIZE - 100 {
                    if let Some(previous) = previous.take() {
                        self.write_group(&previous, &mut output)?;
                        next_count += 1;
                    }
                    previous = Some(std::mem::take(&mut group));
                    used = 12;
                }
                used += len;
                group.push(pair);
            }
            if let Some(mut previous) = previous {
                if group.len() == 1 {
                    group.insert(0, previous.pop().unwrap());
                }
                self.write_group(&previous, &mut output)?;
                next_count += 1;
            }
            self.write_group(&group, &mut output)?;
            next_count += 1;
            file = output.into_inner().map_err(|e| crate::Error::Io(e.into_error()))?;
            count = next_count;
        }
        file.seek(SeekFrom::Start(0))?;
        let mut root = [0; 4];
        file.read_exact(&mut root)?;
        self.out.flush()?;
        Ok((u32::from_le_bytes(root), self.next, self.out))
    }

    fn write_group(&mut self, group: &[(u32, i64)], output: &mut impl Write) -> Result<()> {
        let cells: Vec<_> = group[..group.len() - 1]
            .iter()
            .map(|&(child, key)| {
                let mut c = child.to_be_bytes().to_vec();
                varint(&mut c, key as u64);
                c
            })
            .collect();
        let (right, key) = group[group.len() - 1];
        let page = self.alloc();
        self.write_page(page, &node_bytes(0x05, &cells, Some(right)))?;
        output.write_all(&page.to_le_bytes())?;
        output.write_all(&key.to_le_bytes())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn spilled_page_directories_preserve_database_bytes() {
        for count in [0, 1, 512, 513, 514, 515, 1200] {
            let record = record(&[Value::Blob(vec![7; 3000])]);
            let mut normal = StreamTable::starting_at(std::io::Cursor::new(Vec::new()), 2);
            let mut disk = StreamTable::starting_at_spilled(std::io::Cursor::new(Vec::new()), 2).unwrap();
            for id in 1..=count {
                normal.push(id, &record).unwrap();
                disk.push(id, &record).unwrap();
            }
            let (a, b, normal) = normal.finish_with_output().unwrap();
            let (c, d, disk) = disk.finish_with_output().unwrap();
            assert_eq!((a, b), (c, d), "{count}");
            assert_eq!(normal.into_inner(), disk.into_inner(), "{count}");
        }
    }
}

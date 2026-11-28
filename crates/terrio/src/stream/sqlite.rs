//! A read-only SQLite reader that reads pages from the file as it needs them, so that the
//! feature tables of GeoPackages larger than memory are read a leaf page at a time.

use crate::drivers::sqlite::{self, PageSet, SchemaEntry, Table, Value, bad, cell_pointer, varint};
use crate::drivers::vector::gpkg_read::Tables;
use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom};
use std::sync::Mutex;

/// Where the pages of a [`FileDb`] come from.
enum Pages {
    File(std::fs::File),
    /// A remote file, read by range requests through its cache of `cache_size` bytes in blocks
    /// of `block_size`.
    #[cfg(remote)]
    Http {
        reader: crate::http::RangeReader,
        cache_size: u64,
        block_size: u64,
    },
}

impl Pages {
    fn read_exact_at(&mut self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        fn read(r: &mut (impl Read + Seek), offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
            r.seek(SeekFrom::Start(offset))?;
            r.read_exact(buf)
        }
        match self {
            Pages::File(f) => read(f, offset, buf),
            #[cfg(remote)]
            Pages::Http { reader, .. } => read(reader, offset, buf),
        }
    }
}

/// An SQLite database file read page by page.
pub(crate) struct FileDb {
    file: Mutex<Pages>,
    page_size: usize,
    usable: usize,
    pages: u64,
    encoding: u32,
    schema: Vec<SchemaEntry>,
    /// The pages of the last [`Tables::lookup`], from the root down: lookups of ascending rowids
    /// (an R-tree's candidates) read each page on their way once, not once a row.
    lookup_path: Mutex<Vec<(u32, Vec<u8>)>>,
    /// The pages read, in order.
    #[cfg(test)]
    reads: Mutex<Vec<u32>>,
}

impl FileDb {
    /// Opens a database file, reading its header and schema.
    pub(crate) fn open(path: &str) -> Result<Self> {
        // A database in WAL mode may have pages that only its `-wal` file holds.
        let wal = std::path::Path::new(&format!("{path}-wal")).metadata().is_ok_and(|m| m.len() > 0);
        if wal {
            return Err(Error::Unsupported(format!("'{path}' has a write-ahead log: checkpoint it before streaming")));
        }
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        Self::read(Pages::File(file), len)
    }

    /// Opens a remote database file, reading its header and schema by range requests through
    /// `reader`, opened with `options`. A `-wal` file beside it is not looked for: its pages are
    /// read as the file holds them.
    #[cfg(remote)]
    pub(crate) fn open_remote(reader: crate::http::RangeReader, options: &crate::http::HttpOptions) -> Result<Self> {
        let len = reader.len();
        Self::read(Pages::Http { reader, cache_size: options.cache_size, block_size: options.block_size.max(1) }, len)
    }

    /// Reads a database's header and schema from its pages; `len` is the file's length.
    fn read(mut file: Pages, len: u64) -> Result<Self> {
        let mut h = [0u8; 100];
        file.read_exact_at(0, &mut h).map_err(|_| bad("not an SQLite database"))?;
        if !h.starts_with(b"SQLite format 3\0") {
            return Err(bad("not an SQLite database"));
        }
        let page_size = match u16::from_be_bytes([h[16], h[17]]) {
            1 => 65536,
            p if p >= 512 && p.is_power_of_two() => p as usize,
            _ => return Err(bad("invalid page size")),
        };
        // SQLite requires at least 480 usable bytes a page.
        let usable = page_size.checked_sub(h[20] as usize).filter(|&u| u >= 480).ok_or_else(|| bad("invalid reserved space"))?;
        let mut db = Self {
            file: Mutex::new(file),
            page_size,
            usable,
            pages: (len / page_size as u64).min(u64::from(u32::MAX)),
            encoding: sqlite::text_encoding(u32::from_be_bytes([h[56], h[57], h[58], h[59]])),
            schema: Vec::new(),
            lookup_path: Mutex::default(),
            #[cfg(test)]
            reads: Mutex::default(),
        };
        let mut entries = Vec::new();
        let mut cursor = db.cursor(1);
        while let Some(rows) = cursor.next_leaf(&db)? {
            for (_, payload) in rows {
                let r = db.decode_record(&payload)?;
                let text = |i: usize| r.get(i).and_then(Value::as_str).unwrap_or_default().to_string();
                entries.push((text(0), text(1), text(2), r.get(3).and_then(Value::as_i64).unwrap_or(0) as u32, text(4)));
            }
        }
        db.schema = entries;
        // The GeoPackage tables an open reads, together.
        db.prefetch(
            db.schema.iter().filter(|e| e.0 == "table" && e.1.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("gpkg_"))).map(|e| e.3),
        )?;
        Ok(db)
    }

    /// Fetches pages together, before they are read, when they come over the network: runs of
    /// adjacent pages as one range each, through the block cache, which coalesces and fetches
    /// them in parallel. A no-op for a local file.
    #[cfg_attr(not(remote), allow(unused_variables))]
    fn prefetch(&self, pages: impl IntoIterator<Item = u32>) -> Result<()> {
        #[cfg(remote)]
        if let Pages::Http { reader, .. } = &*self.file.lock().map_err(|_| bad("file lock"))? {
            let mut pages: Vec<u32> = pages.into_iter().filter(|&n| n > 0 && n as u64 <= self.pages).collect();
            pages.sort_unstable();
            pages.dedup();
            let ps = self.page_size as u64;
            let mut ranges: Vec<(u64, u64)> = Vec::new();
            for n in pages {
                let at = (n as u64 - 1) * ps;
                match ranges.last_mut() {
                    Some((off, len)) if *off + *len == at => *len += ps,
                    _ => ranges.push((at, ps)),
                }
            }
            reader.prefetch(&ranges)?;
        }
        Ok(())
    }

    /// The most pages a prefetch asks for at once: those a quarter of the cache holds (a block
    /// each, when blocks are larger than pages), so that each prefetch is fetched in one batch
    /// and stays cached until it is read. `None` for a local file, which is not prefetched.
    fn prefetch_budget(&self) -> Option<usize> {
        match &*self.file.lock().ok()? {
            #[cfg(remote)]
            Pages::Http { cache_size, block_size, .. } => Some((cache_size / 4 / (*block_size).max(self.page_size as u64)).max(1) as usize),
            Pages::File(_) => None,
        }
    }

    /// The first overflow page and the overflow page count of a cell's payload of `size` bytes
    /// starting at `start` on `page`; `None` when the payload fits on the page (or the cell is
    /// corrupt, which its read reports).
    fn overflow_chain(&self, page: &[u8], start: usize, size: usize) -> Option<(u32, usize)> {
        let u = self.usable;
        if size <= u - 35 {
            return None;
        }
        let local = sqlite::local_payload_size(u, size, u - 35);
        let first = u32::from_be_bytes(page.get(start + local..start + local + 4)?.try_into().ok()?);
        let count = (size - local).div_ceil(u - 4);
        (first != 0 && first as u64 <= self.pages && count as u64 <= self.pages).then_some((first, count))
    }

    /// Prefetches the pages of overflow chains `(first page, pages)`, at most `budget` pages: each
    /// chain's pages asked for as one run (where a file written in order holds them), and past a
    /// jump as runs from one page, twice as long each time it goes on in order (see
    /// [`next_run`]), so that a chain spread over the file costs rounds rather than many pages
    /// it does not hold. Callers give chains of at most half the budget, but for the first leaf's
    /// or row's, which the budget cuts.
    fn prefetch_chains(&self, chains: Vec<(u32, usize)>, budget: usize) -> Result<()> {
        // (next page, pages left in the chain, pages to ask for)
        let mut chains: Vec<(u32, usize, usize)> = chains.into_iter().map(|(p, left)| (p, left, left)).collect();
        let mut fetched = 0;
        while !chains.is_empty() && fetched < budget {
            let mut runs = Vec::new();
            for (p, left, ask) in chains {
                let n = ask.min(budget - fetched);
                if n == 0 {
                    break;
                }
                runs.push((p, left, n));
                fetched += n;
            }
            self.prefetch(runs.iter().flat_map(|&(p, _, n)| (p..=u32::MAX).take(n)))?;
            chains = Vec::new();
            for (mut p, left, n) in runs {
                for k in 0..n {
                    let page = self.page(p)?;
                    let to = u32::from_be_bytes([page[0], page[1], page[2], page[3]]);
                    // The end of the chain, or a page the read reports.
                    if to == 0 || k + 1 == left || to as u64 > self.pages {
                        break;
                    }
                    if to != p.wrapping_add(1) || k + 1 == n {
                        chains.push((to, left - k - 1, next_run(n, p, to, left - k - 1)));
                        break;
                    }
                    p = to;
                }
            }
        }
        Ok(())
    }

    /// Prefetches `pages` of a scan, then the overflow pages of the rows of the first of them
    /// that are table leaves, as many as half the `budget` holds (the first leaf's, whatever they
    /// take, up to the budget: the read of a longer chain fetches the rest a budget at a time).
    /// Returns how many of the first pages that covers (at least one). Pages that cannot be read
    /// are left for the scan to report.
    fn prefetch_scan(&self, pages: &[u32], budget: usize) -> Result<usize> {
        self.prefetch(pages.iter().copied())?;
        let (mut chains, mut held, mut covered) = (Vec::new(), 0, 0);
        for &n in pages {
            if let Ok(page) = self.page(n)
                && page[if n == 1 { 100 } else { 0 }] == 0x0d
                && let Ok(cells) = sqlite::table_leaf_cells(&page, n, self.usable)
            {
                let more: Vec<_> = cells.iter().filter_map(|c| self.overflow_chain(&page, c.payload_start, c.payload_size)).collect();
                let size: usize = more.iter().map(|c| c.1).sum();
                if covered > 0 && held + size > budget / 2 {
                    break;
                }
                held += size;
                chains.extend(more);
            }
            covered += 1;
        }
        // A chain page that cannot be read is left for the scan to report.
        let _ = self.prefetch_chains(chains, budget);
        Ok(covered.max(1))
    }

    /// [`Tables::prefetch_rows`]'s descent, counting in `covered` the first ids whose pages it
    /// fetched.
    fn descend_rows(&self, root: u32, ids: &[i64], budget: usize, covered: &mut usize) -> Result<()> {
        // A level of the tree: its pages, and the ids each holds (a run of `ids`).
        let mut level: Vec<(u32, &[i64])> = vec![(root, ids)];
        let (mut chains, mut held, mut full) = (Vec::new(), 0, false);
        // An interior page reached again (a corrupt tree) is not descended again, and the ids
        // past it are not counted: lookups report it.
        let mut interior = std::collections::HashSet::new();
        for _ in 0..64 {
            level.truncate(budget);
            self.prefetch(level.iter().map(|(n, _)| *n))?;
            let mut next = Vec::new();
            for (n, ids) in level {
                let page = self.page(n)?;
                let h = if n == 1 { 100 } else { 0 };
                match page[h] {
                    0x05 => {
                        if !interior.insert(n) {
                            full = true;
                            continue;
                        }
                        let mut ids = ids;
                        while let Some(&first) = ids.first() {
                            let c = child(&page, h, first)?;
                            // The ids under the same child: those below its key.
                            let k = ids.partition_point(|&id| child(&page, h, id).is_ok_and(|other| other == c));
                            next.push((c, &ids[..k.max(1)]));
                            ids = &ids[k.max(1)..];
                        }
                    }
                    0x0d => {
                        for &id in ids {
                            if full {
                                break;
                            }
                            if let Some(cell) = sqlite::table_leaf_find(&page, n, self.usable, id)?
                                && let Some(chain) = self.overflow_chain(&page, cell.payload_start, cell.payload_size)
                            {
                                if *covered > 0 && held + chain.1 > budget / 2 {
                                    full = true;
                                    break;
                                }
                                held += chain.1;
                                chains.push(chain);
                            }
                            *covered += 1;
                        }
                    }
                    _ => return Err(bad("unsupported page type")),
                }
            }
            if next.is_empty() || full {
                break;
            }
            level = next;
        }
        self.prefetch_chains(chains, budget)
    }

    /// Whether the first byte of page `n` of a remote file is cached (a local file's never is).
    #[cfg_attr(not(remote), allow(unused_variables))]
    fn is_cached(&self, n: u32) -> bool {
        match self.file.lock().as_deref() {
            #[cfg(remote)]
            Ok(Pages::Http { reader, .. }) => n > 0 && reader.is_cached((n as u64 - 1) * self.page_size as u64),
            _ => false,
        }
    }

    fn page(&self, n: u32) -> Result<Vec<u8>> {
        if n == 0 || n as u64 > self.pages {
            return Err(bad("page out of range"));
        }
        #[cfg(test)]
        self.reads.lock().map_err(|_| bad("page log lock"))?.push(n);
        let mut buf = vec![0u8; self.page_size];
        let mut f = self.file.lock().map_err(|_| bad("file lock"))?;
        f.read_exact_at((n as u64 - 1) * self.page_size as u64, &mut buf)?;
        Ok(buf)
    }

    /// A cell's payload, following overflow pages.
    fn payload(&self, page_num: u32, page: &[u8], start: usize, size: usize) -> Result<Vec<u8>> {
        let mut seen = std::collections::HashSet::new();
        self.payload_checked(page, start, size, |p| {
            if p == page_num || !seen.insert(p) {
                return Err(bad("overflow page reached twice"));
            }
            Ok(())
        })
    }

    /// A cell's payload, following overflow pages and notifying `visit_overflow` for each overflow
    /// page before reading it so cycles and shared overflow pages are rejected.
    fn payload_checked(
        &self,
        page: &[u8],
        start: usize,
        size: usize,
        mut visit_overflow: impl FnMut(u32) -> Result<()>,
    ) -> Result<Vec<u8>> {
        let u = self.usable;
        let x = u - 35;
        if size <= x {
            return start.checked_add(size).and_then(|end| page.get(start..end)).map(<[u8]>::to_vec).ok_or_else(|| bad("payload"));
        }
        // Each overflow page holds `u - 4` bytes; a chain longer than the file is corrupt.
        if (size / (u - 4)) as u64 > self.pages {
            return Err(bad("payload"));
        }
        let local = sqlite::local_payload_size(u, size, x);
        let mut out = page.get(start..start + local).ok_or_else(|| bad("payload"))?.to_vec();
        let mut next =
            u32::from_be_bytes(page.get(start + local..start + local + 4).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("overflow"))?);
        // Over the network, the pages of a chain no prefetch fetched (one longer than a
        // prefetch's budget) are asked for as runs: from its first page, the rest of it (at most
        // a budget); past a jump, a page, then runs twice as long while it goes on in order.
        let budget = self.prefetch_budget();
        let (mut asked, mut run, mut prev) = (0..0, 0, 0u32);
        while out.len() < size && next != 0 {
            visit_overflow(next)?;
            if let Some(budget) = budget
                && !asked.contains(&next)
                && !self.is_cached(next)
            {
                let left = (size - out.len()).div_ceil(u - 4);
                run = next_run(run, prev, next, left);
                asked = next..next.saturating_add(run.min(budget) as u32);
                self.prefetch(asked.clone())?;
            }
            prev = next;
            let p = self.page(next)?;
            next = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
            let take = (size - out.len()).min(u - 4);
            out.extend_from_slice(&p[4..4 + take]);
        }
        if out.len() < size || next != 0 {
            return Err(bad("payload"));
        }
        Ok(out)
    }

    /// A cursor over the rows of the table b-tree rooted at `root`, in rowid order.
    pub(crate) fn cursor(&self, root: u32) -> Cursor {
        Cursor { stack: vec![Level { pages: vec![root], next: 0, fetched: 0, children_fetched: 0 }], visited: PageSet::new(self.pages) }
    }

    /// Decodes a record for reading, as [`crate::drivers::sqlite::Db::decode_record`] does.
    pub(crate) fn decode_record(&self, payload: &[u8]) -> Result<Vec<Value>> {
        crate::drivers::sqlite_repack::decode_record_lossy(payload, self.encoding)
    }

    /// The root page and `CREATE TABLE` statement of a rowid table.
    pub(crate) fn table_root(&self, name: &str) -> Result<(u32, String)> {
        for (kind, table, _, root, sql) in &self.schema {
            if kind != "table" || !table.eq_ignore_ascii_case(name) {
                continue;
            }
            if *root == 0 {
                return Err(bad(&format!("'{name}' is a virtual table")));
            }
            if sql.to_ascii_uppercase().split_whitespace().collect::<Vec<_>>().join(" ").contains("WITHOUT ROWID") {
                return Err(bad(&format!("'{name}' is a WITHOUT ROWID table, which is not supported")));
            }
            return Ok((*root, sql.clone()));
        }
        Err(bad(&format!("no table '{name}'")))
    }
}

/// Rows `(rowid, payload)`.
pub(crate) type Rows = Vec<(i64, Vec<u8>)>;

/// A position in a table b-tree: for each level from the root down, the pages still to visit
/// (each interior page is visited once).
pub(crate) struct Cursor {
    stack: Vec<Level>,
    /// The pages visited: a b-tree reaches each page once, so a page reached twice is corrupt
    /// (a cycle, or interior pages sharing children, which would otherwise visit exponentially
    /// many leaves).
    visited: PageSet,
}

/// The pages of a level of a [`Cursor`]: `pages[next..]` are still to visit. A remote file's
/// pages are fetched a window at a time: those before `fetched`, and the children of those
/// before `children_fetched`.
struct Level {
    pages: Vec<u32>,
    next: usize,
    fetched: usize,
    children_fetched: usize,
}

impl Cursor {
    /// The rows `(rowid, payload)` of the next leaf page; `None` past the last.
    pub(crate) fn next_leaf(&mut self, db: &FileDb) -> Result<Option<Rows>> {
        loop {
            let depth = self.stack.len();
            let Some(level) = self.stack.last_mut() else { break };
            let Some(&n) = level.pages.get(level.next) else {
                self.stack.pop();
                continue;
            };
            if level.next == level.fetched
                && let Some(budget) = db.prefetch_budget()
            {
                let end = level.pages.len().min(level.next + budget);
                level.fetched = level.next + db.prefetch_scan(&level.pages[level.next..end], budget)?;
            }
            level.next += 1;
            if depth > 64 {
                return Err(bad("b-tree too deep"));
            }
            self.visited.insert(n)?;
            let page = db.page(n)?;
            let h = if n == 1 { 100 } else { 0 };
            match page[h] {
                0x05 => {
                    let children = interior_children(&page, h)?;
                    let mut fetched = 0;
                    if let Some(budget) = db.prefetch_budget() {
                        // The children of this page and, while they fit, of the next pages of its
                        // level fetched with it, in one prefetch: a round for many leaf pages.
                        let at = level.next - 1;
                        fetched = children.len().min(budget);
                        if at >= level.children_fetched {
                            let mut ahead = children[..fetched].to_vec();
                            let mut end = at + 1;
                            while end < level.fetched && ahead.len() < budget {
                                // A page that cannot be read is left for the scan to report.
                                let Ok(sibling) = db.page(level.pages[end]) else { break };
                                let sh = if level.pages[end] == 1 { 100 } else { 0 };
                                let more =
                                    if sibling[sh] == 0x05 { interior_children(&sibling, sh).unwrap_or_default() } else { Vec::new() };
                                if more.is_empty() || ahead.len() + more.len() > budget {
                                    break;
                                }
                                ahead.extend(more);
                                end += 1;
                            }
                            // Where the rows' overflow pages did not all fit, the pages past them
                            // are fetched again with theirs.
                            let covered = db.prefetch_scan(&ahead, budget)?;
                            if covered < ahead.len() {
                                (end, fetched) = (at + 1, fetched.min(covered));
                            }
                            level.children_fetched = end;
                        }
                    }
                    self.stack.push(Level { pages: children, next: 0, fetched, children_fetched: 0 });
                }
                0x0d => {
                    let cells = sqlite::table_leaf_cells(&page, n, db.usable)?;
                    let mut out = Vec::with_capacity(cells.len());
                    for cell in cells {
                        let payload = db.payload_checked(&page, cell.payload_start, cell.payload_size, |p| self.visited.insert(p))?;
                        out.push((cell.rowid, payload));
                    }
                    return Ok(Some(out));
                }
                _ => return Err(bad("unsupported page type")),
            }
        }
        Ok(None)
    }
}

impl Tables for FileDb {
    fn schema(&self) -> Result<Vec<SchemaEntry>> {
        Ok(self.schema.clone())
    }

    fn table(&self, name: &str) -> Result<Table> {
        let (root, sql) = self.table_root(name)?;
        let columns: Vec<String> = sqlite::column_types(&sql).into_iter().map(|(n, _)| n).collect();
        let rowid_col = sqlite::rowid_column(&sql);
        let mut rows = Vec::new();
        let mut cursor = self.cursor(root);
        while let Some(leaf) = cursor.next_leaf(self)? {
            for (rowid, payload) in leaf {
                rows.push(row_values(self, &payload, rowid, columns.len(), rowid_col)?);
            }
        }
        Ok(Table { columns, rows })
    }

    /// A level of the tree at a time from the root down, then the overflow pages of the rows
    /// found ([`FileDb::prefetch_chains`]). Each prefetch asks for at most
    /// [`FileDb::prefetch_budget`] pages, so only the leaves of the first ids, and the overflow
    /// pages of those that half the budget holds (the first id's, whatever they take), may be
    /// fetched.
    fn prefetch_rows(&self, root: u32, ids: &[i64]) -> Result<usize> {
        let Some(budget) = self.prefetch_budget() else { return Ok(ids.len()) };
        // A page that cannot be read ends the prefetch: the lookups report it if they reach it.
        let mut covered = 0;
        let _ = self.descend_rows(root, ids, budget, &mut covered);
        Ok(covered)
    }

    fn lookup(&self, root: u32, rowid: i64) -> Result<Option<Vec<u8>>> {
        let mut path = self.lookup_path.lock().map_err(|_| bad("page path lock"))?;
        let mut n = root;
        for depth in 0..64 {
            if path.get(depth).is_none_or(|(p, _)| *p != n) {
                path.truncate(depth);
                path.push((n, self.page(n)?));
            }
            let page = &path[depth].1;
            let h = if n == 1 { 100 } else { 0 };
            match page[h] {
                0x05 => n = child(page, h, rowid)?,
                0x0d => {
                    return match sqlite::table_leaf_find(page, n, self.usable, rowid)? {
                        Some(cell) => self.payload(n, page, cell.payload_start, cell.payload_size).map(Some),
                        None => Ok(None),
                    };
                }
                _ => return Err(bad("unsupported page type")),
            }
        }
        Err(bad("b-tree too deep"))
    }
}

/// How many pages of an overflow chain to ask for from page `next`, `left` of them left, after
/// asking for `run` pages (0 for none: a prefetch fetched those before) and reading page `prev`
/// (0 before the first): the rest of the chain from its first page, or where it goes on in
/// order from pages a prefetch fetched (a file written in order holds them together); one page
/// past a jump, then twice the last run while it goes on in order. The pages asked for and not
/// held stay fewer than three times those held.
fn next_run(run: usize, prev: u32, next: u32, left: usize) -> usize {
    let run = if prev == 0 || (run == 0 && next == prev.wrapping_add(1)) {
        left
    } else if next == prev.wrapping_add(1) {
        run * 2
    } else {
        1
    };
    run.clamp(1, left.max(1))
}

/// The children of interior table b-tree page `page` (its header at `h`): its cells' left
/// children, then its right-most pointer.
fn interior_children(page: &[u8], h: usize) -> Result<Vec<u32>> {
    let cells = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
    let mut children = Vec::with_capacity(cells + 1);
    for c in 0..cells {
        let off = cell_pointer(page, h + 12 + 2 * c)?;
        children.push(u32::from_be_bytes(page.get(off..off + 4).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("cell"))?));
    }
    children.push(u32::from_be_bytes(page[h + 8..h + 12].try_into().map_err(|_| bad("page"))?));
    Ok(children)
}

/// The child of interior table b-tree page `page` (its header at `h`) whose rows hold `rowid`.
fn child(page: &[u8], h: usize, rowid: i64) -> Result<u32> {
    let cells = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
    let cell = |c: usize| cell_pointer(page, h + 12 + 2 * c);
    let (mut lo, mut hi) = (0, cells);
    while lo < hi {
        let mid = (lo + hi) / 2;
        let (key, _) = varint(page, cell(mid)? + 4).ok_or_else(|| bad("cell"))?;
        if key < rowid { lo = mid + 1 } else { hi = mid }
    }
    if lo < cells {
        let off = cell(lo)?;
        Ok(u32::from_be_bytes(page.get(off..off + 4).and_then(|b| b.try_into().ok()).ok_or_else(|| bad("cell"))?))
    } else {
        Ok(u32::from_be_bytes(page[h + 8..h + 12].try_into().map_err(|_| bad("page"))?))
    }
}

/// A row's values from its record, as [`crate::drivers::sqlite::Db::table`] gives them: padded to the table's columns,
/// the rowid alias column filled in.
pub(crate) fn row_values(db: &FileDb, payload: &[u8], rowid: i64, columns: usize, rowid_col: Option<usize>) -> Result<Vec<Value>> {
    let mut v = db.decode_record(payload)?;
    v.resize(columns.max(v.len()), Value::Null);
    if let Some(c) = rowid_col
        && v[c] == Value::Null
    {
        v[c] = Value::Int(rowid);
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interior pages that share children (each page's cell and right-most pointer naming the
    /// next page) are an error, as in the in-memory reader: a cursor over them would visit
    /// 2^depth leaves. Five levels here would visit the leaf 32 times.
    #[test]
    fn b_tree_pages_reached_twice_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dag.gpkg");
        std::fs::write(&path, crate::drivers::sqlite::fixtures::shared_children(5)).unwrap();
        let error = FileDb::open(path.to_str().unwrap()).err().expect("an error");
        assert!(error.to_string().contains("reached twice"), "{error}");
    }

    #[test]
    fn cyclic_overflow_chains_are_errors() {
        use crate::drivers::sqlite::fixtures;
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in [
            ("self.gpkg", fixtures::cyclic_overflow_self()),
            ("pair.gpkg", fixtures::cyclic_overflow_pair()),
            ("single.gpkg", fixtures::cyclic_overflow_single_page()),
            ("shared.gpkg", fixtures::shared_overflow_cells()),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            let db = open_raw_without_schema(&path);
            let mut cursor = db.cursor(1);
            assert!(cursor.next_leaf(&db).is_err(), "{name}: cursor should reject cyclic/shared overflow");
            if name != "shared.gpkg" {
                assert!(db.lookup(1, 1).is_err(), "{name}: lookup should reject cyclic overflow");
            }
        }

        // Cross-leaf shared overflow page across cursor leaf steps.
        let shared_leaves_path = dir.path().join("shared_leaves.gpkg");
        std::fs::write(&shared_leaves_path, fixtures::shared_overflow_leaves()).unwrap();
        let db = open_raw_without_schema(&shared_leaves_path);
        let mut cursor = db.cursor(1);
        assert!(cursor.next_leaf(&db).unwrap().is_some());
        assert!(cursor.next_leaf(&db).is_err());

        // Cross-leaf disjoint overflow chains succeed across cursor leaf steps and lookup.
        let disjoint_leaves_path = dir.path().join("disjoint_leaves.gpkg");
        std::fs::write(&disjoint_leaves_path, fixtures::disjoint_overflow_leaves()).unwrap();
        let db = open_raw_without_schema(&disjoint_leaves_path);
        let mut cursor = db.cursor(1);
        let leaf1 = cursor.next_leaf(&db).unwrap().unwrap();
        let leaf2 = cursor.next_leaf(&db).unwrap().unwrap();
        assert!(cursor.next_leaf(&db).unwrap().is_none());
        assert_eq!((leaf1.len(), leaf1[0].0, leaf1[0].1[3]), (1, 1, 1));
        assert_eq!((leaf2.len(), leaf2[0].0, leaf2[0].1[3]), (1, 2, 2));
        assert_eq!(db.lookup(1, 1).unwrap().unwrap()[3], 1);
        assert_eq!(db.lookup(1, 2).unwrap().unwrap()[3], 2);
        assert!(db.lookup(1, 3).unwrap().is_none());

        // Also test FileDb::lookup directly on a valid empty-schema DB with a cyclic overflow
        // leaf on page 2 (`3 -> 3`).
        let mut b = fixtures::database(3);
        b[100] = 0x0d; // empty valid schema leaf on page 1
        b[512] = 0x0d; // leaf table on page 2
        b[512 + 3..512 + 5].copy_from_slice(&1u16.to_be_bytes());
        b[512 + 8..512 + 10].copy_from_slice(&200u16.to_be_bytes());
        let start = fixtures::write_leaf_cell(&mut b, 2, 200, 1, 92 + 2 * 508);
        b[start + 92..start + 96].copy_from_slice(&3u32.to_be_bytes());
        b[1024..1028].copy_from_slice(&3u32.to_be_bytes());
        let path = dir.path().join("lookup_cycle.gpkg");
        std::fs::write(&path, b).unwrap();
        let db = FileDb::open(path.to_str().unwrap()).unwrap();
        assert!(db.lookup(2, 1).is_err());
    }

    #[test]
    fn aliased_and_overlapping_leaf_cells_are_errors() {
        use crate::drivers::sqlite::fixtures;
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes) in [("aliased.gpkg", fixtures::aliased_leaf_cells()), ("overlapping.gpkg", fixtures::overlapping_leaf_cells())] {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            let db = open_raw_without_schema(&path);
            let mut cursor = db.cursor(1);
            assert!(cursor.next_leaf(&db).is_err(), "{name}: cursor should reject aliased/overlapping cells");
            // A lookup reads one payload by binary search, so it returns the single matching cell.
            assert!(db.lookup(1, 1).unwrap().is_some(), "{name}: lookup returns one cell");
        }

        // Cell pointer falling inside the page header / cell pointer array (`104 < 110`).
        let mut b = fixtures::database(2);
        b[100] = 0x0d;
        b[103..105].copy_from_slice(&1u16.to_be_bytes());
        b[108..110].copy_from_slice(&104u16.to_be_bytes());
        let header_path = dir.path().join("header_overlap.gpkg");
        std::fs::write(&header_path, b).unwrap();
        let db = open_raw_without_schema(&header_path);
        assert!(db.lookup(1, 1).is_err());
    }

    /// A bbox read looks up the rows its R-tree finds in ascending rowid order, and reads each page
    /// on the way once, not the root and interior pages again for every row. Lookups return what a
    /// scan of the table returns, in any order, and nothing for a rowid the table lacks.
    #[test]
    fn rtree_lookups_read_each_page_once() {
        use wbvector::{Feature, FieldDef, FieldType, FieldValue, Geometry, Layer};
        let mut layer = Layer::new("points");
        layer.add_field(FieldDef::new("name", FieldType::Text));
        for i in 0..3000 {
            layer.push(Feature {
                fid: i,
                geometry: Some(Geometry::point(i as f64, 0.0)),
                attributes: vec![FieldValue::Text(format!("row {i} {}", "x".repeat(120)))],
            });
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("points.gpkg");
        std::fs::write(&path, crate::drivers::vector::geopackage::to_bytes(&[layer]).unwrap()).unwrap();
        let db = FileDb::open(path.to_str().unwrap()).unwrap();
        let (root, _) = db.table_root("points").unwrap();
        assert_eq!(db.page(root).unwrap()[0], 0x05, "the table's root is an interior page");

        let ids =
            crate::drivers::vector::gpkg_read::rtree_candidates(&db, "rtree_points_geom", (100.0, -1.0, 1100.0, 1.0)).unwrap().unwrap();
        db.reads.lock().unwrap().clear();
        let found: Vec<(i64, Vec<u8>)> = ids.iter().map(|&id| (id, db.lookup(root, id).unwrap().unwrap())).collect();
        let reads = db.reads.lock().unwrap().clone();
        let distinct: std::collections::HashSet<_> = reads.iter().collect();
        assert_eq!(found.len(), 1001);
        assert_eq!(reads.len(), distinct.len(), "pages read more than once: {} reads of {} pages", reads.len(), distinct.len());

        let mut scanned = Vec::new();
        let mut cursor = db.cursor(root);
        while let Some(leaf) = cursor.next_leaf(&db).unwrap() {
            scanned.extend(leaf.into_iter().filter(|(id, _)| ids.contains(id)));
        }
        assert_eq!(found, scanned);
        // Backwards, and between rowids the table lacks.
        for (id, payload) in found.iter().rev() {
            assert!(db.lookup(root, -id).unwrap().is_none());
            assert_eq!(&db.lookup(root, *id).unwrap().unwrap(), payload);
            assert!(db.lookup(root, id + 1_000_000).unwrap().is_none());
        }
    }

    /// A remote row whose 40 overflow pages are spread over the file (a file whose free pages
    /// were reused) is read with its pages and few others, in fewer rounds where the chain goes
    /// on in order: a jump at every page, in pairs, once and then in order, or none. With a cache
    /// whose prefetch budget (16 pages) is shorter than the chain, the read fetches the rest
    /// itself, at once where it goes on in order.
    #[cfg(remote)]
    #[test]
    fn remote_overflow_chains_that_jump_fetch_their_pages() {
        use crate::drivers::sqlite::fixtures;
        let layouts: [(&str, Vec<u32>, usize); 4] = [
            ("every page", (0..40).map(|k| 1900 - 40 * k).collect(), 45),
            ("pairs", (0..40).map(|k| 1900 - 40 * (k / 2) + k % 2).collect(), 45),
            ("once", std::iter::once(1900).chain(100..139).collect(), 15),
            ("in order", (100..140).collect(), 4),
        ];
        for (name, chain, most_requests) in layouts {
            for cache_size in [1 << 20, 64 << 10] {
                let mut b = fixtures::database(2000);
                b[100] = 0x0d; // an empty schema
                b[512] = 0x0d; // page 2: a table leaf of one cell
                b[512 + 3..512 + 5].copy_from_slice(&1u16.to_be_bytes());
                b[512 + 8..512 + 10].copy_from_slice(&200u16.to_be_bytes());
                let start = fixtures::write_leaf_cell(&mut b, 2, 200, 1, 92 + 40 * 508);
                b[start + 92..start + 96].copy_from_slice(&chain[0].to_be_bytes());
                for (k, &p) in chain.iter().enumerate() {
                    let at = (p as usize - 1) * 512;
                    let next = chain.get(k + 1).copied().unwrap_or(0);
                    b[at..at + 4].copy_from_slice(&next.to_be_bytes());
                    b[at + 4..at + 512].fill(k as u8);
                }
                let size = b.len();
                let (base, seen) = crate::http::test_server(b);
                let http = crate::http::HttpOptions { block_size: 1024, cache_size, ..Default::default() };
                let remote = crate::http::Remote::Url(format!("{base}/chain.gpkg"));
                let db = FileDb::open_remote(crate::http::RangeReader::open_with(&remote, http.clone()).unwrap(), &http).unwrap();
                let rows = db.cursor(2).next_leaf(&db).unwrap().unwrap();
                assert_eq!((rows.len(), rows[0].1.len()), (1, 92 + 40 * 508), "{name}");
                assert!(rows[0].1[92..].chunks(508).enumerate().all(|(k, c)| c.iter().all(|&v| v == k as u8)), "{name}");
                let ranges: Vec<usize> = seen
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|r| {
                        let (a, b) = r.as_deref().unwrap().strip_prefix("bytes=").unwrap().split_once('-').unwrap();
                        b.parse::<usize>().unwrap().min(size - 1) + 1 - a.parse::<usize>().unwrap()
                    })
                    .collect();
                let fetched: usize = ranges.iter().sum();
                // The 40 pages in blocks of two, a run of 40 pages from the first, and the open.
                assert!(fetched <= 120 * 1024, "{name}, cache {cache_size}: fetched {fetched} bytes for 40 pages of 512");
                assert!(ranges.len() <= most_requests, "{name}, cache {cache_size}: {} requests", ranges.len());
            }
        }
    }

    /// Opens a synthetic 512-byte-page `FileDb` without decoding `sqlite_master` records, so tests
    /// can exercise `Cursor::next_leaf` on raw payloads directly.
    fn open_raw_without_schema(path: &std::path::Path) -> FileDb {
        let file = std::fs::File::open(path).unwrap();
        let len = file.metadata().unwrap().len();
        FileDb {
            file: Mutex::new(Pages::File(file)),
            page_size: 512,
            usable: 512,
            pages: len / 512,
            encoding: 1,
            schema: Vec::new(),
            lookup_path: Mutex::default(),
            reads: Mutex::default(),
        }
    }

    #[test]
    fn file_db_caps_page_count_at_u32_max() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.gpkg");
        let mut bytes = crate::drivers::sqlite::fixtures::database(1);
        bytes[100] = 0x0d; // empty schema leaf on page 1
        std::fs::write(&path, bytes).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let db = FileDb::read(Pages::File(file), (u64::from(u32::MAX) + 100) * 512).unwrap();
        assert_eq!(db.pages, u64::from(u32::MAX));

        let file = std::fs::File::open(&path).unwrap();
        let db = FileDb::read(Pages::File(file), u64::MAX).unwrap();
        assert_eq!(db.pages, u64::from(u32::MAX));
    }
}

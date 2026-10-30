//! Changing an SQLite database file in place, as SQLite does: pages are read when needed and
//! changed in memory, then written back in one transaction protected by a rollback journal.
//!
//! The commit follows SQLite's rollback-journal protocol (`journal_mode=DELETE`): the original
//! content of every page about to be overwritten is written to `<file>-journal` and synced
//! before the database file is touched, the new pages are written and synced, and deleting the
//! journal commits. A process that stops in between leaves a hot journal behind, which SQLite
//! (and [`Editor::open`]) plays back to restore the database as it was. Native row edits take
//! SQLite-compatible POSIX locks on Unix. Full-image repacking additionally takes Windows
//! byte-range locks; ordinary Windows edits keep their rewrite fallback. A conflicting
//! SQLite connection in another process prevents the exclusive lock. POSIX locks
//! are process-scoped: callers must close same-process SQLite connections before
//! using this editor and must not concurrently access the database through them.
//!
//! Table b-trees grow by inserting cells and splitting pages, as SQLite's own b-tree does;
//! small tables and indexes are rebuilt in place (their root pages kept). Pages freed go to the
//! freelist, and pages are taken from it before the file grows.
//!
//! The format is described at <https://www.sqlite.org/fileformat2.html>.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::vector::sqlite::{self as w, PageStore};
use crate::{Error, Result};

/// The magic number starting a rollback journal.
const JOURNAL_MAGIC: [u8; 8] = [0xd9, 0xd5, 0x05, 0xf9, 0x20, 0xa1, 0x63, 0xd7];
/// The pages read together when one is needed.
const READ_PAGES: u32 = 16;
/// The sector size the journal header is padded to, as SQLite assumes by default.
const SECTOR: usize = 512;
/// The byte offset of SQLite's reserved lock-byte page (also a journal sentinel).
const PENDING_BYTE: i64 = 0x4000_0000;

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("SQLite: {msg}"))
}

/// Why a database cannot be changed in place (it is left untouched).
#[derive(Debug)]
pub(crate) struct NotInPlace(#[allow(dead_code)] pub String);

/// Reads a varint at `i`: `(value, length)`.
pub(crate) fn varint(b: &[u8], i: usize) -> Option<(i64, usize)> {
    let mut v: u64 = 0;
    for k in 0..9 {
        let byte = *b.get(i + k)?;
        if k == 8 {
            v = (v << 8) | byte as u64;
            return Some((v as i64, 9));
        }
        v = (v << 7) | (byte & 0x7f) as u64;
        if byte & 0x80 == 0 {
            return Some((v as i64, k + 1));
        }
    }
    None
}

/// A b-tree page, parsed: its type, cells (as stored, with child pointers, payload sizes and
/// overflow page numbers) and right-most child.
#[derive(Debug, Clone)]
pub(crate) struct Node {
    pub kind: u8,
    pub cells: Vec<Vec<u8>>,
    pub right: Option<u32>,
}

/// Full-image replacement owns both buffers; it never clones their pages into the edit cache.
struct FullRewrite {
    original: Vec<u8>,
    image: Vec<u8>,
}

/// A database file opened for changes.
pub(crate) struct Editor {
    file: File,
    path: PathBuf,
    page_size: usize,
    /// The database size in pages when opened: pages past it need no journal.
    orig_pages: u32,
    /// The current size in pages.
    n_pages: u32,
    /// The pages read or changed.
    cache: HashMap<u32, Vec<u8>>,
    /// The original content of the pages changed (up to `orig_pages`), for the journal.
    originals: BTreeMap<u32, Vec<u8>>,
    dirty: BTreeSet<u32>,
    /// Whether the schema changed (the schema cookie is then incremented).
    pub schema_changed: bool,
    rewrite: Option<FullRewrite>,
}

impl PageStore for Editor {
    fn page_size(&self) -> usize {
        self.page_size
    }

    fn alloc(&mut self) -> Result<u32> {
        let (trunk, count) = (self.header_u32(32)?, self.header_u32(36)?);
        if count > 0 && trunk > 0 {
            let t = self.page(trunk)?;
            let next = u32::from_be_bytes([t[0], t[1], t[2], t[3]]);
            let leaves = u32::from_be_bytes([t[4], t[5], t[6], t[7]]) as usize;
            let n = if leaves > 0 {
                let at = 8 + 4 * (leaves - 1);
                let leaf = u32::from_be_bytes([t[at], t[at + 1], t[at + 2], t[at + 3]]);
                let t = self.page_mut(trunk)?;
                t[4..8].copy_from_slice(&((leaves - 1) as u32).to_be_bytes());
                t[at..at + 4].fill(0);
                leaf
            } else {
                self.set_header_u32(32, next)?;
                trunk
            };
            self.set_header_u32(36, count - 1)?;
            self.page_mut(n)?.fill(0);
            return Ok(n);
        }
        self.n_pages += 1;
        // The page holding the bytes at 1 GiB is SQLite's lock-byte page: never used.
        if u64::from(self.n_pages) == 0x4000_0000 / self.page_size as u64 + 1 {
            self.n_pages += 1;
        }
        let n = self.n_pages;
        self.cache.insert(n, vec![0; self.page_size]);
        self.dirty.insert(n);
        Ok(n)
    }

    fn page_mut(&mut self, n: u32) -> Result<&mut [u8]> {
        self.load(n)?;
        if n <= self.orig_pages && !self.originals.contains_key(&n) {
            self.originals.insert(n, self.cache[&n].clone());
        }
        self.dirty.insert(n);
        Ok(self.cache.get_mut(&n).expect("the page is loaded"))
    }
}

impl Editor {
    /// Opens a database file for changes, after playing back a hot journal a stopped update
    /// left. `Ok(Err(..))` when the file is one this editor does not change in place (a
    /// write-ahead log, auto-vacuum, UTF-16 text or reserved page bytes), or on a platform
    /// where it does not take SQLite's locks.
    pub(crate) fn open(path: &Path) -> Result<std::result::Result<Self, NotInPlace>> {
        Self::open_mode(path, false)
    }

    /// Opens a full-image rewrite. Encoded-record rebuilding supports UTF-16 and pointer maps.
    pub(crate) fn open_repack(path: &Path) -> Result<std::result::Result<Self, NotInPlace>> {
        Self::open_mode(path, true)
    }

    fn open_mode(path: &Path, repack: bool) -> Result<std::result::Result<Self, NotInPlace>> {
        if !cfg!(all(unix, not(target_os = "emscripten"))) && !(repack && cfg!(windows)) {
            // Without SQLite's locks a reader could see a half-written file.
            return Ok(Err(NotInPlace("in-place changes need SQLite's file locks, not taken on this platform".into())));
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true);
        #[cfg(unix)]
        if repack {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        #[cfg(windows)]
        if repack {
            use std::os::windows::fs::OpenOptionsExt;
            options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
        }
        let mut file = options.open(path)?;
        let metadata = file.metadata()?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() > 1 {
                return Err(bad("database handle has hard-link aliases"));
            }
        }
        if repack {
            if !metadata.is_file() {
                return Err(bad("repack handle is not a regular file"));
            }
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawHandle;
                use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
                let mut information = BY_HANDLE_FILE_INFORMATION::default();
                // SAFETY: the open database handle and output structure are valid.
                if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                if information.nNumberOfLinks > 1 {
                    return Err(bad("repack handle has hard-link aliases"));
                }
            }
        }
        #[cfg(any(all(unix, not(target_os = "emscripten")), windows))]
        lock(&file, path)?;
        {
            for suffix in ["-journal", "-wal", "-shm"] {
                let sidecar = with_suffix(path, suffix);
                match std::fs::symlink_metadata(&sidecar) {
                    Ok(metadata) => {
                        if !metadata.file_type().is_file() {
                            return Err(bad(format!(
                                "{}: sidecar must be a regular file, not a symbolic link or special file",
                                sidecar.display()
                            )));
                        }
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::MetadataExt;
                            if metadata.nlink() > 1 {
                                return Err(bad(format!("{}: sidecar has hard-link aliases", sidecar.display())));
                            }
                        }
                        #[cfg(windows)]
                        {
                            use std::os::windows::io::AsRawHandle;
                            use windows_sys::Win32::Storage::FileSystem::{BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle};
                            let sidecar_file = File::open(&sidecar)?;
                            let mut information = BY_HANDLE_FILE_INFORMATION::default();
                            // SAFETY: both the open handle and the output structure are valid.
                            if unsafe { GetFileInformationByHandle(sidecar_file.as_raw_handle(), &mut information) } == 0 {
                                return Err(std::io::Error::last_os_error().into());
                            }
                            if information.nNumberOfLinks > 1 {
                                return Err(bad(format!("{}: sidecar has hard-link aliases", sidecar.display())));
                            }
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                    Err(error) => return Err(error.into()),
                }
            }
        }
        ensure_path_identity(&file, path)?;
        let wal = with_suffix(path, "-wal");
        if std::fs::metadata(&wal).is_ok_and(|m| m.len() > 0) {
            return Ok(Err(NotInPlace("the database has a write-ahead log".into())));
        }
        recover(&mut file, path)?;
        let mut header = [0u8; 100];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut header).map_err(|_| bad("not an SQLite database"))?;
        if !header.starts_with(b"SQLite format 3\0") {
            return Err(bad("not an SQLite database"));
        }
        let page_size = match u16::from_be_bytes([header[16], header[17]]) {
            1 => 65536,
            p => p as usize,
        };
        if !(512..=65536).contains(&page_size) || !page_size.is_power_of_two() {
            return Err(bad("invalid page size"));
        }
        let be = |o: usize| u32::from_be_bytes([header[o], header[o + 1], header[o + 2], header[o + 3]]);
        let refuse = |why: &str| Ok(Err(NotInPlace(why.into())));
        if header[20] != 0 {
            return refuse("pages have reserved bytes");
        }
        if !repack && be(52) != 0 {
            return refuse("the database uses auto-vacuum");
        }
        if !repack && be(56) > 1 {
            return refuse("the database is UTF-16");
        }
        if header[18] > 2 || header[19] > 2 {
            return refuse("unknown file format version");
        }
        let file_pages = (file.metadata()?.len() / page_size as u64) as u32;
        // The in-header size is valid when the version-valid-for number matches the change counter.
        let n_pages = if be(28) > 0 && be(92) == be(24) { be(28) } else { file_pages };
        Ok(Ok(Self {
            file,
            path: path.to_path_buf(),
            page_size,
            orig_pages: n_pages,
            n_pages,
            cache: HashMap::new(),
            originals: BTreeMap::new(),
            dirty: BTreeSet::new(),
            schema_changed: false,
            rewrite: None,
        }))
    }

    /// Snapshot through the already locked handle. Opening/closing another handle to this
    /// inode would release process-associated POSIX locks on Unix.
    pub(crate) fn image(&mut self) -> Result<Vec<u8>> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut image = Vec::new();
        self.file.read_to_end(&mut image)?;
        Ok(image)
    }

    /// Install a rebuilt image through the normal rollback-journal transaction. Truncated
    /// original pages must be journaled too, so interruption can restore the complete file.
    /// `old` must be the original snapshot from this editor's locked handle (`image`).
    pub(crate) fn replace_image(&mut self, old: Vec<u8>, image: Vec<u8>) -> Result<()> {
        if image.is_empty() || !image.len().is_multiple_of(self.page_size) || old.len() != self.orig_pages as usize * self.page_size {
            return Err(bad("replacement image size"));
        }
        if !self.dirty.is_empty() || self.rewrite.is_some() {
            return Err(bad("full-image replacement requires a fresh editor"));
        }
        self.n_pages = u32::try_from(image.len() / self.page_size).map_err(|_| bad("replacement page count"))?;
        self.cache.clear();
        self.rewrite = Some(FullRewrite { original: old, image });
        Ok(())
    }

    fn load(&mut self, n: u32) -> Result<()> {
        if n == 0 || n > self.n_pages {
            return Err(bad(format!("page {n} out of range")));
        }
        if !self.cache.contains_key(&n) {
            // The aligned run of pages around it, in one read: pages of a b-tree written together
            // are often next to each other, and reads from disk cost per request.
            let first = (n - 1) / READ_PAGES * READ_PAGES + 1;
            let last = (first + READ_PAGES - 1).min(self.orig_pages.max(n));
            let mut buf = vec![0u8; (last - first + 1) as usize * self.page_size];
            self.file.seek(SeekFrom::Start((first as u64 - 1) * self.page_size as u64))?;
            let mut got = 0;
            while got < buf.len() {
                match self.file.read(&mut buf[got..])? {
                    0 => break,
                    k => got += k,
                }
            }
            for (k, page) in buf.chunks(self.page_size).enumerate() {
                self.cache.entry(first + k as u32).or_insert_with(|| page.to_vec());
            }
        }
        Ok(())
    }

    /// A page, read when needed.
    pub(crate) fn page(&mut self, n: u32) -> Result<&[u8]> {
        self.load(n)?;
        Ok(&self.cache[&n])
    }

    pub(crate) fn header_u32(&mut self, offset: usize) -> Result<u32> {
        let p = self.page(1)?;
        Ok(u32::from_be_bytes([p[offset], p[offset + 1], p[offset + 2], p[offset + 3]]))
    }

    fn set_header_u32(&mut self, offset: usize, v: u32) -> Result<()> {
        self.page_mut(1)?[offset..offset + 4].copy_from_slice(&v.to_be_bytes());
        Ok(())
    }

    /// Puts a page on the freelist.
    pub(crate) fn free(&mut self, n: u32) -> Result<()> {
        let (trunk, count) = (self.header_u32(32)?, self.header_u32(36)?);
        // SQLite fills trunks to usable/4 - 8 leaves, for compatibility with old versions.
        let max_leaves = self.page_size / 4 - 8;
        if trunk > 0 {
            let leaves = {
                let t = self.page(trunk)?;
                u32::from_be_bytes([t[4], t[5], t[6], t[7]]) as usize
            };
            if leaves < max_leaves {
                let t = self.page_mut(trunk)?;
                let at = 8 + 4 * leaves;
                t[at..at + 4].copy_from_slice(&n.to_be_bytes());
                t[4..8].copy_from_slice(&((leaves + 1) as u32).to_be_bytes());
                self.set_header_u32(36, count + 1)?;
                return Ok(());
            }
        }
        // A new trunk, ahead of the others.
        let p = self.page_mut(n)?;
        p.fill(0);
        p[..4].copy_from_slice(&trunk.to_be_bytes());
        self.set_header_u32(32, n)?;
        self.set_header_u32(36, count + 1)
    }

    fn header_offset(n: u32) -> usize {
        if n == 1 { 100 } else { 0 }
    }

    /// The length of a payload's local part in a cell of a page of type `kind`.
    fn local_len(&self, kind: u8, size: usize) -> usize {
        let (max_local, min_local) =
            if kind == 0x0d { w::table_local_limits(self.page_size) } else { w::index_local_limits(self.page_size) };
        if size <= max_local {
            return size;
        }
        let k = min_local + (size - min_local) % (self.page_size - 4);
        if k <= max_local { k } else { min_local }
    }

    /// Parses a b-tree page.
    pub(crate) fn node(&mut self, n: u32) -> Result<Node> {
        let h = Self::header_offset(n);
        let size = self.page_size;
        self.load(n)?;
        let page = &self.cache[&n];
        let kind = page[h];
        let interior = matches!(kind, 0x02 | 0x05);
        if !matches!(kind, 0x02 | 0x05 | 0x0a | 0x0d) {
            return Err(bad(format!("page {n} is not a b-tree page")));
        }
        let count = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
        let header = if interior { 12 } else { 8 };
        let right = interior.then(|| u32::from_be_bytes([page[h + 8], page[h + 9], page[h + 10], page[h + 11]]));
        let mut cells = Vec::with_capacity(count);
        for c in 0..count {
            let p = h + header + 2 * c;
            let off = u16::from_be_bytes([page[p], page[p + 1]]) as usize;
            if off >= size {
                return Err(bad("cell offset"));
            }
            let len = match kind {
                0x05 => 4 + varint(page, off + 4).ok_or_else(|| bad("cell"))?.1,
                0x0d => {
                    let (sz, n1) = varint(page, off).ok_or_else(|| bad("cell"))?;
                    let (_, n2) = varint(page, off + n1).ok_or_else(|| bad("cell"))?;
                    let local = self.local_len(kind, sz as usize);
                    n1 + n2 + local + if local < sz as usize { 4 } else { 0 }
                }
                _ => {
                    let start = if kind == 0x02 { off + 4 } else { off };
                    let (sz, n1) = varint(page, start).ok_or_else(|| bad("cell"))?;
                    let local = self.local_len(kind, sz as usize);
                    (start - off) + n1 + local + if local < sz as usize { 4 } else { 0 }
                }
            };
            cells.push(page.get(off..off + len).ok_or_else(|| bad("cell"))?.to_vec());
        }
        Ok(Node { kind, cells, right })
    }

    /// The rowid of a table cell (leaf) or its key (interior).
    pub(crate) fn cell_key(kind: u8, cell: &[u8]) -> Result<i64> {
        match kind {
            0x05 => varint(cell, 4).map(|v| v.0).ok_or_else(|| bad("cell")),
            _ => {
                let (_, n1) = varint(cell, 0).ok_or_else(|| bad("cell"))?;
                varint(cell, n1).map(|v| v.0).ok_or_else(|| bad("cell"))
            }
        }
    }

    fn child(cell: &[u8]) -> u32 {
        u32::from_be_bytes([cell[0], cell[1], cell[2], cell[3]])
    }

    /// The payload size, where the local payload starts, and the first overflow page of a cell.
    fn cell_payload_parts(&self, kind: u8, cell: &[u8]) -> Result<(usize, usize, Option<u32>)> {
        let mut at = if kind == 0x02 { 4 } else { 0 };
        let (sz, n1) = varint(cell, at).ok_or_else(|| bad("cell"))?;
        at += n1;
        if kind == 0x0d {
            at += varint(cell, at).ok_or_else(|| bad("cell"))?.1;
        }
        let local = self.local_len(kind, sz as usize);
        let overflow = (local < sz as usize).then(|| {
            let o = cell.len() - 4;
            u32::from_be_bytes([cell[o], cell[o + 1], cell[o + 2], cell[o + 3]])
        });
        Ok((sz as usize, at, overflow))
    }

    /// The payload of a leaf or index cell, following overflow pages.
    pub(crate) fn cell_payload(&mut self, kind: u8, cell: &[u8]) -> Result<Vec<u8>> {
        let (size, at, overflow) = self.cell_payload_parts(kind, cell)?;
        let local = self.local_len(kind, size);
        let mut out = cell.get(at..at + local).ok_or_else(|| bad("payload"))?.to_vec();
        let mut next = overflow.unwrap_or(0);
        let mut guard = 0;
        while out.len() < size && next != 0 {
            guard += 1;
            if guard > self.n_pages as usize {
                return Err(bad("overflow chain loops"));
            }
            let usable = self.page_size;
            let p = self.page(next)?;
            let take = (size - out.len()).min(usable - 4);
            let following = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
            out.extend_from_slice(&p[4..4 + take]);
            next = following;
        }
        if out.len() < size {
            return Err(bad("overflow chain too short"));
        }
        Ok(out)
    }

    /// The overflow pages of a cell.
    fn overflow_pages(&mut self, kind: u8, cell: &[u8], out: &mut Vec<u32>) -> Result<()> {
        let (size, _, overflow) = self.cell_payload_parts(kind, cell)?;
        let mut next = overflow.unwrap_or(0);
        let local = self.local_len(kind, size);
        let mut left = size - local;
        while next != 0 && left > 0 {
            out.push(next);
            let p = self.page(next)?;
            next = u32::from_be_bytes([p[0], p[1], p[2], p[3]]);
            left = left.saturating_sub(self.page_size - 4);
        }
        Ok(())
    }

    /// Every page of the b-tree rooted at `root`: its nodes and overflow pages.
    pub(crate) fn tree_pages(&mut self, root: u32, out: &mut Vec<u32>) -> Result<()> {
        let mut stack = vec![(root, 0usize)];
        while let Some((n, depth)) = stack.pop() {
            if depth > 64 {
                return Err(bad("b-tree too deep"));
            }
            out.push(n);
            let node = self.node(n)?;
            for c in &node.cells {
                if matches!(node.kind, 0x02 | 0x05) {
                    stack.push((Self::child(c), depth + 1));
                }
                if node.kind != 0x05 {
                    self.overflow_pages(node.kind, c, out)?;
                }
            }
            if let Some(r) = node.right {
                stack.push((r, depth + 1));
            }
        }
        Ok(())
    }

    /// Frees the pages of a b-tree; with `keep_root`, the root stays (to be rewritten).
    pub(crate) fn free_tree(&mut self, root: u32, keep_root: bool) -> Result<()> {
        let mut pages = Vec::new();
        self.tree_pages(root, &mut pages)?;
        for p in pages {
            if !(keep_root && p == root) {
                self.free(p)?;
            }
        }
        Ok(())
    }

    /// Every row of a table b-tree: `(rowid, payload)`.
    pub(crate) fn scan(&mut self, root: u32) -> Result<Vec<(i64, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut stack = vec![(root, 0usize)];
        while let Some((n, depth)) = stack.pop() {
            if depth > 64 {
                return Err(bad("b-tree too deep"));
            }
            let node = self.node(n)?;
            match node.kind {
                0x05 => {
                    // Children pushed in reverse, so that they are visited in order.
                    if let Some(r) = node.right {
                        stack.push((r, depth + 1));
                    }
                    for c in node.cells.iter().rev() {
                        stack.push((Self::child(c), depth + 1));
                    }
                }
                0x0d => {
                    for c in &node.cells {
                        out.push((Self::cell_key(0x0d, c)?, self.cell_payload(0x0d, c)?));
                    }
                }
                _ => return Err(bad("not a table b-tree")),
            }
        }
        Ok(out)
    }

    /// The payload of row `rowid` of a table b-tree.
    pub(crate) fn lookup(&mut self, root: u32, rowid: i64) -> Result<Option<Vec<u8>>> {
        let mut n = root;
        for _ in 0..64 {
            self.load(n)?;
            let h = Self::header_offset(n);
            let page = &self.cache[&n];
            let kind = page[h];
            let count = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
            let header = if kind == 0x05 { 12 } else { 8 };
            let cell = |c: usize| -> Result<usize> {
                let p = h + header + 2 * c;
                let off = u16::from_be_bytes([page[p], page[p + 1]]) as usize;
                if off >= page.len() { Err(bad("cell offset")) } else { Ok(off) }
            };
            // A binary search of the cells' keys, read in place.
            let key = |c: usize| -> Result<i64> {
                let off = cell(c)?;
                if kind == 0x05 {
                    return varint(page, off + 4).map(|v| v.0).ok_or_else(|| bad("cell"));
                }
                let (_, n1) = varint(page, off).ok_or_else(|| bad("cell"))?;
                varint(page, off + n1).map(|v| v.0).ok_or_else(|| bad("cell"))
            };
            let (mut lo, mut hi) = (0, count);
            while lo < hi {
                let mid = (lo + hi) / 2;
                if key(mid)? < rowid {
                    lo = mid + 1;
                } else {
                    hi = mid;
                }
            }
            match kind {
                0x05 => {
                    n = if lo < count {
                        let off = cell(lo)?;
                        Self::child(&page[off..off + 4])
                    } else {
                        u32::from_be_bytes([page[h + 8], page[h + 9], page[h + 10], page[h + 11]])
                    };
                }
                0x0d => {
                    if lo >= count || key(lo)? != rowid {
                        return Ok(None);
                    }
                    let off = cell(lo)?;
                    let (sz, n1) = varint(page, off).ok_or_else(|| bad("cell"))?;
                    let (_, n2) = varint(page, off + n1).ok_or_else(|| bad("cell"))?;
                    let local = self.local_len(0x0d, sz as usize);
                    let len = n1 + n2 + local + if local < sz as usize { 4 } else { 0 };
                    let c = page.get(off..off + len).ok_or_else(|| bad("cell"))?.to_vec();
                    return self.cell_payload(0x0d, &c).map(Some);
                }
                _ => return Err(bad("not a table b-tree")),
            }
        }
        Err(bad("b-tree too deep"))
    }

    /// The index of the first cell whose key is at least `rowid`.
    fn position(node: &Node, rowid: i64) -> Result<usize> {
        let (mut lo, mut hi) = (0, node.cells.len());
        while lo < hi {
            let mid = (lo + hi) / 2;
            if Self::cell_key(node.kind, &node.cells[mid])? < rowid {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        Ok(lo)
    }

    /// The largest rowid of a table b-tree (0 when it is empty).
    pub(crate) fn max_rowid(&mut self, root: u32) -> Result<i64> {
        let (_, leaf) = self.rightmost(root)?;
        let node = self.node(leaf)?;
        node.cells.last().map_or(Ok(0), |c| Self::cell_key(0x0d, c))
    }

    /// The number of rows of a table b-tree, from its leaves' cell counts.
    pub(crate) fn count_rows(&mut self, root: u32) -> Result<u64> {
        let mut total = 0u64;
        let mut stack = vec![(root, 0usize)];
        while let Some((n, depth)) = stack.pop() {
            if depth > 64 {
                return Err(bad("b-tree too deep"));
            }
            let h = Self::header_offset(n);
            let page = self.page(n)?;
            let count = u16::from_be_bytes([page[h + 3], page[h + 4]]) as usize;
            match page[h] {
                0x0d => total += count as u64,
                0x05 => {
                    let right = u32::from_be_bytes([page[h + 8], page[h + 9], page[h + 10], page[h + 11]]);
                    let mut kids = Vec::with_capacity(count + 1);
                    for c in 0..count {
                        let p = h + 12 + 2 * c;
                        let off = u16::from_be_bytes([page[p], page[p + 1]]) as usize;
                        kids.push(Self::child(&page[off..off + 4]));
                    }
                    kids.push(right);
                    stack.extend(kids.into_iter().map(|k| (k, depth + 1)));
                }
                _ => return Err(bad("not a table b-tree")),
            }
        }
        Ok(total)
    }

    /// The path of interior pages down the right-most edge (each with the child index taken,
    /// the cell count meaning the right-most pointer) and the right-most leaf.
    fn rightmost(&mut self, root: u32) -> Result<(Vec<(u32, usize)>, u32)> {
        let mut path = Vec::new();
        let mut n = root;
        for _ in 0..64 {
            let node = self.node(n)?;
            match node.kind {
                0x05 => {
                    path.push((n, node.cells.len()));
                    n = node.right.ok_or_else(|| bad("interior page without right pointer"))?;
                }
                0x0d => return Ok((path, n)),
                _ => return Err(bad("not a table b-tree")),
            }
        }
        Err(bad("b-tree too deep"))
    }

    /// The bytes a node's cells need on page `n`.
    fn fits(&self, n: u32, interior: bool, cells: &[Vec<u8>]) -> bool {
        let header = Self::header_offset(n) + if interior { 12 } else { 8 };
        header + cells.iter().map(|c| c.len() + 2).sum::<usize>() <= self.page_size
    }

    /// Writes a node to page `n`; when its cells do not fit, splits it (into new pages to its
    /// right), adding the new children to the parent at the end of `path`, as far up as needed:
    /// a root that splits gets a new level below it, keeping its page.
    fn store(&mut self, path: &[(u32, usize)], n: u32, kind: u8, cells: Vec<Vec<u8>>, right: Option<u32>) -> Result<()> {
        let interior = kind == 0x05;
        if self.fits(n, interior, &cells) {
            return w::write_node(self, n, kind, &cells, right);
        }
        // Groups that fit a (non-root) page, filled in order, with the dividers between them.
        let header = if interior { 12 } else { 8 };
        let mut groups: Vec<(Vec<Vec<u8>>, Option<u32>)> = Vec::new();
        let mut dividers: Vec<i64> = Vec::new();
        let mut current: Vec<Vec<u8>> = Vec::new();
        let mut used = header;
        let mut iter = cells.into_iter().peekable();
        while let Some(c) = iter.next() {
            if current.is_empty() || used + c.len() + 2 <= self.page_size {
                used += c.len() + 2;
                current.push(c);
                continue;
            }
            if interior {
                // The cell becomes the divider: its child is the group's right-most pointer.
                // A divider taken from the last cell would leave the next group with none;
                // the group's own last cell is the divider then.
                if iter.peek().is_none() && current.len() > 1 {
                    let last = current.pop().ok_or_else(|| bad("split"))?;
                    dividers.push(Self::cell_key(kind, &last)?);
                    groups.push((std::mem::take(&mut current), Some(Self::child(&last))));
                    used = header + c.len() + 2;
                    current.push(c);
                } else {
                    dividers.push(Self::cell_key(kind, &c)?);
                    groups.push((std::mem::take(&mut current), Some(Self::child(&c))));
                    used = header;
                }
            } else {
                dividers.push(Self::cell_key(kind, current.last().ok_or_else(|| bad("split"))?)?);
                groups.push((std::mem::take(&mut current), None));
                used = header + c.len() + 2;
                current.push(c);
            }
        }
        groups.push((current, right));

        match path.split_last() {
            None => {
                // The root: its content moves to new pages, and it becomes their parent.
                let mut pages = Vec::with_capacity(groups.len());
                for (g, r) in &groups {
                    let p = self.alloc()?;
                    w::write_node(self, p, kind, g, *r)?;
                    pages.push(p);
                }
                let mut root_cells = Vec::with_capacity(dividers.len());
                for (p, d) in pages.iter().zip(&dividers) {
                    let mut c = p.to_be_bytes().to_vec();
                    w::varint(&mut c, *d as u64);
                    root_cells.push(c);
                }
                w::write_node(self, n, 0x05, &root_cells, pages.last().copied())
            }
            Some((&(parent, index), up)) => {
                let mut pages = vec![n];
                for _ in 1..groups.len() {
                    pages.push(self.alloc()?);
                }
                for ((g, r), p) in groups.iter().zip(&pages) {
                    w::write_node(self, *p, kind, g, *r)?;
                }
                let mut pnode = self.node(parent)?;
                // The pointer that led to `n` now leads to the last group.
                let last = *pages.last().expect("one group at least");
                if index < pnode.cells.len() {
                    pnode.cells[index][..4].copy_from_slice(&last.to_be_bytes());
                } else {
                    pnode.right = Some(last);
                }
                let new_cells: Vec<Vec<u8>> = pages
                    .iter()
                    .zip(&dividers)
                    .map(|(p, d)| {
                        let mut c = p.to_be_bytes().to_vec();
                        w::varint(&mut c, *d as u64);
                        c
                    })
                    .collect();
                pnode.cells.splice(index..index, new_cells);
                self.store(up, parent, 0x05, pnode.cells, pnode.right)
            }
        }
    }

    /// Inserts rows into a table b-tree, replacing those with the same rowids: rows sorted by
    /// rowid, each leaf rewritten once for all the rows that fall in it.
    pub(crate) fn upsert(&mut self, root: u32, rows: Vec<(i64, Vec<u8>)>) -> Result<()> {
        if rows.windows(2).any(|w| w[0].0 >= w[1].0) {
            return Err(Error::InvalidArgument("rows not sorted by rowid".into()));
        }
        let mut rows = rows.into_iter().peekable();
        while let Some(&(first, _)) = rows.peek() {
            // Down to the leaf holding `first`, with the largest key it may hold.
            let mut path = Vec::new();
            let mut n = root;
            let mut bound = i64::MAX;
            let node = loop {
                let node = self.node(n)?;
                match node.kind {
                    0x05 => {
                        let i = Self::position(&node, first)?;
                        path.push((n, i));
                        if i < node.cells.len() {
                            bound = Self::cell_key(0x05, &node.cells[i])?;
                            n = Self::child(&node.cells[i]);
                        } else {
                            n = node.right.unwrap_or(0);
                        }
                        if path.len() > 64 {
                            return Err(bad("b-tree too deep"));
                        }
                    }
                    0x0d => break node,
                    _ => return Err(bad("not a table b-tree")),
                }
            };
            let mut cells = node.cells;
            let mut keys: Vec<i64> = cells.iter().map(|c| Self::cell_key(0x0d, c)).collect::<Result<_>>()?;
            while let Some((rowid, rec)) = rows.next_if(|(k, _)| *k <= bound) {
                let i = keys.partition_point(|k| *k < rowid);
                let cell = w::table_cell(self, rowid, &rec)?;
                if keys.get(i) == Some(&rowid) {
                    let mut freed = Vec::new();
                    self.overflow_pages(0x0d, &cells[i], &mut freed)?;
                    for p in freed {
                        self.free(p)?;
                    }
                    cells[i] = cell;
                } else {
                    cells.insert(i, cell);
                    keys.insert(i, rowid);
                }
            }
            self.store(&path, n, 0x0d, cells, None)?;
        }
        Ok(())
    }

    /// Inserts a row into a table b-tree, replacing the row with the same rowid.
    #[cfg(all(test, unix, not(target_os = "emscripten")))]
    pub(crate) fn insert(&mut self, root: u32, rowid: i64, record: &[u8]) -> Result<()> {
        self.upsert(root, vec![(rowid, record.to_vec())])
    }

    /// Appends rows to a table b-tree, by increasing rowids larger than its largest: the
    /// right-most leaf is filled, then new leaves are started, as SQLite's `balance_quick` does.
    pub(crate) fn append(&mut self, root: u32, rows: impl IntoIterator<Item = (i64, Vec<u8>)>) -> Result<()> {
        let (mut path, mut leaf) = self.rightmost(root)?;
        let mut cells = self.node(leaf)?.cells;
        let mut last = cells.last().map(|c| Self::cell_key(0x0d, c)).transpose()?;
        let mut any = false;
        for (rowid, rec) in rows {
            any = true;
            if last.is_some_and(|l| rowid <= l) {
                return Err(Error::InvalidArgument(format!("rows appended out of order: rowid {rowid}")));
            }
            last = Some(rowid);
            let cell = w::table_cell(self, rowid, &rec)?;
            cells.push(cell);
            if !self.fits(leaf, false, &cells) {
                self.store(&path, leaf, 0x0d, std::mem::take(&mut cells), None)?;
                (path, leaf) = self.rightmost(root)?;
                cells = self.node(leaf)?.cells;
            }
        }
        if !any {
            return Ok(());
        }
        w::write_node(self, leaf, 0x0d, &cells, None)
    }

    /// Replaces the rows of a table b-tree, keeping its root page.
    pub(crate) fn rebuild_table(&mut self, root: u32, rows: &[(i64, Vec<w::Value>)]) -> Result<()> {
        self.free_tree(root, true)?;
        let encoded: Vec<(i64, Vec<u8>)> = rows.iter().map(|(id, v)| (*id, w::record(v))).collect();
        w::build_table(self, &encoded, Some(root))?;
        Ok(())
    }

    /// Builds a new table b-tree of rows and returns its root page.
    pub(crate) fn new_table(&mut self, rows: &[(i64, Vec<w::Value>)]) -> Result<u32> {
        let encoded: Vec<(i64, Vec<u8>)> = rows.iter().map(|(id, v)| (*id, w::record(v))).collect();
        w::build_table(self, &encoded, None)
    }

    /// Replaces the keys of an index b-tree, keeping its root page.
    pub(crate) fn rebuild_index(&mut self, root: u32, keys: &[Vec<w::Value>]) -> Result<()> {
        self.free_tree(root, true)?;
        let mut spare = Vec::new();
        w::build_index(self, keys, Some(root), &mut spare)?;
        for p in spare {
            self.free(p)?;
        }
        Ok(())
    }

    /// Builds a new index b-tree and returns its root page.
    pub(crate) fn new_index(&mut self, keys: &[Vec<w::Value>]) -> Result<u32> {
        let mut spare = Vec::new();
        w::build_index(self, keys, None, &mut spare)
    }

    /// Writes the changes, as one transaction protected by a rollback journal.
    pub(crate) fn commit(self) -> Result<()> {
        self.commit_until(Stage::Done)
    }

    fn commit_until(self, stop: Stage) -> Result<()> {
        self.commit_with_sync(stop, sync_dir)
    }

    fn commit_with_sync(mut self, stop: Stage, mut directory_sync: impl FnMut(&Path) -> std::io::Result<()>) -> Result<()> {
        let mut rewrite = self.rewrite.take();
        if self.dirty.is_empty() && rewrite.is_none() {
            return Ok(());
        }
        // Header updates are made in the owned replacement or in the ordinary page cache.
        if let Some(full) = rewrite.as_mut() {
            let header = &mut full.image[..self.page_size];
            let counter = u32::from_be_bytes(header[24..28].try_into().unwrap()).wrapping_add(1);
            header[24..28].copy_from_slice(&counter.to_be_bytes());
            header[92..96].copy_from_slice(&counter.to_be_bytes());
            header[28..32].copy_from_slice(&self.n_pages.to_be_bytes());
            header[96..100].copy_from_slice(&3_045_000u32.to_be_bytes());
        } else {
            let counter = self.header_u32(24)?.wrapping_add(1);
            self.set_header_u32(24, counter)?;
            self.set_header_u32(92, counter)?;
            self.set_header_u32(28, self.n_pages)?;
            self.set_header_u32(96, 3_045_000)?;
            if self.schema_changed {
                let cookie = self.header_u32(40)?.wrapping_add(1);
                self.set_header_u32(40, cookie)?;
            }
        }

        ensure_path_identity(&self.file, &self.path)?;

        // 1. The journal: the original pages, synced, then its record count, synced.
        let journal_path = with_suffix(&self.path, "-journal");
        let nonce = nonce();
        // Under the database's exclusive lock, any remaining entry is cold/stale. Unlink
        // the entry rather than opening its target; create_new also closes the subsequent race.
        match std::fs::remove_file(&journal_path) {
            Ok(()) => (),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        let mut journal_options = OpenOptions::new();
        journal_options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
            journal_options.mode(self.file.metadata()?.mode() & 0o777);
        }
        let mut journal = journal_options.open(&journal_path)?;
        let mut header = vec![0u8; SECTOR];
        header[..8].copy_from_slice(&JOURNAL_MAGIC);
        header[12..16].copy_from_slice(&nonce.to_be_bytes());
        header[16..20].copy_from_slice(&self.orig_pages.to_be_bytes());
        header[20..24].copy_from_slice(&(SECTOR as u32).to_be_bytes());
        header[24..28].copy_from_slice(&(self.page_size as u32).to_be_bytes());
        let mut buf = header.clone();
        let mut records = 0u32;
        {
            let mut append = |number: u32, data: &[u8]| -> Result<()> {
                buf.extend_from_slice(&number.to_be_bytes());
                buf.extend_from_slice(data);
                buf.extend_from_slice(&checksum(nonce, data).to_be_bytes());
                records += 1;
                if buf.len() > 1 << 24 {
                    journal.write_all(&buf)?;
                    buf.clear();
                }
                Ok(())
            };
            if let Some(full) = rewrite.as_ref() {
                for (i, page) in full.original.chunks_exact(self.page_size).enumerate() {
                    let number = i as u32 + 1;
                    // The full-image write can tear even an unchanged data page.
                    // Journal every original page except SQLite's unused lock-byte
                    // page, whose number is a super-journal marker in this format.
                    if u64::from(number) != PENDING_BYTE as u64 / self.page_size as u64 + 1 {
                        append(number, page)?;
                    }
                }
            } else {
                for (number, page) in &self.originals {
                    append(*number, page)?;
                }
            }
        }
        journal.write_all(&buf)?;
        journal.sync_all()?;
        journal.seek(SeekFrom::Start(8))?;
        journal.write_all(&records.to_be_bytes())?;
        journal.sync_all()?;
        directory_sync(&self.path)?;
        if stop == Stage::Journal {
            return Ok(());
        }

        // 2. The database pages.
        // Once originals are durable in the journal, release that full buffer before writes.
        let image = rewrite.map(|full| full.image);
        if let Some(image) = image {
            self.file.seek(SeekFrom::Start(0))?;
            let end = if stop == Stage::HalfWritten { self.n_pages.div_ceil(2) as usize * self.page_size } else { image.len() };
            self.file.write_all(&image[..end])?;
            if stop == Stage::HalfWritten {
                self.file.sync_all()?;
                return Ok(());
            }
        } else {
            let pages: Vec<u32> = self.dirty.iter().copied().collect();
            let mut run: Vec<u8> = Vec::new();
            let mut run_start = 0u32;
            for (k, n) in pages.iter().enumerate() {
                if run.is_empty() {
                    run_start = *n;
                }
                run.extend_from_slice(&self.cache[n]);
                let contiguous = pages.get(k + 1).is_some_and(|m| *m == n + 1);
                if !contiguous || run.len() >= 1 << 24 {
                    self.file.seek(SeekFrom::Start((run_start as u64 - 1) * self.page_size as u64))?;
                    self.file.write_all(&run)?;
                    run.clear();
                }
                if stop == Stage::HalfWritten && k >= pages.len() / 2 {
                    self.file.sync_all()?;
                    return Ok(());
                }
            }
        }
        self.file.set_len(self.n_pages as u64 * self.page_size as u64)?;
        self.file.sync_all()?;
        if stop == Stage::Written {
            return Ok(());
        }

        // 3. Deleting the journal commits.
        drop(journal);
        std::fs::remove_file(&journal_path)?;
        directory_sync(&self.path).map_err(|error| Error::Io(std::io::Error::new(error.kind(), format!(
            "SQLite changes committed, but directory synchronization failed; durability is unconfirmed; do not retry the change automatically: {error}"
        ))))?;
        Ok(())
    }
}

/// Where a commit stops, to test recovery from a process stopped during one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Journal,
    HalfWritten,
    Written,
    Done,
}

/// The journal's checksum of a page, as SQLite computes it: the nonce plus every 200th byte.
fn checksum(nonce: u32, data: &[u8]) -> u32 {
    let mut sum = nonce;
    let mut i = data.len() as isize - 200;
    while i > 0 {
        sum = sum.wrapping_add(u32::from(data[i as usize]));
        i -= 200;
    }
    sum
}

fn nonce() -> u32 {
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    (t as u32) ^ ((t >> 32) as u32) ^ std::process::id()
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Refuse a pathname rebound after the database handle was locked. On Unix this uses stat,
/// not a second open/close, which would release this process's POSIX locks on the inode.
fn ensure_path_identity(file: &File, path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(bad("database pathname no longer names a regular file"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let opened = file.metadata()?;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(bad("database pathname changed after opening"));
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, FILE_FLAG_OPEN_REPARSE_POINT, GetFileInformationByHandle,
        };
        let named = OpenOptions::new().read(true).custom_flags(FILE_FLAG_OPEN_REPARSE_POINT).open(path)?;
        let identity = |handle: &File| -> std::io::Result<(u32, u32, u32)> {
            let mut information = BY_HANDLE_FILE_INFORMATION::default();
            // SAFETY: the live file handle and writable output structure are valid.
            if unsafe { GetFileInformationByHandle(handle.as_raw_handle(), &mut information) } == 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok((information.dwVolumeSerialNumber, information.nFileIndexHigh, information.nFileIndexLow))
        };
        if identity(file)? != identity(&named)? {
            return Err(bad("database pathname changed after opening"));
        }
    }
    #[cfg(not(any(unix, windows)))]
    let _ = file;
    Ok(())
}

/// Syncs journal directory entries on Unix. Unsupported directory fsync is an error, so no
/// database pages are written without a durable journal. Windows has no std directory sync;
/// its transaction still flushes both file handles, with the platform's directory durability.
fn sync_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        File::open(dir)?.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// What a hot journal holds: see [`journal_pages`].
pub(crate) type JournalPages<'a> = (u32, usize, Vec<(u32, &'a [u8])>);

/// The pages a hot rollback journal holds: the database size before the interrupted
/// transaction (in pages), the page size, and the original pages, checksums verified. `None`
/// when the journal is empty or zeroed (not hot).
pub(crate) fn journal_pages(journal: &[u8]) -> Option<JournalPages<'_>> {
    if journal.len() < 28 || journal[..8] != JOURNAL_MAGIC {
        return None;
    }
    let be = |b: &[u8], o: usize| u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]);
    let sector = (be(journal, 20) as usize).clamp(SECTOR, 65536);
    let page_size = be(journal, 24) as usize;
    if !(512..=65536).contains(&page_size) {
        return None;
    }
    let original = be(journal, 16);
    let mut pages = Vec::new();
    let mut offset = 0;
    // Segments: a header (one sector), then its records.
    while offset + 28 <= journal.len() && journal[offset..offset + 8] == JOURNAL_MAGIC {
        let n_rec = be(journal, offset + 8);
        let nonce = be(journal, offset + 12);
        let mut at = offset + sector;
        let records = if n_rec == u32::MAX { (journal.len().saturating_sub(at)) / (page_size + 8) } else { n_rec as usize };
        for _ in 0..records {
            if at + page_size + 8 > journal.len() {
                break;
            }
            let pgno = be(journal, at);
            let data = &journal[at + 4..at + 4 + page_size];
            if be(journal, at + 4 + page_size) != checksum(nonce, data) {
                break;
            }
            if pgno > 0 && pgno <= original {
                pages.push((pgno, data));
            }
            at += page_size + 8;
        }
        if n_rec == 0 || n_rec == u32::MAX {
            break;
        }
        offset = at.div_ceil(sector) * sector;
    }
    Some((original, page_size, pages))
}

/// A database file's bytes as they were before a transaction a hot journal says was
/// interrupted (or is under way), without changing the file: what SQLite readers see.
pub(crate) fn read_consistent(path: &Path) -> Result<Vec<u8>> {
    let mut data = crate::par::read_file(path)?;
    if let Some(journal) = read_journal(path)?
        && let Some((original, page_size, pages)) = journal_pages(&journal)
    {
        // Pages are restored in reverse, so that the first image of a page journaled twice wins.
        for (pgno, page) in pages.iter().rev() {
            let start = (*pgno as usize - 1) * page_size;
            if data.len() < start + page_size {
                data.resize(start + page_size, 0);
            }
            data[start..start + page_size].copy_from_slice(page);
        }
        if original > 0 {
            data.truncate(original as usize * page_size);
        }
    }
    Ok(data)
}

/// The rollback journal going with a database file, `None` without one.
fn read_journal(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(with_suffix(path, "-journal")) {
        Ok(j) => Ok(Some(j)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A main-file-only rewrite cannot incorporate committed frames from a pending WAL.
pub(crate) fn ensure_no_wal(path: &Path) -> Result<()> {
    let wal = with_suffix(path, "-wal");
    match std::fs::symlink_metadata(&wal) {
        Ok(metadata) if !metadata.file_type().is_file() || metadata.len() > 0 => Err(Error::Unsupported(format!(
            "{}: the database has a write-ahead log; open it with SQLite to recover/checkpoint it first",
            path.display()
        ))),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

/// Whether a hot journal (one an interrupted transaction left) goes with the database file.
pub(crate) fn has_hot_journal(path: &Path) -> Result<bool> {
    Ok(read_journal(path)?.is_some_and(|j| journal_pages(&j).is_some()))
}

/// Opens a database as SQLite readers see it: mapped in place, or, when a hot journal goes with
/// it, read with the journal's original pages put back (see [`read_consistent`]).
pub(crate) fn open_consistent(path: &Path) -> Result<crate::drivers::sqlite::Db> {
    if has_hot_journal(path)? { crate::drivers::sqlite::Db::new(read_consistent(path)?) } else { crate::drivers::sqlite::Db::open(path) }
}

/// Plays back a hot journal left by an interrupted transaction: the original pages are written
/// back and the file truncated to its original size, then the journal is deleted.
fn recover(file: &mut File, path: &Path) -> Result<()> {
    recover_with_sync(file, path, sync_dir)
}

fn recover_with_sync(file: &mut File, path: &Path, mut sync_directory: impl FnMut(&Path) -> std::io::Result<()>) -> Result<()> {
    let journal_path = with_suffix(path, "-journal");
    let Some(journal) = read_journal(path)? else { return Ok(()) };
    let Some((original, page_size, pages)) = journal_pages(&journal) else {
        // An empty or zeroed journal is not hot.
        let _ = std::fs::remove_file(&journal_path);
        return Ok(());
    };
    // Refuse unsupported or inaccessible directory synchronization before replaying pages.
    sync_directory(path)?;
    for (pgno, data) in pages.iter().rev() {
        file.seek(SeekFrom::Start((*pgno as u64 - 1) * page_size as u64))?;
        file.write_all(data)?;
    }
    if original > 0 {
        file.set_len(original as u64 * page_size as u64)?;
    }
    file.sync_all()?;
    std::fs::remove_file(&journal_path)?;
    sync_directory(path)
        .map_err(|error| std::io::Error::new(error.kind(), format!("SQLite recovery completed but durability is unconfirmed: {error}")))?;
    Ok(())
}

/// Takes SQLite's exclusive lock on the file (POSIX advisory locks on the pending, reserved and
/// shared bytes), failing when another connection holds a lock.
#[cfg(all(unix, not(target_os = "emscripten")))]
fn lock(file: &File, path: &Path) -> Result<()> {
    use std::os::fd::AsRawFd;
    let fd = file.as_raw_fd();
    // The pending and reserved bytes, then the shared range: SQLite's EXCLUSIVE lock.
    for (start, len) in [(PENDING_BYTE, 2), (PENDING_BYTE + 2, 510)] {
        // SAFETY: an all-zero `flock` is a valid value; the fields set are plain integers.
        let mut fl: libc::flock = unsafe { std::mem::zeroed() };
        fl.l_type = libc::F_WRLCK as _;
        fl.l_whence = libc::SEEK_SET as _;
        fl.l_start = start as _;
        fl.l_len = len as _;
        // SAFETY: `fd` is an open file descriptor and `fl` a valid `flock`.
        let r = unsafe { libc::fcntl(fd, libc::F_SETLK, &fl) };
        if r != 0 {
            let error = std::io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(libc::EAGAIN) | Some(libc::EACCES)) {
                return Err(error.into());
            }
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("{}: the database is locked by another connection", path.display()),
            )));
        }
    }
    Ok(())
}

/// SQLite-compatible Windows byte-range locks, released when this file handle closes.
#[cfg(windows)]
fn lock(file: &File, path: &Path) -> Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY, LockFileEx};
    use windows_sys::Win32::System::IO::{OVERLAPPED, OVERLAPPED_0, OVERLAPPED_0_0};
    for (start, length) in [(PENDING_BYTE as u64, 2), (PENDING_BYTE as u64 + 2, 510)] {
        let mut overlap = OVERLAPPED {
            Anonymous: OVERLAPPED_0 { Anonymous: OVERLAPPED_0_0 { Offset: start as u32, OffsetHigh: (start >> 32) as u32 } },
            ..OVERLAPPED::default()
        };
        // SAFETY: the file handle is open and overlap points to a valid zero-initialized
        // OVERLAPPED with the synchronous byte range's offset set.
        let ok =
            unsafe { LockFileEx(file.as_raw_handle(), LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY, 0, length, 0, &mut overlap) };
        if ok == 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION as i32) {
                return Err(error.into());
            }
            return Err(Error::Io(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                format!("{}: the database is locked by another connection", path.display()),
            )));
        }
    }
    Ok(())
}

#[cfg(all(test, any(all(unix, not(target_os = "emscripten")), windows)))]
mod native_repack_tests {
    use super::*;

    pub(super) fn sample(rows: i64) -> Vec<u8> {
        let t = w::Table {
            name: "t".into(),
            sql: "CREATE TABLE t (id INTEGER PRIMARY KEY, s TEXT)".into(),
            rows: (1..=rows).map(|i| (i, vec![w::Value::Null, w::Value::Text(format!("row {i}"))])).collect(),
            autoindexes: vec![],
        };
        w::write_database(&[t], &[], 0, 0).unwrap()
    }

    #[test]
    fn repack_refuses_hard_link_alias_wal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.db");
        let alias = dir.path().join("alias.db");
        let wal = dir.path().join("alias.db-wal");
        let original = sample(100);
        std::fs::write(&path, &original).unwrap();
        std::fs::hard_link(&path, &alias).unwrap();
        std::fs::write(&wal, b"pending frames under another name").unwrap();
        assert!(Editor::open_repack(&path).is_err(), "hard links must be refused before relying on path-based WAL checks");
        #[cfg(unix)]
        assert!(Editor::open(&path).is_err(), "append/update must also refuse aliases before checking only the named WAL");
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert_eq!(std::fs::read(&alias).unwrap(), original);
        assert_eq!(std::fs::read(&wal).unwrap(), b"pending frames under another name");
    }
}

// In-place changes are made where SQLite's locks are taken.
#[cfg(all(test, unix, not(target_os = "emscripten")))]
mod tests {
    use super::native_repack_tests::sample;
    use super::*;
    use crate::drivers::sqlite::Db;

    fn root_of(path: &Path) -> u32 {
        let db = Db::new(std::fs::read(path).unwrap()).unwrap();
        db.schema().unwrap().into_iter().find(|e| e.1 == "t").unwrap().3
    }

    #[test]
    fn append_refuses_linked_sidecars_before_recovery() {
        use std::os::unix::fs::symlink;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("database.db");
        let original = sample(5);
        std::fs::write(&path, &original).unwrap();
        let target = directory.path().join("untouched");
        let bytes = b"must not be read, replayed or modified";
        std::fs::write(&target, bytes).unwrap();
        for suffix in ["-journal", "-wal", "-shm"] {
            let sidecar = with_suffix(&path, suffix);
            for hard_link in [false, true] {
                if hard_link {
                    std::fs::hard_link(&target, &sidecar).unwrap();
                } else {
                    symlink(&target, &sidecar).unwrap();
                }
                let error = match Editor::open(&path) {
                    Err(error) => error.to_string(),
                    Ok(_) => panic!("linked sidecar accepted"),
                };
                assert!(error.contains("sidecar"), "{error}");
                assert_eq!(std::fs::read(&path).unwrap(), original);
                assert_eq!(std::fs::read(&target).unwrap(), bytes);
                std::fs::remove_file(&sidecar).unwrap();
            }
        }
        assert!(Editor::open(&path).unwrap().is_ok());
    }
    #[test]
    fn repack_commit_bumps_the_change_counter_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repack.db");
        let original = sample(2000);
        std::fs::write(&path, &original).unwrap();
        let counter = |bytes: &[u8]| u32::from_be_bytes(bytes[24..28].try_into().unwrap());
        let compact = crate::drivers::sqlite_repack::compact(&original).unwrap();
        assert_eq!(counter(&compact), counter(&original));
        let mut editor = Editor::open_repack(&path).unwrap().unwrap();
        editor.replace_image(original.clone(), compact).unwrap();
        editor.commit().unwrap();
        let after = std::fs::read(&path).unwrap();
        assert_eq!(counter(&after), counter(&original).wrapping_add(1));
        assert_eq!(after[92..96], after[24..28]);
    }
    #[test]
    fn lock_keeps_os_error_that_is_not_contention() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("database.db");
        std::fs::write(&path, sample(1)).unwrap();
        // A read-only descriptor cannot take a write lock: F_SETLK fails with EBADF, not contention.
        let file = File::open(&path).unwrap();
        let Err(Error::Io(error)) = lock(&file, &path) else { panic!("write lock accepted a read-only descriptor") };
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }
    #[test]
    fn repack_open_refuses_a_symlink_even_without_path_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("keep.db");
        let link = dir.path().join("alias.db");
        let original = sample(3);
        std::fs::write(&target, &original).unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(Editor::open_repack(&link).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), original);
    }

    #[test]
    fn renamed_database_refuses_commit_without_creating_a_foreign_journal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.db");
        let moved = dir.path().join("moved.db");
        let original = sample(3);
        std::fs::write(&path, &original).unwrap();
        let compact = crate::drivers::sqlite_repack::compact(&original).unwrap();
        let mut editor = Editor::open_repack(&path).unwrap().unwrap();
        editor.replace_image(original.clone(), compact).unwrap();
        std::fs::rename(&path, &moved).unwrap();
        std::fs::write(&path, b"replacement must remain untouched").unwrap();
        assert!(editor.commit_until(Stage::Journal).is_err(), "renamed database accepted");
        assert!(!with_suffix(&path, "-journal").exists(), "journal attached to replacement");
        assert_eq!(std::fs::read(&moved).unwrap(), original);
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement must remain untouched");
    }

    #[test]
    fn repack_wal_refusal_preserves_hot_journal_and_image() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wal-and-journal.db");
        std::fs::write(&path, sample(3)).unwrap();
        let root = root_of(&path);
        let mut editor = Editor::open(&path).unwrap().unwrap();
        editor.insert(root, 4, &w::record(&[w::Value::Null, w::Value::Text("interrupted".into())])).unwrap();
        editor.commit_until(Stage::Written).unwrap();
        let image = std::fs::read(&path).unwrap();
        let journal_path = with_suffix(&path, "-journal");
        let journal = std::fs::read(&journal_path).unwrap();
        let wal_path = with_suffix(&path, "-wal");
        std::fs::write(&wal_path, b"active WAL must remain untouched").unwrap();
        assert!(Editor::open_repack(&path).unwrap().is_err());
        assert!(std::fs::read(&path).unwrap() == image, "WAL refusal replayed database pages");
        assert!(std::fs::read(&journal_path).unwrap() == journal, "WAL refusal changed the journal");
        assert_eq!(std::fs::read(&wal_path).unwrap(), b"active WAL must remain untouched");
    }

    #[test]
    fn repack_journal_does_not_broaden_database_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("private.db");
        let original = sample(3);
        std::fs::write(&path, &original).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut editor = Editor::open_repack(&path).unwrap().unwrap();
        editor.replace_image(original.clone(), crate::drivers::sqlite_repack::compact(&original).unwrap()).unwrap();
        editor.commit_until(Stage::Journal).unwrap();
        let journal = with_suffix(&path, "-journal");
        assert_eq!(std::fs::metadata(journal).unwrap().permissions().mode() & 0o077, 0, "private database leaked into a broader journal");
        assert_eq!(std::fs::read(path).unwrap(), original);
    }

    #[test]
    fn failed_journal_directory_sync_leaves_database_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.db");
        let original = sample(3);
        std::fs::write(&path, &original).unwrap();
        let compact = crate::drivers::sqlite_repack::compact(&original).unwrap();
        let mut editor = Editor::open_repack(&path).unwrap().unwrap();
        editor.replace_image(original.clone(), compact).unwrap();
        let error = editor.commit_with_sync(Stage::Done, |_| Err(std::io::Error::other("directory sync failed"))).unwrap_err();
        assert!(error.to_string().contains("directory sync failed"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(has_hot_journal(&path).unwrap());
        // A failed sync leaves the synced original pages available to normal recovery.
        drop(Editor::open_repack(&path).unwrap().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(!with_suffix(&path, "-journal").exists());
    }

    #[test]
    fn failed_final_directory_sync_reports_committed_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("committed.db");
        std::fs::write(&path, sample(1)).unwrap();
        let root = root_of(&path);
        let mut editor = Editor::open(&path).unwrap().unwrap();
        editor.insert(root, 2, &w::record(&[w::Value::Null, w::Value::Text("committed".into())])).unwrap();
        let mut calls = 0;
        let error = editor
            .commit_with_sync(Stage::Done, |_| {
                calls += 1;
                if calls == 2 { Err(std::io::Error::other("sync failed")) } else { Ok(()) }
            })
            .unwrap_err();
        assert_eq!(calls, 2);
        assert!(error.to_string().contains("changes committed"));
        assert!(error.to_string().contains("durability is unconfirmed"));
        assert!(!with_suffix(&path, "-journal").exists());
        let database = Db::new(std::fs::read(path).unwrap()).unwrap();
        let table = database.table("t").unwrap();
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.rows[1][1], crate::drivers::sqlite::Value::Text("committed".into()));
    }

    #[test]
    fn recovery_sync_failures_preserve_or_report_the_completed_rollback() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("recover-sync.db");
        let original = sample(3);
        std::fs::write(&path, &original).unwrap();
        let root = root_of(&path);
        let mut editor = Editor::open(&path).unwrap().unwrap();
        editor.insert(root, 4, &w::record(&[w::Value::Null, w::Value::Text("interrupted".into())])).unwrap();
        editor.commit_until(Stage::Written).unwrap();
        let written = std::fs::read(&path).unwrap();
        assert_ne!(written, original);
        let journal_path = with_suffix(&path, "-journal");
        let journal = std::fs::read(&journal_path).unwrap();
        let mut file = OpenOptions::new().read(true).write(true).open(&path).unwrap();
        let error = recover_with_sync(&mut file, &path, |_| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))).unwrap_err();
        assert!(matches!(error, Error::Io(ref error) if error.kind() == std::io::ErrorKind::PermissionDenied));
        assert_eq!(std::fs::read(&path).unwrap(), written);
        assert_eq!(std::fs::read(&journal_path).unwrap(), journal);
        let mut calls = 0;
        let error = recover_with_sync(&mut file, &path, |_| {
            calls += 1;
            if calls == 2 { Err(std::io::Error::other("final sync failed")) } else { Ok(()) }
        })
        .unwrap_err();
        assert_eq!(calls, 2);
        assert!(error.to_string().contains("recovery completed"));
        assert!(error.to_string().contains("durability is unconfirmed"));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(!journal_path.exists());
    }

    #[test]
    fn directory_sync_reports_an_unreachable_parent() {
        let dir = tempfile::tempdir().unwrap();
        assert!(sync_dir(&dir.path().join("missing/data.db")).is_err());
    }

    #[test]
    fn journal_links_planted_after_open_cannot_overwrite_foreign_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("data.db");
        let foreign = dir.path().join("foreign.bin");
        let journal = with_suffix(&target, "-journal");
        for hard in [false, true] {
            let original = sample(3);
            std::fs::write(&target, &original).unwrap();
            std::fs::write(&foreign, b"foreign data must remain").unwrap();
            let compact = crate::drivers::sqlite_repack::compact(&original).unwrap();
            let mut editor = Editor::open_repack(&target).unwrap().unwrap();
            editor.replace_image(original, compact).unwrap();
            if hard {
                std::fs::hard_link(&foreign, &journal).unwrap();
            } else {
                std::os::unix::fs::symlink(&foreign, &journal).unwrap();
            }
            editor.commit().unwrap();
            assert!(std::fs::read(&foreign).unwrap() == b"foreign data must remain", "foreign bytes changed");
        }
    }

    #[test]
    fn torn_unchanged_page_is_recoverable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("torn.db");
        let original = sample(2000);
        std::fs::write(&path, &original).unwrap();
        let mut editor = Editor::open_repack(&path).unwrap().unwrap();
        let page_size = editor.page_size;
        // All data pages are identical in this replacement. Nevertheless write_all
        // overwrites them and an interrupted write may tear any of those pages.
        editor.replace_image(original.clone(), original.clone()).unwrap();
        editor.commit_until(Stage::Written).unwrap();
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(page_size as u64 + 20)).unwrap();
        file.write_all(&[original[page_size + 20] ^ 0xff]).unwrap();
        file.sync_all().unwrap();
        drop(file);
        assert_eq!(read_consistent(&path).unwrap(), original);
        drop(Editor::open_repack(&path).unwrap().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    #[ignore = "requires a >1 GiB SQLite fixture and a Python SQLite reference"]
    fn sqlite_recovers_a_torn_unchanged_page_beyond_the_lock_byte_page() {
        let source = std::env::var_os("TERRIO_LARGE_SQLITE_REFERENCE").expect("set TERRIO_LARGE_SQLITE_REFERENCE");
        let python = std::env::var_os("TERRIO_SQLITE_REFERENCE_PYTHON").expect("set TERRIO_SQLITE_REFERENCE_PYTHON");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target");
        let dir = tempfile::tempdir_in(root).unwrap();
        let path = dir.path().join("large-torn.db");
        std::fs::copy(source, &path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let mut editor = Editor::open_repack(&path).unwrap().unwrap();
        let at = PENDING_BYTE as u64 + editor.page_size as u64 + 20;
        assert!(at < original.len() as u64);
        editor.replace_image(original.clone(), original.clone()).unwrap();
        editor.commit_until(Stage::Written).unwrap();
        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(at)).unwrap();
        file.write_all(&[original[at as usize] ^ 0xff]).unwrap();
        file.sync_all().unwrap();
        drop(file);
        let output = std::process::Command::new(python)
            .args([
                "-c",
                concat!(
                    "import sqlite3,sys; db=sqlite3.connect(sys.argv[1]); ",
                    "assert db.execute('PRAGMA integrity_check').fetchall()==[('ok',)]; db.close()"
                ),
            ])
            .arg(&path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        assert!(std::fs::read(&path).unwrap() == original, "SQLite must restore every original byte beyond the lock page too");
    }

    #[test]
    fn interrupted_repack_restores_truncated_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repack.db");
        std::fs::write(&path, sample(2000)).unwrap();
        let root = root_of(&path);
        let mut editor = Editor::open(&path).unwrap().unwrap();
        editor.rebuild_table(root, &[(1, vec![w::Value::Null, w::Value::Text("keep".into())])]).unwrap();
        editor.commit().unwrap();
        let original = std::fs::read(&path).unwrap();
        for version in [1, 2] {
            let mut original = original.clone();
            original[18] = version;
            original[19] = version;
            let compact = crate::drivers::sqlite_repack::compact(&original).unwrap();
            assert!(compact.len() < original.len());
            for stage in [Stage::Journal, Stage::HalfWritten, Stage::Written] {
                std::fs::write(&path, &original).unwrap();
                let mut editor = Editor::open_repack(&path).unwrap().unwrap();
                editor.replace_image(original.clone(), compact.clone()).unwrap();
                editor.commit_until(stage).unwrap();
                assert_eq!(read_consistent(&path).unwrap(), original, "{stage:?}");
                if let Some(python) = std::env::var_os("TERRIO_SQLITE_REFERENCE_PYTHON") {
                    let code = concat!(
                        "import sqlite3,sys; db=sqlite3.connect(sys.argv[1]); ",
                        "assert db.execute('PRAGMA integrity_check').fetchall()==[('ok',)]; ",
                        "assert db.execute('SELECT s FROM t').fetchall()==[('keep',)]; db.close()"
                    );
                    let result = std::process::Command::new(python).args(["-c", code]).arg(&path).output().unwrap();
                    assert!(result.status.success(), "SQLite reference recovery {stage:?}: {}", String::from_utf8_lossy(&result.stderr));
                }
                drop(Editor::open_repack(&path).unwrap().unwrap());
                assert_eq!(std::fs::read(&path).unwrap(), original, "{stage:?}");
                assert!(!has_hot_journal(&path).unwrap());
            }
        }
    }

    #[test]
    fn appends_inserts_and_splits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.db");
        std::fs::write(&path, sample(3)).unwrap();
        let root = root_of(&path);
        let mut e = Editor::open(&path).unwrap().unwrap();
        // Many rows, some large (overflow pages), appended; then rows inserted in between and
        // replaced.
        let rows: Vec<(i64, Vec<u8>)> = (10..20_000)
            .step_by(2)
            .map(|i| {
                let s = if i % 997 == 0 { "x".repeat(9000) } else { format!("row {i}") };
                (i, w::record(&[w::Value::Null, w::Value::Text(s)]))
            })
            .collect();
        e.append(root, rows).unwrap();
        for i in (11..20_000).step_by(200) {
            e.insert(root, i, &w::record(&[w::Value::Null, w::Value::Text("y".repeat(i as usize % 5000))])).unwrap();
        }
        e.insert(root, 12, &w::record(&[w::Value::Null, w::Value::Text("replaced".into())])).unwrap();
        assert_eq!(e.max_rowid(root).unwrap(), 19_998);
        e.commit().unwrap();
        assert!(!with_suffix(&path, "-journal").exists());

        let db = Db::new(std::fs::read(&path).unwrap()).unwrap();
        let t = db.table("t").unwrap();
        assert_eq!(t.rows.len(), 3 + 9995 + 100);
        let ids: Vec<i64> = t.rows.iter().map(|r| r[0].as_i64().unwrap()).collect();
        assert!(ids.windows(2).all(|w| w[0] < w[1]));
        let row = t.rows.iter().find(|r| r[0].as_i64() == Some(12)).unwrap();
        assert_eq!(row[1].as_str(), Some("replaced"));
        let row = t.rows.iter().find(|r| r[0].as_i64() == Some(9970)).unwrap();
        assert_eq!(row[1].as_str().map(str::len), Some(9000));
    }

    #[test]
    fn frees_and_reuses_pages() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.db");
        std::fs::write(&path, sample(2000)).unwrap();
        let size = std::fs::metadata(&path).unwrap().len();
        let root = root_of(&path);
        let mut e = Editor::open(&path).unwrap().unwrap();
        e.rebuild_table(root, &[(1, vec![w::Value::Null, w::Value::Text("one".into())])]).unwrap();
        let freed = e.header_u32(36).unwrap();
        assert!(freed > 5);
        e.commit().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size);
        let mut e = Editor::open(&path).unwrap().unwrap();
        let rows: Vec<(i64, Vec<u8>)> = (2..500).map(|i| (i, w::record(&[w::Value::Null, w::Value::Text(format!("{i}"))]))).collect();
        e.append(root, rows).unwrap();
        assert!(e.header_u32(36).unwrap() < freed);
        e.commit().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), size);
        let db = Db::new(std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(db.table("t").unwrap().rows.len(), 499);
    }

    #[test]
    fn catalog_reads_original_index_during_a_hot_journal() {
        use crate::drivers::sqlite::{Db, Value};
        use wbvector::{Coord, Feature, FieldDef, FieldType, FieldValue, Geometry, Layer};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("index.gti.gpkg");
        let mut layer = Layer::new("tiles");
        layer.set_crs_epsg(Some(3857));
        layer.add_field(FieldDef::new("location", FieldType::Text));
        let mut feature = Feature::with_geometry(
            0,
            Geometry::polygon(vec![Coord::xy(0., 0.), Coord::xy(4., 0.), Coord::xy(4., 4.), Coord::xy(0., 4.)], vec![]),
            1,
        );
        feature.attributes = vec![FieldValue::Text("tile.tif".into())];
        layer.push(feature);
        std::fs::write(&path, crate::drivers::vector::geopackage::to_bytes(&[layer]).unwrap()).unwrap();
        let db = Db::open(&path).unwrap();
        let schema = db.schema().unwrap();
        let entry = schema.iter().find(|entry| entry.1 == "gpkg_contents").unwrap();
        let field = db.table("gpkg_contents").unwrap().column("data_type").unwrap();
        let (rowid, mut row) = db.stored_rows(entry.3).unwrap().remove(0);
        row[field] = Value::Text("not-a-layer".into());
        let values = row
            .into_iter()
            .map(|value| match value {
                Value::Null => w::Value::Null,
                Value::Int(value) => w::Value::Int(value),
                Value::Real(value) => w::Value::Real(value),
                Value::Text(value) => w::Value::Text(value),
                Value::Blob(value) => w::Value::Blob(value),
            })
            .collect::<Vec<_>>();
        let root = entry.3;
        drop(db);
        let mut editor = Editor::open(&path).unwrap().unwrap();
        editor.insert(root, rowid, &w::record(&values)).unwrap();
        editor.commit_until(Stage::Written).unwrap();
        assert!(has_hot_journal(&path).unwrap());
        assert!(crate::drivers::vector::gpkg_read::contents(&Db::open(&path).unwrap()).unwrap().0.is_empty());
        let options = crate::catalog::CatalogOptions {
            resolution: Some((1., 1.)),
            extent: Some((0., 0., 4., 4.)),
            bands: Some(1),
            data_type: Some(wbraster::DataType::F64),
            ..Default::default()
        };
        let catalog = crate::catalog::RasterCatalog::open(path.to_str().unwrap(), options).unwrap();
        assert_eq!(catalog.header().raster.cols, 4);
        assert_eq!(catalog.stats().source_opens, 0);
        assert!(has_hot_journal(&path).unwrap()); // Read-only recovery leaves the file/journal intact.
    }

    #[test]
    fn recovers_from_an_interrupted_commit() {
        let dir = tempfile::tempdir().unwrap();
        for stage in [Stage::Journal, Stage::HalfWritten, Stage::Written] {
            let path = dir.path().join("c.db");
            let original = sample(500);
            std::fs::write(&path, &original).unwrap();
            let root = root_of(&path);
            let mut e = Editor::open(&path).unwrap().unwrap();
            let rows: Vec<(i64, Vec<u8>)> =
                (501..3000).map(|i| (i, w::record(&[w::Value::Null, w::Value::Text("z".repeat(50))]))).collect();
            e.append(root, rows).unwrap();
            e.insert(root, 3, &w::record(&[w::Value::Null, w::Value::Text("changed".into())])).unwrap();
            e.commit_until(stage).unwrap();
            assert!(with_suffix(&path, "-journal").exists());
            // Readers see the database as it was.
            assert_eq!(read_consistent(&path).unwrap(), original, "{stage:?}");
            // Opening plays the journal back.
            drop(Editor::open(&path).unwrap().unwrap());
            assert!(!with_suffix(&path, "-journal").exists());
            assert_eq!(std::fs::read(&path).unwrap(), original, "{stage:?}");
        }
    }
}

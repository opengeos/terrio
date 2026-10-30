// Written to match the behavior of:
//   GDAL v3.13.3:
//     frmts/gtiff/gtiffdataset_write.cpp
//     swig/python/gdal-utils/osgeo_utils/gdal_edit.py
// No code is translated from these sources.

//! Updating GeoTIFFs in place, as GDAL's GTiff driver does: new or changed images (overviews, the
//! changed blocks of an image) and directories are appended at the end of the file, and the
//! header is pointed at the new chain of directories. No byte already in the file changes but the
//! header's offset of the first directory, so the data of the images left alone stays where it is
//! (and, as in GDAL, the space of replaced data is not reclaimed).
//!
//! Files are changed through [`update_file`]: a copy next to the original is updated, synced and
//! renamed over it, so that an error or an interruption leaves the original as it was. Only
//! little-endian TIFFs (those GDAL and terrio write) are updated; a classic TIFF that would grow
//! past 4 GiB is refused. Even metadata-only edits copy the complete file and need that
//! much free space. Symlink targets are updated, and mode bits are preserved. Replacement
//! detaches hard links and does not preserve ownership, ACLs or extended attributes; the
//! containing directory must be writable.

use super::gtiff_write::{Image, Value};
use crate::{Error, Result};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// `NewSubfileType`'s reduced-resolution bit (overviews).
pub(crate) const REDUCED: u64 = 1;
/// `NewSubfileType`'s transparency mask bit (GDAL's internal masks).
pub(crate) const MASK: u64 = 4;
/// The most directories followed in a chain.
const MAX_DIRECTORIES: usize = 4096;

/// A tag of a directory: its field type, value count and value bytes (little-endian).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Entry {
    pub(crate) tag: u16,
    pub(crate) typ: u16,
    pub(crate) count: u64,
    pub(crate) bytes: Vec<u8>,
}

/// The size of a TIFF field type's values, `None` for unknown types.
fn type_size(typ: u16) -> Option<u64> {
    Some(match typ {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 | 13 => 4,
        5 | 10 | 12 | 16 | 17 | 18 => 8,
        _ => return None,
    })
}

impl Entry {
    /// A tag with these values, encoded for a classic TIFF or a BigTIFF.
    pub(crate) fn new(tag: u16, value: &Value, big: bool) -> Self {
        let (typ, count, bytes) = value.encode(big);
        Self { tag, typ, count, bytes }
    }

    /// The values of an unsigned integer tag (empty for other types).
    pub(crate) fn uints(&self) -> Vec<u64> {
        let size = match self.typ {
            1 | 7 => 1,
            3 => 2,
            4 | 13 => 4,
            16 | 18 => 8,
            _ => return Vec::new(),
        };
        self.bytes
            .chunks_exact(size)
            .map(|c| {
                let mut b = [0u8; 8];
                b[..size].copy_from_slice(c);
                u64::from_le_bytes(b)
            })
            .collect()
    }

    /// The text of an `ASCII` tag, up to its first NUL.
    pub(crate) fn ascii(&self) -> Option<String> {
        (self.typ == 2).then(|| String::from_utf8_lossy(self.bytes.split(|b| *b == 0).next().unwrap_or_default()).into_owned())
    }
}

/// An image file directory: its tags, sorted.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Directory {
    pub(crate) entries: Vec<Entry>,
}

impl Directory {
    /// A tag, when the directory has it.
    pub(crate) fn get(&self, tag: u16) -> Option<&Entry> {
        self.entries.iter().find(|e| e.tag == tag)
    }

    /// The first value of an integer tag.
    pub(crate) fn uint(&self, tag: u16) -> Option<u64> {
        self.get(tag).and_then(|e| e.uints().first().copied())
    }

    /// Adds a tag, or replaces it.
    pub(crate) fn set(&mut self, entry: Entry) {
        self.remove(entry.tag);
        self.entries.push(entry);
        self.entries.sort_by_key(|e| e.tag);
    }

    /// Removes a tag.
    pub(crate) fn remove(&mut self, tag: u16) {
        self.entries.retain(|e| e.tag != tag);
    }

    /// The `NewSubfileType`.
    pub(crate) fn subfile(&self) -> u64 {
        self.uint(254).unwrap_or(0)
    }

    /// Whether the image is an overview (a reduced-resolution image that is not a mask).
    pub(crate) fn is_overview(&self) -> bool {
        self.subfile() & REDUCED != 0 && self.subfile() & MASK == 0
    }

    /// Whether the image is a transparency mask (of the full-resolution image or of an overview).
    pub(crate) fn is_mask(&self) -> bool {
        self.subfile() & MASK != 0
    }

    /// The image's width and height.
    pub(crate) fn size(&self) -> (usize, usize) {
        (self.uint(256).unwrap_or(0) as usize, self.uint(257).unwrap_or(0) as usize)
    }

    /// Whether the image is tiled.
    pub(crate) fn tiled(&self) -> bool {
        self.get(324).is_some()
    }

    /// The chunk (tile or strip) offsets and byte counts.
    pub(crate) fn chunks(&self) -> (Vec<u64>, Vec<u64>) {
        let (o, c) = if self.tiled() { (324, 325) } else { (273, 279) };
        (self.get(o).map(Entry::uints).unwrap_or_default(), self.get(c).map(Entry::uints).unwrap_or_default())
    }
}

/// A TIFF's structure: whether it is a BigTIFF, and its directories in chain order.
#[derive(Debug, Clone)]
pub(crate) struct Tiff {
    pub(crate) big: bool,
    pub(crate) dirs: Vec<Directory>,
}

/// Reads exactly `buf.len()` bytes at `offset`, checked against the file's length.
fn read_at<R: Read + Seek>(r: &mut R, offset: u64, buf: &mut [u8], len: u64) -> Result<()> {
    if offset.checked_add(buf.len() as u64).is_none_or(|end| end > len) {
        return Err(Error::InvalidData("TIFF: a directory or value lies past the end of the file".into()));
    }
    r.seek(SeekFrom::Start(offset))?;
    r.read_exact(buf)?;
    Ok(())
}

/// Reads the structure of a little-endian TIFF: its header and the chain of directories with
/// every tag's values. A tag of an unknown field type is refused: its size, and so its values,
/// are unknown, and leaving it out would drop it from the rewritten file.
pub(crate) fn parse<R: Read + Seek>(r: &mut R) -> Result<Tiff> {
    let len = r.seek(SeekFrom::End(0))?;
    let mut head = [0u8; 16];
    let n = len.min(16) as usize;
    read_at(r, 0, &mut head[..n], len)?;
    let big = match &head[..4.min(n)] {
        b"II*\0" => false,
        b"II+\0" => true,
        b"MM\0*" | b"MM\0+" => {
            return Err(Error::Unsupported("big-endian (Motorola byte order) TIFFs cannot be updated: convert the file first".into()));
        }
        _ => return Err(Error::InvalidData("not a TIFF file".into())),
    };
    let mut next = if big {
        if n < 16 {
            return Err(Error::InvalidData("TIFF: truncated header".into()));
        }
        u64::from_le_bytes(head[8..16].try_into().unwrap())
    } else {
        if n < 8 {
            return Err(Error::InvalidData("TIFF: truncated header".into()));
        }
        u32::from_le_bytes(head[4..8].try_into().unwrap()) as u64
    };
    let mut metadata_budget = 64u64 << 20;
    let (count_len, entry_len, inline) = if big { (8usize, 20usize, 8usize) } else { (2, 12, 4) };
    let mut seen = std::collections::HashSet::new();
    let mut dirs = Vec::new();
    while next != 0 {
        if !seen.insert(next) || dirs.len() >= MAX_DIRECTORIES {
            return Err(Error::InvalidData("TIFF: the chain of directories loops".into()));
        }
        let mut c = [0u8; 8];
        read_at(r, next, &mut c[..count_len], len)?;
        let count = if big { u64::from_le_bytes(c) } else { u16::from_le_bytes([c[0], c[1]]) as u64 };
        let table_len =
            count.checked_mul(entry_len as u64).filter(|n| *n <= len).ok_or_else(|| Error::InvalidData("TIFF: too many tags".into()))?;
        metadata_budget = metadata_budget
            .checked_sub(table_len + inline as u64)
            .ok_or_else(|| Error::InvalidData("TIFF update metadata exceeds 64 MiB".into()))?;
        let mut table = vec![0u8; table_len as usize + inline];
        read_at(r, next + count_len as u64, &mut table, len)?;
        let mut entries = Vec::with_capacity(count as usize);
        for e in table[..table_len as usize].chunks_exact(entry_len) {
            let tag = u16::from_le_bytes([e[0], e[1]]);
            let typ = u16::from_le_bytes([e[2], e[3]]);
            let (n, field) = if big {
                (u64::from_le_bytes(e[4..12].try_into().unwrap()), &e[12..20])
            } else {
                (u32::from_le_bytes(e[4..8].try_into().unwrap()) as u64, &e[8..12])
            };
            // Its size is unknown, so it could not be copied: refused rather than dropped.
            let Some(size) = type_size(typ) else {
                return Err(Error::Unsupported(format!("TIFF tag {tag} has an unknown field type {typ}: the file cannot be rewritten")));
            };
            let bytes_len =
                n.checked_mul(size).filter(|b| *b <= len).ok_or_else(|| Error::InvalidData(format!("TIFF: tag {tag} is too large")))?;
            metadata_budget =
                metadata_budget.checked_sub(bytes_len).ok_or_else(|| Error::InvalidData("TIFF update metadata exceeds 64 MiB".into()))?;
            let bytes = if bytes_len as usize <= inline {
                field[..bytes_len as usize].to_vec()
            } else {
                let offset =
                    if big { u64::from_le_bytes(field.try_into().unwrap()) } else { u32::from_le_bytes(field.try_into().unwrap()) as u64 };
                let mut v = vec![0u8; bytes_len as usize];
                read_at(r, offset, &mut v, len)?;
                v
            };
            entries.push(Entry { tag, typ, count: n, bytes });
        }
        entries.sort_by_key(|e| e.tag);
        let tail = &table[table_len as usize..];
        next = if big { u64::from_le_bytes(tail.try_into().unwrap()) } else { u32::from_le_bytes(tail.try_into().unwrap()) as u64 };
        dirs.push(Directory { entries });
    }
    if dirs.is_empty() {
        return Err(Error::InvalidData("TIFF: no image".into()));
    }
    Ok(Tiff { big, dirs })
}

/// Appends data and directories to a TIFF, then points its header at a new chain of directories.
pub(crate) struct Appender<'a, F: Write + Seek> {
    file: &'a mut F,
    big: bool,
    /// Where the next bytes go: the end of the file.
    pos: u64,
}

impl<'a, F: Write + Seek> Appender<'a, F> {
    /// Appends to `file`, a TIFF (or BigTIFF) whose header is written.
    pub(crate) fn new(file: &'a mut F, big: bool) -> Result<Self> {
        let mut pos = file.seek(SeekFrom::End(0))?;
        if pos % 2 == 1 {
            file.write_all(&[0])?;
            pos += 1;
        }
        Ok(Self { file, big, pos })
    }

    /// Whether the file is a BigTIFF.
    pub(crate) fn is_big(&self) -> bool {
        self.big
    }

    /// Writes bytes at the end of the file (padded to an even length) and returns their offset.
    pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<u64> {
        let at = self.pos;
        let end = at + (bytes.len() as u64).next_multiple_of(2);
        self.check(end)?;
        self.file.seek(SeekFrom::Start(at))?;
        self.file.write_all(bytes)?;
        if bytes.len() % 2 == 1 {
            self.file.write_all(&[0])?;
        }
        self.pos = end;
        Ok(at)
    }

    fn check(&self, end: u64) -> Result<()> {
        if !self.big && end > u32::MAX as u64 {
            return Err(Error::Unsupported("the update would grow the classic TIFF past 4 GiB: convert it to a BigTIFF first".into()));
        }
        Ok(())
    }

    /// Writes an image's chunks and returns its directory, with the chunks' offsets and counts.
    pub(crate) fn image(&mut self, img: Image) -> Result<Directory> {
        let mut offsets = Vec::with_capacity(img.chunks.len());
        let mut counts = Vec::with_capacity(img.chunks.len());
        for c in &img.chunks {
            offsets.push(self.write(c)?);
            counts.push(c.len() as u64);
        }
        let mut dir = Directory { entries: img.tags.iter().map(|(t, v)| Entry::new(*t, v, self.big)).collect() };
        dir.entries.sort_by_key(|e| e.tag);
        let (o, c) = if img.tiled { (324, 325) } else { (273, 279) };
        dir.set(Entry::new(o, &Value::Offsets(offsets), self.big));
        dir.set(Entry::new(c, &Value::Offsets(counts), self.big));
        Ok(dir)
    }

    /// Writes the directories, chained in this order, and points the header at the first.
    pub(crate) fn finish(mut self, dirs: &[Directory]) -> Result<()> {
        if dirs.is_empty() {
            return Err(Error::InvalidArgument("a TIFF needs an image".into()));
        }
        let mut starts = Vec::with_capacity(dirs.len());
        let mut p = self.pos;
        for d in dirs {
            starts.push(p);
            p += directory_len(d, self.big);
        }
        self.check(p)?;
        for (i, d) in dirs.iter().enumerate() {
            let bytes = encode_directory(d, starts[i], starts.get(i + 1).copied().unwrap_or(0), self.big)?;
            self.write(&bytes)?;
        }
        if self.big {
            self.file.seek(SeekFrom::Start(8))?;
            self.file.write_all(&starts[0].to_le_bytes())?;
        } else {
            self.file.seek(SeekFrom::Start(4))?;
            self.file.write_all(&(starts[0] as u32).to_le_bytes())?;
        }
        self.file.flush()?;
        Ok(())
    }
}

/// The header of an empty TIFF or BigTIFF, without a directory yet.
pub(crate) fn header(big: bool) -> Vec<u8> {
    if big {
        let mut h = b"II+\0".to_vec();
        h.extend(8u16.to_le_bytes());
        h.extend(0u16.to_le_bytes());
        h.extend(0u64.to_le_bytes());
        h
    } else {
        let mut h = b"II*\0".to_vec();
        h.extend(0u32.to_le_bytes());
        h
    }
}

fn directory_len(d: &Directory, big: bool) -> u64 {
    let (count_len, entry_len, inline, next_len) = if big { (8u64, 20u64, 8usize, 8u64) } else { (2, 12, 4, 4) };
    let extra: u64 = d.entries.iter().filter(|e| e.bytes.len() > inline).map(|e| (e.bytes.len() as u64).next_multiple_of(2)).sum();
    count_len + entry_len * d.entries.len() as u64 + next_len + extra
}

/// Encodes a directory starting at file offset `start`: its entries, the offset of the next
/// directory, then the values that do not fit in the entries.
fn encode_directory(d: &Directory, start: u64, next: u64, big: bool) -> Result<Vec<u8>> {
    let (count_len, entry_len, inline) = if big { (8u64, 20u64, 8usize) } else { (2, 12, 4) };
    let next_len = if big { 8 } else { 4 };
    let n = d.entries.len() as u64;
    let mut out = Vec::new();
    if big {
        out.extend(n.to_le_bytes());
    } else {
        let n = u16::try_from(n).map_err(|_| Error::InvalidData("TIFF: too many tags".into()))?;
        out.extend(n.to_le_bytes());
    }
    let mut extra_pos = start + count_len + entry_len * n + next_len;
    let mut extra = Vec::new();
    for e in &d.entries {
        if !big && matches!(e.typ, 16..=18) {
            return Err(Error::InvalidData(format!("TIFF: tag {} has a BigTIFF type in a classic TIFF", e.tag)));
        }
        out.extend(e.tag.to_le_bytes());
        out.extend(e.typ.to_le_bytes());
        if big {
            out.extend(e.count.to_le_bytes());
        } else {
            let count = u32::try_from(e.count).map_err(|_| Error::InvalidData(format!("TIFF: tag {} has too many values", e.tag)))?;
            out.extend(count.to_le_bytes());
        }
        if e.bytes.len() <= inline {
            let mut v = e.bytes.clone();
            v.resize(inline, 0);
            out.extend(v);
        } else {
            if big {
                out.extend(extra_pos.to_le_bytes());
            } else {
                out.extend((extra_pos as u32).to_le_bytes());
            }
            extra.extend(&e.bytes);
            if e.bytes.len() % 2 == 1 {
                extra.push(0);
            }
            extra_pos += (e.bytes.len() as u64).next_multiple_of(2);
        }
    }
    if big {
        out.extend(next.to_le_bytes());
    } else {
        out.extend((next as u32).to_le_bytes());
    }
    out.extend(extra);
    Ok(out)
}

/// Where GDAL's structural metadata of a Cloud Optimized GeoTIFF (the "ghost" area after the
/// header) declares the layout intact (`KNOWN_INCOMPATIBLE_EDITION=NO`): the offset of that `NO`.
/// `None` for other TIFFs, and for COGs already edited.
pub(crate) fn intact_cog_layout<R: Read + Seek>(r: &mut R, big: bool) -> Result<Option<u64>> {
    let start = if big { 16u64 } else { 8 };
    let mut head = [0u8; 43];
    r.seek(SeekFrom::Start(start))?;
    if r.read_exact(&mut head).is_err() {
        return Ok(None);
    }
    let prefix = b"GDAL_STRUCTURAL_METADATA_SIZE=";
    if !head.starts_with(prefix) {
        return Ok(None);
    }
    let size: usize = match std::str::from_utf8(&head[prefix.len()..prefix.len() + 6]).ok().and_then(|n| n.trim().parse().ok()) {
        Some(n) if n < 1 << 16 => n,
        _ => return Ok(None),
    };
    let body_at = start + prefix.len() as u64 + 6 + " bytes\n".len() as u64;
    let mut body = vec![0u8; size];
    r.seek(SeekFrom::Start(body_at))?;
    if r.read_exact(&mut body).is_err() {
        return Ok(None);
    }
    let Ok(text) = std::str::from_utf8(&body) else { return Ok(None) };
    let key = "KNOWN_INCOMPATIBLE_EDITION=NO";
    match (text.contains("LAYOUT=IFDS_BEFORE_DATA"), text.find(key)) {
        (true, Some(i)) if body.get(i + key.len()..).is_some_and(|tail| tail.starts_with(b"\n ")) => {
            Ok(Some(body_at + (i + key.len() - 2) as u64))
        }
        _ => Ok(None),
    }
}

/// Before a Cloud Optimized GeoTIFF is updated: refuses, as GDAL does without its
/// `IGNORE_COG_LAYOUT_BREAK=YES` open option, or records in its structural metadata that the
/// layout is broken (`KNOWN_INCOMPATIBLE_EDITION=YES`, as GDAL writes it). The file remains a
/// valid GeoTIFF.
pub(crate) fn break_cog_layout<F: Read + Write + Seek>(f: &mut F, big: bool, allowed: bool) -> Result<()> {
    let Some(at) = intact_cog_layout(f, big)? else { return Ok(()) };
    if !allowed {
        return Err(Error::Unsupported(
            "the file is a Cloud Optimized GeoTIFF: updating it breaks its layout (it stays a valid GeoTIFF); \
             allow it with --ignore-cog-layout-break (GDAL's IGNORE_COG_LAYOUT_BREAK=YES), or write a new COG instead"
                .into(),
        ));
    }
    // "NO\n " becomes "YES\n": the ghost area keeps its size.
    f.seek(SeekFrom::Start(at))?;
    f.write_all(b"YES\n")?;
    Ok(())
}

/// Refuses a path that is not a local file that can be written: remote objects, archive members
/// and read-only files are not updated in place.
pub(crate) fn writable_local_file(path: &str) -> Result<PathBuf> {
    let loc = crate::vfs::Location::parse(path);
    let crate::vfs::Location::Local(p) = loc else {
        return Err(Error::Unsupported(format!("'{path}' is not a local file: only local files can be updated in place")));
    };
    let meta = std::fs::metadata(&p).map_err(|e| Error::Io(std::io::Error::new(e.kind(), format!("{}: {e}", p.display()))))?;
    if !meta.is_file() {
        return Err(Error::InvalidArgument(format!("'{}' is not a file", p.display())));
    }
    if meta.permissions().readonly() {
        return Err(Error::Io(std::io::Error::new(std::io::ErrorKind::PermissionDenied, format!("'{}' is read-only", p.display()))));
    }
    Ok(std::fs::canonicalize(p)?)
}

/// A temporary file next to the file it replaces, removed unless it is persisted.
struct Temporary {
    path: PathBuf,
    keep: bool,
}

impl Temporary {
    /// A new, unused name next to `target`: `.name.terrio-<pid>-<n>.tmp`.
    fn next_to(target: &Path) -> Result<Self> {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = target.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let name = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        for _ in 0..1000 {
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = dir.join(format!(".{name}.terrio-{}-{n}.tmp", std::process::id()));
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(_) => return Ok(Self { path, keep: false }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(Error::InvalidData("no temporary file name available".into()))
    }

    /// Renames the temporary file over `target`.
    fn persist(mut self, target: &Path) -> Result<()> {
        std::fs::rename(&self.path, target)?;
        self.keep = true;
        // The rename itself is durable once the directory is synced (best effort).
        #[cfg(unix)]
        if let Some(dir) = target.parent().filter(|d| !d.as_os_str().is_empty())
            && let Ok(d) = std::fs::File::open(dir)
        {
            let _ = d.sync_all();
        }
        Ok(())
    }
}

impl Drop for Temporary {
    fn drop(&mut self) {
        if !self.keep {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Changes a local file safely: `change` updates a copy of it (opened for reading and writing),
/// which is synced and renamed over the file only when `change` succeeds. On any error the file
/// is left as it was and the copy is removed. The copy keeps the file's permissions.
pub(crate) fn update_file<T>(path: &Path, change: impl FnOnce(&mut std::fs::File) -> Result<T>) -> Result<T> {
    update_file_at(path, |f, _| change(f))
}

/// [`update_file`], whose `change` also gets the path of the copy it updates (to read it back).
pub(crate) fn update_file_at<T>(path: &Path, change: impl FnOnce(&mut std::fs::File, &Path) -> Result<T>) -> Result<T> {
    let resolved = std::fs::canonicalize(path)?;
    let path = resolved.as_path();
    let tmp = Temporary::next_to(path)?;
    std::fs::copy(path, &tmp.path)?;
    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(&tmp.path)?;
    let out = change(&mut file, &tmp.path)?;
    file.sync_all()?;
    drop(file);
    tmp.persist(path)?;
    Ok(out)
}

/// Writes a new file safely: `write` fills a temporary file next to `path`, which is synced and
/// renamed to `path` (replacing any file there) only when `write` succeeds.
pub(crate) fn replace_file<T>(path: &Path, write: impl FnOnce(&mut std::fs::File) -> Result<T>) -> Result<T> {
    let resolved = match std::fs::symlink_metadata(path) {
        Ok(_) => Some(std::fs::canonicalize(path)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e.into()),
    };
    let path = resolved.as_deref().unwrap_or(path);
    let permissions = std::fs::metadata(path).ok().map(|m| m.permissions());
    let tmp = Temporary::next_to(path)?;
    let mut file = std::fs::OpenOptions::new().read(true).write(true).truncate(true).open(&tmp.path)?;
    let out = write(&mut file)?;
    if let Some(permissions) = permissions {
        file.set_permissions(permissions)?;
    }
    file.sync_all()?;
    drop(file);
    tmp.persist(path)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(big: bool) -> Vec<u8> {
        let cfg = wbraster::RasterConfig { cols: 20, rows: 10, bands: 1, data_type: wbraster::DataType::U8, ..Default::default() };
        let r = wbraster::Raster::from_data(cfg, (0..200).map(|v| v as f64).collect()).unwrap();
        let opts = super::super::gtiff_write::TiffOptions { bigtiff: big, ..Default::default() };
        super::super::gtiff_write::encode(&r, &opts).unwrap()
    }

    #[test]
    fn rewritten_chains_read_back_identically() {
        for big in [false, true] {
            let mut bytes = sample(big);
            if bytes.len().is_multiple_of(2) {
                bytes.push(0);
            }
            let tiff = parse(&mut std::io::Cursor::new(&bytes)).unwrap();
            assert_eq!(tiff.big, big);
            let mut out = std::io::Cursor::new(bytes.clone());
            let mut dir = tiff.dirs[0].clone();
            dir.set(Entry::new(305, &Value::Ascii("terrio".into()), big));
            Appender::new(&mut out, big).unwrap().finish(&[dir.clone()]).unwrap();
            let out = out.into_inner();
            let first = if big {
                u64::from_le_bytes(out[8..16].try_into().unwrap())
            } else {
                u32::from_le_bytes(out[4..8].try_into().unwrap()) as u64
            };
            assert_eq!(first % 2, 0);
            let again = parse(&mut std::io::Cursor::new(&out)).unwrap();
            assert_eq!(again.dirs, vec![dir]);
            let a = crate::LazyRaster::from_tiff_bytes(bytes).unwrap().unwrap().read().unwrap();
            let b = crate::LazyRaster::from_tiff_bytes(out).unwrap().unwrap().read().unwrap();
            assert_eq!(a.raster.data.to_f64_vec(), b.raster.data.to_f64_vec());
        }
    }

    #[test]
    fn unknown_field_types_are_refused_not_dropped() {
        let mut bytes = sample(false);
        let first = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        // The first entry's field type, made one TIFF does not define.
        bytes[first + 2 + 2..first + 2 + 4].copy_from_slice(&99u16.to_le_bytes());
        let err = parse(&mut std::io::Cursor::new(bytes)).unwrap_err();
        assert!(matches!(&err, Error::Unsupported(m) if m.contains("unknown field type 99")), "{err}");
    }

    #[test]
    fn refuses_big_endian_and_loops() {
        assert!(matches!(parse(&mut std::io::Cursor::new(b"MM\0*\0\0\0\x08".to_vec())), Err(Error::Unsupported(_))));
        // A directory whose next offset is itself.
        let mut b = b"II*\0\x08\0\0\0".to_vec();
        b.extend(0u16.to_le_bytes());
        b.extend(8u32.to_le_bytes());
        assert!(parse(&mut std::io::Cursor::new(b)).is_err());
    }

    #[test]
    fn failed_updates_leave_the_file_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tif");
        std::fs::write(&path, sample(false)).unwrap();
        let before = std::fs::read(&path).unwrap();
        let r: Result<()> = update_file(&path, |f| {
            Appender::new(f, false)?.write(&[1, 2, 3])?;
            Err(Error::InvalidData("injected failure".into()))
        });
        assert!(r.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        // No temporary file is left behind.
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn read_only_and_remote_files_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.tif");
        std::fs::write(&path, sample(false)).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();
        assert!(writable_local_file(path.to_str().unwrap()).is_err());
        assert!(matches!(writable_local_file("https://example.com/a.tif"), Err(Error::Unsupported(_))));
        assert!(matches!(writable_local_file("/vsizip/a.zip/a.tif"), Err(Error::Unsupported(_))));
    }

    #[test]
    fn aggregate_metadata_budget_bounds_overlapping_tag_payloads() {
        let count = 65u16;
        let payload = 1u32 << 20;
        let offset = 8 + 2 + u32::from(count) * 12 + 4;
        let mut bytes = b"II*\0\x08\0\0\0".to_vec();
        bytes.extend(count.to_le_bytes());
        for tag in 65000..65000 + count {
            bytes.extend(tag.to_le_bytes());
            bytes.extend(1u16.to_le_bytes());
            bytes.extend(payload.to_le_bytes());
            bytes.extend(offset.to_le_bytes());
        }
        bytes.extend(0u32.to_le_bytes());
        bytes.resize((offset + payload) as usize, 0);
        let error = parse(&mut std::io::Cursor::new(bytes)).unwrap_err();
        assert!(error.to_string().contains("metadata exceeds 64 MiB"), "{error}");
    }

    #[test]
    fn invalid_utf8_ghost_metadata_never_produces_a_byte_offset() {
        let body = b"\xFFLAYOUT=IFDS_BEFORE_DATA\nKNOWN_INCOMPATIBLE_EDITION=NO\n ";
        let mut bytes = vec![0; 8];
        bytes.extend(format!("GDAL_STRUCTURAL_METADATA_SIZE={:06} bytes\n", body.len()).as_bytes());
        bytes.extend(body);
        bytes.extend([0; 16]);
        assert!(intact_cog_layout(&mut std::io::Cursor::new(bytes), false).unwrap().is_none());
    }

    #[test]
    fn cog_marker_requires_the_padding_it_overwrites() {
        for (suffix, accepted) in [("\n ", true), ("\nX", false), ("", false)] {
            let body = format!("LAYOUT=IFDS_BEFORE_DATA\nKNOWN_INCOMPATIBLE_EDITION=NO{suffix}");
            let mut bytes = vec![0; 8];
            bytes.extend(format!("GDAL_STRUCTURAL_METADATA_SIZE={:06} bytes\n", body.len()).as_bytes());
            bytes.extend(body.as_bytes());
            bytes.extend([0; 16]);
            assert_eq!(intact_cog_layout(&mut std::io::Cursor::new(bytes), false).unwrap().is_some(), accepted);
        }
    }

    #[cfg(unix)]
    #[test]
    fn replacement_follows_symlinks_and_keeps_mode() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("target");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        let link = dir.path().join("link");
        symlink(&path, &link).unwrap();
        replace_file(&link, |f| Ok(f.write_all(b"new")?)).unwrap();
        assert!(std::fs::symlink_metadata(link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(path.clone()).unwrap(), b"new");
        assert_eq!(std::fs::metadata(path).unwrap().permissions().mode() & 0o777, 0o640);
    }
}

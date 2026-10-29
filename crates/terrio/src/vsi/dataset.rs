//! Location and safety primitives shared with dataset file-set management.
use super::*;

/// Release per-operation archive handles even when a dataset operation uses the
/// low-level copy/probe helpers rather than a single public VSI call.
pub(crate) struct Operation {
    _cache: OperationScope,
}
impl Operation {
    pub(crate) fn new() -> Self {
        Self { _cache: OperationScope::new() }
    }
}

fn raw(node: &Node) -> String {
    match node {
        Node::Url { base, query } if !query.is_empty() => format!("{base}?{query}"),
        Node::Archive { archive, kind, inner } => {
            let prefix = if *kind == ArchiveKind::Zip { "/vsizip/" } else { "/vsitar/" };
            format!("{prefix}{{{}}}/{inner}", raw(archive))
        }
        _ => display_node(node),
    }
}
pub(crate) fn name(path: &str) -> Result<String> {
    Ok(Node::parse(path)?.file_name())
}
pub(crate) fn sibling(path: &str, name: &str) -> Result<String> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\']) {
        return Err(Error::InvalidArgument("dataset component must be a single file name".into()));
    }
    let parent = |s: &str| s.rsplit_once('/').map_or("", |(p, _)| p).to_string();
    let node = match Node::parse(path)? {
        Node::Local(p) => Node::Local(p.with_file_name(name)),
        Node::Mem(k) => Node::Mem(parent(&k)).join(name),
        Node::S3 { bucket, key } => Node::S3 { bucket, key: parent(&key) }.join(name),
        Node::Url { base, query } => {
            // `name` comes from the URL spelling, so do not encode it a second time.
            Node::Url { base: format!("{}/{name}", parent(&base)), query }
        }
        Node::Archive { archive, kind, inner } => Node::Archive { archive, kind, inner: join_key(&parent(&inner), name) },
    };
    Ok(raw(&node))
}
pub(crate) fn child(path: &str, name: &str) -> Result<String> {
    if unsafe_relative(name) || name.split(['/', '\\']).any(|s| s == "..") {
        return Err(Error::InvalidArgument("dataset component escapes its directory".into()));
    }
    Ok(raw(&Node::parse(path)?.join(name)))
}
pub(crate) fn siblings(path: &str) -> Result<Vec<String>> {
    let parent = |key: &str| key.rsplit_once('/').map_or("", |(parent, _)| parent).to_owned();
    let node = match Node::parse(path)? {
        Node::Local(path) => Node::Local(path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf()),
        Node::Mem(key) => Node::Mem(parent(&key)),
        Node::S3 { bucket, key } => Node::S3 { bucket, key: parent(&key) },
        Node::Url { base, query } => Node::Url { base: parent(&base), query },
        Node::Archive { archive, kind, inner } => Node::Archive { archive, kind, inner: parent(&inner) },
    };
    let directory = raw(&node);
    super::list(&directory, &Default::default())?
        .into_iter()
        .filter(|entry| !entry.is_dir())
        .map(|entry| child(&directory, &entry.name))
        .collect()
}
/// Identity for preflight name lookup. Leaf links remain distinct owned entries;
/// ordinary paths normalize parent aliases and case through canonicalization.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct NameKey(Node);
pub(crate) fn name_key(path: &str) -> Result<NameKey> {
    let mut node = Node::parse(path)?;
    if let Node::Local(path) = &mut node {
        if std::fs::symlink_metadata(&path).is_ok_and(|meta| !meta.file_type().is_symlink())
            && let Ok(canonical) = std::fs::canonicalize(&path)
        {
            *path = canonical;
        } else {
            let mut clean = PathBuf::new();
            for component in std::path::absolute(&path)?.components() {
                match component {
                    Component::ParentDir => {
                        clean.pop();
                    }
                    Component::CurDir => (),
                    other => clean.push(other.as_os_str()),
                }
            }
            *path = clean;
        }
    }
    Ok(NameKey(node))
}

/// New local Windows entries must be distinct even before they can be canonicalized.
pub(crate) fn destination_key(path: &str) -> Result<NameKey> {
    let key = name_key(path)?;
    #[cfg(windows)]
    let key = match key {
        NameKey(Node::Local(path)) => NameKey(Node::Local(PathBuf::from(path.to_string_lossy().to_uppercase()))),
        other => other,
    };
    Ok(key)
}

/// Preflight paths are stable until staging. Resolve native paths and file
/// identities once; ordered sets answer ancestor/descendant queries by depth.
#[derive(Default)]
pub(crate) struct OverlapIndex {
    native: std::collections::BTreeSet<PathBuf>,
    virtuals: std::collections::BTreeSet<String>,
    #[cfg(unix)]
    identities: std::collections::HashSet<(u64, u64)>,
    #[cfg(windows)]
    identities: std::collections::HashSet<same_file::Handle>,
}
fn url_key(base: &str) -> Option<String> {
    let (scheme, rest) = base.split_once("://")?;
    let authority = rest.split('/').next()?;
    let path = listing::uri_decode(&rest[authority.len()..]);
    let (userinfo, host) = authority.rsplit_once('@').map_or((String::new(), authority), |(u, h)| (format!("{u}@"), h));
    Some(format!("url:{}://{}{}{}", scheme.to_ascii_lowercase(), userinfo, host.to_ascii_lowercase(), path))
}
fn virtual_key(node: &Node) -> Option<String> {
    match node {
        Node::Mem(key) => Some(format!("mem:{key}")),
        Node::S3 { bucket, key } => Some(format!("s3:{bucket}/{key}")),
        Node::Url { base, .. } => url_key(base),
        _ => None,
    }
}
fn overlap_path(path: &Path) -> Result<PathBuf> {
    let resolved = absolute_path(path).ok_or_else(|| Error::InvalidArgument("cannot resolve dataset path".into()))?;
    let mut clean = PathBuf::new();
    for component in resolved.components() {
        match component {
            Component::ParentDir => {
                clean.pop();
            }
            Component::CurDir => (),
            other => clean.push(other.as_os_str()),
        }
    }
    // Resolve the deepest existing ancestor of the parent, so a symlinked directory names
    // the same place before the rest of the path exists. The leaf stays unresolved, as in
    // `name_key`, so symlink leaves remain distinct entries.
    let (Some(parent), Some(leaf)) = (clean.parent(), clean.file_name()) else {
        return Ok(clean);
    };
    let mut existing = parent.to_path_buf();
    let mut tail = vec![leaf.to_os_string()];
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            return Ok(tail.iter().rev().fold(canonical, |resolved, part| resolved.join(part)));
        }
        let (Some(name), Some(up)) = (existing.file_name(), existing.parent()) else {
            return Ok(clean);
        };
        tail.push(name.to_os_string());
        existing = up.to_path_buf();
    }
}
impl OverlapIndex {
    pub(crate) fn new(paths: &[String]) -> Result<Self> {
        let mut index = Self::default();
        for path in paths {
            index.insert(path)?;
        }
        Ok(index)
    }
    pub(crate) fn insert(&mut self, path: &str) -> Result<()> {
        let node = Node::parse(path)?;
        if let Node::Local(path) = &node {
            self.native.insert(overlap_path(path)?);
            #[cfg(unix)]
            if let Ok(meta) = std::fs::metadata(path) {
                use std::os::unix::fs::MetadataExt;
                self.identities.insert((meta.dev(), meta.ino()));
            }
            #[cfg(windows)]
            if let Ok(handle) = same_file::Handle::from_path(path) {
                self.identities.insert(handle);
            }
        } else if let Some(key) = virtual_key(&node) {
            self.virtuals.insert(key);
        }
        Ok(())
    }
    fn query(&self, path: &str, exclude_self: bool, descendants: bool) -> Result<bool> {
        let node = Node::parse(path)?;
        if let Node::Local(path) = &node {
            let canonical = overlap_path(path)?;
            if canonical.ancestors().skip(usize::from(exclude_self)).any(|parent| self.native.contains(parent)) {
                return Ok(true);
            }
            let descendant = if exclude_self {
                self.native.range((std::ops::Bound::Excluded(canonical.clone()), std::ops::Bound::Unbounded)).next()
            } else {
                self.native.range(canonical.clone()..).next()
            };
            if descendants && descendant.is_some_and(|child| child.starts_with(&canonical)) {
                return Ok(true);
            }
            if !exclude_self {
                #[cfg(unix)]
                if let Ok(meta) = std::fs::metadata(path) {
                    use std::os::unix::fs::MetadataExt;
                    if self.identities.contains(&(meta.dev(), meta.ino())) {
                        return Ok(true);
                    }
                }
                #[cfg(windows)]
                if let Ok(handle) = same_file::Handle::from_path(path)
                    && self.identities.contains(&handle)
                {
                    return Ok(true);
                }
            }
        } else if let Some(key) = virtual_key(&node) {
            if !exclude_self && self.virtuals.contains(&key) {
                return Ok(true);
            }
            let mut parent = key.as_str();
            while let Some((next, _)) = parent.rsplit_once('/') {
                if self.virtuals.contains(next) {
                    return Ok(true);
                }
                parent = next;
            }
            let prefix = format!("{}/", key.trim_end_matches('/'));
            if descendants && self.virtuals.range(prefix.clone()..).next().is_some_and(|child| child.starts_with(&prefix)) {
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub(crate) fn overlaps(&self, path: &str) -> Result<bool> {
        self.query(path, false, true)
    }
    pub(crate) fn contains_ancestor(&self, path: &str) -> Result<bool> {
        self.query(path, false, false)
    }
    pub(crate) fn overlaps_other(&self, path: &str) -> Result<bool> {
        self.query(path, true, true)
    }
}
/// Confine VRT dependencies to the requested source directory, including the
/// actual native target of links. Virtual keys with dot segments are refused;
/// callers can explicitly trust external references instead.
pub(crate) fn in_source_tree(reference: &str, source: &str) -> Result<bool> {
    fn parent(path: &str) -> &str {
        path.rsplit_once('/').map_or("", |(parent, _)| parent)
    }
    fn below(reference: &str, source: &str) -> bool {
        let clean = |path: &str| !path.split(['/', '\\']).any(|part| matches!(part, "." | ".."));
        let directory = parent(source);
        clean(reference) && clean(source) && (directory.is_empty() || reference.starts_with(&format!("{directory}/")))
    }
    let (reference, source) = (Node::parse(reference)?, Node::parse(source)?);
    Ok(match (reference, source) {
        (Node::Local(reference), Node::Local(source)) => {
            let source_parent = source.parent().filter(|path| !path.as_os_str().is_empty()).unwrap_or(Path::new("."));
            let directory = std::fs::canonicalize(source_parent)?;
            std::fs::canonicalize(&reference)
                .map_err(|error| {
                    if error.kind() == std::io::ErrorKind::NotFound {
                        Error::InvalidArgument(format!("VRT source file does not exist: {}", reference.display()))
                    } else {
                        error.into()
                    }
                })?
                .starts_with(directory)
        }
        (Node::Mem(reference), Node::Mem(source)) => below(&reference, &source),
        (Node::S3 { bucket: a, key: reference }, Node::S3 { bucket: b, key: source }) => a == b && below(&reference, &source),
        (Node::Url { base: reference, .. }, Node::Url { base: source, .. }) => {
            // Encoded slashes may be literal server path characters rather than
            // separators. Decoding them cannot establish directory confinement.
            let encoded_slash = |base: &str| {
                base.split_once("://")
                    .and_then(|(_, rest)| rest.split_once('/'))
                    .is_some_and(|(_, path)| path.as_bytes().windows(3).any(|part| part.eq_ignore_ascii_case(b"%2f")))
            };
            if encoded_slash(&reference) || encoded_slash(&source) {
                false
            } else {
                match (url_key(&reference), url_key(&source)) {
                    (Some(reference), Some(source)) => below(&reference, &source),
                    _ => false,
                }
            }
        }
        (Node::Archive { archive: a, kind: ak, inner: reference }, Node::Archive { archive: b, kind: bk, inner: source }) => {
            ak == bk && a == b && below(&reference, &source)
        }
        _ => false,
    })
}
pub(crate) fn same_name(a: &str, b: &str) -> Result<bool> {
    let (a, b) = (Node::parse(a)?, Node::parse(b)?);
    if let (Node::Local(a), Node::Local(b)) = (&a, &b) {
        fn lexical(path: &Path) -> std::io::Result<PathBuf> {
            let mut clean = PathBuf::new();
            for component in std::path::absolute(path)?.components() {
                match component {
                    Component::ParentDir => {
                        clean.pop();
                    }
                    Component::CurDir => (),
                    other => clean.push(other.as_os_str()),
                }
            }
            Ok(clean)
        }
        if lexical(a)? == lexical(b)? {
            return Ok(true);
        }
        // Normalize spelling through parent links and case-insensitive filesystems,
        // without conflating distinct owned symlink entries or hard-link names.
        if [a, b].iter().all(|p| std::fs::symlink_metadata(p).is_ok_and(|m| !m.file_type().is_symlink()))
            && let (Ok(a), Ok(b)) = (std::fs::canonicalize(a), std::fs::canonicalize(b))
        {
            return Ok(a == b);
        }
        // A dangling link cannot be canonicalized. On case-insensitive Unix
        // filesystems, identify case aliases of that directory entry without
        // following the link or merging distinct hard-link names.
        #[cfg(unix)]
        if let (Some(an), Some(bn), Some(ap), Some(bp)) = (a.file_name(), b.file_name(), a.parent(), b.parent())
            && an.to_string_lossy().eq_ignore_ascii_case(&bn.to_string_lossy())
            && let (Ok(am), Ok(bm), Ok(ap), Ok(bp)) =
                (std::fs::symlink_metadata(a), std::fs::symlink_metadata(b), std::fs::canonicalize(ap), std::fs::canonicalize(bp))
        {
            use std::os::unix::fs::MetadataExt;
            if am.file_type().is_symlink() && bm.file_type().is_symlink() && ap == bp && am.dev() == bm.dev() && am.ino() == bm.ino() {
                let mut exact_names = 0;
                for entry in std::fs::read_dir(ap)? {
                    let name = entry?.file_name();
                    exact_names += usize::from(name == an || name == bn);
                }
                return Ok(exact_names <= 1);
            }
        }
        return Ok(false);
    }
    // Query parameters can select different representations of one HTTP path.
    Ok(a == b)
}
pub(crate) fn check_remove(path: &str) -> Result<()> {
    let node = Node::parse(path)?;
    check_deletable(path, &node, "manage dataset")
}
pub(crate) fn check_write(path: &str) -> Result<()> {
    let node = Node::parse(path)?;
    if let Some(reason) = node.read_only_reason() {
        return Err(Error::Unsupported(format!("'{}': {reason}", listing::redact_url(path))));
    }
    check_deletable(path, &node, "replace dataset")?;
    if let Node::Local(p) = node {
        match std::fs::symlink_metadata(p) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(Error::InvalidArgument("dataset destination is a symbolic link".into()));
            }
            Ok(meta) if !meta.is_file() && !meta.is_dir() => {
                return Err(Error::InvalidArgument("dataset destination is not a regular file or directory".into()));
            }
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
            _ => (),
        }
    }
    Ok(())
}
/// Read only the identifying prefix, preserving range requests for cloud sources.
pub(crate) fn prefix(path: &str, size: usize) -> Result<Vec<u8>> {
    let node = Node::parse(path)?;
    fn regular(node: &Node) -> Result<()> {
        check_regular_source(node)?;
        if let Node::Archive { archive, .. } = node {
            regular(archive)?;
        }
        Ok(())
    }
    regular(&node)?;
    match node {
        Node::Local(p) => {
            let mut bytes = Vec::new();
            std::fs::File::open(p)?.take(size as u64).read_to_end(&mut bytes)?;
            Ok(bytes)
        }
        Node::Mem(k) => {
            let bytes = crate::memfs::read_shared(Path::new(&k))?;
            Ok(bytes.as_ref()[..size.min(bytes.as_ref().len())].to_vec())
        }
        #[cfg(remote)]
        Node::S3 { .. } | Node::Url { .. } => Ok(crate::http::read_prefix(&node.remote().unwrap(), size)?.0),
        Node::Archive { .. } => {
            struct Prefix {
                bytes: Vec<u8>,
                size: usize,
                stopped: bool,
            }
            impl Write for Prefix {
                fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                    let n = data.len().min(self.size - self.bytes.len());
                    self.bytes.extend_from_slice(&data[..n]);
                    if n < data.len() {
                        self.stopped = true;
                        return Err(std::io::Error::other("prefix complete"));
                    }
                    Ok(n)
                }
                fn flush(&mut self) -> std::io::Result<()> {
                    Ok(())
                }
            }
            let mut out = Prefix { bytes: Vec::new(), size, stopped: false };
            let result = read_into(&node, &mut out);
            if !out.stopped {
                result?;
            }
            Ok(out.bytes)
        }
        #[cfg(not(remote))]
        _ => Err(Error::Unsupported(remote_disabled_message())),
    }
}

/// A bounded cache of an archive member. Stored ZIP members are seeked directly;
/// other members stream/discard preceding bytes, stopping at the requested block.
/// Partial metadata reads do not validate a whole-member CRC.
struct MemberReader {
    node: Node,
    len: u64,
    pos: u64,
    block_start: u64,
    block: Vec<u8>,
}
impl MemberReader {
    fn open(node: Node) -> Result<Self> {
        let len = stat_node(&node)?
            .filter(|stat| !stat.dir)
            .and_then(|stat| stat.size)
            .ok_or_else(|| Error::InvalidData("archive member has no file size".into()))?;
        Ok(Self { node, len, pos: 0, block_start: 0, block: Vec::new() })
    }
    fn fetch(&self, offset: u64, len: usize) -> Result<Vec<u8>> {
        if let Node::Archive { archive, kind: ArchiveKind::Zip, inner } = &self.node {
            let direct = with_archive_reader(archive, true, |reader| {
                let index = zip_index(archive, reader)?;
                let entry = index.by_name.get(inner).map(|&i| &index.entries[i]).ok_or_else(|| not_found(inner))?;
                if entry.method != 0 {
                    return Ok(None);
                }
                if entry.flags & 1 != 0 {
                    return Err(Error::Unsupported("encrypted ZIP members are unsupported".into()));
                }
                if entry.compressed_size != entry.uncompressed_size {
                    return Err(Error::InvalidData("stored ZIP member sizes differ".into()));
                }
                let start = zipdir::data_offset(reader, entry)?
                    .checked_add(offset)
                    .ok_or_else(|| Error::InvalidData("archive member offset overflows".into()))?;
                reader.seek(std::io::SeekFrom::Start(start))?;
                let mut bytes = vec![0; len];
                reader.read_exact(&mut bytes)?;
                Ok(Some(bytes))
            })?;
            if let Some(bytes) = direct {
                return Ok(bytes);
            }
        }
        struct Range {
            skip: u64,
            remaining: usize,
            bytes: Vec<u8>,
            stopped: bool,
        }
        impl Write for Range {
            fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
                let skip = self.skip.min(data.len() as u64) as usize;
                self.skip -= skip as u64;
                let n = self.remaining.min(data.len() - skip);
                self.bytes.extend_from_slice(&data[skip..skip + n]);
                self.remaining -= n;
                if skip + n < data.len() {
                    self.stopped = true;
                    return Err(std::io::Error::other("archive range complete"));
                }
                Ok(data.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut range = Range { skip: offset, remaining: len, bytes: Vec::with_capacity(len), stopped: false };
        let result = read_into(&self.node, &mut range);
        if !range.stopped {
            result?;
        }
        if range.skip != 0 || range.remaining != 0 {
            return Err(Error::InvalidData("archive member ended before requested metadata".into()));
        }
        Ok(range.bytes)
    }
}
impl Read for MemberReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if out.is_empty() || self.pos >= self.len {
            return Ok(0);
        }
        if self.block.is_empty() || self.pos < self.block_start || self.pos - self.block_start >= self.block.len() as u64 {
            self.block_start = self.pos / (16 << 10) * (16 << 10);
            let len = (self.len - self.block_start).min(16 << 10) as usize;
            self.block = self.fetch(self.block_start, len).map_err(std::io::Error::other)?;
        }
        let start = (self.pos - self.block_start) as usize;
        let n = out.len().min(self.block.len() - start);
        out[..n].copy_from_slice(&self.block[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}
impl std::io::Seek for MemberReader {
    fn seek(&mut self, from: std::io::SeekFrom) -> std::io::Result<u64> {
        let pos = match from {
            std::io::SeekFrom::Start(pos) => i128::from(pos),
            std::io::SeekFrom::Current(offset) => i128::from(self.pos) + i128::from(offset),
            std::io::SeekFrom::End(offset) => i128::from(self.len) + i128::from(offset),
        };
        self.pos = u64::try_from(pos).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid archive member seek"))?;
        Ok(self.pos)
    }
}

/// Read a file through its native, memory, HTTP/object-store or archive reader.
/// Consumers choose the metadata ranges they need rather than materializing it.
pub(crate) fn with_seekable<T>(path: &str, operation: impl FnOnce(&mut dyn ReadSeek) -> Result<T>) -> Result<T> {
    let node = Node::parse(path)?;
    match node {
        Node::Local(p) => operation(&mut std::io::BufReader::new(std::fs::File::open(p)?)),
        Node::Mem(k) => operation(&mut std::io::Cursor::new(crate::memfs::read_shared(Path::new(&k))?)),
        #[cfg(remote)]
        Node::Url { .. } | Node::S3 { .. } => operation(&mut crate::http::RangeReader::open(&node.remote().unwrap())?),
        Node::Archive { .. } => operation(&mut MemberReader::open(node)?),
        #[cfg(not(remote))]
        _ => Err(Error::Unsupported(remote_disabled_message())),
    }
}
pub(crate) fn tiff_has_transform(path: &str) -> Result<bool> {
    with_seekable(path, |reader| {
        crate::geotiff::read_geo_tags_from(reader)
            .map(|tags| tags.geo_transform.is_some())
            .ok_or_else(|| Error::InvalidData("cannot read TIFF georeferencing tags".into()))
    })
}

/// A portable column-chunk path, relative to the output metadata file.
#[cfg(feature = "parquet")]
pub(crate) fn relative_file(input: &str, output: &str) -> Result<String> {
    fn parts(input: &[String], directory: &[String]) -> Result<String> {
        let common = input.iter().zip(directory).take_while(|(a, b)| a == b).count();
        let mut result: Vec<_> = directory[common..].iter().map(|_| "..".to_owned()).collect();
        result.extend_from_slice(&input[common..]);
        if result.is_empty() {
            return Err(Error::InvalidArgument("metadata output and input have the same path".into()));
        }
        Ok(result.join("/"))
    }
    fn keys(input: &str, output: &str) -> Result<String> {
        let split = |key: &str| -> Result<Vec<String>> {
            if key.split('/').any(|part| matches!(part, "." | "..")) {
                return Err(Error::InvalidArgument("Parquet partition keys must not contain dot segments".into()));
            }
            Ok(key.split('/').filter(|part| !part.is_empty()).map(str::to_owned).collect())
        };
        let input = split(input)?;
        let mut output = split(output)?;
        output.pop();
        parts(&input, &output)
    }
    let unsupported = || Error::InvalidArgument("Parquet inputs and metadata output must use the same filesystem".into());
    match (Node::parse(input)?, Node::parse(output)?) {
        (Node::Local(input), Node::Local(output)) => {
            let input = overlap_path(&input)?;
            let output = overlap_path(&output)?;
            let directory = output.parent().ok_or_else(unsupported)?;
            let input: Vec<_> = input.components().collect();
            let directory: Vec<_> = directory.components().collect();
            if input.first() != directory.first() {
                return Err(unsupported());
            }
            let strings = |components: &[Component<'_>]| {
                components
                    .iter()
                    .map(|part| {
                        part.as_os_str()
                            .to_str()
                            .map(str::to_owned)
                            .ok_or_else(|| Error::InvalidArgument("Parquet partition paths must be UTF-8".into()))
                    })
                    .collect::<Result<Vec<_>>>()
            };
            parts(&strings(&input)?, &strings(&directory)?)
        }
        (Node::Mem(input), Node::Mem(output)) => keys(&input, &output),
        (Node::S3 { bucket: a, key: input }, Node::S3 { bucket: b, key: output }) if a == b => keys(&input, &output),
        (Node::Url { base: input, query }, Node::Url { base: output, .. }) => {
            if !query.is_empty() {
                return Err(Error::InvalidArgument(
                    "query-bearing source URLs cannot be stored as portable Parquet paths; use environment credentials".into(),
                ));
            }
            let split = |url: String| -> Result<(String, String)> {
                let (scheme, rest) = url.split_once("://").ok_or_else(unsupported)?;
                let (host, path) = rest.split_once('/').unwrap_or((rest, ""));
                Ok((format!("{scheme}://{host}"), path.to_owned()))
            };
            let (a, input) = split(input)?;
            let (b, output) = split(output)?;
            if a != b {
                return Err(unsupported());
            }
            // file_path is a logical filesystem path, not URL-escaped transport text.
            keys(&listing::uri_decode(&input), &listing::uri_decode(&output))
        }
        (Node::Archive { archive: a, kind: ak, inner: input }, Node::Archive { archive: b, kind: bk, inner: output })
            if a == b && ak == bk =>
        {
            keys(&input, &output)
        }
        _ => Err(unsupported()),
    }
}
/// Resolve existing external column-chunk paths without reading those files.
#[cfg(feature = "parquet")]
pub(crate) fn resolve_file(input: &str, reference: &str) -> Result<String> {
    let parsed = Node::parse(reference)?;
    if !matches!(&parsed, Node::Local(path) if !path.is_absolute()) {
        return Ok(raw(&parsed));
    }
    fn joined(key: &str, reference: &str) -> Result<String> {
        let parent = key.rsplit_once('/').map_or("", |(parent, _)| parent);
        let mut parts: Vec<_> = parent.split('/').filter(|part| !part.is_empty()).collect();
        for part in reference.split('/') {
            match part {
                "" | "." => (),
                ".." => {
                    if parts.pop().is_none() {
                        return Err(Error::InvalidData("Parquet reference escapes its filesystem root".into()));
                    }
                }
                _ => parts.push(part),
            }
        }
        Ok(format!("{}{}", if key.starts_with('/') { "/" } else { "" }, parts.join("/")))
    }
    let node = match Node::parse(input)? {
        Node::Local(path) => Node::Local(path.parent().unwrap_or(Path::new(".")).join(reference)),
        Node::Mem(key) => Node::Mem(joined(&key, reference)?),
        Node::S3 { bucket, key } => Node::S3 { bucket, key: joined(&key, reference)? },
        Node::Url { base, query } => {
            let (scheme, rest) = base.split_once("://").ok_or_else(|| Error::InvalidArgument("invalid source URL".into()))?;
            let (host, key) = rest.split_once('/').unwrap_or((rest, ""));
            let logical = joined(&listing::uri_decode(key), reference)?;
            Node::Url { base: format!("{scheme}://{host}/{}", listing::uri_encode(&logical, false)), query }
        }
        Node::Archive { archive, kind, inner } => Node::Archive { archive, kind, inner: joined(&inner, reference)? },
    };
    Ok(raw(&node))
}

/// Rename accounting for payload copies rather than the logical file size moved.
pub(crate) fn rename(src: &str, dst: &str) -> Result<Report> {
    let (source, target) = (Node::parse(src)?, Node::parse(dst)?);
    // Staging another part can temporarily leave a link dangling. Native rename
    // must move the link itself, without first following it through stat().
    if let (Node::Local(a), Node::Local(b)) = (&source, &target)
        && std::fs::symlink_metadata(a).is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        check_deletable(src, &source, "rename dataset link")?;
        check_write(dst)?;
        if let Some(parent) = b.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        return match std::fs::rename(a, b) {
            Ok(()) => Ok(Report { copied: 1, ..Default::default() }),
            Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
                Err(Error::Unsupported("symbolic links cannot be renamed across filesystems".into()))
            }
            Err(error) => Err(error.into()),
        };
    }
    super::rename_counted(src, dst, false)
}
pub(crate) fn entry_exists(path: &str) -> Result<bool> {
    match Node::parse(path)? {
        Node::Local(path) => match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        },
        node => Ok(stat_node(&node)?.is_some()),
    }
}

/// A companion may itself be a dangling link, which still belongs to its dataset.
/// Ordinary directories with companion-looking names remain excluded.
pub(crate) fn companion_exists(path: &str) -> Result<bool> {
    match Node::parse(path)? {
        Node::Local(path) => match std::fs::symlink_metadata(path) {
            Ok(metadata) => Ok(metadata.is_file() || metadata.file_type().is_symlink()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
        },
        _ => file_exists(path),
    }
}

/// Probe an exact companion file, without treating a missing file as a directory index.
pub(crate) fn file_exists(path: &str) -> Result<bool> {
    let node = Node::parse(path)?;
    match node {
        Node::Local(p) => Ok(std::fs::metadata(p).is_ok_and(|metadata| metadata.is_file())),
        #[cfg(remote)]
        Node::Url { .. } | Node::S3 { .. } => Ok(crate::http::store::stat(&node.remote().unwrap())?.is_some()),
        _ => Ok(stat_node(&node)?.is_some_and(|stat| !stat.dir)),
    }
}

/// The inventory already established that this is a file. Avoid probing its type again
/// when streaming to an ordinary output; directory/cloud copies retain VSI dispatch.
pub(crate) fn copy(src: &str, dst: &str, directory: bool) -> Result<Report> {
    let (source, target) = (Node::parse(src)?, Node::parse(dst)?);
    if !directory && matches!(target, Node::Local(_) | Node::Mem(_)) {
        let bytes = copy_file(&source, &target, None)?;
        return Ok(Report { copied: 1, bytes, ..Default::default() });
    }
    super::copy(src, dst, &CopyOptions { recursive: directory, skip_errors: false })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom};

    #[test]
    fn url_overlap_identity_ignores_host_case_but_preserves_path_case() {
        let index = OverlapIndex::new(&["https://ACC.blob.core.windows.net/c/X.tif?token=one".into()]).unwrap();
        assert!(index.overlaps("https://acc.blob.core.windows.net/c/X.tif?token=two").unwrap());
        assert!(!index.overlaps("https://acc.blob.core.windows.net/c/x.tif").unwrap());
        assert!(index.overlaps("https://acc.blob.core.windows.net/c/%58.tif").unwrap());
        let encoded = OverlapIndex::new(&["https://acc.blob.core.windows.net/c/a%20b.tif".into()]).unwrap();
        assert!(encoded.overlaps("https://ACC.blob.core.windows.net/c/a b.tif").unwrap());
        assert!(!encoded.overlaps("https://acc.blob.core.windows.net/c/a%2520b.tif").unwrap());
        let root = OverlapIndex::new(&["https://ACC.blob.core.windows.net".into()]).unwrap();
        assert!(root.overlaps("https://acc.blob.core.windows.net/c/X.tif").unwrap());
    }
    #[cfg(unix)]
    #[test]
    fn case_aliases_of_dangling_links_do_not_merge_distinct_directory_entries() {
        let dir = tempfile::tempdir().unwrap();
        let lower = dir.path().join("type.csvt");
        let upper = dir.path().join("type.CSVT");
        std::os::unix::fs::symlink("missing-target", &lower).unwrap();
        if std::fs::symlink_metadata(&upper).is_ok() {
            assert!(same_name(lower.to_str().unwrap(), upper.to_str().unwrap()).unwrap());
        } else {
            // A case-sensitive filesystem can own both names, even with one inode.
            std::fs::hard_link(&lower, &upper).unwrap();
            assert!(!same_name(lower.to_str().unwrap(), upper.to_str().unwrap()).unwrap());
        }
        let separate = dir.path().join("other.csvt");
        std::os::unix::fs::symlink("missing-target", &separate).unwrap();
        assert!(!same_name(lower.to_str().unwrap(), separate.to_str().unwrap()).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_parents_alias_inputs_before_the_destination_exists() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir_all(real.join("ds.zarr/sub")).unwrap();
        std::os::unix::fs::symlink(&real, dir.path().join("link")).unwrap();
        let index = OverlapIndex::new(&[real.join("ds.zarr").to_str().unwrap().into()]).unwrap();
        let alias = dir.path().join("link/ds.zarr/sub/copy.zarr");
        assert!(index.overlaps(alias.to_str().unwrap()).unwrap());
        assert!(!index.overlaps(dir.path().join("link/other.zarr").to_str().unwrap()).unwrap());
    }
    #[test]
    fn destination_identity_matches_platform_case_policy() {
        let dir = tempfile::tempdir().unwrap();
        let a = destination_key(dir.path().join("out.dbf").to_str().unwrap()).unwrap();
        let b = destination_key(dir.path().join("out.DBF").to_str().unwrap()).unwrap();
        assert_eq!(a == b, cfg!(windows));
    }
    #[test]
    fn member_cache_handles_boundaries_backward_seeks_and_eof() {
        let bytes: Vec<_> = (0..2_000_003usize).map(|i| (i % 251) as u8).collect();
        for method in [zip::CompressionMethod::Stored, zip::CompressionMethod::Deflated] {
            let _operation = Operation::new();
            let path = format!("/vsimem/member-cache-{method:?}.zip");
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            zip.start_file("data.bin", zip::write::SimpleFileOptions::default().compression_method(method)).unwrap();
            zip.write_all(&bytes).unwrap();
            super::super::write(&path, zip.finish().unwrap().get_ref()).unwrap();
            let mut member = MemberReader::open(Node::parse(&format!("/vsizip/{path}/data.bin")).unwrap()).unwrap();
            for (offset, size) in [(16_380, 32), (1_700_003, 29), (3, 12), (1_999_990, 13)] {
                member.seek(SeekFrom::Start(offset)).unwrap();
                let mut out = vec![0; size];
                member.read_exact(&mut out).unwrap();
                assert_eq!(out, bytes[offset as usize..offset as usize + size]);
                assert!(member.block.len() <= 16 << 10);
            }
            assert_eq!(member.read(&mut [0; 1]).unwrap(), 0);
            assert_eq!(member.seek(SeekFrom::End(-3)).unwrap(), bytes.len() as u64 - 3);
            let mut tail = Vec::new();
            member.read_to_end(&mut tail).unwrap();
            assert_eq!(tail, bytes[bytes.len() - 3..]);
            assert!(member.seek(SeekFrom::Current(-3_000_000)).is_err());
            super::super::delete(&path, false).unwrap();
        }
    }

    #[test]
    fn reference_trees_confine_virtual_roots_hosts_archives_and_encoded_traversal() {
        for (reference, source) in [
            ("/vsimem/work/source.raw", "/vsimem/work/source.vrt"),
            ("s3://bucket/source.raw", "s3://bucket/source.vrt"),
            ("https://example.invalid/work/sub/source.tif", "https://example.invalid/work/source.vrt"),
            ("https://EXAMPLE.invalid/work/sub/source.tif", "https://example.INVALID/work/source.vrt"),
            ("/vsizip/{/vsimem/archive.zip}/source.tif", "/vsizip/{/vsimem/archive.zip}/source.vrt"),
        ] {
            assert!(in_source_tree(reference, source).unwrap(), "{reference}");
        }
        for (reference, source) in [
            ("/vsimem/work2/source.raw", "/vsimem/work/source.vrt"),
            ("/vsimem/work/../source.raw", "/vsimem/work/source.vrt"),
            ("s3://other/source.raw", "s3://bucket/source.vrt"),
            ("https://elsewhere.invalid/work/source.tif", "https://example.invalid/work/source.vrt"),
            ("https://example.invalid/work%2fsecret/source.tif", "https://example.invalid/work/source.vrt"),
            ("https://example.invalid/work%2Fsecret/source.tif", "https://example.invalid/work/source.vrt"),
            ("https://example.invalid/work/inside%2fpath/source.tif", "https://example.invalid/work/source.vrt"),
            ("https://example.invalid/work/a/source.tif", "https://example.invalid/work%2fa/source.vrt"),
            ("https://example.invalid/work/%2e%2e/private.tif", "https://example.invalid/work/source.vrt"),
            ("/vsizip/{/vsimem/other.zip}/source.tif", "/vsizip/{/vsimem/archive.zip}/source.vrt"),
            ("/etc/private.raw", "/vsimem/work/source.vrt"),
        ] {
            assert!(!in_source_tree(reference, source).unwrap_or(false), "{reference}");
        }
    }
    #[test]
    fn virtual_overlap_queries_preserve_roots_queries_and_name_boundaries() {
        let inputs = vec![
            "/vsimem/tiles/a.tif".into(),
            "/vsimem/store.zarr".into(),
            "https://example.invalid/data.tif?version=one".into(),
            "s3://bucket/prefix/data.tif".into(),
        ];
        let index = OverlapIndex::new(&inputs).unwrap();
        for path in [
            "/vsimem/tiles",
            "/vsimem/store.zarr/chunk",
            "/vsimem/store.zarr",
            "https://example.invalid/data.tif?version=two",
            "s3://bucket/prefix",
        ] {
            assert!(index.overlaps(path).unwrap(), "{path}");
        }
        for path in ["/vsimem/tiles/a.tif2", "/vsimem/store.zarr2", "https://elsewhere.invalid/data.tif", "s3://other/prefix"] {
            assert!(!index.overlaps(path).unwrap(), "{path}");
        }
        assert!(!index.overlaps_other("/vsimem/store.zarr").unwrap());
        assert!(index.overlaps_other("/vsimem/tiles").unwrap());
        assert_ne!(
            name_key("https://example.invalid/data.tif?version=one").unwrap(),
            name_key("https://example.invalid/data.tif?version=two").unwrap()
        );
    }

    #[test]
    #[cfg(not(target_family = "wasm"))]
    fn indexed_native_paths_retain_alias_and_ancestor_guards() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let input = root.join("input.csv");
        std::fs::write(&input, b"x,y\n1,2\n").unwrap();
        std::fs::create_dir(root.join("store.zarr")).unwrap();
        let files = vec![input.to_str().unwrap().into(), root.join("store.zarr").to_str().unwrap().into()];
        let index = OverlapIndex::new(&files).unwrap();
        for path in [root.to_path_buf(), input.clone(), root.join("store.zarr/0.0")] {
            assert!(index.overlaps(path.to_str().unwrap()).unwrap(), "{}", path.display());
        }
        // Windows can normalize the missing component lexically; POSIX fails
        // traversal. Both outcomes must prevent treating the input as unrelated.
        assert!(!matches!(index.overlaps(root.join("new/../input.csv").to_str().unwrap()), Ok(false)));
        assert!(!index.overlaps(root.join("input.csv2").to_str().unwrap()).unwrap());
        assert!(!index.overlaps_other(root.join("store.zarr").to_str().unwrap()).unwrap());
        #[cfg(any(unix, windows))]
        {
            let link = root.join("hard.csv");
            std::fs::hard_link(&input, &link).unwrap();
            assert!(index.overlaps(link.to_str().unwrap()).unwrap());
            assert_ne!(name_key(input.to_str().unwrap()).unwrap(), name_key(link.to_str().unwrap()).unwrap());
        }
        #[cfg(unix)]
        {
            let link = root.join("alias.csv");
            std::os::unix::fs::symlink(&input, &link).unwrap();
            assert!(index.overlaps(link.to_str().unwrap()).unwrap());
            assert_ne!(name_key(input.to_str().unwrap()).unwrap(), name_key(link.to_str().unwrap()).unwrap());
        }
    }
}

#[cfg(all(test, feature = "parquet"))]
mod parquet_path_tests {
    use super::*;
    #[test]
    fn relative_partition_names_and_namespace_guards() {
        for root in ["/vsimem/paths", "/vsis3/bucket/paths", "https://example.org/paths"] {
            let input = format!("{root}/a.parquet");
            let output = format!("{root}/nested/_metadata");
            assert_eq!(relative_file(&input, &output).unwrap(), "../a.parquet");
            assert_eq!(resolve_file(&output, "../a.parquet").unwrap(), raw(&Node::parse(&input).unwrap()));
        }
        assert!(relative_file("/vsis3/a/part.parquet", "/vsis3/b/_metadata").is_err());
        assert!(relative_file("https://a.example/part.parquet", "https://b.example/_metadata").is_err());
        assert!(relative_file("https://a.example/part.parquet?token=private", "https://a.example/_metadata").is_err());
        assert!(relative_file("/vsimem/part.parquet", "_metadata").is_err());
        assert_eq!(resolve_file("https://example.org/p/_metadata", "a b.parquet").unwrap(), "https://example.org/p/a%20b.parquet");
        assert_eq!(resolve_file("https://example.org/p/_metadata", "a%20b.parquet").unwrap(), "https://example.org/p/a%2520b.parquet");
        assert_eq!(relative_file("https://example.org/p/a%20b.parquet", "https://example.org/p/_metadata").unwrap(), "a b.parquet");
        assert_eq!(relative_file("https://example.org/p/a%2520b.parquet", "https://example.org/p/_metadata").unwrap(), "a%20b.parquet");
        assert_eq!(relative_file("/vsis3/b/p/a%20b.parquet", "/vsis3/b/p/_metadata").unwrap(), "a%20b.parquet");
    }
}

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
        return Ok(false);
    }
    // Query parameters can select different representations of one HTTP path.
    Ok(a == b)
}
pub(crate) fn overlaps(a: &str, b: &str) -> Result<bool> {
    let (a_node, b_node) = (Node::parse(a)?, Node::parse(b)?);
    #[cfg(windows)]
    if let (Node::Local(a), Node::Local(b)) = (&a_node, &b_node)
        && same_file::is_same_file(a, b).unwrap_or(false)
    {
        return Ok(true);
    }
    Ok(is_within(&a_node, &b_node) || is_within(&b_node, &a_node) || crate::ops::strips::overwrites(a, &[b.into()]))
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

pub(crate) fn tiff_has_transform(path: &str) -> Result<bool> {
    fn read(reader: impl Read + std::io::Seek) -> Result<bool> {
        crate::geotiff::read_geo_tags_from(reader)
            .map(|tags| tags.geo_transform.is_some())
            .ok_or_else(|| Error::InvalidData("cannot read TIFF georeferencing tags".into()))
    }
    let node = Node::parse(path)?;
    match node {
        Node::Local(p) => read(std::io::BufReader::new(std::fs::File::open(p)?)),
        Node::Mem(k) => read(std::io::Cursor::new(crate::memfs::read_shared(Path::new(&k))?)),
        #[cfg(remote)]
        Node::Url { .. } | Node::S3 { .. } => read(crate::http::RangeReader::open(&node.remote().unwrap())?),
        Node::Archive { .. } => read(std::io::Cursor::new(super::read(path)?)),
        #[cfg(not(remote))]
        _ => Err(Error::Unsupported(remote_disabled_message())),
    }
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

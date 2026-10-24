//! A small HDF5 writer: what netCDF-4 files need, laid out as the HDF5 library lays out files
//! compatible with HDF5 1.8 (superblock version 2, version 2 object headers, version 1 B-trees
//! for chunks), so that the netCDF-C library and h5py read them.
//!
//! - groups (the root, and those of datasets named by a path such as `group/sub/var`) holding
//!   datasets and subgroups, with link and attribute creation order tracked;
//! - datasets of fixed-point, floating-point and fixed-length string types, contiguous or
//!   chunked, optionally with the shuffle and deflate filters;
//! - attributes of those types, plus object references for dimension scales: variable-length
//!   lists of references (`DIMENSION_LIST`, in a global heap) and compound lists of references and
//!   indices (`REFERENCE_LIST`).
//!
//! The format is described at <https://docs.hdfgroup.org/hdf5/develop/_f_m_t3.html>.

use std::io::Write as _;

/// The undefined address.
const UNDEF: u64 = u64::MAX;
/// The B-tree "K" of chunk indexes in files without a superblock extension (HDF5's default).
const CHUNK_BTREE_K: usize = 32;
/// The smallest global heap collection the HDF5 library reads.
const GLOBAL_HEAP_SIZE: usize = 4096;

/// An HDF5 datatype.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum H5Type {
    /// A little-endian integer of `size` bytes.
    Int { size: u8, signed: bool },
    /// A little-endian IEEE float of 4 or 8 bytes.
    Float { size: u8 },
    /// A fixed-length, NUL-terminated ASCII string.
    Str { len: u32 },
    /// An object reference.
    Ref,
    /// A variable-length sequence of object references.
    RefVlen,
    /// The dimension scales' `{dataset reference, dimension index}` compound.
    RefList,
}

impl H5Type {
    fn size(&self) -> u32 {
        match self {
            Self::Int { size, .. } | Self::Float { size } => *size as u32,
            Self::Str { len } => (*len).max(1),
            Self::Ref => 8,
            Self::RefVlen => 16,
            Self::RefList => 12,
        }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let header = |out: &mut Vec<u8>, class: u8, version: u8, flags: u32, size: u32| {
            out.push(class | (version << 4));
            out.extend_from_slice(&flags.to_le_bytes()[..3]);
            out.extend_from_slice(&size.to_le_bytes());
        };
        match self {
            Self::Int { size, signed } => {
                header(out, 0, 1, if *signed { 0x08 } else { 0 }, *size as u32);
                out.extend_from_slice(&0u16.to_le_bytes());
                out.extend_from_slice(&(*size as u16 * 8).to_le_bytes());
            }
            Self::Float { size } => {
                let (sign, exp_loc, exp_size, man_size, bias) = if *size == 4 { (31, 23, 8, 23, 127u32) } else { (63, 52, 11, 52, 1023) };
                // Implied leading mantissa bit (normalization 2) and the sign bit's position.
                header(out, 1, 1, 0x20 | (sign << 8), *size as u32);
                out.extend_from_slice(&0u16.to_le_bytes());
                out.extend_from_slice(&(*size as u16 * 8).to_le_bytes());
                out.extend_from_slice(&[exp_loc, exp_size, 0, man_size]);
                out.extend_from_slice(&bias.to_le_bytes());
            }
            Self::Str { .. } => header(out, 3, 1, 0, self.size()),
            Self::Ref => header(out, 7, 1, 0, 8),
            Self::RefVlen => {
                header(out, 9, 1, 0, 16);
                Self::Ref.encode(out);
            }
            Self::RefList => {
                header(out, 6, 3, 2, 12);
                out.extend_from_slice(b"dataset\0");
                out.push(0);
                Self::Ref.encode(out);
                out.extend_from_slice(b"dimension\0");
                out.push(8);
                Self::Int { size: 4, signed: false }.encode(out);
            }
        }
    }
}

/// An attribute's or dataset's values.
#[derive(Debug, Clone)]
pub(crate) enum Values {
    /// Little-endian values of the type.
    Bytes(Vec<u8>),
    /// One list of one reference per element: the datasets (by index) of a `DIMENSION_LIST`.
    DimensionList(Vec<usize>),
    /// The `(dataset index, dimension)` pairs of a `REFERENCE_LIST`.
    ReferenceList(Vec<(usize, u32)>),
}

/// An attribute.
#[derive(Debug, Clone)]
pub(crate) struct Attr {
    pub name: String,
    pub dtype: H5Type,
    /// `None` for a scalar.
    pub shape: Option<Vec<u64>>,
    pub values: Values,
}

impl Attr {
    /// A scalar string attribute.
    pub fn text(name: &str, s: &str) -> Self {
        Self { name: name.into(), dtype: H5Type::Str { len: s.len() as u32 }, shape: None, values: Values::Bytes(s.as_bytes().to_vec()) }
    }

    /// A one-dimensional numeric attribute of little-endian values.
    pub fn numbers(name: &str, dtype: H5Type, n: usize, le_bytes: Vec<u8>) -> Self {
        Self { name: name.into(), dtype, shape: Some(vec![n as u64]), values: Values::Bytes(le_bytes) }
    }
}

/// How a dataset's values are stored.
#[derive(Debug, Clone, Default)]
pub(crate) struct Storage {
    /// Chunk dimensions; contiguous when `None`.
    pub chunks: Option<Vec<u64>>,
    /// The deflate level (with the shuffle filter before it), when compressed.
    pub deflate: Option<u32>,
}

/// A dataset of the root group.
#[derive(Debug, Clone)]
pub(crate) struct Dataset {
    pub name: String,
    pub dtype: H5Type,
    /// `None` for a scalar.
    pub shape: Option<Vec<u64>>,
    /// Optional maximum dimensions; u64::MAX marks an unlimited axis.
    pub max_shape: Option<Vec<u64>>,
    /// Little-endian values, row-major; empty for a dataset never written (it reads as its fill
    /// value).
    pub data: Vec<u8>,
    /// The fill value, little-endian.
    pub fill: Vec<u8>,
    pub storage: Storage,
    pub attrs: Vec<Attr>,
}

/// A file: the root group's attributes and the datasets, those of subgroups named by their path
/// (`group/sub/var`), the groups being created as needed.
#[derive(Debug, Clone, Default)]
pub(crate) struct File {
    pub attrs: Vec<Attr>,
    pub datasets: Vec<Dataset>,
    /// Attributes of subgroups, by path (`group/sub`).
    pub group_attrs: Vec<(String, Vec<Attr>)>,
}

/// The groups of a file below the root, by path, parents before children, in the order their
/// datasets or attributes first name them.
fn group_paths(f: &File) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut add = |path: &str| {
        let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
        for i in 1..=parts.len() {
            let p = parts[..i].join("/");
            if !out.contains(&p) {
                out.push(p);
            }
        }
    };
    for (path, _) in &f.group_attrs {
        add(path);
    }
    for d in &f.datasets {
        if let Some((parent, _)) = d.name.rsplit_once('/') {
            add(parent);
        }
    }
    out
}

/// The parent group path (empty for the root) and the leaf name of a path.
fn split_path(path: &str) -> (&str, &str) {
    path.rsplit_once('/').unwrap_or(("", path))
}

/// Addresses known once the file is laid out.
struct Addresses {
    /// Each subgroup's object header, in `group_paths` order.
    groups: Vec<u64>,
    /// Each dataset's object header.
    datasets: Vec<u64>,
    /// The global heap collection.
    heap: u64,
    /// Each dataset's data (contiguous) or B-tree root (chunked).
    data: Vec<u64>,
}

/// An object header (version 2) under construction: its messages.
struct Header {
    messages: Vec<(u8, Vec<u8>)>,
}

const MSG_DATASPACE: u8 = 0x01;
const MSG_LINK_INFO: u8 = 0x02;
const MSG_DATATYPE: u8 = 0x03;
const MSG_FILL_VALUE: u8 = 0x05;
const MSG_LINK: u8 = 0x06;
const MSG_LAYOUT: u8 = 0x08;
const MSG_GROUP_INFO: u8 = 0x0A;
const MSG_FILTERS: u8 = 0x0B;
const MSG_ATTRIBUTE: u8 = 0x0C;
const MSG_ATTRIBUTE_INFO: u8 = 0x15;

impl Header {
    /// Encodes the header with attribute (and link) creation order tracked.
    fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        let mut attr_index = 0u16;
        for (kind, data) in &self.messages {
            body.push(*kind);
            body.extend_from_slice(&(data.len() as u16).to_le_bytes());
            body.push(0);
            let order = if *kind == MSG_ATTRIBUTE {
                attr_index += 1;
                attr_index - 1
            } else {
                0
            };
            body.extend_from_slice(&order.to_le_bytes());
            body.extend_from_slice(data);
        }
        let mut out = b"OHDR".to_vec();
        out.push(2);
        // Chunk 0's size in 4 bytes, attribute creation order tracked, phase change values stored.
        out.push(0x02 | 0x04 | 0x10);
        let n_attrs = attr_index.max(8);
        out.extend_from_slice(&n_attrs.to_le_bytes());
        out.extend_from_slice(&n_attrs.min(6).to_le_bytes());
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        let sum = hdf5_reader::checksum::jenkins_lookup3(&out);
        out.extend_from_slice(&sum.to_le_bytes());
        out
    }
}

fn dataspace(shape: Option<&[u64]>) -> Vec<u8> {
    dataspace_with_max(shape, None)
}

fn dataspace_with_max(shape: Option<&[u64]>, maximum: Option<&[u64]>) -> Vec<u8> {
    match shape {
        None => vec![2, 0, 0, 0],
        Some(dims) => {
            let mut v = vec![2, dims.len() as u8, u8::from(maximum.is_some()), 1];
            for d in dims {
                v.extend_from_slice(&d.to_le_bytes());
            }
            if let Some(maximum) = maximum {
                for d in maximum {
                    v.extend_from_slice(&d.to_le_bytes());
                }
            }
            v
        }
    }
}

fn datatype(t: &H5Type) -> Vec<u8> {
    let mut v = Vec::new();
    t.encode(&mut v);
    v
}

/// The Attribute Info message: creation order tracked, attributes stored in the header.
fn attribute_info(n: usize) -> Vec<u8> {
    let mut v = vec![0, 0x01];
    v.extend_from_slice(&(n as u16).to_le_bytes());
    v.extend_from_slice(&UNDEF.to_le_bytes());
    v.extend_from_slice(&UNDEF.to_le_bytes());
    v
}

/// An attribute message (version 3), its references resolved with `addr`.
fn attribute(a: &Attr, addr: &Addresses, heap_ids: &[(usize, u32)]) -> Vec<u8> {
    let dt = datatype(&a.dtype);
    let ds = dataspace(a.shape.as_deref());
    let mut v = vec![3, 0];
    v.extend_from_slice(&(a.name.len() as u16 + 1).to_le_bytes());
    v.extend_from_slice(&(dt.len() as u16).to_le_bytes());
    v.extend_from_slice(&(ds.len() as u16).to_le_bytes());
    v.push(0);
    v.extend_from_slice(a.name.as_bytes());
    v.push(0);
    v.extend_from_slice(&dt);
    v.extend_from_slice(&ds);
    match &a.values {
        Values::Bytes(b) => {
            let mut b = b.clone();
            // Strings are padded with NULs to their length (at least one byte).
            b.resize(b.len().max(a.dtype.size() as usize * a.shape.as_ref().map_or(1, |s| s.iter().product::<u64>() as usize)), 0);
            v.extend_from_slice(&b);
        }
        Values::DimensionList(datasets) => {
            // One global heap object per element, each holding one reference.
            for (i, _) in datasets.iter().enumerate() {
                let id = heap_ids.get(i).map_or(0, |h| h.1);
                v.extend_from_slice(&1u32.to_le_bytes());
                v.extend_from_slice(&addr.heap.to_le_bytes());
                v.extend_from_slice(&id.to_le_bytes());
            }
        }
        Values::ReferenceList(refs) => {
            for (ds, dim) in refs {
                v.extend_from_slice(&addr.datasets.get(*ds).copied().unwrap_or(UNDEF).to_le_bytes());
                v.extend_from_slice(&dim.to_le_bytes());
            }
        }
    }
    v
}

/// Shuffles (by byte position within elements) then deflates a chunk, as HDF5's filters do.
fn compress(chunk: &[u8], element: usize, level: u32) -> Vec<u8> {
    let n = chunk.len() / element.max(1);
    let mut shuffled = vec![0u8; chunk.len()];
    if element > 1 {
        for i in 0..n {
            for k in 0..element {
                shuffled[k * n + i] = chunk[i * element + k];
            }
        }
        shuffled[n * element..].copy_from_slice(&chunk[n * element..]);
    } else {
        shuffled.copy_from_slice(chunk);
    }
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(level.min(9)));
    let _ = z.write_all(&shuffled);
    z.finish().unwrap_or_default()
}

/// A stored chunk: its offsets (in elements) and its bytes.
type Chunk = (Vec<u64>, Vec<u8>);

/// A dataset's chunks, in row-major chunk order.
fn chunks(d: &Dataset, chunk: &[u64]) -> Vec<Chunk> {
    let shape = d.shape.clone().unwrap_or_default();
    let esize = d.dtype.size() as usize;
    let counts: Vec<u64> = shape.iter().zip(chunk).map(|(s, c)| s.div_ceil(*c).max(1)).collect();
    let total: u64 = counts.iter().product();
    let chunk_len: usize = chunk.iter().product::<u64>() as usize;
    let rank = shape.len();
    // Row-major strides of the dataset and of a chunk, in elements.
    let strides: Vec<usize> = (0..rank).map(|i| shape[i + 1..].iter().product::<u64>() as usize).collect();
    let mut out = Vec::with_capacity(total as usize);
    for index in 0..total {
        let mut rest = index;
        let mut origin = vec![0u64; rank];
        for i in (0..rank).rev() {
            origin[i] = (rest % counts[i]) * chunk[i];
            rest /= counts[i];
        }
        // Fill the chunk, padding beyond the dataset's edges with the fill value.
        let fill: Vec<u8> = (0..esize).map(|i| d.fill.get(i).copied().unwrap_or(0)).collect();
        let mut buf = fill.repeat(chunk_len);
        let inner = chunk[rank - 1] as usize;
        let rows = chunk_len / inner.max(1);
        for r in 0..rows {
            // The chunk row's position in the dataset.
            let mut rr = r;
            let mut pos = vec![0u64; rank];
            pos[rank - 1] = origin[rank - 1];
            let mut inside = true;
            for i in (0..rank - 1).rev() {
                pos[i] = origin[i] + (rr as u64 % chunk[i]);
                rr /= chunk[i] as usize;
                inside &= pos[i] < shape[i];
            }
            if !inside {
                continue;
            }
            let n = (shape[rank - 1] - origin[rank - 1]).min(inner as u64) as usize;
            let start: usize = pos.iter().zip(&strides).map(|(p, s)| *p as usize * s).sum();
            buf[r * inner * esize..(r * inner + n) * esize].copy_from_slice(&d.data[start * esize..(start + n) * esize]);
        }
        let stored = match d.storage.deflate {
            Some(level) => compress(&buf, esize, level),
            None => buf,
        };
        out.push((origin, stored));
    }
    out
}

/// The size of a version 1 B-tree node of chunk keys for a dataset of `rank` dimensions.
fn btree_node_size(rank: usize) -> usize {
    let key = 4 + 4 + 8 * (rank + 1);
    24 + (2 * CHUNK_BTREE_K + 1) * key + 2 * CHUNK_BTREE_K * 8
}

/// Lays out the version 1 B-tree indexing chunks `(offsets, stored size, address)` of `chunk`
/// dimensions, its nodes starting at `at`. Returns the nodes `(address, bytes)` and the root's
/// address.
fn btree(chunks: &[(Vec<u64>, u32, u64)], chunk: &[u64], at: u64) -> (Vec<(u64, Vec<u8>)>, u64) {
    let rank = chunk.len();
    let node_size = btree_node_size(rank) as u64;
    // An entry: its key (offsets and stored size), its child, and where its last chunk ends (the
    // right key of a node ending with it).
    struct Entry {
        offsets: Vec<u64>,
        size: u32,
        child: u64,
        end: Vec<u64>,
    }
    let mut entries: Vec<Entry> = chunks
        .iter()
        .map(|(o, size, addr)| Entry {
            offsets: o.clone(),
            size: *size,
            child: *addr,
            end: o.iter().zip(chunk).map(|(a, b)| a + b).collect(),
        })
        .collect();
    let key = |b: &mut Vec<u8>, size: u32, offsets: &[u64]| {
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        for o in offsets {
            b.extend_from_slice(&o.to_le_bytes());
        }
        // The element dimension's offset.
        b.extend_from_slice(&0u64.to_le_bytes());
    };
    let mut nodes = Vec::new();
    let mut next = at;
    let mut level = 0u8;
    loop {
        let groups: Vec<&[Entry]> = entries.chunks(2 * CHUNK_BTREE_K).collect();
        let n_nodes = groups.len() as u64;
        let mut parents = Vec::new();
        for (i, g) in groups.iter().enumerate() {
            let addr = next + i as u64 * node_size;
            let mut b = b"TREE".to_vec();
            b.extend_from_slice(&[1, level]);
            b.extend_from_slice(&(g.len() as u16).to_le_bytes());
            let left = if i == 0 { UNDEF } else { addr - node_size };
            let right = if i as u64 + 1 == n_nodes { UNDEF } else { addr + node_size };
            b.extend_from_slice(&left.to_le_bytes());
            b.extend_from_slice(&right.to_le_bytes());
            for e in g.iter() {
                key(&mut b, e.size, &e.offsets);
                b.extend_from_slice(&e.child.to_le_bytes());
            }
            let last = &g[g.len() - 1];
            key(&mut b, 0, &last.end);
            b.resize(node_size as usize, 0);
            nodes.push((addr, b));
            parents.push(Entry { offsets: g[0].offsets.clone(), size: g[0].size, child: addr, end: last.end.clone() });
        }
        next += n_nodes * node_size;
        if parents.len() == 1 {
            return (nodes, parents[0].child);
        }
        entries = parents;
        level += 1;
    }
}

/// Encodes a file.
pub(crate) fn encode(f: &File) -> Vec<u8> {
    // Global heap objects: one per DIMENSION_LIST element, holding a reference.
    let mut heap_objects: Vec<(usize, usize, usize)> = Vec::new(); // (dataset, attribute, element)
    for (di, d) in f.datasets.iter().enumerate() {
        for (ai, a) in d.attrs.iter().enumerate() {
            if let Values::DimensionList(list) = &a.values {
                for e in 0..list.len() {
                    heap_objects.push((di, ai, e));
                }
            }
        }
    }
    let heap_id = |di: usize, ai: usize| -> Vec<(usize, u32)> {
        heap_objects.iter().enumerate().filter(|(_, o)| o.0 == di && o.1 == ai).map(|(i, o)| (o.2, i as u32 + 1)).collect()
    };

    // The data of each dataset: contiguous bytes, or its stored chunks.
    let stored: Vec<Option<Vec<Chunk>>> =
        f.datasets.iter().map(|d| d.storage.chunks.as_ref().filter(|_| d.shape.is_some()).map(|c| chunks(d, c))).collect();

    // Headers depend on addresses only through fixed-size fields: lay them out with placeholder
    // addresses, then encode them again with the real ones.
    let groups = group_paths(f);
    // A group's header: hard links (with their creation order) to its datasets and subgroups,
    // in the order they appear, then its attributes.
    let group_header = |path: &str, attrs: &[Attr], addr: &Addresses| -> Vec<u8> {
        let mut links: Vec<(&str, u64)> = Vec::new();
        for (gi, g) in groups.iter().enumerate() {
            let (parent, name) = split_path(g);
            if parent == path {
                links.push((name, addr.groups[gi]));
            }
        }
        for (i, d) in f.datasets.iter().enumerate() {
            let (parent, name) = split_path(&d.name);
            if parent == path {
                links.push((name, addr.datasets[i]));
            }
        }
        let mut h = Header { messages: Vec::new() };
        let mut li = vec![0, 0x01];
        li.extend_from_slice(&(links.len() as u64).to_le_bytes());
        li.extend_from_slice(&UNDEF.to_le_bytes());
        li.extend_from_slice(&UNDEF.to_le_bytes());
        h.messages.push((MSG_LINK_INFO, li));
        h.messages.push((MSG_GROUP_INFO, vec![0, 0]));
        for (order, (name, target)) in links.iter().enumerate() {
            let mut l = vec![1, 0x04];
            l.extend_from_slice(&(order as u64).to_le_bytes());
            l.push(name.len() as u8);
            l.extend_from_slice(name.as_bytes());
            l.extend_from_slice(&target.to_le_bytes());
            h.messages.push((MSG_LINK, l));
        }
        h.messages.push((MSG_ATTRIBUTE_INFO, attribute_info(attrs.len())));
        for a in attrs {
            h.messages.push((MSG_ATTRIBUTE, attribute(a, addr, &[])));
        }
        h.encode()
    };
    let group_attrs = |path: &str| f.group_attrs.iter().find(|(p, _)| p.trim_matches('/') == path).map_or(&[][..], |(_, a)| a.as_slice());
    let build = |addr: &Addresses| -> (Vec<u8>, Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let root = group_header("", &f.attrs, addr);
        let group_headers: Vec<Vec<u8>> = groups.iter().map(|g| group_header(g, group_attrs(g), addr)).collect();
        let mut headers = Vec::new();
        for (i, d) in f.datasets.iter().enumerate() {
            let mut h = Header { messages: Vec::new() };
            h.messages.push((MSG_DATASPACE, dataspace_with_max(d.shape.as_deref(), d.max_shape.as_deref())));
            h.messages.push((MSG_DATATYPE, datatype(&d.dtype)));
            let chunked = stored[i].is_some();
            // Fill value: allocated late (contiguous) or incrementally (chunked), written if set.
            let mut fv = vec![3, (if chunked { 3 } else { 2 }) | (2 << 2) | 0x20];
            fv.extend_from_slice(&(d.fill.len() as u32).to_le_bytes());
            fv.extend_from_slice(&d.fill);
            h.messages.push((MSG_FILL_VALUE, fv));
            let mut layout = vec![3];
            match (&d.storage.chunks, chunked) {
                (Some(c), true) => {
                    layout.push(2);
                    layout.push(c.len() as u8 + 1);
                    layout.extend_from_slice(&addr.data[i].to_le_bytes());
                    for x in c {
                        layout.extend_from_slice(&(*x as u32).to_le_bytes());
                    }
                    layout.extend_from_slice(&d.dtype.size().to_le_bytes());
                }
                _ => {
                    layout.push(1);
                    let n: u64 = d.shape.as_ref().map_or(1, |s| s.iter().product());
                    let address = if d.data.is_empty() { UNDEF } else { addr.data[i] };
                    layout.extend_from_slice(&address.to_le_bytes());
                    layout.extend_from_slice(&(n * d.dtype.size() as u64).to_le_bytes());
                }
            }
            h.messages.push((MSG_LAYOUT, layout));
            if chunked && let Some(level) = d.storage.deflate {
                // Shuffle (its parameter: the element size), then deflate; both optional.
                let mut p = vec![2, 2];
                for (id, cd) in [(2u16, d.dtype.size()), (1, level)] {
                    p.extend_from_slice(&id.to_le_bytes());
                    p.extend_from_slice(&1u16.to_le_bytes());
                    p.extend_from_slice(&1u16.to_le_bytes());
                    p.extend_from_slice(&cd.to_le_bytes());
                }
                h.messages.push((MSG_FILTERS, p));
            }
            h.messages.push((MSG_ATTRIBUTE_INFO, attribute_info(d.attrs.len())));
            for (ai, a) in d.attrs.iter().enumerate() {
                h.messages.push((MSG_ATTRIBUTE, attribute(a, addr, &heap_id(i, ai))));
            }
            headers.push(h.encode());
        }
        (root, group_headers, headers)
    };

    let n = f.datasets.len();
    let placeholder = Addresses { groups: vec![0; groups.len()], datasets: vec![0; n], heap: 0, data: vec![0; n] };
    let (root, group_headers, headers) = build(&placeholder);
    let mut at = 48 + root.len() as u64;
    let mut group_addrs = Vec::with_capacity(groups.len());
    for h in &group_headers {
        group_addrs.push(at);
        at += h.len() as u64;
    }
    let mut dataset_addrs = Vec::with_capacity(n);
    for h in &headers {
        dataset_addrs.push(at);
        at += h.len() as u64;
    }
    let heap_addr = if heap_objects.is_empty() { UNDEF } else { at };
    if !heap_objects.is_empty() {
        at += (16 + heap_objects.len() * 24).max(GLOBAL_HEAP_SIZE) as u64;
    }
    // Raw data: contiguous values, or chunks followed by their B-tree.
    let mut data_addrs = vec![UNDEF; n];
    let mut raw: Vec<(u64, Vec<u8>)> = Vec::new();
    for (i, d) in f.datasets.iter().enumerate() {
        match &stored[i] {
            Some(chunks) => {
                let mut index = Vec::with_capacity(chunks.len());
                for (offsets, bytes) in chunks {
                    index.push((offsets.clone(), bytes.len() as u32, at));
                    raw.push((at, bytes.clone()));
                    at += bytes.len() as u64;
                }
                let chunk = d.storage.chunks.clone().unwrap_or_default();
                let (nodes, root_addr) = btree(&index, &chunk, at);
                for (addr, b) in nodes {
                    at = at.max(addr + b.len() as u64);
                    raw.push((addr, b));
                }
                data_addrs[i] = root_addr;
            }
            None if !d.data.is_empty() => {
                data_addrs[i] = at;
                raw.push((at, d.data.clone()));
                at += d.data.len() as u64;
            }
            None => {}
        }
    }
    let addr = Addresses { groups: group_addrs, datasets: dataset_addrs, heap: heap_addr, data: data_addrs };
    let (root, group_headers, headers) = build(&addr);
    let eof = at;

    let mut out = Vec::with_capacity(eof as usize);
    out.extend_from_slice(b"\x89HDF\r\n\x1a\n");
    out.extend_from_slice(&[2, 8, 8, 0]);
    out.extend_from_slice(&0u64.to_le_bytes());
    out.extend_from_slice(&UNDEF.to_le_bytes());
    out.extend_from_slice(&eof.to_le_bytes());
    out.extend_from_slice(&48u64.to_le_bytes());
    let sum = hdf5_reader::checksum::jenkins_lookup3(&out);
    out.extend_from_slice(&sum.to_le_bytes());
    out.extend_from_slice(&root);
    for h in group_headers.iter().chain(&headers) {
        out.extend_from_slice(h);
    }
    if !heap_objects.is_empty() {
        let size = (16 + heap_objects.len() * 24).max(GLOBAL_HEAP_SIZE);
        let start = out.len();
        out.extend_from_slice(b"GCOL");
        out.extend_from_slice(&[1, 0, 0, 0]);
        out.extend_from_slice(&(size as u64).to_le_bytes());
        for (i, (di, ai, e)) in heap_objects.iter().enumerate() {
            let target = match &f.datasets[*di].attrs[*ai].values {
                Values::DimensionList(list) => list[*e],
                _ => 0,
            };
            out.extend_from_slice(&(i as u16 + 1).to_le_bytes());
            out.extend_from_slice(&1u16.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&8u64.to_le_bytes());
            out.extend_from_slice(&addr.datasets[target].to_le_bytes());
        }
        // The free space: an object of index 0 spanning the rest of the collection.
        let free = size - (out.len() - start);
        if free >= 16 {
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&[0; 4]);
            out.extend_from_slice(&(free as u64).to_le_bytes());
        }
        out.resize(start + size, 0);
    }
    raw.sort_by_key(|(a, _)| *a);
    for (a, b) in raw {
        out.resize(a as usize, 0);
        out.extend_from_slice(&b);
    }
    out.resize(eof as usize, 0);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(storage: Storage) -> File {
        let data: Vec<u8> = (0..(130 * 7)).flat_map(|i| (i as i16 - 300).to_le_bytes()).collect();
        let x: Vec<u8> = (0..7).flat_map(|i| (i as f64).to_le_bytes()).collect();
        File {
            attrs: vec![Attr::text("Conventions", "CF-1.5")],
            group_attrs: Vec::new(),
            datasets: vec![
                Dataset {
                    name: "x".into(),
                    dtype: H5Type::Float { size: 8 },
                    shape: Some(vec![7]),
                    max_shape: None,
                    data: x,
                    fill: 9.969209968386869e36f64.to_le_bytes().to_vec(),
                    storage: Storage::default(),
                    attrs: vec![Attr::text("CLASS", "DIMENSION_SCALE")],
                },
                Dataset {
                    name: "v".into(),
                    dtype: H5Type::Int { size: 2, signed: true },
                    shape: Some(vec![130, 7]),
                    max_shape: None,
                    data,
                    fill: (-1i16).to_le_bytes().to_vec(),
                    storage,
                    attrs: vec![Attr::numbers("_FillValue", H5Type::Int { size: 2, signed: true }, 1, (-1i16).to_le_bytes().to_vec())],
                },
            ],
        }
    }

    #[test]
    fn reads_back_with_hdf5_reader() {
        for storage in [
            Storage::default(),
            Storage { chunks: Some(vec![1, 7]), deflate: Some(1) },
            Storage { chunks: Some(vec![3, 4]), deflate: None },
            Storage { chunks: Some(vec![50, 5]), deflate: Some(6) },
        ] {
            let bytes = encode(&sample(storage.clone()));
            let f = hdf5_reader::Hdf5File::from_vec(bytes).unwrap();
            let v = f.dataset("/v").unwrap();
            assert_eq!(v.shape(), &[130, 7]);
            let values: Vec<i16> = v.read_array::<i16>().unwrap().iter().copied().collect();
            let expected: Vec<i16> = (0..(130 * 7)).map(|i| i as i16 - 300).collect();
            assert_eq!(values, expected, "{storage:?}");
            let x: Vec<f64> = f.dataset("/x").unwrap().read_array::<f64>().unwrap().iter().copied().collect();
            assert_eq!(x, (0..7).map(f64::from).collect::<Vec<_>>());
            let root = f.root_group().unwrap();
            assert_eq!(root.attribute("Conventions").unwrap().read_string().unwrap(), "CF-1.5");
        }
    }
}

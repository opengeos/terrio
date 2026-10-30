//! HDF5 rasters, read with the pure-Rust `hdf5-reader` crate as GDAL's HDF5 driver reads them.
//!
//! A file is a container of subdatasets: every two- or three-dimensional dataset of a numeric
//! type, named `HDF5:"file.h5"://group/dataset` (paths with GDAL's leading double slash, spaces
//! turned into underscores) and listed in the order GDAL lists them (groups walked depth first,
//! children by name). A file with one such dataset opens it directly; one with several opens the
//! first and lists them all as `SUBDATASET_N_NAME`/`SUBDATASET_N_DESC` metadata, with the
//! attributes of its groups and datasets as `group_subgroup_attribute` items, as `gdalinfo` shows
//! them.
//!
//! A subdataset's first dimension is its bands when it has three; `_FillValue` is its nodata
//! value; its attributes are reported as metadata. Georeferencing follows GDAL's generic case: the
//! CF `grid_mapping` variable of a two-dimensional dataset (with NISAR's `epsg_code`, and NISAR's
//! `xCoordinates`/`yCoordinates` for the geotransform). Product-specific georeferencing (HDF-EOS
//! grids and swaths, COSMO-SkyMed, ODIM, GCPs from latitude and longitude arrays) is not read.

use super::common::{self, Interleave, Layout};
use super::hdfeos::{Eos, FieldKind};
use crate::formats::RasterDriver;
use crate::{Error, RasterDataset, Result};
use hdf5_reader::messages::datatype::{Datatype, StringSize, VarLenKind};
use hdf5_reader::{Attribute, ByteOrder, Hdf5File};
use std::path::Path;
use wbraster::DataType;
use wbraster::raster::RasterData;

/// GDAL truncates long numeric metadata values (`MAX_METADATA_LEN`).
const MAX_METADATA_LEN: usize = 32768;

fn err(path: &Path, e: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("{}: {e}", path.display()))
}

/// Parses GDAL's subdataset syntax, `HDF5:"file.h5"://path/dataset` (or with the file unquoted):
/// `(file, dataset path)`.
pub(crate) fn parse_subdataset(s: &str) -> Option<(String, String)> {
    let rest = s.get(..5).filter(|p| p.eq_ignore_ascii_case("HDF5:")).map(|_| &s[5..])?;
    if let Some(quoted) = rest.strip_prefix('"') {
        let (file, var) = quoted.split_once("\":")?;
        return Some((file.to_string(), var.to_string()));
    }
    // An unquoted name: the dataset path starts at the first `:/` (a Windows drive letter, `C:\`,
    // is followed by a backslash).
    let at = rest.find(":/")?;
    Some((rest[..at].to_string(), rest[at + 1..].to_string()))
}

/// Whether a file is HDF5 and, as GDAL decides between its drivers, not one for another: netCDF-4
/// files carry netCDF's extensions, and KEA and BAG files (HDF5 too) have drivers of their own.
pub(crate) fn is_hdf5(path: &Path) -> bool {
    let other_driver = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| ["nc", "cdf", "nc2", "nc3", "nc4", "grd", "gmac", "kea", "bag"].iter().any(|n| e.eq_ignore_ascii_case(n)));
    !other_driver && has_signature(path)
}

/// Whether a file starts with the HDF5 signature (at offset 0, or 512, 1024, 2048... after a user
/// block).
pub(crate) fn has_signature(path: &Path) -> bool {
    signature_offset(path).is_some()
}

/// Where the HDF5 signature is: 0, or 512, 1024, 2048... after a user block.
fn signature_offset(path: &Path) -> Option<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let mut offset = 0u64;
    for _ in 0..20 {
        let mut b = [0u8; 8];
        if f.seek(SeekFrom::Start(offset)).is_err() || f.read_exact(&mut b).is_err() {
            return None;
        }
        if b == *b"\x89HDF\r\n\x1a\n" {
            return Some(offset);
        }
        offset = if offset == 0 { 512 } else { offset * 2 };
    }
    None
}

/// Opens an HDF5 file. Addresses in a file with a user block (JPSS products have one) are relative
/// to the superblock, which `hdf5-reader` does not account for: such files are read from there.
pub(crate) fn open_file(path: &Path) -> Result<Hdf5File> {
    match signature_offset(path) {
        Some(offset) if offset > 0 => {
            let mut bytes = std::fs::read(path)?;
            bytes.drain(..offset as usize);
            Hdf5File::from_vec(bytes).map_err(|e| err(path, e))
        }
        _ => Hdf5File::open(path).map_err(|e| err(path, e)),
    }
}

/// A numeric HDF5 type, as GDAL maps the native type of a dataset.
#[derive(Debug, Clone, Copy, PartialEq)]
struct NumType {
    data_type: DataType,
    /// A 16-bit float, read as Float32.
    half: bool,
    little_endian: bool,
    /// For a complex type (a compound of a real and an imaginary member of the same type, as GDAL
    /// reads it): the byte offsets of the imaginary part, and the size of an element.
    complex: Option<(usize, usize)>,
}

impl NumType {
    /// The size in bytes of one element (or of one part of a complex element) in the file.
    fn part_size(self) -> usize {
        if self.half { 2 } else { self.data_type.size_bytes() }
    }

    /// The size in bytes of one element in the file.
    fn element_size(self) -> usize {
        self.complex.map_or(self.part_size(), |(_, size)| size)
    }
}

/// The type of a dataset's pixels: a numeric type, or a complex type as GDAL's `GetDataType()`
/// recognizes compounds (two members of the same type, named `r...` and `i...`: `CInt16`,
/// `CInt32`, `CFloat32` from 16- and 32-bit floats, `CFloat64`).
fn pixel_type(dt: &Datatype) -> Option<NumType> {
    let Datatype::Compound { size, fields } = dt else { return num_type(dt) };
    let [re, im] = fields.as_slice() else { return None };
    let first = |n: &str, c: char| n.chars().next().is_some_and(|f| f.eq_ignore_ascii_case(&c));
    if !first(&re.name, 'r') || !first(&im.name, 'i') || re.byte_offset != 0 {
        return None;
    }
    let t = num_type(&re.datatype)?;
    let part = t.part_size();
    // `hdf5-reader` misreads the members after the first of version 1 and 2 compound types (it
    // pads their names from the start of the message, not of the name): a second member at
    // offset 0 is such a misreading, of the second half of the element.
    let misread = im.byte_offset == 0 && *size as usize == 2 * part;
    if im.datatype != re.datatype && !misread {
        return None;
    }
    let im_offset = if misread { part } else { im.byte_offset as usize };
    let supported = t.half || matches!(t.data_type, DataType::I16 | DataType::I32 | DataType::F32 | DataType::F64);
    supported.then_some(NumType { complex: Some((im_offset, *size as usize)), ..t })
}

fn num_type(dt: &Datatype) -> Option<NumType> {
    let (data_type, half, order) = match dt {
        Datatype::FixedPoint { size, signed, byte_order } => {
            let t = match (size, signed) {
                (1, true) => DataType::I8,
                (1, false) => DataType::U8,
                (2, true) => DataType::I16,
                (2, false) => DataType::U16,
                (4, true) => DataType::I32,
                (4, false) => DataType::U32,
                (8, true) => DataType::I64,
                (8, false) => DataType::U64,
                _ => return None,
            };
            (t, false, *byte_order)
        }
        Datatype::FloatingPoint { size: 2, byte_order } => (DataType::F32, true, *byte_order),
        Datatype::FloatingPoint { size: 4, byte_order } => (DataType::F32, false, *byte_order),
        Datatype::FloatingPoint { size: 8, byte_order } => (DataType::F64, false, *byte_order),
        _ => return None,
    };
    Some(NumType { data_type, half, little_endian: order == ByteOrder::LittleEndian, complex: None })
}

/// GDAL's description of a type (`HDF5Dataset::GetDataTypeName()`, with C's `char` signed).
fn type_description(t: NumType) -> String {
    let base = base_type_description(t);
    if t.complex.is_some() { format!("complex, {base}") } else { base.to_string() }
}

fn base_type_description(t: NumType) -> &'static str {
    if t.half {
        return "16-bit floating-point";
    }
    match t.data_type {
        DataType::I8 => "8-bit character",
        DataType::U8 => "8-bit unsigned character",
        DataType::I16 => "16-bit integer",
        DataType::U16 => "16-bit unsigned integer",
        DataType::I32 => "32-bit integer",
        DataType::U32 => "32-bit unsigned integer",
        DataType::I64 => "64-bit integer",
        DataType::U64 => "64-bit unsigned integer",
        DataType::F32 => "32-bit floating-point",
        DataType::F64 => "64-bit floating-point",
    }
}

/// An attribute's value as GDAL's HDF5 driver writes it in metadata. `None` for variable-length
/// sequences, which GDAL skips; types it does not convert (compounds, enumerations, references)
/// are empty.
fn attr_value(a: &Attribute) -> Option<String> {
    match &a.datatype {
        Datatype::VarLen { kind: VarLenKind::String, .. } | Datatype::String { size: StringSize::Variable, .. } => {
            Some(a.read_strings().unwrap_or_default().join(" "))
        }
        Datatype::VarLen { .. } => None,
        Datatype::String { .. } => {
            // The whole storage read as one C string: up to the first NUL.
            let bytes = a.raw_data.split(|b| *b == 0).next().unwrap_or_default();
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
        dt => {
            let Some(t) = num_type(dt) else { return Some(String::new()) };
            let size = t.data_type.size_bytes() / if t.half { 2 } else { 1 };
            let mut out = String::new();
            for (i, chunk) in a.raw_data.chunks_exact(size).enumerate() {
                if out.len() > MAX_METADATA_LEN {
                    break;
                }
                if i > 0 {
                    out.push(' ');
                }
                out.push_str(&format_number(chunk, t));
            }
            Some(out)
        }
    }
}

/// One value, formatted as GDAL formats it (`%d`, `%u`, `%.8g` for floats, `%.15g` for doubles).
fn format_number(b: &[u8], t: NumType) -> String {
    macro_rules! get {
        ($t:ty) => {{
            let a = b.try_into().unwrap_or_default();
            if t.little_endian { <$t>::from_le_bytes(a) } else { <$t>::from_be_bytes(a) }
        }};
    }
    if t.half {
        return common::fmt_g(half_to_f32(get!(u16)) as f64, 8);
    }
    match t.data_type {
        DataType::I8 => get!(i8).to_string(),
        DataType::U8 => get!(u8).to_string(),
        DataType::I16 => get!(i16).to_string(),
        DataType::U16 => get!(u16).to_string(),
        DataType::I32 => get!(i32).to_string(),
        DataType::U32 => get!(u32).to_string(),
        DataType::I64 => get!(i64).to_string(),
        DataType::U64 => get!(u64).to_string(),
        DataType::F32 => common::fmt_g(get!(f32) as f64, 8),
        DataType::F64 => common::fmt_g(get!(f64), 15),
    }
}

/// A single-valued numeric attribute as a double (GDAL's `GH5_FetchAttribute()`).
fn attr_f64(attrs: &[Attribute], name: &str) -> Option<f64> {
    let a = attrs.iter().find(|a| a.name == name)?;
    if a.shape.iter().product::<u64>() != 1 {
        return None;
    }
    value_f64(&a.raw_data, num_type(&a.datatype)?)
}

fn value_f64(b: &[u8], t: NumType) -> Option<f64> {
    macro_rules! get {
        ($t:ty) => {{
            let a = b.get(..size_of::<$t>())?.try_into().ok()?;
            (if t.little_endian { <$t>::from_le_bytes(a) } else { <$t>::from_be_bytes(a) }) as f64
        }};
    }
    if t.half {
        return Some(half_to_f32(get!(u16) as u16) as f64);
    }
    Some(match t.data_type {
        DataType::I8 => get!(i8),
        DataType::U8 => get!(u8),
        DataType::I16 => get!(i16),
        DataType::U16 => get!(u16),
        DataType::I32 => get!(i32),
        DataType::U32 => get!(u32),
        DataType::I64 => get!(i64),
        DataType::U64 => get!(u64),
        DataType::F32 => get!(f32),
        DataType::F64 => get!(f64),
    })
}

/// IEEE 754 half precision to single precision.
fn half_to_f32(h: u16) -> f32 {
    let sign = ((h >> 15) as u32) << 31;
    let exp = ((h >> 10) & 0x1f) as u32;
    let frac = (h & 0x3ff) as u32;
    let bits = match (exp, frac) {
        (0, 0) => sign,
        (0, mut f) => {
            // Subnormal: normalize.
            let mut e = 127 - 15 + 1;
            while f & 0x400 == 0 {
                f <<= 1;
                e -= 1;
            }
            sign | (e << 23) | ((f & 0x3ff) << 13)
        }
        (0x1f, f) => sign | 0x7f80_0000 | (f << 13),
        (e, f) => sign | ((e + 127 - 15) << 23) | (f << 13),
    };
    f32::from_bits(bits)
}

/// A dataset found in the file.
struct Entry {
    /// The path as GDAL names it (`//group/dataset`, spaces turned into underscores).
    gdal_path: String,
    /// The path in the file (`/group/dataset`).
    path: String,
    shape: Vec<u64>,
    num_type: Option<NumType>,
}

/// What a walk of the file collects.
#[derive(Default)]
struct Walk {
    datasets: Vec<Entry>,
    /// The metadata of the container (`gdalinfo file.h5`): every group's attributes and those of
    /// the datasets of a supported type.
    container_metadata: Vec<(String, String)>,
    /// The metadata of a subdataset: every group's attributes.
    group_metadata: Vec<(String, String)>,
    /// The addresses of the groups walked.
    visited: std::collections::HashSet<u64>,
}

/// Sets a metadata item, replacing an earlier one with the same key in place (`CSLSetNameValue`).
fn set_item(md: &mut Vec<(String, String)>, key: String, value: String) {
    match md.iter_mut().find(|(k, _)| k.eq_ignore_ascii_case(&key)) {
        Some(item) => item.1 = value,
        None => md.push((key, value)),
    }
}

/// The metadata key prefix of an object: its GDAL path's components joined with underscores.
fn key_prefix(gdal_path: &str) -> String {
    gdal_path.split('/').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("_")
}

/// Adds an object's attributes as metadata items (`HDF5Dataset::CreateMetadata()`).
fn add_attributes(md: &mut Vec<(String, String)>, prefix: &str, attrs: &[Attribute]) {
    for a in attrs {
        let Some(value) = attr_value(a) else { continue };
        let mut key = prefix.to_string();
        for token in a.name.split(' ').filter(|s| !s.is_empty()) {
            if !key.is_empty() {
                key.push('_');
            }
            key.push_str(token);
        }
        set_item(md, key, value);
    }
}

/// GDAL's path of a child: `//a/b` (the root is `/`), with spaces turned into underscores.
fn gdal_child_path(parent: &str, name: &str) -> String {
    let path = format!("{}/{name}", if parent == "/" { "/" } else { parent });
    path.split(' ').filter(|s| !s.is_empty()).collect::<Vec<_>>().join("_")
}

fn walk_group(g: &hdf5_reader::group::Group, path: &str, gdal_path: &str, w: &mut Walk, depth: usize) -> Result<()> {
    // Hard links can make groups their own descendants: each group is walked once, as GDAL skips
    // the groups it has seen.
    if depth > 64 || !w.visited.insert(g.address()) {
        return Ok(());
    }
    let leaf = |n: &str| n.rsplit('/').next().unwrap_or(n).to_string();
    let (groups, datasets) = g.members().map_err(|e| Error::InvalidData(e.to_string()))?;
    // Children by name, as `H5Giterate()` visits them.
    enum Child {
        Group(hdf5_reader::group::Group),
        Dataset(Box<hdf5_reader::Dataset>),
    }
    let mut children: Vec<(String, Child)> = groups.into_iter().map(|c| (leaf(c.name()), Child::Group(c))).collect();
    children.extend(datasets.into_iter().map(|d| (leaf(d.name()), Child::Dataset(Box::new(d)))));
    children.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    for (name, child) in children {
        let child_path = if path == "/" { format!("/{name}") } else { format!("{path}/{name}") };
        let child_gdal = gdal_child_path(gdal_path, &name);
        match child {
            Child::Group(c) => walk_group(&c, &child_path, &child_gdal, w, depth + 1)?,
            Child::Dataset(d) => {
                let t = pixel_type(d.dtype());
                if t.is_some() {
                    add_attributes(&mut w.container_metadata, &key_prefix(&child_gdal), &d.attributes());
                }
                w.datasets.push(Entry { gdal_path: child_gdal, path: child_path, shape: d.shape().to_vec(), num_type: t });
            }
        }
    }
    let attrs = g.attributes().map_err(|e| Error::InvalidData(e.to_string()))?;
    let prefix = key_prefix(gdal_path);
    add_attributes(&mut w.container_metadata, &prefix, &attrs);
    add_attributes(&mut w.group_metadata, &prefix, &attrs);
    Ok(())
}

fn walk(f: &Hdf5File) -> Result<Walk> {
    let root = f.root_group().map_err(|e| Error::InvalidData(e.to_string()))?;
    let mut w = Walk::default();
    walk_group(&root, "/", "/", &mut w, 0)?;
    Ok(w)
}

/// The HDF-EOS5 structure of a file (`HDFEOS INFORMATION/StructMetadata.0`), when it has one
/// that describes grids or swaths.
fn eos_structure(f: &Hdf5File) -> Option<Eos> {
    let d = f.dataset("/HDFEOS INFORMATION/StructMetadata.0").ok()?;
    if !matches!(d.dtype(), Datatype::String { size: StringSize::Fixed(_), .. }) || !d.shape().is_empty() {
        return None;
    }
    let raw = d.read_raw_bytes().ok()?;
    let text = raw.split(|b| *b == 0).next().unwrap_or_default();
    Eos::parse(&String::from_utf8_lossy(text))
}

/// The subdatasets of a file, `(name, description)`, as GDAL lists them: the datasets of two or
/// three dimensions of a numeric (or complex) type, swath fields described by their dimensions'
/// names.
fn subdatasets(w: &Walk, file: &str, eos: Option<&Eos>) -> Vec<(String, String)> {
    w.datasets
        .iter()
        .filter_map(|e| {
            let t = e.num_type?;
            if !(2..=3).contains(&e.shape.len()) || e.shape.iter().any(|d| *d > i32::MAX as u64) {
                return None;
            }
            let dims = eos
                .and_then(|eos| eos.field(&e.gdal_path, e.shape.len()))
                .and_then(|f| f.swath_description())
                .unwrap_or_else(|| e.shape.iter().map(u64::to_string).collect::<Vec<_>>().join("x"));
            Some((format!("HDF5:\"{file}\":{}", e.gdal_path), format!("[{dims}] {} ({})", e.gdal_path, type_description(t))))
        })
        .collect()
}

/// Reads an HDF5 dataset as a raster: the one named by `subdataset` (a GDAL path such as
/// `//group/dataset`), or the file's only subdataset, or its first one (listing them all).
/// `name` is the file as given, used to name the subdatasets.
pub(crate) fn read(path: &Path, name: &str, subdataset: Option<&str>) -> Result<RasterDataset> {
    read_file(&open_file(path)?, path, name, subdataset)
}

/// Where the HDF5 signature is in bytes: 0, or 512, 1024, 2048... after a user block.
pub(crate) fn signature_offset_in(b: &[u8]) -> Option<usize> {
    let mut offset = 0usize;
    for _ in 0..20 {
        if b.get(offset..offset + 8)? == b"\x89HDF\r\n\x1a\n" {
            return Some(offset);
        }
        offset = if offset == 0 { 512 } else { offset * 2 };
    }
    None
}

/// Opens an HDF5 file held in memory (after its user block, when it has one).
pub(crate) fn open_bytes(mut bytes: Vec<u8>, name: &str) -> Result<Hdf5File> {
    let offset = signature_offset_in(&bytes).ok_or_else(|| err(Path::new(name), "not an HDF5 file"))?;
    bytes.drain(..offset);
    Hdf5File::from_vec(bytes).map_err(|e| err(Path::new(name), e))
}

/// The root group's `File Type` attribute of an HDF5 file held in memory, when it has one (HEC-RAS
/// writes it): see [`root_file_type`]. Reads only the file's metadata, from a temporary copy of the
/// bytes.
#[cfg(feature = "hecras")]
pub(crate) fn root_file_type_in(bytes: &[u8]) -> Option<String> {
    root_file_type(&Hdf5File::from_bytes(&bytes[signature_offset_in(bytes)?..]).ok()?)
}

/// The largest root `File Type` attribute [`root_file_type`] decodes.
#[cfg(feature = "hecras")]
const FILE_TYPE_MAX_BYTES: usize = 4096;

/// The root group's `File Type` attribute, as h5py decodes it, when it is a single fixed-length
/// string of at most 4 KiB: a scalar, as HEC-RAS writes it, or a one-element array, which the
/// HEC-RAS engine accepts too. No other root attribute is decoded, and a `File Type` of any other
/// type, element count or size is `None`, so detection never decodes more than a few bytes of
/// metadata whatever the file stores.
#[cfg(feature = "hecras")]
pub(crate) fn root_file_type(f: &Hdf5File) -> Option<String> {
    use hdf5_reader::messages::datatype::StringPadding;
    let attributes = f.root_group().ok()?.raw_attributes().ok()?;
    let m = attributes.iter().find(|a| a.name() == "File Type")?.message();
    let Datatype::String { size: StringSize::Fixed(size), padding, .. } = &m.datatype else { return None };
    if m.raw_data.len() > FILE_TYPE_MAX_BYTES || m.raw_data.len() != usize::try_from(*size).ok()? {
        return None;
    }
    let mut bytes = m.raw_data.split(|b| *b == 0).next().unwrap_or_default();
    if *padding == StringPadding::SpacePad {
        bytes = bytes.trim_ascii_end();
    }
    String::from_utf8(bytes.to_vec()).ok()
}

/// The stored bytes of the root group's attribute values (a variable-length value counts its heap
/// reference), from the attribute messages, without decoding any of them. `path` names the file
/// in errors.
#[cfg(feature = "hecras")]
pub(crate) fn root_attribute_bytes(f: &Hdf5File, path: &Path) -> Result<usize> {
    let attributes = f.root_group().and_then(|g| g.raw_attributes()).map_err(|e| err(path, e))?;
    Ok(attributes.iter().fold(0usize, |n, a| n.saturating_add(a.message().raw_data.len())))
}

/// [`root_attribute_bytes`] of an HDF5 file held in memory. `name` names it in errors.
#[cfg(feature = "hecras")]
pub(crate) fn root_attribute_bytes_in(bytes: &[u8], name: &str) -> Result<usize> {
    let path = Path::new(name);
    let offset = signature_offset_in(bytes).ok_or_else(|| err(path, "not an HDF5 file"))?;
    root_attribute_bytes(&Hdf5File::from_bytes(&bytes[offset..]).map_err(|e| err(path, e))?, path)
}

/// Reads an HDF5 dataset held in memory as a raster: see [`read`]. `name` is the file name.
pub(crate) fn read_bytes(bytes: Vec<u8>, name: &str, subdataset: Option<&str>) -> Result<RasterDataset> {
    read_file(&open_bytes(bytes, name)?, Path::new(name), name, subdataset)
}

fn read_file(f: &Hdf5File, path: &Path, name: &str, subdataset: Option<&str>) -> Result<RasterDataset> {
    let w = walk(f)?;
    let eos = eos_structure(f);
    let subs = subdatasets(&w, name, eos.as_ref());
    let (entry, metadata) = match subdataset {
        Some(s) => {
            let e = w
                .datasets
                .iter()
                .find(|e| e.gdal_path.eq_ignore_ascii_case(s) || e.path == s)
                .ok_or_else(|| err(path, format!("no dataset '{s}'")))?;
            (e, None)
        }
        None => {
            let first = subs.first().ok_or_else(|| err(path, "no dataset of two or three dimensions"))?;
            let gdal_path = first.0.rsplit_once("\":").map(|(_, p)| p).unwrap_or_default();
            let e = w.datasets.iter().find(|e| e.gdal_path == gdal_path).ok_or_else(|| err(path, "dataset not found"))?;
            if subs.len() == 1 {
                (e, None)
            } else {
                let mut md = w.container_metadata.clone();
                for (i, (n, d)) in subs.iter().enumerate() {
                    md.push((format!("SUBDATASET_{}_NAME", i + 1), n.clone()));
                    md.push((format!("SUBDATASET_{}_DESC", i + 1), d.clone()));
                }
                (e, Some(md))
            }
        }
    };
    read_dataset(f, path, name, &w, eos.as_ref(), entry, metadata)
}

/// Reorders the elements of an array into bands, rows and columns: `x`, `y` and `other` are the
/// dimensions of the columns, rows and bands (rows of 1 without `y`, one band without `other`).
fn to_bsq(raw: &[u8], shape: &[usize], element: usize, x: usize, y: Option<usize>, other: Option<usize>) -> Vec<u8> {
    let nd = shape.len();
    let mut strides = vec![1usize; nd];
    for d in (0..nd.saturating_sub(1)).rev() {
        strides[d] = strides[d + 1] * shape[d + 1];
    }
    let (cols, rows, bands) = (shape[x], y.map_or(1, |y| shape[y]), other.map_or(1, |o| shape[o]));
    let mut out = Vec::with_capacity(cols * rows * bands * element);
    for b in 0..bands {
        for r in 0..rows {
            let base = other.map_or(0, |o| b * strides[o]) + y.map_or(0, |y| r * strides[y]);
            if strides[x] == 1 {
                out.extend_from_slice(&raw[base * element..(base + cols) * element]);
            } else {
                for c in 0..cols {
                    let i = (base + c * strides[x]) * element;
                    out.extend_from_slice(&raw[i..i + element]);
                }
            }
        }
    }
    out
}

/// Decodes elements of a numeric type laid out band by band.
fn decode_parts(raw: &[u8], t: NumType, cols: usize, rows: usize, bands: usize) -> Result<RasterData> {
    if t.half {
        let v: Vec<f32> = raw
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| half_to_f32(if t.little_endian { u16::from_le_bytes(*c) } else { u16::from_be_bytes(*c) }))
            .collect();
        return Ok(RasterData::F32(v));
    }
    let layout = Layout {
        cols,
        rows,
        bands,
        data_type: t.data_type,
        little_endian: t.little_endian,
        interleave: Interleave::Bsq,
        offset: 0,
        bottom_up: false,
    };
    common::decode(raw, &layout)
}

fn read_dataset(
    f: &Hdf5File,
    path: &Path,
    name: &str,
    w: &Walk,
    eos: Option<&Eos>,
    e: &Entry,
    metadata: Option<Vec<(String, String)>>,
) -> Result<RasterDataset> {
    let t = e.num_type.ok_or_else(|| Error::Unsupported(format!("{}: {}: unsupported HDF5 data type", path.display(), e.path)))?;
    let ds = f.dataset(&e.path).map_err(|e| err(path, e))?;
    let shape: Vec<usize> = e.shape.iter().map(|d| *d as usize).collect();
    let nd = shape.len();
    if nd == 0 {
        return Err(err(path, format!("{} is a scalar", e.path)));
    }
    // GDAL's layout: the last two dimensions are the rows and columns, the first of three the
    // bands; HDF-EOS5 fields say which of their dimensions are which.
    let field = eos.and_then(|eos| eos.field(&e.gdal_path, nd));
    let (mut x, mut y, mut other) = (nd - 1, (nd >= 2).then(|| nd - 2), (nd == 3).then_some(0));
    let mut grid = None;
    let mut swath_geolocation = None;
    match field.map(|f| (f, &f.kind)) {
        Some((f, FieldKind::Grid(g))) => {
            if let Some(fx) = f.x {
                x = fx;
            }
            if f.y.is_some() {
                y = f.y;
            }
            if f.other.is_some() {
                other = f.other;
            }
            grid = Some(g);
        }
        Some((f, FieldKind::Swath(geolocation))) if f.x.is_some() && f.y.is_some() => {
            x = f.x.unwrap_or(x);
            y = f.y;
            other = f.other;
            swath_geolocation = geolocation.as_ref();
        }
        _ => {}
    }
    // COSMO-SkyMed single-look complex products (SCS): rows, columns, then the I and Q bands.
    let csk = csk_product(w).filter(|_| !e.gdal_path.contains("QLK") && !name.contains("QLK"));
    if field.is_none() && nd == 3 && csk.as_deref().is_some_and(|p| starts_with_ci(p, "SCS")) {
        (x, y, other) = (1, Some(0), Some(2));
    }
    let (cols, rows, bands) = (shape[x], y.map_or(1, |y| shape[y]), other.map_or(1, |o| shape[o]));
    let raw = ds.read_raw_bytes().map_err(|e| err(path, e))?;
    let element = t.element_size();
    let n = cols * rows * bands;
    // The whole dataset: a layout other than GDAL's is gathered with the strides of its shape.
    let whole = raw.get(..shape.iter().product::<usize>() * element).ok_or_else(|| err(path, "dataset shorter than its shape"))?;
    let standard = x == nd - 1 && y == nd.checked_sub(2) && (other.is_none() || other == Some(0));
    let ordered = if standard {
        std::borrow::Cow::Borrowed(&whole[..n * element])
    } else {
        std::borrow::Cow::Owned(to_bsq(whole, &shape, element, x, y, other))
    };
    let (data, raster_bands) = match t.complex {
        // Complex elements: each band becomes its real parts then its imaginary parts.
        Some((im, size)) => {
            let part = t.part_size();
            let plane = cols * rows;
            let mut parts = Vec::with_capacity(n * 2 * part);
            for b in 0..bands {
                for offset in [0, im] {
                    for i in 0..plane {
                        let at = (b * plane + i) * size + offset;
                        parts.extend_from_slice(&ordered[at..at + part]);
                    }
                }
            }
            (decode_parts(&parts, t, cols, rows, bands * 2)?, bands * 2)
        }
        None => (decode_parts(&ordered, t, cols, rows, bands)?, bands),
    };
    let attrs = ds.attributes();
    let nodata = if t.complex.is_some() { None } else { attr_f64(&attrs, "_FillValue") };
    let (mut gt, mut crs) = (None, None);
    if let Some(g) = grid {
        gt = g.geo_transform();
        crs = g.crs();
    }
    if grid.is_none() && swath_geolocation.is_none() {
        (gt, crs) = match &csk {
            Some(level) => csk_georeferencing(f, w, &attrs, level),
            None => georeferencing(f, w, e, &attrs, nd),
        };
        if csk.is_none()
            && gt.is_none()
            && crs.is_none()
            && rows >= 100
            && cols >= 30
            && !w.datasets.iter().any(|d| d.path.ends_with("/Latitude"))
        {
            (gt, crs) = odim_georeferencing(w, cols, rows);
        }
    }
    let mut raster =
        common::make_raster(cols, rows, raster_bands, data, gt.unwrap_or(common::NO_GEOTRANSFORM), crs.unwrap_or_default(), nodata)?;
    raster.metadata = match metadata {
        Some(md) => md,
        None => {
            // A subdataset: the groups' attributes, then its own (GDAL's band metadata).
            let mut md = w.group_metadata.clone();
            add_attributes(&mut md, "", &attrs);
            md
        }
    };
    let mut out = RasterDataset::with_driver(raster, RasterDriver::HDF5, nodata.is_some());
    out.complex = t.complex.is_some();
    out.geolocation = swath_geolocation.map(|g| crate::Geolocation {
        srs: crate::drivers::common::WGS84_LAT_LONG_WKT.to_string(),
        x_dataset: format!("HDF5:\"{name}\":{}", g.longitude),
        x_band: 1,
        y_dataset: format!("HDF5:\"{name}\":{}", g.latitude),
        y_band: 1,
        pixel_offset: g.pixel_offset as f64,
        pixel_step: g.pixel_step as f64,
        line_offset: g.line_offset as f64,
        line_step: g.line_step as f64,
        pixel_center: true,
    });
    Ok(out)
}

/// Whether `s` starts with `prefix`, ignoring case.
fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.get(..prefix.len()).is_some_and(|p| p.eq_ignore_ascii_case(prefix))
}

/// A metadata item of the file's groups (root attributes such as `Mission_ID`).
fn group_item<'a>(w: &'a Walk, key: &str) -> Option<&'a str> {
    w.group_metadata.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
}

/// The product type (`SCS_B`, `DGM_B`, `GEC_B`...) of a COSMO-SkyMed, KOMPSAT-5 or COSMO-SkyMed
/// Second Generation file, as GDAL identifies them by their `Mission ID`.
fn csk_product(w: &Walk) -> Option<String> {
    let mission = group_item(w, "Mission_ID")?;
    ["CSK", "KMPS", "CSG"]
        .iter()
        .any(|m| mission.eq_ignore_ascii_case(m))
        .then(|| group_item(w, "Product_Type").unwrap_or_default().to_string())
}

/// The values of a numeric attribute, as doubles.
fn attr_f64s(attrs: &[Attribute], name: &str) -> Option<Vec<f64>> {
    let a = attrs.iter().find(|a| a.name == name)?;
    let t = num_type(&a.datatype)?;
    a.raw_data.chunks_exact(t.part_size()).map(|c| value_f64(c, t)).collect()
}

/// COSMO-SkyMed georeferencing, as GDAL's `CreateProjections()` reads it: for geocoded products
/// (GEC, GTC) the geotransform from the image's `Top Left East-North`, `Line Spacing` and
/// `Column Spacing`, and the UTM or UPS projection of the root's `Map Projection` attributes;
/// WGS 84 for the others (whose corners GDAL gives as GCPs, which terrio does not model).
fn csk_georeferencing(f: &Hdf5File, w: &Walk, attrs: &[Attribute], level: &str) -> (Option<[f64; 6]>, Option<wbraster::CrsInfo>) {
    let wgs84 = wbraster::CrsInfo::from_epsg(4326);
    if !(starts_with_ci(level, "GEC") || starts_with_ci(level, "GTC")) {
        return (None, Some(wgs84));
    }
    let first = |name: &str| attr_f64s(attrs, name).filter(|v| !v.is_empty());
    let gt = match (first("Top Left East-North"), first("Line Spacing"), first("Column Spacing")) {
        (Some(ul), Some(dx), Some(dy)) if ul.len() >= 2 => Some([ul[0], dx[0], 0.0, ul[1], 0.0, -dy[0]]),
        _ => None,
    };
    let root = f.root_group().ok().and_then(|g| g.attributes().ok()).unwrap_or_default();
    let get = |name: &str| attr_f64s(&root, name).filter(|v| !v.is_empty());
    let crs = match (
        get("Map Projection False East-North"),
        get("Map Projection Scale Factor"),
        get("Map Projection Centre"),
        group_item(w, "Projection_ID"),
    ) {
        (Some(fe), Some(k), Some(c), Some(id)) if fe.len() >= 2 && c.len() >= 2 => {
            let (x0, y0) = (fe[0], fe[1]);
            // A UTM zone on WGS 84 is identified as its EPSG code, as GDAL's `AutoIdentifyEPSG()`.
            let zone = ((c[1] + 183.0) / 6.0).round();
            let utm = c[0] == 0.0
                && k[0] == 0.9996
                && x0 == 500_000.0
                && (y0 == 0.0 || y0 == 10_000_000.0)
                && (1.0..=60.0).contains(&zone)
                && (zone * 6.0 - 183.0 - c[1]).abs() < 1e-9;
            if id.eq_ignore_ascii_case("UTM") && utm {
                return (gt, Some(wbraster::CrsInfo::from_epsg(if y0 == 0.0 { 32600 } else { 32700 } + zone as u32)));
            }
            let proj = if id.eq_ignore_ascii_case("UTM") {
                Some(format!("+proj=tmerc +lat_0={} +lon_0={} +k={} +x_0={x0} +y_0={y0} +datum=WGS84 +units=m +no_defs", c[0], c[1], k[0]))
            } else if id.eq_ignore_ascii_case("UPS") {
                let pole = if c[0] < 0.0 { -90.0 } else { 90.0 };
                Some(format!("+proj=stere +lat_0={pole} +lon_0={} +k={} +x_0={x0} +y_0={y0} +datum=WGS84 +units=m +no_defs", c[1], k[0]))
            } else {
                None
            };
            match proj {
                Some(p) => crate::crs::SpatialRef::from_proj4(&p).ok().map(|s| s.to_crs_info()),
                None => Some(wgs84),
            }
        }
        // Malformed: GDAL clears the CRS.
        _ => None,
    };
    (gt, crs)
}

/// The georeferencing of an ODIM HDF5 radar composite (`where` group): its PROJ definition, and
/// the geotransform of its lower left and upper right corners (longitudes and latitudes),
/// projected, as GDAL's `CreateODIMH5Projection()`.
fn odim_georeferencing(w: &Walk, cols: usize, rows: usize) -> (Option<[f64; 6]>, Option<wbraster::CrsInfo>) {
    let get = |k: &str| group_item(w, k);
    let num = |k: &str| get(k).and_then(|v| v.trim().parse::<f64>().ok());
    let (Some(projdef), Some(ll_lon), Some(ll_lat), Some(ur_lon), Some(ur_lat)) =
        (get("where_projdef"), num("where_LL_lon"), num("where_LL_lat"), num("where_UR_lon"), num("where_UR_lat"))
    else {
        return (None, None);
    };
    let Ok(srs) = crate::crs::SpatialRef::from_proj4(projdef.trim()) else { return (None, None) };
    let Ok(wgs84) = crate::crs::SpatialRef::from_epsg(4326) else { return (None, None) };
    let Ok(t) = crate::crs::Transformer::new(&wgs84, &srs) else { return (None, None) };
    let (Ok((llx, lly)), Ok((urx, ury))) = (t.transform(ll_lon, ll_lat), t.transform(ur_lon, ur_lat)) else {
        return (None, Some(srs.to_crs_info()));
    };
    let (dx, dy) = ((urx - llx) / cols as f64, (ury - lly) / rows as f64);
    (Some([llx, dx, 0.0, ury, 0.0, -dy]), Some(srs.to_crs_info()))
}

/// GDAL's generic georeferencing of a two-dimensional dataset (`CreateProjections()`): the CRS of
/// its CF grid mapping (NISAR's `epsg_code`), and for NISAR products the geotransform of the
/// `xCoordinates` and `yCoordinates` next to it.
fn georeferencing(f: &Hdf5File, w: &Walk, e: &Entry, attrs: &[Attribute], nd: usize) -> (Option<[f64; 6]>, Option<wbraster::CrsInfo>) {
    if nd != 2 {
        return (None, None);
    }
    let dir = e.path.rsplit_once('/').map(|(d, _)| d).unwrap_or_default();
    let string_attr = |attrs: &[Attribute], name: &str| attrs.iter().find(|a| a.name == name).and_then(attr_value);
    let mut crs = None;
    if let Some(gm) = string_attr(attrs, "grid_mapping")
        && let Ok(g) = f.dataset(&format!("{dir}/{gm}"))
    {
        let gattrs = g.attributes();
        let epsg = string_attr(&gattrs, "epsg_code").and_then(|s| s.trim().parse::<u32>().ok()).filter(|c| *c > 0);
        crs = match epsg {
            Some(code) => Some(wbraster::CrsInfo::from_epsg(code)),
            None => super::cf::cf_crs(
                |k| string_attr(&gattrs, k),
                |k| {
                    let a = gattrs.iter().find(|a| a.name == k)?;
                    let t = num_type(&a.datatype)?;
                    let size = if t.half { 2 } else { t.data_type.size_bytes() };
                    a.raw_data.chunks_exact(size).map(|c| value_f64(c, t)).collect()
                },
            ),
        };
    }
    let mut gt = None;
    let nisar = w.group_metadata.iter().any(|(k, v)| k.eq_ignore_ascii_case("mission_name") && v.eq_ignore_ascii_case("NISAR"));
    if nisar {
        let coords = |name: &str, n: u64| -> Option<Vec<f64>> {
            let d = f.dataset(&format!("{dir}/{name}")).ok()?;
            let ok = d.shape() == [n] && matches!(d.dtype(), Datatype::FloatingPoint { size: 8, .. });
            ok.then(|| d.read_array::<f64>().ok().map(|a| a.iter().copied().collect())).flatten()
        };
        let spacing = |v: &[f64]| -> Option<f64> {
            if v.len() < 3 {
                return None;
            }
            let s = v[1] - v[0];
            v.windows(2).skip(1).all(|p| ((p[1] - p[0]) - s).abs() <= 1e-3 * s.abs()).then_some(s)
        };
        if let (Some(x), Some(y)) = (coords("xCoordinates", e.shape[1]), coords("yCoordinates", e.shape[0]))
            && let (Some(dx), Some(dy)) = (spacing(&x), spacing(&y))
        {
            gt = Some([x[0] - dx / 2.0, dx, 0.0, y[0] - dy / 2.0, 0.0, dy]);
        }
    }
    (gt, crs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_subdataset_names() {
        assert_eq!(parse_subdataset("HDF5:\"/data/a b.h5\"://All_Data/x"), Some(("/data/a b.h5".into(), "//All_Data/x".into())));
        assert_eq!(parse_subdataset("hdf5:f.h5://g/d"), Some(("f.h5".into(), "//g/d".into())));
        assert_eq!(parse_subdataset("f.h5"), None);
    }

    #[test]
    fn converts_half_floats() {
        assert_eq!(half_to_f32(0x3c00), 1.0);
        assert_eq!(half_to_f32(0xc000), -2.0);
        assert_eq!(half_to_f32(0x0001), 2f32.powi(-24));
        assert!(half_to_f32(0x7e00).is_nan());
    }

    #[test]
    fn names_paths_as_gdal() {
        assert_eq!(gdal_child_path("/", "All Data"), "//All_Data");
        assert_eq!(gdal_child_path("//All_Data", "x"), "//All_Data/x");
        assert_eq!(key_prefix("//All_Data/ATMS"), "All_Data_ATMS");
        assert_eq!(key_prefix("/"), "");
    }

    /// An ODIM HDF5 radar composite (DWD's `composite_hx`, at a tenth of its resolution): the
    /// corners of `where` projected with its PROJ definition, as GDAL computes the geotransform
    /// (GDAL 3.12 reads `[-124.99999993981328, 250.00000000001472, 0, 125.00000000139698, 0,
    /// -249.99999999991724]` for the full 4400 x 4800 composite).
    #[test]
    fn reads_odim_georeferencing() {
        use super::super::hdf5_write::{Attr, Dataset, File, H5Type, Storage};
        let num = |name: &str, v: f64| Attr::numbers(name, H5Type::Float { size: 8 }, 1, v.to_le_bytes().to_vec());
        let projdef = "+proj=stere +lat_ts=60 +lat_0=90 +lon_0=10 +x_0=543571.83521776402 +y_0=3622213.8619310022 +units=m +a=6378137 +b=6356752.3142451802 +no_defs";
        let (cols, rows) = (440usize, 480usize);
        let f = File {
            attrs: vec![],
            group_attrs: vec![(
                "where".into(),
                vec![
                    Attr::text("projdef", projdef),
                    num("LL_lat", 45.6964253773901),
                    num("LL_lon", 3.56699463500789),
                    num("UR_lat", 55.8454385632558),
                    num("UR_lon", 18.7316164546675),
                ],
            )],
            datasets: vec![Dataset {
                name: "dataset1/data1/data".into(),
                dtype: H5Type::Int { size: 1, signed: false },
                shape: Some(vec![rows as u64, cols as u64]),
                max_shape: None,
                data: vec![7; cols * rows],
                fill: vec![0],
                storage: Storage::default(),
                attrs: vec![],
            }],
        };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("odim.h5");
        std::fs::write(&p, super::super::hdf5_write::encode(&f)).unwrap();
        let r = read(&p, "odim.h5", None).unwrap();
        let gt = r.geo_transform();
        let expected = [-124.99999993981328, 2500.0000000001472, 0.0, 125.00000000139698, 0.0, -2499.9999999991724];
        for (a, b) in gt.iter().zip(expected) {
            assert!((a - b).abs() < 1e-4, "{gt:?} != {expected:?}");
        }
        assert!(r.crs().proj4.unwrap_or_default().contains("+proj=stere"));
    }

    /// A NISAR-like file (GDAL's `fake_NISAR_L2_epsg_code.h5`): its CRS from the grid mapping's
    /// `epsg_code`, its geotransform from the coordinates, as GDAL 3.13 reads them.
    #[test]
    fn reads_nisar_georeferencing() {
        use super::super::hdf5_write::{Attr, Dataset, File, H5Type, Storage};
        let doubles = |v: &[f64]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        let ds = |name: &str, dtype: H5Type, shape: Option<Vec<u64>>, data: Vec<u8>, attrs: Vec<Attr>| Dataset {
            name: name.into(),
            fill: vec![0; 8],
            dtype,
            shape,
            max_shape: None,
            data,
            storage: Storage::default(),
            attrs,
        };
        let f = File {
            attrs: vec![Attr::text("mission_name", "NISAR")],
            group_attrs: Vec::new(),
            datasets: vec![
                ds(
                    "test",
                    H5Type::Int { size: 1, signed: false },
                    Some(vec![5, 4]),
                    (0..20).collect(),
                    vec![Attr::text("grid_mapping", "projection")],
                ),
                ds("projection", H5Type::Int { size: 1, signed: false }, None, vec![0], vec![Attr::text("epsg_code", "32611")]),
                ds("xCoordinates", H5Type::Float { size: 8 }, Some(vec![4]), doubles(&[10.0, 20.0, 30.0, 40.0]), vec![]),
                ds("yCoordinates", H5Type::Float { size: 8 }, Some(vec![5]), doubles(&[500.0, 400.0, 300.0, 200.0, 100.0]), vec![]),
            ],
        };
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("nisar.h5");
        std::fs::write(&p, super::super::hdf5_write::encode(&f)).unwrap();
        let r = read(&p, "nisar.h5", None).unwrap();
        assert_eq!((r.raster.cols, r.raster.rows), (4, 5));
        assert_eq!(r.geo_transform(), [5.0, 10.0, 0.0, 550.0, 0.0, -100.0]);
        assert_eq!(r.crs().epsg, Some(32611));
        assert_eq!(r.raster.data.to_f64_vec(), (0..20).map(f64::from).collect::<Vec<_>>());
        // The only two-dimensional dataset opens directly: no subdatasets listed.
        assert!(!r.raster.metadata.iter().any(|(k, _)| k.starts_with("SUBDATASET_")));
        assert!(r.raster.metadata.contains(&("mission_name".into(), "NISAR".into())));
    }
}

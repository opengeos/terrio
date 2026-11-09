//! Metadata and typed hyperslabs shared by netCDF classic, netCDF-4 and numeric HDF5.
use super::*;
use netcdf_reader::{
    NcAttrValue, NcAttribute, NcFile, NcGroup, NcIntegerValue, NcMetadataMode, NcOpenOptions, NcSliceInfo, NcSliceInfoElem, NcType, NcValue,
};

fn error(e: impl std::fmt::Display) -> Error {
    Error::InvalidData(format!("multidim: {e}"))
}
fn numeric_type(t: &NcType) -> Option<DataType> {
    Some(match t {
        NcType::Byte => DataType::I8,
        NcType::UByte => DataType::U8,
        NcType::Short => DataType::I16,
        NcType::UShort => DataType::U16,
        NcType::Int => DataType::I32,
        NcType::UInt => DataType::U32,
        NcType::Int64 => DataType::I64,
        NcType::UInt64 => DataType::U64,
        NcType::Float => DataType::F32,
        NcType::Double => DataType::F64,
        _ => return None,
    })
}
fn floating(value: f64) -> Value {
    if value.is_nan() {
        Value::String("NaN".into())
    } else if value == f64::INFINITY {
        Value::String("Infinity".into())
    } else if value == f64::NEG_INFINITY {
        Value::String("-Infinity".into())
    } else {
        serde_json::json!(value)
    }
}
/// A user-defined attribute value as JSON, as GDAL 3.13's `gdalmdiminfo` prints it: a compound
/// as an object of its fields and an enum as its integer. `None` for the fixed-size array, opaque
/// and vlen values GDAL does not support, at any depth: GDAL then prints the whole attribute as
/// `null`. A char is its code, as GDAL reads it; `t` is the value's declared type.
fn user_value(v: &NcValue, t: &NcType) -> Option<Value> {
    Some(match v {
        NcValue::Byte(x) => serde_json::json!(x),
        NcValue::Char(x) | NcValue::UByte(x) => serde_json::json!(x),
        NcValue::Short(x) => serde_json::json!(x),
        NcValue::UShort(x) => serde_json::json!(x),
        NcValue::Int(x) => serde_json::json!(x),
        NcValue::UInt(x) => serde_json::json!(x),
        NcValue::Int64(x) => serde_json::json!(x),
        NcValue::UInt64(x) => serde_json::json!(x),
        NcValue::Float(x) => floating(f64::from(*x)),
        NcValue::Double(x) => floating(*x),
        NcValue::String(x) if matches!(t, NcType::Char) && x.len() <= 1 => serde_json::json!(x.bytes().next().unwrap_or(0)),
        NcValue::String(x) => Value::String(x.clone()),
        NcValue::Enum(e) => match e.value {
            NcIntegerValue::I8(x) => serde_json::json!(x),
            NcIntegerValue::U8(x) => serde_json::json!(x),
            NcIntegerValue::I16(x) => serde_json::json!(x),
            NcIntegerValue::U16(x) => serde_json::json!(x),
            NcIntegerValue::I32(x) => serde_json::json!(x),
            NcIntegerValue::U32(x) => serde_json::json!(x),
            NcIntegerValue::I64(x) => serde_json::json!(x),
            NcIntegerValue::U64(x) => serde_json::json!(x),
        },
        NcValue::Compound(fields) => {
            let declared = match t {
                NcType::Compound { fields: declared, .. } => declared.as_slice(),
                _ => &[],
            };
            // A field missing from the declared type makes the attribute `null`, not mis-typed.
            let mut object = serde_json::Map::new();
            for f in fields {
                let ty = declared.iter().find(|d| d.name == f.name).map(|d| &d.dtype)?;
                object.insert(f.name.clone(), user_value(&f.value, ty)?);
            }
            Value::Object(object)
        }
        NcValue::Array(_) | NcValue::Opaque(_) | NcValue::VLen(_) => return None,
    })
}
fn attribute(a: &NcAttrValue) -> Value {
    // A zero-length numeric, enum or compound attribute is `null` and a zero-length string attribute
    // `[]`, as GDAL 3.13.3's `gdalmdiminfo` prints them.
    fn one(v: Vec<Value>) -> Value {
        match v.len() {
            0 => Value::Null,
            1 => v.into_iter().next().unwrap(),
            _ => Value::Array(v),
        }
    }
    macro_rules! values {
        ($v:expr) => {
            one($v.iter().map(|x| serde_json::json!(x)).collect())
        };
    }
    match a {
        NcAttrValue::Bytes(v) => values!(v),
        NcAttrValue::UBytes(v) => values!(v),
        NcAttrValue::Shorts(v) => values!(v),
        NcAttrValue::UShorts(v) => values!(v),
        NcAttrValue::Ints(v) => values!(v),
        NcAttrValue::UInts(v) => values!(v),
        NcAttrValue::Int64s(v) => values!(v),
        NcAttrValue::UInt64s(v) => values!(v),
        NcAttrValue::Floats(v) => one(v.iter().map(|&x| floating(x as f64)).collect()),
        NcAttrValue::Doubles(v) => one(v.iter().map(|&x| floating(x)).collect()),
        NcAttrValue::Chars(s) => Value::String(s.clone()),
        NcAttrValue::Strings(v) if v.is_empty() => Value::Array(Vec::new()),
        NcAttrValue::Strings(v) => one(v.iter().cloned().map(Value::String).collect()),
        NcAttrValue::UserDefined { dtype, values } => {
            values.iter().map(|v| user_value(v, dtype)).collect::<Option<Vec<_>>>().map_or(Value::Null, one)
        }
    }
}
fn attributes(attrs: &[NcAttribute]) -> Value {
    Value::Object(attrs.iter().map(|a| (a.name.clone(), attribute(&a.value))).collect())
}
/// The named HDF5 datatypes a netCDF-4 group defines, with its ancestors' after its own: the
/// user-defined types in scope there, as netCDF-C finds them.
type NamedTypes = Vec<(String, hdf5_reader::Datatype)>;
fn named_types(hdf: &hdf5_reader::Hdf5File, p: &str) -> NamedTypes {
    let parts: Vec<_> = p.split('/').filter(|n| !n.is_empty()).collect();
    let mut out = Vec::new();
    for depth in (0..=parts.len()).rev() {
        let group = hdf.root_group().and_then(|root| parts[..depth].iter().try_fold(root, |g, name| g.group(name)));
        if let Ok(group) = group {
            out.extend(group_named_types(hdf, &group).unwrap_or_default());
        }
    }
    out
}
/// The named datatypes linked from one HDF5 group, from its compact or dense link storage.
fn group_named_types(hdf: &hdf5_reader::Hdf5File, group: &hdf5_reader::group::Group) -> hdf5_reader::error::Result<NamedTypes> {
    use hdf5_reader::messages::{HdfMessage, link::LinkTarget};
    let (storage, offset_size, length_size) = (group.storage(), group.offset_size(), group.length_size());
    let mut links = Vec::new();
    for message in &hdf.get_or_parse_header(group.address())?.messages {
        match message {
            HdfMessage::Link(link) => links.push(link.clone()),
            HdfMessage::LinkInfo(info) if !hdf5_reader::io::Cursor::is_undefined_offset(info.fractal_heap_address, offset_size) => {
                let heap =
                    hdf5_reader::fractal_heap::FractalHeap::parse_at_storage(storage, info.fractal_heap_address, offset_size, length_size)?;
                let index = hdf5_reader::btree_v2::BTreeV2Header::parse_at_storage(
                    storage,
                    info.btree_name_index_address,
                    offset_size,
                    length_size,
                )?;
                for record in
                    hdf5_reader::btree_v2::collect_btree_v2_records_storage(storage, &index, offset_size, length_size, None, &[], None)?
                {
                    if let hdf5_reader::btree_v2::BTreeV2Record::LinkNameHash { heap_id, .. } = record {
                        let bytes = heap.get_object_storage(&heap_id, storage, offset_size, length_size)?;
                        links.push(hdf5_reader::messages::link::parse(
                            &mut hdf5_reader::io::Cursor::new(&bytes),
                            offset_size,
                            length_size,
                            bytes.len(),
                        )?);
                    }
                }
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    for link in links {
        let LinkTarget::Hard { address } = link.target else { continue };
        // A link whose target cannot be parsed is skipped, keeping the scope's other types.
        let Ok(header) = hdf.get_or_parse_header(address) else { continue };
        // A named datatype's object header holds a datatype but, unlike a dataset, no dataspace.
        if header.messages.iter().any(|m| matches!(m, HdfMessage::Dataspace(_))) {
            continue;
        }
        if let Some(datatype) = header.messages.iter().find_map(|m| match m {
            HdfMessage::Datatype(d) => Some(d.datatype.clone()),
            _ => None,
        }) {
            out.push((link.name, datatype));
        }
    }
    Ok(out)
}
/// Whether an HDF5 datatype is the one netCDF-C reads as `t`.
fn same_type(t: &NcType, h: &hdf5_reader::Datatype) -> bool {
    use hdf5_reader::Datatype as H;
    match (t, h) {
        (NcType::Compound { size, fields }, H::Compound { size: hsize, fields: hfields }) => {
            size == hsize
                && fields.len() == hfields.len()
                && fields
                    .iter()
                    .zip(hfields)
                    .all(|(f, h)| f.name == h.name && f.offset == u64::from(h.byte_offset) && same_type(&f.dtype, &h.datatype))
        }
        (NcType::Enum { base, members }, H::Enum { base: hbase, members: hmembers }) => {
            same_type(base, hbase)
                && members.len() == hmembers.len()
                && members.iter().zip(hmembers).all(|(m, h)| m.name == h.name && m.value.as_i128() == enum_value(&h.value, hbase))
        }
        (NcType::Array { base, dims }, H::Array { base: hbase, dims: hdims }) => dims == hdims && same_type(base, hbase),
        (NcType::Char, H::String { .. }) => true,
        (NcType::Float, H::FloatingPoint { size: 4, .. }) | (NcType::Double, H::FloatingPoint { size: 8, .. }) => true,
        (t, H::FixedPoint { size, signed, .. }) => numeric_type(t).is_some_and(|d| {
            let (int, is_signed) = match d {
                DataType::I8 | DataType::I16 | DataType::I32 | DataType::I64 => (true, true),
                DataType::U8 | DataType::U16 | DataType::U32 | DataType::U64 => (true, false),
                _ => (false, false),
            };
            int && d.size_bytes() == usize::from(*size) && is_signed == *signed
        }),
        _ => false,
    }
}
/// The value of an HDF5 enum member, stored in its integer `base` type.
fn enum_value(bytes: &[u8], base: &hdf5_reader::Datatype) -> Option<i128> {
    let hdf5_reader::Datatype::FixedPoint { signed, byte_order, .. } = base else { return None };
    if bytes.len() > 16 {
        return None;
    }
    let mut le = bytes.to_vec();
    if matches!(byte_order, hdf5_reader::error::ByteOrder::BigEndian) {
        le.reverse();
    }
    let negative = *signed && le.last().is_some_and(|b| b & 0x80 != 0);
    let mut out = [if negative { 0xff } else { 0 }; 16];
    out[..le.len()].copy_from_slice(&le);
    Some(i128::from_le_bytes(out))
}
/// A netCDF-4 compound type as a snapshot writes it, as GDAL 3.13.3 does: enum fields as their
/// base type and char fields as unsigned bytes. `None` when a field, at any depth, is of a type
/// GDAL does not carry (fixed-size arrays, vlens, opaques and strings). The type is named as its
/// group, or an ancestor, names it (`named`), else `fallback`.
fn compound_type(t: &NcType, named: &NamedTypes, fallback: &str) -> Option<CompoundType> {
    let NcType::Compound { size, fields } = t else { return None };
    let fields = fields
        .iter()
        .map(|f| {
            let dtype = match &f.dtype {
                NcType::Char => FieldType::Number(DataType::U8),
                NcType::Enum { base, .. } => FieldType::Number(numeric_type(base)?),
                nested @ NcType::Compound { .. } => FieldType::Compound(compound_type(nested, named, &format!("{}_t", f.name))?),
                other => FieldType::Number(numeric_type(other)?),
            };
            Some(CompoundField { name: f.name.clone(), offset: usize::try_from(f.offset).ok()?, dtype })
        })
        .collect::<Option<Vec<_>>>()?;
    let name = named.iter().find(|(_, h)| same_type(t, h)).map_or_else(|| fallback.to_owned(), |(name, _)| name.clone());
    Some(CompoundType { name, size: *size as usize, fields })
}
/// The source types of attributes; `named` gives the named types in scope, read only when a
/// compound attribute needs one.
fn attribute_types<'a>(attrs: &[NcAttribute], named: &dyn Fn() -> &'a NamedTypes) -> AttributeTypes {
    let mut types = AttributeTypes::new();
    for a in attrs {
        let dtype = match &a.value {
            NcAttrValue::Bytes(_) => DataType::I8,
            NcAttrValue::UBytes(_) => DataType::U8,
            NcAttrValue::Shorts(_) => DataType::I16,
            NcAttrValue::UShorts(_) => DataType::U16,
            NcAttrValue::Ints(_) => DataType::I32,
            NcAttrValue::UInts(_) => DataType::U32,
            NcAttrValue::Int64s(_) => DataType::I64,
            NcAttrValue::UInt64s(_) => DataType::U64,
            NcAttrValue::Floats(_) => DataType::F32,
            NcAttrValue::Doubles(_) => DataType::F64,
            // An enum is written as its base integer type, as GDAL 3.13.3 copies it.
            NcAttrValue::UserDefined { dtype: NcType::Enum { base, .. }, .. } => match numeric_type(base) {
                Some(dtype) => dtype,
                None => continue,
            },
            // A compound that reads as a value (not `null`) keeps its type.
            NcAttrValue::UserDefined { dtype: dtype @ NcType::Compound { .. }, .. } => {
                if !attribute(&a.value).is_null()
                    && let Some(compound) = compound_type(dtype, named(), &format!("{}_t", a.name))
                {
                    types.compound.insert(a.name.clone(), compound);
                }
                continue;
            }
            NcAttrValue::Chars(_) | NcAttrValue::Strings(_) | NcAttrValue::UserDefined { .. } => continue,
        };
        types.numeric.insert(a.name.clone(), dtype);
    }
    types
}
fn group(g: &NcGroup, p: &str, file: &Arc<NcFile>, hdf: Option<&hdf5_reader::Hdf5File>, inherited: &[Dimension]) -> Result<Group> {
    let mut out = Group::new(p);
    out.attributes = attributes(&g.attributes);
    // The named types in scope, read once per group and only when a compound attribute needs them.
    let scope = std::cell::OnceCell::new();
    let named = || scope.get_or_init(|| hdf.map(|h| named_types(h, p)).unwrap_or_default());
    out.attribute_types = attribute_types(&g.attributes, &named);
    let mut visible = inherited.to_vec();
    for d in &g.dimensions {
        let full = path(p, &d.name);
        let local_scale = hdf
            .and_then(|h| h.dataset(&full).ok())
            .and_then(|v| v.attribute("CLASS").ok())
            .and_then(|a| a.read_string().ok())
            .is_some_and(|c| c == "DIMENSION_SCALE");
        if !local_scale && inherited.iter().any(|i| i.name == d.name && i.size as u64 == d.size) {
            continue;
        }
        let size = usize::try_from(d.size).map_err(error)?;
        let dimension = Dimension {
            name: d.name.clone(),
            path: full.clone(),
            size,
            unlimited: d.is_unlimited,
            indexing_variable: g.variables.iter().any(|v| v.name == d.name && v.is_coordinate_variable()).then_some(full),
        };
        visible.retain(|i| i.name != d.name);
        visible.push(dimension.clone());
        out.dimensions.push(dimension);
    }
    for v in &g.variables {
        let full = path(p, &v.name);
        let Some(data_type) =
            (if v.dtype == NcType::Char && v.dimensions.is_empty() { Some(DataType::U8) } else { numeric_type(&v.dtype) })
        else {
            out.unsupported_arrays.push(UnsupportedArray {
                path: full,
                data_type: format!("{:?}", v.dtype),
                shape: v.shape().into_iter().map(|n| usize::try_from(n).map_err(error)).collect::<Result<_>>()?,
            });
            continue;
        };
        let mut dimensions = Vec::new();
        for (i, d) in v.dimensions.iter().enumerate() {
            let size = usize::try_from(d.size).map_err(error)?;
            let dimension = match visible.iter().rev().find(|n| n.name == d.name && n.size == size) {
                Some(d) => d.clone(),
                None => {
                    let name = format!("{}_dim{i}", v.name);
                    let dimension = Dimension { path: path(p, &name), name, size, unlimited: d.is_unlimited, indexing_variable: None };
                    out.dimensions.push(dimension.clone());
                    dimension
                }
            };
            dimensions.push(dimension);
        }
        let chunks = hdf.and_then(|h| h.dataset(&full).ok()).and_then(|d| d.chunks()).map(|s| s.into_iter().map(|n| n as usize).collect());
        let coordinates = dimensions.iter().filter_map(|d| d.indexing_variable.clone()).collect();
        out.arrays.push(Array {
            info: ArrayInfo {
                name: v.name.clone(),
                path: full,
                data_type,
                dimensions,
                chunks,
                attributes: attributes(&v.attributes),
                coordinates,
            },
            source: Source::Netcdf(Arc::clone(file)),
            attribute_types: attribute_types(&v.attributes, &named),
        });
    }
    // Lossy netCDF reconstruction can omit HDF5-specific datatypes. Report those
    // datasets explicitly rather than making numeric siblings hide their existence.
    if let Some(hdf) = hdf {
        let mut raw_group = hdf.root_group().map_err(error)?;
        for name in p.split('/').filter(|n| !n.is_empty()) {
            raw_group = raw_group.group(name).map_err(error)?;
        }
        for raw in raw_group.datasets().map_err(error)? {
            let name = raw.name().rsplit('/').next().unwrap_or(raw.name());
            if g.variables.iter().any(|v| v.name == name) {
                continue;
            }
            let dimension_only = raw
                .attribute("NAME")
                .ok()
                .and_then(|a| a.read_string().ok())
                .is_some_and(|name| name.starts_with("This is a netCDF dimension but not a netCDF variable."));
            if dimension_only {
                continue;
            }
            out.unsupported_arrays.push(UnsupportedArray {
                path: path(p, name),
                data_type: format!("{:?}", raw.dtype()),
                shape: raw.shape().iter().map(|&n| usize::try_from(n).map_err(error)).collect::<Result<_>>()?,
            });
        }
    }
    for child in &g.groups {
        out.groups.push(group(child, &path(p, &child.name), file, hdf, &visible)?);
    }
    Ok(out)
}
fn dataset(file: NcFile, hdf: Option<hdf5_reader::Hdf5File>, driver: &str) -> Result<Dataset> {
    let file = Arc::new(file);
    let mut root = group(file.root_group().map_err(error)?, "/", &file, hdf.as_ref(), &[])?;
    resolve_coordinates(&mut root);
    Ok(Dataset { driver: driver.into(), root })
}
struct OffsetStorage {
    inner: hdf5_reader::storage::DynStorage,
    offset: u64,
}
impl hdf5_reader::storage::Storage for OffsetStorage {
    fn len(&self) -> u64 {
        self.inner.len() - self.offset
    }
    fn read_range(&self, offset: u64, len: usize) -> hdf5_reader::error::Result<hdf5_reader::storage::StorageBuffer> {
        self.inner.read_range(self.offset.checked_add(offset).ok_or(hdf5_reader::error::Error::OffsetOutOfBounds(offset))?, len)
    }
}
pub(super) fn open(name: &str) -> Result<Dataset> {
    let mut storage: hdf5_reader::storage::DynStorage = Arc::new(hdf5_reader::storage::FileStorage::open(name).map_err(error)?);
    // The driver the dataset is opened with, as the raster read names it (netCDF for a classic file
    // or an HDF5 one under netCDF's names, HDF5 otherwise): judged before either reader runs.
    let driver = if crate::drivers::netcdf::is_netcdf(std::path::Path::new(name)) { "netCDF" } else { "HDF5" };
    crate::formats::check_driver_allowed(driver, name, crate::formats::Kind::Raster)?;
    let header = storage.read_range(0, storage.len().min(8) as usize).map_err(error)?;
    let classic = header.starts_with(b"CDF");
    if !classic {
        let mut offset = 0u64;
        for _ in 0..20 {
            if offset.checked_add(8).is_none_or(|end| end > storage.len()) {
                break;
            }
            if storage.read_range(offset, 8).map_err(error)?.as_ref() == b"\x89HDF\r\n\x1a\n" {
                if offset > 0 {
                    storage = Arc::new(OffsetStorage { inner: storage, offset });
                }
                break;
            }
            offset = if offset == 0 { 512 } else { offset * 2 };
        }
    }
    let file = NcFile::from_storage_with_options(
        Arc::clone(&storage),
        NcOpenOptions { metadata_mode: NcMetadataMode::Lossy, ..Default::default() },
    )
    .map_err(error)?;
    let hdf = (!classic).then(|| hdf5_reader::Hdf5File::from_storage(storage).ok()).flatten();
    dataset(file, hdf, driver)
}
pub(super) fn open_bytes(bytes: &[u8]) -> Result<Dataset> {
    // Bytes have no name: a classic file is netCDF's, any other is read as HDF5 (netCDF-4 too), as
    // a file named for neither driver is. Judged before either reader runs.
    let driver = if bytes.starts_with(b"CDF") { "netCDF" } else { "HDF5" };
    crate::formats::check_driver_allowed(driver, "a file in memory", crate::formats::Kind::Raster)?;
    let body = crate::drivers::hdf5::signature_offset_in(bytes).map(|offset| &bytes[offset..]).unwrap_or(bytes);
    open_bytes_owned(body.to_vec())
}
fn owned_storage(bytes: Vec<u8>) -> hdf5_reader::storage::DynStorage {
    let offset = crate::drivers::hdf5::signature_offset_in(&bytes).unwrap_or(0);
    let storage: hdf5_reader::storage::DynStorage = Arc::new(hdf5_reader::storage::BytesStorage::new(bytes));
    if offset == 0 { storage } else { Arc::new(OffsetStorage { inner: storage, offset: offset as u64 }) }
}
pub(super) fn open_bytes_owned(bytes: Vec<u8>) -> Result<Dataset> {
    let classic = bytes.starts_with(b"CDF");
    let driver = if classic { "netCDF" } else { "HDF5" };
    crate::formats::check_driver_allowed(driver, "a file in memory", crate::formats::Kind::Raster)?;
    // Both metadata readers share retained storage, including any userblock.
    // BytesStorage retains the Vec without copying it; avoid an extra input clone here.
    let storage = owned_storage(bytes);
    let file = NcFile::from_storage_with_options(
        Arc::clone(&storage),
        NcOpenOptions { metadata_mode: NcMetadataMode::Lossy, ..Default::default() },
    )
    .map_err(error)?;
    let hdf = (!classic).then(|| hdf5_reader::Hdf5File::from_storage(storage).ok()).flatten();
    dataset(file, hdf, if classic { "netCDF" } else { "netCDF/HDF5" })
}
pub(super) fn read(file: &NcFile, name: &str, t: DataType, selections: &[Selection]) -> Result<Vec<u8>> {
    let variable = file.variable(name.trim_start_matches('/')).map_err(error)?;
    if variable.dtype == NcType::Char && variable.dimensions.is_empty() {
        return file.read_variable_native_bytes(name.trim_start_matches('/')).map_err(error);
    }
    let selection = NcSliceInfo {
        selections: selections
            .iter()
            .map(|s| NcSliceInfoElem::Slice {
                start: s.start as u64,
                end: (s.start + (s.count - 1) * s.step + 1) as u64,
                step: s.step as u64,
            })
            .collect(),
    };
    macro_rules! read {
        ($t:ty) => {{
            let array = file.read_variable_slice::<$t>(name.trim_start_matches('/'), &selection).map_err(error)?;
            array.iter().flat_map(|v| v.to_le_bytes()).collect()
        }};
    }
    Ok(match t {
        DataType::U8 => read!(u8),
        DataType::I8 => read!(i8),
        DataType::U16 => read!(u16),
        DataType::I16 => read!(i16),
        DataType::U32 => read!(u32),
        DataType::I32 => read!(i32),
        DataType::U64 => read!(u64),
        DataType::I64 => read!(i64),
        DataType::F32 => read!(f32),
        DataType::F64 => read!(f64),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use hdf5_reader::storage::{BytesStorage, Storage, StorageBuffer};
    use std::sync::atomic::{AtomicU64, Ordering};
    fn classic_fixture() -> Vec<u8> {
        let mut bytes = b"CDF\x01".to_vec();
        // CDF-1: one dimension, no attributes, one packed Int16 variable.
        for word in [0u32, 10, 1, 1, 0x78000000, 3, 0, 0, 11, 1, 1, 0x76000000, 1, 0, 0, 0, 3, 8, 80] {
            bytes.extend_from_slice(&word.to_be_bytes());
        }
        assert_eq!(bytes.len(), 80);
        for value in [-7i16, 0, 42, 0] {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
        bytes
    }
    #[test]
    fn owned_classic_preserves_values_and_retained_views() {
        let bytes = classic_fixture();
        let borrowed = super::open_bytes(&bytes).unwrap();
        let owned = crate::multidim::open_bytes_owned(bytes).unwrap();
        let array = owned.array("v").unwrap();
        assert_eq!(serde_json::to_value(&array.info).unwrap(), serde_json::to_value(&borrowed.array("v").unwrap().info).unwrap());
        let view = array.view();
        drop(array);
        drop(owned);
        assert_eq!(view.read().unwrap().values().unwrap(), serde_json::json!([-7, 0, 42]));
    }
    #[test]
    fn owned_hdf_userblocks_share_retained_storage_and_retain_views() {
        use crate::drivers::hdf5_write::{self, H5Type};
        let raw = hdf5_write::encode(&hdf5_write::File {
            datasets: vec![hdf5_write::Dataset {
                name: "sample".into(),
                dtype: H5Type::Int { size: 2, signed: true },
                shape: Some(vec![3]),
                max_shape: None,
                data: [-7i16, 0, 42].into_iter().flat_map(i16::to_le_bytes).collect(),
                fill: vec![0; 2],
                storage: Default::default(),
                attrs: vec![],
            }],
            ..Default::default()
        });
        for offset in [0, 512, 1024, 4096] {
            let mut bytes = vec![0; offset];
            bytes.extend_from_slice(&raw);
            let storage = owned_storage(bytes.clone());
            assert_eq!(storage.len() as usize, raw.len());
            assert!(storage.read_range(u64::MAX, 8).is_err());
            assert!(storage.read_range(raw.len() as u64, 1).is_err());
            let storage = owned_storage(bytes);
            let header = storage.read_range(0, 8).unwrap();
            let again = storage.read_range(0, 8).unwrap();
            assert_eq!(header.as_ptr(), again.as_ptr());
            assert_eq!(header.as_ref(), &raw[..8]);
            let mut bytes = vec![0; offset];
            bytes.extend_from_slice(&raw);
            let borrowed = super::open_bytes(&bytes).unwrap();
            let owned = crate::multidim::open_bytes_owned(bytes).unwrap();
            let array = owned.array("sample").unwrap();
            assert_eq!(serde_json::to_value(&array.info).unwrap(), serde_json::to_value(&borrowed.array("sample").unwrap().info).unwrap());
            let view = array.view();
            drop(array);
            drop(owned);
            assert_eq!(view.read().unwrap().values().unwrap(), serde_json::json!([-7, 0, 42]));
        }
    }
    #[test]
    fn owned_producer_written_hdf_userblock() {
        // h5py.File(path, 'w', userblock_size=512), create_dataset('sample',
        // data=[-7, 0, 42], dtype='<i2'): the producer writes real base addresses.
        let bytes = include_bytes!("../../tests/data/multidim-userblock.h5");
        assert_eq!(crate::drivers::hdf5::signature_offset_in(bytes), Some(512));
        for ds in [super::open_bytes(bytes).unwrap(), crate::multidim::open_bytes_owned(bytes.to_vec()).unwrap()] {
            let view = ds.array("sample").unwrap().view();
            drop(ds);
            assert_eq!(view.read().unwrap().values().unwrap(), serde_json::json!([-7, 0, 42]));
        }
    }
    #[test]
    fn owned_malformed_inputs_return_errors() {
        for bytes in [vec![], b"CDF".to_vec(), b"not scientific data".to_vec(), b"\x89HDF\r\n\x1a\n".to_vec(), vec![0; 1024]] {
            assert!(crate::multidim::open_bytes_owned(bytes).is_err());
        }
        let mut bytes = classic_fixture();
        bytes.truncate(24);
        assert!(crate::multidim::open_bytes_owned(bytes).is_err());
    }
    struct CountingStorage {
        inner: BytesStorage,
        bytes: Arc<AtomicU64>,
    }
    impl Storage for CountingStorage {
        fn len(&self) -> u64 {
            self.inner.len()
        }
        fn read_range(&self, offset: u64, length: usize) -> hdf5_reader::error::Result<StorageBuffer> {
            self.bytes.fetch_add(length as u64, Ordering::Relaxed);
            self.inner.read_range(offset, length)
        }
    }
    #[test]
    fn selected_reads_request_a_fraction_of_the_array_bytes() {
        let dimensions: Vec<_> = [("time", 64), ("depth", 2), ("y", 64), ("x", 64)]
            .into_iter()
            .map(|(name, size)| Dimension { name: name.into(), path: format!("/{name}"), size, unlimited: false, indexing_variable: None })
            .collect();
        let shape: Vec<_> = dimensions.iter().map(|d| d.size).collect();
        let count = elements(&shape).unwrap();
        let array = Array::from_bytes(
            ArrayInfo {
                name: "cube".into(),
                path: "/cube".into(),
                data_type: DataType::I64,
                dimensions: dimensions.clone(),
                chunks: Some(vec![1, 1, 16, 16]),
                attributes: serde_json::json!({}),
                coordinates: vec![],
            },
            (0..count as i64).flat_map(i64::to_le_bytes).collect(),
        )
        .unwrap();
        let mut root = Group::new("/");
        root.dimensions = dimensions;
        root.arrays.push(array);
        let raw = Dataset { driver: "Memory".into(), root }.to_netcdf_bytes().unwrap();
        let size = raw.len();
        let transferred = Arc::new(AtomicU64::new(0));
        let storage = Arc::new(CountingStorage { inner: BytesStorage::new(raw), bytes: Arc::clone(&transferred) });
        let file = NcFile::from_storage_with_options(storage, NcOpenOptions { metadata_mode: NcMetadataMode::Lossy, ..Default::default() })
            .unwrap();
        let dataset = super::dataset(file, None, "netCDF").unwrap();
        let baseline = transferred.load(Ordering::Relaxed);
        let array = dataset.array("cube").unwrap();
        let slab = array
            .view()
            .isel("time", 30)
            .unwrap()
            .isel("depth", 1)
            .unwrap()
            .slice("y", 0, 16, 1)
            .unwrap()
            .slice("x", 0, 16, 1)
            .unwrap()
            .read()
            .unwrap();
        assert_eq!(slab.shape, [16, 16]);
        assert_eq!(slab.values().unwrap()[0], serde_json::json!(249856));
        let selected = transferred.load(Ordering::Relaxed) - baseline;
        array.read().unwrap();
        let full = transferred.load(Ordering::Relaxed) - baseline - selected;
        eprintln!("range bytes: metadata={baseline}, selected={selected}, whole={full}, file={size}");
        assert!(selected < (size / 8) as u64, "selected read fetched {selected} bytes from a {size}-byte file");
        assert!(full > selected * 4, "full read {full} was not substantially larger than selected read {selected}");
    }
}

#[cfg(test)]
mod attribute_tests {
    use super::*;
    use crate::drivers::hdf5_write::{self, Attr, H5Type, Values};
    #[test]
    fn numeric_attribute_types_and_nonfinite_values_survive_snapshots() {
        let numeric = |name: &str, values: &[f32]| Attr {
            name: name.into(),
            dtype: H5Type::Float { size: 4 },
            shape: (values.len() != 1).then(|| vec![values.len() as u64]),
            values: Values::Bytes(values.iter().flat_map(|v| v.to_le_bytes()).collect()),
        };
        let file = hdf5_write::File {
            attrs: vec![
                numeric("nan32", &[f32::NAN]),
                numeric("vector32", &[f32::INFINITY, f32::NEG_INFINITY, 1.5]),
                Attr::text("literal", "NaN"),
            ],
            datasets: vec![hdf5_write::Dataset {
                name: "sample".into(),
                dtype: H5Type::Int { size: 1, signed: false },
                shape: Some(vec![2]),
                max_shape: None,
                data: vec![1, 2],
                fill: vec![0],
                storage: Default::default(),
                attrs: vec![numeric("minimum32", &[f32::NEG_INFINITY])],
            }],
            ..Default::default()
        };
        let original = crate::multidim::open_bytes(&hdf5_write::encode(&file)).unwrap();
        let check = |ds: &Dataset| {
            assert_eq!(ds.root.attribute_type("nan32"), Some(DataType::F32));
            assert_eq!(ds.root.attribute_type("vector32"), Some(DataType::F32));
            assert_eq!(ds.root.attribute_type("literal"), None);
            assert_eq!(ds.root.attributes["nan32"], "NaN");
            assert_eq!(ds.root.attributes["literal"], "NaN");
            assert_eq!(ds.root.attributes["vector32"], serde_json::json!(["Infinity", "-Infinity", 1.5]));
            assert_eq!(ds.array("sample").unwrap().attribute_type("minimum32"), Some(DataType::F32));
        };
        check(&original);
        check(&crate::multidim::open_bytes(&original.to_netcdf_bytes().unwrap()).unwrap());
        #[cfg(feature = "zarr")]
        {
            let zarr = crate::multidim::open_zarr_store(original.to_zarr_store().unwrap()).unwrap();
            check(&zarr);
            assert!(zarr.root.attributes.get("_terrio_attribute_types").is_none());
            check(&crate::multidim::open_bytes(&zarr.to_netcdf_bytes().unwrap()).unwrap());
        }
    }

    /// A snapshot commits the compound types GDAL 3.13.3's `gdalmdimtranslate -of netCDF` copy
    /// commits, with the same names, offsets and member types (an enum member as its base type, a
    /// char as an unsigned byte), and its attributes have the types of the copy's: what `ncdump -h`
    /// prints of the types and attributes then matches. `netcdf_copy_types` in `expected.json`
    /// describes GDAL's copies, as this test describes terrio's. `nested_compound_attrs.nc` keeps
    /// its root group's links, the named types among them, in dense storage.
    #[test]
    fn snapshot_compound_types_match_gdal_copy() {
        use hdf5_reader::Datatype as H;
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/netcdf_user_defined");
        let expected: Value = serde_json::from_slice(&std::fs::read(dir.join("expected.json")).unwrap()).unwrap();
        fn describe(t: &H, named: &NamedTypes) -> Value {
            Value::String(match t {
                H::FixedPoint { size, signed, .. } => format!("{}int{}", if *signed { "" } else { "u" }, u32::from(*size) * 8),
                H::FloatingPoint { size, .. } => format!("float{}", u32::from(*size) * 8),
                H::Compound { .. } => named.iter().find(|(_, n)| n == t).map_or_else(|| "anonymous".into(), |(name, _)| name.clone()),
                other => format!("{other:?}"),
            })
        }
        let bookkeeping = ["CLASS", "NAME", "REFERENCE_LIST", "DIMENSION_LIST", "_Netcdf4Coordinates", "_Netcdf4Dimid", "_NCProperties"];
        // The copy of a copy commits the same types as the first copy.
        let copies = expected["netcdf_copy_types"].as_object().unwrap().iter().flat_map(|(fixture, gdal)| {
            let first = super::write::netcdf_bytes(&open(dir.join(fixture).to_str().unwrap()).unwrap()).unwrap();
            let second = super::write::netcdf_bytes(&crate::multidim::open_bytes(&first).unwrap()).unwrap();
            [(fixture.to_string(), first, gdal), (format!("{fixture} (copy of the copy)"), second, gdal)]
        });
        for (fixture, bytes, gdal) in copies {
            let hdf = hdf5_reader::Hdf5File::from_vec(bytes).unwrap();
            let root = hdf.root_group().unwrap();
            let named = group_named_types(&hdf, &root).unwrap();
            let types: serde_json::Map<_, _> = named
                .iter()
                .map(|(name, t)| {
                    let H::Compound { size, fields } = t else { panic!("{name} is not a compound") };
                    let fields: Vec<_> =
                        fields.iter().map(|f| serde_json::json!([f.name, f.byte_offset, describe(&f.datatype, &named)])).collect();
                    (name.clone(), serde_json::json!({"size": size, "fields": fields}))
                })
                .collect();
            assert_eq!(Value::Object(types), gdal["types"], "{fixture}");
            let mut attributes = serde_json::Map::new();
            let objects = std::iter::once(("/".to_owned(), root.attributes().unwrap()))
                .chain(root.datasets().unwrap().into_iter().map(|d| (format!("/{}", d.name().trim_start_matches('/')), d.attributes())));
            for (path, attrs) in objects {
                let found: serde_json::Map<_, _> = attrs
                    .iter()
                    .filter(|a| !bookkeeping.contains(&a.name.as_str()) && !matches!(a.datatype, H::String { .. } | H::VarLen { .. }))
                    .map(|a| (a.name.clone(), describe(&a.datatype, &named)))
                    .collect();
                if !found.is_empty() {
                    attributes.insert(path, Value::Object(found));
                }
            }
            assert_eq!(Value::Object(attributes), gdal["attributes"], "{fixture}");
        }
    }

    /// A compound declaration matches an attribute's type only when its fields' numeric classes
    /// agree: a `float` field is not a 32-bit unsigned integer, nor is an enum one with other members.
    #[test]
    fn named_compound_matches_numeric_class_and_enum_members() {
        use hdf5_reader::Datatype as H;
        use netcdf_reader::{NcCompoundField, NcEnumMember, NcIntegerValue};
        let le = hdf5_reader::error::ByteOrder::LittleEndian;
        let compound = |dtype: NcType| NcType::Compound { size: 4, fields: vec![NcCompoundField { name: "a".into(), offset: 0, dtype }] };
        let declared =
            |datatype: H| H::Compound { size: 4, fields: vec![hdf5_reader::CompoundField { name: "a".into(), byte_offset: 0, datatype }] };
        let uint32 = H::FixedPoint { size: 4, signed: false, byte_order: le };
        assert!(same_type(&compound(NcType::UInt), &declared(uint32.clone())));
        assert!(!same_type(&compound(NcType::Float), &declared(uint32.clone())));
        assert!(!same_type(&compound(NcType::Int), &declared(uint32.clone())));
        let member = |name: &str, value: i32| NcEnumMember { name: name.into(), value: NcIntegerValue::I32(value) };
        let enumeration = |members| NcType::Enum { base: Box::new(NcType::Int), members };
        let int32 = H::FixedPoint { size: 4, signed: true, byte_order: le };
        let declared_enum = declared(H::Enum {
            base: Box::new(int32),
            members: vec![hdf5_reader::EnumMember { name: "low".into(), value: (-1i32).to_le_bytes().to_vec() }],
        });
        assert!(same_type(&compound(enumeration(vec![member("low", -1)])), &declared_enum));
        assert!(!same_type(&compound(enumeration(vec![member("low", 1)])), &declared_enum));
        assert!(!same_type(&compound(enumeration(vec![member("high", -1)])), &declared_enum));
    }
}

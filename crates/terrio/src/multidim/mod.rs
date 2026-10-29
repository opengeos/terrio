//! Native multidimensional scientific data: named groups, dimensions and numeric arrays.
//!
//! Opening reads metadata. Array views select dimensions and transpose lazily; `read` fetches
//! only the selected source hyperslab and preserves packed values and native integer widths.
//! The existing raster interfaces remain available for spatial two-dimensional views.
use crate::{DataType, Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use wbraster::raster::RasterData;

mod mosaic;
#[cfg(feature = "netcdf-hdf5")]
mod netcdf;
pub(crate) mod write;
pub use mosaic::{MosaicOptions, mosaic};

fn bad(message: impl std::fmt::Display) -> Error {
    Error::InvalidArgument(format!("multidim: {message}"))
}
#[cfg(feature = "netcdf-hdf5")]
fn path(parent: &str, name: &str) -> String {
    format!("{}/{}", parent.trim_end_matches('/'), name.trim_start_matches('/'))
}
fn elements(shape: &[usize]) -> Result<usize> {
    shape.iter().try_fold(1usize, |n, &s| n.checked_mul(s).ok_or_else(|| bad("array element count overflows")))
}
fn serialize_type<S: serde::Serializer>(t: &DataType, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(crate::info::gdal_type_name(*t))
}

pub(crate) type AttributeTypes = std::collections::BTreeMap<String, DataType>;
#[cfg(feature = "zarr")]
pub(crate) fn attribute_type_metadata(types: &AttributeTypes) -> Value {
    Value::Object(types.iter().map(|(name, dtype)| (name.clone(), Value::String(crate::info::gdal_type_name(*dtype).into()))).collect())
}
#[cfg(feature = "zarr")]
pub(crate) fn take_attribute_types(attributes: &mut Value) -> Result<AttributeTypes> {
    let Some(saved) = attributes.as_object_mut().and_then(|a| a.remove("_terrio_attribute_types")) else {
        return Ok(AttributeTypes::new());
    };
    let Some(saved) = saved.as_object() else {
        return Err(bad("invalid numeric attribute type metadata"));
    };
    saved
        .iter()
        .map(|(name, dtype)| Ok((name.clone(), crate::ops::parse_data_type(dtype.as_str().ok_or_else(|| bad("invalid attribute type"))?)?)))
        .collect()
}

/// A named dimension, scoped by its defining group.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Dimension {
    /// Local name.
    pub name: String,
    /// Absolute name, including its defining group.
    pub path: String,
    /// Current number of elements.
    pub size: usize,
    /// Whether the source declares this dimension unlimited.
    pub unlimited: bool,
    /// The one-dimensional coordinate array, when defined.
    pub indexing_variable: Option<String>,
}
/// Metadata describing one numeric array.
#[derive(Debug, Clone, Serialize)]
pub struct ArrayInfo {
    /// Local name.
    pub name: String,
    /// Absolute name.
    pub path: String,
    /// Native numeric element type; packed values are not automatically scaled.
    #[serde(serialize_with = "serialize_type")]
    pub data_type: DataType,
    /// Dimensions in source storage order.
    pub dimensions: Vec<Dimension>,
    /// Source chunk shape; None for contiguous storage.
    pub chunks: Option<Vec<usize>>,
    /// Attributes; nonfinite numeric attributes use NaN/Infinity strings.
    pub attributes: Value,
    /// Coordinate array references, including CF auxiliary coordinates.
    pub coordinates: Vec<String>,
}
/// A discoverable array whose payload type or encoding is unsupported by this API.
#[derive(Debug, Clone, Serialize)]
pub struct UnsupportedArray {
    /// Absolute name.
    pub path: String,
    /// Source type description.
    pub data_type: String,
    /// Source shape.
    pub shape: Vec<usize>,
}
/// A group and its locally defined dimensions, arrays and children.
#[derive(Debug, Clone, Serialize)]
pub struct Group {
    /// Local name; "/" for the root.
    pub name: String,
    /// Absolute name.
    pub path: String,
    /// Group attributes.
    pub attributes: Value,
    #[serde(skip)]
    pub(crate) attribute_types: AttributeTypes,
    /// Dimensions defined in this group.
    pub dimensions: Vec<Dimension>,
    /// Numeric arrays, without reading their elements.
    pub arrays: Vec<Array>,
    /// Arrays with unsupported payloads, reported explicitly rather than silently omitted.
    pub unsupported_arrays: Vec<UnsupportedArray>,
    /// Child groups.
    pub groups: Vec<Group>,
}
impl Group {
    /// Constructs an empty group at an absolute path.
    pub fn new(given: &str) -> Self {
        let p = if given.trim_matches('/').is_empty() { "/".into() } else { format!("/{}", given.trim_matches('/')) };
        Self {
            name: p.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("/").into(),
            path: p,
            attributes: serde_json::json!({}),
            attribute_types: AttributeTypes::new(),
            dimensions: Vec::new(),
            arrays: Vec::new(),
            unsupported_arrays: Vec::new(),
            groups: Vec::new(),
        }
    }
    /// Original numeric type of a source attribute, when known.
    pub fn attribute_type(&self, name: &str) -> Option<DataType> {
        self.attribute_types.get(name).copied()
    }
    fn find(&self, p: &str) -> Option<&Array> {
        self.arrays.iter().find(|a| a.info.path == p).or_else(|| self.groups.iter().find_map(|g| g.find(p)))
    }
    /// All numeric and unsupported array paths, recursively in group order.
    pub(crate) fn array_paths_recursive(&self) -> Vec<&str> {
        self.arrays
            .iter()
            .map(|a| a.info.path.as_str())
            .chain(self.unsupported_arrays.iter().map(|a| a.path.as_str()))
            .chain(self.groups.iter().flat_map(|g| g.array_paths_recursive()))
            .collect()
    }
    /// All numeric arrays, recursively in group order.
    pub fn arrays_recursive(&self) -> Vec<&Array> {
        self.arrays.iter().chain(self.groups.iter().flat_map(|g| g.arrays_recursive())).collect()
    }
}
/// An opened scientific dataset. All array payloads remain lazy until read.
#[derive(Debug, Clone, Serialize)]
pub struct Dataset {
    /// Source driver: netCDF, HDF5, Zarr or Memory.
    pub driver: String,
    /// Root group.
    pub root: Group,
}
impl Dataset {
    /// Looks up an absolute array path (a root array may omit its leading slash).
    pub fn array(&self, given: &str) -> Result<Array> {
        let p = format!("/{}", given.trim_matches('/'));
        self.root.find(&p).cloned().ok_or_else(|| bad(format!("no numeric array named '{given}'")))
    }
    /// Writes a numeric snapshot, retaining coordinates, dimensions, attributes and chunk shapes.
    /// The destination extension selects netCDF-4 (`.nc`) or Zarr v2 (`.zarr`).
    pub fn write(&self, destination: &str) -> Result<()> {
        write::write(self, destination)
    }
    /// Serializes a numeric snapshot as netCDF-4 bytes, including coordinates and chunk shapes.
    pub fn to_netcdf_bytes(&self) -> Result<Vec<u8>> {
        write::validate(self)?;
        #[cfg(feature = "netcdf-hdf5")]
        {
            write::netcdf_bytes(self)
        }
        #[cfg(not(feature = "netcdf-hdf5"))]
        {
            Err(Error::disabled("netcdf-hdf5"))
        }
    }
    /// Serializes a numeric Zarr v2 snapshot as key/value files relative to the store root.
    pub fn to_zarr_store(&self) -> Result<Vec<(String, Vec<u8>)>> {
        write::validate(self)?;
        #[cfg(feature = "zarr")]
        {
            let mount = crate::memfs::Mount::new(Vec::<(String, Vec<u8>)>::new());
            let destination = mount.path().join("data.zarr");
            crate::drivers::zarr::md_write(self, destination.to_str().ok_or_else(|| bad("non-UTF8 output path"))?)?;
            Ok(mount
                .files()
                .into_iter()
                .filter_map(|(name, bytes)| name.strip_prefix("data.zarr/").map(|n| (n.to_string(), bytes)))
                .collect())
        }
        #[cfg(not(feature = "zarr"))]
        {
            Err(Error::disabled("zarr"))
        }
    }
}
#[derive(Clone)]
pub(crate) enum Source {
    Memory(Arc<Vec<u8>>),
    Mosaic(Arc<mosaic::MosaicSource>),
    #[cfg(feature = "netcdf-hdf5")]
    Netcdf(Arc<netcdf_reader::NcFile>),
    #[cfg(feature = "zarr")]
    Zarr(Arc<crate::drivers::zarr::MdSource>),
}
/// A numeric array handle. Cloning a handle never reads or copies its payload.
#[derive(Clone, Serialize)]
pub struct Array {
    /// Array metadata.
    #[serde(flatten)]
    pub info: ArrayInfo,
    #[serde(skip)]
    pub(crate) source: Source,
    #[serde(skip)]
    pub(crate) attribute_types: AttributeTypes,
}
impl std::fmt::Debug for Array {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.info.fmt(f)
    }
}
impl Array {
    /// Constructs an array from native little-endian row-major numeric bytes.
    pub fn from_bytes(info: ArrayInfo, bytes: Vec<u8>) -> Result<Self> {
        let shape: Vec<_> = info.dimensions.iter().map(|d| d.size).collect();
        let n = elements(&shape)?.checked_mul(info.data_type.size_bytes()).ok_or_else(|| bad("array byte length overflows"))?;
        if bytes.len() != n {
            return Err(bad("array byte length does not match its dimensions and type"));
        }
        if info.chunks.as_ref().is_some_and(|c| c.len() != shape.len() || c.contains(&0)) {
            return Err(bad("chunk shape must have one positive size per dimension"));
        }
        Ok(Self { info, source: Source::Memory(Arc::new(bytes)), attribute_types: AttributeTypes::new() })
    }
    /// Original numeric type of a source attribute, when known.
    pub fn attribute_type(&self, name: &str) -> Option<DataType> {
        self.attribute_types.get(name).copied()
    }
    /// The full array's shape.
    pub fn shape(&self) -> Vec<usize> {
        self.info.dimensions.iter().map(|d| d.size).collect()
    }
    /// An initially unselected lazy view.
    pub fn view(&self) -> View {
        View {
            array: self.clone(),
            axes: self.shape().into_iter().map(|count| Selection { start: 0, count, step: 1, keep: true }).collect(),
            order: (0..self.info.dimensions.len()).collect(),
        }
    }
    /// Reads the complete numeric array in native source order.
    pub fn read(&self) -> Result<Buffer> {
        self.view().read()
    }
}
/// One source-axis hyperslab selection.
#[derive(Debug, Clone)]
pub(crate) struct Selection {
    pub start: usize,
    pub count: usize,
    pub step: usize,
    pub keep: bool,
}
/// A lazy dimension selection and transposition of an array.
#[derive(Clone)]
pub struct View {
    array: Array,
    axes: Vec<Selection>,
    order: Vec<usize>,
}
impl View {
    /// Dimensions in the current view order, with selected lengths.
    pub fn dimensions(&self) -> Vec<Dimension> {
        self.order
            .iter()
            .map(|&i| {
                let mut d = self.array.info.dimensions[i].clone();
                d.size = self.axes[i].count;
                d
            })
            .collect()
    }
    /// Shape in the current view order.
    pub fn shape(&self) -> Vec<usize> {
        self.order.iter().map(|&i| self.axes[i].count).collect()
    }
    /// Native element type.
    pub fn data_type(&self) -> DataType {
        self.array.info.data_type
    }
    fn axis(&self, name: &str) -> Result<usize> {
        let found: Vec<_> = self
            .order
            .iter()
            .copied()
            .filter(|&i| {
                let d = &self.array.info.dimensions[i];
                d.name == name || d.path == name
            })
            .collect();
        if found.len() != 1 {
            return Err(bad(format!("dimension '{name}' is missing or ambiguous")));
        }
        Ok(found[0])
    }
    /// Selects one index by dimension name and removes that axis, without reading data.
    pub fn isel(&self, dimension: &str, index: usize) -> Result<Self> {
        let source = self.axis(dimension)?;
        self.isel_axis(self.order.iter().position(|&i| i == source).expect("current axis"), index)
    }
    /// Selects an axis by its zero-based position in the current view; repeated names are supported.
    pub fn isel_axis(&self, axis: usize, index: usize) -> Result<Self> {
        let i = *self.order.get(axis).ok_or_else(|| bad("axis position is out of bounds"))?;
        let old = &self.axes[i];
        if index >= old.count {
            return Err(bad("dimension index is out of bounds"));
        }
        let mut out = self.clone();
        out.axes[i] = Selection { start: old.start + index * old.step, count: 1, step: 1, keep: false };
        out.order.retain(|&a| a != i);
        Ok(out)
    }
    /// Selects a positive-stride range relative to the current view of a named dimension.
    pub fn slice(&self, dimension: &str, start: usize, count: usize, step: usize) -> Result<Self> {
        let source = self.axis(dimension)?;
        self.slice_axis(self.order.iter().position(|&i| i == source).expect("current axis"), start, count, step)
    }
    /// Slices a zero-based axis position in the current view, including repeated dimension names.
    pub fn slice_axis(&self, axis: usize, start: usize, count: usize, step: usize) -> Result<Self> {
        let i = *self.order.get(axis).ok_or_else(|| bad("axis position is out of bounds"))?;
        let old = &self.axes[i];
        if step == 0
            || start > old.count
            || (count > 0
                && start
                    .checked_add((count - 1).checked_mul(step).ok_or_else(|| bad("slice overflows"))?)
                    .is_none_or(|last| last >= old.count))
        {
            return Err(bad("slice is out of bounds or has zero step"));
        }
        let mut out = self.clone();
        out.axes[i] = Selection {
            start: old
                .start
                .checked_add(start.checked_mul(old.step).ok_or_else(|| bad("slice overflows"))?)
                .ok_or_else(|| bad("slice overflows"))?,
            count,
            step: old.step.checked_mul(step).ok_or_else(|| bad("slice step overflows"))?,
            keep: true,
        };
        Ok(out)
    }
    /// Reorders the current axes by a permutation of their current positions.
    pub fn transpose(&self, permutation: &[usize]) -> Result<Self> {
        let mut sorted = permutation.to_vec();
        sorted.sort_unstable();
        if sorted != (0..self.order.len()).collect::<Vec<_>>() {
            return Err(bad("transpose requires a permutation of every current axis"));
        }
        let mut out = self.clone();
        out.order = permutation.iter().map(|&i| self.order[i]).collect();
        Ok(out)
    }
    /// Fetches the selected hyperslab, then transposes the requested elements without converting types.
    pub fn read(&self) -> Result<Buffer> {
        let shape = self.shape();
        let n = elements(&shape)?;
        let size = self.data_type().size_bytes();
        let bytes = n.checked_mul(size).ok_or_else(|| bad("array byte length overflows"))?;
        if n == 0 {
            return Ok(Buffer { data_type: self.data_type(), shape, bytes: Vec::new() });
        }
        let raw = match &self.array.source {
            Source::Mosaic(source) => source.read(&self.axes)?,
            Source::Memory(b) => select_bytes(b, &self.array.shape(), &self.axes, size)?,
            #[cfg(feature = "netcdf-hdf5")]
            Source::Netcdf(f) => netcdf::read(f, &self.array.info.path, self.data_type(), &self.axes)?,
            #[cfg(feature = "zarr")]
            Source::Zarr(s) => s.read(&self.axes)?,
        };
        if raw.len() != bytes {
            return Err(Error::InvalidData("multidim: backend returned the wrong byte length".into()));
        }
        let natural: Vec<_> = self.axes.iter().enumerate().filter_map(|(i, s)| s.keep.then_some(i)).collect();
        if natural == self.order {
            return Ok(Buffer { data_type: self.data_type(), shape, bytes: raw });
        }
        let source_shape: Vec<_> = natural.iter().map(|&i| self.axes[i].count).collect();
        let strides = strides(&source_shape)?;
        let permuted: Vec<_> = self.order.iter().map(|axis| strides[natural.iter().position(|a| a == axis).unwrap()]).collect();
        let mut out = vec![0u8; bytes];
        for flat in 0..n {
            let mut remaining = flat;
            let mut src = 0;
            for pos in (0..shape.len()).rev() {
                let coord = remaining % shape[pos];
                remaining /= shape[pos];
                src += coord * permuted[pos];
            }
            out[flat * size..(flat + 1) * size].copy_from_slice(&raw[src * size..(src + 1) * size]);
        }
        Ok(Buffer { data_type: self.data_type(), shape, bytes: out })
    }
}
fn strides(shape: &[usize]) -> Result<Vec<usize>> {
    let mut n = 1usize;
    let mut out = vec![0; shape.len()];
    for i in (0..shape.len()).rev() {
        out[i] = n;
        n = n.checked_mul(shape[i]).ok_or_else(|| bad("array strides overflow"))?;
    }
    Ok(out)
}
fn json_float(value: f64) -> Value {
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
pub(crate) fn select_bytes(raw: &[u8], shape: &[usize], selection: &[Selection], size: usize) -> Result<Vec<u8>> {
    let output: Vec<_> = selection.iter().map(|s| s.count).collect();
    let n = elements(&output)?;
    let st = strides(shape)?;
    let mut out = Vec::with_capacity(n.checked_mul(size).ok_or_else(|| bad("array byte length overflows"))?);
    for flat in 0..n {
        let mut remaining = flat;
        let mut offset = 0;
        for i in (0..output.len()).rev() {
            let coordinate = remaining % output[i];
            remaining /= output[i];
            offset += (selection[i].start + coordinate * selection[i].step) * st[i];
        }
        out.extend_from_slice(raw.get(offset * size..(offset + 1) * size).ok_or_else(|| bad("selected data is truncated"))?);
    }
    Ok(out)
}
/// A row-major selected numeric array, with exact little-endian element bytes.
#[derive(Clone, Debug)]
pub struct Buffer {
    /// Native element type.
    pub data_type: DataType,
    /// Selected shape, after indexed axes are removed and transposition applied.
    pub shape: Vec<usize>,
    /// Little-endian numeric values without padding.
    pub bytes: Vec<u8>,
}
impl Buffer {
    /// Converts to the shared typed numeric buffer without passing through floating point.
    pub fn data(&self) -> Result<RasterData> {
        use crate::drivers::common::{Interleave, Layout};
        crate::drivers::common::decode(
            &self.bytes,
            &Layout {
                data_type: self.data_type,
                little_endian: true,
                cols: elements(&self.shape)?,
                rows: 1,
                bands: 1,
                offset: 0,
                interleave: Interleave::Bsq,
                bottom_up: false,
            },
        )
    }
    /// Values as JSON, retaining signed and unsigned 64-bit integer values exactly.
    pub fn values(&self) -> Result<Value> {
        macro_rules! vals {
            ($v:expr) => {
                Value::Array($v.into_iter().map(|x| serde_json::json!(x)).collect())
            };
        }
        Ok(match self.data()? {
            RasterData::U8(v) => vals!(v),
            RasterData::I8(v) => vals!(v),
            RasterData::U16(v) => vals!(v),
            RasterData::I16(v) => vals!(v),
            RasterData::U32(v) => vals!(v),
            RasterData::I32(v) => vals!(v),
            RasterData::U64(v) => vals!(v),
            RasterData::I64(v) => vals!(v),
            RasterData::F32(v) => Value::Array(v.into_iter().map(|x| json_float(x as f64)).collect()),
            RasterData::F64(v) => Value::Array(v.into_iter().map(json_float).collect()),
        })
    }
}
/// Opens local netCDF/HDF5 or a local/remote Zarr store without reading array payloads.
pub fn open(name: &str) -> Result<Dataset> {
    #[cfg(feature = "zarr")]
    if name.to_ascii_lowercase().ends_with(".zarr")
        || crate::drivers::zarr::parse_name(name).is_some()
        || crate::drivers::zarr::is_zarr(std::path::Path::new(name))
    {
        return crate::drivers::zarr::md_open(name, None);
    }
    #[cfg(feature = "netcdf-hdf5")]
    {
        netcdf::open(name)
    }
    #[cfg(not(feature = "netcdf-hdf5"))]
    {
        let _ = name;
        Err(Error::disabled("netcdf-hdf5"))
    }
}
/// Opens in-memory netCDF/HDF5 bytes. The resulting lazy handle owns the input storage.
pub fn open_bytes(bytes: &[u8]) -> Result<Dataset> {
    #[cfg(feature = "netcdf-hdf5")]
    {
        netcdf::open_bytes(bytes)
    }
    #[cfg(not(feature = "netcdf-hdf5"))]
    {
        let _ = bytes;
        Err(Error::disabled("netcdf-hdf5"))
    }
}
/// Opens Zarr key/value files in memory. Handles retain their mount until the last array drops.
pub fn open_zarr_store(files: Vec<(String, Vec<u8>)>) -> Result<Dataset> {
    #[cfg(feature = "zarr")]
    {
        let candidates: std::collections::BTreeSet<_> = files
            .iter()
            .filter_map(|(name, _)| {
                let (parent, file) = name.rsplit_once('/').unwrap_or(("", name));
                matches!(file, ".zgroup" | ".zarray" | ".zmetadata" | "zarr.json").then_some(parent.to_owned())
            })
            .collect();
        let roots: Vec<_> = candidates
            .iter()
            .filter(|candidate| {
                !candidates.iter().any(|parent| {
                    parent != *candidate
                        && (parent.is_empty() || candidate.strip_prefix(parent.as_str()).is_some_and(|tail| tail.starts_with('/')))
                })
            })
            .collect();
        if roots.len() > 1 {
            return Err(bad("in-memory input contains multiple Zarr store roots; provide one store"));
        }
        let prefix = roots.first().map_or("", |s| s.as_str());
        let mount = Arc::new(crate::memfs::Mount::new(files));
        let root = mount.path().join(prefix).to_string_lossy().to_string();
        crate::drivers::zarr::md_open(&root, Some(mount))
    }
    #[cfg(not(feature = "zarr"))]
    {
        let _ = files;
        Err(Error::disabled("zarr"))
    }
}

/// Resolves CF paths without allowing an explicit relative path to fall back to
/// another ancestor. Known unsupported variables still participate in scope.
pub(crate) fn reference_path(owner: &str, name: &str, names: &std::collections::HashSet<String>) -> Option<String> {
    fn normalized(path: &str) -> Option<String> {
        let mut parts = Vec::new();
        for part in path.split('/') {
            match part {
                "" | "." => (),
                ".." => {
                    parts.pop()?;
                }
                _ => parts.push(part),
            }
        }
        Some(format!("/{}", parts.join("/")))
    }
    if name.is_empty() {
        return None;
    }
    let mut group = owner.rsplit_once('/').map_or("/", |(group, _)| if group.is_empty() { "/" } else { group });
    if name.contains('/') {
        let path =
            if name.starts_with('/') { normalized(name)? } else { normalized(&format!("{}/{}", group.trim_end_matches('/'), name))? };
        return names.contains(&path).then_some(path);
    }
    loop {
        let path = format!("{}/{}", group.trim_end_matches('/'), name);
        if names.contains(&path) {
            return Some(path);
        }
        if group == "/" {
            return None;
        }
        group = group.rsplit_once('/').map_or("/", |(parent, _)| if parent.is_empty() { "/" } else { parent });
    }
}

pub(crate) fn coordinate_reference_path(info: &ArrayInfo, name: &str, names: &std::collections::HashSet<String>) -> Option<String> {
    reference_path(&info.path, name, names).or_else(|| {
        // Only a declared NUG coordinate can use a lateral dimension link.
        (!name.contains('/'))
            .then(|| {
                info.dimensions.iter().find_map(|d| {
                    let path = d.indexing_variable.as_ref()?;
                    (d.name == name && path.rsplit('/').next() == Some(name) && names.contains(path)).then(|| path.clone())
                })
            })
            .flatten()
    })
}

#[cfg(any(feature = "netcdf-hdf5", feature = "zarr"))]
pub(crate) fn resolve_coordinates(root: &mut Group) {
    let names: std::collections::HashSet<_> = root.array_paths_recursive().into_iter().map(str::to_string).collect();
    fn visit(group: &mut Group, names: &std::collections::HashSet<String>) {
        for a in &mut group.arrays {
            if let Some(raw) = a.info.attributes.get("coordinates").and_then(Value::as_str) {
                for name in raw.split_whitespace() {
                    let coordinate = coordinate_reference_path(&a.info, name, names);
                    if let Some(c) = coordinate
                        && !a.info.coordinates.contains(&c)
                    {
                        a.info.coordinates.push(c);
                    }
                }
            }
        }
        for child in &mut group.groups {
            visit(child, names);
        }
    }
    visit(root, &names);
}

//! Numeric snapshots through the existing format writers.
use super::*;

fn valid_fill(value: &Value, dtype: DataType) -> bool {
    if matches!(dtype, DataType::F32 | DataType::F64) {
        if matches!(value.as_str(), Some("NaN" | "Infinity" | "-Infinity")) {
            return true;
        }
        return value.as_f64().is_some_and(|v| dtype == DataType::F64 || v.abs() <= f32::MAX as f64);
    }
    if dtype == DataType::I64 && value.as_i64().is_some() {
        return true;
    }
    if dtype == DataType::U64 && value.as_u64().is_some() {
        return true;
    }
    value.as_f64().is_some_and(|v| {
        if !v.is_finite() || v.fract() != 0. {
            return false;
        }
        match dtype {
            DataType::I64 => (i64::MIN as f64..9223372036854775808.).contains(&v),
            DataType::U64 => (0. ..18446744073709551616.).contains(&v),
            _ => {
                let (lo, hi) = crate::ops::type_range(dtype);
                (lo..=hi).contains(&v)
            }
        }
    })
}

pub(super) fn validate(dataset: &Dataset) -> Result<()> {
    fn valid(path: &str) -> bool {
        path == "/"
            || (path.starts_with('/')
                && !path.contains(['\\', '\0'])
                && path[1..].split('/').all(|part| !part.is_empty() && part != "." && part != ".."))
    }
    fn group(g: &Group, groups: &mut std::collections::HashSet<String>, arrays: &mut std::collections::HashSet<String>) -> Result<()> {
        if let Some(a) = g.unsupported_arrays.first() {
            return Err(Error::Unsupported(format!("array '{}' has an unsupported payload: {}", a.path, a.data_type)));
        }
        if !valid(&g.path) || !groups.insert(g.path.clone()) {
            return Err(bad("group paths must be unique absolute names without empty, . or .. components"));
        }
        for a in &g.arrays {
            if let Some(fill) = a.info.attributes.get("_FillValue")
                && !valid_fill(fill, a.info.data_type)
            {
                return Err(bad(format!("array '{}' _FillValue must be numeric and representable in its data type", a.info.path)));
            }
            if !valid(&a.info.path)
                || a.info.path == "/"
                || !arrays.insert(a.info.path.clone())
                || a.info.dimensions.iter().any(|d| !valid(&d.path) || d.path == "/")
            {
                return Err(bad("array and dimension paths must be valid absolute names, and array paths must be unique"));
            }
        }
        for child in &g.groups {
            group(child, groups, arrays)?;
        }
        Ok(())
    }
    group(&dataset.root, &mut std::collections::HashSet::new(), &mut std::collections::HashSet::new())?;
    fn dimensions(g: &Group, known: &mut std::collections::HashMap<String, Dimension>) -> Result<()> {
        for d in g.dimensions.iter().chain(g.arrays.iter().flat_map(|a| &a.info.dimensions)) {
            if !valid(&d.path) || d.path == "/" {
                return Err(bad("dimension paths must be valid absolute names"));
            }
            if let Some(previous) = known.get(&d.path) {
                if previous != d {
                    return Err(bad(format!("conflicting declarations for dimension '{}'", d.path)));
                }
            } else {
                known.insert(d.path.clone(), d.clone());
            }
        }
        for child in &g.groups {
            dimensions(child, known)?;
        }
        Ok(())
    }
    dimensions(&dataset.root, &mut std::collections::HashMap::new())
}

/// Keep inferred chunk buffers below 8 MiB regardless of array rank.
#[cfg(any(feature = "netcdf-hdf5", feature = "zarr"))]
pub(crate) fn default_chunks(shape: &[usize], dtype: DataType) -> Vec<usize> {
    let mut chunks: Vec<_> = shape.iter().map(|&n| n.clamp(1, 256)).collect();
    let budget = (8 * 1024 * 1024) / dtype.size_bytes();
    while chunks.iter().try_fold(1usize, |n, &c| n.checked_mul(c)).is_none_or(|n| n > budget) {
        let (axis, &largest) = chunks.iter().enumerate().max_by_key(|(_, c)| *c).expect("oversized non-scalar chunk");
        chunks[axis] = largest.div_ceil(2);
    }
    chunks
}
pub(super) fn write(dataset: &Dataset, destination: &str) -> Result<()> {
    validate(dataset)?;
    if destination.to_ascii_lowercase().ends_with(".zarr") {
        #[cfg(feature = "zarr")]
        {
            return crate::drivers::zarr::md_write(dataset, destination);
        }
        #[cfg(not(feature = "zarr"))]
        {
            return Err(Error::disabled("zarr"));
        }
    }
    if !destination.to_ascii_lowercase().ends_with(".nc") {
        return Err(bad("snapshot destination must end in .nc or .zarr"));
    }
    #[cfg(feature = "netcdf-hdf5")]
    {
        // Avoid reading source arrays for a destination that already exists.
        // Exclusive persistence below also protects against creation after this check.
        if std::path::Path::new(destination).exists() {
            return Err(bad("destination already exists"));
        }
        let bytes = netcdf_bytes(dataset)?;
        #[cfg(not(target_family = "wasm"))]
        {
            use std::io::Write;
            let path = std::path::Path::new(destination);
            let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| std::path::Path::new("."));
            let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
            temporary.write_all(&bytes)?;
            temporary.persist_noclobber(path).map_err(|e| e.error)?;
        }
        #[cfg(target_family = "wasm")]
        {
            use std::io::Write;
            let mut output = std::fs::OpenOptions::new().write(true).create_new(true).open(destination)?;
            output.write_all(&bytes)?;
        }
        Ok(())
    }
    #[cfg(not(feature = "netcdf-hdf5"))]
    {
        Err(Error::disabled("netcdf-hdf5"))
    }
}
/// Encodes one fill value exactly in the target native numeric type.
#[cfg(any(feature = "netcdf-hdf5", feature = "zarr"))]
pub(crate) fn sample(value: &Value, t: DataType) -> Vec<u8> {
    let f = value
        .as_f64()
        .or_else(|| {
            value.as_str().and_then(|s| match s {
                "NaN" => Some(f64::NAN),
                "Infinity" => Some(f64::INFINITY),
                "-Infinity" => Some(f64::NEG_INFINITY),
                _ => None,
            })
        })
        .unwrap_or(0.0);
    macro_rules! signed {
        ($t:ty) => {
            value.as_i64().map(|n| n as $t).unwrap_or(f as $t).to_le_bytes().to_vec()
        };
    }
    macro_rules! unsigned {
        ($t:ty) => {
            value.as_u64().map(|n| n as $t).unwrap_or(f as $t).to_le_bytes().to_vec()
        };
    }
    match t {
        DataType::I8 => signed!(i8),
        DataType::I16 => signed!(i16),
        DataType::I32 => signed!(i32),
        DataType::I64 => signed!(i64),
        DataType::U8 => unsigned!(u8),
        DataType::U16 => unsigned!(u16),
        DataType::U32 => unsigned!(u32),
        DataType::U64 => unsigned!(u64),
        DataType::F32 => (f as f32).to_le_bytes().to_vec(),
        DataType::F64 => f.to_le_bytes().to_vec(),
    }
}
#[cfg(feature = "netcdf-hdf5")]
pub(super) fn netcdf_bytes(dataset: &Dataset) -> Result<Vec<u8>> {
    use crate::drivers::hdf5_write::{self, Attr, H5Type, Values};
    use std::collections::BTreeMap;
    fn dtype(t: DataType) -> H5Type {
        match t {
            DataType::F32 => H5Type::Float { size: 4 },
            DataType::F64 => H5Type::Float { size: 8 },
            DataType::I8 | DataType::I16 | DataType::I32 | DataType::I64 => H5Type::Int { size: t.size_bytes() as u8, signed: true },
            _ => H5Type::Int { size: t.size_bytes() as u8, signed: false },
        }
    }
    // HDF5's dimension-scale convention uses NUL-terminated CLASS/NAME strings.
    // Rust metadata readers also accept unterminated strings, but netCDF-C does not.
    fn scale_text(name: &str, text: &str) -> Attr {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        Attr { name: name.into(), dtype: H5Type::Str { len: bytes.len() as u32 }, shape: None, values: Values::Bytes(bytes) }
    }
    fn int(name: &str, value: i32) -> Attr {
        Attr {
            name: name.into(),
            dtype: H5Type::Int { size: 4, signed: true },
            shape: None,
            values: Values::Bytes(value.to_le_bytes().to_vec()),
        }
    }
    fn attrs(raw: &Value, types: &AttributeTypes) -> Vec<Attr> {
        let Some(attrs) = raw.as_object() else {
            return Vec::new();
        };
        attrs
            .iter()
            .filter(|(name, _)| {
                !matches!(
                    name.as_str(),
                    "CLASS"
                        | "NAME"
                        | "DIMENSION_LIST"
                        | "REFERENCE_LIST"
                        | "_Netcdf4Coordinates"
                        | "_Netcdf4Dimid"
                        | "_ARRAY_DIMENSIONS"
                        | "_terrio_dimensions"
                        | "_terrio_attribute_types"
                )
            })
            .map(|(name, value)| {
                let (values, shape) = match value {
                    Value::Array(v) => (v.clone(), Some(vec![v.len() as u64])),
                    v => (vec![v.clone()], None),
                };
                if let Some(&native) = types.get(name)
                    && values.iter().all(|v| v.is_number() || matches!(v.as_str(), Some("NaN" | "Infinity" | "-Infinity")))
                {
                    return Attr {
                        name: name.clone(),
                        dtype: dtype(native),
                        shape,
                        values: Values::Bytes(values.iter().flat_map(|v| sample(v, native)).collect()),
                    };
                }
                if values.iter().all(Value::is_i64) {
                    return Attr {
                        name: name.clone(),
                        dtype: H5Type::Int { size: 8, signed: true },
                        shape,
                        values: Values::Bytes(values.iter().flat_map(|v| v.as_i64().unwrap().to_le_bytes()).collect()),
                    };
                }
                if values.iter().all(Value::is_u64) {
                    return Attr {
                        name: name.clone(),
                        dtype: H5Type::Int { size: 8, signed: false },
                        shape,
                        values: Values::Bytes(values.iter().flat_map(|v| v.as_u64().unwrap().to_le_bytes()).collect()),
                    };
                }
                if values.iter().all(Value::is_number) {
                    return Attr {
                        name: name.clone(),
                        dtype: H5Type::Float { size: 8 },
                        shape,
                        values: Values::Bytes(values.iter().flat_map(|v| v.as_f64().unwrap().to_le_bytes()).collect()),
                    };
                }
                if shape.is_none() {
                    return Attr::text(name, value.as_str().map(str::to_owned).unwrap_or_else(|| value.to_string()).as_str());
                }
                let strings: Vec<_> = values.iter().map(|v| v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())).collect();
                let size = strings.iter().map(String::len).max().unwrap_or(0).max(1);
                let mut bytes = Vec::new();
                for text in strings {
                    let begin = bytes.len();
                    bytes.extend_from_slice(text.as_bytes());
                    bytes.resize(begin + size, 0);
                }
                Attr { name: name.clone(), dtype: H5Type::Str { len: size as u32 }, shape, values: Values::Bytes(bytes) }
            })
            .collect()
    }
    let arrays = dataset.root.arrays_recursive();
    let mut dimensions = Vec::new();
    fn dims(group: &Group, out: &mut Vec<Dimension>) {
        for d in &group.dimensions {
            if !out.iter().any(|known| known.path == d.path) {
                out.push(d.clone());
            }
        }
        for child in &group.groups {
            dims(child, out);
        }
    }
    dims(&dataset.root, &mut dimensions);
    for a in &arrays {
        for d in &a.info.dimensions {
            if !dimensions.iter().any(|known| known.path == d.path) {
                dimensions.push(d.clone());
            }
        }
    }
    let mut file = hdf5_write::File { attrs: attrs(&dataset.root.attributes, &dataset.root.attribute_types), ..Default::default() };
    file.attrs.push(Attr::text("_NCProperties", concat!("version=2,terrio=", env!("CARGO_PKG_VERSION"))));
    fn group_attrs(group: &Group, out: &mut Vec<(String, Vec<Attr>)>) {
        for child in &group.groups {
            out.push((child.path.trim_matches('/').into(), attrs(&child.attributes, &child.attribute_types)));
            group_attrs(child, out);
        }
    }
    group_attrs(&dataset.root, &mut file.group_attrs);
    for array in &arrays {
        let shape = array.shape();
        let mut attributes = attrs(&array.info.attributes, &array.attribute_types);
        let fill = sample(&array.info.attributes["_FillValue"], array.info.data_type);
        if array.info.attributes.get("_FillValue").is_some() {
            attributes.retain(|a| a.name != "_FillValue");
            attributes.push(Attr {
                name: "_FillValue".into(),
                dtype: dtype(array.info.data_type),
                shape: None,
                values: Values::Bytes(fill.clone()),
            });
        }
        let unlimited = array.info.dimensions.iter().any(|d| d.unlimited);
        let chunks = array.info.chunks.clone().or_else(|| unlimited.then(|| default_chunks(&shape, array.info.data_type))).map(|chunks| {
            chunks
                .iter()
                .zip(&array.info.dimensions)
                .map(|(&chunk, dimension)| if dimension.unlimited { chunk } else { chunk.min(dimension.size.max(1)) })
                .collect::<Vec<_>>()
        });
        file.datasets.push(hdf5_write::Dataset {
            name: array.info.path.trim_start_matches('/').into(),
            dtype: dtype(array.info.data_type),
            shape: (!shape.is_empty()).then(|| shape.iter().map(|&n| n as u64).collect()),
            max_shape: unlimited
                .then(|| array.info.dimensions.iter().map(|d| if d.unlimited { u64::MAX } else { d.size as u64 }).collect()),
            data: array.read()?.bytes,
            fill,
            storage: hdf5_write::Storage { chunks: chunks.map(|c| c.into_iter().map(|n| n as u64).collect()), deflate: None },
            attrs: attributes,
        });
    }
    let mut scales = BTreeMap::new();
    for (id, d) in dimensions.iter().enumerate() {
        if let Some(name) = &d.indexing_variable {
            let coordinate = arrays
                .iter()
                .find(|a| &a.info.path == name)
                .ok_or_else(|| bad(format!("dimension '{}' has a missing indexing variable '{name}'", d.path)))?;
            if coordinate.info.dimensions.len() != 1 || coordinate.info.dimensions[0].path != d.path || coordinate.shape() != [d.size] {
                return Err(bad(format!("indexing variable '{name}' must use its own dimension '{}'", d.path)));
            }
        }
        let scale = match d.indexing_variable.as_ref().and_then(|p| arrays.iter().position(|a| &a.info.path == p && a.shape() == [d.size]))
        {
            Some(index) => index,
            None => {
                if file.datasets.iter().any(|a| a.name == d.path.trim_start_matches('/')) {
                    return Err(bad(format!("dimension scale '{}' conflicts with an array", d.path)));
                }
                let index = file.datasets.len();
                file.datasets.push(hdf5_write::Dataset {
                    name: d.path.trim_start_matches('/').into(),
                    dtype: H5Type::Int { size: 4, signed: true },
                    shape: Some(vec![d.size as u64]),
                    max_shape: d.unlimited.then_some(vec![u64::MAX]),
                    data: vec![0; d.size.checked_mul(4).ok_or_else(|| bad("dimension scale byte length overflows"))?],
                    fill: vec![0; 4],
                    storage: hdf5_write::Storage { chunks: d.unlimited.then_some(vec![d.size.clamp(1, 256) as u64]), deflate: None },
                    attrs: vec![scale_text("NAME", &format!("This is a netCDF dimension but not a netCDF variable. {}", d.size))],
                });
                index
            }
        };
        let a = &mut file.datasets[scale];
        a.attrs.push(scale_text("CLASS", "DIMENSION_SCALE"));
        if !a.attrs.iter().any(|a| a.name == "NAME") {
            a.attrs.push(scale_text("NAME", &d.name));
        }
        a.attrs.push(int("_Netcdf4Dimid", id as i32));
        scales.insert(d.path.clone(), scale);
    }
    for (i, array) in arrays.iter().enumerate() {
        if scales.values().any(|&s| s == i) {
            continue;
        }
        let indices: Vec<_> = array.info.dimensions.iter().map(|d| scales[&d.path]).collect();
        if !indices.is_empty() {
            file.datasets[i].attrs.push(Attr {
                name: "DIMENSION_LIST".into(),
                dtype: H5Type::RefVlen,
                shape: Some(vec![indices.len() as u64]),
                values: Values::DimensionList(indices),
            });
        }
        let ids: Vec<i32> =
            array.info.dimensions.iter().map(|d| dimensions.iter().position(|v| v.path == d.path).unwrap() as i32).collect();
        if !ids.is_empty() {
            file.datasets[i].attrs.push(Attr {
                name: "_Netcdf4Coordinates".into(),
                dtype: H5Type::Int { size: 4, signed: true },
                shape: Some(vec![ids.len() as u64]),
                values: Values::Bytes(ids.iter().flat_map(|n| n.to_le_bytes()).collect()),
            });
        }
    }
    for (dimension, &scale) in &scales {
        let references: Vec<_> = arrays
            .iter()
            .enumerate()
            .filter(|(i, _)| !scales.values().any(|s| s == i))
            .flat_map(|(i, a)| {
                a.info.dimensions.iter().enumerate().filter(|(_, d)| d.path == *dimension).map(move |(axis, _)| (i, axis as u32))
            })
            .collect();
        if !references.is_empty() {
            file.datasets[scale].attrs.push(Attr {
                name: "REFERENCE_LIST".into(),
                dtype: H5Type::RefList,
                shape: Some(vec![references.len() as u64]),
                values: Values::ReferenceList(references),
            });
        }
    }
    Ok(hdf5_write::encode(&file))
}

#[cfg(all(test, any(feature = "netcdf-hdf5", feature = "zarr")))]
mod tests {
    #[test]
    fn inferred_chunks_bound_bytes_even_for_high_rank_shapes() {
        for shape in [vec![300; 4], vec![256; 3], vec![usize::MAX; 16], vec![], vec![0, 1]] {
            let chunks = super::default_chunks(&shape, super::DataType::F64);
            assert_eq!(chunks.len(), shape.len());
            assert!(chunks.iter().all(|&n| n > 0));
            assert!(chunks.iter().product::<usize>() * 8 <= 8 * 1024 * 1024);
        }
    }
}

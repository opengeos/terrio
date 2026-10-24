//! Numeric snapshots through the existing format writers.
use super::*;

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
    group(&dataset.root, &mut std::collections::HashSet::new(), &mut std::collections::HashSet::new())
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
        std::fs::write(destination, bytes)?;
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
    use std::collections::HashMap;
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
    fn attrs(raw: &Value) -> Vec<Attr> {
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
                )
            })
            .map(|(name, value)| {
                let (values, shape) = match value {
                    Value::Array(v) => (v.clone(), Some(vec![v.len() as u64])),
                    v => (vec![v.clone()], None),
                };
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
    let mut file = hdf5_write::File { attrs: attrs(&dataset.root.attributes), ..Default::default() };
    file.attrs.push(Attr::text("_NCProperties", concat!("version=2,terrio=", env!("CARGO_PKG_VERSION"))));
    fn group_attrs(group: &Group, out: &mut Vec<(String, Vec<Attr>)>) {
        for child in &group.groups {
            out.push((child.path.trim_matches('/').into(), attrs(&child.attributes)));
            group_attrs(child, out);
        }
    }
    group_attrs(&dataset.root, &mut file.group_attrs);
    for array in &arrays {
        let shape = array.shape();
        let mut attributes = attrs(&array.info.attributes);
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
        let chunks =
            array.info.chunks.clone().or_else(|| unlimited.then(|| shape.iter().map(|&n| n.clamp(1, 256)).collect())).map(|chunks| {
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
    let mut scales = HashMap::new();
    for (id, d) in dimensions.iter().enumerate() {
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
            .filter(|(i, _)| *i != scale)
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

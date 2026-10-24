//! Native multidimensional adapter over the shared Zarr codecs and store.
use super::*;

/// A native multidimensional source retaining an optional in-memory store mount.
pub(crate) struct MdSource {
    array: Array,
    _mount: Option<std::sync::Arc<crate::memfs::Mount>>,
}
impl MdSource {
    pub(crate) fn read(&self, selection: &[crate::multidim::Selection]) -> Result<Vec<u8>> {
        let start: Vec<_> = selection.iter().map(|s| s.start).collect();
        let shape: Vec<_> = selection.iter().map(|s| (s.count - 1) * s.step + 1).collect();
        let raw = read_region(&self.array, &start, &shape)?;
        if selection.iter().all(|s| s.step == 1) {
            return Ok(raw);
        }
        let relative: Vec<_> = selection.iter().map(|s| crate::multidim::Selection { start: 0, ..s.clone() }).collect();
        crate::multidim::select_bytes(&raw, &shape, &relative, self.array.data_type.size_bytes())
    }
}
/// Opens groups and native arrays without reading their chunks.
pub(crate) fn md_open(name: &str, mount: Option<std::sync::Arc<crate::memfs::Mount>>) -> Result<crate::multidim::Dataset> {
    use crate::multidim::{ArrayInfo, Dimension, Group, Source};
    let (store, selected) = parse_name(name).unwrap_or_else(|| (name.to_string(), None));
    let node = open_name(&store, selected.as_deref())?;
    fn unsupported(node: &Node, path: String, reason: Error) -> Result<crate::multidim::UnsupportedArray> {
        let metadata = node.json(".zarray")?.or(node.json("zarr.json")?).unwrap_or(Value::Null);
        let dtype = metadata.get("dtype").or(metadata.get("data_type")).cloned().unwrap_or(Value::Null);
        let shape = metadata["shape"]
            .as_array()
            .map(|s| {
                s.iter()
                    .filter_map(Value::as_u64)
                    .map(|n| usize::try_from(n).map_err(|_| bad("array dimension exceeds this platform")))
                    .collect::<Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default();
        Ok(crate::multidim::UnsupportedArray { path, data_type: format!("{dtype}: {reason}"), shape })
    }
    fn visit(node: &Node, p: &str, mount: &Option<std::sync::Arc<crate::memfs::Mount>>, depth: usize) -> Result<Group> {
        if depth > 64 {
            return Err(bad("multidimensional group nesting exceeds 64"));
        }
        let mut group = Group::new(p);
        group.attributes = group_attrs(node);
        if !group.attributes.is_object() {
            group.attributes = json!({});
        }
        let standalone = match read_array_opt(node) {
            Ok(array) => array.is_some(),
            Err(e @ Error::Unsupported(_)) => {
                group.unsupported_arrays.push(unsupported(node, format!("{}/array", p.trim_end_matches('/')), e)?);
                return Ok(group);
            }
            Err(e) => return Err(e),
        };
        let arrays: Vec<_> = if standalone {
            vec![("array".into(), read_array(node)?)]
        } else {
            let mut arrays = Vec::new();
            for name in node.children() {
                let child = node.join(&name);
                match read_array_opt(&child) {
                    Ok(Some(array)) => arrays.push((name, array)),
                    Ok(None) if is_group(&child) => {
                        group.groups.push(visit(&child, &format!("{}/{}", p.trim_end_matches('/'), name), mount, depth + 1)?)
                    }
                    Ok(None) => {}
                    Err(e @ Error::Unsupported(_)) => {
                        group.unsupported_arrays.push(unsupported(&child, format!("{}/{}", p.trim_end_matches('/'), name), e)?)
                    }
                    Err(e) => return Err(e),
                }
            }
            arrays
        };
        for (name, a) in &arrays {
            let dims: Vec<_> = if a.dims.len() == a.shape.len() {
                a.dims.iter().enumerate().map(|(i, n)| if n.is_empty() { format!("{name}_dim{i}") } else { n.clone() }).collect()
            } else {
                (0..a.shape.len()).map(|i| format!("{name}_dim{i}")).collect()
            };
            for (i, dimension) in dims.iter().enumerate() {
                if let Some(d) = group.dimensions.iter().find(|d| d.name == *dimension) {
                    if d.size != a.shape[i] {
                        return Err(bad(format!("dimension '{dimension}' has conflicting lengths")));
                    }
                    continue;
                }
                let indexing_variable = arrays
                    .iter()
                    .any(|(n, c)| {
                        n == dimension
                            && c.shape == [a.shape[i]]
                            && (c.dims.is_empty() || c.dims.as_slice() == std::slice::from_ref(dimension))
                    })
                    .then(|| format!("{}/{}", p.trim_end_matches('/'), dimension));
                group.dimensions.push(Dimension {
                    name: dimension.clone(),
                    path: format!("{}/{}", p.trim_end_matches('/'), dimension),
                    size: a.shape[i],
                    unlimited: false,
                    indexing_variable,
                });
            }
        }
        for (name, a) in arrays {
            let dims: Vec<_> = if a.dims.len() == a.shape.len() {
                a.dims.iter().enumerate().map(|(i, n)| if n.is_empty() { format!("{name}_dim{i}") } else { n.clone() }).collect()
            } else {
                (0..a.shape.len()).map(|i| format!("{name}_dim{i}")).collect()
            };
            let mut dimensions: Vec<_> = dims.iter().map(|n| group.dimensions.iter().find(|d| d.name == *n).unwrap().clone()).collect();
            if let Some(saved) = a.attrs.get("_terrio_dimensions") {
                dimensions = serde_json::from_value(saved.clone()).map_err(|e| bad(e.to_string()))?;
                if dimensions.len() != a.shape.len() || dimensions.iter().zip(&a.shape).any(|(d, &s)| d.size != s) {
                    return Err(bad("saved dimension metadata does not match the array shape"));
                }
            }
            let mut coordinates: Vec<_> = dimensions.iter().filter_map(|d| d.indexing_variable.clone()).collect();
            if let Some(raw) = a.attrs.get("coordinates").and_then(Value::as_str) {
                for n in raw.split_whitespace() {
                    let n = if n.starts_with('/') { n.to_string() } else { format!("{}/{}", p.trim_end_matches('/'), n) };
                    if !coordinates.contains(&n) {
                        coordinates.push(n);
                    }
                }
            }
            let info = ArrayInfo {
                path: format!("{}/{}", p.trim_end_matches('/'), name),
                name,
                data_type: a.data_type,
                dimensions,
                chunks: Some(a.chunks.clone()),
                attributes: if a.attrs.is_object() { a.attrs.clone() } else { json!({}) },
                coordinates,
            };
            group.arrays.push(crate::multidim::Array {
                info,
                source: Source::Zarr(std::sync::Arc::new(MdSource { array: a, _mount: mount.clone() })),
            });
        }
        if let Some(saved) = group.attributes.get("_terrio_dimensions") {
            group.dimensions = serde_json::from_value(saved.clone()).map_err(|e| bad(e.to_string()))?;
        }
        Ok(group)
    }
    Ok(crate::multidim::Dataset { driver: "Zarr".into(), root: visit(&node, "/", &mount, 0)? })
}

/// Writes a numeric snapshot a source chunk at a time, preserving coordinate and dimension identity.
pub(crate) fn md_write(dataset: &crate::multidim::Dataset, destination: &str) -> Result<()> {
    let root = Path::new(destination);
    if crate::memfs::exists(root) {
        return Err(bad("destination already exists"));
    }
    #[cfg(not(target_family = "wasm"))]
    let staging = if crate::memfs::is_virtual(root) {
        None
    } else {
        let parent = root.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        Some(tempfile::Builder::new().prefix(".terrio-md-").tempdir_in(parent)?)
    };
    #[cfg(not(target_family = "wasm"))]
    let output_root = staging.as_ref().map_or(root, |directory| directory.path());
    #[cfg(target_family = "wasm")]
    let output_root = root;
    let mut metadata = serde_json::Map::new();
    fn group(g: &crate::multidim::Group, root: &Path, metadata: &mut serde_json::Map<String, Value>) -> Result<()> {
        let prefix = g.path.trim_matches('/');
        let directory = root.join(prefix);
        let mut group_attributes = g.attributes.clone();
        if !group_attributes.is_object() {
            group_attributes = json!({});
        }
        group_attributes["_terrio_dimensions"] = serde_json::to_value(&g.dimensions).map_err(|e| bad(e.to_string()))?;
        write_group(&directory, &group_attributes, Version::V2, metadata, prefix)?;
        for array in &g.arrays {
            let shape = array.shape();
            let chunks = array.info.chunks.clone().unwrap_or_else(|| shape.iter().map(|&n| n.clamp(1, 256)).collect());
            if chunks.len() != shape.len() || chunks.contains(&0) {
                return Err(bad("chunk shape must have one positive size per dimension"));
            }
            let mut attrs = array.info.attributes.clone();
            if !attrs.is_object() {
                attrs = json!({});
            }
            attrs["_terrio_dimensions"] = serde_json::to_value(&array.info.dimensions).map_err(|e| bad(e.to_string()))?;
            attrs["_ARRAY_DIMENSIONS"] = Value::Array(array.info.dimensions.iter().map(|d| Value::String(d.name.clone())).collect());
            let fill = attrs.get("_FillValue").cloned().unwrap_or(Value::Null);
            let description = json!({"zarr_format":2,"shape":shape,"chunks":chunks,"dtype":dtype_v2(array.info.data_type),"compressor":null,"fill_value":fill,"order":"C","filters":null});
            let relative = array.info.path.trim_start_matches('/');
            let directory = root.join(relative);
            crate::memfs::create_dir_all(&directory)?;
            for (file, value) in [(".zarray", description), (".zattrs", attrs)] {
                crate::memfs::write(&directory.join(file), serde_json::to_vec(&value).map_err(|e| bad(e.to_string()))?)?;
                metadata.insert(format!("{relative}/{file}"), value);
            }
            let grid: Vec<_> = shape.iter().zip(&chunks).map(|(&s, &c)| s.div_ceil(c)).collect();
            let total = grid.iter().try_fold(1usize, |n, &c| n.checked_mul(c).ok_or_else(|| bad("chunk grid size overflows")))?;
            let chunk_elements =
                chunks.iter().try_fold(1usize, |n, &c| n.checked_mul(c).ok_or_else(|| bad("chunk byte length overflows")))?;
            let fill = crate::multidim::write::sample(&fill, array.info.data_type);
            for flat in 0..total {
                let index = unravel(flat, &grid);
                let start: Vec<_> = index.iter().zip(&chunks).map(|(&i, &c)| i * c).collect();
                let valid: Vec<_> = shape.iter().zip(&start).zip(&chunks).map(|((&s, &o), &c)| (s - o).min(c)).collect();
                let mut view = array.view();
                for (axis, (&origin, &count)) in start.iter().zip(&valid).enumerate() {
                    view = view.slice(&array.info.dimensions[axis].path, origin, count, 1)?;
                }
                let buffer = view.read()?;
                let mut chunk = fill.repeat(chunk_elements);
                let zero = vec![0; shape.len()];
                scatter(&buffer.bytes, &zero, &valid, &mut chunk, &zero, &chunks, array.info.data_type.size_bytes());
                let key = if index.is_empty() { "0".into() } else { index.iter().map(usize::to_string).collect::<Vec<_>>().join(".") };
                crate::memfs::write(&directory.join(key), chunk)?;
            }
        }
        for child in &g.groups {
            group(child, root, metadata)?;
        }
        Ok(())
    }
    group(&dataset.root, output_root, &mut metadata)?;
    crate::memfs::write(
        &output_root.join(".zmetadata"),
        serde_json::to_vec(&json!({"zarr_consolidated_format":1,"metadata":metadata})).map_err(|e| bad(e.to_string()))?,
    )?;
    #[cfg(not(target_family = "wasm"))]
    if let Some(staging) = staging {
        // Claim the new destination exclusively after every source chunk was read.
        // A failed source read leaves only the private staging directory, removed on drop.
        std::fs::create_dir(root)?;
        let publish = (|| -> std::io::Result<()> {
            for entry in std::fs::read_dir(staging.path())? {
                let entry = entry?;
                std::fs::rename(entry.path(), root.join(entry.file_name()))?;
            }
            Ok(())
        })();
        if let Err(error) = publish {
            let _ = std::fs::remove_dir_all(root);
            return Err(error.into());
        }
    }
    Ok(())
}

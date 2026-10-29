//! Coordinate-aligned, lazy mosaics of numeric scientific arrays.
use super::*;
mod references;
use std::collections::{BTreeMap, BTreeSet};

/// Arrays to mosaic, by absolute path or unambiguous local name.
/// Empty selects arrays with at least two dimensions, like GDAL.
#[derive(Debug, Clone, Default)]
pub struct MosaicOptions {
    /// Array selection; dimension indexing variables are included automatically.
    pub arrays: Vec<String>,
}

#[derive(Clone, Copy, Debug)]
enum Coordinate {
    Integer(i128),
    Float(f64),
}
impl Coordinate {
    fn distance(self, origin: Self) -> f64 {
        match (self, origin) {
            (Self::Integer(a), Self::Integer(b)) => (a - b) as f64,
            (Self::Float(a), Self::Float(b)) => a - b,
            _ => unreachable!("coordinate types checked"),
        }
    }
    fn advance(self, step: f64, integer_step: Option<i128>, index: usize) -> Result<Self> {
        let delta = step * index as f64;
        match self {
            Self::Integer(value) => {
                let delta = integer_step
                    .ok_or_else(|| bad("integer indexing variables require an integral grid"))?
                    .checked_mul(index as i128)
                    .ok_or_else(|| bad("coordinate overflow"))?;
                Ok(Self::Integer(value.checked_add(delta).ok_or_else(|| bad("coordinate overflow"))?))
            }
            Self::Float(value) => Ok(Self::Float(value + delta)),
        }
    }
}
// Relative step jitter beyond floating-point roundoff.
const STEP_RELATIVE_TOLERANCE: f64 = 1e-7;
// Four storage ULPs cover rounding in endpoint subtraction and step estimation.
const COORDINATE_ULPS: f64 = 4.;
// Alignment is measured in cells; accumulated drift is capped at 0.1% of a cell.
const ALIGNMENT_BASE_CELLS: f64 = 1e-6;
const MAX_ALIGNMENT_CELLS: f64 = 1e-3;
const MAX_COORDINATE_BYTES: usize = 256 * 1024 * 1024;
fn check_coordinate_size(count: usize) -> Result<()> {
    if count.checked_mul(std::mem::size_of::<Coordinate>()).is_none_or(|bytes| bytes > MAX_COORDINATE_BYTES) {
        return Err(bad("mosaic coordinate buffer exceeds 256 MiB"));
    }
    Ok(())
}
fn coordinates(array: &Array) -> Result<Vec<Coordinate>> {
    check_coordinate_size(elements(&array.shape())?)?;
    let data = array.read()?.data()?;
    use RasterData::*;
    macro_rules! integers {
        ($values:expr) => {
            $values.into_iter().map(|v| Coordinate::Integer(v as i128)).collect()
        };
    }
    let values: Vec<_> = match data {
        U8(v) => integers!(v),
        I8(v) => integers!(v),
        U16(v) => integers!(v),
        I16(v) => integers!(v),
        U32(v) => integers!(v),
        I32(v) => integers!(v),
        U64(v) => integers!(v),
        I64(v) => integers!(v),
        F32(v) => v.into_iter().map(|v| Coordinate::Float(v as f64)).collect(),
        F64(v) => v.into_iter().map(Coordinate::Float).collect(),
    };
    if values.is_empty() || values.iter().any(|v| matches!(v, Coordinate::Float(x) if !x.is_finite())) {
        return Err(bad("indexing variables must be nonempty and finite"));
    }
    Ok(values)
}
fn encode_coordinates(values: &[Coordinate], dtype: DataType) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for value in values {
        macro_rules! integer {
            ($typ:ty) => {{
                let Coordinate::Integer(v) = value else {
                    return Err(bad("indexing variable type changed"));
                };
                bytes.extend_from_slice(&<$typ>::try_from(*v).map_err(|_| bad("coordinate exceeds its type"))?.to_le_bytes());
            }};
        }
        match dtype {
            DataType::U8 => integer!(u8),
            DataType::I8 => integer!(i8),
            DataType::U16 => integer!(u16),
            DataType::I16 => integer!(i16),
            DataType::U32 => integer!(u32),
            DataType::I32 => integer!(i32),
            DataType::U64 => integer!(u64),
            DataType::I64 => integer!(i64),
            DataType::F32 => {
                let Coordinate::Float(v) = value else {
                    return Err(bad("indexing variable type changed"));
                };
                bytes.extend_from_slice(&(*v as f32).to_le_bytes());
            }
            DataType::F64 => {
                let Coordinate::Float(v) = value else {
                    return Err(bad("indexing variable type changed"));
                };
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        }
    }
    Ok(bytes)
}
fn regular_float_step(a: Coordinate, b: Coordinate, step: f64, epsilon: f64) -> bool {
    let magnitude = match (a, b) {
        (Coordinate::Float(a), Coordinate::Float(b)) => a.abs().max(b.abs()),
        _ => 0.,
    };
    (a.distance(b) - step).abs() <= STEP_RELATIVE_TOLERANCE * step.abs() + COORDINATE_ULPS * epsilon * magnitude
}
struct Axis {
    dimension: Dimension,
    coordinate: Option<Array>,
    starts: BTreeMap<usize, usize>,
}
fn axis(inputs: &[(usize, Dimension, Option<Array>)]) -> Result<Axis> {
    let (_, dimension, template) = &inputs[0];
    let Some(template) = template.as_ref() else {
        if inputs.iter().any(|(_, d, a)| a.is_some() || d.size != dimension.size) {
            return Err(bad(format!("unindexed dimension '{}' must have the same size in every input", dimension.path)));
        }
        return Ok(Axis { dimension: dimension.clone(), coordinate: None, starts: inputs.iter().map(|(id, _, _)| (*id, 0)).collect() });
    };
    let mut vectors = Vec::new();
    let mut input_count = 0usize;
    for (_, d, array) in inputs {
        input_count = input_count.checked_add(d.size).ok_or_else(|| bad("coordinate size overflows"))?;
        check_coordinate_size(input_count)?;
        let array = array.as_ref().ok_or_else(|| bad(format!("indexing variable missing in one input for '{}'", d.path)))?;
        if array.info.dimensions.len() != 1
            || array.info.dimensions[0].path != d.path
            || array.shape() != [d.size]
            || array.info.data_type != template.info.data_type
            || !same_coordinate_metadata(array, template)
            || d.indexing_variable != dimension.indexing_variable
        {
            return Err(bad(format!("incompatible indexing variable for '{}'", d.path)));
        }
        vectors.push(coordinates(array)?);
    }
    // A multi-sample source determines grid spacing/direction. Singleton temporal
    // slices can instead have irregular coordinates when all inputs are singleton.
    // Mixed singletons must align to the spacing of a multi-sample series.
    let step = vectors.iter().find(|v| v.len() > 1).map(|v| v[1].distance(v[0]));
    let integer_step = vectors.iter().find(|v| v.len() > 1).and_then(|v| match (v[1], v[0]) {
        (Coordinate::Integer(a), Coordinate::Integer(b)) => Some(a - b),
        _ => None,
    });
    let epsilon = if template.info.data_type == DataType::F32 { f32::EPSILON as f64 } else { f64::EPSILON };
    let mut starts = BTreeMap::new();
    let identical = vectors.iter().all(|v| v.len() == vectors[0].len() && v.iter().zip(&vectors[0]).all(|(a, b)| a.distance(*b) == 0.));
    let regular = step.filter(|step| {
        *step != 0.
            && vectors.iter().all(|v| {
                v.windows(2).all(|w| match (w[1], w[0], integer_step) {
                    (Coordinate::Integer(a), Coordinate::Integer(b), Some(delta)) => a - b == delta,
                    _ => regular_float_step(w[1], w[0], *step, epsilon),
                })
            })
    });
    let output = if identical {
        let direction = step.unwrap_or(1.).signum();
        if vectors[0].windows(2).any(|w| w[1].distance(w[0]) * direction <= 0.) {
            return Err(bad("indexing variables must be strictly monotonic in one direction"));
        }
        for (dataset, _, _) in inputs {
            starts.insert(*dataset, 0);
        }
        vectors[0].clone()
    } else if let Some(step) = regular {
        let deviation = if integer_step.is_none() {
            vectors.iter().flat_map(|v| v.windows(2)).map(|w| (w[1].distance(w[0]) - step).abs()).fold(0., f64::max)
        } else {
            0.
        };
        if deviation > step.abs() * MAX_ALIGNMENT_CELLS {
            return Err(bad("coordinate precision is insufficient to identify a regular grid"));
        }
        let mut origin = vectors[0][0];
        for v in &vectors {
            if v[0].distance(origin) / step < 0. {
                origin = v[0];
            }
        }
        let mut length = 0usize;
        for ((dataset, _, _), v) in inputs.iter().zip(&vectors) {
            let start = match (v[0], origin, integer_step) {
                (Coordinate::Integer(a), Coordinate::Integer(b), Some(delta)) => {
                    if (a - b) % delta != 0 {
                        return Err(bad("indexing variables are not aligned on a common grid"));
                    }
                    usize::try_from((a - b) / delta).map_err(|_| bad("mosaic axis overflows"))?
                }
                _ => {
                    let start = v[0].distance(origin) / step;
                    let magnitude = match (v[0], origin) {
                        (Coordinate::Float(a), Coordinate::Float(b)) => a.abs().max(b.abs()),
                        _ => 0.,
                    };
                    // First-step rounding accumulates over distant tile origins.
                    // Use the observed spacing variation as well as the native
                    // coordinate precision, but never accept a half-cell shift.
                    let tolerance =
                        (ALIGNMENT_BASE_CELLS + COORDINATE_ULPS * epsilon * magnitude / step.abs() + deviation / step.abs() * start.abs())
                            .min(MAX_ALIGNMENT_CELLS);
                    if !start.is_finite() || start < 0. || start > usize::MAX as f64 || (start - start.round()).abs() > tolerance {
                        return Err(bad("indexing variables are not aligned on a common grid"));
                    }
                    start.round() as usize
                }
            };
            length = length.max(start.checked_add(v.len()).ok_or_else(|| bad("mosaic axis overflows"))?);
            starts.insert(*dataset, start);
        }
        // Coordinate metadata is materialized, data arrays stay lazy. Bound the
        // coordinate allocation before generating a grid with a huge empty gap.
        check_coordinate_size(length)?;
        let mut output = (0..length).map(|i| origin.advance(step, integer_step, i)).collect::<Result<Vec<_>>>()?;
        // Placement tolerates storage rounding; retain the actual source coordinates.
        // Only gap cells use the generated grid, and overlapping coordinates follow
        // the same last-source-wins rule as the payload arrays.
        for ((dataset, _, _), values) in inputs.iter().zip(&vectors) {
            let start = starts[dataset];
            output[start..start + values.len()].copy_from_slice(values);
        }
        output
    } else {
        let direction = step.unwrap_or(1.).signum();
        if vectors.iter().any(|v| v.windows(2).any(|w| w[1].distance(w[0]) * direction <= 0.)) {
            return Err(bad("indexing variables must be strictly monotonic in one direction"));
        }
        let mut values: Vec<_> = vectors.iter().flatten().copied().collect();
        values.sort_by(|a, b| (a.distance(*b) * direction).partial_cmp(&0.).expect("finite coordinate difference"));
        values.dedup_by(|a, b| a.distance(*b) == 0.);
        let mut ranges = Vec::new();
        for ((dataset, _, _), v) in inputs.iter().zip(&vectors) {
            let start = values
                .binary_search_by(|c| (c.distance(v[0]) * direction).partial_cmp(&0.).expect("finite coordinate difference"))
                .expect("source coordinate in union");
            if !values[start..].iter().zip(v).all(|(a, b)| a.distance(*b) == 0.) {
                return Err(bad("irregular coordinate ranges overlap or interleave"));
            }
            let end = start + v.len();
            if ranges.iter().any(|&(lo, hi)| start < hi && lo < end && (lo != start || hi != end)) {
                return Err(bad("overlaps require regularly spaced coordinates"));
            }
            ranges.push((start, end));
            starts.insert(*dataset, start);
        }
        values
    };
    let mut dimension = dimension.clone();
    dimension.size = output.len();
    let mut info = template.info.clone();
    info.dimensions = vec![dimension.clone()];
    // A per-tile range describes only that tile, not the mosaic. Remove it
    // rather than copying stale coordinate extrema to the output.
    if let Some(attributes) = info.attributes.as_object_mut() {
        attributes.remove("actual_range");
    }
    let mut coordinate = Array::from_bytes(info, encode_coordinates(&output, template.info.data_type)?)?;
    coordinate.attribute_types.clone_from(&template.attribute_types);
    Ok(Axis { dimension, coordinate: Some(coordinate), starts })
}

#[derive(Clone)]
struct Piece {
    array: Array,
    starts: Vec<usize>,
}
#[derive(Clone)]
pub(crate) struct MosaicSource {
    pieces: Vec<Piece>,
    fill: Vec<u8>,
}
impl MosaicSource {
    pub(super) fn read(&self, selection: &[Selection]) -> Result<Vec<u8>> {
        let shape: Vec<_> = selection.iter().map(|s| s.count).collect();
        let size = self.fill.len();
        let n = elements(&shape)?;
        n.checked_mul(size).ok_or_else(|| bad("mosaic byte length overflows"))?;
        let mut output = self.fill.repeat(n);
        let output_strides = strides(&shape)?;
        for piece in &self.pieces {
            let mut view = piece.array.view();
            let mut origins = Vec::new();
            let mut counts = Vec::new();
            let mut empty = false;
            for (i, ((s, &start), d)) in selection.iter().zip(&piece.starts).zip(&piece.array.info.dimensions).enumerate() {
                let lower = start.saturating_sub(s.start).div_ceil(s.step).min(s.count);
                let end = start.checked_add(d.size).ok_or_else(|| bad("source extent overflows"))?;
                let upper = end.saturating_sub(s.start).div_ceil(s.step).min(s.count);
                if upper <= lower {
                    empty = true;
                    break;
                }
                view = view.slice_axis(i, s.start + lower * s.step - start, upper - lower, s.step)?;
                origins.push(lower);
                counts.push(upper - lower);
            }
            if empty {
                continue;
            }
            let buffer = view.read()?;
            // A selected source hyperslab is contiguous in its returned buffer,
            // including strided views. Copy its innermost rows as byte runs.
            let run = counts.last().copied().unwrap_or(1);
            for flat in (0..elements(&counts)?).step_by(run) {
                let mut remainder = flat;
                let mut target = 0;
                for axis in (0..counts.len()).rev() {
                    target += (origins[axis] + remainder % counts[axis]) * output_strides[axis];
                    remainder /= counts[axis];
                }
                output[target * size..(target + run) * size].copy_from_slice(&buffer.bytes[flat * size..(flat + run) * size]);
            }
        }
        Ok(output)
    }
}
fn fill(array: &Array) -> Result<Vec<u8>> {
    let v = array
        .info
        .attributes
        .get("_FillValue")
        .or_else(|| array.info.attributes.get("missing_value").filter(|v| !v.is_array()))
        .cloned()
        .unwrap_or(serde_json::json!(0));
    let float = || {
        v.as_f64()
            .or_else(|| match v.as_str() {
                Some("NaN") => Some(f64::NAN),
                Some("Infinity") => Some(f64::INFINITY),
                Some("-Infinity") => Some(f64::NEG_INFINITY),
                _ => None,
            })
            .ok_or_else(|| bad("invalid nodata value"))
    };
    macro_rules! integer {
        ($typ:ty) => {{
            let value = v
                .as_i64()
                .map(i128::from)
                .or_else(|| v.as_u64().map(i128::from))
                .or_else(|| {
                    v.as_f64()
                        .filter(|f| f.is_finite() && f.fract() == 0. && *f >= i64::MIN as f64 && *f < 18_446_744_073_709_551_616.)
                        .map(|f| f as i128)
                })
                .ok_or_else(|| bad("invalid integer nodata value"))?;
            <$typ>::try_from(value).map_err(|_| bad("nodata value exceeds its type"))?.to_le_bytes().to_vec()
        }};
    }
    Ok(match array.info.data_type {
        DataType::U8 => integer!(u8),
        DataType::I8 => integer!(i8),
        DataType::U16 => integer!(u16),
        DataType::I16 => integer!(i16),
        DataType::U32 => integer!(u32),
        DataType::I32 => integer!(i32),
        DataType::U64 => integer!(u64),
        DataType::I64 => integer!(i64),
        DataType::F32 => (float()? as f32).to_le_bytes().to_vec(),
        DataType::F64 => float()?.to_le_bytes().to_vec(),
    })
}
fn group_mut<'a>(root: &'a mut Group, path: &str, datasets: &[Dataset]) -> &'a mut Group {
    if path == "/" {
        return root;
    }
    let (parent, _) = path.rsplit_once('/').unwrap();
    let parent = if parent.is_empty() { "/" } else { parent };
    let root = group_mut(root, parent, datasets);
    let index = root.groups.iter().position(|g| g.path == path).unwrap_or_else(|| {
        fn find<'a>(g: &'a Group, path: &str) -> Option<&'a Group> {
            if g.path == path { Some(g) } else { g.groups.iter().find_map(|c| find(c, path)) }
        }
        let mut group = Group::new(path);
        if let Some(template) = datasets.iter().find_map(|d| find(&d.root, path)) {
            group.attributes.clone_from(&template.attributes);
            group.attribute_types.clone_from(&template.attribute_types);
        }
        root.groups.push(group);
        root.groups.len() - 1
    });
    &mut root.groups[index]
}
fn same_number(a: &Value, b: &Value) -> bool {
    let integer = |v: &Value| v.as_i64().map(i128::from).or_else(|| v.as_u64().map(i128::from));
    match (integer(a), integer(b)) {
        (Some(a), Some(b)) => a == b,
        (Some(a), None) => b.as_f64().is_some_and(|b| b.is_finite() && b.fract() == 0. && b as i128 == a),
        (None, Some(b)) => a.as_f64().is_some_and(|a| a.is_finite() && a.fract() == 0. && a as i128 == b),
        (None, None) => a == b || a.as_f64().zip(b.as_f64()).is_some_and(|(a, b)| a.is_finite() && a == b),
    }
}
fn same_value(a: &Value, b: &Value) -> bool {
    a == b
        || match (a, b) {
            (Value::Array(a), Value::Array(b)) => a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_value(a, b)),
            (Value::Object(a), Value::Object(b)) => {
                a.len() == b.len() && a.iter().all(|(key, a)| b.get(key).is_some_and(|b| same_value(a, b)))
            }
            _ => same_number(a, b),
        }
}
fn same_nodata(a: &Array, b: &Array) -> bool {
    match (a.info.attributes.get("_FillValue"), b.info.attributes.get("_FillValue")) {
        (Some(a), Some(b)) => a == b || same_number(a, b),
        (None, None) => true,
        _ => false,
    }
}
fn same_coordinate_metadata(a: &Array, b: &Array) -> bool {
    fn relevant(array: &Array) -> Value {
        let mut attributes = array.info.attributes.clone();
        if let Some(attributes) = attributes.as_object_mut() {
            for name in [
                "actual_range",
                "_FillValue",
                "comment",
                "history",
                "long_name",
                "source",
                "references",
                "bounds",
                "coordinates",
                "grid_mapping",
            ] {
                attributes.remove(name);
            }
        }
        attributes
    }
    same_value(&relevant(a), &relevant(b)) && same_nodata(a, b)
}
fn same_encoding(a: &Array, b: &Array) -> bool {
    for (name, default) in [("scale_factor", serde_json::json!(1)), ("add_offset", serde_json::json!(0))] {
        if !same_number(a.info.attributes.get(name).unwrap_or(&default), b.info.attributes.get(name).unwrap_or(&default)) {
            return false;
        }
    }
    for name in ["valid_range", "valid_min", "valid_max", "missing_value", "_Unsigned", "_CRS"] {
        match (a.info.attributes.get(name), b.info.attributes.get(name)) {
            (None, None) => (),
            (Some(a), Some(b)) if same_value(a, b) => (),
            _ => return false,
        }
    }
    let empty = serde_json::json!("");
    a.info.attributes.get("units").unwrap_or(&empty) == b.info.attributes.get("units").unwrap_or(&empty)
}
fn parent(path: &str) -> &str {
    let p = path.rsplit_once('/').unwrap().0;
    if p.is_empty() { "/" } else { p }
}

/// Builds a coordinate-aligned mosaic without reading data-array payloads.
/// Changing dimensions require numeric one-dimensional indexing variables;
/// common unindexed dimensions are retained positionally at their existing size. Multi-
/// sample axes can be regularly spaced and aligned or disjoint irregular ranges.
/// All-singleton axes may be irregular; mixed singleton slices must align with a regular series.
/// Last input wins overlaps (including nodata), and gaps use
/// source nodata or zero. Data hyperslabs are fetched only when requested.
pub fn mosaic(datasets: &[Dataset], options: &MosaicOptions) -> Result<Dataset> {
    if datasets.is_empty() {
        return Err(bad("mosaic requires at least one dataset"));
    }
    let available: BTreeSet<_> = datasets.iter().flat_map(|d| d.root.arrays_recursive()).map(|a| a.info.path.clone()).collect();
    let mut selected = BTreeSet::new();
    if options.arrays.is_empty() {
        selected.extend(
            datasets.iter().flat_map(|d| d.root.arrays_recursive()).filter(|a| a.info.dimensions.len() >= 2).map(|a| a.info.path.clone()),
        );
    } else {
        for given in &options.arrays {
            let exact = format!("/{}", given.trim_matches('/'));
            let matches: Vec<_> =
                available.iter().filter(|p| **p == exact || (!given.contains('/') && p.rsplit('/').next() == Some(given))).collect();
            if matches.len() != 1 {
                return Err(bad(format!("array '{given}' is missing or ambiguous")));
            }
            selected.insert(matches[0].clone());
        }
    }
    if selected.is_empty() {
        return Err(bad("no arrays selected for mosaic"));
    }
    let references = references::Plan::collect(datasets, &mut selected)?;
    let mut input_axes: BTreeMap<String, BTreeMap<usize, (Dimension, Option<Array>)>> = BTreeMap::new();
    for (i, dataset) in datasets.iter().enumerate() {
        for path in &selected {
            if let Ok(array) = dataset.array(path) {
                for dimension in &array.info.dimensions {
                    let coordinate = dimension.indexing_variable.as_deref().map(|path| dataset.array(path)).transpose()?;
                    if let Some((previous, _)) =
                        input_axes.entry(dimension.path.clone()).or_default().insert(i, (dimension.clone(), coordinate))
                        && previous != *dimension
                    {
                        return Err(bad(format!("conflicting dimension declarations for '{}'", dimension.path)));
                    }
                }
            }
        }
    }
    let axes: BTreeMap<_, _> = input_axes
        .into_iter()
        .map(|(path, entries)| {
            let entries: Vec<_> = entries.into_iter().map(|(i, (d, a))| (i, d, a)).collect();
            Ok((path, axis(&entries)?))
        })
        .collect::<Result<_>>()?;
    let mut root = Group::new("/");
    root.attributes.clone_from(&datasets[0].root.attributes);
    root.attribute_types.clone_from(&datasets[0].root.attribute_types);
    for axis in axes.values() {
        group_mut(&mut root, parent(&axis.dimension.path), datasets).dimensions.push(axis.dimension.clone());
        if let Some(coordinate) = &axis.coordinate {
            group_mut(&mut root, parent(&coordinate.info.path), datasets).arrays.push(coordinate.clone());
        }
    }
    for path in selected {
        if axes.values().any(|a| a.coordinate.as_ref().is_some_and(|c| c.info.path == path)) {
            continue;
        }
        let arrays: Vec<_> = datasets.iter().enumerate().filter_map(|(i, d)| d.array(&path).ok().map(|a| (i, a))).collect();
        let template = &arrays[0].1;
        let mut pieces = Vec::new();
        for (i, array) in &arrays {
            if array.info.data_type != template.info.data_type
                || array.info.dimensions.len() != template.info.dimensions.len()
                || !same_nodata(array, template)
                || !same_encoding(array, template)
                || (references.grids.contains_key(&path) && !same_value(&array.info.attributes, &template.info.attributes))
                || array.info.dimensions.iter().zip(&template.info.dimensions).any(|(a, b)| a.path != b.path || a.name != b.name)
            {
                return Err(bad(format!("incompatible array '{path}': dimensions, type, nodata or encoding metadata differ")));
            }
            let starts = array.info.dimensions.iter().map(|d| axes[&d.path].starts[i]).collect();
            pieces.push(Piece { array: array.clone(), starts });
        }
        let mut info = template.info.clone();
        if let Some(attributes) = info.attributes.as_object_mut() {
            attributes.remove("actual_range");
        }
        info.dimensions = info.dimensions.iter().map(|d| axes[&d.path].dimension.clone()).collect();
        let array = Array {
            info,
            source: Source::Mosaic(Arc::new(MosaicSource { pieces, fill: fill(template)? })),
            attribute_types: template.attribute_types.clone(),
        };
        group_mut(&mut root, parent(&path), datasets).arrays.push(array);
    }
    for (path, mapping) in &references.grids {
        if !root.arrays_recursive().iter().any(|array| array.info.path == *path) {
            group_mut(&mut root, parent(path), datasets).arrays.push(mapping.clone());
        }
    }
    references.apply(&mut root);
    let output = Dataset { driver: "Mosaic".into(), root };
    write::validate(&output)?;
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn dataset(axes: &[(&str, &[i64])], values: &[i64]) -> Dataset {
        let mut root = Group::new("/");
        root.attributes = json!({"title":"mosaic test"});
        for (name, values) in axes {
            let dimension = Dimension {
                name: name.to_string(),
                path: format!("/{name}"),
                size: values.len(),
                unlimited: *name == "time",
                indexing_variable: Some(format!("/{name}")),
            };
            root.dimensions.push(dimension.clone());
            root.arrays.push(
                Array::from_bytes(
                    ArrayInfo {
                        name: name.to_string(),
                        path: format!("/{name}"),
                        data_type: DataType::I64,
                        dimensions: vec![dimension],
                        chunks: Some(vec![2]),
                        attributes: json!({}),
                        coordinates: vec![],
                    },
                    values.iter().flat_map(|v| v.to_le_bytes()).collect(),
                )
                .unwrap(),
            );
        }
        root.arrays.push(
            Array::from_bytes(
                ArrayInfo {
                    name: "v".into(),
                    path: "/v".into(),
                    data_type: DataType::I64,
                    dimensions: root.dimensions.clone(),
                    chunks: Some(vec![1; axes.len()]),
                    attributes: json!({"_FillValue":-9999,"scale_factor":0.1}),
                    coordinates: root.dimensions.iter().map(|d| d.indexing_variable.clone().unwrap()).collect(),
                },
                values.iter().flat_map(|v| v.to_le_bytes()).collect(),
            )
            .unwrap(),
        );
        Dataset { driver: "Memory".into(), root }
    }
    fn move_array(source: &mut Dataset, name: &str, path: &str) {
        let index = source.root.arrays.iter().position(|a| a.info.name == name).unwrap();
        let mut array = source.root.arrays.remove(index);
        array.info.path = path.into();
        group_mut(&mut source.root, parent(path), &[]).arrays.push(array);
    }
    fn auxiliary(source: &mut Dataset, path: &str, values: &[i64]) {
        let mut info = source.root.arrays_recursive().iter().find(|a| a.info.name == "v").unwrap().info.clone();
        info.path = path.into();
        info.name = path.rsplit('/').next().unwrap().into();
        info.attributes = json!({});
        info.coordinates.clear();
        let array = Array::from_bytes(info, values.iter().flat_map(|v| v.to_le_bytes()).collect()).unwrap();
        group_mut(&mut source.root, parent(path), &[]).arrays.push(array);
    }
    fn mapping(source: &mut Dataset, path: &str, definition: Value) {
        let mut info = source.root.arrays_recursive().iter().find(|a| a.info.name == "v").unwrap().info.clone();
        info.path = path.into();
        info.name = path.rsplit('/').next().unwrap().into();
        info.dimensions.clear();
        info.coordinates.clear();
        info.chunks = None;
        info.data_type = DataType::I32;
        info.attributes = definition;
        let array = Array::from_bytes(info, 0i32.to_le_bytes().to_vec()).unwrap();
        group_mut(&mut source.root, parent(path), &[]).arrays.push(array);
    }
    #[test]
    fn reference_resolution_never_skips_an_unsupported_nearer_variable() {
        let mut source = dataset(&[("y", &[0]), ("x", &[0])], &[1]);
        move_array(&mut source, "v", "/g/v");
        auxiliary(&mut source, "/latitude", &[10]);
        source.root.groups[0].arrays[0].info.attributes["coordinates"] = json!("latitude");
        source.root.groups[0].unsupported_arrays.push(UnsupportedArray {
            path: "/g/latitude".into(),
            data_type: "string".into(),
            shape: vec![1, 1],
        });
        #[cfg(any(feature = "netcdf-hdf5", feature = "zarr"))]
        {
            super::super::resolve_coordinates(&mut source.root);
            assert!(source.array("/g/v").unwrap().info.coordinates.contains(&"/g/latitude".into()));
        }
        assert!(mosaic(&[source], &MosaicOptions { arrays: vec!["/g/v".into()] }).unwrap_err().to_string().contains("/g/latitude"));
    }
    #[test]
    fn explicit_relative_references_do_not_search_other_ancestor_paths() {
        let mut source = dataset(&[("y", &[0]), ("x", &[0])], &[1]);
        move_array(&mut source, "v", "/g/v");
        auxiliary(&mut source, "/sub/latitude", &[10]);
        source.root.groups[0].arrays[0].info.attributes["coordinates"] = json!("sub/latitude");
        assert!(mosaic(&[source.clone()], &MosaicOptions { arrays: vec!["/g/v".into()] }).is_err());
        source.root.groups[0].arrays[0].info.attributes["coordinates"] = json!("../sub/latitude");
        let output = mosaic(&[source], &MosaicOptions { arrays: vec!["/g/v".into()] }).unwrap();
        assert!(output.array("/g/v").unwrap().info.coordinates.contains(&"/sub/latitude".into()));
        assert_eq!(output.array("/sub/latitude").unwrap().read().unwrap().values().unwrap(), json!([10]));
    }
    #[test]
    fn selected_payload_keeps_two_dimensional_auxiliary_coordinates() {
        for typed_only in [false, true] {
            let mut sources = Vec::new();
            for row in 0..2i64 {
                let mut source = dataset(&[("y", &[row]), ("x", &[0, 1])], &[row * 2 + 1, row * 2 + 2]);
                auxiliary(&mut source, "/latitude", &[100 + row * 10, 101 + row * 10]);
                auxiliary(&mut source, "/longitude", &[200 + row * 10, 201 + row * 10]);
                let v = source.root.arrays.iter_mut().find(|a| a.info.name == "v").unwrap();
                if typed_only {
                    v.info.coordinates.extend(["/latitude".into(), "/longitude".into()]);
                } else {
                    v.info.attributes["coordinates"] = json!("latitude longitude");
                }
                sources.push(source);
            }
            let output = mosaic(&sources, &MosaicOptions { arrays: vec!["v".into()] }).unwrap();
            assert_eq!(output.array("latitude").unwrap().read().unwrap().values().unwrap(), json!([100, 101, 110, 111]));
            assert_eq!(output.array("longitude").unwrap().read().unwrap().values().unwrap(), json!([200, 201, 210, 211]));
            let info = output.array("v").unwrap().info;
            assert!(info.coordinates.contains(&"/latitude".into()));
            assert_eq!(info.attributes["coordinates"], json!("latitude longitude"));
            #[cfg(feature = "netcdf-hdf5")]
            {
                let back = super::super::open_bytes(&output.to_netcdf_bytes().unwrap()).unwrap();
                assert!(back.array("v").unwrap().info.coordinates.contains(&"/longitude".into()));
                assert_eq!(back.array("latitude").unwrap().read().unwrap().values().unwrap(), json!([100, 101, 110, 111]));
            }
            #[cfg(feature = "zarr")]
            {
                let back = super::super::open_zarr_store(output.to_zarr_store().unwrap()).unwrap();
                assert!(back.array("v").unwrap().info.coordinates.contains(&"/latitude".into()));
                assert_eq!(back.array("longitude").unwrap().read().unwrap().values().unwrap(), json!([200, 201, 210, 211]));
            }
            let mut damaged = sources.clone();
            damaged[1].root.arrays.iter_mut().find(|a| a.info.name == "latitude").unwrap().source = Source::Memory(Arc::new(vec![]));
            let lazy = mosaic(&damaged, &MosaicOptions { arrays: vec!["v".into()] }).unwrap();
            assert_eq!(lazy.array("v").unwrap().read().unwrap().values().unwrap(), json!([1, 2, 3, 4]));
            assert!(lazy.array("latitude").unwrap().read().is_err());
            let mut missing = sources.clone();
            missing[1].root.arrays.retain(|a| a.info.name != "latitude");
            assert!(mosaic(&missing, &MosaicOptions { arrays: vec!["v".into()] }).is_err());
        }
    }
    #[test]
    fn resolved_grid_mapping_aliases_require_matching_crs_metadata() {
        let mut a = dataset(&[("y", &[0]), ("x", &[0])], &[1]);
        let mut b = dataset(&[("y", &[1]), ("x", &[0])], &[2]);
        move_array(&mut a, "v", "/g/v");
        move_array(&mut b, "v", "/g/v");
        a.root.groups[0].arrays[0].info.attributes["grid_mapping"] = json!("crs");
        b.root.groups[0].arrays[0].info.attributes["grid_mapping"] = json!("/crs");
        mapping(&mut a, "/g/crs", json!({"spatial_ref":"EPSG:4326","false_easting":0}));
        mapping(&mut b, "/crs", json!({"spatial_ref":"EPSG:3857","false_easting":0.0}));
        assert!(mosaic(&[a.clone(), b.clone()], &Default::default()).unwrap_err().to_string().contains("grid mapping"));
        b.root.arrays.iter_mut().find(|a| a.info.path == "/crs").unwrap().info.attributes["spatial_ref"] = json!("EPSG:4326");
        // A non-referenced same-path mapping in the second input must not override
        // the canonical mapping actually referenced by the first input.
        mapping(&mut b, "/g/crs", json!({"spatial_ref":"EPSG:3857"}));
        let output = mosaic(&[a, b], &Default::default()).unwrap();
        assert_eq!(output.array("/g/v").unwrap().info.attributes["grid_mapping"], json!("crs"));
        assert_eq!(output.array("/g/crs").unwrap().info.attributes["spatial_ref"], json!("EPSG:4326"));
        assert_eq!(output.array("/g/v").unwrap().read().unwrap().values().unwrap(), json!([1, 2]));
        assert!(output.array("/crs").is_err());
    }
    #[test]
    fn output_group_shadowing_cannot_rebind_auxiliary_coordinates() {
        let mut a = dataset(&[("y", &[0]), ("x", &[0])], &[1]);
        let mut b = dataset(&[("y", &[1]), ("x", &[0])], &[2]);
        move_array(&mut a, "v", "/g/v");
        auxiliary(&mut a, "/latitude", &[10]);
        move_array(&mut b, "v", "/g/v");
        auxiliary(&mut b, "/g/latitude", &[20]);
        a.root.groups[0].arrays[0].info.attributes["coordinates"] = json!("latitude");
        let v = &mut b.root.groups[0].arrays[0];
        v.info.name = "u".into();
        v.info.path = "/g/u".into();
        v.info.attributes["coordinates"] = json!("latitude");
        let output = mosaic(&[a.clone(), b], &MosaicOptions { arrays: vec!["/g/v".into(), "/g/u".into()] }).unwrap();
        assert_eq!(output.array("/g/v").unwrap().info.attributes["coordinates"], json!("/latitude"));
        assert_eq!(output.array("/g/u").unwrap().info.attributes["coordinates"], json!("latitude"));
        assert_eq!(output.array("/latitude").unwrap().read().unwrap().values().unwrap(), json!([10, 0]));
        #[cfg(feature = "zarr")]
        {
            // Re-read a real store with an unqualified ancestor coordinate before
            // mosaicking: the Zarr reader must publish the resolved absolute path.
            let back = super::super::open_zarr_store(a.to_zarr_store().unwrap()).unwrap();
            assert!(back.array("/g/v").unwrap().info.coordinates.contains(&"/latitude".into()));
            assert!(!back.array("/g/v").unwrap().info.coordinates.contains(&"/g/latitude".into()));
        }
    }
    #[test]
    fn equal_bounds_names_cannot_resolve_to_different_input_paths() {
        let mut sources = Vec::new();
        for (row, bounds_path) in [(0i64, "/bounds"), (1, "/g/bounds")] {
            let mut source = dataset(&[("y", &[row]), ("x", &[0])], &[row + 1]);
            move_array(&mut source, "v", "/g/v");
            move_array(&mut source, "y", "/g/y");
            fn relink(group: &mut Group) {
                for d in &mut group.dimensions {
                    if d.path == "/y" {
                        d.indexing_variable = Some("/g/y".into());
                    }
                }
                for a in &mut group.arrays {
                    for d in &mut a.info.dimensions {
                        if d.path == "/y" {
                            d.indexing_variable = Some("/g/y".into());
                        }
                    }
                    for c in &mut a.info.coordinates {
                        if c == "/y" {
                            *c = "/g/y".into();
                        }
                    }
                    if a.info.path == "/g/y" {
                        a.info.attributes["bounds"] = json!("bounds");
                    }
                }
                for child in &mut group.groups {
                    relink(child);
                }
            }
            relink(&mut source.root);
            let bounds_dimension =
                Dimension { name: "bnds".into(), path: "/bnds".into(), size: 2, unlimited: false, indexing_variable: None };
            source.root.dimensions.push(bounds_dimension.clone());
            let mut info = source.root.arrays_recursive().iter().find(|a| a.info.name == "v").unwrap().info.clone();
            info.path = bounds_path.into();
            info.name = "bounds".into();
            info.coordinates.clear();
            info.attributes = json!({});
            info.dimensions = vec![source.root.dimensions[0].clone(), bounds_dimension];
            let array = Array::from_bytes(info, [row * 10, row * 10 + 1].iter().flat_map(|v| v.to_le_bytes()).collect()).unwrap();
            group_mut(&mut source.root, parent(bounds_path), &[]).arrays.push(array);
            sources.push(source);
        }
        assert!(mosaic(&sources, &MosaicOptions { arrays: vec!["/g/v".into()] }).unwrap_err().to_string().contains("CF bounds"));
    }
    #[test]
    fn two_axis_tiles_gaps_descending_coordinates_and_strided_views() {
        let a = dataset(&[("y", &[2, 1]), ("x", &[0, 1])], &[1, 2, 3, 4]);
        let b = dataset(&[("y", &[1, 0]), ("x", &[3, 4])], &[5, 6, 7, 8]);
        let result = mosaic(&[a, b], &Default::default()).unwrap();
        let array = result.array("v").unwrap();
        assert_eq!(array.shape(), [3, 5]);
        assert_eq!(
            array.read().unwrap().values().unwrap(),
            json!([1, 2, -9999, -9999, -9999, 3, 4, -9999, 5, 6, -9999, -9999, -9999, 7, 8])
        );
        assert_eq!(array.view().slice("x", 1, 2, 2).unwrap().isel("y", 1).unwrap().read().unwrap().values().unwrap(), json!([4, 5]));
        assert_eq!(result.array("y").unwrap().read().unwrap().values().unwrap(), json!([2, 1, 0]));
        assert_eq!(array.info.attributes["scale_factor"], json!(0.1));
    }
    #[test]
    fn scalar_cf_missing_value_fills_gaps_without_fill_value() {
        let mut sources =
            vec![dataset(&[("y", &[0]), ("x", &[0, 1, 2])], &[1, 2, 3]), dataset(&[("y", &[0]), ("x", &[5, 6, 7])], &[4, 5, 6])];
        for source in &mut sources {
            source.root.arrays.last_mut().unwrap().info.attributes = json!({"missing_value": -999});
        }
        let out = mosaic(&sources, &Default::default()).unwrap();
        assert_eq!(out.array("v").unwrap().read().unwrap().values().unwrap(), json!([1, 2, 3, -999, -999, 4, 5, 6]));
        assert_eq!(out.array("v").unwrap().info.attributes["missing_value"], json!(-999));
        for source in &mut sources {
            source.root.arrays.last_mut().unwrap().info.attributes["_FillValue"] = json!(-99);
        }
        let out = mosaic(&sources, &Default::default()).unwrap();
        assert_eq!(out.array("v").unwrap().read().unwrap().values().unwrap(), json!([1, 2, 3, -99, -99, 4, 5, 6]));
    }
    #[test]
    fn identical_array_valued_packing_metadata_is_preserved() {
        let mut sources = vec![dataset(&[("y", &[0]), ("x", &[0, 1])], &[1, 2]), dataset(&[("y", &[0]), ("x", &[2, 3])], &[3, 4])];
        for source in &mut sources {
            source.root.arrays.last_mut().unwrap().info.attributes["scale_factor"] = json!([0.1]);
        }
        let out = mosaic(&sources, &Default::default()).unwrap();
        assert_eq!(out.array("v").unwrap().info.attributes["scale_factor"], json!([0.1]));
        sources[1].root.arrays.last_mut().unwrap().info.attributes["scale_factor"] = json!([0.2]);
        assert!(mosaic(&sources, &Default::default()).is_err());
    }
    #[test]
    fn irregular_singleton_slices_preserve_large_integer_coordinates() {
        let base = 9007199254740993i64;
        let a = dataset(&[("time", &[base]), ("x", &[0, 3, 4])], &[1, 2, 3]);
        let b = dataset(&[("time", &[base + 7]), ("x", &[0, 3, 4])], &[4, 5, 6]);
        let c = dataset(&[("time", &[base + 2]), ("x", &[0, 3, 4])], &[7, 8, 9]);
        let result = mosaic(&[a, b, c], &Default::default()).unwrap();
        assert_eq!(result.array("v").unwrap().read().unwrap().values().unwrap(), json!([1, 2, 3, 7, 8, 9, 4, 5, 6]));
        assert_eq!(result.array("time").unwrap().read().unwrap().values().unwrap(), json!([base, base + 2, base + 7]));
        assert!(result.array("time").unwrap().info.dimensions[0].unlimited);
    }
    #[test]
    fn integral_grid_steps_above_float_precision_remain_exact() {
        let step = 9007199254740993i64;
        let a = dataset(&[("y", &[0]), ("x", &[0, step])], &[1, 2]);
        let b = dataset(&[("y", &[0]), ("x", &[step * 2, step * 3])], &[3, 4]);
        let result = mosaic(&[a, b], &Default::default()).unwrap();
        assert_eq!(result.array("x").unwrap().read().unwrap().values().unwrap(), json!([0, step, step * 2, step * 3]));
    }
    #[test]
    fn disjoint_irregular_ranges_are_concatenated() {
        let a = dataset(&[("time", &[0, 1, 3]), ("x", &[0])], &[1, 2, 3]);
        let b = dataset(&[("time", &[4, 7]), ("x", &[0])], &[4, 5]);
        let result = mosaic(&[a, b], &Default::default()).unwrap();
        assert_eq!(result.array("time").unwrap().read().unwrap().values().unwrap(), json!([0, 1, 3, 4, 7]));
        assert_eq!(result.array("v").unwrap().read().unwrap().values().unwrap(), json!([1, 2, 3, 4, 5]));
    }
    #[test]
    fn overlap_uses_last_source_including_nodata() {
        let a = dataset(&[("y", &[0]), ("x", &[0, 1])], &[1, 2]);
        let b = dataset(&[("y", &[0]), ("x", &[0, 1])], &[-9999, 4]);
        assert_eq!(mosaic(&[a, b], &Default::default()).unwrap().array("v").unwrap().read().unwrap().values().unwrap(), json!([-9999, 4]));
    }
    #[test]
    fn unselected_payloads_are_never_read() {
        let a = dataset(&[("y", &[0]), ("x", &[0, 1])], &[1, 2]);
        let mut b = dataset(&[("y", &[0]), ("x", &[4, 5])], &[3, 4]);
        b.root.arrays.last_mut().unwrap().source = Source::Memory(Arc::new(vec![]));
        let result = mosaic(&[a, b], &Default::default()).unwrap();
        let v = result.array("v").unwrap();
        assert_eq!(v.view().slice("x", 0, 2, 1).unwrap().read().unwrap().values().unwrap(), json!([1, 2]));
        assert!(v.view().slice("x", 4, 1, 1).unwrap().read().is_err());
    }
    #[test]
    fn scalar_cf_grid_mapping_metadata_is_retained() {
        let mut a = dataset(&[("y", &[0]), ("x", &[0, 1])], &[1, 2]);
        a.root.arrays.last_mut().unwrap().info.attributes["grid_mapping"] = json!("crs");
        a.root.arrays.push(
            Array::from_bytes(
                ArrayInfo {
                    name: "crs".into(),
                    path: "/crs".into(),
                    data_type: DataType::I32,
                    dimensions: vec![],
                    chunks: None,
                    attributes: json!({"spatial_ref":"source WKT"}),
                    coordinates: vec![],
                },
                0i32.to_le_bytes().to_vec(),
            )
            .unwrap(),
        );
        let result = mosaic(&[a], &Default::default()).unwrap();
        assert_eq!(result.array("crs").unwrap().info.attributes["spatial_ref"], json!("source WKT"));
        assert_eq!(result.array("v").unwrap().info.attributes["grid_mapping"], json!("crs"));
    }
    #[test]
    fn descending_irregular_shared_coordinates_are_sorted_and_deduplicated() {
        let a = dataset(&[("y", &[9, 7, 4]), ("x", &[0])], &[1, 2, 3]);
        let b = dataset(&[("y", &[9, 7, 4]), ("x", &[1])], &[4, 5, 6]);
        let c = dataset(&[("y", &[3, 1]), ("x", &[0])], &[7, 8]);
        let result = mosaic(&[a, b, c], &Default::default()).unwrap();
        assert_eq!(result.array("y").unwrap().read().unwrap().values().unwrap(), json!([9, 7, 4, 3, 1]));
        assert_eq!(result.array("v").unwrap().read().unwrap().values().unwrap(), json!([1, 4, 2, 5, 3, 6, 7, -9999, 8, -9999]));
    }
    #[test]
    fn float64_regular_tiles_preserve_stored_coordinate_bits() {
        let mut sources = Vec::new();
        let mut expected = Vec::new();
        for tile in 0..2 {
            let mut source = dataset(&[("y", &[0, 1, 2, 3, 4, 5, 6, 7]), ("x", &[0])], &[1; 8]);
            let values: Vec<f64> = (0..8).map(|j| 40. + j as f64 * 0.1 + tile as f64 * 1.2).collect();
            expected.push(values.clone());
            let coordinate = &mut source.root.arrays[0];
            coordinate.info.data_type = DataType::F64;
            coordinate.source = Source::Memory(Arc::new(values.iter().flat_map(|v| v.to_le_bytes()).collect()));
            sources.push(source);
        }
        let output = mosaic(&sources, &Default::default()).unwrap();
        let wbraster::raster::RasterData::F64(coordinates) = output.array("y").unwrap().read().unwrap().data().unwrap() else {
            panic!("expected Float64 coordinates")
        };
        let actual: Vec<u64> = coordinates.iter().map(|value| value.to_bits()).collect();
        for (start, values) in [0, 12].into_iter().zip(expected) {
            assert_eq!(&actual[start..start + 8], values.iter().map(|v| v.to_bits()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn float32_regular_tiles_keep_gap_cells() {
        let mut sources = Vec::new();
        for i in 0..2 {
            let mut source = dataset(&[("y", &[0, 1, 2, 3, 4, 5, 6, 7]), ("x", &[0])], &[i + 1; 8]);
            let coordinate = &mut source.root.arrays[0];
            coordinate.info.data_type = DataType::F32;
            coordinate.source =
                Source::Memory(Arc::new((0..8).flat_map(|j| (40f32 + j as f32 * 0.1 + i as f32 * 1.2).to_le_bytes()).collect()));
            sources.push(source);
        }
        let output = mosaic(&sources, &Default::default()).unwrap();
        assert_eq!(output.array("v").unwrap().shape(), [20, 1]);
        let expected: Vec<i64> = [vec![1; 8], vec![-9999; 4], vec![2; 8]].concat();
        assert_eq!(output.array("v").unwrap().read().unwrap().values().unwrap(), json!(expected));
    }
    #[test]
    fn coordinate_memory_limits_precede_payload_reads_and_gap_allocation() {
        let a = dataset(&[("y", &[0]), ("x", &[0, 1])], &[1, 2]);
        let b = dataset(&[("y", &[0]), ("x", &[20_000_000, 20_000_001])], &[3, 4]);
        assert!(mosaic(&[a.clone(), b], &Default::default()).unwrap_err().to_string().contains("256 MiB"));
        let mut forged = a;
        let huge = MAX_COORDINATE_BYTES / std::mem::size_of::<Coordinate>() + 1;
        forged.root.dimensions[1].size = huge;
        forged.root.arrays[1].info.data_type = DataType::U8;
        forged.root.arrays[1].info.dimensions[0].size = huge;
        forged.root.arrays[1].source = Source::Memory(Arc::new(vec![]));
        forged.root.arrays.last_mut().unwrap().info.dimensions[1].size = huge;
        assert!(mosaic(&[forged], &Default::default()).unwrap_err().to_string().contains("256 MiB"));
    }
    #[test]
    fn incompatible_packing_and_units_are_rejected() {
        let a = dataset(&[("y", &[0]), ("x", &[0, 1])], &[1, 2]);
        for (name, value) in [("scale_factor", json!(0.2)), ("add_offset", json!(1)), ("units", json!("K"))] {
            let mut b = a.clone();
            b.root.arrays.last_mut().unwrap().info.attributes[name] = value;
            assert!(mosaic(&[a.clone(), b], &Default::default()).is_err());
        }
        let mut a = a;
        let mut b = a.clone();
        a.root.arrays.last_mut().unwrap().info.attributes["add_offset"] = json!(9007199254740993i64);
        b.root.arrays.last_mut().unwrap().info.attributes["add_offset"] = json!(9007199254740992i64);
        assert!(mosaic(&[a, b], &Default::default()).is_err());
    }
    #[test]
    fn identical_axes_still_require_strict_monotonicity() {
        for coordinates in [&[0, 0, 1][..], &[0, 2, 1][..]] {
            let a = dataset(&[("y", coordinates), ("x", &[0])], &[1, 2, 3]);
            assert!(mosaic(&[a.clone(), a], &Default::default()).unwrap_err().to_string().contains("strictly monotonic"));
        }
        let a = dataset(&[("y", &[3, 2, 0]), ("x", &[0])], &[1, 2, 3]);
        assert!(mosaic(&[a.clone(), a], &Default::default()).is_ok());
    }
    #[test]
    fn coordinate_metadata_compares_numbers_without_integer_precision_loss() {
        let mut a = dataset(&[("y", &[0]), ("x", &[0, 1])], &[1, 2]);
        let mut b = a.clone();
        a.root.arrays[1].info.attributes["valid_range"] = json!([0, 1]);
        b.root.arrays[1].info.attributes["valid_range"] = json!([0.0, 1.0]);
        assert!(mosaic(&[a.clone(), b.clone()], &Default::default()).is_ok());
        a.root.arrays[1].info.attributes["valid_min"] = json!(9007199254740993i64);
        b.root.arrays[1].info.attributes["valid_min"] = json!(9007199254740992.0);
        assert!(mosaic(&[a, b], &Default::default()).is_err());
    }
    #[test]
    fn crs_must_match_even_without_grid_mapping() {
        let mut a = dataset(&[("y", &[0]), ("x", &[0, 1])], &[1, 2]);
        a.root.arrays.last_mut().unwrap().info.attributes["_CRS"] = json!({"url":"EPSG:4326"});
        let mut b = a.clone();
        assert!(mosaic(&[a.clone(), b.clone()], &Default::default()).is_ok());
        b.root.arrays.last_mut().unwrap().info.attributes["_CRS"] = json!({"url":"EPSG:3857"});
        assert!(mosaic(&[a.clone(), b.clone()], &Default::default()).is_err());
        b.root.arrays.last_mut().unwrap().info.attributes.as_object_mut().unwrap().remove("_CRS");
        assert!(mosaic(&[a, b], &Default::default()).is_err());
    }
    #[test]
    fn selected_payload_retains_coordinate_bounds_and_rejects_missing_targets() {
        let sources: Vec<_> = [0, 2]
            .into_iter()
            .map(|offset| {
                let mut a = dataset(&[("y", &[offset, offset + 1]), ("x", &[0])], &[1, 2]);
                a.root.arrays[0].info.attributes["bounds"] = json!("bounds");
                let dimension = Dimension { name: "bnds".into(), path: "/bnds".into(), size: 2, unlimited: false, indexing_variable: None };
                let mut bounds = a.root.arrays.last().unwrap().clone();
                bounds.info.path = "/bounds".into();
                bounds.info.name = "bounds".into();
                bounds.info.dimensions = vec![a.root.dimensions[0].clone(), dimension.clone()];
                bounds.source = Source::Memory(Arc::new(
                    [offset * 10 + 1, offset * 10 + 2, offset * 10 + 11, offset * 10 + 12].iter().flat_map(|v| v.to_le_bytes()).collect(),
                ));
                a.root.dimensions.push(dimension);
                a.root.arrays.push(bounds);
                a
            })
            .collect();
        let options = MosaicOptions { arrays: vec!["v".into()] };
        let result = mosaic(&sources, &options).unwrap();
        assert_eq!(result.array("bounds").unwrap().read().unwrap().values().unwrap(), json!([1, 2, 11, 12, 21, 22, 31, 32]));
        assert_eq!(result.array("y").unwrap().info.attributes["bounds"], json!("bounds"));
        let mut missing = sources.clone();
        missing[1].root.arrays.pop();
        assert!(mosaic(&missing, &options).is_err());
    }
    #[test]
    fn unindexed_bounds_dimensions_are_retained_and_size_checked() {
        let mut a = dataset(&[("y", &[0]), ("x", &[0, 1])], &[1, 2]);
        let dimension = Dimension { name: "bnds".into(), path: "/bnds".into(), size: 2, unlimited: false, indexing_variable: None };
        let mut bounds = a.root.arrays.last().unwrap().clone();
        bounds.info.path = "/bounds".into();
        bounds.info.name = "bounds".into();
        bounds.info.dimensions = vec![a.root.dimensions[0].clone(), dimension.clone()];
        a.root.dimensions.push(dimension);
        a.root.arrays.push(bounds);
        let output = mosaic(&[a.clone()], &Default::default()).unwrap();
        assert_eq!(output.array("bounds").unwrap().shape(), [1, 2]);
        assert!(output.array("v").is_ok());
        assert!(mosaic(&[a.clone()], &MosaicOptions { arrays: vec!["bounds".into()] }).is_ok());
        let mut b = a.clone();
        b.root.arrays.last_mut().unwrap().info.dimensions[1].size = 3;
        assert!(mosaic(&[a, b], &Default::default()).is_err());
    }
    #[test]
    fn true_fractional_cell_misalignment_is_rejected() {
        let mut sources = Vec::new();
        for offset in [0f32, 1.2 + 0.025] {
            let mut source = dataset(&[("y", &[0, 1, 2, 3, 4, 5, 6, 7]), ("x", &[0])], &[1; 8]);
            let coordinate = &mut source.root.arrays[0];
            coordinate.info.data_type = DataType::F32;
            coordinate.source = Source::Memory(Arc::new((0..8).flat_map(|j| (40f32 + j as f32 * 0.1 + offset).to_le_bytes()).collect()));
            sources.push(source);
        }
        assert!(mosaic(&sources, &Default::default()).unwrap_err().to_string().contains("not aligned"));
    }
    #[test]
    fn distant_float32_rounding_is_not_silently_treated_as_alignment() {
        // GDAL 3.13.3 also rejects these stored coordinate grids: local
        // Float32 step estimates differ between the two distant tiles.
        for offset in [900, 1000] {
            let sources: Vec<_> = [0, offset]
                .into_iter()
                .map(|base| {
                    let mut source = dataset(&[("y", &[0, 1, 2, 3, 4, 5, 6, 7]), ("x", &[0])], &[1; 8]);
                    let coordinate = &mut source.root.arrays[0];
                    coordinate.info.data_type = DataType::F32;
                    coordinate.source =
                        Source::Memory(Arc::new((0..8).flat_map(|j| ((40. + (base + j) as f64 * 0.1) as f32).to_le_bytes()).collect()));
                    source
                })
                .collect();
            assert!(mosaic(&sources, &Default::default()).unwrap_err().to_string().contains("not aligned"));
        }
    }
    #[test]
    fn tile_coordinate_ranges_and_integral_float_nodata_are_accepted() {
        let mut a = dataset(&[("y", &[0]), ("x", &[0, 1])], &[1, 2]);
        let mut b = dataset(&[("y", &[0]), ("x", &[4, 5])], &[3, 4]);
        a.root.arrays[1].info.attributes["actual_range"] = json!([0, 1]);
        b.root.arrays[1].info.attributes["actual_range"] = json!([4, 5]);
        b.root.arrays.last_mut().unwrap().info.attributes["_FillValue"] = json!(-9999.0);
        a.root.arrays.last_mut().unwrap().info.attributes["actual_range"] = json!([1, 2]);
        b.root.arrays.last_mut().unwrap().info.attributes["actual_range"] = json!([3, 4]);
        let out = mosaic(&[a, b], &Default::default()).unwrap();
        assert_eq!(out.array("v").unwrap().read().unwrap().values().unwrap(), json!([1, 2, -9999, -9999, 3, 4]));
        assert!(out.array("x").unwrap().info.attributes.get("actual_range").is_none());
        assert!(out.array("v").unwrap().info.attributes.get("actual_range").is_none());
    }
    #[test]
    fn descriptive_coordinate_attributes_can_differ_but_validity_must_match() {
        let mut a = dataset(&[("y", &[0]), ("x", &[0, 1])], &[1, 2]);
        let mut b = dataset(&[("y", &[0]), ("x", &[2, 3])], &[3, 4]);
        a.root.arrays[1].info.attributes = json!({"comment":"west", "history":"first"});
        b.root.arrays[1].info.attributes = json!({"comment":"east", "history":"second"});
        let out = mosaic(&[a.clone(), b.clone()], &Default::default()).unwrap();
        assert_eq!(out.array("v").unwrap().read().unwrap().values().unwrap(), json!([1, 2, 3, 4]));
        assert_eq!(out.array("x").unwrap().info.attributes["comment"], json!("west"));
        a.root.arrays[1].info.attributes["valid_min"] = json!(0);
        b.root.arrays[1].info.attributes["valid_min"] = json!(2);
        assert!(mosaic(&[a, b], &Default::default()).is_err());
    }
    #[test]
    fn extended_cf_grid_mapping_syntax_keeps_each_scalar_mapping() {
        let mut source = dataset(&[("y", &[0]), ("x", &[0, 1])], &[1, 2]);
        source.root.arrays.last_mut().unwrap().info.attributes["grid_mapping"] = json!("crs: y x other: y x");
        for name in ["crs", "other"] {
            let mut info = source.root.arrays.last().unwrap().info.clone();
            info.name = name.into();
            info.path = format!("/{name}");
            info.data_type = DataType::I32;
            info.dimensions.clear();
            info.chunks = None;
            info.coordinates.clear();
            info.attributes = json!({"grid_mapping_name": "latitude_longitude"});
            source.root.arrays.push(Array::from_bytes(info, 0i32.to_le_bytes().to_vec()).unwrap());
        }
        let mut different = source.clone();
        different.root.arrays.iter_mut().find(|a| a.info.name == "v").unwrap().info.attributes["grid_mapping"] = json!("other: y x");
        assert!(mosaic(&[source.clone(), different], &Default::default()).unwrap_err().to_string().contains("encoding metadata"));
        let mut swapped = source.clone();
        swapped.root.arrays.iter_mut().find(|a| a.info.name == "v").unwrap().info.attributes["grid_mapping"] = json!("crs: x y other: y x");
        assert!(mosaic(&[source.clone(), swapped], &Default::default()).is_err());
        let out = mosaic(&[source], &Default::default()).unwrap();
        assert!(out.array("crs").is_ok());
        assert!(out.array("other").is_ok());
    }
    #[test]
    fn mixed_off_grid_singletons_are_rejected_like_gdal() {
        let a = dataset(&[("time", &[0, 6, 12]), ("x", &[0])], &[1, 2, 3]);
        let b = dataset(&[("time", &[19]), ("x", &[0])], &[4]);
        assert!(mosaic(&[a, b], &Default::default()).unwrap_err().to_string().contains("not aligned"));
    }
    #[test]
    fn incompatible_cf_value_interpretation_is_rejected() {
        let a = dataset(&[("y", &[0]), ("x", &[0, 1])], &[1, 2]);
        for (name, value) in [
            ("valid_range", json!([0, 10])),
            ("valid_min", json!(0)),
            ("valid_max", json!(10)),
            ("missing_value", json!(-999)),
            ("_Unsigned", json!("true")),
        ] {
            let mut b = a.clone();
            b.root.arrays.last_mut().unwrap().info.attributes[name] = value;
            assert!(mosaic(&[a.clone(), b], &Default::default()).unwrap_err().to_string().contains("encoding metadata"));
        }
        let mut a = a;
        a.root.arrays.last_mut().unwrap().info.attributes["valid_range"] = json!([0, 10]);
        let mut b = a.clone();
        b.root.arrays.last_mut().unwrap().info.attributes["valid_range"] = json!([0.0, 10.0]);
        assert!(mosaic(&[a, b], &Default::default()).is_ok());
    }
    #[test]
    fn incompatible_grid_types_and_nodata_are_rejected() {
        let a = dataset(&[("y", &[0]), ("x", &[0, 2])], &[1, 2]);
        let b = dataset(&[("y", &[0]), ("x", &[1, 3])], &[3, 4]);
        assert!(mosaic(&[a.clone(), b], &Default::default()).is_err());
        let mut b = a.clone();
        b.root.arrays.last_mut().unwrap().info.attributes["_FillValue"] = json!(-1);
        assert!(mosaic(&[a.clone(), b], &Default::default()).is_err());
        assert!(mosaic(&[a], &MosaicOptions { arrays: vec!["missing".into()] }).is_err());
        assert!(mosaic(&[], &Default::default()).is_err());
    }
}

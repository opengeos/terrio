//! Plan supported CF reference closure before reading any array payloads.
use super::*;
use std::collections::HashSet;

struct Mapping {
    array: Array,
    coordinates: Vec<String>,
}
struct Links {
    coordinates: Vec<String>,
    explicit_coordinates: Option<Vec<String>>,
    bounds: Option<String>,
    mappings: Vec<Mapping>,
    extended_mapping: bool,
}
fn append(paths: &mut Vec<String>, path: String) {
    if !paths.contains(&path) {
        paths.push(path);
    }
}
fn same_paths(a: &[String], b: &[String]) -> bool {
    a.iter().collect::<BTreeSet<_>>() == b.iter().collect::<BTreeSet<_>>()
}
fn attribute<'a>(array: &'a Array, name: &str) -> Result<Option<&'a str>> {
    array.info.attributes.get(name).map(|value| value.as_str().ok_or_else(|| bad(format!("CF {name} must be a string")))).transpose()
}
impl Links {
    fn read(dataset: &Dataset, array: &Array, names: &HashSet<String>) -> Result<Self> {
        if !array.info.attributes.is_object() && !array.info.attributes.is_null() {
            return Err(bad("array attributes must be a JSON object"));
        }
        let path = &array.info.path;
        let target = |name: &str, coordinate: bool| {
            let target = if coordinate {
                super::super::coordinate_reference_path(&array.info, name, names)
            } else {
                super::super::reference_path(path, name, names)
            };
            let target = target.ok_or_else(|| bad(format!("unresolved CF array reference '{name}' from '{path}'")))?;
            dataset.array(&target)
        };
        let resolve = |name: &str| target(name, false);
        let resolve_coordinate = |name: &str| target(name, true);
        let mut coordinates = Vec::new();
        for name in array
            .info
            .dimensions
            .iter()
            .filter_map(|dimension| dimension.indexing_variable.as_deref())
            .chain(array.info.coordinates.iter().map(String::as_str))
        {
            append(&mut coordinates, resolve_coordinate(name)?.info.path.clone());
        }
        let mut explicit_coordinates = attribute(array, "coordinates")?.map(|_| Vec::new());
        if let Some(raw) = attribute(array, "coordinates")? {
            for name in raw.split_whitespace() {
                let resolved = resolve_coordinate(name)?.info.path.clone();
                append(&mut coordinates, resolved.clone());
                append(explicit_coordinates.as_mut().expect("present attribute"), resolved);
            }
        }
        let bounds = attribute(array, "bounds")?
            .map(|name| {
                if name.is_empty() {
                    return Err(bad("CF bounds reference must be a nonempty array name"));
                }
                Ok(resolve(name)?.info.path.clone())
            })
            .transpose()?;
        let mut mappings = Vec::new();
        let mut extended_mapping = false;
        if let Some(raw) = attribute(array, "grid_mapping")? {
            if raw.trim().is_empty() {
                return Err(bad("CF grid_mapping must name a mapping variable"));
            }
            extended_mapping = raw.contains(':');
            let mut clauses: Vec<(&str, Vec<&str>)> = Vec::new();
            if extended_mapping {
                for token in raw.split_whitespace() {
                    if let Some((name, remainder)) = token.split_once(':') {
                        if name.is_empty() {
                            return Err(bad("CF grid mapping clause needs a variable name"));
                        }
                        clauses.push((name, Vec::new()));
                        if !remainder.is_empty() {
                            clauses.last_mut().expect("inserted clause").1.push(remainder);
                        }
                    } else {
                        clauses.last_mut().ok_or_else(|| bad("CF grid mapping coordinates need a preceding mapping clause"))?.1.push(token);
                    }
                }
            } else {
                clauses.push((raw, Vec::new()));
            }
            for (name, names) in clauses {
                let mapping = resolve(name)?;
                if !mapping.info.dimensions.is_empty() {
                    return Err(bad("CF grid mapping variables must be scalar"));
                }
                if extended_mapping && names.is_empty() {
                    return Err(bad("extended CF grid mapping clauses need coordinate variables"));
                }
                let mut mapped_coordinates = Vec::new();
                for name in names {
                    let coordinate = resolve_coordinate(name)?.info.path.clone();
                    append(&mut coordinates, coordinate.clone());
                    append(&mut mapped_coordinates, coordinate);
                }
                mappings.push(Mapping { array: mapping, coordinates: mapped_coordinates });
            }
        }
        // Typed auxiliary references must survive serialization even when a caller
        // constructed ArrayInfo directly without a raw CF coordinates attribute.
        let indexing: BTreeSet<_> = array.info.dimensions.iter().filter_map(|d| d.indexing_variable.as_deref()).collect();
        for path in &coordinates {
            if !indexing.contains(path.as_str()) {
                append(explicit_coordinates.get_or_insert_with(Vec::new), path.clone());
            }
        }
        Ok(Self { coordinates, explicit_coordinates, bounds, mappings, extended_mapping })
    }
    fn compatible(&self, other: &Self) -> bool {
        self.bounds == other.bounds
            && same_paths(&self.coordinates, &other.coordinates)
            && self.mappings.len() == other.mappings.len()
            && self
                .mappings
                .iter()
                .zip(&other.mappings)
                .all(|(a, b)| same_value(&a.array.info.attributes, &b.array.info.attributes) && a.coordinates == b.coordinates)
    }
}

pub(super) struct Plan {
    links: BTreeMap<String, Links>,
    pub(super) grids: BTreeMap<String, Array>,
}
impl Plan {
    pub(super) fn collect(datasets: &[Dataset], selected: &mut BTreeSet<String>) -> Result<Self> {
        let mut plan = Self { links: BTreeMap::new(), grids: BTreeMap::new() };
        let namespaces: Vec<HashSet<String>> =
            datasets.iter().map(|dataset| dataset.root.array_paths_recursive().into_iter().map(str::to_string).collect()).collect();
        let mut pending: Vec<_> = selected.iter().cloned().collect();
        while let Some(path) = pending.pop() {
            for (dataset, names) in datasets.iter().zip(&namespaces) {
                let Ok(array) = dataset.array(&path) else {
                    continue;
                };
                let links = Links::read(dataset, &array, names)?;
                if let Some(template) = plan.links.get(&path) {
                    if !template.compatible(&links) {
                        return Err(bad(format!(
                            "incompatible array '{path}': CF bounds, coordinates or grid mapping reference encoding metadata differ"
                        )));
                    }
                } else {
                    plan.links.insert(path.clone(), links);
                }
            }
            let links = plan.links.get(&path).expect("selected array exists in a source");
            for dependency in links.coordinates.iter().chain(links.bounds.iter()) {
                if selected.insert(dependency.clone()) {
                    pending.push(dependency.clone());
                }
            }
            for mapping in &links.mappings {
                let target = &mapping.array.info.path;
                if let Some(previous) = plan.grids.get(target) {
                    if !same_value(&previous.info.attributes, &mapping.array.info.attributes) {
                        return Err(bad(format!("conflicting CF grid mapping metadata at '{target}'")));
                    }
                } else {
                    plan.grids.insert(target.clone(), mapping.array.clone());
                }
            }
        }
        Ok(plan)
    }
    pub(super) fn apply(&self, root: &mut Group) {
        let paths = root.arrays_recursive().iter().map(|a| a.info.path.clone()).collect();
        self.apply_group(root, &paths);
    }
    fn apply_group(&self, root: &mut Group, paths: &BTreeSet<String>) {
        for array in &mut root.arrays {
            let Some(links) = self.links.get(&array.info.path) else {
                continue;
            };
            let name = |target: &str| reference_name(&array.info.path, target, paths);
            array.info.coordinates.clone_from(&links.coordinates);
            if let Some(coordinates) = &links.explicit_coordinates {
                array.info.attributes["coordinates"] = Value::String(coordinates.iter().map(|p| name(p)).collect::<Vec<_>>().join(" "));
            }
            if let Some(bounds) = &links.bounds {
                array.info.attributes["bounds"] = Value::String(name(bounds));
            }
            if !links.mappings.is_empty() {
                let mapping = if links.extended_mapping {
                    links
                        .mappings
                        .iter()
                        .map(|m| {
                            format!("{}: {}", name(&m.array.info.path), m.coordinates.iter().map(|p| name(p)).collect::<Vec<_>>().join(" "))
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                } else {
                    name(&links.mappings[0].array.info.path)
                };
                array.info.attributes["grid_mapping"] = Value::String(mapping);
            }
        }
        for group in &mut root.groups {
            self.apply_group(group, paths);
        }
    }
}

// Keep a local name when proximity resolves it unambiguously. An absolute CF
// path prevents newly introduced groups from rebinding a reference. Bare names
// also retain compatibility with GDAL 3.13's limited coordinate-variable lookup.
fn reference_name(owner: &str, target: &str, paths: &BTreeSet<String>) -> String {
    let name = target.rsplit('/').next().expect("absolute array path");
    let mut group = parent(owner);
    loop {
        let candidate = format!("{}/{}", group.trim_end_matches('/'), name);
        if paths.contains(&candidate) {
            return if candidate == target { name.into() } else { target.into() };
        }
        if group == "/" {
            return target.into();
        }
        group = parent(group);
    }
}

//! The HECRAS vector driver: detection of HEC-RAS files and the layers of an open, with the
//! joins [`HecRasOpenOptions`](crate::HecRasOpenOptions) asks for.

use super::families::FAMILY_LAYERS;
use super::{
    CrossSectionOptions, CrossSectionTable, Extreme, FeatureSeriesOptions, GeometryPolicy, HecRasHdf, MESH_CELL_POINTS, MESH_CELLS,
    MESH_FACE_POINTS, MESH_FACES, MESH_PERIMETERS, MeshAttribute, MeshOptions, ResultMode,
};
use crate::{Error, HecRasOpenOptions, Layer, Result, VectorOpenOptions};
use ras_commander_fast_hdf::hdf::metadata::HEC_RAS_FILE_TYPES;
use ras_commander_fast_hdf::hdf::{DEFAULT_MAX_BYTES, DEFAULT_READ_MAX_BYTES};
use std::path::Path;

/// Whether `path` is a HEC-RAS geometry or plan HDF file: an HDF5 signature (at offset 0, 512,
/// 1024, ...) and a root `File Type` in [`HEC_RAS_FILE_TYPES`], a single fixed-length string as
/// HEC-RAS writes it. Only that attribute is decoded, and only up to 4 KiB, so detection decodes
/// no other root metadata. Any other HDF5 file, including NetCDF-4 files and HEC terrain and
/// land-cover files, is left to the HDF5 and NetCDF drivers.
///
/// `File Type` says only that the file is HEC-RAS's: which kind it is does not follow from it
/// (plans before 6.4 can say `HEC-RAS Geometry`, and 6.4+ geometry files say `HEC-RAS Results`),
/// so the layers come from what the file stores, never from `File Type`.
pub(crate) fn is_hecras(path: &str) -> bool {
    crate::drivers::hdf5::has_signature(Path::new(path))
        && crate::drivers::hdf5::open_file(Path::new(path))
            .ok()
            .and_then(|f| crate::drivers::hdf5::root_file_type(&f))
            .is_some_and(|t| HEC_RAS_FILE_TYPES.contains(&t.as_str()))
}

/// Whether bytes held in memory are a HEC-RAS geometry or plan file: an HDF5 signature and a
/// root `File Type` in [`HEC_RAS_FILE_TYPES`], as [`is_hecras`] decides for files. Bytes without
/// an HDF5 signature are rejected without a copy; the metadata of HDF5 bytes is read from a
/// temporary copy (`hdf5::root_file_type_in`), freed before the file is opened.
pub(crate) fn is_hecras_bytes(bytes: &[u8]) -> bool {
    crate::drivers::hdf5::root_file_type_in(bytes).is_some_and(|t| HEC_RAS_FILE_TYPES.contains(&t.as_str()))
}

/// Reads the layers `options` selects with the joins `options.hecras` asks for.
///
/// The options are validated first: an invalid request (a series without `TIME_STEP`, an unknown
/// table name) is an error, whichever layers are read. Named layers must all read: any refusal
/// is the open's error. With no layer named, every stored layer is read, and a layer the engine
/// refuses (an unsupported layout, or a table it needs that the file does not store, such as the
/// ghost-cell count of HEC-RAS 5.0 files) is left out with a warning ([`crate::warnings`]),
/// unless an option asks for something of that layer (a join, `MESHES`): then its refusal is the
/// open's error too, and an option whose layers the file does not store at all is an error.
/// Damaged data and I/O errors always fail the open. The caller's byte budgets bound the root
/// attributes, listing and every layer read.
pub(crate) fn read(path: &str, options: &VectorOpenOptions) -> Result<Vec<Layer>> {
    read_hdf(&HecRasHdf::open(path)?, path, options)
}

/// Reads a HEC-RAS file held in memory as [`read`] reads files. `name` names the file in
/// messages and identities.
pub(crate) fn read_bytes(bytes: Vec<u8>, name: &str, options: &VectorOpenOptions) -> Result<Vec<Layer>> {
    read_hdf(&HecRasHdf::from_bytes(bytes, name)?, name, options)
}

fn read_hdf(hdf: &HecRasHdf, path: &str, options: &VectorOpenOptions) -> Result<Vec<Layer>> {
    let o = &options.hecras;
    check_rows(o)?;
    // Every layer's request is built before anything is read, so that an invalid request fails
    // the open instead of leaving layers out.
    let requests = LAYER_NAMES.iter().map(|name| Ok((*name, request(name, o)?))).collect::<Result<Vec<_>>>()?;
    let request_of = |name: &str| requests.iter().find(|(n, _)| *n == name).map(|(_, r)| r);
    let (metadata_max_bytes, max_bytes) = limits(o);
    hdf.root_attributes_within(metadata_max_bytes)?;
    if !options.layers.is_empty() {
        // Only the named layers are looked up; the whole listing is read only to name the
        // layers in an error. A name given twice gives its layer once.
        let mut layers = Vec::with_capacity(options.layers.len());
        let mut seen = std::collections::HashSet::new();
        for name in options.layers.iter().filter(|name| seen.insert(name.as_str())) {
            match request_of(name) {
                Some(r) if hdf.layer_stored_within(name, metadata_max_bytes, max_bytes)? => layers.push(r.read(hdf, name)?),
                _ => {
                    let available = hdf.layer_names_within(metadata_max_bytes, max_bytes)?;
                    return Err(Error::InvalidArgument(format!("no layer named {name} (layers: {})", available.join(", "))));
                }
            }
        }
        return Ok(layers);
    }
    let available = hdf.layer_names_within(metadata_max_bytes, max_bytes)?;
    if available.is_empty() {
        return Err(Error::Unsupported(format!("'{path}' stores no layer the HECRAS driver reads")));
    }
    // An option for layers the file does not store would otherwise be dropped unnoticed.
    if let Some((option, targets)) = option_targets(o).into_iter().find(|(_, t)| !t.iter().any(|t| available.iter().any(|a| a == t))) {
        return Err(Error::InvalidArgument(format!(
            "{option} applies to {}, which '{path}' does not store (layers: {})",
            targets.join(" and "),
            available.join(", ")
        )));
    }
    let mut layers = Vec::with_capacity(available.len());
    for name in &available {
        let r = request_of(name).ok_or_else(|| Error::InvalidData(format!("HECRAS: no request for listed layer {name}")))?;
        match r.read(hdf, name) {
            Ok(layer) => layers.push(layer),
            Err(e @ (Error::Unsupported(_) | Error::InvalidArgument(_))) if !r.targeted => {
                crate::warnings::warn(format!("HECRAS: layer {name} left out: {e} (name it with a layer option to see it fail)"))
            }
            Err(e) => return Err(e),
        }
    }
    if layers.is_empty() {
        return Err(Error::Unsupported(format!("'{path}': the engine refused every HEC-RAS layer (see the warnings)")));
    }
    Ok(layers)
}

/// Refuses a row option nothing reads: `TIME_STEP` without a series option, and `XS_PROFILE`
/// without `XS_RESULTS` or together with `TIME_STEP` (the results are read at one row, steady or
/// unsteady). Checked before anything is read, whichever layers are named.
fn check_rows(o: &HecRasOpenOptions) -> Result<()> {
    if o.xs_profile.is_some() && o.xs_results.is_empty() {
        return Err(Error::InvalidArgument("XS_PROFILE selects the steady profile of XS_RESULTS, which is not set".into()));
    }
    if o.xs_profile.is_some() && o.time_step.is_some() {
        return Err(Error::InvalidArgument("XS_RESULTS are read at TIME_STEP (unsteady) or XS_PROFILE (steady), not both".into()));
    }
    let series = !(o.cell_series.is_empty()
        && o.face_series.is_empty()
        && o.xs_results.is_empty()
        && o.structure_series.is_empty()
        && !o.bc_series
        && o.reference_line_series.is_empty()
        && o.reference_point_series.is_empty());
    if o.time_step.is_some() && !series {
        return Err(Error::InvalidArgument(
            "TIME_STEP selects the output row of a series option (CELL_SERIES, FACE_SERIES, XS_RESULTS, STRUCTURE_SERIES, BC_SERIES, REFERENCE_LINE_SERIES, REFERENCE_POINT_SERIES), and none is set"
                .into(),
        ));
    }
    Ok(())
}

/// The options set in `o` that ask for something of particular layers, with those layers. An
/// option applies when any of its layers is read.
fn option_targets(o: &HecRasOpenOptions) -> Vec<(&'static str, &'static [&'static str])> {
    const CELLS: &[&str] = &[MESH_CELLS, MESH_CELL_POINTS];
    const MESH: &[&str] = &[MESH_CELLS, MESH_CELL_POINTS, MESH_FACES, MESH_FACE_POINTS, MESH_PERIMETERS];
    let lists: [(&str, bool, &[&str]); 19] = [
        ("MESHES", !o.meshes.is_empty(), MESH),
        ("GEOMETRY_POLICY=QUARANTINE", o.quarantine, &[MESH_CELLS]),
        ("CELL_SUMMARIES", !o.cell_summaries.is_empty(), CELLS),
        ("CELL_ATTRIBUTES", !o.cell_attributes.is_empty(), CELLS),
        ("CELL_SERIES", !o.cell_series.is_empty(), CELLS),
        ("CELL_MAXIMUM", !o.cell_maximum.is_empty(), CELLS),
        ("CELL_MINIMUM", !o.cell_minimum.is_empty(), CELLS),
        ("FACE_SUMMARIES", !o.face_summaries.is_empty(), &[MESH_FACES]),
        ("FACE_ATTRIBUTES", !o.face_attributes.is_empty(), &[MESH_FACES]),
        ("FACE_SERIES", !o.face_series.is_empty(), &[MESH_FACES]),
        ("FACE_MAXIMUM", !o.face_maximum.is_empty(), &[MESH_FACES]),
        ("FACE_MINIMUM", !o.face_minimum.is_empty(), &[MESH_FACES]),
        ("XS_SUMMARIES", !o.xs_summaries.is_empty(), &["cross_sections"]),
        ("XS_RESULTS", !o.xs_results.is_empty(), &["cross_sections"]),
        ("XS_GEOMETRY", !o.xs_geometry.is_empty(), &["cross_sections"]),
        ("STRUCTURE_SERIES", !o.structure_series.is_empty(), &["structures"]),
        ("BC_SERIES", o.bc_series, &["bc_lines"]),
        ("REFERENCE_LINE_SERIES", !o.reference_line_series.is_empty(), &["reference_lines"]),
        ("REFERENCE_POINT_SERIES", !o.reference_point_series.is_empty(), &["reference_points"]),
    ];
    lists.into_iter().filter(|(_, set, _)| *set).map(|(name, _, layers)| (name, layers)).collect()
}

/// Every layer name the driver can give, in listing order.
const LAYER_NAMES: &[&str] = &[
    MESH_CELLS,
    MESH_CELL_POINTS,
    MESH_FACES,
    MESH_FACE_POINTS,
    MESH_PERIMETERS,
    "cross_sections",
    "river_centerlines",
    "river_bank_lines",
    "river_edge_lines",
    "river_flow_paths",
    "storage_areas",
    "structures",
    "bc_lines",
    "breaklines",
    "refinement_regions",
    "reference_lines",
    "reference_points",
    "river_junctions",
    "pump_stations",
    "pipe_conduits",
    "pipe_nodes",
];

/// One layer's validated read.
struct Request {
    read: LayerRead,
    /// Whether an option asks for something of this layer: its refusal fails a default open.
    targeted: bool,
}

enum LayerRead {
    Cells(MeshOptions),
    CellPoints(MeshOptions),
    Faces(MeshOptions),
    FacePoints(MeshOptions),
    Perimeters(MeshOptions),
    CrossSections(CrossSectionOptions),
    Structures(FeatureSeriesOptions),
    BcLines(FeatureSeriesOptions),
    ReferenceLines(FeatureSeriesOptions),
    ReferencePoints(FeatureSeriesOptions),
    /// A family layer with its stored attributes only, within these byte budgets (metadata,
    /// arrays).
    Family(usize, usize),
}

impl Request {
    fn read(&self, hdf: &HecRasHdf, name: &str) -> Result<Layer> {
        Ok(match &self.read {
            LayerRead::Cells(m) => hdf.mesh_cells(m)?.layer,
            LayerRead::CellPoints(m) => hdf.mesh_cell_points(m)?.layer,
            LayerRead::Faces(m) => hdf.mesh_faces(m)?.layer,
            LayerRead::FacePoints(m) => hdf.mesh_face_points(m)?.layer,
            LayerRead::Perimeters(m) => hdf.mesh_perimeters(m)?.layer,
            LayerRead::CrossSections(x) => hdf.cross_sections(x)?.layer,
            LayerRead::Structures(f) => hdf.structures_at(f)?.layer,
            LayerRead::BcLines(f) => hdf.bc_lines_at(f)?.layer,
            LayerRead::ReferenceLines(f) => hdf.reference_lines_at(f)?.layer,
            LayerRead::ReferencePoints(f) => hdf.reference_points_at(f)?.layer,
            LayerRead::Family(metadata, arrays) => {
                let (_, family) = FAMILY_LAYERS
                    .iter()
                    .find(|(n, _)| *n == name)
                    .ok_or_else(|| Error::InvalidArgument(format!("no HEC-RAS layer named {name:?}")))?;
                hdf.family_layer(name, *family, *metadata, *arrays)?
            }
        })
    }
}

/// The caller's byte budgets (metadata objects, arrays), or the engine's defaults.
fn limits(o: &HecRasOpenOptions) -> (usize, usize) {
    (o.metadata_max_bytes.unwrap_or(DEFAULT_MAX_BYTES), o.max_bytes.unwrap_or(DEFAULT_READ_MAX_BYTES))
}

/// Validates the options for one layer and builds its read. Every error is the caller's
/// (`InvalidArgument`); nothing is read.
fn request(name: &str, o: &HecRasOpenOptions) -> Result<Request> {
    let series_row =
        || o.time_step.ok_or_else(|| Error::InvalidArgument(format!("{name}: the series options need TIME_STEP (a stored output row)")));
    let feature_series = |variables: &[String]| -> Result<FeatureSeriesOptions> {
        let mut f = FeatureSeriesOptions::at_row(series_row()?);
        f.variables = variables.to_vec();
        budgets(o, &mut f.metadata_max_bytes, &mut f.max_bytes);
        Ok(f)
    };
    let meshes_named = !o.meshes.is_empty();
    let (read, targeted) = match name {
        MESH_CELLS | MESH_CELL_POINTS | MESH_FACES => {
            let m = mesh_options(o, name != MESH_FACES)?;
            let joins = !(m.summaries.is_empty() && m.attributes.is_empty() && m.series.is_empty() && m.extremes.is_empty());
            let read = match name {
                MESH_CELLS => LayerRead::Cells(m),
                MESH_CELL_POINTS => LayerRead::CellPoints(m),
                _ => LayerRead::Faces(m),
            };
            (read, joins || meshes_named)
        }
        MESH_FACE_POINTS => (LayerRead::FacePoints(base_mesh_options(o)), meshes_named),
        MESH_PERIMETERS => (LayerRead::Perimeters(base_mesh_options(o)), meshes_named),
        "cross_sections" if !(o.xs_summaries.is_empty() && o.xs_results.is_empty() && o.xs_geometry.is_empty()) => {
            let mut x = CrossSectionOptions { summaries: o.xs_summaries.clone(), ..Default::default() };
            for g in &o.xs_geometry {
                x.geometry.push(match g.as_str() {
                    "STATION_ELEVATION" => CrossSectionTable::StationElevation,
                    "MANNINGS_N" => CrossSectionTable::ManningsN,
                    "INEFFECTIVE_AREAS" => CrossSectionTable::IneffectiveAreas,
                    other => return Err(Error::InvalidArgument(format!("unknown XS_GEOMETRY value '{other}'"))),
                });
            }
            if !o.xs_results.is_empty() {
                x.results = o.xs_results.clone();
                (x.mode, x.row) = match (o.xs_profile, o.time_step) {
                    (Some(p), _) => (ResultMode::Steady, Some(p)),
                    (None, Some(t)) => (ResultMode::Unsteady, Some(t)),
                    (None, None) => {
                        return Err(Error::InvalidArgument("XS_RESULTS need TIME_STEP (unsteady) or XS_PROFILE (steady)".into()));
                    }
                };
            }
            budgets(o, &mut x.metadata_max_bytes, &mut x.max_bytes);
            (LayerRead::CrossSections(x), true)
        }
        "structures" if !o.structure_series.is_empty() => (LayerRead::Structures(feature_series(&o.structure_series)?), true),
        "bc_lines" if o.bc_series => (LayerRead::BcLines(feature_series(&[])?), true),
        "reference_lines" if !o.reference_line_series.is_empty() => {
            (LayerRead::ReferenceLines(feature_series(&o.reference_line_series)?), true)
        }
        "reference_points" if !o.reference_point_series.is_empty() => {
            (LayerRead::ReferencePoints(feature_series(&o.reference_point_series)?), true)
        }
        other if FAMILY_LAYERS.iter().any(|(n, _)| *n == other) => {
            let (metadata, arrays) = limits(o);
            (LayerRead::Family(metadata, arrays), false)
        }
        other => return Err(Error::InvalidArgument(format!("no HEC-RAS layer named {other:?}"))),
    };
    Ok(Request { read, targeted })
}

fn budgets(o: &HecRasOpenOptions, metadata: &mut usize, arrays: &mut usize) {
    if let Some(m) = o.metadata_max_bytes {
        *metadata = m;
    }
    if let Some(m) = o.max_bytes {
        *arrays = m;
    }
}

fn base_mesh_options(o: &HecRasOpenOptions) -> MeshOptions {
    let mut m = MeshOptions::default();
    if !o.meshes.is_empty() {
        m.meshes = Some(o.meshes.clone());
    }
    if o.quarantine {
        m.policy = GeometryPolicy::QuarantineInvalid;
    }
    budgets(o, &mut m.metadata_max_bytes, &mut m.max_bytes);
    m
}

/// The mesh options of the cell layers (`cells`) or the face layer.
fn mesh_options(o: &HecRasOpenOptions, cells: bool) -> Result<MeshOptions> {
    let mut m = base_mesh_options(o);
    let (summaries, attributes, series, maximum, minimum) = if cells {
        (&o.cell_summaries, &o.cell_attributes, &o.cell_series, &o.cell_maximum, &o.cell_minimum)
    } else {
        (&o.face_summaries, &o.face_attributes, &o.face_series, &o.face_maximum, &o.face_minimum)
    };
    m.summaries = summaries.clone();
    for a in attributes {
        m.attributes.push(match a.as_str() {
            "MINIMUM_ELEVATION" => MeshAttribute::MinimumElevation,
            "MANNINGS_N" => MeshAttribute::ManningsN,
            "NORMAL_AND_LENGTH" => MeshAttribute::FaceNormalAndLength,
            "CELLS" => MeshAttribute::FaceCells,
            other => return Err(Error::InvalidArgument(format!("unknown mesh attribute '{other}'"))),
        });
    }
    if !series.is_empty() {
        m.series = series.clone();
        m.time_step = Some(
            o.time_step.ok_or_else(|| Error::InvalidArgument("CELL_SERIES and FACE_SERIES need TIME_STEP (a stored output row)".into()))?,
        );
    }
    m.extremes =
        maximum.iter().map(|v| (v.clone(), Extreme::Maximum)).chain(minimum.iter().map(|v| (v.clone(), Extreme::Minimum))).collect();
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_without_layers_says_so() {
        // No `Geometry/2D Flow Areas` and no family: nothing to read, and nothing was refused.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/hecras/coverage_mesh_nogeometry.h5");
        let e = read(path, &VectorOpenOptions::default()).unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("stores no layer")), "{e}");
    }
}

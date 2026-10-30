//! HEC-RAS geometry (`.g##.hdf`) and plan (`.p##.hdf`) HDF files as terrio vector layers.
//!
//! This module is a thin adapter: it opens the file with the
//! [`ras-commander-fast-hdf`](https://crates.io/crates/ras-commander-fast-hdf) engine (the one
//! ras-commander uses) and translates what the engine returns into terrio's [`Layer`],
//! [`crate::FieldDef`] and [`crate::Error`] types. It holds no HEC-RAS rule of its own:
//!
//! - **Geometry** is the engine's: packed cell polygons in its ring order (clockwise from the
//!   first listed face), face polylines, stored perimeters and family parts. Closing vertices are
//!   dropped because terrio rings store none. Cells the engine omits (ghost cells, cells with
//!   fewer than three faces) are reported in [`MeshLayer::omitted`], never invented.
//! - **Identities** are the engine's: `mesh_name` (the exact 2D flow area name) with `cell_id`,
//!   `face_id` or `face_point_id` (stored indices that repeat across meshes), and `source_row`
//!   (the stored attribute-table row) with the stored attribute columns for the other families.
//! - **Attributes** are the stored ones, read without h5py: every column of the stored
//!   attribute tables (compound `Attributes` datasets, fixed-length strings decoded as h5py
//!   decodes them), the 2D flow area table on `mesh_perimeters`, cross-section station-elevation,
//!   Manning's n and ineffective areas as JSON fields ([`CrossSectionOptions::geometry`]), and the
//!   root file attributes ([`HecRasHdf::file_attributes`]).
//! - **CRS** is the stored `Projection` text, attached as WKT. No EPSG code is guessed. A file
//!   without one gives layers without a CRS; conflicting, malformed, unsupported or corrupt
//!   projection evidence is an error ([`HecRasHdf::crs`]).
//! - **Results** are stored values, unconverted: a summary such as `Maximum Water Surface` becomes
//!   a float field, and its stored time row (when the engine proves one) a second field. Static
//!   mesh arrays, time-step values and the engine's series extremes join to mesh layers
//!   ([`MeshOptions`]); 1D summaries and results join to `cross_sections` by the engine's
//!   validated cross-section identity ([`HecRasHdf::cross_sections`]).
//! - **Refusals** (unsupported, corrupt, absent, over budget) are errors (see [`error`]), never
//!   empty layers or partial results.
//!
//! ```no_run
//! # fn main() -> Result<(), terrio::Error> {
//! use terrio::hecras::{HecRasHdf, MeshOptions};
//!
//! let hdf = HecRasHdf::open("Muncie.p04.hdf")?;
//! println!("layers: {:?}", hdf.layer_names()?);
//! let cells = hdf.mesh_cells(&MeshOptions::default().with_summary("Maximum Water Surface"))?;
//! println!("{}: {} cells, CRS {:?}", cells.layer.name, cells.layer.len(), cells.layer.crs);
//! let sections = hdf.read_layer("cross_sections")?;
//! # Ok(())
//! # }
//! ```

pub(crate) mod driver;
pub mod error;
mod families;
mod mesh;
mod one_d;
mod series;

pub use mesh::{Extreme, JoinedField, MeshAttribute, MeshLayer, MeshOptions, OmittedCell, SummaryField, TimeStep};
pub use one_d::{CrossSectionField, CrossSectionLayer, CrossSectionOptions, CrossSectionTable};
pub use ras_commander_fast_hdf::hdf::ResultMode;
pub use ras_commander_fast_hdf::mesh::{GeometryPolicy, OmissionKind};
pub use series::{FeatureSeriesOptions, SeriesField, SeriesLayer};

use crate::Layer;
use ras_commander_fast_hdf::crs::EvidenceStatus;
use ras_commander_fast_hdf::hdf::{DEFAULT_MAX_BYTES, DEFAULT_READ_MAX_BYTES, ReadOnlyHdfSession};
use std::path::{Path, PathBuf};

/// The engine contract this crate is written against: API 10 fixes the ring order of mesh cell
/// polygons (clockwise from the first listed face), identical to ras-commander's Python path.
pub const REQUIRED_ENGINE_API: u32 = 10;

/// The engine's `API_VERSION`.
pub fn engine_api_version() -> u32 {
    ras_commander_fast_hdf::api::API_VERSION
}

/// The 2D mesh cell polygon layer ([`HecRasHdf::mesh_cells`]).
pub const MESH_CELLS: &str = "mesh_cells";
/// The 2D mesh cell centre point layer ([`HecRasHdf::mesh_cell_points`]).
pub const MESH_CELL_POINTS: &str = "mesh_cell_points";
/// The 2D mesh face line layer ([`HecRasHdf::mesh_faces`]).
pub const MESH_FACES: &str = "mesh_faces";
/// The 2D mesh face point (corner) layer ([`HecRasHdf::mesh_face_points`]).
pub const MESH_FACE_POINTS: &str = "mesh_face_points";
/// The 2D flow area perimeter polygon layer ([`HecRasHdf::mesh_perimeters`]).
pub const MESH_PERIMETERS: &str = "mesh_perimeters";
/// The mesh layers, in listing order.
pub const MESH_LAYERS: &[&str] = &[MESH_CELLS, MESH_CELL_POINTS, MESH_FACES, MESH_FACE_POINTS, MESH_PERIMETERS];

/// The names of the geometry family layers ([`HecRasHdf::read_layer`]): `cross_sections`,
/// `river_centerlines`, `river_bank_lines`, `river_edge_lines`, `river_flow_paths`,
/// `storage_areas`, `structures`, `bc_lines`, `breaklines`, `refinement_regions`,
/// `reference_lines`, `reference_points`, `river_junctions`, `pump_stations`, `pipe_conduits`
/// and `pipe_nodes`.
pub fn family_layer_names() -> impl Iterator<Item = &'static str> {
    families::FAMILY_LAYERS.iter().map(|(name, _)| *name)
}

/// A non-nullable identity field.
pub(crate) fn id_field(name: &str, kind: crate::FieldType) -> crate::FieldDef {
    let mut f = crate::FieldDef::new(name, kind);
    f.nullable = false;
    f
}

/// A field name for a stored name: lower case, runs of other characters as `_`.
pub(crate) fn field_name(stored: &str) -> String {
    let mut out = String::new();
    for c in stored.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    out.trim_matches('_').to_string()
}

/// Stored numbers as field values: floats (`f32` widened exactly) or integers.
pub(crate) fn numbers(values: ras_commander_fast_hdf::hdf::NumericValues, what: &str) -> crate::Result<Vec<crate::FieldValue>> {
    use crate::FieldValue as V;
    use ras_commander_fast_hdf::hdf::NumericValues as N;
    let int = V::Integer;
    Ok(match values {
        N::F32(v) => v.into_iter().map(|x| V::Float(f64::from(x))).collect(),
        N::F64(v) => v.into_iter().map(V::Float).collect(),
        N::I8(v) => v.into_iter().map(|x| int(x.into())).collect(),
        N::I16(v) => v.into_iter().map(|x| int(x.into())).collect(),
        N::I32(v) => v.into_iter().map(|x| int(x.into())).collect(),
        N::I64(v) => v.into_iter().map(int).collect(),
        N::U8(v) => v.into_iter().map(|x| int(x.into())).collect(),
        N::U16(v) => v.into_iter().map(|x| int(x.into())).collect(),
        N::U32(v) => v.into_iter().map(|x| int(x.into())).collect(),
        N::U64(v) => v
            .into_iter()
            .map(|x| i64::try_from(x).map(int).map_err(|_| crate::Error::Unsupported(format!("{what}: value above i64"))))
            .collect::<crate::Result<_>>()?,
        _ => return Err(crate::Error::Unsupported(format!("{what}: unmapped numeric kind"))),
    })
}

/// An error unless the engine gave `values` values for `rows` rows of `elements` values each, so
/// that slicing them by row cannot panic.
pub(crate) fn check_value_count(values: usize, rows: u64, elements: u64, what: &str) -> crate::Result<()> {
    if rows.checked_mul(elements) != Some(values as u64) {
        return Err(crate::Error::InvalidData(format!("{what}: {values} values for {rows} rows of {elements}")));
    }
    Ok(())
}

/// The field type of values from [`numbers`] (all share one).
pub(crate) fn kind_of(values: &[crate::FieldValue]) -> crate::FieldType {
    match values.first() {
        Some(crate::FieldValue::Integer(_)) => crate::FieldType::Integer,
        _ => crate::FieldType::Float,
    }
}

/// A HEC-RAS geometry or plan HDF file, opened read-only.
///
/// The file is memory-mapped by the engine and must not change while this value lives.
pub struct HecRasHdf {
    session: ReadOnlyHdfSession,
    path: PathBuf,
    /// The stored bytes of the root attributes' values, measured at open without decoding them:
    /// every read of a root attribute is checked against a metadata budget before the engine
    /// decodes any of them.
    root_attribute_bytes: usize,
}

impl HecRasHdf {
    /// Opens a geometry (`.g##.hdf`) or plan (`.p##.hdf`) HDF file.
    pub fn open(path: impl AsRef<Path>) -> crate::Result<Self> {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(8));
        Self::open_with_threads(path, threads)
    }

    /// Opens the file with `threads` engine workers (1 to 1024) for polygon assembly.
    pub fn open_with_threads(path: impl AsRef<Path>, threads: usize) -> crate::Result<Self> {
        const _: () = assert!(ras_commander_fast_hdf::api::API_VERSION >= REQUIRED_ENGINE_API);
        let path = path.as_ref().to_path_buf();
        let session = ReadOnlyHdfSession::with_options(&path, threads, threads > 1).map_err(error::from_engine)?;
        let root_attribute_bytes = crate::drivers::hdf5::root_attribute_bytes(&crate::drivers::hdf5::open_file(&path)?, &path)?;
        Ok(Self { session, path, root_attribute_bytes })
    }

    /// Opens a geometry or plan HDF file held in memory, such as a download or an upload (the only
    /// way to open one in WebAssembly). `name` stands for the path: it names the file in
    /// identities and messages. The whole file stays in memory while it is open, and opening
    /// briefly needs about twice its size; the engine's byte budgets apply to every read as for
    /// files. In WebAssembly (wasm32) memory is limited to 4 GiB in all.
    ///
    /// ```no_run
    /// let bytes = std::fs::read("BaldEagleDamBrk.p03.hdf")?;
    /// let hdf = terrio::hecras::HecRasHdf::from_bytes(bytes, "BaldEagleDamBrk.p03.hdf")?;
    /// println!("{:?}", hdf.layer_names()?);
    /// # Ok::<(), Box<dyn std::error::Error>>(())
    /// ```
    pub fn from_bytes(bytes: Vec<u8>, name: &str) -> crate::Result<Self> {
        let threads = std::thread::available_parallelism().map_or(1, |n| n.get().min(8));
        let path = PathBuf::from(name);
        let root_attribute_bytes = crate::drivers::hdf5::root_attribute_bytes_in(&bytes, name)?;
        let session = ReadOnlyHdfSession::from_bytes(bytes, name, threads, threads > 1).map_err(error::from_engine)?;
        Ok(Self { session, path, root_attribute_bytes })
    }

    /// The file's path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The names of the layers this file can give: those whose data the file stores. Listing
    /// does not validate them: reading a layer can still refuse (unproven or damaged data).
    /// `mesh_perimeters` is listed when every 2D flow area stores a `Perimeter`. Listing reads
    /// within the engine's default byte budgets.
    pub fn layer_names(&self) -> crate::Result<Vec<String>> {
        self.layer_names_within(DEFAULT_MAX_BYTES, DEFAULT_READ_MAX_BYTES)
    }

    /// [`HecRasHdf::layer_names`] within a caller's byte budgets: no dataset larger than them is
    /// read to list a layer. A layer whose data is over budget is still listed, so that reading
    /// it shows the refusal.
    pub(crate) fn layer_names_within(&self, metadata_max_bytes: usize, max_bytes: usize) -> crate::Result<Vec<String>> {
        let mut names = Vec::new();
        if self.has_meshes()? {
            names.extend([MESH_CELLS, MESH_CELL_POINTS, MESH_FACES, MESH_FACE_POINTS].iter().map(|n| n.to_string()));
            if self.perimeters_stored(max_bytes)? {
                names.push(MESH_PERIMETERS.to_string());
            }
        }
        let budget = families::family_budget(metadata_max_bytes, max_bytes);
        for (name, family) in families::FAMILY_LAYERS {
            if self.family_present(*family, budget)? {
                names.push(name.to_string());
            }
        }
        Ok(names)
    }

    /// Whether the file stores layer `name`, as [`HecRasHdf::layer_names_within`] decides, from
    /// that layer's data only. A name this crate does not know is not stored.
    pub(crate) fn layer_stored_within(&self, name: &str, metadata_max_bytes: usize, max_bytes: usize) -> crate::Result<bool> {
        match name {
            MESH_CELLS | MESH_CELL_POINTS | MESH_FACES | MESH_FACE_POINTS => self.has_meshes(),
            MESH_PERIMETERS => Ok(self.has_meshes()? && self.perimeters_stored(max_bytes)?),
            _ => match families::FAMILY_LAYERS.iter().find(|(n, _)| *n == name) {
                Some((_, family)) => self.family_present(*family, families::family_budget(metadata_max_bytes, max_bytes)),
                None => Ok(false),
            },
        }
    }

    /// Reads a layer by name, with default options for mesh layers (every mesh, strict
    /// geometry, no joined summaries). A name this crate does not know, or a layer the file does
    /// not store, is [`crate::Error::InvalidArgument`].
    pub fn read_layer(&self, name: &str) -> crate::Result<Layer> {
        {
            let options = MeshOptions::default();
            match name {
                MESH_CELLS => return Ok(self.mesh_cells(&options)?.layer),
                MESH_CELL_POINTS => return Ok(self.mesh_cell_points(&options)?.layer),
                MESH_FACES => return Ok(self.mesh_faces(&options)?.layer),
                MESH_FACE_POINTS => return Ok(self.mesh_face_points(&options)?.layer),
                MESH_PERIMETERS => return Ok(self.mesh_perimeters(&options)?.layer),
                _ => {}
            }
        }
        match families::FAMILY_LAYERS.iter().find(|(n, _)| *n == name) {
            Some((name, family)) => self.family_layer(name, *family, DEFAULT_MAX_BYTES, DEFAULT_READ_MAX_BYTES),
            None => Err(crate::Error::InvalidArgument(format!("no HEC-RAS layer named {name:?}"))),
        }
    }

    /// The exact 2D flow area names, in the engine's order. A file without a `2D Flow Areas` group
    /// is an error ([`crate::Error::InvalidArgument`]), as is a legacy names-only layout
    /// ([`crate::Error::Unsupported`]).
    pub fn mesh_names(&self) -> crate::Result<Vec<String>> {
        self.session.mesh_names().map_err(error::from_engine)
    }

    fn perimeters_stored(&self, max_bytes: usize) -> crate::Result<bool> {
        let meshes = match self.mesh_names() {
            Ok(meshes) => meshes,
            // A mesh inventory the engine refuses: listed, so that reading it shows why.
            Err(crate::Error::Unsupported(_)) => return Ok(true),
            Err(e) => return Err(e),
        };
        for mesh in meshes {
            match self.session.read_mesh_perimeter(&mesh, max_bytes) {
                Ok(_) => {}
                Err(e) if e.kind == ras_commander_fast_hdf::ErrorKind::NotFound => return Ok(false),
                // Present but refused, over budget or damaged: listed, so that reading it shows why.
                Err(e) if !matches!(e.kind, ras_commander_fast_hdf::ErrorKind::Io | ras_commander_fast_hdf::ErrorKind::Internal) => {}
                Err(e) => return Err(error::from_engine(e)),
            }
        }
        Ok(true)
    }

    fn has_meshes(&self) -> crate::Result<bool> {
        match self.session.mesh_names() {
            Ok(names) => Ok(!names.is_empty()),
            Err(e) if e.kind == ras_commander_fast_hdf::ErrorKind::NotFound => Ok(false),
            // Stored but refused (for example 2D flow areas stored as outlines only, with no
            // computed mesh): listed, so that reading the mesh layers shows the refusal.
            Err(e) if e.kind == ras_commander_fast_hdf::ErrorKind::Unsupported => Ok(true),
            Err(e) => Err(error::from_engine(e)),
        }
    }

    /// Refuses when the root attributes' stored values are larger than `metadata_max_bytes`,
    /// before the engine decodes any of them (`Unsupported`, as the engine's byte budgets are).
    pub(crate) fn root_attributes_within(&self, metadata_max_bytes: usize) -> crate::Result<()> {
        if self.root_attribute_bytes > metadata_max_bytes {
            return Err(crate::Error::Unsupported(format!(
                "HEC-RAS unsupported(resource_limit): '{}': the root attributes store {} bytes, above the metadata budget of {metadata_max_bytes} bytes",
                self.path.display(),
                self.root_attribute_bytes
            )));
        }
        Ok(())
    }

    /// The stored CRS as a terrio layer CRS (WKT only, no EPSG code), or `None` when the file
    /// stores no projection.
    ///
    /// The engine reads the root `Projection` attribute. Malformed text, several disagreeing
    /// declarations, or an attribute it cannot decode are errors: a layer is never placed with a
    /// CRS the file does not state. Root attributes storing more than the engine's default
    /// metadata budget (64 MiB) are refused before they are decoded.
    pub fn crs(&self) -> crate::Result<Option<String>> {
        self.crs_within(DEFAULT_MAX_BYTES)
    }

    /// [`HecRasHdf::crs`] within a caller's metadata budget.
    pub(crate) fn crs_within(&self, metadata_max_bytes: usize) -> crate::Result<Option<String>> {
        self.root_attributes_within(metadata_max_bytes)?;
        let report = self.session.raw_crs_evidence().map_err(error::from_engine)?;
        match &report.status {
            EvidenceStatus::Unknown => Ok(None),
            // Lexically readable; every non-empty declaration has the same exact text.
            EvidenceStatus::Unvalidated => {
                Ok(report.evidence.iter().find(|e| e.status != EvidenceStatus::Unknown).map(|e| e.exact_wkt.clone()))
            }
            status => Err(crate::Error::Crs(match error::require(&status.capability(), "stored Projection") {
                Err(e) => e.to_string(),
                Ok(()) => "HEC-RAS stored Projection is not usable".to_string(),
            })),
        }
    }

    /// The root attributes of the file (`File Type`, `File Version`, `Units System`,
    /// `Projection`, ...), in the engine's order (not necessarily h5py's), each as its stored text: a string attribute as h5py
    /// decodes it, numbers in their shortest exact decimal form, several values joined by `, `.
    /// An attribute the engine cannot decode is an error, never skipped. Root attributes storing
    /// more than the engine's default metadata budget (64 MiB) are refused before they are
    /// decoded.
    ///
    /// ```no_run
    /// let hdf = terrio::hecras::HecRasHdf::open("Muncie.p04.hdf")?;
    /// for (name, value) in hdf.file_attributes()? {
    ///     println!("{name}: {value}");
    /// }
    /// # Ok::<(), terrio::Error>(())
    /// ```
    pub fn file_attributes(&self) -> crate::Result<Vec<(String, String)>> {
        use ras_commander_fast_hdf::hdf::metadata::MetadataValue as M;
        self.root_attributes_within(DEFAULT_MAX_BYTES)?;
        let attributes = self.session.typed_group_attributes("/").map_err(error::from_engine)?;
        attributes
            .into_iter()
            .map(|a| {
                let join = |v: Vec<String>| v.join(", ");
                let text = match a.value {
                    M::Strings(v) => join(v),
                    M::Signed(v) => join(v.iter().map(i64::to_string).collect()),
                    M::Unsigned(v) => join(v.iter().map(u64::to_string).collect()),
                    M::Float(v) => join(v.iter().map(f64::to_string).collect()),
                    M::Bitfield(v) => join(v.iter().map(|b| format!("0x{b:02x}")).collect()),
                    other => {
                        error::require(&other.capability(), &format!("root attribute {:?}", a.name))?;
                        return Err(crate::Error::Unsupported(format!("root attribute {:?}: unmapped value type", a.name)));
                    }
                };
                Ok((a.name, text))
            })
            .collect()
    }

    /// The stored hydraulic unit system text (the root `Units System` attribute, for example
    /// `US Customary`), exactly as stored; `None` when absent. An attribute the engine cannot
    /// decode, or a damaged one, is an error. Root attributes storing more than the engine's
    /// default metadata budget (64 MiB) are refused before they are decoded.
    pub fn units_system(&self) -> crate::Result<Option<String>> {
        self.root_attributes_within(DEFAULT_MAX_BYTES)?;
        let report = self.session.raw_crs_evidence().map_err(error::from_engine)?;
        for evidence in &report.hydraulic_units_evidence {
            error::require(&evidence.capability(), "stored Units System")?;
        }
        Ok(report.hydraulic_units)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn value_counts_must_fill_every_row() {
        assert!(super::check_value_count(6, 2, 3, "x").is_ok());
        assert!(super::check_value_count(3, 1, 3, "x").is_ok());
        for (values, rows, elements) in [(5, 2, 3), (7, 2, 3), (3, 2, 3), (1, u64::MAX, 2)] {
            let e = super::check_value_count(values, rows, elements, "x").unwrap_err();
            assert!(matches!(e, crate::Error::InvalidData(_)), "{values} {rows} {elements}: {e}");
        }
    }

    #[test]
    fn field_names_follow_stored_names() {
        assert_eq!(super::field_name("Maximum Water Surface"), "maximum_water_surface");
        assert_eq!(super::field_name("Maximum Face Velocity (ft/s)"), "maximum_face_velocity_ft_s");
        assert_eq!(super::field_name("SA-2D"), "sa_2d");
        assert_eq!(super::field_name("River Name"), "river_name");
    }
}

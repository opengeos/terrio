//! Open options of the HECRAS driver (HEC-RAS geometry and plan HDF files), by their GDAL-style
//! names. They are parsed in every build; reading needs the `hecras` feature.

use crate::{Error, Result};

/// The cell attributes `CELL_ATTRIBUTES` accepts.
pub(crate) const CELL_ATTRIBUTES: &[&str] = &["MINIMUM_ELEVATION", "MANNINGS_N"];
/// The face attributes `FACE_ATTRIBUTES` accepts.
pub(crate) const FACE_ATTRIBUTES: &[&str] = &["MINIMUM_ELEVATION", "NORMAL_AND_LENGTH", "CELLS"];
/// The stored cross-section tables `XS_GEOMETRY` accepts.
pub(crate) const XS_GEOMETRY: &[&str] = &["STATION_ELEVATION", "MANNINGS_N", "INEFFECTIVE_AREAS"];

/// How to read HEC-RAS geometry (`.g##.hdf`) and plan (`.p##.hdf`) files: which 2D flow areas,
/// and which stored values to join to the layers. Every joined value is a stored number, joined
/// by the engine's element or feature identity; a value the file does not store is an error.
///
/// Lists are comma-separated stored names, such as `CELL_SUMMARIES=Maximum Water Surface`.
/// An incomplete request (a series without `TIME_STEP`) is an error whichever layers are read.
/// With no layer named, a layer an option asks something of must read (its refusal fails the
/// open instead of leaving the layer out), and an option whose layers the file does not store is
/// an error; with layers named, options for the other layers are not looked up.
///
/// ```no_run
/// let mut options = terrio::VectorOpenOptions { layers: vec!["mesh_cells".into()], ..Default::default() };
/// options.hecras.set("CELL_SERIES", "Water Surface")?;
/// options.hecras.set("TIME_STEP", "12")?;
/// let plan = terrio::open_vector("BaldEagleDamBrk.p03.hdf", &options)?;
/// # Ok::<(), terrio::Error>(())
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct HecRasOpenOptions {
    /// `MESHES`: only these 2D flow areas (exact names). Empty means all.
    pub meshes: Vec<String>,
    /// `GEOMETRY_POLICY=STRICT|QUARANTINE`: whether an invalid cell polygon fails the read
    /// (`STRICT`, the default) or is left out of `mesh_cells` (`QUARANTINE`).
    pub quarantine: bool,
    /// `CELL_SUMMARIES`: stored `Summary Output` cell variables joined to `mesh_cells` and
    /// `mesh_cell_points` (`Maximum Water Surface`, ...).
    pub cell_summaries: Vec<String>,
    /// `FACE_SUMMARIES`: stored `Summary Output` face variables joined to `mesh_faces`
    /// (`Maximum Face Velocity`, ...).
    pub face_summaries: Vec<String>,
    /// `CELL_ATTRIBUTES`: stored static cell arrays, `MINIMUM_ELEVATION` and `MANNINGS_N`.
    pub cell_attributes: Vec<String>,
    /// `FACE_ATTRIBUTES`: stored static face arrays, `MINIMUM_ELEVATION`, `NORMAL_AND_LENGTH`
    /// and `CELLS` (the two stored cell indices).
    pub face_attributes: Vec<String>,
    /// `CELL_SERIES`: stored cell time series read at `TIME_STEP` (`Water Surface`, `Depth`, ...).
    pub cell_series: Vec<String>,
    /// `FACE_SERIES`: stored face time series read at `TIME_STEP` (`Face Velocity`, ...).
    pub face_series: Vec<String>,
    /// `CELL_MAXIMUM`, `CELL_MINIMUM`: stored cell time series reduced over every stored row,
    /// with the row of the extreme (`water_surface_max`, `water_surface_max_row`).
    pub cell_maximum: Vec<String>,
    /// See [`HecRasOpenOptions::cell_maximum`].
    pub cell_minimum: Vec<String>,
    /// `FACE_MAXIMUM`, `FACE_MINIMUM`: the same for face time series.
    pub face_maximum: Vec<String>,
    /// See [`HecRasOpenOptions::face_maximum`].
    pub face_minimum: Vec<String>,
    /// `TIME_STEP`: the stored unsteady output row (zero-based) of `CELL_SERIES`, `FACE_SERIES`,
    /// `XS_RESULTS`, `STRUCTURE_SERIES`, `BC_SERIES` and the reference series.
    pub time_step: Option<u64>,
    /// `XS_SUMMARIES`: stored `Summary Output/Cross Sections` variables joined to
    /// `cross_sections` (every labelled row).
    pub xs_summaries: Vec<String>,
    /// `XS_RESULTS`: stored cross-section result variables joined to `cross_sections`, at
    /// `TIME_STEP` (unsteady) or `XS_PROFILE` (steady).
    pub xs_results: Vec<String>,
    /// `XS_PROFILE`: the steady profile index (zero-based) of `XS_RESULTS`.
    pub xs_profile: Option<u64>,
    /// `XS_GEOMETRY`: stored per-section tables joined to `cross_sections` as JSON fields
    /// (`STATION_ELEVATION`, `MANNINGS_N`, `INEFFECTIVE_AREAS`).
    pub xs_geometry: Vec<String>,
    /// `STRUCTURE_SERIES`: stored structure series joined to `structures` at `TIME_STEP`
    /// (`Structure Variables`, ...).
    pub structure_series: Vec<String>,
    /// `BC_SERIES=YES`: each 2D boundary-condition line's stored series joined to `bc_lines` at
    /// `TIME_STEP`.
    pub bc_series: bool,
    /// `REFERENCE_LINE_SERIES`: stored reference-line variables joined to `reference_lines` at
    /// `TIME_STEP`.
    pub reference_line_series: Vec<String>,
    /// `REFERENCE_POINT_SERIES`: stored reference-point variables joined to `reference_points`
    /// at `TIME_STEP`.
    pub reference_point_series: Vec<String>,
    /// `METADATA_MAX_BYTES`: the engine's byte limit for each metadata object it consults
    /// (default 64 MiB). It bounds the root attributes (their stored values together, checked
    /// before any is decoded) and listing the layers too.
    pub metadata_max_bytes: Option<usize>,
    /// `MAX_BYTES`: the engine's byte limit for each array it reads (default 2 GiB). It bounds
    /// listing the layers too. The attribute tables and coordinate arrays of the 1D, 2D and
    /// network feature layers are bounded by the smaller of the two limits. A value above this
    /// platform's address space (4 GiB on wasm32) is an error, never truncated.
    pub max_bytes: Option<usize>,
}

impl HecRasOpenOptions {
    /// Sets an option by its name (see the fields). Returns `false` for names it does not know,
    /// and an error for a known name with an invalid value.
    ///
    /// A list value is split at every comma, so a stored name that contains a comma cannot be
    /// given. Setting a key again replaces its earlier value.
    pub fn set(&mut self, key: &str, value: &str) -> Result<bool> {
        let list = || value.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect::<Vec<_>>();
        let number = |what: &str| -> Result<u64> {
            value.trim().parse().map_err(|_| Error::InvalidArgument(format!("invalid {what} '{value}' (a non-negative integer)")))
        };
        // A byte budget this platform's address space cannot hold (above 4 GiB on wasm32) is an
        // error, never truncated to a smaller budget.
        let bytes = |what: &str| -> Result<usize> {
            usize::try_from(number(what)?).map_err(|_| {
                Error::InvalidArgument(format!("{what} '{value}' is above this platform's address space ({} bytes)", usize::MAX))
            })
        };
        let names = |allowed: &[&str], what: &str| -> Result<Vec<String>> {
            let v: Vec<String> = list().into_iter().map(|s| s.to_ascii_uppercase()).collect();
            match v.iter().find(|s| !allowed.contains(&s.as_str())) {
                Some(bad) => Err(Error::InvalidArgument(format!("unknown {what} '{bad}' ({})", allowed.join(", ")))),
                None => Ok(v),
            }
        };
        match key.to_ascii_uppercase().as_str() {
            "MESHES" => self.meshes = list(),
            "GEOMETRY_POLICY" => {
                self.quarantine = match value.trim().to_ascii_uppercase().as_str() {
                    "STRICT" => false,
                    "QUARANTINE" => true,
                    _ => return Err(Error::InvalidArgument(format!("unknown GEOMETRY_POLICY '{value}' (STRICT, QUARANTINE)"))),
                }
            }
            "CELL_SUMMARIES" => self.cell_summaries = list(),
            "FACE_SUMMARIES" => self.face_summaries = list(),
            "CELL_ATTRIBUTES" => self.cell_attributes = names(CELL_ATTRIBUTES, "CELL_ATTRIBUTES value")?,
            "FACE_ATTRIBUTES" => self.face_attributes = names(FACE_ATTRIBUTES, "FACE_ATTRIBUTES value")?,
            "CELL_SERIES" => self.cell_series = list(),
            "FACE_SERIES" => self.face_series = list(),
            "CELL_MAXIMUM" => self.cell_maximum = list(),
            "CELL_MINIMUM" => self.cell_minimum = list(),
            "FACE_MAXIMUM" => self.face_maximum = list(),
            "FACE_MINIMUM" => self.face_minimum = list(),
            "TIME_STEP" => self.time_step = Some(number("TIME_STEP")?),
            "XS_SUMMARIES" => self.xs_summaries = list(),
            "XS_RESULTS" => self.xs_results = list(),
            "XS_PROFILE" => self.xs_profile = Some(number("XS_PROFILE")?),
            "XS_GEOMETRY" => self.xs_geometry = names(XS_GEOMETRY, "XS_GEOMETRY value")?,
            "STRUCTURE_SERIES" => self.structure_series = list(),
            "BC_SERIES" => self.bc_series = crate::tiles::parse_bool(value)?,
            "REFERENCE_LINE_SERIES" => self.reference_line_series = list(),
            "REFERENCE_POINT_SERIES" => self.reference_point_series = list(),
            "METADATA_MAX_BYTES" => self.metadata_max_bytes = Some(bytes("METADATA_MAX_BYTES")?),
            "MAX_BYTES" => self.max_bytes = Some(bytes("MAX_BYTES")?),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Whether any option is set (a default open reads geometry and stored attributes only).
    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_parse_by_gdal_style_names() {
        let mut o = HecRasOpenOptions::default();
        assert!(o.set("cell_summaries", "Maximum Water Surface, Minimum Water Surface").unwrap());
        assert!(o.set("CELL_ATTRIBUTES", "minimum_elevation,MANNINGS_N").unwrap());
        assert!(o.set("TIME_STEP", "12").unwrap());
        assert!(o.set("BC_SERIES", "YES").unwrap());
        assert!(!o.set("ZOOM_LEVEL", "3").unwrap());
        assert_eq!(o.cell_summaries, ["Maximum Water Surface", "Minimum Water Surface"]);
        assert_eq!(o.cell_attributes, ["MINIMUM_ELEVATION", "MANNINGS_N"]);
        assert_eq!((o.time_step, o.bc_series), (Some(12), true));
        assert!(o.set("FACE_ATTRIBUTES", "MANNINGS_N").is_err());
        assert!(o.set("TIME_STEP", "-1").is_err());
        assert!(o.set("GEOMETRY_POLICY", "LOOSE").is_err());
    }

    #[test]
    fn byte_budgets_are_never_truncated() {
        let mut o = HecRasOpenOptions::default();
        assert!(o.set("MAX_BYTES", "1").unwrap());
        assert!(o.set("METADATA_MAX_BYTES", "4096").unwrap());
        assert_eq!((o.max_bytes, o.metadata_max_bytes), (Some(1), Some(4096)));
        let above = (u64::MAX).to_string();
        // u64::MAX fits a 64-bit usize; a budget above usize::MAX on 32-bit targets is refused.
        let fits = usize::try_from(u64::MAX).is_ok();
        assert_eq!(o.set("MAX_BYTES", &above).is_ok(), fits);
        if !fits {
            assert_eq!(o.max_bytes, Some(1), "a refused budget leaves the previous one");
        }
        assert!(o.set("MAX_BYTES", "18446744073709551616").is_err(), "above u64 is refused everywhere");
    }
}

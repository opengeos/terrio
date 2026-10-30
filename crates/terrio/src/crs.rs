// Written to match the behavior of:
//   GDAL v3.13.3:
//     ogr/ogrspatialreference.cpp, ogr/ogrct.cpp
// No code is translated from these sources.

//! Coordinate reference systems and transformations between them.
//!
//! Coordinates follow GDAL's traditional GIS order. East/north geographic axes are
//! normalized to longitude/latitude; west-positive planetary systems retain their
//! declared axis order and directions.
//!
//! ```no_run
//! use terrio::crs::{SpatialRef, Transformer};
//!
//! let nad27 = SpatialRef::parse("EPSG:4267")?;
//! let utm = SpatialRef::parse("+proj=utm +zone=13 +datum=WGS84")?;
//! println!("{} -> {}", nad27.label(), utm.label());
//! let t = Transformer::new(&nad27, &utm)?;
//! let (x, y) = t.transform(-105.5, 39.5)?;
//! let bounds = t.transform_bounds((-106.0, 39.0, -105.0, 40.0))?;
//! println!("{}", utm.to_wkt().unwrap_or_default());
//! # Ok::<(), terrio::Error>(())
//! ```
//!
//! Datum shift grids (NTv2, GeoTIFF) and geoid models are used when they can be found, as GDAL
//! uses them (see [`register_grid`] and the grid directories and network access described in
//! the [CRS documentation](https://opengeos.github.io/terrio/crs/)). Compound CRSs (such as
//! `EPSG:4326+5773`) and geographic 3D CRSs transform heights with [`Transformer::transform_3d`];
//! time-dependent transformations between dynamic CRSs take the coordinate epoch of
//! [`TransformOptions`]:
//!
//! ```no_run
//! use terrio::crs::{SpatialRef, TransformOptions, Transformer};
//!
//! // Ellipsoidal heights (WGS 84) to EGM96 heights.
//! let t = Transformer::new(&SpatialRef::parse("EPSG:4979")?, &SpatialRef::parse("EPSG:4326+5773")?)?;
//! let (lon, lat, h) = t.transform_3d(-100.0, 40.0, 100.0)?;
//! // ITRF2014 coordinates at epoch 2010.0 to GDA2020.
//! let opts = TransformOptions { source_coordinate_epoch: Some(2010.0), ..Default::default() };
//! let t = Transformer::with_options(&SpatialRef::parse("EPSG:7912")?, &SpatialRef::parse("EPSG:7843")?, &opts)?;
//! let (lon, lat, h) = t.transform_3d(135.0, -25.0, 0.0)?;
//! # Ok::<(), terrio::Error>(())
//! ```

//! The implementation is shared with the standalone `terrio-crs` crate.

#[cfg(remote)]
use crate::Error;
use crate::Result;
use serde::Serialize;

pub(crate) use terrio_crs::{
    compound_code, describe, epsg_ellipsoid, epsg_wkt, from_proj_on, geographic_datum, geographic_on_datum, identify_epsg, is_esri_code,
    semi_major_axis, wkt_proj,
};

/// Starts loading the EPSG table in the background, on terrio's pool (see [`initialize`]), for
/// work that will need it once it has read its input.
pub(crate) fn prefetch() {
    initialize();
    terrio_crs::prefetch();
}

/// The bounds of the engine's WKT reader, which are GDAL's and PROJ's: other WKT readers of
/// terrio's take them.
pub(crate) mod wkt1 {
    pub(crate) use terrio_crs::{WKT_MAX_LEN as MAX_LEN, WKT_MAX_LEVELS as MAX_LEVELS};
}

/// Holds the engine's process-wide locks across a fork, for [`crate::par`]: the forking thread
/// takes them before the process is copied and lets them go after, so that the child never starts
/// with one held by a thread it does not have.
#[cfg(all(unix, not(target_family = "wasm")))]
pub(crate) fn hold_for_fork(held: &mut Vec<Box<dyn std::any::Any>>) {
    terrio_crs::hold_for_fork(held);
}

/// A coordinate reference system, known by an EPSG (or ESRI) code, a WKT definition, a PROJ
/// string or a combination of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct SpatialRef {
    /// The EPSG code, when the CRS has (or was identified as having) one.
    pub epsg: Option<u32>,
    /// The WKT definition.
    pub wkt: Option<String>,
    /// The PROJ string definition.
    pub proj4: Option<String>,
    /// The code of PROJ's ESRI authority (`ESRI:54009`), when the CRS has (or was identified as
    /// having) one rather than an EPSG code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub esri: Option<u32>,
}

impl From<terrio_crs::SpatialRef> for SpatialRef {
    fn from(value: terrio_crs::SpatialRef) -> Self {
        Self { epsg: value.epsg, wkt: value.wkt, proj4: value.proj4, esri: value.esri }
    }
}

impl From<SpatialRef> for terrio_crs::SpatialRef {
    fn from(value: SpatialRef) -> Self {
        Self { epsg: value.epsg, wkt: value.wkt, proj4: value.proj4, esri: value.esri }
    }
}

impl std::fmt::Display for SpatialRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.as_core().fmt(f)
    }
}

impl SpatialRef {
    fn as_core(&self) -> terrio_crs::SpatialRef {
        self.clone().into()
    }
    /// A CRS from an EPSG code. Fails when the code is not in the EPSG registry terrio carries.
    /// The ESRI codes wbprojection holds as EPSG codes (54008, 54009 and 54030) give those ESRI
    /// CRSs; other ESRI codes are not EPSG codes, as in PROJ.
    pub fn from_epsg(code: u32) -> Result<Self> {
        initialize();
        terrio_crs::SpatialRef::from_epsg(code).map(Into::into).map_err(Into::into)
    }
    /// A CRS from a code of PROJ's ESRI authority, such as 54009 (World Mollweide), with the WKT
    /// GDAL writes for it. Fails when terrio does not know the code or cannot compute its
    /// projection.
    pub fn from_esri(code: u32) -> Result<Self> {
        initialize();
        terrio_crs::SpatialRef::from_esri(code).map(Into::into).map_err(Into::into)
    }
    /// A geographic or projected CRS from the versioned IAU_2015 registry.
    pub fn from_iau(code: u32) -> Result<Self> {
        initialize();
        terrio_crs::SpatialRef::from_iau(code).map(Into::into).map_err(Into::into)
    }
    /// A compound CRS from the EPSG codes of a horizontal and a vertical CRS, as GDAL builds
    /// `EPSG:4326+5773`.
    pub fn compound(horizontal: u32, vertical: u32) -> Result<Self> {
        initialize();
        terrio_crs::SpatialRef::compound(horizontal, vertical).map(Into::into).map_err(Into::into)
    }
    /// A CRS from a PROJ string such as `+proj=utm +zone=33 +datum=WGS84`.
    pub fn from_proj4(proj4: &str) -> Result<Self> {
        initialize();
        terrio_crs::SpatialRef::from_proj4(proj4).map(Into::into).map_err(Into::into)
    }
    /// A CRS from the supported PROJJSON v0.7 subset.
    pub fn from_projjson(text: &str) -> Result<Self> {
        initialize();
        terrio_crs::SpatialRef::from_projjson(text).map(Into::into).map_err(Into::into)
    }

    /// The CRS as PROJJSON v0.7 (see the standalone engine's `SpatialRef::to_projjson`).
    pub fn to_projjson(&self) -> Result<String> {
        initialize();
        self.as_core().to_projjson().map_err(Into::into)
    }

    /// Parses a user-supplied CRS: `EPSG:4326`, `4326`, `urn:ogc:def:crs:EPSG::4326`, an OGC URL,
    /// `OGC:CRS84`, `WGS84`, `ESRI:54009` (or `urn:ogc:def:crs:ESRI::54009`), a WKT definition,
    /// a PROJ string, or the path of a file holding one of these (such as a shapefile's `.prj`).
    pub fn parse(s: &str) -> Result<Self> {
        initialize();
        terrio_crs::SpatialRef::parse(s).map(Into::into).map_err(Into::into)
    }
    /// Identify a CRS from a WKT definition.
    pub fn from_wkt(wkt: &str) -> Self {
        initialize();
        terrio_crs::SpatialRef::from_wkt(wkt).into()
    }
    /// Whether nothing is known about the CRS.
    pub fn is_unknown(&self) -> bool {
        self.as_core().is_unknown()
    }
    /// The authority and code of the CRS: `("EPSG", 4326)`, or `("ESRI", 54009)` for a CRS of
    /// PROJ's ESRI authority; IAU codes use `("IAU_2015", code)`.
    pub fn authority(&self) -> Option<(&'static str, u32)> {
        self.as_core().authority()
    }
    /// The authority code as GDAL writes it: `EPSG:4326` or `ESRI:54009`.
    pub fn authority_code(&self) -> Option<String> {
        self.as_core().authority_code()
    }
    /// The WKT definition, built from the EPSG (or ESRI) code or PROJ string when no WKT is
    /// stored.
    pub fn to_wkt(&self) -> Option<String> {
        self.as_core().to_wkt()
    }
    /// The CRS as PROJJSON, as GDAL (3.12.1, with PROJ 9.7.1) writes it: with `database`, for the
    /// CRS GDAL builds from the CRS's EPSG code (`CRS.from_epsg`), else for the one it reads from
    /// the CRS's WKT (a dataset's CRS, `CRS.from_wkt`). Fails for a CRS this writer does not model.
    /// (`to_projjson` writes terrio's own PROJJSON, for more kinds of CRS, with documented
    /// differences from PROJ's.)
    pub fn to_gdal_projjson(&self, database: bool) -> Result<serde_json::Value> {
        initialize();
        self.as_core().to_gdal_projjson(database).map_err(Into::into)
    }
    /// The CRS's name, such as `WGS 84 / UTM zone 33N`.
    pub fn name(&self) -> Option<String> {
        self.as_core().name()
    }
    /// A short label: `EPSG:4326` (or `ESRI:54009`) when the code is known, otherwise the name.
    pub fn label(&self) -> String {
        self.as_core().label()
    }
    /// The EPSG codes of the horizontal and the vertical CRS of a compound CRS (see the standalone
    /// CRS engine's `compound_parts` method).
    pub(crate) fn compound_parts(&self) -> Option<(u32, u32)> {
        self.as_core().compound_parts()
    }
    /// Whether the enabled engines recognize a geographic (longitude/latitude) CRS.
    /// Unrecognized definitions return `false`, as do projected CRSs.
    pub fn is_geographic(&self) -> bool {
        self.as_core().is_geographic()
    }
    /// Whether the CRS has geocentric Cartesian X/Y/Z coordinates.
    pub fn is_geocentric(&self) -> bool {
        self.as_core().is_geocentric()
    }
    /// Whether coordinates have a height that transformations change: a compound CRS (with a
    /// vertical CRS), a geographic 3D or a geocentric CRS.
    pub fn has_height(&self) -> bool {
        self.as_core().has_height()
    }
    /// Whether this is a rotated-pole grid (PROJ's `+proj=ob_tran`, CF's
    /// `rotated_latitude_longitude`): its coordinates are in degrees but are not longitudes and
    /// latitudes, so it is not geographic.
    pub fn is_rotated_pole(&self) -> bool {
        self.as_core().is_rotated_pole()
    }
    /// The WKT of the geographic CRS this CRS is based on (its own when geographic): from the
    /// EPSG table for an EPSG CRS, else the `GEOGCS` node of its WKT 1. `None` when neither is
    /// known.
    pub(crate) fn geographic_wkt(&self) -> Option<String> {
        self.as_core().geographic_wkt()
    }
    /// Metres per unit of the CRS's coordinates, as GDAL reckons them for tile matrix sets: the
    /// linear unit of a projected CRS, the length of a degree on the semi-major axis of a
    /// geographic one (1 when unknown).
    pub(crate) fn metres_per_unit(&self) -> f64 {
        self.as_core().metres_per_unit()
    }
    /// Whether coordinates are in degrees: a geographic CRS or a rotated-pole grid.
    pub(crate) fn is_angular(&self) -> bool {
        self.as_core().is_angular()
    }
    /// See the standalone CRS engine's `geographic_code` method.
    pub(crate) fn geographic_code(&self) -> Option<u32> {
        self.as_core().geographic_code()
    }
    /// See the standalone CRS engine's `rotated_pole_parameters` method.
    pub(crate) fn rotated_pole_parameters(&self) -> Option<([f64; 3], f64, f64)> {
        self.as_core().rotated_pole_parameters()
    }
    /// Whether these definitions describe the same CRS.
    pub fn same_as(&self, other: &Self) -> bool {
        self.as_core().same_as(&other.as_core())
    }
    pub(crate) fn with_esri(self) -> Self {
        let core: terrio_crs::SpatialRef = self.into();
        core.with_esri().into()
    }
    pub(crate) fn to_wbp(&self) -> Result<wbprojection::Crs> {
        self.as_core().to_wbp().map_err(Into::into)
    }
    pub(crate) fn from_raster_crs(c: &wbraster::CrsInfo) -> Self {
        let mut s = Self { epsg: c.epsg, wkt: c.wkt.clone(), proj4: c.proj4.clone(), esri: None };
        // A WKT made from a PROJ string is not identified: it can describe less than the PROJ
        // string (the registry's loose matches took a Robinson on another meridian for ESRI's).
        if s.epsg.is_none()
            && s.proj4.is_none()
            && let Some(w) = &s.wkt
        {
            s.epsg = identify_epsg(w);
        }
        if s.proj4.is_none() {
            return s.with_esri();
        }
        // Nor is it identified as an ESRI CRS, but a pseudo-EPSG ESRI code is not kept as EPSG's.
        s.epsg = s.epsg.filter(|c| !is_esri_code(*c));
        s
    }

    /// The CRS as the raster model stores it.
    pub fn to_crs_info(&self) -> wbraster::CrsInfo {
        wbraster::CrsInfo { epsg: self.epsg, wkt: self.to_wkt(), proj4: self.proj4.clone() }
    }

    /// The CRS a vector layer declares.
    pub fn from_layer(layer: &wbvector::Layer) -> Self {
        let mut s = Self { epsg: layer.crs_epsg(), wkt: layer.crs_wkt().map(str::to_string), ..Default::default() };
        if s.epsg.is_none()
            && let Some(w) = &s.wkt
        {
            s.epsg = identify_epsg(w);
        }
        s.with_esri()
    }

    /// Sets a vector layer's CRS to this one.
    pub fn apply_to_layer(&self, layer: &mut wbvector::Layer) {
        layer.set_crs_epsg(self.epsg);
        layer.set_crs_wkt(self.to_wkt());
    }
}

/// Options of a [`Transformer`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TransformOptions {
    /// The coordinate epoch of the source coordinates, as a decimal year (`2010.5`), for a
    /// dynamic source CRS (GDAL's `-s_coord_epoch`).
    pub source_coordinate_epoch: Option<f64>,
    /// The coordinate epoch of the target coordinates, for a dynamic target CRS (GDAL's
    /// `-t_coord_epoch`).
    pub target_coordinate_epoch: Option<f64>,
    /// Allow approximate datum operations. `None` uses GDAL's default (yes).
    pub allow_ballpark: Option<bool>,
    /// Require the best operation assuming all grids were available (GDAL's ONLY_BEST).
    pub only_best: bool,
    /// Take the heights of 2D CRSs as ellipsoidal heights, so that datum transformations change
    /// them, as GDAL does after `PromoteTo3D` on both CRSs. Off by default, as in GDAL.
    pub promote_to_3d: bool,
}

impl From<&TransformOptions> for terrio_crs::TransformOptions {
    fn from(value: &TransformOptions) -> Self {
        Self {
            source_coordinate_epoch: value.source_coordinate_epoch,
            target_coordinate_epoch: value.target_coordinate_epoch,
            allow_ballpark: value.allow_ballpark,
            only_best: value.only_best,
            promote_to_3d: value.promote_to_3d,
        }
    }
}
impl From<terrio_crs::TransformOptions> for TransformOptions {
    fn from(value: terrio_crs::TransformOptions) -> Self {
        Self {
            source_coordinate_epoch: value.source_coordinate_epoch,
            target_coordinate_epoch: value.target_coordinate_epoch,
            allow_ballpark: value.allow_ballpark,
            only_best: value.only_best,
            promote_to_3d: value.promote_to_3d,
        }
    }
}
impl TransformOptions {
    /// Parse dedicated coordinate-operation options, rejecting unknown keys.
    pub fn from_strict_options<K: AsRef<str>, V: AsRef<str>>(items: &[(K, V)]) -> Result<Self> {
        terrio_crs::TransformOptions::from_strict_options(items).map(Into::into).map_err(Into::into)
    }
    /// Parse GDAL coordinate-operation options, ignoring unrelated keys.
    pub fn from_options<K: AsRef<str>, V: AsRef<str>>(items: &[(K, V)]) -> Result<Self> {
        terrio_crs::TransformOptions::from_options(items).map(Into::into).map_err(Into::into)
    }
}

pub use terrio_crs::CoordinateOperation;

/// A coordinate transformer backed by the shared standalone engine.
pub struct Transformer(terrio_crs::Transformer);
impl Transformer {
    /// Create a transformer with default coordinate-operation options.
    pub fn new(src: &SpatialRef, dst: &SpatialRef) -> Result<Self> {
        Self::with_options(src, dst, &TransformOptions::default())
    }
    /// Create a transformer with coordinate epochs and operation-selection policy.
    pub fn with_options(src: &SpatialRef, dst: &SpatialRef, opts: &TransformOptions) -> Result<Self> {
        initialize();
        terrio_crs::Transformer::with_options(&src.as_core(), &dst.as_core(), &opts.into())
            .map(|t| Self(t.with_warning_handler(crate::warnings::warn)))
            .map_err(Into::into)
    }
    /// Transform an x/y coordinate pair.
    pub fn transform(&self, x: f64, y: f64) -> Result<(f64, f64)> {
        self.0.transform(x, y).map_err(Into::into)
    }
    /// Transform an x/y/z coordinate.
    pub fn transform_3d(&self, x: f64, y: f64, z: f64) -> Result<(f64, f64, f64)> {
        self.0.transform_3d(x, y, z).map_err(Into::into)
    }
    /// Transform a coordinate with its observation epoch.
    pub fn transform_4d(&self, x: f64, y: f64, z: f64, t: f64) -> Result<(f64, f64, f64)> {
        self.0.transform_4d(x, y, z, t).map_err(Into::into)
    }
    /// The coordinate operation [`Transformer::transform_3d`] uses for a point (see the
    /// standalone engine's `Transformer::operation_at`); `None` when it needs none.
    pub fn operation_at(&self, x: f64, y: f64, z: f64) -> Result<Option<CoordinateOperation>> {
        self.0.operation_at(x, y, z).map_err(Into::into)
    }
    /// Transform a bounding box, densifying its edges.
    pub fn transform_bounds(&self, bounds: (f64, f64, f64, f64)) -> Result<(f64, f64, f64, f64)> {
        self.0.transform_bounds(bounds).map_err(Into::into)
    }
    /// Emit new diagnostics on this thread through Terrio's warning sink.
    pub fn flush_warnings(&self) {
        self.0.flush_warnings();
    }
    /// Check strict coordinate-operation failures recorded by workers.
    pub fn check_policy(&self) -> Result<()> {
        self.0.check_policy().map_err(Into::into)
    }
}

/// Register a grid in the shared engine, including WebAssembly callers.
pub fn register_grid(name: &str, bytes: Vec<u8>) {
    initialize();
    terrio_crs::register_grid(name, bytes);
}
/// Override the shared engine's grid search paths.
pub fn set_grid_search_paths(paths: Option<Vec<std::path::PathBuf>>) {
    initialize();
    terrio_crs::set_grid_search_paths(paths);
}
/// Override shared grid network access.
pub fn set_network_enabled(enabled: Option<bool>) {
    initialize();
    terrio_crs::set_network_enabled(enabled);
}
/// Reset decoded grids in the shared engine.
pub fn reset_grid_cache() {
    initialize();
    terrio_crs::reset_grid_cache();
}

/// Makes the engine terrio's, once, before it is used: it reads terrio's configuration (options
/// scoped to the calling thread, then the environment: `TERRIO_NETWORK`, `PROJ_NETWORK` and the
/// CDN endpoint), runs its background work on terrio's pool, and registers terrio's fork handlers
/// before it first takes a process-wide lock, which those handlers hold across a fork (see
/// [`crate::par`]). Every entry point of this module that reaches the engine's locks or its
/// background work calls it first.
fn initialize() {
    static HOST: std::sync::Once = std::sync::Once::new();
    HOST.call_once(|| {
        terrio_crs::set_host(terrio_crs::Host {
            config: crate::config::get,
            #[cfg(not(target_family = "wasm"))]
            spawn: |job| crate::par::spawn(job),
            #[cfg(target_family = "wasm")]
            spawn: |job| job(),
            before_lock: crate::par::fork_safe,
        });
    });
    // Every unit-test entry point uses the same no-grid reference context. Do this
    // before constructing transformers, rather than changing shared state in
    // individual tests that may run alongside another transformation.
    #[cfg(test)]
    {
        static TEST_INIT: std::sync::Once = std::sync::Once::new();
        TEST_INIT.call_once(|| {
            terrio_crs::set_grid_search_paths(Some(Vec::new()));
            terrio_crs::set_network_enabled(Some(false));
        });
    }
    #[cfg(remote)]
    {
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| terrio_crs::network::set_default_grid_transport(std::sync::Arc::new(GridTransport)));
    }
}

#[cfg(remote)]
struct GridTransport;
#[cfg(remote)]
fn transport_error(error: Error) -> std::io::Error {
    // The HTTP layer currently exposes status errors as text. Match its terminal status,
    // not arbitrary text in a URL or a non-HTTP error. The local-server regression below
    // pins both open/download paths until HTTP errors carry a structured status.
    let kind = if matches!(&error, Error::Http(message) if message.ends_with(": status 404")) {
        std::io::ErrorKind::NotFound
    } else {
        std::io::ErrorKind::Other
    };
    std::io::Error::new(kind, error)
}
#[cfg(remote)]
impl terrio_crs::network::GridTransport for GridTransport {
    fn open(&self, url: &str) -> std::io::Result<Box<dyn terrio_crs::network::GridStream>> {
        crate::http::RangeReader::open(&crate::http::Remote::Url(url.to_string()))
            .map(|r| Box::new(r) as Box<dyn terrio_crs::network::GridStream>)
            .map_err(transport_error)
    }
    fn download(&self, url: &str, path: &std::path::Path) -> std::io::Result<()> {
        crate::http::download(&crate::http::Remote::Url(url.to_string()), path).map_err(transport_error)
    }
}

#[cfg(all(test, remote))]
mod transport_tests {
    use super::*;
    use std::io::{Read, Write};
    use terrio_crs::network::GridTransport as _;

    #[test]
    fn grid_transport_preserves_missing_http_status() {
        for (status, expected) in [(404, std::io::ErrorKind::NotFound), (403, std::io::ErrorKind::Other)] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/grid.tif", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                for _ in 0..2 {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut request = [0; 4096];
                    let _ = stream.read(&mut request).unwrap();
                    write!(stream, "HTTP/1.1 {status} Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                }
            });
            let error = GridTransport.open(&url).err().unwrap();
            assert_eq!(error.kind(), expected);
            let temporary = tempfile::tempdir().unwrap();
            let error = GridTransport.download(&url, &temporary.path().join("grid.tif")).unwrap_err();
            assert_eq!(error.kind(), expected);
            server.join().unwrap();
        }
        assert_eq!(transport_error(Error::Http("http://host/status 404: status 403".into())).kind(), std::io::ErrorKind::Other);
        assert_eq!(transport_error(Error::Crs("status 404".into())).kind(), std::io::ErrorKind::Other);
    }
}

#[cfg(all(test, unix, not(target_family = "wasm")))]
mod fork_tests {
    /// The engine's process-wide locks (the grids' settings, registered grids, files read, failed
    /// fetches, availability and grid sets, and the grid transport's) are held across a fork as
    /// terrio's own are: a child forked while another thread holds all of them does not start with
    /// one held, and resets, registers and configures grids. In a process of its own
    /// (`par::alone`), so that no other test delays the fork (another test holding the fork's
    /// locks would) until the locks are free anyway.
    #[test]
    fn a_child_never_starts_with_the_engines_locks_held() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        crate::par::alone(60, || {
            super::initialize();
            let holding = Arc::new(AtomicBool::new(false));
            let h = holding.clone();
            let holder = std::thread::spawn(move || {
                let mut held = Vec::new();
                terrio_crs::hold_for_fork(&mut held);
                h.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(300));
                drop(held);
            });
            while !holding.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            let exit = crate::par::in_child(10, || {
                super::reset_grid_cache();
                super::register_grid("in_child.gtx", Vec::new());
                super::set_network_enabled(Some(false));
                #[cfg(remote)]
                terrio_crs::network::set_default_grid_transport(Arc::new(super::GridTransport));
                true
            });
            holder.join().is_ok() && exit == Some(0)
        });
    }
}

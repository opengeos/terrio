//! Dataset footprint indexes, including metadata for GDAL's GTI raster driver.
//!
//! Raster footprints are computed from source headers. Lazy-capable drivers avoid
//! pixel decoding; other drivers use their existing whole-reader fallback.
//! Vector footprints are layer envelopes, not feature footprints.
use crate::crs::{SpatialRef, Transformer};
use crate::{Error, Feature, FieldDef, FieldType, FieldValue, Geometry, GeometryType, Layer, LazyRaster, Result};
use std::collections::{HashMap, HashSet};
use wbvector::Coord;

/// Serialization of the original source CRS into an attribute field.
#[derive(Debug, Clone, Copy, Default)]
pub enum SourceCrsFormat {
    /// Authority code when available, otherwise WKT.
    #[default]
    Auto,
    /// WKT definition.
    Wkt,
    /// Authority code (empty for definitions without one).
    Epsg,
    /// PROJ string (empty when unavailable).
    Proj,
}
impl SourceCrsFormat {
    /// Parses auto, WKT, EPSG or PROJ, case-insensitively.
    pub fn parse(s: &str) -> Result<Self> {
        match s.to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "wkt" => Ok(Self::Wkt),
            "epsg" => Ok(Self::Epsg),
            "proj" => Ok(Self::Proj),
            _ => Err(invalid("source CRS format must be auto, WKT, EPSG or PROJ")),
        }
    }
    fn value(self, crs: &SpatialRef) -> String {
        match self {
            Self::Auto => crs.authority_code().or_else(|| crs.to_wkt()),
            Self::Wkt => crs.to_wkt(),
            Self::Epsg => crs.authority_code(),
            Self::Proj => crs.proj4.clone().or_else(|| crs.to_wkt().and_then(|w| crate::crs::wkt_proj(&w))),
        }
        .unwrap_or_default()
    }
}

/// Options common to raster and vector dataset indexes.
#[derive(Debug, Clone)]
pub struct IndexOptions {
    /// Explicit output layer name. None uses the raster output basename or vector tileindex.
    pub layer_name: Option<String>,
    /// Field containing source locations (location by default).
    pub location_field: String,
    /// Target CRS (raster warped-grid envelope or vector corner reprojection).
    /// Otherwise use the first source's CRS.
    pub output_crs: Option<SpatialRef>,
    /// Optional field containing each original source CRS.
    pub source_crs_field: Option<String>,
    /// Serialization used for that field.
    pub source_crs_format: SourceCrsFormat,
    /// Metadata stored on the index layer (including optional GTI characteristics).
    pub metadata: Vec<(String, String)>,
}
impl Default for IndexOptions {
    fn default() -> Self {
        Self {
            layer_name: None,
            location_field: "location".into(),
            output_crs: None,
            source_crs_field: None,
            source_crs_format: Default::default(),
            metadata: Vec::new(),
        }
    }
}

/// A source raster's footprint information; contains no pixel buffers.
#[derive(Debug, Clone)]
pub struct RasterIndexSource {
    /// Location stored verbatim in the output field.
    pub location: String,
    /// Raster width and height.
    pub size: [usize; 2],
    /// Number of source bands, used for GTI interleave metadata.
    pub band_count: usize,
    /// Full affine transform, including rotated and south-up grids.
    pub geo_transform: [f64; 6],
    /// Original source CRS.
    pub crs: SpatialRef,
    /// Source dataset metadata available for attribute extraction.
    pub metadata: Vec<(String, String)>,
}
impl RasterIndexSource {
    /// Opens a native/VSI source header, sanitizing diagnostics for locators with queries.
    #[cfg(not(target_family = "wasm"))]
    pub fn open_path(path: &str, location: impl Into<String>) -> Result<Self> {
        (|| Self::from_lazy(location, &crate::open_raster_lazy(path)?))().map_err(|e| index_source_error(path, e))
    }
    /// Inspects an open source without reading pixels. Non-affine sources are refused.
    pub fn from_lazy(location: impl Into<String>, src: &LazyRaster) -> Result<Self> {
        if !src.has_geo_transform() {
            return Err(invalid("raster index requires affine georeferencing"));
        }
        let h = src.header();
        Ok(Self {
            location: location.into(),
            size: [src.width(), src.height()],
            band_count: src.band_count(),
            geo_transform: src.geo_transform(),
            crs: h.crs(),
            metadata: h.raster.metadata,
        })
    }
}

/// A metadata item copied from each raster into an index field.
#[derive(Debug, Clone)]
pub struct MetadataField {
    /// Source metadata key.
    pub key: String,
    /// Output field name.
    pub name: String,
    /// Text, integer or float. Missing metadata is null.
    pub field_type: FieldType,
}
/// Optional characteristics of the virtual raster represented by a GTI index.
#[derive(Debug, Clone, Default)]
pub struct GtiOptions {
    /// Output pixel resolution in index CRS units (positive x/y sizes).
    pub resolution: Option<[f64; 2]>,
    /// Virtual mosaic bounds `[min_x, min_y, max_x, max_y]`.
    pub extent: Option<[f64; 4]>,
    /// Virtual raster sample type (otherwise inferred when opened).
    pub data_type: Option<crate::DataType>,
    /// Virtual raster band count.
    pub band_count: Option<usize>,
    /// Nodata value per band, or one value for every band.
    pub nodata: Vec<f64>,
    /// Band color interpretations, e.g. gray,alpha.
    pub color_interpretations: Vec<String>,
    /// Declare a dataset mask.
    pub mask: bool,
}
/// Nodata and color lists must hold one value, or one per band.
fn check_band_lists(bands: usize, nodata: &[f64], colors: &[String]) -> Result<()> {
    if (!nodata.is_empty() && nodata.len() != 1 && nodata.len() != bands)
        || (!colors.is_empty() && colors.len() != 1 && colors.len() != bands)
    {
        return Err(invalid("GTI nodata/colors need one value or one per explicit/inferred output band"));
    }
    Ok(())
}
impl GtiOptions {
    /// Validates the characteristics and serializes the GTI metadata entries.
    pub fn metadata(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        if let Some([x, y]) = self.resolution {
            if !x.is_finite() || !y.is_finite() || x <= 0. || y <= 0. {
                return Err(invalid("GTI resolution must be finite and positive"));
            }
            out.extend([("RESX".into(), x.to_string()), ("RESY".into(), y.to_string())]);
        }
        if let Some([a, b, c, d]) = self.extent {
            if [a, b, c, d].iter().any(|v| !v.is_finite()) || a >= c || b >= d {
                return Err(invalid("GTI extent must be finite and ordered"));
            }
            out.extend([
                ("MINX".into(), a.to_string()),
                ("MINY".into(), b.to_string()),
                ("MAXX".into(), c.to_string()),
                ("MAXY".into(), d.to_string()),
            ]);
        }
        if let Some(t) = self.data_type {
            out.push(("DATA_TYPE".into(), crate::info::gdal_type_name(t).into()));
        }
        if let Some(n) = self.band_count {
            if n == 0 {
                return Err(invalid("GTI band count must be positive"));
            }
            check_band_lists(n, &self.nodata, &self.color_interpretations)?;
            out.push(("BAND_COUNT".into(), n.to_string()));
        }
        if !self.nodata.is_empty() {
            out.push(("NODATA".into(), self.nodata.iter().map(f64::to_string).collect::<Vec<_>>().join(",")));
        }
        if !self.color_interpretations.is_empty() {
            if self
                .color_interpretations
                .iter()
                .any(|c| !matches!(c.to_ascii_lowercase().as_str(), "red" | "green" | "blue" | "alpha" | "gray" | "undefined"))
            {
                return Err(invalid("invalid GTI color interpretation"));
            }
            out.push(("COLOR_INTERPRETATION".into(), self.color_interpretations.join(",")));
        }
        if self.mask {
            out.push(("MASK_BAND".into(), "YES".into()));
        }
        Ok(out)
    }
}
impl MetadataField {
    /// Builds a metadata extraction field from String, Integer or Real (and text/float aliases).
    pub fn from_type_name(key: impl Into<String>, name: impl Into<String>, kind: &str) -> Result<Self> {
        let field_type = match kind.to_ascii_lowercase().as_str() {
            "string" | "text" => FieldType::Text,
            "integer" => FieldType::Integer,
            "real" | "float" => FieldType::Float,
            _ => return Err(invalid("metadata field type must be String, Integer or Real")),
        };
        Ok(Self { key: key.into(), name: name.into(), field_type })
    }
}
/// Options for raster footprint indexes.
#[derive(Debug, Clone, Default)]
pub struct RasterIndexOptions {
    /// Common layer/CRS/field options.
    pub index: IndexOptions,
    /// Minimum sqrt(envelope area / pixel count), measured after any reprojection.
    pub min_pixel_size: Option<f64>,
    /// Maximum envelope-based pixel size in index coordinates.
    pub max_pixel_size: Option<f64>,
    /// Metadata attributes to extract from the source headers.
    pub fetch_metadata: Vec<MetadataField>,
    /// Optional virtual raster characteristics for the GTI driver.
    pub gti: GtiOptions,
}

/// One vector layer to index. A layer with a known extent may omit its features.
#[derive(Debug, Clone)]
pub struct VectorIndexSource {
    /// Dataset location (before the optional layer-index suffix).
    pub location: String,
    /// Zero-based layer index in the original dataset.
    pub layer_index: usize,
    /// Layer schema, CRS and envelope, or features from which to compute the envelope.
    pub layer: Layer,
}
impl VectorIndexSource {
    /// Copies only a layer's schema/CRS and envelope, never its feature buffers.
    pub fn from_layer(location: impl Into<String>, layer_index: usize, layer: &Layer) -> Self {
        let mut header = crate::stream::schema_of(layer);
        header.extent = layer_bounds(layer);
        Self { location: location.into(), layer_index, layer: header }
    }
}
fn layer_bounds(layer: &Layer) -> Option<wbvector::BBox> {
    layer.extent.clone().or_else(|| {
        layer.features.iter().filter_map(|f| f.geometry.as_ref()?.bbox()).reduce(|mut a, b| {
            a.min_x = a.min_x.min(b.min_x);
            a.min_y = a.min_y.min(b.min_y);
            a.max_x = a.max_x.max(b.max_x);
            a.max_y = a.max_y.max(b.max_y);
            a
        })
    })
}
/// Options for vector dataset indexes.
#[derive(Debug, Clone, Default)]
pub struct VectorIndexOptions {
    /// Common layer/CRS/field options.
    pub index: IndexOptions,
    /// Accept differing source schemas (otherwise skip them with a warning).
    pub accept_different_schemas: bool,
    /// Accept differing CRSs without reprojection (otherwise skip them with a warning).
    /// An explicit output CRS always reprojects the corners.
    pub accept_different_crs: bool,
    /// Omit the comma and zero-based layer index from locations.
    pub dataset_name_only: bool,
}

/// An index layer and its driver metadata, retained together when serializing.
#[derive(Debug, Clone)]
pub struct DatasetIndex {
    /// Polygon features and source fields.
    pub layer: Layer,
    /// Layer metadata, e.g. LOCATION_FIELD and GTI raster characteristics.
    pub metadata: Vec<(String, String)>,
    source_locations: Vec<String>,
    default_raster_name: bool,
    relative_base: Option<std::path::PathBuf>,
    relative_locations: HashMap<String, (std::path::PathBuf, String)>,
}
fn invalid(s: impl Into<String>) -> Error {
    Error::InvalidArgument(s.into())
}
fn field_name(s: &str) -> Result<()> {
    if s.trim().is_empty() || s.trim() != s || s.contains('\0') {
        Err(invalid("index field and layer names must be nonempty, unpadded and contain no NUL"))
    } else {
        Ok(())
    }
}
fn empty(o: &IndexOptions, crs: &SpatialRef) -> Result<DatasetIndex> {
    let name = o.layer_name.as_deref().unwrap_or("tileindex");
    field_name(name)?;
    field_name(&o.location_field)?;
    let mut layer = Layer::new(name);
    layer.geom_type = Some(GeometryType::Polygon);
    crs.apply_to_layer(&mut layer);
    layer.schema.add_field(FieldDef::new(&o.location_field, FieldType::Text));
    if let Some(f) = &o.source_crs_field {
        field_name(f)?;
        if f.eq_ignore_ascii_case(&o.location_field) {
            return Err(invalid("source CRS field must differ from the location field"));
        }
        layer.schema.add_field(FieldDef::new(f, FieldType::Text));
    }
    for (key, value) in &o.metadata {
        if key.is_empty() || key.contains('\0') || value.contains('\0') {
            return Err(invalid("invalid index metadata key/value"));
        }
    }
    Ok(DatasetIndex {
        layer,
        metadata: o.metadata.clone(),
        source_locations: Vec::new(),
        default_raster_name: false,
        relative_base: None,
        relative_locations: HashMap::new(),
    })
}
fn warning_locator(location: &str) -> &str {
    if location.contains("://") || location.starts_with("/vsi") {
        location.split_once('?').map_or(location, |(base, _)| base)
    } else {
        location
    }
}
#[cfg(not(target_family = "wasm"))]
fn index_source_error(path: &str, error: Error) -> Error {
    if warning_locator(path) == path {
        return error;
    }
    // Keep transport/argument categories and I/O kinds without retaining a
    // payload that may repeat the complete signed URL in Display or Debug.
    let message = format!("cannot inspect remote index source {}", warning_locator(path));
    match error {
        Error::Io(e) => Error::Io(std::io::Error::new(e.kind(), message)),
        Error::Crs(_) => Error::Crs(message),
        Error::Http(_) => Error::Http(message),
        Error::Unsupported(_) => Error::Unsupported(message),
        Error::InvalidArgument(_) => Error::InvalidArgument(message),
        Error::Raster(_) | Error::Vector(_) | Error::InvalidData(_) => Error::InvalidData(message),
        e @ Error::DecodedLimitExceeded { .. } => e,
    }
}
#[cfg(not(target_family = "wasm"))]
fn warn_skipped_source(path: &str, error: &Error) {
    if warning_locator(path) != path {
        // Driver/transport errors can repeat the complete signed URL. Keep those
        // diagnostics out of warnings as well as sanitizing the displayed locator.
        crate::warnings::warn(format!("skipping {}: cannot inspect remote source metadata", warning_locator(path)));
    } else {
        crate::warnings::warn(format!("skipping {path}: {error}"));
    }
}
fn warn_query_locations<'a>(mut locations: impl Iterator<Item = &'a str>) {
    if locations.any(|s| s.contains('?') && (s.contains("://") || s.starts_with("/vsi"))) {
        crate::warnings::warn("Index locations retain remote URL query parameters, which may contain access credentials. Use credential-free locations and environment credentials before sharing a catalog.".into());
    }
}
fn same_crs(a: &SpatialRef, b: &SpatialRef) -> bool {
    (a.is_unknown() && b.is_unknown()) || a.same_as(b)
}
fn polygon(mut corners: Vec<Coord>, src: &SpatialRef, dst: &SpatialRef, transform: bool) -> Result<Geometry> {
    if transform && !same_crs(src, dst) {
        if src.is_unknown() || dst.is_unknown() {
            return Err(invalid("cannot reproject an index footprint with an unknown CRS"));
        }
        let t = Transformer::new(src, dst)?;
        for c in &mut corners {
            (c.x, c.y) = t.transform(c.x, c.y)?;
        }
    }
    if corners.iter().any(|c| !c.x.is_finite() || !c.y.is_finite()) {
        return Err(invalid("index footprint coordinates must be finite"));
    }
    Ok(Geometry::Polygon { exterior: wbvector::Ring(corners), interiors: Vec::new() })
}
fn add(out: &mut DatasetIndex, geometry: Geometry, location: String, crs: &SpatialRef, o: &IndexOptions, extra: Vec<FieldValue>) {
    let mut attributes = vec![FieldValue::Text(location)];
    if o.source_crs_field.is_some() {
        let value = o.source_crs_format.value(crs);
        attributes.push(if value.is_empty() { FieldValue::Null } else { FieldValue::Text(value) });
    }
    attributes.extend(extra);
    out.layer.features.push(Feature { fid: out.layer.features.len() as u64 + 1, geometry: Some(geometry), attributes });
}

/// Creates an index of complete affine raster extents without reading pixels.
/// Corners retain source order: top-left, top-right, bottom-right, bottom-left.
/// When changing CRS, index the suggested warped-grid envelope, as GDAL 3.13 does.
/// Input locations are stored verbatim; callers can resolve local paths before indexing.
/// Without an explicit output CRS, incompatible inputs are skipped unless a
/// source-CRS field is requested; that field retains original coordinates with
/// a warning, as GDAL does. Set output_crs for a usable mixed-CRS catalog.
pub fn raster_index(sources: &[RasterIndexSource], o: &RasterIndexOptions) -> Result<DatasetIndex> {
    let first = sources.first().ok_or_else(|| invalid("raster index needs at least one source"))?;
    let crs = o.index.output_crs.as_ref().unwrap_or(&first.crs);
    let mut out = empty(&o.index, crs)?;
    out.default_raster_name = o.index.layer_name.is_none();
    out.source_locations = sources.iter().map(|s| s.location.clone()).collect();
    warn_query_locations(sources.iter().map(|s| s.location.as_str()));
    for limit in [o.min_pixel_size, o.max_pixel_size].into_iter().flatten() {
        if !limit.is_finite() || limit < 0. {
            return Err(invalid("pixel-size limits must be finite and nonnegative"));
        }
    }
    if o.min_pixel_size.zip(o.max_pixel_size).is_some_and(|(a, b)| a > b) {
        return Err(invalid("minimum pixel size exceeds maximum pixel size"));
    }
    let mut names: HashSet<String> = out.layer.schema.fields().iter().map(|f| f.name.to_lowercase()).collect();
    for f in &o.fetch_metadata {
        field_name(&f.name)?;
        if !names.insert(f.name.to_lowercase()) || !matches!(f.field_type, FieldType::Text | FieldType::Integer | FieldType::Float) {
            return Err(invalid("metadata fields must be unique text, integer or float fields"));
        }
        out.layer.schema.add_field(FieldDef::new(&f.name, f.field_type));
    }
    out.metadata.retain(|(k, _)| !k.eq_ignore_ascii_case("LOCATION_FIELD"));
    out.metadata.push(("LOCATION_FIELD".into(), o.index.location_field.clone()));
    // Without an explicit band count, the GTI band lists are checked against the first source.
    let bands = o.gti.band_count.unwrap_or(first.band_count);
    check_band_lists(bands, &o.gti.nodata, &o.gti.color_interpretations)?;
    let gti_metadata = o.gti.metadata()?;
    let configured_gti = !gti_metadata.is_empty();
    for (key, value) in gti_metadata {
        out.metadata.retain(|(k, _)| !k.eq_ignore_ascii_case(&key));
        out.metadata.push((key, value));
    }
    if bands > 1 && !out.metadata.iter().any(|(k, _)| k.eq_ignore_ascii_case("INTERLEAVE")) {
        out.metadata.push(("INTERLEAVE".into(), "PIXEL".into()));
    }
    for src in sources {
        if src.location.is_empty() || src.location.contains('\0') || src.size.contains(&0) {
            return Err(invalid("raster index source needs a location and positive dimensions"));
        }
        let g = src.geo_transform;
        let determinant = g[1] * g[5] - g[2] * g[4];
        if g.iter().any(|v| !v.is_finite()) || !determinant.is_finite() || determinant == 0. {
            return Err(invalid("raster index requires a finite invertible affine transform"));
        }
        if o.index.output_crs.is_none() && !same_crs(&src.crs, crs) {
            if o.index.source_crs_field.is_none() {
                crate::warnings::warn(format!(
                    "skipping {}: different CRS; specify output CRS or a source-CRS field",
                    warning_locator(&src.location)
                ));
                continue;
            }
            crate::warnings::warn(format!(
                "{} has a different CRS; specify output CRS for a mixed-CRS catalog",
                warning_locator(&src.location)
            ));
        }
        let [w, h] = src.size.map(|v| v as f64);
        let corners = [(0., 0.), (w, 0.), (w, h), (0., h)]
            .into_iter()
            .map(|(x, y)| Coord::xy(g[0] + x * g[1] + y * g[2], g[3] + x * g[4] + y * g[5]))
            .collect();
        let geometry = if o.index.output_crs.is_some() && !same_crs(&src.crs, crs) {
            if src.crs.is_unknown() || crs.is_unknown() {
                return Err(invalid("cannot reproject an index footprint with an unknown CRS"));
            }
            // GDAL 3.13 indexes the suggested warped raster envelope, rather than
            // transforming only the four original affine corners.
            let (gt, width, height) = super::warp::suggested_grid(src.size[0], src.size[1], g, &src.crs, crs)?;
            let (minx, maxy) = (gt[0], gt[3]);
            let (maxx, miny) = (minx + width as f64 * gt[1], maxy + height as f64 * gt[5]);
            polygon(vec![Coord::xy(minx, maxy), Coord::xy(maxx, maxy), Coord::xy(maxx, miny), Coord::xy(minx, miny)], crs, crs, false)?
        } else {
            polygon(corners, &src.crs, crs, false)?
        };
        let bounds = geometry.bbox().expect("rectangle has bounds");
        let pixel = (((bounds.max_x - bounds.min_x) * (bounds.max_y - bounds.min_y) / w) / h).sqrt();
        if o.min_pixel_size.is_some_and(|v| pixel < v) || o.max_pixel_size.is_some_and(|v| pixel > v) {
            continue;
        }
        if src.band_count < bands {
            if configured_gti {
                return Err(invalid(format!(
                    "raster index source has {} bands, fewer than the {bands} explicit/inferred output bands",
                    src.band_count
                )));
            }
            crate::warnings::warn("Raster index sources have different band counts; the footprint inventory is retained, but GTI pixel reads need a compatible explicit band count.".into());
        }
        let attrs = o
            .fetch_metadata
            .iter()
            .map(|f| {
                if f.key.eq_ignore_ascii_case("{PIXEL_SIZE}") {
                    return Ok(match f.field_type {
                        FieldType::Float => FieldValue::Float(pixel),
                        FieldType::Integer => FieldValue::Integer(pixel as i64),
                        FieldType::Text => FieldValue::Text(pixel.to_string()),
                        _ => unreachable!(),
                    });
                }
                let Some((_, value)) = src.metadata.iter().rev().find(|(k, _)| k == &f.key) else { return Ok(FieldValue::Null) };
                match f.field_type {
                    FieldType::Text => Ok(FieldValue::Text(value.clone())),
                    FieldType::Integer => {
                        value.parse().map(FieldValue::Integer).map_err(|_| invalid(format!("{} is not integer metadata", f.key)))
                    }
                    FieldType::Float => {
                        value.parse().map(FieldValue::Float).map_err(|_| invalid(format!("{} is not numeric metadata", f.key)))
                    }
                    _ => unreachable!(),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        add(&mut out, geometry, src.location.clone(), &src.crs, &o.index, attrs);
    }
    Ok(out)
}

/// Creates one envelope feature per vector layer. Empty layers are skipped.
/// Without an explicit output CRS, incompatible CRSs/schemas are skipped unless
/// accepted by the corresponding option. Source layer numbers are never renumbered.
pub fn vector_index(sources: &[VectorIndexSource], o: &VectorIndexOptions) -> Result<DatasetIndex> {
    let first = sources.first().ok_or_else(|| invalid("vector index needs at least one source layer"))?;
    let first_crs = SpatialRef::from_layer(&first.layer);
    let crs = o.index.output_crs.as_ref().unwrap_or(&first_crs);
    let mut out = empty(&o.index, crs)?;
    out.source_locations = sources.iter().map(|s| s.location.clone()).collect();
    warn_query_locations(sources.iter().map(|s| s.location.as_str()));
    for src in sources {
        if src.location.is_empty() || src.location.contains('\0') {
            return Err(invalid("vector index source needs a nonempty location"));
        }
        let source_crs = SpatialRef::from_layer(&src.layer);
        let schema_equal = src.layer.schema.fields().iter().map(|f| (&f.name, f.field_type)).eq(first
            .layer
            .schema
            .fields()
            .iter()
            .map(|f| (&f.name, f.field_type)));
        if !o.accept_different_schemas && !schema_equal {
            crate::warnings::warn(format!("skipping {}: different layer schema", warning_locator(&src.location)));
            continue;
        }
        if o.index.output_crs.is_none() && !o.accept_different_crs && !same_crs(&source_crs, crs) {
            crate::warnings::warn(format!("skipping {}: different layer CRS", warning_locator(&src.location)));
            continue;
        }
        if o.index.output_crs.is_none() && !same_crs(&source_crs, crs) {
            crate::warnings::warn("Accepted vector index sources have different CRSs; coordinates remain in their original CRS. Specify an output CRS to reproject them, or a source-CRS field to retain their identity.".into());
        }
        let bounds = layer_bounds(&src.layer);
        let Some(b) = bounds else {
            continue;
        };
        let corners =
            vec![Coord::xy(b.min_x, b.min_y), Coord::xy(b.min_x, b.max_y), Coord::xy(b.max_x, b.max_y), Coord::xy(b.max_x, b.min_y)];
        let geometry = polygon(corners, &source_crs, crs, o.index.output_crs.is_some())?;
        let location = if o.dataset_name_only { src.location.clone() } else { format!("{},{}", src.location, src.layer_index) };
        add(&mut out, geometry, location, &source_crs, &o.index, Vec::new());
    }
    Ok(out)
}

/// Default raster index layer name for a filename; Shapefile ZIP suffixes are a unit.
pub fn raster_index_layer_name(filename: &str) -> String {
    let file = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let stem = if file.to_ascii_lowercase().ends_with(".shp.zip") {
        &file[..file.len() - 8]
    } else {
        file.rsplit_once('.').map_or(file, |(stem, _)| stem)
    };
    if stem.is_empty() { "tileindex".into() } else { stem.into() }
}
fn lexical_path(path: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut result = std::path::PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    result
}
fn relative_path(path: &std::path::Path, base: &std::path::Path) -> std::path::PathBuf {
    let source: Vec<_> = path.components().collect();
    let parent: Vec<_> = base.components().collect();
    let common = source.iter().zip(&parent).take_while(|(a, b)| a == b).count();
    if common == 0 {
        return path.to_path_buf();
    }
    let mut result = std::path::PathBuf::new();
    for _ in common..parent.len() {
        result.push("..");
    }
    for component in &source[common..] {
        result.push(component.as_os_str());
    }
    result
}
/// Writes a relative location with `/` separators, so a catalog written on Windows opens
/// elsewhere (Windows accepts `/` too). An absolute path, such as one on another drive, keeps
/// its native form.
fn portable_location(path: &std::path::Path) -> String {
    // A drive-relative Windows path (`C:tile.tif`) keeps its native form too: written with '/'
    // it would read as `C:/tile.tif`, an absolute path to a different file.
    if path.is_absolute() || path.has_root() || matches!(path.components().next(), Some(std::path::Component::Prefix(_))) {
        return path.to_string_lossy().into_owned();
    }
    path.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}
impl DatasetIndex {
    fn needs_output_resolution(&self) -> bool {
        self.default_raster_name || !self.relative_locations.is_empty()
    }
    fn resolved_for_output(&self, filename: &str) -> Result<Self> {
        let mut named = self.clone();
        if self.default_raster_name {
            named.layer.name = raster_index_layer_name(filename);
        }
        named.default_raster_name = false;
        if !self.relative_locations.is_empty() {
            let parent =
                std::path::Path::new(filename).parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| std::path::Path::new("."));
            let parent = std::env::current_dir()?.join(parent);
            let parent = std::fs::canonicalize(&parent).unwrap_or_else(|_| lexical_path(&parent));
            // Relative locations are written portably either way: re-anchored to the catalog's
            // directory when it differs from the one they were given against, else as given.
            let moved = self.relative_base.as_ref() != Some(&parent);
            for feature in &mut named.layer.features {
                if let Some(FieldValue::Text(location)) = feature.attributes.first_mut()
                    && let Some((absolute, suffix)) = self.relative_locations.get(location)
                {
                    let relative = if moved {
                        relative_path(absolute, &parent)
                    } else {
                        std::path::PathBuf::from(location.strip_suffix(suffix.as_str()).unwrap_or(location))
                    };
                    *location = format!("{}{}", portable_location(&relative), suffix);
                }
            }
            named.source_locations.extend(self.relative_locations.values().map(|(p, _)| p.to_string_lossy().into_owned()));
            named.relative_locations.clear();
            named.relative_base = None;
        }
        Ok(named)
    }
    #[cfg(not(target_family = "wasm"))]
    fn track_relative_location(&mut self, location: &str, suffix: &str) -> Result<()> {
        if location.contains("://") || location.starts_with("/vsi") || std::path::Path::new(location).is_absolute() {
            return Ok(());
        }
        let base = std::env::current_dir()?;
        let joined = base.join(location);
        let absolute = std::fs::canonicalize(&joined).unwrap_or_else(|_| lexical_path(&joined));
        self.relative_locations.insert(format!("{location}{suffix}"), (absolute, suffix.into()));
        self.relative_base = Some(std::fs::canonicalize(&base).unwrap_or(base));
        Ok(())
    }
    /// Adds an opened local file to overwrite protection independently of its stored label.
    /// Relative paths are anchored now, so changing the working directory cannot bypass it.
    #[cfg(not(target_family = "wasm"))]
    pub fn protect_source_path(&mut self, path: &str) -> Result<()> {
        if path.contains("://") || path.starts_with("/vsi") {
            return Ok(());
        }
        let path = std::path::Path::new(path);
        let absolute = std::env::current_dir()?.join(path);
        self.source_locations.push(absolute.to_string_lossy().into_owned());
        Ok(())
    }
    /// Original source locators, including sources filtered out of the output.
    pub fn source_locations(&self) -> &[String] {
        &self.source_locations
    }

    /// Serializes the index. GeoPackage/FlatGeobuf retain GTI layer metadata;
    /// ordinary indexes may also use other formats that terrio writes in memory.
    pub fn to_bytes(&self, name: &str, options: &crate::VectorWriteOptions) -> Result<Vec<u8>> {
        if self.needs_output_resolution() {
            return self.resolved_for_output(name)?.to_bytes(name, options);
        }
        use crate::formats::VectorDriver;
        let driver = options
            .driver
            .or_else(|| {
                if name.to_ascii_lowercase().ends_with(".shp.zip") {
                    Some(VectorDriver::Shapefile)
                } else {
                    VectorDriver::for_extension(name)
                }
            })
            .ok_or_else(|| invalid("unknown index output format"))?;
        if driver == VectorDriver::GPKG {
            #[cfg(feature = "sqlite")]
            {
                let bytes = crate::drivers::vector::geopackage::to_bytes(std::slice::from_ref(&self.layer))?;
                return crate::drivers::vector::gpkg_edit::with_layer_metadata(bytes, &self.layer.name, &self.metadata);
            }
            #[cfg(not(feature = "sqlite"))]
            return Err(Error::disabled("sqlite"));
        }
        if driver == VectorDriver::FlatGeobuf {
            #[cfg(feature = "flatgeobuf")]
            {
                let object = self
                    .metadata
                    .iter()
                    .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                    .collect::<serde_json::Map<String, serde_json::Value>>();
                let text = serde_json::to_string(&object).map_err(|e| Error::InvalidData(e.to_string()))?;
                return crate::drivers::vector::flatgeobuf::to_bytes_with_metadata(&self.layer, Some(&text));
            }
            #[cfg(not(feature = "flatgeobuf"))]
            return Err(Error::disabled("flatgeobuf"));
        }
        if self.metadata.iter().any(|(key, _)| key != "LOCATION_FIELD" && key != "INTERLEAVE") {
            return Err(Error::Unsupported("GTI metadata needs GeoPackage or FlatGeobuf output".into()));
        }
        crate::memory::write_vector_bytes(std::slice::from_ref(&self.layer), name, options)
    }

    /// Writes a new index, or atomically replaces a single-file output if authorized.
    /// Multi-file formats (e.g. Shapefile) require a new basename; all parts are
    /// staged before publishing, and failed creation removes the newly written parts.
    /// Replacing indexed source paths and canonical aliases is refused (Unix
    /// hard-link aliases are detected too).
    #[cfg(not(target_family = "wasm"))]
    pub fn write(&self, path: &str, options: &crate::VectorWriteOptions, overwrite: bool) -> Result<()> {
        if self.needs_output_resolution() {
            return self.resolved_for_output(path)?.write(path, options, overwrite);
        }
        use std::io::Write;
        let output = std::path::Path::new(path);
        if output.exists() && !overwrite {
            return Err(invalid(format!("'{path}' exists; enable overwrite to replace it")));
        }
        if let Ok(real) = std::fs::canonicalize(output) {
            for location in &self.source_locations {
                if std::fs::canonicalize(location).is_ok_and(|p| p == real) {
                    return Err(invalid("an index output cannot replace an indexed source"));
                }
                #[cfg(unix)]
                if let (Ok(a), Ok(b)) = (std::fs::metadata(location), std::fs::metadata(output)) {
                    use std::os::unix::fs::MetadataExt;
                    if a.dev() == b.dev() && a.ino() == b.ino() {
                        return Err(invalid("an index output cannot replace an indexed source"));
                    }
                }
            }
        }
        let driver = options.driver.or_else(|| crate::formats::VectorDriver::for_extension(path));
        if !matches!(driver, Some(crate::formats::VectorDriver::GPKG | crate::formats::VectorDriver::FlatGeobuf))
            && !path.to_ascii_lowercase().ends_with(".shp.zip")
        {
            return self.write_parts(path, options, overwrite);
        }
        let bytes = self.to_bytes(path, options)?;
        let parent = output.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| std::path::Path::new("."));
        let existing_permissions = if overwrite {
            match std::fs::metadata(output) {
                Ok(metadata) => Some(metadata.permissions()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                Err(e) => return Err(e.into()),
            }
        } else {
            None
        };
        #[cfg_attr(not(unix), allow(unused_mut))]
        let mut builder = tempfile::Builder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // The umask applies to this mode, as it does to a newly created file.
            builder.permissions(std::fs::Permissions::from_mode(0o666));
        }
        let mut temp = builder.tempfile_in(parent)?;
        temp.write_all(&bytes)?;
        if let Some(permissions) = existing_permissions {
            temp.as_file().set_permissions(permissions)?;
        }
        temp.flush()?;
        temp.as_file().sync_all()?;
        if overwrite {
            temp.persist(output).map_err(|e| Error::Io(e.error))?;
        } else {
            temp.persist_noclobber(output).map_err(|e| Error::Io(e.error))?;
        }
        Ok(())
    }
    #[cfg(not(target_family = "wasm"))]
    fn write_parts(&self, path: &str, options: &crate::VectorWriteOptions, overwrite: bool) -> Result<()> {
        use std::io::Write;
        if self.metadata.iter().any(|(k, _)| k != "LOCATION_FIELD" && k != "INTERLEAVE") {
            return Err(Error::Unsupported("GTI metadata needs GeoPackage or FlatGeobuf output".into()));
        }
        let target = std::path::Path::new(path);
        let parent = target.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or_else(|| std::path::Path::new("."));
        let stage = tempfile::tempdir_in(parent)?;
        let staged = stage.path().join(target.file_name().ok_or_else(|| invalid("index output needs a filename"))?);
        crate::write_vector(
            std::slice::from_ref(&self.layer),
            staged.to_str().ok_or_else(|| invalid("index path must be UTF-8"))?,
            options,
        )?;
        let mut parts = std::fs::read_dir(stage.path())?.map(|e| e.map(|e| e.path())).collect::<std::result::Result<Vec<_>, _>>()?;
        parts.sort();
        if parts.is_empty() || parts.iter().any(|p| !p.is_file()) {
            return Err(Error::Unsupported("index driver did not produce a file dataset".into()));
        }
        let destinations: Vec<_> = parts.iter().map(|p| parent.join(p.file_name().expect("staged filename"))).collect();
        for destination in &destinations {
            if std::fs::symlink_metadata(destination).is_ok() && (!overwrite || parts.len() > 1) {
                return Err(invalid("output component exists; multi-file indexes must be written to a new basename"));
            }
        }
        let mut created = Vec::new();
        for (source, destination) in parts.iter().zip(destinations) {
            let publish = (|| -> Result<()> {
                let mut temp = tempfile::NamedTempFile::new_in(parent)?;
                std::io::copy(&mut std::fs::File::open(source)?, &mut temp)?;
                temp.flush()?;
                temp.as_file().sync_all()?;
                let permissions = if overwrite && parts.len() == 1 {
                    std::fs::metadata(&destination)
                        .or_else(|e| if e.kind() == std::io::ErrorKind::NotFound { std::fs::metadata(source) } else { Err(e) })?
                        .permissions()
                } else {
                    std::fs::metadata(source)?.permissions()
                };
                temp.as_file().set_permissions(permissions)?;
                if overwrite && parts.len() == 1 {
                    temp.persist(&destination).map_err(|e| Error::Io(e.error))?;
                } else {
                    temp.persist_noclobber(&destination).map_err(|e| Error::Io(e.error))?;
                }
                Ok(())
            })();
            if let Err(e) = publish {
                for file in created {
                    let _ = std::fs::remove_file(file);
                }
                return Err(e);
            }
            created.push(destination);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::{Grid, raster_header};
    struct NoPixels;
    impl crate::cog::WindowSource for NoPixels {
        fn overviews(&self) -> Vec<(usize, usize)> {
            Vec::new()
        }
        fn read(&mut self, _level: usize, _window: crate::Window, _bands: &[usize]) -> Result<crate::raster::RasterData> {
            panic!("index creation must not read raster pixels")
        }
    }
    fn raster() -> RasterIndexSource {
        let mut header = raster_header(
            Grid { width: 4, height: 6, geo_transform: [10., 2., 0., 20., 0., -3.] },
            crate::DataType::U8,
            1,
            None,
            &SpatialRef::parse("EPSG:32611").unwrap(),
        )
        .unwrap();
        header.set_geo_transform([10., 2., 0.5, 20., 0.25, -3.]);
        let lazy = LazyRaster::from_source(header, Box::new(NoPixels));
        RasterIndexSource::from_lazy("tile.tif", &lazy).unwrap()
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn relative_locations_use_forward_slashes_on_every_platform() {
        let relative: std::path::PathBuf = ["..", "tiles", "a.tif"].iter().collect();
        assert_eq!(portable_location(&relative), "../tiles/a.tif");
        let absolute = std::env::temp_dir().join("a.tif");
        assert_eq!(portable_location(&absolute), absolute.to_string_lossy());
    }
    #[cfg(windows)]
    #[test]
    fn drive_relative_locations_keep_their_native_form() {
        assert_eq!(portable_location(std::path::Path::new("C:tile.tif")), "C:tile.tif");
        assert_eq!(portable_location(std::path::Path::new(r"C:tiles\a.tif")), r"C:tiles\a.tif");
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn discovered_local_files_join_with_the_platform_separator() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("a.tif"), b"").unwrap();
        let base = dir.path().to_str().unwrap().to_string();
        let options = IndexInputOptions { recursive: true, ..Default::default() };
        let found = index_input_paths(&[base], &options).unwrap();
        assert_eq!(found, [dir.path().join("sub").join("a.tif").to_string_lossy().into_owned()]);
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn relative_sources_stay_portable_beside_the_catalog() {
        // The catalog is written in the directory the location was given against, so the location
        // is kept as given, with '/' separators (it only differs from the native form on Windows).
        let base = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&base).unwrap();
        let name = dir.path().file_name().unwrap().to_str().unwrap().to_string();
        std::fs::create_dir(dir.path().join("tiles")).unwrap();
        std::fs::write(dir.path().join("tiles").join("a.tif"), b"source").unwrap();
        let relative: std::path::PathBuf = [name.as_str(), "tiles", "a.tif"].iter().collect();
        let relative = relative.to_string_lossy().into_owned();
        let mut index = raster_index(&[raster()], &Default::default()).unwrap();
        index.layer.features[0].attributes[0] = FieldValue::Text(relative.clone());
        index.track_relative_location(&relative, "").unwrap();
        let resolved = index.resolved_for_output("index.gpkg").unwrap();
        assert_eq!(resolved.layer.features[0].attributes[0], FieldValue::Text(format!("{name}/tiles/a.tif")));
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn discovery_drops_doubled_separators_in_a_local_base() {
        // A trailing `//` was trimmed before; one in the middle of the base wasn't.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("a.tif"), b"").unwrap();
        let base = format!("{}//sub//", dir.path().to_str().unwrap());
        let found = index_input_paths(&[base], &IndexInputOptions::default()).unwrap();
        assert_eq!(found, [dir.path().join("sub").join("a.tif").to_string_lossy().into_owned()]);
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn relative_sources_resolve_from_sibling_catalog_directory() {
        let base = std::env::current_dir().unwrap();
        let dir = tempfile::tempdir_in(&base).unwrap();
        std::fs::create_dir(dir.path().join("tiles")).unwrap();
        std::fs::create_dir(dir.path().join("catalogs")).unwrap();
        let source = dir.path().join("tiles/a.tif");
        std::fs::write(&source, b"source").unwrap();
        let relative = source.strip_prefix(&base).unwrap().to_string_lossy();
        let mut index = raster_index(&[raster()], &Default::default()).unwrap();
        index.layer.features[0].attributes[0] = FieldValue::Text(relative.to_string());
        index.track_relative_location(&relative, "").unwrap();
        let resolved = index.resolved_for_output(dir.path().join("catalogs/index.gpkg").to_str().unwrap()).unwrap();
        let FieldValue::Text(location) = &resolved.layer.features[0].attributes[0] else { panic!("missing location") };
        assert!(!std::path::Path::new(location).is_absolute());
        assert_eq!(std::fs::canonicalize(dir.path().join("catalogs").join(location)).unwrap(), std::fs::canonicalize(source).unwrap());
    }
    #[test]
    fn raster_index_is_header_only_and_keeps_rotated_corners() {
        let out = raster_index(&[raster()], &Default::default()).unwrap();
        let Some(Geometry::Polygon { exterior, .. }) = &out.layer.features[0].geometry else { panic!("not a polygon") };
        assert_eq!(exterior.0.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(), [(10., 20.), (18., 21.), (21., 3.), (13., 2.)]);
        assert_eq!(out.metadata, [("LOCATION_FIELD".into(), "location".into())]);
        assert_eq!(out.layer.features[0].attributes, [FieldValue::Text("tile.tif".into())]);
        let filtered = raster_index(&[raster()], &RasterIndexOptions { min_pixel_size: Some(4.), ..Default::default() }).unwrap();
        assert!(filtered.layer.features.is_empty());
        // GDAL 3.13.3: sqrt((11*19)/(4*6)) = 2.950988534937629,
        // not sqrt(abs(affine determinant)) for a rotated grid.
        let retained = raster_index(
            &[raster()],
            &RasterIndexOptions {
                min_pixel_size: Some(2.7),
                fetch_metadata: vec![MetadataField::from_type_name("{PIXEL_SIZE}", "resolution", "Real").unwrap()],
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(retained.layer.features.len(), 1);
        let FieldValue::Float(value) = retained.layer.features[0].attributes[1] else { panic!("missing resolution") };
        assert!((value - 2.950988534937629).abs() < 1e-14);
    }
    #[test]
    fn raster_crs_policy_and_warped_envelopes_match_gdal() {
        let a = raster();
        let mut b = raster();
        b.location = "other.tif".into();
        b.crs = SpatialRef::parse("EPSG:3857").unwrap();
        let sources = [a, b];
        assert_eq!(raster_index(&sources, &Default::default()).unwrap().layer.len(), 1);
        let with_srs =
            RasterIndexOptions { index: IndexOptions { source_crs_field: Some("srs".into()), ..Default::default() }, ..Default::default() };
        assert_eq!(raster_index(&sources, &with_srs).unwrap().layer.len(), 2);
        let mut source = raster();
        source.crs = SpatialRef::parse("EPSG:4326").unwrap();
        let output = raster_index(
            &[source],
            &RasterIndexOptions {
                index: IndexOptions { output_crs: Some(SpatialRef::parse("EPSG:3857").unwrap()), ..Default::default() },
                ..Default::default()
            },
        )
        .unwrap();
        let Some(Geometry::Polygon { exterior, .. }) = &output.layer.features[0].geometry else { panic!("not polygon") };
        // GDAL 3.13.3's north-up virtual warp envelope for the rotated 4x6 grid.
        assert!((exterior.0[0].x - 1113194.9079327357).abs() < 1e-8);
        assert!((exterior.0[0].y - 2391878.5879443143).abs() < 1e-8);
        assert_eq!(exterior.0[0].y, exterior.0[1].y);
        assert_eq!(exterior.0[1].x, exterior.0[2].x);
    }
    #[test]
    fn insufficient_source_bands_are_rejected_before_gti_reads() {
        let mut two = raster();
        two.band_count = 2;
        let one = raster();
        let _ = crate::warnings::take();
        assert_eq!(raster_index(&[two.clone(), one.clone()], &Default::default()).unwrap().layer.len(), 2);
        assert!(crate::warnings::take().iter().any(|w| w.contains("different band counts")));
        let configured = RasterIndexOptions { gti: GtiOptions { band_count: Some(2), ..Default::default() }, ..Default::default() };
        let error = raster_index(&[two.clone(), one.clone()], &configured).unwrap_err();
        assert!(error.to_string().contains("fewer than the 2"));
        let subset = RasterIndexOptions { gti: GtiOptions { band_count: Some(1), ..Default::default() }, ..Default::default() };
        assert_eq!(raster_index(&[two, one], &subset).unwrap().layer.len(), 2);
    }
    #[test]
    fn accepted_mixed_vector_crs_warns_about_untransformed_coordinates() {
        let a = vector("a", 0);
        let mut b = vector("b", 0);
        SpatialRef::parse("EPSG:3857").unwrap().apply_to_layer(&mut b.layer);
        let _ = crate::warnings::take();
        let index = vector_index(&[a, b], &VectorIndexOptions { accept_different_crs: true, ..Default::default() }).unwrap();
        assert_eq!(index.layer.len(), 2);
        assert!(crate::warnings::take().iter().any(|w| w.contains("coordinates remain in their original CRS")));
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn vsi_locators_are_never_anchored_to_the_working_directory() {
        assert_eq!(stored_location("/vsizip/data.zip/a.tif", true).unwrap(), "/vsizip/data.zip/a.tif");
    }
    #[test]
    fn query_parameters_are_retained_but_never_echoed_in_the_warning() {
        let _ = crate::warnings::take();
        let mut source = raster();
        source.location = "https://example.test/tile.tif?sig=private-marker".into();
        let index = raster_index(&[source], &Default::default()).unwrap();
        assert_eq!(index.layer.features[0].attributes[0], FieldValue::Text("https://example.test/tile.tif?sig=private-marker".into()));
        let warnings = crate::warnings::take();
        assert!(warnings.iter().any(|w| w.contains("query parameters")));
        assert!(warnings.iter().all(|w| !w.contains("private-marker")));
    }
    #[test]
    fn skipped_and_mixed_crs_warnings_do_not_echo_url_queries() {
        let a = raster();
        let mut b = raster();
        b.location = "https://example.test/tile.tif?sig=private-marker".into();
        b.crs = SpatialRef::parse("EPSG:3857").unwrap();
        for source_crs_field in [None, Some("srs".into())] {
            let _ = crate::warnings::take();
            let options = RasterIndexOptions { index: IndexOptions { source_crs_field, ..Default::default() }, ..Default::default() };
            raster_index(&[a.clone(), b.clone()], &options).unwrap();
            let warnings = crate::warnings::take();
            assert!(warnings.iter().any(|w| w.contains("different CRS")));
            assert!(warnings.iter().all(|w| !w.contains("private-marker")));
        }
        for schema_mismatch in [false, true] {
            let a = vector("a", 0);
            let mut b = vector("https://example.test/data.gpkg?sig=private-marker", 0);
            SpatialRef::parse("EPSG:3857").unwrap().apply_to_layer(&mut b.layer);
            if schema_mismatch {
                b.layer.schema.add_field(FieldDef::new("extra", FieldType::Text));
            }
            let _ = crate::warnings::take();
            vector_index(&[a, b], &Default::default()).unwrap();
            let warnings = crate::warnings::take();
            assert!(warnings.iter().any(|w| w.contains("skipping")));
            assert!(warnings.iter().all(|w| !w.contains("private-marker")));
        }
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn failed_remote_discovery_does_not_echo_query_in_transport_error() {
        let _ = crate::warnings::take();
        let result = raster_index_paths(
            &["unsupported-scheme://example.test/a.tif?sig=private-marker".into()],
            &IndexInputOptions { skip_errors: true, ..Default::default() },
            &Default::default(),
        );
        assert!(result.is_err());
        let warnings = crate::warnings::take();
        assert!(warnings.iter().any(|w| w.contains("cannot inspect remote source")));
        assert!(warnings.iter().all(|w| !w.contains("private-marker")));
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn returned_open_and_listing_errors_do_not_echo_queries() {
        for path in ["/vsimem/missing-index-test.tif?sig=private-marker", "unsupported-scheme://example.test/a.tif?sig=private-marker"] {
            let error = raster_index_paths(&[path.into()], &Default::default(), &Default::default()).unwrap_err();
            assert!(!error.to_string().contains("private-marker"));
            assert!(!format!("{error:?}").contains("private-marker"));
        }
        let path = "unsupported-scheme://example.test/dir/?sig=private-marker";
        let error = index_input_paths(&[path.into()], &Default::default()).unwrap_err();
        assert!(!error.to_string().contains("private-marker"));
        assert!(!format!("{error:?}").contains("private-marker"));
        let error = vector_index_sources("/vsimem/missing-index-test.geojson?sig=private-marker").unwrap_err();
        assert!(!error.to_string().contains("private-marker"));
    }
    #[test]
    fn selectors_apply_to_the_union_of_input_datasets() {
        let mut a = vector("a.gpkg", 0);
        a.layer.name = "roads".into();
        let mut b = vector("b.gpkg", 0);
        b.layer.name = "counties".into();
        let selection = VectorIndexSelection { names: vec!["roads".into()], indexes: Vec::new() };
        let selected = selection.apply(vec![a, b]).unwrap();
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].location, "a.gpkg");
    }
    #[test]
    fn default_layer_names_are_resolved_at_serialization() {
        assert_eq!(raster_index_layer_name("path/catalog.shp.zip"), "catalog");
        let index = raster_index(&[raster()], &Default::default()).unwrap();
        for filename in ["catalog.gpkg", "catalog.fgb", "catalog.shp.zip"] {
            let bytes = index.to_bytes(filename, &Default::default()).unwrap();
            let crate::Dataset::Vector(dataset) = crate::memory::open_bytes(bytes, filename).unwrap() else { panic!("not vector") };
            assert_eq!(dataset.layers[0].name, "catalog");
        }
        let options =
            RasterIndexOptions { index: IndexOptions { layer_name: Some("explicit".into()), ..Default::default() }, ..Default::default() };
        let index = raster_index(&[raster()], &options).unwrap();
        let bytes = index.to_bytes("catalog.gpkg", &Default::default()).unwrap();
        let crate::Dataset::Vector(dataset) = crate::memory::open_bytes(bytes, "catalog.gpkg").unwrap() else { panic!("not vector") };
        assert_eq!(dataset.layers[0].name, "explicit");
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn wildcard_stars_are_patterns_even_when_names_contain_stars() {
        for (pattern, name) in [("a*", "a*b"), ("*a*", "*ab"), ("*.tif", "a*.tif"), ("a?b", "a*b")] {
            assert!(wildcard(pattern, name));
        }
        assert!(!wildcard("a*b", "a*c"));
    }
    #[test]
    fn gti_counts_use_inferred_source_band_count() {
        let mut options = RasterIndexOptions::default();
        options.gti.nodata = vec![1., 2., 3.];
        assert!(raster_index(&[raster()], &options).is_err());
        options.gti.nodata = vec![1.];
        options.gti.color_interpretations = vec!["red".into(), "green".into(), "blue".into()];
        assert!(raster_index(&[raster()], &options).is_err());
        options.gti.band_count = Some(3);
        let mut rgb = raster();
        rgb.band_count = 3;
        assert!(raster_index(&[rgb], &options).is_ok());
        options.gti.band_count = None;
        options.gti.color_interpretations = vec!["gray".into()];
        assert!(raster_index(&[raster()], &options).is_ok());
    }
    #[cfg(unix)]
    #[test]
    fn index_outputs_honor_umask_and_preserve_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let control = dir.path().join("control");
        std::fs::write(&control, b"control").unwrap();
        let expected = std::fs::metadata(control).unwrap().permissions().mode() & 0o777;
        let index = vector_index(&[vector("source.gpkg", 0)], &Default::default()).unwrap();
        for suffix in ["gpkg", "fgb", "shp.zip", "geojson"] {
            let path = dir.path().join(format!("index.{suffix}"));
            index.write(path.to_str().unwrap(), &Default::default(), false).unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, expected);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
            index.write(path.to_str().unwrap(), &Default::default(), true).unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
        }
    }
    fn vector(name: &str, index: usize) -> VectorIndexSource {
        let mut layer = Layer::new("source");
        layer.extent = Some(wbvector::BBox::new(1., 2., 3., 4.));
        layer.schema.add_field(FieldDef::new("id", FieldType::Integer));
        VectorIndexSource { location: name.into(), layer_index: index, layer }
    }
    #[test]
    fn vector_locations_preserve_original_layer_numbers_and_schema_policy() {
        let mut b = vector("b.gpkg", 3);
        b.layer.schema.add_field(FieldDef::new("other", FieldType::Text));
        let sources = [vector("a.gpkg", 0), b];
        let out = vector_index(&sources, &Default::default()).unwrap();
        assert_eq!(out.layer.features.len(), 1);
        let duplicate = vector_index(&[vector("same.gpkg", 0), vector("same.gpkg", 0)], &Default::default()).unwrap();
        assert_eq!(duplicate.layer.len(), 2);
        let out = vector_index(&sources, &VectorIndexOptions { accept_different_schemas: true, ..Default::default() }).unwrap();
        assert_eq!(out.layer.features[1].attributes, [FieldValue::Text("b.gpkg,3".into())]);
        let Some(Geometry::Polygon { exterior, .. }) = &out.layer.features[1].geometry else { panic!("not a polygon") };
        assert_eq!(exterior.0.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>(), [(1., 2.), (1., 4.), (3., 4.), (3., 2.)]);
    }
    #[test]
    fn metadata_options_are_validated_before_writing() {
        let bad = RasterIndexOptions { gti: GtiOptions { resolution: Some([0., 2.]), ..Default::default() }, ..Default::default() };
        assert!(raster_index(&[raster()], &bad).is_err());
        let bad = RasterIndexOptions {
            fetch_metadata: vec![MetadataField { key: "x".into(), name: "LOCATION".into(), field_type: FieldType::Text }],
            ..Default::default()
        };
        assert!(raster_index(&[raster()], &bad).is_err());
        // The source is one band, so a three-value nodata list is refused even without band_count.
        let bad = RasterIndexOptions { gti: GtiOptions { nodata: vec![0., 0., 0.], ..Default::default() }, ..Default::default() };
        assert!(raster_index(&[raster()], &bad).is_err());
        let bad = RasterIndexOptions {
            gti: GtiOptions { color_interpretations: vec!["gray".into(); 2], ..Default::default() },
            ..Default::default()
        };
        assert!(raster_index(&[raster()], &bad).is_err());
        let ok = RasterIndexOptions { gti: GtiOptions { nodata: vec![0.], ..Default::default() }, ..Default::default() };
        assert!(raster_index(&[raster()], &ok).is_ok());
    }
    #[cfg(all(feature = "sqlite", not(target_family = "wasm")))]
    #[test]
    fn overwriting_an_indexed_source_is_refused_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.gpkg");
        let path = path.to_str().unwrap();
        std::fs::write(path, b"untouched source").unwrap();
        let out = vector_index(&[vector(path, 0)], &Default::default()).unwrap();
        assert!(out.write(path, &Default::default(), true).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"untouched source");
        let output = dir.path().join("index.gpkg");
        out.write(output.to_str().unwrap(), &Default::default(), false).unwrap();
        let before = std::fs::read(&output).unwrap();
        assert!(out.write(output.to_str().unwrap(), &Default::default(), false).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), before);
    }
    #[cfg(all(feature = "sqlite", unix))]
    #[test]
    fn written_index_gets_the_default_file_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let baseline = dir.path().join("baseline");
        std::fs::File::create(&baseline).unwrap();
        let output = dir.path().join("index.gpkg");
        raster_index(&[raster()], &Default::default()).unwrap().write(output.to_str().unwrap(), &Default::default(), false).unwrap();
        assert_eq!(mode(&output), mode(&baseline));
        std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o640)).unwrap();
        raster_index(&[raster()], &Default::default()).unwrap().write(output.to_str().unwrap(), &Default::default(), true).unwrap();
        assert_eq!(mode(&output), 0o640);
    }
    #[cfg(not(target_family = "wasm"))]
    #[test]
    fn filename_filters_match_a_literal_star_in_names() {
        assert!(wildcard("a*", "a*b"));
        assert!(wildcard("*", "a*"));
        assert!(wildcard("a?", "a*"));
        assert!(!wildcard("a*c", "a*b"));
    }
}

/// Discovery of native input files before creating an index.
#[derive(Debug, Clone, Default)]
pub struct IndexInputOptions {
    /// Scan subdirectories too.
    pub recursive: bool,
    /// Case-sensitive `*`/`?` filename filters for directory entries.
    pub filename_filters: Vec<String>,
    /// Store absolute local paths (remote locators are kept unchanged).
    pub absolute_path: bool,
    /// Skip unreadable/unsupported raster inputs with a warning.
    pub skip_errors: bool,
}
#[cfg(not(target_family = "wasm"))]
fn wildcard(pattern: &str, value: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let v: Vec<char> = value.chars().collect();
    let (mut i, mut j, mut star, mut consumed) = (0, 0, None, 0);
    while j < v.len() {
        if i < p.len() && p[i] == '*' {
            star = Some(i);
            i += 1;
            consumed = j;
        } else if i < p.len() && (p[i] == '?' || p[i] == v[j]) {
            i += 1;
            j += 1;
        } else if let Some(s) = star {
            i = s + 1;
            consumed += 1;
            j = consumed;
        } else {
            return false;
        }
    }
    while i < p.len() && p[i] == '*' {
        i += 1;
    }
    i == p.len()
}
/// Expands local directories without opening their raster/vector contents.
/// Remote directories can be listed explicitly by a trailing slash. Filters
/// apply to discovered entries; explicit filenames are always retained.
#[cfg(not(target_family = "wasm"))]
pub fn index_input_paths(paths: &[String], o: &IndexInputOptions) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for path in paths {
        let remote = path.contains("://") || path.starts_with("/vsi");
        let (base, query) = if remote { path.split_once('?').unwrap_or((path, "")) } else { (path.as_str(), "") };
        let directory = if remote { base.ends_with('/') } else { std::path::Path::new(path).is_dir() };
        if !directory {
            out.push(path.clone());
            continue;
        }
        let entries = crate::vsi::list(path, &crate::vsi::ListOptions { recursive: o.recursive, ..Default::default() })
            .map_err(|e| index_source_error(path, e))?;
        // Collecting the components drops doubled and trailing separators (`tiles//` gives `tiles/a.tif`).
        let local_base: std::path::PathBuf = std::path::Path::new(base).components().collect();
        for entry in entries.into_iter().filter(|e| !e.is_dir()) {
            let name = entry.name.rsplit('/').next().unwrap_or(&entry.name);
            if !o.filename_filters.is_empty() && !o.filename_filters.iter().any(|p| wildcard(p, name)) {
                continue;
            }
            let suffix = if query.is_empty() { String::new() } else { format!("?{query}") };
            if remote {
                out.push(format!("{}/{}{suffix}", base.trim_end_matches('/'), entry.name));
            } else {
                // Local entries join with the platform separator, as the directory path itself does.
                // `Path::join` of the '/'-separated entry name would leave a mixed `dir\sub/a.tif`
                // on Windows, so its parts are joined one by one.
                let joined = entry.name.split('/').filter(|part| !part.is_empty()).fold(local_base.clone(), |p, part| p.join(part));
                out.push(joined.to_string_lossy().into_owned());
            }
        }
    }
    Ok(out)
}
#[cfg(not(target_family = "wasm"))]
fn stored_location(path: &str, absolute: bool) -> Result<String> {
    if !absolute || path.contains("://") || path.starts_with("/vsi") || std::path::Path::new(path).is_absolute() {
        return Ok(path.into());
    }
    Ok(std::env::current_dir()?.join(path).to_string_lossy().into_owned())
}
/// Opens raster headers and builds their index. Lazy-capable drivers do not
/// decode pixels; other formats use their existing whole-reader fallback.
#[cfg(not(target_family = "wasm"))]
pub fn raster_index_paths(paths: &[String], inputs: &IndexInputOptions, o: &RasterIndexOptions) -> Result<DatasetIndex> {
    let paths = index_input_paths(paths, inputs)?;
    let mut sources = Vec::new();
    for path in &paths {
        let result = (|| RasterIndexSource::open_path(path, stored_location(path, inputs.absolute_path)?))();
        match result {
            Ok(source) => sources.push(source),
            Err(e) if inputs.skip_errors => warn_skipped_source(path, &e),
            Err(e) => return Err(index_source_error(path, e)),
        }
    }
    let mut output = raster_index(&sources, o)?;
    for path in &paths {
        output.protect_source_path(path)?;
    }
    for source in &sources {
        output.track_relative_location(&source.location, "")?;
    }
    Ok(output)
}
/// Source layer selection for vector index creation. Names and zero-based
/// indexes form a union across the supplied source collection. A missing
/// selector is an error only when absent from every input dataset.
#[derive(Debug, Clone, Default)]
pub struct VectorIndexSelection {
    /// Original layer names.
    pub names: Vec<String>,
    /// Original zero-based layer numbers.
    pub indexes: Vec<usize>,
}
#[cfg(not(target_family = "wasm"))]
fn read_vector_index_headers(path: &str) -> Result<Vec<Layer>> {
    // Retain cached extents without decoding feature geometries where available.
    #[cfg(feature = "sqlite")]
    if crate::formats::extension(path).as_deref() == Some("gpkg") && std::path::Path::new(path).is_file() {
        use crate::drivers::sqlite::Value;
        use crate::drivers::vector::gpkg_read as gp;
        let db = crate::drivers::sqlite_edit::open_consistent(std::path::Path::new(path))?;
        let (contents, _) = gp::contents(&db)?;
        let table = db.table("gpkg_contents")?;
        let mut layers = Vec::new();
        for content in contents {
            let mut layer = gp::layer_def(&db, &content)?.layer;
            layer.extent =
                table.rows.iter().find(|r| table.get(r, "table_name").and_then(Value::as_str) == Some(content.name.as_str())).and_then(
                    |r| {
                        Some(wbvector::BBox::new(
                            table.get(r, "min_x")?.as_f64()?,
                            table.get(r, "min_y")?.as_f64()?,
                            table.get(r, "max_x")?.as_f64()?,
                            table.get(r, "max_y")?.as_f64()?,
                        ))
                    },
                );
            if layer.extent.is_none() {
                layer = scan_vector_index_header(path, &content.name)?;
            }
            layers.push(layer);
        }
        return Ok(layers);
    }
    #[cfg(feature = "flatgeobuf")]
    if crate::formats::extension(path).as_deref() == Some("fgb") && std::path::Path::new(path).is_file() {
        let mut file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        let (info, mut layer) = crate::drivers::vector::flatgeobuf::read_header(&mut file, len)?;
        if let Some([a, b, c, d]) = info.extent {
            layer.extent = Some(wbvector::BBox::new(a, b, c, d));
            return Ok(vec![layer]);
        }
    }
    let mut readers = crate::stream::open_layers(path, &Default::default(), crate::stream::DEFAULT_CHUNK)?;
    readers.iter_mut().map(scan_vector_reader).collect()
}
#[cfg(not(target_family = "wasm"))]
fn scan_vector_index_header(path: &str, layer: &str) -> Result<Layer> {
    let options = crate::VectorOpenOptions { layers: vec![layer.into()], ..Default::default() };
    let mut readers = crate::stream::open_layers(path, &options, crate::stream::DEFAULT_CHUNK)?;
    scan_vector_reader(readers.first_mut().ok_or_else(|| invalid("no requested vector layer"))?)
}
#[cfg(not(target_family = "wasm"))]
fn scan_vector_reader(reader: &mut crate::stream::FeatureReader) -> Result<Layer> {
    let mut layer = reader.layer().clone();
    while let Some(chunk) = reader.read_chunk(crate::stream::DEFAULT_CHUNK)? {
        for bounds in chunk.features.iter().filter_map(|f| f.geometry.as_ref()?.bbox()) {
            match &mut layer.extent {
                None => layer.extent = Some(bounds),
                Some(b) => {
                    b.min_x = b.min_x.min(bounds.min_x);
                    b.min_y = b.min_y.min(bounds.min_y);
                    b.max_x = b.max_x.max(bounds.max_x);
                    b.max_y = b.max_y.max(bounds.max_y);
                }
            }
        }
    }
    Ok(layer)
}
/// Creates vector-layer indexes from paths. Cached GeoPackage/FlatGeobuf bounds
/// are read as metadata; other formats scan features in bounded chunks where
/// their reader supports streaming.
#[cfg(not(target_family = "wasm"))]
pub fn vector_index_paths(
    paths: &[String],
    inputs: &IndexInputOptions,
    selection: &VectorIndexSelection,
    o: &VectorIndexOptions,
) -> Result<DatasetIndex> {
    let paths = index_input_paths(paths, inputs)?;
    let mut sources = Vec::new();
    for path in &paths {
        let mut group = vector_index_sources(path)?;
        let location = stored_location(path, inputs.absolute_path)?;
        for source in &mut group {
            source.location = location.clone();
        }
        sources.extend(group);
    }
    let sources = selection.apply(sources)?;
    let mut output = vector_index(&sources, o)?;
    for path in &paths {
        output.protect_source_path(path)?;
    }
    for source in &sources {
        let suffix = if o.dataset_name_only { String::new() } else { format!(",{}", source.layer_index) };
        output.track_relative_location(&source.location, &suffix)?;
    }
    Ok(output)
}

impl VectorIndexSelection {
    /// Selects source layers without renumbering them; names/numbers form a union.
    pub fn apply(&self, sources: Vec<VectorIndexSource>) -> Result<Vec<VectorIndexSource>> {
        for name in &self.names {
            if !sources.iter().any(|s| &s.layer.name == name) {
                return Err(invalid(format!("no source layer '{name}'")));
            }
        }
        for index in &self.indexes {
            if !sources.iter().any(|s| &s.layer_index == index) {
                return Err(invalid(format!("no source layer {index}")));
            }
        }
        Ok(sources
            .into_iter()
            .filter(|s| {
                self.names.is_empty() && self.indexes.is_empty()
                    || self.names.contains(&s.layer.name)
                    || self.indexes.contains(&s.layer_index)
            })
            .collect())
    }
}
/// Reads source layer schemas and bounds, retaining original layer numbers.
#[cfg(not(target_family = "wasm"))]
pub fn vector_index_sources(path: &str) -> Result<Vec<VectorIndexSource>> {
    Ok(read_vector_index_headers(path)
        .map_err(|e| index_source_error(path, e))?
        .iter()
        .enumerate()
        .map(|(index, layer)| VectorIndexSource::from_layer(path, index, layer))
        .collect())
}

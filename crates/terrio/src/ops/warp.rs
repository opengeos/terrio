//! Reprojection and resampling: an equivalent of `gdalwarp`.
//!
//! The output grid, the coordinate transformation and the resampling kernels follow GDAL's warper
//! closely so that results can be compared pixel for pixel:
//!
//! - the default output grid is GDAL's `GDALSuggestedWarpOutput2()`, followed by `gdalwarp`'s
//!   adjustments for `-te`, `-tr`, `-ts` and `-tap`;
//! - destination pixel centres are mapped to the source through GDAL's approximate transformer
//!   (linear interpolation along each output row, with a maximum error of 0.125 pixel), on the
//!   same chunks of the output GDAL processes (64 MB of working memory);
//! - the kernels (nearest, bilinear, cubic, cubic spline, Lanczos and the area-based methods)
//!   reproduce GDAL's warp kernel, including its treatment of nodata and of image edges.

use super::Ct as Transformer;
use super::overview::OverviewSelection;
use super::{Grid, Resampling, apply_gt, invert_gt, is_nodata, warp_round};
use crate::crs::{SpatialRef, TransformOptions};
use crate::{Error, RasterDataset, Result};
use wbraster::DataType;
use wbraster::raster::RasterData;

/// Nodata handling for an input or the output.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Nodata {
    /// Use the value the dataset declares (for the output: the source's).
    #[default]
    Auto,
    /// Ignore any declared value.
    None,
    /// These values, one per band (the last one repeats for the remaining bands).
    Values(Vec<f64>),
    /// Per-band values, with `None` disabling nodata for that band. The last repeats.
    PerBand(Vec<Option<f64>>),
}

impl Nodata {
    /// Parses `none` or a space- or comma-separated list of values (`nan` allowed).
    pub fn parse(s: &str) -> Result<Self> {
        if s.trim().eq_ignore_ascii_case("none") {
            return Ok(Self::None);
        }
        let values = s
            .split([' ', ','])
            .filter(|t| !t.is_empty())
            .map(|t| t.parse::<f64>().map_err(|_| Error::InvalidArgument(format!("invalid nodata value '{t}'"))))
            .collect::<Result<Vec<_>>>()?;
        if values.is_empty() {
            return Err(Error::InvalidArgument("empty nodata value".into()));
        }
        Ok(Self::Values(values))
    }

    pub(crate) fn value(&self, band: usize) -> Option<f64> {
        match self {
            Self::Values(v) => v.get(band).or(v.last()).copied(),
            Self::PerBand(v) => v.get(band).or(v.last()).copied().flatten(),
            _ => None,
        }
    }
}

/// Polygons, each a list of rings (exterior first), each a list of `(x, y)` vertices.
pub type Polygons = Vec<Vec<Vec<(f64, f64)>>>;

/// A cutline: polygons whose outside is masked out of the sources.
#[derive(Debug, Clone, Default)]
pub struct Cutline {
    /// Polygons, each a list of rings (exterior first), each a list of `(x, y)` vertices.
    pub polygons: Polygons,
    /// The CRS of the coordinates; the source's when unknown.
    pub srs: Option<SpatialRef>,
}

impl Cutline {
    /// The union of the polygons of all features of a vector dataset's layers.
    pub fn from_vector(ds: &crate::VectorDataset) -> Result<Self> {
        let mut polygons = Vec::new();
        let mut srs = None;
        for layer in &ds.layers {
            let s = SpatialRef::from_layer(layer);
            if srs.is_none() && !s.is_unknown() {
                srs = Some(s);
            }
            for f in &layer.features {
                if let Some(g) = &f.geometry {
                    collect_polygons(g, &mut polygons);
                }
            }
        }
        if polygons.is_empty() {
            return Err(Error::InvalidArgument("the cutline has no polygons".into()));
        }
        Ok(Self { polygons, srs })
    }

    /// Reads a cutline from a vector file.
    pub fn open(path: &str) -> Result<Self> {
        Self::from_vector(&crate::open_vector(path, &Default::default())?)
    }

    fn map(&self, f: &dyn Fn(f64, f64) -> Option<(f64, f64)>) -> Result<Polygons> {
        self.polygons
            .iter()
            .map(|p| {
                p.iter()
                    .map(|r| r.iter().map(|&(x, y)| f(x, y).ok_or_else(|| Error::Crs("cannot transform the cutline".into()))).collect())
                    .collect()
            })
            .collect()
    }
}

fn collect_polygons(g: &wbvector::Geometry, out: &mut Polygons) {
    use wbvector::Geometry as G;
    let ring = |r: &wbvector::geometry::Ring| r.0.iter().map(|c| (c.x, c.y)).collect::<Vec<_>>();
    match g {
        G::Polygon { exterior, interiors } => out.push(std::iter::once(exterior).chain(interiors).map(ring).collect()),
        G::MultiPolygon(ps) => {
            for (e, i) in ps {
                out.push(std::iter::once(e).chain(i).map(ring).collect());
            }
        }
        G::GeometryCollection(gs) => gs.iter().for_each(|g| collect_polygons(g, out)),
        _ => {}
    }
}

/// GDAL raster arithmetic version, shared by warp resampling and terrain operations.
/// The default retains Terrio's GDAL 3.12 results.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KernelVersion {
    /// Window-ratio scales and the GDAL 3.12 bilinear/cubic threshold.
    #[default]
    Gdal312,
    /// Local transformed-square scales and the GDAL 3.13 bilinear/cubic threshold.
    Gdal313,
}

impl KernelVersion {
    /// Parse a supported GDAL version.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "3.12" => Ok(Self::Gdal312),
            "3.13" => Ok(Self::Gdal313),
            _ => Err(Error::InvalidArgument("kernel version must be 3.12 or 3.13".into())),
        }
    }
}

/// Options for [`warp`], named after `gdalwarp`'s.
#[derive(Debug, Clone)]
pub struct WarpOptions {
    /// GDAL kernel behavior to reproduce, 3.12 by default.
    pub kernel_version: KernelVersion,
    /// Coordinate operation selection controls.
    pub transform_options: crate::crs::TransformOptions,
    /// Require geolocation-array georeferencing (`-geoloc`). Arrays are used automatically
    /// when present, unless GCPs or RPCs were explicitly supplied.
    pub geoloc: bool,
    /// An in-memory coordinate-array transformer for the first source, including WASM inputs.
    pub geolocation: Option<super::geolocation::GeolocationTransformer>,
    /// The sources' CRS, overriding what they declare (`-s_srs`).
    pub src_srs: Option<SpatialRef>,
    /// An explicit source affine transform, for serialized VRT transformers.
    pub src_transform: Option<[f64; 6]>,
    /// An explicit GDAL-order homography, for serialized VRT transformers.
    pub homography: Option<[f64; 9]>,
    /// The output CRS (`-t_srs`). By default, the first source's.
    pub dst_srs: Option<SpatialRef>,
    /// The output resolution `(x, y)`, both positive (`-tr`).
    pub resolution: Option<(f64, f64)>,
    /// The output extent `(min_x, min_y, max_x, max_y)` (`-te`).
    pub extent: Option<(f64, f64, f64, f64)>,
    /// The CRS of [`WarpOptions::extent`], when not the output's (`-te_srs`).
    pub extent_srs: Option<SpatialRef>,
    /// The output size `(width, height)`; a 0 is computed from the other (`-ts`).
    pub size: Option<(usize, usize)>,
    /// The output geotransform, in GDAL's order: any affine grid, rotated or not, of
    /// [`WarpOptions::size`] pixels (which it needs, both non-zero). Excludes the extent, the
    /// resolution, `-tap` and cropping to the cutline.
    pub dst_transform: Option<[f64; 6]>,
    /// Align the output extent on multiples of the resolution (`-tap`; needs a resolution).
    pub target_aligned_pixels: bool,
    /// The resampling method (`-r`).
    pub resampling: Resampling,
    /// Which stored source resolution to read (`-ovr`), automatic by default.
    pub overview: OverviewSelection,
    /// Source nodata (`-srcnodata`).
    pub src_nodata: Nodata,
    /// Override unified source validity. By default explicit `Values` unify bands;
    /// inherited and per-band nodata use independent masks plus shared validity when all
    /// bands are nodata (`PARTIAL`). `Some(false)` disables that shared mask (`NO`).
    pub unified_src_nodata: Option<bool>,
    /// Output nodata (`-dstnodata`). By default, the source nodata.
    pub dst_nodata: Nodata,
    /// The output data type (`-ot`). By default, the first source's.
    pub output_type: Option<DataType>,
    /// Mask the sources outside these polygons (`-cutline`).
    pub cutline: Option<Cutline>,
    /// Set the output extent to the cutline's (`-crop_to_cutline`).
    pub crop_to_cutline: bool,
    /// Maximum error, in pixels, of the approximate transformer (`-et`; 0 for exact transforms).
    pub error_threshold: f64,
    /// Working memory per chunk, in bytes (`-wm`), which decides how the output is split.
    pub memory_limit: f64,
    /// Ground control points georeferencing the (first) source in place of its geotransform,
    /// in the source CRS: warped with a GCP polynomial or thin plate splines, as `gdalwarp` warps
    /// a raster with GCPs.
    pub gcps: Vec<crate::Gcp>,
    /// How the GCPs georeference the source: GDAL's choice (a homography from 4 or 5 GCPs, else
    /// a polynomial), a polynomial (`-order`), thin plate splines (`-tps`) or a homography.
    pub gcp_method: super::gcp::GcpMethod,
    /// The order of the GCP polynomial (`-order`: 1, 2 or 3; 0 picks GDAL's default).
    pub order: u32,
    /// Rational polynomial coefficients georeferencing the (first) source in place of its
    /// geotransform (and of [`WarpOptions::gcps`]): warped with GDAL's RPC transformer, as
    /// `gdalwarp -rpc` warps. The source CRS is then WGS 84 unless [`WarpOptions::src_srs`] says
    /// otherwise.
    pub rpc: Option<super::rpc::Rpc>,
    /// The options of the RPC transformer (`-to RPC_HEIGHT=...`, `-to RPC_DEM=...`).
    pub rpc_options: super::rpc::RpcOptions,
    /// Extra source pixels read around each chunk's source window (GDAL's `SOURCE_EXTRA` warp
    /// option, which `gdalwarp` sets to 5 for RPCs).
    pub source_extra: Option<usize>,
    /// Find each chunk's source window from every point of its edges (GDAL's
    /// `SAMPLE_STEPS=ALL`, which `gdalwarp` sets with an RPC DEM).
    pub sample_steps_all: bool,
    /// Warp only the window of the output that GDAL's suggested output for the (first) source
    /// covers, and 5 pixels around it: what `gdalwarp` does for a source with RPCs written to a
    /// new GeoTIFF (`SKIP_NOSOURCE=YES`, `AdjustOutputExtentForRPC`), so that pixels the RPCs
    /// map back into the image from beyond its footprint stay empty.
    pub rpc_output_window: bool,
}

impl Default for WarpOptions {
    fn default() -> Self {
        Self {
            kernel_version: KernelVersion::default(),
            transform_options: Default::default(),
            geoloc: false,
            geolocation: None,
            src_srs: None,
            src_transform: None,
            homography: None,
            dst_srs: None,
            resolution: None,
            extent: None,
            extent_srs: None,
            size: None,
            dst_transform: None,
            target_aligned_pixels: false,
            resampling: Resampling::Nearest,
            overview: OverviewSelection::Auto,
            src_nodata: Nodata::Auto,
            unified_src_nodata: None,
            dst_nodata: Nodata::Auto,
            output_type: None,
            cutline: None,
            crop_to_cutline: false,
            error_threshold: 0.125,
            memory_limit: 64.0 * 1024.0 * 1024.0,
            gcps: Vec::new(),
            gcp_method: super::gcp::GcpMethod::Auto,
            order: 0,
            rpc: None,
            rpc_options: super::rpc::RpcOptions::default(),
            source_extra: None,
            sample_steps_all: false,
            rpc_output_window: false,
        }
    }
}

// ─── Coordinate transformation between pixel spaces ─────────────────────────────────────────────

/// How a source's pixels are georeferenced in place of its geotransform.
#[derive(Clone)]
enum Georef {
    /// By longitude/latitude coordinate arrays.
    Geolocation(std::sync::Arc<super::geolocation::GeolocationTransformer>),
    /// By GCPs.
    Gcp(super::gcp::GcpTransformer),
    /// By RPCs, to WGS 84 longitudes and latitudes.
    Rpc(std::sync::Arc<super::rpc::RpcTransformer>),
}

/// Maps source pixel/line coordinates to destination pixel/line coordinates and back, through
/// the CRS transformation: GDAL's `GDALGenImgProjTransform`.
struct GenImgProj {
    src_gt: [f64; 6],
    src_inv: [f64; 6],
    /// Source pixels to georeferenced coordinates by GCPs or RPCs, in place of the geotransform.
    src_gcp: Option<Georef>,
    /// Full-resolution source pixels per selected overview pixel, for GCP/RPC transforms.
    src_scale: (f64, f64),
    dst_gt: [f64; 6],
    dst_inv: [f64; 6],
    /// Source CRS to destination CRS, and back; `None` when both are the same.
    ct: Option<(Transformer, Transformer)>,
}

const IDENTITY: [f64; 6] = [0.0, 1.0, 0.0, 0.0, 0.0, 1.0];

impl GenImgProj {
    fn new(src_gt: [f64; 6], src_srs: &SpatialRef, dst_srs: &SpatialRef) -> Result<Self> {
        Self::with_options(src_gt, src_srs, dst_srs, &Default::default())
    }

    fn with_options(src_gt: [f64; 6], src_srs: &SpatialRef, dst_srs: &SpatialRef, options: &TransformOptions) -> Result<Self> {
        let src_inv = invert_gt(&src_gt).ok_or_else(|| Error::InvalidData("the source geotransform is not invertible".into()))?;
        let ct = if same_crs(src_srs, dst_srs) {
            None
        } else {
            Some((Transformer::with_options(src_srs, dst_srs, options)?, Transformer::with_options(dst_srs, src_srs, options)?))
        };
        Ok(Self { src_gt, src_inv, src_gcp: None, src_scale: (1.0, 1.0), dst_gt: IDENTITY, dst_inv: IDENTITY, ct })
    }

    /// A transformer whose source is georeferenced by GCPs or RPCs.
    fn with_georef(src_srs: &SpatialRef, dst_srs: &SpatialRef, g: Georef) -> Result<Self> {
        let mut t = Self::new(IDENTITY, src_srs, dst_srs)?;
        t.src_gcp = Some(g);
        Ok(t)
    }

    /// A transformer whose source is georeferenced by GCPs.
    fn with_gcps(src_srs: &SpatialRef, dst_srs: &SpatialRef, gcps: super::gcp::GcpTransformer) -> Result<Self> {
        Self::with_georef(src_srs, dst_srs, Georef::Gcp(gcps))
    }

    /// Source pixel/line to source georeferenced coordinates.
    fn src_to_geo(&self, c: f64, r: f64) -> Option<(f64, f64)> {
        let (c, r) = if self.src_gcp.is_some() { (c * self.src_scale.0, r * self.src_scale.1) } else { (c, r) };
        match &self.src_gcp {
            Some(Georef::Geolocation(g)) => g.to_geo(c, r),
            Some(Georef::Gcp(g)) => Some(g.to_geo(c, r)),
            Some(Georef::Rpc(t)) => t.to_geo(c, r, 0.0),
            None => Some(apply_gt(&self.src_gt, c, r)),
        }
    }

    /// Source georeferenced coordinates to source pixel/line.
    fn geo_to_src(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        match &self.src_gcp {
            Some(Georef::Geolocation(g)) => g.to_pixel(x, y),
            Some(Georef::Gcp(g)) => Some(g.to_pixel(x, y)),
            Some(Georef::Rpc(t)) => t.to_pixel(x, y, 0.0),
            None => return Some(apply_gt(&self.src_inv, x, y)),
        }
        .map(|(c, r)| (c / self.src_scale.0, r / self.src_scale.1))
    }

    fn set_dst_gt(&mut self, gt: [f64; 6]) {
        self.dst_gt = gt;
        self.dst_inv = invert_gt(&gt).unwrap_or(IDENTITY);
    }

    fn check_policy(&self) -> Result<()> {
        if let Some((fwd, inv)) = &self.ct {
            fwd.check_policy()?;
            inv.check_policy()?;
        }
        Ok(())
    }

    fn reproject(&self, dst_to_src: bool, x: f64, y: f64) -> Option<(f64, f64)> {
        let r = match &self.ct {
            None => Some((x, y)),
            Some((fwd, inv)) => (if dst_to_src { inv } else { fwd }).transform(x, y).ok(),
        };
        r.filter(|(a, b)| a.is_finite() && b.is_finite())
    }

    fn point(&self, dst_to_src: bool, x: f64, y: f64) -> Option<(f64, f64)> {
        if dst_to_src {
            let (gx, gy) = apply_gt(&self.dst_gt, x, y);
            let (sx, sy) = self.reproject(true, gx, gy)?;
            self.geo_to_src(sx, sy)
        } else {
            let (gx, gy) = self.src_to_geo(x, y)?;
            let (dx, dy) = self.reproject(false, gx, gy)?;
            Some(apply_gt(&self.dst_inv, dx, dy))
        }
    }

    fn transform(&self, dst_to_src: bool, x: &mut [f64], y: &mut [f64], ok: &mut [bool]) -> bool {
        if dst_to_src && let Some(Georef::Rpc(rpc)) = &self.src_gcp {
            // All the points to longitude and latitude, then through the RPCs at once, as GDAL
            // transforms them (a row of points at one latitude samples one row of a DEM).
            for i in 0..x.len() {
                let (gx, gy) = apply_gt(&self.dst_gt, x[i], y[i]);
                match self.reproject(true, gx, gy) {
                    Some((a, b)) => {
                        x[i] = a;
                        y[i] = b;
                        ok[i] = true;
                    }
                    None => {
                        x[i] = f64::INFINITY;
                        y[i] = f64::INFINITY;
                        ok[i] = false;
                    }
                }
            }
            rpc.to_pixel_many(x, y, None, ok);
            for i in 0..x.len() {
                x[i] /= self.src_scale.0;
                y[i] /= self.src_scale.1;
            }
            return true;
        }
        for i in 0..x.len() {
            match self.point(dst_to_src, x[i], y[i]) {
                Some((a, b)) => {
                    x[i] = a;
                    y[i] = b;
                    ok[i] = true;
                }
                None => ok[i] = false,
            }
        }
        true
    }
}

fn same_crs(a: &SpatialRef, b: &SpatialRef) -> bool {
    a.is_unknown() || b.is_unknown() || a.same_as(b)
}

/// GDAL's approximate transformer: along a row of points, transforms the ends and the middle
/// exactly and interpolates linearly when the middle's error is below the threshold, else splits
/// the row in two and recurses.
struct Approx<'a> {
    base: &'a GenImgProj,
    max_error: f64,
}

impl Approx<'_> {
    fn transform(&self, inv: bool, x: &mut [f64], y: &mut [f64], ok: &mut [bool]) -> bool {
        let n = x.len();
        if n == 0 {
            return true;
        }
        let mid = (n - 1) / 2;
        if n <= 5 || y[0] != y[n - 1] || y[0] != y[mid] || x[0] == x[n - 1] || x[0] == x[mid] || self.max_error == 0.0 {
            return self.base.transform(inv, x, y, ok);
        }
        let mut xs = [x[0], x[mid], x[n - 1]];
        let mut ys = [y[0], y[mid], y[n - 1]];
        let mut oks = [false; 3];
        if !self.base.transform(inv, &mut xs, &mut ys, &mut oks) || !oks.iter().all(|b| *b) {
            return self.base.transform(inv, x, y, ok);
        }
        self.internal(inv, x, y, ok, xs, ys)
    }

    fn internal(&self, inv: bool, x: &mut [f64], y: &mut [f64], ok: &mut [bool], xs: [f64; 3], ys: [f64; 3]) -> bool {
        let n = x.len();
        let mid = (n - 1) / 2;
        let dx = (xs[2] - xs[0]) / (x[n - 1] - x[0]);
        let dy = (ys[2] - ys[0]) / (x[n - 1] - x[0]);
        let error = ((xs[0] + dx * (x[mid] - x[0])) - xs[1]).abs() + ((ys[0] + dy * (x[mid] - x[0])) - ys[1]).abs();
        if error > self.max_error {
            let (a, b, c) = ((mid - 1) / 2, mid - 1, mid + (n - mid - 1) / 2);
            let mut xm = [x[a], x[b], x[c]];
            let mut ym = [y[a], y[b], y[c]];
            let mut om = [false; 3];
            let base1 = mid <= 5 || y[0] != y[b] || y[0] != y[a] || x[0] == x[b] || x[0] == x[a];
            let base2 = n - mid <= 5 || y[mid] != y[n - 1] || y[mid] != y[c] || x[mid] == x[n - 1] || x[mid] == x[c];
            let success = if !base1 && !base2 {
                self.base.transform(inv, &mut xm, &mut ym, &mut om)
            } else if !base1 {
                om[2] = true;
                self.base.transform(inv, &mut xm[..2], &mut ym[..2], &mut om[..2])
            } else if !base2 {
                om[0] = true;
                om[1] = true;
                self.base.transform(inv, &mut xm[2..], &mut ym[2..], &mut om[2..])
            } else {
                false
            };
            if !success || !om.iter().all(|b| *b) {
                let s1 = self.base.transform(inv, &mut x[1..mid], &mut y[1..mid], &mut ok[1..mid]);
                let s2 = self.base.transform(inv, &mut x[mid + 1..n - 1], &mut y[mid + 1..n - 1], &mut ok[mid + 1..n - 1]);
                for (i, k) in [(0, 0), (mid, 1), (n - 1, 2)] {
                    x[i] = xs[k];
                    y[i] = ys[k];
                    ok[i] = true;
                }
                return s1 && s2;
            }
            if !base1 {
                self.internal(inv, &mut x[..mid], &mut y[..mid], &mut ok[..mid], [xs[0], xm[0], xm[1]], [ys[0], ym[0], ym[1]]);
            } else {
                self.base.transform(inv, &mut x[1..mid], &mut y[1..mid], &mut ok[1..mid]);
                x[0] = xs[0];
                y[0] = ys[0];
                ok[0] = true;
            }
            if !base2 {
                self.internal(inv, &mut x[mid..], &mut y[mid..], &mut ok[mid..], [xs[1], xm[2], xs[2]], [ys[1], ym[2], ys[2]]);
            } else {
                self.base.transform(inv, &mut x[mid + 1..n - 1], &mut y[mid + 1..n - 1], &mut ok[mid + 1..n - 1]);
                for (i, k) in [(mid, 1), (n - 1, 2)] {
                    x[i] = xs[k];
                    y[i] = ys[k];
                    ok[i] = true;
                }
            }
            return true;
        }
        for i in (0..n).rev() {
            let d = x[i] - x[0];
            x[i] = xs[0] + dx * d;
            y[i] = ys[0] + dy * d;
            ok[i] = true;
        }
        true
    }
}

// ─── Output grid ────────────────────────────────────────────────────────────────────────────────

/// The output grid GDAL suggests for a raster reprojected to `dst`: the geotransform, the size
/// and the extent `[min_x, min_y, max_x, max_y]` (for drivers that warp, such as MBTiles).
pub(crate) fn suggested_output(ds: &RasterDataset, dst: &SpatialRef) -> Result<([f64; 6], usize, usize, [f64; 4])> {
    let src = ds.crs();
    let t = GenImgProj::new(ds.geo_transform(), &src, dst)?;
    suggested_warp_output(&t, ds.raster.cols, ds.raster.rows, &src, dst)
}

/// The output grid GDAL suggests (`GDALSuggestedWarpOutput2()`) for a raster of `width` x
/// `height` pixels with the geotransform `gt` in the CRS `src`, reprojected to `dst`: the
/// geotransform, the width and the height.
///
/// ```no_run
/// use terrio::crs::SpatialRef;
///
/// let (src, dst) = (SpatialRef::parse("EPSG:32611")?, SpatialRef::parse("EPSG:4326")?);
/// let gt = [699960.0, 30.0, 0.0, 4100040.0, 0.0, -30.0];
/// let (gt, w, h) = terrio::ops::warp::suggested_grid(3660, 3660, gt, &src, &dst)?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn suggested_grid(width: usize, height: usize, gt: [f64; 6], src: &SpatialRef, dst: &SpatialRef) -> Result<([f64; 6], usize, usize)> {
    if width == 0 || height == 0 {
        return Err(Error::InvalidArgument("the source raster is empty".into()));
    }
    let t = GenImgProj::new(gt, src, dst)?;
    let (gt, w, h, _) = suggested_warp_output(&t, width, height, src, dst)?;
    Ok((gt, w, h))
}

/// The output grid GDAL suggests for a raster of `width` x `height` pixels georeferenced by GCPs
/// (in `src`) reprojected to `dst`, with a GCP method and polynomial `order` (0 for GDAL's
/// default).
pub fn suggested_grid_gcps(
    width: usize,
    height: usize,
    gcps: &[crate::Gcp],
    method: super::gcp::GcpMethod,
    order: u32,
    src: &SpatialRef,
    dst: &SpatialRef,
) -> Result<([f64; 6], usize, usize)> {
    if width == 0 || height == 0 {
        return Err(Error::InvalidArgument("the source raster is empty".into()));
    }
    let g = super::gcp::GcpTransformer::new(gcps, method, order)?;
    let t = GenImgProj::with_gcps(src, dst, g)?;
    let (gt, w, h, _) = suggested_warp_output(&t, width, height, src, dst)?;
    Ok((gt, w, h))
}

/// The output grid GDAL suggests for a raster of `width` x `height` pixels georeferenced by RPCs
/// reprojected to `dst` (its source CRS is WGS 84), with the RPC transformer's options: what
/// `rasterio.warp.calculate_default_transform(..., rpcs=...)` computes.
///
/// # Errors
/// When the RPC transformer cannot be created or too many points fail to transform.
pub fn suggested_grid_rpc(
    width: usize,
    height: usize,
    rpc: &super::rpc::Rpc,
    options: &super::rpc::RpcOptions,
    dst: &SpatialRef,
) -> Result<([f64; 6], usize, usize)> {
    if width == 0 || height == 0 {
        return Err(Error::InvalidArgument("the source raster is empty".into()));
    }
    let r = super::rpc::RpcTransformer::new(rpc, options)?;
    let src = SpatialRef::from_epsg(4326)?;
    let t = GenImgProj::with_georef(&src, dst, Georef::Rpc(std::sync::Arc::new(r)))?;
    let (gt, w, h, _) = suggested_warp_output(&t, width, height, &src, dst)?;
    Ok((gt, w, h))
}

/// GDAL's `GDALSuggestedWarpOutput2()`: the extent of the transformed source and a square pixel
/// size that keeps the number of pixels along the diagonal. Returns the geotransform, the size and
/// the extent `[min_x, min_y, max_x, max_y]`.
fn suggested_warp_output(
    t: &GenImgProj,
    w: usize,
    h: usize,
    src_srs: &SpatialRef,
    dst_srs: &SpatialRef,
) -> Result<([f64; 6], usize, usize, [f64; 4])> {
    let (wf, hf) = (w as f64, h as f64);
    if t.ct.is_none() && t.src_gt[2] == 0.0 && t.src_gt[4] == 0.0 && t.src_gcp.is_none() && t.dst_gt == IDENTITY {
        let mut gt = t.src_gt;
        let mut ext = [gt[0], gt[3] + hf * gt[5], gt[0] + wf * gt[1], gt[3]];
        // South-up: the output is north-up, over the same extent.
        if gt[5] >= 0.0 {
            gt[3] += hf * gt[5];
            gt[5] = -gt[5];
            ext.swap(1, 3);
        }
        return Ok((gt, w, h, ext));
    }
    let steps = ((w.min(h) as f64 / 50.0 + 0.5) as usize).clamp(20, 100);
    let step = 1.0 / steps as f64;
    let ratio = |i: usize| if i == steps { 1.0 } else { i as f64 * step };
    let n1 = steps + 1;
    let mut xs = Vec::with_capacity(4 * n1);
    let mut ys = Vec::with_capacity(4 * n1);
    for edge in 0..4 {
        for i in 0..=steps {
            let r = ratio(i);
            let (x, y) = match edge {
                0 => (r * wf, 0.0),
                1 => (r * wf, hf),
                2 => (0.0, r * hf),
                _ => (wf, r * hf),
            };
            xs.push(x);
            ys.push(y);
        }
    }
    let (in_x, in_y) = (xs.clone(), ys.clone());
    let mut ok = vec![false; xs.len()];
    t.transform(false, &mut xs, &mut ys, &mut ok);
    let mut failed = ok.iter().filter(|b| !**b).count();
    if failed == 0 {
        let (mut rx, mut ry, mut rok) = (xs.clone(), ys.clone(), vec![false; xs.len()]);
        t.transform(true, &mut rx, &mut ry, &mut rok);
        for i in 0..rx.len() {
            if !rok[i] || (rx[i] - in_x[i]).abs() > wf / steps as f64 || (ry[i] - in_y[i]).abs() > hf / steps as f64 {
                failed += 1;
                break;
            }
        }
    }
    let grid = failed > 0;
    if grid {
        xs.clear();
        ys.clear();
        for j in 0..=steps {
            for i in 0..=steps {
                xs.push(ratio(i) * wf);
                ys.push(ratio(j) * hf);
            }
        }
        ok = vec![false; xs.len()];
        t.transform(false, &mut xs, &mut ys, &mut ok);
    }
    let n = xs.len();
    let (mut minx, mut miny, mut maxx, mut maxy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut got = false;
    let mut add = |x: f64, y: f64, got: &mut bool| {
        if *got {
            minx = minx.min(x);
            miny = miny.min(y);
            maxx = maxx.max(x);
            maxy = maxy.max(y);
        } else {
            *got = true;
            (minx, maxx, miny, maxy) = (x, x, y, y);
        }
    };
    failed = 0;
    for i in 0..n {
        let (xi, yi) = if grid {
            (i % n1, i / n1)
        } else if i < 2 * n1 {
            (i % n1, if i < n1 { 0 } else { steps })
        } else {
            (0, 0)
        };
        if xi > 0 && (ok[i - 1] || ok[i]) {
            let (mut xob, mut xoa) = (xs[i - 1], xs[i]);
            let mut xib = (xi - 1) as f64 * wf / steps as f64;
            let mut xia = xi as f64 * wf / steps as f64;
            let (mut inv_b, mut inv_a) = (!ok[i - 1], !ok[i]);
            let mut iter = 0;
            while (inv_b || inv_a || xob * xoa < 0.0) && iter < 16 {
                let xm = (xib + xia) / 2.0;
                let ym = yi as f64 * hf / steps as f64;
                if let Some((x, y)) = t.point(false, xm, ym) {
                    add(x, y, &mut got);
                    if inv_b || xob * x < 0.0 {
                        inv_a = false;
                        xia = xm;
                        xoa = x;
                    } else {
                        inv_b = false;
                        xob = x;
                        xib = xm;
                    }
                } else if inv_b {
                    xib = xm;
                } else if inv_a {
                    xia = xm;
                } else {
                    break;
                }
                iter += 1;
            }
        }
        if !ok[i] {
            failed += 1;
            continue;
        }
        add(xs[i], ys[i], &mut got);
    }
    if !got || failed + 10 > n {
        return Err(Error::Crs(format!("too many points ({failed} out of {n}) failed to transform")));
    }

    // Geographic output: the poles, and GDAL's TransformBounds() of the source extent.
    let dst_geographic = dst_srs.is_geographic() && t.dst_gt == IDENTITY;
    let mut geographic_deg = false;
    if let Some((fwd, inv)) = &t.ct {
        if dst_geographic && !src_srs.is_geographic() {
            geographic_deg = true;
            for sign in [-1.0f64, 1.0] {
                let lat = 90.0 * sign;
                if let Ok((px, py)) = inv.transform(0.0, lat)
                    && px.is_finite()
                    && py.is_finite()
                {
                    let (c, r) = t.geo_to_src(px, py).unwrap_or((f64::NAN, f64::NAN));
                    if (-1e-5..=wf + 1e-5).contains(&c)
                        && (-1e-5..=hf + 1e-5).contains(&r)
                        && let Ok((_, y2)) = fwd.transform(px, py)
                        && (y2 - lat).abs() <= 1e-6
                    {
                        minx = -180.0;
                        maxx = 180.0;
                        if sign < 0.0 {
                            miny = lat;
                        } else {
                            maxy = lat;
                        }
                    }
                }
            }
        }
        let gt = &t.src_gt;
        if gt[2] == 0.0 && gt[4] == 0.0 && t.src_gcp.is_none() {
            let (x1, y1) = (gt[0] + gt[1] * wf, gt[3] + gt[5] * hf);
            if let Ok(b) = super::transform_bounds(fwd, (gt[0].min(x1), gt[3].min(y1), gt[0].max(x1), gt[3].max(y1)), 2) {
                minx = minx.min(b.0);
                miny = miny.min(b.1);
                maxx = maxx.max(b.2);
                maxy = maxy.max(b.3);
            }
        }
    }

    let (mut ddx, mut ddy) = (0.0, 0.0);
    if ok[0] && ok[n - 1] {
        ddx = xs[n - 1] - xs[0];
        ddy = ys[n - 1] - ys[0];
    }
    if ddx == 0.0 || ddy == 0.0 {
        ddx = maxx - minx;
        ddy = maxy - miny;
    }
    let diagonal = (ddx * ddx + ddy * ddy).sqrt();
    let pixel = diagonal / (wf * wf + hf * hf).sqrt();
    let pixels = ((maxx - minx) / pixel + 0.5) as usize;
    let lines = ((maxy - miny) / pixel + 0.5) as usize;
    if pixels == 0 || lines == 0 {
        return Err(Error::InvalidData("the computed output size is empty".into()));
    }
    let (mut psx, mut psy) = (pixel, pixel);
    let ratios = [0.0, 0.001, 0.01, 0.1, 1.0];
    let border_is_out = |ext: [f64; 4], right: bool, sx: f64, sy: f64| {
        let mut bad = 0;
        for i in 0..21 {
            let r = if i == 20 { 1.0 } else { i as f64 * 0.05 };
            let (ex, ey) = if right { (ext[2], ext[3] - sy * r * lines as f64) } else { (ext[0] + sx * r * pixels as f64, ext[1]) };
            let back = t.point(true, ex, ey).and_then(|(c, r)| t.point(false, c, r));
            match back {
                Some((x, y)) if (x - ex).abs() <= sx && (y - ey).abs() <= sy => {}
                _ => bad += 1,
            }
        }
        bad == 21
    };
    for r in ratios {
        let try_x = psx - psx * r / pixels as f64;
        let ext = [minx, maxy - lines as f64 * psy, minx + pixels as f64 * try_x, maxy];
        if !border_is_out(ext, true, try_x, psy) {
            psx = try_x;
            break;
        }
    }
    for r in ratios {
        let try_y = psy - psy * r / lines as f64;
        let ext = [minx, maxy - lines as f64 * try_y, minx + pixels as f64 * psx, maxy];
        if !border_is_out(ext, false, psx, try_y) {
            psy = try_y;
            break;
        }
    }
    let new_maxx = minx + pixels as f64 * psx;
    if geographic_deg && ((maxx <= 180.0 && new_maxx > 180.0) || maxx == 180.0) {
        maxx = 180.0;
        psx = (maxx - minx) / pixels as f64;
    } else {
        maxx = new_maxx;
    }
    let new_miny = maxy - lines as f64 * psy;
    if geographic_deg && miny >= -90.0 && new_miny < -90.0 {
        miny = -90.0;
        psy = (maxy - miny) / lines as f64;
    } else {
        miny = new_miny;
    }
    Ok(([minx, psx, 0.0, maxy, 0.0, -psy], pixels, lines, [minx, miny, maxx, maxy]))
}

/// A source prepared for warping.
struct Source<'a> {
    transform_options: crate::crs::TransformOptions,
    ds: &'a RasterDataset,
    grid: Grid,
    srs: SpatialRef,
    /// GCPs or RPCs georeferencing the source in place of its geotransform.
    gcp: Option<Georef>,
    pixel_scale: (f64, f64),
}

impl Source<'_> {
    /// The transformer from this source's pixels to the output CRS.
    fn transformer(&self, dst_srs: &SpatialRef) -> Result<GenImgProj> {
        let mut t = GenImgProj::with_options(
            if self.gcp.is_some() { IDENTITY } else { self.grid.geo_transform },
            &self.srs,
            dst_srs,
            &self.transform_options,
        )?;
        t.src_gcp = self.gcp.clone();
        t.src_scale = self.pixel_scale;
        Ok(t)
    }
}

/// The output grid `gdalwarp` would create (`GDALWarpCreateOutput()`).
fn output_grid(sources: &[Source], dst_srs: &SpatialRef, o: &WarpOptions, te: Option<(f64, f64, f64, f64)>) -> Result<Grid> {
    let (mut min_x, mut min_y, mut max_x, mut max_y) = te.unwrap_or((0.0, 0.0, 0.0, 0.0));
    let mut have_te = te.is_some();
    let (mut xres, mut yres) = o.resolution.unwrap_or((0.0, 0.0));
    let (force_w, force_h) = o.size.unwrap_or((0, 0));

    // Geographic to Mercator: clamp the latitudes to ±85.0511°.
    if sources.len() == 1 && !have_te && is_mercator(dst_srs) && sources[0].srs.is_geographic() && sources[0].gcp.is_none() {
        const MAX_LAT: f64 = 85.0511287798066;
        let (x0, y0, x1, y1) = sources[0].grid.bounds();
        if x0 >= -180.0 - 1e-3 && x1 <= 180.0 + 1e-3 && ((y1 > MAX_LAT && y0 < MAX_LAT) || (y1 > -MAX_LAT && y0 < -MAX_LAT)) {
            let t = Transformer::new(&sources[0].srs, dst_srs)?;
            if let (Ok(ll), Ok(ur)) = (t.transform(x0.max(-180.0), y0.max(-MAX_LAT)), t.transform(x1.min(180.0), y1.min(MAX_LAT))) {
                (min_x, min_y, max_x, max_y) = (ll.0, ll.1, ur.0, ur.1);
                have_te = true;
            }
        }
    }

    let needs_suggested = !(((force_w != 0 && force_h != 0) || (xres != 0.0 && yres != 0.0)) && have_te);
    let te_without_res = have_te && force_w == 0 && force_h == 0 && xres == 0.0 && yres == 0.0;

    let (mut wmin_x, mut wmax_x, mut wmin_y, mut wmax_y, mut wres_x, mut wres_y) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
    let mut res_from_te = f64::INFINITY;
    let mut unique: Option<GenImgProj> = None;
    for s in sources {
        let t = s.transformer(dst_srs)?;
        if te_without_res {
            res_from_te = res_from_te.min(resolution_from_target_extent(&t, &s.grid, (min_x, min_y, max_x, max_y)));
        }
        let (mut gt, mut ext) = ([0.0; 6], [0.0; 4]);
        if needs_suggested {
            let (g, _, _, e) = suggested_warp_output(&t, s.grid.width, s.grid.height, &s.srs, dst_srs)?;
            (gt, ext) = (g, e);
        }
        let sgt = s.grid.geo_transform;
        if needs_suggested
            && s.gcp.is_none()
            && xres == 0.0
            && yres == 0.0
            && force_w == 0
            && force_h == 0
            && sgt[2] == 0.0
            && sgt[4] == 0.0
            && sgt[5] < 0.0
            && same_crs(&s.srs, dst_srs)
        {
            gt = sgt;
            ext = [sgt[0], sgt[3] + s.grid.height as f64 * sgt[5], sgt[0] + s.grid.width as f64 * sgt[1], sgt[3]];
            res_from_te = f64::INFINITY;
        }
        if needs_suggested {
            if wmax_x == 0.0 && wmin_x == 0.0 {
                (wmin_x, wmax_x, wmax_y, wmin_y, wres_x, wres_y) = (ext[0], ext[2], ext[3], ext[1], gt[1], gt[5].abs());
            } else {
                wmin_x = wmin_x.min(ext[0]);
                wmax_x = wmax_x.max(ext[2]);
                wmax_y = wmax_y.max(ext[3]);
                wmin_y = wmin_y.min(ext[1]);
                wres_x = wres_x.min(gt[1]);
                wres_y = wres_y.min(gt[5].abs());
            }
        }
        if sources.len() == 1 {
            unique = Some(t);
        }
    }
    if res_from_te.is_finite() {
        wres_x = res_from_te;
        wres_y = res_from_te;
    }

    let mut gt = [0.0; 6];
    let (mut pixels, mut lines) = (0usize, 0usize);
    if needs_suggested {
        gt = [wmin_x, wres_x, 0.0, wmax_y, 0.0, -wres_y];
        pixels = ((wmax_x - wmin_x) / wres_x).round() as usize;
        lines = ((wmax_y - wmin_y) / wres_y).round() as usize;
    }
    let px_from = |min_x: f64, max_x: f64, xres: f64| ((max_x - min_x) / xres).round().max(1.0);
    let consistent = force_w != 0
        && force_h != 0
        && xres != 0.0
        && yres != 0.0
        && have_te
        && ((max_x - min_x) / xres - force_w as f64).abs() <= 1e-8
        && ((max_y - min_y) / yres - force_h as f64).abs() <= 1e-8;
    if consistent {
        gt = [min_x, xres, 0.0, max_y, 0.0, -yres];
        pixels = force_w;
        lines = force_h;
    } else if xres != 0.0 && yres != 0.0 {
        let mut detect_blank = false;
        if !have_te {
            detect_blank = needs_suggested;
            min_x = gt[0];
            max_x = gt[0] + gt[1] * pixels as f64;
            max_y = gt[3];
            min_y = gt[3] + gt[5] * lines as f64;
        }
        if o.target_aligned_pixels {
            if needs_suggested {
                detect_blank = true;
            }
            const EPS: f64 = 1e-8;
            min_x = (min_x / xres + EPS).floor() * xres;
            max_x = (max_x / xres - EPS).ceil() * xres;
            min_y = (min_y / yres + EPS).floor() * yres;
            max_y = (max_y / yres - EPS).ceil() * yres;
        }
        let update = |min_x: f64, min_y: f64, max_x: f64, max_y: f64| -> ([f64; 6], usize, usize) {
            let p = px_from(min_x, max_x, xres) as usize;
            let l = ((max_y - min_y).abs() / yres).round().max(1.0) as usize;
            ([min_x, xres, 0.0, max_y, 0.0, if max_y > min_y { -yres } else { yres }], p, l)
        };
        if detect_blank && let Some(t) = unique.as_mut() {
            let (sw, sh) = (sources[0].grid.width as f64, sources[0].grid.height as f64);
            loop {
                let (g, p, l) = update(min_x, min_y, max_x, max_y);
                if p <= 1 && l <= 1 {
                    break;
                }
                t.set_dst_gt(g);
                let blank = |pts: Vec<(f64, f64)>, is_ok: &dyn Fn(f64, f64) -> bool| {
                    !pts.into_iter().any(|(x, y)| t.point(true, x, y).is_some_and(|(a, b)| is_ok(a, b)))
                };
                let top = blank((0..p).map(|i| (i as f64 + 0.5, 0.5)).collect(), &|_, y| y >= 0.0);
                let bottom = blank((0..p).map(|i| (i as f64 + 0.5, l as f64 - 0.5)).collect(), &|_, y| y <= sh);
                let left = blank((0..l).map(|i| (0.5, i as f64 + 0.5)).collect(), &|x, _| x >= 0.0);
                let right = blank((0..l).map(|i| (p as f64 - 0.5, i as f64 + 0.5)).collect(), &|x, _| x <= sw);
                if !(top || bottom || left || right) {
                    break;
                }
                if top {
                    if max_y - min_y <= 2.0 * yres {
                        break;
                    }
                    max_y -= yres;
                }
                if bottom {
                    if max_y - min_y <= 2.0 * yres {
                        break;
                    }
                    min_y += yres;
                }
                if left {
                    if max_x - min_x <= 2.0 * xres {
                        break;
                    }
                    min_x += xres;
                }
                if right {
                    if max_x - min_x <= 2.0 * xres {
                        break;
                    }
                    max_x -= xres;
                }
            }
        }
        (gt, pixels, lines) = update(min_x, min_y, max_x, max_y);
    } else if force_w != 0 && force_h != 0 {
        if !have_te {
            (min_x, max_x, max_y, min_y) = (wmin_x, wmax_x, wmax_y, wmin_y);
        }
        xres = (max_x - min_x) / force_w as f64;
        yres = (max_y - min_y) / force_h as f64;
        gt = [min_x, xres, 0.0, max_y, 0.0, -yres];
        pixels = force_w;
        lines = force_h;
    } else if force_w != 0 {
        if !have_te {
            (min_x, max_x, max_y, min_y) = (wmin_x, wmax_x, wmax_y, wmin_y);
        }
        xres = (max_x - min_x) / force_w as f64;
        yres = xres;
        gt = [min_x, xres, 0.0, max_y, 0.0, if max_y > min_y { -yres } else { yres }];
        pixels = force_w;
        lines = ((max_y - min_y).abs() / yres).round().max(1.0) as usize;
    } else if force_h != 0 {
        if !have_te {
            (min_x, max_x, max_y, min_y) = (wmin_x, wmax_x, wmax_y, wmin_y);
        }
        yres = (max_y - min_y) / force_h as f64;
        xres = yres.abs();
        gt = [min_x, xres, 0.0, max_y, 0.0, -yres];
        pixels = px_from(min_x, max_x, xres) as usize;
        lines = force_h;
    } else if have_te {
        let (rx, ry) = (gt[1], gt[5].abs());
        pixels = px_from(min_x, max_x, rx) as usize;
        lines = ((max_y - min_y).abs() / ry).round().max(1.0) as usize;
        xres = (max_x - min_x) / pixels as f64;
        yres = (max_y - min_y) / lines as f64;
        gt = [min_x, xres, 0.0, max_y, 0.0, -yres];
    }
    if pixels == 0 || lines == 0 {
        return Err(Error::InvalidArgument("the output would be empty".into()));
    }
    Ok(Grid { width: pixels, height: lines, geo_transform: gt })
}

/// The output grid of [`WarpOptions::dst_transform`] and [`WarpOptions::size`].
fn given_grid(gt: [f64; 6], o: &WarpOptions) -> Result<Grid> {
    let Some((width, height)) = o.size.filter(|(w, h)| *w != 0 && *h != 0) else {
        return Err(Error::InvalidArgument("an output geotransform needs an output size, both non-zero".into()));
    };
    if o.extent.is_some() || o.resolution.is_some() || o.target_aligned_pixels || o.crop_to_cutline {
        return Err(Error::InvalidArgument(
            "an output geotransform excludes an extent, a resolution, -tap and cropping to the cutline".into(),
        ));
    }
    if !gt.iter().all(|v| v.is_finite()) || invert_gt(&gt).is_none() {
        return Err(Error::InvalidArgument("the output geotransform is not invertible".into()));
    }
    Ok(Grid { width, height, geo_transform: gt })
}

/// The resolution of the source, in output units, over the target extent (`gdalwarp` with `-te`
/// and no `-tr` or `-ts`).
fn resolution_from_target_extent(t: &GenImgProj, grid: &Grid, te: (f64, f64, f64, f64)) -> f64 {
    let (nx, ny) = (10, 10);
    let eps = (te.2 - te.0).min((te.3 - te.1).abs()) / 1000.0;
    let mut pts = Vec::with_capacity(3 * nx * ny);
    for iy in 0..ny {
        for ix in 0..nx {
            let x = te.0 + ix as f64 * (te.2 - te.0) / (nx - 1) as f64;
            let y = te.1 + iy as f64 * (te.3 - te.1) / (ny - 1) as f64;
            pts.push((x, y));
            pts.push((if ix == nx - 1 { x - eps } else { x + eps }, y));
            pts.push((x, if iy == ny - 1 { y - eps } else { y + eps }));
        }
    }
    let gt = grid.geo_transform;
    let to_src = |x: f64, y: f64| t.reproject(true, x, y);
    let Some((ox, oy)) = to_src(te.0, te.1) else { return f64::INFINITY };
    let pix: Vec<Option<(f64, f64)>> =
        pts.iter().map(|&(x, y)| to_src(x, y).map(|(a, b)| ((a - ox) / gt[1], ((b - oy) / gt[5]).abs()))).collect();
    let dist = |a: (f64, f64), b: (f64, f64)| ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
    let mut pairs = Vec::new();
    for i in (0..pix.len()).step_by(3) {
        if let (Some(p0), Some(p1), Some(p2)) = (pix[i], pix[i + 1], pix[i + 2])
            && p0.0 >= 0.0
            && p0.1 >= 0.0
        {
            let (r1, r2) = (eps.abs() / dist(p0, p1), eps.abs() / dist(p0, p2));
            if r1.is_finite() && r2.is_finite() {
                pairs.push((r1, r2));
            }
        }
    }
    if pairs.is_empty() {
        return f64::INFINITY;
    }
    pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
    let m1 = pairs[pairs.len() / 2].0;
    pairs.retain(|p| p.0 > m1 / 10.0);
    if pairs.is_empty() {
        return f64::INFINITY;
    }
    pairs.sort_by(|a, b| a.1.total_cmp(&b.1));
    let m2 = pairs[pairs.len() / 2].1;
    let mut res: Vec<f64> = pairs.iter().filter(|p| p.1 > m2 / 10.0).map(|p| (p.0 + p.1) / 2.0).collect();
    res.sort_by(f64::total_cmp);
    if res.is_empty() {
        return f64::INFINITY;
    }
    let m = res[res.len() / 2];
    res.into_iter().find(|r| *r > m / 10.0).unwrap_or(f64::INFINITY)
}

fn is_mercator(srs: &SpatialRef) -> bool {
    matches!(srs.epsg, Some(3857 | 900913 | 3395 | 3785))
        || srs.to_wkt().is_some_and(|w| {
            let u = w.to_ascii_uppercase();
            u.contains("\"MERCATOR_1SP\"") || u.contains("POPULAR VISUALISATION PSEUDO MERCATOR")
        })
}

// ─── The warp itself ────────────────────────────────────────────────────────────────────────────

/// A warp set up from the sources' metadata: the output grid, type and nodata.
struct Plan<'a> {
    srcs: Vec<Source<'a>>,
    dst_srs: SpatialRef,
    grid: Grid,
    bands: usize,
    out_type: DataType,
    dst_nodata: Option<f64>,
    dst_nodata_all: Vec<Option<f64>>,
}

impl<'a> Plan<'a> {
    /// Reads only the sources' metadata.
    fn new(sources: &[&'a RasterDataset], o: &WarpOptions) -> Result<Self> {
        let first = sources.first().ok_or_else(|| Error::InvalidArgument("no source rasters".into()))?;
        let mut srcs: Vec<Source> = sources
            .iter()
            .map(|ds| Source {
                ds,
                transform_options: o.transform_options.clone(),
                grid: Grid {
                    width: ds.raster.cols,
                    height: ds.raster.rows,
                    geo_transform: o.src_transform.unwrap_or_else(|| ds.geo_transform()),
                },
                srs: o.src_srs.clone().unwrap_or_else(|| ds.crs()),
                gcp: None,
                pixel_scale: (1.0, 1.0),
            })
            .collect();
        if (o.geoloc || o.geolocation.is_some())
            && (!o.gcps.is_empty() || o.rpc.is_some() || o.homography.is_some() || o.src_transform.is_some())
        {
            return Err(Error::InvalidArgument("geolocation arrays exclude GCP and RPC georeferencing".into()));
        }
        if o.gcps.is_empty() && o.rpc.is_none() && o.homography.is_none() && o.src_transform.is_none() {
            for (index, source) in srcs.iter_mut().enumerate() {
                if index == 0
                    && let Some(g) = &o.geolocation
                {
                    if o.cutline.is_some() {
                        return Err(Error::Unsupported("cutlines are not supported with geolocation arrays".into()));
                    }
                    source.gcp = Some(Georef::Geolocation(std::sync::Arc::new(g.clone())));
                    if o.src_srs.is_none() {
                        source.srs = SpatialRef::parse(if g.srs().is_empty() { "EPSG:4326" } else { g.srs() })?;
                    }
                    continue;
                }
                if let Some(g) = &source.ds.geolocation {
                    // Preserve affine cutline workflows on datasets that also happen to
                    // carry array metadata. A plain swath has no usable affine fallback.
                    let default_gt = [0.0, 1.0, 0.0, source.grid.height as f64, 0.0, -1.0];
                    if !o.geoloc && o.cutline.is_some() && (source.ds.affine.is_some() || source.ds.geo_transform() != default_gt) {
                        continue;
                    }
                    if o.cutline.is_some() {
                        return Err(Error::Unsupported("cutlines are not supported with geolocation arrays".into()));
                    }
                    source.gcp = Some(Georef::Geolocation(std::sync::Arc::new(super::geolocation::GeolocationTransformer::open(
                        g,
                        source.grid.width,
                        source.grid.height,
                    )?)));
                    if o.src_srs.is_none() {
                        source.srs = SpatialRef::parse(if g.srs.is_empty() { "EPSG:4326" } else { &g.srs })?;
                    }
                } else if o.geoloc {
                    return Err(Error::InvalidArgument("source has no geolocation arrays".into()));
                }
            }
        }
        if !o.gcps.is_empty() {
            if o.cutline.is_some() {
                return Err(Error::Unsupported("cutlines are not supported with GCPs".into()));
            }
            srcs[0].gcp = Some(Georef::Gcp(super::gcp::GcpTransformer::new(&o.gcps, o.gcp_method, o.order)?));
        }
        if let Some(homography) = o.homography {
            if o.cutline.is_some() {
                return Err(Error::Unsupported("cutlines are not supported with homographies".into()));
            }
            srcs[0].gcp = Some(Georef::Gcp(super::gcp::GcpTransformer::from_homography(homography)?));
        }
        if let Some(rpc) = &o.rpc {
            if o.cutline.is_some() {
                return Err(Error::Unsupported("cutlines are not supported with RPCs".into()));
            }
            srcs[0].gcp = Some(Georef::Rpc(std::sync::Arc::new(super::rpc::RpcTransformer::new(rpc, &o.rpc_options)?)));
            // RPCs give WGS 84 longitudes and latitudes.
            if o.src_srs.is_none() {
                srcs[0].srs = SpatialRef::from_epsg(4326)?;
            }
        }
        let dst_srs = o.dst_srs.clone().unwrap_or_else(|| srcs[0].srs.clone());
        let bands = first.raster.bands;
        if srcs.iter().any(|s| s.ds.raster.bands != bands) {
            return Err(Error::InvalidArgument("all sources must have the same number of bands".into()));
        }

        // The target extent, in the output CRS.
        let mut te = o.extent;
        if let (Some(b), Some(srs)) = (te, &o.extent_srs)
            && !same_crs(srs, &dst_srs)
        {
            let t = Transformer::with_options(srs, &dst_srs, &o.transform_options)?;
            let bounds = super::transform_bounds(&t, b, 21);
            t.check_policy()?;
            te = Some(bounds?);
        }
        // The cutline, in each source's pixel space, and its extent.
        if o.crop_to_cutline {
            let c = o.cutline.as_ref().ok_or_else(|| Error::InvalidArgument("cropping to the cutline needs a cutline".into()))?;
            te = Some(cutline_extent(c, &srcs[0], &dst_srs, o.resolution.is_some())?);
        }

        let grid = match o.dst_transform {
            Some(gt) => given_grid(gt, o)?,
            None => output_grid(&srcs, &dst_srs, o, te)?,
        };
        let out_type = o.output_type.unwrap_or(first.raster.data_type);
        // The output's nodata defaults to the source's.
        let dst_nodata_all: Vec<_> = (0..bands)
            .map(|band| match &o.dst_nodata {
                Nodata::Auto => src_nodata(&srcs[0], o, band),
                Nodata::None => None,
                v => v.value(band),
            })
            .collect();
        let dst_nodata = dst_nodata_all[0];
        Ok(Self { srcs, dst_srs, grid, bands, out_type, dst_nodata, dst_nodata_all })
    }

    /// Select the input resolution after computing the target grid at full resolution.
    fn select_overview(&mut self, k: usize, levels: &[(usize, usize)], o: &WarpOptions) -> Result<Option<usize>> {
        let full = self.srcs[k].grid;
        if levels.is_empty() && !matches!(o.overview, OverviewSelection::Level(_)) {
            return Ok(None);
        }
        let ratio = if matches!(o.overview, OverviewSelection::Auto | OverviewSelection::AutoOffset(_)) {
            let t = self.transformer(k)?;
            let explicit_grid = o.dst_transform.is_some()
                || (o.extent.is_some() && (o.resolution.is_some() || o.size.is_some_and(|(w, h)| w != 0 && h != 0)));
            if explicit_grid {
                let (mut minx, mut miny, mut maxx, mut maxy) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
                for x in 0..10 {
                    for y in 0..10 {
                        if let Some((c, r)) =
                            t.point(true, self.grid.width as f64 * x as f64 / 9.0, self.grid.height as f64 * y as f64 / 9.0)
                        {
                            minx = minx.min(c);
                            maxx = maxx.max(c);
                            miny = miny.min(r);
                            maxy = maxy.max(r);
                        }
                    }
                }
                ((maxx - minx) / self.grid.width as f64).min((maxy - miny) / self.grid.height as f64)
            } else {
                let source = &self.srcs[k];
                let (natural, _, _, _) = suggested_warp_output(&t, source.grid.width, source.grid.height, &source.srs, &self.dst_srs)?;
                1.0 / natural[1].abs()
            }
        } else {
            1.0
        };
        let level = o.overview.select(full.width, levels, ratio);
        if let Some(index) = level {
            let (width, height) = levels[index];
            let source = &mut self.srcs[k];
            source.pixel_scale = (full.width as f64 / width as f64, full.height as f64 / height as f64);
            let (sx, sy) = source.pixel_scale;
            let gt = full.geo_transform;
            source.grid = Grid { width, height, geo_transform: [gt[0], gt[1] * sx, gt[2] * sy, gt[3], gt[4] * sx, gt[5] * sy] };
        }
        Ok(level)
    }

    /// The output's initial values, rounded to its type, one per band.
    fn fill(&self) -> Vec<f64> {
        self.dst_nodata_all.iter().map(|v| warp_round(v.unwrap_or(0.), self.out_type)).collect()
    }

    /// The transformer from source `k`'s pixels to the output's.
    fn transformer(&self, k: usize) -> Result<GenImgProj> {
        let mut t = self.srcs[k].transformer(&self.dst_srs)?;
        t.set_dst_gt(self.grid.geo_transform);
        Ok(t)
    }

    /// Source `k`'s nodata values, band by band.
    fn src_nodata(&self, k: usize, o: &WarpOptions) -> Vec<Option<f64>> {
        (0..self.bands).map(|b| src_nodata(&self.srcs[k], o, b)).collect()
    }

    /// The window of the output source `k` is warped into: the whole output, or with
    /// [`WarpOptions::rpc_output_window`], the window GDAL's suggested output for the first source
    /// covers and 5 pixels around it, as `gdalwarp` restricts it (`AdjustOutputExtentForRPC`);
    /// `None` when they don't intersect.
    fn rpc_window(&self, k: usize, t: &GenImgProj, o: &WarpOptions) -> Option<(usize, usize, usize, usize)> {
        let (w, h) = (self.grid.width as i64, self.grid.height as i64);
        let s = &self.srcs[k];
        if !o.rpc_output_window || k != 0 {
            return Some((0, 0, w as usize, h as usize));
        }
        let Ok((_, _, _, e)) = suggested_warp_output(t, s.grid.width, s.grid.height, &s.srs, &self.dst_srs) else {
            return Some((0, 0, w as usize, h as usize));
        };
        let limit = i32::MAX as f64 / 2.0;
        if e.iter().any(|v| v.abs() >= limit) {
            return Some((0, 0, w as usize, h as usize));
        }
        let pad = 5;
        let x = (e[0].floor() as i64 - pad).max(0);
        let y = (e[1].floor() as i64 - pad).max(0);
        let xs = (w - x).min(e[2].ceil() as i64 + pad - x);
        let ys = (h - y).min(e[3].ceil() as i64 + pad - y);
        (xs > 0 && ys > 0).then_some((x as usize, y as usize, xs as usize, ys as usize))
    }

    /// The warp of source `k`, through `t` ([`Plan::transformer`]), and the chunks of the output
    /// it is warped in, in order.
    fn context<'t>(&self, k: usize, t: &'t GenImgProj, o: &WarpOptions) -> (WarpContext<'t>, Vec<Chunk>, Sink) {
        let s = &self.srcs[k];
        let nodata = self.src_nodata(k, o);
        let unified = o.unified_src_nodata.unwrap_or(matches!(o.src_nodata, Nodata::Values(_))) && self.bands > 1;
        let translation = t.ct.is_none() && s.gcp.is_none() && is_pixel_translation(&s.grid.geo_transform, &self.grid.geo_transform);
        let ctx = WarpContext {
            kernel_version: o.kernel_version,
            t,
            approx: Approx { base: t, max_error: o.error_threshold },
            src_w: s.grid.width,
            src_h: s.grid.height,
            resampling: o.resampling,
            translation,
            memory_limit: o.memory_limit,
            bands: self.bands,
            out_bits: self.out_type.size_bytes() * 8,
            src_has_nodata: nodata.iter().any(Option::is_some),
            // GDAL takes a single band's validity as the unified one, whatever UNIFIED_SRC_NODATA
            // says ("If there's just a single band, then transfer papanBandSrcValid[0] as
            // panUnifiedSrcValid"); several bands are unified unless it is NO.
            global_validity: self.bands == 1 || o.unified_src_nodata != Some(false),
            dst_has_nodata: self.dst_nodata_all.iter().any(Option::is_some),
            cutline: o.cutline.is_some(),
            source_extra: o.source_extra,
            sample_all: o.sample_steps_all,
            src_grid: std::sync::OnceLock::new(),
        };
        let mut chunks = Vec::new();
        if let Some((x, y, w, h)) = self.rpc_window(k, t, o) {
            ctx.collect_chunks(x, y, w, h, &mut chunks);
        }
        chunks.sort_by_key(|c| (c.dy, c.dx));
        // As GDAL's warp kernel: valid pixels equal to the output nodata are nudged off it,
        // except with a nodata value unified across several bands. Pixels are masked by nodata
        // values and cutlines (see `validity`).
        let masked = nodata.iter().any(Option::is_some) || o.cutline.is_some();
        let sink = Sink {
            out_type: self.out_type,
            dst_nodata: self.dst_nodata_all.clone(),
            avoid_nodata: masked && (self.bands == 1 || !unified),
            avoid_all_nodata: false,
        };
        (ctx, chunks, sink)
    }
}

/// A source's nodata value for a band: explicit, else declared by the source.
fn src_nodata(s: &Source, o: &WarpOptions, band: usize) -> Option<f64> {
    match &o.src_nodata {
        Nodata::Auto => s.ds.band_nodata(band + 1),
        Nodata::None => None,
        v => v.value(band),
    }
}

/// Reprojects and resamples one or more rasters into one output, as `gdalwarp` does. Later
/// sources are drawn over earlier ones where they have valid pixels.
pub fn warp(sources: &[&RasterDataset], o: &WarpOptions) -> Result<RasterDataset> {
    let p = Plan::new(sources, o)?;
    warp_data(&p, sources, o)
}

/// Render pixels from `sources` with a plan already built from their metadata.
fn warp_data(p: &Plan<'_>, sources: &[&RasterDataset], o: &WarpOptions) -> Result<RasterDataset> {
    warp_data_with_storage(p, sources, o, p.out_type)
}

// Keep storage separate from the logical output type so its representation can
// be checked without changing resampling, quantization or chunk planning.
fn warp_data_with_storage(p: &Plan<'_>, sources: &[&RasterDataset], o: &WarpOptions, storage: DataType) -> Result<RasterDataset> {
    let (grid, bands, out_type) = (p.grid, p.bands, p.out_type);
    let n = grid.width * grid.height;
    // The output, in its type: chunks are warped into `f64` rows, then stored (the values are
    // already rounded to the type, see `Sink::value`).
    let fill = p.fill();
    let mut out = RasterData::new_filled(storage, n * bands, 0.0);
    if n != 0 {
        crate::pixel::with_data!(&mut out, v => v.chunks_mut(n).zip(&fill).for_each(|(band,fill)|band.iter_mut().for_each(|x|*x=*fill as _)));
    }

    for (k, s) in p.srcs.iter().enumerate() {
        let t = p.transformer(k)?;
        let (ctx, chunks, sink) = p.context(k, &t, o);
        t.check_policy()?;
        let nodata = p.src_nodata(k, o);
        let unified = o.unified_src_nodata.unwrap_or(matches!(o.src_nodata, Nodata::Values(_))) && bands > 1;
        let src_bands: Vec<super::BandView> = (0..bands).map(|b| super::BandView::of(&sources[k].raster, b)).collect();
        let cut_mask = match &o.cutline {
            Some(c) => Some(cutline_mask(c, s)?),
            None => None,
        };
        let valid = validity(&src_bands, &nodata, unified, cut_mask);
        // The source's bands in their type: the kernels are compiled for each (no conversion of
        // the bands, and no dispatch on the type per pixel).
        let sn = s.grid.width * s.grid.height;
        let target = Target { out: &mut out, width: grid.width, n, fill: &fill, out_type, first: k == 0 };
        crate::pixel::with_data!(&sources[k].raster.data, v => {
            let typed: Vec<&[_]> = (0..bands).map(|b| &v[b * sn..(b + 1) * sn]).collect();
            let data = SrcData { bands: &typed, valid: &valid, width: s.grid.width, origin: (0, 0) };
            warp_source(&ctx, &chunks, &data, &sink, target);
        });
        t.check_policy()?;
    }
    let mut ds = output_dataset(grid, out, bands, p.dst_nodata, &p.dst_srs)?;
    ds.raster.metadata = p.srcs[0].ds.raster.metadata.iter().filter(|(k, _)| k == "AREA_OR_POINT").cloned().collect();
    ds.band_nodata = p.dst_nodata_all.clone();
    Ok(ds)
}

/// Metadata for a window-readable warp source. Pixels may remain unloaded.
pub struct WarpSourceInfo {
    /// Full-resolution raster metadata, including its georeferencing and shared nodata.
    pub raster: RasterDataset,
    /// Overview dimensions, in the reader's level order (level 0 is the first entry).
    pub overviews: Vec<(usize, usize)>,
}

/// A source whose metadata and selected-resolution windows can be read independently.
/// Readers are called on the caller's thread; rendering their returned pixels may
/// run in parallel. This permits bindings to borrow a shared dataset only during
/// each read, including when it occurs more than once in a mosaic.
pub trait WarpSource {
    /// Visits already-loaded pixels without copying them. Returns false for lazy sources.
    /// The callback runs while any shared-reader borrow remains alive; implementations
    /// must invoke it exactly once when returning true and must not load pixels here.
    fn with_dataset(&self, _visit: &mut dyn FnMut(&RasterDataset)) -> Result<bool> {
        Ok(false)
    }
    /// Returns full-resolution metadata and the available overview dimensions.
    fn info(&mut self) -> Result<WarpSourceInfo>;
    /// Reads valid raster data for this window and overview, with the requested bands.
    fn read_window(&mut self, window: crate::Window, options: &crate::WindowOptions) -> Result<RasterDataset>;
}

impl WarpSource for crate::LazyRaster {
    fn with_dataset(&self, visit: &mut dyn FnMut(&RasterDataset)) -> Result<bool> {
        if let Some(dataset) = self.dataset() {
            visit(dataset);
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn info(&mut self) -> Result<WarpSourceInfo> {
        Ok(WarpSourceInfo { raster: self.header(), overviews: self.overviews() })
    }
    fn read_window(&mut self, window: crate::Window, options: &crate::WindowOptions) -> Result<RasterDataset> {
        crate::LazyRaster::read_window(self, window, options)
    }
}

/// Warps one raster as [`warp`] does, reading from it only the source window each chunk of the
/// output needs: for a lazy raster (a GeoTIFF or COG read on demand), a small output costs the
/// pixels it reads, not the whole source. The result is [`warp`]'s, bit for bit. Without a
/// cutline, which masks the whole source.
pub fn warp_lazy(src: &mut crate::LazyRaster, o: &WarpOptions) -> Result<RasterDataset> {
    warp_sources(&mut [src], o)
}

fn warp_reader(src: &mut dyn WarpSource, o: &WarpOptions) -> Result<RasterDataset> {
    warp_reader_selected(src, o, None, None, false, None)
}

/// Serialized VRT working type: source conversion happens before validity and resampling.
pub(crate) fn warp_lazy_bands_working(
    src: &mut crate::LazyRaster,
    bands: &[usize],
    o: &WarpOptions,
    working: Option<DataType>,
    unified_sink: bool,
    source_overview: Option<usize>,
) -> Result<RasterDataset> {
    if bands.is_empty() || bands.iter().any(|b| *b == 0 || *b > src.band_count()) {
        return Err(Error::InvalidArgument("invalid source band selection".into()));
    }
    warp_reader_selected(src, o, Some(bands), working, unified_sink, source_overview)
}

fn warp_reader_selected(
    src: &mut dyn WarpSource,
    o: &WarpOptions,
    selection: Option<&[usize]>,
    working: Option<DataType>,
    unified_sink: bool,
    source_overview: Option<usize>,
) -> Result<RasterDataset> {
    if o.cutline.is_some() {
        if selection.is_some() || working.is_some() {
            return Err(Error::Unsupported("a serialized working warp cannot use a cutline".into()));
        }
        return warp_sources(&mut [src], o);
    }
    let info = src.info()?;
    let mut header = info.raster;
    if let Some(level) = source_overview {
        let &(width, height) = info
            .overviews
            .get(level)
            .ok_or_else(|| Error::InvalidArgument(format!("serialized source overview {level} is unavailable")))?;
        let (sx, sy) = (header.raster.cols as f64 / width as f64, header.raster.rows as f64 / height as f64);
        let gt = header.geo_transform();
        header.raster.cols = width;
        header.raster.rows = height;
        header.set_geo_transform([gt[0], gt[1] * sx, gt[2] * sy, gt[3], gt[4] * sx, gt[5] * sy]);
    }
    if let Some(bands) = selection {
        header.band_nodata = bands.iter().map(|b| header.band_nodata(*b)).collect();
        header.has_nodata = header.band_nodata[0].is_some();
        header.raster.nodata = header.band_nodata[0].unwrap_or(-9999.);
        header.raster.bands = bands.len();
        header.raster.data = RasterData::new_filled(header.raster.data_type, 0, 0.);
    }
    if let Some(dtype) = working {
        header.raster.data_type = dtype;
        header.raster.data = RasterData::new_filled(dtype, 0, 0.);
    }
    let mut p = Plan::new(&[&header], o)?;
    // A serialized VRT's source view is already selected in OpenOptions and its
    // transformer. Selecting again would double-scale its source coordinates.
    let overview = if working.is_some() { source_overview } else { p.select_overview(0, &info.overviews, o)? };
    let read_options = crate::WindowOptions { bands: selection.map_or_else(Vec::new, |v| v.to_vec()), overview };
    let (grid, bands, out_type) = (p.grid, p.bands, p.out_type);
    let n = grid.width * grid.height;
    let t = p.transformer(0)?;
    let (ctx, chunks, mut sink) = p.context(0, &t, o);
    let nodata = p.src_nodata(0, o);
    if working.is_some() && nodata.iter().any(Option::is_some) {
        sink.avoid_nodata = bands == 1 || !unified_sink;
        sink.avoid_all_nodata = bands > 1 && unified_sink;
    }
    let unified = o.unified_src_nodata.unwrap_or(matches!(o.src_nodata, Nodata::Values(_))) && bands > 1;
    let fill = p.fill();
    let mut out = RasterData::new_filled(out_type, n * bands, 0.0);
    if n != 0 {
        crate::pixel::with_data!(&mut out, v => v.chunks_mut(n).zip(&fill).for_each(|(band,fill)|band.iter_mut().for_each(|x|*x=*fill as _)));
    }
    for c in &chunks {
        let mut window = src.read_window(crate::Window::new(c.sx, c.sy, c.sw, c.sh), &read_options)?;
        if let Some(dtype) = working
            && window.raster.data_type != dtype
        {
            let values = window.raster.data.to_f64_vec().into_iter().map(|v| super::convert_value(v, dtype)).collect();
            window.raster.data = RasterData::from_f64_vec(dtype, values);
            window.raster.data_type = dtype;
        }
        let src_bands: Vec<super::BandView> = (0..bands).map(|b| super::BandView::of(&window.raster, b)).collect();
        let valid = validity(&src_bands, &nodata, unified, None);
        let sn = c.sw * c.sh;
        // The chunks partition the output: each finds it at its initial value.
        let target = Target { out: &mut out, width: grid.width, n, fill: &fill, out_type, first: true };
        crate::pixel::with_data!(&window.raster.data, v => {
            let typed: Vec<&[_]> = (0..bands).map(|b| &v[b * sn..(b + 1) * sn]).collect();
            let data = SrcData { bands: &typed, valid: &valid, width: c.sw, origin: (c.sx, c.sy) };
            warp_source(&ctx, std::slice::from_ref(c), &data, &sink, target);
        });
    }
    t.check_policy()?;
    let mut ds = output_dataset(grid, out, bands, p.dst_nodata, &p.dst_srs)?;
    ds.raster.metadata = p.srcs[0].ds.raster.metadata.iter().filter(|(k, _)| k == "AREA_OR_POINT").cloned().collect();
    ds.band_nodata = p.dst_nodata_all.clone();
    Ok(ds)
}

/// Warps multiple lazy rasters, selecting a stored source resolution for each one.
/// The target grid is computed from full-resolution headers before selection.
/// Single sources without cutlines use windowed reads; mosaics and cutlines load
/// only the selected source levels into memory before rendering.
///
/// ```no_run
/// let mut a = terrio::open_raster_lazy("a.tif")?;
/// let mut b = terrio::open_raster_lazy("b.tif")?;
/// let out = terrio::ops::warp::warp_lazy_many(&mut [&mut a, &mut b], &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn warp_lazy_many(sources: &mut [&mut crate::LazyRaster], o: &WarpOptions) -> Result<RasterDataset> {
    let mut readers: Vec<&mut dyn WarpSource> = sources.iter_mut().map(|s| &mut **s as &mut dyn WarpSource).collect();
    warp_sources(&mut readers, o)
}

/// Warps window-readable sources, retaining stored overviews across language bindings.
/// A single source without a cutline is read by chunk. Multiple sources and cutlines
/// currently load the selected source levels before compositing; the target grid is
/// always computed from full-resolution metadata.
///
/// ```no_run
/// use terrio::ops::warp::{warp_sources, WarpSource};
/// let mut dem = terrio::open_raster_lazy("dem.tif")?;
/// let sources: &mut [&mut dyn WarpSource] = &mut [&mut dem];
/// let out = warp_sources(sources, &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn warp_sources(sources: &mut [&mut dyn WarpSource], o: &WarpOptions) -> Result<RasterDataset> {
    // Scoped visits retain binding borrow guards until rendering has finished, including
    // repeated references to one shared raster. No full or per-chunk pixel copies.
    fn memory(sources: &[&mut dyn WarpSource], datasets: &[&RasterDataset], o: &WarpOptions) -> Result<Option<RasterDataset>> {
        let Some((first, rest)) = sources.split_first() else {
            if let Some(first) = datasets.first() {
                o.overview.select(first.raster.cols, &[], 1.0);
            }
            return warp(datasets, o).map(Some);
        };
        let mut result = Ok(None);
        first.with_dataset(&mut |dataset| {
            let mut datasets = datasets.to_vec();
            datasets.push(dataset);
            result = memory(rest, &datasets, o);
        })?;
        result
    }
    // Bound nested shared-reader guards for very large in-memory mosaics.
    if sources.len() <= 128
        && let Some(output) = memory(sources, &[], o)?
    {
        return Ok(output);
    }
    if sources.len() == 1 && o.cutline.is_none() {
        return warp_reader(sources[0], o);
    }
    let info = sources.iter_mut().map(|s| s.info()).collect::<Result<Vec<_>>>()?;
    let refs: Vec<_> = info.iter().map(|s| &s.raster).collect();
    let mut p = Plan::new(&refs, o)?;
    let mut data = Vec::with_capacity(sources.len());
    for (k, source) in sources.iter_mut().enumerate() {
        let overview = p.select_overview(k, &info[k].overviews, o)?;
        let grid = p.srcs[k].grid;
        data.push(source.read_window(crate::Window::full(grid.width, grid.height), &crate::WindowOptions { overview, bands: Vec::new() })?);
    }
    let refs: Vec<_> = data.iter().collect();
    warp_data(&p, &refs, o)
}

/// Opens and warps paths with lazy source reads and stored overview selection.
/// The first source's file GCPs or RPCs are used when needed, as in [`warp_to_path`].
///
/// ```no_run
/// let out = terrio::ops::warp::warp_paths(&["dem.tif".into()], &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn warp_paths(paths: &[String], o: &WarpOptions) -> Result<RasterDataset> {
    let mut sources = paths.iter().map(|path| crate::open_raster_lazy(path)).collect::<Result<Vec<_>>>()?;
    let first = sources.first().ok_or_else(|| Error::InvalidArgument("no source rasters".into()))?;
    let header = first.header();
    let o = with_file_gcps(o, &paths[0], &header)?;
    let mut refs: Vec<_> = sources.iter_mut().collect();
    warp_lazy_many(&mut refs, &o)
}

/// The warped raster of `grid`, holding `out`, whatever its geotransform.
fn output_dataset(grid: Grid, out: RasterData, bands: usize, nodata: Option<f64>, srs: &SpatialRef) -> Result<RasterDataset> {
    let (stored, gt) = stored_grid(grid);
    let mut ds = super::build_raster_data(stored, out, bands, nodata, srs)?;
    if let Some(gt) = gt {
        ds.set_geo_transform(gt);
    }
    Ok(ds)
}

/// `grid` with a north-up geotransform, as rasters are built, and its own geotransform when it
/// is not north-up (a rotated [`WarpOptions::dst_transform`]), to set on the output after.
fn stored_grid(grid: Grid) -> (Grid, Option<[f64; 6]>) {
    if crate::is_north_up(&grid.geo_transform) {
        (grid, None)
    } else {
        (Grid { geo_transform: [0.0, 1.0, 0.0, grid.height as f64, 0.0, -1.0], ..grid }, Some(grid.geo_transform))
    }
}

/// Warps rasters as [`warp`] does and writes the output to `path`. A single source without a
/// cutline is read a chunk at a time (the source window each chunk of the output needs), and a
/// GeoTIFF output written as its rows are done: neither is held whole. Otherwise (and when
/// `path` is the source) the sources are read whole. Without `o.gcps` and `o.rpc`, a first source
/// with no geotransform is georeferenced by its GCPs (else its RPCs), as `gdalwarp` does.
///
/// ```no_run
/// use terrio::ops::warp::{WarpOptions, warp_to_path};
///
/// let o = WarpOptions { dst_srs: Some(terrio::crs::SpatialRef::parse("EPSG:3857")?), ..Default::default() };
/// warp_to_path(&["dem.tif".to_string()], &o, "dem_3857.tif", &Default::default())?;
/// # Ok::<(), terrio::Error>(())
/// ```
pub fn warp_to_path(paths: &[String], o: &WarpOptions, path: &str, options: &crate::RasterWriteOptions) -> Result<()> {
    if options.driver == Some(crate::formats::RasterDriver::VRT)
        || (options.driver.is_none() && crate::formats::extension(path).as_deref() == Some("vrt"))
    {
        if paths.len() != 1 {
            return Err(Error::Unsupported("a warped VRT requires one source; mosaic it first".into()));
        }
        if options.compression.is_some()
            || options.tile_size.is_some()
            || options.bigtiff
            || !options.creation_options.is_empty()
            || !options.band_metadata.is_empty()
            || !options.color_table.is_empty()
            || !options.overviews.is_empty()
            || options.overview_resampling.is_some()
            || options.geo_transform.is_some()
            || !options.gcps.is_empty()
            || options.mask.is_some()
            || !options.rpc.is_empty()
        {
            return Err(Error::Unsupported("raster creation options are not supported for a serialized warped VRT".into()));
        }

        if super::strips::overwrites(path, paths) {
            return Err(Error::InvalidArgument("a VRT cannot replace its source".into()));
        }
        let document = super::vrt::VirtualRaster::warped(&paths[0], o)?;
        return Ok(crate::memfs::write(std::path::Path::new(path), document.to_xml())?);
    }
    if paths.is_empty() {
        return Err(Error::InvalidArgument("no source rasters".into()));
    }
    if paths.len() != 1 || o.cutline.is_some() || super::strips::overwrites(path, paths) {
        return crate::write_raster_dataset(&warp_paths(paths, o)?, path, options);
    }
    let mut src = crate::open_raster_lazy(&paths[0])?;
    let header = src.header();
    let o = &*with_file_gcps(o, &paths[0], &header)?;
    let mut p = Plan::new(&[&header], o)?;
    let overview = p.select_overview(0, &src.overviews(), o)?;
    let (grid, bands, out_type) = (p.grid, p.bands, p.out_type);
    let w = grid.width;
    let t = p.transformer(0)?;
    let (ctx, chunks, sink) = p.context(0, &t, o);
    t.check_policy()?;
    let nodata = p.src_nodata(0, o);
    let unified = o.unified_src_nodata.unwrap_or(matches!(o.src_nodata, Nodata::Values(_))) && bands > 1;
    let fill = p.fill();
    let (stored, gt) = stored_grid(grid);
    let mut header = super::raster_header(stored, out_type, bands, p.dst_nodata, &p.dst_srs)?;
    header.raster.metadata = p.srcs[0].ds.raster.metadata.iter().filter(|(k, _)| k == "AREA_OR_POINT").cloned().collect();
    header.band_nodata = p.dst_nodata_all.clone();
    if let Some(gt) = gt {
        header.set_geo_transform(gt);
    }
    let mut out = super::strips::StripWriter::create(&header, path, options)?;

    // The last chunk (in order) writing each row: rows are written once it is warped.
    let mut last: Vec<Option<usize>> = vec![None; grid.height];
    for (i, c) in chunks.iter().enumerate() {
        last[c.dy..c.dy + c.dh].iter_mut().for_each(|l| *l = Some(i));
    }
    // The rows warped but not yet written, from row `base`, band after band, in the output type.
    let mut rows = RasterData::new_filled(out_type, 0, 0.0);
    let mut base = 0;
    // Adds rows of the initial value up to row `upto`.
    let grow = |rows: &mut RasterData, base: usize, upto: usize| {
        let (old, new) = (rows.len() / bands / w, upto.saturating_sub(base));
        if new > old {
            crate::pixel::with_data!(rows, v => {
                let mut grown = Vec::with_capacity(new * w * bands);
                for b in 0..bands {
                    grown.extend_from_slice(&v[b * old * w..(b + 1) * old * w]);
                    grown.resize((b + 1) * new * w, fill[b] as _);
                }
                *v = grown;
            });
        }
    };
    let mut write_done = |rows: &mut RasterData, base: &mut usize, upto: usize| -> Result<()> {
        if upto <= *base {
            return Ok(());
        }
        grow(rows, *base, upto);
        let (n, total) = ((upto - *base) * w, rows.len() / bands);
        let data = crate::pixel::map_data!(&*rows, v => (0..bands).flat_map(|b| v[b * total..b * total + n].iter().copied()).collect());
        crate::pixel::with_data!(rows, v => {
            *v = (0..bands).flat_map(|b| v[b * total + n..(b + 1) * total].iter().copied()).collect();
        });
        *base = upto;
        out.write(data, n / w)
    };
    // Rows no chunk writes, at the top.
    let first_row = |from: usize, done: usize| (from..grid.height).find(|r| last[*r].is_some_and(|l| l > done)).unwrap_or(grid.height);
    if chunks.is_empty() {
        write_done(&mut rows, &mut base, grid.height)?;
    }
    super::strips::pipelined(
        chunks.len(),
        |i| {
            let c = &chunks[i];
            src.read_window(crate::Window::new(c.sx, c.sy, c.sw, c.sh), &crate::WindowOptions { overview, bands: Vec::new() })
        },
        |i, window| {
            let c = &chunks[i];
            // The chunk's part of the output, as `f64`: the output's initial value (the chunks of
            // a source don't overlap), warped.
            let mut staged: Vec<Vec<f64>> = fill.iter().map(|fill| vec![*fill; c.dw * c.dh]).collect();
            let src_bands: Vec<super::BandView> = (0..bands).map(|b| super::BandView::of(&window.raster, b)).collect();
            let valid = validity(&src_bands, &nodata, unified, None);
            let sn = c.sw * c.sh;
            crate::pixel::with_data!(&window.raster.data, v => {
                let typed: Vec<&[_]> = (0..bands).map(|b| &v[b * sn..(b + 1) * sn]).collect();
                let data = SrcData { bands: &typed, valid: &valid, width: c.sw, origin: (c.sx, c.sy) };
                ctx.warp_chunk(c, &data, &sink, chunk_rows(&mut staged, c.dw, &Chunk { dx: 0, dy: 0, ..*c }));
            });
            // Stored in the pending rows, rounded to the output type.
            grow(&mut rows, base, c.dy + c.dh);
            let total = rows.len() / bands;
            crate::pixel::with_data!(&mut rows, v => {
                for (b, part) in staged.iter().enumerate() {
                    for (r, line) in part.chunks(c.dw).enumerate() {
                        let at = b * total + (c.dy + r - base) * w + c.dx;
                        for (o, x) in v[at..at + c.dw].iter_mut().zip(line) {
                            *o = warp_round(*x, out_type) as _;
                        }
                    }
                }
            });
            let upto = first_row(base, i);
            write_done(&mut rows, &mut base, upto)
        },
    )?;
    write_done(&mut rows, &mut base, grid.height)?;
    drop(write_done);
    t.check_policy()?;
    out.finish()
}

/// Plan a virtual warp without reading source pixels.
pub(crate) fn virtual_header(source: &RasterDataset, o: &WarpOptions) -> Result<RasterDataset> {
    let p = Plan::new(&[source], o)?;
    // As GDAL's VRT authoring path: explicit destination nodata is adjusted to
    // the band type, while inherited nodata and parsed XML retain raw values.
    let nodata: Vec<_> = p
        .dst_nodata_all
        .iter()
        .map(|value| value.map(|value| if matches!(o.dst_nodata, Nodata::Auto) { value } else { super::convert_value(value, p.out_type) }))
        .collect();
    let mut header = super::raster_header(p.grid, p.out_type, p.bands, nodata[0], &p.dst_srs)?;
    header.band_nodata = nodata;
    Ok(header)
}

/// Selects the source view for a serialized warp after computing its full-resolution target grid.
pub(crate) fn virtual_source_overview(source: &RasterDataset, levels: &[(usize, usize)], o: &WarpOptions) -> Result<Option<usize>> {
    let mut plan = Plan::new(&[source], o)?;
    plan.select_overview(0, levels, o)
}

/// `o`, with the GCPs of the raster at `path` (opened as `ds`) when `o` has neither GCPs nor RPCs
/// and the raster has no geotransform; else with its RPCs, as GDAL picks a geotransform, then
/// GCPs, then RPCs.
pub fn with_file_gcps<'a>(o: &'a WarpOptions, path: &str, ds: &RasterDataset) -> Result<std::borrow::Cow<'a, WarpOptions>> {
    let default_gt = [0.0, 1.0, 0.0, ds.raster.rows as f64, 0.0, -1.0];
    if o.geoloc
        || o.geolocation.is_some()
        || ds.geolocation.is_some()
        || !o.gcps.is_empty()
        || o.rpc.is_some()
        || o.homography.is_some()
        || o.src_transform.is_some()
        || ds.affine.is_some()
        || ds.geo_transform() != default_gt
    {
        return Ok(std::borrow::Cow::Borrowed(o));
    }
    let gcps = crate::read_gcps(path)?;
    if gcps.is_empty() {
        if let Some(rpc) = super::rpc::read_rpc(path)? {
            return Ok(std::borrow::Cow::Owned(WarpOptions { rpc: Some(rpc), ..o.clone() }));
        }
        return Ok(std::borrow::Cow::Borrowed(o));
    }
    Ok(std::borrow::Cow::Owned(WarpOptions { gcps, ..o.clone() }))
}

/// Whether the destination grid is the source grid shifted by whole pixels.
fn is_pixel_translation(src: &[f64; 6], dst: &[f64; 6]) -> bool {
    let near_int = |v: f64| (v - v.round()).abs() < 1e-8;
    src[2] == 0.0
        && src[4] == 0.0
        && (src[1] - dst[1]).abs() < 1e-12 * src[1].abs()
        && (src[5] - dst[5]).abs() < 1e-12 * src[5].abs()
        && near_int((dst[0] - src[0]) / src[1])
        && near_int((dst[3] - src[3]) / src[5])
}

/// Per-band validity masks (`None`: all valid), from nodata values and a cutline mask. With a
/// unified nodata, a pixel is invalid only when every band is nodata.
fn validity(bands: &[super::BandView], nodata: &[Option<f64>], unified: bool, cut: Option<Vec<bool>>) -> Vec<Option<Vec<bool>>> {
    let combine = |m: Option<Vec<bool>>| -> Option<Vec<bool>> {
        match (m, &cut) {
            (Some(mut m), Some(c)) => {
                m.iter_mut().zip(c).for_each(|(a, b)| *a &= *b);
                Some(m)
            }
            (None, Some(c)) => Some(c.clone()),
            (m, None) => m,
        }
    };
    if unified && nodata.iter().all(Option::is_some) {
        let n = bands[0].len();
        let m: Vec<bool> = (0..n).map(|i| bands.iter().zip(nodata).any(|(b, nd)| !is_nodata(b.get(i), nd.unwrap()))).collect();
        let m = combine(Some(m));
        return vec![m; bands.len()];
    }
    bands.iter().zip(nodata).map(|(b, nd)| combine(nd.map(|nd| crate::par::map_range(b.len(), |i| !is_nodata(b.get(i), nd))))).collect()
}

/// The transformed extent of a cutline, as `gdalwarp -crop_to_cutline` computes it.
fn cutline_extent(c: &Cutline, s: &Source, dst_srs: &SpatialRef, has_res: bool) -> Result<(f64, f64, f64, f64)> {
    let to_src: Option<Transformer> = match &c.srs {
        Some(cs) if !same_crs(cs, &s.srs) => Some(Transformer::with_options(cs, &s.srs, &s.transform_options)?),
        _ => None,
    };
    let to_dst = (!same_crs(&s.srs, dst_srs)).then(|| Transformer::with_options(&s.srs, dst_srs, &s.transform_options)).transpose()?;
    let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
    for poly in &c.polygons {
        for ring in poly {
            for w in ring.windows(2).chain(std::iter::once(&ring[ring.len().saturating_sub(1)..])) {
                // Densify each edge in the source CRS before reprojecting it.
                let a = w[0];
                let b = *w.get(1).unwrap_or(&w[0]);
                let steps = if to_dst.is_some() && w.len() == 2 { 32 } else { 1 };
                for k in 0..steps {
                    let f = k as f64 / steps as f64;
                    let mut p = (a.0 + (b.0 - a.0) * f, a.1 + (b.1 - a.1) * f);
                    if let Some(t) = &to_src {
                        p = t.transform(p.0, p.1)?;
                    }
                    if let Some(t) = &to_dst {
                        p = t.transform(p.0, p.1)?;
                    }
                    x0 = x0.min(p.0);
                    y0 = y0.min(p.1);
                    x1 = x1.max(p.0);
                    y1 = y1.max(p.1);
                }
            }
        }
    }
    if to_dst.is_none() && !has_res {
        // No reprojection: snap inwards on the source's pixel boundaries.
        let gt = s.grid.geo_transform;
        const REL: f64 = 1e-3;
        x0 = gt[0] + ((x0 - gt[0]) / gt[1] - REL).ceil() * gt[1];
        y0 = gt[3] + ((y0 - gt[3]) / gt[5] + REL).floor() * gt[5];
        x1 = gt[0] + ((x1 - gt[0]) / gt[1] + REL).floor() * gt[1];
        y1 = gt[3] + ((y1 - gt[3]) / gt[5] - REL).ceil() * gt[5];
    }
    for t in [&to_src, &to_dst].into_iter().flatten() {
        t.check_policy()?;
    }
    Ok((x0, y0, x1, y1))
}

/// Rasterizes the cutline in a source's pixel space: pixels whose centre is inside are valid.
fn cutline_mask(c: &Cutline, s: &Source) -> Result<Vec<bool>> {
    let to_src: Option<Transformer> = match &c.srs {
        Some(cs) if !same_crs(cs, &s.srs) => Some(Transformer::with_options(cs, &s.srs, &s.transform_options)?),
        _ => None,
    };
    let inv = invert_gt(&s.grid.geo_transform).ok_or_else(|| Error::InvalidData("invalid geotransform".into()))?;
    let polys = c.map(&|x, y| {
        let (x, y) = match &to_src {
            Some(t) => t.transform(x, y).ok()?,
            None => (x, y),
        };
        Some(apply_gt(&inv, x, y))
    })?;
    if let Some(t) = &to_src {
        t.check_policy()?;
    }
    Ok(rasterize_polygons(&polys, s.grid.width, s.grid.height))
}

/// Burns polygons (in pixel coordinates) into a mask with GDAL's pixel-centre rule (even-odd
/// filling of all rings).
pub(crate) fn rasterize_polygons(polys: &[Vec<Vec<(f64, f64)>>], w: usize, h: usize) -> Vec<bool> {
    let mut mask = vec![false; w * h];
    let mut xs = Vec::new();
    for poly in polys {
        let (mut ymin, mut ymax) = (f64::INFINITY, f64::NEG_INFINITY);
        for p in poly.iter().flatten() {
            ymin = ymin.min(p.1);
            ymax = ymax.max(p.1);
        }
        if !ymin.is_finite() {
            continue;
        }
        let r0 = (ymin.floor().max(0.0)) as usize;
        let r1 = (ymax.ceil().max(0.0) as usize).min(h);
        for row in r0..r1 {
            let y = row as f64 + 0.5;
            xs.clear();
            for ring in poly {
                let n = ring.len();
                for i in 0..n {
                    let (a, b) = (ring[i], ring[(i + 1) % n]);
                    if a == b {
                        continue;
                    }
                    let (lo, hi) = if a.1 < b.1 { (a, b) } else { (b, a) };
                    if y < lo.1 || y >= hi.1 {
                        continue;
                    }
                    xs.push(lo.0 + (y - lo.1) * (hi.0 - lo.0) / (hi.1 - lo.1));
                }
            }
            xs.sort_by(f64::total_cmp);
            for pair in xs.chunks(2) {
                if pair.len() < 2 {
                    break;
                }
                let c0 = ((pair[0] + 0.5).floor().max(0.0)) as usize;
                let c1 = ((pair[1] + 0.5).floor().max(0.0) as usize).min(w);
                for col in c0..c1 {
                    mask[row * w + col] = true;
                }
            }
        }
    }
    mask
}

/// A chunk of the output and the source window it reads.
#[derive(Debug, Clone, Copy)]
struct Chunk {
    dx: usize,
    dy: usize,
    dw: usize,
    dh: usize,
    sx: usize,
    sy: usize,
    sw: usize,
    sh: usize,
    /// The unpadded size of the source window (GDAL's `dfSrcXSizeRaw`, `dfSrcYSizeRaw`).
    raw_w: f64,
    raw_h: f64,
}

/// Scales from four transformed unit-square corners in GDAL's 00, 10, 01, 11 order.
fn square_scales(p: [(f64, f64); 4]) -> Option<(f64, f64)> {
    let edge = |a: usize, b: usize| (p[b].0 - p[a].0, p[b].1 - p[a].1);
    let length2 = |v: (f64, f64)| v.0 * v.0 + v.1 * v.1;
    let (v01, v23) = (edge(0, 1), edge(2, 3));
    let (a, b, c, d) = (length2(v01), length2(v23), length2(edge(0, 2)), length2(edge(1, 3)));
    let (x, y) =
        if v01.1.abs() < 0.35 * v01.0 && v23.1.abs() < 0.35 * v23.0 && (a - b).abs() < 0.1 * a.abs() && (c - d).abs() < 0.1 * c.abs() {
            ((a * b).sqrt().sqrt(), (c * d).sqrt().sqrt())
        } else {
            let poly = [p[0], p[1], p[3], p[2]];
            let mut area = 0.0;
            for i in 0..4 {
                area += poly[i].0 * (poly[(i + 1) % 4].1 - poly[(i + 3) % 4].1);
            }
            let side = (0.5 * area.abs()).sqrt();
            (side, side)
        };
    (x > 1e-10 && y > 1e-10).then_some((1.0 / x, 1.0 / y))
}

struct WarpContext<'a> {
    kernel_version: KernelVersion,
    t: &'a GenImgProj,
    approx: Approx<'a>,
    src_w: usize,
    src_h: usize,
    resampling: Resampling,
    translation: bool,
    memory_limit: f64,
    bands: usize,
    out_bits: usize,
    src_has_nodata: bool,
    /// GDAL's unified source validity (`panUnifiedSrcValid`): an output pixel whose source pixel
    /// under the sample point is invalid in every band is left unwritten.
    global_validity: bool,
    dst_has_nodata: bool,
    cutline: bool,
    /// GDAL's `SOURCE_EXTRA` warp option.
    source_extra: Option<usize>,
    /// GDAL's `SAMPLE_STEPS=ALL` warp option.
    sample_all: bool,
    /// A grid of source points and where they land in the output (computed once).
    #[allow(clippy::type_complexity)]
    src_grid: std::sync::OnceLock<(Vec<(f64, f64)>, Vec<(f64, f64)>, Vec<bool>)>,
}

/// A source's pixels, or a window of them: `width` columns from the source's pixel `origin`.
struct SrcData<'a, T> {
    bands: &'a [&'a [T]],
    valid: &'a [Option<Vec<bool>>],
    width: usize,
    origin: (usize, usize),
}

/// The output warped into: all its bands (`n` values each, rows of `width`), in its type.
struct Target<'a> {
    out: &'a mut RasterData,
    width: usize,
    n: usize,
    /// The initial value, rounded to the output type.
    fill: &'a [f64],
    out_type: DataType,
    /// Whether this is the first source (whose chunks find the output at its initial value).
    first: bool,
}

/// Warps a source's chunks into the output: each chunk's part of the output as `f64` (the first
/// source's chunks don't overlap: the output there is still its initial value), warped, then
/// stored back. The chunks of a source partition the output (`collect_chunks`), and the values
/// stored are already rounded to the output type (`Sink::value`), so storing them back is exact.
fn warp_source<T: crate::pixel::Pixel>(ctx: &WarpContext, chunks: &[Chunk], data: &SrcData<T>, sink: &Sink, t: Target) {
    let bands = data.bands.len();
    let (w, n) = (t.width, t.n);
    let mut staged: Vec<Vec<f64>> = vec![Vec::new(); bands];
    for c in chunks {
        let rows = c.dy..c.dy + c.dh;
        for (b, part) in staged.iter_mut().enumerate() {
            part.clear();
            part.resize(c.dw * c.dh, t.fill[b]);
            if !t.first {
                crate::pixel::with_data!(&*t.out, v => {
                    let src = &v[b * n + rows.start * w..b * n + rows.end * w];
                    crate::par::chunks_mut(part, c.dw, |r, o| {
                        for (o, x) in o.iter_mut().zip(&src[r * w + c.dx..r * w + c.dx + c.dw]) {
                            *o = crate::pixel::Pixel::to_f64(*x);
                        }
                    });
                });
            }
        }
        ctx.warp_chunk(c, data, sink, chunk_rows(&mut staged, c.dw, &Chunk { dx: 0, dy: 0, ..*c }));
        for (b, part) in staged.iter().enumerate() {
            crate::pixel::with_data!(&mut *t.out, v => {
                let dst = &mut v[b * n + rows.start * w..b * n + rows.end * w];
                crate::par::chunks_mut(dst, w, |r, o| {
                    for (o, x) in o[c.dx..c.dx + c.dw].iter_mut().zip(&part[r * c.dw..(r + 1) * c.dw]) {
                        *o = warp_round(*x, t.out_type) as _;
                    }
                });
            });
        }
    }
}

/// How warped values are stored in the output.
struct Sink {
    out_type: DataType,
    dst_nodata: Vec<Option<f64>>,
    avoid_nodata: bool,
    avoid_all_nodata: bool,
}

/// One output row of a chunk: the chunk's part of the row in each band.
type RowOut<'a> = Vec<&'a mut [f64]>;

/// Splits the part of the output a chunk covers into rows, each with its slice of every band.
fn chunk_rows<'a>(out: &'a mut [Vec<f64>], width: usize, c: &Chunk) -> Vec<RowOut<'a>> {
    let mut rows: Vec<RowOut> = (0..c.dh).map(|_| Vec::with_capacity(out.len())).collect();
    for band in out.iter_mut() {
        for (row, line) in rows.iter_mut().zip(band.chunks_mut(width).skip(c.dy)) {
            row.push(&mut line[c.dx..c.dx + c.dw]);
        }
    }
    rows
}

impl Sink {
    fn nudge(&self, v: f64) -> f64 {
        if super::is_integer(self.out_type) {
            let (lo, _) = super::type_range(self.out_type);
            if v == lo { lo + 1. } else { v - 1. }
        } else if self.out_type == DataType::F32 {
            let f = v as f32;
            (if f == f32::MAX { f32::from_bits(f.to_bits() - 1) } else { next_up_f32(f) }) as f64
        } else if v == f64::MAX {
            f64::from_bits(v.to_bits() - 1)
        } else {
            next_up_f64(v)
        }
    }
    /// The value to store for a warped value `v`.
    fn value(&self, v: f64, band: usize) -> f64 {
        let v = warp_round(v, self.out_type);
        if self.avoid_nodata && self.dst_nodata[band].is_some_and(|nd| v == nd) { self.nudge(v) } else { v }
    }
    fn avoid_all(&self, out: &mut [&mut [f64]], index: usize) {
        if self.avoid_all_nodata && out.iter().zip(&self.dst_nodata).all(|(v, nd)| nd.is_some_and(|nd| v[index] == nd)) {
            for band in out {
                band[index] = self.nudge(band[index]);
            }
        }
    }
}

fn next_up_f32(f: f32) -> f32 {
    if f == 0.0 {
        return f32::from_bits(1);
    }
    let b = f.to_bits();
    f32::from_bits(if f > 0.0 { b + 1 } else { b - 1 })
}

fn next_up_f64(f: f64) -> f64 {
    if f == 0.0 {
        return f64::from_bits(1);
    }
    let b = f.to_bits();
    f64::from_bits(if f > 0.0 { b + 1 } else { b - 1 })
}

impl WarpContext<'_> {
    fn working_memory(&self, sw: usize, sh: usize, dw: usize, dh: usize) -> f64 {
        let mut src_bits = self.out_bits * self.bands;
        if self.cutline {
            src_bits += 32;
        }
        if self.src_has_nodata {
            src_bits += self.bands;
        }
        let mut dst_bits = self.out_bits * self.bands;
        if self.dst_has_nodata {
            dst_bits += self.bands;
        }
        (src_bits as f64 * sw as f64 * sh as f64 + dst_bits as f64 * dw as f64 * dh as f64) / 8.0
    }

    /// GDAL's `CollectChunkListInternal()`: splits the output in halves until a chunk's working
    /// memory fits.
    fn collect_chunks(&self, dx: usize, dy: usize, dw: usize, dh: usize, out: &mut Vec<Chunk>) {
        let win = self.source_window(dx, dy, dw, dh);
        let (sx, sy, sw, sh, fill, raw_w, raw_h) = win.unwrap_or((0, 0, 0, 0, 0.0, 0.0, 0.0));
        let mem = self.working_memory(sw, sh, dw, dh);
        if (mem > self.memory_limit && (dw > 2 || dh > 2)) || (fill > 0.0 && fill < 0.5 && (dw > 100 || dh > 100)) {
            if dw > dh {
                let c1 = dw / 2;
                self.collect_chunks(dx, dy, c1, dh, out);
                self.collect_chunks(dx + c1, dy, dw - c1, dh, out);
            } else {
                let c1 = dh / 2;
                self.collect_chunks(dx, dy, dw, c1, out);
                self.collect_chunks(dx, dy + c1, dw, dh - c1, out);
            }
            return;
        }
        if win.is_some() && sw > 0 && sh > 0 {
            out.push(Chunk { dx, dy, dw, dh, sx, sy, sw, sh, raw_w, raw_h });
        }
    }

    /// GDAL's `ComputeSourceWindow()`: the source window a destination window reads, with room for
    /// the kernel, and the fill ratio.
    #[allow(clippy::type_complexity)]
    fn source_window(&self, dx: usize, dy: usize, dw: usize, dh: usize) -> Option<(usize, usize, usize, usize, f64, f64, f64)> {
        let (dxf, dyf, dwf, dhf) = (dx as f64, dy as f64, dw as f64, dh as f64);
        let all = self.sample_all || {
            let mut cx = [dxf, dxf + dwf, dxf, dxf + dwf];
            let mut cy = [dyf, dyf, dyf + dhf, dyf + dhf];
            let mut cok = [false; 4];
            self.approx.transform(true, &mut cx, &mut cy, &mut cok);
            !cok.iter().all(|b| *b)
        };
        let sample = |grid: bool| -> (Vec<f64>, Vec<f64>) {
            let (mut xs, mut ys) = (Vec::new(), Vec::new());
            if grid {
                let steps = 21usize;
                let step = 1.0 / (steps - 1) as f64;
                for iy in 0..steps + 2 {
                    let ry = if iy == 0 {
                        0.5 / dwf
                    } else if iy <= steps {
                        (iy - 1) as f64 * step
                    } else {
                        1.0 - 0.5 / dwf
                    };
                    for ix in 0..steps + 2 {
                        let rx = if ix == 0 {
                            0.5 / dwf
                        } else if ix <= steps {
                            (ix - 1) as f64 * step
                        } else {
                            1.0 - 0.5 / dwf
                        };
                        xs.push(rx * dwf + dxf);
                        ys.push(ry * dhf + dyf);
                    }
                }
            } else if all {
                for ix in 0..=dw {
                    xs.extend([dxf + ix as f64, dxf + ix as f64]);
                    ys.extend([dyf, dyf + dhf]);
                }
                for iy in 1..dh {
                    xs.extend([dxf, dxf + dwf]);
                    ys.extend([dyf + iy as f64, dyf + iy as f64]);
                }
            } else {
                let step = 1.0 / 20.0;
                let mut r = 0.0;
                while r <= 1.0 + step * 0.5 {
                    xs.extend([r * dwf + dxf, r * dwf + dxf, dxf, dwf + dxf]);
                    ys.extend([dyf, dyf + dhf, r * dhf + dyf, r * dhf + dyf]);
                    r += step;
                }
            }
            (xs, ys)
        };
        let run = |grid: bool| {
            let (mut xs, mut ys) = sample(grid);
            let mut ok = vec![false; xs.len()];
            self.approx.transform(true, &mut xs, &mut ys, &mut ok);
            let (mut x0, mut y0, mut x1, mut y1) = (f64::INFINITY, f64::INFINITY, f64::NEG_INFINITY, f64::NEG_INFINITY);
            let mut failed = 0;
            for i in 0..xs.len() {
                if !ok[i] || xs[i].is_nan() || ys[i].is_nan() {
                    failed += 1;
                    continue;
                }
                x0 = x0.min(xs[i]);
                y0 = y0.min(ys[i]);
                x1 = x1.max(xs[i]);
                y1 = y1.max(ys[i]);
            }
            (x0, y0, x1, y1, failed, xs.len())
        };
        let mut r = run(false);
        let grid = r.4 > 0;
        if grid {
            // GDAL keeps the bounds of the edge points (its ComputeSourceWindowTransformPoints()
            // only grows them), and counts the grid's failures.
            let g = run(true);
            r = (r.0.min(g.0), r.1.min(g.1), r.2.max(g.2), r.3.max(g.3), g.4, g.5);
        }
        let (mut x0, mut y0, mut x1, mut y1, failed, total) = r;
        if failed + 5 > total {
            return None;
        }
        let (sw, sh) = (self.src_w as f64, self.src_h as f64);
        if grid {
            // GDAL's ComputeSourceWindowStartingFromSource(): the points of a grid over the
            // source that land in the window extend it (where the reverse transformation fails).
            let (gx, gy, gok) = self.src_grid.get_or_init(|| {
                let steps = 21usize;
                let step = 1.0 / (steps - 1) as f64;
                let ratio = |i: usize, n: f64| {
                    if i == 0 {
                        0.5 / n
                    } else if i <= steps {
                        (i - 1) as f64 * step
                    } else {
                        1.0 - 0.5 / n
                    }
                };
                let (mut xs, mut ys) = (Vec::new(), Vec::new());
                for iy in 0..steps + 2 {
                    for ix in 0..steps + 2 {
                        xs.push(ratio(ix, sw) * sw);
                        ys.push(ratio(iy, sh) * sh);
                    }
                }
                let (src_x, src_y) = (xs.clone(), ys.clone());
                let mut ok = vec![false; xs.len()];
                self.approx.transform(false, &mut xs, &mut ys, &mut ok);
                let inside: Vec<bool> = (0..xs.len()).map(|i| ok[i]).collect();
                // Keep the destination positions, and the source ones to extend by.
                let dst: Vec<(f64, f64)> = xs.into_iter().zip(ys).collect();
                (dst, src_x.into_iter().zip(src_y).collect(), inside)
            });
            for i in 0..gx.len() {
                let (dx, dy) = gx[i];
                if gok[i] && dx >= dxf && dx <= dxf + dwf && dy >= dyf && dy <= dyf + dhf {
                    let (sx, sy) = gy[i];
                    x0 = x0.min(sx);
                    y0 = y0.min(sy);
                    x1 = x1.max(sx);
                    y1 = y1.max(sy);
                }
            }
        }
        if x0 > sw || x1 < 0.0 || y0 > sh || y1 < 0.0 {
            return Some((0, 0, 0, 0, 0.0, 0.0, 0.0));
        }
        let round_close = |v: f64| if (v.round() - v).abs() < 1e-6 { v.round() } else { v };
        (x0, y0, x1, y1) = (round_close(x0), round_close(y0), round_close(x1), round_close(y1));
        let res_win = if self.translation { 0 } else { self.resampling.radius() };
        let xscale = (dwf / (x1 - x0)).max(1e-3);
        let yscale = (dhf / (y1 - y0)).max(1e-3);
        let mut xr = if xscale < 0.95 { (res_win as f64 / xscale).ceil() as i64 } else { res_win as i64 };
        let mut yr = if yscale < 0.95 { (res_win as f64 / yscale).ceil() as i64 } else { res_win as i64 };
        if let Some(extra) = self.source_extra {
            xr += extra as i64;
            yr += extra as i64;
        } else if failed > 0 {
            xr += 10;
            yr += 10;
        }
        let (iw, ih) = (self.src_w as i64, self.src_h as i64);
        let minxc = x0.max(0.0) as i64;
        let minyc = y0.max(0.0) as i64;
        let maxxc = x1.ceil().min(sw) as i64;
        let maxyc = y1.ceil().min(sh) as i64;
        let (sx, sws) = if (maxxc - minxc) as f64 > 0.9 * sw {
            (0, iw)
        } else {
            let sx = (minxc - xr).clamp(0, iw);
            (sx, (iw - sx).min(maxxc - sx + xr).max(0))
        };
        let (sy, shs) = if (maxyc - minyc) as f64 > 0.9 * sh {
            (0, ih)
        } else {
            let sy = (minyc - yr).clamp(0, ih);
            (sy, (ih - sy).min(maxyc - sy + yr).max(0))
        };
        let fill = (sws * shs) as f64 / ((x1 - x0 + 2.0 * xr as f64) * (y1 - y0 + 2.0 * yr as f64)).max(1.0);
        let raw_w = (sw - minxc as f64).min(x1 - x0).max(0.0);
        let raw_h = (sh - minyc as f64).min(y1 - y0).max(0.0);
        Some((sx as usize, sy as usize, sws as usize, shs as usize, fill, raw_w, raw_h))
    }

    /// The kernel scales of a chunk, as GDAL 3.12's warp kernel computes them: the ratio of the
    /// destination window's size to the (unpadded) source window's.
    fn scales(&self, c: &Chunk) -> (f64, f64) {
        if self.kernel_version == KernelVersion::Gdal313 {
            return self.local_scales(c);
        }
        let (dw, dh) = (c.dw as f64, c.dh as f64);
        let (extra_x, extra_y) = (c.sw as f64 - c.raw_w, c.sh as f64 - c.raw_h);
        let mut sx = dw / (c.sw as f64 - extra_x);
        let mut sy = dh / (c.sh as f64 - extra_y);
        if c.sw >= c.dw && c.sw as f64 <= dw + extra_x {
            sx = 1.0;
        }
        if c.sh >= c.dh && c.sh as f64 <= dh + extra_y {
            sy = 1.0;
        }
        let fix = |s: f64| {
            if s < 1.0 {
                let r = 1.0 / s;
                let n = (r + 0.5) as i64;
                if (r - n as f64).abs() < 0.05 {
                    return 1.0 / n as f64;
                }
            }
            s
        };
        (sx, sy) = (fix(sx), fix(sy));
        if sy / sx > 100.0 {
            // Probably a source wrapping around the antimeridian: sample the local x scale.
            let npx = c.dw.min(10);
            let npy = c.dh.min(10);
            let mut scales = Vec::new();
            for iy in 0..npy {
                for ix in 0..npx {
                    let x = if npx == 1 { 0.0 } else { ix as f64 * dw / (npx - 1) as f64 };
                    let y = if npy == 1 { 0.0 } else { iy as f64 * dh / (npy - 1) as f64 };
                    let mut xs = [x, if ix == npx - 1 { x - 1.0 } else { x + 1.0 }, x];
                    let mut ys = [y, y, if iy == npy - 1 { y - 1.0 } else { y + 1.0 }];
                    let mut ok = [false; 3];
                    self.approx.transform(true, &mut xs, &mut ys, &mut ok);
                    if ok.iter().all(|b| *b) {
                        scales.push(1.0 / (xs[1] - xs[0]).abs().max((xs[2] - xs[0]).abs()));
                    }
                }
            }
            scales.sort_by(f64::total_cmp);
            if let Some(&max) = scales.last() {
                let kept: Vec<f64> = scales.into_iter().filter(|v| *v > max / 10.0).collect();
                if !kept.is_empty() {
                    sx = kept.iter().sum::<f64>() / kept.len() as f64;
                }
            }
        }
        (sx, sy)
    }

    /// GDAL 3.13 samples transformed unit squares on a 10 by 10 destination grid.
    fn local_scales(&self, c: &Chunk) -> (f64, f64) {
        let (nx, ny) = (c.dw.min(10), c.dh.min(10));
        let mut xs = Vec::with_capacity(nx * ny * 4);
        let mut ys = Vec::with_capacity(nx * ny * 4);
        let position = |i: usize, n: usize, size: usize, offset: usize| {
            offset as f64 + if n == 1 { 0.0 } else { i as f64 * size as f64 / (n - 1) as f64 - if i == n - 1 { 1.0 } else { 0.0 } }
        };
        for iy in 0..ny {
            let y = position(iy, ny, c.dh, c.dy);
            for ix in 0..nx {
                let x = position(ix, nx, c.dw, c.dx);
                xs.extend([x, x + 1.0, x, x + 1.0]);
                ys.extend([y, y, y + 1.0, y + 1.0]);
            }
        }
        let mut ok = vec![false; xs.len()];
        self.approx.transform(true, &mut xs, &mut ys, &mut ok);
        let mut values: Vec<(f64, f64)> = (0..xs.len())
            .step_by(4)
            .filter_map(|i| {
                if !ok[i..i + 4].iter().all(|v| *v) {
                    return None;
                }
                square_scales(std::array::from_fn(|k| (xs[i + k], ys[i + k])))
            })
            .collect();
        values.sort_by(|a, b| (a.0 * a.1).total_cmp(&(b.0 * b.1)));
        let max = values.iter().fold((0.0_f64, 0.0_f64), |a, b| (a.0.max(b.0), a.1.max(b.1)));
        let mut mean = (0.0, 0.0);
        let mut count = 0;
        for (x, y) in values {
            if x > 0.1 * max.0 && y > 0.1 * max.1 {
                count += 1;
                let inv = 1.0 / count as f64;
                mean.0 += (x - mean.0) * inv;
                mean.1 += (y - mean.1) * inv;
            }
        }
        let fix = |s: f64| {
            if s <= 0.0 {
                return 1.0;
            }
            if s < 1.0 {
                let r = 1.0 / s;
                let n = (r + 0.5) as i64;
                if (r - n as f64).abs() < 0.05 {
                    return 1.0 / n as f64;
                }
            }
            s
        };
        (fix(mean.0), fix(mean.1))
    }

    /// Warps a chunk, its output rows in parallel.
    fn warp_chunk<T: crate::pixel::Pixel>(&self, c: &Chunk, src: &SrcData<T>, sink: &Sink, rows: Vec<RowOut>) {
        let (xscale, yscale) = self.scales(c);
        let k = Kernel { src, win: *c, xscale, yscale, method: self.resampling };
        if self.resampling.is_area_based() {
            crate::par::for_each_owned(rows, |row, mut out| self.warp_area_row(c, &k, sink, row, &mut out));
            return;
        }
        crate::par::for_each_owned(rows, |row, mut out| self.warp_row(c, &k, sink, row, &mut out));
    }

    /// Warps one output row of a chunk.
    fn warp_row<T: crate::pixel::Pixel>(&self, c: &Chunk, k: &Kernel<T>, sink: &Sink, row: usize, out: &mut [&mut [f64]]) {
        let (xscale, yscale) = (k.xscale, k.yscale);
        // Include stored masks/cutlines as well as nodata, but skip the all-band
        // hot-loop check when this chunk has no validity planes.
        let global_validity = self.global_validity && k.src.valid.iter().any(Option::is_some);
        let four = matches!(self.resampling, Resampling::Bilinear | Resampling::Cubic)
            && match self.kernel_version {
                KernelVersion::Gdal312 => xscale >= 0.95 && yscale >= 0.95,
                KernelVersion::Gdal313 => xscale > 0.5 && yscale > 0.5,
            };
        let mut xs = vec![0.0; c.dw];
        let mut ys = vec![0.0; c.dw];
        let mut ok = vec![false; c.dw];
        let (sxo, syo, sw, sh) = (c.sx as f64, c.sy as f64, c.sw as f64, c.sh as f64);
        {
            let y = row as f64 + 0.5 + c.dy as f64;
            for (i, x) in xs.iter_mut().enumerate() {
                *x = i as f64 + 0.5 + c.dx as f64;
            }
            ys.iter_mut().for_each(|v| *v = y);
            self.approx.transform(true, &mut xs, &mut ys, &mut ok);
            for i in 0..c.dw {
                // GWKCheckAndComputeSrcOffsets(): points just outside the window are transformed
                // again exactly.
                let mut accepted = false;
                for attempt in 0..2 {
                    if attempt == 1 {
                        match self.t.point(true, i as f64 + 0.5 + c.dx as f64, y) {
                            Some((a, b)) => {
                                xs[i] = a;
                                ys[i] = b;
                            }
                            None => break,
                        }
                    } else if !ok[i] {
                        break;
                    }
                    let (px, py) = (xs[i], ys[i]);
                    if px.is_nan() || py.is_nan() {
                        break;
                    }
                    if px < sxo {
                        if attempt == 0 && px > sxo - 1.0 {
                            continue;
                        }
                        break;
                    }
                    if py < syo {
                        if attempt == 0 && py > syo - 1.0 {
                            continue;
                        }
                        break;
                    }
                    if px + 1e-10 > sw + sxo {
                        if attempt == 0 && px < sw + sxo + 1.0 {
                            continue;
                        }
                        break;
                    }
                    if py + 1e-10 > sh + syo {
                        if attempt == 0 && py < sh + syo + 1.0 {
                            continue;
                        }
                        break;
                    }
                    accepted = true;
                    break;
                }
                if !accepted {
                    continue;
                }
                let mut ix = (xs[i] + 1e-10) as usize - c.sx;
                let mut iy = (ys[i] + 1e-10) as usize - c.sy;
                if ix == c.sw {
                    ix -= 1;
                }
                if iy == c.sh {
                    iy -= 1;
                }
                if global_validity && (0..self.bands).all(|b| !k.valid(b, ix, iy)) {
                    continue;
                }
                let (fx, fy) = (xs[i] - sxo, ys[i] - syo);
                let mut found = false;
                for (b, o) in out.iter_mut().enumerate() {
                    let v = if self.resampling == Resampling::Nearest || c.sw == 1 || c.sh == 1 {
                        k.valid(b, ix, iy).then(|| k.value(b, ix, iy))
                    } else if four && self.resampling == Resampling::Bilinear {
                        k.bilinear(b, fx, fy)
                    } else if four {
                        k.cubic(b, fx, fy)
                    } else {
                        k.general(b, fx, fy)
                    };
                    if let Some(v) = v {
                        o[i] = sink.value(v, b);
                        found = true;
                    }
                }
                if found {
                    sink.avoid_all(out, i);
                }
            }
        }
    }

    /// The area-based methods (average, mode, min, max, quantiles, RMS, sum).
    fn warp_area_row<T: crate::pixel::Pixel>(&self, c: &Chunk, k: &Kernel<T>, sink: &Sink, row: usize, out: &mut [&mut [f64]]) {
        let xmargin = 2.0 * (1.0f64).max((1.0 / k.xscale).ceil());
        let ymargin = 2.0 * (1.0f64).max((1.0 / k.yscale).ceil());
        let n = c.dw;
        let (mut x1, mut y1, mut o1) = (vec![0.0; n], vec![0.0; n], vec![false; n]);
        let (mut x2, mut y2, mut o2) = (vec![0.0; n], vec![0.0; n], vec![false; n]);
        let (sxo, syo, sw, sh) = (c.sx as f64, c.sy as f64, c.sw as f64, c.sh as f64);
        let mut values: Vec<(f64, f64)> = Vec::new();
        {
            for i in 0..n {
                x1[i] = (i + c.dx) as f64;
                y1[i] = (row + c.dy) as f64;
                x2[i] = (i + c.dx) as f64 + 1.0;
                y2[i] = (row + c.dy) as f64 + 1.0;
            }
            self.approx.transform(true, &mut x1, &mut y1, &mut o1);
            self.approx.transform(true, &mut x2, &mut y2, &mut o2);
            for i in 0..n {
                let (mut ax, mut bx, mut ay, mut by) = (x1[i], x2[i], y1[i], y2[i]);
                if !(ax - sxo >= -xmargin
                    && bx - sxo >= -xmargin
                    && ay - syo >= -ymargin
                    && by - syo >= -ymargin
                    && ax - sxo - sw <= xmargin
                    && bx - sxo - sw <= xmargin
                    && ay - syo - sh <= ymargin
                    && by - syo - sh <= ymargin)
                {
                    continue;
                }
                if ax > bx {
                    std::mem::swap(&mut ax, &mut bx);
                }
                if ay > by {
                    std::mem::swap(&mut ay, &mut by);
                }
                let (xmin, xmax, ymin, ymax) = (ax - sxo, bx - sxo, ay - syo, by - syo);
                const EPS: f64 = 1e-10;
                if !(xmax > -EPS && xmin < sw + EPS) || !(ymax > -EPS && ymin < sh + EPS) {
                    continue;
                }
                let ix0 = (xmin + EPS).floor().max(0.0) as usize;
                let mut ix1 = ((xmax - EPS).ceil().max(0.0) as usize).min(c.sw);
                if ix0 == ix1 && ix1 < c.sw {
                    ix1 += 1;
                }
                let iy0 = (ymin + EPS).floor().max(0.0) as usize;
                let mut iy1 = ((ymax - EPS).ceil().max(0.0) as usize).min(c.sh);
                if iy0 == iy1 && iy1 < c.sh {
                    iy1 += 1;
                }
                let wy = |y: usize| {
                    if y == iy0 {
                        if iy0 + 1 == iy1 { 1.0 } else { 1.0 - (ymin - iy0 as f64) }
                    } else if y + 1 == iy1 {
                        1.0 - (iy1 as f64 - ymax)
                    } else {
                        1.0
                    }
                };
                let wx = |x: usize, w: f64| {
                    if x == ix0 {
                        if ix0 + 1 == ix1 { w } else { w * (1.0 - (xmin - ix0 as f64)) }
                    } else if x + 1 == ix1 {
                        w * (1.0 - (ix1 as f64 - xmax))
                    } else {
                        w
                    }
                };
                let mut found = false;
                for (b, o) in out.iter_mut().enumerate() {
                    values.clear();
                    for y in iy0..iy1 {
                        let w = wy(y);
                        for x in ix0..ix1 {
                            if k.valid(b, x, y) {
                                values.push((k.value(b, x, y), wx(x, w)));
                            }
                        }
                    }
                    if let Some(v) = area_value(
                        self.resampling,
                        &mut values,
                        matches!(sink.out_type, DataType::U8 | DataType::I8 | DataType::U16 | DataType::I16),
                    ) {
                        o[i] = sink.value(v, b);
                        found = true;
                    }
                }
                if found {
                    sink.avoid_all(out, i);
                }
            }
        }
    }
}

/// Combines the `(value, weight)` pairs of the source pixels a destination pixel covers.
pub(crate) fn area_value(method: Resampling, values: &mut [(f64, f64)], small_int: bool) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    match method {
        Resampling::Average => {
            let (mut total, mut mean) = (0.0, 0.0);
            for &(v, w) in values.iter() {
                if w > 0.0 {
                    total += w;
                    mean += (w / total) * (v - mean);
                }
            }
            (total > 0.0).then_some(mean)
        }
        Resampling::Rms => {
            let (mut total, mut sum) = (0.0, 0.0);
            for &(v, w) in values.iter() {
                total += w;
                sum += v * v * w;
            }
            (total > 0.0).then(|| (sum / total).sqrt())
        }
        Resampling::Sum => Some(values.iter().map(|(v, w)| v * w).sum()),
        Resampling::Min => values.iter().map(|v| v.0).reduce(f64::min),
        Resampling::Max => values.iter().map(|v| v.0).reduce(f64::max),
        Resampling::Mode => {
            // The value with the largest total weight; ties keep the first one met. For 8- and
            // 16-bit types GDAL counts in bins and any value can take the lead; for the other
            // types a value only takes the lead when its count grows past the leader's.
            let mut counts: Vec<(f64, f32)> = Vec::new();
            let mut best: Option<usize> = None;
            for &(v, w) in values.iter() {
                let v = if small_int { v.trunc() } else { v };
                let (i, inserted) = match counts.iter().position(|(c, _)| *c == v || (c.is_nan() && v.is_nan())) {
                    Some(i) => {
                        counts[i].1 += w as f32;
                        (i, false)
                    }
                    None => {
                        counts.push((v, w as f32));
                        (counts.len() - 1, true)
                    }
                };
                match best {
                    None => best = Some(i),
                    Some(b) if (small_int || !inserted) && counts[i].1 > counts[b].1 => best = Some(i),
                    _ => {}
                }
            }
            best.map(|b| counts[b].0)
        }
        Resampling::Med | Resampling::Q1 | Resampling::Q3 => {
            let q = match method {
                Resampling::Med => 0.5f32,
                Resampling::Q1 => 0.25,
                _ => 0.75,
            };
            values.sort_by(|a, b| a.0.total_cmp(&b.0));
            let idx = (q * values.len() as f32 - 1.0).ceil().max(0.0) as usize;
            Some(values[idx.min(values.len() - 1)].0)
        }
        _ => None,
    }
}

/// The interpolation kernels, on a chunk's source window.
struct Kernel<'a, T> {
    src: &'a SrcData<'a, T>,
    win: Chunk,
    xscale: f64,
    yscale: f64,
    method: Resampling,
}

impl<T: crate::pixel::Pixel> Kernel<'_, T> {
    #[inline]
    fn index(&self, x: usize, y: usize) -> usize {
        (self.win.sy + y - self.src.origin.1) * self.src.width + self.win.sx + x - self.src.origin.0
    }

    #[inline]
    fn value(&self, b: usize, x: usize, y: usize) -> f64 {
        crate::pixel::Pixel::to_f64(self.src.bands[b][self.index(x, y)])
    }

    #[inline]
    fn valid(&self, b: usize, x: usize, y: usize) -> bool {
        self.src.valid[b].as_ref().is_none_or(|m| m[self.index(x, y)])
    }

    /// `GWKBilinearResample4Sample()`: invalid or off-window neighbours are left out and the
    /// weights renormalized.
    fn bilinear(&self, b: usize, sx: f64, sy: f64) -> Option<f64> {
        let (w, h) = (self.win.sw as i64, self.win.sh as i64);
        let mut ix = (sx - 0.5).floor() as i64;
        let mut iy = (sy - 0.5).floor() as i64;
        let mut rx = 1.5 - (sx - ix as f64);
        let mut ry = 1.5 - (sy - iy as f64);
        if ix == -1 {
            ix = 0;
            rx = 1.0;
        }
        if iy == -1 {
            iy = 0;
            ry = 1.0;
        }
        let (mut acc, mut div) = (0.0, 0.0);
        for (dx, dy, m) in [(0, 0, rx * ry), (1, 0, (1.0 - rx) * ry), (0, 1, rx * (1.0 - ry)), (1, 1, (1.0 - rx) * (1.0 - ry))] {
            let (x, y) = (ix + dx, iy + dy);
            if x >= 0 && x < w && y >= 0 && y < h && self.valid(b, x as usize, y as usize) {
                div += m;
                acc += self.value(b, x as usize, y as usize) * m;
            }
        }
        if div == 1.0 {
            Some(acc)
        } else if div < 0.00001 {
            None
        } else {
            Some(acc / div)
        }
    }

    /// `GWKCubicResample4Sample()`: falls back to bilinear at the edges and when a neighbour is
    /// invalid.
    fn cubic(&self, b: usize, sx: f64, sy: f64) -> Option<f64> {
        let ix = (sx - 0.5) as i64;
        let iy = (sy - 0.5) as i64;
        if ix - 1 < 0 || ix + 2 >= self.win.sw as i64 || iy - 1 < 0 || iy + 2 >= self.win.sh as i64 {
            return self.bilinear(b, sx, sy);
        }
        let weights = |x: f64| {
            let half = 0.5 * x;
            let three = 3.0 * x;
            let half2 = half * x;
            [half * (-1.0 + x * (2.0 - x)), 1.0 + half2 * (-5.0 + three), half * (1.0 + x * (4.0 - three)), half2 * (-1.0 + x)]
        };
        let cx = weights(sx - 0.5 - ix as f64);
        let mut rows = [0.0; 4];
        for (j, r) in rows.iter_mut().enumerate() {
            let y = (iy - 1 + j as i64) as usize;
            let mut acc = 0.0;
            for (i, w) in cx.iter().enumerate() {
                let x = (ix - 1 + i as i64) as usize;
                if !self.valid(b, x, y) {
                    return self.bilinear(b, sx, sy);
                }
                acc += w * self.value(b, x, y);
            }
            *r = acc;
        }
        let cy = weights(sy - 0.5 - iy as f64);
        Some(cy[0] * rows[0] + cy[1] * rows[1] + cy[2] * rows[2] + cy[3] * rows[3])
    }

    /// `GWKResample()`: a separable kernel, stretched when downsampling, renormalized over the
    /// valid pixels.
    fn general(&self, b: usize, sx: f64, sy: f64) -> Option<f64> {
        let radius = self.method.radius();
        let xr = if self.xscale < 1.0 { (radius as f64 / self.xscale).ceil() as i64 } else { radius as i64 };
        let yr = if self.yscale < 1.0 { (radius as f64 / self.yscale).ceil() as i64 } else { radius as i64 };
        let init_x = ((radius + 1) % 2) as i64 - xr;
        let init_y = ((radius + 1) % 2) as i64 - yr;
        let ix = (sx - 0.5).floor() as i64;
        let iy = (sy - 0.5).floor() as i64;
        let dx = sx - 0.5 - ix as f64;
        let dy = sy - 0.5 - iy as f64;
        let (w, h) = (self.win.sw as i64, self.win.sh as i64);
        let (mut j0, mut j1) = (init_y, yr);
        if iy + j0 < 0 {
            j0 = -iy;
        }
        if iy + j1 >= h {
            j1 = h - iy - 1;
        }
        let (mut i0, mut i1) = (init_x, xr);
        if ix + i0 < 0 {
            i0 = -ix;
        }
        if ix + i1 >= w {
            i1 = w - ix - 1;
        }
        let masked = self.src.valid[b].is_some();
        if self.method == Resampling::Lanczos {
            // GWKResampleOptimizedLanczos(): only taps within the kernel's support, and no value
            // when fewer than half of them are valid.
            let (fx, fy) = (self.xscale.min(1.0), self.yscale.min(1.0));
            while (i0 as f64 - dx) * fx < -3.0 {
                i0 += 1;
            }
            while (i1 as f64 - dx) * fx > 3.0 {
                i1 -= 1;
            }
            while (j0 as f64 - dy) * fy < -3.0 {
                j0 += 1;
            }
            while (j1 as f64 - dy) * fy > 3.0 {
                j1 -= 1;
            }
            if masked {
                let mut count = 0;
                for j in j0..=j1 {
                    for i in i0..=i1 {
                        if self.valid(b, (ix + i) as usize, (iy + j) as usize) {
                            count += 1;
                        }
                    }
                }
                if count < (j1 - j0 + 1) * (i1 - i0 + 1) / 2 {
                    return None;
                }
            }
        }
        let wfn = |d: f64, s: f64| if s < 1.0 { self.method.weight(d * s) } else { self.method.weight(d) };
        let (mut acc, mut wsum) = (0.0, 0.0);
        for j in j0..=j1 {
            let wy = wfn(j as f64 - dy, self.yscale);
            let y = (iy + j) as usize;
            let (mut la, mut lw) = (0.0, 0.0);
            for i in i0..=i1 {
                let x = (ix + i) as usize;
                if !self.valid(b, x, y) {
                    continue;
                }
                let wx = wfn(i as f64 - dx, self.xscale);
                la += self.value(b, x, y) * wx;
                lw += wx;
            }
            acc += la * wy;
            wsum += lw * wy;
        }
        if wsum < 0.000001 {
            return None;
        }
        if !(0.99999..=1.00001).contains(&wsum) { Some(acc / wsum) } else { Some(acc) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wbraster::{Raster, RasterConfig};

    /// RPCs of a 120 x 100 image near (10.0 E, 45.0 N), whose columns move with the height.
    fn test_rpc() -> super::super::rpc::Rpc {
        let mut items: Vec<(String, String)> = [
            ("LINE_OFF", "50"),
            ("SAMP_OFF", "60"),
            ("LAT_OFF", "45"),
            ("LONG_OFF", "10"),
            ("HEIGHT_OFF", "500"),
            ("LINE_SCALE", "50"),
            ("SAMP_SCALE", "60"),
            ("LAT_SCALE", "0.05"),
            ("LONG_SCALE", "0.06"),
            ("HEIGHT_SCALE", "1000"),
            ("LINE_NUM_COEFF", "0 0.004 -1 0.05 0.002 0 0 0 -0.01 0 0 0 0 0 0 0 0 0 0 0"),
            ("SAMP_NUM_COEFF", "0 1 0.02 0.08 0.01 0 0 0.005 0 0 0 0 0 0 0 0 0 0 0 0"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        for k in ["LINE_DEN_COEFF", "SAMP_DEN_COEFF"] {
            items.push((k.into(), "1 0.001 -0.002 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0".into()));
        }
        super::super::rpc::Rpc::from_metadata(&items).unwrap()
    }

    /// Warped by RPCs (nearest), each output pixel holds the source pixel its centre maps to
    /// through the RPCs; a file with RPCs and no geotransform is warped by them as `gdalwarp`
    /// does; the RPC output window keeps pixels beyond the footprint empty.
    #[test]
    fn warps_by_rpcs() {
        let rpc = test_rpc();
        let mut src = raster(120, 100, [0.0, 1.0, 0.0, 100.0, 0.0, -1.0], 4326, |c, r| (c + 1000 * r) as f64 + 1.0);
        src.has_nodata = false;
        let ro = super::super::rpc::RpcOptions { height: Some(800.0), ..Default::default() };
        let o = WarpOptions {
            rpc: Some(rpc.clone()),
            rpc_options: ro.clone(),
            error_threshold: 0.0,
            dst_nodata: Nodata::Values(vec![0.0]),
            ..Default::default()
        };
        let out = warp(&[&src], &o).unwrap();
        let t = super::super::rpc::RpcTransformer::new(&rpc, &ro).unwrap();
        let gt = out.geo_transform();
        let (w, h) = (out.raster.cols, out.raster.rows);
        assert!((100..160).contains(&w) && (80..130).contains(&h), "{w} x {h}");
        let mut checked = 0;
        for r in (0..h).step_by(7) {
            for c in (0..w).step_by(5) {
                let (x, y) = apply_gt(&gt, c as f64 + 0.5, r as f64 + 0.5);
                let (p, l) = t.to_pixel(x, y, 0.0).unwrap();
                let v = out.raster.get(0, r as isize, c as isize);
                if p >= 0.0 && l >= 0.0 && p < 120.0 && l < 100.0 {
                    assert_eq!(v, (p.floor() + 1000.0 * l.floor()) + 1.0, "{c} {r}");
                    checked += 1;
                } else {
                    assert_eq!(v, 0.0);
                }
            }
        }
        assert!(checked > 100);

        // A GeoTIFF carrying the RPCs, without a geotransform.
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("rpc.tif");
        let wo = crate::RasterWriteOptions { rpc: rpc.to_tiff_tag(), ..Default::default() };
        crate::write_raster_dataset(&src, input.to_str().unwrap(), &wo).unwrap();
        let back = super::super::rpc::read_rpc(input.to_str().unwrap()).unwrap().unwrap();
        assert_eq!(back.samp_num_coeff, rpc.samp_num_coeff);
        let file_o = WarpOptions { rpc: None, ..o.clone() };
        let path = dir.path().join("out.tif");
        warp_to_path(&[input.to_str().unwrap().to_string()], &file_o, path.to_str().unwrap(), &Default::default()).unwrap();
        let written = crate::open_raster(path.to_str().unwrap()).unwrap();
        assert_eq!(written.raster.data.to_f64_vec(), out.raster.data.to_f64_vec());

        // A larger output: only the window around the footprint is warped with the RPC window.
        let big = WarpOptions { extent: Some((9.8, 44.8, 10.2, 45.2)), resolution: Some((0.001, 0.001)), ..o.clone() };
        let all = warp(&[&src], &big).unwrap();
        let windowed = warp(&[&src], &WarpOptions { rpc_output_window: true, ..big }).unwrap();
        let (a, b) = (all.raster.data.to_f64_vec(), windowed.raster.data.to_f64_vec());
        assert!(b.iter().filter(|v| **v != 0.0).count() > 1000);
        assert!(a.iter().zip(&b).all(|(x, y)| y == x || *y == 0.0));
    }

    #[test]
    fn gdal313_scales_follow_local_geometry() {
        assert_eq!(square_scales([(0.0, 0.0), (2.0, 0.0), (0.0, 4.0), (2.0, 4.0)]), Some((0.5, 0.25)));
        let rotated = square_scales([(0.0, 0.0), (2.0, 2.0), (-2.0, 2.0), (0.0, 4.0)]).unwrap();
        assert!((rotated.0 - 1.0 / 8.0_f64.sqrt()).abs() < 1e-15);
        assert_eq!(rotated.0, rotated.1);
        assert!(square_scales([(0.0, 0.0); 4]).is_none());
    }

    #[test]
    fn strict_warp_keeps_projection_domain_failures_as_nodata() {
        let source = raster(36, 36, [-20_000_000.0, 40_000_000.0 / 36.0, 0.0, 20_000_000.0, 0.0, -40_000_000.0 / 36.0], 3857, |_, _| 7.0);
        let options = WarpOptions {
            dst_srs: Some(SpatialRef::parse("EPSG:4326").unwrap()),
            extent: Some((-10.0, 80.0, 10.0, 100.0)),
            size: Some((32, 32)),
            transform_options: TransformOptions { allow_ballpark: Some(false), only_best: true, ..Default::default() },
            error_threshold: 0.0,
            ..Default::default()
        };
        let out = warp(&[&source], &options).unwrap();
        let values = super::super::band_f64(&out.raster, 0);
        assert_eq!(values.iter().filter(|v| **v == 7.0).count(), 256);
        assert_eq!(values.iter().filter(|v| **v != 7.0).count(), 768);
    }

    #[test]
    fn point_metadata_survives_memory_and_lazy_warp() {
        let mut source = raster(8, 8, [10.0, 0.01, 0.0, 47.0, 0.0, -0.01], 4326, |x, y| (x + y) as f64);
        source.raster.metadata.push(("AREA_OR_POINT".into(), "Point".into()));
        source.raster.metadata.push(("STATISTICS_MINIMUM".into(), "-999".into()));
        let options = WarpOptions { dst_srs: Some(SpatialRef::parse("EPSG:3857").unwrap()), ..Default::default() };
        let out = warp(&[&source], &options).unwrap();
        assert!(out.raster.metadata.iter().any(|(k, v)| k == "AREA_OR_POINT" && v == "Point"));
        assert!(!out.raster.metadata.iter().any(|(k, _)| k == "STATISTICS_MINIMUM"));
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.tif");
        let output = dir.path().join("out.tif");
        crate::write_raster_dataset(&source, input.to_str().unwrap(), &Default::default()).unwrap();
        let mut lazy = crate::open_raster_lazy(input.to_str().unwrap()).unwrap();
        assert!(lazy.header().raster.metadata.iter().any(|(k, _)| k == "STATISTICS_MINIMUM"));
        let window = lazy.read_window(crate::Window::new(1, 1, 4, 4), &Default::default()).unwrap();
        assert!(window.raster.metadata.iter().any(|(k, v)| k == "AREA_OR_POINT" && v == "Point"));
        assert!(!window.raster.metadata.iter().any(|(k, _)| k == "STATISTICS_MINIMUM"));
        warp_to_path(&[input.to_str().unwrap().into()], &options, output.to_str().unwrap(), &Default::default()).unwrap();
        let out = crate::open_raster(output.to_str().unwrap()).unwrap();
        assert!(out.raster.metadata.iter().any(|(k, v)| k == "AREA_OR_POINT" && v == "Point"));
        assert!(!out.raster.metadata.iter().any(|(k, _)| k == "STATISTICS_MINIMUM"));
    }

    fn raster(cols: usize, rows: usize, gt: [f64; 6], epsg: u32, f: impl Fn(usize, usize) -> f64) -> RasterDataset {
        let cfg = RasterConfig {
            cols,
            rows,
            bands: 1,
            x_min: gt[0],
            y_min: gt[3] + gt[5] * rows as f64,
            cell_size: gt[1],
            cell_size_y: Some(-gt[5]),
            nodata: -9999.0,
            data_type: DataType::F32,
            crs: wbraster::CrsInfo::from_epsg(epsg),
            metadata: vec![],
        };
        let data = (0..rows).flat_map(|r| (0..cols).map(move |c| (r, c))).map(|(r, c)| f(c, r)).collect();
        RasterDataset::new(Raster::from_data(cfg, data).unwrap(), false)
    }

    /// A source warped a chunk at a time, read by windows and written as its rows are done (as a
    /// GeoTIFF, and in another format), is the warp of the whole source.
    #[test]
    fn chunks_read_by_windows_give_the_whole_warp() {
        let dir = tempfile::tempdir().unwrap();
        let mut ds = raster(37, 29, [500000.0, 30.0, 0.0, 4000000.0, 0.0, -30.0], 32610, |c, r| ((c * 7 + r * 13) % 50) as f64 + 0.25);
        ds.has_nodata = true;
        ds.raster.set(0, 5, 6, -9999.0).unwrap();
        let input = dir.path().join("src.tif");
        crate::write_raster_dataset(&ds, input.to_str().unwrap(), &Default::default()).unwrap();
        let paths = [input.to_str().unwrap().to_string()];
        let utm11 = SpatialRef::parse("EPSG:32611").unwrap();
        for resampling in [Resampling::Nearest, Resampling::Bilinear, Resampling::Cubic, Resampling::Average] {
            for (memory_limit, ext) in [(64.0 * 1024.0 * 1024.0, "tif"), (2000.0, "tif"), (500.0, "tif"), (2000.0, "asc")] {
                let o = WarpOptions {
                    dst_srs: Some(utm11.clone()),
                    resampling,
                    memory_limit,
                    output_type: Some(DataType::F64),
                    ..Default::default()
                };
                let whole = dir.path().join(format!("whole.{ext}"));
                crate::write_raster_dataset(&warp(&[&ds], &o).unwrap(), whole.to_str().unwrap(), &Default::default()).unwrap();
                let out = dir.path().join(format!("out.{ext}"));
                warp_to_path(&paths, &o, out.to_str().unwrap(), &Default::default()).unwrap();
                let (a, b) = (crate::open_raster(whole.to_str().unwrap()).unwrap(), crate::open_raster(out.to_str().unwrap()).unwrap());
                assert_eq!((a.raster.cols, a.raster.rows, a.geo_transform()), (b.raster.cols, b.raster.rows, b.geo_transform()));
                assert_eq!(a.raster.data.to_f64_vec(), b.raster.data.to_f64_vec(), "{resampling:?} {memory_limit} {ext}");
            }
        }
    }

    #[test]
    fn lazy_warp_gives_the_whole_warp() {
        let dir = tempfile::tempdir().unwrap();
        let mut ds = raster(97, 83, [500000.0, 30.0, 0.0, 4000000.0, 0.0, -30.0], 32610, |c, r| ((c * 7 + r * 13) % 50) as f64 + 0.25);
        ds.has_nodata = true;
        ds.raster.set(0, 40, 30, -9999.0).unwrap();
        let input = dir.path().join("src.tif");
        crate::write_raster_dataset(&ds, input.to_str().unwrap(), &Default::default()).unwrap();
        let th = 30f64.to_radians();
        let rotated = [500900.0, 40.0 * th.cos(), 40.0 * th.sin(), 3998200.0, 40.0 * th.sin(), -40.0 * th.cos()];
        for resampling in [Resampling::Nearest, Resampling::Bilinear, Resampling::Cubic, Resampling::Average] {
            for (dst_transform, size, memory_limit) in [
                (None, None, 64.0 * 1024.0 * 1024.0),
                (None, None, 2000.0),
                (Some(rotated), Some((20, 16)), 64.0 * 1024.0 * 1024.0),
                (Some(rotated), Some((20, 16)), 500.0),
            ] {
                let o = WarpOptions {
                    dst_srs: Some(SpatialRef::parse("EPSG:32611").unwrap()),
                    dst_transform,
                    size,
                    resampling,
                    memory_limit,
                    ..Default::default()
                };
                let whole = warp(&[&ds], &o).unwrap();
                let mut lazy = crate::open_raster_lazy(input.to_str().unwrap()).unwrap();
                let out = warp_lazy(&mut lazy, &o).unwrap();
                assert_eq!(whole.geo_transform(), out.geo_transform());
                assert_eq!(whole.raster.data.to_f64_vec(), out.raster.data.to_f64_vec(), "{resampling:?} {memory_limit}");
            }
        }
        // Georeferenced by GCPs, with each of GDAL's transformers.
        let gcp = |pixel: f64, line: f64, x: f64, y: f64| crate::Gcp { pixel, line, x, y, ..Default::default() };
        let gcps = vec![
            gcp(0.0, 0.0, 500000.0, 4000000.0),
            gcp(97.0, 0.0, 502910.0, 4000050.0),
            gcp(0.0, 83.0, 499950.0, 3997510.0),
            gcp(97.0, 83.0, 502890.0, 3997480.0),
            gcp(48.0, 40.0, 501445.0, 3998790.0),
            gcp(20.0, 70.0, 500610.0, 3997905.0),
        ];
        for gcp_method in [super::super::gcp::GcpMethod::Polynomial, super::super::gcp::GcpMethod::Tps] {
            for memory_limit in [64.0 * 1024.0 * 1024.0, 2000.0] {
                let o =
                    WarpOptions { gcps: gcps.clone(), gcp_method, resampling: Resampling::Bilinear, memory_limit, ..Default::default() };
                let whole = warp(&[&ds], &o).unwrap();
                let mut lazy = crate::open_raster_lazy(input.to_str().unwrap()).unwrap();
                let out = warp_lazy(&mut lazy, &o).unwrap();
                assert_eq!(whole.geo_transform(), out.geo_transform());
                assert_eq!(whole.raster.data.to_f64_vec(), out.raster.data.to_f64_vec(), "{gcp_method:?} {memory_limit}");
            }
        }
    }

    #[test]
    fn rotated_output_grid() {
        // A 90° rotation of the source grid onto itself: output pixel (c, r) is source pixel
        // (r, c), read by nearest neighbour exactly.
        let ds = raster(6, 4, [0.0, 1.0, 0.0, 4.0, 0.0, -1.0], 32610, |c, r| (c * 10 + r) as f64);
        let o = WarpOptions { dst_transform: Some([0.0, 0.0, 1.0, 4.0, -1.0, 0.0]), size: Some((4, 6)), ..Default::default() };
        let out = warp(&[&ds], &o).unwrap();
        assert_eq!(out.geo_transform(), [0.0, 0.0, 1.0, 4.0, -1.0, 0.0]);
        assert_eq!((out.raster.cols, out.raster.rows), (4, 6));
        for r in 0..6 {
            for c in 0..4 {
                assert_eq!(out.raster.get(0, r as isize, c as isize), (r * 10 + c) as f64, "({c}, {r})");
            }
        }
        // A geotransform needs a size, and excludes an extent.
        assert!(warp(&[&ds], &WarpOptions { dst_transform: o.dst_transform, ..Default::default() }).is_err());
        assert!(warp(&[&ds], &WarpOptions { extent: Some((0.0, 0.0, 1.0, 1.0)), ..o.clone() }).is_err());
    }

    #[test]
    fn same_crs_identity() {
        let ds = raster(20, 10, [500000.0, 30.0, 0.0, 4000000.0, 0.0, -30.0], 32610, |c, r| (c * 100 + r) as f64);
        let out = warp(&[&ds], &WarpOptions::default()).unwrap();
        assert_eq!((out.raster.cols, out.raster.rows), (20, 10));
        assert_eq!(out.geo_transform(), ds.geo_transform());
        assert_eq!(out.raster.data.to_f64_vec(), ds.raster.data.to_f64_vec());
    }

    #[test]
    fn resample_by_two() {
        // Halving the resolution with average: each output pixel is the mean of 2×2 inputs.
        let ds = raster(8, 8, [0.0, 1.0, 0.0, 8.0, 0.0, -1.0], 32610, |c, r| (c + 8 * r) as f64);
        let o = WarpOptions { resolution: Some((2.0, 2.0)), resampling: Resampling::Average, ..Default::default() };
        let out = warp(&[&ds], &o).unwrap();
        assert_eq!((out.raster.cols, out.raster.rows), (4, 4));
        let v = out.raster.data.to_f64_vec();
        assert_eq!(v[0], (0.0 + 1.0 + 8.0 + 9.0) / 4.0);
        let o = WarpOptions { resolution: Some((2.0, 2.0)), resampling: Resampling::Nearest, ..Default::default() };
        let out = warp(&[&ds], &o).unwrap();
        // Output pixel centre (1, 1) falls on the corner between four inputs: GDAL takes (1, 1).
        assert_eq!(out.raster.data.to_f64_vec()[0], 9.0);
    }

    #[test]
    fn nodata_and_bilinear() {
        let ds = raster(4, 4, [0.0, 1.0, 0.0, 4.0, 0.0, -1.0], 32610, |c, _| if c == 0 { -9999.0 } else { c as f64 });
        let ds = RasterDataset { has_nodata: true, ..ds };
        let o =
            WarpOptions { extent: Some((0.5, 0.0, 3.5, 4.0)), size: Some((3, 4)), resampling: Resampling::Bilinear, ..Default::default() };
        let out = warp(&[&ds], &o).unwrap();
        let v = out.raster.data.to_f64_vec();
        // Centre x = 1.0 lies between column 0 (nodata) and column 1: only column 1 counts.
        assert_eq!(&v[..3], &[1.0, 1.5, 2.5]);
        assert_eq!(out.nodata(), Some(-9999.0));
        // Centre x = 0.75 lies in column 0, which is nodata: GDAL leaves the pixel nodata rather
        // than renormalising over column 1 (#307), with one band too and whatever
        // UNIFIED_SRC_NODATA says. Values from gdalwarp 3.12.2 and 3.13.1.
        for (resampling, unified) in [(Resampling::Bilinear, None), (Resampling::Bilinear, Some(false)), (Resampling::Cubic, None)] {
            let o = WarpOptions {
                extent: Some((0.25, 0.0, 3.25, 4.0)),
                size: Some((3, 4)),
                resampling,
                unified_src_nodata: unified,
                ..Default::default()
            };
            let v = warp(&[&ds], &o).unwrap().raster.data.to_f64_vec();
            assert_eq!(&v[..3], &[-9999.0, 1.25, 2.25], "{resampling:?}, unified {unified:?}");
        }
    }

    #[test]
    fn utm_to_geographic_grid() {
        let ds = raster(100, 100, [500000.0, 30.0, 0.0, 4000000.0, 0.0, -30.0], 32610, |c, r| (c + r) as f64);
        let o = WarpOptions { dst_srs: Some(SpatialRef::from_epsg(4326).unwrap()), ..Default::default() };
        let out = warp(&[&ds], &o).unwrap();
        let gt = out.geo_transform();
        assert!((gt[0] - -123.0).abs() < 0.01, "{gt:?}");
        assert!(out.raster.cols > 90 && out.raster.cols < 130, "{}", out.raster.cols);
        assert!((gt[1] + gt[5]).abs() < 1e-12, "square pixels");
    }

    #[test]
    fn approx_transformer_matches_exact_within_threshold() {
        let src = SpatialRef::from_epsg(32610).unwrap();
        let dst = SpatialRef::from_epsg(4326).unwrap();
        let mut t = GenImgProj::new([500000.0, 30.0, 0.0, 4000000.0, 0.0, -30.0], &src, &dst).unwrap();
        t.set_dst_gt([-123.0, 0.0003, 0.0, 36.2, 0.0, -0.0003]);
        let a = Approx { base: &t, max_error: 0.125 };
        let n = 1000;
        let mut xs: Vec<f64> = (0..n).map(|i| i as f64 + 0.5).collect();
        let mut ys = vec![10.5; n];
        let mut ok = vec![false; n];
        a.transform(true, &mut xs, &mut ys, &mut ok);
        for i in 0..n {
            let (ex, ey) = t.point(true, i as f64 + 0.5, 10.5).unwrap();
            assert!((xs[i] - ex).abs() + (ys[i] - ey).abs() <= 0.25, "{i}");
        }
    }

    #[test]
    fn rasterizes_pixel_centres() {
        let square = vec![vec![(1.0, 1.0), (3.0, 1.0), (3.0, 3.0), (1.0, 3.0), (1.0, 1.0)]];
        let m = rasterize_polygons(&[square], 4, 4);
        let on: Vec<usize> = m.iter().enumerate().filter(|(_, b)| **b).map(|(i, _)| i).collect();
        assert_eq!(on, vec![5, 6, 9, 10]);
    }

    /// Compare typed output storage with the previous Float64 buffer representation,
    /// retaining the same logical output type, resampling, nodata and chunk planning.
    #[test]
    fn typed_output_keeps_values() {
        let a = raster(60, 40, [500000.0, 30.0, 0.0, 4000000.0, 0.0, -30.0], 32610, |c, r| (c * 7 + r * 3) as f64 + 0.37);
        let b = raster(50, 30, [500600.0, 30.0, 0.0, 3999400.0, 0.0, -30.0], 32610, |c, r| {
            if (c + r) % 5 == 0 { -9999.0 } else { 1000.0 - (c * 3 + r) as f64 * 1.5 }
        });
        for (resampling, out_type) in
            [(Resampling::Bilinear, DataType::I16), (Resampling::Cubic, DataType::U8), (Resampling::Nearest, DataType::F32)]
        {
            for memory_limit in [1e9, 4096.0] {
                let options = WarpOptions {
                    dst_srs: Some(SpatialRef::from_epsg(4326).unwrap()),
                    resampling,
                    output_type: Some(out_type),
                    src_nodata: Nodata::Values(vec![-9999.0]),
                    memory_limit,
                    error_threshold: 0.0,
                    ..Default::default()
                };
                let sources = [&a, &b];
                let actual = warp(&sources, &options).unwrap();
                let plan = Plan::new(&sources, &options).unwrap();
                let reference = warp_data_with_storage(&plan, &sources, &options, DataType::F64).unwrap();
                assert_eq!(actual.raster.data_type, out_type);
                assert_eq!(actual.raster.data.data_type(), out_type);
                assert_eq!(reference.raster.data.data_type(), DataType::F64);
                assert_eq!((actual.raster.cols, actual.raster.rows), (reference.raster.cols, reference.raster.rows));
                assert_eq!(actual.geo_transform(), reference.geo_transform());
                assert_eq!(
                    super::super::band_f64(&actual.raster, 0),
                    super::super::band_f64(&reference.raster, 0),
                    "{resampling:?}, memory {memory_limit}"
                );
            }
        }
    }
}

#[cfg(test)]
mod overview_tests {
    use super::*;

    #[test]
    fn loaded_shared_sources_do_not_read_or_clone_windows() {
        use std::cell::RefCell;
        use std::rc::Rc;
        struct Shared(Rc<RefCell<crate::LazyRaster>>);
        impl WarpSource for Shared {
            fn with_dataset(&self, visit: &mut dyn FnMut(&RasterDataset)) -> Result<bool> {
                self.0.borrow().with_dataset(visit)
            }
            fn info(&mut self) -> Result<WarpSourceInfo> {
                panic!("loaded source must not request metadata")
            }
            fn read_window(&mut self, _: crate::Window, _: &crate::WindowOptions) -> Result<RasterDataset> {
                panic!("loaded source must not copy windows")
            }
        }
        let grid = Grid { width: 4, height: 4, geo_transform: [0., 1., 0., 4., 0., -1.] };
        let ds = super::super::build_raster(
            grid,
            vec![(0..16).map(|i| i as f64).collect()],
            DataType::F64,
            None,
            &SpatialRef::from_epsg(3857).unwrap(),
            super::super::convert_value,
        )
        .unwrap();
        let lazy = Rc::new(RefCell::new(crate::LazyRaster::from(ds.clone())));
        let mut a = Shared(lazy.clone());
        let mut b = Shared(lazy);
        for overview in [OverviewSelection::Auto, OverviewSelection::None, OverviewSelection::Level(2)] {
            let options = WarpOptions { overview, size: Some((2, 2)), ..Default::default() };
            let expected = warp(&[&ds, &ds], &options).unwrap();
            let got = warp_sources(&mut [&mut a, &mut b], &options).unwrap();
            assert_eq!(got.geo_transform(), expected.geo_transform());
            assert_eq!(super::super::band_f64(&got.raster, 0), super::super::band_f64(&expected.raster, 0));
        }
    }

    #[test]
    fn selects_stored_pixels_without_changing_the_target_grid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overviews.tif");
        let grid = Grid { width: 32, height: 32, geo_transform: [0., 1., 0., 32., 0., -1.] };
        let values = (0..1024).map(|i| (i / 32 * (i / 32) + i % 32) as f64).collect();
        let ds = super::super::build_raster(
            grid,
            vec![values],
            DataType::F32,
            None,
            &SpatialRef::from_epsg(3857).unwrap(),
            super::super::convert_value,
        )
        .unwrap();
        crate::write_raster_dataset(
            &ds,
            path.to_str().unwrap(),
            &crate::RasterWriteOptions { overviews: vec![2, 4, 8], overview_resampling: Some("average".into()), ..Default::default() },
        )
        .unwrap();
        let mut lazy = crate::open_raster_lazy(path.to_str().unwrap()).unwrap();
        let options = WarpOptions { size: Some((8, 8)), ..Default::default() };
        let full = warp_lazy(&mut lazy, &WarpOptions { overview: OverviewSelection::None, ..options.clone() }).unwrap();
        for (choice, level) in [(OverviewSelection::Auto, 1), (OverviewSelection::Level(1), 1), (OverviewSelection::AutoOffset(1), 0)] {
            let got = warp_lazy(&mut lazy, &WarpOptions { overview: choice, ..options.clone() }).unwrap();
            let (w, h) = lazy.overviews()[level];
            let selected =
                lazy.read_window(crate::Window::full(w, h), &crate::WindowOptions { overview: Some(level), bands: Vec::new() }).unwrap();
            let expected = warp(&[&selected], &options).unwrap();
            assert_eq!(got.geo_transform(), full.geo_transform());
            assert_eq!(super::super::band_f64(&got.raster, 0), super::super::band_f64(&expected.raster, 0));
            assert_ne!(super::super::band_f64(&got.raster, 0), super::super::band_f64(&full.raster, 0));
        }
        let paths = vec![path.to_str().unwrap().to_string(); 2];
        let many = warp_paths(&paths, &options).unwrap();
        let single = warp_lazy(&mut lazy, &options).unwrap();
        assert_eq!(super::super::band_f64(&many.raster, 0), super::super::band_f64(&single.raster, 0));
    }
}

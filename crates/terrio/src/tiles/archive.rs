//! Tile archives read and written (the `tiles` feature).

use super::*;
use source::ByteSource;

enum Inner {
    Pm(Box<pmtiles::Reader>),
    Mb(mbtiles::MbTiles),
}

/// Per-payload decoded-byte limits for PMTiles. `None` preserves unlimited decoding.
///
/// Limits apply before directory/JSON parsing and before returning decompressed tile bytes.
/// They do not bound parsed object overhead, codec working memory, image/MVT decoding,
/// cumulative cache memory, or compressed bytes fetched. Available with `tiles` on native
/// and WebAssembly targets; none, gzip and zstd use the same pure-Rust readers on both.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PmTilesReadLimits {
    /// Maximum decoded bytes in the root directory (checked during construction).
    pub root_directory: Option<usize>,
    /// Maximum decoded bytes in each leaf directory.
    pub leaf_directory: Option<usize>,
    /// Maximum decoded bytes in JSON metadata (checked during construction).
    pub metadata: Option<usize>,
    /// Maximum decoded bytes in each tile payload.
    pub tile: Option<usize>,
}

/// A PMTiles or MBTiles archive opened for reading tiles.
pub struct TileArchive {
    inner: Inner,
    /// Keeps a local copy (of an archive member) alive.
    _local: Option<crate::vfs::LocalPath>,
}

impl std::fmt::Debug for TileArchive {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TileArchive")
            .field("kind", &self.kind())
            .field("format", &self.format())
            .field("zooms", &self.zoom_range())
            .finish_non_exhaustive()
    }
}

/// The kinds of dataset an archive opened without a kind may hold: whether its tiles are images or
/// vectors is known only once it is opened.
const EITHER_KIND: [crate::formats::Kind; 2] = [crate::formats::Kind::Raster, crate::formats::Kind::Vector];

/// Refuses an archive `driver` (MBTiles or PMTiles) is about to open when the process-wide
/// allow-list of any of `kinds` leaves the driver out, as a de-registered GDAL driver opens nothing.
fn check_archive_driver(driver: &str, shown: &str, kinds: &[crate::formats::Kind]) -> Result<()> {
    kinds.iter().try_for_each(|&kind| crate::formats::check_driver_allowed(driver, shown, kind))
}

impl TileArchive {
    /// Opens an archive: a local path, or a URL, `s3://`, `gs://` or `az://` object. PMTiles are
    /// read in place (only the header and root directory now); MBTiles are read whole. Under a
    /// process-wide allow-list, the archive's driver (by its name, or its content for other names)
    /// must be allowed for both kinds of dataset, whose tiles it may hold.
    pub fn open(path: &str) -> Result<Self> {
        Self::open_judged(path, Some(&EITHER_KIND))
    }

    /// [`TileArchive::open`] for terrio's opens of a dataset, which judged the archive's name
    /// against the allow-list of their kind first: only an MBTiles archive found by its content is
    /// judged here.
    pub(crate) fn open_named(path: &str) -> Result<Self> {
        Self::open_judged(path, None)
    }

    /// Opens an archive, judging the driver it is opened with against the allow-lists of `kinds`
    /// (`None`: only a driver found by content, against both kinds').
    fn open_judged(path: &str, kinds: Option<&[crate::formats::Kind]>) -> Result<Self> {
        let kind = archive_kind(&crate::vfs::Location::parse(path).file_name()).or_else(|| archive_kind(path));
        match kind {
            Some(ArchiveKind::MBTiles) => {
                check_archive_driver("MBTiles", path, kinds.unwrap_or_default())?;
                let local = crate::vfs::resolve(path)?;
                let bytes = std::fs::read(local.path())?;
                Ok(Self { inner: Inner::Mb(mbtiles::MbTiles::from_bytes(bytes)?), _local: None })
            }
            _ => {
                let (mut source, local) = ByteSource::open(path)?;
                let head = source.read_at(0, source.len().min(16))?;
                if head.starts_with(b"SQLite format 3\0") {
                    drop(source);
                    // An MBTiles archive under another name, found by its content: read only if
                    // the MBTiles driver is allowed, by the list of each kind it serves.
                    check_archive_driver("MBTiles", path, kinds.unwrap_or(&EITHER_KIND))?;
                    let local = crate::vfs::resolve(path)?;
                    return Ok(Self { inner: Inner::Mb(mbtiles::MbTiles::from_bytes(std::fs::read(local.path())?)?), _local: None });
                }
                check_archive_driver("PMTiles", path, kinds.unwrap_or_default())?;
                Ok(Self { inner: Inner::Pm(Box::new(pmtiles::Reader::open(source)?)), _local: local })
            }
        }
    }

    /// Opens an archive held in memory (PMTiles or MBTiles, told apart by their content). Under a
    /// process-wide allow-list, its driver must be allowed for both kinds of dataset.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::from_bytes_judged(bytes, &EITHER_KIND)
    }

    /// [`TileArchive::from_bytes`], judging the driver against the allow-lists of `kinds` (none for
    /// terrio's opens of bytes, which judged them first, and for archives terrio made itself).
    pub(crate) fn from_bytes_judged(bytes: Vec<u8>, kinds: &[crate::formats::Kind]) -> Result<Self> {
        if bytes.starts_with(b"SQLite format 3\0") {
            check_archive_driver("MBTiles", "an archive in memory", kinds)?;
            return Ok(Self { inner: Inner::Mb(mbtiles::MbTiles::from_bytes(bytes)?), _local: None });
        }
        check_archive_driver("PMTiles", "an archive in memory", kinds)?;
        Ok(Self { inner: Inner::Pm(Box::new(pmtiles::Reader::open(ByteSource::Memory(bytes))?)), _local: None })
    }

    /// Opens a remote PMTiles archive whose bytes the caller fetches (see [`crate::sparse`]):
    /// reads fail until the bytes they need are fetched, and are then run again.
    pub fn from_sparse(file: crate::sparse::SparseFile) -> Result<Self> {
        Self::from_sparse_with_limits(file, PmTilesReadLimits::default())
    }

    /// Opens caller-fetched PMTiles with limits, including root and metadata decoding on open.
    ///
    /// Returns [`Error::DecodedLimitExceeded`] at the first decoded byte beyond a limit,
    /// without collecting the rest of the payload. Exactly-at-limit payloads succeed.
    /// Missing ranges retain [`crate::sparse`] retry/fill behavior; this does not fetch the
    /// whole archive. Brotli remains unsupported. Limits are retained for later leaf/tile reads.
    pub fn from_sparse_with_limits(file: crate::sparse::SparseFile, limits: PmTilesReadLimits) -> Result<Self> {
        check_archive_driver("PMTiles", "a remote archive", &EITHER_KIND)?;
        Ok(Self { inner: Inner::Pm(Box::new(pmtiles::Reader::open_with_limits(ByteSource::Sparse(file), limits)?)), _local: None })
    }

    /// The archive as a dataset: a raster for image tiles (at the zoom level of `options.tiles`),
    /// vector layers for vector tiles (those of `options.layers`, from the tiles that intersect
    /// `options.spatial_filter`, in EPSG:3857). `name` is the archive's file name.
    pub fn into_dataset(self, name: &str, options: &crate::VectorOpenOptions) -> Result<crate::Dataset> {
        archive_dataset(self, name, options)
    }

    /// PMTiles or MBTiles.
    pub fn kind(&self) -> ArchiveKind {
        match self.inner {
            Inner::Pm(_) => ArchiveKind::PMTiles,
            Inner::Mb(_) => ArchiveKind::MBTiles,
        }
    }

    /// Whether the archive is read in place from a remote file.
    pub fn is_remote(&self) -> bool {
        matches!(&self.inner, Inner::Pm(r) if r.is_remote())
    }

    /// Whether the archive is read from a remote file whose bytes the caller fetches.
    pub(crate) fn is_sparse(&self) -> bool {
        matches!(&self.inner, Inner::Pm(r) if r.is_sparse())
    }

    /// The tiles' format.
    pub fn format(&self) -> TileFormat {
        match &self.inner {
            Inner::Pm(r) => match r.header.tile_type {
                pmtiles::TILE_MVT => TileFormat::Mvt,
                pmtiles::TILE_PNG => TileFormat::Png,
                pmtiles::TILE_JPEG => TileFormat::Jpeg,
                pmtiles::TILE_WEBP => TileFormat::Webp,
                pmtiles::TILE_AVIF => TileFormat::Avif,
                _ => TileFormat::Unknown,
            },
            Inner::Mb(m) => match m.item("format").map(TileFormat::from_mbtiles) {
                Some(f) if f != TileFormat::Unknown => f,
                _ => m.zoom_range().and_then(|(_, z)| m.any_tile(z)).map(TileFormat::sniff).unwrap_or(TileFormat::Unknown),
            },
        }
    }

    /// Whether the tiles are vector tiles.
    pub fn is_vector(&self) -> bool {
        self.format() == TileFormat::Mvt
    }

    /// The lowest and highest zoom levels.
    pub fn zoom_range(&self) -> (u8, u8) {
        match &self.inner {
            Inner::Pm(r) => (r.header.min_zoom.min(30), r.header.max_zoom.min(30)),
            Inner::Mb(m) => m.zoom_range().unwrap_or((0, 0)),
        }
    }

    /// The bounds `(min_lon, min_lat, max_lon, max_lat)` the archive declares.
    pub fn bounds(&self) -> Option<[f64; 4]> {
        match &self.inner {
            Inner::Pm(r) => Some(r.header.bounds()),
            Inner::Mb(m) => {
                let v: Vec<f64> = m.item("bounds")?.split(',').filter_map(|s| s.trim().parse().ok()).collect();
                // GDAL ignores bounds it finds invalid.
                let ok = v.len() == 4
                    && v[0].abs() <= 180.0
                    && v[2].abs() <= 180.0
                    && v[1].abs() < 89.99
                    && v[3].abs() < 89.99
                    && v[0] <= v[2]
                    && v[1] <= v[3];
                ok.then(|| [v[0], v[1], v[2], v[3]])
            }
        }
    }

    /// The metadata as `(key, value)` pairs: MBTiles' `metadata` table, or the top-level items of
    /// PMTiles' JSON metadata (objects and arrays as JSON text).
    pub fn metadata(&self) -> Vec<(String, String)> {
        match &self.inner {
            Inner::Mb(m) => m.metadata.clone(),
            Inner::Pm(r) => match &r.metadata {
                serde_json::Value::Object(o) => o
                    .iter()
                    .map(|(k, v)| {
                        let s = match v {
                            serde_json::Value::String(s) => s.clone(),
                            v => v.to_string(),
                        };
                        (k.clone(), s)
                    })
                    .collect(),
                _ => Vec::new(),
            },
        }
    }

    /// The JSON metadata describing vector tiles: PMTiles' metadata, or MBTiles' `json` item.
    pub(crate) fn json(&self) -> serde_json::Value {
        match &self.inner {
            Inner::Pm(r) => {
                let mut v = r.metadata.clone();
                // Tippecanoe's archives nest vector_layers in a "json" string, as in MBTiles.
                if v.get("vector_layers").is_none()
                    && let Some(inner) = v.get("json").and_then(|j| j.as_str()).and_then(|s| serde_json::from_str(s).ok())
                {
                    v = inner;
                }
                v
            }
            Inner::Mb(m) => m.item("json").and_then(|s| serde_json::from_str(s).ok()).unwrap_or(serde_json::Value::Null),
        }
    }

    /// A tile's bytes (decompressed when the archive compresses tiles), `None` when the archive
    /// does not have it. `y` counts rows from the top, as in XYZ tile URLs.
    pub fn tile(&mut self, z: u8, x: u32, y: u32) -> Result<Option<Vec<u8>>> {
        match &mut self.inner {
            Inner::Pm(r) => r.tile(z, x, y),
            Inner::Mb(m) => Ok(m.tile(z, x, y).map(|b| b.to_vec())),
        }
    }

    /// Several tiles, fetched in as few requests as possible.
    pub(crate) fn tiles(&mut self, wanted: &[(u8, u32, u32)]) -> Result<Vec<Option<Vec<u8>>>> {
        match &mut self.inner {
            Inner::Pm(r) => r.tiles(wanted),
            Inner::Mb(m) => Ok(wanted.iter().map(|&(z, x, y)| m.tile(z, x, y).map(|b| b.to_vec())).collect()),
        }
    }

    /// The tiles `(x, y)` the archive has at zoom level `z`.
    pub fn tiles_at_zoom(&mut self, z: u8) -> Result<Vec<(u32, u32)>> {
        match &mut self.inner {
            Inner::Pm(r) => r.tiles_at_zoom(z),
            Inner::Mb(m) => Ok(m.tiles_at_zoom(z)),
        }
    }

    /// The driver name, as GDAL names it.
    pub fn driver_name(&self) -> &'static str {
        match self.kind() {
            ArchiveKind::PMTiles => "PMTiles",
            ArchiveKind::MBTiles => "MBTiles",
        }
    }
}

/// Whether a path (or URL) names a PMTiles or MBTiles archive.
pub fn is_archive(path: &str) -> bool {
    let loc = crate::vfs::Location::parse(path);
    !matches!(loc, crate::vfs::Location::Zip { .. }) && archive_kind(&loc.file_name()).is_some()
}

/// Opens a tile archive as a dataset: a raster for image tiles (at the zoom level of
/// `options.tiles`), vector layers for vector tiles (those of `options.layers`, from the tiles
/// that intersect `options.spatial_filter`, in EPSG:3857).
pub fn open_dataset(path: &str, options: &crate::VectorOpenOptions) -> Result<crate::Dataset> {
    let archive = TileArchive::open(path)?;
    let name = crate::vfs::Location::parse(path).file_name();
    archive_dataset(archive, &name, options)
}

/// [`open_dataset`] for terrio's opens of a dataset, which judged its name first (see
/// [`TileArchive::open_named`]).
pub(crate) fn open_named_dataset(path: &str, options: &crate::VectorOpenOptions) -> Result<crate::Dataset> {
    let archive = TileArchive::open_named(path)?;
    let name = crate::vfs::Location::parse(path).file_name();
    archive_dataset(archive, &name, options)
}

/// A dataset of an opened archive.
pub(crate) fn archive_dataset(mut archive: TileArchive, name: &str, options: &crate::VectorOpenOptions) -> Result<crate::Dataset> {
    if archive.is_vector() {
        let layers = vector::read_layers(&mut archive, &options.tiles, &options.layers, options.spatial_filter)?;
        let driver = match archive.kind() {
            ArchiveKind::PMTiles => crate::formats::VectorDriver::PMTiles,
            ArchiveKind::MBTiles => crate::formats::VectorDriver::MBTiles,
        };
        let names: Vec<String> = layers.iter().map(|l| l.name.clone()).collect();
        let mut ds = crate::dataset::finish_vector(layers, name, driver);
        // Every layer keeps the name the tiles give it.
        for (l, n) in ds.layers.iter_mut().zip(names) {
            l.name = n;
        }
        return Ok(crate::Dataset::Vector(ds));
    }
    let mut r = raster::TileRaster::new(archive, options.tiles.zoom_level)?;
    Ok(crate::Dataset::Raster(r.read_all()?))
}

/// Opens a tile archive held in memory as a dataset.
pub(crate) fn open_dataset_bytes(bytes: Vec<u8>, name: &str, options: &crate::VectorOpenOptions) -> Result<crate::Dataset> {
    archive_dataset(TileArchive::from_bytes_judged(bytes, &[])?, name, options)
}

// ─── Writing ────────────────────────────────────────────────────────────────────────────────────

/// Encodes a tile pyramid of image tiles as an archive, with the metadata GDAL writes.
pub(crate) fn write_raster_archive(rt: &raster::RasterTiles, o: &TileWriteOptions, name: &str, kind: ArchiveKind) -> Result<Vec<u8>> {
    write_raster_archive_with(rt, o, name, kind, &[])
}

/// [`write_raster_archive`], keeping the metadata items `kept` (an updated archive's) that it does
/// not write itself.
fn write_raster_archive_with(
    rt: &raster::RasterTiles,
    o: &TileWriteOptions,
    name: &str,
    kind: ArchiveKind,
    kept: &[(String, String)],
) -> Result<Vec<u8>> {
    let md = raster_metadata(rt.bounds, rt.min_zoom, rt.max_zoom, rt.format, o, name, kept);
    match kind {
        ArchiveKind::MBTiles => mbtiles::write(&md, rt.tiles.clone()),
        ArchiveKind::PMTiles => {
            let (json, params) = pmtiles_parts(md, rt.format, rt.min_zoom, rt.max_zoom, rt.bounds);
            let tiles = rt.tiles.iter().map(|(z, x, y, d)| pmtiles::OutTile { z: *z, x: *x, y: *y, data: d.clone() }).collect();
            pmtiles::write_archive(tiles, &json, &params)
        }
    }
}

/// The metadata items GDAL writes for a tile pyramid of image tiles, with the items `kept` (an
/// updated archive's) that it does not write itself.
fn raster_metadata(
    b: [f64; 4],
    min_zoom: u8,
    max_zoom: u8,
    format: TileFormat,
    o: &TileWriteOptions,
    name: &str,
    kept: &[(String, String)],
) -> Vec<(String, String)> {
    let name = o.name.clone().unwrap_or_else(|| name.to_string());
    let mut md: Vec<(String, String)> = vec![
        ("name".into(), name.clone()),
        ("type".into(), o.tileset_type.clone()),
        ("description".into(), o.description.clone().unwrap_or(name)),
        ("version".into(), "1.1".into()),
        ("format".into(), format.name().into()),
        ("bounds".into(), format!("{},{},{},{}", g17(b[0]), g17(b[1]), g17(b[2]), g17(b[3]))),
        ("minzoom".into(), min_zoom.to_string()),
        ("maxzoom".into(), max_zoom.to_string()),
    ];
    keep_items(&mut md, kept);
    md
}

/// The JSON metadata and header fields of a PMTiles archive of image tiles.
fn pmtiles_parts(
    md: Vec<(String, String)>,
    format: TileFormat,
    min_zoom: u8,
    max_zoom: u8,
    b: [f64; 4],
) -> (Vec<u8>, pmtiles::ArchiveParams) {
    let json: serde_json::Map<String, serde_json::Value> = md.into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
    let tile_type = match format {
        TileFormat::Jpeg => pmtiles::TILE_JPEG,
        TileFormat::Webp => pmtiles::TILE_WEBP,
        _ => pmtiles::TILE_PNG,
    };
    let params = pmtiles::ArchiveParams {
        tile_type,
        tile_compression: pmtiles::COMPRESSION_NONE,
        min_zoom,
        max_zoom,
        bounds: b,
        center: ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0, max_zoom),
    };
    (serde_json::Value::Object(json).to_string().into_bytes(), params)
}

/// Writes a raster's tile pyramid to `path` without holding its pixels or its tiles whole: the
/// pyramid is built a tile row at a time ([`raster::make_tiles_streamed`] for a raster read by
/// window, [`raster::make_tiles_from_dataset`] for one in memory), and the tiles of a PMTiles
/// archive are spilled to a temporary file beside `path`, then laid out in tile id order. The
/// archive is the one the in-memory writer writes, byte for byte. MBTiles archives gather their
/// (encoded) tiles in memory. The tile index is held whole, though: a `SpilledTile` (about 32
/// bytes) per tile, the directory entries and a map of the distinct tile contents, so memory grows
/// with the number of tiles, not with their bytes.
#[cfg(not(target_family = "wasm"))]
fn write_pyramid(
    path: &str,
    kind: ArchiveKind,
    o: &TileWriteOptions,
    build: impl FnOnce(&mut dyn super::pyramid::TileSink) -> Result<raster::Streamed>,
) -> Result<()> {
    let name = stem(path);
    match kind {
        ArchiveKind::MBTiles => {
            let mut tiles = Vec::new();
            let s = build(&mut tiles)?;
            let md = raster_metadata(s.bounds, s.min_zoom, s.max_zoom, s.format, o, &name, &[]);
            let bytes = mbtiles::write(&md, tiles)?;
            // Created before anything is written, as for PMTiles, so that a failure to write removes it.
            let mut out = std::fs::File::create(path)?;
            let written = std::io::Write::write_all(&mut out, &bytes);
            drop(out);
            if written.is_err() {
                let _ = std::fs::remove_file(path);
            }
            written?;
            Ok(())
        }
        ArchiveKind::PMTiles => {
            let mut spill = super::pyramid::SpillSink::beside(path)?;
            let s = build(&mut spill)?;
            let md = raster_metadata(s.bounds, s.min_zoom, s.max_zoom, s.format, o, &name, &[]);
            let (json, params) = pmtiles_parts(md, s.format, s.min_zoom, s.max_zoom, s.bounds);
            let tiles = std::mem::take(&mut spill.tiles);
            // Created before anything is written, so that a failure to create it removes nothing.
            let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
            let written = (|| -> Result<()> {
                pmtiles::write_archive_spilled(tiles, spill.file()?, &json, &params, &mut out)?;
                std::io::Write::flush(&mut out)?;
                Ok(())
            })();
            drop(out);
            if written.is_err() {
                let _ = std::fs::remove_file(path);
            }
            written
        }
    }
}

/// Writes a raster read by window as a PMTiles or MBTiles archive (see [`TileWriteOptions`]), as
/// [`write_raster`] writes the raster read whole, without holding it whole when it is already on
/// its zoom level's pixel grid (see [`raster::make_tiles_streamed`]).
#[cfg(not(target_family = "wasm"))]
pub(crate) fn write_raster_lazy(
    src: &mut crate::LazyRaster,
    path: &str,
    driver: crate::formats::RasterDriver,
    options: &crate::RasterWriteOptions,
) -> Result<()> {
    let kind = if driver == crate::formats::RasterDriver::MBTiles { ArchiveKind::MBTiles } else { ArchiveKind::PMTiles };
    let o = raster_write_options(options)?;
    write_pyramid(path, kind, &o, |sink| raster::make_tiles_streamed(src, &o, sink))
}

/// Formats a number as C's `%.17g` does (enough digits to round-trip).
fn g17(v: f64) -> String {
    format!("{v}")
}

/// Encodes vector tiles as an archive, with the metadata GDAL writes: MBTiles' items and `json`
/// (`vector_layers`, `tilestats`); for PMTiles, the same items as JSON members.
pub(crate) fn write_vector_archive(vt: &vector::VectorTiles, o: &TileWriteOptions, name: &str, kind: ArchiveKind) -> Result<Vec<u8>> {
    write_vector_archive_with(vt, o, name, kind, &[])
}

/// [`write_vector_archive`], keeping the metadata items `kept` (an updated archive's) that it does
/// not write itself.
fn write_vector_archive_with(
    vt: &vector::VectorTiles,
    o: &TileWriteOptions,
    name: &str,
    kind: ArchiveKind,
    kept: &[(String, String)],
) -> Result<Vec<u8>> {
    let mut md = vector::metadata_items(vt, o, name);
    keep_items(&mut md, kept);
    let gz = |d: &[u8]| pmtiles::compress(d, pmtiles::COMPRESSION_GZIP);
    match kind {
        ArchiveKind::MBTiles => {
            md.push(("scheme".into(), "tms".into()));
            md.push(("json".into(), serde_json::to_string_pretty(&vt.json).unwrap_or_default()));
            let mut tiles = Vec::with_capacity(vt.tiles.len());
            for (z, x, y, d) in &vt.tiles {
                tiles.push((*z, *x, *y, gz(d)?));
            }
            mbtiles::write(&md, tiles)
        }
        ArchiveKind::PMTiles => {
            let mut json: serde_json::Map<String, serde_json::Value> =
                md.into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
            if let serde_json::Value::Object(o) = &vt.json {
                for (k, v) in o {
                    json.insert(k.clone(), v.clone());
                }
            }
            let mut tiles = Vec::with_capacity(vt.tiles.len());
            for (z, x, y, d) in &vt.tiles {
                tiles.push(pmtiles::OutTile { z: *z, x: *x, y: *y, data: gz(d)? });
            }
            let b = vt.bounds;
            let params = pmtiles::ArchiveParams {
                tile_type: pmtiles::TILE_MVT,
                tile_compression: pmtiles::COMPRESSION_GZIP,
                min_zoom: vt.min_zoom,
                max_zoom: vt.max_zoom,
                bounds: b,
                center: ((b[0] + b[2]) / 2.0, (b[1] + b[3]) / 2.0, vt.min_zoom),
            };
            pmtiles::write_archive(tiles, serde_json::Value::Object(json).to_string().as_bytes(), &params)
        }
    }
}

/// Adds the items of `kept` that `md` lacks (but those the writers derive from the tiles).
fn keep_items(md: &mut Vec<(String, String)>, kept: &[(String, String)]) {
    const DERIVED: [&str; 4] = ["json", "scheme", "vector_layers", "tilestats"];
    for (k, v) in kept {
        if !DERIVED.contains(&k.as_str()) && !md.iter().any(|(m, _)| m == k) {
            md.push((k.clone(), v.clone()));
        }
    }
}

/// The options to rewrite an archive with: its own name, description and type, unless given.
fn options_of(archive: &TileArchive, o: &TileWriteOptions) -> (TileWriteOptions, Vec<(String, String)>) {
    let items = archive.metadata();
    let item = |k: &str| items.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let mut o = o.clone();
    o.name = o.name.or_else(|| item("name"));
    o.description = o.description.or_else(|| item("description"));
    if o.tileset_type == TileWriteOptions::default().tileset_type
        && let Some(t) = item("type")
    {
        o.tileset_type = t;
    }
    (o, items)
}

/// Adds a raster to an archive of image tiles held in memory, as `gdalwarp` into an existing
/// MBTiles file followed by `gdaladdo` does: the raster is warped onto the grid of the archive's
/// highest zoom level and laid over its tiles (where it is not transparent), the extent grows to
/// hold it, and the lower zoom levels are computed again. The tiles keep their format (PNG, JPEG
/// or WebP); the archive keeps its metadata.
pub fn update_raster_bytes(bytes: Vec<u8>, ds: &crate::RasterDataset, o: &TileWriteOptions) -> Result<Vec<u8>> {
    let archive = TileArchive::from_bytes_judged(bytes, &[crate::formats::Kind::Raster])?;
    let kind = archive.kind();
    let (o, items) = options_of(&archive, o);
    let rt = raster::update_tiles(archive, ds, &o)?;
    write_raster_archive_with(&rt, &o, "tiles", kind, &items)
}

/// Adds vector layers to an archive of vector tiles held in memory: each layer's features are
/// cut into the archive's zoom levels and added to the tiles' layer of the same name, or with
/// `overwrite` replace that layer in every tile; other layers are kept, and the metadata
/// (`vector_layers`, `tilestats`, bounds, zoom levels) is merged. GDAL's MBTiles and PMTiles
/// drivers cannot update vector tiles: the result is what writing all the layers at once gives.
pub fn update_vector_bytes(bytes: Vec<u8>, layers: &[wbvector::Layer], o: &TileWriteOptions, overwrite: bool) -> Result<Vec<u8>> {
    let mut archive = TileArchive::from_bytes_judged(bytes, &[crate::formats::Kind::Vector])?;
    let kind = archive.kind();
    let (o, items) = options_of(&archive, o);
    let vt = vector::update_tiles(&mut archive, layers, &o, overwrite)?;
    write_vector_archive_with(&vt, &o, "tiles", kind, &items)
}

/// Writes bytes over a file, through a temporary file renamed over it once complete.
#[cfg(not(target_family = "wasm"))]
fn replace_file(path: &str, bytes: &[u8]) -> Result<()> {
    let tmp = format!("{path}.terrio-update");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

/// Adds a raster to a local PMTiles or MBTiles archive of image tiles: see
/// [`update_raster_bytes`]. The archive is rewritten, and replaced once complete.
#[cfg(not(target_family = "wasm"))]
pub fn update_raster(path: &str, ds: &crate::RasterDataset, o: &TileWriteOptions) -> Result<()> {
    let bytes = update_raster_bytes(std::fs::read(path)?, ds, o)?;
    replace_file(path, &bytes)
}

/// Adds vector layers to a local PMTiles or MBTiles archive of vector tiles: see
/// [`update_vector_bytes`]. The archive is rewritten, and replaced once complete.
#[cfg(not(target_family = "wasm"))]
pub fn update_vector(path: &str, layers: &[wbvector::Layer], o: &TileWriteOptions, overwrite: bool) -> Result<()> {
    let bytes = update_vector_bytes(std::fs::read(path)?, layers, o, overwrite)?;
    replace_file(path, &bytes)
}

/// Writes a raster as a PMTiles or MBTiles archive (see [`TileWriteOptions`]).
pub(crate) fn raster_to_bytes(ds: &crate::RasterDataset, o: &TileWriteOptions, name: &str, kind: ArchiveKind) -> Result<Vec<u8>> {
    let rt = raster::make_tiles(ds, o)?;
    write_raster_archive(&rt, o, name, kind)
}

/// Writes vector layers as a PMTiles or MBTiles archive of vector tiles.
pub(crate) fn vector_to_bytes(layers: &[wbvector::Layer], o: &TileWriteOptions, name: &str, kind: ArchiveKind) -> Result<Vec<u8>> {
    let vt = vector::make_tiles(layers, o)?;
    write_vector_archive(&vt, o, name, kind)
}

/// Writes vector layers as a directory of MVT tiles (`z/x/y.pbf`, gzipped) with a
/// `metadata.json`, as GDAL's MVT driver does.
#[cfg(not(target_family = "wasm"))]
pub(crate) fn write_mvt_dir(layers: &[wbvector::Layer], o: &TileWriteOptions, path: &std::path::Path) -> Result<()> {
    let vt = vector::make_tiles(layers, o)?;
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("tiles");
    if path.exists() {
        return Err(Error::InvalidArgument(format!("'{}' already exists", path.display())));
    }
    std::fs::create_dir_all(path)?;
    let mut md: serde_json::Map<String, serde_json::Value> =
        vector::metadata_items(&vt, o, name).into_iter().map(|(k, v)| (k, serde_json::Value::String(v))).collect();
    md.insert("json".into(), serde_json::Value::String(serde_json::to_string_pretty(&vt.json).unwrap_or_default()));
    std::fs::write(path.join("metadata.json"), serde_json::to_string_pretty(&serde_json::Value::Object(md)).unwrap_or_default())?;
    for (z, x, y, d) in &vt.tiles {
        let dir = path.join(z.to_string()).join(x.to_string());
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join(format!("{y}.pbf")), pmtiles::compress(d, pmtiles::COMPRESSION_GZIP)?)?;
    }
    Ok(())
}

/// Reads a directory of MVT tiles (`z/x/y.pbf` with a `metadata.json`), as GDAL's MVT driver
/// does: the tiles of one zoom level (the highest by default).
#[cfg(not(target_family = "wasm"))]
pub(crate) fn read_mvt_dir(path: &std::path::Path, options: &crate::VectorOpenOptions) -> Result<Vec<wbvector::Layer>> {
    let text = std::fs::read_to_string(path.join("metadata.json"))?;
    let mut md: serde_json::Value = serde_json::from_str(&text).map_err(|e| Error::InvalidData(format!("metadata.json: {e}")))?;
    let zooms: Vec<u8> = std::fs::read_dir(path)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().and_then(|s| s.parse().ok()))
        .collect();
    let (min_z, max_z) = (zooms.iter().copied().min().unwrap_or(0), zooms.iter().copied().max().unwrap_or(0));
    let mut tiles = Vec::new();
    let z = options.tiles.zoom_level.unwrap_or(max_z);
    let zdir = path.join(z.to_string());
    if zdir.is_dir() {
        for xe in std::fs::read_dir(&zdir)?.filter_map(|e| e.ok()) {
            let Some(x) = xe.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
            for ye in std::fs::read_dir(xe.path())?.filter_map(|e| e.ok()) {
                let name = ye.file_name();
                let Some(y) = name.to_str().and_then(|s| s.split('.').next()).and_then(|s| s.parse::<u32>().ok()) else { continue };
                tiles.push((z, x, y, std::fs::read(ye.path())?));
            }
        }
    }
    // An MBTiles file in memory gives the same reading path as archives.
    let mut items: Vec<(String, String)> = Vec::new();
    if let serde_json::Value::Object(o) = &mut md {
        for (k, v) in o.iter() {
            items.push((k.clone(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string())));
        }
    }
    items.retain(|(k, _)| k != "minzoom" && k != "maxzoom");
    items.push(("minzoom".into(), min_z.to_string()));
    items.push(("maxzoom".into(), max_z.to_string()));
    let mut archive = TileArchive::from_bytes_judged(mbtiles::write(&items, tiles)?, &[])?;
    vector::read_layers(&mut archive, &options.tiles, &options.layers, options.spatial_filter)
}

/// Parses raster creation options (and the compression, as the tile format) into tile options.
pub(crate) fn raster_write_options(options: &crate::RasterWriteOptions) -> Result<TileWriteOptions> {
    let mut o = TileWriteOptions::default();
    if let Some(c) = &options.compression {
        o.tile_format = Some(c.to_ascii_uppercase());
    }
    for (k, v) in &options.creation_options {
        if !o.set(k, v)? {
            return Err(Error::InvalidArgument(format!("unknown creation option '{k}' for tile archives")));
        }
    }
    Ok(o)
}

/// The file name without its extension, as GDAL names tilesets.
fn stem(path: &str) -> String {
    std::path::Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("tiles").to_string()
}

/// Writes a raster as a PMTiles or MBTiles archive.
pub(crate) fn write_raster(
    ds: &crate::RasterDataset,
    path: &str,
    driver: crate::formats::RasterDriver,
    options: &crate::RasterWriteOptions,
) -> Result<()> {
    let kind = if driver == crate::formats::RasterDriver::MBTiles { ArchiveKind::MBTiles } else { ArchiveKind::PMTiles };
    let o = raster_write_options(options)?;
    #[cfg(not(target_family = "wasm"))]
    return write_pyramid(path, kind, &o, |sink| raster::make_tiles_from_dataset(ds, &o, sink));
    #[cfg(target_family = "wasm")]
    {
        std::fs::write(path, raster_to_bytes(ds, &o, &stem(path), kind)?)?;
        Ok(())
    }
}

/// Writes vector layers as a PMTiles or MBTiles archive, or a directory of MVT tiles.
pub(crate) fn write_vector(
    layers: &[wbvector::Layer],
    path: &str,
    driver: crate::formats::VectorDriver,
    o: &TileWriteOptions,
) -> Result<()> {
    use crate::formats::VectorDriver as V;
    let kind = match driver {
        V::MBTiles => ArchiveKind::MBTiles,
        V::PMTiles => ArchiveKind::PMTiles,
        _ => {
            #[cfg(not(target_family = "wasm"))]
            return write_mvt_dir(layers, o, std::path::Path::new(path));
            #[cfg(target_family = "wasm")]
            return Err(Error::Unsupported("directories of MVT tiles cannot be written in WebAssembly builds".into()));
        }
    };
    std::fs::write(path, vector_to_bytes(layers, o, &stem(path), kind)?)?;
    Ok(())
}

/// Whether bytes look like a Mapbox Vector Tile (gzipped, or starting with a layer).
pub(crate) fn is_mvt(b: &[u8]) -> bool {
    b.starts_with(&[0x1f, 0x8b]) || b.first() == Some(&0x1a)
}

/// Reads a local MVT tile (`.mvt`, `.mvt.gz`, or a `.pbf` that is not OpenStreetMap data) or a
/// directory of tiles with a `metadata.json`. `None` when the path is neither.
pub(crate) fn open_mvt_local(path: &str, options: &crate::VectorOpenOptions) -> Option<Result<Vec<wbvector::Layer>>> {
    let p = std::path::Path::new(path);
    // Found by the `metadata.json` beside its tiles, or by the content of a `.pbf` (OpenStreetMap's
    // extension), whatever driver the name points to: read only if the MVT driver is allowed.
    let allowed = || crate::formats::check_driver_allowed("MVT", path, crate::formats::Kind::Vector);
    #[cfg(not(target_family = "wasm"))]
    if p.is_dir() {
        if p.join("metadata.json").is_file() {
            return Some(allowed().and_then(|()| read_mvt_dir(p, options)));
        }
        // A zoom level directory of such a directory.
        let zoom = p.file_name().and_then(|n| n.to_str()).and_then(|n| n.parse::<u8>().ok());
        if let (Some(z), Some(parent)) = (zoom, p.parent())
            && parent.join("metadata.json").is_file()
        {
            let mut o = options.clone();
            o.tiles.zoom_level = Some(z);
            return Some(allowed().and_then(|()| read_mvt_dir(parent, &o)));
        }
    }
    let lower = path.to_ascii_lowercase();
    if !(lower.ends_with(".mvt") || lower.ends_with(".mvt.gz") || (lower.ends_with(".pbf") && !lower.ends_with(".osm.pbf"))) || !p.is_file()
    {
        return None;
    }
    let bytes = match std::fs::read(p) {
        Ok(b) => b,
        Err(e) => return Some(Err(e.into())),
    };
    if !is_mvt(&bytes) {
        return None;
    }
    Some(allowed().and_then(|()| vector::read_tile_file(&bytes, path, &options.tiles)))
}

/// The layer names of a vector tile archive, from its metadata (without reading tiles).
pub(crate) fn layer_names(path: &str) -> Result<Vec<String>> {
    let a = TileArchive::open_named(path)?;
    let names: Vec<String> = a
        .json()
        .get("vector_layers")
        .and_then(|v| v.as_array())
        .map(|v| v.iter().filter_map(|l| l.get("id").and_then(|i| i.as_str()).map(str::to_string)).collect())
        .unwrap_or_default();
    if names.is_empty() {
        return match archive_dataset(a, path, &Default::default())? {
            crate::Dataset::Vector(v) => Ok(v.layers.into_iter().map(|l| l.name).collect()),
            crate::Dataset::Raster(_) => Ok(Vec::new()),
        };
    }
    Ok(names)
}

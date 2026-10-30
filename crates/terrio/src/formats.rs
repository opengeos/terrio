//! The format drivers terrio knows, named as GDAL names them.

use serde::Serialize;
use std::path::Path;

/// Whether a driver handles rasters, vectors or point clouds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[non_exhaustive]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Gridded data.
    Raster,
    /// Features with geometries and attributes.
    Vector,
    /// Point clouds (see [`crate::pointcloud`]).
    #[serde(rename = "pointcloud")]
    PointCloud,
}

/// A raster format driver. New drivers are added in minor releases: match with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum RasterDriver {
    /// GeoTIFF and BigTIFF.
    GTiff,
    /// Cloud Optimized GeoTIFF (write only; COGs are read by [`RasterDriver::GTiff`]).
    Cog,
    /// Esri ASCII grid.
    AAIGrid,
    /// Esri binary grid workspace.
    AIG,
    /// Esri `.hdr` labelled raster: `.bil` or `.flt` with a `.hdr`.
    EHdr,
    /// GRASS ASCII raster.
    GRASSASCIIGrid,
    /// Surfer grid.
    GSBG,
    /// PCRaster.
    PCRaster,
    /// SAGA binary grid.
    SAGA,
    /// Idrisi / TerrSet.
    RST,
    /// ER Mapper.
    ERS,
    /// ENVI labelled raster.
    ENVI,
    /// ERDAS Imagine.
    HFA,
    /// GeoPackage tiles.
    GPKG,
    /// JPEG 2000.
    JP2,
    /// PNG with a world file.
    PNG,
    /// JPEG with a world file.
    JPEG,
    /// WebP with a world file, written lossless (see `drivers::webp`).
    WEBP,
    /// XYZ ASCII grid.
    XYZ,
    /// Zarr v2 and v3.
    Zarr,
    /// DTED elevation.
    DTED,
    /// SRTM HGT elevation tiles (`N37W120.hgt`), as GDAL's SRTMHGT driver reads and writes them.
    SRTMHGT,
    /// NetCDF: classic and netCDF-4 (groups included) read; classic (CDF-1, CDF-2, CDF-5) and
    /// netCDF-4 written.
    NetCDF,
    /// HDF5 (read only): its datasets as subdatasets, as GDAL's HDF5 driver reads them.
    HDF5,
    /// GRIB editions 1 and 2, as GDAL's GRIB driver reads them: a band per message. GRIB2 is
    /// written as GDAL writes it, with simple packing or IEEE floating point; through GDAL in
    /// builds with the `gdal` feature and without `grib`.
    GRIB,
    /// GDAL virtual raster (read by [`crate::ops::vrt`]; written by mosaics).
    VRT,
    /// GDAL-compatible indexed raster catalogs (GeoPackage/FlatGeobuf, read only).
    GTI,
    /// A mosaic of the assets of STAC items (see [`crate::stac`]); read only.
    STACIT,
    /// A STAC item's tiled assets, one raster with its tile matrices as overviews, as GDAL's
    /// STACTA driver reads it (see [`crate::stac`]); read only.
    STACTA,
    /// MBTiles raster tiles (PNG, JPEG, WebP), see [`crate::tiles`].
    MBTiles,
    /// PMTiles raster tiles (PNG, JPEG, WebP), see [`crate::tiles`].
    PMTiles,
    /// Any raster GDAL reads, through the optional GDAL backend (the `gdal` feature, see
    /// `gdal_backend`). Listed in [`RasterDriver::ALL`] only when the feature is enabled.
    Gdal,
}

/// A vector format driver. New drivers are added in minor releases: match with a wildcard arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[non_exhaustive]
pub enum VectorDriver {
    /// GeoJSON.
    GeoJSON,
    /// GeoJSON text sequences (RFC 8142): a feature per line, as GDAL's GeoJSONSeq driver reads
    /// and writes them.
    GeoJSONSeq,
    /// TopoJSON.
    TopoJSON,
    /// GeoPackage.
    GPKG,
    /// GeoParquet.
    Parquet,
    /// FlatGeobuf.
    FlatGeobuf,
    /// Esri Shapefile.
    Shapefile,
    /// KML.
    KML,
    /// Zipped KML.
    KMZ,
    /// GML.
    GML,
    /// GPS Exchange Format.
    GPX,
    /// AutoCAD DXF (ASCII or binary). Requires the `cad` feature.
    DXF,
    /// AutoCAD DWG. Requires the `cad` feature.
    DWG,
    /// MapInfo interchange format.
    MapInfo,
    /// OpenStreetMap PBF and XML.
    OSM,
    /// HEC-RAS geometry (`.g##.hdf`) and plan (`.p##.hdf`) HDF5 files, read only (the `hecras`
    /// feature and its `terrio::hecras` module). Recognized by content: an HDF5 signature and the
    /// root `File Type` HEC-RAS writes, so that other HDF5 files stay rasters.
    HECRAS,
    /// Comma-separated values with X/Y or WKT geometry columns.
    CSV,
    /// MBTiles vector tiles, see [`crate::tiles`].
    MBTiles,
    /// PMTiles vector tiles, see [`crate::tiles`].
    PMTiles,
    /// Mapbox Vector Tiles: a single tile, or a directory of tiles with a `metadata.json`.
    MVT,
    /// Any vector format GDAL reads, through the optional GDAL backend (the `gdal` feature, see
    /// `gdal_backend`). Listed in [`VectorDriver::ALL`] only when the feature is enabled.
    Gdal,
}

/// A description of a driver, for listings.
#[derive(Debug, Clone, Serialize)]
pub struct DriverInfo {
    /// The short name, as GDAL names the driver (`GTiff`, `ESRI Shapefile`).
    pub name: &'static str,
    /// The long name.
    pub long_name: &'static str,
    /// Raster or vector.
    pub kind: Kind,
    /// File extensions, without the dot.
    pub extensions: &'static [&'static str],
    /// Whether terrio reads the format.
    pub read: bool,
    /// Whether terrio writes the format.
    pub write: bool,
}

impl RasterDriver {
    /// Every raster driver.
    pub const ALL: &[RasterDriver] = &[
        Self::GTiff,
        Self::Cog,
        Self::AAIGrid,
        Self::AIG,
        Self::EHdr,
        Self::GRASSASCIIGrid,
        Self::GSBG,
        Self::PCRaster,
        Self::SAGA,
        Self::RST,
        Self::ERS,
        Self::ENVI,
        Self::HFA,
        Self::GPKG,
        Self::JP2,
        Self::PNG,
        Self::JPEG,
        Self::WEBP,
        Self::XYZ,
        Self::Zarr,
        Self::DTED,
        Self::SRTMHGT,
        Self::NetCDF,
        Self::HDF5,
        Self::GRIB,
        Self::VRT,
        Self::GTI,
        Self::STACIT,
        Self::STACTA,
        Self::MBTiles,
        Self::PMTiles,
        #[cfg(feature = "gdal")]
        Self::Gdal,
    ];

    /// The driver's description.
    pub fn info(self) -> DriverInfo {
        let (name, long_name, extensions, read, write): (_, _, &'static [&'static str], _, _) = match self {
            Self::GTiff => ("GTiff", "GeoTIFF", &["tif", "tiff"], true, true),
            Self::Cog => ("COG", "Cloud Optimized GeoTIFF", &["tif", "tiff"], true, true),
            Self::AAIGrid => ("AAIGrid", "Esri ASCII Grid", &["asc"], true, true),
            Self::AIG => ("AIG", "Esri Binary Grid", &["adf"], true, true),
            Self::EHdr => ("EHdr", "Esri .hdr Labelled (BIL, FLT)", &["bil", "flt"], true, true),
            Self::GRASSASCIIGrid => ("GRASSASCIIGrid", "GRASS ASCII Raster", &[], true, true),
            Self::GSBG => ("GSBG", "Surfer Grid", &["grd"], true, true),
            Self::PCRaster => ("PCRaster", "PCRaster", &["map"], true, true),
            Self::SAGA => ("SAGA", "SAGA GIS Binary Grid", &["sdat", "sgrd"], true, true),
            Self::RST => ("RST", "Idrisi / TerrSet Raster", &["rst", "rdc"], true, true),
            Self::ERS => ("ERS", "ER Mapper", &["ers"], true, true),
            Self::ENVI => ("ENVI", "ENVI Labelled Raster", &["envi", "hdr", "dat", "bsq", "bip"], true, true),
            // Written through GDAL when the `gdal` feature is enabled.
            Self::HFA => ("HFA", "ERDAS Imagine", &["img"], true, cfg!(feature = "gdal")),
            Self::GPKG => ("GPKG", "GeoPackage Tiles", &["gpkg"], true, true),
            Self::JP2 => ("JP2", "JPEG 2000", &["jp2", "j2k"], true, true),
            Self::PNG => ("PNG", "PNG with World File", &["png"], true, true),
            Self::JPEG => ("JPEG", "JPEG with World File", &["jpg", "jpeg"], true, true),
            Self::WEBP => ("WEBP", "WebP with World File (written lossless)", &["webp"], true, true),
            Self::XYZ => ("XYZ", "XYZ ASCII Grid", &["xyz"], true, true),
            Self::Zarr => ("Zarr", "Zarr v2/v3", &["zarr"], true, true),
            Self::DTED => ("DTED", "DTED Elevation", &["dt0", "dt1", "dt2"], true, true),
            Self::SRTMHGT => ("SRTMHGT", "SRTMHGT File Format", &["hgt"], true, true),
            Self::NetCDF => ("netCDF", "Network Common Data Format", &["nc", "nc4", "cdf"], true, true),
            Self::HDF5 => ("HDF5", "Hierarchical Data Format Release 5", &["h5", "hdf5", "he5"], true, false),
            // Written by terrio's GRIB2 writer, or through GDAL in builds with the `gdal` feature
            // and without `grib`.
            Self::GRIB => ("GRIB", "GRIdded Binary (.grb, .grb2)", &["grb", "grb2", "grib", "grib2"], true, true),
            Self::GTI => ("GTI", "GDAL Raster Tile Index", &["gti"], true, false),
            Self::VRT => ("VRT", "Virtual Raster", &["vrt"], true, true),
            Self::STACIT => ("STACIT", "Spatio-Temporal Asset Catalog Items", &[], true, false),
            Self::STACTA => ("STACTA", "Spatio-Temporal Asset Catalog Tiled Assets", &[], true, false),
            Self::MBTiles => ("MBTiles", "MBTiles", &["mbtiles"], true, true),
            Self::PMTiles => ("PMTiles", "ProtoMap Tiles", &["pmtiles"], true, true),
            Self::Gdal => ("GDAL", "Any raster GDAL reads (optional backend)", &[], cfg!(feature = "gdal"), false),
        };
        let built = self.missing_feature().is_none();
        DriverInfo { name, long_name, kind: Kind::Raster, extensions, read: read && built, write: write && built }
    }

    /// The cargo feature that builds the driver, when this build leaves it out.
    pub(crate) fn missing_feature(self) -> Option<&'static str> {
        let (feature, built) = match self {
            Self::GPKG => ("sqlite", cfg!(feature = "sqlite")),
            Self::JP2 => ("jp2", cfg!(feature = "jp2")),
            Self::Zarr => ("zarr", cfg!(feature = "zarr")),
            Self::NetCDF | Self::HDF5 => ("netcdf-hdf5", cfg!(feature = "netcdf-hdf5")),
            Self::MBTiles | Self::PMTiles => ("tiles", cfg!(feature = "tiles")),
            // Read and written by GDAL in builds with the GDAL backend without terrio's driver.
            Self::GRIB => ("grib", cfg!(feature = "grib") || cfg!(feature = "gdal")),
            _ => return None,
        };
        (!built).then_some(feature)
    }

    /// Fails when this build leaves the driver out.
    pub(crate) fn ensure_built(self) -> crate::Result<()> {
        self.missing_feature().map_or(Ok(()), |f| Err(crate::Error::disabled(f)))
    }

    /// The driver's short name.
    pub fn name(self) -> &'static str {
        self.info().name
    }

    /// The driver with a short name (case-insensitive), accepting GDAL's aliases.
    pub fn from_name(name: &str) -> Option<Self> {
        let n = name.to_ascii_lowercase();
        let alias = match n.as_str() {
            "geotiff" | "tif" | "tiff" => Some(Self::GTiff),
            "jpeg2000" | "jp2openjpeg" | "jp2kak" | "jp2ecw" => Some(Self::JP2),
            "gs7bg" | "gsag" | "surfer" => Some(Self::GSBG),
            "hgt" | "srtm" => Some(Self::SRTMHGT),
            "imagine" | "erdas" => Some(Self::HFA),
            "jpg" => Some(Self::JPEG),
            "geopackage" => Some(Self::GPKG),
            "nc" | "netcdf4" => Some(Self::NetCDF),
            "hdf5image" => Some(Self::HDF5),
            "grib2" | "grb" | "grb2" => Some(Self::GRIB),
            _ => None,
        };
        alias.or_else(|| Self::ALL.iter().copied().find(|d| d.name().eq_ignore_ascii_case(name)))
    }

    /// The driver for an output path's extension.
    pub fn for_extension(path: &str) -> Option<Self> {
        let ext = extension(path)?;
        match ext.as_str() {
            "tif" | "tiff" => Some(Self::GTiff),
            "grd" => Some(Self::GSBG),
            "img" => Some(Self::HFA),
            _ => Self::ALL.iter().copied().find(|d| *d != Self::Cog && d.info().extensions.contains(&ext.as_str())),
        }
    }

    pub(crate) fn to_wb(self) -> wbraster::RasterFormat {
        use wbraster::RasterFormat as F;
        match self {
            Self::GTiff | Self::Cog => F::GeoTiff,
            Self::AAIGrid => F::EsriAscii,
            Self::AIG => F::EsriBinary,
            Self::EHdr => F::EsriFloat,
            Self::GRASSASCIIGrid => F::GrassAscii,
            Self::GSBG => F::SurferGrd,
            Self::PCRaster => F::Pcraster,
            Self::SAGA => F::Saga,
            Self::RST => F::Idrisi,
            Self::ERS => F::ErMapper,
            Self::ENVI => F::Envi,
            Self::HFA => F::HfaImg,
            Self::GPKG => F::GeoPackage,
            Self::JP2 => F::Jpeg2000,
            Self::PNG => F::Png,
            Self::JPEG => F::Jpeg,
            Self::XYZ => F::Xyz,
            Self::Zarr => F::Zarr,
            Self::DTED => F::Dted,
            // Never used: terrio reads and writes NetCDF, GRIB, WebP and SRTM tiles, and reads HDF5,
            // itself.
            Self::NetCDF | Self::HDF5 | Self::GRIB | Self::WEBP | Self::SRTMHGT => F::Zarr,
            // Never used: terrio reads and writes VRTs and tile archives, and reads STAC mosaics
            // itself; GDAL reads its formats.
            Self::VRT | Self::GTI | Self::STACIT | Self::STACTA | Self::MBTiles | Self::PMTiles | Self::Gdal => F::GeoTiff,
        }
    }

    pub(crate) fn from_wb(f: wbraster::RasterFormat) -> Self {
        use wbraster::RasterFormat as F;
        match f {
            F::GeoTiff => Self::GTiff,
            F::EsriAscii => Self::AAIGrid,
            F::EsriBinary => Self::AIG,
            F::EsriFloat => Self::EHdr,
            F::GrassAscii => Self::GRASSASCIIGrid,
            F::SurferGrd => Self::GSBG,
            F::Pcraster => Self::PCRaster,
            F::Saga => Self::SAGA,
            F::Idrisi => Self::RST,
            F::ErMapper => Self::ERS,
            F::Envi => Self::ENVI,
            F::HfaImg => Self::HFA,
            F::GeoPackage => Self::GPKG,
            F::Jpeg2000 => Self::JP2,
            F::Png => Self::PNG,
            F::Jpeg => Self::JPEG,
            F::Xyz => Self::XYZ,
            F::Zarr => Self::Zarr,
            F::Dted => Self::DTED,
        }
    }
}

impl VectorDriver {
    /// Every vector driver.
    pub const ALL: &[VectorDriver] = &[
        Self::GeoJSON,
        Self::GeoJSONSeq,
        Self::TopoJSON,
        Self::GPKG,
        Self::Parquet,
        Self::FlatGeobuf,
        Self::Shapefile,
        Self::KML,
        Self::KMZ,
        Self::GML,
        Self::GPX,
        Self::DXF,
        Self::DWG,
        Self::MapInfo,
        Self::OSM,
        Self::HECRAS,
        Self::CSV,
        Self::MBTiles,
        Self::PMTiles,
        Self::MVT,
        #[cfg(feature = "gdal")]
        Self::Gdal,
    ];

    /// The driver's description.
    pub fn info(self) -> DriverInfo {
        let (name, long_name, extensions, read, write): (_, _, &'static [&'static str], _, _) = match self {
            Self::GeoJSON => ("GeoJSON", "GeoJSON", &["geojson", "json"], true, true),
            Self::GeoJSONSeq => ("GeoJSONSeq", "GeoJSON text sequences", &["geojsonl", "geojsons", "geojsonseq"], true, true),
            Self::TopoJSON => ("TopoJSON", "TopoJSON", &["topojson"], true, true),
            Self::GPKG => ("GPKG", "GeoPackage", &["gpkg"], true, true),
            Self::Parquet => ("Parquet", "GeoParquet", &["parquet", "geoparquet"], true, true),
            Self::FlatGeobuf => ("FlatGeobuf", "FlatGeobuf", &["fgb"], true, true),
            Self::Shapefile => ("ESRI Shapefile", "Esri Shapefile", &["shp"], true, true),
            Self::KML => ("KML", "Keyhole Markup Language", &["kml"], true, true),
            Self::KMZ => ("KMZ", "Zipped KML", &["kmz"], true, true),
            Self::GML => ("GML", "Geography Markup Language", &["gml"], true, true),
            Self::GPX => ("GPX", "GPS Exchange Format", &["gpx"], true, true),
            Self::DXF => ("DXF", "AutoCAD DXF", &["dxf"], true, true),
            Self::DWG => ("DWG", "AutoCAD DWG", &["dwg"], true, true),
            Self::MapInfo => ("MapInfo File", "MapInfo TAB and Interchange Format", &["mif", "tab"], true, true),
            Self::OSM => ("OSM", "OpenStreetMap PBF and XML", &["pbf", "osm"], true, false),
            // No extension of its own: HEC-RAS files share `.hdf` and `.h5` with other HDF5 files.
            Self::HECRAS => ("HECRAS", "HEC-RAS geometry and plan HDF5", &[], true, false),
            Self::CSV => ("CSV", "Comma Separated Value", &["csv", "tsv"], true, true),
            Self::MBTiles => ("MBTiles", "MBTiles", &["mbtiles"], true, true),
            Self::PMTiles => ("PMTiles", "ProtoMap Tiles", &["pmtiles"], true, true),
            Self::MVT => ("MVT", "Mapbox Vector Tiles", &["mvt"], true, true),
            Self::Gdal => ("GDAL", "Any vector format GDAL reads (optional backend)", &[], cfg!(feature = "gdal"), false),
        };
        let built = self.missing_feature().is_none();
        DriverInfo { name, long_name, kind: Kind::Vector, extensions, read: read && built, write: write && built }
    }

    /// The cargo feature that builds the driver, when this build leaves it out.
    pub(crate) fn missing_feature(self) -> Option<&'static str> {
        let (feature, built) = match self {
            Self::GPKG => ("sqlite", cfg!(feature = "sqlite")),
            Self::Parquet => ("parquet", cfg!(feature = "parquet")),
            Self::FlatGeobuf => ("flatgeobuf", cfg!(feature = "flatgeobuf")),
            Self::DXF | Self::DWG => ("cad", cfg!(feature = "cad")),
            Self::OSM => ("osm", cfg!(feature = "osm")),
            Self::HECRAS => ("hecras", cfg!(feature = "hecras")),
            Self::MBTiles | Self::PMTiles | Self::MVT => ("tiles", cfg!(feature = "tiles")),
            _ => return None,
        };
        (!built).then_some(feature)
    }

    /// Fails when this build leaves the driver out.
    pub(crate) fn ensure_built(self) -> crate::Result<()> {
        self.missing_feature().map_or(Ok(()), |f| Err(crate::Error::disabled(f)))
    }

    /// The driver's short name.
    pub fn name(self) -> &'static str {
        self.info().name
    }

    /// The driver with a short name (case-insensitive), accepting GDAL's aliases.
    pub fn from_name(name: &str) -> Option<Self> {
        let n = name.to_ascii_lowercase();
        let alias = match n.as_str() {
            "shp" | "shapefile" | "esri_shapefile" => Some(Self::Shapefile),
            "geopackage" => Some(Self::GPKG),
            "geoparquet" => Some(Self::Parquet),
            "fgb" => Some(Self::FlatGeobuf),
            "libkml" => Some(Self::KML),
            "mapinfo" | "mif" => Some(Self::MapInfo),
            "json" => Some(Self::GeoJSON),
            "hec-ras" | "hec_ras" | "hecras_hdf" => Some(Self::HECRAS),
            _ => None,
        };
        alias.or_else(|| Self::ALL.iter().copied().find(|d| d.name().eq_ignore_ascii_case(name)))
    }

    /// The driver for a path's extension.
    pub fn for_extension(path: &str) -> Option<Self> {
        let lower = path.to_ascii_lowercase();
        if lower.ends_with(".osm.pbf") {
            return Some(Self::OSM);
        }
        let ext = extension(path)?;
        Self::ALL.iter().copied().find(|d| d.info().extensions.contains(&ext.as_str()))
    }

    /// The format library's format, when terrio does not supply the driver itself.
    pub(crate) fn to_wb(self) -> Option<wbvector::VectorFormat> {
        use wbvector::VectorFormat as F;
        Some(match self {
            Self::GeoJSON => F::GeoJson,
            Self::TopoJSON => F::TopoJson,
            Self::GPKG => F::GeoPackage,
            #[cfg(feature = "parquet")]
            Self::Parquet => F::GeoParquet,
            Self::FlatGeobuf => F::FlatGeobuf,
            Self::Shapefile => F::Shapefile,
            Self::KML => F::Kml,
            Self::KMZ => F::Kmz,
            Self::GML => F::Gml,
            Self::GPX => F::Gpx,
            Self::MapInfo => F::MapInfoMif,
            #[cfg(feature = "osm")]
            Self::OSM => F::OsmPbf,
            #[cfg(not(feature = "parquet"))]
            Self::Parquet => return None,
            #[cfg(not(feature = "osm"))]
            Self::OSM => return None,
            Self::DXF
            | Self::DWG
            | Self::GeoJSONSeq
            | Self::CSV
            | Self::MBTiles
            | Self::PMTiles
            | Self::MVT
            | Self::HECRAS
            | Self::Gdal => return None,
        })
    }
}

/// Every driver: rasters, vectors, then point clouds.
pub fn all_drivers() -> Vec<DriverInfo> {
    let point_clouds = [
        DriverInfo {
            name: "LAS",
            long_name: "ASPRS LAS 1.0 to 1.4",
            kind: Kind::PointCloud,
            extensions: &["las"],
            read: true,
            write: true,
        },
        DriverInfo {
            name: "LAZ",
            long_name: "LASzip-compressed LAS",
            kind: Kind::PointCloud,
            extensions: &["laz"],
            read: true,
            write: true,
        },
        DriverInfo {
            name: "COPC",
            long_name: "Cloud Optimized Point Cloud",
            kind: Kind::PointCloud,
            extensions: &["copc.laz"],
            read: true,
            write: true,
        },
        DriverInfo {
            name: "EPT",
            long_name: "Entwine Point Tile",
            kind: Kind::PointCloud,
            extensions: &["json"],
            read: true,
            write: false,
        },
        DriverInfo { name: "PLY", long_name: "Polygon File Format", kind: Kind::PointCloud, extensions: &["ply"], read: true, write: true },
        DriverInfo {
            name: "E57",
            long_name: "ASTM E57 3D imaging data",
            kind: Kind::PointCloud,
            extensions: &["e57"],
            read: true,
            write: true,
        },
    ];
    let point_clouds = point_clouds.map(|d| DriverInfo {
        read: d.read && cfg!(feature = "pointcloud"),
        write: d.write && cfg!(feature = "pointcloud"),
        ..d
    });
    RasterDriver::ALL.iter().map(|d| d.info()).chain(VectorDriver::ALL.iter().map(|d| d.info())).chain(point_clouds).collect()
}

/// The lowercase extension of a path.
pub(crate) fn extension(path: &str) -> Option<String> {
    Path::new(path.trim_end_matches(['/', '\\'])).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase())
}

// ---------------------------------------------------------------------------------------------
// Which drivers may open a dataset: GDAL's driver de-registration (`GDALDeregisterDriver`) and the
// allowed drivers of `GDALOpenEx`, for code that opens untrusted files.
// ---------------------------------------------------------------------------------------------

/// The process-wide lists set with [`set_allowed_drivers`], upper case: the rasters', then the
/// vectors'. `None` until the first call for that kind.
static ALLOWED: std::sync::RwLock<AllowLists> = std::sync::RwLock::new([None, None]);

/// An allow-list per kind of driver: rasters, then vectors.
type AllowLists = [Option<Vec<String>>; 2];

/// A driver name as terrio names it, in upper case. GDAL's names for formats terrio names
/// differently are accepted (`JP2OpenJPEG` is `JP2`).
pub fn canonical_driver_name(name: &str) -> String {
    let upper = name.trim().to_ascii_uppercase();
    match upper.as_str() {
        "JP2OPENJPEG" | "JPEG2000" | "JP2KAK" | "JP2ECW" | "JP2MRSID" => "JP2".to_string(),
        _ => upper,
    }
}

/// The slots of [`AllowLists`] a kind names: both for `None`.
fn allow_list_slots(kind: Option<Kind>) -> crate::Result<&'static [usize]> {
    match kind {
        None => Ok(&[0, 1]),
        Some(Kind::Raster) => Ok(&[0]),
        Some(Kind::Vector) => Ok(&[1]),
        Some(other) => Err(crate::Error::InvalidArgument(format!("{other:?} drivers have no allow-list: use Raster, Vector or None"))),
    }
}

/// Restricts the drivers datasets of `kind` may be opened with, process-wide: rasters, vectors, or
/// both (`None`). It is the counterpart of de-registering GDAL's other drivers, and like a
/// de-registration it lasts for the process: the first call for a kind sets its list, later calls
/// intersect with it, so they narrow it but never re-enable a driver, and lifting a list once set
/// (`drivers: None`) is refused. Names are terrio's or GDAL's. With a restriction, a dataset opens
/// only if every driver its name and its content point to is allowed (see [`check_drivers`]);
/// pointer formats (VRT, STAC, STACIT, Zarr) must be named. The entry points that read a dataset
/// with one driver whatever its name (the multidimensional, VRT, tile archive, caller-fetched and
/// point cloud readers, `LazyRaster::open_tiff` and `from_tiff_bytes`, the GDAL backend) read it
/// only if that driver is allowed.
pub fn set_allowed_drivers(drivers: Option<&[&str]>, kind: Option<Kind>) -> crate::Result<()> {
    crate::par::fork_safe();
    let mut lists = ALLOWED.write().unwrap_or_else(|e| e.into_inner());
    narrow_allow_lists(&mut lists, drivers, kind)
}

/// Holds the allow-list's lock across a fork, for [`crate::par`]: the forking thread takes it
/// before the process is copied and lets it go after, so that the child never starts with it held
/// by a thread it does not have.
#[cfg(all(unix, not(target_family = "wasm")))]
pub(crate) fn hold_for_fork(held: &mut Vec<Box<dyn std::any::Any>>) {
    crate::par::fork_safe();
    held.push(Box::new(ALLOWED.write().unwrap_or_else(|e| e.into_inner())));
}

/// [`set_allowed_drivers`] on `lists`: sets the list of each kind `kind` names that has none, and
/// intersects the others with `drivers`. Nothing changes when it fails.
fn narrow_allow_lists(lists: &mut AllowLists, drivers: Option<&[&str]>, kind: Option<Kind>) -> crate::Result<()> {
    let slots = allow_list_slots(kind)?;
    let Some(drivers) = drivers else {
        if slots.iter().any(|&i| lists[i].is_some()) {
            return Err(crate::Error::InvalidArgument(
                "the allowed drivers cannot be lifted once set: like a de-registered GDAL driver, a refused driver stays \
                 refused for the life of the process"
                    .into(),
            ));
        }
        return Ok(());
    };
    let mut names: Vec<String> = Vec::new();
    for name in drivers.iter().map(|n| canonical_driver_name(n)) {
        if !name.is_empty() && !names.contains(&name) {
            names.push(name);
        }
    }
    for &i in slots {
        lists[i] = Some(match lists[i].take() {
            None => names.clone(),
            Some(set) => set.into_iter().filter(|n| names.contains(n)).collect(),
        });
    }
    Ok(())
}

/// The drivers datasets of `kind` may be opened with: its list from [`set_allowed_drivers`] and the
/// comma-separated `TERRIO_ALLOWED_DRIVERS` option (see [`crate::config`]), which applies to every
/// kind; when both are set, a driver must be in both. For `None` (a dataset whose kind is not
/// known before it is opened), a driver must be allowed by each kind it belongs to. `None` when
/// nothing restricts them.
pub fn allowed_drivers(kind: Option<Kind>) -> Option<Vec<String>> {
    crate::par::fork_safe();
    let lists = ALLOWED.read().unwrap_or_else(|e| e.into_inner()).clone();
    let set = match kind {
        Some(Kind::Raster) => lists[0].clone(),
        Some(Kind::Vector) => lists[1].clone(),
        Some(_) => None,
        None => allowed_by_every_kind(&lists),
    };
    let option = crate::config::get("TERRIO_ALLOWED_DRIVERS")
        .map(|v| v.split(',').map(canonical_driver_name).filter(|n| !n.is_empty()).collect::<Vec<_>>());
    match (set, option) {
        (Some(a), Some(b)) => Some(a.into_iter().filter(|d| b.contains(d)).collect()),
        (a, b) => a.or(b),
    }
}

/// The drivers allowed by the list of every kind they belong to, as GDAL drops a driver that serves
/// both kinds (GPKG) when either kind's de-registration drops it. A name terrio lists under neither
/// kind counts as both. `None` when neither kind has a list.
fn allowed_by_every_kind(lists: &AllowLists) -> Option<Vec<String>> {
    if lists.iter().all(Option::is_none) {
        return None;
    }
    let raster: Vec<String> = RasterDriver::ALL.iter().map(|d| canonical_driver_name(d.info().name)).collect();
    let vector: Vec<String> = VectorDriver::ALL.iter().map(|d| canonical_driver_name(d.info().name)).collect();
    let mut names: Vec<String> = raster.iter().chain(&vector).chain(lists.iter().flatten().flatten()).cloned().collect();
    names.sort();
    names.dedup();
    let allows = |list: &Option<Vec<String>>, name: &String| list.as_ref().is_none_or(|l| l.contains(name));
    names.retain(|n| {
        let (is_raster, is_vector) = match (raster.contains(n), vector.contains(n)) {
            (false, false) => (true, true),
            kinds => kinds,
        };
        (!is_raster || allows(&lists[0], n)) && (!is_vector || allows(&lists[1], n))
    });
    Some(names)
}

/// What a dataset's name and its first bytes say its format is, as upper-case driver names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identified {
    /// The drivers its name points to: a GDAL-style prefix (`STAC:`, `ZARR:`, `NETCDF:`...), a
    /// `.zarr` component, or its extension (several drivers may share one).
    pub by_name: Vec<String>,
    /// The format its first bytes show, when they show one. Read from local files, and from remote
    /// ones only when the name points to no driver.
    pub by_content: Option<String>,
}

/// Identifies a dataset's format by its name and its content, without opening it with any driver.
pub fn identify(path: &str) -> Identified {
    let trimmed = path.trim();
    let (location, by_name) = match named_drivers(trimmed) {
        Named::Prefix(driver) => return Identified { by_name: vec![driver.to_string()], by_content: None },
        Named::Name(location, by_name) => (location, by_name),
    };
    let mut remote_len = None;
    let head = if location.is_local() {
        read_head_local(Path::new(trimmed))
    } else if matches!(location, crate::vfs::Location::Zip { .. }) {
        // A member of an archive, local or remote: its first bytes, read from the archive.
        crate::vfs::member_head(&location, 1024)
    } else if by_name.is_empty() {
        read_head_remote(&location).map(|(head, len)| {
            remote_len = Some(len);
            head
        })
    } else {
        None
    };
    // An SRTM tile has no signature: its name and size are its content, as GDAL's SRTMHGT driver
    // identifies it, and terrio reads it whatever driver its extension names (`.img`, HFA's). A
    // remote file's size is the one its first request gave.
    let by_content = head.as_deref().and_then(sniff).or_else(|| {
        let tile = match remote_len {
            Some(len) => crate::drivers::hgt::is_tile(&location.file_name(), len, head.as_deref().unwrap_or_default()),
            None => location.is_local() && crate::drivers::hgt::is_hgt(Path::new(trimmed)),
        };
        tile.then_some("SRTMHGT")
    });
    Identified { by_name, by_content: by_content.map(str::to_string) }
}

/// Identifies a dataset given as bytes by its name and its first bytes, as [`identify`] does a
/// file.
pub fn identify_bytes(name: &str, bytes: &[u8]) -> Identified {
    match named_drivers(name.trim()) {
        Named::Prefix(driver) => Identified { by_name: vec![driver.to_string()], by_content: None },
        Named::Name(_, by_name) => {
            let by_content = sniff(&bytes[..bytes.len().min(1024)])
                .or_else(|| crate::drivers::hgt::is_hgt_bytes(name.trim(), bytes).then_some("SRTMHGT"));
            Identified { by_name, by_content: by_content.map(str::to_string) }
        }
    }
}

/// What a dataset's name says its format is.
enum Named {
    /// A GDAL-style prefix (`STAC:`, `ZARR:`...): it names the driver, and nothing is read.
    Prefix(&'static str),
    /// Its location, and the drivers a `.zarr` component or its extension points to.
    Name(crate::vfs::Location, Vec<String>),
}

/// The drivers a dataset's name points to (see [`Named`]).
fn named_drivers(trimmed: &str) -> Named {
    let upper = trimmed.to_ascii_uppercase();
    for (prefix, driver) in [
        ("STACIT:", "STACIT"),
        ("STACTA:", "STACTA"),
        ("STAC:", "STAC"),
        ("ZARR:", "ZARR"),
        ("NETCDF:", "NETCDF"),
        ("HDF5:", "HDF5"),
        ("GPKG:", "GPKG"),
        ("GDAL:", "GDAL"),
        ("MEM:", "MEM"),
    ] {
        if upper.starts_with(prefix) {
            return Named::Prefix(driver);
        }
    }
    let location = crate::vfs::Location::parse(trimmed);
    let name = location.file_name().to_ascii_lowercase();
    let mut by_name = Vec::new();
    let without_query = trimmed.split(['?', '#']).next().unwrap_or(trimmed);
    if without_query.split(['/', '\\']).any(|c| c.to_ascii_lowercase().ends_with(".zarr")) {
        by_name.push("ZARR".to_string());
    } else {
        for d in all_drivers() {
            if d.extensions.iter().any(|e| name.ends_with(&format!(".{e}"))) {
                let n = canonical_driver_name(d.name);
                if !by_name.contains(&n) {
                    by_name.push(n);
                }
            }
        }
    }
    Named::Name(location, by_name)
}

/// Up to 1024 bytes from the start of a local file.
fn read_head_local(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut buf = vec![0u8; 1024];
    let n = f.read(&mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Up to 1024 bytes from the start of a remote file, and the file's length.
fn read_head_remote(location: &crate::vfs::Location) -> Option<(Vec<u8>, u64)> {
    #[cfg(remote)]
    {
        // Read as a lazy open reads it, so that the open after this check finds the bytes in the
        // process-wide cache instead of requesting them again.
        crate::http::head_and_len(&location.remote()?, 1024).ok()
    }
    #[cfg(not(remote))]
    {
        let _ = location;
        None
    }
}

/// The drivers whose files start with a signature, with what shows it in a file's first bytes: a file
/// named for one of these drivers alone is read only if its first bytes show one of their formats.
/// Each is what terrio's reader of the format requires at the start of a file, or more lenient: a
/// TIFF's, PNG's, JPEG's, JPEG 2000's, GRIB's, WebP's or VRT's start as [`sniff`] finds it, SQLite's
/// header (GeoPackage, MBTiles), netCDF's (classic, or HDF5), HDF5's ([`shows_hdf5`]), an Imagine
/// file's, a DTED file's (any of the labels GDAL accepts, though terrio reads those starting with
/// `UHL1`), a Surfer grid's (text, 6 binary or 7 binary), and an Esri ASCII grid's header
/// ([`shows_ascii_grid`]).
type Signature = fn(&[u8]) -> bool;
const SIGNED_DRIVERS: &[(&str, Signature)] = &[
    ("GTIFF", |h| sniff(h) == Some("GTIFF")),
    ("COG", |h| sniff(h) == Some("GTIFF")),
    ("PNG", |h| sniff(h) == Some("PNG")),
    ("JPEG", |h| sniff(h) == Some("JPEG")),
    ("JP2", |h| sniff(h) == Some("JP2")),
    ("GRIB", |h| sniff(h) == Some("GRIB")),
    ("WEBP", |h| sniff(h) == Some("WEBP")),
    ("VRT", |h| sniff(h) == Some("VRT")),
    ("GPKG", |h| h.starts_with(b"SQLite format 3\0")),
    ("MBTILES", |h| h.starts_with(b"SQLite format 3\0")),
    ("NETCDF", |h| h.starts_with(b"CDF") || shows_hdf5(h)),
    ("HDF5", shows_hdf5),
    ("HFA", |h| h.starts_with(b"EHFA_HEADER_TAG")),
    ("DTED", |h| h.starts_with(b"UHL") || h.starts_with(b"VOL") || h.starts_with(b"HDR")),
    ("GSBG", |h| h.starts_with(b"DSAA") || h.starts_with(b"DSBB") || h.starts_with(b"DSRB")),
    ("AAIGRID", shows_ascii_grid),
];

/// Whether a file's first bytes show an HDF5 file: its signature at 0, or after a user block of
/// 512, 1,024, 2,048... bytes as far as `head` goes, or a user block marked `<HDF_UserBlock>`,
/// whose file GDAL's HDF5 driver takes too.
pub(crate) fn shows_hdf5(head: &[u8]) -> bool {
    if head.starts_with(b"<HDF_UserBlock>") {
        return true;
    }
    let mut offset = 0;
    while offset + 8 <= head.len() {
        if head[offset..offset + 8] == *b"\x89HDF\r\n\x1a\n" {
            return true;
        }
        offset = if offset == 0 { 512 } else { offset * 2 };
    }
    false
}

/// Whether a file's first bytes show an Esri ASCII grid as terrio's reader takes one: its first line
/// that is not blank starts with a header keyword. First bytes that are all blank tell nothing.
fn shows_ascii_grid(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head);
    text.lines().map(str::trim).find(|line| !line.is_empty()).is_none_or(|line| {
        let key = line.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
        crate::drivers::aaigrid::HEADER_KEYS.contains(&key.as_str())
    })
}

/// Checks the first bytes of a remote file (up to [`crate::http::FIRST_READ`] of them) and its
/// length, `len`, before the rest is downloaded, as GDAL decides from its first read whether any
/// driver recognises a file: a file whose name points only to drivers that recognise their files
/// by a signature ([`SIGNED_DRIVERS`]) is refused when its first bytes show none, rather than
/// downloaded whole and refused by the reader: 64 MiB of noise named `.tif` cost 67,174,400 bytes
/// (GDAL 16,384), and a 50,000,000-byte GeoJSON document named `.vrt`, `.nc`, `.webp`, `.h5` or
/// `.gpkg` cost 49,999,930 (GDAL 49,152), `.vrt` under an allow-list of VRT too.
///
/// An SRTM tile has no signature: it is known by its name and size ([`crate::drivers::hgt::is_tile`]),
/// whatever driver its extension names (`N37W120.img`, Imagine's), and read when the SRTMHGT driver
/// is allowed (refused here otherwise, as its reader refuses it once downloaded). A file under a
/// tile's extension that is no tile, by its size (`N38W120.hgt` of 50,000,000 bytes) or by its name
/// (`big.hgt`, `N38W120_v2.hgt`), is refused, as GDAL's SRTMHGT driver does not take it (from its
/// name, or its size by `VSIStatL`) without reading it, unless what it is named for besides takes
/// it: a file named for signed drivers alone (`.img`) by their signatures, as above, any other by
/// what the read takes under its name ([`crate::drivers::takes_lone_file`]). Files named for other
/// drivers, or for none, are not judged here. `name` is the file's name, `shown` how the refusal
/// names it.
///
/// A Surfer 6 grid, which the read takes by its content whatever its name, is refused when the
/// GSBG driver is not allowed, as a local one is, under any name a raster read takes (a raster
/// driver's, or no driver's; a vector driver's name is read by that driver): `big.hgt`, a
/// 50,000,000-byte file starting with a Surfer 6 header, was downloaded whole (2 requests) under an
/// allow-list of SRTMHGT and GTiff and opened as GSBG.
#[cfg(remote)]
pub(crate) fn check_remote_head(name: &str, shown: &str, head: &[u8], len: u64) -> crate::Result<()> {
    let shown = shown.split(['?', '#']).next().unwrap_or(shown);
    if sniff(head) == Some("GSBG") && read_as_raster(name) {
        check_driver_allowed("GSBG", shown, Kind::Raster)?;
    }
    if crate::drivers::hgt::is_tile(name, len, head) {
        return check_driver_allowed("SRTMHGT", shown, Kind::Raster);
    }
    let Named::Name(_, by_name) = named_drivers(name) else { return Ok(()) };
    let signed = |driver: &String| SIGNED_DRIVERS.iter().find(|(d, _)| d == driver).map(|(_, signature)| *signature);
    if by_name.is_empty() || !by_name.iter().all(|driver| signed(driver).is_some()) {
        if crate::drivers::hgt::has_tile_extension(name) && !crate::drivers::takes_lone_file(name, head, len) {
            let what = if crate::drivers::hgt::is_tile_name(name) {
                format!("it has {len} bytes, which is no SRTM tile's size")
            } else {
                "its name is no SRTM tile's".to_string()
            };
            return Err(crate::Error::Unsupported(format!(
                "'{shown}' not recognized as being in a supported file format: named for SRTMHGT, {what}"
            )));
        }
        return Ok(());
    }
    if by_name.iter().filter_map(signed).any(|shows| shows(head)) {
        return Ok(());
    }
    let what = match sniff(head) {
        Some(c) => format!("holds {c} data"),
        None => "starts with no signature of its format".to_string(),
    };
    Err(crate::Error::Unsupported(format!(
        "'{shown}' not recognized as being in a supported file format: named for {}, it {what}",
        by_name.join("/")
    )))
}

/// Whether a remote file's first bytes show a GRIB file: for remote files whose name has no GRIB
/// extension (NOAA names its products `gfs.t00z.pgrb2.1p00.f000`), so that they are read by range
/// as named ones are rather than downloaded whole.
pub(crate) fn remote_is_grib(location: &crate::vfs::Location) -> bool {
    read_head_remote(location).is_some_and(|(head, _)| sniff(&head) == Some("GRIB"))
}

/// Whether every whole read of a file named `name` reaches terrio's raster drivers, whatever kind of
/// dataset it opens: a name of a raster driver, or of none (an open of either kind reads a file
/// named for a vector driver with that driver).
#[cfg(remote)]
fn read_as_raster(name: &str) -> bool {
    RasterDriver::for_extension(name).is_some() || matches!(named_drivers(name), Named::Name(_, by_name) if by_name.is_empty())
}

/// The formats [`sniff`] names that terrio reads as rasters under names of theirs (a GeoPackage's
/// or an MBTiles archive's SQLite header, a PMTiles archive's, a STAC document's).
#[cfg(remote)]
const RASTER_CONTENT: &[&str] = &["GTIFF", "JP2", "PNG", "JPEG", "WEBP", "GRIB", "NETCDF", "HDF5", "GPKG", "PMTILES", "VRT", "STAC"];

/// The formats [`sniff`] names that terrio reads as vectors under names of theirs.
#[cfg(remote)]
const VECTOR_CONTENT: &[&str] = &["GEOJSON", "TOPOJSON", "PARQUET", "FLATGEOBUF", "ESRI SHAPEFILE", "KML", "GPX", "GML"];

/// Whether terrio reads files of the format [`sniff`] names `format` under names of theirs: as
/// rasters, or, `generic`, as rasters or vectors.
#[cfg(remote)]
pub(crate) fn read_under_own_names(format: &str, generic: bool) -> bool {
    RASTER_CONTENT.contains(&format) || (generic && VECTOR_CONTENT.contains(&format))
}

/// The format a file's first bytes show, as an upper-case driver name.
pub fn sniff(head: &[u8]) -> Option<&'static str> {
    let starts = |p: &[u8]| head.starts_with(p);
    if starts(b"II*\0") || starts(b"MM\0*") || starts(b"II+\0") || starts(b"MM\0+") {
        return Some("GTIFF");
    }
    if starts(b"\0\0\0\x0cjP  \r\n\x87\n") || starts(&[0xFF, 0x4F, 0xFF, 0x51]) {
        return Some("JP2");
    }
    if starts(b"\x89PNG\r\n\x1a\n") {
        return Some("PNG");
    }
    if starts(&[0xFF, 0xD8, 0xFF]) {
        return Some("JPEG");
    }
    if starts(b"RIFF") && head.get(8..12) == Some(b"WEBP") {
        return Some("WEBP");
    }
    if starts(b"GRIB") {
        return Some("GRIB");
    }
    if starts(b"CDF\x01") || starts(b"CDF\x02") || starts(b"CDF\x05") {
        return Some("NETCDF");
    }
    if starts(b"\x89HDF\r\n\x1a\n") {
        return Some("HDF5");
    }
    if starts(b"SQLite format 3\0") {
        return Some("GPKG");
    }
    if starts(b"PMTiles") {
        return Some("PMTILES");
    }
    if starts(b"PAR1") {
        return Some("PARQUET");
    }
    if starts(b"fgb\x03") {
        return Some("FLATGEOBUF");
    }
    if starts(&[0x00, 0x00, 0x27, 0x0A]) {
        return Some("ESRI SHAPEFILE");
    }
    // A Surfer 6 binary grid, which terrio's reader takes by its content whatever its name, as
    // GDAL's GSBG driver identifies it.
    if starts(b"DSBB") {
        return Some("GSBG");
    }
    // A GRIB message after a header (a WMO bulletin's, say): GDAL's GRIB driver looks for "GRIB"
    // and an edition (1 or 2) in the first kilobyte.
    if head[..head.len().min(1024)].windows(8).any(|w| &w[..4] == b"GRIB" && (w[7] == 1 || w[7] == 2)) {
        return Some("GRIB");
    }
    let text = String::from_utf8_lossy(head);
    let text = text.trim_start_matches('\u{feff}').trim_start();
    if text.starts_with('<') {
        let lower = text.to_ascii_lowercase();
        return Some(if lower.contains("<vrtdataset") {
            "VRT"
        } else if lower.contains("<kml") {
            "KML"
        } else if lower.contains("<gpx") {
            "GPX"
        } else if lower.contains("gml") {
            "GML"
        } else {
            "XML"
        });
    }
    if text.starts_with('{') || text.starts_with('[') {
        return Some(if text.contains("\"stac_version\"") {
            "STAC"
        } else if text.contains("\"Topology\"") {
            "TOPOJSON"
        } else {
            "GEOJSON"
        });
    }
    None
}

/// Refuses `path` unless every driver its name points to — at least one of them, when several
/// share its extension — and the format its content shows are in `allowed` (terrio or GDAL names).
/// A dataset whose format neither its name nor its content shows is refused: an allow-list fails
/// closed. Nothing is opened with any driver.
///
/// rasterio's `driver=` asks GDAL to try only those drivers, and GDAL's GTiff driver identifies a
/// TIFF by its first bytes, whatever its name: a local TIFF named `a.jp2` opens with
/// `driver="GTiff"`. It is let through here when GTiff is allowed and its name points to
/// no driver that claims a file by its name before reading it (`opened_as_tiff_by_content`), so
/// that terrio's reader is the GeoTIFF one. The process-wide allow-list (`check_allowed`) keeps
/// the name rule.
pub fn check_drivers(path: &str, allowed: &[String]) -> crate::Result<()> {
    let id = identify(path);
    if opened_as_tiff_by_content(&id, allowed) {
        return Ok(());
    }
    check_identified(path, &id, allowed)?;
    #[cfg(remote)]
    check_remote_tile(path, &id, allowed)?;
    Ok(())
}

/// [`check_drivers`] for a remote file named as an SRTM tile and for a driver `allowed` has
/// (`N37W120.img`, Imagine's, as NASADEM names its tiles), whose content [`identify`] does not
/// read: when `allowed` leaves SRTMHGT out, its first request (made by the open anyway, which then
/// finds it in the process-wide cache) tells whether it is a tile, refused as a local one is.
#[cfg(remote)]
fn check_remote_tile(path: &str, id: &Identified, allowed: &[String]) -> crate::Result<()> {
    let location = crate::vfs::Location::parse(path.trim());
    let name = location.file_name();
    if id.by_content.is_some() || !crate::drivers::hgt::is_tile_name(&name) || allowed.iter().any(|d| canonical_driver_name(d) == "SRTMHGT")
    {
        return Ok(());
    }
    let Some(remote) = location.remote() else { return Ok(()) };
    match crate::http::head_and_len(&remote, crate::http::FIRST_READ) {
        Ok((head, len)) if crate::drivers::hgt::is_tile(&name, len, &head) => {
            check_identified(path, &Identified { by_name: id.by_name.clone(), by_content: Some("SRTMHGT".to_string()) }, allowed)
        }
        _ => Ok(()),
    }
}

/// Whether a dataset whose first bytes show a TIFF, with GTiff among the `allowed` drivers, is
/// opened by terrio's GeoTIFF reader whatever its name says: `LazyRaster::open` reads a TIFF by
/// its content unless a driver that claims files by their name (GRIB by its extension, tile
/// archives, Zarr stores, STAC names) takes it first. Its content is known only for local files
/// and archive members.
fn opened_as_tiff_by_content(id: &Identified, allowed: &[String]) -> bool {
    const BY_NAME: [&str; 7] = ["GRIB", "PMTILES", "MBTILES", "ZARR", "STAC", "STACTA", "STACIT"];
    id.by_content.as_deref() == Some("GTIFF")
        && allowed.iter().any(|a| canonical_driver_name(a) == "GTIFF")
        && !id.by_name.iter().any(|d| BY_NAME.contains(&d.as_str()))
}

/// [`check_drivers`] on what [`identify`] or [`identify_bytes`] found for `path`.
fn check_identified(path: &str, id: &Identified, allowed: &[String]) -> crate::Result<()> {
    let allowed: Vec<String> = allowed.iter().map(|n| canonical_driver_name(n)).collect();
    let shown = path.split(['?', '#']).next().unwrap_or(path);
    if !id.by_name.is_empty() && !id.by_name.iter().any(|d| allowed.contains(d)) {
        return Err(crate::Error::Unsupported(format!(
            "'{shown}': the {} driver is not allowed (allowed: {})",
            id.by_name.join("/"),
            allowed.join(", ")
        )));
    }
    if let Some(content) = &id.by_content
        && !allowed.contains(content)
    {
        return Err(crate::Error::Unsupported(format!(
            "'{shown}' holds {content} data, and the {content} driver is not allowed (allowed: {})",
            allowed.join(", ")
        )));
    }
    if id.by_name.is_empty() && id.by_content.is_none() {
        return Err(crate::Error::Unsupported(format!("'{shown}': no allowed driver recognises its format")));
    }
    Ok(())
}

/// [`check_drivers`] against the process-wide allowed drivers of `kind` (see [`allowed_drivers`]),
/// when there are any: by name and by content, every path that opens a file (the whole-file
/// readers dispatch by extension) refusing a name that points to a driver the list leaves out.
pub(crate) fn check_allowed(path: &str, kind: Option<Kind>) -> crate::Result<()> {
    match allowed_drivers(kind) {
        Some(list) => check_identified(path, &identify(path), &list),
        None => Ok(()),
    }
}

/// Refuses `driver` when the process-wide allowed drivers of `kind` leave it out: for a driver
/// that recognises a file by more than its name and first bytes (an SRTM tile, by its name and
/// size, whatever driver its extension names), and for a reader that always opens files with one
/// driver, checked where it opens one. `path` names the dataset in the refusal. Public for the
/// bindings' own readers (Python's `tiff_tags`).
pub fn check_driver_allowed(driver: &str, path: &str, kind: Kind) -> crate::Result<()> {
    match allowed_drivers(Some(kind)) {
        Some(list) if !list.contains(&canonical_driver_name(driver)) => {
            let shown = path.split(['?', '#']).next().unwrap_or(path);
            Err(crate::Error::Unsupported(format!(
                "'{shown}' holds {driver} data, and the {driver} driver is not allowed (allowed: {})",
                list.join(", ")
            )))
        }
        _ => Ok(()),
    }
}

/// [`check_allowed`] for a dataset given as bytes, by its name and its first bytes.
pub(crate) fn check_allowed_bytes(name: &str, bytes: &[u8], kind: Option<Kind>) -> crate::Result<()> {
    match allowed_drivers(kind) {
        Some(list) => check_identified(name, &identify_bytes(name, bytes), &list),
        None => Ok(()),
    }
}

#[cfg(test)]
mod allow_list_tests {
    use super::*;

    fn temp(name: &str, bytes: &[u8]) -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        std::fs::write(&path, bytes).unwrap();
        (dir, path.to_str().unwrap().to_string())
    }

    const VRT: &[u8] = b"<VRTDataset rasterXSize=\"1\" rasterYSize=\"1\"><VRTRasterBand dataType=\"Byte\" band=\"1\">\
<SimpleSource><SourceFilename>/vsicurl/http://127.0.0.1:9/x.tif</SourceFilename></SimpleSource></VRTRasterBand></VRTDataset>";

    #[test]
    fn identifies_by_name_and_content() {
        let (_d, tif) = temp("a.tif", b"II*\0\x08\0\0\0");
        assert_eq!(identify(&tif), Identified { by_name: vec!["GTIFF".into(), "COG".into()], by_content: Some("GTIFF".into()) });
        let (_d, vrt) = temp("disguised.tif", VRT);
        assert_eq!(identify(&vrt).by_content.as_deref(), Some("VRT"));
        assert_eq!(identify("STACIT:\"https://x/search\":asset=red").by_name, vec!["STACIT".to_string()]);
        assert_eq!(identify("https://x.blob.core.windows.net/c/a.TIF?sv=1&sig=2").by_name, vec!["GTIFF".to_string(), "COG".to_string()]);
        assert_eq!(identify("https://x/store.zarr/group/b02").by_name, vec!["ZARR".to_string()]);
    }

    #[test]
    fn allowed_drivers_refuse_pointer_files_and_unknown_formats() {
        let allowed: Vec<String> = ["GTiff", "COG", "JP2OpenJPEG", "PNG", "JPEG"].iter().map(|s| s.to_string()).collect();
        let (_d, tif) = temp("a.tif", b"II*\0\x08\0\0\0");
        assert!(check_drivers(&tif, &allowed).is_ok());
        let (_d, jp2) = temp("a.jp2", b"\0\0\0\x0cjP  \r\n\x87\nrest");
        assert!(check_drivers(&jp2, &allowed).is_ok(), "GDAL's JP2OpenJPEG name allows terrio's JP2");
        for (name, bytes) in [("pointer.vrt", VRT), ("disguised.tif", VRT), ("no_extension", VRT)] {
            let (_d, p) = temp(name, bytes);
            let err = check_drivers(&p, &allowed).unwrap_err().to_string();
            assert!(err.contains("VRT") || err.contains("not allowed"), "{name}: {err}");
        }
        let (_d, unknown) = temp("blob", b"\x00\x01\x02\x03");
        assert!(check_drivers(&unknown, &allowed).is_err(), "unknown formats fail closed");
        assert!(check_drivers("STAC:\"https://x/item.json\":red", &allowed).is_err());
        let with_vrt: Vec<String> = allowed.iter().cloned().chain(["VRT".to_string()]).collect();
        let (_d, vrt) = temp("pointer.vrt", VRT);
        assert!(check_drivers(&vrt, &with_vrt).is_ok(), "a pointer format opens when named");
    }

    /// GDAL's GTiff driver identifies a TIFF by its content, so `driver="GTiff"` opens a
    /// TIFF named `.jp2` (or `.png`). It does not open other content under a TIFF's name, nor a
    /// TIFF a driver outside the list would claim by its name; the process-wide list keeps the
    /// name rule.
    #[test]
    fn a_forced_gtiff_driver_opens_a_tiff_whatever_its_name() {
        let gtiff = vec!["GTiff".to_string()];
        let tiff = b"II*\0\x08\0\0\0".as_slice();
        for name in ["a.jp2", "a.png", "a.JP2", "asset"] {
            let (_d, p) = temp(name, tiff);
            assert!(check_drivers(&p, &gtiff).is_ok(), "{name}");
        }
        // Content the list does not allow, whatever the name.
        let (_d, jp2) = temp("a.tif", b"\0\0\0\x0cjP  \r\n\x87\nrest");
        assert!(check_drivers(&jp2, &gtiff).is_err());
        let (_d, vrt) = temp("a.tif", VRT);
        assert!(check_drivers(&vrt, &gtiff).is_err());
        // A TIFF named for a driver that claims files by name, and a TIFF with another driver forced.
        for name in ["a.grib2", "a.pmtiles", "a.mbtiles"] {
            let (_d, p) = temp(name, tiff);
            assert!(check_drivers(&p, &gtiff).is_err(), "{name}");
        }
        let (_d, p) = temp("a.jp2", tiff);
        assert!(check_drivers(&p, &["JP2OpenJPEG".to_string()]).is_err());
        let (_d, p) = temp("a.jp2", tiff);
        assert!(check_identified(&p, &identify(&p), &gtiff).is_err(), "the process-wide list keeps the name rule");
    }

    /// Finding 17: a member of a zip archive is identified by its content too, as a file is.
    #[test]
    fn allowed_drivers_see_inside_zip_members() {
        let allowed = vec!["GTIFF".to_string()];
        let mut buf = std::io::Cursor::new(Vec::new());
        let mut zip = zip::ZipWriter::new(&mut buf);
        for (name, bytes) in [("x.tif", VRT), ("y.tif", b"II*\0\x08\0\0\0".as_slice())] {
            zip.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut zip, bytes).unwrap();
        }
        zip.finish().unwrap();
        let (_d, archive) = temp("a.zip", &buf.into_inner());
        let err = check_drivers(&format!("/vsizip/{archive}/x.tif"), &allowed).unwrap_err().to_string();
        assert!(err.contains("holds VRT data"), "{err}");
        assert!(check_drivers(&format!("{archive}!x.tif"), &allowed).is_err());
        assert!(check_drivers(&format!("/vsizip/{archive}/y.tif"), &allowed).is_ok(), "a GeoTIFF member is allowed");
    }

    #[test]
    fn bytes_are_identified_by_name_and_content() {
        let allowed = vec!["GTIFF".to_string()];
        let tif = b"II*\0\x08\0\0\0".as_slice();
        assert!(check_identified("a.tif", &identify_bytes("a.tif", tif), &allowed).is_ok());
        let err = check_identified("a.tif", &identify_bytes("a.tif", VRT), &allowed).unwrap_err().to_string();
        assert!(err.contains("holds VRT data"), "a disguised pointer file: {err}");
        assert!(check_identified("a.png", &identify_bytes("a.png", tif), &allowed).is_err(), "named PNG");
        assert!(check_identified("blob", &identify_bytes("blob", b"\x00\x01"), &allowed).is_err(), "unknown fails closed");
    }

    /// A Surfer 6 grid (`DSBB`), which terrio's reader takes by its content whatever its name, is
    /// identified so (`sniff` knew no Surfer signature, so a grid named `x.tif` passed as a TIFF's
    /// name alone): refused before it is opened unless GSBG is allowed, by `driver=` and by the
    /// process-wide lists, from a file or bytes.
    #[test]
    fn a_surfer_6_grid_is_identified_by_its_content() {
        let grid = [b"DSBB".as_slice(), &[0u8; 52]].concat();
        assert_eq!(sniff(&grid), Some("GSBG"));
        let (_d, tif) = temp("x.tif", &grid);
        assert_eq!(identify(&tif), Identified { by_name: vec!["GTIFF".into(), "COG".into()], by_content: Some("GSBG".into()) });
        let err = check_drivers(&tif, &["GTiff".to_string()]).unwrap_err().to_string();
        assert_eq!(err, format!("'{tif}' holds GSBG data, and the GSBG driver is not allowed (allowed: GTIFF)"));
        check_drivers(&tif, &["GTiff".to_string(), "GSBG".to_string()]).unwrap();
        let err = check_identified("x.png", &identify_bytes("x.png", &grid), &["PNG".to_string()]).unwrap_err().to_string();
        assert_eq!(err, "'x.png' holds GSBG data, and the GSBG driver is not allowed (allowed: PNG)");
    }

    #[test]
    fn the_option_restricts_drivers_on_its_thread() {
        assert_eq!(allowed_drivers(None), None);
        crate::config::push_scope(std::collections::HashMap::from([(
            "TERRIO_ALLOWED_DRIVERS".to_string(),
            Some("GTiff, JP2OpenJPEG".to_string()),
        )]));
        assert_eq!(allowed_drivers(None), Some(vec!["GTIFF".to_string(), "JP2".to_string()]));
        assert_eq!(
            allowed_drivers(Some(Kind::Vector)),
            Some(vec!["GTIFF".to_string(), "JP2".to_string()]),
            "the option applies to every kind"
        );
        let (_d, vrt) = temp("pointer.vrt", VRT);
        assert!(check_allowed(&vrt, Some(Kind::Raster)).is_err());
        crate::config::pop_scope();
        assert!(check_allowed(&vrt, Some(Kind::Raster)).is_ok(), "no restriction without the option");
    }

    // The process-wide lists last for the process, so these tests run the narrowing on lists of
    // their own; `tests/allow_list.rs` checks the process-wide wiring in a process of its own.

    fn list(names: &[&str]) -> Option<Vec<String>> {
        Some(names.iter().map(|n| n.to_string()).collect())
    }

    #[test]
    fn a_later_broader_call_narrows_and_never_re_enables() {
        let mut lists: AllowLists = [None, None];
        narrow_allow_lists(&mut lists, Some(&["GTiff", "COG", "PNG"]), Some(Kind::Raster)).unwrap();
        assert_eq!(lists, [list(&["GTIFF", "COG", "PNG"]), None], "the first call sets the raster list only");
        narrow_allow_lists(&mut lists, Some(&["GTiff", "PNG", "VRT", "JPEG"]), Some(Kind::Raster)).unwrap();
        assert_eq!(lists[0], list(&["GTIFF", "PNG"]), "VRT and JPEG are not re-enabled; COG is dropped");
        narrow_allow_lists(&mut lists, Some(&["GeoJSON", "ESRI Shapefile", "GTiff"]), None).unwrap();
        assert_eq!(lists, [list(&["GTIFF"]), list(&["GEOJSON", "ESRI SHAPEFILE", "GTIFF"])], "None narrows both kinds");
        narrow_allow_lists(&mut lists, Some(&[]), Some(Kind::Vector)).unwrap();
        assert_eq!(lists, [list(&["GTIFF"]), list(&[])], "an empty list refuses every driver of its kind");
    }

    #[test]
    fn lifting_a_set_list_is_refused_and_changes_nothing() {
        let mut lists: AllowLists = [None, None];
        narrow_allow_lists(&mut lists, None, None).unwrap();
        assert_eq!(lists, [None, None], "lifting no list is a no-op");
        narrow_allow_lists(&mut lists, Some(&["GeoJSON"]), Some(Kind::Vector)).unwrap();
        narrow_allow_lists(&mut lists, None, Some(Kind::Raster)).unwrap();
        for kind in [Some(Kind::Vector), None] {
            let err = narrow_allow_lists(&mut lists, None, kind).unwrap_err().to_string();
            assert!(err.contains("cannot be lifted"), "{err}");
        }
        assert!(narrow_allow_lists(&mut lists, Some(&["GTiff"]), Some(Kind::PointCloud)).is_err());
        assert_eq!(lists, [None, list(&["GEOJSON"])]);
    }

    #[test]
    fn a_dataset_of_unknown_kind_needs_every_kind_of_its_driver() {
        // GPKG serves rasters and vectors: dropped from either list, it is gone for both.
        let lists: AllowLists = [list(&["GTIFF", "PNG", "VRT"]), None];
        let both = allowed_by_every_kind(&lists).unwrap();
        for name in ["GTIFF", "PNG", "VRT", "GEOJSON", "ESRI SHAPEFILE"] {
            assert!(both.contains(&name.to_string()), "{name}");
        }
        for name in ["GPKG", "JPEG", "NETCDF", "MBTILES", "STACIT"] {
            assert!(!both.contains(&name.to_string()), "{name}");
        }
        let lists: AllowLists = [list(&["GTIFF", "GPKG", "MEM"]), list(&["GEOJSON"])];
        let both = allowed_by_every_kind(&lists).unwrap();
        assert!(both.contains(&"GTIFF".to_string()) && both.contains(&"GEOJSON".to_string()));
        assert!(!both.contains(&"GPKG".to_string()), "the vector list drops GPKG");
        assert!(!both.contains(&"MEM".to_string()), "a name of neither kind needs both lists");
        assert_eq!(allowed_by_every_kind(&[None, None]), None);
    }
}

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
    /// XYZ ASCII grid.
    XYZ,
    /// Zarr v2 and v3.
    Zarr,
    /// DTED elevation.
    DTED,
    /// NetCDF: classic and netCDF-4 read, classic (CDF-1, CDF-2, CDF-5) written.
    NetCDF,
    /// GDAL virtual raster (read by [`crate::ops::vrt`]; written by mosaics).
    VRT,
    /// A mosaic of the assets of STAC items (see [`crate::stac`]); read only.
    STACIT,
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
    /// MapInfo interchange format.
    MapInfo,
    /// OpenStreetMap PBF and XML.
    OSM,
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
        Self::XYZ,
        Self::Zarr,
        Self::DTED,
        Self::NetCDF,
        Self::VRT,
        Self::STACIT,
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
            Self::XYZ => ("XYZ", "XYZ ASCII Grid", &["xyz"], true, true),
            Self::Zarr => ("Zarr", "Zarr v2/v3", &["zarr"], true, true),
            Self::DTED => ("DTED", "DTED Elevation", &["dt0", "dt1", "dt2"], true, true),
            Self::NetCDF => ("netCDF", "Network Common Data Format", &["nc", "nc4", "cdf"], true, true),
            Self::VRT => ("VRT", "Virtual Raster", &["vrt"], true, true),
            Self::STACIT => ("STACIT", "Spatio-Temporal Asset Catalog Items", &[], true, false),
            Self::MBTiles => ("MBTiles", "MBTiles", &["mbtiles"], true, true),
            Self::PMTiles => ("PMTiles", "ProtoMap Tiles", &["pmtiles"], true, true),
            Self::Gdal => ("GDAL", "Any raster GDAL reads (optional backend)", &[], cfg!(feature = "gdal"), false),
        };
        DriverInfo { name, long_name, kind: Kind::Raster, extensions, read, write }
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
            "imagine" | "erdas" => Some(Self::HFA),
            "jpg" => Some(Self::JPEG),
            "geopackage" => Some(Self::GPKG),
            "nc" | "netcdf4" => Some(Self::NetCDF),
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
            // Never used: terrio reads and writes NetCDF itself.
            Self::NetCDF => F::Zarr,
            // Never used: terrio reads and writes VRTs and tile archives, and reads STAC mosaics
            // itself; GDAL reads its formats.
            Self::VRT | Self::STACIT | Self::MBTiles | Self::PMTiles | Self::Gdal => F::GeoTiff,
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
        Self::TopoJSON,
        Self::GPKG,
        Self::Parquet,
        Self::FlatGeobuf,
        Self::Shapefile,
        Self::KML,
        Self::KMZ,
        Self::GML,
        Self::GPX,
        Self::MapInfo,
        Self::OSM,
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
            Self::TopoJSON => ("TopoJSON", "TopoJSON", &["topojson"], true, true),
            Self::GPKG => ("GPKG", "GeoPackage", &["gpkg"], true, true),
            Self::Parquet => ("Parquet", "GeoParquet", &["parquet", "geoparquet"], true, true),
            Self::FlatGeobuf => ("FlatGeobuf", "FlatGeobuf", &["fgb"], true, true),
            Self::Shapefile => ("ESRI Shapefile", "Esri Shapefile", &["shp"], true, true),
            Self::KML => ("KML", "Keyhole Markup Language", &["kml"], true, true),
            Self::KMZ => ("KMZ", "Zipped KML", &["kmz"], true, true),
            Self::GML => ("GML", "Geography Markup Language", &["gml"], true, true),
            Self::GPX => ("GPX", "GPS Exchange Format", &["gpx"], true, true),
            Self::MapInfo => ("MapInfo File", "MapInfo TAB and Interchange Format", &["mif", "tab"], true, true),
            Self::OSM => ("OSM", "OpenStreetMap PBF and XML", &["pbf", "osm"], true, false),
            Self::CSV => ("CSV", "Comma Separated Value", &["csv", "tsv"], true, true),
            Self::MBTiles => ("MBTiles", "MBTiles", &["mbtiles"], true, true),
            Self::PMTiles => ("PMTiles", "ProtoMap Tiles", &["pmtiles"], true, true),
            Self::MVT => ("MVT", "Mapbox Vector Tiles", &["mvt"], true, true),
            Self::Gdal => ("GDAL", "Any vector format GDAL reads (optional backend)", &[], cfg!(feature = "gdal"), false),
        };
        DriverInfo { name, long_name, kind: Kind::Vector, extensions, read, write }
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
            Self::Parquet => F::GeoParquet,
            Self::FlatGeobuf => F::FlatGeobuf,
            Self::Shapefile => F::Shapefile,
            Self::KML => F::Kml,
            Self::KMZ => F::Kmz,
            Self::GML => F::Gml,
            Self::GPX => F::Gpx,
            Self::MapInfo => F::MapInfoMif,
            Self::OSM => F::OsmPbf,
            Self::CSV | Self::MBTiles | Self::PMTiles | Self::MVT | Self::Gdal => return None,
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
    ];
    RasterDriver::ALL.iter().map(|d| d.info()).chain(VectorDriver::ALL.iter().map(|d| d.info())).chain(point_clouds).collect()
}

/// The lowercase extension of a path.
pub(crate) fn extension(path: &str) -> Option<String> {
    Path::new(path.trim_end_matches(['/', '\\'])).extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase())
}

# Acknowledgements

terrio stands on the work of many projects and their authors. Thank you to all of them.

!!! note "Disclaimer"

    terrio is primarily built with [Claude Opus 5.5](https://www.anthropic.com/claude), an AI
    model by Anthropic, working under human direction and review. Its results are tested against
    GDAL, PROJ and PDAL, but check them for your own use, and please
    [report](https://github.com/opengeos/terrio/issues) anything that looks wrong.

## Whitebox

terrio began on the pure-Rust format libraries of
[whitebox_next_gen](https://github.com/jblindsay/whitebox_next_gen) by John Lindsay:
[`wbraster`](https://crates.io/crates/wbraster), [`wbvector`](https://crates.io/crates/wbvector) and
[`wbprojection`](https://crates.io/crates/wbprojection). They still read and write many of the
formats terrio supports, and they made a C-free geospatial library in Rust a realistic goal.

## GDAL, PROJ and PDAL

[GDAL](https://gdal.org), [PROJ](https://proj.org) and [PDAL](https://pdal.io) are the reference
terrio measures itself against. Its commands, options and outputs follow GDAL's and PDAL's, and its
tests compare its results with theirs, pixel for pixel and point for point.

- The map projections in `crates/terrio-crs/src/` are ported from PROJ (MIT licence). The
  ellipsoidal gnomonic, as PROJ's, uses Charles Karney's geodesic algorithms, from
  [`geographiclib-rs`](https://crates.io/crates/geographiclib-rs) (MIT licence), a port of
  [GeographicLib](https://geographiclib.sourceforge.io).
- The EPSG table, with the coordinate operations GDAL uses for each CRS, is generated with GDAL and
  PROJ by `scripts/gen_epsg_table.py`, the ESRI table, from PROJ's ESRI authority, by
  `scripts/gen_esri_table.py`, and the datum names by `scripts/gen_datum_names.py`.
- The point cloud filters follow PDAL's (`filters.csf`, `filters.pmf`, `filters.elm`,
  `filters.hag_nn`, `filters.normal` and others); the cloth simulation filter is a port of the
  [CSF](https://github.com/jianbingshen/CSF) code PDAL vendors (Apache-2.0, copyright the State
  Key Laboratory of Remote Sensing Science, Beijing Normal University), after Zhang et al. (2016),
  *An Easy-to-Use Airborne LiDAR Data Filtering Method Based on Cloth Simulation*.

## The EPSG dataset and Esri's projection data

terrio's EPSG table and datum names (`crates/terrio-crs/data/epsg.tsv.zst` and `datums.tsv.zst`) are
derived from the [EPSG Geodetic Parameter Dataset](https://epsg.org), copyright the International
Association of Oil & Gas Producers (IOGP), whose [terms of use](https://epsg.org/terms-of-use.html)
permit their redistribution with this attribution.

Its ESRI table (`crates/terrio-crs/data/esri.tsv.zst`) and the ESRI datum names are derived, through
PROJ's database, from Esri's
[projection engine data](https://github.com/Esri/projection-engine-db-doc), copyright Esri,
under the Apache License 2.0.

## Python APIs

terrio's Python package borrows its shape from [rasterio](https://rasterio.readthedocs.io),
[xarray](https://xarray.dev) and [rioxarray](https://corteva.github.io/rioxarray), so that code
written for them carries over. `terrio.rio` follows rasterio's API and several of its algorithms
(BSD-3-Clause, copyright Mapbox), and `terrio.xarray` is adapted from rioxarray (Apache-2.0).

## Pure-Rust codecs and format crates

terrio reads and writes its formats with these crates, among others:

| Crate | For |
|---|---|
| [`tiff`](https://crates.io/crates/tiff), [`png`](https://crates.io/crates/png), [`image-webp`](https://crates.io/crates/image-webp), [`weezl`](https://crates.io/crates/weezl) ([image-rs](https://github.com/image-rs)) | TIFF, PNG and WebP, LZW |
| [`jpeg-decoder`](https://crates.io/crates/jpeg-decoder), [`jpeg-encoder`](https://crates.io/crates/jpeg-encoder) | JPEG |
| [`openjp2`](https://crates.io/crates/openjp2) | JPEG 2000, a port of [OpenJPEG](https://www.openjpeg.org) |
| [`rusty_av1d`](https://crates.io/crates/rusty_av1d) | AVIF, a port of [dav1d](https://code.videolan.org/videolan/dav1d) |
| [`jxl-oxide`](https://crates.io/crates/jxl-oxide), [`zune-jpegxl`](https://crates.io/crates/zune-jpegxl) | JPEG XL |
| [`lerc-reader`](https://crates.io/crates/lerc-reader), [`lerc-writer`](https://crates.io/crates/lerc-writer) | LERC |
| [`flate2`](https://crates.io/crates/flate2), [`ruzstd`](https://crates.io/crates/ruzstd), [`zstd`](https://crates.io/crates/zstd), [`lz4_flex`](https://crates.io/crates/lz4_flex), [`lzma-rust2`](https://crates.io/crates/lzma-rust2), [`zip`](https://crates.io/crates/zip) | Compression and archives |
| [`flatgeobuf`](https://crates.io/crates/flatgeobuf) | FlatGeobuf |
| [Apache Arrow and Parquet](https://github.com/apache/arrow-rs) | GeoParquet and Arrow |
| [`netcdf-reader`](https://crates.io/crates/netcdf-reader), [`hdf5-reader`](https://crates.io/crates/hdf5-reader) | netCDF and HDF5 |
| [`osmpbfreader`](https://crates.io/crates/osmpbfreader) | OpenStreetMap PBF |
| [`quick-xml`](https://crates.io/crates/quick-xml) | KML, GPX and other XML formats |
| [`laz`](https://crates.io/crates/laz) | LAZ and COPC, a port of [LASzip](https://laszip.org) |
| [`e57`](https://crates.io/crates/e57) | E57 laser scans |
| [`delaunator`](https://crates.io/crates/delaunator) | Delaunay triangulation |

## Tooling

[clap](https://crates.io/crates/clap) parses the command line, [rayon](https://crates.io/crates/rayon)
runs it in parallel, [ureq](https://crates.io/crates/ureq) fetches remote data,
[serde](https://serde.rs) handles JSON, [PyO3](https://pyo3.rs) and
[rust-numpy](https://github.com/PyO3/rust-numpy) build the Python package, and
[wasm-bindgen](https://github.com/rustwasm/wasm-bindgen) builds the JavaScript one. The optional
`gdal` feature uses the [`gdal`](https://crates.io/crates/gdal) crate.

JPEG writing (through `jpeg-encoder`) is based in part on the work of the Independent JPEG
Group.

## Notices and licenses

[`NOTICE`](https://github.com/opengeos/terrio/blob/main/NOTICE) holds the copyright and license
notices of the code and data terrio adapts (PROJ, CSF, the EPSG dataset, Esri's projection data,
rasterio, rioxarray, Apache Arrow and the IJG), and
[`LICENSE-THIRD-PARTY.md`](https://github.com/opengeos/terrio/blob/main/LICENSE-THIRD-PARTY.md)
the licenses of every Rust crate terrio's packages and binaries are built with, generated with
[cargo-about](https://github.com/EmbarkStudios/cargo-about). Both ship with the Python wheels,
the npm package and the command-line archives.

The full list of dependencies is in `Cargo.toml` and `Cargo.lock`. If your work is missing here,
please [open an issue](https://github.com/opengeos/terrio/issues).

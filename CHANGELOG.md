# Changelog

## Unreleased

### Terrain

- `terrio dem color-relief` and Python's `terrio.dem(path, ..., dst=path)` read the DEM and write
  the output a strip of rows at a time, as `terrio dem` already did for the other products
  (Rust: `color_relief_to_path`, and `ColorMap::parse_lazy` for color files with percentages).
  A path-to-path `terrio.dem` now returns the result opened lazily from `dst` instead of in
  memory; the pixels are unchanged. On a 15,000 x 12,000 Float32 DEM, a color relief with alpha
  peaks at about 560 MiB (160 MiB with `max_memory_mib=256`) instead of 7,569 MiB.
- `--max-memory` (CLI), `max_memory_mib` (Python) and `DemOptions::max_memory_mib` set the strip
  height from a memory budget, like `gdalwarp -wm` (by default, strips of about 64 MiB of input
  values, as before). The values do not depend on it.
- The in-memory `color_relief` colors the band directly into the Byte output, in parallel,
  instead of holding a Float64 copy of the band and four Float64 output planes.
- `dem_to_path` and `color_relief_to_path` read a DEM that is also the output whole first,
  instead of truncating it while it is read.
- On Unix, an output that is a hard link to an input now counts as that input wherever an output
  may replace its input: the DEM paths, `warp`, `mosaic`, `LazyRaster::write` and pansharpening
  read the input whole first (or refuse, for a VRT output), as for the same path. On Windows, a
  hard link is not detected.
- `terrio dem` names `--max-memory`, not `max_memory_mib`, when it refuses a budget for an output
  that is a hard link to the input.

### Formats

- GeoJSON and GeoJSONSeq outputs write a field as each feature's `id` member with GDAL's
  `ID_FIELD` and `ID_TYPE` creation options (`--lco`, `GeoJsonOptions::id_field` and
  `id_type`), so the string identifiers read from a GeoJSON `id` go back out as RFC 7946 `id`
  members. `GeoJsonOptions` is no longer `Copy`.
- KMZ archives can carry files such as the icon images of `SYMBOL` styles (#394):
  `VectorWriteOptions::kmz` (`KmzOptions::resources`), `terrio convert --kmz-resource`, Python's
  `kmz_resources=` and JavaScript's `kmzResources` pack them beside the layer documents, where
  relative icon links resolve.
- HEC-RAS geometry (`.g##.hdf`) and plan (`.p##.hdf`) HDF files are read as
  vector layers (the `HECRAS` driver, the `hecras` feature, on by default), through the
  [`ras-commander-fast-hdf`](https://crates.io/crates/ras-commander-fast-hdf)
  engine: 2D mesh cells, cell centres, faces, face points and perimeters (with
  the stored 2D flow area table); cross sections (with station-elevation,
  Manning's n and ineffective areas as JSON fields), river lines, storage
  areas, structures, boundary-condition lines, breaklines, refinement regions,
  reference lines and points, junctions, pump stations, and pipe conduits and
  nodes, each with its stored attribute columns. Stored results join through
  open options (summary maxima and minima with their times, time-step values,
  series extremes, cross-section, structure, boundary and reference results).
  The CRS is the stored `Projection` WKT. A file is claimed only when its root
  `File Type` is `HEC-RAS Geometry` or `HEC-RAS Results`; other HDF5 and
  NetCDF files are not affected. Compared with h5py on the HEC-RAS 7.0.1 files
  of a public corpus: every value identical. See `docs/hecras.md`.
- `hdf5-reader` and `netcdf-reader` are the crates.io 0.10.1 releases; the
  `[patch]` to a netcdf-rust branch is gone (its fix, roteiro-gis/netcdf-rust#95,
  is in 0.10). The HEC-RAS engine uses the same `hdf5-reader`, so a build
  compiles one HDF5 reader.

- SRTM HGT tiles are read and written (the `SRTMHGT` driver), as GDAL's SRTMHGT
  driver does: 3", 1" and 0.5" tiles georeferenced from their name
  (`N37W120.hgt`), `.hgt.zip` archives, and the NASADEM and water body variants.
  Tiles are written as Int16 with -32768 for nodata; a grid of another size is
  refused.
- SRTM HGT tiles and the other raw-layout rasters (EHdr, ENVI, SAGA, ERS,
  Idrisi, Surfer 6 binary grids) are read in place (#333): `open_raster_lazy`
  reads only the header, a window reads only the rows it covers, and `info`
  decodes no pixels. Remote SRTM tiles are read with HTTP range requests, and
  `convert` to a GeoTIFF or COG writes these inputs a strip at a time.
- Lazy GeoTIFF reads keep the tile or strip offsets and byte counts of each
  image (full resolution or overview) after its first window read (#348): later
  windows of the image no longer read and decode both tables again, nor does
  terrio's own codec (LZW and the other compressions it decodes) read them a
  second time, and mask reads no longer copy the mask's tables. Windows decoded
  on the calling thread (one chunk, or remote and sparse files) no longer cost
  more as the image has more tiles; the threads decoding the chunks of larger
  windows of local files still each read the tables, as does moving to another
  overview. The tables kept take 16 bytes per chunk of each image read.

### GeoPackage

- `terrio gpkg-repack` (CLI, and the `gpkg-repack` MCP tool) compacts a native GeoPackage in
  place, rebuilding every B-tree from its encoded records and preserving rowids, SQL objects,
  text encodings, rasters and auto-vacuum pointer maps. Python `gpkg_repack(path)` and
  `gpkg_repack_bytes(data)`, and JavaScript `gpkgRepack` and `gpkgRepackBytes`, expose the same
  operation. Scope and storage safety are in [docs/cli.md](docs/cli.md#geopackage-repacking).

### Remote I/O

- Signed S3 uploads of local files over 8 MiB sent over HTTPS read the file once
  per attempt instead of twice (#335), as a retried, redirected or
  region-rerouted request reads it again: they are signed with
  `UNSIGNED-PAYLOAD`, as the AWS SDKs sign S3 uploads over HTTPS, and the body
  is streamed. The signature no longer covers the body of these uploads; TLS
  protects it in transit. Over plain HTTP, and for smaller or staged uploads,
  the payload hash is signed as before.

### Breaking changes

- Dataset/VSI paths retain leading and trailing whitespace across CLI, Python, JavaScript
  and MCP (#372). Native filenames and virtual keys with edge whitespace are distinct from their
  trimmed neighbors; callers must normalize pasted paths/URLs themselves. `@FILE` lines
  still apply their file-list whitespace rules.
- Native Unix SQLite in-place append/update and recovery propagate parent-directory open/fsync
  failures. Filesystems without directory fsync support can fail before page writes. An error
  after journal deletion reports committed changes or completed recovery with unconfirmed
  durability; do not blindly retry an append. Native append also refuses linked/special
  journal, WAL and shared-memory sidecars before recovery, and refuses a database file with
  hard-link aliases (Unix `nlink > 1`).
- VRT `SourceFilename` values retain trailing whitespace and newlines, matching GDAL.
  Remove formatting whitespace from inside the element when it is not part of the filename;
  encoded leading whitespace and CDATA remain significant.
- Signed S3 uploads of local files over 8 MiB over HTTPS use `UNSIGNED-PAYLOAD`
  (#335). An S3-compatible service, or a bucket policy, that refuses it now
  rejects those uploads; they can be sent over plain HTTP to keep a signed
  payload, with `AWS_HTTPS=NO`, or an `http://` `AWS_S3_ENDPOINT` when the
  endpoint names its scheme (an `https://` endpoint ignores `AWS_HTTPS`). Plain
  HTTP does not encrypt the request body or headers, including
  `x-amz-security-token` for temporary credentials; use this workaround only
  over a trusted connection.
- A path-to-path Python `terrio.dem(src_path, ..., dst=path)` (other than to a VRT) returns the
  result opened lazily from `dst` instead of in memory: the returned `Raster` reads `dst`, which
  must stay in place and unchanged while the `Raster` is in use.
- `DemOptions` has a new public field, `max_memory_mib`: struct literals need it or
  `..Default::default()`.
- `mosaic`, `warp`, `concat` and `filter merge` now treat an input beginning
  with `@` as a file list, so a file named `@name.tif` must be written
  `./@name.tif`.
- WKT2 `Goode Homolosine` (PROJ's uninterrupted `goode`) is refused; it was read
  as `igh`.

### Command line

- `mosaic`, `warp`, `concat` and `filter merge` read inputs from a file list:
  `@FILE` stands for the paths listed in FILE, one per line, as GDAL's
  `--input @filelist.txt` does (#323). MCP tool calls refuse `@FILE`, so that a
  client cannot read a file back through the errors naming its lines.
- `validate_cog` (Rust, Python) and `terrio validate-cog` (and MCP) check that a GeoTIFF is a
  Cloud Optimized GeoTIFF with the checks and messages of rio-cogeo's `cog_validate` (GDAL's
  `validate_cloud_optimized_geotiff.py`), reading only headers and chunk offsets: local files,
  `/vsizip/` paths, and HTTP(S) or S3 URLs by range requests. The command exits with status 1
  when the file is not a valid COG.
- Dataset file operations (`dataset-copy`, `dataset-rename`, `dataset-delete` and
  `dataset-files`) use paths and URLs exactly
  as given, across CLI, Python, JavaScript and MCP. Native filenames and virtual keys with
  edge whitespace are distinct from their trimmed neighbors, so callers must trim pasted
  paths/URLs themselves.

### Point clouds

- PDAL's `filters.hexbin`: the boundary of points on a grid of hexagons (the hexagons with at
  least a threshold of points, as polygons with holes), with its area, density and point
  spacing, and the hexagons with their counts. `terrio filter hexbin` (and MCP) writes them as
  vector datasets, reading the points in chunks; Python's `PointCloud.hexbin` returns PDAL's
  metadata (`boundary` as WKT, `boundary_json`...) with both as layers; JavaScript's
  `PointCloud.hexbin` returns both as a vector dataset; Rust has `pointcloud::filters::hexbin`
  and `HexBinner`. PDAL's options (`edge_length`/`edge_size` or its estimate from
  `sample_size` points, `threshold`, `smooth`, `hole_cull_area_tolerance`, `precision`) and a
  grid `origin`; H3 grids are not implemented. Hexagons, counts and boundaries (smoothed or not)
  are identical to PDAL 2.10's on autzen; points are assigned to hexagons as hexer assigns them
  (a port, attributed in `NOTICE`).
- Reprojection (`terrio convert --t-srs`, `convert_points(t_srs=...)`,
  `PointCloud.reproject`, `append_points`) now transforms `z` when the points'
  CRS and the target CRS both have heights, as PDAL's `filters.reprojection`
  does: vertical units, geoid models and vertical datum transformations apply.
  It previously transformed only `x` and `y`, so 300 ftUS in
  `EPSG:6588+6360` stayed 300 in `EPSG:6588+5703` (now 91.44018 m). The
  vertical CRS a LAS file declares is now part of the CRS reprojected from
  (it was dropped) unless a CRS is assigned with `--a-srs`, and LAS files
  written with GeoKeys record a compound CRS's vertical CRS in
  `VerticalCSTypeGeoKey`. When only the target has heights, `z` is kept and a
  warning says so.

### CRS compatibility

- Transformer construction now checks celestial-body compatibility for all CRS
  pairs, including custom PROJ definitions without IAU identifiers. Different
  known bodies are rejected. When either body is unidentified, semi-major axes
  must agree within 0.7% of the target radius. For example, `+R=1` to `+R=2`, or a
  lunar/custom small sphere to EPSG:4326, now returns an error; matching custom
  figures remain supported.
- Earth datum operations (`+datum`, `+towgs84`, or datum grids) require an Earth
  figure, including for otherwise matching unidentified custom radii. They are
  rejected on planetary/custom small figures rather than interpreted as valid
  Earth shifts. This intentionally also applies without IAU identifiers. There
  is no `PROJ_IGNORE_CELESTIAL_BODY` override in Rust, CLI/MCP, Python or
  JavaScript. Use a CRS on the correct body; changes of arbitrary coordinate
  units should use the applicable CRS unit parameters.
- Geographic WKT2 definitions with west- or south-positive axes now preserve
  those directions and their declared order, including Earth CRSs. They were
  previously treated as ordinary east/north coordinates. This matches GDAL's
  traditional GIS axis strategy; ordinary north/east Earth axes still use
  longitude, latitude order.
- Vertical CRSs without geoid models of their own now use those of the vertical
  CRSs of their datum, as PROJ does. NAVD88 height in US survey feet
  (EPSG:6360) or feet (EPSG:8228) and NAVD88 depth (EPSG:6357, EPSG:6358) go
  through GEOID18 like NAVD88 height (EPSG:5703), in both directions, with the
  unit and direction converted. They previously only converted the unit,
  an error of the geoid undulation (about 21 to 28 m across Texas):
  `EPSG:6318+6360` to `EPSG:6319` at 30 N, 97 W turns 328.0833 ftUS
  (100 m) into 73.4074 m, as PROJ 9.9.0, instead of 100 m.

### Projections

- `+proj=igh` (interrupted Goode homolosine) is computed as PROJ does; it was
  refused. GDAL's WKT1 `Interrupted_Goode_Homolosine`, ESRI's
  `Goode_Homolosine` with `Option` 1 and WKT2 `Interrupted Goode Homolosine`
  are read, and the first is written.
- `+proj=ob_tran` with an `+o_proj` other than `longlat` (a projection on a
  rotated sphere or ellipsoid) is computed as PROJ does; it was refused. The
  pole may be given by `+o_lat_p`/`+o_lon_p`, by `+o_lon_c`/`+o_lat_c`/`+o_alpha`
  or by `+o_lon_1`/`+o_lat_1`/`+o_lon_2`/`+o_lat_2`, for rotated-pole grids too.
  PROJ's WKT2 `PROJ ob_tran o_proj=...` method is read.
- `+proj=qsc` (quadrilateralized spherical cube) is computed as PROJ does, on
  each of the six cube faces and on a sphere or an ellipsoid; it was refused.
  GDAL's and ESRI's WKT1 `Quadrilateralized_Spherical_Cube` and WKT2
  `Quadrilateralized Spherical Cube` are read, and the WKT1 form is written.
- `+proj=healpix` and `+proj=rhealpix` are computed as PROJ does, with
  `+rot_xy` and `+north_square`/`+south_square`; they were refused. On an
  ellipsoid they use the authalic latitude, as PROJ 9.8.1. GDAL's WKT1 (which
  carries the PROJ string), ESRI's `PROJ_healpix` and `PROJ_rhealpix` and WKT2
  `PROJ healpix` and `PROJ rhealpix` are read, and the first is written.
- `+proj=isea` is computed as PROJ 9.8.1 does, with `+orient`, `+azi`, `+lon_0`,
  `+lat_0`, `+resolution` and `+aperture`; it was refused. As in PROJ 9.8.1, the
  inverse is defined for the default grid in its two orientations only, and on an
  ellipsoid the forward takes the geodetic latitude on the semi-major axis while
  the inverse uses the authalic radius. `+mode=di`, `dd` and `hex` (grid
  addresses) are refused. GDAL's WKT1 (which carries the PROJ string), ESRI's
  `PROJ_isea` and WKT2 `PROJ isea` are read, and the first is written.
- WKT2 `PROJ name` methods take only the projection's string-valued parameters
  in the name (`o_proj` for `ob_tran`, `orient` and `mode` for `isea`), and refuse
  `R_C`, `geoc`, `vunits` and `vto_meter` among their PARAMETERs, as they refused
  the figure, datum and units before.

### Warp

- With bilinear, cubic, cubic spline and Lanczos, an output pixel whose source
  pixel under the sample point is nodata (or cut out) is now left unwritten
  (at the output's nodata value, when it has one) for a single-band source too,
  as GDAL does: it took a value renormalised over the valid neighbours, which
  moved the edge of the data about a pixel into nodata areas. Several bands
  already behaved so, with unified nodata.

### Shapefile

- The rings of polygon records can be organized by any of the methods of GDAL's
  `OGR_ORGANIZE_POLYGONS`, chosen with the `ORGANIZE_POLYGONS` open option (`--oo`,
  `VectorOpenOptions::shapefile`) or the `OGR_ORGANIZE_POLYGONS` (or `TERRIO_ORGANIZE_POLYGONS`)
  environment variable. `DEFAULT` groups rings by containment, whatever their winding, so files
  that wind every ring one way keep their holes. `ONLY_CCW` stays the default, as in GDAL. With
  every method, the self-touching rings below are split first, as GDAL 3.13 does.
- In polygon records of several parts, rings that touch themselves at a vertex
  to enclose a hole, which Esri allows, are now split into an exterior and
  holes when read, wherever the stored ring starts, as GDAL 3.13 does (GDAL
  3.12 keeps them as stored). A ring whose loops are not holes (a figure-eight,
  a loop outside the ring, a spike), and a record of a single ring, are still
  read as stored, also as GDAL does. The split rings start at the vertex the
  rings GEOS rebuilds for GDAL start at. The `shapefile-rings` conformance
  suite compares 18 such cases with GDAL.
- A hole whose first vertex lies on the exterior now belongs to that exterior.
  It was read as a polygon of its own.

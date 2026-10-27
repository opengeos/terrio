# Changelog

## Unreleased

### Formats

- SRTM HGT tiles are read and written (the `SRTMHGT` driver), as GDAL's SRTMHGT
  driver does: 3", 1" and 0.5" tiles georeferenced from their name
  (`N37W120.hgt`), `.hgt.zip` archives, and the NASADEM and water body variants.
  Tiles are written as Int16 with -32768 for nodata; a grid of another size is
  refused.

### Breaking changes

- `mosaic`, `warp`, `concat` and `filter merge` now treat an input beginning
  with `@` as a file list, so a file named `@name.tif` must be written
  `./@name.tif`.

### Command line

- `mosaic`, `warp`, `concat` and `filter merge` read inputs from a file list:
  `@FILE` stands for the paths listed in FILE, one per line, as GDAL's
  `--input @filelist.txt` does (#323). MCP tool calls refuse `@FILE`, so that a
  client cannot read a file back through the errors naming its lines.

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

### Shapefile

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

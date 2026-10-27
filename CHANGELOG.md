# Changelog

## Unreleased

### Formats

- SRTM HGT tiles are read and written (the `SRTMHGT` driver), as GDAL's SRTMHGT
  driver does: 3", 1" and 0.5" tiles georeferenced from their name
  (`N37W120.hgt`), `.hgt.zip` archives, and the NASADEM and water body variants.
  Tiles are written as Int16 with -32768 for nodata; a grid of another size is
  refused.

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

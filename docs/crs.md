# Coordinate Systems

## Defining a CRS

Everywhere a CRS is expected (`--t-srs`, `--a-srs`, `terrio crs`, the APIs), terrio accepts:

- EPSG codes: `EPSG:32613`, `32613`, `urn:ogc:def:crs:EPSG::32613`,
  `http://www.opengis.net/def/crs/EPSG/0/32613`, including geographic 3D CRSs (`EPSG:4979`) and
  compound CRSs (`EPSG:5498`);
- a horizontal and a vertical CRS, as GDAL builds them: `EPSG:4326+5773` (WGS 84 with EGM96
  heights);
- `OGC:CRS84` and `WGS84`;
- ESRI codes, the geographic and projected CRSs of PROJ's ESRI authority: `ESRI:54009` (World
  Mollweide), `esri:102003`, `urn:ogc:def:crs:ESRI::102003`. The few whose projection terrio
  does not compute (Goode homolosine, Peirce quincuncial and others) are refused;
- WKT1 and WKT2 (2015 and 2019) definitions, including ESRI `.prj` WKT and WKT2 `BOUNDCRS`s
  (a CRS with its transformation to WGS 84, as `+towgs84`); an unknown WKT2 projection method
  or parameter is an error rather than ignored;
- PROJ strings: `+proj=utm +zone=13 +datum=WGS84`, with `+nadgrids=` and `+geoidgrids=` for
  datum shift and geoid grids;
- the path of a file holding one of these.

Of PROJ's 117 projections, terrio computes 86 as PROJ does (within 1 mm, on a sphere and on WGS 84,
as PROJ strings with typical parameters and as pyproj's WKT1 and WKT2 of them:
`crates/terrio/tests/proj_strings.rs`) and refuses the 31 others (`airy`, `bacon`, `igh`, `nsper`,
`tpers`, `vandg2` and others) rather than computing something else. The known difference is
Krovak off Bessel's ellipsoid, where PROJ's inverse is not the inverse of its forward.

Rotated-pole grids (regional climate models: CORDEX, COSMO-CLM) are read as PROJ strings
(`+proj=ob_tran +o_proj=longlat` with `+o_lat_p`, `+o_lon_p` and `+lon_0`), as the WKT2 derived
geographic CRSs PROJ and GDAL write (the netCDF CF, GRIB and PROJ `ob_tran` pole rotations) and as
CF `rotated_latitude_longitude` grid mappings. Their coordinates are in degrees, but they are not
geographic CRSs: `is_geographic()` is false, and `is_rotated_pole()` is true. Other `ob_tran`
forms, and other WKT2 deriving conversions, are errors.

Coordinates are always in GIS order: x (easting, longitude) first, then y (northing, latitude),
whatever the axis order the authority defines.

## Identification

When a dataset stores WKT without an EPSG code, as shapefiles do, terrio identifies the code: an
EPSG CRS on the WKT's datum, known by its `AUTHORITY` or its name (official, alias, or ESRI's:
`D_European_1950` is ED50, `D_ETRS_1989` is ETRS89), with the same conversion and units. When
several codes match, it prefers the one named as the WKT's CRS, then (for a projected CRS) the one
with its axis order, then the base datum over its later realizations (NAD83 over NAD83(HARN) and
NAD83(2011)) unless the WKT names one, then the lowest code. CRSs that terrio's EPSG table lacks or
whose projection it cannot compare are looked up in wbprojection's registry, on the same datum,
with the last two preferences. This is what GDAL reports: the New York City boroughs shapefile is
EPSG:2263 in both, and ESRI's `GCS_European_1950` is EPSG:4230.

A datum terrio does not know by name and without `TOWGS84` (`Unknown based on Bessel 1841
ellipsoid`, as GDAL and PROJ write a PROJ string's `+ellps=bessel`) gets no EPSG code and no datum
shift, as in GDAL: the CRS is used as it is.

A CRS of PROJ's ESRI authority is reported under it, as GDAL reports it: a GeoTIFF GDAL writes in
`ESRI:54009` is `ESRI:54009` (from the ESRI code the WKT declares or, for GDAL's ESRI PE strings,
the ESRI CRS of the same name and definition), never an EPSG code.

GeoTIFFs with user-defined projected CRSs, common in NASA products, are read from their GeoKeys:
UTM zones become their EPSG codes, and other projections (Albers, Lambert conformal conic,
transverse Mercator, polar stereographic and more) become PROJ definitions.

## Transformations

```sh
terrio crs EPSG:4326 --to EPSG:32613 -105.5 39.5
echo "-105.5 39.5 1600" | terrio crs EPSG:4979 --to EPSG:6318+5703      # with heights
echo "135 -25 0" | terrio crs EPSG:7912 --to EPSG:7843 --s_coord_epoch 2010.5
```

Between two CRSs on different datums, terrio picks the datum transformation GDAL (PROJ) picks for
each point: the most accurate of those GDAL lists for the pair whose area of use holds the point,
else a chain through an intermediate datum such as WGS 84; when none applies, no shift (PROJ's
"ballpark" transformation). NAD83 and WGS 84 are treated as equivalent, as GDAL's default
operations do. Bounds are transformed with densified edges, so curved edges in the target CRS are
covered. `scripts/gen_epsg_points.py` writes GDAL's results for every EPSG CRS, which
`crates/terrio/tests/epsg_accuracy.rs` checks terrio against (see the
[conformance report](conformance.md)).

### Datum shift grids

Many datum transformations interpolate shifts in a grid: NADCON and NADCON5 in the United
States, NTv2 grids in Canada, Australia, Germany, Great Britain (OSTN15) and elsewhere, France's
geocentric translations. terrio reads the grids PROJ uses, in pure Rust:

- PROJ's GeoTIFF grids (`.tif`), as published on the [PROJ CDN](https://cdn.proj.org) (PROJ-data),
  with their subgrids, offsets in arc-seconds or degrees, scale and offset metadata, and every
  layout PROJ reads (tiled or stripped, chunky or planar, deflate, LZW or ZSTD compressed);
- NTv2 (`.gsb`) files with their subgrids, and GTX (`.gtx`) geoid grids;
- the old names of PROJ-data grids (`conus` for `us_noaa_conus.tif`).

They are applied as PROJ applies them: bilinear interpolation (biquadratic for NADCON5 grids, as
their metadata asks), PROJ's iterated inverse, its handling of grid edges and of the
antimeridian, and `hgridshift`, `vgridshift`, `gridshift`, `xyzgridshift` and `deformation` steps.

A transformation that needs a grid is used only when the grid can be found, so that terrio gives
GDAL's result for the same installed grids: with the grids, the transformations GDAL lists when
they are available; without them, those it lists when they are not. When the point falls outside
the grid, the next best transformation is used, as PROJ does. Grids are looked up, in order:

1. in grids registered from memory (`terrio::crs::register_grid`, `registerGrid` in JavaScript,
   which has no file system);
2. in the directories of `TERRIO_GRID_DIR` (a path list), then `PROJ_DATA` and `PROJ_LIB` (so
   grids installed for PROJ, such as conda's `proj-data`, are found), then PROJ's user directory
   (`~/.local/share/proj`, where `projsync` writes) and terrio's download cache;
3. when network access is on (`TERRIO_NETWORK=ON`, or `PROJ_NETWORK=ON` when the first is not
   set), on the PROJ CDN (`TERRIO_CDN_ENDPOINT`, default `https://cdn.proj.org`): the grid is
   downloaded once into the cache (`TERRIO_GRID_CACHE`, default `~/.cache/terrio/grids`).

The search directories and network access can also be set from code
(`terrio::crs::set_grid_search_paths`, `set_network_enabled`, and the same functions in Python),
as PROJ's `proj_context_set_search_paths` and `proj_context_set_enable_network`.

```sh
TERRIO_NETWORK=ON terrio crs EPSG:4267 --to EPSG:4269 -100 40   # downloads the NADCON5 grid
TERRIO_GRID_DIR=~/grids terrio crs EPSG:27700 --to EPSG:4258 434422 233624   # OSTN15
```

### Heights and vertical datums

Between CRSs that both have heights (geographic 3D CRSs such as `EPSG:4979`, and compound CRSs of
a horizontal and a vertical CRS such as `EPSG:4326+5773` or `EPSG:7405`), heights go through the
transformation as in GDAL: geoid models (EGM96, EGM2008, GEOID18, OSGM15... the `vgridshift`
transformations from a geographic 3D CRS to a vertical CRS) turn heights into ellipsoidal heights
and back, a horizontal transformation first when the geoid model is on another datum (OSTN15
before OSGM15 for British National Grid with ODN heights), and 3D datum transformations change
ellipsoidal heights. Vertical units (US survey feet) and depths are converted. When no geoid
model applies, heights are kept, as PROJ's ballpark vertical transformation does. Between 2D CRSs,
heights are kept as GDAL keeps them (or transformed, for the pipelines PROJ runs on heights).

`Transformer::transform_3d` in Rust, `transform_points(xs, ys, src, dst, zs)` in Python,
`transformPoints(xs, ys, src, dst, zs)` in JavaScript and `x y z` lines on `terrio crs`'s
standard input transform heights. Transformations between vertical CRSs are made through
ellipsoidal heights; PROJ's direct vertical-to-vertical transformations (VERTCON, offsets) are not
used.

### Dynamic datums and coordinate epochs

ITRF realizations, WGS 84's realizations, ETRF and other dynamic reference frames move with the
tectonic plates, and the transformations between them and to static datums (GDA2020, ETRS89,
NAD83(CSRS)) have rates of change: the result depends on the coordinates' epoch. As in GDAL, the
coordinate epoch is a property of the coordinates (`-s_coord_epoch` and `-t_coord_epoch` of
`gdaltransform`, `TransformOptions` in Rust, `s_coord_epoch` and `t_coord_epoch` in Python and
JavaScript, `--s_coord_epoch` and `--t_coord_epoch` on `terrio crs`):

- from a dynamic CRS to a static one, the source epoch is used;
- from a static CRS to a dynamic one, the target epoch;
- between two dynamic CRSs, the source epoch when both are given (PROJ does not move coordinates
  from one epoch to the other);
- otherwise the transformations' parameters at their reference epoch are used, as PROJ does
  without an epoch.

`Transformer::transform_4d` takes the time of each point instead (`x y z t` lines on
`terrio crs`). Time-dependent Helmert transformations (15 parameters) and velocity grids
(`deformation`) are supported.

```rust
use terrio::crs::{SpatialRef, TransformOptions, Transformer};

let opts = TransformOptions { source_coordinate_epoch: Some(2010.0), ..Default::default() };
let t = Transformer::with_options(&SpatialRef::parse("EPSG:7912")?, &SpatialRef::parse("EPSG:7843")?, &opts)?;
let (lon, lat, h) = t.transform_3d(135.0, -25.0, 0.0)?;
```

### How close to GDAL

With the same grids, terrio's results are GDAL's to the micrometre (`scripts/compare_crs_grids_with_gdal.py`
compares random points through every grid of `scripts/proj_grids.txt`, legacy NTv2 and GTX files,
geoid models, compound CRSs and epochs: 250 cases, 50,000 points, the largest difference 0.02 mm).
For every EPSG CRS, GDAL's reference points computed with those grids
(`gen_epsg_points.py --grids`, 50 points of each kind per CRS spread over its area of use,
checked by `crates/terrio/tests/epsg_accuracy_grids.rs`) are matched within 1 cm forward and
inverse; from CRS to CRS, every point of 97.5 % of the pairs is within 1 cm and of 98.8 % within
1 m: PROJ chains some pairs through several intermediate datums (NAD27 to ITRF92 through
NAD83(HARN), NAD83(2011) and ITRF2008, for instance) where terrio tries a single intermediate
datum, and picks another transformation than terrio at some points outside the areas of use of
the pair's transformations.

## Attribution

EPSG definitions come from the [EPSG Geodetic Parameter Dataset](https://epsg.org), through the
`wbprojection` crate and PROJ. The grid algorithms follow [PROJ](https://proj.org) (MIT licence);
the grids themselves come from [PROJ-data](https://cdn.proj.org), under their producers' licences.

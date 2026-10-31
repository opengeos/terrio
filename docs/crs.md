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
  does not compute (Goode homolosine of the oceans, Peirce quincuncial and others) are refused;
- WKT1 and WKT2 (2015 and 2019) definitions, including ESRI `.prj` WKT and WKT2 `BOUNDCRS`s
  (a CRS with its transformation to WGS 84, as `+towgs84`); an unknown WKT2 projection method
  or parameter is an error rather than ignored;
- PROJJSON v0.7 definitions in the [supported object subset](#projjson-definitions);
- PROJ strings: `+proj=utm +zone=13 +datum=WGS84`, with `+nadgrids=` and `+geoidgrids=` for
  datum shift and geoid grids;
- the path of a file holding one of these.

Of PROJ's 117 projections, terrio computes 89 as PROJ does (within 1 mm, on a sphere and on WGS 84,
as PROJ strings with typical parameters and as pyproj's WKT1 and WKT2 of them:
`crates/terrio-crs/tests/proj_strings.rs`) and refuses the 28 others (`airy`, `bacon`, `goode`,
`vandg2` and others) rather than computing something else. The known difference is
Krovak off Bessel's ellipsoid, where PROJ's inverse is not the inverse of its forward.

Mollweide and Wagner IV/V use Terrio's own ports of PROJ's formulas, including
PROJ's finite Mollweide pole approximation. The orthographic inverse intersects
the viewing ray with the ellipsoid directly, including local scale and azimuth.
For Winkel Tripel, an omitted `lat_1` means 0, as in PROJ's CRS model and GDAL;
PROJ's bare operation instead defaults to `acos(2/pi)` (about 50.46°). Supply
`lat_1` explicitly when comparing these paths. See the
[CRS benchmark regressions](testing.md#crs-benchmark-regressions-266-268) for
reference versions, tolerances and boundary differences.

Rotated-pole grids (regional climate models: CORDEX, COSMO-CLM) are read as PROJ strings
(`+proj=ob_tran +o_proj=longlat` with `+o_lat_p`, `+o_lon_p` and `+lon_0`), as the WKT2 derived
geographic CRSs PROJ and GDAL write (the netCDF CF, GRIB and PROJ `ob_tran` pole rotations) and as
CF `rotated_latitude_longitude` grid mappings. Their coordinates are in degrees, but they are not
geographic CRSs: `is_geographic()` is false, and `is_rotated_pole()` is true. The pole may
also be given as PROJ's other forms (see
[General oblique transformation](#general-oblique-transformation)) in a PROJ string; other WKT2
deriving conversions are errors.

Coordinates are always in GIS order: x (easting, longitude) first, then y (northing, latitude),
whatever the axis order the authority defines.

## Standalone Rust crate

Applications that only need coordinate systems can depend on `terrio-crs` rather
than Terrio's raster/vector library:

```toml
[dependencies]
terrio-crs = "0.2"
```

The [published crate](https://crates.io/crates/terrio-crs/0.2.0) works offline by
default. Enable `wbprojection-fallback` when Terrio's wider projection coverage
is required; the feature matrix below describes the supported configurations.

```rust
use terrio_crs::{SpatialRef, Transformer};

let source = SpatialRef::parse("EPSG:4326")?;
let target = SpatialRef::parse("EPSG:32613")?;
let transform = Transformer::new(&source, &target)?;
let (easting, northing) = transform.transform(-105.0, 40.0)?;
# Ok::<(), terrio_crs::Error>(())
```

| Configuration | Behavior |
|---|---|
| Default | EPSG/ESRI tables, WKT and PROJ strings, 40 native methods from the 117-method test corpus, datum operations, epochs, and local or registered grids; offline |
| `wbprojection-fallback` | 46 additional projection methods and legacy WKT/identification fallback; enabled by Terrio |
| `grid-transport` (native) | An application supplies `network::GridTransport` with seekable readers; the engine retains lazy TIFF block decoding |
| Terrio's `remote` feature | Supplies Terrio's existing HTTP range reader, request controls and cache to the shared engine |
| WebAssembly | Register grid bytes with `register_grid`; the host handles network access |

The transformation implementation and embedded tables live in
`crates/terrio-crs`. `terrio::crs` retains its existing structs, public fields,
methods, `terrio::Error` results, and raster/vector conversion methods. Owned
`SpatialRef` values convert between the two crates with `From`/`Into`.
The standalone crate returns its own `terrio_crs::Error`.

Grid registration, search paths, network configuration and decoded-grid caches
are shared by both interfaces in a process. Setting a custom standalone transport
before using Terrio preserves that transport. Terrio installs its default HTTP
transport lazily on the first CRS construction, transformation, grid registration
or configuration call through `terrio::crs`; linking Terrio or calling only
`reset_grid_cache()` does not install it. Direct `terrio_crs` callers should
configure their own transport, or first call a Terrio CRS entry point such as
`terrio::crs::set_network_enabled(None)`. Network access remains controlled
by the settings described below. Direct callers retrieve diagnostics with
`terrio_crs::take_warnings()`, while Terrio continues to use
`terrio::warnings::take()`. A custom transformer warning handler is also available.

The standalone default omits `wbprojection`; Terrio enables the fallback to retain
its existing coverage. Bundled tables use portable `ruzstd` decompression on all
targets. The standalone default has no C dependency: Zstandard TIFF grids are read only with the
`libzstd` feature, which uses the zstd C library and which Terrio does not enable. The PROJ CDN's
grids are Deflate-compressed (all 11 sampled, among them those this page and the tests name), so this
concerns grids recompressed with Zstandard.
The [dependency and build measurements](crs-footprint.md) record both standalone
configurations and full Terrio before/after extraction. Extraction and validation are tracked in
[#268](https://github.com/opengeos/terrio/issues/268). Capability additions are tracked separately in #269–#272.

### Publication and release workflow

The initial `terrio-crs` 0.1.0 release was published from a verified checkout with
`cargo publish -p terrio-crs --locked`. Its crates.io trusted publisher is configured
for repository `opengeos/terrio`, workflow `release.yml`, environment `crates-io`.
Subsequent versions use the release workflow. Its bootstrap
check runs before registry authentication or uploads and fails with setup guidance
if the crate is unavailable. Already published versions are skipped.

The workflow publishes `terrio-crs`, then `terrio`, then `terrio-cli` using each
package's own version. Cargo waits for index visibility between publications; a
polling timeout requires checking registry state before attempting another upload.
See [Cargo's publication behavior](https://doc.rust-lang.org/cargo/commands/cargo-publish.html).

The [standalone benchmark validation](crs-benchmark-validation.md) records the
direct native/fallback results against both pinned upstream suites.

Canonical EPSG references use one parser in both standalone configurations.
Versioned URNs/URLs resolve the code separately from the authority version
(`urn:ogc:def:crs:EPSG:6.6:4326` resolves EPSG:4326). The fallback retains its
legacy free-form authority extraction only after canonical parsing fails;
inputs such as `EPSG:4326junk` are refused by the native default and retain the
legacy acceptance with the fallback. Inputs beginning with `+` remain PROJ
strings in every configuration.

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
transverse Mercator, polar stereographic and more) become PROJ definitions. A user-defined
geographic CRS on an EPSG datum (GeogGeodeticDatumGeoKey) keeps that datum, as GDAL reads it: the
GeoKeys of PDAL's `autzen_trim.las`, on NAD83(HARN), give NAD83(HARN) / Oregon GIC Lambert (ft)
(EPSG:2994), not the same projection on NAD83, and a UTM zone on such a datum is that datum's.
The same GeoKeys are read in LAS files.

## PROJ string interpretation changes

PROJ strings supplied as CRSs follow PROJ's CRS import rules. For polar
`+proj=stere` with `+lat_0=90` or `-90`, an explicit `+lat_ts` selects the
hemisphere: negative selects south, and zero or positive selects north. For
example, `+lat_0=-90 +lat_ts=90` now describes the north pole. This differs from
PROJ's bare stereographic operation, which takes the hemisphere from `lat_0`.
Use a negative `lat_ts` for a southern standard parallel. Terrio's CRS API does
not interpret these strings as raw projection operations.

For `+proj=lcc +lat_1=X` with neither `lat_0` nor `lat_2`, CRS import supplies
zero for both omitted parameters. Earlier Terrio versions used X for both,
matching a bare projection operation instead. Supply `+lat_0=X +lat_2=X`
explicitly to retain that tangent-cone definition. Coordinates from previously
ambiguous strings can change substantially under these corrected CRS rules.

## Transformations

```sh
terrio crs EPSG:4326 --to EPSG:32613 -105.5 39.5
echo "-105.5 39.5 1600" | terrio crs EPSG:4979 --to EPSG:6318+5703      # with heights
echo "135 -25 0" | terrio crs EPSG:7912 --to EPSG:7843 --s_coord_epoch 2010.5
```

Between two CRSs on different datums, terrio picks the datum transformation GDAL (PROJ) picks for
each point: the most accurate of those GDAL lists for the pair whose area of use holds the point;
when PROJ's database has no transformation between the two datums, a chain of two of the
transformations it registers (each possibly a concatenation of several) through intermediate CRSs
and datums, found and sorted as PROJ's `proj_create_operations` does (through an intermediate CRS
first, between the geographic and geocentric CRSs of the datums, then through an intermediate datum,
whatever its CRSs; without the transformations replaced by another it finds); when none applies, no
shift (PROJ's "ballpark" transformation, of 0 m accuracy between CRSs that differ only by their
prime meridian). NAD27 to ITRF92, for instance, goes through NAD83(2011) (PROJ's concatenation of
the NADCON5 grids), ITRF2008 and ITRF2014. NAD83 and WGS 84 are treated as equivalent, as GDAL's
default operations do. Bounds are transformed with densified edges, so curved edges in the target
CRS are covered. `scripts/gen_epsg_points.py` writes GDAL's results for every EPSG CRS, which
`crates/terrio-crs/tests/epsg_accuracy.rs` checks terrio against (see the
[conformance report](conformance.md)).

`Transformer::operation_at(x, y, z)` returns the transformations `transform_3d` runs for a point,
as PROJ's `proj_trans_get_last_used_operation`: their names, summed accuracy, grids and whether one
is a ballpark, or `None` between CRSs of one datum. Conversions (projections, units) are not
named, and geoid models are named without PROJ's EPSG index (`WGS 84 to EGM96 height`).

```rust
let t = terrio_crs::Transformer::new(&SpatialRef::parse("EPSG:27700")?, &SpatialRef::parse("EPSG:4326")?)?;
let op = t.operation_at(530000.0, 180000.0, 0.0)?;   // "OSGB36 to WGS 84 (9)", 1 m, OSTN15
```

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
   read in place with HTTP range requests, as PROJ reads it, fetching only its header and the
   blocks around the points transformed (four EGM2008 heights fetch 0.85 MB of the 80 MB
   `us_nga_egm08_25.tif`). With `TERRIO_GRID_DOWNLOAD=YES`, it is downloaded whole into the
   cache instead (`TERRIO_GRID_CACHE`, default `~/.cache/terrio/grids`), where later runs find
   it.

The search directories and network access can also be set from code
(`terrio::crs::set_grid_search_paths`, `set_network_enabled`, and the same functions in Python),
as PROJ's `proj_context_set_search_paths` and `proj_context_set_enable_network`.

```sh
TERRIO_NETWORK=ON terrio crs EPSG:4267 --to EPSG:4269 -100 40   # reads the NADCON5 grid by range
TERRIO_GRID_DIR=~/grids terrio crs EPSG:27700 --to EPSG:4258 434422 233624   # OSTN15
```

### Heights and vertical datums

Between CRSs that both have heights (geographic 3D CRSs such as `EPSG:4979`, and compound CRSs of
a horizontal and a vertical CRS such as `EPSG:4326+5773` or `EPSG:7405`), heights go through the
transformation as in GDAL: geoid models (EGM96, EGM2008, GEOID18, OSGM15... the `vgridshift`
transformations from a geographic 3D CRS to a vertical CRS) turn heights into ellipsoidal heights
and back, a horizontal transformation first when the geoid model is on another datum (OSTN15
before OSGM15 for British National Grid with ODN heights), and 3D datum transformations change
ellipsoidal heights. Vertical units (US survey feet) and depths are converted. A vertical CRS
without geoid models of its own uses those of the vertical CRSs of its datum, as PROJ does:
NAVD88 height (ftUS) (`EPSG:6360`), NAVD88 height (ft) (`EPSG:8228`) and NAVD88 depth go
through NAVD88 height's GEOID18. When no geoid model applies, heights are kept, as PROJ's
ballpark vertical transformation does. Between 2D CRSs, heights are kept as GDAL keeps them (or
transformed, for the pipelines PROJ runs on heights).
With `TransformOptions { promote_to_3d: true, .. }` (Rust), the heights of 2D CRSs are ellipsoidal
heights that datum transformations change, as GDAL gives them after `PromoteTo3D`: EPSG:28992
(85000, 447000, 0) is 43.576 m above the WGS 84 ellipsoid in EPSG:4979, where it stays 0 by default. Vector layer
conversions pass existing Z values through the promoted transformer and preserve
2D input geometries as 2D, matching OGR geometry transformation.

`Transformer::transform_3d` in Rust, `transform_points(xs, ys, src, dst, zs)` in Python,
`transformPoints(xs, ys, src, dst, zs)` in JavaScript and `x y z` lines on `terrio crs`'s
standard input transform heights.

Between compound CRSs on two vertical CRSs that the EPSG dataset connects, heights go through the
transformations PROJ lists between them, as PROJ prefers them to geoid models: vertical offsets,
offsets and slopes (`vertoffset`), VERTCON's grids from NGVD29 to NAVD88 and other grids, directly
or chained through an intermediate vertical CRS (DHHN92 to SNN76 through EVRF2000), in the CRSs'
axis directions (depths included) and units. For each point, the most accurate transformation
whose area holds it is used, PROJ's way: one interpolating in another datum's geographic CRS
counts the accuracy of the horizontal transformation to it (both ways), and is evaluated there.
Where none applies, heights are kept. Vertical CRSs of one datum (NAVD88 height in metres and in
US survey feet) only convert units.

```sh
TERRIO_NETWORK=ON terrio crs EPSG:4267+7968 --to EPSG:4267+5703 -105.27 40.01 1700   # VERTCON: 1701.0026
terrio crs EPSG:4258+5783 --to EPSG:4258+5785 13.4 52.5 100                       # DHHN92 to SNN76: 99.856
```

These are listed in `crates/terrio-crs/data/epsg_vertical.tsv.zst` (`scripts/gen_vertical_table.py`)
and compared with PROJ by `scripts/compare_vertical_with_proj.py`: of 732 points over the 604
pairs, 727 have PROJ's heights to the micrometre; the other 5 (EVRF2000 Austria, where PROJ chains
the horizontal transformations of some concatenations in its own way) differ by 0.3 mm to 1 cm.

### Dynamic datums and coordinate epochs

ITRF realizations, WGS 84's realizations, ETRF and other dynamic reference frames move with the
tectonic plates, and the transformations between them and to static datums (GDA2020, ETRS89,
NAD83(CSRS)) have rates of change: the result depends on the coordinates' epoch. As in GDAL, the
coordinate epoch is a property of the coordinates (`-s_coord_epoch` and `-t_coord_epoch` of
`gdaltransform`, `TransformOptions` in Rust, `s_coord_epoch` and `t_coord_epoch` in Python and
JavaScript, `--s_coord_epoch` and `--t_coord_epoch` on `terrio crs`):

- from a dynamic CRS to a static one, the source epoch is used;
- from a static CRS to a dynamic one, the target epoch;
- between two dynamic CRSs, the source epoch when both are given;
- between two epochs of one datum that has a point motion operation (the Canada velocity grids
  v6, v7 and v8 of NAD83(CSRS), the NKG_RF17vel land uplift model of the Nordic ETRF2014
  realization), the coordinates move with its velocity grid over the time between the epochs,
  as PROJ moves them (`projinfo -s EPSG:8254@2010 -t EPSG:8254@2020`): geographic 2D and 3D
  CRSs, not projected ones (which PROJ does not move);
- otherwise the transformations' parameters at their reference epoch are used, as PROJ does
  without an epoch.

`Transformer::transform_4d` takes the time of each point instead (`x y z t` lines on
`terrio crs`). Time-dependent Helmert transformations (15 parameters) and velocity grids
(`deformation`) are supported.

```sh
# NAD83(CSRS)v7 coordinates of 2010 at epoch 2020 (Canada velocity grid v7, from the PROJ CDN)
echo "-75.7 45.4 100" | TERRIO_NETWORK=ON terrio crs EPSG:8254 --to EPSG:8254 --s_coord_epoch 2010 --t_coord_epoch 2020
```

On 1,100 points of NAD83(CSRS) v6 and v7 (2D and 3D, both directions) and NKG_RF17vel, the
coordinates are PROJ's to 4 nanometres. (GDAL 3.12's `osr` applies the forward motion when the
target epoch is the earlier one; PROJ's own pipeline, which terrio follows, moves them back.)

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
checked by `crates/terrio-crs/tests/epsg_accuracy_grids.rs`) are matched within 1 cm forward and
inverse; from CRS to CRS, every point of 6,291 of the 6,295 pairs is within 1 cm and of 6,294
within 1 m: the other is a deprecated CRS whose own datum PROJ uses (#153).

The chains through intermediate datums are checked by `scripts/compare_chains_with_proj.py`, on
every pair of EPSG geographic 2D CRSs on two datums no transformation links directly whose areas of
use overlap (31,127 pairs, 5 points each), and on a projected CRS of each datum (1,204 pairs):
without grids, every point of every geographic pair is GDAL's within 1 cm (31,126 pairs within
1 mm), as are 1,202 projected pairs; with the grids of `scripts/proj_grids.txt`, 31,126 geographic
pairs and 1,200 projected ones are within 1 cm, and every geographic pair within 1 m. The others
go through transformations PROJ's database
registers between projected CRSs (IGN Astro 1960 / UTM zone 28N to Mauritania 1999 / UTM zone 28N,
S-JTSK / Krovak East North to S-JTSK/05 / Modified Krovak East North), which terrio does not
apply.

## Attribution

EPSG definitions come from the [EPSG Geodetic Parameter Dataset](https://epsg.org), through the
`wbprojection` crate and PROJ. The grid algorithms follow [PROJ](https://proj.org) (MIT licence);
the grids themselves come from [PROJ-data](https://cdn.proj.org), under their producers' licences.

## Geocentric Cartesian CRSs

The shared pure-Rust engine accepts geocentric Cartesian endpoints such as
`EPSG:4978` (WGS 84 X/Y/Z), `+proj=geocent`, WKT1 `GEOCCS`, and WKT2
`GEODCRS`/`GEODETICCRS` with a three-axis Cartesian CS. Authority-free WKT is
supported. The bundled table adds 269 EPSG geocentric definitions from PROJ 9.8.1 /
GDAL 3.12.2; the update script reuses each datum's existing geographic operation
lists. This works in the standalone crate without optional features, in Terrio,
and in WebAssembly. These CRSs report `is_geocentric()`, not `is_geographic()`.

Use all three ordinates. Cartesian X points toward longitude 0, latitude 0,
Y toward longitude 90, latitude 0, and Z toward the north pole. Linear units apply
to all three coordinates; PROJ `+units`, `+to_meter`, prime meridians and axis
options are supported. Geocentric PROJ axes must contain one east/west and one
north/south direction in the first two positions, followed by up/down. WKT2
requires three canonical geocentric X/Y/Z axes with consistent linear units;
other layouts are refused. Geographic inputs remain longitude, latitude,
ellipsoidal height in traditional GIS order. Geocentric conversions compose with projected
CRSs, datum shifts and existing coordinate-epoch options.

```rust
let src = terrio_crs::SpatialRef::from_epsg(4979)?;
let dst = terrio_crs::SpatialRef::from_epsg(4978)?;
let xyz = terrio_crs::Transformer::new(&src, &dst)?.transform_3d(147.0, -42.0, 100.0)?;
// Approximately (-3981230.646139, 2585441.411873, -4245670.749162) metres.
```

```sh
terrio crs EPSG:4979 --to EPSG:4978 --dim 3 147 -42 100
# Standard input also accepts XYZ or XYZ plus observation epoch, as before.
```

`--dim 3` (or `--dim 4` with epoch) groups positional ordinates and makes the same
operation available to the MCP `crs` tool, whose arguments are CLI arguments.

```python
x, y, z = terrio.transform_points([147], [-42], "EPSG:4979", "EPSG:4978", zs=[100])
```

```js
const [x, y, z] = terrio.transformPoints([147], [-42], "EPSG:4979", "EPSG:4978", [100]);
```

Vector reprojection involving a geocentric endpoint retains all three output
ordinates, treating missing input Z as zero; measures remain unchanged. The 2D
point API likewise uses input Z=0, but returns only X/Y. Use the 3D API to retain
the result's Z; a two-coordinate raster or bounding box does not supply a 3D
height model.

Native, CLI/MCP, Python and WASM regressions check all three ordinates against
the pinned `rust-proj-capability` PROJ 9.9.0 fixture and a frozen GDAL 3.12.2 /
PROJ 9.8.1 suite. Cartesian/projected coordinates and heights allow 1 mm;
geographic angles allow 1e-8 degrees. This is numerical agreement, not bit
identity. The inverse uses PROJ's normalized Bowring convention: at the ellipsoid
centre longitude is zero, latitude is +90 and height is minus the polar radius;
interior points can have no unique geographic inverse. Nonfinite coordinates,
invalid linear units/ellipsoids and out-of-range geographic latitude are errors.

One reference discrepancy is explicit: PROJ 9.8.1's CRS factory skips a declared
`TOWGS84` on a custom geocentric BoundCRS in the tested Airy case. Terrio honors
that declared shift. Those two composition cases are compared with the declared
Helmert operation run as an explicit GDAL pipeline, recorded in
`geocentric_reference.json`, rather than certifying a lost datum transformation.

## PROJJSON definitions

The shared parser accepts serialized [PROJJSON v0.7](https://proj.org/schemas/v0.7/projjson.schema.json)
objects and files containing them. `SpatialRef::from_projjson` is also available
in both Rust crates. Python and JavaScript CRS arguments accept JSON strings;
serialize a language object with `json.dumps` or `JSON.stringify`.

| Object | Supported subset |
|---|---|
| `GeographicCRS`, `GeodeticCRS` | Two-dimensional ellipsoidal coordinates on a static geodetic reference frame or datum ensemble; longitude/latitude in GIS order |
| `ProjectedCRS` | A supported geographic base and WKT2 projection conversion, two Cartesian axes, including north-first and west/south orientations |
| `BoundCRS` | A geographic/projected source, a Greenwich WGS 84 geographic target, and a complete three- or seven-parameter Helmert transformation, identified by EPSG codes or standard names |

Geographic axes must span east and north; projected axes may use west and south.
Both axes need the same explicit unit. Standard unit strings include `metre`,
`degree`, `radian`, `grad`, `unity`, `arc-second` and `parts per million`.
Other units use a `LinearUnit`, `AngularUnit` or `ScaleUnit` object with a finite,
positive `conversion_factor`, such as the US survey foot in PROJ's exports.
Ellipsoid axis/radius quantities and prime-meridian longitude quantities are
converted using their declared units. Projection and Helmert parameters require
explicit units of the appropriate kind, using the shared WKT2 parameter
catalogue. Duplicate parameter identities and contradictory known names/IDs
are refused. Zero Helmert parameters retain the
conversion between different ellipsoids.

```sh
terrio crs OGC:CRS84 --to british-grid.projjson -1.5 52.5
```

```python
import json
x, y = terrio.transform_points(lons, lats, "EPSG:4326", json.dumps(definition))
```

```js
const [x, y] = terrio.transformPoints(lons, lats, "EPSG:4326", JSON.stringify(definition));
```

The supplied conversion parameters are retained even when the root object has
an authority ID. IDs identify component methods, parameters and datums; they do
not replace the complete definition with a registry lookup. The resulting CRS
stores WKT2 and does not assign a root EPSG/ESRI code merely from an ID. The
exported WKT omits root CRS identifiers so that reopening it also preserves
caller-supplied conversion parameters; component identifiers remain. Descriptive
usage fields (`scope`, `area`, `bbox`, `usages`, `remarks`) are accepted without
affecting coordinates.

This is a reader for the documented subset, rather than a general JSON Schema
validator; [Writing PROJJSON](#writing-projjson) describes the writer. Unsupported semantic members and object types
are errors: dynamic/derived/compound/vertical CRSs, Cartesian geocentric or 3D
PROJJSON, polar axis meridians, grid/deformation operations and nested bound
sources are not supported.
Geocentric and 3D CRSs remain available through EPSG, PROJ and WKT definitions.
This prevents silent loss of datum, axis, unit or operation information.

### Writing PROJJSON

`SpatialRef::to_projjson` writes PROJJSON v0.7 for geographic 2D and 3D, geocentric, projected,
vertical, compound and bound (`+towgs84`) CRSs: `terrio crs CRS --projjson` (and MCP), Python
`rio.CRS.to_projjson()` (and `to_dict(projjson=True)`), JavaScript `crsToProjjson(crs)`. PROJJSON
is read wherever a CRS is (above). The `projjson-export` suite of `scripts/conformance.py`
compares it with live PROJ's PROJJSON of 52 CRSs by meaning (identifiers, datums, ellipsoids,
prime meridians, axes, methods and parameters by EPSG code) and has GDAL transform points with
them and with the originals, through each CRS's geographic CRS (a vertical CRS paired with
WGS 84; no grids), and does the same for every IAU_2015 CRS. Planetary CRSs are written as PROJ writes them: west-positive longitudes as west
axes, planetocentric latitudes (`+geoc`, a spherical coordinate system) as a `GeodeticCRS` with a
spherical coordinate system, IAU_2015 CRSs with their `IAU`/2015 identifier. Differences from
PROJ's output:

- no `scope`, `area`, `bbox` or `usages`;
- the WGS 84 and ETRS89 ensembles are written as their datum, without members (so GDAL's
  `IsSame` with the EPSG CRS is false; coordinates are the same);
- parameters are in degrees and metres, whatever the CRS's units (EPSG:27572's in grads);
- projection conversions are named `unknown`, except UTM zones (`UTM zone 32N`, EPSG:16032);
- polar CSs have east and north axes rather than axes along meridians;
- static datums have no `anchor_epoch`.

Refused rather than written wrongly: dynamic CRSs (their frame epochs; by EPSG code or a WKT 2
`DYNAMIC` node), projections without an
EPSG method (`+proj=robin`), engineering CRSs, projections on an auxiliary sphere of an
ellipsoid (`+R_C`, `+R_A`; 19 IAU_2015 CRSs), planetocentric latitudes with heights, a
`+towgs84` of other than 3 or 7 values, a prime meridian it cannot read (`+pm=foo`), non-finite
values, a Krovak projection with another `+alpha` than its default (PROJ ignores its `+lat_1` and
`+lat_ts`, so they are not written), and any parameter of the PROJ definition the PROJJSON would
not carry (named in the error), unless it has the value used when it is absent (`+lat_0=0`,
`+k=1`...). A CRS made from an EPSG code is written unbound (its datum shift is EPSG's); a
`+towgs84` or `TOWGS84` the definition gives makes a `BoundCRS`, to WGS 84's geographic 2D or 3D
CRS, or to its geocentric CRS for a geocentric CRS, as PROJ writes them.


## IAU planetary coordinate systems

The shared pure-Rust engine includes the **IAU_2015** registry from PROJ 9.8.1,
with source Git blob `900564ccf2e1f77fb708ddc2c8544843b29bc24a`. Its 2,201 definitions
comprise 115 geographic CRSs (including spheres), 12 ellipsoidal planetocentric
geographic CRSs and 2,074 projected CRSs. The source is PROJ's
[`data/sql/iau.sql`](https://github.com/OSGeo/PROJ/blob/9.8.1/data/sql/iau.sql), based
on the [IAU Working Group's 2015 report](https://doi.org/10.1007/s10569-017-9805-5).
The embedded table is about 50 KB compressed; GDAL is used for validation and is
not a runtime dependency. See `NOTICE` and `scripts/gen_iau_table.py`.

Supported references include:

- `IAU_2015:49900` or `IAU:49900` (the latter defaults to this pinned registry);
- `IAU:2015:49900`;
- `urn:ogc:def:crs:IAU:2015:49900`;
- `urn:ogc:def:crs:IAU_2015::49900`;
- `https://www.opengis.net/def/crs/IAU/2015/49900`.

Unknown codes and other registry years are errors; `IAU:0:49900` is not a
latest-version alias. Both Rust crates expose `SpatialRef::from_iau(code)`. Authority
reporting uses `("IAU_2015", code)` and does not assign an EPSG code. Python
`rio.CRS.to_authority()` and JavaScript `describeCrs(...).authority` retain this
versioned identity. Stored WKT2 preserves the IAU version and the geographic
coordinate-system type. Python `rio.CRS.to_wkt()` returns this stored WKT when
called without an explicit format-conversion request.

```sh
terrio crs IAU_2015:49900 --to IAU_2015:49910 137.4 -4.6
```

```python
x, y = terrio.transform_points([137.4], [-4.6], "IAU_2015:49900", "IAU_2015:49910")
```

```js
const [x, y] = terrio.transformPoints([137.4], [-4.6], "IAU_2015:49900", "IAU_2015:49910");
```

Coordinates follow GDAL's traditional GIS order, including its planetary axis
conventions. For example, Mars `49900` uses east-positive longitude then
latitude. Mars `49901` uses **latitude then west-positive longitude**; its
projected `49911` uses westing then northing. Planetocentric `49902` uses
longitude then planetocentric latitude. The engine converts between
planetocentric and ellipsoidal geodetic latitude, so these two latitude types
are not interchangeable on an ellipsoid. Spherical and ellipsoidal projected
coordinates retain their declared units, offsets and axes. Mercator (Spherical)
on an ellipsoid uses the conformal-sphere radius (`+R_C`). PROJ 9.8.1
initializes this radius before reading `lat_0`, so it remains the equatorial
radius even when that latitude parameter is nonzero; Terrio follows this
reference behavior. Geographic and geocentric `+R_C` endpoints also use this
auxiliary sphere for runtime conversion. Their WKT retains the original
ellipsoid and a PROJ4 extension preserves the radius modifier on reopening.

CRSs on different known celestial bodies cannot be transformed into each other.
The PROJ-compatible Earth-radius heuristic treats an undeclared figure as Earth
when its semi-major axis is within 0.7% of 6,375,000 m. This is a radius
heuristic, not proof of body identity: use an explicit IAU body identifier for a
non-Earth figure, even if it has an Earth-like radius. Explicit body identities
take precedence over the heuristic. When a body identity remains unknown, semi-major axes must agree within 0.7% of the target's
axis. This is a compatibility change: custom CRS pairs with different small or
planetary radii that previously passed through as an unshifted datum now fail
when their axes exceed that allowance (for example, unit spheres of radii 1 and
2). Use IAU authority references to retain explicit non-Earth body identities. Earth datum operations
are rejected for planetary coordinates. There is no implicit Earth-frame bridge
and no `PROJ_IGNORE_CELESTIAL_BODY` bypass. Planetary datum shifts and time-varying
body orientation models are outside this registry's static 2D scope.

Validation compares all 2,074 projected CRS pairs (4,148 points) against frozen
and live GDAL 3.12.2 / PROJ 9.8.1, with component allowances of **1 mm projected**
and **1e-8 degrees inverse**, plus separate unit/offset cases. The original Mars
benchmark retains its distinct frozen PROJ 9.9.0 oracle. Nonzero differences are
reported as within tolerance. Projection domain and numerical limits still
apply: the reference itself refuses five inverse samples and has a worst
3.8343-degree round-trip residual for the highly flattened Eros transverse
Mercator definition `IAU_2015:200043361`. Agreement with these reference outputs
carries those limits; it is not a guarantee of accurate inverse recovery for
arbitrary extreme flattening. Reference refusals are recorded separately, and
native inverse refusal or accurate recovery of the original point is accepted.
The PROJJSON reader's existing object subset remains unchanged; use authority
strings or WKT2 for spherical-CS planetocentric definitions.

IAU datum identities are distinct from EPSG datums. Conversions within one registered
IAU datum (including planetographic/planetocentric latitude conversions) remain
available with `ALLOW_BALLPARK=NO`. Changes between different IAU datums, or from a
registered datum to an unidentified custom planetary datum, require the default
ballpark policy; strict mode rejects them. Editing a registered WKT datum does not
retain the original datum identity merely because the outer CRS identifier remains.

IAU authority reporting checks the stored definition against the registered CRS.
Editing parameters while retaining a root IAU identifier does not preserve that
registry identity or make the edited CRS equivalent to the original.

## Perspective projections

`+proj=nsper` (near-sided perspective) and `+proj=tpers` (tilted perspective)
use the native pure-Rust engine. Supply a positive viewpoint height `+h` in
metres and the view centre `+lat_0` / `+lon_0`. Tilted perspective additionally
accepts `+tilt` and `+azi` in degrees, both zero by default. The shared `+units`,
`+to_meter`, `+x_0` and `+y_0` options apply; height and false offsets remain in
metres even when the output is in kilometres or feet.

These are PROJ's spherical perspective formulas. With an ellipsoid such as
WGS 84, its semi-major axis supplies the sphere radius, matching PROJ 9.8.1;
this does not implement an ellipsoidal 3D camera model. All four viewing
aspects (north/south pole, equatorial and oblique) are supported. Coordinates
beyond the visible horizon and inverse coordinates outside the projection
domain raise a coordinate error. The CLI's streaming input reports each failed
point and continues; Python and JavaScript coordinate arrays raise if any point
fails.

PROJ strings and their WKT1 PROJ4 extensions retain the parameters. WKT2
`PROJ tpers` and PROJ's `Vertical Perspective` conversion are accepted; the latter
requires zero topocentric-origin height. Nonzero topocentric-origin height is
refused explicitly. This restriction preserves the supported spherical model
instead of ignoring the extra parameter.

The original `seed-tpers` and `edge-tpers-back` fixtures retain their separate
PROJ 9.9.0 oracle. The additional GDAL 3.12.2 / PROJ 9.8.1 corpus covers 20
configurations and 2,500 forward samples, both sides of the horizon, inverse
domain boundaries, metres/kilometres/US survey feet, offsets and non-default
origins/tilt/azimuth. Component allowances are 1 mm projected and 1e-8 degrees
inverse, with longitude weighted by cos(latitude) at the poles. Other projection
families in #272 remain tracked there.

## Interrupted Goode homolosine

`+proj=igh` is computed as PROJ 9.8.1 does (`igh.cpp`): twelve zones, spherical
sinusoidal between the equator and 40 degrees 44' 11.8" and Mollweide poleward of it, each
with its own central meridian, the Mollweide zones shifted north or south to meet
the sinusoidal ones. It is a spherical projection: with an ellipsoid, the
semi-major axis is the radius, as in PROJ. `+lon_0`, `+units`, `+x_0` and `+y_0`
apply. The inverse refuses points in the interruptions, beyond the poles' northings
and past 180 degrees from the central meridian, as PROJ's does, including PROJ's southern
bound, which also refuses the south pole's own northing.

GDAL's WKT1 `Interrupted_Goode_Homolosine`, ESRI's `Goode_Homolosine` with
`Option` 1, and WKT2 `Interrupted Goode Homolosine` are read, and terrio writes
the first, as GDAL does. ESRI's `Goode_Homolosine` without `Option` 1 and WKT2
`Goode Homolosine` are PROJ's uninterrupted `goode` (or, with `Option` 2,
`igh_o`), which terrio refuses; WKT2 `Goode Homolosine` was previously read as
`igh`. PROJJSON export refuses `igh`, as for other projections without an EPSG
method.

The `seed-igh` fixture keeps its PROJ 9.9.0 oracle. A PROJ 9.8.1 corpus covers
8 configurations and 5,120 samples (see Testing). Allowances are 1 mm projected
and 1e-8 degrees inverse.

## General oblique transformation

`+proj=ob_tran` with an `+o_proj` other than `longlat` is computed as PROJ 9.8.1 does
(`ob_tran.cpp`): the longitude, relative to `+lon_0`, and latitude are rotated to a new pole,
then projected by `+o_proj`'s own formulas, with its own parameters (`+lat_ts`, `+lat_1`,
`+zone`...) and the CRS's figure, which may be an ellipsoid. As in PROJ, the rotated projection's
own central meridian and false easting and northing do not apply: `+lon_0`, `+units`, `+x_0` and
`+y_0` apply to the whole. The new pole is `+o_lat_p` and `+o_lon_p`; or a centre `+o_lon_c`,
`+o_lat_c` and azimuth `+o_alpha`; or two points of the new equator `+o_lon_1`, `+o_lat_1`,
`+o_lon_2` and `+o_lat_2`, refused where PROJ refuses them. A pole latitude within 1e-10
radians of 0 is PROJ's transverse case. These forms apply to rotated-pole grids
(`+o_proj=longlat`) too.

PROJ strings, GDAL's WKT1 (which carries the PROJ string) and PROJ's WKT2 method
`PROJ ob_tran o_proj=...` are read; terrio writes the WKT1 form. PROJJSON export refuses
`ob_tran`. Where a projection's own refusals are stricter than PROJ's, they apply rotated too:
PROJ's spherical sinusoidal inverse returns a latitude past a pole, which its rotation turns into
a point, and terrio's refuses it; terrio's Mercator refuses the poles. Where terrio is more
lenient, that applies rotated too: its `utm` accepts a sphere, which PROJ refuses.

The `seed-ob_tran-moll` fixture keeps its PROJ 9.9.0 oracle. A PROJ 9.8.1 corpus covers
15 configurations and 4,680 samples (see Testing). Allowances are 1 mm projected and 1e-8
degrees inverse.

## Quadrilateralized spherical cube

`+proj=qsc` is computed as PROJ 9.8.1 does (`qsc.cpp`): one face of a cube, chosen by the
projection centre (`+lat_0` of at least 67.5 degrees north or south for the top or bottom face,
otherwise `+lon_0` within 45 degrees of 0, 90, 180 or -90 for the front, right, back or left
face). PROJ's face centres are those values exactly; for other centres, `+lon_0` stays the
central meridian. On an ellipsoid, geodetic latitudes are converted to
geocentric latitudes on a sphere of the semi-major axis, as in PROJ. The whole globe projects
onto the plane of the face, with no refusals; `+units`, `+x_0` and `+y_0` apply. The inverse at
the point opposite the face centre is sensitive to rounding: a change of 1e-15 in the northing
moves its latitude by about 1e-6 degrees, in PROJ as in terrio, and so may differ between
platforms.

GDAL's and ESRI's WKT1 `Quadrilateralized_Spherical_Cube` and WKT2 `Quadrilateralized Spherical
Cube` are read, and terrio writes the WKT1 form, as GDAL does. PROJJSON export refuses `qsc`, as
for other projections without an EPSG method.

The `method-qsc` fixture keeps its PROJ 9.9.0 oracle. A PROJ 9.8.1 corpus covers 16
configurations and 4,288 samples (see Testing). Allowances are 1 mm projected and 1e-8 degrees
inverse. At the point opposite the face centre the inverse is allowed 1e-5 degrees, and must
project back within 1 mm.

## HEALPix and rHEALPix

`+proj=healpix` and `+proj=rhealpix` are computed as PROJ 9.8.1 does (`healpix.cpp`): an
equal-area cylindrical projection between the latitudes of asin(2/3) and Collignon projections
in the four polar caps on each side, which `healpix` leaves as triangles and `rhealpix` combines
into one square at each pole. `+rot_xy` rotates the `healpix` image, in degrees; `+north_square`
and `+south_square` (0 to 3, the default 0) pick the equatorial square below which `rhealpix`
places each polar square, and other values are refused, as in PROJ. On an ellipsoid the
latitude is the authalic latitude and the image is scaled by the authalic radius, as in PROJ
9.8.1 (PROJ 9.4 differs by 0.18 m on WGS 84 at the south pole). `+lon_0`, `+units`,
`+x_0` and `+y_0` apply. The inverse refuses points outside the image, as PROJ's does. As in
PROJ, whose point-in-polygon test leaves out the polygon's closing edge, the `rhealpix` inverse
accepts points left of the image between the polar squares, such as (-4, 0) on the unit sphere.

GDAL's WKT1 (`custom_proj4`, which carries the PROJ string), ESRI's `PROJ_healpix` and
`PROJ_rhealpix`, and PROJ's WKT2 methods `PROJ healpix` and `PROJ rhealpix` are read; terrio
writes the WKT1 form, as GDAL does. PROJJSON with these methods is not read, and PROJJSON
export refuses them, as for other projections without an EPSG method.

On the edge of the image (the poles, the antimeridian of the central meridian and the edges of
the polar caps) the last bit of a projected coordinate moves a point in or out of the image.
There a US survey foot in the PROJ string's `+units` can decide it: PROJ 9.8.1 takes that unit
in these methods' own `+units` as 0.304800609601219 m, and terrio, as PROJ's `unitconvert` for
other methods, as 1200/3937 m.

The `method-healpix` and `method-rhealpix` fixtures keep their PROJ 9.9.0 oracles. A PROJ
9.8.1 corpus covers 22 configurations and 6,600 samples (see Testing). Allowances are 1 mm
projected and 1e-8 degrees inverse.

## Icosahedral Snyder Equal Area

`+proj=isea` is computed as PROJ 9.8.1 does (`isea.cpp`): Snyder's equal-area projection of the
sphere onto the 20 faces of an icosahedron, unfolded in the plane. `+orient=isea` (the default)
places the poles on edge midpoints and makes the equator symmetric; `+orient=pole` places a
vertex at each pole. As in PROJ, `+lon_0` and `+lat_0` set the icosahedron's orientation, and
`+lon_0` is also the central meridian; `+azi` rotates it. `+units`, `+x_0` and `+y_0` apply.
`+mode=plane` is the default; `+mode=di`, `dd` and `hex`, which give grid addresses rather than
coordinates, are refused. Invalid `+orient`, `+mode`, `+resolution` and `+aperture` values are
refused, as in PROJ.

As in PROJ 9.8.1, the inverse is defined only for the default grid (`+resolution=4`,
`+aperture=3`, no `+azi`) whose origin, after any `+lon_0` and `+lat_0`, is exactly the default
orientation's (which `+lon_0=11.25` keeps) or the pole orientation's (which
`+lat_0=90 +lon_0=0` gives), and refuses points outside the unfolded icosahedron; elsewhere it
is refused. On an ellipsoid the forward takes the geodetic latitude on a sphere of the
semi-major axis, while the inverse uses the authalic radius without the authalic latitude, so
the inverse does not return the projected point: on WGS 84, (147, -42) projects and unprojects
to (147.166, -41.969), in PROJ as in terrio.

GDAL's WKT1 (`custom_proj4`, which carries the PROJ string), ESRI's `PROJ_isea` (with
`PROJ_isea_orient=pole` and other options in the method's name) and PROJ's WKT2 method
`PROJ isea` (`PROJ isea orient=pole`) are read; terrio writes the WKT1 form, as GDAL does.
PROJJSON export refuses `isea`, as for other projections without an EPSG method.

Other PROJ versions differ: PROJ 9.4 has no inverse and a forward up to 2 mm different,
and PROJ 9.9 replaced the method with a new implementation (another default origin, the
authalic latitude on an ellipsoid, other parameters). The `method-isea` fixture, from PROJ
9.9.0, is therefore not used. A PROJ 9.8.1 corpus covers 14 configurations and 7,784 samples
(see Testing). Allowances are 1 mm projected and 1e-8 degrees inverse.

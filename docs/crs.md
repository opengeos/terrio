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
`crates/terrio-crs/tests/proj_strings.rs`) and refuses the 31 others (`airy`, `bacon`, `igh`, `nsper`,
`tpers`, `vandg2` and others) rather than computing something else. The known difference is
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
geographic CRSs: `is_geographic()` is false, and `is_rotated_pole()` is true. Other `ob_tran`
forms, and other WKT2 deriving conversions, are errors.

Coordinates are always in GIS order: x (easting, longitude) first, then y (northing, latitude),
whatever the axis order the authority defines.

## Standalone Rust crate

Applications that only need coordinate systems can depend on `terrio-crs` rather
than Terrio's raster/vector library:

```toml
[dependencies]
terrio-crs = "0.1"
```

The [published crate](https://crates.io/crates/terrio-crs/0.1.0) works offline by
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
targets. TIFF grid codecs retain their previous features, including native Zstd.
The [dependency and build measurements](crs-footprint.md) record both standalone
configurations and full Terrio before/after extraction. Extraction and validation are tracked in
[#268](https://github.com/opengeos/terrio/issues/268). The new crate does not add
geocentric, PROJJSON, IAU or additional projection support by itself.

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
ellipsoidal heights. Vertical units (US survey feet) and depths are converted. When no geoid
model applies, heights are kept, as PROJ's ballpark vertical transformation does. Between 2D CRSs,
heights are kept as GDAL keeps them (or transformed, for the pipelines PROJ runs on heights).

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
options are supported. Geographic inputs remain longitude, latitude, ellipsoidal
height in traditional GIS order. Geocentric conversions compose with projected
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

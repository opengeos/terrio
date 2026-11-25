# Coordinate Systems

## Defining a CRS

Everywhere a CRS is expected (`--t-srs`, `--a-srs`, `terrio crs`, the APIs), terrio accepts:

- EPSG codes: `EPSG:32613`, `32613`, `urn:ogc:def:crs:EPSG::32613`,
  `http://www.opengis.net/def/crs/EPSG/0/32613`, including geographic 3D CRSs (`EPSG:4979`) and
  compound CRSs (`EPSG:5498`);
- Horizontal plus vertical EPSG pairs: `EPSG:4326+5773` (WGS 84 with EGM96 heights), `EPSG:2154+5720` and
  `urn:ogc:def:crs,crs:EPSG::2154,crs:EPSG::5720`. Both components are retained,
  including in GML and CityGML metadata. Compound URNs require exactly two
  components: a two-dimensional horizontal CRS followed by a vertical CRS.
  Unsupported authorities, missing components and invalid component roles are errors.
  This also rejects previously accepted strings with a 3D horizontal component such as
  `EPSG:4979+5720`; string references require two horizontal axes before adding a height.
  An unversioned URN component needs the empty version separator (`crs:EPSG::2154`);
  the formerly accepted CityGML form `crs:EPSG:2154` is refused.
  The native GML reader rejects an unsupported compound CRS when it is selected
  for a layer, rather than keeping one component or silently discarding that metadata.
  A geometry's `srsName` takes precedence over the collection envelope's reference;
  an unused unsupported envelope CRS can therefore be ignored. The optional
  GDAL fallback can read additional authorities and follows GDAL's driver settings.
  WFS service metadata retains its existing permissive handling of unrecognized CRSs;
  the strict unsupported-compound refusal here applies to native GML/CityGML reads.
  GML layers refuse differing resolved geometry CRSs, including vertical-component
  differences, instead of assigning the first geometry's metadata to every feature.
  GML and WFS `srsName` values are parsed as literal metadata and never as local CRS files.
  Terrio retains the compound metadata in GML; GDAL requires
  [`GML_REPORT_COMPD_CS=YES`](https://github.com/OSGeo/gdal/blob/v3.13.3/ogr/ogrsf_frmts/gml/ogrgmldatasource.cpp#L1601)
  to report both GML components;
- `OGC:CRS84` and `WGS84`;
- ESRI codes, the geographic and projected CRSs of PROJ's ESRI authority: `ESRI:54009` (World
  Mollweide), `esri:102003`, `urn:ogc:def:crs:ESRI::102003`. The few whose projection terrio
  does not compute (Goode homolosine of the oceans, Peirce quincuncial and others) are refused;
- WKT1 parameters by GDAL's names or EPSG's (`Latitude of false origin`, `Longitude of projection
  centre`, `Angle from Rectified to Skew Grid`, variant B's `Latitude of standard parallel`), as
  PROJ 9.9.0 reads both; GDAL's `Polar_Stereographic` without `latitude_of_origin` (with GDAL's
  `standard_parallel_1` or EPSG's standard parallel alone) is refused, as PROJ refuses it. A
  `PROJCS` with two `UNIT` nodes is in the first, as PROJ takes it.
- WKT1 and WKT2 (2015 and 2019) definitions, including ESRI `.prj` WKT and WKT2 `BOUNDCRS`s with a
  Helmert transformation, recognized and applied as PROJ 9.9.0 does: the 25 methods of its table
  (geocentric translations, position vector and coordinate frame in each domain, coordinate
  frame's full matrix with its rotation exact, geog3D to compound, the time-dependent methods
  without their rates and reference epoch, which a `BOUNDCRS` has no coordinate epoch for, and
  Molodensky-Badekas about its evaluation point), by their EPSG code or name, and other names by
  the words of their convention with their kind's number of parameters. One of that family PROJ
  does not apply (time-specific, which needs a coordinate epoch) is refused, as is any other
  method terrio does not apply but a grid's (a name PROJ does not know, such as `PositionVector`;
  Molodensky, EPSG 9604 and 9605, which PROJ applies), and a `BOUNDCRS` without its
  transformation: to WGS 84, as `+towgs84`, and to another CRS (ED50), followed by that
  CRS's own transformations to WGS 84, as PROJ concatenates them (into a CRS on that CRS's datum,
  when terrio identifies it, the shift alone; into one on the source CRS's own datum, no shift,
  whatever the target; to a CRS without transformations, the source CRS's own, else PROJ's
  ballpark); such a shift wins over the EPSG code of a source CRS terrio identifies, and over the
  `TOWGS84` of a WKT 1 source CRS (GDAL's `GEOGCS` or `PROJCS` in `SOURCECRS`). A target that is
  bound itself (a `BOUNDCRS`) is not chained through, as PROJ does not chain through it, nor
  through a WKT 1 target with its own `TOWGS84`; PROJ links such CRSs by its ballpark. A WKT 1
  `GEOGCS` target without one is chained through as PROJ 9.9.0 chains it, found by its codes or
  its datum's name (ESRI's `D_Monte_Mario`), for a shift `+towgs84` stands for; a full-matrix or
  evaluation-point shift to it is left out, as PROJ leaves it out. When a
  `BOUNDCRS`'s transformation is not applied (those cases, or a grid's transformation), each
  transformer using the CRS warns
  (`take_warnings`), and its points are placed without it (by the source CRS's own
  transformations when terrio identifies it, else by the ballpark). Their PROJ
  string leaves out a shift to a CRS other than WGS 84, as PROJ's does. Their scale difference is a
  ratio in unity (`1.0000067`, as PROJ writes it) or in the unit given (parts per million, where
  PROJ 9.9.0 misreads 6.7 ppm as the ratio 6.7, which terrio does not follow); without a unit, a
  ratio within 0.01 of 1, and farther from 1 (`0.6`, `7`) an error, where PROJ 9.9.0 reads any
  value as a ratio (`0.6` as -400000 ppm), since it may well be parts per million; an unknown WKT2
  projection method or parameter, or a Helmert transformation parameter, is an error rather than
  ignored (a target CRS terrio cannot convert is left out). A projected
  CRS's `AXIS` nodes give its axes in WKT 1 as in WKT 2, as GDAL and PROJ read them in GIS order:
  `AXIS["Y",WEST],AXIS["X",SOUTH]` counts coordinates west and south, and northing then easting is
  read as easting then northing; south-orientated Transverse Mercator is west and south but for
  north-east axes (south and west), and Krovak without them south and west. A WKT 1 `GEOGCS`'s
  `AXIS` directions count longitudes west and latitudes south as WKT 2's do (west-positive
  planetary longitudes), and a WKT 2 parameter without a unit is in degrees or metres, not in its
  CS's or base CRS's unit, as PROJ 9.9.0 reads them. South-orientated
  Transverse Mercator subtracts its false easting and northing (EPSG method 9808: W = FE - x,
  S = FN - y); PROJ 9.9.0 refuses one with offsets, and terrio writes it no PROJ string. A world
  projection given a latitude of natural origin its method has no use for (Robinson, Sinusoidal)
  is read without it, as PROJ ignores it. Its PROJ string carries
  only the axes PROJ writes: south-orientated Transverse Mercator's `+axis=wsu` and Krovak's
  `+axis=swu`, not a coordinate system's. Export follows PROJ 9.9.0 and can lose the WKT's
  coordinate mapping: north-east south-orientated Transverse Mercator transforms south-west but
  exports west-south; west-south Krovak exports without that axis order. Geographic and geographic
  BOUNDCRS exports also omit CS axes, while transforms retain them. Keep WKT for a round trip that
  preserves the declared axes;
- PROJJSON v0.7 definitions in the [supported object subset](#projjson-definitions);
- PROJ strings: `+proj=utm +zone=13 +datum=WGS84`, with `+nadgrids=` and `+geoidgrids=` for
  datum shift and geoid grids;
- the CLI and Rust's `SpatialRef::parse` also accept the path of a trusted regular file
  holding one of these. Python and JavaScript CRS strings, and CRS metadata stored in
  datasets, are parsed literally and never interpreted as local filenames.

When a Python caller deliberately wants a CRS from a trusted local file, read the text
explicitly, for example `CRS.from_user_input(Path("zone.prj").read_text())`. Rust callers
handling untrusted strings should use `SpatialRef::parse_literal`. Errors from the trusted
file-parsing form name the file without quoting its contents.

Definition-file paths may be symlinks to regular files. Pipes (including shell process
substitution) and devices are refused without reading them. Callers that intentionally
read a stream can pass its text as a literal definition instead. Native file opens check
the opened descriptor too; on Unix they use a nonblocking open to avoid waiting if a
regular path was replaced by a FIFO.

Engineering WKT roots (`LOCAL_CS`, `ENGCRS`, `ENGINEERINGCRS`) retain their local
coordinate-system definitions without being assigned an unrelated EPSG code. Both
`VERTCRS` and its long form `VERTICALCRS` read the identifier of the CRS itself;
identifiers on its datum, coordinate system or unit do not replace it. The root's
identifier is read wherever it is among the root's children: before a `REMARK` or
`AXIS`, or with a `URI` or `CITATION` of its own. With several identifiers, the
first that is not an ESRI identifier, nor an EPSG identifier whose code is not a
number or not a CRS of the root's kind, gives the code, as PROJ's `to_epsg()` does;
when all are passed over there is none. So an identifier of another authority placed
before the EPSG one leaves no EPSG code. An engineering or bound root takes a
registered code of any kind. An ESRI code is read the same way, passing over EPSG
identifiers instead. A single identifier gives its code whatever it is. These forms
work in Rust, CLI/MCP, Python and JavaScript. Parsing engineering metadata does not
add transformations between arbitrary local grids and Earth-based CRSs.

ESRI's vertical CRS, `VERTCS`, is read as `VERT_CS` is, as PROJ 9.9.0 reads it: its
declared code is kept when its unit and direction are the code's, and
`PARAMETER["Direction",-1]` (exactly -1) is an axis down unless an `AXIS` says otherwise.
ESRI's compound CRS, a `GEOGCS` or `PROJCS` followed by a `VERTCS` (`GEOGCS[...],VERTCS[...]`,
ESRI's WKT of EPSG:9707), is read as a `COMPD_CS` named after its two parts, and so written;
after a `VERT_CS` or a WKT 2 root, a second node is ignored, as by PROJ. A vertical CRS
without a code (`VERTCS`, `VERT_CS` or `VERTCRS`) is the EPSG CRS PROJ identifies by its name
(an EPSG name or alias in any case, ESRI's `EGM96_height` included) on the datum its datum's name
is, in the same unit and direction; it then has that CRS's geoid models. GDAL's `PROJ4_GRIDS`
extension in its vertical datum, where GDAL writes it, makes it its grids, of no code, even with
a declared code, as PROJ reads it (a bound CRS); one at its root, which PROJ ignores, is ignored
beside a declared code of its unit and direction, and otherwise read as before (no code, its
grids, where PROJ identifies the CRS by its name). Its PROJ string's `+geoidgrids` are the same
grids. A compound CRS gets the EPSG code it declares when its content fits it; without a code
of its own, it is identified by its parts (see Identification). Its parts are known when its
vertical CRS is identified.

Where a WKT 2 CRS has two nodes PROJ reads one of, the first in its order counts, whatever its
keyword, as in PROJ 9.9.0: of two datums (`DATUM`, `TRF`, `GEODETICDATUM`; a datum and an
`ENSEMBLE`, which PROJ refuses, by the first too), two ellipsoids (`ELLIPSOID`, `SPHEROID`) or
two base CRSs (`BASEGEOGCRS`, `BASEGEODCRS`). A WKT 2 ellipsoid is in its `LENGTHUNIT`
(PROJ's WKT 2 of 32 EPSG CRSs, EPSG:4157 for one, gives it in feet). GDAL's root code, which
binds a CRS to WGS 84 in its PROJ string export, is the root's first `AUTHORITY` or `ID`.

GeoTIFF and COG writers retain engineering definitions, including root identifiers,
as full WKT in an ESRI PE citation with user-defined model type 32767 and a general
citation. Readers keep that definition even when its identifier is absent from
Terrio's EPSG table. Compact geographic/projected GeoKeys are reserved for registered
horizontal CRSs; other coded definitions retain their WKT. This follows the model
type and citation requirements in [GeoTIFF 1.1](https://docs.ogc.org/is/19-008r4/19-008r4.html).
This change adds these four root spellings. Other definition-shaped metadata,
including `FITTED_CS`, `DERIVEDPROJCRS`, `PARAMETRICCRS`, `TIMECRS` and `IMAGECRS`,
can be preserved by format adapters without being supported by the shared parser.

Of PROJ's 117 projections, terrio computes 89 as PROJ does (within 1 mm, on a sphere and on WGS 84,
as PROJ strings with typical parameters and as pyproj's WKT1 and WKT2 of them:
`crates/terrio-crs/tests/proj_strings.rs`) and refuses the 28 others (`airy`, `bacon`, `goode`,
`vandg2` and others) rather than computing something else. The known difference is
Krovak off Bessel's ellipsoid, where PROJ's inverse is not the inverse of its forward.

Polar Stereographic (variant C), EPSG method 9830, puts its false origin on the standard parallel
φF rather than at the pole, and no `+proj=` string names it. terrio reads it, from EPSG:2985 and
EPSG:2986 (Petrels 1972 and Perroud 1950 / Terre Adelie Polar Stereographic) and from WKT1, WKT2
and PROJJSON definitions that name it, in both configurations of `terrio-crs`, as variant B on the
same standard parallel, whose pole lies at the northing at the false origin minus
ρF = a cos φF / (1 − e² sin² φF)^0.5 (plus ρF for the north pole case). For EPSG:2985 this is a
false northing of −2,299,363.488 m. `to_projjson` writes the CRS as variant B with this false
northing, and `to_gdal_projjson` refuses method 9830. Definitions must supply a finite standard parallel
within −90° to 90°, longitude of origin, and easting and northing at false origin. Incomplete
definitions and invalid parameter values are refused.

Mollweide and Wagner IV/V use Terrio's own ports of PROJ's formulas, including
PROJ's finite Mollweide pole approximation. Their inverses judge antimeridian
rounding on the normalized projected ellipse, allowing its radius up to
`1 + 8 * f64::EPSILON`. Longitude estimates at or above `π - 8 * ulp(π)` in
absolute value snap to the corresponding antimeridian if they pass that radius
check; points farther outside the ellipse remain errors. The resulting longitude
allowance grows toward the poles because inversion amplifies projected-coordinate
rounding there. This deliberately differs from PROJ's strict exclusion of the edge
in GDAL 3.13.3, avoiding refusals caused by platform libm rounding.
The orthographic inverse intersects
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

Inverse projections refuse non-finite input coordinates before evaluating their formulas.
In particular, a failed transform's `(inf, inf)` marker cannot become a valid-looking opposite
pole through a polar stereographic inverse. Finite distant coordinates remain valid where the
projection permits them. If conversion to metres overflows before normalization by the
ellipsoid radius, the inverse combines those scale factors to preserve finite normalized
coordinates and their direction. Unrepresentable normalized coordinates are refused.
This is stricter than GDAL 3.13.3 for some infinity pairs, such as
`(0, -inf)` in EPSG:3413, which GDAL accepts as a pole.

## Standalone Rust crate

Applications that only need coordinate systems can depend on `terrio-crs` rather
than Terrio's raster/vector library:

```toml
[dependencies]
terrio-crs = "0.3.0"
```

The [published crate](https://crates.io/crates/terrio-crs/0.3.0) works offline by
default. Enable `wbprojection-fallback` when Terrio's wider projection coverage
is required; the feature matrix below describes the supported configurations.
Since 0.2.4, Eckert IV and Natural Earth are in the default build; 0.2.3 and earlier
provide them only through that feature. The native method counts in the matrix below
count the 117-method test corpus only. They are the same in 0.3.0 as in 0.2.4: neither the
Transverse Mercator Zoned Grid System that 0.3.0 adds nor `isea`, which now follows PROJ 9.9.0's
algorithm, is in that corpus.

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
| Default | EPSG/ESRI tables, WKT and PROJ strings, 45 native methods from the 117-method test corpus, datum operations, epochs, and local or registered grids; offline |
| `wbprojection-fallback` | 44 additional projection methods and legacy WKT/identification fallback; enabled by Terrio |
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
`terrio_crs::take_warnings()`. Terrio's `terrio::warnings::take()` drains both
Terrio's queue and the standalone queue on the current thread, combining duplicate
messages. Thus a Terrio drain also consumes any pending direct-caller diagnostics;
callers mixing the APIs should use one drain or take standalone diagnostics first.
Each queue retains its own order, but the combined order is not chronological
across queues. A custom transformer warning handler is also available.

The standalone default omits `wbprojection`; Terrio enables the fallback to retain
its existing coverage. Bundled tables use portable `zrip` decompression on all
targets. The standalone default has no C dependency: Zstandard TIFF grids are read only with the
`libzstd` feature, which uses the zstd C library and which Terrio does not enable. The PROJ CDN's
grids are Deflate-compressed (all 11 sampled, among them those this page and the tests name), so this
concerns grids recompressed with Zstandard.
The [dependency and build measurements](crs-footprint.md) record both standalone
configurations and full Terrio before/after extraction. Extraction and validation are tracked in
[#268](https://github.com/opengeos/terrio/issues/268). Remaining projection coverage is tracked in #272.

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
The legacy unversioned `urn:x-ogc:def:crs:EPSG:4326` form is now accepted by both
the native parser and the fallback;
versioned X-OGC forms remain unsupported. Use the modern versioned OGC URN below.
An unversioned modern OGC URN requires the empty version separator, as in
`urn:ogc:def:crs:EPSG::4326`; the fallback now refuses the single-colon form
`urn:ogc:def:crs:EPSG:4326` that its former free-form parser accepted.
Versioned URNs/URLs resolve the code separately from the authority version
(`urn:ogc:def:crs:EPSG:6.6:4326` resolves EPSG:4326). The fallback retains its
legacy numeric suffix handling only after a recognized authority prefix fails
canonical parsing; unrelated text is never searched for an EPSG code;
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
EPSG:2263 in both, and ESRI's `GCS_European_1950` is EPSG:4230. A WKT 1 CRS on a datum no CRS is
on has no code of its own, and is placed by a ballpark as PROJ 9.9.0 places it; its geographic CRS
(`SpatialRef::geographic_code`) is the one PROJ identifies it as by its name, on a datum with which
its own has a name in common, by the name of its `GEOGCS`: ESRI's `GCS_Greenland_1996` on
`D_Greenland_1996` (Greenland 1996), and the projected CRSs on it, are on GR96, EPSG:4747, on the
GR96 ensemble. Under another name, or in WKT 2, there is none, as in PROJ.

Ellipsoids are compared as PROJ 9.9.0 compares them: the semi-major axis, the semi-minor axis and
the inverse flattening each within 1e-10. GRS 1980 is not WGS 84, so EPSG:6986's WKT (IGD05/12 on
GRS 1980) is not EPSG:7138 (IGD05/12 on WGS 84). A compound CRS without a code of its own is the
registry's compound CRS of its horizontal and vertical CRSs, each identified: EPSG:7405's WKT
without its codes is EPSG:7405, and ESRI's `GEOGCS[…],VERTCS[…]` of WGS 84 and EGM96 height is
EPSG:9707, as PROJ and GDAL identify them.

A code the WKT declares at its root (`AUTHORITY["EPSG","27700"]`, `ID["EPSG",27700]`) is kept only
when the WKT's content is that CRS: the same conversion, units and axes included, on the same
ellipsoid and datum, and of the same kind. The datum is compared by its EPSG code, from the WKT's
`AUTHORITY` or `ID` or as its name identifies it, else by name: ETRS89 declaring NAD83 is not NAD83,
and a datum terrio does not know is not the code's unless named as it or `unknown`, PROJ's
placeholder. A geocentric WKT is not a geographic code, nor a geographic 2D WKT 2 a 3D code (a WKT 1
`GEOGCS` may be either, as in PROJ). A vertical CRS keeps only a vertical code; an engineering or
local CRS keeps no code terrio places. Otherwise the WKT is read and identified by its content, as PROJ 9.9.0 reads it: a projected
CRS declaring a geographic code, a code that does not exist, or a 2D CRS in degrees declaring a
compound code in grads is not that code. When terrio cannot read the content (a method only the
registry has), the code is kept if the WKT names the code's own projection method; an unknown method
stays unusable. A declared ESRI code is kept only on the code's ellipsoid, all of it (an
altered flattening is not ESRI:54009, as PROJ matches it at 25%). An ESRI CRS is identified by its
name only on its datum and ellipsoid: ESRI's
`Monte_Mario_Italy_1` on a datum renamed `D_Local` has no code, and PROJ places it by a ballpark.

A datum terrio does not know by name and without `TOWGS84` (`Unknown based on Bessel 1841
ellipsoid`, as GDAL and PROJ write a PROJ string's `+ellps=bessel`) gets no EPSG code and no datum
shift, as in GDAL: the CRS is used as it is.

A WKT's datum known only by its EPSG ID (WKT 2 `DATUM[...,ID["EPSG",6277]]`, WKT 1
`AUTHORITY["EPSG","6277"]` on the `DATUM`), whatever the datum's and the CRS's names, or by its
name alone (`Ordnance Survey of Great Britain 1936`, ESRI's `D_OSGB_1936` and `D_Monte_Mario`), gets
the datum transformations of that datum's EPSG geographic CRS, as PROJ transforms it. Where PROJ
does not resolve the WKT's geographic CRS to an EPSG CRS (ESRI's WKT 1 has no `AXIS`, so longitude
first, unlike the EPSG CRS; or the CRS has another name), it also searches transformations through
another datum, and terrio follows it (see "Transformations"): ESRI's `GCS_Monte_Mario_Rome` through
Monte Mario (72 m from EPSG:4806's own), `Tokyo_1892` through Tokyo (390 m), and ONGD14 by its
geocentric transformation (1.2 m from the EPSG CRS's).
Without grids, this is PROJ's own choice; with OSTN15, BETA2007, CHENyx06 or the NZGD2000 grids
installed, PROJ may place some of these points otherwise (46 of the 180 test cases; see
`docs/testing.md`). The repository's evidence is those 180 cases on 20 CRSs (a wider sweep of
3,648 variants was a local run whose script and data are not committed).

On another ellipsoid than its datum's (ESRI's `D_Monte_Mario` on an International 1924 of another
axis), the datum's transformation runs on the WKT's ellipsoid, as PROJ 9.9.0 runs it. A null
transformation ("TWD97 to WGS 84 (1)") is still geocentric, from the WKT's ellipsoid to WGS 84's:
TWD97 on International 1924 is placed 70 m from where its degrees are. A transformation registered
between geocentric CRSs (ONGD14 to WGS 84 (1), NAD83(CSRS)v6 through NAD83(2011)) runs on the
datum's own ellipsoid, after PROJ's ballpark into the datum's CRS, which keeps the point. Heights
are kept between 2D CRSs. A datum name PROJ finds wins over the datum's ID there: `Greenland_1996`
with GR96's ID is the former Greenland 1996 datum, which no current CRS has, and is placed by a
ballpark (`cargo test -p terrio-crs --test esri_datum_altered_ellipsoid`).

A WKT 1's angles are in its geographic CRS's angular unit: NTF (Paris) (EPSG:4807) and its Lambert
zones (EPSG:27572) have their parameters in grads, and coordinates in a geographic CRS in grads are
in grads. Its prime meridian is in that unit too, but where PROJ 9.9.0 reads it in degrees: a
`PRIMEM["Paris",2.33722917]` in grads (NTF (Paris) as GDAL and ESRI write it), and a meridian PROJ
knows by name for this (Copenhagen is not one) with its value in degrees or EPSG's sexagesimal
degrees, whatever the unit (`PRIMEM["Paris RGS",2.33720833333333]` in ATF (Paris), in grads;
`PRIMEM["Ferro",-17.4]`, 17°40'W). A WKT 2 prime meridian is in its own unit; without one, in the
unit of its CRS (or base CRS) given as an `ANGLEUNIT`, or as a `UNIT` named `degree` or `grad`,
which PROJ takes for an angle; else in degrees, never in its axes' unit (`UNIT["radian",1]` or
`UNIT["gon",...]` leaves it in degrees, as does a geocentric CRS's `LENGTHUNIT`). The same
corrections apply: `PRIMEM["Ferro",-17.4,ANGLEUNIT["degree",...]]` is 17°40'W. ESRI's Krovak
`XY_Plane_Rotation` is in the CRS's unit as well (100 grads for 90°, east-north). As PROJ does
between CRSs, a prime meridian within 1e-10 radians of Greenwich or one of PROJ's named meridians
(`+pm=`) is that meridian when transforming: Paris's 2.33722917° (2.5969213 grads) is `+pm=paris`,
2.337229166667°, 0.25 mm west. The CRS is still written with its own value
(`PRIMEM["Paris",2.33722917]`), as PROJ writes it.

ESRI's `Behrmann` projection is cylindrical equal area true at 30 degrees, or at the
`Standard_Parallel_1` it gives (EASE-Grid 2.0), PROJ's `+proj=cea +lat_ts=30`.

ESRI's `Mercator_Variant_A` (EPSG:3002, Makassar / NEIEZ, in an ESRI `.prj`) is Mercator (1SP), its
`Scale_Factor` 1 when not given; as in PROJ, a `Latitude_Of_Origin` or `Standard_Parallel_1` it
gives is left out. EPSG's name in WKT 1, `Mercator (variant A)` (or `Mercator_(variant_A)`), is read
the same way, but with a `Latitude_Of_Origin` (or `Latitude_of_natural_origin`) other than 0 it is
refused, as PROJ reads it but neither transforms it nor writes it as a PROJ string.

A node of a keyword terrio does not know is skipped, whatever its keyword, as PROJ skips it:
`vendor:mark["x"]`, `a-b["x"]`, `a"b c"["x"]` and an empty keyword (`["x"]`) are read past. A
keyword ends at a space (C's `isspace`, the vertical tab included) or a delimiter, outside a quoted
run, and a keyword with a space, a NUL or an unclosed quote in it (`a b["x"]`, `a"b["x"]`) makes the
WKT one terrio refuses, as PROJ does. A quote in a value runs to the next quote, delimiters
included, as in PROJ 9.9.0: `x["a",b"c,d"e]` is a node of two values, and a quote left unclosed
makes the WKT one terrio refuses. A definition given as text (WKT, PROJJSON, any text
`SpatialRef::parse` takes) ends at its first NUL, as PROJ and GDAL take it as a C string, and so
does a GeoTIFF's GeoAsciiParamsTag, as GDAL 3.12.1 reads it with libgeotiff, in GeoTIFFs and LAS
files alike (a key that spans the NUL is cut there, one that ends at it is read without it, and any
other that ends after it leaves the GeoKeys without a CRS, their EPSG code included; offsets count
bytes, also after bytes that aren't UTF-8, which become U+FFFD in their own key; without the tag,
any ASCII key leaves no CRS); text after a complete WKT was already ignored. A layer's or raster's
WKT that the NUL cuts inside a quoted name or a node (an open quote or bracket left) is no CRS, as
GDAL reads it, and none is written from it; one only padded with NULs is the text before them,
whatever terrio reads of it. A NUL a PROJJSON name gives (`\u0000`) is part of the name, as in PROJ.
Written to any file or its metadata (a layer's or raster's WKT, a `.prj`, GeoPackage, GeoParquet,
netCDF, VRT, EPT, a LAS WKT record) or to GeoKeys, such a name is cut at the NUL, as PROJ 9.9.0
gives the name (its own WKT of the CRS ends at the NUL), so the CRS reads back; the cut name can
change what the CRS is identified as by name, as in PROJ, and a CRS with one that GeoKeys can't
describe, whose WKT would go in an ESRI PE citation, is refused, as GDAL 3.12.1 refuses to write it.
WKT 1 and WKT 2 share this parser, as do the readers of PROJJSON and of IAU definitions, which go
through WKT, so an unknown node is skipped in each.

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

## Listing registry CRSs

An application can list the CRSs terrio resolves, for a searchable CRS picker, without GDAL's
`OSRGetCRSInfoListFromDatabase`, network access or a PROJ installation:

```rust
use terrio::crs::{CrsKind, CrsQuery, SpatialRef, list_crs};

let query = CrsQuery { kinds: vec![CrsKind::Projected], text: Some("albers".into()), ..Default::default() };
for entry in list_crs(&query) {
    println!("{} {} {:?}", entry.authority_code(), entry.name, entry.area_name);
}
let albers = SpatialRef::parse("EPSG:3577")?;
# Ok::<(), terrio::Error>(())
```

```python
terrio.list_crs("albers", authority="EPSG", kind="projected")
```

```js
listCrs("albers", { authority: "EPSG", kind: "projected" });
```

```sh
terrio list-crs albers --authority EPSG --kind projected [--json]
```

(`terrio_crs::list_crs` in the standalone crate.) Each entry has the CRS's authority and code,
name, kind (`geographic_2d`, `geographic_3d`, `geocentric`, `projected`, `vertical`, `compound`,
or `other` for IAU_2015's planetocentric CRSs with spherical coordinates), whether it is
deprecated, and for a deprecated EPSG CRS the codes of those that replace it (`replaced_by`, as
PROJ's `proj_get_non_deprecated` lists them: `[5105]` for EPSG:4855), the name and bounding box
(west, south, east, north, in degrees) of its area of use, its projection method and its celestial
body, as GDAL gives them: for a CRS with several areas of
use, the one `OSRGetAreaOfUse` reports. Entries come in order of authority, then code. The query
filters by authorities, kinds and words, each of which must be in the name or begin the code or
`AUTHORITY:CODE`, whatever their case (`Albers`, `3577` and `EPSG:3577` all find EPSG:3577); it
leaves out deprecated CRSs unless `include_deprecated` is set. `"{authority}:{code}"` of every
entry is a CRS terrio accepts. A deprecated EPSG code keeps its own definition, as PROJ gives it
(`CRS.from_epsg`): EPSG:4855, the deprecated ETRS89 / NTM zone 5, has its false northing of
1,000,000 m and latitude of origin 0, not those of its replacement EPSG:5105. GDAL substitutes the
replacement by default (`OSR_USE_NON_DEPRECATED=YES`); `SpatialRef::from_epsg_non_deprecated`
does the same, for a code with a single replacement.

The list is a snapshot of PROJ 9.9.0's database: EPSG v13.102 (2026-08-27,
`terrio::crs::EPSG_VERSION`), ESRI ArcGIS Pro 3.6 and IAU_2015, generated by
`scripts/gen_crs_catalog.py` (164 KB compressed). It holds one row per code. PROJ's 8,021 EPSG usage rows contain 17 duplicate codes:
8,004 distinct codes minus the 43 terrio's table does not hold gives 7,961 entries, all
listed.

| Authority | Listed | Of which deprecated | Not listed (GDAL lists them) |
|---|---:|---:|---|
| EPSG | 7,961: 718 geographic 2D, 300 geographic 3D, 296 geocentric, 5,754 projected, 309 vertical, 584 compound | 530 | 16 engineering and other CRSs, 26 projected CRSs GDAL does not import (Qornoq 1927 Greenland zones...), and the deprecated EPSG:3752 (Mercator variant A with a latitude of origin, which PROJ neither writes as a PROJ string nor transforms) |
| ESRI | 2,582: 323 geographic 2D, 14 geographic 3D, 2,245 projected | 1,339 | 272 vertical CRSs; 128 geographic and projected CRSs terrio's ESRI table leaves out (119 of other bodies than the Earth, and those PROJ does not transform from WGS 84); 37 on projections terrio does not compute without the `wbprojection-fallback` feature of `terrio-crs` (9 of them with it, Wagner VII, Compact Miller, Peirce quincuncial...) |
| IAU_2015 | 2,201: 115 geographic 2D, 2,074 projected, 12 other | 0 | none |

The conformance suite `crs-list` (`scripts/compare_crs_list_with_gdal.py`) compares every listed
CRS with GDAL 3.13.3's list (PROJ 9.9.0), field by field: all are identical.

## PROJ string interpretation changes

Recognized boolean switches, including UTM and UPS `south`, accept a bare flag
or an explicit case-insensitive value: `true`, `t`, `yes`, `y`, `on`, `1` enable
it; `false`, `f`, `no`, `n`, `off`, `0` disable it. Other values are refused.
This differs from treating every supplied value as an enabled flag. Python's
`terrio.rio.CRS.to_proj4()` writes enabled flags as `+flag` and omits disabled
ones; `to_dict()` keeps boolean types.
Flags combined with `+init=epsg:N`, except the metadata switches `no_defs` and
`wktext`, are refused because that path cannot apply overrides to the registry
definition. Use an expanded `+proj` definition when changing these flags.

A value PROJ 9.9.0 cannot read is refused when the method reads the parameter, as PROJ refuses it
("invalid value for lat_ts"), rather than taken as the parameter's default: `+proj=eqc +lat_ts=abc`,
an unreadable `+towgs84` or `+pm`. A `towgs84` list must contain three or seven numeric values.
An angular value ending in `r` is in radians (`0.5r`, as PROJ's `dmstor` reads it).
Scale factors retain their existing numeric-alias policy: `+k_0=0.5r` uses a fully
numeric `+k`, or defaults to 1 without one. This is a known difference from PROJ, which can use
the numeric prefix 0.5 or reject the CRS, depending on the method.
A known `+units` comes before `+to_meter`, as in PROJ, and `+to_meter` is otherwise
refused unless it is a positive number (likewise `+vunits` and `+vto_meter` with `+geoidgrids`); a
scale factor is refused unless the number it starts with is positive (NaN passes, as in PROJ).
Names PROJ 9.9.0 does not know are refused, as it refuses them: an `+ellps` (unless beside `+R`,
which PROJ reads instead), a `+datum`, a `+units` or a `+vunits`, compared as written (`wgs84`,
`meter` and `feet` are unknown), and a `+vto_meter` that is not a positive number. Angles are read
as PROJ's `dmstor` reads them: `40d30'N`, `40D30`, `40°30'`, `0.7rS`; a value with text after the
angle (`9d-30`, `40.5NS`) or that is not finite is refused. `lat_0`, `lon_0`, `x_0` and `y_0` that
are not finite numbers (or angles) are refused for the methods PROJ's CRS parser maps them for
(`+proj=tmerc +lat_0=abc`), and read as absent for the others (`+proj=robin +lat_0=abc`), as PROJ
reads them; `+f`, `+es` and `+e` without `+a` stay as lenient as before. A WKT's `PROJ4` extension
is read as PROJ reads a pipeline step, without refusing the values PROJ's CRS parser would (an
unreadable `+lat_ts` or `+lat_0` is read as 0, and an angle by its longest readable start: `9d-30`
is 9°), with a `+vto_meter` fraction (`1200/3937`); unknown names, the scale-factor, `+to_meter` and
`towgs84`-length refusals still apply.
Unreadable `towgs84` lists inside extensions retain the legacy behavior of ignoring the
shift. This differs from PROJ, which can interpret unreadable elements as zero.

PROJ parameter exports preserve projection-specific standard parallels, the
validated PROJ4 extension of a WKT definition (including Web Mercator), named
PROJ datums and explicit WKT `TOWGS84` operations. A datum with neither a PROJ
name nor an explicit shift is exported as an ellipsoid with a warning that
its identity and coordinate operations are omitted. An ellipsoid is not a
datum: a round trip can then select a ballpark operation and move points.
Use `to_wkt()`, PROJJSON or the authority code to preserve such datums and
their area-dependent or grid-based operation selection. Unsupported projection
exports remain empty rather than describing another projection.
An explicit `TOWGS84` operation takes precedence over a named datum and exports
as the ellipsoid plus that shift. A recognized datum name with a custom spheroid
and no effective registry identity exports the supplied ellipsoid with a warning.
An ordinary WKT retaining an outer EPSG identity uses the registry definition
for both transformations and parameter exports, even if its nodes were edited.
Parameter exports warn when they detect WKT1 projection or datum parameters conflicting with that
registry definition. Remove that identity when defining a custom CRS. Mapping constructors with
`init` retain the authority and ignore non-switch parameters for compatibility;
recognized boolean overrides are still validated and refused as described above.

Python's existing WKT-to-parameter adapter handles these rasterio-compatible
dictionary exports. Its canonical datum spheroids come from the shared native
CRS reader and are cached in Python; projection and datum transformations remain
in Rust. The adapter preserves WKT standard-parallel conventions when producing
PROJ's parameter names, including negative southern polar standard parallels.

Transforms retain authority-based operation selection for unbound WKT and dataset
CRSs. WKT containing an explicit `TOWGS84`, `BOUNDCRS` or bound transformation keeps
that operation even when it also declares an EPSG code. This follows PROJ's bound
CRS interpretation and can differ from rasterio's authority normalization of
GDAL-written WKT1. Tests add synthetic shifts to EPSG:27700 and EPSG:23031,
serialize the resulting WKT with GDAL and compare it against PROJ within 1 mm.
These optional reference tests require both GDAL Python bindings and pyproj;
the real-data CI job installs them, while the ordinary Python job can skip them.

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

A PROJ string's ellipsoid is read as PROJ 9.9.0 reads it with `+type=crs`:

- `+R` makes a sphere and overrides every other ellipsoid parameter.
- Otherwise the ellipsoid is `+datum`'s, else `+ellps`' (`+ellps` beside `+datum` is ignored),
  else WGS 84. `+a` changes its size. Of `+rf`, `+f`, `+es`, `+e` and `+b`, only the first
  given counts. On a named ellipsoid or datum, only `+b` changes the shape, or `+rf` on an
  ellipsoid PROJ defines by its inverse flattening. `+f`, `+es` and `+e` are ignored.
  `+ellps=clrk66`, `+ellps=sphere` and other ellipsoids PROJ defines by their semi-minor
  axis keep that axis under `+a`. A datum's ellipsoid is defined by its inverse flattening,
  NAD27's Clarke 1866 too, so `+a` and `+rf` change it as they change `+ellps=WGS84`. So
  `+datum=WGS84 +a=6400000` keeps WGS 84's flattening, and `+ellps=WGS84 +f=0` is WGS 84.
  Without a named ellipsoid, `+a` and the first shape parameter give the ellipsoid, and a
  shape parameter without `+a` is refused.
- Ellipsoid parameters beside `+datum` make PROJ drop the datum for an unnamed one on its
  ellipsoid, so `+datum=NAD27 +rf=300` has no NAD27 transformation.
- `+f=0`, written so, makes orthographic spherical on the CRS's ellipsoid (ESRI:102035,
  `+proj=ortho +f=0 +datum=WGS84`). It does not on an ellipsoid PROJ writes by its inverse
  flattening (`+datum=WGS84 +a=6400000 +f=0`).
- `+R_A` on `laea` and `cea` is the method's spherical variant on the CRS's ellipsoid.
  PROJ's parser reads parameter names regardless of case, so `+R_a` is `+R_A` there. Miller
  and van der Grinten are on the authalic sphere without a flag. Any other spherification
  flag (`+R_A` elsewhere, `mill` and `vandg` included, `+R_V`, `+R_a`, `+R_g`,
  `+R_h`, `+R_lat_a`, `+R_lat_g`, `+R_C`) makes PROJ compute on the string as a pipeline
  step reads it. In that case `+a` keeps the named ellipsoid's eccentricity, the first shape
  parameter changes its shape, and the first flag in that order makes a sphere of it.
  A `+datum=` name there shifts points from WGS 84 to that sphere as PROJ's `pj_datum_set`
  defines it (a null `+towgs84` for WGS84 and NAD83, the `+towgs84` or `+nadgrids` of the
  others), but under `+R_C`, where PROJ keeps the datum's transformations. Without an
  ellipsoid, that step is on GRS 1980.

A WKT's `PROJ4` extension (`EXTENSION["PROJ4", ...]`) is computed on as PROJ computes on it, as
a pipeline step: `+ellps=WGS84 +f=0` there is a sphere, `+ellps=clrk66 +a=6400000` keeps Clarke
1866's eccentricity, and an ellipsoid the step refuses (`+rf=0`, `+es=1`, a shape without a
size) is refused, as PROJ refuses to transform with it. Its `+datum=` is shifted as a step's is,
under `+R_C` too, and without an ellipsoid it is on GRS 1980. The extension stays in the WKT
(`SpatialRef::from_wkt` does not take it as the CRS's PROJ string), and PROJJSON and GeoKeys,
which describe a PROJ string by the CRS rules, refuse one the step places otherwise (by its
figure, beyond the round-off between WGS 84 and GRS 1980, or by its datum, as both place points
without grids whatever is installed: by the step's grids, or more than 1 mm apart around the
origin). `+datum=NAD83`, which the step takes as WGS 84, is described. A `+datum=`
with its grids (NAD27, potsdam) wins over the string's `+towgs84` in a step, and so does the
string's own `+nadgrids`: `+nadgrids=@null +towgs84=1000,0,0` is no shift, as in PROJ, and
`+datum=OSGB36 +nadgrids=@null` keeps OSGB36's datum (the identity into EPSG:4277), with no
shift to WGS 84, and reaches another datum through WGS 84 (`(-2, 52)` in ETRS89, as in PROJ), not
by OSGB36's transformations. A WKT with that extension is not identified as EPSG:4277, whose
transformations would place it otherwise (nor NAD83's as EPSG:4269, shifted in places); one on
WGS 84 is EPSG:4326. PROJJSON
writes no shift for such a string (PROJ writes the null grid). It is refused for an extension on
another ellipsoid than the WKT's `SPHEROID` (but Web Mercator's, `+a=6378137 +b=6378137` with a
null standard parallel and a scale of 1 beside WGS 84, written as Popular Visualisation Pseudo
Mercator, as PROJ writes it), and for spherical
orthographic (`+f=0`), which PROJ writes as its own "Orthographic (Spherical)".

Where WKT 1 cannot describe a CRS given by a PROJ string, terrio writes its PROJ string in the
extension with the figure the CRS computes on in place of the string's ellipsoid parameters
(`+ellps=`, a sphere's `+R=`, else `+a=` and `+rf=`), without a `+datum=` the CRS parser drops,
so that PROJ, GDAL and terrio read the WKT as the string. A `+datum=` the parser keeps for its
EPSG transformations (`+datum=NAD27` beside an otherwise undescribed parameter) cannot be
carried so: PROJ 9.9.0 ignores the WKT's datum nodes beside an extension and shifts by the
step's own definition (no shift without NAD27's grids).

The CRS still reports the ellipsoid of the first rules (`projection_params`, WKT); a WKT with
an extension, its `SPHEROID`, as PROJ reports it, whatever the extension computes on.
`tests/proj_ellipsoid_shape.rs` checks these rules against PROJ 9.9.0 on 484 strings, 27 WKT
extensions and 9 extensions terrio writes. Not covered: other parameters that PROJ's parser does
not take, which also make it compute on the string as a step reads it.

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
prime meridian). A transformation whose area of use is the whole world, as the ballpark's, holds
every point, even one beyond a pole, as in PROJ; a datum shift refuses a latitude beyond a pole
rather than folding it back, so such a point is kept as it is by the ballpark, or refused where
there is none (`+towgs84`). NAD27 to ITRF92, for instance, goes through NAD83(2011) (PROJ's
concatenation of the NADCON5 grids), ITRF2008 and ITRF2014. NAD83 and WGS 84 are treated as
equivalent, as GDAL's default operations do. Bounds are transformed with densified edges, so curved
edges in the target CRS are covered. `scripts/gen_epsg_points.py` writes GDAL's results for every
EPSG CRS, which `crates/terrio-crs/tests/epsg_accuracy.rs` checks terrio against (see the
[conformance report](conformance.md)).

A WKT that PROJ does not resolve to its EPSG CRS is transformed as PROJ transforms it, even though
terrio reads it as that code. PROJ resolves a CRS by its ID. Without one, it looks the CRS up by its
name among its database's names and aliases (an ESRI `GCS_` name first stands for the CRS it is an
ESRI alias of), and the WKT must match the CRS it finds, axis order included. So an ESRI WKT 1
geographic CRS, which has no `AXIS` and is therefore longitude first, is never resolved. Neither is
a renamed CRS, nor a projected CRS whose axis order differs (ESRI's Gauss-Krüger zones), together
with its geographic CRS. A projected CRS that PROJ does not find by name, or finds but does not
match (a custom conversion), is placed by its geographic CRS: one given as WKT 1 `GEOGCS` without
`AXIS` (longitude first) is not resolved, one given as WKT 2 `BASEGEOGCRS` (latitude first) is, by
its name. The same holds for a WKT 2 `GEODCRS` with an ellipsoidal CS, which PROJ reads as a
geographic CRS, and for the horizontal CRS of a compound CRS. PROJ takes such a CRS's area from the
CRS it finds by name, even when the WKT does not match it, and it has none for a name it does not
find. Because the direct lookup in its database finds nothing, PROJ also searches chains through an
intermediate datum. It does so when the CRSs' own transformations cover at most half the CRSs'
common area (with no area for the CRS, that is the other CRS's area), or go only through the WGS 84
ensemble, or are only time-dependent between static CRSs. It then sorts all of them together. ESRI's
`GCS_Monte_Mario_Rome` (EPSG:4806) goes through Monte Mario, 72 m from EPSG:4806's own
transformation, which covers only Sardinia. `Tokyo_1892` (EPSG:5132) goes through Tokyo, and a
renamed DHDN goes through ETRS89. This works in both directions and to every CRS. The names PROJ
finds come from `crates/terrio-crs/data/crs_names.tsv.zst` (`scripts/gen_crs_names.py`).

PROJ's datum pivot, which tries the datum's CRSs in order of code, the geocentric ones too, is
followed: ESRI's ONGD14 (EPSG:7373) without its code gets "ONGD14 to WGS 84 (1)", registered between
the geocentric CRSs, as in PROJ, while EPSG:7373 by its code keeps "(2)". PROJ pivots from an
unresolved CRS, and into one only when it cannot find it by its name (ONGD14 renamed); into one it
finds, it goes by the code's own transformations. Under the code's name, it takes the first of the
datum's CRSs of that name with transformations, alone. When the datum's name is not its ID's
(`DATUM["Local datum",...,ID["EPSG",6806]]`), PROJ goes into the datum's CRSs by its ballpark, of
unknown accuracy, and so are the chains and the transformations through geocentric CRSs it makes;
the CRS's own registered transformations keep their accuracy. A renamed Monte Mario (Rome) then
goes through Monte Mario, 72 m from EPSG:4806's own transformation, and a renamed Georgia Geodetic
Datum (EPSG:10831) through Pulkovo 1942, 7 m away. A prime meridian in grads (Carthage (Paris),
EPSG:4816) is not PROJ's registered one to the last digit, so PROJ rotates the longitudes instead,
without the ballpark, and the accuracies are kept. One step is not followed yet: from a WKT with an
ID of an authority PROJ does not know (`AUTHORITY["LOCAL","1"]`), PROJ's pivot takes only the first
of the datum's CRSs with transformations; terrio places it by the code.
terrio also does not follow PROJ for an ESRI ATF (Paris) (EPSG:4901) WKT without a code. PROJ takes
IGNF's ATIG geocentric chain, which applies the Paris prime meridian twice and lands 178 km from
PROJ's own placement of EPSG:4901. terrio, which has no IGNF transformations, identifies the WKT as
EPSG:4901 and places it as PROJ places EPSG:4901 (opengeos/terrio#1012).

`Transformer::operation_at(x, y, z)` returns the transformations `transform_3d` runs for a point,
as PROJ's `proj_trans_get_last_used_operation`: their names, summed accuracy, grids and whether one
is a ballpark, or `None` between CRSs of one datum. EPSG's null transformations (`SWEREF99 to WGS
84 (1)`, 1 m) and their concatenations are named, as by PROJ, not reported as a ballpark.
Conversions (projections, units) are not named, and geoid models are named without PROJ's EPSG index (`WGS 84 to EGM96 height`).

```rust
let t = terrio_crs::Transformer::new(&SpatialRef::parse("EPSG:27700")?, &SpatialRef::parse("EPSG:4326")?)?;
let op = t.operation_at(530000.0, 180000.0, 0.0)?;   // "OSGB36 to WGS 84 (9)", 1 m, OSTN15
```

`Transformer::has_non_ballpark_operation()` tells whether any transformation but PROJ's ballpark
links the two CRSs within their extents, as PROJ 9.9.0's candidate operations
(`proj_create_operations`) do. It is false when only the ballpark does, so that no datum
transformation is known between them in those extents: for a datum known by its ellipsoid alone,
for example. It is true between CRSs of one datum. A transformation whose grid is missing counts
when PROJ's catalog lists a direct-download URL, including non-CDN URLs, regardless of the network
setting. Locally available grids count too; other missing grids do not (SIRGAS-CON SIR17P01's
`SIRGAS2000-to-SIRGAS-CONSIR17P01.gsb`). This checks horizontal datums, including geocentric, 3D
and compound CRSs, and excludes vertical transformations and geoid models. It is currently a
standalone `terrio-crs` Rust API, intended for callers such as RustGIS.

### Datum shift grids

Many datum transformations interpolate shifts in a grid: NADCON and NADCON5 in the United
States, NTv2 grids in Canada, Australia, Germany, Great Britain (OSTN15) and elsewhere, France's
geocentric translations. terrio reads the grids PROJ uses, in pure Rust:

- PROJ's GeoTIFF grids (`.tif`), as published on the [PROJ CDN](https://cdn.proj.org) (PROJ-data),
  with their subgrids, offsets in arc-seconds or degrees, scale and offset metadata, and every
  layout PROJ reads (tiled or stripped, chunky or planar, deflate, LZW or ZSTD compressed);
- NTv2 (`.gsb`) files with their subgrids, and GTX (`.gtx`) geoid grids;
- the old names of PROJ-data grids, both ways, as PROJ looks them up in its database: an operation
  naming `us_noaa_conus.tif` finds a file named `conus`, and `+nadgrids=conus` finds
  `us_noaa_conus.tif`, locally or, with the network on, on the PROJ CDN.

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
(`terrio::crs::set_grid_search_paths`, `set_network_enabled`, and the same functions in Python), as
PROJ's `proj_context_set_search_paths` and `proj_context_set_enable_network`. Grid names inside the
search directories must use ordinary path components (`subdir/grid.gsb`): embedded `..` components
are refused with a warning even when they would normalize inside a directory (`sub/../grid.gsb`).
Only regular files of at most 2 GiB are read (a larger one is skipped with a warning).

A Rust application that downloads grids itself, with terrio's network off (built for WebAssembly, or
to download in the background), asks a transformer which grids a point needs:
`Transformer::best_operation_grids_at(x, y, z)` lists the grids of the operation PROJ's `proj_trans`
would use there with every grid present, missing ones included, each with its name on the PROJ CDN
(also for a definition that gives its name before PROJ 7; a producer's name, which PROJ does not
open the grid by under its CDN name, is kept), its URL, whether the URL gives the file and whether
the grid has an open licence (from PROJ's database, as `proj_coordoperation_get_grid_used` reports
them), and whether terrio finds it now. Download the missing ones, register them under their name
(or save them in a search directory and call `reset_grid_cache()`, as whether a grid is found is
cached), build the transformer again, and ask again: when the point is outside a grid once it is
there, the next operation's grids come up. Between compound CRSs, the grid of the transformation
between their vertical CRSs (VERTCON...) is listed too, but geoid models are not (`+geoidgrids`, nor
those of a geoid model's CRS, from a geographic 3D CRS too), and between CRSs with heights the
horizontal grids listed are those between their geographic CRSs. With `ONLY_BEST=YES`, the error for
a missing grid names it too. (Since terrio-crs 0.3.0.)

```rust
use terrio_crs::{SpatialRef, Transformer, register_grid};
let (src, dst) = (SpatialRef::parse("EPSG:4267")?, SpatialRef::parse("EPSG:4326")?);
loop {
    let t = Transformer::new(&src, &dst)?;
    let missing: Vec<_> = t.best_operation_grids_at(-100.0, 40.0, 0.0)?.into_iter().filter(|g| !g.available).collect();
    if missing.is_empty() {
        break; // t has every grid the point needs
    }
    for g in missing {
        // us_noaa_conus.tif and us_noaa_kshpgn.tif, from https://cdn.proj.org/<name>; a grid
        // PROJ's database does not know has no URL to download it from.
        let url = g.url.as_deref().ok_or_else(|| terrio_crs::Error::MissingGrid(g.name.clone()))?;
        register_grid(&g.name, download(url)?);
    }
}
```

`terrio_crs::default_grid_search_paths()` gives the directories of point 2 (empty in
WebAssembly), so an application can put its own grid directory first and keep these:
`set_grid_search_paths(Some(own.into_iter().chain(default_grid_search_paths()).collect()))`.
Path-named grids are then refused, as below.

A grid named by a path (`+nadgrids=/data/a.gsb`, `./a.gsb`) is read from that path by default, as
PROJ does, but once search paths are set from code it is treated as missing, with a warning, so a
CRS definition from untrusted input cannot read grid files from other paths. Add its directory to
the search paths, or register the grid, instead.

MCP tool processes make the host's effective search directories explicit before running
commands. A grid named by an absolute or dot-relative path in a client-supplied CRS is
therefore treated as missing. Put trusted grids in `TERRIO_GRID_DIR`, `PROJ_DATA` or
`PROJ_LIB` and refer to them by name; the normal user/cache directories remain available.
Direct CLI calls and library calls retain PROJ-compatible path-named grids by default.
Rust and Python hosts can use `confine_grid_search_paths()` to apply the MCP policy to their current
directories, or set an explicit directory list with `set_grid_search_paths`.
Python hosts can also register encoded grid bytes with `terrio.register_grid(name, data)`;
registered grids remain available with confined search paths and networking disabled.
Warning names are bounded and control characters are escaped. URL-shaped grid names are
omitted before truncation, and credential-bearing locations are redacted by the shared drain.
JavaScript already exposes in-memory registration as `registerGrid(name, bytes)`.
The browser/WebAssembly binding has no host filesystem grid search, so native search-path
configuration and confinement do not apply there.

A local entry refused for its type or size is treated as missing. With network access enabled, a
plain grid name can therefore use its CDN copy instead; path-named grids never use the CDN. The
search-directory boundary is lexical: symlinks inside trusted search directories are followed.

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
through NAVD88 height's GEOID18. A geoid model without grid that EPSG registers (Saba to Saba
height (1), whose heights are Saba's ellipsoidal heights) is applied as one. When no geoid model
applies, heights are kept, as PROJ's ballpark vertical transformation does. When the vertical
CRS has no geoid model or transformation that can be used (their grids missing), PROJ chains that
ballpark with each horizontal transformation, which makes every one of unknown accuracy, and uses
for a point the one of the smallest area that holds it. So does terrio: from WGS 84 (3D) into
`EPSG:7405` without OSGM15's grid, (-2°, 54°) goes through OSGB36 to WGS 84 (3), not (6), 5.2 m
apart (from WGS 84 (2D), which has no heights, through (6)). The height runs through the
horizontal transformation in the vertical CRS's unit, where PROJ's pipeline converts it, and
`ALLOW_BALLPARK=NO` refuses such an operation. Between 2D CRSs, heights are kept as GDAL keeps them (or
transformed, for the pipelines PROJ runs on heights).

Between a 2D CRS and a geographic 3D or geocentric CRS, the height is kept through the datum
shift too, as PROJ keeps it (`push v_3` and `pop v_3`), and converted to or from geocentric
coordinates on the geocentric CRS's own ellipsoid: WGS 84 (2D) at (0°, 0°, 0 m) is
X = 6378136 m in PZ-90 (geocentric) (`EPSG:4922`), on PZ-90's ellipsoid, and 6378137 m from
WGS 84 (3D). Transformations PROJ runs between geocentric CRSs (PZ-90 to PZ-90.11 (1)) still
move it.
An exception is a 2D CRS's own unnamed `+towgs84` operation beside a geocentric CRS: it still
runs on geocentric coordinates and can change the ellipsoidal height.
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
TERRIO_NETWORK=ON terrio crs EPSG:4267+7968 --to EPSG:4267+5703 --dim 3 -105.27 40.01 1700   # VERTCON: 1701.0026
terrio crs EPSG:4258+5783 --to EPSG:4258+5785 --dim 3 13.4 52.5 100                       # DHHN92 to SNN76: 99.856
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
  CRSs, not projected ones (which PROJ does not move). When the velocity grid is missing, or its
  download failed, the transformation fails with a missing-grid error (`Error::MissingGrid` in
  Rust); a point outside a grid that is there fails as one outside the domain;
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
checked by `crates/terrio-crs/tests/epsg_accuracy_grids.rs`, GDAL 3.13.3 and PROJ 9.9.0) are
matched within 1 cm forward and inverse; from CRS to CRS, every point of 6,434 of the 6,440 pairs is
within 1 cm and of 6,437 within 1 m: the three others go from or to RRAF 1991 (EPSG:4640 and 2989),
deprecated CRSs whose own datum PROJ chains through its RRAFG transformation, where terrio uses its
ballpark (#153).

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

EPSG definitions come from the [EPSG Geodetic Parameter Dataset](https://epsg.org), generated from
GDAL and PROJ by `scripts/gen_epsg_table.py`. The grid algorithms follow [PROJ](https://proj.org) (MIT licence);
the grids themselves come from [PROJ-data](https://cdn.proj.org), under their producers' licences.

## Geocentric Cartesian CRSs

The shared pure-Rust engine accepts geocentric Cartesian endpoints such as
`EPSG:4978` (WGS 84 X/Y/Z), `+proj=geocent`, WKT1 `GEOCCS`, and WKT2
`GEODCRS`/`GEODETICCRS` with a three-axis Cartesian CS. Authority-free WKT is
supported. The bundled table adds 296 EPSG geocentric definitions from PROJ 9.9.0 /
GDAL 3.13.3; the update script reuses each datum's existing geographic operation
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
| `ProjectedCRS` | A supported geographic base (whose `type` is optional) and WKT2 projection conversion, two Cartesian axes, including north-first and west/south orientations |
| `BoundCRS` | A geographic/projected source, a Greenwich WGS 84 geographic target, and a complete three- or seven-parameter Helmert transformation, identified by EPSG codes or standard names |
| `VerticalCRS` | A vertical reference frame and one axis, up or down, in a length unit; identified by its name, datum, unit and direction |
| `CompoundCRS` | A geographic or projected CRS and a vertical CRS, as PROJ writes EPSG:7405; identified by its parts |

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
stores WKT2 and identifies an EPSG/ESRI code from that definition as the WKT reader
does; it does not assign a root code merely from an ID. The
exported WKT omits root CRS identifiers so that reopening it also preserves
caller-supplied conversion parameters; component identifiers remain. Descriptive
usage fields (`scope`, `area`, `bbox`, `usages`, `remarks`) are accepted without
affecting coordinates.

Identification also follows the WKT reader's transformation policy: an identified
code uses the registry's datum operations and available grids. Retained WKT is
not a separate WKT-only transformation mode. Definitions that do not meet the
WKT reader's registry-identification criteria remain custom; a supplied bound Helmert
operation remains explicit. The no-grid conformance comparisons do not establish
equivalence for every possible grid installation.

This is a reader for the documented subset, rather than a general JSON Schema
validator; [Writing PROJJSON](#writing-projjson) describes the writer. Unsupported semantic members and object types
are errors: dynamic/derived/compound/vertical CRSs, Cartesian geocentric or 3D
PROJJSON, polar axis meridians, grid/deformation operations and nested bound
sources are not supported.
Geocentric and 3D CRSs remain available through EPSG, PROJ and WKT definitions.
This prevents silent loss of datum, axis, unit or operation information.

### Writing PROJ strings

`SpatialRef::to_proj4` writes the PROJ string PROJ 9.9.0 writes (`projinfo -o PROJ`, pyproj's
`CRS.to_proj4()`): `terrio crs CRS --proj4`, Python `terrio._terrio.crs_to_proj4(crs)`,
JavaScript `crsToProj4(crs)`. (Python's `rio.CRS.to_proj4()` keeps writing rasterio's parameter
string.) It is `None` for a CRS PROJ has none for (an engineering CRS, Polar
Stereographic variant C, an EPSG code read only through the wbprojection fallback). Like PROJ's,
the string leaves out what a PROJ string cannot hold, so keep the code or WKT to keep a CRS:

- the datum transformation of an EPSG or ESRI CRS: `+datum=` names only WGS 84, NAD83 and NAD27,
  and `+towgs84` comes only from a definition that gives it (a WKT's `TOWGS84` or `BOUNDCRS`,
  written with the ellipsoid in place of the datum, as PROJ does, also for a WKT of a code);
- the angular unit of a geographic CRS (EPSG:4807, in grads, is written in degrees);
- the axis order, but for south-orientated Transverse Mercator and Krovak;
- a PROJ string's vertical unit (`+vunits`, `+vto_meter`) without a geoid model
  (`+geoidgrids`): PROJ writes it only with one.

`SpatialRef::to_proj4_gdal` writes the PROJ string GDAL 3.13.3 writes (`OSRExportToProj4`;
`terrio crs CRS --proj4-gdal`, Python `terrio._terrio.crs_to_proj4_gdal(crs)`, JavaScript
`crsToProj4Gdal(crs)`): the same, without `+type=crs`, and for an EPSG or ESRI code's own
definition, with what GDAL adds to it
(`scripts/gen_gdal_towgs84.py`, 5,130 CRSs): the datum transformation to WGS 84 as `+towgs84`, where
PROJ finds a single one for the CRS's area of use (EPSG:31467 has
`+towgs84=598.1,73.7,418.2,0.202,0.045,-2.455,6.7`; EPSG:27700, with several, has none), and a
compound CRS's geoid model (`+geoidgrids`, `+geoid_crs=WGS84`). Another CRS keeps its own
`+towgs84`, if any. A WKT 1 or WKT 2 whose root `AUTHORITY` or `ID` names the code has the same
additions, as GDAL binds a CRS by its root identifier, unless it has a datum shift of its own
(`TOWGS84`, `+nadgrids`), which it keeps. A PROJ string or a WKT without a root identifier of a
single CRS (an ESRI `.prj`'s) has none, as in GDAL. Unlike GDAL, a PROJJSON has none either, nor a
WKT whose parameters are not those of the code it names; a compound CRS given by a WKT or PROJ
string is not always written as GDAL writes it (GDAL's WKT 1 of one carries its geoid model in a
`PROJ4_GRIDS` extension, its WKT 2 does not; `tests/gdal_proj4_export.rs` lists the cases), and
`+init=epsg:2056`, the code itself to terrio, has its additions, which GDAL does not write. A
deprecated code has its own definition's, as GDAL with `OSR_USE_NON_DEPRECATED=NO`.

`terrio_crs::helmert_to_wgs84(code)` gives the datum shift to WGS 84 of an EPSG geographic CRS (of
a projected or compound CRS's base geographic CRS) as `+towgs84` carries it, also where GDAL writes
none: the first transformation PROJ 9.9.0 picks without grids, when PROJ runs it as a single Helmert
transformation of 3 or 7 parameters (ED50 has `-87,-98,-121,0,0,0,0`, from ED50 to WGS 84 (1)).
`None` otherwise: a Molodensky-Badekas, time-dependent or exact-rotation Helmert transformation, a
geographic offset, several shifts, PROJ's ballpark. A transformation that shifts nothing (NAD83 to
WGS 84 (1)) gives `Some` zeros, as `+towgs84=0,0,0`, unlike an unsupported one. MGI (Ferro),
EPSG:4805, gives `None` where PROJ has a Helmert transformation: PROJ's MGI (Ferro) to WGS 84 (1)
turns longitudes by MGI 1901's Ferro meridian, 13.98" from the CRS's own, which a `+towgs84` beside
`+pm=ferro` would leave out. It is in the Rust crate only.

A compound CRS (`EPSG:4326+5773`, a registry code, a WKT's `COMPD_CS`) is its horizontal CRS's
string with the vertical unit (`+vunits=m`), and a WKT's geoid model (`+geoidgrids`). A CRS
given by a PROJ string or a WKT without a code comes back as PROJ writes it:

- `+datum=` for WGS84, NAD83 and NAD27 when no shift, ellipsoid parameter or prime meridian
  changes them (`+ellps` beside `+datum` is ignored), else (and for PROJ's other datums) the
  datum's ellipsoid;
- the ellipsoid by name when PROJ has the name, else a sphere as `+R`, else as given (`+rf` or
  `+b`); on a named ellipsoid or datum, a flattening given as `+f`, `+es` or `+e` is dropped,
  as PROJ drops it;
- `+towgs84` with seven values, or `+nadgrids` in its place; prime meridians and units by
  PROJ's names; the unit of a projected CRS written;
- UTM zones as `+proj=utm`; GDAL's Web Mercator string as PROJ's.

Its parameters stay in their order, which PROJ rewrites in its own, adding the defaults (and
dropping some, such as polar stereographic's `+k=1`). A WKT's PROJ4 extension (GDAL's
`EXTENSION["PROJ4", ...]`) is written as it is, as PROJ writes it. A planetocentric CRS on an
ellipsoid (`+geoc`) has none, as in PROJ.

For every EPSG and ESRI code terrio reads (8,329 in the default build) and every IAU_2015 CRS
(2,201), the string is PROJ 9.9.0's character for character. `tests/proj_string_export.rs`
checks every 20th of these codes, the codes behind each rule, custom definitions and compound
references, frozen by `scripts/gen_proj_string_export.py`; the whole tables are compared, as a
manual check that CI does not run, with
`scripts/gen_proj_string_export.py --all PATH` (pyproj 3.8.0), then
`TERRIO_PROJ_STRINGS_ALL=PATH cargo test -p terrio-crs --test proj_string_export -- --ignored`.
Reading a string back gives the CRS's conversion, with the ellipsoid rules of
[PROJ string interpretation changes](#proj-string-interpretation-changes) (ESRI:102035's
`+proj=ortho +f=0 +datum=WGS84` is spherical orthographic).

Known differences, all of WKTs and PROJ strings without a code:

- an ESRI WKT of Miller's projection, which PROJ reads on the authalic sphere (`+R_A`), is read
  and written without it;
- a WKT's grid shift (GDAL's `PROJ4_GRIDS` in a datum, a `BOUNDCRS` with a grid) is not read,
  so its string has no `+nadgrids`;
- a WKT 2 ellipsoid in other units than metres is written `+a +b` where PROJ writes `+a +rf`
  (the same ellipsoid);
- an ESRI `.prj` that terrio identifies as a code gets the code's order of standard parallels,
  where PROJ keeps the WKT's;
- a WKT 2 `BOUNDCRS` whose WKT 1 source CRS has a `TOWGS84` of its own has no string, where
  PROJ writes that `TOWGS84` but transforms by the bound transformation (which terrio does too).
  As in PROJ, a `BOUNDCRS` whose position vector or coordinate frame transformation lacks any of
  its seven parameters, or whose transformation is a Molodensky-Badekas one or a time-dependent
  one given by its code or with its rates, has no string either;
- a WKT 2 `BOUNDCRS` with a geocentric translations transformation given a rotation or scale, or
  with coordinate frame's full matrix, has no string, where PROJ writes a `+towgs84` it does not
  transform by (without the rotation; by the small-angle approximation, 9 m away for Saba to
  WGS 84's rotations of hundreds of arc-seconds).

### Writing strict WKT 1

`SpatialRef::to_wkt` writes GDAL's WKT 1, which keeps what WKT 1 cannot describe in an
`EXTENSION["PROJ4", ...]`. Formats whose readers ignore that extension (a shapefile `.prj`)
need `SpatialRef::to_wkt1_strict`, as PROJ 9.9.0 exports `WKT1_GDAL`: `terrio crs CRS
--wkt1-strict`, Python `rio.CRS.to_wkt1_strict()` and `terrio._terrio.crs_to_wkt1_strict(crs)`,
JavaScript `crsToWkt1Strict(crs)`. It is the WKT 1 of `to_wkt`, or `None`:

- where PROJ writes no WKT 1: geographic 3D CRSs (EPSG:4979), Equal Earth (EPSG:8857), the
  registry CRSs whose method has no WKT 1 name (Colombia Urban, Modified Krovak, Lambert Conic
  Conformal 1SP variant B and 2SP Michigan, the Guam projection...), rotated poles
  (`+proj=ob_tran`); and for an EPSG code terrio reads only through the wbprojection fallback,
  whose WKT is not the registry's;
- where the WKT 1 needs its extension: a datum grid (`+nadgrids`, a geoid model, a WKT 2
  bound CRS's `PARAMETERFILE`), an axis order or parameter its nodes do not describe
  (`+axis=neu` on a projected CRS, `+approx`, geostationary's `+sweep=x`), GDAL's
  `custom_proj4` method for a projection WKT 1 has no name for.

Web Mercator (EPSG:3857) keeps GDAL's extension, as PROJ does: its `AUTHORITY` identifies it.
A Web Mercator WKT with a `TOWGS84` of its own (ESRI's WGS_1984_Web_Mercator_Auxiliary_Sphere
with one, zero or not) is placed as EPSG:3857, whose `+nadgrids=@null` wins over the shift, as
PROJ 9.9.0 places it; the shift is not applied on the projection's sphere.
A WKT 1 given is returned as it is when it needs no extension (or its root code is the CRS's
own), else `None`. A WKT 2 that terrio identifies as a code, without a datum
shift of its own, is written as the code's WKT 1; with the code's two horizontal axes swapped
(ETRS89 in longitude and latitude, or the WKT 2 text of `OGC:CRS84`, identified as EPSG:4326),
as the code's WKT 1 with its axes swapped and without the codes that give their order. Another
WKT 2 is written as the WKT 1 of its PROJ string, when
that reads back as the same conversion and the WKT 2's axes are east-north (or north-east, for a
geographic CRS); on a datum terrio identifies (a custom projection on OSGB36), on that EPSG
geographic CRS, which keeps the datum's name, code and transformations. A planetocentric CRS
on an ellipsoid has none: PROJ writes a `GEOGCS` that its readers take for planetographic
latitudes.

For every EPSG and ESRI code terrio reads, the WKT 1 is PROJ's character for character, with
GDAL's axes (pyproj 3.8.0's `CRS.to_wkt("WKT1_GDAL", output_axis_rule=True)`), and `None`
where PROJ has none, but for EPSG:2985 and 2986, whose table WKT is GDAL's, without PROJ
9.9.0's axes. Every IAU_2015 CRS and custom definition is `None` where PROJ has none or needs
an extension, else reads back as the CRS. `tests/wkt1_strict_export.rs` checks every 20th code,
every code PROJ refuses and custom definitions, frozen by `scripts/gen_wkt1_strict_export.py`;
the whole tables are compared, as a manual check that CI does not run, with
`scripts/gen_wkt1_strict_export.py --all PATH`, then
`TERRIO_WKT1_ALL=PATH cargo test -p terrio-crs --test wkt1_strict_export -- --ignored`.

Known differences, all of definitions without a code, where terrio is stricter than PROJ (it
returns `None` where PROJ writes WKT 1):

- a projected CRS of other axes than east-north, unless it is a code's with its axes swapped;
- a cylindrical equal-area projection given by its scale;
- a named datum changed by a prime meridian or a shift (`+datum=WGS84 +pm=paris`);
- spherical orthographic (`+f=0`) on a named ellipsoid, which PROJ's WKT writes as orthographic
  on that ellipsoid;
- an ellipsoid given by `+es` or `+e` with a shift (`+a +es +towgs84`);
- planetocentric latitudes on a sphere;
- a WKT 2 compound CRS, a WKT 2 with a west axis (planetographic IAU_2015 CRSs), a WKT 2 in
  another angular unit than the degree with its axes swapped, a geocentric WKT 2 on another
  prime meridian given in other digits than PROJ's (`PRIMEM["Paris",2.33722917]`);
- a compound CRS on Web Mercator (`EPSG:3857+3855`), or a WKT 2 of Web Mercator with its axes
  swapped, whose extension is not under the CRS's code;
- a few other combinations PROJ writes: `+R_A` on a sphere, a WKT 2 in grads
  without a code, a WKT 2 of south and west axes;
- a PROJ string's `+datum=` changed by its own `+towgs84` or `+nadgrids`
  (`+datum=WGS84 +towgs84=1,2,3`), which PROJ writes as the datum with that shift.

UPS (`+proj=ups`), which PROJ keeps in its extension, is written as Polar Stereographic. The
code `OGC:CRS84`, which terrio reads as EPSG:4326, gets EPSG:4326's WKT 1 (latitude first),
where PROJ writes CRS84's; its WKT 2 text gets its own axes.

`to_wkt` itself writes the defining inverse flattening of a named ellipsoid (298.257223563 for
WGS 84, as PROJ does) and describes a geocentric CRS, a sphere's `+R_C` and an ellipsoid given by
`+a` with `+es` or `+e` without the extension. A prime meridian in degrees, minutes and seconds
(`+pm=2d20'14.025"E`) is read and written for every CRS, as PROJ reads it; it was read for
geocentric CRSs only (and Greenwich taken for others).

`--only wkt1-strict-export` (`scripts/compare_wkt1_strict_with_proj.py`) has live PROJ read the
strict WKT 1 of every custom definition of the frozen data: 203 read as the original, 3 within
tolerance, 45 none where PROJ has none, the rest not compared (the known differences above).

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
values, a Krovak or Krovak Modified projection with another `+alpha` than its default (PROJ
ignores its `+lat_1` and `+lat_ts`, so they are not written), and any parameter of the PROJ definition the PROJJSON would
not carry (named in the error), unless it has the value used when it is absent (`+lat_0=0`,
`+k=1`...). A CRS made from an EPSG code is written unbound (its datum shift is EPSG's); a
`+towgs84` or `TOWGS84` the definition gives makes a `BoundCRS`, to WGS 84's geographic 2D or 3D
CRS, or to its geocentric CRS for a geocentric CRS, as PROJ writes them.


### GeoTIFF GeoKeys

The GeoKeys of a GeoTIFF (and of a LAS `LASF_Projection` record) are read and written by
terrio-crs, so that other programs can use them without the `terrio` crate:
`SpatialRef::from_geokeys(directory, doubles, ascii)` reads the CRS of the GeoKeyDirectoryTag
(34735), GeoDoubleParamsTag (34736) and GeoAsciiParamsTag (34737), as GDAL reads them
(`GTIFGetOGISDefnAsOSR`); `geokeys::read` gives the same CRS and the projection method left
unread, for a warning. `SpatialRef::to_geokeys()` gives the CRS's GeoKeys, as GDAL 3.12.1 writes
them (`GTIFSetFromOGISDefnEx`), without the raster type (GTRasterTypeGeoKey), and
`GeoKeyValues::encode()` the three tags. `terrio`'s GeoTIFF and LAS readers and writers use them.
They are Rust API only: the Python and JavaScript CRS objects read and write GeoTIFFs through the
drivers, and the GeoKey arrays serve programs with their own TIFF code.

They read and write:

- registered geographic and projected CRSs by their EPSG codes, and a compound CRS of EPSG CRSs by
  its horizontal and vertical codes (GeoTIFF 1.1), read as the registry's compound CRS of the two
  when there is one; a geographic 3D CRS by its geographic 2D CRS's code with its own as the
  vertical CRS, as GDAL writes and reads it (EPSG:4979 as 4326 and 4979). Unlike GDAL, terrio
  also reads VerticalCSTypeGeoKey in a GeoTIFF 1.0 directory, where LAS records hold it;
- user-defined geographic and projected CRSs: the projection methods GeoTIFF names
  (ProjCoordTransGeoKey 1, 3, 7 to 28, and 9815 for Hotine oblique Mercator variant B and Swiss
  oblique Mercator), a UTM zone by its ProjectionGeoKey, linear units by EPSG code or by size
  (ProjLinearUnitSizeGeoKey), angular units, and EPSG datums, ellipsoids, prime meridians and
  `TOWGS84` shifts. These parameters are read as libgeotiff's `GTIFFetchProjParms` reads them, as
  GDAL 3.12.2 does: the false easting and northing from ProjFalseEasting/NorthingGeoKey, else
  ProjCenterEasting/NorthingGeoKey, else the false origin's; a Hotine oblique Mercator without
  ProjRectifiedGridAngleGeoKey at 90 in the keys' angular unit (81° in grads); and a polar
  stereographic CRS by its origin latitude when given (ProjNatOriginLatGeoKey, else
  ProjFalseOriginLatGeoKey, else ProjCenterLatGeoKey: 90° and a ProjStdParallel1GeoKey of 71° or
  -71° is variant A of its scale, the standard parallel unread); Mercator 2SP by its
  ProjStdParallel1GeoKey whatever its scale, else 1SP of its latitude of origin (as `+lat_ts`) and
  ProjScaleAtNatOriginGeoKey alone; Stereographic of ProjScaleAtNatOriginGeoKey alone; Laborde
  (CT 4) as `+proj=labrd`. An ellipsoid, datum or prime meridian given by its code without its
  values is the registry's (Airy 1830 for 7001 or for datum 6277, which also gets OSGB36's
  transformations; the spheres 7035, 7047 and 7048; Paris for 8903), as libgeotiff looks them up.
  A projected code alone (no projection, method, geographic CRS, datum or ellipsoid key) is the
  registry's CRS, a deprecated code's single replacement, in the keys' linear unit
  (ProjLinearUnitsGeoKey, else the stated code's own), without its code when that is not the CRS's
  unit: EPSG:32633 with ProjLinearUnitsGeoKey 9002 is UTM zone 33N in feet; beside other keys, the
  stated code, deprecated or not (EPSG:4855 with GeographicTypeGeoKey 4258 is 4855, not 5105), as
  GDAL 3.12 reads them (#1012 items 32 to 35). Not yet as libgeotiff reads them: without any origin
  latitude, a polar stereographic CRS's ProjStdParallel1GeoKey is still read as its standard
  parallel, leniently, where libgeotiff takes a latitude of 0 (GDAL places such points tens of
  thousands of kilometres away). A
  user-defined projected CRS can be read by a registered code (see below). A horizontal CRS with a
  datum shift of its own (a WKT's `TOWGS84` or `BOUNDCRS` to WGS 84, a PROJ string's `+towgs84`)
  is written with GeogTOWGS84GeoKey: with the code its WKT declares, else as a user-defined CRS on
  a user-defined datum, never as a code it is only identified as (ESRI's British_National_Grid
  with `TOWGS84[375,-111,431,0,0,0,0]`, EPSG:27700 alone, would read 3.1 m away). GDAL 3.10.3
  writes the same keys but names the EPSG datum (GeogGeodeticDatumGeoKey 6277), and reads that
  file back without the shift; it keeps the shift of terrio's keys. A CRS identified by a code
  with a zero shift on WGS 84 is written by the code, as GDAL does. A code stated by the keys with
  GeogTOWGS84GeoKey next to it is read as the code's WKT 1 with that `TOWGS84` (but not a zero
  shift on the WGS 84 datum, which is no shift; on another datum of WGS 84's ellipsoid it is kept), so that such a file, terrio's or GDAL's, reads back with its shift (the CRS keeps its
  code; GDAL 3.13.3 reads it without the shift). A WKT 2 `BOUNDCRS` whose transformation
  `+towgs84` only approximates (a Molodensky-Badekas evaluation point, a full rotation matrix) is
  written as its WKT in an ESRI PE citation, which keeps it. Not yet: a compound CRS with a shift is
  written by its codes, without it (#1012 item 62);
- ESRI PE citations (`ESRI PE String = <WKT>`), which GDAL writes for a CRS GeoKeys cannot hold
  (an engineering CRS, a method GeoTIFF has no code for, a definition only its PROJ string
  describes): terrio writes them for the same CRSs. A CRS whose WKT holds a NUL (from a PROJJSON
  name) can't be written so, as GeoAsciiParamsTag ends at its first NUL: writing its GeoKeys, and
  so a GeoTIFF or COG in it, fails, where terrio used to write a file whose CRS GDAL couldn't
  read. GDAL 3.12.1 refuses to write such a CRS too. Citation names are written up to a NUL.

A user-defined projected CRS gets a registered code more often than in GDAL, which names only an
equivalent registered CRS of the citation's exact name (or an ESRI CRS, as terrio does too). terrio
also names a UTM zone by its ProjectionGeoKey or a `UTM zone 33N` citation, on WGS 84, NAD83 or
NAD27, in the GeoKeys' unit, and, with the `wbprojection-fallback` feature (which `terrio` enables),
the projected CRS a citation names (an `EPSG:` reference or a CRS name), on the GeoKeys' datum when
they give its code. The code's definition then replaces the keys': where the keys give a datum of
their own or none, a citation naming a CRS on another ellipsoid (`EPSG:32633` on keys of Clarke
1866, `NAD27 / UTM zone 15N` on keys without a datum, which GDAL reads on WGS 84) gives other
coordinates than GDAL. The GeoKeys of a CRS terrio reads only through wbprojection need that feature
too.

`tests/geokeys_gdal.rs` compares both directions with GDAL 3.12.1 (rasterio 1.5.0), frozen by
`scripts/gen_geokeys_gdal.py`: the GeoKeys GDAL writes for 376 CRSs (every 20th EPSG projected and
geographic 2D code, the codes of each rule, PROJ strings of every method GeoTIFF names in other
units and on other datums, ESRI codes, compound and geographic 3D CRSs) are read as the CRS GDAL
reads from them (by its EPSG code when GDAL's has one, else at two points), all 376. terrio's
GeoKeys of the 374 of them it reads from its tables hold GDAL's model type, EPSG codes, projection
method, linear unit and projection parameters, and read back as the CRS. Known differences:

- GDAL also writes keys the codes imply (citations, the angular unit, the ellipsoid's axes), and
  a UTM zone by its ProjectionGeoKey where terrio writes its method and parameters; both read the
  same;
- World Robinson (ESRI:54030) is written on a user-defined geographic CRS of WGS 84's ellipsoid,
  where GDAL names EPSG:4326 (the ESRI WKT's `GCS_WGS_1984` has no EPSG code terrio identifies);
- Miller on the authalic sphere (`+R_A`) is written as an ESRI PE citation, where GDAL writes the
  method.

### Axis order, conversion parameters and body

Four accessors describe a CRS as PROJ 9.9.0 reads it, for programs that show CRS details or
choose map behaviour from them: `terrio_crs::SpatialRef` and `terrio::crs::SpatialRef` in Rust,
and the `Axis order`, `Body` and `Method` lines of `terrio crs CRS` (and its MCP tool).

- `axis_order()` gives `AxisOrder::EastNorth`, `NorthEast`, `Other` or `Unknown` (a CRS terrio
  can't read, or an EPSG code it reads only through wbprojection, whose WKT has no axes), the
  order of the first two axes as PROJ reads the CRS:
  - an EPSG or ESRI code is in the registry's order (EPSG:4326 north then east);
  - a PROJ string is in its `+axis` order, east then north by default
    (`+proj=longlat +datum=WGS84` is east then north, though its WKT names EPSG:4326's axes);
  - otherwise the WKT's `AXIS` nodes give the order. Without them, a WKT 1 geographic CRS with an
    EPSG code is in that code's order, and any other CRS east then north.

  Geocentric and vertical CRSs, Polar Stereographic (whose axes point along meridians, unless a
  WKT states others) and axes other than north and east (`northEast`, `south`) are `Other`.
  `has_north_east_axis_order()` goes by the CRS's WKT instead, for GML's authority order (a
  bound CRS's by its source CRS, as `axis_order()`).
- `projection_params()` gives the method (the `+proj` name PROJ writes: `etmerc` is `tmerc`,
  `webmerc` `merc`, `latlong` `longlat`), `lon_0`, `lat_0`, `k_0`, `x_0`, `y_0`, the prime
  meridian, the ellipsoid's `a` and `b` and metres per unit. They are those of
  the CRS's PROJ string (the registry's, the one given, or the one converted from its WKT), with
  PROJ's defaults. A UTM or UPS zone has its own. The values are those of the definition:
  Mercator's `+lat_ts` is not a scale factor here, nor Oblique Mercator's `+lonc` a central
  meridian. The ellipsoid is the one PROJ reports:
  - for a code, the registry's (Web Mercator is on WGS 84, though it computes on a sphere);
  - for a PROJ string, the CRS's ellipsoid as PROJ reads it (its named ellipsoid under `+f`,
    `+es` and `+e`), before a spherification flag (`+R_A`, `+R_V`, `+R_C`...);
  - for a WKT with a PROJ4 extension, its `SPHEROID`, whatever the extension computes on.

  A WKT that declares an EPSG code terrio has gives that code's values, as terrio transforms it
  (PROJ reads the WKT's own nodes, which may differ). `None` for a CRS terrio can't read, or
  reads only through wbprojection's WKT parser. Where
  PROJ writes no PROJ string (Polar Stereographic variant C, planetocentric IAU_2015 projected
  CRSs on an ellipsoid), terrio gives those of the string it reads the CRS by.
- `body()` names the celestial body as PROJ does: `Earth` for an EPSG or ESRI code, the body of
  an IAU_2015 CRS (`Mars`), and for another ellipsoid, `Earth` within 0.7% of the Earth's mean
  radius, else the body of PROJ's table whose semi-major axis is within 0.7% of the ellipsoid's
  (`+R=3396190` is on Mars). It gives `None` for an ellipsoid that matches no body, or bodies of
  several names (PROJ's `Non-Earth body`). `is_earth()` is `body() == Some("Earth")`.

`tests/crs_accessors.rs` compares them with PROJ 9.9.0 (pyproj 3.8.0) on 777 CRSs, frozen by
`scripts/gen_crs_accessors.py`:
- every 20th EPSG and ESRI code and the codes behind each rule;
- 46 PROJ strings and 49 WKTs (WKT 1, WKT 2 and ESRI's);
- compound references, every 20th IAU_2015 CRS and a CRS of every IAU_2015 body.

The body table (`src/bodies.rs`) is generated from PROJ's database by
`scripts/gen_celestial_bodies.py`.

## IAU planetary coordinate systems

The shared pure-Rust engine includes the **IAU_2015** registry from PROJ 9.9.0,
with source Git blob `900564ccf2e1f77fb708ddc2c8544843b29bc24a`. Its 2,201 definitions
comprise 115 geographic CRSs (including spheres), 12 ellipsoidal planetocentric
geographic CRSs and 2,074 projected CRSs. The source is PROJ's
[`data/sql/iau.sql`](https://github.com/OSGeo/PROJ/blob/9.9.0/data/sql/iau.sql), based
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
on an ellipsoid uses the conformal-sphere radius (`+R_C`). PROJ 9.9.0
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
WGS 84, its semi-major axis supplies the sphere radius, matching PROJ 9.9.0;
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
families from #272 are documented below.

## Interrupted Goode homolosine

`+proj=igh` is computed as PROJ 9.9.0 does (`igh.cpp`): twelve zones, spherical
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

The `seed-igh` fixture keeps its PROJ 9.9.0 oracle. A PROJ 9.9.0 corpus covers
8 configurations and 5,120 samples (see Testing). Allowances are 1 mm projected
and 1e-8 degrees inverse.

## General oblique transformation

`+proj=ob_tran` with an `+o_proj` other than `longlat` is computed as PROJ 9.9.0 does
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

The `seed-ob_tran-moll` fixture keeps its PROJ 9.9.0 oracle. A PROJ 9.9.0 corpus covers
15 configurations and 4,680 samples (see Testing). Allowances are 1 mm projected and 1e-8
degrees inverse.

## Quadrilateralized spherical cube

`+proj=qsc` is computed as PROJ 9.9.0 does (`qsc.cpp`): one face of a cube, chosen by the
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

The `method-qsc` fixture keeps its PROJ 9.9.0 oracle. A PROJ 9.9.0 corpus covers 16
configurations and 4,288 samples (see Testing). Allowances are 1 mm projected and 1e-8 degrees
inverse. At the point opposite the face centre the inverse is allowed 1e-5 degrees, and must
project back within 1 mm.

## HEALPix and rHEALPix

`+proj=healpix` and `+proj=rhealpix` are computed as PROJ 9.9.0 does (`healpix.cpp`): an
equal-area cylindrical projection between the latitudes of asin(2/3) and Collignon projections
in the four polar caps on each side, which `healpix` leaves as triangles and `rhealpix` combines
into one square at each pole. `+rot_xy` rotates the `healpix` image, in degrees; `+north_square`
and `+south_square` (0 to 3, the default 0) pick the equatorial square below which `rhealpix`
places each polar square, and other values are refused, as in PROJ. On an ellipsoid the
latitude is the authalic latitude and the image is scaled by the authalic radius, as in PROJ
9.9.0 (PROJ 9.4 differs by 0.18 m on WGS 84 at the south pole). `+lon_0`, `+units`,
`+x_0` and `+y_0` apply. The inverse refuses points outside the image, as PROJ's does. As in
PROJ, whose point-in-polygon test leaves out the polygon's closing edge, the `rhealpix` inverse
accepts points left of the image between the polar squares, such as (-4, 0) on the unit sphere.

GDAL's WKT1 (`custom_proj4`, which carries the PROJ string), ESRI's `PROJ_healpix` and
`PROJ_rhealpix`, and PROJ's WKT2 methods `PROJ healpix` and `PROJ rhealpix` are read; terrio
writes the WKT1 form, as GDAL does. PROJJSON with these methods is not read, and PROJJSON
export refuses them, as for other projections without an EPSG method.

On the edge of the image (the poles, the antimeridian of the central meridian and the edges of
the polar caps) the last bit of a projected coordinate moves a point in or out of the image.
There a US survey foot in the PROJ string's `+units` can decide it: PROJ 9.9.0 takes that unit
in these methods' own `+units` as 0.304800609601219 m, and terrio, as PROJ's `unitconvert` for
other methods, as 1200/3937 m.

The `method-healpix` and `method-rhealpix` fixtures keep their PROJ 9.9.0 oracles. A PROJ
9.9.0 corpus covers 22 configurations and 6,600 samples (see Testing). Allowances are 1 mm
projected and 1e-8 degrees inverse.

## Icosahedral Snyder Equal Area

`+proj=isea` is computed as PROJ 9.9.0 does (its polyhedral projections,
`src/projections/polyhedral.cpp`): Snyder's equal-area projection of the authalic sphere onto the
faces of an icosahedron, each face split into six triangles from its centre, and unfolded into
PROJ's strip of 4 × 5 faces. `+orient=isea` (the default) is Snyder's orientation, and
`+orient=pole` places a vertex at each pole. `+orient_lat`, `+orient_lon` (the point brought to the
pole, its latitude geodetic) and `+azi` set the orientation otherwise. `+lat_0` and `+lon_0` name
the point projected to the origin, through the orientation; they do not move the central meridian.
With `+dual`, the dodecahedron's net (`+net=dsea`, the default, `a5`, `crescent` or `flower`)
carries the icosahedron's triangles, as PROJ's `isea +dual`. On an ellipsoid the latitude is
authalic and the scale the authalic radius, so that the projection is equal-area, and the inverse
returns the longitude and geodetic latitude that were projected. `+units`, `+x_0` and `+y_0` apply.
`+mode`, `+resolution` and `+aperture` are ignored, as in PROJ 9.9.0.

As PROJ 9.9.0 reads them, a parameter given as a number must be read whole as an angle (`+azi=30.5`
and `+lat_0=30d30'` are read, `+azi=30x` is refused), one given as a word is read as 0 (`+azi=abc`),
and `+dual` is true bare or with a value starting with `t` or `T`, false with one starting with `f`
or `F`, and refused otherwise. In a WKT's `PROJ4` extension, which PROJ reads as a pipeline step, an
angle is its readable start (`+lat_0=30x` is 30°). The inverse refuses points outside the unfolded
net.

GDAL's WKT1 (`custom_proj4`, which carries the PROJ string), ESRI's `PROJ_isea` (with
`PROJ_isea_orient=pole`, `PROJ_isea_dual` and other options in the method's name) and PROJ's
WKT2 method `PROJ isea` (`PROJ isea orient=pole`, `PROJ isea dual net=a5`) are read; terrio
writes the WKT1 form, as GDAL does. PROJJSON export refuses `isea`, as for other projections
without an EPSG method.

PROJ 9.8 and earlier computed another method (`isea.cpp`: another orientation by `+lon_0`, the
geodetic latitude on an ellipsoid, an inverse only for its default grid), which terrio computed
before. On the default sphere the two agree within a millimetre. A PROJ 9.9.0 corpus covers 24
configurations and 13,344 samples (see Testing). Allowances are 1 mm projected and 1e-8 degrees
inverse (1e-5 within 0.001° of a pole, where the latitude is the arcsine of a number near 1).

## Two-point oblique Mercator

`+proj=omerc` without `+alpha` or `+gamma` takes its centre line through two
points, `+lat_1`/`+lon_1` and `+lat_2`/`+lon_2`, as PROJ does: the azimuth and
the central meridian are derived from them, and `+lon_0` is ignored. The
formulas are PROJ 9.9.0's (`omerc.cpp`), on a sphere or an ellipsoid, with
`+lat_0`, `+k`, `+units`, `+x_0` and `+y_0`. PROJ's checks apply: neither point
at a pole, different latitudes, a first point off the equator and `+lat_0` off a
pole. A point left out is 0, as PROJ reads it; a value that isn't a number is
refused by Terrio.

GDAL's WKT1 `Hotine_Oblique_Mercator_Two_Point_Natural_Origin`, with GDAL's
(`latitude_of_point_1`) or ESRI's (`Latitude_Of_1st_Point`) parameter names, and
WKT2 `Hotine Oblique Mercator Two Point Natural Origin` are read, and terrio
writes the former, as GDAL does. The `method-omerc-2pt` fixture keeps its PROJ
9.9.0 oracle; a GDAL 3.12.2 / PROJ 9.8.1 corpus covers 12 lines and 1,524
samples (see Testing). Allowances are 1 mm projected and 1e-8 degrees inverse.
The line poles are refused; their finite neighbours and geographic poles are checked
against live GDAL. PROJJSON export of this non-EPSG method remains unsupported.

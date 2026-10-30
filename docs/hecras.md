# HEC-RAS

terrio reads the HDF5 files of [HEC-RAS](https://www.hec.usace.army.mil/software/hec-ras/), the
US Army Corps of Engineers' river and floodplain hydraulics model, as vector layers:

| Name | Format | Read | Write | Extensions |
|---|---|---|---|---|
| HECRAS | HEC-RAS geometry (`.g##.hdf`) and plan results (`.p##.hdf`) HDF5 files | ✓ | | by content (`.hdf`, `.h5`) |

A file gives its 2D mesh (cells, cell centres, faces, face points, flow area perimeters), its 1D
geometry (cross sections, rivers, storage areas, structures), its 2D features (boundary-condition
lines, breaklines, refinement regions, reference lines and points), its pump stations and pipe
networks, each with every attribute column HEC-RAS stored, and the results HEC-RAS stored, joined
to those features on request. The driver only reads: it does not run HEC-RAS, write HEC-RAS
files, or read the project and plan text files (`.prj`, `.p##`, `.g##`).

```sh
terrio info BaldEagleDamBrk.p18.all2d.hdf
terrio convert BaldEagleDamBrk.p18.all2d.hdf cells.fgb -l mesh_cells --oo "CELL_SUMMARIES=Maximum Water Surface"
```

## Supported versions

The driver is **tested on files written by HEC-RAS 7.0.1**: the comparison with h5py and the
real-data tests use 7.0.1 files only. Files from older versions (5.x, 6.x) are read where their
layout is the same, best effort: a layout the engine does not support is refused with an error,
never read as empty or approximate data.

## Detection

A file is a HEC-RAS file when it has an HDF5 signature and its root `File Type` attribute is
`HEC-RAS Geometry` or `HEC-RAS Results`. The extension decides nothing: HEC-RAS files share `.hdf`
and `.h5` with other HDF5 files. Every other file keeps its driver: NetCDF-4 files, HDF5 files
without a `File Type`, and HEC's own terrain and land-cover files (`File Type` `HEC Terrain`,
`HEC Land Cover`) are not claimed.

`File Type` only says that a file is HEC-RAS's, not which kind: plans written before 6.4 can say
`HEC-RAS Geometry`, and geometry files written by 6.4 and later say `HEC-RAS Results`. The layers
come from what the file stores. `terrio::open`, `open_vector`, `terrio info` and `terrio convert`
read HEC-RAS files as vector layers; `open_raster` (and `open_raster_bytes`) still reads them as
HDF5 rasters, as before.

## Layers

A default open reads every layer the file stores. A layer the engine refuses there (an
unsupported layout, or a table the file lacks, such as the ghost-cell count of HEC-RAS 5.0 files)
is left out with a warning (a `UserWarning` in Python), unless an open option asks for something
of that layer (a join, `MESHES`): then the refusal fails the open, and an option for layers the
file does not store at all (`XS_SUMMARIES` on a file without cross sections, say) is an error
naming the option. Damaged data and I/O errors always fail the open. A layer named with `-l` (`layers`) must read, or the open fails with the
engine's refusal.

### 2D mesh

| Layer | Geometry | Fields |
|---|---|---|
| `mesh_cells` | Polygon | `mesh_name`, `cell_id`, joined cell values |
| `mesh_cell_points` | Point | `mesh_name`, `cell_id`, `is_ghost`, joined cell values |
| `mesh_faces` | LineString | `mesh_name`, `face_id`, joined face values |
| `mesh_face_points` | Point | `mesh_name`, `face_point_id`, `is_perimeter`, `elevation` |
| `mesh_perimeters` | Polygon | `mesh_name`, then the 2D flow area table |

- `mesh_name` is the 2D flow area's name; `cell_id`, `face_id` and `face_point_id` are the stored
  row indices, which repeat across meshes.
- **Cells** are the engine's polygons: rings run clockwise from the first listed face. Cells
  HEC-RAS does not draw (fewer than three faces, such as ghost cells) are left out of
  `mesh_cells`; `mesh_cell_points` has every stored cell, with `is_ghost` true for the rows at and
  above the 2D flow area's physical cell count.
- **Faces** are polylines from the start face point through the stored perimeter vertices to the
  end face point.
- **Face points** are the mesh corners. `is_perimeter` is the stored `FacePoints Is Perimeter`
  integer, unconverted (HEC-RAS writes -1 on the perimeter and 0 inside), and `elevation` the
  stored `FacePoints Elevation` (null for a mesh that stores none).
- **Perimeters** are the stored `Perimeter` polygons, listed when every 2D flow area stores one.
  The 2D flow area table follows: `name`, `locked`, `mann`, `multiple_face_mann_n`,
  `composite_lc`, `cell_vol_tol`, `cell_min_area_fraction`, `face_profile_tol`, `face_area_tol`,
  `face_conv_ratio`, `laminar_depth`, `min_face_length_ratio`, `spacing_dx`, `spacing_dy`,
  `shift_dx`, `shift_dy`, `cell_count` (HEC-RAS 7.0.1; files without the table give `mesh_name`
  only).

### 1D, 2D and network features

Each feature has `source_row`, its row in the stored attribute table, then every stored column of
that table as a field named after it (lower case, other characters as `_`: `River Name` is
`river_name`, `SA-2D` is `sa_2d`), with its stored type (text, integer or float). The columns are
the file's own; those HEC-RAS 7.0.1 writes are:

| Layer | Geometry | Stored columns (HEC-RAS 7.0.1) |
|---|---|---|
| `cross_sections` | LineString | `river`, `reach`, `rs`, `name`, `description`, `len_left`, `len_channel`, `len_right`, `left_bank`, `right_bank`, `friction_mode`, `contr`, `expan`, `hp_count`, `hp_start_elev`, `hp_vert_incr`, `hp_lob_slices`, `hp_chan_slices`, `hp_rob_slices`, `ineff_block_mode`, `obstr_block_mode`, `default_centerline`, `last_edited` |
| `river_centerlines` | LineString | `river_name`, `reach_name`, `us_type`, `us_name`, `ds_type`, `ds_name`, `junction_to_us_xs`, `ds_xs_to_junction` |
| `river_bank_lines`, `river_edge_lines`, `river_flow_paths` | LineString | the stored columns, if the file stores a table (otherwise `source_row` only) |
| `storage_areas` | Polygon | `name`, `mode`, `avg_area`, `min_elev`, `elev_vol_count` |
| `structures` | LineString | `type` (`Inline`, `Lateral`, `Bridge`, `Culvert`, `Connection`, ...), `mode`, `river`, `reach`, `rs`, `connection`, `us_type`, `us_sa_2d`, `ds_type`, `ds_sa_2d`, `description`, the weir, gate, culvert, bridge and HTAB parameters (about 100 columns) |
| `bc_lines` | LineString | `name`, `sa_2d`, `type` |
| `breaklines` | LineString | `name`, `cell_spacing_near`, `cell_spacing_far`, `near_repeats`, `protection_radius` |
| `refinement_regions` | Polygon | `name`, `spacing_dx`, `spacing_dy`, `shift_dx`, `shift_dy`, `perimeter_spacing`, `near_spacing_repeats`, `far_spacing`, `protection_radius` |
| `reference_lines` | LineString | `name`, `sa_2d`, `mode`, `type` |
| `reference_points` | Point | `name`, `sa_2d`, `cell_index`, `usxsid`, `dsxsid`, `us_fraction` |
| `river_junctions` | Point | `name`, `description`, `momentum`, `add_weight`, `add_friction`, `unsteady_energy` |
| `pump_stations` | Point | `name`, the inlet, outlet and reference river, reach, station, 2D area and pipe node, `highest_pump_line_elevation`, `pump_groups` |
| `pipe_conduits` | LineString | `name`, `system_name`, `us_node`, `ds_node`, `modeling_approach`, `conduit_length`, `max_cell_length`, `shape`, `rise`, `span`, `manning_s_n`, `us_offset`, `ds_offset`, `us_elevation`, `ds_elevation`, `slope`, the loss coefficients, `ds_gate_type`, `major_group`, `minor_group`, `ds_flap_gate` |
| `pipe_nodes` | Point | `name`, `system_name`, `node_type`, `node_status`, `condtui_connections_us_ds` (HEC-RAS's spelling), `invert_elevation`, `base_area`, `terrain_elevation`, `terrain_elevation_override`, `depth`, `top_inlet_type`, `top_inlet_elevation`, `side_inlet_type`, `side_inlet_elevation`, `total_connection_count` |

A line with several parts is a MultiLineString. A polygon feature with several parts is refused,
because which parts are holes is not a proven layout.

### Units

Values are the stored numbers, unconverted: `f32` values are widened to `f64` exactly, and
nothing is rescaled. Coordinates are in the units of the file's CRS. Lengths, elevations, flows
and velocities are in the model's unit system, the root `Units System` attribute (`US Customary`:
feet, cubic feet per second; `SI Units`: metres, cubic metres per second). Times of maxima and
minima are the stored days since the start of the simulation. The unit text HEC-RAS stores with
each result (`ft`, `cfs`, ...) is reported on the `terrio::hecras` API (`SummaryField::units`,
`JoinedField::units`, `CrossSectionField::units`, `SeriesField::units`), because vector layers
have no metadata slot; `HecRasHdf::units_system` and `HecRasHdf::file_attributes` give the
stored text of the root attributes.

## Results

Stored results join to the layers through open options (`--oo KEY=VALUE` on the command line,
`options=` in Python, `openOptions` in JavaScript, `VectorOpenOptions::set` in Rust). Every joined
value is a stored number, joined by the engine's element or feature identity. Asking for a value
the file does not store is an error, never an empty field. The options are checked before
anything is read: an incomplete request (a series option without `TIME_STEP`, `XS_RESULTS`
without `TIME_STEP` or `XS_PROFILE`) is an error whichever layers are read. With no layer named,
an option whose layers the file does not store is an error; with layers named, options for the
other layers are not looked up. Lists are comma-separated stored names, so a stored name that
contains a comma (a 2D flow area or structure named `Area, A`) cannot be given. A key given
twice keeps its last value.

| Option | Joins | Fields |
|---|---|---|
| `CELL_SUMMARIES`, `FACE_SUMMARIES` | stored `Summary Output` variables to the cell layers or `mesh_faces` (`Maximum Water Surface`, `Minimum Water Surface`, `Maximum Face Velocity`, `Cell Maximum Water Surface Error`, ...) | `<variable>`, and `<variable>_time` (stored days) when the engine proves a time row: `maximum_water_surface`, `maximum_water_surface_time` |
| `CELL_ATTRIBUTES` | `MINIMUM_ELEVATION`, `MANNINGS_N` | `minimum_elevation`, `mannings_n` |
| `FACE_ATTRIBUTES` | `MINIMUM_ELEVATION`, `NORMAL_AND_LENGTH`, `CELLS` | `minimum_elevation`, `normal_x`, `normal_y`, `length`, `cell_a`, `cell_b` |
| `CELL_SERIES`, `FACE_SERIES` (with `TIME_STEP`) | stored time series at one output row (`Water Surface`, `Face Velocity`, `Depth` where stored) | `<variable>`: `water_surface` |
| `CELL_MAXIMUM`, `CELL_MINIMUM`, `FACE_MAXIMUM`, `FACE_MINIMUM` | the engine's reduction of a stored series over every row (first occurrence; NaN propagates) | `<variable>_max`, `<variable>_max_row` (or `_min`, `_min_row`) |
| `TIME_STEP` | the stored unsteady output row (zero-based) of the series options; without one of them it is an error | |
| `XS_SUMMARIES` | stored `Summary Output/Cross Sections` variables to `cross_sections` | `<variable>`, `<variable>_time`, `<variable>_<row label>` |
| `XS_RESULTS` (with `TIME_STEP` or `XS_PROFILE`) | stored cross-section results at an unsteady row or a steady profile (`Water Surface`, `Flow`, `Velocity Channel`, ...) | `<variable>` |
| `XS_PROFILE` | the steady profile (zero-based) of `XS_RESULTS`; an error without `XS_RESULTS` or with `TIME_STEP` | |
| `XS_GEOMETRY` | `STATION_ELEVATION`, `MANNINGS_N`, `INEFFECTIVE_AREAS`: the stored per-section tables of `Geometry/Cross Sections` | JSON fields: `station_elevation` (`[[station, elevation], ...]`), `mannings_n` (`[[station, n], ...]`), `ineffective_areas` (`[{"Left Sta": .., "Right Sta": .., "Elevation": .., "Permanent": ..}, ...]`); a NaN, which JSON cannot hold, is `null` |
| `STRUCTURE_SERIES` (with `TIME_STEP`) | stored structure series (`Structure Variables`, ...) to `structures` | `<series>_<column label>` |
| `BC_SERIES=YES` (with `TIME_STEP`) | each 2D boundary-condition line's stored series to `bc_lines` | one field per stored column label (`stage`, `flow`, ...); lines of an area that stores no series get nulls |
| `REFERENCE_LINE_SERIES`, `REFERENCE_POINT_SERIES` (with `TIME_STEP`) | stored reference-line and reference-point variables | `<variable>` |
| `MESHES` | only these 2D flow areas | |
| `GEOMETRY_POLICY=STRICT\|QUARANTINE` | `QUARANTINE` leaves invalid cell polygons out of `mesh_cells` instead of failing | |
| `METADATA_MAX_BYTES`, `MAX_BYTES` | the engine's byte budgets per metadata object and per array (defaults 64 MiB and 2 GiB). `METADATA_MAX_BYTES` bounds the root attributes (their stored values together, measured from the attribute messages before the engine decodes any; a variable-length value counts its heap reference); both bound listing the layers and every layer read, and the attribute tables and coordinate arrays of the 1D, 2D and network feature layers are bounded by the smaller of the two. Data above a budget is refused before it is decoded. Detection, before any option applies, decodes only the root `File Type`, up to 4 KiB | |

Geometry and hydraulic values are the stored data. The only values not read as stored are the
optional reductions (`CELL_MAXIMUM`, `CELL_MINIMUM`, `FACE_MAXIMUM`, `FACE_MINIMUM`): the engine's
exact maximum or minimum of a stored series over its stored rows, with the row it was found at.
No quantity is derived: depth, for example, is joined only where HEC-RAS stored it (HEC-RAS 5.0
plans store a cell `Depth` series), never computed from water surface and terrain; `Summary
Output` maxima are HEC-RAS's own.

## CRS

The CRS is the root `Projection` attribute, the WKT HEC-RAS stores, attached to every layer as
that text. The driver sets no EPSG code and skips the EPSG re-identification terrio applies to
other drivers' WKT (a shapefile's `.prj`, say), so the layer keeps exactly what the modeller set.
terrio's CRS engine can still recognize the WKT when it parses it: `terrio info` shows
`EPSG:2271 (NAD83 / Pennsylvania North (ftUS))` for the Bald Eagle Creek files, and writers that
store an authority code may store that one.

A file without a projection (many geometry files) gives layers without a CRS; give one with
`--s-srs` on `terrio convert`. Malformed, conflicting, unsupported or corrupt projection evidence
is an error (`Error::Crs`), never a guess. A project's `.rasmap` and `.prj` projection files are
not read.

## Enabling the driver

The driver is the `hecras` Cargo feature, part of the default features; it implies
`netcdf-hdf5`.

- **Rust**: `terrio = "0.5"` includes it. With `default-features = false`, add it:
  `features = ["hecras"]`. The `terrio::hecras` module gives the typed API.
- **Command line**: the `terrio` binary and `cargo install terrio-cli` include it;
  `terrio formats` lists `HECRAS`.
- **Python**: the `terrio` wheels include it (`pip install terrio`).
- **JavaScript**: the full module includes it; the `core` module does not.

## Examples

These read files of the HEC-RAS 7.0.1 golden corpus,
[ras-hdf-test-corpus](https://github.com/gpt-cmdr/ras-hdf-test-corpus) (release v1.0.1, `smoke`
profile), whose files come from HEC's public example projects, run in HEC-RAS 7.0.1:
`BaldEagleDamBrk.p18.all2d.hdf` (Bald Eagle Creek dam break, 2D), `BaldEagle.p02.hdf` (Bald Eagle
Creek, 1D steady, eight profiles) and `DavisStormSystem.p02.hdf` (Davis storm sewer, pipe network).

```sh
terrio info BaldEagleDamBrk.p18.all2d.hdf
terrio info BaldEagleDamBrk.p18.all2d.hdf -l mesh_cells --oo "CELL_SUMMARIES=Maximum Water Surface"

# 2D cells with the maximum water surface and its time, as GeoPackage
terrio convert BaldEagleDamBrk.p18.all2d.hdf cells.gpkg -l mesh_cells --oo "CELL_SUMMARIES=Maximum Water Surface"

# Cross sections with their station-elevation tables and the 100-year water surface (steady profile 7)
terrio convert BaldEagle.p02.hdf xs.geojson -l cross_sections --oo XS_GEOMETRY=STATION_ELEVATION --oo "XS_RESULTS=Water Surface" --oo XS_PROFILE=7

# The storm sewer network
terrio convert DavisStormSystem.p02.hdf pipes.fgb -l pipe_conduits
```

```python
import terrio

ds = terrio.open_vector(
    "BaldEagleDamBrk.p18.all2d.hdf",
    layers=["mesh_cells", "mesh_faces"],
    options={"CELL_SUMMARIES": "Maximum Water Surface", "FACE_SUMMARIES": "Maximum Face Velocity"},
)
cells = ds["mesh_cells"].to_geopandas()
print(cells[["mesh_name", "cell_id", "maximum_water_surface", "maximum_water_surface_time"]].head())
```

```rust,no_run
use terrio::hecras::{HecRasHdf, MeshOptions};

// Through terrio's generic API, with open options
let mut options = terrio::VectorOpenOptions { layers: vec!["mesh_cells".into()], ..Default::default() };
options.set("CELL_SUMMARIES", "Maximum Water Surface")?;
let ds = terrio::open_vector("BaldEagleDamBrk.p18.all2d.hdf", &options)?;
terrio::write_vector(&ds.layers, "cells.fgb", &Default::default())?;

// Or the typed API, with the reports of each read (units, omitted cells, time rows)
let hdf = HecRasHdf::open("BaldEagleDamBrk.p18.all2d.hdf")?;
let cells = hdf.mesh_cells(&MeshOptions::default().with_summary("Maximum Water Surface"))?;
println!("{} cells, {} omitted, units {:?}", cells.layer.len(), cells.omitted.len(), cells.summaries[0].units);
# Ok::<(), terrio::Error>(())
```

```js
const ds = terrio.openVector(bytes, "BaldEagleDamBrk.p18.all2d.hdf", {
  layers: ["mesh_cells"],
  openOptions: { CELL_SUMMARIES: "Maximum Water Surface" },
});
```

## Refusals and errors

A read the engine cannot prove exact is refused: it is an error, never an empty layer, a partial
result or a guessed value. Refusals keep the engine's stable spelling at the start of the message
(`HEC-RAS unsupported(identity_layout): ...`), and map to terrio's errors:

| Engine | terrio | Meaning |
|---|---|---|
| `file_not_found` | `Error::Io` (`NotFound`) | the file is not there |
| `io`, `internal` | `Error::Io` | the file could not be read |
| `unsupported`, or a byte budget (`resource_limit`) | `Error::Unsupported` | a layout the engine does not support, or a dataset above the budget |
| `not_found`, `invalid_request`, an unavailable capability | `Error::InvalidArgument` | the file does not store what was asked for, or the request is invalid |
| `corrupt`, `structural_topology`, `invalid_geometry` | `Error::InvalidData` | the file is damaged, or its mesh topology is invalid |
| CRS evidence that is not usable | `Error::Crs` | malformed or conflicting projection |

A default open leaves a refused layer out with a warning (`Unsupported` or `InvalidArgument`);
naming the layer shows the refusal. Damaged data and I/O errors always fail the open.

## Limitations

Not read, on purpose:

- Project and plan text files (`.prj`, `.p##`, `.g##`, `.u##`, `.rasmap`), HEC-RAS execution and
  editing, and RAS Mapper's raster outputs (depth and velocity grids).
- Derived values: depth where only water surface is stored, maxima of structure series.
- Storage-area results (the engine refuses their column order).
- Multi-part polygons with holes (unproven layout).
- Structures stored without a centerline (an attribute row with no `Centerline` datasets, such as
  some lateral structures): there is no geometry to draw.
- 2D flow areas stored as outlines only, with no computed mesh: the mesh layers are listed but
  refused.
- Calibration regions (no tested file stores one), pipe network cells (`Geometry/Pipe Networks`),
  cross-section interpolation surfaces, bridge and culvert profile and coefficient tables, and
  hydraulic property tables: model tables rather than feature layers.

## In memory and in WebAssembly

`terrio::memory::open_bytes`, `open_vector_bytes`, Python's `open_bytes` and the JavaScript
package's `open` and `openVector` read HEC-RAS files from bytes (natively the bytes are spilled
to a temporary file; in WebAssembly the engine reads them in memory). Limits:

- The whole file is held in memory while it is open, and opening needs about twice its size for a
  moment (detection and the root-attribute budget read the file's metadata from a copy).
- The wasm32 module's memory is 4 GiB in all, so plan files approaching that size cannot be read
  there; native builds have no such limit.
- The byte budgets (`METADATA_MAX_BYTES`, `MAX_BYTES`) apply as for files: root attributes and
  datasets above them are refused before they are decoded.

Range reads (HTTP, S3) are not supported yet.

## How it works

The driver is a thin adapter over
[`ras-commander-fast-hdf`](https://crates.io/crates/ras-commander-fast-hdf), the read-only
HEC-RAS HDF engine of the [ras-commander](https://github.com/gpt-cmdr/ras-commander) Python
library (MIT). The engine owns every HEC-RAS rule: the HDF layouts, mesh, cell and cross-section
identities, polygon ring order, CRS evidence, units and refusals. The driver translates its values
into terrio layers, fields and errors. Attributes are read without h5py: compound `Attributes`
tables column by column with their stored types, fixed-length strings decoded as h5py decodes them
(padding removed, nothing else trimmed), and root attributes (`File Type`, `File Version`,
`Units System`, `Projection`) through `HecRasHdf::file_attributes`.

terrio and the engine use the same `hdf5-reader` 0.10.1 from crates.io (roteiro-gis/netcdf-rust),
so a build with `hecras` compiles one HDF5 reader (`cargo tree -d -e normal -p terrio` lists the
same duplicate crates with and without `hecras`, none of them an HDF5 or NetCDF crate).

## Build and size

`hecras` is a default feature, as every terrio driver is. Over the other default features it adds
one crate, the engine (`ras-commander-fast-hdf` 0.2.4, with the features `hdf`, `mesh` and
`results`); its other dependencies, including the HDF5 reader, are already terrio's. Measured on
Windows x86-64 with Rust 1.99.1, release profile (2026-10-28):

| | Without `hecras` | With `hecras` | Difference |
|---|---:|---:|---:|
| `terrio` command-line binary | 68.71 MB | 71.61 MB | +2.90 MB (+4.2%) |
| JavaScript module (`terrio-js`, `full` features; `cargo build`, before `wasm-bindgen` and `wasm-opt`) | 42.20 MB | 45.64 MB | +3.45 MB (+8.2%) |
| The same, gzipped (`gzip -9`) | 11.08 MB | 11.59 MB | +0.51 MB (+4.6%) |

To reproduce them from a checkout:

```sh
# Command-line binary, with hecras (the defaults), then without it
cargo build --release -p terrio-cli
sed -i 's/"libzstd", "hecras"\]/"libzstd"]/' crates/terrio/Cargo.toml
CARGO_TARGET_DIR=target-without cargo build --release -p terrio-cli
git checkout -- crates/terrio/Cargo.toml

# JavaScript module, without and with hecras
F=cad,core,parquet,avif,netcdf-hdf5,osm,jp2,pointcloud,zarr,grib
cargo build --release -p terrio-js --target wasm32-unknown-unknown --no-default-features --features $F
cargo build --release -p terrio-js --target wasm32-unknown-unknown --no-default-features --features $F,hecras
```

**Licence.** The engine is MIT; its crate licence is `MIT AND ISC` because it vendors
`freestiler-core`, whose supercluster/KDBush port is ISC. That code compiles only under the
engine's `python` feature, which terrio does not enable, so no ISC-licensed code is compiled into
terrio. Cargo's licence metadata is per crate, so `cargo about` (`LICENSE-THIRD-PARTY.md`) still
lists ISC for the engine.

## Testing

```sh
cargo test -p terrio --test hecras_driver --test hecras_results --test hecras_synthetic
python scripts/compare_hecras_with_h5py.py <corpus>/fixtures   # the 7.0.1 comparison with h5py
```

- **Unit tests** run on synthetic files in `crates/terrio/tests/data/hecras` (written by
  `scripts/make_hecras_fixture.py`, `scripts/make_hecras_root_fixtures.py` and
  `scripts/make_hecras_area_fixture.py`, or copied from the
  engine's committed synthetic fixtures; no model data).
- **The comparison with h5py** reads a corpus directory, whose location is a parameter, and
  compares its HEC-RAS 7.0.1 files: every cell polygon, face polyline, perimeter ring and face
  point, every family feature with every stored attribute column (type and value), the 2D flow
  area and cross-section tables, the root attributes, and every joined value, bit for bit. A
  refusal is accepted only where the test lists it. GDAL cannot read HEC-RAS files, so the
  reference is h5py: ras-commander reads these datasets with h5py, and `scripts/hecras_oracle.py`
  reads them the same way, independently of terrio and its engine. See
  [Conformance](conformance.md).
- **Test data** is public only: the
  [ras-hdf-test-corpus](https://github.com/gpt-cmdr/ras-hdf-test-corpus) release (HEC's example
  projects and FEMA eBFE models, run in HEC-RAS 7.0.1, plus synthetic files derived from them,
  such as one with its projection replaced; CC0-1.0 for the corpus's own contribution, with each
  file's source or derivation in its manifest and the upstream attribution in its NOTICE),
  fetched at a pinned tag with its SHA-256 sums checked.
  CI runs the comparison on its `smoke` profile on every push to `main` and, for pull
  requests, on demand (the `ci:hecras` label, or a manual run of *CI (npm and conformance)*);
  no model file is committed to terrio.

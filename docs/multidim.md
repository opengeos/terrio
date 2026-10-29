# Native multidimensional arrays

The `multidim` API exposes scientific groups, named dimensions, coordinate arrays and numeric
variables independently of raster bands. Opening reads metadata; array elements are fetched
when a view is read. Rust, CLI/MCP, Python and JavaScript use the same pure-Rust implementation.
Existing `open_raster` and subdataset raster views continue to work.

```python
import terrio

cube = terrio.open_multidim("ocean.nc")
print(cube.array_paths)
a = cube["/science/temperature"]
print(a.shape, a.dimension_names, a.attributes, a.coordinates)
slab = a.isel("time", 2).isel("depth", 0).transpose(["lon", "lat"])
values = slab.read()  # writable NumPy array, in the variable's native numeric type
cube.write("ocean.zarr")
```

## Data model and reads

A dataset contains a root group. Groups contain attributes, locally defined dimensions, numeric
arrays and child groups. A dimension records its local name, absolute defining path, current
length, unlimited status and optional one-dimensional indexing variable. Arrays retain their
ordered dimension references, source chunk shape, attributes and coordinate-array references,
including CF auxiliary `coordinates` when available. Coordinates are arrays and remain lazy.
Anonymous HDF5/Zarr axes get stable names derived from the array name and axis position.

`isel(dimension, index)` removes one axis. `slice(dimension, start, count, step=1)` selects a
positive-stride range relative to the current view. `transpose` reorders the remaining axes.
All three construct lazy views; `read` materializes the selected values. Dimension selectors
accept either a local name or its absolute defining path; ambiguous or missing names are errors.
Indices are zero-based. Empty ranges and scalar arrays are supported.
For repeated dimension names, such as a covariance array with axes `[x, x]`, use
`isel_axis(axis, index)` and `slice_axis(axis, start, count, step=1)` in Rust/Python,
`iselAxis`/`sliceAxis` in JavaScript, or `--isel-axis`/`--slice-axis` in the CLI.
Axis positions refer to the current view and change after indexing or transposition.

Values retain the source's packed representation, original axis order, signedness and numeric
width, including exact 64-bit integers. Scale/offset, units, fill values and calendar metadata
remain attributes; CF masking, unpacking and date decoding are left to the caller. Floating
nonfinite attributes and CLI JSON values use `NaN`, `Infinity` and `-Infinity` strings. Rust
buffers and NumPy/JavaScript typed arrays keep the original floating-point values.
Original numeric attribute types are retained internally, so netCDF snapshots preserve
Float32 attributes and numeric nonfinite values without converting literal text `NaN` to a number.

Native reads support local netCDF classic/CDF-2/CDF-5, netCDF-4 and numeric HDF5, including JPSS
user blocks, plus local/remote Zarr v2/v3 using the existing codecs and store backends. netCDF/HDF5
hyperslabs read through range-backed storage. Zarr views fetch only source chunks containing selected elements, including sparse positive
strides; sharded stores fetch the selected inner chunks when their codecs permit range reads.
The source chunk is the minimum decompression unit. Array handles keep their source storage
alive after the parent dataset is released.

String, compound and other unsupported payload types/encodings are reported in
`unsupported_arrays`; numeric siblings remain accessible. General string/compound reads are
outside this initial numeric API. Scalar netCDF character placeholders used for grid mappings
are retained as UInt8 with their metadata. netCDF/HDF5 paths are local files; in-memory bytes
support browser and Python callers. Remote Zarr stores retain their existing range-reading path.

## Snapshots

`Dataset.write(destination)` chooses netCDF-4 for `.nc` and Zarr v2 for `.zarr`. The destination
must be new. `_FillValue` must be numeric (or a floating nonfinite marker) and
representable in the array's numeric type. Snapshots preserve groups, numeric values/types, coordinate variables, attribute
values and source chunk shapes where the destination permits them. netCDF output clamps chunks
to fixed-axis lengths, while retaining chunk sizes on unlimited axes and the unlimited flags.
Zarr output uses
standard `_ARRAY_DIMENSIONS` names and stores dimension identity, inherited coordinate
references and unlimited provenance in `_terrio_dimensions` JSON attributes. A reserved
`_terrio_attribute_types` map retains original numeric attribute widths across netCDF–Zarr
snapshots; it is removed from the public attributes on reopen. Other readers can ignore these
extensions. Source compression settings are not copied; snapshots are uncompressed.

Zarr snapshots iterate source chunks and keep one output chunk in memory. When the source
has no chunk shape, inferred chunks are capped at 8 MiB in both snapshot formats;
source-declared chunk shapes are preserved where valid. netCDF serialization
currently materializes its numeric arrays before encoding the file. A snapshot containing an
unsupported payload fails explicitly rather than silently dropping that array.
Native snapshots stage their output before publishing it to a new destination. An unreadable
source chunk leaves no partial destination; an existing destination is never replaced.

Python `open_multidim` accepts a path, netCDF/HDF5 bytes, or a dictionary of Zarr keys and bytes.
`to_bytes()` writes netCDF-4 bytes; `to_zarr_store()` returns a Zarr key/value dictionary.

```js
import { openMultidim } from "terrio-js";

const cube = openMultidim(bytes); // netCDF/HDF5 bytes or a Zarr key/Uint8Array object
const slice = cube.array("/temperature").isel("time", 2).transpose(["lon", "lat"]);
console.log(slice.shape, slice.dimensionNames);
const values = slice.read(); // flat native TypedArray; Int64/UInt64 use BigInt typed arrays
const netcdf = cube.toBytes();
const zarrFiles = cube.toZarrStore();
```

JavaScript `shape` is a native-width typed array: `Uint32Array` in the 32-bit modules
and `BigUint64Array` in `terrio-js/memory64`. For small dimensions,
`Array.from(array.shape, Number)` provides ordinary numbers in either module.
Keep BigInt dimensions when they exceed JavaScript's safe integer range. Numeric
array samples retain their native typed arrays, including exact Int64/UInt64 values.

Rust entry points are `terrio::multidim::{open, open_bytes, open_zarr_store}`. `Dataset::array`
returns a cloneable handle; `Array::view` exposes lazy indexing/slicing/transposition and
`View::read` returns an exact little-endian `Buffer`. `Buffer::data` converts to the shared typed
numeric buffer without passing through floating point. `Array::from_bytes` supports constructing
numeric in-memory datasets with explicit `ArrayInfo` and dimensions.

```sh
terrio mdinfo ocean.nc --json
terrio mdread ocean.nc --array /temperature --isel time=2 --isel depth=0 \
  --slice lat=10,20 --slice lon=20,30,2 --transpose lon,lat
terrio mdconvert ocean.nc ocean.zarr
terrio mdmosaic west.nc east.nc combined.zarr
```

`mdread` emits the selected shape, dimensions, type and row-major values as JSON. Indexing is
applied before ranges, then transposition. The MCP tools are `mdinfo`, `mdread`, `mdconvert` and `mdmosaic`;
the first two have read-only annotations. JSON reads default to at most 1,000,000 selected
elements, checked before payload reads; use selectors to reduce the result, increase
`--max-elements`, or explicitly set `--max-elements 0` for an unrestricted CLI read.

## Conformance

`scripts/compare_multidim_with_gdal.py` compares real NOAA wind, JPSS HDF5, Sentinel-5P methane
and GDAL-derived SRTM Zarr data with GDAL's multidimensional API and xarray. It checks native
values/types, named axes, coordinate associations, selected/transposed reads, numeric snapshots,
source chunk shapes and unlimited-axis metadata. A chunked four-dimensional cube built from real
wind data exercises time/depth slices and verifies that corrupted unselected netCDF/Zarr chunks
are skipped. Run it through `scripts/conformance.py --only multidim`.

The supported model follows the numeric subset of
[GDAL's multidimensional data model](https://gdal.org/en/stable/user/multidim_raster_data_model.html).

The range-read regression uses a 4,322,152-byte chunked cube: metadata requests 19,144
bytes, a 16×16 slab requests another 121,696 bytes (including chunk-index metadata), and a
subsequent whole-array read requests 4,313,952 bytes. The selected read requests about 2.8%
of the whole-read bytes while returning the same native values.

A local HTTP Zarr regression reads two UInt16 values 1,000 positions apart using exactly
two chunk requests and four payload bytes after metadata, while skipping a corrupted intervening
chunk. Sparse strided reads keep chunk-sized temporary buffers plus the selected result, rather
than allocating the full bounding rectangle.


CLI selections apply in fixed groups: `--isel`, `--slice`, `--isel-axis`,
`--slice-axis`, then `--transpose`, regardless of flag order. Numeric axes
refer to the view remaining after earlier groups and earlier indexing entries.

Arrays sharing a named dimension in one group must declare the same length.
Conflicting metadata is rejected rather than silently renaming dimensions and
changing coordinate identity. Open a selected array or subgroup directly with
a Zarr selector to inspect it independently of an inconsistent sibling.


In-memory netCDF/HDF5 opens copy the encoded input into one owned storage buffer
shared by both metadata readers; they do not promise zero-copy storage. HDF5 userblock
prefixes are omitted from that buffer. Lazy selections avoid decoding unselected payloads,
but retain the encoded input. Prefer local file paths for large netCDF/HDF5 files.

Zarr snapshots stage payloads before exclusively creating the destination and
moving entries into it. Publication is not an atomic directory swap: coordinate
concurrent readers until the write returns. A process interruption can leave a
partial destination that must be removed before retrying. This preserves the
no-clobber guarantee even when another process creates an empty destination.

Named CLI transposition rejects ambiguous dimension names with an explicit error;
use unique full dimension paths when available. Repeated identical dimension paths
cannot be transposed by name.

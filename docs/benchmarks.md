# Benchmarks

The previous tables have been withdrawn following [#246](https://github.com/opengeos/terrio/issues/246).
They used a GDAL thread setting known to be slower for some tasks, did not validate outputs before
scoring them, and included runs made while other work was using the machine. Those speed-up ratios
are not supported as comparisons of equivalent work at each tool's best observed settings.

The replacement harness uses GDAL **3.13.3**, the current release listed on GDAL's
[download page](https://gdal.org/en/stable/download.html), and a release build of Terrio. Results
are measurements of complete command invocations, including startup, reading, and writing.
They are machine-specific snapshots, not guarantees of performance on other data or hardware.

## Method

- Each tool is tuned independently at CPU budgets of 1, 2, 4, 8, 16, and the available maximum.
  Every setting receives a warm-up and three timed samples. Each tool selects its own lowest
  median, then receives an independent warm-up and fresh measurement samples. These are the best
  observed settings among the tested budgets, not a claim of globally optimal configuration.
- Measurement runs alternate Terrio/GDAL and GDAL/Terrio order. Tables show the median and full
  minimum–maximum range of seven samples, or five for large files. Raw samples and tuning results
  accompany the tables in JSON. Ratios describe observed medians, not statistical significance.
  The Checks column marks a **wide timing range** when either tool's maximum sample exceeds twice
  its median; this annotation does not alter the samples or their median ratio.
- On Linux, `taskset` restricts each process and all its child threads to its selected CPU budget.
  `RAYON_NUM_THREADS`, `GDAL_NUM_THREADS`, and `gdalwarp`'s `NUM_THREADS` also configure the pools.
  A budget of one therefore bounds Terrio's workers outside Rayon as well. On other platforms,
  these variables are hints; the script does not claim a hard CPU limit.
  The Linux runner uses the lowest-numbered available CPU IDs for both tools.
  Budgets count logical CPUs; the selection does not distinguish physical cores from SMT siblings.
  Hardware topology and OS placement can
  therefore affect these machine-specific results. Budgets above the available affinity are rejected.
- The Linux runner estimates competing CPU use from machine CPU time minus the measured child's
  CPU time. Estimates are averaged by elapsed time across each paired tuning or measurement
  series, reducing clock-tick noise in short commands. If a series exceeds one CPU, the row
  receives no ratio. The check is intentionally machine-wide: work outside the pinned CPU set can
  still contend for shared caches, memory bandwidth and package power. Raw background estimates
  are retained. This detects contention; it does not
  provide hardware isolation or remove filesystem and network noise. On other platforms,
  background CPU use is unmeasured and ratios carry no background-load guarantee.
- Every invocation starts with an empty, separate output directory. PAM statistics caches are
  disabled. Validation runs after timing, against the final output of each selected setting.
  The benchmark does not prove that every earlier invocation produced the same output.
- GDAL uses its single-process `gdal raster mosaic` and `gdal raster polygonize` commands.
  Mosaic resolution is explicitly averaged, as in the former `gdalbuildvrt` command, to accept
  tiles whose resolutions differ at floating-point precision. Both COG writers build fresh average
  overviews; GDAL is explicitly told to ignore source overviews. Warp benchmarks retain NAD83 (`EPSG:4269` → `EPSG:26913`) to avoid measuring different
  datum transformations or grid availability. Networking for datum grids is disabled.
- GDAL Shapefile output explicitly uses UTF-8 encoding to match Terrio. The countries GeoJSON
  fixture has scalar attributes supported by Shapefile.
- Memory ratios are omitted: launching commands from a Python process that also validates large
  datasets can contaminate child peak-RSS accounting with inherited parent memory.

## Output mismatch follow-up

The [#253 investigation](benchmark-parity.md) records the underlying corrections.
A [current release recheck](benchmark-253-current.md) at `328c3cc2` passes the established
output validation on all eight original public-fixture cases against GDAL 3.13.3.
It includes fresh timing samples and independent tuning at CPU budgets one and
two; all eight rows pass the unchanged background-load gate. Separate checks at
one, two, four and twelve CPUs match both GDAL and each tool's single-thread
outputs. Full-image diagnostics and raw commands/samples are retained with the report.

The [earlier rerun](benchmark-253-rerun.md), its six-pass result, and the older
tables below are historical snapshots. Their samples are preserved rather than
combined with the current measurements. The five vendor-reported reproductions
still lack their exact inputs and commands, so #253 remains open for that scope.

## Correctness gates

A zero exit status is insufficient. Failed commands, validation errors, differing outputs, and
excessive background CPU use are visible in the table and receive **no speed ratio**.

Raster outputs are read through GDAL and compared at every base and overview pixel, including
validity masks, data types, nodata, scale/offset, units, color interpretation and color tables.
CRSs must be semantically equal; affine transforms use absolute tolerance `1e-8` and relative
tolerance `1e-12`. Pixels are compared exactly (with NaNs equal). Approximate pixel agreement
is deliberately not scored as equivalence; a rounding difference may therefore exclude an
otherwise useful result. Metadata outside these checks, compression ratios, and physical file
layouts are not asserted to be identical.

Vector outputs compare field names/types, CRS, and every feature's attribute values and normalized
geometry WKB. Disk-sorted hashes allow different feature order without keeping whole datasets in
memory. Generated feature IDs and layer names are ignored. Coordinates are compared exactly after
normalizing ring starts, orientations and multipart order. Topologically equal geometries with
different vertices can consequently be excluded. The check is conservative rather than a test of
all possible notions of vector equivalence.

Summary cases check the captured JSON against the source. Statistics from **both** commands are
checked against an independent NumPy reduction of valid source pixels, using relative tolerance
`1e-6` and absolute tolerance `1e-8`. This matters for GDAL 3.13.3: the SRTM case returned incorrect
statistics in our check, even though `gdalinfo` exited successfully. It must not receive a speed
ratio for that result.

A controlled check used the same `gdalinfo -json -stats srtm90.tif` command with
`GDAL_PAM_ENABLED=NO`. The source had no stored `STATISTICS_*` keys and no nodata value.
GDAL 3.13.3 with `GDAL_NUM_THREADS=1` matched the full-resolution NumPy reduction (minimum 15,
maximum 4338); with eight threads it returned minimum 26 and maximum 4148. GDAL 3.12.2 matched
NumPy at both thread counts. This identifies a thread-dependent discrepancy in the tested GDAL
3.13.3 build, not a requested approximate-statistics mode or a stored-statistics cache. GDAL's
reported valid percentage falls with the thread count, 12.54 at eight threads, as
[measured here](benchmark-parity.md#gdal-multithreaded-statistics). Its internal cause remains
unisolated; the invalid eight-thread result is excluded from speed comparisons.

## Rerun on 24 October 2026

The rerun covers all 19 small-file/remote cases plus the large-mosaic window, hillshade and slope
cases. The other historical large-file timings remain withdrawn. The full case list is still
available in the script.

22 distinct cases were measured; 13 passed the output checks and 8 also passed the background-load
gate. Other computation jobs were temporarily paused for these measurements. Any rows still marked
**background load** are diagnostic timings, not supported performance comparisons.

Hardware: Intel Xeon w7-3445, 40 available logical CPUs, Linux. GDAL 3.13.3; Terrio 0.5.0, optimized
release build. All runs tested CPU budgets 1, 2, 4, 8, 16 and 40, with three tuning samples per
budget. Small-file measurements use seven fresh samples and large-file measurements use five. Times
below are median [min–max].

[Raw tuning and measurement samples, commands, background CPU estimates and complete validation
details](benchmark-results.json) are retained for every row. The nearest warp was repeated after an
earlier load rejection; the table uses the latest run for that task and retains both runs in JSON.
Machine-specific paths are replaced with documented placeholders. A dash means the row was excluded
from speed comparisons.


### Summaries

| Task | terrio median [range] | GDAL median [range] | CPUs T / G | terrio vs GDAL | Checks |
|---|---|---|---|---|---|
| Raster info: USGS 3DEP tile (3612×3612) | 14 ms [13 ms–15 ms] | 55 ms [53 ms–57 ms] | 1 / 1 | — | background load |
| Raster statistics: SRTM 90 m (4269×2465) | 105 ms [99 ms–108 ms] | 47 ms [44 ms–51 ms] | 8 / 8 | — | GDAL statistics differ from NumPy |
| Vector info: US counties GeoPackage | 18 ms [14 ms–20 ms] | 28 ms [26 ms–30 ms] | 4 / 1 | 1.56× faster | pass |

### Raster conversion

| Task | terrio median [range] | GDAL median [range] | CPUs T / G | terrio vs GDAL | Checks |
|---|---|---|---|---|---|
| GeoTIFF → COG (Deflate): USGS 3DEP tile | 195 ms [189 ms–200 ms] | 296 ms [286 ms–309 ms] | 40 / 40 | — | pixels differ (exact check) |
| GeoTIFF → GeoTIFF (LZW, tiled): Landsat (4 bands) | 56 ms [55 ms–59 ms] | 80 ms [77 ms–84 ms] | 40 / 16 | — | background load |
| GeoTIFF → Zarr: SRTM 90 m | 64 ms [61 ms–66 ms] | 51 ms [50 ms–55 ms] | 16 / 8 | — | band 1: color interpretation differs |

### Raster operations

| Task | terrio median [range] | GDAL median [range] | CPUs T / G | terrio vs GDAL | Checks |
|---|---|---|---|---|---|
| Translate: window + Int16: USGS 3DEP tile | 46 ms [44 ms–49 ms] | 56 ms [54 ms–57 ms] | 40 / 40 | 1.21× faster | pass |
| Warp to NAD83 / UTM 13N, nearest: USGS 3DEP tile | 155 ms [149 ms–589 ms] | 229 ms [214 ms–482 ms] | 40 / 8 | 1.48× faster | pass; wide timing range |
| Warp to NAD83 / UTM 13N, bilinear: USGS 3DEP tile | 197 ms [194 ms–202 ms] | 261 ms [257 ms–271 ms] | 40 / 8 | — | pixels differ (exact check); background load |
| Mosaic 4 USGS 3DEP tiles → GeoTIFF | 373 ms [285 ms–769 ms] | 541 ms [513 ms–1.02 s] | 40 / 8 | 1.45× faster | pass; wide timing range |
| Mosaic 4 USGS 3DEP tiles → VRT | 14 ms [13 ms–14 ms] | 33 ms [31 ms–33 ms] | 1 / 1 | — | band 1: color interpretation differs; background load |

### Vector conversion

| Task | terrio median [range] | GDAL median [range] | CPUs T / G | terrio vs GDAL | Checks |
|---|---|---|---|---|---|
| Shapefile → GeoPackage: 7,342 populated places | 57 ms [54 ms–61 ms] | 80 ms [74 ms–82 ms] | 8 / 1 | — | field names/types differ |
| GeoPackage → FlatGeobuf: US counties | 51 ms [49 ms–56 ms] | 64 ms [62 ms–67 ms] | 4 / 8 | — | background load |
| GeoPackage → GeoParquet: US counties | 70 ms [68 ms–76 ms] | 104 ms [100 ms–107 ms] | 4 / 1 | 1.48× faster | pass |
| GeoJSON → Shapefile: world countries (176 features) | 31 ms [27 ms–33 ms] | 49 ms [47 ms–51 ms] | 8 / 1 | 1.60× faster | pass |
| Filter + reproject: US counties → EPSG:5070 | 45 ms [42 ms–48 ms] | 50 ms [47 ms–52 ms] | 4 / 1 | — | features differ (exact check); background load |

### Raster ↔ vector

| Task | terrio median [range] | GDAL median [range] | CPUs T / G | terrio vs GDAL | Checks |
|---|---|---|---|---|---|
| Rasterize US counties at 0.01° | 137 ms [133 ms–160 ms] | 317 ms [315 ms–327 ms] | 40 / 4 | — | background load |
| Polygonize NLCD land cover (966×731) | 585 ms [582 ms–618 ms] | 432 ms [413 ms–442 ms] | 16 / 16 | — | background load |

### Remote data

| Task | terrio median [range] | GDAL median [range] | CPUs T / G | terrio vs GDAL | Checks |
|---|---|---|---|---|---|
| Info of a remote COG (USGS 3DEP on S3) | 471 ms [334 ms–540 ms] | 623 ms [619 ms–636 ms] | 1 / 1 | 1.32× faster | pass |

### Large rasters

| Task | terrio median [range] | GDAL median [range] | CPUs T / G | terrio vs GDAL | Checks |
|---|---|---|---|---|---|
| Window 2048² of the mosaic | 38 ms [38 ms–39 ms] | 62 ms [56 ms–67 ms] | 16 / 16 | 1.65× faster | pass |
| Hillshade of the mosaic | 498 ms [495 ms–511 ms] | 3.96 s [3.91 s–4.03 s] | 40 / 4 | — | pixels differ (exact check) |
| Slope of the mosaic | 788 ms [718 ms–1.61 s] | 4.67 s [4.61 s–4.84 s] | 40 / 4 | — | pixels differ (exact check); wide timing range |

## Terrain rows, 5 November 2026

The hillshade and slope cases of the large mosaic, rerun when `terrio dem` began computing its
products a row at a time: the previous build (main at `696709c`) and the new one, each measured
against GDAL by the harness above on the same machine, at the best CPU budgets (1 to 32) and at
one CPU (`--thread-counts 1`). The `--kernel-version 3.13` arithmetic that matches GDAL 3.13.3
supports only these two products; the others are not compared with GDAL here.

Hardware: Intel Core i9-14900K, 32 available logical CPUs, Linux. GDAL 3.13.3; Terrio 0.7.0,
optimized release builds. Large-file measurements use five samples; times are median [min–max].
All rows pass output validation and the background-load gate. A first run of the previous build
failed that gate and is excluded; it is kept with the others in the
[raw samples](benchmark-results-2026-11-05.json), where each run's `terrio_commit` names the build
it measured: `696709c` for the previous build, `885b5a2` for this one. These timings come from a
different machine than the 24 October rerun, so the two tables are not comparable with each other.

| Task | CPUs | previous build | this build | GDAL | terrio vs GDAL |
|---|---|---|---|---|---|
| Hillshade of the mosaic | best (32 / 16) | 684 ms [672 ms–708 ms] | 566 ms [554 ms–637 ms] | 2.70 s [2.69 s–2.74 s] | 4.77× faster |
| Slope of the mosaic | best (32 / 16) | 1.08 s [1.07 s–1.09 s] | 1.06 s [1.06 s–1.08 s] | 3.51 s [3.49 s–3.53 s] | 3.31× faster |
| Hillshade of the mosaic | 1 | 5.77 s [5.73 s–5.79 s] | 4.97 s [4.93 s–5.02 s] | 3.19 s [3.17 s–3.20 s] | 1.56× slower |
| Slope of the mosaic | 1 | 7.22 s [7.16 s–7.24 s] | 6.73 s [6.72 s–6.75 s] | 4.01 s [4.00 s–4.02 s] | 1.68× slower |

The GDAL column and ratio are from the runs of this build; GDAL's medians in the previous build's
runs are within 0.5% of these. On one CPU most of Terrio's time is reading the 14412 × 14412
Deflate mosaic: `terrio info --stats` of it alone takes about 4.1 s there, against 4.0 s for the
whole of `gdaldem slope`. With all CPUs, slope is bound by that reading and the writing, so its
faster rows barely show.

## Running the benchmarks

Use the latest GDAL release and matching Python bindings. The commands below pin the version
used for this update; check GDAL's download page before a future rerun.

Linux CPU affinity requires `taskset` from util-linux. Vector validation requires an external
`sort` supporting `-S 64M` and `-T DIR`, as provided by GNU coreutils or compatible BSD versions.
`gsort` is preferred when available (for example, GNU coreutils installed on macOS). Missing or
incompatible sort commands are reported as validation failures and receive no ratio. Windows
users need these Unix tools through WSL or another compatible environment.
Vector geometry normalization also requires GDAL built with GEOS. Missing GEOS or a failed
normalization produces a validation failure and receives no ratio.

```sh
conda create -n terrio-benchmark -c conda-forge gdal=3.13.3 libgdal-arrow-parquet=3.13.3 numpy
conda activate terrio-benchmark
export PROJ_DATA="$CONDA_PREFIX/share/proj"
scripts/fetch_test_data.sh ../terrio-data
scripts/fetch_large_data.sh ../terrio-large-data
CARGO_BUILD_JOBS=2 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 cargo build --release -p terrio-cli

# Keep large temporary outputs on disk, and stop competing workloads before timing.
mkdir -p ../terrio-benchmark-scratch
TMPDIR="$PWD/../terrio-benchmark-scratch" python scripts/benchmark.py ../terrio-data \
  --large ../terrio-large-data --remote --out benchmarks.md
```

`--threads N` sets the **maximum** candidate CPU budget, rather than claiming that both tools
use exactly N threads. `--thread-counts 1,4,16` supplies an explicit candidate set. `--runs`,
`--large-runs`, and `--tune-runs` control the respective sample counts; all must be positive.
`--only hillshade` selects a description substring for a focused rerun. `--max-background-cpus`
controls the competing-load threshold; increasing it weakens that protection and is recorded in
JSON. `--out FILE` writes Markdown and `FILE.json`, checkpointing completed rows. The recorded
`source_commit` identifies the harness checkout; the selected `TERRIO_BIN` and its reported version
are recorded separately.

The regression tests cover wrong-but-successful commands, independent tuning, alternating order,
fresh outputs, inherited CPU affinity, noisy measurements, pixel/mask/metadata differences,
vector feature differences, and a deliberately corrupted GDAL statistics summary:

```sh
python -m unittest discover -s scripts/tests -v
```

## Performance matrix

`scripts/perf_matrix.py` looks for slow cases across a whole format in one run, rather than
one file at a time. Each format lists the ways its files vary and the ways they are read:

| Format | Files vary by | Reads |
|---|---|---|
| GeoTIFF | codec and predictor, data type, tiled or stripped, band or pixel interleave, 1–12 bands | one window, 16 windows, whole file, 8x decimated (overview), 1, 2, 4 and 8 threads sharing 16 windows |
| Zarr | v2 or v3, codec (zlib, gzip, zstd, Blosc LZ4/Zstd with shuffle or bitshuffle, none), data type, chunk size, sharding, cube chunking | one window, 16 windows, whole array, one band of a cube, 1, 2, 4 and 8 threads sharing 16 windows |
| GeoParquet | geometry type, WKB or GeoArrow, compression, row group size, covering bbox column, spatial sort | all columns, two columns, a bbox of 1% of the extent, 16 bboxes, 1, 2, 4 and 8 threads sharing 16 bboxes, an attribute filter |
| FlatGeobuf | geometry type, with or without a spatial index | all columns, two columns, a bbox of 1% of the extent, 16 bboxes |
| GeoPackage (vector) | geometry type, R-tree or none, rows in Hilbert order or not, 400,000 or 2,000,000 rows | as GeoParquet's |
| Shapefile | geometry type, `.qix` spatial index or none, UTF-8 or LATIN1 `.cpg` | all columns, two columns, a bbox of 1% of the extent, 16 bboxes, 1, 2, 4 and 8 threads sharing 16 bboxes |
| GeoTIFF writes (`geotiff-write`) | codec and predictor (none, DEFLATE, LZW and ZSTD with predictor 2 or 3, RGB and YCbCr JPEG), data type, tile size, band or pixel interleave, 1–12 bands | a COG with average overviews, a COG without overviews, a tiled GeoTIFF, the COG with one thread |
| GeoParquet writes (`geoparquet-write`) | geometry type (400,000 polygons or lines, 2,000,000 points) | WKB, a covering bbox column, Snappy, row groups of 65,536 rows |
| GeoTIFF, real data (`geotiff-real`) | public files: a Sentinel-2 band with nodata, a sparse COG and a COG with a mask band made from it, a big-endian DEM in strips | as GeoTIFF's, and every band's mask |
| GeoParquet, real data (`geoparquet-real`) | one public file: 737,334 building footprints | as GeoParquet's, on the file's own extent and columns |

GDAL or zarr-python writes one fixture per file combination. terrio and GDAL each read every
fixture in every way, from a local path and over HTTP from a local server that adds a 20 ms
round-trip time. Every read runs in a fresh process, in alternating order, three times; the
median is reported. A process's first open and read pay one-time costs (library loading, thread
pools); they are measured on a tiny file before the case and reported apart as `init`.

Every case records:

- time (open plus read), against GDAL's;
- bytes read (local: `rchar` of `/proc/self/io`) or transferred (HTTP), against GDAL's and against
  the bytes the read needs: the compressed chunks, tiles or row groups' columns it touches, read
  from the file's own chunk tables;
- HTTP requests and sequential rounds of requests: a request that starts after every request in
  flight has ended waited for them, so it opens a new round;
- peak memory, and whether terrio's values equal GDAL's, in every sample of both tools. Time and
  peak memory are taken before this check.

The values check hashes the values a read returned, not its metadata (a CRS or a geometry type
returned beside a table is not compared). A raster read hashes as its data type and its
bytes in C order (a band read as `(1, h, w)` or `(h, w)` hashes the same); a table hashes each
column by name, cast to a canonical type (64-bit integers and floats, large strings and binaries,
top-level extension columns as their storage) and serialized as Arrow IPC, so row order, every attribute and
the geometry's bytes count. A case whose hashes equal GDAL's in every sample is `exact`. Otherwise
the check falls back to the sum and count of the pixels, or of the rows' `id` (`value` when `id`
is not read): counts equal and sums within relative tolerance `1e-6` give `checksum_match`, which is
still ranked but listed apart as not byte-identical. Anything else is a mismatch. A geometry that
one tool returns as WKB and the other as native GeoArrow (a bbox read of a GeoArrow file: terrio
native, GDAL WKB) hashes differently, so these cases are `checksum_match` at best.
Process peak RSS includes initialization, warm-up and retained read results, but excludes
subsequent validation allocations. On Linux it is the worker's own, `VmHWM` of
`/proc/self/status`: `ru_maxrss` carries the parent's memory over fork and exec, so it would
include the runner's. Without `VmHWM` it is `ru_maxrss`, its units normalized for Linux and macOS.

Only cases where every sample of both tools succeeded and matched GDAL's values (`exact` or
`checksum_match`) are ranked; the report lists failures, timeouts and value mismatches apart. Local byte counts need Linux's
`/proc/self/io`. Local reads after the first come from the OS page cache, so the matrix measures
decoding and I/O logic, not the disk.

Writes time the write call alone. Each tool loads a lossless GeoTIFF fixture into memory first
(terrio's `open_raster`, a GDAL `MEM` copy), writes a 256 × 256 crop of it with the case's options
as its warm-up, then writes the whole raster to a new file on local disk: terrio with
`terrio.write_raster`, GDAL with `gdal.Translate` and the `COG` or `GTiff` driver. Both get the
same creation options, spelled out (compression, predictor, block size, interleave, photometric,
`OVERVIEW_RESAMPLING=AVERAGE` and `OVERVIEWS=IGNORE_EXISTING` or `NONE`); GDAL also gets
`NUM_THREADS` from `--threads`. ZSTD is written at level 1 (`ZSTD_LEVEL`, `LEVEL` for GDAL's COG
driver): this build of terrio writes the levels above 4 at level 4. At level 1 the UInt16 files
match GDAL's in size, while terrio's Float32 predictor-3 files are about 3% larger. GDAL's COG driver takes no `PHOTOMETRIC`: it writes 3-band JPEG
as YCbCr itself, so RGB JPEG is measured as a tiled GeoTIFF only. The `cog_threads1` pattern sets
`RAYON_NUM_THREADS` and `GDAL_NUM_THREADS` to 1. `init` then includes loading the source.

A write is checked against GDAL's output, not by a checksum. GDAL writes a reference output once
per case, untimed; after each terrio sample GDAL reads terrio's output and the reference and
compares their grid, overview sizes, geotransform (absolute tolerance `1e-8`, relative `1e-12`),
CRS, band types, color interpretations, mask flags and nodata, then every pixel of every band and
overview level, NaNs equal. The case is `exact` when nothing differs, `within_tol` when only
pixels differ and none by more than the case's tolerance, and `DIFFER` otherwise; the worst
sample decides. The tolerance is 0 except for JPEG: 24, the largest difference measured between
the two writers' RGB JPEG of the fixture, where 1.3% of the values differ and both outputs have
the same mean error against the source. The row keeps the comparison (values compared, differing,
beyond the tolerance, largest and summed difference) and the two output sizes, shown as `out
KiB`. For writes, the bytes columns are the bytes each process read during the write (GDAL's COG
driver reads back its temporary overview file), and `need` is the source's uncompressed size.
Each output is deleted after its sample, and the run's output directory when it ends.

GeoParquet writes load a GeoParquet fixture (terrio's `open_vector`, a GDAL `MEM` copy) and write
it with `terrio.write_vector` and with `gdal.VectorTranslate` to the Parquet driver. terrio's
GeoParquet writer takes no options, so GDAL is given its layout: `GEOMETRY_ENCODING=WKB`,
`WRITE_COVERING_BBOX=YES`, `COMPRESSION=SNAPPY`, `ROW_GROUP_SIZE=65536` and `SORT_BY_BBOX=NO`.
GeoArrow, other compressions, row group sizes and sorting are therefore not measured. terrio's
output is compared with GDAL's reference as the benchmark compares vectors (field names and
types, CRS, every feature's attributes and normalized geometry, in any order), then by row and
row group counts, columns, compression, geometry encoding and covering. An output whose bytes
were already compared in the case is not compared again. The comparison runs in the sample's
process, within `--timeout`: comparing the 2,000,000 points takes about a minute on a desktop.
For this reason `/perf-matrix` with no format leaves `geoparquet-write` out: request it by name,
with a longer timeout (`/perf-matrix geoparquet-write --timeout 600`).

The GeoParquet `where` request is reported separately as `where/sql-vs-filter`: Terrio uses
`terrio.sql`, while GDAL uses `pyogrio.read_arrow(where=...)`. This compares complete filtering
workflows, including their different execution paths. Unsupported HTTP SQL inputs appear as
errors and are excluded from rankings.

FlatGeobuf reads use two Terrio entry points: `terrio.open_vector(bbox=...)` for the bbox
reads, since it searches the spatial index, and `terrio.open_stream` for whole and column
reads, since it selects columns. GDAL reads all of them with `pyogrio.read_arrow`, whose geometry
column, `wkb_geometry`, the values check compares as terrio's `geometry`. A FlatGeobuf stores each
feature whole, so a column read needs the same bytes as a whole read. The bytes a
bbox read needs are the header, the index nodes visited and the features found, or every feature
without an index. As for the other formats' chunks, a node or feature that several of the 16 boxes
touch counts once.

GeoPackage reads are `terrio.open_stream` against `pyogrio.read_arrow`, and the `where` request is
`terrio.sql` against `read_arrow(where=...)`, labelled `where/sql-vs-filter` too. The bytes a
GeoPackage read needs are the schema's pages and, for a bbox read through an R-tree, the nodes the
R-tree visits and the pages of the rows it gives; any other read needs the whole table, as the
fixtures have no attribute index and a row store reads a row's columns together. Over HTTP,
`terrio.open_stream` reads a GeoPackage's pages by range requests, as GDAL does, while `terrio.sql`
downloads the file first. GDAL's HTTP bbox reads of the larger GeoPackage fixtures take from about
110 s to over 300 s at the 20 ms round-trip time, so the default `--timeout` of 120 s reports
several of them as timeouts; three samples with `--timeout 300` take about 95 minutes.

Shapefile fixtures are GDAL translations of the vector sources, written with
`SPATIAL_INDEX=YES` (a `.qix` quadtree) or `NO` and `ENCODING=UTF-8` or `LATIN1` (a `.cpg`);
the LATIN1 fixture's `class` values with an `e` take an `é`, so their decoding is compared. Each
fixture is a directory holding one shapefile, since a shapefile is several files: a case's bytes
and HTTP requests include its `.shx`, `.dbf`, `.prj`, `.cpg` and `.qix`, and GDAL's probes for
sidecars that are not there. terrio reads a bbox with `terrio.open_vector(bbox=...)`, as
`terrio.open_stream` does not filter a local shapefile by bbox, and whole and column reads with
`terrio.open_stream`; GDAL reads all of them with `pyogrio.read_arrow`. A whole or column read
needs the `.shp`, `.dbf`, `.prj` and `.cpg` (a `.dbf` row stores its fields together). A bbox
read needs the `.prj`, the `.cpg`, the `.dbf` header and the rows of the features GDAL's spatial
filter finds, and either the whole `.shp` or, through a `.qix`, the quadtree nodes shapelib's
search reads and the `.shx` entries and `.shp` records of the shapes it gives. GDAL cannot
write `.sbn` indexes, so they are not measured.

The worker environment sets `GDAL_DISABLE_READDIR_ON_OPEN=EMPTY_DIR` to avoid directory scans,
`GDAL_HTTP_MERGE_CONSECUTIVE_RANGES=YES` to enable GDAL range merging, and `GDAL_CACHEMAX=512`
for its raster cache. These GDAL settings apply to GDAL; Terrio uses its own I/O and cache
policies. HTTP request and round counts therefore compare these configured workflows.

A local file is read from the OS page cache after its first read. `--source cold` adds a third
source, on request: the local path, dropped from the page cache before each sample of each tool,
without root. After its warm-up, the worker flushes each of the fixture's files (`fsync`) and
calls `posix_fadvise(POSIX_FADV_DONTNEED)` on it. It counts the files' pages in the page cache
with `mincore` before and after, and records them with the bytes the worker process read from
block devices during the open and read (`read_bytes` of `/proc/self/io`, `disk KiB` in the
report). That count includes any other file the process reads then, such as a library loaded on
first use, so it is an upper bound on the fixture's storage reads. A sample whose pages did not
all leave the page cache, for example pages another process maps, is reported as an error rather
than timed as cold, as is a sample that read nothing from a block device (network and FUSE file
systems). When `cold` is among the sources, the runner reads the
fixture into the page cache before each warm case, so a warm case never follows a cold one
uncached. The report compares each tool's cold time with its warm local time over
the cases read both ways. Cold reads need Linux, fixtures on a local file system that caches
in the page cache (ext4 or XFS, not tmpfs or ZFS, whose own cache `mincore` does not see), and
fixtures the user owns or can write: `mincore` reports other files' pages as cached.

The report ranks the worst cases, gives the geometric mean of the time ratios for each value of
each axis (by codec, layout, access pattern and source), and lists matching-checksum cases with
more request rounds than GDAL, bytes above 1.25 times GDAL plus 64 KiB, or bytes above 1.5 times
the estimated need plus 256 KiB. A slow axis value points to
one area of code (decoding, copying into the output, fetching, threading or opening), so one pull
request can fix all the cases of that area, with the matrix's numbers before and after.

### Operations

The `warp`, `terrain` and `vectorize` formats time raster operations rather than reads. They run
on the GeoTIFF fixture `f32_none`: a 4096 × 4096 Float32 DEM in UTM zone 31N (EPSG:32631, 10 m
pixels), uncompressed, as a COG with overviews; proximity runs on `u8_none`, its 8-bit
counterpart, and polygonize on a class map (below). Their files sit in the GeoTIFF fixture
directory, so `fixtures --format warp` makes the GeoTIFF fixtures (`fixtures --format vectorize`
also makes the class map). Each access pattern is one operation:

| Format | Operation | terrio | GDAL | Tolerance |
|---|---|---|---|---|
| `warp` | `near`, `bilinear`, `cubic`: to EPSG:4326 at GDAL's default size (4744 × 3367) | `terrio.warp` | `gdal.Warp` | near 0, others 1e-2 |
| `warp` | `bilinear_ts8192` (about 2x upsampled), `bilinear_ts1024` (about 4x downsampled, from an overview) | | | 1e-2 |
| `warp` | `average_ts1024` | | | 1e-2 |
| `terrain` | `slope`, `hillshade` | `terrio.dem`, GDAL 3.13 arithmetic | `gdal.DEMProcessing` | 0; hillshade 1 (Byte) |
| `terrain` | `aspect` | `terrio.dem`, GDAL 3.12 arithmetic | `gdal.DEMProcessing` | 1e-4 degrees |
| `terrain` | `proximity`: pixel distances to the 255s | `terrio.proximity` | `gdal.ComputeProximity` | 0 |
| `terrain` | `calc`: `sqrt(abs(A)) * 2 + A / 3` | `terrio.calc` | `gdal_calc.py`'s `Calc` | 0 |
| `vectorize` | `polygonize`, `polygonize8` (8-connected): polygons of the class map | `terrio.polygonize` | `gdal.Polygonize` | totals 1e-9 |
| `vectorize` | `contour`: lines every 200 m | `terrio.contour` | `gdal.ContourGenerateEx` | totals 1e-10 |

An operation receives the input's local path and returns its output in memory: a `terrio.Raster` or
`terrio.Layer`, or a GDAL `MEM` dataset. The time is that call, reading the input from the OS page
cache included. Before the timed call, the worker runs the same operation on the warm-up file, so
that one-time costs (CRS database, thread pools) are reported as `init`. Operations run from a local
path only. `--threads` sets both tools' threads: Rayon's, `GDAL_NUM_THREADS`, and `gdalwarp`'s
`NUM_THREADS` with `-multi`. Cases ending in `_t1` run with one thread in both tools.
With `GDAL_NUM_THREADS=4`, GDAL's terrain took more CPU and wall time than with 1 on the
development machine (hillshade about twice the wall time), so the `_t1` terrain cases are GDAL's
faster setting. GDAL's proximity and `gdal_calc.py` (NumPy) compute in one thread.

terrio's warp runs in its GDAL reproduction mode: `kernel_version="3.13"` and
`grid_invariant=False`. Its default grid-invariant sampling gives other values, so its time
would not compare equal work. The terrio aspect has no GDAL 3.13 arithmetic: the 3.12 arithmetic
differs from GDAL 3.13.3's by float32 rounding, one float32 step (3.05e-5 degrees between 256
and 360) at most on the fixture. GDAL 3.13's hillshade rounding depends on the CPU, hence one
level of tolerance.

After the timing, each terrio sample of `warp` and `terrain` computes GDAL's output in its own
process and compares the two outputs pixel by pixel, with the rules of
`scripts/compare_ops_with_gdal.py` except for nodata, where it is stricter (below). Shape, data
type, CRS, geotransform (to 1e-6 pixel) and nodata must match. A float nodata is compared as the
output's type holds it: `gdal_calc.py` records float32's largest value as `3.402823466e+38`.
Then every pixel counts, nodata included (that script compares only the pixels valid in GDAL's
output): equal values (NaN with NaN) agree, and any other pixel differs by its absolute
difference. A case is `exact` when no pixel differs and `within_tol` when every pixel is within
the case's tolerance. The report lists the within-tolerance cases with their differing pixel
count and maximum difference. Otherwise, the case is `DIFFER`, with the number of pixels
beyond the tolerance, and is not ranked. `average_ts1024` differs at pixels on the edge of the
output's footprint (about 0.1% of them). This is the area-weighting difference that the
`ops` conformance suite records for #703. The warp tolerances are those of
`scripts/compare_ops_with_gdal.py`: 0 for nearest, 1e-2 for the other resamplings.

The `vectorize` format turns rasters into features. Polygonize runs on `u8_classes`, a class map
that `fixtures --format vectorize` derives from the DEM: its elevations in 15 classes 200 m
apart, a Deflate COG whose noisy class edges make 645,832 polygons (317,596 8-connected).
Contour runs on `f32_none`, every 200 m (440,113 lines). The output is a `terrio.Layer` or a
GDAL vector `MEM` dataset, with GDAL's fields as `gdal_polygonize` (`DN`) and
`gdal_contour -a ELEV -i 200` (`ID`, `ELEV`) write them. Both tools trace in one thread
(each tool's read of the input may use `--threads`), so the format has no `_t1` cases. Each terrio
sample then computes GDAL's output in its own process. The comparison follows
`scripts/compare_vector_ops_with_gdal.py` (polygonize) and
`scripts/compare_analysis_bindings_with_gdal.py` (contour), with each feature matched by its
vertices in place of their rasterized round trip and Hausdorff distance: feature count and CRS
must match, and each of terrio's features must match one of GDAL's with the same value and the
same vertex sequence, bit for bit (through a 128-bit hash), in each ring or line. Where a ring
or a closed line starts, which way a ring or line runs and the order of a polygon's holes are
left out (a polygon's shell is still compared as its shell): the two tracers choose them
differently. A feature without a match differs. Each value's total area (polygons) or length
(lines) must then agree within 1e-9 or 1e-10 relative, the conformance scripts' relative
tolerances (without the contour script's absolute one); a single tiny ring's area or length,
summed from another start, can differ by more. A case is `exact` when every feature matches and
every total agrees, so `exact` means the same features, not the same start or direction. A total
beyond its tolerance makes the case `DIFFER`.

The `rasterize` and `zonal` formats time the two directions between features and pixels:

| Format | Operation | terrio | GDAL | Tolerance |
|---|---|---|---|---|
| `rasterize` | `burn`, `burn_all_touched`, `burn_t1`: 1s into a Byte raster | `terrio.rasterize` | `gdal.Rasterize` | 0 |
| `rasterize` | `attribute`, `attribute_all_touched`: the `value` field into a Float32 raster | | | 0 |
| `zonal` | `fields`, `fields_all_touched`, `fields_t1`, `blocks`: count, sum, mean, min and max of `f32_none` per zone | `terrio.zonal_stats` | `gdal raster zonal-stats` (`gdal.Run`) | 1e-9, relative |

`rasterize` burns each FlatGeobuf fixture with a spatial index, `poly_index` (400,000 polygons)
and `line_index` (400,000 lines), on 25 m pixels over the fixtures' 100 km extent (4000 × 4000).
Both tools start from 0 without nodata, and its pixels are compared as the other operations'.
`fixtures --format rasterize` makes the FlatGeobuf fixtures.

`zonal` summarizes `f32_none` over polygons that `fixtures --format zonal` writes beside the
GeoTIFF fixtures. `fields` has 20,000 irregular polygons with vertices 12 to 200 m from their
centre, clustered, some overlapping or crossing the raster's edge, with vertices rounded to the
centimetre as the vector fixtures' (a few fall on pixel edges). `blocks` has 16 squares of
1024 × 1024 pixels that tile the raster. GDAL's route is its zonal statistics command (GDAL 3.13.3) with
its default strategy (zone by zone); the pixel counts are GDAL's `default` (pixel centre in the
zone) and `all-touched`. Zones are matched by their `id`, and a statistic agrees when it is equal
or missing in both tools (null, or NaN: GDAL gives an empty zone's mean as NaN and its min and
max as null). Otherwise it differs by `|terrio - GDAL| / max(1, |GDAL|)`, infinite where one tool
has no value; the tolerance allows sums in another order. The results file records the largest
difference's zone, statistic and values (`worst`). The warm-up runs on its own small raster and
zone in the DEM's CRS, because terrio refuses zones with a CRS over a raster without one. GDAL's
rasterize and zonal statistics compute in one thread. The `fields` cases differ from GDAL in
known ways: terrio gives the sum of a zone holding no pixel as null and GDAL as 0, and with all
touched a few zones with a vertex on a pixel edge count one pixel more than GDAL's. Those cases
are therefore labeled `DIFFER`, with the count of values beyond the tolerance, and are not
ranked.

### Real data

The `geotiff-real` and `geoparquet-real` formats read public files, and COGs GDAL makes from one
of them, as the GeoTIFF and GeoParquet fixtures are read: from a local path and over the HTTP
server, with the same access patterns and values check. `fixtures` downloads each file once into
the format's fixture directory and keeps it only when its size and SHA-256 are the ones pinned in
the script (`DOWNLOADS`), as it checks a file already there; a file that changed upstream stops
the command. About 370 MB in all:

| Fixture | File | What it exercises |
|---|---|---|
| `s2_b04_nodata` | Sentinel-2 L2A band B04 of tile 31UER, 30 July 2024 (`sentinel-cogs` on AWS Open Data), 87 MB | a 10980 × 10980 UInt16 COG, DEFLATE with predictor 2, 1024-pixel tiles, nodata 0 over the 60% of the tile outside the swath |
| `s2_b04_sparse` | made from `s2_b04_nodata` by GDAL's COG driver with `SPARSE_OK=TRUE` and 512-pixel tiles | a sparse COG: the tiles holding only nodata are left out of the file (36 of the 64 tiles the 16 windows touch) |
| `s2_b04_mask` | made from `s2_b04_nodata` by GDAL's COG driver with `-mask mask,1 -a_nodata none` and 512-pixel tiles | an internal mask band in place of nodata, with its overviews |
| `dem_i16_big_endian` | USGS 30 m DEM `m30dem.tif` of the GeoTIFF samples on download.osgeo.org, 5.5 MB | big-endian Int16, uncompressed, one-row strips, 1479 × 1863 (not square), no nodata, no overviews |
| `buildings_est` | Estonia's buildings of the Google-Microsoft Open Buildings by VIDA (Source Cooperative), 101 MB | GeoParquet 1.1, 737,334 WKB polygons in WGS 84, all with `bf_source` "microsoft", Snappy, row groups of 5,000 rows clustered in space, a covering bbox column |

`s2_b04_sparse` and `s2_b04_mask` are derived, not downloaded: GDAL makes them in `fixtures`, so
their bytes depend on the GDAL version. Their source is pinned. The `masks` access reads every
band's mask: terrio's `read_masks` against GDAL's `GetMaskBand`, 255 for valid pixels and 0 for the
others. Its `need` is the mask band's blocks, the pixel blocks of a mask derived from nodata, or
nothing for an all-valid mask (the DEM). A non-square raster's windows lie in the square of its
shorter side; its whole and decimated reads cover all of it. The GeoParquet boxes are 1% of the
file's extent, the two columns are `area_in_meters` and `bf_source`, the filter is
`area_in_meters < 5.2` (1% of the rows; every row group holds smaller buildings, so statistics prune none), and the
sum checked is that of `area_in_meters`. `/perf-matrix` runs these formats only by name, so that a
run with no format downloads nothing.

```sh
# The same environment as above, with terrio's Python module built in release mode, pyogrio and
# PyArrow (GeoParquet) and zarr-python 3 (Zarr fixtures).
(cd python && maturin develop --release)
python scripts/perf_matrix.py fixtures --format geotiff      # 0.9 GB in ~/.cache/terrio-bench/matrix
python scripts/perf_matrix.py run --format geotiff           # every case; 50 to 100 minutes per format
python scripts/perf_matrix.py fixtures --format zarr
python scripts/perf_matrix.py run --format zarr --only shard --access window --quick
python scripts/perf_matrix.py run --format geotiff-write      # the GeoTIFF fixtures, written
python scripts/perf_matrix.py fixtures --format geoparquet-write
python scripts/perf_matrix.py run --format geoparquet-write   # the GeoParquet fixtures, written
python scripts/perf_matrix.py run --format warp              # operations, on the GeoTIFF fixtures
python scripts/perf_matrix.py run --format terrain --access slope slope_t1
python scripts/perf_matrix.py fixtures --format vectorize    # the GeoTIFF fixtures and the class map
python scripts/perf_matrix.py run --format vectorize --access contour

python scripts/perf_matrix.py fixtures --format rasterize    # the FlatGeobuf fixtures
python scripts/perf_matrix.py run --format rasterize
python scripts/perf_matrix.py fixtures --format zonal        # the GeoTIFF fixtures and the zones
python scripts/perf_matrix.py run --format zonal
python scripts/perf_matrix.py fixtures --format geotiff-real # 93 MB downloaded, 174 MB made from it
python scripts/perf_matrix.py run --format geotiff-real --quick
# `run` prints its results file path; report summarizes one again:
python scripts/perf_matrix.py report "$HOME/.cache/terrio-bench/matrix/geotiff/results-20261124-120000.jsonl"
# Run the matrix's regression tests:
python -m pytest -q scripts/tests/test_perf_matrix.py
```

Fixture names identify fixed specifications. Use an empty fixture directory after changing a
specification or fixture generation code, since existing files are reused by name.

The threaded reads, `threads1` to `threads8`, open one handle in each of N threads, which read
every Nth of the 16 windows or bboxes; opening is timed as part of the read. The report's thread
scaling section gives each tool's time with one thread divided by its time with N, per source, over
the cases measured with both. `--access threads4` selects one thread count, as before. These counts
are reading threads: the libraries' own pools stay at `--threads` for every access, so `threads1`
is one reading thread, not a single-threaded process; `--threads 1` measures that.

`--threads` sets `RAYON_NUM_THREADS` and `GDAL_NUM_THREADS` (4 by default), `--latency-ms` the
simulated round-trip time, `--samples` the samples per tool, and `--quick` takes one. A read that
takes longer than `--timeout` seconds (120) is reported as a timeout and not repeated. `run`
refuses to start on battery power (`--allow-battery` to override) and writes each run to a new
file whose first line describes the machine: CPU, CPUs and CPU quota, memory, OS, power, load
and library versions. Both ratios and absolute times depend on the machine: compare builds in
the same session. `report` warns when combined result files record different machines.

`--base DIR` times a second terrio build in the same run, each case alternating this build, the
base and GDAL: the report then lists the cases 10% or more slower or faster than the base. Install
the base's wheel with `pip install --no-deps --target DIR terrio-*.whl`.

`--gdal-allcpus` times GDAL a second time in each case, with its default `GDAL_CACHEMAX` and
`GDAL_NUM_THREADS=ALL_CPUS`, so the comparison does not rest on one GDAL tuning; the other settings
above stay. Each case then also gives terrio's time over GDAL's in that configuration, and the
report gives the geometric means of both ratios by source and access pattern, and the cases where
terrio is faster than GDAL in one configuration only. A failure in that configuration, or read
values that differ from GDAL's first configuration (for operations, a different pixel or feature
count), drops only its ratio; GDAL's outputs of operations and writes are otherwise not compared
between configurations. The operations and writes that run with one thread (`_t1`, `cog_threads1`)
set `GDAL_NUM_THREADS=1` in both configurations, so only the cache size differs there; the `threads1`
reads keep `ALL_CPUS`.

On a pull request, a maintainer's comment `/perf-matrix` runs the matrix on a GitHub runner
(`.github/workflows/perf-matrix.yml`): the pull request's terrio, its base branch's and GDAL
3.13.3, with the base branch's harness, and posts the report. With no arguments it runs every
format but `geoparquet-write`, `geotiff-real` and `geoparquet-real` with one sample per tool;
`/perf-matrix geoparquet --only geoarrow` runs one format with the options of `run`; options
alone (`/perf-matrix --samples 5`) run the read formats except the real-data ones;
`/perf-matrix warp`, `/perf-matrix terrain`, `/perf-matrix rasterize`, `/perf-matrix zonal` and
`/perf-matrix vectorize` time the operations.
Runner timings vary more than a quiet desktop's, but each comparison is made within one run on
one runner. The Actions tab runs the same workflow for a pull request
number. Each format runs in its own job, in parallel.

Run this workflow only on code you would build yourself. Pull request build scripts share the
measurement job's workspace and environment, so they can influence the harness, fixtures,
base build and GDAL reference as well as their own build. The posted measurements are not an
independent security check of pull request code.

`perf_matrix.py touched` reads changed file paths on standard input and prints the formats whose
reads or writes go through them (`PATHS` in the script; shared I/O files and `Cargo.lock` count
for every format).

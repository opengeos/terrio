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
3.13.3 build, not a requested approximate-statistics mode or a stored-statistics cache. Its internal
cause remains unisolated; the invalid eight-thread result is excluded from speed comparisons.

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

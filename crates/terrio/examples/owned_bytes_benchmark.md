# Owned byte opening allocation benchmark

Run from the repository root with Rust 1.90 or newer:

```powershell
$env:CARGO_PROFILE_DEV_DEBUG='0'
$env:CARGO_BUILD_JOBS='2'
$env:CARGO_TARGET_DIR='C:/path/to/dedicated/disk/target'
cargo build -p terrio --example owned_bytes_peak --no-default-features --features netcdf-hdf5
foreach ($format in 'classic','hdf5','hdf5-userblock') {
  foreach ($mode in 'old','owned') {
    & "$env:CARGO_TARGET_DIR/debug/examples/owned_bytes_peak.exe" $format $mode 64
  }
}
```

The executable synthesizes a valid 64 MiB Int16 CDF-1 array with a repeating
-50..50 pattern. The HDF5 case converts that array with Terrio's netCDF-4 writer.
Every decoded value is checked after dropping the dataset, outside measurement.
No downloaded data or non-Rust fixture generator is required.

A forwarding System global allocator measures requested live Rust heap bytes,
including reallocations, with atomic counters. Fixture construction is excluded;
the source Vec stays live in both cases. The measured expressions are exactly
`open_bytes(&bytes.to_vec())` and `open_bytes_owned(bytes.to_vec())`, including
the simulated Uint8Array-to-Vec copy. Reported peak_delta subtracts the baseline
(source and process bookkeeping); retained_delta is sampled immediately after
opening. Run each combination as a fresh process, preferably three times.

This is not RSS, allocator usable size, cumulative allocation traffic, or a
browser memory measurement. It excludes JS memory, allocator fragmentation and
native allocations bypassing Rust's global allocator. Reallocation measures
logical requested live sizes, not transient allocator implementation copies.
With hdf5-reader 0.12.1, BytesStorage retains the Vec without copying it. The
JavaScript-to-Vec copy and decoded-array allocations remain.
Correctness coverage for real producer userblocks and malformed inputs lives in
`multidim::netcdf::tests::owned_*`; the optional hdf5-userblock case prepends a synthetic 512-byte userblock.

## Measured on 2026-11-02

Historical results with the older reader dependency, before its Vec retention
improvement. These figures do not describe the current 0.12.1 dependency.

Windows 11 Pro for Workstations 10.0.26300, x86_64 MSVC, rustc 1.99.1
(58cf8f9c3), dev profile with debug=0, two Cargo jobs; no dependency changes.
Input parameter 64 MiB, three fresh processes per format/mode. Opening peaks
exclude subsequent full-array JSON validation. Every sample is checked exactly.

| Format | Input bytes | Old peak delta | Owned peak delta | Retained delta (both) |
|---|---:|---:|---:|---:|
| Classic CDF-1 | 67,108,944 | 201,326,848 | 134,217,904 | 67,111,099 |
| netCDF-4/HDF5 | 201,331,605 | 603,994,834 | 402,663,229 | 201,356,999 |
| HDF5 + 512-byte userblock | 201,332,117 | 603,995,346 | 402,664,253 | 201,356,999 / 201,357,551 |

The HDF5 serialization adds dimension-coordinate storage: its measured input is
about 192 MiB despite the 64 MiB sample array. Without a userblock, savings equal one complete input buffer (about one third
of the old opening peak), with retained storage unchanged. The userblock case
saves 201,331,093 peak bytes; the owned path keeps the 512-byte userblock and
an offset-storage wrapper, retaining 552 more bytes than the borrowed path,
which discards the prefix. That row lists old / owned retained bytes.
These are allocation results for a native surrogate of the binding expressions;
no throughput, RSS, actual browser peak, or zero-copy claim follows from them.

Raw counter results (identical in all three trials per row; bytes):

```csv
format,mode,input,baseline,peak,peak_delta,retained_delta
classic,old,67108944,67109136,268435984,201326848,67111099
classic,owned,67108944,67109138,201327042,134217904,67111099
hdf5,old,201331605,201331794,805326628,603994834,201356999
hdf5,owned,201331605,201331796,603995025,402663229,201356999
hdf5-userblock,old,201332117,201332316,805327662,603995346,201356999
hdf5-userblock,owned,201332117,201332318,603996571,402664253,201357551
```

## Rechecked on 2026-11-05 with readers 0.12.1

Windows x86_64 GNU, rustc 1.99.1, debug symbols disabled, 8 MiB input
parameter. One fresh process per format/mode; every decoded value was checked.
These are requested Rust heap bytes, not browser memory or RSS. The dependency
now retains a Vec without copying; the old binding expression still makes an
extra input clone. Larger inputs and repeated timing runs were not measured.

```csv
format,mode,input,peak_delta,retained_delta
classic,old,8388688,16779912,8390875
classic,owned,8388688,8391224,8390875
hdf5,old,25170836,50404138,25202169
hdf5,owned,25170836,25233302,25202169
hdf5-userblock,old,25171348,50404650,25202169
hdf5-userblock,owned,25171348,25233854,25202721
```

The owned path removes one payload-sized clone. Keeping the userblock also
retains its 512 bytes and the offset wrapper. No throughput claim follows.

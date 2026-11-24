# TIFF LZW writer performance

The TIFF and COG writer uses an independent TIFF LZW encoder that preserves the
compressed bytes of weezl 0.2.1. It reuses dictionary and optional long-match
witness storage within each Rayon work piece. Each chunk starts an independent
stream; no dictionary codes cross chunk boundaries. The pool helper preserves
ordered output and the existing serial fallback for overlapping external callers.

The implementation and byte-compatibility rules are described in
[lzw-encoder.md](lzw-encoder.md). Adaptive long-match selection and its limitations
are described in [tiff-lzw-long-match.md](tiff-lzw-long-match.md). Retained encoder
heap storage is 84 KiB on 64-bit targets and 68 KiB on 32-bit targets after a chunk uses
witnesses. Before witnesses are needed, the encoder uses 36 KiB of heap storage.
The writer's memory estimate includes that storage plus a conservative 512 KiB
allowance for the byte-compatible weezl fallback on collision-heavy chunks.

## Reproduce the synthetic writer workload

First copy the candidate's unchanged
`crates/terrio/examples/lzw_write_benchmark.rs` into the same path in the baseline
checkout. Cargo discovers this example automatically. This preparation adds only
the workload; retain each checkout's own library sources and Cargo manifest.

Then build the example separately in the baseline and candidate checkouts:

```sh
cargo build -p terrio --release --no-default-features --features geotiff \
  --example lzw_write_benchmark
RAYON_NUM_THREADS=3 target/release/examples/lzw_write_benchmark output.tif
RAYON_NUM_THREADS=3 target/release/examples/lzw_write_benchmark dense.tif dense
```

The example writes a deterministic 16384 by 2048 Float32 TIFF with one-row strips.
The default has many NoData rows; the `dense` argument retains all generated
samples. It requires no external dataset or GDAL. These synthetic values have no
assigned engineering units or coordinate system.

Build before timing. Warm both executables, interleave at least seven measured
pairs on the same machine, record process CPU, wall time and peak memory, and
compare complete output bytes after every run. Repeat with one and three threads.
Report machine, compiler, source revisions and settings with the results; these
measurements do not establish a speed-up for every input or platform.

`lzw_codec_profile` is a separate **Weezl-only reuse diagnostic**. It measures
fresh versus reset Weezl encoders and can count allocation requests with `--alloc`.
It does not measure the specialized encoder, and its instrumented timing is not
writer acceptance evidence. See the [example guide](../crates/terrio/examples/lzw_write_benchmark.md).

Contributor measurements and their limitations are recorded in
[the original contribution, #1285](https://github.com/opengeos/terrio/pull/1285).
The retained tests check weezl byte identity and complete TIFF/COG fixtures across
thread counts and concurrent writes. A chunk-wide collision budget bounds work
in the fixed dictionary hash and restarts excessive-probe chunks with weezl;
see [lzw-encoder.md](lzw-encoder.md).

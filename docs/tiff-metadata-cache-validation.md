# TIFF metadata cache validation

Local review on 2026-10-28 of [issue #348](https://github.com/opengeos/terrio/issues/348).
Origin's nine new commits were integrated by fast-forward to `9ca108f` on
`codex/tiff-metadata-cache`. The saved implementation reapplied without conflicts.
No commits or pushes were made.

## Review

No correctness or API compatibility defect was found in the caching change.
Each level owns its tables and codec decision; external overviews use their
own reader's levels. Tables are immutable and shared with codecs through `Arc`.
Shared lazy handles serialize access to the decoder, while chunk workers borrow
the codec within a scoped thread lifetime. Failed metadata initialization is
retried, and the reader's decoding limits remain fixed for its lifetime.

Two additional regressions exercise IFDs with different codecs/strip geometry
and concurrent shared readers switching levels, with pixels, masks, nodata,
type and georeferencing checked. Clippy warnings in the new test file were fixed.
The production implementation was retained.

## Matched release benchmarks

The baseline was rebuilt from an archive of upstream `9ca108f`; the other build
used the working implementation. Both standalone harness builds have identical
dependency lockfiles and release settings. The original baseline binaries were
not used for these comparisons, because upstream updated dependencies.

Environment: native aarch64 macOS, Rust 1.99.1. The same existing harness and
fixtures were used: 15 TIFFs, DEFLATE/LZW, classic/BigTIFF, masks, two overviews,
256–8192 pixels per side and 16-pixel tiles (256–262,144 full-resolution chunks).
One persistent reader serves each sequence; opening and first reads are separate.
The matrix covers repeated/varying positions, one/many chunks, level switching
and masks, with one/four workers. Each build ran three 50 ms samples per case in
each of two rounds, reversing build order; the table uses the median of six
samples. Allocation and filesystem counters were enabled in both builds.
These instrumented local timings are observations, not performance guarantees.

| Workload, 8192² TIFF unless stated | Workers | Before µs/read | After µs/read | Speedup |
| --- | ---: | ---: | ---: | ---: |
| DEFLATE, repeated 16×16 | 1 | 950.57 | 3.73 | 254.9× |
| LZW, repeated 16×16 | 1 | 1885.66 | 3.01 | 627.4× |
| LZW, repeated unaligned 40×40 | 1 | 1922.69 | 30.95 | 62.1× |
| LZW, repeated 512×512 | 1 | 5049.84 | 3053.53 | 1.65× |
| LZW, repeated unaligned 40×40 | 4 | 3317.61 | 1501.01 | 2.21× |
| Internal mask, 4096² TIFF, repeated 40×40 | 1 | 31.73 | 18.51 | 1.71× |

All 304 build-paired case groups have identical pixel/mask hashes. The 512×512
cases ranged from 0.99–1.65× with one worker and 0.95–1.32× with four workers.
One small four-worker case initially appeared 13% slower; a longer alternating
recheck (ten 500 ms samples per build) measured 126.04 → 122.04 µs/read, so that
slowdown did not reproduce. No substantial repeatable regression was found.

For repeated 16×16 reads of the 262,144-tile TIFFs:

| Metric per read | DEFLATE before → after | LZW before → after |
| --- | ---: | ---: |
| Allocated bytes | 21,053,228 → 81,636 | 42,002,500 → 59,348 |
| Filesystem bytes read | 2,129,920 → 32,768 | 4,202,496 → 8,192 |
| Files opened | 0 → 0 | 0 → 0 |

Touching all three levels retains an additional 5.25 MiB: 16 bytes per cached
chunk plus small container overhead. For the large DEFLATE fixture, the two
opening observations had medians of 3.88/4.10 ms before/after; first full-level
reads were about 1.02 ms in both builds. These opening samples are too few to
establish a systematic difference.

A separate localhost HTTP range benchmark, with a 256 KiB client cache and
varying 16×16 windows, reduced mean response-body bytes/read from approximately
2.27 MB to 0.095 MB (DEFLATE), and 4.46 MB to 0.065 MB (LZW). Worker decoder
initialization and the TIFF crate's parsing when changing levels remain the
explicit follow-up scope of #348; four-worker multi-chunk reads still open four
decoders per window.

## Validation

- `cargo fmt --all -- --check`: passed.
- `cargo clippy --workspace --all-targets --locked --offline -- -D warnings`: passed.
- `cargo test --workspace --locked --offline`: 1,514 passed, 16 ignored. The first
  sandbox run could not bind localhost test servers; the unrestricted rerun passed.
- All seven `tiff_chunk_tables` regressions passed, including allocation and
  metadata-only eviction checks. Four additional real-data lazy-reader tests passed.
- `compare_windows_with_gdal.py local`: 96 windows across nine real GeoTIFFs and
  their overviews matched GDAL 3.13.2, including georeferencing.
- The benchmark's 150 unique pixel/full-resolution-mask sequences matched GDAL;
  its two overview-mask sequences matched the documented sampling policy below.
- Optimized Python wheel built and installed. Python 3.14.7 pytest: 3,330 passed,
  380 skipped, exit 0. The full run emitted warnings and six shutdown
  `sys.excepthook` diagnostics without traceback details. Focused binding checks
  exited cleanly: 640 pixel windows matched GDAL, and 160 mask checks passed.
- Optimized `terrio-js` WASM build and wasm-bindgen 0.2.129 generation passed.
  Node 24.22.1: 72 passed, 65 skipped. A further 96 repeated WASM window/overview
  reads matched GDAL exactly.
- Pre-commit on every changed file passed, including its formatting and Clippy hooks.
- The complete downloaded-data conformance matrix, public-cloud tests and
  browser/threaded-WASM variants were not run.

Internal overview masks retain the documented existing policy of sampling the
full-resolution mask. They are checked against that policy, rather than claimed
identical to GDAL's overview mask; see [rasterio limitations](rasterio.md#not-covered).

The local backup, matched harness sources/binaries, raw JSON results and logs are
under `/private/tmp/terrio-tiff-review-20261028/`. The original benchmark fixtures
remain in Claude's scratchpad; neither build outputs nor downloaded data are
included in the working diff.

# Terrio LZW write-path prototype — local result

Encoder reuse improves the synthetic writer, but **does not improve the frozen large case**.
The requested no-slower-than-Go goal is not demonstrated. Keep this as an experimental
prototype for review; no adoption, push, PR, CI dispatch or fleet execution was performed.

## Source and ownership

- Worktree: `D:/CLB-Dev/wt/terrio-lzw-perf`; branch `perf/lzw-write-path`.
- Base: upstream v0.8.0, `7f93ff1712b34e02a147753bd700364861dfed8e`.
- Code commit: `d828510854e282cabfb1803457b71dd31411354a`.
- Regression/workload commit: `bb9e3ca5cb296c4b65f705a347a6aab2089fd845`.
- Frozen flows2fim source: `6087b5b49edc7623c040c4514ca4cba337beabf0`, recovered
  from the retained archive and verified SHA256
  `0f7465013eacba076a5605f826f17976dc6b0aa3891e12fc28fbb3d625631356`.
  Scratch: `D:/CLB-Dev/wt/flows2fim-terrio-lzw-frozen`. A `[patch.crates-io]`
  path dependency exists only in this scratch copy; nothing was committed to flows2fim.
- The original H checkout predates `compose`. Its separately retained integration
  build is not the frozen benchmark executable. Shared H checkouts were read only.
- The path dependency also selects local terrio-crs 0.2.3; its entire `src` tree
  matches the registry source after CRLF normalization (`crs-source-comparison.json`).
  No dependencies were added or upgraded in Terrio.

## Change and profile evidence

`par::map_range_init_coarse` keeps the original coarse pool selection, fork/WASM
rules and ordered collection, with scratch owned per Rayon work piece. `Level::image`
lazily constructs a TIFF/MSB/8 Weezl encoder, resets it before every independent
strip/tile and retains dictionary capacity until that work piece ends. This is
not one persistent encoder per OS thread, and dictionary codes never cross chunks.
Output Vec ownership, chunk order/error order, dimensions, raw serialization,
predictors, mask encoding and other codecs are unchanged. There is no public API change.

WPR CPU recording failed with policy error `0xc5585011`; no policy was changed.
A private stdlib-only Python sampler used Win64 DbgHelp/StackWalk64 and local PDBs
on its own child process, briefly suspending/resuming threads. Synthetic: 11
samples, 52/54 unique addresses symbolized. Correct frozen case: 639 samples,
185/189 addresses symbolized, with output still exactly `c50acda6...`.
Observed frozen endpoint stacks: 243 LZW encoder, 5 serialization, 4 allocation,
21 other/inline/unknown, 366 wait/endpoint alias. Weezl dictionary traversal
(`Tree::iterate`) is a recurring active leaf. No named source-decode or predictor
frames were captured. **These counts are not CPU-time percentages**: the sampler
perturbs scheduling, CPU deltas are coarse and endpoint waits can follow active
work; inlining and kernel/private OS frames limit attribution. Exact decode,
serialization, predictor and allocation CPU fractions remain unresolved.

A separate bounded diagnostic extracts v0.8.0's f32 `chunk_bytes` and predictor
functions verbatim (54431 x 512 synthetic strips). Predictor 1 elapsed phase sums:
62.155 ms serialization, 0.0227 ms predictor, 858.755 ms compression; predictor 3:
64.096 / 102.3681 / 917.2846 ms. These overlapped builds, exclude decode and are
not acceptance timings or full-writer CPU shares.

The codec allocation diagnostic (4096 independent 54431-sample strips) counted:

| | Fresh encoder | Retained/reset encoder |
|---|---:|---:|
| Allocation/reallocation requests | 102400 | 20500 |
| Sum of requested bytes | 1549877248 | 520345108 |

These include fresh compressed output allocations and are not peak memory.
Construction itself was small; table allocations also occur during encoding.
The final example was rerun after its Clippy-only loop-style fix and produced
identical allocation counts. Atomic-instrumented times are not speedup evidence.

Raw/predictor buffer reuse was not implemented: frozen predictor 1 is a no-op,
and the available evidence does not establish serialization as the large-case
bottleneck. Compression was already parallel; no new parallelism or grain tuning
was added. Allocation reuse is the one isolated candidate measured here.

## Paired measurements

Windows x86_64 MSVC, Rust 1.99.1, i7-11700K, 8 cores/8 logical processors. Release
builds with debug symbols; frozen flows2fim retains thin LTO/codegen-units=1.
Three Terrio threads, warm file cache, fresh process each run, seeded randomized
arm order, one warm-up per arm plus seven measured pairs **per workload**. Our
builds/tests had drained before timing; this was a shared desktop, not a reserved
fleet node. No affinity pin or cache dropping. No Go/GDAL timing arm was run.

Wall includes process creation. CPU is child-process user+kernel time from
GetProcessTimes; memory is process PeakWorkingSetSize, not Linux RSS or job memory.
The parent hashes output files outside each timed child. Executable/source/lock
identities and all per-run measurements are retained in the evidence bundle.

Medians; baseline v0.8.0 -> candidate:

| Workload | Wall seconds | CPU seconds | Median process peak WS, MiB |
|---|---:|---:|---:|
| synthetic | 1.156 -> 1.009 | 1.297 -> 1.188 | 261.08 -> 261.08 |
| frozen | 26.408 -> 27.343 | 35.266 -> 35.281 | 166.05 -> 166.67 |

Synthetic wall -12.7%, CPU -8.4%; candidate faster in 6/7 wall pairs. Frozen wall
+3.5%, CPU effectively flat (0.016 s between medians); candidate
faster in only 3/7 wall pairs. Frozen paired median candidate-minus-baseline:
+0.400 s wall, +0.188 s CPU. Frozen wall ranges: baseline 24.543–34.347 s,
candidate 24.674–32.992 s. These noisy samples show no large-case speed win.
Maximum measured peak WS: synthetic 261.1 MiB both; frozen baseline 168.5 MiB,
candidate 170.0 MiB. Do not extrapolate Windows results to the retained Linux
runtime (which also uses a different allocator/runtime).

Every warm-up and measured output matched its required full-file SHA256:

- Synthetic: `34566a5261db61420546cde5e26ae7f275ec2ec228d9cbf3cda19386591546bd`,
  27042318 bytes, all 16 files equal.
- Frozen: `c50acda66d143167e568dc2efb6ea324297cbcb4da81896028b2816d472080b7`,
  157402596 bytes, all 16 files equal. Candidate also matched at one thread.
- All 128 staged input hashes were rechecked after measurement and remain equal
  to the frozen manifest.

Staging correction: the manifest member order initially differed from the drawing
order. The original ordered list was recovered and verified as
`d942f581692e2925c83288faa85ae7cb448fb84586fd8ed5e13b2593aac7f6a4`.
Only its root prefix was translated. The earlier manifest-order profile/hash
`7b77c6a7...` is retained as a discarded diagnostic; **all acceptance timings use
the corrected frozen order** and the original `c50acda6...` gate.

Both Python benchmark harnesses completed successfully with 16 records each.
The enclosing shell returned 1 because its final Get-Process inventory was empty;
this was after both successful harness exits and does not indicate a benchmark
failure. Empty inventory files were normalized to JSON `[]`.

## Tests and checks

- `cargo test -p terrio`: default-feature unit, integration and doctests passed;
  zero failures, 53 ignored by the existing suite. Logs sum 2881 passes including
  nested child-process summaries; this is not a unique-test count.
- New regression: 48 baseline full TIFF/COG files (483782 bytes total), covering
  strips, padded tiles, planar/pixel layout, predictors 1/2/3, classic/BigTIFF,
  NaN payloads, signed zero and infinities. Exact file bytes at 1 and 3 threads,
  concurrent writers, TIFF early/late width-switch distinction, independent LZW
  stream round trips through dictionary saturation/clears and ordered range results.
- `cargo check -p terrio --no-default-features --features geotiff --target wasm32-unknown-unknown` passed.
  This is compilation of the serial WASM fallback, not a browser runtime test.
- `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`
  and complete changed-file pre-commit checks and diff checks passed.
- `cargo test -p flows2fim-core -p flows2fim-raster` against the frozen source and
  path-patched candidate passed, with `FLOWS2FIM_NOAA_TESTDATA` pointing to the real
  retained reference directory. Includes the three raster NOAA oracle tests.
- `cargo test -p flows2fim-cli --test noaa_parity -- --nocapture`: 7 passed;
  18 outputs (six return periods x VRT/GTiff/COG) match stored reference rasters,
  and 400 synthetic control cases ran through Rust. `FLOWS2FIM_GO_ORACLE` was unset,
  so live Go comparisons were skipped. No GDAL was installed or invoked.
- The initial complete GeoTIFF-only test command failed to compile 25 pre-existing
  optional-driver test references. The default-feature complete package suite
  above passed; this unrelated test-feature issue was not patched.

The default-suite library/test binaries were compiled before a loop-style-only fix in the separate codec
profiling example. Production/library/test code did not change afterward; final
workspace Clippy/pre-commit and the allocation example passed after that fix.
No big-endian hardware, threaded-WASM runtime, external-data/GDAL conformance or
Linux CI/fleet run was performed.

## Dependency review and remaining fleet work

The dependency-freshness-review skill was applied before commits. Weezl 0.2.1 and
Rayon 1.12.0 are resolved and were the latest stable releases returned by their
official registries on 2026-11-20. Versions were retained to preserve this experiment.
Rayon 1.12.0 release notes (2026-04-13) do not change this use. Weezl's GitHub release
list is empty; its pinned reset source/API was inspected. A bounded RustSec search
found no relevant advisory; this is not a whole-graph security audit.
Sources: https://crates.io/api/v1/crates/weezl,
https://crates.io/api/v1/crates/rayon,
https://github.com/rayon-rs/rayon/blob/main/RELEASES.md,
https://github.com/image-rs/weezl.

Before adoption, review this negative large-case result. If a fleet rerun is chosen,
build a distinct candidate from the reviewed code against frozen flows2fim 6087b5b,
retain compiler/dependency and Linux allocator/library identities, and leave 71007
unchanged. Verify the original 128-member drawing order, all input hashes and the
full Rust `c50acda6...` gate plus the retained GDAL decoded comparator. Then repeat
the original Go-vs-candidate paired workload with three CPUs, 18 GiB, GTiff LZW,
GDAL_CACHEMAX=2048 and Rust memory=2147483648: warm-up plus at least seven pairs,
recording wall, CPU, memory, output size and quiet-node evidence. A contemporaneous
unmodified Rust control would help isolate runtime drift. No 1.5x bar is imposed here.

This candidate has not established Go equivalence, COG performance or end-to-end
qualification. The observed active encoder dictionary lookup path is a better
next profiling target than assuming constructor allocation dominates encoding.

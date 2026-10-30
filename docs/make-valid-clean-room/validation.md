# Validation report — independent tile repair kernel

Date 2026-10-30. Reference runtime: Shapely 2.2.0, GEOS 3.14.1
(`3.14.1-CAPI-1.20.5`, libgeos sha256 bbe7a60a…912a, libgeos_c 2166c0a8…ec93),
NumPy 2.5.4, Python 3.13 (sandbox), Linux x86-64. Production encoder
`reference/tile-encoder` sha256 da04be3b…4af. Rust 1.99.0 stable, edition 2024,
MSRV field 1.90. Host CPU for timings: 1 visible core, i7-6700 @ 3.40 GHz;
`CARGO_BUILD_JOBS=2`, target `/build`. All commands were run directly inside
the sandbox (per RUN-ENVIRONMENT.md), not through tools/isolate.py.

## Summary

| Gate | Command | Result |
| --- | --- | --- |
| Format | `cargo fmt --all -- --check` | **pass** |
| Strict Clippy | `cargo clippy --workspace --all-targets --locked --offline -- -D warnings` | **pass** (0 warnings) |
| Native tests | `cargo test --locked --offline` | **pass**: 4 unit + 20 integration tests |
| Native build | `cargo build --locked --offline` | **pass** |
| WASM library | `cargo build --locked --offline --lib --target wasm32-unknown-unknown` | **pass** |
| Fixture replay, Layer A+B | `python3 tools/replay.py` | **pass**: 51/51 kernel; 50/50 tile-adapter; `wide-i32-bowtie` kernel pass + encoder-domain exclusion (as specified) |
| Harness self-checks | `tools/replay.py --reference-self-check`, `tools/check_harness.py` | **pass** |
| Immutable references | `python3 tools/verify_package.py` | **pass** (hashes unchanged) |
| Scaffold prep check | `tools/check_scaffold.py` | fails, **as intended** once implemented |
| Generated differential | `candidate/tests/oracle/generated_oracle.py` (3 seeds, 4,500 cases) | **0 mismatches**, 0 oracle failures, 0 candidate failures; 8 reported candidate-only encoder overflows (below) |
| Stress, direct API | `cargo build --release …; /build/release/examples/stress <case>` | completed; numbers below |
| Stress vs oracle (optional) | `candidate/tests/oracle/stress_oracle.py` | **incomplete — not a pass** (stopped by custodian) |

## Layer A/B: fixture replay (immutable `tools/replay.py`)

`candidate-acceptance`, 51 cases: 50 × `pass` (validity, emptiness, hole count,
exact `equals` for integral fixtures, area ratio ≤ 1e-10, boundary Hausdorff ≤
max(1e-10, 8 ulp(M)), then identical encoder presence and exact integer tile
geometry) and 1 × `kernel-pass/encoder-domain-excluded` (`wide-i32-bowtie`,
reference encoder overflow as documented in PROTOCOL.md). Failures: none.
Native test `fixture_exact_coordinates_for_fractional_crossings` additionally
checks that for every invalid fixture each output vertex is bit-identical to a
vertex in the oracle output (including 42/13, 30/13, −0.5 and the near-`i32::MAX`
crossings).

## Layer C: generated differential cases (original generator)

Generator: `candidate/tests/oracle/generated_oracle.py`, Python `random.Random(seed)`,
families random-rings, rectangles, fractional, near-integer, star, degenerate,
fan, nested, far-offset, dense, wide-i32, collinear, many-parts (weights in the
file). Expected results are computed live from the oracle; nothing is stored.
Each case goes through the immutable `replay.check_case` comparator.

| Seed | Cases | Invalid inputs | pass | pass-structural (invalid reference tile) | kernel-pass, reference encoder overflow | kernel-pass, **candidate-only** encoder overflow | mismatch | oracle failure | candidate failure |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 20261030 | 2000 | 1873 | 1857 | 71 | 69 | 3 | 0 | 0 | 0 |
| 7 | 1000 | 921 | 923 | 36 | 38 | 3 | 0 | 0 | 0 |
| 99 | 1500 | 1380 | 1407 | 38 | 53 | 2 | 0 | 0 | 0 |
| **Total** | **4500** | **4174** | **4187** | **145** | **160** | **8** | **0** | **0** | **0** |

Reproduce: `python3 candidate/tests/oracle/generated_oracle.py --seed S --count N`
(≈ 7–8 ms per case; exit status nonzero on any mismatch/failure).

### Explanation of the non-"pass" categories (not hidden as passes)

- **pass-structural / invalid reference tile (145).** The kernel comparison
  passed, but the immutable comparator's final `MultiPolygon.equals` on the
  *encoded integer tiles* returned false. In every such case the reference's own
  encoded tile is an **invalid** geometry (overlapping/retraced slivers produced
  by truncation), and GEOS `equals` on it is order-dependent: for 5 of the first
  9 analysed cases the reference tile was not `equals` to itself with polygon
  order reversed. Reproducing that would require GEOS's private component/ring
  enumeration order, which SPEC MV-04 excludes. For these cases only, the
  harness applies a **stricter exact** check: identical presence and identical
  multisets of encoded polygons (each shell and its holes as integer rings,
  normalised only by start rotation). All 145 are exactly identical. The
  immutable replay itself is unchanged and all 51 fixtures pass it directly.
  (During development a harness bug — canonicalising ring starts at the first
  copy of a repeated vertex — produced 6 false mismatches; fixed by using the
  lexicographically smallest rotation.)
- **kernel-pass, reference encoder overflow (160).** `wide-i32` family inputs
  whose oracle output already overflows the encoder's `i32` deltas — the same
  documented domain limitation as `wide-i32-bowtie`. Kernel comparison passed.
- **kernel-pass, candidate-only encoder overflow (8) — a reported difference.**
  Full-range inputs (spans ≈ 2^32) where the oracle's output encodes but the
  candidate's output makes the encoder overflow. The geometry passed Layer A;
  the difference is only ring start/component order, which changes the
  cursor-move delta between rings (candidate rings start at their
  lexicographically smallest vertex). Inputs (seed, id): 20261030
  wide-i32-600/830/1244; 7 wide-i32-25/301/807; 99 wide-i32-646/1313 (full inputs
  are regenerated by the seeds). Real tile coordinates are far inside this
  domain, but the candidate does not guarantee encodability whenever GEOS's order
  happens to encode. Not fixed; for custodian review.

## Layer C: native adversarial/property tests (`candidate/tests/properties.rs`)

Labelled in the file: [O] GEOS-observed, [P] defensive policy, [Inv] invariants.
- [O] fixture polygon/hole counts and areas for all 51; bit-exact vertex sets
  for invalid fixtures.
- [P] empty outer/polygon/ring inputs; empty exterior ignores holes (MV-05);
  unclosed rings closed logically; 1-point, 2-point, retraced and collinear rings
  never fill; consecutive duplicates ignored; full `i32` square/bowtie and
  nearly-parallel full-range edges; 60-deep nesting (as components and as holes);
  900 disconnected parts; 64-triangle point fan and 32-triangle shared-edge fan;
  partially coincident segments; near-integer crossing rounded exactly.
- [Inv] determinism (bitwise) and non-mutation (300 random); invariance under
  ring rotation, reversal and component permutation (400 random, exact output
  equality); idempotence of integral outputs (600 inputs, >50 integral checked);
  integer translation moves output within rounding tolerance (200 random).
- Structural checks on every output: closed, finite, ≥3 distinct vertices, no
  consecutive duplicates, shells clockwise / holes counter-clockwise.
- Unit tests: 256-bit product comparison, canonical points, correctly rounded
  rational → binary64 (incl. ties-to-even and 20,000 cross-checks against IEEE
  division).

## Stress (release build, direct Rust API, one process per case)

| Case | Input | Arrangement (edges / vertices) | Iterations | Output | Wall time | Peak RSS |
| --- | --- | --- | --- | --- | --- | --- |
| sparse-ring: 200k-vertex circle, one twist | 200,003 vertices | 200,004 / 200,003 | 2 | 1 polygon, 1 hole (pinched bay), 200,006 vertices | 0.33 s | 66 MiB |
| many-parts: 50k squares + cover | 250,005 vertices | 200,750 / 200,301 | 3 | 44,375 polygons, 5,476 holes | 0.45 s | 71 MiB |
| dense-random-ring-600 (4096 box) | 601 vertices | 80,852 / 40,726 | 134 | 20,134 polygons | 0.92 s | 23 MiB |
| strip-grid 150×150 overlapping strips | 1,500 vertices | 180,300 / 90,600 | 151 | 22,425 polygons, 1 hole | 1.05 s | 46 MiB |
| onion: 400 crossing squares/diamonds | 2,000 vertices | 406,336 / 203,968 | 401 | 101,136 polygons, 507k vertices | 9.3 s | 101 MiB |
| dense-random-ring: 2,000 vertices (10^4 box) | 2,001 vertices | 903,162 / 452,581 | 467 | 225,194 polygons, 1.13M vertices | 47.6 s | 220 MiB |

Sparse inputs are near-linear (BVH candidate search, no all-pairs work).
Intersection-dense *layered* inputs hit the O(I·m) regime of the iterative fill
(design.md §4). Failure policy: no cutoffs; invariant violations are returned as
`RepairError` (transport `status: error`); OOM is not guarded. No time budget is
proposed beyond these measurements.

**Stress vs oracle: incomplete.** `stress_oracle.py sparse-ring dense-random-ring-600
strip-grid many-parts` was stopped by the custodian after several CPU minutes
before printing any result (the time went to the oracle/comparator side). Per
custodian instruction it was not re-run. The stress outputs are therefore
**not** oracle-verified; only the direct-API completion, structural invariants
and the exact-arithmetic design back them.

## Numeric tolerances and precision

- All predicates exact (i128 and 256-bit products); no epsilon anywhere in the
  kernel. Output coordinates are the nearest binary64 to the exact rational node.
- Acceptance tolerances are the immutable ones (area ratio 1e-10, Hausdorff
  max(1e-10, 8 ulp)); the grid layer is exact.
- Limitation: two distinct exact nodes within one binary64 ulp would round to the
  same output point; consecutive duplicates are removed and a ring with <3
  distinct rounded vertices is dropped (its area is sub-ulp). Not observed in any
  test; no proof it cannot create an invalid touch in adversarial full-range input.
- Valid inputs: the oracle returns them unchanged; the candidate re-derives them
  and may add integral contact nodes (e.g. hole vertex on a shell edge) and uses
  its own ring direction/start. Integral nodes are unchanged by truncation.

## Interface/transport changes in the mutable candidate

- `try_make_valid` (fallible, approved for production by notes/custodian-response.md)
  and `fallible_implementation()` added; `implementation()` returns
  `Some(make_valid)` (compatibility wrapper, panics only on an internal invariant
  violation; not on any production/transport path).
- `src/main.rs` now calls the fallible API and reports `RepairError` as
  `{"status":"error"}`; the request format and all other statuses are unchanged.

## Not done / outstanding (for the custodian)

- Layer D production integration, GDAL real-data differential tiles, Python/JS
  binding tests, MSRV toolchain check (1.90 not available here; built with 1.99).
- The 8 candidate-only encoder overflows above (order-dependent cursor deltas on
  full-range inputs).
- Whether the `equals`-on-invalid-tile limitation of the immutable comparator
  should be amended (e.g. to the exact structural comparison used here).
- Oracle verification of the stress cases (incomplete).
- No additional dependency is needed; no notes/custodian-request.md was filed.

## Provenance statement

Kernel: `exact.rs`, `arrangement.rs`, `topology.rs`, `lib.rs` written in this
session from the design in notes/design.md; only `std` (serde_json is used by the
transport/tests only). No GEOS, JTS, PostGIS, GDAL or Terrio source was available
or consulted (notes/exposure.md). The fill rule was derived from black-box
oracle probes kept in `candidate/tests/oracle/probes/`. Functional agreement is
not proof of legal independence; that gate belongs to the maintainer.

# Standalone CRS benchmark validation

The standalone crate was run directly through the reporter's unmodified judges
and frozen inputs at `rust-proj-capability` revision
[`ddd9fac70ec370acd9651ebb8b46035207347ad5`](https://github.com/mdsumner/rust-proj-capability/tree/ddd9fac70ec370acd9651ebb8b46035207347ad5).
The Terrio source revision was `f454562` on `perf/crs-optional-fallback`.
[Machine-readable results](benchmarks/crs-standalone.json) include input hashes,
oracle environments, per-fixture results and projection error maxima.

## CRS study

The 93-fixture study retains its frozen PROJ 9.9.0 / pyproj 3.8.0 oracle, with no
datum grids and network disabled. The judge's "exact" classification requires
less than 1 mm; it does not assert bitwise equality.

| Configuration | Exact | Close | Failed | Unresolved | Refused like oracle |
| --- | ---: | ---: | ---: | ---: | ---: |
| Standalone native default | 71 | 1 | 1 | 15 | 5 |
| Standalone with fallback | 74 | 1 | 1 | 12 | 5 |

The fallback configuration retains the reported Terrio 0.5.0 score of 74 exact
fixtures. The native configuration refuses `method-eck4`, `method-natearth` and
`method-hammer`, with an error naming `wbprojection-fallback`. Both configurations
retain the existing approximately 0.815 m forward difference for
`form-2264-proj-bound` and refusal at the Mercator pole (`edge-merc-pole`).
Geocentric, PROJJSON, IAU and additional projection capabilities are tracked in
[#269](https://github.com/opengeos/terrio/issues/269)–
[#272](https://github.com/opengeos/terrio/issues/272).

## Projection graticules

The 54-projection suite uses its separate PROJ 9.10.0 development oracle,
revision `5fa31c29f0a806568ed7bc76c1a557604db4a804`, with pyproj 3.8.0.
Its "exact" classification requires forward error below 1 mm and inverse error
below 1e-8 degrees, with longitude differences wrapped and scaled by
`cos(reference latitude)`.

| Configuration | Exact | Close | Gaps | Off | Unsupported |
| --- | ---: | ---: | ---: | ---: | ---: |
| Standalone native default | 35 | 2 | 2 | 1 | 14 |
| Standalone with fallback | 40 | 2 | 5 | 1 | 6 |

Both configurations classify the four corrected projections as exact across
each 91-point graticule, including poles and the reported orthographic case:

| Projection | Maximum forward difference | Maximum inverse angular difference |
| --- | ---: | ---: |
| Mollweide | 0 m | 3.56e-15 degrees |
| Wagner IV | 0 m | 4.24e-22 degrees |
| Wagner V | 0 m | 2.69e-14 degrees |
| Ellipsoidal orthographic | 0 m | 5.94e-13 degrees |

These are native build observations for the pinned fixtures. WASM pole rounding
has separate allowances in the repository's regression tests; see
[testing](testing.md#crs-benchmark-regressions-266-268).

Winkel Tripel remains "off" in the original projection benchmark because its
bare operation oracle supplies PROJ's operation default `lat_1=acos(2/pi)`, while
Terrio's CRS parser supplies the CRS default `lat_1=0`. With an explicit
`lat_1=50.467…`, the pinned regression agrees within its documented tolerances.
The original benchmark inputs were retained, so the table exposes this semantic
difference. No oracle was regenerated with the local PROJ installation.

## Reproduction

In the pinned benchmark checkout, replace its two `terrio` Cargo dependencies
with aliases of the standalone package at the Terrio revision above:

```toml
terrio = { package = "terrio-crs", git = "https://github.com/opengeos/terrio", rev = "f454562", default-features = false }
```

Retain `optional = true` in `engines/rust-crs/Cargo.toml`. Replace the adapter
imports `terrio::crs::` with `terrio::` (also in the CRS compile check), and label
the candidate/engine `terrio-crs`, version `0.1.0`. The measured run used a local
path alias to this same source. The adapters, fixtures and judges otherwise stay
unchanged.

Use an empty directory for `TERRIO_GRID_DIR`, `TERRIO_GRID_CACHE`, `PROJ_DATA` and
`PROJ_LIB`, and an isolated `XDG_DATA_HOME`; set `TERRIO_NETWORK=OFF` and
`PROJ_NETWORK=OFF`. Retain the workspace's bounded local-build settings.

From `engines/rust-crs`, run:

```sh
cargo run --release --no-default-features --features c-terrio -- \
  --candidate terrio-crs ../../suites/crs/crs.json
cargo run --release --no-default-features \
  --features c-terrio,terrio/wbprojection-fallback -- \
  --candidate terrio-crs ../../suites/crs/crs.json
```

From `engines/rust-projections`, run:

```sh
cargo run --release -- --engine terrio-crs ../../suites/projections/reference.json
cargo run --release --features terrio/wbprojection-fallback -- \
  --engine terrio-crs ../../suites/projections/reference.json
```

Save each JSON-lines result separately and pass projection records to the pinned
`report/projections.py::judge` against the corresponding reference entry. The
CRS executable includes its judge and emits verdicts directly. Keep generated
lockfiles to reproduce the resolved third-party dependencies.

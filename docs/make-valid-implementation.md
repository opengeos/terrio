# Independent tile polygon repair

The implementation replaces the deferred tile-grid repair formerly in
`crates/terrio/src/tiles/valid.rs`. The public `ops/topology.rs` operation is a
separate path and is outside this replacement. The behavioral contract is in
[the specification](make-valid-clean-room-spec.md). This record
preserves the implementation's provenance; the one-off agent launcher and package
scaffold are not part of Terrio's maintained tooling. Runtime and regression
tests do not depend on an AI provider, a sandbox launcher or login credentials.
The specification's original observation filename now refers to the unchanged
canonical fixture at `crates/terrio/tests/data/make-valid.json`.

## Information boundary and authorship

The implementing model is **Claude Opus 5.5**, requested as
`claude-opus-5-5` without fallback. Its first implementation session is
`591d41e0-232b-4e2e-9080-504dd41b4d81`. It began in a new native Claude CLI
process, with no parent transcript, saved sessions or host Claude configuration.
The earlier T3 thread that declined the work did not supply implementation code.

An ordinary agent running shell commands through a sandbox is insufficient:
other tools can still read the original checkout or conversations. The actual
implementation process, including every tool it starts, runs inside Bubblewrap
with new user, mount, PID, IPC and network namespaces, cleared environment,
private process visibility and disabled nested user namespaces. The original
checkout, host home, repository history, other conversations, browser and MCP
servers are absent. Only reviewed handoff files, vendored dependencies, selected
compiler/system executables, and hashed executable reference libraries are
available. The implementer checked and recorded the boundary before designing.

The CLI runs in `--bare` mode with empty MCP configuration, empty setting sources,
disabled hooks, disabled session persistence and nonessential traffic. Its actual
initial tools were Bash, Read and Edit. Only candidate source, notes, scratch
build output and fresh agent state are writable.

Model inference uses a private Unix socket exposed to the sandbox. A host broker
forwards only approved Messages API routes and the exact model to Anthropic.
The provider credential is used by that broker for its HTTPS requests and is
not mounted into the implementer's sandbox. The sandbox has a local loopback relay
inside its private network namespace, with no host web listener or general proxy.
The broker audit records model, tool names and request hashes, never credentials.
The first run used the initial route/model/tool restrictions; subsequent gateway
hardening also rejects URL-backed documents and remote MCP inputs. These later
source edits do not retroactively change the policy of an already running process.

The original run used a saved Claude CLI OAuth access token in the host broker
for requests to Anthropic. That credential handling was inappropriate for a
reusable repository script. The launcher, gateway and standalone package tooling
were removed from the maintained tree. The original setup and authentication
remain documented as historical facts; no token value or credential file was
included in the PR. The original sealed archive is retained separately.

The source-exposed custodian authored the behavioral package, isolation tools,
reference adapter and production integration. The implementer authored the
replacement kernel and its design and generated tests. The custodian did not
supply an algorithm from the retired source. A behavior-only interface decision
approved a fallible repair API so invariant failures can propagate to callers.

These are documented task-local access and provenance controls. They cannot
establish what a pretrained model previously encountered or constitute a legal
opinion about the repository as a whole.

## Production integration

The entire 628-line legacy `tiles/valid.rs` was removed, including its graph/face
machinery, ordering code, orientation/envelope helpers and shared grid index.
The JTS-derived validity checker remains separate and imports only plain point
and polygon aliases from the new module. The existing clipping-failure predictor
now obtains envelope candidate pairs through the already installed `rstar`
dependency. No repair dependency was added.

The production repair uses `try_make_valid`, returning an explicit error for
internal invariant failures. That error propagates through polygon encoding,
per-tile preparation and `cut_layers` to the writer. The harness's infallible,
panicking compatibility wrapper is absent from production. The polygon encoder
also checks all signed cursor deltas: full-range inputs that cannot be encoded
fail with `InvalidData` instead of overflowing arithmetic. A regression exercises
both overflowing directions and the representable signed endpoints.

[The import ledger](make-valid-import.json) records original and production
hashes and the complete source retirement boundary. The three kernel module
files were compared against the final implementer delivery after changing only
`crate` imports to `super` and applying rustfmt. The fallible repair entry point's
body was checked for exact equality. The custodian authored the caller error
propagation, checked encoder arithmetic, R-tree adapter and test wiring.

Review follow-up adds checks outside the delivered kernel: `tiles/repair_checks.rs`
compares complete fixture rings after normalizing ring starts, directions and
polygon/hole order, preserving connectivity and grouping. Valid-input fixtures
are excluded from that exact comparison because their contract permits added
contact nodes. A writer-entry regression also checks propagation of unsupported
coordinate deltas with large extent and buffer options. The delivered kernel and
its original tests remain unchanged.

Final PR packaging rechecked every source hash against the sealed delivery
archive (SHA256 `5257385a86089d787eee6adf80102872d5fd84c3e0858828bc2ccac1a99cc1ab`).
The original ledger had retained an earlier hash for the standalone test file;
the ledger now records the final archive hash and preserves the previous value
explicitly. No production kernel or test code changed in that correction. All
three runtime kernel modules were also compared mechanically against the archive
after only the documented import changes and rustfmt.

Both root and crate NOTICE files now identify this independent MIT implementation
separately from the JTS ports. The earlier JTS record retains its historical
scope and points here for the later repair replacement.

## Implementer delivery and checks

The original [design](make-valid-clean-room/design.md),
[exposure record](make-valid-clean-room/exposure.md),
[standalone validation](make-valid-clean-room/validation.md),
[approved manifest](https://github.com/Mavrick-F/terrio/blob/db3e102aff48854ab77d487ecdc5892c01404a97/docs/make-valid-clean-room/approved-manifest.json),
[run metadata](https://github.com/Mavrick-F/terrio/blob/db3e102aff48854ab77d487ecdc5892c01404a97/docs/make-valid-clean-room/run.json) and
[process-control message](make-valid-clean-room/custodian-control.json) are
preserved, with verbose one-off setup records pinned to the original reviewed
commit. The [custodian process research](https://github.com/Mavrick-F/terrio/blob/db3e102aff48854ab77d487ecdc5892c01404a97/docs/make-valid-clean-room-process.md)
and [package-preparation checks](https://github.com/Mavrick-F/terrio/blob/db3e102aff48854ab77d487ecdc5892c01404a97/docs/make-valid-handoff-validation.json)
are historical references rather than setup instructions for Terrio contributors.
The standalone validation predates production integration and its
outstanding integration list is historical; the production results below apply.

The independent design uses an exact planar arrangement, rational intersection
nodes, exact wide-product comparisons, deterministic graph traversal and an
iterative face fill. Conversion to binary64 occurs at the output boundary.
Only Rust's standard library is used by the kernel.

| Standalone check | Result |
| --- | --- |
| Captured kernel observations | 51/51 pass |
| Supported production-encoder observations | 50/50 pass; one documented wide-i32 encoder exclusion |
| Native tests | 4 arithmetic unit tests and 20 adversarial/property tests pass |
| Generated differential inputs | 4,500 kernel comparisons across three seeds; no kernel mismatches or execution failures |
| Generated supported encoder comparisons | 4,187 geometric matches and 145 exact structural matches where the reference tile itself is invalid |
| Generated full-range encoder exclusions | 160 reference overflows; 8 candidate-only overflows from different ring/cursor order |
| Format, strict Clippy, native and WASM library builds | pass |
| Stress completion | 200,003-vertex sparse input: 0.33 s / 66 MiB; 2,001-vertex dense input producing 225,194 polygons: 47.6 s / 220 MiB |

These local stress measurements establish completion and resource observations,
not reference agreement. The optional reference comparison of those largest
inputs was interrupted after several CPU minutes, before a result, and is
explicitly **incomplete**. The control message provided only process priorities
and the fallible interface decision, with no algorithm or prohibited source.

## Production validation

The [production evidence](make-valid-production-validation.json) preserves exact
commands, artifact/data hashes, initial failures and the final classifications.

| Production check | Result |
| --- | --- |
| Workspace Clippy, all targets, `-D warnings`; formatting | pass |
| Tile unit suite before caller overflow guard | 41 pass, including the imported kernel and envelope adapter |
| Affected vector unit tests after overflow guard | 3 pass, including checked-delta regression |
| Tile integration with Mercator-safe countries | 8 pass; 2 network tests ignored; 3 missing external fixtures explicitly excluded |
| Existing JTS behavior regressions | 5 pass |
| Optimized CLI and Python wheel | build successfully |
| Full WASM JavaScript binding and targeted Node tile test | build successfully; 1 targeted test passes |
| Fresh installed Python binding tile round trips | 2 pass |
| Invalid-input binding smoke | 47 source features; Python and WASM each match CLI at 4 zooms (8 comparisons) |
| GDAL differential tiles | 15 zoom comparisons; no unclassified geometry failures; classifications below |
| Comparator negative controls | 9 tests pass, rejecting omitted/duplicated rings, winding changes, moved nonzero holes and non-grid changes |
| Wheel license contents | pass; wheel NOTICE equals root and crate NOTICE; installed extension hash equals fresh wheel |

The first broad integration attempt failed on three absent external fixtures
(`landsat.mbtiles`, `countries_mvt.mbtiles`, `cb_2018_us_zcta510_500k.pmtiles`)
and an Antarctica coordinate outside Web Mercator. The retained final run uses
the documented countries subset without Antarctica and names the three excluded
tests explicitly. Those missing tests are not claimed as passes. The five other
JTS behavior regressions and the repair-specific real-data comparisons ran.

**GDAL comparison classifications.** Countries match at zooms 0–6. Counties
were checked at zooms 0–3, comparing 13,930 feature instances; countries account
for 4,122 feature instances. The writer's existing synthetic clipping differences
remain unchanged at 25 feature-key/zoom combinations; actual geometries were
checked against frozen baseline outputs, rather than waiving a list of IDs.

Seven county feature instances, across six key/zoom combinations, have a separate
**invalid collinear-ring ordering difference**. Each contains a ring that becomes
collinear after integer truncation. The oriented integer ring multisets are
exactly identical, and polygon groupings of every non-collinear ring agree.
GDAL associates the zero-area collinear ring with a different preceding polygon
because the independent implementation enumerates polygons differently. One
reference decode consequently reports negative area; its geometry is already
invalid. These cases are reported as serialization differences, not as valid
geometry matches or silently treated as unchanged baseline output.

The initial strict comparison rejected those six keys. The final comparator
adds only the explicit collinear-ring classification: both geometries must be
invalid, coordinates must reconstruct the exact tile grid, all oriented rings
and multiplicities must match, and non-collinear polygon groupings must match.
It cannot waive a changed nonzero hole or missing ring. Nine positive/negative
controls exercise those restrictions. The original failed report is retained.
The 16 reference-invalid county feature instances remain labelled invalid.

All original 51 kernel observations and their 50 supported encoder comparisons
still pass the unchanged standalone replay. The collinear-ring classification
changes only the additional production comparison's handling of undefined
invalid decoded geometry; the repair implementation and recorded fixtures were
not edited in response.

## Scope and limitations

The compatibility target is the polygonal content of GEOS 3.14.1 linework repair
for integer tile inputs. Polygon/ring enumeration and ring starts are not a
compatibility requirement. Kernel tolerances are those in the specification;
the encoder comparisons use exact integer geometry and feature presence.

Full-range `i32` inputs remain useful kernel tests but are not universally
encodable with signed 32-bit cursor deltas. The eight candidate-only overflow
cases remain output-order differences from the reference; production now returns
an error on such an unsupported delta. No claim of matching the reference's
private traversal order is made. The implementer's validation also records the
unproved edge case of distinct exact nodes coalescing under binary64 rounding,
and unguarded allocator exhaustion. The MSRV 1.90 toolchain was not available;
builds used the installed Rust 1.99 toolchain.

The existing pre-quantization clipping-failure heuristic and other geometry
operations retain their documented scope. This replacement does not certify
every possible geometry, every GEOS version or the rest of Terrio's licensing.

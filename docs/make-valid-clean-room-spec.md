# Independent tile polygon repair: implementer specification

Version 2, 2026-10-29. Repository baseline:
`0281ec0add52008c051cd024ab9a23ca855055d7`.

**Status:** behavioral handoff reviewed by a separate research agent, with 51
captured reference observations. A standalone scaffold, encoder adapter and
per-command isolation runner are prepared alongside this specification. No
replacement has been implemented or implementation agent launched. A fresh
agent's complete tool access must obey the approved boundary before a strict
isolation claim is made. The custodian keeps process/provenance notes separate
from implementation input.

## 1. Purpose and scope

Terrio needs an independently written, pure-Rust replacement for the polygon
repair used when writing vector tiles. The replacement must produce the same
polygonal content as the public GEOS **linework** MakeValid operation on the
integer-coordinate polygons submitted by the tile writer. The target includes
repairs caused by quantization, not just invalid source geometries.

The callable contract is small; robust geometry repair is a substantial task.
Do not estimate the work from the size of the current entry point.

This specification concerns the private tile path only. Terrio's public
`ops::topology::make_valid`, CLI `make-valid`, Python/JavaScript geometry-repair
APIs and `clean` use a separate path and are outside this replacement. Preserve
their behavior. The tile writer is shared by the relevant Rust, CLI/MCP,
Python and JavaScript/WebAssembly tile-writing surfaces; they should continue
to use one common repair implementation.

### Evidence labels

- **[I] Interface:** facts supplied from Terrio's caller/type boundary at the
  pinned baseline. They describe integration, not the old repair algorithm.
- **[D] Documentation:** behavior stated in the cited public API documentation.
- **[O] Observation:** results of the accompanying independent-input executable
  experiment against GEOS 3.14.1, through Shapely 2.2.0.
- **[P] Project requirement:** a proposed acceptance or robustness policy in
  this specification. It is not a claim about undocumented GEOS internals.

An observation on 51 cases is not proof of equivalence for arbitrary inputs.
Keep these categories separate when extending the specification.

## 2. Information allowed into implementation

The initial implementation package consists of this file, the paired
`make-valid-clean-room-observations.json`, the independently authored
`scripts/reference/capture_make_valid_contract.py`, a reviewed standalone
scaffold containing only the types and entry point below, and a reviewed
test-only tile encoder adapter (or isolated callable service with a documented
input/output protocol). The adapter must expose the actual existing truncation,
winding and degeneracy behavior required by section 8; a generic MVT encoder is
not a substitute. The custodian must supply, review and hash these materials
before release. The standalone handoff now contains those artifacts and its
file manifest. Use its README and PROTOCOL for paths and invocation; do not
begin by searching the original repository for MakeValid.

Use a fresh agent conversation without this task's analyst transcript or
summary. Work in an isolated directory with access restricted to approved
inputs. A normal Git worktree still exposes source history and is insufficient
as the only isolation measure. The analyst has seen legacy source material and
must not become the replacement implementer.

Permitted references are the public documentation linked here and independently
chosen mathematical references after provenance review. Do not follow
documentation links into GEOS implementation source. Do not inspect the old
Terrio repair, its history, patches, private helpers or unreviewed tests. Do not
translate source, reconstruct remembered source, or ask another agent to do so.
Ask the custodian for additional observable requirements or original oracle
cases when a behavior is unclear.

An implementation may use compatible, reviewed Rust dependencies or port
compatible JTS primitives with explicit attribution. A primitive port changes
the provenance description: identify exactly what was ported and its license.
Do not call that code wholly original. Dependency availability alone is not a
license/provenance audit of a particular component.

## 3. Types and entry point [I]

```rust
type IPt = (i32, i32);
type P = (f64, f64);
type Polygon<T> = Vec<Vec<T>>;

fn make_valid(polys: &[Polygon<IPt>]) -> Vec<Polygon<P>>;
```

The production function has crate visibility. The standalone implementation may
temporarily expose it publicly for its harness. Names of new internal types,
functions, files and algorithms are the implementer's choice.

Interpret the nesting as follows:

| Value | Meaning |
| --- | --- |
| `polys` | The components of **one feature's** polygonal geometry; process together |
| `polys[p]` | One polygon: exterior first, followed by zero or more holes |
| `polys[p][r]` | A ring; normal caller inputs repeat the first coordinate at the end |
| `polys[p][r][v]` | One signed integer tile coordinate `(x, y)` |
| returned outer vector | Polygonal components of the repair, with fractional coordinates allowed |

The coordinates are already in tile space. They are neither geographic degrees
nor projected map coordinates at this boundary. No CRS conversion belongs here.
Negative and buffered coordinates outside the nominal tile extent are allowed.
The API type is `i32`; arithmetic must be safe across its full range.

No attributes, IDs, layer schema, CRS, Z or M enter this function. It must not
mutate the borrowed input or perform I/O. Returning an empty vector means that
no polygonal area survives; it is not an error status.

## 4. Position in the tile pipeline [I]

The integration custodian supplies this caller contract:

1. Clip/reproject as the existing writer does, convert rings to the integer tile
   grid, close them, remove consecutive duplicate grid coordinates and reject
   rings that cannot be encoded.
2. Run the existing, independently attributed integer validity predicate on all
   encoded components of the feature.
3. If valid, keep the existing encoding. The replacement is not called.
4. If invalid, pass the encoded polygon list to this function and discard that
   feature's provisional geometry commands.
5. Encode the returned polygons using **truncation toward zero** of fractional
   coordinates, the existing winding/degeneracy rules, and a reset geometry
   cursor. Omit the feature in this tile if no polygon can be encoded.

Do not move repair ahead of quantization, repair each component in isolation,
repair valid production features unconditionally, or substitute nearest-integer
rounding for step 5. Components can interact, and this order affects tile content.
The current clipping-failure/omission policy is separate and remains outside
this kernel's scope.

The output representation can affect step 5 even when two floating polygons
enclose identical areas. For example, inserting a collinear fractional vertex
can alter a boundary after each vertex is truncated separately. Ring direction
can also affect whether the existing encoder drops a hole. Therefore passing a
floating area comparison alone is insufficient.

## 5. Normative geometry behavior

### MV-01: Reference operation [D, O, P]

For well-formed caller inputs, the reference is:

```text
g = MultiPolygon(all input polygons, in input order)
r = GEOS MakeValid(g, method = LINEWORK)
expected = every nonempty Polygon component of r,
           recursively extracted through MultiPolygon/GeometryCollection
```

The collector uses the equivalent public call
`shapely.make_valid(g, method="linework", keep_collapsed=True)`. Always specify
the method; do not rely on a future default. `keep_collapsed=True` is required
by Shapely's linework API. It is not a request for Terrio to return lines or
points: polygon extraction is the separate final step.

GEOS documents repair of invalid geometry, possible splitting and dimensional
collapse. Its public API distinguishes linework from structure. Those facts
define the behavioral target without prescribing the implementation.
[GEOS repair contract](https://libgeos.org/doxygen/classgeos_1_1operation_1_1valid_1_1MakeValid.html),
[GEOS public C API](https://libgeos.org/doxygen/geos__c_8h.html),
[Shapely repair methods](https://shapely.readthedocs.io/en/stable/reference/shapely.make_valid.html).

The fixture pins Linux x86-64 GEOS 3.14.1. The online GEOS documentation is a
moving development reference, not evidence that a future release behaves
identically. Reference upgrades require a separately reviewed fixture update.

### MV-02: Polygonal projection and collapse [I, D, O]

Return polygons only. A GEOS result can also contain linework from spikes,
shared boundaries or collapsed rings; discard such line/point components after
determining the polygonal result. A fully collapsed input returns `[]`.
Do not convert a leftover line into an area or buffer it to keep it visible.
An input vertex that survives only in a discarded line need not appear in output.

Do not delete apparently collapsed/retraced segments before deciding whether
they affect the polygonal repair. Only the dimensional projection of the final
repair is specified here. GDAL also documents a distinct lower-dimension
removal step in its public make-valid command; that command is useful context,
but does not replace the tile-specific adapter above.
[GDAL make-valid behavior](https://gdal.org/en/stable/programs/gdal_vector_make_valid.html).

### MV-03: Joint linework and ring roles [D, O]

Linework repair combines input boundaries before constructing valid area.
Do not implement a rule that always unions exterior interiors and subtracts
declared holes. Outside holes can become independent shells; nested polygon
components can create a hole; overlapping components can split into different
regions.

Documentation describes noded linework and even/odd area selection, but that
description is **not** a sufficient instruction to XOR the original ring
interiors. Coincident/retraced edges are decisive counterexamples in the fixture.
Do not reduce duplicate boundaries modulo two on the assumption that their
enclosed area disappears. The observed examples below are binding regression
requirements, including the apparently surprising ones.
[Public description of linework versus structure](https://postgis.net/docs/ST_MakeValid.html).

### MV-04: Representation and validity [P]

Every returned component has one exterior followed by its holes. Rings must be
closed, finite, nonzero in area, and have at least three distinct XY vertices
plus closure. No empty polygon, dangling line or point belongs in the result.
The combined polygonal output must satisfy default OGC polygon validity:
non-self-crossing rings, holes properly assigned and component interiors
disjoint. Point contacts permitted by those rules are allowed. Adjacent pieces
sharing a whole boundary must not be left as an invalid MultiPolygon.

Support intersections in segment interiors, endpoint/segment contacts,
collinear partial overlaps, reversed coincident edges and multiple segments
meeting at a common location. Preserve distinct intersection locations unless
the documented numerical policy explicitly justifies identifying them.

No raw WKB equality, GEOS ring starting coordinate, component enumeration order,
private sorting order or memory layout is required. Output must nevertheless
be deterministic and pass the downstream encoding checks in section 8.
Do not normalize ring directions or remove boundary subdivisions merely to make
the floating comparison easier when that changes the encoded result.

### MV-05: Valid and empty inputs [I, O, P]

Production bypasses this function for valid geometry. Direct calls on valid
polygonal inputs should preserve their area and holes; exact ring starts/order
are not required by this helper contract. Empty outer input and empty polygon
components are allowed and contribute no area. Empty rings may be ignored.
If a declared exterior has no surviving vertices, ignore that polygon rather
than promote its declared holes. This last rule is defensive project policy,
not an observation of a GEOS polygon with an absent shell.

### MV-06: Defensive inputs outside the normal caller domain [P]

These rules allow a total internal API without fabricating a GEOS observation
for malformed WKT or polygons rejected by its constructors:

- Logically close a nonempty unclosed ring by connecting last to first.
- Ignore zero-length segments and consecutive duplicate coordinates when they
  do not change linework support. Do not indiscriminately remove a repeated
  coordinate elsewhere in a ring: it can mark separate lobes or a spike.
- A ring with fewer than three distinct vertices cannot itself contribute area.
  Degenerate rings must not panic or create a filled triangle by accident.
- No recursion or walk may run forever on malformed connectivity. Indexing,
  allocation-size calculations and integer arithmetic must be checked/bounded.

Normal caller inputs do not contain non-finite coordinates because they are
integers. The implementation must not manufacture non-finite output.

### MV-07: Numerical precision [P]

Keep fractional intersections until the caller truncates them. Use exact or
robust predicates where needed to distinguish crossings from touches. Converting
all input coordinates to `f32`, snapping output to the tile grid early, or using
one arbitrary epsilon to merge nearby nodes is unacceptable.

Widen operands **before** subtraction/multiplication. An orientation determinant
for full-range `i32` input can be computed in `i128`; this does not imply that
all rational-intersection comparisons fit `i128`. Products of rational
numerators/denominators can exceed that range. Document the bounds of the
chosen construction and comparison methods, and use checked arithmetic or an
appropriately reviewed robust representation where the bounds require it.

When output rounding falls near an integer threshold, the grid comparison is
authoritative; a loose floating tolerance must not excuse a one-unit tile shift.
For a numerical case outside the pinned observations, request a new public-API
oracle probe instead of consulting the legacy algorithm.

### MV-08: Failure and resource limits [I, P]

The existing signature has no error channel. Empty output is a legitimate
geometric result and must not silently double as a resource-exhaustion or
unresolved-topology result. Never return partially reconstructed area as success.

The first implementation must document its complexity in terms of input
segments, intersections and output size, plus practical stress-test results.
Avoid unconditional all-pairs work on ordinary sparse large rings. Quadratic
intersection output itself is possible; a claim of linear memory for all
inputs would be misleading.

If the chosen implementation requires explicit complexity limits or can report
ordinary repair failure, propose a separate fallible internal API and propagate
errors through the writer with the integration custodian. Do not change this
interface unilaterally, invent an arbitrary silent cutoff, or promise immunity
from operating-system out-of-memory termination. Resource/error-policy changes
are an explicit contract amendment before integration.

## 6. Reference examples [O]

All examples below come from the paired capture, not the legacy tests. Coordinates
are independently authored. Let `S(x0,y0,x1,y1)` be the closed rectangle with
vertices `(x0,y0),(x1,y0),(x1,y1),(x0,y1)`. `A = S(0,0,6,6)`.
The JSON contains exact nested input arrays and unrounded binary outputs.

| Fixture ID | Input idea | Required polygonal result |
| --- | --- | --- |
| `empty` / `empty-component` | No area input | No polygons |
| `square` | A | One square, area 36 |
| `square-hole` | A with S(2,2,4,4) as hole | One polygon, one hole, area 32 |
| `bowtie` | Ring `(0,0),(6,6),(0,6),(6,0),close` | Two lobes meeting at `(3,3)`, total area 18 |
| `fractional-crossing` | Ring `(0,0),(7,5),(0,5),(6,0),close` | Two lobes; intersection `(42/13,30/13)`; area `425/26` |
| `half-integer-crossing` | Same, with `(7,0)` instead of `(6,0)` | Two lobes meeting at `(3.5,2.5)`, area 17.5 |
| `collapsed-collinear` / `collapsed-point` | No two-dimensional support | No polygons, despite nonempty lower-dimensional reference output |
| `outward-spike` | Square with retraced spur up to `(3,9)` | Square area 36; discard the separate line |
| `double-wound` | Traverse A's perimeter twice | Square area 36, not empty |
| `identical-components` | Two copies of A | Square area 36, not empty |
| `identical-components-reversed` | A and its reverse | Square area 36 |
| `adjacent-squares` | A, S(6,0,12,6) | Joined rectangle, area 72; discard residual line |
| `overlap-crossing` | A, S(3,2,9,8) | Two polygonal regions, total area 48 |
| `overlap-coincident-edges` | A, S(3,0,9,6) | One rectangle covering x=0..9, y=0..6, area 54; discard interior lines |
| `nested-components` | S(0,0,9,9), S(3,3,6,6) | Outer square with inner hole, area 72 |
| `hole-outside` | A with S(9,0,12,3) declared as hole | Two shells, total area 45 |
| `hole-crosses-shell` | A with S(3,-3,9,3) declared as hole | Two polygonal regions, area 54 |
| `hole-shares-shell-edge` | A with S(0,2,3,4) declared as hole | Full A, area 36, plus discarded reference linework; not a notch |
| `duplicate-holes` | A with the same inner hole twice | One hole remains, area 32 |
| `shell-equals-hole` | A also used as its own hole | Full A, area 36 |
| `nested-holes` / `three-nested-rings` | Three nested square boundaries | Two polygons, one hole in total, area 96 |
| `valid-plus-collapsed` | A plus a collapsed separate component | A survives, area 36 |

The capture also includes overlapping holes, point contacts, repeated vertices,
collinear subdivisions, full-range `i32` coordinates, a small polygon near
`i32::MAX`, negative fractional intersections, reversed rings, cyclic ring
starts and reversed component order. Do not use the table as an area-only test:
equal area does not establish equal geometry.

## 7. Fixture format, provenance and reproduction

The capture file contains metadata and an `observations` array:

| Field | Interpretation |
| --- | --- |
| `input_polygons` | Authoritative nested integer API input, including empty components |
| `input_wkb` | The geometry supplied to the oracle; its constructor can normalize empty components |
| `input_valid` | Oracle validity before repair |
| `result_wkt` / `result_wkb` | Complete observed GEOS result, including dimensions Terrio discards |
| `polygonal_wkb` | MultiPolygon wrapper around recursively extracted nonempty polygons |
| `polygonal_coordinates` | Same polygonal components as explicit XY rings |
| counts, area and validity | Diagnostic summaries; not substitutes for comparing the geometry |

Use binary output for precision-sensitive comparisons. JSON numbers provide
the normal round-trippable binary64 representation. The file records method,
versions, OS/architecture, collector SHA-256 and loaded GEOS binary hashes.
The input generator contains no Terrio import, repair algorithm, old test
excerpt or upstream fixture.

To reproduce on mavrick's box using the already provisioned validation Python:

```sh
hq run "terrio capture MakeValid contract" --slot gate -- bash -c \
  '/mnt/scratch/terrio/tools/.pixi/envs/default/bin/python scripts/reference/capture_make_valid_contract.py > /mnt/scratch/terrio/make-valid-contract-new.json'
```

On another machine, use an isolated environment with the recorded Shapely/GEOS
versions and run the collector with that Python. Do not install into system
Python. Different binary hashes require recording the new runtime, not silently
editing the old manifest. Compare a new capture to the existing file before
replacing it. The script writes stdout and does not overwrite fixtures itself.

This capture is an executable behavioral reference, not an already implemented
Rust replay harness. Implementer work includes adding that harness and the
downstream grid comparisons. No comparison against the old Terrio repair was
run to create these observations.

## 8. Acceptance tests and equivalence [P]

### Layer A: Standalone kernel replay

For all 51 observations, feed `input_polygons` to the replacement. Require
finite closed valid polygonal output and compare its geometry to
`polygonal_wkb`. Empty/nonempty status and holes must agree. Ignore ring start,
orientation, component order and Polygon versus one-part MultiPolygon wrapping
for this geometric comparison only.

Use both normalized symmetric-difference area and boundary Hausdorff distance:

```text
area(A symmetric_difference B) / max(1, area(A), area(B)) <= 1e-10
boundary_hausdorff(A, B) <= max(1e-10, 8 * ulp(M))
M = max(1, absolute coordinate ordinates across A and B)
```

Handle two empty results explicitly and reject one-empty/one-nonempty. Check
validity independently; an invalid output must not pass via a forgiving overlay.
For exact integral fixtures, prefer exact topological equality as an additional
check. The tolerance is an acceptance policy, not a claim about universal GEOS
accuracy. Near `i32` limits, area equality alone would be especially weak.

### Layer B: Tile adapter compatibility

Pass both the replacement result and the captured reference polygons through
the **same existing production truncation/winding/degeneracy encoder**, starting
with the same reset cursor. Decode and compare polygonal content exactly in
integer tile coordinates, allowing ring starts and serialization ordering to
differ. Compare feature presence, not just area totals. The floating comparison
cannot waive a mismatch at this layer.

The original encoder uses `i32` coordinate deltas. The full-range
`wide-i32-bowtie` fixture exceeds that arithmetic domain when checks are enabled.
Its kernel comparison remains mandatory; its grid comparison is explicitly
excluded and reported by the prepared adapter protocol. The other 50 captured
cases must encode successfully. This measured caller limitation does not weaken
the kernel's full-`i32` requirements or permit silent skipping of other failures.

This layer must cover the fractional/negative fixtures and cases where holes
or narrow pieces collapse during truncation. The custodian must expose the
reviewed production encoder adapter described in section 2. This is a required
handoff dependency, not an optional replacement for geometric tests. Do not copy
the old repair into the harness. Source review of the caller/adaptor remains
with the custodian unless that source is independently approved for handoff.

### Layer C: Independent adversarial/property tests

Add original tests for unclosed/empty/too-short rings, consecutive duplicates,
integer extremes, deep nesting, many disconnected parts, high-degree contacts,
partially coincident segments and near-integer intersections. Distinguish
defensive policy tests from GEOS-observed tests.

Use a newly authored deterministic integer-input generator, retain its seed,
and record every minimized mismatch. Target at least 1,000 small generated
inputs, including invalid cases, against the pinned public oracle. A target
count is a review gate, not statistical proof of correctness. If reference
construction or repair fails, record it as an oracle failure; do not count a
skipped case as agreement or invent an empty expected result.

Check determinism, non-mutation, output validity, and geometric invariance under
ring rotation/reversal and component permutations, using fresh oracle probes
to resolve any actual exception. For transformations that are not guaranteed
by a floating reference, such as translations near numerical extremes, compare
to an independently transformed reference input before asserting invariance.
When every returned coordinate is exactly integral and within `i32` range,
repairing that valid shape again should be geometrically idempotent. Optional
floating-core idempotence checks are useful only if the independent design
exposes such an API. Do not truncate fractional output back to `i32` and call
that a repair-idempotence test.

Stress at least a large sparse ring and a deliberately intersection-dense
case. Report sizes, wall time, memory and the failure policy. Preserve the
host queue and scratch-disk rules. Set any numerical/time budgets after
measurement; this specification does not invent a performance result.

### Layer D: Production integration

The integration custodian must exercise these existing tests after replacement:

- `cargo test -p terrio --test tiles invalid_polygons_are_made_valid_as_gdal_does`
- `cargo test -p terrio --test tiles sudan_as_gdal_writes_it`
- `cargo test -p terrio --lib tiles::validity`
- Tile-writing checks in Python and JavaScript, plus a shared-core WASM build.

Audit existing fixture provenance before giving their content to the implementer.
The custodian can run them separately and return only behavior-level failures.
Tests requiring data can skip when it is missing; verify they actually ran.

Run real-data differential tile writes on Natural Earth countries (excluding
Antarctica when outside the supported projection), US counties, and independently
authored invalid features. Compare each zoom/tile and feature key with GDAL
3.13.3/GEOS 3.14.1, reading **unclipped** tile geometries where appropriate.
Require matching feature presence/counts, valid surviving geometry, matching
attributes/IDs under the existing writer policy, and per-key symmetric-difference
area ratio at most `1e-10`; also inspect the integer boundary differences.
Countries zooms 0–6, counties 0–3 and synthetic zooms 0–3 form the minimum
documented smoke comparison; expand to the available county zoom corpus for
final acceptance. This scope is a requirement, not a test run already performed.

`scripts/compare_tiles_with_gdal.py` contains useful existing end-to-end
comparisons, but its printed report is not by itself a strict assertion harness.
Make the replacement's acceptance runner fail on mismatch or missing required
data. Keep raster work out of a focused repair iteration when possible. Do not
claim `topology` conformance exercises the deferred tile kernel; it targets the
separate public geometry operations.

Follow repository validation instructions after shared code changes: formatting,
workspace Clippy with warnings denied, appropriate Rust tests, affected binding
tests, WASM, documented MSRV and changed-file pre-commit checks. Run builds and
test suites through `hq` on this host; use `--slot gate` for suites/hooks.
Set `CARGO_PROFILE_DEV_DEBUG=0`, `CARGO_PROFILE_TEST_DEBUG=0`,
`CARGO_BUILD_JOBS=2` and a work-specific target under `/mnt/scratch/terrio/`.
Build artifacts are disposable; specifications, source and audit records are not.

## 9. Implementation choices and prohibited shortcuts

The implementer chooses a design and submits a short rationale **before code**.
Describe the representation, geometric robustness, selected fill behavior,
complexity, determinism and source provenance.

| Approach | Tradeoff |
| --- | --- |
| Independently designed planar geometry repair | Greatest control over the contract; significant work in robust intersections, valid area assembly and numeric edge cases |
| Compose or port reviewed JTS noding/polygonization/overlay primitives | Reuses documented primitives under a compatible license when verified; requires attribution and still needs an independent composition and compatibility validation |
| Reuse an existing compatible Rust repair library | Potentially less implementation work; acceptable only after a source/provenance audit and proof that its actual semantics pass this contract |

JTS `GeometryFixer` is a related operation, **not an established equivalent**:
its documented polygon policy treats shell/hole structure and merges repaired
MultiPolygon parts, and it processes even valid inputs. JTS `Polygonizer` expects
correctly noded input and distinguishes dangles/cut edges; its polygon-only mode
can discard areas. Those APIs are useful primitives/references, not proof of a
drop-in linework repair.
[GeometryFixer semantics](https://locationtech.github.io/jts/javadoc/org/locationtech/jts/geom/util/GeometryFixer.html),
[Polygonizer contract](https://locationtech.github.io/jts/javadoc/org/locationtech/jts/operation/polygonize/Polygonizer.html).

Do not replace the target with zero-distance buffering, generic shell-union
repair, the separate public `geo`-based repair, triangulated parity alone, or a
call to GEOS/GDAL. Any proposed library must pass the same observations and tile
adapter checks. Native GEOS may be used by the external oracle harness only;
the shipped default implementation remains pure Rust and WASM-capable.

Do not imitate old private ordering to obtain byte-for-byte reference output.
If a downstream byte requirement is discovered, bring concrete evidence to the
custodian for an explicit amendment rather than reading the old ordering code.

## 10. Required delivery

Deliver independently authored code; original tests and generator seeds;
fixture replay and strict tile-adapter comparisons; design/robustness notes;
dependency/source provenance with licenses; and a validation report separating
passes, skips, differences and untested scope. State the exact reference versions.

Record task-local exposure, approved input hashes, provider/model identification
when available, and implementation access limitations. A fresh context does not
prove anything about a model's pretraining. Report accidental prohibited source
exposure immediately and stop work in that context pending custodian review.

Functional acceptance and provenance acceptance are separate gates. Neither
reference agreement nor a similarity scanner establishes legal independence by
itself. The maintainer decides release readiness only after both gates pass.

### Suggested fresh-agent brief

> Implement the standalone integer-tile polygon repair described in the approved
> specification, using only the supplied clean-room package and permitted
> references. First record your task-local exposure and propose an independent
> design. Do not read the legacy Terrio repair or GEOS implementation, repository
> history, the analyst transcript or unreviewed fixtures. Preserve fractional
> intersections, polygon-only output and the downstream truncation contract.
> Prove the captured behavior through replay, original adversarial tests and
> the supplied adapter protocol. Route unknown behavior through the custodian;
> report every source exposure and every unexplained mismatch. Do not edit
> production integration or licensing records yourself in the first phase.

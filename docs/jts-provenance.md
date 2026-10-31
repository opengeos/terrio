# Geometry implementation provenance

The geometry replacements on this branch were written from the JTS Topology
Suite **1.20.0**, tag commit
[`6e95fe82feb986a7aa657f4ffa406d8c290af509`](https://github.com/locationtech/jts/tree/6e95fe82feb986a7aa657f4ffa406d8c290af509).
They use its Eclipse Distribution License 1.0 (EDL-1.0), reproduced with copyright
notices in the root and crate `NOTICE`. This changes implementation provenance;
it is not a renaming of the previous GEOS-based implementations.

All JTS paths below are relative to `modules/core/src/main/java/org/locationtech/jts/`.

| Terrio implementation | JTS sources | Adaptations |
| --- | --- | --- |
| `ops/simplify.rs` | `simplify/TopologyPreservingSimplifier.java`, `TaggedLineString.java`, `TaggedLineStringSimplifier.java`, `ComponentJumpChecker.java`; `algorithm/Distance.java`, `RayCrossingCounter.java`, `RobustLineIntersector.java` | Explicit work stack, tagged coordinate indices, robust `geo` predicates and separate `rstar` input/output segment indexes with inactive-entry flags. Terrio geometry conversion preserves Z and drops M. |
| `ops/hull.rs` | `algorithm/ConvexHull.java`; `algorithm/hull/ConcaveHull.java`, `ConcaveHullOfPolygons.java`, `HullTri.java`; `triangulate/tri/Tri.java` | JTS quad-edge point triangulation and traversal; JTS hole joining, ear clipping and constrained Delaunay flips within the finite frame; directed-edge cancellation and boundary tracing in Terrio's coordinate model. The optional convex-hull interior-point reduction is omitted; the two-site result retains input order, matching observed GEOS behavior. |
| `tiles/overlay_envelope.rs`, called by `tiles/vector.rs` | `operation/overlayng/OverlayUtil.java`, `RobustClipEnvelopeComputer.java` | Floating-precision intersection envelopes only: buffered input overlap, full segment endpoint expansion against the fixed target, then the final safe buffer. Rectangle and ring adapters use Terrio coordinates. |
| `ops/strtree.rs`, called by `ops/vector_geom.rs` | `index/strtree/STRtree.java` | Only the first packed leaf level is needed for feature ordering. Capacity is 10; equal centres retain input order. No query tree is constructed. |
| `tiles/validity.rs` | `operation/valid/IsValidOp.java`, `PolygonIntersectionAnalyzer.java`, `PolygonTopologyAnalyzer.java`, `PolygonRing.java`; `algorithm/PolygonNodeTopology.java`, `RayCrossingCounter.java` | Integer-grid specialization with exact i128 orientation, doubled-coordinate midpoint probes, an R-tree for segment candidates, and a bipartite ring/contact graph for interior connectivity. Default OGC rules, without inverted-ring permission. |

The Rust sources retain their existing public entry points. Geometry traversal,
ring closure and rebuilding adapters, and GDAL-derived segmentization are retained.
The separate MakeValid implementation and its helpers formerly in
`tiles/valid.rs` were excluded from the JTS replacement, including its private
STR ordering helper. They have since been removed by the
[independent repair replacement](make-valid-implementation.md). No MakeValid
source was used to write the JTS-derived validity checker.

`ops/jts_delaunay.rs` additionally derives from JTS's
`triangulate/DelaunayTriangulationBuilder.java`,
`IncrementalDelaunayTriangulator.java` and
`triangulate/quadedge/{QuadEdge,QuadEdgeSubdivision,LastFoundQuadEdgeLocator,Vertex,TrianglePredicate}.java`.
Arena indices replace object links, and topology walks are bounded. The polygon
frame expansion (four times the envelope diameter) is retained because it affects
boundary triangulation even for an edge-length ratio of one.

The simplifier keeps the bulk-loaded input index immutable and inserts replacement
segments into an initially empty output index. A generated 387-vertex polygon
reproduced an `rstar` panic when insertion was mixed with bulk loading; its captured
GEOS simplification is replayed by a dedicated regression.

If all point-to-segment distances are uncomparable after floating-point overflow,
the simplifier subdivides the section rather than interpreting its initial
distance sentinel as within tolerance. A three-coordinate line around 1e155
records the executable GEOS behavior.

A real multipart contact case minimized to a triangle and quadrilateral. Cyclic
ring starts, reflections and component-order probes establish a behavioral
adaptation to JTS: once a component is complete, its first retained coordinate
replaces its original second coordinate as the jump-check representative. The
unprocessed components retain their original second-coordinate probes.

The component jump check retains JTS 1.20's linear scan of component
representatives. The R-tree substitution applies to segment-conflict queries,
not to this check. For a candidate span of S segments and C components, the
current caller allocates O(S) window storage and computes its envelope in O(S).
It scans C representatives; each of the R representatives inside that envelope
can require another O(S) crossing-count walk. The cost is O(S + C + RS), up to
O(CS) per candidate, and no near-linear overall simplification bound is claimed.
C counts components within one geometry; layer operations simplify separate
features separately.

## Boundary of this change

**This is not a claim that Terrio is fully license-compliant.** Linework MakeValid
was deferred by the JTS change and has since been replaced independently; its
[separate provenance record](make-valid-implementation.md) describes that work.
Its earlier inclusion under the JTS notice was inaccurate. The repair API in
`ops/topology.rs` (which uses the `geo` crate) remains a separate implementation.

The MVT failure predictor in `tiles/vector.rs` remains a compatibility heuristic,
not a full overlay implementation. Its clipping-envelope calculation is now a
separate, explicitly attributed JTS port. The crossing and omission policy retains
its documented limitation: the original random probes agreed with GEOS 3.14 on
97% of self-crossing and overlapping inputs. This patch does not claim universal
equivalence for that policy. The later independent replacement addresses only the
post-quantization repair. Tile output comparisons exercise
the production writer separately from the geometry-kernel corpus.

For a feature with recorded crossings, the envelope predictor scans its S total
ring segments for each candidate tile. Across K tiles this adds O(KS) work,
plus the crossing checks. With no crossings it returns before the envelope scan.
The result depends on the tile rectangle, so it cannot be cached once per
feature. No dense-input speed or indexed-envelope-query claim is made.

The `pointcloud/filters/hexbin.rs` call site uses the replaced simplifier.
Other references to GEOS in shapefile normalization and linear referencing describe
compatibility behavior. This change does not certify every other algorithm or
transitive dependency.
The [retirement inventory](geos-retirement-audit.md) records the combined JTS and
independent-repair scope, remaining references and release follow-through.

## Behavioral references and replay

GEOS and GDAL are executable oracles only: their implementation source is not an
implementation reference for these replacements. JTS is both the permitted source
reference and an executable cross-check. The small Java adapter under
`scripts/reference/JtsOracle.java` calls JTS APIs; it embeds no reference
implementation. Reference libraries and build products are not committed.

`scripts/compare_jts_behavior.py` deterministically generates adversarial cases,
compares the geometry probe against GEOS, and can refresh the recorded WKB
observations in `crates/terrio/tests/data/jts/`. Its fixed seed is `0x4A5453`.
The six corpora are deterministic gzip-compressed JSON (`*.json.gz`, timestamp
zero). They retain all 1,645 observations and every original JSON byte while
reducing the tracked data from 2,521,571 to 177,440 bytes (93%). Rust replays use
the existing pure-Rust gzip dependency; no external decompressor is needed.
For inspection, use `gzip -dc crates/terrio/tests/data/jts/concave.json.gz`.
The Maven Central `jts-core-1.20.0.jar` used for executable checks has SHA256
`6a783d8f9dba3d3cf7265435f134402f63c05838aa6cbcc4297ad3a5b2842baf`.
The adapter checks the loaded JTS version before refreshing observations.
The fixtures record the reference versions and retain differing JTS observations
rather than silently treating JTS/GEOS differences as identical.

```sh
CARGO_TARGET_DIR="$(cargo metadata --locked --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
export CARGO_TARGET_DIR
cargo build -p terrio --example geometry_probe
python scripts/compare_jts_behavior.py \
  --probe "$CARGO_TARGET_DIR/debug/examples/geometry_probe" \
  --output /path/to/comparison.json
# Optional JTS observations: compile the Java adapter against jts-core 1.20.0,
# then pass --java /path/to/java --jts-classpath classes:jts-core-1.20.0.jar.
```

Simplification compares coordinates and component structure exactly. Hull checks
first compare undirected edges (ignoring ring start and orientation), then require
valid outputs, relative symmetric-difference area at most **1e-10** and Hausdorff
distance at most **1e-10 times max(1, absolute reference ordinate)**. STR ordering
compares exact IDs, allowing only the documented ambiguity for tied sort centres.
Validity booleans must agree exactly. Recorded invalid reference hulls require
exact normalized component/ring coordinates, multiplicities and shell/hole
grouping; no area tolerance applies to them. They are reported separately from
valid output matches. These checks make no equivalence claim for
MakeValid, invalid polygon hull input, or every possible floating-point input.

Polygon hulls require valid, non-overlapping polygonal input. For a square mixed
with a two-distinct-vertex shell, Terrio returns an argument error while GEOS
3.14.1 retains an invalid degenerate component; other collinear components can
be ignored. Crossing rectangle parts return bounded errors in both observed
implementations. Repeated directed edges or more than two faces on an edge are
rejected when building the hull mesh. This consistency check is not a general
polygon validation or repair operation.

Point triangulation retains JTS's subdivision traversal order. Equal-size,
equal-area erosion candidates also require the observed Linux heap order.
Hole seeds are sorted by increasing size with an independently written introsort
reproducing the observed Linux/libstdc++ `std::sort` tie behavior. This is a
compatibility requirement, not a claim of a new sorting algorithm. The helper
was written from executable observations, without GEOS or libstdc++ implementation
source; its provenance is separate from the attributed JTS ports. It is checked
against executable reference results;
a stable seed sort produced materially different holes on regular grids.
This compatibility code does not use GEOS or C++ library implementation source.
The 2026-10-30 reference check used Linux GEOS 3.14.1 with the conda-forge
`libstdcxx` 16.2.0 runtime (`libstdc++.so.6.0.36`, SHA256
`10f0d3c46c070acd4abea2ccf2500ff0ec77e9b90154b82ea2720dba05510cb9`).
The standalone `std::sort` observation probe used Debian g++ 14.2.0-19. Runtime
identity does not establish which compiler built GEOS or its inlined sort.
Revisit this compatibility sort if a supported reference build changes the
recorded equal-edge hull results, or if the public contract deliberately adopts
a different tie policy; a stable replacement requires checking those fixtures.
The recorded reference is Linux GEOS 3.14.1, not a claim about every platform or
future library version.

For the recorded 20-item all-equal-key probe, input IDs `0..20` are emitted as
`[10, 19, 18, 17, 16, 15, 14, 13, 12, 11, 0, 9, 8, 7, 6, 5, 4, 3, 2, 1]`.
The `hole_seed_ties_match_the_recorded_sort_observation` regression pins this
list with both equal areas and distinct areas in reverse ID order. The seed
comparison intentionally uses size alone; area remains a tie-break in the
erosion heap, which is a separate ordering. The unequal-area executable probe
produces the same permutation. The polygon/point hull fixtures additionally
check its geometric effects.

Boundary hole assignment requires the candidate shell's envelope to contain the
entire hole envelope. An envelope intersection alone admits an island touching
one hole vertex, and the parity test can then assign the hole to that island.
A valid slanted-hole/point-contact-island regression checks that the larger
outer shell retains the hole, matching executable GEOS 3.14.1.

Ring-endpoint simplification removes both replaced segments from the input and
output indexes, including original leaf segments, as JTS's
`TaggedLineStringSimplifier.simplifyRingEndpoint` specifies. The replacement
segment remains indexed. A regression checks that a later component's shortcut
still sees this boundary and cannot cross it.

The on-edge insertion regression inserts `(4, 0)` after `(0, 0)`, `(8, 0)` and
`(0, 8)` in an existing subdivision. Both the native port and executable JTS
1.20 return the two non-degenerate triangles covering area 32. Native area-mesh
extraction ignores zero-area faces; this fixture does not show the hypothesized
gap or extra hole. It does not establish that JTS omits every possible
collinear face.

The polygon path also preserves JTS's hole-join, ear-creation and Delaunay-flip
sequence (`triangulate/polygon/{ConstrainedDelaunayTriangulator,PolygonHoleJoiner,
PolygonNoder,PolygonEarClipper,TriDelaunayImprover}.java`, with `Tri` and
`TriangulationBuilder`), implemented in `ops/jts_cdt.rs`. Observed GEOS polygon
erosion uses the frame triangle sequence, a stack and first-exposed edges. Four
Natural Earth hull differences and a two-triangle regression isolated this order
requirement: an equivalent constrained triangle set with another construction
sequence can retain materially different fill.

The polygon triangulator shares its immutable coordinate copy and vertex R-tree
across the frame and all original polygons. Its Delaunay improvement deliberately
retains JTS 1.20.0's `size - 1` source-triangle scan and 200-pass limit; the final
triangle remains eligible as a neighbor. Reaching that limit retains the last
mesh, as JTS does, without guaranteeing convergence for every floating-point
input. Polygon hole-seed scans advance monotonically because triangle degrees
only decrease and edge lengths stay fixed. Ring-envelope indexes prune frame
containment and integer-validity candidates. Polygon hole joining sorts only the
current polygon's candidate vertices once and maintains active sorted ranks as
holes join the ring. A boundary-segment R-tree prunes visibility tests without
changing the exact conflict predicate or descending candidate order. Ring
splices, join-position scans and ear clipping can still take quadratic work.
For H holes and up to N vertices in the growing joined ring, splices cost
O(HN), and candidate traversal can examine O(HN) sites in addition to the
indexed conflict queries. These indexes do not promise linear overall
triangulation. An unclosed mesh
boundary returns an error instead of silently discarding that component.
Boundary hole assignment caches shell areas and queries a shell-envelope index
before running the original point-in-ring test. Candidate IDs are restored to
shell order so equal-area owners keep the existing tie behavior. Overlapping
envelopes can still require a containment walk for every candidate.
Tight erosion intersects per-vertex
shell memberships, retaining every owner of a shared contact vertex. Boundary walks are
split at every repeated vertex so a hole touching its shell remains a separate simple ring.
Observed zero-area polygon inputs, including both zero and nonzero extents,
return an empty polygon before triangulation.

Executable GEOS frame selection can depend on a touching component's first
vertex. When its envelope is covered, a first vertex on another shell counts as
contained. The port records this behavior rather than assuming cyclic invariance.
The contact corpus includes both component orders and all three triangle starts.
Some non-tight reference outputs overlap and are invalid; only exact normalized
component/ring agreement is accepted for those observations.

Numeric limits are tested explicitly. A triangle of sites around 1e-200 makes
both executable GEOS 3.14.1 and this point triangulator report a locate failure;
Terrio returns a bounded error. Rescaling only one orientation test would not
provide scale-independent incircle, frame, distance and area calculations. The
polygon API returns the observed empty result when area underflows to zero,
before the frame calculation. STR centres use overflow-safe binary64 midpoints,
with multi-slice regressions near the largest finite coordinate and across
opposite-sign extreme bounds.

Both circle predicates use JTS 1.20's normalized binary64 determinant:
`jts_delaunay::Subdivision::in_circle` resolves point IDs for incremental site
insertion, while `jts_cdt::in_circle` receives coordinates for constrained-mesh
flips. Their arithmetic has the same precision and product order; the separate
callers preserve their own construction/flip ordering. Neither is an exact or
double-double predicate.

The polygon circle predicate deliberately retains JTS 1.20 arithmetic. At
1e90, its degree-four products can overflow and miss a circle inclusion. The
JTS executable reproduces this limit; rescaling changed six of eight captured
large-coordinate hull results away from executable GEOS. Those observations
remain regression cases; this API does not promise scale-independent predicates.

A mixed NaN/finite-distance simplification section can still flatten at extreme
coordinate magnitudes. Executable GEOS 3.14.1 does the same for the captured
four-coordinate 1e155 example. This compatibility behavior is tested explicitly;
the API does not promise a mathematical distance bound when its arithmetic
overflows.

An earlier paired performance run for shell-membership and validity indexing
retained identical normalized geometry hashes
for 32/128/512-shell tight hulls and 1,024/4,096/16,384-island tile features.
The 512-shell debug hull took 1.758s before and 1.404s after; the 16,384-island
release tile write took 1.382s before and 0.808s after. At 1,024 islands it took
0.043s before and 0.063s after, so this is evidence for pruning at larger sizes,
not a claim of universal speedup or linear overall complexity.

The subsequent hole-joining run retained identical geometry hashes for
128, 512 and 1,024 shells. At 1,024 shells the debug probe took 4.393s before
and 1.309s after; the 10,000-shell case exceeded a 20-second limit in both runs.
The sorting/visibility indexes improve the completed cases while vector splices,
join-position walks and the rest of triangulation retain scale limits.

The Python and Rust STR comparators also use overflow-safe center keys on both
axes. Negative controls reject reversed X slices and Y order near 1e308 without
collapsing distinct subnormal centers. One retained JTS ordering is consequently
classified as a difference instead of a legal tie; native GEOS comparison
classifications are unchanged.

## Validation of this replacement

The final Linux reference run used GEOS 3.14.1, GDAL 3.13.3 and JTS 1.20.0.
The fixed-seed corpus contains **1,645 observations**: 1,327 native
geometry/ordering results match exactly, 4 invalid reference hulls match exact
normalized component/ring structure, 3 STR results differ only in legal equal-key
ordering, and 311 integer validity decisions are replayed by Rust unit tests.
The separate index-panic regression adds one captured simplification observation.
Differing JTS observations remain in the fixtures; JTS itself is not assumed to
produce every GEOS compatibility outcome.

The MVT writer also passed **15 zoom comparisons** against GDAL: Natural Earth
countries at zooms 0–6 (excluding Antarctica, outside Mercator), US counties at
zooms 0–3, and synthetic invalid polygons at zooms 0–3. Feature counts agree and
per-key relative area differences were at most 1e-10 in that JTS validation run.
At that stage the deferred MakeValid code remained unchanged. The later
[independent replacement record](make-valid-implementation.md) reports current
feature-by-feature comparisons and retained pre-existing differences.

The strict public-CLI real-data run passed **66 comparisons** on Tennessee
and US counties, Las Vegas roads, Natural Earth countries and populated places.
All simplification coordinates match the GDAL reference. Hull comparisons use
the validity, area and distance requirements above. Natural Earth's mixed polygon
features are promoted to a MultiPolygon layer before comparison, preserving their
geometry instead of coercing disconnected components into holes. Group fields
are resolved case-insensitively, as GDAL resolves them.

Validation also passed workspace Clippy with warnings denied, workspace Rust
tests, the affected Python geometry/tile checks and Node geometry/tile checks,
and a fresh WASM build. The release Python wheel and source archive contain the
license notices. Changed-file pre-commit checks run before the local commit.

## Polygon coverage validation

`ops/coverage.rs` ports the coverage validation classes from JTS commit
[`3460207216f3091cd7f72d48d6ddd1b561b2b8fd`](https://github.com/locationtech/jts/tree/3460207216f3091cd7f72d48d6ddd1b561b2b8fd):
`coverage/CoverageValidator.java`, `CoveragePolygonValidator.java`, `CoverageRing.java`,
`InvalidSegmentDetector.java`, `CoveragePolygon.java`, plus
`algorithm/PolygonNodeTopology.java` and `geom/LineSegment.java` helpers.
The Eclipse Distribution License 1.0 is selected. File headers and both `NOTICE`
files retain the source copyright notices.

Rust edge IDs replace mutable Java ring references. Canonical edge groups classify
same/opposite polygon-interior sides before further interaction tests; `rstar`
indexes replace the segment mutual intersector and point-in-area indexes. Source
ring coordinates supply the invalid-edge output without constructing new vertices.

## Coverage simplification

`ops/coverage_simplify.rs` ports `CoverageSimplifier`, `CoverageRingEdges`,
`CoverageEdge`, `CoverageBoundarySegmentFinder`, `VertexRingCounter`,
`TPVWSimplifier`, `Corner`, and the default area calculation from `CornerArea`,
plus `simplify/LinkedLine` and `geom/Triangle` helpers, from the same pinned JTS
commit `3460207216f3091cd7f72d48d6ddd1b561b2b8fd`. The source header names all paths
and retains the copyright notices under the selected EDL/BSD license.

Rust ring/edge IDs replace Java object references. A global static vertex R-tree
with live-vertex flags replaces per-edge removable packed vertex indexes; the
queried coordinate sets are the same. A separate edge-envelope index preserves
the two-vertex baseline constraint. Binary heaps retain JTS's area/index ordering,
and stored neighbor IDs discard obsolete corners. The API exposes one distance
tolerance and boundary preservation, using the default unweighted triangle area
with ring retention matching the GDAL/GEOS reference.

GDAL 3.13.3 with GEOS 3.14.1 differs from the pinned JTS implementation in three
observed details: simplified rings can reach three distinct vertices (four with
closure), single-part MultiPolygons retain their geometry type, and small holes
and polygon parts are retained. The port follows those independently tested
GDAL behaviors.

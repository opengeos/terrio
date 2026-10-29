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
The separate MakeValid implementation and all code below its section marker in
`tiles/valid.rs` are excluded, including its private STR ordering helper. No
MakeValid source was used to write the new validity checker.

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

A real multipart contact case minimized to a triangle and quadrilateral. Cyclic
ring starts, reflections and component-order probes establish a behavioral
adaptation to JTS: once a component is complete, its first retained coordinate
replaces its original second coordinate as the jump-check representative. The
unprocessed components retain their original second-coordinate probes.

## Boundary of this change

**This is not a claim that Terrio is fully license-compliant.** Linework MakeValid
in `tiles/valid.rs` is still deferred for an independent replacement. Its earlier
inclusion under the JTS notice was inaccurate; the notice now explicitly separates
it. This branch does not relicense that code or change the repair API in
`ops/topology.rs` (which uses the `geo` crate).

The MVT failure predictor in `tiles/vector.rs` remains a compatibility heuristic,
not a full overlay implementation. Its clipping-envelope calculation is now a
separate, explicitly attributed JTS port. The crossing and omission policy retains
its documented limitation: the original random probes agreed with GEOS 3.14 on
97% of self-crossing and overlapping inputs. This patch does not claim universal
equivalence for that policy or replace MakeValid. Tile output comparisons exercise
the production writer separately from the geometry-kernel corpus.

The `pointcloud/filters/hexbin.rs` call site uses the replaced simplifier.
Other references to GEOS in shapefile normalization and linear referencing describe
compatibility behavior. This change does not certify every other algorithm or
transitive dependency.

## Behavioral references and replay

GEOS and GDAL are executable oracles only: their implementation source is not an
implementation reference for these replacements. JTS is both the permitted source
reference and an executable cross-check. The small Java adapter under
`scripts/reference/JtsOracle.java` calls JTS APIs; it embeds no reference
implementation. Reference libraries and build products are not committed.

`scripts/compare_jts_behavior.py` deterministically generates adversarial cases,
compares the geometry probe against GEOS, and can refresh the recorded WKB
observations in `crates/terrio/tests/data/jts/`. Its fixed seed is `0x4A5453`.
The Maven Central `jts-core-1.20.0.jar` used for executable checks has SHA256
`6a783d8f9dba3d3cf7265435f134402f63c05838aa6cbcc4297ad3a5b2842baf`.
The adapter checks the loaded JTS version before refreshing observations.
The fixtures record the reference versions and retain differing JTS observations
rather than silently treating JTS/GEOS differences as identical.

```sh
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
Validity booleans must agree exactly. These checks make no equivalence claim for
MakeValid, invalid polygon hull input, or every possible floating-point input.

Point triangulation retains JTS's subdivision traversal order. Equal-size,
equal-area erosion candidates also require the observed Linux heap order.
Hole seeds are sorted by increasing size with an independently written
median-partition/insertion sort, checked against executable reference results;
a stable seed sort produced materially different holes on regular grids.
This compatibility code does not use GEOS or C++ library implementation source.
The recorded reference is Linux GEOS 3.14.1, not a claim about every platform or
future library version.

The polygon path also preserves JTS's hole-join, ear-creation and Delaunay-flip
sequence (`triangulate/polygon/{ConstrainedDelaunayTriangulator,PolygonHoleJoiner,
PolygonNoder,PolygonEarClipper,TriDelaunayImprover}.java`, with `Tri` and
`TriangulationBuilder`), implemented in `ops/jts_cdt.rs`. Observed GEOS polygon
erosion uses the frame triangle sequence, a stack and first-exposed edges. Four
Natural Earth hull differences and a two-triangle regression isolated this order
requirement: an equivalent constrained triangle set with another construction
sequence can retain materially different fill.

## Validation of this replacement

The final Linux reference run used GEOS 3.14.1, GDAL 3.13.3 and JTS 1.20.0.
The fixed-seed corpus contains **1,554 observations**: 1,240 native
geometry/ordering results match exactly, 3 STR results differ only in legal equal-key
ordering, and 311 integer validity decisions are replayed by Rust unit tests.
The separate index-panic regression adds one captured simplification observation.
Differing JTS observations remain in the fixtures; JTS itself is not assumed to
produce every GEOS compatibility outcome.

The MVT writer also passed **15 zoom comparisons** against GDAL: Natural Earth
countries at zooms 0–6 (excluding Antarctica, outside Mercator), US counties at
zooms 0–3, and synthetic invalid polygons at zooms 0–3. Feature counts agree and
per-key relative area differences are at most 1e-10. The deferred MakeValid code
is exercised by these checks but remains unchanged.

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

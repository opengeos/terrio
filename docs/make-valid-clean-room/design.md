# Independent design: exact planar-arrangement linework repair

Written before repair code. Behavioural claims are labelled with the probe that
supports them (see exposure.md); everything else is my design choice.

## 1. Behavioural model (derived from black-box observation)

Let L be the union (as point sets) of every input ring's segments, from all
polygons of the feature jointly; empty exteriors drop their polygon (MV-05
policy), unclosed rings are closed logically, zero-length segments vanish.

1. Node L into a planar straight-line graph E (vertices = input vertices and
   all pairwise intersection points; coincident pieces dissolved into one edge).
2. Repeat with the current graph E_k:
   - ignore edges with the same face on both sides (dangles/cut edges);
   - every face boundary walk is split at repeated vertices into simple
     *minimal rings*. For a bounded face, one ring (CCW, face inside) is its
     shell; its other rings (CW) are holes **and pinched-off bays**;
   - depth(f) = number of CW minimal rings of *bounded* faces enclosing f;
   - A_k = union of bounded faces with even depth;
   - R ← R xor A_k;  E_{k+1} = E_k minus the edges of boundary(A_k);
   - stop when no edge is removed.
3. Output the polygonal R. Its rings contain every node of E lying on the
   boundary of R (input vertices, intersections, and contact nodes of
   discarded lines), intersections correctly rounded to binary64.

Evidence: all fixture cases by hand (e.g. overlap-crossing needs the second
xor step: 60 − 12 = 48; overlapping-holes needs it to add back 4: 98 + 4);
a Python hypothesis model (public Shapely primitives only) agreed with the
oracle on 400/400 random small invalid/valid inputs; the vertex-set rule agreed
exactly (binary equality of coordinates) on 353/353 random invalid inputs; the
"bay counts as hole" refinement came from the `inverted-bay` and `bay-chain`
probes, where plain face-tree nesting would be wrong.

Ring direction convention of the output follows the oracle's repaired output:
shells clockwise, holes counter-clockwise in y-up coordinates (signed-area sign).
Probe: the production encoder can keep or drop a hole that becomes degenerate
after truncation depending on input direction (14/300 random cases), while ring
start rotation never changed the result (0/300). Ring starts are therefore free;
I start each ring at its lexicographically smallest vertex for determinism.

Valid inputs: the oracle returns them unchanged; this model returns the same
point set possibly with additional integral contact nodes (e.g. where a hole
vertex touches a shell edge). Integral nodes are unchanged by truncation.

## 2. Representation and exact arithmetic

- Input coordinates widened to i64 immediately. Segment direction differences
  |d| ≤ 2^32; orientation determinants ≤ 2^65 in i128.
- Every node is a canonical rational point (X, Y, W), W > 0, gcd(X,Y,W) = 1
  (integers have W = 1). A crossing of integer segments AB, CD:
  t = cross(C−A, D−C)/cross(B−A, D−C), |num|,|den| ≤ 2^65, so
  X = Ax·den + (Bx−Ax)·num, |X|,|Y| < 2^98, W ≤ 2^65: fits i128.
  Equality/hashing on the reduced triple is exact.
- Comparing node coordinates (lexicographic rank, order along a segment)
  needs X1·W2 vs X2·W1 ≤ 2^163: done with an exact 256-bit product compare
  (own small helper, u128 limbs). No floating point is used in any predicate.
- Every graph edge is a piece of an original integer segment, so edge
  *directions* are integer vectors: angular sorting at a vertex, turn tests,
  and slopes are i128-exact.
- Point location (once, on the full arrangement) uses only integer query
  points: the lexicographically smallest vertex of a connected component of a
  union of segments is always a segment endpoint, i.e. an input vertex. The
  horizontal-ray intercept comparison at integer y (with +ε symbolic offset, a
  half-open rule ymin ≤ y < ymax and slope tiebreak) is ≤ 2^98: i128.
- Output: each coordinate X/W is converted to the nearest binary64 (ties to
  even) by exact long division. No early snapping, no epsilon merging, no f32.
  Limitation: two distinct exact nodes closer than an ulp would round to the
  same binary64; consecutive duplicates are then removed, which can only occur
  for sub-ulp features (documented, tested as defensive policy).

## 3. Algorithm

1. **Segments.** Collect non-degenerate segments from rings, canonicalise
   endpoint order, deduplicate.
2. **Candidate pairs.** Static bounding-volume hierarchy (median split, built
   by sorting) over segment boxes; each segment queries for overlapping boxes.
   Not all-pairs on sparse input; output-sensitive in the number of box
   overlaps.
3. **Exact intersection** per candidate pair (proper crossing, endpoint
   touches, collinear overlap → endpoints inside the other segment). Split
   points are sorted along each segment exactly, consecutive duplicates
   dropped, pieces emitted; identical pieces from overlapping segments
   deduplicated by vertex pair.
4. **DCEL.** Half-edges 2e, 2e+1; outgoing lists per vertex sorted CCW exactly;
   next(h) = clockwise neighbour of twin(h). Trace all walks.
5. **Atomic faces.** Union-find over vertices gives components; each
   component's outer walk is the walk left of the steepest outgoing edge at its
   minimum vertex (the wedge containing direction −x). Point location by a
   segment tree over the distinct query y values, each node holding the edges
   spanning it ordered left-to-right (valid because noded edges do not cross);
   the hit edge's downward half-edge gives the enclosing walk. Union-find over
   walks + a virtual unbounded node yields atomic faces.
6. **Iteration** on atomic faces with a persistent union-find (faces only ever
   merge across removed edges): compact effective edges per vertex, trace
   walks, split into minimal rings with a vertex-position stack, classify each
   ring by the exact turn at its minimum-rank vertex, propagate depth by BFS
   over the dual graph (Δ = cw(h) − cw(twin(h)), counting only rings of bounded
   faces), keep even-depth bounded faces, xor into per-atomic-face R bits,
   remove boundary edges. Depth inconsistency, a ring walk exceeding the
   half-edge count, or an iteration count above the edge count are internal
   invariant errors.
7. **Assembly.** Boundary half-edges of R (R on left), traced with the same
   clockwise-neighbour rule restricted to them, split into simple rings,
   classified CCW (shell) / CW (hole). Interior components of R (union-find of
   atomic faces across non-boundary edges) group exactly one shell with its
   holes. Rings are reversed to the oracle's direction convention, rounded,
   started at their smallest vertex; polygons sorted by shell start vertex.

## 4. Complexity (n input segments, k intersections, m = n + k edges)

- Candidate pairs: O(n log n + p) with p overlapping box pairs (p can be Θ(n²)
  for adversarial long segments; the real intersection count k can also be).
- Noding/sorting: O((n + k) log(n + k)) exact comparisons; memory O(n + k).
- Point location: O(m log m + c log² m) for c components; memory O(m log m).
- Iterations: each removes ≥ 1 edge, each costs O(m α(m)), so O(I·m) with I
  ≤ m; observed I is small (nesting/overlap layers). Worst case quadratic in m.
- Output linear in boundary size.
No silent cutoff is imposed (MV-08). Stress measurements are in validation.md.

**Measured after implementation:** the iteration count I is the number of
xor "layers" and is large for deliberately layered inputs (401 for 400 mutually
crossing squares/diamonds, 467 for a 2,000-vertex random ring), small for
ordinary inputs (2 for a 200k-vertex twisted ring, 3 for 50k parts). Each
iteration is O(V + alive edges + live faces) after removing buffer allocation,
so the layered cases cost O(I·m) — the documented quadratic regime.

## 5. Failure policy

`try_make_valid` returns `Result<Vec<Polygon<P>>, RepairError>`; errors mean an
internal invariant violation (a bug), never "no area". As originally proposed,
the infallible `make_valid` panicked on such an error because its signature
has no error channel.

**Update after implementation (custodian decision, notes/custodian-response.md):**
production will propagate the `Result` from `try_make_valid`. The transport now
calls `fallible_implementation()` (= `try_make_valid`) and reports an error as
`{"status":"error","message":...}`. `make_valid` remains only as the scaffold's
`Repair`-typed `implementation()` registration (compatibility wrapper, off every
production/transport path); the custodian may remove it at integration.
Allocation sizes come from input lengths only; all index arithmetic is checked
or bounded; walks are bounded by the half-edge count.

## 6. Determinism

No hash-iteration order reaches the output: vertex ids are assigned in a
sorted order; all sorts are total orders on exact values; output order is a
function of geometry. HashMap is used only for exact lookups.

## 7. Provenance

Wholly my own composition of textbook techniques (planar subdivision/DCEL,
exact rational predicates, ray-shooting point location, union-find). No GEOS,
JTS, PostGIS, GDAL or Terrio code is ported or consulted. Dependencies:
none beyond the vetted `serde_json` already used by the transport; the kernel
itself uses only `std`.

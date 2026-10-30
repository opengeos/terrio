# GEOS implementation retirement

This audit covers the geometry paths identified for replacement in Terrio,
starting from `07182d56e06f9cbf685a6f992df6411cbe99e223`. It distinguishes
implementation provenance from executable compatibility references. It is a
technical inventory, not a repository-wide legal clearance.

## Replacement inventory

| Original path | Current implementation and evidence |
| --- | --- |
| `ops/simplify.rs` | Pinned JTS 1.20.0 tagged-line simplifier and predicates; [source mapping and observations](jts-provenance.md). |
| `ops/hull.rs` | JTS convex scan and concave erosion, with JTS point and constrained polygon triangulations in `jts_delaunay.rs` and `jts_cdt.rs`. |
| `tiles/valid.rs`: validity | Separate JTS integer-grid validity checker in `tiles/validity.rs`. |
| `tiles/valid.rs`: MakeValid and private/shared helpers | Entire file removed. Independently authored `tiles/repair/` imported mechanically from the isolated implementer's delivery. [Process and validation](make-valid-implementation.md), [import ledger](make-valid-import.json). |
| `tiles/vector.rs`: clipping-envelope calculation | JTS `OverlayUtil` / `RobustClipEnvelopeComputer` port in `tiles/overlay_envelope.rs`. |
| `tiles/vector.rs`: shared box candidates | Adapter to the existing `rstar` dependency in `tiles/boxes.rs`; the retired repair's grid helper is removed. |
| `ops/vector_geom.rs`: STR packing | JTS packing in `ops/strtree.rs`, preserving all IDs with documented equal-key ambiguity. |
| `pointcloud/filters/hexbin.rs` | Calls the replaced shared simplifier; no separate simplifier implementation. |

The two implementation stages remain separate commits. The second stage includes
the complete repair retirement, independent design and exposure records, approved
input manifest, import hashes, behavioral fixtures, reproducible handoff tools
and production comparison reports. Root and crate `NOTICE` distinguish the
EDL-1.0 JTS ports from the independent MIT repair. The CLI guide, README and
acknowledgements describe the final implementation rather than the retired port.

## Remaining references and boundaries

- `tiles/vector.rs` retains its empirically tuned crossing/omission predictor.
  Its original probes reported about 97% agreement with GEOS 3.14. The predictor
  models observed clipping failures and is not a full OverlayNG port. The JTS
  clipping envelope is explicitly attributed. Current production comparisons
  separately report 25 unchanged synthetic feature-key/zoom differences; they
  are checked against frozen geometries rather than waived by identifier.
- `tiles/clip.rs` uses Sutherland-Hodgman rectangle clipping. Its GEOS reference
  explains why a clipped ring can remain one ring instead of multiple polygons.
- Shapefile ring organization describes GDAL's normalization and ring-start
  behavior; linear referencing explicitly documents independent mathematical
  projection/distance formulas and MIT GDAL adapters. No additional GEOS source
  port was identified in these paths by this inventory.
- Public `ops/topology.rs::make_valid` uses GeoRust `geo`; it is a separate API
  from the retired tile repair. It did not require replacement for this task.
- Reference collectors, conformance scripts and fixtures intentionally retain
  GEOS names, version numbers and executable calls. Production repair replay
  tests need no native GEOS installation. The optional GDAL backend invokes an
  installed external GDAL program, which may itself use GEOS.
- `+proj=geos` and the CRS module's `Geos` type mean the geostationary satellite
  projection. They are unrelated to the GEOS geometry library.

`cargo metadata --locked --offline` found **385 packages** in the current
workspace resolution, with no GEOS or `geos-sys` package. The two `r-efi`
versions advertise an LGPL alternative alongside MIT/Apache alternatives;
that license expression is not a GEOS dependency. No dependency was added by
either replacement. The freshly built Linux CLI's ELF dependencies contain
only the platform C/math/compiler runtimes, with no GEOS or GDAL library.
This is evidence about this resolution and artifact, not every future feature
selection or external program on a user's machine.

## Compatibility limits

The independent repair passed 51 captured kernel cases, 50 supported encoder
cases and 4,500 generated kernel comparisons. Seven county feature instances
have a documented difference in which polygon owns an identical collinear ring
after integer truncation. These are invalid decoded geometries, not valid
geometry matches. Oriented ring multiplicity, exact tile-grid coordinates and
all non-collinear polygon groups must agree for that classification.

Eight generated full-range `i32` inputs caused candidate-only encoder overflows
because the replacement enumerates rings differently. The production encoder
now returns an explicit error for an unrepresentable signed cursor delta. The
kernel still supports those input ordinates; reference traversal order is not
part of its contract. The largest stress inputs completed locally, but their
optional GEOS differential run was stopped without a result. The installed
Rust toolchain was 1.99; the declared 1.90 MSRV has not been exercised locally.

## Release follow-through

The known source-replacement inventory above is complete. Before distribution,
reviewers should assess both provenance records and the recorded behavior limits,
and run the repository's release dry run on supported platforms. Its packaging
workflow stages the notices into Python artifacts, CLI archives and the npm
package; the crate also carries its synchronized `NOTICE`.

Retiring source changes future builds. Existing releases and their notices need
a separate inventory of what was actually shipped and a maintainer decision on
any corrective distribution steps. This patch does not rewrite Git history,
withdraw releases or purport to settle past distribution obligations. Keep the
audit evidence and upstream JTS copyrights with future releases.

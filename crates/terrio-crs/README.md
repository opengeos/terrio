# terrio-crs

Coordinate reference systems and transformations, extracted from Terrio.
The crate includes EPSG and ESRI definitions, WKT/PROJ parsing, the documented PROJJSON v0.7 subset, datum operation
selection, coordinate epochs, and local or in-memory datum grids. Coordinates
use traditional GIS order: longitude/easting, then latitude/northing.

```rust
use terrio_crs::{SpatialRef, Transformer};

let source = SpatialRef::parse("EPSG:4326")?;
let target = SpatialRef::parse("EPSG:3857")?;
let transformer = Transformer::new(&source, &target)?;
let (x, y) = transformer.transform(-100.0, 40.0)?;
# Ok::<(), terrio_crs::Error>(())
```

## Features and supported scope

The default configuration works offline and uses the native projection engine.
In the checked-in 117-method PROJ conformance inventory it supports 40 methods;
`wbprojection-fallback` adds 46 methods, including Hammer, Aitoff and Eckert.
The inventory is a test corpus, not an exhaustive list of EPSG CRSs. The remaining
31 methods are explicitly refused. Known Krovak inverse differences off Bessel's
ellipsoid are recorded in the conformance tests.

| Feature | Default | Effect |
| --- | --- | --- |
| `wbprojection-fallback` | Off | Additional projection methods and legacy identification/WKT fallback |
| `grid-transport` | Off | Host-provided seekable grid transport, without an HTTP dependency |

Terrio enables `wbprojection-fallback` to retain its existing projection coverage.
A method requiring the feature returns an error naming the feature when it is
disabled. Canonical authority references, including versioned EPSG URNs/URLs, use the same
parser in both configurations. Permissive legacy authority fragments remain a
fallback feature. Both configurations include the same EPSG/ESRI tables, native datum
operations and local grid readers. Table decompression uses `ruzstd` on all
targets. TIFF grid codecs retain their existing features, including native Zstd;
the default crate does not claim a dependency graph free of C codecs.

Enable `grid-transport` to supply a
seekable network transport through `network::GridTransport`; Terrio supplies
its existing HTTP range reader. Registered grids, configuration and decoded
grid caches are shared between Terrio and direct users of this crate.

Warnings are collected on the calling thread and retrieved with `take_warnings()`.
Embedding applications may route a transformer's diagnostics with
`Transformer::with_warning_handler`.

The algorithms and bundled data retain their attribution and terms in `NOTICE`.
The library is maintained in the [Terrio workspace](https://github.com/opengeos/terrio).

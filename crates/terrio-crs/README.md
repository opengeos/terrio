# terrio-crs

Coordinate reference systems and transformations, extracted from Terrio.
The crate includes EPSG, ESRI and IAU_2015 definitions, WKT/PROJ parsing, the documented PROJJSON v0.7 subset, datum operation
selection, coordinate epochs, and local or in-memory datum grids. Coordinates
use GDAL's traditional GIS mapping: longitude/easting, then latitude/northing
for ordinary Earth CRSs. Planetary west-positive systems retain their registry
mapping; for example, IAU_2015:49901 uses latitude then west-positive longitude,
and its projected CRS 49911 uses westing then northing.

```rust
use terrio_crs::{SpatialRef, Transformer};

let source = SpatialRef::parse("EPSG:4326")?; // or parse_literal to refuse file paths
let target = SpatialRef::parse("EPSG:3857")?;
let transformer = Transformer::new(&source, &target)?;
let (x, y) = transformer.transform(-100.0, 40.0)?;
# Ok::<(), terrio_crs::Error>(())
```

`list_crs` lists the EPSG (v12.029), ESRI and IAU_2015 CRSs the crate resolves, with their
names, kinds, deprecation, areas of use and projection methods, for searchable CRS pickers:

```rust
use terrio_crs::{CrsQuery, list_crs};

let found = list_crs(&CrsQuery { text: Some("australian albers".into()), ..Default::default() });
assert_eq!(found[0].authority_code(), "EPSG:3577");
```

## Features and supported scope

The default configuration works offline and uses the native projection engine.
In the checked-in 117-method PROJ conformance inventory it supports 45 methods,
including Eckert IV and Natural Earth; `wbprojection-fallback` adds 44 methods, including
Hammer, Aitoff and the other Eckerts.
The inventory is a test corpus, not an exhaustive list of EPSG CRSs. The remaining
28 methods are explicitly refused. Known Krovak inverse differences off Bessel's
ellipsoid are recorded in the conformance tests.

| Feature | Default | Effect |
| --- | --- | --- |
| `wbprojection-fallback` | Off | Additional projection methods and legacy identification/WKT fallback |
| `grid-transport` | Off | Host-provided seekable grid transport, without an HTTP dependency |
| `libzstd` | Off | Zstandard TIFF grids, with the zstd C library |

Terrio enables `wbprojection-fallback` to retain its existing projection coverage.
A method requiring the feature returns an error naming the feature when it is
disabled. Canonical authority references, including versioned EPSG URNs/URLs, use the same
parser in both configurations. Permissive legacy authority fragments remain a
fallback feature. Both configurations include the same EPSG/ESRI/IAU tables, native datum
operations and local grid readers. Table decompression uses `ruzstd` on all
targets. The default crate has no C dependency: Zstandard TIFF grids are read only with the
`libzstd` feature (PROJ's grids are DEFLATE-compressed).

Enable `grid-transport` to supply a
seekable network transport through `network::GridTransport`; Terrio supplies
its existing HTTP range reader. Registered grids, configuration and decoded
grid caches are shared between Terrio and direct users of this crate.

Warnings are collected on the calling thread and retrieved with `take_warnings()`.
Embedding applications may route a transformer's diagnostics with
`Transformer::with_warning_handler`.

The algorithms and bundled data retain their attribution and terms in `NOTICE`.
The library is maintained in the [Terrio workspace](https://github.com/opengeos/terrio).

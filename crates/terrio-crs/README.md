# terrio-crs

Coordinate reference systems and transformations, extracted from Terrio.
The crate includes EPSG and ESRI definitions, WKT/PROJ parsing, datum operation
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

The default configuration works offline. Enable `grid-transport` to supply a
seekable network transport through `network::GridTransport`; Terrio supplies
its existing HTTP range reader. Registered grids, configuration and decoded
grid caches are shared between Terrio and direct users of this crate.

Warnings are collected on the calling thread and retrieved with `take_warnings()`.
Embedding applications may route a transformer's diagnostics with
`Transformer::with_warning_handler`.

The algorithms and bundled data retain their attribution and terms in `NOTICE`.
The library is maintained in the [Terrio workspace](https://github.com/opengeos/terrio).

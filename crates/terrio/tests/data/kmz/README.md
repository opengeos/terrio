# KMZ cross-document selection names

`cross_document_names.kmz` is a synthetic regression fixture for issue #787.
`doc.kml` contains folders named `roads (2)` and `roads`, with points (1,46)
and (2,46), then links to `layers/a.kml` under the name `roads`. That linked
document contains point (3,46) at its root and a `roads` folder with point
(4,46). Each placemark has name `point X` for its X coordinate.

The dataset names must be `roads (2)`, `roads`, `roads (3)`, `roads (4)` in
that order. The global suffix rule must run once after assembling documents;
applying it separately first would incorrectly produce a nested suffix.
The Python test generates this structure directly, alongside other collision
and link-order controls. This archived copy exercises the WASM byte reader.

`case_variant_names.kmz` is the ASCII case-variant counterpart. Its expected
layer names are `Roads (2)`, `Roads`, `roads (3)` and `ROADS (4)`. Their point
coordinates are `(1, 46)` through `(4, 46)`, respectively, as asserted by the
Node/WASM byte-reader test.

# Shapefile projection-encoding fixtures

`prj_latin1.zip` and `prj_utf16.zip` contain three 2D points (10,46),
(11,46), (12,46), with integer `id` values 0, 1, 2. The SHP, SHX and DBF
were generated with GDAL 3.13.3. Only `pts.prj` differs:

- Latin-1 WKT has the non-ASCII CRS name `Réseau local` and a transverse
  Mercator definition with central meridian 55.5 degrees.
- UTF-16 WKT has a byte-order mark and GDAL's ESRI WGS 84 definition.

These are regression fixtures for unreadable UTF-8 metadata, not a claim
that the arbitrary sample coordinates belong to the custom projected CRS.
Terrio must warn with the sidecar name and decoding reason, retain the
three geometries and attributes, and leave the CRS absent. Valid UTF-8,
empty and missing sidecars are tested separately in Python.

`prj_bad_crc.zip` has the same geometry/attributes and valid UTF-8 WGS 84
projection text, but both stored CRC fields for `pts.prj` are deliberately
wrong. Native extraction retains its fatal checksum-error behavior. The
WASM memory codec must diagnose the failed optional sidecar read instead
of treating it as absent without a warning. Resource-limit refusals remain
fatal on both paths.

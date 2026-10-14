# Coordinate Systems

## Defining a CRS

Everywhere a CRS is expected (`--t-srs`, `--a-srs`, `terrio crs`, the APIs), terrio accepts:

- EPSG codes: `EPSG:32613`, `32613`, `urn:ogc:def:crs:EPSG::32613`,
  `http://www.opengis.net/def/crs/EPSG/0/32613`;
- `OGC:CRS84` and `WGS84`;
- WKT1 and WKT2 definitions, including ESRI `.prj` WKT;
- PROJ strings: `+proj=utm +zone=13 +datum=WGS84`;
- the path of a file holding one of these.

Coordinates are always in GIS order: x (easting, longitude) first, then y (northing, latitude),
whatever the axis order the authority defines.

## Identification

When a dataset stores WKT without an EPSG code, as shapefiles do, terrio identifies the code. When
several codes match equally, it prefers the base datum over its later realizations (NAD83 over
NAD83(HARN) and NAD83(2011)) unless the WKT names one, then the lowest code. This is what GDAL
reports: the New York City boroughs shapefile is EPSG:2263 in both.

GeoTIFFs with user-defined projected CRSs, common in NASA products, are read from their GeoKeys:
UTM zones become their EPSG codes, and other projections (Albers, Lambert conformal conic,
transverse Mercator, polar stereographic and more) become PROJ definitions.

## Transformations

```sh
terrio crs EPSG:4326 --to EPSG:32613 -105.5 39.5
```

NAD83 and WGS 84 are treated as equivalent, as GDAL's default operations do. Bounds are
transformed with densified edges, so curved edges in the target CRS are covered.

## Attribution

EPSG definitions come from the [EPSG Geodetic Parameter Dataset](https://epsg.org), through the
`wbprojection` crate and PROJ.

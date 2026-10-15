# Notebooks

Jupyter notebooks that use the [Python package](python.md) on real data. Open one in Google Colab
to run it without installing anything, or download it and run it locally after
`pip install terrio matplotlib`.

## Quickstart

[![Open in Colab](https://colab.research.google.com/assets/colab-badge.svg)](https://colab.research.google.com/github/opengeos/terrio/blob/main/docs/notebooks/quickstart.ipynb)
[View on GitHub](https://github.com/opengeos/terrio/blob/main/docs/notebooks/quickstart.ipynb) ·
[Download](notebooks/quickstart.ipynb)

A tour of terrio in Python:

- a USGS 3DEP elevation COG on Amazon S3, read in place: its header, an overview, and a
  full-resolution window written to a GeoTIFF with statistics;
- hillshade and slope with `terrio.dem`, as `gdaldem` computes them;
- reprojection to UTM with `terrio.warp`;
- NDVI from a Landsat 7 image with `terrio.calc`;
- the world's countries from a GeoJSON URL: filtered with SQL, reprojected, and written as
  GeoPackage, FlatGeobuf, GeoParquet, Shapefile and KML;
- rasterizing the countries, and transforming coordinates between EPSG CRSs.

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

## xarray

[![Open in Colab](https://colab.research.google.com/assets/colab-badge.svg)](https://colab.research.google.com/github/opengeos/terrio/blob/main/docs/notebooks/xarray.ipynb)
[View on GitHub](https://github.com/opengeos/terrio/blob/main/docs/notebooks/xarray.ipynb) ·
[Download](notebooks/xarray.ipynb)

[`terrio.xarray`](xarray.md), rioxarray's API without a GDAL installation (`pip install "terrio[xarray]"`):

- the USGS 3DEP elevation COG opened lazily as a dask-backed DataArray, with rioxarray's
  coordinates, attributes and `spatial_ref` grid mapping;
- a window clipped by longitude and latitude (only its tiles are fetched), an overview, a
  reprojection to UTM, a clip to a polygon, two tiles merged;
- a Landsat image through xarray's backend, `xr.open_dataset(url, engine="terrio")`;
- writing a COG and GeoZarr with `to_raster`, and xarray's `to_zarr` keeping the georeferencing;
- `register_rio_accessor()`, for code written for rioxarray.

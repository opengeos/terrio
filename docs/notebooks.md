# Notebooks

Jupyter notebooks that use the [Python package](python.md) on real data. Run them in your browser
below, open one in Google Colab to run it without installing anything, or download it and run it
locally after `pip install terrio matplotlib`.

## In your browser

[JupyterLite](https://jupyterlite.readthedocs.io) runs Python and the latest terrio release in
your browser, compiled to WebAssembly, with nothing to install: choose **Run > Run All Cells**. The
first run downloads Python and its packages. Edits you save stay in this browser, so download the
notebook to keep them elsewhere.

<div markdown="0">
  <p>
    <a href="lite/lab/index.html?path=quickstart.ipynb" target="_blank" rel="noopener">Quickstart in a new tab</a> ·
    <a href="lite/lab/index.html?path=xarray.ipynb" target="_blank" rel="noopener">xarray in a new tab</a>
  </p>
  <iframe src="lite/notebooks/index.html?path=quickstart.ipynb" title="terrio quickstart in JupyterLite" loading="lazy" style="width: 100%; height: 700px; border: 1px solid var(--md-default-fg-color--lightest);"></iframe>
</div>

These notebooks are adapted for the browser, where terrio does not open URLs: they download smaller
samples first, so they do not show HTTP range reads of remote files. The notebooks below do.

## Quickstart notebook

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

## xarray notebook

[![Open in Colab](https://colab.research.google.com/assets/colab-badge.svg)](https://colab.research.google.com/github/opengeos/terrio/blob/main/docs/notebooks/xarray.ipynb)
[View on GitHub](https://github.com/opengeos/terrio/blob/main/docs/notebooks/xarray.ipynb) ·
[Download](notebooks/xarray.ipynb)

[`terrio.xarray`](xarray.md), rioxarray's API without installing GDAL
(`pip install "terrio[xarray]"`):

- the USGS 3DEP elevation COG opened lazily as a dask-backed DataArray, with rioxarray's
  coordinates, attributes and `spatial_ref` grid mapping;
- a window clipped by longitude and latitude (only its tiles are fetched), an overview, a
  reprojection to UTM, a clip to a polygon, two tiles merged;
- a Landsat image through xarray's backend, `xr.open_dataset(url, engine="terrio")`;
- writing a COG and GeoZarr with `to_raster`, and xarray's `to_zarr` keeping the georeferencing;
- `register_rio_accessor()`, for code written for rioxarray.

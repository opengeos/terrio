# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     docs/topics/masking-by-shapefile.rst

"""Masking a raster with polygons, after rasterio's "Masking a raster using a shapefile" guide.

Usage: python mask_by_polygon.py DATA_DIR OUT_DIR
"""

import json
import sys

import rasterio
import rasterio.mask
from rasterio.warp import transform_geom

data_dir, out_dir = sys.argv[1], sys.argv[2]

with open(f"{data_dir}/las_vegas_bounds.geojson") as f:
    collection = json.load(f)

with rasterio.open(f"{data_dir}/las_vegas_dem.tif") as src:
    shapes = [transform_geom("EPSG:4326", src.crs, feature["geometry"]) for feature in collection["features"]]
    out_image, out_transform = rasterio.mask.mask(src, shapes, crop=True)
    out_meta = src.meta

out_meta.update({"driver": "GTiff", "height": out_image.shape[1], "width": out_image.shape[2], "transform": out_transform})

with rasterio.open(f"{out_dir}/masked.tif", "w", **out_meta) as dest:
    dest.write(out_image)

print(out_image.shape, out_transform)

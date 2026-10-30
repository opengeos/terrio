# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     docs/api/rasterio.merge.rst

"""Merging tiles into a mosaic, after rasterio's merge example.

Usage: python merge_tiles.py DATA_DIR OUT_DIR
"""

import sys

import rasterio
from rasterio.merge import merge

data_dir, out_dir = sys.argv[1], sys.argv[2]
# usgs_dem.tif is the n40w106 tile.
names = ["usgs_dem.tif", "USGS_1_n40w107.tif", "USGS_1_n41w106.tif", "USGS_1_n41w107.tif"]
sources = [rasterio.open(f"{data_dir}/{n}") for n in names]

mosaic, out_trans = merge(sources, res=0.001)

out_meta = sources[0].meta.copy()
out_meta.update({"driver": "GTiff", "height": mosaic.shape[1], "width": mosaic.shape[2], "transform": out_trans})
with rasterio.open(f"{out_dir}/mosaic.tif", "w", **out_meta) as dest:
    dest.write(mosaic)
for src in sources:
    src.close()
print(mosaic.shape, out_trans)

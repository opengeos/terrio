# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     docs/topics/windowed-rw.rst

"""Windowed reading and writing, after rasterio's "Windowed reading and writing" guide.

Processes a Landsat scene block by block and writes the result with the source profile.

Usage: python windowed_rw.py DATA_DIR OUT_DIR
"""

import sys

import numpy as np

import rasterio
from rasterio.windows import Window

data_dir, out_dir = sys.argv[1], sys.argv[2]

with rasterio.open(f"{data_dir}/landsat.tif") as src:
    profile = src.profile
    profile.update(dtype=rasterio.float32, count=1, compress="lzw", nodata=-1.0)
    with rasterio.open(f"{out_dir}/ndvi.tif", "w", **profile) as dst:
        for ji, window in src.block_windows(1):
            red = src.read(3, window=window).astype("float32")
            nir = src.read(4, window=window).astype("float32")
            total = red + nir
            ndvi = np.where(total > 0, (nir - red) / np.where(total > 0, total, 1), -1.0)
            dst.write(ndvi.astype("float32"), 1, window=window)
    # A window by row and column ranges, and one by bounds.
    w = src.read(1, window=Window(100, 200, 64, 32))
    left, bottom, right, top = src.window_bounds(Window(100, 200, 64, 32))
    print(w.shape, w.sum(), left, bottom, right, top)
    win = src.window(left, bottom, right, top)
    print(win, src.read(1, window=win).sum())

with rasterio.open(f"{out_dir}/ndvi.tif") as ndvi:
    print(ndvi.profile["blockxsize"], ndvi.profile["blockysize"], ndvi.profile["tiled"])
    print(round(float(ndvi.read(1).mean()), 6))

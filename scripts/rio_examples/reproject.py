# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     docs/topics/reproject.rst

"""Reprojecting a raster, after rasterio's "Reprojection" guide.

Usage: python reproject.py DATA_DIR OUT_DIR
"""

import sys

import rasterio
from rasterio.warp import Resampling, calculate_default_transform, reproject

data_dir, out_dir = sys.argv[1], sys.argv[2]
dst_crs = "EPSG:4326"

with rasterio.open(f"{data_dir}/OPERA_L3_DSWx_WTR.tif") as src:
    transform, width, height = calculate_default_transform(
        src.crs, dst_crs, src.width, src.height, *src.bounds
    )
    kwargs = src.meta.copy()
    kwargs.update({"crs": dst_crs, "transform": transform, "width": width, "height": height})

    with rasterio.open(f"{out_dir}/reprojected.tif", "w", **kwargs) as dst:
        for i in range(1, src.count + 1):
            reproject(
                source=rasterio.band(src, i),
                destination=rasterio.band(dst, i),
                src_transform=src.transform,
                src_crs=src.crs,
                dst_transform=transform,
                dst_crs=dst_crs,
                resampling=Resampling.nearest,
            )
    print(width, height)

with rasterio.open(f"{out_dir}/reprojected.tif") as r:
    print(r.crs.to_epsg(), r.width, r.height, r.nodata)

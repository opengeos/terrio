# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause
#
# Derived from (adapted and modified):
#   rasterio 1.5.2:
#     docs/quickstart.rst

"""Reading a dataset, after rasterio's quickstart.

Usage: python read_dem.py DATA_DIR OUT_DIR
"""

import json
import sys

import rasterio

data_dir, out_dir = sys.argv[1], sys.argv[2]

with rasterio.open(f"{data_dir}/srtm_sub.tif") as dataset:
    print(dataset.name.split("/")[-1], dataset.mode, dataset.count, dataset.width, dataset.height)
    print(dataset.bounds)
    print(dataset.transform)
    print(dataset.crs.to_epsg(), dataset.indexes, dataset.dtypes, dataset.nodata)
    band1 = dataset.read(1, masked=True)
    print("min", band1.min(), "max", band1.max(), "mean", round(float(band1.mean()), 6))
    x, y = dataset.xy(dataset.height // 2, dataset.width // 2)
    row, col = dataset.index(x, y)
    print("centre", x, y, row, col, band1[row, col])
    print("upper left", dataset.transform * (0, 0))
    print("lower right", dataset.transform * (dataset.width, dataset.height))
    with open(f"{out_dir}/read_dem.json", "w") as f:
        json.dump({"profile": {k: str(v) for k, v in dataset.profile.items()}, "tags": dataset.tags()}, f)

"""Writes `crates/terrio/tests/data/netcdf_order/definition_order.nc` and prints GDAL's subdatasets.

Usage:
    python scripts/make_netcdf_order_fixtures.py

A netCDF-4 file whose variables and groups are defined in reverse name order, alternating
variables with a `_FillValue` and variables with netCDF's default fill value, whose object headers
then do not follow their definition order. GDAL lists the subdatasets in definition order (netCDF's
variable ids); the printed list is the expected order of the `netcdf4_definition_order` test.
Needs netCDF4 and rasterio.
"""

from __future__ import annotations

from pathlib import Path

import netCDF4
import numpy as np
import rasterio

OUT = Path(__file__).resolve().parent.parent / "crates" / "terrio" / "tests" / "data" / "netcdf_order"
# name: fill value, None for netCDF's default one.
VARIABLES = {"zulu": -1, "yankee": None, "xray": -1, "whiskey": None, "victor": -1}


def define(group: netCDF4.Dataset | netCDF4.Group) -> None:
    """Defines the variables of `VARIABLES` in `group`, in their order."""
    for name, fill in VARIABLES.items():
        fill_value = None if fill is None else np.int16(fill)
        group.createVariable(name, "i2", ("y", "x"), fill_value=fill_value)[:] = np.arange(4, dtype="i2").reshape(2, 2)


def main() -> None:
    """Writes the file and prints GDAL's subdataset names."""
    OUT.mkdir(parents=True, exist_ok=True)
    path = OUT / "definition_order.nc"
    with netCDF4.Dataset(path, "w", format="NETCDF4") as nc:
        nc.createDimension("y", 2)
        nc.createDimension("x", 2)
        define(nc)
        for name in ("sierra", "romeo"):
            define(nc.createGroup(name))
    print(f"GDAL {rasterio.__gdal_version__}")
    with rasterio.open(path) as ds:
        # By number: the metadata come back in key order (1, 10, 11, ..., 2).
        tags = ds.tags(ns="SUBDATASETS")
        for i in range(1, len(tags) // 2 + 1):
            print(tags[f"SUBDATASET_{i}_NAME"].rsplit(":", 1)[-1])


if __name__ == "__main__":
    main()

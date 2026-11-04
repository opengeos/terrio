"""Writes `crates/terrio/tests/data/netcdf_int64_unsigned/ranges.nc` and prints what GDAL reads of it.

Usage:
    python scripts/make_netcdf_int64_unsigned_fixtures.py

The file holds `Int64` variables flagged `_Unsigned = "true"`, which GDAL reads as `Int64`, one with
its `_FillValue` only and one with an inverted `valid_range` (`[10, -10]`), and `Short` variables
with an inverted `valid_range` or `valid_min` and `valid_max`, which GDAL ignores. The printed
values are the pixels GDAL (through rasterio) reads, the expected values of the
`netcdf_int64_unsigned_and_inverted_ranges` test. Needs netCDF4 and rasterio.
"""

from __future__ import annotations

from pathlib import Path

import netCDF4
import numpy as np
import rasterio

OUT = (
    Path(__file__).resolve().parent.parent
    / "crates"
    / "terrio"
    / "tests"
    / "data"
    / "netcdf_int64_unsigned"
)
VALUES = [[5, 10, 1000, -10], [-9, -2, -1, 30000]]
# name: (type, attributes)
VARIABLES = {
    "int64_unsigned": ("i8", {"_Unsigned": "true"}),
    "int64_unsigned_inverted_range": (
        "i8",
        {"_Unsigned": "true", "valid_range": np.array([10, -10], dtype="i8")},
    ),
    "short_inverted_range": ("i2", {"valid_range": np.array([10, -10], dtype="i2")}),
    "short_inverted_min_max": (
        "i2",
        {"valid_min": np.int16(10), "valid_max": np.int16(-10)},
    ),
}


def main() -> None:
    """Writes the file and prints GDAL's pixels of each variable."""
    OUT.mkdir(parents=True, exist_ok=True)
    path = OUT / "ranges.nc"
    with netCDF4.Dataset(path, "w", format="NETCDF4") as nc:
        nc.createDimension("y", len(VALUES))
        nc.createDimension("x", len(VALUES[0]))
        for name, (dtype, attrs) in VARIABLES.items():
            var = nc.createVariable(
                name, dtype, ("y", "x"), fill_value=np.array(-1, dtype=dtype)
            )
            var.set_auto_maskandscale(False)
            for key, value in attrs.items():
                var.setncattr(key, value)
            var[:] = np.array(VALUES, dtype=dtype)
    print(f"GDAL {rasterio.__gdal_version__}")
    for name in VARIABLES:
        with rasterio.open(f'NETCDF:"{path}":{name}') as ds:
            print(name, ds.dtypes[0], ds.nodata, ds.read(1).ravel().tolist())


if __name__ == "__main__":
    main()

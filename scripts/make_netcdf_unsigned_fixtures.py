"""Writes `crates/terrio/tests/data/netcdf_unsigned/valid_range.nc` and prints what GDAL reads of it.

Usage:
    python scripts/make_netcdf_unsigned_fixtures.py

The file holds two `Short` variables flagged `_Unsigned = "true"` with `_FillValue = -1`, as GOES
ABI L2 products are: `valid_range` holds `[9200, -9536]` (unsigned 9200 to 56000) and the other
variable the same bounds as `valid_min` and `valid_max`. Their values lie below, on, inside and
above the unsigned range, and on the fill value. The pixels GDAL (through rasterio) reads are
printed and written to `expected.json`, the expected values of the `netcdf_unsigned_valid_range`
test. Needs netCDF4 and rasterio.
"""

from __future__ import annotations

import json
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
    / "netcdf_unsigned"
)
# The signed bits of unsigned values below (0, 1, 9199), on (9200, 56000), inside (9201, 20000,
# 32197, 32769) and above (56001, 65534) the range, and of the fill value (65535).
VALUES = np.array(
    [[9199, 9200, 32197, -32767], [-9536, -9535, -2, -1], [20000, 0, 1, 9201]],
    dtype="int16",
)


def main() -> None:
    """Writes the file and prints GDAL's pixels of each variable."""
    OUT.mkdir(parents=True, exist_ok=True)
    path = OUT / "valid_range.nc"
    with netCDF4.Dataset(path, "w", format="NETCDF4_CLASSIC") as nc:
        nc.createDimension("y", VALUES.shape[0])
        nc.createDimension("x", VALUES.shape[1])
        for name in ("valid_range", "valid_min_max"):
            # Packed as GDAL and netCDF-C read `_Unsigned` data: the attributes in the variable's type.
            var = nc.createVariable(name, "i2", ("y", "x"), fill_value=np.int16(-1))
            var.set_auto_maskandscale(False)
            var.setncattr("_Unsigned", "true")
            if name == "valid_range":
                var.setncattr("valid_range", np.array([9200, -9536], dtype="int16"))
            else:
                var.setncattr("valid_min", np.int16(9200))
                var.setncattr("valid_max", np.int16(-9536))
            var[:] = VALUES
    print(f"GDAL {rasterio.__gdal_version__}")
    expected = {}
    for name in ("valid_range", "valid_min_max"):
        with rasterio.open(f'NETCDF:"{path}":{name}') as ds:
            pixels = ds.read(1).ravel().tolist()
            expected[name] = pixels
            print(name, ds.dtypes[0], ds.nodata, pixels)
    (OUT / "expected.json").write_text(json.dumps(expected, indent=1) + "\n")


if __name__ == "__main__":
    main()

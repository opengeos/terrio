"""Writes the files of `crates/terrio/tests/data/netcdf_order` and GDAL's subdatasets of them.

Usage:
    python scripts/make_netcdf_order_fixtures.py

`definition_order.nc` is a netCDF-4 file whose variables and groups are defined in reverse name
order, alternating variables with a `_FillValue` and variables with netCDF's default fill value,
whose object headers then do not follow their definition order. GDAL lists them in definition
order (netCDF's ids, the creation order of the HDF5 links).

`untracked_order.nc` is an HDF5 file written without link creation order (h5py's default) in the
latest file format: the root group holds enough variables and groups for HDF5 to store their links
densely, by name hash, and each subgroup two variables, stored compactly in definition order. GDAL
lists them in name order, as netCDF lists the links of a group that does not track their creation
order.

Classic, 64-bit-offset and CDF5 fixtures additionally pin unchanged definition order.

`expected.json` holds, for each netCDF-4 file, the subdataset names GDAL lists, the expected order of the
`netcdf4_variable_order` test. Needs netCDF4, h5py and GDAL Python bindings.
"""

from __future__ import annotations

import json
from pathlib import Path

import h5py
import netCDF4
import numpy as np
from osgeo import gdal

OUT = Path(__file__).resolve().parent.parent / "crates" / "terrio" / "tests" / "data" / "netcdf_order"
# name: fill value, None for netCDF's default one.
VARIABLES = {"zulu": -1, "yankee": None, "xray": -1, "whiskey": None, "victor": -1}
# Defined in neither name nor hash order; more than 8 links each, which HDF5 stores densely.
UNTRACKED_VARIABLES = "quebec kilo zulu alpha mike bravo echo xray golf lima delta oscar".split()
UNTRACKED_GROUPS = "sierra romeo hotel india papa charlie foxtrot juliet november".split()
VALUES = np.arange(4, dtype="i2").reshape(2, 2)


def define(group: netCDF4.Dataset | netCDF4.Group) -> None:
    """Defines the variables of `VARIABLES` in `group`, in their order."""
    for name, fill in VARIABLES.items():
        fill_value = None if fill is None else np.int16(fill)
        group.createVariable(name, "i2", ("y", "x"), fill_value=fill_value)[:] = VALUES


def write_definition_order(path: Path) -> None:
    """Writes `definition_order.nc`."""
    with netCDF4.Dataset(path, "w", format="NETCDF4") as nc:
        nc.createDimension("y", 2)
        nc.createDimension("x", 2)
        define(nc)
        for name in ("sierra", "romeo"):
            define(nc.createGroup(name))


def write_untracked_order(path: Path) -> None:
    """Writes `untracked_order.nc`."""
    with h5py.File(path, "w", track_order=False, libver="latest") as f:
        for name in UNTRACKED_VARIABLES:
            f.create_dataset(name, data=VALUES)
        for name in UNTRACKED_GROUPS:
            group = f.create_group(name, track_order=False)
            for variable in ("zulu", "alpha"):
                group.create_dataset(variable, data=VALUES)


CLASSIC_FORMATS = [
    ("classic_order.nc", "NETCDF3_CLASSIC"),
    ("offset64_order.nc", "NETCDF3_64BIT_OFFSET"),
    ("cdf5_order.nc", "NETCDF3_64BIT_DATA"),
]


def write_classic_order(path: Path, file_format: str) -> None:
    """Write several classic variables in definition order rather than name order."""
    with netCDF4.Dataset(path, "w", format=file_format) as nc:
        nc.createDimension("y", 2)
        nc.createDimension("x", 2)
        for index, name in enumerate(("zulu", "alpha", "middle")):
            nc.createVariable(name, "i2", ("y", "x"))[:] = VALUES + index


def subdatasets(path: Path) -> list[str]:
    """GDAL's subdataset names of `path`, without the file part."""
    gdal.UseExceptions()
    with gdal.Open(str(path)) as ds:
        return [name.rsplit(":", 1)[-1] for name, _ in ds.GetSubDatasets()]


def main() -> None:
    """Writes the files and `expected.json`."""
    OUT.mkdir(parents=True, exist_ok=True)
    expected = {}
    for name, write in [("definition_order.nc", write_definition_order), ("untracked_order.nc", write_untracked_order)]:
        write(OUT / name)
        expected[name] = subdatasets(OUT / name)
        (OUT / f"{name}.aux.xml").unlink(missing_ok=True)
    for name, file_format in CLASSIC_FORMATS:
        path = OUT / name
        write_classic_order(path, file_format)
        # GDAL 3.13.3 opens classic and 64-bit-offset files but does not recognize this CDF5
        # fixture. Its definition-order regression uses netCDF variable IDs independently.
        if file_format != "NETCDF3_64BIT_DATA":
            assert subdatasets(path) == ["zulu", "alpha", "middle"]
        (OUT / f"{name}.aux.xml").unlink(missing_ok=True)
    print(gdal.VersionInfo("--version"))
    (OUT / "expected.json").write_text(json.dumps(expected, indent=1) + "\n")


if __name__ == "__main__":
    main()

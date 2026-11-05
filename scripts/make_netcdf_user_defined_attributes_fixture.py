"""Writes `crates/terrio/tests/data/netcdf_user_defined_attributes.nc`.

Usage:
    python scripts/make_netcdf_user_defined_attributes_fixture.py

A NetCDF-4 file with a variable `grid`, and attributes of user-defined types added with h5py. On
the root: `pairs` (compound), and `mixed` (compound with an enum field, a 2 x 2 array field and a
field of three chars). On `grid`: `state` (enum), `blob` (opaque) and `counts` (variable-length).
Needs netCDF4 and h5py.
"""

from __future__ import annotations

from pathlib import Path

import h5py
import netCDF4
import numpy as np

OUT = Path(__file__).resolve().parent.parent / "crates" / "terrio" / "tests" / "data"
PATH = OUT / "netcdf_user_defined_attributes.nc"


def main() -> None:
    with netCDF4.Dataset(PATH, "w", format="NETCDF4") as f:
        f.createDimension("y", 2)
        f.createDimension("x", 3)
        grid = f.createVariable("grid", "f4", ("y", "x"))
        grid[:] = np.arange(6, dtype="f4").reshape(2, 3)
        f.title = "user-defined attributes"
        grid.units = "m"

    state = h5py.enum_dtype({"wet": 1, "dry": 2}, basetype="i1")
    with h5py.File(PATH, "r+") as f:
        f.attrs.create("pairs", np.array([(1.5, 7), (2.5, -8)], dtype=[("x", "<f8"), ("n", "<i4")]))
        mixed = np.dtype([("kind", state), ("xy", "<i4", (2, 2)), ("tag", "S1", (3,))])
        f.attrs.create("mixed", np.array([(2, [[1, 2], [3, 4]], [b"a", b"b", b"c"])], dtype=mixed))
        f["grid"].attrs.create("state", 2, dtype=state)
        f["grid"].attrs.create("blob", np.void(b"\x01\x02\x03"))
        counts = [np.array([1, 2], dtype="i4"), np.array([3], dtype="i4")]
        f["grid"].attrs.create("counts", counts, dtype=h5py.vlen_dtype("i4"))


if __name__ == "__main__":
    main()

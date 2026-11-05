"""Writes `crates/terrio/tests/data/netcdf_user_defined/user_defined_attrs.nc` and what GDAL reads of it.

Usage:
    python scripts/make_netcdf_user_defined_fixture.py

The file, generated from `user_defined_attrs.cdl` by `ncgen -4`, holds attributes of user-defined
types: compounds (one with a 2-D array field, one with enum, ubyte and char fields), an enum and an
int vlen. The attributes `gdalmdiminfo` prints and the metadata `gdalinfo -json` reports are
written to `expected.json`, the expected values of the `netcdf_user_defined_attributes_match_gdal`
test, with the GDAL version that produced them. `zero_length_attrs.h5`, written with h5py, holds
zero-length compound, enum, int, short, float, double and string attributes, which `ncgen` cannot
write, and a `units` string; `gdalmdiminfo` prints each zero-length one as `null`. Needs `ncgen`, `gdalmdiminfo` and `gdalinfo` on `PATH`, and h5py and numpy.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

import h5py
import numpy as np

OUT = Path(__file__).resolve().parent.parent / "crates" / "terrio" / "tests" / "data" / "netcdf_user_defined"


def run_json(*args: str) -> dict:
    """Runs a GDAL utility and parses its JSON output.

    Args:
        *args: The command and its arguments.

    Returns:
        The parsed JSON document.
    """
    return json.loads(subprocess.run(args, check=True, capture_output=True, text=True).stdout)


def main() -> None:
    """Regenerates the fixture and its `expected.json`."""
    nc = OUT / "user_defined_attrs.nc"
    subprocess.run(["ncgen", "-4", "-o", str(nc), str(OUT / "user_defined_attrs.cdl")], check=True)
    h5 = OUT / "zero_length_attrs.h5"
    with h5py.File(h5, "w") as f:
        z = f.create_dataset("z", data=np.zeros((2, 3), "f4"))
        z.attrs.create("empty", np.zeros(0, np.dtype([("id", "<i4"), ("elevation", "<f8")])))
        z.attrs.create("ints", np.zeros(0, "<i4"))
        z.attrs.create("shorts", np.zeros(0, "<i2"))
        z.attrs.create("floats", np.zeros(0, "<f4"))
        z.attrs.create("doubles", np.zeros(0, "<f8"))
        z.attrs.create("strings", np.zeros(0, h5py.string_dtype()))
        z.attrs.create("states", np.zeros(0, "i1"), dtype=h5py.enum_dtype({"off": 0, "on": 1}, basetype="i1"))
        z.attrs["units"] = "m"
    mdim = run_json("gdalmdiminfo", str(nc))
    zero = run_json("gdalmdiminfo", str(h5))
    info = run_json("gdalinfo", "-json", str(nc))
    version = subprocess.run(["gdalinfo", "--version"], check=True, capture_output=True, text=True).stdout.strip()
    expected = {
        "gdal": version,
        "multidim": {"/": mdim["attributes"], "/z": mdim["arrays"]["z"]["attributes"]},
        "metadata": info["metadata"][""],
        "zero_length": zero["arrays"]["z"]["attributes"],
    }
    (OUT / "expected.json").write_text(json.dumps(expected, indent=2) + "\n")
    print(json.dumps(expected, indent=2))


if __name__ == "__main__":
    main()

"""Writes `crates/terrio/tests/data/netcdf_user_defined/user_defined_attrs.nc` and what GDAL reads of it.

Usage:
    python scripts/make_netcdf_user_defined_fixture.py

The file, generated from `user_defined_attrs.cdl` by `ncgen -4`, holds attributes of user-defined
types: compounds (one with a 2-D array field, one with enum, ubyte and char fields), an enum and an
int vlen. The attributes `gdalmdiminfo` prints and the metadata `gdalinfo -json` reports are
written to `expected.json`, the expected values of the `netcdf_user_defined_attributes_match_gdal`
test, with the GDAL version that produced them. Needs `ncgen`, `gdalmdiminfo` and `gdalinfo` on
`PATH`.
"""

from __future__ import annotations

import json
import subprocess
from pathlib import Path

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
    mdim = run_json("gdalmdiminfo", str(nc))
    info = run_json("gdalinfo", "-json", str(nc))
    version = subprocess.run(["gdalinfo", "--version"], check=True, capture_output=True, text=True).stdout.strip()
    expected = {
        "gdal": version,
        "multidim": {"/": mdim["attributes"], "/z": mdim["arrays"]["z"]["attributes"]},
        "metadata": info["metadata"][""],
    }
    (OUT / "expected.json").write_text(json.dumps(expected, indent=2) + "\n")
    print(json.dumps(expected, indent=2))


if __name__ == "__main__":
    main()

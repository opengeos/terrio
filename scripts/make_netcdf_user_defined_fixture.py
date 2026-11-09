"""Writes `crates/terrio/tests/data/netcdf_user_defined/user_defined_attrs.nc` and what GDAL reads of it.

Usage:
    python scripts/make_netcdf_user_defined_fixture.py

The file, generated from `user_defined_attrs.cdl` by `ncgen -4`, holds attributes of user-defined
types: compounds (one with a 2-D array field, one with enum, ubyte and char fields), an enum and an
int vlen. The attributes `gdalmdiminfo` prints and the metadata `gdalinfo -json` reports are
written to `expected.json`, the expected values of the `netcdf_user_defined_attributes_match_gdal`
test, with the GDAL version that produced them. `zero_length_attrs.h5`, written with h5py, holds
zero-length compound, enum, int, short, float, double and string attributes, which `ncgen` cannot
write, and a `units` string. `gdalmdiminfo` prints the zero-length string attribute as `[]` and
the others as `null` (`zero_length`), and reports `units` as the array's unit rather than an
attribute; the attributes left in its `gdalmdimtranslate -of netCDF` copy are
`zero_length_netcdf_copy`. `nested_compound_attrs.nc`, from `nested_compound_attrs.cdl`, holds
nested compound attributes and enough variables that HDF5 keeps its root group's links in dense
storage. The named datatypes of the `gdalmdimtranslate -of netCDF` copies of the two netCDF files,
and the type of each numeric or compound attribute there, are `netcdf_copy_types`. Needs `ncgen`,
`gdalmdiminfo`, `gdalmdimtranslate` and `gdalinfo` on `PATH`, and h5py and numpy.
"""

from __future__ import annotations

import json
import subprocess
import tempfile
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


# Attributes of netCDF-4's and HDF5's dimension-scale bookkeeping, and the global ones GDAL adds.
BOOKKEEPING = {"CLASS", "NAME", "REFERENCE_LIST", "DIMENSION_LIST", "_Netcdf4Coordinates", "_Netcdf4Dimid", "_NCProperties"}


def copy_types(path: Path) -> dict:
    """Describes the named datatypes of a netCDF-4 file and the types of its attributes.

    Args:
        path: The file.

    Returns:
        `types`: each named datatype's size and fields (name, byte offset, and a NumPy type name or
        the name of the named datatype it equals); `attributes`: by object path, the type of each
        numeric or compound attribute, described the same way.
    """
    import h5py

    with h5py.File(path) as f:
        named = {name: obj.dtype for name, obj in f.items() if isinstance(obj, h5py.Datatype)}

        def describe(dtype):
            if dtype.fields is None:
                return dtype.name
            return next((name for name, other in named.items() if other == dtype), "anonymous")

        types = {
            name: {
                "size": dtype.itemsize,
                "fields": [[field, offset, describe(sub)] for field, (sub, offset) in dtype.fields.items()],
            }
            for name, dtype in named.items()
        }
        attributes = {}
        for obj_path, obj in [("/", f), *((f"/{name}", obj) for name, obj in f.items() if isinstance(obj, h5py.Dataset))]:
            found = {}
            for name in obj.attrs:
                dtype = obj.attrs.get_id(name).dtype
                if name not in BOOKKEEPING and dtype.kind not in "SOU":
                    found[name] = describe(dtype)
            if found:
                attributes[obj_path] = found
    return {"types": types, "attributes": attributes}


def main() -> None:
    """Regenerates the fixture and its `expected.json`."""
    nc = OUT / "user_defined_attrs.nc"
    subprocess.run(["ncgen", "-4", "-o", str(nc), str(OUT / "user_defined_attrs.cdl")], check=True)
    nested = OUT / "nested_compound_attrs.nc"
    subprocess.run(["ncgen", "-4", "-o", str(nested), str(OUT / "nested_compound_attrs.cdl")], check=True)
    h5 = OUT / "zero_length_attrs.h5"
    import h5py  # Only the zero-length fixture needs h5py and numpy.
    import numpy as np

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
    with tempfile.TemporaryDirectory() as tmp:
        copy = Path(tmp) / "copy.nc"
        subprocess.run(["gdalmdimtranslate", "-of", "netCDF", str(h5), str(copy)], check=True, capture_output=True)
        zero_copy = run_json("gdalmdiminfo", str(copy))
        netcdf_copy_types = {}
        for source in [nc, nested]:
            copy = Path(tmp) / f"copy_{source.name}"
            subprocess.run(["gdalmdimtranslate", "-of", "netCDF", str(source), str(copy)], check=True, capture_output=True)
            netcdf_copy_types[source.name] = copy_types(copy)
    info = run_json("gdalinfo", "-json", str(nc))
    version = subprocess.run(["gdalinfo", "--version"], check=True, capture_output=True, text=True).stdout.strip()
    expected = {
        "gdal": version,
        "multidim": {"/": mdim["attributes"], "/z": mdim["arrays"]["z"]["attributes"]},
        "metadata": info["metadata"][""],
        "zero_length": zero["arrays"]["z"]["attributes"],
        "zero_length_netcdf_copy": zero_copy["arrays"]["z"].get("attributes", {}),
        "netcdf_copy_types": netcdf_copy_types,
    }
    (OUT / "expected.json").write_text(json.dumps(expected, indent=2) + "\n")
    print(json.dumps(expected, indent=2))


if __name__ == "__main__":
    main()

"""Writes two synthetic HEC-RAS geometry files with large root attributes, for the byte-budget tests.

Both are crates/terrio/tests/data/hecras/pipes.h5 (two pipe conduits and three pipe nodes, root
`File Type` `HEC-RAS Geometry`) with root attributes added; they hold no model data:

- `large_projection.h5`: a `Projection` of 16,028 bytes, a well-formed WKT whose CRS name is
  padded out;
- `large_root_attribute.h5`: a short `Projection` and an unrelated 16,384-byte root attribute,
  `Notes`.

Both are fixed-length strings, as HEC-RAS writes its root attributes.

Usage: python -I make_hecras_root_fixtures.py <pipes.h5> <out dir>
"""
import shutil
import sys
from pathlib import Path

import h5py
import numpy as np

WKT = (
    'PROJCS["{name}",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",'
    'SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],'
    'UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],'
    'PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],'
    'PARAMETER["Central_Meridian",-87.0],PARAMETER["Scale_Factor",0.9996],'
    'PARAMETER["Latitude_Of_Origin",0.0],UNIT["Foot_US",0.3048006096012192]]'
)
PROJECTION_BYTES = 16028


def fixed(text):
    """A fixed-length string attribute value, as HEC-RAS stores root attributes."""
    data = text.encode("ascii")
    return np.array(data, dtype=f"S{len(data)}")


def main(pipes, out):
    out = Path(out)
    short = WKT.format(name="Synthetic")
    large = WKT.format(name="Synthetic " + "x" * (PROJECTION_BYTES - len(short) - len(" ")))
    assert len(large) == PROJECTION_BYTES
    for name, attributes in [
        ("large_projection.h5", {"Projection": large}),
        ("large_root_attribute.h5", {"Projection": short, "Notes": "n" * 16384}),
    ]:
        target = out / name
        shutil.copyfile(pipes, target)
        with h5py.File(target, "r+") as f:
            for key, value in attributes.items():
                f.attrs[key] = fixed(value)


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])

"""Writes `crates/terrio/tests/data/rotated_pole/eur11.nc`, a CF rotated-pole grid.

Usage:
    python scripts/make_rotated_pole_fixture.py

A 4x3 classic netCDF file on CORDEX EUR-11's grid (its north pole at 39.25 N, 162 W, on
COSMO-CLM's sphere): `rlon` and `rlat` coordinates and a `rotated_latitude_longitude` grid
mapping given by its CF parameters only (no `crs_wkt`), as CORDEX files store it. Needs numpy and
scipy.
"""

from __future__ import annotations

from pathlib import Path

import numpy as np
from scipy.io import netcdf_file

OUT = Path(__file__).resolve().parent.parent / "crates" / "terrio" / "tests" / "data" / "rotated_pole" / "eur11.nc"


def main() -> None:
    """Writes the fixture."""
    OUT.parent.mkdir(parents=True, exist_ok=True)
    with netcdf_file(OUT, "w", version=1) as f:
        f.createDimension("rlat", 3)
        f.createDimension("rlon", 4)
        rlat = f.createVariable("rlat", "f8", ("rlat",))
        rlat[:] = -23.375 + 0.11 * np.arange(3)
        rlat.standard_name = b"grid_latitude"
        rlat.long_name = b"latitude in rotated pole grid"
        rlat.units = b"degrees"
        rlat.axis = b"Y"
        rlon = f.createVariable("rlon", "f8", ("rlon",))
        rlon[:] = -28.375 + 0.11 * np.arange(4)
        rlon.standard_name = b"grid_longitude"
        rlon.long_name = b"longitude in rotated pole grid"
        rlon.units = b"degrees"
        rlon.axis = b"X"
        pole = f.createVariable("rotated_pole", "c", ())
        pole.grid_mapping_name = b"rotated_latitude_longitude"
        pole.grid_north_pole_latitude = 39.25
        pole.grid_north_pole_longitude = -162.0
        pole.north_pole_grid_longitude = 0.0
        pole.earth_radius = 6371229.0
        tas = f.createVariable("tas", "f4", ("rlat", "rlon"))
        tas[:] = np.arange(12, dtype="f4").reshape(3, 4)
        tas.grid_mapping = b"rotated_pole"
        tas.units = b"K"


if __name__ == "__main__":
    main()

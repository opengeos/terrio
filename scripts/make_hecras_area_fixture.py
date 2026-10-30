"""Writes crates/terrio/tests/data/hecras/results_two_areas.h5, for the boundary-series tests.

It is crates/terrio/tests/data/hecras/results_mesh_xs.h5 (see make_hecras_fixture.py) with a second
2D flow area, `Dry`: a copy of the `Good` area's geometry and results groups without the results'
`Boundary Conditions` group, as HEC-RAS writes an area that no boundary-condition line feeds. It
holds no model data.

Usage: python -I make_hecras_area_fixture.py <results_mesh_xs.h5> <out.h5>
"""
import shutil
import sys

import h5py

AREAS = "Geometry/2D Flow Areas"
RESULTS = "Results/Unsteady/Output/Output Blocks/Base Output/Unsteady Time Series/2D Flow Areas"


def main(source, out):
    shutil.copyfile(source, out)
    with h5py.File(out, "r+") as f:
        f.copy(f[f"{AREAS}/Good"], f"{AREAS}/Dry")
        f.copy(f[f"{RESULTS}/Good"], f"{RESULTS}/Dry")
        del f[f"{RESULTS}/Dry/Boundary Conditions"]


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[2])

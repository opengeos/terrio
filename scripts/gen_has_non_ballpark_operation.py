# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""Generate what ``Transformer::has_non_ballpark_operation`` is checked against.

For every EPSG geographic 2D CRS that is not deprecated, into WGS 84 (EPSG:4326), and for every
seventh of them into ETRS89 (EPSG:4258) and OSGB36 (EPSG:4277): whether PROJ's candidate
operations (pyproj's ``TransformerGroup`` with ballparks allowed, its available operations and
those whose grids are missing) hold one that is not PROJ's ballpark ("Ballpark geographic
offset"). An operation whose grid is missing counts when every grid it lacks has a direct-download
URL in PROJ's catalog, including non-CDN URLs; one whose grid has no such URL
(``HS2TN15_NTv2.gsb``) does not.

Writes ``crates/terrio-crs/tests/data/has_non_ballpark_operation.tsv``, lines
``source<TAB>target<TAB>0 or 1``.

Usage: python scripts/gen_has_non_ballpark_operation.py

Needs pyproj on PROJ 9.9.0 (conda-forge ``pyproj=3.8.0 proj=9.9.0``) with no grids installed; the
network is turned off.
"""

import os
from pathlib import Path

import pyproj
import pyproj.datadir
import pyproj.network
from pyproj.database import query_crs_info
from pyproj.transformer import TransformerGroup

OUT = (
    Path(__file__).resolve().parents[1]
    / "crates/terrio-crs/tests/data/has_non_ballpark_operation.tsv"
)


def obtainable(op):
    """Check whether every missing grid has a direct-download catalog URL.

    Args:
        op: An unavailable PROJ coordinate operation.

    Returns:
        True if the operation has grids and each is available or directly downloadable.
    """
    # pyproj's Grid exposes no optional flag. This reference conservatively requires
    # every listed grid; Terrio ignores optional grids. No frozen pair exercises that
    # distinction. A direct-download catalog flag is the policy, independent of URL text.
    return bool(op.grids) and all(
        g.available or g.direct_download for g in op.grids
    )


def has(src, dst):
    g = TransformerGroup(f"EPSG:{src}", f"EPSG:{dst}", allow_ballpark=True)
    names = [t.description for t in g.transformers]
    names += [o.name for o in g.unavailable_operations if obtainable(o)]
    return any("Ballpark" not in n for n in names)


def main():
    assert pyproj.proj_version_str == "9.9.0", pyproj.proj_version_str
    pyproj.network.set_network_enabled(False)
    for d in pyproj.datadir.get_data_dir().split(os.pathsep) + [
        pyproj.datadir.get_user_data_dir()
    ]:
        grids = [
            f
            for f in (os.listdir(d) if os.path.isdir(d) else [])
            if f.endswith((".tif", ".gsb", ".gtx"))
        ]
        assert not grids, (d, grids)
    codes = sorted(
        int(i.code)
        for i in query_crs_info(auth_name="EPSG", pj_types=["GEOGRAPHIC_2D_CRS"])
        if not i.deprecated
    )
    pairs = [(c, 4326) for c in codes]
    pairs += [(c, d) for d in (4258, 4277) for c in codes[::7]]
    lines = [f"{s}\t{d}\t{int(has(s, d))}" for s, d in pairs]
    OUT.write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()

# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT

"""The no-grid check of the PROJ reference generators (``installed_grids``, which
``gen_wkt_unresolved.py`` and ``gen_wkt_datum_shift.py`` refuse to run with grids under)."""

import os
import sys
from pathlib import Path

import pytest

pyproj = pytest.importorskip("pyproj")

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import gen_wkt_datum_shift
import gen_wkt_unresolved


@pytest.fixture
def search_path(tmp_path, monkeypatch):
    """A PROJ search path with PROJ's database alone, and an empty user data directory."""
    data = tmp_path / "data"
    data.mkdir()
    real = next(
        Path(d, "proj.db")
        for d in pyproj.datadir.get_data_dir().split(os.pathsep)
        if Path(d, "proj.db").is_file()
    )
    (data / "proj.db").symlink_to(real)
    user = tmp_path / "user"
    user.mkdir()
    monkeypatch.setattr(pyproj.datadir, "get_data_dir", lambda: str(data))
    monkeypatch.setattr(pyproj.datadir, "get_user_data_dir", lambda: str(user))
    return data, user


def test_the_generators_share_the_check():
    assert gen_wkt_unresolved.installed_grids is gen_wkt_datum_shift.installed_grids


def test_no_grid_is_found_beside_the_database_alone(search_path):
    assert gen_wkt_unresolved.installed_grids() == []


@pytest.mark.parametrize(
    "name",
    [
        # Legacy (pre-PROJ 7) grid names, in PROJ's database as `old_proj_grid_name`.
        "ABCSRSV4.GSB",
        "ntv1_can.dat",
        # A grid format whatever the case of its extension, in a subdirectory too.
        "sub/some_grid.TIF",
        "sub/other.Gtx",
    ],
)
def test_grids_are_found_by_legacy_name_or_format(search_path, name):
    data, _ = search_path
    path = data / name
    path.parent.mkdir(exist_ok=True)
    path.write_bytes(b"")
    found = gen_wkt_unresolved.installed_grids()
    assert [Path(f).name for f in found] == [path.name]


def test_grids_are_found_in_the_user_data_directory(search_path):
    _, user = search_path
    (user / "ntv1_can.dat").write_bytes(b"")
    assert [Path(f).name for f in gen_wkt_unresolved.installed_grids()] == [
        "ntv1_can.dat"
    ]

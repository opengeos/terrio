"""Grid confinement, in-memory grids and refusal diagnostics at the Python boundary."""

import struct
import subprocess
import sys
import warnings

import pytest

import terrio


@pytest.fixture(autouse=True)
def grid_settings():
    """Restore process-wide grid settings after each test.

    Yields:
        None: Grid policy with networking disabled.
    """
    terrio.set_network_enabled(False)
    terrio.set_grid_search_paths(None)
    try:
        yield
    finally:
        terrio.set_grid_search_paths(None)
        terrio.set_network_enabled(None)


def tiny_grid():
    """Return a GTX geoid with a 22.5 metre offset at the test point."""
    return struct.pack(">4d2i4f", 0.0, 0.0, 1.0, 1.0, 2, 2, 10.0, 20.0, 30.0, 40.0)


def height(name, **kwargs):
    """Transform through the named geoid and return its ellipsoidal height."""
    source = f"+proj=longlat +datum=WGS84 +geoidgrids={name} +vunits=m"
    return terrio.transform_points([0.25], [0.5], source, "EPSG:4979", [100.0], **kwargs)[2][0]


@pytest.mark.parametrize("explicit", [False, True])
def test_confinement_preserves_effective_directories(tmp_path, monkeypatch, explicit):
    """Keep host or explicit directories while refusing path-named grids."""
    grid = tmp_path / "tiny.gtx"
    grid.write_bytes(tiny_grid())
    monkeypatch.setenv("TERRIO_GRID_DIR", str(tmp_path))
    if explicit:
        terrio.set_grid_search_paths([tmp_path])
    else:
        assert height(str(grid)) == pytest.approx(122.5)
    terrio.confine_grid_search_paths()
    terrio.confine_grid_search_paths()
    assert height("tiny.gtx") == pytest.approx(122.5)
    with pytest.warns(UserWarning, match="named by a path is not read"):
        with pytest.raises(terrio.CRSError):
            height(str(grid))
    terrio.set_grid_search_paths(None)
    assert height(str(grid)) == pytest.approx(122.5)


def test_registered_grid_survives_confinement():
    """Supply a registered grid even with an empty filesystem search list."""
    terrio.set_grid_search_paths([])
    terrio.register_grid("python_policy_registered.gtx", tiny_grid())
    terrio.confine_grid_search_paths()
    assert height("python_policy_registered.gtx") == pytest.approx(122.5)


@pytest.mark.parametrize("name", ["absolute", "./tiny.gtx", "sub/../tiny.gtx"])
def test_refusal_warns_without_leaking_to_later_calls(tmp_path, name):
    """Emit failed-transform diagnostics once and leave unrelated calls warning-free."""
    grid = tmp_path / "tiny.gtx"
    grid.write_bytes(tiny_grid())
    terrio.set_grid_search_paths([tmp_path])
    name = str(grid) if name == "absolute" else name
    for _ in range(2):
        with pytest.warns(UserWarning) as caught:
            with pytest.raises(terrio.CRSError):
                height(name)
        assert len(caught) == 1
        assert "is not read" in str(caught[0].message)
    with warnings.catch_warnings(record=True) as later:
        warnings.simplefilter("always")
        assert height("tiny.gtx") == pytest.approx(122.5)
    assert not later


def test_size_refusal_is_warned(tmp_path):
    """Report the size refusal of a sparse grid before trying to decode it."""
    grid = tmp_path / "huge.gtx"
    grid.write_bytes(tiny_grid())
    with grid.open("r+b") as stream:
        stream.truncate((2 << 30) + 1)
    terrio.set_grid_search_paths([tmp_path])
    with pytest.warns(UserWarning, match="larger than"):
        with pytest.raises(terrio.CRSError):
            height("huge.gtx")


def test_refusal_warns_when_failed_points_are_returned(tmp_path):
    """Emit the diagnostic when non-strict batches return infinite coordinates."""
    terrio.set_grid_search_paths([])
    with pytest.warns(UserWarning, match="named by a path is not read"):
        assert height(str(tmp_path / "missing.gtx"), errcheck=False) == float("inf")


def test_grid_refusal_escapes_terminal_controls(tmp_path):
    """Keep terminal controls in untrusted grid names out of emitted warnings."""
    terrio.set_grid_search_paths([])
    with pytest.warns(UserWarning, match="named by a path is not read") as caught:
        with pytest.raises(terrio.CRSError):
            height(str(tmp_path / "\x1b[31m.gtx"))
    assert len(caught) == 1
    assert "\x1b" not in str(caught[0].message)
    assert r"\u{1b}" in str(caught[0].message)


def test_cli_reports_grid_lookup_refusal():
    """Report standalone grid diagnostics in the CLI as well as Python warnings."""
    result = subprocess.run(
        [
            sys.executable,
            "-m",
            "terrio",
            "crs",
            "+proj=longlat +datum=WGS84 +geoidgrids=sub/../tiny.gtx +vunits=m",
            "--to",
            "EPSG:4979",
            "--dim",
            "3",
            "0.25",
            "0.5",
            "100",
        ],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert result.returncode != 0
    assert "cannot transform" in result.stderr
    assert "sub/../tiny.gtx" in result.stderr
    assert "warning" in result.stderr.lower()

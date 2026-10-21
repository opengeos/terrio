"""The WKT terrio writes for every PROJ projection it implements, read back by PROJ (#134).

The cases and PROJ's reference points are those of ``crates/terrio/tests/proj_strings.rs``
(``crates/terrio/tests/data/proj_strings.tsv``, from ``scripts/gen_proj_string_points.py``): each
PROJ string terrio accepts is written as WKT by terrio, read by pyproj, and PROJ's coordinates
from that WKT compared with PROJ's from the PROJ string, within 1 mm. Known differences, none
since #124, are listed in ``KNOWN_DIFFERENCES``.
"""

import math
import warnings
from pathlib import Path

import pytest

import terrio.rio as rio

pyproj = pytest.importorskip("pyproj")

DATA = Path(__file__).resolve().parents[2] / "crates/terrio/tests/data/proj_strings.tsv"

# PROJ strings whose WKT, as terrio writes it, PROJ reads as another CRS or refuses, by
# projection; both figures unless a figure is given. None since #124.
KNOWN_DIFFERENCES: dict[str, str] = {}


def _cases():
    """The cases of the reference file.

    Returns:
        A list of ``(name, earth, proj, points)``, points as ``(lon, lat, x, y)`` that PROJ
        projects.
    """
    cases = []
    for line in DATA.read_text().splitlines():
        if line.startswith("# case "):
            name, earth = line[len("# case ") :].split()
            cases.append([name, earth, None, []])
        elif line.startswith("# proj ") and cases:
            cases[-1][2] = line[len("# proj ") :]
        elif line and not line.startswith("#") and cases:
            v = [float(c) for c in line.split("\t")]
            if math.isfinite(v[2]):
                cases[-1][3].append(tuple(v[:4]))
    return [tuple(c) for c in cases if c[3]]


CASES = _cases()


def _terrio_wkt(proj):
    """terrio's WKT of a PROJ string.

    Args:
        proj: The PROJ string.

    Returns:
        The WKT, or None when terrio refuses the PROJ string.
    """
    try:
        return rio.CRS.from_string(proj).to_wkt()
    except Exception:  # noqa: BLE001 - terrio refusing a projection is checked in Rust
        return None


@pytest.mark.parametrize(
    ("name", "earth", "proj", "points"), CASES, ids=[f"{c[0]}-{c[1]}" for c in CASES]
)
def test_proj_reads_terrios_wkt_as_the_same_crs(name, earth, proj, points):
    """PROJ's coordinates from terrio's WKT are PROJ's from the PROJ string.

    Args:
        name: The projection.
        earth: ``sphere`` or ``wgs84``.
        proj: The PROJ string.
        points: PROJ's points from the PROJ string.
    """
    wkt = _terrio_wkt(proj)
    if wkt is None:
        pytest.skip("terrio refuses the PROJ string")
    known = KNOWN_DIFFERENCES.get(f"{name} {earth}") or KNOWN_DIFFERENCES.get(name)
    try:
        with warnings.catch_warnings():
            warnings.simplefilter("ignore")
            crs = pyproj.CRS(wkt)
            t = pyproj.Transformer.from_crs(crs.geodetic_crs, crs, always_xy=True)
        worst = 0.0
        for lon, lat, x, y in points:
            tx, ty = t.transform(lon, lat)
            d = math.hypot(tx - x, ty - y)
            worst = max(worst, d if math.isfinite(d) else math.inf)
        ok = worst <= 1e-3
        detail = f"{worst:.3g} m from PROJ's coordinates of the PROJ string"
    except (pyproj.exceptions.CRSError, pyproj.exceptions.ProjError) as err:
        ok, detail = False, f"PROJ refuses it: {err}"
    # A known difference that now reads back passes.
    if known and not ok:
        pytest.xfail(f"{known}: {detail}")
    assert ok, f"{name} ({earth}): {detail}\n{wkt}"

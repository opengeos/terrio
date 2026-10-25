"""Writes `crates/terrio-crs/tests/data/world_projections.tsv`: PROJ's coordinates for the spherical
world projections terrio ports from PROJ (Robinson, the Winkels, Gall, Foucaut sinusoidal...).

Usage:
    python scripts/gen_world_projection_points.py

Each projection is on a sphere and on WGS 84, from the geographic CRS on the same figure (so no
datum transformation is involved). Each row holds a CRS, a longitude and latitude, PROJ's
projected x and y (`inf` when PROJ refuses the point) and PROJ's inverse of that x and y. Robinson
has a point every 0.1° of latitude, at longitudes up to ±180°. Needs pyproj.
"""

from __future__ import annotations

from pathlib import Path

import numpy as np
import pyproj

OUT = Path(__file__).resolve().parent.parent / "crates" / "terrio-crs" / "tests" / "data" / "world_projections.tsv"

FIGURES = {
    "+R=6371000": "+proj=longlat +R=6371000 +no_defs",
    "+datum=WGS84": "+proj=longlat +datum=WGS84 +no_defs",
}

# The projections, with their parameters and defaults.
CASES = [
    "+proj=robin",
    "+proj=robin +lon_0=-100 +x_0=1000 +y_0=-2000",
    "+proj=gall",
    "+proj=gall +lon_0=20",
    "+proj=gstmerc",
    "+proj=gstmerc +lat_0=-21.116666667 +lon_0=55.53333333 +k_0=1 +x_0=160000 +y_0=50000",
    "+proj=wintri",
    "+proj=wintri +lat_1=30",
    "+proj=wintri +lat_1=50.467",
    "+proj=wink1",
    "+proj=wink1 +lat_ts=30",
    "+proj=wink2",
    "+proj=wink2 +lat_1=30",
    "+proj=fouc_s",
    "+proj=fouc_s +n=0.3",
    "+proj=mbtfps",
    "+proj=putp4p",
    "+proj=murd2 +lat_1=20 +lat_2=60",
    "+proj=murd2 +lat_1=-30 +lat_2=-60 +lat_0=-40 +lon_0=10",
    "+proj=tissot +lat_1=20 +lat_2=60",
    "+proj=tissot +lat_1=-30 +lat_2=-60 +lat_0=-40 +lon_0=10",
]

# Points over the globe, relative to the central meridian.
LONS = [-180.0, -179.5, -120.3, -45.0, -10.0, 0.0, 0.5, 60.0, 150.0, 179.9, 180.0]
LATS = [-90.0, -89.5, -62.5, -20.0, -5.0, 0.0, 2.5, 40.0, 80.0, 89.5, 90.0]


def points(crs: str) -> tuple[np.ndarray, np.ndarray]:
    """The longitudes and latitudes to project in `crs`.

    Args:
        crs: The PROJ string of the projected CRS.

    Returns:
        The longitudes and latitudes in degrees.
    """
    lon0 = float(crs.split("+lon_0=")[1].split()[0]) if "+lon_0=" in crs else 0.0
    if crs.startswith("+proj=robin ") and "+lon_0" not in crs:
        lat = np.round(np.linspace(-90, 90, 1801), 1)
        cycle = np.array([180.0, -180.0, 179.9, -135.5, 90.0, 45.25, -12.0, 0.0])
        lon = cycle[np.arange(lat.size) % cycle.size]
    else:
        lon, lat = (a.ravel() for a in np.meshgrid(LONS, LATS))
    return lon0 + lon, lat


def main() -> None:
    """Writes the table."""
    rows = []
    n = 0
    for case in CASES:
        for figure, geog in FIGURES.items():
            crs = f"{case} {figure} +units=m +no_defs"
            rows.append(f"# {crs}")
            lon, lat = points(crs)
            fwd = pyproj.Transformer.from_crs(geog, crs, always_xy=True)
            # Rounded to the micrometre, which the inverse then starts from.
            x, y = (np.round(v, 6) for v in fwd.transform(lon, lat, errcheck=False))
            inv = pyproj.Transformer.from_crs(crs, geog, always_xy=True)
            ilon, ilat = inv.transform(x, y, errcheck=False)
            for row in zip(lon, lat, x, y, ilon, ilat):
                lo, la, px, py, il, ia = (float(v) for v in row)
                cols = [repr(lo), repr(la), f"{px:.6f}", f"{py:.6f}"]
                # PROJ's inverse, when it is not the point itself (or fails).
                same = np.isfinite(il) and abs((il - lo + 180) % 360 - 180) < 1e-10 and abs(ia - la) < 1e-10
                if not same:
                    cols += [repr(il), repr(ia)]
                rows.append("\t".join(cols))
                n += 1
    OUT.write_text(f"# PROJ {pyproj.proj_version_str}'s points, from scripts/gen_world_projection_points.py\n# lon\tlat\tx\ty[\tinv_lon\tinv_lat]\n" + "\n".join(rows) + "\n")
    print(f"{n} points in {OUT}")


if __name__ == "__main__":
    main()

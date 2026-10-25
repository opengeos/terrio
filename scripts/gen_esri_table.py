"""Generate terrio's ESRI table from GDAL and PROJ.

Writes ``crates/terrio-crs/data/esri.tsv.zst``, a zstd-compressed text table of the geographic (2D
and 3D) and projected CRSs of PROJ's ESRI authority (``ESRI:54009``, ``ESRI:102003``), in the
format of the EPSG table's ``O`` and ``C`` lines (see ``scripts/gen_epsg_table.py``): the
datum transformations to WGS 84 GDAL uses for them, then each CRS with its name, the EPSG codes
of its datum and geographic CRS (empty when they are not EPSG ones), its transformations, flags,
area of use, PROJ string and the WKT GDAL writes for it (which ends with
``AUTHORITY["ESRI","<code>"]``). The transformations are numbered apart from the EPSG table's.
CRSs PROJ cannot transform from WGS 84 (those of the Moon and Mars, and a few whose parameters
PROJ rejects) are left out.

Usage: python scripts/gen_esri_table.py

Needs pyproj, GDAL's Python bindings and the ``zstd`` command.
"""

import os
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

sys.path.insert(0, os.path.dirname(__file__))

import pyproj  # noqa: E402
from gen_epsg_table import gis_axis, normalize_proj, transformations  # noqa: E402
from osgeo import gdal, osr  # noqa: E402
from pyproj.enums import PJType  # noqa: E402

OUT = os.path.join(os.path.dirname(__file__), "..", "crates", "terrio-crs", "data", "esri.tsv.zst")


def esri_codes():
    """List the ESRI geographic (2D and 3D) and projected CRS codes, deprecated ones included.

    Returns:
        The sorted codes.
    """
    codes = set()
    for kind in (PJType.GEOGRAPHIC_2D_CRS, PJType.GEOGRAPHIC_3D_CRS, PJType.PROJECTED_CRS):
        codes.update(int(c) for c in pyproj.get_codes("ESRI", kind, allow_deprecated=True))
    return sorted(codes)


def epsg_code(srs, node):
    """Give the EPSG code of a node of a GDAL spatial reference.

    Args:
        srs: The GDAL spatial reference.
        node: The node, such as ``DATUM`` or ``GEOGCS``.

    Returns:
        The code, or an empty string when the node has none or another authority's.
    """
    if (srs.GetAuthorityName(node) or "").upper() != "EPSG":
        return ""
    return srs.GetAuthorityCode(node) or ""


def esri_proj(proj, srs, crs):
    """Normalize the PROJ string GDAL exports for an ESRI CRS, as ``normalize_proj`` does.

    Args:
        proj: The PROJ string from ``ExportToProj4``.
        srs: The GDAL spatial reference.
        crs: The pyproj CRS.

    Returns:
        The PROJ string for terrio: a spherical form (``+f=0``) keeps its sphere rather than
        the datum's ellipsoid, and an axis order of the projection itself (``+axis=wsu`` of the
        South African Lo grids, whose WKT has no axes) is kept.
    """
    tokens = proj.split()
    if "+f=0" in tokens:
        tokens = [t for t in tokens if not t.startswith("+datum=")] + [f"+a={crs.ellipsoid.semi_major_metre!r}"]
    out = normalize_proj(" ".join(tokens), crs, gis_axis(srs, crs))
    axis = next((t for t in tokens if t.startswith("+axis=")), None)
    if axis and "+axis=" not in out:
        first, _, rest = out.partition(" ")
        out = f"{first} {axis} {rest}".strip()
    return out


def rows():
    """Yield the table's rows.

    Yields:
        ``O`` rows for each new datum transformation, then the ``C`` rows.
    """
    crss = []
    for code in esri_codes():
        try:
            srs = osr.SpatialReference()
            srs.SetFromUserInput(f"ESRI:{code}")
            proj = srs.ExportToProj4()
            wkt = srs.ExportToWkt()
            crs = pyproj.CRS.from_authority("ESRI", code)
            # Those PROJ cannot transform from WGS 84 (the Moon's and Mars's, and those whose
            # parameters PROJ rejects) are left out, so that terrio does not either.
            pyproj.Transformer.from_crs("EPSG:4326", crs)
        except (pyproj.exceptions.CRSError, pyproj.exceptions.ProjError, RuntimeError):
            continue
        if not proj or "+proj=" not in proj or not wkt:
            continue
        name = crs.name.replace("\t", " ")
        datum = epsg_code(srs, "DATUM") or epsg_code(srs, "GEOGCS|DATUM")
        is_wgs84 = datum == "6326"
        pm = crs.prime_meridian
        pm = pm.longitude * pm.unit_conversion_factor * 180 / 3.141592653589793
        three_d = crs.type_name == "Geographic 3D CRS"
        geog = epsg_code(srs, "GEOGCS")
        if three_d:
            g = crs.to_2d().to_authority(min_confidence=100)
            geog = g[1] if g and g[0] == "EPSG" else ""
        flags = ("d" if srs.IsDynamic() else "") + ("3" if three_d else "")
        aou = crs.area_of_use
        area = ",".join(repr(round(v, 6)) for v in (aou.west, aou.south, aou.east, aou.north)) if aou else ""
        line = (code, name, datum, geog, flags, area, esri_proj(proj, srs, crs), wkt)
        crss.append((is_wgs84, pm, three_d, line))

    def to_wgs84(c, grids):
        """List the transformations of a CRS to WGS 84.

        Args:
            c: The CRS's entry in ``crss``.
            grids: Whether to list those with grids.

        Returns:
            The transformations.
        """
        is_wgs84, pm, three_d, line = c
        if is_wgs84:
            return [("", (-180.0, -90.0, 180.0, 90.0), -1.0, "")]
        return transformations(f"ESRI:{line[0]}", "EPSG:4979" if three_d else "EPSG:4326", pm, 0.0, grids)

    with ThreadPoolExecutor(os.cpu_count()) as pool:
        found = list(pool.map(lambda c: to_wgs84(c, False), crss))
        found_grids = list(pool.map(lambda c: to_wgs84(c, True), crss))
    ops = {}

    def op_ids(transfos):
        """Number transformations, new ones first seen.

        Args:
            transfos: The transformations from ``transformations``.

        Returns:
            Their ids and the ``O`` rows of those not seen before.
        """
        ids, new = [], []
        for steps, bounds, accuracy, flags in transfos:
            key = (steps, bounds, accuracy, flags)
            if key not in ops:
                ops[key] = len(ops)
                new.append(("O", ops[key], ",".join(repr(round(b, 6)) for b in bounds), repr(accuracy), flags, steps))
            if ops[key] not in ids:
                ids.append(ops[key])
        return ids, new

    for (_, _, _, (code, name, datum, geog, flags, area, proj, wkt)), transfos, transfos_grids in zip(crss, found, found_grids):
        ids, new = op_ids(transfos)
        gids, gnew = op_ids(transfos_grids)
        yield from new + gnew
        gids = [] if gids == ids else gids
        yield ("C", code, name, datum, geog, ",".join(map(str, ids)), ",".join(map(str, gids)), flags, area, proj, wkt)


def main():
    """Write the compressed table."""
    lines = ["\t".join(map(str, r)) + "\n" for r in rows()]
    header = f"# PROJ's ESRI authority, via PROJ {pyproj.proj_version_str} and GDAL {gdal.__version__}. Generated by scripts/gen_esri_table.py.\n"
    data = (header + "".join(lines)).encode()
    out = subprocess.run(["zstd", "-19", "-q", "-c"], input=data, capture_output=True, check=True).stdout
    with open(OUT, "wb") as f:
        f.write(out)
    kinds = {}
    for line in lines:
        kinds[line[0]] = kinds.get(line[0], 0) + 1
    print(f"{kinds}, {len(data)} bytes, {len(out)} compressed")


if __name__ == "__main__":
    main()

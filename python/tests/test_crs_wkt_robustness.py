"""WKT that GDAL reads or refuses at once never hangs or aborts terrio.

terrio's WKT reader looped forever, allocating, on a closing bracket that did not match its
opener, until the process aborted: from a Shapefile `.prj`, a GeoJSON `crs` member, a GeoTIFF's
ESRI PE citation, a PAM `<SRS>` and every string CRS. Each case here runs in a fresh interpreter
under a 2 GiB address-space limit and a timeout, so a regression fails the test instead of
killing the suite. GDAL 3.12.1 (rasterio 1.5.0, PROJ 9.7.1's WKT parser) takes either closer for
any node, so where it reads such WKT terrio must read the same CRS: the values asserted are
GDAL's, measured on the reproducers: a fuzzing campaign's case 1424 (a `.prj`), and the
same malformed WKT through a GeoJSON `crs` member, a PAM `<SRS>` and a GeoTIFF's ESRI PE citation.
"""

import json
import struct
import subprocess
import sys
import textwrap

import pytest

if sys.platform != "linux":
    pytest.skip("needs RLIMIT_AS and /proc", allow_module_level=True)

import resource  # noqa: E402

WGS84 = (
    'GEOGCS["WGS 84",DATUM["WGS_1984",SPHEROID["WGS 84",6378137,298.257223563]],'
    'PRIMEM["Greenwich",0],UNIT["degree",0.0174532925199433]]'
)
# The reproducers' input: the spheroid's `]` replaced by `)`.
WGS84_PAREN = WGS84.replace("298.257223563]]", "298.257223563)]")
# Vector case 1424's mutated `.prj`: GDAL refuses it ("missing , or ]").
CASE_1424_PRJ = (
    'GEOGCS["GCQ_WGS_1984",DATUM["D_WGS_1985"(SPHEROID["WGS_1984",6378137*0,298.257223563]],'
    'PRIMEM["Greenwhch",0.0],UNIT[&Degree",0.0174532925199433]]'
)
LIMIT = 2 << 30
TIMEOUT = 60


def _limit():
    resource.setrlimit(resource.RLIMIT_AS, (LIMIT, LIMIT))


def _run(code, *args):
    """Runs `code` in a fresh interpreter under the memory limit; its last stdout line, as JSON.

    Args:
        code: Python source; it prints one JSON line.
        *args: Arguments, as `sys.argv[1:]`.

    Returns:
        Any: The decoded line.
    """
    try:
        p = subprocess.run(
            [sys.executable, "-c", textwrap.dedent(code), *map(str, args)],
            capture_output=True,
            text=True,
            timeout=TIMEOUT,
            preexec_fn=_limit,
        )
    except subprocess.TimeoutExpired:
        pytest.fail(f"no answer within {TIMEOUT} s")
    assert p.returncode == 0, f"exit {p.returncode}: {p.stderr[-2000:]}"
    return json.loads(p.stdout.strip().splitlines()[-1])


@pytest.mark.parametrize("entry", ["from_wkt", "from_user_input", "from_string", "transform"])
def test_string_crs_with_a_mismatched_closer_reads_as_gdal_reads_it(entry):
    out = _run(
        """
        import json, sys
        from terrio.rio.crs import CRS
        from terrio.rio.warp import transform
        wkt, entry = sys.argv[1], sys.argv[2]
        if entry == "transform":
            xs, ys = transform(wkt, "EPSG:4326", [10.0], [20.0])
            print(json.dumps([xs[0], ys[0]]))
        else:
            print(json.dumps(getattr(CRS, entry)(wkt).to_epsg()))
        """,
        WGS84_PAREN,
        entry,
    )
    # rasterio 1.5.0: CRS.from_wkt(WGS84_PAREN).to_epsg() == 4326.
    assert out == ([10.0, 20.0] if entry == "transform" else 4326)


def test_wkt_nested_deeper_than_proj_allows_is_refused():
    out = _run(
        """
        import json, sys
        from terrio.rio.crs import CRS
        from terrio.rio.errors import CRSError
        levels = int(sys.argv[1])
        wkt = 'GEOGCS["x",' + "A[" * levels + "1" + "]" * levels + "]"
        try:
            CRS.from_wkt(wkt).to_wkt()
            print(json.dumps("read"))
        except CRSError as e:
            print(json.dumps("CRSError"))
        """,
        30_000,
    )
    # GDAL: "too many nesting levels".
    assert out == "CRSError"


@pytest.mark.parametrize("which", ["dev-zero", "self-reference"])
def test_crs_files_are_read_as_gdal_reads_them(tmp_path, which):
    """A string CRS may name a file: GDAL reads at most 100,000 bytes of it, and reads its text as
    a definition, not as another file. terrio read `/dev/zero` until memory ran out, and recursed
    forever (a stack overflow) on a file naming itself."""
    path = "/dev/zero" if which == "dev-zero" else str(tmp_path / "own.txt")
    if which == "self-reference":
        (tmp_path / "own.txt").write_text(path)
    out = _run(
        """
        import json, sys
        from terrio.rio.crs import CRS
        from terrio.rio.errors import CRSError
        try:
            CRS.from_user_input(sys.argv[1])
            result = "read"
        except CRSError:
            result = "CRSError"
        # This interpreter's own peak (VmHWM, kB), not getrusage's, which keeps the peak of the
        # test process forked to start it.
        status = open("/proc/self/status").read().splitlines()
        peak = int(next(line.split()[1] for line in status if line.startswith("VmHWM:")))
        print(json.dumps([result, peak // 1024]))
        """,
        path,
    )
    # rasterio 1.5.0: CRSError for both, in under a second, at about 55 MiB.
    assert out[0] == "CRSError"
    assert out[1] < 400, f"peak RSS {out[1]} MiB"


def test_geojson_crs_member_with_a_mismatched_closer(tmp_path):
    path = tmp_path / "crs_name.geojson"
    path.write_text(
        json.dumps(
            {
                "type": "FeatureCollection",
                "crs": {"type": "name", "properties": {"name": WGS84_PAREN}},
                "features": [
                    {
                        "type": "Feature",
                        "properties": {"a": 1},
                        "geometry": {"type": "Point", "coordinates": [1, 2]},
                    }
                ],
            }
        )
    )
    out = _run(
        """
        import json, sys
        import terrio.pyogrio as pyogrio
        from terrio.rio.crs import CRS
        info = pyogrio.read_info(sys.argv[1])
        print(json.dumps([CRS.from_user_input(info["crs"]).to_epsg(), int(info["features"])]))
        """,
        path,
    )
    # pyogrio 0.13.0 (GDAL 3.12.4): the layer's CRS is WGS 84, 1 feature.
    assert out == [4326, 1]


def _point_shapefile(base):
    """Writes a one-point Shapefile (`.shp`, `.shx`, `.dbf`) at `base`."""
    rec = struct.pack("<idd", 1, 1.0, 2.0)
    header = lambda length: (  # noqa: E731
        struct.pack(">i5ii", 9994, 0, 0, 0, 0, 0, length // 2)
        + struct.pack("<ii4d4d", 1000, 1, 1, 2, 1, 2, 0, 0, 0, 0)
    )
    shp = header(100 + 8 + len(rec)) + struct.pack(">ii", 1, len(rec) // 2) + rec
    shx = header(108) + struct.pack(">ii", 50, len(rec) // 2)
    fields = b"A".ljust(11, b"\0") + b"N" + b"\0" * 4 + bytes([5, 0]) + b"\0" * 14
    dbf = (
        struct.pack("<B3BIHH20x", 3, 26, 1, 1, 1, 32 + 32 + 1, 1 + 5)
        + fields
        + b"\r"
        + b"     1"
        + b"\x1a"
    )
    for ext, data in ((".shp", shp), (".shx", shx), (".dbf", dbf)):
        (base.parent / (base.name + ext)).write_bytes(data)


@pytest.mark.parametrize("prj", ["case1424", "paren"])
def test_shapefile_prj_is_read_or_refused_promptly(tmp_path, prj):
    base = tmp_path / "pt"
    _point_shapefile(base)
    (tmp_path / "pt.prj").write_text(CASE_1424_PRJ if prj == "case1424" else WGS84_PAREN)
    out = _run(
        """
        import json, sys
        import terrio.pyogrio as pyogrio
        from terrio.rio.crs import CRS
        try:
            info = pyogrio.read_info(sys.argv[1])
        except Exception as e:
            print(json.dumps(["error", type(e).__name__]))
            raise SystemExit
        crs = info["crs"]
        try:
            code = CRS.from_user_input(crs).to_epsg() if crs else None
        except Exception as e:
            code = type(e).__name__
        print(json.dumps(["ok", code, int(info["features"])]))
        """,
        tmp_path / "pt.shp",
    )
    if prj == "paren":
        # GDAL reads the .prj as WGS 84.
        assert out == ["ok", 4326, 1]
    else:
        # GDAL raises (CRSError: missing , or ]); terrio may read the file without that CRS, but
        # must answer, and must not take the broken definition for a known CRS.
        assert out[0] in ("ok", "error")
        if out[0] == "ok":
            assert out[1] is None or out[1] in ("CRSError",), out


def test_pam_srs_with_a_mismatched_closer(tmp_path):
    import numpy as np

    import terrio.rio as rio

    path = tmp_path / "pam.tif"
    with rio.open(
        path,
        "w",
        driver="GTiff",
        width=4,
        height=4,
        count=1,
        dtype="uint8",
        crs="EPSG:3857",
        transform=rio.transform.from_origin(0, 0, 1, 1),
    ) as ds:
        ds.write(np.zeros((1, 4, 4), "uint8"))
    srs = WGS84_PAREN.replace('"', "&quot;")
    (tmp_path / "pam.tif.aux.xml").write_text(f"<PAMDataset><SRS>{srs}</SRS></PAMDataset>")
    out = _run(
        """
        import json, sys
        import terrio.rio as rio
        with rio.open(sys.argv[1]) as ds:
            crs = ds.crs
            print(json.dumps([crs.is_geographic, crs.to_dict().get("datum")]))
        """,
        path,
    )
    # rasterio 1.5.0: the PAM SRS wins over the GeoTIFF's, read as WGS 84.
    assert out == [True, "WGS84"]


def test_esri_pe_citation_with_a_mismatched_closer(tmp_path):
    rasterio = pytest.importorskip("rasterio")
    import numpy as np
    from rasterio.transform import from_origin

    good, bad = tmp_path / "good.tif", tmp_path / "bad.tif"
    # GDAL stores a Mollweide CRS as an ESRI PE string in a citation GeoKey (as the
    # reproducer was made); one byte becomes `)` in place, so every TIFF offset stays valid.
    with rasterio.open(
        good,
        "w",
        driver="GTiff",
        width=8,
        height=8,
        count=1,
        dtype="uint8",
        crs="+proj=moll +lon_0=0 +datum=WGS84 +units=m +no_defs",
        transform=from_origin(0, 0, 1000, 1000),
    ) as ds:
        ds.write(np.zeros((1, 8, 8), "uint8"))
    data = bytearray(good.read_bytes())
    at = data.find(b"ESRI PE String = ")
    assert at >= 0
    close = data.find(b"]", at)
    data[close : close + 1] = b")"
    bad.write_bytes(bytes(data))
    out = _run(
        """
        import json, sys
        import terrio.rio as rio
        crs = [rio.open(p).crs for p in sys.argv[1:]]
        print(json.dumps([crs[0] == crs[1], crs[1].to_dict().get("proj")]))
        """,
        good,
        bad,
    )
    # rasterio 1.5.0 reads bad.tif's CRS as good.tif's (Mollweide on WGS 84).
    assert out == [True, "moll"]

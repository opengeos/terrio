"""Compare terrio's remote Zarr reads with GDAL's Zarr driver and with zarr-python and xarray.

Usage:
    python scripts/compare_remote_zarr.py [--json FILE] [CASE ...]

Each case opens a public cloud Zarr store in place, as terrio, as GDAL (through ``/vsicurl/``,
``/vsis3/``, ``/vsigs/`` or ``/vsiaz/``) and, when zarr-python and xarray are installed, as
xarray, then checks that ``terrio info`` reports GDAL's size, band count, geotransform and nodata,
and that windows read by ``terrio translate --srcwin`` hold the same values as GDAL's and xarray's.
It also reports the bytes and requests each fetched and the time each took.

Cases:

    eopf      Sentinel-2 L2A from the EOPF sample service (Zarr v2, Blosc Zstandard with bit
              shuffling, X/Y coordinate arrays), found through its STAC API.
    geozarr   Sentinel-2 L2A GeoZarr from the EOPF explorer (Zarr v3, one shard per band with
              inner chunks, ``scale_offset`` and ``cast_value`` codecs, a ``multiscales`` layout
              read as overviews), found through its STAC API. GDAL 3.12 does not read its
              codecs; compared with xarray only (which needs the ``cast-value-rs`` package).
    hrrr      NOAA HRRR on S3 (Zarr v2, float16, Blosc LZ4, coordinates in a parent group).
    cmip6     CMIP6 on Google Cloud Storage (Zarr v2, time as bands, south-up rows).
    daymet    Daymet on Azure, with a Planetary Computer SAS token (Zarr v2, CF grid mapping);
              xarray reads it through ``adlfs``.
    sharded   A Zarr v3 store with sharding written by zarr-python and served over HTTP on a local
              port, compared with the values written (GDAL 3.12 does not read sharded arrays):
              terrio fetches only the shard indexes and the inner chunks a window needs.

Needs GDAL's command-line tools (``gdalinfo``, ``gdal_translate``) on the ``PATH``, NumPy, network
access, and a built ``terrio`` binary (``target/release/terrio``, or the path in ``TERRIO_BIN``).
Exits non-zero when anything differs.
"""

from __future__ import annotations

import argparse
import functools
import http.server
import json
import os
import re
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from pathlib import Path

import numpy as np

TERRIO = os.environ.get("TERRIO_BIN", str(Path(__file__).resolve().parent.parent / "target" / "release" / "terrio"))
RESULTS: list[dict] = []
FAILURES: list[str] = []


def check(ok: bool, case: str, what: str, detail: str = "") -> None:
    """Record and print a comparison.

    Args:
        ok: Whether terrio agrees with the reference.
        case: The case name.
        what: A description of the comparison.
        detail: Numbers to print with it.
    """
    print(("ok   " if ok else "FAIL ") + f"{case}: {what}" + (f" ({detail})" if detail else ""), flush=True)
    RESULTS.append({"case": case, "what": what, "ok": bool(ok), "detail": detail})
    if not ok:
        FAILURES.append(f"{case}: {what}")


def run(cmd: list[str], env: dict | None = None) -> tuple[str, str, float]:
    """Run a command.

    Args:
        cmd: The command and its arguments.
        env: Environment variables to set.

    Returns:
        tuple: The standard output, standard error and wall time in seconds.
    """
    started = time.perf_counter()
    r = subprocess.run(cmd, capture_output=True, text=True, env={**os.environ, **(env or {})}, timeout=1800)
    elapsed = time.perf_counter() - started
    if r.returncode != 0:
        raise RuntimeError(f"{' '.join(cmd)}\n{r.stderr.strip()[-2000:]}")
    return r.stdout, r.stderr, elapsed


def terrio_fetched(stderr: str) -> tuple[int, int]:
    """The requests and bytes of a terrio run with ``TERRIO_HTTP_LOG=YES``.

    Args:
        stderr: The run's standard error.

    Returns:
        tuple: The number of requests and of bytes received.
    """
    sizes = [int(n) for n in re.findall(r"received (\d+) bytes", stderr)]
    return len(re.findall(r"terrio: GET ", stderr)), sum(sizes)


def gdal_fetched(stderr: str) -> tuple[int, int]:
    """The requests and bytes of a GDAL run with ``CPL_VSIL_SHOW_NETWORK_STATS=YES``.

    Args:
        stderr: The run's standard error.

    Returns:
        tuple: The number of GET requests and of bytes downloaded.
    """
    start = stderr.find("Network statistics:")
    if start < 0:
        return 0, 0
    text = stderr[stderr.find("{", start) :]
    try:
        stats, _ = json.JSONDecoder().raw_decode(text)
    except ValueError:
        return 0, 0
    methods = stats.get("methods", {})
    get = methods.get("GET", {})
    return int(get.get("count", 0)), int(get.get("downloaded_bytes", 0))


def read_raw(path: Path, dtype: str, bands: int, h: int, w: int) -> np.ndarray:
    """Read a band-sequential raw file written by ``-of ENVI``.

    Args:
        path: The file.
        dtype: The NumPy data type.
        bands: The number of bands.
        h: The height.
        w: The width.

    Returns:
        numpy.ndarray: The values, shaped ``(bands, h, w)``.
    """
    return np.fromfile(path, dtype=dtype).reshape(bands, h, w)


NUMPY_TYPES = {
    "Byte": "u1",
    "Int8": "i1",
    "UInt16": "<u2",
    "Int16": "<i2",
    "UInt32": "<u4",
    "Int32": "<i4",
    "UInt64": "<u8",
    "Int64": "<i8",
    "Float32": "<f4",
    "Float64": "<f8",
}


def same(a: np.ndarray, b: np.ndarray) -> tuple[bool, str]:
    """Compare two arrays, NaN equal to NaN.

    Args:
        a: terrio's values.
        b: The reference values.

    Returns:
        tuple: Whether they are equal, and a description.
    """
    if a.shape != b.shape:
        return False, f"shapes {a.shape} and {b.shape}"
    a64, b64 = a.astype("f8"), b.astype("f8")
    equal = (a64 == b64) | (np.isnan(a64) & np.isnan(b64))
    n = equal.size
    valid = int((~np.isnan(b64)).sum())
    return bool(equal.all()), f"{int(equal.sum())}/{n} values equal, {valid} not NaN"


class Case:
    """A remote store compared with GDAL and xarray."""

    def __init__(
        self,
        name: str,
        terrio_path: str,
        gdal_path: str | None,
        windows: list[tuple],
        env: dict | None = None,
        xarray_read=None,
        overviews: list[tuple] | None = None,
        overviews_expected: int = 0,
    ) -> None:
        """Describe a case.

        Args:
            name: The case name.
            terrio_path: The dataset name terrio opens.
            gdal_path: The dataset name GDAL opens, or None when GDAL cannot read the store.
            windows: The windows ``(x, y, width, height, bands)`` to compare, in terrio's (north-up)
                rows; ``bands`` is a list of band numbers or None for all.
            env: Environment variables both need (credentials, tokens).
            xarray_read: A function ``(level, y0, y1, x0, x1, bands) -> array`` reading a window of
                the store's array (rows as stored) with xarray, or None.
            overviews: Windows ``(overview, x, y, width, height)`` of overviews to compare with
                xarray, read through terrio's Python bindings.
            overviews_expected: The number of overviews the store has.
        """
        self.name = name
        self.terrio_path = terrio_path
        self.gdal_path = gdal_path
        self.windows = windows
        self.env = env or {}
        self.xarray_read = xarray_read
        self.overviews = overviews or []
        self.overviews_expected = overviews_expected


def compare(case: Case, work: Path) -> None:
    """Run one case.

    Args:
        case: The case.
        work: A scratch directory.
    """
    env = {**case.env, "TERRIO_HTTP_LOG": "YES"}
    out, err, t_info = run([TERRIO, "info", "--json", case.terrio_path], env)
    info = json.loads(out)
    req, nbytes = terrio_fetched(err)
    w, h = info["size"]
    bands = info["band_count"]
    dtype = info["bands"][0]["data_type"]
    gt = info["geo_transform"]
    check(True, case.name, "terrio info", f"{w}x{h}, {bands} band(s), {dtype}, {t_info:.2f} s, {req} requests, {nbytes} bytes")
    south_up = False
    if case.gdal_path:
        genv = {**case.env, "CPL_VSIL_SHOW_NETWORK_STATS": "YES", "GDAL_LOAD_EXTRA_DIM_METADATA_DELAY": "0"}
        gout, gerr, t_ginfo = run(["gdalinfo", "-json", "-nomd", case.gdal_path], genv)
        # The network statistics follow the JSON summary.
        g, end = json.JSONDecoder().raw_decode(gout)
        greq, gbytes = gdal_fetched(gout[end:] + gerr)
        ggt = g["geoTransform"]
        south_up = ggt[5] > 0
        # terrio reads south-up rows north-up.
        flipped = [ggt[0], ggt[1], ggt[2], ggt[3] + ggt[5] * g["size"][1], ggt[4], -ggt[5]] if south_up else ggt
        check(
            g["size"] == [w, h] and len(g["bands"]) == bands,
            case.name,
            "size and bands vs gdalinfo",
            f"GDAL {g['size'][0]}x{g['size'][1]}, {len(g['bands'])} band(s), {t_ginfo:.2f} s, {greq} requests, {gbytes} bytes",
        )
        check(np.allclose(flipped, gt, rtol=0, atol=1e-6), case.name, "geotransform vs gdalinfo", f"{gt}")
        gnd = g["bands"][0].get("noDataValue")
        tnd = info["bands"][0].get("nodata")
        nd_same = (gnd is None and tnd is None) or (
            # As the band's type holds it (GDAL reports a Float32 band's nodata as a double).
            gnd is not None and tnd is not None and np.array_equal(np.float32(gnd), np.float32(tnd), equal_nan=True)
        )
        check(nd_same, case.name, "nodata vs gdalinfo", f"{tnd}")
    for i, (x, y, ww, hh, bl) in enumerate(case.windows):
        blist = bl or list(range(1, bands + 1))
        bargs = [a for b in blist for a in ("-b", str(b))]
        t_out = work / f"{case.name}_{i}_t.bin"
        _, err, t_time = run([TERRIO, "translate", "-f", "ENVI", "--srcwin", str(x), str(y), str(ww), str(hh), *bargs, case.terrio_path, str(t_out)], env)
        treq, tbytes = terrio_fetched(err)
        tv = read_raw(t_out, NUMPY_TYPES[dtype], len(blist), hh, ww)
        what = f"window {x},{y} {ww}x{hh} bands {blist if bl else 'all'}"
        detail = f"terrio {t_time:.2f} s, {treq} requests, {tbytes} bytes"
        if case.gdal_path:
            gy = h - y - hh if south_up else y
            g_out = work / f"{case.name}_{i}_g.bin"
            genv = {**case.env, "GDAL_LOAD_EXTRA_DIM_METADATA_DELAY": "0"}
            _, _, g_time = run(
                ["gdal_translate", "-q", "-of", "ENVI", "-ot", dtype, "-srcwin", str(x), str(gy), str(ww), str(hh), *bargs, case.gdal_path, str(g_out)],
                genv,
            )
            gv = read_raw(g_out, NUMPY_TYPES[dtype], len(blist), hh, ww)
            if south_up:
                gv = gv[:, ::-1, :]
            ok, d = same(tv, gv)
            check(ok, case.name, f"{what} vs GDAL", f"{d}; {detail}; GDAL {g_time:.2f} s")
        if case.xarray_read is not None:
            try:
                started = time.perf_counter()
                ys = (h - y - hh, h - y) if south_up else (y, y + hh)
                xv = np.asarray(case.xarray_read(None, ys[0], ys[1], x, x + ww, blist))
                x_time = time.perf_counter() - started
                if south_up:
                    xv = xv[:, ::-1, :]
                ok, d = same(tv, xv)
                check(ok, case.name, f"{what} vs xarray", f"{d}; {detail}; xarray {x_time:.2f} s")
            except ImportError as e:
                print(f"skip {case.name}: xarray: {e}")
    if case.overviews:
        try:
            import terrio as terrio_py
        except ImportError:
            print(f"skip {case.name}: overviews: terrio's Python bindings are not installed")
            return
        r = terrio_py.open_raster(case.terrio_path, lazy=True)
        check(len(r.overviews) == case.overviews_expected, case.name, "overviews (multiscales levels)", f"{r.overviews}")
        for ov, x, y, ww, hh in case.overviews:
            started = time.perf_counter()
            tv = np.asarray(r.read(window=(x, y, ww, hh), overview=ov))
            t_time = time.perf_counter() - started
            xv = np.asarray(case.xarray_read(ov, y, y + hh, x, x + ww, [1]))[0]
            ok, d = same(tv.reshape(xv.shape), xv)
            ow, oh = r.overviews[ov]
            check(ok, case.name, f"overview {ov} ({ow}x{oh}) window {x},{y} {ww}x{hh} vs xarray", f"{d}; terrio {t_time:.2f} s")


def stac_first(url: str) -> dict:
    """The first item of a STAC API search.

    Args:
        url: The items URL.

    Returns:
        dict: The item.
    """
    with urllib.request.urlopen(url, timeout=60) as r:
        return json.load(r)["features"][0]


def xarray_reader(store: str, group: str | None, var: str, levels: list[str] | None = None, storage_options=None, band_dim=True):
    """A function reading windows of a variable with xarray.

    Args:
        store: The store URL.
        group: The group holding the variable (or its levels).
        var: The variable.
        levels: The groups of the multiscales levels below ``group``, largest first.
        storage_options: fsspec options.
        band_dim: Whether the variable has a leading band dimension.

    Returns:
        callable: ``(overview, y0, y1, x0, x1, bands) -> array`` of shape ``(bands, rows, cols)``,
        ``overview`` None for full resolution.
    """

    @functools.cache
    def dataset(g):
        import xarray as xr

        return xr.open_zarr(store, group=g, consolidated=None, decode_cf=False, mask_and_scale=False, storage_options=storage_options)

    def read(overview, y0, y1, x0, x1, bands):
        g = group
        if levels:
            level = levels[0 if overview is None else overview + 1]
            g = f"{group}/{level}" if group else level
        v = dataset(g)[var]
        if band_dim:
            return np.stack([v[b - 1, y0:y1, x0:x1].values for b in bands])
        return v[y0:y1, x0:x1].values[None, :, :]

    return read


def eopf_case() -> Case:
    """The EOPF Sentinel-2 L2A case (Zarr v2)."""
    item = stac_first("https://stac.core.eopf.eodc.eu/collections/sentinel-2-l2a/items?limit=1")
    href = item["assets"]["B02_10m"]["href"]
    store, sub = href.split(".zarr/", 1)
    store += ".zarr"
    print(f"eopf: {item['id']}")
    return Case(
        "eopf",
        href,
        f'ZARR:"/vsicurl/{store}":/{sub}',
        [(0, 0, 512, 512, None), (4900, 5100, 1000, 700, None), (10000, 10000, 980, 980, None)],
        xarray_read=xarray_reader(store, os.path.dirname(sub), os.path.basename(sub), band_dim=False),
    )


def geozarr_case() -> Case:
    """The EOPF explorer GeoZarr case (Zarr v3, sharded, scale_offset and cast_value)."""
    item = stac_first("https://api.explorer.eopf.copernicus.eu/stac/collections/sentinel-2-l2a/items?limit=1")
    href = item["assets"]["reflectance"]["href"].rstrip("/")
    store, sub = href.split(".zarr/", 1)
    store += ".zarr"
    print(f"geozarr: {item['id']}")
    return Case(
        "geozarr",
        f"{href}/r10m/b02",
        None,
        [(0, 9500, 1024, 1024, None), (100, 10000, 512, 512, None), (2000, 10600, 380, 380, None)],
        xarray_read=xarray_reader(store, sub, "b02", ["r10m", "r20m", "r60m", "r120m", "r360m", "r720m"], band_dim=False),
        overviews=[(1, 0, 1500, 300, 330), (4, 0, 0, 152, 152)],
        overviews_expected=5,
    )


def hrrr_case() -> Case:
    """NOAA HRRR on S3 (Zarr v2, float16)."""
    path = "hrrrzarr/sfc/20240101/20240101_00z_anl.zarr/2m_above_ground/TMP/2m_above_ground/TMP"
    store = "https://hrrrzarr.s3.amazonaws.com/sfc/20240101/20240101_00z_anl.zarr"
    return Case(
        "hrrr",
        f'ZARR:"/vsis3/{path}"',
        f'ZARR:"/vsis3/{path}"',
        [(0, 0, 1799, 1059, None), (700, 300, 400, 333, None)],
        env={"AWS_NO_SIGN_REQUEST": "YES", "AWS_REGION": "us-west-1"},
        xarray_read=xarray_reader(store, "2m_above_ground/TMP/2m_above_ground", "TMP", band_dim=False),
    )


def cmip6_case() -> Case:
    """CMIP6 on Google Cloud Storage (time as bands, south-up)."""
    path = "cmip6/CMIP6/CMIP/NOAA-GFDL/GFDL-CM4/historical/r1i1p1f1/Amon/tas/gr1/v20180701"
    return Case(
        "cmip6",
        f'ZARR:"gs://{path}":/tas',
        f'ZARR:"/vsigs/{path}":/tas',
        [(0, 0, 288, 180, [1, 2, 1980]), (100, 50, 64, 40, [1000])],
        env={"GS_NO_SIGN_REQUEST": "YES"},
        xarray_read=xarray_reader(f"https://storage.googleapis.com/{path}", None, "tas"),
    )


def daymet_case() -> Case:
    """Daymet on Azure (a SAS token from the Planetary Computer)."""
    with urllib.request.urlopen("https://planetarycomputer.microsoft.com/api/sas/v1/token/daymeteuwest/daymet-zarr", timeout=60) as r:
        token = json.load(r)["token"]
    return Case(
        "daymet",
        # With AZURE_STORAGE_ACCOUNT set, az:// and /vsiaz/ names start with the container.
        'ZARR:"az://daymet-zarr/daily/hi.zarr":/tmax',
        'ZARR:"/vsiaz/daymet-zarr/daily/hi.zarr":/tmax',
        [(0, 0, 284, 584, [1]), (50, 100, 100, 200, [400, 14965])],
        env={"AZURE_STORAGE_ACCOUNT": "daymeteuwest", "AZURE_STORAGE_SAS_TOKEN": token},
        xarray_read=xarray_reader(
            "az://daymet-zarr/daily/hi.zarr", None, "tmax", storage_options={"account_name": "daymeteuwest", "sas_token": token}
        ),
    )


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    """A static file server with range requests and without logging."""

    def log_message(self, *args) -> None:
        """Log nothing."""

    def send_head(self):
        """Serve a byte range when asked, else the whole file."""
        rng = self.headers.get("Range")
        path = self.translate_path(self.path)
        if not rng or not os.path.isfile(path):
            return super().send_head()
        size = os.path.getsize(path)
        m = re.match(r"bytes=(\d*)-(\d*)", rng)
        if m.group(1):
            a = int(m.group(1))
            b = min(int(m.group(2)) if m.group(2) else size - 1, size - 1)
        else:
            a, b = max(0, size - int(m.group(2))), size - 1
        f = open(path, "rb")
        f.seek(a)
        self.send_response(206)
        self.send_header("Content-Range", f"bytes {a}-{b}/{size}")
        self.send_header("Content-Length", str(b - a + 1))
        self.end_headers()
        self.wfile.write(f.read(b - a + 1))
        f.close()
        return None


def sharded_case(work: Path) -> Case:
    """A sharded Zarr v3 store written by zarr-python, served on a local port."""
    import zarr

    root = work / "served"
    store = root / "sharded.zarr"
    rng = np.random.default_rng(7)
    data = (rng.random((2, 2048, 3072)) * 4000).astype("<i2")
    g = zarr.open_group(str(store), mode="w", zarr_format=3)
    a = g.create_array(
        "band_data",
        shape=data.shape,
        dtype="int16",
        chunks=(1, 128, 128),
        shards=(1, 1024, 1024),
        dimension_names=["band", "y", "x"],
        fill_value=-1,
    )
    a[:] = data
    a.attrs["_CRS"] = {"url": "http://www.opengis.net/def/crs/EPSG/0/32633"}
    for name, n, start, step in [("x", 3072, 500005.0, 10.0), ("y", 2048, 4600005.0, -10.0)]:
        c = g.create_array(name, shape=(n,), dtype="float64", chunks=(n,), dimension_names=[name])
        c[:] = start + step * np.arange(n)
    zarr.consolidate_metadata(str(store))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(QuietHandler, directory=str(root)))
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_address[1]}/sharded.zarr"
    size = sum(p.stat().st_size for p in store.rglob("*") if p.is_file())
    print(f"sharded: {url}, {size} bytes on disk")

    def read(level, y0, y1, x0, x1, bands):
        return np.stack([data[b - 1, y0:y1, x0:x1] for b in bands])

    # GDAL 3.12 does not read sharded arrays: compared with the values written.
    return Case(
        "sharded",
        f'ZARR:"{url}":/band_data',
        None,
        [(100, 200, 64, 64, [1]), (900, 1000, 300, 200, None), (0, 0, 3072, 2048, None)],
        xarray_read=read,
    )


CASES = {
    "eopf": eopf_case,
    "geozarr": geozarr_case,
    "hrrr": hrrr_case,
    "cmip6": cmip6_case,
    "daymet": daymet_case,
}


def main() -> None:
    """Run the cases named on the command line (all by default)."""
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("cases", nargs="*", default=[*CASES, "sharded"])
    parser.add_argument("--json", help="Write the results to this JSON file")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="terrio-zarr-") as tmp:
        work = Path(tmp)
        for name in args.cases:
            try:
                case = sharded_case(work) if name == "sharded" else CASES[name]()
                compare(case, work)
            except Exception as e:  # noqa: BLE001 - a store that does not respond is reported, not fatal
                check(False, name, f"error: {e}")
    if args.json:
        Path(args.json).write_text(json.dumps(RESULTS, indent=1))
    print(f"{len(FAILURES)} difference(s)")
    sys.exit(1 if FAILURES else 0)


if __name__ == "__main__":
    main()

"""terrio.pyogrio against pyogrio 0.13.0 (GDAL 3.12.4), and the guarantees of its read path.

The parity tests compare with pyogrio itself when it is installed beside terrio, and are skipped
otherwise. The guarantee tests need only geopandas: the values they expect were measured with
pyogrio 0.13.0 on the same bytes (quoted in each docstring), and each one fails under the
mutation of the read path that it names.

The corpus is written byte by byte here, without GDAL.
"""

import json
import os
import struct
import subprocess
import sys
import textwrap
import zipfile

import pytest

gpd = pytest.importorskip("geopandas")
shapely = pytest.importorskip("shapely")
np = pytest.importorskip("numpy")

import terrio.pyogrio as tpg  # noqa: E402

UTM33_PRJ = (
    'PROJCS["WGS_1984_UTM_Zone_33N",GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",'
    'SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],'
    'UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],'
    'PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],'
    'PARAMETER["Central_Meridian",15.0],PARAMETER["Scale_Factor",0.9996],'
    'PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]'
)
WGS84_PRJ = (
    'GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],'
    'PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]'
)
NAMES = ["Zürich", "São Paulo", "Ærø €", "plain"]


# ------------------------------------------------------------------------------------- corpus


def _shape(shape_type, geom):
    if geom is None:
        return struct.pack("<i", 0)
    base = shape_type % 10
    has_z, has_m = shape_type // 10 == 1, shape_type // 10 in (1, 2)
    if base == 1:
        out = struct.pack("<idd", shape_type, geom[0], geom[1])
        if has_z:
            out += struct.pack("<d", geom[2])
        if has_m:
            out += struct.pack("<d", geom[3] if has_z else geom[2])
        return out
    parts = [geom] if base == 8 else geom
    pts = [p for part in parts for p in part]
    xs, ys = [p[0] for p in pts], [p[1] for p in pts]
    out = struct.pack("<i4d", shape_type, min(xs), min(ys), max(xs), max(ys))
    if base == 8:
        out += struct.pack("<i", len(pts))
    else:
        out += struct.pack("<ii", len(parts), len(pts))
        start = 0
        for part in parts:
            out += struct.pack("<i", start)
            start += len(part)
    out += b"".join(struct.pack("<dd", p[0], p[1]) for p in pts)
    for k, flag in ((2, has_z), (3 if has_z else 2, has_m)):
        if flag:
            vals = [p[k] for p in pts]
            out += struct.pack("<dd", min(vals), max(vals)) + b"".join(
                struct.pack("<d", v) for v in vals
            )
    return out


def write_shapefile(
    base,
    shape_type,
    geoms,
    fields,
    records,
    prj=WGS84_PRJ,
    cpg="UTF-8",
    encoding="utf-8",
    ldid=0,
    count=None,
):
    """Writes `base`.shp/.shx/.dbf (and .prj, .cpg when given). `count` overrides the DBF header's
    record count."""
    recs = [_shape(shape_type, g) for g in geoms]
    length = 100 + sum(8 + len(r) for r in recs)
    head = (
        struct.pack(">7i", 9994, 0, 0, 0, 0, 0, length // 2)
        + struct.pack("<ii", 1000, shape_type)
        + bytes(64)
    )
    with open(f"{base}.shp", "wb") as shp, open(f"{base}.shx", "wb") as shx:
        shp.write(head)
        shx.write(head[:24] + struct.pack(">i", (100 + 8 * len(recs)) // 2) + head[28:])
        offset = 100
        for i, r in enumerate(recs):
            shp.write(struct.pack(">ii", i + 1, len(r) // 2) + r)
            shx.write(struct.pack(">ii", offset // 2, len(r) // 2))
            offset += 8 + len(r)
    header_len, record_len = 32 + 32 * len(fields) + 1, 1 + sum(f[2] for f in fields)
    dbf = bytearray(
        struct.pack(
            "<BBBBIHH",
            3,
            125,
            1,
            1,
            len(records) if count is None else count,
            header_len,
            record_len,
        )
    )
    dbf += bytes(17) + bytes([ldid]) + bytes(2)
    for name, kind, width, dec in fields:
        dbf += (
            name.encode()[:11].ljust(11, b"\0")
            + kind.encode()
            + bytes(4)
            + bytes([width, dec])
            + bytes(14)
        )
    dbf += b"\r"
    for rec in records:
        dbf += b" "
        for (_name, kind, width, dec), v in zip(fields, rec, strict=True):
            if v is None:
                cell = b"*" * width if kind in "NF" else b" " * width
            elif kind == "C":
                cell = v.encode(encoding)
            elif kind in "NF":
                cell = (f"{v:.{dec}f}" if dec else str(v)).encode().rjust(width)
            elif kind == "L":
                cell = b"T" if v else b"F"
            else:
                cell = str(v).encode()
            dbf += cell[:width].ljust(width)
    with open(f"{base}.dbf", "wb") as f:
        f.write(bytes(dbf) + b"\x1a")
    for ext, text in (("prj", prj), ("cpg", cpg)):
        if text is not None:
            with open(f"{base}.{ext}", "w") as f:
                f.write(text)
    return f"{base}.shp"


def square(x, y, s, cw=True):
    pts = [(x, y), (x, y + s), (x + s, y + s), (x + s, y), (x, y)]
    return pts if cw else pts[::-1]


def feature(geometry, properties, **extra):
    return {"type": "Feature", **extra, "properties": properties, "geometry": geometry}


def write_geojson(path, features, **extra):
    with open(path, "w", encoding="utf-8") as f:
        json.dump(
            {"type": "FeatureCollection", **extra, "features": features}, f, ensure_ascii=False
        )
    return str(path)


def point(*xy):
    return {"type": "Point", "coordinates": list(xy)}


@pytest.fixture(scope="module")
def corpus(tmp_path_factory):
    d = tmp_path_factory.mktemp("vector")
    c = {}
    c["basic"] = write_geojson(
        d / "basic.geojson",
        [
            feature(point(1.5, 2.25), {"name": "a", "count": 1, "value": 1.5, "flag": True}),
            feature(point(-3, 4), {"name": None, "count": 2, "value": None, "flag": False}),
            feature(
                point(0.1 + 0.2, 1 / 3),
                {"name": "Zürich ✓", "count": None, "value": 2.0, "flag": None},
            ),
        ],
    )
    c["nested"] = write_geojson(
        d / "nested.geojson",
        [
            feature(
                point(0, 0),
                {
                    "obj": {"a": 1, "b": [1, 2]},
                    "ints": [1, 2, 3],
                    "strs": ["x", "y"],
                    "reals": [1.5, 2],
                    "mixed": [1, "a"],
                    "empty": [],
                    "nul": None,
                },
            ),
            feature(
                point(1, 1),
                {
                    "obj": None,
                    "ints": [4],
                    "strs": [],
                    "reals": [3.25],
                    "mixed": [None],
                    "empty": [],
                    "nul": None,
                },
            ),
        ],
    )
    c["int64"] = write_geojson(
        d / "int64.geojson",
        [
            feature(
                point(0, 0), {"i32": 2147483647, "i64": 2147483648, "neg64": -9223372036854775808}
            ),
            feature(point(1, 0), {"i32": -2147483648, "i64": 1, "neg64": 0}),
        ],
    )
    c["mixed"] = write_geojson(
        d / "mixed.geojson",
        [
            feature(
                point(0, 0),
                {"int_str": 1, "int_real": 1, "bool_int": True, "str_int": "a", "real_bool": 1.5},
            ),
            feature(
                point(1, 0),
                {"int_str": "b", "int_real": 2.5, "bool_int": 3, "str_int": 2, "real_bool": False},
            ),
        ],
    )
    c["dates"] = write_geojson(
        d / "dates.geojson",
        [
            feature(
                point(0, 0),
                {
                    "d": "2020-01-31",
                    "dt": "2020-01-31T10:20:30",
                    "dtz": "2020-01-31T10:20:30Z",
                    "dto": "2020-01-31T10:20:30+02:00",
                    "dtms": "2020-01-31T10:20:30.123Z",
                    "t": "10:20:30",
                    "mixd": "2020-01-31",
                    "notdate": "2020-13-45",
                },
            ),
            feature(
                point(1, 0),
                {
                    "d": None,
                    "dt": "2021-06-01T00:00:00",
                    "dtz": "2021-06-01T23:59:59Z",
                    "dto": "2021-06-01T00:00:00-05:00",
                    "dtms": None,
                    "t": "23:59:59",
                    "mixd": "2021-06-01T12:00:00",
                    "notdate": "x",
                },
            ),
        ],
    )
    c["crs_legacy"] = write_geojson(
        d / "crs_legacy.geojson",
        [feature(point(500000, 4649776.22482), {"a": 1})],
        crs={"type": "name", "properties": {"name": "urn:ogc:def:crs:EPSG::32633"}},
    )
    c["crs84"] = write_geojson(
        d / "crs84.geojson",
        [feature(point(10, 20), {"a": 1})],
        crs={"type": "name", "properties": {"name": "urn:ogc:def:crs:OGC:1.3:CRS84"}},
    )
    ring = [[0, 0], [10, 0], [10, 10], [0, 10], [0, 0]]
    hole = [[2, 2], [2, 4], [4, 4], [4, 2], [2, 2]]
    c["geoms"] = write_geojson(
        d / "geoms.geojson",
        [
            feature(point(1, 2, 3), {"k": "pointz"}),
            feature({"type": "MultiPoint", "coordinates": [[0, 0], [1, 1]]}, {"k": "multipoint"}),
            feature(
                {"type": "MultiLineString", "coordinates": [[[0, 0], [1, 1]], [[2, 2], [3, 3]]]},
                {"k": "mline"},
            ),
            feature({"type": "Polygon", "coordinates": [ring, hole]}, {"k": "polygon"}),
            feature(
                {
                    "type": "MultiPolygon",
                    "coordinates": [[ring, hole], [[[20, 20], [30, 20], [30, 30], [20, 20]]]],
                },
                {"k": "mpolygon"},
            ),
            feature(
                {"type": "GeometryCollection", "geometries": [point(5, 5)]}, {"k": "collection"}
            ),
            feature(None, {"k": "null"}),
            feature({"type": "Point", "coordinates": []}, {"k": "empty point"}),
            feature({"type": "LineString", "coordinates": []}, {"k": "empty line"}),
            feature({"type": "Polygon", "coordinates": []}, {"k": "empty polygon"}),
        ],
    )
    c["ids"] = write_geojson(
        d / "ids.geojson",
        [feature(point(0, 0), {"a": 1}, id="first"), feature(point(1, 1), {"a": 2}, id="second")],
    )
    fields = [("NAME", "C", 40, 0), ("ID", "N", 9, 0)]
    pts = [(8.5, 47.4), (-46.6, -23.5), (10.4, 54.9), (0.0, 0.0)]
    recs = [[n, i] for i, n in enumerate(NAMES)]
    latin = [[n.replace("€", "E"), i] for i, n in enumerate(NAMES)]
    c["shp_utf8"] = write_shapefile(d / "utf8", 1, pts, fields, recs)
    c["shp_latin1"] = write_shapefile(
        d / "latin1", 1, pts, fields, latin, cpg="LATIN1", encoding="latin-1"
    )
    c["shp_cp1252"] = write_shapefile(
        d / "cp1252", 1, pts, fields, recs, cpg="1252", encoding="cp1252"
    )
    c["shp_ldid57"] = write_shapefile(
        d / "ldid57", 1, pts, fields, recs, cpg=None, encoding="cp1252", ldid=0x57
    )
    c["shp_ldid03"] = write_shapefile(
        d / "ldid03", 1, pts, fields, recs, cpg=None, encoding="cp1252", ldid=0x03
    )
    c["shp_raw_utf8"] = write_shapefile(d / "raw_utf8", 1, pts, fields, recs, cpg=None)
    c["shp_noprj"] = write_shapefile(d / "noprj", 1, pts, fields, recs, prj=None)
    c["shp_utm"] = write_shapefile(
        d / "utm", 1, [(500000.0, 5000000.0)], fields, [["a", 1]], prj=UTM33_PRJ
    )
    types = [
        ("C10", "C", 10, 0),
        ("N9", "N", 9, 0),
        ("N10", "N", 10, 0),
        ("N19", "N", 19, 0),
        ("N12_3", "N", 12, 3),
        ("F13_5", "F", 13, 5),
        ("L1", "L", 1, 0),
        ("D8", "D", 8, 0),
    ]
    c["shp_types"] = write_shapefile(
        d / "types",
        1,
        [(0, 0), (1, 1)],
        types,
        [
            ["  lead", 123456789, 1234567890, 1234567890123456789, 1.5, 2.25, True, "20200131"],
            ["abc", -1, -9, 0, -0.001, 1e5, False, "19991231"],
        ],
    )
    c["shp_types_null"] = write_shapefile(
        d / "types_null",
        1,
        [(0, 0), (1, 1)],
        types,
        [["abc", 1, 2, 3, 1.5, 2.25, True, "20200131"], [None] * 8],
    )
    polys = [
        [square(0, 0, 10), square(2, 2, 2, cw=False)],
        [square(0, 0, 1), square(5, 5, 1)],
        None,
    ]
    c["shp_polygons"] = write_shapefile(
        d / "polygons", 5, polys, fields, [["hole", 1], ["two", 2], ["null", 3]]
    )
    zpoly = [[[p + (1.0 + k % 4, 100.0 + k % 4) for k, p in enumerate(square(0, 0, 10))]]]
    c["shp_polygonzm"] = write_shapefile(d / "polygonzm", 15, zpoly, fields, [["z", 1]])
    c["shp_lines_m"] = write_shapefile(
        d / "lines_m",
        23,
        [[[(0, 0, 0.5), (1, 1, 1.5)]], [[(0, 0, 1), (1, 1, 2)], [(5, 5, 3), (6, 6, 4)]]],
        fields,
        [["one", 1], ["two", 2]],
    )
    c["shp_multipoint"] = write_shapefile(
        d / "multipoint", 8, [[(0, 0), (1, 1)], [(5, 5)]], fields, [["two", 1], ["one", 2]]
    )
    z = d / "zipped.zip"
    with zipfile.ZipFile(z, "w") as zf:
        for ext in ("shp", "shx", "dbf", "prj", "cpg"):
            zf.write(d / f"utf8.{ext}", f"inner.{ext}")
    c["zip"] = str(z)
    sub = d / "dir_shapefile"
    sub.mkdir()
    write_shapefile(sub / "inside", 5, polys[:2], fields, [["hole", 1], ["two", 2]], prj=UTM33_PRJ)
    c["directory"] = str(sub)
    return c


# ------------------------------------------------------------------------- isolated processes


def run_python(code, timeout=120, limit=None):
    """Runs `code` in a fresh interpreter with terrio.pyogrio registered as `pyogrio`, optionally
    under an address-space limit (bytes)."""
    prelude = "import sys, terrio.pyogrio\nsys.modules['pyogrio'] = terrio.pyogrio\n"
    if limit:
        prelude = (
            f"import resource\nresource.setrlimit(resource.RLIMIT_AS, ({limit}, {limit}))\n"
        ) + prelude
    # Under RLIMIT_AS, thread pools must stay small: OpenBLAS starts one thread per core at
    # import (numpy), and each thread reserves address space.
    env = dict(
        os.environ,
        OPENBLAS_NUM_THREADS="1",
        MALLOC_ARENA_MAX="2",
        RAYON_NUM_THREADS="4",
        PYTHONPATH="",
    )
    return subprocess.run(
        [sys.executable, "-c", prelude + textwrap.dedent(code)],
        capture_output=True,
        text=True,
        timeout=timeout,
        env=env,
    )


# --------------------------------------------------------------------------------- guarantees


def test_crs_comes_from_prj(corpus):
    """Mutation: ignore the .prj. pyogrio 0.13.0 reads EPSG:32633 from this ESRI
    WKT, EPSG:4326 from the WGS 84 one, and no CRS without a .prj."""
    assert tpg.read_dataframe(corpus["shp_utm"]).crs.to_epsg() == 32633
    assert tpg.read_dataframe(corpus["shp_utf8"]).crs.to_epsg() == 4326
    assert tpg.read_dataframe(corpus["directory"]).crs.to_epsg() == 32633
    assert tpg.read_dataframe(corpus["shp_noprj"]).crs is None


@pytest.mark.parametrize(
    ("case", "expected"),
    [
        ("shp_utf8", NAMES),
        ("shp_latin1", [n.replace("€", "E") for n in NAMES]),
        ("shp_cp1252", NAMES),
        ("shp_ldid03", NAMES),
        ("shp_ldid57", ["Zürich", "São Paulo", "Ærø \x80", "plain"]),
        ("shp_raw_utf8", [n.encode("utf-8").decode("latin-1") for n in NAMES]),
    ],
)
def test_cpg_decides_dbf_encoding(corpus, case, expected):
    """Mutation: ignore the .cpg. As pyogrio 0.13.0 decodes them: the .cpg names
    the encoding, else the DBF language driver id (0x03 is Windows-1252, 0x57 ISO-8859-1), else
    nothing is recoded and pyogrio reads the bytes as ISO-8859-1."""
    assert tpg.read_dataframe(corpus[case])["NAME"].tolist() == expected


def test_rows_slice_returns_exactly_those_rows(tmp_path):
    """Mutation: off by one, through geopandas' own `rows=` mapping in a process
    where terrio.pyogrio is `pyogrio`. pyogrio reads `slice(a, a)` as every row from `a`
    (max_features=0 means no limit)."""
    n = 50
    path = write_shapefile(
        tmp_path / "rows",
        1,
        [(i, i) for i in range(n)],
        [("ID", "N", 9, 0)],
        [[i] for i in range(n)],
    )
    code = f"""
        import geopandas as gpd, json
        out = {{}}
        for a, b in [(0, 10), (3, 7), (49, 50), (45, 60), (60, 70), (10, 10)]:
            df = gpd.read_file({path!r}, rows=slice(a, b))
            out[f"{{a}}:{{b}}"] = [df["ID"].tolist(), [g.x for g in df.geometry]]
        out["int"] = gpd.read_file({path!r}, rows=5)["ID"].tolist()
        print(json.dumps(out))
    """
    r = run_python(code)
    assert r.returncode == 0, r.stderr
    got = json.loads(r.stdout)
    for a, b in [(0, 10), (3, 7), (49, 50), (45, 60), (60, 70)]:
        rows = list(range(a, min(b, n)))
        assert got[f"{a}:{b}"] == [rows, [float(i) for i in rows]], (a, b)
    assert got["10:10"][0] == list(range(10, n))
    assert got["int"] == list(range(5))


def test_dbf_record_count_does_not_abort(tmp_path):
    """Vector records (mutation: allocate the header's record count infallibly). A
    DBF declaring 2**31 - 1 records, read under a 2 GiB address-space limit, gives the records the
    file holds; it never aborts."""
    path = write_shapefile(
        tmp_path / "count", 1, [(0, 0), (1, 1)], [("ID", "N", 9, 0)], [[1], [2]], count=2**31 - 1
    )
    r = run_python(
        f"""
        import terrio.pyogrio as tpg
        print(tpg.read_dataframe({path!r})["ID"].tolist())
    """,
        limit=2 * 1024**3,
    )
    assert r.returncode == 0, (r.returncode, r.stderr[-2000:])
    assert r.stdout.strip() == "[1, 2]"


def test_geojson_too_large_for_memory_errors(tmp_path):
    """GeoJSON (mutation: parse without reserving its memory first). A document
    whose parse needs more memory than a 1 GiB address-space limit leaves fails with an error,
    and the process lives on."""
    path = tmp_path / "big.geojson"
    with open(path, "w") as f:
        f.write('{"type":"FeatureCollection","features":[')
        row = (
            '{"type":"Feature","properties":{"a":1},'
            '"geometry":{"type":"Point","coordinates":[0,0]}},'
        )
        f.write(row * (90 * 1024**2 // len(row)))
        f.write('{"type":"Feature","properties":{},"geometry":null}]}')
    r = run_python(
        f"""
        import terrio.pyogrio as tpg
        try:
            tpg.read_dataframe({str(path)!r})
            print("read")
        except Exception as e:
            print("error", type(e).__name__, str(e)[:200])
    """,
        limit=1024**3,
        timeout=300,
    )
    assert r.returncode == 0, (r.returncode, r.stderr[-2000:])
    assert r.stdout.startswith("error DataSourceError"), r.stdout


def test_allow_list_refuses_other_drivers(corpus):
    """Mutation: bypass the check on this path. With only GTiff allowed, a GeoJSON
    is refused whether it is a path, a file object or bytes, and so is a zipped shapefile; with
    GeoJSON and ESRI Shapefile allowed (an ingestion pipeline's list), both read."""
    code = f"""
        import io, terrio, terrio.pyogrio as tpg
        terrio.set_allowed_drivers(["GTiff"])
        data = open({corpus["basic"]!r}, "rb").read()
        sources = [{corpus["basic"]!r}, io.BytesIO(data), data, {corpus["zip"]!r}]
        for src in sources + [{corpus["shp_utf8"]!r}]:
            try:
                tpg.read_dataframe(src)
                print("read")
            except tpg.errors.DataSourceError as e:
                print("refused", "not allowed" in str(e))
    """
    r = run_python(code)
    assert r.returncode == 0, r.stderr
    assert r.stdout.split("\n")[:5] == ["refused True"] * 5
    r = run_python(f"""
        import terrio, terrio.pyogrio as tpg
        terrio.set_allowed_drivers(["GeoJSON", "ESRI Shapefile"])
        a = tpg.read_dataframe({corpus["basic"]!r})
        b = tpg.read_dataframe({corpus["directory"]!r})
        print(len(a), len(b))
    """)
    assert r.returncode == 0, r.stderr
    assert r.stdout.strip() == "3 2"


# ------------------------------------------------------------------------------------- parity


def _snapshot(df):
    """What parity compares: column order, dtypes, index, values, WKB and CRS."""
    geom = df.geometry.name if isinstance(df, gpd.GeoDataFrame) else None
    attrs = df.drop(columns=[geom]) if geom else df

    def norm(v):
        if isinstance(v, np.ndarray):
            return ("array", str(v.dtype), v.tolist())
        if isinstance(v, float) and v != v:
            return "NaN"
        return v

    snap = {
        "columns": list(df.columns),
        "dtypes": [str(t) for t in df.dtypes],
        "index": (type(df.index).__name__, list(df.index)),
        "values": {c: [norm(v) for v in attrs[c].tolist()] for c in attrs.columns},
    }
    if geom:
        snap["wkb"] = [
            None if g is None else shapely.to_wkb(g, flavor="iso", output_dimension=4)
            for g in df.geometry
        ]
        snap["crs"] = None if df.crs is None else (df.crs.to_epsg(), df.crs.to_wkt())
    return snap


PARITY_CASES = [
    "basic",
    "nested",
    "int64",
    "mixed",
    "dates",
    "crs_legacy",
    "crs84",
    "geoms",
    "ids",
    "shp_utf8",
    "shp_latin1",
    "shp_cp1252",
    "shp_ldid57",
    "shp_ldid03",
    "shp_raw_utf8",
    "shp_noprj",
    "shp_utm",
    "shp_types",
    "shp_types_null",
    "shp_polygons",
    "shp_polygonzm",
    "shp_lines_m",
    "shp_multipoint",
    "zip",
    "directory",
]


@pytest.fixture(scope="module")
def pyogrio():
    import importlib

    mod = pytest.importorskip("pyogrio")
    if mod is tpg or getattr(mod, "__terrio_version__", None):
        pytest.skip("pyogrio is terrio.pyogrio here")
    return importlib.import_module("pyogrio")


def _skip_before_pyogrio_0_13(pyogrio):
    """Skips the test on pyogrio before 0.13, which leaves out time columns; terrio reads them.

    Args:
        pyogrio: The pyogrio module compared with.
    """
    if tuple(int(n) for n in pyogrio.__version__.split(".")[:2]) < (0, 13):
        pytest.skip("pyogrio before 0.13 leaves out time columns")


@pytest.mark.filterwarnings("ignore")
@pytest.mark.parametrize("case", PARITY_CASES)
def test_read_parity(pyogrio, corpus, case):
    """Reads each corpus file with pyogrio and terrio.pyogrio: identical snapshots."""
    if case == "dates":
        _skip_before_pyogrio_0_13(pyogrio)
    assert _snapshot(tpg.read_dataframe(corpus[case])) == _snapshot(
        pyogrio.read_dataframe(corpus[case])
    )


@pytest.mark.filterwarnings("ignore")
@pytest.mark.parametrize(("skip", "limit"), [(0, 2), (1, 2), (2, None), (3, 5)])
def test_feature_range_parity(pyogrio, corpus, skip, limit):
    for case in ("shp_utf8", "geoms"):
        ours = tpg.read_dataframe(corpus[case], skip_features=skip, max_features=limit)
        theirs = pyogrio.read_dataframe(corpus[case], skip_features=skip, max_features=limit)
        assert _snapshot(ours) == _snapshot(theirs), case


@pytest.mark.filterwarnings("ignore")
def test_file_object_and_bytes_parity(pyogrio, corpus):
    """A statistics service reads `gpd.read_file(handle)`: file objects and bytes, GeoJSON and
    zipped."""
    import io

    _skip_before_pyogrio_0_13(pyogrio)
    for case in ("basic", "dates", "zip"):
        data = open(corpus[case], "rb").read()
        ref = _snapshot(pyogrio.read_dataframe(io.BytesIO(data)))
        assert _snapshot(tpg.read_dataframe(io.BytesIO(data))) == ref, case
        assert _snapshot(tpg.read_dataframe(data)) == ref, case


@pytest.mark.filterwarnings("ignore")
def test_write_geojson_bytes_match_gdal(pyogrio, corpus, tmp_path):
    """GeoDataFrame.to_file(driver="GeoJSON") as GDAL writes it: identical bytes."""
    import pandas as pd

    df = gpd.GeoDataFrame(
        {
            "i": np.array([1, 2, 3], dtype="int64"),
            "f": [0.1 + 0.2, 1 / 3, np.nan],
            "s": ["a", None, 'é "/\\\n'],
            "b": [True, False, True],
            "d": pd.to_datetime(["2020-01-01", "2020-01-02 10:20:30.123", None], format="ISO8601"),
        },
        geometry=[
            shapely.Point(0.1 + 0.2, 1 / 3),
            shapely.Polygon(square(0, 0, 10), [square(2, 2, 2, False)]),
            None,
        ],
        crs="EPSG:4326",
    )
    frames = {
        "mixed": df,
        "utm": df.to_crs(32633),
        "no_crs": df.set_crs(None, allow_override=True),
        "read_back": tpg.read_dataframe(corpus["shp_polygons"]),
    }
    for name, frame in frames.items():
        theirs, ours = tmp_path / f"{name}_gdal.geojson", tmp_path / f"{name}_terrio.geojson"
        pyogrio.write_dataframe(frame, str(theirs), driver="GeoJSON")
        tpg.write_dataframe(frame, str(ours), driver="GeoJSON", layer=theirs.stem)
        assert ours.read_bytes() == theirs.read_bytes(), name


# GeoJSON without a `crs` member: GDAL's CRS by the heights of the layer type it infers.
# Measured with pyogrio 0.13.0 (GDAL 3.12.4) `read_info` on these documents.
_P2 = {"type": "Point", "coordinates": [10.0, 20.0]}
_P3 = {"type": "Point", "coordinates": [10.0, 20.0, 30.0]}
GEOJSON_DEFAULT_CRS = {
    "point_z": ([_P3], "EPSG:4979"),
    "point_2d": ([_P2], "EPSG:4326"),
    "2d_then_3d": ([_P2, _P3], "EPSG:4979"),
    "3d_then_2d": ([_P3, _P2], "EPSG:4979"),
    "mixed_types_3d": (
        [_P3, {"type": "LineString", "coordinates": [[10, 20, 1], [11, 21, 2]]}],
        "EPSG:4326",
    ),
    "first_position_2d": (
        [{"type": "LineString", "coordinates": [[1, 2], [3, 4, 5]]}],
        "EPSG:4326",
    ),
    "first_position_3d": (
        [{"type": "LineString", "coordinates": [[1, 2, 3], [3, 4]]}],
        "EPSG:4979",
    ),
    "collection_first_member_3d": (
        [{"type": "GeometryCollection", "geometries": [_P3, _P2]}],
        "EPSG:4979",
    ),
    "collection_first_member_2d": (
        [{"type": "GeometryCollection", "geometries": [_P2, _P3]}],
        "EPSG:4326",
    ),
    "null_then_3d": ([None, _P3], "EPSG:4979"),
    "four_values": ([{"type": "Point", "coordinates": [10, 20, 30, 40]}], "EPSG:4326"),
}


def _geojson(tmp_path, name, geometries):
    path = tmp_path / f"{name}.geojson"
    features = [
        {"type": "Feature", "properties": {"i": i}, "geometry": g} for i, g in enumerate(geometries)
    ]
    path.write_text(json.dumps({"type": "FeatureCollection", "features": features}))
    return str(path)


@pytest.mark.parametrize("name", sorted(GEOJSON_DEFAULT_CRS))
def test_geojson_default_crs_follows_the_heights_as_gdal(tmp_path, name):
    """3-D coordinates and no `crs` read as EPSG:4979, as GDAL reads them (the USGS
    earthquake feed); 2-D, mixed geometry types and a 2-D first position as EPSG:4326."""
    geometries, expected = GEOJSON_DEFAULT_CRS[name]
    path = _geojson(tmp_path, name, geometries)
    assert tpg.read_info(path)["crs"] == expected
    assert tpg.read_dataframe(path).crs.to_epsg() == int(expected.split(":")[1])


@pytest.mark.filterwarnings("ignore")
@pytest.mark.parametrize("name", sorted(GEOJSON_DEFAULT_CRS))
def test_geojson_default_crs_parity(pyogrio, tmp_path, name):
    path = _geojson(tmp_path, name, GEOJSON_DEFAULT_CRS[name][0])
    assert tpg.read_info(path)["crs"] == pyogrio.read_info(path)["crs"]
    assert tpg.read_dataframe(path).crs == pyogrio.read_dataframe(path).crs


# The layer geometry type GDAL's GeoJSON drivers declare (`read_info`'s `geometry_type`): from the
# features' geometry objects in turn (the first position's values, a collection's first member,
# type names without case, `measures`), `Unknown` once two types differ or one is unknown and for
# a layer without geometries; a bare geometry's from the geometry read. Measured with pyogrio
# 0.13.0 (GDAL 3.12.4) `read_info` on these documents (a 26-document GeoJSON matrix compared with
# pyogrio); the first eight differed when the type was inferred from the geometries read.
def _fc(*geometries, **members):
    features = [
        {"type": "Feature", "properties": {"i": i}, "geometry": g} for i, g in enumerate(geometries)
    ]
    return {"type": "FeatureCollection", **members, "features": features}


_L3 = {"type": "LineString", "coordinates": [[10.0, 20.0, 1.0], [11.0, 21.0, 2.0]]}
_LINE_2D_FIRST = {"type": "LineString", "coordinates": [[1.0, 2.0], [3.0, 4.0, 5.0]]}
_CRS84 = {"type": "name", "properties": {"name": "urn:ogc:def:crs:OGC:1.3:CRS84"}}
GEOJSON_GEOMETRY_TYPE = {
    "point3d_line3d": (_fc(_P3, _L3), "Unknown"),
    "collection_first_member_2d": (
        _fc({"type": "GeometryCollection", "geometries": [_P2, _P3]}),
        "GeometryCollection",
    ),
    "lowercase_type": (_fc({"type": "point", "coordinates": [10.0, 20.0, 30.0]}), "Point Z"),
    "all_null": (_fc(None, None), "Unknown"),
    "four_values": (_fc({"type": "Point", "coordinates": [10.0, 20.0, 30.0, 40.0]}), "Point"),
    "measures_3": (
        _fc({"type": "Point", "coordinates": [10.0, 20.0, 30.0], "measures": {"enabled": True}}),
        "Point",
    ),
    "unknown_then_3d": (_fc({"type": "Pointy", "coordinates": [1.0, 2.0, 3.0]}, _P3), "Unknown"),
    "bare_line_2d_first_3d_second": (_LINE_2D_FIRST, "LineString Z"),
    "point_z": (_fc(_P3), "Point Z"),
    "point_2d": (_fc(_P2), "Point"),
    "2d_then_3d": (_fc(_P2, _P3), "Point Z"),
    "3d_then_2d": (_fc(_P3, _P2), "Point Z"),
    "line_2d_first_3d_second": (_fc(_LINE_2D_FIRST), "LineString"),
    "line_3d_first_2d_second": (
        _fc({"type": "LineString", "coordinates": [[1.0, 2.0, 3.0], [3.0, 4.0]]}),
        "LineString Z",
    ),
    "collection_first_member_3d": (
        _fc({"type": "GeometryCollection", "geometries": [_P3, _P2]}),
        "GeometryCollection Z",
    ),
    "null_then_3d": (_fc(None, _P3), "Point Z"),
    "crs_member": (_fc(_P3, crs=_CRS84), "Point Z"),
    "measures_4": (
        _fc(
            {
                "type": "Point",
                "coordinates": [10.0, 20.0, 30.0, 40.0],
                "measures": {"enabled": True},
            }
        ),
        "Point Z",
    ),
    "bare_point_2d": (_P2, "Point"),
    "single_feature_3d": (
        {"type": "Feature", "properties": {"a": 1}, "geometry": _P3},
        "Point Z",
    ),
    "empty_coordinates_then_3d": (
        _fc({"type": "Point", "coordinates": []}, _P3),
        "Point Z",
    ),
    "multipolygon_z": (
        _fc(
            {
                "type": "MultiPolygon",
                "coordinates": [[[[0, 0, 1], [1, 0, 1], [1, 1, 1], [0, 0, 1]]]],
            }
        ),
        "MultiPolygon Z",
    ),
    "polygon_2d_hole_3d": (
        _fc(
            {
                "type": "Polygon",
                "coordinates": [
                    [[0, 0], [1, 0], [1, 1], [0, 0]],
                    [[0.2, 0.2, 5], [0.4, 0.2, 5], [0.4, 0.4, 5], [0.2, 0.2, 5]],
                ],
            }
        ),
        "Polygon",
    ),
    "bare_collection_2d_then_3d": (
        {"type": "GeometryCollection", "geometries": [_P2, _P3]},
        "GeometryCollection Z",
    ),
    "mixed_types_2d": (
        _fc(_P2, {"type": "LineString", "coordinates": [[1.0, 2.0], [3.0, 4.0]]}),
        "Unknown",
    ),
    "sequence_3d": ([{"type": "Feature", "properties": {}, "geometry": _P3}] * 2, "Point Z"),
}


def _geometry_type_document(tmp_path, name):
    doc = GEOJSON_GEOMETRY_TYPE[name][0]
    if isinstance(doc, list):
        path = tmp_path / f"{name}.geojsonl"
        path.write_text("\n".join(json.dumps(f) for f in doc) + "\n")
    else:
        path = tmp_path / f"{name}.geojson"
        path.write_text(json.dumps(doc))
    return str(path)


@pytest.mark.parametrize("name", list(GEOJSON_GEOMETRY_TYPE))
def test_geojson_geometry_type_is_the_type_gdal_declares(tmp_path, name):
    path = _geometry_type_document(tmp_path, name)
    assert tpg.read_info(path)["geometry_type"] == GEOJSON_GEOMETRY_TYPE[name][1]


@pytest.mark.filterwarnings("ignore")
@pytest.mark.parametrize("name", list(GEOJSON_GEOMETRY_TYPE))
def test_geojson_geometry_type_parity(pyogrio, tmp_path, name):
    path = _geometry_type_document(tmp_path, name)
    assert tpg.read_info(path)["geometry_type"] == pyogrio.read_info(path)["geometry_type"]

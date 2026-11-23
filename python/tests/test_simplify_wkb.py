"""terrio.simplify_wkb: Douglas-Peucker on WKB, against GEOS's DouglasPeuckerSimplifier (shapely).

GEOS is a behavioral oracle only: terrio's implementation is derived from JTS. GEOS rebuilds an
invalid polygonal result with a zero-width buffer and terrio does not, so features whose result
is invalid in either are left out of the vertex comparisons.
"""

import math
import random

import pytest
import shapely

import terrio


def _dp(geoms, tolerance, **kw):
    wkb = [None if g is None else shapely.to_wkb(g) for g in geoms]
    out = terrio.simplify_wkb(wkb, tolerance, **kw)
    return [None if b is None else shapely.from_wkb(b) for b in out]


def _coords(g):
    return shapely.get_coordinates(g).tolist()


def _same_vertices(a, b):
    """The same type and vertices; a multi geometry of one part (terrio keeps the input's type
    code) matches that part (JTS's and GEOS's `buildGeometry` return it alone)."""
    if a.geom_type.startswith("Multi") and not b.geom_type.startswith("Multi"):
        if shapely.get_num_geometries(a) == 1:
            a = shapely.get_geometry(a, 0)
    same_type = a.geom_type == b.geom_type
    return same_type and shapely.equals_exact(a, b, tolerance=0) and _coords(a) == _coords(b)


def _wobbly_ring(rng, cx, cy, radius, n, noise):
    pts = []
    for k in range(n):
        a = 2 * math.pi * k / n
        r = radius * (1 + noise * (rng.random() - 0.5))
        pts.append((cx + r * math.cos(a), cy + r * math.sin(a)))
    return pts


def _comparable(a, b):
    return shapely.is_valid(b) and shapely.is_valid(a)


def test_lines_and_rings_like_geos():
    line = shapely.LineString([(0, 0), (1, 0.05), (2, 0)])
    # Ring start points within the tolerance of their neighbours' segment, on a side, at a corner
    # (kept: too far) and in clockwise rings.
    square = shapely.Polygon([(5, 0), (10, 0), (10, 10), (0, 10), (0, 0), (5, 0)])
    cw = shapely.Polygon([(0, 0), (0, 10), (10, 10), (10, 0), (5, 0.01)])
    corner = shapely.Polygon([(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)])
    closed = shapely.LineString([(5, 0), (10, 0), (10, 10), (0, 10), (0, 0), (5, 0)])
    point = shapely.Point(1, 2)
    multi = shapely.MultiLineString([[(0, 0), (1, 0.01), (2, 0)], [(5, 5), (6, 6)]])
    holes = shapely.Polygon(
        [(0, 0), (100, 0), (100, 100), (0, 100)],
        [[(50, 10), (60, 10), (60, 10.01), (60, 20), (50, 20)]],
    )
    collection = shapely.GeometryCollection([line, square, point])
    for g in [line, square, cw, corner, closed, point, multi, holes, collection]:
        for oriented in [g, shapely.orient_polygons(g, exterior_cw=True)]:
            (ours,) = _dp([oriented], 0.1)
            expected = shapely.simplify(oriented, 0.1, preserve_topology=False)
            assert _same_vertices(ours, expected), oriented.wkt


def test_a_thin_triangle_collapses_as_in_jts_and_geos():
    """The start of a triangle within the tolerance of its opposite side is dropped, as JTS's
    `simplifyRingEndpoint` drops it, and the ring collapses: GEOS empties the polygon, as
    `collapse="drop"` does; `"keep"` keeps the triangle."""
    triangle = shapely.Polygon([(5, 0.3), (0, 0), (10, 0)])
    assert shapely.simplify(triangle, 0.5, preserve_topology=False).is_empty
    (dropped,) = _dp([triangle], 0.5, collapse="drop")
    assert dropped.is_empty
    (kept,) = _dp([triangle], 0.5)
    assert _same_vertices(kept, triangle)


def test_random_polygons_and_lines_match_geos_vertex_for_vertex():
    rng = random.Random(15)
    geoms = []
    for _ in range(300):
        shell = _wobbly_ring(rng, 0, 0, 100, rng.randint(8, 400), rng.choice([0.01, 0.05, 0.2]))
        centre = (rng.uniform(-30, 30), rng.uniform(-30, 30))
        holes = [_wobbly_ring(rng, *centre, 10, rng.randint(6, 60), 0.05)[::-1]]
        start = rng.randrange(len(shell))
        shell = shell[start:] + shell[:start]
        if rng.random() < 0.5:
            shell = shell[::-1]
        geoms.append(shapely.Polygon(shell, holes if rng.random() < 0.5 else None))
        geoms.append(shapely.LineString(shell[: rng.randint(2, len(shell))]))
    far = shapely.Polygon([(x + 500, y) for x, y in geoms[2].exterior.coords])
    geoms.append(shapely.MultiPolygon([geoms[0], far]))
    geoms.append(None)
    compared = 0
    for tol in [0.0, 0.1, 1.0, 5.0]:
        ours = _dp(geoms, tol, collapse="drop")
        geos = shapely.simplify(geoms, tol, preserve_topology=False)
        for g, a, b in zip(geoms, ours, geos, strict=True):
            if g is None:
                assert a is None
                continue
            if not _comparable(a, b):
                continue
            assert _same_vertices(a, b), (tol, g.wkt[:80])
            compared += 1
    assert compared > 2000


def test_collapse_keep_and_drop():
    shell = [(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)]
    thin = [(1, 1), (2, 1), (2, 1.05), (1, 1.05), (1, 1)]
    hole = [(5, 5), (6, 5), (6, 6), (5, 6), (5, 5)]
    p = shapely.Polygon(shell, [thin, hole])
    out, stats = terrio.simplify_wkb([shapely.to_wkb(p)], 0.1, stats=True)
    assert shapely.from_wkb(out[0]).equals_exact(p, 0)
    assert stats["rings_kept"] == 1 and stats["rings_dropped"] == 0
    out, stats = terrio.simplify_wkb([shapely.to_wkb(p)], 0.1, collapse="drop", stats=True)
    assert _same_vertices(
        shapely.from_wkb(out[0]), shapely.simplify(p, 0.1, preserve_topology=False)
    )
    assert stats["rings_dropped"] == 1
    sliver = shapely.Polygon(thin)
    (dropped,) = _dp([sliver], 0.1, collapse="drop")
    assert dropped.is_empty and shapely.simplify(sliver, 0.1, preserve_topology=False).is_empty
    (kept,) = _dp([sliver], 0.1)
    assert kept.equals_exact(sliver, 0)
    # A collapsed member leaves its multipolygon, which keeps its type with one part left.
    multi = shapely.MultiPolygon([shapely.Polygon(shell), sliver])
    (ours,) = _dp([multi], 0.1, collapse="drop")
    assert _same_vertices(ours, shapely.simplify(multi, 0.1, preserve_topology=False))


def test_stats_and_z():
    g = shapely.LineString([(0, 0, 1), (1, 0.05, 2), (2, 0, 3)])
    out, stats = terrio.simplify_wkb([shapely.to_wkb(g), None], 0.1, stats=True)
    assert _coords(shapely.from_wkb(out[0])) == [[0, 0], [2, 0]]
    assert shapely.get_coordinates(shapely.from_wkb(out[0]), include_z=True)[:, 2].tolist() == [
        1,
        3,
    ]
    assert out[1] is None
    assert {k: stats[k] for k in ("features", "nulls", "vertices_in", "vertices_out")} == {
        "features": 2,
        "nulls": 1,
        "vertices_in": 3,
        "vertices_out": 2,
    }
    assert stats["seconds"] >= 0


def test_errors():
    line = shapely.to_wkb(shapely.LineString([(0, 0), (1, 1)]))
    with pytest.raises(ValueError, match="tolerance"):
        terrio.simplify_wkb([line], -1)
    with pytest.raises(TypeError):
        terrio.simplify_wkb([line], 1, method="preserve-topology")
    with pytest.raises(ValueError, match="collapse"):
        terrio.simplify_wkb([line], 1, collapse="maybe")
    with pytest.raises(ValueError, match="feature 1"):
        terrio.simplify_wkb([line, line[:12]], 1)
    with pytest.raises(ValueError, match="bytes"):
        terrio.simplify_wkb(["LINESTRING (0 0, 1 1)"], 1)


def test_arrow_arrays_keep_their_type():
    pa = pytest.importorskip("pyarrow")  # installed in the real-data job
    wkb = [shapely.to_wkb(shapely.LineString([(0, 0), (1, 0.05), (2, 0)])), None]
    for typ in [pa.binary(), pa.large_binary(), pa.binary_view()]:
        out = terrio.simplify_wkb(pa.array(wkb, typ), 0.1)
        assert isinstance(out, pa.Array) and out.type == typ
        assert out.null_count == 1
        assert _coords(shapely.from_wkb(out[0].as_py())) == [[0, 0], [2, 0]]
    chunked = pa.chunked_array([pa.array(wkb[:1], pa.binary()), pa.array(wkb[1:], pa.binary())])
    assert len(terrio.simplify_wkb(chunked, 0.1)) == 2
    with pytest.raises(ValueError, match="WKB"):
        terrio.simplify_wkb(pa.array([1, 2]), 0.1)


def test_layer_simplify_douglas_peucker():
    fc = {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {},
                "geometry": {"type": "LineString", "coordinates": [[0, 0], [5, 0.1], [10, 0]]},
            }
        ],
    }
    out = terrio.simplify(fc, 1.0, method="douglas-peucker").to_geojson()
    assert out["features"][0]["geometry"]["coordinates"] == [[0, 0], [10, 0]]
    with pytest.raises(ValueError):
        terrio.simplify(fc, 1.0, method="nope")


@pytest.mark.parametrize(
    ("name", "tolerances"),
    [
        ("TN_Counties.geojson", [0.001, 0.01, 0.05]),
        ("countries.geojson", [0.01, 0.1, 1.0]),
        ("las_vegas_roads.geojson", [1e-5, 1e-4]),
    ],
)
def test_real_layers_match_ogr_simplify(data_dir, ogr, name, tolerances):
    """Every feature of real layers against OGR's `Simplify` (GEOS's DouglasPeuckerSimplifier),
    with collapsed rings dropped as GEOS drops them. Features where either result is invalid are
    left out, since GEOS rebuilds those with a zero-width buffer; the rest have GEOS's vertices."""
    path = data_dir / name
    if not path.exists():
        pytest.skip(f"{name} is not in the test data")
    ds = ogr.Open(str(path))
    geoms = [f.GetGeometryRef().Clone() for f in ds.GetLayer(0) if f.GetGeometryRef() is not None]
    ds = None
    wkb = [bytes(g.ExportToIsoWkb()) for g in geoms]
    for tolerance in tolerances:
        ours = terrio.simplify_wkb(wkb, tolerance, collapse="drop")
        compared = 0
        for g, out in zip(geoms, ours, strict=True):
            ref = g.Simplify(tolerance)
            a = shapely.from_wkb(out)
            b = shapely.from_wkb(bytes(ref.ExportToIsoWkb()))
            if not (ref.IsValid() and shapely.is_valid(a)):
                continue
            if a.is_empty and b.is_empty:
                compared += 1
                continue
            assert _same_vertices(a, b), (name, tolerance, g.ExportToWkt()[:80])
            compared += 1
        assert compared >= 0.95 * len(geoms), (name, tolerance, compared, len(geoms))

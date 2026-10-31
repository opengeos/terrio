"""`terrio.rio.warp.transform_geom` builds its output as rasterio's.

rasterio builds the transformed geometry with `GeomBuilder` from GDAL's: a new dict of `type` and
`coordinates` (or `geometries`), positions as tuples in lists (a Point's coordinates a tuple),
every position with a height when one position of the geometry has one (GDAL's
`HomogenizeDimensionalityWith()`, 0 where none was given), rings closed (`OGR_G_CloseRings()`);
with a precision, `recursive_round` makes every sequence a list. The structure is compared exactly
(a tuple is not a list) and the values within a tolerance (rtol 1e-12, atol 1e-9); skipped
without rasterio but for the checks that need no reference.
"""

import warnings

import numpy as np
import pytest

from terrio.rio.errors import RasterioDeprecationWarning
from terrio.rio.warp import transform_geom

P = [366000.0, 5807000.0]
Q = [367000.0, 5808000.0]
RING = [[366000.0, 5807000.0], [367000.0, 5807000.0], [367000.0, 5808000.0], [366000.0, 5807000.0]]
HOLE = [[366200.0, 5807200.0], [366400.0, 5807200.0], [366400.0, 5807400.0], [366200.0, 5807200.0]]

GEOMETRIES = {
    "Point": {"type": "Point", "coordinates": P},
    "Point, tuple, with a height": {"type": "Point", "coordinates": (*P, 12.5)},
    "MultiPoint, one height": {"type": "MultiPoint", "coordinates": [P, [*Q, 3.0]]},
    "LineString of tuples": {
        "type": "LineString",
        "coordinates": tuple(tuple(p) for p in RING[:3]),
    },
    "LinearRing, open": {"type": "LinearRing", "coordinates": RING[:3]},
    "MultiLineString": {"type": "MultiLineString", "coordinates": [RING[:3], RING[1:]]},
    "Polygon, open ring and a bbox": {
        "type": "Polygon",
        "coordinates": [RING[:3]],
        "bbox": [0, 0, 1, 1],
    },
    "Polygon with a 3-D hole": {
        "type": "Polygon",
        "coordinates": [RING, [[*p, 4.0] for p in HOLE]],
    },
    "MultiPolygon": {"type": "MultiPolygon", "coordinates": [[RING], [RING[:3], HOLE]]},
    "GeometryCollection, one height": {
        "type": "GeometryCollection",
        "geometries": [
            {"type": "Point", "coordinates": [*P, 2.0]},
            {
                "type": "GeometryCollection",
                "geometries": [{"type": "LineString", "coordinates": [P, Q]}],
            },
        ],
    },
}


def _split(v):
    """A result's container types and keys down to its numbers, and its numbers in order."""
    if isinstance(v, dict):
        items = [(k, _split(x)) for k, x in v.items()]
        return (type(v).__name__, [(k, s) for k, (s, _) in items]), [
            n for _, (_, ns) in items for n in ns
        ]
    if isinstance(v, (list, tuple)):
        items = [_split(x) for x in v]
        return (type(v).__name__, [s for s, _ in items]), [n for _, ns in items for n in ns]
    if isinstance(v, str):
        return v, []
    return type(v).__name__, [v]


@pytest.mark.parametrize("precision", [-1, 3])
@pytest.mark.parametrize("name", sorted(GEOMETRIES))
def test_types_and_values_are_rasterios(name, precision):
    rasterio = pytest.importorskip("rasterio")
    from rasterio.warp import transform_geom as r_transform_geom

    geom = GEOMETRIES[name]
    if precision >= 0 and geom["type"] == "GeometryCollection":
        pytest.skip("rasterio 1.5.0 raises KeyError('coordinates') rounding a collection")
    want = r_transform_geom(
        rasterio.crs.CRS.from_epsg(32633), "EPSG:4326", geom, precision=precision
    )
    got = transform_geom("EPSG:32633", "EPSG:4326", geom, precision=precision)
    (got_structure, got_numbers), (want_structure, want_numbers) = _split(got), _split(want)
    assert got_structure == want_structure, (got, want)
    np.testing.assert_allclose(got_numbers, want_numbers, rtol=1e-12, atol=1e-9)


def test_points_are_tuples_in_lists_without_a_precision():
    """The reproducer: a Polygon's ring points are tuples (rasterio 1.3.11 and 1.5.0)."""
    out = transform_geom("EPSG:32633", "EPSG:4326", {"type": "Polygon", "coordinates": [RING]})
    assert list(out) == ["type", "coordinates"]
    assert type(out["coordinates"]) is list and type(out["coordinates"][0]) is list
    assert [type(p) for p in out["coordinates"][0]] == [tuple] * 4
    assert [len(p) for p in out["coordinates"][0]] == [2] * 4
    point = transform_geom("EPSG:32633", "EPSG:4326", {"type": "Point", "coordinates": [*P, 7]})
    assert type(point["coordinates"]) is tuple and point["coordinates"][2] == 7.0
    multi = transform_geom(
        "EPSG:32633", "EPSG:4326", {"type": "MultiPoint", "coordinates": [P, [*Q, 3]]}
    )
    # The geometry is 3-D once a position is: the other gets a height of 0, as in GDAL.
    assert [p[2] for p in multi["coordinates"]] == [0.0, 3.0]
    ring = transform_geom("EPSG:32633", "EPSG:4326", {"type": "Polygon", "coordinates": [RING[:3]]})
    assert (
        len(ring["coordinates"][0]) == 4 and ring["coordinates"][0][0] == ring["coordinates"][0][3]
    )


def test_a_precision_makes_every_sequence_a_list_and_rounds_heights():
    out = transform_geom(
        "EPSG:32633", "EPSG:4326", {"type": "Point", "coordinates": (*P, 1.23456)}, precision=2
    )
    assert out["coordinates"] == [13.03, 52.4, 1.23] and type(out["coordinates"]) is list
    line = transform_geom(
        "EPSG:32633",
        "EPSG:4326",
        {"type": "LineString", "coordinates": (tuple(P), tuple(Q))},
        precision=3,
    )
    assert type(line["coordinates"]) is list and [type(p) for p in line["coordinates"]] == [
        list,
        list,
    ]


def test_sequences_iterables_and_geo_interface():
    class Shape:
        __geo_interface__ = {"type": "Point", "coordinates": P}

    single = transform_geom("EPSG:32633", "EPSG:4326", Shape())
    assert type(single) is dict and type(single["coordinates"]) is tuple
    several = transform_geom("EPSG:32633", "EPSG:4326", (g for g in [Shape(), GEOMETRIES["Point"]]))
    assert type(several) is list and [type(g["coordinates"]) for g in several] == [tuple, tuple]


def test_invalid_geometries_raise_as_rasterios():
    for bad in (
        {"type": "LineString", "coordinates": []},
        {"type": "GeometryCollection", "geometries": []},
        {"type": "Point"},
    ):
        with pytest.raises(ValueError, match="Input is not a valid geometry object"):
            transform_geom("EPSG:32633", "EPSG:4326", bad)
    feature = {"type": "Feature", "geometry": GEOMETRIES["Point"], "properties": {}}
    with pytest.raises(ValueError, match="Unsupported geometry type Feature"):
        transform_geom("EPSG:32633", "EPSG:4326", feature)


def test_the_antimeridian_options_warn_as_deprecated():
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        transform_geom("EPSG:32633", "EPSG:4326", GEOMETRIES["Point"], antimeridian_cutting=True)
    assert [w.category for w in caught] == [RasterioDeprecationWarning]

"""Zonal bindings retain metadata and expose the shared engine's controls."""

import sqlite3

import numpy as np
import pytest

import terrio


def feature_ids(layer, path):
    path.write_bytes(layer.to_bytes("output.gpkg"))
    with sqlite3.connect(path) as db:
        table = db.execute(
            "SELECT table_name FROM gpkg_contents WHERE data_type='features'"
        ).fetchone()[0]
        quoted = table.replace('"', '""')
        return [row[0] for row in db.execute(f'SELECT fid FROM "{quoted}" ORDER BY fid')]


def inputs():
    raster = terrio.Raster.from_array(
        np.array([[[1, 2], [3, -99]], [[10, 20], [30, -99]]], dtype="float32"),
        (0, 1, 0, 2, 0, -1),
        crs="EPSG:4326",
        nodata=-99,
    )
    zones = terrio.Layer.from_geojson(
        {
            "type": "FeatureCollection",
            "features": [
                {
                    "type": "Feature",
                    "id": 7,
                    "properties": {"zone": "full"},
                    "geometry": {
                        "type": "Polygon",
                        "coordinates": [[[0, 0], [2, 0], [2, 2], [0, 2], [0, 0]]],
                    },
                },
                {"type": "Feature", "id": 8, "properties": {"zone": "empty"}, "geometry": None},
            ],
        },
        crs="EPSG:4326",
    )
    return zones, raster


@pytest.mark.parametrize("kind", ["memory", "lazy", "path"])
def test_zonal_multiple_rasters_bands_nodata_and_empty_zones(tmp_path, kind):
    zones, raster = inputs()
    if kind != "memory":
        path = tmp_path / "raster.tif"
        raster.write(str(path))
        raster = path if kind == "path" else terrio.open_raster(str(path), lazy=True)
    out = terrio.zonal_stats(
        zones,
        [raster, raster],
        bands=[2, 1],
        prefixes=["a", "b"],
        stats=["count", "mean", "stddev", "median"],
        decimal_places=2,
        t_srs="EPSG:3857",
    )
    features = out.to_geojson()["features"]
    a, b = [f["properties"] for f in features]
    assert a["zone"] == "full" and a["a_b2_count"] == 3
    assert a["a_b2_mean"] == 20 and a["a_b2_median"] == 20
    assert a["a_b1_stddev"] == 0.82 and a["b_b2_stddev"] == 8.16
    assert b["a_b1_count"] == 0 and b["a_b1_mean"] is None
    assert feature_ids(out, tmp_path / "statistics.gpkg") == feature_ids(
        zones, tmp_path / "zones.gpkg"
    )
    assert out.crs == "EPSG:3857"
    assert features[0]["geometry"]["coordinates"][0][1][0] > 200000


def test_zonal_pixel_membership_and_defaults():
    _, raster = inputs()
    zone = {
        "type": "Polygon",
        "coordinates": [[[0.9, 0.9], [1.1, 0.9], [1.1, 1.1], [0.9, 1.1], [0.9, 0.9]]],
    }
    # GeoJSON default CRS is EPSG:4326, matching the input raster.
    centers = terrio.zonal_stats(zone, [raster]).to_geojson()["features"][0]["properties"]
    touched = terrio.zonal_stats(zone, [raster], all_touched=True).to_geojson()["features"][0][
        "properties"
    ]
    assert centers["raster1_b1_count"] == 0
    assert touched["raster1_b1_count"] == 3 and touched["raster1_b1_sum"] == 6


@pytest.mark.parametrize(
    "options, message",
    [
        ({"prefixes": []}, "one prefix"),
        ({"prefixes": [" "]}, "cannot be empty"),
        ({"bands": []}, "at least one"),
        ({"bands": [3]}, "band"),
        ({"stats": []}, "at least one"),
        ({"stats": ["mean", "mean"]}, "duplicate"),
        ({"stats": ["bad"]}, "unknown zonal"),
        ({"decimal_places": 16}, "0"),
    ],
)
def test_zonal_invalid_options(options, message):
    zones, raster = inputs()
    with pytest.raises(ValueError, match=message):
        terrio.zonal_stats(zones, [raster], **options)


def test_singlepart_preserves_attributes_nulls_and_xyz(tmp_path):
    source = {
        "type": "FeatureCollection",
        "features": [
            {
                "type": "Feature",
                "properties": {"name": "parts"},
                "geometry": {
                    "type": "GeometryCollection",
                    "geometries": [
                        {"type": "MultiPoint", "coordinates": [[1, 2, 3], [4, 5, 6]]},
                        {"type": "MultiLineString", "coordinates": [[[0, 0, 7], [1, 1, 8]]]},
                    ],
                },
            },
            {"type": "Feature", "properties": {"name": "null"}, "geometry": None},
            {
                "type": "Feature",
                "properties": {"name": "empty"},
                "geometry": {"type": "MultiPoint", "coordinates": []},
            },
        ],
    }
    layer = terrio.singlepart(source)
    result = layer.to_geojson()["features"]
    assert len(result) == 4
    assert feature_ids(layer, tmp_path / "parts.gpkg") == [1, 2, 3, 4]
    assert result[0]["geometry"]["coordinates"] == [1, 2, 3]
    assert result[2]["geometry"]["coordinates"] == [[0, 0, 7], [1, 1, 8]]
    assert result[2]["properties"] == {"name": "parts"}
    assert result[3]["geometry"] is None and result[3]["properties"] == {"name": "null"}

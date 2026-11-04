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
        ({"decimal_places": 16}, "between 0 and 15"),
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
    for kind in ["Point", "LineString"]:
        source["features"].append(
            {
                "type": "Feature",
                "properties": {"name": kind},
                "geometry": {"type": kind, "coordinates": []},
            }
        )
    layer = terrio.singlepart(source)
    result = layer.to_geojson()["features"]
    assert len(result) == 6
    assert feature_ids(layer, tmp_path / "parts.gpkg") == [1, 2, 3, 4, 5, 6]
    assert result[0]["geometry"]["coordinates"] == [1, 2, 3]
    assert result[2]["geometry"]["coordinates"] == [[0, 0, 7], [1, 1, 8]]
    assert result[2]["properties"] == {"name": "parts"}
    assert result[3]["geometry"] is None and result[3]["properties"] == {"name": "null"}

    # GeoJSON represents an empty Point as null; its feature and attributes remain.
    assert result[4]["properties"] == {"name": "Point"}
    assert result[4]["geometry"] is None
    assert result[5]["properties"] == {"name": "LineString"}
    assert result[5]["geometry"] == {"type": "LineString", "coordinates": []}


@pytest.mark.parametrize("kind", ["memory", "lazy", "str", "path"])
def test_zonal_single_raster_matches_sequence(tmp_path, kind):
    zones, raster = inputs()
    if kind != "memory":
        path = tmp_path / "source.tif"
        raster.write(str(path))
        raster = (
            terrio.open_raster(str(path), lazy=True)
            if kind == "lazy"
            else (str(path) if kind == "str" else path)
        )
    one = terrio.zonal_stats(zones, raster).to_geojson()
    many = terrio.zonal_stats(zones, [raster]).to_geojson()
    assert one == many
    assert one["features"][0]["properties"]["raster1_b1_count"] == 3


def test_pathlike_source_name_is_retained_for_vrt(tmp_path):
    _, raster = inputs()
    path = tmp_path / "source.tif"
    raster.write(str(path))
    vrt = tmp_path / "mosaic.vrt"
    assert terrio.mosaic(path, str(vrt)) is None
    np.testing.assert_array_equal(terrio.open_raster(str(vrt)).read(1), raster.read(1))


def test_pathlike_is_resolved_once_for_vrt_metadata_and_reference(tmp_path):
    _, raster = inputs()
    first, second = tmp_path / "first.tif", tmp_path / "second.tif"
    raster.write(str(first))
    terrio.create_raster(str(second), size=(1, 1), burn=[99], crs="EPSG:4326", bbox=(0, 0, 1, 1))

    class ChangingPath:
        calls = 0

        def __fspath__(self):
            self.calls += 1
            return str(first if self.calls == 1 else second)

    source = ChangingPath()
    output = tmp_path / "resolved.vrt"
    assert terrio.mosaic(source, str(output)) is None
    assert source.calls == 1
    np.testing.assert_array_equal(terrio.open_raster(str(output)).read(1), raster.read(1))


def test_invalid_zonal_options_precede_raster_reads():
    zones, _ = inputs()
    for options, message in [
        ({"stats": ["bad"]}, "unknown zonal"),
        ({"prefixes": [""]}, "cannot be empty"),
    ]:
        with pytest.raises(ValueError, match=message):
            terrio.zonal_stats(zones, "not-a-real-raster.tif", **options)


@pytest.mark.parametrize("precision", [-1, 256])
def test_unsigned_decimal_precision_reports_conversion_error(precision):
    zones, raster = inputs()
    with pytest.raises(OverflowError):
        terrio.zonal_stats(zones, raster, decimal_places=precision)


def test_raw_bytes_are_not_a_path_or_raster_input():
    with pytest.raises(ValueError, match="expected a path or a terrio.Raster"):
        terrio.mosaic(b"not-a-raster-upload")


@pytest.mark.parametrize("kind", ["memory", "path", "lazy"])
def test_fractional_zonal_weight_raster(kind, tmp_path):
    """Coverage and a second raster affect only their documented statistics."""
    gt = (0, 1, 0, 1, 0, -1)
    values = terrio.Raster.from_array(np.array([[10.0, 30.0]]), gt, crs="EPSG:4326")
    weights = terrio.Raster.from_array(np.array([[2.0, 4.0]]), gt, crs="EPSG:4326")
    if kind != "memory":
        path = str(tmp_path / "weights.tif")
        weights.write(path)
        weights = path if kind == "path" else terrio.open_raster(path, lazy=True)
    zones = terrio.Layer.from_geojson(
        {"type": "Polygon", "coordinates": [[[0.5, 0], [2, 0], [2, 1], [0.5, 1], [0.5, 0]]]},
        crs="EPSG:4326",
    )
    result = terrio.zonal_stats(
        zones,
        values,
        fractional=True,
        weights=weights,
        stats=["count", "sum", "median", "weighted_mean", "weighted_sum", "weighted_stddev"],
    ).to_geojson()["features"][0]["properties"]
    expected = {
        "count": 1.5,
        "sum": 35.0,
        "median": 20.0,
        "weighted_mean": 26.0,
        "weighted_sum": 130.0,
        "weighted_stddev": 8.0,
    }
    for stat, value in expected.items():
        assert result[f"raster1_b1_{stat}"] == pytest.approx(value)
    with pytest.raises(ValueError, match="mutually exclusive"):
        terrio.zonal_stats(zones, values, fractional=True, all_touched=True)
    with pytest.raises(ValueError, match="require a weighting raster"):
        terrio.zonal_stats(zones, values, stats=["weighted_mean"])


def test_unweighted_statistics_do_not_open_unused_weight_paths(tmp_path):
    zones, values = inputs()
    out = terrio.zonal_stats(zones, values, stats=["count"], weights=str(tmp_path / "missing.tif"))
    assert out.to_geojson()["features"][0]["properties"]["raster1_b1_count"] > 0


def test_zonal_labelled_inputs_dicts_and_dataframe(tmp_path):
    zones, raster = inputs()
    path = tmp_path / "B04.tif"
    raster.write(str(path))
    rows = [
        {"raster": str(path), "time": "2024-07-01", "label": "B04"},
        {"raster": raster, "time": "2024-07-11", "label": "B04", "band": 2},
    ]
    out = terrio.zonal_stats(zones, rows, stats=["mean", "quantile_50", "variety"])
    props = out.to_geojson()["features"][0]["properties"]
    assert props["2024_07_01_B04_mean"] == 2.0
    assert props["2024_07_01_B04_quantile_50"] == 2.0
    assert props["2024_07_11_B04_mean"] == 20.0
    assert props["2024_07_11_B04_variety"] == 3
    with pytest.raises(ValueError, match="prefixes"):
        terrio.zonal_stats(zones, rows, prefixes=["a", "b"])
    with pytest.raises(ValueError, match="raster"):
        terrio.zonal_stats(zones, [{"time": "2024-07-01"}])
    pd = pytest.importorskip("pandas")
    frame = pd.DataFrame(
        {
            "raster": [str(path), str(path)],
            "time": ["2024-07-01", "2024-07-11"],
            "label": ["B04", "B04"],
        }
    )
    props = terrio.zonal_stats(zones, frame, stats=["mean"]).to_geojson()["features"][0][
        "properties"
    ]
    assert props["2024_07_01_B04_mean"] == props["2024_07_11_B04_mean"] == 2.0


def test_zonal_labelled_inputs_missing_values_and_several_bands(tmp_path):
    zones, raster = inputs()
    path = tmp_path / "B04.tif"
    raster.write(str(path))

    def props(rows, **options):
        out = terrio.zonal_stats(zones, rows, stats=["mean"], **options)
        return out.to_geojson()["features"][0]["properties"]

    # A None label is no label; a label over several bands names each band.
    assert props([{"raster": str(path), "label": None}])["raster1_b1_mean"] == 2.0
    several = props([{"raster": str(path), "label": "B04"}], bands=[1, 2])
    assert (several["B04_b1_mean"], several["B04_b2_mean"]) == (2.0, 20.0)
    pd = pytest.importorskip("pandas")
    # Missing DataFrame values (NaN) are absent, and a band column of floats holds band numbers.
    frame = pd.DataFrame(
        {
            "raster": [str(path), str(path)],
            "time": ["2024-07-01", np.nan],
            "label": ["B04", "B08"],
            "band": [2, None],
        }
    )
    assert frame["band"].dtype == "float64"
    got = props(frame)
    assert (got["2024_07_01_B04_mean"], got["B08_mean"]) == (20.0, 2.0)
    assert not any(key.startswith("nan") for key in got)
    # Timestamps name fields by their date; pandas' NA is missing.
    frame = pd.DataFrame(
        {
            "raster": [str(path), str(path)],
            "time": pd.to_datetime(["2024-07-01", "2024-07-11 06:30"], format="ISO8601"),
            "label": pd.array(["B04", pd.NA], dtype="string"),
        }
    )
    got = props(frame)
    assert (got["2024_07_01_B04_mean"], got["2024_07_11T06_30_00_b1_mean"]) == (2.0, 2.0)

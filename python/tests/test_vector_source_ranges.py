"""Public vector ranges across STAC assets and tile sources, referenced to GDAL 3.13.3."""

import json
import subprocess
import sys

import pytest

import terrio  # noqa: E402
import terrio.pyogrio as tpg  # noqa: E402
from terrio import _terrio  # noqa: E402


@pytest.fixture(scope="module")
def sources(tmp_path_factory):
    """Write real multi-layer assets, STAC items and vector tile outputs.

    Args:
        tmp_path_factory: Module-scoped temporary directory factory.

    Returns:
        dict: Dataset paths and their enclosing temporary directory.
    """
    directory = tmp_path_factory.mktemp("vector-ranges")
    layers = []
    for name, count in [("five", 5), ("three", 3)]:
        features = [
            {
                "type": "Feature",
                "properties": {"sequence": i},
                "geometry": {"type": "Point", "coordinates": [10 + i, 46]},
            }
            for i in range(count)
        ]
        layers.append(
            terrio.Layer.from_geojson(
                {"type": "FeatureCollection", "features": features}, name=name, crs="EPSG:4326"
            )
        )
    gpkg = directory / "two.gpkg"
    terrio.write_vector(layers, str(gpkg))
    geojson = directory / "five.geojson"
    layers[0].write(str(geojson))
    result = {"directory": directory, "gpkg": str(gpkg), "geojson": str(geojson)}
    for extension, asset in [("gpkg", gpkg), ("geojson", geojson)]:
        item = directory / f"item-{extension}.json"
        item.write_text(
            json.dumps(
                {
                    "type": "Feature",
                    "stac_version": "1.0.0",
                    "id": extension,
                    "geometry": {"type": "Point", "coordinates": [10, 46]},
                    "bbox": [10, 46, 14, 46],
                    "properties": {"datetime": "2024-01-01T00:00:00Z"},
                    "links": [],
                    "assets": {"data": {"href": asset.name, "roles": ["data"]}},
                }
            )
        )
        result[f"stac-{extension}"] = f"STAC:{item}:data"
    for extension in ["mbtiles", "pmtiles", "mvt"]:
        target = directory / ("tiles" if extension == "mvt" else f"two.{extension}")
        terrio.convert(
            str(gpkg),
            str(target),
            format="MVT" if extension == "mvt" else None,
            options={"MAXZOOM": "1"},
        )
        result[extension] = str(target)
    result["tile"] = str(directory / "tiles/0/0/0.pbf")
    return result


@pytest.mark.parametrize("extension", ["gpkg", "geojson"])
@pytest.mark.parametrize("skip,limit", [(0, 0), (1, 2), (4, None), (99, 1)])
def test_stac_dataframe_ranges_match_direct_assets(sources, extension, skip, limit):
    """STAC forwards per-layer ranges, including the facade's zero-as-unbounded case.

    Args:
        sources: Generated vector fixtures.
        extension: Asset format.
        skip: Number of live features to skip.
        limit: Maximum live features, or no maximum.
    """
    pytest.importorskip("geopandas")
    options = {"skip_features": skip, "max_features": limit, "layer": 0}
    direct = tpg.read_dataframe(sources[extension], **options)
    through_item = tpg.read_dataframe(sources[f"stac-{extension}"], **options)
    assert through_item["sequence"].tolist() == direct["sequence"].tolist()
    assert through_item.crs == direct.crs
    if limit == 0:
        # The compatibility facade treats zero as unbounded; native zero means empty.
        native = _terrio.pyogrio_read(sources[f"stac-{extension}"], skip, 0, True)
        assert len(native["fids"]) == 0


@pytest.mark.parametrize("source", ["mbtiles", "pmtiles", "mvt", "tile"])
@pytest.mark.parametrize("skip,limit", [(0, 0), (1, 1), (99, 1)])
def test_native_tile_ranges_are_bounded(sources, source, skip, limit):
    """Native Python reads retain zero or one feature from each tile source.

    Args:
        sources: Generated vector fixtures.
        source: Archive, directory or single tile key.
        skip: Number of live features to skip.
        limit: Maximum number of features.
    """
    full = _terrio.pyogrio_read(sources[source], 0, None, True)
    result = _terrio.pyogrio_read(sources[source], skip, limit, True)
    assert len(result["fids"]) == len(full["fids"][skip : skip + limit])


@pytest.mark.parametrize("source", ["mbtiles", "pmtiles", "tile"])
@pytest.mark.parametrize("skip,limit", [(0, 0), (1, 1), (99, 1)])
def test_tile_dataframe_ranges_match_native_reads(sources, source, skip, limit):
    """Check the optional dataframe facade against native tile ranges.

    Args:
        sources: Generated vector fixtures.
        source: Archive or single tile key.
        skip: Number of live features to skip.
        limit: Facade maximum, where zero means unbounded.
    """
    pytest.importorskip("geopandas")
    frame = tpg.read_dataframe(sources[source], skip_features=skip, max_features=limit, layer=0)
    # The facade uses zero for unbounded reads; the native API uses None.
    expected = _terrio.pyogrio_read(sources[source], skip, None if limit == 0 else limit, True)
    assert len(frame) == len(expected["fids"])


def test_stac_cli_layer_selection_matches_the_asset(sources):
    """The portable CLI retains only the selected STAC asset layer.

    Args:
        sources: Generated vector fixtures.
    """
    output = sources["directory"] / "selected.gpkg"
    subprocess.run(
        [
            sys.executable,
            "-m",
            "terrio",
            "convert",
            sources["stac-gpkg"],
            str(output),
            "-l",
            "three",
        ],
        check=True,
        capture_output=True,
        text=True,
        timeout=60,
    )
    assert [(layer.name, len(layer)) for layer in terrio.open_vector(str(output))] == [("three", 3)]

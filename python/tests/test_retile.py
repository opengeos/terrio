"""Retile layout and publication entry points retain lazy/native raster behavior."""

import json

import numpy as np
import pytest

import terrio


@pytest.mark.parametrize("kind", ["loaded", "lazy", "path"])
def test_retile_layout_and_resume(tmp_path, kind):
    values = np.arange(35, dtype="uint16").reshape(1, 5, 7)
    source = terrio.Raster.from_array(values, (10, 2, 0, 50, 0, -2), crs="EPSG:4326")
    if kind != "loaded":
        path = tmp_path / "source.tif"
        source.write(str(path))
        source = path if kind == "path" else terrio.open_raster(str(path), lazy=True)
    output = tmp_path / "tiles"
    options = dict(tile_size=[3, 3], levels=1, prefix="sample", csv="tiles.csv")
    plan = terrio.retile(source, output, dry_run=True, **options)
    assert plan["planned"] == 7
    assert not output.exists()
    report = terrio.retile(source, output, **options)
    assert report["written"] == 7
    first = terrio.open_raster(str(output / "sample_1_1.tif"))
    np.testing.assert_array_equal(first.read(1), values[0, :3, :3])
    pyramid = terrio.open_raster(str(output / "1/sample_1_1.tif"))
    np.testing.assert_array_equal(pyramid.read(1), [[8, 10, 12], [22, 24, 26]])
    assert len((output / "tiles.csv").read_text().splitlines()) == 6
    report = terrio.retile(source, output, resume=True, **options)
    assert report["resumed"] == 7 and report["written"] == 0


def test_publication_stacta_and_custom_template(tmp_path):
    source = terrio.Raster.from_array(
        np.full((1, 64, 128), 100, dtype="uint8"),
        (-180, 2.8125, 0, 90, 0, -2.8125),
        crs="EPSG:4326",
    )
    output = tmp_path / "published"
    terrio.raster_tiles(
        source,
        output,
        tiling_scheme="WorldCRS84Quad",
        grid={"tile_size": 64, "min_zoom": 0, "max_zoom": 0},
        resampling="nearest",
        publication={
            "viewers": ["all"],
            "kml": True,
            "title": "${URL} & map",
            "mapml_template": "<title>${TITLE}</title>",
        },
    )
    assert (output / "mapml.mapml").read_text() == "<title>${URL} &amp; map</title>"
    assert (output / "doc.kml").exists()
    document = json.loads((output / "stacta.json").read_text())
    assert len(document["asset_templates"]["bands"]["bands"]) == 2
    published = terrio.open_raster(str(output / "stacta.json"), lazy=True)
    np.testing.assert_array_equal(published.read(1), np.full((64, 128), 100, dtype="uint8"))

"""Public PROJJSON parsing against pinned independent GDAL coordinates."""

import json
from pathlib import Path

import numpy as np
import pytest

import terrio
from terrio.rio.crs import CRS

REFERENCE = json.loads(
    (
        Path(__file__).resolve().parents[2] / "crates/terrio-crs/tests/data/projjson_reference.json"
    ).read_text()
)


@pytest.mark.parametrize("case", REFERENCE["cases"], ids=lambda c: c["name"])
def test_projjson_coordinate_oracles(case):
    for definition in (case["src"], case["dst"]):
        crs = CRS.from_string(json.dumps(definition))
        assert crs.to_epsg() is None
        assert crs.to_authority() is None
    points = np.array(case["points"])
    terrio.set_grid_search_paths([])
    terrio.set_network_enabled(False)
    try:
        result = np.column_stack(
            terrio.transform_points(
                points[:, 0], points[:, 1], json.dumps(case["src"]), json.dumps(case["dst"])
            )
        )
        assert np.all(np.abs(result - case["expected"]) <= case["tolerance"])
    finally:
        terrio.set_grid_search_paths(None)
        terrio.set_network_enabled(None)


def test_unsupported_projjson_is_an_error():
    with pytest.raises(terrio.CRSError, match="PROJJSON"):
        terrio.transform_points(
            [0], [0], "EPSG:4326", '{"type":"VerticalCRS","name":"unsupported"}'
        )

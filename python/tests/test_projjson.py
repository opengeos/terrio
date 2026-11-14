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

IDENTITIES = {
    "British National Grid": (4326, 27700),
    "Web Mercator": (4326, 3857),
    "North-first projected axes": (3044, 4258),
    "West and south projected axes": (2046, 4148),
    "US survey feet": (2227, 4269),
    "Paris prime meridian and grads": (4807, 4275),
    "Bound zero translation": (None, 4326),
    "Bound geographic translation": (None, 4326),
    "Bound projected seven parameters": (None, 4326),
}


@pytest.mark.parametrize("case", REFERENCE["cases"], ids=lambda c: c["name"])
def test_projjson_coordinate_oracles(case):
    name = case["name"].removesuffix(" with IDs").removesuffix(" without IDs")
    identities = IDENTITIES[name]
    for definition, code in zip((case["src"], case["dst"]), identities, strict=True):
        crs = CRS.from_string(json.dumps(definition))
        assert crs.to_epsg() == code
        assert crs.to_authority() == (None if code is None else ("EPSG", str(code)))
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


@pytest.mark.parametrize("code", [4326, 32633, 2154])
@pytest.mark.parametrize("identifier", [None, 3857])
def test_projjson_reports_identity_from_definition(code, identifier):
    """Identify a round trip even when the root ID is absent or wrong."""
    original = CRS.from_epsg(code)
    document = json.loads(original.to_projjson())
    document.pop("id", None)
    if identifier is not None:
        document["id"] = {"authority": "EPSG", "code": identifier}
    read = CRS.from_user_input(json.dumps(document))
    assert read.to_epsg() == code
    assert read.to_authority() == ("EPSG", str(code))
    assert read.to_string() == f"EPSG:{code}"
    assert read == original


def test_projected_base_accepts_omitted_type():
    """Read PROJJSON's unambiguous geographic base without an explicit type."""
    original = CRS.from_epsg(32633)
    document = json.loads(original.to_projjson())
    del document["base_crs"]["type"]
    assert CRS.from_user_input(json.dumps(document)) == original

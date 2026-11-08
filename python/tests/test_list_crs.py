"""`terrio.list_crs`: the registry CRSs terrio resolves, for searchable CRS pickers.

The catalog is checked against GDAL's `osr.GetCRSInfoListFromDatabase` by
`scripts/compare_crs_list_with_gdal.py`; these tests check the binding's arguments and that
each listed CRS resolves.
"""

import pytest

import terrio
from terrio.errors import InvalidArgumentError


def test_finds_australian_albers_by_name_and_code():
    for text in ("Albers", "3577", "EPSG:3577", "gda94 ALBERS"):
        found = [e for e in terrio.list_crs(text) if e["authority"] == "EPSG" and e["code"] == 3577]
        assert len(found) == 1, text
    entry = terrio.list_crs("EPSG:3577")[0]
    assert entry == {
        "authority": "EPSG",
        "code": 3577,
        "name": "GDA94 / Australian Albers",
        "kind": "projected",
        "deprecated": False,
        "area_name": entry["area_name"],
        "bounds": [112.85, -43.7, 153.69, -9.86],
        "projection_method": "Albers Equal Area",
        "celestial_body": "Earth",
    }
    assert entry["area_name"].startswith("Australia - Australian Capital Territory")


def test_filters_by_authority_kind_and_deprecation():
    epsg = terrio.list_crs(authority="epsg", kind=["geographic_2d", "projected"])
    assert {e["authority"] for e in epsg} == {"EPSG"}
    assert {e["kind"] for e in epsg} == {"geographic_2d", "projected"}
    assert not any(e["deprecated"] for e in epsg)
    assert terrio.list_crs("EPSG:3785") == []
    deprecated = terrio.list_crs("EPSG:3785", include_deprecated=True)
    assert [(e["code"], e["deprecated"]) for e in deprecated] == [(3785, True)]
    esri = terrio.list_crs("contiguous albers", authority=["ESRI"])
    assert 102003 in {e["code"] for e in esri}
    mars = terrio.list_crs("49902", authority="IAU_2015")
    assert [(e["kind"], e["celestial_body"]) for e in mars] == [("other", "Mars")]
    both = terrio.list_crs("albers", authority=["epsg", "ESRI"])
    assert {e["authority"] for e in both} == {"EPSG", "ESRI"}
    with pytest.raises(InvalidArgumentError, match="unknown CRS authority"):
        terrio.list_crs(authority="OGC")
    with pytest.raises(InvalidArgumentError, match="unknown CRS kind"):
        terrio.list_crs(kind="geographic")


def test_listed_crs_resolve():
    entries = terrio.list_crs(include_deprecated=True)
    assert len(entries) > 12000
    # In order of authority, then code.
    order = {"EPSG": 0, "ESRI": 1, "IAU_2015": 2}
    keys = [(order[e["authority"]], e["code"]) for e in entries]
    assert keys == sorted(keys)
    for entry in entries[::211]:
        info = terrio._terrio.crs_info(f"{entry['authority']}:{entry['code']}")
        assert info["name"] is not None, entry

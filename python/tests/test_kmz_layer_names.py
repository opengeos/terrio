"""KMZ layers have unique selection names across every linked document."""

import io
import sqlite3
import subprocess
import sys
import zipfile

import pytest

import terrio


def document(body):
    return f'<kml xmlns="http://www.opengis.net/kml/2.2"><Document>{body}</Document></kml>'


def point(x):
    return (
        f"<Placemark><name>point {x}</name><Point>"
        f"<coordinates>{x},46</coordinates></Point></Placemark>"
    )


def folder(name, x):
    return f"<Folder><name>{name}</name>{point(x)}</Folder>"


def link(name, member):
    return f"<NetworkLink><name>{name}</name><Link><href>{member}</href></Link></NetworkLink>"


CASES = [
    (
        {
            "doc.kml": document(folder("Roads", 1) + link("roads", "layers/a.kml")),
            "layers/a.kml": document(point(2)),
        },
        ["Roads", "roads (2)"],
        [1, 2],
    ),
    (
        {
            "doc.kml": document(
                folder("Roads (2)", 1) + folder("Roads", 2) + link("roads", "layers/a.kml")
            ),
            "layers/a.kml": document(point(3) + folder("ROADS", 4)),
        },
        ["Roads (2)", "Roads", "roads (3)", "ROADS (4)"],
        [1, 2, 3, 4],
    ),
    (
        {
            "doc.kml": document(folder("roads", 1) + link("roads", "layers/a.kml")),
            "layers/a.kml": document(point(2)),
        },
        ["roads", "roads (2)"],
        [1, 2],
    ),
    (
        {
            "doc.kml": document(link("roads", "layers/a.kml") + link("roads", "layers/b.kml")),
            "layers/a.kml": document(point(1)),
            "layers/b.kml": document(point(2)),
        },
        ["roads", "roads (2)"],
        [1, 2],
    ),
    (
        {
            "doc.kml": document(
                folder("roads (2)", 1) + folder("roads", 2) + link("roads", "layers/a.kml")
            ),
            "layers/a.kml": document(point(3) + folder("roads", 4)),
        },
        ["roads (2)", "roads", "roads (3)", "roads (4)"],
        [1, 2, 3, 4],
    ),
    (
        {
            "doc.kml": document("<name>roads</name>" + point(1) + link("roads", "layers/a.kml")),
            "layers/a.kml": document(point(2)),
        },
        ["roads", "roads (2)"],
        [1, 2],
    ),
    (
        {
            "doc.kml": document(link("roads", "layers/b.kml") + link("roads", "layers/a.kml")),
            "layers/a.kml": document(point(1)),
            "layers/b.kml": document(point(2)),
        },
        ["roads", "roads (2)"],
        [2, 1],
    ),
]


def archive(members):
    buffer = io.BytesIO()
    with zipfile.ZipFile(buffer, "w", compression=zipfile.ZIP_DEFLATED) as output:
        for name, text in members.items():
            output.writestr(name, text)
    return buffer.getvalue()


@pytest.mark.parametrize("members,names,coordinates", CASES)
@pytest.mark.parametrize("memory", [False, True])
def test_kmz_selection_and_geopackage_conversion_keep_every_layer(
    tmp_path, members, names, coordinates, memory
):
    data = archive(members)
    source = tmp_path / "source.kmz"
    source.write_bytes(data)
    dataset = (
        terrio.open_bytes(data, filename="source.kmz")
        if memory
        else terrio.open_vector(str(source))
    )
    assert dataset.layer_names == names
    for name, x in zip(names, coordinates, strict=True):
        feature = dataset[name].to_geojson()["features"][0]
        assert feature["geometry"]["coordinates"] == [x, 46]
        assert feature["properties"]["Name"] == f"point {x}"
    output = tmp_path / "output.gpkg"
    dataset.write(str(output))
    with sqlite3.connect(output) as connection:
        tables = [row[0] for row in connection.execute("SELECT table_name FROM gpkg_contents")]
    assert sorted(tables) == sorted(names)
    restored = terrio.open_vector(str(output))
    for name, x in zip(names, coordinates, strict=True):
        assert restored[name].to_geojson()["features"][0]["geometry"]["coordinates"] == [x, 46]


@pytest.mark.parametrize("members,names,coordinates", CASES)
def test_cli_converts_duplicate_document_names_to_distinct_tables(
    tmp_path, members, names, coordinates
):
    source = tmp_path / "source.kmz"
    source.write_bytes(archive(members))
    output = tmp_path / "output.gpkg"
    done = subprocess.run(
        [sys.executable, "-m", "terrio", "convert", str(source), str(output)],
        capture_output=True,
        text=True,
    )
    assert done.returncode == 0, done.stderr
    restored = terrio.open_vector(str(output))
    assert sorted(restored.layer_names) == sorted(names)
    for name, x in zip(names, coordinates, strict=True):
        assert restored[name].to_geojson()["features"][0]["geometry"]["coordinates"] == [x, 46]


@pytest.mark.parametrize("members,names,coordinates", CASES)
def test_kmz_preserves_gdal_per_document_xy_and_names(tmp_path, members, names, coordinates):
    """GDAL 3.13.3 KML controls; the combined dataset's selection names are Terrio policy."""
    gdal = pytest.importorskip("osgeo.gdal")
    if gdal.GetDriverByName("KML") is None:
        pytest.skip("GDAL KML driver is unavailable")
    gdal.UseExceptions()
    expected = []
    for index, text in enumerate(members.values()):
        path = tmp_path / f"document_{index}.kml"
        path.write_text(text)
        source = gdal.OpenEx(str(path), gdal.OF_VECTOR, allowed_drivers=["KML"])
        for layer in source:
            for feature in layer:
                geometry = feature.GetGeometryRef()
                expected.append((geometry.GetX(), geometry.GetY(), feature.GetField("Name")))
    dataset = terrio.open_bytes(archive(members), filename="source.kmz")
    actual = []
    for name in dataset.layer_names:
        for feature in dataset[name].to_geojson()["features"]:
            x, y = feature["geometry"]["coordinates"]
            actual.append((x, y, feature["properties"]["Name"]))
    assert sorted(actual) == sorted(expected)

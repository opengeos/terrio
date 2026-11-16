"""MIF reads require a header while retaining legitimate empty layers."""

import io
import zipfile

import pytest

import terrio


@pytest.mark.parametrize("surface", ["bytes", "path", "archive"])
@pytest.mark.parametrize(
    "data",
    [
        b"",
        b"hello\n",
        b'{"type":"FeatureCollection","features":[{"type":"Feature","properties":{},'
        b'"geometry":{"type":"Point","coordinates":[10,46]}}]}',
        b"name,x,y\na,10,46\n",
        b"<kml></kml>",
        b"<FeatureCollection/>",
        b"Data\n",
        b"Version 300\nData\n",
        b"Columns 0\nData\n",
        b"Version 300\nColumns 0\n",
        b"Version 300\nColumns 1\n",
        b"Version 300\nColumns invalid\nData\n",
        b"Version 300\nColumns 2\nN Integer\nData\n",
    ],
)
def test_mif_rejects_missing_or_incomplete_header(tmp_path, data, surface):
    """Refuse wrong-format content and incomplete MIF documents.

    Args:
        tmp_path: Directory for filesystem input.
        data: Wrong-format bytes or an incomplete MIF header.
        surface: The standalone byte, filesystem or ZIP-member reader.
    """
    with pytest.raises(terrio.InvalidDataError, match="MIF:"):
        _read(tmp_path, data, surface)


def _read(tmp_path, data, surface):
    """Open the same MIF payload through a public input surface.

    Args:
        tmp_path: Directory for filesystem input.
        data: MIF payload.
        surface: Byte, path or archive input selector.

    Returns:
        Dataset: The opened vector dataset.
    """
    if surface == "path":
        path = tmp_path / "x.mif"
        path.write_bytes(data)
        return terrio.open_vector(str(path))
    if surface == "archive":
        buffer = io.BytesIO()
        with zipfile.ZipFile(buffer, "w") as archive:
            archive.writestr("x.mif", data)
            archive.writestr("x.mid", b"")
        return terrio.open_bytes(buffer.getvalue(), "x.mif.zip")
    return terrio.open_bytes(data, "x.mif")


@pytest.mark.parametrize("surface", ["bytes", "path", "archive"])
@pytest.mark.parametrize("columns", [b"0", b"1\n N Integer"])
@pytest.mark.parametrize("mixed_case", [False, True])
def test_mif_header_without_objects_retains_schema(tmp_path, surface, columns, mixed_case):
    """Preserve zero-column and declared-column empty layers.

    Args:
        tmp_path: Directory for filesystem input.
        surface: Byte, path or archive input selector.
        columns: The column declaration and optional field.
        mixed_case: Whether to use mixed-case keywords, CRLF and a UTF-8 BOM.
    """
    data = b'Version 300\nCharset "UTF-8"\nDelimiter ","\nColumns ' + columns + b"\nData\n"
    if mixed_case:
        data = b"\xef\xbb\xbf" + data.replace(b"Version", b"vErSiOn").replace(
            b"Columns", b"cOlUmNs"
        ).replace(b"Data", b"dAtA").replace(b"\n", b"\r\n")
    dataset = _read(tmp_path, data, surface)
    assert len(dataset) == 1
    assert len(dataset[0]) == 0
    assert dataset[0].fields == ({"N": "Integer"} if columns.startswith(b"1") else {})

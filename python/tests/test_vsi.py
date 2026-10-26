"""File management with terrio.vsi_* (GDAL's ``gdal vsi``)."""

import os
import zipfile

import pytest

import terrio
from terrio.errors import InvalidArgumentError, NotFoundError, TerrioIOError


@pytest.fixture
def tree(tmp_path):
    """Creates ``t/x.txt``, ``t/a/y.txt`` and ``t/a/b/z.txt``.

    Args:
        tmp_path: The pytest temporary directory.

    Returns:
        pathlib.Path: The ``t`` directory.
    """
    t = tmp_path / "t"
    (t / "a" / "b").mkdir(parents=True)
    (t / "x.txt").write_text("hello\n")
    (t / "a" / "y.txt").write_text("abc\n")
    (t / "a" / "b" / "z.txt").write_text("z\n")
    return t


def test_list(tree):
    names = [e["name"] for e in terrio.vsi_list(str(tree), recursive=True)]
    assert names == ["a", "a/b", "a/b/z.txt", "a/y.txt", "x.txt"]
    x = terrio.vsi_list(str(tree))[1]
    assert (x["type"], x["size"]) == ("file", 6)
    assert x["last_modification_date"].endswith("Z")
    assert [e["name"] for e in terrio.vsi_list(str(tree), recursive=True, depth=2)] == [
        "a",
        "a/b",
        "a/y.txt",
        "x.txt",
    ]
    assert terrio.vsi_list(str(tree), absolute=True)[0]["name"] == os.path.join(
        os.path.abspath(tree), "a"
    )
    assert terrio.vsi_stat(str(tree / "missing")) is None
    with pytest.raises(NotFoundError):
        terrio.vsi_list(str(tree / "missing"))


def test_copy_sync_move_delete(tree, tmp_path):
    out = tmp_path / "out"
    with pytest.raises(InvalidArgumentError):
        terrio.vsi_copy(str(tree), str(out))
    r = terrio.vsi_copy(str(tree), str(out), recursive=True)
    assert (r["copied"], r["bytes"]) == (3, 12)
    assert (out / "a" / "b" / "z.txt").read_text() == "z\n"
    s = tmp_path / "s"
    assert terrio.vsi_sync(str(tree) + "/", str(s), recursive=True)["copied"] == 3
    assert terrio.vsi_sync(str(tree) + "/", str(s), recursive=True)["unchanged"] == 3
    assert (
        terrio.vsi_sync(str(tree) + "/", str(s), recursive=True, strategy="overwrite")["copied"]
        == 3
    )
    terrio.vsi_move(str(out), str(tmp_path / "moved"))
    assert not out.exists() and (tmp_path / "moved" / "x.txt").exists()
    keep = tmp_path / "moved2"
    keep.mkdir()
    (keep / "k").write_text("k")
    assert terrio.vsi_delete(str(tmp_path / "moved"), recursive=True)["deleted"] == 6
    assert (keep / "k").exists()
    for target in ["", "/", ".", "s3://bucket"]:
        with pytest.raises(InvalidArgumentError):
            terrio.vsi_delete(target, recursive=True)


def test_memory_and_archives(tree, tmp_path):
    for root in ["/vsimem", "/vsimem/"]:
        with pytest.raises(InvalidArgumentError, match="root"):
            terrio.vsi_write(root, b"not a directory")
        assert terrio.vsi_stat(root)["type"] == "directory"
    terrio.vsi_write("/vsimem/pytest_vsi/a.txt", b"bytes")
    assert terrio.vsi_read("/vsimem/pytest_vsi/a.txt") == b"bytes"
    assert [e["name"] for e in terrio.vsi_list("/vsimem/pytest_vsi")] == ["a.txt"]
    terrio.vsi_delete("/vsimem/pytest_vsi", recursive=True)
    assert terrio.vsi_stat("/vsimem/pytest_vsi") is None

    archive = tmp_path / "t.zip"
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as z:
        for p in sorted(tree.rglob("*")):
            z.write(p, p.relative_to(tree).as_posix())
    root = f"/vsizip/{archive}"
    assert [e["name"] for e in terrio.vsi_list(root, recursive=True)] == [
        "a",
        "a/b",
        "a/b/z.txt",
        "a/y.txt",
        "x.txt",
    ]
    assert terrio.vsi_read(f"{root}/a/y.txt") == b"abc\n"
    terrio.vsi_copy(root, str(tmp_path / "unzipped"), recursive=True)
    assert (tmp_path / "unzipped" / "a" / "b" / "z.txt").read_text() == "z\n"


def test_matches_gdal_listing(tree, tmp_path):
    gdal = pytest.importorskip("osgeo.gdal")
    archive = tmp_path / "t.zip"
    with zipfile.ZipFile(archive, "w") as z:
        for p in sorted(tree.rglob("*")):
            z.write(p, p.relative_to(tree).as_posix())
    for path in [str(tree), f"/vsizip/{archive}"]:
        ours = {
            (e["name"], e["type"] == "directory", e["size"] if e["type"] == "file" else None)
            for e in terrio.vsi_list(path, recursive=True)
        }
        theirs = set()
        for name in gdal.ReadDirRecursive(path):
            name = name.rstrip("/")
            st = gdal.VSIStatL(f"{path}/{name}")
            is_dir = st.IsDirectory()
            theirs.add((name, is_dir, None if is_dir else st.size))
        assert ours == theirs, path


def test_recursive_copy_preserves_io_exception(tree, tmp_path):
    destination = tmp_path / "out"
    (destination / "t" / "x.txt").mkdir(parents=True)
    with pytest.raises(TerrioIOError):
        terrio.vsi_copy(str(tree), str(destination), recursive=True)
    assert (destination / "t" / "x.txt").is_dir()
    assert (tree / "x.txt").read_text() == "hello\n"

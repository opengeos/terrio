"""The wheel tools: check_wheel_paths.py, remap_wheel_sbom.py and check_wheel_glibc.py (scripts/).

scripts/build_wheel_manylinux_2_28.sh maps the build machine's paths out of the wheel, then checks
it. Here the scripts run on small wheels made by the tests, holding known paths. The paths found
are counted by their label in the output, since the temporary directories may themselves lie in a
home directory. The glibc check runs on wheels holding ELF objects made by the tests, laid out
as a linker lays out a shared object, some altered to hide what they need from section headers.
"""

import base64
import hashlib
import os
import re
import struct
import subprocess
import sys
import zipfile
import zlib
from pathlib import Path
from urllib.parse import quote

import pytest

if sys.platform == "win32":
    pytest.skip("the wheel tools check Linux wheels", allow_module_level=True)

ROOT = Path(__file__).resolve().parents[2]
CHECK = ROOT / "scripts" / "check_wheel_paths.py"
REMAP = ROOT / "scripts" / "remap_wheel_sbom.py"
GLIBC = ROOT / "scripts" / "check_wheel_glibc.py"
USER = "builder"
DIST = "demo-1.0.dist-info"
SBOM = f"{DIST}/sboms/demo.cyclonedx.json"


def make_wheel(path: Path, members: dict[str, bytes]) -> Path:
    """A wheel holding `members`, and a RECORD listing them as maturin writes it."""
    rows = []
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as archive:
        for name, data in members.items():
            archive.writestr(name, data)
            digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
            rows.append(f"{name},sha256={digest.decode()},{len(data)}\n")
        archive.writestr(f"{DIST}/RECORD", "".join(rows) + f"{DIST}/RECORD,,\n")
    return path


def check(wheel: Path, *forbid: Path | str, home: str = os.sep) -> subprocess.CompletedProcess[str]:
    """scripts/check_wheel_paths.py on `wheel`, with HOME set to `home` (the root: no HOME rule)."""
    args = [sys.executable, str(CHECK), str(wheel)]
    for path in forbid:
        args += ["--forbid", str(path)]
    env = {**os.environ, "HOME": home}
    return subprocess.run(args, capture_output=True, text=True, check=False, env=env)


def found(result: subprocess.CompletedProcess[str]) -> int:
    match = re.search(r"^paths found: (\d+)$", result.stdout, re.MULTILINE)
    assert match, result.stdout + result.stderr
    return int(match.group(1))


def counted(result: subprocess.CompletedProcess[str], member: str, label: str) -> int:
    """The paths of one label the output counts in one member (0 when it lists none)."""
    line = next((s for s in result.stdout.splitlines() if s.startswith(f"{member}: ")), "")
    match = re.search(rf"(?:\(|, ){re.escape(label)}: (\d+)[,)]", "(" + line.partition(" (")[2])
    return int(match.group(1)) if match else 0


def elf(section: str, payload: bytes, method: int = 1) -> bytes:
    """A 64-bit little-endian ELF object with one section stored compressed (`method` 1: zlib)."""
    names = b"\0.shstrtab\0" + section.encode() + b"\0"
    body = struct.pack("<IIQQ", method, 0, len(payload), 1) + zlib.compress(payload)
    names_at, body_at = 64, 64 + len(names)
    table_at = body_at + len(body)
    header = b"\x7fELF" + bytes([2, 1, 1]) + bytes(9)
    header += struct.pack("<HHIQQQIHHHHHH", 3, 62, 1, 0, 0, table_at, 0, 64, 0, 0, 64, 3, 1)

    def entry(name: int, kind: int, flags: int, offset: int, size: int) -> bytes:
        return struct.pack("<IIQQQQIIQQ", name, kind, flags, 0, offset, size, 0, 0, 1, 0)

    table = bytes(64) + entry(1, 3, 0, names_at, len(names))
    table += entry(11, 1, 0x800, body_at, len(body))
    return header + names + body + table


def test_home_directories_are_found_where_string_literals_touch(tmp_path: Path) -> None:
    """Rust packs its string literals without separators: each path counts, wherever it starts."""
    paths = [
        f"/home/{USER}/.cargo/registry/src/index/a-1.0/src/lib.rs",
        "/root/.rustup/toolchains/stable/lib/rustlib/src/rust/library/core/src/b.rs",
        f"/Users/{USER}/src/c.rs",
        f"C:\\Users\\{USER}\\src\\d.rs",
        f"C:\\\\Users\\\\{USER}\\\\src\\\\e.rs",
        f"d:/users/{USER}/f.rs",
    ]
    wheel = make_wheel(tmp_path / "w.whl", {"demo/_x.so": "event".join(paths).encode()})
    result = check(wheel)
    assert result.returncode == 1 and found(result) == 6
    assert counted(result, "demo/_x.so", "/home/<user>/") == 1
    assert counted(result, "demo/_x.so", "/root/") == 1
    assert counted(result, "demo/_x.so", "/Users/<user>/") == 1
    assert counted(result, "demo/_x.so", "C:\\Users\\") == 3
    assert "FAIL: 6 absolute paths of the build machine in 1 of 2 files" in result.stdout


def test_neutral_paths_pass(tmp_path: Path) -> None:
    text = (
        "/cargo/registry/src/index/a-1.0/src/lib.rs /rustup/toolchains/stable/lib/x.rs "
        "/src/crates/a/src/lib.rs /target/release/build/a-1/out/b.rs /rustc/0abc/library/c.rs "
        "/homes/a/ /rooted/ /Users/ crates/a/src/lib.rs path+file:///src/crates/a#0.1.0"
    )
    wheel = make_wheel(tmp_path / "w.whl", {"demo/_x.so": text.encode(), SBOM: text.encode()})
    result = check(wheel)
    assert result.returncode == 0 and found(result) == 0, result.stdout
    assert "OK: no absolute path of the build machine in 3 files" in result.stdout


def test_overlapping_matches_are_one_path(tmp_path: Path) -> None:
    """`C:/Users/x/` matches the Windows rule and, from its slash, `/Users/<user>/`: one path."""
    wheel = make_wheel(tmp_path / "w.whl", {"demo/a.py": f"'C:/Users/{USER}/a.py'".encode()})
    result = check(wheel)
    assert found(result) == 1 and counted(result, "demo/a.py", "C:\\Users\\") == 1


def test_forbidden_directories_as_given_resolved_and_in_file_urls(tmp_path: Path) -> None:
    """A --forbid directory given through a link is found by its real path, in a file URL too.

    Not in a longer name, though: `repo` is not in `repository`.
    """
    checkout = tmp_path / "work dir" / "repo"
    checkout.mkdir(parents=True)
    (tmp_path / "link").symlink_to(tmp_path / "work dir")
    given = tmp_path / "link" / "repo"
    real = str(checkout)
    text = f"{real}/crates/a/src/lib.rs|path+file://{quote(real)}/crates/a#0.1.0|{real}sitory/b"
    wheel = make_wheel(tmp_path / "w.whl", {"demo/_x.so": text.encode()})
    result = check(wheel, given)
    assert result.returncode == 1
    assert counted(result, "demo/_x.so", f"--forbid {given}") == 2, result.stdout


def test_home_from_the_environment(tmp_path: Path) -> None:
    home = tmp_path / "a home"
    text = f"{home}/.cargo/registry/a.rs {home}2/b.rs"
    wheel = make_wheel(tmp_path / "w.whl", {"demo/_x.so": text.encode()})
    result = check(wheel, home=str(home))
    assert result.returncode == 1
    assert counted(result, "demo/_x.so", f"HOME {home}") == 1, result.stdout
    assert counted(check(wheel), "demo/_x.so", f"HOME {home}") == 0


def test_compressed_elf_sections_are_read(tmp_path: Path) -> None:
    payload = b"\0/root/.cargo/registry/src/index/a-1.0/src/lib.rs\0"
    wheel = make_wheel(tmp_path / "w.whl", {"demo/_x.so": elf(".debug_str", payload)})
    result = check(wheel)
    assert result.returncode == 1 and found(result) == 1
    assert counted(result, "demo/_x.so [.debug_str]", "/root/") == 1


def test_a_section_compressed_otherwise_than_with_zlib_fails(tmp_path: Path) -> None:
    wheel = make_wheel(tmp_path / "w.whl", {"demo/_x.so": elf(".debug_str", b"x", method=2)})
    result = check(wheel)
    assert result.returncode == 1 and found(result) == 0
    assert "FAIL: demo/_x.so: cannot read its compressed sections" in result.stdout


def test_the_sbom_is_remapped_and_the_record_updated(tmp_path: Path) -> None:
    checkout = tmp_path / "my checkout"
    url = f"path+file://{quote(str(checkout))}"
    sbom = (
        f'{{"bom-ref": "{url}/python#demo@1.0", "ref": "{url}/crates/a#0.1.0", '
        f'"dependsOn": ["{url}#root@0.1.0", "path+file://{checkout}/crates/b#0.1.0", '
        f'"{url}-other/c#0.1.0"]}}'
    ).encode()
    module = b"print('unchanged')\n"
    wheel = make_wheel(tmp_path / "w.whl", {"demo/__init__.py": module, SBOM: sbom})
    before = zipfile.ZipFile(wheel).infolist()
    result = subprocess.run(
        [sys.executable, str(REMAP), str(wheel), str(checkout), "/src"],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 0, result.stderr
    assert f"{SBOM}: 4 URLs of {checkout} mapped to /src" in result.stdout
    with zipfile.ZipFile(wheel) as archive:
        after = archive.infolist()
        text = archive.read(SBOM).decode()
        record = archive.read(f"{DIST}/RECORD").decode()
        assert archive.read("demo/__init__.py") == module
        new = archive.read(SBOM)
    assert [i.filename for i in after] == [i.filename for i in before]
    assert [i.external_attr for i in after] == [i.external_attr for i in before]
    assert text.count("path+file:///src/") == 3 and text.count("path+file:///src#") == 1
    assert f"{url}-other/c#0.1.0" in text
    digest = base64.urlsafe_b64encode(hashlib.sha256(new).digest()).rstrip(b"=").decode()
    assert f"{SBOM},sha256={digest},{len(new)}\n" in record
    assert counted(check(wheel, checkout), SBOM, f"--forbid {checkout}") == 0


# The address of the synthetic objects' one loaded segment, which holds the file from its start:
# their addresses are not their offsets.
BASE = 0x10000
# Alterations of an object whose section headers then hide what its dynamic segment needs: the
# section header size 0, .gnu.version_r counting no entry, the need counting no version (vn_cnt),
# .dynsym and .gnu.version_r retyped. Each object so altered loads only where glibc has what it
# needs, as the loader reads the dynamic segment.
HIDDEN = ["shentsize_0", "verneed_sh_info_0", "vn_cnt_0", "dynsym_and_verneed_retyped"]


def gnu_hash(name: bytes) -> int:
    """The GNU hash of a symbol name."""
    h = 5381
    for c in name:
        h = (h * 33 + c) & 0xFFFFFFFF
    return h


def importer(
    version: str | None,
    kind: str = "",
    extended: bool = False,
    table: bool = True,
    loaded: bool = True,
) -> bytes:
    """A 64-bit little-endian shared object importing `memcpy` from libc.so.6 at `version`.

    As a linker lays one out: a loaded segment holding the file at address `BASE`, the dynamic
    segment, the dynamic strings and symbols (the import, and `demo`, defined), the symbols'
    versions, the version needs (none when `version` is None) and both hash tables; then section
    headers listing the same, unless `table` is false, their count in section 0 (the ELF header's
    field 0) when `extended`. Without program headers unless `loaded`. `kind` alters the object as
    one of `HIDDEN` alters it, or gives DT_VERNEEDNUM 2 (`verneednum_2`), or gives DT_VERNEED the
    version needs' file offset, an address outside the loaded segment (`verneed_unloaded`), or
    makes the dynamic segment's program header a null one (`no_dynamic`), in an executable
    (`static`) rather than a shared object.
    """
    strings = b"\0libc.so.6\0memcpy\0demo\0" + (version or "").encode() + b"\0"
    libc, memcpy, demo, name = 1, 11, 18, 23
    symbols = bytes(24) + struct.pack("<IBBHQQ", memcpy, 0x12, 0, 0, 0, 0)
    symbols += struct.pack("<IBBHQQ", demo, 0x12, 0, 1, BASE, 1)
    versions = struct.pack("<3H", 0, 2 if version else 1, 1)
    count = 0 if kind == "vn_cnt_0" else 1
    need = struct.pack("<HHIII", 1, count, libc, 16, 0) + struct.pack("<IHHII", 0, 0, 2, name, 0)
    # DT_HASH: one bucket, three chains; DT_GNU_HASH: one bucket, the hashed symbols from 2 (demo),
    # one bloom word.
    sysv = struct.pack("<5I", 1, 3, 2, 0, 0) + bytes(4)
    gnu = struct.pack("<4IQII", 1, 2, 1, 6, 0, 2, gnu_hash(b"demo") | 1)
    bodies = [strings, symbols, versions, sysv, gnu] + ([need] if version else [])
    offsets, at = [], 64 + (2 * 56 if loaded else 0)
    for body in bodies:
        at += -at % 8
        offsets.append(at)
        at += len(body)
    entries = [(1, libc), (4, offsets[3]), (0x6FFFFEF5, offsets[4]), (5, offsets[0])]
    entries += [(10, len(strings)), (6, offsets[1]), (11, 24), (0x6FFFFFF0, offsets[2])]
    if version:
        verneednum = 2 if kind == "verneednum_2" else 1
        entries += [(0x6FFFFFFE, offsets[5]), (0x6FFFFFFF, verneednum)]
    # The tags whose values are addresses: the hash tables, the strings, the symbols, their
    # versions and the version needs.
    addresses = {4, 5, 6, 0x6FFFFEF5, 0x6FFFFFF0, 0x6FFFFFFE}
    if kind == "verneed_unloaded":
        addresses.remove(0x6FFFFFFE)
    dynamic = b"".join(
        struct.pack("<qQ", tag, BASE + value if tag in addresses else value)
        for tag, value in entries + [(0, 0)]
    )
    at += -at % 8
    dynamic_at = at
    at += len(dynamic)
    out = bytearray(64)
    if loaded:
        out += struct.pack("<IIQQQQQQ", 1, 6, 0, BASE, BASE, at, at, 0x1000)
        dynamic_address, size = BASE + dynamic_at, len(dynamic)
        dynamic_type = 0 if kind in ("no_dynamic", "static") else 2
        addressed = [dynamic_at, *[dynamic_address] * 2, *[size] * 2, 8]
        out += struct.pack("<IIQQQQQQ", dynamic_type, 6, *addressed)
    for offset, body in zip(offsets + [dynamic_at], bodies + [dynamic], strict=True):
        out += bytes(offset - len(out)) + body
    # Section 1 the strings, 2 the symbols (or PROGBITS, 1), 3 their versions, 4 the version needs
    # (or PROGBITS, or counting none): by body, type, link, info, entry size.
    dynsym, verneed = (1 if kind == "dynsym_and_verneed_retyped" else c for c in (11, 0x6FFFFFFE))
    listed = [(0, 3, 0, 0, 0), (1, dynsym, 1, 1, 24), (2, 0x6FFFFFFF, 2, 0, 2)]
    if version:
        listed.append((5, verneed, 1, 0 if kind == "verneed_sh_info_0" else 1, 0))
    table_at = len(out) if table else 0
    entries = []
    for body, code, link, info, size in listed:
        where, length = offsets[body], len(bodies[body])
        fields = (0, code, 2, BASE + where, where, length, link, info, 8, size)
        entries.append(struct.pack("<IIQQQQIIQQ", *fields))
    sections = len(entries) + 1
    entries.insert(
        0, struct.pack("<IIQQQQIIQQ", 0, 0, 0, 0, 0, sections if extended else 0, 0, 0, 0, 0)
    )
    shentsize = 0 if kind == "shentsize_0" else 64
    shnum = 0 if extended or not table else sections
    phoff, phnum = (64, 2) if loaded else (0, 0)
    object_type = 2 if kind == "static" else 3
    fields = (object_type, 62, 1, 0, phoff, table_at, 0, 64, 56, phnum, shentsize, shnum, 0)
    out[:64] = b"\x7fELF" + bytes([2, 1, 1]) + bytes(9) + struct.pack("<HHIQQQIHHHHHH", *fields)
    return bytes(out) + (b"".join(entries) if table else b"")


def check_glibc(tmp_path: Path, so: bytes) -> subprocess.CompletedProcess[str]:
    """scripts/check_wheel_glibc.py --max 2.28 on a manylinux_2_28 wheel holding `so`."""
    tag = "cp312-abi3-manylinux_2_28_x86_64"
    members = {"demo/_x.so": so, f"{DIST}/WHEEL": f"Tag: {tag}\n".encode()}
    wheel = make_wheel(tmp_path / f"demo-1.0-{tag}.whl", members)
    args = [sys.executable, str(GLIBC), str(wheel), "--max", "2.28"]
    return subprocess.run(args, capture_output=True, text=True, check=False)


def test_glibc_versions_are_read_from_the_dynamic_segment(tmp_path: Path) -> None:
    # With section headers (their count in the ELF header or in section 0) or without, as the
    # loader reads the object.
    for extended, table in [(False, True), (True, True), (False, False)]:
        ok = check_glibc(tmp_path, importer("GLIBC_2.17", extended=extended, table=table))
        assert ok.returncode == 0, ok.stdout + ok.stderr
        assert "demo/_x.so: needs GLIBC_2.17\n  GLIBC_2.17: memcpy" in ok.stdout
        assert "OK: at most glibc 2.28" in ok.stdout
        over = check_glibc(tmp_path, importer("GLIBC_2.39", extended=extended, table=table))
        assert over.returncode == 1, over.stdout
        assert "demo/_x.so: needs GLIBC_2.39\n  GLIBC_2.39: memcpy" in over.stdout, over.stdout
        assert "FAIL: demo/_x.so needs glibc 2.39, the tags promise 2.28" in over.stdout
        assert "FAIL: glibc 2.39 is above the maximum, 2.28" in over.stdout


def test_an_object_whose_glibc_needs_are_unknown_fails(tmp_path: Path) -> None:
    for so, why in [
        (importer("GLIBC_2.17", loaded=False), "no program headers"),
        (importer(None), "imported symbols but no version needs (DT_VERNEED)"),
    ]:
        result = check_glibc(tmp_path, so)
        assert result.returncode == 1, result.stdout
        want = f"FAIL: demo/_x.so: its glibc needs cannot be established ({why})"
        assert want in result.stdout, result.stdout
        assert "OK:" not in result.stdout


@pytest.mark.parametrize("kind", HIDDEN)
def test_section_headers_hiding_the_needs_fail(tmp_path: Path, kind: str) -> None:
    # The loader reads GLIBC_2.39 (or 2.17) in the dynamic segment, and so does the check; the
    # section headers, which disagree, fail it too, even when what the dynamic segment needs is
    # old enough.
    why = {
        "shentsize_0": "its section headers cannot be read to cross-check (section headers of "
        "0 bytes, not 64)",
    }.get(kind, "its section headers disagree with its dynamic segment on {}")
    for version in ["GLIBC_2.39", "GLIBC_2.17"]:
        result = check_glibc(tmp_path, importer(version, kind))
        assert result.returncode == 1, result.stdout
        assert f"demo/_x.so: needs {version}\n" in result.stdout, result.stdout
        assert f"FAIL: demo/_x.so: {why.format(version)}" in result.stdout, result.stdout
        over = "FAIL: demo/_x.so needs glibc 2.39, the tags promise 2.28" in result.stdout
        assert over == (version == "GLIBC_2.39"), result.stdout
        assert "OK:" not in result.stdout


def test_counts_their_chains_disagree_with_fail(tmp_path: Path) -> None:
    # Without section headers: the loader follows the chains, whatever vn_cnt and DT_VERNEEDNUM
    # count, and so does the check, which reports the disagreement.
    for kind, why in [
        ("vn_cnt_0", "the needs of libc.so.6 count 0 versions, their chain 1"),
        ("verneednum_2", "DT_VERNEEDNUM counts 2 libraries, the chain 1"),
    ]:
        result = check_glibc(tmp_path, importer("GLIBC_2.17", kind, table=False))
        assert result.returncode == 1, result.stdout
        assert "demo/_x.so: needs GLIBC_2.17\n  GLIBC_2.17: memcpy" in result.stdout
        assert f"FAIL: demo/_x.so: {why}" in result.stdout, result.stdout


def test_an_address_outside_the_loaded_segments_fails(tmp_path: Path) -> None:
    # DT_VERNEED gives the version needs' file offset, an address that no loaded segment holds:
    # the loader would read the needs elsewhere. Read at that offset, the needs (GLIBC_2.17)
    # would pass the check.
    result = check_glibc(tmp_path, importer("GLIBC_2.17", "verneed_unloaded"))
    assert result.returncode == 1, result.stdout
    match = re.search(
        r"^FAIL: demo/_x\.so: its glibc needs cannot be established \(16 bytes at address "
        r"(0x[0-9a-f]+) are outside the loaded segments\)$",
        result.stdout,
        re.MULTILINE,
    )
    assert match and 0 < int(match.group(1), 16) < BASE, result.stdout
    assert "OK:" not in result.stdout


def test_a_shared_object_without_a_dynamic_segment_fails(tmp_path: Path) -> None:
    # Its dynamic segment's program header made a null one, and no section headers: the loader
    # refuses it ("object file has no dynamic section"), and the check fails it, whatever its
    # dynamic array needs. An executable without one is statically linked and needs nothing.
    for version in ["GLIBC_2.39", "GLIBC_2.17"]:
        result = check_glibc(tmp_path, importer(version, "no_dynamic", table=False))
        assert result.returncode == 1, result.stdout
        why = "no dynamic segment (PT_DYNAMIC)"
        assert f"FAIL: demo/_x.so: its glibc needs cannot be established ({why})" in result.stdout
        assert "OK:" not in result.stdout
    static = check_glibc(tmp_path, importer("GLIBC_2.39", "static", table=False))
    assert static.returncode == 0, static.stdout
    assert "demo/_x.so: no glibc symbol versions" in static.stdout, static.stdout
    assert "OK: at most glibc 2.28" in static.stdout

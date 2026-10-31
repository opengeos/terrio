"""The unsafe gate: scripts/check_unsafe.py on this workspace, and its heuristic on small ones.

The script counts the `unsafe` of each file in compiled, non-test code and compares the counts
with scripts/unsafe_allowlist.toml. In the small workspaces below, a line marked `// counted` is
one the script must list, and no other.
"""

import re
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

tomllib = pytest.importorskip("tomllib", reason="the gate needs Python 3.11 or later")

ROOT = Path(__file__).resolve().parents[2]
SCRIPT = ROOT / "scripts" / "check_unsafe.py"
SITE = re.compile(r"^  (\S+):(\d+): ", re.MULTILINE)


def run(root: Path, allowlist: Path | None = None) -> subprocess.CompletedProcess[str]:
    args = [sys.executable, str(SCRIPT), "--root", str(root), "--list"]
    if allowlist is not None:
        args += ["--allowlist", str(allowlist)]
    return subprocess.run(args, capture_output=True, text=True, check=False)


def workspace(tmp_path: Path, files: dict[str, str], allowed: dict[str, int]) -> Path:
    """A workspace of one crate, `crates/a`, with `files` in its src/, and its allow-list."""
    (tmp_path / "Cargo.toml").write_text('[workspace]\nmembers = ["crates/*"]\n')
    crate = tmp_path / "crates" / "a"
    (crate / "src").mkdir(parents=True)
    (crate / "Cargo.toml").write_text('[package]\nname = "a"\n')
    for name, text in files.items():
        path = crate / "src" / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(text).lstrip("\n"))
    allowlist = tmp_path / "allowlist.toml"
    entries = [
        f'[files."crates/a/src/{n}"]\ncount = {c}\nreason = "r"\norigin = "added"\n'
        for n, c in allowed.items()
    ]
    allowlist.write_text("[files]\n" + "".join(entries))
    return allowlist


def sites(output: str) -> set[tuple[str, int]]:
    return {(path, int(line)) for path, line in SITE.findall(output)}


def marked(files: dict[str, str]) -> set[tuple[str, int]]:
    """The lines of `files` marked `// counted`."""
    found: set[tuple[str, int]] = set()
    for name, text in files.items():
        lines = textwrap.dedent(text).lstrip("\n").splitlines()
        for number, line in enumerate(lines, 1):
            if line.endswith("// counted"):
                found.add((f"crates/a/src/{name}", number))
    return found


def test_the_workspace_has_the_unsafe_its_allow_list_allows() -> None:
    allowed = tomllib.loads((ROOT / "scripts" / "unsafe_allowlist.toml").read_text())["files"]
    result = run(ROOT)
    assert result.returncode == 0, result.stdout + result.stderr
    total = sum(entry["count"] for entry in allowed.values())
    assert f"unsafe in compiled, non-test code: {total} in {len(allowed)} files" in result.stdout
    listed = sites(result.stdout)
    for path, entry in allowed.items():
        assert len({line for p, line in listed if p == path}) == entry["count"], path
    assert all(entry["origin"] in ("existing", "added") for entry in allowed.values())


LITERALS = {
    "lib.rs": """
        //! unsafe in the crate's documentation
        /// unsafe in an item's documentation, with an example: `unsafe { f() }`
        /* unsafe /* nested unsafe */ still a comment: unsafe */
        pub fn f() -> usize {
            let a = "unsafe member path";
            let b = r#"unsafe "quoted" unsafe"#;
            let c = b"unsafe";
            let d = '"';
            let e = "an escaped \\" unsafe";
            let g = '\\'';
            #[allow(unsafe_code)]
            let r#unsafe = 1;
            a.len() + b.len() + c.len() + (d as usize) + e.len() + (g as usize) + r#unsafe
        }
        pub fn h<'a>(x: &'a [u8]) -> u8 {
            unsafe { *x.as_ptr() } // counted
        }
    """,
}


def test_comments_strings_characters_and_identifiers_do_not_count(tmp_path: Path) -> None:
    result = run(tmp_path, workspace(tmp_path, LITERALS, {"lib.rs": 1}))
    assert result.returncode == 0, result.stdout
    assert sites(result.stdout) == marked(LITERALS) == {("crates/a/src/lib.rs", 16)}


TESTS = {
    "lib.rs": """
        #[cfg(test)]
        mod tests {
            fn f() { unsafe {} }
        }
        #[cfg(all(test, unix))]
        fn g() { unsafe {} }
        #[cfg(all(unix, test))]
        #[allow(dead_code)]
        fn g2() { unsafe {} }
        #[test]
        fn h() { unsafe {} }
        #[cfg(test)]
        mod more;
        #[cfg(not(test))]
        fn i() { unsafe {} } // counted
        #[cfg(any(test, unix))]
        fn j() { unsafe {} } // counted
        #[cfg(all(not(test), unix))]
        fn j2() { unsafe {} } // counted
        #[cfg_attr(test, allow(dead_code))]
        fn k() { unsafe {} } // counted
        struct S {
            #[cfg(test)]
            pub x: u8,
            y: unsafe fn(), // counted
        }
        mod inline {
            #![cfg(test)]
            fn m() { unsafe {} }
        }
        fn l() { unsafe {} } // counted
    """,
    "more.rs": """
        mod deeper;
        fn n() { unsafe {} }
    """,
    "more/deeper.rs": """
        fn o() { unsafe {} }
    """,
    "tests/helper.rs": """
        fn p() { unsafe {} }
    """,
}


def test_test_code_does_not_count_and_other_cfgs_do(tmp_path: Path) -> None:
    result = run(tmp_path, workspace(tmp_path, TESTS, {"lib.rs": 6}))
    assert result.returncode == 0, result.stdout
    assert sites(result.stdout) == marked(TESTS)
    assert len(marked(TESTS)) == 6


@pytest.mark.parametrize(
    ("files", "allowed", "failure"),
    [
        (
            {"lib.rs": "fn f() { unsafe {} }\n", "other.rs": "// g\nfn g() { unsafe {} }\n"},
            {"lib.rs": 1},
            "FAIL crates/a/src/other.rs: 1 unsafe at line 2, not in the allow-list: a new unsafe "
            "needs a recorded decision (+1)",
        ),
        (
            {"lib.rs": "fn f() { unsafe {} }\nfn g() { unsafe {} }\n"},
            {"lib.rs": 1},
            "FAIL crates/a/src/lib.rs: 2 unsafe at lines 1, 2, allowed 1: a new unsafe needs a "
            "recorded decision (+1)",
        ),
        (
            {"lib.rs": "fn f() { unsafe {} }\n"},
            {"lib.rs": 3},
            "FAIL crates/a/src/lib.rs: 1 unsafe at line 1, allowed 3: lower the allow-list's "
            "count (-2)",
        ),
        (
            {"lib.rs": "fn f() {}\n"},
            {"gone.rs": 1},
            "FAIL crates/a/src/gone.rs: 0 unsafe, allowed 1: remove the allow-list's entry (-1)",
        ),
    ],
    ids=["file-not-allowed", "one-more", "fewer", "entry-of-no-file"],
)
def test_a_difference_fails_and_names_the_file_and_the_difference(
    tmp_path: Path, files: dict[str, str], allowed: dict[str, int], failure: str
) -> None:
    result = run(tmp_path, workspace(tmp_path, files, allowed))
    assert result.returncode == 1
    assert [line for line in result.stdout.splitlines() if line.startswith("FAIL")] == [failure]


def test_an_allowance_needs_a_reason(tmp_path: Path) -> None:
    allowlist = workspace(tmp_path, {"lib.rs": "fn f() { unsafe {} }\n"}, {})
    allowlist.write_text(
        '[files."crates/a/src/lib.rs"]\ncount = 1\nreason = " "\norigin = "added"\n'
    )
    result = run(tmp_path, allowlist)
    assert result.returncode == 1
    assert "FAIL crates/a/src/lib.rs: the allow-list's reason must be one line" in result.stdout


@pytest.mark.parametrize("origin", ['origin = "upstream"\n', "origin = 1\n", ""])
def test_an_allowance_needs_an_existing_or_added_origin(tmp_path: Path, origin: str) -> None:
    allowlist = workspace(tmp_path, {"lib.rs": "fn f() { unsafe {} }\n"}, {})
    allowlist.write_text('[files."crates/a/src/lib.rs"]\ncount = 1\nreason = "r"\n' + origin)
    result = run(tmp_path, allowlist)
    assert result.returncode == 1
    assert [line for line in result.stdout.splitlines() if line.startswith("FAIL")] == [
        "FAIL crates/a/src/lib.rs: the allow-list's origin must be one of existing, added"
    ]

"""Check the ``unsafe`` in the workspace's own crates against a reviewed allow-list.

Usage: python scripts/check_unsafe.py [--root DIR] [--allowlist FILE] [--list]

Counts the ``unsafe`` keyword (blocks, functions, impls, traits, extern blocks and
``#[unsafe(...)]`` attributes alike) in the compiled, non-test source of each workspace member (the
``src/`` directories of the ``[workspace] members`` of the root ``Cargo.toml``), and compares the
count of each file with ``scripts/unsafe_allowlist.toml``, where each allowed count has a one-line
reason and an origin, ``"existing"`` for code present before the change that added the allow-list
or ``"added"`` for code that change added. It prints every file that differs, with its count, the
allowed one, the difference and the lines, and exits 1 when one does: a file with more ``unsafe``
than allowed, one not in the allow-list, or one with fewer (lower its count, so that the slack
cannot hide a new one); it exits 1 too on an entry without a positive count, a one-line reason or
one of the two origins. ``--list`` prints every site counted. Needs Python 3.11 or later
(``tomllib``).

What counts is decided by a lexical heuristic, not by the compiler:

- comments (line, block and nested block comments, doc comments with their examples) and literals
  (strings, raw, byte and C strings, characters) are blanked first, so ``"unsafe member path"``
  does not count; neither do ``unsafe_code``-like words or the raw identifier ``r#unsafe``;
- an item, statement, field or match arm under ``#[cfg(test)]``, under ``#[cfg(all(test, ...))]``
  (``test`` an argument of the outermost ``all``) or marked ``#[test]`` is test code, as is a block
  or file under an inner ``#![cfg(test)]``, the file of a ``mod name;`` declared under them (with
  its submodules) and any file under a ``tests/`` directory;
- an item ends at its first ``;`` or its braced body, a field or arm at the next ``,``, whatever
  comes first outside brackets.

Its limits: other cfgs count as compiled (a platform's or a feature's code counts, and so does
``cfg(any(test, ...))``); ``unsafe`` that macros generate (pyo3's, for one) and code brought in by
``include!`` are not seen; a test module's file is found by the ``mod`` declaration's name or
``#[path]``, as rustc finds it, but not through macros; braces in a generic argument before an
item's body (``Foo<{ N }>``) end the item early, which counts more, never less. Build scripts,
benches, examples and fuzz targets are not scanned.
"""

import argparse
import re
import sys
import tomllib
from pathlib import Path

ALLOWLIST = Path(__file__).with_name("unsafe_allowlist.toml")
# An entry's origin: code present before the change that added the allow-list, or code it added.
ORIGINS = ("existing", "added")
UNSAFE = re.compile(r"(?<!r#)\bunsafe\b")
IDENT = re.compile(r"[A-Za-z_][A-Za-z0-9_]*")
RAW_STRING = re.compile(r'[bc]?r(#*)"')
# The tokens that start an item or a `let` statement (anything else under a cfg is a field, a
# variant, a match arm or an expression, which a comma ends).
ITEMS = {
    "async", "const", "crate", "default", "enum", "extern", "fn", "impl", "let", "macro_rules",
    "mod", "static", "struct", "trait", "type", "union", "unsafe", "use",
}  # fmt: skip


def blank(source):
    """Blank the comments and literals of Rust source, keeping every offset and line break.

    Args:
        source: The text of a Rust file.

    Returns:
        The text, each character of a comment or literal but line breaks replaced by a space.
    """
    out = list(source)
    n = len(source)

    def clear(start, stop):
        for k in range(start, min(stop, n)):
            if out[k] != "\n":
                out[k] = " "

    def ident_before(at):
        return at > 0 and (source[at - 1].isalnum() or source[at - 1] == "_")

    i = 0
    while i < n:
        c = source[i]
        if source.startswith("//", i):
            stop = source.find("\n", i)
            stop = n if stop < 0 else stop
        elif source.startswith("/*", i):
            depth, stop = 1, i + 2
            while stop < n and depth:
                if source.startswith("/*", stop):
                    depth, stop = depth + 1, stop + 2
                elif source.startswith("*/", stop):
                    depth, stop = depth - 1, stop + 2
                else:
                    stop += 1
        elif c in "bcr" and not ident_before(i) and RAW_STRING.match(source, i):
            m = RAW_STRING.match(source, i)
            close = source.find('"' + m.group(1), m.end())
            stop = n if close < 0 else close + 1 + len(m.group(1))
        elif c == '"' or (c in "bc" and source.startswith('"', i + 1) and not ident_before(i)):
            stop = i + (1 if c == '"' else 2)
            while stop < n and source[stop] != '"':
                stop += 2 if source[stop] == "\\" else 1
            stop += 1
        elif c == "'" or (c == "b" and source.startswith("'", i + 1) and not ident_before(i)):
            quote = i if c == "'" else i + 1
            if source.startswith("\\", quote + 1):
                stop = source.find("'", quote + 3)
                stop = n if stop < 0 else stop + 1
            elif source.startswith("'", quote + 2):
                stop = quote + 3
            else:  # a lifetime or a label
                i = quote + 1
                continue
        else:
            i += 1
            continue
        clear(i, stop)
        i = stop
    return "".join(out)


def closing(text, at):
    """The offset of the bracket that closes the one at ``at``, or the end of the text.

    Args:
        text: Blanked Rust source.
        at: The offset of an opening bracket.

    Returns:
        The offset of its closing bracket.
    """
    depth = 0
    for k in range(at, len(text)):
        if text[k] in "([{":
            depth += 1
        elif text[k] in ")]}":
            depth -= 1
            if depth == 0:
                return k
    return len(text)


def test_only(attribute):
    """Whether an attribute's content makes its item test code.

    Args:
        attribute: The text between ``#[`` and ``]``.

    Returns:
        True for ``test``, ``cfg(test)`` and ``cfg(all(..., test, ...))``.
    """
    content = re.sub(r"\s+", "", attribute)
    if content == "test":
        return True
    m = re.fullmatch(r"cfg\((.*)\)", content)
    if not m:
        return False
    predicate = m.group(1)
    if predicate == "test":
        return True
    if not (predicate.startswith("all(") and closing(predicate, 3) == len(predicate) - 1):
        return False
    args, depth, start = [], 0, 4
    for k, ch in enumerate(predicate[4:-1], 4):
        depth += (ch == "(") - (ch == ")")
        if ch == "," and depth == 0:
            args.append(predicate[start:k])
            start = k + 1
    args.append(predicate[start:-1])
    return "test" in args


def item_end(text, at):
    """The end of the item, statement, field or arm that starts at ``at``.

    Args:
        text: Blanked Rust source.
        at: The offset of the item's first token, after its attributes.

    Returns:
        The offset just past it.
    """
    words = IDENT.findall(text[at : at + 200])
    first = words[0] if words else ""
    if first == "pub":
        rest = re.match(r"pub\s*(\([^)]*\))?\s*([A-Za-z_]\w*)?", text[at:])
        first = rest.group(2) or "" if rest else ""
    commas = first not in ITEMS
    depth = 0
    for k in range(at, len(text)):
        ch = text[k]
        if ch == "{" and depth == 0:
            return closing(text, k) + 1
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            if depth == 0:
                return k
            depth -= 1
        elif depth == 0 and (ch == ";" or (ch == "," and commas)):
            return k + 1
    return len(text)


def test_spans(text):
    """The spans of test code in blanked Rust source, and the test modules it declares.

    Args:
        text: Blanked Rust source.

    Returns:
        A list of ``(start, stop)`` offsets, and the ``(name, path attribute or None)`` of each
        ``mod name;`` declared as test code. A whole file of test code is one span.
    """
    spans, modules = [], []
    i = 0
    while (i := text.find("#", i)) >= 0:
        j = i + 1
        while j < len(text) and text[j].isspace():
            j += 1
        if text.startswith("!", j):
            k = j + 1
            while k < len(text) and text[k].isspace():
                k += 1
            if text.startswith("[", k):
                stop = closing(text, k)
                if test_only(text[k + 1 : stop]):
                    opened = [m for m in range(i) if text[m] == "{"]
                    holder = next((m for m in reversed(opened) if closing(text, m) > i), None)
                    spans.append(
                        (0, len(text)) if holder is None else (holder, closing(text, holder) + 1)
                    )
                i = stop + 1
                continue
        if not text.startswith("[", j):
            i += 1
            continue
        start, test, k, attrs = i, False, i, []
        while text.startswith("#", k):
            j = k + 1
            while j < len(text) and text[j].isspace():
                j += 1
            if not text.startswith("[", j):
                break
            stop = closing(text, j)
            attrs.append((j, stop))
            test = test or test_only(text[j + 1 : stop])
            k = stop + 1
            while k < len(text) and text[k].isspace():
                k += 1
        if test:
            stop = item_end(text, k)
            spans.append((start, stop))
            m = re.match(r"(?:pub\s*(?:\([^)]*\))?\s*)?mod\s+([A-Za-z_]\w*)\s*;", text[k:stop])
            if m:
                paths = [a for a in attrs if re.match(r"\[\s*path\s*=", text[a[0] : a[1]])]
                modules.append((m.group(1), paths[0] if paths else None))
            i = stop
        else:
            i = k if k > i else i + 1
    return spans, modules


def module_file(parent, name):
    """The file of module ``name`` declared in ``parent``, as rustc finds it.

    Args:
        parent: The declaring file.
        name: The module's name.

    Returns:
        The path of ``name.rs`` or ``name/mod.rs``, whichever exists, else the former.
    """
    home = (
        parent.parent if parent.name in ("lib.rs", "main.rs", "mod.rs") else parent.with_suffix("")
    )
    file = home / f"{name}.rs"
    if not file.exists() and (home / name / "mod.rs").exists():
        file = home / name / "mod.rs"
    return file.resolve()


def sources(root):
    """The Rust files of the workspace members' ``src/`` directories.

    Args:
        root: The workspace's root directory.

    Returns:
        Their paths, sorted.
    """
    manifest = tomllib.loads((root / "Cargo.toml").read_text(encoding="utf-8"))
    files = set()
    for pattern in manifest.get("workspace", {}).get("members", []):
        for member in root.glob(pattern):
            if (member / "Cargo.toml").is_file():
                files.update((member / "src").rglob("*.rs"))
    return sorted(files)


def count(root):
    """The lines of the ``unsafe`` of each file in compiled, non-test code.

    Args:
        root: The workspace's root directory.

    Returns:
        A dict from each file with any (a POSIX path relative to ``root``) to the sorted line
        numbers of its ``unsafe``, one per occurrence.
    """
    files = sources(root)
    texts = {f: blank(f.read_text(encoding="utf-8")) for f in files}
    found = {f: test_spans(t) for f, t in texts.items()}
    tests = {f for f in files if "tests" in f.relative_to(root).parts[:-1]}
    pending = list(tests)
    for f, (_, modules) in found.items():
        source = f.read_text(encoding="utf-8")
        for name, path in modules:
            literal = re.search(r'"([^"]*)"', source[path[0] : path[1]]) if path else None
            pending.append(
                (f.parent / literal.group(1)).resolve() if literal else module_file(f, name)
            )
    while pending:  # a test module's submodules are test code too
        f = pending.pop()
        tests.add(f)
        if f in texts:
            children = re.findall(r"\bmod\s+([A-Za-z_]\w*)\s*;", texts[f])
            pending.extend(c for c in (module_file(f, n) for n in children) if c not in tests)
    result = {}
    for f, text in texts.items():
        if f in tests:
            continue
        spans = found[f][0]
        lines = [
            text.count("\n", 0, m.start()) + 1
            for m in UNSAFE.finditer(text)
            if not any(a <= m.start() < b for a, b in spans)
        ]
        if lines:
            result[f.relative_to(root).as_posix()] = lines
    return result


def main():
    """Compare the ``unsafe`` counted with the allow-list; exit 1 on any difference."""
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent)
    parser.add_argument("--allowlist", type=Path, default=ALLOWLIST)
    parser.add_argument("--list", action="store_true", help="print every site counted")
    args = parser.parse_args()
    root = args.root.resolve()
    allowed = tomllib.loads(args.allowlist.read_text(encoding="utf-8")).get("files", {})
    found = count(root)
    problems, limits = [], {}
    for path, entry in sorted(allowed.items()):
        limit, reason = entry.get("count"), entry.get("reason")
        if isinstance(limit, int) and not isinstance(limit, bool) and limit > 0:
            limits[path] = limit
        else:
            problems.append(f"{path}: the allow-list's count must be a positive integer")
        if not isinstance(reason, str) or not reason.strip() or "\n" in reason.strip():
            problems.append(f"{path}: the allow-list's reason must be one line")
        if entry.get("origin") not in ORIGINS:
            problems.append(f"{path}: the allow-list's origin must be one of {', '.join(ORIGINS)}")
    for path in sorted(set(found) | set(limits)):
        lines, limit = found.get(path, []), limits.get(path, 0)
        if len(lines) == limit:
            continue
        where = (
            f" at line{'s' if len(lines) > 1 else ''} {', '.join(map(str, lines))}" if lines else ""
        )
        if path not in limits:
            note = "not in the allow-list: a new unsafe needs a recorded decision"
        elif len(lines) > limit:
            note = f"allowed {limit}: a new unsafe needs a recorded decision"
        elif lines:
            note = f"allowed {limit}: lower the allow-list's count"
        else:
            note = f"allowed {limit}: remove the allow-list's entry"
        problems.append(f"{path}: {len(lines)} unsafe{where}, {note} ({len(lines) - limit:+d})")
    total = sum(map(len, found.values()))
    print(
        f"unsafe in compiled, non-test code: {total} in {len(found)} files ({args.allowlist.name})"
    )
    if args.list:
        for path, lines in sorted(found.items()):
            source = (root / path).read_text(encoding="utf-8").splitlines()
            for line in lines:
                print(f"  {path}:{line}: {source[line - 1].strip()}")
    for problem in problems:
        print(f"FAIL {problem}")
    if not problems:
        print(f"OK: every file as allowed ({len(limits)} files)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())

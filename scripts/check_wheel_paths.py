r"""Check that a wheel embeds no absolute path of the machine that built it.

Usage: python scripts/check_wheel_paths.py WHEEL [--forbid PATH ...]

A build leaves its paths in what it compiles. Rust names a source file in each panic location
and debug string (``file!()``, ``#[track_caller]``), so a plain build of the extension module
embeds the builder's Cargo home (the registry's sources, git checkouts), its rustup toolchain
(the standard library's sources, when the ``rust-src`` component is installed) and the target
directory (the files a build script generates in ``OUT_DIR``); maturin's SBOM names the checkout.
``scripts/build_wheel_manylinux_2_28.sh`` maps them to neutral prefixes and runs this check.

The check reads every file of the wheel as bytes (a shared object's strings with the rest, and
the sections an ELF object stores compressed, decompressed) and looks for:

- home directories: ``/home/<user>/``, ``/root/``, ``/Users/<user>/`` and ``C:\Users\`` (any
  drive, either slash, a backslash maybe doubled as in an escaped string);
- the directory in ``HOME``, when it is set;
- each ``--forbid`` path, such as the build's target directory and checkout, as given and with
  its symbolic links resolved, also percent-encoded as in a file URL (an SBOM's).

A ``HOME`` or ``--forbid`` path counts where no file name character (a letter, a digit, ``.``,
``_``, ``-``, ``~``, ``+`` or a non-ASCII byte) follows it: ``/work/repo`` is not found in
``/work/repository``. Matches that overlap are one path.

It prints the number of paths found, the files holding them and up to 10 examples, and exits 1
when it finds any or cannot read a file (an ELF section compressed otherwise than with zlib).
"""

import argparse
import os
import re
import struct
import sys
import zipfile
import zlib
from pathlib import Path
from urllib.parse import quote

# Home directories: Linux and most Unix systems, root's, macOS and Windows.
HOMES = [
    ("/home/<user>/", rb"/home/[^/\\\x00-\x20\x7f]+/"),
    ("/root/", rb"/root/"),
    ("/Users/<user>/", rb"/Users/[^/\\\x00-\x20\x7f]+/"),
    ("C:\\Users\\", rb"[A-Za-z]:(?:\\{1,2}|/)[Uu][Ss][Ee][Rr][Ss](?:\\{1,2}|/)"),
]
# What may not follow a HOME or --forbid path: a character that continues a file name.
NAME_GOES_ON = rb"(?![A-Za-z0-9._~+\-\x80-\xff])"
# The characters Cargo's file URLs keep as they are, besides letters, digits and "_.-~" (which
# quote() keeps too): the url crate's Url::from_file_path percent-encodes the others.
URL_KEPT = "/!$&'()*+,:;=@[]^|"
SHF_COMPRESSED = 0x800
ELFCOMPRESS_ZLIB = 1
EXAMPLES = 10


def variants(path):
    """The byte strings a directory may appear as in a build's output.

    Args:
        path: The directory, absolute or relative to the working directory.

    Returns:
        Its absolute path as given and with symbolic links resolved, without a trailing
        separator, each also percent-encoded as in a file URL and with its backslashes doubled
        as in an escaped string.
    """
    forms = set()
    for form in (os.path.normpath(os.path.abspath(path)), os.path.realpath(path)):
        raw = os.fsencode(form).rstrip(b"/\\")
        forms |= {raw, quote(raw, safe=URL_KEPT).encode(), raw.replace(b"\\", b"\\\\")}
    return sorted(forms)


def rules(directories):
    """The patterns to look for.

    Args:
        directories: ``(label, path)`` pairs, for ``HOME`` and each ``--forbid`` path.

    Returns:
        ``(label, compiled pattern)`` pairs: the home directories, then each directory's
        variants.

    Raises:
        ValueError: When a directory is the root, which every absolute path is in.
    """
    found = [(label, re.compile(pattern)) for label, pattern in HOMES]
    for label, path in directories:
        for raw in variants(path):
            if not raw:
                raise ValueError(f"{label}: {path!r} is the root directory")
            found.append((label, re.compile(re.escape(raw) + NAME_GOES_ON)))
    return found


def compressed_sections(data):
    """The sections an ELF object stores compressed, decompressed.

    Args:
        data: The object's bytes.

    Returns:
        ``(section name, contents)`` pairs for the sections with the ``SHF_COMPRESSED`` flag
        and GNU's older ``.zdebug`` sections.

    Raises:
        ValueError: When the object cannot be parsed, or a section is compressed otherwise than
            with zlib.
    """
    if data[4] not in (1, 2) or data[5] not in (1, 2):
        raise ValueError("not a 32- or 64-bit ELF object of a known byte order")
    end = "<" if data[5] == 1 else ">"
    if data[4] == 2:
        (shoff,) = struct.unpack_from(end + "Q", data, 0x28)
        shentsize, shnum, shstrndx = struct.unpack_from(end + "HHH", data, 0x3A)
        layout, header = end + "IIQQQQIIQQ", end + "IIQQ"
    else:
        (shoff,) = struct.unpack_from(end + "I", data, 0x20)
        shentsize, shnum, shstrndx = struct.unpack_from(end + "HHH", data, 0x2E)
        layout, header = end + "IIIIIIIIII", end + "III"
    if not shoff:
        return []
    # Each section: name, type, flags, addr, offset, size, link, info, addralign, entsize. With
    # more sections than the ELF header's fields hold, section 0 holds the count and the index.
    first = struct.unpack_from(layout, data, shoff)
    shnum = shnum or first[5]
    shstrndx = first[6] if shstrndx == 0xFFFF else shstrndx
    sections = [struct.unpack_from(layout, data, shoff + i * shentsize) for i in range(shnum)]
    names = sections[shstrndx][4]
    found = []
    for sec in sections:
        name = data[names + sec[0] : data.index(b"\0", names + sec[0])].decode(errors="replace")
        body = data[sec[4] : sec[4] + sec[5]]
        if sec[2] & SHF_COMPRESSED:
            (method,) = struct.unpack_from(end + "I", body)
            if method != ELFCOMPRESS_ZLIB:
                raise ValueError(f"section {name} is compressed with method {method}, not zlib")
            found.append((name, zlib.decompress(body[struct.calcsize(header) :])))
        elif name.startswith(".zdebug") and body[:4] == b"ZLIB":
            found.append((name, zlib.decompress(body[12:])))
    return found


def occurrences(data, patterns):
    """The paths some bytes hold.

    Args:
        data: The bytes.
        patterns: ``(label, compiled pattern)`` pairs.

    Returns:
        ``(start, end, label)`` triples in order, matches that overlap merged into one, labelled
        with the longest of those starting first.
    """
    matches = sorted(
        ((m.start(), m.end(), label) for label, rx in patterns for m in rx.finditer(data)),
        key=lambda m: (m[0], -m[1]),
    )
    merged = []
    for start, end, label in matches:
        if merged and start < merged[-1][1]:
            merged[-1] = (merged[-1][0], max(end, merged[-1][1]), merged[-1][2])
        else:
            merged.append((start, end, label))
    return merged


def excerpt(data, start, stop):
    """The text of a path found, for an example.

    Args:
        data: The bytes holding it.
        start: Where it starts.
        stop: Where the next path found starts, or None.

    Returns:
        The printable characters from ``start``, up to ``stop`` and at most 120 of them.
    """
    limit = min(start + 120, stop if stop is not None else len(data))
    return re.match(rb"[^\x00-\x20\x7f-\xff\"'`<>|]*", data[start:limit]).group(0).decode()


def main():
    """Report the build-machine paths in a wheel and exit 1 when there are any."""
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("wheel", help="the .whl file")
    parser.add_argument(
        "--forbid",
        action="append",
        default=[],
        metavar="PATH",
        help="a directory of the build machine, such as the target directory (repeatable)",
    )
    args = parser.parse_args()
    directories = [(f"--forbid {path}", path) for path in args.forbid]
    home = os.environ.get("HOME", "")
    if os.path.isabs(home) and os.path.realpath(home) != os.path.realpath(os.sep):
        directories.insert(0, (f"HOME {home}", home))
    try:
        patterns = rules(directories)
    except ValueError as error:
        parser.error(str(error))
    found = {}  # part (a file, or one of its sections) -> [(label, kind, example)]
    files = set()  # the files holding a path
    problems = []
    with zipfile.ZipFile(args.wheel) as archive:
        names = archive.namelist()
        for name in names:
            data = archive.read(name)
            parts = [(name, data)]
            if data[:4] == b"\x7fELF":
                try:
                    parts += [(f"{name} [{s}]", body) for s, body in compressed_sections(data)]
                except (ValueError, struct.error, IndexError, zlib.error) as error:
                    problems.append(f"{name}: cannot read its compressed sections: {error}")
            for part, body in parts:
                spans = occurrences(body, patterns)
                for i, (start, end, label) in enumerate(spans):
                    stop = spans[i + 1][0] if i + 1 < len(spans) else None
                    text = excerpt(body, start, stop)
                    # The kind of path: the two names after the match (".cargo/registry").
                    kind = "/".join(text[end - start :].split("/")[:2])
                    found.setdefault(part, []).append((label, kind, text))
                    files.add(name)
    print(f"wheel: {Path(args.wheel).name}, {len(names)} files")
    looked = [label for label, _ in HOMES] + [label for label, _ in directories]
    print(f"looking for: {', '.join(looked)}")
    for part, hits in found.items():
        labels = {}
        for label, _, _ in hits:
            labels[label] = labels.get(label, 0) + 1
        print(f"{part}: {len(hits)} ({', '.join(f'{k}: {n}' for k, n in labels.items())})")
    total = sum(len(hits) for hits in found.values())
    print(f"paths found: {total}")
    if total:
        print(f"examples ({min(total, EXAMPLES)} of {total}):")
        # Round robin over the files and kinds of path, so that each shows.
        groups = {}
        for part, hits in found.items():
            for label, kind, text in hits:
                groups.setdefault((part, label, kind), []).append(text)
        shown, row = 0, 0
        while shown < min(total, EXAMPLES):
            for (part, _, _), texts in groups.items():
                if row < len(texts) and shown < EXAMPLES:
                    print(f"  {part}: {texts[row]}")
                    shown += 1
            row += 1
        problems.append(
            f"{total} absolute paths of the build machine in {len(files)} of {len(names)} files"
        )
    for problem in problems:
        print(f"FAIL: {problem}")
    if not problems:
        print(f"OK: no absolute path of the build machine in {len(names)} files")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())

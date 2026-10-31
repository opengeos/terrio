"""Map the checkout's path in a wheel's SBOMs to a neutral one, as the build maps rustc's paths.

Usage: python scripts/remap_wheel_sbom.py WHEEL FROM TO

maturin puts a CycloneDX SBOM of the Rust dependency tree in the wheel's ``.dist-info/sboms/``.
It names each crate of the workspace by its Cargo package ID, for a local crate a
``path+file://`` URL of the crate's directory: the build machine's checkout, which rustc's
``--remap-path-prefix`` does not reach. The script replaces FROM, at the start of those URLs'
paths (as given and percent-encoded, followed by ``/`` or ``#``), with TO in each file of
``.dist-info/sboms/``, updates their RECORD entries and rewrites the wheel in place, with the
members in the same order and with the same attributes. It prints how many URLs each file had.
"""

import argparse
import base64
import csv
import hashlib
import io
import os
import re
import sys
import zipfile
from urllib.parse import quote

URL = "path+file://"
# The characters Cargo's file URLs keep as they are, besides letters, digits and "_.-~" (which
# quote() keeps too): the url crate's Url::from_file_path percent-encodes controls, space, '"',
# "#", "%", "<", ">", "?", "\\", "`", "{", "}" and non-ASCII bytes.
KEPT = "/!$&'()*+,:;=@[]^|"


def remap(text, old, new):
    """Replace a directory at the start of the ``path+file://`` URLs of a text.

    Args:
        text: The text.
        old: The directory, without a trailing separator.
        new: The directory to put in its place.

    Returns:
        The text, and the number of URLs changed.
    """
    count = 0
    for form in dict.fromkeys([old, quote(old, safe=KEPT)]):
        pattern = re.compile(re.escape(URL + form) + r"(?=[/#])")
        text, n = pattern.subn(lambda _: URL + new, text)
        count += n
    return text, count


def record_hash(data):
    """The hash of a file, as a wheel's RECORD lists it.

    Args:
        data: The file's bytes.

    Returns:
        ``sha256=`` and the digest in URL-safe base64 without padding.
    """
    digest = base64.urlsafe_b64encode(hashlib.sha256(data).digest()).rstrip(b"=")
    return f"sha256={digest.decode()}"


def main():
    """Rewrite the SBOMs of a wheel and its RECORD."""
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("wheel", help="the .whl file, rewritten in place")
    parser.add_argument("old", metavar="FROM", help="the checkout, as Cargo names it")
    parser.add_argument("new", metavar="TO", help="what to name it, such as /src")
    args = parser.parse_args()
    old = args.old.rstrip("/")
    if not old:
        parser.error("FROM is the root directory")
    with zipfile.ZipFile(args.wheel) as archive:
        infos = archive.infolist()
        members = {info.filename: archive.read(info) for info in infos}
    record = next(n for n in members if re.fullmatch(r"[^/]+\.dist-info/RECORD", n))
    changed = {}
    for name in members:
        if re.fullmatch(r"[^/]+\.dist-info/sboms/[^/]+", name):
            text, count = remap(members[name].decode(), old, args.new)
            print(f"{name}: {count} URLs of {old} mapped to {args.new}")
            if count:
                changed[name] = members[name] = text.encode()
    if not changed:
        return 0
    rows = list(csv.reader(io.StringIO(members[record].decode())))
    for row in rows:
        if row and row[0] in changed:
            row[1:3] = [record_hash(changed[row[0]]), str(len(changed[row[0]]))]
    text = io.StringIO()
    csv.writer(text, lineterminator="\n").writerows(rows)
    members[record] = text.getvalue().encode()
    temporary = f"{args.wheel}.part"
    with zipfile.ZipFile(temporary, "w") as archive:
        for info in infos:
            archive.writestr(info, members[info.filename])
    os.replace(temporary, args.wheel)
    return 0


if __name__ == "__main__":
    sys.exit(main())

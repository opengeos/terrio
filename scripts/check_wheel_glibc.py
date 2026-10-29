"""Check the glibc a Linux wheel needs, by its platform tags and by its shared objects.

Usage: python scripts/check_wheel_glibc.py WHEEL [--max VERSION]

A Linux wheel runs where the C library is at least as recent as two things, which must agree:

- its platform tags (PEP 600): ``manylinux_X_Y`` promises glibc X.Y or later, and installers
  refuse the wheel on an older one. ``manylinux2014`` is 2.17, ``manylinux2010`` 2.12 and
  ``manylinux1`` 2.5;
- the ``GLIBC_X.Y`` symbol versions its ELF objects require (their version needs, which
  ``readelf -V`` lists): the dynamic loader refuses to load an object when the C library lacks
  one of them ("version `GLIBC_2.39' not found"). ``GLIBC_ABI_DT_RELR`` counts as 2.36, the
  release that added it.

The script reads both with the standard library alone (no readelf, objdump or pyelftools), the
version needs as the loader reads them: from the dynamic segment, whose addresses are mapped to
the file through the loaded segments, every need followed along its chain. The section headers,
which the loader ignores, are read only to cross-check. It prints the tags, each object's highest
version with the symbols that need it, and the highest version of all. It exits 1 when that is
above ``--max``, when an object needs a later glibc than the oldest the tags promise, when an
object needs a ``GLIBC_`` version that is not a release (``GLIBC_PRIVATE``), when an object's
needs cannot be established (no dynamic segment, an address outside the loaded segments,
imported symbols but no version needs), or when its counts or section headers disagree with what
it needs. Without ``--max`` it only reports. Only glibc is checked: the libraries a wheel may
link and the libgcc and libstdc++ symbol versions are auditwheel's checks.
"""

import argparse
import re
import struct
import sys
import zipfile
from pathlib import Path

SHT_DYNSYM = 11
SHT_GNU_VERNEED = 0x6FFFFFFE
SHT_GNU_VERSYM = 0x6FFFFFFF
ET_EXEC = 2
PT_LOAD = 1
PT_DYNAMIC = 2
# The dynamic tags read: the string table and its size, the symbol table and its entry size, the
# two hash tables (which give the number of symbols), the symbols' versions and the version needs.
DT_NULL = 0
DT_HASH = 4
DT_STRTAB = 5
DT_SYMTAB = 6
DT_STRSZ = 10
DT_SYMENT = 11
DT_GNU_HASH = 0x6FFFFEF5
DT_VERSYM = 0x6FFFFFF0
DT_VERNEED = 0x6FFFFFFE
DT_VERNEEDNUM = 0x6FFFFFFF
TAGS = {DT_HASH, DT_STRTAB, DT_SYMTAB, DT_STRSZ, DT_SYMENT, DT_GNU_HASH, DT_VERSYM, DT_VERNEED}
TAGS.add(DT_VERNEEDNUM)
# The most version needs, and symbols, read of one object (a real one has dozens and thousands).
MOST = 1_000_000
# The legacy manylinux tags and the glibc version each promises (PEP 600).
LEGACY = {"manylinux1": (2, 5), "manylinux2010": (2, 12), "manylinux2014": (2, 17)}
# glibc's version names that are not release numbers, and the release that introduced each.
MARKERS = {"GLIBC_ABI_DT_RELR": (2, 36)}
RELEASE = re.compile(r"GLIBC_(\d+(?:\.\d+)+)")


def parse_version(text):
    """Parse a dotted version.

    Args:
        text: The version, such as ``2.28`` or ``2.2.5``.

    Returns:
        A tuple of its numbers.
    """
    return tuple(int(part) for part in text.split("."))


def show(version):
    """Format a version tuple.

    Args:
        version: A tuple of numbers, or None.

    Returns:
        The dotted version, or ``none``.
    """
    return ".".join(map(str, version)) if version else "none"


def tag_glibc(tag):
    """The glibc version a platform tag promises.

    Args:
        tag: One platform tag, such as ``manylinux_2_28_x86_64``.

    Returns:
        A version tuple, or None when the tag is not a manylinux tag.
    """
    m = re.fullmatch(r"manylinux_(\d+)_(\d+)_\w+", tag)
    if m:
        return int(m.group(1)), int(m.group(2))
    return LEGACY.get(tag.split("_", 1)[0])


def platform_tags(wheel, names):
    """The platform tags of a wheel, from its file name and from its WHEEL metadata.

    Args:
        wheel: The wheel's path.
        names: The wheel's member names.

    Returns:
        The set of platform tags of the file name (which installers match) and the set of the
        ``Tag:`` lines of ``.dist-info/WHEEL``.
    """
    with zipfile.ZipFile(wheel) as archive:
        meta = next((n for n in names if re.fullmatch(r"[^/]+\.dist-info/WHEEL", n)), None)
        text = archive.read(meta).decode() if meta else ""
    from_name = set(Path(wheel).name.removesuffix(".whl").split("-")[-1].split("."))
    from_meta = set()
    for line in text.splitlines():
        if line.startswith("Tag:"):
            from_meta.add(line.split(":", 1)[1].strip().split("-", 2)[-1])
    return from_name, from_meta


def elf_class(data):
    """The byte order and the word size of an ELF object.

    Args:
        data: The object's bytes.

    Returns:
        The struct byte order (``<`` or ``>``) and whether the object is 64-bit.

    Raises:
        ValueError: When the object is not a 32- or 64-bit ELF object of a known byte order.
    """
    if data[4] not in (1, 2) or data[5] not in (1, 2):
        raise ValueError("not a 32- or 64-bit ELF object of a known byte order")
    return ("<" if data[5] == 1 else ">"), data[4] == 2


def glibc_needs(data):
    """The glibc symbol versions an ELF object requires, as the dynamic loader reads them.

    The loader reads the dynamic segment (``PT_DYNAMIC``), and the addresses it gives
    (``DT_VERNEED``, ``DT_VERSYM``, ``DT_STRTAB``, ``DT_SYMTAB``) in the loaded segments
    (``PT_LOAD``), whose file offsets this maps them to. It follows the version needs along their
    chains (``vn_next``, ``vna_next``) and requires every version they name, whatever their counts
    (``vn_cnt``, ``DT_VERNEEDNUM``) say. This reads them so, and reports a count its chain
    disagrees with. The number of dynamic symbols is that of the hash tables.

    Args:
        data: The object's bytes.

    Returns:
        A dict from each ``GLIBC_`` version name in the object's version needs to the sorted
        names of the dynamic symbols it imports at that version, and the list of the counts its
        chains disagree with.

    Raises:
        ValueError: When the object's needs cannot be established: it is not an ELF object this
            reads; it has no dynamic segment (a statically linked executable needs nothing); an
            address it gives is outside the file bytes of its loaded segments; its symbols' number
            is unknown or its hash tables disagree on it; it imports symbols but has no version
            needs.
    """
    end, wide = elf_class(data)
    if wide:
        (phoff,) = struct.unpack_from(end + "Q", data, 0x20)
        phentsize, phnum = struct.unpack_from(end + "HH", data, 0x36)
        header, symbol = end + "IIQQQQQQ", 24
    else:
        (phoff,) = struct.unpack_from(end + "I", data, 0x1C)
        phentsize, phnum = struct.unpack_from(end + "HH", data, 0x2A)
        header, symbol = end + "IIIIIIII", 16
    if not phoff or not phnum:
        raise ValueError("no program headers")
    if phentsize != struct.calcsize(header) or phnum == 0xFFFF:
        raise ValueError(f"{phnum} program headers of {phentsize} bytes")
    # Each segment: type, offset, address, size in the file.
    segments = []
    for i in range(phnum):
        h = struct.unpack_from(header, data, phoff + i * phentsize)
        segments.append((h[0], h[2], h[3], h[5]) if wide else (h[0], h[1], h[2], h[4]))
    loaded = [s[1:] for s in segments if s[0] == PT_LOAD]
    dynamic = [s[1:] for s in segments if s[0] == PT_DYNAMIC]
    if not dynamic:
        (kind,) = struct.unpack_from(end + "H", data, 0x10)
        if kind == ET_EXEC:
            return {}, []
        raise ValueError("no dynamic segment (PT_DYNAMIC)")
    if len(dynamic) > 1:
        raise ValueError("more than one dynamic segment")

    def at(address, size):
        """The file offset of the ``size`` bytes at ``address``, in a loaded segment."""
        for offset, start, length in loaded:
            if start <= address and address + size <= start + length:
                if offset + address - start + size > len(data):
                    raise ValueError(f"address {address:#x} is past the end of the file")
                return offset + address - start
        raise ValueError(f"{size} bytes at address {address:#x} are outside the loaded segments")

    def words(address, count):
        return struct.unpack_from(f"{end}{count}I", data, at(address, 4 * count))

    # The dynamic array, to its DT_NULL entry, where the loader reads it.
    entry = end + ("qQ" if wide else "iI")
    step = struct.calcsize(entry)
    offset, address, _ = dynamic[0]
    if at(address, step) != offset:
        raise ValueError("the dynamic segment's offset is not that of its address")
    tags = {}
    for i in range(MOST):
        tag, value = struct.unpack_from(entry, data, at(address + i * step, step))
        if tag == DT_NULL:
            break
        if tag in TAGS:
            if tag in tags:
                raise ValueError(f"the dynamic segment gives tag {tag:#x} twice")
            tags[tag] = value

    def string(offset):
        if DT_STRTAB not in tags or DT_STRSZ not in tags or offset >= tags[DT_STRSZ]:
            raise ValueError(f"string {offset} is outside the dynamic string table")
        start = at(tags[DT_STRTAB], tags[DT_STRSZ])
        stop = data.find(b"\0", start + offset, start + tags[DT_STRSZ])
        if stop < 0:
            raise ValueError(f"string {offset} of the dynamic string table is not terminated")
        return data[start + offset : stop].decode()

    versions = {}  # version index -> name, for the versions needed from any library
    disagreements = []
    if DT_VERNEED in tags:
        need, libraries = tags[DT_VERNEED], 0
        while libraries < MOST:
            version, count, library, aux, following = struct.unpack_from(
                end + "HHIII", data, at(need, 16)
            )
            if version != 1:
                raise ValueError(f"a version need of revision {version}, not 1")
            link, chained = need + aux, 0
            while chained < MOST:
                _, _, index, name, onward = struct.unpack_from(end + "IHHII", data, at(link, 16))
                versions[index & 0x7FFF] = string(name)
                chained += 1
                if not onward:
                    break
                link += onward
            if chained != count:
                disagreements.append(
                    f"the needs of {string(library)} count {count} versions, their chain {chained}"
                )
            libraries += 1
            if not following:
                break
            need += following
        if tags.get(DT_VERNEEDNUM, libraries) != libraries:
            disagreements.append(
                f"DT_VERNEEDNUM counts {tags[DT_VERNEEDNUM]} libraries, the chain {libraries}"
            )
    # The number of dynamic symbols: DT_HASH's chain count, or past the last symbol of
    # DT_GNU_HASH's chains.
    counts = []
    if DT_HASH in tags:
        counts.append(words(tags[DT_HASH], 2)[1])
    if DT_GNU_HASH in tags:
        buckets, first, blooms, _ = words(tags[DT_GNU_HASH], 4)
        table = tags[DT_GNU_HASH] + 16 + blooms * (8 if wide else 4)
        last = max(words(table, buckets)) if buckets else 0
        if not last or last < first:
            counts.append(first)
        else:
            chain = table + 4 * buckets + 4 * (last - first)
            while last < MOST and not words(chain, 1)[0] & 1:
                last, chain = last + 1, chain + 4
            counts.append(last + 1)
    if len(set(counts)) > 1:
        raise ValueError(f"the hash tables count {counts[0]} and {counts[1]} dynamic symbols")
    symbols = counts[0] if counts else 0
    if DT_SYMTAB in tags:
        if not counts:
            raise ValueError("dynamic symbols but no hash table giving their number")
        if tags.get(DT_SYMENT) != symbol:
            raise ValueError(f"dynamic symbols of {tags.get(DT_SYMENT)} bytes, not {symbol}")
    elif symbols:
        raise ValueError("a hash table but no dynamic symbols (DT_SYMTAB)")
    needs = {name: [] for name in versions.values() if name.startswith("GLIBC_")}
    imports = False
    for i in range(1, symbols):
        where = tags[DT_SYMTAB] + i * symbol
        (section,) = struct.unpack_from(end + "H", data, at(where + (6 if wide else 14), 2))
        imports = imports or section == 0
        if DT_VERSYM in tags:
            (index,) = struct.unpack_from(end + "H", data, at(tags[DT_VERSYM] + 2 * i, 2))
            name = versions.get(index & 0x7FFF)
            if name in needs:
                needs[name].append(string(words(where, 1)[0]))
    # Imports without version needs: the versions were stripped, or are not where the loader
    # reads them, so what the object requires is unknown.
    if imports and DT_VERNEED not in tags:
        raise ValueError("imported symbols but no version needs (DT_VERNEED)")
    return {name: sorted(symbols) for name, symbols in needs.items()}, disagreements


def listed_needs(data):
    """The glibc symbol versions an ELF object's section headers list, to cross-check.

    Args:
        data: The object's bytes.

    Returns:
        A dict from each ``GLIBC_`` version name to the sorted names of the dynamic symbols at it,
        as :func:`glibc_needs` gives one, read from the sections ``.gnu.version_r``, ``.dynsym``
        and ``.gnu.version`` by their counts; None when the object has no section headers.

    Raises:
        ValueError: When the section headers cannot be read.
    """
    end, wide = elf_class(data)
    if wide:
        (shoff,) = struct.unpack_from(end + "Q", data, 0x28)
        shentsize, shnum = struct.unpack_from(end + "HH", data, 0x3A)
        layout = end + "IIQQQQIIQQ"
    else:
        (shoff,) = struct.unpack_from(end + "I", data, 0x20)
        shentsize, shnum = struct.unpack_from(end + "HH", data, 0x2E)
        layout = end + "IIIIIIIIII"
    if not shoff:
        return None
    if shentsize != struct.calcsize(layout):
        raise ValueError(f"section headers of {shentsize} bytes, not {struct.calcsize(layout)}")
    # Each section: name, type, flags, addr, offset, size, link, info, addralign, entsize. With
    # more sections than the ELF header's field holds, it is 0 and section 0 holds the count.
    shnum = shnum or struct.unpack_from(layout, data, shoff)[5]
    sections = [struct.unpack_from(layout, data, shoff + i * shentsize) for i in range(shnum)]

    def string(table, offset):
        start = sections[table][4] + offset
        return data[start : data.index(b"\0", start)].decode()

    versions = {}  # version index -> name, for the versions needed from any library
    for sec in sections:
        if sec[1] != SHT_GNU_VERNEED:
            continue
        entry = sec[4]
        for _ in range(sec[7]):
            _, count, _, aux, following = struct.unpack_from(end + "HHIII", data, entry)
            at = entry + aux
            for _ in range(count):
                _, _, index, name, step = struct.unpack_from(end + "IHHII", data, at)
                versions[index & 0x7FFF] = string(sec[6], name)
                at += step
            if not following:
                break
            entry += following
    needs = {name: [] for name in versions.values() if name.startswith("GLIBC_")}
    dynsym = next((s for s in sections if s[1] == SHT_DYNSYM), None)
    versym = next((s for s in sections if s[1] == SHT_GNU_VERSYM), None)
    if dynsym and versym and dynsym[9]:
        for i in range(dynsym[5] // dynsym[9]):
            (index,) = struct.unpack_from(end + "H", data, versym[4] + 2 * i)
            name = versions.get(index & 0x7FFF)
            if name in needs:
                (symbol,) = struct.unpack_from(end + "I", data, dynsym[4] + i * dynsym[9])
                needs[name].append(string(dynsym[6], symbol))
    return {name: sorted(symbols) for name, symbols in needs.items()}


def version_of(name):
    """The glibc release a version name needs.

    Args:
        name: A version name, such as ``GLIBC_2.28``.

    Returns:
        A version tuple, or None when the name is not a release (``GLIBC_PRIVATE``).
    """
    m = RELEASE.fullmatch(name)
    return parse_version(m.group(1)) if m else MARKERS.get(name)


def main():
    """Report a wheel's glibc requirements and exit 1 when they exceed ``--max``."""
    parser = argparse.ArgumentParser(description=__doc__.split("\n", 1)[0])
    parser.add_argument("wheel", help="the .whl file")
    parser.add_argument("--max", help="the latest glibc the wheel may need, such as 2.28")
    args = parser.parse_args()
    limit = parse_version(args.max) if args.max else None
    problems = []
    with zipfile.ZipFile(args.wheel) as archive:
        names = archive.namelist()
        from_name, from_meta = platform_tags(args.wheel, names)
        objects = {}
        for name in names:
            with archive.open(name) as member:
                if member.read(4) != b"\x7fELF":
                    continue
            data = archive.read(name)
            try:
                objects[name], disagreements = glibc_needs(data)
            except (ValueError, IndexError, struct.error) as e:
                problems.append(f"{name}: its glibc needs cannot be established ({e})")
                continue
            problems.extend(f"{name}: {d}" for d in disagreements)
            # The section headers, which the loader ignores, must list the same needs.
            try:
                listed = listed_needs(data)
            except (ValueError, IndexError, struct.error) as e:
                problems.append(f"{name}: its section headers cannot be read to cross-check ({e})")
                continue
            if listed is not None and listed != objects[name]:
                versions = {*listed, *objects[name]}
                differ = [v for v in versions if listed.get(v) != objects[name].get(v)]
                differ.sort(key=lambda v: (version_of(v) or (), v))
                problems.append(
                    f"{name}: its section headers disagree with its dynamic segment on "
                    + ", ".join(differ)
                )
    print(f"wheel: {Path(args.wheel).name}")
    promised = [v for v in map(tag_glibc, sorted(from_name)) if v]
    tag_floor = min(promised) if promised else None
    print(f"platform tags: {' '.join(sorted(from_name))} (glibc {show(tag_floor)})")
    if from_meta and from_meta != from_name:
        problems.append(f"the WHEEL metadata's tags {sorted(from_meta)} are not the file name's")
    highest = None
    for name, needs in sorted(objects.items()):
        known = {}
        for version, symbols in needs.items():
            release = version_of(version)
            if release is None:
                problems.append(f"{name} needs {version}, which is not a glibc release")
            else:
                known[version] = (release, symbols)
        if not known:
            print(f"{name}: no glibc symbol versions")
            continue
        top = max(release for release, _ in known.values())
        highest = max(highest, top) if highest else top
        listed = ", ".join(v for v, (r, _) in sorted(known.items(), key=lambda kv: kv[1][0]))
        print(f"{name}: needs {listed}")
        over = {v: s for v, (r, s) in known.items() if r == top or (limit and r > limit)}
        for version, symbols in sorted(over.items(), key=lambda kv: version_of(kv[0])):
            print(f"  {version}: {', '.join(symbols) or '(no symbol: a version marker)'}")
        if tag_floor and top > tag_floor:
            problems.append(f"{name} needs glibc {show(top)}, the tags promise {show(tag_floor)}")
    if not objects:
        print("no ELF objects")
    worst = max(v for v in (highest, tag_floor, (0,)) if v)
    print(f"highest glibc needed: {show(worst) if worst != (0,) else 'none'}", end="")
    print(f" (tags {show(tag_floor)}, objects {show(highest)})")
    if limit and worst > limit:
        problems.append(f"glibc {show(worst)} is above the maximum, {show(limit)}")
    for problem in problems:
        print(f"FAIL: {problem}")
    if limit and not problems:
        print(f"OK: at most glibc {show(limit)}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())

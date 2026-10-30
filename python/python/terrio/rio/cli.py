# SPDX-FileCopyrightText: Copyright (c) 2013-2021, Mapbox
# SPDX-FileCopyrightText: Copyright 2022 Planet Labs PBC
# SPDX-FileCopyrightText: Copyright (c) 2014 Mapbox
# SPDX-FileCopyrightText: Terrio contributors
# SPDX-License-Identifier: MIT AND BSD-3-Clause AND Apache-2.0
#
# Derived from (translated to Python and modified):
#   rasterio 1.5.2:
#     rasterio/rio/
#   rasterio (vendored snuggs) 1.5.2:
#     rasterio/_vendor/snuggs.py

"""A `rio` command line on top of terrio.rio: ``terrio-rio info dem.tif``.

The commands, options, defaults and outputs are those of rasterio's ``rio`` command line
(``blocks``, ``bounds``, ``calc``, ``clip``, ``convert``, ``create``, ``edit-info``, ``env``,
``gcps``, ``info``, ``insp``, ``mask``, ``merge``, ``overview``, ``rasterize``, ``rm``,
``sample``, ``shapes``, ``stack``, ``transform``, ``warp``), run with terrio.rio instead of
rasterio and GDAL, and parsed with :mod:`argparse` instead of click. The console script is
``terrio-rio`` (so that it does not shadow rasterio's ``rio``); ``python -m terrio.rio`` runs
the same command.

The commands follow rasterio's ``rasterio.rio`` modules (Copyright (c) 2013-2021, Mapbox;
BSD-3-Clause), and ``calc`` evaluates snuggs expressions (Copyright (c) 2014 Mapbox, MIT) with
its own parser.
"""

from __future__ import annotations

import argparse
import builtins
import json
import logging
import math
import os
import re
import sys
from collections import OrderedDict
from collections.abc import Callable, Iterator, Sequence
from contextlib import ExitStack
from itertools import zip_longest
from typing import IO, Any, NamedTuple

logger = logging.getLogger("terrio.rio.cli")

PROG = "terrio-rio"

# Data types of the `-t/--dtype` options, as rasterio lists them.
DTYPES = [
    "ubyte",
    "uint8",
    "uint16",
    "uint32",
    "uint64",
    "int8",
    "int16",
    "int32",
    "int64",
    "float16",
    "float32",
    "float64",
]


class Stats(NamedTuple):
    """The minimum, maximum and mean of `rio insp`'s `stats`."""

    min: Any
    max: Any
    mean: Any


class UsageError(Exception):
    """A command line misuse: reported with the command's usage, exit status 2."""


class CommandError(Exception):
    """A command's failure: reported as an error, exit status 1."""


# ─── Option values ──────────────────────────────────────────────────────────────────────────────


def key_val(values: Sequence[str] | None) -> dict[str, Any]:
    """Collects `--co KEY=VAL` options in a dictionary, as rasterio does (lower-cased).

    Args:
        values: The `KEY=VAL` strings.

    Returns:
        dict: Lower-cased keys and values; `none`, `null`, `nil` and `nada` become None.

    Raises:
        UsageError: If a value has no `=`.
    """
    out: dict[str, Any] = {}
    for pair in values or []:
        if "=" not in pair:
            raise UsageError(f"Invalid syntax for KEY=VAL arg: {pair}")
        k, v = pair.split("=", 1)
        k, v = k.lower(), v.lower()
        out[k] = None if v in ("none", "null", "nil", "nada") else v
    return out


def parse_nodata(value: str | None) -> float | None:
    """A `--nodata` value: a number, `nan`, or nothing.

    Args:
        value: The option's text.

    Returns:
        float | None: The value; None for `null`, `nil`, `none` and `nada`.

    Raises:
        UsageError: If the value is not a number.
    """
    if value is None or value.lower() in ("null", "nil", "none", "nada"):
        return None
    try:
        return float(value)
    except (TypeError, ValueError):
        raise UsageError(f"Invalid value for '--nodata': {value!r} is not a number") from None


def parse_bounds(value: str | None) -> tuple[float, ...] | None:
    """A `--bounds` value: `left bottom right top` or `[left, bottom, right, top]`.

    Args:
        value: The option's text.

    Returns:
        tuple | None: The four numbers.

    Raises:
        UsageError: If the value is not four numbers.
    """
    if value is None:
        return None
    try:
        text = value.strip(", []")
        out = tuple(float(x) for x in re.split(r"[,\s]+", text))
        if len(out) != 4:
            raise ValueError(out)
        return out
    except Exception:
        raise UsageError(
            f"Invalid value for '--bounds': {value!r} is not a valid bounding box representation"
        ) from None


def is_remote(path: str) -> bool:
    """Whether a path names a remote or virtual dataset rather than a local file.

    Args:
        path: The path.

    Returns:
        bool: True for URLs and GDAL `/vsi` paths.
    """
    return bool(re.match(r"^[a-zA-Z][a-zA-Z0-9+.-]*://", path)) or path.startswith("/vsi")


def file_in(value: str) -> str:
    """Normalizes an input file as rasterio's `rio` does: local datasets get absolute paths.

    Other values (URLs, missing files) are kept: opening them reports the error.

    Args:
        value: The path or URL.

    Returns:
        str: The path, absolute with forward slashes, or the value unchanged.
    """
    import terrio.rio.shutil as rio_shutil

    if is_remote(value):
        return value
    if os.path.exists(value) and rio_shutil.exists(value):
        return "/".join(os.path.abspath(value).split(os.path.sep))
    return value


def resolve_inout(
    input: str | None = None,
    output: str | None = None,
    files: Sequence[str] | None = None,
    overwrite: bool = False,
    num_inputs: int | None = None,
) -> tuple[str | None, list[str]]:
    """Resolves inputs and output from positional arguments and `-o`, as rasterio does.

    Args:
        input: A single input.
        output: The `-o/--output` value.
        files: Positional files, the last of which is the output unless `-o` is given.
        overwrite: Overwrite an existing output.
        num_inputs: The number of inputs required.

    Returns:
        tuple: The output and the inputs.

    Raises:
        CommandError: If the output exists and `overwrite` is False.
        UsageError: If there are too few or too many inputs.
    """
    resolved_output = output or (files[-1] if files else None)
    if not overwrite and resolved_output and os.path.exists(resolved_output):
        raise CommandError(
            "file exists and won't be overwritten without use of the `--overwrite` option."
        )
    resolved_inputs = ([input] if input else []) + (
        list(files[: -1 if not output else None]) if files else []
    )
    if num_inputs is not None:
        if len(resolved_inputs) < num_inputs:
            raise UsageError("Insufficient inputs")
        if len(resolved_inputs) > num_inputs:
            raise UsageError("Too many inputs")
    return resolved_output, resolved_inputs


def open_text(name: str) -> IO[str]:
    """Opens a text file for reading, `-` meaning standard input.

    Args:
        name: The path, or `-`.

    Returns:
        file: The open file.
    """
    if name == "-":
        return sys.stdin
    return open(name, encoding="utf-8")


def lines_of(name: str) -> list[str]:
    """The lines of a file, of standard input (`-`), or the text itself if it is not a file.

    Args:
        name: A path, `-`, or text.

    Returns:
        list: The lines.
    """
    try:
        with ExitStack() as stack:
            f = open_text(name)
            if f is not sys.stdin:
                stack.enter_context(f)
            return f.readlines()
    except OSError:
        return [name]


def echo(value: Any = "", out: IO[str] | None = None) -> None:
    """Prints a value as click.echo does: None as an empty line.

    Args:
        value: The value.
        out: The stream (standard output by default).
    """
    print("" if value is None else value, file=out or sys.stdout)


def to_json(obj: Any) -> Any:
    """Converts what `json` cannot serialize: affine transforms and NumPy arrays and scalars.

    Args:
        obj: The object.

    Returns:
        A JSON-serializable form: a transform's 9 coefficients, an array's list.

    Raises:
        TypeError: For other objects.
    """
    import numpy as np
    from affine import Affine

    if isinstance(obj, Affine):
        return tuple(obj)
    if isinstance(obj, (np.ndarray, np.generic)):
        return obj.tolist()
    raise TypeError(f"Object of type {type(obj).__name__} is not JSON serializable")


def dump_kwds(indent: int | None, compact: bool) -> dict[str, Any]:
    """`json.dumps` keywords for the `--indent` and `--compact` options.

    Args:
        indent: The indentation.
        compact: Use compact separators.

    Returns:
        dict: The keywords, with sorted keys.
    """
    kw: dict[str, Any] = {"sort_keys": True}
    if indent:
        kw["indent"] = indent
    if compact:
        kw["separators"] = (",", ":")
    return kw


def coords(obj: Any) -> Iterator[tuple[float, ...]]:
    """The coordinate tuples of a geometry or feature.

    Args:
        obj: A GeoJSON geometry, feature or coordinate array.

    Yields:
        tuple: Coordinates.
    """
    if isinstance(obj, (tuple, list)):
        coordinates = obj
    elif "geometry" in obj:
        coordinates = obj["geometry"]["coordinates"]
    else:
        coordinates = obj.get("coordinates", obj)
    for e in coordinates:
        if isinstance(e, (float, int)):
            yield tuple(coordinates)
            break
        yield from coords(e)


class FeatureSource:
    """Features made on demand, with the bounding box `write_features` writes for them all."""

    def __init__(
        self,
        features: Callable[[], Iterator[dict[str, Any]]],
        bbox: Callable[[], Sequence[float]],
    ) -> None:
        """Creates the source.

        Args:
            features: A function yielding the features.
            bbox: A function giving the bounding box (once the features are made).
        """
        self._features = features
        self._bbox = bbox

    def __call__(self) -> Iterator[dict[str, Any]]:
        """The features.

        Returns:
            iterator: The features.
        """
        return self._features()

    @property
    def bbox(self) -> Sequence[float]:
        """The bounding box of the features.

        Returns:
            sequence: `(left, bottom, right, top)`.
        """
        return self._bbox()


def write_features(
    fobj: IO[str],
    collection: Any,
    sequence: bool = False,
    geojson_type: str = "feature",
    use_rs: bool = False,
    **kw: Any,
) -> None:
    """Writes features as a sequence or a collection, as `rio` does.

    Args:
        fobj: The output stream.
        collection: A callable yielding features, with a `bbox` property.
        sequence: Write a sequence of texts (else a single collection or bbox).
        geojson_type: `feature`, `bbox` or `collection`.
        use_rs: Prefix the texts of a sequence with RS (0x1E).
        **kw: `json.dumps` keywords.
    """
    if sequence:
        for feat in collection():
            points = list(coords(feat))
            if not points:
                raise CommandError(f"a feature without coordinates: {feat.get('id', feat)}")
            xs, ys = list(zip(*points, strict=False))[:2]
            bbox = (min(xs), min(ys), max(xs), max(ys))
            if use_rs:
                fobj.write("\u001e")
            if geojson_type == "bbox":
                fobj.write(json.dumps(bbox, **kw))
            else:
                fobj.write(json.dumps(feat, **kw))
            fobj.write("\n")
    else:
        features = list(collection())
        if geojson_type == "bbox":
            fobj.write(json.dumps(collection.bbox, **kw))
        else:
            fobj.write(
                json.dumps(
                    {"bbox": collection.bbox, "type": "FeatureCollection", "features": features},
                    **kw,
                )
            )
        fobj.write("\n")


def geojson_type(args: argparse.Namespace, allowed: Sequence[str], default: str) -> str:
    """The GeoJSON output mode of `--feature`, `--bbox` and `--collection` flags.

    Args:
        args: The parsed arguments.
        allowed: The flags the command has.
        default: The mode when none is given.

    Returns:
        str: The mode.

    Raises:
        UsageError: If more than one flag is given.
    """
    specified = [name for name in allowed if getattr(args, f"geojson_type_{name}", False)]
    if len(specified) > 1:
        names = ",".join(f"--{name}" for name in specified)
        raise UsageError(f"too many GeoJSON output formats: {names} may not be specified together")
    return specified[0] if specified else default


def parse_indexes(item: str, src_indexes: Sequence[int]) -> list[int]:
    """Band indexes of `N`, `M,N,O`, `M..O`, `..N` and `N..`.

    Args:
        item: The text.
        src_indexes: The dataset's band indexes.

    Returns:
        list: The indexes.
    """
    if ".." in item:
        start, stop = (int(x) if x else None for x in item.split(".."))
        if start is None:
            start = 1
        if start < 1 or (stop is not None and stop < 1):
            raise UsageError(f"Invalid band index range '{item}': bands are numbered from 1")
        return list(src_indexes[slice(start - 1, stop)])
    return [int(x) for x in item.split(",")]


# ─── Option builders ────────────────────────────────────────────────────────────────────────────


def flag_pair(
    p: argparse.ArgumentParser, on: str, off: str, dest: str, default: bool, help: str
) -> None:
    """Adds a click-style `--on/--off` boolean pair.

    Args:
        p: The parser.
        on: The option setting True.
        off: The option setting False.
        dest: The attribute.
        default: The default.
        help: The help text.
    """
    p.add_argument(on, dest=dest, action="store_true", help=help)
    p.add_argument(off, dest=dest, action="store_false", help=argparse.SUPPRESS)
    p.set_defaults(**{dest: default})


def opt_precision(p: argparse.ArgumentParser) -> None:
    """Adds `--precision`.

    Args:
        p: The parser.
    """
    p.add_argument("--precision", type=int, default=-1, help="Decimal precision of coordinates.")


def opt_indent(p: argparse.ArgumentParser) -> None:
    """Adds `--indent` and `--compact/--not-compact`.

    Args:
        p: The parser.
    """
    p.add_argument("--indent", type=int, default=None, help="Indentation level for JSON output")
    flag_pair(
        p, "--compact", "--not-compact", "compact", False, "Use compact separators (',', ':')."
    )


def opt_rs(p: argparse.ArgumentParser) -> None:
    """Adds `--rs/--no-rs`.

    Args:
        p: The parser.
    """
    flag_pair(
        p,
        "--rs",
        "--no-rs",
        "use_rs",
        False,
        "Use RS (0x1E) as a prefix for individual texts in a sequence.",
    )


def opt_projection(p: argparse.ArgumentParser, names: Sequence[str], default: str | None) -> None:
    """Adds `--geographic`, `--projected` and `--mercator` switches.

    Args:
        p: The parser.
        names: The switches.
        default: The default projection.
    """
    helps = {
        "geographic": "Output in geographic coordinates.",
        "projected": "Output in dataset's own, projected coordinates.",
        "mercator": "Output in Web Mercator coordinates.",
    }
    for name in names:
        p.add_argument(
            f"--{name}", dest="projection", action="store_const", const=name, help=helps[name]
        )
    p.set_defaults(projection=default)


def opt_geojson_type(p: argparse.ArgumentParser, allowed: Sequence[str], default: str) -> None:
    """Adds GeoJSON output mode flags.

    Args:
        p: The parser.
        allowed: `collection`, `feature` and `bbox` flags to add.
        default: The default mode.
    """
    helps = {
        "collection": "feature collection(s)",
        "feature": "feature(s)",
        "bbox": "bounding box array(s)",
    }
    for name in allowed:
        suffix = " (default)" if name == default else ""
        p.add_argument(
            f"--{name}",
            dest=f"geojson_type_{name}",
            action="store_true",
            help=f"Output as GeoJSON {helps[name]}{suffix}",
        )


def opt_output(p: argparse.ArgumentParser) -> None:
    """Adds `-o/--output`.

    Args:
        p: The parser.
    """
    p.add_argument(
        "-o",
        "--output",
        default=None,
        help="Path to output file (optional alternative to a positional arg).",
    )


def opt_format(p: argparse.ArgumentParser) -> None:
    """Adds `-f/--format/--driver`.

    Args:
        p: The parser.
    """
    p.add_argument(
        "-f", "--format", "--driver", dest="driver", default=None, help="Output format driver."
    )


def opt_dtype(p: argparse.ArgumentParser) -> None:
    """Adds `-t/--dtype`.

    Args:
        p: The parser.
    """
    p.add_argument("-t", "--dtype", choices=DTYPES, default=None, help="Output data type.")


def opt_overwrite(p: argparse.ArgumentParser) -> None:
    """Adds `--overwrite`.

    Args:
        p: The parser.
    """
    p.add_argument(
        "--overwrite", action="store_true", help="Always overwrite an existing output file."
    )


def opt_co(p: argparse.ArgumentParser) -> None:
    """Adds `--co/--profile NAME=VALUE`.

    Args:
        p: The parser.
    """
    p.add_argument(
        "--co",
        "--profile",
        dest="creation_options",
        metavar="NAME=VALUE",
        action="append",
        default=[],
        help="Driver specific creation options.",
    )


def opt_res(p: argparse.ArgumentParser) -> None:
    """Adds `-r/--res`, given once (square pixels) or twice.

    Args:
        p: The parser.
    """
    p.add_argument(
        "-r",
        "--res",
        type=float,
        action="append",
        default=[],
        help="Output dataset resolution in units of coordinate reference system.",
    )


def opt_nodata(p: argparse.ArgumentParser) -> None:
    """Adds `--nodata NUMBER|nan`.

    Args:
        p: The parser.
    """
    p.add_argument("--nodata", default=None, metavar="NUMBER|nan", help="Set a Nodata value.")


def opt_bounds(p: argparse.ArgumentParser) -> None:
    """Adds `--bounds 'left bottom right top'`.

    Args:
        p: The parser.
    """
    p.add_argument(
        "--bounds",
        default=None,
        help="Bounds: 'left bottom right top' or '[left, bottom, right, top]'.",
    )


def opt_all_touched(p: argparse.ArgumentParser) -> None:
    """Adds `-a/--all/--all_touched`.

    Args:
        p: The parser.
    """
    p.add_argument(
        "-a",
        "--all",
        "--all_touched",
        dest="all_touched",
        action="store_true",
        help="Use all pixels touched by features.",
    )


def resampling_names(values: Any) -> list[str]:
    """The names of resampling methods.

    Args:
        values: Enum members.

    Returns:
        list: Their names.
    """
    return [r.name for r in values]


# ─── Commands ───────────────────────────────────────────────────────────────────────────────────


def cmd_info(args: argparse.Namespace, env: Any) -> None:
    """`info`: prints a dataset's metadata as JSON, or one item of it.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import terrio.rio as rasterio
    from terrio.rio.transform import from_gcps

    with env, rasterio.open(file_in(args.input)) as src:
        info: dict[str, Any] = dict(src.profile)
        info["shape"] = (info["height"], info["width"])
        info["bounds"] = src.bounds
        if src.crs:
            epsg = src.crs.to_epsg()
            info["crs"] = f"EPSG:{epsg}" if epsg else src.crs.to_string()
        else:
            info["crs"] = None
        info["res"] = src.res
        info["colorinterp"] = [ci.name for ci in src.colorinterp]
        info["units"] = [units or None for units in src.units]
        info["descriptions"] = src.descriptions
        info["indexes"] = src.indexes
        info["mask_flags"] = [[flag.name for flag in flags] for flags in src.mask_flag_enums]
        if src.crs:
            info["lnglat"] = src.lnglat()
        gcps, gcps_crs = src.gcps
        if gcps:
            info["gcps"] = {"points": [p.asdict() for p in gcps]}
            if gcps_crs:
                epsg = gcps_crs.to_epsg()
                info["gcps"]["crs"] = f"EPSG:{epsg}" if epsg else gcps_crs.to_string()
            else:
                info["gcps"]["crs"] = None
            info["gcps"]["transform"] = from_gcps(gcps)
        if args.verbose:
            info["stats"] = [
                {"min": s.min, "max": s.max, "mean": s.mean, "std": s.std} for s in src.stats()
            ]
            info["checksum"] = [src.checksum(i) for i in src.indexes]
        member = args.meta_member
        if args.aspect == "meta":
            if member == "subdatasets":
                for name in src.subdatasets:
                    echo(name)
            elif member == "stats":
                st = src.stats(indexes=args.bidx)[0]
                echo(f"{st.min} {st.max} {st.mean} {st.std}")
            elif member == "checksum":
                echo(str(src.checksum(args.bidx)))
            elif member:
                if member not in info:
                    raise CommandError(f"no {member}: the dataset has no CRS")
                if isinstance(info[member], (list, tuple)):
                    echo(" ".join(map(str, info[member])))
                else:
                    echo(info[member])
            else:
                echo(json.dumps(info, sort_keys=True, indent=args.indent, default=to_json))
        else:
            echo(json.dumps(src.tags(ns=args.namespace), indent=args.indent, default=to_json))


def add_info(sub: Any) -> None:
    """Registers `info`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "info",
        cmd_info,
        "Print information about a data file.",
        "Print metadata about the dataset as JSON.\n\nOptionally print a single metadata item as a "
        "string.",
    )
    p.add_argument("input", metavar="INPUT")
    p.add_argument(
        "--meta",
        dest="aspect",
        action="store_const",
        const="meta",
        help="Show data file structure (default).",
    )
    p.add_argument(
        "--tags", dest="aspect", action="store_const", const="tags", help="Show data file tags."
    )
    p.set_defaults(aspect="meta", meta_member=None)
    p.add_argument("--namespace", help="Select a tag namespace.")
    p.add_argument(
        "--indent", type=int, default=None, help="Indentation level for pretty printed output"
    )
    members = [
        (("--count",), "count", "Print the count of bands."),
        (("-t", "--dtype"), "dtype", "Print the dtype name."),
        (("--nodata",), "nodata", "Print the nodata value."),
        (("-f", "--format", "--driver"), "driver", "Print the format driver."),
        (("--shape",), "shape", "Print the (height, width) shape."),
        (("--height",), "height", "Print the height (number of rows)."),
        (("--width",), "width", "Print the width (number of columns)."),
        (("--crs",), "crs", "Print the CRS as a PROJ.4 string."),
        (("--bounds",), "bounds", "Print the boundary coordinates (left, bottom, right, top)."),
        (("-r", "--res"), "res", "Print pixel width and height."),
        (("--lnglat",), "lnglat", "Print longitude and latitude at center."),
        (("--stats",), "stats", "Print statistics (min, max, mean) of a single band (use --bidx)."),
        (("--checksum",), "checksum", "Print integer checksum of a single band (use --bidx)."),
        (("--subdatasets",), "subdatasets", "Print subdataset identifiers."),
    ]
    for flags, const, text in members:
        p.add_argument(*flags, dest="meta_member", action="store_const", const=const, help=text)
    p.add_argument(
        "-v",
        "--tell-me-more",
        "--verbose",
        dest="verbose",
        action="store_true",
        help="Output extra information.",
    )
    p.add_argument("-b", "--bidx", type=int, default=1, help="Input file band index (default: 1).")
    flag_pair(
        p,
        "--masked",
        "--not-masked",
        "masked",
        True,
        "Evaluate expressions using masked arrays (the default) or ordinary numpy arrays.",
    )


def cmd_bounds(args: argparse.Namespace, env: Any) -> None:
    """`bounds`: writes the bounding boxes of datasets as GeoJSON.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import terrio.rio as rasterio
    from terrio.rio.warp import transform_bounds

    kw = dump_kwds(args.indent, args.compact)
    gtype = geojson_type(args, ("feature", "bbox"), "feature")
    dst_crs = args.dst_crs.lower()
    paths = args.input
    precision = args.precision
    projection = args.projection

    xs: list[float] = []
    ys: list[float] = []

    def features() -> Iterator[dict[str, Any]]:
        """The features.

        Yields:
            dict: A feature per dataset.
        """
        for i, path in enumerate(paths):
            with rasterio.open(path) as src:
                b = src.bounds
                if dst_crs:
                    bbox = transform_bounds(src.crs, dst_crs, *b)
                elif projection == "mercator":
                    bbox = transform_bounds(src.crs, "EPSG:3857", *b)
                elif projection == "geographic":
                    bbox = transform_bounds(src.crs, "EPSG:4326", *b)
                else:
                    bbox = b
            if precision >= 0:
                bbox = tuple(round(v, precision) for v in bbox)
            yield {
                "type": "Feature",
                "bbox": bbox,
                "geometry": {
                    "type": "Polygon",
                    "coordinates": [
                        [
                            [bbox[0], bbox[1]],
                            [bbox[2], bbox[1]],
                            [bbox[2], bbox[3]],
                            [bbox[0], bbox[3]],
                            [bbox[0], bbox[1]],
                        ]
                    ],
                },
                "properties": {"id": str(i), "title": path, "filename": os.path.basename(path)},
            }
            xs.extend(bbox[::2])
            ys.extend(bbox[1::2])

    with env:
        write_features(
            sys.stdout,
            FeatureSource(features, lambda: (min(xs), min(ys), max(xs), max(ys))),
            sequence=args.sequence,
            geojson_type=gtype,
            use_rs=args.use_rs,
            **kw,
        )


def add_bounds(sub: Any) -> None:
    """Registers `bounds`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "bounds",
        cmd_bounds,
        "Write bounding boxes to stdout as GeoJSON.",
        "Write bounding boxes to stdout as GeoJSON.\n\nIf a destination crs is passed via dst_crs, "
        "it takes precedence over the projection parameter.",
    )
    p.add_argument("input", nargs="+", metavar="INPUT")
    opt_precision(p)
    opt_indent(p)
    opt_projection(p, ("geographic", "projected", "mercator"), "geographic")
    p.add_argument(
        "--dst-crs", default="", metavar="EPSG:NNNN", help="Output in specified coordinates."
    )
    flag_pair(
        p,
        "--sequence",
        "--collection",
        "sequence",
        True,
        "Write a LF-delimited sequence of texts containing individual objects (the default) or "
        "write a single JSON text containing a feature collection object.",
    )
    opt_rs(p)
    opt_geojson_type(p, ("feature", "bbox"), "feature")


def cmd_blocks(args: argparse.Namespace, env: Any) -> None:
    """`blocks`: writes a dataset's blocks as GeoJSON features.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import terrio.rio as rasterio
    from terrio.rio.warp import transform_bounds

    kw = dump_kwds(args.indent, args.compact)
    geographic = args.projection != "projected"
    precision = args.precision
    with ExitStack() as stack:
        out = stack.enter_context(open(args.output, "w")) if args.output else sys.stdout
        stack.enter_context(env)
        src = stack.enter_context(rasterio.open(file_in(args.input)))
        if args.bidx and args.bidx not in src.indexes:
            raise UsageError("Invalid value for '--bidx': Not a valid band index")

        def normalize(bounds: Sequence[float]) -> tuple[float, ...]:
            """Bounds in the output CRS and precision.

            Args:
                bounds: Bounds in the dataset's CRS.

            Returns:
                tuple: The bounds.
            """
            if geographic:
                bounds = transform_bounds(
                    src.crs, "EPSG:4326", bounds[0], bounds[1], bounds[2], bounds[3]
                )
            if precision >= 0:
                bounds = [round(v, precision) for v in bounds]
            return tuple(bounds)

        def features() -> Iterator[dict[str, Any]]:
            """The features.

            Yields:
                dict: A feature per block.
            """
            for idx, (block, window) in enumerate(src.block_windows(bidx=args.bidx)):
                xmin, ymin, xmax, ymax = normalize(src.window_bounds(window))
                yield {
                    "type": "Feature",
                    "id": f"{os.path.basename(src.name)}:{idx}",
                    "properties": {"block": json.dumps(block), "window": window.todict()},
                    "geometry": {
                        "type": "Polygon",
                        "coordinates": [[(xmin, ymin), (xmin, ymax), (xmax, ymax), (xmax, ymin)]],
                    },
                }

        write_features(
            out,
            FeatureSource(features, lambda: normalize(src.bounds)),
            sequence=args.sequence,
            geojson_type="feature" if args.sequence else "collection",
            use_rs=args.use_rs,
            **kw,
        )


def add_blocks(sub: Any) -> None:
    """Registers `blocks`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "blocks",
        cmd_blocks,
        "Write dataset blocks as GeoJSON features.",
        "Write dataset blocks as GeoJSON features.\n\nOutput features have two JSON encoded "
        "properties: block and window. Features are reprojected to WGS 84 unless --projected is "
        "given.",
    )
    p.add_argument("input", metavar="INPUT")
    opt_output(p)
    opt_precision(p)
    opt_indent(p)
    opt_projection(p, ("projected",), None)
    flag_pair(
        p,
        "--sequence",
        "--collection",
        "sequence",
        False,
        "Write a single JSON text containing a feature collection object (the default) or write "
        "a LF-delimited sequence of texts containing individual objects.",
    )
    # rasterio before 1.5.2 (cligj's option).
    p.add_argument("--no-sequence", dest="sequence", action="store_false", help=argparse.SUPPRESS)
    opt_rs(p)
    p.add_argument(
        "--bidx", type=int, default=0, help="Index of the band that is the source of shapes."
    )


def cmd_shapes(args: argparse.Namespace, env: Any) -> None:
    """`shapes`: writes the shapes of a band or mask as GeoJSON.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import terrio.rio as rasterio
    from terrio.rio.features import dataset_features

    kw = dump_kwds(args.indent, args.compact)
    gtype = geojson_type(args, ("feature", "bbox"), "feature")
    bidx = 1 if args.bandidx is None and args.band else args.bandidx
    if not args.sequence:
        gtype = "collection"
    geographic = args.projection == "geographic"
    with ExitStack() as stack:
        out = stack.enter_context(open(args.output, "w")) if args.output else sys.stdout
        stack.enter_context(env)
        src = stack.enter_context(rasterio.open(file_in(args.input)))

        bboxes: list[Sequence[float]] = []

        def bbox() -> tuple[float, float, float, float]:
            """The bounds of every shape.

            Returns:
                tuple: `(left, bottom, right, top)`.

            Raises:
                CommandError: If there is no shape.
            """
            if not bboxes:
                raise CommandError("no shapes: the band (or mask) has no valid data")
            minxs, minys, maxxs, maxys = zip(*bboxes, strict=True)
            return min(minxs), min(minys), max(maxxs), max(maxys)

        def features() -> Iterator[dict[str, Any]]:
            """The features.

            Yields:
                dict: A feature per shape.
            """
            for f in dataset_features(
                src,
                bidx,
                sampling=args.sampling,
                band=args.band,
                as_mask=args.as_mask,
                with_nodata=args.with_nodata,
                geographic=geographic,
                precision=args.precision,
            ):
                bboxes.append(f["bbox"])
                yield f

        write_features(
            out,
            FeatureSource(features, bbox),
            sequence=args.sequence,
            geojson_type=gtype,
            use_rs=args.use_rs,
            **kw,
        )


def add_shapes(sub: Any) -> None:
    """Registers `shapes`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "shapes",
        cmd_shapes,
        "Write shapes extracted from bands or masks.",
        "Extracts shapes from one band or mask of a dataset and writes them out as GeoJSON. Unless "
        "otherwise specified, the shapes will be transformed to WGS 84 coordinates.",
    )
    p.add_argument("input", metavar="INPUT")
    opt_output(p)
    opt_precision(p)
    opt_indent(p)
    opt_projection(p, ("geographic", "projected"), "geographic")
    flag_pair(
        p,
        "--sequence",
        "--collection",
        "sequence",
        True,
        "Write a LF-delimited sequence of texts containing individual objects (the default) or "
        "write a single JSON text containing a feature collection object.",
    )
    opt_rs(p)
    opt_geojson_type(p, ("feature", "bbox"), "feature")
    flag_pair(
        p,
        "--band",
        "--mask",
        "band",
        True,
        "Choose to extract from a band (the default) or a mask.",
    )
    p.add_argument(
        "--bidx",
        dest="bandidx",
        type=int,
        default=None,
        help="Index of the band or mask that is the source of shapes.",
    )
    p.add_argument(
        "--sampling",
        type=int,
        default=1,
        help="Inverse of the sampling fraction; a value of 10 decimates.",
    )
    flag_pair(
        p,
        "--with-nodata",
        "--without-nodata",
        "with_nodata",
        False,
        "Include or do not include (the default) nodata regions.",
    )
    flag_pair(
        p,
        "--as-mask",
        "--not-as-mask",
        "as_mask",
        False,
        "Interpret a band as a mask and output only one class of valid data shapes.",
    )


def cmd_clip(args: argparse.Namespace, env: Any) -> None:
    """`clip`: clips a raster to bounds, a template's bounds or its valid data.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import terrio.rio as rasterio
    from terrio.rio import windows
    from terrio.rio.coords import disjoint_bounds
    from terrio.rio.crs import CRS
    from terrio.rio.enums import MaskFlags
    from terrio.rio.warp import transform_bounds

    bounds = parse_bounds(args.bounds)
    nodata = parse_nodata(args.nodata)
    creation_options = key_val(args.creation_options)
    with env:
        output, files = resolve_inout(
            files=args.files, output=args.output, overwrite=args.overwrite
        )
        with rasterio.open(files[0]) as src:
            if not src.transform.is_rectilinear:
                raise UsageError(
                    "Non-rectilinear rasters (i.e. with rotation or shear) cannot be clipped"
                )
            if bounds:
                if args.projection == "geographic":
                    bounds = transform_bounds(
                        CRS.from_epsg(4326), src.crs, bounds[0], bounds[1], bounds[2], bounds[3]
                    )
                if disjoint_bounds(bounds, src.bounds):
                    raise UsageError(
                        "Invalid value for '--bounds': must overlap the extent of the input raster"
                    )
                bounds_window = src.window(*bounds)
            elif args.like:
                with rasterio.open(args.like) as template_ds:
                    bounds = template_ds.bounds
                    if template_ds.crs != src.crs:
                        bounds = transform_bounds(template_ds.crs, src.crs, *bounds)
                    if disjoint_bounds(bounds, src.bounds):
                        raise UsageError(
                            "Invalid value for '--like': must overlap the extent of the input "
                            "raster"
                        )
                bounds_window = src.window(*bounds)
            elif args.to_data_window:
                src_win = windows.Window(0, 0, width=src.width, height=src.height)
                data_wins = []
                for sw in windows.subdivide(src_win, 512, 512):
                    dw = windows.get_data_window(src.read(1, masked=True, window=sw))
                    if dw.width * dw.height > 0:
                        data_wins.append(
                            windows.Window(
                                col_off=dw.col_off + sw.col_off,
                                row_off=dw.row_off + sw.row_off,
                                width=dw.width,
                                height=dw.height,
                            )
                        )
                bounds_window = windows.union(data_wins)
            else:
                raise UsageError("--bounds, --like, or --to-data-window required")
            if not args.with_complement:
                bounds_window = bounds_window.intersection(
                    windows.Window(0, 0, src.width, src.height)
                )
            out_window = bounds_window.round_lengths().round_offsets()
            height = int(out_window.height)
            width = int(out_window.width)
            out_kwargs = src.profile
            if args.driver:
                out_kwargs["driver"] = args.driver
            if nodata is not None:
                out_kwargs["nodata"] = nodata
            out_kwargs.update(
                {"height": height, "width": width, "transform": src.window_transform(out_window)}
            )
            out_kwargs.update(**creation_options)
            if "blockxsize" in out_kwargs and int(out_kwargs["blockxsize"]) > width:
                del out_kwargs["blockxsize"]
                logger.warning(
                    "Blockxsize removed from creation options to accommodate small output width"
                )
            if "blockysize" in out_kwargs and int(out_kwargs["blockysize"]) > height:
                del out_kwargs["blockysize"]
                logger.warning(
                    "Blockysize removed from creation options to accommodate small output height"
                )
            with rasterio.open(output, "w", **out_kwargs) as out:
                dest_win = windows.Window(0, 0, width, height)
                for dw, sw in zip(
                    windows.subdivide(dest_win, 512, 512),
                    windows.subdivide(out_window, 512, 512),
                    strict=False,
                ):
                    out_shape = (src.count, sw.height, sw.width)
                    out.write(
                        src.read(window=sw, out_shape=out_shape, boundless=True, masked=True),
                        window=dw,
                    )
                    if MaskFlags.per_dataset in src.mask_flag_enums[0]:
                        out.write_mask(
                            src.read_masks(window=sw, out_shape=out_shape, boundless=True)[0],
                            window=dw,
                        )


def add_clip(sub: Any) -> None:
    """Registers `clip`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "clip",
        cmd_clip,
        "Clip a raster to given bounds.",
        "Clips a raster using projected or geographic bounds.\n\n  terrio-rio clip input.tif "
        "output.tif --bounds xmin ymin xmax ymax\n  terrio-rio clip input.tif output.tif --like "
        "template.tif",
    )
    p.add_argument("files", nargs="+", metavar="INPUT OUTPUT")
    opt_output(p)
    opt_bounds(p)
    p.add_argument(
        "--to-data-window",
        dest="to_data_window",
        action="store_const",
        const="datawindow",
        default=None,
        help=(
            "Clip the raster to the region of valid data by removing areas of surrounding NoData "
            "values."
        ),
    )
    p.add_argument("--like", help="Raster dataset to use as a template for bounds")
    opt_format(p)
    opt_nodata(p)
    p.add_argument(
        "--geographic",
        dest="projection",
        action="store_const",
        const="geographic",
        help="Bounds in geographic coordinates.",
    )
    p.add_argument(
        "--projected",
        dest="projection",
        action="store_const",
        const="projected",
        help="Bounds in input's own projected coordinates (the default).",
    )
    p.set_defaults(projection="projected")
    opt_overwrite(p)
    opt_co(p)
    flag_pair(
        p,
        "--with-complement",
        "--without-complement",
        "with_complement",
        False,
        "Include the relative complement of the raster in the given bounds (giving a larger "
        "result), else return results only from the intersection of the raster and the bounds (the "
        "default).",
    )


def cmd_convert(args: argparse.Namespace, env: Any) -> None:
    """`convert`: copies a raster to another format or data type, optionally scaled.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import numpy as np

    import terrio.rio as rasterio
    from terrio.rio.enums import MaskFlags
    from terrio.rio.windows import Window, subdivide

    creation_options = key_val(args.creation_options)
    files = [file_in(f) for f in args.files[:-1]] + args.files[-1:]
    with env:
        outputfile, inputs = resolve_inout(
            files=files, output=args.output, overwrite=args.overwrite, num_inputs=1
        )
        with rasterio.open(inputs[0]) as src:
            profile = src.profile
            profile.pop("driver", None)
            if args.driver:
                profile["driver"] = args.driver
            if args.dtype:
                profile["dtype"] = args.dtype
            dst_dtype = profile["dtype"]
            if args.photometric:
                creation_options["photometric"] = args.photometric
            profile.update(**creation_options)
            bxsize = bysize = 1024
            if profile.get("tiled", False):
                bxsize = int(profile.get("blockxsize", bxsize))
                bysize = int(profile.get("blockysize", bysize))
            srcds = Window(0, 0, src.width, src.height)
            with rasterio.open(outputfile, "w", **profile) as dst:
                for chunk in subdivide(srcds, bxsize, bysize):
                    data = src.read(window=chunk, masked=True)
                    if args.scale_ratio:
                        data = data.astype("float64", casting="unsafe", copy=False)
                        np.multiply(data, args.scale_ratio, out=data, casting="unsafe")
                    if args.scale_offset:
                        data = data.astype("float64", casting="unsafe", copy=False)
                        np.add(data, args.scale_offset, out=data, casting="unsafe")
                    result = data.astype(dst_dtype, casting="unsafe", copy=False)
                    dst.write(result, window=chunk)
                    if MaskFlags.per_dataset in src.mask_flag_enums[0]:
                        dst.write_mask(src.read_masks(window=chunk)[0], window=chunk)


def add_convert(sub: Any) -> None:
    """Registers `convert`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "convert",
        cmd_convert,
        "Copy and convert raster dataset.",
        "Copy and convert raster datasets to other data types and formats.\n\nDestination values "
        "are dst = scale_ratio * src + scale_offset.\n\n  terrio-rio convert in16.tif out8.tif "
        "--dtype uint8 --scale-ratio 0.0625",
    )
    p.add_argument("files", nargs="+", metavar="INPUTS... OUTPUT")
    opt_output(p)
    opt_format(p)
    opt_dtype(p)
    p.add_argument(
        "--scale-ratio", type=float, default=None, help="Source to destination scaling ratio."
    )
    p.add_argument(
        "--scale-offset", type=float, default=None, help="Source to destination scaling offset."
    )
    p.add_argument(
        "--rgb",
        dest="photometric",
        action="store_const",
        const="rgb",
        default=None,
        help="Set RGB photometric interpretation.",
    )
    opt_overwrite(p)
    opt_co(p)


def cmd_merge(args: argparse.Namespace, env: Any) -> None:
    """`merge`: merges rasters into one.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    from terrio.rio.enums import Resampling
    from terrio.rio.merge import merge as merge_tool

    bounds = parse_bounds(args.bounds)
    nodata = parse_nodata(args.nodata)
    creation_options = key_val(args.creation_options)
    if args.precision is not None:
        logger.warning(
            "The --precision option is unused, deprecated, and will be removed in 2.0.0."
        )
    files = [file_in(f) for f in args.files[:-1]] + args.files[-1:]
    output, inputs = resolve_inout(files=files, output=args.output, overwrite=args.overwrite)
    if args.driver:
        creation_options.update(driver=args.driver)
    with env:
        merge_tool(
            inputs,
            bounds=bounds,
            res=tuple(args.res),
            nodata=nodata,
            dtype=args.dtype,
            indexes=(args.bidx or None),
            resampling=Resampling[args.resampling],
            method=args.method,
            target_aligned_pixels=args.target_aligned_pixels,
            mem_limit=args.mem_limit,
            use_highest_res=args.use_highest_res,
            dst_path=output,
            dst_kwds=creation_options,
        )


def add_merge(sub: Any) -> None:
    """Registers `merge`.

    Args:
        sub: The subparsers.
    """
    from terrio.rio.enums import Resampling
    from terrio.rio.merge import MERGE_METHODS

    p = command(
        sub,
        "merge",
        cmd_merge,
        "Merge a stack of raster datasets.",
        "Copy valid pixels from input files to an output file.\n\nAll files must have the same "
        "number of bands, data type, and coordinate reference system. Input files are merged in "
        "their listed order using the reverse painter's algorithm.",
    )
    p.add_argument("files", nargs="+", metavar="INPUTS... OUTPUT")
    opt_output(p)
    opt_format(p)
    opt_bounds(p)
    opt_res(p)
    p.add_argument(
        "--resampling",
        choices=[r.name for r in Resampling if r.value <= 7],
        default="nearest",
        help="Resampling method.  [default: nearest]",
    )
    p.add_argument(
        "--method",
        choices=list(MERGE_METHODS),
        default="first",
        help="Merging strategy.  [default: first]",
    )
    opt_nodata(p)
    opt_dtype(p)
    p.add_argument(
        "-b", "--bidx", type=int, action="append", default=[], help="Indexes of input file bands."
    )
    opt_overwrite(p)
    p.add_argument(
        "--precision",
        type=int,
        default=None,
        help="Unused, deprecated, and will be removed in 2.0.0.",
    )
    flag_pair(
        p,
        "--target-aligned-pixels",
        "--no-target-aligned-pixels",
        "target_aligned_pixels",
        False,
        "Align the output bounds based on the resolution.",
    )
    p.add_argument(
        "--mem-limit",
        type=int,
        default=64,
        help="Limit on memory used to perform calculations, in MB.",
    )
    flag_pair(
        p,
        "--use-highest-res",
        "--use-first-res",
        "use_highest_res",
        False,
        "Use the highest resolution of sources or the resolution of the first source argument.",
    )
    opt_co(p)


def parse_build(value: str | None) -> Any:
    """A `--build` value: `n,n,n`, `b^min..max` or `auto`.

    Args:
        value: The option's text.

    Returns:
        list | str | None: The factors, or `auto`.

    Raises:
        UsageError: If the value has none of these forms.
    """
    if not value:
        return value
    try:
        if "^" in value:
            base, exp_range = value.split("^")
            exp_min, exp_max = (int(v) for v in exp_range.split(".."))
            return [pow(int(base), k) for k in range(exp_min, exp_max + 1)]
        if "," in value:
            return [int(v) for v in value.split(",")]
        if value == "auto":
            return value
        raise ValueError(value)
    except Exception:
        raise UsageError(
            "Invalid value for '--build': must match 'n,n,n,…', 'n^n..n', or 'auto'."
        ) from None


def cmd_overview(args: argparse.Namespace, env: Any) -> None:
    """`overview`: builds or lists overviews.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import terrio.rio as rasterio
    from terrio.rio.enums import OverviewResampling

    build = parse_build(args.build)
    path = file_in(args.input)
    with env:
        if args.ls:
            with rasterio.open(path, "r") as dst:
                method = dst.tags(ns="rio_overview").get("resampling") or "unknown"
                echo("Overview factors:")
                for idx in dst.indexes:
                    echo(f"  Band {idx}: {dst.overviews(idx) or 'None'} (method: '{method}')")
        elif args.rebuild:
            with rasterio.open(path, "r+") as dst:
                factors = set().union(*[set(dst.overviews(i)) for i in dst.indexes])
                method = dst.tags(ns="rio_overview").get("resampling") or args.resampling
                dst.build_overviews(list(factors), OverviewResampling[method])
        elif build:
            with rasterio.open(path, "r+") as dst:
                if build == "auto":
                    level = 0
                    factor = 1
                    while min(dst.width // factor, dst.height // factor) > 256:
                        factor *= 2
                        level += 1
                    build = [2**j for j in range(1, level + 1)]
                dst.build_overviews(build, OverviewResampling[args.resampling])
                dst.update_tags(ns="rio_overview", resampling=args.resampling)
        else:
            raise UsageError("Please specify --ls, --rebuild, or --build ...")


def add_overview(sub: Any) -> None:
    """Registers `overview`.

    Args:
        sub: The subparsers.
    """
    from terrio.rio.enums import OverviewResampling

    p = command(
        sub,
        "overview",
        cmd_overview,
        "Construct overviews in an existing dataset.",
        "Construct overviews in an existing dataset.\n\n  terrio-rio overview --build 2,4,8,16\n  "
        "terrio-rio overview --build 2^1..4\n  terrio-rio overview --build auto\n  terrio-rio "
        "overview --ls",
    )
    p.add_argument("input", metavar="INPUT")
    p.add_argument(
        "--build",
        metavar="f1,f2,…|b^min..max|auto",
        help=(
            "A sequence of decimation factors specified as comma-separated list of numbers or a "
            "base and range of exponents, or 'auto' to automatically determine the maximum factor."
        ),
    )
    p.add_argument("--ls", action="store_true", help="Print the overviews for each band.")
    p.add_argument("--rebuild", action="store_true", help="Reconstruct existing overviews.")
    p.add_argument(
        "--resampling",
        choices=[r.name for r in OverviewResampling],
        default="nearest",
        help="Resampling algorithm.  [default: nearest]",
    )


def cmd_warp(args: argparse.Namespace, env: Any) -> None:
    """`warp`: reprojects a raster.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import numpy as np
    from affine import Affine

    import terrio.rio as rasterio
    from terrio.rio.crs import CRS
    from terrio.rio.env import setenv
    from terrio.rio.errors import CRSError
    from terrio.rio.transform import compose, rowcol
    from terrio.rio.warp import Resampling, aligned_target, reproject, transform_bounds
    from terrio.rio.warp import calculate_default_transform as calcdt

    creation_options = key_val(args.creation_options)
    warper_options = key_val(args.warper_options)
    files = [file_in(f) for f in args.files[:-1]] + args.files[-1:]
    output, inputs = resolve_inout(files=files, output=args.output, overwrite=args.overwrite)
    resampling = Resampling[args.resampling]
    res: Any = (
        None
        if not args.res
        else ((args.res[0], args.res[0]) if len(args.res) == 1 else tuple(args.res))
    )
    dst_crs: Any = args.dst_crs
    dimensions, src_bounds, dst_bounds = args.dimensions, args.src_bounds, args.dst_bounds
    if args.target_aligned_pixels and not res:
        raise UsageError("--target-aligned-pixels requires a specified resolution")
    if args.like:
        if any(p for p in (dimensions, dst_bounds, dst_crs, res) if p is not None):
            raise UsageError(
                "--like cannot be used with any of --dimensions, --bounds, --dst-crs, or --res"
            )
    elif dimensions:
        if any(p for p in (dst_bounds, res) if p is not None):
            raise UsageError("--dimensions cannot be used with --bounds or --res")
    with env:
        setenv(CHECK_WITH_INVERT_PROJ=args.check_invert_proj)
        with rasterio.open(inputs[0]) as src:
            left, bottom, right, top = src.bounds
            out_kwargs = src.profile
            out_kwargs.pop("driver", None)
            if args.driver:
                out_kwargs["driver"] = args.driver
            if src_bounds and dst_bounds:
                raise UsageError(
                    "--src-bounds and destination --bounds may not be specified simultaneously."
                )
            if args.like:
                with rasterio.open(args.like) as template_ds:
                    dst_crs = template_ds.crs
                    dst_transform = template_ds.transform
                    dst_height = template_ds.height
                    dst_width = template_ds.width
            elif dst_crs is not None:
                try:
                    dst_crs = CRS.from_string(dst_crs)
                except ValueError as err:
                    raise UsageError(f"Invalid value for dst_crs: {err}") from None
                if dimensions:
                    dst_width, dst_height = dimensions
                    try:
                        xmin, ymin, xmax, ymax = transform_bounds(
                            src.crs, dst_crs, *(src_bounds or src.bounds)
                        )
                    except CRSError as err:
                        raise UsageError(f"Invalid value for dst_crs: {err}") from None
                    dst_transform = Affine(
                        (xmax - xmin) / float(dst_width),
                        0,
                        xmin,
                        0,
                        (ymin - ymax) / float(dst_height),
                        ymax,
                    )
                elif src_bounds or dst_bounds:
                    if not res:
                        raise UsageError("Invalid value for res: Required when using --bounds.")
                    if src_bounds:
                        try:
                            xmin, ymin, xmax, ymax = transform_bounds(src.crs, dst_crs, *src_bounds)
                        except CRSError as err:
                            raise UsageError(f"Invalid value for dst_crs: {err}") from None
                    else:
                        xmin, ymin, xmax, ymax = dst_bounds
                    dst_transform = Affine(res[0], 0, xmin, 0, -res[1], ymax)
                    dst_width = max(int(math.ceil((xmax - xmin) / res[0])), 1)
                    dst_height = max(int(math.ceil((ymax - ymin) / res[1])), 1)
                else:
                    try:
                        if src.transform.is_identity and src.gcps:
                            src_crs = src.gcps[1]
                            kwargs: dict[str, Any] = {"gcps": src.gcps[0]}
                        else:
                            src_crs = src.crs
                            kwargs = src.bounds._asdict()
                        dst_transform, dst_width, dst_height = calcdt(
                            src_crs,
                            dst_crs,
                            src.width,
                            src.height,
                            resolution=res,
                            **kwargs,
                            **warper_options,
                        )
                    except CRSError as err:
                        raise UsageError(f"Invalid value for dst_crs: {err}") from None
            elif dimensions:
                dst_crs = src.crs
                dst_width, dst_height = dimensions
                lft, btm, rgt, tp = src_bounds or (left, bottom, right, top)
                dst_transform = Affine(
                    (rgt - lft) / float(dst_width), 0, lft, 0, (btm - tp) / float(dst_height), tp
                )
            elif src_bounds or dst_bounds:
                if not res:
                    res = (src.transform.a, -src.transform.e)
                dst_crs = src.crs
                xmin, ymin, xmax, ymax = src_bounds or dst_bounds
                dst_transform = Affine(res[0], 0, xmin, 0, -res[1], ymax)
                dst_width = max(int(round((xmax - xmin) / res[0])), 1)
                dst_height = max(int(round((ymax - ymin) / res[1])), 1)
            elif res:
                dst_crs = src.crs
                dst_transform = Affine(res[0], 0, left, 0, -res[1], top)
                dst_width = max(int(round((right - left) / res[0])), 1)
                dst_height = max(int(round((top - bottom) / res[1])), 1)
            else:
                dst_crs = src.crs
                rows, cols = rowcol(
                    src.transform,
                    np.array([left, right, right, left]),
                    np.array([top, top, bottom, bottom]),
                    op=float,
                )
                cols, rows = np.asarray(cols), np.asarray(rows)
                px = (right - left) / (cols.max() - cols.min())
                py = (top - bottom) / (rows.max() - rows.min())
                res = max(px, py)
                dst_width = max(int(round((right - left) / res)), 1)
                dst_height = max(int(round((top - bottom) / res)), 1)
                dst_transform = compose(Affine.translation(left, top), Affine.scale(res, -res))
            if args.target_aligned_pixels:
                dst_transform, dst_width, dst_height = aligned_target(
                    dst_transform, dst_width, dst_height, res
                )
            if args.src_nodata is not None:
                out_kwargs.update(nodata=args.src_nodata)
            if args.dst_nodata is not None:
                if args.src_nodata is None and src.meta["nodata"] is None:
                    raise UsageError("--src-nodata must be provided because dst-nodata is not None")
                out_kwargs.update(nodata=args.dst_nodata)
            out_kwargs.update(
                crs=dst_crs, transform=dst_transform, width=dst_width, height=dst_height
            )
            if "blockxsize" in out_kwargs and dst_width < int(out_kwargs["blockxsize"]):
                del out_kwargs["blockxsize"]
                logger.warning(
                    "Blockxsize removed from creation options to accommodate small output width"
                )
            if "blockysize" in out_kwargs and dst_height < int(out_kwargs["blockysize"]):
                del out_kwargs["blockysize"]
                logger.warning(
                    "Blockxsize removed from creation options to accommodate small output height"
                )
            out_kwargs.update(**creation_options)
            if args.dry_run:
                if out_kwargs.get("crs", None):
                    out_crs = CRS.from_user_input(out_kwargs["crs"])
                    epsg = out_crs.to_epsg()
                    out_kwargs["crs"] = f"EPSG:{epsg}" if epsg else out_crs.to_string()
                echo("Output dataset profile:")
                echo(json.dumps(dict(**out_kwargs), indent=2, default=to_json))
            else:
                with rasterio.open(output, "w", **out_kwargs) as dst:
                    reproject(
                        source=rasterio.band(src, list(range(1, src.count + 1))),
                        destination=rasterio.band(dst, list(range(1, src.count + 1))),
                        src_transform=src.transform,
                        src_crs=src.crs,
                        src_nodata=args.src_nodata,
                        dst_transform=out_kwargs["transform"],
                        dst_crs=out_kwargs["crs"],
                        dst_nodata=args.dst_nodata,
                        resampling=resampling,
                        num_threads=args.threads,
                        **warper_options,
                    )


def add_warp(sub: Any) -> None:
    """Registers `warp`.

    Args:
        sub: The subparsers.
    """
    from terrio.rio.warp import SUPPORTED_RESAMPLING

    p = command(
        sub,
        "warp",
        cmd_warp,
        "Warp a raster dataset.",
        "Warp a raster dataset.\n\n  terrio-rio warp input.tif output.tif --like template.tif\n  "
        "terrio-rio warp input.tif output.tif --dimensions 100 200 --dst-crs EPSG:4326\n  "
        "terrio-rio warp input.tif output.tif --bounds -78 22 -76 24 --res 0.1 --dst-crs EPSG:4326",
    )
    p.add_argument("files", nargs="+", metavar="INPUTS... OUTPUT")
    opt_output(p)
    opt_format(p)
    p.add_argument(
        "--like",
        help=(
            "Raster dataset to use as a template for obtaining affine transform (bounds and "
            "resolution), and crs."
        ),
    )
    p.add_argument("--dst-crs", default=None, help="Target coordinate reference system.")
    p.add_argument(
        "--dimensions",
        nargs=2,
        type=int,
        default=None,
        help="Output dataset width, height in number of pixels.",
    )
    p.add_argument(
        "--src-bounds",
        nargs=4,
        type=float,
        default=None,
        help=(
            "Determine output extent from source bounds: left bottom right top. Cannot be used "
            "with destination --bounds"
        ),
    )
    p.add_argument(
        "--bounds",
        "--dst-bounds",
        dest="dst_bounds",
        nargs=4,
        type=float,
        default=None,
        help="Determine output extent from destination bounds: left bottom right top",
    )
    opt_res(p)
    p.add_argument(
        "--resampling",
        choices=resampling_names(SUPPORTED_RESAMPLING),
        default="nearest",
        help="Resampling method.  [default: nearest]",
    )
    p.add_argument("--src-nodata", type=float, default=None, help="Manually override source nodata")
    p.add_argument(
        "--dst-nodata", type=float, default=None, help="Manually override destination nodata"
    )
    p.add_argument("--threads", type=int, default=1, help="Number of processing threads.")
    flag_pair(
        p,
        "--check-invert-proj",
        "--no-check-invert-proj",
        "check_invert_proj",
        True,
        "Constrain output to valid coordinate region in dst-crs",
    )
    flag_pair(
        p,
        "--target-aligned-pixels",
        "--no-target-aligned-pixels",
        "target_aligned_pixels",
        False,
        "align the output bounds based on the resolution",
    )
    opt_overwrite(p)
    opt_co(p)
    p.add_argument(
        "--to",
        "--wo",
        "--transformer-option",
        "--warper-option",
        dest="warper_options",
        metavar="NAME=VALUE",
        action="append",
        default=[],
        help="GDAL warper and coordinate transformer options.",
    )
    p.add_argument(
        "--dry-run",
        action="store_true",
        help=(
            "Do not create an output file, but report on its expected size and other "
            "characteristics."
        ),
    )


def cmd_sample(args: argparse.Namespace, env: Any) -> None:
    """`sample`: samples a dataset at points.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import terrio.rio as rasterio

    files = list(args.files)
    source_path = files.pop(0)
    points = lines_of(files.pop(0) if files else "-")
    with env, rasterio.open(source_path) as src:
        indexes = src.indexes if args.bidx is None else parse_indexes(args.bidx, src.indexes)
        for vals in src.sample((json.loads(line) for line in points), indexes=indexes):
            echo(json.dumps(vals, default=to_json))


def add_sample(sub: Any) -> None:
    """Registers `sample`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "sample",
        cmd_sample,
        "Sample a dataset.",
        "Sample a dataset at one or more points.\n\nPoints (x, y) encoded as JSON arrays in the "
        "dataset's CRS are read from the second argument or stdin; the values of the bands are "
        "written as JSON arrays.",
    )
    p.add_argument("files", nargs="+", metavar='FILE "[x, y]"')
    p.add_argument("-b", "--bidx", default=None, help="Indexes of input file bands.")


def cmd_transform(args: argparse.Namespace, env: Any) -> None:
    """`transform`: transforms coordinates between CRSs.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import terrio.rio as rasterio
    import terrio.rio.shutil as rio_shutil
    from terrio.rio.warp import transform

    lines = lines_of(args.input)
    with env:
        crss: list[Any] = []
        for crs in (args.src_crs, args.dst_crs):
            if not crs.startswith("EPSG") and rio_shutil.exists(crs):
                with rasterio.open(crs) as f:
                    crs = f.crs
            crss.append(crs)
        src_crs, dst_crs = crss
        for line in lines:
            values = json.loads(line)
            xs, ys = transform(src_crs, dst_crs, values[::2], values[1::2])
            if args.precision >= 0:
                xs = [round(v, args.precision) for v in xs]
                ys = [round(v, args.precision) for v in ys]
            result: list[Any] = [0] * len(values)
            result[::2] = list(xs)
            result[1::2] = list(ys)
            echo(json.dumps(result))


def add_transform(sub: Any) -> None:
    """Registers `transform`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "transform",
        cmd_transform,
        "Transform coordinates.",
        "Transform coordinates between coordinate reference systems.\n\nJSON arrays of interleaved "
        "coordinates are read from stdin; arrays of transformed coordinates are written to stdout.",
    )
    p.add_argument("input", nargs="?", default="-", metavar="INPUT")
    p.add_argument("--src-crs", "--src_crs", default="EPSG:4326", help="Source CRS.")
    p.add_argument("--dst-crs", "--dst_crs", default="EPSG:4326", help="Destination CRS.")
    opt_precision(p)


def read_geojson(name: str, param: str) -> Any:
    """Reads a GeoJSON text from a file or standard input.

    Args:
        name: The path, or `-`.
        param: The option the text comes from, for errors.

    Returns:
        dict: The GeoJSON object.

    Raises:
        UsageError: If the text is not JSON.
    """
    try:
        with ExitStack() as stack:
            f = open_text(name)
            if f is not sys.stdin:
                stack.enter_context(f)
            return json.loads(f.read())
    except ValueError:
        raise UsageError(
            f"Invalid value for {param}: GeoJSON could not be read from {param} or stdin"
        ) from None


def cmd_mask(args: argparse.Namespace, env: Any) -> None:
    """`mask`: masks a raster with GeoJSON features.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import terrio.rio as rasterio
    import terrio.rio.shutil as rio_shutil
    from terrio.rio.mask import mask as mask_tool

    creation_options = key_val(args.creation_options)
    files = [file_in(f) for f in args.files[:-1]] + args.files[-1:]
    output, inputs = resolve_inout(files=files, output=args.output, overwrite=args.overwrite)
    src_path = inputs[0]
    if args.geojson_mask is None:
        echo("No GeoJSON provided, INPUT will be copied to OUTPUT", sys.stderr)
        rio_shutil.copyfiles(src_path, output)
        return
    invert = args.invert
    if args.crop and invert:
        echo("Invert option ignored when using --crop", sys.stderr)
        invert = False
    with env:
        geojson = read_geojson(args.geojson_mask, "--geojson-mask")
        if "features" in geojson:
            geometries = [f["geometry"] for f in geojson["features"]]
        elif "geometry" in geojson:
            geometries = [geojson["geometry"]]
        elif "coordinates" in geojson:
            geometries = [geojson]
        else:
            raise UsageError("Invalid value for 'input': Invalid GeoJSON")
        with rasterio.open(src_path) as src:
            try:
                out_image, out_transform = mask_tool(
                    src, geometries, crop=args.crop, invert=invert, all_touched=args.all_touched
                )
            except ValueError as e:
                if e.args and e.args[0] == "Input shapes do not overlap raster." and args.crop:
                    raise UsageError(
                        "Invalid value for '--crop': not allowed for GeoJSON outside the extent of "
                        "the input raster"
                    ) from None
                raise
            profile = src.profile
            profile.update(**creation_options)
            profile.pop("driver", None)
            profile.update(
                {
                    "driver": args.driver,
                    "height": out_image.shape[1],
                    "width": out_image.shape[2],
                    "transform": out_transform,
                }
            )
            with rasterio.open(output, "w", **profile) as out:
                out.write(out_image)


def add_mask(sub: Any) -> None:
    """Registers `mask`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "mask",
        cmd_mask,
        "Mask in raster using features.",
        "Masks in raster using GeoJSON features (masks out all areas not covered by features), and "
        "optionally crops the output raster to the extent of the features.\n\n  terrio-rio mask "
        "input.tif output.tif --geojson-mask features.json\n  terrio-rio mask input.tif output.tif "
        "--geojson-mask - < features.json",
    )
    p.add_argument("files", nargs="+", metavar="INPUTS... OUTPUT")
    opt_output(p)
    p.add_argument(
        "-j",
        "--geojson-mask",
        dest="geojson_mask",
        default=None,
        help=(
            'GeoJSON file to use for masking raster.  Use "-" to read from stdin.  If not '
            "provided, original raster will be returned"
        ),
    )
    opt_format(p)
    opt_all_touched(p)
    p.add_argument(
        "--crop", action="store_true", help="Crop output raster to the extent of the geometries."
    )
    p.add_argument(
        "-i",
        "--invert",
        action="store_true",
        help=(
            "Inverts the mask, so that areas covered by features are masked out and areas not "
            "covered are retained.  Ignored if using --crop"
        ),
    )
    opt_overwrite(p)
    opt_co(p)


def cmd_rasterize(args: argparse.Namespace, env: Any) -> None:
    """`rasterize`: burns GeoJSON features into a new or existing raster.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    from affine import Affine

    import terrio.rio as rasterio
    import terrio.rio.shutil as rio_shutil
    from terrio.rio.coords import disjoint_bounds
    from terrio.rio.crs import CRS
    from terrio.rio.errors import CRSError
    from terrio.rio.features import bounds as calculate_bounds
    from terrio.rio.features import rasterize

    bounds = parse_bounds(args.bounds)
    nodata = parse_nodata(args.nodata)
    creation_options = key_val(args.creation_options)
    files = list(args.files)
    output, files = resolve_inout(files=files, output=args.output, overwrite=args.overwrite)
    has_src_crs = args.src_crs is not None
    try:
        src_crs = CRS.from_string(args.src_crs) if has_src_crs else CRS.from_string("EPSG:4326")
    except CRSError:
        raise UsageError(
            "Invalid value for '--src_crs': invalid CRS.  Must be an EPSG code."
        ) from None
    default_value: Any = args.default_value
    fill: Any = args.fill
    if default_value == int(default_value):
        default_value = int(default_value)
    if fill == int(fill):
        fill = int(fill)
    res = args.res
    prop = args.prop

    def feature_value(feature: dict[str, Any]) -> Any:
        """The value burned for a feature.

        Args:
            feature: The feature.

        Returns:
            The `--property` value, else the default value.
        """
        if prop and "properties" in feature:
            return feature["properties"].get(prop, default_value)
        return default_value

    with env:
        geojson = read_geojson(files.pop(0) if files else "-", "input")
        if "features" in geojson:
            geometries = [(f["geometry"], feature_value(f)) for f in geojson["features"]]
        elif "geometry" in geojson:
            geometries = [(geojson["geometry"], feature_value(geojson))]
        else:
            raise UsageError("Invalid value for 'input': Invalid GeoJSON")
        geojson_bounds = geojson.get("bbox", calculate_bounds(geojson))
        if not output:
            raise UsageError("an output path is required")
        if rio_shutil.exists(output):
            with rasterio.open(output, "r+") as out:
                if has_src_crs and src_crs != out.crs:
                    raise UsageError(
                        "Invalid value for 'input': GeoJSON does not match crs of existing output "
                        "raster"
                    )
                if disjoint_bounds(geojson_bounds, out.bounds):
                    echo(
                        "GeoJSON outside bounds of existing output raster. Are they in different "
                        "coordinate reference systems?",
                        sys.stderr,
                    )
                meta = out.meta
                result = rasterize(
                    geometries,
                    out_shape=(meta["height"], meta["width"]),
                    transform=meta.get("affine", meta["transform"]),
                    all_touched=args.all_touched,
                    dtype=meta.get("dtype", None),
                    default_value=default_value,
                    fill=fill,
                )
                for bidx in range(1, meta["count"] + 1):
                    data = out.read(bidx, masked=True)
                    ne = result != fill
                    data[ne] = result[ne]
                    if data.mask.any():
                        data.mask[ne] = False
                    out.write(data, indexes=bidx)
            return
        if args.like is not None:
            with rasterio.open(args.like) as template_ds:
                if has_src_crs and src_crs != template_ds.crs:
                    raise UsageError(
                        "Invalid value for 'input': GeoJSON does not match crs of --like raster"
                    )
                if disjoint_bounds(geojson_bounds, template_ds.bounds):
                    echo(
                        "GeoJSON outside bounds of --like raster. Are they in different coordinate "
                        "reference systems?",
                        sys.stderr,
                    )
                kwargs = template_ds.profile
                kwargs["count"] = 1
                kwargs["transform"] = template_ds.transform
        else:
            bounds = bounds or geojson_bounds
            if src_crs.is_geographic:
                if bounds[0] < -180 or bounds[2] > 180 or bounds[1] < -80 or bounds[3] > 80:
                    raise UsageError(
                        "Invalid value for '--bounds': Bounds are beyond the valid extent for "
                        "EPSG:4326."
                    )
            if args.dimensions:
                width, height = args.dimensions
                res = (
                    (bounds[2] - bounds[0]) / float(width),
                    (bounds[3] - bounds[1]) / float(height),
                )
            else:
                if not res:
                    raise UsageError("Invalid value for '--res': pixel dimensions are required")
                if len(res) == 1:
                    res = (res[0], res[0])
                width = max(int(math.ceil((bounds[2] - bounds[0]) / float(res[0]))), 1)
                height = max(int(math.ceil((bounds[3] - bounds[1]) / float(res[1]))), 1)
            kwargs = {
                "count": 1,
                "crs": src_crs,
                "width": width,
                "height": height,
                "transform": Affine(res[0], 0, bounds[0], 0, -res[1], bounds[3]),
                "driver": args.driver,
            }
        kwargs.update(**creation_options)
        if nodata is not None:
            kwargs["nodata"] = nodata
        result = rasterize(
            geometries,
            out_shape=(kwargs["height"], kwargs["width"]),
            transform=kwargs["transform"],
            all_touched=args.all_touched,
            dtype=kwargs.get("dtype", None),
            default_value=default_value,
            fill=fill,
        )
        if "dtype" not in kwargs:
            kwargs["dtype"] = result.dtype
        with rasterio.open(output, "w", **kwargs) as out:
            out.write(result, indexes=1)


def add_rasterize(sub: Any) -> None:
    """Registers `rasterize`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "rasterize",
        cmd_rasterize,
        "Rasterize features.",
        "Rasterize GeoJSON into a new or existing raster.\n\nIf the output raster exists, feature "
        "values are burned into all its bands. Otherwise --like, or --bounds (by default the "
        "GeoJSON's) with --dimensions or --res, give the output grid.",
    )
    p.add_argument("files", nargs="*", metavar="INPUTS... OUTPUT")
    opt_output(p)
    opt_format(p)
    p.add_argument(
        "--like",
        help=(
            "Raster dataset to use as a template for obtaining affine transform (bounds and "
            "resolution), crs, data type, and driver used to create the output."
        ),
    )
    opt_bounds(p)
    p.add_argument(
        "--dimensions",
        nargs=2,
        type=int,
        default=None,
        help="Output dataset width, height in number of pixels.",
    )
    opt_res(p)
    p.add_argument(
        "--src-crs",
        "--src_crs",
        dest="src_crs",
        default=None,
        help="Source coordinate reference system.  Default: EPSG:4326",
    )
    opt_all_touched(p)
    p.add_argument(
        "--default-value",
        "--default_value",
        dest="default_value",
        type=float,
        default=1,
        help="Default value for rasterized pixels",
    )
    p.add_argument(
        "--fill",
        type=float,
        default=0,
        help="Fill value for all pixels not overlapping features.  Default: 0",
    )
    p.add_argument(
        "--property",
        dest="prop",
        default=None,
        help="Property in GeoJSON features to use for rasterized values.",
    )
    opt_overwrite(p)
    opt_nodata(p)
    opt_co(p)


def cmd_stack(args: argparse.Namespace, env: Any) -> None:
    """`stack`: stacks bands of rasters into one.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import terrio.rio as rasterio
    from terrio.rio.enums import Resampling
    from terrio.rio.stack import stack as stack_tool

    bounds = parse_bounds(args.bounds)
    nodata = parse_nodata(args.nodata)
    key_val(args.creation_options)
    files = [file_in(f) for f in args.files[:-1]] + args.files[-1:]
    output, inputs = resolve_inout(files=files, output=args.output, overwrite=args.overwrite)
    output_count = 0
    indexes: list[Any] = []
    for path, item in zip_longest(inputs, args.bidx, fillvalue=None):
        with rasterio.open(path) as src:
            src_indexes = src.indexes
        if item is None:
            indexes.append(src_indexes)
            output_count += len(src_indexes)
        elif ".." in item:
            got = parse_indexes(item, src_indexes)
            indexes.append(got)
            output_count += len(got)
        else:
            parts = [int(x) for x in item.split(",")]
            indexes.append(parts[0] if len(parts) == 1 else parts)
            output_count += len(parts)
    dst_kwds: dict[str, Any] = {}
    if args.driver:
        dst_kwds["driver"] = args.driver
    if args.photometric:
        dst_kwds["photometric"] = args.photometric
    with env:
        stack_tool(
            inputs,
            bounds=bounds,
            res=tuple(args.res),
            nodata=nodata,
            dtype=args.dtype,
            indexes=indexes,
            output_count=output_count,
            resampling=Resampling[args.resampling],
            target_aligned_pixels=args.target_aligned_pixels,
            mem_limit=args.mem_limit,
            use_highest_res=args.use_highest_res,
            dst_path=output,
            dst_kwds=dst_kwds,
        )


def add_stack(sub: Any) -> None:
    """Registers `stack`.

    Args:
        sub: The subparsers.
    """
    from terrio.rio.enums import Resampling

    p = command(
        sub,
        "stack",
        cmd_stack,
        "Stack a number of bands into a multiband dataset.",
        "Stack a number of bands from one or more input files into a multiband dataset.\n\n  "
        "--bidx N, --bidx M,N,O, --bidx M..O, --bidx ..N, --bidx N.. select bands of each input in "
        "turn.\n\n"
        "  terrio-rio stack RGB.byte.tif --bidx ..2 RGB.byte.tif --bidx 3.. -o stacked.tif",
    )
    p.add_argument("files", nargs="+", metavar="INPUTS... OUTPUT")
    opt_output(p)
    opt_format(p)
    p.add_argument(
        "--rgb",
        dest="photometric",
        action="store_const",
        const="rgb",
        default=None,
        help="Set RGB photometric interpretation.",
    )
    opt_bounds(p)
    opt_res(p)
    p.add_argument(
        "--resampling",
        choices=[r.name for r in Resampling if r.value <= 7],
        default="nearest",
        help="Resampling method.  [default: nearest]",
    )
    opt_nodata(p)
    opt_dtype(p)
    p.add_argument("-b", "--bidx", action="append", default=[], help="Indexes of input file bands.")
    opt_overwrite(p)
    flag_pair(
        p,
        "--target-aligned-pixels",
        "--no-target-aligned-pixels",
        "target_aligned_pixels",
        False,
        "Align the output bounds based on the resolution.",
    )
    p.add_argument(
        "--mem-limit",
        type=int,
        default=64,
        help="Limit on memory used to perform calculations, in MB.",
    )
    flag_pair(
        p,
        "--use-highest-res",
        "--use-first-res",
        "use_highest_res",
        False,
        "Use the highest resolution of sources or the resolution of the first source argument.",
    )
    opt_co(p)


def cmd_edit_info(args: argparse.Namespace, env: Any) -> None:
    """`edit-info`: edits a dataset's metadata in place.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import warnings

    import terrio.rio as rasterio
    from terrio.rio.crs import CRS
    from terrio.rio.dtypes import in_dtype_range
    from terrio.rio.enums import ColorInterp
    from terrio.rio.errors import CRSError
    from terrio.rio.transform import guard_transform

    path = file_in(args.input)
    like: dict[str, Any] | None = None
    if args.like:
        with rasterio.open(args.like) as src:
            like = dict(src.meta)
            like["tags"] = src.tags()
            like["colorinterp"] = src.colorinterp
    all_like = bool(args.allmd) and like is not None

    def from_like(name: str, value: Any) -> Any:
        """A value taken from the `--like` dataset, for `like` or `--all`.

        Args:
            name: The metadata item.
            value: The option's value.

        Returns:
            The template's value, or None.
        """
        if like and (value == "like" or all_like):
            return like[name]
        return None

    nodata = None
    if args.nodata == "like" or args.nodata is None:
        nodata = from_like("nodata", args.nodata)
    if nodata is None:
        nodata = parse_nodata(args.nodata if args.nodata != "like" else None)
    crs = from_like("crs", args.crs)
    if crs is None and args.crs:
        value: Any = args.crs
        try:
            value = json.loads(value)
        except ValueError:
            pass
        try:
            crs = CRS(value) if isinstance(value, dict) else CRS.from_string(value)
        except CRSError:
            raise UsageError(f"Invalid value for crs: '{value}' is not a recognized CRS.") from None
    transform = from_like("transform", args.transform)
    if transform is None and args.transform:
        value = args.transform
        try:
            value = json.loads(value)
        except ValueError:
            pass
        try:
            transform = guard_transform(value)
        except Exception:
            raise UsageError(
                f"Invalid value for transform: '{value}' is not recognized as an Affine array."
            ) from None
    tags = from_like("tags", tuple(args.tags) if args.tags else None)
    if tags is None and args.tags:
        try:
            tags = dict(p.split("=") for p in args.tags)
        except Exception:
            raise UsageError(
                f"Invalid value for transform: '{tuple(args.tags)}' contains a malformed tag."
            ) from None
    colorinterp: Any = None
    if args.colorinterp is not None:
        ci = args.colorinterp
        if ci.lower() == "like":
            colorinterp = from_like("colorinterp", ci)
        elif ci.lower() == "rgb":
            colorinterp = (ColorInterp.red, ColorInterp.green, ColorInterp.blue)
        elif ci.lower() == "rgba":
            colorinterp = (ColorInterp.red, ColorInterp.green, ColorInterp.blue, ColorInterp.alpha)
        else:
            names = tuple(ci.split(","))
            for n in names:
                if n not in ColorInterp.__members__:
                    raise UsageError(
                        f"Invalid value for '--colorinterp': color interpretation '{n}' is "
                        f"invalid.  "
                        f"Must be one of: {', '.join(ColorInterp.__members__)}"
                    )
            colorinterp = tuple(ColorInterp[n] for n in names)
    with env, rasterio.open(path, "r+") as dst:
        if args.allmd:
            if like is None:
                raise UsageError("--all requires --like")
            nodata = like["nodata"]
            crs = like["crs"]
            transform = like["transform"]
            tags = like["tags"]
            colorinterp = like["colorinterp"]
        if args.unset_nodata and nodata is not None:
            raise UsageError("--unset-nodata and --nodata cannot be used together.")
        if args.unset_crs and crs:
            raise UsageError("--unset-crs and --crs cannot be used together.")
        if args.unset_nodata:
            dst.nodata = None
        elif nodata is not None:
            dtype = dst.dtypes[0]
            if not in_dtype_range(nodata, dtype):
                raise UsageError(
                    f"Invalid value for nodata: outside the range of the file's data type "
                    f"({dtype})."
                )
            dst.nodata = nodata
        if args.unset_crs:
            dst.crs = None
        elif crs:
            dst.crs = crs
        if transform:
            dst.transform = transform
        if tags:
            dst.update_tags(**tags)
        if args.units:
            dst.set_band_unit(args.bidx, args.units)
        if args.description:
            dst.set_band_description(args.bidx, args.description)
        if colorinterp:
            if args.like and len(colorinterp) != dst.count:
                raise CommandError(
                    "When using '--like' for color interpretation the template and target images "
                    "must "
                    f"have the same number of bands.  Found {len(colorinterp)} color "
                    f"interpretations for "
                    f"template image and {dst.count} bands in target image."
                )
            try:
                dst.colorinterp = colorinterp
            except ValueError as e:
                raise CommandError(str(e)) from None
    if args.unset_crs:
        with env, rasterio.open(path, "r") as src:
            if src.crs:
                warnings.warn("CRS was not unset.", stacklevel=1)


def add_edit_info(sub: Any) -> None:
    """Registers `edit-info`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "edit-info",
        cmd_edit_info,
        "Edit dataset metadata.",
        "Edit a dataset's metadata: coordinate reference system, affine transformation matrix, "
        "nodata value, and tags.\n\n  --crs 'EPSG:4326'\n  --transform '[300.038, 0.0, 101985.0, "
        "0.0, -300.042, 2826915.0]'\n  terrio-rio edit-info example.tif --like template.tif "
        "--all\n  terrio-rio edit-info example.tif --like template.tif --transform like",
    )
    p.add_argument("input", metavar="INPUT")
    p.add_argument("-b", "--bidx", type=int, default=1, help="Input file band index (default: 1).")
    p.add_argument(
        "--nodata", default=None, metavar="NUMBER|nan|null", help="Modify the Nodata value."
    )
    p.add_argument("--unset-nodata", action="store_true", help="Unset the dataset's nodata value.")
    p.add_argument("--crs", default=None, help="New coordinate reference system")
    p.add_argument("--unset-crs", action="store_true", help="Unset the dataset's CRS value.")
    p.add_argument("--transform", default=None, help="New affine transform matrix")
    p.add_argument("--units", help="Edit units of a band (requires --bidx)")
    p.add_argument("--description", help="Edit description of a band (requires --bidx)")
    p.add_argument(
        "--tag", dest="tags", metavar="KEY=VAL", action="append", default=[], help="New tag."
    )
    p.add_argument(
        "--all",
        dest="allmd",
        action="store_const",
        const="like",
        default=None,
        help="Copy all metadata items from the template file.",
    )
    p.add_argument(
        "--colorinterp",
        metavar="name[,name,...]|RGB|RGBA|like",
        help="Set color interpretation for all bands like 'red,green,blue,alpha'.",
    )
    p.add_argument(
        "--like",
        help=(
            "Raster dataset to use as a template for obtaining affine transform (bounds and "
            "resolution), crs, and nodata values."
        ),
    )


def cmd_env(args: argparse.Namespace, env: Any) -> None:
    """`env`: prints the formats (drivers) and credentials of the environment.

    Args:
        args: The parsed arguments.
        env: The environment.

    Raises:
        CommandError: For `--gdal-data` and `--proj-data`: terrio uses neither.
    """
    with env as e:
        if args.key == "credentials":
            echo(json.dumps(e.session.credentials))
        elif args.key == "gdal_data":
            raise CommandError("terrio.rio does not use GDAL, nor its data files")
        elif args.key == "proj_data":
            raise CommandError("terrio.rio does not use PROJ, nor its data files")
        else:
            for k, v in sorted(e.drivers().items()):
                echo(f"{k}: {v}")


def add_env(sub: Any) -> None:
    """Registers `env`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "env",
        cmd_env,
        "Print information about the terrio.rio environment.",
        "Print information about the terrio.rio environment.",
    )
    p.add_argument(
        "--formats",
        dest="key",
        action="store_const",
        const="formats",
        help="Enumerate the available formats.",
    )
    p.add_argument(
        "--credentials",
        dest="key",
        action="store_const",
        const="credentials",
        help="Print credentials.",
    )
    p.add_argument(
        "--gdal-data",
        dest="key",
        action="store_const",
        const="gdal_data",
        help="Print GDAL data path (terrio has none).",
    )
    p.add_argument(
        "--proj-data",
        dest="key",
        action="store_const",
        const="proj_data",
        help="Print PROJ data path (terrio has none).",
    )
    p.set_defaults(key="formats")


def cmd_gcps(args: argparse.Namespace, env: Any) -> None:
    """`gcps`: prints a dataset's ground control points as GeoJSON.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import terrio.rio as rasterio
    from terrio.rio.warp import transform_geom

    gtype = geojson_type(args, ("collection", "feature"), "feature")
    if gtype == "feature" and args.indent and not args.use_rs:
        raise UsageError("Pretty-printing a sequence of Features requires the --rs option")
    separators = (",", ":") if args.compact else None
    with env, rasterio.open(file_in(args.input)) as src:
        gcps, crs = src.gcps
        if not gcps or crs is None:
            raise CommandError(f"{args.input} has no ground control points with a CRS")
        proj = crs.to_string()
        proj = proj.split("=")[1].upper() if proj.startswith("+init=epsg") else proj

        def feature(p: Any) -> dict[str, Any]:
            """A GCP as a feature, in the output CRS and precision.

            Args:
                p: The GCP.

            Returns:
                dict: The feature.
            """
            feat = p.__geo_interface__
            feat["properties"].update(crs=proj)
            if args.projection == "geographic" or args.precision >= 0:
                dst_crs = "epsg:4326" if args.projection == "geographic" else crs
                feat["geometry"] = transform_geom(
                    crs, dst_crs, feat["geometry"], precision=args.precision
                )
            return feat

        if gtype == "collection":
            features = [feature(p) for p in gcps]
            echo(
                json.dumps(
                    {"type": "FeatureCollection", "features": features},
                    separators=separators,
                    indent=args.indent,
                )
            )
        else:
            for p in gcps:
                if args.use_rs:
                    sys.stdout.write("\x1e")
                echo(json.dumps(feature(p), separators=separators, indent=args.indent))


def add_gcps(sub: Any) -> None:
    """Registers `gcps`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "gcps",
        cmd_gcps,
        "Print ground control points as GeoJSON.",
        "Print GeoJSON representations of a dataset's control points.\n\nEach ground control point "
        "is a feature whose properties hold its row, col, x, y, z, crs, id and info.",
    )
    p.add_argument("input", metavar="INPUT")
    opt_geojson_type(p, ("collection", "feature"), "feature")
    opt_projection(p, ("geographic", "projected"), "geographic")
    opt_precision(p)
    flag_pair(
        p,
        "--sequence",
        "--no-sequence",
        "sequence",
        True,
        "Write a LF-delimited sequence of texts containing individual objects (the default) or "
        "write a single JSON text containing a feature collection object.",
    )
    opt_rs(p)
    opt_indent(p)


def cmd_create(args: argparse.Namespace, env: Any) -> None:
    """`create`: creates an empty dataset.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    from affine import Affine

    import terrio.rio as rasterio
    from terrio.rio.crs import CRS
    from terrio.rio.errors import CRSError
    from terrio.rio.transform import compose, guard_transform

    output = args.output
    if os.path.exists(output) and not args.overwrite:
        raise CommandError(
            "File exists and won't be overwritten without use of the '--overwrite' option."
        )
    crs = None
    if args.crs is not None:
        try:
            crs = CRS.from_string(args.crs)
        except CRSError:
            raise UsageError(
                f"Invalid value for crs: {args.crs} is not a recognized CRS."
            ) from None
    transform = None
    if args.transform is not None:
        try:
            transform = guard_transform(json.loads(args.transform))
        except Exception:
            raise UsageError(
                f"Invalid value for transform: {args.transform} is not recognized as a "
                f"transformarray."
            ) from None
    bounds = parse_bounds(args.bounds)
    geo_transform = None
    if bounds:
        left, bottom, right, top = bounds
        geo_transform = compose(
            Affine.translation(left, top),
            Affine.scale((right - left) / args.width, (bottom - top) / args.height),
        )
    if transform:
        if geo_transform is not None:
            echo(
                "--transform value is overriding --bounds value. Use only one of these options to "
                "avoid this warning.",
                sys.stderr,
            )
        geo_transform = transform
    profile = dict(
        driver=args.driver,
        dtype=args.dtype,
        count=args.count,
        height=args.height,
        width=args.width,
        nodata=parse_nodata(args.nodata),
        crs=crs,
        transform=geo_transform,
        **key_val(args.creation_options),
    )
    with env, rasterio.open(output, "w", **profile):
        pass


def add_create(sub: Any) -> None:
    """Registers `create`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "create",
        cmd_create,
        "Create an empty or filled dataset.",
        "Create an empty dataset.\n\n  terrio-rio create new.tif -f GTiff -t uint8 -n 3 -h 512 -w "
        "512 --co tiled=true --co blockxsize=256 --co blockysize=256",
    )
    p.add_argument("output", metavar="OUTPUT")
    opt_format(p)
    opt_dtype(p)
    p.add_argument("-n", "--count", type=int, help="Number of raster bands.")
    p.add_argument("-h", "--height", type=int, help="Raster height, or number of rows.")
    p.add_argument("-w", "--width", type=int, help="Raster width, or number of columns.")
    opt_nodata(p)
    p.add_argument("--crs", default=None, help="Coordinate reference system.")
    p.add_argument(
        "--transform",
        default=None,
        help="Affine transform matrix. Overrides any given bounds option.",
    )
    opt_bounds(p)
    opt_overwrite(p)
    opt_co(p)


def cmd_rm(args: argparse.Namespace, env: Any) -> None:
    """`rm`: deletes a dataset and its sidecar files.

    Args:
        args: The parsed arguments.
        env: The environment.

    Raises:
        CommandError: If the dataset cannot be deleted.
    """
    import terrio.rio.shutil as rio_shutil
    from terrio.rio.errors import DriverRegistrationError, RasterioIOError

    if not args.yes:
        answer = input("Are you sure you want to delete the dataset? [y/N]: ")
        if answer.strip().lower() not in ("y", "yes"):
            raise CommandError("Aborted!")
    try:
        rio_shutil.delete(args.path, driver=args.driver)
    except (DriverRegistrationError, RasterioIOError) as e:
        raise CommandError(str(e)) from None


def add_rm(sub: Any) -> None:
    """Registers `rm`.

    Args:
        sub: The subparsers.
    """
    p = command(sub, "rm", cmd_rm, "Delete a dataset.", "Delete a dataset, and its sidecar files.")
    p.add_argument("path", metavar="PATH")
    p.add_argument("--yes", action="store_true", help="Confirm delete without prompting")
    p.add_argument(
        "-f",
        "--format",
        "--driver",
        dest="driver",
        help="Explicitly delete with this driver rather than probing for the appropriate driver.",
    )


def cmd_insp(args: argparse.Namespace, env: Any) -> None:
    """`insp`: opens a dataset in an interactive Python interpreter.

    Args:
        args: The parsed arguments.
        env: The environment.
    """
    import code
    import importlib

    import numpy as np

    import terrio.rio as rasterio

    plt: Any
    try:
        plt = importlib.import_module("matplotlib.pyplot")
    except Exception:
        plt = None

    def stats(dataset: Any) -> Any:
        """The minimum, maximum and mean of an array or a `(dataset, band)` pair.

        Args:
            dataset: An array, or a dataset and a band.

        Returns:
            Stats: The statistics.
        """
        arr = dataset[0].read(dataset[1]) if isinstance(dataset, tuple) else dataset
        return Stats(np.min(arr), np.max(arr), np.mean(arr))

    with env, rasterio.open(file_in(args.input), args.mode) as src:
        banner = (
            f"terrio.rio {rasterio.__version__} Interactive Inspector (Python "
            f"{'.'.join(map(str, sys.version_info[:3]))})\n"
            'Type "src.meta", "src.read(1)", or "help(src)" for more information.'
        )
        local = {
            "Stats": Stats,
            "stats": stats,
            "src": src,
            "np": np,
            "rio": rasterio,
            "plt": plt,
        }
        if args.interpreter == "ipython":
            ipython: Any = importlib.import_module("IPython")
            ipython.InteractiveShell.banner1 = banner
            ipython.start_ipython(argv=[], user_ns=local)
        else:
            code.interact(banner, local=local)


def add_insp(sub: Any) -> None:
    """Registers `insp`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "insp",
        cmd_insp,
        "Open a data file and start an interpreter.",
        "Open the input file in a Python interpreter.",
    )
    p.add_argument("input", metavar="INPUT")
    p.add_argument(
        "--ipython",
        dest="interpreter",
        action="store_const",
        const="ipython",
        default=None,
        help="Use IPython as interpreter.",
    )
    p.add_argument(
        "-m", "--mode", choices=["r", "r+"], default="r", help="File mode (default 'r')."
    )


# ─── calc: snuggs expressions ───────────────────────────────────────────────────────────────────


class ExpressionError(SyntaxError):
    """An error in a calc expression, with its position."""

    filename = "<string>"
    lineno = 1


class _Kwarg:
    """A `:name` keyword marker of an expression."""

    def __init__(self, name: str) -> None:
        """Creates the marker.

        Args:
            name: The keyword.
        """
        self.name = name


_TOKEN = re.compile(
    r"""\s*(?:(?P<open>\()|(?P<close>\))|(?P<str>'[^']*'|"[^"]*")|(?P<atom>[^\s()'"]+))"""
)
_SCI = re.compile(r"^[+-]?(?:\d+\.?\d*|\.\d+)[eE][+-]?\d+$")
_REAL = re.compile(r"^[+-]?(?:\d+\.\d*|\.\d+)$")
_INT = re.compile(r"^[+-]?\d+$")
_IDENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")


def _ops() -> dict[str, Callable[..., Any]]:
    """The operators of snuggs expressions.

    Returns:
        dict: Operator names and functions.
    """
    import functools
    import operator

    def reduce(op: Callable[[Any, Any], Any]) -> Callable[..., Any]:
        return lambda *args: functools.reduce(op, args)

    return {
        "*": reduce(operator.mul),
        "+": reduce(operator.add),
        "/": reduce(operator.truediv),
        "-": reduce(operator.sub),
        "&": reduce(operator.and_),
        "|": reduce(operator.or_),
        "<": operator.lt,
        "<=": operator.le,
        "==": operator.eq,
        "!=": operator.ne,
        ">=": operator.ge,
        ">": operator.gt,
        "truth": operator.truth,
        "is": operator.is_,
        "not": operator.not_,
    }


def _higher_funcs() -> dict[str, Callable[..., Any]]:
    """The higher-order functions of snuggs expressions.

    Returns:
        dict: Names and functions.
    """
    import functools
    import operator

    return {
        "map": map,
        "partial": functools.partial,
        "reduce": functools.reduce,
        "itemgetter": operator.itemgetter,
    }


# The builtins an expression may call: those computing on values. Other names become method calls
# on the first argument, as in snuggs, except private ones and those writing files.
_SAFE_BUILTINS = {
    "abs",
    "all",
    "any",
    "bool",
    "complex",
    "dict",
    "divmod",
    "enumerate",
    "filter",
    "float",
    "frozenset",
    "int",
    "len",
    "list",
    "max",
    "min",
    "pow",
    "range",
    "reversed",
    "round",
    "set",
    "slice",
    "sorted",
    "str",
    "sum",
    "tuple",
    "zip",
}
_UNSAFE_METHODS = {"dump", "dumps", "tofile"}


class Snuggs:
    """An evaluator of snuggs s-expressions (rasterio's `rio calc` language), without pyparsing."""

    def __init__(
        self, funcs: dict[str, Callable[..., Any]], context: OrderedDict[str, Any]
    ) -> None:
        """Creates the evaluator.

        Args:
            funcs: The functions (`read`, `take`, `asarray`...).
            context: The named inputs, in order.
        """
        self.funcs = funcs
        self.context = context
        self.ops = _ops()
        self.higher = _higher_funcs()

    def lookup(self, index: Any, subindex: Any = None) -> Any:
        """The `index`-th input (from 1), or one of its bands.

        Args:
            index: The input.
            subindex: The band.

        Returns:
            The array.
        """
        if int(index) < 1 or (subindex is not None and int(subindex) < 1):
            raise ValueError(f"inputs and bands are numbered from 1: ({index} {subindex})")
        s = list(self.context.values())[int(index) - 1]
        return s[int(subindex) - 1] if subindex else s

    def func(self, name: str) -> Callable[..., Any]:
        """Resolves a function name, as rasterio's `FuncMapper` does.

        Args:
            name: The name.

        Returns:
            callable: A function, a builtin, or a call of the method of that name of the first
            argument.
        """
        if name in self.funcs:
            return self.funcs[name]
        if name in _SAFE_BUILTINS:
            return getattr(builtins, name)

        def method(g: Any, *args: Any, **kwargs: Any) -> Any:
            if name.startswith("_") or name in _UNSAFE_METHODS:
                raise AttributeError(f"'{name}' is not available in expressions")
            attr = getattr(g, name)
            return attr(*args, **kwargs) if callable(attr) else attr

        return method

    def parse(self, source: str) -> Any:
        """Parses an expression into nested lists of `(kind, text, offset)` atoms.

        Args:
            source: The expression.

        Returns:
            list: The expression tree.

        Raises:
            ExpressionError: If the parentheses do not balance.
        """
        pos = 0
        stack: list[list[Any]] = [[]]
        while pos < len(source):
            m = _TOKEN.match(source, pos)
            if m is None or m.end() == pos:
                if source[pos:].strip() == "":
                    break
                raise self.error("Expected an expression", source, pos)
            pos = m.end()
            if m.group("open"):
                stack.append([("(", "(", m.start("open"))])
            elif m.group("close"):
                if len(stack) < 2:
                    raise self.error("Unbalanced parentheses", source, m.start("close"))
                done = stack.pop()
                stack[-1].append(done)
            elif m.group("str"):
                stack[-1].append(("str", m.group("str")[1:-1], m.start("str")))
            else:
                stack[-1].append(("atom", m.group("atom"), m.start("atom")))
        if len(stack) != 1 or len(stack[0]) != 1 or not isinstance(stack[0][0], list):
            raise self.error("Expected '('", source, 0)
        return stack[0][0]

    @staticmethod
    def error(msg: str, source: str, offset: int) -> ExpressionError:
        """An expression error at an offset.

        Args:
            msg: The message.
            source: The expression.
            offset: The 0-based offset.

        Returns:
            ExpressionError: The error.
        """
        err = ExpressionError(msg)
        err.text = source
        err.offset = offset + 1
        return err

    def operand(self, node: Any, source: str) -> Any:
        """Evaluates an operand.

        Args:
            node: A list (an expression) or an atom.
            source: The expression, for errors.

        Returns:
            The value.

        Raises:
            ExpressionError: For unknown names.
        """
        if isinstance(node, list):
            return self.evaluate(node, source)
        kind, text, offset = node
        if kind == "str":
            return text
        if text == "true":
            return True
        if text == "false":
            return False
        if text == "null":
            return None
        if text.startswith(":") and len(text) > 1:
            return _Kwarg(text[1:])
        if _IDENT.match(text):
            if text in self.context:
                return self.context[text]
            raise self.error(f"name '{text}' is not defined", source, offset)
        if _SCI.match(text) or _REAL.match(text):
            return float(text)
        if _INT.match(text):
            return int(text)
        raise self.error(f"Expected an operand, found '{text}'", source, offset)

    def evaluate(self, node: list[Any], source: str) -> Any:
        """Evaluates an expression `(function operands...)`.

        Args:
            node: The expression.
            source: The source, for errors.

        Returns:
            The value.

        Raises:
            ExpressionError: If the expression is malformed.
        """
        start = node[0][2]
        items = node[1:]
        if not items:
            raise self.error("expected a function or operator", source, start + 1)
        head = items[0]
        rest = items[1:]
        if isinstance(head, list) or head[0] != "atom":
            raise self.error("expected a function or operator", source, start + 1)
        name = head[1]
        if name in self.higher:
            func: Callable[..., Any] = self.higher[name]
            args: list[Any] = []
            if rest and not isinstance(rest[0], list) and rest[0][0] == "atom":
                first = rest[0][1]
                if first in self.ops:
                    args.append(self.ops[first])
                    rest = rest[1:]
                elif first == "null":
                    args.append(None)
                    rest = rest[1:]
                elif _IDENT.match(first) and first not in ("true", "false"):
                    args.append(self.func(first))
                    rest = rest[1:]
            values = args + [self.operand(x, source) for x in rest]
        else:
            if name in self.ops:
                func = self.ops[name]
            elif re.match(r"^[A-Za-z0-9_]+$", name):
                func = self.func(name)
            else:
                raise self.error(f"'{name}' is not a function or operator", source, head[2])
            if not rest:
                raise self.error("Expected an operand", source, head[2] + len(name))
            values = [self.operand(x, source) for x in rest]
        positional: list[Any] = []
        kwds: dict[str, Any] = {}
        it = iter(values)
        for v in it:
            if isinstance(v, _Kwarg):
                try:
                    kwds[v.name] = next(it)
                except StopIteration:
                    raise self.error(f"keyword ':{v.name}' has no value", source, start) from None
            else:
                positional.append(v)
        try:
            return func(*positional, **kwds)
        except TypeError:
            # snuggs retries builtins taking one iterable (`(list 1 2)`) with the arguments as a
            # list; other functions' errors are reported as they are.
            if getattr(builtins, getattr(func, "__name__", ""), None) is not func:
                raise
            return func(positional, **kwds)

    def eval(self, source: str) -> Any:
        """Evaluates an expression.

        Args:
            source: The expression.

        Returns:
            The value.
        """
        return self.evaluate(self.parse(source), source)


def cmd_calc(args: argparse.Namespace, env: Any) -> None:
    """`calc`: evaluates a snuggs expression of rasters and writes the result.

    Args:
        args: The parsed arguments.
        env: The environment.

    Raises:
        CommandError: If the expression is invalid.
    """
    import numpy

    import terrio.rio as rasterio
    from terrio.rio.features import sieve
    from terrio.rio.windows import Window, subdivide

    creation_options = key_val(args.creation_options)
    dst = None
    with env, ExitStack() as stack:
        output, files = resolve_inout(
            files=args.files, output=args.output, overwrite=args.overwrite
        )
        inputs = [tuple(n.split("=")) for n in args.name] + [(None, n) for n in files]
        sources = [stack.enter_context(rasterio.open(path)) for _, path in inputs]

        def get_bands(d: Any, i: Any = None) -> Any:
            idx = d if d in dict(inputs) else int(d) - 1
            src = sources[idx]
            return rasterio.band(src, i) if i else [rasterio.band(src, j) for j in src.indexes]

        def asarray(*a: Any) -> Any:
            if len(a) == 1 and hasattr(a[0], "__iter__"):
                return numpy.asanyarray(list(a[0]))
            return numpy.asanyarray(list(a))

        def fillnodata(*a: Any, **k: Any) -> Any:
            raise CommandError("fillnodata is not supported by terrio.rio")

        evaluator: Snuggs

        def read_array(ix: Any, subix: Any = None, dtype: Any = None) -> Any:
            arr = evaluator.lookup(ix, subix)
            return arr.astype(dtype) if dtype else arr

        funcs: dict[str, Callable[..., Any]] = {
            "asarray": asarray,
            "take": lambda a, idx: numpy.take(a, idx - 1, axis=0),
            "read": read_array,
            "band": lambda d, i: get_bands(d, i),
            "bands": lambda d: get_bands(d),
            "fillnodata": fillnodata,
            "sieve": sieve,
        }
        first = sources[0]
        kwargs = first.profile
        kwargs.update(**creation_options)
        dtype = args.dtype or first.meta["dtype"]
        kwargs["dtype"] = dtype
        kwargs.pop("driver", None)
        if args.driver:
            kwargs["driver"] = args.driver
        work_windows = [Window(0, 0, 16, 16)]
        for window in work_windows:
            ctxkwds: OrderedDict[str, Any] = OrderedDict()
            for i, ((name, _), src) in enumerate(zip(inputs, sources, strict=True)):
                ctxkwds[name or f"_i{i + 1}"] = src.read(masked=args.masked, window=window)
            evaluator = Snuggs(funcs, ctxkwds)
            try:
                res = evaluator.eval(args.command)
            except ExpressionError as err:
                echo("Expression Error:")
                echo(f"  {err.text}")
                echo(" {}^".format(" " * (err.offset or 0)))
                echo(err)
                raise CommandError("Aborted!") from None
            results = res.astype(dtype)
            if isinstance(results, numpy.ma.MaskedArray):
                # Masked pixels take the nodata value, or 0 when the first input has none.
                nodata = kwargs.get("nodata")
                results = results.filled(0 if nodata is None else float(nodata))
                if len(results.shape) == 2:
                    results = numpy.ma.asanyarray([results])
            elif len(results.shape) == 2:
                results = numpy.asanyarray([results])
            if dst is None:
                kwargs["count"] = results.shape[0]
                dst = stack.enter_context(rasterio.open(output, "w", **kwargs))
                max_pixels = (
                    args.mem_limit * 1.0e6 / (numpy.dtype(dst.dtypes[0]).itemsize * dst.count)
                )
                chunk_size = int(math.floor(math.sqrt(max_pixels)))
                work_windows.extend(
                    subdivide(Window(0, 0, dst.width, dst.height), chunk_size, chunk_size)
                )
            else:
                dst.write(results, window=window)


def add_calc(sub: Any) -> None:
    """Registers `calc`.

    Args:
        sub: The subparsers.
    """
    p = command(
        sub,
        "calc",
        cmd_calc,
        "Raster data calculator.",
        "A raster data calculator: evaluates a lisp-like (snuggs) expression of input datasets and "
        "writes the result to a new dataset.\n\n"
        "  (read i) is the i-th input (a 3-D array); (read i j) its j-th band; (read i j "
        "'float64') cast;\n"
        "  (take foo j) the j-th band of an input named with --name foo=path;\n"
        "  +, -, *, /, &, |, <, <=, ==, !=, >=, > and the methods of NumPy arrays; asarray makes a "
        "3-D array.\n\n"
        '  terrio-rio calc "(+ 2 (* 0.95 (read 1)))" in.tif out.tif',
    )
    p.add_argument("command", metavar="COMMAND")
    p.add_argument("files", nargs="+", metavar="INPUTS... OUTPUT")
    opt_output(p)
    opt_format(p)
    p.add_argument(
        "--name",
        action="append",
        default=[],
        help=(
            "Specify an input file with a unique short (alphas only) name for use in commands like "
            '"a=tests/data/RGB.byte.tif".'
        ),
    )
    opt_dtype(p)
    flag_pair(
        p,
        "--masked",
        "--not-masked",
        "masked",
        True,
        "Evaluate expressions using masked arrays (the default) or ordinary numpy arrays.",
    )
    opt_overwrite(p)
    p.add_argument(
        "--mem-limit",
        type=int,
        default=64,
        help="Limit on memory used to perform calculations, in MB.",
    )
    opt_co(p)


# ─── Main ───────────────────────────────────────────────────────────────────────────────────────


class _Parser(argparse.ArgumentParser):
    """An argument parser with click's help option (`--help` only, so `-h` stays free)."""

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        """Creates the parser.

        Args:
            *args: Arguments of `ArgumentParser`.
            **kwargs: Keywords of `ArgumentParser`.
        """
        kwargs.setdefault("add_help", False)
        kwargs.setdefault("allow_abbrev", False)
        kwargs.setdefault("formatter_class", argparse.RawDescriptionHelpFormatter)
        super().__init__(*args, **kwargs)
        self.add_argument("--help", action="help", help="Show this message and exit.")


class _Commands:
    """The commands' parsers, by name, and their one-line helps."""

    def __init__(self) -> None:
        """Creates the registry."""
        self.parsers: dict[str, argparse.ArgumentParser] = {}
        self.helps: dict[str, str] = {}

    def add_parser(self, name: str, help: str, description: str) -> argparse.ArgumentParser:
        """Adds a command's parser.

        Args:
            name: The command.
            help: Its one-line help.
            description: Its help text.

        Returns:
            ArgumentParser: The parser.
        """
        p = _Parser(prog=f"{PROG} {name}", description=description)
        self.parsers[name] = p
        self.helps[name] = help
        return p


def command(
    sub: _Commands,
    name: str,
    func: Callable[[argparse.Namespace, Any], None],
    short: str,
    description: str,
) -> argparse.ArgumentParser:
    """Adds a command's parser.

    Args:
        sub: The registry of commands.
        name: The command.
        func: Its implementation.
        short: Its one-line help.
        description: Its help text.

    Returns:
        ArgumentParser: The command's parser.
    """
    p = sub.add_parser(name, help=short, description=description)
    p.set_defaults(func=func)
    return p


COMMANDS = [
    add_blocks,
    add_bounds,
    add_calc,
    add_clip,
    add_convert,
    add_create,
    add_edit_info,
    add_env,
    add_gcps,
    add_info,
    add_insp,
    add_mask,
    add_merge,
    add_overview,
    add_rasterize,
    add_rm,
    add_sample,
    add_shapes,
    add_stack,
    add_transform,
    add_warp,
]


def parser() -> tuple[argparse.ArgumentParser, dict[str, argparse.ArgumentParser]]:
    """The command line's parsers.

    Returns:
        tuple: The parser of the global options, and the parsers of the commands by name.
    """
    from terrio import __version__

    commands = _Commands()
    for add in COMMANDS:
        add(commands)
    width = max(len(n) for n in commands.helps)
    listing = "\n".join(f"  {n:<{width}}  {h}" for n, h in commands.helps.items())
    p = _Parser(
        prog=PROG,
        usage=f"{PROG} [OPTIONS] COMMAND [ARGS]...",
        description="terrio.rio command line interface: rasterio's rio commands, run with terrio.",
        epilog=f"Commands:\n{listing}",
    )
    p.add_argument("-v", "--verbose", action="count", default=0, help="Increase verbosity.")
    p.add_argument("-q", "--quiet", action="count", default=0, help="Decrease verbosity.")
    p.add_argument("--aws-profile", help="Select a profile from the AWS credentials file")
    p.add_argument("--aws-no-sign-requests", action="store_true", help="Make requests anonymously")
    p.add_argument(
        "--aws-requester-pays", action="store_true", help="Requester pays data transfer costs"
    )
    p.add_argument(
        "--version", action="version", version=__version__, help="Show the version and exit."
    )
    p.add_argument(
        "--gdal-version", action="store_true", help="terrio does not use GDAL: says so and exits."
    )
    p.add_argument("--show-versions", action="store_true", help="Show dependency versions")
    return p, commands.parsers


def split_argv(argv: Sequence[str], commands: dict[str, Any]) -> tuple[list[str], list[str]]:
    """Splits the arguments into the global options and the command with its arguments.

    Args:
        argv: The arguments.
        commands: The command names.

    Returns:
        tuple: The global options, and the command and its arguments (empty without a command).
    """
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--aws-profile":
            i += 2
            continue
        if not a.startswith("-"):
            return list(argv[:i]), list(argv[i:])
        i += 1
    return list(argv), []


def show_versions() -> None:
    """Prints the versions of terrio, Python and the dependencies."""
    import platform

    import affine
    import numpy

    from terrio import __version__

    echo("terrio.rio info:")
    echo(f"  terrio: {__version__}")
    echo("    GDAL: none (terrio reads and writes the formats itself)")
    echo("")
    echo("System:")
    echo(f"  python: {sys.version.splitlines()[0]}")
    echo(f"executable: {sys.executable}")
    echo(f" machine: {platform.platform()}")
    echo("")
    echo("Python deps:")
    echo(f"   numpy: {numpy.__version__}")
    echo(f"  affine: {affine.__version__}")


def main(argv: Sequence[str] | None = None) -> int:
    """Runs the command line.

    Args:
        argv: The arguments (by default, the process's).

    Returns:
        int: The exit status: 0 on success, 1 when a command fails, 2 for a usage error.
    """
    import terrio.rio as rasterio
    from terrio.rio.session import AWSSession

    p, commands = parser()
    argv = list(sys.argv[1:] if argv is None else argv)
    head, rest = split_argv(argv, commands)
    try:
        g = p.parse_args(head)
        if g.gdal_version:
            print(f"{PROG}: terrio does not use GDAL", file=sys.stderr)
            return 1
        if g.show_versions:
            show_versions()
            return 0
        if not rest:
            p.print_help()
            return 0
        name = rest[0]
        if name not in commands:
            p.error(f"No such command '{name}'.")
        sub = commands[name]
        args = sub.parse_intermixed_args(rest[1:])
    except SystemExit as e:
        return e.code if isinstance(e.code, int) else (0 if e.code is None else 2)
    verbosity = g.verbose - g.quiet
    logging.basicConfig(stream=sys.stderr, level=max(10, 30 - 10 * verbosity))
    if g.aws_profile or g.aws_no_sign_requests or g.aws_requester_pays:
        env = rasterio.Env(
            session=AWSSession(
                profile_name=g.aws_profile,
                aws_unsigned=g.aws_no_sign_requests,
                requester_pays=g.aws_requester_pays,
            )
        )
    else:
        env = rasterio.Env()
    try:
        args.func(args, env)
    except UsageError as e:
        sub.print_usage(sys.stderr)
        print(f"{PROG} {name}: error: {e}", file=sys.stderr)
        return 2
    except BrokenPipeError:
        return 0
    except Exception as e:
        print(f"{PROG} {name}: error: {e}", file=sys.stderr)
        if verbosity > 0:
            import traceback

            traceback.print_exc()
        return 1
    return 0


__all__ = ["main", "parser"]

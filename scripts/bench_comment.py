#!/usr/bin/env python3
"""Write the pull-request comment of the `bench` workflow (Markdown, on stdout).

Reads the JSON summaries `cargo bench -p terrio --bench instructions` and `cargo bench -p
terrio-cli --bench commands` print with `-- --baseline=main --output-format=json` (one object per
benchmark, concatenated) and compares three measures of each
benchmark with the `main` baseline: CPU instructions (Callgrind's `Ir`), estimated cycles
(`EstimatedCycles`, the speed) and the peak heap of the operation (reported to DHAT as
ad-hoc `TotalUnits`, the memory). A change
of 10% or more is flagged, and for memory only when it is also 1 MB or more; an increase makes
the comment carry a `<!-- terrio-bench: regression -->` line, which bench-comment.yml reads to set
the `perf-regression` label.

The comment says in its heading how many benchmarks got worse, better or match main, then lists
each flagged measure with a bar, and folds the rest into a table by area: a `terrio` command's
area is its section of `crates/terrio-cli/benches/cases.txt`.

    python3 scripts/bench_comment.py pr.json --base <sha> --head <sha>
"""

import argparse
import json
import re
import sys
from pathlib import Path

FLAG = 10.0  # percent
MEMORY_FLOOR = 1_000_000  # bytes: a smaller memory change is not flagged, whatever its percent

# (column, tool, metric) of each measure, in table order.
MEASURES = (("CPU", "Callgrind", "Ir"), ("speed", "Callgrind", "EstimatedCycles"), ("memory", "DHAT", "TotalUnits"))


def summaries(text):
    """The JSON objects of `text`, concatenated or one per line."""
    decoder, i, out = json.JSONDecoder(), 0, []
    while True:
        while i < len(text) and text[i].isspace():
            i += 1
        if i == len(text):
            return out
        value, i = decoder.raw_decode(text, i)
        out.extend(value if isinstance(value, list) else [value])


def metrics(summary):
    """The (main, pull request) pair of each of MEASURES in a benchmark summary: main is None
    without a baseline, and the pair is None when this run did not report the metric."""
    found = {p["tool"]: p["data"]["total"]["metrics"] for p in summary["profiles"]}
    pairs = []
    for _, tool, key in MEASURES:
        values = found.get(tool, {}).get(key, {}).get("values")
        if values is not None and values.get("new") is None:  # only main has it
            values = None
        old = None if values is None else values.get("old")
        pairs.append(None if values is None else (None if old is None else int(old), int(values["new"])))
    if pairs[0] is None:
        raise ValueError(f"no Callgrind instructions in {summary.get('function_name')}")
    return pairs


def count(n, unit=""):
    """`n` with an SI prefix, three significant digits; the prefix is chosen after rounding, so
    999,999 reads 1 M, not 1e+03 k."""
    for div, prefix in ((1e9, " G"), (1e6, " M"), (1e3, " k")):
        if float(f"{n / div:.3g}") >= 1:
            return f"{n / div:.3g}{prefix}{unit}"
    return f"{n} {unit}".rstrip()


CASES = Path(__file__).resolve().parents[1] / "crates" / "terrio-cli" / "benches" / "cases.txt"

# The area of each library benchmark function, named as the sections of CASES.
LIBRARY_AREA = {
    "warp_f32": "Reprojection",
    "dem_f32": "Terrain",
    "read_geotiff_f32": "GeoTIFF decoding",
    "zonal_stats_f32": "Vectors and zonal statistics",
}

# What each benchmark runs (crates/terrio/benches/instructions.rs), for readers who don't know the
# bench names. A benchmark missing here shows its name alone.
WHAT = {
    "warp_f32 reproject_near": "Reproject 1 M pixels, EPSG:4326 → UTM 31N, nearest",
    "warp_f32 reproject_bilinear": "Reproject 1 M pixels, EPSG:4326 → UTM 31N, bilinear",
    "warp_f32 reproject_cubic": "Reproject 1 M pixels, EPSG:4326 → UTM 31N, cubic",
    "warp_f32 reproject_near_gdal_grid": "Reproject as above, nearest, with GDAL's chunk-dependent sampling",
    "warp_f32 half_pixel_shift_near": "Shift 1 M pixels by half a pixel, no CRS change, nearest",
    "dem_f32 slope": "Slope of a 1 M-pixel DEM",
    "dem_f32 hillshade": "Hillshade of a 1 M-pixel DEM",
    "read_geotiff_f32 deflate_predictor3": "Read a tiled DEFLATE GeoTIFF, floating-point predictor, 1 M pixels",
    "zonal_stats_f32 octagons": "Zonal statistics of 1024 octagons over 1 M pixels",
}


def percent(pct):
    """A change as text: two decimals under 1%, so small drifts show; "new" from nothing."""
    if pct == float("inf"):
        return "new"
    text = "0%" if round(pct, 2) == 0 else f"{pct:+.2f}%" if abs(pct) < 1 else f"{pct:+.1f}%"
    return text.replace("-", "−")


def change(old, new):
    """The change in percent, rounded as it is shown, so a row flagged at 10% reads 10.0% or more,
    and as text."""
    if not old:
        pct = float("inf") if new else 0.0
    else:
        pct = (new - old) / old * 100.0
        pct = round(pct, 2) if abs(pct) < 1 else round(pct, 1)
    return pct, percent(pct)


def commands():
    """{case: (area, command)} of CASES: the area is the first words of the case's section comment,
    the command its `terrio` arguments without the input and output paths (in braces)."""
    out, area = {}, "Commands"
    try:
        text = CASES.read_text(encoding="utf-8")
    except OSError:
        return out
    for line in map(str.strip, text.splitlines()):
        if line.startswith("# "):
            area = re.split(r"[,.:]", line[2:], maxsplit=1)[0]
        elif line and not line.startswith("#"):
            name, _, args = line.partition(" ")
            out[name] = (area, " ".join(["terrio", *(a for a in args.split() if "{" not in a)]))
    return out


def bar(old, new):
    """main's value as ten blocks, plus a ▒ per 10% more, or with a ░ per 10% less."""
    if not old:
        return ""
    tenths = round(new / old * 10)
    if tenths >= 10:
        return "`" + "█" * 10 + "▒" * min(tenths - 10, 20) + ("…" if tenths > 30 else "") + "`"
    return "`" + "█" * tenths + "░" * (10 - tenths) + "`"


def signed(n, unit):
    return ("+" if n >= 0 else "−") + count(abs(n), unit)


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("summaries", help="the JSON output of the pull request's run")
    parser.add_argument("--base", required=True, help="main's commit")
    parser.add_argument("--head", required=True, help="the pull request's commit")
    args = parser.parse_args()
    with open(args.summaries, encoding="utf-8") as f:
        found = summaries(f.read())
    cases = commands()

    benches = []
    for s in found:
        function, case = s["function_name"], s.get("id")
        name = f"{function} {case}" if case else function
        if function == "cli" and case in cases:
            area, command = cases[case]
            label = f"`{command}`<br><sub>{area} · `{name}`</sub>"
        else:
            area = LIBRARY_AREA.get(function, "Other")
            what = WHAT.get(name)
            label = f"{what}<br><sub>{area} · `{name}`</sub>" if what else f"`{name}`<br><sub>{area}</sub>"
        cells = []  # (column, unit, pair, percent, text, +1 worse / -1 better / 0)
        for (column, _, _), pair in zip(MEASURES, metrics(s)):
            unit = "B" if column == "memory" else ""
            if pair is None or pair[0] is None:
                cells.append((column, unit, pair, None, None, 0))
                continue
            pct, text = change(*pair)
            flag = abs(pct) >= FLAG and (column != "memory" or abs(pair[1] - pair[0]) >= MEMORY_FLOOR)
            cells.append((column, unit, pair, pct, text, (1 if pct > 0 else -1) if flag else 0))
        worse = max((c[3] for c in cells if c[5] > 0), default=None)
        better = min((c[3] for c in cells if c[5] < 0), default=None)
        state = "new" if cells[0][2][0] is None else "worse" if worse is not None else "better" if better is not None else "same"
        benches.append({"name": name, "area": area, "label": label, "cells": cells, "state": state, "key": (worse, better)})

    n = {k: sum(b["state"] == k for b in benches) for k in ("worse", "better", "same", "new")}
    new = f" · 🆕 {n['new']} new" if n["new"] else ""
    out = ["<!-- terrio-bench: regression -->"] if n["worse"] else []
    if n["worse"] or n["better"]:
        moved = ", ".join(f"{n[k]} {k}" for k in ("worse", "better") if n[k])
        out.append(f"### {'⚠️' if n['worse'] else '✅'} Performance: {moved} · {n['same']} match main{new}")
    elif n["same"]:
        out += [f"### 🟢 Performance holds: all {n['same']} benchmarks match main{new}", "CPU, speed and memory all match main."]
    else:
        out.append(f"### 🆕 Performance: {n['new']} new benchmarks, none on main yet")

    # Each flagged measure, worse first, the largest change first.
    flagged = sorted(
        (b for b in benches if b["state"] in ("worse", "better")),
        key=lambda b: (0, -b["key"][0]) if b["state"] == "worse" else (1, b["key"][1]),
    )
    if flagged:
        out += ["", "| | benchmark | measure | main → PR | change | |", "|---|---|---|---:|---:|---|"]
        for b in flagged:
            first = True
            for column, unit, pair, pct, text, flag in b["cells"]:
                if not flag:
                    continue
                old, new_value = pair
                if column == "memory" and pct != float("inf"):
                    text = f"{signed(new_value - old, unit)} ({text})"
                lead = f"| {'⚠️' if b['state'] == 'worse' else '✅'} | {b['label']} |" if first else "| | |"
                first = False
                out.append(f"{lead} {column} | {count(old, unit)} → {count(new_value, unit)} | **{text}** | {bar(old, new_value)} |")

    # One row per area, the flagged areas first.
    areas = {}
    for b in benches:
        areas.setdefault(b["area"], []).append(b)
    order = {"worse": 0, "better": 1, "new": 2, "same": 3}
    rows = []
    for area, bs in areas.items():
        compared = [b for b in bs if b["state"] != "new"]
        cells = []
        for i in range(len(MEASURES)):
            w = sum(b["cells"][i][5] > 0 for b in compared)
            g = sum(b["cells"][i][5] < 0 for b in compared)
            parts = ([f"⚠️ {w} worse"] if w else []) + ([f"✅ {g} better"] if g else [])
            cells.append(" · ".join(parts) or ("✓" if compared else "🆕"))
        state = min((b["state"] for b in compared), key=order.get, default="new")
        mark = {"worse": "⚠️", "better": "✅", "new": "🆕", "same": "🟢"}[state]
        k = len(bs) - len(compared)
        count_text = f"{len(bs)}" + (f" ({k} new)" if k else "")
        rows.append((order[state], f"| {mark} {area} | {count_text} | {' | '.join(cells)} |"))
    out += [
        "",
        f"<details><summary>All {len(benches)} benchmarks, by area</summary>",
        "",
        "| area | benchmarks | CPU | speed | memory |",
        "|---|:-:|:-:|:-:|:-:|",
        *(r for _, r in sorted(rows, key=lambda r: r[0])),
        "</details>",
    ]

    # Every benchmark's values.
    def value(column, unit, pair, pct, text, flag):
        if pair is None:
            return ""
        if pair[0] is None:
            return f"{count(pair[1], unit)} (new)"
        shown = count(pair[1], unit) if pair[0] == pair[1] else f"{count(pair[0], unit)} → {count(pair[1], unit)}"
        return f"{shown} ({f'**{text}**' if flag else text})"

    marks = {"worse": "⚠️", "better": "✅", "new": "🆕", "same": "🟢"}
    out += [
        "",
        "<details><summary>Every benchmark's values</summary>",
        "",
        "| | benchmark | CPU (instructions) | speed (est. cycles) | memory (peak heap) |",
        "|---|---|---:|---:|---:|",
        *(f"| {marks[b['state']]} | `{b['name']}` | {' | '.join(value(*c) for c in b['cells'])} |" for b in benches),
        "</details>",
        "",
        "<details><summary>How to read this</summary>",
        "",
        "- **CPU**: instructions executed. **Speed**: estimated cycles, instructions plus cache and RAM misses. "
        "**Memory**: the peak heap of the operation beyond its input, its output included.",
        "- Memory repeats exactly between runs, instructions within 0.01% and est. cycles within 0.1%: "
        "a flagged change comes from the code.",
        "- The bar is main as ten blocks, plus a ▒ per 10% more, or with a ░ per 10% less.",
        "- Library benchmarks run on 1 M synthetic pixels; `terrio` commands run in-process on 512 x 512 "
        "synthetic rasters and 1024 polygons, with their input and output paths left out here.",
        "- Run them locally: `cargo bench -p terrio --bench instructions` and "
        "`cargo bench -p terrio-cli --bench commands`.",
        "</details>",
        "",
        (
            f"<sub>main `{args.base[:7]}` → PR `{args.head[:7]}` · one worker thread, under Valgrind · "
            f"flagged at {FLAG:g}% ({count(MEMORY_FLOOR, 'B')} for memory) · informational, never fails CI</sub>"
        ),
    ]
    sys.stdout.write("\n".join(out) + "\n")


if __name__ == "__main__":
    main()

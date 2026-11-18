#!/usr/bin/env python3
"""Write the pull-request comment of the `bench` workflow (Markdown, on stdout).

Reads the JSON summaries `cargo bench -p terrio --bench instructions -- --baseline=main
--output-format=json` prints (one object per benchmark) and compares three measures of each
benchmark with the `main` baseline: CPU instructions (Callgrind's `Ir`), estimated cycles
(`EstimatedCycles`, the speed) and the peak heap of the operation (reported to DHAT as
ad-hoc `TotalUnits`, the memory). A change
of 10% or more is flagged, and for memory only when it is also 1 MB or more; an increase makes
the comment carry a `<!-- terrio-bench: regression -->` line, which bench-comment.yml reads to set
the `perf-regression` label.

    python3 scripts/bench_comment.py pr.json --base <sha> --head <sha>
"""

import argparse
import json
import sys

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
    without a baseline, and the pair is None when the tool did not report the metric."""
    found = {p["tool"]: p["data"]["total"]["metrics"] for p in summary["profiles"]}
    pairs = []
    for _, tool, key in MEASURES:
        values = found.get(tool, {}).get(key, {}).get("values")
        old = None if values is None else values.get("old")
        pairs.append(None if values is None else (None if old is None else int(old), int(values["new"])))
    if pairs[0] is None:
        raise ValueError(f"no Callgrind instructions in {summary.get('function_name')}")
    return pairs


def count(n, unit=""):
    """`n` with an SI prefix, three significant digits."""
    for div, prefix in ((1e9, " G"), (1e6, " M"), (1e3, " k")):
        if n >= div:
            return f"{n / div:.3g}{prefix}{unit}"
    return f"{n} {unit}".rstrip()


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


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("summaries", help="the JSON output of the pull request's run")
    parser.add_argument("--base", required=True, help="main's commit")
    parser.add_argument("--head", required=True, help="the pull request's commit")
    args = parser.parse_args()
    with open(args.summaries, encoding="utf-8") as f:
        rows = [(s["function_name"] + (f" {s['id']}" if s.get("id") else ""), metrics(s)) for s in summaries(f.read())]

    table = []  # (sort key, mark, table line, label, flagged changes, largest change)
    for n, (name, pairs) in enumerate(rows):
        cells, worse, better, big = [], [], [], (0.0, "")
        for (column, _, _), pair in zip(MEASURES, pairs):
            unit = "B" if column == "memory" else ""
            if pair is None:
                cells.append("")
            elif pair[0] is None:
                cells.append(f"{count(pair[1], unit)} (new)")
            else:
                pct, text = change(*pair)
                flag = abs(pct) >= FLAG and (column != "memory" or abs(pair[1] - pair[0]) >= MEMORY_FLOOR)
                if flag:
                    (worse if pct > 0 else better).append((pct, column))
                big = max(big, (pct, column), key=lambda c: abs(c[0]))
                values = count(pair[1], unit) if pair[0] == pair[1] else f"{count(pair[0], unit)} → {count(pair[1], unit)}"
                cells.append(f"{values} ({f'**{text}**' if flag else text})")
        mark = "🆕" if pairs[0][0] is None else "⚠️" if worse else "✅" if better else "🟢"
        what = WHAT.get(name)
        label = f"{what}<br>`{name}`" if what else f"`{name}`"
        # Worse, then better, the largest change first; the others in bench order.
        key = (0, -max(worse)[0], n) if mark == "⚠️" else (1, min(better)[0], n) if mark == "✅" else (2, 0.0, n)
        line = f"| {mark} | {label} | {' | '.join(cells)} |"
        table.append((key, mark, line, what or name, worse if mark == "⚠️" else better, big))
    table.sort(key=lambda r: r[0])

    worse = [r for r in table if r[1] == "⚠️"]
    better = [r for r in table if r[1] == "✅"]
    compared = sum(r[1] != "🆕" for r in table)

    def listed(rs):
        def measure(p, c):
            return f"**{c} from 0**" if p == float("inf") else f"**{percent(p)} {c}**"

        return "".join(f"\n- {r[3]}: {', '.join(measure(p, c) for p, c in r[4])}" for r in rs)

    out = ["<!-- terrio-bench: regression -->"] if worse else []
    out.append("### Performance vs main")
    if not compared:
        out.append(f"🆕 main has no benchmarks yet: these are this PR's counts, {len(table)} benchmarks.")
    if worse:
        out.append(f"⚠️ **{len(worse)} of {compared} benchmarks got worse** than main:{listed(worse)}")
    if better:
        out += ([""] if worse else []) + [f"✅ **{len(better)} of {compared} got better**:{listed(better)}"]
    if compared and not worse and not better:
        top = max((r for r in table if r[1] != "🆕"), key=lambda r: abs(r[5][0]))
        out.append(
            f"🟢 **As fast and as lean as main.** All {compared} benchmarks are within ±{FLAG:g}% on CPU, "
            f"speed and memory; the largest change is {percent(top[5][0])} {top[5][1]} ({top[3]})."
        )
    # Only the flagged rows stay in view; the others fold away.
    header = [
        "| | benchmark | CPU (instructions) | speed (est. cycles) | memory (peak heap) |",
        "|---|---|---:|---:|---:|",
    ]
    flagged = [r[2] for r in table if r[1] in ("⚠️", "✅")]
    rest = [r[2] for r in table if r[1] not in ("⚠️", "✅")]
    if flagged:
        out += ["", *header, *flagged]
    if rest:
        green = sum(r[1] == "🟢" for r in table)
        label = f"🟢 {green} within ±{FLAG:g}%" if green else f"🆕 {len(rest)} benchmarks"
        out += ["", f"<details><summary>{label}</summary>", "", *header, *rest, "</details>"]
    out += [
        "",
        "<details><summary>How to read this</summary>",
        "",
        (
            f"- Counted under Valgrind for main `{args.base[:7]}` and this PR `{args.head[:7]}`, "
            "with one worker thread, on synthetic inputs."
        ),
        (
            "- **CPU**: the instructions executed, the work done (Callgrind). **Speed**: estimated cycles, "
            "instructions plus cache and RAM misses, an estimate of the time taken (Callgrind). "
            "**Memory**: the peak heap, the most memory the operation holds at once beyond its "
            "input, its output included."
        ),
        (
            "- Memory repeats exactly between runs, instructions within 0.01% and est. cycles within "
            "0.1%: a larger change comes from the code."
        ),
        (
            f"- ⚠️ worse or ✅ better by {FLAG:g}% or more (and by {count(MEMORY_FLOOR, 'B')} or more for "
            "memory); 🟢 within. Informational: this check never fails."
        ),
        "- Run them locally: `cargo bench -p terrio --bench instructions`.",
        "</details>",
    ]
    sys.stdout.write("\n".join(out) + "\n")


if __name__ == "__main__":
    main()

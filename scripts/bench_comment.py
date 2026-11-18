#!/usr/bin/env python3
"""Write the pull-request comment of the `bench` workflow (Markdown, on stdout).

Reads the JSON summaries `cargo bench -p terrio --bench instructions -- --baseline=main
--output-format=json` prints (one object per benchmark) and compares the CPU instructions
(Callgrind's `Ir`) and estimated cycles (`EstimatedCycles`) of each benchmark with the `main`
baseline. A change of 10% or more in either is flagged; an increase makes the comment carry a
`<!-- terrio-bench: regression -->` line, which bench-comment.yml reads to set the
`perf-regression` label.

    python3 scripts/bench_comment.py pr.json --base <sha> --head <sha>
"""

import argparse
import json
import sys

FLAG = 10.0  # percent


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
    """((main, pull request) instructions, (main, pull request) estimated cycles) of a benchmark
    summary; main is None without a baseline, and the cycles pair is None when Callgrind has none."""
    for profile in summary["profiles"]:
        if profile["tool"] == "Callgrind":
            found = profile["data"]["total"]["metrics"]
            pairs = []
            for key in ("Ir", "EstimatedCycles"):
                values = found.get(key, {}).get("values")
                old = None if values is None else values.get("old")
                pairs.append(None if values is None else (None if old is None else int(old), int(values["new"])))
            if pairs[0] is None:
                break
            return tuple(pairs)
    raise ValueError(f"no Callgrind instructions in {summary.get('function_name')}")


def count(n):
    """`n` with an SI prefix, three significant digits."""
    for div, unit in ((1e9, " G"), (1e6, " M"), (1e3, " k")):
        if n >= div:
            return f"{n / div:.3g}{unit}"
    return str(n)


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


def change(old, new):
    """The change in percent, and as text: two decimals under 1%, so small drifts show."""
    pct = (new - old) / old * 100.0 if old else float("inf")
    text = "0%" if pct == 0 else f"{pct:+.2f}%" if abs(pct) < 1 else f"{pct:+.1f}%"
    return pct, text.replace("-", "−")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("summaries", help="the JSON output of the pull request's run")
    parser.add_argument("--base", required=True, help="main's commit")
    parser.add_argument("--head", required=True, help="the pull request's commit")
    args = parser.parse_args()
    with open(args.summaries, encoding="utf-8") as f:
        rows = [(s["function_name"] + (f" {s['id']}" if s.get("id") else ""), *metrics(s)) for s in summaries(f.read())]

    table = []  # (sort key, mark, table line, label, largest change)
    for n, (name, ir, cycles) in enumerate(rows):
        cells, up, down, big = [], 0.0, 0.0, 0.0
        for pair in (ir, cycles):
            if pair is None:
                cells.append("")
            elif pair[0] is None:
                cells.append(f"{count(pair[1])} (new)")
            else:
                pct, text = change(*pair)
                up, down, big = max(up, pct), min(down, pct), max(big, pct, key=abs)
                cells.append(f"{count(pair[0])} → {count(pair[1])} ({f'**{text}**' if abs(pct) >= FLAG else text})")
        mark = "🆕" if ir[0] is None else "⚠️" if up >= FLAG else "✅" if down <= -FLAG else "🟢"
        what = WHAT.get(name)
        label = f"{what}<br>`{name}`" if what else f"`{name}`"
        # Slower, then faster, the largest change first; the others in bench order.
        key = {"⚠️": (0, -up), "✅": (1, down)}.get(mark, (2, 0.0)) + (n,)
        table.append((key, mark, f"| {mark} | {label} | {cells[0]} | {cells[1]} |", what or name, big))
    table.sort(key=lambda r: r[0])

    slower = [r for r in table if r[1] == "⚠️"]
    faster = [r for r in table if r[1] == "✅"]
    compared = sum(r[1] != "🆕" for r in table)

    def listed(rs):
        return "; ".join(f"{r[3]} {change(100, 100 + r[4])[1]}" for r in rs)

    out = ["<!-- terrio-bench: regression -->"] if slower else []
    out.append("### Performance vs main")
    if not compared:
        out.append(f"🆕 main has no benchmarks yet: these are this PR's counts, {len(table)} benchmarks.")
    if slower:
        out.append(f"⚠️ **{len(slower)} of {compared} benchmarks are slower** than main: {listed(slower)}.")
    if faster:
        out.append(f"✅ **{len(faster)} of {compared} are faster**: {listed(faster)}.")
    if compared and not slower and not faster:
        top = max((r for r in table if r[1] != "🆕"), key=lambda r: abs(r[4]))
        out.append(
            f"🟢 **No slowdown.** All {compared} benchmarks are within ±{FLAG:g}% of main; "
            f"the largest change is {change(100, 100 + top[4])[1]} ({top[3]})."
        )
    out += [
        "",
        "| | benchmark | CPU instructions | est. cycles |",
        "|---|---|---:|---:|",
        *(r[2] for r in table),
        "",
        "<details><summary>How to read this</summary>",
        "",
        (f"- Counted under Valgrind (Callgrind) for main `{args.base[:7]}` and this PR `{args.head[:7]}`, "
         "with one worker thread, on synthetic inputs."),
        ("- **CPU instructions**: the work done. **Est. cycles**: instructions plus cache and RAM misses, "
         "an estimate of the time taken."),
        "- Both repeat within 0.01% between runs, so a change beyond that comes from the code, not from noise.",
        (f"- ⚠️ slower or ✅ faster by {FLAG:g}% or more on either; 🟢 within ±{FLAG:g}%. "
         "Informational: this check never fails."),
        "- Run them locally: `cargo bench -p terrio --bench instructions`.",
        "</details>",
    ]
    sys.stdout.write("\n".join(out) + "\n")


if __name__ == "__main__":
    main()

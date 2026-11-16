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
NOTE = 2.0  # percent: smaller changes are written "≈"


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


def change(old, new):
    pct = (new - old) / old * 100.0 if old else float("inf")
    return pct, f"{pct:+.1f}%".replace("-", "−")


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("summaries", help="the JSON output of the pull request's run")
    parser.add_argument("--base", required=True, help="main's commit")
    parser.add_argument("--head", required=True, help="the pull request's commit")
    args = parser.parse_args()
    with open(args.summaries, encoding="utf-8") as f:
        rows = [(s["function_name"] + (f" {s['id']}" if s.get("id") else ""), *metrics(s)) for s in summaries(f.read())]

    header = ["| | benchmark | instructions | change | est. cycles | change |", "|---|---|---:|---:|---:|---:|"]
    flagged, unchanged, regression = [], [], False
    for name, ir, cycles in rows:
        if ir[0] is None:
            unchanged.append(f"| 🆕 | {name} | {count(ir[1])} | new | {count(cycles[1]) if cycles else ''} | new |")
            continue
        cells, worst, best = [], 0.0, 0.0
        for pair in (ir, cycles):
            if pair is None or pair[0] is None:
                cells += ["", ""]
                continue
            pct, text = change(*pair)
            worst, best = max(worst, pct), min(best, pct)
            flag = abs(pct) >= FLAG
            cells += [f"{count(pair[0])} → {count(pair[1])}", f"**{text}**" if flag else text if abs(pct) >= NOTE else "≈"]
        line = f"| {{}} | {name} | {' | '.join(cells)} |"
        if worst >= FLAG:
            regression = True
            flagged.append(line.format("⚠️"))
        elif best <= -FLAG:
            flagged.append(line.format("✅"))
        else:
            unchanged.append(line.format("🟢"))

    out = []
    if regression:
        out.append("<!-- terrio-bench: regression -->")
    out += [
        "### Performance vs main",
        f"Under Valgrind, main `{args.base[:7]}` vs this PR `{args.head[:7]}`: CPU instructions, and estimated cycles",
        "(instructions plus cache and RAM misses, a speed estimate). Both repeat within 0.01% between runs.",
        f"Flagged at ±{FLAG:g}% on either. Informational: this check never fails.",
        "",
    ]
    compared = sum(ir[0] is not None for _, ir, _ in rows)
    ok = compared - len(flagged)
    if not compared:
        out += ["main has no benchmarks yet: these are this PR's counts.", ""]
    if flagged:
        out += [*header, *flagged, ""]
    elif compared:
        out += [f"🟢 All {ok}/{compared} benchmarks within ±{FLAG:g}% of main.", ""]
    if unchanged:
        label = f"🟢 {ok}/{compared} within ±{FLAG:g}%" if compared else f"{len(unchanged)} benchmarks"
        out += [f"<details><summary>{label}</summary>", "", *header, *unchanged, "</details>"]
    sys.stdout.write("\n".join(out) + "\n")


if __name__ == "__main__":
    main()

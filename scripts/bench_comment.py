#!/usr/bin/env python3
"""Write the pull-request comment of the `bench` workflow (Markdown, on stdout).

Reads the JSON summaries `cargo bench -p terrio --bench instructions -- --baseline=main
--output-format=json` prints (one object per benchmark) and compares the CPU instructions
(Callgrind's `Ir`) of each benchmark with the `main` baseline. Changes of 10% or more are
flagged; the comment then carries a `<!-- terrio-bench: regression -->` line, which
bench-comment.yml reads to set the `perf-regression` label.

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


def instructions(summary):
    """(main, pull request) instruction counts of a benchmark summary; main is None without a baseline."""
    for profile in summary["profiles"]:
        if profile["tool"] == "Callgrind":
            values = profile["data"]["total"]["metrics"]["Ir"]["values"]
            old = values.get("old")
            return (None if old is None else int(old), int(values["new"]))
    raise ValueError(f"no Callgrind profile in {summary.get('function_name')}")


def count(n):
    """`n` instructions with an SI prefix, three significant digits."""
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
        rows = []
        for s in summaries(f.read()):
            name = s["function_name"] + (f" {s['id']}" if s.get("id") else "")
            rows.append((name, *instructions(s)))

    flagged, unchanged, regression = [], [], False
    for name, old, new in rows:
        if old is None:
            unchanged.append(f"| {name} | | {count(new)} | new |")
            continue
        pct, text = change(old, new)
        if abs(pct) >= FLAG:
            regression |= pct > 0
            mark = "⚠️" if pct > 0 else "✅"
            flagged.append(f"| {mark} | {name} | {count(old)} | {count(new)} | **{text}** |")
        else:
            unchanged.append(f"| {name} | {count(old)} | {count(new)} | {text if abs(pct) >= NOTE else '≈'} |")

    out = []
    if regression:
        out.append("<!-- terrio-bench: regression -->")
    out += [
        "### Performance vs main",
        f"CPU instructions under Valgrind (deterministic, about ±1%), main `{args.base[:7]}` vs this PR `{args.head[:7]}`.",
        f"Flagged at ±{FLAG:g}%. Informational: this check never fails.",
        "",
    ]
    if not any(old is not None for _, old, _ in rows):
        out += ["main has no benchmarks yet: these are this PR's counts.", ""]
    if flagged:
        out += ["| | benchmark | main | this PR | change |", "|---|---|---:|---:|---:|", *flagged, ""]
    elif any(old is not None for _, old, _ in rows):
        out += [f"No benchmark changed by {FLAG:g}% or more.", ""]
    if unchanged:
        label = f"{len(unchanged)} within ±{FLAG:g}%" if flagged or any(old is not None for _, old, _ in rows) else f"{len(unchanged)} benchmarks"
        out += [
            f"<details><summary>{label}</summary>",
            "",
            "| benchmark | main | this PR | change |",
            "|---|---:|---:|---:|",
            *unchanged,
            "</details>",
        ]
    sys.stdout.write("\n".join(out) + "\n")


if __name__ == "__main__":
    main()

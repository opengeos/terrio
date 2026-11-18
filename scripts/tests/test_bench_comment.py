"""The bench workflow's comment flags 10% changes of instructions or estimated cycles, marks
benchmarks within that range green, says what each one runs, and marks only regressions for
the label."""

import json
import subprocess
import sys
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "bench_comment.py"


def summary(name, new, old=None, cycles=None, function="warp_f32"):
    """A gungraun summary; `cycles` is (new, old) estimated cycles, by default the instructions."""
    def values(n, o):
        return {"values": {"new": n} if o is None else {"new": n, "old": o}}

    cycles = cycles or (new, old)
    metrics = {"Ir": values(new, old), "EstimatedCycles": values(*cycles)}
    profile = {"tool": "Callgrind", "data": {"total": {"metrics": metrics}}}
    return {"function_name": function, "id": name, "profiles": [profile]}


def comment(tmp_path, *summaries):
    path = tmp_path / "pr.json"
    path.write_text("\n".join(json.dumps(s) for s in summaries))
    run = [sys.executable, str(SCRIPT), str(path), "--base", "a" * 40, "--head", "b" * 40]
    return subprocess.run(run, check=True, capture_output=True, text=True).stdout


def test_regression_and_improvement_are_flagged_first(tmp_path):
    out = comment(
        tmp_path,
        summary("same", 1001, 1000),
        summary("faster", 75, 100),
        summary("slower", 1_400_000_000, 1_000_000_000),
    )
    assert out.startswith("<!-- terrio-bench: regression -->\n")
    assert "⚠️ **1 of 3 benchmarks are slower** than main: warp_f32 slower +40.0%." in out
    assert "✅ **1 of 3 are faster**: warp_f32 faster −25.0%." in out
    shown, folded = out.split("<details><summary>🟢 1 within ±10%</summary>")
    rows = [line for line in shown.splitlines() if line.startswith("| ") and "→" in line]
    assert rows == [
        "| ⚠️ | `warp_f32 slower` | 1 G → 1.4 G (**+40.0%**) | 1 G → 1.4 G (**+40.0%**) |",
        "| ✅ | `warp_f32 faster` | 100 → 75 (**−25.0%**) | 100 → 75 (**−25.0%**) |",
    ]
    assert "| 🟢 | `warp_f32 same` | 1 k → 1 k (+0.10%) | 1 k → 1 k (+0.10%) |" in folded


def test_cycles_alone_flag_a_regression(tmp_path):
    out = comment(tmp_path, summary("misses", 1000, 1000, cycles=(1500, 1000)))
    assert out.startswith("<!-- terrio-bench: regression -->\n")
    assert "| ⚠️ | `warp_f32 misses` | 1 k → 1 k (0%) | 1 k → 1.5 k (**+50.0%**) |" in out


def test_a_larger_drop_does_not_hide_a_regression(tmp_path):
    out = comment(tmp_path, summary("mixed", 1150, 1000, cycles=(500, 1000)))
    assert out.startswith("<!-- terrio-bench: regression -->\n")
    assert "⚠️ **1 of 1 benchmarks are slower** than main: warp_f32 mixed +15.0%." in out


def test_a_tiny_drop_reads_zero(tmp_path):
    out = comment(tmp_path, summary("drift", 99_999, 100_000))
    assert "| 🟢 | `warp_f32 drift` | 100 k → 100 k (0%) | 100 k → 100 k (0%) |" in out


def test_improvement_alone_sets_no_label(tmp_path):
    out = comment(tmp_path, summary("faster", 50, 100), summary("noted", 1050, 1000))
    assert "regression -->" not in out
    assert "| 🟢 | `warp_f32 noted` | 1 k → 1.05 k (+5.0%) | 1 k → 1.05 k (+5.0%) |" in out


def test_all_within_threshold_is_green_and_folded(tmp_path):
    out = comment(tmp_path, summary("same", 1001, 1000), summary("noted", 1050, 1000))
    assert "🟢 **As fast as main.** All 2 benchmarks are within ±10%; the largest change is +5.0% (warp_f32 noted)." in out
    shown = out.split("<details><summary>🟢 2 within ±10%</summary>")[0]
    assert "| 🟢 |" not in shown


def test_known_benchmarks_say_what_they_run(tmp_path):
    out = comment(tmp_path, summary("slope", 54, 54, function="dem_f32"))
    assert "| 🟢 | Slope of a 1 M-pixel DEM<br>`dem_f32 slope` | 54 → 54 (0%) | 54 → 54 (0%) |" in out


def test_without_baseline_counts_are_listed(tmp_path):
    out = comment(tmp_path, summary("first", 2_000_000))
    assert "main has no benchmarks yet" in out
    assert "| 🆕 | `warp_f32 first` | 2 M (new) | 2 M (new) |" in out


def test_from_zero_reads_new(tmp_path):
    out = comment(tmp_path, summary("grew", 500, 0))
    assert "| ⚠️ | `warp_f32 grew` | 0 → 500 (**new**) | 0 → 500 (**new**) |" in out
    assert "inf" not in out


def test_just_under_the_threshold_is_not_shown_as_10(tmp_path):
    out = comment(tmp_path, summary("close", 10_996, 10_000), summary("over", 10_999, 10_000))
    assert "10 k → 11 k (+10.0%)" not in out
    assert "**+10.0%**" in out


def test_verdict_says_from_0(tmp_path):
    out = comment(tmp_path, summary("grew", 500, 0))
    assert "warp_f32 grew from 0." in out

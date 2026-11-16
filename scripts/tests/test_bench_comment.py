"""The bench workflow's comment flags 10% changes of instructions or estimated cycles, marks
benchmarks within that range green, and marks only regressions for the label."""

import json
import subprocess
import sys
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "bench_comment.py"


def summary(name, new, old=None, cycles=None):
    """A gungraun summary; `cycles` is (new, old) estimated cycles, by default the instructions."""
    def values(n, o):
        return {"values": {"new": n} if o is None else {"new": n, "old": o}}

    cycles = cycles or (new, old)
    metrics = {"Ir": values(new, old), "EstimatedCycles": values(*cycles)}
    profile = {"tool": "Callgrind", "data": {"total": {"metrics": metrics}}}
    return {"function_name": "warp_f32", "id": name, "profiles": [profile]}


def comment(tmp_path, *summaries):
    path = tmp_path / "pr.json"
    path.write_text("\n".join(json.dumps(s) for s in summaries))
    run = [sys.executable, str(SCRIPT), str(path), "--base", "a" * 40, "--head", "b" * 40]
    return subprocess.run(run, check=True, capture_output=True, text=True).stdout


def test_regression_and_improvement_are_flagged(tmp_path):
    out = comment(tmp_path, summary("slower", 1_400_000_000, 1_000_000_000), summary("faster", 75, 100), summary("same", 1001, 1000))
    assert out.startswith("<!-- terrio-bench: regression -->\n")
    assert "| ⚠️ | warp_f32 slower | 1 G → 1.4 G | **+40.0%** | 1 G → 1.4 G | **+40.0%** |" in out
    assert "| ✅ | warp_f32 faster | 100 → 75 | **−25.0%** | 100 → 75 | **−25.0%** |" in out
    assert "| 🟢 | warp_f32 same | 1 k → 1 k | ≈ | 1 k → 1 k | ≈ |" in out
    assert "<summary>🟢 1/3 within ±10%</summary>" in out


def test_cycles_alone_flag_a_regression(tmp_path):
    out = comment(tmp_path, summary("misses", 1000, 1000, cycles=(1500, 1000)))
    assert out.startswith("<!-- terrio-bench: regression -->\n")
    assert "| ⚠️ | warp_f32 misses | 1 k → 1 k | ≈ | 1 k → 1.5 k | **+50.0%** |" in out


def test_improvement_alone_sets_no_label(tmp_path):
    out = comment(tmp_path, summary("faster", 50, 100), summary("noted", 1050, 1000))
    assert "regression -->" not in out
    assert "| 🟢 | warp_f32 noted | 1 k → 1.05 k | +5.0% | 1 k → 1.05 k | +5.0% |" in out


def test_all_within_threshold_is_green(tmp_path):
    out = comment(tmp_path, summary("same", 1001, 1000), summary("noted", 1050, 1000))
    assert "🟢 All 2/2 benchmarks within ±10% of main." in out
    assert "<summary>🟢 2/2 within ±10%</summary>" in out


def test_without_baseline_counts_are_listed(tmp_path):
    out = comment(tmp_path, summary("first", 2_000_000))
    assert "main has no benchmarks yet" in out
    assert "| 🆕 | warp_f32 first | 2 M | new | 2 M | new |" in out

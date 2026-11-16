"""The bench workflow's comment flags 10% changes and marks only regressions for the label."""

import json
import subprocess
import sys
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "bench_comment.py"


def summary(name, new, old=None):
    values = {"new": new} if old is None else {"new": new, "old": old}
    profile = {"tool": "Callgrind", "data": {"total": {"metrics": {"Ir": {"values": values}}}}}
    return {"function_name": "warp_f32", "id": name, "profiles": [profile]}


def comment(tmp_path, *summaries):
    path = tmp_path / "pr.json"
    path.write_text("\n".join(json.dumps(s) for s in summaries))
    run = [sys.executable, str(SCRIPT), str(path), "--base", "a" * 40, "--head", "b" * 40]
    return subprocess.run(run, check=True, capture_output=True, text=True).stdout


def test_regression_and_improvement_are_flagged(tmp_path):
    out = comment(tmp_path, summary("slower", 1_400_000_000, 1_000_000_000), summary("faster", 75, 100), summary("same", 1001, 1000))
    assert out.startswith("<!-- terrio-bench: regression -->\n")
    assert "| ⚠️ | warp_f32 slower | 1 G | 1.4 G | **+40.0%** |" in out
    assert "| ✅ | warp_f32 faster | 100 | 75 | **−25.0%** |" in out
    assert "| warp_f32 same | 1 k | 1 k | ≈ |" in out


def test_improvement_alone_sets_no_label(tmp_path):
    out = comment(tmp_path, summary("faster", 50, 100), summary("noted", 1050, 1000))
    assert "regression -->" not in out
    assert "| warp_f32 noted | 1 k | 1.05 k | +5.0% |" in out


def test_without_baseline_counts_are_listed(tmp_path):
    out = comment(tmp_path, summary("first", 2_000_000))
    assert "main has no benchmarks yet" in out
    assert "| warp_f32 first | | 2 M | new |" in out

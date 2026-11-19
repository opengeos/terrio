"""The bench workflow's comment counts benchmarks worse, better or matching main in its heading,
lists each flagged measure first, folds the rest by area, and marks only regressions for the
label."""

import json
import re
import subprocess
import sys
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "bench_comment.py"


def summary(name, new, old=None, cycles=None, function="warp_f32", memory=None):
    """A gungraun summary; `cycles` is (new, old) estimated cycles, by default the instructions,
    and `memory` (new, old) peak heap bytes, by default none."""

    def values(n, o):
        return {"values": {"new": n} if o is None else {"new": n, "old": o}}

    cycles = cycles or (new, old)
    metrics = {"Ir": values(new, old), "EstimatedCycles": values(*cycles)}
    profiles = [{"tool": "Callgrind", "data": {"total": {"metrics": metrics}}}]
    if memory:
        profiles.append({"tool": "DHAT", "data": {"total": {"metrics": {"TotalUnits": values(*memory)}}}})
    return {"function_name": function, "id": name, "profiles": profiles}


def comment(tmp_path, *summaries):
    path = tmp_path / "pr.json"
    path.write_text("\n".join(json.dumps(s) for s in summaries))
    run = [sys.executable, str(SCRIPT), str(path), "--base", "a" * 40, "--head", "b" * 40]
    return subprocess.run(run, check=True, capture_output=True, text=True).stdout


def flagged_rows(out):
    """The rows of the flagged table: those with a measure column."""
    return [line for line in out.split("<details>")[0].splitlines() if line.startswith("| ") and " → " in line and not line.startswith("| | benchmark")]


def test_worse_then_better_are_listed_with_bars(tmp_path):
    out = comment(
        tmp_path,
        summary("same", 1001, 1000),
        summary("faster", 75, 100),
        summary("slower", 1_400_000_000, 1_000_000_000),
    )
    assert out.startswith("<!-- terrio-bench: regression -->\n### ⚠️ Performance: 1 worse, 1 better · 1 match main\n")
    assert flagged_rows(out) == [
        "| ⚠️ | `warp_f32 slower`<br><sub>Reprojection</sub> | CPU | 1 G → 1.4 G | **+40.0%** | `██████████▒▒▒▒` |",
        "| | | speed | 1 G → 1.4 G | **+40.0%** | `██████████▒▒▒▒` |",
        "| ✅ | `warp_f32 faster`<br><sub>Reprojection</sub> | CPU | 100 → 75 | **−25.0%** | `████████░░` |",
        "| | | speed | 100 → 75 | **−25.0%** | `████████░░` |",
    ]
    assert "| ⚠️ Reprojection | 3 | ⚠️ 1 worse · ✅ 1 better | ⚠️ 1 worse · ✅ 1 better | ✓ |" in out


def test_all_matching_main_says_it_holds(tmp_path):
    out = comment(tmp_path, summary("same", 1001, 1000), summary("noted", 1050, 1000))
    assert out.startswith("### 🟢 Performance holds: all 2 benchmarks match main\nCPU, speed and memory all match main.\n")
    assert flagged_rows(out) == []
    assert "| 🟢 | `warp_f32 noted` | 1 k → 1.05 k (+5.0%) | 1 k → 1.05 k (+5.0%) |  |" in out


def test_cycles_alone_flag_a_regression(tmp_path):
    out = comment(tmp_path, summary("misses", 1000, 1000, cycles=(1500, 1000)))
    assert out.startswith("<!-- terrio-bench: regression -->\n")
    assert [r.split(" | ")[2] for r in flagged_rows(out)] == ["speed"]


def test_a_larger_drop_does_not_hide_a_regression(tmp_path):
    out = comment(tmp_path, summary("mixed", 1150, 1000, cycles=(500, 1000)))
    assert out.startswith("<!-- terrio-bench: regression -->\n### ⚠️ Performance: 1 worse · 0 match main\n")


def test_improvement_alone_sets_no_label(tmp_path):
    out = comment(tmp_path, summary("faster", 50, 100), summary("noted", 1050, 1000))
    assert out.startswith("### ✅ Performance: 1 better · 1 match main\n")


def test_memory_flags_at_10_percent_and_1_mb(tmp_path):
    out = comment(
        tmp_path,
        summary("doubled", 1000, 1000, memory=(10_000_000, 5_000_000)),
        summary("small", 1000, 1000, memory=(300_000, 200_000)),
        summary("lean", 1000, 1000, memory=(4_000_000, 6_000_000)),
    )
    assert out.startswith("<!-- terrio-bench: regression -->\n")
    assert flagged_rows(out) == [
        "| ⚠️ | `warp_f32 doubled`<br><sub>Reprojection</sub> | memory | 5 MB → 10 MB | **+5 MB (+100.0%)** | `██████████▒▒▒▒▒▒▒▒▒▒` |",
        "| ✅ | `warp_f32 lean`<br><sub>Reprojection</sub> | memory | 6 MB → 4 MB | **−2 MB (−33.3%)** | `███████░░░` |",
    ]
    # +50% but only 100 kB: not flagged.
    assert "| 🟢 | `warp_f32 small` | 1 k (0%) | 1 k (0%) | 200 kB → 300 kB (+50.0%) |" in out


def test_commands_show_their_command_line_and_section(tmp_path):
    out = comment(tmp_path, summary("write_zstd_u8", 1200, 1000, function="cli"))
    assert "| ⚠️ | `terrio translate --compress ZSTD`<br><sub>GeoTIFF encoding · `cli write_zstd_u8`</sub> | CPU |" in out
    assert "| ⚠️ GeoTIFF encoding | 1 |" in out


def test_known_benchmarks_say_what_they_run(tmp_path):
    out = comment(tmp_path, summary("slope", 54, 30, function="dem_f32"))
    assert "| ⚠️ | Slope of a 1 M-pixel DEM<br><sub>Terrain · `dem_f32 slope`</sub> | CPU |" in out


def test_without_baseline_counts_are_listed(tmp_path):
    out = comment(tmp_path, summary("first", 2_000_000))
    assert out.startswith("### 🆕 Performance: 1 new benchmarks, none on main yet\n")
    assert "| 🆕 | `warp_f32 first` | 2 M (new) | 2 M (new) |  |" in out
    assert "| 🆕 Reprojection | 1 (1 new) | 🆕 | 🆕 | 🆕 |" in out


def test_new_benchmarks_beside_matching_ones(tmp_path):
    out = comment(tmp_path, summary("same", 1000, 1000), summary("added", 500))
    assert out.startswith("### 🟢 Performance holds: all 1 benchmarks match main · 🆕 1 new\n")
    assert "| 🟢 Reprojection | 2 (1 new) |" in out


def test_from_zero_reads_new(tmp_path):
    out = comment(tmp_path, summary("grew", 500, 0))
    assert "| ⚠️ | `warp_f32 grew`<br><sub>Reprojection</sub> | CPU | 0 → 500 | **new** |  |" in out
    assert not re.search(r"\binf\b", out)


def test_a_change_that_rounds_to_10_is_flagged_as_shown(tmp_path):
    out = comment(tmp_path, summary("close", 10_996, 10_000), summary("over", 10_999, 10_000))
    assert "10 k → 11 k (+10.0%)" not in out
    assert "**+10.0%**" in out


def test_sizes_just_under_a_prefix_round_up(tmp_path):
    out = comment(tmp_path, summary("edge", 1000, 1000, memory=(999_999, 999_999)))
    assert "| 1 MB (0%) |" in out
    assert "e+03" not in out


def test_a_metric_only_main_has_is_left_blank(tmp_path):
    s = summary("gone", 1000, 1000, memory=(5, 5))
    del s["profiles"][1]["data"]["total"]["metrics"]["TotalUnits"]["values"]["new"]
    out = comment(tmp_path, s)
    assert "| 🟢 | `warp_f32 gone` | 1 k (0%) | 1 k (0%) |  |" in out


def test_footer_names_both_commits_last(tmp_path):
    out = comment(tmp_path, summary("same", 1000, 1000))
    assert out.rstrip().splitlines()[-1].startswith("<sub>main `aaaaaaa` → PR `bbbbbbb` ·")

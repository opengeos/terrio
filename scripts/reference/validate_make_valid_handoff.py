"""Custodian-only end-to-end checks for the prepared implementation package."""

import argparse
import json
from pathlib import Path
import subprocess
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    args = parser.parse_args()
    root = args.package.resolve()
    launch = ["python3", "tools/isolate.py"]
    checks = [
        ("initial-hashes", ["python3", "tools/verify_package.py", "--initial"]),
        ("boundary", [*launch, "--", "python3", "tools/check_boundary.py"]),
        ("native-build", [*launch, "--slot", "heavy", "--", "cargo", "build", "--locked", "--offline"]),
        ("scaffold", [*launch, "--", "python3", "tools/check_scaffold.py"]),
        ("harness-controls", [*launch, "--", "python3", "tools/check_harness.py"]),
        ("reference-replay", [*launch, "--", "python3", "tools/replay.py", "--reference-self-check"]),
        ("expected-unimplemented-failure", [*launch, "--", "python3", "tools/replay.py"]),
        ("rust-tests", [*launch, "--", "cargo", "test", "--locked", "--offline"]),
        ("clippy", [*launch, "--", "cargo", "clippy", "--workspace", "--all-targets", "--locked", "--offline", "--", "-D", "warnings"]),
        ("format", [*launch, "--", "cargo", "fmt", "--all", "--", "--check"]),
        ("wasm-build", [*launch, "--slot", "heavy", "--", "cargo", "build", "--locked", "--offline", "--lib", "--target", "wasm32-unknown-unknown"]),
    ]
    results = []
    for name, command in checks:
        started = time.monotonic()
        completed = subprocess.run(command, cwd=root, text=True, capture_output=True, timeout=300)
        expected = 1 if name == "expected-unimplemented-failure" else 0
        record = {
            "name": name, "command": command, "expected_exit": expected,
            "exit": completed.returncode, "seconds": round(time.monotonic() - started, 3),
            "passed": completed.returncode == expected,
        }
        if name in {"reference-replay", "expected-unimplemented-failure"}:
            try:
                capture = json.loads(completed.stdout[completed.stdout.index("{"):])
                record["results"] = len(capture["results"])
                record["failures"] = len(capture["failures"])
                if name == "expected-unimplemented-failure":
                    record["passed"] &= len(capture["failures"]) == 51 and all(c["message"] == "candidate status: unimplemented" for c in capture["failures"])
                else:
                    record["passed"] &= len(capture["results"]) == 51 and not capture["failures"]
                    record["encoder_domain_exclusions"] = [c["id"] for c in capture["results"] if c["status"] == "kernel-pass/encoder-domain-excluded"]
            except (ValueError, KeyError):
                record["passed"] = False
        else:
            record["stdout"] = completed.stdout[-3500:]
        record["stderr"] = completed.stderr[-5000:]
        results.append(record)
        print(json.dumps({k: v for k, v in record.items() if k not in {"stdout", "stderr", "command"}}), flush=True)
        if not record["passed"]:
            print(completed.stdout[-5000:])
            print(completed.stderr[-5000:])
            break
    report = {"checks": results, "complete": len(results) == len(checks), "passed": len(results) == len(checks) and all(r["passed"] for r in results)}
    args.output.write_text(json.dumps(report, indent=2) + "\n")
    raise SystemExit(0 if report["passed"] else 1)


if __name__ == "__main__":
    main()

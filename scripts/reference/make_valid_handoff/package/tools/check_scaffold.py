"""Check transport behavior while requiring that the candidate is still unimplemented."""

import json
from pathlib import Path
import subprocess
import sys


def invoke(binary, polygons):
    completed = subprocess.run(
        [str(binary)], input=json.dumps({"polygons": polygons}), text=True,
        capture_output=True, timeout=10, check=True,
    )
    return json.loads(completed.stdout)


def main():
    root = Path(__file__).resolve().parents[1]
    binary = Path(sys.argv[1]) if len(sys.argv) > 1 else Path("/build/debug/candidate")
    fixtures = json.loads((root / "fixtures/observations.json").read_text())["observations"]
    for case in fixtures:
        response = invoke(binary, case["input_polygons"])
        if response != {"status": "unimplemented"}:
            raise SystemExit(f"{case['id']}: scaffold did not explicitly report unimplemented")
    invalid = subprocess.run([str(binary)], input="not json", text=True, capture_output=True, timeout=10, check=True)
    if json.loads(invalid.stdout).get("status") != "error":
        raise SystemExit("invalid transport input did not report error")
    print(f"PASS: {len(fixtures)} scaffold inputs explicitly unimplemented; invalid input reports error")


if __name__ == "__main__":
    main()

"""Test the comparator and independently decode the reference's actual MVT commands."""

from copy import deepcopy
import json
from pathlib import Path

from replay import check_case, invoke


def decode(commands):
    x = y = index = 0
    ring, rings = [], []
    while index < len(commands):
        word = commands[index]
        index += 1
        command, count = word & 7, word >> 3
        if command in (1, 2):
            for _ in range(count):
                a, b = commands[index:index + 2]
                index += 2
                x += (a >> 1) ^ -(a & 1)
                y += (b >> 1) ^ -(b & 1)
                ring.append([x, y])
        elif command == 7 and count == 1 and ring:
            rings.append(ring + [ring[0]])
            ring = []
        else:
            raise ValueError("invalid reference command sequence")
    if ring:
        raise ValueError("unclosed command ring")
    return rings


def rejected(case, polygons, adapter):
    try:
        check_case(case, polygons, adapter)
    except ValueError:
        return
    raise SystemExit("comparator accepted a deliberately wrong result: " + case["id"])


def main():
    root = Path(__file__).resolve().parents[1]
    cases = {c["id"]: c for c in json.loads((root / "fixtures/observations.json").read_text())["observations"]}
    adapter = root / "reference/tile-encoder"
    decoded = 0
    for case in cases.values():
        response = invoke(adapter, case["polygonal_coordinates"])
        if response["status"] == "encoder_overflow" and case["id"] == "wide-i32-bowtie":
            continue
        if response["status"] != "ok":
            raise SystemExit("unexpected encoder failure: " + case["id"])
        if decode(response["commands"]) != [r for p in response["polygons"] for r in p]:
            raise SystemExit("encoded commands differ from emitted integer rings: " + case["id"])
        decoded += 1
    rejected(cases["square"], [], adapter)
    rejected(cases["square-hole"], [cases["square"]["polygonal_coordinates"][0]], adapter)
    wrong = deepcopy(cases["fractional-crossing"]["polygonal_coordinates"])
    for polygon in wrong:
        for ring in polygon:
            for point in ring:
                if point[0] == 6:
                    point[0] -= 1e-12
    rejected(cases["fractional-crossing"], wrong, adapter)
    tiny_hole = deepcopy(cases["square"]["polygonal_coordinates"])
    tiny_hole[0].append([[1, 1], [1 + 1e-8, 1], [1 + 1e-8, 1 + 1e-8], [1, 1 + 1e-8], [1, 1]])
    rejected(cases["square"], tiny_hole, adapter)
    if invoke(adapter, [[[[0, 0], [1, 0], [0, 0]]]])["present"]:
        raise SystemExit("collapsed reference ring incorrectly retained")
    print(f"PASS: {decoded} encoded command streams independently decoded; missing area/hole, tiny extra hole and truncation-threshold controls rejected")


if __name__ == "__main__":
    main()

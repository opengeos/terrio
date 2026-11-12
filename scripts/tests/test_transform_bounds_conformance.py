"""Selected conformance comparisons must run completely despite inherited pytest options."""

import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import conformance  # noqa: E402


@pytest.mark.parametrize("outcome", ["passed", "skipped", "failed", "missing"])
def test_selected_conformance_requires_every_successful_result(monkeypatch, tmp_path, outcome):
    """Execute real pytest collection and JUnit reporting with conflicting filters.

    Args:
        monkeypatch: Environment and module patch fixture.
        tmp_path: Isolated miniature repository.
        outcome: Successful, skipped, failing or missing comparison.
    """
    filename = "test_comparisons.py"
    test_dir = tmp_path / "python" / "tests"
    test_dir.mkdir(parents=True)
    actions = ["pass", "pass", {"skipped": "pytest.skip('no reference')", "failed": "assert False"}.get(outcome, "pass")]
    if outcome == "missing":
        actions.pop()
    source = "import pytest\n" + "\n".join(
        f'def test_comparison_{index}():\n    """Exercise comparison {index}."""\n    {action}\n'
        for index, action in enumerate(actions)
    )
    (test_dir / filename).write_text(source)
    (tmp_path / "pytest.ini").write_text(
        "[pytest]\naddopts = --deselect=python/tests/test_comparisons.py::test_comparison_0\n"
    )
    monkeypatch.setenv("PYTEST_ADDOPTS", "--deselect=python/tests/test_comparisons.py::test_comparison_1")
    monkeypatch.setattr(conformance, "ROOT", tmp_path)
    complete, detail = conformance._pytest_module_conformance(
        SimpleNamespace(data=tmp_path), filename, selection="comparison", expected_count=3
    )
    assert complete == (outcome == "passed"), detail

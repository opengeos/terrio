"""PDAL reference pipelines and narrowly checked optional EPT codec support."""

import functools
import json
from pathlib import Path
import subprocess


class PdalPipelineError(subprocess.CalledProcessError):
    """Include captured diagnostics when an unexpected reference error is uncaught."""

    def __str__(self):
        return f"{super().__str__()}\n{self.stderr}".rstrip()


def run_pipeline(pdal: str, stages: list) -> subprocess.CompletedProcess:
    """Keep a failed PDAL process's status and diagnostics, including invalid UTF-8."""
    result = subprocess.run(
        [pdal, "pipeline", "--stdin"], input=json.dumps(stages).encode("utf-8"),
        check=False, capture_output=True,
    )
    stdout = result.stdout.decode("utf-8", errors="replace")
    stderr = result.stderr.decode("utf-8", errors="replace")
    if result.returncode:
        raise PdalPipelineError(result.returncode, result.args, output=stdout, stderr=stderr)
    return subprocess.CompletedProcess(result.args, result.returncode, stdout, stderr)


@functools.cache
def ept_codec_available(pdal: str, reference: Path, data_type: str) -> bool:
    """Only skip Zstandard if PDAL rejects its own known-good test fixture's codec.

    An error reading Terrio's output is never used to infer codec availability.
    Missing fixtures and other reference failures remain errors.
    """
    if data_type != "zstandard":
        return True
    if not reference.is_file():
        raise FileNotFoundError(reference)
    try:
        run_pipeline(pdal, [{"type": "readers.ept", "filename": str(reference)}, {"type": "writers.null"}])
    except subprocess.CalledProcessError as error:
        if "unrecognized ept datatype" in error.stderr.casefold():
            return False
        raise
    return True

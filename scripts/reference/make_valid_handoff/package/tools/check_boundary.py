"""Check the expected Bubblewrap mount, network, environment and write boundaries."""

import os
from pathlib import Path
import socket


def main():
    if Path.cwd() != Path("/work"):
        raise SystemExit("run this check through tools/isolate.py")
    for path in ("/home/mavrick", "/mnt", "/run", "/usr/local", "/usr/share", "/work/.git", "/work/docs/make-valid-clean-room-process.md"):
        if Path(path).exists():
            raise SystemExit("unexpected host/source exposure: " + path)
    for directory in (Path("/usr/bin"), Path("/usr/lib"), Path("/work/reference"), Path("/work/vendor")):
        try:
            (directory / ".boundary-probe").write_text("probe")
        except OSError:
            pass
        else:
            (directory / ".boundary-probe").unlink()
            raise SystemExit("expected read-only mount: " + str(directory))
    assert os.environ["HOME"] == "/home/implementer"
    assert "T3_ACP_MCP_NODE" not in os.environ
    with socket.socket() as connection:
        connection.settimeout(0.5)
        try:
            connection.connect(("1.1.1.1", 443))
        except OSError:
            pass
        else:
            raise SystemExit("unexpected external network access")
    for directory in ("/work/notes", "/work/candidate", "/build"):
        probe = Path(directory) / ".boundary-probe"
        probe.write_text("probe")
        probe.unlink()
    import shapely
    import numpy
    print(f"PASS: source/home/network hidden; references read-only; candidate/build writable; Shapely {shapely.__version__}, GEOS {shapely.geos_version_string}, NumPy {numpy.__version__}")


if __name__ == "__main__":
    main()

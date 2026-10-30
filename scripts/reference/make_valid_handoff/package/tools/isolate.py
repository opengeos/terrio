"""Run one command inside the reviewed offline filesystem boundary, through the host queue."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

sys.dont_write_bytecode = True
from runtime_hash import hash_path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--slot", choices=["gate", "heavy"], default="gate")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command
    if command and command[0] == "--":
        command = command[1:]
    if not command:
        parser.error("supply a command after --")
    root = Path(__file__).resolve().parents[1]
    subprocess.run([sys.executable, str(root / "tools/verify_package.py")], check=True)
    runtime = json.loads((root / "HOST-RUNTIME.json").read_text())
    for binding in runtime["reference_bindings"]:
        if hash_path(binding["host"]) != binding["sha256"]:
            raise SystemExit("approved external runtime changed: " + binding["sandbox"])
    build = Path(runtime["build_directory"])
    marker = build / ".handoff-manifest-sha256"
    if not marker.is_file() or marker.read_text().strip() != hashlib.sha256((root / "MANIFEST.json").read_bytes()).hexdigest():
        raise SystemExit("build directory is not provisioned for this package; ask the custodian to recreate it")
    sandbox = [
        "/usr/bin/bwrap", "--unshare-all", "--unshare-user", "--die-with-parent", "--new-session", "--disable-userns", "--clearenv",
        "--dir", "/usr", "--ro-bind", "/usr/bin", "/usr/bin", "--ro-bind", "/usr/lib", "/usr/lib",
        "--ro-bind", "/usr/libexec", "/usr/libexec",
        "--ro-bind", "/usr/lib64", "/usr/lib64", "--symlink", "usr/bin", "/bin",
        "--symlink", "usr/lib", "/lib", "--symlink", "usr/lib64", "/lib64",
        "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
        "--dir", "/home/implementer", "--dir", "/reference/lib",
        "--ro-bind", runtime["toolchain"], "/toolchain",
        "--ro-bind", str(root), "/work",
        "--bind", str(root / "candidate"), "/work/candidate",
        "--bind", str(root / "notes"), "/work/notes",
        "--bind", str(build), "/build",
    ]
    for binding in runtime["reference_bindings"]:
        sandbox += ["--ro-bind", binding["host"], binding["sandbox"]]
    environment = {
        "HOME": "/home/implementer", "PATH": "/toolchain/bin:/usr/bin:/bin",
        "CARGO_HOME": "/tmp/cargo", "CARGO_TARGET_DIR": "/build", "CARGO_BUILD_JOBS": "2",
        "CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0",
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER": "/usr/bin/gcc",
        "CARGO_NET_OFFLINE": "true", "PYTHONPATH": "/reference", "PYTHONDONTWRITEBYTECODE": "1",
        "LD_LIBRARY_PATH": "/reference/lib", "LANG": "C.UTF-8",
        "OPENBLAS_NUM_THREADS": "1", "OMP_NUM_THREADS": "1",
    }
    for key, value in environment.items():
        sandbox += ["--setenv", key, value]
    sandbox += ["--chdir", "/work", "--", *command]
    queue = [runtime["host_queue"], "run", "terrio clean-room " + Path(command[0]).name, "--slot", args.slot, "--", *sandbox]
    os.execv(queue[0], queue)


if __name__ == "__main__":
    main()

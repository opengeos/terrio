"""Custodian-only isolated Claude Opus 5.5 launcher and model-only Unix gateway.

Provider credentials remain in the host broker and are never mounted or passed
to the implementer. No host HTTP listener or generic network proxy is created.
Run the implementation invocation through hq --slot gate, so all child builds
and tests inherit that lease. This launcher does not implement geometry repair.
"""

import argparse
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import socketserver
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

sys.dont_write_bytecode = True
TOOLS = Path(__file__).resolve().parent / "make_valid_handoff/package/tools"
sys.path.insert(0, str(TOOLS))
from runtime_hash import hash_path


class Gateway(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def reject(self, code, message):
        payload = json.dumps({"type": "error", "error": {"type": "invalid_request_error", "message": message}}).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        self.wfile.write(payload)
        self.server.audit({"rejected_status": code, "reason": message})

    def do_GET(self):
        self.reject(403, "Only the approved Messages API routes are available")

    def do_POST(self):
        if self.path.split("?", 1)[0] not in {"/v1/messages", "/v1/messages/count_tokens"}:
            return self.reject(403, "Unapproved endpoint")
        try:
            length = int(self.headers.get("Content-Length", "0"))
            if not 0 < length <= 32 * 1024 * 1024:
                return self.reject(413, "Request size outside gateway policy")
            payload = self.rfile.read(length)
            request = json.loads(payload)
            if request.get("model") != "claude-opus-5-5":
                return self.reject(403, "Only claude-opus-5-5 is approved; no fallback")
            for tool in request.get("tools", []):
                if tool.get("type", "custom") not in {"custom"} or tool.get("name") not in {"Bash", "Edit", "Read", "Write", "Glob", "Grep"}:
                    return self.reject(403, "Server-side tools are not approved")
            def external_input(value):
                if isinstance(value, dict):
                    source = value.get("source")
                    if isinstance(source, dict) and source.get("type") == "url":
                        return True
                    return any(external_input(v) for v in value.values())
                return isinstance(value, list) and any(external_input(v) for v in value)
            if request.get("mcp_servers") or external_input(request):
                return self.reject(403, "Remote MCP and URL-backed inputs are not approved")
            # Read only in the host broker. Neither contents nor tokens are logged.
            credentials = json.loads(Path("/home/mavrick/.claude/.credentials.json").read_text())
            token = credentials["claudeAiOauth"]["accessToken"]
            beta = [x.strip() for x in self.headers.get("anthropic-beta", "").split(",") if x.strip()]
            for required in ("oauth-2025-04-20", "claude-code-20250219"):
                if required not in beta:
                    beta.append(required)
            headers = {
                "Authorization": "Bearer " + token, "Content-Type": "application/json",
                "anthropic-version": self.headers.get("anthropic-version", "2023-06-01"),
                "anthropic-beta": ",".join(beta), "User-Agent": "claude-code/2.1.301",
            }
            upstream = urllib.request.Request("https://api.anthropic.com" + self.path, data=payload, headers=headers, method="POST")
            with urllib.request.urlopen(upstream, timeout=240) as response:
                self.send_response(response.status)
                self.send_header("Content-Type", response.headers.get("Content-Type", "application/json"))
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True
                while chunk := response.read1(65536):
                    self.wfile.write(chunk)
                    self.wfile.flush()
            self.server.audit({"model": request["model"], "route": self.path.split("?", 1)[0], "status": 200, "request_sha256": hashlib.sha256(payload).hexdigest(), "tools": [t.get("name", t.get("type")) for t in request.get("tools", [])]})
        except urllib.error.HTTPError as error:
            # Do not echo provider response bodies, credentials, or headers into logs.
            self.server.audit({"route": self.path.split("?", 1)[0], "upstream_status": error.code})
            self.reject(error.code, "Model upstream rejected the request; see sanitized custodian audit")
        except (OSError, ValueError, KeyError) as error:
            self.server.audit({"gateway_error_type": type(error).__name__})
            self.reject(502, "Model gateway transport error")


class UnixServer(socketserver.ThreadingUnixStreamServer):
    daemon_threads = True


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package", type=Path)
    parser.add_argument("--state-root", type=Path, default=Path("/mnt/scratch/terrio/isolated-opus"))
    parser.add_argument("--prompt", required=True, type=Path)
    parser.add_argument("--smoke", action="store_true")
    args = parser.parse_args()
    root = args.package.resolve()
    subprocess.run([sys.executable, str(root / "tools/verify_package.py")], check=True)
    runtime = json.loads((root / "HOST-RUNTIME.json").read_text())
    for binding in runtime["reference_bindings"]:
        if hash_path(binding["host"]) != binding["sha256"]:
            raise SystemExit("external reference runtime changed")
    args.state_root.mkdir(parents=True, exist_ok=True)
    state = Path(tempfile.mkdtemp(prefix="smoke-" if args.smoke else "implementation-", dir=args.state_root))
    state.chmod(0o700)
    (state / "agent-state").mkdir()
    (state / "prompt.txt").write_bytes(args.prompt.read_bytes())
    # Freeze execution policy alongside each run; later edits cannot silently
    # change a mounted entrypoint or be mistaken for the policy of an old run.
    (state / "launcher.py").write_bytes(Path(__file__).read_bytes())
    (state / "entry.py").write_bytes(Path(__file__).with_name("isolated_claude_entry.py").read_bytes())
    socket_path = state / "model.sock"
    audit_file = state / "gateway-audit.jsonl"

    def audit(item):
        item["time"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
        with audit_file.open("a") as stream:
            stream.write(json.dumps(item) + "\n")

    server = UnixServer(str(socket_path), Gateway)
    server.audit = audit
    threading.Thread(target=server.serve_forever, daemon=True).start()
    claude = Path(shutil.which("claude")).resolve()
    build = Path(runtime["build_directory"])
    command = [
        "/usr/bin/bwrap", "--unshare-all", "--unshare-user", "--die-with-parent", "--new-session", "--disable-userns", "--clearenv",
        "--dir", "/usr", "--ro-bind", "/usr/bin", "/usr/bin", "--ro-bind", "/usr/lib", "/usr/lib",
        "--ro-bind", "/usr/libexec", "/usr/libexec", "--ro-bind", "/usr/lib64", "/usr/lib64",
        "--symlink", "usr/bin", "/bin", "--symlink", "usr/lib", "/lib", "--symlink", "usr/lib64", "/lib64",
        "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--dir", "/home/implementer", "--dir", "/reference/lib",
        "--ro-bind", runtime["toolchain"], "/toolchain", "--ro-bind", str(root), "/work",
        "--bind", str(root / "candidate"), "/work/candidate", "--bind", str(root / "notes"), "/work/notes",
        "--bind", str(build), "/build", "--bind", str(state / "agent-state"), "/agent-state",
        "--dir", "/bridge", "--ro-bind", str(socket_path), "/bridge/model.sock",
        "--dir", "/agent-tools", "--ro-bind", str(claude), "/agent-tools/claude",
        "--ro-bind", str(state / "entry.py"), "/agent-tools/entry.py",
        "--ro-bind", str(state / "prompt.txt"), "/agent-tools/prompt.txt",
    ]
    for binding in runtime["reference_bindings"]:
        command += ["--ro-bind", binding["host"], binding["sandbox"]]
    environment = {
        "HOME": "/home/implementer", "PATH": "/toolchain/bin:/usr/bin:/bin", "CARGO_HOME": "/tmp/cargo",
        "CARGO_TARGET_DIR": "/build", "CARGO_BUILD_JOBS": "2", "CARGO_PROFILE_DEV_DEBUG": "0", "CARGO_PROFILE_TEST_DEBUG": "0",
        "CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER": "/usr/bin/gcc", "CARGO_NET_OFFLINE": "true",
        "PYTHONPATH": "/reference", "PYTHONDONTWRITEBYTECODE": "1", "LD_LIBRARY_PATH": "/reference/lib",
        "LANG": "C.UTF-8", "OPENBLAS_NUM_THREADS": "1", "OMP_NUM_THREADS": "1",
    }
    for key, value in environment.items():
        command += ["--setenv", key, value]
    command += ["--chdir", "/work", "--", "/usr/bin/python3", "/agent-tools/entry.py"]
    if args.smoke:
        command += ["--smoke"]
    metadata = {
        "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "launcher_sha256": hashlib.sha256((state / "launcher.py").read_bytes()).hexdigest(),
        "entrypoint_sha256": hashlib.sha256((state / "entry.py").read_bytes()).hexdigest(),
        "model": "claude-opus-5-5", "claude_binary_sha256": hashlib.sha256(claude.read_bytes()).hexdigest(),
        "manifest_sha256": hashlib.sha256((root / "MANIFEST.json").read_bytes()).hexdigest(),
        "prompt_sha256": hashlib.sha256(args.prompt.read_bytes()).hexdigest(),
        "credential_mounts": [], "mcp_servers": [], "network": "private namespace; model-only Unix gateway",
        "tools": ["Bash", "Read", "Write", "Edit", "Glob", "Grep"], "command": command,
    }
    (state / "run.json").write_text(json.dumps(metadata, indent=2) + "\n")
    print("Isolated run state: " + str(state), flush=True)
    try:
        with (state / "events.jsonl").open("w") as output, (state / "stderr.log").open("w") as errors:
            result = subprocess.run(command, stdout=output, stderr=errors)
        print("Isolated Claude exit: " + str(result.returncode), flush=True)
        print("Events: " + str(state / "events.jsonl"), flush=True)
        raise SystemExit(result.returncode)
    finally:
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()

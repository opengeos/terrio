"""Inside-sandbox entrypoint: model socket relay, verified boundary, fresh Claude CLI."""

import argparse
import json
import os
from pathlib import Path
import socket
import socketserver
import subprocess
import threading


class Relay(socketserver.BaseRequestHandler):
    def handle(self):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as remote:
            remote.connect("/bridge/model.sock")

            def copy(source, target):
                try:
                    while chunk := source.recv(65536):
                        target.sendall(chunk)
                except OSError:
                    pass
                finally:
                    try:
                        target.shutdown(socket.SHUT_WR)
                    except OSError:
                        pass

            forward = threading.Thread(target=copy, args=(self.request, remote), daemon=True)
            forward.start()
            copy(remote, self.request)
            forward.join(timeout=2)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--prompt", default="/agent-tools/prompt.txt")
    parser.add_argument("--smoke", action="store_true")
    args = parser.parse_args()
    subprocess.run(["python3", "tools/check_boundary.py"], check=True)
    state = Path("/agent-state")
    (state / "home").mkdir(exist_ok=True)
    (state / "claude").mkdir(exist_ok=True)
    Path("/build/tmp").mkdir(exist_ok=True)
    os.environ.update({
        "HOME": "/agent-state/home", "CLAUDE_CONFIG_DIR": "/agent-state/claude",
        "ANTHROPIC_BASE_URL": "http://127.0.0.1:39876",
        "ANTHROPIC_API_KEY": "isolated-gateway-placeholder-not-a-provider-credential",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC": "1",
        "DISABLE_AUTOUPDATER": "1", "DISABLE_TELEMETRY": "1", "DISABLE_ERROR_REPORTING": "1",
        "TMPDIR": "/build/tmp",
        "ANTHROPIC_DEFAULT_OPUS_MODEL": "claude-opus-5-5",
        "ANTHROPIC_DEFAULT_SONNET_MODEL": "claude-opus-5-5",
        "ANTHROPIC_DEFAULT_HAIKU_MODEL": "claude-opus-5-5",
    })
    server = socketserver.ThreadingTCPServer(("127.0.0.1", 39876), Relay)
    server.daemon_threads = True
    threading.Thread(target=server.serve_forever, daemon=True).start()
    # The only listener is inside a private network namespace; the host has no web port.
    command = [
        "/agent-tools/claude", "--bare", "--print", "--model", "claude-opus-5-5",
        "--effort", "high", "--strict-mcp-config", "--mcp-config", '{"mcpServers":{}}',
        "--setting-sources", "", "--settings", '{"disableAllHooks":true}',
        "--disable-slash-commands", "--no-session-persistence", "--dangerously-skip-permissions",
        "--tools", "Bash,Read,Write,Edit,Glob,Grep", "--allowedTools", "Bash,Read,Write,Edit,Glob,Grep",
        "--output-format", "stream-json", "--verbose",
    ]
    prompt = Path(args.prompt).read_text()
    if args.smoke:
        prompt = "Return exactly ISOLATED_OPUS_READY. Do not use tools. This checks the model-only gateway."
    result = subprocess.run(command, input=prompt, text=True)
    server.shutdown()
    raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()

"""Security regression checks: the agent cannot turn its model gateway into network access."""

import http.client
import io
import json
from pathlib import Path
import socket
import tempfile
import threading
import unittest
from unittest.mock import patch

from run_isolated_claude import Gateway, UnixServer


class GatewayTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.address = str(Path(self.directory.name) / "test.sock")
        self.server = UnixServer(self.address, Gateway)
        self.audit = []
        self.server.audit = self.audit.append
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.directory.cleanup()

    def request(self, route, payload, method="POST"):
        connection = http.client.HTTPConnection("unused")
        connection.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        connection.sock.connect(self.address)
        body = json.dumps(payload)
        connection.request(method, route, body=body, headers={"Content-Type": "application/json", "Authorization": "Bearer client-placeholder"})
        response = connection.getresponse()
        output = response.status, response.read()
        connection.close()
        return output

    def test_rejects_nonmodel_routes_and_other_models_without_reading_auth(self):
        with patch.object(Path, "read_text", side_effect=AssertionError("credentials must not be read")):
            for method, route, body in [
                ("GET", "/v1/messages", {}),
                ("POST", "/threads", {"model": "claude-opus-5-5"}),
                ("POST", "/v1/messages", {"model": "claude-sonnet-5-5"}),
                ("POST", "/v1/messages", {"model": "claude-opus-5-5", "tools": [{"type": "web_search_20250305", "name": "web_search"}]}),
                ("POST", "/v1/messages", {"model": "claude-opus-5-5", "tools": [{"name": "mcp__thread_search"}]}),
                ("POST", "/v1/messages", {"model": "claude-opus-5-5", "mcp_servers": [{"url": "https://example.invalid"}]}),
                ("POST", "/v1/messages", {"model": "claude-opus-5-5", "messages": [{"content": [{"type": "document", "source": {"type": "url", "url": "https://example.invalid/source.pdf"}}]}]}),
            ]:
                self.assertEqual(self.request(route, body, method)[0], 403)

    def test_only_host_credential_reaches_fixed_upstream(self):
        seen = []

        def upstream(request, **_):
            seen.append(request)
            response = io.BytesIO(b'data: {"type":"message_stop"}\n\n')
            response.status = 200
            response.headers = {"Content-Type": "text/event-stream"}
            return response

        credential = json.dumps({"claudeAiOauth": {"accessToken": "synthetic-server-credential"}})
        with patch.object(Path, "read_text", return_value=credential), patch("urllib.request.urlopen", side_effect=upstream):
            status, body = self.request("/v1/messages", {"model": "claude-opus-5-5", "tools": [{"name": "Bash"}]})
        self.assertEqual(status, 200)
        self.assertEqual(seen[0].full_url, "https://api.anthropic.com/v1/messages")
        self.assertEqual(seen[0].get_header("Authorization"), "Bearer synthetic-server-credential")
        self.assertNotIn(b"synthetic-server-credential", body)
        self.assertNotIn("synthetic-server-credential", json.dumps(self.audit))


if __name__ == "__main__":
    unittest.main()

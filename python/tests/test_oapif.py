# SPDX-FileCopyrightText: Copyright (c) 2026 Qiusheng Wu and Terrio contributors
# SPDX-License-Identifier: MIT
"""Real HTTP discovery and paged reads through the Python adapter."""

import json
from contextlib import contextmanager
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from threading import Thread
from urllib.parse import parse_qs, urlsplit

import terrio


@contextmanager
def service():
    requests = []

    class Handler(BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def do_GET(self):
            requests.append(self.path)
            path = urlsplit(self.path)
            if path.path == "/collections":
                value = {
                    "collections": [
                        {
                            "id": "places",
                            "links": [
                                {
                                    "rel": "items",
                                    "href": "/collections/places/items",
                                    "type": "application/geo+json",
                                },
                                {
                                    "rel": "describedby",
                                    "href": "/schema",
                                    "type": "application/schema+json",
                                },
                            ],
                        }
                    ]
                }
            elif path.path == "/schema":
                value = {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"},
                        "missing": {"type": "integer"},
                    },
                }
            else:
                query = parse_qs(path.query)
                start = int(query.get("offset", [0])[0])
                end = min(start + int(query.get("limit", [2])[0]), 5)
                value = {
                    "type": "FeatureCollection",
                    "numberMatched": 5,
                    "features": [
                        {
                            "type": "Feature",
                            "id": i,
                            "properties": {"name": f"p{i}"},
                            "geometry": {"type": "Point", "coordinates": [10 + i, 20 + i, 3]},
                        }
                        for i in range(start, end)
                    ],
                    "links": [{"rel": "next", "href": f"?offset={end}&limit=2"}] if end < 5 else [],
                }
            body = json.dumps(value).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    worker = Thread(target=server.serve_forever, daemon=True)
    worker.start()
    try:
        yield f"OAPIF:http://127.0.0.1:{server.server_port}/collections", requests
    finally:
        server.shutdown()
        server.server_close()
        worker.join()


def test_oapif_pages_schema_and_range():
    with service() as (url, requests):
        ds = terrio.open_vector(
            url, options={"PAGE_SIZE": "2", "OAPIF_SKIP_FEATURES": "1", "OAPIF_MAX_FEATURES": "3"}
        )
        geojson = ds.layers[0].to_geojson()
        assert [f["properties"]["name"] for f in geojson["features"]] == ["p1", "p2", "p3"]
        assert geojson["features"][0]["properties"] == {"name": "p1", "missing": None}
        assert geojson["features"][0]["geometry"]["coordinates"] == [11, 21, 3]
        assert len(requests) == 4, requests
        assert sum("/items" in path for path in requests) == 2

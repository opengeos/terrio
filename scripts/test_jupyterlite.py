#!/usr/bin/env python3
"""Runs the notebooks of a built JupyterLite site in a headless browser.

Usage: test_jupyterlite.py SITE_DIRECTORY [NOTEBOOK ...]

Exits with an error when a code cell of a notebook shows an error or does not run.
Needs Playwright with Chromium.
"""

import functools
import http.server
import json
import re
import sys
import threading
from pathlib import Path

from playwright.sync_api import sync_playwright

TIMEOUT_MS = 480_000

STDERR = ".jp-OutputArea-output[data-mime-type='application/vnd.jupyter.stderr']"
# Error output contains this, case-insensitively, in Python and in the browser.
ERROR = "traceback|error"

# True once every code cell has an execution count, or a cell has failed, which stops the run.
FINISHED = """([cells, stderr, error]) => {
    const failed = [...document.querySelectorAll(stderr)]
        .some((e) => new RegExp(error, "i").test(e.innerText));
    const prompts = [...document.querySelectorAll(".jp-CodeCell .jp-InputArea-prompt")];
    const ran = prompts.every((p) => /^\\[\\d+\\]:$/.test(p.innerText.trim()));
    return failed || (prompts.length === cells && ran);
}"""


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    """Serves the site without logging every request."""

    def log_message(self, format, *args):
        """Discards a log message.

        Args:
            format: The message's format.
            *args: The message's arguments.
        """


def code_cells(path):
    """Counts the code cells of a notebook.

    Args:
        path: The notebook's path.

    Returns:
        The number of code cells.
    """
    cells = json.loads(path.read_text(encoding="utf-8"))["cells"]
    return sum(cell["cell_type"] == "code" for cell in cells)


def run_notebook(browser, base_url, name, cells):
    """Runs all the cells of a notebook of the site.

    Args:
        browser: The Playwright browser.
        base_url: The URL of the site.
        name: The notebook's file name.
        cells: The number of code cells of the notebook.

    Returns:
        The text of the errors shown by the notebook; empty when there are none.
    """
    page = browser.new_page(viewport={"width": 1400, "height": 1000})
    page.goto(f"{base_url}/notebooks/index.html?path={name}")
    page.wait_for_selector(".jp-Notebook .jp-CodeCell", timeout=TIMEOUT_MS)
    page.get_by_role("menuitem", name="Run", exact=True).click()
    page.get_by_role("menuitem", name="Run All Cells", exact=True).click()
    page.wait_for_function(FINISHED, arg=[cells, STDERR, ERROR], timeout=TIMEOUT_MS)
    errors = page.locator(STDERR)
    texts = [errors.nth(i).inner_text() for i in range(errors.count())]
    page.close()
    return [text for text in texts if re.search(ERROR, text, re.IGNORECASE)]


def main():
    """Serves the site, runs its notebooks and reports the errors.

    Returns:
        The process's exit status.
    """
    site = Path(sys.argv[1])
    names = sys.argv[2:] or sorted(p.name for p in (site / "files").glob("*.ipynb"))
    if not names:
        print(f"no notebooks in {site / 'files'}")
        return 1
    handler = functools.partial(QuietHandler, directory=site)
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base_url = f"http://127.0.0.1:{server.server_port}"
    status = 0
    with sync_playwright() as playwright:
        browser = playwright.chromium.launch()
        for name in names:
            errors = run_notebook(browser, base_url, name, code_cells(site / "files" / name))
            print(f"{name}: {'ok' if not errors else 'FAILED'}")
            for error in errors:
                print(error)
                status = 1
        browser.close()
    return status


if __name__ == "__main__":
    sys.exit(main())

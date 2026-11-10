"""Draft the changelog section of a release from the pull requests merged since the last one.

Usage: python scripts/draft_changelog.py [--since TAG] [--until REF] [--version X.Y.Z]

Feature pull requests do not edit ``CHANGELOG.md``; the release pull request writes the section for
its version. This script drafts that section: it takes the pull request numbers from the squash
commit subjects (``... (#123)``) in ``TAG..REF`` (by default from the last ``v*`` tag before
``REF``, and ``REF`` is ``HEAD``), reads their titles and descriptions with ``gh``, and prints a Markdown section with one
entry per pull request, the description's first paragraph followed by its number. Pull requests
of type ``ci``, ``chore``, ``build`` or ``test``, release pull requests (``Release X.Y.Z``) and
those ``gh`` cannot read (with a warning) are left out, and the
rest are grouped by the type of their title (features, fixes, performance, then the others). The
draft is a starting point: edit the entries into the changelog's style before committing them.
Needs ``git`` and an authenticated ``gh``.
"""

from __future__ import annotations

import argparse
import datetime
import json
import re
import subprocess
import sys

SKIPPED_TYPES = {"ci", "chore", "build", "test", "release"}
GROUPS = [("feat", "Features"), ("fix", "Fixes"), ("perf", "Performance")]
OTHER_GROUP = "Other changes"
PR_NUMBER = re.compile(r"\(#(\d+)\)\s*$")
TITLE_TYPE = re.compile(r"^(\w+)(?:\([^)]*\))?!?:")
# The release pull request's title, matched as the Changelog workflow does.
RELEASE_TITLE = re.compile(r"Release \d+\.\d+\.\d+")
# Paragraphs that only link the pull request to issues or other pull requests.
REFERENCE_ONLY = re.compile(
    r"^(?:(?:closes|fixes|resolves|part of|follow-up to|refs?)\s+#\d+[.,]?\s*)+$", re.IGNORECASE
)


def run(args: list[str]) -> str:
    """Run a command and return its standard output.

    Args:
        args: The command and its arguments.

    Returns:
        The command's standard output as text.
    """
    return subprocess.run(args, check=True, capture_output=True, text=True).stdout


def previous_tag(until: str) -> str:
    """Return the most recent ``v*`` tag before a revision.

    The tag is looked for from the revision's parent, so a revision that is itself tagged gives
    the release before it.

    Args:
        until: The revision the draft ends at.

    Returns:
        The tag name, such as ``v0.8.0``.
    """
    return run(["git", "describe", "--tags", "--abbrev=0", "--match", "v*", f"{until}^"]).strip()


def merged_pr_numbers(since: str, until: str) -> list[int]:
    """List the pull request numbers named by the commit subjects in a range.

    Args:
        since: The revision the range starts after, such as the previous release tag.
        until: The revision the range ends at.

    Returns:
        The pull request numbers, oldest first.
    """
    subjects = run(["git", "log", "--reverse", "--format=%s", f"{since}..{until}"]).splitlines()
    numbers = [int(m.group(1)) for s in subjects if (m := PR_NUMBER.search(s))]
    return list(dict.fromkeys(numbers))


def fetch_pr(number: int) -> dict | None:
    """Read a pull request's title and description with ``gh``.

    Args:
        number: The pull request number.

    Returns:
        A dict with the ``number``, ``title`` and ``body`` of the pull request, or ``None``
        (after a warning on standard error) when ``gh`` cannot read it, as for an issue number.
    """
    try:
        return json.loads(run(["gh", "pr", "view", str(number), "--json", "number,title,body"]))
    except subprocess.CalledProcessError as err:
        print(f"warning: skipped #{number}: {err.stderr.strip()}", file=sys.stderr)
        return None


def title_type(title: str) -> str:
    """Return the conventional-commit type of a pull request title.

    Args:
        title: The pull request title, such as ``fix(vrt): read fractional windows``.

    Returns:
        The lower-case type (``fix``), ``release`` for a release pull request, or an empty
        string when the title has no type.
    """
    if RELEASE_TITLE.match(title):
        return "release"
    match = TITLE_TYPE.match(title)
    return match.group(1).lower() if match else ""


def first_paragraph(body: str | None) -> str:
    """Return a pull request description's first paragraph of prose, on one line.

    Headings, tables, fenced code blocks, HTML comments (which hold the review bots' generated
    summaries), wholly bold notes such as ``**Stacked on #733.**`` and paragraphs that only
    reference issues (``Closes #732.``) are skipped; prose right below a heading is kept.

    Args:
        body: The pull request description, possibly empty.

    Returns:
        The first paragraph, or an empty string when there is none.
    """
    text = re.sub(r"<!--.*?-->|```.*?```", "", body or "", flags=re.DOTALL)
    text = re.sub(r"(?m)^\s*#.*$", "", text)
    for paragraph in re.split(r"\n\s*\n", text):
        paragraph = paragraph.strip()
        if (
            paragraph
            and not paragraph.startswith(("|", "```"))
            and not re.fullmatch(r"\*\*[^*]*\*\*\.?", paragraph)
            and not REFERENCE_ONLY.match(paragraph)
        ):
            # A description that opens with a list gives its first item.
            if re.match(r"[-*]\s", paragraph):
                paragraph = re.split(r"\n\s*[-*]\s", paragraph[2:])[0]
            return " ".join(paragraph.split())
    return ""


def entry(pr: dict) -> str:
    """Format one changelog entry for a pull request.

    Args:
        pr: A pull request as returned by :func:`fetch_pr`.

    Returns:
        A Markdown list item: the description's first paragraph, or the title when the
        description is empty, followed by the pull request number.
    """
    text = first_paragraph(pr["body"]) or pr["title"]
    return f"- {text} (#{pr['number']})"


def draft(prs: list[dict], version: str | None) -> str:
    """Build the changelog section from a list of pull requests.

    Args:
        prs: Pull requests as returned by :func:`fetch_pr`, oldest first.
        version: The release version for the heading, or ``None`` for ``Unreleased``.

    Returns:
        The Markdown section.
    """
    groups: dict[str, list[str]] = {}
    for pr in prs:
        kind = title_type(pr["title"])
        if kind in SKIPPED_TYPES:
            continue
        name = dict(GROUPS).get(kind, OTHER_GROUP)
        groups.setdefault(name, []).append(entry(pr))
    heading = f"{version} ({datetime.date.today().isoformat()})" if version else "Unreleased"
    lines = [f"## {heading}", ""]
    for name in [g for _, g in GROUPS] + [OTHER_GROUP]:
        if name in groups:
            lines += [f"### {name}", "", *groups[name], ""]
    return "\n".join(lines)


def main() -> int:
    """Print the drafted changelog section.

    Returns:
        The process exit status.
    """
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--since", help="previous release tag (default: the last v* tag before --until)")
    parser.add_argument("--until", default="HEAD", help="end of the range (default: HEAD)")
    parser.add_argument("--version", help="version for the heading (default: Unreleased)")
    args = parser.parse_args()
    since = args.since or previous_tag(args.until)
    prs = [pr for n in merged_pr_numbers(since, args.until) if (pr := fetch_pr(n))]
    print(draft(prs, args.version))
    return 0


if __name__ == "__main__":
    sys.exit(main())

"""Draft the changelog section of a release from the pull requests merged since the last one.

Usage: python scripts/draft_changelog.py [--since TAG] [--until REF] [--version X.Y.Z]

Feature pull requests do not edit ``CHANGELOG.md``; the release pull request writes the section for
its version. This script drafts that section: it takes the pull request numbers from the squash
commit subjects (``... (#123)``) in ``TAG..REF``, where ``--until`` gives ``REF`` (default
``HEAD``) and ``--since`` gives ``TAG`` (default: the last ``v*`` tag before ``REF``). It reads
their titles and descriptions with ``gh`` and prints a Markdown section with one entry per pull
request, the description's first paragraph followed by its number. Pull requests of type ``ci``,
``chore``, ``build`` or ``test``, release pull requests (``Release X.Y.Z``) and numbers that name
no pull request (with a warning) are left out; the rest are grouped by the type of their title
(features, fixes, performance, then the others). The draft is a starting point: edit the entries
into the changelog's style before committing them. Needs ``git`` with full history and tags, and
an authenticated ``gh``.
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
RELEASE_TITLE = re.compile(r"Release [0-9]+\.[0-9]+\.[0-9]+")
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

    Raises:
        SystemExit: When no ``v*`` tag precedes the revision, as in a shallow clone without tags.
    """
    try:
        return run(
            ["git", "describe", "--tags", "--abbrev=0", "--match", "v*", f"{until}^"]
        ).strip()
    except subprocess.CalledProcessError as err:
        raise SystemExit(
            f"error: no v* tag before {until} ({err.stderr.strip()}); pass --since, or run in a"
            " full clone with its tags"
        ) from None


def merged_pr_numbers(since: str, until: str) -> list[int]:
    """List the pull request numbers named by the commit subjects in a range.

    Args:
        since: The revision the range starts after, such as the previous release tag.
        until: The revision the range ends at.

    Returns:
        The pull request numbers, oldest first.

    Raises:
        SystemExit: When git cannot list the range, as for a revision that does not exist.
    """
    try:
        log = run(["git", "log", "--reverse", "--format=%s", f"{since}..{until}"])
    except subprocess.CalledProcessError as err:
        raise SystemExit(f"error: cannot list {since}..{until}: {err.stderr.strip()}") from None
    subjects = log.splitlines()
    numbers = [int(m.group(1)) for s in subjects if (m := PR_NUMBER.search(s))]
    return list(dict.fromkeys(numbers))


def fetch_pr(number: int) -> dict | None:
    """Read a pull request's title and description with ``gh``.

    Args:
        number: The pull request number.

    Returns:
        A dict with the ``number``, ``title`` and ``body`` of the pull request, or ``None``
        (after a warning on standard error) when the number names no pull request, as an
        issue number does.

    Raises:
        SystemExit: When ``gh`` is missing or fails for another reason, such as no
            authentication, so that a failure does not print an empty draft.
    """
    try:
        return json.loads(run(["gh", "pr", "view", str(number), "--json", "number,title,body"]))
    except FileNotFoundError:
        raise SystemExit("error: gh is not installed") from None
    except subprocess.CalledProcessError as err:
        message = err.stderr.strip()
        if "Could not resolve to a PullRequest" not in message:
            raise SystemExit(f"error: gh could not read #{number}: {message}") from None
        print(f"warning: skipped #{number}: not a pull request", file=sys.stderr)
        return None


def title_type(title: str) -> str:
    """Return the conventional-commit type of a pull request title.

    Args:
        title: The pull request title, such as ``fix(vrt): read fractional windows``.

    Returns:
        The lower-case type (``fix``), ``release`` for a release pull request, or an empty
        string when the title has no type.
    """
    if RELEASE_TITLE.fullmatch(title):
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
    text = re.sub(r"(?m)^[ \t]*#{1,6}[ \t].*$", "", text)
    for paragraph in re.split(r"\n\s*\n", text):
        paragraph = paragraph.strip()
        # The text before a list and each of its items are candidates of their own; a lead-in
        # such as "Changes:" is not an entry.
        for candidate in re.split(r"(?m)^[ \t]*[-*][ \t]+", paragraph):
            candidate = " ".join(candidate.split())
            if (
                candidate
                and not candidate.endswith(":")
                and not candidate.startswith(("|", "```"))
                and not re.fullmatch(r"\*\*[^*]*\*\*\.?", candidate)
                and not REFERENCE_ONLY.match(candidate)
            ):
                return candidate
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
    parser.add_argument(
        "--since", help="previous release tag (default: the last v* tag before --until)"
    )
    parser.add_argument("--until", default="HEAD", help="end of the range (default: HEAD)")
    parser.add_argument("--version", help="version for the heading (default: Unreleased)")
    args = parser.parse_args()
    since = args.since or previous_tag(args.until)
    prs = [pr for n in merged_pr_numbers(since, args.until) if (pr := fetch_pr(n))]
    print(draft(prs, args.version))
    return 0


if __name__ == "__main__":
    sys.exit(main())

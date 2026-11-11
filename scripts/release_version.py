"""The version of a release, from the trailers of HEAD (docs/release.md).

The same rules as the `version` job of the GitHub CI that it replaces
(TER-613):

- `pre-release: true` on HEAD: X.Y.Z-alpha, a pre-release.
- `release: true` on HEAD: X.Y.Z, a stable release.
- Neither: no release (`do_release=false`), exit 0.
- The bump comes from the conventional-commit subjects since the last `v*`
  tag (0.x semantics: a breaking change bumps the minor, not the major).
- `bump: major|minor|patch` on HEAD overrides the scan. Another value fails.
- `release: true` on top of the last tag vX.Y.Z-alpha promotes it: X.Y.Z
  with no bump, unless a bump trailer is set, `pre-release: true` is set
  too, or vX.Y.Z exists.

Usage: python3 scripts/release_version.py [--repo DIR] [--json]
It prints `key=value` lines (version, prerelease, do_release, bump,
last_tag, tested_at_cut), or one JSON object with --json. `tested_at_cut`
is true only for a release or pre-release with a non-empty tested-at-cut trailer.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys

BREAKING_SUBJECT = re.compile(r"^[a-z]+(\(.+\))?!:")
BREAKING_BODY = re.compile(r"^BREAKING[ -]CHANGE:", re.IGNORECASE | re.MULTILINE)
FEAT = re.compile(r"^feat(\(.+\))?:")
PATCH = re.compile(r"^(fix|perf|refactor|build|ci|docs|style|test|chore|revert)(\(.+\))?:")
RANK = {"none": 0, "patch": 1, "minor": 2, "major": 3}


class VersionError(Exception):
    pass


def git(repo: str, *args: str, check: bool = True) -> str:
    proc = subprocess.run(
        ["git", *args], cwd=repo, capture_output=True, text=True, check=False
    )
    if check and proc.returncode != 0:
        raise VersionError(f"git {' '.join(args)}: {proc.stderr.strip()}")
    return proc.stdout if proc.returncode == 0 else ""


def trailer(repo: str, key: str) -> str:
    """The values of trailer `key` on HEAD, without the final newlines (as `$(...)`)."""
    return git(repo, "log", "-1", f"--format=%(trailers:key={key},valueonly)").rstrip("\n")


def commit_bump(subject: str, body: str) -> str:
    if BREAKING_SUBJECT.search(subject) or BREAKING_BODY.search(body):
        return "major"
    if FEAT.search(subject):
        return "minor"
    if PATCH.search(subject):
        return "patch"
    return "none"


def calc(repo: str) -> dict[str, str]:
    is_release = trailer(repo, "release") == "true"
    is_pre = trailer(repo, "pre-release") == "true"
    bump_trailer = re.sub(r"\s", "", trailer(repo, "bump"))
    if not is_release and not is_pre:
        return {"do_release": "false", "tested_at_cut": "false"}

    tested_at_cut = bool(re.sub(r"\s", "", trailer(repo, "tested-at-cut")))

    last_tag = git(repo, "describe", "--tags", "--match", "v*", "--abbrev=0", check=False).strip()
    promote = False
    ver = ""
    if not last_tag:
        prev = (0, 0, 0)
        rev_range = "HEAD"
    else:
        ver = last_tag[1:].split("-", 1)[0]
        parts = ver.split(".")
        try:
            prev = tuple(int(p) for p in (parts + ["0", "0", "0"])[:3])
        except ValueError as e:
            raise VersionError(f"the last tag {last_tag} is not vX.Y.Z: {e}") from e
        rev_range = f"{last_tag}..HEAD"
        if (
            is_release
            and not is_pre
            and last_tag.endswith("-alpha")
            and not bump_trailer
            and not git(repo, "tag", "--list", f"v{ver}").strip()
        ):
            promote = True

    bump = "none"
    for sha in git(repo, "log", "--format=%H", rev_range).split():
        subject = git(repo, "log", "-1", "--format=%s", sha).rstrip("\n")
        body = git(repo, "log", "-1", "--format=%b", sha)
        found = commit_bump(subject, body)
        if RANK[found] > RANK[bump]:
            bump = found
    if bump == "none":
        bump = "patch"
    if bump_trailer:
        if bump_trailer not in ("major", "minor", "patch"):
            raise VersionError(
                f"bump trailer must be major, minor, or patch (got '{bump_trailer}')"
            )
        bump = bump_trailer

    major, minor, patch = prev
    if bump == "major":
        if major == 0:
            major, minor, patch = 0, minor + 1, 0
        else:
            major, minor, patch = major + 1, 0, 0
    elif bump == "minor":
        minor, patch = minor + 1, 0
    else:
        patch += 1

    version = f"{major}.{minor}.{patch}"
    prerelease = "false"
    if is_pre:
        version += "-alpha"
        prerelease = "true"
    if promote:
        version = ver
        bump = f"promoted from {last_tag}"
    return {
        "do_release": "true",
        "version": version,
        "prerelease": prerelease,
        "bump": bump,
        "last_tag": last_tag,
        "tested_at_cut": "true" if tested_at_cut else "false",
    }


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--repo", default=".")
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args(argv)
    try:
        values = calc(a.repo)
    except VersionError as e:
        print(f"release_version: {e}", file=sys.stderr)
        return 1
    if a.json:
        print(json.dumps(values, sort_keys=True))
    else:
        for k, v in values.items():
            print(f"{k}={v}")
    return 0


if __name__ == "__main__":
    sys.exit(main())

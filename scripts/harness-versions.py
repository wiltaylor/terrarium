#!/usr/bin/env python3
"""Read harnesses.toml, and keep docker/Dockerfile in step with it.

harnesses.toml at the repo root is the one place for a harness version
(docs/CONVENTIONS.md). The image installs no harness: `terrariumd harness
install` installs the pins into the tools volume at the first start. This
script is stdlib only (Python 3.11+, tomllib).

    harness-versions.py check   exit 1 when the Dockerfile is not in step:
                                `ARG NODE_IMAGE` differs from [node] image,
                                an `npm install -g` installs a pinned package,
                                or harnesses.toml is not copied to
                                /etc/terrarium/harnesses.toml
    harness-versions.py sync    rewrite the NODE_IMAGE default
    harness-versions.py json    print the pins as JSON
"""

from __future__ import annotations

import argparse
import json
import re
import sys
import tomllib
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TOML = ROOT / "harnesses.toml"
DOCKERFILE = ROOT / "docker" / "Dockerfile"

HARNESS_KEYS = ("package", "version", "binary")
EXTENSION_KEYS = ("package", "version", "binary", "for")


class PinError(Exception):
    """harnesses.toml or the Dockerfile is not what this script expects."""


def load(path: Path = TOML) -> dict:
    """Read and check harnesses.toml."""
    try:
        cfg = tomllib.loads(path.read_text())
    except (OSError, tomllib.TOMLDecodeError) as e:
        raise PinError(f"{path}: {e}") from e
    return validate(cfg)


def validate(cfg: dict) -> dict:
    if not isinstance(cfg.get("node", {}).get("image"), str):
        raise PinError("harnesses.toml: [node] needs `image`")
    harnesses = cfg.get("harness")
    if not isinstance(harnesses, dict) or not harnesses:
        raise PinError("harnesses.toml: no [harness.<name>] table")
    for name, h in harnesses.items():
        missing = [k for k in HARNESS_KEYS if not isinstance(h.get(k), str)]
        if missing:
            raise PinError(f"harnesses.toml: [harness.{name}] needs {', '.join(missing)}")
    for name, e in cfg.get("extension", {}).items():
        missing = [k for k in EXTENSION_KEYS if not isinstance(e.get(k), str)]
        if missing:
            raise PinError(f"harnesses.toml: [extension.{name}] needs {', '.join(missing)}")
    return cfg


NODE_ARG = "NODE_IMAGE"
TOML_COPY_RE = re.compile(r"^COPY[ \t]+(?:--\S+[ \t]+)*harnesses\.toml[ \t]+/etc/terrarium/harnesses\.toml[ \t]*$", re.MULTILINE)
ARG_RE = re.compile(r"^ARG[ \t]+([A-Za-z_][A-Za-z0-9_]*)(?:=(\S*))?[ \t]*$", re.MULTILINE)


def packages(cfg: dict) -> list[str]:
    """Every pinned npm package: the harnesses, then the extensions."""
    out = [h["package"] for h in cfg["harness"].values()]
    out += [e["package"] for e in cfg.get("extension", {}).values()]
    return out


def dockerfile_defaults(text: str) -> dict[str, str | None]:
    """Every `ARG K[=V]` in the Dockerfile. The first one of a name wins."""
    found: dict[str, str | None] = {}
    for m in ARG_RE.finditer(text):
        found.setdefault(m.group(1), m.group(2))
    return found


def npm_installs(text: str) -> str:
    """The text of each `npm install -g` command, with its continued lines."""
    joined = re.sub(r"\\\n", " ", text)
    return "\n".join(
        line for line in joined.splitlines() if re.search(r"\bnpm\s+(?:install|i)\b.*\s(?:-g|--global)\b", line)
    )


def check_dockerfile(cfg: dict, text: str) -> list[str]:
    """What differs between harnesses.toml and the Dockerfile. Empty: in step."""
    problems = []
    image = cfg["node"]["image"]
    default = dockerfile_defaults(text).get(NODE_ARG, "")
    if default == "":
        problems.append(f"docker/Dockerfile has no `ARG {NODE_ARG}` default (harnesses.toml has {image})")
    elif default != image:
        problems.append(
            f"docker/Dockerfile: ARG {NODE_ARG}={default}, harnesses.toml: {image}"
            " (run `uv run python scripts/harness-versions.py sync`)"
        )
    installs = npm_installs(text)
    for package in packages(cfg):
        if re.search(re.escape(package) + r"(?:@|\b)", installs):
            problems.append(
                f"docker/Dockerfile installs {package} with `npm install -g`; the harnesses go in the tools "
                "volume (`terrariumd harness install`), not in the image"
            )
    if not TOML_COPY_RE.search(text):
        problems.append("docker/Dockerfile does not copy harnesses.toml to /etc/terrarium/harnesses.toml")
    return problems


def sync_dockerfile(cfg: dict, text: str) -> str:
    """The Dockerfile with the NODE_IMAGE default set from harnesses.toml."""
    done = False

    def repl(m: re.Match) -> str:
        nonlocal done
        if m.group(1) == NODE_ARG and not done:
            done = True
            return f"ARG {NODE_ARG}={cfg['node']['image']}"
        return m.group(0)

    return ARG_RE.sub(repl, text)


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("command", choices=["check", "sync", "json"])
    p.add_argument("--toml", type=Path, default=TOML, help="default: harnesses.toml at the repo root")
    p.add_argument("--dockerfile", type=Path, default=DOCKERFILE, help="default: docker/Dockerfile")
    a = p.parse_args(argv)
    try:
        cfg = load(a.toml)
        if a.command == "json":
            print(json.dumps(cfg, indent=2))
        elif a.command == "check":
            problems = check_dockerfile(cfg, a.dockerfile.read_text())
            for line in problems:
                print(line, file=sys.stderr)
            if problems:
                return 1
            print(f"{a.dockerfile.name} is in step with {a.toml.name}")
        else:
            text = a.dockerfile.read_text()
            new = sync_dockerfile(cfg, text)
            if new != text:
                a.dockerfile.write_text(new)
                print(f"updated the NODE_IMAGE default in {a.dockerfile}")
            else:
                print(f"{a.dockerfile} is in step")
    except (PinError, OSError) as e:
        print(f"harness-versions: {e}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())

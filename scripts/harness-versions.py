#!/usr/bin/env python3
"""Turn harnesses.toml into docker build args, and keep docker/Dockerfile in step.

harnesses.toml at the repo root is the one place for a harness version
(docs/CONVENTIONS.md). This script is stdlib only (Python 3.11+, tomllib).

    harness-versions.py build-args   print `--build-arg K=V ...` for docker build
    harness-versions.py check        exit 1 when the Dockerfile ARG defaults or
                                     its install line differ from harnesses.toml
    harness-versions.py sync         rewrite the Dockerfile ARG defaults
    harness-versions.py json         print the pins as JSON

Each value becomes one build arg:

    [node] image                      NODE_IMAGE
    [harness.<name>] version          <NAME>_VERSION      (CLAUDE_VERSION, ...)
    [extension.<name>] version        <NAME>_VERSION      (COPILOT_PROMPT_VERSION)
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

HARNESS_KEYS = ("package", "version", "tested", "binary")
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


def arg_name(name: str) -> str:
    """`copilot-prompt` -> `COPILOT_PROMPT_VERSION`."""
    return re.sub(r"[^A-Za-z0-9]", "_", name).upper() + "_VERSION"


def pins(cfg: dict) -> list[tuple[str, str, str | None]]:
    """(build arg, value, npm package or None) in file order."""
    out: list[tuple[str, str, str | None]] = [("NODE_IMAGE", cfg["node"]["image"], None)]
    for name, h in cfg["harness"].items():
        out.append((arg_name(name), h["version"], h["package"]))
    for name, e in cfg.get("extension", {}).items():
        out.append((arg_name(name), e["version"], e["package"]))
    return out


def build_args(cfg: dict) -> list[str]:
    """The `docker build` arguments, as a list."""
    args: list[str] = []
    for key, value, _ in pins(cfg):
        args += ["--build-arg", f"{key}={value}"]
    return args


ARG_RE = re.compile(r"^ARG[ \t]+([A-Za-z_][A-Za-z0-9_]*)(?:=(\S*))?[ \t]*$", re.MULTILINE)


def dockerfile_defaults(text: str) -> dict[str, str | None]:
    """Every `ARG K[=V]` in the Dockerfile. The first one of a name wins."""
    found: dict[str, str | None] = {}
    for m in ARG_RE.finditer(text):
        found.setdefault(m.group(1), m.group(2))
    return found


def check_dockerfile(cfg: dict, text: str) -> list[str]:
    """What differs between harnesses.toml and the Dockerfile. Empty: in step."""
    problems = []
    defaults = dockerfile_defaults(text)
    for key, value, package in pins(cfg):
        if key not in defaults:
            problems.append(f"docker/Dockerfile has no `ARG {key}` (harnesses.toml has {value})")
        elif defaults[key] != value:
            problems.append(
                f"docker/Dockerfile: ARG {key}={defaults[key]}, harnesses.toml: {value}"
                " (run `uv run python scripts/harness-versions.py sync`)"
            )
        if package and f'"{package}@${{{key}}}"' not in text:
            problems.append(f'docker/Dockerfile does not install "{package}@${{{key}}}"')
    return problems


def sync_dockerfile(cfg: dict, text: str) -> str:
    """The Dockerfile with every pinned ARG default set from harnesses.toml."""
    values = {key: value for key, value, _ in pins(cfg)}
    done: set[str] = set()

    def repl(m: re.Match) -> str:
        key = m.group(1)
        if key in values and key not in done:
            done.add(key)
            return f"ARG {key}={values[key]}"
        return m.group(0)

    return ARG_RE.sub(repl, text)


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("command", choices=["build-args", "check", "sync", "json"])
    p.add_argument("--toml", type=Path, default=TOML, help="default: harnesses.toml at the repo root")
    p.add_argument("--dockerfile", type=Path, default=DOCKERFILE, help="default: docker/Dockerfile")
    a = p.parse_args(argv)
    try:
        cfg = load(a.toml)
        if a.command == "build-args":
            print(" ".join(build_args(cfg)))
        elif a.command == "json":
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
                print(f"updated the ARG defaults in {a.dockerfile}")
            else:
                print(f"{a.dockerfile} is in step")
    except (PinError, OSError) as e:
        print(f"harness-versions: {e}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    sys.exit(main())

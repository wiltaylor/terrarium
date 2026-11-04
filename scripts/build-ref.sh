#!/bin/sh
# Print the git ref of a build for TERRARIUM_GIT_REF (crates/terrarium-version):
# the full commit of HEAD, with `-dirty` when tracked files have changes.
# Prints nothing outside a git checkout (the binaries then say `+unknown`).
set -u
cd "$(dirname "$0")/.." || exit 0
commit=$(git rev-parse --verify --quiet HEAD 2>/dev/null) || exit 0
if [ -n "$(git --no-optional-locks status --porcelain --untracked-files=no 2>/dev/null)" ]; then
	echo "$commit-dirty"
else
	echo "$commit"
fi

#!/bin/sh
# Spike 3b: --dangerously-skip-permissions as root, with and without IS_SANDBOX=1.
# Runs the host's claude binary in a throwaway debian container with NO credentials,
# so no model turn can happen. Usage: sh root-check.sh
BIN=$(readlink -f "${CLAUDE_BIN:-$HOME/.local/bin/claude}")
for env in "" "-e IS_SANDBOX=1"; do
  for user in root 1000; do
    echo "--- user=$user env='${env}'"
    docker run --rm -u $user $env -e HOME=/tmp -v "$BIN:/usr/local/bin/claude:ro" debian:bookworm-slim \
      sh -c 'timeout 20 claude --dangerously-skip-permissions -p "Reply with exactly: pong" 2>&1 | head -5; echo "rc=$?"'
  done
done

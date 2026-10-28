set working-directory := '../..'

import '../shared.just'

[default, private]
main:
	@just --list ci

# Everything a change must pass before it can merge
[group('ci')]
check: fmt-check lint test scripts-test dashboard-check

# Check Rust formatting without changing files
[group('ci')]
fmt-check: require-cargo
	cargo fmt --all --check

# Lint the workspace with clippy; warnings fail the gate
[group('ci')]
lint: require-cargo
	cargo clippy --workspace --all-targets -- -D warnings

# Run the Rust unit and integration tests
[group('ci')]
test: require-cargo
	cargo test --workspace

# Run the Python tests under scripts/ (harnesses.toml, the Dockerfile pins, the host installer, the deploy, the CI workflow)
[group('ci')]
scripts-test: require-uv
	uv run python -m unittest discover -s scripts -p 'test_*.py'

# Run the dashboard's `npm run check` (typecheck and lint); skips when dashboard/ has no package.json
[group('ci')]
dashboard-check:
	#!/usr/bin/env bash
	set -euo pipefail
	if [ ! -f dashboard/package.json ]; then
		echo "dashboard-check: skipped, dashboard/package.json does not exist"
		exit 0
	fi
	command -v npm > /dev/null 2>&1 || { echo "npm is required: install Node 22.19 or later" >&2; exit 1; }
	cd dashboard
	if [ ! -d node_modules ]; then
		if [ -f package-lock.json ]; then npm ci; else npm install; fi
	fi
	npm run check
	npm run test -- --run

# Adapter compatibility suite against the pinned harness versions (docs/compat-suite.md). Not in `check`: it needs real logins and spends up to 3 model turns per harness. `harness`: all, claude, codex, copilot, pi, or a comma list
[group('ci')]
compat harness='all': require-cargo
	cargo build -p terrarium
	PATH="{{justfile_directory()}}/tools/bin:$PATH" TERRARIUM_COMPAT_HARNESSES={{quote(harness)}} cargo test -p terrarium-compat --test compat -- --ignored --exact real_harnesses --nocapture

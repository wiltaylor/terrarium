set working-directory := '../..'

import '../shared.just'

[default, private]
main:
	@just --list ci

# Everything a change must pass before it can merge
[group('ci')]
check: fmt-check lint test dashboard-check

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

# Adapter compatibility suite against the pinned harness versions. Not in `check`: it needs real logins in config/ and the image
[group('ci')]
compat:
	@echo "ci::compat is not implemented yet: it arrives with the adapters in Phase 3 (PLAN §23.5)" >&2
	@exit 1

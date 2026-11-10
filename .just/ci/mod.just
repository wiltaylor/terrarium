set working-directory := '../..'

import '../shared.just'

[default, private]
main:
	@just --list ci

# Everything a change must pass before it can merge
[group('ci')]
check: fmt-check lint test scripts-test dashboard-check

# The validation gate for a card: every part GitHub CI runs (ci.yml check job), in CI's order, with CI's toolchain and cargo env. Run it on a clean checkout of the pushed commit (AGENTS.md)
[group('ci')]
validate: require-uv
	#!/usr/bin/env bash
	set -euo pipefail
	export RUSTUP_TOOLCHAIN=1.97.1 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
	# Stable test-only inputs across slot checkouts. Release builds do not use this gate.
	export TERRARIUM_GIT_REF=ci-test TERRARIUM_TEST_BUILD=1 SOURCE_DATE_EPOCH=0
	uv run python scripts/affected.py --check-commit-trailers
	just ci::fmt-check
	just ci::lint
	just ci::test
	if [ "${TERRARIUM_JOB_KIND:-}" = merge ]; then just ci::test-unprivileged; fi
	just ci::scripts-test
	just ci::dashboard-check

# The gate of a queue job (TER-100): a validate job runs only the checks that its changed files can reach (scripts/affected.py; the rules are in its header). A merge job, or a job without a list of changed files, runs the full `ci::validate`. Reads TERRARIUM_JOB_KIND and TERRARIUM_CHANGED_FILES from the job (docs/api.md 35.2)
[group('ci')]
validate-affected: require-uv
	#!/usr/bin/env bash
	set -euo pipefail
	export RUSTUP_TOOLCHAIN=1.97.1 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
	export TERRARIUM_GIT_REF=ci-test TERRARIUM_TEST_BUILD=1 SOURCE_DATE_EPOCH=0
	kind=${TERRARIUM_JOB_KIND:-}
	list=${TERRARIUM_CHANGED_FILES:-}
	if [ "$kind" = merge ]; then
		echo "validate-affected: a merge job runs the full suite"
		exec just ci::merge
	fi
	if [ -z "$list" ] || [ ! -f "$list" ]; then
		echo "validate-affected: no list of changed files, the full suite runs"
		exec just ci::validate
	fi
	uv run python scripts/affected.py --files "$list" --run

# The merge-only Docker key is resolved by the job runner, never by a worker.
[group('ci')]
merge:
	python3 scripts/docker_ci.py

# Print the plan of `validate-affected` without running it: the changed files of TERRARIUM_CHANGED_FILES, else the diff of HEAD against its merge-base with `base`
[group('ci')]
affected base='origin/next': require-uv
	#!/usr/bin/env bash
	set -euo pipefail
	if [ -n "${TERRARIUM_CHANGED_FILES:-}" ]; then
		exec uv run python scripts/affected.py --files "$TERRARIUM_CHANGED_FILES"
	fi
	f=$(mktemp)
	trap 'rm -f "$f"' EXIT
	git diff --name-only --no-renames "$(git merge-base HEAD {{quote(base)}})" HEAD > "$f"
	uv run python scripts/affected.py --files "$f"

# Check Rust formatting without changing files
[group('ci')]
fmt-check: require-cargo
	cargo fmt --all --check

# Lint the workspace with clippy; warnings fail the gate
[group('ci')]
lint: require-cargo
	cargo clippy --workspace --all-targets -- -D warnings

# Install the same pinned runner locally, in merge jobs, and in GitHub CI.
[group('ci')]
install-nextest: require-cargo
	#!/usr/bin/env bash
	set -euo pipefail
	case "$(cargo nextest --version 2>/dev/null || true)" in
		'cargo-nextest 0.9.103 ('*) ;;
		*) cargo install cargo-nextest --version 0.9.103 --locked ;;
	esac

# Default Rust gate. Cargo test was faster in the TER-239 contention comparison.
[group('ci')]
test: require-cargo
	#!/usr/bin/env bash
	set -euo pipefail
	if [ -n "${RESULTS_DIR:-}" ]; then
		mkdir -p "$RESULTS_DIR"
		: > "$RESULTS_DIR/rust.json"
	fi
	python3 scripts/rust_tests.py --workspace --no-fail-fast

# Run the terrarium-host test binaries of `ci::test` again as uid 1001 (the release CI user), not as root (TER-579). `ci::validate` runs it in merge jobs only
[group('ci')]
test-unprivileged: require-cargo
	python3 scripts/unprivileged_tests.py

# Opt-in parallel runner. RESULTS_DIR retains rust.xml and doctests.json.
[group('ci')]
test-nextest: require-cargo
	#!/usr/bin/env bash
	set -euo pipefail
	if [ -n "${RESULTS_DIR:-}" ]; then
		mkdir -p "$RESULTS_DIR"
		rm -f "$RESULTS_DIR"/rust*.xml "$RESULTS_DIR/doctests.json"
	fi
	python3 scripts/rust_tests.py --nextest --workspace --no-fail-fast

# Run the Python tests under scripts/ (harnesses.toml, the Dockerfile pins, the host installer, the deploy, the CI workflow)
[group('ci')]
scripts-test: require-uv
	uv run python -m unittest discover -s scripts -p 'test_*.py'

# Run the dashboard's `npm run check` (typecheck and lint) and its unit tests; skips when dashboard/ has no package.json. With RESULTS_DIR set, the tests also write JUnit XML to $RESULTS_DIR/dashboard.xml
[group('ci')]
dashboard-check:
	#!/usr/bin/env bash
	set -euo pipefail
	if [ ! -f dashboard/package.json ]; then
		echo "dashboard-check: skipped, dashboard/package.json does not exist"
		exit 0
	fi
	command -v npm > /dev/null 2>&1 || { echo "npm is required: install Node 22.19 or later" >&2; exit 1; }
	results=""
	if [ -n "${RESULTS_DIR:-}" ]; then
		mkdir -p "$RESULTS_DIR"
		results=$(realpath "$RESULTS_DIR")
	fi
	cd dashboard
	if [ ! -d node_modules ]; then
		if [ -f package-lock.json ]; then npm ci; else npm install; fi
	fi
	npm run check
	if [ -z "$results" ]; then
		npm run test -- --run
	else
		npm run test -- --run --reporter=default --reporter=junit --outputFile.junit="$results/dashboard.xml"
	fi

# Adapter compatibility suite against the pinned harness versions (docs/compat-suite.md). Not in `check`: it needs real logins and spends up to 3 model turns per harness. `harness`: all, claude, codex, copilot, pi, or a comma list
[group('ci')]
compat harness='all': require-cargo
	cargo build -p terrarium
	PATH="{{justfile_directory()}}/tools/bin:$PATH" TERRARIUM_COMPAT_HARNESSES={{quote(harness)}} cargo test -p terrarium-compat --test compat -- --ignored --exact real_harnesses --nocapture

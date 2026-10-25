set working-directory := '../..'

import '../shared.just'

# harnesses.toml is the one place for a harness version. The logic is in
# scripts/harness.py (norm 12); these recipes only call it.

[default, private]
main:
	@just --list harness

# Show each harness: the pin, the tested range, the version in the container and on the host, and the latest on npm; changes nothing (flag: --json)
[group('harness')]
check *flags: require-uv
	uv run python scripts/harness.py check {{flags}}

# Bump one pin (default: the latest on npm), widen the tested range for a patch or minor bump (a major needs --allow-major), build the image, run the dry compat (flags: --dry-run, --allow-major, --no-build)
[group('harness')]
update name *args: require-uv
	uv run python scripts/harness.py update {{quote(name)}} {{args}}

# Pull every harness and the extension with a newer npm release to the latest (majors too, with a warning), build the image once, run the dry compat, and roll out (flags: --dry-run, --no-rollout)
[group('harness')]
latest *flags: require-uv
	uv run python scripts/harness.py latest {{flags}}

# Roll the stack onto the new image with `just up`: the container stops the agents gracefully, and the daemon resumes them (flag: --dry-run)
[group('harness')]
rollout *flags: require-uv require-docker
	uv run python scripts/harness.py rollout {{flags}}

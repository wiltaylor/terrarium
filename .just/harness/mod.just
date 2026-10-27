set working-directory := '../..'

import '../shared.just'

# harnesses.toml holds the pins of the first install. The logic is in
# scripts/harness.py (norm 12); these recipes only call it.

[default, private]
main:
	@just --list harness

# Show each harness: the pin, the tested range, the version in the container (the tools manifest) and on the host, and the latest on npm; changes nothing (flag: --json)
[group('harness')]
check *flags: require-uv
	uv run python scripts/harness.py check {{flags}}

# Update one harness in the running container to a version (default: the latest on npm): npm installs it into a new version dir and the daemon switches; running agents keep their version until a restart; no image build (flags: --dry-run, --allow-major, --force, --pin)
[group('harness')]
update name *args: require-uv require-docker
	uv run python scripts/harness.py update {{quote(name)}} {{args}}

# Update every harness and the extension with a newer npm release in the running container, in one pass (majors too, with a warning); no image build (flags: --dry-run, --force, --pin)
[group('harness')]
latest *flags: require-uv require-docker
	uv run python scripts/harness.py latest {{flags}}

# Shared helpers. Import into each module that needs them.

[private]
require-docker:
	@command -v docker > /dev/null 2>&1 || (echo "docker is required: install Docker Engine with the compose plugin" >&2 && exit 1)
	@docker compose version > /dev/null 2>&1 || (echo "the docker compose plugin is required" >&2 && exit 1)

[private]
require-cargo:
	@command -v cargo > /dev/null 2>&1 || (echo "cargo is required: install Rust 1.88 or later with rustup" >&2 && exit 1)

[private]
require-node:
	@command -v node > /dev/null 2>&1 || (echo "node is required: install Node 22.19 or later" >&2 && exit 1)
	@command -v npm > /dev/null 2>&1 || (echo "npm is required: install Node 22.19 or later" >&2 && exit 1)

[private]
require-just:
	@command -v just > /dev/null 2>&1 || (echo "just is required" >&2 && exit 1)

[private]
require-uv:
	@command -v uv > /dev/null 2>&1 || (echo "uv is required: see https://docs.astral.sh/uv/" >&2 && exit 1)

[private]
require-ssh:
	@command -v ssh > /dev/null 2>&1 || (echo "ssh is required" >&2 && exit 1)
	@command -v scp > /dev/null 2>&1 || (echo "scp is required" >&2 && exit 1)

[private]
require-dashboard:
	@test -f dashboard/package.json || (echo "dashboard/package.json is missing: the dashboard app is not in this checkout" >&2 && exit 1)

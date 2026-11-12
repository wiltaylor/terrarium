set working-directory := '../..'

import '../shared.just'

image := env("TERRARIUM_IMAGE", "terrarium:dev")
uid := env("TERRARIUM_UID", "1000")
gid := env("TERRARIUM_GID", "1000")
# The git ref of the build for the binaries' version (scripts/build-ref.sh); compose reads it too.
export TERRARIUM_GIT_REF := env("TERRARIUM_GIT_REF", `scripts/build-ref.sh`)

[default, private]
main:
	@just --list docker

# Build the Terrarium image (daemon, CLI, dashboard, Node); the harnesses are not in it: the container installs them into ./tools
[group('image')]
image-build: require-docker
	docker build -f docker/Dockerfile --build-arg UID={{uid}} --build-arg GID={{gid}} --build-arg TERRARIUM_GIT_REF={{TERRARIUM_GIT_REF}} -t {{image}} .

# Tag the image and push it to a registry of your own, for example `just docker::image-push registry.lan/terrarium:0.1.0` (CI pushes no image: see `just deploy`)
[group('image')]
image-push tag: require-docker
	docker tag {{image}} {{tag}}
	docker push {{tag}}

# Start the stack in the background and wait until it is healthy
[group('stack')]
up: require-docker volumes-init env-init
	docker compose up -d --wait

# Stop and remove the stack; bind-mounted data stays
[group('stack')]
down: require-docker
	docker compose down

# Restart the running container
[group('stack')]
restart: require-docker
	docker compose restart

# Follow the container logs
[group('stack')]
logs: require-docker
	docker compose logs -f

# Open a shell in the container as the terrarium user
[group('stack')]
shell: require-docker
	docker compose exec -u terrarium terrarium bash

# Start the stack, kill terrariumd with SIGKILL, and check that it comes back and reconnects to the live runner
[group('stack')]
crash-test: up require-uv
	uv run docker/crash-test.py

# Create config/*, state/, tools/, repos/, work/, and the definition directories, owned by you
[group('config')]
volumes-init:
	mkdir -p config/claude config/codex config/copilot config/pi state/front tools config/terrarium projects agents targets subscriptions fleets repos work config/tls updater/run updater/state

# Open the bind-mounted home of one harness (claude, codex, copilot, pi) in $EDITOR
[group('config')]
config-edit harness: volumes-init
	#!/usr/bin/env bash
	set -euo pipefail
	case "{{harness}}" in
		claude|codex|copilot|pi) ;;
		*) echo "unknown harness '{{harness}}': use claude, codex, copilot, or pi" >&2; exit 1 ;;
	esac
	"${EDITOR:-vi}" "config/{{harness}}"

# Make .env from .env.example with a fresh token, when .env does not exist
[private]
env-init:
	#!/usr/bin/env bash
	set -euo pipefail
	if [ -f .env ]; then exit 0; fi
	token="$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')"
	sed "s/^TERRARIUM_TOKEN=.*/TERRARIUM_TOKEN=${token}/" .env.example > .env
	chmod 600 .env
	echo "created .env with a new TERRARIUM_TOKEN"

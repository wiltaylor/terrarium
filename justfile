set dotenv-load

export TERRARIUM_IMAGE := env("TERRARIUM_IMAGE", "terrarium:dev")
export TERRARIUM_PORT  := env("TERRARIUM_PORT", "8080")
export TERRARIUM_UID   := env("TERRARIUM_UID", `id -u`)
export TERRARIUM_GID   := env("TERRARIUM_GID", `id -g`)

[default, private]
main:
	@just --list

# Verification gate
mod ci '.just/ci'
# Image and compose lifecycle
mod docker '.just/docker'
# Daemon and CLI (cargo)
mod daemon '.just/daemon'
# SolidJS dashboard
mod dashboard '.just/dashboard'
# Harness versions: check, update, roll out (harnesses.toml)
mod harness '.just/harness'
# Host agent build and install
mod host '.just/host'
# Install, update, and roll back a server: build the image on the target (no registry)
mod deploy '.just/deploy'
# Packaged defaults: the prompts and harness settings of the config repo
mod defaults '.just/defaults'
# The release build: test, build, sign, and publish a release (docs/release.md)
mod release '.just/release'
# Phase 0 spike scripts
mod spikes '.just/spikes'

# Build everything: crates, dashboard, image
[group('build')]
build: daemon::build dashboard::build docker::image-build

# Test everything that runs without a container
[group('test')]
test: daemon::test dashboard::test

# Build the image and start the stack
[group('run')]
up: docker::image-build docker::up

# Stop the stack
[group('run')]
down: docker::down

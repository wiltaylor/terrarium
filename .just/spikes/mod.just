set working-directory := '../..'

import '../shared.just'

image := env("TERRARIUM_IMAGE", "terrarium:dev")

[default, private]
main:
	@just --list spikes

# Drive a harness login flow (claude, codex, copilot, pi) in a PTY and capture the output (not wired yet)
[group('spikes')]
login harness:
	@echo "spikes::login {{harness}} is not wired yet: add the script under spikes/ and call it here" >&2
	@exit 1

# Check that bwrap can make a sandbox in the image with the security options of compose.yaml
[group('spikes')]
bwrap-check: require-docker
	docker run --rm --security-opt seccomp=docker/seccomp-bwrap.json --security-opt apparmor=unconfined --security-opt systempaths=unconfined --entrypoint bwrap {{image}} --ro-bind / / --unshare-user --unshare-pid --unshare-ipc --unshare-uts --proc /proc --dev /dev --tmpfs /tmp -- /bin/sh -c 'echo "bwrap ok: uid=$(id -u) pid=$$"'

# Capture the hook payloads of one harness (not wired yet)
[group('spikes')]
hooks-capture harness:
	@echo "spikes::hooks-capture {{harness}} is not wired yet: add the script under spikes/ and call it here" >&2
	@exit 1

# Query the usage endpoint of one provider with the credentials in config/ (not wired yet)
[group('spikes')]
usage-probe provider:
	@echo "spikes::usage-probe {{provider}} is not wired yet: add the script under spikes/ and call it here" >&2
	@exit 1

# Regenerate docs/compat.md from the spike results (not wired yet)
[group('spikes')]
compat-matrix:
	@echo "spikes::compat-matrix is not wired yet: add the script under spikes/ and call it here" >&2
	@exit 1

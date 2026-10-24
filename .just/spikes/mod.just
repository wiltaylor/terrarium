set working-directory := '../..'

import '../shared.just'

image := env("TERRARIUM_IMAGE", "terrarium:dev")

[default, private]
main:
	@just --list spikes

# Drive a harness login flow in a PTY (not wired: Phase 0 has no login spike)
[group('spikes')]
login harness:
	@echo "spikes::login {{harness}} is not wired: Phase 0 has no login spike. See spikes/README.md" >&2
	@exit 1

# Check that bwrap can make a sandbox in the image with the security options of compose.yaml
[group('spikes')]
bwrap-check: require-docker
	docker run --rm --security-opt seccomp=docker/seccomp-bwrap.json --security-opt apparmor=unconfined --security-opt systempaths=unconfined --entrypoint bwrap {{image}} --ro-bind / / --unshare-user --unshare-pid --unshare-ipc --unshare-uts --proc /proc --dev /dev --tmpfs /tmp -- /bin/sh -c 'echo "bwrap ok: uid=$(id -u) pid=$$"'

# Capture the hook payloads of one turn in interactive mode (spike 1; only claude has it)
[group('spikes')]
hooks-capture harness idle_s='70': require-uv (require-spike harness)
	uv run spikes/{{harness}}/hooks-in-interactive/run.py {{idle_s}}

# Check which settings sources load hooks, and --bare (spike 1b)
[group('spikes')]
claude-sources-check: (spike-run "hooks-in-interactive/sources.py")

# Check --session-id, reuse, --resume, and the session file (spike 2, two turns)
[group('spikes')]
claude-session-check: (spike-run "session-id-and-resume/run.py")

# Check the start-up dialogs and the quiet env for each pre-seed variant (spike 3)
[group('spikes')]
claude-yolo-check *variants: (spike-run "yolo-and-trust/run.py" variants)

# Check yolo as root in Docker (spike 3b)
[group('spikes')]
claude-yolo-root-check: require-docker
	sh spikes/claude/yolo-and-trust/root-check.sh

# Check --system-prompt, --append-system-prompt, and the resume snapshot (spike 4; pass e for the extra run)
[group('spikes')]
claude-prompt-check *variant: (spike-run "system-prompt/run.py" variant)

# Check typing into the TUI: paste, CR, busy queue, AskUserQuestion (spike 5, seven turns)
[group('spikes')]
claude-typing-check: (spike-run "typing-into-tui/run.py")

# Run the screen detector over the captured screens, offline (spike 5b)
[group('spikes')]
claude-detector-check: (spike-run "typing-into-tui/detector_check.py")

# Check bwrap inside Docker with each seccomp profile (spike 6)
[group('spikes')]
claude-bwrap-check: require-docker (spike-run "bwrap-in-docker/run.py")

# Check claude auth status output for each login state (spike 7)
[group('spikes')]
claude-auth-check: (spike-run "auth-status/run.py")

# Query the usage endpoint of one provider (not wired: Phase 0 has no usage spike)
[group('spikes')]
usage-probe provider:
	@echo "spikes::usage-probe {{provider}} is not wired: Phase 0 has no usage-probe spike. See docs/compat.md" >&2
	@exit 1

# Regenerate docs/compat.md (not wired: compat.md is written by hand from the captures)
[group('spikes')]
compat-matrix:
	@echo "spikes::compat-matrix is not wired: docs/compat.md is written by hand from spikes/*/captured" >&2
	@exit 1

[private]
spike-run script *args: require-uv
	uv run spikes/claude/{{script}} {{args}}

[private]
require-spike harness:
	@test -d "spikes/{{harness}}" || (echo "no Phase 0 spikes for {{harness}}: only claude has them (spikes/README.md)" >&2 && exit 1)

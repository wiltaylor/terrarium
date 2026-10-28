set working-directory := '../..'

import '../shared.just'

# The repo is private, so no image goes to a registry: a deploy sends the
# source (`git archive`) and builds the image on the target. The logic is in
# scripts/deploy.py (norm 12); these recipes only call it. The target is
# user@host, as ssh takes it; `local` is this machine, with no ssh.

[default, private]
main:
	@just --list deploy

# Install or re-install on user@host over ssh: preflight, send the source, make .env once, build, swap, start, wait for health (flags: --dir, --ref, --public-url, --port, --dry-run, --yes, --allow-dirty, --ssh-option)
[group('deploy')]
server target *flags: require-git require-ssh require-uv
	uv run python scripts/deploy.py server {{quote(target)}} {{flags}}

# Install or re-install in another dir on this machine, with no ssh; refuses this repo dir (flags: as server, without --ssh-option)
[group('deploy')]
local *flags: require-git require-uv
	uv run python scripts/deploy.py server local {{flags}}

# Send a new source, build from src.new, then swap and `up -d`; a failed build leaves src/ and the stack as they were (flags: --dir, --ref, --dry-run, --allow-dirty, --ssh-option)
[group('deploy')]
update target *flags: require-git require-uv
	uv run python scripts/deploy.py update {{quote(target)}} {{flags}}

# Swap src.prev and src, and `up -d` with the image of the previous deploy; a second rollback goes forward again (flags: --dir, --dry-run, --ssh-option)
[group('deploy')]
rollback target *flags: require-uv
	uv run python scripts/deploy.py rollback {{quote(target)}} {{flags}}

# Show the deployed ref, compose ps, and the health check; changes nothing (flags: --dir, --ssh-option)
[group('deploy')]
status target *flags: require-uv
	uv run python scripts/deploy.py status {{quote(target)}} {{flags}}

# Test the deploy script with fakes only: no ssh, no docker
[group('deploy')]
test: require-uv
	uv run python -m unittest discover -s scripts -p 'test_deploy.py'

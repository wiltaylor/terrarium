set working-directory := '../..'

import '../shared.just'

[default, private]
main:
	@just --list release

# The release without a push, signatures or GitHub release: the version, the gate, the image as an OCI layout in release-out/, and the host binaries out of it with SHA256SUMS (scripts/release.py, docs/release.md). `just release::dry-run --no-gate` skips the gate
[group('release')]
dry-run *args: require-git
	python3 scripts/release.py dry-run {{args}}

# The release of HEAD in the release build container: the gate as a non-root user, then the image to GHCR, the cosign signatures and the GitHub release. Needs GHCR_TOKEN, GH_TOKEN, COSIGN_PRIVATE_KEY and COSIGN_PASSWORD in the env (docs/release.md)
[group('release')]
publish *args: require-git
	python3 scripts/release.py publish {{args}}

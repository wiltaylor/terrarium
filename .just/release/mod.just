set working-directory := '../..'

import '../shared.just'

[default, private]
main:
	@just --list release

# The release without a push, signatures or GitHub release: the version, the gate and the image on the release VM (TERRARIUM_DOCKER_HOST), the image as an OCI layout in release-out/, and the host binaries out of it with SHA256SUMS (scripts/release.py, docs/release.md). `just release::dry-run --no-gate` skips the gate
[group('release')]
dry-run *args: require-git
	python3 scripts/release.py dry-run {{args}}

# The release of HEAD on the release client: the gate and the image build on the release VM (TERRARIUM_DOCKER_HOST), then the push to GHCR, the cosign signatures with Rekor and the GitHub release from the client. Needs GHCR_TOKEN, GH_TOKEN, COSIGN_PRIVATE_KEY and COSIGN_PASSWORD in the env (docs/release.md)
[group('release')]
publish *args: require-git
	python3 scripts/release.py publish {{args}}

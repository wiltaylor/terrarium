# Releases

This document tells you how to make a Terrarium release and how to verify
what a release publishes. The workflow is `.github/workflows/ci.yml`. The
host agent self-update that uses the release binary is in
[host-protocol.md, "Self-update"](host-protocol.md#self-update).

## What a release publishes

| Item | Where |
|---|---|
| The image | `ghcr.io/wiltaylor/terrarium:<version>` |
| The channel tag | `:latest` for a stable release, `:pre` for a pre-release |
| The host agent | `terrarium-host-<version>-linux-x86_64` on the GitHub release |
| The checksums | `SHA256SUMS` on the GitHub release (the host agent) |
| The tag | `v<version>` on the commit |

The image has these parts:

- The runtime stage of `docker/Dockerfile`, for `linux/amd64`.
- The OCI labels `org.opencontainers.image.version`, `.revision` (the
  commit), and `.source` (the repo URL), and also `.title`,
  `.description`, and `.licenses`.
- A BuildKit SBOM and a BuildKit provenance attestation (`mode=max`) in
  the image index.
- A GitHub build provenance attestation for the image digest. It is a
  Sigstore bundle with a keyless certificate for the identity of the
  workflow (`actions/attest-build-provenance`). The bundle is pushed to
  the registry and is also stored on GitHub.
- Each binary reports `<version>+<commit>` (`TERRARIUM_GIT_REF` is the
  commit; [CONVENTIONS.md](CONVENTIONS.md), "Build version").

The host agent asset comes out of the pushed image
(`/opt/terrarium/dist/terrarium-host-x86_64-linux`). Thus the release
binary and the binary that a daemon of this image serves at
`GET /install/terrarium-host` have the same bytes. CI does these checks
before the upload:

1. The SHA-256 of the binary is the same as the `.sha256` file in the
   image. The daemon serves that file, and the self-update offer holds
   that hash.
2. `terrarium-host --version` reports the release version.

The binary also gets a GitHub build provenance attestation.

## How to make a release

A release occurs only for a push to `main` when the head commit has a
trailer:

| Trailer | Result |
|---|---|
| `release: true` | A stable release `X.Y.Z`. The image gets `:latest`. |
| `pre-release: true` | A pre-release `X.Y.Z-alpha`. The image gets `:pre`. |
| `bump: major\|minor\|patch` | Optional. It overrides the bump that CI finds in the conventional-commit subjects since the last `v*` tag. |

With no trailer, CI runs only the tests. Example:

```sh
git commit -m "feat(board): card templates" -m "release: true"
git push origin main
```

The jobs run in this sequence:

1. `check`: `just ci::check`, one part in each step.
2. `version`: it calculates the version from the last `v*` tag (0.x
   semantics: a breaking change bumps the minor).
3. `image`: it stamps the version into `Cargo.toml`, builds and pushes
   the image, attests it, takes the host agent out of it, writes
   `SHA256SUMS`, and attests the host agent.
4. `release`: it pushes the tag and makes the GitHub release with the
   host agent, `SHA256SUMS`, and notes that give the image digest.

The `image` job pushes the image before the `release` job makes the tag.
If `release` fails, the image exists with no release. Run the `release`
job again (re-run failed jobs), or delete the package version.

### Permissions

The workflow gives `contents: read` to each job by default. Only these
jobs get more:

| Job | Permissions | For |
|---|---|---|
| `image` | `packages: write` | the push to `ghcr.io` |
| | `id-token: write` | the Sigstore certificate of the attestations |
| | `attestations: write` | the storage of the attestations on GitHub |
| `release` | `contents: write` | the tag and the release |

Each action is pinned to a full commit SHA. A comment gives the version.
To update an action, find the commit of the new tag
(`gh api repos/<owner>/<repo>/commits/<tag> --jq .sha`). Then change the
SHA and the comment together.

### Make the package public (one time)

A new GHCR package is private, and the REST API cannot change the
visibility of a package. After the first release, do these steps one
time:

1. Open `https://github.com/users/wiltaylor/packages/container/terrarium/settings`.
2. In **Danger Zone**, select **Change visibility**, then **Public**.
3. Make sure that **Manage Actions access** gives the repository
   `wiltaylor/terrarium` the role **Write**. The
   `org.opencontainers.image.source` label links the package to the
   repository, and the first push from the workflow usually sets this
   role.

To check the visibility, pull the image with no login:
`docker logout ghcr.io && docker pull ghcr.io/wiltaylor/terrarium:latest`.

## How to verify a release

You need the GitHub CLI (`gh`) 2.49 or later. `gh attestation verify`
checks the Sigstore bundle. It also checks that the certificate identity
is a workflow of the repository.

**The image:**

```sh
gh attestation verify oci://ghcr.io/wiltaylor/terrarium:0.3.0 \
  --repo wiltaylor/terrarium \
  --signer-workflow wiltaylor/terrarium/.github/workflows/ci.yml
```

The command prints the digest, the workflow, and the commit. Use the
digest from the release notes to pull the exact image:
`docker pull ghcr.io/wiltaylor/terrarium@sha256:<digest>`.

To read the labels and the BuildKit SBOM:

```sh
docker buildx imagetools inspect ghcr.io/wiltaylor/terrarium:0.3.0 --format '{{json .Image.Config.Labels}}'
docker buildx imagetools inspect ghcr.io/wiltaylor/terrarium:0.3.0 --format '{{json .SBOM}}'
```

**The host agent:**

```sh
gh release download v0.3.0 --repo wiltaylor/terrarium
sha256sum -c SHA256SUMS
gh attestation verify terrarium-host-0.3.0-linux-x86_64 \
  --repo wiltaylor/terrarium \
  --signer-workflow wiltaylor/terrarium/.github/workflows/ci.yml
```

A host agent that updates itself checks the SHA-256 that the daemon
sends. The daemon reads that hash from the image. When the dist dir of
the daemon has the `SHA256SUMS` of the release, the host agent also
checks the release hash ([host-protocol.md, "Self-update"](host-protocol.md#self-update)).
The host agent does not verify the attestation itself: the image that
holds the binary is the item to verify.

## Not in this document

The updater that pulls a new image and goes back to the old image when
the update fails is [issue #19](https://github.com/wiltaylor/terrarium/issues/19),
part B. A development deploy from the source (`just deploy::*`) does not
use the release image.

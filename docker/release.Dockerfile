# syntax=docker/dockerfile:1.7
#
# The release client (TER-613, docs/release.md): the tools of the target where
# the release job runs `just release::publish`, as the non-root user `release`.
# META-119 makes the client a small VM; this file is the pinned list of its
# tools, and a container image of it for a test. The client runs no code of the
# release: the gate and the image build run on the release VM's Docker daemon
# (TERRARIUM_DOCKER_HOST, the daemon's relay). The client only pushes the built
# bytes (crane), signs them (cosign, with Rekor), and creates the GitHub release
# (gh). With:
#
#   git, python3 (3.11 or later), curl, jq     Debian 13 packages
#   the docker CLI 29.0.3 with buildx          no daemon on the client
#   crane 0.22.1, cosign 3.1.4, gh 2.100.0     downloads checked by SHA-256
#
# cosign has the version of the cosign stage of docker/Dockerfile (the updater
# verifies with it; scripts/test_release.py checks it). The secrets reach only
# the env of the release process; the registry login and the state of cosign
# and gh live in a RAM directory (/dev/shm or XDG_RUNTIME_DIR, on tmpfs).

ARG DEBIAN_IMAGE=debian:trixie-slim@sha256:a72b4433de1cf7d6bae0c4cee74d0547f58601a643b3375f070c51600d63b6b2
ARG DOCKER_CLI_IMAGE=docker:29.0.3-cli@sha256:200642d6d6e6dbdc0f240d34f0636db56bbb1cc28a39195570f13eaf1c80134c

FROM ${DOCKER_CLI_IMAGE} AS docker-cli

# ---------------------------------------------------------------------------
# The downloads, each checked against its SHA-256 (amd64 only).
FROM ${DEBIAN_IMAGE} AS tools
ARG CRANE_VERSION=0.22.1
ARG CRANE_SHA256=0ab7a1d6932a213aed964ce97666c3077fe691c8606413674a8b3e0b9ec4cda0
ARG COSIGN_VERSION=3.1.4
ARG COSIGN_SHA256=424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa
ARG GH_VERSION=2.100.0
ARG GH_SHA256=e4d4bb4498e8d007abe545b6568926793ace1b6447da598294a610018cb164be
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/*
RUN set -eu; \
    mkdir -p /out /tmp/dl; cd /tmp/dl; \
    fetch() { curl -fsSL -o "$1" "$2"; echo "$3  $1" | sha256sum -c -; }; \
    fetch crane.tgz "https://github.com/google/go-containerregistry/releases/download/v${CRANE_VERSION}/go-containerregistry_Linux_x86_64.tar.gz" "$CRANE_SHA256"; \
    tar -xzf crane.tgz crane && mv crane /out/crane; \
    fetch /out/cosign "https://github.com/sigstore/cosign/releases/download/v${COSIGN_VERSION}/cosign-linux-amd64" "$COSIGN_SHA256"; \
    fetch gh.tgz "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_amd64.tar.gz" "$GH_SHA256"; \
    tar -xzf gh.tgz && mv "gh_${GH_VERSION}_linux_amd64/bin/gh" /out/gh; \
    chmod 0755 /out/*

# ---------------------------------------------------------------------------
FROM ${DEBIAN_IMAGE}
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates curl git jq python3 \
    && rm -rf /var/lib/apt/lists/*
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=docker-cli /usr/local/libexec/docker/cli-plugins/docker-buildx /usr/local/libexec/docker/cli-plugins/docker-buildx
COPY --from=tools /out/ /usr/local/bin/
RUN groupadd -g 1001 release && useradd -m -u 1001 -g 1001 -s /bin/bash release
USER release
WORKDIR /home/release

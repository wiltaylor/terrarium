# syntax=docker/dockerfile:1.7
#
# The release client container (TER-613, docs/release.md): the image of the
# target where the release job runs `just release::publish`, as uid 1001
# (the user of the old GitHub release CI), with:
#
#   the toolchain of the gate   rust 1.97.1 with rustfmt and clippy, just, uv, node 22, bubblewrap, git
#   the image build             the docker CLI with buildx: the build runs on the release VM's
#                               Docker daemon (TERRARIUM_DOCKER_HOST, the daemon's relay)
#   the publish                 crane (push the built layout), cosign, gh
#
# Every tool is pinned: the base images by digest, the downloads by SHA-256.
# cosign has the version of the cosign stage of docker/Dockerfile (the
# updater verifies with it).
#
# Build: docker build -f docker/release.Dockerfile -t terrarium-release:<date> docker/
# (the context needs no file). The container needs no privilege, no Docker
# socket of its host, and no secret: the release job passes the secrets
# into the env of its process only, and this target runs no other job.

ARG RUST_IMAGE=rust:1.97.1-trixie@sha256:b1b3c9c0d921d7fa0a6d1f9ec7e4eab87f8c8ec97644c3d791450f131dec813f
# harnesses.toml [node] image.
ARG NODE_IMAGE=node:22.23.4-trixie-slim@sha256:be1e2a3dc2c4fd68cdfadad2a696a280e0d2749098bee509230174d72fb768c6
ARG DOCKER_CLI_IMAGE=docker:29.0.3-cli@sha256:200642d6d6e6dbdc0f240d34f0636db56bbb1cc28a39195570f13eaf1c80134c
ARG UV_IMAGE=ghcr.io/astral-sh/uv:0.13.0

FROM ${DOCKER_CLI_IMAGE} AS docker-cli
FROM ${NODE_IMAGE} AS node
FROM ${UV_IMAGE} AS uv

# ---------------------------------------------------------------------------
# The downloads, each checked against its SHA-256 (amd64 only).
FROM ${RUST_IMAGE} AS tools
ARG JUST_VERSION=1.59.0
ARG JUST_SHA256=409044617b6f06517c1035e55ca38c8c856ff8b650e41bc2aa1ef82d5903a23e
ARG CRANE_VERSION=0.22.1
ARG CRANE_SHA256=0ab7a1d6932a213aed964ce97666c3077fe691c8606413674a8b3e0b9ec4cda0
ARG COSIGN_VERSION=3.1.4
ARG COSIGN_SHA256=424e2c97d0b45fd7a6b608a87f090f35633f4c23ece5f092d48bba11aaf4bbfa
ARG GH_VERSION=2.100.0
ARG GH_SHA256=e4d4bb4498e8d007abe545b6568926793ace1b6447da598294a610018cb164be
RUN set -eu; \
    mkdir -p /out /tmp/dl; cd /tmp/dl; \
    fetch() { curl -fsSL -o "$1" "$2"; echo "$3  $1" | sha256sum -c -; }; \
    fetch just.tgz "https://github.com/casey/just/releases/download/${JUST_VERSION}/just-${JUST_VERSION}-x86_64-unknown-linux-musl.tar.gz" "$JUST_SHA256"; \
    tar -xzf just.tgz just && mv just /out/just; \
    fetch crane.tgz "https://github.com/google/go-containerregistry/releases/download/v${CRANE_VERSION}/go-containerregistry_Linux_x86_64.tar.gz" "$CRANE_SHA256"; \
    tar -xzf crane.tgz crane && mv crane /out/crane; \
    fetch /out/cosign "https://github.com/sigstore/cosign/releases/download/v${COSIGN_VERSION}/cosign-linux-amd64" "$COSIGN_SHA256"; \
    fetch gh.tgz "https://github.com/cli/cli/releases/download/v${GH_VERSION}/gh_${GH_VERSION}_linux_amd64.tar.gz" "$GH_SHA256"; \
    tar -xzf gh.tgz && mv "gh_${GH_VERSION}_linux_amd64/bin/gh" /out/gh; \
    chmod 0755 /out/*

# ---------------------------------------------------------------------------
FROM ${RUST_IMAGE}
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bubblewrap ca-certificates git python3 util-linux \
    && rm -rf /var/lib/apt/lists/* \
    && rustup component add rustfmt clippy --toolchain 1.97.1
COPY --from=node /usr/local/bin/node /usr/local/bin/node
COPY --from=node /usr/local/lib/node_modules /usr/local/lib/node_modules
RUN ln -s ../lib/node_modules/npm/bin/npm-cli.js /usr/local/bin/npm \
    && ln -s ../lib/node_modules/npm/bin/npx-cli.js /usr/local/bin/npx
COPY --from=uv /uv /uvx /usr/local/bin/
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=docker-cli /usr/local/libexec/docker/cli-plugins/docker-buildx /usr/local/libexec/docker/cli-plugins/docker-buildx
COPY --from=tools /out/ /usr/local/bin/
RUN groupadd -g 1001 release \
    && useradd -m -u 1001 -g 1001 -s /bin/bash release \
    && mkdir -p /src && chown release:release /src
USER 1001
WORKDIR /home/release
# The toolchain of rustup in the base image is read-only for 1001; the
# crates registry goes to the user's own CARGO_HOME.
ENV CARGO_HOME=/home/release/.cargo \
    PATH=/home/release/.cargo/bin:/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin \
    RUSTUP_TOOLCHAIN=1.97.1

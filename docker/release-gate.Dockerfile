# syntax=docker/dockerfile:1.7
#
# The release gate image (TER-613, docs/release.md): `just release::publish`
# builds it on the release VM's Docker daemon (TERRARIUM_DOCKER_HOST) and
# runs the gate in it: `just ci::validate` as the user `release` (uid 1001,
# the user of the old GitHub release CI) on a clone of the commit in /src,
# with RESULTS_DIR=/results. The container gets no secret. With:
#
#   rust 1.97.1 with rustfmt and clippy, just, uv, node 22, bubblewrap, git, python3
#
# Every tool is pinned: the base images by digest, the downloads by SHA-256.
# The release script builds it with `docker buildx build --load -f
# docker/release-gate.Dockerfile docker` (the context needs no file).

ARG RUST_IMAGE=rust:1.97.1-trixie@sha256:b1b3c9c0d921d7fa0a6d1f9ec7e4eab87f8c8ec97644c3d791450f131dec813f
# harnesses.toml [node] image.
ARG NODE_IMAGE=node:22.23.4-trixie-slim@sha256:be1e2a3dc2c4fd68cdfadad2a696a280e0d2749098bee509230174d72fb768c6
ARG UV_IMAGE=ghcr.io/astral-sh/uv:0.13.0@sha256:cdc6093146eb3ff6a40107b38f008b789e050e77ad87865e381d9917da55a168

FROM ${NODE_IMAGE} AS node
FROM ${UV_IMAGE} AS uv

# ---------------------------------------------------------------------------
# The downloads, each checked against its SHA-256 (amd64 only).
FROM ${RUST_IMAGE} AS tools
ARG JUST_VERSION=1.59.0
ARG JUST_SHA256=409044617b6f06517c1035e55ca38c8c856ff8b650e41bc2aa1ef82d5903a23e
RUN set -eu; \
    mkdir -p /out /tmp/dl; cd /tmp/dl; \
    fetch() { curl -fsSL -o "$1" "$2"; echo "$3  $1" | sha256sum -c -; }; \
    fetch just.tgz "https://github.com/casey/just/releases/download/${JUST_VERSION}/just-${JUST_VERSION}-x86_64-unknown-linux-musl.tar.gz" "$JUST_SHA256"; \
    tar -xzf just.tgz just && mv just /out/just; \
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
COPY --from=tools /out/ /usr/local/bin/
RUN groupadd -g 1001 release \
    && useradd -m -u 1001 -g 1001 -s /bin/bash release \
    && mkdir -p /src /results && chown release:release /src /results
USER 1001
WORKDIR /src
# The toolchain of rustup in the base image (/usr/local/rustup) is readable
# by 1001; the crates registry goes to the user's own CARGO_HOME.
ENV HOME=/home/release \
    CARGO_HOME=/home/release/.cargo \
    PATH=/home/release/.cargo/bin:/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin \
    RUSTUP_TOOLCHAIN=1.97.1 \
    RESULTS_DIR=/results

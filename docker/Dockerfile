# syntax=docker/dockerfile:1.7
#
# Terrarium: one image with the daemon, the CLI, the embedded dashboard, and
# Node (PLAN §23.3). The image installs no harness: the entrypoint installs
# the pins of harnesses.toml into the tools volume (/opt/terrarium/tools) at
# the first start, and `POST /api/harnesses/{name}/update` updates them in
# place (docs/getting-started.md §7). Build from the repo root:
#
#   docker build -f docker/Dockerfile --build-arg UID=$(id -u) --build-arg GID=$(id -g) -t terrarium:dev .
#
# The Node image comes from harnesses.toml [node] image. The NODE_IMAGE
# default below is a copy; `just ci::check` fails when it differs
# (scripts/harness-versions.py check). The image copies harnesses.toml to
# /etc/terrarium/harnesses.toml: `terrariumd harness install` reads the pins
# there (docs/CONVENTIONS.md).
#
# Stages:
#   dashboard    npm ci && npm run build for the SolidJS app
#   rust         cargo build --release for terrariumd, terrarium, terrarium-runner, and terrarium-updater
#                (terrariumd embeds the dashboard)
#   host-build   static musl terrarium-host and terrarium-host-helper, for
#                `just host::build` and for the runtime's /opt/terrarium/dist
#                (GET /install/terrarium-host; the helper is a release asset)
#   host-export  scratch stage that holds only the host binary, for `--output`
#   gh           the GitHub CLI, pinned with its SHA-256, for the updater's image check
#   runtime      Debian slim + Node and npm, bubblewrap, git, ssh, python3, ripgrep, uv (no harness),
#                the static terrarium-host with its SHA-256 for the host installer and
#                the host agent self-update, and the updater with the gh CLI (the
#                entrypoint of the terrarium-updater service, docs/api.md §34). The
#                release image of CI (ghcr.io/wiltaylor/terrarium, docs/release.md): CI
#                takes the release's terrarium-host and terrarium-host-helper assets
#                out of this stage.

ARG RUST_IMAGE=rust:1.97-trixie
# The git ref of the build (crates/terrarium-version): `<commit>` or
# `<commit>-dirty`. The build context has no .git, so the caller passes it:
# `just docker::image-build` and `just host::build` (scripts/build-ref.sh),
# the deploy wrapper (src/.deployed-ref), and CI. Empty: `+unknown`.
ARG TERRARIUM_GIT_REF=
# harnesses.toml [node] image.
ARG NODE_IMAGE=node:22.23.4-trixie-slim

# ---------------------------------------------------------------------------
FROM ${NODE_IMAGE} AS dashboard
WORKDIR /src/dashboard
COPY dashboard/package.json dashboard/package-lock.json ./
RUN --mount=type=cache,target=/root/.npm npm ci --no-audit --no-fund
COPY dashboard/ ./
RUN npm run build && test -d dist

# ---------------------------------------------------------------------------
FROM ${RUST_IMAGE} AS rust
ARG TERRARIUM_GIT_REF
WORKDIR /src
COPY Cargo.toml Cargo.lock harnesses.toml ./
COPY crates ./crates
# terrariumd embeds the host installer (GET /install/host.sh).
COPY scripts/install-host.sh ./scripts/install-host.sh
COPY --from=dashboard /src/dashboard/dist ./dashboard/dist
# The git dependency wcl_lang (Cargo.toml) fetches from GitHub at the
# commit that Cargo.lock pins (`--locked`); its checkout is cached as the
# registry is.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,id=terrarium-target \
    cargo build --release --locked -p terrariumd -p terrarium -p terrarium-runner -p terrarium-updater \
    && mkdir -p /out \
    && cp target/release/terrariumd target/release/terrarium target/release/terrarium-runner \
        target/release/terrarium-updater /out/

# ---------------------------------------------------------------------------
FROM ${RUST_IMAGE} AS host-build
ARG TERRARIUM_GIT_REF
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add "$(uname -m)-unknown-linux-musl"
WORKDIR /src
COPY Cargo.toml Cargo.lock harnesses.toml ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/src/target,id=terrarium-host-target \
    target="$(uname -m)-unknown-linux-musl" \
    && cargo build --release --locked --target "$target" -p terrarium-host \
        --bin terrarium-host --bin terrarium-host-helper \
    && mkdir -p /out /dist \
    && cp "target/$target/release/terrarium-host" "target/$target/release/terrarium-host-helper" /out/ \
    && for bin in terrarium-host terrarium-host-helper; do \
        dist="$bin-$(uname -m)-linux" \
        && cp "/out/$bin" "/dist/$dist" \
        && (cd /dist && sha256sum "$dist" > "$dist.sha256") || exit 1; \
    done

FROM scratch AS host-export
COPY --from=host-build /out/terrarium-host /terrarium-host

# ---------------------------------------------------------------------------
# The GitHub CLI for the updater's image check (`gh attestation verify`,
# docs/release.md). Pinned with the SHA-256 of the release archive of each
# arch (cli/cli gh_<version>_checksums.txt); another arch stops the build.
FROM ${RUST_IMAGE} AS gh
ARG TARGETARCH
ARG GH_VERSION=2.100.0
ARG GH_SHA256_AMD64=e4d4bb4498e8d007abe545b6568926793ace1b6447da598294a610018cb164be
ARG GH_SHA256_ARM64=ea4e7a581a32ccad6cc7923cb1576ac5859ba4b9a16ab22eb8f8a96e78e2e961
RUN set -eu; \
    arch="${TARGETARCH:-$(dpkg --print-architecture)}"; \
    case "$arch" in \
        amd64) sum="$GH_SHA256_AMD64" ;; \
        arm64) sum="$GH_SHA256_ARM64" ;; \
        *) echo "no gh checksum for $arch" >&2; exit 1 ;; \
    esac; \
    file="gh_${GH_VERSION}_linux_${arch}.tar.gz"; \
    curl -fsSL -o "/tmp/$file" "https://github.com/cli/cli/releases/download/v${GH_VERSION}/$file"; \
    echo "$sum  /tmp/$file" | sha256sum -c -; \
    mkdir -p /out; \
    tar -xzf "/tmp/$file" -C /tmp; \
    cp "/tmp/gh_${GH_VERSION}_linux_${arch}/bin/gh" /out/gh

# ---------------------------------------------------------------------------
FROM docker:29.0.3-cli AS docker-cli

# Every file that changes with a release lives in one final runtime layer.
# The updater compares all preceding RootFS layer digests, not a mutable tag
# or a caller-supplied "base" label. Changing this layout bumps hot-swap-layout.
FROM ${NODE_IMAGE} AS release-payload
COPY --from=rust /out/ /payload/bin/
COPY --from=host-build /dist/ /payload/dist/
COPY harnesses.toml /payload/harnesses.toml
COPY defaults /payload/defaults/
RUN cd /payload && find . -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum | cut -d' ' -f1 > release-id

FROM ${NODE_IMAGE} AS runtime

# Compose resolves the deployment environment; swaps still use the Engine API.
COPY --from=docker-cli /usr/local/bin/docker /usr/local/bin/docker
COPY --from=docker-cli /usr/local/libexec/docker/cli-plugins/docker-compose /usr/local/libexec/docker/cli-plugins/docker-compose

ARG UID=1000
ARG GID=1000

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bubblewrap \
        ca-certificates \
        git \
        openssh-client \
        rsync \
        procps \
        python3 \
        ripgrep \
        tini \
    && rm -rf /var/lib/apt/lists/*

# uv and uvx for Python in the sandboxes (bwrap binds /usr read-only, so
# /usr/local/bin is on every agent's PATH). Pinned: the static binaries
# of Astral's image.
COPY --from=ghcr.io/astral-sh/uv:0.13.0 /uv /uvx /usr/local/bin/

# The node image ships a `node` user with UID 1000. Replace it with
# `terrarium`, whose UID and GID match the owner of the bind mounts.
RUN userdel --remove node 2>/dev/null || true \
    && (getent group node > /dev/null && groupdel node || true) \
    && (getent group "${GID}" > /dev/null || groupadd --gid "${GID}" terrarium) \
    && useradd --uid "${UID}" --gid "${GID}" --create-home --home-dir /home/terrarium \
        --shell /bin/bash terrarium

# Harness homes live on the /auth bind mounts. The symlinks serve harnesses
# that read $HOME; the env vars serve harnesses that read them.
RUN mkdir -p /auth/claude /auth/codex /auth/copilot /auth/pi \
        /state /work /run/terrarium /config/terrarium /opt/terrarium/tools \
        /etc/terrarium/projects /etc/terrarium/agents \
        /etc/terrarium/targets /etc/terrarium/subscriptions \
        /home/terrarium/.pi \
    && ln -s /auth/claude /home/terrarium/.claude \
    && ln -s /auth/codex /home/terrarium/.codex \
    && ln -s /auth/copilot /home/terrarium/.copilot \
    && ln -s /auth/pi /home/terrarium/.pi/agent \
    && chown -R "${UID}:${GID}" /auth /state /work /run/terrarium /etc/terrarium /home/terrarium \
        /opt/terrarium/tools /config

# The harnesses live in the tools volume, not in the image. npm installs
# into it (NPM_CONFIG_PREFIX), and its bin/ comes first on PATH for the
# daemon, the runners, and `docker compose exec`. The sandbox binds it
# read-only (crates/terrariumd/src/sandbox.rs).
ENV TERRARIUM_TOOLS_DIR=/opt/terrarium/tools \
    NPM_CONFIG_PREFIX=/opt/terrarium/tools \
    NPM_CONFIG_UPDATE_NOTIFIER=false \
    NPM_CONFIG_FUND=false \
    NPM_CONFIG_AUDIT=false \
    PATH=/opt/terrarium/tools/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

ENV HOME=/home/terrarium \
    CLAUDE_CONFIG_DIR=/auth/claude \
    CODEX_HOME=/auth/codex \
    COPILOT_HOME=/auth/copilot \
    PI_CODING_AGENT_DIR=/auth/pi \
    TERRARIUM_STATE_DIR=/state \
    TERRARIUM_CONFIG_DIR=/etc/terrarium \
    TERRARIUM_API_SOCKET=/run/terrarium/api.sock \
    TERRARIUM_LISTEN=0.0.0.0:8080 \
    TERRARIUM_RUNNER_MODE=process \
    TERRARIUM_RUNNER=/usr/local/bin/terrarium-runner \
    TERRARIUM_CONFIG_REPO=/config/terrarium \
    TERRARIUM_DEFAULTS_DIR=/opt/terrarium/defaults \
    TERRARIUM_HOST_DIST_DIR=/state/releases/current/dist

# One terrarium-runner process holds each agent (PLAN §6), so an agent
# lives through a restart of terrariumd.
RUN ln -s /state/releases/current/bin/terrariumd /usr/local/bin/terrariumd \
    && ln -s /state/releases/current/bin/terrarium /usr/local/bin/terrarium \
    && ln -s /state/releases/current/bin/terrarium-runner /usr/local/bin/terrarium-runner \
    && ln -s /opt/terrarium/bundled/bin/terrarium-updater /usr/local/bin/terrarium-updater \
    && ln -s /opt/terrarium/bundled/dist /opt/terrarium/dist \
    && ln -s /state/releases/current/defaults /opt/terrarium/defaults \
    && ln -s /state/releases/current/harnesses.toml /etc/terrarium/harnesses.toml
# The updater (docs/api.md §34): the same image with the entrypoint
# terrarium-updater (compose.yaml, profile `updater`). `gh attestation
# verify` checks the build provenance of a release image before the
# updater pulls it (TERRARIUM_UPDATE_VERIFY=gh, the default; the gh stage).
COPY --from=gh /out/gh /usr/local/bin/gh
# The host installer serves these (GET /install/terrarium-host?arch=<arch>,
# TERRARIUM_HOST_DIST_DIR). One arch for now: the arch of the build.
COPY docker/entrypoint.sh /usr/local/bin/terrarium-entrypoint
COPY docker/hot-swap.py /usr/local/lib/terrarium-hot-swap.py
# The pins of the first install (`terrariumd harness install`).
# The packaged defaults (PLAN §8.3): the prompts and the harness settings.
# The daemon copies them into the config repo (/config/terrarium) at each
# start: a missing file is written, an unedited one is upgraded, and an
# edited one gets the new default as <file>.new.

USER terrarium
WORKDIR /home/terrarium
# 8080 dashboard and API, 8081 artifact origin (§15.2), 8082 host agent dial-in (§19),
# 8083 webhooks of the event sources (docs/api.md §20).
EXPOSE 8080 8081 8082 8083
# tini is PID 1: it reaps the runners that terrariumd leaves when it dies.
# No `-g`: tini sends SIGTERM to the entrypoint only, and the entrypoint
# stops terrariumd and then the runners in order (docker/README.md).
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/terrarium-entrypoint"]
CMD ["--daemon-service", "terrariumd"]

# Must remain the LAST filesystem layer. Everything before it is the runtime
# base and must be byte-identical for a hot swap. Metadata below adds no layer.
COPY --from=release-payload /payload/ /opt/terrarium/bundled/
LABEL io.terrarium.hot-swap-layout="1" \
      io.terrarium.runner-compat-epoch="1"

# OCI labels (docs/release.md). Last, so a new ref or version does not
# rebuild a layer. CI sets the same version, revision, and source labels.
ARG TERRARIUM_GIT_REF
# The release version (CI); empty for a local build.
ARG TERRARIUM_VERSION=
LABEL org.opencontainers.image.title="terrarium" \
      org.opencontainers.image.description="Terrarium: the daemon, the CLI, the dashboard, and the host agent binary for /install" \
      org.opencontainers.image.source="https://github.com/wiltaylor/terrarium" \
      org.opencontainers.image.licenses="MIT" \
      org.opencontainers.image.revision="${TERRARIUM_GIT_REF}" \
      org.opencontainers.image.version="${TERRARIUM_VERSION}"

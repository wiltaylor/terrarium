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
#   rust         cargo build --release for terrariumd, terrarium, and terrarium-runner
#                (terrariumd embeds the dashboard)
#   host-build   static musl terrarium-host, for `just host::build` (not in the image)
#   host-export  scratch stage that holds only the host binary, for `--output`
#   runtime      Debian slim + Node and npm, bubblewrap, git, ssh, python3 (no harness)

ARG RUST_IMAGE=rust:1.97-trixie
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
WORKDIR /src
COPY Cargo.toml Cargo.lock harnesses.toml ./
COPY crates ./crates
COPY --from=dashboard /src/dashboard/dist ./dashboard/dist
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=terrarium-target \
    cargo build --release --locked -p terrariumd -p terrarium -p terrarium-runner \
    && mkdir -p /out \
    && cp target/release/terrariumd target/release/terrarium target/release/terrarium-runner /out/

# ---------------------------------------------------------------------------
FROM ${RUST_IMAGE} AS host-build
RUN apt-get update \
    && apt-get install -y --no-install-recommends musl-tools \
    && rm -rf /var/lib/apt/lists/* \
    && rustup target add "$(uname -m)-unknown-linux-musl"
WORKDIR /src
COPY Cargo.toml Cargo.lock harnesses.toml ./
COPY crates ./crates
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target,id=terrarium-host-target \
    target="$(uname -m)-unknown-linux-musl" \
    && cargo build --release --locked --target "$target" -p terrarium-host \
    && mkdir -p /out \
    && cp "target/$target/release/terrarium-host" /out/

FROM scratch AS host-export
COPY --from=host-build /out/terrarium-host /terrarium-host

# ---------------------------------------------------------------------------
FROM ${NODE_IMAGE} AS runtime

ARG UID=1000
ARG GID=1000

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        bubblewrap \
        ca-certificates \
        git \
        openssh-client \
        procps \
        python3 \
        tini \
    && rm -rf /var/lib/apt/lists/*

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
        /state /work /run/terrarium /prompts /opt/terrarium/tools \
        /etc/terrarium/projects /etc/terrarium/agents \
        /etc/terrarium/targets /etc/terrarium/subscriptions \
        /home/terrarium/.pi \
    && ln -s /auth/claude /home/terrarium/.claude \
    && ln -s /auth/codex /home/terrarium/.codex \
    && ln -s /auth/copilot /home/terrarium/.copilot \
    && ln -s /auth/pi /home/terrarium/.pi/agent \
    && chown -R "${UID}:${GID}" /auth /state /work /run/terrarium /etc/terrarium /home/terrarium \
        /opt/terrarium/tools

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
    TERRARIUM_RUNNER=/usr/local/bin/terrarium-runner

# One terrarium-runner process holds each agent (PLAN §6), so an agent
# lives through a restart of terrariumd.
COPY --from=rust /out/terrariumd /out/terrarium /out/terrarium-runner /usr/local/bin/
COPY docker/entrypoint.sh /usr/local/bin/terrarium-entrypoint
# The pins of the first install (`terrariumd harness install`).
COPY harnesses.toml /etc/terrarium/harnesses.toml

USER terrarium
WORKDIR /home/terrarium
# 8080 dashboard and API, 8081 artifact origin (§15.2), 8082 host agent dial-in (§19),
# 8083 webhooks of the event sources (docs/api.md §20).
EXPOSE 8080 8081 8082 8083
# tini is PID 1: it reaps the runners that terrariumd leaves when it dies.
# No `-g`: tini sends SIGTERM to the entrypoint only, and the entrypoint
# stops terrariumd and then the runners in order (docker/README.md).
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/terrarium-entrypoint"]
CMD ["terrariumd"]

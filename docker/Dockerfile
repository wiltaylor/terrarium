# syntax=docker/dockerfile:1.7
#
# Terrarium: one image with the daemon, the CLI, the embedded dashboard, and
# the pinned harnesses (PLAN §23.3). Build from the repo root:
#
#   docker build -f docker/Dockerfile --build-arg UID=$(id -u) --build-arg GID=$(id -g) -t terrarium:dev .
#
# The harness versions and the Node image come from harnesses.toml at the
# repo root. `just docker::image-build` passes them as build args
# (scripts/harness-versions.py build-args). The ARG defaults below are a
# copy for a build by hand; `just harness::update` rewrites them, and
# `just ci::check` fails when they differ from harnesses.toml. Do not edit
# a version here: edit harnesses.toml (docs/CONVENTIONS.md).
#
# Stages:
#   dashboard    npm ci && npm run build for the SolidJS app
#   rust         cargo build --release for terrariumd, terrarium, and terrarium-runner
#                (terrariumd embeds the dashboard)
#   host-build   static musl terrarium-host, for `just host::build` (not in the image)
#   host-export  scratch stage that holds only the host binary, for `--output`
#   runtime      Debian slim + Node, bubblewrap, git, ssh, python3, and the harnesses

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
# Pinned harness versions: harnesses.toml [harness.<name>] version, and
# [extension.copilot-prompt] version. The adapters read the same file.
ARG CLAUDE_VERSION=2.1.310
ARG CODEX_VERSION=0.160.0
ARG COPILOT_VERSION=1.0.103
ARG PI_VERSION=0.85.1
ARG COPILOT_PROMPT_VERSION=0.1.2

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

RUN --mount=type=cache,target=/root/.npm \
    npm install -g --no-audit --no-fund \
        "@anthropic-ai/claude-code@${CLAUDE_VERSION}" \
        "@openai/codex@${CODEX_VERSION}" \
        "@github/copilot@${COPILOT_VERSION}" \
        "@earendil-works/pi-coding-agent@${PI_VERSION}" \
        "copilot-custom-system-prompt@${COPILOT_PROMPT_VERSION}" \
    && for bin in claude codex copilot pi copilot-custom-system-prompt; do \
        command -v "$bin" > /dev/null || { echo "missing $bin" >&2; exit 1; }; \
    done

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
        /state /work /run/terrarium /prompts /usr/local/share/terrarium \
        /etc/terrarium/projects /etc/terrarium/agents \
        /etc/terrarium/targets /etc/terrarium/subscriptions \
        /home/terrarium/.pi \
    && ln -s /auth/claude /home/terrarium/.claude \
    && ln -s /auth/codex /home/terrarium/.codex \
    && ln -s /auth/copilot /home/terrarium/.copilot \
    && ln -s /auth/pi /home/terrarium/.pi/agent \
    && chown -R "${UID}:${GID}" /auth /state /work /run/terrarium /etc/terrarium /home/terrarium

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

# The entrypoint reinstalls the Copilot extension when this string changes.
RUN echo "terrarium-$(terrariumd --version | cut -d' ' -f2) copilot-prompt-${COPILOT_PROMPT_VERSION} copilot-${COPILOT_VERSION}" \
        > /usr/local/share/terrarium/image-version

USER terrarium
WORKDIR /home/terrarium
# 8080 dashboard and API, 8081 artifact origin (§15.2), 8082 host agent dial-in (§19).
EXPOSE 8080 8081 8082
# tini is PID 1: it reaps the runners that terrariumd leaves when it dies.
# No `-g`: tini sends SIGTERM to the entrypoint only, and the entrypoint
# stops terrariumd and then the runners in order (docker/README.md).
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/bin/terrarium-entrypoint"]
CMD ["terrariumd"]

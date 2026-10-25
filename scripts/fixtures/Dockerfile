ARG NODE_IMAGE=node:21.0.0-slim
FROM ${NODE_IMAGE} AS runtime
ARG CLAUDE_VERSION=2.1.300
ARG CODEX_VERSION=0.160.0
ARG COPILOT_PROMPT_VERSION=0.1.2
RUN npm install -g \
        "@anthropic-ai/claude-code@${CLAUDE_VERSION}" \
        "@openai/codex@${CODEX_VERSION}" \
        "copilot-custom-system-prompt@${COPILOT_PROMPT_VERSION}"

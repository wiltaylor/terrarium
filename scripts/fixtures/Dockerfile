ARG NODE_IMAGE=node:21.0.0-slim
FROM ${NODE_IMAGE} AS dashboard
RUN npm ci --no-audit --no-fund
FROM ${NODE_IMAGE} AS runtime
ENV NPM_CONFIG_PREFIX=/opt/terrarium/tools
COPY harnesses.toml /etc/terrarium/harnesses.toml

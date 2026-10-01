# syntax=docker/dockerfile:1.7
# The OctoPage service with its dashboard, as one image. `fly deploy` builds it (see
# docs/deploy.md); CI builds it on every push to main.

# The dashboard: static files.
FROM node:22-bookworm-slim AS dashboard
WORKDIR /web
COPY web/dashboard/package.json web/dashboard/package-lock.json ./
RUN npm ci --no-audit --no-fund
COPY web/dashboard/ ./
RUN npm run build

# The server.
FROM rust:1.97.1-slim-bookworm AS server
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p octopage-server \
 && cp target/release/octopage-server /usr/local/bin/octopage-server

# What runs: the server, the dashboard's files, and certificates for GitHub's TLS.
FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates tini \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin octopage
COPY --from=server /usr/local/bin/octopage-server /usr/local/bin/octopage-server
COPY --from=dashboard /web/dist /app/dashboard
COPY deploy/start.sh /usr/local/bin/start
ENV OCTOPAGE_LISTEN=0.0.0.0:8080 \
    OCTOPAGE_DATA=/data \
    OCTOPAGE_DASHBOARD=/app/dashboard \
    RUST_LOG=info
EXPOSE 8080
ENTRYPOINT ["/usr/bin/tini", "--", "/bin/sh", "/usr/local/bin/start"]

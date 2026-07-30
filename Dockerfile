# Leviath in a container: the shared-world daemon plus its REST/WebSocket API.
#
# `lev serve` auto-starts the daemon inside the container and routes agent
# actions through its control socket, so this single process is the whole
# deployment. Agent tools (shell, file I/O) run INSIDE the container - which
# is the isolation most deployments want. The `[sandbox]` feature spawns
# sibling containers and would need the Docker socket mounted; on this image
# leave sandboxing unset and let the container itself be the boundary.
#
#   docker build -t leviath .
#   docker run -d -p 3000:3000 \
#     -e LEVIATH_API_TOKEN=change-me \
#     -v leviath-data:/data \
#     leviath
#
# Configuration lives in the volume at /data/.leviath/config.toml (LEVIATH_HOME
# is /data). Provider keys can go there or in the environment, e.g.
# `-e ANTHROPIC_API_KEY=...`.

FROM rust:1.97.1-slim-bookworm AS builder
WORKDIR /src
COPY . .
# The release profile (fat LTO, one codegen unit) is what ships everywhere
# else; the image gets the same binary users install.
RUN cargo build --release -p leviath-cli

FROM debian:bookworm-slim
# ca-certificates: provider APIs and MCP servers are HTTPS.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/lev /usr/local/bin/lev

# All persistent state (config, runs, agents, providers) under one volume.
ENV LEVIATH_HOME=/data
VOLUME /data
WORKDIR /data

EXPOSE 3000
ENTRYPOINT ["lev"]
# 0.0.0.0 is correct inside the container: Docker's port mapping is the
# boundary, and the API refuses to start without LEVIATH_API_TOKEN anyway.
CMD ["serve", "--host", "0.0.0.0", "--port", "3000"]

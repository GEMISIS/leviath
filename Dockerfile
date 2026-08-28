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
# else; the image gets the same binary users install. `--locked` because the
# committed Cargo.lock and the pinned toolchain are the two halves of a
# reproducible build, and this was the one place that could re-resolve. The
# cache mounts keep the registry and the compiled dependencies between builds
# (BuildKit, the default since Docker 23), so a source edit does not rebuild
# all ~490 packages under fat LTO; the binary is copied out because a cache
# mount is not part of the layer.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p leviath-cli \
    && cp target/release/lev /lev

FROM debian:bookworm-slim
# ca-certificates: provider APIs and MCP servers are HTTPS.
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=builder /lev /usr/local/bin/lev

# All persistent state (config, runs, agents, providers) under one volume.
# Owned by an unprivileged user: the header above says the container is the
# boundary for model-chosen tool calls, and a root process inside it is a
# weaker boundary than it needs to be. A bind-mounted host directory must be
# writable by uid 10001.
ENV LEVIATH_HOME=/data
RUN useradd --system --uid 10001 --home-dir /data --shell /usr/sbin/nologin leviath \
    && mkdir -p /data && chown leviath:leviath /data
VOLUME /data
WORKDIR /data
USER leviath

EXPOSE 3000
# `lev daemon status` exits 0 whether or not the daemon is up (it is a report,
# not a probe), so the check reads its answer. The API itself needs a token,
# which the healthcheck does not have.
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s \
    CMD ["/bin/sh", "-c", "lev daemon status | grep -q 'daemon running'"]
ENTRYPOINT ["lev"]
# 0.0.0.0 is correct inside the container: Docker's port mapping is the
# boundary, and the API refuses to start without LEVIATH_API_TOKEN anyway.
CMD ["serve", "--host", "0.0.0.0", "--port", "3000"]

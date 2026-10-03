# syntax=docker/dockerfile:1

# Plumb Search node for servers: `plumb run` serves the search page on port
# 8080 and keeps everything it downloads and builds in the /data volume.
# docs/docker.md covers running, settings and updates.
#
#   docker build -t plumb-search .
#   docker run -d --init -p 8080:8080 -v plumb-data:/data plumb-search

# Rust toolchain of the build stage; pin one with --build-arg RUST_VERSION=1.99.
ARG RUST_VERSION=1

# Built on bookworm to run on bookworm, so the binary links against the glibc
# it runs with.
FROM rust:${RUST_VERSION}-slim-bookworm AS build
ARG TARGETPLATFORM
WORKDIR /src
# The whole workspace (minus what .dockerignore leaves out): `--locked` checks
# Cargo.lock against every member's manifest, including the desktop app,
# which `-p plumb-node` does not build.
COPY . .
# The crate registry and target/ are cache mounts, so a rebuild compiles only
# what changed. Cache mounts are not saved in the layer, so the binary is
# copied out in the same step.
RUN --mount=type=cache,id=plumb-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=plumb-cargo-git,target=/usr/local/cargo/git/db \
    --mount=type=cache,id=plumb-target-${TARGETPLATFORM},target=/src/target,sharing=locked \
    cargo build --release --locked -p plumb-node \
 && install -D -m 0755 target/release/plumb /out/plumb


FROM debian:bookworm-slim

LABEL org.opencontainers.image.title="Plumb Search" \
      org.opencontainers.image.description="Self-hostable search engine. Runs a Plumb Search node with its search page on port 8080." \
      org.opencontainers.image.source="https://github.com/SueHeir/plumb-search" \
      org.opencontainers.image.documentation="https://github.com/SueHeir/plumb-search/blob/main/docs/docker.md" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"

# Root certificates for the seed downloads and homepage crawls (TLS itself is
# rustls, compiled into the binary).
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates \
 && rm -rf /var/lib/apt/lists/*

# An unprivileged user with a fixed uid and gid (10001), so a host directory
# can be handed to it with `chown 10001:10001`. /data belongs to it, which
# makes a new named volume mounted there writable.
RUN groupadd --gid 10001 plumb \
 && useradd --uid 10001 --gid plumb --no-create-home --no-log-init \
        --home-dir /data --shell /usr/sbin/nologin plumb \
 && mkdir /data \
 && chown plumb:plumb /data

COPY --link LICENSE-MIT LICENSE-APACHE /usr/share/doc/plumb-search/
COPY --link --from=build /out/plumb /usr/local/bin/plumb

USER 10001:10001
WORKDIR /data
# The node keeps seed/, records.jsonl and indexes/ here. Mount a volume at
# /data itself, not at an index directory or a file inside it: the node
# replaces those by renaming, and a mount point cannot be renamed.
VOLUME /data
EXPOSE 8080
# Node-to-node connections, used with `plumb run --network` (docs/network.md).
EXPOSE 4001/tcp 4001/udp
ENTRYPOINT ["plumb"]
CMD ["run", "--data", "/data", "--bind", "0.0.0.0:8080"]

# syntax=docker/dockerfile:1
#
# The {{name}} server in a small non-root Linux image.
#   docker compose up -d --build              ({{compose_note}})
#   docker build -t {{image}} .          (the image alone)
#
# Build arguments (the same as in net_backend's deploy/docker/Dockerfile): CARGO_ARGS (what to
# build), BINARY (the built file), CONFIG (the configuration copied to /etc/net-backend/config.toml).
# Every setting comes from that file; the image sets no NBS__* variable, so a Compose file that
# mounts its own configuration over /etc/net-backend gets exactly that configuration.

ARG RUST_VERSION=1.96.0

FROM rust:${RUST_VERSION}-slim-bookworm AS build
WORKDIR /src
ARG CARGO_ARGS="--bin {{package}}"
ARG BINARY=target/release/{{package}}
ARG CONFIG={{docker_config}}
COPY . .
# Cache mounts keep the registry and the build directory between builds (BuildKit). The project's own
# migrations (./migrations, when present) go to /app/migrations.
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release ${CARGO_ARGS} \
    && install -D -m 0755 "${BINARY}" /out/net-backend-server \
    && install -D -m 0644 "${CONFIG}" /out/config.toml \
    && install -d -m 0755 /out/data /out/migrations \
    && if [ -d migrations ]; then cp -R migrations/. /out/migrations/; fi

# glibc + libgcc only, no shell, no package manager; runs as uid/gid 65532 ("nonroot"); /data belongs
# to it (for a SQLite file).
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/net-backend-server /usr/local/bin/net-backend-server
COPY --from=build /out/config.toml /etc/net-backend/config.toml
COPY --from=build /out/migrations /app/migrations
COPY --from=build --chown=65532:65532 /out/data /data
ENV NBS_CONFIG=/etc/net-backend/config.toml
EXPOSE 8080
USER nonroot:nonroot
# `healthcheck` asks the server's own /readyz (database reachable, not shutting down).
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/net-backend-server", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/net-backend-server"]
CMD ["serve"]

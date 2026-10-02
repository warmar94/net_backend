# syntax=docker/dockerfile:1
#
# The reference server (Auth + Storage + Chat) in a small non-root image.
# Build context: the repository root (compose.yaml sets it), e.g.
#   docker build -f deploy/docker/Dockerfile -t net-backend-server .
#
# Your own server binary: point EXAMPLE / PACKAGE / BIN at it (or copy this file into your project);
# every server built with net_backend_server has the same command line (serve, migrate, ...).

ARG RUST_VERSION=1.96.0

FROM rust:${RUST_VERSION}-slim-bookworm AS build
WORKDIR /src
# The cargo features of the binary: MySQL and PostgreSQL (the URL picks one), the modules, mail, Steam.
ARG FEATURES="mysql,postgres,storage,chat,smtp,steam"
ARG PACKAGE=net_backend_server
ARG EXAMPLE=server
COPY . .
# Cache mounts keep the registry and the build directory between builds (BuildKit).
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p "${PACKAGE}" --example "${EXAMPLE}" --no-default-features --features "${FEATURES}" \
    && install -D -m 0755 "target/release/examples/${EXAMPLE}" /out/net-backend-server

# glibc + libgcc only, no shell, no package manager; runs as uid/gid 65532 ("nonroot").
FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/net-backend-server /usr/local/bin/net-backend-server
ENV NBS_CONFIG=/etc/net-backend/config.toml
EXPOSE 8080
USER nonroot:nonroot
# `healthcheck` asks the server's own /readyz (database reachable, not shutting down).
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/net-backend-server", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/net-backend-server"]
CMD ["serve"]

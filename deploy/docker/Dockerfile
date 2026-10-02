# syntax=docker/dockerfile:1
#
# A net_backend_server binary in a small non-root image (distroless: glibc + libgcc, no shell).
# Build context: the repository root, e.g.
#   docker build -f deploy/docker/Dockerfile -t net-backend-server .
#
# Your own server: copy this file into your project and set the three build arguments, e.g.
#   docker build --build-arg CARGO_ARGS="--bin mygame" --build-arg BINARY=target/release/mygame \
#       --build-arg CONFIG=config.toml -t mygame .
# (in Compose: `build: { context: ., args: { CARGO_ARGS: "--bin mygame", ... } }`). Every server built
# with net_backend_server has the same command line (serve, migrate, config check, user:create, ...).

# Base images pinned by digest (multi-arch indexes), so a rebuild uses the same bytes.
ARG BUILD_IMAGE=rust:1.96.0-slim-bookworm@sha256:4732ca96fd086cb9be682050c3f0176288eebaac2b80aa2bcefccfaf198e1950
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian12:nonroot@sha256:9dac0a79194e45a7da0158a9c6da57b217585af0786db3845d1f0ec1a0dd182f

FROM ${BUILD_IMAGE} AS build
WORKDIR /src
# What to build: the package / binary and its cargo features. The default is the reference server
# with every database (the URL picks one), the modules, mail and Steam.
ARG CARGO_ARGS="-p net_backend_server --example server --no-default-features --features sqlite,mysql,postgres,storage,chat,smtp,steam"
# Where cargo writes that binary.
ARG BINARY=target/release/examples/server
# The configuration baked into the image (the Compose files point the server at their own).
ARG CONFIG=deploy/docker/trial.toml
COPY . .
# Cache mounts keep the registry and the build directory between builds (BuildKit).
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,sharing=locked \
    cargo build --release --locked ${CARGO_ARGS} \
    && install -D -m 0755 "${BINARY}" /out/net-backend-server \
    && install -D -m 0644 "${CONFIG}" /out/config.toml \
    && install -d -m 0755 /out/data

# Runs as uid/gid 65532 ("nonroot"); /data belongs to it (SQLite of the baked configuration).
FROM ${RUNTIME_IMAGE}
COPY --from=build /out/net-backend-server /usr/local/bin/net-backend-server
COPY --from=build /out/config.toml /etc/net-backend/config.toml
COPY --from=build --chown=65532:65532 /out/data /data
ENV NBS_CONFIG=/etc/net-backend/config.toml
EXPOSE 8080
USER nonroot:nonroot
# `healthcheck` asks the server's own /readyz (database reachable, not shutting down).
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/net-backend-server", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/net-backend-server"]
CMD ["serve"]

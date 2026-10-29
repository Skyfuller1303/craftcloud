# CraftCloud server image: Rust build, admin UI build, minimal nonroot runtime.
#
# Build context is the repository root:
#   docker build -f deploy/compose/Dockerfile -t craftcloud-server:local .
#
# Every base image is pinned by digest (multi-arch index digest, tag shown for
# humans). Update tag and digest together; see docs/06-operations/deployment.md.

# rust:1.99-trixie (MSRV 1.99, glibc 2.41 like the runtime image below)
ARG RUST_IMAGE=rust:1.99-trixie@sha256:09027f873d45b8ecd1dc86f1fa13b7a62b875c961f017a0e331267b350430137
# node:24-trixie-slim (current LTS; package.json engines: ^24.15.0)
ARG NODE_IMAGE=node:24-trixie-slim@sha256:54f84b4cf8432b21aaffd5d3ce8d41e12cea0d10d33c09d2f3c9879d49d6b446
# debian:trixie-slim: only the source of /usr/bin/df and the data directories
ARG DEBIAN_IMAGE=debian:trixie-slim@sha256:f6b08b09a05725f9431502d3b5714f5c4576e82c1a84fa1debe0c24a7e0c3ec4
# gcr.io/distroless/cc-debian13:nonroot (glibc and libgcc, no shell, no package manager, uid 65532)
ARG RUNTIME_IMAGE=gcr.io/distroless/cc-debian13:nonroot@sha256:3ee00291cf386ec52a552e6a858d555debd74776eb4bfa53fa3df2cd6677515e

# ---- admin UI (same bytes `cargo xtask web` builds) -------------------------
FROM ${NODE_IMAGE} AS admin
WORKDIR /src/apps/craftcloud-admin
COPY apps/craftcloud-admin/package.json apps/craftcloud-admin/package-lock.json ./
# Lifecycle scripts of dependencies never run.
RUN npm ci --ignore-scripts
COPY apps/craftcloud-admin/ ./
RUN npm run typecheck && npm run build

# ---- server -------------------------------------------------------------------
FROM ${RUST_IMAGE} AS server
# Docker builds run inside a memory-limited VM: keep the compiler parallelism low.
ENV CARGO_BUILD_JOBS=2 \
    CARGO_INCREMENTAL=0 \
    CARGO_PROFILE_RELEASE_STRIP=symbols
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY apps/craftcloud-server apps/craftcloud-server
COPY crates crates
# The workspace lists xtask as a member, so its manifest must exist.
COPY xtask xtask
RUN cargo build --release --locked -p craftcloud-server \
    && install -D target/release/craftcloud-server /out/craftcloud-server

# ---- runtime files that need a shell to create -----------------------------
FROM ${DEBIAN_IMAGE} AS files
# The store belongs to the runtime user from the start so that a fresh named
# volume inherits the owner and mode 0700 (the server insists on a writable directory).
RUN install -d -m 0700 -o 65532 -g 65532 /rootfs/var/lib/craftcloud/store /rootfs/backups \
    && install -D -m 0755 /usr/bin/df /rootfs/usr/bin/df

# ---- runtime ----------------------------------------------------------------
FROM ${RUNTIME_IMAGE}
LABEL org.opencontainers.image.title="craftcloud-server" \
      org.opencontainers.image.description="CraftCloud server with the admin UI (development prototype, not released)"
# `df` backs the operator health view (free disk space); it needs only libc.
COPY --from=files /rootfs/usr/bin/df /usr/bin/df
COPY --from=files --chown=65532:65532 /rootfs/var/lib/craftcloud /var/lib/craftcloud
COPY --from=files --chown=65532:65532 /rootfs/backups /backups
COPY --from=server /out/craftcloud-server /usr/local/bin/craftcloud-server
COPY --from=admin /src/apps/craftcloud-admin/dist /opt/craftcloud/admin
# Stable layout of the image; deployment specifics (public URL, secrets,
# database role, trusted proxies) come from the Compose file.
ENV CRAFTCLOUD_BIND=0.0.0.0:8080 \
    CRAFTCLOUD_ADMIN_DIR=/opt/craftcloud/admin \
    CRAFTCLOUD_STORAGE_DIR=/var/lib/craftcloud/store \
    CRAFTCLOUD_LOG_FORMAT=json
USER 65532:65532
EXPOSE 8080
VOLUME ["/var/lib/craftcloud/store"]
# No shell and no curl in this image: the binary checks itself.
HEALTHCHECK --interval=15s --timeout=5s --start-period=30s --retries=3 \
    CMD ["/usr/local/bin/craftcloud-server", "healthcheck"]
ENTRYPOINT ["/usr/local/bin/craftcloud-server"]

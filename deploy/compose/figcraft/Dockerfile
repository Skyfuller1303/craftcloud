# Unmodified upstream source, pinned by commit and archive SHA-256.
# No FigCraft source from an unrelated worktree enters this build.
FROM rust:1.99-trixie@sha256:09027f873d45b8ecd1dc86f1fa13b7a62b875c961f017a0e331267b350430137 AS build
ADD --checksum=sha256:772978d5d974827f1be0398872f644fbf65e3bc11a59003949f4d2247f90a780 https://codeload.github.com/storytold/figcraft/tar.gz/ddda495a6ff40c326a117fbc393a8cfaa319f529 /tmp/figcraft.tar.gz
WORKDIR /src
RUN tar -xzf /tmp/figcraft.tar.gz --strip-components=1 -C /src && rm /tmp/figcraft.tar.gz
ENV CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0 CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16 CARGO_PROFILE_RELEASE_STRIP=symbols
RUN cargo build --release --locked -p figcraft-server

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:3ee00291cf386ec52a552e6a858d555debd74776eb4bfa53fa3df2cd6677515e
LABEL org.opencontainers.image.source="https://github.com/storytold/figcraft" \
      org.opencontainers.image.revision="ddda495a6ff40c326a117fbc393a8cfaa319f529" \
      org.opencontainers.image.licenses="MIT OR Apache-2.0"
COPY --from=build /src/target/release/figcraft-server /usr/local/bin/figcraft-server
USER 65532:65532
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/local/bin/figcraft-server", "--config", "/etc/figcraft-server/server.toml"]

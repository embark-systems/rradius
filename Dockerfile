FROM rust:1.99-slim-bookworm@sha256:f2c004e3d2d8b547e8cf7ffbc29205385f4e71d23e53e4e383065db5e9f822cd AS build
RUN apt-get update \
    && apt-get install --yes --no-install-recommends cmake g++ make \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /src
# The workspace: cargo reads every member's manifest, so every member's manifest and sources
# come along (.dockerignore keeps the rest out), though only the server and its library are built.
COPY . ./
# As .cargo/config.toml does for builds in the repository: SQLite would otherwise plan some
# queries again on every read (see docs/usage.md section 1).
ENV LIBSQLITE3_FLAGS=-DSQLITE_ENABLE_QPSG
# Kept between builds on this machine, so a code change does not rebuild every dependency.
# jemalloc fixes the page size it supports when it is built, from the machine building it.
# arm64 Linux hosts run kernels with 4, 16 or 64 KiB pages, so an arm64 image is built for
# 64 KiB, which also runs on the smaller ones.
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build \
    if [ "$(uname -m)" = aarch64 ]; then export JEMALLOC_SYS_WITH_LG_PAGE=16; fi \
    && CARGO_TARGET_DIR=/build cargo build --release --locked -p rradius-server \
    && cp /build/release/rradius-server /usr/local/bin/rradius-server

FROM debian:bookworm-slim@sha256:734a72b3d6142a71993b4bc580ca01d8ca687f77a2c422d96a961d3376b179bc
# ca-certificates: LDAP over TLS trusts the system's certificates.
RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --no-create-home --uid 10001 rradius \
    && mkdir -p /etc/rradius /var/lib/rradius \
    && chown rradius:rradius /var/lib/rradius
COPY --from=build /usr/local/bin/rradius-server /usr/local/bin/rradius-server
COPY share /usr/share/rradius
USER rradius
VOLUME /var/lib/rradius
# RADIUS authentication and accounting, TACACS+, cluster Raft and API. Without
# [[cluster.nodes]] or `listen` the cluster ports only bind 127.0.0.1.
EXPOSE 1812/udp 1813/udp 49/tcp 8100/tcp 8200/tcp
ENTRYPOINT ["/usr/local/bin/rradius-server"]
CMD ["--config", "/etc/rradius/rradius.toml"]

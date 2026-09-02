# The LB image: our own source built with the pinned toolchain, both
# base images held still by digest. The FROM tag is this build's
# compiler pin; rust-toolchain.toml is deliberately not copied in, so
# rustup does not also install the repo's rustfmt/clippy components.
# Keep the tag and the toolchain file in step when bumping either.
FROM rust:1.95.0-slim-trixie@sha256:e14e87345b4d5964ddcc3491d27ee046a0f23820f340c3c1e24da6880141f7c0 AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p lb

# Same Debian release as the build stage, so the binary meets the glibc
# it was linked against. ca-certificates: the reference endpoint is
# https.
FROM debian:trixie-slim@sha256:d7e12182ce18b85b93007c1dedf31f2d29e01ccf3182cc4017c709b6259bc132
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/arkiv-lb /usr/local/bin/arkiv-lb
USER 65534:65534
# The config is mounted in by compose.
ENTRYPOINT ["arkiv-lb", "/etc/arkiv-lb/config.toml"]

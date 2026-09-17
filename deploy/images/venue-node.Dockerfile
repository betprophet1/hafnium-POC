# The mini venue's image: venue-node (core/node), venue-gateway (core/gateway), venue-parlays (core/parlays), venue-sim
# (core/sim), venue-treasury (core/treasury), venue-identity (core/identity) and venue-observer (core/observer). Build from the repository root:
#   docker build -f deploy/images/venue-node.Dockerfile -t venue:dev .
# The toolchain comes from core/rust-toolchain.toml; release profile, as every environment runs (blueprint §16).

FROM rust:1.96.0-slim-bookworm AS build
WORKDIR /src
COPY core/ core/
RUN cd core && cargo build --release --locked -p venue-node -p venue-gateway -p venue-parlays -p venue-sim -p venue-treasury -p venue-identity -p venue-observer

FROM debian:bookworm-slim
# tini runs as PID 1 so SIGTERM stops the binaries at once. The kernel gives PID 1 no default signal actions, and a
# process ignoring SIGTERM holds its sessions open until the kill after the grace period; a deleted maker pod would
# keep quoting beside its replacement.
RUN apt-get update && apt-get install -y --no-install-recommends tini && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --home-dir /var/lib/venue venue \
 && mkdir -p /var/lib/venue && chown 10001:10001 /var/lib/venue
COPY --from=build /src/core/target/release/venue-node /src/core/target/release/venue-gateway \
     /src/core/target/release/venue-parlays /src/core/target/release/venue-sim \
     /src/core/target/release/venue-treasury /src/core/target/release/venue-identity \
     /src/core/target/release/venue-observer /usr/local/bin/
USER 10001
ENV VENUE_LISTEN=0.0.0.0:7101 VENUE_DATA=/var/lib/venue
EXPOSE 7101 7102 7103 8080 8081 8082 8083 8084 9101 9102 9103
VOLUME /var/lib/venue
ENTRYPOINT ["tini", "--", "venue-node"]
CMD ["serve"]

# The mini venue's image: venue-node (core/node), venue-gateway (core/gateway), venue-parlays (core/parlays), venue-sim
# (core/sim), venue-treasury (core/treasury) and venue-identity (core/identity). Build from the repository root:
#   docker build -f deploy/images/venue-node.Dockerfile -t venue:dev .
# The toolchain comes from core/rust-toolchain.toml; release profile, as every environment runs (blueprint §16).

FROM rust:1.96.0-slim-bookworm AS build
# OpenSSL's headers, for TLS to PostgreSQL (core/store).
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY core/ core/
RUN cd core && cargo build --release --locked -p venue-node -p venue-gateway -p venue-parlays -p venue-sim -p venue-treasury -p venue-identity

FROM debian:bookworm-slim
# tini runs as PID 1 so SIGTERM stops the binaries at once. The kernel gives PID 1 no default signal actions, and a
# process ignoring SIGTERM holds its sessions open until the kill after the grace period; a deleted maker pod would
# keep quoting beside its replacement.
# libssl3 and the CA certificates: the services reach PostgreSQL over TLS where it is configured (verify-full checks
# the server against these CAs unless a CA file is given).
RUN apt-get update && apt-get install -y --no-install-recommends tini libssl3 ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --home-dir /var/lib/venue venue \
 && mkdir -p /var/lib/venue && chown 10001:10001 /var/lib/venue
COPY --from=build /src/core/target/release/venue-node /src/core/target/release/venue-gateway \
     /src/core/target/release/venue-parlays /src/core/target/release/venue-sim \
     /src/core/target/release/venue-treasury /src/core/target/release/venue-identity /usr/local/bin/
USER 10001
ENV VENUE_LISTEN=0.0.0.0:7101 VENUE_DATA=/var/lib/venue
EXPOSE 7101 7102 7103 8080 8081 8082 8083 8084
VOLUME /var/lib/venue
ENTRYPOINT ["tini", "--", "venue-node"]
CMD ["serve"]

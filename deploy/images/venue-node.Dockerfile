# The mini venue's image: venue-node (core/node), venue-gateway (core/gateway), venue-parlays (core/parlays), venue-sim
# (core/sim), venue-treasury (core/treasury), venue-identity (core/identity) and venue-observer (core/observer). Build from the repository root:
#   docker build -f deploy/images/venue-node.Dockerfile -t venue:dev .
# The toolchain comes from core/rust-toolchain.toml; release profile, as every environment runs (blueprint §16).

FROM rust:1.96.0-slim-bookworm AS build
# OpenSSL's headers, for TLS to PostgreSQL (core/store).
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY core/ core/
# venue-link carries `venue-view`, which takes both halves of a snapshot pair. It is in the image because
# the scheduled pair job runs it against the live journal volume, not because any service needs it.
RUN cd core && cargo build --release --locked -p venue-node -p venue-gateway -p venue-parlays -p venue-sim -p venue-treasury -p venue-identity -p venue-observer -p venue-link

# Atlas applies the schemas (core/store/decisions.md, S-2), pinned by version and by the hash of the binary.
FROM debian:bookworm-slim AS atlas
ARG ATLAS_VERSION=v1.3.3
ARG ATLAS_SHA256=a41ea66b5aaad1e363fd829613a6725c3934378792d0be5591269c041c112b96
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates && rm -rf /var/lib/apt/lists/* \
 && curl -fsSL -o /atlas "https://release.ariga.io/atlas/atlas-linux-amd64-${ATLAS_VERSION}" \
 && echo "${ATLAS_SHA256}  /atlas" | sha256sum -c - \
 && chmod +x /atlas

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
     /src/core/target/release/venue-treasury /src/core/target/release/venue-identity \
     /src/core/target/release/venue-observer /src/core/target/release/venue-view /usr/local/bin/
COPY --from=atlas /atlas /usr/local/bin/atlas
# The migrations as files, which is what Atlas applies: the same directories the services embed, under the service
# name (the migrate job names one of them, deploy/charts/venue-mini/templates/migrate.yaml).
COPY core/treasury/migrations/ /usr/share/venue/migrations/treasury/
COPY core/identity/migrations/ /usr/share/venue/migrations/identity/
COPY core/parlays/migrations/ /usr/share/venue/migrations/parlays/
COPY core/sim/migrations/ /usr/share/venue/migrations/oracle/
USER 10001
ENV VENUE_LISTEN=0.0.0.0:7101 VENUE_DATA=/var/lib/venue
EXPOSE 7101 7102 7103 8080 8081 8082 8083 8084 9101 9102 9103
VOLUME /var/lib/venue
ENTRYPOINT ["tini", "--", "venue-node"]
CMD ["serve"]

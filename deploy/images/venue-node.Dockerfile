# The core's image: venue-node, the single-member node (core/node). Build from the repository root:
#   docker build -f deploy/images/venue-node.Dockerfile -t venue-node:dev .
# The toolchain comes from core/rust-toolchain.toml; release profile, as every environment runs (blueprint §16).

FROM rust:1.96.0-slim-bookworm AS build
WORKDIR /src
COPY core/ core/
RUN cd core && cargo build --release --locked -p venue-node

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /var/lib/venue venue \
 && mkdir -p /var/lib/venue && chown 10001:10001 /var/lib/venue
COPY --from=build /src/core/target/release/venue-node /usr/local/bin/venue-node
USER 10001
ENV VENUE_LISTEN=0.0.0.0:7101 VENUE_DATA=/var/lib/venue
EXPOSE 7101
VOLUME /var/lib/venue
ENTRYPOINT ["venue-node"]
CMD ["serve"]

# The mini venue's image: venue-node (core/node) and venue-sim (core/sim). Build from the repository root:
#   docker build -f deploy/images/venue-node.Dockerfile -t venue:dev .
# The toolchain comes from core/rust-toolchain.toml; release profile, as every environment runs (blueprint §16).

FROM rust:1.96.0-slim-bookworm AS build
WORKDIR /src
COPY core/ core/
RUN cd core && cargo build --release --locked -p venue-node -p venue-sim

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /var/lib/venue venue \
 && mkdir -p /var/lib/venue && chown 10001:10001 /var/lib/venue
COPY --from=build /src/core/target/release/venue-node /src/core/target/release/venue-sim /usr/local/bin/
USER 10001
ENV VENUE_LISTEN=0.0.0.0:7101 VENUE_DATA=/var/lib/venue
EXPOSE 7101 7102 8080
VOLUME /var/lib/venue
ENTRYPOINT ["venue-node"]
CMD ["serve"]

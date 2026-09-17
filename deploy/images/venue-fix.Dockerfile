# The image venue-fix: the FIX 4.4 adapter (adapters/fix, interface 4), built from source. Build from the repository
# root:
#   docker build -f deploy/images/venue-fix.Dockerfile -t venue-fix:dev .
# Pinned as adapters/fix/README.md pins its toolchain, each base image by tag and digest: Gradle 9.7.1 on JDK 21 to
# build, a Temurin 21 JRE to run. Gradle runs from
# the build image rather than the wrapper, whose jar is not committed; build.gradle pins Artio and everything else,
# and resolves them from Maven Central. Nothing secret enters any layer: API keys and dev tokens arrive at logon.

FROM gradle:9.7.1-jdk21-noble@sha256:80f853adfcd9ee9c5078dabca29377b019d1236aa7d1a68034104445297271d7 AS build
WORKDIR /src/adapters/fix
# The codecs are generated from the interface's dictionary, which build.gradle reads from ../../interfaces/fix.
COPY interfaces/fix/venue-fix44.xml /src/interfaces/fix/venue-fix44.xml
COPY adapters/fix/build.gradle adapters/fix/settings.gradle adapters/fix/gradle.properties ./
# Dependencies in a layer of their own, so a source change does not fetch them again.
RUN gradle --no-daemon --console=plain -q dependencies --configuration runtimeClasspath >/dev/null
COPY adapters/fix/src/main src/main
RUN gradle --no-daemon --console=plain installDist

FROM eclipse-temurin:21.0.12_8-jre-noble@sha256:86883d2dc1d0e57d4fb2c539f5fd3a2155749c0bdcdccb9cb452828bdc8b0caf
# tini as PID 1 forwards SIGTERM to the JVM, whose shutdown handler stops Artio's engine (which logs its sessions out)
# and closes the audit file and the archive cleanly (FixAdapter.main).
RUN apt-get update && apt-get install -y --no-install-recommends tini && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --home-dir /var/lib/venue-fix venue \
 && mkdir -p /var/lib/venue-fix && chown 10001:10001 /var/lib/venue-fix && chmod 0700 /var/lib/venue-fix
COPY --from=build /src/adapters/fix/build/install/venue-fix-adapter /opt/venue-fix-adapter
USER 10001
# Artio's archive under the data directory keeps every message, Logon Password (554) included, in clear: whatever
# the adapter creates is readable by its own user only.
ENV VENUE_FIX_LISTEN=0.0.0.0:9878 VENUE_FIX_DATA=/var/lib/venue-fix/data VENUE_FIX_AUDIT_DIR=/var/lib/venue-fix/audit
EXPOSE 9878
ENTRYPOINT ["tini", "--", "/bin/sh", "-c", "umask 077 && exec /opt/venue-fix-adapter/bin/venue-fix-adapter \"$@\"", "venue-fix-adapter"]

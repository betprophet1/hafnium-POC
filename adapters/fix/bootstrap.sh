#!/usr/bin/env bash
# Fetches the pinned JDK and the Gradle wrapper into adapters/fix/.toolchain, so the adapter builds on a machine
# with no Java installed and nothing is installed system-wide. Every download is checked against a pinned SHA-256;
# nothing fetched here is committed (see .gitignore).
#
#   adapters/fix/bootstrap.sh          idempotent: does nothing when the toolchain is already in place
#   eval "$(adapters/fix/bootstrap.sh --env)"   prints JAVA_HOME and GRADLE_USER_HOME exports
#   adapters/fix/bootstrap.sh --check  exit 0 when the toolchain is in place, 1 when it is not; fetches nothing
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
TOOLS="$HERE/.toolchain"

# Temurin 21, the long-term-support JDK Artio 0.18x is built and tested against.
JDK_VERSION="21.0.12.1+1"
JDK_URL="https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.12.1%2B1/OpenJDK21U-jdk_x64_linux_hotspot_21.0.12.1_1.tar.gz"
JDK_SHA256="ce79869e1307ed8ee1e2baa86a412b1eb5b75d10a01006d788a6f968bcfaee94"
# Gradle is only needed once, to write the wrapper jar; gradle/wrapper/gradle-wrapper.properties pins the
# distribution the wrapper itself runs, with the same checksum.
GRADLE_VERSION="9.7.1"
GRADLE_URL="https://services.gradle.org/distributions/gradle-$GRADLE_VERSION-bin.zip"
GRADLE_SHA256="acd53f1edaf02f1a8ff99879f8a34b302661a057d9b063ae9e35b552f804d20a"

JAVA_HOME="$TOOLS/jdk-$JDK_VERSION"
export JAVA_HOME
export GRADLE_USER_HOME="$TOOLS/gradle-home"

if [[ "${1:-}" == "--check" ]]; then
  [[ -x "$JAVA_HOME/bin/java" && -f "$HERE/gradle/wrapper/gradle-wrapper.jar" ]]
  exit
fi

if [[ "${1:-}" == "--env" ]]; then
  printf 'export JAVA_HOME=%q\nexport GRADLE_USER_HOME=%q\n' "$JAVA_HOME" "$GRADLE_USER_HOME"
  exit 0
fi

fetch() { # url sha256 destination
  if [[ ! -f "$3" ]] || ! echo "$2  $3" | sha256sum --quiet -c - 2>/dev/null; then
    echo "fetching $1"
    curl -fsSL -o "$3.part" "$1"
    echo "$2  $3.part" | sha256sum --quiet -c - || { echo "checksum mismatch for $1" >&2; exit 1; }
    mv "$3.part" "$3"
  fi
}

mkdir -p "$TOOLS/downloads"
if [[ ! -x "$JAVA_HOME/bin/java" ]]; then
  archive="$TOOLS/downloads/$(basename "${JDK_URL//%2B/+}")"
  fetch "$JDK_URL" "$JDK_SHA256" "$archive"
  rm -rf "$JAVA_HOME.part" && mkdir -p "$JAVA_HOME.part"
  tar -xzf "$archive" -C "$JAVA_HOME.part" --strip-components 1
  mv "$JAVA_HOME.part" "$JAVA_HOME"
fi

if [[ ! -f "$HERE/gradle/wrapper/gradle-wrapper.jar" ]]; then
  zip="$TOOLS/downloads/gradle-$GRADLE_VERSION-bin.zip"
  fetch "$GRADLE_URL" "$GRADLE_SHA256" "$zip"
  if [[ ! -x "$TOOLS/gradle-$GRADLE_VERSION/bin/gradle" ]]; then
    (cd "$TOOLS" && "$JAVA_HOME/bin/jar" xf "$zip" && chmod +x "gradle-$GRADLE_VERSION/bin/gradle")
  fi
  # Writes gradle/wrapper/gradle-wrapper.jar (and rewrites gradlew identically). The properties file is committed.
  (cd "$HERE" && "$TOOLS/gradle-$GRADLE_VERSION/bin/gradle" --no-daemon -q wrapper \
     --gradle-version "$GRADLE_VERSION" --distribution-type bin --gradle-distribution-sha256-sum "$GRADLE_SHA256")
fi
echo "toolchain ready: JAVA_HOME=$JAVA_HOME"

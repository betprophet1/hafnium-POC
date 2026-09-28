#!/usr/bin/env bash
# Provision one box: toolchain, source, binaries, and the kernel limits a 30k-socket client needs.
# $1 = public IP, $2 = role (server|client)
set -eu
T=/home/someuser/.claude-prophetx/jobs/5ec94852/tmp
C=$T/cloud
HOST="$1"; ROLE="$2"
SSH="ssh -i $C/venue-harness-test.pem -o StrictHostKeyChecking=no -o ConnectTimeout=15 ubuntu@$HOST"

for _ in $(seq 1 30); do $SSH true 2>/dev/null && break; sleep 10; done
echo "[$ROLE] ssh up"

$SSH 'sudo apt-get update -qq && sudo apt-get install -y -qq build-essential pkg-config libssl-dev >/dev/null 2>&1; echo apt done'
$SSH 'curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null 2>&1; echo rust done'

# The client opens two sockets per retail customer to one remote ip:port, so it needs the widest ephemeral
# range it can have and a lot of descriptors and threads. The server needs the descriptors too. Nothing here
# listens below 20000 on the client, and on the server the range is kept clear of 17101/17102/18081/18090 --
# an ephemeral connection grabbing a service port is how the node failed to bind last time.
if [ "$ROLE" = client ]; then RANGE="1024 65535"; else RANGE="20000 65535"; fi
$SSH "sudo sysctl -w fs.file-max=3000000 net.ipv4.ip_local_port_range='$RANGE' net.core.somaxconn=16384 \
  net.ipv4.tcp_max_syn_backlog=16384 kernel.threads-max=1000000 vm.max_map_count=1048576 \
  net.ipv4.tcp_tw_reuse=1 >/dev/null; echo limits set"
$SSH "printf '* soft nofile 1048576\n* hard nofile 1048576\n* soft nproc 400000\n* hard nproc 400000\n' | sudo tee -a /etc/security/limits.conf >/dev/null; echo ulimits written"

$SSH 'mkdir -p ~/venue/run'
scp -i $C/venue-harness-test.pem -o StrictHostKeyChecking=no -q $C/src-harness.tar.gz ubuntu@$HOST:~/
# touch after extracting: `git archive` stamps files with the commit time, which can be older than artifacts
# from an earlier build, and cargo then decides nothing changed and silently keeps the stale binary.
$SSH 'cd ~/venue && tar xzf ~/src-harness.tar.gz && find . -name "*.rs" -o -name "*.toml" | xargs touch'
$SSH 'cd ~/venue/core && . ~/.cargo/env && cargo build --profile perf --bins 2>&1 | tail -1'
$SSH 'cd ~/venue/core && ./target/perf/venue-sim gwload 2>&1 | head -2 || true'
echo "[$ROLE] BUILT"

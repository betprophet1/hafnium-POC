#!/usr/bin/env bash
# Throughput and memory of the gateway under load, on two throwaway EC2 boxes: node and gateway on one, the load
# generator on the other. How to use it, what it costs and how to read the results: deploy/loadtest/README.md.
#
#   loadtest.sh up              make the two boxes (key pair, security group, instances)
#   loadtest.sh provision       build the code at $REF on both (a minute or two each, in parallel)
#   loadtest.sh run [--quick]   the load: 3 representative replicates, a retail ladder, a 5-minute soak (~20 min);
#                               --quick is one 15-second load, to check the setup works
#   loadtest.sh fetch DIR       copy the results and both machines' facts into DIR
#   loadtest.sh down            remove everything `up` made, and list anything left under the name
#   loadtest.sh status          what exists under the name now
#   loadtest.sh all DIR         up, provision, run, fetch, and down, which runs even if a step fails
#
# Settings, by environment: AWS_PROFILE (venue), AWS_REGION (eu-central-1), LOADTEST_AZ (<region>b),
# SERVER_TYPE (c6i.8xlarge), CLIENT_TYPE (c6i.4xlarge), SERVER_MARKET and CLIENT_MARKET (ondemand | spot),
# REF (HEAD), LOADTEST_NAME (venue-loadtest), LOADTEST_STATE (~/.cache/venue-loadtest).
#
# Never run the load itself on a laptop or a shared machine (README.md, why two boxes).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

PROFILE="${AWS_PROFILE:-venue}"
REGION="${AWS_REGION:-eu-central-1}"
AZ="${LOADTEST_AZ:-${REGION}b}"
SERVER_TYPE="${SERVER_TYPE:-c6i.8xlarge}"
CLIENT_TYPE="${CLIENT_TYPE:-c6i.4xlarge}"
SERVER_MARKET="${SERVER_MARKET:-ondemand}"
CLIENT_MARKET="${CLIENT_MARKET:-ondemand}"
REF="${REF:-HEAD}"
NAME="${LOADTEST_NAME:-venue-loadtest}"
STATE="${LOADTEST_STATE:-$HOME/.cache/venue-loadtest}"

AWS=(aws --profile "$PROFILE" --region "$REGION")
KEY="$STATE/$NAME.pem"
SSHOPTS=(-i "$KEY" -o StrictHostKeyChecking=no -o ConnectTimeout=15 -o ServerAliveInterval=30)
mkdir -p "$STATE"

say() { echo "$(date -u +%T) $*"; }
server_host() { cat "$STATE/SERVER_HOST"; }
client_host() { cat "$STATE/CLIENT_HOST"; }
server_private() { cat "$STATE/SERVER_PRIVATE"; }
srv() { ssh "${SSHOPTS[@]}" "ubuntu@$(server_host)" "$@"; }
cli() { ssh "${SSHOPTS[@]}" "ubuntu@$(client_host)" "$@"; }

up() {
  if [ -n "$("${AWS[@]}" ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" \
      --query 'SecurityGroups[].GroupId' --output text)" ]; then
    echo "a security group named $NAME already exists: run '$0 down' first, or set LOADTEST_NAME" >&2; exit 1
  fi
  local ip sg ami subnet
  ip=$(curl -s --max-time 10 https://checkip.amazonaws.com)
  "${AWS[@]}" ec2 create-key-pair --key-name "$NAME" --query KeyMaterial --output text > "$KEY"
  chmod 600 "$KEY"
  sg=$("${AWS[@]}" ec2 create-security-group --group-name "$NAME" \
    --description "throwaway: gateway load test" --query GroupId --output text)
  echo "$sg" > "$STATE/SG"
  # ssh from this machine only, and anything between the two boxes.
  "${AWS[@]}" ec2 authorize-security-group-ingress --group-id "$sg" --protocol tcp --port 22 --cidr "$ip/32" >/dev/null
  "${AWS[@]}" ec2 authorize-security-group-ingress --group-id "$sg" --protocol all --source-group "$sg" >/dev/null
  say "security group $sg: ssh from $ip/32, all traffic within the group"
  ami=$("${AWS[@]}" ssm get-parameter \
    --name /aws/service/canonical/ubuntu/server/24.04/stable/current/amd64/hvm/ebs-gp3/ami-id \
    --query Parameter.Value --output text)
  # One zone for both, so they talk over private addresses.
  subnet=$("${AWS[@]}" ec2 describe-subnets --filters "Name=availability-zone,Values=$AZ" \
    --query 'Subnets[0].SubnetId' --output text)
  launch() {  # role type market
    local market=(--instance-market-options '{"MarketType":"spot","SpotOptions":{"SpotInstanceType":"one-time"}}')
    [ "$3" = ondemand ] && market=()
    "${AWS[@]}" ec2 run-instances --image-id "$ami" --instance-type "$2" --key-name "$NAME" \
      --security-group-ids "$sg" --subnet-id "$subnet" "${market[@]}" \
      --block-device-mappings '[{"DeviceName":"/dev/sda1","Ebs":{"VolumeSize":80,"VolumeType":"gp3"}}]' \
      --tag-specifications "ResourceType=instance,Tags=[{Key=Name,Value=$NAME-$1},{Key=Purpose,Value=throwaway gateway load test}]" \
      --query 'Instances[0].InstanceId' --output text
  }
  local server client
  server=$(launch server "$SERVER_TYPE" "$SERVER_MARKET")
  client=$(launch client "$CLIENT_TYPE" "$CLIENT_MARKET")
  say "server $server ($SERVER_TYPE, $SERVER_MARKET), client $client ($CLIENT_TYPE, $CLIENT_MARKET), zone $AZ"
  "${AWS[@]}" ec2 wait instance-running --instance-ids "$server" "$client"
  addr() { "${AWS[@]}" ec2 describe-instances --instance-ids "$1" --query "Reservations[0].Instances[0].$2" --output text; }
  addr "$server" PublicIpAddress > "$STATE/SERVER_HOST"
  addr "$server" PrivateIpAddress > "$STATE/SERVER_PRIVATE"
  addr "$client" PublicIpAddress > "$STATE/CLIENT_HOST"
  say "up: server $(server_host) (private $(server_private)), client $(client_host)"
}

provision_one() {  # host role
  local host="$1" role="$2" ssh=(ssh "${SSHOPTS[@]}" "ubuntu@$1") range
  for _ in $(seq 1 30); do "${ssh[@]}" true 2>/dev/null && break; sleep 10; done
  "${ssh[@]}" 'sudo apt-get update -qq && sudo apt-get install -y -qq build-essential pkg-config libssl-dev >/dev/null 2>&1'
  "${ssh[@]}" 'curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal >/dev/null 2>&1'
  # The client opens two sockets per retail client to one ip:port, so it needs the widest ephemeral range; the
  # server keeps its range clear of the venue's ports (17101, 17102, 18081, 18090).
  if [ "$role" = client ]; then range="1024 65535"; else range="20000 65535"; fi
  "${ssh[@]}" "sudo sysctl -w fs.file-max=3000000 net.ipv4.ip_local_port_range='$range' net.core.somaxconn=16384 \
    net.ipv4.tcp_max_syn_backlog=16384 kernel.threads-max=1000000 vm.max_map_count=1048576 net.ipv4.tcp_tw_reuse=1 >/dev/null"
  "${ssh[@]}" "printf '* soft nofile 1048576\n* hard nofile 1048576\n* soft nproc 400000\n* hard nproc 400000\n' | sudo tee -a /etc/security/limits.conf >/dev/null"
  "${ssh[@]}" 'mkdir -p ~/venue/run'
  scp "${SSHOPTS[@]}" -q "$STATE/src.tar.gz" "$HERE"/remote/*.sh "ubuntu@$host:~/"
  # touch after extracting: `git archive` stamps files with the commit time, and cargo could otherwise keep a stale binary.
  "${ssh[@]}" 'cd ~/venue && tar xzf ~/src.tar.gz && find . -name "*.rs" -o -name "*.toml" | xargs touch'
  "${ssh[@]}" 'cd ~/venue/core && . ~/.cargo/env && cargo build --profile perf -p venue-node -p venue-gateway -p venue-sim 2>&1 | tail -1'
  "${ssh[@]}" 'test -x ~/venue/core/target/perf/venue-gateway && test -x ~/venue/core/target/perf/venue-sim'
  say "$role built"
}

provision() {
  git -C "$ROOT" archive --format=tar.gz -o "$STATE/src.tar.gz" "$REF"
  git -C "$ROOT" rev-parse "$REF" > "$STATE/REF"
  say "building $(cut -c1-12 "$STATE/REF") on both boxes"
  provision_one "$(server_host)" server > "$STATE/provision-server.log" 2>&1 & local s=$!
  provision_one "$(client_host)" client > "$STATE/provision-client.log" 2>&1 & local c=$!
  local failed=0
  wait $s || { echo "server provisioning failed: $STATE/provision-server.log" >&2; failed=1; }
  wait $c || { echo "client provisioning failed: $STATE/provision-client.log" >&2; failed=1; }
  [ $failed -eq 0 ] || exit 1
  say "both built"
}

server_up()     { srv "bash ~/start_server.sh $1 $2 $3" >/dev/null; }
sampler_start() { srv "nohup bash ~/sample.sh ~/venue/run/srv-$1.csv > /dev/null 2>&1 < /dev/null & echo \$! > ~/sampler.pid"; }
sampler_stop()  { srv 'kill $(cat ~/sampler.pid) 2>/dev/null; true'; }
load()          { local label="$1"; shift; cli "bash ~/client_run.sh $(server_private) $label $*" >/dev/null; srv "bash ~/srv_after.sh $label"; say "$label done"; }

run() {
  srv 'rm -f ~/venue/run/server.txt ~/venue/run/srv-*.csv'
  cli 'rm -f ~/venue/run/client.txt ~/venue/run/cli-*.csv'
  if [ "${1:-}" = --quick ]; then
    server_up small 512 1200000
    sampler_start quick
    load quick --connections 32 --seconds 15 --mode trade --orders-per-trade 10
    sampler_stop
    cli 'tail -2 ~/venue/run/client.txt'
    return
  fi
  # The representative load of ADR-0002 (docs/adr/measurements/real.sh): 8 books with exponentially unequal
  # popularity, ~10 orders per trade, 60 s at 32 and at 128 connections, three replicates on a fresh venue each.
  for r in 1 2 3; do
    server_up small 512 1200000
    sampler_start rep-$r
    load rep$r-trade32  --connections 32  --seconds 60 --mode trade --orders-per-trade 10
    load rep$r-trade128 --connections 128 --seconds 60 --mode trade --orders-per-trade 10
    sampler_stop
  done
  # The retail population sweep (docs/adr/measurements/client-sweep.sh). `launch`, not `small`: small holds 4,096
  # accounts and fail-stops partway through seeding a retail-sized population.
  for n in 0 1024 4096 16384; do
    server_up launch 33000 100000000
    sampler_start ladder-$n
    load ladder-$n --connections 32 --mode trade --orders-per-trade 10 --retail $n --retail-books 2 \
      --watchers 8 --retail-order-seconds 10 --seconds 60
    sampler_stop
    sleep 8
  done
  # Five minutes at 128 connections: whether anything grows with time.
  server_up small 512 1200000
  sampler_start soak
  load soak --connections 128 --seconds 300 --mode trade --orders-per-trade 10
  sampler_stop
  srv 'pkill -x venue-gateway; pkill -x venue-node; true'
  say "run complete"
}

fetch() {
  local out="${1:?usage: $0 fetch DIR}"
  mkdir -p "$out"
  scp "${SSHOPTS[@]}" -q "ubuntu@$(server_host):~/venue/run/srv-*.csv" "ubuntu@$(server_host):~/venue/run/server.txt" "$out/"
  scp "${SSHOPTS[@]}" -q "ubuntu@$(client_host):~/venue/run/cli-*.csv" "ubuntu@$(client_host):~/venue/run/client.txt" "$out/"
  local facts='echo "kernel: $(uname -sr)"; echo "cpu: $(lscpu | sed -n "s/^Model name: *//p")"; echo "vcpus: $(nproc)"; echo "memory: $(free -g | awk "NR==2{print \$2}") GiB"; . ~/.cargo/env; echo "rustc: $(rustc --version)"'
  srv "$facts" > "$out/facts-server.txt"
  cli "$facts" > "$out/facts-client.txt"
  cp "$STATE/REF" "$out/REF"
  say "fetched into $out ($(ls "$out" | wc -l) files)"
}

down() {
  local ids sg
  ids=$("${AWS[@]}" ec2 describe-instances --filters "Name=tag:Name,Values=$NAME-*" \
    "Name=instance-state-name,Values=pending,running,stopping,stopped" \
    --query 'Reservations[].Instances[].InstanceId' --output text)
  if [ -n "$ids" ]; then
    say "terminating $ids"
    "${AWS[@]}" ec2 terminate-instances --instance-ids $ids >/dev/null
    "${AWS[@]}" ec2 wait instance-terminated --instance-ids $ids
  fi
  sg=$("${AWS[@]}" ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" \
    --query 'SecurityGroups[].GroupId' --output text)
  if [ -n "$sg" ]; then
    for _ in 1 2 3 4 5 6; do
      "${AWS[@]}" ec2 delete-security-group --group-id "$sg" >/dev/null 2>&1 && { say "deleted security group $sg"; break; }
      sleep 10
    done
  fi
  "${AWS[@]}" ec2 delete-key-pair --key-name "$NAME" >/dev/null 2>&1 && say "deleted key pair $NAME"
  rm -f "$KEY"
  status
}

status() {
  echo "under the name $NAME (nothing listed = nothing left):"
  "${AWS[@]}" ec2 describe-instances --filters "Name=tag:Name,Values=$NAME-*" \
    "Name=instance-state-name,Values=pending,running,stopping,stopped" \
    --query 'Reservations[].Instances[].[InstanceId,InstanceType,State.Name,Tags[?Key==`Name`]|[0].Value]' --output text
  "${AWS[@]}" ec2 describe-security-groups --filters "Name=group-name,Values=$NAME" --query 'SecurityGroups[].GroupId' --output text
  "${AWS[@]}" ec2 describe-key-pairs --filters "Name=key-name,Values=$NAME" --query 'KeyPairs[].KeyName' --output text
}

all() {
  local out="${1:?usage: $0 all DIR}"
  # Whatever happens after `up`, the boxes are removed: a forgotten pair of the default types costs about $50 a day.
  trap 'echo; say "removing everything"; down' EXIT
  up
  provision
  run
  fetch "$out"
}

case "${1:-}" in
  up) up ;;
  provision) provision ;;
  run) shift; run "$@" ;;
  fetch) shift; fetch "$@" ;;
  down) down ;;
  status) status ;;
  all) shift; all "$@" ;;
  *) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 1 ;;
esac

#!/usr/bin/env bash
# Starting, stopping and setting up the AWS box (deploy/aws/README.md).
#
# `stop` is the whole cost control. A stopped instance bills only its disk, so the difference between using this a
# few hours a week and leaving it up is roughly $8 a month against $50. The Elastic IP survives a stop, so nothing
# has to be reconfigured on the way back.
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
TOFU="${VENUE_TOFU:-tofu}"
HERE_FOR_OUTPUT="$(cd "$(dirname "$0")" && pwd)"
# Whatever the deployment was actually made with, rather than a guess that has to be corrected on every command.
PROFILE="${VENUE_AWS_PROFILE:-$("$TOFU" -chdir="$HERE_FOR_OUTPUT" output -raw profile 2>/dev/null || echo venue)}"
REGION="${VENUE_AWS_REGION:-$("$TOFU" -chdir="$HERE_FOR_OUTPUT" output -raw region 2>/dev/null || echo eu-central-1)}"
AWS=(aws --profile "$PROFILE" --region "$REGION")

usage() {
  cat <<'EOF'
usage: venue.sh <command>

  start      start the instance and wait for the API to answer
  stop       stop it; the disk and the address stay, the hourly charge does not
  status     what it is doing, and what that is costing
  ssh        a shell on it
  kubeconfig fetch its kubeconfig to ~/.kube/venue-aws
  install    ArgoCD, cert-manager and the preview machinery, on a running box
  destroy    tear everything down, disk included
EOF
}

instance_id() {
  "$TOFU" -chdir="$HERE" output -raw instance_id 2>/dev/null
}

case "${1:-}" in
  start)
    id=$(instance_id)
    "${AWS[@]}" ec2 start-instances --instance-ids "$id" >/dev/null
    echo "starting $id"
    "${AWS[@]}" ec2 wait instance-running --instance-ids "$id"
    ip=$("$TOFU" -chdir="$HERE" output -raw address)
    echo "running at $ip; waiting for the API"
    for _ in $(seq 1 60); do
      if (exec 3<>"/dev/tcp/$ip/6443") 2>/dev/null; then echo "the cluster is up"; exit 0; fi
      sleep 5
    done
    echo "the API did not answer in five minutes; try venue.sh ssh and look at k3s" >&2
    exit 1
    ;;

  stop)
    id=$(instance_id)
    "${AWS[@]}" ec2 stop-instances --instance-ids "$id" >/dev/null
    echo "stopping $id — the disk stays, the hourly charge does not"
    ;;

  status)
    id=$(instance_id)
    state=$("${AWS[@]}" ec2 describe-instances --instance-ids "$id" \
      --query 'Reservations[0].Instances[0].State.Name' --output text)
    kind=$("${AWS[@]}" ec2 describe-instances --instance-ids "$id" \
      --query 'Reservations[0].Instances[0].InstanceType' --output text)
    echo "instance $id ($kind): $state"
    case "$state" in
      running) echo "billing for compute and disk" ;;
      stopped) echo "billing for the disk only" ;;
    esac
    echo "this month so far:"
    "${AWS[@]}" ce get-cost-and-usage \
      --time-period "Start=$(date -u +%Y-%m-01),End=$(date -u +%Y-%m-%d)" \
      --granularity MONTHLY --metrics UnblendedCost \
      --query 'ResultsByTime[0].Total.UnblendedCost.[Amount,Unit]' --output text 2>/dev/null \
      | awk '{printf "  %.2f %s\n", $1, $2}' || echo "  (cost explorer has not answered; it lags a day)"
    ;;

  ssh)
    ip=$("$TOFU" -chdir="$HERE" output -raw address)
    shift || true
    exec ssh "ubuntu@$ip" "$@"
    ;;

  kubeconfig)
    ip=$("$TOFU" -chdir="$HERE" output -raw address)
    mkdir -p "$HOME/.kube"
    # k3s writes 127.0.0.1 into its kubeconfig, which is true on the box and useless anywhere else. The address is
    # rewritten here rather than on the instance: this end already knows it, and an instance working out its own
    # public address needs the metadata service, a token, and to be right about both.
    ssh "ubuntu@$ip" "sudo cat /etc/rancher/k3s/k3s.yaml" \
      | sed "s#https://127.0.0.1:6443#https://$ip:6443#" > "$HOME/.kube/venue-aws"
    chmod 600 "$HOME/.kube/venue-aws"
    echo "written to ~/.kube/venue-aws"
    echo "use it with: export KUBECONFIG=~/.kube/venue-aws"
    ;;

  install)
    : "${KUBECONFIG:?set KUBECONFIG to ~/.kube/venue-aws}"
    # Which cluster this lands in is decided by one environment variable, and the wrong answer is somebody's running
    # venue rather than an error. So it is checked rather than assumed: the API this kubectl talks to must be the
    # address OpenTofu made.
    ip=$("$TOFU" -chdir="$HERE" output -raw address)
    server=$(kubectl config view --minify -o jsonpath='{.clusters[0].cluster.server}')
    case "$server" in
      *"$ip"*) echo "installing into $server" ;;
      *) echo "refusing: KUBECONFIG points at $server, which is not the box at $ip" >&2; exit 1 ;;
    esac
    kubectl cluster-info >/dev/null
    echo "== ArgoCD"
    kubectl create namespace argocd --dry-run=client -o yaml | kubectl apply -f - >/dev/null
    # --server-side: the ApplicationSet CRD is larger than the annotation a client-side apply writes.
    kubectl apply --server-side -n argocd \
      -f https://raw.githubusercontent.com/argoproj/argo-cd/stable/manifests/install.yaml >/dev/null
    echo "== cert-manager"
    kubectl apply --server-side \
      -f https://github.com/cert-manager/cert-manager/releases/latest/download/cert-manager.yaml >/dev/null
    echo "== certificate issuers"
    kubectl apply -f "$HERE/clusterissuer.yaml" >/dev/null
    echo "== the preview machinery"
    # The committed ApplicationSet names a placeholder domain rather than anybody's real one, so it is put in here,
    # from the same value the DNS was built with. One source for the name, and it never reaches the repository.
    domain=$("$TOFU" -chdir="$HERE" output -raw domain)
    argocd="$HERE/../argocd"
    kubectl apply -f "$argocd/project.yaml" >/dev/null
    sed "s/venue\\.example/$domain/" "$argocd/applicationset.yaml" | kubectl apply -f - >/dev/null
    kubectl apply -f "$argocd/reaper.yaml" >/dev/null
    echo "   previews will answer on pr-<number>.$domain"
    echo
    echo "still to do by hand, because each needs a secret this script will not invent:"
    echo "  - venue-github-token  (the ApplicationSet listing pull requests, and the reaper)"
    echo "  - venue-repo          (ArgoCD reading the repository)"
    echo "  - ghcr-pull           (pulling a preview's own image, in each preview namespace)"
    echo
    echo "until those exist the ApplicationSet will log that it cannot reach GitHub, which is expected."
    ;;

  destroy)
    echo "this destroys the instance, its disk and the DNS records."
    read -r -p "type the word destroy to go ahead: " answer
    [ "$answer" = "destroy" ] || { echo "left alone"; exit 1; }
    "$TOFU" -chdir="$HERE" destroy
    ;;

  *) usage; exit 1 ;;
esac

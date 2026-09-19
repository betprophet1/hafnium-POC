# A venue on AWS, cheaply

One EC2 instance running k3s, with a real domain and real certificates — the two things a laptop cluster cannot
give you, and the two that the preview environments were never tested against (`deploy/README.md`, an environment
per pull request).

It is deliberately not EKS. EKS bills about $73 a month for its control plane whether or not anything is deployed,
and none of what is being proved here needs it: ArgoCD, the ApplicationSet, the reaper, the chart, DNS and TLS all
behave the same on one node. What a single node cannot show you is autoscaling, scale-to-zero and multi-node
scheduling — so when the question changes from *does this work* to *does this scale*, this is the wrong shape and
EKS is the right one.

## What it costs

The instance is billed by the hour while it runs, and its disk is billed whether it runs or not. So the lever that
matters is stopping it:

| | |
|---|---|
| Running all month | roughly $50 |
| Started when you work on it, say 40 hours a month | roughly $8 |
| Stopped entirely | about $4, which is the disk |

`./venue.sh stop` and `./venue.sh start` are the whole cost control. The Elastic IP keeps the address across a stop,
so the DNS record does not move and nothing has to be reconfigured.

**x86, not Graviton.** ARM instances are about 20% cheaper per hour, which on a box that is stopped most of the time
is pennies a month — and the cost of it is cross-building arm64 images under emulation in GitHub Actions, which for
a Rust workspace this size is slow enough to notice on every pull request. The saving is not worth the build time
here. On a fleet running continuously, that arithmetic reverses.

## What you need first

- An AWS account, ideally one of its own. Creating this needs permission to make IAM roles, VPCs and load balancers,
  which is close enough to administrator that a mistake or a leaked key reaches everything else in the account.
- Credentials in a profile, never on a command line. Best is IAM Identity Center —
  `aws configure sso --profile venue --use-device-code` — which leaves nothing long-lived in `~/.aws`; a plain
  access key through `aws configure --profile venue` also works.
- A domain, either registered in Route53 or delegated to it.

## The order things happen in

1. **A budget alarm, before anything that bills.** In an organisation, make it once in the console of the account
   the box lives in — that keeps billing permissions off this key entirely. `create_budget = true` makes it from
   here instead, where the key does have them.
2. The rest: `tofu apply`
3. `./venue.sh install` — k3s, ArgoCD, cert-manager, and the venue chart
4. `./venue.sh stop` when you are done for the day

Teardown is `tofu destroy`, which takes the disk with it.

variable "region" {
  description = "Where it runs. Frankfurt is nearest most of Europe; the choice barely moves the price."
  type        = string
  default     = "eu-central-1"
}

variable "profile" {
  description = "The AWS profile to use, so credentials stay in ~/.aws and never on a command line."
  type        = string
  default     = "venue"
}

variable "monthly_budget" {
  description = "What the alarm is set at, in dollars. Low on purpose: the point is to hear about a mistake in days rather than at the end of the month."
  type        = string
  default     = "25"
}

variable "create_budget" {
  description = <<-EOT
    Whether the budget alarm is made here. False by default: in an organisation it is usually set once in the
    account that owns the others, or by hand in the console, and this key then needs no billing permissions at all.
  EOT
  type        = bool
  default     = false
}

variable "alert_email" {
  description = "Where the budget alarm goes, when one is made here. An alarm nobody receives is not an alarm."
  type        = string
  default     = ""
}

variable "instance_type" {
  description = <<-EOT
    x86 rather than Graviton, deliberately. ARM is about 20% cheaper an hour, which on a box that is stopped most of
    the time is pennies, and it would cost a cross-build under emulation on every pull request.

      t3.large    2 vCPU,  8 GiB  — ArgoCD and one preview, tight
      t3.xlarge   4 vCPU, 16 GiB  — ArgoCD and two or three previews
  EOT
  type        = string
  default     = "t3.large"
}

variable "disk_gb" {
  description = "Root volume. Images are the bulk of it: the venue image is not small and every preview pulls its own."
  type        = number
  default     = 60
}

variable "ssh_public_key" {
  description = "The public half of the key this machine reaches the box with. Registered with EC2; the private half never leaves here."
  type        = string
  default     = "~/.ssh/id_ed25519.pub"
}

variable "admin_cidrs" {
  description = <<-EOT
    Who may reach SSH and the Kubernetes API. Your address, with a /32 — not 0.0.0.0/0. The API is the whole
    cluster, and k3s protects it with a token whose safety is whatever the network in front of it is.
  EOT
  type        = list(string)
}

variable "zone_domain" {
  description = <<-EOT
    The domain Route53 hosts. Usually the whole thing you own, delegated by pointing the registrar's nameservers
    here, which then lets every name under it be managed in one place.
  EOT
  type        = string
}

variable "domain" {
  description = <<-EOT
    The name the venue itself answers on, which must be at or below `zone_domain`. Previews are its subdomains:
    a pull request is `pr-<number>.<domain>`.
  EOT
  type        = string
}

variable "create_zone" {
  description = "True to have Route53 host `zone_domain` here; false when the zone already exists and its id is given."
  type        = bool
  default     = true
}

variable "zone_id" {
  description = "An existing Route53 zone to put the records in, when create_zone is false."
  type        = string
  default     = ""
}

variable "preview_node" {
  description = <<-EOT
    Whether a second machine is made for preview environments to run on.

    False by default, and worth understanding before turning on. With it, previews run where the live venue is not:
    a preview asking for the launch engine cannot take CPU from the venue people are looking at, which is the whole
    reason for it. Without it, everything shares one box, and on a t3.large one small preview is already most of it.

    The node is tainted when it joins, so nothing lands there that has not asked to, and `preview.yaml` is what asks.

    What it does not do is keep previews off the server: `preview.yaml` carries a toleration and deliberately no
    nodeSelector, so with this node stopped, or never made, a preview runs wherever there is room — which is on top
    of the live venue. That is the documented choice (see the comment in preview.yaml: a nodeSelector would leave
    every preview Pending with no event anybody would think to look for), but it does mean turning this off is not
    a way to stop previews running. Not labelling a pull request is.
  EOT
  type        = bool
  default     = false
}

variable "preview_instance_type" {
  description = <<-EOT
    The preview node. Bigger than the server, because this is where the environments actually run:

      t3.large    2 vCPU,  8 GiB  — two small previews
      t3.xlarge   4 vCPU, 16 GiB  — four or five, or one asking for the launch engine

    The smaller by default: the budget alarm here is set at $25 a month and an xlarge left running is over four
    times that on its own. Raise it for an afternoon of throughput work and put it back.
  EOT
  type        = string
  default     = "t3.large"
}

variable "preview_disk_gb" {
  description = "Root volume on the preview node. Every preview pulls its own image, so this is mostly images."
  type        = number
  default     = 80
}

variable "archive" {
  description = <<-EOT
    Whether the archive bucket is made: where closed audit files go when they leave a node's disk
    (interfaces/audit/README.md, interface 29). Without it there is nowhere to put them and they stay until the
    volume is full, which stops the gateway that writes them.

    **Which of them are immutable is not settled here.** The bucket is made able to lock objects and locks none:
    Object Lock without a default retention leaves every object deletable until something asks for otherwise, and
    the shipper asks per object (`audit_ship.py --retain-years`). So one bucket holds a production environment's
    records under a five-year compliance lock and a preview's under a ninety-day expiry, and turning the first on
    is a flag on a job rather than a new bucket and a copy somebody has to verify.
  EOT
  type        = bool
  default     = false
}

variable "archive_expire_days" {
  description = <<-EOT
    How long an archived object is kept when the archive is *not* immutable, 0 to keep it for ever. This is the
    whole point of a deletable archive: a demo's audit logs are evidence of nothing and should cost nothing a
    month later.
  EOT
  type        = number
  default     = 90
}

variable "archive_glacier_days" {
  description = <<-EOT
    Days before an archived object moves to Glacier Instant Retrieval, 0 to leave everything in standard storage.

    Instant Retrieval and not the cheaper classes: §12 requires a regulatory record to be readily accessible for
    the whole retention, and a class with a multi-hour restore is not that. It is about a fifth the price of
    standard and reads back in milliseconds.
  EOT
  type        = number
  default     = 30
}

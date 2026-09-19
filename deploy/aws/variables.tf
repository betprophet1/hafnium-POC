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

variable "alert_email" {
  description = "Where the budget alarm goes. Required: an alarm nobody receives is not an alarm."
  type        = string
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

variable "key_name" {
  description = "An existing EC2 key pair to reach the box with."
  type        = string
}

variable "admin_cidrs" {
  description = <<-EOT
    Who may reach SSH and the Kubernetes API. Your address, with a /32 — not 0.0.0.0/0. The API is the whole
    cluster, and k3s protects it with a token whose safety is whatever the network in front of it is.
  EOT
  type        = list(string)
}

variable "domain" {
  description = "The name the venue answers on, with previews beneath it at *.preview.<domain>."
  type        = string
}

variable "create_zone" {
  description = "True to have Route53 host the zone here; false when the zone already exists and its id is given."
  type        = bool
  default     = true
}

variable "zone_id" {
  description = "An existing Route53 zone to put the records in, when create_zone is false."
  type        = string
  default     = ""
}

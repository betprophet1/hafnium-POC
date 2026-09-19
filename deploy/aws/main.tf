# One instance, one address, one DNS zone, and a budget that shouts before the bill does (deploy/aws/README.md).
#
# Everything here is deliberately small. A proof of concept that is expensive to run is one that gets left running
# by accident or torn down before it has answered anything, and neither is useful.

terraform {
  required_version = ">= 1.6"
  required_providers {
    aws = {
      source  = "hashicorp/aws"
      version = "~> 5.70"
    }
  }
}

provider "aws" {
  region  = var.region
  profile = var.profile
  default_tags {
    tags = {
      Project = "venue"
      Purpose = "preview environments, proof of concept"
      # So that everything made here can be found and destroyed together, including by hand if the state is lost.
      ManagedBy = "opentofu:deploy/aws"
    }
  }
}

# ---------------------------------------------------------------------------- the alarm, first
# Applied before anything else: `tofu apply -target=aws_budgets_budget.venue`. A budget cannot stop spending — only
# tell you about it — so it is worth having in place before the first thing that costs money, not after.
resource "aws_budgets_budget" "venue" {
  name         = "venue-preview-poc"
  budget_type  = "COST"
  limit_amount = var.monthly_budget
  limit_unit   = "USD"
  time_unit    = "MONTHLY"

  # Eighty per cent of what was actually spent, and a forecast that says the month is heading over. The forecast is
  # the useful one: it fires while there is still time to do something.
  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 80
    threshold_type             = "PERCENTAGE"
    notification_type          = "ACTUAL"
    subscriber_email_addresses = [var.alert_email]
  }
  notification {
    comparison_operator        = "GREATER_THAN"
    threshold                  = 100
    threshold_type             = "PERCENTAGE"
    notification_type          = "FORECASTED"
    subscriber_email_addresses = [var.alert_email]
  }
}

# ---------------------------------------------------------------------------- network
# The account's default VPC, on purpose. A VPC of our own would mean subnets, a route table, an internet gateway and
# — if anything private needed to reach out — a NAT gateway at about $32 a month, which would be the largest line on
# the bill for a single-node proof of concept. One instance with a public address needs none of it.
data "aws_vpc" "default" {
  default = true
}

data "aws_subnets" "default" {
  filter {
    name   = "vpc-id"
    values = [data.aws_vpc.default.id]
  }
}

resource "aws_security_group" "venue" {
  name        = "venue-preview"
  description = "One k3s node serving preview environments"
  vpc_id      = data.aws_vpc.default.id

  # The web, from anywhere: previews are meant to be looked at, and Let's Encrypt has to reach :80 to answer an
  # HTTP-01 challenge.
  ingress {
    description = "http"
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }
  ingress {
    description = "https"
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  # SSH and the Kubernetes API, from where you are and nowhere else. The API especially: it is the whole cluster,
  # and k3s serves it with a token that is only as good as the network in front of it.
  ingress {
    description = "ssh"
    from_port   = 22
    to_port     = 22
    protocol    = "tcp"
    cidr_blocks = var.admin_cidrs
  }
  ingress {
    description = "kubernetes api"
    from_port   = 6443
    to_port     = 6443
    protocol    = "tcp"
    cidr_blocks = var.admin_cidrs
  }

  egress {
    description = "anywhere"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }
}

# ---------------------------------------------------------------------------- the instance
data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"] # Canonical
  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*"]
  }
}

resource "aws_instance" "venue" {
  ami                    = data.aws_ami.ubuntu.id
  instance_type          = var.instance_type
  subnet_id              = data.aws_subnets.default.ids[0]
  vpc_security_group_ids = [aws_security_group.venue.id]
  key_name               = var.key_name

  root_block_device {
    volume_size = var.disk_gb
    volume_type = "gp3"
    encrypted   = true
    # The disk goes when the instance does. This is a proof of concept: anything worth keeping is in git or in the
    # registry, and a volume left behind is a bill nobody is watching.
    delete_on_termination = true
  }

  user_data = file("${path.module}/cloud-init.yaml")

  # So that `venue.sh stop` and a later `start` do not rebuild the machine.
  lifecycle {
    ignore_changes = [ami]
  }

  tags = { Name = "venue-preview" }
}

# An address that survives a stop, so the DNS record does not have to be rewritten every morning.
resource "aws_eip" "venue" {
  instance = aws_instance.venue.id
  domain   = "vpc"
  tags     = { Name = "venue-preview" }
}

# ---------------------------------------------------------------------------- names
resource "aws_route53_zone" "venue" {
  count = var.create_zone ? 1 : 0
  name  = var.domain
}

locals {
  zone_id = var.create_zone ? aws_route53_zone.venue[0].zone_id : var.zone_id
}

# The venue itself, and every preview under it. The wildcard is what lets a pull request have a name without anyone
# creating a record for it.
resource "aws_route53_record" "apex" {
  zone_id = local.zone_id
  name    = var.domain
  type    = "A"
  ttl     = 60
  records = [aws_eip.venue.public_ip]
}

resource "aws_route53_record" "previews" {
  zone_id = local.zone_id
  name    = "*.preview.${var.domain}"
  type    = "A"
  ttl     = 60
  records = [aws_eip.venue.public_ip]
}

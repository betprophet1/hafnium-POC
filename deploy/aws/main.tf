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
      # No comma: budget tags allow only letters, numbers, spaces and _ . : / = + - @, and reject the rest.
      Purpose = "preview environments - proof of concept"
      # So that everything made here can be found and destroyed together, including by hand if the state is lost.
      ManagedBy = "opentofu:deploy/aws"
    }
  }
}

# ---------------------------------------------------------------------------- the alarm, first
# Applied before anything else: `tofu apply -target=aws_budgets_budget.venue`. A budget cannot stop spending — only
# tell you about it — so it is worth having in place before the first thing that costs money, not after.
resource "aws_budgets_budget" "venue" {
  # Off by default. In an organisation the budget is usually made once, by hand or in the account that owns the
  # others, and giving this key billing permissions it does not otherwise need is the wrong trade. Turn it on with
  # `create_budget = true` where the key does have them.
  count        = var.create_budget ? 1 : 0
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

  # Between the machines in this group, and nowhere else. A second node has to reach the API to join, the kubelets
  # have to reach each other for logs and exec, and flannel carries the pod network over VXLAN. `self` is what keeps
  # this to the cluster's own machines: a CIDR here would mean anything in the default VPC.
  ingress {
    description = "kubernetes api between nodes"
    from_port   = 6443
    to_port     = 6443
    protocol    = "tcp"
    self        = true
  }
  ingress {
    description = "kubelet"
    from_port   = 10250
    to_port     = 10250
    protocol    = "tcp"
    self        = true
  }
  ingress {
    description = "flannel vxlan"
    from_port   = 8472
    to_port     = 8472
    protocol    = "udp"
    self        = true
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
# The key this machine already has, registered with EC2 so there is no key pair to create, download or lose. Only
# the public half ever leaves here.
resource "aws_key_pair" "venue" {
  key_name   = "venue-preview"
  public_key = file(pathexpand(var.ssh_public_key))
}

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
  key_name               = aws_key_pair.venue.key_name
  # So the shipper needs no keys of its own. Associating a profile is an in-place change: the instance keeps
  # running, and without an archive there is no profile and nothing to associate.
  iam_instance_profile = var.archive ? aws_iam_instance_profile.archive[0].name : null

  root_block_device {
    volume_size = var.disk_gb
    volume_type = "gp3"
    encrypted   = true
    # The disk goes when the instance does. This is a proof of concept: anything worth keeping is in git or in the
    # registry, and a volume left behind is a bill nobody is watching.
    delete_on_termination = true
  }

  user_data = templatefile("${path.module}/cloud-init.yaml", {
    public_ip = aws_eip.venue.public_ip
    domain    = var.domain
  })

  lifecycle {
    # `venue.sh stop` and a later `start` must not rebuild the machine, and neither should a change to the
    # cloud-init: it only ever runs on a first boot, so applying one to a running box would mean destroying it to
    # no purpose. A deliberate rebuild is `tofu taint aws_instance.venue`.
    ignore_changes = [ami, user_data]
  }

  tags = { Name = "venue-preview" }
}

# An address that survives a stop, so the DNS record does not have to be rewritten every morning. Allocated on its
# own rather than as a property of the instance, because the instance has to be told this address at boot — k3s
# signs its API certificate for the addresses it is given, and one it learns about later is one the certificate
# does not cover.
resource "aws_eip" "venue" {
  domain = "vpc"
  tags   = { Name = "venue-preview" }
}

resource "aws_eip_association" "venue" {
  instance_id   = aws_instance.venue.id
  allocation_id = aws_eip.venue.id
}

# ---------------------------------------------------------------------------- names
# The zone is the domain that was bought; the venue is one name inside it. Keeping them apart is what lets previews
# be siblings of the venue rather than another label deeper.
resource "aws_route53_zone" "venue" {
  count = var.create_zone ? 1 : 0
  name  = var.zone_domain
}

locals {
  zone_id = var.create_zone ? aws_route53_zone.venue[0].zone_id : var.zone_id
}

# The venue itself, and every preview beside it. The wildcard is what lets a pull request have a name without anyone
# creating a record for it, and previews sit one label below the venue — `pr-7.pmex.example` next to `pmex.example`
# rather than under a `preview.` that carries no information `pr-7` does not.
#
# That is a choice about names and nothing more. A DNS wildcard matches a name with one label below it or several
# (RFC 4592), so `*.pmex.example` answers for `a.b.pmex.example` too, and a deeper preview would have resolved
# perfectly well. Certificates are where depth bites — an X.509 wildcard really does cover exactly one label — but
# previews take a certificate per host over HTTP-01, so that does not bind here either.
resource "aws_route53_record" "venue" {
  zone_id = local.zone_id
  name    = var.domain
  type    = "A"
  ttl     = 60
  records = [aws_eip.venue.public_ip]
}

resource "aws_route53_record" "previews" {
  zone_id = local.zone_id
  name    = "*.${var.domain}"
  type    = "A"
  ttl     = 60
  records = [aws_eip.venue.public_ip]
}

# ---------------------------------------------------------------- the preview node
# A second machine for preview environments, so one asking for a large engine cannot take CPU from the venue people
# are looking at. It joins as a k3s agent and is tainted, so only a pod that tolerates the taint lands on it.
#
# It carries no cloud-init on purpose. An agent needs the server's node token to join, and the only ways to get it
# in here would be to write it into user_data — which the instance metadata then hands to anything running on the
# box — or to read it in Terraform, which writes it into the state file. `venue.sh previews join` reads it over SSH
# at the moment it is needed and it goes no further than that command.
resource "aws_instance" "preview" {
  count                  = var.preview_node ? 1 : 0
  ami                    = data.aws_ami.ubuntu.id
  instance_type          = var.preview_instance_type
  subnet_id              = data.aws_subnets.default.ids[0]
  vpc_security_group_ids = [aws_security_group.venue.id]
  key_name               = aws_key_pair.venue.key_name
  # Previews write audit logs too, and a preview node fills up faster than the server does.
  iam_instance_profile = var.archive ? aws_iam_instance_profile.archive[0].name : null

  root_block_device {
    volume_size = var.preview_disk_gb
    volume_type = "gp3"
    encrypted   = true
    # Nothing on this machine outlives it: the environments are rebuilt from their pull requests.
    delete_on_termination = true
  }

  lifecycle {
    ignore_changes = [ami]
  }

  tags = { Name = "venue-preview-node" }
}

# ── The archive ───────────────────────────────────────────────────────────────────────────────────────────────
#
# Where closed audit files go when they leave a node's disk (interface 29). `interfaces/tools/audit_ship.py` puts
# them here and removes them from the volume that a gateway would otherwise fill.
#
# Deletable by default. `archive_immutable` is the other choice and is deliberately one flag, because Object Lock
# is fixed when a bucket is made: turning it on makes a new bucket rather than changing this one.

resource "aws_s3_bucket" "archive" {
  count  = var.archive ? 1 : 0
  bucket = "${replace(var.domain, ".", "-")}-venue-archive"

  # Fixed at creation and nowhere else. `true` here is what makes the lock configuration below possible at all;
  # it does not by itself lock anything, which the retention rule does.
  object_lock_enabled = var.archive_immutable
}

# Object Lock requires it, and without the lock it is what stops a bad upload overwriting a good record.
resource "aws_s3_bucket_versioning" "archive" {
  count  = var.archive ? 1 : 0
  bucket = aws_s3_bucket.archive[0].id
  versioning_configuration {
    status = "Enabled"
  }
}

resource "aws_s3_bucket_public_access_block" "archive" {
  count                   = var.archive ? 1 : 0
  bucket                  = aws_s3_bucket.archive[0].id
  block_public_acls       = true
  block_public_policy     = true
  ignore_public_acls      = true
  restrict_public_buckets = true
}

resource "aws_s3_bucket_server_side_encryption_configuration" "archive" {
  count  = var.archive ? 1 : 0
  bucket = aws_s3_bucket.archive[0].id
  rule {
    apply_server_side_encryption_by_default {
      sse_algorithm = "AES256"
    }
  }
}

# The bucket's own default, so an object is locked whether or not whoever wrote it remembered to ask. The tool can
# set a longer retention on a particular object; neither it nor anyone else can set a shorter one.
resource "aws_s3_bucket_object_lock_configuration" "archive" {
  count  = var.archive && var.archive_immutable ? 1 : 0
  bucket = aws_s3_bucket.archive[0].id
  rule {
    default_retention {
      mode = "COMPLIANCE"
      days = var.archive_retain_days
    }
  }
}

resource "aws_s3_bucket_lifecycle_configuration" "archive" {
  count      = var.archive ? 1 : 0
  bucket     = aws_s3_bucket.archive[0].id
  depends_on = [aws_s3_bucket_versioning.archive]

  rule {
    id     = "cool-then-expire"
    status = "Enabled"
    filter {}

    dynamic "transition" {
      for_each = var.archive_glacier_days > 0 ? [1] : []
      content {
        days          = var.archive_glacier_days
        storage_class = "GLACIER_IR"
      }
    }

    # Only where objects may be deleted at all. Under compliance-mode Object Lock a rule like this is refused for
    # anything still within its retention, which is the lock doing its job.
    dynamic "expiration" {
      for_each = !var.archive_immutable && var.archive_expire_days > 0 ? [1] : []
      content {
        days = var.archive_expire_days
      }
    }

    # The manifest is uploaded whole after every run, so without this its old versions accumulate for ever. Under
    # a lock they are evidence and are kept.
    dynamic "noncurrent_version_expiration" {
      for_each = var.archive_immutable ? [] : [1]
      content {
        noncurrent_days = 30
      }
    }
  }
}

# What the nodes may do with it: put an object, read one back to check it arrived, and list. Never delete — the
# tool removes files from the node's disk, never from the archive, and an instance that cannot delete is one that
# cannot be made to.
resource "aws_iam_role" "archive" {
  count = var.archive ? 1 : 0
  name  = "${replace(var.domain, ".", "-")}-venue-archive"
  assume_role_policy = jsonencode({
    Version   = "2012-10-17"
    Statement = [{ Effect = "Allow", Principal = { Service = "ec2.amazonaws.com" }, Action = "sts:AssumeRole" }]
  })
}

resource "aws_iam_role_policy" "archive" {
  count = var.archive ? 1 : 0
  name  = "archive-write"
  role  = aws_iam_role.archive[0].id
  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect   = "Allow"
        Action   = ["s3:PutObject", "s3:PutObjectRetention", "s3:GetObject", "s3:GetObjectVersion"]
        Resource = "${aws_s3_bucket.archive[0].arn}/*"
      },
      {
        Effect   = "Allow"
        Action   = ["s3:ListBucket", "s3:GetBucketObjectLockConfiguration"]
        Resource = aws_s3_bucket.archive[0].arn
      },
    ]
  })
}

resource "aws_iam_instance_profile" "archive" {
  count = var.archive ? 1 : 0
  name  = "${replace(var.domain, ".", "-")}-venue-archive"
  role  = aws_iam_role.archive[0].name
}

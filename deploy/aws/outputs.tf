output "address" {
  description = "Where the box is. Kept across a stop, so the DNS records do not move."
  value       = aws_eip.venue.public_ip
}

output "ssh" {
  description = "How to reach it."
  value       = "ssh ubuntu@${aws_eip.venue.public_ip}"
}

output "instance_id" {
  description = "What venue.sh stops and starts."
  value       = aws_instance.venue.id
}

output "nameservers" {
  description = <<-EOT
    The nameservers this zone answers on. When the domain is registered elsewhere, these have to be set at the
    registrar before any name here resolves — the commonest reason a fresh setup looks broken when it is not.
  EOT
  value       = var.create_zone ? aws_route53_zone.venue[0].name_servers : []
}

output "profile" {
  description = "The AWS profile this deployment was made with, so the scripts need not be told it twice."
  value       = var.profile
}

output "domain" {
  description = "The name this deployment answers on, which the ArgoCD manifests are pointed at."
  value       = var.domain
}

output "previews" {
  description = "What a pull request's environment will be called."
  value       = "pr-<number>.preview.${var.domain}"
}

output "region" {
  description = "Where it runs, so the scripts need not be told that twice either."
  value       = var.region
}

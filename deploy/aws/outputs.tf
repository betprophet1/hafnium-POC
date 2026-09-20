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
    The nameservers this zone answers on. For a whole domain these replace whatever the registrar has under its
    nameserver or "custom DNS" setting; for a delegated label they go in as NS records at the parent. Either way
    nothing here resolves until they are in place, which is the commonest reason a fresh setup looks broken.
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
  value       = "pr-<number>.${var.domain}"
}

output "region" {
  description = "Where it runs, so the scripts need not be told that twice either."
  value       = var.region
}

output "preview_node" {
  description = "The preview node's address, when there is one. Empty means previews share the server's machine."
  value       = var.preview_node ? aws_instance.preview[0].public_ip : ""
}

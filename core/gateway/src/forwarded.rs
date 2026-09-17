//! Who a client is when it reaches the gateway through a proxy (interfaces/audit/README.md, gateway).
//!
//! Behind an ingress every connection comes from the ingress, so the address a record's `peer` gives is the proxy's.
//! A proxy says whom it forwarded for in `X-Forwarded-For` (each proxy appends the address it received the request
//! from) or `Forwarded` (RFC 7239, `for=`). Anyone can write those headers, so they are read only on a connection from
//! an address the gateway was told to trust (`--trusted-proxies`, none by default), and then only from the right:
//! entries appended by trusted proxies are skipped, and the first one that is not a trusted proxy is the client. Every
//! entry left of it may have been written by the client itself. Nothing here decides anything but what the audit log
//! says; flood and logon limits are per account and session, never per address.

use std::net::{IpAddr, SocketAddr};

/// The most of an entry that is not an address (`unknown`, an obfuscated identifier) a record keeps.
const MAX_OPAQUE: usize = 64;

/// An address or a network: `192.0.2.7`, `10.42.0.0/16`, `2001:db8::/32`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Network {
    base: IpAddr,
    prefix: u8,
}

impl Network {
    fn parse(text: &str) -> Result<Self, String> {
        let (address, prefix) = match text.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (text, None),
        };
        let base: IpAddr =
            address.parse().map_err(|_| format!("{text} is not an address or a network (such as 10.42.0.0/16)"))?;
        let base = base.to_canonical();
        let bits = if base.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            None => bits,
            Some(p) => match p.parse::<u8>() {
                Ok(p) if p <= bits => p,
                _ => return Err(format!("{text}: the prefix length must be 0 to {bits}")),
            },
        };
        Ok(Self { base, prefix })
    }

    fn contains(self, ip: IpAddr) -> bool {
        match (self.base, ip.to_canonical()) {
            (IpAddr::V4(base), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - u32::from(self.prefix)).unwrap_or(0);
                u32::from(base) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(base), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - u32::from(self.prefix)).unwrap_or(0);
                u128::from(base) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// The proxies whose forwarding headers are believed. Empty trusts none, and every header is ignored.
#[derive(Clone, Debug, Default)]
pub struct TrustedProxies(Vec<Network>);

impl TrustedProxies {
    /// A comma-separated list of addresses and networks, as `--trusted-proxies` gives it.
    pub fn parse(list: &str) -> Result<Self, String> {
        list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(Network::parse).collect::<Result<_, _>>().map(Self)
    }

    fn trusts(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|network| network.contains(ip))
    }

    /// Whether a connection from `peer` may say whom it forwards for.
    pub fn trusts_peer(&self, peer: Option<IpAddr>) -> bool {
        peer.is_some_and(|ip| self.trusts(ip))
    }

    /// The client a request from `peer` was made by, as its forwarding headers say, or `None` when `peer` is not a
    /// trusted proxy or the request names no one. `X-Forwarded-For` is read if the request has it, `Forwarded`
    /// otherwise: a proxy that writes only `Forwarded` must remove an `X-Forwarded-For` a client sent.
    pub fn client(&self, peer: Option<IpAddr>, headers: &Forwarding) -> Option<String> {
        if !self.trusts_peer(peer) {
            return None;
        }
        let hops: Vec<Hop> = match (&headers.x_forwarded_for, &headers.forwarded) {
            (Some(list), _) if list.split(',').any(|e| !e.trim().is_empty()) => {
                list.split(',').map(str::trim).filter(|e| !e.is_empty()).map(Hop::parse).collect()
            }
            (_, Some(forwarded)) => forwarded_for(forwarded),
            _ => return None,
        };
        // The right-most entry that is not a trusted proxy; if every one is, the left-most, where the chain began.
        hops.iter()
            .rev()
            .find(|hop| !hop.ip.is_some_and(|ip| self.trusts(ip)))
            .or_else(|| hops.first())
            .map(Hop::recorded)
    }
}

/// A request's forwarding headers, each with every value it was sent, in order and joined by commas.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Forwarding {
    pub x_forwarded_for: Option<String>,
    pub forwarded: Option<String>,
}

/// One entry of a forwarding header: an address, or something that stands for one (`unknown`, `_hidden`).
struct Hop {
    text: String,
    ip: Option<IpAddr>,
}

impl Hop {
    fn parse(entry: &str) -> Self {
        let text = entry.trim().trim_matches('"');
        // `[2001:db8::1]:4711`, `[2001:db8::1]`, `192.0.2.1:4711`, or a bare address.
        let ip = match text.strip_prefix('[').and_then(|rest| rest.split_once(']')) {
            Some((inside, _)) => inside.parse::<IpAddr>().ok(),
            None => text.parse::<IpAddr>().ok().or_else(|| text.parse::<SocketAddr>().ok().map(|a| a.ip())),
        };
        Self { text: text.to_string(), ip: ip.map(|ip| ip.to_canonical()) }
    }

    /// What a record says the client was: the address alone, or what the header gave, cut short.
    fn recorded(&self) -> String {
        match self.ip {
            Some(ip) => ip.to_string(),
            None => self.text.chars().take(MAX_OPAQUE).collect(),
        }
    }
}

/// The `for=` of each element of a `Forwarded` header, in order; an element without one stands for an unknown hop.
fn forwarded_for(header: &str) -> Vec<Hop> {
    header
        .split(',')
        .filter(|element| !element.trim().is_empty())
        .map(|element| {
            let value = element.split(';').find_map(|pair| {
                let (name, value) = pair.split_once('=')?;
                name.trim().eq_ignore_ascii_case("for").then_some(value)
            });
            Hop::parse(value.unwrap_or("unknown"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> Option<IpAddr> {
        Some(text.parse().expect("an address"))
    }

    fn xff(list: &str) -> Forwarding {
        Forwarding { x_forwarded_for: Some(list.to_string()), forwarded: None }
    }

    #[test]
    fn networks_and_addresses_are_parsed_and_matched() {
        let trusted = TrustedProxies::parse(" 10.42.0.0/16, 192.0.2.7 ,2001:db8::/32,").expect("parsed");
        assert!(trusted.trusts_peer(ip("10.42.3.4")));
        assert!(trusted.trusts_peer(ip("::ffff:10.42.0.1")), "an IPv4-mapped address is its IPv4 address");
        assert!(!trusted.trusts_peer(ip("10.43.0.1")));
        assert!(trusted.trusts_peer(ip("192.0.2.7")));
        assert!(!trusted.trusts_peer(ip("192.0.2.8")));
        assert!(trusted.trusts_peer(ip("2001:db8:1::1")));
        assert!(!trusted.trusts_peer(None));
        assert!(TrustedProxies::parse("0.0.0.0/0").expect("parsed").trusts_peer(ip("203.0.113.1")));
        assert!(!TrustedProxies::parse("").expect("empty is none").trusts_peer(ip("127.0.0.1")));
        for bad in ["10.42.0.0/33", "ten", "10.42.0.0/x", "::/129"] {
            assert!(TrustedProxies::parse(bad).is_err(), "{bad} was taken");
        }
    }

    #[test]
    fn a_trusted_proxy_names_the_client() {
        let trusted = TrustedProxies::parse("10.42.0.0/16").expect("parsed");
        assert_eq!(trusted.client(ip("10.42.0.9"), &xff("198.51.100.77")).as_deref(), Some("198.51.100.77"));
        assert_eq!(trusted.client(ip("10.42.0.9"), &Forwarding::default()), None, "no header names no client");
        let forwarded =
            Forwarding { x_forwarded_for: None, forwarded: Some(r#"for="[2001:db8::7]:4711";proto=http"#.into()) };
        assert_eq!(trusted.client(ip("10.42.0.9"), &forwarded).as_deref(), Some("2001:db8::7"));
    }

    #[test]
    fn an_untrusted_peer_cannot_name_a_client() {
        let trusted = TrustedProxies::parse("10.42.0.0/16").expect("parsed");
        let forged =
            Forwarding { x_forwarded_for: Some("198.51.100.1".into()), forwarded: Some("for=198.51.100.2".into()) };
        assert_eq!(trusted.client(ip("203.0.113.5"), &forged), None);
        assert_eq!(TrustedProxies::default().client(ip("10.42.0.9"), &forged), None, "nothing is trusted by default");
    }

    #[test]
    fn across_several_hops_the_right_most_untrusted_entry_is_the_client() {
        let trusted = TrustedProxies::parse("10.42.0.0/16, 172.18.0.2").expect("parsed");
        let peer = ip("10.42.0.9");
        // What the client wrote comes first; each trusted proxy appended the address it was reached from.
        assert_eq!(trusted.client(peer, &xff("1.2.3.4, 198.51.100.77, 172.18.0.2")).as_deref(), Some("198.51.100.77"));
        assert_eq!(
            trusted.client(peer, &xff("1.2.3.4,198.51.100.77:5123,10.42.0.3")).as_deref(),
            Some("198.51.100.77")
        );
        assert_eq!(
            trusted.client(peer, &xff("10.42.0.1, 172.18.0.2")).as_deref(),
            Some("10.42.0.1"),
            "all trusted: the left-most"
        );
        assert_eq!(trusted.client(peer, &xff("unknown, 172.18.0.2")).as_deref(), Some("unknown"));
        assert_eq!(trusted.client(peer, &xff(" , ")), None);
        let forwarded = Forwarding {
            x_forwarded_for: None,
            forwarded: Some("for=198.51.100.1, for=192.0.2.60;proto=http, For=172.18.0.2".into()),
        };
        assert_eq!(trusted.client(peer, &forwarded).as_deref(), Some("192.0.2.60"));
        // X-Forwarded-For wins over Forwarded when both are sent.
        let both = Forwarding { x_forwarded_for: Some("192.0.2.1".into()), forwarded: Some("for=192.0.2.2".into()) };
        assert_eq!(trusted.client(peer, &both).as_deref(), Some("192.0.2.1"));
        let long = "x".repeat(500);
        assert_eq!(trusted.client(peer, &xff(&long)).map(|c| c.len()), Some(MAX_OPAQUE));
    }
}

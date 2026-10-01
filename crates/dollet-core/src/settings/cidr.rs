//! Minimal CIDR containment, for the `network_access` allowlists.
//!
//! Hand-rolled rather than pulling in a crate: this is the only place the
//! project needs prefix matching, and the whole thing is a mask comparison.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

#[derive(Debug, thiserror::Error)]
#[error("not a CIDR block: {0}")]
pub struct CidrError(String);

impl FromStr for Cidr {
    type Err = CidrError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = match s.split_once('/') {
            Some((addr, len)) => {
                let prefix: u8 = len.parse().map_err(|_| CidrError(s.to_owned()))?;
                (addr, Some(prefix))
            }
            // A bare address is a host route. The UI accepts both.
            None => (s, None),
        };

        let network: IpAddr = addr.parse().map_err(|_| CidrError(s.to_owned()))?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        if prefix > max {
            return Err(CidrError(s.to_owned()));
        }

        Ok(Self { network, prefix })
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.network, self.prefix)
    }
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.network, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                same_prefix(&net.octets(), &ip.octets(), self.prefix)
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                same_prefix(&net.octets(), &ip.octets(), self.prefix)
            }
            // An IPv4-mapped client (::ffff:10.0.0.1) reaching an IPv4 rule is
            // common behind a dual-stack listener, so unwrap before giving up.
            (IpAddr::V4(_), IpAddr::V6(ip)) => match ip.to_ipv4_mapped() {
                Some(v4) => self.contains(IpAddr::V4(v4)),
                None => false,
            },
            (IpAddr::V6(_), IpAddr::V4(_)) => false,
        }
    }
}

/// Loopback plus the RFC1918 / ULA ranges a reverse proxy realistically sits in.
///
/// Used to decide whether a peer may be believed about `X-Forwarded-For`, which
/// is the difference between an allowlist and a suggestion.
pub fn is_loopback_or_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                // 100.64.0.0/10, carrier-grade NAT and what Tailscale hands out.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_loopback_or_private(IpAddr::V4(v4)),
            None => {
                v6.is_loopback()
                    // fc00::/7 unique-local and fe80::/10 link-local.
                    || (v6.segments()[0] & 0xfe00) == 0xfc00
                    || (v6.segments()[0] & 0xffc0) == 0xfe80
            }
        },
    }
}

fn same_prefix(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let whole = (prefix / 8) as usize;
    if a[..whole] != b[..whole] {
        return false;
    }
    let bits = prefix % 8;
    if bits == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - bits);
    a[whole] & mask == b[whole] & mask
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn matches_on_byte_and_sub_byte_boundaries() {
        let slash8: Cidr = "10.0.0.0/8".parse().unwrap();
        assert!(slash8.contains(ip("10.255.1.2")));
        assert!(!slash8.contains(ip("11.0.0.1")));

        let slash20: Cidr = "172.16.16.0/20".parse().unwrap();
        assert!(slash20.contains(ip("172.16.31.255")));
        assert!(!slash20.contains(ip("172.16.32.0")));
    }

    #[test]
    fn a_bare_address_is_a_host_route() {
        let host: Cidr = "192.168.1.5".parse().unwrap();
        assert!(host.contains(ip("192.168.1.5")));
        assert!(!host.contains(ip("192.168.1.6")));
    }

    #[test]
    fn slash_zero_matches_everything_of_its_family() {
        let all: Cidr = "0.0.0.0/0".parse().unwrap();
        assert!(all.contains(ip("203.0.113.1")));
        assert!(!all.contains(ip("2001:db8::1")));
    }

    #[test]
    fn ipv6_and_mapped_clients() {
        let v6: Cidr = "2001:db8::/32".parse().unwrap();
        assert!(v6.contains(ip("2001:db8:1234::1")));
        assert!(!v6.contains(ip("2001:db9::1")));

        let v4: Cidr = "10.0.0.0/8".parse().unwrap();
        assert!(v4.contains(ip("::ffff:10.1.2.3")));
        assert!(!v4.contains(ip("::1")));
    }

    #[test]
    fn private_and_loopback_are_recognised_including_mapped_form() {
        for addr in [
            "127.0.0.1",
            "10.1.2.3",
            "192.168.1.1",
            "172.16.0.1",
            "100.64.0.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:10.0.0.1",
        ] {
            assert!(is_loopback_or_private(ip(addr)), "{addr} not recognised");
        }

        for addr in [
            "8.8.8.8",
            "203.0.113.9",
            "2606:4700::1111",
            "::ffff:8.8.8.8",
        ] {
            assert!(!is_loopback_or_private(ip(addr)), "{addr} treated as local");
        }
    }

    #[test]
    fn rejects_nonsense() {
        for bad in [
            "",
            "not-a-cidr",
            "10.0.0.0/33",
            "10.0.0.0/x",
            "2001:db8::/200",
        ] {
            assert!(bad.parse::<Cidr>().is_err(), "{bad} parsed");
        }
    }
}

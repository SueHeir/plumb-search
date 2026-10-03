//! Name resolution that keeps the crawler off private networks.
//!
//! Plumb is meant to run on home servers, so a hostile domain whose DNS
//! points at 192.168.1.1, 127.0.0.1 or the like must not get the crawler to
//! fetch pages on the operator's own network (request forgery through DNS).
//! [`Resolver`] drops every address that is not globally routable before
//! reqwest connects, and since reqwest connects only to the addresses it
//! returns, a second DNS answer cannot slip a private address past the
//! check. Hosts written as IP addresses are not looked up and so are not
//! checked; the crawler only requests one when a target URL names it,
//! because a redirect to another site (which an IP address always is, for a
//! domain's homepage) is never followed. Names sent through a proxy are not
//! checked either, since the proxy looks them up, which is why the crawler
//! connects directly unless `CrawlConfig::use_system_proxy` is set.

use std::error::Error;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use thiserror::Error;

type BoxError = Box<dyn Error + Send + Sync>;

/// IPv4 ranges that are not globally routable, after IANA's special-purpose
/// address registry.
const NON_GLOBAL_V4: &[(Ipv4Addr, u8)] = &[
    (Ipv4Addr::new(0, 0, 0, 0), 8),       // "this network", incl. 0.0.0.0
    (Ipv4Addr::new(10, 0, 0, 0), 8),      // private
    (Ipv4Addr::new(100, 64, 0, 0), 10),   // shared address space (CGNAT)
    (Ipv4Addr::new(127, 0, 0, 0), 8),     // loopback
    (Ipv4Addr::new(169, 254, 0, 0), 16),  // link-local, incl. cloud metadata
    (Ipv4Addr::new(172, 16, 0, 0), 12),   // private
    (Ipv4Addr::new(192, 0, 0, 0), 24),    // IETF protocol assignments
    (Ipv4Addr::new(192, 0, 2, 0), 24),    // documentation (TEST-NET-1)
    (Ipv4Addr::new(192, 88, 99, 0), 24),  // 6to4 relay anycast (deprecated)
    (Ipv4Addr::new(192, 168, 0, 0), 16),  // private
    (Ipv4Addr::new(198, 18, 0, 0), 15),   // benchmarking
    (Ipv4Addr::new(198, 51, 100, 0), 24), // documentation (TEST-NET-2)
    (Ipv4Addr::new(203, 0, 113, 0), 24),  // documentation (TEST-NET-3)
    (Ipv4Addr::new(224, 0, 0, 0), 4),     // multicast
    (Ipv4Addr::new(240, 0, 0, 0), 4),     // reserved, incl. broadcast
];

/// All globally routable IPv6 unicast lives here; the rest of the space is
/// loopback, unspecified, unique-local, link-local, multicast, discard,
/// IPv4-compatible and other special ranges.
const GLOBAL_UNICAST_V6: (Ipv6Addr, u8) = (Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0), 3);

/// Ranges inside [`GLOBAL_UNICAST_V6`] that are not globally routable.
const NON_GLOBAL_V6: &[(Ipv6Addr, u8)] = &[
    // IETF protocol assignments: Teredo, benchmarking, ORCHID and others.
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
    // Documentation.
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
];

/// Resolves host names with Tokio. Unless `allow_private` is set it keeps
/// only globally routable addresses ([`is_global`]), and when none is left
/// the lookup fails with [`NonPublicHost`], so the request fails instead of
/// connecting.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Resolver {
    pub(crate) allow_private: bool,
}

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(lookup(name.as_str().to_string(), self.allow_private))
    }
}

async fn lookup(host: String, allow_private: bool) -> Result<Addrs, BoxError> {
    // Port 0 stands for "the URL's port", which reqwest fills in.
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
    if allow_private {
        return Ok(Box::new(resolved.into_iter()));
    }
    let (public, mut refused): (Vec<SocketAddr>, Vec<SocketAddr>) =
        resolved.into_iter().partition(|addr| is_global(addr.ip()));
    if public.is_empty() && !refused.is_empty() {
        refused.sort_unstable();
        refused.dedup();
        let addrs = refused.iter().map(SocketAddr::ip).collect();
        return Err(NonPublicHost { host, addrs }.into());
    }
    Ok(Box::new(public.into_iter()))
}

/// A host that resolved only to addresses the crawler does not visit.
#[derive(Debug, Error)]
#[error(
    "{host} resolves only to non-public addresses ({}), which are not \
     crawled unless allow_private_addresses is set",
    list(.addrs)
)]
pub(crate) struct NonPublicHost {
    host: String,
    addrs: Vec<IpAddr>,
}

fn list(addrs: &[IpAddr]) -> String {
    let addrs: Vec<String> = addrs.iter().map(IpAddr::to_string).collect();
    addrs.join(", ")
}

/// Whether connecting to `ip` stays on the public internet. False for
/// loopback, private, link-local, shared (CGNAT), documentation,
/// benchmarking, multicast, broadcast and other reserved IPv4 ranges, for
/// IPv6 outside global unicast or in its special ranges (so unique-local
/// `fc00::/7`, link-local `fe80::/10` and the deprecated IPv4-compatible
/// `::a.b.c.d` are out too), and for IPv6 addresses standing for an IPv4
/// address (IPv4-mapped, NAT64, 6to4) that is not global itself.
pub(crate) fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_global_v4(ip),
        IpAddr::V6(ip) => is_global_v6(ip),
    }
}

fn is_global_v4(ip: Ipv4Addr) -> bool {
    !NON_GLOBAL_V4.iter().any(|&range| in_v4_range(ip, range))
}

fn is_global_v6(ip: Ipv6Addr) -> bool {
    if let Some(v4) = embedded_v4(ip) {
        return is_global_v4(v4);
    }
    in_v6_range(ip, GLOBAL_UNICAST_V6) && !NON_GLOBAL_V6.iter().any(|&range| in_v6_range(ip, range))
}

/// The IPv4 address an IPv6 address stands for: IPv4-mapped
/// `::ffff:a.b.c.d`, NAT64 `64:ff9b::a.b.c.d` (RFC 6052) or 6to4
/// `2002:aabb:ccdd::/48` (RFC 3056). Connecting to one of these reaches,
/// or is routed towards, that IPv4 address.
fn embedded_v4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let v4 = |high: u16, low: u16| Ipv4Addr::from((u32::from(high) << 16) | u32::from(low));
    match ip.segments() {
        [0, 0, 0, 0, 0, 0xffff, high, low] | [0x64, 0xff9b, 0, 0, 0, 0, high, low] => {
            Some(v4(high, low))
        }
        [0x2002, high, low, ..] => Some(v4(high, low)),
        _ => None,
    }
}

fn in_v4_range(ip: Ipv4Addr, (network, prefix): (Ipv4Addr, u8)) -> bool {
    let mask = u32::MAX.checked_shl(32 - u32::from(prefix)).unwrap_or(0);
    (u32::from(ip) ^ u32::from(network)) & mask == 0
}

fn in_v6_range(ip: Ipv6Addr, (network, prefix): (Ipv6Addr, u8)) -> bool {
    let mask = u128::MAX.checked_shl(128 - u32::from(prefix)).unwrap_or(0);
    (u128::from(ip) ^ u128::from(network)) & mask == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(addrs: &[&str], global: bool) {
        for addr in addrs {
            let ip: IpAddr = addr.parse().unwrap();
            assert_eq!(is_global(ip), global, "{addr}");
        }
    }

    #[test]
    fn public_ipv4_is_global() {
        check(
            &[
                "1.1.1.1",
                "8.8.8.8",
                "93.184.215.14",
                "223.255.255.255",
                // Just outside the special ranges.
                "9.255.255.255",
                "11.0.0.0",
                "100.63.255.255",
                "100.128.0.0",
                "126.255.255.255",
                "128.0.0.0",
                "169.253.255.255",
                "169.255.0.0",
                "172.15.255.255",
                "172.32.0.0",
                "192.0.1.0",
                "192.0.3.0",
                "192.88.98.255",
                "192.167.255.255",
                "192.169.0.0",
                "198.17.255.255",
                "198.20.0.0",
                "198.51.99.255",
                "203.0.112.255",
            ],
            true,
        );
    }

    #[test]
    fn special_ipv4_is_not_global() {
        check(
            &[
                "0.0.0.0",
                "0.1.2.3",
                "10.0.0.1",
                "10.255.255.255",
                "100.64.0.1",
                "100.127.255.255",
                "127.0.0.1",
                "127.255.255.254",
                "169.254.169.254",
                "172.16.0.1",
                "172.31.255.255",
                "192.0.0.8",
                "192.0.2.1",
                "192.88.99.1",
                "192.168.1.1",
                "198.18.0.1",
                "198.19.255.255",
                "198.51.100.7",
                "203.0.113.9",
                "224.0.0.1",
                "239.255.255.250",
                "240.0.0.1",
                "255.255.255.255",
            ],
            false,
        );
    }

    #[test]
    fn public_ipv6_is_global() {
        check(
            &[
                "2606:4700:4700::1111",
                "2001:4860:4860::8888",
                "2a00:1450:4001:82a::200e",
                "2001:200::1",
                "3fff:1000::1",
                // Standing for a public IPv4 address.
                "::ffff:8.8.8.8",
                "64:ff9b::808:808",
                "2002:808:808::1",
            ],
            true,
        );
    }

    #[test]
    fn special_ipv6_is_not_global() {
        check(
            &[
                "::",
                "::1",
                "fc00::1",
                "fd12:3456:789a::1",
                "fe80::1",
                "febf::1",
                "fec0::1",
                "ff02::1",
                "ff0e::1",
                "100::1",
                "5f00::1",
                "2001::1",
                "2001:2::1",
                "2001:1ff::1",
                "2001:db8::1",
                "3fff::1",
                // IPv4-mapped, IPv4-compatible, NAT64 and 6to4 forms of
                // non-public IPv4 addresses.
                "::ffff:127.0.0.1",
                "::ffff:192.168.1.1",
                "::ffff:10.0.0.1",
                "::ffff:0.0.0.0",
                "::127.0.0.1",
                "::192.168.1.1",
                "::8.8.8.8",
                "64:ff9b::7f00:1",
                "64:ff9b::c0a8:101",
                "2002:c0a8:101::1",
                "2002:7f00:1::1",
            ],
            false,
        );
    }

    #[tokio::test]
    async fn lookups_drop_non_public_addresses() {
        // "localhost" resolves from the hosts file, without the network.
        let err = lookup("localhost".into(), false).await.err().unwrap();
        let message = err.to_string();
        assert!(
            message.starts_with("localhost resolves only to non-public addresses (")
                && message.contains("allow_private_addresses"),
            "{message}"
        );

        let addrs: Vec<SocketAddr> = lookup("localhost".into(), true).await.unwrap().collect();
        assert!(
            addrs.iter().any(|addr| addr.ip().is_loopback()),
            "{addrs:?}"
        );
    }
}

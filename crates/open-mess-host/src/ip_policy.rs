//! Which addresses a module may reach: globally routable unicast only (spec 8.3, R9).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddressPolicy {
    /// Globally routable unicast addresses only.
    Strict,
    /// Strict, plus loopback so tests can run a local server. The daemon cannot construct it.
    #[cfg(any(test, feature = "test-support"))]
    AllowLoopbackForTests,
}

pub fn is_allowed(ip: IpAddr, policy: AddressPolicy) -> bool {
    match policy {
        AddressPolicy::Strict => is_global(ip),
        #[cfg(any(test, feature = "test-support"))]
        AddressPolicy::AllowLoopbackForTests => ip.is_loopback() || is_global(ip),
    }
}

/// IPv4 ranges that are not globally reachable unicast: the IANA special-purpose registry, plus
/// multicast and the reserved 240/4 (which holds the broadcast address).
const V4_DENIED: &[(Ipv4Addr, u32)] = &[
    (Ipv4Addr::new(0, 0, 0, 0), 8),
    (Ipv4Addr::new(10, 0, 0, 0), 8),
    (Ipv4Addr::new(100, 64, 0, 0), 10),
    (Ipv4Addr::new(127, 0, 0, 0), 8),
    (Ipv4Addr::new(169, 254, 0, 0), 16),
    (Ipv4Addr::new(172, 16, 0, 0), 12),
    (Ipv4Addr::new(192, 0, 0, 0), 24),
    (Ipv4Addr::new(192, 0, 2, 0), 24),
    (Ipv4Addr::new(192, 88, 99, 0), 24),
    (Ipv4Addr::new(192, 168, 0, 0), 16),
    (Ipv4Addr::new(198, 18, 0, 0), 15),
    (Ipv4Addr::new(198, 51, 100, 0), 24),
    (Ipv4Addr::new(203, 0, 113, 0), 24),
    (Ipv4Addr::new(224, 0, 0, 0), 4),
    (Ipv4Addr::new(240, 0, 0, 0), 4),
];

/// Special-purpose parts of IPv6 global unicast (2000::/3): IETF protocol assignments (Teredo,
/// benchmarking, ORCHID), documentation, and 6to4, which can tunnel to any IPv4 address.
const V6_DENIED: &[(Ipv6Addr, u32)] = &[
    (Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
    (Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    (Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    (Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
];

const NAT64: Ipv6Addr = Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0);
const GLOBAL_UNICAST: Ipv6Addr = Ipv6Addr::new(0x2000, 0, 0, 0, 0, 0, 0, 0);

fn in_v4(ip: Ipv4Addr, net: Ipv4Addr, len: u32) -> bool {
    let mask = u32::MAX.checked_shl(32 - len).unwrap_or(0);
    u32::from(ip) & mask == u32::from(net)
}

fn in_v6(ip: Ipv6Addr, net: Ipv6Addr, len: u32) -> bool {
    let mask = u128::MAX.checked_shl(128 - len).unwrap_or(0);
    u128::from(ip) & mask == u128::from(net)
}

fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !V4_DENIED.iter().any(|&(net, len)| in_v4(v4, net, len)),
        IpAddr::V6(v6) => {
            // Addresses that carry an IPv4 address are judged by it: IPv4-mapped, IPv4-compatible
            // (this covers :: and ::1) and NAT64.
            if let Some(v4) = v6.to_ipv4() {
                return is_global(IpAddr::V4(v4));
            }
            if in_v6(v6, NAT64, 96) {
                return is_global(IpAddr::V4(Ipv4Addr::from(u128::from(v6) as u32)));
            }
            in_v6(v6, GLOBAL_UNICAST, 3) && !V6_DENIED.iter().any(|&(net, len)| in_v6(v6, net, len))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_special_ranges() {
        for ip in [
            "127.0.0.1",
            "0.0.0.0",
            "10.1.2.3",
            "172.16.0.1",
            "192.168.1.1",
            "100.64.1.1",
            "169.254.1.1",
            "224.0.0.1",
            "255.255.255.255",
            "192.0.2.1",
            "198.18.0.1",
            "::1",
            "::",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "::ffff:192.168.1.1",
            "64:ff9b::10.0.0.1",
            // More of the IANA special-purpose registry.
            "192.0.0.8",
            "198.51.100.7",
            "203.0.113.9",
            "240.0.0.1",
            "2001:db8::1",
            "2001::1",
            "2002:c0a8:101::1",
            "3fff::1",
            "fec0::1",
            "::127.0.0.1",
        ] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(!is_allowed(ip, AddressPolicy::Strict), "{ip} should be blocked");
        }
    }

    #[test]
    fn allows_public_unicast() {
        for ip in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111", "::ffff:8.8.8.8", "64:ff9b::8.8.8.8"] {
            let ip: IpAddr = ip.parse().unwrap();
            assert!(is_allowed(ip, AddressPolicy::Strict), "{ip} should be allowed");
        }
    }

    #[test]
    fn test_policy_adds_loopback_only() {
        assert!(is_allowed("127.0.0.1".parse().unwrap(), AddressPolicy::AllowLoopbackForTests));
        assert!(is_allowed("::1".parse().unwrap(), AddressPolicy::AllowLoopbackForTests));
        assert!(!is_allowed("192.168.1.1".parse().unwrap(), AddressPolicy::AllowLoopbackForTests));
    }
}

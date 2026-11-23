use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

/// True for loopback, RFC 1918, IPv4 link-local, IPv6 ULA (fc00::/7) and
/// IPv6 link-local (fe80::/10). The app is LAN-only, so anything else is refused
/// before a handshake is even attempted.
pub fn is_allowed_ip(ip: IpAddr) -> bool {
    // An IPv4-mapped IPv6 address arrives on dual-stack sockets and must be
    // judged by its IPv4 meaning, not as a global v6 address.
    match ip.to_canonical() {
        IpAddr::V4(v4) => is_allowed_v4(v4),
        IpAddr::V6(v6) => is_allowed_v6(v6),
    }
}

pub fn is_allowed_addr(addr: SocketAddr) -> bool {
    is_allowed_ip(addr.ip())
}

fn is_allowed_v4(ip: Ipv4Addr) -> bool {
    ip.is_loopback() || ip.is_private() || ip.is_link_local()
}

fn is_allowed_v6(ip: Ipv6Addr) -> bool {
    let first = ip.segments()[0];
    ip.is_loopback() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> bool {
        is_allowed_ip(s.parse().unwrap())
    }

    #[test]
    fn allows_lan_ranges() {
        for s in [
            "127.0.0.1",
            "127.8.9.10",
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.254",
            "192.168.1.20",
            "169.254.3.4",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "febf::1",
            "::ffff:192.168.0.5",
        ] {
            assert!(ok(s), "{s} should be allowed");
        }
    }

    #[test]
    fn rejects_public_and_odd_ranges() {
        for s in [
            "8.8.8.8",
            "172.15.255.255",
            "172.32.0.1",
            "192.169.0.1",
            "100.64.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "224.0.0.1",
            "2001:4860::8888",
            "fec0::1",
            "ff02::1",
            "::",
            "::ffff:8.8.8.8",
        ] {
            assert!(!ok(s), "{s} should be rejected");
        }
    }
}

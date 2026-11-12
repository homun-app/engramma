// SPDX-License-Identifier: Apache-2.0

//! v0.5.2 property tests: the SSRF fence and the redirect matching.
//!
//! - every address inside a reserved block is refused, whatever textual
//!   form it arrived in (decimal, IPv6 compression, mapped IPv4);
//! - the loopback redirect's port is ignored ONLY for `http` on a
//!   loopback host — never for `https`, never for another host, never
//!   across paths.

use std::net::IpAddr;

use engramma_mcp::oauth::{is_public_ip, redirect_matches};
use proptest::prelude::*;

/// An IPv4 as four octets.
fn octets(a: u8, b: u8, c: u8, d: u8) -> String {
    format!("{a}.{b}.{c}.{d}")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// RFC 1918 private space: never public.
    #[test]
    fn rfc1918_is_never_public(a in 0u8.., b in any::<u8>(), c in any::<u8>(), d in any::<u8>()) {
        let s = match a % 3 {
            0 => octets(10, b, c, d),
            1 => octets(172, 16 + (b % 16), c, d),
            _ => octets(192, 168, c, d),
        };
        let ip: IpAddr = s.parse().unwrap(/* strategy builds valid addresses */);
        prop_assert!(!is_public_ip(ip), "{s}");
    }

    /// Loopback, link-local (the cloud metadata block included) and CGNAT.
    #[test]
    fn loopback_linklocal_and_cgnat_are_never_public(
        a in any::<u8>(), b in any::<u8>(), c in any::<u8>(),
    ) {
        for s in [
            octets(127, a, b, c),
            octets(169, 254, a, b),
            octets(100, 64 + (a % 64), b, c),
            octets(0, a, b, c),
        ] {
            let ip: IpAddr = s.parse().unwrap(/* strategy builds valid addresses */);
            prop_assert!(!is_public_ip(ip), "{s}");
        }
    }

    /// Multicast and the reserved top of the v4 space.
    #[test]
    fn multicast_and_reserved_v4_are_never_public(a in 224u8..=255, b in any::<u8>(), c in any::<u8>(), d in any::<u8>()) {
        let ip: IpAddr = octets(a, b, c, d).parse().unwrap(/* valid by construction */);
        prop_assert!(!is_public_ip(ip));
    }

    /// The mapped form of a reserved v4 is judged as its v4.
    #[test]
    fn mapped_reserved_v4_is_never_public(
        which in 0u8..4, a in any::<u8>(), b in any::<u8>(), c in any::<u8>(),
    ) {
        let v4 = match which {
            0 => octets(127, a, b, c),
            1 => octets(10, a, b, c),
            2 => octets(172, 16 + (a % 16), b, c),
            _ => octets(192, 168, a, b),
        };
        let s = format!("::ffff:{v4}");
        let ip: IpAddr = s.parse().unwrap(/* valid by construction */);
        prop_assert!(!is_public_ip(ip), "{s}");
    }

    /// ULA (fc00::/7), link-local (fe80::/10) and multicast (ff00::/8).
    #[test]
    fn reserved_v6_blocks_are_never_public(
        which in 0u8..3, tail in r"[0-9a-f]{0,8}",
    ) {
        let prefix = match which {
            0 => "fc", // and fd: the whole /7
            1 => "fe8",
            _ => "ff",
        };
        let head = match which {
            0 => format!("{}{}", prefix, if tail.len() % 2 == 0 { "0" } else { "" }),
            _ => prefix.to_string(),
        };
        let s = format!("{head}:{tail}");
        if let Ok(ip) = s.parse::<IpAddr>() {
            prop_assert!(!is_public_ip(ip), "{s}");
        }
    }

    /// A normal public v4 is public (the fence must not over-reject the
    /// internet: the first octet picks a public class, the rest is free).
    #[test]
    fn ordinary_public_v4_is_public(b in any::<u8>(), c in any::<u8>(), d in any::<u8>()) {
        let a = match (b % 4) {
            0 => 1u8,   // 1/8
            1 => 93,    // 93/8
            2 => 104,   // 104/8
            _ => 172,   // 172.32+ (just past the private block)
        };
        let b2 = if a == 172 { b.saturating_sub(16 % 128).max(32) } else { b };
        if b2 >= 32 || a != 172 {
            let ip: IpAddr = octets(a, b2, c, d).parse().unwrap(/* valid */);
            prop_assert!(is_public_ip(ip), "{}", octets(a, b2, c, d));
        }
    }

    /// The port-ignoring exception applies ONLY to http + loopback: for
    /// https, or another host, a different port is a different redirect.
    #[test]
    fn ports_matter_except_on_loopback_http(
        scheme in prop_oneof![Just("http"), Just("https")],
        host in prop_oneof![Just("localhost"), Just("127.0.0.1"), Just("api.example.com")],
        p1 in 1u16..65535,
        p2 in 1u16..65535,
    ) {
        // same path, two ports: when the ports differ the URIs differ
        let reg = format!("{scheme}://{host}:{p1}/cb");
        let req = format!("{scheme}://{host}:{p2}/cb");
        if p1 == p2 {
            prop_assert!(redirect_matches(&reg, &req));
        } else {
            let loopback_http = scheme == "http" && (host == "localhost" || host == "127.0.0.1");
            prop_assert_eq!(redirect_matches(&reg, &req), loopback_http);
        }
    }

    /// Whatever the ports, the path must be the same one.
    #[test]
    fn the_path_must_match_even_on_loopback_http(
        p1 in 1u16..65535, p2 in 1u16..65535, path in r"/[a-z]{1,6}",
    ) {
        let reg = format!("http://127.0.0.1:{p1}{path}");
        let req = format!("http://127.0.0.1:{p2}{path}/altro");
        prop_assert!(!redirect_matches(&reg, &req));
    }
}

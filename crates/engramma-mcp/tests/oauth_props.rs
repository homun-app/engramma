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
// v0.6.14 (N3): `current` is a trait method of the value trees
use proptest::strategy::ValueTree as _;

/// v0.6.14 (N3): a runner seeded by the property's OWN seed. The body of
/// every property takes that one seed and derives its inputs from it
/// through the same strategies the property always drew — so a failing
/// seed replays in ONE case (`scripts/seme-sul-tag.sh <proprietà> <seme>
/// <tag>`), on any tree that holds the body.
fn seeded_runner(seed: u64) -> proptest::test_runner::TestRunner {
    // ChaCha (the default) wants a 32-byte seed: the u64 is laid four times
    let block = seed.to_le_bytes();
    let mut bytes = [0_u8; 32];
    for k in 0..4 {
        bytes[k * 8..k * 8 + 8].copy_from_slice(&block);
    }
    proptest::test_runner::TestRunner::new_with_rng(
        proptest::test_runner::Config::default(),
        proptest::test_runner::TestRng::from_seed(
            proptest::test_runner::RngAlgorithm::default(),
            &bytes,
        ),
    )
}

/// An IPv4 as four octets.
fn octets(a: u8, b: u8, c: u8, d: u8) -> String {
    format!("{a}.{b}.{c}.{d}")
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(512))]

    /// RFC 1918 private space: never public.
    #[test]
    fn rfc1918_is_never_public(seed in any::<u64>()) {
        rfc1918_is_never_public_body(seed)?;
    }

    /// Loopback, link-local (the cloud metadata block included) and CGNAT.
    #[test]
    fn loopback_linklocal_and_cgnat_are_never_public(seed in any::<u64>()) {
        loopback_linklocal_and_cgnat_are_never_public_body(seed)?;
    }

    /// Multicast and the reserved top of the v4 space.
    #[test]
    fn multicast_and_reserved_v4_are_never_public(seed in any::<u64>()) {
        multicast_and_reserved_v4_are_never_public_body(seed)?;
    }

    /// The mapped form of a reserved v4 is judged as its v4.
    #[test]
    fn mapped_reserved_v4_is_never_public(seed in any::<u64>()) {
        mapped_reserved_v4_is_never_public_body(seed)?;
    }

    /// ULA (fc00::/7), link-local (fe80::/10) and multicast (ff00::/8).
    #[test]
    fn reserved_v6_blocks_are_never_public(seed in any::<u64>()) {
        reserved_v6_blocks_are_never_public_body(seed)?;
    }

    /// A normal public v4 is public (the fence must not over-reject the
    /// internet: the first octet picks a public class, the rest is free).
    #[test]
    fn ordinary_public_v4_is_public(seed in any::<u64>()) {
        ordinary_public_v4_is_public_body(seed)?;
    }

    /// The port-ignoring exception applies ONLY to http + loopback: for
    /// https, or another host, a different port is a different redirect.
    #[test]
    /// v0.5.3 (P3.5): 6to4 is judged by the IPv4 it embeds — a reserved v4
    /// in a 6to4 coat is never public, a public one always is.
    #[test]
    fn sixto_four_is_the_ipv4_it_carries(seed in any::<u64>()) {
        sixto_four_is_the_ipv4_it_carries_body(seed)?;
    }

    /// v0.5.3 (P3.5): local NAT64 (RFC 8215), Teredo and the deprecated
    /// IPv4-compatible form are refused, whatever follows the prefix.
    #[test]
    fn nat64_teredo_and_ipv4_compatible_are_never_public(seed in any::<u64>()) {
        nat64_teredo_and_ipv4_compatible_are_never_public_body(seed)?;
    }
    /// Same path, two ports: when the ports differ the URIs differ, except
    /// over plain `http` on a loopback host. (v0.6.14, N3: this property
    /// had no `#[test]` — it never ran, and the hunts recorded it green.
    /// The inventory checks it now.)
    #[test]
    fn ports_matter_except_on_loopback_http(seed in any::<u64>()) {
        ports_matter_except_on_loopback_http_body(seed)?;
    }

    /// Whatever the ports, the path must be the same one.
    #[test]
    fn the_path_must_match_even_on_loopback_http(seed in any::<u64>()) {
        the_path_must_match_even_on_loopback_http_body(seed)?;
    }
}

/// v0.6.14 (N3): the body of `rfc1918_is_never_public` in ONE case — the seed
/// derives every input through the property's own strategies
/// (`seeded_runner`). The property in the `proptest!` block
/// hands its seed to this function; a failing seed replays here.
fn rfc1918_is_never_public_body(seed: u64) -> Result<(), proptest::test_runner::TestCaseError> {
    let mut runner = seeded_runner(seed);
    let (a, b, c, d) = ((0u8..), (any::<u8>()), (any::<u8>()), (any::<u8>()))
        .new_tree(&mut runner)
        .expect(/* t */ "the strategy builds its tree")
        .current();
    let s = match a % 3 {
        0 => octets(10, b, c, d),
        1 => octets(172, 16 + (b % 16), c, d),
        _ => octets(192, 168, c, d),
    };
    let ip: IpAddr = s.parse().unwrap(/* strategy builds valid addresses */);
    prop_assert!(!is_public_ip(ip), "{s}");
    Ok(())
}

/// v0.6.14 (N3): the body of `loopback_linklocal_and_cgnat_are_never_public` in ONE case — the seed
/// derives every input through the property's own strategies
/// (`seeded_runner`). The property in the `proptest!` block
/// hands its seed to this function; a failing seed replays here.
fn loopback_linklocal_and_cgnat_are_never_public_body(
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let mut runner = seeded_runner(seed);
    let (a, b, c) = ((any::<u8>()), (any::<u8>()), (any::<u8>()))
        .new_tree(&mut runner)
        .expect(/* t */ "the strategy builds its tree")
        .current();
    for s in [
        octets(127, a, b, c),
        octets(169, 254, a, b),
        octets(100, 64 + (a % 64), b, c),
        octets(0, a, b, c),
    ] {
        let ip: IpAddr = s.parse().unwrap(/* strategy builds valid addresses */);
        prop_assert!(!is_public_ip(ip), "{s}");
    }
    Ok(())
}

/// v0.6.14 (N3): the body of `multicast_and_reserved_v4_are_never_public` in ONE case — the seed
/// derives every input through the property's own strategies
/// (`seeded_runner`). The property in the `proptest!` block
/// hands its seed to this function; a failing seed replays here.
fn multicast_and_reserved_v4_are_never_public_body(
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let mut runner = seeded_runner(seed);
    let (a, b, c, d) = ((224u8..=255), (any::<u8>()), (any::<u8>()), (any::<u8>()))
        .new_tree(&mut runner)
        .expect(/* t */ "the strategy builds its tree")
        .current();
    let ip: IpAddr = octets(a, b, c, d).parse().unwrap(/* valid by construction */);
    prop_assert!(!is_public_ip(ip));
    Ok(())
}

/// v0.6.14 (N3): the body of `mapped_reserved_v4_is_never_public` in ONE case — the seed
/// derives every input through the property's own strategies
/// (`seeded_runner`). The property in the `proptest!` block
/// hands its seed to this function; a failing seed replays here.
fn mapped_reserved_v4_is_never_public_body(
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let mut runner = seeded_runner(seed);
    let (which, a, b, c) = ((0u8..4), (any::<u8>()), (any::<u8>()), (any::<u8>()))
        .new_tree(&mut runner)
        .expect(/* t */ "the strategy builds its tree")
        .current();
    let v4 = match which {
        0 => octets(127, a, b, c),
        1 => octets(10, a, b, c),
        2 => octets(172, 16 + (a % 16), b, c),
        _ => octets(192, 168, a, b),
    };
    let s = format!("::ffff:{v4}");
    let ip: IpAddr = s.parse().unwrap(/* valid by construction */);
    prop_assert!(!is_public_ip(ip), "{s}");
    Ok(())
}

/// v0.6.14 (N3): the body of `reserved_v6_blocks_are_never_public` in ONE case — the seed
/// derives every input through the property's own strategies
/// (`seeded_runner`). The property in the `proptest!` block
/// hands its seed to this function; a failing seed replays here.
fn reserved_v6_blocks_are_never_public_body(
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let mut runner = seeded_runner(seed);
    let (which, tail) = ((0u8..3), (r"[0-9a-f]{0,8}"))
        .new_tree(&mut runner)
        .expect(/* t */ "the strategy builds its tree")
        .current();
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
    };
    Ok(())
}

/// v0.6.14 (N3): the body of `ordinary_public_v4_is_public` in ONE case — the seed
/// derives every input through the property's own strategies
/// (`seeded_runner`). The property in the `proptest!` block
/// hands its seed to this function; a failing seed replays here.
fn ordinary_public_v4_is_public_body(
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let mut runner = seeded_runner(seed);
    let (b, c, d) = ((any::<u8>()), (any::<u8>()), (any::<u8>()))
        .new_tree(&mut runner)
        .expect(/* t */ "the strategy builds its tree")
        .current();
    let a = match b % 4 {
        0 => 1u8, // 1/8
        1 => 93,  // 93/8
        2 => 104, // 104/8
        _ => 200, // 200/8
    };
    let ip: IpAddr = octets(a, b, c, d).parse().unwrap(/* valid by construction */);
    prop_assert!(is_public_ip(ip), "{}", octets(a, b, c, d));
    Ok(())
}

/// v0.6.14 (N3): the body of `sixto_four_is_the_ipv4_it_carries` in ONE case — the seed
/// derives every input through the property's own strategies
/// (`seeded_runner`). The property in the `proptest!` block
/// hands its seed to this function; a failing seed replays here.
fn sixto_four_is_the_ipv4_it_carries_body(
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let mut runner = seeded_runner(seed);
    let (a, b, c, d) = ((any::<u8>()), (any::<u8>()), (any::<u8>()), (any::<u8>()))
        .new_tree(&mut runner)
        .expect(/* t */ "the strategy builds its tree")
        .current();
    let s = format!("2002:{a:02x}{b:02x}:{c:02x}{d:02x}::");
    let v4 = format!("{a}.{b}.{c}.{d}");
    prop_assert_eq!(
        is_public_ip(s.parse().unwrap(/* strategy: valid ip */)),
        is_public_ip(v4.parse().unwrap(/* strategy: valid ip */)),
        "{} vs {}",
        s,
        v4
    );
    Ok(())
}

/// v0.6.14 (N3): the body of `nat64_teredo_and_ipv4_compatible_are_never_public` in ONE case — the seed
/// derives every input through the property's own strategies
/// (`seeded_runner`). The property in the `proptest!` block
/// hands its seed to this function; a failing seed replays here.
fn nat64_teredo_and_ipv4_compatible_are_never_public_body(
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let mut runner = seeded_runner(seed);
    let (a, b, c, d, e) = (
        (any::<u8>()),
        (any::<u8>()),
        (any::<u8>()),
        (any::<u8>()),
        (any::<u8>()),
    )
        .new_tree(&mut runner)
        .expect(/* t */ "the strategy builds its tree")
        .current();
    for s in [
        format!("64:ff9b:1:{a:x}:{b:x}:{c:x}:{d:x}:{e:x}"),
        format!("2001:0:{a:x}:{b:x}:{c:x}:{d:x}:{e:x}:1"),
        format!("::{a:x}.{b:x}.{c:x}.{d:x}"),
    ] {
        let Ok(addr) = s.parse() else { continue };
        prop_assert!(!is_public_ip(addr), "{s} must not be public");
    }
    Ok(())
}

/// v0.6.14 (N3): the body of `ports_matter_except_on_loopback_http` in ONE case — the seed
/// derives every input through the property's own strategies
/// (`seeded_runner`). The property in the `proptest!` block
/// hands its seed to this function; a failing seed replays here.
fn ports_matter_except_on_loopback_http_body(
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let mut runner = seeded_runner(seed);
    let (scheme, host, p1, p2) = (
        (prop_oneof![Just("http"), Just("https")]),
        (prop_oneof![
            Just("localhost"),
            Just("127.0.0.1"),
            Just("api.example.com")
        ]),
        (1u16..65535),
        (1u16..65535),
    )
        .new_tree(&mut runner)
        .expect(/* t */ "the strategy builds its tree")
        .current();
    // same path, two ports: when the ports differ the URIs differ
    let reg = format!("{scheme}://{host}:{p1}/cb");
    let req = format!("{scheme}://{host}:{p2}/cb");
    if p1 == p2 {
        prop_assert!(redirect_matches(&reg, &req));
    } else {
        let loopback_http = scheme == "http" && (host == "localhost" || host == "127.0.0.1");
        prop_assert_eq!(redirect_matches(&reg, &req), loopback_http);
    };
    Ok(())
}

/// v0.6.14 (N3): the body of `the_path_must_match_even_on_loopback_http` in ONE case — the seed
/// derives every input through the property's own strategies
/// (`seeded_runner`). The property in the `proptest!` block
/// hands its seed to this function; a failing seed replays here.
fn the_path_must_match_even_on_loopback_http_body(
    seed: u64,
) -> Result<(), proptest::test_runner::TestCaseError> {
    let mut runner = seeded_runner(seed);
    let (p1, p2, path) = ((1u16..65535), (1u16..65535), (r"/[a-z]{1,6}"))
        .new_tree(&mut runner)
        .expect(/* t */ "the strategy builds its tree")
        .current();
    let reg = format!("http://127.0.0.1:{p1}{path}");
    let req = format!("http://127.0.0.1:{p2}{path}/altro");
    prop_assert!(!redirect_matches(&reg, &req));
    Ok(())
}

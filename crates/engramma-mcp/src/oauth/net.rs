// SPDX-License-Identifier: Apache-2.0

//! v0.5.2 (P2): the two pure nets of the OAuth surface.
//!
//! - [`is_public_ip`]: the SSRF fence of the CIMD fetch. A client_id is an
//!   `https://` URL chosen by WHOEVER CALLS `/authorize`: before downloading
//!   it, the DNS answer is filtered to public addresses only — loopback,
//!   RFC 1918, link-local (the cloud metadata service lives there), CGNAT,
//!   ULA, multicast, unspecified, IPv4-mapped IPv6 and the documentation
//!   blocks are all discarded, whatever textual form they came in.
//! - [`redirect_matches`] / [`redirect_uri_ok`]: which redirect URIs a
//!   client may register and how a request may match one — exact strings,
//!   with the ONE exception of RFC 8252 § 7.3: a loopback `http` redirect
//!   ignores the port (Claude Code binds an ephemeral one).

use std::net::IpAddr;

/// Whether an IP may be *connected to* by the CIMD fetch: only genuinely
/// public addresses pass. The blocks follow the phase spec (plus the
/// documentation ranges, which can never serve a real client_id document).
#[must_use]
pub fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(v4.octets()),
        IpAddr::V6(v6) => {
            let o = v6.octets();
            // an IPv4-mapped IPv6 address is its IPv4: same judgment
            if o[..10].iter().all(|b| *b == 0) && o[10] == 0xff && o[11] == 0xff {
                return is_public_v4([o[12], o[13], o[14], o[15]]);
            }
            // :: and ::1
            if o.iter().all(|b| *b == 0) {
                return false;
            }
            if o[..15].iter().all(|b| *b == 0) && o[15] == 1 {
                return false;
            }
            // fc00::/7 unique-local, fe80::/10 link-local, ff00::/8 multicast
            if (o[0] & 0xfe) == 0xfc {
                return false;
            }
            if (o[0] == 0xfe) && (o[1] & 0xc0) == 0x80 {
                return false;
            }
            if o[0] == 0xff {
                return false;
            }
            // 2001:db8::/32 documentation, 64:ff9b::/96 well-known NAT64
            if o[0] == 0x20 && o[1] == 0x01 && o[2] == 0x0d && o[3] == 0xb8 {
                return false;
            }
            if o[..8] == [0x00, 0x64, 0xff, 0x9b, 0, 0, 0, 0] {
                return false;
            }
            true
        }
    }
}

fn is_public_v4(o: [u8; 4]) -> bool {
    let (a, b) = (o[0], o[1]);
    match (a, b) {
        // 0.0.0.0/8 "this network" and unspecified
        (0, _) => false,
        // 10.0.0.0/8 private
        (10, _) => false,
        // 100.64.0.0/10 CGNAT
        (100, 64..=127) => false,
        // 127.0.0.0/8 loopback
        (127, _) => false,
        // 169.254.0.0/16 link-local (and the cloud metadata service)
        (169, 254) => false,
        // 172.16.0.0/12 private
        (172, 16..=31) => false,
        // 192.0.0.0/24 IETF assignments, 192.0.2.0/24 documentation
        (192, 0) => false,
        // 192.168.0.0/16 private
        (192, 168) => false,
        // 198.18.0.0/15 benchmarking, 198.51.100.0/24 documentation
        (198, 18..=19) | (198, 51) => false,
        // 203.0.113.0/24 documentation
        (203, 0) => false,
        // 224.0.0.0/4 multicast, 240.0.0.0/4 reserved (and the broadcast)
        (224..=255, _) => false,
        _ => true,
    }
}

/// One parsed redirect URI, as a (scheme, host, port, path-and-query) tuple
/// the matching rules can look at without a URL crate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedirectUri {
    /// `https` or `http`, lowercase.
    pub scheme: String,
    /// The host, lowercase (no userinfo is ever accepted).
    pub host: String,
    /// The explicit port, when the string carries one.
    pub port: Option<u16>,
    /// Path and query, exactly as written.
    pub rest: String,
}

/// Parse a redirect URI; `None` when the shape is not a URI at all.
#[must_use]
pub fn parse_redirect_uri(s: &str) -> Option<RedirectUri> {
    // no userinfo, no fragment, no control characters
    if s.contains('@') || s.contains('#') || s.chars().any(char::is_control) {
        return None;
    }
    let (scheme, rest) = s.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "https" && scheme != "http" {
        return None;
    }
    let (authority, path) = match rest.find(['/', '?']) {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return None;
    }
    // split off the port without eating IPv6 brackets (a [::1] host is a
    // loopback one, but the spec names 127.0.0.1 and localhost: keep the
    // brackets in the host and let the equality decide)
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        let close = stripped.find(']')?;
        let h = &stripped[..close];
        let after = &stripped[close + 1..];
        let p = after.strip_prefix(':').and_then(|p| p.parse::<u16>().ok());
        if !after.is_empty() && p.is_none() {
            return None;
        }
        (h.to_string(), p)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse::<u16>().ok()),
            None => (authority.to_string(), None),
        }
    };
    if host.is_empty() || host.contains(':') {
        return None;
    }
    Some(RedirectUri {
        scheme,
        host: host.to_ascii_lowercase(),
        port,
        rest: path.to_string(),
    })
}

/// Whether `host` (already lowercase) is a loopback name for the native
/// redirect exception.
fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host == "127.0.0.1" || host == "::1"
}

/// Whether a redirect URI may be REGISTERED (CIMD document or dynamic
/// registration): `https://` anywhere, or `http://` on a loopback host for
/// native clients (RFC 8252). Everything else is refused.
#[must_use]
pub fn redirect_uri_ok(s: &str) -> bool {
    let Some(u) = parse_redirect_uri(s) else {
        return false;
    };
    match u.scheme.as_str() {
        "https" => true,
        // the native exception: loopback host, any port
        "http" => is_loopback_host(&u.host),
        _ => false,
    }
}

/// Whether a request's `redirect_uri` matches the registered one: exact
/// string first, then the RFC 8252 § 7.3 exception — a loopback `http`
/// redirect matches on any port. Never for `https`, never for other hosts.
#[must_use]
pub fn redirect_matches(registered: &str, requested: &str) -> bool {
    if registered == requested {
        return true;
    }
    let (Some(reg), Some(req)) = (
        parse_redirect_uri(registered),
        parse_redirect_uri(requested),
    ) else {
        return false;
    };
    // the port is ignored ONLY for http on a loopback host
    reg.scheme == "http"
        && req.scheme == "http"
        && is_loopback_host(&reg.host)
        && reg.host == req.host
        && reg.rest == req.rest
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap(/* test-only */)
    }

    #[test]
    fn reserved_blocks_are_not_public() {
        for s in [
            "127.0.0.1",
            "127.255.0.9",
            "0.0.0.0",
            "0.1.2.3",
            "10.1.2.3",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "169.254.169.254",
            "100.64.0.1",
            "100.127.255.255",
            "224.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "fc00::1",
            "fd12:3456::1",
            "fe80::1",
            "ff02::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "2001:db8::1",
        ] {
            assert!(!is_public_ip(ip(s)), "{s} must not be public");
        }
    }

    #[test]
    fn real_addresses_are_public() {
        for s in [
            "1.1.1.1",
            "8.8.8.8",
            "172.32.0.1",
            "100.128.0.1",
            "2606:4700::1111",
        ] {
            assert!(is_public_ip(ip(s)), "{s} must be public");
        }
    }

    #[test]
    fn ipv4_mapped_ipv6_is_judged_as_its_ipv4() {
        // the mapped form of a public v4 is public, of a private one is not
        assert!(is_public_ip(ip("::ffff:8.8.8.8")));
        assert!(!is_public_ip(ip("::ffff:192.168.1.1")));
    }

    #[test]
    fn registered_redirect_shapes() {
        assert!(redirect_uri_ok(
            "https://chatgpt.com/connector_platform_oauth_redirect"
        ));
        assert!(redirect_uri_ok("https://claude.ai/api/mcp/auth_callback"));
        assert!(redirect_uri_ok("http://127.0.0.1:57123/callback"));
        assert!(redirect_uri_ok("http://localhost/callback"));
        assert!(!redirect_uri_ok("http://evil.example/cb"), "not loopback");
        assert!(!redirect_uri_ok("ftp://x/cb"), "not https/http");
        assert!(!redirect_uri_ok("https://user:pw@host/cb"), "userinfo");
        assert!(!redirect_uri_ok("https://host/cb#frag"), "fragment");
        assert!(!redirect_uri_ok("https://"), "no host");
    }

    #[test]
    fn loopback_http_matches_on_any_port_but_only_there() {
        assert!(redirect_matches(
            "http://localhost/callback",
            "http://localhost:49152/callback"
        ));
        assert!(redirect_matches(
            "http://127.0.0.1/callback",
            "http://127.0.0.1:49152/callback"
        ));
        // same port, same string — trivially
        assert!(redirect_matches(
            "https://claude.ai/api/mcp/auth_callback",
            "https://claude.ai/api/mcp/auth_callback"
        ));
        // never for https
        assert!(!redirect_matches(
            "https://localhost/cb",
            "https://localhost:8443/cb"
        ));
        // never across hosts, never across paths
        assert!(!redirect_matches(
            "http://localhost/cb",
            "http://127.0.0.1:49152/cb"
        ));
        assert!(!redirect_matches(
            "http://localhost/cb",
            "http://localhost:other"
        ));
        // a port only in the registered side still matches portless requests
        assert!(redirect_matches(
            "http://127.0.0.1:3311/cb",
            "http://127.0.0.1/cb"
        ));
        // non-loopback http is compared exactly
        assert!(!redirect_matches(
            "http://evil.example/cb",
            "http://evil.example:81/cb"
        ));
    }
}

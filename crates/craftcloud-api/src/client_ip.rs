//! Client address resolution behind trusted reverse proxies.
//!
//! The peer address of the TCP connection is authoritative. Only when the
//! peer is a configured trusted proxy is `X-Forwarded-For` consulted, walked
//! from right to left skipping trusted hops; the first untrusted hop is the
//! client. A malformed or oversized header, or a header from an untrusted
//! peer, is ignored and the peer address is used.
//!
//! `Host`, `X-Forwarded-Host` and `Forwarded` are never used. `X-Forwarded-Proto`
//! is used only by device sign-in routes and only when the TCP peer is a
//! configured trusted proxy. Absolute URLs come from the configured public URL.
//! The resolved address is a personal datum and is not logged by default.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::HeaderMap,
    middleware::Next,
    response::Response,
};
use ipnet::IpNet;

const FORWARDED_FOR: &str = "x-forwarded-for";
/// Maximum total size of all `X-Forwarded-For` header values.
const MAX_HEADER_BYTES: usize = 1024;
/// Maximum number of hops considered.
const MAX_HOPS: usize = 16;

/// Resolved client address, stored as a request extension. Absent when the
/// connection info is unavailable (for example in unit tests with `oneshot`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientIp(pub IpAddr);

/// Whether the TCP peer is an operator-configured reverse proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrustedProxy(pub bool);

pub(crate) async fn resolve_client_ip(
    State(trusted): State<Arc<Vec<IpNet>>>,
    mut request: Request,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip());
    let trusted_peer = peer.is_some_and(|peer| is_trusted(&trusted, peer));
    request.extensions_mut().insert(TrustedProxy(trusted_peer));
    if let Some(peer) = peer {
        let chain = forwarded_chain(request.headers());
        let ip = resolve(peer, &trusted, chain.as_deref());
        request.extensions_mut().insert(ClientIp(ip));
    }
    next.run(request).await
}

/// Parses `X-Forwarded-For`. Returns `None` if absent or malformed.
fn forwarded_chain(headers: &HeaderMap) -> Option<Vec<IpAddr>> {
    let mut joined = String::new();
    for value in headers.get_all(FORWARDED_FOR) {
        if !joined.is_empty() {
            joined.push(',');
        }
        joined.push_str(value.to_str().ok()?);
        if joined.len() > MAX_HEADER_BYTES {
            return None;
        }
    }
    if joined.is_empty() {
        return None;
    }
    let mut hops = Vec::new();
    for part in joined.split(',') {
        if hops.len() == MAX_HOPS {
            return None;
        }
        hops.push(part.trim().parse::<IpAddr>().ok()?.to_canonical());
    }
    Some(hops)
}

fn is_trusted(trusted: &[IpNet], ip: IpAddr) -> bool {
    let ip = ip.to_canonical();
    trusted.iter().any(|net| net.contains(&ip))
}

/// Core algorithm; `chain` is the parsed `X-Forwarded-For` hop list, if valid.
/// If every hop is trusted, the left-most hop is the client.
fn resolve(peer: IpAddr, trusted: &[IpNet], chain: Option<&[IpAddr]>) -> IpAddr {
    let peer = peer.to_canonical();
    let Some(chain) = chain else {
        return peer;
    };
    if !is_trusted(trusted, peer) {
        return peer;
    }
    chain
        .iter()
        .rev()
        .find(|hop| !is_trusted(trusted, **hop))
        .or_else(|| chain.first())
        .copied()
        .unwrap_or(peer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn nets(list: &[&str]) -> Vec<IpNet> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for v in values {
            map.append(FORWARDED_FOR, HeaderValue::from_str(v).unwrap());
        }
        map
    }

    fn run(peer: &str, trusted: &[&str], xff: &[&str]) -> IpAddr {
        let chain = forwarded_chain(&headers(xff));
        resolve(ip(peer), &nets(trusted), chain.as_deref())
    }

    #[test]
    fn untrusted_peer_ignores_spoofed_header() {
        assert_eq!(
            run("203.0.113.9", &["10.0.0.0/8"], &["1.2.3.4"]),
            ip("203.0.113.9")
        );
        assert_eq!(run("203.0.113.9", &[], &["1.2.3.4"]), ip("203.0.113.9"));
    }

    #[test]
    fn trusted_peer_takes_first_untrusted_from_right() {
        // client, spoofed-by-client entry, then two trusted hops.
        assert_eq!(
            run(
                "10.0.0.1",
                &["10.0.0.0/8"],
                &["6.6.6.6, 198.51.100.7, 10.0.0.5"]
            ),
            ip("198.51.100.7")
        );
        // Spoofed left part is never reached.
        assert_eq!(
            run("10.0.0.1", &["10.0.0.1/32"], &["6.6.6.6", "198.51.100.7"]),
            ip("198.51.100.7")
        );
    }

    #[test]
    fn all_trusted_chain_returns_leftmost() {
        assert_eq!(
            run("10.0.0.1", &["10.0.0.0/8"], &["10.0.0.9, 10.0.0.8"]),
            ip("10.0.0.9")
        );
    }

    #[test]
    fn trusted_peer_without_header_is_peer() {
        assert_eq!(run("10.0.0.1", &["10.0.0.0/8"], &[]), ip("10.0.0.1"));
    }

    #[test]
    fn garbage_header_falls_back_to_peer() {
        for bad in [
            "garbage",
            "",
            "1.2.3.4,",
            ",1.2.3.4",
            "1.2.3.4:80",
            "1.2.3.4, nope",
            "unknown",
        ] {
            assert_eq!(
                run("10.0.0.1", &["10.0.0.0/8"], &[bad]),
                ip("10.0.0.1"),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn oversized_or_too_many_hops_fall_back_to_peer() {
        let many = vec!["198.51.100.7"; MAX_HOPS + 1].join(",");
        assert_eq!(
            run("10.0.0.1", &["10.0.0.0/8"], &[many.as_str()]),
            ip("10.0.0.1")
        );
        let ok = vec!["198.51.100.7"; MAX_HOPS].join(",");
        assert_eq!(
            run("10.0.0.1", &["10.0.0.0/8"], &[ok.as_str()]),
            ip("198.51.100.7")
        );
        let big = format!("{}1.2.3.4", " ".repeat(MAX_HEADER_BYTES));
        assert_eq!(
            run("10.0.0.1", &["10.0.0.0/8"], &[big.as_str()]),
            ip("10.0.0.1")
        );
    }

    #[test]
    fn non_utf8_header_falls_back_to_peer() {
        let mut map = HeaderMap::new();
        map.append(
            FORWARDED_FOR,
            HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        assert_eq!(forwarded_chain(&map), None);
    }

    #[test]
    fn ipv6_chains() {
        assert_eq!(
            run("fd00::1", &["fd00::/8"], &["2001:db8::5, fd00::7"]),
            ip("2001:db8::5")
        );
        assert_eq!(
            run("2001:db8::9", &["fd00::/8"], &["2001:db8::5"]),
            ip("2001:db8::9")
        );
    }

    #[test]
    fn mixed_family_and_mapped_addresses() {
        assert_eq!(
            run("::ffff:10.0.0.1", &["10.0.0.0/8"], &["198.51.100.7"]),
            ip("198.51.100.7")
        );
        assert_eq!(
            run(
                "10.0.0.1",
                &["10.0.0.0/8"],
                &["198.51.100.7, ::ffff:10.0.0.5"]
            ),
            ip("198.51.100.7")
        );
    }
}

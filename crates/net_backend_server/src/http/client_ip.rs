//! The client's address: the connection's peer, or, behind a trusted reverse proxy, the address
//! the proxy reports in `X-Forwarded-For`.

use std::net::{IpAddr, SocketAddr};

use axum::extract::{ConnectInfo, FromRequestParts, Request};
use axum::middleware::Next;
use axum::response::Response;
use http::request::Parts;
use http::HeaderMap;

use crate::state::AppState;

/// The `X-Forwarded-For` header.
pub const FORWARDED_FOR_HEADER: &str = "x-forwarded-for";

/// The client's address ([`crate::http`]): the connection's peer, or the address a trusted proxy
/// (`http.trusted_proxies`) reports in `X-Forwarded-For` (the right-most entry that is not itself
/// a trusted proxy). `None` when unknown (in-process tests without a connection).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ClientIp(pub Option<IpAddr>);

impl ClientIp {
    /// The address, if known.
    pub fn ip(self) -> Option<IpAddr> {
        self.0
    }
}

impl<S: Send + Sync> FromRequestParts<S> for ClientIp {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts.extensions.get::<ClientIp>().copied().unwrap_or_else(|| ClientIp(peer(&parts.extensions))))
    }
}

/// An address block: `203.0.113.0/24`, `::1/128`, or a single address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IpNet {
    addr: IpAddr,
    prefix: u8,
}

impl IpNet {
    /// Parse `addr` or `addr/prefix`.
    pub(crate) fn parse(text: &str) -> Option<IpNet> {
        let text = text.trim();
        let (addr, prefix) = match text.split_once('/') {
            Some((addr, prefix)) => (addr.parse::<IpAddr>().ok()?, Some(prefix.parse::<u8>().ok()?)),
            None => (text.parse::<IpAddr>().ok()?, None),
        };
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        (prefix <= max).then_some(IpNet { addr, prefix })
    }

    pub(crate) fn contains(&self, ip: IpAddr) -> bool {
        // An IPv4 peer may arrive as an IPv4-mapped IPv6 address on a dual-stack listener.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => mask_eq(&net.octets(), &ip.octets(), self.prefix),
            (IpAddr::V6(net), IpAddr::V6(ip)) => mask_eq(&net.octets(), &ip.octets(), self.prefix),
            _ => false,
        }
    }
}

fn mask_eq(a: &[u8], b: &[u8], prefix: u8) -> bool {
    let full = usize::from(prefix / 8);
    let rest = prefix % 8;
    if a.get(..full) != b.get(..full) {
        return false;
    }
    if rest == 0 {
        return true;
    }
    let mask = 0xffu8 << (8 - rest);
    match (a.get(full), b.get(full)) {
        (Some(x), Some(y)) => x & mask == y & mask,
        _ => false,
    }
}

fn peer(extensions: &http::Extensions) -> Option<IpAddr> {
    extensions.get::<ConnectInfo<SocketAddr>>().map(|c| match c.0.ip() {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(IpAddr::V6(v6), IpAddr::V4),
        v4 => v4,
    })
}

/// The client address for a peer and the request headers.
pub(crate) fn resolve(peer: Option<IpAddr>, headers: &HeaderMap, trusted: &[IpNet]) -> Option<IpAddr> {
    let peer = peer?;
    if !trusted.iter().any(|net| net.contains(peer)) {
        return Some(peer);
    }
    // Every X-Forwarded-For header, in order, as one list; walk it from the right (the entry the
    // trusted proxy itself appended) and stop at the first address that is not a trusted proxy.
    let entries: Vec<&str> = headers.get_all(FORWARDED_FOR_HEADER).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(',')).collect();
    let mut client = peer;
    for entry in entries.iter().rev() {
        let Some(ip) = parse_forwarded(entry) else {
            // Garbage from further left cannot be trusted: keep the last trusted hop's view.
            return Some(client);
        };
        client = ip;
        if !trusted.iter().any(|net| net.contains(ip)) {
            return Some(ip);
        }
    }
    Some(client)
}

/// Once per process: a local reverse proxy sends `X-Forwarded-For` but none is trusted, so every
/// client counts as the proxy's address (per-address limits become one global limit).
fn warn_untrusted_proxy() {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            "requests arrive from a local proxy with X-Forwarded-For, but http.trusted_proxies is empty: every client counts as the proxy's address (set http.trusted_proxies = [\"127.0.0.1\", \"::1\"])"
        );
    }
}

fn parse_forwarded(entry: &str) -> Option<IpAddr> {
    let entry = entry.trim();
    if let Ok(ip) = entry.parse::<IpAddr>() {
        return Some(ip);
    }
    // `[v6]:port` or `v4:port`.
    entry.parse::<SocketAddr>().ok().map(|s| s.ip())
}

/// Middleware: attach the [`ClientIp`].
pub(crate) async fn attach(axum::extract::State(state): axum::extract::State<AppState>, mut req: Request, next: Next) -> Response {
    let trusted = state.trusted_proxies();
    let peer_ip = peer(req.extensions());
    if trusted.is_empty() && peer_ip.is_some_and(|ip| ip.is_loopback()) && req.headers().contains_key(FORWARDED_FOR_HEADER) {
        warn_untrusted_proxy();
    }
    let ip = resolve(peer_ip, req.headers(), trusted);
    req.extensions_mut().insert(ClientIp(ip));
    next.run(req).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(values: &[&str]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for v in values {
            map.append(FORWARDED_FOR_HEADER, v.parse().unwrap_or_else(|_| http::HeaderValue::from_static("")));
        }
        map
    }

    #[test]
    fn nets() {
        let net = IpNet::parse("10.0.0.0/8").unwrap_or(IpNet { addr: IpAddr::from([0, 0, 0, 0]), prefix: 32 });
        assert!(net.contains("10.200.3.4".parse().unwrap_or(IpAddr::from([0, 0, 0, 0]))));
        assert!(!net.contains("11.0.0.1".parse().unwrap_or(IpAddr::from([0, 0, 0, 0]))));
        assert!(IpNet::parse("::1").is_some_and(|n| n.contains("::1".parse().unwrap_or(IpAddr::from([0, 0, 0, 0])))));
        assert!(IpNet::parse("127.0.0.1").is_some_and(|n| n.contains("::ffff:127.0.0.1".parse().unwrap_or(IpAddr::from([0, 0, 0, 0])))));
        assert!(IpNet::parse("192.168.1.0/23").is_some_and(|n| n.contains(IpAddr::from([192, 168, 0, 255]))));
        assert!(IpNet::parse("192.168.1.0/23").is_some_and(|n| !n.contains(IpAddr::from([192, 168, 2, 0]))));
        for bad in ["", "x", "10.0.0.0/33", "::/129", "10.0.0.0/x"] {
            assert!(IpNet::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn forwarded_for_only_from_trusted_proxies() {
        let proxy = IpAddr::from([127, 0, 0, 1]);
        let client = IpAddr::from([203, 0, 113, 9]);
        let trusted = [IpNet::parse("127.0.0.1").unwrap_or(IpNet { addr: proxy, prefix: 32 })];
        // An untrusted peer's header is ignored.
        assert_eq!(resolve(Some(client), &headers(&["1.2.3.4"]), &trusted), Some(client));
        // The trusted proxy appended the real client; a client-sent spoof further left is ignored.
        assert_eq!(resolve(Some(proxy), &headers(&["6.6.6.6, 203.0.113.9"]), &trusted), Some(client));
        assert_eq!(resolve(Some(proxy), &headers(&["6.6.6.6", "203.0.113.9"]), &trusted), Some(client));
        assert_eq!(resolve(Some(proxy), &headers(&["[2001:db8::1]:443"]), &trusted), Some("2001:db8::1".parse().unwrap_or(proxy)));
        // No header, or only garbage: the proxy's address.
        assert_eq!(resolve(Some(proxy), &HeaderMap::new(), &trusted), Some(proxy));
        assert_eq!(resolve(Some(proxy), &headers(&["garbage"]), &trusted), Some(proxy));
        // Without trusted proxies the peer is the client.
        assert_eq!(resolve(Some(proxy), &headers(&["1.2.3.4"]), &[]), Some(proxy));
        assert_eq!(resolve(None, &headers(&["1.2.3.4"]), &trusted), None);
    }
}

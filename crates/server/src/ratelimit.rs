//! Rate limiting for the credential endpoints (`/api/setup`, `/api/auth/login`,
//! `/api/auth/redeem`).
//!
//! Two token buckets sit in front of those routes:
//!
//! 1. **Per client** — keyed by the client's IP address, so one noisy client
//!    only ever locks *itself* out.
//! 2. **Global backstop** — one shared bucket, generous enough that a family
//!    never notices it, but it caps the total Argon2 work an attacker spread
//!    over many addresses can cause.
//!
//! Working out "the client's IP" is the subtle part. Behind a reverse proxy
//! (tailscale serve, Caddy, a Cloudflare tunnel) every request arrives from the
//! proxy's address, so keying on the socket peer would put the whole family in
//! one bucket again. The real client is in a header the proxy adds — but anyone
//! can *send* that header, so it is only believed when the socket peer is one
//! of the configured trusted proxies. See [`ClientIpConfig`].
//!
//! **IPv6 clients are keyed by their /64**, not the full address. One IPv6
//! host usually owns a whole /64 (SLAAC, privacy addresses), so keying on the
//! full address would hand an attacker 2^64 fresh buckets. IPv4 — including
//! IPv4-mapped IPv6 from a dual-stack listener — is keyed as-is.
//!
//! **Bounding the per-client map.** The per-client layer runs *outside* the
//! global one, so a request later refused by the global backstop has already
//! touched (and possibly created) its per-client entry. Swapping the order
//! would avoid that, but then a client over its own limit would still spend
//! global tokens — one noisy address could drain the shared bucket and lock
//! everyone out. So the order stays, and the map is bounded instead, twice:
//! `main` drops fully refilled buckets every minute (`retain_recent`), and
//! once the map holds [`MAX_CLIENT_KEYS`] entries every request is keyed to a
//! single shared overflow bucket until that cleanup makes room. The overflow
//! bucket is shared by *all* clients while it is in use (the limiter can't
//! answer "is this key already tracked?"), which only happens under a flood
//! from ~100k addresses — when the global backstop is refusing nearly
//! everything anyway.

use axum::Json;
use axum::extract::ConnectInfo;
use axum::http::{HeaderValue, Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use perkele_shared::ErrorResponse;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tower_governor::GovernorError;
use tower_governor::GovernorLayer;
use tower_governor::governor::GovernorConfigBuilder;
use tower_governor::key_extractor::{GlobalKeyExtractor, KeyExtractor};

/// Per-client bucket: a client may fire this many credential requests
/// back-to-back…
pub const PER_CLIENT_BURST: u32 = 10;
/// …after which it gets one more every this often. (tower_governor calls this
/// the "period": the time to replenish ONE token — not a rate per second.)
pub const PER_CLIENT_REFILL: Duration = Duration::from_millis(2000);

/// Global backstop across ALL clients: burst of 100, then one request per
/// 200 ms (5/s sustained). Far above what a family generates, low enough to
/// bound Argon2 load from a distributed attacker.
pub const GLOBAL_BURST: u32 = 100;
pub const GLOBAL_REFILL: Duration = Duration::from_millis(200);

/// Most per-client buckets tracked at once (see the module docs). An entry is
/// a few dozen bytes, so this caps the map at a few MB.
pub const MAX_CLIENT_KEYS: usize = 100_000;

/// The shared bucket new clients fall into while the map is full. An IPv6
/// documentation address, so it can't collide with a real client key.
const OVERFLOW_KEY: IpAddr = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0xffff, 0xffff, 0, 0, 0, 0));

/// Default `PERKELE_TRUSTED_PROXIES`: loopback (tailscale serve, a host-level
/// Caddy) plus Docker's default bridge range (a proxy in a sibling container).
pub const DEFAULT_TRUSTED_PROXIES: &str = "127.0.0.1/32,::1/128,172.16.0.0/12";

/// The bucket key used when a request carries no socket address at all. That
/// only happens in tests that call the router directly (`oneshot`), never with
/// a real listener — but it must not be an error: a failed key extraction
/// becomes a 500 in tower_governor.
const NO_PEER_KEY: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

/// Where the client's IP address comes from (`PERKELE_CLIENT_IP_SOURCE`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientIpSource {
    /// The TCP peer address. Correct when clients connect directly.
    Peer,
    /// The rightmost `X-Forwarded-For` entry (tailscale serve, Caddy, nginx…).
    XForwardedFor,
    /// Cloudflare's `CF-Connecting-IP` (Cloudflare tunnel / proxy).
    CfConnectingIp,
}

/// An IPv4 or IPv6 network in CIDR notation, e.g. `172.16.0.0/12`.
///
/// Hand-rolled instead of pulling in a crate: matching is just "do the first
/// `prefix` bits agree?", which is a mask-and-compare on the address as an
/// integer (`u32` for IPv4, `u128` for IPv6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cidr {
    network: IpAddr,
    prefix: u8,
}

impl Cidr {
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        // A bare address means a single host (/32 or /128).
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let network: IpAddr = addr
            .parse()
            .map_err(|_| format!("'{s}' is not an IP address or CIDR"))?;
        let max = if network.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(p) => p
                .parse::<u8>()
                .ok()
                .filter(|p| *p <= max)
                .ok_or_else(|| format!("'{s}' has an invalid prefix length (0–{max})"))?,
            None => max,
        };
        Ok(Self { network, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        // `to_canonical` turns an IPv4-mapped IPv6 address (`::ffff:1.2.3.4`,
        // what a dual-stack `[::]` listener reports for IPv4 clients) back
        // into plain IPv4 so it can match an IPv4 range.
        match (self.network, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                // `checked_shl` avoids the overflow panic of `u32 << 32` for /0.
                let mask = u32::MAX
                    .checked_shl(32 - u32::from(self.prefix))
                    .unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.prefix))
                    .unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false, // IPv4 range vs IPv6 address or vice versa
        }
    }
}

/// Parsed once at startup from `PERKELE_CLIENT_IP_SOURCE` and
/// `PERKELE_TRUSTED_PROXIES`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIpConfig {
    pub source: ClientIpSource,
    pub trusted_proxies: Vec<Cidr>,
}

impl ClientIpConfig {
    /// Read the two env vars. An invalid value is a startup error — silently
    /// falling back would leave the rate limiter keyed on the wrong thing.
    pub fn from_env() -> Result<Self, String> {
        Self::parse(
            std::env::var("PERKELE_CLIENT_IP_SOURCE").ok().as_deref(),
            std::env::var("PERKELE_TRUSTED_PROXIES").ok().as_deref(),
        )
    }

    /// `None` or blank means "use the default" for both inputs (compose passes
    /// unset variables through as empty strings).
    pub fn parse(source: Option<&str>, trusted: Option<&str>) -> Result<Self, String> {
        let source = match source.map(str::trim).filter(|s| !s.is_empty()) {
            None | Some("peer") => ClientIpSource::Peer,
            Some("x-forwarded-for") => ClientIpSource::XForwardedFor,
            Some("cf-connecting-ip") => ClientIpSource::CfConnectingIp,
            Some(other) => {
                return Err(format!(
                    "PERKELE_CLIENT_IP_SOURCE='{other}' is invalid; \
                     use one of: peer, x-forwarded-for, cf-connecting-ip"
                ));
            }
        };
        let trusted = trusted
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(DEFAULT_TRUSTED_PROXIES);
        let trusted_proxies = trusted
            .split(',')
            .filter(|s| !s.trim().is_empty())
            // `collect` into `Result<Vec<_>, _>` stops at the first error —
            // a handy way to validate every element of a list at once.
            .map(Cidr::parse)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("PERKELE_TRUSTED_PROXIES: {e}"))?;
        Ok(Self {
            source,
            trusted_proxies,
        })
    }

    fn is_trusted(&self, peer: IpAddr) -> bool {
        self.trusted_proxies.iter().any(|c| c.contains(peer))
    }

    /// The address to rate-limit a request by.
    pub fn client_ip<B>(&self, req: &Request<B>) -> IpAddr {
        // `ConnectInfo` is put into the request extensions by
        // `into_make_service_with_connect_info` in `main`. Absent in tests.
        let Some(peer) = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(addr)| addr.ip().to_canonical())
        else {
            return NO_PEER_KEY;
        };
        if self.source == ClientIpSource::Peer || !self.is_trusted(peer) {
            // Headers from an untrusted peer are attacker-controlled: ignore.
            return peer;
        }
        let from_header = match self.source {
            ClientIpSource::Peer => None,
            // Each proxy APPENDS the address it saw, so the rightmost entry is
            // the one our trusted hop wrote; everything left of it came from
            // the client and may be forged.
            ClientIpSource::XForwardedFor => last_header_str(req, "x-forwarded-for")
                .and_then(|v| v.rsplit(',').next())
                .and_then(|ip| ip.trim().parse::<IpAddr>().ok()),
            ClientIpSource::CfConnectingIp => last_header_str(req, "cf-connecting-ip")
                .and_then(|ip| ip.trim().parse::<IpAddr>().ok()),
        };
        from_header.map(|ip| ip.to_canonical()).unwrap_or(peer)
    }
}

/// The LAST line of a header that may appear several times. `headers().get`
/// would return the first line — the one a client can send itself — while the
/// trusted proxy's line (if it adds a new one rather than appending) comes last.
fn last_header_str<'a, B>(req: &'a Request<B>, name: &str) -> Option<&'a str> {
    req.headers()
        .get_all(name)
        .iter()
        // `next_back` = the last element, taken from the end directly
        // (`last()` would walk every line to get there).
        .next_back()
        .and_then(|v| v.to_str().ok())
}

/// The bucket key for a client address: IPv4 as-is, IPv6 masked to its /64.
pub fn rate_limit_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        v4 @ IpAddr::V4(_) => v4,
        IpAddr::V6(v6) => {
            // Keep the top 64 bits (network prefix), zero the interface id.
            let masked = u128::from(v6) & (u128::MAX << 64);
            IpAddr::V6(Ipv6Addr::from(masked))
        }
    }
}

/// "How many keys does the per-client limiter hold?" — a closure so the
/// extractor doesn't have to spell out the limiter's long generic type.
type KeyCount = Box<dyn Fn() -> usize + Send + Sync>;

/// tower_governor key extractor that keys the bucket by
/// [`rate_limit_key`]`(`[`ClientIpConfig::client_ip`]`)`, falling back to
/// [`OVERFLOW_KEY`] while the limiter already tracks `cap` keys.
///
/// Everything sits behind `Arc`s because tower clones the extractor freely.
/// `tracked` is a `OnceLock` because of a chicken-and-egg: the limiter is
/// built *from* the extractor, so the extractor gets its handle on the
/// limiter afterwards (see `apply_with_cap`).
#[derive(Clone)]
pub struct ClientIpKeyExtractor {
    config: Arc<ClientIpConfig>,
    tracked: Arc<OnceLock<KeyCount>>,
    cap: usize,
}

// Manual `Debug`: the boxed closure has no `Debug` impl to derive from.
impl std::fmt::Debug for ClientIpKeyExtractor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientIpKeyExtractor")
            .field("config", &self.config)
            .field("cap", &self.cap)
            .finish_non_exhaustive()
    }
}

impl KeyExtractor for ClientIpKeyExtractor {
    type Key = IpAddr;

    fn extract<T>(&self, req: &Request<T>) -> Result<Self::Key, GovernorError> {
        // Never an error: every path has a fallback.
        let full = self.tracked.get().is_some_and(|len| len() >= self.cap);
        if full {
            return Ok(OVERFLOW_KEY);
        }
        Ok(rate_limit_key(self.config.client_ip(req)))
    }
}

/// The 429 body, in the same `{"error": "..."}` shape as every other API error
/// so the app shows it instead of tower_governor's English plain text.
fn too_many_requests(err: GovernorError) -> Response {
    let wait = match err {
        GovernorError::TooManyRequests { wait_time, .. } => wait_time,
        // Our extractor never fails; treat anything else as "try again soon".
        _ => 1,
    };
    let mut resp = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(ErrorResponse {
            error: "Liian monta yritystä. Yritä hetken kuluttua uudelleen.".to_owned(),
        }),
    )
        .into_response();
    resp.headers_mut()
        .insert(header::RETRY_AFTER, HeaderValue::from(wait.max(1)));
    resp
}

/// Wrap `router` in both limiters. Returns the wrapped router plus a cleanup
/// function that drops idle per-client buckets; `main` calls it periodically so
/// the key map can't grow without bound. (The global bucket has one key.)
pub fn apply<S>(
    router: axum::Router<S>,
    ip_config: Arc<ClientIpConfig>,
) -> (axum::Router<S>, impl Fn() + Send + Sync + 'static)
where
    S: Clone + Send + Sync + 'static,
{
    apply_with_cap(router, ip_config, MAX_CLIENT_KEYS)
}

/// [`apply`] with a configurable per-client key cap (tests use a tiny one).
fn apply_with_cap<S>(
    router: axum::Router<S>,
    ip_config: Arc<ClientIpConfig>,
    cap: usize,
) -> (axum::Router<S>, impl Fn() + Send + Sync + 'static)
where
    S: Clone + Send + Sync + 'static,
{
    let tracked: Arc<OnceLock<KeyCount>> = Arc::new(OnceLock::new());
    let per_client = GovernorConfigBuilder::default()
        .period(PER_CLIENT_REFILL)
        .burst_size(PER_CLIENT_BURST)
        .key_extractor(ClientIpKeyExtractor {
            config: ip_config,
            tracked: tracked.clone(),
            cap,
        })
        .finish()
        .expect("per-client limits are non-zero");
    // Now that the limiter exists, give the extractor its key counter. The
    // closure holds its own `Arc` to the limiter; no cycle, since the limiter
    // never points back at the extractor.
    let counted = per_client.limiter().clone();
    let _ = tracked.set(Box::new(move || counted.len()));
    let global = GovernorConfigBuilder::default()
        .period(GLOBAL_REFILL)
        .burst_size(GLOBAL_BURST)
        .key_extractor(GlobalKeyExtractor)
        .finish()
        .expect("global limits are non-zero");

    let per_client_limiter = per_client.limiter().clone();
    let cleanup = move || per_client_limiter.retain_recent();

    // `.layer(a).layer(b)` makes `b` the OUTER layer. Per-client goes outside
    // so a client that is already over its own limit is turned away before it
    // can spend tokens from the shared global bucket.
    let router = router
        .layer(GovernorLayer::new(global).error_handler(too_many_requests))
        .layer(GovernorLayer::new(per_client).error_handler(too_many_requests));
    (router, cleanup)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::routing::get;
    use tower::ServiceExt;

    fn cfg(source: &str) -> ClientIpConfig {
        ClientIpConfig::parse(Some(source), None).unwrap()
    }

    /// A request as the real server would see it: with the socket peer in the
    /// `ConnectInfo` extension, plus optional headers.
    fn req_from(peer: &str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut b = Request::builder().uri("/");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let mut req = b.body(Body::empty()).unwrap();
        let addr: SocketAddr = format!("{peer}:40000").parse().unwrap();
        req.extensions_mut().insert(ConnectInfo(addr));
        req
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn cidr_matching() {
        let docker = Cidr::parse("172.16.0.0/12").unwrap();
        assert!(docker.contains(ip("172.17.0.1")));
        assert!(docker.contains(ip("172.31.255.255")));
        assert!(!docker.contains(ip("172.32.0.1")));
        assert!(!docker.contains(ip("::1")));
        // IPv4-mapped IPv6 (dual-stack listener) still matches the v4 range.
        assert!(
            Cidr::parse("127.0.0.1/32")
                .unwrap()
                .contains(ip("::ffff:127.0.0.1"))
        );
        assert!(Cidr::parse("::1/128").unwrap().contains(ip("::1")));
        assert!(Cidr::parse("0.0.0.0/0").unwrap().contains(ip("8.8.8.8")));
        // A bare address is a single host.
        assert!(Cidr::parse("10.0.0.5").unwrap().contains(ip("10.0.0.5")));
        assert!(!Cidr::parse("10.0.0.5").unwrap().contains(ip("10.0.0.6")));
    }

    #[test]
    fn invalid_config_is_rejected_with_a_clear_message() {
        let e = ClientIpConfig::parse(Some("x-real-ip"), None).unwrap_err();
        assert!(e.contains("PERKELE_CLIENT_IP_SOURCE"), "{e}");
        let e = ClientIpConfig::parse(None, Some("10.0.0.0/33")).unwrap_err();
        assert!(e.contains("PERKELE_TRUSTED_PROXIES"), "{e}");
        let e = ClientIpConfig::parse(None, Some("not-an-ip")).unwrap_err();
        assert!(e.contains("PERKELE_TRUSTED_PROXIES"), "{e}");
    }

    #[test]
    fn blank_config_means_defaults() {
        let c = ClientIpConfig::parse(Some(""), Some("  ")).unwrap();
        assert_eq!(c, ClientIpConfig::parse(None, None).unwrap());
        assert_eq!(c.source, ClientIpSource::Peer);
        assert_eq!(c.trusted_proxies.len(), 3);
    }

    #[test]
    fn peer_source_ignores_headers() {
        let r = req_from("127.0.0.1", &[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(cfg("peer").client_ip(&r), ip("127.0.0.1"));
    }

    #[test]
    fn rightmost_forwarded_for_is_used_from_a_trusted_peer() {
        // The client forged "6.6.6.6"; the trusted proxy appended the real one.
        let r = req_from("127.0.0.1", &[("x-forwarded-for", "6.6.6.6, 100.64.0.7")]);
        assert_eq!(cfg("x-forwarded-for").client_ip(&r), ip("100.64.0.7"));
    }

    #[test]
    fn spoofed_forwarded_for_from_an_untrusted_peer_is_ignored() {
        let r = req_from("203.0.113.9", &[("x-forwarded-for", "1.2.3.4")]);
        assert_eq!(cfg("x-forwarded-for").client_ip(&r), ip("203.0.113.9"));
        let r = req_from("203.0.113.9", &[("cf-connecting-ip", "1.2.3.4")]);
        assert_eq!(cfg("cf-connecting-ip").client_ip(&r), ip("203.0.113.9"));
    }

    #[test]
    fn cf_connecting_ip_from_a_trusted_peer() {
        let r = req_from("172.18.0.2", &[("cf-connecting-ip", "198.51.100.4")]);
        assert_eq!(cfg("cf-connecting-ip").client_ip(&r), ip("198.51.100.4"));
    }

    #[test]
    fn missing_or_garbage_header_falls_back_to_the_peer() {
        let c = cfg("x-forwarded-for");
        assert_eq!(c.client_ip(&req_from("127.0.0.1", &[])), ip("127.0.0.1"));
        let r = req_from("127.0.0.1", &[("x-forwarded-for", "not-an-ip")]);
        assert_eq!(c.client_ip(&r), ip("127.0.0.1"));
    }

    /// `req_from` with repeated header lines, which the builder's `.header`
    /// can't express (it would still work — `header` appends — but being
    /// explicit documents that there are two separate lines).
    fn req_with_lines(peer: &str, name: &'static str, lines: &[&str]) -> Request<Body> {
        let mut req = req_from(peer, &[]);
        for v in lines {
            req.headers_mut()
                .append(name, HeaderValue::from_str(v).unwrap());
        }
        req
    }

    #[test]
    fn with_two_forwarded_for_lines_the_last_one_wins() {
        // The client sent its own (forged) X-Forwarded-For line; the trusted
        // proxy added a second line rather than appending to the first.
        let r = req_with_lines(
            "127.0.0.1",
            "x-forwarded-for",
            &["6.6.6.6", "7.7.7.7, 100.64.0.7"],
        );
        assert_eq!(cfg("x-forwarded-for").client_ip(&r), ip("100.64.0.7"));
    }

    #[test]
    fn with_two_cf_connecting_ip_lines_the_last_one_wins() {
        let r = req_with_lines(
            "172.18.0.2",
            "cf-connecting-ip",
            &["6.6.6.6", "198.51.100.4"],
        );
        assert_eq!(cfg("cf-connecting-ip").client_ip(&r), ip("198.51.100.4"));
    }

    #[test]
    fn ipv6_clients_are_keyed_by_their_64_prefix() {
        let k = |s| rate_limit_key(ip(s));
        // One household / one attacker's /64: same bucket.
        assert_eq!(k("2001:db8:1:2::1"), k("2001:db8:1:2:ffff:ffff:ffff:ffff"));
        assert_eq!(k("2001:db8:1:2::1"), ip("2001:db8:1:2::"));
        // A different /64: a different bucket.
        assert_ne!(k("2001:db8:1:2::1"), k("2001:db8:1:3::1"));
        // IPv4 is untouched, and IPv4-mapped IPv6 is treated as IPv4 — NOT
        // masked to /64, which would put every IPv4 client in one bucket.
        assert_eq!(k("192.0.2.1"), ip("192.0.2.1"));
        assert_eq!(k("::ffff:192.0.2.1"), ip("192.0.2.1"));
        assert_ne!(k("::ffff:192.0.2.1"), k("::ffff:192.0.2.2"));
    }

    #[tokio::test]
    async fn one_ipv6_64_shares_a_single_bucket() {
        let app = limited_app("peer");
        for i in 0..PER_CLIENT_BURST {
            // A fresh address each time — but all inside one /64.
            let peer = format!("[2001:db8::{:x}]", i + 1);
            assert_eq!(status(&app, req_from(&peer, &[])).await, StatusCode::OK);
        }
        let other_in_same_64 = req_from("[2001:db8::ffff]", &[]);
        assert_eq!(
            status(&app, other_in_same_64).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[tokio::test]
    async fn the_per_client_map_is_capped() {
        // Cap of 2 tracked clients: once full, further new clients share one
        // overflow bucket instead of each adding an entry.
        let (app, _cleanup) = apply_with_cap(
            Router::new().route("/", get(|| async { "ok" })),
            Arc::new(cfg("peer")),
            2,
        );
        for peer in ["192.0.2.1", "192.0.2.2"] {
            assert_eq!(status(&app, req_from(peer, &[])).await, StatusCode::OK);
        }
        for _ in 0..PER_CLIENT_BURST {
            assert_eq!(
                status(&app, req_from("192.0.2.3", &[])).await,
                StatusCode::OK
            );
        }
        // 192.0.2.4 was never seen, yet it is refused: it landed in the
        // overflow bucket that 192.0.2.3 just emptied.
        assert_eq!(
            status(&app, req_from("192.0.2.4", &[])).await,
            StatusCode::TOO_MANY_REQUESTS
        );
    }

    #[test]
    fn no_connect_info_uses_the_fallback_key() {
        let r = Request::builder().uri("/").body(Body::empty()).unwrap();
        assert_eq!(cfg("x-forwarded-for").client_ip(&r), NO_PEER_KEY);
    }

    fn limited_app(source: &str) -> Router {
        let (r, _cleanup) = apply(
            Router::new().route("/", get(|| async { "ok" })),
            Arc::new(cfg(source)),
        );
        r
    }

    async fn status(app: &Router, req: Request<Body>) -> StatusCode {
        app.clone().oneshot(req).await.unwrap().status()
    }

    #[tokio::test]
    async fn per_client_bucket_isolates_clients() {
        let app = limited_app("peer");
        // Client A spends its whole burst and is then refused…
        for _ in 0..PER_CLIENT_BURST {
            assert_eq!(
                status(&app, req_from("192.0.2.1", &[])).await,
                StatusCode::OK
            );
        }
        let resp = app
            .clone()
            .oneshot(req_from("192.0.2.1", &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(resp.headers().get(header::RETRY_AFTER).is_some());
        // …while client B is unaffected.
        assert_eq!(
            status(&app, req_from("192.0.2.2", &[])).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn forwarded_clients_behind_one_proxy_get_separate_buckets() {
        let app = limited_app("x-forwarded-for");
        let a = || req_from("127.0.0.1", &[("x-forwarded-for", "100.64.0.1")]);
        for _ in 0..PER_CLIENT_BURST {
            assert_eq!(status(&app, a()).await, StatusCode::OK);
        }
        assert_eq!(status(&app, a()).await, StatusCode::TOO_MANY_REQUESTS);
        let b = req_from("127.0.0.1", &[("x-forwarded-for", "100.64.0.2")]);
        assert_eq!(status(&app, b).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn global_backstop_caps_many_clients() {
        let app = limited_app("peer");
        // Each request from a fresh address: the per-client buckets never
        // trip, so only the global bucket can produce the 429.
        let mut saw_429 = false;
        for i in 0..(GLOBAL_BURST + 20) {
            let peer = format!("10.{}.{}.1", i / 256, i % 256);
            if status(&app, req_from(&peer, &[])).await == StatusCode::TOO_MANY_REQUESTS {
                saw_429 = true;
                break;
            }
        }
        assert!(saw_429, "global backstop never tripped");
    }

    #[tokio::test]
    async fn requests_without_connect_info_are_not_a_server_error() {
        let app = limited_app("x-forwarded-for");
        let r = Request::builder().uri("/").body(Body::empty()).unwrap();
        assert_eq!(status(&app, r).await, StatusCode::OK);
    }
}

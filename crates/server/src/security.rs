//! Response hardening: a middleware that stamps security headers onto every
//! response. The rate limiter lives in `ratelimit`.

use axum::extract::{Request, State};
use axum::http::{HeaderValue, header};
use axum::middleware::Next;
use axum::response::Response;

/// Content-Security-Policy for the WASM single-page app.
///
/// - `script-src 'wasm-unsafe-eval'` — required to instantiate WebAssembly.
/// - `connect-src 'self'` — XHR/fetch and the future SSE stream, same origin.
/// - `frame-ancestors 'none'` — can't be embedded in an iframe (clickjacking).
///
/// `style-src 'unsafe-inline'` is permitted for now because the framework emits
/// inline styles; revisit once the UI is in place (Phase 1F).
const CSP: &str = "default-src 'self'; \
     script-src 'self' 'wasm-unsafe-eval'; \
     style-src 'self' 'unsafe-inline'; \
     img-src 'self' data:; \
     connect-src 'self'; \
     base-uri 'self'; \
     form-action 'self'; \
     frame-ancestors 'none'";

/// Cache policy chosen by request path:
/// - hashed assets (`/assets/…`, `/wasm/…`) have content-hashed names and never
///   change → cache them forever;
/// - API responses must never be cached;
/// - everything else is the HTML shell / SPA fallback → `no-cache` (revalidate
///   every load), so a redeployed app is picked up immediately instead of a
///   stale `index.html` pinning the browser to the old bundle.
fn cache_control(path: &str) -> &'static str {
    if path.starts_with("/assets/") || path.starts_with("/wasm/") {
        "public, max-age=31536000, immutable"
    } else if path.starts_with("/api/") {
        "no-store"
    } else {
        "no-cache"
    }
}

/// HSTS: once a browser has seen this over HTTPS it refuses plain http for the
/// host for a year. Only sent when cookies are `Secure` (i.e. the deployment is
/// HTTPS) — sending it from a plain-http dev server would be pointless at best.
const HSTS: &str = "max-age=31536000";

/// Middleware that adds security headers to every response.
///
/// `State(https)` is the `cookie_secure` flag, handed in by
/// `from_fn_with_state` in `main` — middleware gets state the same way
/// handlers do.
pub async fn security_headers(State(https): State<bool>, req: Request, next: Next) -> Response {
    // Capture the path before the request is consumed; it decides caching below.
    let path = req.uri().path().to_owned();
    let mut resp = next.run(req).await;
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CSP),
    );
    // Don't let browsers MIME-sniff responses into a different content type.
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Belt-and-braces with the CSP frame-ancestors directive.
    headers.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    // Don't leak the app's URLs to other origins.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(cache_control(&path)),
    );
    if https {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static(HSTS),
        );
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use tower::ServiceExt;

    fn app(https: bool) -> Router {
        Router::new().route("/", get(|| async { "hi" })).layer(
            axum::middleware::from_fn_with_state(https, security_headers),
        )
    }

    async fn get_root(app: Router) -> axum::response::Response {
        app.oneshot(HttpRequest::builder().uri("/").body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn middleware_sets_security_headers() {
        let resp = get_root(app(false)).await;
        let h = resp.headers();
        assert!(h.get(header::CONTENT_SECURITY_POLICY).is_some());
        assert_eq!(h.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert_eq!(h.get(header::X_FRAME_OPTIONS).unwrap(), "DENY");
        // The HTML shell must always revalidate so redeploys are picked up.
        assert_eq!(h.get(header::CACHE_CONTROL).unwrap(), "no-cache");
        // Plain-http dev: no HSTS.
        assert!(h.get(header::STRICT_TRANSPORT_SECURITY).is_none());
    }

    #[tokio::test]
    async fn hsts_is_sent_when_cookies_are_secure() {
        let resp = get_root(app(true)).await;
        assert_eq!(
            resp.headers()
                .get(header::STRICT_TRANSPORT_SECURITY)
                .unwrap(),
            "max-age=31536000"
        );
    }

    #[test]
    fn cache_control_policy_by_path() {
        assert_eq!(cache_control("/"), "no-cache");
        assert_eq!(cache_control("/login"), "no-cache"); // SPA route
        assert_eq!(
            cache_control("/assets/perkele-app-dxh123.js"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            cache_control("/wasm/perkele-app_bg.wasm"),
            "public, max-age=31536000, immutable"
        );
        assert_eq!(cache_control("/api/me"), "no-store");
    }
}

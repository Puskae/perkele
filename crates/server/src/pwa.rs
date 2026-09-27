//! Serves the PWA manifest, service worker, and icons as inline static routes.
//!
//! Text files are embedded via `include_str!` and the PNG icons via
//! `include_bytes!`, so there's no runtime file dependency and they're always
//! available.

use axum::Router;
use axum::body::Bytes;
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;

use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/manifest.webmanifest", get(manifest))
        .route("/sw.js", get(service_worker))
        .route("/icon-192.png", get(icon_192))
        .route("/icon-512.png", get(icon_512))
        .route("/icon-maskable-512.png", get(icon_maskable_512))
        .route("/apple-touch-icon.png", get(apple_touch_icon))
}

async fn manifest() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "application/manifest+json")],
        include_str!("../static/manifest.webmanifest"),
    )
}

async fn service_worker() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        include_str!("../static/sw.js"),
    )
}

async fn icon_192() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/png")],
        Bytes::from_static(include_bytes!("../static/icon-192.png")),
    )
}

async fn icon_512() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/png")],
        Bytes::from_static(include_bytes!("../static/icon-512.png")),
    )
}

async fn icon_maskable_512() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/png")],
        Bytes::from_static(include_bytes!("../static/icon-maskable-512.png")),
    )
}

async fn apple_touch_icon() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "image/png")],
        Bytes::from_static(include_bytes!("../static/apple-touch-icon.png")),
    )
}

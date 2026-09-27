//! A single error type for API handlers.
//!
//! Handlers return `Result<T, ApiError>`. `ApiError` knows how to turn itself
//! into an HTTP response, so handlers can use `?` freely. Crucially, internal
//! details (database errors) are logged server-side but never leaked to the
//! client — the caller only sees a generic message.

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use perkele_shared::ErrorResponse;

#[derive(Debug)]
pub enum ApiError {
    /// 400 — the request body failed validation.
    BadRequest(String),
    /// 401 — no valid session.
    Unauthorized,
    /// 401 — wrong username or password (kept generic so it doesn't reveal
    /// which of the two was wrong).
    LoginFailed,
    /// 403 — authenticated, but not allowed to do this.
    Forbidden,
    /// 404 — the requested resource does not exist.
    NotFound,
    /// 403 — `POST /api/setup` without the right one-time setup token.
    SetupTokenRejected,
    /// 409 — conflicts with current state (username taken, already set up, …).
    Conflict(String),
    /// 429 — too many attempts; the value is the `Retry-After` in seconds.
    TooManyRequests(u64),
    /// 500 — something unexpected; details are logged, not returned.
    Internal,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let retry_after = match self {
            ApiError::TooManyRequests(secs) => Some(secs.max(1)),
            _ => None,
        };
        let (status, message) = match self {
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Unauthorized => (
                StatusCode::UNAUTHORIZED,
                "Ei kirjauduttu sisään.".to_owned(),
            ),
            ApiError::LoginFailed => (
                StatusCode::UNAUTHORIZED,
                "Virheellinen käyttäjänimi tai salasana.".to_owned(),
            ),
            ApiError::Forbidden => (StatusCode::FORBIDDEN, "Ei sallittu.".to_owned()),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "Ei löydy.".to_owned()),
            ApiError::SetupTokenRejected => (
                StatusCode::FORBIDDEN,
                "Virheellinen asennustunnus.".to_owned(),
            ),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m),
            ApiError::TooManyRequests(_) => (
                StatusCode::TOO_MANY_REQUESTS,
                "Liian monta yritystä. Yritä hetken kuluttua uudelleen.".to_owned(),
            ),
            ApiError::Internal => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Palvelinvirhe.".to_owned(),
            ),
        };
        let mut resp = (status, Json(ErrorResponse { error: message })).into_response();
        if let Some(secs) = retry_after {
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from(secs));
        }
        resp
    }
}

/// Any database error becomes an opaque 500 — and is logged with full detail so
/// we can debug it without exposing internals to clients.
impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        tracing::error!("database error: {e:?}");
        ApiError::Internal
    }
}

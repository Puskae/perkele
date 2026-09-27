//! Session lifecycle and the `CurrentUser` request extractor.
//!
//! A session is a random token stored as a cookie on the client and as a *hash*
//! in the `sessions` table. Each request that needs authentication asks for a
//! `CurrentUser`; the extractor reads the cookie, looks the session up, checks
//! it hasn't expired, and loads the member — or rejects with 401.

use crate::AppState;
use crate::auth::{generate_token, hash_token};
use crate::db::Db;
use crate::error::ApiError;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use perkele_shared::auth::{Role, UserView};
use time::{Duration, OffsetDateTime};

/// Name of the session cookie.
pub const SESSION_COOKIE: &str = "perkele_session";

/// How long a session stays valid.
const SESSION_TTL: Duration = Duration::days(30);

/// The authenticated caller. Produced by the extractor; carries just enough to
/// authorize and scope queries (note `family_id` — every tenant query uses it).
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub user_id: i64,
    pub family_id: i64,
    pub role: Role,
    pub username: String,
    pub display_name: String,
}

impl CurrentUser {
    pub fn to_view(&self) -> UserView {
        UserView {
            id: self.user_id,
            family_id: self.family_id,
            username: self.username.clone(),
            display_name: self.display_name.clone(),
            role: self.role,
        }
    }
}

/// Create a session row for `user_id` and return the plaintext token to put in
/// the cookie. Only the hash is persisted.
pub async fn create(db: &Db, user_id: i64) -> Result<String, ApiError> {
    let token = generate_token();
    let token_hash = hash_token(&token);
    let now = OffsetDateTime::now_utc();
    let expires_at = now + SESSION_TTL;
    sqlx::query!(
        "INSERT INTO sessions (user_id, token_hash, expires_at, created_at) VALUES (?, ?, ?, ?)",
        user_id,
        token_hash,
        expires_at,
        now,
    )
    .execute(db)
    .await?;
    Ok(token)
}

/// Delete the session identified by `token`, if any. Used on logout. Idempotent.
pub async fn destroy(db: &Db, token: &str) -> Result<(), ApiError> {
    let token_hash = hash_token(token);
    sqlx::query!("DELETE FROM sessions WHERE token_hash = ?", token_hash)
        .execute(db)
        .await?;
    Ok(())
}

/// Look up the member behind a session token, or `None` if the token is
/// unknown or expired.
async fn lookup(db: &Db, token: &str) -> Result<Option<CurrentUser>, ApiError> {
    let token_hash = hash_token(token);
    // SQLite can't always prove NOT NULL through a join, so `name!: Type`
    // overrides force the column to be non-null and pick the Rust type.
    let row = sqlx::query!(
        r#"SELECT
               u.id           AS "user_id!: i64",
               u.family_id    AS "family_id!: i64",
               u.role         AS "role!: String",
               u.username     AS "username!: String",
               u.display_name AS "display_name!: String",
               s.expires_at   AS "expires_at!: OffsetDateTime"
           FROM sessions s
           JOIN users u ON u.id = s.user_id
           WHERE s.token_hash = ?"#,
        token_hash,
    )
    .fetch_optional(db)
    .await?;

    let Some(row) = row else {
        return Ok(None);
    };
    if row.expires_at < OffsetDateTime::now_utc() {
        return Ok(None);
    }
    let Some(role) = Role::from_db(&row.role) else {
        return Ok(None);
    };

    Ok(Some(CurrentUser {
        user_id: row.user_id,
        family_id: row.family_id,
        role,
        username: row.username,
        display_name: row.display_name,
    }))
}

/// Like `CurrentUser`, but the request only succeeds if the member is an admin
/// — otherwise 403. Handlers that take an `AdminUser` argument are therefore
/// admin-only by construction.
#[derive(Debug, Clone)]
pub struct AdminUser(pub CurrentUser);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Reuse the authentication extractor, then add the role check on top.
        let user = CurrentUser::from_request_parts(parts, state).await?;
        if user.role.is_admin() {
            Ok(AdminUser(user))
        } else {
            Err(ApiError::Forbidden)
        }
    }
}

/// Build the session cookie. `secure` is off only for local http development;
/// in production (HTTPS via Tailscale) it must be on.
pub fn cookie(token: String, secure: bool) -> Cookie<'static> {
    Cookie::build((SESSION_COOKIE, token))
        .http_only(true) // unreadable from JavaScript → mitigates XSS token theft
        .same_site(SameSite::Strict) // not sent on cross-site requests → CSRF defense
        .secure(secure)
        .path("/")
        .max_age(SESSION_TTL)
        .build()
}

/// A cookie that clears the session client-side (same attributes, empty value,
/// immediate expiry).
pub fn clearing_cookie(secure: bool) -> Cookie<'static> {
    let mut c = cookie(String::new(), secure);
    c.make_removal();
    c
}

/// Extractor: any handler taking a `CurrentUser` argument requires a valid
/// session, and gets 401 otherwise — authentication enforced by the type
/// system rather than a check you might forget to write.
impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let jar = CookieJar::from_headers(&parts.headers);
        let token = jar
            .get(SESSION_COOKIE)
            .map(|c| c.value().to_owned())
            .ok_or(ApiError::Unauthorized)?;
        lookup(&state.db, &token)
            .await?
            .ok_or(ApiError::Unauthorized)
    }
}

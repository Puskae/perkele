//! Perhechatti: one chat room per family. Paged messages, emoji reactions,
//! per-user read marker. Live updates ride the shared per-family SSE channel.

use crate::AppState;
use crate::error::ApiError;
use crate::session::CurrentUser;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use perkele_shared::auth::Role;
use perkele_shared::chat::{
    ChatMessage, ChatPage, MarkReadRequest, PAGE_SIZE, Reaction, SendMessageRequest,
    ToggleReactionRequest, validate_body, validate_emoji,
};
use serde::Deserialize;
use std::collections::HashMap;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/chat/messages", get(list).post(create))
        .route("/api/chat/messages/{id}", axum::routing::delete(delete))
        .route("/api/chat/reactions", axum::routing::put(toggle_reaction))
        .route("/api/chat/read", axum::routing::put(mark_read))
        .route("/api/chat/stream", get(crate::sync::sse_events))
}

fn now_rfc3339() -> Result<String, ApiError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)
}

#[derive(Deserialize)]
pub struct PageParams {
    before: Option<i64>,
    limit: Option<i64>,
}

/// GET /api/chat/messages — the newest page before `?before=` (or the very
/// latest when absent). Also carries latest/read/unread so ONE fetch answers
/// both "what's new" and "how many unread".
async fn list(
    user: CurrentUser,
    State(state): State<AppState>,
    Query(p): Query<PageParams>,
) -> Result<Json<ChatPage>, ApiError> {
    let limit = p.limit.unwrap_or(PAGE_SIZE).clamp(1, 100);
    // "No cursor" = everything; i64::MAX makes one query serve both cases.
    let before = p.before.unwrap_or(i64::MAX);
    let fetch = limit + 1; // one extra row = "has_more" probe
    let mut rows = sqlx::query!(
        r#"SELECT m.id           AS "id!: i64",
                  m.body         AS "body!: String",
                  m.author_id    AS "author_id!: i64",
                  u.display_name AS "author_name!: String",
                  m.created_at   AS "created_at!: String"
           FROM messages m
           JOIN users u ON u.id = m.author_id
           WHERE m.family_id = ? AND m.deleted_at IS NULL AND m.id < ?
           ORDER BY m.id DESC
           LIMIT ?"#,
        user.family_id,
        before,
        fetch,
    )
    .fetch_all(&state.db)
    .await?;
    let has_more = rows.len() as i64 > limit;
    rows.truncate(limit as usize);
    rows.reverse(); // ascending for direct top-to-bottom rendering

    // Reactions for the page in one query: the page is a contiguous id-DESC
    // slice, so BETWEEN its min/max id catches exactly its reactions (plus
    // rows for soft-deleted messages in the gap, which the map lookup below
    // simply never reads).
    let (min_id, max_id) = match (rows.first(), rows.last()) {
        (Some(f), Some(l)) => (f.id, l.id),
        _ => (0, -1), // empty page → empty BETWEEN
    };
    let reaction_rows = sqlx::query!(
        r#"SELECT r.message_id AS "message_id!: i64",
                  r.user_id    AS "user_id!: i64",
                  r.emoji      AS "emoji!: String"
           FROM message_reactions r
           JOIN messages m ON m.id = r.message_id
           WHERE m.family_id = ? AND r.message_id BETWEEN ? AND ?
           ORDER BY r.id"#,
        user.family_id,
        min_id,
        max_id,
    )
    .fetch_all(&state.db)
    .await?;
    let mut by_msg: HashMap<i64, Vec<Reaction>> = HashMap::new();
    for r in reaction_rows {
        by_msg.entry(r.message_id).or_default().push(Reaction {
            user_id: r.user_id,
            emoji: r.emoji,
        });
    }

    let latest_id = sqlx::query_scalar!(
        r#"SELECT COALESCE(MAX(id), 0) AS "latest!: i64" FROM messages
           WHERE family_id = ? AND deleted_at IS NULL"#,
        user.family_id,
    )
    .fetch_one(&state.db)
    .await?;
    let last_read_id = sqlx::query_scalar!(
        r#"SELECT last_read_id AS "last_read_id!: i64" FROM chat_reads WHERE user_id = ?"#,
        user.user_id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or(0);
    let unread_count = sqlx::query_scalar!(
        r#"SELECT COUNT(*) AS "n!: i64" FROM messages
           WHERE family_id = ? AND deleted_at IS NULL AND id > ?"#,
        user.family_id,
        last_read_id,
    )
    .fetch_one(&state.db)
    .await?;

    let messages = rows
        .into_iter()
        .map(|r| ChatMessage {
            id: r.id,
            body: r.body,
            author_id: r.author_id,
            author_name: r.author_name,
            created_at: r.created_at,
            reactions: by_msg.remove(&r.id).unwrap_or_default(),
        })
        .collect();
    Ok(Json(ChatPage {
        messages,
        has_more,
        latest_id,
        last_read_id,
        unread_count,
    }))
}

/// POST /api/chat/messages — any member. No push in v1 (spec decision):
/// SSE-only live updates.
async fn create(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<SendMessageRequest>,
) -> Result<(StatusCode, Json<ChatMessage>), ApiError> {
    validate_body(&req.body).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    let body = req.body.trim().to_owned();
    let now = now_rfc3339()?;
    let id = sqlx::query!(
        "INSERT INTO messages (family_id, author_id, body, created_at)
         VALUES (?, ?, ?, ?)",
        user.family_id,
        user.user_id,
        body,
        now,
    )
    .execute(&state.db)
    .await?
    .last_insert_rowid();
    crate::sync::poke(&state, user.family_id).await;
    Ok((
        StatusCode::CREATED,
        Json(ChatMessage {
            id,
            body,
            author_id: user.user_id,
            author_name: user.display_name.clone(),
            created_at: now,
            reactions: Vec::new(),
        }),
    ))
}

/// Load one message's author for the author-or-admin check; None = not in
/// this family (or already deleted) → caller turns that into 404, never
/// leaking that the id exists elsewhere.
async fn author_of(state: &AppState, family_id: i64, id: i64) -> Result<Option<i64>, ApiError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT author_id AS "author_id!: i64" FROM messages
           WHERE id = ? AND family_id = ? AND deleted_at IS NULL"#,
        id,
        family_id,
    )
    .fetch_optional(&state.db)
    .await?)
}

fn author_or_admin(user: &CurrentUser, author: i64) -> Result<(), ApiError> {
    if author != user.user_id && !matches!(user.role, Role::Admin) {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

/// DELETE /api/chat/messages/{id} — soft delete; author or admin.
async fn delete(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let author = author_of(&state, user.family_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    author_or_admin(&user, author)?;
    let now = now_rfc3339()?;
    sqlx::query!("UPDATE messages SET deleted_at = ? WHERE id = ?", now, id)
        .execute(&state.db)
        .await?;
    crate::sync::poke(&state, user.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// PUT /api/chat/reactions — toggle the caller's reaction. DELETE-first:
/// if a row vanished it was ON and is now off; otherwise insert turns it on.
async fn toggle_reaction(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<ToggleReactionRequest>,
) -> Result<StatusCode, ApiError> {
    validate_emoji(&req.emoji).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    // The message must be the caller's family's and alive — 404 otherwise.
    author_of(&state, user.family_id, req.message_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let removed = sqlx::query!(
        "DELETE FROM message_reactions WHERE message_id = ? AND user_id = ? AND emoji = ?",
        req.message_id,
        user.user_id,
        req.emoji,
    )
    .execute(&state.db)
    .await?
    .rows_affected();
    if removed == 0 {
        sqlx::query!(
            "INSERT INTO message_reactions (message_id, user_id, emoji) VALUES (?, ?, ?)",
            req.message_id,
            user.user_id,
            req.emoji,
        )
        .execute(&state.db)
        .await?;
    }
    crate::sync::poke(&state, user.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// PUT /api/chat/read — upsert the caller's marker; MAX() keeps it from ever
/// moving backward (stale tabs can't un-read). No poke: reading is per-user,
/// other family members don't need a refetch for it.
async fn mark_read(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<MarkReadRequest>,
) -> Result<StatusCode, ApiError> {
    sqlx::query!(
        "INSERT INTO chat_reads (user_id, family_id, last_read_id) VALUES (?, ?, ?)
         ON CONFLICT(user_id) DO UPDATE SET last_read_id = MAX(last_read_id, excluded.last_read_id)",
        user.user_id,
        user.family_id,
        req.last_read_id,
    )
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    /// Router + pool. In-memory SQLite lives in ONE connection: seeding must
    /// use this pool, never a second one.
    pub(super) async fn chat_app_with_db() -> (axum::Router, crate::db::Db) {
        let db = crate::db::test_pool().await;
        let state = AppState::for_test(db.clone());
        let app = crate::routes::router().merge(router()).with_state(state);
        (app, db)
    }

    pub(super) async fn chat_app() -> axum::Router {
        chat_app_with_db().await.0
    }

    pub(super) fn req(
        method: &str,
        path: &str,
        cookie: &str,
        body: Option<Value>,
    ) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, cookie);
        match body {
            Some(v) => {
                b = b.header(header::CONTENT_TYPE, "application/json");
                b.body(Body::from(v.to_string())).unwrap()
            }
            None => b.body(Body::empty()).unwrap(),
        }
    }

    pub(super) fn cookie_pair(resp: &axum::response::Response) -> String {
        resp.headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned()
    }

    pub(super) async fn json_body(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    pub(super) async fn setup_admin(app: &axum::Router) -> String {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/setup")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "family_name": "Virtanen", "username": "mikko",
                                "display_name": "Mikko", "password": "hunter2!" })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        cookie_pair(&resp)
    }

    /// Admin mints a member invite, "matti" redeems it; returns matti's cookie.
    pub(super) async fn join_member(app: &axum::Router, admin: &str) -> String {
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/family/invites",
                admin,
                Some(json!({ "role": "member" })),
            ))
            .await
            .unwrap();
        let code = json_body(resp).await["code"].as_str().unwrap().to_owned();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/redeem")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "code": code, "username": "matti",
                                "display_name": "Matti", "password": "hunter2!" })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        cookie_pair(&resp)
    }

    pub(super) async fn login(app: &axum::Router, username: &str, password: &str) -> String {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "username": username, "password": password }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        cookie_pair(&resp)
    }

    /// A second family seeded directly (runtime queries need no sqlx cache),
    /// to prove cross-family isolation.
    pub(super) async fn seed_other_family(db: &crate::db::Db) {
        let now = OffsetDateTime::now_utc();
        let fid = sqlx::query("INSERT INTO families (name, created_at) VALUES (?, ?)")
            .bind("Other")
            .bind(now)
            .execute(db)
            .await
            .unwrap()
            .last_insert_rowid();
        let hash = crate::auth::hash_password("password1").unwrap();
        sqlx::query(
            "INSERT INTO users (family_id, username, display_name, pw_hash, role, created_at)
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(fid)
        .bind("outsider")
        .bind("Outsider")
        .bind(hash)
        .bind("member")
        .bind(now)
        .execute(db)
        .await
        .unwrap();
    }

    /// Send one message, return its id.
    pub(super) async fn send(app: &axum::Router, cookie: &str, body: &str) -> i64 {
        json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/chat/messages",
                    cookie,
                    Some(json!({ "body": body })),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap()
    }

    #[tokio::test]
    async fn send_then_latest_page_with_unread_math() {
        let app = chat_app().await;
        let admin = setup_admin(&app).await;
        let first = send(&app, &admin, "Kuka söi jäätelön?").await;
        let second = send(&app, &admin, "Se olin minä 😂").await;

        let page = json_body(
            app.oneshot(req("GET", "/api/chat/messages", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        let msgs = page["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        // Ascending: oldest first, so the client renders top-to-bottom.
        assert_eq!(msgs[0]["id"].as_i64().unwrap(), first);
        assert_eq!(msgs[0]["body"], "Kuka söi jäätelön?");
        assert_eq!(msgs[0]["author_name"], "Mikko");
        assert_eq!(msgs[1]["id"].as_i64().unwrap(), second);
        assert_eq!(page["has_more"], false);
        assert_eq!(page["latest_id"].as_i64().unwrap(), second);
        assert_eq!(page["last_read_id"].as_i64().unwrap(), 0);
        assert_eq!(page["unread_count"].as_i64().unwrap(), 2);
    }

    #[tokio::test]
    async fn paging_walks_backward_until_exhausted() {
        let app = chat_app().await;
        let admin = setup_admin(&app).await;
        let mut ids = Vec::new();
        for i in 1..=5 {
            ids.push(send(&app, &admin, &format!("viesti {i}")).await);
        }

        // Latest page of 2 → the two newest, more behind.
        let page = json_body(
            app.clone()
                .oneshot(req("GET", "/api/chat/messages?limit=2", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        let msgs = page["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["id"].as_i64().unwrap(), ids[3]);
        assert_eq!(msgs[1]["id"].as_i64().unwrap(), ids[4]);
        assert_eq!(page["has_more"], true);

        // Page before the oldest loaded id → the previous two.
        let before = ids[3];
        let page = json_body(
            app.clone()
                .oneshot(req(
                    "GET",
                    &format!("/api/chat/messages?limit=2&before={before}"),
                    &admin,
                    None,
                ))
                .await
                .unwrap(),
        )
        .await;
        let msgs = page["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["id"].as_i64().unwrap(), ids[1]);
        assert_eq!(msgs[1]["id"].as_i64().unwrap(), ids[2]);
        assert_eq!(page["has_more"], true);

        // Final page: one message, nothing beyond it.
        let before = ids[1];
        let page = json_body(
            app.clone()
                .oneshot(req(
                    "GET",
                    &format!("/api/chat/messages?limit=2&before={before}"),
                    &admin,
                    None,
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(page["messages"].as_array().unwrap().len(), 1);
        assert_eq!(page["has_more"], false);

        // Beyond the beginning: empty page, has_more false.
        let before = ids[0];
        let page = json_body(
            app.clone()
                .oneshot(req(
                    "GET",
                    &format!("/api/chat/messages?limit=2&before={before}"),
                    &admin,
                    None,
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(page["messages"].as_array().unwrap().len(), 0);
        assert_eq!(page["has_more"], false);
    }

    #[tokio::test]
    async fn author_deletes_own_member_cannot_delete_anothers_admin_can() {
        let app = chat_app().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;
        let admins_msg = send(&app, &admin, "Adminin viesti").await;
        let members_msg = send(&app, &member, "Jäsenen viesti").await;

        // Member deleting another's message → 403.
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chat/messages/{admins_msg}"),
                &member,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Author deletes their own → 204; admin deletes anyone's → 204.
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chat/messages/{members_msg}"),
                &member,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chat/messages/{admins_msg}"),
                &admin,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        // Soft delete hides both; latest_id/unread follow suit.
        let page = json_body(
            app.oneshot(req("GET", "/api/chat/messages", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(page["messages"].as_array().unwrap().len(), 0);
        assert_eq!(page["latest_id"].as_i64().unwrap(), 0);
        assert_eq!(page["unread_count"].as_i64().unwrap(), 0);
    }

    #[tokio::test]
    async fn delete_cross_family_is_404_not_403() {
        // 404 (not 403) so ids don't leak across families.
        let (app, db) = chat_app_with_db().await;
        let admin = setup_admin(&app).await;
        let id = send(&app, &admin, "Meidän juttu").await;
        seed_other_family(&db).await;
        let outsider = login(&app, "outsider", "password1").await;
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chat/messages/{id}"),
                &outsider,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // And the outsider's room is empty.
        let page = json_body(
            app.oneshot(req("GET", "/api/chat/messages", &outsider, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(page["messages"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn reaction_toggles_on_and_off() {
        let app = chat_app().await;
        let admin = setup_admin(&app).await;
        let id = send(&app, &admin, "Reagoikaa tähän").await;

        // Toggle on → the page shows it with the reactor's user id.
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/chat/reactions",
                &admin,
                Some(json!({ "message_id": id, "emoji": "👍" })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let page = json_body(
            app.clone()
                .oneshot(req("GET", "/api/chat/messages", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        let reactions = page["messages"][0]["reactions"].as_array().unwrap();
        assert_eq!(reactions.len(), 1);
        assert_eq!(reactions[0]["emoji"], "👍");

        // Same PUT again → toggled off.
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/chat/reactions",
                &admin,
                Some(json!({ "message_id": id, "emoji": "👍" })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let page = json_body(
            app.oneshot(req("GET", "/api/chat/messages", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            page["messages"][0]["reactions"].as_array().unwrap().len(),
            0
        );
    }

    #[tokio::test]
    async fn reaction_rejects_unknown_emoji_and_foreign_message() {
        let (app, db) = chat_app_with_db().await;
        let admin = setup_admin(&app).await;
        let id = send(&app, &admin, "Meidän viesti").await;

        // Outside the fixed set → 400.
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/chat/reactions",
                &admin,
                Some(json!({ "message_id": id, "emoji": "🦄" })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // Another family's message → 404.
        seed_other_family(&db).await;
        let outsider = login(&app, "outsider", "password1").await;
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/chat/reactions",
                &outsider,
                Some(json!({ "message_id": id, "emoji": "👍" })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn read_marker_moves_forward_only() {
        let app = chat_app().await;
        let admin = setup_admin(&app).await;
        let first = send(&app, &admin, "eka").await;
        let second = send(&app, &admin, "toka").await;

        // Mark everything read → unread drops to 0.
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/chat/read",
                &admin,
                Some(json!({ "last_read_id": second })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let page = json_body(
            app.clone()
                .oneshot(req("GET", "/api/chat/messages", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(page["last_read_id"].as_i64().unwrap(), second);
        assert_eq!(page["unread_count"].as_i64().unwrap(), 0);

        // A stale/smaller marker must NOT move it backward.
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/chat/read",
                &admin,
                Some(json!({ "last_read_id": first })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let page = json_body(
            app.oneshot(req("GET", "/api/chat/messages", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(page["last_read_id"].as_i64().unwrap(), second);
        assert_eq!(page["unread_count"].as_i64().unwrap(), 0);
    }

    #[tokio::test]
    async fn body_validation_rejects_empty_and_too_long() {
        let app = chat_app().await;
        let admin = setup_admin(&app).await;
        for bad in [json!({"body": "   "}), json!({"body": "x".repeat(2001)})] {
            let resp = app
                .clone()
                .oneshot(req("POST", "/api/chat/messages", &admin, Some(bad)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }
    }
}

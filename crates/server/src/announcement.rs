//! The announcements board ("fridge door"): list, post, edit, pin, delete.
//! Any member posts; authors edit/delete their own; admins pin and moderate.

use crate::AppState;
use crate::error::ApiError;
use crate::session::{AdminUser, CurrentUser};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use perkele_shared::announcement::{
    Announcement, SaveAnnouncementRequest, SetPinnedRequest, validate_body,
};
use perkele_shared::auth::Role;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/announcements", get(list).post(create))
        .route(
            "/api/announcements/{id}",
            axum::routing::put(update).delete(delete),
        )
        .route(
            "/api/announcements/{id}/pinned",
            axum::routing::put(set_pinned),
        )
        .route("/api/announcements/stream", get(crate::sync::sse_events))
}

fn now_rfc3339() -> Result<String, ApiError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)
}

/// Announcements have no title; use a short prefix of the body as the audit
/// label so "Poisti tiedotteen 'Perjantaina saunotaan…'" stays readable.
fn body_label(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= 40 {
        trimmed.to_owned()
    } else {
        let head: String = trimmed.chars().take(40).collect();
        format!("{head}…")
    }
}

/// GET /api/announcements — pinned first, then newest first.
async fn list(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Announcement>>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT a.id         AS "id!: i64",
                  a.body       AS "body!: String",
                  a.pinned     AS "pinned!: i64",
                  a.created_by AS "created_by!: i64",
                  u.display_name AS "author_name!: String",
                  a.created_at AS "created_at!: String",
                  a.updated_at AS "updated_at!: String"
           FROM announcements a
           JOIN users u ON u.id = a.created_by
           WHERE a.family_id = ? AND a.deleted_at IS NULL
           ORDER BY a.pinned DESC, a.created_at DESC"#,
        user.family_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| Announcement {
                id: r.id,
                body: r.body,
                pinned: r.pinned != 0,
                created_by: r.created_by,
                author_name: r.author_name,
                created_at: r.created_at,
                updated_at: r.updated_at,
            })
            .collect(),
    ))
}

/// POST /api/announcements — any member. (6A-4 adds the push fan-out here.)
async fn create(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<SaveAnnouncementRequest>,
) -> Result<(StatusCode, Json<Announcement>), ApiError> {
    validate_body(&req.body).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    let body = req.body.trim().to_owned();
    let now = now_rfc3339()?;
    let id = sqlx::query!(
        "INSERT INTO announcements (family_id, body, pinned, created_by, created_at, updated_at)
         VALUES (?, ?, 0, ?, ?, ?)",
        user.family_id,
        body,
        user.user_id,
        now,
        now,
    )
    .execute(&state.db)
    .await?
    .last_insert_rowid();
    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "announcement",
        Some(id),
        "create",
        &body_label(&body),
    )
    .await?;
    crate::sync::poke(&state, user.family_id).await;
    // Push in the background: the poster shouldn't wait on N web-push
    // round-trips, and a push failure must never fail the post itself.
    spawn_push(
        state.clone(),
        user.family_id,
        user.user_id,
        user.display_name.clone(),
        body.clone(),
    );
    Ok((
        StatusCode::CREATED,
        Json(Announcement {
            id,
            body,
            pinned: false,
            created_by: user.user_id,
            author_name: user.display_name.clone(),
            created_at: now.clone(),
            updated_at: now,
        }),
    ))
}

/// Fan out on the shared transport, off the request path. Errors are
/// logged only — the board post already succeeded.
fn spawn_push(state: AppState, family_id: i64, author_id: i64, author_name: String, body: String) {
    tokio::spawn(async move {
        if let Err(e) = crate::push::push_announcement(
            &state.db,
            // Arc<WebPushPusher> → &WebPushPusher (see push::test_notification).
            &*state.pusher,
            family_id,
            author_id,
            &author_name,
            &body,
        )
        .await
        {
            tracing::warn!("announcement push failed: {e:?}");
        }
    });
}

/// Load one row's author for the author-or-admin checks; None = not in this
/// family (or deleted) → the caller turns that into 404, never leaking that
/// the id exists elsewhere.
async fn author_of(state: &AppState, family_id: i64, id: i64) -> Result<Option<i64>, ApiError> {
    Ok(sqlx::query_scalar!(
        r#"SELECT created_by AS "created_by!: i64" FROM announcements
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

/// PUT /api/announcements/{id} — edit body; author or admin.
async fn update(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SaveAnnouncementRequest>,
) -> Result<StatusCode, ApiError> {
    validate_body(&req.body).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    let author = author_of(&state, user.family_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    author_or_admin(&user, author)?;
    let body = req.body.trim().to_owned();
    let now = now_rfc3339()?;
    sqlx::query!(
        "UPDATE announcements SET body = ?, updated_at = ? WHERE id = ?",
        body,
        now,
        id,
    )
    .execute(&state.db)
    .await?;
    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "announcement",
        Some(id),
        "update",
        &body_label(&body),
    )
    .await?;
    crate::sync::poke(&state, user.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// PUT /api/announcements/{id}/pinned — admin only (the extractor 403s).
async fn set_pinned(
    admin: AdminUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetPinnedRequest>,
) -> Result<StatusCode, ApiError> {
    let pinned = req.pinned as i64;
    let now = now_rfc3339()?;
    let changed = sqlx::query!(
        "UPDATE announcements SET pinned = ?, updated_at = ?
         WHERE id = ? AND family_id = ? AND deleted_at IS NULL",
        pinned,
        now,
        id,
        admin.0.family_id,
    )
    .execute(&state.db)
    .await?
    .rows_affected();
    if changed == 0 {
        return Err(ApiError::NotFound);
    }
    // Pin/unpin is an edit of the announcement, not a new body — the audit
    // label is still the (current, unchanged) body text.
    let body = sqlx::query_scalar!(
        r#"SELECT body AS "body!: String" FROM announcements WHERE id = ?"#,
        id,
    )
    .fetch_one(&state.db)
    .await?;
    crate::audit::record(
        &state.db,
        admin.0.family_id,
        admin.0.user_id,
        "announcement",
        Some(id),
        "update",
        &body_label(&body),
    )
    .await?;
    crate::sync::poke(&state, admin.0.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/announcements/{id} — soft delete; author or admin.
async fn delete(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let author = author_of(&state, user.family_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    author_or_admin(&user, author)?;
    // Capture the body BEFORE the soft delete so the audit label still has it.
    let body = sqlx::query_scalar!(
        r#"SELECT body AS "body!: String" FROM announcements WHERE id = ?"#,
        id,
    )
    .fetch_one(&state.db)
    .await?;
    let now = now_rfc3339()?;
    sqlx::query!(
        "UPDATE announcements SET deleted_at = ? WHERE id = ?",
        now,
        id,
    )
    .execute(&state.db)
    .await?;
    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "announcement",
        Some(id),
        "delete",
        &body_label(&body),
    )
    .await?;
    crate::sync::poke(&state, user.family_id).await;
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
    async fn board_app_with_db() -> (axum::Router, crate::db::Db) {
        let db = crate::db::test_pool().await;
        let state = AppState::for_test(db.clone());
        let app = crate::routes::router().merge(router()).with_state(state);
        (app, db)
    }

    async fn board_app() -> axum::Router {
        board_app_with_db().await.0
    }

    fn req(method: &str, path: &str, cookie: &str, body: Option<Value>) -> Request<Body> {
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

    fn cookie_pair(resp: &axum::response::Response) -> String {
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

    async fn json_body(resp: axum::response::Response) -> Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn setup_admin(app: &axum::Router) -> String {
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
    async fn join_member(app: &axum::Router, admin: &str) -> String {
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

    async fn login(app: &axum::Router, username: &str, password: &str) -> String {
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
    async fn seed_other_family(db: &crate::db::Db) {
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

    #[tokio::test]
    async fn create_then_list_orders_pinned_first() {
        let app = board_app().await;
        let admin = setup_admin(&app).await;
        // Two posts; then pin the FIRST (older) one.
        let first = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/announcements",
                    &admin,
                    Some(json!({"body": "Vanhempi ilmoitus"})),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        app.clone()
            .oneshot(req(
                "POST",
                "/api/announcements",
                &admin,
                Some(json!({"body": "Uudempi ilmoitus"})),
            ))
            .await
            .unwrap();
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/announcements/{first}/pinned"),
                &admin,
                Some(json!({"pinned": true})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let body = json_body(
            app.oneshot(req("GET", "/api/announcements", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // Pinned wins over recency; author_name is joined in.
        assert_eq!(arr[0]["body"], "Vanhempi ilmoitus");
        assert_eq!(arr[0]["pinned"], true);
        assert_eq!(arr[0]["author_name"], "Mikko");
        assert_eq!(arr[1]["body"], "Uudempi ilmoitus");
    }

    #[tokio::test]
    async fn member_cannot_pin_or_delete_anothers_post_admin_can() {
        let app = board_app().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;
        let id = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/announcements",
                    &admin,
                    Some(json!({"body": "Adminin ilmoitus"})),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();

        // Member: pin → 403 (AdminUser extractor), delete another's → 403.
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/announcements/{id}/pinned"),
                &member,
                Some(json!({"pinned": true})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/announcements/{id}"),
                &member,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // Admin deletes anyone's; the list is then empty (soft delete hides it).
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/announcements/{id}"),
                &admin,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let body = json_body(
            app.oneshot(req("GET", "/api/announcements", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn author_can_edit_and_delete_own_post() {
        let app = board_app().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;
        let id = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/announcements",
                    &member,
                    Some(json!({"body": "Jäsenen ilmoitus"})),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/announcements/{id}"),
                &member,
                Some(json!({"body": "Korjattu teksti"})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let body = json_body(
            app.clone()
                .oneshot(req("GET", "/api/announcements", &member, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body[0]["body"], "Korjattu teksti");
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/announcements/{id}"),
                &member,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn body_validation_rejects_empty_and_too_long() {
        let app = board_app().await;
        let admin = setup_admin(&app).await;
        for bad in [json!({"body": "   "}), json!({"body": "x".repeat(2001)})] {
            let resp = app
                .clone()
                .oneshot(req("POST", "/api/announcements", &admin, Some(bad)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn announcement_mutations_write_audit_rows() {
        let (app, db) = board_app_with_db().await;
        let admin = setup_admin(&app).await;

        let created = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/announcements",
                    &admin,
                    Some(json!({"body": "Perjantaina saunotaan"})),
                ))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_i64().unwrap();
        app.clone()
            .oneshot(req(
                "PUT",
                &format!("/api/announcements/{id}/pinned"),
                &admin,
                Some(json!({"pinned": true})),
            ))
            .await
            .unwrap();
        app.clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/announcements/{id}"),
                &admin,
                None,
            ))
            .await
            .unwrap();

        let rows = sqlx::query!(
            r#"SELECT entity AS "entity!", op AS "op!", label AS "label!" FROM audit_log ORDER BY id"#
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            (
                rows[0].entity.as_str(),
                rows[0].op.as_str(),
                rows[0].label.as_str()
            ),
            ("announcement", "create", "Perjantaina saunotaan")
        );
        assert_eq!(rows[1].op, "update"); // pin
        assert_eq!(rows[2].op, "delete");
    }

    #[tokio::test]
    async fn other_family_gets_404_not_403() {
        // 404 (not 403) so ids don't leak across families.
        let (app, db) = board_app_with_db().await;
        let admin = setup_admin(&app).await;
        let id = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/announcements",
                    &admin,
                    Some(json!({"body": "Meidän juttu"})),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        seed_other_family(&db).await;
        let outsider = login(&app, "outsider", "password1").await;
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/announcements/{id}"),
                &outsider,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        // And the outsider's list doesn't contain it.
        let body = json_body(
            app.oneshot(req("GET", "/api/announcements", &outsider, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), 0);
    }
}

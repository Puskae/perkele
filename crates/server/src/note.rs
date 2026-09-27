//! Muistiot (notes): topics of mixed text + checklist blocks. Any member
//! reads and ticks checklist blocks (the collaborative part); rewriting or
//! deleting a topic is author-or-admin.
//! Checkbox state is SET per block (idempotent) so two phones can tick
//! simultaneously; full edits replace all blocks in one transaction.

use crate::AppState;
use crate::error::ApiError;
use crate::session::CurrentUser;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use perkele_shared::note::{
    BlockKind, CreateTopicRequest, NoteBlock, NoteTopic, NoteTopicSummary, SaveTopicRequest,
    SetBlockCheckedRequest, validate_blocks, validate_topic_title,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/notes", get(list).post(create))
        .route("/api/notes/{id}", get(get_topic).put(update).delete(delete))
        .route(
            "/api/notes/{id}/blocks/{block_id}/checked",
            axum::routing::put(set_block_checked),
        )
}

fn now_rfc3339() -> Result<String, ApiError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)
}

/// GET /api/notes — summaries with checklist progress, newest-updated first.
async fn list(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<NoteTopicSummary>>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT t.id         AS "id!: i64",
                  t.title      AS "title!: String",
                  t.updated_at AS "updated_at!: String",
                  COALESCE(SUM(CASE WHEN b.kind = 'check' THEN 1 ELSE 0 END), 0)
                      AS "check_total!: i64",
                  COALESCE(SUM(CASE WHEN b.kind = 'check' AND b.checked = 1 THEN 1 ELSE 0 END), 0)
                      AS "check_done!: i64"
           FROM note_topics t
           LEFT JOIN note_blocks b ON b.topic_id = t.id
           WHERE t.family_id = ? AND t.deleted_at IS NULL
           GROUP BY t.id
           ORDER BY t.updated_at DESC"#,
        user.family_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|r| NoteTopicSummary {
                id: r.id,
                title: r.title,
                check_done: r.check_done,
                check_total: r.check_total,
                updated_at: r.updated_at,
            })
            .collect(),
    ))
}

/// POST /api/notes — create an empty topic; any member.
async fn create(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<CreateTopicRequest>,
) -> Result<(StatusCode, Json<NoteTopic>), ApiError> {
    validate_topic_title(&req.title).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    let title = req.title.trim().to_owned();
    let now = now_rfc3339()?;
    let id = sqlx::query!(
        "INSERT INTO note_topics (family_id, title, created_by, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?)",
        user.family_id,
        title,
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
        "note",
        Some(id),
        "create",
        &title,
    )
    .await?;
    crate::sync::poke(&state, user.family_id).await;
    Ok((
        StatusCode::CREATED,
        Json(NoteTopic {
            id,
            title,
            created_by: user.user_id,
            updated_at: now,
            blocks: vec![],
        }),
    ))
}

/// Load the topic header (family-scoped, alive). None → caller's 404, so ids
/// never leak across families.
async fn topic_header(
    state: &AppState,
    family_id: i64,
    id: i64,
) -> Result<Option<(String, i64, String)>, ApiError> {
    Ok(sqlx::query!(
        r#"SELECT title AS "title!: String",
                  created_by AS "created_by!: i64",
                  updated_at AS "updated_at!: String"
           FROM note_topics
           WHERE id = ? AND family_id = ? AND deleted_at IS NULL"#,
        id,
        family_id,
    )
    .fetch_optional(&state.db)
    .await?
    .map(|r| (r.title, r.created_by, r.updated_at)))
}

async fn load_blocks(state: &AppState, topic_id: i64) -> Result<Vec<NoteBlock>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT id AS "id!: i64", kind AS "kind!: String",
                  content AS "content!: String", checked AS "checked!: i64"
           FROM note_blocks WHERE topic_id = ? ORDER BY position"#,
        topic_id,
    )
    .fetch_all(&state.db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| NoteBlock {
            id: r.id,
            kind: BlockKind::from_db(&r.kind),
            content: r.content,
            checked: r.checked != 0,
        })
        .collect())
}

/// GET /api/notes/{id} — full topic with blocks.
async fn get_topic(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<NoteTopic>, ApiError> {
    let (title, created_by, updated_at) = topic_header(&state, user.family_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let blocks = load_blocks(&state, id).await?;
    Ok(Json(NoteTopic {
        id,
        title,
        created_by,
        updated_at,
        blocks,
    }))
}

/// PUT /api/notes/{id} — replace title AND all blocks in one transaction.
/// Author-or-admin: replace-all can wipe the whole note, so it's the same
/// rule as delete. (Ticking a checklist block via `set_block_checked` stays
/// open to every member — that's the collaborative part.) Returns the fresh
/// topic so the client can swap state without a refetch (block ids are NEW
/// every time).
async fn update(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SaveTopicRequest>,
) -> Result<Json<NoteTopic>, ApiError> {
    validate_topic_title(&req.title).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    validate_blocks(&req.blocks).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    // Existence + ownership check outside the tx keeps the 404/403 paths cheap.
    let (_, created_by, _) = topic_header(&state, user.family_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !crate::grocery::can_modify(&user, created_by) {
        return Err(ApiError::Forbidden);
    }

    let title = req.title.trim().to_owned();
    let now = now_rfc3339()?;
    let mut tx = state.db.begin().await?;
    sqlx::query!("DELETE FROM note_blocks WHERE topic_id = ?", id)
        .execute(&mut *tx)
        .await?;
    for (i, b) in req.blocks.iter().enumerate() {
        let kind = b.kind.as_str();
        // A text row can't be "checked" — normalize rather than reject.
        let checked = (b.kind == BlockKind::Check && b.checked) as i64;
        let pos = i as i64;
        sqlx::query!(
            "INSERT INTO note_blocks (topic_id, family_id, kind, content, checked, position)
             VALUES (?, ?, ?, ?, ?, ?)",
            id,
            user.family_id,
            kind,
            b.content,
            checked,
            pos,
        )
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query!(
        "UPDATE note_topics SET title = ?, updated_at = ? WHERE id = ?",
        title,
        now,
        id,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;

    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "note",
        Some(id),
        "update",
        &title,
    )
    .await?;
    crate::sync::poke(&state, user.family_id).await;
    let blocks = load_blocks(&state, id).await?;
    let (_, created_by, updated_at) = topic_header(&state, user.family_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(NoteTopic {
        id,
        title,
        created_by,
        updated_at,
        blocks,
    }))
}

/// PUT /api/notes/{id}/blocks/{block_id}/checked — idempotent SET, the
/// grocery flaky-network pattern. Only 'check' blocks of a live topic match;
/// everything else (text block, foreign family, deleted topic) is 404.
async fn set_block_checked(
    user: CurrentUser,
    State(state): State<AppState>,
    Path((id, block_id)): Path<(i64, i64)>,
    Json(req): Json<SetBlockCheckedRequest>,
) -> Result<StatusCode, ApiError> {
    let checked = req.checked as i64;
    let changed = sqlx::query!(
        "UPDATE note_blocks SET checked = ?
         WHERE id = ? AND topic_id = ? AND family_id = ? AND kind = 'check'
           AND EXISTS (SELECT 1 FROM note_topics t
                       WHERE t.id = note_blocks.topic_id AND t.deleted_at IS NULL)",
        checked,
        block_id,
        id,
        user.family_id,
    )
    .execute(&state.db)
    .await?
    .rows_affected();
    if changed == 0 {
        return Err(ApiError::NotFound);
    }
    // Ticking counts as activity: bump the topic so the list sorts it up.
    let now = now_rfc3339()?;
    sqlx::query!(
        "UPDATE note_topics SET updated_at = ? WHERE id = ?",
        now,
        id
    )
    .execute(&state.db)
    .await?;
    crate::sync::poke(&state, user.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/notes/{id} — soft delete; author or admin (announcements rule).
async fn delete(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let (title, created_by, _) = topic_header(&state, user.family_id, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !crate::grocery::can_modify(&user, created_by) {
        return Err(ApiError::Forbidden);
    }
    let now = now_rfc3339()?;
    sqlx::query!(
        "UPDATE note_topics SET deleted_at = ? WHERE id = ?",
        now,
        id
    )
    .execute(&state.db)
    .await?;
    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "note",
        Some(id),
        "delete",
        &title,
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
    use sqlx::Row;
    use tower::ServiceExt;

    /// Router + pool. In-memory SQLite lives in ONE connection: seeding must
    /// use this pool, never a second one.
    async fn notes_app_with_db() -> (axum::Router, crate::db::Db) {
        let db = crate::db::test_pool().await;
        let state = AppState::for_test(db.clone());
        let app = crate::routes::router().merge(router()).with_state(state);
        (app, db)
    }

    async fn notes_app() -> axum::Router {
        notes_app_with_db().await.0
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
    async fn create_then_list_shows_counts_and_recency_order() {
        let app = notes_app().await;
        let admin = setup_admin(&app).await;
        let first = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/notes",
                    &admin,
                    Some(json!({"title": "Häälista"})),
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
                "POST",
                "/api/notes",
                &admin,
                Some(json!({"title": "Mökkilista"})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let body = json_body(
            app.clone()
                .oneshot(req("GET", "/api/notes", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        // Newest-updated first; a fresh topic has zero checkboxes.
        assert_eq!(arr[0]["title"], "Mökkilista");
        assert_eq!(arr[1]["id"], first);
        assert_eq!(arr[0]["check_total"], 0);
        assert_eq!(arr[0]["check_done"], 0);
    }

    #[tokio::test]
    async fn get_returns_topic_with_empty_blocks() {
        let app = notes_app().await;
        let admin = setup_admin(&app).await;
        let id = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/notes",
                    &admin,
                    Some(json!({"title": "Häälista"})),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let body = json_body(
            app.clone()
                .oneshot(req("GET", &format!("/api/notes/{id}"), &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["title"], "Häälista");
        assert_eq!(body["blocks"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn title_validation_rejects_empty_and_too_long() {
        let app = notes_app().await;
        let admin = setup_admin(&app).await;
        for bad in [json!({"title": "  "}), json!({"title": "x".repeat(201)})] {
            let resp = app
                .clone()
                .oneshot(req("POST", "/api/notes", &admin, Some(bad)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn other_family_gets_404_on_get() {
        let (app, db) = notes_app_with_db().await;
        let admin = setup_admin(&app).await;
        let id = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/notes",
                    &admin,
                    Some(json!({"title": "Meidän lista"})),
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
            .oneshot(req("GET", &format!("/api/notes/{id}"), &outsider, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn put_replaces_blocks_of_own_topic() {
        let app = notes_app().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;
        let id = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/notes",
                    &member,
                    Some(json!({"title": "Häälista"})),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        // The MEMBER edits their own topic.
        let body = json_body(
            app.clone()
                .oneshot(req(
                    "PUT",
                    &format!("/api/notes/{id}"),
                    &member,
                    Some(json!({"title": "Häälista v2", "blocks": [
                        {"kind": "text", "content": "Muista pappi!", "checked": false},
                        {"kind": "check", "content": "tilaa puvut", "checked": false},
                        {"kind": "check", "content": "varaa kampaaja", "checked": true},
                    ]})),
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["title"], "Häälista v2");
        let blocks = body["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[2]["checked"], true); // checked state carried by the request
        // Summary counts reflect the new blocks.
        let list = json_body(
            app.clone()
                .oneshot(req("GET", "/api/notes", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(list[0]["check_total"], 2);
        assert_eq!(list[0]["check_done"], 1);
    }

    /// Replacing a note's content is author-or-admin: a member can't wipe the
    /// admin's note, but CAN still tick its checklist (collaborative, stays
    /// open); the admin may rewrite the member's note.
    #[tokio::test]
    async fn put_is_author_or_admin_but_ticking_is_open() {
        let app = notes_app().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;
        let create = |who: String, title: &'static str| {
            let app = app.clone();
            async move {
                json_body(
                    app.oneshot(req(
                        "POST",
                        "/api/notes",
                        &who,
                        Some(json!({"title": title})),
                    ))
                    .await
                    .unwrap(),
                )
                .await["id"]
                    .as_i64()
                    .unwrap()
            }
        };
        let admins = create(admin.clone(), "Adminin lista").await;
        let body = json_body(
            app.clone()
                .oneshot(req(
                    "PUT",
                    &format!("/api/notes/{admins}"),
                    &admin,
                    Some(json!({"title": "Adminin lista", "blocks": [
                        {"kind": "check", "content": "osta kakku", "checked": false},
                    ]})),
                ))
                .await
                .unwrap(),
        )
        .await;
        let block = body["blocks"][0]["id"].as_i64().unwrap();

        let wipe = json!({"title": "tyhjä", "blocks": []});
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/notes/{admins}"),
                &member,
                Some(wipe.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/notes/{admins}/blocks/{block}/checked"),
                &member,
                Some(json!({"checked": true})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let topic = json_body(
            app.clone()
                .oneshot(req("GET", &format!("/api/notes/{admins}"), &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(topic["title"], "Adminin lista");
        assert_eq!(topic["blocks"][0]["checked"], true);

        let members = create(member.clone(), "Matin lista").await;
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/notes/{members}"),
                &admin,
                Some(wipe),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn checkbox_set_is_idempotent_and_scoped() {
        let app = notes_app().await;
        let admin = setup_admin(&app).await;
        let id = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/notes",
                    &admin,
                    Some(json!({"title": "Lista"})),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let body = json_body(
            app.clone()
                .oneshot(req(
                    "PUT",
                    &format!("/api/notes/{id}"),
                    &admin,
                    Some(json!({"title": "Lista", "blocks": [
                        {"kind": "check", "content": "osta sormukset", "checked": false},
                        {"kind": "text", "content": "prose", "checked": false},
                    ]})),
                ))
                .await
                .unwrap(),
        )
        .await;
        let check_id = body["blocks"][0]["id"].as_i64().unwrap();
        let text_id = body["blocks"][1]["id"].as_i64().unwrap();

        // Double "true" stays true (set semantics — a late duplicate can't flip back).
        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(req(
                    "PUT",
                    &format!("/api/notes/{id}/blocks/{check_id}/checked"),
                    &admin,
                    Some(json!({"checked": true})),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        }
        let topic = json_body(
            app.clone()
                .oneshot(req("GET", &format!("/api/notes/{id}"), &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(topic["blocks"][0]["checked"], true);

        // A TEXT block is not checkable → 404; unknown block id → 404.
        for bad in [text_id, 999_999] {
            let resp = app
                .clone()
                .oneshot(req(
                    "PUT",
                    &format!("/api/notes/{id}/blocks/{bad}/checked"),
                    &admin,
                    Some(json!({"checked": true})),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn delete_is_author_or_admin() {
        let app = notes_app().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;
        // Admin's topic: member may NOT delete it (403), admin may.
        let id = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/notes",
                    &admin,
                    Some(json!({"title": "Adminin lista"})),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let resp = app
            .clone()
            .oneshot(req("DELETE", &format!("/api/notes/{id}"), &member, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let resp = app
            .clone()
            .oneshot(req("DELETE", &format!("/api/notes/{id}"), &admin, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        // Soft-deleted → hidden from list, GET 404s.
        let list = json_body(
            app.clone()
                .oneshot(req("GET", "/api/notes", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(list.as_array().unwrap().len(), 0);
        let resp = app
            .clone()
            .oneshot(req("GET", &format!("/api/notes/{id}"), &admin, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// CREATE → UPDATE → DELETE leaves three audit rows, newest first, with the
    /// right labels (delete captures the title before the soft-delete). Uses
    /// runtime queries so the sqlx offline cache needs no new entries.
    #[tokio::test]
    async fn note_mutations_leave_an_audit_trail() {
        let (app, db) = notes_app_with_db().await;
        let admin = setup_admin(&app).await;

        let id = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/notes",
                    &admin,
                    Some(json!({"title": "Häälista"})),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();

        app.clone()
            .oneshot(req(
                "PUT",
                &format!("/api/notes/{id}"),
                &admin,
                Some(json!({"title": "Häälista v2", "blocks": []})),
            ))
            .await
            .unwrap();

        app.clone()
            .oneshot(req("DELETE", &format!("/api/notes/{id}"), &admin, None))
            .await
            .unwrap();

        let rows = sqlx::query(
            "SELECT entity AS entity, op AS op, label AS label FROM audit_log
             WHERE family_id = 1 AND actor_user_id = 1 AND entity = 'note'
             ORDER BY id DESC",
        )
        .fetch_all(&db)
        .await
        .unwrap();

        let labels: Vec<(String, String)> = rows
            .into_iter()
            .map(|r| {
                let entity: String = r.get("entity");
                let op: String = r.get("op");
                let label: String = r.get("label");
                (format!("{entity}:{op}"), label)
            })
            .collect();

        assert_eq!(labels.len(), 3);
        assert_eq!(
            labels[0],
            ("note:delete".to_owned(), "Häälista v2".to_owned())
        );
        assert_eq!(
            labels[1],
            ("note:update".to_owned(), "Häälista v2".to_owned())
        );
        assert_eq!(labels[2], ("note:create".to_owned(), "Häälista".to_owned()));
    }

    #[tokio::test]
    async fn cross_family_put_and_check_are_404() {
        let (app, db) = notes_app_with_db().await;
        let admin = setup_admin(&app).await;
        let id = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/notes",
                    &admin,
                    Some(json!({"title": "Meidän"})),
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
                "PUT",
                &format!("/api/notes/{id}"),
                &outsider,
                Some(json!({"title": "Kaapattu", "blocks": []})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/notes/{id}/blocks/1/checked"),
                &outsider,
                Some(json!({"checked": true})),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}

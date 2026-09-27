//! Chores API: admin-managed definitions, a computed "today" list, and
//! anyone-can-complete done-tracking. Due-ness and rotation math live in
//! perkele_shared::chore so this module stays a thin DB adapter.

use crate::AppState;
use crate::db::Db;
use crate::error::ApiError;
use crate::session::{AdminUser, CurrentUser};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use perkele_shared::chore::{
    Chore, DueChore, SaveChoreRequest, assignee_on, is_due_on, validate_chore,
};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/chores", get(list).post(create))
        .route("/api/chores/today", get(today))
        .route(
            "/api/chores/{id}",
            axum::routing::put(update).delete(delete),
        )
        .route(
            "/api/chores/{id}/complete",
            post(complete).delete(uncomplete),
        )
}

/// The server's LOCAL calendar date — chores are floating local dates, and
/// the home server lives in the family's timezone (same 5C constraint).
pub(crate) fn local_today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn now_rfc3339() -> Result<String, ApiError> {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)
}

/// All live chore definitions of one family, rotation JSON decoded. A bad
/// rotation_json (shouldn't happen — we validate on write) decodes to None,
/// i.e. degrades to a whole-family chore instead of erroring the list.
pub(crate) async fn load_chores(db: &Db, family_id: i64) -> Result<Vec<Chore>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT id               AS "id!: i64",
                  title            AS "title!: String",
                  rrule            AS "rrule!: String",
                  start_date       AS "start_date!: String",
                  assigned_user_id AS "assigned_user_id?: i64",
                  rotation_json    AS "rotation_json?: String",
                  remind_at        AS "remind_at?: String",
                  points           AS "points!: i64"
           FROM chores WHERE family_id = ? AND deleted_at IS NULL
           ORDER BY title"#,
        family_id,
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| Chore {
            id: r.id,
            title: r.title,
            rrule: r.rrule,
            start_date: r.start_date,
            assigned_user_id: r.assigned_user_id,
            rotation: r
                .rotation_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok()),
            remind_at: r.remind_at,
            points: r.points,
        })
        .collect())
}

/// The computed due-list for one family and date. The /today handler's core,
/// split out so tests (and future callers) can pass a fixed date.
pub(crate) async fn due_on(db: &Db, family_id: i64, date: &str) -> Result<Vec<DueChore>, ApiError> {
    let mut out = Vec::new();
    for c in load_chores(db, family_id).await? {
        if !is_due_on(&c, date) {
            continue;
        }
        let done_by = sqlx::query_scalar!(
            r#"SELECT completed_by AS "completed_by!: i64"
               FROM chore_completions WHERE chore_id = ? AND date = ?"#,
            c.id,
            date,
        )
        .fetch_optional(db)
        .await?;
        out.push(DueChore {
            chore_id: c.id,
            title: c.title.clone(),
            date: date.to_owned(),
            assignee_id: assignee_on(&c, date),
            remind_at: c.remind_at.clone(),
            done_by,
            points: c.points,
        });
    }
    Ok(out)
}

/// Server-side re-check that every referenced user is in the caller's
/// family — the shared validator can't know the member list.
async fn check_members(db: &Db, family_id: i64, req: &SaveChoreRequest) -> Result<(), ApiError> {
    let mut ids: Vec<i64> = req.rotation.clone().unwrap_or_default();
    ids.extend(req.assigned_user_id);
    for id in ids {
        let n = sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "n!: i64" FROM users WHERE id = ? AND family_id = ?"#,
            id,
            family_id,
        )
        .fetch_one(db)
        .await?;
        if n == 0 {
            return Err(ApiError::BadRequest("Tuntematon perheenjäsen.".to_owned()));
        }
    }
    Ok(())
}

async fn list(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<Chore>>, ApiError> {
    Ok(Json(load_chores(&state.db, user.family_id).await?))
}

async fn today(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<DueChore>>, ApiError> {
    Ok(Json(
        due_on(&state.db, user.family_id, &local_today()).await?,
    ))
}

async fn create(
    admin: AdminUser,
    State(state): State<AppState>,
    Json(req): Json<SaveChoreRequest>,
) -> Result<(StatusCode, Json<Chore>), ApiError> {
    validate_chore(&req).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    check_members(&state.db, admin.0.family_id, &req).await?;
    let title = req.title.trim().to_owned();
    let rotation_json = req
        .rotation
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| ApiError::Internal)?;
    let now = now_rfc3339()?;
    // No `points` sent (or explicitly omitted) on create just means "use the
    // default" — unlike update, there's no existing row value to preserve.
    let points = req.points.unwrap_or(1);
    let id = sqlx::query!(
        "INSERT INTO chores (family_id, title, rrule, start_date, assigned_user_id,
                             rotation_json, remind_at, created_by, created_at, updated_at, points)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        admin.0.family_id,
        title,
        req.rrule,
        req.start_date,
        req.assigned_user_id,
        rotation_json,
        req.remind_at,
        admin.0.user_id,
        now,
        now,
        points,
    )
    .execute(&state.db)
    .await?
    .last_insert_rowid();
    crate::audit::record(
        &state.db,
        admin.0.family_id,
        admin.0.user_id,
        "chore",
        Some(id),
        "create",
        &title,
    )
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(Chore {
            id,
            title,
            rrule: req.rrule,
            start_date: req.start_date,
            assigned_user_id: req.assigned_user_id,
            rotation: req.rotation,
            remind_at: req.remind_at,
            points,
        }),
    ))
}

async fn update(
    admin: AdminUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SaveChoreRequest>,
) -> Result<StatusCode, ApiError> {
    validate_chore(&req).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    check_members(&state.db, admin.0.family_id, &req).await?;
    let title = req.title.trim().to_owned();
    let rotation_json = req
        .rotation
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|_| ApiError::Internal)?;
    let now = now_rfc3339()?;
    // `req.points` is `None` when an old cached client's PUT body doesn't
    // include the field at all. COALESCE(?, points) binds that as SQL NULL
    // and falls back to the row's current value, so a stale full-replace PUT
    // can't silently reset an admin-configured point value back to 1.
    let changed = sqlx::query!(
        "UPDATE chores SET title = ?, rrule = ?, start_date = ?, assigned_user_id = ?,
                           rotation_json = ?, remind_at = ?, updated_at = ?, points = COALESCE(?, points)
         WHERE id = ? AND family_id = ? AND deleted_at IS NULL",
        title,
        req.rrule,
        req.start_date,
        req.assigned_user_id,
        rotation_json,
        req.remind_at,
        now,
        req.points,
        id,
        admin.0.family_id,
    )
    .execute(&state.db)
    .await?
    .rows_affected();
    if changed == 0 {
        return Err(ApiError::NotFound);
    }
    crate::audit::record(
        &state.db,
        admin.0.family_id,
        admin.0.user_id,
        "chore",
        Some(id),
        "update",
        &title,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete(
    admin: AdminUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    // Capture the title before the tombstone UPDATE so the audit label still
    // has something to say — mirrors grocery::delete_item / recipe::delete_recipe.
    let title = sqlx::query_scalar!(
        r#"SELECT title AS "title!: String" FROM chores
           WHERE id = ? AND family_id = ? AND deleted_at IS NULL"#,
        id,
        admin.0.family_id,
    )
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::NotFound)?;

    let now = now_rfc3339()?;
    let changed = sqlx::query!(
        "UPDATE chores SET deleted_at = ? WHERE id = ? AND family_id = ? AND deleted_at IS NULL",
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
    crate::audit::record(
        &state.db,
        admin.0.family_id,
        admin.0.user_id,
        "chore",
        Some(id),
        "delete",
        &title,
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Load one live chore of the caller's family, or 404.
async fn family_chore(db: &Db, family_id: i64, id: i64) -> Result<Chore, ApiError> {
    load_chores(db, family_id)
        .await?
        .into_iter()
        .find(|c| c.id == id)
        .ok_or(ApiError::NotFound)
}

/// POST /api/chores/{id}/complete — mark done for TODAY (lapse model: no
/// other date is ever writable). Idempotent: double-tap = one row.
async fn complete(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let chore = family_chore(&state.db, user.family_id, id).await?;
    let date = local_today();
    if !is_due_on(&chore, &date) {
        return Err(ApiError::BadRequest(
            "Kotityö ei ole tänään vuorossa.".to_owned(),
        ));
    }
    let now = now_rfc3339()?;
    sqlx::query!(
        "INSERT INTO chore_completions (chore_id, date, completed_by, completed_at, points)
         VALUES (?, ?, ?, ?, ?) ON CONFLICT DO NOTHING",
        id,
        date,
        user.user_id,
        now,
        chore.points,
    )
    .execute(&state.db)
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

/// DELETE /api/chores/{id}/complete — undo today's completion (mis-tap).
/// Only the member who ticked it (or an admin) may undo it: the completion
/// row carries that member's points, so letting anyone delete it would let
/// one sibling erase another's points (and re-tick the chore as their own).
async fn uncomplete(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    // 404 for foreign/deleted chores, same as complete.
    family_chore(&state.db, user.family_id, id).await?;
    let date = local_today();
    let completed_by = sqlx::query_scalar!(
        r#"SELECT completed_by AS "completed_by!: i64" FROM chore_completions
           WHERE chore_id = ? AND date = ?"#,
        id,
        date,
    )
    .fetch_optional(&state.db)
    .await?;
    // Nothing to undo: idempotent success (a double-tap on "undo").
    let Some(completed_by) = completed_by else {
        return Ok(StatusCode::NO_CONTENT);
    };
    if !crate::grocery::can_modify(&user, completed_by) {
        return Err(ApiError::Forbidden);
    }
    // `completed_by` in the WHERE pins the delete to the exact row we just
    // authorized, so a racing re-tick by someone else can't be swept up.
    sqlx::query!(
        "DELETE FROM chore_completions WHERE chore_id = ? AND date = ? AND completed_by = ?",
        id,
        date,
        completed_by,
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

    async fn chore_app_with_db() -> (axum::Router, crate::db::Db) {
        let db = crate::db::test_pool().await;
        let state = AppState::for_test(db.clone());
        let app = crate::routes::router().merge(router()).with_state(state);
        (app, db)
    }

    async fn chore_app() -> axum::Router {
        chore_app_with_db().await.0
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

    /// The outsider is an ADMIN of family 2 on purpose: chore DELETE takes
    /// `AdminUser`, so a member-outsider would 403 at the extractor and never
    /// reach the family-scoping we want to prove returns 404.
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
        .bind("admin")
        .bind(now)
        .execute(db)
        .await
        .unwrap();
    }

    /// Date-determinism rule: handler tests only use FREQ=DAILY chores with a
    /// start far in the past (always due) or far in the future (never due) —
    /// weekday-dependent rules are covered by the shared unit tests.
    fn daily(title: &str) -> Value {
        json!({ "title": title, "rrule": "FREQ=DAILY", "start_date": "2020-01-01",
                "assigned_user_id": null, "rotation": null, "remind_at": null })
    }

    #[tokio::test]
    async fn member_cannot_manage_chores() {
        let app = chore_app().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;
        let resp = app
            .clone()
            .oneshot(req("POST", "/api/chores", &member, Some(daily("Tiskit"))))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn create_today_complete_undo_roundtrip() {
        let app = chore_app().await;
        let admin = setup_admin(&app).await;
        let id = json_body(
            app.clone()
                .oneshot(req("POST", "/api/chores", &admin, Some(daily("Tiskit"))))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();

        // Due today, not done.
        let body = json_body(
            app.clone()
                .oneshot(req("GET", "/api/chores/today", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["title"], "Tiskit");
        assert_eq!(body[0]["points"], 1); // default value flows to the today list
        assert!(body[0]["done_by"].is_null());
        assert!(body[0]["assignee_id"].is_null()); // whole family

        // Complete twice → idempotent; done_by = admin (user id 1).
        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(req(
                    "POST",
                    &format!("/api/chores/{id}/complete"),
                    &admin,
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        }
        let body = json_body(
            app.clone()
                .oneshot(req("GET", "/api/chores/today", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body[0]["done_by"], 1);

        // Undo → not done again.
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/chores/{id}/complete"),
                &admin,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let body = json_body(
            app.oneshot(req("GET", "/api/chores/today", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert!(body[0]["done_by"].is_null());
    }

    /// Points can't be stolen: a member can't undo the admin's tick (403, the
    /// completion and its points stay), but can undo their own, and the admin
    /// can undo anyone's.
    #[tokio::test]
    async fn uncomplete_is_completer_or_admin() {
        let (app, db) = chore_app_with_db().await;
        let admin = setup_admin(&app).await;
        let member = join_member(&app, &admin).await;
        let mut body = daily("Tiskit");
        body["points"] = json!(5);
        let id = json_body(
            app.clone()
                .oneshot(req("POST", "/api/chores", &admin, Some(body)))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let path = format!("/api/chores/{id}/complete");
        let call = |method: &'static str, who: String| {
            let app = app.clone();
            let path = path.clone();
            async move {
                app.oneshot(req(method, &path, &who, None))
                    .await
                    .unwrap()
                    .status()
            }
        };
        // Runtime query (not `query!`) so the test needs no sqlx cache entry.
        let row = || async {
            sqlx::query_as::<_, (i64, i64)>(
                "SELECT completed_by, points FROM chore_completions WHERE chore_id = ?",
            )
            .bind(id)
            .fetch_optional(&db)
            .await
            .unwrap()
        };

        // Admin (user 1) ticks; member tries to undo → refused, points intact.
        assert_eq!(call("POST", admin.clone()).await, StatusCode::NO_CONTENT);
        assert_eq!(call("DELETE", member.clone()).await, StatusCode::FORBIDDEN);
        assert_eq!(row().await, Some((1, 5)));

        // Admin may undo anyone's; member may undo their own.
        assert_eq!(call("DELETE", admin.clone()).await, StatusCode::NO_CONTENT);
        assert_eq!(row().await, None);
        assert_eq!(call("POST", member.clone()).await, StatusCode::NO_CONTENT);
        assert_eq!(call("DELETE", member.clone()).await, StatusCode::NO_CONTENT);
        assert_eq!(row().await, None);
        assert_eq!(call("POST", member.clone()).await, StatusCode::NO_CONTENT);
        assert_eq!(call("DELETE", admin.clone()).await, StatusCode::NO_CONTENT);
        assert_eq!(row().await, None);
        // Undo with nothing to undo stays an idempotent 204.
        assert_eq!(call("DELETE", member).await, StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn future_chore_is_not_due_and_cannot_be_completed() {
        let app = chore_app().await;
        let admin = setup_admin(&app).await;
        let id = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/chores",
                    &admin,
                    Some(json!({ "title": "Joulusiivous", "rrule": "FREQ=DAILY",
                                 "start_date": "2199-01-01", "assigned_user_id": null,
                                 "rotation": null, "remind_at": null })),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let body = json_body(
            app.clone()
                .oneshot(req("GET", "/api/chores/today", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), 0);
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chores/{id}/complete"),
                &admin,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rotation_of_one_always_assigns_that_member() {
        // Deterministic for any date: rotation [member] mod 1 == member.
        let app = chore_app().await;
        let admin = setup_admin(&app).await;
        let _member = join_member(&app, &admin).await; // user id 2
        app.clone()
            .oneshot(req(
                "POST",
                "/api/chores",
                &admin,
                Some(json!({ "title": "Roskat", "rrule": "FREQ=DAILY",
                             "start_date": "2020-01-01", "assigned_user_id": null,
                             "rotation": [2], "remind_at": null })),
            ))
            .await
            .unwrap();
        let body = json_body(
            app.oneshot(req("GET", "/api/chores/today", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body[0]["assignee_id"], 2);
    }

    #[tokio::test]
    async fn validation_rejects_bad_requests() {
        let app = chore_app().await;
        let admin = setup_admin(&app).await;
        let _member = join_member(&app, &admin).await;
        // Both assignee and rotation.
        let bad = json!({ "title": "Tiskit", "rrule": "FREQ=DAILY", "start_date": "2020-01-01",
                          "assigned_user_id": 1, "rotation": [2], "remind_at": null });
        let resp = app
            .clone()
            .oneshot(req("POST", "/api/chores", &admin, Some(bad)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // Rotation member from another family (id 999 doesn't exist here).
        let bad = json!({ "title": "Tiskit", "rrule": "FREQ=DAILY", "start_date": "2020-01-01",
                          "assigned_user_id": null, "rotation": [999], "remind_at": null });
        let resp = app
            .clone()
            .oneshot(req("POST", "/api/chores", &admin, Some(bad)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // Broken rrule.
        let bad = json!({ "title": "Tiskit", "rrule": "FREQ=NELJÄSTI", "start_date": "2020-01-01",
                          "assigned_user_id": null, "rotation": null, "remind_at": null });
        let resp = app
            .oneshot(req("POST", "/api/chores", &admin, Some(bad)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn complete_snapshots_points_and_edits_do_not_rewrite_history() {
        let (app, db) = chore_app_with_db().await;
        let admin = setup_admin(&app).await;
        let mut body = daily("Tiskit");
        body["points"] = json!(5);
        let id = json_body(
            app.clone()
                .oneshot(req("POST", "/api/chores", &admin, Some(body.clone())))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        app.clone()
            .oneshot(req(
                "POST",
                &format!("/api/chores/{id}/complete"),
                &admin,
                None,
            ))
            .await
            .unwrap();

        // The completion row carries the chore's value AT TICK TIME…
        let pts: i64 =
            sqlx::query_scalar("SELECT points FROM chore_completions WHERE chore_id = ?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(pts, 5);

        // …and lowering the chore's value afterwards must not touch it.
        body["points"] = json!(1);
        let resp = app
            .clone()
            .oneshot(req("PUT", &format!("/api/chores/{id}"), &admin, Some(body)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let pts: i64 =
            sqlx::query_scalar("SELECT points FROM chore_completions WHERE chore_id = ?")
                .bind(id)
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(pts, 5);
    }

    #[tokio::test]
    async fn put_without_points_key_preserves_the_existing_value() {
        // Simulates a stale cached PWA bundle whose form predates the points
        // field: the PUT body simply has no `points` key at all (not even
        // null). Regression test for the bug where serde's old default
        // silently deserialized that as 1, resetting an admin-set value.
        let app = chore_app().await;
        let admin = setup_admin(&app).await;
        let mut body = daily("Tiskit");
        body["points"] = json!(5);
        let id = json_body(
            app.clone()
                .oneshot(req("POST", "/api/chores", &admin, Some(body.clone())))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();

        // Build the PUT body WITHOUT a "points" key at all.
        let mut no_points = body.clone();
        no_points.as_object_mut().unwrap().remove("points");
        assert!(!no_points.as_object().unwrap().contains_key("points"));
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/chores/{id}"),
                &admin,
                Some(no_points),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let list = json_body(
            app.oneshot(req("GET", "/api/chores", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(list[0]["points"], 5);
    }

    #[tokio::test]
    async fn cross_family_chore_is_404() {
        let (app, db) = chore_app_with_db().await;
        let admin = setup_admin(&app).await;
        let id = json_body(
            app.clone()
                .oneshot(req("POST", "/api/chores", &admin, Some(daily("Tiskit"))))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        seed_other_family(&db).await;
        let outsider = login(&app, "outsider", "password1").await;
        for (method, path) in [
            ("POST", format!("/api/chores/{id}/complete")),
            ("DELETE", format!("/api/chores/{id}")),
        ] {
            let resp = app
                .clone()
                .oneshot(req(method, &path, &outsider, None))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{method} {path}");
        }
    }

    #[tokio::test]
    async fn chore_mutations_write_audit_rows() {
        let (app, db) = chore_app_with_db().await;
        let admin = setup_admin(&app).await;

        let created = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/chores",
                    &admin,
                    Some(json!({ "title": "Roskat", "rrule": "FREQ=WEEKLY",
                                 "start_date": "2026-07-24", "assigned_user_id": null,
                                 "remind_at": null, "points": 1 })),
                ))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_i64().unwrap();

        app.clone()
            .oneshot(req("DELETE", &format!("/api/chores/{id}"), &admin, None))
            .await
            .unwrap();

        let rows = sqlx::query!(
            r#"SELECT entity AS "entity!", op AS "op!", label AS "label!" FROM audit_log ORDER BY id"#
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            (
                rows[0].entity.as_str(),
                rows[0].op.as_str(),
                rows[0].label.as_str()
            ),
            ("chore", "create", "Roskat")
        );
        assert_eq!(
            (rows[1].op.as_str(), rows[1].label.as_str()),
            ("delete", "Roskat")
        );
    }
}

//! Aisles ("hyllyt"): walking-order aisle list + learned item-name → aisle
//! map. Sorting the main grocery list by aisle happens client-side; this
//! module only owns the data. Mutations poke SSE (other phones refetch on
//! the tick) but do NOT write sync_log — the log is the grocery items' delta
//! stream, and aisle state is small enough to refetch whole.

use crate::AppState;
use crate::error::ApiError;
use crate::session::{AdminUser, CurrentUser};
use crate::sync::poke;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use perkele_shared::aisle::{
    Aisle, AislesResponse, ItemAisle, ReorderAislesRequest, SaveAisleRequest, SetFavoriteRequest,
    SetItemAisleRequest, validate_aisle_name,
};
use perkele_shared::grocery::validate_grocery_item_name;
use perkele_shared::recipe::RecipeIngredient;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/aisles", get(list_aisles))
        .route("/api/aisles", post(create_aisle))
        .route("/api/aisles/order", put(reorder_aisles))
        .route("/api/aisles/map", put(set_item_aisle))
        .route("/api/aisles/favorite", put(set_favorite))
        .route("/api/aisles/{id}", put(rename_aisle))
        .route("/api/aisles/{id}", delete(delete_aisle))
}

async fn list_aisles(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<AislesResponse>, ApiError> {
    let aisles = sqlx::query!(
        r#"SELECT id AS "id!: i64", name AS "name!: String", position AS "position!: i64"
           FROM grocery_aisles WHERE family_id = ? ORDER BY position ASC"#,
        user.family_id,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| Aisle {
        id: r.id,
        name: r.name,
        position: r.position,
    })
    .collect();

    let map = sqlx::query!(
        r#"SELECT item_name AS "item_name!: String", aisle_id AS "aisle_id!: i64"
           FROM item_aisles WHERE family_id = ?"#,
        user.family_id,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| ItemAisle {
        item_name: r.item_name,
        aisle_id: r.aisle_id,
    })
    .collect();

    let favorites = sqlx::query_scalar!(
        r#"SELECT item_name AS "item_name!: String"
           FROM item_favorites WHERE family_id = ?"#,
        user.family_id,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(AislesResponse {
        aisles,
        map,
        favorites,
    }))
}

async fn create_aisle(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<SaveAisleRequest>,
) -> Result<(StatusCode, Json<Aisle>), ApiError> {
    let name = req.name.trim().to_owned();
    validate_aisle_name(&name).map_err(|m| ApiError::BadRequest(m.to_owned()))?;

    let position = sqlx::query_scalar!(
        r#"SELECT COALESCE(MAX(position), 0) + 1 AS "pos!: i64"
           FROM grocery_aisles WHERE family_id = ?"#,
        user.family_id,
    )
    .fetch_one(&state.db)
    .await?;

    // UNIQUE(family_id, name) turns a duplicate into a DB error; surface it
    // as a friendly 400 instead of a 500.
    let id = sqlx::query!(
        "INSERT INTO grocery_aisles (family_id, name, position) VALUES (?, ?, ?)",
        user.family_id,
        name,
        position,
    )
    .execute(&state.db)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db) if db.is_unique_violation() => {
            ApiError::BadRequest("Hylly on jo olemassa.".to_owned())
        }
        other => ApiError::from(other),
    })?
    .last_insert_rowid();

    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "aisle",
        Some(id),
        "create",
        &name,
    )
    .await?;

    poke(&state, user.family_id).await;
    Ok((StatusCode::CREATED, Json(Aisle { id, name, position })))
}

async fn rename_aisle(
    user: CurrentUser,
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Json(req): Json<SaveAisleRequest>,
) -> Result<StatusCode, ApiError> {
    let name = req.name.trim().to_owned();
    validate_aisle_name(&name).map_err(|m| ApiError::BadRequest(m.to_owned()))?;

    let rows = sqlx::query!(
        "UPDATE grocery_aisles SET name = ? WHERE id = ? AND family_id = ?",
        name,
        id,
        user.family_id,
    )
    .execute(&state.db)
    .await
    .map_err(|e| match e {
        sqlx::Error::Database(ref db) if db.is_unique_violation() => {
            ApiError::BadRequest("Hylly on jo olemassa.".to_owned())
        }
        other => ApiError::from(other),
    })?
    .rows_affected();

    if rows == 0 {
        return Err(ApiError::NotFound);
    }

    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "aisle",
        Some(id),
        "update",
        &name,
    )
    .await?;

    poke(&state, user.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Deleting an aisle also deletes its item mappings: item_aisles.aisle_id has
/// ON DELETE CASCADE, and the pool enables PRAGMA foreign_keys (db.rs), so one
/// DELETE is enough. Items fall back to "Lajittelematon".
///
/// Admin-only (the `AdminUser` extractor 403s everyone else). Aisles are a
/// shared family setting — the store's walking order — with no author column
/// (recipe imports even create them automatically), so author-or-admin can't
/// apply; and a delete also throws away every learned item→aisle mapping for
/// it, which is the destructive part. Adding, renaming and reordering stay
/// open to everyone: they lose nothing and are trivially reversible.
async fn delete_aisle(
    admin: AdminUser,
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Result<StatusCode, ApiError> {
    // `AdminUser` is a tuple struct wrapping the CurrentUser; `.0` unwraps it.
    let user = admin.0;
    // Grab the name before deleting so the audit label can still say what
    // was removed — the row is gone by the time we'd otherwise read it.
    let name = sqlx::query_scalar!(
        r#"SELECT name AS "name!: String" FROM grocery_aisles WHERE id = ? AND family_id = ?"#,
        id,
        user.family_id,
    )
    .fetch_optional(&state.db)
    .await?;

    let rows = sqlx::query!(
        "DELETE FROM grocery_aisles WHERE id = ? AND family_id = ?",
        id,
        user.family_id,
    )
    .execute(&state.db)
    .await?
    .rows_affected();

    if rows == 0 {
        return Err(ApiError::NotFound);
    }

    if let Some(name) = name {
        crate::audit::record(
            &state.db,
            user.family_id,
            user.user_id,
            "aisle",
            Some(id),
            "delete",
            &name,
        )
        .await?;
    }

    poke(&state, user.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Body carries the FULL id list in the new order. Requiring the complete set
/// (validated against the DB) makes the write idempotent and immune to races
/// where another phone added an aisle mid-edit: such a request fails with 400
/// and the client refetches.
async fn reorder_aisles(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<ReorderAislesRequest>,
) -> Result<StatusCode, ApiError> {
    let existing: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64" FROM grocery_aisles WHERE family_id = ?"#,
        user.family_id,
    )
    .fetch_all(&state.db)
    .await?;

    let mut sent = req.ids.clone();
    sent.sort_unstable();
    let mut have = existing;
    have.sort_unstable();
    if sent != have {
        return Err(ApiError::BadRequest(
            "Hyllyjärjestys on vanhentunut.".to_owned(),
        ));
    }

    for (idx, id) in req.ids.iter().enumerate() {
        let position = (idx + 1) as i64;
        sqlx::query!(
            "UPDATE grocery_aisles SET position = ? WHERE id = ? AND family_id = ?",
            position,
            id,
            user.family_id,
        )
        .execute(&state.db)
        .await?;
    }

    poke(&state, user.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Assign or clear one item name's aisle — the "learning" write. Lowercase
/// in Rust (SQLite LOWER is ASCII-only). `aisle_id: None` clears.
async fn set_item_aisle(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<SetItemAisleRequest>,
) -> Result<StatusCode, ApiError> {
    validate_grocery_item_name(&req.item_name).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    let key = req.item_name.trim().to_lowercase();

    match req.aisle_id {
        Some(aisle_id) => {
            // The aisle must exist in the caller's family (also blocks
            // cross-family ids).
            let exists = sqlx::query_scalar!(
                r#"SELECT COUNT(*) AS "n!: i64" FROM grocery_aisles
                   WHERE id = ? AND family_id = ?"#,
                aisle_id,
                user.family_id,
            )
            .fetch_one(&state.db)
            .await?;
            if exists == 0 {
                return Err(ApiError::NotFound);
            }

            sqlx::query!(
                "INSERT INTO item_aisles (family_id, item_name, aisle_id)
                 VALUES (?, ?, ?)
                 ON CONFLICT (family_id, item_name)
                 DO UPDATE SET aisle_id = excluded.aisle_id",
                user.family_id,
                key,
                aisle_id,
            )
            .execute(&state.db)
            .await?;
        }
        None => {
            sqlx::query!(
                "DELETE FROM item_aisles WHERE family_id = ? AND item_name = ?",
                user.family_id,
                key,
            )
            .execute(&state.db)
            .await?;
        }
    }

    poke(&state, user.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Star/unstar an item by name (family-wide, like the aisle map). `starred`
/// carries the target state so a duplicate/late request is idempotent.
async fn set_favorite(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<SetFavoriteRequest>,
) -> Result<StatusCode, ApiError> {
    validate_grocery_item_name(&req.item_name).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    let key = req.item_name.trim().to_lowercase();

    if req.starred {
        sqlx::query!(
            "INSERT INTO item_favorites (family_id, item_name) VALUES (?, ?)
             ON CONFLICT (family_id, item_name) DO NOTHING",
            user.family_id,
            key,
        )
        .execute(&state.db)
        .await?;
    } else {
        sqlx::query!(
            "DELETE FROM item_favorites WHERE family_id = ? AND item_name = ?",
            user.family_id,
            key,
        )
        .execute(&state.db)
        .await?;
    }

    poke(&state, user.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Feed the item→aisle map from recipe-import ingredients. For each
/// ingredient with a category: find the aisle case-insensitively (folded in
/// Rust — SQLite LOWER is ASCII-only), create it at the end of the walking
/// order if missing, and map the ingredient name to it UNLESS the name is
/// already mapped — an import never overrides a manual assignment.
/// The caller pokes SSE afterwards (imports already do).
pub(crate) async fn teach_item_aisles(
    db: &crate::db::Db,
    family_id: i64,
    ingredients: &[RecipeIngredient],
) -> Result<(), ApiError> {
    // Snapshot the family's aisles once: lowercased name -> id.
    let rows = sqlx::query!(
        r#"SELECT id AS "id!: i64", name AS "name!: String",
                  position AS "position!: i64"
           FROM grocery_aisles WHERE family_id = ?"#,
        family_id,
    )
    .fetch_all(db)
    .await?;
    let mut max_position = rows.iter().map(|r| r.position).max().unwrap_or(0);
    let mut by_name: std::collections::HashMap<String, i64> = rows
        .into_iter()
        .map(|r| (r.name.trim().to_lowercase(), r.id))
        .collect();

    for ing in ingredients {
        let Some(category) = ing
            .category
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
        else {
            continue;
        };
        let aisle_key = category.to_lowercase();

        let aisle_id = match by_name.get(&aisle_key) {
            Some(&id) => id,
            None => {
                max_position += 1;
                // A concurrent import may create the same aisle between our
                // snapshot and this INSERT. UNIQUE(family_id, name) turns
                // that race into a DB error; recover by reading the row the
                // winner created instead of failing the whole import.
                let inserted = sqlx::query!(
                    "INSERT INTO grocery_aisles (family_id, name, position) VALUES (?, ?, ?)",
                    family_id,
                    category,
                    max_position,
                )
                .execute(db)
                .await;
                let id = match inserted {
                    Ok(res) => res.last_insert_rowid(),
                    Err(sqlx::Error::Database(ref db_err)) if db_err.is_unique_violation() => {
                        max_position -= 1; // we didn't create a row after all
                        sqlx::query_scalar!(
                            r#"SELECT id AS "id!: i64" FROM grocery_aisles
                               WHERE family_id = ? AND name = ?"#,
                            family_id,
                            category,
                        )
                        .fetch_one(db)
                        .await?
                    }
                    Err(e) => return Err(e.into()),
                };
                by_name.insert(aisle_key, id);
                id
            }
        };

        let item_key = ing.name.trim().to_lowercase();
        // OR IGNORE + UNIQUE(family_id, item_name) = "only if not mapped yet".
        sqlx::query!(
            "INSERT OR IGNORE INTO item_aisles (family_id, item_name, aisle_id)
             VALUES (?, ?, ?)",
            family_id,
            item_key,
            aisle_id,
        )
        .execute(db)
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn aisle_app() -> (axum::Router, crate::db::Db) {
        let db = crate::db::test_pool().await;
        let state = AppState::for_test(db.clone());
        let app = crate::routes::router().merge(router()).with_state(state);
        (app, db)
    }

    fn post(path: &str, body: Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn post_auth(path: &str, body: Value, cookie: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, cookie)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn get_auth(path: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    }

    fn put_auth(path: &str, body: Value, cookie: &str) -> Request<Body> {
        Request::builder()
            .method("PUT")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::COOKIE, cookie)
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    fn delete_auth(path: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .method("DELETE")
            .uri(path)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
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
            .oneshot(post(
                "/api/setup",
                json!({
                    "family_name": "Virtanen",
                    "username": "mikko",
                    "display_name": "Mikko",
                    "password": "hunter2!"
                }),
            ))
            .await
            .unwrap();
        cookie_pair(&resp)
    }

    /// Creates one admin + n aisles, returns (cookie, ids in creation order).
    async fn setup_with_aisles(app: &axum::Router, names: &[&str]) -> (String, Vec<i64>) {
        let cookie = setup_admin(app).await;
        let mut ids = Vec::new();
        for name in names {
            let resp = app
                .clone()
                .oneshot(post_auth("/api/aisles", json!({ "name": name }), &cookie))
                .await
                .unwrap();
            ids.push(json_body(resp).await["id"].as_i64().unwrap());
        }
        (cookie, ids)
    }

    #[tokio::test]
    async fn aisles_require_auth() {
        let app = aisle_app().await.0;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/aisles")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn create_and_list_aisles_in_position_order() {
        let app = aisle_app().await.0;
        let cookie = setup_admin(&app).await;

        for name in ["Kasvikset", "Maito"] {
            let resp = app
                .clone()
                .oneshot(post_auth("/api/aisles", json!({ "name": name }), &cookie))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }

        let resp = app
            .clone()
            .oneshot(get_auth("/api/aisles", &cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        let names: Vec<&str> = body["aisles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Kasvikset", "Maito"]); // creation order = position order
        assert_eq!(body["aisles"][0]["position"], 1);
        assert_eq!(body["aisles"][1]["position"], 2);
        assert_eq!(body["map"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn blank_aisle_name_is_rejected() {
        let app = aisle_app().await.0;
        let cookie = setup_admin(&app).await;
        let resp = app
            .clone()
            .oneshot(post_auth("/api/aisles", json!({ "name": "   " }), &cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn duplicate_aisle_name_is_rejected() {
        let app = aisle_app().await.0;
        let cookie = setup_admin(&app).await;
        let first = app
            .clone()
            .oneshot(post_auth(
                "/api/aisles",
                json!({ "name": "Maito" }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::CREATED);
        let dup = app
            .clone()
            .oneshot(post_auth(
                "/api/aisles",
                json!({ "name": "Maito" }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(dup.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rename_aisle_keeps_position() {
        let app = aisle_app().await.0;
        let (cookie, ids) = setup_with_aisles(&app, &["Maito", "Liha"]).await;

        let resp = app
            .clone()
            .oneshot(put_auth(
                &format!("/api/aisles/{}", ids[0]),
                json!({ "name": "Maitotuotteet" }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let body = json_body(
            app.clone()
                .oneshot(get_auth("/api/aisles", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["aisles"][0]["name"], "Maitotuotteet");
        assert_eq!(body["aisles"][0]["position"], 1);
    }

    #[tokio::test]
    async fn reorder_rewrites_positions() {
        let app = aisle_app().await.0;
        let (cookie, ids) = setup_with_aisles(&app, &["Maito", "Liha", "Leipä"]).await;

        let resp = app
            .clone()
            .oneshot(put_auth(
                "/api/aisles/order",
                json!({ "ids": [ids[2], ids[0], ids[1]] }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let body = json_body(
            app.clone()
                .oneshot(get_auth("/api/aisles", &cookie))
                .await
                .unwrap(),
        )
        .await;
        let names: Vec<&str> = body["aisles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Leipä", "Maito", "Liha"]);
    }

    #[tokio::test]
    async fn reorder_with_wrong_id_set_is_rejected() {
        let app = aisle_app().await.0;
        let (cookie, ids) = setup_with_aisles(&app, &["Maito", "Liha"]).await;

        // Missing one id → 400 (must send the FULL list).
        let resp = app
            .clone()
            .oneshot(put_auth(
                "/api/aisles/order",
                json!({ "ids": [ids[0]] }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn rename_missing_aisle_is_404() {
        let app = aisle_app().await.0;
        let cookie = setup_admin(&app).await;
        let resp = app
            .clone()
            .oneshot(put_auth("/api/aisles/999", json!({ "name": "X" }), &cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_aisle_removes_it_from_the_list() {
        let app = aisle_app().await.0;
        let (cookie, ids) = setup_with_aisles(&app, &["Maito", "Liha"]).await;

        let resp = app
            .clone()
            .oneshot(delete_auth(&format!("/api/aisles/{}", ids[0]), &cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let body = json_body(
            app.clone()
                .oneshot(get_auth("/api/aisles", &cookie))
                .await
                .unwrap(),
        )
        .await;
        let names: Vec<&str> = body["aisles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Liha"]);
    }

    /// Aisles have no author column, so delete is admin-only: a kid (or
    /// member) gets 403 and the aisle survives, even one they created.
    #[tokio::test]
    async fn only_admin_may_delete_an_aisle() {
        let app = aisle_app().await.0;
        let (admin, ids) = setup_with_aisles(&app, &["Maito"]).await;
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/family/invites",
                json!({ "role": "kid" }),
                &admin,
            ))
            .await
            .unwrap();
        let code = json_body(resp).await["code"].as_str().unwrap().to_owned();
        let resp = app
            .clone()
            .oneshot(post(
                "/api/auth/redeem",
                json!({ "code": code, "username": "kalle",
                        "display_name": "Kalle", "password": "hunter2!" }),
            ))
            .await
            .unwrap();
        let kid = cookie_pair(&resp);

        let resp = app
            .clone()
            .oneshot(post_auth("/api/aisles", json!({ "name": "Karkit" }), &kid))
            .await
            .unwrap();
        let kids_aisle = json_body(resp).await["id"].as_i64().unwrap();

        for id in [ids[0], kids_aisle] {
            let resp = app
                .clone()
                .oneshot(delete_auth(&format!("/api/aisles/{id}"), &kid))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        }
        let body = json_body(
            app.clone()
                .oneshot(get_auth("/api/aisles", &admin))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["aisles"].as_array().unwrap().len(), 2);

        let resp = app
            .oneshot(delete_auth(&format!("/api/aisles/{kids_aisle}"), &admin))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn aisle_lifecycle_writes_audit_rows() {
        let (app, db) = aisle_app().await;
        let cookie = setup_admin(&app).await;

        let created = json_body(
            app.clone()
                .oneshot(post_auth(
                    "/api/aisles",
                    json!({ "name": "Maitohylly" }),
                    &cookie,
                ))
                .await
                .unwrap(),
        )
        .await;
        let id = created["id"].as_i64().unwrap();

        app.clone()
            .oneshot(put_auth(
                &format!("/api/aisles/{id}"),
                json!({ "name": "Kylmähylly" }),
                &cookie,
            ))
            .await
            .unwrap();

        app.clone()
            .oneshot(delete_auth(&format!("/api/aisles/{id}"), &cookie))
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
            ("aisle", "create", "Maitohylly")
        );
        assert_eq!(
            (rows[1].op.as_str(), rows[1].label.as_str()),
            ("update", "Kylmähylly")
        );
        // Deletion happens AFTER the rename above, so the label captured
        // "before the delete" is the current (renamed) name, not the
        // original creation name — matches the "capture the live value
        // right before the mutating query" pattern used elsewhere (e.g.
        // grocery.rs's delete_section).
        assert_eq!(
            (rows[2].op.as_str(), rows[2].label.as_str()),
            ("delete", "Kylmähylly")
        );
    }

    #[tokio::test]
    async fn delete_missing_aisle_is_404() {
        let app = aisle_app().await.0;
        let cookie = setup_admin(&app).await;
        let resp = app
            .clone()
            .oneshot(delete_auth("/api/aisles/999", &cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn assign_reassign_and_clear_mapping() {
        let app = aisle_app().await.0;
        let (cookie, ids) = setup_with_aisles(&app, &["Maito", "Juomat"]).await;

        // Assign — note the un-normalized name; server stores "maito".
        let resp = app
            .clone()
            .oneshot(put_auth(
                "/api/aisles/map",
                json!({ "item_name": "  Maito ", "aisle_id": ids[0] }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let body = json_body(
            app.clone()
                .oneshot(get_auth("/api/aisles", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["map"][0]["item_name"], "maito");
        assert_eq!(body["map"][0]["aisle_id"], ids[0]);

        // Reassign (upsert, not duplicate).
        let resp = app
            .clone()
            .oneshot(put_auth(
                "/api/aisles/map",
                json!({ "item_name": "maito", "aisle_id": ids[1] }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let body = json_body(
            app.clone()
                .oneshot(get_auth("/api/aisles", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["map"].as_array().unwrap().len(), 1);
        assert_eq!(body["map"][0]["aisle_id"], ids[1]);

        // Clear.
        let resp = app
            .clone()
            .oneshot(put_auth(
                "/api/aisles/map",
                json!({ "item_name": "maito", "aisle_id": null }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let body = json_body(
            app.clone()
                .oneshot(get_auth("/api/aisles", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["map"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn mapping_to_unknown_aisle_is_404() {
        let app = aisle_app().await.0;
        let cookie = setup_admin(&app).await;
        let resp = app
            .clone()
            .oneshot(put_auth(
                "/api/aisles/map",
                json!({ "item_name": "maito", "aisle_id": 999 }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_aisle_cascades_its_mappings() {
        let (app, db) = aisle_app().await;
        let (cookie, ids) = setup_with_aisles(&app, &["Maito"]).await;

        // Map an item, then delete the aisle: the mapping must vanish.
        let resp = app
            .clone()
            .oneshot(put_auth(
                "/api/aisles/map",
                json!({ "item_name": "maito", "aisle_id": ids[0] }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app
            .clone()
            .oneshot(delete_auth(&format!("/api/aisles/{}", ids[0]), &cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM item_aisles")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);

        let body = json_body(
            app.clone()
                .oneshot(get_auth("/api/aisles", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["aisles"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn set_and_clear_favorite_reflected_in_list() {
        let (app, _db) = aisle_app().await;
        let cookie = setup_admin(&app).await;

        // Star "Maito" — name is lowercased server-side.
        let resp = app
            .clone()
            .oneshot(put_auth(
                "/api/aisles/favorite",
                json!({ "item_name": "Maito", "starred": true }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let body = json_body(
            app.clone()
                .oneshot(get_auth("/api/aisles", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["favorites"], json!(["maito"]));

        // Unstar it.
        app.clone()
            .oneshot(put_auth(
                "/api/aisles/favorite",
                json!({ "item_name": "maito", "starred": false }),
                &cookie,
            ))
            .await
            .unwrap();
        let body = json_body(
            app.clone()
                .oneshot(get_auth("/api/aisles", &cookie))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["favorites"], json!([]));
    }

    fn ing(name: &str, category: Option<&str>) -> perkele_shared::recipe::RecipeIngredient {
        perkele_shared::recipe::RecipeIngredient {
            name: name.into(),
            qty: None,
            unit: None,
            category: category.map(Into::into),
        }
    }

    /// Direct DB fixtures (no HTTP): family id straight from an INSERT, same
    /// approach as seed.rs::fixture_family.
    async fn fixture_family(db: &crate::db::Db) -> i64 {
        sqlx::query("INSERT INTO families (name, created_at) VALUES ('T', '2026-01-01T00:00:00Z')")
            .execute(db)
            .await
            .unwrap()
            .last_insert_rowid()
    }

    #[tokio::test]
    async fn teach_creates_missing_aisles_and_mappings() {
        let db = crate::db::test_pool().await;
        let fam = fixture_family(&db).await;

        teach_item_aisles(
            &db,
            fam,
            &[
                ing("maito", Some("maito")),
                ing("kerma", Some("maito")),
                ing("riisi", None),
            ],
        )
        .await
        .unwrap();

        let aisles: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM grocery_aisles WHERE family_id = ? ORDER BY position",
        )
        .bind(fam)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(aisles, ["maito"]); // created once, not twice; riisi (no category) ignored

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM item_aisles WHERE family_id = ?")
            .bind(fam)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 2);
    }

    #[tokio::test]
    async fn teach_never_overrides_existing_mapping() {
        let db = crate::db::test_pool().await;
        let fam = fixture_family(&db).await;

        // Manual state: aisle "Juomat" and maito already mapped to it.
        sqlx::query(
            "INSERT INTO grocery_aisles (family_id, name, position) VALUES (?, 'Juomat', 1)",
        )
        .bind(fam)
        .execute(&db)
        .await
        .unwrap();
        let juomat: i64 = sqlx::query_scalar("SELECT id FROM grocery_aisles WHERE family_id = ?")
            .bind(fam)
            .fetch_one(&db)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO item_aisles (family_id, item_name, aisle_id) VALUES (?, 'maito', ?)",
        )
        .bind(fam)
        .bind(juomat)
        .execute(&db)
        .await
        .unwrap();

        teach_item_aisles(&db, fam, &[ing("Maito", Some("maito"))])
            .await
            .unwrap();

        // A new "maito" AISLE may exist now, but the mapping still points at Juomat.
        let mapped: i64 = sqlx::query_scalar(
            "SELECT aisle_id FROM item_aisles WHERE family_id = ? AND item_name = 'maito'",
        )
        .bind(fam)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(mapped, juomat);
    }

    #[tokio::test]
    async fn teach_matches_existing_aisle_case_insensitively() {
        let db = crate::db::test_pool().await;
        let fam = fixture_family(&db).await;
        sqlx::query(
            "INSERT INTO grocery_aisles (family_id, name, position) VALUES (?, 'Maito', 1)",
        )
        .bind(fam)
        .execute(&db)
        .await
        .unwrap();

        teach_item_aisles(&db, fam, &[ing("kerma", Some("maito"))])
            .await
            .unwrap();

        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM grocery_aisles WHERE family_id = ?")
            .bind(fam)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(n, 1); // matched "Maito", did not create "maito"
    }

    #[tokio::test]
    async fn teach_positions_multiple_new_aisles_sequentially() {
        let db = crate::db::test_pool().await;
        let fam = fixture_family(&db).await;
        // One pre-existing aisle at position 1; the import brings two new ones.
        sqlx::query(
            "INSERT INTO grocery_aisles (family_id, name, position) VALUES (?, 'Maito', 1)",
        )
        .bind(fam)
        .execute(&db)
        .await
        .unwrap();

        teach_item_aisles(
            &db,
            fam,
            &[
                ing("sipuli", Some("kasvikset")),
                ing("jauheliha", Some("liha")),
                ing("kerma", Some("maito")), // existing, case-insensitive match
            ],
        )
        .await
        .unwrap();

        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT name, position FROM grocery_aisles WHERE family_id = ? ORDER BY position",
        )
        .bind(fam)
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("Maito".to_owned(), 1),
                ("kasvikset".to_owned(), 2),
                ("liha".to_owned(), 3),
            ]
        );
    }
}

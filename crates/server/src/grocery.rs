use crate::AppState;
use crate::error::ApiError;
use crate::session::CurrentUser;
use crate::sync::poke;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use perkele_shared::grocery::{
    AddItemRequest, EditItemRequest, GroceryItem, SetCheckedRequest, SetCheckedResponse,
    SyncResponse, validate_grocery_item_name, validate_qty_unit, validate_section_name,
};
use perkele_shared::recipe::{ImportSummary, RecipeIngredient};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The family-wide ownership rule ("author-or-admin"): anyone may delete or
/// destructively overwrite content THEY created; an admin may do it to
/// anything in their family. Shared by grocery, recipes, calendar and notes so
/// the rule lives in one place. (Family scoping is the caller's job — every
/// handler has already looked the row up `WHERE family_id = ?`.)
///
/// Takes `&CurrentUser` (a borrow) because the check only reads two fields;
/// the handler keeps ownership of `user` for the rest of its work.
pub(crate) fn can_modify(user: &CurrentUser, created_by: i64) -> bool {
    created_by == user.user_id || user.role.is_admin()
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/grocery", get(list_items))
        .route("/api/grocery", post(add_item))
        .route("/api/grocery/sync", get(sync_items))
        .route("/api/grocery/{id}/check", put(set_checked))
        .route("/api/grocery/{id}", put(edit_item))
        .route("/api/grocery/{id}", delete(delete_item))
        .route("/api/grocery/clear-checked", post(clear_checked))
        .route("/api/grocery/section/{name}", delete(delete_section))
        .route("/api/grocery/events", get(crate::sync::sse_events))
}

async fn list_items(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<GroceryItem>>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT
               id         AS "id!: i64",
               name       AS "name!: String",
               qty        AS "qty?: String",
               unit       AS "unit?: String",
               category   AS "category?: String",
               checked    AS "checked!: bool",
               added_by   AS "added_by!: i64",
               recipe_id  AS "recipe_id?: i64",
               created_at AS "created_at!: String"
           FROM grocery_items
           WHERE family_id = ? AND deleted_at IS NULL
           ORDER BY checked ASC, created_at ASC"#,
        user.family_id,
    )
    .fetch_all(&state.db)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| GroceryItem {
            id: r.id,
            name: r.name,
            qty: r.qty,
            unit: r.unit,
            category: r.category,
            checked: r.checked,
            added_by: r.added_by,
            recipe_id: r.recipe_id,
            created_at: r.created_at,
        })
        .collect();
    Ok(Json(items))
}

/// Returns the full grocery list together with the family's current sync
/// sequence number. The client stores this seq in localStorage and compares
/// it on reconnect: if the stored seq matches the server seq, nothing changed
/// and the cached list is still fresh.
async fn sync_items(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<SyncResponse>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT
               id         AS "id!: i64",
               name       AS "name!: String",
               qty        AS "qty?: String",
               unit       AS "unit?: String",
               category   AS "category?: String",
               checked    AS "checked!: bool",
               added_by   AS "added_by!: i64",
               recipe_id  AS "recipe_id?: i64",
               created_at AS "created_at!: String"
           FROM grocery_items
           WHERE family_id = ? AND deleted_at IS NULL
           ORDER BY checked ASC, created_at ASC"#,
        user.family_id,
    )
    .fetch_all(&state.db)
    .await?;

    let items = rows
        .into_iter()
        .map(|r| GroceryItem {
            id: r.id,
            name: r.name,
            qty: r.qty,
            unit: r.unit,
            category: r.category,
            checked: r.checked,
            added_by: r.added_by,
            recipe_id: r.recipe_id,
            created_at: r.created_at,
        })
        .collect();

    // COALESCE handles the empty-table case, returning 0 when no mutations exist yet.
    let seq = sqlx::query_scalar!(
        r#"SELECT COALESCE(MAX(seq), 0) AS "seq!: i64" FROM sync_log WHERE family_id = ?"#,
        user.family_id,
    )
    .fetch_one(&state.db)
    .await?;

    Ok(Json(SyncResponse { items, seq }))
}

async fn add_item(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<AddItemRequest>,
) -> Result<(StatusCode, Json<GroceryItem>), ApiError> {
    validate_grocery_item_name(&req.name).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    validate_qty_unit(req.qty.as_deref(), req.unit.as_deref())
        .map_err(|m| ApiError::BadRequest(m.to_owned()))?;

    // Normalize before validating: a blank section means "no section" (what
    // the UI sends when the field is empty), so only a *present* section name
    // is validated. `as_deref` borrows the Option's String as &str so the
    // trim/filter chain doesn't consume `req.section`.
    let section = req
        .section
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    if let Some(s) = &section {
        validate_section_name(s).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    }

    let now = OffsetDateTime::now_utc();
    let created_at = now.format(&Rfc3339).map_err(|_| ApiError::Internal)?;

    let id = sqlx::query!(
        "INSERT INTO grocery_items (family_id, name, qty, unit, category, added_by, created_at)
         VALUES (?, ?, ?, ?, ?, ?, ?)",
        user.family_id,
        req.name,
        req.qty,
        req.unit,
        section,
        user.user_id,
        created_at,
    )
    .execute(&state.db)
    .await?
    .last_insert_rowid();

    append_sync_log(&state.db, user.family_id, id, "insert").await?;

    // Audit: capture the display name before it moves into the response body.
    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "grocery_item",
        Some(id),
        "create",
        &req.name,
    )
    .await?;

    poke(&state, user.family_id).await;

    Ok((
        StatusCode::CREATED,
        Json(GroceryItem {
            id,
            name: req.name,
            qty: req.qty,
            unit: req.unit,
            category: section,
            checked: false,
            added_by: user.user_id,
            recipe_id: None,
            created_at,
        }),
    ))
}

/// Set an item's checked state to exactly what the client asked for.
/// Idempotent by design — see `SetCheckedRequest` for why that matters.
async fn set_checked(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SetCheckedRequest>,
) -> Result<Json<SetCheckedResponse>, ApiError> {
    let rows = sqlx::query!(
        "UPDATE grocery_items SET checked = ?
         WHERE id = ? AND family_id = ? AND deleted_at IS NULL",
        req.checked,
        id,
        user.family_id,
    )
    .execute(&state.db)
    .await?
    .rows_affected();

    if rows == 0 {
        return Err(ApiError::NotFound);
    }

    // Return the seq this write produced: the client raises its stale-snapshot
    // watermark to it, so an in-flight sync that predates this check can't
    // visually revert it (the checkbox-flicker bug).
    let seq = append_sync_log(&state.db, user.family_id, id, "update").await?;
    poke(&state, user.family_id).await;
    Ok(Json(SetCheckedResponse { seq }))
}

/// Edit an existing item's text fields (name/qty/unit). Any family member may
/// edit any item — same trust level as checking off or clearing another
/// member's items, so there is no `added_by` ownership check (unlike delete).
/// The family-scoped WHERE makes a wrong id or another family's item a 404.
async fn edit_item(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<EditItemRequest>,
) -> Result<StatusCode, ApiError> {
    validate_grocery_item_name(&req.name).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    validate_qty_unit(req.qty.as_deref(), req.unit.as_deref())
        .map_err(|m| ApiError::BadRequest(m.to_owned()))?;

    let rows = sqlx::query!(
        "UPDATE grocery_items SET name = ?, qty = ?, unit = ?
         WHERE id = ? AND family_id = ? AND deleted_at IS NULL",
        req.name,
        req.qty,
        req.unit,
        id,
        user.family_id,
    )
    .execute(&state.db)
    .await?
    .rows_affected();

    if rows == 0 {
        return Err(ApiError::NotFound);
    }

    // Same "update" sync_log entry set_checked writes, so every open client
    // refetches and converges on the edited text.
    append_sync_log(&state.db, user.family_id, id, "update").await?;

    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "grocery_item",
        Some(id),
        "update",
        &req.name,
    )
    .await?;

    poke(&state, user.family_id).await;
    Ok(StatusCode::OK)
}

async fn delete_item(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    let item = sqlx::query!(
        r#"SELECT added_by AS "added_by!: i64", name AS "name!: String"
           FROM grocery_items
           WHERE id = ? AND family_id = ? AND deleted_at IS NULL"#,
        id,
        user.family_id,
    )
    .fetch_optional(&state.db)
    .await?;

    let Some(item) = item else {
        return Err(ApiError::NotFound);
    };

    if !can_modify(&user, item.added_by) {
        return Err(ApiError::Forbidden);
    }

    let now = OffsetDateTime::now_utc();
    sqlx::query!(
        "UPDATE grocery_items SET deleted_at = ? WHERE id = ?",
        now,
        id,
    )
    .execute(&state.db)
    .await?;

    append_sync_log(&state.db, user.family_id, id, "delete").await?;

    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "grocery_item",
        Some(id),
        "delete",
        &item.name,
    )
    .await?;

    poke(&state, user.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Deliberately open to every member, unlike single-item delete: checking
/// items off is a shared chore (whoever did the shopping ticks everything,
/// regardless of who added it), and clearing the checked pile is the tail end
/// of that same act. It only removes items someone already marked as bought,
/// so it can't wipe out a live shopping list the way a section delete could.
async fn clear_checked(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<StatusCode, ApiError> {
    let now = OffsetDateTime::now_utc();

    let ids: Vec<i64> = sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64"
           FROM grocery_items
           WHERE family_id = ? AND checked = 1 AND deleted_at IS NULL"#,
        user.family_id,
    )
    .fetch_all(&state.db)
    .await?;

    if ids.is_empty() {
        return Ok(StatusCode::NO_CONTENT);
    }

    // Captured before the `for id in ids` loop below consumes `ids`.
    let ids_len = ids.len();

    sqlx::query!(
        "UPDATE grocery_items SET deleted_at = ?
         WHERE family_id = ? AND checked = 1 AND deleted_at IS NULL",
        now,
        user.family_id,
    )
    .execute(&state.db)
    .await?;

    for id in ids {
        append_sync_log(&state.db, user.family_id, id, "delete").await?;
    }

    // Bulk op → ONE audit row with a count label, not N (spec).
    let label = format!("{} ostosta", ids_len);
    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "grocery_item",
        None,
        "delete",
        &label,
    )
    .await?;

    poke(&state, user.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Remove a whole section: soft-delete every live item in the caller's family
/// carrying this section name, one sync_log "delete" per item. Axum
/// percent-decodes `{name}`, so "Äidin apteekki" arrives as written.
///
/// Ownership: author-or-admin, applied ALL-OR-NOTHING. An admin may remove any
/// section; anyone else only a section whose items are all their own (e.g. a
/// "Rautakauppa" list they made themselves). A section holding someone else's
/// item is refused outright with 403 rather than silently deleting just the
/// caller's items — "Poista osio" promises the section disappears, and a
/// half-deleted section that stays on screen would be more confusing than a
/// clear "not allowed". The app hides the button in exactly that case.
async fn delete_section(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT id AS "id!: i64", added_by AS "added_by!: i64"
           FROM grocery_items
           WHERE family_id = ? AND category = ? AND deleted_at IS NULL"#,
        user.family_id,
        name,
    )
    .fetch_all(&state.db)
    .await?;

    if rows.is_empty() {
        return Ok(StatusCode::NO_CONTENT);
    }
    // `.all(..)` short-circuits on the first item the caller may not touch.
    if !rows.iter().all(|r| can_modify(&user, r.added_by)) {
        return Err(ApiError::Forbidden);
    }
    let now = OffsetDateTime::now_utc();
    // Tombstone exactly the rows checked above, by id — NOT a second
    // `WHERE category = ?` sweep, which could also catch an item someone else
    // added to the section in the gap between the SELECT and this UPDATE
    // (and so delete it without the permission check). Same SQL as
    // `delete_item`; the ids were already scoped to the caller's family.
    for r in rows {
        sqlx::query!(
            "UPDATE grocery_items SET deleted_at = ? WHERE id = ?",
            now,
            r.id,
        )
        .execute(&state.db)
        .await?;
        append_sync_log(&state.db, user.family_id, r.id, "delete").await?;
    }

    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "grocery_item",
        None,
        "delete",
        &name,
    )
    .await?;

    poke(&state, user.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

pub(crate) async fn append_sync_log(
    db: &crate::db::Db,
    family_id: i64,
    entity_id: i64,
    op: &str,
) -> Result<i64, ApiError> {
    append_sync_log_entity(db, family_id, entity_id, "grocery_item", op).await
}

/// Append a sync_log row for any entity type (per-family monotonic seq).
/// Returns the seq it wrote, so mutation handlers can hand it to the client
/// (which uses it to reject sync snapshots older than its own write).
pub(crate) async fn append_sync_log_entity(
    db: &crate::db::Db,
    family_id: i64,
    entity_id: i64,
    entity: &str,
    op: &str,
) -> Result<i64, ApiError> {
    let now = OffsetDateTime::now_utc();
    // RETURNING hands back the row we just inserted — one round trip, no
    // re-SELECT race with a concurrent writer bumping MAX(seq).
    let seq = sqlx::query_scalar!(
        r#"INSERT INTO sync_log (family_id, seq, entity, entity_id, op, updated_at)
         VALUES (
             ?,
             COALESCE((SELECT MAX(seq) FROM sync_log WHERE family_id = ?), 0) + 1,
             ?,
             ?,
             ?,
             ?
         )
         RETURNING seq AS "seq!: i64""#,
        family_id,
        family_id,
        entity,
        entity_id,
        op,
        now,
    )
    .fetch_one(db)
    .await?;
    Ok(seq)
}

/// Insert recipe ingredients into the grocery list without creating
/// duplicates. Matching is by trimmed, lowercased name — lowercased in RUST,
/// because SQLite's LOWER() only folds ASCII and this family shops for
/// "Öljyä" and "Äitienpäiväkakkua". Per ingredient:
///   - an UNCHECKED item with that name exists  → skip (already being bought)
///   - only a CHECKED item exists (a staple)    → revive it: checked = 0
///   - no match                                 → insert a new row
///
/// Writes a sync_log row per mutation; the CALLER pokes SSE once afterwards.
/// Ingredient categories never become sections; they feed the aisle map.
pub(crate) async fn add_ingredients_dedup(
    db: &crate::db::Db,
    family_id: i64,
    user_id: i64,
    ingredients: &[RecipeIngredient],
    recipe_id: Option<i64>,
) -> Result<ImportSummary, ApiError> {
    // Ingredient categories are AISLE names, not store sections. Teach the
    // family's item→aisle map from them; the items themselves stay
    // sectionless (category = NULL below).
    crate::aisle::teach_item_aisles(db, family_id, ingredients).await?;

    // Snapshot the live list once: name-key -> (id, checked). If a name
    // appears twice, an unchecked row wins (skipping beats double-reviving).
    let rows = sqlx::query!(
        r#"SELECT
               id      AS "id!: i64",
               name    AS "name!: String",
               checked AS "checked!: bool"
           FROM grocery_items
           WHERE family_id = ? AND deleted_at IS NULL"#,
        family_id,
    )
    .fetch_all(db)
    .await?;

    let mut existing: std::collections::HashMap<String, (i64, bool)> =
        std::collections::HashMap::new();
    for row in rows {
        let key = row.name.trim().to_lowercase();
        // Keep an existing unchecked entry; otherwise take this row.
        if !matches!(existing.get(&key), Some((_, false))) {
            existing.insert(key, (row.id, row.checked));
        }
    }

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)?;

    let mut summary = ImportSummary {
        added: 0,
        skipped: 0,
        unchecked: 0,
    };
    for ing in ingredients {
        let key = ing.name.trim().to_lowercase();
        match existing.get(&key) {
            Some((_, false)) => {
                summary.skipped += 1;
            }
            Some(&(id, true)) => {
                sqlx::query!("UPDATE grocery_items SET checked = 0 WHERE id = ?", id)
                    .execute(db)
                    .await?;
                append_sync_log(db, family_id, id, "update").await?;
                summary.unchecked += 1;
                // Now unchecked — a later same-name ingredient must skip,
                // not revive again (which would double-count).
                existing.insert(key, (id, false));
            }
            None => {
                let id = sqlx::query!(
                    "INSERT INTO grocery_items
                        (family_id, name, qty, unit, added_by, recipe_id, created_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                    family_id,
                    ing.name,
                    ing.qty,
                    ing.unit,
                    user_id,
                    recipe_id,
                    now,
                )
                .execute(db)
                .await?
                .last_insert_rowid();
                append_sync_log(db, family_id, id, "insert").await?;
                summary.added += 1;
                existing.insert(key, (id, false));
            }
        }
    }
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn grocery_app() -> (axum::Router, crate::db::Db) {
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

    async fn add_member(app: &axum::Router, admin_cookie: &str) -> String {
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/family/invites",
                json!({ "role": "member" }),
                admin_cookie,
            ))
            .await
            .unwrap();
        let code = json_body(resp).await["code"].as_str().unwrap().to_owned();

        let resp = app
            .clone()
            .oneshot(post(
                "/api/auth/redeem",
                json!({
                    "code": code,
                    "username": "matti",
                    "display_name": "Matti",
                    "password": "hunter2!"
                }),
            ))
            .await
            .unwrap();
        cookie_pair(&resp)
    }

    #[tokio::test]
    async fn grocery_requires_auth() {
        let app = grocery_app().await.0;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/grocery")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn add_item_appears_in_list() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "milk", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let item = json_body(resp).await;
        assert_eq!(item["name"], "milk");
        assert_eq!(item["checked"], false);
        assert_eq!(item["qty"], Value::Null);

        let resp = app
            .oneshot(get_auth("/api/grocery", &cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list = json_body(resp).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["name"], "milk");
    }

    #[tokio::test]
    async fn add_item_with_qty_and_unit() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        let resp = app
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "flour", "qty": "500", "unit": "g" }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let item = json_body(resp).await;
        assert_eq!(item["qty"], "500");
        assert_eq!(item["unit"], "g");
    }

    #[tokio::test]
    async fn add_item_empty_name_is_rejected() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;
        let resp = app
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "   ", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn list_is_scoped_to_family() {
        let (app, _db) = grocery_app().await;
        let admin = setup_admin(&app).await;

        app.clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "butter", "qty": null, "unit": null }),
                &admin,
            ))
            .await
            .unwrap();

        let member = add_member(&app, &admin).await;
        let resp = app
            .oneshot(get_auth("/api/grocery", &member))
            .await
            .unwrap();
        let list = json_body(resp).await;
        assert_eq!(list[0]["name"], "butter");
    }

    #[tokio::test]
    async fn set_checked_stores_the_requested_state() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "eggs", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let resp = app
            .clone()
            .oneshot(put_auth(
                &format!("/api/grocery/{id}/check"),
                json!({ "checked": true }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .clone()
            .oneshot(get_auth("/api/grocery", &cookie))
            .await
            .unwrap();
        assert_eq!(json_body(resp).await[0]["checked"], true);

        app.clone()
            .oneshot(put_auth(
                &format!("/api/grocery/{id}/check"),
                json!({ "checked": false }),
                &cookie,
            ))
            .await
            .unwrap();

        let resp = app
            .oneshot(get_auth("/api/grocery", &cookie))
            .await
            .unwrap();
        assert_eq!(json_body(resp).await[0]["checked"], false);
    }

    /// The store-aisle regression: a duplicate or late-arriving request must
    /// not undo the user's intent. "Set checked = true" twice stays checked —
    /// unlike the old flip semantics, where the second request reverted it.
    #[tokio::test]
    async fn set_checked_is_idempotent() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "milk", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(put_auth(
                    &format!("/api/grocery/{id}/check"),
                    json!({ "checked": true }),
                    &cookie,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }

        let resp = app
            .oneshot(get_auth("/api/grocery", &cookie))
            .await
            .unwrap();
        assert_eq!(json_body(resp).await[0]["checked"], true);
    }

    /// The flicker fix: the client needs to know the seq its own check-off
    /// produced, so it can reject sync snapshots fetched before the write
    /// landed (they'd briefly "uncheck" the item). The response body carries
    /// the new seq, and it must match what /sync reports afterwards.
    #[tokio::test]
    async fn set_checked_returns_the_new_sync_seq() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "milk", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let resp = app
            .clone()
            .oneshot(put_auth(
                &format!("/api/grocery/{id}/check"),
                json!({ "checked": true }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        // The add wrote seq 1, so the check-off is seq 2.
        let seq = json_body(resp).await["seq"].as_i64().unwrap();
        assert_eq!(seq, 2);

        let resp = app
            .oneshot(get_auth("/api/grocery/sync", &cookie))
            .await
            .unwrap();
        assert_eq!(json_body(resp).await["seq"], seq);
    }

    #[tokio::test]
    async fn set_checked_nonexistent_item_is_not_found() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;
        let resp = app
            .oneshot(put_auth(
                "/api/grocery/9999/check",
                json!({ "checked": true }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn edit_item_updates_name_qty_unit() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "milk", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let resp = app
            .clone()
            .oneshot(put_auth(
                &format!("/api/grocery/{id}"),
                json!({ "name": "oat milk", "qty": "2", "unit": "L" }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(get_auth("/api/grocery", &cookie))
            .await
            .unwrap();
        let list = json_body(resp).await;
        assert_eq!(list[0]["name"], "oat milk");
        assert_eq!(list[0]["qty"], "2");
        assert_eq!(list[0]["unit"], "L");
    }

    #[tokio::test]
    async fn overlong_qty_or_unit_rejected_on_add_and_edit() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;
        let long = "x".repeat(perkele_shared::grocery::QTY_UNIT_MAX + 1);
        for body in [
            json!({ "name": "milk", "qty": long, "unit": null }),
            json!({ "name": "milk", "qty": null, "unit": long }),
        ] {
            let resp = app
                .clone()
                .oneshot(post_auth("/api/grocery", body, &cookie))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "milk", "qty": "2", "unit": "l" }),
                &cookie,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();
        for body in [
            json!({ "name": "milk", "qty": long, "unit": null }),
            json!({ "name": "milk", "qty": null, "unit": long }),
        ] {
            let resp = app
                .clone()
                .oneshot(put_auth(&format!("/api/grocery/{id}"), body, &cookie))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn edit_item_empty_name_is_rejected() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "milk", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let resp = app
            .oneshot(put_auth(
                &format!("/api/grocery/{id}"),
                json!({ "name": "   ", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn edit_item_in_another_family_is_not_found() {
        // Genuine cross-family check: seed a second family + user + session directly
        // via raw sqlx (bypassing /api/setup, which only runs once per DB), same as
        // delete_cross_family_item_is_not_found below. This proves the UPDATE's WHERE
        // clause is actually scoped by family_id — a nonexistent id in the *same*
        // family would 404 too, but wouldn't catch a dropped family_id scope.
        let (app, db) = grocery_app().await;
        let admin = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "jam", "qty": null, "unit": null }),
                &admin,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let now = time::OffsetDateTime::now_utc();
        let fid: i64 = sqlx::query_scalar(
            "INSERT INTO families (name, created_at) VALUES ('Other', ?) RETURNING id",
        )
        .bind(now)
        .fetch_one(&db)
        .await
        .unwrap();
        let hash = crate::auth::hash_password("password1").unwrap();
        let uid: i64 = sqlx::query_scalar(
            "INSERT INTO users (family_id, username, display_name, pw_hash, role, created_at)
             VALUES (?, 'outsider', 'Outsider', ?, 'member', ?) RETURNING id",
        )
        .bind(fid)
        .bind(hash)
        .bind(now)
        .fetch_one(&db)
        .await
        .unwrap();
        let token = crate::auth::generate_token();
        let token_hash = crate::auth::hash_token(&token);
        let expires = now + time::Duration::days(30);
        sqlx::query(
            "INSERT INTO sessions (user_id, token_hash, expires_at, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(uid)
        .bind(&token_hash)
        .bind(expires)
        .bind(now)
        .execute(&db)
        .await
        .unwrap();
        let outsider_cookie = format!("perkele_session={token}");

        let resp = app
            .clone()
            .oneshot(put_auth(
                &format!("/api/grocery/{id}"),
                json!({ "name": "stolen", "qty": null, "unit": null }),
                &outsider_cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // The item itself must be untouched — confirm via the owning family's list.
        let resp = app.oneshot(get_auth("/api/grocery", &admin)).await.unwrap();
        let list = json_body(resp).await;
        assert_eq!(list[0]["name"], "jam");
    }

    #[tokio::test]
    async fn edit_item_by_another_family_member_succeeds() {
        // Deliberate no-ownership-check behavior: unlike delete (which is
        // owner-or-admin only, see member_cannot_delete_another_users_item),
        // edit is scoped to the family only — any member may edit any item.
        let app = grocery_app().await.0;
        let admin = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "cheese", "qty": null, "unit": null }),
                &admin,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let member = add_member(&app, &admin).await;
        let resp = app
            .clone()
            .oneshot(put_auth(
                &format!("/api/grocery/{id}"),
                json!({ "name": "cheddar", "qty": "200", "unit": "g" }),
                &member,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app.oneshot(get_auth("/api/grocery", &admin)).await.unwrap();
        let list = json_body(resp).await;
        assert_eq!(list[0]["name"], "cheddar");
        assert_eq!(list[0]["qty"], "200");
        assert_eq!(list[0]["unit"], "g");
    }

    #[tokio::test]
    async fn edit_item_writes_an_update_sync_log_row() {
        let (app, db) = grocery_app().await;
        let cookie = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "bread", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        app.oneshot(put_auth(
            &format!("/api/grocery/{id}"),
            json!({ "name": "rye bread", "qty": null, "unit": null }),
            &cookie,
        ))
        .await
        .unwrap();

        let log: Vec<(i64, String, String)> =
            sqlx::query_as("SELECT seq, entity, op FROM sync_log ORDER BY seq")
                .fetch_all(&db)
                .await
                .unwrap();
        // seq 1 = insert, seq 2 = the edit's update.
        assert_eq!(log[1], (2, "grocery_item".to_owned(), "update".to_owned()));
    }

    #[tokio::test]
    async fn delete_own_item_removes_it_from_list() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "butter", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let resp = app
            .clone()
            .oneshot(delete_auth(&format!("/api/grocery/{id}"), &cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app
            .oneshot(get_auth("/api/grocery", &cookie))
            .await
            .unwrap();
        assert!(json_body(resp).await.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn member_cannot_delete_another_users_item() {
        let app = grocery_app().await.0;
        let admin = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "cheese", "qty": null, "unit": null }),
                &admin,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let member = add_member(&app, &admin).await;
        let resp = app
            .oneshot(delete_auth(&format!("/api/grocery/{id}"), &member))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn delete_cross_family_item_is_not_found() {
        let (app, db) = grocery_app().await;
        let admin = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "jam", "qty": null, "unit": null }),
                &admin,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let now = time::OffsetDateTime::now_utc();
        let fid: i64 = sqlx::query_scalar(
            "INSERT INTO families (name, created_at) VALUES ('Other', ?) RETURNING id",
        )
        .bind(now)
        .fetch_one(&db)
        .await
        .unwrap();
        let hash = crate::auth::hash_password("password1").unwrap();
        let uid: i64 = sqlx::query_scalar(
            "INSERT INTO users (family_id, username, display_name, pw_hash, role, created_at)
             VALUES (?, 'outsider', 'Outsider', ?, 'member', ?) RETURNING id",
        )
        .bind(fid)
        .bind(hash)
        .bind(now)
        .fetch_one(&db)
        .await
        .unwrap();
        let token = crate::auth::generate_token();
        let token_hash = crate::auth::hash_token(&token);
        let expires = now + time::Duration::days(30);
        sqlx::query(
            "INSERT INTO sessions (user_id, token_hash, expires_at, created_at) VALUES (?, ?, ?, ?)",
        )
        .bind(uid)
        .bind(&token_hash)
        .bind(expires)
        .bind(now)
        .execute(&db)
        .await
        .unwrap();
        let outsider_cookie = format!("perkele_session={token}");

        let resp = app
            .oneshot(delete_auth(&format!("/api/grocery/{id}"), &outsider_cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn clear_checked_tombstones_checked_items_only() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        let add = |name: &'static str| {
            let app = app.clone();
            let cookie = cookie.clone();
            async move {
                let resp = app
                    .oneshot(post_auth(
                        "/api/grocery",
                        json!({ "name": name, "qty": null, "unit": null }),
                        &cookie,
                    ))
                    .await
                    .unwrap();
                json_body(resp).await["id"].as_i64().unwrap()
            }
        };
        let id1 = add("milk").await;
        let _id2 = add("eggs").await;

        app.clone()
            .oneshot(put_auth(
                &format!("/api/grocery/{id1}/check"),
                json!({ "checked": true }),
                &cookie,
            ))
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(post_auth("/api/grocery/clear-checked", json!({}), &cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app
            .oneshot(get_auth("/api/grocery", &cookie))
            .await
            .unwrap();
        let list = json_body(resp).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["name"], "eggs");
    }

    #[tokio::test]
    async fn sync_log_records_add_and_delete() {
        let (app, db) = grocery_app().await;
        let cookie = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "bread", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let log: Vec<(i64, String, String)> =
            sqlx::query_as("SELECT seq, entity, op FROM sync_log ORDER BY seq")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0], (1, "grocery_item".to_owned(), "insert".to_owned()));

        app.oneshot(delete_auth(&format!("/api/grocery/{id}"), &cookie))
            .await
            .unwrap();

        let log: Vec<(i64, String, String)> =
            sqlx::query_as("SELECT seq, entity, op FROM sync_log ORDER BY seq")
                .fetch_all(&db)
                .await
                .unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[1], (2, "grocery_item".to_owned(), "delete".to_owned()));
    }

    #[tokio::test]
    async fn sync_log_clear_checked_writes_one_entry_per_item() {
        let (app, db) = grocery_app().await;
        let cookie = setup_admin(&app).await;

        for name in ["milk", "eggs"] {
            let resp = app
                .clone()
                .oneshot(post_auth(
                    "/api/grocery",
                    json!({ "name": name, "qty": null, "unit": null }),
                    &cookie,
                ))
                .await
                .unwrap();
            let id = json_body(resp).await["id"].as_i64().unwrap();
            app.clone()
                .oneshot(put_auth(
                    &format!("/api/grocery/{id}/check"),
                    json!({ "checked": true }),
                    &cookie,
                ))
                .await
                .unwrap();
        }

        app.clone()
            .oneshot(post_auth("/api/grocery/clear-checked", json!({}), &cookie))
            .await
            .unwrap();

        // 2 inserts + 2 toggles + 2 clear-checked deletes = 6 entries
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sync_log")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 6);

        let last_two: Vec<String> =
            sqlx::query_scalar("SELECT op FROM sync_log ORDER BY seq DESC LIMIT 2")
                .fetch_all(&db)
                .await
                .unwrap();
        assert!(last_two.iter().all(|op| op == "delete"));
    }

    #[tokio::test]
    async fn sync_returns_items_and_seq() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        // Fresh family: seq is 0, list is empty.
        let resp = app
            .clone()
            .oneshot(get_auth("/api/grocery/sync", &cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["seq"], 0);
        assert!(body["items"].as_array().unwrap().is_empty());

        // Add an item — seq advances to 1.
        app.clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "milk", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();

        let resp = app
            .oneshot(get_auth("/api/grocery/sync", &cookie))
            .await
            .unwrap();
        let body = json_body(resp).await;
        assert_eq!(body["seq"], 1);
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        assert_eq!(body["items"][0]["name"], "milk");
    }

    #[tokio::test]
    async fn add_item_with_section_round_trips() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "Burana", "qty": null, "unit": null, "section": "Apteekki" }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(json_body(resp).await["category"], "Apteekki");

        let resp = app
            .oneshot(get_auth("/api/grocery/sync", &cookie))
            .await
            .unwrap();
        assert_eq!(json_body(resp).await["items"][0]["category"], "Apteekki");
    }

    #[tokio::test]
    async fn add_item_section_is_trimmed_and_blank_becomes_null() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "Burana", "qty": null, "unit": null, "section": "  Apteekki  " }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(json_body(resp).await["category"], "Apteekki");

        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "milk", "qty": null, "unit": null, "section": "   " }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(json_body(resp).await["category"], Value::Null);
    }

    #[tokio::test]
    async fn add_item_overlong_section_is_rejected() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;
        let resp = app
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "milk", "qty": null, "unit": null, "section": "x".repeat(41) }),
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn delete_section_tombstones_only_that_sections_items() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        for (name, section) in [
            ("Burana", Some("Apteekki")),
            ("milk", None),
            ("Panadol", Some("Apteekki")),
        ] {
            app.clone()
                .oneshot(post_auth(
                    "/api/grocery",
                    json!({ "name": name, "qty": null, "unit": null, "section": section }),
                    &cookie,
                ))
                .await
                .unwrap();
        }

        let resp = app
            .clone()
            .oneshot(delete_auth("/api/grocery/section/Apteekki", &cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app
            .oneshot(get_auth("/api/grocery", &cookie))
            .await
            .unwrap();
        let list = json_body(resp).await;
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["name"], "milk");
    }

    /// Author-or-admin: a member may NOT remove a section that holds someone
    /// else's items — that would bulk-delete content they didn't create.
    #[tokio::test]
    async fn member_cannot_delete_section_with_others_items() {
        let app = grocery_app().await.0;
        let admin = setup_admin(&app).await;

        app.clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "Burana", "qty": null, "unit": null, "section": "Apteekki" }),
                &admin,
            ))
            .await
            .unwrap();

        let member = add_member(&app, &admin).await;
        // Mixed section: the member's own item doesn't unlock the admin's.
        app.clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "Laastari", "qty": null, "unit": null, "section": "Apteekki" }),
                &member,
            ))
            .await
            .unwrap();
        let resp = app
            .clone()
            .oneshot(delete_auth("/api/grocery/section/Apteekki", &member))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        // All-or-nothing: nothing was deleted, not even the member's own item.
        let resp = app.oneshot(get_auth("/api/grocery", &admin)).await.unwrap();
        assert_eq!(json_body(resp).await.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn member_can_delete_section_of_own_items() {
        let app = grocery_app().await.0;
        let admin = setup_admin(&app).await;
        let member = add_member(&app, &admin).await;

        app.clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "naulat", "qty": null, "unit": null, "section": "Rauta" }),
                &member,
            ))
            .await
            .unwrap();
        let resp = app
            .clone()
            .oneshot(delete_auth("/api/grocery/section/Rauta", &member))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app.oneshot(get_auth("/api/grocery", &admin)).await.unwrap();
        assert!(json_body(resp).await.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn admin_can_delete_section_with_others_items() {
        let app = grocery_app().await.0;
        let admin = setup_admin(&app).await;
        let member = add_member(&app, &admin).await;

        app.clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "naulat", "qty": null, "unit": null, "section": "Rauta" }),
                &member,
            ))
            .await
            .unwrap();
        let resp = app
            .clone()
            .oneshot(delete_auth("/api/grocery/section/Rauta", &admin))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app.oneshot(get_auth("/api/grocery", &admin)).await.unwrap();
        assert!(json_body(resp).await.as_array().unwrap().is_empty());
    }

    /// Clearing checked items stays open to everyone on purpose (see the
    /// handler's comment): a member clears the admin's checked item.
    #[tokio::test]
    async fn member_can_clear_others_checked_items() {
        let app = grocery_app().await.0;
        let admin = setup_admin(&app).await;
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "milk", "qty": null, "unit": null }),
                &admin,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();
        let member = add_member(&app, &admin).await;
        app.clone()
            .oneshot(put_auth(
                &format!("/api/grocery/{id}/check"),
                json!({ "checked": true }),
                &member,
            ))
            .await
            .unwrap();
        let resp = app
            .clone()
            .oneshot(post_auth("/api/grocery/clear-checked", json!({}), &member))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let resp = app.oneshot(get_auth("/api/grocery", &admin)).await.unwrap();
        assert!(json_body(resp).await.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_section_is_scoped_to_family() {
        let (app, db) = grocery_app().await;
        let admin = setup_admin(&app).await;

        // Hand-build a second family with one "Apteekki" item.
        let now = time::OffsetDateTime::now_utc();
        let fid: i64 = sqlx::query_scalar(
            "INSERT INTO families (name, created_at) VALUES ('Other', ?) RETURNING id",
        )
        .bind(now)
        .fetch_one(&db)
        .await
        .unwrap();
        let hash = crate::auth::hash_password("password1").unwrap();
        let uid: i64 = sqlx::query_scalar(
            "INSERT INTO users (family_id, username, display_name, pw_hash, role, created_at)
             VALUES (?, 'outsider', 'Outsider', ?, 'member', ?) RETURNING id",
        )
        .bind(fid)
        .bind(hash)
        .bind(now)
        .fetch_one(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO grocery_items (family_id, name, category, added_by, created_at)
             VALUES (?, 'Burana', 'Apteekki', ?, ?)",
        )
        .bind(fid)
        .bind(uid)
        .bind(
            now.format(&time::format_description::well_known::Rfc3339)
                .unwrap(),
        )
        .execute(&db)
        .await
        .unwrap();

        // Admin (family 1) deletes their "Apteekki" — the other family's
        // identically-named section must survive.
        let resp = app
            .clone()
            .oneshot(delete_auth("/api/grocery/section/Apteekki", &admin))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let survivors: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM grocery_items WHERE family_id = ? AND deleted_at IS NULL",
        )
        .bind(fid)
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(survivors, 1);
    }

    #[tokio::test]
    async fn delete_section_writes_sync_log_delete_per_item() {
        let (app, db) = grocery_app().await;
        let cookie = setup_admin(&app).await;

        for name in ["Burana", "Panadol"] {
            app.clone()
                .oneshot(post_auth(
                    "/api/grocery",
                    json!({ "name": name, "qty": null, "unit": null, "section": "Apteekki" }),
                    &cookie,
                ))
                .await
                .unwrap();
        }

        app.clone()
            .oneshot(delete_auth("/api/grocery/section/Apteekki", &cookie))
            .await
            .unwrap();

        // 2 inserts + 2 section deletes = 4 entries
        let ops: Vec<String> = sqlx::query_scalar("SELECT op FROM sync_log ORDER BY seq")
            .fetch_all(&db)
            .await
            .unwrap();
        assert_eq!(ops, ["insert", "insert", "delete", "delete"]);
    }

    /// Section names travel in the URL path, so non-ASCII names arrive
    /// percent-encoded and must decode back before matching.
    #[tokio::test]
    async fn delete_section_decodes_url_encoded_name() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;

        app.clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "Burana", "qty": null, "unit": null, "section": "Äidin apteekki" }),
                &cookie,
            ))
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(delete_auth(
                "/api/grocery/section/%C3%84idin%20apteekki",
                &cookie,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app
            .oneshot(get_auth("/api/grocery", &cookie))
            .await
            .unwrap();
        assert!(json_body(resp).await.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn delete_nonexistent_section_is_a_no_op() {
        let (app, db) = grocery_app().await;
        let cookie = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(delete_auth("/api/grocery/section/Apteekki", &cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sync_log")
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn sse_without_auth_is_unauthorized() {
        let app = grocery_app().await.0;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/grocery/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn sse_with_auth_returns_event_stream() {
        let app = grocery_app().await.0;
        let cookie = setup_admin(&app).await;
        let resp = app
            .oneshot(get_auth("/api/grocery/events", &cookie))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get(header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.starts_with("text/event-stream"), "got: {ct}");
    }

    /// Drive the helper directly: seed a family via the API, then look the
    /// ids up from the DB (the helper takes raw ids, not a session).
    #[tokio::test]
    async fn dedup_helper_skips_unchecked_revives_checked_inserts_new() {
        let (app, db) = grocery_app().await;
        let cookie = setup_admin(&app).await;
        let (uid, fid): (i64, i64) = sqlx::query_as("SELECT id, family_id FROM users LIMIT 1")
            .fetch_one(&db)
            .await
            .unwrap();

        // On the list already: "Sipuli" unchecked, "ÖLJY" checked (a staple
        // resting at the bottom). Casing differs from the incoming rows on
        // purpose — matching must fold Finnish letters, which SQLite's
        // ASCII-only LOWER() can't do; that's why the fold happens in Rust.
        for (name, checked) in [("Sipuli", false), ("ÖLJY", true)] {
            let resp = app
                .clone()
                .oneshot(post_auth(
                    "/api/grocery",
                    json!({ "name": name, "qty": null, "unit": null }),
                    &cookie,
                ))
                .await
                .unwrap();
            let id = json_body(resp).await["id"].as_i64().unwrap();
            if checked {
                app.clone()
                    .oneshot(put_auth(
                        &format!("/api/grocery/{id}/check"),
                        json!({ "checked": true }),
                        &cookie,
                    ))
                    .await
                    .unwrap();
            }
        }

        let incoming = vec![
            perkele_shared::recipe::RecipeIngredient {
                name: "sipuli".into(),
                qty: Some("3".into()),
                unit: None,
                category: None,
            },
            perkele_shared::recipe::RecipeIngredient {
                name: "öljy".into(),
                qty: Some("1".into()),
                unit: Some("dl".into()),
                category: None,
            },
            perkele_shared::recipe::RecipeIngredient {
                name: "jauheliha".into(),
                qty: Some("400".into()),
                unit: Some("g".into()),
                category: Some("liha".into()),
            },
        ];

        let summary = add_ingredients_dedup(&db, fid, uid, &incoming, Some(42))
            .await
            .unwrap();
        assert_eq!(summary.added, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.unchecked, 1);

        // List now: sipuli (untouched), öljy (revived → unchecked, qty NOT
        // overwritten), jauheliha (new row carrying recipe_id 42).
        let resp = app
            .oneshot(get_auth("/api/grocery", &cookie))
            .await
            .unwrap();
        let list = json_body(resp).await;
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 3);
        let oljy = arr.iter().find(|i| i["name"] == "ÖLJY").unwrap();
        assert_eq!(oljy["checked"], false);
        assert_eq!(oljy["qty"], Value::Null);
        let uusi = arr.iter().find(|i| i["name"] == "jauheliha").unwrap();
        assert_eq!(uusi["recipe_id"], 42);
        assert_eq!(uusi["qty"], "400");
    }

    /// Two incoming rows with the same name but different units (merge keeps
    /// them separate): the first inserts, the second must see the first and
    /// skip — the in-batch map update, not just the initial DB snapshot.
    #[tokio::test]
    async fn dedup_helper_dedups_within_the_batch_too() {
        let (app, db) = grocery_app().await;
        let cookie = setup_admin(&app).await;
        let (uid, fid): (i64, i64) = sqlx::query_as("SELECT id, family_id FROM users LIMIT 1")
            .fetch_one(&db)
            .await
            .unwrap();

        let incoming = vec![
            perkele_shared::recipe::RecipeIngredient {
                name: "maito".into(),
                qty: Some("2".into()),
                unit: Some("dl".into()),
                category: None,
            },
            perkele_shared::recipe::RecipeIngredient {
                name: "Maito".into(),
                qty: Some("1".into()),
                unit: Some("l".into()),
                category: None,
            },
        ];
        let summary = add_ingredients_dedup(&db, fid, uid, &incoming, None)
            .await
            .unwrap();
        assert_eq!(summary.added, 1);
        assert_eq!(summary.skipped, 1);

        let resp = app
            .oneshot(get_auth("/api/grocery", &cookie))
            .await
            .unwrap();
        assert_eq!(json_body(resp).await.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn grocery_mutations_write_audit_rows() {
        let (app, db) = grocery_app().await;
        let cookie = setup_admin(&app).await;

        // create
        let resp = app
            .clone()
            .oneshot(post_auth(
                "/api/grocery",
                json!({ "name": "Maito", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        // edit (update)
        app.clone()
            .oneshot(put_auth(
                &format!("/api/grocery/{id}"),
                json!({ "name": "Kevytmaito", "qty": null, "unit": null }),
                &cookie,
            ))
            .await
            .unwrap();

        // delete
        app.clone()
            .oneshot(delete_auth(&format!("/api/grocery/{id}"), &cookie))
            .await
            .unwrap();

        let rows = sqlx::query!(
            r#"SELECT entity AS "entity!", op AS "op!", label AS "label!",
                      entity_id AS "entity_id?: i64"
               FROM audit_log ORDER BY id"#
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
            ("grocery_item", "create", "Maito")
        );
        assert_eq!(
            (rows[1].op.as_str(), rows[1].label.as_str()),
            ("update", "Kevytmaito")
        );
        assert_eq!(
            (
                rows[2].op.as_str(),
                rows[2].label.as_str(),
                rows[2].entity_id
            ),
            ("delete", "Kevytmaito", Some(id))
        );
    }

    #[tokio::test]
    async fn clear_checked_writes_one_bulk_audit_row() {
        let (app, db) = grocery_app().await;
        let cookie = setup_admin(&app).await;

        for name in ["Maito", "Leipä", "Voi"] {
            let resp = app
                .clone()
                .oneshot(post_auth(
                    "/api/grocery",
                    json!({ "name": name, "qty": null, "unit": null }),
                    &cookie,
                ))
                .await
                .unwrap();
            let id = json_body(resp).await["id"].as_i64().unwrap();
            app.clone()
                .oneshot(put_auth(
                    &format!("/api/grocery/{id}/check"),
                    json!({ "checked": true }),
                    &cookie,
                ))
                .await
                .unwrap();
        }

        app.clone()
            .oneshot(post_auth("/api/grocery/clear-checked", json!({}), &cookie))
            .await
            .unwrap();

        let bulk = sqlx::query!(
            r#"SELECT label AS "label!", entity_id AS "entity_id?: i64"
               FROM audit_log WHERE op = 'delete'"#
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(bulk.len(), 1); // ONE row, not three
        assert_eq!(bulk[0].entity_id, None);
        assert_eq!(bulk[0].label, "3 ostosta");
    }
}

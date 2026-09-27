use crate::AppState;
use crate::error::ApiError;
use crate::session::CurrentUser;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use perkele_shared::recipe::{
    ImportSummary, MealPlanEntry, MealPlanWeek, Recipe, RecipeIngredient, RecipeSummary,
    SaveRecipeRequest, SetMealRequest, prepare_for_grocery, validate_meal, validate_recipe,
};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/recipes", get(list_recipes).post(create_recipe))
        .route(
            "/api/recipes/{id}",
            get(get_recipe).put(update_recipe).delete(delete_recipe),
        )
        .route("/api/recipes/{id}/grocery", post(add_recipe_to_grocery))
        .route("/api/mealplan", get(get_week))
        .route("/api/mealplan/range", get(get_range))
        .route("/api/mealplan/grocery", post(add_week_to_grocery))
        .route("/api/mealplan/{date}", put(set_meal).delete(clear_meal))
}

/// Validate the request with the shared rule set (title, ingredient names as
/// grocery items, and field length/count limits). Returns a BadRequest on the
/// first failure. `pub(crate)` so the startup seeder (seed.rs) can hold its
/// data to the same rules the API enforces.
pub(crate) fn validate_save(req: &SaveRecipeRequest) -> Result<(), ApiError> {
    validate_recipe(req).map_err(|m| ApiError::BadRequest(m.to_owned()))
}

async fn list_recipes(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<Vec<RecipeSummary>>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT
               id       AS "id!: i64",
               title    AS "title!: String",
               servings AS "servings?: i64",
               prep_min AS "prep_min?: i64",
               cook_min AS "cook_min?: i64"
           FROM recipes
           WHERE family_id = ? AND deleted_at IS NULL
           ORDER BY title COLLATE NOCASE ASC"#,
        user.family_id,
    )
    .fetch_all(&state.db)
    .await?;

    let recipes = rows
        .into_iter()
        .map(|r| RecipeSummary {
            id: r.id,
            title: r.title,
            servings: r.servings,
            prep_min: r.prep_min,
            cook_min: r.cook_min,
        })
        .collect();
    Ok(Json(recipes))
}

/// Load one recipe + its ordered ingredients, scoped to the caller's family.
async fn get_recipe(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Recipe>, ApiError> {
    let recipe = load_recipe(&state.db, user.family_id, id).await?;
    Ok(Json(recipe))
}

/// Shared loader used by get/create/update so they all return the same shape.
async fn load_recipe(db: &crate::db::Db, family_id: i64, id: i64) -> Result<Recipe, ApiError> {
    let r = sqlx::query!(
        r#"SELECT
               id           AS "id!: i64",
               family_id    AS "family_id!: i64",
               title        AS "title!: String",
               instructions AS "instructions?: String",
               servings     AS "servings?: i64",
               prep_min     AS "prep_min?: i64",
               cook_min     AS "cook_min?: i64",
               source       AS "source?: String",
               created_by   AS "created_by!: i64",
               created_at   AS "created_at!: String",
               updated_at   AS "updated_at!: String"
           FROM recipes
           WHERE id = ? AND family_id = ? AND deleted_at IS NULL"#,
        id,
        family_id,
    )
    .fetch_optional(db)
    .await?;

    let Some(r) = r else {
        return Err(ApiError::NotFound);
    };

    let ingredients = load_ingredients(db, id).await?;
    Ok(Recipe {
        id: r.id,
        family_id: r.family_id,
        title: r.title,
        instructions: r.instructions,
        servings: r.servings,
        prep_min: r.prep_min,
        cook_min: r.cook_min,
        source: r.source,
        created_by: r.created_by,
        created_at: r.created_at,
        updated_at: r.updated_at,
        ingredients,
    })
}

async fn load_ingredients(
    db: &crate::db::Db,
    recipe_id: i64,
) -> Result<Vec<RecipeIngredient>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT
               name     AS "name!: String",
               qty      AS "qty?: String",
               unit     AS "unit?: String",
               category AS "category?: String"
           FROM recipe_ingredients
           WHERE recipe_id = ?
           ORDER BY position ASC"#,
        recipe_id,
    )
    .fetch_all(db)
    .await?;
    Ok(rows
        .into_iter()
        .map(|r| RecipeIngredient {
            name: r.name,
            qty: r.qty,
            unit: r.unit,
            category: r.category,
        })
        .collect())
}

/// Insert the recipe header then its ingredient rows, in one transaction.
async fn create_recipe(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<SaveRecipeRequest>,
) -> Result<(StatusCode, Json<Recipe>), ApiError> {
    validate_save(&req)?;

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)?;

    let mut tx = state.db.begin().await?;
    let id = sqlx::query!(
        "INSERT INTO recipes
            (family_id, title, instructions, servings, prep_min, cook_min, source,
             created_by, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        user.family_id,
        req.title,
        req.instructions,
        req.servings,
        req.prep_min,
        req.cook_min,
        req.source,
        user.user_id,
        now,
        now,
    )
    .execute(&mut *tx)
    .await?
    .last_insert_rowid();

    insert_ingredients(&mut tx, user.family_id, id, &req.ingredients).await?;
    tx.commit().await?;

    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "recipe",
        Some(id),
        "create",
        &req.title,
    )
    .await?;

    let recipe = load_recipe(&state.db, user.family_id, id).await?;
    Ok((StatusCode::CREATED, Json(recipe)))
}

/// Replace-all ingredient update: bump updated_at, delete old rows, re-insert.
async fn update_recipe(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<SaveRecipeRequest>,
) -> Result<Json<Recipe>, ApiError> {
    validate_save(&req)?;
    // Replace-all is a destructive overwrite → author-or-admin.
    require_recipe_owner(&state.db, &user, id).await?;

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)?;

    let mut tx = state.db.begin().await?;
    let affected = sqlx::query!(
        "UPDATE recipes
         SET title = ?, instructions = ?, servings = ?, prep_min = ?,
             cook_min = ?, source = ?, updated_at = ?
         WHERE id = ? AND family_id = ? AND deleted_at IS NULL",
        req.title,
        req.instructions,
        req.servings,
        req.prep_min,
        req.cook_min,
        req.source,
        now,
        id,
        user.family_id,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if affected == 0 {
        return Err(ApiError::NotFound);
    }

    sqlx::query!("DELETE FROM recipe_ingredients WHERE recipe_id = ?", id)
        .execute(&mut *tx)
        .await?;
    insert_ingredients(&mut tx, user.family_id, id, &req.ingredients).await?;
    tx.commit().await?;

    // NOTE: deviates from the brief's literal placement (right after the
    // rows_affected 404 guard, still inside `tx`): `state.db` is a *separate*
    // pool connection from `tx`, and the test pool caps at 1 connection
    // (see db::test_pool), so calling audit::record before tx.commit() would
    // try to check out a second connection while the first is still held by
    // the open transaction — a self-deadlock that surfaced as a 500 in
    // update_replaces_ingredients. Mirrors create_recipe: record after commit.
    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "recipe",
        Some(id),
        "update",
        &req.title,
    )
    .await?;

    let recipe = load_recipe(&state.db, user.family_id, id).await?;
    Ok(Json(recipe))
}

/// Insert ingredient rows with stable 0-based positions. `tx` keeps header +
/// ingredients atomic so a partial recipe can never be observed.
/// `pub(crate)` so the startup seeder (seed.rs) inserts through the same path
/// as the API handlers.
pub(crate) async fn insert_ingredients(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    family_id: i64,
    recipe_id: i64,
    ingredients: &[RecipeIngredient],
) -> Result<(), ApiError> {
    for (i, ing) in ingredients.iter().enumerate() {
        let position = i as i64;
        sqlx::query!(
            "INSERT INTO recipe_ingredients
                (recipe_id, family_id, name, qty, unit, category, position)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            recipe_id,
            family_id,
            ing.name,
            ing.qty,
            ing.unit,
            ing.category,
            position,
        )
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Look up a live recipe in the caller's family and enforce author-or-admin.
/// 404 if it doesn't exist (or belongs to another family), 403 if the caller
/// neither wrote it nor is an admin. Returns the title for audit labels.
async fn require_recipe_owner(
    db: &crate::db::Db,
    user: &CurrentUser,
    id: i64,
) -> Result<String, ApiError> {
    let row = sqlx::query!(
        r#"SELECT title AS "title!: String", created_by AS "created_by!: i64"
           FROM recipes
           WHERE id = ? AND family_id = ? AND deleted_at IS NULL"#,
        id,
        user.family_id,
    )
    .fetch_optional(db)
    .await?
    .ok_or(ApiError::NotFound)?;
    if !crate::grocery::can_modify(user, row.created_by) {
        return Err(ApiError::Forbidden);
    }
    Ok(row.title)
}

async fn delete_recipe(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<StatusCode, ApiError> {
    // Capture the title before the soft-delete so the audit label stays
    // readable even after the row is gone from normal reads (mirrors
    // grocery::delete_item's pattern).
    // Also the author-or-admin gate (404 unknown, 403 not allowed).
    let title = require_recipe_owner(&state.db, &user, id).await?;

    let now = OffsetDateTime::now_utc();
    let affected = sqlx::query!(
        "UPDATE recipes SET deleted_at = ?
         WHERE id = ? AND family_id = ? AND deleted_at IS NULL",
        now,
        id,
        user.family_id,
    )
    .execute(&state.db)
    .await?
    .rows_affected();

    if affected == 0 {
        return Err(ApiError::NotFound);
    }

    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "recipe",
        Some(id),
        "delete",
        &title,
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

// --- meal planner + grocery glue -------------------------------------------

#[derive(Deserialize)]
struct WeekQuery {
    /// Monday of the requested week, 'YYYY-MM-DD'.
    week: String,
}

/// Return all 7 days from `week` (Monday) with whatever dinner is planned.
/// Days with no entry are still returned with all-None fields so the UI can
/// render an empty slot per day without guessing dates.
async fn get_week(
    user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<WeekQuery>,
) -> Result<Json<MealPlanWeek>, ApiError> {
    let dates = week_dates(&q.week)?;
    let first = &dates[0];
    let last = &dates[6];

    let rows = sqlx::query!(
        r#"SELECT
               m.date      AS "date!: String",
               m.recipe_id AS "recipe_id?: i64",
               m.free_text AS "free_text?: String",
               r.title     AS "recipe_title?: String"
           FROM meal_plan_entries m
           LEFT JOIN recipes r ON r.id = m.recipe_id AND r.deleted_at IS NULL
           WHERE m.family_id = ? AND m.date >= ? AND m.date <= ?"#,
        user.family_id,
        first,
        last,
    )
    .fetch_all(&state.db)
    .await?;

    let entries = dates
        .iter()
        .map(|d| {
            rows.iter()
                .find(|r| &r.date == d)
                .map(|r| MealPlanEntry {
                    date: r.date.clone(),
                    recipe_id: r.recipe_id,
                    recipe_title: r.recipe_title.clone(),
                    free_text: r.free_text.clone(),
                })
                .unwrap_or(MealPlanEntry {
                    date: d.clone(),
                    recipe_id: None,
                    recipe_title: None,
                    free_text: None,
                })
        })
        .collect();

    Ok(Json(MealPlanWeek {
        week_start: dates[0].clone(),
        entries,
    }))
}

#[derive(Deserialize)]
struct RangeQuery {
    /// Inclusive 'YYYY-MM-DD' bounds.
    from: String,
    to: String,
}

/// All planned dinners in [from, to] — only days that have one, date-ordered.
/// The calendar's read-only meal projection uses this: its windows (6-week
/// month grid, 8-week agenda) span more than the single week `get_week`
/// covers, and the fixed empty-slot padding would be dead weight there.
async fn get_range(
    user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<RangeQuery>,
) -> Result<Json<Vec<MealPlanEntry>>, ApiError> {
    validate_date(&q.from)?;
    validate_date(&q.to)?;
    // 'YYYY-MM-DD' strings order like the dates they name, so plain string
    // comparison is enough — both here and in the SQL BETWEEN below.
    if q.from > q.to {
        return Err(ApiError::BadRequest("Virheellinen aikaväli.".into()));
    }

    let rows = sqlx::query!(
        r#"SELECT
               m.date      AS "date!: String",
               m.recipe_id AS "recipe_id?: i64",
               m.free_text AS "free_text?: String",
               r.title     AS "recipe_title?: String"
           FROM meal_plan_entries m
           LEFT JOIN recipes r ON r.id = m.recipe_id AND r.deleted_at IS NULL
           WHERE m.family_id = ? AND m.date >= ? AND m.date <= ?
           ORDER BY m.date"#,
        user.family_id,
        q.from,
        q.to,
    )
    .fetch_all(&state.db)
    .await?;

    Ok(Json(
        rows.into_iter()
            .map(|r| MealPlanEntry {
                date: r.date,
                recipe_id: r.recipe_id,
                recipe_title: r.recipe_title,
                free_text: r.free_text,
            })
            .collect(),
    ))
}

/// Upsert one day's dinner (the UNIQUE(family_id, date) drives the conflict).
async fn set_meal(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(date): Path<String>,
    Json(req): Json<SetMealRequest>,
) -> Result<StatusCode, ApiError> {
    validate_date(&date)?;
    validate_meal(&req).map_err(|m| ApiError::BadRequest(m.to_owned()))?;

    // If a recipe_id is given, verify it belongs to this family.
    if let Some(rid) = req.recipe_id {
        let exists = sqlx::query_scalar!(
            r#"SELECT 1 AS "x!: i64" FROM recipes
               WHERE id = ? AND family_id = ? AND deleted_at IS NULL"#,
            rid,
            user.family_id,
        )
        .fetch_optional(&state.db)
        .await?;
        if exists.is_none() {
            return Err(ApiError::NotFound);
        }
    }

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)?;

    sqlx::query!(
        "INSERT INTO meal_plan_entries
            (family_id, date, recipe_id, free_text, created_by, created_at)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(family_id, date) DO UPDATE SET
            recipe_id = excluded.recipe_id,
            free_text = excluded.free_text",
        user.family_id,
        date,
        req.recipe_id,
        req.free_text,
        user.user_id,
        now,
    )
    .execute(&state.db)
    .await?;

    // Dinners are keyed by (family_id, date) with no per-row id the UI
    // shows, so we log against the date itself. This is always an "update"
    // op: the upsert has no cheap create/update distinction, and "changed
    // the dinner for that day" is the accountability question that matters.
    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "dinner",
        None,
        "update",
        &date,
    )
    .await?;

    Ok(StatusCode::NO_CONTENT)
}

/// Clear one day's dinner. Deliberately open to every family member, like
/// `set_meal` (which can overwrite the day anyway): the meal plan is shared
/// scheduling — "what are we eating on Tuesday" — not a piece of content
/// someone authored. Clearing a day destroys at most one line of free text or
/// a reference to a recipe (the recipe itself is untouched), and the audit log
/// records who did it. Gating this on `created_by` would also force gating
/// `set_meal`, and then nobody but the first planner could move a dinner.
async fn clear_meal(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(date): Path<String>,
) -> Result<StatusCode, ApiError> {
    let affected = sqlx::query!(
        "DELETE FROM meal_plan_entries WHERE family_id = ? AND date = ?",
        user.family_id,
        date,
    )
    .execute(&state.db)
    .await?
    .rows_affected();

    // Only log a real delete — clearing an already-empty day is a no-op
    // that shouldn't show up in the family's change history.
    if affected > 0 {
        crate::audit::record(
            &state.db,
            user.family_id,
            user.user_id,
            "dinner",
            None,
            "delete",
            &date,
        )
        .await?;
    }

    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct ServingsQuery {
    servings: Option<i64>,
}

/// The per-recipe grocery button: scale this recipe's ingredients to the
/// requested servings, round countables up, and dedup-insert into the list.
/// The factor uses the same base rule as the app's stepper
/// (servings.unwrap_or(4).max(1)) so the list gets exactly what the view
/// shows. No `servings` param means factor 1 (the recipe as written).
async fn add_recipe_to_grocery(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<ServingsQuery>,
) -> Result<Json<ImportSummary>, ApiError> {
    // load_recipe 404s for other families' and deleted recipes.
    let recipe = load_recipe(&state.db, user.family_id, id).await?;

    let factor = match q.servings {
        None => 1.0,
        Some(s) if s >= 1 => {
            let base = recipe.servings.unwrap_or(4).max(1);
            s as f64 / base as f64
        }
        Some(_) => return Err(ApiError::BadRequest("Virheellinen annosmäärä.".into())),
    };

    let prepared = prepare_for_grocery(recipe.ingredients, factor);
    let summary = crate::grocery::add_ingredients_dedup(
        &state.db,
        user.family_id,
        user.user_id,
        &prepared,
        Some(id),
    )
    .await?;

    // Bulk import → ONE "create" audit row with a count label (never N),
    // mirroring clear_checked's bulk pattern. `added` counts genuinely new
    // items; a revived checked staple (unchecked) or an already-present skip
    // isn't a new addition, so only log when something new landed.
    if summary.added > 0 {
        crate::audit::record(
            &state.db,
            user.family_id,
            user.user_id,
            "grocery_item",
            None,
            "create",
            &format!("{} ostosta", summary.added),
        )
        .await?;
    }

    crate::sync::poke(&state, user.family_id).await;
    Ok(Json(summary))
}

/// THE GLUE. Gather every ingredient from the recipes planned this week,
/// merge duplicates, round countable sums up, and dedup-insert into the
/// grocery list (skip items already there unchecked; revive checked staples).
/// Each mutation writes a sync_log row so other devices update over SSE.
async fn add_week_to_grocery(
    user: CurrentUser,
    State(state): State<AppState>,
    Query(q): Query<WeekQuery>,
) -> Result<Json<ImportSummary>, ApiError> {
    let dates = week_dates(&q.week)?;
    let first = &dates[0];
    let last = &dates[6];

    // All ingredients from recipes planned in this date range. The JOIN chain:
    // mealplan -> recipe (not deleted) -> its ingredients.
    let rows = sqlx::query!(
        r#"SELECT
               ri.name     AS "name!: String",
               ri.qty      AS "qty?: String",
               ri.unit     AS "unit?: String",
               ri.category AS "category?: String"
           FROM meal_plan_entries m
           JOIN recipes r ON r.id = m.recipe_id AND r.deleted_at IS NULL
           JOIN recipe_ingredients ri ON ri.recipe_id = r.id
           WHERE m.family_id = ? AND m.date >= ? AND m.date <= ?
           ORDER BY ri.position ASC"#,
        user.family_id,
        first,
        last,
    )
    .fetch_all(&state.db)
    .await?;

    let ingredients: Vec<RecipeIngredient> = rows
        .into_iter()
        .map(|r| RecipeIngredient {
            name: r.name,
            qty: r.qty,
            unit: r.unit,
            category: r.category,
        })
        .collect();

    // Factor 1.0: the week's quantities are used as written — but the
    // pipeline still merges shared ingredients across recipes and rounds
    // countable sums up, and the helper dedups against the live list.
    let prepared = prepare_for_grocery(ingredients, 1.0);
    let summary = crate::grocery::add_ingredients_dedup(
        &state.db,
        user.family_id,
        user.user_id,
        &prepared,
        None, // rows merge across recipes, so no single recipe_id applies
    )
    .await?;

    // Bulk import → ONE "create" audit row with a count label (never N),
    // mirroring clear_checked's bulk pattern. `added` counts genuinely new
    // items; a revived checked staple (unchecked) or an already-present skip
    // isn't a new addition, so only log when something new landed.
    if summary.added > 0 {
        crate::audit::record(
            &state.db,
            user.family_id,
            user.user_id,
            "grocery_item",
            None,
            "create",
            &format!("{} ostosta", summary.added),
        )
        .await?;
    }

    crate::sync::poke(&state, user.family_id).await;
    Ok(Json(summary))
}

/// Validate a 'YYYY-MM-DD' date string and return the 7 dates of its week,
/// Monday-first. Reuses `time` (already a server dep).
fn week_dates(monday: &str) -> Result<Vec<String>, ApiError> {
    use time::Date;
    use time::macros::format_description;
    // Shared check first: exact shape + the 1900..=2200 year bound.
    validate_date(monday)?;
    let fmt = format_description!("[year]-[month]-[day]");
    let start = Date::parse(monday, &fmt)
        .map_err(|_| ApiError::BadRequest("Virheellinen päivä.".into()))?;
    (0..7)
        .map(|i| {
            start
                .checked_add(time::Duration::days(i))
                .ok_or(ApiError::Internal)
                .and_then(|d| d.format(&fmt).map_err(|_| ApiError::Internal))
        })
        .collect()
}

/// Thin wrapper: the rule itself (exact 'YYYY-MM-DD', real day, year within
/// the shared bounds) lives in `perkele_shared::dates` next to the client.
fn validate_date(date: &str) -> Result<(), ApiError> {
    perkele_shared::dates::validate_date(date).map_err(|m| ApiError::BadRequest(m.to_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn recipe_app_with_db() -> (axum::Router, crate::db::Db) {
        let db = crate::db::test_pool().await;
        let state = AppState::for_test(db.clone());
        // Merge the grocery router too: the glue test reads back GET /api/grocery
        // to confirm the imported items landed there. The aisle router is
        // merged so the import test can confirm the aisle map got taught.
        let app = crate::routes::router()
            .merge(router())
            .merge(crate::grocery::router())
            .merge(crate::aisle::router())
            .with_state(state);
        (app, db)
    }

    async fn recipe_app() -> axum::Router {
        recipe_app_with_db().await.0
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

    fn get_auth(path: &str, cookie: &str) -> Request<Body> {
        Request::builder()
            .uri(path)
            .header(header::COOKIE, cookie)
            .body(Body::empty())
            .unwrap()
    }

    fn post_setup(path: &str, body: Value) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
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
            .oneshot(post_setup(
                "/api/setup",
                json!({ "family_name": "Virtanen", "username": "mikko",
                        "display_name": "Mikko", "password": "hunter2!" }),
            ))
            .await
            .unwrap();
        cookie_pair(&resp)
    }

    /// Admin mints an invite with `role` ("member"/"kid"); `username`
    /// redeems it. Returns the new user's session cookie.
    async fn add_user(app: &axum::Router, admin: &str, role: &str, username: &str) -> String {
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/family/invites",
                admin,
                Some(json!({ "role": role })),
            ))
            .await
            .unwrap();
        let code = json_body(resp).await["code"].as_str().unwrap().to_owned();
        let resp = app
            .clone()
            .oneshot(post_setup(
                "/api/auth/redeem",
                json!({ "code": code, "username": username,
                        "display_name": username, "password": "hunter2!" }),
            ))
            .await
            .unwrap();
        cookie_pair(&resp)
    }

    /// POST sample_recipe() as `cookie`; returns the new recipe id.
    async fn create_sample(app: &axum::Router, cookie: &str) -> i64 {
        let resp = app
            .clone()
            .oneshot(req("POST", "/api/recipes", cookie, Some(sample_recipe())))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        json_body(resp).await["id"].as_i64().unwrap()
    }

    /// Author-or-admin: a kid can neither overwrite nor delete the admin's
    /// recipe, and the recipe comes through untouched.
    #[tokio::test]
    async fn kid_cannot_update_or_delete_others_recipe() {
        let app = recipe_app().await;
        let admin = setup_admin(&app).await;
        let id = create_sample(&app, &admin).await;
        let kid = add_user(&app, &admin, "kid", "kalle").await;

        let mut edit = sample_recipe();
        edit["title"] = json!("Kallen versio");
        let resp = app
            .clone()
            .oneshot(put(&format!("/api/recipes/{id}"), &kid, edit))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        let resp = app
            .clone()
            .oneshot(req("DELETE", &format!("/api/recipes/{id}"), &kid, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        let resp = app
            .oneshot(get_auth(&format!("/api/recipes/{id}"), &admin))
            .await
            .unwrap();
        let r = json_body(resp).await;
        assert_eq!(r["title"], "Lihapullat");
        assert_eq!(r["ingredients"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn member_can_update_and_delete_own_recipe() {
        let app = recipe_app().await;
        let admin = setup_admin(&app).await;
        let member = add_user(&app, &admin, "member", "matti").await;
        let id = create_sample(&app, &member).await;

        let mut edit = sample_recipe();
        edit["title"] = json!("Matin pullat");
        let resp = app
            .clone()
            .oneshot(put(&format!("/api/recipes/{id}"), &member, edit))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .oneshot(req("DELETE", &format!("/api/recipes/{id}"), &member, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn admin_can_update_and_delete_members_recipe() {
        let app = recipe_app().await;
        let admin = setup_admin(&app).await;
        let member = add_user(&app, &admin, "member", "matti").await;
        let id = create_sample(&app, &member).await;

        let mut edit = sample_recipe();
        edit["title"] = json!("Korjattu");
        let resp = app
            .clone()
            .oneshot(put(&format!("/api/recipes/{id}"), &admin, edit))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .oneshot(req("DELETE", &format!("/api/recipes/{id}"), &admin, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    /// The meal plan is shared scheduling, not authored content: anyone may
    /// clear or replace a day someone else planned (see `clear_meal`).
    #[tokio::test]
    async fn kid_can_change_meal_plan_set_by_admin() {
        let app = recipe_app().await;
        let admin = setup_admin(&app).await;
        let kid = add_user(&app, &admin, "kid", "kalle").await;
        let resp = app
            .clone()
            .oneshot(put(
                "/api/mealplan/2026-07-06",
                &admin,
                json!({"recipe_id": null, "free_text": "Kalakeitto"}),
            ))
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let resp = app
            .clone()
            .oneshot(put(
                "/api/mealplan/2026-07-06",
                &kid,
                json!({"recipe_id": null, "free_text": "Pizzaa"}),
            ))
            .await
            .unwrap();
        assert!(resp.status().is_success());
        let resp = app
            .oneshot(req("DELETE", "/api/mealplan/2026-07-06", &kid, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    fn sample_recipe() -> Value {
        json!({
            "title": "Lihapullat",
            "instructions": "Sekoita ja paista.",
            "servings": 4,
            "prep_min": 15,
            "cook_min": 30,
            "source": null,
            "ingredients": [
                { "name": "jauheliha", "qty": "400", "unit": "g", "category": "liha" },
                { "name": "kerma", "qty": "2", "unit": "dl", "category": "maito" }
            ]
        })
    }

    #[tokio::test]
    async fn recipes_require_auth() {
        let app = recipe_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/recipes")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn create_then_get_roundtrips_ingredients() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(req("POST", "/api/recipes", &cookie, Some(sample_recipe())))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = json_body(resp).await;
        let id = created["id"].as_i64().unwrap();
        assert_eq!(created["ingredients"].as_array().unwrap().len(), 2);

        let resp = app
            .oneshot(req("GET", &format!("/api/recipes/{id}"), &cookie, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let got = json_body(resp).await;
        assert_eq!(got["title"], "Lihapullat");
        assert_eq!(got["ingredients"][0]["name"], "jauheliha");
        assert_eq!(got["ingredients"][1]["unit"], "dl");
    }

    #[tokio::test]
    async fn list_excludes_deleted() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;
        let resp = app
            .clone()
            .oneshot(req("POST", "/api/recipes", &cookie, Some(sample_recipe())))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let resp = app
            .clone()
            .oneshot(req("DELETE", &format!("/api/recipes/{id}"), &cookie, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let resp = app
            .oneshot(req("GET", "/api/recipes", &cookie, None))
            .await
            .unwrap();
        let list = json_body(resp).await;
        assert_eq!(list.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn update_replaces_ingredients() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;
        let resp = app
            .clone()
            .oneshot(req("POST", "/api/recipes", &cookie, Some(sample_recipe())))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let updated = json!({
            "title": "Lihapullat v2", "instructions": null, "servings": 2,
            "prep_min": null, "cook_min": null, "source": null,
            "ingredients": [ { "name": "riisi", "qty": "2", "unit": "dl", "category": null } ]
        });
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/recipes/{id}"),
                &cookie,
                Some(updated),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["title"], "Lihapullat v2");
        assert_eq!(body["ingredients"].as_array().unwrap().len(), 1);
        assert_eq!(body["ingredients"][0]["name"], "riisi");
    }

    #[tokio::test]
    async fn empty_title_is_rejected() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;
        let bad = json!({ "title": "  ", "instructions": null, "servings": null,
                          "prep_min": null, "cook_min": null, "source": null, "ingredients": [] });
        let resp = app
            .oneshot(req("POST", "/api/recipes", &cookie, Some(bad)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn overlong_recipe_fields_rejected_on_create_and_update() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;
        let resp = app
            .clone()
            .oneshot(req("POST", "/api/recipes", &cookie, Some(sample_recipe())))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let mut bads = Vec::new();
        let mut b = sample_recipe();
        b["instructions"] = json!("x".repeat(20_001));
        bads.push(b);
        let mut b = sample_recipe();
        b["source"] = json!("x".repeat(501));
        bads.push(b);
        let mut b = sample_recipe();
        b["ingredients"][0]["qty"] = json!("9".repeat(33));
        bads.push(b);
        let mut b = sample_recipe();
        b["ingredients"][0]["unit"] = json!("x".repeat(33));
        bads.push(b);
        let mut b = sample_recipe();
        b["ingredients"][0]["category"] = json!("x".repeat(65));
        bads.push(b);
        let mut b = sample_recipe();
        let one = b["ingredients"][0].clone();
        b["ingredients"] = json!(vec![one; 201]);
        bads.push(b);

        for bad in bads {
            let resp = app
                .clone()
                .oneshot(req("POST", "/api/recipes", &cookie, Some(bad.clone())))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
            let resp = app
                .clone()
                .oneshot(req(
                    "PUT",
                    &format!("/api/recipes/{id}"),
                    &cookie,
                    Some(bad),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }
    }

    /// Every seeded default recipe passes the shared validator that the API
    /// (and the client) apply — the new length limits must not reject them.
    #[test]
    fn seeded_recipes_pass_shared_recipe_validation() {
        for r in crate::seed::default_recipes() {
            assert_eq!(
                perkele_shared::recipe::validate_recipe(&r),
                Ok(()),
                "seed recipe {:?}",
                r.title
            );
        }
    }

    #[tokio::test]
    async fn meal_plan_rejects_bad_dates_and_long_free_text() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;
        let body = json!({"recipe_id": null, "free_text": "Pizzaa"});
        for date in ["0001-01-01", "9999-12-31", "2026-02-30"] {
            let resp = app
                .clone()
                .oneshot(put(&format!("/api/mealplan/{date}"), &cookie, body.clone()))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{date}");
        }
        let resp = app
            .clone()
            .oneshot(put(
                "/api/mealplan/2026-07-06",
                &cookie,
                json!({"recipe_id": null, "free_text": "x".repeat(201)}),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        for q in [
            "/api/mealplan?week=0001-01-01",
            "/api/mealplan/range?from=0001-01-01&to=2026-07-06",
            "/api/mealplan/range?from=2026-07-06&to=9999-01-01",
        ] {
            let resp = app
                .clone()
                .oneshot(req("GET", q, &cookie, None))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{q}");
        }
    }

    fn put(path: &str, cookie: &str, body: Value) -> Request<Body> {
        req("PUT", path, cookie, Some(body))
    }

    #[tokio::test]
    async fn add_week_dedups_into_grocery() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;

        // Two recipes that share an ingredient (kerma, dl).
        let r1 = json!({ "title": "A", "instructions": null, "servings": null,
            "prep_min": null, "cook_min": null, "source": null,
            "ingredients": [ { "name": "kerma", "qty": "2", "unit": "dl", "category": "maito" } ] });
        let r2 = json!({ "title": "B", "instructions": null, "servings": null,
            "prep_min": null, "cook_min": null, "source": null,
            "ingredients": [ { "name": "kerma", "qty": "5", "unit": "dl", "category": "maito" },
                             { "name": "riisi", "qty": "3", "unit": "dl", "category": null } ] });

        let id1 = json_body(
            app.clone()
                .oneshot(req("POST", "/api/recipes", &cookie, Some(r1)))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let id2 = json_body(
            app.clone()
                .oneshot(req("POST", "/api/recipes", &cookie, Some(r2)))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();

        // Plan them on Mon + Tue of a known week.
        app.clone()
            .oneshot(put(
                "/api/mealplan/2026-07-06",
                &cookie,
                json!({"recipe_id": id1, "free_text": null}),
            ))
            .await
            .unwrap();
        app.clone()
            .oneshot(put(
                "/api/mealplan/2026-07-07",
                &cookie,
                json!({"recipe_id": id2, "free_text": null}),
            ))
            .await
            .unwrap();

        // Import the week (Monday 2026-07-06).
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/mealplan/grocery?week=2026-07-06",
                &cookie,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["added"], 2); // kerma (merged) + riisi
        assert_eq!(body["skipped"], 0);
        assert_eq!(body["unchecked"], 0);

        // Grocery list should hold the merged kerma 7 dl + riisi.
        let list = json_body(
            app.oneshot(req("GET", "/api/grocery", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let kerma = arr.iter().find(|i| i["name"] == "kerma").unwrap();
        assert_eq!(kerma["qty"], "7");
        assert_eq!(kerma["unit"], "dl");
    }

    /// Re-importing the same week must not duplicate anything: every
    /// ingredient is already on the list unchecked, so it all skips.
    #[tokio::test]
    async fn add_week_twice_skips_everything_second_time() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;

        let r = json!({ "title": "A", "instructions": null, "servings": null,
            "prep_min": null, "cook_min": null, "source": null,
            "ingredients": [ { "name": "kerma", "qty": "2", "unit": "dl", "category": "maito" } ] });
        let id = json_body(
            app.clone()
                .oneshot(req("POST", "/api/recipes", &cookie, Some(r)))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        app.clone()
            .oneshot(put(
                "/api/mealplan/2026-07-06",
                &cookie,
                json!({"recipe_id": id, "free_text": null}),
            ))
            .await
            .unwrap();

        for (added, skipped) in [(1, 0), (0, 1)] {
            let resp = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/api/mealplan/grocery?week=2026-07-06",
                    &cookie,
                    None,
                ))
                .await
                .unwrap();
            let body = json_body(resp).await;
            assert_eq!(body["added"], added);
            assert_eq!(body["skipped"], skipped);
        }

        let list = json_body(
            app.oneshot(req("GET", "/api/grocery", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(list.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn week_returns_seven_days() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;
        let resp = app
            .oneshot(req("GET", "/api/mealplan?week=2026-07-06", &cookie, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["entries"].as_array().unwrap().len(), 7);
        assert_eq!(body["entries"][0]["date"], "2026-07-06");
        assert_eq!(body["entries"][6]["date"], "2026-07-12");
    }

    #[tokio::test]
    async fn range_returns_only_planned_days() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;

        // Dinners on a Tuesday and the Monday of the following week — a span
        // one /api/mealplan?week= call can't cover.
        app.clone()
            .oneshot(put(
                "/api/mealplan/2026-06-30",
                &cookie,
                json!({"recipe_id": null, "free_text": "Pizzaa"}),
            ))
            .await
            .unwrap();
        app.clone()
            .oneshot(put(
                "/api/mealplan/2026-07-06",
                &cookie,
                json!({"recipe_id": null, "free_text": "Keittoa"}),
            ))
            .await
            .unwrap();

        // A range spanning both weeks returns both — and nothing for the
        // empty days in between (unlike the week endpoint's fixed 7 slots).
        let body = json_body(
            app.clone()
                .oneshot(req(
                    "GET",
                    "/api/mealplan/range?from=2026-06-29&to=2026-07-12",
                    &cookie,
                    None,
                ))
                .await
                .unwrap(),
        )
        .await;
        let arr = body.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["date"], "2026-06-30");
        assert_eq!(arr[0]["free_text"], "Pizzaa");
        assert_eq!(arr[1]["date"], "2026-07-06");

        // A narrower range excludes the later dinner.
        let body = json_body(
            app.clone()
                .oneshot(req(
                    "GET",
                    "/api/mealplan/range?from=2026-06-29&to=2026-07-05",
                    &cookie,
                    None,
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body.as_array().unwrap().len(), 1);

        // Malformed dates and inverted ranges are rejected.
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                "/api/mealplan/range?from=nonsense&to=2026-07-05",
                &cookie,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let resp = app
            .oneshot(req(
                "GET",
                "/api/mealplan/range?from=2026-07-05&to=2026-06-29",
                &cookie,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn set_meal_upserts_and_clear_removes() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;

        // Free-text dinner, then overwrite it, then clear.
        app.clone()
            .oneshot(put(
                "/api/mealplan/2026-07-06",
                &cookie,
                json!({"recipe_id": null, "free_text": "Pizzaa"}),
            ))
            .await
            .unwrap();
        let body = json_body(
            app.clone()
                .oneshot(req("GET", "/api/mealplan?week=2026-07-06", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["entries"][0]["free_text"], "Pizzaa");

        app.clone()
            .oneshot(req("DELETE", "/api/mealplan/2026-07-06", &cookie, None))
            .await
            .unwrap();
        let body = json_body(
            app.oneshot(req("GET", "/api/mealplan?week=2026-07-06", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["entries"][0]["free_text"], Value::Null);
    }

    #[tokio::test]
    async fn bogus_id_is_not_found() {
        let app = recipe_app().await;
        let admin = setup_admin(&app).await;
        let resp = app
            .clone()
            .oneshot(req("POST", "/api/recipes", &admin, Some(sample_recipe())))
            .await
            .unwrap();
        let id = json_body(resp).await["id"].as_i64().unwrap();

        let resp = app
            .oneshot(req(
                "GET",
                &format!("/api/recipes/{}", id + 999),
                &admin,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Seed helper for the button tests: one recipe for 4, with a countable
    /// and a measurable ingredient. Returns its id.
    async fn seed_scaling_recipe(app: &axum::Router, cookie: &str) -> i64 {
        let r = json!({ "title": "Kastike", "instructions": null, "servings": 4,
            "prep_min": null, "cook_min": null, "source": null,
            "ingredients": [
                { "name": "sipuli", "qty": "1", "unit": null, "category": null },
                { "name": "kerma", "qty": "2", "unit": "dl", "category": "maito" }
            ] });
        json_body(
            app.clone()
                .oneshot(req("POST", "/api/recipes", cookie, Some(r)))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap()
    }

    #[tokio::test]
    async fn recipe_to_grocery_scales_and_rounds_countables() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;
        let id = seed_scaling_recipe(&app, &cookie).await;

        // 4 → 6 servings: factor 1.5. sipuli 1 → 1.5 → ceil 2 (countable);
        // kerma 2 dl → 3 dl exact (measurable).
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/recipes/{id}/grocery?servings=6"),
                &cookie,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["added"], 2);
        assert_eq!(body["skipped"], 0);
        assert_eq!(body["unchecked"], 0);

        let list = json_body(
            app.clone()
                .oneshot(req("GET", "/api/grocery", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        let arr = list.as_array().unwrap();
        let sipuli = arr.iter().find(|i| i["name"] == "sipuli").unwrap();
        assert_eq!(sipuli["qty"], "2");
        assert_eq!(sipuli["recipe_id"], id);
        let kerma = arr.iter().find(|i| i["name"] == "kerma").unwrap();
        assert_eq!(kerma["qty"], "3");
        assert_eq!(kerma["unit"], "dl");
        // Ingredient categories no longer become store sections; they feed
        // the aisle map instead.
        assert_eq!(kerma["category"], Value::Null);

        let resp = app.oneshot(get_auth("/api/aisles", &cookie)).await.unwrap();
        let body = json_body(resp).await;
        let aisle_names: Vec<&str> = body["aisles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["name"].as_str().unwrap())
            .collect();
        assert!(aisle_names.contains(&"maito"));
        assert!(
            body["map"]
                .as_array()
                .unwrap()
                .iter()
                .any(|m| m["item_name"] == "kerma")
        );
    }

    #[tokio::test]
    async fn recipe_to_grocery_skips_existing_and_revives_staple() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;
        let id = seed_scaling_recipe(&app, &cookie).await;

        // "Sipuli" already on the list unchecked; "KERMA" is a checked staple.
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/grocery",
                &cookie,
                Some(json!({ "name": "Sipuli", "qty": null, "unit": null })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/grocery",
                &cookie,
                Some(json!({ "name": "KERMA", "qty": null, "unit": null })),
            ))
            .await
            .unwrap();
        let kerma_id = json_body(resp).await["id"].as_i64().unwrap();
        app.clone()
            .oneshot(put(
                &format!("/api/grocery/{kerma_id}/check"),
                &cookie,
                json!({ "checked": true }),
            ))
            .await
            .unwrap();

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/recipes/{id}/grocery?servings=4"),
                &cookie,
                None,
            ))
            .await
            .unwrap();
        let body = json_body(resp).await;
        assert_eq!(body["added"], 0);
        assert_eq!(body["skipped"], 1);
        assert_eq!(body["unchecked"], 1);

        // Still exactly two rows; the staple is now unchecked.
        let list = json_body(
            app.oneshot(req("GET", "/api/grocery", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        let arr = list.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        let kerma = arr.iter().find(|i| i["name"] == "KERMA").unwrap();
        assert_eq!(kerma["checked"], false);
    }

    #[tokio::test]
    async fn recipe_to_grocery_defaults_base_when_servings_omitted() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;
        let id = seed_scaling_recipe(&app, &cookie).await;

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/recipes/{id}/grocery"),
                &cookie,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // Factor 1: quantities land unscaled.
        let list = json_body(
            app.oneshot(req("GET", "/api/grocery", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        let sipuli = list
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["name"] == "sipuli")
            .unwrap()
            .clone();
        assert_eq!(sipuli["qty"], "1");
    }

    #[tokio::test]
    async fn recipe_to_grocery_rejects_bad_servings_and_unknown_recipe() {
        let app = recipe_app().await;
        let cookie = setup_admin(&app).await;
        let id = seed_scaling_recipe(&app, &cookie).await;

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/recipes/{id}/grocery?servings=0"),
                &cookie,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // load_recipe scopes by family and deleted_at, so an unknown id is a
        // clean 404 (other families' recipes look identical to missing ones).
        let resp = app
            .clone()
            .oneshot(req("POST", "/api/recipes/9999/grocery", &cookie, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn recipe_mutations_write_audit_rows() {
        let (app, db) = recipe_app_with_db().await;
        let admin = setup_admin(&app).await;

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/recipes",
                &admin,
                Some(json!({
                    "title": "Lohikeitto", "instructions": "", "servings": 4,
                    "prep_min": null, "cook_min": null, "source": null, "ingredients": []
                })),
            ))
            .await
            .unwrap();
        let created = json_body(resp).await;
        let id = created["id"].as_i64().unwrap();

        app.clone()
            .oneshot(req("DELETE", &format!("/api/recipes/{id}"), &admin, None))
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
            ("recipe", "create", "Lohikeitto")
        );
        assert_eq!(
            (rows[1].op.as_str(), rows[1].label.as_str()),
            ("delete", "Lohikeitto")
        );
    }

    #[tokio::test]
    async fn dinner_set_and_clear_write_audit_rows() {
        let (app, db) = recipe_app_with_db().await;
        let admin = setup_admin(&app).await;

        app.clone()
            .oneshot(put(
                "/api/mealplan/2026-07-24",
                &admin,
                json!({"recipe_id": null, "free_text": "Pizza"}),
            ))
            .await
            .unwrap();
        app.clone()
            .oneshot(req("DELETE", "/api/mealplan/2026-07-24", &admin, None))
            .await
            .unwrap();

        let rows = sqlx::query!(
            r#"SELECT entity AS "entity!", op AS "op!", label AS "label!",
                      entity_id AS "entity_id?: i64" FROM audit_log ORDER BY id"#
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(
            (
                rows[0].entity.as_str(),
                rows[0].op.as_str(),
                rows[0].label.as_str(),
                rows[0].entity_id
            ),
            ("dinner", "update", "2026-07-24", None)
        );
        assert_eq!(
            (rows[1].op.as_str(), rows[1].label.as_str()),
            ("delete", "2026-07-24")
        );
    }

    #[tokio::test]
    async fn clear_meal_on_empty_day_writes_no_audit_row() {
        // Deleting a day with no dinner entry is a no-op DELETE (0 rows
        // affected) and must not pollute the audit log.
        let (app, db) = recipe_app_with_db().await;
        let admin = setup_admin(&app).await;

        app.oneshot(req("DELETE", "/api/mealplan/2026-07-25", &admin, None))
            .await
            .unwrap();

        let count = sqlx::query_scalar!(r#"SELECT COUNT(*) AS "n!: i64" FROM audit_log"#)
            .fetch_one(&db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn recipe_import_writes_one_bulk_audit_row_and_repeat_writes_none() {
        // Coverage gap closed: the per-recipe "add to grocery" button creates
        // items but previously wrote no audit row at all.
        let (app, db) = recipe_app_with_db().await;
        let cookie = setup_admin(&app).await;
        let id = seed_scaling_recipe(&app, &cookie).await;

        // First import: both ingredients are new -> added == 2, one bulk row.
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/recipes/{id}/grocery"),
                &cookie,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["added"], 2);

        // Seeding the recipe itself already wrote a "recipe"/"create" audit
        // row (see recipe_mutations_write_audit_rows), so filter this
        // reused (already-cached) query down to "grocery_item" rows in Rust
        // rather than adding a new query! text to the WHERE clause.
        let rows = sqlx::query!(
            r#"SELECT entity AS "entity!", op AS "op!", label AS "label!",
                      entity_id AS "entity_id?: i64" FROM audit_log ORDER BY id"#
        )
        .fetch_all(&db)
        .await
        .unwrap();
        let grocery_rows: Vec<_> = rows.iter().filter(|r| r.entity == "grocery_item").collect();
        assert_eq!(grocery_rows.len(), 1);
        assert_eq!(grocery_rows[0].entity, "grocery_item");
        assert_eq!(grocery_rows[0].op, "create");
        assert_eq!(grocery_rows[0].entity_id, None);
        assert_eq!(grocery_rows[0].label, "2 ostosta");

        // Second import of the same recipe: everything is already on the
        // list (added == 0), so no new audit row should appear.
        let resp = app
            .oneshot(req(
                "POST",
                &format!("/api/recipes/{id}/grocery"),
                &cookie,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["added"], 0);

        let rows = sqlx::query!(
            r#"SELECT entity AS "entity!", op AS "op!", label AS "label!",
                      entity_id AS "entity_id?: i64" FROM audit_log ORDER BY id"#
        )
        .fetch_all(&db)
        .await
        .unwrap();
        let grocery_count = rows.iter().filter(|r| r.entity == "grocery_item").count();
        assert_eq!(grocery_count, 1);
    }
}

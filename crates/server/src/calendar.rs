use crate::AppState;
use crate::error::ApiError;
use crate::grocery::append_sync_log_entity;
use crate::session::CurrentUser;
use crate::sync::poke;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use perkele_shared::calendar::{
    CalendarSync, EditScope, Event, Exdate, SaveEventRequest, validate_event,
    validate_occurrence_start,
};
use serde::Deserialize;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tokio_stream::wrappers::BroadcastStream;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/calendar/sync", get(sync))
        .route("/api/calendar/events", post(create_event))
        .route(
            "/api/calendar/events/{uid}",
            axum::routing::put(update_event).delete(delete_event),
        )
        .route("/api/calendar/stream", get(stream))
}

/// All request rules live in `perkele_shared::calendar::validate_event`
/// (title, times, uid, field lengths, rrule, recurrence_id, reminder range)
/// so the client pre-checks exactly what the server enforces here.
fn validate(req: &SaveEventRequest) -> Result<(), ApiError> {
    validate_event(req).map_err(|m| ApiError::BadRequest(m.to_owned()))
}

async fn sync(
    user: CurrentUser,
    State(state): State<AppState>,
) -> Result<Json<CalendarSync>, ApiError> {
    let events = load_events(&state.db, user.family_id).await?;
    let exdates = load_exdates(&state.db, user.family_id).await?;
    let seq = sqlx::query_scalar!(
        r#"SELECT COALESCE(MAX(seq), 0) AS "seq!: i64" FROM sync_log WHERE family_id = ?"#,
        user.family_id,
    )
    .fetch_one(&state.db)
    .await?;
    Ok(Json(CalendarSync {
        events,
        exdates,
        seq,
    }))
}

/// Load all non-deleted events for a family, each with its attendee ids.
/// `pub(crate)`: the reminder scheduler scans through this too.
pub(crate) async fn load_events(
    db: &crate::db::Db,
    family_id: i64,
) -> Result<Vec<Event>, ApiError> {
    let rows = sqlx::query!(
        r#"SELECT
               id         AS "id!: i64",
               uid        AS "uid!: String",
               title      AS "title!: String",
               all_day    AS "all_day!: bool",
               starts_at  AS "starts_at!: String",
               ends_at    AS "ends_at!: String",
               location   AS "location?: String",
               notes      AS "notes?: String",
               rrule      AS "rrule?: String",
               series_uid AS "series_uid?: String",
               recurrence_id AS "recurrence_id?: String",
               reminder_minutes AS "reminder_minutes?: i64",
               created_by AS "created_by!: i64",
               created_at AS "created_at!: String",
               updated_at AS "updated_at!: String"
           FROM events
           WHERE family_id = ? AND deleted_at IS NULL
           ORDER BY starts_at ASC"#,
        family_id,
    )
    .fetch_all(db)
    .await?;

    let mut events = Vec::with_capacity(rows.len());
    for r in rows {
        let attendee_ids = sqlx::query_scalar!(
            r#"SELECT user_id AS "user_id!: i64" FROM event_attendees WHERE event_id = ?"#,
            r.id,
        )
        .fetch_all(db)
        .await?;
        events.push(Event {
            uid: r.uid,
            title: r.title,
            all_day: r.all_day,
            starts_at: r.starts_at,
            ends_at: r.ends_at,
            location: r.location,
            notes: r.notes,
            rrule: r.rrule,
            series_uid: r.series_uid,
            recurrence_id: r.recurrence_id,
            reminder_minutes: r.reminder_minutes,
            attendee_ids,
            created_by: r.created_by,
            created_at: r.created_at,
            updated_at: r.updated_at,
        });
    }
    Ok(events)
}

/// All exdates of a family (sync and the reminder scan share this).
pub(crate) async fn load_exdates(
    db: &crate::db::Db,
    family_id: i64,
) -> Result<Vec<Exdate>, ApiError> {
    Ok(sqlx::query!(
        r#"SELECT series_uid AS "series_uid!: String", occ_start AS "occ_start!: String"
           FROM event_exdates WHERE family_id = ?"#,
        family_id,
    )
    .fetch_all(db)
    .await?
    .into_iter()
    .map(|r| Exdate {
        series_uid: r.series_uid,
        occ_start: r.occ_start,
    })
    .collect())
}

/// Load one event by uid, family-scoped. None if missing/other-family/deleted.
async fn load_event(
    db: &crate::db::Db,
    family_id: i64,
    uid: &str,
) -> Result<Option<Event>, ApiError> {
    let all = load_events(db, family_id).await?;
    Ok(all.into_iter().find(|e| e.uid == uid))
}

/// Verify every attendee id belongs to the caller's family.
async fn check_attendees(db: &crate::db::Db, family_id: i64, ids: &[i64]) -> Result<(), ApiError> {
    for id in ids {
        let ok = sqlx::query_scalar!(
            r#"SELECT 1 AS "x!: i64" FROM users WHERE id = ? AND family_id = ?"#,
            id,
            family_id,
        )
        .fetch_optional(db)
        .await?;
        if ok.is_none() {
            return Err(ApiError::BadRequest("Tuntematon osallistuja.".into()));
        }
    }
    Ok(())
}

/// Called when an `ON CONFLICT(family_id, uid) DO NOTHING` insert inserted
/// nothing. A LIVE row with that uid is an idempotent replay (fine — the
/// caller returns it). A soft-deleted one still owns the uid through the
/// unique index, so the insert can never land: e.g. an offline create replayed
/// after someone deleted the event. That's a 409; returning `Err` drops `tx`
/// uncommitted, which rolls back everything the handler did before it.
async fn ensure_uid_live(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    family_id: i64,
    uid: &str,
) -> Result<(), ApiError> {
    // Same SQL text as update_event's lookup, so it reuses that `.sqlx` entry.
    let id = sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64" FROM events
           WHERE family_id = ? AND uid = ? AND deleted_at IS NULL"#,
        family_id,
        uid,
    )
    // `&mut **tx`: deref the `&mut Transaction` to the transaction, then
    // again to its connection — that's what implements sqlx's `Executor`.
    .fetch_optional(&mut **tx)
    .await?;
    if id.is_none() {
        return Err(ApiError::Conflict(
            "Tapahtuma on poistettu. Päivitä kalenteri ja yritä uudelleen.".into(),
        ));
    }
    Ok(())
}

/// Idempotent create: INSERT ... ON CONFLICT(family_id, uid) DO NOTHING, then
/// (re)load the row by uid. A replayed POST with the same uid is a safe no-op.
async fn create_event(
    user: CurrentUser,
    State(state): State<AppState>,
    Json(req): Json<SaveEventRequest>,
) -> Result<(StatusCode, Json<Event>), ApiError> {
    validate(&req)?;
    check_attendees(&state.db, user.family_id, &req.attendee_ids).await?;

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)?;

    let mut tx = state.db.begin().await?;
    let inserted = sqlx::query!(
        "INSERT INTO events
            (family_id, uid, title, all_day, starts_at, ends_at, location, notes,
             rrule, reminder_minutes, created_by, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(family_id, uid) DO NOTHING",
        user.family_id,
        req.uid,
        req.title,
        req.all_day,
        req.starts_at,
        req.ends_at,
        req.location,
        req.notes,
        req.rrule,
        req.reminder_minutes,
        user.user_id,
        now,
        now,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        ensure_uid_live(&mut tx, user.family_id, &req.uid).await?;
    }

    // Only attach attendees + log on a real insert (replay is a no-op).
    if inserted == 1 {
        let id = sqlx::query_scalar!(
            r#"SELECT id AS "id!: i64" FROM events WHERE family_id = ? AND uid = ?"#,
            user.family_id,
            req.uid,
        )
        .fetch_one(&mut *tx)
        .await?;
        for aid in &req.attendee_ids {
            sqlx::query!(
                "INSERT INTO event_attendees (event_id, user_id, family_id) VALUES (?, ?, ?)",
                id,
                aid,
                user.family_id,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        append_sync_log_entity(&state.db, user.family_id, id, "event", "insert").await?;
        crate::audit::record(
            &state.db,
            user.family_id,
            user.user_id,
            "calendar_event",
            Some(id),
            "create",
            &req.title,
        )
        .await?;
        poke(&state, user.family_id).await;
    } else {
        tx.commit().await?;
    }

    let event = load_event(&state.db, user.family_id, &req.uid)
        .await?
        .ok_or(ApiError::Internal)?;
    Ok((StatusCode::CREATED, Json(event)))
}

async fn update_event(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Json(req): Json<SaveEventRequest>,
) -> Result<Json<Event>, ApiError> {
    validate(&req)?;
    check_attendees(&state.db, user.family_id, &req.attendee_ids).await?;
    // Every edit flavour (plain, this-only, this-and-following) rewrites the
    // event named in the path, so one author-or-admin check covers all three.
    // The returned author is handed on to the scoped variants: the rows they
    // create (override / new master) keep belonging to the series' author.
    let author = require_event_owner(&state.db, &user, &uid).await?;

    // "Muokkaa vain tätä": don't touch the master — skip the original
    // occurrence (EXDATE) and create a replacement row in its place.
    if req.scope == Some(EditScope::ThisOnly) {
        return this_only_override(user, state, uid, req, author).await;
    }
    // "Tästä eteenpäin": split the series at the tapped occurrence.
    if req.scope == Some(EditScope::ThisAndFollowing) {
        return split_series(user, state, uid, req, author).await;
    }

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)?;

    let id = sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64" FROM events
           WHERE family_id = ? AND uid = ? AND deleted_at IS NULL"#,
        user.family_id,
        uid,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(id) = id else {
        return Err(ApiError::NotFound);
    };

    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "UPDATE events SET title = ?, all_day = ?, starts_at = ?, ends_at = ?,
             location = ?, notes = ?, rrule = ?, reminder_minutes = ?, updated_at = ?
         WHERE id = ?",
        req.title,
        req.all_day,
        req.starts_at,
        req.ends_at,
        req.location,
        req.notes,
        req.rrule,
        req.reminder_minutes,
        now,
        id,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!("DELETE FROM event_attendees WHERE event_id = ?", id)
        .execute(&mut *tx)
        .await?;
    for aid in &req.attendee_ids {
        sqlx::query!(
            "INSERT INTO event_attendees (event_id, user_id, family_id) VALUES (?, ?, ?)",
            id,
            aid,
            user.family_id,
        )
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;

    append_sync_log_entity(&state.db, user.family_id, id, "event", "update").await?;
    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "calendar_event",
        Some(id),
        "update",
        &req.title,
    )
    .await?;
    poke(&state, user.family_id).await;

    let event = load_event(&state.db, user.family_id, &uid)
        .await?
        .ok_or(ApiError::Internal)?;
    Ok(Json(event))
}

/// The one-occurrence edit: both halves land in a single transaction, and
/// both INSERTs are `ON CONFLICT DO NOTHING`, so an offline replay of the
/// same request (same client-generated override uid) changes nothing.
async fn this_only_override(
    user: CurrentUser,
    state: AppState,
    master_uid: String,
    req: SaveEventRequest,
    author: Option<i64>,
) -> Result<Json<Event>, ApiError> {
    // The override inherits the series author (see `series_author`), so an
    // admin fixing one occurrence doesn't lock the author out of it.
    let created_by = series_author(&user, author);
    let Some(occ) = req.recurrence_id.as_deref() else {
        return Err(ApiError::BadRequest("recurrence_id vaaditaan.".into()));
    };
    // The path must name an existing, still-recurring master.
    let (master_id, _) = require_master(&state.db, user.family_id, &master_uid).await?;

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)?;

    let mut tx = state.db.begin().await?;
    sqlx::query!(
        "INSERT INTO event_exdates (family_id, series_uid, occ_start)
         VALUES (?, ?, ?) ON CONFLICT DO NOTHING",
        user.family_id,
        master_uid,
        occ,
    )
    .execute(&mut *tx)
    .await?;
    // The override is a plain one-off row (rrule NULL) linked to its series.
    let inserted = sqlx::query!(
        "INSERT INTO events
            (family_id, uid, title, all_day, starts_at, ends_at, location, notes,
             series_uid, recurrence_id, reminder_minutes, created_by, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(family_id, uid) DO NOTHING",
        user.family_id,
        req.uid,
        req.title,
        req.all_day,
        req.starts_at,
        req.ends_at,
        req.location,
        req.notes,
        master_uid,
        occ,
        req.reminder_minutes,
        created_by,
        now,
        now,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        ensure_uid_live(&mut tx, user.family_id, &req.uid).await?;
    }
    if inserted == 1 {
        let id = sqlx::query_scalar!(
            r#"SELECT id AS "id!: i64" FROM events WHERE family_id = ? AND uid = ?"#,
            user.family_id,
            req.uid,
        )
        .fetch_one(&mut *tx)
        .await?;
        for aid in &req.attendee_ids {
            sqlx::query!(
                "INSERT INTO event_attendees (event_id, user_id, family_id) VALUES (?, ?, ?)",
                id,
                aid,
                user.family_id,
            )
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        append_sync_log_entity(&state.db, user.family_id, master_id, "event", "update").await?;
        // This is a terminal route handler (update_event returns here without
        // falling through to its own audit::record call), so exactly one row
        // per HTTP request still holds. The request always carries a title.
        crate::audit::record(
            &state.db,
            user.family_id,
            user.user_id,
            "calendar_event",
            Some(master_id),
            "update",
            &req.title,
        )
        .await?;
        poke(&state, user.family_id).await;
    } else {
        tx.commit().await?;
    }

    let event = load_event(&state.db, user.family_id, &req.uid)
        .await?
        .ok_or(ApiError::Internal)?;
    Ok(Json(event))
}

/// Author-or-admin gate for editing/deleting the event `uid`. Only a LIVE row
/// that the caller may not touch is refused (403); an unknown or already
/// deleted uid returns Ok so each route keeps its existing contract (404 for
/// an edit, idempotent 204 for a replayed delete).
///
/// On success returns the event's author (`None` for an unknown uid), so the
/// scoped edits can carry it over to the rows they create.
async fn require_event_owner(
    db: &crate::db::Db,
    user: &CurrentUser,
    uid: &str,
) -> Result<Option<i64>, ApiError> {
    let created_by = sqlx::query_scalar!(
        r#"SELECT created_by AS "created_by!: i64" FROM events
           WHERE family_id = ? AND uid = ? AND deleted_at IS NULL"#,
        user.family_id,
        uid,
    )
    .fetch_optional(db)
    .await?;
    match created_by {
        Some(cb) if !crate::grocery::can_modify(user, cb) => Err(ApiError::Forbidden),
        // Passing the Option through: `Some(author)` or `None` (unknown uid).
        other => Ok(other),
    }
}

/// Who owns a row that a scoped edit (this-only override, series split)
/// creates: the series' own author, so an admin editing a kid's weekly event
/// leaves every piece of it still editable by the kid. Falls back to the
/// caller only when the master was unknown — `require_master` 404s that case
/// right after, so the fallback is never actually stored.
fn series_author(user: &CurrentUser, author: Option<i64>) -> i64 {
    author.unwrap_or(user.user_id)
}

/// Look up a still-live series master by uid; errors mirror the routes'
/// contract (404 unknown, 400 for a one-off event).
async fn require_master(
    db: &crate::db::Db,
    family_id: i64,
    uid: &str,
) -> Result<(i64, String), ApiError> {
    let row = sqlx::query!(
        r#"SELECT id AS "id!: i64", rrule AS "rrule?: String" FROM events
           WHERE family_id = ? AND uid = ? AND deleted_at IS NULL"#,
        family_id,
        uid,
    )
    .fetch_optional(db)
    .await?;
    let Some(row) = row else {
        return Err(ApiError::NotFound);
    };
    let Some(rrule) = row.rrule else {
        return Err(ApiError::BadRequest("Tapahtuma ei toistu.".into()));
    };
    Ok((row.id, rrule))
}

/// Trim the master's rule to end before `occ` and sweep the series'
/// occurrence-level exceptions from the split point on (they belonged to the
/// part of the series that no longer exists). Setting the same trimmed rule
/// twice is a no-op, which is what makes the split replay-safe.
async fn trim_series(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    family_id: i64,
    master_id: i64,
    master_uid: &str,
    trimmed_rule: &str,
    occ: &str,
    now: &str,
) -> Result<(), ApiError> {
    sqlx::query!(
        "UPDATE events SET rrule = ?, updated_at = ? WHERE id = ?",
        trimmed_rule,
        now,
        master_id,
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "UPDATE events SET deleted_at = ?
         WHERE family_id = ? AND series_uid = ? AND recurrence_id >= ? AND deleted_at IS NULL",
        now,
        family_id,
        master_uid,
        occ,
    )
    .execute(&mut **tx)
    .await?;
    sqlx::query!(
        "DELETE FROM event_exdates
         WHERE family_id = ? AND series_uid = ? AND occ_start >= ?",
        family_id,
        master_uid,
        occ,
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

/// The series split: trim the old master, then create the request body as a
/// brand-new master starting at the split. One transaction; the new-master
/// INSERT is `ON CONFLICT DO NOTHING`, so offline replay is safe.
async fn split_series(
    user: CurrentUser,
    state: AppState,
    master_uid: String,
    req: SaveEventRequest,
    author: Option<i64>,
) -> Result<Json<Event>, ApiError> {
    // The new "from here on" master stays the series author's (see
    // `series_author`), not the editor's.
    let created_by = series_author(&user, author);
    let Some(occ) = req.recurrence_id.as_deref() else {
        return Err(ApiError::BadRequest("recurrence_id vaaditaan.".into()));
    };
    let (master_id, old_rule) = require_master(&state.db, user.family_id, &master_uid).await?;
    let Some(trimmed) = perkele_shared::recur::trim_until(&old_rule, occ) else {
        return Err(ApiError::BadRequest("Toistosääntö ei kelpaa.".into()));
    };

    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .map_err(|_| ApiError::Internal)?;

    let mut tx = state.db.begin().await?;
    trim_series(
        &mut tx,
        user.family_id,
        master_id,
        &master_uid,
        &trimmed,
        occ,
        &now,
    )
    .await?;
    let inserted = sqlx::query!(
        "INSERT INTO events
            (family_id, uid, title, all_day, starts_at, ends_at, location, notes,
             rrule, reminder_minutes, created_by, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(family_id, uid) DO NOTHING",
        user.family_id,
        req.uid,
        req.title,
        req.all_day,
        req.starts_at,
        req.ends_at,
        req.location,
        req.notes,
        req.rrule,
        req.reminder_minutes,
        created_by,
        now,
        now,
    )
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if inserted == 0 {
        ensure_uid_live(&mut tx, user.family_id, &req.uid).await?;
    }
    if inserted == 1 {
        let id = sqlx::query_scalar!(
            r#"SELECT id AS "id!: i64" FROM events WHERE family_id = ? AND uid = ?"#,
            user.family_id,
            req.uid,
        )
        .fetch_one(&mut *tx)
        .await?;
        for aid in &req.attendee_ids {
            sqlx::query!(
                "INSERT INTO event_attendees (event_id, user_id, family_id) VALUES (?, ?, ?)",
                id,
                aid,
                user.family_id,
            )
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    append_sync_log_entity(&state.db, user.family_id, master_id, "event", "update").await?;
    // Terminal route handler (update_event returns here without falling
    // through to its own audit::record call) — one row per HTTP request.
    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "calendar_event",
        Some(master_id),
        "update",
        &req.title,
    )
    .await?;
    poke(&state, user.family_id).await;

    let event = load_event(&state.db, user.family_id, &req.uid)
        .await?
        .ok_or(ApiError::Internal)?;
    Ok(Json(event))
}

/// `?occ=` on DELETE narrows it to one occurrence ("poista vain tämä");
/// `?from=` deletes that occurrence and everything after it (a trim).
#[derive(Deserialize)]
struct DeleteParams {
    occ: Option<String>,
    from: Option<String>,
}

async fn delete_event(
    user: CurrentUser,
    State(state): State<AppState>,
    Path(uid): Path<String>,
    Query(params): Query<DeleteParams>,
) -> Result<StatusCode, ApiError> {
    // Both values are stored / compared as occurrence starts: refuse
    // anything that isn't an exact, in-range timestamp before touching the DB.
    for v in [params.occ.as_deref(), params.from.as_deref()]
        .into_iter()
        .flatten()
    {
        validate_occurrence_start(v).map_err(|m| ApiError::BadRequest(m.to_owned()))?;
    }
    // Author-or-admin for all three variants (whole / ?occ= / ?from=). An
    // unknown uid passes here and stays the idempotent no-op below.
    require_event_owner(&state.db, &user, &uid).await?;
    let now = OffsetDateTime::now_utc();

    // One occurrence: record an EXDATE, leave the master alone. Idempotent
    // both via ON CONFLICT and via the unknown-uid no-op below.
    if let Some(occ) = params.occ.as_deref() {
        let id = sqlx::query_scalar!(
            r#"SELECT id AS "id!: i64" FROM events
               WHERE family_id = ? AND uid = ? AND deleted_at IS NULL"#,
            user.family_id,
            uid,
        )
        .fetch_optional(&state.db)
        .await?;
        let Some(id) = id else {
            return Ok(StatusCode::NO_CONTENT);
        };
        let inserted = sqlx::query!(
            "INSERT INTO event_exdates (family_id, series_uid, occ_start)
             VALUES (?, ?, ?) ON CONFLICT DO NOTHING",
            user.family_id,
            uid,
            occ,
        )
        .execute(&state.db)
        .await?
        .rows_affected();
        if inserted == 1 {
            append_sync_log_entity(&state.db, user.family_id, id, "event", "update").await?;
            // The master's row isn't touched (only an exdate is added), so
            // its title is still readable here for the audit label.
            let title = sqlx::query_scalar!(
                r#"SELECT title AS "title!" FROM events
                   WHERE id = ? AND family_id = ?"#,
                id,
                user.family_id,
            )
            .fetch_optional(&state.db)
            .await?
            .unwrap_or_default();
            crate::audit::record(
                &state.db,
                user.family_id,
                user.user_id,
                "calendar_event",
                Some(id),
                "update",
                &title,
            )
            .await?;
            poke(&state, user.family_id).await;
        }
        return Ok(StatusCode::NO_CONTENT);
    }

    // This-and-following: trim the rule so the series ends before `from`.
    if let Some(from) = params.from.as_deref() {
        let (master_id, old_rule) = match require_master(&state.db, user.family_id, &uid).await {
            Ok(v) => v,
            // Unknown/already-deleted series: replaying the trim is a no-op.
            Err(ApiError::NotFound) => return Ok(StatusCode::NO_CONTENT),
            Err(e) => return Err(e),
        };
        let Some(trimmed) = perkele_shared::recur::trim_until(&old_rule, from) else {
            return Err(ApiError::BadRequest("Toistosääntö ei kelpaa.".into()));
        };
        let now_s = now.format(&Rfc3339).map_err(|_| ApiError::Internal)?;
        let mut tx = state.db.begin().await?;
        trim_series(
            &mut tx,
            user.family_id,
            master_id,
            &uid,
            &trimmed,
            from,
            &now_s,
        )
        .await?;
        tx.commit().await?;
        append_sync_log_entity(&state.db, user.family_id, master_id, "event", "update").await?;
        // The trim only shortens the rrule; the master row (and its title)
        // survives, so it can still be looked up for the audit label.
        let title = sqlx::query_scalar!(
            r#"SELECT title AS "title!" FROM events
               WHERE id = ? AND family_id = ?"#,
            master_id,
            user.family_id,
        )
        .fetch_optional(&state.db)
        .await?
        .unwrap_or_default();
        crate::audit::record(
            &state.db,
            user.family_id,
            user.user_id,
            "calendar_event",
            Some(master_id),
            "update",
            &title,
        )
        .await?;
        poke(&state, user.family_id).await;
        return Ok(StatusCode::NO_CONTENT);
    }
    let id = sqlx::query_scalar!(
        r#"SELECT id AS "id!: i64" FROM events
           WHERE family_id = ? AND uid = ? AND deleted_at IS NULL"#,
        user.family_id,
        uid,
    )
    .fetch_optional(&state.db)
    .await?;
    let Some(id) = id else {
        // Idempotent: deleting an unknown/already-deleted uid is a no-op success.
        return Ok(StatusCode::NO_CONTENT);
    };
    // Capture the title before the row is soft-deleted, so the audit label
    // stays readable after the event is gone.
    let title = sqlx::query_scalar!(
        r#"SELECT title AS "title!" FROM events
           WHERE id = ? AND family_id = ?"#,
        id,
        user.family_id,
    )
    .fetch_optional(&state.db)
    .await?
    .unwrap_or_default();

    let mut tx = state.db.begin().await?;
    sqlx::query!("UPDATE events SET deleted_at = ? WHERE id = ?", now, id)
        .execute(&mut *tx)
        .await?;
    // Deleting a series sweeps its exception records with it: overrides are
    // soft-deleted like events, exdates are pure bookkeeping and just go.
    sqlx::query!(
        "UPDATE events SET deleted_at = ?
         WHERE family_id = ? AND series_uid = ? AND deleted_at IS NULL",
        now,
        user.family_id,
        uid,
    )
    .execute(&mut *tx)
    .await?;
    sqlx::query!(
        "DELETE FROM event_exdates WHERE family_id = ? AND series_uid = ?",
        user.family_id,
        uid,
    )
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    append_sync_log_entity(&state.db, user.family_id, id, "event", "delete").await?;
    crate::audit::record(
        &state.db,
        user.family_id,
        user.user_id,
        "calendar_event",
        Some(id),
        "delete",
        &title,
    )
    .await?;
    poke(&state, user.family_id).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn stream(
    user: CurrentUser,
    State(state): State<AppState>,
) -> impl axum::response::IntoResponse {
    let rx = {
        let mut map = state.sync_tx.lock().await;
        map.entry(user.family_id)
            .or_insert_with(|| broadcast::channel(16).0)
            .subscribe()
    };
    let stream = BroadcastStream::new(rx).filter_map(|r| {
        r.ok()
            .map(|_| Ok::<SseEvent, std::convert::Infallible>(SseEvent::default().data("updated")))
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    /// Router + pool. In-memory SQLite lives in ONE connection: seeding must
    /// use this pool, never a second one.
    async fn cal_app_with_db() -> (axum::Router, crate::db::Db) {
        let db = crate::db::test_pool().await;
        let state = AppState::for_test(db.clone());
        let app = crate::routes::router().merge(router()).with_state(state);
        (app, db)
    }

    async fn cal_app() -> axum::Router {
        cal_app_with_db().await.0
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

    /// Returns the admin's own user id (attendee lists need a real family member).
    async fn me_id(app: &axum::Router, cookie: &str) -> i64 {
        let resp = app
            .clone()
            .oneshot(req("GET", "/api/me", cookie, None))
            .await
            .unwrap();
        json_body(resp).await["id"].as_i64().unwrap()
    }

    fn sample_event(uid: &str, attendees: Vec<i64>) -> Value {
        json!({
            "uid": uid, "title": "Hammaslääkäri", "all_day": false,
            "starts_at": "2026-07-01T11:00:00Z", "ends_at": "2026-07-01T12:00:00Z",
            "location": "Keskusta", "notes": null, "attendee_ids": attendees
        })
    }

    #[tokio::test]
    async fn requires_auth() {
        let app = cal_app().await;
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/api/calendar/sync")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn create_then_sync_roundtrips() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        let uid_me = me_id(&app, &cookie).await;

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                &cookie,
                Some(sample_event("abc", vec![uid_me])),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = json_body(resp).await;
        assert_eq!(created["uid"], "abc");
        assert_eq!(created["attendee_ids"][0], uid_me);

        let resp = app
            .oneshot(req("GET", "/api/calendar/sync", &cookie, None))
            .await
            .unwrap();
        let body = json_body(resp).await;
        assert_eq!(body["events"].as_array().unwrap().len(), 1);
        assert_eq!(body["events"][0]["title"], "Hammaslääkäri");
        assert!(body["seq"].as_i64().unwrap() >= 1);
    }

    #[tokio::test]
    async fn create_is_idempotent_on_uid() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/api/calendar/events",
                    &cookie,
                    Some(sample_event("dup", vec![])),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }
        let body = json_body(
            app.oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["events"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn update_replaces_fields_and_attendees() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        let uid_me = me_id(&app, &cookie).await;
        app.clone()
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                &cookie,
                Some(sample_event("e1", vec![uid_me])),
            ))
            .await
            .unwrap();

        let updated = json!({ "uid": "e1", "title": "Siirretty", "all_day": true,
            "starts_at": "2026-07-04T00:00:00Z", "ends_at": "2026-07-04T00:00:00Z",
            "location": null, "notes": "koko päivä", "attendee_ids": [] });
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/calendar/events/e1",
                &cookie,
                Some(updated),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = json_body(resp).await;
        assert_eq!(body["title"], "Siirretty");
        assert_eq!(body["all_day"], true);
        assert_eq!(body["attendee_ids"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn delete_hides_from_sync() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        app.clone()
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                &cookie,
                Some(sample_event("gone", vec![])),
            ))
            .await
            .unwrap();
        let resp = app
            .clone()
            .oneshot(req("DELETE", "/api/calendar/events/gone", &cookie, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let body = json_body(
            app.oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["events"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn calendar_mutations_write_audit_rows() {
        let (app, db) = cal_app_with_db().await;
        let cookie = setup_admin(&app).await;

        // create
        app.clone()
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                &cookie,
                Some(sample_event("audit-1", vec![])),
            ))
            .await
            .unwrap();

        // update
        let updated = json!({ "uid": "audit-1", "title": "Siirretty aika", "all_day": false,
            "starts_at": "2026-07-02T11:00:00Z", "ends_at": "2026-07-02T12:00:00Z",
            "location": null, "notes": null, "attendee_ids": [] });
        app.clone()
            .oneshot(req(
                "PUT",
                "/api/calendar/events/audit-1",
                &cookie,
                Some(updated),
            ))
            .await
            .unwrap();

        // delete
        app.clone()
            .oneshot(req("DELETE", "/api/calendar/events/audit-1", &cookie, None))
            .await
            .unwrap();

        let rows = sqlx::query!(
            r#"SELECT entity AS "entity!", op AS "op!", label AS "label!" FROM audit_log ORDER BY id"#
        )
        .fetch_all(&db)
        .await
        .unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].entity, "calendar_event");
        assert_eq!(rows[0].op, "create");
        assert_eq!(rows[0].label, "Hammaslääkäri");
        assert_eq!(rows[1].entity, "calendar_event");
        assert_eq!(rows[1].op, "update");
        assert_eq!(rows[1].label, "Siirretty aika");
        assert_eq!(rows[2].entity, "calendar_event");
        assert_eq!(rows[2].op, "delete");
        assert_eq!(rows[2].label, "Siirretty aika");
    }

    #[tokio::test]
    async fn non_family_attendee_rejected() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        let resp = app
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                &cookie,
                Some(sample_event("bad", vec![9999])),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// A recurring event: the request carries an RRULE, and it must come back
    /// verbatim on create and in sync (the client expands it locally).
    #[tokio::test]
    async fn create_with_rrule_roundtrips() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;

        let mut body = sample_event("rec", vec![]);
        body["rrule"] = json!("FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE");
        let resp = app
            .clone()
            .oneshot(req("POST", "/api/calendar/events", &cookie, Some(body)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let created = json_body(resp).await;
        assert_eq!(created["rrule"], "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE");

        let synced = json_body(
            app.oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(
            synced["events"][0]["rrule"],
            "FREQ=WEEKLY;INTERVAL=2;BYDAY=MO,WE"
        );
    }

    /// Garbage and rules outside the editor subset are rejected at write time,
    /// so every stored rule is one the UI can render and edit.
    #[tokio::test]
    async fn invalid_or_unsupported_rrule_rejected() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        for bad_rule in ["FREQ=BOGUS", "FREQ=MONTHLY;BYMONTHDAY=15"] {
            let mut body = sample_event("badr", vec![]);
            body["rrule"] = json!(bad_rule);
            let resp = app
                .clone()
                .oneshot(req("POST", "/api/calendar/events", &cookie, Some(body)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad_rule}");
        }
    }

    /// PUT can turn a one-off into a series (set rrule) and back (clear it).
    #[tokio::test]
    async fn update_sets_and_clears_rrule() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        app.clone()
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                &cookie,
                Some(sample_event("s1", vec![])),
            ))
            .await
            .unwrap();

        let mut with_rule = sample_event("s1", vec![]);
        with_rule["rrule"] = json!("FREQ=DAILY;COUNT=5");
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/calendar/events/s1",
                &cookie,
                Some(with_rule),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(json_body(resp).await["rrule"], "FREQ=DAILY;COUNT=5");

        // No "rrule" key at all (old-client shape) must mean None → cleared.
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/calendar/events/s1",
                &cookie,
                Some(sample_event("s1", vec![])),
            ))
            .await
            .unwrap();
        assert_eq!(json_body(resp).await["rrule"], Value::Null);
    }

    /// "Poista vain tämä": DELETE with ?occ= records an EXDATE instead of
    /// touching the master, and replaying it is a no-op.
    #[tokio::test]
    async fn this_only_delete_records_exdate() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        let mut body = sample_event("rec", vec![]);
        body["rrule"] = json!("FREQ=WEEKLY;BYDAY=MO");
        body["starts_at"] = json!("2026-07-06T18:00:00Z");
        body["ends_at"] = json!("2026-07-06T19:00:00Z");
        app.clone()
            .oneshot(req("POST", "/api/calendar/events", &cookie, Some(body)))
            .await
            .unwrap();

        for _ in 0..2 {
            // Twice: an offline replay must not duplicate the exdate.
            let resp = app
                .clone()
                .oneshot(req(
                    "DELETE",
                    "/api/calendar/events/rec?occ=2026-07-13T18:00:00Z",
                    &cookie,
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        }

        let body = json_body(
            app.oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        // Master survives; the skip is data, not a row deletion.
        assert_eq!(body["events"].as_array().unwrap().len(), 1);
        assert_eq!(body["events"][0]["uid"], "rec");
        let exdates = body["exdates"].as_array().unwrap();
        assert_eq!(exdates.len(), 1);
        assert_eq!(exdates[0]["series_uid"], "rec");
        assert_eq!(exdates[0]["occ_start"], "2026-07-13T18:00:00Z");
    }

    /// "Muokkaa vain tätä": PUT on the master with scope=this_only records an
    /// EXDATE and creates an override row linked back to the series.
    #[tokio::test]
    async fn this_only_edit_creates_exdate_and_override() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        let mut master = sample_event("rec", vec![]);
        master["rrule"] = json!("FREQ=WEEKLY;BYDAY=MO");
        master["starts_at"] = json!("2026-07-06T18:00:00Z");
        master["ends_at"] = json!("2026-07-06T19:00:00Z");
        app.clone()
            .oneshot(req("POST", "/api/calendar/events", &cookie, Some(master)))
            .await
            .unwrap();

        // Move the July 13th occurrence one hour later.
        let over = json!({
            "uid": "ov1", "title": "Jumppa (siirretty)", "all_day": false,
            "starts_at": "2026-07-13T19:00:00Z", "ends_at": "2026-07-13T20:00:00Z",
            "location": null, "notes": null, "attendee_ids": [],
            "scope": "this_only", "recurrence_id": "2026-07-13T18:00:00Z"
        });
        for _ in 0..2 {
            // Twice: replay must not duplicate the override or the exdate.
            let resp = app
                .clone()
                .oneshot(req(
                    "PUT",
                    "/api/calendar/events/rec",
                    &cookie,
                    Some(over.clone()),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let returned = json_body(resp).await;
            assert_eq!(returned["uid"], "ov1");
            assert_eq!(returned["series_uid"], "rec");
            assert_eq!(returned["recurrence_id"], "2026-07-13T18:00:00Z");
        }

        let body = json_body(
            app.oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["events"].as_array().unwrap().len(), 2); // master + override
        assert_eq!(body["exdates"].as_array().unwrap().len(), 1);
    }

    /// Deleting the whole series sweeps its overrides and exdates with it.
    #[tokio::test]
    async fn delete_all_cascades_to_overrides_and_exdates() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        let mut master = sample_event("rec", vec![]);
        master["rrule"] = json!("FREQ=WEEKLY;BYDAY=MO");
        master["starts_at"] = json!("2026-07-06T18:00:00Z");
        master["ends_at"] = json!("2026-07-06T19:00:00Z");
        app.clone()
            .oneshot(req("POST", "/api/calendar/events", &cookie, Some(master)))
            .await
            .unwrap();
        let over = json!({
            "uid": "ov1", "title": "Siirretty", "all_day": false,
            "starts_at": "2026-07-13T19:00:00Z", "ends_at": "2026-07-13T20:00:00Z",
            "location": null, "notes": null, "attendee_ids": [],
            "scope": "this_only", "recurrence_id": "2026-07-13T18:00:00Z"
        });
        app.clone()
            .oneshot(req("PUT", "/api/calendar/events/rec", &cookie, Some(over)))
            .await
            .unwrap();

        // Precondition: the override + exdate really exist before the delete,
        // so the final zero-counts prove the cascade (not a vacuous pass).
        let body = json_body(
            app.clone()
                .oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["events"].as_array().unwrap().len(), 2);
        assert_eq!(body["exdates"].as_array().unwrap().len(), 1);

        let resp = app
            .clone()
            .oneshot(req("DELETE", "/api/calendar/events/rec", &cookie, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);

        let body = json_body(
            app.oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["events"].as_array().unwrap().len(), 0);
        assert_eq!(body["exdates"].as_array().unwrap().len(), 0);
    }

    /// Seeds the standard weekly-Monday master used by the split tests.
    async fn seed_weekly_master(app: &axum::Router, cookie: &str) {
        let mut master = sample_event("rec", vec![]);
        master["rrule"] = json!("FREQ=WEEKLY;BYDAY=MO");
        master["starts_at"] = json!("2026-07-06T18:00:00Z");
        master["ends_at"] = json!("2026-07-06T19:00:00Z");
        app.clone()
            .oneshot(req("POST", "/api/calendar/events", cookie, Some(master)))
            .await
            .unwrap();
    }

    /// Admin mints a kid invite, "kalle" redeems it; returns kalle's cookie.
    async fn add_kid(app: &axum::Router, admin: &str) -> String {
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/family/invites",
                admin,
                Some(json!({ "role": "kid" })),
            ))
            .await
            .unwrap();
        let code = json_body(resp).await["code"].as_str().unwrap().to_owned();
        let resp = app
            .clone()
            .oneshot(post_setup(
                "/api/auth/redeem",
                json!({ "code": code, "username": "kalle",
                        "display_name": "Kalle", "password": "hunter2!" }),
            ))
            .await
            .unwrap();
        cookie_pair(&resp)
    }

    /// Author-or-admin on every destructive calendar route: a kid gets 403 on
    /// the admin's series for a plain edit, both scoped edits, a whole delete
    /// and both scoped deletes — and the series is left exactly as it was.
    #[tokio::test]
    async fn kid_cannot_edit_or_delete_others_event() {
        let app = cal_app().await;
        let admin = setup_admin(&app).await;
        let kid = add_kid(&app, &admin).await;
        seed_weekly_master(&app, &admin).await;

        let mut plain = sample_event("rec", vec![]);
        plain["title"] = json!("Kallen juttu");
        let mut this_only = plain.clone();
        this_only["uid"] = json!("ovr");
        this_only["scope"] = json!("this_only");
        this_only["recurrence_id"] = json!("2026-07-13T18:00:00Z");
        let mut following = plain.clone();
        following["uid"] = json!("m2");
        following["rrule"] = json!("FREQ=WEEKLY;BYDAY=MO");
        following["scope"] = json!("this_and_following");
        following["recurrence_id"] = json!("2026-07-20T18:00:00Z");
        for body in [plain, this_only, following] {
            let resp = app
                .clone()
                .oneshot(req("PUT", "/api/calendar/events/rec", &kid, Some(body)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        }
        for q in [
            "",
            "?occ=2026-07-13T18:00:00Z",
            "?from=2026-07-20T18:00:00Z",
        ] {
            let resp = app
                .clone()
                .oneshot(req(
                    "DELETE",
                    &format!("/api/calendar/events/rec{q}"),
                    &kid,
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{q}");
        }

        let body = json_body(
            app.oneshot(req("GET", "/api/calendar/sync", &admin, None))
                .await
                .unwrap(),
        )
        .await;
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["title"], "Hammaslääkäri");
        assert_eq!(events[0]["rrule"], "FREQ=WEEKLY;BYDAY=MO");
        assert!(body["exdates"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn kid_can_edit_and_delete_own_event_and_admin_can_too() {
        let app = cal_app().await;
        let admin = setup_admin(&app).await;
        let kid = add_kid(&app, &admin).await;
        for uid in ["k1", "k2"] {
            let resp = app
                .clone()
                .oneshot(req(
                    "POST",
                    "/api/calendar/events",
                    &kid,
                    Some(sample_event(uid, vec![])),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CREATED);
        }
        // Kid edits + deletes their own; admin edits + deletes the kid's.
        for (uid, who) in [("k1", &kid), ("k2", &admin)] {
            let mut edit = sample_event(uid, vec![]);
            edit["title"] = json!("Muutettu");
            let resp = app
                .clone()
                .oneshot(req(
                    "PUT",
                    &format!("/api/calendar/events/{uid}"),
                    who,
                    Some(edit),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let resp = app
                .clone()
                .oneshot(req(
                    "DELETE",
                    &format!("/api/calendar/events/{uid}"),
                    who,
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        }
    }

    /// Rows created by an admin's scoped edit of a kid's series (the
    /// this-only override and the "from here on" master) stay the KID's: the
    /// kid can still edit and delete every piece of their own event.
    #[tokio::test]
    async fn admin_scoped_edits_keep_the_series_author() {
        let app = cal_app().await;
        let admin = setup_admin(&app).await;
        let kid = add_kid(&app, &admin).await;
        let kid_id = me_id(&app, &kid).await;
        seed_weekly_master(&app, &kid).await;

        let mut this_only = sample_event("ovr", vec![]);
        this_only["title"] = json!("Siirretty");
        this_only["scope"] = json!("this_only");
        this_only["recurrence_id"] = json!("2026-07-13T18:00:00Z");
        let mut following = sample_event("m2", vec![]);
        following["rrule"] = json!("FREQ=WEEKLY;BYDAY=TU");
        following["starts_at"] = json!("2026-07-21T18:00:00Z");
        following["ends_at"] = json!("2026-07-21T19:00:00Z");
        following["scope"] = json!("this_and_following");
        following["recurrence_id"] = json!("2026-07-20T18:00:00Z");
        for body in [this_only, following] {
            let resp = app
                .clone()
                .oneshot(req("PUT", "/api/calendar/events/rec", &admin, Some(body)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            assert_eq!(json_body(resp).await["created_by"], kid_id);
        }

        // The kid edits the override and deletes the new master.
        let mut edit = sample_event("ovr", vec![]);
        edit["title"] = json!("Kallen oma");
        let resp = app
            .clone()
            .oneshot(req("PUT", "/api/calendar/events/ovr", &kid, Some(edit)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp = app
            .oneshot(req("DELETE", "/api/calendar/events/m2", &kid, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    /// "Tästä eteenpäin": the old master is trimmed to end the day before the
    /// split and a NEW master starts from it, carrying the edits.
    #[tokio::test]
    async fn this_and_following_edit_splits_series() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        seed_weekly_master(&app, &cookie).await;

        // From July 20th on, the session moves to 19:00.
        let split = json!({
            "uid": "m2", "title": "Jumppa", "all_day": false,
            "starts_at": "2026-07-20T19:00:00Z", "ends_at": "2026-07-20T20:00:00Z",
            "location": null, "notes": null, "attendee_ids": [],
            "rrule": "FREQ=WEEKLY;BYDAY=MO",
            "scope": "this_and_following", "recurrence_id": "2026-07-20T18:00:00Z"
        });
        for _ in 0..2 {
            // Twice: replaying the queued split must change nothing.
            let resp = app
                .clone()
                .oneshot(req(
                    "PUT",
                    "/api/calendar/events/rec",
                    &cookie,
                    Some(split.clone()),
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let returned = json_body(resp).await;
            assert_eq!(returned["uid"], "m2");
            assert_eq!(returned["rrule"], "FREQ=WEEKLY;BYDAY=MO");
        }

        let body = json_body(
            app.oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 2);
        let old = events.iter().find(|e| e["uid"] == "rec").unwrap();
        assert_eq!(old["rrule"], "FREQ=WEEKLY;BYDAY=MO;UNTIL=20260719T235959Z");
        let new = events.iter().find(|e| e["uid"] == "m2").unwrap();
        assert_eq!(new["starts_at"], "2026-07-20T19:00:00Z");
    }

    /// "Muokkaa vain tätä" is a terminal route handler (`this_only_override`)
    /// that returns before `update_event`'s own audit::record call, so it
    /// needs — and must produce exactly — its own audit row.
    #[tokio::test]
    async fn this_only_edit_writes_one_audit_row() {
        let (app, db) = cal_app_with_db().await;
        let cookie = setup_admin(&app).await;
        let mut master = sample_event("rec", vec![]);
        master["rrule"] = json!("FREQ=WEEKLY;BYDAY=MO");
        master["starts_at"] = json!("2026-07-06T18:00:00Z");
        master["ends_at"] = json!("2026-07-06T19:00:00Z");
        app.clone()
            .oneshot(req("POST", "/api/calendar/events", &cookie, Some(master)))
            .await
            .unwrap();

        let over = json!({
            "uid": "ov1", "title": "Jumppa (siirretty)", "all_day": false,
            "starts_at": "2026-07-13T19:00:00Z", "ends_at": "2026-07-13T20:00:00Z",
            "location": null, "notes": null, "attendee_ids": [],
            "scope": "this_only", "recurrence_id": "2026-07-13T18:00:00Z"
        });
        let resp = app
            .oneshot(req("PUT", "/api/calendar/events/rec", &cookie, Some(over)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let rows = sqlx::query!(
            r#"SELECT entity AS "entity!", op AS "op!", label AS "label!" FROM audit_log ORDER BY id"#
        )
        .fetch_all(&db)
        .await
        .unwrap();
        // Just the create (master) + the this_only update — one row per
        // HTTP request, none leaking out of `update_event`'s own fallthrough.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].entity, "calendar_event");
        assert_eq!(rows[1].op, "update");
        assert_eq!(rows[1].label, "Jumppa (siirretty)");
    }

    /// "Tästä eteenpäin" is likewise terminal (`split_series`) and must write
    /// its own audit row against the (trimmed) master's id.
    #[tokio::test]
    async fn this_and_following_edit_writes_one_audit_row() {
        let (app, db) = cal_app_with_db().await;
        let cookie = setup_admin(&app).await;
        seed_weekly_master(&app, &cookie).await;

        let split = json!({
            "uid": "m2", "title": "Jumppa (myöhemmin)", "all_day": false,
            "starts_at": "2026-07-20T19:00:00Z", "ends_at": "2026-07-20T20:00:00Z",
            "location": null, "notes": null, "attendee_ids": [],
            "rrule": "FREQ=WEEKLY;BYDAY=MO",
            "scope": "this_and_following", "recurrence_id": "2026-07-20T18:00:00Z"
        });
        let resp = app
            .oneshot(req("PUT", "/api/calendar/events/rec", &cookie, Some(split)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let rows = sqlx::query!(
            r#"SELECT entity AS "entity!", op AS "op!", label AS "label!" FROM audit_log ORDER BY id"#
        )
        .fetch_all(&db)
        .await
        .unwrap();
        // Just the create (master) + the split update.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].entity, "calendar_event");
        assert_eq!(rows[1].op, "update");
        assert_eq!(rows[1].label, "Jumppa (myöhemmin)");
    }

    /// Deleting "tästä eteenpäin" just trims the master's rule.
    #[tokio::test]
    async fn this_and_following_delete_trims_master() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        seed_weekly_master(&app, &cookie).await;

        for _ in 0..2 {
            let resp = app
                .clone()
                .oneshot(req(
                    "DELETE",
                    "/api/calendar/events/rec?from=2026-07-20T18:00:00Z",
                    &cookie,
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        }

        let body = json_body(
            app.oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["events"].as_array().unwrap().len(), 1);
        assert_eq!(
            body["events"][0]["rrule"],
            "FREQ=WEEKLY;BYDAY=MO;UNTIL=20260719T235959Z"
        );
    }

    /// A split supersedes single-occurrence exceptions AFTER the split point:
    /// they belonged to the part of the series that no longer exists.
    #[tokio::test]
    async fn split_sweeps_later_exceptions() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        seed_weekly_master(&app, &cookie).await;

        // First: a this-only edit on July 27th (override + exdate).
        let over = json!({
            "uid": "ov1", "title": "Siirretty", "all_day": false,
            "starts_at": "2026-07-27T20:00:00Z", "ends_at": "2026-07-27T21:00:00Z",
            "location": null, "notes": null, "attendee_ids": [],
            "scope": "this_only", "recurrence_id": "2026-07-27T18:00:00Z"
        });
        app.clone()
            .oneshot(req("PUT", "/api/calendar/events/rec", &cookie, Some(over)))
            .await
            .unwrap();

        // Then split at July 20th — before the override's occurrence.
        let split = json!({
            "uid": "m2", "title": "Jumppa", "all_day": false,
            "starts_at": "2026-07-20T19:00:00Z", "ends_at": "2026-07-20T20:00:00Z",
            "location": null, "notes": null, "attendee_ids": [],
            "rrule": "FREQ=WEEKLY;BYDAY=MO",
            "scope": "this_and_following", "recurrence_id": "2026-07-20T18:00:00Z"
        });
        app.clone()
            .oneshot(req("PUT", "/api/calendar/events/rec", &cookie, Some(split)))
            .await
            .unwrap();

        let body = json_body(
            app.oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        let events = body["events"].as_array().unwrap();
        // Old (trimmed) + new master; the July 27th override is swept.
        assert_eq!(events.len(), 2);
        assert!(events.iter().all(|e| e["uid"] != "ov1"));
        assert_eq!(body["exdates"].as_array().unwrap().len(), 0);
    }

    /// The reminder offset survives create → sync → update → clear.
    #[tokio::test]
    async fn reminder_minutes_round_trips() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;

        let mut body = sample_event("rem", vec![]);
        body["reminder_minutes"] = json!(30);
        let resp = app
            .clone()
            .oneshot(req("POST", "/api/calendar/events", &cookie, Some(body)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        assert_eq!(json_body(resp).await["reminder_minutes"], 30);

        let synced = json_body(
            app.clone()
                .oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(synced["events"][0]["reminder_minutes"], 30);

        // Absent key (old-client shape) clears it, like rrule.
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                "/api/calendar/events/rem",
                &cookie,
                Some(sample_event("rem", vec![])),
            ))
            .await
            .unwrap();
        assert_eq!(json_body(resp).await["reminder_minutes"], Value::Null);
    }

    /// Security finding: out-of-range reminder offsets crashed the reminder
    /// scheduler inside chrono. Rejected on create AND update.
    #[tokio::test]
    async fn out_of_range_reminder_minutes_rejected() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                &cookie,
                Some(sample_event("ok", vec![])),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        for m in [
            json!(200_000_000_000_000_i64),
            json!(-100_000_000_000_000_i64),
            json!(i64::MIN),
            json!(-1),
            json!(1441),
        ] {
            let mut body = sample_event("bad", vec![]);
            body["reminder_minutes"] = m.clone();
            let resp = app
                .clone()
                .oneshot(req("POST", "/api/calendar/events", &cookie, Some(body)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "create {m}");

            let mut body = sample_event("ok", vec![]);
            body["reminder_minutes"] = m.clone();
            let resp = app
                .clone()
                .oneshot(req("PUT", "/api/calendar/events/ok", &cookie, Some(body)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "update {m}");
        }
    }

    /// Year-0001 DTSTART + FREQ=DAILY made each expansion walk 2000 years;
    /// overlong free text is rejected too.
    #[tokio::test]
    async fn out_of_range_dates_and_overlong_fields_rejected() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        let mut ancient = sample_event("a", vec![]);
        ancient["starts_at"] = json!("0001-01-01T00:00:00Z");
        ancient["rrule"] = json!("FREQ=DAILY");
        let mut far = sample_event("f", vec![]);
        far["ends_at"] = json!("9999-01-01T00:00:00Z");
        let mut loc = sample_event("l", vec![]);
        loc["location"] = json!("x".repeat(201));
        let mut notes = sample_event("n", vec![]);
        notes["notes"] = json!("x".repeat(4001));
        let long_uid = "u".repeat(65);
        let long_uid_ev = sample_event(&long_uid, vec![]);
        for body in [ancient, far, loc, notes, long_uid_ev] {
            let resp = app
                .clone()
                .oneshot(req("POST", "/api/calendar/events", &cookie, Some(body)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        }
    }

    /// `?occ=` / `?from=` / `recurrence_id` are stored as occurrence starts;
    /// anything not timestamp-shaped is refused before it reaches the DB.
    #[tokio::test]
    async fn malformed_occurrence_values_rejected() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        seed_weekly_master(&app, &cookie).await;

        for q in ["occ=garbage", "occ=0001-01-01T00:00:00Z", "from=garbage"] {
            let resp = app
                .clone()
                .oneshot(req(
                    "DELETE",
                    &format!("/api/calendar/events/rec?{q}"),
                    &cookie,
                    None,
                ))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{q}");
        }
        let synced = json_body(
            app.clone()
                .oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(synced["exdates"].as_array().unwrap().len(), 0);

        let mut body = sample_event("ovr", vec![]);
        body["scope"] = json!("this_only");
        body["recurrence_id"] = json!("not-a-time");
        let resp = app
            .oneshot(req("PUT", "/api/calendar/events/rec", &cookie, Some(body)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn end_before_start_rejected() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        let bad = json!({ "uid": "x", "title": "T", "all_day": false,
            "starts_at": "2026-07-01T12:00:00Z", "ends_at": "2026-07-01T10:00:00Z",
            "location": null, "notes": null, "attendee_ids": [] });
        let resp = app
            .oneshot(req("POST", "/api/calendar/events", &cookie, Some(bad)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    /// POST then DELETE `uid`, leaving a soft-deleted row behind.
    async fn create_and_delete(app: &axum::Router, cookie: &str, uid: &str) {
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                cookie,
                Some(sample_event(uid, vec![])),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let resp = app
            .clone()
            .oneshot(req(
                "DELETE",
                &format!("/api/calendar/events/{uid}"),
                cookie,
                None,
            ))
            .await
            .unwrap();
        assert!(resp.status().is_success());
    }

    /// Too many or duplicate attendees are a clean 400 on create and update —
    /// a duplicate used to hit event_attendees' primary key and 500.
    #[tokio::test]
    async fn attendee_cap_and_duplicates_rejected() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        let me = me_id(&app, &cookie).await;
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                &cookie,
                Some(sample_event("ok", vec![me])),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);

        let too_many: Vec<i64> = (1..=51).collect();
        for attendees in [vec![me, me], too_many] {
            for (method, path) in [
                ("POST", "/api/calendar/events"),
                ("PUT", "/api/calendar/events/ok"),
            ] {
                let mut body = sample_event("ok", attendees.clone());
                if method == "POST" {
                    body["uid"] = json!("new");
                }
                let resp = app
                    .clone()
                    .oneshot(req(method, path, &cookie, Some(body)))
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{method} {path}");
                // Rejected by validate_event, not by the family-member check.
                let err = json_body(resp).await["error"].as_str().unwrap().to_owned();
                assert_ne!(err, "Tuntematon osallistuja.");
            }
        }
    }

    /// Reusing a soft-deleted uid (an offline create replayed after someone
    /// deleted the event) is a 409, not a 500.
    #[tokio::test]
    async fn create_with_deleted_uid_conflicts() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        create_and_delete(&app, &cookie, "gone").await;
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/api/calendar/events",
                &cookie,
                Some(sample_event("gone", vec![])),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = json_body(
            app.oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(body["events"].as_array().unwrap().len(), 0);
    }

    /// Same for both scoped edits — and the whole transaction rolls back, so
    /// the master is neither EXDATE'd nor trimmed.
    #[tokio::test]
    async fn scoped_edits_with_deleted_uid_conflict() {
        let app = cal_app().await;
        let cookie = setup_admin(&app).await;
        seed_weekly_master(&app, &cookie).await;
        create_and_delete(&app, &cookie, "gone").await;

        for scope in ["this_only", "this_and_following"] {
            let body = json!({
                "uid": "gone", "title": "Jumppa", "all_day": false,
                "starts_at": "2026-07-20T19:00:00Z", "ends_at": "2026-07-20T20:00:00Z",
                "location": null, "notes": null, "attendee_ids": [],
                "rrule": if scope == "this_only" { Value::Null } else { json!("FREQ=WEEKLY;BYDAY=MO") },
                "scope": scope, "recurrence_id": "2026-07-20T18:00:00Z"
            });
            let resp = app
                .clone()
                .oneshot(req("PUT", "/api/calendar/events/rec", &cookie, Some(body)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::CONFLICT, "{scope}");
        }

        let body = json_body(
            app.oneshot(req("GET", "/api/calendar/sync", &cookie, None))
                .await
                .unwrap(),
        )
        .await;
        let events = body["events"].as_array().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["rrule"], "FREQ=WEEKLY;BYDAY=MO");
        assert_eq!(body["exdates"].as_array().unwrap().len(), 0);
    }
}

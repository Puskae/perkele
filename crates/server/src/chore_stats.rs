//! Chore stats: leaderboard totals, per-chore breakdown, and a 12-week trend.
//! Read-only aggregates over chore_completions. Windows are computed from the
//! server's LOCAL date (chores are floating local dates) and never stored —
//! "this week" is Monday-to-today, "this month" is the 1st-to-today.

use crate::AppState;
use crate::chore::{load_chores, local_today};
use crate::error::ApiError;
use crate::session::CurrentUser;
use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{Datelike, Duration, NaiveDate};
use perkele_shared::chore::{ChoreBreakdown, ChoreStats, MemberCount, MemberTotal, WeekBucket};
use perkele_shared::recur;
use std::collections::BTreeMap;

pub fn router() -> Router<AppState> {
    Router::new().route("/api/chores/stats", get(stats))
}

#[derive(serde::Deserialize)]
struct StatsParams {
    window: String,
}

/// Monday of the week containing `d` — Finnish weeks start on Monday.
fn monday_of(d: NaiveDate) -> NaiveDate {
    d - Duration::days(i64::from(d.weekday().num_days_from_monday()))
}

/// Same cap as shared::chore uses for rotation math: 65535 daily occurrences
/// ≈ 179 years, i.e. effectively unbounded for a family app.
const EXPAND_CAP: u16 = u16::MAX;

async fn stats(
    user: CurrentUser,
    State(state): State<AppState>,
    Query(p): Query<StatsParams>,
) -> Result<Json<ChoreStats>, ApiError> {
    let today = local_today();
    let today_d = NaiveDate::parse_from_str(&today, "%Y-%m-%d").map_err(|_| ApiError::Internal)?;
    // The window's inclusive lower bound as an ISO date. "0000-01-01" sorts
    // before every real date, so one string comparison in SQL covers "all"
    // too — dates in this schema are TEXT and ISO order == chronological.
    let from = match p.window.as_str() {
        "week" => monday_of(today_d).format("%Y-%m-%d").to_string(),
        "month" => today_d
            .with_day(1)
            .ok_or(ApiError::Internal)?
            .format("%Y-%m-%d")
            .to_string(),
        "all" => "0000-01-01".to_owned(),
        _ => return Err(ApiError::BadRequest("Tuntematon aikaväli.".to_owned())),
    };
    let fid = user.family_id;

    // Leaderboard totals. JOIN chores for family scoping but do NOT filter
    // deleted_at: completions of soft-deleted chores still count — deleting
    // a chore must never confiscate points someone already earned.
    let totals: Vec<MemberTotal> = sqlx::query!(
        r#"SELECT cc.completed_by AS "user_id!: i64",
                  SUM(cc.points)  AS "points!: i64",
                  COUNT(*)        AS "completions!: i64"
           FROM chore_completions cc JOIN chores ch ON ch.id = cc.chore_id
           WHERE ch.family_id = ? AND cc.date >= ?
           GROUP BY cc.completed_by
           ORDER BY SUM(cc.points) DESC, cc.completed_by"#,
        fid,
        from,
    )
    .fetch_all(&state.db)
    .await?
    .into_iter()
    .map(|r| MemberTotal {
        user_id: r.user_id,
        points: r.points,
        completions: r.completions,
    })
    .collect();

    // 12-week trend: fetch raw rows once, bucket by Monday in Rust (simpler
    // and more testable than SQLite date functions). Every week is present
    // even when empty so the chart's columns line up client-side.
    let mondays: Vec<String> = (0..12)
        .map(|i| {
            (monday_of(today_d) - Duration::weeks(11 - i))
                .format("%Y-%m-%d")
                .to_string()
        })
        .collect();
    let trend_rows = sqlx::query!(
        r#"SELECT cc.completed_by AS "user_id!: i64",
                  cc.date         AS "date!: String",
                  cc.points       AS "points!: i64"
           FROM chore_completions cc JOIN chores ch ON ch.id = cc.chore_id
           WHERE ch.family_id = ? AND cc.date >= ?"#,
        fid,
        mondays[0],
    )
    .fetch_all(&state.db)
    .await?;
    let mut buckets: BTreeMap<String, BTreeMap<i64, i64>> = mondays
        .iter()
        .map(|m| (m.clone(), BTreeMap::new()))
        .collect();
    for r in trend_rows {
        if let Ok(d) = NaiveDate::parse_from_str(&r.date, "%Y-%m-%d") {
            let wk = monday_of(d).format("%Y-%m-%d").to_string();
            if let Some(b) = buckets.get_mut(&wk) {
                *b.entry(r.user_id).or_insert(0) += r.points;
            }
        }
    }
    let trend: Vec<WeekBucket> = mondays
        .iter()
        .map(|m| WeekBucket {
            week_start: m.clone(),
            points: buckets[m]
                .iter()
                .map(|(u, p)| MemberCount {
                    user_id: *u,
                    count: *p,
                })
                .collect(),
        })
        .collect();

    // Per-chore breakdown — live chores only (a deleted chore has no rrule
    // future, and its points already live on in `totals`).
    let mut per_chore = Vec::new();
    for c in load_chores(&state.db, fid).await? {
        // Expansion window: the later of window start and the chore's own
        // start, so a chore created mid-window isn't "due" before it existed.
        let win_start = if from.as_str() > c.start_date.as_str() {
            from.clone()
        } else {
            c.start_date.clone()
        };
        let due = recur::expand(
            &c.rrule,
            &format!("{}T00:00:00Z", c.start_date),
            &format!("{win_start}T00:00:00Z"),
            &format!("{today}T23:59:59Z"),
            EXPAND_CAP,
        )
        .len() as i64;
        let rows = sqlx::query!(
            r#"SELECT completed_by AS "user_id!: i64", COUNT(*) AS "n!: i64"
               FROM chore_completions
               WHERE chore_id = ? AND date >= ?
               GROUP BY completed_by ORDER BY completed_by"#,
            c.id,
            from,
        )
        .fetch_all(&state.db)
        .await?;
        per_chore.push(ChoreBreakdown {
            chore_id: c.id,
            title: c.title.clone(),
            due,
            done: rows.iter().map(|r| r.n).sum(),
            by: rows
                .into_iter()
                .map(|r| MemberCount {
                    user_id: r.user_id,
                    count: r.n,
                })
                .collect(),
        });
    }

    Ok(Json(ChoreStats {
        totals,
        per_chore,
        trend,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn stats_app_with_db() -> (axum::Router, crate::db::Db) {
        let db = crate::db::test_pool().await;
        let state = AppState::for_test(db.clone());
        let app = crate::routes::router()
            .merge(crate::chore::router())
            .merge(router())
            .with_state(state);
        (app, db)
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

    /// Create a chore due every day since 2020 (deterministic: always due
    /// today) with an explicit point value, returning its id.
    async fn make_chore(app: &axum::Router, admin: &str, title: &str, points: i64) -> i64 {
        json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/chores",
                    admin,
                    Some(json!({ "title": title, "rrule": "FREQ=DAILY",
                                 "start_date": "2020-01-01", "assigned_user_id": null,
                                 "rotation": null, "remind_at": null, "points": points })),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap()
    }

    async fn complete(app: &axum::Router, cookie: &str, id: i64) {
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                &format!("/api/chores/{id}/complete"),
                cookie,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }

    /// Backdate a completion straight into the DB — the API only writes
    /// "today" (lapse model), but stats tests need history.
    async fn insert_completion(
        db: &crate::db::Db,
        chore_id: i64,
        days_ago: i64,
        user_id: i64,
        points: i64,
    ) {
        let date = (chrono::Local::now().date_naive() - Duration::days(days_ago))
            .format("%Y-%m-%d")
            .to_string();
        sqlx::query(
            "INSERT INTO chore_completions (chore_id, date, completed_by, completed_at, points)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(chore_id)
        .bind(date)
        .bind(user_id)
        .bind("2020-01-01T00:00:00Z")
        .bind(points)
        .execute(db)
        .await
        .unwrap();
    }

    async fn get_stats(app: &axum::Router, cookie: &str, window: &str) -> Value {
        let resp = app
            .clone()
            .oneshot(req(
                "GET",
                &format!("/api/chores/stats?window={window}"),
                cookie,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        json_body(resp).await
    }

    #[tokio::test]
    async fn stats_requires_auth_and_a_known_window() {
        let (app, _db) = stats_app_with_db().await;
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/chores/stats?window=week")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        let admin = setup_admin(&app).await;
        let resp = app
            .oneshot(req("GET", "/api/chores/stats?window=vuosi", &admin, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn windows_cut_history_correctly() {
        let (app, db) = stats_app_with_db().await;
        let admin = setup_admin(&app).await;
        let id = make_chore(&app, &admin, "Tiskit", 5).await;
        complete(&app, &admin, id).await; // today: admin (user 1) earns 5
        insert_completion(&db, id, 400, 1, 7).await; // >1 year ago: 7 more

        // Today is always inside the current week and month.
        for w in ["week", "month"] {
            let s = get_stats(&app, &admin, w).await;
            assert_eq!(s["totals"].as_array().unwrap().len(), 1, "window {w}");
            assert_eq!(s["totals"][0]["points"], 5, "window {w}");
            assert_eq!(s["totals"][0]["completions"], 1, "window {w}");
        }
        let s = get_stats(&app, &admin, "all").await;
        assert_eq!(s["totals"][0]["points"], 12);
        assert_eq!(s["totals"][0]["completions"], 2);
    }

    #[tokio::test]
    async fn editing_points_does_not_rewrite_standings() {
        let (app, _db) = stats_app_with_db().await;
        let admin = setup_admin(&app).await;
        let id = make_chore(&app, &admin, "Imurointi", 5).await;
        complete(&app, &admin, id).await;
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                &format!("/api/chores/{id}"),
                &admin,
                Some(json!({ "title": "Imurointi", "rrule": "FREQ=DAILY",
                             "start_date": "2020-01-01", "assigned_user_id": null,
                             "rotation": null, "remind_at": null, "points": 1 })),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let s = get_stats(&app, &admin, "all").await;
        assert_eq!(s["totals"][0]["points"], 5); // snapshot, not current value
    }

    #[tokio::test]
    async fn untick_takes_the_points_back() {
        let (app, _db) = stats_app_with_db().await;
        let admin = setup_admin(&app).await;
        let id = make_chore(&app, &admin, "Roskat", 3).await;
        complete(&app, &admin, id).await;
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
        let s = get_stats(&app, &admin, "all").await;
        assert_eq!(s["totals"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn per_chore_breakdown_counts_due_done_and_by() {
        let (app, _db) = stats_app_with_db().await;
        let admin = setup_admin(&app).await;
        // start_date = today → due exactly once in every window. The 2020
        // chore from other tests would make `due` depend on the run date.
        let today = local_today();
        let id = json_body(
            app.clone()
                .oneshot(req(
                    "POST",
                    "/api/chores",
                    &admin,
                    Some(json!({ "title": "Tiskit", "rrule": "FREQ=DAILY",
                                 "start_date": today, "assigned_user_id": null,
                                 "rotation": null, "remind_at": null, "points": 2 })),
                ))
                .await
                .unwrap(),
        )
        .await["id"]
            .as_i64()
            .unwrap();
        let s = get_stats(&app, &admin, "week").await;
        assert_eq!(s["per_chore"][0]["due"], 1);
        assert_eq!(s["per_chore"][0]["done"], 0);
        complete(&app, &admin, id).await;
        let s = get_stats(&app, &admin, "week").await;
        assert_eq!(s["per_chore"][0]["done"], 1);
        assert_eq!(s["per_chore"][0]["by"][0]["user_id"], 1);
        assert_eq!(s["per_chore"][0]["by"][0]["count"], 1);

        // Soft-deleting the chore removes it from per_chore but the earned
        // points survive in totals (spec: deletion never confiscates points).
        let resp = app
            .clone()
            .oneshot(req("DELETE", &format!("/api/chores/{id}"), &admin, None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let s = get_stats(&app, &admin, "week").await;
        assert_eq!(s["per_chore"].as_array().unwrap().len(), 0);
        assert_eq!(s["totals"][0]["points"], 2);
    }

    #[tokio::test]
    async fn trend_has_12_aligned_buckets() {
        let (app, db) = stats_app_with_db().await;
        let admin = setup_admin(&app).await;
        let id = make_chore(&app, &admin, "Tiskit", 5).await;
        complete(&app, &admin, id).await; // this week's bucket
        insert_completion(&db, id, 7, 1, 3).await; // exactly one week back
        insert_completion(&db, id, 400, 1, 9).await; // far outside the 12 weeks

        let s = get_stats(&app, &admin, "week").await;
        let trend = s["trend"].as_array().unwrap();
        assert_eq!(trend.len(), 12);
        // Buckets are Mondays, oldest→newest, 7 days apart.
        let first = trend[0]["week_start"].as_str().unwrap();
        let last = trend[11]["week_start"].as_str().unwrap();
        let fd = NaiveDate::parse_from_str(first, "%Y-%m-%d").unwrap();
        let ld = NaiveDate::parse_from_str(last, "%Y-%m-%d").unwrap();
        assert_eq!(ld - fd, Duration::weeks(11));
        assert_eq!(ld.weekday(), chrono::Weekday::Mon);
        // today's completion lands in the newest bucket, last week's in #10.
        assert_eq!(trend[11]["points"][0]["count"], 5);
        assert_eq!(trend[10]["points"][0]["count"], 3);
        assert_eq!(trend[0]["points"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn week_window_cuts_exactly_at_monday() {
        // Pins the `>=` boundary of the week window: a completion landing
        // exactly ON this week's Monday must count, one landing the day
        // before (Sunday, last week) must not — on any day of the week,
        // including when today itself IS Monday (days_to_monday == 0, so
        // the "on Monday" row is today's).
        let (app, db) = stats_app_with_db().await;
        let admin = setup_admin(&app).await;
        let id = make_chore(&app, &admin, "Tiskit", 1).await;
        let days_to_monday = chrono::Local::now()
            .date_naive()
            .weekday()
            .num_days_from_monday() as i64;
        insert_completion(&db, id, days_to_monday, 1, 2).await; // this week's Monday
        insert_completion(&db, id, days_to_monday + 1, 1, 9).await; // Sunday before: out

        let s = get_stats(&app, &admin, "week").await;
        assert_eq!(s["totals"][0]["points"], 2);
        assert_eq!(s["totals"][0]["completions"], 1);

        let s = get_stats(&app, &admin, "all").await;
        assert_eq!(s["totals"][0]["points"], 11);
        assert_eq!(s["totals"][0]["completions"], 2);
    }

    #[tokio::test]
    async fn stats_are_family_scoped() {
        let (app, db) = stats_app_with_db().await;
        let admin = setup_admin(&app).await;
        let id = make_chore(&app, &admin, "Tiskit", 5).await;
        complete(&app, &admin, id).await;

        // Second family; its admin must see NONE of family 1's data.
        let now = time::OffsetDateTime::now_utc();
        let fid = sqlx::query("INSERT INTO families (name, created_at) VALUES (?, ?)")
            .bind("Other")
            .bind(now)
            .execute(&db)
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
        .execute(&db)
        .await
        .unwrap();
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/auth/login")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "username": "outsider", "password": "password1" }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        let outsider = cookie_pair(&resp);

        let s = get_stats(&app, &outsider, "all").await;
        assert_eq!(s["totals"].as_array().unwrap().len(), 0);
        assert_eq!(s["per_chore"].as_array().unwrap().len(), 0);
    }
}

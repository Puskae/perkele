//! Tilastot: chore leaderboard, 12-week points trend, per-chore breakdown.
//! Online-only by design — standings are a "check the score" screen, unlike
//! the offline-cached today list.

use crate::api;
use crate::screens::chores::name_of;
use dioxus::prelude::*;
use perkele_shared::auth::UserView;
use perkele_shared::chore::ChoreStats;

/// Fixed member colors for the trend chart, assigned by member-list position
/// (stable — color follows the person, never their rank). Dark-surface
/// categorical palette, CVD-validated against the app card color; members
/// beyond six fall back to the muted ink rather than recycling a hue.
const SERIES: [&str; 6] = [
    "#3987e5", "#199e70", "#c98500", "#008300", "#9085e9", "#e66767",
];

fn color_of(members: &[UserView], id: i64) -> &'static str {
    members
        .iter()
        .position(|m| m.id == id)
        .and_then(|i| SERIES.get(i).copied())
        .unwrap_or("#9aa3b2")
}

/// "2026-07-06" → "6.7." (Finnish short date for the chart's x labels).
fn week_label(ws: &str) -> String {
    if ws.len() != 10 {
        return ws.to_owned();
    }
    format!(
        "{}.{}.",
        ws[8..10].trim_start_matches('0'),
        ws[5..7].trim_start_matches('0'),
    )
}

#[component]
pub fn ChoreStatsScreen() -> Element {
    let mut window = use_signal(|| "week".to_owned());
    let members = use_resource(api::members);
    let stats = use_resource(move || {
        let w = window();
        async move { api::chore_stats(&w).await }
    });
    let members_list = members().and_then(|r| r.ok()).unwrap_or_default();

    rsx! {
        div { class: "card wide",
            h1 { "Tilastot" }
            div { class: "row", style: "gap:6px;margin:10px 0;",
                for (val , label) in [("week", "Viikko"), ("month", "Kuukausi"), ("all", "Kaikki")] {
                    button {
                        class: if window() == val { "primary" } else { "ghost" },
                        style: "margin:0;width:auto;padding:6px 12px;",
                        onclick: move |_| window.set(val.to_owned()),
                        "{label}"
                    }
                }
            }
            match stats() {
                Some(Ok(s)) => rsx! {
                    StatsBody { stats: s, members: members_list.clone() }
                },
                Some(Err(e)) => rsx! {
                    div { class: "error", "{e}" }
                },
                None => rsx! {
                    p { class: "muted", "Ladataan…" }
                },
            }
        }
    }
}

#[component]
fn StatsBody(stats: ChoreStats, members: Vec<UserView>) -> Element {
    // Chart scale: the tallest week = full bar height. max(1) avoids a /0
    // when there are no completions at all.
    let max_week: i64 = stats
        .trend
        .iter()
        .map(|b| b.points.iter().map(|p| p.count).sum::<i64>())
        .max()
        .unwrap_or(0)
        .max(1);
    // Legend: only members who actually appear in the trend window.
    let mut legend_ids: Vec<i64> = stats
        .trend
        .iter()
        .flat_map(|b| b.points.iter().map(|p| p.user_id))
        .collect();
    legend_ids.sort_unstable();
    legend_ids.dedup();

    rsx! {
        h2 { "Pistetilanne" }
        if stats.totals.is_empty() {
            p { class: "muted", "Ei suorituksia vielä." }
        }
        ul { class: "members",
            for (i , t) in stats.totals.iter().enumerate() {
                li {
                    span {
                        {["🥇 ", "🥈 ", "🥉 "].get(i).copied().unwrap_or("")}
                        {name_of(&members, t.user_id)}
                    }
                    span { class: "muted", "{t.points} p · {t.completions} krt" }
                }
            }
        }

        h2 { "Pisteet viikoittain" }
        div { class: "trend-chart",
            for b in stats.trend.iter() {
                div { class: "trend-col",
                    div { class: "trend-bars",
                        for p in b.points.iter().filter(|p| p.count > 0) {
                            div {
                                class: "trend-seg",
                                style: format!(
                                    "height:{}%;background:{};",
                                    p.count * 100 / max_week,
                                    color_of(&members, p.user_id),
                                ),
                                // Cheap tooltip: the browser-native title.
                                title: "{name_of(&members, p.user_id)}: {p.count} p",
                            }
                        }
                    }
                    div { class: "trend-label", {week_label(&b.week_start)} }
                }
            }
        }
        div { class: "legend",
            for id in legend_ids {
                span {
                    span {
                        class: "legend-dot",
                        style: format!("background:{};", color_of(&members, id)),
                    }
                    {name_of(&members, id)}
                }
            }
        }

        h2 { "Kotityöt" }
        if stats.per_chore.is_empty() {
            p { class: "muted", "Ei kotitöitä." }
        } else {
            table { class: "stats-table",
                thead {
                    tr {
                        th { "Kotityö" }
                        th { "Tehty" }
                        th { "Kuka" }
                    }
                }
                tbody {
                    for c in stats.per_chore.iter() {
                        tr {
                            td { "{c.title}" }
                            td { "{c.done}/{c.due}" }
                            td { class: "muted",
                                {
                                    c.by
                                        .iter()
                                        .map(|m| format!("{} ×{}", name_of(&members, m.user_id), m.count))
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
